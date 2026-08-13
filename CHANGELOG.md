# Changelog

All notable changes to captain-miao are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Every prebuilt binary now carries a server.** `npx @hyperlogue/captain-miao`
  and the GitHub tarballs all ship with an x86-64 glibc `miao-server` embedded,
  so setting up a remote host that has nothing installed no longer waits on a
  download. Hosts on another architecture, or with no glibc, still fetch theirs
  at deploy time.
- **Release tarballs are named `miao-v<version>-<target>.tar.gz`**, matching the
  binary rather than the project. The old `captain-miao-v…` name is gone; npm
  installs are unaffected.
- **`miao-server` is 30% smaller**, which claws back most of what bundling one
  costs: the server now builds size-tuned and without the SQLite features it
  never uses, keeping the pty path at full optimization. Around 890 KiB off every
  download.

### Added

- **`miao-bundled-all-server-v<version>-<target>.tar.gz`** — a larger download
  carrying every published server (both arches, glibc and musl), for a mixed
  fleet or a machine that can't reach the network at deploy time.
- **`packages.captain-miao-bundle-small`** (Nix) — a bundled dashboard built for
  size throughout, 22.6% smaller than the equivalent regular bundle. Nix-only and
  opt-in: it trades optimization on the dashboard's own paths for bytes, which is
  a good deal when you build locally and never download anything.

## [0.3.0] - 2026-08-12

### Added

- **Remote dev is out of the experimental stage.** Federate several machines into
  one dashboard: each host runs its sessions in its own pty pool, so a dropped
  connection or a slept laptop detaches windows without touching the sessions,
  and reconnecting brings them back.

  - `Space h` opens a hosts panel showing each host's connection state, session
    counts, daemon version, latency, and CPU + memory, with `l` for its full
    connection log and `c` to suspend a host in place instead of deleting it.
  - The dashboard deploys `miao-server` to a host that hasn't got one, offering a
    build and letting the host prove it can run it; `cargo xtask dist` is what
    bundles the servers into a dashboard.
  - A host with no terminfo entry for your `TERM` is offered this terminal's,
    which stops sessions there silently falling back to `xterm-256color`.
  - `u` upgrades a host's server in place, and the panel says which hosts a
    restart would move.
  - Each host takes verbatim ssh `Options`, mostly for port forwards — they come
    up with the connection, come back after a reconnect, and go away with it.
  - `Space A` attaches a window to every detached session that is free to take.
  - `[launcher] pooled = true` runs *this* machine's sessions in a pool too, so
    they survive closing the window, a crashed multiplexer, and logging out.
  - `programs.captain-miao.server.enable = true` — a Home Manager module, and the
    whole setup a Nix host needs to be reachable from another machine's dashboard.

- 🧪 Add experimental support for **tmux**, as a third terminal backend alongside
  Kitty and zellij. Probe-verified against tmux 3.7b only; the documented ≥ 3.2
  floor is a claim, not a tested one.

- 🧪 Add experimental support for **git worktrees**: `Ctrl-g` in the new-session
  picker starts the session in a fresh one, which the agent creates and owns.

- **Misc**

  - `Space l` switches between one shared session tab and one tab per session.
  - The dashboard's own tab says how many sessions want you — `miao (2)`.

### Changed

- **The commands are now `miao`, `miao-server` and `miao-client`** (were
  `captain-miao`, …) — re-run your installer, and delete the old `captain-miao`
  binary if `cargo install` left one in `~/.cargo/bin`.
- **The shared tab holding your sessions is `miao:sessions`**, renamed from
  `cm:sessions`; sessions in an old tab keep running and stay reachable.
- **`r` lists one host at a time** (named in the picker, `Ctrl-h` switches), so
  the cross-host browser `b` is gone.
- **The resume picker loads off the UI thread** and shows the 50 most recent
  sessions on the default host, keeping your typed filter when the list lands.
- **Killing a remote session answers the keystroke** instead of freezing the
  dashboard for the ssh round trip.
- **Pooled sessions are created by their first attach**, so the agent's TUI probes
  a real terminal and Shift+Enter no longer arrives as a bare Enter.
- **Attach windows report their own end** rather than being found by polling the
  whole window tree, which on zellij cost about 20 ms per pane.
- **A host that is still dialing says so** instead of counting with the ones that
  are down.
- **The status glyphs are ordinary emoji**, so they no longer need a Nerd Font.
- **Keep-awake counts only sessions on this machine**, so a busy session on a
  remote host no longer keeps your laptop awake.
- **Release binaries are about 20% smaller** (LTO, and 11 crates dropped from the
  dependency tree) — `--help` is no longer coloured and third-party crates no
  longer log at ERROR.

### Fixed

- **The cursor stays on the session you selected** when something re-sorts the
  table underneath it.
- **A long-running session no longer freezes on macOS** when the periodic
  `$TMPDIR` sweep deletes its hook socket out from under it.
- **`w` works on a remote host whose login shell is fish.**
- **A failed window-tree snapshot no longer drops every window binding.**

### Removed

- **The mute flag (`m`).** A session you don't want to look at is one you scroll
  past; a `muted` left in your state files is ignored.

## [0.2.1] - 2026-08-02

### Added

- **Startup check for the terminal control channel.** Being *in* a supported
  terminal is not the same as being able to drive it. On Kitty, a remote-control
  setup that is missing `listen_on` or carries a mismatched password used to
  surface one failed action at a time, with kitty's raw error in the status line;
  captain-miao now proves the channel works before the dashboard takes over the
  screen, and a failure prints which half of the setup is wrong — plus the config
  block to fix it — while stderr is still visible.

### Fixed

- **Codex sessions on macOS track their transcript again.** Codex rows showed no
  context usage, never picked up the first-prompt title, and stayed green forever
  after an Esc-interrupt: the rollout watch matched the wrong spelling of a
  symlinked path, and macOS reports nothing at all for the way Codex writes (one
  long-held file descriptor, appended to for the whole session). Context tokens
  and interrupts now land with the event that caused them, and the watch parks
  itself while a session is idle, so a session sitting at rest costs nothing.
- **Codex no longer refuses to start with "local database appears to be
  damaged."** The synthetic `$CODEX_HOME` now repairs entries where a real file
  sits where a link into your real `~/.codex` belongs — what Codex leaves behind
  whenever it adds a new state file, which eventually split a SQLite database
  across the two homes.
- **Review-pending detection for commands containing a quote.** An `r3 watch`
  wrapped in something like `nix develop … --command bash -c '…'` was truncated
  when normalised, so the row sat at "Task" instead of "Review" — and every
  command wrapped that way collapsed onto a single entry in the learned
  long-running set.
- **`CLAUDE_CONFIG_DIR` is honoured** when locating Claude's home. An instance
  that relocates the agent's config dir no longer has the resume picker and the
  `b` browser listing every project from the real `~/.claude` instead.
- **`nix build` works again.** The build's source filter dropped `assets/`, so
  the logo the dashboard embeds at compile time was missing and the build failed.

### Changed

- **README rewritten around installation and configuration**, gathering the
  Cargo, npm, and Nix routes under one section and documenting every CLI
  subcommand and config key in tables.

### Security

- **The recommended Kitty setup is now scoped by an authorization script.**
  Pairing the remote-control password with a small `is_cmd_allowed` script shuts
  the in-terminal escape-code channel outright — the vector by which a shell
  running inside a Kitty window, including one on the far end of an ssh session,
  could otherwise drive your terminal — and confines even an authenticated
  request to the eight commands captain-miao actually issues. The README carries
  the script, and now says plainly that substituting your own password is what
  makes that check real: the built-in default is a published constant and
  authenticates nothing on its own.

## [0.2.0] - 2026-08-01

First public release.

(0.1.0 was never a usable release: its four per-platform binary packages reached
npm, but the launcher that resolves them did not, and no GitHub Release was ever
cut. 0.2.0 is the first version published as a complete set.)

### Added

- **The whole fleet at a glance.** Every agent session in one table — status,
  working directory, model, context usage, git branch, and a live transcript
  preview — so you can tell at a glance which session is working, which is
  waiting on you, and which has finished.
- **Claude Code and Codex.** Both agents are supported side by side, and rows of
  either kind mix freely in one dashboard.
- **Full session lifecycle from the dashboard.** Launch, resume, fork, and kill
  sessions without leaving it, with a filterable picker over recent working
  directories and path completion for new ones.
- **Status detection that distinguishes waiting from working.** Sessions blocked
  on your approval or an answer are flagged and sorted to the top; `s` jumps to
  the next one needing attention.
- **Background jobs split into Task and Server.** A build or test the agent is
  waiting on keeps the session green, grouped with the active sessions, and your
  machine awake. A dev server or watcher the agent parked does not — it goes
  yellow, sorts with the idle sessions, and flags itself for follow-up. Common
  dev servers are recognised out of the box, and anything unrecognised that runs
  past an hour is learned, so it is classified correctly from the first moment
  next time.
- **Review-pending detection for [r3](https://github.com/hyperlogue/r3).** When
  every background job a session is waiting on is an `r3 watch`, the agent is
  blocked on a human review rather than working, and the row says so.
- **Kitty and zellij backends.** captain-miao brings no terminal of its own — it
  drives the one you already run, so every session is a native window or pane.
- **Switchable session layout.** Stacked (all sessions in one shared
  `cm:sessions` tab, the dashboard being the switcher) or per-tab (one tab each,
  switchable with your terminal's own keys). `Space l` toggles it, and the choice
  persists.
- **Keep-awake.** Prevents the machine from sleeping while any session is still
  working (`caffeinate` on macOS, `systemd-inhibit` on Linux), with an indicator
  shown only while it is actively inhibiting.
- **Pin, mute, and follow-up flags**, plus per-directory icon and colour marks
  with a searchable emoji picker.
- **Configurable keybindings.** Every Normal-mode command is remappable from a
  `[keybinds]` table in `config.toml`, including the `Space`-leader ones.
- **Prebuilt binaries on npm and GitHub Releases** for macOS (Apple silicon and
  Intel) and Linux (x86-64 and arm64). `npx @hyperlogue/captain-miao` runs the
  matching native binary with no Rust toolchain and no runtime download.

### Security

- Session state is written owner-only — `0600` files under `0700` directories —
  because state records your prompt text, working directories, and session ids.
  A tree created by an earlier build is tightened in place on the next run.

### Known limitations

- **Remote hosts over SSH are experimental and off by default.** The full
  cross-host lifecycle is implemented but unverified against a real remote host,
  and restart and fork remain local-only. Build with `--features remote` to try
  it; without it the dashboard is strictly local-only.
- **`t` (move window to tab) is unsupported on zellij**, which has no CLI to
  reparent a pane across tabs. The key reports it and the help entry is hidden.
- **Linux binaries are glibc builds** (built against glibc 2.35, so Ubuntu
  22.04+, Debian 12+, RHEL 9+). musl/Alpine needs a source build.

[0.3.0]: https://github.com/hyperlogue/captain-miao/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/hyperlogue/captain-miao/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/hyperlogue/captain-miao/releases/tag/v0.2.0
