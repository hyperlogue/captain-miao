<p align="center">
  <img src="assets/logo.jpg" alt="captain-miao logo" width="220">
</p>

<h1 align="center">captain-miao</h1>

<p align="center"><b>Mission control for AI coding sessions in terminal.</b><br>
See the status of all sessions and jump right to the terminal tab that needs you.<br>
Support persistent sessions on remote servers. Close the lid and remote sessions
will keep working.</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@hyperlogue/captain-miao"><img src="https://img.shields.io/npm/v/@hyperlogue/captain-miao?color=cb3837&amp;logo=npm&amp;label=%40hyperlogue%2Fcaptain-miao" alt="npm version"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="license: MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="platforms: macOS, Linux">
</p>

<a href="assets/captain-miao.mp4"><img src="assets/dashboard.png" alt="captain-miao dashboard in Kitty with 15 active and waiting agent sessions"></a>

[Watch the 60-second demo](assets/captain-miao.mp4)

## Quick start

Install captain-miao and at least one [supported agent](#supported-agents):

```sh
npm install -g @hyperlogue/captain-miao
```

You can also run `npx @hyperlogue/captain-miao` after you configure your terminal.
[Prebuilt binaries](https://github.com/hyperlogue/captain-miao/releases) are available for macOS and Linux.

**1. Configure your terminal**

| Terminal                                   | Preparation                                                                   |
| ------------------------------------------ | ----------------------------------------------------------------------------- |
| [Kitty](https://sw.kovidgoyal.net/kitty/)  | Enable remote control as shown below. Then restart Kitty.                     |
| [zellij](https://zellij.dev/) ≥ 0.44       | Start inside a zellij session. Requires no extra configuration.               |
| [tmux](https://github.com/tmux/tmux) ≥ 3.2 | Start inside a tmux session. Requires no extra configuration.                 |
| [Ghostty](https://ghostty.org/) ≥ 1.3      | macOS: permit AppleScript Automation when prompted. No transcript preview.    |
| [iTerm2](https://iterm2.com/) ≥ 3.0        | macOS: permit AppleScript Automation when prompted. Plain-text previews only. |

For Kitty, add these lines to `~/.config/kitty/kitty.conf`:

```conf
allow_remote_control socket-only
listen_on unix:/tmp/captain-miao-kitty
```

These settings allow programs with socket access to control Kitty windows.
For more security, Kitty also supports [password authorization](https://sw.kovidgoyal.net/kitty/remote-control/).

**2. Start the dashboard**

```sh
cd /path/to/your/project
miao
```

Press `o` to start an agent in the directory of the selected session.
If you do not select a session, `o` opens the directory picker.
Press `O` to select a directory.
In the directory picker, press `Ctrl-o` to choose an agent and host side by side.
Use `qwertyuiop` for agents and `asdfgzxcvb` for the ten hosts on each page;
`[` and `]` turn host pages. The configured defaults start selected.
Press `Enter` to keep the choices and return to directory selection, `Ctrl-o`
to launch immediately in the previously highlighted directory, or `Esc` to
discard changes and return. Choices apply to this launch only. The selector
shows the launch directory and validates it on the chosen host before launching.
The existing `Ctrl-t` (cycle agent) and `Ctrl-h` (cycle host) shortcuts also work
in the directory picker.
Press `r` to resume a previous conversation.
Inside the resume picker, `Ctrl-o` opens the same agent and host selector.
The agent filter starts on **All** (`0`); individual agents use `qwertyuiop`.
It filters the loaded session list alongside the text search. `Enter` keeps
the filters and returns to the list, and `Esc` cancels selector changes.
`Ctrl-o` resumes the previously highlighted session when it still matches
the chosen host and agent; otherwise it applies the filters and returns to
the list so you can choose a session. Hosts use `asdfgzxcvb` and `[` / `]`
for paging, just as in the new-session selector.

Resume lists are cached in memory. Opening the picker or switching hosts shows
the last available result immediately and refreshes it in the background,
preserving your search and selection. A failed refresh keeps the cached sessions.
The server warms its cache once at startup; there is no timed refresh. Native
history readers reuse unchanged files, while CLI/API-only sources use their
normal listing operation. Updated servers keep entries per agent so the agent
filter applies before the visible session limit. Caches reset when their process
restarts.

The dashboard highlights sessions that require attention.
Press `s` to select the next highlighted session.
Press `Enter` to focus its window.
In Ghostty, start agents from the dashboard.

## Why captain-miao

- **Keep your terminal environment.** Sessions run in native windows, tabs, or panes.
  Your fonts, settings, and key bindings do not change.
- **No changes to global agent settings.** Managed sessions use isolated tracking hooks.
  The files `~/.claude/settings.json` and `~/.codex/hooks.json` remain unchanged.
  Integrations use separate files and profiles.
- **Persistent remote sessions.** `miao-server` runs agents in a [shpool](https://github.com/shell-pool/shpool) pty pool.
  Sessions continue when your computer sleeps or disconnects.
  Enable clipboard sharing to paste local images into remote agents with `Ctrl-V`.

## Supported agents

The ✓ mark indicates stable integrations in both support tables.
Other integrations have API limitations or less testing.

| Agent                                                        | Stable | Support notes                                                                                    |
| ------------------------------------------------------------ | :----: | ------------------------------------------------------------------------------------------------ |
| [Claude Code](https://claude.com/claude-code)                |   ✓    | Configures tracking hooks per session.                                                           |
| [Codex](https://github.com/openai/codex)                     |   ✓    | Uses a managed native profile or shared app-server. Do not use native `--profile`.               |
| [Grok Build](https://github.com/xai-org/grok-build)          |   ✓    | Uses a dedicated hook file. The session row does not show the worktree name.                     |
| [Antigravity](https://antigravity.google/docs/cli/reference) |   ✓    | Tracks model, context, approvals, cancellations, and background tasks.                           |
| [Reasonix](https://github.com/esengine/DeepSeek-Reasonix)    |        | Does not show token or model columns.                                                            |
| [Kimi Code](https://github.com/MoonshotAI/kimi-code)         |        | Does not support session fork or worktrees.                                                      |
| [opencode](https://github.com/anomalyco/opencode)            |        | Uses a generated plugin. Does not support worktrees or background-task tiers.                    |
| [Pi](https://github.com/earendil-works/pi)                   |        | Supports resume picker, fork, and context tracking. Does not track approval states or worktrees. |
| [omp](https://github.com/can1357/oh-my-pi)                   |        | Tracks approvals, decisions, and background tasks. Does not support worktrees.                   |

Antigravity uses a separate configuration copy for managed sessions.
captain-miao preserves your custom status-line command.
Updates to your global configuration apply during the next launch.

## Supported terminals

| Terminal                                   | Stable | Support notes                                                                   |
| ------------------------------------------ | :----: | ------------------------------------------------------------------------------- |
| [Kitty](https://sw.kovidgoyal.net/kitty/)  |   ✓    | Full support, including stacked windows and styled previews.                    |
| [zellij](https://zellij.dev/) ≥ 0.44       |   ✓    | Supports floating panes and styled previews. Cannot move sessions between tabs. |
| [tmux](https://github.com/tmux/tmux) ≥ 3.2 |        | One window per session with styled previews. No stacked layout.                 |
| [Ghostty](https://ghostty.org/) ≥ 1.3      |        | macOS only. Start agents from the dashboard. No transcript preview.             |
| [iTerm2](https://iterm2.com/) ≥ 3.0        |        | macOS only. Shows plain-text previews of the active screen.                     |

## Key bindings

The six most frequent shortcuts:

| Key                  | Action                                                 |
| -------------------- | ------------------------------------------------------ |
| `j` / `k`, `↑` / `↓` | Navigate sessions.                                     |
| `Enter`              | Focus the window of the selected session.              |
| `o` / `O`            | Start session in current directory / choose directory. |
| `r` / `f`            | Resume a conversation / fork the selected session.     |
| `X`                  | Stop the selected session.                             |
| `s`                  | Select the next session that requires attention.       |

Press `?` to view all key bindings.
Press `,` to open preferences.

Restart waits for the previous session to end before its replacement resumes
the conversation. If the host cannot confirm removal, the dashboard reports an
error. Resume also refuses conversations still managed on the selected host.

## Remote sessions

Press `Space h` to add an SSH host and configure `miao-server`.
Remote sessions continue to run when your network disconnects or your computer sleeps.
Press `Enter` on a detached session to reconnect.
Closing a window stops the session by default.
To keep sessions active after window close, set `[remote] on_window_close = "detach"` in `~/.config/captain-miao/config.toml`.

## How it works

- **Local sessions:** `miao launch <agent>` starts the agent process with a launcher wrapper.
  The launcher captures hook events, reads agent session data, and writes status files.
  The dashboard monitors these files and updates the session table.
  When you select a session, the dashboard uses terminal controls to focus that window or pane.
- **Remote sessions:** The agent and launcher run on the remote SSH host inside a `miao-server` pty pool.
  The server transmits session updates across SSH to your local dashboard.
  Your local terminal connects to the remote session for user input and output.
  If the network connection drops or your computer sleeps, the agent continues on the host.
  Select the session and press `Enter` to reconnect.

## License

MIT. See [LICENSE](LICENSE).
