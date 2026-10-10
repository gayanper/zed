# Agent Comments: Specification

Status: implemented (2026-09-30). Scope and UX were approved in the proof of concept; Phases 2–4 and the production gaps below are built and tested. See "Implementation notes" at the end for where production differs from the text.
Branch: `agent-comments`

## Summary

Users can leave comments for the AI agent on text in three places:
- a **source code editor**;
- the **markdown preview**;
- an **agent terminal thread** (a terminal session in the agent panel).

Commented text stays tinted in the editor and the preview, and terminal comments get a gutter marker, so the user can see what they've already commented on and reopen a comment to edit or remove it. The comments collect in the workspace they were written in. A button on the agent toolbar shows how many are pending, and clicking it inserts them into the prompt as plain text.

This feature is separate from the diff review comments (`stored_review_comments`, `AddDiffReviewComment` in `crates/editor/src/git.rs`). It shares no storage, actions or UI with them.

## Scope

**In scope**
- Adding, viewing, editing and removing comments in full-mode project editors.
- The same, in the markdown preview.
- Adding, editing and removing comments on text in agent panel terminal threads (see Agent terminal threads).
- Collecting comments per workspace and inserting them into the visible session's prompt (Phases 2–4).
- The `agent.enable_comments` setting, off by default.

**Out of scope**
- Commenting from terminals outside the agent panel (terminal panel, center pane) and from terminals embedded in thread tool calls.
- Changes to diff review comments.
- Keeping comments across restarts.
- Sending comments automatically, or opening a new thread for them.
- Comments in collab participants' views.
- Comments in other markdown views, such as agent panel messages, hover popovers and the channel notes renderer.
- Showing that a file has comments in a thread other than the visible one.

---

## UX (approved)

Everything in this section was built and approved in the proof of concept. Production must keep this behaviour.

### Visual language

| Element | Look | Theme token |
|---|---|---|
| Comment action icon | `IconButton`, `IconName::Chat`, `IconSize::Small`, square, `ButtonStyle::Filled` | button defaults |
| Icon tooltip | "Comment for Agent", with the `ToggleComment` keybinding in the editor | — |
| Tint on commented text | Background behind the exact commented range, in the editor and the preview | `agent_comment.background` (falls back to `info.background`) |
| Scrollbar marker (editor only) | Marker in the middle column of the vertical scrollbar spanning the commented rows; the left gutter is left to git diff hunks | `agent_comment` (falls back to `info`) |
| Comment input | Card, 28 rems wide, `elevated_surface_background`, 1px `border`, rounded, `shadow_md` | theme colors |

Themes and `theme_overrides` can set `agent_comment` (scrollbar marker in the editor, terminal gutter marker) and `agent_comment.background` (tint in the editor and the preview). Both are optional in `ThemeColors`; `agent_comments::agent_comment_color` and `agent_comment_background` fall back to `info` and `info.background` when a theme leaves them unset. A theme that sets `info` without `info.background` gets an opaque tint: `info.background` then keeps the base `StatusColors` value (opaque blue step 9), because no translucent background is derived from the theme's `info`. Switching themes, or editing `theme_overrides`, re-tints existing comments in the editor and the preview without reopening the file.

### The comment input

One component (`CommentInput`) is used everywhere. Top to bottom it contains:
1. A title label (small, muted, truncated): `Comment on `path:12-18``.
2. A multi-line editor: 2 lines tall, growing to at most 4, with the placeholder "Add a comment for the agent…". Long lines soft-wrap at the input's width, and wrapped rows count toward its height. Past 4 rows the input scrolls vertically.
3. A button row:
   - **New comment:** `Cancel` and `Add Comment`, right-aligned.
   - **Existing comment:** `Remove` on the left; `Cancel` and `Update Comment` on the right.
   - The primary button shows the `Submit` keybinding.

Keys inside the input:

| Key | Action |
|---|---|
| `cmd-enter` (macOS) / `ctrl-enter` (Linux, Windows) | `agent_comments::Submit` |
| `escape` | cancel |

Behaviour:
- Submitting an empty or whitespace-only body does nothing.
- Submit, remove and cancel all close the input and return focus to the view that opened it.
- The input takes focus when it opens.

### Source code editor

**Adding a comment**
1. The user selects text. When the mouse is released, the comment icon appears at the end of the selection's last line, one `em` after the last character (or after a fold's trailer), centred on the line.
2. The icon shows only when all of these hold:
   - the editor is focused;
   - no selection drag is pending;
   - the newest selection is not empty;
   - the selection hasn't changed for `CODE_ACTIONS_DEBOUNCE_TIMEOUT` (250 ms, the delay before the code actions indicator appears). Every selection change hides the icon and restarts the delay, so brief accidental selections don't show it. The cursor-in-comment icon has no delay.
3. Clicking the icon, or pressing `ctrl-alt-m`, opens `CommentInput` as a block below the selection's last line, indented past the gutter. The editor scrolls so the block is visible.
4. On submit:
   - the comment is added;
   - the range gets its tint and scrollbar marker;
   - Phase 1 only: a notification shows the payload.

**Viewing and editing a comment**
1. When the cursor sits inside a tinted range with nothing selected, the same icon appears at the end of the cursor's line. The input doesn't open by itself when the cursor lands there.
2. Clicking the icon, or pressing `ctrl-alt-m`, opens the input below the comment's last line, filled with the existing text, with `Remove`, `Cancel` and `Update Comment`.
3. `Update Comment` replaces the text. `Remove` deletes the comment and its tint and scrollbar marker.

**`ctrl-alt-m` toggles.** Evaluated in this order:
1. An input is open in this editor: close it (same as cancel).
2. Text is selected and it overlaps an existing comment: open that comment for editing (see No overlapping comments).
3. Text is selected: open the input for a new comment.
4. The cursor is inside a commented range: open that comment for editing.
5. Otherwise: nothing.

The icon's click follows the same rules.

**No overlapping comments.** Comment ranges never overlap.
- A selection that shares at least one character with an existing comment's range opens that comment for editing instead of starting a new one. A selection that only touches a comment's edge doesn't overlap it, so text right next to a comment can get its own comment.
- A cursor with nothing selected matches a comment it is inside or touching at either edge.
- If the selection overlaps several comments, open the first one in document order.
- The selection is left as it is; the comment's range doesn't grow to cover it. (Growing the range was considered and not built.)
- This applies to `ToggleComment`, the editor icon, and the preview icon alike. The editor icon shows for a selection that overlaps a comment, as for any selection.
- Validated in the proof of concept, including partial overlaps.

**Setting.** The feature is off unless `"agent": { "enable_comments": true }` is set (default `false`). It is also off when the agent is disabled (`agent.enabled: false` or `disable_ai`). While it is off, everything behaves as with no active agent thread: no icons, no tints, no terminal selection button or gutter markers, no toolbar button, and `ToggleComment` and modifier-click do nothing. Existing comments are kept and come back when it is turned on again. Changing the setting takes effect immediately.

**No active agent thread.** When the agent panel has no visible thread or terminal thread:
- the comment icon is hidden in the editor (for selections and for the cursor in a comment) and in the preview;
- `ToggleComment` and the preview's modifier-click do nothing, except closing an input that is already open;
- existing tints are hidden, since they belong to a thread (see Phase 2 decisions).

The icon comes back as soon as a thread becomes visible. Validated in the proof of concept.

**Tint and scrollbar marker**
- The tint covers the exact commented characters.
- The scrollbar marker covers every row the range touches. It sits in the scrollbar's middle column, with search, selected-text and symbol markers, which are painted on top of it (symbol markers are half transparent, so the comment color shows through them). Its color repeats the `agent_comment` → `info` fallback in `editor`'s `refresh_slow_scrollbar_markers`, because `editor` can't depend on `agent_comments`; keep the two in sync. Git diff markers keep the left column.
- Both follow the text as the file is edited, because ranges are stored as buffer anchors.
- The tint shows in every editor that displays the buffer, including split panes and multibuffers that contain an excerpt of it. The scrollbar marker shows only in single-buffer editors, like the other middle-column markers.
- The marker is controlled by `"scrollbar": { "agent_comments": true }` (default `true`, also in the settings UI under Scrollbar). With `show: "auto"`, comments make the scrollbar visible. Turning the marker off leaves only the tint.
- Comments don't draw in the left gutter, so they never cover git diff hunks.
- Ranges are sorted in multibuffer order before they're handed to the editor, whose highlight lookup is a binary search; comments are kept in the order they were added.

**Comments on removed lines**
- A selection whose both ends are in the deleted text of one diff hunk comments on the removed lines: in a diff with expanded hunks (project, commit and branch diffs, or hunks expanded in a file), and on the left side of a split diff.
- The comment is stored against the diff's base text buffer (`BufferDiff::base_text_buffer`: HEAD for working-tree diffs, the parent or merge base for commit and branch diffs), with `diff_base_of` naming the file's buffer. A working-tree diff's base text buffer is edited in place when HEAD changes, so a comment whose text leaves HEAD collapses and is removed like any other.
- Known limitation: the project holds diffs weakly. If every view of a diff closes and it is opened again, the new diff has a new base text buffer, and older removed-lines comments no longer tint or open for editing there. They are still sent to the agent and can be cleared from the agent panel.
- The tint shows on the deleted rows wherever the hunk is expanded, and on the left side of a split diff.
- The agent gets `From removed lines of `path`:` followed by the quoted removed text, since base text rows can't be opened in the file.
- Any other selection keeps the usual behavior: one that also covers current rows comments on the current-file part only, and one on unchanged rows of a split diff's left side adds nothing.

### Markdown preview

**Adding a comment**
1. The user selects rendered text. When the mouse is released, the icon appears just after the end of the selection, 4px to the right, centred on the line.
2. Clicking the icon opens `CommentInput` as a floating popover at the icon's position, kept inside the window with an 8px margin.
3. The selected text is mapped back to the markdown source and then to the file's lines. The comment is a **code comment** (`` `README.md:12-18` ``), exactly as if it had been made in the source editor.
4. If the preview has no file behind it, fall back to a quote comment (see Payload format).

**Viewing and editing a comment**
- **Click then icon:** clicking inside tinted text with nothing selected shows the icon at the click point. Clicking the icon opens the popover with the existing comment, and `Remove`, `Cancel` and `Update Comment`.
- **Modifier-click toggle:**
  - cmd-click (macOS) or ctrl-click (Linux, Windows) inside tinted text opens that comment directly.
  - Cmd/ctrl-clicking anywhere in the preview while the popover is open closes it.
  - Other modifiers are taken:
    - shift-click extends the selection;
    - alt-click on a link opens it externally.

**Mouse only.** Commenting in the preview needs the mouse. The preview has no text cursor to place the popover at, so `ToggleComment` isn't bound there.

**No icon delay.** The icon appears as soon as the mouse is released, without the editor's 250 ms delay. Nothing shows during a drag, so accidental selections rarely show it, and a delay would need a `markdown` crate change.

**Tint**
- The commented source range is tinted in the rendered output.
- Search highlights and the selection are drawn on top of it.
- Find keeps working alongside it.

### Agent terminal threads

Terminal text has no buffer, so terminal comments are kept apart from buffer comments and work differently.

**Adding a comment**
1. The user selects terminal text. In a TUI that captures the mouse, that takes shift-drag, or a plain drag while comment mode is on (see Comment mode).
2. When the selection is finished, the comment icon appears one cell after its last cell, centred on the line. Clicking it opens `CommentInput` as a floating popover at the icon, titled "Comment on terminal selection".
3. On submit, the comment is added with the selected text, captured as it is now; it is never re-read.

**Marker instead of a tint.** A thin bar in `status().info` marks the comment's rows in the terminal's left gutter. The text isn't tinted.

**Anchor.** A resize changes which rows show the text: lines rewrap, and TUIs redraw with other padding and at other rows. So a comment is found by its selected text with whitespace and line breaks left out, matched against the on-screen text with the same characters left out. The marker covers the lines from the first to the last matched character. When the text shows in more than one place, the place closest to the row the comment started on (`history size + grid line`) wins. When the text isn't on screen, for example because it was cleared or scrolled away, the marker hides. The comment is kept, counts on the toolbar and is inserted with its captured text; it shows again when its text comes back.

**Editing.** Selecting text on any row of a comment whose marker shows opens that comment for editing (the topmost if several), with `Remove`, `Cancel` and `Update Comment`. Overlap is judged by whole rows: the selected rows against the rows the marker covers. There is no modifier-click toggle (cmd/ctrl-click opens links in the terminal) and no keyboard binding: `ToggleComment` isn't bound in the terminal.

**Scope.** A terminal comment applies only to the terminal thread it was made in, because the text it quotes is only in that terminal. File comments apply to every thread, since they name their file and location. The toolbar's count, Insert and Clear cover the file comments plus the visible terminal thread's own comments; an agent thread or another terminal thread leaves them out, and the button shows only when that count is above 0. Clear in any thread still removes every file comment.

**Lifetime.** Closing or archiving the terminal thread drops its comments. Otherwise they are removed like other comments: by inserting them, by the toolbar's Clear, or by closing the workspace. A comment whose marker is hidden can't be edited or removed on its own.

**Comment mode.** A toolbar toggle (`IconName::CursorIBeam`, tooltip "Select text for comments"), shown for a terminal thread while comments are on, keeps mouse clicks, drags and moves local. A plain drag then selects text as shift-drag does, and several comments in a row don't need shift. Scrolling still goes to the program. While it is on, right-click opens Zed's terminal context menu, middle-click isn't sent to the program and the program gets no hover events. The state belongs to each terminal thread and survives switching threads. It is turned off by the toggle, by `ToggleTerminalCommentMode`, or by turning comments off.

**Popover.** The agent panel hosts the popover. It closes when the visible session changes, when its comment is gone, when comments are turned off, and on submit or cancel; focus returns to the terminal. Closing it from the input (submit, update, remove or cancel) also clears the terminal selection, so the comment icon goes away; the other closes keep the selection.

### Shared behaviour across the editor and the preview

- The preview shows exactly the file's buffer text, so preview source offsets equal buffer offsets.
- Both views read the same comments for a buffer. So:
  - A comment added in the preview is tinted, and can be edited, in the source editor, and vice versa.
  - Editing or removing it in one view updates the other immediately.

---

## Payload format

Every comment from the editor or the preview is a code comment:

```text
`crates/editor/src/element.rs:2049-2060`
This lays out the icon twice; can we share it?
```

Rules:
- **Path:** project-relative, shown with the file's path style.
- **Lines:** 1-based. A single line is `path:12`; a range is `path:12-18`.
- **Last line:** a range that ends at column 0 of a line leaves that line out.
- **Body:** the comment is trimmed, then placed on the line after the location.
- **Several comments:** joined with one blank line between them.
- **Line numbers are read when the payload is built** (Phase 4 insert), from the current anchor positions. Edits made after commenting are reflected, so they are not frozen at the time of commenting.
- **Terminal comments** are quoted without a label, from the text captured when the comment was made, with the quote trimming rules below:

```text
> quoted terminal line
> another line
Comment body
```

- Terminal and code comments are inserted together, in the order they were added.
- **Quote fallback,** only for a preview with no file behind it:

```text
From markdown preview:
> quoted source line
>
> another line
Comment body
```

- In a quote, trailing whitespace on each line and blank lines at either end are trimmed. An empty inner line becomes `>`.
- The quote uses the raw markdown source, not the rendered text.

---

## Architecture

### Crates and dependency direction

- `agent_comments` (new crate, root `src/agent_comments.rs`) holds:
  - the data model and store;
  - `CommentInput` and `CommentPopover`;
  - payload formatting;
  - all editor wiring.
- `markdown_preview` depends on `agent_comments`.
- `editor` and `markdown` must **not** depend on `agent_comments`. It depends on `editor`, and the rule keeps the upstream crates free of this feature. They only expose small, generic hooks (see Changes to upstream crates).
- `agent_comments::init(cx)` is called from `crates/zed/src/main.rs`. It:
  - turns the feature on for each new editor through `cx.observe_new::<Editor>`, skipping editors that aren't full mode or have no project;
  - enables the icon, registers the `ToggleComment` handler, starts highlighting, and watches cursor moves.

### Data model

```rust
pub struct AgentComment {
    pub id: AgentCommentId,
    pub buffer: Entity<Buffer>,          // strong: keeps the buffer alive (option A)
    pub range: Range<language::Anchor>,  // exact commented text
    pub body: String,
}
```

- **Anchors:** ranges are anchors in the file's `Buffer`, so they track edits.
  - Start with `anchor_after` and end with `anchor_before` (`comment_anchor_range`), so text typed at either edge doesn't join the range.
  - A range whose text is fully deleted collapses to empty. The comment is dropped from its store automatically, and any open input for it closes.
- **The source is derived, not stored.** The path, lines and quote are computed from `(buffer, range)` when needed:
  - the input title;
  - the payload.
- **Buffer lifetime:** see Phase 2, Comment and buffer lifetime.

### Store (proof of concept vs production)

- **Proof of concept:** `CommentRegions`, a global in-memory `HashMap<BufferId, Vec<RegionComment>>` with `add`, `update`, `remove`, `comments`, `comments_at(snapshot, range)` and `offset_ranges(snapshot)`. It notifies observers when anything changes.
  - Thread scoping: `set_active_thread(Option<EntityId>)` parks the current thread's comments and restores the new thread's. The key is the `ConversationView` or terminal view entity id. `has_active_thread()` drives the no-thread rule.
  - The agent panel reports the visible thread from `render_agent_comments_button`, deferred because the store change notifies the panel mid-render.
- **Production:** comments belong to the agent session's store (Phase 2). Views still need a per-buffer lookup to draw tints. That lookup should be:
  - an index over the active session's store, or
  - a global registry of all sessions' stores, filtered by buffer.

  Only the visible session's comments are tinted (Phase 2, decision 2).

### Actions and keybindings

| Action | Where defined | Purpose |
|---|---|---|
| `agent_comments::ToggleComment` | `zed_actions::agent_comments` | Toggle for the icon and the keyboard (see Source code editor). It is defined in `zed_actions` so `editor` can dispatch it without depending on `agent_comments`. |
| `agent_comments::Submit` | `agent_comments` | Submits `CommentInput`. |
| `agent_comments::ToggleTerminalCommentMode` | `agent_comments` | Toggles comment mode in the visible terminal thread. Handled by the agent panel. It has no default binding; a user binding belongs in `AgentPanel > Terminal`. |

Default keymap entries, in all three platform files:

```json
{ "context": "Editor && mode == full", "bindings": { "ctrl-alt-m": "agent_comments::ToggleComment" } },
{ "context": "AgentCommentInput > Editor", "bindings": { "cmd-enter": "agent_comments::Submit" } }
```

The input also binds `tab` / `shift-tab` to `agent_comments::FocusNext` / `FocusPrevious` (in `AgentCommentInput` and `AgentCommentInput > Editor`), so its buttons can be reached from the keyboard. On Linux and Windows the submit binding uses `ctrl-enter`. `ctrl-alt-m` is unused in the default keymaps. Check it against the vim/helix and JetBrains/VS Code keymap presets before release.

### Editor wiring (in `agent_comments`)

- **Icon visibility:**
  - `set_show_selection_comment_button(true)` when the editor is set up.
  - Both are recomputed on store change and on `EditorEvent::SelectionsChanged { local: true }`.
  - `set_show_selection_comment_button` is true while a thread is active.
  - `set_show_cursor_comment_button` is true while a thread is active and the newest selection overlaps a comment (see No overlapping comments).
- **Highlights:** on store change, and when the editor is set up:
  - collect the comment ranges of every buffer in the editor's multibuffer and turn them into multibuffer anchors (`anchor_range_in_buffer`);
  - then call `highlight_background(HighlightKey::AgentComment, …)`, or clear it when there are none. The editor builds the scrollbar markers from this highlight in `refresh_slow_scrollbar_markers`.
  - Production must also refresh when excerpts are added to a multibuffer (`EditorEvent::ExcerptsAdded`); the proof of concept doesn't.
- **Input block:**
  - `insert_blocks` with `BlockStyle::Sticky`, `BlockPlacement::Below(end of the last commented line)`, `Autoscroll::fit()`.
  - Each editor tracks its one open input, which `ToggleComment` closes.
  - Only one input may be open per editor; opening another first closes the current one.

### Preview wiring (in `markdown_preview`)

- `render_markdown_element` turns on `on_selection_action` and `selection_action_in_range_highlights()` while no popover is open and a thread is active.
- The callback receives `(selected_range, selected_source, position)`:
  - if the range overlaps a comment, edit that comment;
  - else, if the range is empty, do nothing;
  - otherwise, open a new comment.
- A mouse-up on the preview's root checks `event.modifiers.secondary()`. With the modifier held it toggles: close an open popover, or else, when a thread is active, open the comment at `Markdown::cursor_offset()`.
- The preview re-renders on store change, so the icon follows the active thread.
- `refresh_agent_comment_highlights` maps the active buffer's comment ranges to offsets and calls `Markdown::set_range_highlights`. It runs:
  - after every `Markdown::reset`, because the source is re-parsed;
  - on store change;
  - when the preview switches to another editor.
- The popover is `CommentPopover` (`deferred(anchored())`, priority 1, occluding). It focuses the input after a `window.defer`, so the mouse-down that opened it doesn't take focus back.

---

## Changes to upstream crates

We want as few changes to core crates as possible, so upstream changes merge easily. Keep every hook generic, with no mention of agents or comments, and off by default. This list is the complete set allowed; anything more needs a spec change.

### `editor` (about 120 lines, low to medium merge risk)

| Change | File | Notes |
|---|---|---|
| `show_selection_comment_button` / `show_cursor_comment_button` fields | `editor.rs` | 2 bools, default false |
| `set_show_selection_comment_button`, `set_show_cursor_comment_button` | `config.rs` | setters |
| `HighlightKey::AgentComment` | `display_map.rs` | one enum variant |
| `layout_selection_comment_button`, `paint_selection_comment_button`, `EditorLayout.selection_comment_button` | `element.rs` (~L2118, ~L6696) | laid out in the newest-selection-head block next to inline blame; painted after the inline code actions. **Highest merge risk:** `element.rs` changes often. |

The editor's button dispatches `zed_actions::agent_comments::ToggleComment`. The button relies on `ButtonLike` preventing the default so a click doesn't clear the selection. Keep a test for that.

### `markdown` (about 140 lines, low to medium merge risk)

| Change | Notes |
|---|---|
| `on_selection_action(Fn(Range<usize>, SharedString, Point<Pixels>, &mut Window, &mut App))` | Opt-in builder. Lays out and paints a button after the selection end. The text and range are captured during prepaint, because clicking outside the text clears the selection before the handler runs. The button uses `on_mouse_down` and `occlude`. |
| `selection_action_in_range_highlights()` | Opt-in. Also shows the button when the selection is empty and the cursor is inside a range highlight. |
| `set_range_highlights(Vec<(Range<usize>, Hsla)>)` | Background highlights by source range. Merged in `MarkdownHighlights::highlights_for_line` before search and selection. Unlike search highlights, `reset` does not clear them. |
| `cursor_offset() -> Option<usize>` | Read-only getter. |
| `RenderedMarkdown.selection_action_button` | Laid out in prepaint, painted after the text. |

### `theme`, `settings_content`, `theme_settings` (about 30 lines, low merge risk)

| Change | File | Notes |
|---|---|---|
| `agent_comment`, `agent_comment_background: Option<Hsla>` | `crates/theme/src/styles/colors.rs` | optional, like `editor_code_lens_foreground` |
| `None` defaults | `crates/theme/src/{default_colors,fallback_themes}.rs` | |
| `agent_comment`, `agent_comment.background` keys | `crates/settings_content/src/theme.rs` | |
| Parse-only refinement | `crates/theme_settings/src/schema.rs` | no fallback; `agent_comments` resolves it at use time |
| Override test | `crates/theme_settings/src/settings.rs` | `agent_comment_colors_from_theme_survive_info_override` |

### `terminal_view` (about 165 lines, low to medium merge risk)

| Change | Notes |
|---|---|
| `TerminalView::on_selection_action(tooltip, Fn(Range, String, Point<Pixels>, &mut Window, &mut App))` / `clear_selection_action` | Opt-in. `TerminalElement` lays out an icon button one cell after a finished, non-empty selection, and paints it after the text. The range and text are captured in prepaint. The button uses `on_mouse_down` and `occlude`, so the click keeps the selection. |
| `TerminalView::set_local_selection(bool)` / `is_local_selection` | Opt-in, off by default. When on, mouse clicks, drags and moves use `MouseInputMode::LocalSelection`, as read-only views do. Scrolling keeps `ReportToTerminal` unless the view is read-only. |
| `TerminalView::set_gutter_markers(Fn(&Content, &App) -> Vec<(RangeInclusive<i32>, Hsla)>)` / `clear_gutter_markers` | Opt-in. Called on every paint with the content being painted; the ranges of grid lines get a 2px bar in the existing one-cell left gutter. |

The on-screen text and its grid lines come from the public `Content`.

### `terminal` (about 4 lines, low merge risk)

| Change | Notes |
|---|---|
| `Terminal::clear_selection` | Public wrapper over the private `set_selection(None)`. The agent panel calls it when the terminal comment popover closes from its input. The only public alternative, `copy(Some(false))`, overwrites the clipboard. |

### `settings_content`, `agent_settings` (about 10 lines, low merge risk)

- `AgentSettingsContent::enable_comments: Option<bool>` and `AgentSettings::enable_comments: bool`, next to `enable_feedback`.
- `assets/settings/default.json`: `"enable_comments": false`.
- Test fixtures that build `AgentSettings` literally (`agent_ui.rs`, `agent/src/tool_permissions.rs`) set it to `false`.

### `zed_actions`

- An `agent_comments` module with `ToggleComment`.

### Other small changes

- `zed`: call `agent_comments::init`.
- Keymaps: the two entries above.
- Workspace `Cargo.toml`: add the member and dependency.

### Upstreaming

The `markdown` hooks and the editor highlight key could be proposed upstream as generic features on their own ("selection action", "range highlights"). That would remove most of our merge risk. It is optional and not required for release.

---

## Production gaps found in the proof of concept

Each one must be closed before release.

**Correctness**
1. The input block's height is fixed at `EDITOR_BLOCK_HEIGHT = 8` lines. Measure it, or recompute it when the editor's height changes, so short comments don't leave empty space and long ones don't clip.
2. A selection overlapping several comments opens the first one in the order they were added, not document order. Sort by range start.
3. `EditorEvent::ExcerptsAdded` doesn't refresh the highlights (see Editor wiring).
4. Cmd/ctrl-clicking a link inside tinted text in the preview both opens the link and opens the comment. Ignore the comment toggle when the click lands on a link.
5. A comment whose range collapses to empty (its text was deleted) is still in the store. It must be dropped automatically (see Data model).
6. Remote/collab buffers: check anchors and `BufferId` in shared projects. Allow commenting on your own view of a guest buffer, but never sync comments.
7. With several open previews and editors on one buffer, confirm only the view that opened an input shows it.
8. The active thread is only reported when the agent panel renders its toolbar. If the panel is closed at startup, no thread is active and the icon stays hidden until the panel is shown. Production must update the active-store slot on session change, not on render (see Phase 2).
9. Parked comments of closed or archived threads are never dropped, so their buffers stay alive (see Session end).

**Keyboard and accessibility**

10. Give `CommentInput` an accessible name and make sure its buttons can be reached with tab.

**Code quality**

11. Remove all terminal code (see Scope).
12. Add `enable_comments` to the settings UI (`settings_ui/src/page_data.rs`) next to `show_turn_stats`.
13. Remove `submit_comment`'s notification sink once Phase 3 routes comments to the store.
14. Replace the `CommentRegions` global with the Phase 2 store and its per-buffer index.
15. Run `./script/clippy` over every changed crate.

**Tests to add**

16. `format_comment` / `format_comments` already cover a single line, a range, quotes and joining. Add: a range ending at column 0; paths containing spaces.
17. Store tests: add, update, remove, `comments_at` at both ends and outside, and `offset_ranges` sorting after edits.
18. Editor GPUI tests:
    - the icon shows only when the selection is non-empty, or the cursor is in a region;
    - `ToggleComment` covers all four cases;
    - highlights are present after adding and gone after removing;
    - the selection survives a click on the icon.
19. Markdown tests: `range_highlights` survive `reset`; `selection_action_in_range_highlights` shows the button only inside a highlight.
20. Preview GPUI tests:
    - a comment added in the preview is tinted in the source editor, and the other way round;
    - preview comments produce `path:lines` payloads;
    - modifier-click toggles.

---

## Phase 2: The store, bound to the active agent session

**Goal:** build the production comment store, move the tint and edit UX onto it, and settle how a comment finds the active session. There are no UI changes beyond replacing the proof-of-concept store.

### Proposed design

- **`AgentCommentStore`** lives in `agent_comments` so it can be tested without `agent_ui`:
  - `comments: Vec<AgentComment>` and `next_id`;
  - `add`, `update(id, body)`, `remove(id)`, `clear`, `drain() -> Vec<AgentComment>`, `len`;
  - `comments_for_buffer(buffer_id)` and `comments_at(snapshot, offset)`;
  - emits `AgentCommentStoreEvent::Changed`.
- **Ownership:** one `Entity<AgentCommentStore>` per agent session:
  - Zed/ACP thread: held by `ConversationView` (`crates/agent_ui/src/conversation_view.rs`) and created with it.
  - Terminal thread: held by `AgentTerminal` (`crates/agent_ui/src/agent_panel.rs`).
- **Routing without an `agent_ui` dependency:**
  - `agent_comments` holds a global "active store" slot: `ActiveCommentStore(Option<WeakEntity<AgentCommentStore>>)`.
  - `agent_ui` updates the slot whenever the visible session changes.
  - The editor and preview wiring read and watch this slot, instead of the proof-of-concept global.
- **"Active"** means the agent panel's visible session, even when the panel isn't focused: `active_conversation_view()` if there is one, else the `AgentTerminal` for `active_terminal_id()`. The slot is `None` when neither exists, which hides the icon and the tints.

### Decisions

1. **No active session:** hide the comment icon, and make `ToggleComment` and the preview's modifier-click do nothing.
2. **Switching threads:** comments stay with the thread they were added to. Only the visible thread's comments are tinted, editable and counted on the toolbar; switching threads swaps the tints and the count. Inserting clears only the visible thread's comments. Validated in the proof of concept for Zed threads and terminal threads.
   - Nothing shows that a file has comments in another thread. Out of scope for the first release.
3. **Terminal threads receive comments.** An `AgentTerminal` owns a store like `ConversationView`, and Phase 4 pastes into it (see Phase 4). Comments still can't be made *from* a terminal.
4. **Closed files:** option A. Comments keep their file's buffer alive (see Comment and buffer lifetime).
5. **Deleted text:** a comment whose text is fully deleted is dropped automatically.
6. **Overlaps:** not supported. A selection that overlaps an existing comment opens that comment for editing (see No overlapping comments).

### Comment and buffer lifetime

- **Closed files (option A):** each comment holds a strong `Entity<Buffer>`, which keeps the file's buffer alive while any comment references it.
  - Closing every tab on the file is allowed and never prompts.
  - The comments stay in the thread's store and in the toolbar count.
  - Reopening the file gets the same buffer back from the project, so the tints return and stay editable.
  - Line numbers in the payload are always read from the live buffer, so they stay accurate.
- **Released when:**
  - the comment is removed;
  - the comment is dropped because its text was deleted;
  - the comment is sent (Phase 4 drains the store);
  - its thread's store is released (see Session end).
- **Session end:** when a thread (`ConversationView`) or terminal thread (`AgentTerminal`) is closed or archived, its store and every comment in it are dropped silently, with no confirmation. That releases every buffer they held and removes their tints.
  - The store is owned only by that session, so dropping the session drops the store. Nothing else may hold a strong `Entity<AgentCommentStore>`; the active-store slot holds a `WeakEntity`.
  - Test both close and archive, for threads and terminal threads, and check that the buffer is released afterwards: no strong handles remain once all editors for it are closed.
- **No persistence:** comments exist only in memory. Restarting Zed loses them, which is acceptable.

### Phase 2 acceptance

- A short written decision on each question, appended to this document.
- `AgentCommentStore` with unit tests for add, update, remove, drain, region lookups and `Changed` events.
- Tints and edit/remove work unchanged on the new store; the proof-of-concept global is gone.
- `ConversationView` owns a store, and `agent_ui` keeps the active-store slot current.

---

## Phase 3: Route comments into the active session's store

**Goal:** submitted comments go into the active session's store instead of a notification.

- The submit paths in the editor block and the preview popover call `store.add`, through the active-store slot.
- On success, show a brief toast: "Comment added for the agent".
- `format_comment` stays the one place that builds payloads.

### Phase 3 acceptance

- Comments from the editor and the preview land in the visible thread's store. Verify with a GPUI test in `agent_ui`.
- Switching the visible thread sends new comments to the new thread. Earlier ones stay where they were.
- The no-session case behaves as decided in Phase 2.
- No notification sink remains.

---

## Phase 4: Pending comments button on the agent toolbar

**Goal:** a toolbar button shows the pending count and, on click, inserts all pending comments into the prompt as text.

**Status:** UX approved from the proof of concept. The proof of concept reads the shared `CommentRegions` list. Production reads the visible session's `AgentCommentStore`.

### Placement

- In `AgentPanel::render_toolbar` (`crates/agent_ui/src/agent_panel.rs`), inside the right-hand button group.
- **Order in that group:** sandbox status, terminal comment mode toggle (terminal threads only, see Comment mode), **comments button**, new-thread "+" menu, full-screen toggle, options menu.
- It is one toolbar shared by normal threads and terminal threads, so the button appears in the same place for both.

### Visibility

The button shows only when both of these hold:
- the panel is showing a session (`visible_surface()` is `AgentThread` or `Terminal`, not `Uninitialized`);
- the visible session's store has at least one comment.

Otherwise it isn't rendered at all. With no comments, the toolbar layout is the same as upstream, except that a terminal thread shows the comment mode toggle while comments are on.

### Look

| Part | Value |
|---|---|
| Component | `Button` (not `IconButton`), id `agent-comments-insert` |
| Start icon | `IconName::Chat`, `IconSize::Small`: the same icon as the comment action |
| Label | the count, e.g. `3`, `LabelSize::Small` |
| Tooltip | "Insert 1 pending comment into prompt" / "Insert N pending comments into prompt" |
| Style | default button style, matching the neighbouring toolbar buttons |

### Live updates

- The panel watches the store and calls `cx.notify()` whenever it changes, so the count updates when comments are added, updated, removed or dropped.
  - Proof of concept: `_agent_comments_subscription`, a `cx.observe` on the shared `CommentRegions` entity.
  - Production: the visible session's `AgentCommentStore`. Re-subscribe when the visible session changes, e.g. in `refresh_base_view_subscriptions`.

### On click

1. Collect the visible session's comments in the order they were added.
2. Build the text with `format_comments`. Each comment's path and line numbers are computed at click time from its buffer and anchors, so edits made after commenting are reflected.
3. Insert the text into the visible session's input (see below). Nothing is submitted or run.
4. Clear the session's store. That hides the button, removes the tints and scrollbar markers in every editor and preview, and releases the buffers the comments were holding.
5. If the text comes out empty (for example, every comment's file has gone), insert nothing and keep the store unchanged.

**Normal thread**
- Target: the conversation's `active_thread()`, falling back to `root_thread_view()`, then its `message_editor`.
- `MessageEditor::insert_text(&text)` at the current cursor, then focus the message editor.
- Plain text only: no mention creases.
- Paths are project-relative, from the buffer's `File::path()` in the file's path style.

**Terminal thread**
- Target: the visible `TerminalView` of the agent terminal.
- `terminal.paste(&text)` uses bracketed paste, so a multi-line payload lands in the input without being run. The terminal sends newlines as `\r`. Then focus the terminal view.
- Never add a trailing newline, so the terminal agent doesn't treat it as Enter.
- Paths come from `mention_path_for_terminal` (already in `agent_panel.rs`):
  - relative to the terminal's working directory when the file is under it;
  - absolute otherwise.
  - The working directory is the terminal's live working directory, falling back to the directory the agent terminal was started in (`AgentTerminal.working_directory`).
  - The terminal agent's working directory may not be the project root, so project-relative paths could be wrong there.

**Formatting API.** `pending_comments(path_for, cx)` takes a path function `Fn(&Buffer, &App) -> Option<SharedString>`, so the same comments can be formatted for either target. A comment it returns `None` for is skipped.

### Other decisions

- **No prompt prefix.** The `agent-reviews` branch puts a configurable prefix (`review_comments_prompt_prefix`, default "Please address these review comments:") before diff review comments. Agent comments insert only the comments, because the user writes the instruction in the prompt themselves. A setting can be added later if wanted.
- **No confirmation** before inserting or clearing.
- **Clearing.** A close icon button next to the count, and the `agent_comments::ClearPendingComments` action, remove the visible session's pending comments without inserting them. This recovers from comments collected against the wrong thread or agent.
- **Out of scope:** a list of pending comments on the toolbar. Individual comments are edited or removed from the text they're attached to.

### Production gaps found in the proof of concept

1. The count and the insert use the proof-of-concept global, scoped to the visible thread by `set_active_thread`, instead of the visible session's store. Replaced by Phase 2.
2. A comment whose file can't be formatted is silently dropped from the text but still cleared from the store. Keep such comments instead (step 5 above covers the all-empty case only).
3. No keybinding or command-palette action inserts the comments. Consider an `agent_comments::InsertPendingComments` action registered on the panel, so the tooltip can show a keybinding.

### Phase 4 acceptance

- The count updates live as comments are added, updated or removed.
- Clicking inserts the exact `format_comments` text into the right input, empties the store, hides the button and clears the tints.
- The button shows the visible session's count only.
- GPUI tests cover inserting into a thread's message editor and the count returning to 0.
- A GPUI test pastes into an agent terminal thread. It checks the terminal's input log has the payload with no trailing `\r`, following `test_send_review_comments_pastes_into_active_terminal_thread` on `agent-reviews`.

---

## Files touched (overview)

| Area | Files |
|---|---|
| Feature crate | `crates/agent_comments/` (new) |
| Editor hooks | `crates/editor/src/{editor,config,display_map,element}.rs` |
| Markdown hooks | `crates/markdown/src/markdown.rs` |
| Preview wiring | `crates/markdown_preview/src/markdown_preview_view.rs`, `crates/markdown_preview/Cargo.toml` |
| Actions and init | `crates/zed_actions/src/lib.rs`, `crates/zed/src/main.rs`, `crates/zed/Cargo.toml`, root `Cargo.toml` |
| Setting | `crates/settings_content/src/agent.rs`, `crates/agent_settings/src/agent_settings.rs`, `assets/settings/default.json`, fixtures in `crates/agent_ui/src/agent_ui.rs` and `crates/agent/src/tool_permissions.rs`; settings UI in `crates/settings_ui/src/page_data.rs` (production) |
| Keymaps | `assets/keymaps/default-{macos,linux,windows}.json` |
| Agent integration (Phases 2–4) | `crates/agent_ui/src/{conversation_view,agent_panel,message_editor}.rs`, `crates/agent_ui/Cargo.toml` |
| Terminal hooks | `crates/terminal_view/src/{terminal_view,terminal_element}.rs` |
| Theme keys | `crates/theme/src/styles/colors.rs`, `crates/theme/src/{default_colors,fallback_themes}.rs`, `crates/settings_content/src/theme.rs`, `crates/theme_settings/src/schema.rs` |

---

## Implementation notes

- **Store (2026-10-05):** one `AgentCommentStore` per workspace, replacing the per-session stores and the app-wide `ActiveCommentStore` slot described in Phase 2. With one global slot, the panel that synced last owned it, so comments written in one workspace could land in another workspace's thread with no error. `AgentCommentStores` maps each workspace to its store, drops the store when the workspace is released, and notifies when any store changes or comments are turned on or off; editors and previews observe it and resolve the store from their own workspace. Comments are kept while no agent panel is open or no session is visible, and every thread and terminal thread in the workspace counts the same file comments. Terminal comments count only in their own terminal thread (2026-10-08); see Agent terminal threads. The "No active session" rules no longer apply: only the setting hides the icons and tints.
- **Comment ids** are unique across stores, so an open input can tell that its comment is gone (text deleted, or the visible session changed) and close itself.
- **Session end:** closing or archiving a thread or terminal thread no longer drops comments, since they belong to the workspace. Inserting or clearing them is the only way to remove them, other than deleting the commented text or closing the workspace.
- **Input block height:** 4 lines of chrome plus the input's wrapped row count (2 to 4), resized with `resize_blocks` when that count changes. The input watches its editor, since wrapping is only known after layout.
- **Preview link clicks:** the modifier-click toggle is ignored while a link is hovered.
- **Preview without a buffer:** no comment can be made (comments need a buffer). A buffer without a file uses the quote fallback.
- **`InsertPendingComments`** (`agent_comments::InsertPendingComments`) is handled by the agent panel and shown in the button's tooltip. It has no default binding.
- **Not covered by tests:** remote/collab buffers (gap 6); the one-input-per-view rule is structural (gap 7).
- **Terminal comments (2026-10-08):** `TerminalComment` lives in a second list of the workspace store (`add_terminal`, `terminal_comments`, `remove_terminal_comments`); `len`, `contains`, `update`, `remove` and `clear` cover both lists. The toolbar uses `visible_len`, `clear_visible`, `pending_comments` and `take_pending_comments`, which take the visible terminal (`None` for an agent thread) and leave out other terminals' comments; payloads are ordered by `AgentCommentId`. The wiring is in `agent_panel.rs` (`register_terminal_comment_hooks`, `open_terminal_comment`, `terminal_comment_markers`, `ScreenText`, `visible_comment_terminal`). Comments were first anchored to fixed scrollback rows and then to whole-row text; both hid the marker after a resize, since lines rewrap and TUIs redraw with other padding, so a comment is now found by its selected text without whitespace.
- **Terminal comment mode (2026-10-09):** `TerminalView::set_local_selection` reuses the existing `MouseInputMode::LocalSelection`, so comment mode needs no change in `terminal`. `scroll_wheel` picks its mode from `read_only` alone, which keeps the wheel going to the TUI. The panel wiring is `render_terminal_comment_mode_button` and `toggle_terminal_comment_mode` in `agent_panel.rs`; `register_terminal_comment_hooks` turns the mode off when comments are off.
- **Clearing the terminal selection (2026-10-09):** the `on_close` closure in `open_terminal_comment` clears the terminal's selection through `Terminal::clear_selection` before `close_terminal_comment`. `CommentPopover` doesn't pass the input event to `on_close`, so every close from the input clears it. The panel's `cx.notify()` redraws the terminal element, whose prepaint `sync` applies it.
