# Remote sessions in captain-miao — from libshpool to the TUI

Everything involved in the remote-session feature, bottom-up: the pty pool
(libshpool), the per-host daemon, the wire protocol, the ssh transport, the
dashboard's backend seam, the window/binding machinery, and finally every key
and pixel in the TUI that knows about hosts. Closes with what's actually left.

Code references are `path:line` in the captain-miao repo (current as of
2026-08-07). The design behind this revision came out of a multi-round review
(r3, 2026-08-06/07); the decisions it reached are recorded inline as "agreed in
review" / "adjudicated" marks. **As of 2026-08-07 all of it is implemented** —
this document describes the code as it stands, and §10 lists the two items that
remain open (end-to-end host verification, and the pool-engine ruling).

## 0. The shape in one screen

```
user's machine (client)                      each session host (server)
┌────────────────────────────────┐           ┌───────────────────────────────────────┐
│ dashboard (ratatui TUI)        │           │ miao-server daemon (singleton) │
│                                │           │  ├─ protocol server (unix socket)      │
│  Backend[0] this machine ──────┼─in-proc──►│  │    ▲ same LocalBackend logic        │
│    (or a socket to its daemon) │           │  ├─ LocalBackend (server-core)         │
│  Backend[1] Remote("hostA") ───┼─socket───►│  ├─ sessions/ notify watcher           │
│  Backend[2] Remote("hostB")    │ (ssh -L)  │  └─ libshpool pty pool (thread)        │
│   each: mirror + conn task     │           │       └─ pool session "cm-…"           │
│                                │           │            └─ launcher ─── agent       │
│ Terminal (kitty/zellij ctl)    │           │                  ▲           └─ hooks ─┐│
│ WindowBindings (token→window)  │           │                  └── unix socket ──────┘│
└────────────────────────────────┘           │ ~/.local/state/captain-miao/            │
                                             │   sessions/{pid}.json  ← state truth    │
                                             │   session-flags.json   ← host-owned     │
                                             └───────────────────────────────────────┘
```

Two facts dictate this whole topology (§1):

- **Agent + launcher + hooks are an irreducible same-host triple.** The
  launcher spawns the agent and `wait`s on it, hooks reach the launcher over a
  Unix socket, and the launcher `notify`-watches the agent's transcript and
  session files. None of that crosses a network, so the triple always runs on
  the host where the session lives — unchanged whether that host is local or
  remote. The triple never knows it's remote (verified: `HostId` appears in
  cm-core outside `state.rs` only as `HostId::local()` default-fill).
- **The terminal is the user's machine.** Windows, tabs, focus, preview
  capture all happen where the dashboard runs. A remote session never owns a
  window; a *local window attaches to it*.

So a per-host **server** owns lifecycle + objective facts (session list,
resumables, spawn/kill, host-fs queries, per-session flags), and the **client**
owns everything visual (windows, selection, colors, layout). The load-bearing
principle: **locality is invisible above the backend seam** — app code branches
only on (1) the row's host, to route; (2) a reported capability; (3) connection
state.

Workspace split (`docs/crate-split.md`): `cm-core` (shared logic/types, no TUI,
no libshpool — cross-compiles), `captain-miao` (the dashboard TUI, whose binary
is `miao`), `miao-server` (the per-host daemon, the binary deployed to
Linux remotes), `miao-client` (thin local pool CLI: `list`/`attach`).

## 1. The foundation: libshpool and the pty pool

What makes a session *persistent* — surviving ssh drops, laptop sleep, and
dashboard restarts — is that it doesn't run in any terminal at all. It runs
under a **pty pool**: a daemon-held pseudo-terminal a client can attach to and
detach from at will, the same trick as tmux/screen, provided by
[libshpool](https://github.com/shell-pool/shpool) (v0.11) embedded **as a
library**.

- **Embedded, not shelled out.** captain-miao runs its own shpool daemon on a
  dedicated thread inside `miao-server` (`crates/cm-server/src/pty_pool.rs`),
  on its own private socket (`cm_core::state::pool_socket_path` — shared const
  with the client crate so the path can't drift), with a config file it
  authors. A user's standalone `shpool` install shares nothing with it.
- **Session semantics.** Pool sessions are created detached:
  `shpool attach --background --dir <cwd> --cmd '<launcher argv>'`. The
  command runs under a **login shell wrapper** (`sh -lc`, plus a sane `TERM`)
  because the pool strips the environment — PATH must be rebuilt the way a real
  login would (`crates/cm-server/src/server_pool.rs`; this fixed the original
  agent-not-found bug). `--dir` gets the **expanded** path: it's a chdir, not a
  shell word, so a host-canonical `~` would be a literal directory name (§3).
- **Detached pty size is 80×24.** A `--background` create has no client tty,
  so libshpool falls back to its default `TtySize { rows: 24, cols: 80 }`
  (libshpool `src/attach.rs:246-250`; `open_in_pool` spawns the create with
  `Stdio::null()`). The agent TUI renders at that size while nothing is
  attached; the first attach sends the client's real size and the SIGWINCH
  repaint re-lays everything out. After a detach the pty keeps its last
  attached size (resize is purely client-driven). Nothing captain-miao shows
  depends on the detached rendering — previews are captured from the local
  attach *window*, which only exists while attached — so the only visible
  artifact is the momentary repaint on attach, and `simple` restore (below)
  means no 80-column-wrapped scrollback is ever replayed at the new width.
- **TERM is `xterm-256color`, fixed at creation.** The wrapper upgrades only
  an empty/`dumb` TERM (`server_pool.rs`) — and since the session is created
  detached there is no attaching terminal to copy from, so that is what a pool
  session gets. This is the *correct* choice, not just a fallback: a process's
  environment can't change after start, while different terminals may attach
  over the session's life (kitty today, a zellij pane tomorrow), and a
  client-specific TERM like `xterm-kitty` breaks on any host missing that
  terminfo — the same reason tmux pins `tmux-256color`. What a kitty user gives
  up inside the session: terminfo/TERM-gated kitty features don't engage, and
  escape-*query*-detected features (e.g. the kitty keyboard protocol) get no
  reply at detached startup, so apps settle to conservative defaults there too.
  TERM has no bearing on the query-negotiated features: the app writes a query
  escape and enables the feature only if the live terminal replies, so
  `TERM=xterm-kitty` wouldn't turn them on — it would only sway TERM-sniffing
  apps, at the cost of lying to every non-kitty attacher and of broken terminfo
  on hosts without kitty's entry. In the standard dashboard flow the
  degradation is rarer than it sounds: the attach window is spawned immediately
  after the create and usually connects before the agent's TUI finishes booting
  through the login shell, so startup queries typically *are* answered by the
  real terminal. The residual cases: a session left genuinely detached at boot,
  and reattaching from a *different* terminal later — negotiated state is
  terminal-side, and shpool won't re-negotiate on the app's behalf (tmux can,
  only because it implements the protocol itself). Truecolor is usually gated
  on `COLORTERM`, which the pool strips — exporting `COLORTERM=truecolor` in
  `POOL_SHELL` would be a cheap, low-risk upgrade (every terminal in this
  ecosystem supports 24-bit).
- **OSC 52 (clipboard) works end-to-end.** libshpool's live relay is a
  transparent byte pipe — its source contains no OSC handling at all (the
  vterm engine exists only for the `screen`/`lines` restore buffer, unused in
  `simple` mode) — and ssh passes tty bytes verbatim, so a clipboard write
  from the agent reaches the attaching terminal intact. The only gate is that
  terminal's own policy: kitty's `clipboard_control` (writes allowed by
  default), or zellij's own OSC 52 handling for an attach running in a pane.
- **Restore mode is `simple`** (`pty_pool.rs`): reattach = reconnect +
  SIGWINCH, **no scrollback replay**. Fine for full-screen agent TUIs, which
  repaint on resize anyway.
- **One client at a time — with an explicit steal.** A pool session that
  already has a terminal attached declines a second attach; `--force` steals
  it instead (§10.2 — implemented). libshpool's attach client implements the
  whole steal: on a busy session it sends a `Detach` (kicking the other client,
  whose attach process simply exits — which that dashboard already handles as a
  detach-by-window-close) and retries the dial, up to 20×100ms (libshpool
  `src/attach.rs:158-200`). The session itself is undisturbed; detach is clean,
  nothing restarts.
- **Naming.** The daemon mints `cm-<agent>-<pid>-<seq>`
  (`server_pool.rs`) — the pool session name is also the session↔window
  *binding token* (§6).

The pool is why kill/detach have clean semantics: **detach** closes the local
window and touches nothing on the host — the launcher keeps running in its
pool pty; **kill** signals the agent and the whole triple tears down, pool
session included.

## 2. The per-host daemon: `miao-server`

One persistent process per host, doing two jobs with one lifetime: it **hosts
the pool** and it **answers the protocol**. That coupling is deliberate — the
thing that owns pooled sessions and the thing that reports them can't disagree
about what exists.

- **Server-core = `LocalBackend`.** The daemon wraps the *same*
  `cm_core::backend::LocalBackend` struct the dashboard uses for this machine
  (`LocalBackend::server_core()`): reading state files, overlaying Codex sqlite
  titles, listing resumables, planning launches, host-fs queries — written
  once, so the in-process path and the wire path cannot drift. `server_core()`
  additionally owns the two things only a *serving* backend has: the
  per-session flags sidecar and the pool's live attached bit, both overlaid
  onto the rows it serves (§8, §10.2).
- **Lifecycle** (`server.rs`): self-daemonizing (`daemon ensure` double-forks +
  `setsid`, detaching from the ssh channel that started it — this is what
  survives disconnects); singleton via `flock(server.pid)` (the *lock* is the
  gate, not the pid file, so a dead daemon can never wedge it); idempotent to
  start; **auto-exits when idle** — the watchdog (30s ticks, `IDLE_GRACE` 300s)
  exits once there are **no pool sessions and no connected clients** for 5
  minutes, so the daemon dies shortly after the last session does. Both
  conditions matter: exiting while a client is still connected would just trip
  that client's reconnect loop into re-`ensure`-ing it (an exit/restart cycle —
  the very fact that `daemon ensure` runs on every connect is why exit must
  wait for the clients to leave), and the grace absorbs kill-and-reopen churn.
  The count is *pool* sessions only — a session running outside the pool never
  pins the daemon. CLI: `daemon ensure` (start + print socket path) /
  `print-path` / `status` / `stop [--force]`.
- **Two hardening items, both landed.** (1) The accept loop **logs and
  continues** rather than propagating: the daemon *is* the pool, so returning
  on one transient EMFILE would kill every session on the host. A failed accept
  backs off 200ms so a persistent fd exhaustion can't spin the loop hot.
  (2) The **socket-gone wedge** self-heals. Without `loginctl enable-linger`,
  systemd-logind removes `/run/user/<uid>` at last logout, unlinking the
  control socket out from under a daemon that survives holding deleted inodes
  and the flock — so `daemon ensure` no-ops forever, printing a socket path
  nothing binds. Two layers fix it, in order of preference:
  * **Daemon-side rebind** (`rebind_if_socket_vanished`, a 5s stat tick):
    the daemon notices its socket path is gone and re-binds once the runtime
    dir is back — which is the next login, since a non-root user can't
    recreate `/run/user/<uid>` itself. **Every pooled session survives**, which
    is why this is the primary heal.
  * **`ensure`-side restart** (`heal_wedged_daemon`), the backstop: a lock
    held with an unreachable socket gets a 3s grace (in case a rebind is
    imminent), then SIGTERM → SIGKILL, and `ensure` starts a fresh daemon.

  **`loginctl enable-linger` is a documented host requirement** — see
  `README.md`. On `KillUserProcesses=yes` distros the daemon is killed outright
  at logout and only linger prevents it.
- **Two sockets, easily confused:** the **control socket** (the protocol; what
  `daemon ensure` prints; what the dashboard forwards/dials) and the **pool
  socket** (libshpool's own; what `attach` and `cm-client list` dial). They
  live in the same runtime dir but are distinct endpoints.
- **Watchers.** The daemon `notify`-watches `sessions/` plus Codex's title
  WAL, feeding a broadcast channel that drives the per-connection push stream.
  A `SetSessionFlags` also pokes that channel, so a flag another dashboard set
  reaches every subscriber as an ordinary `Delta`.
- **Snapshot = every state file.** `handle_conn`'s snapshot is just
  `LocalBackend::list_sessions()` — pooled or not. Sessions an on-server
  dashboard spawned into zellij panes therefore *appear* on the wire; the
  client hides them, since it can't attach to them (§9, §10.1).

Also in the crate: the `claude`/`codex`/`hook` launcher entrypoints (the same
`cm_core::launcher::run` the local binary uses — the triple inside the pool is
byte-for-byte the local triple), `attach` (proxies the pty via `libshpool::run`),
and `pty-daemon`. Headless — no terminal-emulator requirement.

## 3. The wire protocol (`crates/cm-core/src/protocol.rs`)

Length-prefixed JSON frames (4-byte BE length + serde JSON, 8 MiB inbound cap)
over a Unix socket. One connection per host carries two interleaved
conversations:

```
client → server                        server → client
─────────────────────────────────────  ─────────────────────────────────────────
Hello{client_version, protocol}        Welcome{server_version, protocol, host}
Subscribe                              Snapshot{sessions}         then push:
                                       Delta{state}  |  Removed{key}
ListResumable{req_id, limit}           Resumable{req_id, candidates, errors}
KillSession{req_id, key}               Killed{req_id, ok}
OpenSession{req_id, spec}              Opened{req_id, session_name? | error?}
SetSessionFlags{req_id, key, flags}    FlagsSet{req_id, ok}
ListRecentDirs{req_id}                 RecentDirs{req_id, cwds}
CompletePath{req_id, prefix}           PathCompletions{req_id, matches}
CheckDir{req_id, path}                 DirChecked{req_id, exists}
```

- `PROTOCOL_VERSION` = 4. Deltas are **per-session, full-state**: each
  connection diffs against what *it* last sent, so a late subscriber is correct
  from its own Snapshot on and the server keeps zero cross-connection state.
- **Session identity is opaque.** `SessionKey` — minted by the owning backend,
  never parsed above the seam — is the only identifier on seam or wire, and the
  mirror is keyed by it. The **server re-resolves key → current pid from the
  live state file at signal time**, which is the fix that matters: kill used to
  be a blind `SIGTERM(child_pid)` on a pid the client sent, so a mirror lagging
  a session's exit plus OS pid reuse was a mis-kill hazard. An unknown key is
  refused rather than guessed at. Two sibling leaks closed with it:
  `backend_for` **errors** on an unknown host instead of falling back to
  `backends[0]` (a stale `HostId` used to silently target localhost), and the
  session-name index is kept **per host** rather than merged on bare pids
  (`App::index_for`) — the merge let a remote pid collide with a local one and
  hand a local row the remote's session id, which then flowed into restart,
  fork, and crash recovery.
- **Upgrade story.** v4 is intended as the **last refusing bump**: decoding is
  forward-tolerant — unknown frame variants decode to `Unknown` and are ignored,
  unknown fields are skipped, new fields must be additive with `#[serde(default)]`,
  and refusal happens only *below* `PROTOCOL_MIN` (`protocol_compatible`, pinned
  by test). A newer peer is fine in either direction, so later protocol changes
  stop stranding deployed daemons. The sharper half of the problem remains: the
  daemon hosts the pool and pool children die with it (session leaders on its
  pty masters), so "restart the daemon to upgrade" still means killing every
  pooled session on the host. Splitting the pool into a separately-stable
  process (the `pty-daemon` entrypoint already exists) is evaluated with the
  engine ruling (§10.2) — with per-session zellij/tmux servers it disappears.
- **`$HOME` has left the wire.** The **host-canonical `~` form is the wire
  format itself** (`cm_core::paths`). The server collapses every path it
  returns (`~`-prefixed when under the host home, absolute otherwise) and
  expands `~` in every path it receives (`CompletePath` prefixes, `CheckDir`,
  `OpenSpec.cwd`); the daemon likewise collapses `LauncherState.cwd` before
  `Snapshot`/`Delta` (an overlay, like titles — the state file on disk stays
  absolute). The client is fully home-ignorant: what it displays *is* the wire
  string, and submit round-trips it back verbatim. The local backend applies
  the same collapse, so the two arms are indistinguishable — pinned by
  `local_backend_speaks_host_canonical_paths`. Two pleasant consequences:
  `RecentDirs.home` is *deleted* rather than relocated, and cwd-keyed client
  state (directory marks) is home-relative — the same repo path on two hosts
  shares its icon. Three hardenings the review asked for, all in place:
  * the wire form is a **single canonical spelling** — the server *always*
    collapses a path under home, so `~/abc` simply *is* that path's one
    identity, never an alternate of an absolute twin. The underlying assumption
    is explicit: **single-user servers**, one account, one home.
  * the collapse∘expand round trip is **property-tested** on both arms
    (`collapse_expand_round_trips_for_awkward_paths`), including the
    component-boundary case that a naive `starts_with` gets wrong (`/home/us`
    against a `/home/user` home).
  * the **shell landmine** is closed by construction: a `~` path spliced into a
    remote command line goes through `paths::shell_quote_host_path`, which
    emits `"$HOME"'/proj'` so the *remote* shell expands the tilde while the
    remainder stays quoted. Single-quoting it — the obvious thing — would make
    `cd '~/proj'` fail on every host.

## 4. Getting there: transports and the connection task

`RemoteBackend` (in `src/backend.rs`) owns a background **connection task**
per host; the dashboard thread never does socket I/O for reads. The task runs
the full sequence and re-runs it on every reconnect:

1. **Probe, then provision** —
   `ssh <target> 'echo $HOME; uname -sm; <version checks>; <digest marker>'`,
   then decide: a version-matching `miao-server` on PATH, else one at
   the cache path (`REMOTE_CACHE_REL`), else **deploy the one this dashboard
   carries**.

   The self-upload that died with the crate split is back, on a sounder
   footing: what it sends is a real `miao-server`, cross-built and
   embedded by `build.rs` in the same command that builds the dashboard, rather
   than the dashboard binary (which no longer links the pool and so wouldn't be
   a functional server). It streams into `cat` over the connection the probe just
   opened, and is `chmod`ed and **run on the host** before being moved into
   place — so a truncated transfer or a wrong-ABI payload never becomes the
   binary the next connect invokes. Ownership rule: **PATH is the user's, the
   cache path is ours** — a binary the user installed is never overwritten, and
   a digest marker beside ours distinguishes *this* build from a same-versioned
   one, which is what makes the dev loop work without a version bump. Full
   design in `docs/crate-split.md`.

   Where a deploy isn't possible — no payload for that arch, or a build that
   embedded none (the default) — the original story stands, **assume it's already
   there, verify, and fail loudly**, and it is loud: `provision_failure` turns a
   fall-back into a sentence (*"miao-server version mismatch (found
   0.3.1, need 0.4.0)"*, *"…not found (need 0.4.0); no payload for Linux
   riscv64 (this build carries x86_64-unknown-linux-gnu) — install it on the
   host"*, *"could not deploy miao-server: <what the host
   said>"*) that becomes the host's `ConnState::Failed` text and shows verbatim
   in the hosts panel. The advice names no repo script: `redeploy.sh` is a
   dev-loop convenience here, not something an installed user has.

   That text is **held across the retry**. The reconnect loop re-dials on a
   backoff, and storing `Connecting` at the top of each pass blinked the reason
   off and back once per tick — unreadable at 500ms, and worse, it read as a
   transient. A diagnosis now stands until an attempt concludes something else
   (`standing_failure` in `connection_task`), so the panel shows one steady
   sentence and the `⚠` stays lit.
2. **Ensure** — `ssh <target> <exe> daemon ensure` → prints the control-socket
   path; idempotent. Its stderr becomes the `Failed` reason when the probe had
   nothing to say.
3. **Forward** — cancel any stale forward, then a **forward-only**
   `ssh -N -L <local>:<remote> <target>` child (`kill_on_drop`), under
   `ControlMaster=auto` + per-host `ControlPath` + `BatchMode` (key/agent auth
   only). Steps 1–3 ride one authenticated TCP connection. Control sockets
   live in a flat `cm-<uid>` dir to stay under the ~104-byte `sockaddr_un`
   limit. **Attach and `w`-shell windows now share that same ControlMaster**
   (`attach_argv`/`remote_shell_argv` both splice `ssh_common_opts`), so
   opening one skips authentication entirely — instant, no 2FA re-prompt. The
   deliberate cost is **shared fate**: OpenSSH multiplexes channels over the
   master's single TCP connection, so a master death detaches every attach
   window on that host at once. That's benign — the pooled sessions survive,
   and each window is one `Enter` to reattach (or comes back on its own via
   the auto-reattach sweep, §7).
4. **Connect** — dial the local socket (with retry; the far end binds a beat
   later).
5. **Handshake** — `Hello ⇄ Welcome` → `Subscribe` → `Snapshot`. The server's
   version is kept for the hosts panel; a peer below `PROTOCOL_MIN` is refused
   with a `Failed` reason naming both versions.
6. **Serve** — until drop. On any loss: kill the tunnel child, **clear the
   mirror** (no stale rows), mark `Disconnected`/`Failed`, back off 500ms → 30s
   (reset only after ≥20s healthy, so a flapping host can't storm), retry. Each
   `Disconnected → Connected` edge bumps a **reconnect epoch**, which is what
   the auto-reattach sweep watches (§7).

Round-trip time is sampled from ordinary request traffic (`RemoteBackend::request`
times the oneshot) — there is deliberately **no `Ping` frame**: every reply is
already `req_id`-matched, so timing one is free.

Key decoupling: **the daemon and the tunnel have independent lifetimes.** A
dashboard disconnect/reconnect kills only the `-N -L` child; the daemon and
every pooled session in it persist. (This replaced the original
"server-inside-the-ssh-channel" model, whose lifetime was tied to one client —
the disconnect bug.)

The other transport is `Transport::LocalSocket` — dial a daemon socket **on
this same machine**, skipping steps 1–4. Local-only is part of its contract,
not an accident: it's the pooled-localhost transport (§10.1), so its attach
argv is a bare `miao-server attach <name>` with no ssh, and its `w`
shell is opened in process.

## 5. The dashboard side: the `Backend` seam

`Backend` = `Local(LocalHost) | Remote(RemoteBackend)`, and the surfaces are
congruent — every method exists on both, which is what makes rows from
different hosts indistinguishable to the app layer: `list_sessions`,
`list_resumable`, `kill_session`, `open_session`, `set_session_flags`,
`session_index`, `recent_dirs`, `complete_path`, `dir_exists`, `attach_plan`,
`shell_plan`, `capabilities`, `subscribe`, `host_id`, `conn_state`,
`daemon_version`, `latency`.

- **Reads are free.** `Remote::list_sessions` reads the in-memory mirror (the
  host's list as of the last push) — no round-trip ever. Round-trip methods
  queue a `PendingRequest` and block on a oneshot (`block_in_place`); against a
  `Disconnected`/`Failed` host they **fail fast** instead of hanging through
  the backoff.
- **Open is a plan, not a boolean.** `open_session(OpenSpec{agent, cwd,
  resume?})` returns a `LaunchPlan`: `SpawnLocal{argv}` (the window IS the
  launcher, dashboard mints `--launch-id`) or `AttachRemote{argv,
  session_name}` (the daemon already started the launcher in the pool via
  `open_in_pool`; argv is `ssh -t <target> <exe> attach <name>`, or a bare
  `attach` under pooled-localhost). The client's open path is one line either
  way: spawn the argv, bind the window. `launch_agent` resolves the backend by
  host and executes the plan uniformly.
- **Change notification is behind the seam.** Every backend answers
  `subscribe()` with a `BackendEvents` handle: the local one is fed by a
  `notify` watcher **the backend owns** (`sessions/` plus each agent's
  `watch_paths()`), a remote one by its connection task's mirror pushes and
  connect/disconnect transitions. The run loop drains one handle per backend
  and has no filesystem knowledge of its own — so the answer to "does the
  dashboard watch that directory" is now *no, its local backend does*, and
  pooled-localhost gets there for free (that backend has no watcher to own).
  The handle is drained on the loop's existing tick rather than awaited,
  because the main loop is crossterm-poll-driven; that is exactly where the old
  fs-event drain sat, so nothing got slower.
- **Plans, not `Option`s.** `attach_plan(name, force)` and `shell_plan(cwd)`
  return `Result`, so a host that can't do the thing *explains itself* instead
  of handing back a bare `None` the caller has to invent a message for.
  `shell_plan` answers `InProcess{cwd}` for this machine (including
  pooled-localhost, where the "remote" host is us) and `Spawn{argv}` for an ssh
  host. `capabilities()` reports `{pooled, shell}`, and app code asks *"does
  this host pool its sessions?"* rather than *"is this host local?"* — which is
  what makes `D` (detach) and the steal work identically under pooled-localhost.
- **One binding token.** `LauncherState::binding_token()` — `pool_session`
  when pooled, else `launch_id` — replaces the four sites that used to
  re-derive the choice from `host.is_local()` (window resolution, binding GC,
  binding re-seed, launch bind). Keyed on *pooled-ness*, not host, so a pooled
  local session takes the pool name like any other.

**Feature gating:** the *remote-hosts* half ships **off by default** behind the
`remote` cargo feature, whose runtime gate is the const `app::REMOTE_ENABLED` —
deliberately not `#[cfg]` scattered across ~240 remote references. It closes the
only two doors in: `build_backends_from_config` reading `hosts.json`, and the
`Space h` hosts panel. Both configurations compile and are tested.
Pooled-localhost is deliberately *not* behind it — it uses no ssh and has its
own config flag.

## 6. Sessions ↔ windows: one token mechanism

The dashboard owns every session↔window binding. The problem: at spawn time the
launcher's pid (the row's identity) doesn't exist yet, so bindings need a
correlation token minted *before* the process:

- **Direct-local**: the dashboard mints `--launch-id <uuid>` onto the spawn argv.
- **Pooled** (remote or local): the pool session name (`--pool-session cm-…`)
  *is* the token — the daemon mints it, `Opened`/`LaunchPlan` carry it back.

Both flow identically: the launcher echoes the token onto its state file
(`LauncherState.launch_id` / `.pool_session`), the dashboard records
`(host, token) → window_id` in `WindowBindings` (persisted to
`window-bindings.json`, re-seeded at startup, also read by the external `focus`
bell), and every window consumer resolves through one choke point,
`App::window_id_for_session`. A hand-launched, token-less session is the one
exception: its launcher self-reports its own window id and the resolver falls
back to that. Token-bearing launchers never touch the terminal — which is
exactly what lets them run headless in a pool.

`WindowBindings` also carries an **expected-attached** set, which deliberately
outlives the binding: `prune_dead` drops a binding when its window dies but
leaves the expectation, while an explicit `D` clears it. That one distinction —
"the link dropped" vs "you detached" — is the whole basis of auto-reattach (§7).

## 7. Lifecycle flows, condensed

One discovery path covers every host: **a launcher writes its state file;
whoever watches that host's `sessions/` dir picks it up.** The dashboard never
learns of a session from the spawn call — only from the state file arriving —
so sessions opened by another dashboard, or adopted after a restart, flow
through the identical path.

- **OPEN** (`o`/`O`, resume `r`): direct-local → spawn a window running the
  launcher; pooled → `OpenSession` RPC → `open_in_pool` (detached, no window) →
  spawn an attach window; bind `(host, name)`. The host records the cwd into
  its *own* recent list, so a mac path never pollutes a Linux box's picker.
- **RUN**: identical everywhere — hooks → launcher socket → state file;
  transcripts folded by the launcher. Local: the backend's watcher fires.
  Pooled/remote: daemon diffs vs last-sent → `Delta` push → mirror → dirty →
  debounced reload. Titles ride `LauncherState.name` at the source (Claude's
  launcher folds its rename; the daemon overlays Codex's sqlite title before
  push), so remote rows are titled with no extra RPC.
- **ATTACH** (`Enter` on a detached row): mirror row carries `pool_session` →
  `attach_plan` → spawn the attach window → bind → **focus immediately**, so
  the user watches the ssh progress in the window rather than a frozen
  dashboard. `Enter` on an already-bound row just focuses.
- **STEAL** (`Space s`): the same attach with `force`, behind a y/N confirm —
  skipped entirely when the host's attached-bit overlay says nobody is there.
- **AUTO-REATTACH**: on a host's `Disconnected → Connected` edge (tracked by
  the backend's reconnect epoch), every remembered `(host, pool_session)`
  without a live window gets its attach window respawned into the current
  layout — without stealing the cursor, since a reconnect restoring five
  windows must not fight the user for focus. A laptop-sleep or broken-pipe
  reconnect thus restores the whole working set, while a `D`-detached session
  stays detached. `App::reattach_targets` is pure over bindings + rows, so the
  edge condition is unit-tested (a host's *first* sighting is the initial
  connect, not a reconnect).
- **DETACH** (`D`, or closing the attach window): close the local window, drop
  the binding *and the expectation*, send nothing to the host. The pooled
  session keeps running; the row stays, window-less, sorted into the detached
  tier; Enter re-attaches. The reload's `prune_detached_sessions` treats
  externally-closed windows the same way, minus the intent (gated on
  `has_remote()` + an interval floor).
- **KILL** (`x`): `KillSession{key}` → the host resolves the key to a live pid
  and SIGTERMs the agent → launcher tears down, removes its state file →
  `Removed` push → row gone. Later the session shows in that host's resumable
  list; resuming is OPEN with `resume: Some(…)`.
- **RESTART / FORK**: kill + reopen **on the row's own host** (fork with
  `fork = true`), landing in that host's pool and auto-attaching like any open.
  No longer local-only.

## 8. State: what lives where, who writes it

Three layers, strictly ordered by authority:

1. **Truth — the launcher's state file** (`sessions/{pid}.json` on the
   session's host). One writer, atomic rename. Killing the daemon, dashboard,
   or tunnel loses nothing; state lives with the session.
2. **Host-owned, alongside truth** — `session-flags.json`, a
   `SessionKey → SessionFlags` sidecar the server-core owns. Deliberately a
   sidecar and not a field on the state file: that file has exactly one writer
   (its launcher), and flags are set by someone else entirely. Overlaid onto
   served rows like the Codex titles, updated by `SetSessionFlags`, and
   garbage-collected against live sessions.
3. **Server — in-memory only, all rebuildable**: per-connection `last_sent`
   diff maps, `LocalBackend` caches, the pool's ptys (which live as long as
   the daemon), plus the host's persisted `recent-cwds.json`.
4. **Dashboard — projections + preferences**: mirrors and the host-stamped row
   list in memory; on disk `hosts.json` (targets, labels, colors, icons),
   `window-bindings.json`, `dashboard-overrides.json` (pins/mutes for
   *direct-local* rows, plus keep-awake / default agent / default host /
   layout), `dashboard-sessions.json` (crash-recovery snapshot — direct-local
   by design, since a pooled session survives a dashboard crash on its own and
   "recovering" it would mean resuming a session that never stopped).

Identity is `(host, launcher_pid)` everywhere in the client; `HostId` is
stamped at reload (`#[serde(skip)]` — a host doesn't know what the client
calls it) so a remote pid can't collide with a local one.

**Multi-dashboard semantics**: several dashboards on one host are supported by
construction — each is just another subscriber, and now they agree on
pins/mutes too (the flags sidecar, pushed as a `Delta` to every subscriber).
All shared mutable state lives in host-fs files with **last-writer-wins**
semantics, accepted as-is. Steal-attach is an action, not state. Nothing
coordinates concurrent writers beyond atomic file replacement, by decision.

## 9. The TUI surface — everything that operates on hosts

Deliberate principle: **the remote UX reuses the local keys; the row's host
decides what they mean**.

- **`Space h` — the hosts panel**: a list view, not a staged edit form. Each
  host shows live connection state (including the `Failed` reason verbatim),
  running/attached session counts, the daemon version from `Welcome`, and a
  latency sample; its ssh/socket target sits on a dim second line. Editing a
  row exposes label / target (`^t` toggles ssh↔socket) / **icon** (`^e` opens
  the same searchable emoji picker as `Space i`) / color. There is **no Save
  step**: adding a host persists and connects immediately — so its state
  animates live in the list — an edit applies when you commit the row, and `d`
  removes behind a `y/N` confirm. A `Failed` reason is **flattened and
  truncated** to its row (it quotes host output, so it carries newlines that
  would corrupt the row and a length no row can hold), with the whole text one
  key away.
- **`l` — the connection log** (per host, in the panel). The row gets one line
  for a failure whose reason is routinely a paragraph, and the *sequence* is
  what diagnoses: "probed the host, decided to deploy, the deploy came back with
  this" tells you what to fix where the surviving one-liner is a symptom. So
  every step of probe → decide → deploy → `daemon ensure` → forward → handshake
  writes a line, and anything the host said is quoted **whole**. Kept in memory
  per backend (`ConnLog`, capped at 200 lines so a week-long flap is bounded),
  oldest first, ages at seconds resolution — a whole connect attempt happens
  inside one minute, so the coarse `<1m` used elsewhere would label the entire
  story identically. Pager keys; any other key is swallowed rather than falling
  through to the list underneath. This is deliberately *not* the debug log: it
  is always on, needs no config flag, and holds only this host's connection
  story.
- **Header**: an **aggregate only** — a `☁` tally of three colored numbers,
  good (green) / failing (attention) / down-or-dialing (dim), sitting
  immediately right of the default-host indicator, since both answer "which
  machines am I working across" and read as one group. An **empty bucket is
  dropped, not printed as `0`**: all-healthy is a single green number, and a
  problem announces itself by a second number *appearing* beside it rather than
  by a zero quietly changing. Every host lands in exactly one bucket, so an
  all-zero tally means no remote hosts — which is what the whole pair (tally
  *and* default host) hides on, since naming a default is meaningless when
  localhost is the only host. All per-host detail lives one `Space h` away, so
  the header stays glanceable no matter how many hosts exist.
- **Host column**: a compact, colored **emoji**, shown only when more than one
  host exists or a local row lives in another terminal instance. Per-host icons
  are configured in the panel exactly like the workdir marks, with a
  deterministic FNV-derived fallback so a host always has one. An icon rather
  than a name because the column is a glance-level "which box is this?", and a
  name either truncates to noise or eats six cells.
- **`Enter`** — the focus-or-attach decision, in order: foreign-terminal local
  row → error; a row with a resolvable window → focus; a pooled row without one
  → spawn the attach window and focus it at once. There is no fourth case:
  **non-attachable remote rows are filtered out at reload**
  (`is_actionable_row`). Challenged in the first-principles review (an
  attention state on a hidden row goes invisible remotely) and reaffirmed: *the
  dashboard is for actionable sessions* — a row this dashboard can neither
  attach nor act on doesn't earn a slot, the hosts panel's session count keeps
  them countable, and the host's own dashboard remains their surface. (If this
  ever needs softening, the recorded refinement is: hide *unless* the row has
  an attention state.)
- **Detached sessions**: a pooled row with no bound window gets its own icon
  (joining the pinned/muted/follow-up set) and its own sort tier at the
  **bottom**, below plain idle — it's running somewhere else, so it shouldn't
  compete for the eye with what's in front of you. An attention state still
  outranks: a parked approval prompt is urgent regardless of a window. New /
  resumed / forked sessions auto-attach on create.
- **`o`** on a row opens another session on *that row's host + cwd*, not
  locally. With nothing selected it targets the default host.
- **The workdir picker is host-aware and cache-first.** `Ctrl-h` cycles the
  host this launch opens on; switching re-seeds from a **per-host cache**
  (seeded on first use, invalidated when a launch records a cwd there), so a
  host switch renders instantly rather than paying an RTT. Path completion
  (`Tab`) is a live filesystem read by nature and does cross the wire;
  validation happens only at submit. The governing rule: **never put a round
  trip between a keystroke and its echo.** Everything the picker handles is in
  the host-canonical `~` form, so what's shown is what's submitted and no
  machine's `$HOME` is involved.
- **`Space H` — the default host**: the exact analog of `Space a`'s default
  agent, persisted in `dashboard-overrides.json` and shown in the header
  cluster (once more than one host exists). `O`, a bare `o`, and `r` target it;
  `o` on a row keeps that row's host, and **fork follows the focused session's
  host** — never the default.
- **`r` resume** lists **one host at a time**, named in the picker title, with
  in-picker `Ctrl-h` to switch. This replaced the cross-host union, whose scope
  was implicit and whose cost scaled with the host count. With that, **`b` (the
  cross-host browser) is gone**: the table covers running and `r` covers
  resumable. A cross-host search can come back if it's actually missed.
- **`D` detach** — a *pooled* concept (close the attach window, keep the
  session; contrast `x` kill), keyed on the capability rather than on locality
  so it works under pooled-localhost too.
- **`Space s` steal** — attach, kicking whatever client holds the session,
  behind a y/N confirm. Skipped when the host's attached-bit overlay says the
  session is free. Hidden from `?` when no host pools its sessions.
- **`w` work tab** — `shell_plan` decides: an in-process shell for this
  machine, an `ssh -t <host>` tab that cds into the session's cwd for a remote.
- **Fork and restart** work on any host.
- **Preferences**: pins/mutes/follow-ups on a pooled host's rows are stored
  **server-side** (§8) and adopted onto `App.flags` at reload, so every
  dashboard attached to that host — and a phone-ssh user on the box — sees the
  same ones, and they survive a dashboard restart. `pin_seq` stays client-side:
  pin *ordering* is presentation. Direct-local rows keep using
  `dashboard-overrides.json`.

### How mixed is remote support with the rest of the code?

Audited 2026-07-31 (pre-implementation): **~3,600 lines (~13% of the ~24k
non-test workspace) are remote-only, almost all quarantined** — cm-server,
cm-client, `protocol.rs`, ~90% of `src/backend.rs`, the hosts popup. What leaked
into general dashboard code was **~35 `is_local()` branch sites**. This round
cut into that directly: `capabilities()`, `binding_token()`, `shell_plan`, and
`is_direct_local` replaced the branches that were asking about *locality* when
they meant *pooled-ness* or *in-process-ness*, and the `backend_for` fallback —
the one correctness-grade leak — is now an error. **Still zero remote awareness
in the launcher, hooks, agents, and terminal subsystems.** The seam is real, not
decorative.

## 10. What's left

### 10.1 Pooled localhost — implemented, opt-in

**The gap it closes.** Run the dashboard *on* a Linux server under the zellij
backend; its sessions live as zellij floating panes. Connect from a laptop: the
rows appear (the daemon's Snapshot is every state file, pooled or not) but there
is nothing to attach to, because attachability is keyed on
`LauncherState.pool_session` and only `open_in_pool` writes it. Not a bug so
much as the two spawn paths having different persistence stories.

*Rejected bridges, for the record:* attaching via zellij itself
(`ssh -t host zellij attach`) works as a manual stopgap but is probe-verified
wrong to build on — the smallest attached client dictates *everyone's* grid (a
100×30 laptop window shrank the seat's panes to 28 rows), granularity is
whole-session, and zellij attributes CLI actions to the last-typing client, so
scripted post-attach `focus-pane-id` calls are racy. Late adoption into the pool
is impossible: libshpool's only session-creating operation is attach, and
re-homing a live pty across processes is reptyr-style ptrace surgery.

**The end state, now built: two permanent modes, by machine role.**

* **Laptops run direct-local** (the default). Nobody remotes into a laptop, so
  the pool buys no persistence there — only an extra hop, no scrollback replay,
  and single-attach. The attach gap is *accepted* for this population, which is
  never served remotely.
* **Dev servers run pooled-local** (`[launcher] pooled = true`), because they
  have two kinds of consumer needing the *same* attachable sessions: a laptop
  dashboard over the protocol, and a phone-ssh user with no local dashboard at
  all, who sshs in and runs captain-miao inside a zellij session on the server —
  whose panes are then just `miao-server attach` clients into the pool.

Mechanically: `build_backends_from_config` makes `backends[0]` a `RemoteBackend`
over `Transport::LocalSocket` to this host's own daemon, **replacing**
`Backend::local()` (never adding alongside — `collect_sessions` doesn't dedup
and both read the same `sessions/` dir), after bootstrapping the daemon with an
idempotent `daemon ensure`. Its `HostId` is the hostname, since `"local"` is
reserved and `is_local()` gates behaviour a pooled session genuinely doesn't
want; `w` gets an `InProcess` shell plan; and every capability-keyed affordance
(`D`, the steal, the detached tier) starts working on this machine. A missing
`miao-server` logs and falls back to direct-local rather than starting
empty. Migrating already-running pane sessions needs no code: kill + resume
lands them in the pool.

Costs, unchanged from the analysis: one extra process hop per pane, no
scrollback replay on reattach, and single-attach until you steal.

### 10.2 Steal-attach — implemented; the engine ruling is still open

`--force` is threaded through `miao-server attach` (into libshpool's
attach subargs, bypassing only the *busy* half of the pre-guard — the
stale-name/resurrection guard is never forceable, since attaching to a dead name
would silently mint a bare login shell wearing it) and through
`miao-client attach`. In the TUI it's `Space s` behind a y/N confirm,
and the daemon overlays libshpool's live attached bit onto the rows it serves
(`LauncherState.attached`, exactly like the Codex title overlay) so the UI can
tell whether anyone is actually there and skip the confirm when nobody is.

**Still open: which pool engine.** The adjudication asked to price **tmux**
(`tmux -S`, private socket) and **zellij** behind the existing
`open_in_pool`/`attach` seam before committing further to shpool. Zellij's
isolation is probe-verified on 0.44.3: `ZELLIJ_SOCKET_DIR` gives a fully private
session/socket namespace (a background session there is invisible to the user's
own `zellij list-sessions`), `ZELLIJ_CONFIG_FILE`/`--data-dir` isolate
config/state, `attach --create-background` creates detached sessions, and
`dump-screen` provides capture. A multi-client engine would delete steal-attach,
the attached-bit overlay, and shpool's busy-exit-0 ambiguity outright. tmux still
edges zellij for the pool role: per-client size policy (`window-size latest` vs
zellij's smallest-client-wins grid, probe-verified) and protocol stability
(zellij namespaces its sockets by IPC `contract_version_N`, so a
contract-bumping upgrade strands running sessions).

Note that the three work items the ruling would retire are now *built*, which
changes the calculus: they are sunk cost rather than a reason to defer. What
remains true is the migration analysis — the blast radius is confined to
cm-server internals + cm-client, because the dashboard, wire protocol,
`LauncherState`, and the attach argv (`<exe> attach <name>`) are all
engine-blind. Concretely a switch would mean: rewrite `pty_pool.rs` (embedded
libshpool → driving the `zellij`/`tmux` CLI under an isolated env), rework
`open_in_pool`'s create step (`attach --background --cmd` → background create +
command layout; the login-shell/TERM wrapper carries over), swap cm-client's
list/attach internals, and adjust the daemon's idle-count and `stop` — with
per-session servers the daemon stops *hosting* sessions at all, so a daemon
restart stops killing them, which retires the upgrade edge in §3 as a side
effect. Days, not weeks; no state migration. **Running sessions can't move**
(same pty truth as ever) — the transition is kill + resume per session, or
letting both engines coexist on separate sockets while old sessions drain.

The cost of *staying* on shpool is a known, bounded list: the single-client
model and everything it forces (the busy-exit-0 wrapper, the attached-bit
overlay, steal-attach — all now built), no remote preview without attach, no
scrollback replay across reattach (`simple` restore), the resurrection guard,
and the shpool keybinding trie scanning input (we author the config, so its
detach binding can be disabled). What shpool buys for that: an embedded,
hermetic engine with no external binary or version matrix on any host,
in-process control with no CLI parsing, and the simplest possible mental model.
One zellij-specific spike item stays recorded: nested attach (a zellij attach
client inside a zellij pane — the phone-ssh persona) likely needs `ZELLIJ`
unset in the pane, mirroring tmux's `TMUX=` convention.

*Zellij library mode* — verified on crates.io — is a third rung:
`zellij-server` 0.44.3 is published as "the server-side library for Zellij"
(~129k lines) with `zellij-client` beside it, so embedding both halves the way
we embed libshpool is possible and would pin client and server to one contract
version by construction. But these are the binary's internals published to build
it, not a curated embedding API (no docs, no semver promises, churn every
release), and the tree is 47 crates including the `wasmi` WASM interpreter
(zellij's own UI chrome runs as wasm plugins) and `isahc` → libcurl (a C
dependency complicating the server crate's clean cross-compile). The realistic
ladder: shpool embedded (simplest, single-client) → zellij driven as an external
CLI (multi-client, host dependency + contract care) → zellij embedded (hermetic
multi-client, heaviest build).

### 10.3 End-to-end host verification

**The top remaining item.** The full remote lifecycle is implemented but has not
been exercised against a real Linux host from a real laptop — the dev sandbox
has no remote. Until it has, the feature ships off by default behind the
`remote` cargo feature. The checklist for that session:

- `loginctl enable-linger` on the host, then verify the daemon survives logout
  *and* that the socket-gone rebind fires if linger is absent (§2).
- The ControlMaster sharing on attach windows (§4 step 3) — confirm an attach
  opens with no auth prompt, and that a master death detaches every window on
  that host without disturbing the pooled sessions.
- Auto-reattach across a real laptop sleep (§7), and that a `D`-detached
  session stays detached through it.
- The canonical-path round trip against a host whose `$HOME` differs from the
  client's (§3), including a `w` work tab into a `~` cwd.
- A steal from a second client, and the attached-bit overlay's accuracy (§10.2).
- Pooled-localhost on the server itself, with a phone-ssh zellij session and a
  laptop dashboard attached to the same pool (§10.1).

### 10.4 Smaller deferred items

- **Per-host keep-awake and remote focus/bell.** The sleep inhibitor and the
  `miao focus` bell are both client-side and only meaningful for sessions with a
  local window; a remote session that wants attention currently reaches the
  user through the row, not the OS.
- ~~**A `deploy` command** wrapping `redeploy.sh`'s scp step~~ — superseded.
  The dashboard now deploys its own embedded server on connect (§4 step 1), so
  a version mismatch fixes itself rather than needing a keystroke. What's left
  here is a **packaging** decision: release CI builds only the dashboard, so the
  binaries on npm and GitHub Releases carry no payload. Wiring the server into
  the release matrix (both Linux arches already build natively there) is what
  would make this zero-touch for users rather than only for source builds —
  worth doing when `remote` comes out from behind its cargo feature, and not
  before, since it adds ~7 MB to every download for a feature that's off.
- ~~**A musl payload.**~~ — done. The two musl targets are built and published,
  the deploy loops `[gnu, musl]` and keeps the first the host proves it can run,
  and a released (gnu-only) dashboard downloads the published musl asset when it
  meets a host with no generic loader. glibc stays preferred because its NSS is
  load-bearing: a static build cannot see LDAP/SSSD users, and the session fails
  to attach rather than degrading — which is why the host-run check is now
  `self-check` (it resolves the user) instead of `--version` (which never did).
- **`remote_shell_argv` doesn't survive a fish login shell.** The `w` work-tab
  command emits `cd '<dir>' && exec "${SHELL:-/bin/sh}" -l`, and
  `${SHELL:-/bin/sh}` is not fish syntax — the same class of bug the deploy path
  hit and fixed with `login_shell_safe`. It can't reuse that wrapper directly:
  the cwd is already single-quoted by `shell_quote_host_path`, which collides
  with the wrapper's own quoting, so it needs its own thought (§3's `~` handling
  is the constraint).
- **A clippy `disallowed-methods` quarantine on `is_local()`**, to keep new
  code asking about capabilities rather than locality.
