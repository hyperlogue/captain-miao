# Remote sessions in captain-miao — from libshpool to the TUI

Everything involved in the remote-session feature, bottom-up: the pty pool
(libshpool), the per-host daemon, the wire protocol, the ssh transport, the
dashboard's backend seam, the window/binding machinery, and finally every key
and pixel in the TUI that knows about hosts. Closes with the current gaps —
including the on-server-zellij attach gap — and how the roadmap resolves them.

Code references are `path:line` in the captain-miao repo (current as of
2026-08-07). This revision **replaces** the earlier design doc wholesale,
following a multi-round design review (r3, 2026-08-06/07) whose decisions are
recorded inline as "agreed in review" / "adjudicated" marks; the superseded
revision lives in git history. Marks like "§n" and "roadmap n" cite that
superseded revision's structure and survive here as historical pointers.
Where the *target* shape and today's *implementation* differ, this doc says
so explicitly.

## 0. The shape in one screen

```
user's machine (client)                      each session host (server)
┌────────────────────────────────┐           ┌───────────────────────────────────────┐
│ dashboard (ratatui TUI)        │           │ captain-miao-server daemon (singleton) │
│                                │           │  ├─ protocol server (unix socket)      │
│  Backend[0] Local ─────────────┼─in-proc──►│  │    ▲ same LocalBackend logic        │
│  Backend[1] Remote("hostA") ───┼─socket───►│  ├─ LocalBackend (server-core)         │
│  Backend[2] Remote("hostB")    │ (ssh -L)  │  ├─ sessions/ notify watcher           │
│   each: mirror + conn task     │           │  └─ libshpool pty pool (thread)        │
│                                │           │       └─ pool session "cm-…"           │
│ Terminal (kitty/zellij ctl)    │           │            └─ launcher ─── agent       │
│ WindowBindings (token→window)  │           │                  ▲           └─ hooks ─┐│
└────────────────────────────────┘           │                  └── unix socket ──────┘│
                                             │ ~/.local/state/captain-miao/            │
                                             │   sessions/{pid}.json  ← state truth    │
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
resumables, spawn/kill, host-fs queries), and the **client** owns everything
visual and preference-y (windows, selection, pins, colors). The load-bearing
principle: **locality is invisible above the backend seam** — app code may
branch only on (1) the row's host, to route; (2) a reported capability; (3)
connection state (§1).

Workspace split (`docs/crate-split.md`): `cm-core` (shared logic/types, no TUI,
no libshpool — cross-compiles), `captain-miao` (the dashboard TUI), 
`captain-miao-server` (the per-host daemon, the binary deployed to Linux
remotes), `captain-miao-client` (thin local pool CLI: `list`/`attach`).

## 1. The foundation: libshpool and the pty pool

What makes a remote session *persistent* — surviving ssh drops, laptop sleep,
and dashboard restarts — is that it doesn't run in any terminal at all. It runs
under a **pty pool**: a daemon-held pseudo-terminal a client can attach to and
detach from at will, the same trick as tmux/screen, provided by
[libshpool](https://github.com/shell-pool/shpool) (v0.11) embedded **as a
library**.

- **Embedded, not shelled out.** captain-miao runs its own shpool daemon on a
  dedicated thread inside `captain-miao-server` (`crates/cm-server/src/pty_pool.rs`),
  on its own private socket (`cm_core::state::pool_socket_path` — shared const
  with the client crate so the path can't drift), with a config file it
  authors. A user's standalone `shpool` install shares nothing with it (§5.1).
- **Session semantics.** Pool sessions are created detached:
  `shpool attach --background --dir <cwd> --cmd '<launcher argv>'`. The
  command runs under a **login shell wrapper** (`sh -lc`, plus a sane `TERM`)
  because the pool strips the environment — PATH must be rebuilt the way a real
  login would (`crates/cm-server/src/server_pool.rs:92-111`; this fixed the
  original agent-not-found bug).
- **Detached pty size is 80×24.** A `--background` create has no client tty,
  so libshpool falls back to its default `TtySize { rows: 24, cols: 80 }`
  (libshpool `src/attach.rs:246-250`; `open_in_pool` spawns the create with
  `Stdio::null()`). The agent TUI renders at that size while nothing is
  attached; the first attach sends the client's real size and the SIGWINCH
  repaint re-lays everything out. After a detach the pty keeps its last
  attached size (resize is purely client-driven). Nothing captain-miao shows
  depends on the detached rendering — remote previews are captured from the
  local attach *window*, which only exists while attached — so the only
  visible artifact is the momentary repaint on attach, and `simple` restore
  (below) means no 80-column-wrapped scrollback is ever replayed at the new
  width.
- **TERM is `xterm-256color`, fixed at creation.** The wrapper upgrades only
  an empty/`dumb` TERM (`server_pool.rs:41-42`) — and since the session is
  created detached there is no attaching terminal to copy from, so that is
  what a pool session gets. This is the *correct* choice, not just a
  fallback: a process's environment can't change after start, while different
  terminals may attach over the session's life (kitty today, a zellij pane
  tomorrow), and a client-specific TERM like `xterm-kitty` breaks on any host
  missing that terminfo — the same reason tmux pins `tmux-256color`. What a
  kitty user gives up inside the session: terminfo/TERM-gated kitty features
  don't engage, and escape-*query*-detected features (e.g. the kitty keyboard
  protocol) get no reply at detached startup, so apps settle to conservative
  defaults there too. TERM has no bearing on the query-negotiated features:
  the app writes a query escape and enables the feature only if the live
  terminal replies, so `TERM=xterm-kitty` wouldn't turn them on — it would
  only sway TERM-sniffing apps, at the cost of lying to every non-kitty
  attacher and of broken terminfo on hosts without kitty's entry. In the
  standard dashboard flow the degradation is rarer than it sounds: the attach
  window is spawned immediately after the create and usually connects before
  the agent's TUI finishes booting through the login shell, so startup
  queries typically *are* answered by the real terminal. The residual cases:
  a session left genuinely detached at boot, and reattaching from a
  *different* terminal later — negotiated state is terminal-side, and shpool
  won't re-negotiate on the app's behalf (tmux can, only because it
  implements the protocol itself). Truecolor is usually gated on
  `COLORTERM`, which the
  pool strips — exporting `COLORTERM=truecolor` in `POOL_SHELL` would be a
  cheap, low-risk upgrade (every terminal in this ecosystem supports 24-bit).
- **OSC 52 (clipboard) works end-to-end.** libshpool's live relay is a
  transparent byte pipe — its source contains no OSC handling at all (the
  vterm engine exists only for the `screen`/`lines` restore buffer, unused in
  `simple` mode) — and ssh passes tty bytes verbatim, so a clipboard write
  from the agent reaches the attaching terminal intact. The only gate is that
  terminal's own policy: kitty's `clipboard_control` (writes allowed by
  default), or zellij's own OSC 52 handling for an attach running in a pane.
- **Restore mode is `simple`** (`pty_pool.rs:32-40`): reattach = reconnect +
  SIGWINCH, **no scrollback replay**. Fine for full-screen agent TUIs, which
  repaint on resize anyway.
- **One client at a time — but libshpool natively supports stealing.** A
  pool session that already has a terminal attached declines a second attach
  (`crates/cm-client/src/pool.rs:136-138` guards this explicitly; the
  `captain-miao-server attach` path used by dashboard windows relies on
  libshpool's own busy behavior). libshpool's attach client, however,
  already implements the whole steal under its `--force` flag: on a busy
  session it sends a `Detach` (kicking the other client — that client's
  attach process simply exits, which the kicked side's dashboard already
  handles as a detach-by-window-close) and retries the dial, up to 20×100ms
  (libshpool `src/attach.rs:158-200`). The session itself is undisturbed —
  detach is clean, nothing restarts. captain-miao doesn't expose the flag
  yet; force-attach is an agreed follow-up (§10.2).
- **Naming.** The daemon mints `cm-<agent>-<pid>-<seq>`
  (`server_pool.rs:78`) — the pool session name is also the session↔window
  *binding token* (§6).

The pool is why kill/detach have clean semantics: **detach** closes the local
window and touches nothing on the host — the launcher keeps running in its
pool pty; **kill** signals the agent and the whole triple tears down, pool
session included.

## 2. The per-host daemon: `captain-miao-server`

One persistent process per host, doing two jobs with one lifetime: it **hosts
the pool** and it **answers the protocol**. That coupling is deliberate — the
thing that owns pooled sessions and the thing that reports them can't disagree
about what exists (§2).

- **Server-core = `LocalBackend`.** The daemon wraps the *same*
  `cm_core::backend::LocalBackend` struct the dashboard uses for localhost
  (`crates/cm-server/src/server.rs:346`): reading state files, overlaying
  Codex sqlite titles, listing resumables, planning launches, host-fs queries
  — written once, so the in-process path and the wire path cannot drift.
- **Lifecycle** (`server.rs`, §5.1): self-daemonizing (`daemon ensure`
  double-forks + `setsid`, detaching from the ssh channel that started it —
  this is what survives disconnects); singleton via `flock(server.pid)` (the
  *lock* is the gate, not the pid file, so a dead daemon can never wedge it);
  idempotent to start; **auto-exits when idle** — the watchdog (30s ticks,
  `IDLE_GRACE` 300s, `server.rs:44-47`) exits once there are **no pool
  sessions and no connected clients** for 5 minutes, so the daemon dies
  shortly after the last session does. Both conditions matter: exiting while
  a client is still connected would just trip that client's reconnect loop
  into re-`ensure`-ing it (an exit/restart cycle — the very fact that
  `daemon ensure` runs on every connect is why exit must wait for the
  clients to leave), and the grace absorbs kill-and-reopen churn. The count
  is *pool* sessions only (`server.rs:189-193`) — a session running outside
  the pool never pins the daemon. CLI: `daemon ensure` (start + print socket
  path) / `print-path` / `status` / `stop [--force]`.
- **Two hardening items (adjudicated)**: (1) the accept loop currently
  propagates any accept error (`accepted.context("accept")?`,
  `server.rs:369-370`) — and since the daemon *is* the pool, one transient
  EMFILE kills every session on the host; it must log-and-continue. (2)
  systemd-logind survival joins the roadmap-1 host checklist: without
  `loginctl enable-linger`, last-logout removes `/run/user/<uid>` (and
  `KillUserProcesses=yes` distros kill the daemon outright); the non-kill
  case *wedges* — the daemon survives holding deleted sockets and the flock,
  so `daemon ensure` no-ops forever, printing a socket path nothing binds.
  `ensure` must self-heal the socket-gone state, and linger becomes a
  documented host requirement. (A connected dashboard masks all of this in
  testing; it fires exactly when the user disconnects and expects
  persistence.)
- **Two sockets, easily confused:** the **control socket** (the protocol; what
  `daemon ensure` prints; what the dashboard forwards/dials) and the **pool
  socket** (libshpool's own; what `attach` and `cm-client list` dial). They
  live in the same runtime dir but are distinct endpoints.
- **Watchers.** The daemon `notify`-watches `sessions/` plus Codex's title
  WAL, feeding a broadcast channel that drives the per-connection push stream.
- **Snapshot = every state file.** `handle_conn`'s snapshot is just
  `LocalBackend::list_sessions()` (`server.rs:461-463`) — pooled or not. This
  is why sessions the on-server dashboard spawned into zellij panes *appear*
  on a remote dashboard even though they can't be attached (§8.1).

Also in the crate: the `claude`/`codex`/`hook` launcher entrypoints (the same
`cm_core::launcher::run` the local binary uses — the triple inside the pool is
byte-for-byte the local triple, `crates/cm-server/src/main.rs:35-45`), `attach`
(proxies the pty via `libshpool::run`, `pty_pool.rs:167-197`), and
`pty-daemon`. Headless — no terminal-emulator requirement.

## 3. The wire protocol (`crates/cm-core/src/protocol.rs`, 218 lines)

Length-prefixed JSON frames (4-byte BE length + serde JSON, 8 MiB inbound cap)
over a Unix socket. One connection per host carries two interleaved
conversations (§4):

```
client → server                      server → client
─────────────────────────────────    ─────────────────────────────────────────
Hello{client_version, protocol}      Welcome{server_version, protocol, host}
Subscribe                            Snapshot{sessions}        then push:
                                     Delta{state}  |  Removed{launcher_pid}
ListResumable{req_id, limit}         Resumable{req_id, candidates, errors}
KillSession{req_id, child_pid}       Killed{req_id, ok}
OpenSession{req_id, spec}            Opened{req_id, session_name? | error?}
ListRecentDirs{req_id}               RecentDirs{req_id, cwds, home}
CompletePath{req_id, prefix}         PathCompletions{req_id, matches}
CheckDir{req_id, path}               DirChecked{req_id, exists}
```

- `PROTOCOL_VERSION` = 3; the server always answers `Hello` with `Welcome`
  (so an old client can *report* a mismatch) then closes on disagreement.
- Deltas are **per-session, full-state**: each connection diffs against what
  *it* last sent, so a late subscriber is correct from its own Snapshot on and
  the server keeps zero cross-connection state.
- Known wart — **agreed in review to fix early, not "eventually"**: the wire
  leaks the session-key encoding — `Removed` carries the launcher pid,
  `KillSession` the agent pid. The target is one opaque `SessionKey` on seam
  + wire. This should land *before* pooled localhost adds more clients that
  would depend on the leaked encoding — one protocol bump, taken once.
  **Adjudicated target — a coherent session-identity model**: `SessionKey`,
  minted by the owning backend, is the *only* identifier crossing seam or
  wire; the server re-resolves key → current pid from the state file **at
  signal time** (kill is today a blind `SIGTERM(child_pid)`,
  `cm-core/backend.rs:215-217` — a mis-kill hazard under mirror staleness +
  pid reuse); `backend_for` errors on an unknown host instead of falling
  back to `backends[0]`; and the session-index merge becomes host-qualified
  (today `refresh_session_index` merges per-host shards last-writer-wins on
  bare pid, `app/mod.rs:1880-1890`, so a remote/local pid collision hands a
  local row the remote's identity — flowing into restart/fork/crash
  recovery).
- **Upgrade story (adjudicated)**: the planned bump is the **last refusing
  bump**. It ships with forward-tolerant decoding — additive frames,
  unknown-variant tolerance, refusal only below a version floor — so later
  protocol changes stop stranding deployed daemons. The sharper half of the
  problem: the daemon hosts the pool and pool children die with it (session
  leaders on its pty masters), so today "restart the daemon to upgrade"
  means killing every pooled session on the host. Splitting the pool into a
  separately-stable process (the `pty-daemon` entrypoint already exists)
  gets evaluated when pooled-localhost lands — the point where the blast
  radius starts including the user's own machine.
- **`$HOME` leaves the wire entirely (agreed in review).** An earlier target
  parked the host home in `Welcome` so the client could collapse paths to
  `~/…` for display and expand typed `~`. The review pushed one step further
  and it holds up: make the **host-canonical `~` form the wire format
  itself**. The server collapses every path it returns (`~`-prefixed when
  under the host home, absolute otherwise) and expands `~` in every path it
  receives (`CompletePath` prefixes, `CheckDir`, `OpenSpec.cwd`); the daemon
  likewise collapses `LauncherState.cwd` before `Snapshot`/`Delta` (an
  overlay, like titles — the state file on disk stays absolute). The client
  becomes fully home-ignorant: what it displays *is* the wire string, and
  submit round-trips it back verbatim. The local backend applies the same
  collapse so the two arms stay indistinguishable. Two pleasant
  consequences: `RecentDirs.home` gets *deleted* rather than relocated, and
  cwd-keyed client state (directory marks) becomes home-relative — the same
  repo path on two hosts shares its icon. One implementation care: paths
  handed to shells (the `w` tab's `cd`) must be expanded host-side or left
  to the remote shell to expand — never quoted into inertness. Challenged
  in the first-principles review (mixed-form strings, migration, the shell
  landmine) and **reaffirmed** with two hardenings: the wire form is a
  **single canonical spelling** — the server *always* collapses a
  path under home, so `~/abc` simply *is* that path's one identity, never
  an alternate of an absolute twin (the underlying assumption is now
  explicit: single-user servers — one account, one home); and the
  collapse∘expand round-trip gets property-tested on both backend arms.

## 4. Getting there: transports and the connection task

`RemoteBackend` (in `src/backend.rs`) owns a background **connection task**
per host; the dashboard thread never does socket I/O for reads. The task runs
the full sequence and re-runs it on every reconnect (§5.1):

1. **Probe** — `ssh <target> 'echo $HOME; uname -sm; <version checks>'`.
   Provisioning is **read-only**: it *chooses* between a matching
   `captain-miao-server` on PATH and one at the cache path
   (`REMOTE_CACHE_REL`); `redeploy.sh` is what puts a binary there. (The old
   self-upload died with the crate split — the dashboard no longer links the
   pool, so the binary it could send wouldn't be a functional server.)
   **Agreed v1 distribution story (review): assume it's already there,
   verify, and fail loudly** — which is what the probe does, minus the
   "loudly": today a missing/mismatched binary surfaces as a generic
   connection failure. The missing piece is `ConnState::Failed{reason}`
   (roadmap 2) so the header can say *"hostA: captain-miao-server missing /
   version mismatch (found 0.3.1, need 0.4.x)"* instead of a silent `⚠`.
   Anything smarter (a `deploy` command wrapping `redeploy.sh`'s
   scp-the-artifact step) is a later, deliberately dumb convenience — no
   auto-upload magic.
2. **Ensure** — `ssh <target> <exe> daemon ensure` → prints the control-socket
   path; idempotent.
3. **Forward** — cancel any stale forward, then a **forward-only**
   `ssh -N -L <local>:<remote> <target>` child (`kill_on_drop`), under
   `ControlMaster=auto` + per-host `ControlPath` + `BatchMode` (key/agent auth
   only). Steps 1–3 ride one authenticated TCP connection. Control sockets
   live in a flat `cm-<uid>` dir to stay under the ~104-byte `sockaddr_un`
   limit. Attach windows currently do **not** ride this connection — they are
   ordinary `ssh -t` processes dialing their own. Making them share the
   ControlMaster is a one-line argv change (same `-o ControlPath`) with a
   real win — attach opens skip auth entirely (instant, no 2FA re-prompt) —
   and one deliberate cost: **shared fate.** OpenSSH multiplexes channels
   over the master's single TCP connection, so a master death detaches every
   attach window on that host at once (the pooled sessions survive; each
   window is one `Enter` to reattach — benign, but visible). Agreed in
   review to adopt it, validated during host verification (roadmap 1).
4. **Connect** — dial the local socket (with retry; the far end binds a beat
   later).
5. **Handshake** — `Hello ⇄ Welcome` → `Subscribe` → `Snapshot`.
6. **Serve** — until drop. On any loss: kill the tunnel child, **clear the
   mirror** (no stale rows), mark `Disconnected`, back off 500ms → 30s (reset
   only after ≥20s healthy, so a flapping host can't storm), retry.

Key decoupling: **the daemon and the tunnel have independent lifetimes.** A
dashboard disconnect/reconnect kills only the `-N -L` child; the daemon and
every pooled session in it persist. (This replaced the original
"server-inside-the-ssh-channel" model, whose lifetime was tied to one client —
the disconnect bug.)

The only other transport is `Transport::Socket` — dial a daemon socket **on
this same machine**, skipping steps 1–4. This is the pooled-localhost
transport (§8.1); the design doc renames it `LocalSocket` and makes local-only
part of its contract (roadmap 2). `RemoteBackend::connect` already sets
`attach_target: None` for it, which makes attach argv a bare
`captain-miao-server attach <name>` with no ssh (`src/backend.rs:296-299,
439-452`; unit-tested at 1153-1175).

## 5. The dashboard side: the `Backend` seam

`Backend` (`src/backend.rs:47-228`) = `Local(LocalBackend) |
Remote(RemoteBackend)`, ~13 methods, and the surfaces are congruent — every
method exists on both, which is what makes rows from different hosts
indistinguishable to the app layer. Uniformly dispatched: `list_sessions`,
`list_resumable`, `kill_session`, `open_session`, `session_index`,
`recent_dirs`, `complete_path`, `dir_exists`, `host_id`, `conn_state`.

- **Reads are free.** `Remote::list_sessions` reads the in-memory mirror (the
  host's list as of the last push) — no round-trip ever. Round-trip methods
  queue a `PendingRequest` and block on a oneshot (`block_in_place`);
  against a `Disconnected` host they **fail fast** instead of hanging through
  the backoff.
- **Open is a plan, not a boolean** (§3.1). `open_session(OpenSpec{agent,
  cwd, resume?})` returns a `LaunchPlan`: `SpawnLocal{argv}` (the window IS
  the launcher, dashboard mints `--launch-id`) or `AttachRemote{argv,
  session_name}` (the daemon already started the launcher in the pool via
  `open_in_pool`, `server_pool.rs:74-176`; argv is
  `ssh -t <target> <exe> attach <name>`). The client's open path is one line
  either way: spawn the argv, bind the window. `launch_agent` resolves the
  backend by host and executes the plan uniformly (`src/app/run.rs:355-441`).
- **Today's three mechanical gaps** vs the target seam (§3, admitted in the
  doc and confirmed in code): a polled `take_dirty()` instead of
  `subscribe()` event streams (folded into the run loop's reload wake,
  `run.rs:726-736`); raw pids instead of `SessionKey`; and `Option`-returning
  `attach_argv`/`shell_argv` (`backend.rs:163-187`) instead of
  `capabilities()` + `Result`-returning `AttachPlan`/`ShellPlan`. Their
  migration is roadmap item 2.
- **Change-notification is *not* behind the seam yet.** The dashboard's run
  loop itself creates the notify watcher on the local `sessions/` dir plus
  each agent's `watch_paths()` (`run.rs:601-623`) — the local backend's
  change signal is the app's own fs watch, while remote change signals arrive
  as mirror pushes polled via `take_dirty`. So today the answer to "does the
  dashboard ever explicitly watch that directory" is yes, for localhost. The
  `subscribe()` migration is exactly the abstraction that fixes this: each
  backend feeds its own `BackendEvent` stream (the local one from a watcher
  *it* owns, mirroring how the daemon already owns its server-side watch),
  and the run loop just selects over streams. Pooled localhost (§10.1) gets
  there for free: a Socket-transport backend receives daemon-pushed deltas,
  so once `backends[0]` is pooled the dashboard needs no sessions-dir watch
  at all.

**Feature gating:** the whole remote feature ships **off by default** behind
the `remote` cargo feature, whose runtime gate is the const
`app::REMOTE_ENABLED` — deliberately not `#[cfg]` scattered across ~240
remote references. It closes the only two doors in:
`build_backends_from_config` reading `hosts.json`, and the `Space h` hosts
editor. Both configurations compile and are tested.

## 6. Sessions ↔ windows: one token mechanism

The dashboard owns every session↔window binding (§3.2). The problem: at spawn
time the launcher's pid (the row's identity) doesn't exist yet, so bindings
need a correlation token minted *before* the process:

- **Local**: the dashboard mints `--launch-id <uuid>` onto the spawn argv
  (`run.rs:392-397`).
- **Remote**: the pool session name (`--pool-session cm-…`) *is* the token —
  the daemon mints it, `Opened`/`LaunchPlan` carry it back.

Both flow identically: the launcher echoes the token onto its state file
(`LauncherState.launch_id` / `.pool_session`,
`crates/cm-core/src/state.rs:555-560`), the dashboard records `(host, token) →
window_id` in `WindowBindings` (persisted to `window-bindings.json`, re-seeded
at startup, also read by the external `focus` bell), and every window consumer
resolves through one choke point, `App::window_id_for_session`
(`src/app/mod.rs:2242-2266`). A hand-launched, token-less session is the one
exception: its launcher self-reports its own window id
(`crates/cm-core/src/launcher.rs:36-40`) and the resolver falls back to that.
Token-bearing launchers never touch the terminal — which is exactly what lets
them run headless in a pool.

Wart worth knowing: the launch-id-vs-pool-session choice is re-derived at
four call sites (window resolution, binding GC, binding re-seed, launch bind
— `mod.rs:2255-2258, 2068-2071, 2161-2171`, `run.rs:387-396`); roadmap 2's
single `binding_token()` accessor collapses all four.

## 7. Lifecycle flows, condensed (§5.2)

One discovery path covers every host: **a launcher writes its state file;
whoever watches that host's `sessions/` dir picks it up.** The dashboard never
learns of a session from the spawn call — only from the state file arriving —
so sessions opened by another dashboard, or adopted after a restart, flow
through the identical path.

- **OPEN** (`o`/`O`, resume `r`, browser `b`): local → spawn window running
  the launcher; remote → `OpenSession` RPC → `open_in_pool` (detached, no
  window) → spawn an `ssh -t … attach` window; bind `(host, name)`. The
  daemon records the remote cwd into the *host's* recent list server-side.
- **RUN**: identical everywhere — hooks → launcher socket → state file;
  transcripts folded by the launcher. Local: dashboard reload reads the file.
  Remote: daemon diffs vs last-sent → `Delta` push → mirror → dirty →
  debounced reload. Titles ride `LauncherState.name` at the source (Claude's
  launcher folds its rename; the daemon overlays Codex's sqlite title before
  push), so remote rows are titled with no extra RPC (§6).
- **ATTACH** (`Enter` on a running remote row with no window): mirror row
  carries `pool_session` → `attach_argv` → spawn the ssh-attach window → bind.
  Enter on an already-bound row just focuses.
- **AUTO-REATTACH (adjudicated requirement)**: the dashboard remembers which
  sessions had attach windows — the bindings already record the pairing;
  extend them with an *expected-attached* flag that a deliberate `D`
  clears. On a host's `Disconnected → Connected` transition, every
  remembered `(host, pool_session)` without a live window gets its attach
  window respawned into the current layout. A laptop-sleep or broken-pipe
  reconnect thus restores the whole working set without manual re-Entering,
  while a `D`-detached session stays detached.
- **DETACH** (`D`, or closing the attach window): close the local window,
  drop the binding, send *nothing* to the host. The pooled session keeps
  running; the row stays, window-less; Enter re-attaches. The reload's
  `prune_detached_sessions` treats externally-closed windows the same way
  (gated on `has_remote()` + an interval floor, `run.rs:768-805`).
- **KILL** (`x`): `KillSession{child_pid}` → daemon SIGTERMs the agent →
  launcher tears down, removes its state file → `Removed` push → row gone.
  Later the session shows in that host's resumable list; resuming is OPEN
  with `resume: Some(…)`.

## 8. State: what lives where, who writes it (§6)

Three layers, strictly ordered by authority:

1. **Truth — the launcher's state file** (`sessions/{pid}.json` on the
   session's host). One writer, atomic rename. Killing the daemon, dashboard,
   or tunnel loses nothing; state lives with the session.
2. **Server — in-memory only, all rebuildable**: per-connection `last_sent`
   diff maps, `LocalBackend` caches, the pool's ptys (which live as long as
   the daemon — the reason `daemon stop` is guarded), plus the one persisted
   thing: the host's `recent-cwds.json`.
3. **Dashboard — projections + preferences**: mirrors and the host-stamped
   row list in memory; on disk `hosts.json` (targets, labels, colors),
   `window-bindings.json`, `dashboard-overrides.json` (pins/mutes — **local
   rows only** today), `dashboard-sessions.json` (crash-recovery snapshot,
   local-only by design while restart is local-only).

Identity is `(host, launcher_pid)` everywhere in the client; `HostId` is
stamped at reload (`#[serde(skip)]` — a host doesn't know what the client
calls it) so a remote pid can't collide with a local one.

**Multi-dashboard semantics (adjudicated)**: several dashboards on one host
are supported by construction — each is just another subscriber. All shared
mutable state lives in host-fs files with **last-writer-wins** semantics,
accepted as-is: server-side flags (the daemon's sidecar) and the host's
`recent-cwds.json`. Steal-attach is an action, not state. Nothing
coordinates concurrent writers beyond atomic file replacement, by decision.

## 9. The TUI surface — everything that operates on hosts

Deliberate principle: **the remote UX reuses the local keys; the row's host
decides what they mean** (§7).

- **`Space h` — the hosts popup**: add/edit/remove hosts (label, ssh target
  *or* socket path, per-host name color), persisted to `hosts.json`; saving
  reconnects the backends. (`src/app/keys.rs:810-935`, `draw.rs:318-375`,
  `hosts.rs`.) **Agreed redesign (review)** — grow it from an edit form into
  a *hosts panel*: a list view where each host shows live connection state
  (with the `Failed{reason}` text once that lands), session counts
  (running/attached — the attached bit arrives with the libshpool overlay
  from the steal-attach work), daemon version (already in `Welcome`), and
  latency (**no dedicated `Ping` frame** — agreed in review: every
  request/response pair is already timestamped for `req_id` matching, so RTT
  is sampled opportunistically from real traffic, and the panel fires a
  cheap read when its sample is stale; a `Ping` frame only gets added if
  that ever proves insufficient). And fix
  the staged-Save flow: adding a host should persist + connect immediately
  (its conn state then animates live in the panel), edits apply on commit,
  remove behind a confirm — no separate Save step to forget.
- **Header**: today, per-host connection health — `⟳ <host>` connecting,
  `⚠ <host>` disconnected (`draw.rs:575-593`). **Agreed in review**: the
  header should carry only an aggregate — hosts configured / erroring /
  offline (e.g. `hosts 3 ⚠1`) — with all per-host detail (including the
  `Failed{reason}` text) living in the hosts panel, which is one `Space h`
  away. The header stays glanceable no matter how many hosts exist.
- **Host column**: compact, colored, shown only when remotes are configured
  (`draw.rs:745-770`) — a zero-remote user never sees a pixel of it.
  **Agreed in review**: render a per-host **emoji icon** rather than a name,
  configurable in the hosts panel exactly like the workdir icons (`Space i`
  pattern — same searchable emoji picker, per-host icon + color stored in
  `hosts.json`).
- **`Enter`** — the focus-or-attach decision (`mod.rs:2286-2315`), in order:
  foreign-terminal local row → error; local row with a resolvable window →
  focus; remote row with `pool_session` → spawn/focus the attach window;
  remote row *without* one → status line "Remote session isn't attachable yet
  (no pool session)" (`mod.rs:2307-2313`, pinned by `tests.rs:908` — this is
  the §10.1 gap). Shared by double-click, `Ctrl-1..9`, and the browser.
  **Agreed in review**: non-attachable remote rows (no `pool_session`) get
  **hidden** — filtered at reload — instead of sitting in the list as
  dead-ends. Challenged in the first-principles review (an attention state
  on a hidden row — a parked approval prompt — goes invisible remotely) and
  **reaffirmed**: *the dashboard is for actionable sessions* — a row this
  dashboard can neither attach nor act on doesn't earn a slot. The hosts
  panel's session count keeps them countable, and the host's own dashboard
  remains their surface. (If this ever needs softening, the recorded
  refinement is: hide *unless* the row has an attention state.)
- **Detached sessions (agreed redesign)**: a remote row with a
  `pool_session` but no bound window gets its own **icon** (joining the
  pinned/muted/follow-up icon set) and sorts into its own tier at the
  **bottom** of the list (below plain idle; an attention state on a detached
  row should still outrank — a parked approval prompt is urgent regardless
  of a window). `Enter` on one attaches-then-focuses *instantly* — spawn the
  attach window and focus it at once, letting the user watch the ssh
  progress in the window (this is today's behavior, now explicitly the
  contract). `D` remains the detach key. New / resumed / forked attachable
  sessions **auto-attach on create** — also today's behavior for open and
  resume (the attach window spawns in the same action); fork joins it when
  its gate lifts (below).
- **`o`** on a remote row opens another session on *that host + cwd* (its
  pool), not locally (`keys.rs:282-288`).
- **The workdir picker is host-aware**: `Ctrl-h` cycles the host the launch
  opens on (shown only when remotes exist); switching re-seeds recent dirs,
  `Tab` completion, and submit-time validation against *that machine's*
  filesystem over `ListRecentDirs`/`CompletePath`/`CheckDir`, with `~`
  resolving to the remote `$HOME` — so a remote cwd is a real remote path,
  never a local guess. A disconnected host fails the picker fast
  (`keys.rs:582-591`). **Agreed in review — these must be blazing fast,
  cache-first**: recent dirs get cached per host (seeded at connect,
  invalidated by the daemon's own `record_recent_cwd` on open) so a host
  switch renders instantly from cache; `Tab` completion stays a round-trip
  by nature (it reads the live fs) but runs async with a debounce so typing
  never blocks on the wire; validation happens only at submit. The
  transport is an established tunnel — per-op cost is one RTT, so the rule
  is simply: never put an RTT between a keystroke and its echo.
- **`r` resume / `b` browser** — today: cross-host unions (`r` merges every
  backend's resumable list; `b` searches every running + resumable session
  across hosts, host-tagged). **Agreed redesign: replace the unions with a
  "default host"** — the exact analog of the default agent (`Space a`),
  persisted in `dashboard-overrides.json`, shown in the header cluster. All
  new-session operations target it by default: `O`, `o` with no session
  selected, and `r` (which lists *one* host's resumables at a time,
  switchable in-picker with `Ctrl-h` exactly like the agent switch); `o` on
  a row keeps using that row's host + cwd, and **fork follows the focused
  session's host** — never the default. This removes the union complexity
  and makes every picker's scope explicit. With that, `b` loses most of its
  reason to exist (the table covers running, `r` covers resumable) — the
  proposal on the table is to **drop `b`** and reintroduce a cross-host
  search only if it's actually missed.
- **`D` detach** — remote-only concept (close the attach window, keep the
  pooled session; contrast `x` kill). `keys.rs:347-367`.
- **`w` work tab** — a local shell tab for a local row; an `ssh -t <host>`
  tab that cds into the session's cwd for a remote row (`run.rs:1082-1135`).
- **Fork and restart** (`f`, `Space e`/`Space E`) — still gated local-only
  today, but **agreed in review: just lift the gates** (roadmap 4 promoted
  to near-term). The seam already carries `resume: (session_id, fork)`, so
  restart is kill + reopen on the row's host and fork is the same with
  `fork = true`, landing in the host's pool and auto-attaching like any
  open.
- **Preferences**: pins/mutes/follow-ups on remote rows are session-lifetime
  (not persisted) today. **Agreed in review — persist them *server-side***,
  not just under host-qualified local keys (which was roadmap 6's original
  shape): the daemon owns a per-session flags sidecar (it must not touch the
  launcher's state file — single-writer rule), overlays it onto the rows it
  serves exactly like the Codex titles, and a `SetSessionFlags` request
  updates it. Every dashboard attached to the host then sees the same
  pins/mutes, and pooled-localhost gets the same for free. Wire cost: one
  more frame in the already-planned protocol bump — which after this round's
  simplifications is just **`SessionKey` + `SetSessionFlags`** (plus
  *deleting* `RecentDirs.home`): `Welcome.home` and `Ping` both dropped out
  of it.

### How mixed is remote support with the rest of the code?

Audited 2026-07-31: **~3,600 lines (~13% of the ~24k non-test workspace) are
remote-only, almost all quarantined** — cm-server (1,234), cm-client (387),
`protocol.rs` (218), ~90% of `src/backend.rs` (~1,120 non-test), the hosts
popup (~300). What leaks into general dashboard code is **~35 `is_local()`
branch sites** (~6% of app code; ~3% excluding the popup) — about half
semantically inherent (attach *is* different from focus; disconnect ≠ death),
most of the rest known-temporary roadmap gates. **Zero remote awareness in
the launcher, hooks, agents, and terminal subsystems.** The seam is real, not
decorative: local lifecycle routes through the same `Backend` methods. Full
rip-out would touch two crates + four modules + ~35 mechanical branch takes,
nothing below the app layer.

The one *correctness-grade* leak: `backend_for` falls back to `backends[0]`
on an unknown host (`mod.rs:1882-1887`) — a stale `HostId` silently targets
localhost. Roadmap 2 makes it an error.

## 10. Current gaps and sharp edges

### 10.1 The on-server-zellij attach gap (found 2026-07-31)

**Symptom:** run the dashboard *on* a Linux server under the zellij backend;
its sessions live as zellij floating panes. Connect from a laptop via the
remote-hosts feature: the rows appear (the daemon's Snapshot is every state
file, pooled or not — `server.rs:461-463`) but Enter dead-ends with "no pool
session".

**Root cause:** attachability is keyed on exactly one thing —
`LauncherState.pool_session` — and only `open_in_pool` ever writes it.
Dashboard-local spawns mint `--launch-id` instead; their panes are ptys owned
by the zellij server, unreachable through the pool. Not a bug so much as the
two spawn paths having different persistence stories.

**Rejected bridges:**

- *Attach via zellij itself* (`ssh -t host zellij attach <session>`): works
  today as a manual stopgap, but probe-verified costs make it wrong to build
  on — the smallest attached client dictates *everyone's* grid (a 100×30
  laptop window shrank the seat's panes to 28 rows, forcing full agent
  repaints); granularity is whole-session (you get the seat's entire zellij
  UI, dashboard included); and zellij attributes CLI actions to the
  last-typing client (verified in zellij 0.44.3 source), so scripted
  post-attach `focus-pane-id` calls are racy across two clients.
- *Late adoption into the pool*: impossible. libshpool's only
  session-creating operation is attach; re-homing a live pty across processes
  is reptyr-style ptrace surgery — a non-option for a launcher+agent+MCP
  tree.

**The real bridge is roadmap item 3, "pooled localhost"** (§8) — *and review
feedback (2026-08-06) independently converged on the same shape: run the
server daemon locally, connect the dashboard to it, and every session starts
in shpool, attachable by any client. Treating this as the agreed direction —
and per review, the mode is **opt-in** (an explicit config flag, e.g.
`[launcher] pooled = true`; default stays direct-spawn local sessions).*

**The end state is now named (adjudicated): two permanent modes, by machine
role.** Laptops run **direct-local** — no pool overhead where persistence
buys nothing (nobody remotes into a laptop; native scrollback and zero extra
hops win). Dev servers run **pooled-local**, because they have two kinds of
consumer needing the *same* attachable sessions: a laptop dashboard over the
protocol, and a phone-ssh user with no local dashboard at all — they ssh in
and run captain-miao inside a zellij session on the server, whose panes are
then just `captain-miao-server attach` clients into the pool. The attach gap
is *accepted* for direct-local laptop sessions — that population is never
served remotely.

On the server, make `backends[0]` a `RemoteBackend` over `Transport::Socket`
to the *local* daemon. Every new session then opens through `open_in_pool`, and the
zellij floating pane runs a bare `captain-miao-server attach <name>` — an
attach client like any other. Local and remote attach become symmetric, and
sessions additionally survive zellij crashes and seat logouts. Most machinery
already exists and is tested (the no-ssh attach argv, the socket transport,
the pool spawn path); the delta is: a pooled-localhost mode in
`build_backends_from_config` that **replaces** `Backend::local()` (never adds
alongside — `collect_sessions` doesn't dedup and both would read the same
`sessions/` dir), a local `daemon ensure` bootstrap, a hostname-based
`HostId` ("local" is reserved), and an `InProcess` answer for `w` shell tabs.
Migration of already-running pane sessions needs no code: kill + resume lands
them in the pool. Costs: one extra process hop per pane, no scrollback replay
on reattach, single-attach until a steal RPC exists, and session-lifetime
pins/mutes until roadmap 6.

### 10.2 Other known gaps

- **Steal-attach (agreed in review, 2026-08-06)**: today, seat attached ⇒
  laptop attach declined until the seat detaches. An earlier draft claimed
  the steal needs a daemon-side detach RPC — wrong: libshpool's attach
  client already implements it under `--force` (busy → `Detach` → retry,
  `attach.rs:158-200`), and the kicked client just exits, which the other
  dashboard's reload already treats as a window-closed detach. The real
  delta is flag plumbing + UX: thread `--force` through
  `captain-miao-server attach` (`Commands::Attach` →
  `pty_pool::run_attach` → the shpool subargs) and
  `captain-miao-client attach` (bypassing its pre-guard); in the TUI, v1 is
  an explicit force-attach action behind a y/N confirm ("kick the attached
  client?") since the dashboard can't yet see attached-state; the clean
  follow-up is the daemon overlaying libshpool's live attached bit onto the
  rows it serves (exactly like the Codex title overlay), so the UI can show
  attached/detached per row and offer the steal only when it applies.
  **Adjudication update — spike alternative pool engines first.** Before
  steal ships, price both **tmux** (`tmux -S`, private socket) and
  **zellij** behind the existing `open_in_pool`/`attach` seam. Zellij's
  isolation is probe-verified on 0.44.3: `ZELLIJ_SOCKET_DIR` gives a fully
  private session/socket namespace (a background session there is invisible
  to the user's own `zellij list-sessions`), `ZELLIJ_CONFIG_FILE`/
  `--data-dir` isolate config/state, `attach --create-background` creates
  detached sessions, and `dump-screen` provides capture. A multi-client
  engine deletes steal-attach, the attached-bit RPC, and shpool's
  busy-exit-0 ambiguity outright. tmux still edges zellij for the pool
  role: per-client size policy (`window-size latest` vs zellij's
  smallest-client-wins grid, probe-verified earlier) and protocol stability
  (zellij namespaces its sockets by IPC `contract_version_N`, so a
  contract-bumping zellij upgrade strands running sessions from the new
  binary). If shpool stays: wrap busy-exit-0 into a distinct exit code,
  refuse attach when no live `pool_session` row matches (the
  resurrection guard — a name-attach to a dead-detached shpool session
  silently spawns a bare login shell), and fix the detach-lock wedge
  before `--force` ships.
- **Migration cost if we switch engines later** (analysis for the pending
  ruling): the blast radius is confined to cm-server internals + cm-client
  — the dashboard, wire protocol, `LauncherState`, and the attach argv
  (`<exe> attach <name>`) are all engine-blind, which is the seam earning
  its keep. Concretely: rewrite `pty_pool.rs` (embedded libshpool →
  driving `zellij`/`tmux` CLI under the isolated env), rework
  `open_in_pool`'s create step (`attach --background --cmd` → background
  create + command layout; the login-shell/TERM wrapper carries over),
  swap cm-client's list/attach internals, and adjust the daemon's
  idle-count and `stop` (with per-session zellij servers the daemon stops
  *hosting* sessions at all — daemon restart stops killing sessions, which
  retires C2's sharpest edge as a side effect). Days of work, not weeks;
  no state migration. **Running sessions can't move** (same pty truth as
  ever) — the transition is kill + resume per session, or letting both
  engines coexist (separate sockets) while old sessions drain.
  **The cost of staying on shpool** is a known, bounded list: the
  single-client model and everything it forces (busy-exit-0 wrapper,
  attached-bit overlay, steal-attach + the detach-lock wedge fix — the
  three work items that exist only because of this property, and which
  become sunk cost if we migrate after building them); no remote preview
  without attach; no scrollback replay across reattach (`simple` restore);
  the resurrection guard; and the shpool keybinding trie scanning input
  (we author the config, so its detach binding can be disabled). What
  shpool buys for that: an embedded, hermetic engine with no external
  binary or version matrix on any host, in-process control with no CLI
  parsing, and the simplest possible mental model — which is why the
  cheapest decision point is *before* the steal/attached-bit work, and
  deferring that work defers the decision for free. One zellij-specific
  spike item recorded: nested attach (a zellij attach client running
  inside a zellij pane, the phone-ssh persona) likely needs the `ZELLIJ`
  env var unset in the pane, mirroring tmux's `TMUX=` convention.
- **Zellij library mode — exists, but heavy** (verified on crates.io):
  `zellij-server` 0.44.3 is published as "the server-side library for
  Zellij" (~129k lines) with `zellij-client` beside it — the zellij binary
  is a thin wrapper over them, so embedding both halves the way we embed
  libshpool is *possible*, and would keep the no-external-binary property
  while pinning client and server to one contract version by construction.
  The costs are real, though: these are the binary's internals published to
  build it, not a curated embedding API (no docs, no semver promises,
  churns every release), and the dependency tree is 47 crates including
  the `wasmi` WASM interpreter (zellij's own UI chrome runs as wasm
  plugins, so the runtime isn't optional) and `isahc` → libcurl (a C
  dependency complicating the server crate's clean cross-compile).
  Contrast libshpool: purpose-extracted for embedding, small, stable. The
  realistic ladder is: shpool embedded (simplest, single-client) → zellij
  driven as an external CLI (multi-client, host dependency + contract
  care) → zellij embedded (hermetic multi-client, heaviest build).
- **Seam migration debt** (roadmap 2): `take_dirty` polling → `subscribe()`
  streams; raw pids → `SessionKey`; `Option` argvs → `capabilities()` +
  plans; `backend_for` fallback → error; `binding_token()` accessor; then a
  clippy `disallowed-methods` quarantine on `is_local()`.
- **Remote fork/restart** (roadmap 4), **per-host keep-awake + remote
  focus/bell** (5), **host-qualified preference persistence** (6).
- **End-to-end host verification** (roadmap 1) is still the top item: the
  full remote lifecycle is implemented but pending verification against a
  real Linux host; the feature ships off by default behind the `remote`
  cargo feature until then.
