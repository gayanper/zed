use std::{
    cell::RefCell,
    ops::{Range, RangeInclusive},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use collections::{HashMap, HashSet};
use editor::{
    CODE_ACTIONS_DEBOUNCE_TIMEOUT, Editor, EditorEvent, ToPoint as _,
    display_map::{
        BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, HighlightKey,
    },
    scroll::Autoscroll,
};
use gpui::{
    Anchor, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Global,
    Pixels, Point, Subscription, Task, WeakEntity, Window, actions, anchored, deferred, px,
};
use language::{
    Buffer, BufferEvent, BufferId, BufferSnapshot, Point as BufferPoint, ToOffset as _,
};
use multi_buffer::MultiBufferRow;
use ui::{KeyBinding, prelude::*};
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};
use zed_actions::agent_comments::ToggleComment;

actions!(
    agent_comments,
    [
        /// Adds the comment being written for the agent.
        Submit,
        /// Moves focus to the next control in the agent comment input.
        FocusNext,
        /// Moves focus to the previous control in the agent comment input.
        FocusPrevious,
        /// Inserts the visible agent session's pending comments into its prompt.
        InsertPendingComments,
        /// Removes the visible agent session's pending comments without inserting them.
        ClearPendingComments,
    ]
);

/// Editor lines taken by the comment input's title, button row and padding.
const INPUT_BLOCK_CHROME_LINES: u32 = 4;
const INPUT_MIN_LINES: usize = 2;
const INPUT_MAX_LINES: usize = 4;

#[derive(Clone, Debug, PartialEq)]
pub enum CommentSource {
    Code {
        path: SharedString,
        /// Zero-based buffer rows.
        rows: RangeInclusive<u32>,
    },
    Quote {
        label: SharedString,
        text: String,
    },
}

impl CommentSource {
    fn title(&self) -> SharedString {
        match self {
            Self::Code { path, rows } => format!("Comment on {}", code_location(path, rows)).into(),
            Self::Quote { label, .. } => format!("Comment on selection from {label}").into(),
        }
    }
}

/// A comment as it is written into the agent's prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct CommentPayload {
    pub source: CommentSource,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentCommentId(usize);

impl AgentCommentId {
    /// Ids are unique across every store, so an open input can tell whether
    /// its comment still exists after the visible thread changes.
    fn next() -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug)]
pub struct AgentComment {
    pub id: AgentCommentId,
    /// Keeps the buffer alive so the comment survives its file being closed.
    pub buffer: Entity<Buffer>,
    pub range: Range<language::Anchor>,
    pub body: String,
}

impl AgentComment {
    fn offset_range(&self, snapshot: &BufferSnapshot) -> Range<usize> {
        self.range.start.to_offset(snapshot)..self.range.end.to_offset(snapshot)
    }
}

pub enum AgentCommentStoreEvent {
    Changed,
}

/// The comments an agent session has collected, in the order they were added.
#[derive(Default)]
pub struct AgentCommentStore {
    comments: Vec<AgentComment>,
    buffer_subscriptions: HashMap<BufferId, Subscription>,
}

impl EventEmitter<AgentCommentStoreEvent> for AgentCommentStore {}

impl AgentCommentStore {
    pub fn add(
        &mut self,
        buffer: Entity<Buffer>,
        range: Range<language::Anchor>,
        body: String,
        cx: &mut Context<Self>,
    ) -> AgentCommentId {
        let buffer_id = buffer.read(cx).remote_id();
        if let collections::hash_map::Entry::Vacant(entry) =
            self.buffer_subscriptions.entry(buffer_id)
        {
            entry.insert(cx.subscribe(&buffer, move |this, _, event, cx| {
                if let BufferEvent::Edited { .. } = event {
                    this.remove_collapsed(buffer_id, cx);
                }
            }));
        }
        let id = AgentCommentId::next();
        self.comments.push(AgentComment {
            id,
            buffer,
            range,
            body,
        });
        cx.emit(AgentCommentStoreEvent::Changed);
        id
    }

    pub fn update(&mut self, id: AgentCommentId, body: String, cx: &mut Context<Self>) {
        if let Some(comment) = self.comments.iter_mut().find(|comment| comment.id == id) {
            comment.body = body;
            cx.emit(AgentCommentStoreEvent::Changed);
        }
    }

    pub fn remove(&mut self, id: AgentCommentId, cx: &mut Context<Self>) {
        let len = self.comments.len();
        self.comments.retain(|comment| comment.id != id);
        if self.comments.len() != len {
            self.drop_unused_subscriptions(cx);
            cx.emit(AgentCommentStoreEvent::Changed);
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.drain(cx);
    }

    pub fn drain(&mut self, cx: &mut Context<Self>) -> Vec<AgentComment> {
        let comments = std::mem::take(&mut self.comments);
        self.buffer_subscriptions.clear();
        if !comments.is_empty() {
            cx.emit(AgentCommentStoreEvent::Changed);
        }
        comments
    }

    pub fn len(&self) -> usize {
        self.comments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }

    pub fn contains(&self, id: AgentCommentId) -> bool {
        self.comments.iter().any(|comment| comment.id == id)
    }

    pub fn comments(&self) -> &[AgentComment] {
        &self.comments
    }

    pub fn comments_for_buffer(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> impl Iterator<Item = &AgentComment> {
        self.comments
            .iter()
            .filter(move |comment| comment.buffer.read(cx).remote_id() == buffer_id)
    }

    /// Comments overlapping `range`, in document order. An empty range
    /// matches comments that contain or touch it; a selection only touching
    /// a comment's edge doesn't, so text next to a comment can get its own.
    pub fn comments_at(
        &self,
        snapshot: &BufferSnapshot,
        range: Range<usize>,
        cx: &App,
    ) -> Vec<AgentComment> {
        let mut comments = self
            .comments_for_buffer(snapshot.remote_id(), cx)
            .filter_map(|comment| {
                let comment_range = comment.offset_range(snapshot);
                let overlaps = if range.is_empty() {
                    comment_range.start <= range.start && range.start <= comment_range.end
                } else {
                    comment_range.start < range.end && range.start < comment_range.end
                };
                overlaps.then(|| (comment_range, comment.clone()))
            })
            .collect::<Vec<_>>();
        comments.sort_by_key(|(comment_range, _)| (comment_range.start, comment_range.end));
        comments.into_iter().map(|(_, comment)| comment).collect()
    }

    /// Non-empty offset ranges in `snapshot`, sorted by start.
    pub fn offset_ranges(&self, snapshot: &BufferSnapshot, cx: &App) -> Vec<Range<usize>> {
        let mut ranges = self
            .comments_for_buffer(snapshot.remote_id(), cx)
            .map(|comment| comment.offset_range(snapshot))
            .filter(|range| range.start < range.end)
            .collect::<Vec<_>>();
        ranges.sort_by_key(|range| (range.start, range.end));
        ranges
    }

    /// All comments in the order they were added, with their locations read
    /// from the buffers now. `path_for` names each comment's file; comments it
    /// returns `None` for are skipped.
    pub fn pending_comments(
        &self,
        path_for: impl Fn(&Buffer, &App) -> Option<SharedString>,
        cx: &App,
    ) -> Vec<CommentPayload> {
        self.comments
            .iter()
            .filter_map(|comment| comment_payload(comment, &path_for, cx))
            .collect()
    }

    /// Removes and returns the comments `path_for` can name. The others are
    /// kept, so nothing the user wrote is lost.
    pub fn take_pending_comments(
        &mut self,
        path_for: impl Fn(&Buffer, &App) -> Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Vec<CommentPayload> {
        let mut payloads = Vec::new();
        let mut taken = HashSet::default();
        for comment in &self.comments {
            if let Some(payload) = comment_payload(comment, &path_for, cx) {
                payloads.push(payload);
                taken.insert(comment.id);
            }
        }
        if !taken.is_empty() {
            self.comments.retain(|comment| !taken.contains(&comment.id));
            self.drop_unused_subscriptions(cx);
            cx.emit(AgentCommentStoreEvent::Changed);
        }
        payloads
    }

    /// Drops comments whose text was deleted.
    fn remove_collapsed(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        let len = self.comments.len();
        self.comments.retain(|comment| {
            let buffer = comment.buffer.read(cx);
            if buffer.remote_id() != buffer_id {
                return true;
            }
            !comment.offset_range(&buffer.snapshot()).is_empty()
        });
        if self.comments.len() != len {
            self.drop_unused_subscriptions(cx);
            cx.emit(AgentCommentStoreEvent::Changed);
        }
    }

    fn drop_unused_subscriptions(&mut self, cx: &App) {
        let used = self
            .comments
            .iter()
            .map(|comment| comment.buffer.read(cx).remote_id())
            .collect::<HashSet<_>>();
        self.buffer_subscriptions
            .retain(|buffer_id, _| used.contains(buffer_id));
    }
}

fn comment_payload(
    comment: &AgentComment,
    path_for: &impl Fn(&Buffer, &App) -> Option<SharedString>,
    cx: &App,
) -> Option<CommentPayload> {
    let buffer = comment.buffer.read(cx);
    let snapshot = buffer.snapshot();
    Some(CommentPayload {
        source: CommentSource::Code {
            path: path_for(buffer, cx)?,
            rows: comment_rows(&snapshot, comment.offset_range(&snapshot)),
        },
        body: comment.body.clone(),
    })
}

/// The comment store of the agent session that is visible in the agent panel,
/// which is where comments are added and whose comments views show. It
/// notifies its observers when the session changes or its comments change.
#[derive(Default)]
pub struct ActiveCommentStore {
    store: Option<WeakEntity<AgentCommentStore>>,
    thread_title: SharedString,
    _store_subscription: Option<Subscription>,
}

struct GlobalActiveCommentStore(Entity<ActiveCommentStore>);

impl Global for GlobalActiveCommentStore {}

impl ActiveCommentStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalActiveCommentStore>() {
            return global.0.clone();
        }
        let active_store = cx.new(|_| Self::default());
        cx.set_global(GlobalActiveCommentStore(active_store.clone()));
        active_store
    }

    /// Makes `store` the one comments go to. `None` turns commenting off.
    pub fn set(
        &mut self,
        store: Option<&Entity<AgentCommentStore>>,
        thread_title: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.thread_title = thread_title;
        let current = self.store();
        if current.as_ref().map(Entity::entity_id) == store.map(Entity::entity_id) {
            return;
        }
        self.store = store.map(Entity::downgrade);
        self._store_subscription = store
            .map(|store| cx.subscribe(store, |_, _, _: &AgentCommentStoreEvent, cx| cx.notify()));
        cx.notify();
    }

    pub fn store(&self) -> Option<Entity<AgentCommentStore>> {
        self.store.as_ref()?.upgrade()
    }

    pub fn thread_title(&self) -> SharedString {
        self.thread_title.clone()
    }
}

pub fn active_comment_store(cx: &mut App) -> Option<Entity<AgentCommentStore>> {
    ActiveCommentStore::global(cx).read(cx).store()
}

/// Anchors a commented range so that text typed at either edge doesn't join it.
pub fn comment_anchor_range(
    snapshot: &BufferSnapshot,
    range: Range<usize>,
) -> Range<language::Anchor> {
    snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end)
}

/// Describes a range of a file's buffer as a code location.
pub fn code_source_for_buffer_range(
    buffer: &BufferSnapshot,
    range: Range<usize>,
    cx: &App,
) -> Option<CommentSource> {
    let file = buffer.file()?;
    Some(CommentSource::Code {
        path: file.path().display(file.path_style(cx)).into_owned().into(),
        rows: comment_rows(buffer, range),
    })
}

fn comment_rows(buffer: &BufferSnapshot, range: Range<usize>) -> RangeInclusive<u32> {
    let start = buffer.offset_to_point(range.start);
    let mut end = buffer.offset_to_point(range.end);
    // A range ending at the start of a line doesn't include that line.
    if end.column == 0 && end.row > start.row {
        end.row -= 1;
    }
    start.row..=end.row
}

fn code_location(path: &str, rows: &RangeInclusive<u32>) -> String {
    let start = rows.start() + 1;
    let end = rows.end() + 1;
    if start == end {
        format!("`{path}:{start}`")
    } else {
        format!("`{path}:{start}-{end}`")
    }
}

fn trimmed_lines(text: &str) -> Vec<&str> {
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let Some(first) = lines.iter().position(|line| !line.is_empty()) else {
        return Vec::new();
    };
    let last = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .unwrap_or(first);
    lines[first..=last].to_vec()
}

pub fn format_comment(comment: &CommentPayload) -> String {
    let mut output = String::new();
    match &comment.source {
        CommentSource::Code { path, rows } => {
            output.push_str(&code_location(path, rows));
            output.push('\n');
        }
        CommentSource::Quote { label, text } => {
            output.push_str(&format!("From {label}:\n"));
            for line in trimmed_lines(text) {
                if line.is_empty() {
                    output.push_str(">\n");
                } else {
                    output.push_str(&format!("> {line}\n"));
                }
            }
        }
    }
    output.push_str(comment.body.trim());
    output
}

pub fn format_comments(comments: &[CommentPayload]) -> String {
    comments
        .iter()
        .map(format_comment)
        .collect::<Vec<_>>()
        .join("\n\n")
}

struct CommentAddedToast;

/// Adds a new comment to the visible agent session's store.
fn add_comment(
    buffer: Entity<Buffer>,
    range: Range<language::Anchor>,
    body: String,
    workspace: &WeakEntity<Workspace>,
    cx: &mut App,
) {
    let active_store = ActiveCommentStore::global(cx);
    let Some(store) = active_store.read(cx).store() else {
        return;
    };
    let thread_title = active_store.read(cx).thread_title();
    store.update(cx, |store, cx| store.add(buffer, range, body, cx));
    let message = if thread_title.is_empty() {
        "Comment added to agent thread".to_string()
    } else {
        format!("Comment added to {thread_title}")
    };
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<CommentAddedToast>(), message).autohide(),
                cx,
            );
        })
        .log_err();
}

fn apply_edit_event(
    event: &CommentInputEvent,
    store: &WeakEntity<AgentCommentStore>,
    comment_id: AgentCommentId,
    cx: &mut App,
) {
    match event {
        CommentInputEvent::Submitted(body) => {
            store
                .update(cx, |store, cx| store.update(comment_id, body.clone(), cx))
                .log_err();
        }
        CommentInputEvent::Removed => {
            store
                .update(cx, |store, cx| store.remove(comment_id, cx))
                .log_err();
        }
        CommentInputEvent::Cancelled => {}
    }
}

struct AgentCommentGutter;

fn refresh_editor_comment_highlights(editor: &mut Editor, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let buffer_ids = snapshot.all_buffer_ids().collect::<HashSet<_>>();
    let ranges = match active_comment_store(cx) {
        Some(store) => store
            .read(cx)
            .comments()
            .iter()
            .filter(|comment| buffer_ids.contains(&comment.buffer.read(cx).remote_id()))
            .filter_map(|comment| snapshot.anchor_range_in_buffer(comment.range.clone()))
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    if ranges.is_empty() {
        editor.clear_gutter_highlights::<AgentCommentGutter>(cx);
        editor.clear_background_highlights(HighlightKey::AgentComment, cx);
        return;
    }
    editor.highlight_gutter::<AgentCommentGutter>(
        ranges.clone(),
        |cx| cx.theme().status().info,
        cx,
    );
    editor.highlight_background(
        HighlightKey::AgentComment,
        &ranges,
        |_, theme| theme.status().info_background,
        cx,
    );
}

/// Comments go to the visible agent session, so without one there is nowhere
/// to send them and the icons are hidden.
fn refresh_editor_comment_buttons(
    editor: &mut Editor,
    show_selection_button: bool,
    cx: &mut Context<Editor>,
) {
    let has_active_store = active_comment_store(cx).is_some();
    editor.set_show_selection_comment_button(show_selection_button && has_active_store, cx);
    let in_comment = has_active_store && comment_at_cursor(editor, cx).is_some();
    editor.set_show_cursor_comment_button(in_comment, cx);
}

/// The first comment, in document order, overlapping the newest selection.
fn comment_at_cursor(
    editor: &Editor,
    cx: &mut App,
) -> Option<(Entity<AgentCommentStore>, BufferSnapshot, AgentComment)> {
    let store = active_comment_store(cx)?;
    let multi_buffer_snapshot = editor.buffer().read(cx).snapshot(cx);
    let (snapshot, range) = multi_buffer_snapshot
        .anchor_range_to_buffer_anchor_range(editor.selections.newest_anchor().range())?;
    let range = range.start.to_offset(snapshot)..range.end.to_offset(snapshot);
    let comment = store
        .read(cx)
        .comments_at(snapshot, range, cx)
        .into_iter()
        .next()?;
    Some((store, snapshot.clone(), comment))
}

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _window, cx| {
        if !editor.mode().is_full() || editor.project().is_none() {
            return;
        }
        let open_input = OpenInputSlot::default();
        refresh_editor_comment_buttons(editor, true, cx);
        refresh_editor_comment_highlights(editor, cx);
        let active_store = ActiveCommentStore::global(cx);
        cx.observe(&active_store, {
            let open_input = open_input.clone();
            move |editor, _, cx| {
                close_input_if_comment_gone(&open_input, cx);
                refresh_editor_comment_buttons(editor, true, cx);
                refresh_editor_comment_highlights(editor, cx)
            }
        })
        .detach();
        let pending_selection_button: Rc<RefCell<Option<Task<()>>>> = Rc::default();
        cx.subscribe_self(move |editor, event: &EditorEvent, cx| match event {
            EditorEvent::SelectionsChanged { local: true } => {
                // Wait for the selection to settle, so brief accidental
                // selections don't flash the icon.
                refresh_editor_comment_buttons(editor, false, cx);
                let task = cx.spawn(async move |editor, cx| {
                    cx.background_executor()
                        .timer(CODE_ACTIONS_DEBOUNCE_TIMEOUT)
                        .await;
                    editor
                        .update(cx, |editor, cx| {
                            refresh_editor_comment_buttons(editor, true, cx)
                        })
                        .log_err();
                });
                pending_selection_button.replace(Some(task));
            }
            EditorEvent::BufferRangesUpdated { .. } => {
                refresh_editor_comment_highlights(editor, cx);
            }
            _ => {}
        })
        .detach();
        let editor_handle = cx.entity().downgrade();
        editor
            .register_action(move |_: &ToggleComment, window, cx| {
                editor_handle
                    .update(cx, |editor, cx| {
                        toggle_editor_comment(editor, &open_input, window, cx)
                    })
                    .log_err();
            })
            .detach();
    })
    .detach();
}

/// The comment input an editor shows, and the comment it edits.
struct OpenInput {
    input: Entity<CommentInput>,
    comment_id: Option<AgentCommentId>,
}

type OpenInputSlot = Rc<RefCell<Option<OpenInput>>>;

/// Whether an input has nothing left to submit to: no session is visible,
/// or the comment it edits is gone from the visible session.
fn is_input_stale(comment_id: Option<AgentCommentId>, cx: &mut App) -> bool {
    let Some(store) = active_comment_store(cx) else {
        return true;
    };
    comment_id.is_some_and(|comment_id| !store.read(cx).contains(comment_id))
}

/// Closes an input with nothing left to submit to, so its text isn't
/// silently dropped on submit.
fn close_input_if_comment_gone(open_input: &OpenInputSlot, cx: &mut App) {
    let input = {
        let open_input = open_input.borrow();
        let Some(OpenInput { input, comment_id }) = open_input.as_ref() else {
            return;
        };
        if !is_input_stale(*comment_id, cx) {
            return;
        }
        input.clone()
    };
    input.update(cx, |_, cx| cx.emit(CommentInputEvent::Cancelled));
}

/// Closes the open comment input, or else comments on the selection, or
/// else opens the comment under the cursor.
fn toggle_editor_comment(
    editor: &mut Editor,
    open_input: &OpenInputSlot,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let open = open_input.borrow_mut().take();
    if let Some(open) = open {
        open.input
            .update(cx, |_, cx| cx.emit(CommentInputEvent::Cancelled));
        return;
    }
    if active_comment_store(cx).is_none() {
        return;
    }
    let display_snapshot = editor.display_snapshot(cx);
    let selection = editor.selections.newest::<BufferPoint>(&display_snapshot);
    // Comments can't overlap, so a selection touching one edits it instead.
    if selection.is_empty() || comment_at_cursor(editor, cx).is_some() {
        edit_comment_at_cursor(editor, open_input, window, cx);
        return;
    }
    let multi_buffer_snapshot = display_snapshot.buffer_snapshot();
    let Some((buffer_snapshot, range)) = multi_buffer_snapshot
        .anchor_range_to_buffer_anchor_range(editor.selections.newest_anchor().range())
    else {
        return;
    };
    let Some(buffer) = editor.buffer().read(cx).buffer(buffer_snapshot.remote_id()) else {
        return;
    };
    let offset_range = range.start.to_offset(buffer_snapshot)..range.end.to_offset(buffer_snapshot);
    let range = comment_anchor_range(buffer_snapshot, offset_range.clone());
    let Some(source) = code_source_for_buffer_range(buffer_snapshot, offset_range, cx) else {
        log::warn!("agent comment: selection is not in a file");
        return;
    };
    let Some(workspace) = editor.workspace().map(|workspace| workspace.downgrade()) else {
        return;
    };
    let Some(block_anchor) = block_anchor_after_line(editor, range.end, cx) else {
        return;
    };
    let input = cx.new(|cx| CommentInput::new(source.title(), window, cx));
    show_input_block(
        editor,
        block_anchor,
        OpenInput {
            input,
            comment_id: None,
        },
        open_input,
        window,
        cx,
        move |event, cx| {
            if let CommentInputEvent::Submitted(body) = event {
                add_comment(buffer.clone(), range.clone(), body.clone(), &workspace, cx);
            }
        },
    );
}

/// The end of the line containing `anchor`, so an input block appears below
/// the whole line.
fn block_anchor_after_line(
    editor: &Editor,
    anchor: language::Anchor,
    cx: &App,
) -> Option<editor::Anchor> {
    let multi_buffer_snapshot = editor.buffer().read(cx).snapshot(cx);
    let end = multi_buffer_snapshot.anchor_in_buffer(anchor)?;
    let end_point = end.to_point(&multi_buffer_snapshot);
    let mut end_row = end_point.row;
    // A range ending at the start of a line doesn't include that line.
    if end_point.column == 0 && end_row > 0 {
        end_row -= 1;
    }
    Some(multi_buffer_snapshot.anchor_after(BufferPoint::new(
        end_row,
        multi_buffer_snapshot.line_len(MultiBufferRow(end_row)),
    )))
}

fn edit_comment_at_cursor(
    editor: &mut Editor,
    open_input: &OpenInputSlot,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some((store, snapshot, comment)) = comment_at_cursor(editor, cx) else {
        return;
    };
    let Some(source) = code_source_for_buffer_range(&snapshot, comment.offset_range(&snapshot), cx)
    else {
        return;
    };
    let Some(block_anchor) = block_anchor_after_line(editor, comment.range.end, cx) else {
        return;
    };
    let store = store.downgrade();
    let comment_id = comment.id;
    let input = cx.new(|cx| CommentInput::editing(source.title(), &comment.body, window, cx));
    show_input_block(
        editor,
        block_anchor,
        OpenInput {
            input,
            comment_id: Some(comment_id),
        },
        open_input,
        window,
        cx,
        move |event, cx| apply_edit_event(event, &store, comment_id, cx),
    );
}

fn input_block_height(input: &Entity<CommentInput>, cx: &App) -> u32 {
    INPUT_BLOCK_CHROME_LINES + input.read(cx).visible_lines() as u32
}

/// Shows the input in a block below `block_anchor` until it's submitted,
/// removed or cancelled.
fn show_input_block(
    editor: &mut Editor,
    block_anchor: editor::Anchor,
    open: OpenInput,
    open_input: &OpenInputSlot,
    window: &mut Window,
    cx: &mut Context<Editor>,
    on_event: impl Fn(&CommentInputEvent, &mut App) + 'static,
) {
    let previous = open_input.borrow_mut().take();
    if let Some(previous) = previous {
        previous
            .input
            .update(cx, |_, cx| cx.emit(CommentInputEvent::Cancelled));
    }

    let input = open.input.clone();
    let block = BlockProperties {
        style: BlockStyle::Sticky,
        placement: BlockPlacement::Below(block_anchor),
        height: Some(input_block_height(&input, cx)),
        render: Arc::new({
            let input = input.clone();
            move |cx: &mut BlockContext| {
                div()
                    .pl(cx.margins.gutter.full_width())
                    .py_1()
                    .child(input.clone())
                    .into_any_element()
            }
        }),
        priority: 0,
    };
    let Some(block_id) = editor
        .insert_blocks([block], Some(Autoscroll::fit()), cx)
        .into_iter()
        .next()
    else {
        return;
    };

    *open_input.borrow_mut() = Some(open);
    let resize_subscription = cx.observe(&input, move |editor, input, cx| {
        resize_input_block(editor, block_id, &input, cx)
    });
    let open_input = open_input.clone();
    let resize_subscription = RefCell::new(Some(resize_subscription));
    cx.subscribe_in(&input, window, move |editor, _, event, window, cx| {
        resize_subscription.borrow_mut().take();
        open_input.borrow_mut().take();
        editor.remove_blocks(HashSet::from_iter([block_id]), None, cx);
        on_event(event, cx);
        window.focus(&editor.focus_handle(cx), cx);
    })
    .detach();

    window.focus(&input.focus_handle(cx), cx);
}

fn resize_input_block(
    editor: &mut Editor,
    block_id: CustomBlockId,
    input: &Entity<CommentInput>,
    cx: &mut Context<Editor>,
) {
    let height = input_block_height(input, cx);
    editor.resize_blocks(HashMap::from_iter([(block_id, height)]), None, cx);
}

pub enum CommentInputEvent {
    Submitted(String),
    Removed,
    Cancelled,
}

pub struct CommentInput {
    title: SharedString,
    editor: Entity<Editor>,
    /// Whether this edits an existing comment rather than adding one.
    is_existing: bool,
    visible_lines: usize,
    _editor_subscription: Subscription,
}

impl CommentInput {
    pub fn new(title: SharedString, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(INPUT_MIN_LINES, INPUT_MAX_LINES, window, cx);
            editor.set_placeholder_text("Add a comment for the agent…", window, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor
        });
        // The editor comes first when tabbing through the input's controls.
        editor.focus_handle(cx).tab_index(0).tab_stop(true);
        // Wrapping is only known after layout, so watch the editor rather
        // than its edits, and notify only when the height changes.
        let editor_subscription = cx.observe(&editor, |this: &mut Self, editor, cx| {
            let visible_lines = Self::wrapped_lines(&editor, cx);
            if visible_lines != this.visible_lines {
                this.visible_lines = visible_lines;
                cx.notify();
            }
        });
        Self {
            title,
            editor,
            is_existing: false,
            visible_lines: INPUT_MIN_LINES,
            _editor_subscription: editor_subscription,
        }
    }

    pub fn editing(
        title: SharedString,
        body: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(title, window, cx);
        this.editor
            .update(cx, |editor, cx| editor.set_text(body, window, cx));
        this.is_existing = true;
        this
    }

    /// Lines of text the input shows, within its minimum and maximum height.
    /// Beyond the maximum the input scrolls.
    fn visible_lines(&self) -> usize {
        self.visible_lines
    }

    fn wrapped_lines(editor: &Entity<Editor>, cx: &mut App) -> usize {
        let snapshot = editor.update(cx, |editor, cx| editor.display_snapshot(cx));
        let rows = snapshot.max_point().row().0 as usize + 1;
        rows.clamp(INPUT_MIN_LINES, INPUT_MAX_LINES)
    }

    fn submit(&mut self, _: &Submit, _window: &mut Window, cx: &mut Context<Self>) {
        let body = self.editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        cx.emit(CommentInputEvent::Submitted(body));
    }

    fn cancel(
        &mut self,
        _: &editor::actions::Cancel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.emit(CommentInputEvent::Cancelled);
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn focus_previous(&mut self, _: &FocusPrevious, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
    }
}

impl EventEmitter<CommentInputEvent> for CommentInput {}

impl Focusable for CommentInput {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for CommentInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let focus_handle = self.editor.focus_handle(cx);
        v_flex()
            .id("agent-comment-input")
            .key_context("AgentCommentInput")
            .aria_label(self.title.clone())
            .tab_group()
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_previous))
            .w(rems(28.))
            .p_2()
            .gap_2()
            .bg(colors.elevated_surface_background)
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .shadow_md()
            .child(
                Label::new(self.title.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .when(self.is_existing, |this| {
                        this.child(
                            Button::new("remove", "Remove")
                                .label_size(LabelSize::Small)
                                .tab_index(1isize)
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(CommentInputEvent::Removed);
                                })),
                        )
                        .child(div().flex_1())
                    })
                    .child(
                        Button::new("cancel", "Cancel")
                            .label_size(LabelSize::Small)
                            .tab_index(2isize)
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(CommentInputEvent::Cancelled);
                            })),
                    )
                    .child(
                        Button::new(
                            "submit",
                            if self.is_existing {
                                "Update Comment"
                            } else {
                                "Add Comment"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .tab_index(3isize)
                        .key_binding(KeyBinding::for_action_in(&Submit, &focus_handle, cx))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.submit(&Submit, window, cx);
                        })),
                    ),
            )
    }
}

/// A comment input floating over a view that has no inline block support,
/// such as the markdown preview.
pub struct CommentPopover {
    input: Entity<CommentInput>,
    position: Point<Pixels>,
    comment_id: Option<AgentCommentId>,
    _subscription: Subscription,
}

impl CommentPopover {
    /// Starts a new comment on `range` of `buffer`. Without a buffer, the
    /// comment can't be kept, so the popover isn't shown. `on_close` runs
    /// after the input is submitted or cancelled; the view should drop the
    /// popover there.
    pub fn new<V: 'static>(
        source: CommentSource,
        buffer: Entity<Buffer>,
        range: Range<language::Anchor>,
        position: Point<Pixels>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<V>,
        on_close: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let input = cx.new(|cx| CommentInput::new(source.title(), window, cx));
        let subscription = cx.subscribe_in(&input, window, move |view, _, event, window, cx| {
            if let CommentInputEvent::Submitted(body) = event {
                add_comment(buffer.clone(), range.clone(), body.clone(), &workspace, cx);
            }
            on_close(view, window, cx);
        });
        Self::with_input(input, None, position, subscription, window, cx)
    }

    /// Opens the first comment, in document order, overlapping `range` in
    /// `buffer` for editing, if there is one.
    pub fn edit_existing<V: 'static>(
        buffer: &BufferSnapshot,
        range: Range<usize>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<V>,
        on_close: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Option<Self> {
        let store = active_comment_store(cx)?;
        let comment = store
            .read(cx)
            .comments_at(buffer, range, cx)
            .into_iter()
            .next()?;
        let source = code_source_for_buffer_range(buffer, comment.offset_range(buffer), cx)?;
        let store = store.downgrade();
        let comment_id = comment.id;
        let input = cx.new(|cx| CommentInput::editing(source.title(), &comment.body, window, cx));
        let subscription = cx.subscribe_in(&input, window, move |view, _, event, window, cx| {
            apply_edit_event(event, &store, comment_id, cx);
            on_close(view, window, cx);
        });
        Some(Self::with_input(
            input,
            Some(comment_id),
            position,
            subscription,
            window,
            cx,
        ))
    }

    fn with_input<V: 'static>(
        input: Entity<CommentInput>,
        comment_id: Option<AgentCommentId>,
        position: Point<Pixels>,
        subscription: Subscription,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Self {
        // Deferred so the focus change made by the mouse down that opened
        // the popover doesn't steal focus back from the input.
        let focus_handle = input.focus_handle(cx);
        window.defer(cx, move |window, cx| window.focus(&focus_handle, cx));
        Self {
            input,
            position,
            comment_id,
            _subscription: subscription,
        }
    }

    /// Whether the popover edits a comment that is no longer in the visible
    /// session, so the view should close it.
    pub fn is_stale(&self, cx: &mut App) -> bool {
        is_input_stale(self.comment_id, cx)
    }

    pub fn render(&self) -> impl IntoElement {
        deferred(
            anchored()
                .position(self.position)
                .anchor(Anchor::TopLeft)
                .snap_to_window_with_margin(px(8.))
                .child(div().occlude().child(self.input.clone())),
        )
        .with_priority(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn code(rows: RangeInclusive<u32>, body: &str) -> CommentPayload {
        CommentPayload {
            source: CommentSource::Code {
                path: "src/foo.rs".into(),
                rows,
            },
            body: body.to_string(),
        }
    }

    fn quote(text: &str, body: &str) -> CommentPayload {
        CommentPayload {
            source: CommentSource::Quote {
                label: "markdown preview".into(),
                text: text.to_string(),
            },
            body: body.to_string(),
        }
    }

    #[test]
    fn test_format_code_comment() {
        assert_eq!(
            format_comment(&code(11..=11, "Rename this.")),
            "`src/foo.rs:12`\nRename this."
        );
        assert_eq!(
            format_comment(&code(11..=17, "  Split this up.\n")),
            "`src/foo.rs:12-18`\nSplit this up."
        );
    }

    #[test]
    fn test_format_code_comment_with_spaces_in_path() {
        let comment = CommentPayload {
            source: CommentSource::Code {
                path: "docs/my notes/read me.md".into(),
                rows: 2..=3,
            },
            body: "Typo.".to_string(),
        };
        assert_eq!(
            format_comment(&comment),
            "`docs/my notes/read me.md:3-4`\nTypo."
        );
    }

    #[test]
    fn test_format_quote_comment() {
        assert_eq!(
            format_comment(&quote(
                "\n\nerror[E0308]: mismatched types   \n\n  expected u32\n\n",
                "Fix this."
            )),
            "From markdown preview:\n> error[E0308]: mismatched types\n>\n>   expected u32\nFix this."
        );
        assert_eq!(
            format_comment(&quote("   \n", "Empty.")),
            "From markdown preview:\nEmpty."
        );
    }

    #[test]
    fn test_format_comments() {
        assert_eq!(
            format_comments(&[code(0..=0, "One."), quote("text", "Two.")]),
            "`src/foo.rs:1`\nOne.\n\nFrom markdown preview:\n> text\nTwo."
        );
        assert_eq!(format_comments(&[]), "");
    }

    fn new_store(cx: &mut TestAppContext) -> (Entity<AgentCommentStore>, Rc<RefCell<usize>>) {
        let store = cx.new(|_| AgentCommentStore::default());
        let changes = Rc::new(RefCell::new(0));
        cx.update(|cx| {
            let changes = changes.clone();
            cx.subscribe(&store, move |_, _: &AgentCommentStoreEvent, _| {
                *changes.borrow_mut() += 1;
            })
            .detach();
        });
        (store, changes)
    }

    fn add(
        store: &Entity<AgentCommentStore>,
        buffer: &Entity<Buffer>,
        range: Range<usize>,
        body: &str,
        cx: &mut TestAppContext,
    ) -> AgentCommentId {
        store.update(cx, |store, cx| {
            let snapshot = buffer.read(cx).snapshot();
            let range = comment_anchor_range(&snapshot, range);
            store.add(buffer.clone(), range, body.to_string(), cx)
        })
    }

    fn ids_at(
        store: &Entity<AgentCommentStore>,
        buffer: &Entity<Buffer>,
        range: Range<usize>,
        cx: &mut TestAppContext,
    ) -> Vec<AgentCommentId> {
        cx.update(|cx| {
            store
                .read(cx)
                .comments_at(&buffer.read(cx).snapshot(), range, cx)
                .into_iter()
                .map(|comment| comment.id)
                .collect()
        })
    }

    #[gpui::test]
    fn test_store_add_update_remove(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two three", cx));
        let (store, changes) = new_store(cx);

        let id = add(&store, &buffer, 4..7, "Rename.", cx);
        assert_eq!(*changes.borrow(), 1);
        store.read_with(cx, |store, _| {
            assert_eq!(store.len(), 1);
            assert_eq!(store.comments()[0].body, "Rename.");
        });

        store.update(cx, |store, cx| store.update(id, "Remove.".to_string(), cx));
        assert_eq!(*changes.borrow(), 2);
        store.read_with(cx, |store, _| {
            assert_eq!(store.comments()[0].body, "Remove.")
        });

        store.update(cx, |store, cx| store.remove(id, cx));
        assert_eq!(*changes.borrow(), 3);
        store.read_with(cx, |store, _| assert!(store.is_empty()));

        store.update(cx, |store, cx| store.remove(id, cx));
        assert_eq!(
            *changes.borrow(),
            3,
            "removing a missing comment is a no-op"
        );
    }

    #[gpui::test]
    fn test_store_drain_and_clear(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two three", cx));
        let (store, changes) = new_store(cx);
        add(&store, &buffer, 0..3, "First.", cx);
        add(&store, &buffer, 4..7, "Second.", cx);

        let drained = store.update(cx, |store, cx| store.drain(cx));
        assert_eq!(
            drained
                .iter()
                .map(|comment| comment.body.as_str())
                .collect::<Vec<_>>(),
            ["First.", "Second."]
        );
        assert_eq!(*changes.borrow(), 3);
        store.read_with(cx, |store, _| assert!(store.is_empty()));

        store.update(cx, |store, cx| store.clear(cx));
        assert_eq!(*changes.borrow(), 3, "clearing an empty store is a no-op");
    }

    #[gpui::test]
    fn test_store_comments_at(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two three four", cx));
        let (store, _) = new_store(cx);
        let two = add(&store, &buffer, 4..7, "Two.", cx);

        assert_eq!(ids_at(&store, &buffer, 4..4, cx), [two], "cursor at start");
        assert_eq!(ids_at(&store, &buffer, 5..5, cx), [two], "cursor inside");
        assert_eq!(ids_at(&store, &buffer, 7..7, cx), [two], "cursor at end");
        assert!(
            ids_at(&store, &buffer, 3..3, cx).is_empty(),
            "cursor before"
        );
        assert!(ids_at(&store, &buffer, 8..8, cx).is_empty(), "cursor after");
        assert_eq!(ids_at(&store, &buffer, 6..10, cx), [two], "partial overlap");
        assert!(
            ids_at(&store, &buffer, 7..10, cx).is_empty(),
            "a selection touching the end doesn't overlap"
        );
        assert!(
            ids_at(&store, &buffer, 0..4, cx).is_empty(),
            "a selection touching the start doesn't overlap"
        );
    }

    #[gpui::test]
    fn test_store_comments_at_uses_document_order(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two three four", cx));
        let (store, _) = new_store(cx);
        let three = add(&store, &buffer, 8..13, "Three.", cx);
        let one = add(&store, &buffer, 0..3, "One.", cx);

        assert_eq!(ids_at(&store, &buffer, 0..18, cx), [one, three]);
    }

    #[gpui::test]
    fn test_store_offset_ranges_follow_edits(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two three", cx));
        let (store, _) = new_store(cx);
        add(&store, &buffer, 8..13, "Three.", cx);
        add(&store, &buffer, 0..3, "One.", cx);

        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "zero ")], None, cx));
        let ranges = cx.update(|cx| {
            store
                .read(cx)
                .offset_ranges(&buffer.read(cx).snapshot(), cx)
        });
        assert_eq!(ranges, [5..8, 13..18]);

        buffer.update(cx, |buffer, cx| buffer.edit([(8..8, "!")], None, cx));
        let ranges = cx.update(|cx| {
            store
                .read(cx)
                .offset_ranges(&buffer.read(cx).snapshot(), cx)
        });
        assert_eq!(
            ranges,
            [5..8, 14..19],
            "text typed at an edge isn't included"
        );
    }

    #[gpui::test]
    fn test_store_drops_comment_when_text_is_deleted(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two three", cx));
        let (store, changes) = new_store(cx);
        add(&store, &buffer, 4..7, "Two.", cx);
        add(&store, &buffer, 8..13, "Three.", cx);

        buffer.update(cx, |buffer, cx| buffer.edit([(5..6, "")], None, cx));
        store.read_with(cx, |store, _| assert_eq!(store.len(), 2));

        buffer.update(cx, |buffer, cx| buffer.edit([(3..7, "")], None, cx));
        store.read_with(cx, |store, _| {
            assert_eq!(store.len(), 1);
            assert_eq!(store.comments()[0].body, "Three.");
        });
        assert_eq!(*changes.borrow(), 3);
    }

    #[gpui::test]
    fn test_store_pending_comments(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one\ntwo\nthree\n", cx));
        let (store, _) = new_store(cx);
        add(&store, &buffer, 4..14, "Lines two and three.", cx);
        add(&store, &buffer, 0..4, "Ends at column zero.", cx);

        let text = cx.update(|cx| {
            format_comments(
                &store
                    .read(cx)
                    .pending_comments(|_, _| Some("src/a.rs".into()), cx),
            )
        });
        assert_eq!(
            text,
            "`src/a.rs:2-3`\nLines two and three.\n\n`src/a.rs:1`\nEnds at column zero."
        );

        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "zero\n")], None, cx));
        let text = cx.update(|cx| {
            format_comments(
                &store
                    .read(cx)
                    .pending_comments(|_, _| Some("src/a.rs".into()), cx),
            )
        });
        assert_eq!(
            text, "`src/a.rs:3-4`\nLines two and three.\n\n`src/a.rs:2`\nEnds at column zero.",
            "line numbers are read when the payload is built"
        );
    }

    #[gpui::test]
    fn test_store_take_pending_comments_keeps_unnamed(cx: &mut TestAppContext) {
        let named = cx.new(|cx| Buffer::local("named", cx));
        let unnamed = cx.new(|cx| Buffer::local("unnamed", cx));
        let (store, _) = new_store(cx);
        add(&store, &named, 0..5, "Kept in prompt.", cx);
        add(&store, &unnamed, 0..7, "Kept in store.", cx);

        let named_id = named.read_with(cx, |buffer, _| buffer.remote_id());
        let payloads = store.update(cx, |store, cx| {
            store.take_pending_comments(
                |buffer, _| (buffer.remote_id() == named_id).then(|| "a.rs".into()),
                cx,
            )
        });
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].body, "Kept in prompt.");
        store.read_with(cx, |store, _| {
            assert_eq!(store.len(), 1);
            assert_eq!(store.comments()[0].body, "Kept in store.");
        });
    }

    #[gpui::test]
    fn test_active_store_notifies_on_changes(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two", cx));
        let first = cx.new(|_| AgentCommentStore::default());
        let second = cx.new(|_| AgentCommentStore::default());
        let active_store = cx.update(ActiveCommentStore::global);
        let notifications = Rc::new(RefCell::new(0));
        cx.update(|cx| {
            let notifications = notifications.clone();
            cx.observe(&active_store, move |_, _| *notifications.borrow_mut() += 1)
                .detach();
        });

        active_store.update(cx, |active, cx| {
            active.set(Some(&first), "First".into(), cx)
        });
        assert_eq!(*notifications.borrow(), 1);
        active_store.update(cx, |active, cx| {
            active.set(Some(&first), "Renamed".into(), cx)
        });
        assert_eq!(*notifications.borrow(), 1, "same store doesn't notify");
        assert_eq!(
            active_store.read_with(cx, |active, _| active.thread_title()),
            "Renamed"
        );

        add(&first, &buffer, 0..3, "One.", cx);
        assert_eq!(*notifications.borrow(), 2);
        add(&second, &buffer, 4..7, "Two.", cx);
        assert_eq!(*notifications.borrow(), 2, "other stores are ignored");

        active_store.update(cx, |active, cx| {
            active.set(Some(&second), "Second".into(), cx)
        });
        assert_eq!(*notifications.borrow(), 3);
        add(&first, &buffer, 4..7, "Ignored.", cx);
        assert_eq!(*notifications.borrow(), 3);

        drop(second);
        cx.run_until_parked();
        assert!(
            cx.update(active_comment_store).is_none(),
            "the slot is weak"
        );
    }

    mod editor_tests {
        use super::*;
        use editor::SelectionEffects;
        use fs::FakeFs;
        use gpui::VisualTestContext;
        use project::Project;
        use serde_json::json;
        use std::path::PathBuf;
        use util::path;
        use workspace::{AppState, OpenOptions};

        async fn open_editor<'a>(
            text: &str,
            cx: &'a mut TestAppContext,
        ) -> (Entity<Editor>, &'a mut VisualTestContext) {
            cx.update(|cx| {
                cx.set_global(db::AppDatabase::test_new());
                AppState::test(cx);
                editor::init(cx);
                init(cx);
            });
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(path!("/project"), json!({"main.rs": text}))
                .await;
            let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
            let (workspace, cx) =
                cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        PathBuf::from(path!("/project/main.rs")),
                        OpenOptions::default(),
                        window,
                        cx,
                    )
                })
                .await
                .expect("file should open");
            let editor = item
                .downcast::<Editor>()
                .expect("file should open in an editor");
            cx.run_until_parked();
            (editor, cx)
        }

        fn activate_store(cx: &mut VisualTestContext) -> Entity<AgentCommentStore> {
            let store = cx.new(|_| AgentCommentStore::default());
            let active_store = cx.update(|_, cx| ActiveCommentStore::global(cx));
            active_store.update(cx, |active, cx| {
                active.set(Some(&store), "Thread".into(), cx)
            });
            cx.run_until_parked();
            store
        }

        fn select(editor: &Entity<Editor>, range: Range<usize>, cx: &mut VisualTestContext) {
            editor.update_in(cx, |editor, window, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let start = snapshot.offset_to_point(editor::MultiBufferOffset(range.start));
                let end = snapshot.offset_to_point(editor::MultiBufferOffset(range.end));
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([start..end])
                });
            });
        }

        fn buttons(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (bool, bool) {
            editor.read_with(cx, |editor, _| {
                (
                    editor.show_selection_comment_button(),
                    editor.show_cursor_comment_button(),
                )
            })
        }

        fn editor_focused(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
            editor.update_in(cx, |editor, window, cx| {
                editor.focus_handle(cx).is_focused(window)
            })
        }

        fn bodies(store: &Entity<AgentCommentStore>, cx: &mut VisualTestContext) -> Vec<String> {
            store.read_with(cx, |store, _| {
                store
                    .comments()
                    .iter()
                    .map(|comment| comment.body.clone())
                    .collect()
            })
        }

        fn highlight_count(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> usize {
            editor.update_in(cx, |editor, window, cx| {
                editor.all_text_background_highlights(window, cx).len()
            })
        }

        #[gpui::test]
        async fn test_buttons_follow_selection_and_active_store(cx: &mut TestAppContext) {
            let (editor, cx) = open_editor("one two three\n", cx).await;

            select(&editor, 4..7, cx);
            cx.executor().advance_clock(CODE_ACTIONS_DEBOUNCE_TIMEOUT);
            cx.run_until_parked();
            assert_eq!(buttons(&editor, cx), (false, false), "no active session");

            let store = activate_store(cx);
            assert_eq!(buttons(&editor, cx), (true, false));

            select(&editor, 0..3, cx);
            assert_eq!(
                buttons(&editor, cx),
                (false, false),
                "hidden while settling"
            );
            cx.executor().advance_clock(CODE_ACTIONS_DEBOUNCE_TIMEOUT);
            cx.run_until_parked();
            assert_eq!(buttons(&editor, cx), (true, false));

            editor.update(cx, |editor, cx| {
                let buffer = editor.buffer().read(cx).as_singleton().expect("singleton");
                let snapshot = buffer.read(cx).snapshot();
                let range = comment_anchor_range(&snapshot, 4..7);
                store.update(cx, |store, cx| {
                    store.add(buffer, range, "Two.".to_string(), cx)
                });
            });
            select(&editor, 5..5, cx);
            cx.run_until_parked();
            assert!(buttons(&editor, cx).1, "cursor inside a comment");
            select(&editor, 9..9, cx);
            cx.run_until_parked();
            assert!(!buttons(&editor, cx).1, "cursor outside a comment");

            let active_store = cx.update(|_, cx| ActiveCommentStore::global(cx));
            active_store.update(cx, |active, cx| {
                active.set(None, SharedString::default(), cx)
            });
            select(&editor, 5..5, cx);
            cx.executor().advance_clock(CODE_ACTIONS_DEBOUNCE_TIMEOUT);
            cx.run_until_parked();
            assert_eq!(buttons(&editor, cx), (false, false));
        }

        #[gpui::test]
        async fn test_toggle_comment(cx: &mut TestAppContext) {
            let (editor, cx) = open_editor("one two three\nfour\n", cx).await;

            select(&editor, 4..7, cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            assert!(editor_focused(&editor, cx), "no active session");

            let store = activate_store(cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            assert!(!editor_focused(&editor, cx), "the input takes focus");
            cx.dispatch_action(Submit);
            cx.run_until_parked();
            assert!(bodies(&store, cx).is_empty(), "empty comments aren't added");
            cx.simulate_input("Rename.");
            cx.dispatch_action(Submit);
            cx.run_until_parked();
            assert!(editor_focused(&editor, cx), "focus returns to the editor");
            assert_eq!(bodies(&store, cx), ["Rename."]);
            assert_eq!(highlight_count(&editor, cx), 1);
            let comment = store.read_with(cx, |store, cx| {
                let comment = &store.comments()[0];
                let snapshot = comment.buffer.read(cx).snapshot();
                snapshot
                    .text_for_range(comment.offset_range(&snapshot))
                    .collect::<String>()
            });
            assert_eq!(comment, "two");

            // An open input closes on toggle.
            select(&editor, 5..5, cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            assert!(!editor_focused(&editor, cx), "the cursor's comment opens");
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            assert!(editor_focused(&editor, cx));

            // A selection overlapping a comment edits it.
            select(&editor, 6..16, cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            cx.simulate_input("!");
            cx.dispatch_action(Submit);
            cx.run_until_parked();
            let comment_bodies = bodies(&store, cx);
            assert_eq!(comment_bodies.len(), 1, "no overlapping comment is added");
            assert_ne!(comment_bodies[0], "Rename.");

            // A selection touching the comment's edge gets its own comment.
            select(&editor, 7..13, cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            cx.simulate_input("Three.");
            cx.dispatch_action(Submit);
            cx.run_until_parked();
            assert_eq!(bodies(&store, cx).len(), 2);
            assert_eq!(highlight_count(&editor, cx), 2);

            // A cursor outside any comment does nothing.
            select(&editor, 16..16, cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            assert!(editor_focused(&editor, cx));

            store.update(cx, |store, cx| store.clear(cx));
            cx.run_until_parked();
            assert_eq!(highlight_count(&editor, cx), 0);
        }

        #[gpui::test]
        async fn test_input_wraps_long_lines(cx: &mut TestAppContext) {
            cx.update(|cx| {
                cx.set_global(db::AppDatabase::test_new());
                AppState::test(cx);
                editor::init(cx);
            });
            let (input, cx) =
                cx.add_window_view(|window, cx| CommentInput::new("Comment".into(), window, cx));
            let input_editor = input.read_with(cx, |input, _| input.editor.clone());
            input_editor.update_in(cx, |editor, window, cx| {
                window.focus(&editor.focus_handle(cx), cx)
            });
            cx.simulate_input("word ");
            cx.run_until_parked();
            assert_eq!(input.read_with(cx, |input, _| input.visible_lines()), 2);

            cx.simulate_input(&"word ".repeat(30));
            cx.run_until_parked();
            let (buffer_rows, visible_lines) = input.read_with(cx, |input, cx| {
                let buffer_rows = input_editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .read(cx)
                    .max_point()
                    .row
                    + 1;
                (buffer_rows, input.visible_lines())
            });
            assert_eq!(buffer_rows, 1);
            assert!(
                visible_lines > 2,
                "a wrapped line grows the input, got {visible_lines}"
            );

            cx.simulate_input(&"word ".repeat(200));
            cx.run_until_parked();
            assert_eq!(
                input.read_with(cx, |input, _| input.visible_lines()),
                INPUT_MAX_LINES,
                "longer text scrolls inside the input"
            );
        }

        #[gpui::test]
        async fn test_input_closes_when_its_comment_is_deleted(cx: &mut TestAppContext) {
            let (editor, cx) = open_editor("one two three\n", cx).await;
            let store = activate_store(cx);
            editor.update(cx, |editor, cx| {
                let buffer = editor.buffer().read(cx).as_singleton().expect("singleton");
                let snapshot = buffer.read(cx).snapshot();
                let range = comment_anchor_range(&snapshot, 4..7);
                store.update(cx, |store, cx| {
                    store.add(buffer, range, "Two.".to_string(), cx)
                });
            });
            select(&editor, 5..5, cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            assert!(!editor_focused(&editor, cx));

            editor.update(cx, |editor, cx| {
                let buffer = editor.buffer().read(cx).as_singleton().expect("singleton");
                buffer.update(cx, |buffer, cx| buffer.edit([(3..8, " ")], None, cx));
            });
            cx.run_until_parked();
            assert!(bodies(&store, cx).is_empty());
            assert!(editor_focused(&editor, cx), "the input closed");
        }

        #[gpui::test]
        async fn test_new_input_closes_without_active_store(cx: &mut TestAppContext) {
            let (editor, cx) = open_editor("one two three\n", cx).await;
            let _store = activate_store(cx);
            select(&editor, 4..7, cx);
            cx.dispatch_action(ToggleComment);
            cx.run_until_parked();
            assert!(!editor_focused(&editor, cx));

            let active_store = cx.update(|_, cx| ActiveCommentStore::global(cx));
            active_store.update(cx, |active, cx| active.set(None, "".into(), cx));
            cx.run_until_parked();
            assert!(editor_focused(&editor, cx), "the input closed");
        }
    }
}
