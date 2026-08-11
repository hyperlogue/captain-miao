<p align="center">
  <img src="assets/logo.jpg" alt="captain-miao logo" width="320">
</p>

# captain-miao

<img src="https://oss-assets.hyperlogue.tech/captain-miao/cm_screenshot.png" alt="captain-miao dashboard">

A TUI dashboard for managing multiple AI coding sessions running in the terminal emulator or multiplexer of your choice, such as [Kitty](https://sw.kovidgoyal.net/kitty/), [zellij](https://zellij.dev/) and [tmux](https://github.com/tmux/tmux).

https://github.com/user-attachments/assets/e51ffc2f-0d6c-41c1-a825-0de32f2bed3a

When you run several agent sessions at once, it's hard to tell which is working, which is waiting on you, and which has already finished. captain-miao watches every session and shows the whole fleet at a glance (status, working directory, context usage, and a live preview), and lets you start, focus, fork, or kill any of them without leaving the dashboard.

Unlike herdr or cmux, captain-miao embeds no terminal of its own. It drives the
Kitty, zellij or tmux you already run (every session is a native window or pane,
controlled through the terminal's own protocol), so it stays one small, focused
tool and the rest of your workflow is yours to compose.

## Highlights

- **The whole fleet at a glance:** every session in one table, with status, working directory, model, context usage, git branch, and a live transcript preview.
- **Never miss a prompt:** sessions waiting on your approval or an answer are flagged.
- **Full session lifecycle:** launch, resume, fork, and kill sessions from the dashboard, with a filterable picker for recent working directories.
- **Support [Claude Code](https://claude.com/claude-code) and [Codex](https://github.com/openai/codex)** today, behind a backend abstraction built to extend to other coding agents.
- **direnv-aware:** a session started in a directory with an `.envrc` picks up that environment automatically (via `direnv exec`).
- **[r3](https://github.com/hyperlogue/r3) integration:** when a session's running background task is an `r3 watch` waiting for your review, it flags as **Review** and surfaces as needing your attention.
- **Keep-awake:** prevents your machine from sleeping while any session is still working (`caffeinate` on macOS, `systemd-inhibit` on Linux).
- **Pin, mute, mark:** pin important sessions to the top, mute the ones you don't need right now, and flag the ones to follow up on.
- **Sessions that outlive their window** (opt-in): run them in a local pty pool so closing the terminal — or logging out — doesn't end them, and a dashboard on another machine can attach to the same ones.
- **Sessions on other machines:** federate several hosts into one dashboard, each running its sessions in its own pty pool, so a dropped connection or a slept laptop detaches windows without touching the sessions.
- 🧪 **Isolated worktrees:** start a session in a fresh git worktree so it can't touch your main checkout — see [Experimental](#experimental).

## Experimental

Two parts of captain-miao are usable but not yet load-bearing. They're marked 🧪
throughout this README, and each is listed here with what specifically is
unproven — so you can judge the risk rather than guess at it.

| Feature | State | What's unverified |
| --- | --- | --- |
| **tmux backend** | On by default when you run inside tmux | Probe-verified against **tmux 3.7b only**. The documented ≥ 3.2 floor is a claim, not a tested one, and the backend's live test doesn't yet run in CI. |
| **Worktrees** (`Ctrl-g`) | On, Claude Code only | Newly added and lightly exercised. captain-miao only passes `--worktree`; the worktree itself is the agent's, so the blast radius is small — but the flag has not been through a real multi-week workflow. |

Everything else — the dashboard, Kitty and zellij, both agent backends, pooled
local sessions, and remote hosts — is the supported path.

## Requirements

- A supported terminal: **Kitty** with remote control enabled (see [Kitty setup](#kitty-setup)), **zellij** ≥ 0.44, or 🧪 **tmux** ≥ 3.2 (for either multiplexer, run captain-miao inside the session it should drive; no extra setup needed).
- **Claude Code** and/or **Codex** on your `PATH`.

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
[GitHub Release](https://github.com/hyperlogue/captain-miao/releases) as a
`.tar.gz` if you'd rather download one directly.

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

## Usage

Run the dashboard inside a supported terminal (Kitty, zellij or tmux):

```sh
miao
```

> `miao` must be launched from within Kitty or a zellij/tmux session; it exits with an error otherwise. Inside a multiplexer it auto-selects that backend — zellij first, then tmux, else Kitty (override with `[terminal] backend` in the config). Both multiplexers deliberately beat an ambient Kitty: nested inside Kitty, every pane inherits the outer `KITTY_WINDOW_ID`, so a Kitty backend would drive the wrong window.

From the dashboard, `o` / `O` start new sessions and `r` resumes existing ones. You can also drive captain-miao from the shell:

| Command                       | What it does                                                                                                                               |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `miao`                          | Run the TUI dashboard (the default).                                                                                                       |
| `miao claude [dir] [args…]`     | Launch Claude Code in `dir` (default `.`) with tracking hooks. Args starting with `-` (e.g. `--resume`) are forwarded straight to `claude`. |
| `miao codex [dir] [args…]`      | Launch Codex in `dir` with tracking hooks; extra args are forwarded to `codex`.                                                             |
| `miao focus [--window-id <id>]` | Focus the running dashboard window; with `--window-id`, also ring the session running in that Kitty window.                                 |
| `miao hook <event>`             | Internal: forwards an agent hook event to the launcher. You won't run this yourself; it's wired up automatically.                           |

Sessions launched via `claude` / `codex` are wrapped by a _launcher_ process that injects the tracking hooks, so they show up in the dashboard automatically. Hooks are injected per-session and torn down on exit; nothing is written to your global `~/.claude/settings.json`.

### Key bindings

Press `?` in the dashboard for the complete list. Highlights:

| Key                            | Action                                                                 |
| ------------------------------ | ---------------------------------------------------------------------- |
| `j`/`k`, `↑`/`↓`, `Ctrl-n`/`p` | Navigate sessions                                                      |
| `gg` / `G`                     | Jump to top / bottom                                                   |
| `1..9` / `Ctrl-1..9`           | Select Nth session / select and focus its window                       |
| `Enter`                        | Focus the selected session's window                                    |
| `o` / `O`                      | New session (same tab / prompt for cwd)                                |
| `r` / `f`                      | Resume picker (one host; `Ctrl-h` switches) / fork the selected session |
| `x` / `D`                      | Kill the selected session / detach from it, leaving it running         |
| `s`                            | Jump to the next session needing attention                             |
| `m` / `p` / `i`                | Mute / pin / toggle needs-input on the selected session                |
| `y`                            | Copy the selected session id to the clipboard                          |
| `t` / `w`                      | Move window to tab (Kitty and tmux) / switch to or open the cwd's work tab |
| `h`/`l`, `←`/`→`               | Scroll the preview horizontally                                        |
| `Ctrl-u` / `Ctrl-d`            | Scroll the preview up / down                                           |
| `R`                            | Refresh the preview now                                                |
| `Space v` / `Space d`          | Toggle the preview / detail panel                                      |
| `Space i`                      | Edit the selected directory's icon + color                             |
| `Space e` / `Space E`          | Restart the selected / all idle sessions                               |
| `Space z`                      | Toggle keep-awake (inhibit OS sleep while sessions work)               |
| `Space a` / `Space H`          | Set the default backend / default host for new sessions                |
| `Space l`                      | Switch session layout (stacked in one tab / one tab per session; not offered on tmux, which has only the one) |
| `Space h` / `Space s`          | Hosts panel (add, edit, suspend with `c`, connection log with `l`) / attach to a session, kicking the client holding it |
| `?`                            | Show the full key list (help overlay)                                  |
| `/`                            | Search                                                                 |
| `q` / `Ctrl-c`                 | Quit                                                                   |

Pressing `Space` (the leader) shows a which-key strip of the available follow-up keys in the footer.

In the cwd picker, `Ctrl-t` switches the backend for that one launch, `Ctrl-h` the host, `Ctrl-d` drops the highlighted recent directory, and 🧪 `Ctrl-g` starts the session in a fresh **git worktree**.

> 🧪 **Worktrees are experimental** — newly added and lightly exercised. See
> [Experimental](#experimental).

`Ctrl-g` also asks for a name: type one and press Enter for
`.claude/worktrees/<name>` on a `worktree-<name>` branch (a `#1234` PR reference
works too), or just press Enter to let the agent name it. `Esc` there means "no
worktree after all".

Worktrees are created by the agent, not by captain-miao: `Ctrl-g` adds
`--worktree` to the launch, and Claude Code does the rest — the branch,
`.worktreeinclude` files copied in, edits to your main checkout blocked, and a
keep-or-remove prompt when the session exits. Resuming or restarting a worktree
session returns it to the same worktree without asking again. Codex has no
equivalent, so the key is hidden when it's the selected backend.

Worktree rows keep their project's identity: the directory icon and colour you
set with `Space i` are shared with the repo (set it once, every worktree of it
follows), while `w` opens a work tab per worktree — they're different branches —
titled `<repo>@<worktree>` so the tab bar stays readable.

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
backend = "kitty"            # "kitty" | "zellij" | "tmux"; unset auto-detects (zellij, then tmux, else Kitty)
sessions_layout = "stacked"  # "stacked" | "per-tab" (the runtime Space l toggle overrides this; tmux is always per-tab)

[kitty]
rc_password = "i-am-the-captain-miao"   # the built-in default, and a published constant; set your own (see Kitty setup)

[launcher]
default_agent = "claude"     # backend for new sessions: "claude" | "codex" (Space a overrides)
approval_grace_secs = 2      # grace window after a permission dialog before a transcript change reads as "dismissed"
max_recent_cwds = 50         # entries kept in the workdir picker's recent list
resume_list_limit = 50       # max sessions listed in the resume picker (most recent first)
new_tab_title = "{agent}: {basename}"     # new-session tab title; placeholders: {agent} {basename} {cwd}
resume_tab_title = "{agent}: {basename}"  # resumed-session tab title
pooled = false               # run this machine's sessions in the local pty pool (see below)

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

### Pooled sessions (`[launcher] pooled`)

By default a session *is* its terminal window: closing the window ends it. With
`pooled = true` captain-miao instead runs each session in a local pty pool (an
embedded [libshpool](https://github.com/shell-pool/shpool)), and the window
merely *attaches* to it — so the session survives closing the window, a crashed
multiplexer, and logging out.

This is meant for **dev servers, not laptops**. On a machine you only ever sit
at, the pool buys no persistence and costs an extra process hop, no scrollback
replay when you reattach, and one client at a time. On a machine you also reach
from elsewhere it's the point: a dashboard on your laptop and a captain-miao you
ssh into from a phone become two clients of the *same* sessions.

Needs `miao-server` on `PATH` (it hosts the pool); without it the
dashboard says so and falls back to the default behaviour. On Linux, also run
`loginctl enable-linger` — see below.

### Running sessions on other machines

Add hosts with `Space h`.
Each host runs a `miao-server` daemon holding its sessions in a pty
pool, and the dashboard attaches local windows to them over ssh, so a dropped
connection or a slept laptop detaches windows without touching the sessions —
and reconnecting brings them all back. Full design notes:
[docs/remote-sessions.md](docs/remote-sessions.md).

**Getting the daemon onto each host.** Either install `miao-server`
there yourself (any copy on `PATH` matching your dashboard's version is used as
is, and never touched), or let the dashboard carry one and deploy it for you:

```sh
# Download this version's published servers and bundle them in — needs only
# curl and tar, no cross toolchain:
cargo xtask dist --variant bundle-linux --from release

# …or cross-compile them from the sources beside you, which is what you want
# while changing the server itself:
nix develop                                # provides zig + the cross toolchains
cargo xtask dist --variant bundle-linux

# …or straight from the flake, no dev shell needed:
nix build .#captain-miao-bundle-linux
```

Either way it ends with the servers embedded in the finished dashboard, and
there is no separate step to run or keep up to date.

The resulting binary pushes the right server to any host that's missing one,
verifies it runs there before putting it in place, and skips the work on later
connects. The embedded binaries target glibc 2.28 (Debian 10, RHEL 8, and newer)
and cost about 7 MB (7.6 → 14.2 MB).

What a dashboard carries is fixed when it is built, and `miao --version` reports
it — including each server's digest, which is what tells two builds of the same
version apart. `cargo xtask dist` builds the named release variants side by
side — a plain `miao` carrying nothing and a `miao-bundle-linux` carrying both — and
`--list` shows the rest, including single-arch bundles if your fleet is only one.
The flake exposes the same set as packages (`captain-miao-bundle-linux`,
`-x86_64`, `-aarch64`). Binaries from npm and GitHub Releases are the plain
build; each release also publishes the servers on their own, which is what
`--from release` downloads.

`miao --version` says what any given binary is carrying:

```
miao 0.3.0
embedded miao-server:
  aarch64-unknown-linux-gnu     3.2 MiB  36fd6ac00444
  x86_64-unknown-linux-gnu      3.4 MiB  c1a3cd563639
```

**On any Linux host that runs the daemon — including your own machine under
`pooled = true` — run `loginctl enable-linger`:**

```sh
loginctl enable-linger "$USER"
```

Without it, systemd-logind removes `/run/user/<uid>` when you log out, taking
the daemon's sockets with it (and on distros with `KillUserProcesses=yes`,
killing the daemon outright). captain-miao recovers on the next login — the
daemon notices its socket is gone and rebinds — but linger avoids the outage
entirely, which matters precisely when you're away and expecting persistence.

## How it works

captain-miao is built around a strict unidirectional data flow:

- The **launcher** wraps each agent process and is the single source of truth for that session's state. It receives hook events over a Unix socket and writes a JSON state file.
- **Hooks** are thin forwarders: they parse the agent's hook payload from stdin and send it to the launcher socket.
- The **dashboard** is a pure viewer. It watches the session state directory and per-backend transcript dirs with `notify` (FSEvents on macOS, inotify on Linux) and re-reads files when they change. It performs no IPC of its own.

State lives under `~/.local/state/captain-miao/` and runtime sockets under `$XDG_RUNTIME_DIR/captain-miao/`, both owner-only: session state files record your prompt text, so they are written `0600` under a `0700` directory. For a deeper tour of the architecture, module layout, hook wiring, and data files, see [AGENTS.md](AGENTS.md).

## Roadmap

Graduating the two 🧪 [experimental](#experimental) features is the near-term
work:

- [ ] 🧪 **tmux**: probe-verified on 3.7b only, and its live-server test doesn't run in CI yet. tmux is the one backend that *can* be tested headlessly — a server on a socket is the whole dependency — so putting that test in CI, and pinning the claimed ≥ 3.2 floor with it, is what graduates it.
- [ ] 🧪 **Worktrees**: shipped for Claude Code, which owns the worktree itself. Codex has [no equivalent flag yet](https://github.com/openai/codex/issues/12862); when it lands, captain-miao needs one match arm.
- [ ] **More agent backends**: the per-session backend is an abstraction, so other coding agents (Kimi Code, opencode, Grok, …) can slot in alongside Claude Code and Codex.
- [ ] **More terminal backends**: the terminal layer is an abstraction (Kitty, zellij and tmux today), so other terminals and multiplexers (WezTerm, …) can slot in.

## License

MIT. See [LICENSE](LICENSE).
