# Changelog

All notable changes to captain-miao are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.2.0]: https://github.com/hyperlogue/captain-miao/releases/tag/v0.2.0
