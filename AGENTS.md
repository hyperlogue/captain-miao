# captain-miao

TUI dashboard to monitor and manage multiple Claude Code sessions in the Kitty and zellij terminal emulators.

## Architecture

Unidirectional data flow:

```
Claude Code hook → captain-miao hook → launcher (Unix socket)
                                            ↓
                                    writes state file (JSON)
                                            ↓
                              dashboard (notify watcher) reads files
```

- **Launcher** (`src/launcher.rs`) is the single source of truth for session state. Wraps the agent process, receives hook events, writes state files. Per-process: each launcher hosts exactly one agent session.
- **Dashboard** (`src/app/`) is a pure viewer. Watches `~/.local/state/captain-miao/sessions/` (the launcher state files) plus each backend's `watch_paths()` (Claude's session-file dir, Codex's title-store WAL) via `notify` (FSEvents on macOS, inotify on Linux), re-reads state files on change. It does **not** read transcripts: context tokens, model, and first-prompt auto-title are folded from the transcript by the launcher (which already watches it) and stamped onto the state file, and the **session name** lands on `LauncherState.name` by one of two paths — Claude's launcher folds the `/rename` from the agent's own session file (`~/.claude/sessions/<pid>.json` `name`), while Codex's sqlite title (rename or auto-title) is overlaid by the **per-host** `LocalBackend` as sessions are served (one throttled reader per host — see **Codex hooks → Session names**) — so the dashboard reads everything straight off `LauncherState`. No IPC. The row's title is `name` when present, otherwise the folded **first prompt** (`session_display_name` precedence: `name → resume-index → first_prompt → random`). For Claude the two are told apart by the session file's `nameSource`: Claude marks its own auto-derived `project-name-hash` slug `nameSource:"derived"` and drops the field on a `/rename`, so `read_session_name` returns only the rename and lets the auto slug fall through to the first prompt (a better title than `captain-miao-da`). (Because the name rides `LauncherState` rather than the transcript's `custom-title` line, it reaches *remote* rows for free; the transcript `custom-title` parse survives only for the dormant-session resume list, which has no live session file.)
- **Hooks** (`src/hooks.rs`) are thin forwarders — parse the agent's stdin JSON, send to the launcher socket.
- **Terminal abstraction** (`src/terminal/`) is the per-emulator backend dispatch for window/tab control. The `Terminal` trait (`mod.rs`) is the set of irreducible primitives — `snapshot`, `spawn`, `focus_window`, `focus_tab`, `close_window`, `capture_text`, `move_window_to_tab`, `current_window`, plus one `capabilities()` query — keyed by opaque `WindowId`/`TabId` (string newtypes). A `Window` is now **just an id**: the old cwd-based window lookup and the `kitten @ ls` cwd/foreground-process parse are gone, so no caller derives a window from its directory or child processes anymore. Everything still derivable from the window tree (`window_tab_map`, `list_tabs`) is a pure function over a `snapshot()` in `mod.rs`, so the policy is written once and unit-tested without a backend. The **launcher never snapshots** — window/tab lookup is presentation-only and a launcher may be headless/remote, so the dashboard resolves each local session's display-only `tab_id` from its own snapshot via `window_tab_map` (cached, refreshed lazily only when a local window is unresolved — and a spawn **seeds** the cache from its own `SpawnResult.tab` when the backend reports one, so opening/restarting a session doesn't make the next reload snapshot for a fact the spawn already knew; a later full snapshot still replaces the whole map, so a seeded entry can't outlive the truth). The **dashboard owns every session↔window binding** (`docs/remote-sessions.md` §3.2): it mints a `--launch-id` token onto each local spawn (the analog of remote `--pool-session`), the launcher echoes it onto `LauncherState.launch_id`, and `App::window_id_for_session` resolves the row's token through `WindowBindings` (persisted to `window-bindings.json`, re-seeded on startup, also read by the external `focus` bell). The launcher self-reports its own `window_id` (`current_window`, an RC-free env read) **only when no token is set** — a hand-launched `captain-miao claude`, where the resolver falls back to that field; a token-bearing (dashboard/pooled) launcher never touches the terminal (it does always stamp `LauncherState.terminal`, though — the identity classifies the row either way). Every persisted window id is **namespaced by the terminal instance that minted it**: kitty window ids and zellij pane ids are overlapping small integers, so `LauncherState.terminal`, `WindowBinding.terminal`, and the `dashboard-window-id` prefix all carry `zellij:<session>` / `kitty:<socket|pid>` — one cm-core env read derives (identity, window id) together so they can't disagree, while the dashboard's own identity is the active **backend's** `identity()` (the instance it *drives*, so the nested-zellij `backend = "kitty"` override classifies its kitty-spawned sessions as its own). A local row stamped with a different instance is **foreign**: drawn dimmed with its terminal named, window ops inert (`x` still kills by pid), its bindings held out of the in-memory map — never resolved, pruned, or reaped here — and carried verbatim through every `window-bindings.json` rewrite, so switching the dashboard between terminals loses nothing; the `focus` bell matches only same-identity bindings and declines to drive a dashboard window recorded in another terminal. `terminal::get()` returns the process-wide backend (lazy `OnceLock`, mirrors `config::get()`), picking it **zellij-first**: an explicit `[terminal] backend` override wins, else a live `ZELLIJ_SESSION_NAME` selects zellij, else Kitty is the status-quo fallback (`detect_backend` is a pure function — the env reads stay at the `get()` edge so the precedence is unit-tested). Zellij must **beat the ambient Kitty env**: a nested zellij (running inside Kitty) leaks the outer `KITTY_WINDOW_ID` into every pane, so a Kitty backend would drive the wrong (outer) window — only an explicit override overrides that. The **same argument moves cm-core's `current_window()` to read `ZELLIJ_PANE_ID` first** (then `KITTY_WINDOW_ID`), so a launcher self-reports its own zellij pane, not the shared outer Kitty window. Two backends implement the trait: **Kitty** (`src/terminal/kitty.rs`) wraps `kitten @` remote control; **zellij** (`src/terminal/zellij.rs`) wraps one `zellij action` subprocess per call, pinned to the `ZELLIJ_SESSION_NAME` captured at startup (min zellij 0.44). The **capability seam** is the single `capabilities()` method returning a `Capabilities` struct (Kitty's answer by default; backends opt in/out per field) — a new backend limitation becomes a new field there, not another trait method. The dashboard calls it exactly once, at startup (`App::new` caches it as `App.capabilities`); every consumer reads that cache. `move_to_tab`: zellij has no CLI to reparent a pane across tabs (`BreakPane` is keybind/plugin-only), so the dashboard hides the `t` affordance rather than offer a key that only errors. The **session arrangement** is a runtime-switchable **layout mode** — `SessionsLayout { Stacked, PerTab }` (`Default = Stacked`) — crossed with the backend's `window_stacking`/`floating_sessions` capabilities by `resolve_spawn_target` (pure, tested), now keyed on `(capabilities, layout)` **only**: the old per-session window anchor is retired, along with `SpawnTarget::AdjacentTo` and the `target_window`/`fallback_anchor` plumbing that threaded it through `launch_agent`/`restart_one`/`RestartAll`/the workdir picker/`Action::NewSessionSplit`/`Action::ResumeSession`. The layout is a **spawn-time policy applied to new sessions only** — toggling it (`Space l`, persisted — see Key bindings) never relocates a running session; migration is automatic on restart, since `Space e`/`Space E` respawn each session into the current layout (there is no live-reflow code). **Why new-sessions-only, not live reflow:** zellij can't move a running pane between tabs (no `break-pane`/reparent CLI — `move-pane` is within-tab, `toggle-pane-embed-or-floating` has no `--tab-id`), the same root cause as `t`/move-to-tab being unsupported there, so rather than offer live reflow on Kitty alone the design keeps both backends symmetric: spawn-time policy + restart-to-migrate. **Per-tab** spawns every session into a fresh `NewTab` (per-project tab title) on both backends. **Stacked** (the default, today's behavior) consolidates every session into one shared `cm:sessions` tab, one visible at a time: on Kitty each session joins a single global `cm:sessions` **stack-layout** tab (`SpawnTarget::SharedStackTab` — Kitty's analog of the zellij-only `Floating`, "the shared `cm:sessions` stack tab, created if absent"), found by title on each spawn (one `kitten @ ls`), created with `--tab-title=cm:sessions` + `goto-layout stack` on first use and joined via `launch --type=window -m window_id:<w>` (selecting the shared tab by a window it contains; the new window stacks into its existing stack layout) after — this replaced the old per-project stack tabs (`AdjacentTo`), so Stacked is now symmetric with zellij (one global tab regardless of project); on zellij each session is a **full-size borderless floating pane** in that tab (`SpawnTarget::Floating`). All session panes sit at identical `100%×100%` geometry (probe-verified on 0.44.3: borderless floating gets the exact full-viewport pty a lone tiled pane would, `100%` is re-derived on client resize with one clean pty resize, and hiding the floating layer never resizes), so the z-order top is the visible session and **switching is a pure raise**: `focus_window` is a single `focus-pane-id` (~18ms) that switches tabs, shows the layer, and raises the pane in one action; focusing an embedded pane auto-hides the layer. **Spawning is blink-free**: `new-pane --floating --tab-id <sessions-tab>` into the non-active sessions tab moves nothing — not the client, not the layer, not even the floating focus — and prints the pane id, so no `list-panes` recovery. The sessions tab is found **by title** on each spawn (one cheap `list-tabs`; an id could be recycled onto an impostor — see the tab-counter note below) and created on first use — the only client blink, snapped back immediately, once per zellij session. The arrangements this replaced all scaled badly: one-tab-per-session crowded the tab bar; native stacked panes cost a title-bar row per collapsed session and a real pty resize on every expand (a slow agent repaint per switch); the fullscreen emulation before that flickered (`toggle-fullscreen` acts on a tab's *focused* pane and focusing moves the client, so a background tab can't be pre-arranged). A related performance cliff: **`list-panes` costs ~20ms per pane server-side** (measured ~475ms at 22 panes and ~780ms at 30, vs ~18-40ms for every other action), so it stays off hot paths — never on focus, session spawn, or **restart**, only where unavoidable (snapshot, pane-id recovery after a `new-tab` work-tab spawn — best-effort: once the tab exists the spawn never errors, a failed recovery just returns no window id). This is the whole reason `restart_one` closes the old window unconditionally instead of first checking a snapshot for it: the check cost a `list-panes` and guarded nothing (an id being *present* is exactly what a recycled id looks like too — the real guard is the `kill_old` flag, false on every path where recycling is possible), and closing an already-gone pane is a silent no-op on zellij / an ignored error on kitty. The other zellij quirks the backend encodes: plugin panes share the pane-id namespace with terminal panes (always filtered on `is_plugin == false`, and addressed to the CLI as `terminal_<n>`); pane commands inherit the zellij *server*'s environment, not the caller's, so an `Exec` argv is wrapped in `/usr/bin/env PATH=<dashboard PATH> …`; an exited command pane is held open by default, so `hold: false` maps to `--close-on-exit` — and the held pane of a session that dies *without* a clean kill (crash, SIGKILL) is reaped by the dashboard on row removal, since on this backend it would otherwise pile up invisibly under the `cm:sessions` floating stack (kitty keeps such windows visible as crash forensics; only dashboard-created windows are ever reaped — a hand-launched row's own pane is never closed); there is no `--dont-take-focus`, so a `new-tab` that shouldn't keep focus (`take_focus: false`) — the `cm:sessions` tab created on the first floating spawn inside `ensure_sessions_tab`, and, in **Per-tab** layout, every session spawn (a background session mustn't yank the client to it) — snaps back to the dashboard's own pane (`ZELLIJ_PANE_ID`) immediately, keeping the client's visible dwell to one ~20ms action (work tabs deliberately take focus — `take_focus: true`, `w` means "go there"; fully focus-less tab creation exists only in the wasm-plugin API); and `focus-pane-id` on an already-focused pane exits non-zero ("already focused"), which `focus_pane` treats as success. (Zellij's tab counter is max-plus-one over *live* tabs — probe-verified — so a closed highest tab's id is recycled onto the next tab created; pane ids never recycle. The sessions-tab lookup guards against this with a title check; the work-tab map validates title **and** the pane the spawn created, so a recycled tab id wearing the same basename title still fails.)
- **Agent abstraction** (`src/agent.rs` + `src/agents/`) is the per-session backend dispatch. `AgentControl` is an enum carrying which backend each session uses (`Claude`, `Codex`). The dashboard mixes backends per-row: every per-row lookup dispatches through `state.agent`, and `AgentControl::ALL` is iterated for watch paths / session-index / resume listing.

## Module layout

captain-miao is a **Cargo workspace** with four members. The split keeps the
portable logic in a library all binaries link, the ratatui client in one binary,
the libshpool-hosting daemon in another that cross-compiles to Linux and is
deployed to remote hosts, and a small pool client in a fourth. Full rationale in
`docs/crate-split.md`.

- **`cm-core`** (`crates/cm-core/`) — the logic + data all binaries share. No
  ratatui/crossterm (presentation) and no libshpool (the pool), so it stays a
  portable data/logic layer that cross-compiles cleanly as part of the server.
- **`captain-miao`** (root package, `src/`) — the dashboard: the ratatui TUI
  client. Also carries `claude`/`codex`/`hook` (a local launch needs only this
  one binary) + `focus`. No pty pool.
- **`captain-miao-server`** (`crates/cm-server/`) — the headless per-host daemon
  + pty pool a remote dashboard reaches over ssh. The binary built for Linux and
  deployed to remotes; it **hosts** the pool (feature `pty-pool`, default on).
- **`captain-miao-client`** (`crates/cm-client/`) — a thin user-facing CLI over
  the *local* pool socket: `list` the daemon's sessions and `attach` to one. The
  only other crate that links libshpool (for the in-process attach), but it hosts
  no daemon/pool — a pure client. `--no-default-features` drops libshpool →
  list-only (attach declines), so it still builds on macOS.

**cm-core (`crates/cm-core/src/`):**

- `state.rs` — shared types (`LauncherState`, `SessionStatus`, `HookEvent`, `HookMessage`), path utilities, bell-flag plumbing
- `protocol.rs` — wire protocol for the remote path: length-prefixed JSON `ClientFrame`/`ServerFrame` (handshake + subscription + request/response) + async codec
- `agent.rs` — `AgentControl` enum + the generic types it dispatches to (`SessionIndex`, `ResumeCandidate`, `TranscriptScan`)
- `agents/claude.rs` — Claude Code backend: transcript parsing (incremental stats/title/first-prompt fold), hook payload mapping, launch command
- `agents/codex.rs` — Codex CLI backend: rollout JSONL parsing, synthetic `$CODEX_HOME` launch, hooks.json generation, hook payload mapping (carries the bundled-SQLite Codex-title read)
- `launcher.rs` — wraps the agent process, hook-event handler, state file writer, transcript watcher
- `hooks.rs` — hook event forwarder (stdin JSON → launcher socket)
- `backend.rs` — `LocalBackend` (the **server-core**: reads state files, overlays Codex titles per-host, lists resumable, signals local agents, plans launch argv, answers host-fs queries) + the shared `OpenSpec`/`LaunchPlan` seam types the dashboard's `Backend` enum and the wire protocol both use
- `terminal.rs` — the opaque `WindowId`/`TabId` ids (serialized into state + on the wire) + the launcher's `current_window()` self-report; the `Terminal` trait, Kitty backend, and snapshot policy stay in the dashboard
- `config.rs` — the `[launcher]`/`[debug]` config sections + the loader; the dashboard's presentation config layers on top, parsing the same file
- `cli.rs` — shared arg helpers (`split_cwd`, the `--pool-session`/`--launch-id` extractors) + the `claude`/`codex`/`hook` entrypoint bodies both binaries dispatch
- `logging.rs` — tracing setup (`init_tracing`)

**captain-miao dashboard (`src/`):**

- `main.rs` — CLI entrypoint with clap: TUI (default), `claude`, `codex`, `hook`, `focus`
- `app/` — Ratatui TUI: `mod.rs` (App state + event loop wiring, incl. the `REMOTE_ENABLED` feature gate), `run.rs` (entry + main loop), `draw.rs` (rendering), `keys.rs` (key + mouse dispatch), `keymap.rs` (configurable Normal-mode keybindings), `picker.rs` (telescope-style filterable picker), `format.rs` (text/color helpers, incl. the status→color mapping), `hosts.rs` (remote-host config + `hosts.json`), `bindings.rs` (the client-side window↔session `WindowBindings` map), `logo.rs` (the kitty-graphics header paw + walking cat), `keybind_log.rs` (debug-mode TSV log), `tests.rs`
- `backend.rs` — `Backend` enum (per-host session management): `Local` (wraps `cm_core::backend::LocalBackend`) and `Remote` (mirrors a `captain-miao-server daemon` over a socket / ssh-forward via `Transport`) + the ssh transport + remote-binary resolution (a **read-only** probe choosing between a `captain-miao-server` on PATH and one at the cache path; the old same-arch auto-upload was removed with the crate split — see the note above `REMOTE_CACHE_REL`). The dashboard aggregates `App.backends` (localhost #0 + one per remote host); see Remote hosts below and `docs/remote-sessions.md`.
- `terminal/` — terminal-emulator backend: `mod.rs` (`Terminal` trait + `Tab`/`Window`/`TabInfo`/`SpawnSpec` types + pure policy fns + `get()`'s zellij-first backend detection, re-exporting the id types from cm-core), `kitty.rs` (Kitty `kitten @` backend), `zellij.rs` (zellij `zellij action` backend), `graphics.rs` (kitty graphics-protocol primitives backing `app/logo.rs`), `tests.rs` (pure-policy tests)
- `config.rs` — the presentation config (colors, ui, thresholds, polling, keybinds) layered on cm-core's loader
- `sleep.rs` — OS-sleep inhibitor (caffeinate / systemd-inhibit)

**captain-miao-server (`crates/cm-server/src/`):**

- `main.rs` — CLI entrypoint with clap: `daemon` (`ensure`/`print-path`/`status`/`stop`), `claude`/`codex`/`hook` (launchers running inside the pool), `attach`, `pty-daemon`. Headless — no Kitty gate.
- `server.rs` — the daemon: the single persistent per-host process. Hosts the pty pool (libshpool on a thread) **and** wraps a `LocalBackend` to answer the protocol (snapshot + delta subscription, `ListResumable`/`KillSession`/`OpenSession`, host-fs queries `ListRecentDirs`/`CompletePath`/`CheckDir`). Self-daemonizing (double-fork + setsid), singleton (`server.pid`), auto-exits when idle. Dispatched in `main()` pre-runtime (daemonize + pool thread precede tokio).
- `pty_pool.rs` — the libshpool `pty-daemon`/`attach` entrypoints
- `server_pool.rs` — starts launchers inside the pool (`open_in_pool`), records the remote cwd

**captain-miao-client (`crates/cm-client/src/`):**

- `main.rs` — CLI entrypoint with clap: `list` (default) / `attach <name>`. Synchronous (no tokio) — `attach` calls `libshpool::run`, which must precede any thread.
- `pool.rs` — the pool client: a read-only session `list` that speaks libshpool's msgpack wire codec directly (its protocol client is private, so no libshpool needed for listing — just `shpool-protocol` + `rmp-serde`), enriched by joining each session against the `LauncherState` whose `pool_session` matches (agent/cwd/title); plus `attach`, guarded to only reattach an existing, currently-detached session (via the same list), proxying the pty through `libshpool::run` (feature `pty-pool`). The pool socket path is `cm_core::state::pool_socket_path` — shared with the server so it can't drift.

## Data files

State (`~/.local/state/captain-miao/`) — the tree is **owner-only**: directories
are created `0700` and JSON is written `0600` (`state::create_dir_all_private` /
`write_json_atomic`), because a state file records the user's own prompt text
(`first_prompt`/`last_prompt`), cwds, and session ids, which the ambient umask
would otherwise leave world-readable at `0755`/`0644`. A tree created by an older
build is tightened in place on the next run (`harden_dir`), and `write_json_atomic`
creates its `.tmp` staging file `0600` so the visible file is never briefly
readable. Pinned by `state_dirs_and_files_are_owner_only`.

- `sessions/{pid}.json` — launcher state files (atomic write via temp+rename)
- `sessions/bell-{pid}.flag` — bell sentinels dropped by `captain-miao focus --window-id`
- `dashboard.pid` — dashboard singleton lock
- `dashboard-window-id` — for the `focus` command, written as `<terminal-identity>|<window-id>` so a focus process in another terminal declines instead of driving a same-numbered foreign window
- `window-bindings.json` — the dashboard's persisted window↔session bindings (`window_id`, `host`, `launcher_pid`, `token`, `terminal`); same-terminal entries are validated/pruned against live rows, foreign-terminal entries are carried verbatim so a dashboard backend switch loses nothing
- `dashboard-overrides.json` — pin/mute/follow-up flags + the prevent-sleep toggle + the default new-session backend (`Space a`) + the session layout (`Space l`, Stacked / Per-tab), persisted across restarts
- `dashboard-sessions.json` — last-seen restartable session snapshot (incl. each session's pin/mute/follow-up flags, re-adopted on crash-recovery restart); presence at startup signals an unclean previous exit
- `recent-cwds.json` — recent cwds shown in the workdir picker
- `directory-marks.json` — user-set per-cwd icon + color overrides
- `work-tabs.json` — dashboard-owned `(host, cwd) → (tab, spawned pane)` map for the work tabs `w` opened, re-seeded on startup so `w` returns to an existing work tab (the terminal keeps it alive across a dashboard restart) instead of spawning a duplicate; validated lazily against a live snapshot on use — tab id, basename title, and pane-in-tab (pane ids never recycle) — so a stale entry self-heals (safe to delete)
- `codex-home/` — shared synthetic `$CODEX_HOME` for Codex sessions: symlinks every entry of the real `~/.codex` plus captain-miao's own `hooks.json`. **`config.toml` is a writable copy, not a symlink** — the launcher writes the pre-seeded hook trust (`[hooks.state]`, see Codex hooks → Trust) into it, which fails if it points at a read-only file (e.g. a nix-store / home-manager symlink). The copy is reseeded from the real config only when the real one changes (tracked via `.config-source.toml`), and the hook-trust `[hooks.state]` is (re)merged on top every launch, so the user's config edits propagate while trust stays current. The mirror pass also **repairs shadow entries** — a real file/dir sitting where a symlink belongs, which is what Codex leaves behind whenever it adds a new state file: it creates the file *inside* the synthetic home before that name exists in the real one, and the two copies then diverge permanently. The failure this prevents is a split-brain SQLite DB (stale synthetic `goals_1.sqlite` against `-wal`/`-shm` symlinks into the real home, once it grew them), on which Codex refuses to start at all — "local database appears to be damaged". Created/refreshed by the launcher; safe to delete (regenerated on next Codex launch).
- `long-running-commands/` — the self-learning set of background commands observed to run past the long-running threshold (1h), one file per normalized command (hashed name, contents = the command for grep-ability). Written by any launcher that learns a command; read by every launcher to classify a `run_in_background` shell as an at-rest `BackgroundServer` vs a busy `BackgroundActive` (see Background-job states). Safe to delete — it just re-learns.
- `logs/launcher-{pid}.log` — per-launcher tracing
- `logs/debug.log` — shared verbose log (only when `[debug] enabled = true` or `CAPTAIN_MIAO_DEBUG=1`); each process writes a `===== ROLE START pid=… =====` separator on startup
- `logs/keybinds.log` — TSV of every dashboard keystroke for frequency analysis (debug mode only)

Runtime (`$XDG_RUNTIME_DIR/captain-miao/`):

- `launchers/{pid}.sock` — launcher Unix sockets
- `launchers/{pid}-settings.json` — per-session hooks settings written before the agent spawns

## Kitty requirements

When captain-miao runs under the **Kitty** backend, the user's `kitty.conf` must allow remote control. The setup the README recommends is password-scoped:
```
allow_remote_control password
remote_control_password "choose-your-own-secret"
listen_on unix:/tmp/mykitty
```

`[kitty] rc_password` in `config.toml` must match; it defaults to `i-am-the-captain-miao`. **That default is a published constant, so it authenticates nothing** — the README tells users to choose their own, and this is worth repeating anywhere the setup is documented. Note also that `allow_remote_control yes` disables the password check entirely (kitty honours every request regardless of what captain-miao sends), and that `socket-only` is what shuts the in-terminal escape channel — the vector by which a shell inside a kitty window, including one on the far end of an ssh session, could otherwise drive the terminal. The password itself reaches `kitten @` via `--password-env` + an env var, never argv, so it stays out of `ps` / `/proc/<pid>/cmdline` (`src/terminal/kitty.rs`).

**The dashboard verifies the channel before it starts** — the second half of the startup gate in `main`, after `supported_terminal_present`. Detection picks the backend, then `terminal::verify_control()` asks *that* backend to prove it can drive its instance (`Terminal::verify_control`): a no-op default, which is zellij's answer (its CLI is trusted by the session it runs in — nothing to misconfigure), and on Kitty one `kitten @ ls` — the cheapest read-only rc command, and the same call `snapshot` makes, so a pass means every rc path has a working transport. A failure prints an actionable diagnosis (which of `listen_on` / the password / a restarted-kitty socket / a missing `kitten` binary is wrong, plus the config block) and exits 1. `diagnose` is pure over a `ProbeOutcome`, so the message classification is unit-tested without a broken kitty to reproduce against. Three things this design turns on, none optional: (1) **it must exit, not warn** — a dashboard that can't reach kitty cannot spawn, focus, preview, or move a window, so it has no degraded mode worth entering; (2) **the probe imposes its own 3s timeout**, because a password kitty doesn't accept produces *no reply at all* — kitty asks the user to approve the request in its own window instead (verified against kitty 0.47), so the request never returns, which is also why `kitten_cmd` has no timeout of its own to lean on and why a mismatch caught late would be a frozen loop rather than an error; (3) the timeout is enforced by dropping the future, so `kitten_command` sets **`kill_on_drop(true)`** — without it the probe would leave an orphaned `kitten` blocked on that prompt. Only the dashboard pays the probe: a launcher never touches the terminal (it self-reports its window from the env, and a hand-launched `captain-miao claude` must keep working in a kitty with no `listen_on`), and `focus` is a single rc call whose own error already reaches stderr.

## Zellij requirements

captain-miao drives zellij through its `zellij action` CLI, so it needs **zellij ≥ 0.44** — the release that added the pieces the backend relies on: JSON output from `list-panes`/`list-tabs` (`-j`), `focus-pane-id`, `dump-screen --pane-id --ansi`, `go-to-tab-by-id`, a `new-tab` that prints the new tab's id, and a `new-pane` that prints the new pane's id and takes `--tab-id`, `--floating` with `--x/--y/--width/--height` geometry, and `--borderless` (the floating-session spawn; `--stacked`, the primitive behind the now-retired `AdjacentTo` target, is kept faithful but unused by the session arrangement). On an older zellij one or more of these is missing and the backend won't work.

The dashboard must **run inside the zellij session it controls** — detection reads `ZELLIJ_SESSION_NAME` (which also pins every `zellij action` call to that session), so a captain-miao started outside zellij falls back to Kitty. No extra config is needed: zellij's CLI is trusted by the session itself, so there is **no remote-control password** to set (contrast Kitty). If the env heuristic guesses wrong — e.g. a nested zellij where you actually want the Kitty backend — pin it explicitly:

```
[terminal]
backend = "kitty"          # or "zellij"; unset auto-detects (zellij when ZELLIJ_SESSION_NAME is set, else Kitty)
sessions_layout = "stacked" # or "per-tab"; unset ⇒ stacked. Toggle at runtime with Space l (persisted)
```

**Known zellij limitations:**

- **`t` (move window to tab) is unsupported.** zellij 0.44 has no CLI to reparent a pane across tabs (`BreakPane` is keybind/plugin-only), so the `move_to_tab` capability is `false`: the key reports "not supported" and the `?`-help entry is hidden.
- **In Stacked layout, sessions are invisible in the tab bar — the dashboard is the switcher.** In the default **Stacked** layout all sessions live as full-size floating panes stacked in the one `cm:sessions` tab (the `floating_sessions` capability), so the tab bar shows a single tab no matter how many sessions run; only the z-order top is visible, and only the dashboard (or zellij's own floating-pane cycling keybinds) reaches the rest. **Per-tab** layout (`Space l` toggles the two, persisted) instead gives each new session its own tab, visible in the tab bar and switchable natively — at the cost of a crowded bar. The layout is a spawn-time policy on **new** sessions only: switching it never moves a running pane (zellij can't reparent one — see the `t`/move-to-tab note above), so restart a session (`Space e`/`Space E`) to migrate it into the current layout. Kitty is symmetric: Stacked is one global `cm:sessions` stack tab, Per-tab is one tab per session. Manual re-layouts stick: the dashboard never rearranges panes.
- **The floating layer of the sessions tab is shared territory.** A floating pane the user opens in `cm:sessions` joins the same z-order stack, and zellij's toggle-floating keybind (default `Alt+f`) there hides/shows *all* sessions at once — harmless (no pty resize, sessions keep running; the next dashboard `Enter` shows the layer again), just surprising. Closing the `cm:sessions` tab closes every session pane in it (the launchers get SIGHUP), same as closing session tabs before.
- **Spawning a session is blink-free** (`new-pane --floating --tab-id` into the non-active sessions tab moves the client not at all) — except the very first spawn, which creates the `cm:sessions` tab via `new-tab` (no `--dont-take-focus`) and snaps back with the next action (~20ms), once per zellij session. Work tabs (`w`) deliberately keep the focus `new-tab` gives them — `w` means "go there", so nothing snaps back. Fully focus-less tab creation exists only in the wasm-plugin API.
- **The `captain-miao focus` bell keybind is deferred on zellij.** zellij keybinds run a command in a *new pane* rather than a background process, so the Kitty-style `map … launch --type=background captain-miao focus` has no direct analog. A floating, close-on-exit pane binding works as a workaround today; a `focus --window-id auto` (self-resolving the calling pane) is a planned follow-up.

## Claude Code hooks

The launcher injects hooks via `--settings <file>` pointing to a per-session JSON file. Hooks registered: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `Elicitation`, `ElicitationResult`, `Stop`, `StopFailure`, `PreCompact`, `PostCompact`, `CwdChanged`. Each calls `captain-miao hook --sock <launcher-socket> <event>`.

Hooks do NOT go in `~/.claude/settings.json` — they're injected per-session by the launcher and torn down when the session exits.

## Background-job states (`BackgroundActive` / `BackgroundServer`)

A turn can end while a `run_in_background` Bash command the agent spawned keeps running. captain-miao splits that into **two** states by *what* the command is — because a short build the agent is waiting on and a dev server it parked and moved on from are opposite things:

- **`BackgroundActive`** (green "Task", **busy**) — a **short-term** background step (a build/test/script) the agent is waiting to finish before it resumes. Finite work is genuinely in progress, so it stays `is_busy()`: active-grouped and keep-awake, exactly like `Active`.
- **`BackgroundServer`** (yellow "Server", **at-rest**) — a **long-running** service (dev server, file watcher) the agent left running. The agent isn't working, so it is **not** `is_busy()` (no keep-awake, idle-grouped), and *entering* it **arms the row's follow-up bell** so the parked session still draws a glance. (Rationale: many sessions sit for hours running a background dev server, which shouldn't read as the agent actively working or keep the machine awake.)

The base state from the session file is `BackgroundActive`; the launcher **refines** it to `BackgroundServer` (or `ReviewPending`) by classifying the running command — see the classification + self-learning below.

### Classifying long-running vs short-term (self-learning)

Which of the two a background shell is comes from **classifying its command**, folded from the live process tree by the same scan that detects r3 review-watches (below). Two sources, OR'd:

- **Seed heuristic** (`claude::is_long_running_command`) — a curated recognizer for the common dev servers/watchers (`npm|pnpm|yarn|bun run dev|serve|start|watch`, `vite`, `next dev`, `cargo watch`, `--watch`, `nodemon`, `uvicorn`/`gunicorn`, `rails server`, `flask run`, `docker compose up`, a bare `serve` / `…server` token, …), with a build/test guard so `vite build` / `npm run build` stay transient. Deliberately conservative: an unrecognized command is **not** long-running, so it stays busy (keeping the machine awake for a mystery task is safe; parking a real in-progress build is not). This gives instant recognition for the common case with no cold-start.
- **Learned store** (`cm_core::learned`) — captain-miao *learns* the long-running commands the seed misses. When a background command the seed didn't recognize has been running past a threshold (`LEARN_LONG_RUNNING_AFTER`, 1h), the launcher records its normalized command in `state_dir()/long-running-commands/` (one file per command, hashed name, atomic create — concurrency-safe across many launchers, no shared-JSON read-modify-write). Every **future** session running that same command is then treated as at-rest from the first moment, and the **current** session flips too. A parked session otherwise never wakes to notice the threshold, so the launcher schedules a one-shot `learn_at` deadline (the select! arm beside the flush deadline) at the oldest candidate's `first_seen + threshold`; `bg_first_seen` (launcher-lifetime, per normalized command) times each still-unrecognized command. The learning key is the command *normalized* — `normalize_bg_command` extracts the eval'd body from the Bash-tool wrapper (a bash single-quoted string can't contain `'`, so the first quote after `eval '` closes it), dropping the per-session snapshot path + cwd temp so "the same command" matches across sessions.

`classify_and_learn` folds a session's shells into an aggregate class with precedence **any transient → busy `BackgroundActive`** (finite work dominates — keep awake), else **any long-running (seed or learned) → at-rest `BackgroundServer`**, else (all watches) **`ReviewPending`**; it returns the class plus the next learn deadline. `is_learned`/`learn` are injected so the precedence/timing logic is unit-tested without touching the filesystem. `refine_background_kind` applies the class to the status (gated to the three background states, so a `None` — no shells / unreadable tree — leaves the way out to the session-file idle transition).

Detection reads Claude's **own session-status file**, `~/.claude/sessions/<pid>.json` (keyed by the agent's process id, which is `child_pid` — `direnv exec` execs in place so the pid matches even with a `.envrc`). Its `status` field is Claude's authority on the **coarse working/idle/background-shell axis**: `"busy"` mid-turn (verified live — a foreground tool reads `"busy"` throughout, it never dips to `"idle"`), `"shell"` when the turn has ended but a background shell is still running, `"idle"` at rest. Claude maintains it, so there is **no edge-tracking and nothing to go stale**: we just mirror it. The hooks still own the *fine-grained* states the file can't express (`Compacting`, `Compacted`, `WaitingForApproval`, `WaitingForDecision`).

This single signal does two jobs: it surfaces `BackgroundActive`, **and** it catches interrupts — Claude fires no `Stop` hook on Esc, so without the file an `Active` row would stick forever. (The session file replaced an earlier transcript-`<task-notification>`-scan for background detection, which had to reason about missed-completion staleness.)

**`Stop` defers to the file; a background subagent stays `Active`, not `BackgroundActive`.** Claude's `Stop` hook no longer forces `Idle` — it defers to the session file via `status_after_stop`: hold the current `Active` while the file still reads `"busy"`, map `"shell"` to `BackgroundActive`, and settle to `Idle` only when the file agrees (or is missing/unreadable — the safe fallback, the pre-subagent behaviour). So for Claude the file's idle-write now settles *every* turn end via the `Active + Idle`→`Idle` reconciliation below, not just Esc-interrupts; `Stop` merely stops overriding it (the file trails `Stop` by ~tens of ms, absorbed by the write-throttle, so a normal end still shows a clean `Active`→`Idle`). The reason: a background **subagent** (the Agent tool) ends the model's turn while dispatched subagents keep running, and Claude holds the file at `"busy"` (frozen — no rewrite, no wake) throughout. Unlike a `run_in_background` shell a subagent is **in-process** — no `"shell"` status and no child process, so the `ReviewPending` process-tree scan can't see it and `"busy"` is the only signal — so `status_after_stop` holds `Active` rather than flashing the row to `Idle` for the whole gap. Net: `BackgroundActive`/"Task" is specifically the `run_in_background`-**shell** state (file `"shell"`) and the base the launcher refines into `BackgroundServer`/`ReviewPending`; an actively-orchestrated subagent stays plain `Active`. This mirrors Claude's own `"busy"` vs `"shell"` vocabulary — the source of truth we defer to. (Claude-only: Codex has no session file, so its `Stop` still settles straight to `Idle`.)

- `claude::session_activity(pid)` reads the file and maps `status` → `AgentActivity`: `"busy"`→`Working`, `"shell"`→`BackgroundShell`, `"idle"`→`Idle`; `None` if missing/unreadable, the JSON doesn't parse, **or** the status is unrecognized (caller leaves the status unchanged). A torn read of a mid-rewrite file (or a future unknown status) maps to `None`, never a definite state, so it can't spuriously demote a live `Active`/`BackgroundActive` row. Codex has no such file and returns `None`, so its `Active`↔`Idle` rides hooks (+ rollout `turn_aborted`) and it's never refined into `BackgroundActive`.
- The launcher watches `agent.session_watch_path(child_pid)` (the session file) with the same `start_file_watcher` used for the transcript, but on a **separate channel** (`sess_rx`, not the transcript's `fs_rx`) — `status` changes fire no hook, so the watch is what wakes us on the `working ↔ idle ↔ shell` transitions. The separate channel matters: `on_transcript_changed` treats any `fs_rx` wake past the approval-grace window as "the user dismissed the permission dialog → `Active`", so a session-file write (e.g. a background job finishing during a *later* turn's approval prompt) must not reach it. The session-file `sess_rx` arm does no transcript work; it just lets the refinement below re-read the file. The pid is fixed for the session, so this watcher is started once.
- `process_hooks` refinement is **demote-only** — hook events own the rest→`Active` direction, so a momentary `"busy"` read can never bounce a row into `Active`. Consulted only while `Active`/`Idle`/`BackgroundActive`/`BackgroundServer` (fine-grained states are skipped, so the file can't clobber them): `Active + Idle`→`Idle` and `Active + BackgroundShell`→`BackgroundActive` (interrupt with no hook); `Idle + BackgroundShell`→`BackgroundActive` and `BackgroundActive/BackgroundServer + Idle`→`Idle` (background shell appearing / all shells gone). The session-file write can lag the `Stop` hook slightly; the watcher catches the late write and refines on the next wake.
- The transcript scan (`scan_transcript_signals`) is still the signal for the cases the session file deliberately doesn't cover: Codex interrupts (no session file), Claude's interrupt *while a permission dialog is up* (refinement skips `WaitingForApproval`), and `compact_aborted` (refinement skips `Compacting`).

`SessionStatus::is_busy()` (Active | Compacting | **BackgroundActive**) is the single source of the busy/at-rest split used by the active-group sort, the keep-awake inhibitor, and the launcher's `active_since` gate, so a short-term background task keeps the row in the active group and keeps the machine awake. **`BackgroundServer`** is deliberately **excluded**: a parked dev server is at-rest, so a Server row sits in the idle pile and does **not** keep the machine awake. Instead it earns a follow-up bell on **entry** (`Active/Idle/BackgroundActive → BackgroundServer` arms `follow_up`, via `follow_up_transitions` — guarded so a first-seen Server row at startup, with no prior status, doesn't light up); the busy `BackgroundActive` is **not** armed on entry (it's work in progress) but arms on its exit to `Idle` like `Active`, and `BackgroundServer → Idle` earns the same auto-mark when the server stops. `BackgroundServer`'s status label renders **yellow** (`status_color`), matching the attention states; `BackgroundActive` stays green with `Active`. Coupling risk: this depends on Claude's session-file `status` vocabulary (`busy`/`shell`/`idle`); `parse_session_activity`'s test pins the contract (including that a torn/unknown read maps to `None`, not a definite state), and the same file is already parsed for session names, so a format change surfaces there too.

## Review-pending state (`ReviewPending`)

**r3** (<https://github.com/hyperlogue/r3>) is a local human↔agent review tool: an agent finishes a change, creates a review, then runs `r3 watch <review-id>` as a `run_in_background` Bash task and **blocks** until the human submits feedback in the browser. Without special-casing that reads as a plain background row — indistinguishable from a build still cooking or a dev server parked — even though the agent isn't *working*, it's waiting on **you**. `ReviewPending` (yellow "Review" label, matching the other attention states) is the refinement that says so: the turn ended and every running background shell is a review-watch, so the agent is parked on a human review. Like `BackgroundServer` it's non-busy (neither keeps the machine awake), but where a Server row is an attention row only while its auto-armed follow-up bell is still set, `ReviewPending` is *unconditionally* an **attention** state (`needs_attention()`) — `s` jumps to it and it does **not** keep the machine awake (no point caffeinating for a 2h `watch` timeout waiting on a human). It does **not** float to the top attention rank, though: unlike an approval/decision the agent isn't blocking on a live prompt, so `compute_visible_indices` ranks it like a follow-up idle row but in its own tier **below** the actual follow-up-flagged rows (above plain idle), sorted oldest-first like the other attention tiers.

Detection is a refinement of `BackgroundActive` sharing the same process-tree scan as the long-running-server classification above — the session file still owns whether a background shell is running at all; the scan only **classifies** what's running:

- The launcher reads the agent's running `run_in_background` shells straight off the **process tree**: the Bash tool runs every background command in a wrapper shell that stays a direct child of the agent process for the task's lifetime, with the eval'd command embedded verbatim in its command line. `claude::bg_shells(agent_pid)` lists the agent's children (Linux: one `/proc` walk matching ppid; macOS: one `ps -Aww -o ppid=,command=` exec), keeps only Bash-tool shells — they source a `…/shell-snapshots/snapshot-…` env snapshot, a marker the agent's *other* children (MCP stdio servers, helpers) never carry — and reduces each to a normalized command + a `BgSeedKind` (`ReviewWatch` via `is_r3_watch_command` — the `watch review_<id>` form or an `r3` / `…/r3/cli/index.ts` entrypoint followed by `watch`, narrow enough not to fire on `cargo watch`; `LongRunning` via the seed heuristic; else `Other`). The tree is present-tense truth, so the classification **can't go stale**: a task that ends with *no transcript marker* — stopped from the UI, a Monitor timeout, agent teardown, or a `--resume` orphan from a previous process incarnation — simply isn't in the tree anymore. (This replaced an earlier transcript fold that tracked launch/`<task-notification>` pairs by tool-use id: Claude Code's own notification text admits those stop paths "leave no transcript marker", so the fold could carry a phantom task forever and pin the row at `BackgroundActive`. There is no hook, state-file field, or registry for background-shell lifecycle — the session file's `"shell"` status is computed from Claude's in-memory state — so the process tree is the only reliable source of *what* is running.)
- `classify_and_learn` + `refine_background_kind` (run on every launcher wake, but **gated to the three background states** `BackgroundActive`/`BackgroundServer`/`ReviewPending` — so a busy foreground turn never pays for the scan, and no *foreground* tool shell can be among the children when it runs) promote `→ ReviewPending` when every running shell is a review-watch, `→ BackgroundServer` when they're all long-running, else `→ BackgroundActive`. A `None` (no shells / unreadable tree — e.g. the watch just finished) is deliberately left to the session-file `shell → idle` transition, so the row exits cleanly to `Idle` instead of flickering through a background state. There is no polling: the scan runs only on wakes (hook, transcript, session-file, flush deadline, or the `learn_at` long-running deadline), which in practice means a small burst at each turn boundary and nothing while the row sits parked on a review. The activity reconciliation treats `ReviewPending` as a background-shell state (settles `ReviewPending + Idle → Idle`), and `ReviewPending → Idle` earns the same follow-up auto-mark as `BackgroundServer → Idle`. When the human submits, the agent resumes and the hooks drive `ReviewPending → Active`.

Codex has no `run_in_background` concept, so its `AgentControl::bg_shells` is always `None` and it never refines into `BackgroundServer` or `ReviewPending`.

## Failed launches (`FailedToStart`)

A launch can fail before any agent runs: `direnv` blocked on the session's `.envrc` (`check_direnv_allowed` bails), the agent binary is missing, or the spawn itself errors. Rather than tear the state file down and vanish (the held Kitty window's error text is easy to miss in an unfocused tab), the launcher **holds**: `hold_failed_launch` stamps `status = FailedToStart` + `last_error` onto the state file, prints the error to the held window, and **blocks on `wait_for_termination_signal`** until the user dismisses it — closing the window (kitty SIGHUP) or killing the row (the dashboard SIGTERMs `launcher_pid`; there's no `child_pid`). So a failed start is a first-class, dismissable red "Failed" row carrying the reason, not a silently reaped one.

- **The launcher never drives the terminal.** Focus is a presentation-layer concern (`docs/remote-sessions.md` §1) and a launcher may be headless/remote, so it records the failure on the normal `LauncherState` channel and the *dashboard* focuses the held window. `reload_sessions` queues the window (`newly_failed_windows`, pinned by a unit test) on the **transition into `FailedToStart`** — once, local sessions with a window only — and the run loop drains `failed_launch_focus_queue` and calls `terminal::get().focus_window`. Pre-existing failed rows aren't auto-focused at startup (the initial queue is cleared). Because the row rides the state file, a *remote* launch failure surfaces the same way (as a non-local row, just not focused).
- **It slots into the existing row machinery for free.** `is_busy()`/`needs_attention()` are derived: `FailedToStart` is not busy (no keep-awake, idle-grouped by that axis) but *does* need attention, so it floats to the attention sort rank and `s` jumps to it. Kill already targets `launcher_pid` when `child_pid` is `None`; restart is gated to `Idle`/`Compacted` so a failed row isn't offered (there's no session to resume — the fix is `direnv allow` then relaunch); the crash-recovery snapshot self-excludes it (no `live_session_id`).

## Codex hooks

Codex's hook system is a near-clone of Claude's (same payload field names, a subset of the events), so the launcher loop and `HookMessage` are reused unchanged. Two things differ:

- **Injection.** Codex has no per-invocation hooks flag; it discovers hooks from `$CODEX_HOME/hooks.json`. So the launcher points Codex at the shared synthetic `$CODEX_HOME` (`state_dir()/codex-home`, see Data files) and writes captain-miao's `hooks.json` there. The hook command is `captain-miao hook --agent codex <event>` with **no `--sock`** — the socket arrives via the `CAPTAIN_MIAO_SOCK` env var the launcher sets on the Codex child. Keeping the command socket-free makes `hooks.json` byte-identical across sessions so Codex's content-hashed trust state holds. A consequence of the synthetic home worth knowing: the `transcript_path` Codex reports in every hook payload points at `$CODEX_HOME/sessions/…`, i.e. **through the `sessions` symlink**, so the rollout the launcher watches is reached by a non-canonical path. macOS FSEvents reports the *resolved* path (Linux inotify echoes the registered one), so `start_file_watcher`'s filter matches either spelling (`canonical_watch_target`) — without that it silently dropped every Codex rollout event, freezing the transcript fold and leaving Codex rows with no context tokens.
- **Rollout watch (stat poll on macOS).** Codex opens its rollout once and appends through that fd for the whole session, and **macOS FSEvents reports nothing for writes through a long-held fd until the file is closed** (measured: 12 flushed appends over 36s produced 0 events — on both a file-level and a directory-level watch, with or without fsync; the close produced 1) — so an event-driven watch never wakes the launcher during a Codex session: no context tokens, no first-prompt fold, and an Esc-interrupt (`turn_aborted`, which fires no hook) leaves the row Active forever. The fix lives at the watcher seam, not in the launcher loop: `AgentControl::transcript_poll_interval` selects the hand-rolled stat poll `launcher::start_stat_poll` (2s) for Codex on macOS — a task that stats the rollout and signals whenever full-precision `(size, mtime)` moves, which sees each flushed append immediately (`write(2)` updates the stat metadata at write time; only the FSEvents notification waits for close) — keeping every other watch (Claude transcripts, the session-status file, Linux everywhere — inotify fires per write) event-driven. Hand-rolled deliberately, **not notify's `PollWatcher`**: that one compares mtime truncated to whole seconds and nothing else, so the second of two appends within one wall-clock second never fires — a rollout's exact write pattern (a `turn_aborted` landing sub-second after the previous line would stick the row at Active forever). **The poll runs only while the session is off Idle**: an idle rollout doesn't change without a hook firing first, so the launcher parks the watch at Idle (a session sitting at rest costs zero stats) and re-creates it on any other status — Idle is deliberately the only parked state, since approval/decision waits need the rollout wake (approval-granted fast path; an Esc there writes `turn_aborted` with no hook). Both lifecycle edges fire one synthetic fs wake, because the boundary writes are otherwise invisible: the final `token_count` lands ~20ms before `Stop` (inside the last tick window), and bytes written while parked predate a fresh poll's baseline stat. And **every hook wake does an inline catch-up read before `dispatch_hook`** (`rescan_transcript` in the hook arm), so a state-changing moment folds its rollout bytes at hook latency instead of a tick later — Codex writes the lines *before* firing the matching hook, so they're already on disk. The transcript-before-dispatch ordering is load-bearing: the scan may consume a stale `turn_aborted` from an Esc no tick had read yet, which must settle the *old* turn before the hook applies — scanning after the dispatch would clobber a fresh `UserPromptSubmit`'s Active with Idle and stick the row there. Pinned by `stat_poll_sees_held_fd_appends`. (Claude is immune: it opens/writes/closes per transcript line. And no hook can replace the poll: verified against the codex source at 0.142.3, an aborted turn returns before `run_turn_stop_hooks` and the `notify` program — `turn_aborted` reaches only the rollout, so a transcript read is the sole signal.)
- **Trust.** Codex gates new/changed command hooks behind an interactive "Trust all and continue" prompt, recording per-hook trust in `$CODEX_HOME/config.toml` under `[hooks.state]` (key `"<hooks.json path>:<label>:<group>:<handler>"`, value `trusted_hash = "sha256:…"`). Because captain-miao **authors** `hooks.json`, it pre-computes that exact hash and writes the trust itself — `codex::seed_hook_trust`, run from `ensure_synth_home` on every launch — so no prompt ever fires and **`--dangerously-bypass-hook-trust` is not passed** (only `-c features.hooks=true`). The hash reproduces Codex's `version_for_toml`: a normalized identity `{event_name, matcher, hooks:[{type,command,timeout:600,async:false}]}` → canonical (recursively key-sorted, compact) JSON → sha256, pinned by `command_hook_hash` and its real-value regression test. Recomputing every launch means trust never goes stale even when the embedded exe path changes (e.g. a nix-store rebuild) or the user edits their real config (which reseeds the synth `config.toml`). The coupling risk: if a future Codex changes its hashing, the seeded hash mismatches and the one-time prompt returns — the regression test fails loudly first. (A normal directory-trust prompt is separate and unaffected.)

Events registered (Codex's subset): `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Stop`, `PreCompact`, `PostCompact`. Codex has no `Elicitation`/`StopFailure`/`CwdChanged` hooks, and folds tool failures into `PostToolUse` (so the standalone stop-failure banner doesn't surface for Codex). Interrupts come from the rollout's `turn_aborted` event (via `scan_transcript_signals`), and context tokens / first prompt / git branch come straight from typed rollout events rather than heuristics.

**`request_user_input` → `WaitingForDecision`.** Codex's analog of Claude's `AskUserQuestion` is the `request_user_input` tool (questions + labeled options + an `isOther` free-form choice). Unlike Claude's `AskUserQuestion`, which rides the `PermissionRequest` hook (and so maps to `WaitingForApproval` unless special-cased — see Claude's `dispatch_hook`), Codex's `request_user_input` is *outside* the approval path: it emits its own ephemeral `RequestUserInput` event (persistence `None` — never written to the rollout, so `scan_transcript_signals` can't see it) and blocks. The only signal captain-miao gets is the `PreToolUse` hook, so `dispatch_hook`'s `PreToolUse` arm maps `tool_name == "request_user_input"` to `WaitingForDecision` ("Decision", needs-attention); the paired `PostToolUse` — which fires once the user answers — resets it to Active, so no grace-period machinery is needed. This is the only path that surfaces `WaitingForDecision` for Codex (the registered `Elicitation` hook is never emitted).

**Session names.** Unlike Claude (which writes a `custom-title` line into the transcript on `/rename`), Codex stores the session title — both user renames and its own first-message auto-title — in `~/.codex/state_5.sqlite` (`threads.title`, keyed by session id). There is no hook or rollout line for a rename (it touches sqlite only — confirmed against the codex source), so the title is read by the **per-host title overlay** in `cm_core::backend::LocalBackend` — of which exactly one exists per host process: the dashboard's local backend, or the remote daemon's server-core shared across connections — which stamps titles onto `LauncherState.name` as sessions are served (`overlay_codex_titles`, inside `list_sessions`). One reader per host, no matter how many Codex sessions run. Because the daemon overlays before `Snapshot`/`Delta`, the title reaches **remote** rows exactly like Claude's launcher-folded name, and `session_display_name` picks both up at precedence step 1 (`name`). `codex::read_session_index` stays empty; the index carries no Codex names. An untitled session (or a read failure) falls through to the first-prompt auto-title the launcher folds from the rollout onto `LauncherState.first_prompt` (identical to Codex's own auto-title for un-renamed sessions).

The overlay is **heavily throttled** — it reads sqlite only when something can have changed, and never faster than a floor: no live Codex sessions → no reads at all; a session id it has never seen → one immediate batch read (first-load titling, incl. resumes); otherwise a read requires **both** a store change — the `(db, wal)` mtime stamp moved (`codex::title_store_mtimes`, two stats, no sqlite) — **and** ≥ `CODEX_TITLE_REFRESH_FLOOR` (30s) since the last read (`title_refresh_due`, pinned by test). So even a wal-churning Codex burst costs at most one batched read-only query (`codex::read_thread_titles`, all live ids over one connection) per 30s per host. The wake that makes an at-rest rename surface: `codex::watch_paths()` nominates the **`-wal` sidecar** (`state_5.sqlite` is a WAL DB — writes land in `state_5.sqlite-wal`; watching that one file keeps the churny `logs_2.sqlite-wal` telemetry sibling from waking anything), which the dashboard's reload watcher and the daemon's sessions watcher both register best-effort. A missed wake self-heals: the next session event re-runs the overlay. (The daemon's idle watchdog reads raw state files, not `list_sessions`, so its tick never touches the overlay.)

The title read uses **bundled SQLite** — the `rusqlite` crate with the `bundled` feature statically compiles SQLite into the binary, so there is **no runtime `sqlite3` CLI dependency** and renames work on any host (including remote daemons). `read_thread_titles` opens the DB read-only (`SQLITE_OPEN_READ_ONLY`) so the live WAL is never disturbed, then `query_thread_title` runs each lookup and cleans the result. The id is a **bound parameter**, so it can never alter the query regardless of contents — no shape validation needed. `query_thread_title`/`query_thread_titles` are split out from the IO so the cleaning/empty/NULL handling is testable against an in-memory DB (`Connection::open_in_memory`). The tradeoff: `bundled` compiles the SQLite C amalgamation, so the build needs a C compiler (already present in the nix dev shell and crane build).

## Key bindings (TUI)

Press `?` in the dashboard for the full list. Highlights:

- `j/k`, `↑/↓`, `Ctrl-n/p` — navigate sessions; `gg`/`G` — top/bottom; `1..9` — select Nth; `Ctrl-1..9` — select Nth and focus its window
- `Enter` — focus selected window (for a running *remote* session with a known pool name, attaches a local `ssh … captain-miao attach` window to it instead, or focuses the one already bound); `o` / `O` — new session (`o` in the selected row's cwd, `O` prompts for one; placement follows the current **session layout** — Stacked puts it in the shared `cm:sessions` tab, Per-tab in a fresh tab, on both backends — see `Space l` below and Known zellij limitations) — `o` on a *remote* row opens another session on that host + cwd (its pty pool), not locally. In the cwd picker, `Ctrl-t` switches the backend for this launch (title reflects it), `Ctrl-d` drops the highlighted recent directory (local host only), and `Ctrl-h` cycles the host this launch opens on (local → each configured remote; only shown when remotes exist). The picker is **host-aware**: switching to a remote re-seeds the recent-dir list, path completion (`Tab`), and submit-time validation against *that machine's* filesystem (over the protocol's `ListRecentDirs`/`CompletePath`/`CheckDir`), with `~` resolving to the remote `$HOME` — so a remote launch's cwd is a real path on the remote, not a local one that libshpool's `--dir` would reject. Remote launches record their cwd into the remote host's recent list server-side (in `open_in_pool`); local launches record into the local list.
- `r` — resume picker (cross-host: unions every backend's resumable list, each row tagged with its host when remotes exist; resume opens it on that host); `b` — cross-host browser (one searchable list of every **running** session — focus/attach — and every **resumable** one — resume — across all hosts, each row tagged `running`/`resumable` + host); `f` — fork (resume selected in place, local sessions only); `y` — copy selected session id to clipboard (platform CLI `pbcopy`/`wl-copy`/`xclip`/`xsel`, falling back to an OSC 52 terminal escape when none is installed); `x` — kill; `D` — detach from a *remote* session (close the local `ssh attach` window but leave the pooled session running; the row stays and `Enter` re-attaches — contrast `x`, which kills it); `t` — move window to tab (**unsupported on the zellij backend** — no cross-tab pane reparent, so the key reports it and the `?`-help entry is hidden); `w` — switch to the per-`(host, cwd)` **work tab** captain-miao created earlier for that dir (validated against a live snapshot on the tab id, the basename title the spawn stamped, *and* — when recorded — the pane the spawn created: zellij recycles a closed highest tab's id, and two same-basename dirs share a title, but pane ids never recycle, so the window check defeats both; a failed check prunes the entry, while a failed *snapshot* bails without pruning — the map is persisted, so a transient terminal error must not do durable damage), otherwise open a fresh shell tab titled with the dir basename and record it (a local shell for a local row; an `ssh -t <host>` tab that cds into the dir for a remote row). It no longer jumps to a manually-opened shell that merely happens to sit in the cwd — only tabs captain-miao created count. (`W`, the old always-open variant, is removed.)
- `m` / `p` / `i` — mute / pin / toggle needs-input; `s` — jump to next attention; `h`/`l` (or `←`/`→`, or `<`/`>`) — scroll preview horizontally; `R` — refresh preview now (the preview also auto-refreshes every `polling.preview_auto_refresh_secs` = 10s, but only while the dashboard has terminal focus — tracked via FocusGained/FocusLost — the selected session `is_busy()`, and the preview isn't scrolled; 0 disables). A preview whose content is older than `thresholds.preview_stale_secs` = 20s shows an `updated <age> ago` label in its title — minute resolution like the Updated column (`<1m`, then `3m` / `1h05m`); success-stamped, so a failing re-fetch keeps the age growing.
- `Space` is the leader key (pressing it shows a which-key strip of the available continuations in the footer; `g` shows its own). `Space v` / `Space d` — toggle preview / detail panels. The **panel arrangement is responsive to the body width** (`ui.panels.narrow_max_width`, default 90): above it the panels sit side-by-side (detail in the right column, preview across the bottom); at or below it they **stack vertically** — session list → detail → preview — with the session table trimmed to just status / workdir-icon / name (the title truncates, the rest fixed-width), a compact detail panel showing only agent / model / context / last-updated, and a dynamic-height preview that drops out entirely when the viewport is too short to spare it. The stacked layout auto-sizes (the split-resize border drags are wide-only), and `Space v` / `Space d` still gate preview/detail visibility in both. In the wide layout the **name column is a fixed max-width column** (`Max(name_truncate + 10)`, 45 cells by default) and the title is truncated to that same width so an over-long title's ellipsis lands at the column edge; the **last-prompt column is elastic** (`Fill`), soaking up the slack when there's room and yielding (truncating) first when there isn't — so a tight-but-wide viewport shortens the prompt rather than collapsing the session title (the old `Min`-width prompt column out-prioritized the capped name column and did exactly that). `Space i` — edit selected dir's icon + color (Tab cycles the Icon text field and the Color palette; from the Icon field `Ctrl-E` opens a **searchable emoji picker** — the standard telescope `Picker` over every emoji (`emojis` crate, one representative per skin-tone family), filtered by typing the CLDR name/shortcode; selecting writes the glyph into the Icon field and returns to the editor, cancelling returns without changing it); `Space e` / `Space E` — restart selected / all (idle only, y/N confirm) to pick up agent or `.envrc` updates; `Space z` — toggle keep-awake (defaults on when supported; runs `caffeinate -dis -w <pid>` on macOS or `systemd-inhibit ... sleep infinity` on Linux while any session is Active/Compacting). A ☕ shows at the top-right of the header **only while it's actively inhibiting sleep** (the feature is on and a session is busy); when idle, disabled, or unsupported there's no indicator at all. `Space a` — open a picker to set the *default* backend for new sessions (Claude / Codex), persisted across restarts in `dashboard-overrides.json` (initial value from `[launcher] default_agent`). The current default is shown in the header's top-right cluster (`Default agent: <backend>`, just left of the ☕ indicator); the footer hint reads `Space a switch default agent`. A single launch can override the default in the new-session cwd picker with `Ctrl-t`. `Space l` — toggle the **session layout** between **Stacked** (all sessions consolidated in one shared `cm:sessions` tab, one visible at a time — floating panes on zellij, a stack-layout tab on Kitty) and **Per-tab** (one session per tab, both backends), persisted across restarts in `dashboard-overrides.json` (initial value from `[terminal] sessions_layout`) and shown in the header's top-right cluster (`Layout: stacked` / `Layout: per-tab`, mirroring the `Space a` default-agent indicator). It's a spawn-time policy on **new** sessions only — toggling never moves a running session; restart with `Space e` (selected) / `Space E` (all) to migrate existing sessions into the new layout, since the restart path respawns each into the current layout. `Space h` — open the remote-hosts popup (add/edit/remove hosts + per-host name color; persisted in `hosts.json`); on save it reconnects the backends. See Remote hosts.
- `/` — search; `q` / `Ctrl-c` — quit

### Configurable keybindings (`src/app/keymap.rs`)

Every Normal-mode command above (including the `Space`-leader ones) is remappable via a `[keybinds]` table in `config.toml`; `keys.rs` dispatches through a `Keymap` instead of a hard-coded `match`. The handler resolves each `KeyEvent` to a `keymap::Chord`, then either completes a pending two-chord sequence, starts one (any first chord of a two-chord binding is a *prefix*), or runs a single-chord binding via `App::run_command`. Each binding maps a `KeySeq` (one or two `Chord`s) to a `Command` enum variant; `run_command` is the single place a `Command` turns into a side effect, so default and remapped keys share one body.

- **Config shape.** `command-id = "key"` or `command-id = ["key", "alt", …]`. Ids are `Command::id()` (`kill`, `jump_attention`, `new_session_cwd`, `restart`, `toggle_preview`, …); keys parse via `Chord::parse` (`"ctrl+u"`, `"O"`/`"shift+o"` — same chord, `"<"`, `"space e"`, `"g g"`, `"enter"`, `"f5"`, arrows by name). `+` separates modifiers; Shift on a letter folds into the uppercased char (matching crossterm + the old "match on `code`, ignore Shift" behaviour). `KeyBinding` (in `config.rs`) is an untagged string-or-list enum.
- **Merge semantics** (`Keymap::from_config`, overlaid on the `DEFAULTS` table): overriding a command **replaces all** its default keys (use a list to keep alternates; an **empty list unbinds**). A key claimed by an override is removed from whatever default command previously held it. Unknown ids, unparseable keys, and two overrides colliding on one key each emit a warning string; `App::new` joins them into the startup status line (the TUI swallows stderr, so that's the only place they'd surface).
- **Safety / fixed keys.** After a prefix (e.g. `Space`), a non-matching second key is **swallowed**, never re-interpreted as its single-key command — so `Space x` can't fall through to kill. (Contrast `g g`, kept bespoke outside the keymap precisely because it wants the opposite: `g` then a non-`g` key *does* fall through, so `g j` still navigates.) Not remappable: `Ctrl-c` (always quit), the `g g` prefix, and the digit selectors `1..9` / `Ctrl-1..9`. Only Normal mode is configurable — text-input modes (Search/Picker/DirEdit/Confirm/Help) keep fixed keys.
- **Discoverability.** While a prefix is pending, the footer shows a **which-key strip** of the available continuations (`Keymap::continuations` → `(second-key, Command::short_label)`); the bespoke `g` prefix shows its own one-item hint (`pending_g`). The `?` help overlay and the steady-state footer render live bindings via `Keymap::keys_for` / `primary_key` (showing `(unbound)` / dropping the hint when a command has no key), so a remap shows through without touching `draw.rs`.
- **Recent default tweaks** (informed by the `keybinds.log` sweep): horizontal preview scroll moved its canonical keys to `h`/`l` (+ `←`/`→`), keeping `<`/`>` as alternates, since the old shifted `<`/`>` saw zero use; the directory-mark editor moved to `Space i` (was `Space c`) and the detail-panel toggle to `Space d`; the `:` command line (`q`/`quit`/`clear` — all duplicates of existing keys, zero use) was removed along with `InputMode::Command`.

## Remote hosts (SSH)

The dashboard federates remote hosts: it aggregates sessions from a local backend
(`backends[0]`) plus one `RemoteBackend` per configured host, each mirroring a
`captain-miao daemon` on that host over a unix socket. Full design in
`docs/remote-sessions.md`.

**Gated off by default.** The feature is unverified end-to-end against a real
remote host (and restart/fork are still local-only), so it ships behind the
`remote` cargo feature — `cargo build --features remote`. The gate is the runtime
const `app::REMOTE_ENABLED` (`cfg!(feature = "remote")`), *not* `#[cfg]` on the
~240 remote references across the dashboard: the code compiles, type-checks, and
runs its tests in both configurations, and the const closes the only two doors
that reach it — `build_backends_from_config` (returns local-only without ever
reading `hosts.json`, so no connection task is spawned) and the `Space h` handler
(reports the feature is off; its `?`-help entry is hidden, mirroring how the
unsupported `t` is hidden on zellij). Keep new remote entry points funnelling
through those two rather than adding a third gate.

- **Daemon** (`captain-miao daemon`, `src/server.rs`) is the single persistent
  per-host process. It **hosts the pty pool** (the libshpool daemon on a dedicated
  thread — `pty-pool` feature) AND wraps a `LocalBackend` to serve the protocol: a
  `Snapshot` then per-session `Delta`/`Removed` (driven by the `sessions/` notify
  watch), plus `ListResumable`/`KillSession`/`OpenSession` + the host-fs queries.
  **Self-daemonizing** (`daemon ensure` double-forks + `setsid`, so it detaches
  from the ssh session that started it and survives disconnects); singleton per
  host (`server.pid`); idempotent to start; **auto-exits when idle** (no pool
  sessions and no connected clients for 5 min); Kitty-exempt (headless). Lifecycle
  CLI (tmux/zellij-style): `daemon ensure` (start + print socket path), `daemon
  print-path`, `daemon status`, `daemon stop [--force]` (SIGTERM → tears down the
  pool + all its sessions). The daemon is dispatched in `main()` before the tokio
  runtime because the daemonize fork + the libshpool thread must precede it.
- **Client** (`RemoteBackend`) runs a background task that keeps an in-memory
  **mirror** of the host's sessions and pumps request/response by `req_id`. The
  sync `Backend` methods read the mirror (no round-trip) or `block_in_place` on a
  oneshot for a reply. The task is a **reconnect loop**: on any connection loss it
  re-establishes the transport (re-running `setup_ssh`, so the stale-forward
  cancel re-fires), re-`Hello`/`Subscribe`s and re-`Snapshot`s, with exponential
  backoff (500ms → 30s, reset on a good connect). `serve` returns a `ServeOutcome`
  (`BackendDropped` stops the loop; `ConnectionLost`/`HandshakeFailed` reconnect).
  On disconnect the mirror is cleared and per-host `ConnState`
  (`Connecting`/`Connected`/`Disconnected`) is surfaced in the header
  (`⟳`/`⚠ <host>`); a `Disconnected` host fails requests fast instead of blocking
  the caller through the backoff.
- **Transport.** `Transport::Socket` connects to a path directly (manual forward /
  testing); `Transport::Ssh` (a) ensures the remote daemon is up and learns its
  socket path in one `ssh … daemon ensure` call (idempotent, self-daemonizing,
  prints the path), then (b) runs a **forward-only** `ssh -N -L <local>:<remote>
  target` child (`kill_on_drop`) that just holds the tunnel — under `ControlMaster`
  + `BatchMode` (key/agent auth only). The daemon and the tunnel are **decoupled**:
  a dashboard disconnect / reconnect kills only the tunnel; the persistent daemon
  (and its pooled sessions) are untouched. (This replaced the old "server runs
  inside the ssh channel + `exec sleep`" model, whose lifetime was tied to one
  client's channel — the disconnect bug.)
- **Identity.** Each session is tagged with its `HostId` during reload (a
  `#[serde(skip)]` field, never persisted). Per-row state (`flags`, selection)
  keys on `(host, launcher_pid)` so a remote pid can't collide with a local one.
  Only local flags persist (remote flags are session-lifetime).
- **Config.** Hosts are mutable TUI state (`hosts.json`: label, ssh target /
  socket, color), managed via the `Space h` popup; saving reconnects the
  backends. A compact, colored **Host** column appears only when remotes are
  configured.
- **Remote spawn/attach/detach** work through the pty pool: a new session (`o`,
  or the picker's `Ctrl-h`) and a resume open in the remote's pool
  (`OpenSession` → `open_in_pool`), and the dashboard attaches a local
  `ssh … captain-miao-server attach <pool_session>` window (`Enter` /
  `AttachRemoteRunning`), bound by `(host, pool_session)`. `D` detaches (closes
  that window, leaves the pooled session running); closing the window otherwise
  detaches via the reload's `prune_detached_sessions`. Still local-only: **fork**
  and **restart** (both create local Kitty windows via `launch_agent(host=None)`).
- **Deferred** (follow-ups): Session names now ride `LauncherState.name` for both backends (Claude's
  launcher folds its session-file rename; the daemon's server-core overlays the
  Codex sqlite title before `Snapshot`/`Delta`), so remote rows get real titles —
  no name-index / title RPC needed; the Claude `session_id → name` manifest index
  stays local-only. The ssh
  transport is under active host-verification against a real remote (see
  `docs/remote-sessions.md` §8); the dev sandbox has no remote.

## Dev commands

```sh
cargo run                    # run TUI dashboard
cargo run -- claude .        # launch Claude in current dir with hooks
cargo run -- codex .         # launch Codex in current dir with hooks
cargo run -- focus           # focus the dashboard window
cargo run -- focus --window-id $KITTY_WINDOW_ID
                              # focus dashboard AND ring the bell on the session
                              # running in this kitty window (bind to a kitty key)
# The daemon + pty pool is the separate `captain-miao-server` binary:
cargo run -p captain-miao-server -- daemon ensure   # start the per-host daemon (pool + protocol); prints its socket path
cargo run -p captain-miao-server -- daemon status   # is the daemon running? pid, socket, session count
cargo run -p captain-miao-server -- daemon stop     # stop it (kills the pool + all its sessions)
cargo build --workspace      # build all four packages (dashboard + server + client + core)
cargo build --features remote  # dashboard with the WIP remote-hosts feature enabled
cargo test --workspace       # run the full test suite
cargo watch -x run           # auto-reload the dashboard on changes
```

## Release + npm distribution

Releases are tag-driven (`v*` → `.github/workflows/release.yml`). Two channels
ship from the **same** artifacts: a GitHub Release carrying a `.tar.gz` per
target, and npm.

**`Cargo.toml`'s `[workspace.package] version` is the single version source.**
The workflow's `verify` job — which everything else `needs` — greps it out of
`Cargo.toml` (plain awk — no job here has a Rust toolchain) and fails the run if
the tag disagrees; every npm version and pin is then stamped from that one value,
so the launcher's pins can never name a package version that wasn't built.
Bumping a release is therefore a one-line edit plus a tag.

The same `verify` job also pins the tag to **plain SemVer**
(`^v[0-9]+\.[0-9]+\.[0-9]+(-…)?$`), which is a security gate, not just hygiene:
`github.ref_name` flows downstream into artifact names and shell scripts, and git
accepts `` ` ``, `$( )`, quotes, `;`, `&` and `|` in a ref name (only space and
`*` are rejected). An unvalidated tag is therefore a script-injection vector.
The second layer is a rule the whole `.github/workflows/` tree now holds to:
**no `run:` body interpolates a `${{ }}` expression** — values reach the shell
through `env:` instead, because expressions are substituted into the script
*text* before bash parses it. Keep new steps to that rule.

**npm shape** (the standard prebuilt-binary pattern, mirroring the sibling `r3`
repo): four per-platform packages `@hyperlogue/captain-miao-<os>-<arch>` each
carrying only `bin/captain-miao` at 0755 and declaring `os`/`cpu`/`libc`, so a
package manager installs just the one matching the host; plus the launcher
package `@hyperlogue/captain-miao`, whose `bin` is `npm/launch.mjs` — a
dependency-free Node ESM file that `require.resolve`s the platform package and
execs the binary. Nothing is downloaded at runtime. `stdio: "inherit"` is
load-bearing there: the dashboard is a full-screen ratatui TUI that needs the
real tty for raw mode, resizes, and the kitty graphics protocol.

- `scripts/stage-npm-packages.sh` builds `dist/npm/` from the release tarballs and
  stamps the launcher's `version` + `optionalDependencies`. Bash + jq on purpose —
  this is a Rust workspace with no JS toolchain, and jq ships on GitHub runners.
  Watch the quoting: the `jq` program is bash-single-quoted, so **an apostrophe in
  a comment inside it ends the quote** and turns the remainder into bash. Comments
  about that object live above the `jq` call, not in it.
- **Publish order is load-bearing**: platform packages first, then a poll until
  each is *visible* on the registry, then the launcher. A launcher whose
  `optionalDependencies` name a package npm can't resolve yet is a broken install
  for anyone who runs `npx` in that window. Every step is idempotent (skip if the
  version already exists) so a re-run after a partial failure converges.
- Linux builds run on **ubuntu-22.04**, not `ubuntu-latest` — that pins the glibc
  floor at 2.35 (Debian 12 / RHEL 9 / Ubuntu 22.04), where a 24.04 build would die
  on the older loader. Both Linux arches build natively (`ubuntu-22.04-arm` is free
  for public repos), which drops the gcc cross toolchain bundled SQLite needed and
  lets the aarch64 binary be smoke-tested rather than shipped unexecuted.
- **No `.sha256` sidecars are published.** GitHub already hashes these artifacts
  twice: `upload-artifact` records a SHA-256 digest that `download-artifact`
  verifies on the way back out, and a release asset exposes a
  `digest: "sha256:…"` field in the REST API. A sidecar would be a third copy of
  the same number, trusted no more than the tarball beside it — it travels with
  the artifact, so it proves integrity only against corruption, never against
  anyone who could rewrite both.
- The extracted path **is** asserted to be a regular file before the staging step
  touches its mode — tar will happily extract a member recorded as a symlink, and
  the `chmod 0755` that follows would then follow it out of the staging dir.
- The publish job does **not exec** the staged binary (`test -x` only). build.yml
  already smoke-tests every natively-runnable target, and running an unverified
  artifact inside the job holding `NPM_TOKEN` would let a poisoned linux-x64
  binary rewrite the other three packages before they publish.
- Needs an `NPM_TOKEN` repo secret; `--provenance` needs `id-token: write` (already
  set) and a `repository.url` matching the repo the workflow runs in. Publishes
  run `--ignore-scripts` so a package lifecycle hook can never run with the token
  in its env. The publish job declares `environment: release` — **configure that
  environment with required reviewers, plus a `v*` tag ruleset**, or the gate is
  nominal (GitHub creates a missing environment implicitly and unprotected). Node
  is pinned to 24 for npm ≥ 11, the floor for npm **Trusted Publishing** (OIDC):
  link the packages to this workflow on npmjs.com and the long-lived `NPM_TOKEN`
  can be deleted — npm picks OIDC up automatically and falls back to the token
  until then.

## Committing

- **Run `cargo fmt` (and clippy) before you commit.** CI's first job is `cargo
  fmt --all --check` then `cargo clippy … -D warnings`; unformatted code or a
  lint warning turns it red. There is no local pre-commit gate, so running both
  yourself before you stage is what keeps CI green.
- **Work on `main` directly.** This repo doesn't use feature branches: commit to
  `main`, and don't open PRs for routine work.
- **Before you stage, check for a concurrent committer.** captain-miao's whole
  premise is running many agent sessions at once, so another one may be
  mid-commit in this same tree. The workflow is _stage your files, then commit_,
  so a non-empty index you didn't create means someone else is inside their
  stage→commit window. Run `git diff --cached --name-only` first; if it lists
  anything, back off and re-check on a 5s → 10s → 30s schedule until the index is
  empty, then proceed. If files are _still_ staged after the 30s wait, **stop and
  report to the user** — never commit over another agent's staged work.
- **Commit only what you changed — stage _and_ commit by path.** Stage your files
  explicitly (`git add <path>…`) and commit them by path (`git commit -- <path>…`);
  verify with `git diff --cached --name-only` that the staged set is _only_ yours.
  **Never** `git add -A` / `.` / `-u`, and **never** `git commit -a` / `-am` or a
  bare index-wide `git commit` — each sweeps another session's staged or modified
  files into your commit. If staging or committing hits a blocker, pause and ask.
- **Commit once the work is fully complete.** As long as the work is done with no
  open decision or unanswered question left for the user, go ahead and commit it
  yourself (commit only; pushing still waits for the user). Prefer small, focused
  commits over one big commit. If there's still an open question, decision, or
  unexpected tradeoff, finish the work but leave `git add` / `commit` / `push` to
  the user unless they explicitly ask.

### Commit message format

- **Subject: a capitalized, imperative summary** — no trailing period, ≤72 chars
  (`Run pool launchers through a login shell (fixes agent-not-found)`). This is a
  single crate, so subjects describe the change directly rather than carrying a
  subsystem scope; a lightweight `doc:` / `docs(remote):` prefix is fine for
  docs-only commits, but don't force a scope onto code changes. One logical change
  per commit — else split it.
- **Body** (blank line, wrapped ~72) whenever the _why_ isn't obvious from subject
  + diff: explain the motivation or the non-obvious constraint, don't narrate the
  diff. Close with a short verification note when you ran one (`clippy clean`,
  `backend tests pass`, `Verified on the host: …`).
- **No `Co-Authored-By` trailer.** Strip it if the harness appends one.
