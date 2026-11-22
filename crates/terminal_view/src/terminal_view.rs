mod persistence;
pub mod terminal_element;
pub mod terminal_panel;
mod terminal_path_like_target;
pub mod terminal_scrollbar;

use editor::{
    Editor, EditorSettings, actions::SelectAll, blink_manager::BlinkManager,
    ui_scrollbar_settings_from_raw,
};
use gpui::{
    Action, AnyElement, App, ClipboardEntry, Corners, DismissEvent, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, Font, Hsla, KeyContext, KeyDownEvent, Keystroke,
    MouseButton, MouseDownEvent, Pixels, Point as GpuiPoint, Rems, Render, ScrollEvent, Styled,
    Subscription, Task, TaskExt, WeakEntity, actions, anchored, deferred, div,
};
use menu;
use persistence::TerminalDb;
use project::{Project, ProjectEntryId, search::SearchQuery};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{
    SeedQuerySetting, Settings, SettingsStore, TerminalBell, TerminalBlink, WorkingDirectory,
};
use std::{
    any::Any,
    cmp,
    ops::{Range as StdRange, RangeInclusive},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use task::TaskId;
use terminal::{
    Clear, Content, Copy, Event, HoveredWord, IndexedCell, MaybeNavigationTarget, Modes,
    MouseInputMode, Paste, PasteText, Point, Range, ScrollLineDown, ScrollLineUp, ScrollPageDown,
    ScrollPageUp, ScrollToBottom, ScrollToTop, Search, ShowCharacterPalette, TaskState, TaskStatus,
    Terminal, TerminalBounds, ToggleViMode,
    terminal_settings::{CursorShape, TerminalSettings},
};
use terminal_element::TerminalElement;
use terminal_panel::TerminalPanel;
use terminal_path_like_target::{hover_path_like_target, open_path_like_target};
use terminal_scrollbar::TerminalScrollHandle;
use ui::{
    ContextMenu, Divider, ScrollAxes, Scrollbars, Tooltip, WithScrollbar,
    prelude::*,
    scrollbars::{self, ScrollbarVisibility},
};
use util::ResultExt;
use workspace::{
    CloseActiveItem, DraggedSelection, DraggedTab, NewCenterTerminal, NewTerminal, Pane,
    ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    item::{
        HighlightedText, Item, ItemEvent, SerializableItem, TabContentParams, TabTooltipContent,
    },
    register_serializable_item,
    searchable::{
        Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
    },
};
use zed_actions::{agent::AddSelectionToThread, assistant::InlineAssist};

struct ImeState {
    marked_text: String,
}

fn viewport_line_for_point(point: Point, display_offset: usize) -> Option<usize> {
    let display_offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    let line = point.line.saturating_add(display_offset);
    if line < 0 {
        None
    } else {
        usize::try_from(line).ok()
    }
}

pub(crate) type GutterMarkers = Rc<dyn Fn(&Content, &App) -> Vec<GutterMarker>>;

pub(crate) type SelectionActionHandler =
    Rc<dyn Fn(Range, GpuiPoint<Pixels>, &mut Window, &mut App)>;

#[derive(Clone)]
pub(crate) struct SelectionAction {
    pub(crate) label: SharedString,
    /// What the line highlight mode is called in the label it shows.
    pub(crate) mode_label: SharedString,
    pub(crate) handler: SelectionActionHandler,
    /// When set, the context menu entry dispatches this action instead of
    /// running `handler`, so it shows the action's key binding.
    pub(crate) menu_action: Option<SelectionMenuAction>,
}

#[derive(Clone)]
pub(crate) struct SelectionMenuAction {
    pub(crate) action: Rc<dyn Action>,
    /// Whether the entry shows for the terminal's current content.
    is_available: Rc<dyn Fn(&Content) -> bool>,
}

/// Box borders aren't part of the text they surround.
pub fn is_box_drawing(character: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&character)
}

/// Characters that finding text again skips: whitespace and box borders
/// change when a TUI redraws or the terminal resizes, and an ellipsis marks a
/// truncated quote.
pub fn is_ignored_by_anchor(character: char) -> bool {
    character.is_whitespace() || is_box_drawing(character) || character == '…'
}

/// The scrollback row of a grid line, which keeps its number as output
/// scrolls it up.
pub fn scrollback_row(content: &Content, line: i32) -> i64 {
    content.total_lines as i64 - content.screen_lines as i64 + i64::from(line)
}

/// The terminal's text without the characters that [`is_ignored_by_anchor`]
/// skips, with the grid point of each byte. Text is found in it again after
/// a resize rewrapped it or a TUI redrew it with other padding.
pub struct ScreenText {
    text: String,
    byte_points: Vec<Point>,
}

impl ScreenText {
    pub fn new(cells: &[IndexedCell]) -> Self {
        let mut text = String::new();
        let mut byte_points = Vec::new();
        for cell in cells {
            // Selected text includes a cell's zero-width characters, such as
            // the U+FE0F in "➡️", so they must be here for text to match.
            let characters = std::iter::once(cell.character())
                .chain(cell.zerowidth().into_iter().flatten().copied());
            for character in characters {
                if !is_ignored_by_anchor(character) {
                    text.push(character);
                    byte_points.extend(std::iter::repeat_n(cell.point, character.len_utf8()));
                }
            }
        }
        Self { text, byte_points }
    }

    /// The first and last grid points of `text` where it shows closest to
    /// scrollback row `near_row`, then closest to column `near_column`, since
    /// short text can show several times on a row.
    pub fn find(
        &self,
        text: &str,
        near_row: i64,
        near_column: Option<usize>,
        content: &Content,
    ) -> Option<(Point, Point)> {
        let text = text
            .chars()
            .filter(|character| !is_ignored_by_anchor(*character))
            .collect::<String>();
        if text.is_empty() {
            return None;
        }
        self.text
            .match_indices(text.as_str())
            .filter_map(|(start, matched)| {
                let first = *self.byte_points.get(start)?;
                let last = *self.byte_points.get(start + matched.len() - 1)?;
                Some((first, last))
            })
            .min_by_key(|(start, _)| {
                (
                    (scrollback_row(content, start.line) - near_row).abs(),
                    near_column.map_or(0, |column| start.column.abs_diff(column)),
                )
            })
    }
}

/// A marker in the gutter over grid `lines`. `count` is the number of
/// marked items it stands for, shown when there's more than one.
#[derive(Clone, Debug, PartialEq)]
pub struct GutterMarker {
    pub lines: RangeInclusive<i32>,
    pub color: Hsla,
    pub count: usize,
}

pub(crate) type GutterMarkerClick =
    Rc<dyn Fn(RangeInclusive<i32>, GpuiPoint<Pixels>, &mut Window, &mut App)>;

/// Lines highlighted for the selection action. The highlight covers whole
/// logical lines, where a line the terminal wrapped over several rows counts
/// as one, and its start and end can be moved by words. Rows are counted from
/// the start of the scrollback, so output scrolling the screen doesn't move
/// them.
#[derive(Clone)]
struct LineHighlight {
    /// A row in the logical line at the far end.
    anchor: i64,
    /// A row in the logical line at the active end, which the keyboard and
    /// drags move.
    head: i64,
    /// Where the start was moved to by words, as a row and a column. Never
    /// after the end.
    start: Option<(i64, usize)>,
    /// Where the end was moved to by words, as a row and a column.
    end: Option<(i64, usize)>,
}

/// The highlighted grid lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightedLines {
    pub lines: RangeInclusive<i32>,
    /// The first highlighted column of the first line, when the start was
    /// moved by words. Otherwise the first line is highlighted from its
    /// start.
    pub start_column: Option<usize>,
    /// The last highlighted column of the last line, when the end was moved
    /// by words. Otherwise the last line is highlighted to its end.
    pub end_column: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineHighlightGrab {
    /// A line, which moves the active end by lines.
    Line,
    /// The start handle, which moves the start by words.
    StartHandle,
    /// The end handle, which moves the end by words.
    EndHandle,
}

/// A press that moves an end of the line highlight until its release.
struct LineHighlightDrag {
    pressed_line: i32,
    moved: bool,
    /// Whether a release without a move highlights the pressed line alone,
    /// as when the press grabbed an edge of a highlight of several lines.
    collapse_on_click: bool,
    grab: LineHighlightGrab,
    /// The highlight from before the press, which Esc restores.
    before: LineHighlight,
}

/// How far from a handle, in cells, a press grabs it.
const LINE_HIGHLIGHT_HANDLE_REACH: f32 = 0.5;

/// How long after a resize to look for the highlighted text again, so the
/// program has redrawn by then.
const LINE_HIGHLIGHT_REFIND_DELAY: Duration = Duration::from_millis(500);

/// The keys of the line highlight mode, which its label's tooltip lists.
const LINE_HIGHLIGHT_KEYS: &str = "↑↓ to move · Shift+↑↓ to move the end · Alt+Shift+↑↓ to move the far end · Shift+←→ to move the end by a word · Alt+Shift+←→ to move the start by a word · Drag the handles at the start and end to adjust them · Enter to act on the lines · Esc to stop";

const LINE_HIGHLIGHT_START_HANDLE_TOOLTIP: &str = "Drag to adjust start · Alt+Shift+←/→";
const LINE_HIGHLIGHT_END_HANDLE_TOOLTIP: &str = "Drag to adjust end · Shift+←/→";

const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Event to transmit the scroll from the element to the view
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollTerminal(pub i32);

/// Sends the specified text directly to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendText(String);

/// Sends a keystroke sequence to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendKeystroke(String);

actions!(
    terminal,
    [
        /// Reruns the last executed task in the terminal.
        RerunTask,
        /// Highlights the line above the line highlight's active end.
        MoveLineHighlightUp,
        /// Highlights the line below the line highlight's active end.
        MoveLineHighlightDown,
        /// Moves the line highlight's active end up a line.
        MoveLineHighlightEndUp,
        /// Moves the line highlight's active end down a line.
        MoveLineHighlightEndDown,
        /// Moves the far end of the line highlight up a line.
        MoveLineHighlightFarEndUp,
        /// Moves the far end of the line highlight down a line.
        MoveLineHighlightFarEndDown,
        /// Extends the end of the line highlight by a word.
        ExtendLineHighlightByWord,
        /// Trims a word off the end of the line highlight.
        TrimLineHighlightByWord,
        /// Moves the start of the line highlight left by a word.
        MoveLineHighlightStartLeftByWord,
        /// Moves the start of the line highlight right by a word.
        MoveLineHighlightStartRightByWord,
        /// Runs the terminal's selection action, such as commenting, on the
        /// highlighted lines.
        ConfirmLineHighlight,
        /// Ends the line highlight mode.
        ExitLineHighlight,
    ]
);

/// Renames the terminal tab.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct RenameTerminal;

pub fn init(cx: &mut App) {
    terminal_panel::init(cx);

    register_serializable_item::<TerminalView>(cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(TerminalView::deploy);
    })
    .detach();
}

pub struct BlockProperties {
    pub height: u8,
    pub render: Box<dyn Send + Fn(&mut BlockContext) -> AnyElement>,
}

pub struct BlockContext<'a, 'b> {
    pub window: &'a mut Window,
    pub context: &'b mut App,
    pub dimensions: TerminalBounds,
}

///A terminal view, maintains the PTY's file handles and communicates with the terminal
pub struct TerminalView {
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    focus_handle: FocusHandle,
    //Currently using iTerm bell, show bell emoji in tab until input is received
    has_bell: bool,
    context_menu: Option<(Entity<ContextMenu>, GpuiPoint<Pixels>, Subscription)>,
    cursor_shape: CursorShape,
    blink_manager: Entity<BlinkManager>,
    mode: TerminalMode,
    /// Corner radii for the terminal's background, for when it's embedded in a
    /// container with rounded corners.
    ///
    /// GPUI can't clip children to rounded corners, so without this the square
    /// background paints over the container's corners. Only the background is
    /// rounded, terminal content isn't clipped.
    background_corner_radii: Option<Corners<Rems>>,
    content_top_padding: Rems,
    read_only: bool,
    // Explicit override for whether workspace-specific context menu actions are shown.
    // When `None`, visibility is derived from `mode` (hidden for embedded terminals).
    show_workspace_actions: Option<bool>,
    blinking_terminal_enabled: bool,
    needs_serialize: bool,
    custom_title: Option<String>,
    hover: Option<HoverTarget>,
    hover_tooltip_update: Task<()>,
    workspace_id: Option<WorkspaceId>,
    show_breadcrumbs: bool,
    block_below_cursor: Option<Rc<BlockProperties>>,
    pub(crate) selection_action: Option<SelectionAction>,
    gutter_markers: Option<GutterMarkers>,
    pub(crate) gutter_marker_click: Option<GutterMarkerClick>,
    /// The grid line of the gutter cell under the pointer.
    pub(crate) gutter_hover_line: Option<i32>,
    /// A press in the gutter on a marker, which opens the marker on release
    /// unless a drag highlights lines instead.
    gutter_press: Option<i32>,
    line_highlight: Option<LineHighlight>,
    line_highlight_suspended: bool,
    line_highlight_drag: Option<LineHighlightDrag>,
    /// The highlight's lines, looked up before every paint.
    pub(crate) highlighted_lines: Option<HighlightedLines>,
    /// The highlighted text and the row it starts on, as of the last paint,
    /// to find the highlight again after a resize.
    line_highlight_text: Option<(String, i64, Option<usize>)>,
    line_highlight_refind: Option<Task<()>>,
    /// The grid's columns and lines as of the last paint.
    grid_size: (usize, usize),
    /// The viewport row and column under the pointer, while it's over the
    /// terminal.
    pointer_cell: Option<(usize, usize)>,
    /// The pointer's distance from the grid's left edge, in cells.
    pointer_x: Option<f32>,
    /// Set when a press is the line highlight's or the gutter's, so its
    /// drag and release aren't reported to a program that never saw it.
    pub(crate) swallow_left_mouse_up: bool,
    scroll_top: Pixels,
    scroll_handle: TerminalScrollHandle,
    ime_state: Option<ImeState>,
    self_handle: WeakEntity<Self>,
    rename_editor: Option<Entity<Editor>>,
    rename_editor_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
    _terminal_subscriptions: Vec<Subscription>,
}

#[derive(Default, Clone)]
pub enum TerminalMode {
    #[default]
    Standalone,
    Embedded {
        max_lines_when_unfocused: Option<usize>,
    },
}

#[derive(Clone)]
pub enum ContentMode {
    Scrollable,
    Inline {
        displayed_lines: usize,
        total_lines: usize,
    },
}

impl ContentMode {
    pub fn is_limited(&self) -> bool {
        match self {
            ContentMode::Scrollable => false,
            ContentMode::Inline {
                displayed_lines,
                total_lines,
            } => displayed_lines < total_lines,
        }
    }

    pub fn is_scrollable(&self) -> bool {
        matches!(self, ContentMode::Scrollable)
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone, Eq, PartialEq))]
struct HoverTarget {
    tooltip: String,
    hovered_word: HoveredWord,
}

impl EventEmitter<Event> for TerminalView {}
impl EventEmitter<ItemEvent> for TerminalView {}
impl EventEmitter<SearchEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    ///Create a new Terminal in the current working directory or the user's home directory
    pub fn deploy(
        workspace: &mut Workspace,
        action: &NewCenterTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let local = action.local;
        let working_directory = default_working_directory(workspace, cx);
        TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
            if local {
                project.create_local_terminal(cx)
            } else {
                project.create_terminal_shell(working_directory, cx)
            }
        })
        .detach_and_log_err(cx);
    }

    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        project: WeakEntity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_handle = workspace.clone();
        let terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, workspace, window, cx);

        let focus_handle = cx.focus_handle();
        let focus_in = cx.on_focus_in(&focus_handle, window, |terminal_view, window, cx| {
            terminal_view.focus_in(window, cx);
        });
        let focus_out = cx.on_focus_out(
            &focus_handle,
            window,
            |terminal_view, _event, window, cx| {
                terminal_view.focus_out(window, cx);
            },
        );
        let cursor_shape = TerminalSettings::get_global(cx).cursor_shape;

        let scroll_handle = TerminalScrollHandle::new(terminal.read(cx));

        let blink_manager = cx.new(|cx| {
            BlinkManager::new(
                CURSOR_BLINK_INTERVAL,
                |cx| {
                    !matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::Off
                    )
                },
                cx,
            )
        });

        let subscriptions = vec![
            focus_in,
            focus_out,
            cx.observe(&blink_manager, |_, _, cx| cx.notify()),
            cx.observe_global::<SettingsStore>(Self::settings_changed),
        ];

        Self {
            terminal,
            workspace: workspace_handle,
            project,
            has_bell: false,
            focus_handle,
            context_menu: None,
            cursor_shape,
            blink_manager,
            blinking_terminal_enabled: false,
            hover: None,
            hover_tooltip_update: Task::ready(()),
            mode: TerminalMode::Standalone,
            background_corner_radii: None,
            content_top_padding: Rems::ZERO,
            read_only: false,
            show_workspace_actions: None,
            workspace_id,
            show_breadcrumbs: TerminalSettings::get_global(cx).toolbar.breadcrumbs,
            block_below_cursor: None,
            selection_action: None,
            gutter_markers: None,
            gutter_marker_click: None,
            gutter_hover_line: None,
            gutter_press: None,
            line_highlight: None,
            line_highlight_suspended: false,
            line_highlight_drag: None,
            highlighted_lines: None,
            line_highlight_text: None,
            line_highlight_refind: None,
            grid_size: (0, 0),
            pointer_cell: None,
            pointer_x: None,
            swallow_left_mouse_up: false,
            scroll_top: Pixels::ZERO,
            scroll_handle,
            needs_serialize: false,
            custom_title: None,
            ime_state: None,
            self_handle: cx.entity().downgrade(),
            rename_editor: None,
            rename_editor_subscription: None,
            _subscriptions: subscriptions,
            _terminal_subscriptions: terminal_subscriptions,
        }
    }

    /// Enable 'embedded' mode where the terminal displays the full content with an optional limit of lines.
    pub fn set_embedded_mode(
        &mut self,
        max_lines_when_unfocused: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.mode = TerminalMode::Embedded {
            max_lines_when_unfocused,
        };
        cx.notify();
    }

    /// Rounds the background without clipping terminal content to the corners.
    pub fn set_background_corner_radii(
        &mut self,
        corner_radii: Option<Corners<Rems>>,
        cx: &mut Context<Self>,
    ) {
        self.background_corner_radii = corner_radii;
        cx.notify();
    }

    pub fn set_content_top_padding(&mut self, padding: Rems, cx: &mut Context<Self>) {
        self.content_top_padding = padding;
        cx.notify();
    }

    fn content_background_corner_radii(
        &self,
        content_mode: &ContentMode,
        cx: &App,
    ) -> Corners<Rems> {
        let mut radii = self.background_corner_radii.unwrap_or_default();
        if content_mode.is_scrollable()
            && TerminalScrollbarSettingsWrapper.visibility(cx) != scrollbars::ShowScrollbar::Never
        {
            radii.top_right = Rems::ZERO;
            radii.bottom_right = Rems::ZERO;
        }
        radii
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Fixes the view's input policy at construction, without restricting producer output
    /// or local navigation.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub(crate) fn mouse_input_mode(&self) -> MouseInputMode {
        if self.read_only || self.takes_mouse_for_line_highlight() {
            MouseInputMode::LocalSelection
        } else {
            MouseInputMode::ReportToTerminal
        }
    }

    /// Explicitly override whether workspace-specific context menu actions (e.g. creating or
    /// closing terminal tabs, inline assist) are shown.
    ///
    /// This lets hosts that aren't workspace panes (such as the agent panel) hide these
    /// actions without `terminal_view` needing to know about those hosts. When never called,
    /// visibility is derived from the terminal's `mode`.
    pub fn set_show_workspace_actions(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_workspace_actions = Some(show);
        cx.notify();
    }

    /// Sets the action that runs on a selection or on highlighted lines,
    /// which turns on the line highlight mode: `handler` gets the range and
    /// the window position to open at. `mode_label` names the mode in the
    /// label it shows.
    pub fn on_selection_action(
        &mut self,
        label: impl Into<SharedString>,
        mode_label: impl Into<SharedString>,
        handler: impl Fn(Range, GpuiPoint<Pixels>, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.selection_action = Some(SelectionAction {
            label: label.into(),
            mode_label: mode_label.into(),
            handler: Rc::new(handler),
            menu_action: None,
        });
        cx.notify();
    }

    /// Makes the selection's context menu entry dispatch `action`, and show
    /// only while `is_available` holds for the terminal's content.
    pub fn set_selection_menu_action(
        &mut self,
        action: Box<dyn Action>,
        is_available: impl Fn(&Content) -> bool + 'static,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection_action) = self.selection_action.as_mut() {
            selection_action.menu_action = Some(SelectionMenuAction {
                action: Rc::from(action),
                is_available: Rc::new(is_available),
            });
            cx.notify();
        }
    }

    /// Whether the selection action is available for the terminal's
    /// current content, on a selection or on highlighted lines.
    pub fn selection_menu_entry_available(&self, cx: &App) -> bool {
        self.selection_action
            .as_ref()
            .is_some_and(|selection_action| {
                selection_action
                    .menu_action
                    .as_ref()
                    .is_none_or(|menu_action| {
                        (menu_action.is_available)(self.terminal.read(cx).last_content())
                    })
            })
    }

    pub fn clear_selection_action(&mut self, cx: &mut Context<Self>) {
        self.selection_action = None;
        self.exit_line_highlight(cx);
        cx.notify();
    }

    /// The finished selection, its text and the window position its action
    /// opens at. When the selection's end is off screen the
    /// position is clamped to the nearest visible row, since scrolling in vi
    /// mode would move the vi cursor and the selection with it.
    pub fn selection_action_target(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<(Range, String, GpuiPoint<Pixels>)> {
        let terminal = self.terminal.read(cx);
        if terminal.selection_started() {
            return None;
        }
        let content = terminal.last_content();
        let selection = content.selection?;
        let text = content
            .selection_text
            .clone()
            .filter(|text| !text.is_empty())?;
        let dimensions = content.terminal_bounds;
        let display_offset = i32::try_from(content.display_offset).ok()?;
        let num_lines = i32::try_from(dimensions.num_lines()).ok()?;
        if num_lines == 0 {
            return None;
        }
        let row = (selection.end.line + display_offset).clamp(0, num_lines - 1);
        let origin = terminal_element::selection_action_origin(
            selection.end.column,
            row as usize,
            dimensions,
            self.scroll_top,
            window,
            cx,
        );
        Some((selection.point_range(), text, origin))
    }

    /// Marks ranges of grid lines with a bar in the gutter left of the text.
    /// `markers` runs on every paint, with the content being painted, so the
    /// marks follow the terminal's output.
    pub fn set_gutter_markers(
        &mut self,
        markers: impl Fn(&Content, &App) -> Vec<GutterMarker> + 'static,
        cx: &mut Context<Self>,
    ) {
        self.gutter_markers = Some(Rc::new(markers));
        cx.notify();
    }

    /// Runs `handler` with a marker's lines and the window position of a
    /// click on it.
    pub fn on_gutter_marker_click(
        &mut self,
        handler: impl Fn(RangeInclusive<i32>, GpuiPoint<Pixels>, &mut Window, &mut App) + 'static,
    ) {
        self.gutter_marker_click = Some(Rc::new(handler));
    }

    pub fn clear_gutter_markers(&mut self, cx: &mut Context<Self>) {
        self.gutter_markers = None;
        self.gutter_marker_click = None;
        cx.notify();
    }

    /// The gutter marker over a grid line, if any.
    pub(crate) fn gutter_marker_at(&self, line: i32, cx: &App) -> Option<GutterMarker> {
        let markers = self.gutter_markers.as_ref()?;
        markers(self.terminal.read(cx).last_content(), cx)
            .into_iter()
            .find(|marker| marker.lines.contains(&line))
    }

    /// Whether the line highlight mode can start: the host set a selection
    /// action, which is available for the terminal's content.
    pub fn line_highlight_available(&self, cx: &App) -> bool {
        self.selection_menu_entry_available(cx)
    }

    /// Hides the highlight and gives the mouse back to the program without
    /// ending the mode, while the action started from it is in progress.
    pub fn set_line_highlight_suspended(&mut self, suspended: bool, cx: &mut Context<Self>) {
        self.line_highlight_suspended = suspended;
        cx.notify();
    }

    /// Whether the line highlight mode is on, suspended or not.
    pub fn has_line_highlight(&self) -> bool {
        self.line_highlight.is_some()
    }

    pub(crate) fn is_dragging_line_highlight(&self) -> bool {
        self.line_highlight_drag.is_some()
    }

    pub(crate) fn takes_mouse_for_line_highlight(&self) -> bool {
        self.line_highlight.is_some() && !self.line_highlight_suspended
    }

    /// Starts the line highlight mode on grid `line`, else on the last
    /// visible line with text. Esc, an unbound key, losing focus or
    /// [`Self::exit_line_highlight`] ends it.
    pub fn start_line_highlight(&mut self, line: Option<i32>, cx: &mut Context<Self>) {
        if !self.line_highlight_available(cx) {
            return;
        }
        let line = line.unwrap_or_else(|| self.last_text_line(cx));
        self.line_highlight_suspended = false;
        self.line_highlight_drag = None;
        self.set_line_highlight(line, line, cx);
    }

    pub fn exit_line_highlight(&mut self, cx: &mut Context<Self>) {
        self.line_highlight_drag = None;
        self.line_highlight_refind = None;
        self.line_highlight_text = None;
        self.highlighted_lines = None;
        if self.line_highlight.take().is_some() {
            cx.notify();
        }
    }

    fn set_line_highlight(&mut self, anchor: i32, head: i32, cx: &mut Context<Self>) {
        let (Some(anchor), Some(head)) = (
            self.scrollback_line(anchor, cx),
            self.scrollback_line(head, cx),
        ) else {
            return;
        };
        self.line_highlight = Some(LineHighlight {
            anchor,
            head,
            start: None,
            end: None,
        });
        cx.notify();
    }

    /// The last visible line with text, else the last visible line.
    fn last_text_line(&self, cx: &App) -> i32 {
        let visible_lines = self.visible_lines(cx);
        self.terminal
            .read(cx)
            .grid_rows(visible_lines.clone())
            .iter()
            .rev()
            .find(|row| {
                row.characters
                    .iter()
                    .any(|character| !character.is_whitespace())
            })
            .map_or(*visible_lines.end(), |row| row.line)
    }

    /// The grid lines from the top of the scrollback to the bottom of the
    /// screen.
    fn grid_lines(&self, cx: &App) -> RangeInclusive<i32> {
        let content = self.terminal.read(cx).last_content();
        let history_lines =
            i32::try_from(content.total_lines.saturating_sub(content.screen_lines)).unwrap_or(0);
        let screen_lines = i32::try_from(content.screen_lines).unwrap_or(0);
        -history_lines..=screen_lines - 1
    }

    /// The rows of the logical line at `line`: the rows the terminal wrapped
    /// it over.
    fn logical_line(&self, line: i32, cx: &App) -> RangeInclusive<i32> {
        let grid_lines = self.grid_lines(cx);
        let line = line.clamp(*grid_lines.start(), *grid_lines.end());
        let terminal = self.terminal.read(cx);
        let wraps = |line: i32| {
            terminal
                .grid_rows(line..=line)
                .first()
                .is_some_and(|row| row.wraps)
        };
        let mut start = line;
        while start > *grid_lines.start() && wraps(start - 1) {
            start -= 1;
        }
        let mut end = line;
        while end < *grid_lines.end() && wraps(end) {
            end += 1;
        }
        start..=end
    }

    /// The first line of the logical line before or after the one at
    /// `line`.
    fn adjacent_logical_line(&self, line: i32, forward: bool, cx: &App) -> Option<i32> {
        let logical_line = self.logical_line(line, cx);
        let grid_lines = self.grid_lines(cx);
        let next = if forward {
            logical_line.end() + 1
        } else {
            logical_line.start() - 1
        };
        grid_lines
            .contains(&next)
            .then(|| *self.logical_line(next, cx).start())
    }

    /// The highlight's grid lines, looked up from the terminal's content.
    pub fn line_highlight_lines(&self, cx: &App) -> Option<HighlightedLines> {
        let highlight = self.line_highlight.as_ref()?;
        let anchor = self.grid_line(highlight.anchor, cx)?;
        let head = self.grid_line(highlight.head, cx)?;
        let start = highlight
            .start
            .and_then(|(row, column)| Some((self.grid_line(row, cx)?, column)));
        let end = highlight
            .end
            .and_then(|(row, column)| Some((self.grid_line(row, cx)?, column)));
        let first = match start {
            Some((line, _)) => line,
            None => *self.logical_line(anchor.min(head), cx).start(),
        };
        let last = match end {
            Some((line, _)) => line,
            None => *self.logical_line(anchor.max(head), cx).end(),
        };
        Some(HighlightedLines {
            lines: first..=last.max(first),
            start_column: start.map(|(_, column)| column),
            end_column: end.map(|(_, column)| column),
        })
    }

    /// The first and last highlighted cells.
    fn line_highlight_points(&self, highlighted: &HighlightedLines, cx: &App) -> (Point, Point) {
        let num_columns = self
            .terminal
            .read(cx)
            .last_content()
            .terminal_bounds
            .num_columns();
        (
            Point::new(
                *highlighted.lines.start(),
                highlighted.start_column.unwrap_or(0),
            ),
            Point::new(
                *highlighted.lines.end(),
                highlighted
                    .end_column
                    .unwrap_or(num_columns.saturating_sub(1)),
            ),
        )
    }

    /// Moves an edge of the highlight to a word boundary at `point`. The
    /// line of the start or the end also becomes the first or last line of
    /// the anchor and the head, so later line moves start from it.
    fn set_line_highlight_edge(
        &mut self,
        edge: LineHighlightGrab,
        point: Point,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.scrollback_line(point.line, cx) else {
            return;
        };
        let Some(highlight) = &mut self.line_highlight else {
            return;
        };
        let head_is_first = highlight.head < highlight.anchor;
        match edge {
            LineHighlightGrab::StartHandle => {
                if head_is_first {
                    highlight.head = row;
                } else {
                    highlight.anchor = row;
                }
                highlight.start = Some((row, point.column));
            }
            LineHighlightGrab::EndHandle => {
                if head_is_first {
                    highlight.anchor = row;
                } else {
                    highlight.head = row;
                }
                highlight.end = Some((row, point.column));
            }
            LineHighlightGrab::Line => {}
        }
        cx.notify();
    }

    /// Moves the anchor and the head by lines. An edge left on its line
    /// keeps its column, and an edge moved to another line snaps to the
    /// nearest word boundary at or before its column there, else the line's
    /// start or end.
    fn move_line_highlight_lines(
        &mut self,
        update: impl FnOnce(&mut LineHighlight),
        cx: &mut Context<Self>,
    ) {
        let before = self.line_highlight_lines(cx);
        let Some(highlight) = &mut self.line_highlight else {
            return;
        };
        let head_was_first = highlight.head < highlight.anchor;
        update(highlight);
        // An edge whose anchor or head moved past the other one becomes the
        // other kind of edge, whose column it doesn't have.
        let flipped = head_was_first != (highlight.head < highlight.anchor)
            && highlight.head != highlight.anchor;
        highlight.start = None;
        highlight.end = None;
        cx.notify();
        let (Some(mut before), Some(after)) = (before, self.line_highlight_lines(cx)) else {
            return;
        };
        if flipped {
            before.start_column = None;
            before.end_column = None;
        }
        let same_logical_line = |before: i32, after: i32| {
            self.logical_line(before, cx).start() == self.logical_line(after, cx).start()
        };
        let start = before.start_column.and_then(|column| {
            let before_line = *before.lines.start();
            let after_line = *after.lines.start();
            if same_logical_line(before_line, after_line) {
                Some(Point::new(before_line, column))
            } else {
                self.word_starts(after_line, cx)
                    .into_iter()
                    .rev()
                    .find(|start| start.column <= column)
            }
        });
        let end = before.end_column.and_then(|column| {
            let before_line = *before.lines.end();
            let after_line = *after.lines.end();
            if same_logical_line(before_line, after_line) {
                Some(Point::new(before_line, column))
            } else {
                self.word_ends(after_line, cx)
                    .into_iter()
                    .rev()
                    .find(|end| end.column <= column)
            }
        });
        // Snapping the end back on the start's line can put it before the
        // start.
        let end = match (start, end) {
            (Some(start), Some(end)) if end < start => self
                .word_ends(end.line, cx)
                .into_iter()
                .find(|word_end| *word_end >= start),
            _ => end,
        };
        let start =
            start.and_then(|point| Some((self.scrollback_line(point.line, cx)?, point.column)));
        let end = end.and_then(|point| Some((self.scrollback_line(point.line, cx)?, point.column)));
        if let Some(highlight) = &mut self.line_highlight {
            highlight.start = start;
            highlight.end = end;
        }
    }

    /// Highlights the line before or after the active end's, alone.
    fn move_line_highlight(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(head) = self
            .line_highlight
            .as_ref()
            .and_then(|highlight| self.grid_line(highlight.head, cx))
        else {
            return;
        };
        let Some(line) = self.adjacent_logical_line(head, forward, cx) else {
            return;
        };
        self.set_line_highlight(line, line, cx);
        self.scroll_to_line(line, cx);
    }

    /// Moves the active end, or the far end, a line up or down, which grows
    /// or shrinks the highlight.
    fn move_line_highlight_end(&mut self, far_end: bool, forward: bool, cx: &mut Context<Self>) {
        let Some(highlight) = &self.line_highlight else {
            return;
        };
        let row = if far_end {
            highlight.anchor
        } else {
            highlight.head
        };
        let Some(line) = self
            .grid_line(row, cx)
            .and_then(|line| self.adjacent_logical_line(line, forward, cx))
        else {
            return;
        };
        let Some(row) = self.scrollback_line(line, cx) else {
            return;
        };
        self.move_line_highlight_lines(
            |highlight| {
                if far_end {
                    highlight.anchor = row;
                } else {
                    highlight.head = row;
                }
            },
            cx,
        );
        self.scroll_to_line(line, cx);
    }

    /// Moves the highlight's end to the next word end, which can be past
    /// the last highlighted line, or to the previous one, never before the
    /// start's word.
    fn move_line_highlight_end_by_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(highlighted) = self.line_highlight_lines(cx) else {
            return;
        };
        let first = *highlighted.lines.start();
        let last = *highlighted.lines.end();
        let (start, _) = self.line_highlight_points(&highlighted, cx);
        let current = match highlighted.end_column {
            Some(column) => Point::new(last, column),
            None => (first..=last)
                .rev()
                .find_map(|line| self.word_ends(line, cx).last().copied())
                .unwrap_or(Point::new(last, 0)),
        };
        let next = if forward {
            (current.line..=*self.grid_lines(cx).end()).find_map(|line| {
                self.word_ends(line, cx)
                    .into_iter()
                    .find(|end| *end > current)
            })
        } else {
            (first..=current.line)
                .rev()
                .find_map(|line| {
                    self.word_ends(line, cx)
                        .into_iter()
                        .rev()
                        .find(|end| *end < current)
                })
                .filter(|end| *end >= start)
        };
        let Some(next) = next else {
            return;
        };
        self.set_line_highlight_edge(LineHighlightGrab::EndHandle, next, cx);
        self.scroll_to_line(next.line, cx);
    }

    /// Moves the highlight's start to the previous word start, which can be
    /// before the first highlighted line, or to the next one, never past the
    /// end's word.
    fn move_line_highlight_start_by_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(highlighted) = self.line_highlight_lines(cx) else {
            return;
        };
        let first = *highlighted.lines.start();
        let last = *highlighted.lines.end();
        let (_, end) = self.line_highlight_points(&highlighted, cx);
        let current = match highlighted.start_column {
            Some(column) => Point::new(first, column),
            None => (first..=last)
                .find_map(|line| self.word_starts(line, cx).first().copied())
                .unwrap_or(Point::new(first, 0)),
        };
        let next = if forward {
            (current.line..=last)
                .find_map(|line| {
                    self.word_starts(line, cx)
                        .into_iter()
                        .find(|start| *start > current)
                })
                .filter(|start| *start <= end)
        } else {
            (*self.grid_lines(cx).start()..=current.line)
                .rev()
                .find_map(|line| {
                    self.word_starts(line, cx)
                        .into_iter()
                        .rev()
                        .find(|start| *start < current)
                })
        };
        let Some(next) = next else {
            return;
        };
        self.set_line_highlight_edge(LineHighlightGrab::StartHandle, next, cx);
        self.scroll_to_line(next.line, cx);
    }

    /// Where words start on a row, in order. A word starts after a word
    /// separator, or at the start of the row unless the terminal wrapped the
    /// row before onto it.
    fn word_starts(&self, line: i32, cx: &App) -> Vec<Point> {
        let terminal = self.terminal.read(cx);
        let rows = terminal.grid_rows(line..=line);
        let Some(row) = rows.first() else {
            return Vec::new();
        };
        let wrapped_onto = line > *self.grid_lines(cx).start()
            && terminal
                .grid_rows(line - 1..=line - 1)
                .first()
                .is_some_and(|row| row.wraps);
        let is_word = |column: usize| {
            row.characters
                .get(column)
                .is_some_and(|character| !terminal::is_word_separator(*character))
        };
        let mut starts = Vec::new();
        for column in 0..row.characters.len() {
            if row.wide_char_spacers.contains(&column) || !is_word(column) {
                continue;
            }
            let mut previous = column.checked_sub(1);
            while let Some(spacer) =
                previous.filter(|column| row.wide_char_spacers.contains(column))
            {
                previous = spacer.checked_sub(1);
            }
            let continues = match previous {
                Some(previous) => is_word(previous),
                None => wrapped_onto,
            };
            if !continues {
                starts.push(Point::new(line, column));
            }
        }
        starts
    }

    /// The column right after the cell at `point`, past a wide character's
    /// spacer.
    fn column_after(&self, point: Point, cx: &App) -> usize {
        let rows = self.terminal.read(cx).grid_rows(point.line..=point.line);
        let mut next = point.column + 1;
        if let Some(row) = rows.first() {
            while row.wide_char_spacers.contains(&next) {
                next += 1;
            }
        }
        next
    }

    /// Where words end on a row, in order. A word runs to a word separator,
    /// or to the end of the row unless the terminal wrapped it.
    fn word_ends(&self, line: i32, cx: &App) -> Vec<Point> {
        let rows = self.terminal.read(cx).grid_rows(line..=line);
        let Some(row) = rows.first() else {
            return Vec::new();
        };
        let is_word = |column: usize| {
            row.characters
                .get(column)
                .is_some_and(|character| !terminal::is_word_separator(*character))
        };
        let mut ends = Vec::new();
        for column in 0..row.characters.len() {
            if row.wide_char_spacers.contains(&column) || !is_word(column) {
                continue;
            }
            let mut next = column + 1;
            while row.wide_char_spacers.contains(&next) {
                next += 1;
            }
            let continues = if next < row.characters.len() {
                is_word(next)
            } else {
                row.wraps
            };
            if !continues {
                ends.push(Point::new(line, column));
            }
        }
        ends
    }

    /// Scrolls the view so grid `line` is visible.
    fn scroll_to_line(&mut self, line: i32, cx: &mut Context<Self>) {
        let visible_lines = self.visible_lines(cx);
        if line < *visible_lines.start() {
            let lines = usize::try_from(visible_lines.start() - line).unwrap_or(0);
            self.terminal
                .update(cx, |terminal, _| terminal.scroll_up_by(lines));
        } else if line > *visible_lines.end() {
            let lines = usize::try_from(line - visible_lines.end()).unwrap_or(0);
            self.terminal
                .update(cx, |terminal, _| terminal.scroll_down_by(lines));
        }
    }

    /// The highlighted range and the window position the action opens at,
    /// below the highlight's last visible line.
    pub fn take_line_highlight_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(Range, GpuiPoint<Pixels>)> {
        if !self.takes_mouse_for_line_highlight() {
            return None;
        }
        let highlighted = self.line_highlight_lines(cx)?;
        let (start, end) = self.line_highlight_points(&highlighted, cx);
        let content = self.terminal.read(cx).last_content();
        let last = *highlighted.lines.end();
        let range = Range::new(start, end);
        let visible_lines = self.visible_lines(cx);
        let anchor_line = last.clamp(*visible_lines.start(), *visible_lines.end());
        let origin = terminal_element::line_highlight_action_origin(
            anchor_line,
            content.display_offset,
            content.terminal_bounds,
            self.scroll_top,
            window,
            cx,
        )?;
        Some((range, origin))
    }

    fn confirm_line_highlight(
        &mut self,
        _: &ConfirmLineHighlight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection_action) = self.selection_action.clone() else {
            return;
        };
        // Such as while a program shows a picker that Enter is meant for.
        if !self.selection_menu_entry_available(cx) {
            self.exit_line_highlight(cx);
            return;
        }
        let Some((range, origin)) = self.take_line_highlight_target(window, cx) else {
            return;
        };
        // The handler may update this view.
        window.defer(cx, move |window, cx| {
            (selection_action.handler)(range, origin, window, cx)
        });
    }

    /// A press in the highlight mode at grid `line`: Shift extends the
    /// highlight to the line, a press on its first or last line grabs that
    /// edge, and any other press highlights the line. A drag moves the
    /// pressed end until the release.
    pub(crate) fn press_line_highlight(
        &mut self,
        line: i32,
        x: f32,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        self.swallow_left_mouse_up = true;
        let Some(before) = self.line_highlight.clone() else {
            return;
        };
        if !shift && let Some(grab) = self.line_highlight_handle_at(line, x, cx) {
            self.line_highlight_drag = Some(LineHighlightDrag {
                pressed_line: line,
                moved: false,
                collapse_on_click: false,
                grab,
                before,
            });
            return;
        }
        let highlighted = self.line_highlight_lines(cx);
        let mut collapse_on_click = false;
        match highlighted {
            Some(highlighted) if shift => {
                // The end farther from the line stays put.
                let far_end = if line < *highlighted.lines.start() {
                    *highlighted.lines.end()
                } else {
                    *highlighted.lines.start()
                };
                self.set_line_highlight(far_end, line, cx);
            }
            Some(highlighted) if highlighted.lines.start() != highlighted.lines.end() => {
                let first = self.logical_line(*highlighted.lines.start(), cx);
                let last = self.logical_line(*highlighted.lines.end(), cx);
                // Grabbing an edge keeps the columns, as its lines stay.
                let grabbed = if first.contains(&line) {
                    Some(*highlighted.lines.end())
                } else if last.contains(&line) {
                    Some(*highlighted.lines.start())
                } else {
                    None
                };
                match grabbed.zip(self.scrollback_line(line, cx)) {
                    Some((far_end, head)) => {
                        collapse_on_click = true;
                        if let Some(anchor) = self.scrollback_line(far_end, cx)
                            && let Some(highlight) = &mut self.line_highlight
                        {
                            highlight.anchor = anchor;
                            highlight.head = head;
                        }
                    }
                    None => self.set_line_highlight(line, line, cx),
                }
            }
            _ => self.set_line_highlight(line, line, cx),
        }
        self.line_highlight_drag = Some(LineHighlightDrag {
            pressed_line: line,
            moved: false,
            collapse_on_click,
            grab: LineHighlightGrab::Line,
            before,
        });
    }

    fn set_line_highlight_head(&mut self, line: i32, cx: &mut Context<Self>) {
        let Some(row) = self.scrollback_line(line, cx) else {
            return;
        };
        if self
            .line_highlight
            .as_ref()
            .is_some_and(|highlight| highlight.head != row)
        {
            self.move_line_highlight_lines(|highlight| highlight.head = row, cx);
        }
    }

    pub(crate) fn on_line_highlight_handle(&self, line: i32, x: f32, cx: &App) -> bool {
        self.line_highlight_handle_at(line, x, cx).is_some()
    }

    /// The handle a press at grid `line` and `x` cells from the grid's left
    /// edge grabs. The nearer handle wins when both are in reach.
    fn line_highlight_handle_at(&self, line: i32, x: f32, cx: &App) -> Option<LineHighlightGrab> {
        if !self.takes_mouse_for_line_highlight() {
            return None;
        }
        let highlighted = self.line_highlight_lines(cx)?;
        let (start, end) = self.line_highlight_points(&highlighted, cx);
        let reach = |point: Point, edge_column: usize| {
            let distance = (x - edge_column as f32).abs();
            (point.line == line && distance <= LINE_HIGHLIGHT_HANDLE_REACH).then_some(distance)
        };
        let start_distance = reach(start, start.column);
        let end_distance = reach(end, self.column_after(end, cx));
        match (start_distance, end_distance) {
            (Some(start_distance), Some(end_distance)) if end_distance < start_distance => {
                Some(LineHighlightGrab::EndHandle)
            }
            (Some(_), _) => Some(LineHighlightGrab::StartHandle),
            (None, Some(_)) => Some(LineHighlightGrab::EndHandle),
            (None, None) => None,
        }
    }

    /// Moves the dragged handle's edge to the word boundary nearest `x` on
    /// grid `line`, stopping at the other edge's word.
    fn drag_line_highlight_handle(
        &mut self,
        edge: LineHighlightGrab,
        line: i32,
        x: f32,
        cx: &mut Context<Self>,
    ) {
        let Some(highlighted) = self.line_highlight_lines(cx) else {
            return;
        };
        let (start, end) = self.line_highlight_points(&highlighted, cx);
        let distance = |column: usize| (x - column as f32).abs();
        let target = match edge {
            LineHighlightGrab::StartHandle => self
                .word_starts(line, cx)
                .into_iter()
                .min_by(|a, b| distance(a.column).total_cmp(&distance(b.column)))
                .and_then(|target| {
                    if target <= end {
                        return Some(target);
                    }
                    (*highlighted.lines.start()..=end.line)
                        .rev()
                        .find_map(|line| {
                            self.word_starts(line, cx)
                                .into_iter()
                                .rev()
                                .find(|word_start| *word_start <= end)
                        })
                }),
            LineHighlightGrab::EndHandle => self
                .word_ends(line, cx)
                .into_iter()
                .map(|word_end| (self.column_after(word_end, cx), word_end))
                .min_by(|(a, _), (b, _)| distance(*a).total_cmp(&distance(*b)))
                .map(|(_, word_end)| word_end)
                .and_then(|target| {
                    if target >= start {
                        return Some(target);
                    }
                    (start.line..=*self.grid_lines(cx).end()).find_map(|line| {
                        self.word_ends(line, cx)
                            .into_iter()
                            .find(|word_end| *word_end >= start)
                    })
                }),
            LineHighlightGrab::Line => None,
        };
        let current = match edge {
            LineHighlightGrab::StartHandle => highlighted.start_column.map(|_| start),
            _ => highlighted.end_column.map(|_| end),
        };
        if let Some(target) = target
            && current != Some(target)
        {
            self.set_line_highlight_edge(edge, target, cx);
        }
    }

    /// Ends a drag with Esc, restoring the highlight from before it.
    /// Returns whether a drag was going on.
    fn cancel_line_highlight_drag(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.line_highlight_drag.take() else {
            return false;
        };
        self.line_highlight = Some(drag.before);
        cx.notify();
        true
    }

    /// The tooltip of the handle under the pointer.
    pub(crate) fn line_highlight_handle_tooltip(&self, cx: &App) -> Option<&'static str> {
        if self.line_highlight_drag.is_some() {
            return None;
        }
        let line = self.pointer_point(cx)?.line;
        match self.line_highlight_handle_at(line, self.pointer_x?, cx)? {
            LineHighlightGrab::StartHandle => Some(LINE_HIGHLIGHT_START_HANDLE_TOOLTIP),
            LineHighlightGrab::EndHandle => Some(LINE_HIGHLIGHT_END_HANDLE_TOOLTIP),
            LineHighlightGrab::Line => None,
        }
    }

    /// Whether the pointer shows that a drag moves an edge of the highlight
    /// sideways.
    pub fn shows_line_column_resize_cursor(&self, cx: &App) -> bool {
        if let Some(drag) = &self.line_highlight_drag {
            return drag.grab != LineHighlightGrab::Line;
        }
        let (Some(point), Some(x)) = (self.pointer_point(cx), self.pointer_x) else {
            return false;
        };
        self.line_highlight_handle_at(point.line, x, cx).is_some()
    }

    /// The grid line of a window position, and its distance from the grid's
    /// left edge in cells.
    pub(crate) fn grid_position_at(&self, position: GpuiPoint<Pixels>, cx: &App) -> (i32, f32) {
        let terminal = self.terminal.read(cx);
        let bounds = terminal.last_content().terminal_bounds;
        let mut scrolled = position;
        scrolled.y += self.scroll_top;
        let line = terminal.grid_point_at(scrolled).line;
        let x = if bounds.cell_width > px(0.) {
            (position.x - bounds.bounds.origin.x) / bounds.cell_width
        } else {
            0.
        };
        (line, x)
    }

    /// A press in the gutter at grid `line`: on a marker it waits for the
    /// release to open the marker, unless a drag highlights lines instead,
    /// and elsewhere it starts the highlight mode on the line. Returns
    /// whether the gutter takes the press.
    pub(crate) fn press_gutter(&mut self, line: i32, cx: &mut Context<Self>) -> bool {
        let on_marker =
            self.gutter_marker_click.is_some() && self.gutter_marker_at(line, cx).is_some();
        if !on_marker && !self.line_highlight_available(cx) {
            return false;
        }
        self.swallow_left_mouse_up = true;
        if on_marker {
            self.gutter_press = Some(line);
        } else {
            self.start_line_highlight(Some(line), cx);
            self.begin_gutter_line_drag(line);
        }
        true
    }

    /// Ends a press of the highlight or the gutter, and returns the line of
    /// a gutter marker it clicked.
    pub(crate) fn release_line_highlight(&mut self, cx: &mut Context<Self>) -> Option<i32> {
        if let Some(drag) = self.line_highlight_drag.take()
            && drag.collapse_on_click
            && !drag.moved
        {
            self.set_line_highlight(drag.pressed_line, drag.pressed_line, cx);
        }
        self.gutter_press.take()
    }

    /// Whether the pointer shows that a drag moves an edge of the
    /// highlight.
    pub fn shows_line_resize_cursor(&self, cx: &App) -> bool {
        if !self.takes_mouse_for_line_highlight() {
            return false;
        }
        if self
            .line_highlight_drag
            .as_ref()
            .is_some_and(|drag| drag.moved)
        {
            return true;
        }
        let Some(line) = self.pointer_point(cx).map(|point| point.line) else {
            return false;
        };
        self.highlighted_lines.as_ref().is_some_and(|highlighted| {
            highlighted.lines.start() != highlighted.lines.end()
                && (self
                    .logical_line(*highlighted.lines.start(), cx)
                    .contains(&line)
                    || self
                        .logical_line(*highlighted.lines.end(), cx)
                        .contains(&line))
        })
    }

    /// The grid line of a window position in the gutter left of the text.
    /// Only gutter lines that comment or open a marker are taken from the
    /// program and the selection.
    pub(crate) fn gutter_line_at(&self, position: GpuiPoint<Pixels>, cx: &App) -> Option<i32> {
        let content = self.terminal.read(cx).last_content();
        let bounds = content.terminal_bounds;
        let gutter_left = bounds.bounds.origin.x - bounds.cell_width;
        if position.x >= bounds.bounds.origin.x
            || position.x < gutter_left
            || bounds.line_height <= px(0.)
        {
            return None;
        }
        let row =
            ((position.y - bounds.bounds.origin.y + self.scroll_top) / bounds.line_height).floor();
        if row < 0. || row >= bounds.num_lines() as f32 {
            return None;
        }
        let display_offset = i32::try_from(content.display_offset).ok()?;
        let line = i32::try_from(row as i64).ok()? - display_offset;
        let takes_line = self.line_highlight_available(cx)
            || (self.gutter_marker_click.is_some() && self.gutter_marker_at(line, cx).is_some());
        takes_line.then_some(line)
    }

    fn visible_lines(&self, cx: &App) -> RangeInclusive<i32> {
        let content = self.terminal.read(cx).last_content();
        let top = -i32::try_from(content.display_offset).unwrap_or(0);
        let screen_lines = i32::try_from(content.screen_lines).unwrap_or(0);
        top..=top + screen_lines - 1
    }

    fn pointer_point(&self, cx: &App) -> Option<Point> {
        let (row, column) = self.pointer_cell?;
        let display_offset = self.terminal.read(cx).last_content().display_offset;
        let line = i32::try_from(row).ok()? - i32::try_from(display_offset).ok()?;
        Some(Point::new(line, column))
    }

    fn scrollback_line(&self, line: i32, cx: &App) -> Option<i64> {
        let content = self.terminal.read(cx).last_content();
        let history_lines =
            i64::try_from(content.total_lines).ok()? - i64::try_from(content.screen_lines).ok()?;
        Some(history_lines + i64::from(line))
    }

    fn grid_line(&self, row: i64, cx: &App) -> Option<i32> {
        let content = self.terminal.read(cx).last_content();
        let history_lines =
            i64::try_from(content.total_lines).ok()? - i64::try_from(content.screen_lines).ok()?;
        i32::try_from(row - history_lines).ok()
    }

    pub(crate) fn set_pointer_cell(
        &mut self,
        pointer_cell: Option<(usize, usize)>,
        pointer_x: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        if self.pointer_cell == pointer_cell && self.pointer_x == pointer_x {
            return;
        }
        let cell_changed = self.pointer_cell != pointer_cell;
        let hovered_handle = (
            self.shows_line_column_resize_cursor(cx),
            self.line_highlight_handle_tooltip(cx),
        );
        self.pointer_cell = pointer_cell;
        self.pointer_x = pointer_x;
        let handle_drag = self
            .line_highlight_drag
            .as_ref()
            .is_some_and(|drag| drag.grab != LineHighlightGrab::Line);
        if !cell_changed && !handle_drag {
            // Handles are in reach of part of a cell.
            if hovered_handle
                != (
                    self.shows_line_column_resize_cursor(cx),
                    self.line_highlight_handle_tooltip(cx),
                )
            {
                cx.notify();
            }
            return;
        }
        let Some(line) = self.pointer_point(cx).map(|point| point.line) else {
            return;
        };
        // A drag from a gutter marker highlights lines instead of opening the
        // marker.
        if let Some(pressed_line) = self.gutter_press
            && pressed_line != line
            && self.line_highlight_available(cx)
        {
            self.gutter_press = None;
            self.start_line_highlight(Some(pressed_line), cx);
            self.begin_gutter_line_drag(pressed_line);
        }
        let Some(drag) = &mut self.line_highlight_drag else {
            return;
        };
        match drag.grab {
            LineHighlightGrab::Line => {
                if line != drag.pressed_line {
                    drag.moved = true;
                }
                if drag.moved {
                    self.set_line_highlight_head(line, cx);
                }
            }
            edge => {
                drag.moved = true;
                if let Some(x) = self.pointer_x {
                    self.drag_line_highlight_handle(edge, line, x, cx);
                }
            }
        }
        cx.notify();
    }

    /// A drag from the gutter, which moves the highlight it started by
    /// lines.
    fn begin_gutter_line_drag(&mut self, pressed_line: i32) {
        if let Some(before) = self.line_highlight.clone() {
            self.line_highlight_drag = Some(LineHighlightDrag {
                pressed_line,
                moved: false,
                collapse_on_click: false,
                grab: LineHighlightGrab::Line,
                before,
            });
        }
    }

    pub(crate) fn set_gutter_hover_line(&mut self, line: Option<i32>, cx: &mut Context<Self>) {
        if self.gutter_hover_line != line {
            self.gutter_hover_line = line;
            cx.notify();
        }
    }

    /// The highlighted text without whitespace and box borders.
    fn line_highlight_text(&self, highlighted: &HighlightedLines, cx: &App) -> String {
        let terminal = self.terminal.read(cx);
        let num_columns = terminal.last_content().terminal_bounds.num_columns();
        let last_column = num_columns.saturating_sub(1);
        let rows = highlighted
            .lines
            .clone()
            .map(|line| {
                let start = match highlighted.start_column {
                    Some(column) if line == *highlighted.lines.start() => column,
                    _ => 0,
                };
                let end = match highlighted.end_column {
                    Some(column) if line == *highlighted.lines.end() => column,
                    _ => last_column,
                };
                (line, start..=end)
            })
            .collect::<Vec<_>>();
        terminal
            .rows_text(&rows)
            .concat()
            .chars()
            .filter(|character| !is_ignored_by_anchor(*character))
            .collect()
    }

    /// Finds the highlighted text again after a resize, once the program has
    /// redrawn: where it shows closest to where it was, else the last line.
    fn schedule_line_highlight_refind(
        &mut self,
        text: String,
        near_row: i64,
        near_column: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.line_highlight_refind = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(LINE_HIGHLIGHT_REFIND_DELAY)
                .await;
            this.update(cx, |this, cx| {
                this.line_highlight_refind = None;
                if this.line_highlight.is_none() {
                    return;
                }
                let terminal = this.terminal.read(cx);
                let found = ScreenText::new(&terminal.grid_cells()).find(
                    &text,
                    near_row,
                    near_column,
                    terminal.last_content(),
                );
                match found {
                    Some((start, end)) => this.set_found_line_highlight(start, end, cx),
                    None => {
                        let line = this.last_text_line(cx);
                        this.set_line_highlight(line, line, cx);
                    }
                }
            })
            .ok();
        }));
    }

    /// Highlights text found again from its first and last cells. An edge
    /// with only ignored characters beyond it on its logical line covers
    /// the line's full start or end.
    fn set_found_line_highlight(&mut self, start: Point, end: Point, cx: &mut Context<Self>) {
        self.set_line_highlight(start.line, end.line, cx);
        let only_ignored = |lines: RangeInclusive<i32>, keep: &dyn Fn(i32, usize) -> bool| {
            self.terminal.read(cx).grid_rows(lines).iter().all(|row| {
                row.characters
                    .iter()
                    .enumerate()
                    .filter(|(column, _)| keep(row.line, *column))
                    .all(|(_, character)| is_ignored_by_anchor(*character))
            })
        };
        let start_is_line_start = only_ignored(
            *self.logical_line(start.line, cx).start()..=start.line,
            &|line, column| line < start.line || column < start.column,
        );
        let end_is_line_end = only_ignored(
            end.line..=*self.logical_line(end.line, cx).end(),
            &|line, column| line > end.line || column > end.column,
        );
        let start = (!start_is_line_start)
            .then(|| Some((self.scrollback_line(start.line, cx)?, start.column)))
            .flatten();
        let end = (!end_is_line_end)
            .then(|| Some((self.scrollback_line(end.line, cx)?, end.column)))
            .flatten();
        if let Some(highlight) = &mut self.line_highlight {
            highlight.start = start;
            highlight.end = end;
        }
    }

    /// Looks the highlight up again, since output may have moved it, ends a
    /// mode the terminal lost focus during, and finds the highlight again
    /// after a resize. Runs before every paint.
    pub(crate) fn update_line_highlight(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.takes_mouse_for_line_highlight()
            && !self.focus_handle.is_focused(window)
            && self.context_menu.is_none()
        {
            self.exit_line_highlight(cx);
        }
        let bounds = self.terminal.read(cx).last_content().terminal_bounds;
        let grid_size = (bounds.num_columns(), bounds.num_lines());
        let resized = self.grid_size != (0, 0) && self.grid_size != grid_size;
        self.grid_size = grid_size;
        if resized
            && self.line_highlight.is_some()
            && let Some((text, near_row, near_column)) = self.line_highlight_text.take()
        {
            self.schedule_line_highlight_refind(text, near_row, near_column, cx);
        }
        self.highlighted_lines = None;
        if self.line_highlight.is_none() {
            return;
        }
        // Kept up while suspended too, as the highlight moved on to the line
        // after the commented ones.
        let highlighted = self.line_highlight_lines(cx);
        if self.line_highlight_refind.is_none()
            && let Some(highlighted) = &highlighted
            && let Some(near_row) = self.scrollback_line(*highlighted.lines.start(), cx)
        {
            self.line_highlight_text = Some((
                self.line_highlight_text(highlighted, cx),
                near_row,
                highlighted.start_column,
            ));
        }
        if self.takes_mouse_for_line_highlight() {
            self.highlighted_lines = highlighted;
        }
    }

    fn shows_workspace_actions(&self) -> bool {
        self.show_workspace_actions
            .unwrap_or_else(|| !matches!(self.mode, TerminalMode::Embedded { .. }))
    }

    const MAX_EMBEDDED_LINES: usize = 1_000;

    /// Returns the current `ContentMode` depending on the set `TerminalMode` and the current number of lines
    ///
    /// Note: Even in embedded mode, the terminal will fallback to scrollable when its content exceeds `MAX_EMBEDDED_LINES`
    pub fn content_mode(&self, window: &Window, cx: &App) -> ContentMode {
        match &self.mode {
            TerminalMode::Standalone => ContentMode::Scrollable,
            TerminalMode::Embedded {
                max_lines_when_unfocused,
            } => {
                let terminal = self.terminal.read(cx);
                let total_lines = terminal.total_lines();

                if total_lines > Self::MAX_EMBEDDED_LINES {
                    ContentMode::Scrollable
                } else {
                    let mut displayed_lines = terminal.used_lines().min(total_lines);

                    if !self.focus_handle.is_focused(window)
                        && let Some(max_lines) = max_lines_when_unfocused
                    {
                        displayed_lines = displayed_lines.min(*max_lines)
                    }

                    ContentMode::Inline {
                        displayed_lines,
                        total_lines,
                    }
                }
            }
        }
    }

    /// Sets the marked (pre-edit) text from the IME.
    pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    /// Gets the current marked range (UTF-16).
    pub(crate) fn marked_text_range(&self) -> Option<StdRange<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    /// Clears the marked (pre-edit) text state.
    pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    /// Commits (sends) the given text to the PTY. Called by InputHandler::replace_text_in_range.
    pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !self.read_only && !text.is_empty() {
            self.exit_line_highlight(cx);
            self.terminal.update(cx, |term, _| {
                term.input(text.to_string().into_bytes());
            });
        }
    }

    pub(crate) fn terminal_bounds(&self, cx: &App) -> TerminalBounds {
        self.terminal.read(cx).last_content().terminal_bounds
    }

    pub fn entity(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn has_bell(&self) -> bool {
        self.has_bell
    }

    pub fn custom_title(&self) -> Option<&str> {
        self.custom_title.as_deref()
    }

    pub fn set_custom_title(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        let label = label.filter(|l| !l.trim().is_empty());
        if self.custom_title != label {
            self.custom_title = label;
            self.needs_serialize = true;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    pub(crate) fn mark_needs_serialize(&mut self, cx: &mut Context<Self>) {
        self.needs_serialize = true;
        cx.emit(ItemEvent::UpdateTab);
    }

    pub fn is_renaming(&self) -> bool {
        self.rename_editor.is_some()
    }

    pub fn rename_editor_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.rename_editor
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
    }

    fn finish_renaming(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.rename_editor.take() else {
            return;
        };
        self.rename_editor_subscription = None;
        if save {
            let new_label = editor.read(cx).text(cx).trim().to_string();
            let label = if new_label.is_empty() {
                None
            } else {
                // Only set custom_title if the text differs from the terminal's dynamic title.
                // This prevents subtle layout changes when clicking away without making changes.
                let terminal_title = self.terminal.read(cx).title(true);
                if new_label == terminal_title {
                    None
                } else {
                    Some(new_label)
                }
            };
            self.set_custom_title(label, cx);
        }
        cx.notify();
        self.focus_handle.focus(window, cx);
    }

    pub fn rename_terminal(
        &mut self,
        _: &RenameTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal.read(cx).task().is_some() {
            return;
        }

        let current_label = self
            .custom_title
            .clone()
            .unwrap_or_else(|| self.terminal.read(cx).title(true));

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let rename_editor_subscription = cx.subscribe_in(&rename_editor, window, {
            let rename_editor = rename_editor.clone();
            move |_this, _, event, window, cx| {
                if let editor::EditorEvent::Blurred = event {
                    // Defer to let focus settle (avoids canceling during double-click).
                    let rename_editor = rename_editor.clone();
                    cx.defer_in(window, move |this, window, cx| {
                        let still_current = this
                            .rename_editor
                            .as_ref()
                            .is_some_and(|current| current == &rename_editor);
                        if still_current && !rename_editor.focus_handle(cx).is_focused(window) {
                            this.finish_renaming(false, window, cx);
                        }
                    });
                }
            }
        });

        self.rename_editor = Some(rename_editor.clone());
        self.rename_editor_subscription = Some(rename_editor_subscription);

        rename_editor.update(cx, |editor, cx| {
            editor.set_text(current_label, window, cx);
            editor.select_all(&SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    pub fn clear_bell(&mut self, cx: &mut Context<TerminalView>) {
        self.has_bell = false;
        cx.emit(Event::Wakeup);
    }

    /// Shows the context menu. The selection action runs on a selection the
    /// user made, else it starts the line highlight mode on `clicked_line`.
    pub fn deploy_context_menu(
        &mut self,
        position: GpuiPoint<Pixels>,
        has_selection: bool,
        has_user_selection: bool,
        clicked_line: Option<i32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let assistant_enabled = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<TerminalPanel>(cx))
            .is_some_and(|terminal_panel| terminal_panel.read(cx).assistant_enabled());
        let selection_action = self
            .selection_action
            .clone()
            .filter(|_| has_user_selection && self.selection_menu_entry_available(cx))
            .zip(self.selection_action_target(window, cx));
        let line_highlight_entry = self
            .selection_action
            .as_ref()
            .map(|selection_action| selection_action.label.clone())
            .filter(|_| selection_action.is_none() && self.line_highlight_available(cx))
            .zip(clicked_line);
        let terminal_view = self.self_handle.clone();
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .when(self.shows_workspace_actions(), |menu| {
                    menu.action("New Terminal", Box::new(NewTerminal::default()))
                        .action(
                            "New Center Terminal",
                            Box::new(NewCenterTerminal::default()),
                        )
                        .separator()
                })
                .action("Copy", Box::new(Copy))
                .when_some(
                    selection_action,
                    |menu,
                     (
                        SelectionAction {
                            label,
                            handler,
                            menu_action,
                            ..
                        },
                        (range, _, origin),
                    )| {
                        match menu_action {
                            Some(menu_action) => {
                                menu.action(label, menu_action.action.boxed_clone())
                            }
                            None => menu.entry(label, None, move |window, cx| {
                                handler(range, origin, window, cx)
                            }),
                        }
                    },
                )
                .when_some(line_highlight_entry, |menu, (label, line)| {
                    menu.entry(label, None, move |window, cx| {
                        terminal_view
                            .update(cx, |terminal_view, cx| {
                                terminal_view.start_line_highlight(Some(line), cx);
                                window.focus(&terminal_view.focus_handle, cx);
                            })
                            .log_err();
                    })
                })
                .when(
                    !self.read_only && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| {
                        menu.action("Paste", Box::new(Paste))
                            .action("Paste Text", Box::new(PasteText))
                    },
                )
                .action("Select All", Box::new(SelectAll))
                .when(
                    !self.read_only && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| menu.action("Clear", Box::new(Clear)),
                )
                .when(
                    assistant_enabled && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| {
                        menu.separator()
                            .when(!self.read_only, |menu| {
                                menu.action("Inline Assist", Box::new(InlineAssist::default()))
                            })
                            .when(has_selection && self.shows_workspace_actions(), |menu| {
                                menu.action("Add to Agent Thread", Box::new(AddSelectionToThread))
                            })
                    },
                )
                .when(self.shows_workspace_actions(), |menu| {
                    menu.separator().action(
                        "Close Terminal Tab",
                        Box::new(CloseActiveItem {
                            save_intent: None,
                            close_pinned: true,
                        }),
                    )
                })
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu.0.focus_handle(cx).contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );

        self.context_menu = Some((context_menu, position, subscription));
    }

    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = TerminalSettings::get_global(cx);
        let breadcrumb_visibility_changed = self.show_breadcrumbs != settings.toolbar.breadcrumbs;
        self.show_breadcrumbs = settings.toolbar.breadcrumbs;

        let should_blink = match settings.blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };
        let new_cursor_shape = settings.cursor_shape;
        let old_cursor_shape = self.cursor_shape;
        if old_cursor_shape != new_cursor_shape {
            self.cursor_shape = new_cursor_shape;
            self.terminal.update(cx, |term, _| {
                term.set_cursor_shape(self.cursor_shape);
            });
        }

        self.blink_manager.update(
            cx,
            if should_blink {
                BlinkManager::enable
            } else {
                BlinkManager::disable
            },
        );

        if breadcrumb_visibility_changed {
            cx.emit(ItemEvent::UpdateBreadcrumbs);
        }
        cx.notify();
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        if self
            .terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
        {
            self.terminal.update(cx, |term, cx| {
                term.try_keystroke(
                    &Keystroke::parse("ctrl-cmd-space").unwrap(),
                    TerminalSettings::get_global(cx).option_as_meta,
                )
            });
        } else {
            window.show_character_palette();
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.select_all());
        cx.notify();
    }

    fn rerun_task(&mut self, _: &RerunTask, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let task = self
            .terminal
            .read(cx)
            .task()
            .map(|task| terminal_rerun_override(&task.spawned_task.id))
            .unwrap_or_default();
        window.dispatch_action(Box::new(task), cx);
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.scroll_top = px(0.);
        self.terminal.update(cx, |term, _| term.clear());
        cx.notify();
    }

    fn max_scroll_top(&self, cx: &App) -> Pixels {
        let terminal = self.terminal.read(cx);

        let Some(block) = self.block_below_cursor.as_ref() else {
            return Pixels::ZERO;
        };

        let line_height = terminal.last_content().terminal_bounds.line_height;
        let viewport_lines = terminal.viewport_lines();
        let cursor_line = viewport_line_for_point(
            terminal.last_content.cursor.point,
            terminal.last_content.display_offset,
        )
        .unwrap_or_default();
        let max_scroll_top_in_lines =
            (block.height as usize).saturating_sub(viewport_lines.saturating_sub(cursor_line + 1));

        max_scroll_top_in_lines as f32 * line_height
    }

    fn scroll_wheel(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let terminal_content = self.terminal.read(cx).last_content();

        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let line_height = terminal_content.terminal_bounds.line_height;
            let y_delta = event.delta.pixel_delta(line_height).y;
            if y_delta < Pixels::ZERO || self.scroll_top > Pixels::ZERO {
                self.scroll_top = cmp::max(
                    Pixels::ZERO,
                    cmp::min(self.scroll_top - y_delta, self.max_scroll_top(cx)),
                );
                cx.notify();
                return;
            }
        }
        // Not `mouse_input_mode()`: under local selection the wheel still
        // reports to the program, so TUIs keep scrolling their own view.
        let mode = if self.read_only {
            MouseInputMode::LocalSelection
        } else {
            MouseInputMode::ReportToTerminal
        };
        self.terminal.update(cx, |term, cx| {
            term.scroll_wheel(
                event,
                TerminalSettings::get_global(cx).scroll_multiplier.max(0.01),
                mode,
            )
        });
    }

    fn is_alt_screen(&self, cx: &App) -> bool {
        self.terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
    }

    fn scroll_line_up(&mut self, _: &ScrollLineUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some()
            && terminal_content.display_offset == 0
            && self.scroll_top > Pixels::ZERO
        {
            let line_height = terminal_content.terminal_bounds.line_height;
            self.scroll_top = cmp::max(self.scroll_top - line_height, Pixels::ZERO);
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_up());
        cx.notify();
    }

    fn scroll_line_down(&mut self, _: &ScrollLineDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let max_scroll_top = self.max_scroll_top(cx);
            if self.scroll_top < max_scroll_top {
                let line_height = terminal_content.terminal_bounds.line_height;
                self.scroll_top = cmp::min(self.scroll_top + line_height, max_scroll_top);
            }
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_down());
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        if self.scroll_top == Pixels::ZERO {
            self.terminal.update(cx, |term, _| term.scroll_page_up());
        } else {
            let line_height = self
                .terminal
                .read(cx)
                .last_content
                .terminal_bounds
                .line_height();
            let visible_block_lines = (self.scroll_top / line_height) as usize;
            let viewport_lines = self.terminal.read(cx).viewport_lines();
            let visible_content_lines = viewport_lines - visible_block_lines;

            if visible_block_lines >= viewport_lines {
                self.scroll_top = ((visible_block_lines - viewport_lines) as f32) * line_height;
            } else {
                self.scroll_top = px(0.);
                self.terminal
                    .update(cx, |term, _| term.scroll_up_by(visible_content_lines));
            }
        }
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_page_down());
        let terminal = self.terminal.read(cx);
        if terminal.last_content().display_offset < terminal.viewport_lines() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_top());
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_bottom());
        if self.block_below_cursor.is_some() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    fn toggle_vi_mode(&mut self, _: &ToggleViMode, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.toggle_vi_mode());
        cx.notify();
    }

    pub fn should_show_cursor(&self, focused: bool, cx: &mut Context<Self>) -> bool {
        // Hide cursor when in embedded mode and not focused (read-only output like Agent panel)
        if let TerminalMode::Embedded { .. } = &self.mode {
            if !focused {
                return false;
            }
        }

        // For Standalone mode: always show cursor when not focused or in special modes
        if !focused
            || self
                .terminal
                .read(cx)
                .last_content
                .mode
                .contains(Modes::ALT_SCREEN)
        {
            return true;
        }

        // When focused, check blinking settings and blink manager state
        match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => true,
            TerminalBlink::TerminalControlled => {
                !self.blinking_terminal_enabled || self.blink_manager.read(cx).visible()
            }
            TerminalBlink::On => self.blink_manager.read(cx).visible(),
        }
    }

    pub fn pause_cursor_blinking(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn set_block_below_cursor(
        &mut self,
        block: BlockProperties,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.block_below_cursor = Some(Rc::new(block));
        self.scroll_to_bottom(&ScrollToBottom, window, cx);
        cx.notify();
    }

    pub fn clear_block_below_cursor(&mut self, cx: &mut Context<Self>) {
        self.block_below_cursor = None;
        self.scroll_top = Pixels::ZERO;
        cx.notify();
    }

    ///Attempt to paste the clipboard into the terminal
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.copy(None));
        cx.notify();
    }

    /// Specific handler for the [`editor::actions::Copy`] action in order for
    /// the `Edit > Copy` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_copy(
        &mut self,
        _: &editor::actions::Copy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy(&Copy, window, cx);
    }

    ///Attempt to paste the clipboard into the terminal
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        self.exit_line_highlight(cx);

        match clipboard.entries().first() {
            Some(ClipboardEntry::Image(image)) if !image.bytes.is_empty() => {
                self.forward_ctrl_v(cx);
            }
            Some(ClipboardEntry::ExternalPaths(paths)) => {
                self.add_paths_to_terminal(paths.paths(), window, cx);
            }
            _ => {
                if let Some(text) = clipboard.text() {
                    self.terminal
                        .update(cx, |terminal, _cx| terminal.paste(&text));
                }
            }
        }
    }

    /// Specific handler for the [`editor::actions::Paste`] action in order for
    /// the `Edit > Paste` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_paste(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste(&Paste, window, cx);
    }

    ///Attempt to paste the clipboard text into the terminal
    fn paste_text(&mut self, _: &PasteText, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        if let Some(text) = clipboard.text() {
            self.exit_line_highlight(cx);
            self.terminal
                .update(cx, |terminal, _cx| terminal.paste(&text));
        }
    }

    /// Emits a raw Ctrl+V so TUI agents can read the OS clipboard directly
    /// and attach images using their native workflows.
    fn forward_ctrl_v(&self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| {
            term.input(vec![0x16]);
        });
    }

    pub fn add_paths_to_terminal(&self, paths: &[PathBuf], window: &mut Window, cx: &mut App) {
        if self.read_only {
            return;
        }
        let mut text = paths
            .iter()
            .filter_map(|path| Some(format!(" {}", shlex::try_quote(path.to_str()?).ok()?)))
            .collect::<String>();
        text.push(' ');
        window.focus(&self.focus_handle(cx), cx);
        self.terminal.update(cx, |terminal, _| {
            terminal.paste(&text);
        });
    }

    fn send_text(&mut self, text: &SendText, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        self.exit_line_highlight(cx);
        self.clear_bell(cx);
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
        self.terminal.update(cx, |term, _| {
            term.input(text.0.to_string().into_bytes());
        });
    }

    fn send_keystroke(&mut self, text: &SendKeystroke, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(keystroke) = Keystroke::parse(&text.0).log_err() {
            self.clear_bell(cx);
            self.blink_manager.update(cx, BlinkManager::pause_blinking);
            self.exit_line_highlight(cx);
            self.process_keystroke(&keystroke, cx);
        }
    }

    fn dispatch_context(&self, cx: &App) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("Terminal");

        if self.terminal.read(cx).vi_mode_enabled() {
            dispatch_context.add("vi_mode");
        }

        let mode = self.terminal.read(cx).last_content.mode;
        dispatch_context.set(
            "screen",
            if mode.contains(Modes::ALT_SCREEN) {
                "alt"
            } else {
                "normal"
            },
        );

        if mode.contains(Modes::APP_CURSOR) {
            dispatch_context.add("DECCKM");
        }
        if mode.contains(Modes::APP_KEYPAD) {
            dispatch_context.add("DECPAM");
        } else {
            dispatch_context.add("DECPNM");
        }
        if mode.contains(Modes::SHOW_CURSOR) {
            dispatch_context.add("DECTCEM");
        }
        if mode.contains(Modes::LINE_WRAP) {
            dispatch_context.add("DECAWM");
        }
        if mode.contains(Modes::ORIGIN) {
            dispatch_context.add("DECOM");
        }
        if mode.contains(Modes::INSERT) {
            dispatch_context.add("IRM");
        }
        //LNM is apparently the name for this. https://vt100.net/docs/vt510-rm/LNM.html
        if mode.contains(Modes::LINE_FEED_NEW_LINE) {
            dispatch_context.add("LNM");
        }
        if mode.contains(Modes::FOCUS_IN_OUT) {
            dispatch_context.add("report_focus");
        }
        if mode.contains(Modes::ALTERNATE_SCROLL) {
            dispatch_context.add("alternate_scroll");
        }
        if mode.contains(Modes::BRACKETED_PASTE) {
            dispatch_context.add("bracketed_paste");
        }
        if mode.intersects(Modes::MOUSE_MODE) {
            dispatch_context.add("any_mouse_reporting");
        }
        {
            let mouse_reporting = if mode.contains(Modes::MOUSE_REPORT_CLICK) {
                "click"
            } else if mode.contains(Modes::MOUSE_DRAG) {
                "drag"
            } else if mode.contains(Modes::MOUSE_MOTION) {
                "motion"
            } else {
                "off"
            };
            dispatch_context.set("mouse_reporting", mouse_reporting);
        }
        {
            let format = if mode.contains(Modes::SGR_MOUSE) {
                "sgr"
            } else if mode.contains(Modes::UTF8_MOUSE) {
                "utf8"
            } else {
                "normal"
            };
            dispatch_context.set("mouse_format", format);
        };

        if self.terminal.read(cx).last_content.selection.is_some() {
            dispatch_context.add("selection");
        }
        if self.takes_mouse_for_line_highlight() {
            dispatch_context.add("line_highlight");
        }

        dispatch_context
    }

    fn set_terminal(
        &mut self,
        terminal: Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<TerminalView>,
    ) {
        self._terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, self.workspace.clone(), window, cx);
        self.terminal = terminal;
    }

    fn rerun_button(&self, task: &TaskState) -> Option<IconButton> {
        if self.read_only || !task.spawned_task.show_rerun {
            return None;
        }

        let task_id = task.spawned_task.id.clone();
        Some(
            IconButton::new("rerun-icon", IconName::Rerun)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Compact)
                .icon_color(Color::Default)
                .shape(ui::IconButtonShape::Square)
                .tooltip(move |_window, cx| Tooltip::for_action("Rerun task", &RerunTask, cx))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(terminal_rerun_override(&task_id)), cx);
                }),
        )
    }
}

fn terminal_rerun_override(task: &TaskId) -> zed_actions::Rerun {
    zed_actions::Rerun {
        task_id: Some(task.0.clone()),
        allow_concurrent_runs: Some(true),
        use_new_terminal: Some(false),
        reevaluate_context: false,
    }
}

fn subscribe_for_terminal_events(
    terminal: &Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<TerminalView>,
) -> Vec<Subscription> {
    let terminal_subscription = cx.observe(terminal, |_, _, cx| cx.notify());
    let mut previous_cwd = None;
    let terminal_events_subscription = cx.subscribe_in(
        terminal,
        window,
        move |terminal_view, terminal, event, window, cx| {
            let current_cwd = terminal.read(cx).working_directory();
            if current_cwd != previous_cwd {
                previous_cwd = current_cwd;
                terminal_view.needs_serialize = true;
            }

            match event {
                Event::Wakeup => {
                    cx.notify();
                    window.invalidate_character_coordinates();
                    cx.emit(Event::Wakeup);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.emit(SearchEvent::MatchesInvalidated);
                }

                Event::OutputReplaced => {
                    terminal_view.hover = None;
                    terminal_view.hover_tooltip_update = Task::ready(());
                    terminal_view.scroll_top = Pixels::ZERO;
                    terminal_view.scroll_handle.future_display_offset.set(None);
                    terminal_view.scroll_handle.update(terminal.read(cx));
                    terminal_view.block_below_cursor = None;
                    window.invalidate_character_coordinates();
                    cx.emit(SearchEvent::MatchesInvalidated);
                    cx.emit(SearchEvent::ActiveMatchChanged);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }

                Event::Bell => {
                    terminal_view.has_bell = true;
                    if let TerminalBell::System = TerminalSettings::get_global(cx).bell {
                        window.play_system_bell();
                    }
                    cx.emit(Event::Wakeup);
                }

                Event::BlinkChanged(blinking) => {
                    terminal_view.blinking_terminal_enabled = *blinking;

                    // If in terminal-controlled mode and focused, update blink manager
                    if matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::TerminalControlled
                    ) && terminal_view.focus_handle.is_focused(window)
                    {
                        terminal_view.blink_manager.update(cx, |manager, cx| {
                            if *blinking {
                                manager.enable(cx);
                            } else {
                                manager.disable(cx);
                            }
                        });
                    }
                }

                Event::TitleChanged => {
                    cx.emit(ItemEvent::UpdateTab);
                }

                Event::NewNavigationTarget(maybe_navigation_target) => {
                    match maybe_navigation_target
                        .as_ref()
                        .zip(terminal.read(cx).last_content.last_hovered_word.as_ref())
                    {
                        Some((MaybeNavigationTarget::Url(url), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = Some(HoverTarget {
                                    tooltip: url.clone(),
                                    hovered_word: hovered_word.clone(),
                                });
                                terminal_view.hover_tooltip_update = Task::ready(());
                                cx.notify();
                            }
                        }
                        Some((MaybeNavigationTarget::PathLike(path_like_target), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = None;
                                terminal_view.hover_tooltip_update = hover_path_like_target(
                                    &workspace,
                                    hovered_word.clone(),
                                    path_like_target,
                                    cx,
                                );
                                cx.notify();
                            }
                        }
                        None => {
                            terminal_view.hover = None;
                            terminal_view.hover_tooltip_update = Task::ready(());
                            cx.notify();
                        }
                    }
                }

                Event::Open(maybe_navigation_target) => match maybe_navigation_target {
                    MaybeNavigationTarget::Url(url) => cx.open_url(url),
                    MaybeNavigationTarget::PathLike(path_like_target) => open_path_like_target(
                        &workspace,
                        terminal_view,
                        path_like_target,
                        window,
                        cx,
                    ),
                },
                Event::BreadcrumbsChanged => cx.emit(ItemEvent::UpdateBreadcrumbs),
                Event::CloseTerminal => cx.emit(ItemEvent::CloseItem),
                Event::SelectionsChanged => {
                    window.invalidate_character_coordinates();
                    cx.emit(SearchEvent::ActiveMatchChanged)
                }
            }
        },
    );
    vec![terminal_subscription, terminal_events_subscription]
}

fn regex_search_for_query(query: &SearchQuery) -> Option<Search> {
    let str = query.as_str();
    if query.is_regex() {
        if str == "." {
            return None;
        }
        Search::new(str)
    } else {
        Search::new(&regex::escape(str))
    }
}

#[derive(Default)]
struct TerminalScrollbarSettingsWrapper;

impl ScrollbarVisibility for TerminalScrollbarSettingsWrapper {
    fn visibility(&self, cx: &App) -> scrollbars::ShowScrollbar {
        TerminalSettings::get_global(cx)
            .scrollbar
            .show
            .map(ui_scrollbar_settings_from_raw)
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl TerminalView {
    /// Attempts to process a keystroke in the terminal. Returns true if handled.
    ///
    /// In vi mode, explicitly triggers a re-render because vi navigation (like j/k)
    /// updates the cursor locally without sending data to the shell, so there's no
    /// shell output to automatically trigger a re-render.
    fn process_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        if self.read_only && !self.terminal.read(cx).vi_mode_enabled() {
            return false;
        }
        let (handled, vi_mode_enabled) = self.terminal.update(cx, |term, cx| {
            (
                term.try_keystroke(keystroke, TerminalSettings::get_global(cx).option_as_meta),
                term.vi_mode_enabled(),
            )
        });

        if handled && vi_mode_enabled {
            cx.notify();
        }

        handled
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_bell(cx);
        // Keys that the line highlight mode binds never get here, and any
        // other key ends the mode and types into the program as usual.
        self.exit_line_highlight(cx);
        self.pause_cursor_blinking(window, cx);

        if event.prefer_character_input
            && event.keystroke.key_char.is_some()
            && !self.terminal.read(cx).vi_mode_enabled()
        {
            return;
        }

        if self.process_keystroke(&event.keystroke, cx) {
            cx.stop_propagation();
        }
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _| {
            terminal.set_cursor_shape(self.cursor_shape);
            if !self.read_only {
                terminal.focus_in();
            }
        });

        let should_blink = match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };

        if should_blink {
            self.blink_manager.update(cx, BlinkManager::enable);
        }

        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn focus_out(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // The mode carries on while its action has focus, or the context
        // menu opened from it.
        if !self.line_highlight_suspended && self.context_menu.is_none() {
            self.exit_line_highlight(cx);
        }
        self.blink_manager.update(cx, BlinkManager::disable);
        self.terminal.update(cx, |terminal, _| {
            if !self.read_only {
                terminal.focus_out();
            }
            terminal.set_cursor_shape(CursorShape::Hollow);
        });
        cx.notify();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // TODO: this should be moved out of render
        self.scroll_handle.update(self.terminal.read(cx));

        if let Some(new_display_offset) = self.scroll_handle.future_display_offset.take() {
            self.terminal.update(cx, |term, _| {
                let delta = new_display_offset as i32 - term.last_content.display_offset as i32;
                match delta.cmp(&0) {
                    cmp::Ordering::Greater => term.scroll_up_by(delta as usize),
                    cmp::Ordering::Less => term.scroll_down_by(-delta as usize),
                    cmp::Ordering::Equal => {}
                }
            });
        }

        let terminal_handle = self.terminal.clone();
        let terminal_view_handle = cx.entity();

        let focused = self.focus_handle.is_focused(window);
        let content_mode = self.content_mode(window, cx);
        let content_corner_radii = self.content_background_corner_radii(&content_mode, cx);

        div()
            .id("terminal-view")
            .size_full()
            .relative()
            .track_focus(&self.focus_handle(cx))
            .key_context(self.dispatch_context(cx))
            .on_action(cx.listener(TerminalView::send_text))
            .on_action(cx.listener(TerminalView::send_keystroke))
            .on_action(cx.listener(TerminalView::copy))
            .on_action(cx.listener(TerminalView::editor_copy))
            .on_action(cx.listener(TerminalView::paste))
            .on_action(cx.listener(TerminalView::editor_paste))
            .on_action(cx.listener(TerminalView::paste_text))
            .on_action(cx.listener(TerminalView::clear))
            .on_action(cx.listener(TerminalView::scroll_line_up))
            .on_action(cx.listener(TerminalView::scroll_line_down))
            .on_action(cx.listener(TerminalView::scroll_page_up))
            .on_action(cx.listener(TerminalView::scroll_page_down))
            .on_action(cx.listener(TerminalView::scroll_to_top))
            .on_action(cx.listener(TerminalView::scroll_to_bottom))
            .on_action(cx.listener(TerminalView::toggle_vi_mode))
            .on_action(cx.listener(TerminalView::show_character_palette))
            .on_action(cx.listener(TerminalView::select_all))
            .on_action(cx.listener(TerminalView::rerun_task))
            .on_action(cx.listener(TerminalView::rename_terminal))
            .when(self.takes_mouse_for_line_highlight(), |this| {
                this.on_action(cx.listener(|this, _: &MoveLineHighlightUp, _, cx| {
                    this.move_line_highlight(false, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveLineHighlightDown, _, cx| {
                    this.move_line_highlight(true, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveLineHighlightEndUp, _, cx| {
                    this.move_line_highlight_end(false, false, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveLineHighlightEndDown, _, cx| {
                    this.move_line_highlight_end(false, true, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveLineHighlightFarEndUp, _, cx| {
                    this.move_line_highlight_end(true, false, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveLineHighlightFarEndDown, _, cx| {
                    this.move_line_highlight_end(true, true, cx)
                }))
                .on_action(cx.listener(|this, _: &ExtendLineHighlightByWord, _, cx| {
                    this.move_line_highlight_end_by_word(true, cx)
                }))
                .on_action(cx.listener(|this, _: &TrimLineHighlightByWord, _, cx| {
                    this.move_line_highlight_end_by_word(false, cx)
                }))
                .on_action(cx.listener(Self::confirm_line_highlight))
                .on_action(
                    cx.listener(|this, _: &MoveLineHighlightStartLeftByWord, _, cx| {
                        this.move_line_highlight_start_by_word(false, cx)
                    }),
                )
                .on_action(
                    cx.listener(|this, _: &MoveLineHighlightStartRightByWord, _, cx| {
                        this.move_line_highlight_start_by_word(true, cx)
                    }),
                )
                .on_action(cx.listener(|this, _: &ExitLineHighlight, _, cx| {
                    if !this.cancel_line_highlight_drag(cx) {
                        this.exit_line_highlight(cx)
                    }
                }))
            })
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let gutter_line = this.gutter_line_at(event.position, cx);
                    if gutter_line.is_none()
                        && this
                            .terminal
                            .read(cx)
                            .mouse_mode(event.modifiers.shift, this.mouse_input_mode())
                    {
                        return;
                    }
                    let clicked_line = gutter_line.or_else(|| {
                        let terminal = this.terminal.read(cx);
                        let mut position = event.position;
                        position.y += this.scroll_top;
                        Some(terminal.grid_point_at(position).line)
                    });
                    let had_selection = this.terminal.read(cx).last_content.selection.is_some();
                    // A right-click in the gutter or the highlight mode is
                    // about lines, not the word under it.
                    let selects_word = !had_selection
                        && gutter_line.is_none()
                        && !this.takes_mouse_for_line_highlight();
                    if selects_word {
                        this.terminal.update(cx, |terminal, _| {
                            terminal.select_word_at_event_position(event);
                        });
                    }
                    let has_selection = had_selection
                        && this
                            .terminal
                            .read(cx)
                            .last_content
                            .selection_text
                            .as_ref()
                            .is_some_and(|text| !text.is_empty());
                    // The gutter's entry always starts the mode on its line.
                    this.deploy_context_menu(
                        event.position,
                        has_selection || selects_word,
                        has_selection && gutter_line.is_none(),
                        clicked_line,
                        window,
                        cx,
                    );
                    cx.notify();
                }),
            )
            .child(
                // TODO: Oddly this wrapper div is needed for TerminalElement to not steal events from the context menu
                div()
                    .id("terminal-view-container")
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    .when_some(self.background_corner_radii, |this, radii| {
                        this.rounded_tl(radii.top_left)
                            .rounded_tr(radii.top_right)
                            .rounded_bl(radii.bottom_left)
                            .rounded_br(radii.bottom_right)
                    })
                    .child(
                        div()
                            .size_full()
                            .pt(self.content_top_padding)
                            .bg(cx.theme().colors().terminal_background)
                            .rounded_tl(content_corner_radii.top_left)
                            .rounded_tr(content_corner_radii.top_right)
                            .rounded_bl(content_corner_radii.bottom_left)
                            .rounded_br(content_corner_radii.bottom_right)
                            .child(TerminalElement::new(
                                terminal_handle,
                                terminal_view_handle,
                                self.workspace.clone(),
                                self.focus_handle.clone(),
                                focused,
                                self.should_show_cursor(focused, cx),
                                self.block_below_cursor.clone(),
                                self.mode.clone(),
                            )),
                    )
                    .when(content_mode.is_scrollable(), |div| {
                        let colors = cx.theme().colors();
                        div.custom_scrollbars(
                            Scrollbars::for_settings::<TerminalScrollbarSettingsWrapper>()
                                .show_along(ScrollAxes::Vertical)
                                .with_stable_track_along(
                                    ScrollAxes::Vertical,
                                    colors.editor_background,
                                )
                                .tracked_scroll_handle(&self.scroll_handle),
                            window,
                            cx,
                        )
                    }),
            )
            .when(self.takes_mouse_for_line_highlight(), |this| {
                let colors = cx.theme().colors();
                let mode_label = self
                    .selection_action
                    .as_ref()
                    .map(|selection_action| selection_action.mode_label.clone())
                    .unwrap_or_default();
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .border_1()
                        .border_color(colors.text_accent),
                )
                .child(
                    div()
                        .id("line-highlight-mode-label")
                        .absolute()
                        .top_1()
                        .right_3()
                        .child(
                            Label::new(mode_label)
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                        .tooltip(Tooltip::text(LINE_HIGHLIGHT_KEYS)),
                )
            })
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Item for TerminalView {
    type Event = ItemEvent;

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let terminal = self.terminal().read(cx);
            let title = terminal.title(false);
            let pid = terminal.pid_getter()?.fallback_pid();

            move |_, _| {
                v_flex()
                    .gap_1()
                    .child(Label::new(title.clone()))
                    .child(h_flex().flex_grow_1().child(Divider::horizontal()))
                    .child(
                        Label::new(format!("Process ID (PID): {}", pid))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let terminal = self.terminal().read(cx);
        let title = self
            .custom_title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| terminal.title(true));

        let (icon, icon_color, rerun_button) = match terminal.task() {
            Some(terminal_task) => match &terminal_task.status {
                TaskStatus::Running => (
                    IconName::PlayFilled,
                    Color::Disabled,
                    self.rerun_button(terminal_task),
                ),
                TaskStatus::Unknown => (
                    IconName::Warning,
                    Color::Warning,
                    self.rerun_button(terminal_task),
                ),
                TaskStatus::Completed { success } => {
                    let rerun_button = self.rerun_button(terminal_task);

                    if *success {
                        (IconName::Check, Color::Success, rerun_button)
                    } else {
                        (IconName::XCircle, Color::Error, rerun_button)
                    }
                }
            },
            None => (IconName::Terminal, Color::Muted, None),
        };

        let self_handle = self.self_handle.clone();
        h_flex()
            .gap_1()
            .group("term-tab-icon")
            .when(!params.selected, |this| {
                this.track_focus(&self.focus_handle)
            })
            .on_action(move |action: &RenameTerminal, window, cx| {
                self_handle
                    .update(cx, |this, cx| this.rename_terminal(action, window, cx))
                    .ok();
            })
            .child(
                h_flex()
                    .group("term-tab-icon")
                    .child(
                        div()
                            .when(rerun_button.is_some(), |this| {
                                this.hover(|style| style.invisible().w_0())
                            })
                            .child(Icon::new(icon).color(icon_color)),
                    )
                    .when_some(rerun_button, |this, rerun_button| {
                        this.child(
                            div()
                                .absolute()
                                .visible_on_hover("term-tab-icon")
                                .child(rerun_button),
                        )
                    }),
            )
            .child(
                div()
                    .relative()
                    .child(
                        Label::new(title)
                            .single_line()
                            .color(params.text_color())
                            .when(self.is_renaming(), |this| this.alpha(0.)),
                    )
                    .when_some(self.rename_editor.clone(), |this, editor| {
                        let self_handle = self.self_handle.clone();
                        let self_handle_cancel = self.self_handle.clone();
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .child(editor)
                                .on_action(move |_: &menu::Confirm, window, cx| {
                                    self_handle
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(true, window, cx)
                                        })
                                        .ok();
                                })
                                .on_action(move |_: &menu::Cancel, window, cx| {
                                    self_handle_cancel
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(false, window, cx)
                                        })
                                        .ok();
                                }),
                        )
                    }),
            )
            .into_any()
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        if let Some(custom_title) = self.custom_title.as_ref().filter(|l| !l.trim().is_empty()) {
            return custom_title.clone().into();
        }
        let terminal = self.terminal().read(cx);
        terminal.title(detail == 0).into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn handle_drop(
        &self,
        active_pane: &Pane,
        dropped: &dyn Any,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if self.read_only {
            return false;
        }
        let Some(project) = self.project.upgrade() else {
            return false;
        };

        if let Some(paths) = dropped.downcast_ref::<ExternalPaths>() {
            let is_local = project.read(cx).is_local();
            if is_local {
                self.add_paths_to_terminal(paths.paths(), window, cx);
                return true;
            }

            return false;
        } else if let Some(tab) = dropped.downcast_ref::<DraggedTab>() {
            let Some(self_handle) = self.self_handle.upgrade() else {
                return false;
            };

            let Some(workspace) = self.workspace.upgrade() else {
                return false;
            };

            let Some(this_pane) = workspace.read(cx).pane_for(&self_handle) else {
                return false;
            };

            let item = if tab.pane == this_pane {
                active_pane.item_for_index(tab.ix)
            } else {
                tab.pane.read(cx).item_for_index(tab.ix)
            };

            let Some(item) = item else {
                return false;
            };

            if item.downcast::<TerminalView>().is_some() {
                let Some(split_direction) = active_pane.drag_split_direction() else {
                    return false;
                };

                let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
                    return false;
                };

                if !terminal_panel.read(cx).center.panes().contains(&&this_pane) {
                    return false;
                }

                let source = tab.pane.clone();
                let item_id_to_move = item.item_id();
                let is_zoomed = {
                    let terminal_panel = terminal_panel.read(cx);
                    if terminal_panel.active_pane == this_pane {
                        active_pane.is_zoomed()
                    } else {
                        terminal_panel.active_pane.read(cx).is_zoomed()
                    }
                };

                let workspace = workspace.downgrade();
                let terminal_panel = terminal_panel.downgrade();
                // Defer the split operation to avoid re-entrancy panic.
                // The pane may be the one currently being updated, so we cannot
                // call mark_positions (via split) synchronously.
                window
                    .spawn(cx, async move |cx| {
                        cx.update(|window, cx| {
                            let Ok(new_pane) = terminal_panel.update(cx, |terminal_panel, cx| {
                                let new_pane = terminal_panel::new_terminal_pane(
                                    workspace, project, is_zoomed, window, cx,
                                );
                                terminal_panel.apply_tab_bar_buttons(&new_pane, cx);
                                terminal_panel.center.split(
                                    &this_pane,
                                    &new_pane,
                                    split_direction,
                                    cx,
                                );
                                anyhow::Ok(new_pane)
                            }) else {
                                return;
                            };

                            let Some(new_pane) = new_pane.log_err() else {
                                return;
                            };

                            workspace::move_item(
                                &source,
                                &new_pane,
                                item_id_to_move,
                                new_pane.read(cx).active_item_index(),
                                true,
                                window,
                                cx,
                            );
                        })
                        .ok();
                    })
                    .detach();

                return true;
            } else {
                if let Some(project_path) = item.project_path(cx)
                    && let Some(path) = project.read(cx).absolute_path(&project_path, cx)
                {
                    self.add_paths_to_terminal(&[path], window, cx);
                    return true;
                }
            }

            return false;
        } else if let Some(selection) = dropped.downcast_ref::<DraggedSelection>() {
            let project = project.read(cx);
            let paths = selection
                .items()
                .map(|selected_entry| selected_entry.entry_id)
                .filter_map(|entry_id| project.path_for_entry(entry_id, cx))
                .filter_map(|project_path| project.absolute_path(&project_path, cx))
                .collect::<Vec<_>>();

            if !paths.is_empty() {
                self.add_paths_to_terminal(&paths, window, cx);
            }

            return true;
        } else if let Some(&entry_id) = dropped.downcast_ref::<ProjectEntryId>() {
            let project = project.read(cx);
            if let Some(path) = project
                .path_for_entry(entry_id, cx)
                .and_then(|project_path| project.absolute_path(&project_path, cx))
            {
                self.add_paths_to_terminal(&[path], window, cx);
            }

            return true;
        }

        false
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn gpui::Action>)> {
        let terminal = self.terminal.read(cx);
        if terminal.task().is_none() {
            vec![("Rename".into(), Box::new(RenameTerminal))]
        } else {
            Vec::new()
        }
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let Ok(terminal) = self.project.update(cx, |project, cx| {
            let cwd = project
                .active_project_directory(cx)
                .map(|it| it.to_path_buf());
            project.clone_terminal(self.terminal(), cx, cwd)
        }) else {
            return Task::ready(None);
        };
        cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal.await.log_err()?;
            this.update_in(cx, |this, window, cx| {
                cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        this.workspace.clone(),
                        workspace_id,
                        this.project.clone(),
                        window,
                        cx,
                    )
                })
            })
            .ok()
        })
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match self.terminal.read(cx).task() {
            Some(task) => task.status == TaskStatus::Running,
            None => self.has_bell(),
        }
    }

    fn has_conflict(&self, _cx: &App) -> bool {
        false
    }

    fn can_save_as(&self, _cx: &App) -> bool {
        false
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if self.show_breadcrumbs && !self.terminal().read(cx).breadcrumb_text.trim().is_empty() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        Some((
            vec![HighlightedText {
                text: self.terminal().read(cx).breadcrumb_text.clone().into(),
                highlights: vec![],
            }],
            None,
        ))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal().read(cx).task().is_none() {
            if let Some((new_id, old_id)) = workspace.database_id().zip(self.workspace_id) {
                log::debug!(
                    "Updating workspace id for the terminal, old: {old_id:?}, new: {new_id:?}",
                );
                let db = TerminalDb::global(cx);
                let entity_id = cx.entity_id().as_u64();
                cx.background_spawn(async move {
                    db.update_workspace_id(new_id, old_id, entity_id).await
                })
                .detach();
            }
            self.workspace_id = workspace.database_id();
        }
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl SerializableItem for TerminalView {
    fn serialized_item_kind() -> &'static str {
        "Terminal"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = TerminalDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "terminals", &db, cx)
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let terminal = self.terminal().read(cx);
        if terminal.task().is_some() {
            return None;
        }

        if !self.needs_serialize {
            return None;
        }

        let workspace_id = self.workspace_id?;
        let cwd = terminal.working_directory();
        let custom_title = self.custom_title.clone();
        self.needs_serialize = false;

        let db = TerminalDb::global(cx);
        Some(cx.background_spawn(async move {
            if let Some(cwd) = cwd {
                db.save_working_directory(item_id, workspace_id, cwd)
                    .await?;
            }
            db.save_custom_title(item_id, workspace_id, custom_title)
                .await?;
            Ok(())
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        self.needs_serialize
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let (cwd, custom_title) = cx
                .update(|_window, cx| {
                    let db = TerminalDb::global(cx);
                    let from_db = db
                        .get_working_directory(item_id, workspace_id)
                        .log_err()
                        .flatten();
                    let cwd = if from_db
                        .as_ref()
                        .is_some_and(|from_db| !from_db.as_os_str().is_empty())
                    {
                        from_db
                    } else {
                        workspace
                            .upgrade()
                            .and_then(|workspace| default_working_directory(workspace.read(cx), cx))
                    };
                    let custom_title = db
                        .get_custom_title(item_id, workspace_id)
                        .log_err()
                        .flatten()
                        .filter(|title| !title.trim().is_empty());
                    (cwd, custom_title)
                })
                .ok()
                .unwrap_or((None, None));

            let terminal = project
                .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                .await?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal,
                        workspace,
                        Some(workspace_id),
                        project.downgrade(),
                        window,
                        cx,
                    );
                    if custom_title.is_some() {
                        view.custom_title = custom_title;
                    }
                    view
                })
            })
        })
    }
}

impl SearchableItem for TerminalView {
    type Match = Range;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: false,
            word: false,
            regex: true,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    /// Clear stored matches
    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.terminal().update(cx, |term, _| term.matches.clear())
    }

    /// Store matches returned from find_matches somewhere for rendering
    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        _active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.matches = matches.to_vec())
    }

    /// Returns the selection content to pre-load into this search
    fn query_suggestion(
        &mut self,
        _seed_query_override: Option<SeedQuerySetting>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.terminal()
            .read(cx)
            .last_content
            .selection_text
            .clone()
            .unwrap_or_default()
    }

    /// Focus match at given index into the Vec of matches
    fn activate_match(
        &mut self,
        index: usize,
        _: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.activate_match(index));
        cx.notify();
    }

    /// Add selections for all matches given.
    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.select_matches(matches));
        cx.notify();
    }

    /// Get all of the matches for this query, should be done on the background
    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        if let Some(s) = regex_search_for_query(&query) {
            self.terminal()
                .update(cx, |term, cx| term.find_matches(s, cx))
        } else {
            Task::ready(vec![])
        }
    }

    /// Reports back to the search toolbar what the active match should be (the selection)
    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Selection head might have a value if there's a selection that isn't
        // associated with a match. Therefore, if there are no matches, we should
        // report None, no matter the state of the terminal

        if !matches.is_empty() {
            if let Some(selection_head) = self.terminal().read(cx).selection_head {
                // If selection head is contained in a match. Return that match
                match direction {
                    Direction::Prev => {
                        // If no selection before selection head, return the first match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .rev()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() < selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(0),
                        )
                    }
                    Direction::Next => {
                        // If no selection after selection head, return the last match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() > selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(matches.len().saturating_sub(1)),
                        )
                    }
                }
            } else {
                // Matches found but no active selection, return the first last one (closest to cursor)
                Some(matches.len().saturating_sub(1))
            }
        } else {
            None
        }
    }
    fn replace(
        &mut self,
        _: &Self::Match,
        _: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        // Replacement is not supported in terminal view, so this is a no-op.
    }
}

/// Gets the working directory for the given workspace, respecting the user's settings.
/// Falls back to home directory when no project directory is available.
///
/// For remote projects, local-only resolution (home dir fallback, shell expansion,
/// local `is_dir` checks) is skipped -- returning `None` lets the remote shell
/// open in the remote user's home directory by default.
pub fn default_working_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let is_remote = workspace.project().read(cx).is_remote();
    let directory = match &TerminalSettings::get_global(cx).working_directory {
        WorkingDirectory::CurrentFileDirectory => workspace
            .project()
            .read(cx)
            .active_entry_directory(cx)
            .or_else(|| current_project_directory(workspace, cx)),
        WorkingDirectory::CurrentProjectDirectory => current_project_directory(workspace, cx),
        WorkingDirectory::FirstProjectDirectory => first_project_directory(workspace, cx),
        WorkingDirectory::AlwaysHome => None,
        WorkingDirectory::Always { directory } if !is_remote => shellexpand::full(directory)
            .ok()
            .map(|dir| Path::new(&dir.to_string()).to_path_buf())
            .filter(|dir| dir.is_dir()),
        WorkingDirectory::Always { .. } => None,
    };

    if is_remote {
        directory
    } else {
        directory.or_else(dirs::home_dir)
    }
}

fn current_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace
        .project()
        .read(cx)
        .active_project_directory(cx)
        .as_deref()
        .map(Path::to_path_buf)
        .or_else(|| first_project_directory(workspace, cx))
}

///Gets the first project's home directory, or the home directory
fn first_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let worktree = workspace.worktrees(cx).next()?.read(cx);
    let worktree_path = worktree.abs_path();
    if worktree.root_entry()?.is_dir() {
        Some(worktree_path.to_path_buf())
    } else {
        // If worktree is a file, return its parent directory
        worktree_path.parent().map(|p| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::{Entry, Project, ProjectPath, Worktree};
    use remote::RemoteClient;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use util::paths::PathStyle;
    use util::rel_path::RelPath;
    use workspace::item::test::{TestItem, TestProjectItem};
    use workspace::{AppState, MultiWorkspace, SelectedEntry};

    fn expected_drop_text(paths: &[PathBuf]) -> String {
        let mut text = String::new();
        for path in paths {
            text.push(' ');
            text.push_str(&shlex::try_quote(path.to_str().unwrap()).unwrap());
        }
        text.push(' ');
        text
    }

    fn assert_drop_writes_to_terminal(
        pane: &Entity<Pane>,
        terminal_view_index: usize,
        terminal: &Entity<Terminal>,
        dropped: &dyn Any,
        expected_text: &str,
        window: &mut Window,
        cx: &mut Context<MultiWorkspace>,
    ) {
        let _ = terminal.update(cx, |terminal, _| terminal.take_input_log());

        let handled = pane.update(cx, |pane, cx| {
            pane.item_for_index(terminal_view_index)
                .unwrap()
                .handle_drop(pane, dropped, window, cx)
        });
        assert!(handled, "handle_drop should return true for {:?}", dropped);

        let mut input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
        assert_eq!(input_log.len(), 1, "expected exactly one write to terminal");
        let written =
            String::from_utf8(input_log.remove(0)).expect("terminal write should be valid UTF-8");
        assert_eq!(written, expected_text);
    }

    // DEC private mode 1049: a program writes this to enter the alternate screen buffer.
    const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";

    // CSI `1;2A` = cursor-up with the xterm Shift modifier (`1 + 1` for Shift).
    const SHIFT_UP_ESCAPE: &[u8] = b"\x1b[1;2A";

    #[gpui::test]
    async fn replacing_output_resets_view_scrolling_hover_and_search(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        terminal.update(&mut cx, |terminal, cx| {
            terminal.write_raw_output(b"\x1b[1 q", cx);
        });
        cx.run_until_parked();
        assert!(terminal_view.read_with(&cx, |view, _| view.blinking_terminal_enabled));
        cx.update(|_, cx| {
            terminal_view.update(cx, |view, _| {
                view.scroll_top = px(25.);
                view.scroll_handle.future_display_offset.set(Some(3));
                view.hover = Some(HoverTarget {
                    tooltip: "stale".into(),
                    hovered_word: HoveredWord {
                        word: "stale".into(),
                        word_match: Range::new(
                            terminal::Point::new(0, 0),
                            terminal::Point::new(0, 5),
                        ),
                        id: 0,
                    },
                });
            });
            terminal.update(cx, |terminal, cx| {
                terminal.write_raw_output(b"old", cx);
                terminal.matches.push(Range::new(
                    terminal::Point::new(0, 0),
                    terminal::Point::new(0, 3),
                ));
                terminal
                    .replace_display_output(b"new", cx)
                    .expect("replace");
            });
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            let view = terminal_view.read(cx);
            assert_eq!(view.scroll_top, Pixels::ZERO);
            assert!(view.scroll_handle.future_display_offset.get().is_none());
            assert!(view.hover.is_none());
            assert!(view.is_read_only());
            assert!(!view.blinking_terminal_enabled);
            assert!(terminal.read(cx).matches.is_empty());
        });
    }

    #[gpui::test]
    async fn edit_menu_copy_and_paste_are_available_when_terminal_is_focused(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            assert!(window.is_action_available(&editor::actions::Copy, cx));
            assert!(window.is_action_available(&editor::actions::Paste, cx));

            cx.write_to_clipboard(gpui::ClipboardItem::new_string("foo".to_string()));
            terminal.update(cx, |terminal, _| terminal.take_input_log());
            window.dispatch_action(Box::new(editor::actions::Paste), cx);
        });
        cx.run_until_parked();

        cx.update(|_, cx| {
            let input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
            assert_eq!(input_log, vec![b"foo".to_vec()]);
        });
    }

    #[gpui::test]
    async fn read_only_blocks_input_actions_and_preserves_output(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let rerun_count = Rc::new(std::cell::Cell::new(0));
        cx.update(|cx| {
            cx.on_action({
                let rerun_count = rerun_count.clone();
                move |_: &zed_actions::Rerun, _| rerun_count.set(rerun_count.get() + 1)
            });
        });
        let (pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);

        for output in [
            b"provider output".as_slice(),
            b"\x1b[?1049hprovider alternate-screen output".as_slice(),
        ] {
            let content = cx.update(|window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.write_output(output, cx);
                    terminal.sync(window, cx);
                    terminal.get_content()
                })
            });
            cx.simulate_keystrokes("a enter ctrl-v shift-up");
            cx.update(|_, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("clipboard input".into()));
            });

            let actions: Vec<Box<dyn Action>> = vec![
                Box::new(SendText("direct input".into())),
                Box::new(SendKeystroke("enter".into())),
                Box::new(Paste),
                Box::new(editor::actions::Paste),
                Box::new(PasteText),
                Box::new(Clear),
                Box::new(ShowCharacterPalette),
                Box::new(RerunTask),
            ];
            for action in actions {
                cx.update(|window, cx| window.dispatch_action(action, cx));
                cx.run_until_parked();
            }
            cx.update(|window, cx| {
                let paths = ExternalPaths(vec![PathBuf::from(util::path!("/root.txt"))].into());
                assert!(!pane.update(cx, |pane, cx| {
                    terminal_view.update(cx, |view, cx| view.handle_drop(pane, &paths, window, cx))
                }));
                terminal_view.update(cx, |view, cx| {
                    view.add_paths_to_terminal(paths.paths(), window, cx);
                });
                terminal.update(cx, |terminal, cx| {
                    terminal.sync(window, cx);
                    assert!(terminal.take_input_log().is_empty());
                    assert!(terminal.take_pty_write_log().is_empty());
                    assert_eq!(terminal.get_content(), content);
                });
            });
        }
        assert_eq!(rerun_count.get(), 0);
    }

    #[gpui::test]
    async fn read_only_preserves_vi_navigation_without_enabling_input(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"first line\nsecond line\n", cx);
                terminal.sync(window, cx);
            });
            window.dispatch_action(Box::new(ToggleViMode), cx);
        });
        cx.run_until_parked();
        let cursor = terminal.read_with(&cx, |terminal, _| {
            assert!(terminal.vi_mode_enabled());
            terminal.last_content.cursor.point
        });
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(SendKeystroke("k".into())), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            terminal.read_with(&cx, |terminal, _| terminal.last_content.cursor.point.line),
            cursor.line - 1,
        );
        cx.simulate_keystrokes("v l");
        assert!(
            terminal
                .read_with(&cx, |terminal, _| terminal
                    .last_content
                    .selection_text
                    .clone())
                .is_some_and(|text| !text.is_empty()),
        );
        cx.simulate_keystrokes("y");
        assert!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
                .is_some_and(|text| !text.is_empty()),
        );
        cx.simulate_keystrokes("i");
        assert!(!terminal.read_with(&cx, |terminal, _| terminal.vi_mode_enabled()));
        cx.simulate_keystrokes("a enter");
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(SendKeystroke("enter".into())), cx);
        });
        cx.run_until_parked();
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_input_log())
                .is_empty()
        );
    }

    #[gpui::test]
    async fn read_only_mouse_selection_and_focus_do_not_report_input(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"\x1b[?1003h\x1b[?1006h\x1b[?1004hhello world\n", cx);
                terminal.sync(window, cx);
                terminal.take_pty_write_log();
            });
            window.blur(cx);
        });
        cx.run_until_parked();
        let bounds = terminal.read_with(&cx, |terminal, _| terminal.last_content.terminal_bounds);
        let start =
            bounds.bounds.origin + gpui::point(bounds.cell_width * 0.1, bounds.line_height * 0.5);
        let end =
            bounds.bounds.origin + gpui::point(bounds.cell_width * 5.1, bounds.line_height * 0.5);
        cx.simulate_mouse_move(start, None, gpui::Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        cx.update(|window, cx| window.dispatch_action(Box::new(Copy), cx));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("hello".into()),
        );
        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty()
        );
    }

    type SelectionActions = Rc<RefCell<Vec<Range>>>;

    fn record_selection_actions(
        terminal_view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
    ) -> SelectionActions {
        let actions = SelectionActions::default();
        terminal_view.update(cx, |terminal_view, cx| {
            let actions = actions.clone();
            terminal_view.on_selection_action(
                "Act on Lines",
                "Acting",
                move |range, _, _, _| {
                    actions.borrow_mut().push(range);
                },
                cx,
            );
        });
        actions
    }

    fn drag(from: GpuiPoint<Pixels>, to: GpuiPoint<Pixels>, cx: &mut VisualTestContext) {
        cx.simulate_mouse_move(from, None, gpui::Modifiers::default());
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(to, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
    }

    fn click(position: GpuiPoint<Pixels>, cx: &mut VisualTestContext) {
        cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
        cx.simulate_mouse_down(position, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(position, MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn selection_action_target_keeps_an_off_screen_vi_selection(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, true, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let output = (0..200)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(output.as_bytes(), cx);
                terminal.scroll_to_top();
                terminal.sync(window, cx);
                terminal.toggle_vi_mode();
                terminal.select_all();
                terminal.sync(window, cx);
            });
        });
        let content = |cx: &mut VisualTestContext| {
            terminal.read_with(cx, |terminal, _| {
                let content = &terminal.last_content;
                (
                    content.selection.map(|selection| selection.point_range()),
                    content.display_offset,
                )
            })
        };
        let (selection_before, display_offset_before) = content(&mut cx);
        let bounds = terminal.read_with(&cx, |terminal, _| terminal.last_content.terminal_bounds);
        let end_row = selection_before
            .as_ref()
            .map(|range| range.end().line + display_offset_before as i32)
            .expect("terminal should have a selection");
        assert!(
            end_row >= bounds.num_lines() as i32,
            "the selection's end should start out below the viewport"
        );

        let target = terminal_view.update_in(&mut cx, |terminal_view, window, cx| {
            terminal_view.selection_action_target(window, cx)
        });
        cx.update(|window, cx| terminal.update(cx, |terminal, cx| terminal.sync(window, cx)));

        let (range, text, origin) = target.expect("the selection should have a target");
        assert!(text.contains("line 199"));
        assert_eq!(Some(range), selection_before);
        assert!(
            bounds.bounds.contains(&origin),
            "the position should be on screen"
        );
        assert_eq!(
            content(&mut cx),
            (selection_before, display_offset_before),
            "the terminal shouldn't scroll or change its selection"
        );
    }

    const ENABLE_MOUSE_REPORTING: &str = "\x1b[?1002h\x1b[?1006h";

    const FIVE_LINES: &str = "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n";

    fn write_output(terminal: &Entity<Terminal>, text: &str, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(text.as_bytes(), cx);
                terminal.sync(window, cx);
            });
        });
        cx.run_until_parked();
    }

    /// The window position of the middle of the cell at a column and a grid
    /// line, where column -0.5 is the gutter.
    fn cell_position(
        terminal: &Entity<Terminal>,
        cx: &mut VisualTestContext,
    ) -> impl Fn(f32, i32) -> GpuiPoint<Pixels> + use<> {
        let bounds = terminal.read_with(cx, |terminal, _| terminal.last_content.terminal_bounds);
        move |column, line| {
            bounds.bounds.origin
                + gpui::point(
                    bounds.cell_width * column,
                    bounds.line_height * (line as f32 + 0.5),
                )
        }
    }

    fn highlighted(
        terminal_view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
    ) -> Option<(RangeInclusive<i32>, Option<usize>)> {
        terminal_view.read_with(cx, |terminal_view, cx| {
            terminal_view
                .line_highlight_lines(cx)
                .map(|highlighted| (highlighted.lines, highlighted.end_column))
        })
    }

    fn highlighted_lines(
        terminal_view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
    ) -> Option<RangeInclusive<i32>> {
        highlighted(terminal_view, cx).map(|(lines, _)| lines)
    }

    fn start_line_highlight(
        terminal_view: &Entity<TerminalView>,
        line: Option<i32>,
        cx: &mut VisualTestContext,
    ) {
        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.start_line_highlight(line, cx)
        });
        cx.run_until_parked();
    }

    fn has_line_highlight(
        terminal_view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
    ) -> bool {
        terminal_view.read_with(cx, |terminal_view, _| terminal_view.has_line_highlight())
    }

    fn keys(keystrokes: &str, cx: &mut VisualTestContext) {
        cx.simulate_keystrokes(keystrokes);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn keyboard_grows_and_shrinks_the_line_highlight(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(&terminal, FIVE_LINES, &mut cx);

        start_line_highlight(&terminal_view, None, &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(4..=4),
            "the mode should start on the last line with text"
        );
        keys("up k", &mut cx);
        assert_eq!(highlighted_lines(&terminal_view, &mut cx), Some(2..=2));
        keys("shift-down shift-j", &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(2..=4),
            "grow down"
        );
        keys("shift-up", &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(2..=3),
            "shrink from the bottom"
        );
        keys("alt-shift-down", &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(3..=3),
            "shrink from the top"
        );
        keys("shift-up shift-k", &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(1..=3),
            "grow up"
        );
        keys("down", &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(2..=2),
            "down should move the active end's line alone"
        );

        keys("enter", &mut cx);
        let last_column = terminal.read_with(&cx, |terminal, _| {
            terminal.last_content.terminal_bounds.num_columns() - 1
        });
        assert_eq!(
            actions.borrow().as_slice(),
            &[Range::new(Point::new(2, 0), Point::new(2, last_column))]
        );
        assert!(has_line_highlight(&terminal_view, &mut cx));
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(2..=2),
            "the highlight should stay on the line acted on"
        );
    }

    #[gpui::test]
    async fn mouse_grows_and_shrinks_the_line_highlight(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(
            &terminal,
            &format!("{ENABLE_MOUSE_REPORTING}{FIVE_LINES}"),
            &mut cx,
        );
        let cell = cell_position(&terminal, &mut cx);
        start_line_highlight(&terminal_view, None, &mut cx);
        terminal.update(&mut cx, |terminal, _| terminal.take_pty_write_log());

        click(cell(1.5, 1), &mut cx);
        assert_eq!(highlighted_lines(&terminal_view, &mut cx), Some(1..=1));
        drag(cell(1.5, 1), cell(1.5, 3), &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(1..=3),
            "grow down"
        );
        drag(cell(1.5, 3), cell(1.5, 2), &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(1..=2),
            "dragging the bottom edge up should shrink from the bottom"
        );
        drag(cell(1.5, 1), cell(1.5, 0), &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(0..=2),
            "dragging the top edge up should grow up"
        );
        drag(cell(1.5, 0), cell(1.5, 1), &mut cx);
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(1..=2),
            "dragging the top edge down should shrink from the top"
        );
        cx.simulate_mouse_move(cell(1.5, 4), None, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(1..=2),
            "hovering shouldn't move the highlight"
        );
        cx.simulate_mouse_down(cell(1.5, 4), MouseButton::Left, gpui::Modifiers::shift());
        cx.simulate_mouse_up(cell(1.5, 4), MouseButton::Left, gpui::Modifiers::shift());
        cx.run_until_parked();
        assert_eq!(
            highlighted_lines(&terminal_view, &mut cx),
            Some(1..=4),
            "shift-click should extend to the clicked line"
        );

        assert!(
            terminal.read_with(&cx, |terminal, _| terminal.last_content.selection.is_none()),
            "highlight drags shouldn't select"
        );
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty(),
            "the program shouldn't see highlight presses, drags or releases",
        );
    }

    #[gpui::test]
    async fn shift_arrows_move_the_line_highlight_end_by_words(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(&terminal, "alpha beta\r\ngamma delta\r\n", &mut cx);
        start_line_highlight(&terminal_view, Some(0), &mut cx);

        keys("shift-left", &mut cx);
        assert_eq!(highlighted(&terminal_view, &mut cx), Some((0..=0, Some(4))));
        keys("shift-left", &mut cx);
        assert_eq!(
            highlighted(&terminal_view, &mut cx),
            Some((0..=0, Some(4))),
            "the first word should stay highlighted"
        );
        keys("shift-right shift-right", &mut cx);
        assert_eq!(
            highlighted(&terminal_view, &mut cx),
            Some((0..=1, Some(4))),
            "extending past the last line should go on into the next"
        );
        keys("shift-left", &mut cx);
        assert_eq!(highlighted(&terminal_view, &mut cx), Some((0..=0, Some(9))));
        keys("shift-right enter", &mut cx);
        assert_eq!(
            actions.borrow().as_slice(),
            &[Range::new(Point::new(0, 0), Point::new(1, 4))]
        );
    }

    /// The highlight's lines and its start and end columns.
    fn highlighted_columns(
        terminal_view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
    ) -> Option<(RangeInclusive<i32>, Option<usize>, Option<usize>)> {
        terminal_view.read_with(cx, |terminal_view, cx| {
            terminal_view.line_highlight_lines(cx).map(|highlighted| {
                (
                    highlighted.lines,
                    highlighted.start_column,
                    highlighted.end_column,
                )
            })
        })
    }

    fn num_columns(terminal: &Entity<Terminal>, cx: &mut VisualTestContext) -> usize {
        terminal.read_with(cx, |terminal, _| {
            terminal.last_content.terminal_bounds.num_columns()
        })
    }

    #[gpui::test]
    async fn alt_shift_arrows_move_the_line_highlight_start_by_words(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(
            &terminal,
            "alpha beta gamma\r\ndelta epsilon\r\nzeta eta theta\r\n",
            &mut cx,
        );
        start_line_highlight(&terminal_view, Some(0), &mut cx);

        keys("alt-shift-right", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(6), None))
        );
        keys("alt-shift-right alt-shift-right", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(11), None)),
            "the start should stop at the last word"
        );
        keys("shift-left", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(11), None)),
            "the end shouldn't move before the start"
        );
        keys("alt-shift-left alt-shift-left alt-shift-left", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(0), None)),
        );
        keys("alt-shift-right alt-shift-right", &mut cx);
        keys("shift-right", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=1, Some(11), Some(4))),
            "the end should move on into the next line"
        );
        keys("shift-down", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=2, Some(11), Some(3))),
            "the end should snap to the word end before its column"
        );
        keys("shift-up", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=1, Some(11), None)),
            "without a word end before its column, the end should be the line end"
        );
        keys("alt-shift-down", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=1, Some(6), None)),
            "the start should snap to the word start before its column"
        );
        keys("alt-shift-left alt-shift-left", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=1, Some(11), None)),
            "the start should move on into the previous line"
        );
        keys("enter", &mut cx);
        assert_eq!(
            actions.borrow().last(),
            Some(&Range::new(
                Point::new(0, 11),
                Point::new(1, num_columns(&terminal, &mut cx) - 1)
            ))
        );
        start_line_highlight(&terminal_view, Some(0), &mut cx);
        keys("alt-shift-right shift-left down", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=1, None, None)),
            "a plain arrow should clear both columns"
        );
    }

    #[gpui::test]
    async fn moving_an_end_past_the_other_clears_the_columns(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(&terminal, "zeta eta theta\r\nalpha beta gamma\r\n", &mut cx);
        start_line_highlight(&terminal_view, Some(1), &mut cx);
        keys("alt-shift-right shift-left", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=1, Some(6), Some(9)))
        );
        keys("shift-up", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=1, None, None)),
            "neither edge's column belongs to its new kind of edge"
        );
    }

    #[gpui::test]
    async fn dragging_the_handles_moves_the_line_highlight_edges_by_words(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(
            &terminal,
            &format!("{ENABLE_MOUSE_REPORTING}alpha beta gamma\r\ndelta epsilon\r\n"),
            &mut cx,
        );
        let cell = cell_position(&terminal, &mut cx);
        let line_end = num_columns(&terminal, &mut cx) as f32;
        start_line_highlight(&terminal_view, Some(0), &mut cx);
        terminal.update(&mut cx, |terminal, _| terminal.take_pty_write_log());

        // The start handle at column 0 reaches into the gutter.
        drag(cell(-0.4, 0), cell(6.3, 0), &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(6), None))
        );
        drag(cell(6.0, 0), cell(40.0, 0), &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(11), None)),
            "a drag past the line end should clamp to its last word"
        );
        drag(cell(line_end, 0), cell(3.0, 0), &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(11), Some(15))),
            "the end should stop at the start's word"
        );
        drag(cell(11.0, 0), cell(-3.0, 0), &mut cx);
        drag(cell(16.0, 0), cell(10.4, 0), &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(0), Some(9))),
            "a drag past the line start should clamp to its first word"
        );
        drag(cell(10.0, 0), cell(7.0, 1), &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=1, Some(0), Some(4))),
            "a diagonal drag should move the end to the word nearest the pointer on its line"
        );

        let before = highlighted_columns(&terminal_view, &mut cx);
        cx.simulate_mouse_move(cell(0.0, 0), None, gpui::Modifiers::default());
        cx.simulate_mouse_down(cell(0.0, 0), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(
            cell(6.0, 1),
            Some(MouseButton::Left),
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=1, Some(0), Some(4))),
            "the start should follow the drag and stop at the end's word"
        );
        keys("escape", &mut cx);
        cx.simulate_mouse_up(cell(6.0, 1), MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            before,
            "Esc should restore the highlight from before the drag"
        );
        assert!(has_line_highlight(&terminal_view, &mut cx));

        assert!(terminal.read_with(&cx, |terminal, _| terminal.last_content.selection.is_none()));
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty(),
            "the program shouldn't see handle presses, drags or releases",
        );
    }

    #[gpui::test]
    async fn the_nearer_line_highlight_handle_wins(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(&terminal, "a b c\r\n", &mut cx);
        let cell = cell_position(&terminal, &mut cx);
        start_line_highlight(&terminal_view, Some(0), &mut cx);
        drag(cell(0.0, 0), cell(2.0, 0), &mut cx);
        let line_end = num_columns(&terminal, &mut cx) as f32;
        drag(cell(line_end, 0), cell(3.0, 0), &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((0..=0, Some(2), Some(2)))
        );
        let handle_at = |x: f32, cx: &mut VisualTestContext| {
            terminal_view.read_with(cx, |terminal_view, cx| {
                terminal_view.line_highlight_handle_at(0, x, cx)
            })
        };
        assert_eq!(
            handle_at(2.4, &mut cx),
            Some(LineHighlightGrab::StartHandle)
        );
        assert_eq!(handle_at(2.6, &mut cx), Some(LineHighlightGrab::EndHandle));
        assert_eq!(handle_at(1.4, &mut cx), None);
    }

    #[gpui::test]
    async fn handles_and_keys_land_on_the_same_words(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        let num_columns = num_columns(&terminal, &mut cx);
        // A word wrapped from the end of the second row onto the third.
        let wrapped = format!("a{}bcde", " ".repeat(num_columns - 3));
        write_output(&terminal, &format!("ab 日本 cd\r\n{wrapped}\r\n"), &mut cx);
        let cell = cell_position(&terminal, &mut cx);

        start_line_highlight(&terminal_view, Some(0), &mut cx);
        keys("alt-shift-right shift-left", &mut cx);
        let by_keys = highlighted_columns(&terminal_view, &mut cx);
        assert_eq!(by_keys, Some((0..=0, Some(3), Some(5))));
        start_line_highlight(&terminal_view, Some(0), &mut cx);
        drag(cell(0.0, 0), cell(3.2, 0), &mut cx);
        drag(cell(num_columns as f32, 0), cell(7.1, 0), &mut cx);
        assert_eq!(highlighted_columns(&terminal_view, &mut cx), by_keys);

        start_line_highlight(&terminal_view, Some(1), &mut cx);
        keys("alt-shift-right", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=2, Some(num_columns - 2), None))
        );
        drag(cell(num_columns as f32 - 2., 1), cell(0.0, 2), &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=2, Some(num_columns - 2), None)),
            "a wrapped word's continuation isn't a word start"
        );
        keys("alt-shift-right", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=2, Some(num_columns - 2), None)),
            "nor for the keys"
        );
    }

    #[gpui::test]
    async fn a_partial_line_highlight_is_found_again_after_a_resize(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(
            &terminal,
            "before\r\nalpha beta gamma\r\nafter\r\n",
            &mut cx,
        );
        start_line_highlight(&terminal_view, Some(1), &mut cx);
        keys("alt-shift-right shift-left", &mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=1, Some(6), Some(9)))
        );

        narrow_window(&mut cx);
        assert_eq!(
            highlighted_columns(&terminal_view, &mut cx),
            Some((1..=1, Some(6), Some(9)))
        );
    }

    #[gpui::test]
    async fn a_gutter_drag_highlights_lines_without_reporting_them(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(
            &terminal,
            &format!("{ENABLE_MOUSE_REPORTING}{FIVE_LINES}"),
            &mut cx,
        );
        let cell = cell_position(&terminal, &mut cx);
        terminal.update(&mut cx, |terminal, _| terminal.take_pty_write_log());

        click(cell(-0.5, 2), &mut cx);
        assert!(has_line_highlight(&terminal_view, &mut cx));
        assert_eq!(highlighted_lines(&terminal_view, &mut cx), Some(2..=2));

        terminal_view.update(&mut cx, |terminal_view, cx| {
            terminal_view.exit_line_highlight(cx)
        });
        drag(cell(-0.5, 1), cell(-0.5, 3), &mut cx);
        assert_eq!(highlighted_lines(&terminal_view, &mut cx), Some(1..=3));
        assert!(terminal.read_with(&cx, |terminal, _| terminal.last_content.selection.is_none()));
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty(),
            "the program shouldn't see gutter presses, drags or releases",
        );
    }

    #[gpui::test]
    async fn the_context_menu_entry_starts_the_line_highlight_on_the_clicked_line(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(&terminal, FIVE_LINES, &mut cx);
        let cell = cell_position(&terminal, &mut cx);

        for (position, line) in [(cell(1., 3), 3), (cell(-0.5, 1), 1)] {
            cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
            cx.simulate_mouse_down(position, MouseButton::Right, gpui::Modifiers::default());
            cx.simulate_mouse_up(position, MouseButton::Right, gpui::Modifiers::default());
            cx.run_until_parked();
            assert!(terminal_view.read_with(&cx, |view, _| view.context_menu.is_some()));
            // Past the workspace actions and Copy.
            let entry_index =
                if terminal_view.read_with(&cx, |view, _| view.shows_workspace_actions()) {
                    3
                } else {
                    1
                };
            cx.dispatch_action(menu::SelectFirst);
            for _ in 0..entry_index {
                cx.dispatch_action(menu::SelectNext);
            }
            cx.dispatch_action(menu::Confirm);
            cx.run_until_parked();
            assert_eq!(
                highlighted_lines(&terminal_view, &mut cx),
                Some(line..=line)
            );
            terminal_view.update(&mut cx, |terminal_view, cx| {
                terminal_view.exit_line_highlight(cx)
            });
        }
    }

    #[gpui::test]
    async fn the_gutter_is_left_alone_without_a_selection_action(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        write_output(&terminal, FIVE_LINES, &mut cx);
        let cell = cell_position(&terminal, &mut cx);

        drag(cell(-0.5, 1), cell(3., 2), &mut cx);
        assert!(!has_line_highlight(&terminal_view, &mut cx));
        assert!(
            terminal.read_with(&cx, |terminal, _| terminal.last_content.selection.is_some()),
            "a drag from the gutter should select text"
        );
        drag(cell(1., 1), cell(-0.5, 2), &mut cx);
        assert!(
            !terminal.read_with(&cx, |terminal, _| terminal.selection_started()),
            "a drag released over the gutter should end"
        );
    }

    #[gpui::test]
    async fn a_gutter_click_on_a_marker_opens_it(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        let opened = Rc::new(RefCell::new(Vec::new()));
        terminal_view.update(&mut cx, |terminal_view, cx| {
            terminal_view.set_gutter_markers(
                |_, _| {
                    vec![GutterMarker {
                        lines: 1..=1,
                        color: gpui::red(),
                        count: 1,
                    }]
                },
                cx,
            );
            let opened = opened.clone();
            terminal_view
                .on_gutter_marker_click(move |lines, _, _, _| opened.borrow_mut().push(lines));
        });
        write_output(&terminal, FIVE_LINES, &mut cx);
        let cell = cell_position(&terminal, &mut cx);

        click(cell(-0.5, 1), &mut cx);
        assert_eq!(opened.borrow().as_slice(), &[1..=1]);
        assert!(!has_line_highlight(&terminal_view, &mut cx));

        drag(cell(-0.5, 1), cell(-0.5, 2), &mut cx);
        assert_eq!(
            opened.borrow().len(),
            1,
            "a drag from a marker shouldn't open it"
        );
        assert_eq!(highlighted_lines(&terminal_view, &mut cx), Some(1..=2));
    }

    #[gpui::test]
    async fn escape_an_unbound_key_and_focus_loss_end_the_line_highlight(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(&terminal, FIVE_LINES, &mut cx);

        start_line_highlight(&terminal_view, None, &mut cx);
        keys("escape", &mut cx);
        assert!(!has_line_highlight(&terminal_view, &mut cx));

        start_line_highlight(&terminal_view, None, &mut cx);
        terminal.update(&mut cx, |terminal, _| terminal.take_input_log());
        keys("x", &mut cx);
        assert!(!has_line_highlight(&terminal_view, &mut cx));
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![b"x".to_vec()],
            "an unbound key should go to the program"
        );

        start_line_highlight(&terminal_view, None, &mut cx);
        cx.update(|window, cx| window.blur(cx));
        cx.run_until_parked();
        assert!(!has_line_highlight(&terminal_view, &mut cx));
    }

    /// The text of grid `lines`, without whitespace.
    fn lines_text(
        terminal: &Entity<Terminal>,
        lines: RangeInclusive<i32>,
        cx: &mut VisualTestContext,
    ) -> String {
        terminal.read_with(cx, |terminal, _| {
            terminal
                .grid_rows(lines)
                .iter()
                .flat_map(|row| row.characters.iter())
                .filter(|character| !character.is_whitespace())
                .collect()
        })
    }

    fn narrow_window(cx: &mut VisualTestContext) {
        let bounds = cx.update(|window, _| window.bounds());
        cx.simulate_resize(gpui::size(bounds.size.width / 3., bounds.size.height));
        cx.run_until_parked();
        cx.executor().advance_clock(LINE_HIGHLIGHT_REFIND_DELAY);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_line_highlight_is_found_again_after_a_resize(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        let long_line = "word ".repeat(30);
        write_output(
            &terminal,
            &format!("before\r\n{long_line}\r\nafter\r\n"),
            &mut cx,
        );
        start_line_highlight(&terminal_view, Some(1), &mut cx);

        narrow_window(&mut cx);
        assert!(has_line_highlight(&terminal_view, &mut cx));
        let lines = highlighted_lines(&terminal_view, &mut cx).expect("the highlight should stay");
        assert!(lines.end() > lines.start(), "the long line should wrap");
        assert_eq!(
            lines_text(&terminal, lines, &mut cx),
            long_line.replace(' ', ""),
        );
    }

    #[gpui::test]
    async fn the_line_highlight_falls_back_to_the_last_line_when_its_text_is_gone(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(&terminal, FIVE_LINES, &mut cx);
        start_line_highlight(&terminal_view, Some(1), &mut cx);

        write_output(&terminal, "\x1b[H\x1b[2J\x1b[3Jlast\r\n", &mut cx);
        narrow_window(&mut cx);
        assert!(has_line_highlight(&terminal_view, &mut cx));
        let lines = highlighted_lines(&terminal_view, &mut cx).expect("the highlight should stay");
        assert_eq!(lines_text(&terminal, lines, &mut cx), "last");
    }

    #[gpui::test]
    async fn line_highlight_keeps_scroll_reports(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        write_output(
            &terminal,
            &format!("{ENABLE_MOUSE_REPORTING}hello world\n"),
            &mut cx,
        );
        let cell = cell_position(&terminal, &mut cx);
        start_line_highlight(&terminal_view, None, &mut cx);
        terminal.update(&mut cx, |terminal, _| terminal.take_pty_write_log());

        cx.simulate_event(ScrollEvent {
            position: cell(2.0, 0),
            delta: gpui::ScrollDelta::Lines(gpui::point(0.0, 1.0)),
            ..Default::default()
        });
        cx.run_until_parked();

        assert!(
            !terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty(),
            "scrolling should still be reported to the program",
        );
    }

    #[gpui::test]
    async fn shift_up_scrolls_history_in_normal_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let output = (0..200)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(output.as_bytes(), cx);
                terminal.sync(window, cx);
            });
        });
        terminal.read_with(&cx, |terminal, _| {
            assert!(!terminal.last_content.mode.contains(Modes::ALT_SCREEN));
            assert_eq!(terminal.last_content.display_offset, 0);
        });

        cx.simulate_keystrokes("shift-up");
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        });

        assert_eq!(
            terminal.read_with(&cx, |terminal, _| terminal.last_content.display_offset),
            1,
            "shift-up should scroll terminal history in the normal screen",
        );
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_input_log())
                .is_empty(),
            "shift-up in the normal screen should not be forwarded to the shell",
        );
    }

    #[gpui::test]
    async fn shift_up_is_forwarded_to_program_in_alt_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(ENTER_ALT_SCREEN, cx);
                terminal.sync(window, cx);
            });
        });
        terminal.read_with(&cx, |terminal, _| {
            assert!(terminal.last_content.mode.contains(Modes::ALT_SCREEN));
        });

        cx.simulate_keystrokes("shift-up");
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![SHIFT_UP_ESCAPE.to_vec()],
            "shift-up should be forwarded to the program in the alternate screen",
        );
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    async fn ctrl_q_is_forwarded_to_terminal_not_quit(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("ctrl-q");
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![vec![0x11]],
            "ctrl-q in a focused terminal should send 0x11 to the PTY, not trigger zed::Quit",
        );
    }

    #[gpui::test]
    async fn altgr_character_input_is_not_swallowed_as_meta_sequence(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().option_as_meta = Some(true);
                });
            });
        });
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let mut altgr_a = Keystroke::parse("ctrl-alt-a").unwrap();
        altgr_a.key_char = Some("ą".to_string());

        let propagate = cx.update(|window, cx| {
            window
                .dispatch_event(
                    gpui::PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: altgr_a.clone(),
                        is_held: false,
                        prefer_character_input: true,
                    }),
                    cx,
                )
                .propagate
        });
        assert!(
            propagate,
            "a keystroke that produced a character must propagate so the platform can commit the text",
        );
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            Vec::<Vec<u8>>::new(),
            "AltGr-produced characters must not be sent as meta escape sequences",
        );

        let propagate = cx.update(|window, cx| {
            window
                .dispatch_event(
                    gpui::PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: altgr_a,
                        is_held: false,
                        prefer_character_input: false,
                    }),
                    cx,
                )
                .propagate
        });
        assert!(!propagate);
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![b"\x1b\x01".to_vec()],
            "ctrl-alt-a without character input preference is still sent as meta ctrl-a",
        );
    }

    // Working directory calculation tests

    // No Worktrees in project -> home_dir()
    #[gpui::test]
    async fn no_worktree(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure environment is as expected
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_none());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, dirs::home_dir());
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, None);
        });
    }

    #[gpui::test]
    async fn remote_no_worktree_uses_remote_shell_default_cwd(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let (_project, workspace) = init_remote_test(cx, server_cx).await;

        cx.read(|cx| {
            let workspace = workspace.read(cx);

            assert!(workspace.project().read(cx).is_remote());
            assert!(workspace.worktrees(cx).next().is_none());
            assert_eq!(default_working_directory(workspace, cx), None);
        });
    }

    // No active entry, but a worktree, worktree is a file -> parent directory
    #[gpui::test]
    async fn no_active_entry_worktree_is_file(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        create_file_wt(project.clone(), "/root.txt", cx).await;
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure environment is as expected
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/").to_path_buf()));
        });
    }

    // No active entry, but a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn no_active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
        });
    }

    // Active entry with a work tree, worktree is a file -> worktree_folder()
    #[gpui::test]
    async fn active_entry_worktree_is_file(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root1/", cx).await;
        let (wt2, entry2) = create_file_wt(project.clone(), "/root2.txt", cx).await;
        insert_active_entry_for(wt2, entry2, project.clone(), cx);

        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
        });
    }

    // Active entry, with a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root1/", cx).await;
        let (wt2, entry2) = create_folder_wt(project.clone(), "/root2/", cx).await;
        insert_active_entry_for(wt2, entry2, project.clone(), cx);

        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root2/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
        });
    }

    // active_entry_directory: No active entry -> returns None (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_no_active_entry(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;

        cx.update(|cx| {
            assert!(project.read(cx).active_entry().is_none());

            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, None);
        });
    }

    // active_entry_directory: Active entry is file -> returns parent directory (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_active_file(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        let entry = create_file_in_worktree(wt.clone(), "src/main.rs", cx).await;
        insert_active_entry_for(wt, entry, project.clone(), cx);

        cx.update(|cx| {
            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, Some(Path::new("/root/src").to_path_buf()));
        });
    }

    // active_entry_directory: Active entry is directory -> returns that directory (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_active_dir(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (wt, entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        insert_active_entry_for(wt, entry, project.clone(), cx);

        cx.update(|cx| {
            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
        });
    }

    /// Creates a worktree with 1 file: /root.txt
    pub async fn init_test(cx: &mut TestAppContext) -> (Entity<Project>, Entity<Workspace>) {
        let (project, workspace, _) = init_test_with_window(cx).await;
        (project, workspace)
    }

    fn load_default_keymap(cx: &mut App) {
        cx.bind_keys(
            settings::KeymapFile::load_asset_allow_partial_failure(
                settings::DEFAULT_KEYMAP_PATH,
                cx,
            )
            .unwrap(),
        );
    }

    pub(super) fn add_display_only_terminal(
        project: &Entity<Project>,
        window_handle: gpui::WindowHandle<MultiWorkspace>,
        focus: bool,
        read_only: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Pane>, Entity<Terminal>, Entity<TerminalView>) {
        let project = project.clone();
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let active_pane = workspace.read(cx).active_pane().clone();

                let terminal = cx.new(|cx| {
                    terminal::TerminalBuilder::new_display_only(
                        CursorShape::default(),
                        terminal::terminal_settings::AlternateScroll::On,
                        None,
                        0,
                        cx.background_executor(),
                        PathStyle::local(),
                    )
                    .subscribe(cx)
                });
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                    .with_read_only(read_only)
                });

                active_pane.update(cx, |pane, cx| {
                    pane.add_item(
                        Box::new(terminal_view.clone()),
                        true,
                        false,
                        None,
                        window,
                        cx,
                    );
                });

                if focus {
                    let focus_handle = terminal_view.read(cx).focus_handle.clone();
                    focus_handle.focus(window, cx);
                }

                (active_pane, terminal, terminal_view)
            })
            .unwrap()
    }

    /// Creates a worktree with 1 file /root.txt and returns the project, workspace, and window handle.
    pub(super) async fn init_test_with_window(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<Workspace>,
        gpui::WindowHandle<MultiWorkspace>,
    ) {
        let params = cx.update(AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let project = Project::test(params.fs.clone(), [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        (project, workspace, window_handle)
    }

    async fn init_remote_test(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Workspace>) {
        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let params = cx.update(AppState::test);
        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
        let ping_handler = server_cx.new(|_| ());
        server_session.add_request_handler::<rpc::proto::Ping, _, _, _>(
            ping_handler.downgrade(),
            |_entity, _envelope, _cx| async { Ok(rpc::proto::Ack {}) },
        );
        drop(connect_guard);

        let remote_client = RemoteClient::connect_mock(opts, cx).await;
        let project = cx.update(|cx| {
            Project::remote(
                remote_client,
                params.client.clone(),
                params.node_runtime.clone(),
                params.user_store.clone(),
                params.languages.clone(),
                params.fs.clone(),
                false,
                cx,
            )
        });

        let window_handle = cx.add_window({
            let params = params.clone();
            let project_for_workspace = project.clone();
            move |window, cx| {
                window.activate_window();
                let workspace = cx.new(|cx| {
                    Workspace::new(
                        None,
                        project_for_workspace.clone(),
                        params.clone(),
                        window,
                        cx,
                    )
                });
                MultiWorkspace::new(workspace, window, cx)
            }
        });
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        (project, workspace)
    }

    /// Creates a file in the given worktree and returns its entry.
    async fn create_file_in_worktree(
        worktree: Entity<Worktree>,
        relative_path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> Entry {
        cx.update(|cx| {
            worktree.update(cx, |worktree, cx| {
                worktree.create_entry(
                    RelPath::new(relative_path.as_ref(), PathStyle::local())
                        .unwrap()
                        .as_ref()
                        .into(),
                    false,
                    None,
                    cx,
                )
            })
        })
        .await
        .unwrap()
        .into_included()
        .unwrap()
    }

    /// Creates a worktree with 1 folder: /root{suffix}/
    async fn create_folder_wt(
        project: Entity<Project>,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        create_wt(project, true, path, cx).await
    }

    /// Creates a worktree with 1 file: /root{suffix}.txt
    async fn create_file_wt(
        project: Entity<Project>,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        create_wt(project, false, path, cx).await
    }

    async fn create_wt(
        project: Entity<Project>,
        is_dir: bool,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        let (wt, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path, true, cx)
            })
            .await
            .unwrap();

        let entry = cx
            .update(|cx| {
                wt.update(cx, |wt, cx| {
                    wt.create_entry(RelPath::empty_arc(), is_dir, None, cx)
                })
            })
            .await
            .unwrap()
            .into_included()
            .unwrap();

        (wt, entry)
    }

    pub fn insert_active_entry_for(
        wt: Entity<Worktree>,
        entry: Entry,
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let p = ProjectPath {
                worktree_id: wt.read(cx).id(),
                path: entry.path,
            };
            project.update(cx, |project, cx| project.set_active_path(Some(p), cx));
        });
    }

    // Terminal drag/drop test

    #[gpui::test]
    async fn test_handle_drop_writes_paths_for_all_drop_types(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;

        let (worktree, _) = create_folder_wt(project.clone(), "/root/", cx).await;
        let first_entry = create_file_in_worktree(worktree.clone(), "first.txt", cx).await;
        let second_entry = create_file_in_worktree(worktree.clone(), "second.txt", cx).await;

        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        let first_path = project
            .read_with(cx, |project, cx| {
                project.absolute_path(
                    &ProjectPath {
                        worktree_id,
                        path: first_entry.path.clone(),
                    },
                    cx,
                )
            })
            .unwrap();
        let second_path = project
            .read_with(cx, |project, cx| {
                project.absolute_path(
                    &ProjectPath {
                        worktree_id,
                        path: second_entry.path.clone(),
                    },
                    cx,
                )
            })
            .unwrap();

        let (active_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, false, cx);

        let tab_item = window_handle
            .update(cx, |_, window, cx| {
                let tab_project_item = cx.new(|_| TestProjectItem {
                    entry_id: Some(second_entry.id),
                    project_path: Some(ProjectPath {
                        worktree_id,
                        path: second_entry.path.clone(),
                    }),
                    is_dirty: false,
                });
                let tab_item =
                    cx.new(|cx| TestItem::new(cx).with_project_items(&[tab_project_item]));
                active_pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(tab_item.clone()), true, false, None, window, cx);
                });
                tab_item
            })
            .unwrap();

        cx.run_until_parked();

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let terminal_view_index =
                    active_pane.read(cx).index_for_item(&terminal_view).unwrap();
                let dragged_tab_index = active_pane.read(cx).index_for_item(&tab_item).unwrap();

                assert!(
                    workspace.read(cx).pane_for(&terminal_view).is_some(),
                    "terminal view not registered with workspace after run_until_parked"
                );

                // Dragging an external file should write its path to the terminal
                let external_paths = ExternalPaths(vec![first_path.clone()].into());
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &external_paths,
                    &expected_drop_text(std::slice::from_ref(&first_path)),
                    window,
                    cx,
                );

                // Dragging a tab should write the path of the tab's item to the terminal
                let dragged_tab = DraggedTab {
                    pane: active_pane.clone(),
                    item: Box::new(tab_item.clone()),
                    ix: dragged_tab_index,
                    detail: 0,
                    is_active: false,
                };
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dragged_tab,
                    &expected_drop_text(std::slice::from_ref(&second_path)),
                    window,
                    cx,
                );

                // Dragging multiple selections should write both paths to the terminal
                let dragged_selection = DraggedSelection {
                    active_selection: SelectedEntry {
                        worktree_id,
                        entry_id: first_entry.id,
                    },
                    marked_selections: Arc::from([
                        SelectedEntry {
                            worktree_id,
                            entry_id: first_entry.id,
                        },
                        SelectedEntry {
                            worktree_id,
                            entry_id: second_entry.id,
                        },
                    ]),
                };
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dragged_selection,
                    &expected_drop_text(&[first_path.clone(), second_path.clone()]),
                    window,
                    cx,
                );

                // Dropping a project entry should write the entry's path to the terminal
                let dropped_entry_id = first_entry.id;
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dropped_entry_id,
                    &expected_drop_text(&[first_path]),
                    window,
                    cx,
                );
            })
            .unwrap();
    }

    // Terminal rename tests

    #[gpui::test]
    async fn test_custom_title_initially_none(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, _cx| {
            assert!(view.custom_title().is_none());
        });
    }

    #[gpui::test]
    async fn test_set_custom_title(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("frontend".to_string()), cx);
            assert_eq!(view.custom_title(), Some("frontend"));
        });
    }

    #[gpui::test]
    async fn test_set_custom_title_empty_becomes_none(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("test".to_string()), cx);
            assert_eq!(view.custom_title(), Some("test"));

            view.set_custom_title(Some("".to_string()), cx);
            assert!(view.custom_title().is_none());

            view.set_custom_title(Some("  ".to_string()), cx);
            assert!(view.custom_title().is_none());
        });
    }

    #[gpui::test]
    async fn test_custom_title_marks_needs_serialize(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.needs_serialize = false;
            view.set_custom_title(Some("new_label".to_string()), cx);
            assert!(view.needs_serialize);
        });
    }

    #[gpui::test]
    async fn test_tab_content_uses_custom_title(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("my-server".to_string()), cx);
            let text = view.tab_content_text(0, cx);
            assert_eq!(text.as_ref(), "my-server");
        });

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(None, cx);
            let text = view.tab_content_text(0, cx);
            assert_ne!(text.as_ref(), "my-server");
        });
    }

    async fn draw_standalone_terminal(
        output: &[u8],
        cx: &mut TestAppContext,
    ) -> (gpui::Bounds<Pixels>, gpui::Size<Pixels>) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(output, cx);
        });

        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            )
        });

        let draw_size = gpui::size(px(400.), px(201.));
        cx.simulate_resize(draw_size);
        cx.draw(gpui::Point::default(), draw_size, |_, _| {
            terminal_view.clone().into_any_element()
        });
        cx.run_until_parked();
        cx.draw(gpui::Point::default(), draw_size, |_, _| {
            terminal_view.clone().into_any_element()
        });

        let bounds = terminal.read_with(cx, |terminal, _| {
            terminal.last_content().terminal_bounds.bounds
        });
        (bounds, draw_size)
    }

    #[gpui::test]
    async fn test_short_standalone_terminal_stays_top_anchored_on_resize(cx: &mut TestAppContext) {
        let (bounds, _) = draw_standalone_terminal(b"$ ", cx).await;
        assert_eq!(bounds.origin.y, px(0.));
    }

    #[gpui::test]
    async fn test_full_standalone_terminal_stays_bottom_anchored_on_resize(
        cx: &mut TestAppContext,
    ) {
        let (bounds, draw_size) = draw_standalone_terminal(
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            cx,
        )
        .await;
        assert!(bounds.origin.y > px(0.));
        assert_eq!(bounds.bottom(), draw_size.height);
    }

    #[gpui::test]
    async fn test_short_alt_screen_stays_bottom_anchored_on_resize(cx: &mut TestAppContext) {
        let (bounds, draw_size) = draw_standalone_terminal(b"\x1b[?1049h$ ", cx).await;
        assert!(bounds.origin.y > px(0.));
        assert_eq!(bounds.bottom(), draw_size.height);
    }

    #[gpui::test]
    async fn test_inline_terminal_displays_all_of_its_lines(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            let mut terminal_view = TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            );
            terminal_view.set_embedded_mode(None, cx);
            terminal_view
        });

        for _ in 1..=20 {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"line\n", cx);
            });
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
            terminal.read_with(cx, |terminal, _| {
                assert_eq!(terminal.viewport_lines(), terminal.total_lines());
            })
        }
    }

    #[gpui::test]
    async fn test_inline_terminal_shrinks_after_clear(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            let mut terminal_view = TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            );
            terminal_view.set_embedded_mode(None, cx);
            terminal_view
        });

        for _ in 1..=20 {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"line\n", cx);
            });
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
        }
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.total_lines(), 21);
        });

        terminal.update(cx, |terminal, _| terminal.clear());
        for _ in 1..=2 {
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
        }
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.total_lines(), 1);
            assert_eq!(terminal.viewport_lines(), 1);
        });
    }

    #[gpui::test]
    async fn test_tab_content_shows_terminal_title_when_custom_title_directly_set_empty(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.custom_title = Some("".to_string());
            let text = view.tab_content_text(0, cx);
            assert!(
                !text.is_empty(),
                "Tab should show terminal title, not empty string; got: '{}'",
                text
            );
        });

        terminal_view.update(cx, |view, cx| {
            view.custom_title = Some("   ".to_string());
            let text = view.tab_content_text(0, cx);
            assert!(
                !text.is_empty() && text.as_ref() != "   ",
                "Tab should show terminal title, not whitespace; got: '{}'",
                text
            );
        });
    }
}
