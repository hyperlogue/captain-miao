<p align="center">
  <img src="assets/logo.jpg" alt="captain-miao logo" width="320">
</p>

# captain-miao

<img src="https://oss-assets.hyperlogue.tech/captain-miao/cm_screenshot.png" alt="captain-miao dashboard">

A TUI dashboard for managing multiple AI coding sessions running in the terminal emulator or multiplexer of your choice, such as [Kitty](https://sw.kovidgoyal.net/kitty/), [Ghostty](https://ghostty.org/), [zellij](https://zellij.dev/) and [tmux](https://github.com/tmux/tmux).

https://github.com/user-attachments/assets/e51ffc2f-0d6c-41c1-a825-0de32f2bed3a

When you run several agent sessions at once, it's hard to tell which is working, which is waiting on you, and which has already finished. captain-miao watches every session and shows the whole fleet at a glance (status, working directory, context usage, and a live preview), and lets you start, focus, fork, or kill any of them without leaving the dashboard.

Unlike herdr or cmux, captain-miao embeds no terminal of its own. It drives the
Kitty, Ghostty, zellij or tmux you already run (every session is a native window
or pane, controlled through the terminal's own protocol), so it stays one small,
focused tool and the rest of your workflow is yours to compose.

## Highlights

- **The whole fleet at a glance:** every session in one table, with status, working directory, model, context usage, git branch, and a live transcript preview.
- **Never miss a prompt:** sessions waiting on your approval or an answer are flagged.
- **Full session lifecycle:** launch, resume, fork, and kill sessions from the dashboard.
- **Sessions on remote servers:** federate several hosts into one dashboard, each running its sessions in its own pty pool ([shpool](https://github.com/shell-pool/shpool)), so a dropped connection or a slept laptop detaches windows without touching the sessions.
- **Support [Claude Code](https://claude.com/claude-code) and [Codex](https://github.com/openai/codex)** today, behind a backend abstraction built to extend to other coding agents. [Reasonix](https://github.com/esengine/DeepSeek-Reasonix) ships too, with [known limits](#reasonix-support).
- **direnv-aware:** a session started in a directory with an `.envrc` picks up that environment automatically (via `direnv exec`).
- **[r3](https://github.com/hyperlogue/r3) integration:** when a session's running background task is an `r3 watch` waiting for your review, it flags as **Review** and surfaces as needing your attention.
- **Keep-awake:** prevents your machine from sleeping while any session is still working (`caffeinate` on macOS, `systemd-inhibit` on Linux).

## Requirements

One supported terminal to drive, and at least one agent CLI on your `PATH`.

### Terminals

| Terminal                                                    | Notes                                                                                                                                                                       |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **[Kitty](https://github.com/kovidgoyal/kitty)**             | Needs remote control enabled ([Kitty setup](#kitty-setup)). Most of features in captain-miao are designed around Kitty.                                                     |
| **[Ghostty](https://github.com/ghostty-org/ghostty)** ≥ 1.3  | **macOS only**, driven through Ghostty's AppleScript dictionary ([Ghostty setup](#ghostty-setup)). Nothing in that API reads a window's screen, so there is **no preview**. |
| **[zellij](https://github.com/zellij-org/zellij)** ≥ 0.44    | Sessions live as full-size floating panes in one `miao:sessions` tab.                                                                                                       |
| **[tmux](https://github.com/tmux/tmux)** ≥ 3.2               | One window per session.                                                                                                                                                     |

### Agents

| Agent                                                         | Notes                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **[Claude Code](https://claude.com/claude-code)**             |                                                                                                                                                                                                                                                                 |
| **[Codex](https://github.com/openai/codex)**                  | Hooks can't be injected per-invocation, so a session runs under a synthetic `CODEX_HOME` that symlinks your real one. No support for pasting in remote sessions, as it reads the clipboard in-process ([details](#pasting-a-screenshot-into-a-remote-session)). |
| **[Reasonix](https://github.com/esengine/DeepSeek-Reasonix)** | Token/model columns, resume-picker entries and worktrees don't work ([known limits](#reasonix-support)).                                                                                                                                                        |

## Installation

### From source with Cargo

```sh
cargo install --git https://github.com/hyperlogue/captain-miao
```

This installs the `miao` command (the project is captain-miao; the binary is short because you'll type it a lot).

Building needs a Rust toolchain and a C compiler (for the statically-bundled SQLite that reads Codex session titles).

### From a prebuilt binary (npm)

No Rust toolchain, no build:

```sh
npx @hyperlogue/captain-miao          # run it once
npm install -g @hyperlogue/captain-miao   # or install the `miao` command
```

`bunx @hyperlogue/captain-miao` works too. The npm package is a small launcher
that execs a prebuilt native binary shipped as a per-platform optional
dependency, so your package manager downloads only the one binary matching your
machine; nothing is fetched at runtime. Prebuilt binaries cover **macOS** (Apple
silicon + Intel) and **Linux** (x86-64 + arm64), and are also attached to every
[GitHub Release](https://github.com/hyperlogue/captain-miao/releases) as
`miao-v<version>-<target>.tar.gz` if you'd rather download one directly.

Every prebuilt binary carries an x86-64 glibc `miao-server`, so it can set up a
remote host that has nothing installed on it — see [Running sessions on remote
servers](#running-sessions-on-remote-servers). A host on another architecture, or
one with no glibc, gets its server downloaded at deploy time instead. If you'd
rather not depend on that, each release also has a
`miao-bundled-all-server-v<version>-<target>.tar.gz` carrying every server
captain-miao publishes — larger, and never needs the network.

### With Nix

A flake is provided; run it straight from GitHub:

```sh
nix run github:hyperlogue/captain-miao
```

## Kitty setup

captain-miao drives Kitty over its [remote-control protocol](https://sw.kovidgoyal.net/kitty/remote-control/), so your `kitty.conf` must allow it. Remote control is a real privilege (a program that has it can read your terminal and run commands), so the tightest setup kitty offers pairs a password with an authorization script:

```conf
allow_remote_control password
remote_control_password "i-am-the-captain-miao" captain_miao_rc.py
listen_on unix:/tmp/mykitty
```

Kitty resolves that filename against your config directory, so put the script at `~/.config/kitty/captain_miao_rc.py`:

```python
# The only remote-control commands captain-miao issues.
ALLOWED_COMMANDS = frozenset({
    "ls", "get-text", "launch", "focus-window",
    "focus-tab", "close-window", "detach-window", "goto-layout",
})

def is_cmd_allowed(pcmd, window, from_socket, extra_data):
    # Reject the in-terminal escape-code channel; only the listen_on socket gets in.
    return from_socket and pcmd["cmd"] in ALLOWED_COMMANDS
```

Every request must now clear three checks: arrive over the socket (not the escape-code channel that a shell, even one across `ssh`, could otherwise use), carry the password, and name one of the commands above. `i-am-the-captain-miao` is captain-miao's built-in default, so this works as written; to use your own secret instead, set `remote_control_password` (above) and `[kitty] rc_password` in captain-miao's config to match. Keep the script the _last_ item after the password; command names listed alongside it are allowed without ever calling your function.

Looser alternatives: `allow_remote_control socket-only` (off the escape-code channel, but no password and no allowlist) or `allow_remote_control yes` (no checks at all; avoid it). captain-miao verifies remote control at startup and exits with a diagnostic if it can't connect.

**Keep the `stack` layout enabled.** captain-miao's default **Stacked** session layout puts every session in one kitty tab and shows one at a time via kitty's `stack` layout. The default `enabled_layouts *` already includes it; if you've narrowed that list, add `stack` or sessions tile instead of stacking. (The alternate **Per-tab** layout, toggled with `Space l`, needs no particular layout.)

## Ghostty setup

**macOS only, Ghostty ≥ 1.3.** captain-miao drives Ghostty through its [AppleScript dictionary](https://ghostty.org/docs/features/applescript), which is enabled by default — there is no config file to edit. The one thing you must do is approve the Automation prompt macOS raises the first time captain-miao talks to Ghostty; if you dismissed it, re-enable it under **System Settings → Privacy & Security → Automation**, in the entry for whatever launched captain-miao. It verifies the channel at startup and exits with a diagnostic naming the fix if it can't get through.

The Linux build of Ghostty exposes no equivalent control channel, so captain-miao does not claim it there — run it under zellij or tmux instead.

Three things work differently here, all of them because the dictionary has no way to express them:

- **No preview.** Nothing in Ghostty's automation API reads a window's screen or scrollback, so the preview pane says so instead of showing output. Everything else on the row — status, context usage, working directory — comes from the agent's own files and is unaffected.
- **No move-to-tab.** `t` is hidden, as it is on zellij.
- **New sessions bring Ghostty to the front.** Ghostty activates itself whenever a script creates a window or tab ([ghostty#11457](https://github.com/ghostty-org/ghostty/issues/11457)), with no way to opt out, so a spawn takes focus even when captain-miao asks it not to.

Sessions always get their own tab: Ghostty has neither a stack layout nor floating panes, so `Space l` has nothing to toggle and is hidden, exactly as on tmux.

## Usage

Run the dashboard inside a supported terminal (Kitty, Ghostty, zellij or tmux):

```sh
miao
```

From the dashboard, `o` / `O` start new sessions and `r` resumes existing ones. You can also drive captain-miao from the shell:

| Command                         | What it does                                                                                                                                |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `miao`                          | Run the TUI dashboard (the default).                                                                                                        |
| `miao claude [dir] [args…]`     | Launch Claude Code in `dir` (default `.`) with tracking hooks. Args starting with `-` (e.g. `--resume`) are forwarded straight to `claude`. |
| `miao codex [dir] [args…]`      | Launch Codex in `dir` with tracking hooks; extra args are forwarded to `codex`.                                                             |
| `miao reasonix [dir] [args…]`   | Launch Reasonix in `dir` with tracking hooks; extra args are forwarded to `reasonix`. See [known limits](#reasonix-support).                |
| `miao focus [--window-id <id>]` | Focus the running dashboard window; with `--window-id`, also ring the session running in that Kitty window.                                 |
| `miao hook <event>`             | Internal: forwards an agent hook event to the launcher. You won't run this yourself; it's wired up automatically.                           |

Sessions launched via `claude` / `codex` / `reasonix` are wrapped by a _launcher_ process that injects the tracking hooks, so they show up in the dashboard automatically. Hooks are injected per-session and torn down on exit; nothing is written to your global `~/.claude/settings.json`.

#### Reasonix support

Reasonix rows track status — working, idle, waiting for approval, compacting — and launch, resume and fork all work. It is newer than the other two backends and does less:

- **No token or model columns, and no resume picker entries.** Both need Reasonix's session sidecars, whose on-disk schema hasn't been settled yet. Its hook payload carries no transcript path, so the dashboard reads nothing from disk for these sessions.
- **No worktrees** (`Ctrl-g` hides itself) and **no background-task tiers** — a `Stop` while a background task runs reads as `Idle`.
- **Run `reasonix setup` outside captain-miao once first.** A session runs under a synthetic config home that mirrors your real one; a first-time setup performed _inside_ a session lands in that mirror and is cleared on a later launch.

It also hasn't yet been exercised against a released `reasonix` build end to end, so please report anything that looks wrong rather than assuming it's expected.

### Key bindings

Press `?` in the dashboard for the complete list. The six you'll reach for most:

| Key                            | Action                                                                                                             |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `j`/`k`, `↑`/`↓`, `Ctrl-n`/`p` | Navigate sessions                                                                                                  |
| `Enter`                        | Focus the selected session's window, or attach one to a detached session (asking first if another client holds it) |
| `o` / `O`                      | New session (same cwd / prompt for cwd)                                                                            |
| `r` / `f`                      | Resume picker (one host; `Ctrl-h` switches) / fork the selected session                                            |
| `x` / `D`                      | Kill the selected session / detach from it, leaving it running                                                     |
| `s`                            | Jump to the next session needing attention                                                                         |

#### Remaining key bindings

| Key                   | Action                                                                                                                                                                     |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `gg` / `G`            | Jump to top / bottom                                                                                                                                                       |
| `1..9` / `Ctrl-1..9`  | Select Nth session / select and focus its window                                                                                                                           |
| `p` / `i`             | Pin / toggle needs-input on the selected session                                                                                                                           |
| `y`                   | Copy the selected session id to the clipboard                                                                                                                              |
| `t` / `w`             | Move window to tab (Kitty and tmux) / switch to or open the cwd's work tab                                                                                                 |
| `h`/`l`, `←`/`→`      | Scroll the preview horizontally                                                                                                                                            |
| `Ctrl-u` / `Ctrl-d`   | Scroll the preview up / down                                                                                                                                               |
| `R`                   | Refresh the preview now                                                                                                                                                    |
| `Space v` / `Space d` | Toggle the preview / detail panel                                                                                                                                          |
| `Space i`             | Edit the selected directory's icon + color                                                                                                                                 |
| `Space e` / `Space E` | Restart the selected / all idle sessions                                                                                                                                   |
| `Space z`             | Toggle keep-awake (inhibit OS sleep while sessions work)                                                                                                                   |
| `Space a` / `Space H` | Set the default backend / default host for new sessions                                                                                                                    |
| `Space l`             | Switch session layout (stacked in one tab / one tab per session; not offered on tmux or Ghostty, which have only the one)                                                  |
| `Space h` / `Space s` | Hosts panel (add, edit, port forwards, suspend with `c`, upgrade the host's server with `u`, connection log with `l`) / attach to a session, kicking the client holding it |
| `Space A`             | Attach a window to every detached session that's free to take (rows another client holds are skipped, not stolen)                                                          |
| `?`                   | Show the full key list (help overlay)                                                                                                                                      |
| `/`                   | Search                                                                                                                                                                     |
| `q` / `Ctrl-c`        | Quit                                                                                                                                                                       |

Pressing `Space` (the leader) shows a which-key strip of the available follow-up keys in the footer.

In the cwd picker, `Ctrl-t` switches the backend for that one launch, `Ctrl-h` the host, and `Ctrl-d` drops the highlighted recent directory.

**Custom keybindings.** Every Normal-mode command above is remappable via a `[keybinds]` table in `~/.config/captain-miao/config.toml`. Map a command id to a key (or list of keys); an empty list unbinds it:

```toml
[keybinds]
kill = "X"                      # move kill from x to X
jump_attention = ["s", "n"]     # bind two keys to one command
restart = "space r"             # remap a leader sequence
toggle_detail = []              # unbind a command
```

Keys parse forms like `"ctrl+u"`, `"O"` (= `"shift+o"`), `"space e"`, `"enter"`, `"f5"`, and arrow names. `Ctrl-c`, `g g`, and the `1..9` / `Ctrl-1..9` selectors are fixed.

Command ids are the string in each `Command::id()`; the authoritative list lives in the `DEFAULTS` table in [`src/app/keymap.rs`](src/app/keymap.rs), and they match the actions in the key-bindings table above.

## Configuration

captain-miao reads an optional TOML file at `~/.config/captain-miao/config.toml` (or `$XDG_CONFIG_HOME/captain-miao/config.toml`). Every key is optional and falls back to the default shown below; an unparseable file falls back to defaults rather than crashing. The complete set of options:

```toml
[terminal]
backend = "kitty"            # "kitty" | "ghostty" | "zellij" | "tmux"; unset auto-detects
                             # (zellij, then tmux, then Ghostty, else Kitty)
sessions_layout = "stacked"  # "stacked" | "per-tab" (the runtime Space l toggle overrides this;
                             # tmux and Ghostty are always per-tab)

[kitty]
rc_password = "i-am-the-captain-miao"   # the built-in default, and a published constant; set your own (see Kitty setup)

[launcher]
default_agent = "claude"     # backend for new sessions: "claude" | "codex" (Space a overrides)
approval_grace_secs = 2      # grace window after a permission dialog before a transcript change reads as "dismissed"
max_recent_cwds = 50         # entries kept in the workdir picker's recent list
resume_list_limit = 50       # max sessions listed in the resume picker (most recent first)
new_tab_title = "{agent}: {basename}"     # new-session tab title; placeholders: {agent} {basename} {cwd}
resume_tab_title = "{agent}: {basename}"  # resumed-session tab title
pooled = false               # run this machine's sessions in a local pty pool, so they
                             # survive closing the window; needs miao-server on PATH

[remote]
on_window_close = "close"    # "close" | "detach": what closing a pooled session's window
                             # does to the session. Only a window *you* close counts — an
                             # attach that ends because its link died (a laptop resuming to
                             # a dropped ssh) always detaches, and the session keeps running.

[thresholds]
context_warning_tokens = 175000    # context usage turns to the warning color here
context_critical_tokens = 400000   # …and to the critical color here
preview_stale_secs = 20            # show "updated Ns ago" once the preview is older than this (0 = always)

[polling]
fs_reload_debounce_ms = 100        # debounce for filesystem-watch reloads
preview_debounce_ms = 200          # debounce before re-fetching the preview
event_poll_ms = 100                # input poll interval (floored at 10)
preview_auto_refresh_secs = 10     # auto-refresh the preview while focused + busy + unscrolled (0 disables)

[ui.panels]
preview_auto_min_height = 16       # min body height before the preview auto-shows
detail_auto_min_width = 70         # min body width before the detail panel auto-shows
detail_default_width = 36          # detail panel column width
narrow_max_width = 90              # at/below this body width the layout stacks vertically

[ui.table]
name_truncate = 35                 # max characters of a session name before truncation

[colors.ui]
title_fg = "cyan"
header_fg = "cyan"
attention_fg = "yellow"
error_fg = "red"
highlight_bg = "dark_gray"
selection_fg = "blue"
selection_symbol = "❯ "

[colors.picker]
highlight_bg = "dark_gray"
chevron_fg = "blue"

[debug]
enabled = false                    # verbose logging; also enabled by CAPTAIN_MIAO_DEBUG=1
log_file = "debug.log"
keybind_log_file = "keybinds.log"

[keybinds]
# Remap any Normal-mode command: command-id = "key" or ["key", "alt"]; [] unbinds.
# command-ids are the Command::id() strings in src/app/keymap.rs (DEFAULTS table).
# e.g. kill = "X"  /  jump_attention = ["s", "n"]  /  restart = "space r"
```

Colors accept named values (`cyan`, `dark_gray`, …) or `#rrggbb` hex. The command ids for `[keybinds]` are the ones in the key-bindings table above (`kill`, `jump_attention`, `restart`, `toggle_preview`, …).

### Running sessions on remote servers

Add hosts with `Space h`. Each runs a `miao-server` daemon holding its sessions
in a pty pool, and the dashboard attaches local windows to them over ssh — so a
dropped connection or a slept laptop detaches windows without touching the
sessions, and reconnecting brings them back. Full design notes:
[docs/remote-sessions.md](docs/remote-sessions.md).

The panel is where each host reports in: connection state and the reason when it
failed, session counts, daemon version, latency, and CPU + memory. `l` opens its
full connection log, `c` suspends it, `u` upgrades its server.

- **Detached rows** — running there, no window here — are dimmed and marked 🙈
  when free or 👀 when another client is holding one. `Enter` attaches, `Space A`
  attaches every free one, `Space s` steals a held one.
- **Closing a session's window ends it**, the same as `x`; set `on_window_close =
"detach"` under `[remote]` for the opposite. A window lost to a dropped link
  detaches instead, so a flaky network never costs you a session.
- **`Options`** takes verbatim ssh arguments, mainly port forwards
  (`-L 8080:localhost:3000`), which come up and go away with the connection.
  Everything else belongs in `~/.ssh/config`.
- **Terminfo** — a host with no entry for your `TERM` is offered yours, so
  sessions there stop falling back to `xterm-256color`. It asks first.
- **The daemon** is either your own on `PATH` or one the dashboard deploys.
  `cargo xtask dist` bundles servers into the binary (`--list` shows the
  variants); carrying none for a host, it offers to download the published one.
  `miao --version` reports what a binary carries.
- **Run `loginctl enable-linger "$USER"`** on any Linux host running the daemon,
  or systemd-logind takes its sockets away at your last logout.

#### Pasting a screenshot into a remote session

`p` in the hosts panel offers that host **this machine's clipboard**, so `Ctrl+V`
in an agent running there attaches a screenshot you just took here. It works by
shadowing `xclip`/`wl-paste` on the agent's `PATH` with a shim that asks back over
an owner-only unix socket, ssh-forwarded while the host is connected.

**Only images are ever served.** Text is not filtered out — it is never requested,
so a remote can't read your password manager through this. It is off by default
and per-host, because while a host is connected anything running as you there
(including the agent, which runs arbitrary code by design) can read your clipboard
when it holds an image.

Sharp edges worth knowing:

- **Codex has no `Ctrl+V`** here: it reads the clipboard in-process, so no shim
  can serve it. Run `clipboard-paste` in the session instead — it writes the image
  beside the agent and prints the path to hand it.
- **A macOS host** gets nothing: the agent's clipboard path there is `osascript`,
  which never reaches a shim. `clipboard-paste` is the whole story on such a host.
- **On a Linux dashboard** only what the clipboard actually offers can be served,
  so a browser-copied JPEG answers "no image" — there is no converter on that side.
  macOS re-encodes, so anything on the pasteboard works.
- **Only sessions started after you enable it** are shimmed; restart a session to
  pick it up.
- **Two dashboards on different machines against one host** collide: the later one
  wins the forward and the earlier one's paste stops working until it reconnects.

## How it works

captain-miao is built around a strict unidirectional data flow:

- The **launcher** wraps each agent process and is the single source of truth for that session's state. It receives hook events over a Unix socket and writes a JSON state file.
- **Hooks** are thin forwarders: they parse the agent's hook payload from stdin and send it to the launcher socket.
- The **dashboard** is a pure viewer. It watches the session state directory and per-backend transcript dirs with `notify` (FSEvents on macOS, inotify on Linux) and re-reads files when they change. It performs no IPC of its own.

State lives under `~/.local/state/captain-miao/` and runtime sockets under `$XDG_RUNTIME_DIR/captain-miao/`, both owner-only: session state files record your prompt text, so they are written `0600` under a `0700` directory. For a deeper tour of the architecture, module layout, hook wiring, and data files, see [AGENTS.md](AGENTS.md).

## Roadmap

- [ ] **tmux**: its live-server test now runs in CI on both Linux and macOS (tmux is the one backend that _can_ be tested headlessly — a server on a private socket is the whole dependency), but only against the one version the flake pins. The claimed ≥ 3.2 floor is still unverified; testing a matrix of versions down to it is what graduates this.
- [ ] **More agent backends**: the per-session backend is an abstraction, so other coding agents can slot in alongside Claude Code and Codex. Reasonix is the first to do so and is still unproven against a released build ([known limits](#reasonix-support)); Grok, Kimi Code, Pi and opencode are mapped but unwritten.
- [ ] **Ghostty**: shipped, but nothing in it has run against a live Ghostty — the AppleScript backend is unit-tested only, since driving one needs a Mac with a GUI session and a hand-clicked Automation grant that CI can't supply. First-hand confirmation on a real Mac is what graduates it.
- [ ] **More terminal backends**: the terminal layer is an abstraction (Kitty, Ghostty, zellij and tmux today), so other terminals and multiplexers (WezTerm, …) can slot in.

## License

MIT. See [LICENSE](LICENSE).
