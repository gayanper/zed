# Example prompts

## Agent comments in Terminal Threads

1. Testing comment highlighting

model: opus
effort: low

```
Do not use any tools, read no files, and do not add commentary. Reply with exactly the text between the markers below, nothing before or after. Keep each paragraph and bullet as one continuous line of text with no manual line breaks; the terminal will wrap it.

=====BEGIN=====
The reviewer approved the change in round 2 with no open findings. Agent comments still work in the code editor, the agent panel and the terminal: every agent_comments item they use still has callers. Clippy is clean, all 33 agent_comments tests pass and all 36 markdown_preview tests pass, including all four close_and_return_to_editor_* tests.

- Spec line 17 is wrong. The agent panel and terminal don't use the markdown crate's on_selection_action or selection_action_in_range_highlights. Those two, plus set_range_highlights and cursor_offset, now have no callers outside the markdown crate. Spec line 26 puts the markdown crate out of scope, so I left them; removing them could be a follow-up change.

- Removed AgentCommentStore::offset_ranges and its test test_store_offset_ranges_follow_edits; the preview was the only real caller (should-fix).
- Removed an empty impl MarkdownPreviewView {} block (nit).

I changed the lookup in the file crates/terminal_view/src/terminal_view.rs and then the call to Store::offset_ranges as well. The ca-respawn follow-up still cleans up stopped sessions, and the Cargo.toml edit is unchanged from what others' branches expect, which keeps the merge simple.

1. Move the parser into its own crate so it can be tested alone and reused by the other tools in the workspace.
2. Add tests.

Here is a code block, a listing and a log, which must each stay as separate lines:

```rust
let first_value = compute_value_from_input(argument_one, argument_two);
let second_value = compute_other_value_from_input(argument_three);
```

drwxr-xr-x  5 user staff 160 crates
drwxr-xr-x  5 user staff 160 assets

2026-11-10T10:00:01 INFO started the background indexing worker ok
2026-11-10T10:00:02 INFO listening on the local socket for requests

## A heading on its own line
Text under the heading that is long enough to wrap onto a second row in a narrow terminal window.
=====END=====
```
