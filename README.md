<p align="center">
  <img src="assets/logo.jpg" alt="captain-miao logo" width="220">
</p>

<h1 align="center">captain-miao</h1>

<p align="center"><b>Mission control for AI coding sessions in your terminal.</b><br>
Monitor all agent sessions and jump to the window that needs attention.<br>
Remote sessions persist across disconnects and system sleep.</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@hyperlogue/captain-miao"><img src="https://img.shields.io/npm/v/@hyperlogue/captain-miao?color=orange&amp;logo=npm&amp;label=%40hyperlogue%2Fcaptain-miao" alt="npm version"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="license: MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="platforms: macOS, Linux">
</p>

<img src="assets/dashboard.png" alt="captain-miao dashboard in Kitty with 15 active and waiting agent sessions">

<video src="https://github.com/user-attachments/assets/49709d95-c30a-4390-be64-d52f6b95a751" controls width="100%"></video>

## Quick start

Install captain-miao and at least one [supported agent](#supported-agents):

```sh
npm install -g @hyperlogue/captain-miao
```

You can also run `npx @hyperlogue/captain-miao` after terminal configuration.
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

These settings allow socket access to control Kitty windows.
For higher security, use [password authorization](https://sw.kovidgoyal.net/kitty/remote-control/).

**2. Start the dashboard**

```sh
cd /path/to/your/project
miao
```

Press `o` to start a session with the selected host, directory, and agent.
If no session is selected, `o` opens the directory picker.
Press `O` to open the directory picker directly.

Slow actions run in the background. If an action exceeds 0.8 seconds,
a bottom-right progress notification appears. The notification clears
when results appear. Actions like Git push report success explicitly.
Errors remain until dismissed. Dismissing progress does not cancel the action.
Press `x` to dismiss notifications, oldest first. Press `Esc` to close panels,
clear input, and dismiss notifications.

On macOS, system wake or network changes trigger an immediate connection check.
The check waits three seconds before reconnecting stalled links.
Recovery requires active Wi-Fi or VPN.

In the directory picker, press `Ctrl-o` to select an agent and host.
Press `qwertyuiop` for agents and `asdfgzxcvb` for hosts (`[` and `]` turn pages).
Press `Enter` to confirm choices, `Ctrl-o` to launch immediately, or `Esc` to cancel.
Shortcuts `Ctrl-t` and `Ctrl-h` cycle agents and hosts.

Press `r` to resume a conversation. In the picker, press `Ctrl-o` to filter
by agent and host (`0` for all, `qwertyuiop` per agent). Press `Enter` to apply
filters, or `Ctrl-o` to resume the selected session.

The resume picker caches results in memory. It displays cached sessions
immediately and refreshes them in the background. Search filters and selections
persist during refresh. If refresh fails, cached sessions remain visible.
Process restarts clear the cache.

The dashboard highlights sessions that need attention.
Press `s` to select the next highlighted session.
Press `Enter` to focus its window.
In Ghostty, start agents from the dashboard.

## Why captain-miao

- **Keep your terminal environment.** Sessions run in native windows, tabs, or panes.
  Fonts, themes, and key bindings remain unchanged.
- **Keep global settings clean.** Managed sessions use isolated tracking hooks.
  Configuration files like `~/.claude/settings.json` and `~/.codex/hooks.json` remain untouched.
- **Persistent remote sessions.** `miao-server` runs agents in a [shpool](https://github.com/shell-pool/shpool) PTY pool.
  Sessions survive network drops and client sleep.
  Enable clipboard sharing to paste local images with `Ctrl-V`.

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
| [omp](https://github.com/can1357/oh-my-pi)                   |        | Tracks approvals, decisions, plan reviews, and background tasks. Does not support worktrees.     |

Antigravity uses isolated configuration copies for managed sessions, preserving
custom status lines. Global configuration changes apply on the next launch.

The detail panel appends reported effort levels to model names (for example,
`Opus 4.8 (high)`). Claude Code, Codex, Pi, and omp supply effort data.

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
| `o` / `O`            | Start session with selected host, directory, and agent / choose directory. |
| `r` / `f`            | Resume a conversation / fork the selected session.     |
| `X`                  | Stop the selected session.                             |
| `s`                  | Select the next session that requires attention.       |

Press `?` to view all key bindings.
Press `,` to open preferences.

Background commands count as **Task** by default. Dev servers and watchers
(such as `npm run dev` or `cargo watch`) count as **Idle**.
A session becomes Idle only when all background commands are Idle.
Foreground turns and approval prompts keep their own status.

Press `Space t i` to override command classifications.
Configure rules in **Preferences → Idle tasks**:
- `Enter`: Edit pattern.
- `t`: Toggle Task/Idle.
- `a`: Add rule.
- `d` or `r`: Remove override.

Patterns match complete command strings. Use `*` as a wildcard and `\\` to escape
literal characters. Custom rules override defaults across all hosts.

Restart waits for the previous session to stop before resuming.
If removal cannot be confirmed, the dashboard displays an error.
Resume rejects sessions that remain active on the host.

## Remote sessions

Press `Space h` to add an SSH host and configure `miao-server`.
Remote sessions continue running when your network drops or your computer sleeps.
Press `Enter` on a detached session to reconnect.
In the Hosts panel, press `c` to disconnect an SSH connection, and `c` again to reconnect.
Remote pooled sessions continue running while disconnected.
Closing a window terminates the session by default.
To detach instead, set `[remote] on_window_close = "detach"` in `~/.config/captain-miao/config.toml`.

## How it works

- **Local sessions:** `miao launch <agent>` wraps the agent process and records
  hook events to status files. The dashboard reads these files and controls terminal focus.
- **Remote sessions:** The agent runs inside a `miao-server` PTY pool on the SSH host.
  The server streams status updates over SSH. If the network drops, the agent
  continues running. Press `Enter` on the session to reconnect.

## License

MIT. See [LICENSE](LICENSE).
