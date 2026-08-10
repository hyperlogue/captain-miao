# Changelog

All notable changes to captain-miao are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-08-09

### Added

- **The dashboard can deploy its own server to a remote host.** Connecting to a
  host with no `miao-server` — or one built from a different version —
  used to be a dead end that told you to go install it yourself. A dashboard
  built to carry a server now pushes the right binary over the ssh connection it
  just opened, checks it actually runs there before putting it in place, and
  remembers what it deployed so the next connect doesn't repeat the work. When
  it *can't* help (no payload for that architecture, or the host refused the
  write) it says exactly that instead of a generic failure.

  This is opt-in, and it is one command: `cargo xtask dist` obtains the servers
  and writes them into the finished dashboard. It produces a plain `miao` alongside
  a `miao-bundle-linux` carrying servers for both Linux architectures (single-arch
  variants too, if your fleet is only one). A regular build is unchanged and
  costs nothing extra; bundling both arches costs about 7 MB. Prebuilt downloads
  (npm, GitHub Releases) are the plain build for now — bundling them waits on
  remote hosts leaving experimental.

  **Where the servers come from is up to you.** `--from build` (the default)
  cross-compiles them from the sources beside you, against an old glibc (2.28 —
  Debian 10, RHEL 8) so they run on machines far older than the one that built
  them. `--from release` downloads the ones a release published, so a bundled
  dashboard needs only `curl` and `tar` — no cross toolchain at all. Each release
  now publishes the servers as their own assets to make that possible, and
  `miao --version` prints the digest of everything a binary carries, which is what
  tells two builds of the same version apart.

  **The host decides which build it can run.** `uname` reports an architecture
  but never a libc, so rather than guess, the deploy offers a candidate and lets
  the host rule on it: the binary is staged, run there via a new `miao-server
  self-check`, and only moved into place if it passes. That check resolves a
  user account — the thing a static build silently cannot do on an LDAP/SSSD
  host, where a plain `--version` would pass and the first attach would then
  fail. A host that can't run the glibc build falls back to a static musl one,
  and what it accepted is remembered, so this runs once per host rather than
  once per connect. A `miao-server` already on the host's PATH is now kept when
  its *protocol* is compatible rather than only when its version matches
  exactly, and a daemon already running is left alone.

  **A dashboard that carries nothing for a host can still reach it.** Payloads
  resolve through a chain — `$CAPTAIN_MIAO_SERVER_<TARGET>`, then
  `$CAPTAIN_MIAO_SERVER_DIR`, then whatever the binary embeds, then a local
  cache — and, when all of those are exhausted, by downloading the published
  server for that target. The download asks first and a refusal is the default,
  so an ordinary release build can serve a NixOS or Alpine host it carries
  nothing for, and never leaves the machine without being asked.

- **Sessions that outlive their window** (`[launcher] pooled = true`, opt-in).
  By default a session *is* its terminal window and closing it ends the session.
  Pooled mode runs each of this machine's sessions in a local pty pool instead,
  with the window merely attached — so a session survives closing the window, a
  crashed multiplexer, and logging out, and a dashboard on another machine can
  attach to the very same ones. Meant for dev servers, not laptops: where nobody
  connects from elsewhere the pool buys no persistence and costs an extra hop,
  no scrollback replay on reattach, and one client at a time. Needs
  `miao-server` on `PATH`; without it the dashboard says so and keeps
  the default behaviour.

- **A Home Manager module.** `programs.captain-miao.enable = true` puts `miao`
  on your PATH, and `programs.captain-miao.server.enable = true` puts
  `miao-server` there — which is the entire setup a Nix host needs to be
  reachable from another machine's dashboard, since a server that host builds
  for itself runs where a deployed generic-glibc binary cannot even start. No
  systemd unit: the daemon starts on demand and exits when idle. (Set
  `users.users.<name>.linger = true` on NixOS if you want pooled sessions to
  survive your last logout.)

- **`Space H` — a default host** for new-session operations, the exact analog of
  `Space a`'s default backend, persisted and shown in the header once you have
  more than one host.

- **`Space s` — steal a session** from whatever client is attached to it, behind
  a confirm (skipped when nobody is actually there). The pool is one client at a
  time, so this is how you take a session back from a terminal you can't reach.
  Also available as `--force` on `miao-server attach` and
  `miao-client attach`.

### Changed

- **Release binaries are about 20% smaller.** Link-time optimisation is now on
  for release builds, taking `miao` from 7.6 MB to 6.6 MB and the same
  proportion off `miao-server`; dropping 11 crates from the dashboard's
  dependency tree (a regex engine pulled in by a log-filter syntax nothing used,
  and a terminal-colour chain for `--help`) takes it to 6.1 MB. LTO costs about
  90 seconds of build time; debug builds are untouched. Two visible
  consequences: `--help` is no longer coloured, and third-party crates no longer
  log at ERROR by default — set `RUST_LOG` if you want theirs.

- **Remote hosts: the whole feature is now implemented** (still behind the
  `remote` cargo feature until it's verified end to end against a real host).
  Restart and fork work on any host; windows you had open come back by
  themselves when a slept laptop or a dropped connection reconnects, while a
  session you detached with `D` stays detached; pins and mutes on a pooled host
  are stored by that host, so every dashboard watching it agrees and they
  survive a restart; and opening an attach or `w` window reuses the existing ssh
  connection, so it costs no second authentication. Design notes:
  [docs/remote-sessions.md](docs/remote-sessions.md).

- **`Space h` is a hosts panel, not a form.** Each host shows its live
  connection state, running/attached session counts, daemon version and latency,
  and gets a configurable emoji shown in the session table's Host column. There
  is no Save step to forget: adding a host connects it immediately, edits apply
  when you leave the row, and removal asks first. When something is wrong the
  panel now says *what* — "miao-server not found", "version mismatch
  (found 0.3.1, need 0.4.0)" — where the header used to show only a warning
  triangle, and **`l` opens that host's whole connection log**: every step of
  probe, deploy, daemon start and handshake, with the host's own replies quoted
  in full, for when a one-line reason isn't enough to explain the failure. The
  header carries a `☁` tally rather than a raw count — one number per bucket
  (healthy, failing, dialing), with empty buckets left out — so an all-green
  fleet reads as a single number and a problem announces itself by a number
  appearing.

- **`r` lists one host at a time** (named in the picker title, `Ctrl-h` to
  switch) instead of merging every host's resumable sessions into one list whose
  scope you had to infer. With that, **`b` (the cross-host browser) is gone** —
  the table covers running sessions and `r` covers resumable ones.

- **Remote rows you can't act on are hidden**, and a session still running on
  its host with no window here sorts to the bottom of the list with its own
  icon — though an approval prompt still floats to the top wherever it is.

- **A daemon no longer dies with your login session.** It rebinds its socket
  when systemd-logind takes the runtime directory away at logout, which used to
  wedge it permanently, and one transient error accepting a connection no longer
  tears down every session on the host. **Run `loginctl enable-linger` on any
  Linux machine hosting sessions** to avoid the outage entirely.

- **The commands are now `miao`, `miao-server` and `miao-client`** (were
  `captain-miao`, `captain-miao-server`, `captain-miao-client`) — you reach for
  the dashboard dozens of times a day, so it should be short enough to type
  without thinking. Only the executables were renamed: the project, the crates,
  the npm package (`@hyperlogue/captain-miao`), the release tarballs, and the
  `~/.config` + `~/.local/state` directories all keep the captain-miao name, so
  an upgrade moves no state and no config. Every subcommand follows the binary —
  `miao claude`, `miao codex`, `miao focus`, `miao hook`.

  The shared tab holding your sessions in the Stacked layout is renamed to match,
  from `cm:sessions` to `miao:sessions`. It is found by title, so a dashboard
  running against a terminal that still has the old tab simply creates the new
  one beside it; sessions in the old tab keep running and stay reachable with
  `Enter`. Close it once it empties.

  **Upgrading:** the old `captain-miao` command is gone rather than aliased.
  Re-run your installer (`cargo install --git …`, `npm i -g
  @hyperlogue/captain-miao`, or `nix run`) to pick up the new name, and update
  any Kitty keybind that calls it — `launch --type=background miao focus
  --window-id @active-kitty-window-id`. A `cargo install` upgrade leaves the old
  `captain-miao` binary behind in `~/.cargo/bin`; delete it so a stale build
  can't shadow the new one. Sessions already running keep hooks pointing at the
  absolute path of the binary that launched them, so restart them once the old
  path is gone (npm removes it; `cargo install` leaves it in place).

### Known limitations

- **Remote hosts over SSH are experimental and off by default.** The whole
  cross-host lifecycle is implemented, but it has not been verified end to end
  against a real remote host. Build with `--features remote` to try it; without
  it the dashboard is strictly local-only.
- **`t` (move window to tab) is unsupported on zellij**, which has no CLI to
  reparent a pane across tabs. The key reports it and the help entry is hidden.
- **The Linux dashboard binaries are glibc builds** (glibc 2.35, so Ubuntu
  22.04+, Debian 12+, RHEL 9+); on Alpine or a musl system, build from source.
  This is about the dashboard you run, not the hosts you reach: `miao-server` is
  published for musl too, and a host that can't run the glibc build is served
  the static one automatically.

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

[unreleased]: https://github.com/hyperlogue/captain-miao/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/hyperlogue/captain-miao/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/hyperlogue/captain-miao/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/hyperlogue/captain-miao/releases/tag/v0.2.0
