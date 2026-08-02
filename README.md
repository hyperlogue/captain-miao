<p align="center">
  <img src="assets/logo.jpg" alt="captain-miao logo" width="320">
</p>

# captain-miao

A TUI dashboard for managing multiple AI coding sessions running in the terminal emulator or multiplexer of your choice — for example, [Kitty](https://sw.kovidgoyal.net/kitty/) and [zellij](https://zellij.dev/).

When you run several agent sessions at the same time, it's hard to tell which
one is actively working, which one is waiting on your decision, and which one
has already finished. captain-miao watches every session and shows you the
whole fleet at a glance: status, working directory, context usage, and a live
preview. From there you can jump to any session, open a new one, fork one, or
kill one without leaving the dashboard.

Unlike herdr or cmux, captain-miao brings no terminal of its own. It drives the
Kitty or zellij you already run — every session is a native window or pane,
controlled through the terminal's own protocol — so it stays one small, focused
tool and the rest of your workflow is yours to compose.

## Highlights

- **The whole fleet at a glance:** every session in one table, with status, working directory, model, context usage, git branch, and a live transcript preview.
- **Never miss a prompt:** sessions waiting on your approval or an answer are flagged.
- **Full session lifecycle:** launch, resume, fork, and kill sessions from the dashboard, with a filterable picker for recent working directories.
- **[Claude Code](https://claude.com/claude-code) and [Codex](https://github.com/openai/codex)** today, behind a backend abstraction built to extend to other coding agents.
- **Keep-awake:** prevents your machine from sleeping while any session is still working (`caffeinate` on macOS, `systemd-inhibit` on Linux).
- **Pin, mute, mark:** pin important sessions to the top, mute the ones you don't need right now, and flag the ones to follow up on.

## Requirements

**To run:**

- A supported terminal: **Kitty** with remote control enabled (see [Kitty setup](#kitty-setup)), or **zellij** ≥ 0.44 (run captain-miao inside the zellij session; no extra setup needed).
- **Claude Code** and/or **Codex** on your `PATH`.

**To build from source:**

- **Rust** 1.88 or newer and a C compiler (for the statically-bundled SQLite used to read Codex session titles).

## Installation

### From npm (prebuilt binary)

The quickest route — no Rust toolchain, no build:

```sh
npx @hyperlogue/captain-miao          # run it once
npm install -g @hyperlogue/captain-miao   # or install the `captain-miao` command
```

`bunx @hyperlogue/captain-miao` works too. The npm package is a small launcher
that execs a prebuilt native binary shipped as a per-platform optional
dependency, so your package manager downloads only the one binary matching your
machine — nothing is fetched at runtime.

Prebuilt binaries are published for **macOS** (Apple silicon + Intel) and
**Linux** (x86-64 + arm64). The Linux builds are glibc, built against glibc 2.35,
so they run on Ubuntu 22.04+, Debian 12+, RHEL 9+ and similar. On musl/Alpine the
launcher says so and points you at a source build. There is no Windows build —
captain-miao needs Unix sockets, `$XDG_RUNTIME_DIR`, and Kitty or zellij.

The same binaries are attached to every [GitHub
Release](https://github.com/hyperlogue/captain-miao/releases) as a `.tar.gz` if
you'd rather download one directly. GitHub records a SHA-256 digest for each
asset, so you can verify a download without a checksum file:

```sh
gh release view v0.2.0 --repo hyperlogue/captain-miao --json assets \
  --jq '.assets[] | "\(.digest)  \(.name)"'
```

### From source with Cargo

```sh
git clone https://github.com/hyperlogue/captain-miao
cd captain-miao
cargo install --path .
```

Or build a release binary and put it on your `PATH`:

```sh
cargo build --release
# binary at ./target/release/captain-miao
```

### With Nix

A flake is provided:

```sh
nix build          # result/bin/captain-miao
nix run            # run the dashboard directly
nix develop        # dev shell with the pinned Rust toolchain
```

## Kitty setup

captain-miao drives Kitty via its remote-control protocol, so your `kitty.conf` must allow it. The recommended setup is password-scoped:

```conf
allow_remote_control password
remote_control_password "choose-your-own-secret"
listen_on unix:/tmp/mykitty
```

and the matching line in captain-miao's config:

```toml
[kitty]
rc_password = "choose-your-own-secret"
```

> **Pick your own password.** captain-miao's built-in default is `i-am-the-captain-miao` — fine to get started, but it is a published constant, so anything that can reach the socket already knows it. Setting a value you choose is what actually makes the password meaningful.

**Understand what you're enabling.** Kitty's remote control is a real privilege: a program that has it can read your terminal's contents, open windows, and run commands. Three levels are worth knowing apart:

- `allow_remote_control password` — only requests carrying a password from `remote_control_password` are honoured. Recommended, and what the config above sets up.
- `allow_remote_control socket-only` — refuses the in-terminal escape channel and only listens on the `listen_on` socket, so a process inside a kitty window (including a shell on the far end of an `ssh` session) cannot drive your terminal. Combine with `password` for both protections.
- `allow_remote_control yes` — allows *everything*, with **no password check at all**. captain-miao still sends its password, but kitty won't verify it. Simplest to set up; least protective.

captain-miao passes the password to `kitten @` out-of-band via an environment variable rather than on the command line, so it isn't visible in `ps` or `/proc/<pid>/cmdline`.

**The dashboard checks this at startup.** Before drawing anything it makes one real remote-control request, and if that fails it prints what is wrong (no `listen_on` socket, a socket from a kitty that has since restarted, a password kitty doesn't accept, a missing `kitten` binary) along with the config above, and exits. Failing there is deliberate: without remote control the dashboard cannot open, focus, preview, or move a window — and a password mismatch doesn't produce an error at all. Kitty responds to an unrecognised password by asking *you* to approve the request in its own window, so the request simply never returns; caught at startup that is a message, caught later it would be a frozen dashboard.

### Ring the dashboard from any session (optional)

Bind a Kitty key to focus the dashboard and flag the session running in the current window; its bell indicator lights up so you can find it again:

```conf
map ctrl+shift+c launch --type=background captain-miao focus --window-id @active-kitty-window-id
```

## Usage

Run the dashboard inside a supported terminal (Kitty or zellij):

```sh
captain-miao
```

> captain-miao must be launched from within Kitty or a zellij session; it exits with an error otherwise. When run inside a zellij session it auto-selects the zellij backend (override with `[terminal] backend` in the config).

From the dashboard, `o` / `O` start new sessions and `r` resumes existing ones. You can also launch sessions directly from a shell:

```sh
captain-miao claude .            # launch Claude Code in the current dir, with hooks
captain-miao claude --resume     # any extra args are forwarded straight to claude
captain-miao codex .             # launch Codex in the current dir, with hooks
captain-miao focus               # focus the running dashboard window
```

Sessions launched this way are wrapped by a _launcher_ process that injects the tracking hooks, so they show up in the dashboard automatically. Hooks are injected per-session and torn down on exit; nothing is written to your global `~/.claude/settings.json`.

### Key bindings

Press `?` in the dashboard for the complete list. Highlights:

| Key                            | Action                                                            |
| ------------------------------ | ----------------------------------------------------------------- |
| `j`/`k`, `↑`/`↓`, `Ctrl-n`/`p` | Navigate sessions                                                 |
| `gg` / `G`                     | Jump to top / bottom                                              |
| `1..9` / `Ctrl-1..9`           | Select Nth session / select and focus its window                  |
| `Enter`                        | Focus the selected session's window                               |
| `o` / `O`                      | New session (same tab / prompt for cwd)                           |
| `r` / `f`                      | Resume picker / fork (resume selected in place)                   |
| `b`                            | Browse every running and resumable session in one list            |
| `x`                            | Kill the selected session                                         |
| `s`                            | Jump to the next session needing attention                        |
| `m` / `p` / `i`                | Mute / pin / toggle needs-input on the selected session           |
| `y`                            | Copy the selected session id to the clipboard                     |
| `t` / `w`                      | Move window to tab (Kitty only) / switch to or open the cwd's work tab |
| `h`/`l`, `←`/`→`               | Scroll the preview horizontally                                   |
| `R`                            | Refresh the preview now                                           |
| `Space v` / `Space d`          | Toggle the preview / detail panel                                 |
| `Space i`                      | Edit the selected directory's icon + color                        |
| `Space e` / `Space E`          | Restart the selected / all idle sessions                          |
| `Space z`                      | Toggle keep-awake (inhibit OS sleep while sessions work)          |
| `Space a`                      | Set the default backend for new sessions (Claude / Codex)         |
| `Space l`                      | Switch session layout (stacked in one tab / one tab per session)  |
| `/`                            | Search                                                            |
| `q` / `Ctrl-c`                 | Quit                                                              |

Pressing `Space` (the leader) shows a which-key strip of the available follow-up keys in the footer.

In the cwd picker, `Ctrl-t` switches the backend for that one launch and `Ctrl-d` drops the highlighted recent directory.

**Custom keybindings.** Every Normal-mode command above is remappable via a `[keybinds]` table in `~/.config/captain-miao/config.toml`. Map a command id to a key (or list of keys); an empty list unbinds it:

```toml
[keybinds]
kill = "X"                      # move kill from x to X
jump_attention = ["s", "n"]     # bind two keys to one command
restart = "space r"             # remap a leader sequence
toggle_detail = []              # unbind a command
```

Keys parse forms like `"ctrl+u"`, `"O"` (= `"shift+o"`), `"space e"`, `"enter"`, `"f5"`, and arrow names. `Ctrl-c`, `g g`, and the `1..9` / `Ctrl-1..9` selectors are fixed.

### Session statuses

| Label                  | Meaning                                                                              |
| ---------------------- | ------------------------------------------------------------------------------------ |
| Starting               | Session launching                                                                    |
| Active                 | Agent is working on a turn                                                           |
| Compacting / Compacted | Context compaction in progress / just finished                                       |
| Task                   | Turn ended, but a short-term background job (build, test) is still running           |
| Server                 | Turn ended and a long-running service (dev server, watcher) was left running         |
| Review                 | Agent is parked waiting on a human code review                                       |
| Approval               | Waiting for you to approve a tool use                                                |
| Decision               | Waiting for you to answer a question                                                 |
| Idle                   | At rest, waiting for your next prompt                                                |
| Failed                 | The launch never produced an agent (e.g. blocked `direnv`, missing binary)           |

**Task vs Server.** Both mean "the turn ended but a background shell is still alive", and captain-miao tells them apart by what the command is. A build or test the agent is waiting on counts as work in progress: it stays green, sorts with the active sessions, and keeps your machine awake. A dev server or file watcher the agent parked and moved on from does not: it goes yellow, sorts with the idle sessions, and flags itself for follow-up. Common dev servers are recognised out of the box, and anything unrecognised that keeps running for more than an hour is **learned**, so it's classified correctly from the first moment next time.

**Review** is a refinement of the same idea for [r3](https://github.com/hyperlogue/r3), a local human↔agent review tool: when every background job a session is waiting on is an `r3 watch`, the agent isn't working — it's blocked on *you*. Those rows are surfaced as needing attention, and `s` jumps to them.

### Session layout

`Space l` switches how new sessions are placed, and the choice persists:

- **Stacked** (default) — every session goes into one shared `cm:sessions` tab, one visible at a time (a stack-layout tab on Kitty, full-size floating panes on zellij). The tab bar stays clean no matter how many sessions run, and the dashboard is how you switch between them.
- **Per-tab** — each session gets its own tab, visible in the tab bar and switchable with your terminal's own keys, at the cost of a crowded bar.

The layout applies to **new** sessions only; toggling it never moves a running one. Restart a session (`Space e`, or `Space E` for all idle ones) to migrate it into the current layout.

## Remote hosts over SSH (experimental)

captain-miao can federate remote machines: one local dashboard monitoring and managing sessions across several hosts, with remote sessions living in a per-host pty pool so they survive ssh drops, laptop sleep, and dashboard restarts.

> **This is a work in progress and is off by default.** The full lifecycle (open / resume / attach / detach / kill / browse across hosts) is implemented, but it has not been verified end-to-end against a real remote host, and restart and fork remain local-only. Build with the `remote` feature to try it:
>
> ```sh
> cargo build --release --features remote
> ```
>
> Without the feature the dashboard is strictly local-only: `hosts.json` is never read, no remote connection is ever opened, and `Space h` reports that the feature is unavailable.

With it enabled, `Space h` manages the host list, a **Host** column appears in the table, and `o` / `r` / `b` operate across every configured host. This needs `captain-miao-server` on the remote host and key- or agent-based ssh auth (captain-miao runs ssh in `BatchMode`, so it never prompts for a password). The design is written up in [docs/remote-sessions.md](docs/remote-sessions.md).

## Configuration

Optional TOML config at `~/.config/captain-miao/config.toml` (or `$XDG_CONFIG_HOME/captain-miao/config.toml`). All keys are optional and fall back to sensible defaults; an unparseable file falls back to defaults rather than crashing. A few of the available sections:

```toml
[terminal]
backend = "kitty"   # or "zellij"; unset auto-detects (zellij when run inside a zellij session, else Kitty)

[kitty]
rc_password = "choose-your-own-secret"   # must match remote_control_password in kitty.conf

[launcher]
default_agent = "claude"   # backend for new sessions: "claude" or "codex"

[thresholds]
context_warning_tokens = 175000
context_critical_tokens = 400000
preview_stale_secs = 20

[polling]
preview_auto_refresh_secs = 10   # 0 disables

[colors.ui]
title_fg = "cyan"
highlight_bg = "dark_gray"

[debug]
enabled = false   # or set CAPTAIN_MIAO_DEBUG=1
```

Colors accept named values (`cyan`, `dark_gray`, …) or `#rrggbb` hex. See `src/config.rs` for the full set of options.

## How it works

captain-miao is built around a strict unidirectional data flow:

- The **launcher** wraps each agent process and is the single source of truth for that session's state. It receives hook events over a Unix socket and writes a JSON state file.
- **Hooks** are thin forwarders: they parse the agent's hook payload from stdin and send it to the launcher socket.
- The **dashboard** is a pure viewer. It watches the session state directory and per-backend transcript dirs with `notify` (FSEvents on macOS, inotify on Linux) and re-reads files when they change. It performs no IPC of its own.

State lives under `~/.local/state/captain-miao/` and runtime sockets under `$XDG_RUNTIME_DIR/captain-miao/`, both owner-only — session state files record your prompt text, so they are written `0600` under a `0700` directory. For a deeper tour of the architecture, module layout, hook wiring, and data files, see [AGENTS.md](AGENTS.md).

## Development

captain-miao is a Cargo workspace of four packages: the `captain-miao` dashboard
(root package), the `captain-miao-server` per-host daemon + pty pool
(`crates/cm-server`, built for and deployed to remote hosts), the
`captain-miao-client` pool CLI (`crates/cm-client`, lists and attaches to local
pooled sessions), and the shared `cm-core` library. See
[docs/crate-split.md](docs/crate-split.md).

```sh
cargo run                # run the TUI dashboard
cargo run -- claude .    # launch Claude in the current dir with hooks
cargo run -- codex .     # launch Codex in the current dir with hooks
cargo test --workspace   # run the full test suite (all four packages)
cargo build --release -p captain-miao-server   # build the remote-host daemon
cargo watch -x run       # auto-reload the dashboard on changes
```

CI runs `cargo fmt --all --check` and `cargo clippy --workspace --all-targets
--all-features -D warnings`, so run both locally before you commit.

### Cutting a release

`Cargo.toml`'s `[workspace.package] version` is the single version source — every
npm package version and pin is stamped from it. Bump it, commit, then tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

That drives `.github/workflows/release.yml`, which builds all four targets,
publishes a GitHub Release with the tarballs, then publishes to npm — the four
per-platform binary packages first, then the `@hyperlogue/captain-miao` launcher
that pins them. A tag that isn't plain SemVer, or that disagrees with
`Cargo.toml`, fails the run in seconds before anything is built, and every
publish step is idempotent so a re-run after a transient failure converges
instead of double-publishing. A SemVer prerelease tag (`v0.2.0-rc.1`)
is marked prerelease on GitHub and goes to npm's `next` dist-tag, so `latest`
never resolves an RC.

Publishing needs an `NPM_TOKEN` repository secret — an npm automation token with
publish rights on the `@hyperlogue` scope. The publish job runs in the `release`
GitHub environment: configure it with required reviewers (and add a ruleset on
`v*` tags), since a tag push otherwise bypasses branch protection straight into a
signed publish. Node is pinned to 24 so npm supports Trusted Publishing — linking
the packages to this workflow on npmjs.com lets you drop the `NPM_TOKEN` secret
entirely in favour of OIDC.

## License

MIT — see [LICENSE](LICENSE).
