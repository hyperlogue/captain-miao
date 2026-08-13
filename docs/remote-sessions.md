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
- **Session semantics: `OpenSession` reserves, the first attach creates.** The
  server mints the pool name and writes a **reservation** —
  `state_dir()/pending-sessions/<name>.json`, holding the libshpool `--cmd`
  (the login-shell wrapper plus the launcher argv) and `--dir`. No pty and no
  launcher exist yet. The window the dashboard opens next runs
  `miao-server attach <name>`, which finds that record, *claims* it, and hands
  its `cmd`/`dir` to libshpool — whose `attach` creates the session when it
  doesn't exist. So the session is born with this terminal already on the far
  end. The command still runs under the **login shell wrapper** (`sh -lc`, plus
  a sane `TERM`/`COLORTERM`) because the pool strips the environment — PATH must
  be rebuilt the way a real login would (`crates/cm-server/src/server_pool.rs`;
  this fixed the original agent-not-found bug). `--dir` gets the **expanded**
  path: it's a chdir, not a shell word, so a host-canonical `~` would be a
  literal directory name (§3).

  **Why, and what it replaced.** Sessions used to be created *detached*
  (`attach --background --cmd`), which put the agent's TUI in front of a pty
  with nobody on the other end. Everything the TUI negotiates by *asking* the
  terminal — the kitty keyboard protocol's `CSI ? u`, truecolor probes,
  cursor-position round trips — went into the void, got no reply, and settled on
  the conservative fallbacks, **permanently**: shpool never re-negotiates on the
  app's behalf when a client later connects (tmux can, only because it
  implements the protocols itself). The reported symptom was Shift+Enter
  arriving as a bare CR — submit instead of newline — for the whole life of
  every pooled session. It also forced the environment to be *guessed* at create
  time, since libshpool applies the attach header's `TERM` and tty size only
  when it spawns the session's command. Creating from the first attach makes all
  of it fall out for free: real `TERM`, real window size, and queries answered by
  the terminal the user is looking at.

  Five properties to preserve:
  * **Claiming is the `remove_file`, not the read.** Only one unlinker wins, so
    two attaches racing a fresh name can't both decide they are the creator; the
    loser falls through to a plain reattach, which is the right handling for a
    session the winner is bringing up.
  * **Consume before creating, not after.** Holding the record for the session's
    lifetime would let a later attach (a steal, a second window) re-enter the
    create path and skip the stale-name guard. The cost — a crash between claim
    and create loses the reservation — is a reopen, which beats a reservation
    redeemable twice.
  * **The stale-name guard is skipped only on a claimed reservation.** That is
    the one case where "no live launcher owns this name" is correct rather than
    the resurrection hazard the guard exists for.
  * **It is host-local state, not wire protocol.** Reserving and attaching both
    happen inside `miao-server` on the pool's own host, so nothing about it
    reaches the dashboard — which is also what makes the change compatible in
    both directions: an old dashboard drives a new server fine (it just runs the
    attach argv, which now creates), and a new dashboard against an old server
    finds no reservation and plainly reattaches the session that server created
    eagerly. No protocol bump.
  * **Reservations are pruned when the daemon starts.** The pool lives *in* that
    process, so records from a previous incarnation are unredeemable anyway
    (names carry the minting daemon's pid). Inert litter; pruning just stops it
    accumulating.

  Two consequences. A window that never reaches its attach (ssh refused, the
  terminal failed to spawn it) now leaves **no session** rather than an agent
  running headless that nobody asked to keep — a window closed *after* the
  create still just detaches, as always. And a create failure surfaces in that
  held window rather than as the dashboard's "Launch failed:" line, since there
  is no server-side create whose stderr we could capture; what the reservation
  step can still refuse locally (a dead pool) it does.
- **The pty is born at the attaching client's size.** libshpool takes the tty
  size from the attach header, and the create *is* an attach now, so the agent's
  first paint is laid out for the real window. (A `--background` create had no
  client tty and fell back to libshpool's default `TtySize { rows: 24, cols: 80 }`
  — libshpool `src/attach.rs:246-250` — so every pooled session booted at 80×24
  and re-laid out on the first SIGWINCH.) After a detach the pty keeps its last
  attached size; resize is purely client-driven. `simple` restore (below) means
  no scrollback is ever replayed at a new width.
- **TERM comes from the attaching terminal, validated against the host's
  terminfo.** libshpool forwards the attach header's `TERM` into the session it
  spawns, and over `ssh -t` that is the dashboard's own terminal — so a kitty
  user's session now genuinely runs as `xterm-kitty` and TERM-sniffing features
  engage. The wrapper guards the one way that bites: `infocmp "$TERM"` must
  succeed, else it downgrades to `xterm-256color`. A host without kitty's
  terminfo entry (most servers — it ships with kitty, not with ncurses-base)
  would otherwise give every app in the session "unknown terminal type", which
  is far worse than under-reporting. An empty or `dumb` TERM is upgraded as
  before; `dumb` needs its own case precisely because ncurses *does* know it.

  This supersedes the old policy of pinning `xterm-256color` unconditionally,
  whose stated reason — "the session is created detached, so there is no
  attaching terminal to copy from" — stopped being true. Its *other* argument
  still stands and is why the `infocmp` guard exists rather than a bare
  passthrough: TERM is fixed for the session's life, so a later attach from a
  different terminal inherits whatever the first one was. That residual is
  accepted (the value is at worst as wrong as the old fixed one, and right in
  the common case where a session is watched from the terminal that opened it).
  Query-negotiated features are unaffected by any of this — the app enables them
  only if the live terminal replies, which is what create-on-first-attach fixed.
- **`COLORTERM=truecolor` is exported by the wrapper**, and has to be. 24-bit
  support is gated on `COLORTERM` by every library that detects it, and the pool
  strips it — so before this a pooled session rendered its whole UI in 256-color
  approximations of the colors a local one gets ("the color is kind of wrong").
  Note that create-on-first-attach does **not** fix this one the way it fixes
  TERM: libshpool forwards a hard-coded four (`TERM`, `DISPLAY`, `LANG`,
  `SSH_AUTH_SOCK`, plus anything in its own `forward_env` config —
  `src/attach.rs`), and `COLORTERM` is not among them, so the attaching
  terminal's value never arrives no matter who is attached. Verified: a client
  attaching with `COLORTERM=8bit` still lands in a session reading `truecolor`.
  Hard-coding is therefore the mechanism, not a shortcut, and it is safe rather
  than a guess: the dashboard refuses to start outside Kitty or zellij, and both
  are 24-bit, so every terminal that can ever attach supports it. Set only when
  empty, so a host publishing its own value through `/etc/environment` (which
  libshpool *does* load into the session) still wins. Pinned, along with the TERM rules above, by
  `the_wrapper_fixes_the_environment_libshpool_hands_it`, which reproduces
  libshpool's own handling — it `shell_words::split`s `--cmd` and execs the argv
  **directly**, no shell involved on its side (`daemon/server.rs`, the
  `header.cmd` branch) — so the test also pins that `join` → `split` round-trips.
  (The `{`/`}` ban is separate and still real: libshpool runs the string through
  its session-name *template* parser before that.)
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
- **The pool has no keybindings** (`keybinding = []`, `pty_pool.rs`). libshpool
  scans every byte on the client→pty path for a chord and detaches on one,
  defaulting to `Ctrl-Space Ctrl-q`. captain-miao never chose that binding, it
  duplicates an escape hatch the dashboard already owns (`D`, `DetachRemote`),
  and it sits on a prefix an agent TUI may want. The empty list is the
  disabling form rather than `action = "noop"`, because the pump snips a
  matched sequence *before* dispatching the action — `noop` would still eat the
  keys — and because a partial match is buffered until the next byte
  disambiguates it, which delays a lone `Ctrl-Space` either way. Note the
  config file is passed with `--config-file`, which makes it the *only* one
  libshpool loads: a user's own `~/.config/shpool/config.toml` never reaches the
  pool, so it can't put the binding back (nor change anything else here).
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
GetVitals{req_id}                      Vitals{req_id, vitals}
```

- `PROTOCOL_VERSION` = 4. Deltas are **per-session, full-state**: each
  connection diffs against what *it* last sent, so a late subscriber is correct
  from its own Snapshot on and the server keeps zero cross-connection state.
- **`Vitals` is pulled, not pushed** (`cm_core::vitals`). Utilisation is
  displayed in exactly one place — the hosts panel — which is open for seconds
  at a time, so a push would spend a frame per host per interval through hours
  in which nobody can see the answer. The dashboard asks **only while the panel
  is open** (`Backend::poll_vitals`, every 15s, self-throttled so the run loop
  can call it every pass), which also means nothing is measured, sent, or woken
  the rest of the time.

  Three consequences worth naming, because pulling moves work rather than
  removing it:
  * **The sampler still needs a cadence.** A CPU percentage is a *difference*
    between two readings of a monotonic counter, so on-demand sampling has to
    say what "now" means: `MAX_CPU_WINDOW` (60s) discards a previous reading too
    old to describe the present, and the daemon then takes a second reading
    200ms later so the *first* poll after opening the panel already carries a
    figure instead of leaving the column blank for a whole interval.
  * **The daemon caches for 10s**, deliberately shorter than the poll interval:
    a lone dashboard therefore gets a genuinely fresh probe every time it asks,
    while several watching one host collapse onto a single probe. The cache is
    daemon-wide — the one deliberate exception to the zero-cross-connection-state
    rule the session diff follows, and a safe one, since a sample is a fact
    about the host rather than about a client.
  * **A poll must be able to give up.** An older daemon *ignores* a frame it
    can't decode (§3 forward tolerance), so the answer to `GetVitals` there is
    silence; `request_within` puts a deadline on the wait, and the serve loop
    prunes the pending entries whose caller has gone. On the client the reply is
    deliberately **not** wired to the mirror's dirty flag: it changes no row, so
    it raises a redraw-only signal the run loop honours while the panel is
    open (§9).
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
  stop stranding deployed daemons. The sharper half of the problem is
  mechanically unchanged: the daemon hosts the pool and pool children die with
  it (session leaders on its pty masters), so restarting it to upgrade still
  ends every pooled session on the host.

  What changed is that this is no longer the *user's* problem — `u` in the
  hosts panel (§9) upgrades a host and resumes what the restart ended, so the
  cost is a reconnect rather than lost work. Three properties are worth keeping
  straight, because they are what make that safe rather than merely convenient:
  * **Verification precedes the stop.** `upgrade_script` is the deploy script
    split at its `mv`, with `daemon stop --force` inserted between the halves
    under one `set -e` — so the host has run the new binary (`self-check`) and
    agreed its version before anything is ended. A payload it refuses costs a
    transfer and nothing else.
  * **Publishing follows the stop**, not merely accompanies it. `mv`-ing onto a
    live daemon's own path leaves its `/proc/<pid>/exe` reading `(deleted)`,
    and the launcher argv it bakes into reservations comes from `current_exe()`
    — so a session opened in that window would carry an unexecutable path.
  * **The host is held off the air across it** (an in-memory suspended set, not
    the persisted `disabled` flag). The reconnect backoff floors at 500ms, and
    a redial landing between the stop and the `mv` would run `daemon ensure`
    against the old binary still at the cache path and resurrect it — having
    killed every session for nothing.

  Splitting the pool into a separately-stable process (the `pty-daemon`
  entrypoint already exists) is still the fix for the mechanism itself, and is
  evaluated with the engine ruling (§10.2) — with per-session zellij/tmux
  servers it disappears. `u` makes the restart survivable; it does not make it
  unnecessary.
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

   The probe carries one rider that isn't about the binary: **does this host
   have a terminfo entry for the dashboard's own `TERM`?** If not, the dashboard
   **asks** — on the same consent channel as the download, so it inherits the
   queueing, the 90s lapse, and the rule that every ambiguous outcome (no UI,
   Esc, quit, a timeout) declines. Deploying a server is what the user asked for
   by adding the host; writing into their `$HOME` is a side effect they didn't,
   and that is the line the prompt draws. A **decline is remembered without a
   deadline in a gate that is never cleared** — unlike the deploy's, which
   forgets once the host works. That asymmetry is the whole point: a host that
   connects perfectly well is exactly the host that would otherwise re-ask on
   every reconnect. A *failure* still uses the ordinary cooldown, since a full
   disk may not be true next week and the user did say yes.

   On a yes, the local entry is piped into the host's `tic` (`infocmp -x` →
   `tic -x -o ~/.terminfo`, the same stream-it-over-the-open-connection shape as
   the deploy, at a thousandth of the size) and the host is asked to resolve the
   name afterwards — tic's exit status says the file compiled, not that ncurses
   will find it.
   It belongs *here*, in provisioning, because it cannot help later: the pool
   wrapper rewrites an unresolvable `TERM` to `xterm-256color`, and libshpool
   fixes a session's environment when it **spawns** the command, so that rewrite
   is permanent for the session's life. Sessions already created keep what they
   were born with — the detail panel's `Terminfo` warning is what names those.
   Idempotent by construction (the next probe answers `yes`), never fatal (a
   host that won't take it still runs sessions, in `xterm-256color`), and asked
   only when there is something to ask: the probe reports `no` only if the host
   has both `infocmp` and `tic` to act on it with, and the name is sent only if
   it passes an `[A-Za-z0-9._+-]` allowlist — it is spliced into a script that
   `login_shell_safe` wraps in single quotes, so a `TERM` carrying a quote would
   be a command-injection seam out of an environment variable.
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
- **Two round trips don't block the UI thread at all**, because their latency
  is the thing the user would feel: the resume list (`start_resume_load`, which
  is why the picker opens empty and fills in) and the kill (`start_kill`). Both
  run on a pool thread through an `Arc<RemoteBackend>` clone — which is why
  `Backend::Remote` holds one, a spawned task being unable to borrow the `App`
  that owns the backend — and deliver their answer back to the run loop over an
  unbounded channel it drains each tick.
- **A kill is optimistic**, and `presume_killed`/`unpresume_killed` are the seam
  for it: `Remote` hides the key from `list_sessions` (a `presumed_dead` set
  beside the mirror, never a write *to* the mirror — see §7), `Local` no-ops,
  since its kill is an in-process signal its own watcher reports within the
  settle. `KillOutcome` is three-valued for the same reason: `AlreadyGone`
  ("this host has no such live session") and `Unreachable` ("this host never
  heard you") used to be the same `false`, and only the second is grounds to
  put the row back.
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

**The attach window reports its own end; the prune is the backstop.** Closing an
attach window is invisible to every change signal the dashboard has: the pooled
session keeps running untouched, so no state file moves and the host pushes no
delta — and neither Kitty nor zellij has a window-closed callback. Detection was
therefore a periodic `snapshot()` of the whole window tree, which is both a poll
and, on zellij, an expensive one (`list-panes`, ~20ms per pane).

So the attach command is **wrapped in a shell that reports its exit**
(`report_on_exit_argv` → `miao attach-exited` → a `DetachReport` sentinel in the
sessions dir → the dashboard's watcher → `App::apply_detach_reports`). The
mechanism is deliberately the same shape as the `focus` bell: a sentinel in a
directory already under `notify`, not a socket or a signal, because the reporter
runs from a dying window's trap — it must not block, must not need the dashboard
to be reachable, and must survive the dashboard being restarted between the write
and the read. Three properties earn it:

- **It covers every way an attach ends, not just a closed window.** The window
  closing SIGHUPs the wrapper (hence its own `HUP` trap, below), while an
  in-session shpool detach or a dropped ssh ends the attach process normally. The
  old snapshot only ever saw the first of the three; the other two left a bound
  row that was no longer attached to anything.
- **It is scoped to the exact question.** The binding asks "does *my* window
  still exist", which is local knowledge — no host round trip can answer it
  better, and the host's own `attached` bit answers a different question (see
  §10.2).
- **It cannot invent a binding**, only retire one. A report for a binding we no
  longer hold (the prune or a `D` got there first) is a no-op.

**The wrapper also decides whether its window survives the attach.** A window
left behind is a corpse: it shows a dead session's last frame while the row reads
detached, and the next `Enter` opens a *second* window beside it. (A dead
ControlMaster does this to every attach window on the host at once.) But an
attach *refused on arrival* — the busy guard, a stale name, ssh auth — holds the
only copy of that error, since the dashboard never sees an attach's stderr; that
window has to stay, and the row gets a status line pointing at it.

**The terminal cannot be asked to do this half of it.** Spawning the window
`hold: true` was the first attempt and it is wrong on Kitty, whose `--hold` is
not a freeze: kitty rewrites the command to `kitten run-shell --shell=<login
shell> … -- <cmd>` and runs that shell once the command exits ("at a shell
prompt. The shell will be run after the launched command exits"). So every ended
attach became a live *local* shell wearing a session's title — a fish prompt
where an agent had been, arriving en masse the moment a laptop woke and every ssh
dropped at once. So the window is spawned `hold: false` and
`ATTACH_REPORT_SCRIPT` holds it itself, with a `read` behind a "press Enter to
close" line: uniform across all three backends, and unmistakably a dead window
rather than a shell.

`attach_window_is_spent` is the rule, pure and tested, over the wrapper's exit
status and how long the attach ran. The wrapper applies it to decide whether to
hold (it is passed `ATTACH_STARTUP_GRACE` rather than duplicating the number);
the dashboard applies it to the report, to decide whether to say "see its window"
and whether to close the window as a backstop — for an attach that ran unwrapped
(no resolvable reporter exe), or a backend that held it anyway:

| status | meaning | outcome |
| --- | --- | --- |
| `0` / absent | clean end, or a reporter that couldn't tell | spent → close |
| `129` / `130` / `143` | the window was torn down under the attach — `129` is the wrapper's own hangup (below), `130`/`143` the other signals it traps | spent → close |
| anything else | ambiguous | spent only past `ATTACH_STARTUP_GRACE` (10s) |

Both halves are load-bearing, and the signal row is spelled out rather than
written `>= 128` for one reason: **ssh exits 255 both for a mid-session drop and
for a failure to connect**, so the status cannot decide alone, and swallowing 255
into the signal case would close the window on every failed connection. Equally,
duration cannot decide alone — it would keep a window for every session detached
inside the grace.

**A window the user closed ends its session** — `[remote] on_window_close`,
default `close`, the same host RPC `x` makes. Closing a window by hand reads as
"I'm done with this", and the alternative leaves a pooled session running with
nothing on screen to show for it. `detach` opts out; either way the binding is
retired, since the policy decides the session's fate, never the window's.

Two guards keep that from eating sessions nobody meant to end, and both are the
same distinction the rest of this section turns on — *the attach ended* versus
*the window was taken away*:

- **Only status `129`** is a user close (`closed_by_the_user`) — and the wrapper
  reports it from **the SIGHUP it takes itself**, never from the attach's exit
  status. That indirection is the load-bearing part, because a terminal ends a
  window one of two ways. It may `killpg(SIGHUP)` the foreground group, which
  kills ssh with the signal and yields 129 of its own accord; or it may simply
  close the pty master, which by POSIX signals the *session leader* alone — the
  wrapper. ssh is then never signalled: it finds its tty gone and exits **255**,
  identical to a dropped link. A wrapper passing `$?` through therefore read
  every deliberate close on that second route as a network failure and detached
  instead of closing, which is exactly the bug this rule exists to prevent.
  `trap 'r 129' HUP` makes the two routes agree, and which one a given terminal
  picks stops mattering. ssh's 255 still covers a genuine dropped link and a
  failure to connect; `0` is an in-session detach or a steal; `130`/`143` arrive
  by routes that aren't a window closing. None of them end a session — the
  session is what *survived* the failure, and ending it would turn every flaky
  link and every laptop resume into lost work.
- **Only reports drained while the dashboard is running** (`ReportOrigin::Live`).
  A quitting terminal SIGHUPs every attach window on its way out and takes the
  dashboard — living in that same terminal — with it, so those reports are
  waiting at the next startup, wearing a status identical to a deliberate close.
  Acting on the startup backlog would end every session on the host because you
  quit kitty.
- **And the kill waits `CLOSE_ON_WINDOW_CLOSE_DELAY` (1s)** before going out,
  which closes the sliver the origin gate leaves: during a quit the dashboard is
  briefly *still live*, and its watcher can drain a report the instant before its
  own pty dies. It cannot outlive a second of that — no SIGHUP handler, and an
  `event::poll` that fails on a dead pty — so anything still waiting is dropped
  with it. The trade is deliberate: a dropped close leaves a session running,
  which `x` fixes in a keystroke, where the reverse ends every session on the
  host. Note this is the *only* guard that is a duration; it is here because the
  question ("is this terminal going away?") has no authoritative answer that
  survives the terminal going away.

Within those, closing a *tab* still closes the windows in it, so under `stacked`
one gesture ends every session sharing `miao:sessions`. That is the policy
working, not a bug, and the run loop says how many it closed.

**The duration is the wrapper's, not the binding's.** The report carries
`held_secs`, measured in wall clock (`date`) around the attach, and the dashboard
prefers it to how long the binding lived — which is an `Instant`, i.e.
CLOCK_MONOTONIC, which does not advance while the machine is suspended. Reading
the binding, a laptop that slept through an eight-hour attach and woke to a dead
ssh judged it by the minutes it had been awake for: inside the grace, so filed as
a refusal, so the window stayed. That is the same event as the `--hold` fish
prompt above, and it wanted fixing in both places.

The script takes the exe, host, token, grace and attach argv as **positional
parameters** — nothing is interpolated into it. The attach argv holds ssh options
and a session name, and splicing those into a script is how quoting bugs become
command injection. A `$d` latch keeps the EXIT/HUP pair from reporting twice.
With no resolvable `current_exe` there is nothing to report *with*, so `$e`
arrives empty and the report is skipped — but the wrapper still runs, because the
hold is its job too, and the backstop covers the missing report.

What no trap can cover is the terminal emulator being killed outright. So the
periodic prune stays — floored at `DETACH_PRUNE_MIN_INTERVAL`, now **60s**
because it is no longer the primary path, and still gated on `has_remote()` (a
purely local dashboard never snapshots for this at all).

**Evidence short-circuits the heartbeat.** For the cases the report can't reach,
the prune is also armed by the closest proxies available, each floored at
`EVIDENCE_PRUNE_MIN_INTERVAL` (2s, because focus events flap and every prune is a
`snapshot()`):

- **The dashboard regaining focus** — the move that *follows* closing a session
  window, and the moment a stale binding starts lying.
- **A preview capture that stopped answering** — a window that won't serve
  `get-text`/`dump-screen` is usually a window that no longer exists.
- **A failed focus** (`Enter` on the row), which is stronger: it snapshots *right
  there* rather than arming a later one, then finishes what the keypress asked
  for — see below.

None of these retires a binding by itself. One failed rc call, or one unreadable
window, is not proof the window is gone, and treating it as proof would strand a
live session as "detached"; only a real snapshot may prune (`prune_detached_from_tabs`).

**A failed focus re-decides and attaches, so `Enter` is one press.** Before, the
first press only reported "Focus failed" and the user pressed again once the
prune caught up. Now the arm snapshots inline, prunes, re-finds the row **by
identity** (the prune re-sorts the list — detachment is a sort key — so the
pre-focus index may belong to another session by then) and re-runs
`focus_or_attach`, which for a now-window-less pooled row attaches. A row that
still resolves to a window reports the original error instead: the failure was
transient, and retrying focus on the same id would only fail again.

**Retiring a binding must `mark_dirty()`.** `is_detached_row` feeds the sort, but
the visible order is cached against `mutation_version` — so a prune used to
re-icon the row (computed live at draw time) while leaving it in its old slot
until an unrelated reload bumped the version, and *nothing reloads* when an
attach window closes. Both binding mutations the dashboard makes outside a
reload — `record_window_binding` and `prune_detached_sessions` — invalidate it.

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
- **ATTACH ALL** (`Space A`): the same attach, run over every detached row the
  attached-bit overlay reports free — the manual form of the auto-reattach
  below, and focus-less for the same reason. Held rows are skipped, never
  stolen: a steal takes someone else's terminal away, so it stays a per-session
  decision behind its own confirm.
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

  **The row leaves at the keystroke, not at the `Removed`.** That whole chain is
  an ssh round trip plus a process teardown, and it used to run on the UI thread
  — so `x` froze the dashboard for its duration with the row it was killing
  still sitting there. `start_kill` now marks the key *presumed dead* before the
  request goes out (hidden from `Remote::list_sessions`) and makes the call from
  a pool thread. The same applies to the window-close policy below, which is the
  same RPC on a timer.

  Three things unwind a presumption, and the third is what makes it safe:
  the host's own `Removed` (it happened — drop the guess, and with it any chance
  of a recycled launcher pid inheriting the hide); a reconnect's `Snapshot` or a
  dropped link (a full account of the host supersedes every guess about it); and
  a **10s lapse**, after which the row simply comes back. The lapse is load-
  bearing because the presumption is *not* a write to the mirror: the server
  pushes only what changed, so a session that survived a kill it never heard
  about is one the host has no reason to re-send, and an edit to the mirror
  would leave nothing to correct against. An `Unreachable` reply — no answer at
  all, so nothing was signalled — withdraws it immediately rather than waiting
  the lapse out.
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
   `window-bindings.json`, `dashboard-overrides.json` (pins/bells for
   *direct-local* rows, plus keep-awake / default agent / default host /
   layout), `dashboard-sessions.json` (crash-recovery snapshot — direct-local
   by design, since a pooled session survives a dashboard crash on its own and
   "recovering" it would mean resuming a session that never stopped).

Identity is `(host, launcher_pid)` everywhere in the client; `HostId` is
stamped at reload (`#[serde(skip)]` — a host doesn't know what the client
calls it) so a remote pid can't collide with a local one.

**Multi-dashboard semantics**: several dashboards on one host are supported by
construction — each is just another subscriber, and now they agree on
pins/bells too (the flags sidecar, pushed as a `Delta` to every subscriber).
All shared mutable state lives in host-fs files with **last-writer-wins**
semantics, accepted as-is. Steal-attach is an action, not state. Nothing
coordinates concurrent writers beyond atomic file replacement, by decision.

## 9. The TUI surface — everything that operates on hosts

Deliberate principle: **the remote UX reuses the local keys; the row's host
decides what they mean**.

- **`Space h` — the hosts panel**: a list view, not a staged edit form. Each
  host shows live connection state (including the `Failed` reason verbatim),
  running/attached session counts, the daemon version from `Welcome`, its
  **CPU/memory utilisation**, and a latency sample; its ssh/socket target sits
  on a dim second line.
  - **Utilisation is the host's own measurement**, asked for over the protocol
    while the panel is open (§3) rather than measured from here: only the host
    can answer for a socket transport, and an `ssh host uptime` per poll would
    be a process per poll per host reporting the *link's* view anyway. It sits
    beside the latency because the two together are the launch decision —
    reachable, and with room; and because the poll refreshes the latency sample
    too, the whole line goes live while it is being read. Rendered as two
    percentages, no absolutes and no colour threshold: the row is a scannable
    line, and a pegged CPU on a build box is normal rather than an alert. A host
    that reports nothing (an older daemon, an OS we can't read, a link that just
    dropped) shows nothing rather than zeros, which on a utilisation display
    would read as a definitely-idle host. Editing a
  row exposes label / target (`^t` toggles ssh↔socket) / **options** (below) /
  **icon** (`^e` opens the same searchable emoji picker as `Space i`). There is
  **no Save
  step**: adding a host persists and connects immediately — so its state
  animates live in the list — an edit applies when you commit the row, and `d`
  removes behind a `y/N` confirm. A `Failed` reason is **flattened and
  truncated** to its row (it quotes host output, so it carries newlines that
  would corrupt the row and a length no row can hold), with the whole text one
  key away.
  - **`u` — upgrade this host's server** (§3 for the mechanism). Offered only
    on a row that has somewhere to go: the connection task re-asks
    `decide_provision` with the running daemon out of the picture, and only an
    `Upload` becomes an offer, so a host on a user's own PATH install (never
    overwritten) or already on our exact digest advertises nothing. The row
    wears `↑<version>` where an offer exists and the footer hint appears with
    it — a key that would silently do nothing is worse than no key, and every
    other key here works on every row. A stale-but-unfixable host keeps the
    plain `(older than ours)` annotation instead.
  - **Two refusals, and they are one rule seen twice.** The upgrade ends every
    session on the host and brings each one back as a window *here*, so it
    declines a host with any **non-idle** session and any session **another
    client is attached to** — the first would lose work, the second would take
    a session from whoever is using it rather than hand it back. "Idle" is the
    restart-all whitelist (`Idle | Compacted`), deliberately not
    `SessionStatus::is_busy`: `Starting`, `WaitingForApproval` and
    `ReviewPending` all read as at-rest by that narrower test and are exactly
    what you would hate to have restarted. An *unreadable* attached bit is not
    evidence of a second client, matching the detached glyph's rule.
  - **The refusal renders in the panel**, on the same line the confirm uses.
    This mode's footer is key hints, so there is no status line to put it on,
    and a message set elsewhere would surface stale after the panel closed.
  - **A failure drops the restore list.** `set -e` puts everything destructive
    downstream of the host's own verdict, so the common failures stopped
    nothing — and resuming a session that never died would fork it into a live
    duplicate. The residual case (stop succeeded, `mv` did not) loses the
    automatic restore, says so, and leaves those sessions in the resume picker,
    since a killed pool session's transcript on the host is untouched.
- **`Options` — per-host ssh arguments** (`hosts::split_options`,
  `backend::split_connection_options`). Passed through verbatim to every ssh
  captain-miao runs for that host, with no grammar of our own on top.

  The feature has exactly two coherent shapes — a raw argument string, or a
  structured editor where a forward is a row with a type and two endpoints — and
  anything in between is a bespoke syntax to learn *and* a ceiling to hit. This
  is the raw one. An earlier pass built the middle (a `Ports` field with `3000` /
  `8080:3000` shorthands, canonicalisation, per-spec validation, plus a second
  field for everything else) and it was more machinery than the problem.
  - **What it is really for is forwards.** Host identity — port, `ProxyJump`,
    `IdentityFile` — belongs in a `~/.ssh/config` `Host` block, which covers the
    attach windows and the `w` shell too, since captain-miao reaches a host by
    plain `ssh <target>`. What ssh_config *can't* express is anything scoped to
    our connection alone, and a forward is not a property of the machine at all:
    it is something you want up while working on that host and gone when you
    aren't. That lifecycle is what the field adds over a hand-run `ssh -L`.
  - **A forward can't ride `ssh_common_opts` with the rest**, which is the one
    piece of real machinery left. An option is a property of the connection and
    repeating it is free; a forward is a *resource the connection holds*, and
    repeating it collides three ways: `daemon ensure` re-requests what the probe
    already registered; the transport's own `ssh <opts> -O cancel -L <sock>
    target` would name it too, and `-O cancel` cancels every forward on its
    command line, so we would tear it down once per reconnect; and every attach
    window would ask for it again. So `split_connection_options` lifts
    `-L`/`-R`/`-D` (glued or separated, normalised apart) onto the `ssh -N -L`
    tunnel child, and nothing else carries them. `ExitOnForwardFailure` stays at
    its default `no`: a port already in use must cost that one forward, not the
    link to the host.
  - **The user's arguments go first**, because ssh keeps the first value it
    obtains for an option. Ours first would make the field inert for exactly its
    motivating settings — `ConnectTimeout`, `ServerAliveInterval` and
    `ControlPersist` are all set by `ssh_common_opts`. The price is that
    `ControlPath`, `ControlMaster` and `BatchMode` are overridable and each
    breaks something real: the first two split the multiplexing this depends on
    (including the `-O cancel` that retires forwards), the third lets ssh prompt
    on a child whose stdin is `/dev/null`.
  - **`-O cancel` before every request**, and on every host that leaves the ssh
    set. A forward requested by a multiplexed client is registered with the
    *master*, so one deleted from the field would hold its port for as long as
    the master lives, and one still in the field would make the re-request fail.
    Nothing enumerates a master's forwards, so `REQUESTED_FORWARDS` remembers
    what this process asked each `(label, target)` for and cancels that set ∪ the
    new one before requesting; `retire_unlisted_forwards` covers the host that
    was deleted, suspended, renamed or switched to a socket, whose forwards would
    otherwise outlive a row the panel calls disconnected.
  - **Nothing is validated**, which is what verbatim means. A bad argument is
    still diagnosable rather than a silent flap: it reaches `daemon ensure`
    before the tunnel child, and that call captures stderr into the `Failed`
    reason the panel shows. The one exception is a trailing `-L` with no
    argument, dropped because it is a usage error on *every* call that would
    carry it — including the attach window.
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
- **Host icon**: a compact **emoji**, shown only when more than one host exists
  or a local row lives in another terminal instance. Per-host icons are
  configured in the panel exactly like the workdir marks, with a deterministic
  FNV-derived fallback so a host always has one. An icon rather than a name
  because it is a glance-level "which box is this?", and a name either truncates
  to noise or eats six cells. It shares the **workdir-icon column** rather than
  holding one of its own — `<host>│<workdir>`, divider and all — because both
  answer "where is this?" and read better as one glyph pair than as two columns
  a table apart; the freed width goes to the elastic last-prompt column. A local
  row that lives in another terminal instance takes `⧉` in the host half (the
  row is already dimmed, and the detail panel names the instance in full).
  A host carries no configurable **colour**: it said the same thing the emoji
  says, less well, so the field is gone from the panel and ignored in
  `hosts.json`.
- **`Enter`** — the focus-or-attach decision, in order: foreign-terminal local
  row → error; a row with a resolvable window → focus; a pooled row without one
  → spawn the attach window and focus it at once — an explicit `focus_window`
  after the spawn, since an attach spawn is `take_focus: false` on both backends
  (it must not yank the client mid-creation) and would otherwise leave the
  session running unseen in the background; auto-reattach passes `focus: false`,
  because a reconnect can restore five windows at once. A focus that *fails* re-enters
  the same decision after pruning the dead binding, so a stale binding costs no
  second press (§5). Because the attach runs inline in the run loop — planning
  the argv, spawning a window, waiting on the terminal backend — it paints an
  **"Attaching…" overlay** in the pre-action frame; otherwise the only feedback
  for the whole round trip is a frozen dashboard, which reads as a dead key and
  invites exactly the second press this is removing. There is no fourth case:
  **non-attachable remote rows are filtered out at reload**
  (`is_actionable_row`). Challenged in the first-principles review (an
  attention state on a hidden row goes invisible remotely) and reaffirmed: *the
  dashboard is for actionable sessions* — a row this dashboard can neither
  attach nor act on doesn't earn a slot, the hosts panel's session count keeps
  them countable, and the host's own dashboard remains their surface. (If this
  ever needs softening, the recorded refinement is: hide *unless* the row has
  an attention state.)
- **Detached sessions**: a pooled row with no bound window gets its own icon
  (joining the pinned/follow-up set) and its own sort tier at the very
  **bottom** — it's running somewhere else, so it shouldn't compete for the eye
  with what's in front of you. Detachment is the **first sort key**: no status
  lifts a row out of the tier, not a follow-up bell and not a live approval or
  decision prompt. Those are urgent, but urgent *elsewhere* — nothing there can
  be answered until you attach, so seating it above the sessions on this screen
  buries the work you can actually do now, and `follow_up` is auto-armed on
  every Active→Idle, so a detached session that merely finished a turn would
  homestead the attention block. The single exception is an explicit **pin**:
  `p` is the user naming that row, not the dashboard inferring. (A detached row
  remains a valid `s` jump target either way: `s` is an explicit "take me to
  what wants me", the tier is only about where a row sits at rest.) The preview
  panel names the case rather than showing
  `(loading…)` for a fetch that will never arrive — there is no local window to
  capture. New / resumed / forked sessions auto-attach on create.
  - **Free vs. held by another client** is a split in the *glyph*, not in the
    tier: `🙈` for a pty nobody holds, `👀` for one another terminal is attached
    to (the host's attached-bit overlay, §10.2). The distinction is worth
    drawing because `Enter` differs — a free row attaches, a held one needs the
    steal, which the preview panel spells out by its live binding. It is
    deliberately *not* a second sort tier: a row is out of sight either way, and
    the order has no business flapping on another client's comings and goings.
    An **unknown** bit (the pool couldn't be read) draws as free — an unreadable
    pool must not put every row behind an implied steal, matching how the steal
    confirm treats unknown.
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
- **`Space A` attach-all** — one window per free detached row, no confirm: it
  only opens windows onto sessions already running, `D` puts any of them back,
  and it is what the reconnect sweep does unprompted. Held rows are skipped and
  counted in the status, so a partial batch doesn't read as a whole one. Hidden
  from `?` alongside the steal.
- **`w` work tab** — `shell_plan` decides: an in-process shell for this
  machine, an `ssh -t <host>` tab that cds into the session's cwd for a remote.
- **Fork and restart** work on any host.
- **Preferences**: pins/follow-ups on a pooled host's rows are stored
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
and the daemon overlays the pool's attached bit onto the rows it serves
(`LauncherState.attached`, exactly like the Codex title overlay) so the UI can
tell whether anyone is actually there and skip the confirm when nobody is.

**The bit is pushed by libshpool's hooks, not sampled** (`pty_pool::PoolHooks`,
feeding `ATTACHED`). It has to be. libshpool keeps no attached flag: its `List`
reconstructs one by `try_lock`ing the session's `SessionInner`, the mutex the
attach path holds for exactly as long as a client is attached. So every query is
a *sample*, stale the moment it is read — a detach and a re-attach either side of
the round trip both answer truthfully and disagree — whereas the hooks fire in
the daemon's own causal order. The hooks also carry the **wake**: an attach or a
detach touches nothing under `sessions/`, so before this the notify watch never
fired for one and the bit reached other dashboards only when some unrelated
session happened to write state. Idle rows were the worst case, and idle rows are
what the steal confirm and the reconnect sweep act on. No seeding is needed —
the pool is a thread of the daemon's own process, so a daemon only now starting
hosts no sessions and the hooks have seen every one that can exist.

The one remaining sample is the attach wrapper's busy pre-check, which runs in a
separate short-lived process and so can only ask. It stays racy on purpose: a
wrong "busy" costs a retry, and a wrong "free" falls through to libshpool's own
refusal. What makes that acceptable is that the attach attempt is the only
operation that actually takes the lock — it is a transaction, not an
observation — so its answer is authoritative, and §6 spends it: `ATTACH_EXIT_BUSY`
names the reason in the dashboard and corrects the row it came from.

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

The hosts panel's `u` (§3, §9) changes what that side effect is worth without
changing whether it is worth having. `u` already turns an upgrade into a
reconnect: it verifies on the host before stopping anything, and resumes what
the stop ended. What it cannot do is make the restart free — a resumed session
is a new pid on the same transcript, so scrollback, in-flight work and anything
the agent held in memory are still lost, and the gate has to refuse a host that
is busy or that another client is attached to. A split pool (or per-session
servers) removes the restart from the picture entirely, and `u` would then be
deploy-and-reload with nothing to resume. Read the keystroke as evidence that
the edge is *survivable*, not as a reason the ruling matters less.

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
