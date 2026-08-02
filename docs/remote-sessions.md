# Design: remote (SSH) session management

**Goal:** one local captain-miao dashboard monitors and manages Claude/Codex
sessions running on remote machines over SSH, with the same UX as local
sessions. Remote sessions are **persistent**: they run inside a per-host pty
pool, so they survive ssh drops, laptop sleep, and dashboard restarts, and can
be re-attached from a fresh terminal window at any time.

**Why:** agent sessions are long-running and increasingly live on remote build
or GPU boxes. Without first-class support, each remote session costs a
hand-managed ssh window, is invisible to the dashboard's status/attention
machinery, and dies with its connection. The dashboard already solves
monitoring, attention-ranking, and lifecycle for local sessions; this design
extends the same model across hosts instead of inventing a parallel one.

**Status:** the full remote lifecycle is implemented (open / resume / attach /
detach / kill / browse across hosts); pending end-to-end verification on a real
Linux host. Restart and fork are still local-only (roadmap §8).

Because it is unverified end-to-end, the feature ships **off by default**,
behind the `remote` cargo feature (`cargo build --features remote`). The gate is
the runtime const `app::REMOTE_ENABLED`, deliberately *not* `#[cfg]` scattered
across the ~240 remote references in the dashboard: the code compiles and is
type-checked/tested in both configurations, and the const closes the only two
doors that reach it — `build_backends_from_config` reading `hosts.json` to
construct a `Backend::Remote`, and the `Space h` hosts editor. With the feature
off the dashboard never opens a remote connection and every row is local.

**Binary provisioning is read-only.** The dashboard used to auto-upload *itself*
to a same-arch remote; that died with the crate split (it no longer links the
pty pool, so the binary it could send wouldn't be a functional server). The
probe now only *chooses* between a `captain-miao-server` on PATH and one at the
cache path — `redeploy.sh` is what puts a binary there. See the note above
`REMOTE_CACHE_REL` in `src/backend.rs`.

---

## 1. Topology: what must run where

Two facts about the existing system dictate the whole shape:

- **Agent + launcher + hooks are an irreducible triple.** The launcher spawns
  the agent as a child and `wait`s on it; hooks talk to the launcher over a
  Unix socket; the launcher `notify`-watches the agent's transcript and session
  files. All of that is same-host-only, so the triple always runs **on the host
  where the session lives**, unchanged whether that host is local or remote.
- **The terminal is the user's machine.** Kitty windows, tabs, focus, preview
  capture — all window control happens where the dashboard runs. A remote
  session never owns a window; a *local window attaches to it*.

So the split is:

- A **per-host server** owns session lifecycle and objective facts on that
  host: the live session list, the resumable list, spawning and killing, and
  host-filesystem queries. The launcher's state files remain the single source
  of truth on each host.
- The **client** (the dashboard) owns everything visual and preference-y:
  windows, selection, pins/mutes, previews, colors.

**The load-bearing principle: locality is invisible above the backend seam.**
The dashboard treats every host — including localhost — as a backend of the
same shape. App code above the seam may branch on exactly three things:

1. **The row's host**, and only to *route* the operation to that host's
   backend.
2. **A capability the backend reports** ("can this host produce an attach
   command?"), never "is this local".
3. **Connection state**, for fail-fast behavior and the header indicator.

Everything else — how a session is listed, spawned, killed, or titled — is the
backend's business. Presentation concerns that are genuinely local by nature
(window reuse, the host column, bells) live in a small, explicit set of
client-side sites; the roadmap (§8) hardens this boundary into a lint.

## 2. Component model: a federation of hosts, localhost is #0

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
│ Terminal (kitty remote ctl)    │           │            └─ launcher ─── agent       │
│ WindowBindings (token→window)  │           │                  ▲           └─ hooks ─┐│
└────────────────────────────────┘           │                  └── unix socket ──────┘│
                                             │ ~/.local/state/captain-miao/            │
                                             │   sessions/{pid}.json  ← state truth    │
                                             └───────────────────────────────────────┘
```

- The dashboard holds one `Backend` per host: `backends[0]` is always
  localhost, plus one `Remote` per configured host (`Space h`, `hosts.json`).
  Reload stamps each session with its backend's `HostId`; all per-row state
  keys on `(host, launcher_pid)` so pids can't collide across hosts.
- **`LocalBackend` is the server-core.** The dashboard's localhost backend and
  the remote daemon wrap the *same* struct: reading state files, overlaying
  Codex titles, listing resumables, planning launches, and answering
  host-filesystem queries are written once. The wire protocol is a thin
  serialization of that surface — the in-process path and the remote path
  cannot drift.
- The **daemon** is the single persistent process per host. It serves the
  protocol *and* hosts the pty pool, so the thing that owns pooled sessions
  and the thing that reports them are one process with one lifetime.
  Transports to it are disposable; the daemon is not.

Workspace crates (full rationale in `docs/crate-split.md`):

| crate | role |
|---|---|
| `cm-core` | shared logic + types: state, protocol, agents, launcher, `LocalBackend`. No TUI, no libshpool — cross-compiles cleanly |
| `captain-miao` | the dashboard TUI + local `claude`/`codex`/`hook`/`focus` entrypoints |
| `captain-miao-server` | the per-host daemon: protocol server + libshpool pty pool; the binary deployed to remotes |
| `captain-miao-client` | thin CLI over the local pool socket (`list`, `attach`) |

## 3. The backend seam (the key interface)

`Backend` is the dashboard's per-host handle. `Local` answers in-process;
`Remote` answers from a live mirror or by RPC. The surfaces are congruent
one-to-one — every method exists on both — which is what makes rows from
different hosts indistinguishable to the app layer. The target shape:

```rust
impl Backend {                        // Local(LocalBackend) | Remote(RemoteBackend)
    // identity & affordances
    fn host_id(&self) -> HostId;
    fn host_info(&self) -> HostInfo;         // static facts, learned once at connect
    fn capabilities(&self) -> Capabilities;  // which operations this host offers
    fn conn_state(&self) -> ConnState;

    // events — the reactive seam: one stream per backend
    fn subscribe(&self) -> Receiver<BackendEvent>;

    // facts about the host
    fn list_sessions(&self) -> Vec<LauncherState>;
    fn list_resumable(&self, limit: usize) -> Result<Vec<ResumeCandidate>>;
    fn recent_dirs(&self) -> Result<Vec<String>>;  // absolute paths
    fn complete_path(&self, prefix: &str) -> Result<Vec<String>>;
    fn dir_exists(&self, path: &str) -> Result<bool>;

    // lifecycle, keyed by an opaque per-host session key
    fn open_session(&self, spec: &OpenSpec) -> Result<LaunchPlan>;
    fn kill_session(&self, key: &SessionKey) -> Result<()>;
    fn attach_plan(&self, key: &SessionKey) -> Result<AttachPlan>;
    fn shell_plan(&self, cwd: &str) -> Result<ShellPlan>;
}

enum ConnState {
    Connecting,
    Connected,
    Disconnected,              // transient — the reconnect loop is working on it
    Failed { reason: String }, // sticky until reconfigured: protocol mismatch,
}                              // auth failure … — the header can say *why*

enum BackendEvent {
    Sessions,                  // the session set/state changed — re-read the list
    Conn(ConnState),           // a connection transition (including Failed)
}

struct SessionKey(String);     // opaque; the backend chooses the encoding
struct HostInfo { home: String }  // + room for arch / os / server version
struct Capabilities { attach: bool, shell: bool, fork: bool, restart: bool }
```

Contract rules that keep the seam honest:

- **A backend owns session lifecycle + objective facts on one host.** Nothing
  visual crosses the seam: no window ids in, no colors out.
- **Push, not poll.** `subscribe()` is the single change-notification
  mechanism: the local backend feeds it from its sessions-dir watcher, a
  remote one from its connection task (mirror pushes and connection
  transitions). The run loop selects over every backend's stream and debounces
  into one reload path — a backend whose changes can't wake the UI is
  unrepresentable, and connection health drives the header reactively instead
  of being sampled.
- **Session identity is opaque.** A `SessionKey` is minted by the backend
  (today it encodes the launcher pid) and is the only session identifier that
  crosses the seam — kill and attach take it, the wire carries it. Which OS
  pid to signal (launcher vs. agent) is the backend's internal business, so
  the client can never signal the wrong process, or the right process on the
  wrong host.
- **Capabilities gate affordances; `Result`s carry refusals.**
  `capabilities()` tells the UI what to offer for rows on this host (attach,
  shell, fork, restart — e.g. a `Local` row needs no attach window, and
  fork/restart stay gated per-host until their remote paths land). The action
  methods return `Result`, so an unsupported or failed operation is an
  explainable error in the status line — never a silent guess, and never a
  fallback onto the local machine.
- **Remote methods never block the UI on a dead host.** Mirror reads
  (`list_sessions`) are always instant; round-trip methods fail fast when the
  host is `Disconnected`/`Failed` instead of hanging through a reconnect
  backoff, and callers wrap potentially-blocking calls in `block_in_place`.
- **The host-filesystem queries exist so pickers are host-aware.** A remote
  launch's cwd must be validated, completed, and recalled against *that
  machine's* filesystem. Every path that crosses the seam is absolute. The
  host `$HOME` is *static host metadata*, not a per-query payload: it never
  changes for the life of a connection, so `host_info()` serves it (a remote
  learns it once at the handshake) and the picker uses it purely for
  presentation — collapsing absolute paths to `~/…` for display, expanding
  the user's typed `~` before querying — instead of guessing a remote home
  from local env.
- **Names are data, not an operation.** Session titles arrive on
  `LauncherState.name` at the source (Claude's launcher folds its rename;
  the daemon overlays Codex's sqlite title before pushing), so remote rows
  are titled with no extra RPC. The Claude pid→name manifest shard
  (`SessionIndex`) is a *dashboard-local* naming fallback for local rows and
  the resume list — it is not part of the seam. (This also makes the whole
  surface `&self`: the per-host caches behind it use interior mutability.)

Today's implementation matches this contract with three mechanical gaps —
a polled `take_dirty()` flag instead of `subscribe()`, raw pids instead of
`SessionKey`, and `Option` returns instead of capabilities + plans. The
migration is roadmap item 2 (§8).

### 3.1 `OpenSpec` → `LaunchPlan`: plans, not booleans

"Open a session" is always two steps: **(a)** make the launcher exist,
**(b)** attach a local window to it. Only the backend knows how (a) works on
its host, and only the client can do (b) — so the backend *describes* the
window and the client executes it, uniformly:

```rust
pub struct OpenSpec {                    // what to open (rides the wire)
    pub agent: AgentControl,             // Claude | Codex
    pub cwd: String,
    pub resume: Option<(String, bool)>,  // (session_id, fork) to resume/fork
}

pub enum LaunchPlan {                    // how the client attaches a window
    SpawnLocal  { argv: Vec<String> },   // the window IS the launcher
    AttachRemote { argv: Vec<String>, session_name: String },
                                         // launcher already runs in the pool;
                                         // argv = ssh -t <host> … attach <name>
}
```

- **Local**: `open_session` is pure metadata — it returns the launcher argv
  (`captain-miao claude <cwd> …`); nothing runs until the client spawns the
  window. The window and the launcher share a lifetime.
- **Remote**: `open_session` RPCs the daemon, which starts the launcher inside
  the pty pool *now* (detached, no window), and returns the attach argv. The
  session's lifetime is decoupled from any window — that's what makes it
  persistent.

The client's open path is one line either way: `terminal.spawn(plan.argv())`,
then bind the window (§3.2). This pattern — the backend returns a *plan*, the
client executes it — is the house style for any operation whose mechanics
differ by host: a boolean or a bare `Option` invites the client to guess the
other half, a plan makes the right action explicit. The same shape covers the
other host-varying actions:

```rust
enum ShellPlan {
    InProcess,                     // this machine: open the shell tab yourself
    Spawn { argv: Vec<String> },   // ssh -t <target> 'cd <cwd> && exec $SHELL -l'
}
struct AttachPlan {                // window argv + the token to bind it under
    argv: Vec<String>,
    token: SessionKey,
}
```

`shell_plan` says *which* of the two the client should do — `InProcess` for a
host whose filesystem is this machine's (`Local`, and a `LocalSocket` daemon,
§5.1), `Spawn` for an ssh host — instead of an ambiguous `None` the client
might misread as "open one locally" (the misreading that once put a local
shell in a remote cwd). `attach_plan` pairs the window argv with the binding
token so a client cannot attach a window without also knowing how to record
the binding.

### 3.2 The binding token: one session↔window mechanism

The dashboard owns every session↔window binding. The problem it solves: at
spawn time the launcher's pid (the session's identity key) doesn't exist yet,
so the binding needs a correlation token minted *before* the process:

- **Local**: the dashboard mints a `--launch-id <uuid>` onto the spawn argv.
- **Remote**: the pool session name (`--pool-session cm-…`) *is* the token —
  the daemon mints it and `Opened`/`LaunchPlan` carry it back.

Both flow identically: the launcher echoes the token onto its state file
(`LauncherState.launch_id` / `.pool_session`), the dashboard records
`(host, token) → window_id` in `WindowBindings`, and every window consumer
(focus, preview, move-to-tab, kill, restart) resolves through one choke point,
`App::window_id_for_session`. Bindings persist to `window-bindings.json` so a
restarted dashboard re-resolves live sessions, and the external
`captain-miao focus` bell keybind reads the same file. A hand-launched session
(`captain-miao claude` typed in a terminal — no token) is the one exception:
its launcher self-reports `$KITTY_WINDOW_ID` and the resolver falls back to
that field. Token-bearing launchers never touch the terminal, which is what
lets them run headless in a pool.

## 4. Wire protocol

Length-prefixed JSON frames (4-byte big-endian length + serde JSON, 8 MiB
inbound cap) over a Unix socket, usually ssh-forwarded. JSON keeps frames
debuggable and reuses the existing serde derives; payloads are small (state is
snippet-capped) so encoding overhead is irrelevant. One connection per host
carries everything, two interleaved conversations at once:

- **A subscription stream** (server→client push): `Snapshot` once, then
  per-session `Delta`/`Removed` as state files change.
- **Request/response** multiplexed by `req_id` (client-assigned, monotonic).

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

`PROTOCOL_VERSION` (currently 3) bumps on any incompatible frame change; the
server always answers `Hello` with `Welcome` (so an old client can report the
mismatch), then closes if the versions disagree.

Deltas are **per-session, full-state**: a `Delta` carries the whole (small)
`LauncherState`, and each connection diffs against what *it* last sent — so a
late subscriber is correct from its own `Snapshot` onward, and the server
keeps no cross-connection state. Field-level deltas are an optimization the
capped state size doesn't warrant.

The wire should carry the same opaque session key as the seam. Today it leaks
the key's encoding — `Removed` names the launcher pid, `KillSession` the agent
pid — and collapsing both onto `SessionKey` is part of the §8 seam migration
(one protocol bump). The same bump extends `Welcome` with the static host
metadata behind `host_info()` (the host `$HOME`), so it's learned once at the
handshake instead of riding every `RecentDirs` reply.

## 5. Flows

### 5.1 Remote host provisioning & daemon startup

Adding a host (`Space h`) or starting the dashboard connects each configured
host. The connection task owns the whole sequence and re-runs it on every
reconnect:

```
dashboard: RemoteBackend::connect(Transport::Ssh{target, local_sock}, host)
  │  spawns the connection task ──┐
  ▼                               ▼
┌──────────────────── connection task (loop) ────────────────────────────────┐
│ 1. PROBE      ssh <target> 'echo $HOME; uname -sm; <version checks>'       │
│               → decide the remote exe:                                     │
│                 PATH has matching version → use `captain-miao-server`      │
│                 cache has matching version → use ~/.cache/…/bin copy       │
│                 else → fall back to PATH (must be preinstalled)            │
│               (read-only: the probe never writes to the remote)            │
│ 2. ENSURE     ssh <target> <exe> daemon ensure                             │
│               → prints the daemon's socket path; idempotent               │
│ 3. FORWARD    ssh -O cancel -L … (drop any stale ControlMaster forward)    │
│               ssh -N -L <local_sock>:<remote_sock> <target>                │
│               (forward-ONLY child, kill_on_drop; BatchMode — key/agent     │
│                auth only; ControlMaster shared; sockets in a flat cm-<uid> │
│                dir to stay under the ~104-byte sockaddr_un limit)          │
│ 4. CONNECT    dial local_sock (retry — the far end binds a beat later)     │
│ 5. HANDSHAKE  Hello ⇄ Welcome (protocol check) → Subscribe → Snapshot      │
│ 6. SERVE      §5.3 until the connection drops or the backend is dropped    │
│                                                                            │
│ on any loss: kill the tunnel child, clear the mirror (no stale rows),      │
│ mark Disconnected, back off 500ms → 30s (reset only after a ≥20s healthy   │
│ connection, so a flapping host can't trigger a reconnect storm), retry.    │
└────────────────────────────────────────────────────────────────────────────┘
```

The daemon end of step 2, on the remote host:

```
captain-miao-server daemon ensure
  ├─ print the control-socket path      (stdout still reaches the ssh channel)
  ├─ flock(server.pid)  ── already held? → exit 0 (a daemon is up; idempotent)
  ├─ daemonize: double-fork + setsid, chdir /, stdio → logs/daemon.log
  │             (detached from the ssh session — survives its disconnect)
  ├─ start the libshpool pty pool on a dedicated thread; wait for its socket
  ├─ bind the control socket (dir 0700, socket 0600 — single-user)
  ├─ notify-watch sessions/ (+ the Codex title-store WAL) → broadcast channel
  └─ accept loop:  connections × { handshake, subscribe, requests }
       exits on SIGTERM (`daemon stop` — kills the pool and all its sessions,
       guarded by --force when sessions are live), or after 5 idle minutes
       (no pool sessions and no connected clients — an idle host doesn't keep
       a daemon around; the next connect re-runs `daemon ensure`).
```

The daemon and the tunnel are deliberately decoupled: a dashboard disconnect
or reconnect only ever kills the `-N -L` child. The daemon — and every pooled
session in it — persists until stopped or idle-reaped.

Details that make this robust:

- **One multiplexed ssh connection.** Every backend ssh/scp invocation (the
  probe, `daemon ensure`, the forward cancel, the tunnel itself) shares
  `ControlMaster=auto` + one per-host `ControlPath` (`ControlPersist=120`), so
  steps 1–3 ride a single authenticated TCP connection instead of re-dialing
  and re-authenticating per step. Attach windows are ordinary user-visible
  `ssh -t` processes and dial their own. (Control sockets live in a flat
  `cm-<uid>` dir to stay under the ~104-byte `sockaddr_un` path limit.)
- **A dead daemon can't wedge the gate.** The singleton is the `flock`, not
  the pid file: the kernel releases the lock when the holding process dies —
  cleanly or not — so the next `daemon ensure` acquires it and starts a fresh
  daemon (any stale socket file is removed before rebinding). The pid file's
  content is diagnostic (`daemon status`), never the gate.
- **No conflict with a user's own shpool.** The pool is libshpool *embedded
  as a library*: captain-miao runs its own pool daemon on its own private
  socket in its per-user runtime dir, with a config file it authors. A
  standalone `shpool` install on the same host keeps its own socket, daemon,
  and sessions — the two share nothing; `cm-…` session names exist only
  inside captain-miao's pool.
- **Transports.** `Transport::Ssh` is the flow above. The only other
  transport is `Transport::LocalSocket`: dial a daemon socket **on this same
  machine**, skipping steps 1–4 — the pooled-localhost mode (§8). Local-only
  is part of the contract, and it buys real simplification: attach is always
  available (a bare `captain-miao-server attach <name>`, no ssh),
  `shell_plan` is `InProcess` (the host's filesystem *is* this machine's),
  and none of the provisioning, tunnel, or reconnect-backoff machinery
  applies. A socket that merely forwards to some other machine's daemon is
  deliberately out of scope — reaching a remote host is what
  `Transport::Ssh` is for.

### 5.2 Session lifecycle

One path covers spawn and discovery on every host: **a launcher writes its
state file; whoever watches that host's `sessions/` dir picks it up.** The
dashboard never learns about a session from the spawn call — only from the
state file arriving — so a session opened by another dashboard, or adopted
after a restart, flows through the identical path.

```
OPEN (o/O picker — Ctrl-h selects the host; r resume; b browser)
  dashboard: OpenSpec{agent, cwd, resume?}
  │
  ├─ local row ────► LaunchPlan::SpawnLocal{argv + --launch-id <token>}
  │                  Terminal::spawn(argv) → window; bind (local, token)
  │                  window runs: captain-miao claude <cwd> …   ← window IS launcher
  │
  └─ remote row ───► ClientFrame::OpenSession{spec}
                     daemon: open_in_pool
                       name = "cm-<agent>-<pid>-<seq>"
                       shpool attach --background --dir <cwd> \
                         --cmd 'sh -lc … launcher argv … --pool-session <name>'
                       (login shell: the pool strips the env, so PATH/TERM are
                        rebuilt the way a real login would; attach stderr+log
                        captured so a failed create surfaces a reason)
                     ← Opened{session_name}
                     LaunchPlan::AttachRemote{argv: ssh -t <target> <exe> attach <name>}
                     Terminal::spawn(argv) → window; bind (host, name)

RUN (identical on every host — the launcher triple doesn't know it's remote)
  agent ── hook events ──► launcher socket ──► state file write
  agent transcript/session files ── notify ──► launcher folds → state file write
  state file ── notify ──► host's watcher:
      local host:  dashboard reload reads the file directly
      remote host: daemon diffs vs last-sent → Delta push → mirror → dirty
  either way: the row's status/tokens/title update on the next reload tick

ATTACH (Enter on a running remote row without a window)
  mirror row carries pool_session ──► attach_argv(name) ──► spawn ssh-attach
  window ──► bind (host, name). Enter on an already-bound row just focuses.

DETACH (D, or closing the attach window)
  close the local window; drop the binding; send nothing to the host.
  The pooled session keeps running; the row stays (window-less); Enter
  re-attaches. Reload's prune notices externally-closed windows the same way.

KILL (x)
  KillSession{child_pid} ──► daemon SIGTERMs the agent ──► launcher tears down
  and removes its state file ──► watcher ──► Removed push ──► row disappears.

END OF LIFE
  agent exits (or is killed) → launcher exits → state file removed → Removed.
  Later, the session appears in that host's resumable list (ListResumable
  walks transcripts server-side); resuming it is OPEN with resume: Some(…).
```

### 5.3 Steady-state client/server communication

Inside `RemoteBackend`, a background **connection task** owns the socket; the
dashboard thread never does I/O for reads:

```
 dashboard (sync)              connection task                    daemon
 ────────────────              ───────────────                    ──────
 list_sessions() ──► read the in-memory mirror        (no round-trip, no await)

 kill/resumable/                                       ┌ handle_conn:
 open/host-fs …  ──► queue PendingRequest ── frame ───►│ dispatch on the
     block on oneshot ◄────────── reply{req_id} ───────│ LocalBackend, reply
                                                       │
 run loop:                     mirror ◄── Snapshot/Delta/Removed push ── watcher
   BackendEvent ──► debounced  │  + emit BackendEvent::Sessions            diffs
   reload + redraw             │
                               └ conn transitions → BackendEvent::Conn
                                 (⟳ / ⚠ / the failure reason in the header)
```

- **Reads are free.** The mirror is the host's session list as of the last
  push; `list_sessions` never blocks. `BackendEvent` is the seam's uniform
  "this host changed" signal — the run loop selects over every backend's
  stream (the local backend's is fed by its filesystem watcher) and debounces
  into one reload path. (Today the events are coalesced into a per-backend
  dirty flag the loop polls; the stream is the §8 migration.)
- **Requests fail fast when down.** A request against a `Disconnected` host
  returns `None` immediately (surfaced as an error in the status line) rather
  than queueing behind a reconnect backoff; requests racing a disconnect are
  failed, not stranded. The initial `Connecting` window still queues, so the
  first request after `connect()` rides the pending connection.
- **Disconnect clears the mirror.** A down host shows no stale rows; the
  header's `⟳ <host>` / `⚠ <host>` is its only surface until the reconnect
  snapshot refills the mirror.

## 6. State management: what lives where, who writes it

Three layers, strictly ordered by authority.

**Truth — the launcher's state file** (`sessions/{pid}.json` on the session's
host). One file per session, exactly one writer (its launcher), written
atomically (temp + rename) so a reader never sees a torn state. Everything a
row shows — status, cwd, context tokens, model, title, first prompt, the
binding token — is folded onto this file by the launcher (from hook events,
the transcript, and the agent's own session files). Killing the daemon, the
dashboard, or the tunnel loses no session state; it lives with the session.

**Server state — in-memory, none of it durable.** The daemon is deliberately
state-light; everything it holds is rebuildable:

| state | where | lifetime |
|---|---|---|
| per-connection `last_sent` map (delta diffing) | memory | dies with the connection; the next subscriber gets a fresh `Snapshot` |
| `LocalBackend` caches (session-index mtimes; Codex title cache + read-throttle stamp) | memory | rebuilt on demand from the state files / sqlite |
| pool session state (ptys, processes) | libshpool, in-process | lives as long as the daemon — the reason `daemon stop` is guarded |
| `server.pid`, control socket, logs | disk | recreated on every start; never read as state |
| the host's `recent-cwds.json` | disk | the one thing the server persists (appended when a pool session opens) |

**Dashboard state — a derived view plus client preferences.** In memory: the
per-remote mirrors, the merged host-stamped row list, selection, and the
live `WindowBindings`. On disk, JSON files in the local state dir, each with
a single writer (the dashboard) and each either a *projection* (rebuildable
from truth + the terminal snapshot) or a *preference* (not):

| file | kind | contents |
|---|---|---|
| `hosts.json` | preference | host labels, ssh targets / socket paths, colors |
| `dashboard-overrides.json` | preference | pins/mutes/follow-ups, keep-awake, default agent |
| `directory-marks.json` | preference | per-cwd icon + color |
| `recent-cwds.json` | preference | the *local* host's recent workdirs (each host keeps its own) |
| `window-bindings.json` | projection | token → window; re-seeded at startup, pruned against the live terminal snapshot; also read by the external `focus` bell |
| `dashboard-sessions.json` | projection | crash-recovery snapshot of restartable local sessions |

Identity is `(host, session key)` everywhere in the client: `HostId` is
stamped onto each session at reload (never persisted in the state file — a
host doesn't know what the client calls it) and qualifies every per-row key so
a remote pid can't collide with a local one.

Session titles need no dedicated RPC: both agents' names land on
`LauncherState.name` at the source (Claude's launcher folds the session-file
rename; the daemon's server-core overlays Codex's sqlite title before
`Snapshot`/`Delta`), so remote rows get titles by riding the normal stream.

## 7. UX surface

The remote UX reuses the local keys — the row's host decides what they mean.
`o` opens another session on the row's host and cwd; the workdir picker's
`Ctrl-h` cycles hosts and re-seeds recent dirs / completion / validation
against the selected host's filesystem; `Enter` attaches (or focuses the
existing attach window); `D` detaches; `x` kills; `w`/`W` open a shell tab on
the row's host in the session's cwd; `r` and `b` list resumable/running
sessions across all hosts, host-tagged. A **Host** column and per-host colors
appear only when remotes are configured; per-host connection health shows in
the header. Full key list: AGENTS.md.

## 8. Roadmap

In priority order:

1. **Host-verify end-to-end on Linux.** Everything past this point is best
   tuned after observing real ssh + pool behavior: open/resume/attach/detach
   from a macOS dashboard against a Linux pool host, daemon persistence across
   disconnect and laptop sleep, reattach-after-wake.
2. **Adopt the target seam (§3) and harden the locality boundary.** The
   mechanical gaps first: replace the polled dirty flag with `subscribe()`
   event streams (the run loop selects over every backend's stream, so local
   fs events and remote pushes become the same wake); add
   `ConnState::Failed { reason }` so the header can say *why* a host is down;
   collapse raw pids into the opaque `SessionKey` on the seam and the wire,
   and extend `Welcome` with the `host_info()` metadata (one protocol bump
   covers both); replace the `Option` capabilities with `capabilities()` +
   `Result`-returning plan methods (`ShellPlan`/`AttachPlan`); rename
   `Transport::Socket` to `Transport::LocalSocket` and make its local-only
   contract explicit; move `session_index` off the seam (it is a
   dashboard-local naming fallback), making the whole surface `&self`. Then
   the app layer: make `backend_for(host)` return `Option` (an unknown host
   surfaces an error, never falls back to localhost); collapse the
   launch-id/pool-session choice into one `binding_token()` accessor; and
   quarantine `is_local()` behind a clippy `disallowed-methods` lint with
   `#[allow]` + justification at the handful of genuinely-presentation
   sites. After that, "remote is first-class" is a compile-time property,
   not a convention.
3. **Pooled localhost.** Run the daemon on the user's own machine and let the
   dashboard manage it over `Transport::LocalSocket` — the same daemon can
   serve the local dashboard and remote dashboards at once. Local sessions become
   pool-hosted: they survive dashboard restarts and terminal crashes exactly
   like remote ones, attach windows are a bare `captain-miao-server attach
   <name>` (no ssh), and a Linux GUI host can be driven from its own seat or
   from a laptop interchangeably. This is the locality-invisibility principle
   earning its keep: localhost becomes just another host, with the fastest
   transport.
4. **Remote restart and fork.** The seam already supports both (`OpenSpec`
   carries `resume: (session_id, fork)`; the pool can host the resumed
   launcher) — restart is kill + reopen on the row's host, fork is the same
   with `fork = true`. Deleting the local-only gates makes the last two
   second-class operations first-class.
5. **Per-host keep-awake + remote focus/bell.** Keep-awake should inhibit
   sleep on the host whose session is busy (the daemon runs the inhibitor;
   `Space z` keeps governing the local machine). The `focus` bell should
   resolve a remote attach window to `(host, session)` through
   `window-bindings.json` and ring over RPC.
6. **Host-qualified preference persistence.** Persist pins/mutes/follow-ups
   under `(host, key)` keys so remote flags survive a dashboard restart; prune
   remote entries by mirror presence (a host-scoped fact must be answered by
   that host, never by a local process probe).
7. **Attached-only dashboard (maybe).** Show only sessions the client has a
   window for; the browser (`b`) remains the superset view. A presentation
   refinement to decide after living with the federated table.
