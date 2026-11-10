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
    Clear, Content, Copy, Event, GridRow, HoveredWord, MaybeNavigationTarget, Modes,
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
use unicode_segmentation::UnicodeSegmentation;
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
    Rc<dyn Fn(Range, String, Option<PickedTextKind>, GpuiPoint<Pixels>, &mut Window, &mut App)>;

/// What a pick that runs the selection action took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickedTextKind {
    pub level: TextBlockLevel,
    /// Whether texts picked to act on together were joined.
    pub combined: bool,
}

#[derive(Clone)]
pub(crate) struct SelectionAction {
    pub(crate) label: SharedString,
    /// Receives the level of a picked text block, or none for a selection.
    pub(crate) handler: SelectionActionHandler,
}

/// A block of text in the grid that can be picked for the selection action.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBlock {
    /// Grid lines, top to bottom, each with the columns its text spans.
    pub rows: Vec<(i32, RangeInclusive<usize>)>,
    /// The text the block's action receives, which may reach past `rows`.
    pub text: String,
    /// Replaces the selection action's label in the pick's hint, for
    /// example when the action on this block differs.
    pub label: Option<SharedString>,
    /// Runs of grid lines that are one paragraph each, whatever their rows
    /// look like, such as a question with its options.
    pub paragraphs: Vec<RangeInclusive<i32>>,
}

impl TextBlock {
    fn lines(&self) -> Option<RangeInclusive<i32>> {
        Some(self.rows.first()?.0..=self.rows.last()?.0)
    }

    pub fn range(&self) -> Option<Range> {
        let (first_line, first_columns) = self.rows.first()?;
        let (last_line, last_columns) = self.rows.last()?;
        Some(Range::new(
            Point::new(*first_line, *first_columns.start()),
            Point::new(*last_line, *last_columns.end()),
        ))
    }
}

fn unit_start(unit: &TextBlock) -> Point {
    unit.rows
        .first()
        .map_or(Point::new(0, 0), |(line, columns)| {
            Point::new(*line, *columns.start())
        })
}

fn unit_contains(unit: &TextBlock, point: Point) -> bool {
    unit.rows
        .iter()
        .any(|(line, columns)| *line == point.line && columns.contains(&point.column))
}

/// Whether text starts with a list marker followed by a space: a bullet,
/// an agent's message or answer marker, or a number or letter followed by
/// `.` or `)`.
pub fn starts_with_list_marker(text: &[char]) -> bool {
    const BULLETS: &[char] = &['-', '*', '•', '◦', '▪', '●', '⏺', '⎿', '>', '❯'];
    let followed_by_space =
        |index: usize| text.get(index).is_some_and(|character| *character == ' ');
    let Some(first) = text.first() else {
        return false;
    };
    if BULLETS.contains(first) {
        return followed_by_space(1);
    }
    let digits = text
        .iter()
        .take_while(|character| character.is_ascii_digit())
        .count();
    let label_length = if (1..=3).contains(&digits) {
        digits
    } else if first.is_ascii_alphabetic() {
        1
    } else {
        return false;
    };
    text.get(label_length)
        .is_some_and(|character| *character == '.' || *character == ')')
        && followed_by_space(label_length + 1)
}

/// The columns and characters of a row's text, without whitespace and the
/// cells that pad wide characters.
fn row_text_columns(row: &GridRow, columns: &RangeInclusive<usize>) -> Vec<(usize, char)> {
    columns
        .clone()
        .filter_map(|column| Some((column, *row.characters.get(column)?)))
        .filter(|(column, character)| {
            !character.is_whitespace() && !row.wide_char_spacers.contains(column)
        })
        .collect()
}

/// The column a paragraph's rows line up at: its first text, or the text
/// after the list marker it starts with.
fn paragraph_text_indent(row: &GridRow, columns: &RangeInclusive<usize>) -> Option<usize> {
    let text = row_text_columns(row, columns);
    let (first_column, _) = text.first()?;
    let characters = row.characters.get(*first_column..=*columns.end())?.to_vec();
    if !starts_with_list_marker(&characters) {
        return Some(*first_column);
    }
    // The marker is the first word, and the text after it starts at the
    // next one.
    let after_marker = text
        .windows(2)
        .find(|pair| pair[1].0 != pair[0].0 + 1)
        .map(|pair| pair[1].0);
    Some(after_marker.unwrap_or(*first_column))
}

/// The rows of each paragraph in a block's `rows`. A paragraph runs over
/// rows the terminal wrapped, the lines of the block's `paragraphs`, and
/// rows that look like the program wrapped prose (see
/// [`continues_paragraph`]). Any other line break ends a paragraph, since
/// terminal output is mostly lines that stand on their own.
fn paragraph_rows(
    block: &TextBlock,
    grid_rows: &[GridRow],
) -> Vec<Vec<(i32, RangeInclusive<usize>)>> {
    let mut paragraphs: Vec<Vec<(i32, RangeInclusive<usize>)>> = Vec::new();
    let mut previous_row: Option<(&GridRow, &RangeInclusive<usize>)> = None;
    let mut text_indent = None;
    let group_of = |line: i32| {
        block
            .paragraphs
            .iter()
            .position(|group| group.contains(&line))
    };
    for (line, columns) in &block.rows {
        let Some(grid_row) = grid_rows.iter().find(|row| row.line == *line) else {
            continue;
        };
        let continues = previous_row.is_some_and(|(previous, previous_columns)| {
            if previous.line.saturating_add(1) != *line {
                return false;
            }
            match (group_of(previous.line), group_of(*line)) {
                (Some(previous_group), Some(group)) => previous_group == group,
                (None, None) => {
                    previous.wraps
                        || text_indent.is_some_and(|text_indent| {
                            continues_paragraph(
                                previous,
                                previous_columns,
                                grid_row,
                                columns,
                                text_indent,
                            )
                        })
                }
                _ => false,
            }
        });
        match paragraphs.last_mut() {
            Some(paragraph) if continues => paragraph.push((*line, columns.clone())),
            _ => {
                paragraphs.push(vec![(*line, columns.clone())]);
                text_indent = paragraph_text_indent(grid_row, columns);
            }
        }
        previous_row = Some((grid_row, columns));
    }
    paragraphs
}

/// Whether a line break looks like a program wrapping prose to fit the
/// terminal, as agent TUIs do, rather than ending a line of a log or of
/// code. The terminal can't tell them apart, so this guesses: the row
/// reaches well across the terminal and ends in a word or punctuation of
/// prose, and the next row carries on at the paragraph's indent with a
/// word. Rows of code, listings and logs rarely look like that.
fn continues_paragraph(
    row: &GridRow,
    columns: &RangeInclusive<usize>,
    next_row: &GridRow,
    next_columns: &RangeInclusive<usize>,
    text_indent: usize,
) -> bool {
    let text = row_text_columns(row, columns);
    let next_text = row_text_columns(next_row, next_columns);
    let Some((last_column, _)) = text.last() else {
        return false;
    };
    // A Markdown heading stands on its own line.
    if text.first().is_some_and(|(_, character)| *character == '#') {
        return false;
    }
    let Some((next_first_column, _)) = next_text.first() else {
        return false;
    };
    // Closing quotes and brackets after the last word don't matter.
    let ends_in_prose = text
        .iter()
        .rev()
        .map(|(_, character)| *character)
        .find(|character| !matches!(character, ')' | ']' | '"' | '\'' | '”' | '’' | '`'))
        .is_some_and(|character| {
            character.is_alphabetic() || matches!(character, ',' | '.' | '?' | '!' | ':')
        });
    let word_length = next_text
        .windows(2)
        .position(|pair| pair[1].0 != pair[0].0 + 1)
        .map_or(next_text.len(), |index| index + 1);
    let next_characters = next_text[..word_length]
        .iter()
        .map(|(_, character)| *character)
        .collect::<Vec<_>>();
    let next_word = next_characters.iter().collect::<String>();
    // A label such as "warning:" starts a line of its own.
    if next_word.ends_with(':') {
        return false;
    }
    let next_word = next_word
        .trim_start_matches(['"', '“', '(', '`'])
        .trim_end_matches([',', '.', ';', ':', '!', '?', '"', '”', ')', '`']);
    let width = row.characters.len();
    ends_in_prose
        && last_column.saturating_mul(3) >= width.saturating_mul(2)
        && *next_first_column == text_indent
        && !next_row
            .characters
            .get(*next_first_column..)
            .is_some_and(starts_with_list_marker)
        && next_word.starts_with(char::is_alphabetic)
        // Apostrophes, as in "others'" or "don't", and dots, as in
        // "Cargo.toml", but not the hyphens of listings such as
        // "drwxr-xr-x", nor the digits of log lines.
        && next_word
            .chars()
            .all(|character| character.is_alphabetic() || matches!(character, '\'' | '’' | '.'))
}

/// The rows of each sentence of a paragraph, by Unicode sentence
/// boundaries.
fn sentence_rows(
    paragraph_rows: &[(i32, RangeInclusive<usize>)],
    grid_rows: &[GridRow],
) -> Vec<Vec<(i32, RangeInclusive<usize>)>> {
    let mut paragraph = String::new();
    let mut byte_points = Vec::new();
    let mut previous_wraps = true;
    for (line, columns) in paragraph_rows {
        let Some(grid_row) = grid_rows.iter().find(|row| row.line == *line) else {
            continue;
        };
        if !previous_wraps
            && !paragraph.ends_with(char::is_whitespace)
            && let Some(last_point) = byte_points.last().copied()
        {
            // The program's line break stood for a space.
            paragraph.push(' ');
            byte_points.push(last_point);
        }
        for column in columns.clone() {
            if grid_row.wide_char_spacers.contains(&column) {
                continue;
            }
            let Some(character) = grid_row.characters.get(column) else {
                break;
            };
            paragraph.push(*character);
            byte_points.extend(std::iter::repeat_n(
                Point::new(*line, column),
                character.len_utf8(),
            ));
        }
        previous_wraps = grid_row.wraps;
    }
    let mut sentences = Vec::new();
    push_paragraph_sentences(&paragraph, &byte_points, &mut sentences);
    sentences
}

/// Abbreviations that a sentence doesn't end at, though Unicode sentence
/// boundaries break after them before a capital.
const ABBREVIATIONS: &[&str] = &[
    "e.g.", "i.e.", "vs.", "cf.", "dr.", "mr.", "mrs.", "approx.",
];

fn ends_with_abbreviation(text: &str) -> bool {
    text.split_whitespace().last().is_some_and(|word| {
        let word = word.trim_start_matches(['(', '"', '“']).to_lowercase();
        ABBREVIATIONS.contains(&word.as_str())
    })
}

fn push_paragraph_sentences(
    paragraph: &str,
    byte_points: &[Point],
    sentences: &mut Vec<Vec<(i32, RangeInclusive<usize>)>>,
) {
    let mut byte_ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for (start, sentence) in paragraph.split_sentence_bound_indices() {
        let trimmed_start = sentence.len() - sentence.trim_start().len();
        let trimmed = sentence.trim();
        if trimmed.is_empty() {
            continue;
        }
        let start = start + trimmed_start;
        let end = start + trimmed.len();
        match byte_ranges.last_mut() {
            Some(previous) if ends_with_abbreviation(&paragraph[previous.clone()]) => {
                previous.end = end;
            }
            _ => byte_ranges.push(start..end),
        }
    }
    for byte_range in byte_ranges {
        let Some(points) = byte_points.get(byte_range) else {
            continue;
        };
        let mut rows: Vec<(i32, RangeInclusive<usize>)> = Vec::new();
        for point in points {
            match rows.last_mut() {
                Some((line, columns)) if *line == point.line => {
                    *columns = *columns.start()..=point.column.max(*columns.end());
                }
                _ => rows.push((point.line, point.column..=point.column)),
            }
        }
        sentences.push(rows);
    }
}

pub(crate) type TextBlockProvider = Rc<dyn Fn(&Terminal, Point, &App) -> Option<TextBlock>>;

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

/// How much of a text block a pick takes: the whole block, which is a
/// message, or a paragraph or a sentence of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextBlockLevel {
    #[default]
    Message,
    Paragraph,
    Sentence,
}

impl TextBlockLevel {
    const ALL: [Self; 3] = [Self::Message, Self::Paragraph, Self::Sentence];

    fn name(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Paragraph => "paragraph",
            Self::Sentence => "sentence",
        }
    }
}

/// The text a pick points at.
pub(crate) struct PickedTextBlock {
    /// The picked text: the block, or a paragraph or a sentence of it.
    pub(crate) block: TextBlock,
    /// The block a picked paragraph or sentence is in.
    pub(crate) parent: Option<TextBlock>,
    /// The other texts picked to act on together.
    pub(crate) collected: Vec<TextBlock>,
    /// Where the pick points on the block, which the action's popover
    /// opens next to.
    pub(crate) anchor: Point,
}

/// Picking a text block for the selection action, which the host starts on
/// request. Clicks and moves stay local while picking.
struct TextBlockPick {
    /// The picked point as a scrollback row and column, which output
    /// scrolling the screen doesn't change until the scrollback is full.
    position: Option<(i64, usize)>,
    level: TextBlockLevel,
    /// Where the other texts picked to act on together start, as scrollback
    /// rows and columns.
    collected: Vec<(i64, usize)>,
}

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
    font_scale: Option<f32>,
    blinking_terminal_enabled: bool,
    needs_serialize: bool,
    custom_title: Option<String>,
    hover: Option<HoverTarget>,
    hover_tooltip_update: Task<()>,
    workspace_id: Option<WorkspaceId>,
    show_breadcrumbs: bool,
    block_below_cursor: Option<Rc<BlockProperties>>,
    selection_action: Option<SelectionAction>,
    gutter_markers: Option<GutterMarkers>,
    pub(crate) gutter_marker_click: Option<GutterMarkerClick>,
    text_block_provider: Option<TextBlockProvider>,
    text_block_suspended: bool,
    /// The viewport row and column under the pointer, while it's over the
    /// terminal.
    pointer_cell: Option<(usize, usize)>,
    text_block_pick: Option<TextBlockPick>,
    pub(crate) picked_text_block: Option<PickedTextBlock>,
    /// Set when a click picks a block, so its release isn't reported to a
    /// program that never saw the press.
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
            font_scale: None,
            workspace_id,
            show_breadcrumbs: TerminalSettings::get_global(cx).toolbar.breadcrumbs,
            block_below_cursor: None,
            selection_action: None,
            gutter_markers: None,
            gutter_marker_click: None,
            text_block_provider: None,
            text_block_suspended: false,
            pointer_cell: None,
            text_block_pick: None,
            picked_text_block: None,
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
        if self.read_only || self.takes_mouse_for_text_block_pick() {
            MouseInputMode::LocalSelection
        } else {
            MouseInputMode::ReportToTerminal
        }
    }

    /// Scales the terminal's font size, for hosts that show the terminal as a
    /// compact preview. `None` uses the configured font size.
    pub fn set_font_scale(&mut self, font_scale: Option<f32>, cx: &mut Context<Self>) {
        if self.font_scale != font_scale {
            self.font_scale = font_scale;
            cx.notify();
        }
    }

    pub fn font_scale(&self) -> Option<f32> {
        self.font_scale
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

    /// Sets the action that a picked text block runs: `handler` gets the
    /// block's range, its text, the pick's level, which is none for a
    /// selection, and the window position to open at.
    pub fn on_selection_action(
        &mut self,
        label: impl Into<SharedString>,
        handler: impl Fn(
            Range,
            String,
            Option<PickedTextKind>,
            GpuiPoint<Pixels>,
            &mut Window,
            &mut App,
        ) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.selection_action = Some(SelectionAction {
            label: label.into(),
            handler: Rc::new(handler),
        });
        cx.notify();
    }

    pub fn clear_selection_action(&mut self, cx: &mut Context<Self>) {
        self.selection_action = None;
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

    /// Finds the text block at a grid point, which a pick highlights and
    /// runs the selection action on.
    pub fn set_text_block_provider(
        &mut self,
        provider: impl Fn(&Terminal, Point, &App) -> Option<TextBlock> + 'static,
        cx: &mut Context<Self>,
    ) {
        self.text_block_provider = Some(Rc::new(provider));
        cx.notify();
    }

    pub fn clear_text_block_provider(&mut self, cx: &mut Context<Self>) {
        self.text_block_provider = None;
        self.cancel_text_block_pick(cx);
    }

    /// Hides the pick and gives the mouse back to the program without ending
    /// the pick, for example while the action started from it is in
    /// progress.
    pub fn set_text_block_suspended(&mut self, suspended: bool, cx: &mut Context<Self>) {
        self.text_block_suspended = suspended;
        cx.notify();
    }

    pub fn is_picking_text_block(&self) -> bool {
        self.text_block_pick.is_some()
    }

    pub(crate) fn takes_mouse_for_text_block_pick(&self) -> bool {
        self.text_block_pick.is_some() && !self.text_block_suspended
    }

    /// Starts picking a text block, at the vi cursor in vi mode, else under
    /// the pointer, else the last block on screen. Typing, losing focus or
    /// [`Self::cancel_text_block_pick`] ends the pick.
    pub fn start_text_block_pick(&mut self, cx: &mut Context<Self>) {
        if self.text_block_provider.is_none() {
            return;
        }
        let terminal = self.terminal.read(cx);
        let point = if terminal.vi_mode_enabled() {
            Some(terminal.last_content().cursor.point)
        } else {
            self.pointer_point(cx).or_else(|| {
                // The last paragraph on screen, which is the most recent
                // output.
                let block_point = self.next_text_block_point(self.visible_lines(cx).rev(), cx)?;
                let block = self.text_block_at(block_point, cx)?;
                let last = self
                    .block_units(&block, TextBlockLevel::Paragraph, cx)
                    .pop()?;
                Some(unit_start(&last))
            })
        };
        let position = point.and_then(|point| self.scrollback_position(point, cx));
        self.text_block_pick = Some(TextBlockPick {
            position,
            level: TextBlockLevel::Paragraph,
            collected: Vec::new(),
        });
        cx.notify();
    }

    pub fn text_block_pick_level(&self) -> Option<TextBlockLevel> {
        Some(self.text_block_pick.as_ref()?.level)
    }

    fn pick_point(&self, cx: &App) -> Option<Point> {
        self.text_block_pick
            .as_ref()?
            .position
            .and_then(|position| self.grid_point(position, cx))
    }

    /// Picks the whole block, or a paragraph or a sentence of it: the one
    /// under the pointer, else the one the pick is in. Clears the texts
    /// picked to act on together, which are of the old level.
    pub fn set_text_block_pick_level(&mut self, level: TextBlockLevel, cx: &mut Context<Self>) {
        let Some(pick) = &self.text_block_pick else {
            return;
        };
        if pick.level == level {
            return;
        }
        let from_message = pick.level == TextBlockLevel::Message;
        let position = self
            .pick_point(cx)
            .and_then(|point| self.level_point(point, level, from_message, cx))
            .and_then(|point| self.scrollback_position(point, cx))
            .or(pick.position);
        if let Some(pick) = &mut self.text_block_pick {
            pick.level = level;
            pick.position = position;
            pick.collected.clear();
        }
        cx.notify();
    }

    /// Where a pick at `point` rests at `level`: the start of the unit
    /// under the pointer when it's in the same block, else of the unit at
    /// `point`, or of the block's first when the pick is on the whole
    /// block.
    fn level_point(
        &self,
        point: Point,
        level: TextBlockLevel,
        from_message: bool,
        cx: &App,
    ) -> Option<Point> {
        if level == TextBlockLevel::Message {
            return Some(point);
        }
        let block = self.text_block_at(point, cx)?;
        // The pointer only counts while it's on the picked text, since the
        // keyboard may have moved the pick away from it.
        let current_unit = self
            .text_block_pick
            .as_ref()
            .and_then(|pick| self.unit_at(point, pick.level, cx))
            .map(|(unit, _)| unit);
        let unit_point = self
            .pointer_point(cx)
            .filter(|pointer| {
                current_unit
                    .as_ref()
                    .is_some_and(|unit| unit_contains(unit, *pointer))
            })
            .or_else(|| from_message.then(|| unit_start(&block)))
            .unwrap_or(point);
        let (unit, _) = self.unit_at(unit_point, level, cx)?;
        Some(unit_start(&unit))
    }

    /// The nearest level past the pick's, toward smaller texts when
    /// `narrower` is set, that picks other text than the pick does now.
    /// Levels that would pick the same text as their neighbor are skipped,
    /// toward the message when widening.
    fn next_distinct_level(&self, narrower: bool, cx: &App) -> Option<TextBlockLevel> {
        let pick = self.text_block_pick.as_ref()?;
        let point = self.pick_point(cx)?;
        let current = self.unit_at(point, pick.level, cx)?.0.range();
        let index = TextBlockLevel::ALL
            .iter()
            .position(|level| *level == pick.level)?;
        let candidates: Vec<TextBlockLevel> = if narrower {
            TextBlockLevel::ALL[index + 1..].to_vec()
        } else {
            TextBlockLevel::ALL[..index].iter().rev().copied().collect()
        };
        let from_message = pick.level == TextBlockLevel::Message;
        let ranges = candidates
            .iter()
            .map(|level| {
                self.level_point(point, *level, from_message, cx)
                    .and_then(|point| self.unit_at(point, *level, cx))
                    .and_then(|(unit, _)| unit.range())
            })
            .collect::<Vec<_>>();
        let first = ranges
            .iter()
            .position(|range| range.is_some() && *range != current)?;
        let mut chosen = first;
        if !narrower {
            while ranges
                .get(chosen + 1)
                .is_some_and(|range| *range == ranges[first])
            {
                chosen += 1;
            }
        }
        candidates.get(chosen).copied()
    }

    /// Picks a paragraph of the picked message, or a sentence of the
    /// picked paragraph.
    pub fn narrow_text_block_pick(&mut self, cx: &mut Context<Self>) {
        if let Some(level) = self.next_distinct_level(true, cx) {
            self.set_text_block_pick_level(level, cx);
        }
    }

    /// Picks the paragraph or the message the pick is in.
    pub fn widen_text_block_pick(&mut self, cx: &mut Context<Self>) {
        if let Some(level) = self.next_distinct_level(false, cx) {
            self.set_text_block_pick_level(level, cx);
        }
    }

    /// Clears the texts picked to act on together, else goes back to the
    /// paragraph or the message the pick is in, else ends the pick.
    pub fn back_out_of_text_block_pick(&mut self, cx: &mut Context<Self>) {
        if let Some(pick) = &mut self.text_block_pick
            && !pick.collected.is_empty()
        {
            pick.collected.clear();
            cx.notify();
            return;
        }
        match self.next_distinct_level(false, cx) {
            Some(level) => self.set_text_block_pick_level(level, cx),
            None => self.cancel_text_block_pick(cx),
        }
    }

    pub fn cancel_text_block_pick(&mut self, cx: &mut Context<Self>) {
        if self.text_block_pick.take().is_some() {
            self.picked_text_block = None;
            cx.notify();
        }
    }

    /// Moves the pick to the next or the previous text at the pick's level,
    /// which carries on into the neighboring block.
    pub fn move_text_block_pick(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(pick) = &self.text_block_pick else {
            return;
        };
        let point = self.adjacent_unit_point(self.pick_point(cx), pick.level, forward, cx);
        let Some(position) = point.and_then(|point| self.scrollback_position(point, cx)) else {
            return;
        };
        if let Some(pick) = &mut self.text_block_pick {
            pick.position = Some(position);
        }
        cx.notify();
    }

    /// Adds the picked text to the texts to act on together, or takes it
    /// out when it's already one of them, and moves the pick on.
    pub fn extend_text_block_pick(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(pick) = &self.text_block_pick else {
            return;
        };
        let point = self.pick_point(cx);
        let start = point
            .and_then(|point| self.unit_at(point, pick.level, cx))
            .and_then(|(unit, _)| self.scrollback_position(unit_start(&unit), cx));
        // At the first or the last text there's nowhere to move, so the
        // text is only added, or pressing again would take it back out.
        let at_end = self
            .adjacent_unit_point(point, pick.level, forward, cx)
            .is_none();
        if let Some(start) = start
            && let Some(pick) = &mut self.text_block_pick
        {
            match pick.collected.iter().position(|other| *other == start) {
                Some(_) if at_end => {}
                Some(index) => {
                    pick.collected.remove(index);
                }
                None => pick.collected.push(start),
            }
        }
        self.move_text_block_pick(forward, cx);
        cx.notify();
    }

    fn adjacent_unit_point(
        &self,
        current_point: Option<Point>,
        level: TextBlockLevel,
        forward: bool,
        cx: &App,
    ) -> Option<Point> {
        if level == TextBlockLevel::Message {
            return self.adjacent_block_point(current_point, forward, cx);
        }
        let Some((unit, block)) = current_point.and_then(|point| self.unit_at(point, level, cx))
        else {
            return self.adjacent_block_point(current_point, forward, cx);
        };
        let units = self.block_units(&block, level, cx);
        let index = units.iter().position(|other| *other == unit)?;
        let next_index = if forward {
            index.checked_add(1)
        } else {
            index.checked_sub(1)
        };
        if let Some(next) = next_index.and_then(|index| units.get(index)) {
            return Some(unit_start(next));
        }
        let block_point = self.adjacent_block_point(current_point, forward, cx)?;
        let next_block = self.text_block_at(block_point, cx)?;
        let next_units = self.block_units(&next_block, level, cx);
        let next = if forward {
            next_units.first()
        } else {
            next_units.last()
        };
        Some(next.map_or(block_point, unit_start))
    }

    fn adjacent_block_point(
        &self,
        current_point: Option<Point>,
        forward: bool,
        cx: &App,
    ) -> Option<Point> {
        let visible_lines = self.visible_lines(cx);
        let current_lines = current_point
            .and_then(|point| self.text_block_at(point, cx))
            .and_then(|block| block.lines());
        if forward {
            let first_line = current_lines
                .map(|lines| lines.end() + 1)
                .or(current_point.map(|point| point.line + 1))
                .unwrap_or(*visible_lines.start());
            self.next_text_block_point(first_line..=*visible_lines.end(), cx)
        } else {
            let first_line = current_lines
                .map(|lines| lines.start() - 1)
                .or(current_point.map(|point| point.line - 1))
                .unwrap_or(*visible_lines.end());
            self.next_text_block_point((*visible_lines.start()..=first_line).rev(), cx)
        }
    }

    /// The picked texts' range, their text, their level and the window
    /// position the action opens at. Texts picked to act on together are
    /// joined in the order they're on screen, with a line of `…` where
    /// other text is between them. Ends the pick unless `keep_picking` is
    /// set.
    pub fn take_text_block_pick_target(
        &mut self,
        keep_picking: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(Range, String, PickedTextKind, GpuiPoint<Pixels>)> {
        if !self.takes_mouse_for_text_block_pick() {
            return None;
        }
        let level = self.text_block_pick.as_ref()?.level;
        let picked = self.picked_text(cx)?;
        let content = self.terminal.read(cx).last_content();
        let origin = terminal_element::text_block_action_origin(
            picked.anchor,
            content.display_offset,
            content.terminal_bounds,
            self.scroll_top,
            window,
            cx,
        )?;
        let mut units = picked.collected;
        units.push(picked.block);
        units.sort_by_key(unit_start);
        units.dedup_by_key(|unit| unit_start(unit));
        let range = Range::new(
            units.first()?.range()?.start(),
            units.last()?.range()?.end(),
        );
        let mut text = String::new();
        for (index, unit) in units.iter().enumerate() {
            if let Some(previous) = index.checked_sub(1).and_then(|index| units.get(index)) {
                let next_start =
                    self.adjacent_unit_point(Some(unit_start(previous)), level, true, cx);
                text.push_str(if next_start != Some(unit_start(unit)) {
                    "\n…\n"
                } else if level == TextBlockLevel::Sentence {
                    " "
                } else {
                    "\n"
                });
            }
            text.push_str(&unit.text);
        }
        if !keep_picking {
            self.cancel_text_block_pick(cx);
        } else if let Some(pick) = &mut self.text_block_pick {
            pick.collected.clear();
        }
        let kind = PickedTextKind {
            level,
            combined: units.len() > 1,
        };
        Some((range, text, kind, origin))
    }

    /// The text the pick points at, looked up from the terminal's content.
    fn picked_text(&self, cx: &App) -> Option<PickedTextBlock> {
        let pick = self.text_block_pick.as_ref()?;
        let point = self.pick_point(cx)?;
        let (unit, block) = self.unit_at(point, pick.level, cx)?;
        let anchor = if unit_contains(&unit, point) {
            point
        } else {
            unit_start(&unit)
        };
        let collected = pick
            .collected
            .iter()
            .filter_map(|position| {
                let start = self.grid_point(*position, cx)?;
                let (collected, _) = self.unit_at(start, pick.level, cx)?;
                (unit_start(&collected) == start && collected != unit).then_some(collected)
            })
            .collect();
        Some(PickedTextBlock {
            block: unit,
            parent: (pick.level != TextBlockLevel::Message).then_some(block),
            collected,
            anchor,
        })
    }

    /// The text at `level` at `point`, else the first one after it in its
    /// block, else the block's last, with the block.
    fn unit_at(
        &self,
        point: Point,
        level: TextBlockLevel,
        cx: &App,
    ) -> Option<(TextBlock, TextBlock)> {
        let block = self.text_block_at(point, cx)?;
        let units = self.block_units(&block, level, cx);
        let unit = units
            .iter()
            .find(|unit| unit_contains(unit, point))
            .or_else(|| units.iter().find(|unit| unit_start(unit) >= point))
            .or(units.last())?
            .clone();
        Some((unit, block))
    }

    /// The texts of a block at `level`: the block itself, its paragraphs or
    /// their sentences.
    fn block_units(&self, block: &TextBlock, level: TextBlockLevel, cx: &App) -> Vec<TextBlock> {
        if level == TextBlockLevel::Message {
            return vec![block.clone()];
        }
        let Some(lines) = block.lines() else {
            return Vec::new();
        };
        let terminal = self.terminal.read(cx);
        let grid_rows = terminal.grid_rows(lines);
        let paragraphs = paragraph_rows(block, &grid_rows);
        let unit_rows = match level {
            TextBlockLevel::Sentence => paragraphs
                .iter()
                .flat_map(|paragraph| sentence_rows(paragraph, &grid_rows))
                .collect(),
            _ => paragraphs,
        };
        unit_rows
            .into_iter()
            .map(|rows| {
                let mut text = String::new();
                for ((line, _), row_text) in rows.iter().zip(terminal.rows_text(&rows)) {
                    // A row the program wrapped ends at a space it left out.
                    let terminal_wrapped = grid_rows
                        .iter()
                        .any(|row| row.line + 1 == *line && row.wraps);
                    if !text.is_empty() && !terminal_wrapped {
                        text.truncate(text.trim_end().len());
                        text.push(' ');
                        text.push_str(row_text.trim_start());
                    } else {
                        text.push_str(&row_text);
                    }
                }
                TextBlock {
                    rows,
                    text,
                    label: None,
                    paragraphs: Vec::new(),
                }
            })
            .collect()
    }

    /// The first line's text block, as the point that a keyboard pick rests
    /// on: the end of the block's first row.
    fn next_text_block_point(
        &self,
        mut lines: impl Iterator<Item = i32>,
        cx: &App,
    ) -> Option<Point> {
        lines.find_map(|line| {
            let block = self.text_block_at(Point::new(line, 0), cx)?;
            let (first_line, columns) = block.rows.first()?;
            Some(Point::new(*first_line, *columns.end()))
        })
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

    fn scrollback_position(&self, point: Point, cx: &App) -> Option<(i64, usize)> {
        let content = self.terminal.read(cx).last_content();
        let history_lines =
            i64::try_from(content.total_lines).ok()? - i64::try_from(content.screen_lines).ok()?;
        Some((history_lines + i64::from(point.line), point.column))
    }

    fn grid_point(&self, (row, column): (i64, usize), cx: &App) -> Option<Point> {
        let content = self.terminal.read(cx).last_content();
        let history_lines =
            i64::try_from(content.total_lines).ok()? - i64::try_from(content.screen_lines).ok()?;
        Some(Point::new(i32::try_from(row - history_lines).ok()?, column))
    }

    pub(crate) fn set_pointer_cell(
        &mut self,
        pointer_cell: Option<(usize, usize)>,
        cx: &mut Context<Self>,
    ) {
        if self.pointer_cell == pointer_cell {
            return;
        }
        self.pointer_cell = pointer_cell;
        if !self.takes_mouse_for_text_block_pick() {
            return;
        }
        // A pointer leaving the terminal keeps the pick where it was, so
        // the keyboard can carry on from there.
        let Some(position) = self
            .pointer_point(cx)
            .and_then(|point| self.scrollback_position(point, cx))
        else {
            return;
        };
        if let Some(pick) = &mut self.text_block_pick {
            pick.position = Some(position);
        }
        cx.notify();
    }

    /// What clicking or Enter does to the picked block, and the other keys.
    /// The block is looked up here rather than at paint, so the hint
    /// matches the block that this frame highlights.
    fn text_block_pick_hint(&self, cx: &App) -> Option<SharedString> {
        if !self.takes_mouse_for_text_block_pick() {
            return None;
        }
        let pick = self.text_block_pick.as_ref()?;
        let level = pick.level;
        let picked = self.picked_text(cx);
        let position = picked.as_ref().and_then(|picked| {
            let parent = picked.parent.as_ref()?;
            let units = self.block_units(parent, level, cx);
            let index = units.iter().position(|unit| *unit == picked.block)?;
            Some(format!(" {}/{}", index + 1, units.len()))
        });
        let collected = picked.as_ref().map_or(0, |picked| picked.collected.len());
        let label = picked.map(|picked| picked.block).map(|block| {
            block.label.or_else(|| {
                self.selection_action
                    .as_ref()
                    .map(|action| action.label.clone())
            })
        });
        let escape = if collected > 0 {
            "Esc to clear".to_string()
        } else {
            match self.next_distinct_level(false, cx) {
                Some(wider) => format!("Esc for the {}", wider.name()),
                None => "Esc to cancel".to_string(),
            }
        };
        let mut level_name = level.name().to_string();
        if let Some(first) = level_name.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let picked_count = if collected > 0 {
            format!(" · {} picked", collected + 1)
        } else {
            String::new()
        };
        let place = format!("{level_name}{}{picked_count}", position.unwrap_or_default());
        Some(match label {
            Some(Some(label)) => format!(
                "{place} · {label}: click or Enter · Shift+↑↓ to add · ↑↓ to move · ←→ for message, paragraph or sentence · {escape}"
            )
            .into(),
            _ => format!("Point at a message · ↑↓ to move · {escape}").into(),
        })
    }

    fn text_block_at(&self, point: Point, cx: &App) -> Option<TextBlock> {
        let provider = self.text_block_provider.as_ref()?;
        provider(self.terminal.read(cx), point, cx).filter(|block| !block.rows.is_empty())
    }

    /// Looks the picked block up again, since output may have changed it,
    /// and ends a pick the terminal lost focus during. Runs before every
    /// paint.
    pub(crate) fn update_picked_text_block(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.takes_mouse_for_text_block_pick() && !self.focus_handle.is_focused(window) {
            self.text_block_pick = None;
            // This paint already has the pick's border and hint.
            cx.notify();
        }
        self.picked_text_block = None;
        if !self.takes_mouse_for_text_block_pick() {
            return;
        }
        self.picked_text_block = self.picked_text(cx);
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
            self.cancel_text_block_pick(cx);
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

    pub fn deploy_context_menu(
        &mut self,
        position: GpuiPoint<Pixels>,
        has_selection: bool,
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
            .filter(|_| has_selection)
            .zip(self.selection_action_target(window, cx));
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
                    |menu, (SelectionAction { label, handler }, (range, text, origin))| {
                        menu.entry(label, None, move |window, cx| {
                            handler(range, text.clone(), None, origin, window, cx)
                        })
                    },
                )
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
        self.cancel_text_block_pick(cx);

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
            self.cancel_text_block_pick(cx);
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
        self.cancel_text_block_pick(cx);
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
            self.cancel_text_block_pick(cx);
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
        if self.takes_mouse_for_text_block_pick() {
            dispatch_context.add("text_block_pick");
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
        // Keys that picking binds never get here, and any other key types
        // into the program as usual.
        self.cancel_text_block_pick(cx);
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
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if !this
                        .terminal
                        .read(cx)
                        .mouse_mode(event.modifiers.shift, this.mouse_input_mode())
                    {
                        let had_selection = this.terminal.read(cx).last_content.selection.is_some();
                        if !had_selection {
                            this.terminal.update(cx, |terminal, _| {
                                terminal.select_word_at_event_position(event);
                            });
                        }
                        let has_selection = !had_selection
                            || this
                                .terminal
                                .read(cx)
                                .last_content
                                .selection_text
                                .as_ref()
                                .is_some_and(|text| !text.is_empty());
                        this.deploy_context_menu(event.position, has_selection, window, cx);
                        cx.notify();
                    }
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
            .when_some(self.text_block_pick_hint(cx), |this, hint| {
                let colors = cx.theme().colors();
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .border_1()
                        .border_color(colors.border_focused),
                )
                .child(
                    div()
                        .absolute()
                        .bottom_1()
                        .right_3()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.elevated_surface_background)
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
                        .children(self.render_text_block_level_buttons(cx)),
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

impl TerminalView {
    /// Switches between picking messages, paragraphs and sentences, for
    /// pointer users who don't know the keys.
    fn render_text_block_level_buttons(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let current_level = self.text_block_pick_level()?;
        let button = |id: &'static str, label: &'static str, level: TextBlockLevel| {
            Button::new(id, label)
                .label_size(LabelSize::XSmall)
                .toggle_state(current_level == level)
                .tooltip(Tooltip::text(format!(
                    "Pick a {} · ←→ to change",
                    level.name()
                )))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.set_text_block_pick_level(level, cx)
                }))
        };
        Some(
            h_flex()
                .gap_0p5()
                .child(button(
                    "text-block-level-message",
                    "Message",
                    TextBlockLevel::Message,
                ))
                .child(button(
                    "text-block-level-paragraph",
                    "Paragraph",
                    TextBlockLevel::Paragraph,
                ))
                .child(button(
                    "text-block-level-sentence",
                    "Sentence",
                    TextBlockLevel::Sentence,
                ))
                .into_any_element(),
        )
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
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};
    use util::paths::PathStyle;
    use util::rel_path::RelPath;
    use workspace::item::test::{TestItem, TestProjectItem};
    use workspace::{AppState, MultiWorkspace, SelectedEntry};

    /// `text` without its first line break and the indentation its lines
    /// share.
    fn dedent(text: &str) -> String {
        let text = text.strip_prefix('\n').unwrap_or(text);
        let text = text.trim_end_matches(' ');
        let indent = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        text.lines()
            .map(|line| format!("{}\n", line.get(indent..).unwrap_or("")))
            .collect()
    }

    /// The grid rows of `text`, padded to `width` columns, and a block of
    /// its lines with text. A line ending in `\` wraps.
    fn text_grid(text: &str, width: usize) -> (Vec<GridRow>, TextBlock) {
        let mut grid_rows = Vec::new();
        let mut block_rows = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let (line, wraps) = match line.strip_suffix('\\') {
                Some(line) => (line, true),
                None => (line, false),
            };
            let mut characters = line.chars().collect::<Vec<_>>();
            characters.resize(width.max(characters.len()), ' ');
            let line_number = index as i32;
            let first = characters
                .iter()
                .position(|character| !character.is_whitespace());
            let last = characters
                .iter()
                .rposition(|character| !character.is_whitespace());
            if let (Some(first), Some(last)) = (first, last) {
                let last = if wraps { characters.len() - 1 } else { last };
                block_rows.push((line_number, first..=last));
            }
            grid_rows.push(GridRow {
                line: line_number,
                characters,
                wide_char_spacers: Vec::new(),
                wraps,
            });
        }
        let block = TextBlock {
            rows: block_rows,
            text: String::new(),
            label: None,
            paragraphs: Vec::new(),
        };
        (grid_rows, block)
    }

    fn rows_text(rows: &[(i32, RangeInclusive<usize>)], grid_rows: &[GridRow]) -> String {
        let mut text = String::new();
        for (line, columns) in rows {
            let Some(row) = grid_rows.iter().find(|row| row.line == *line) else {
                continue;
            };
            let row_text = columns
                .clone()
                .filter_map(|column| row.characters.get(column))
                .collect::<String>();
            if !text.is_empty() && !text.ends_with(' ') {
                text.push(' ');
            }
            text.push_str(row_text.trim());
        }
        text
    }

    fn paragraph_texts(block: &TextBlock, grid_rows: &[GridRow]) -> Vec<String> {
        paragraph_rows(block, grid_rows)
            .iter()
            .map(|rows| rows_text(rows, grid_rows))
            .collect()
    }

    fn sentence_texts(block: &TextBlock, grid_rows: &[GridRow]) -> Vec<String> {
        paragraph_rows(block, grid_rows)
            .iter()
            .flat_map(|paragraph| sentence_rows(paragraph, grid_rows))
            .map(|rows| rows_text(&rows, grid_rows))
            .collect()
    }

    fn paragraphs_of(text: &str) -> Vec<String> {
        let (grid_rows, block) = text_grid(text, 40);
        paragraph_texts(&block, &grid_rows)
    }

    #[test]
    fn test_paragraphs_of_an_agent_message() {
        assert_eq!(
            paragraphs_of(&dedent(
                "
                  The build failed because the linker
                  couldn't find the library. I fixed
                  Cargo.toml and reran the tests.

                  All of them pass now.
            "
            )),
            vec![
                "The build failed because the linker couldn't find the library. I fixed Cargo.toml and reran the tests.",
                "All of them pass now.",
            ],
            "rows the program wrapped join, a row starting with \"I\" or a \
             name included, and a blank line ends a paragraph"
        );
    }

    #[test]
    fn test_paragraphs_join_rows_the_terminal_wrapped() {
        assert_eq!(
            paragraphs_of("A row the terminal wrapped at its edg\\\ne goes on.\nNext line\n"),
            vec![
                "A row the terminal wrapped at its edg e goes on.",
                "Next line"
            ],
        );
    }

    #[test]
    fn test_paragraphs_of_bullets_with_wrapped_rows() {
        assert_eq!(
            paragraphs_of(&dedent(
                "
                - First step that takes long enough
                  to wrap onto a second row.
                - Second step.
                  • A nested bullet that is long
                    enough to wrap as well.
                * Third step.
            "
            )),
            vec![
                "- First step that takes long enough to wrap onto a second row.",
                "- Second step.",
                "• A nested bullet that is long enough to wrap as well.",
                "* Third step.",
            ],
        );
    }

    #[test]
    fn test_paragraphs_of_numbered_and_lettered_lists() {
        assert_eq!(
            paragraphs_of(&dedent(
                "
                1. Move the parser into its own crate
                   so it can be tested alone.
                2. Add tests.
                a) Keep the old API as it is
                   for now.
                b) Remove it.
            "
            )),
            vec![
                "1. Move the parser into its own crate so it can be tested alone.",
                "2. Add tests.",
                "a) Keep the old API as it is for now.",
                "b) Remove it.",
            ],
        );
    }

    #[test]
    fn test_paragraphs_keep_code_diffs_listings_and_logs_apart() {
        for text in [
            &dedent(
                "
                fn long_name(arg: Type) -> Result {
                    let value = compute(arg);
                    value
            ",
            ),
            &dedent(
                "
                @@ -1,3 +1,4 @@ fn main() in a file
                +    let added = line();
                     unchanged context line
            ",
            ),
            &dedent(
                "
                drwxr-xr-x  5 user staff 160 crates
                drwxr-xr-x  5 user staff 160 assets
            ",
            ),
            &dedent(
                "
                2026-11-10T10:00:01 INFO started ok
                2026-11-10T10:00:02 INFO listening
            ",
            ),
            &dedent(
                "
                error[E0308]: mismatched types here, found
                warning: unused variable `value` in main
            ",
            ),
            &dedent(
                "
                ## A heading on its own line
                Text under the heading that is long
            ",
            ),
        ] {
            let paragraphs = paragraphs_of(text);
            assert_eq!(paragraphs.len(), text.lines().count(), "{paragraphs:?}");
        }
    }

    #[test]
    fn test_paragraphs_of_quotes_and_tool_results() {
        assert_eq!(
            paragraphs_of(&dedent(
                "
                > A quoted reply that runs long enough
                  to wrap onto the next row.
                Read 42 lines from the file you named
                and found nothing.
            "
            )),
            vec![
                "> A quoted reply that runs long enough to wrap onto the next row.",
                "Read 42 lines from the file you named and found nothing.",
            ],
        );
    }

    #[test]
    fn test_paragraph_runs_keep_their_lines_together() {
        let (grid_rows, mut block) = text_grid(
            &dedent(
                "
                1. Which auth method should we use?
                   a) OAuth
                   b) API key
                2. Should tokens expire?
            ",
            ),
            40,
        );
        block.paragraphs = vec![0..=2];
        assert_eq!(
            paragraph_texts(&block, &grid_rows),
            vec![
                "1. Which auth method should we use? a) OAuth b) API key",
                "2. Should tokens expire?",
            ],
        );
    }

    #[test]
    fn test_paragraphs_of_wide_characters() {
        let (mut grid_rows, block) = text_grid(
            "界 is a wide character in a long row\nand this row carries it on.\n",
            40,
        );
        grid_rows[0].characters.insert(1, ' ');
        grid_rows[0].characters.pop();
        grid_rows[0].wide_char_spacers = vec![1];
        let block = TextBlock {
            rows: vec![(0, 0..=36), (1, 0..=26)],
            ..block
        };
        assert_eq!(paragraph_rows(&block, &grid_rows).len(), 1);
    }

    #[test]
    fn test_sentences_of_paragraphs() {
        let (grid_rows, block) = text_grid(
            &dedent(
                "
                One. Two runs on over a row that the
                program wrapped. Three
                No stop
            ",
            ),
            40,
        );
        assert_eq!(
            sentence_texts(&block, &grid_rows),
            vec![
                "One.",
                "Two runs on over a row that the program wrapped.",
                "Three",
                "No stop"
            ],
        );
    }

    #[test]
    fn test_sentences_go_on_after_abbreviations() {
        let (grid_rows, block) = text_grid(
            "Use a parser, e.g. Pest. Ask Dr. Who. It ends etc. Then more.\n",
            70,
        );
        assert_eq!(
            sentence_texts(&block, &grid_rows),
            vec![
                "Use a parser, e.g. Pest.",
                "Ask Dr. Who.",
                "It ends etc.",
                "Then more."
            ],
        );
    }

    #[test]
    fn test_sentence_rows_skip_wide_char_spacers() {
        // The spacer at column 1 holds a space in the grid.
        let grid_rows = [GridRow {
            line: 0,
            characters: "界  x. Next.".chars().collect(),
            wide_char_spacers: vec![1],
            wraps: false,
        }];
        assert_eq!(
            sentence_rows(&[(0, 0..=10)], &grid_rows),
            vec![vec![(0, 0..=4)], vec![(0, 6..=10)]],
        );
    }

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
    async fn test_set_font_scale_notifies_only_on_change(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, _terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, true, cx);
        let notifications = Rc::new(Cell::new(0));
        let _subscription = cx.update({
            let notifications = notifications.clone();
            |cx| {
                cx.observe(&terminal_view, move |_, _| {
                    notifications.set(notifications.get() + 1);
                })
            }
        });
        let set_font_scale = |font_scale: Option<f32>, cx: &mut TestAppContext| {
            cx.update(|cx| {
                terminal_view.update(cx, |view, cx| view.set_font_scale(font_scale, cx));
            });
        };

        set_font_scale(Some(0.8), cx);
        assert_eq!(notifications.get(), 1);
        set_font_scale(Some(0.8), cx);
        assert_eq!(notifications.get(), 1);
        set_font_scale(None, cx);
        assert_eq!(notifications.get(), 2);
        set_font_scale(None, cx);
        assert_eq!(notifications.get(), 2);
        assert_eq!(
            terminal_view.read_with(cx, |view, _| view.font_scale()),
            None
        );
    }

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

    type SelectionActions = Rc<RefCell<Vec<(Range, String)>>>;

    fn record_selection_actions(
        terminal_view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
    ) -> SelectionActions {
        let actions = SelectionActions::default();
        terminal_view.update(cx, |terminal_view, cx| {
            let actions = actions.clone();
            terminal_view.on_selection_action(
                "Act on Selection",
                move |range, text, _, _, _, _| actions.borrow_mut().push((range, text)),
                cx,
            );
        });
        actions
    }

    /// Writes `text` on the first line and returns the window position of
    /// the middle of the cell at `column`.
    fn write_first_line(
        terminal: &Entity<Terminal>,
        text: &str,
        cx: &mut VisualTestContext,
    ) -> impl Fn(f32) -> GpuiPoint<Pixels> + use<> {
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(text.as_bytes(), cx);
                terminal.sync(window, cx);
            });
        });
        cx.run_until_parked();
        let bounds = terminal.read_with(cx, |terminal, _| terminal.last_content.terminal_bounds);
        move |column| {
            bounds.bounds.origin + gpui::point(bounds.cell_width * column, bounds.line_height * 0.5)
        }
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

    /// Offers a block on each of `lines`, spanning the line's first five
    /// columns.
    fn provide_line_blocks(
        terminal_view: &Entity<TerminalView>,
        lines: RangeInclusive<i32>,
        cx: &mut VisualTestContext,
    ) {
        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.set_text_block_provider(
                move |_, point, _| {
                    lines.contains(&point.line).then(|| TextBlock {
                        rows: vec![(point.line, 0..=4)],
                        text: "hello".into(),
                        label: None,
                        paragraphs: Vec::new(),
                    })
                },
                cx,
            );
        });
    }

    fn start_pick(terminal_view: &Entity<TerminalView>, cx: &mut VisualTestContext) {
        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.start_text_block_pick(cx)
        });
        cx.run_until_parked();
    }

    fn move_pick(terminal_view: &Entity<TerminalView>, forward: bool, cx: &mut VisualTestContext) {
        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.move_text_block_pick(forward, cx)
        });
        cx.run_until_parked();
    }

    fn picked_line(
        terminal_view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
    ) -> Option<i32> {
        terminal_view.read_with(cx, |terminal_view, _| {
            let picked = terminal_view.picked_text_block.as_ref()?;
            Some(picked.block.rows.first()?.0)
        })
    }

    fn is_picking(terminal_view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> bool {
        terminal_view.read_with(cx, |terminal_view, _| terminal_view.is_picking_text_block())
    }

    #[gpui::test]
    async fn hovering_highlights_nothing_until_a_pick_starts(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        provide_line_blocks(&terminal_view, 0..=0, &mut cx);
        let cell = write_first_line(&terminal, "hello world\n", &mut cx);

        cx.simulate_mouse_move(cell(1.5), None, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(picked_line(&terminal_view, &mut cx), None);

        start_pick(&terminal_view, &mut cx);
        assert_eq!(
            picked_line(&terminal_view, &mut cx),
            Some(0),
            "the pick should start under the pointer, without waiting"
        );
        let hint = terminal_view.read_with(&cx, |terminal_view, cx| {
            terminal_view.text_block_pick_hint(cx)
        });
        assert!(
            hint.is_some_and(
                |hint| hint.starts_with("Paragraph 1/1 · Act on Selection: click or Enter")
            ),
            "the hint should name the action on the picked block"
        );

        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        assert!(
            !is_picking(&terminal_view, &mut cx),
            "typing should end the pick"
        );
        assert_eq!(picked_line(&terminal_view, &mut cx), None);
    }

    #[gpui::test]
    async fn keyboard_pick_moves_between_blocks_on_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        provide_line_blocks(&terminal_view, 1..=3, &mut cx);
        let _cell = write_first_line(&terminal, "a\r\nb\r\nc\r\nd\r\n", &mut cx);

        start_pick(&terminal_view, &mut cx);
        assert_eq!(
            picked_line(&terminal_view, &mut cx),
            Some(3),
            "without the pointer the pick should start at the last block on screen"
        );
        move_pick(&terminal_view, false, &mut cx);
        assert_eq!(picked_line(&terminal_view, &mut cx), Some(2));
        move_pick(&terminal_view, false, &mut cx);
        move_pick(&terminal_view, false, &mut cx);
        assert_eq!(
            picked_line(&terminal_view, &mut cx),
            Some(1),
            "the pick should stop at the first block"
        );
        move_pick(&terminal_view, true, &mut cx);
        assert_eq!(picked_line(&terminal_view, &mut cx), Some(2));
    }

    #[gpui::test]
    async fn pick_starts_at_the_vi_cursor(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        provide_line_blocks(&terminal_view, 0..=3, &mut cx);
        let cell = write_first_line(&terminal, "a\r\nb\r\nc\r\nd\r\n", &mut cx);
        cx.simulate_mouse_move(cell(0.5), None, gpui::Modifiers::default());
        terminal.update(&mut cx, |terminal, _| terminal.toggle_vi_mode());
        cx.simulate_keystrokes("k k");
        cx.run_until_parked();
        let cursor_line =
            terminal.read_with(&cx, |terminal, _| terminal.last_content().cursor.point.line);

        start_pick(&terminal_view, &mut cx);
        assert_eq!(picked_line(&terminal_view, &mut cx), Some(cursor_line));
        assert_ne!(cursor_line, 0, "the vi cursor should win over the pointer");
    }

    #[gpui::test]
    async fn clicking_a_picked_block_runs_the_action_without_reporting_the_click(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let actions = record_selection_actions(&terminal_view, &mut cx);
        provide_line_blocks(&terminal_view, 0..=0, &mut cx);
        let cell = write_first_line(
            &terminal,
            &format!("{ENABLE_MOUSE_REPORTING}hello world\n"),
            &mut cx,
        );
        terminal.update(&mut cx, |terminal, _| terminal.take_pty_write_log());

        start_pick(&terminal_view, &mut cx);
        click(cell(1.5), &mut cx);
        assert_eq!(
            actions.borrow().as_slice(),
            &[(
                Range::new(Point::new(0, 0), Point::new(0, 4)),
                "hello".into()
            )]
        );
        assert!(
            !is_picking(&terminal_view, &mut cx),
            "a click should end the pick"
        );
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty(),
            "the program shouldn't see the click, or its release",
        );

        drag(cell(0.1), cell(4.9), &mut cx);
        assert!(
            !terminal
                .update(&mut cx, |terminal, _| terminal.take_pty_write_log())
                .is_empty(),
            "after the pick the program should get the mouse again",
        );
    }

    #[gpui::test]
    async fn shift_click_keeps_picking(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let actions = record_selection_actions(&terminal_view, &mut cx);
        provide_line_blocks(&terminal_view, 0..=0, &mut cx);
        let cell = write_first_line(&terminal, "hello world\n", &mut cx);

        start_pick(&terminal_view, &mut cx);
        cx.simulate_mouse_move(cell(1.5), None, gpui::Modifiers::default());
        cx.simulate_mouse_down(cell(1.5), MouseButton::Left, gpui::Modifiers::shift());
        cx.simulate_mouse_up(cell(1.5), MouseButton::Left, gpui::Modifiers::shift());
        cx.run_until_parked();

        assert_eq!(actions.borrow().len(), 1);
        assert!(is_picking(&terminal_view, &mut cx));
    }

    #[gpui::test]
    async fn a_pick_click_released_off_the_terminal_keeps_later_releases(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        let _actions = record_selection_actions(&terminal_view, &mut cx);
        provide_line_blocks(&terminal_view, 0..=0, &mut cx);
        let cell = write_first_line(&terminal, "hello world\n", &mut cx);
        let bounds = terminal.read_with(&cx, |terminal, _| terminal.last_content.terminal_bounds);
        let outside = bounds.bounds.bottom_right() + gpui::point(px(50.), px(50.));

        start_pick(&terminal_view, &mut cx);
        cx.simulate_mouse_move(cell(1.5), None, gpui::Modifiers::default());
        cx.simulate_mouse_down(cell(1.5), MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(outside, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(outside, MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();

        assert!(
            !terminal_view.read_with(&cx, |terminal_view, _| terminal_view.swallow_left_mouse_up),
            "a release anywhere should end the swallowing"
        );
        drag(cell(0.1), cell(4.9), &mut cx);
        assert_eq!(
            terminal.read_with(&cx, |terminal, _| terminal
                .last_content
                .selection_text
                .clone()),
            Some("hello".into()),
            "a later drag should select again"
        );
    }

    const ENABLE_MOUSE_REPORTING: &str = "\x1b[?1002h\x1b[?1006h";

    #[gpui::test]
    async fn picking_keeps_scroll_reports(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, true, false, cx);
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        provide_line_blocks(&terminal_view, 0..=0, &mut cx);
        let cell = write_first_line(
            &terminal,
            &format!("{ENABLE_MOUSE_REPORTING}hello world\n"),
            &mut cx,
        );
        start_pick(&terminal_view, &mut cx);
        terminal.update(&mut cx, |terminal, _| terminal.take_pty_write_log());

        cx.simulate_event(ScrollEvent {
            position: cell(2.0),
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
