---
title: Terminal Threads - Zed
description: Run agent CLIs and TUIs directly in terminal-backed threads in Zed.
---

# Terminal Threads

Terminal Threads are terminal-backed threads in the [Threads Sidebar](./parallel-agents.md#threads-sidebar). Use them when you want to run an agent CLI or TUI directly in Zed.

Terminal Threads are different from [External Agents](./external-agents.md). External Agents integrate with Zed through ACP and render as agent threads. Terminal Threads run the native command-line tool in a terminal that Zed organizes as a thread.

## What Zed Owns {#what-zed-owns}

Zed owns the thread surface:

- the terminal-backed thread in the Threads Sidebar
- thread grouping by project
- switching and organizing the terminal session alongside other threads

## What the CLI Owns {#what-the-cli-owns}

The CLI or TUI running inside the terminal owns its own:

- authentication
- model/provider configuration
- subscriptions or API keys
- tool configuration
- skills and instruction files
- MCP configuration

[Zed Agent profiles](./agent-profiles.md), Zed Agent tool permissions, Zed Skills, and Zed Agent MCP settings do not automatically apply to Terminal Threads.

## Opening a Terminal Thread {#opening-a-terminal-thread}

Open the new-thread menu from the [Agent Panel](./agent-panel.md) using the agent selector button on the left or the `+` icon in the top-right of the panel toolbar, then choose **Terminal**. The Terminal Thread opens in the panel body, just like switching to an agent thread.

You can open as many Terminal Threads as you like. Each gets its own entry in the Threads Sidebar.

## Running a Command Automatically {#terminal-thread-init-command}

If you always run the same CLI in Terminal Threads, set the `agent.terminal_init_command` setting to have Zed run a command automatically whenever Zed creates a Terminal Thread shell:

```json [settings]
{
  "agent": {
    "terminal_init_command": "claude"
  }
}
```

The command is sent to the shell as if you had typed it, so it is interpreted by your configured shell—including on Windows and in remote or WSL projects—and the terminal remains a regular interactive shell after the command exits. It runs when creating a new Terminal Thread and when recreating a saved Terminal Thread after reopening a project.

To choose from multiple CLIs in the new-thread menu, configure named commands:

```json [settings]
{
  "agent": {
    "terminal_init_command": [
      { "name": "Claude", "command": "claude" },
      { "name": "Codex", "command": "codex" },
      { "name": "Reviewer", "command": "claude --agent reviewer", "kind": "sub" }
    ]
  }
}
```

Set `kind` to `main` (the default) for commands that start top-level Terminal Threads, or `sub` for commands offered under **New Terminal in Thread**. A plain `terminal_init_command` string is a `main` command. New Terminal in Thread options appear only when at least one `sub` command is configured.

In the Threads Sidebar, the project `+` menu lists each open workspace with its `main` commands, so you can choose which command starts the Terminal Thread. If a command list has no `main` entry, the menu says so, and a Terminal Thread started another way runs a plain shell.

Zed shows each name with the Terminal icon. It remembers the selected command for that Terminal Thread and uses the same command when it recreates the thread.

## Terminal Thread Titles {#terminal-thread-titles}

The terminal title in the toolbar updates automatically to reflect the running shell or process. You can set a custom name by clicking the title or the pencil icon that appears on hover. In the Threads Sidebar, right-click a Terminal Thread and select **Rename Title**, or select it and press {#kb agent::RenameSelectedThread}.

To edit the title of the active Terminal Thread from the Agent Panel, custom-map {#action agent::RenameSelectedThread} in your `keymap.json`. Its default binding is scoped to the Threads Sidebar.

## Notifications {#terminal-thread-notifications}

When a terminal produces a bell character while not in focus, Zed notifies you the same way it does when an agent finishes: with a visual pop-up and an optional sound. Clicking the notification brings the terminal into focus and clears the indicator.

The same `agent.notify_when_agent_waiting` and `agent.play_sound_when_agent_done` settings apply.

## Closing Terminal Threads {#closing-terminal-threads}

Unlike agent threads, Terminal Threads are closed rather than archived. They do not go to Thread History. To close one, hover over it in the Threads Sidebar and click the **×** button, or select it and press {#kb agent::ArchiveSelectedThread}.

## Terminals in a Terminal Thread {#terminal-tabs}

A terminal thread can hold several terminals. Add one from the agent panel's `+` menu under **New Terminal in Thread**. It opens as a new tab in the side that has focus, and takes focus.

With two or more terminals, the toolbar shows one tab per terminal in place of the title. Each tab shows the terminal's title, with the full title in its tooltip. Tabs shrink as space runs out, and the tab strip then scrolls sideways with the mouse wheel or trackpad. A dot on a tab means its terminal rang the bell; it clears when the terminal gets focus. Clicking the bell in the Threads Sidebar opens the terminal that rang most recently.

The tabs follow the `tabs.show_close_button` and `tabs.activate_on_close` settings. Right-click a tab to rename or close it. {#action agent::RenameSelectedThread} renames the thread instead, with the title editor shown in place of the tabs. Renaming a tab doesn't change the terminal thread's title, and renaming the thread doesn't change any tab. Without a custom name, the thread is titled after its oldest terminal's shell or process. When a terminal's shell exits, its tab closes. A terminal that fails to start shows an error, with buttons to retry or close it.

While a terminal has focus, the tab keys of the editor work on the tabs: {#kb pane::ActivateNextItem} and {#kb pane::ActivatePreviousItem} switch tabs, {#kb pane::ActivateLastItem} goes to the last tab, and {#kb pane::CloseActiveItem} closes the active tab. With one terminal, {#kb pane::CloseActiveItem} does nothing; it never closes the editor's active item.

Drag a tab to reorder it. The order is kept after restarting Zed.

## Split View {#terminal-split-view}

{#kb agent::ToggleTerminalSplit}, or the split button in the toolbar, shows a terminal thread's terminals side by side. The button needs at least two terminals. Turning split view on moves the most recently used other terminal to the right side. Each side has its own tabs. Turning it off moves the right side's tabs after the left side's.

Drag a tab onto the other side's tabs or terminal to move it there. With split view off, dropping a tab on the right half of the terminal opens split view with that tab on the right. When a side has no tabs left, split view turns off.

{#kb workspace::ActivatePaneLeft} and {#kb workspace::ActivatePaneRight} move focus between the sides, and {#action workspace::MoveItemToPaneInDirection} moves a tab to the other side. Drag the divider between the sides to resize them, and double-click it to make them equal. The split button shows a dot when a terminal whose tab isn't active rang.

Each terminal thread keeps its split view, active tabs, divider position and focused side when you switch threads and after restarting Zed.

## Commenting on Terminal Output {#terminal-comments}

Comments for the agent quote text from a terminal thread, and are added to the prompt with {#action agent_comments::InsertPendingComments}. A bar in the gutter marks the commented lines.

To comment on lines, highlight them in comment mode:

1. Press {#kb agent_comments::ToggleComment} in the terminal to start on the last line with text. You can also click or drag in the gutter left of the text, or choose Comment for Agent from the right-click menu to start on the clicked line. Hovering the gutter shows a glyph whose tooltip names the key. While the mode is on, the terminal has an accent border and a Commenting label at the top right, whose tooltip lists the keys.
2. A highlight covers whole lines; a line the terminal wrapped counts as one. {#kb terminal::MoveLineHighlightUp} and {#kb terminal::MoveLineHighlightDown} (or `k` and `j`) move it as one line. {#kb terminal::MoveLineHighlightEndUp} and {#kb terminal::MoveLineHighlightEndDown} (or `K` and `J`) move its active end, and {#kb terminal::MoveLineHighlightFarEndUp} and {#kb terminal::MoveLineHighlightFarEndDown} move the other end. {#kb terminal::ExtendLineHighlightByWord} extends the end word by word into the next line, and {#kb terminal::TrimLineHighlightByWord} trims words off it. With the mouse, click a line, drag over lines, Shift-click to extend, or drag the top or bottom edge.
3. Press {#kb terminal::ConfirmLineHighlight} or {#kb agent_comments::ToggleComment} to write a comment on the highlight. The quote leaves out agent decorations such as `⏺` and box borders. Once the comment is added or discarded, the mode carries on with the line after the commented ones highlighted.

{#kb terminal::ExitLineHighlight}, any other key, or moving focus away ends the mode, and any other key goes to the terminal as usual. While the mode is on, clicks and drags stay in Zed instead of going to the program; the gutter never goes to the program. After a resize, the highlight is found again by its text, or moves to the last line when its text is gone. Clicking a bar in the gutter opens the comments on its lines.

Selecting text stays for copying. To comment on a selection, press {#kb agent_comments::ToggleComment} with the text selected; the comment covers the selected lines whole. A selection or highlight touching one comment opens that comment for editing; touching several lists them by their first line, to choose the one to edit. Pressing it again closes an empty comment input; an input with text stays open.

`agent_comments::ToggleComment` is the same action that comments in editors. It's bound to `ctrl-alt-m` on macOS and `alt-shift-m` on Linux and Windows. While the agent shows an interactive prompt, such as a menu to choose from, it shows a message asking you to finish the prompt first.

> **Note:** `agent_comments::ToggleComment` replaces `agent_comments::CommentTerminalSelection`. If your keymap binds the old name, bind `agent_comments::ToggleComment` instead.

## CLI/TUI Setup Notes {#cli-setup}

Some agent CLIs and TUIs can send terminal signals, such as bell notifications or title updates, that Zed uses to show useful context in the sidebar.

### Claude Code Notifications {#claude-code-notifications}

Claude Code can notify you when it finishes a task or pauses for permission. To enable this, set `preferredNotifChannel` to `"terminal_bell"` in your Claude Code user settings:

```json
{
  "preferredNotifChannel": "terminal_bell"
}
```

You can also set this from within Claude Code by running `/config`, selecting `Local Notifications`, and choosing `Terminal Bell`.

> If you run Claude Code inside tmux, bell notifications may not reach the outer terminal unless passthrough is enabled. Add this to `~/.tmux.conf`:
>
> ```
> set -g allow-passthrough on
> ```

For more, see the [Claude Code documentation](https://code.claude.com/docs/en/terminal-config).

### Amp Notifications {#amp-notifications}

Amp updates terminal titles automatically and can also notify you when it needs your attention. To enable notifications in Zed Terminal Threads, add `AMP_FORCE_BEL=1` to your terminal environment settings:

```json [settings]
{
  "terminal": {
    "env": {
      "AMP_FORCE_BEL": "1"
    }
  }
}
```

Restart Amp after adding the environment variable.

### OpenCode Notifications {#opencode-notifications}

OpenCode can update terminal titles automatically. For Zed notifications, add an OpenCode plugin that emits a terminal bell when OpenCode needs your attention.

Create `.opencode/plugins/zed-bell.js` in your project, or `~/.config/opencode/plugins/zed-bell.js` to use it globally:

```js
export const ZedBell = async () => {
  return {
    event: async ({ event }) => {
      if (process.env.OPENCODE_CLIENT === "acp") return;

      if (event.type === "session.idle" || event.type === "permission.asked") {
        process.stdout.write("\x07");
      }
    },
  };
};
```

Restart OpenCode after adding the plugin.

### Pi Notifications {#pi-notifications}

Pi can use an extension to emit a notification when it finishes a turn. Create `.pi/extensions/zed-bell.ts` in your project, or `~/.pi/agent/extensions/zed-bell.ts` to use it globally:

```ts
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.on("agent_end", async () => {
    process.stdout.write("\x07");
  });
}
```

Restart Pi after adding the extension, or run `/reload` if the extension is in one of Pi's auto-discovered extension locations.

### Codex Terminal Titles {#codex-terminal-titles}

Codex can update the terminal title as it works, which Zed uses to show useful context for Codex Terminal Threads in the sidebar, such as the project, current status, branch, model, or task progress.

To configure this from within Codex, run `/title` and use the picker to choose which fields appear and in what order. Codex saves the selection to `tui.terminal_title` in `~/.codex/config.toml`. You can also edit it directly:

```toml
[tui]
terminal_title = ["spinner", "project-name", "run-state", "thread-title"]
```

## Credentials and Remote Projects {#credentials-and-remote-projects}

Credentials come from the terminal session and the CLI/TUI running inside it.

In remote projects, the CLI may read the remote shell environment and remote config files. In local Terminal Threads, it reads the local shell environment and local config files. Zed does not copy [API keys from LLM provider settings](./use-api-access.md) into Terminal Threads.

## When to Use Terminal Threads {#when-to-use-terminal-threads}

Use Terminal Threads when:

- you want the tool's native CLI/TUI experience
- no ACP integration exists
- you want subscription behavior owned by the CLI
- you want the CLI to use its own native config files

For ACP-integrated agents, see [External Agents](./external-agents.md).
