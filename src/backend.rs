//! `Backend` is the dashboard's seam to *where sessions run and where their
//! files live*. `Local` is in-process (the dashboard and the agents share one
//! host); `Remote` reaches a `miao-server` over a (possibly
//! ssh-forwarded) socket. Enum-dispatched to match `AgentControl`'s style: no
//! dyn, no registry, just a `match` per operation.
//!
//! A backend owns *session lifecycle + objective facts on one host*: the live
//! session list (with the per-host Codex title overlay already applied), the
//! resumable list, the session-name index, and killing a session. Everything
//! visual or preference-y — selection, Terminal control, pins/mutes, preview
//! capture — stays in the TUI (the *client*), which overlays its own state on
//! what the backend returns.
//!
//! [`LocalBackend`] is also the **server-core**: `miao-server` wraps one
//! to answer a remote dashboard's requests, so the same local-read logic backs
//! both the in-process path and the remote path. See `docs/remote-sessions.md`.
//!
//! Phase 1 routed the reads and the kill through here. Phase 3's first slice adds
//! the spawn seam: [`Backend::open_session`] turns an [`OpenSpec`] into a
//! [`LaunchPlan`] — today always the argv for a local Kitty window (the window
//! *is* the launcher); the remote `AttachRemote` plan lands once the pty pool can
//! host a launcher. See §14.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::agent::{ResumeCandidate, SessionIndex};
use crate::protocol::{
    ClientFrame, PROTOCOL_MIN, PROTOCOL_VERSION, ServerFrame, protocol_compatible, read_frame,
    write_frame,
};
use crate::server_payload::ServerPayload;
use crate::state::{self, HostId, LauncherState, SessionFlags, SessionKey};

// `LocalBackend` (the server-core), `OpenSpec`, and `LaunchPlan` live in cm-core;
// re-exported so `crate::backend::…` paths across the dashboard resolve unchanged.
pub use cm_core::backend::{LaunchPlan, LocalBackend, OpenSpec};

/// Per-host session management. `Local` is in-process; `Remote` speaks the wire
/// protocol to a `miao-server` over a (possibly ssh-forwarded) socket.
pub(crate) enum Backend {
    Local(LocalHost),
    Remote(RemoteBackend),
}

/// Connection health of a backend, surfaced in the header aggregate and, in
/// full, in the hosts panel. `Local` is always `Connected`; a `Remote`'s
/// background task moves it Connecting → Connected → Disconnected (then back to
/// Connecting as it retries with backoff), or parks on `Failed` when the reason
/// is diagnosable and won't fix itself by retrying.
///
/// `Failed` is what closes the "silent ⚠" gap (§4): a missing or
/// version-mismatched `miao-server` on the remote used to surface as an
/// ordinary disconnect, so the user saw a warning triangle and no way to learn
/// *why*. The reason travels with the state and the panel prints it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnState {
    Connecting,
    Connected,
    Disconnected,
    /// Reachable-but-unusable: the reason is a short human sentence, already
    /// phrased for display.
    Failed(String),
}

impl ConnState {
    /// Whether this host is currently usable for requests.
    pub(crate) fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected)
    }

    /// A short label for the hosts panel / header.
    pub(crate) fn label(&self) -> &str {
        match self {
            ConnState::Connecting => "connecting",
            ConnState::Connected => "connected",
            ConnState::Disconnected => "disconnected",
            ConnState::Failed(reason) => reason,
        }
    }
}

/// The in-process host: a [`LocalBackend`] plus **its own** change watcher.
///
/// Owning the watcher here is the point (§5): the dashboard's run loop used to
/// create a `notify` watch on `sessions/` itself, so "how do I learn a session
/// changed" had two answers — an app-level fs watch for localhost and a mirror
/// push for remotes. Now every backend answers [`Backend::subscribe`] the same
/// way and the app has no filesystem knowledge at all. (It also makes
/// pooled-localhost free: that backend is a `Remote` over a local socket, and
/// it simply has no watcher to own.)
pub(crate) struct LocalHost {
    inner: LocalBackend,
    /// Bumped by the notify callback; the run loop reads it through
    /// [`BackendEvents`]. Held here so the watcher outlives `subscribe`.
    changed: Arc<AtomicBool>,
    watcher: Option<notify::RecommendedWatcher>,
}

/// A backend's change signal, taken (and cleared) by the run loop. One handle
/// per backend, from [`Backend::subscribe`]; a local one is fed by that
/// backend's fs watcher, a remote one by its connection task's mirror pushes
/// and connect/disconnect transitions.
pub(crate) struct BackendEvents {
    changed: Arc<AtomicBool>,
}

impl BackendEvents {
    /// Whether this backend changed since the last call (and clear the signal).
    pub(crate) fn take(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }
}

/// What a host can do, as the host itself reports it — the `capabilities()`
/// seam that replaced `Option`-returning `attach_argv`/`shell_argv` (§5). App
/// code asks "does this host pool its sessions?", never "is this host local?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendCaps {
    /// Sessions live in a pty pool, so a local window *attaches* to one rather
    /// than being it — which is what makes detach (`D`), re-attach, and the
    /// steal meaningful. True for any host reached over the protocol, including
    /// a pooled localhost.
    pub pooled: bool,
    /// A `w` work-tab shell can be opened on this host.
    pub shell: bool,
}

/// How the client opens a shell on a host for the `w` work tab.
pub(crate) enum ShellPlan {
    /// Run the user's own shell locally in `cwd` (the terminal backend does it;
    /// there is no argv).
    InProcess { cwd: String },
    /// Spawn this argv — an `ssh -t <target>` that cds into the host's cwd.
    Spawn { argv: Vec<String> },
}

/// How the client attaches a window to an already-running pooled session.
pub(crate) struct AttachPlan {
    pub argv: Vec<String>,
}

impl Backend {
    pub(crate) fn local() -> Self {
        Backend::Local(LocalHost {
            inner: LocalBackend::new(),
            changed: Arc::new(AtomicBool::new(false)),
            watcher: None,
        })
    }

    /// The host this backend manages — `local` for in-process, the configured
    /// label for a remote. The dashboard stamps it onto each session it reads.
    pub(crate) fn host_id(&self) -> HostId {
        match self {
            Backend::Local(_) => HostId::local(),
            Backend::Remote(b) => b.host.clone(),
        }
    }

    /// Connection health, for the header surface. A local backend is always
    /// connected; a remote reflects its background connection task's state.
    pub(crate) fn conn_state(&self) -> ConnState {
        match self {
            Backend::Local(_) => ConnState::Connected,
            Backend::Remote(b) => b.conn_state(),
        }
    }

    /// What this host supports, so app code branches on the capability rather
    /// than on locality (§1's load-bearing principle).
    pub(crate) fn capabilities(&self) -> BackendCaps {
        match self {
            Backend::Local(_) => BackendCaps {
                pooled: false,
                shell: true,
            },
            Backend::Remote(b) => BackendCaps {
                pooled: true,
                // Reached over ssh → an `ssh -t` shell tab. Reached over a
                // *local* socket (pooled-localhost) → there's no ssh target,
                // but the host is this machine, so the shell is in-process.
                shell: b.attach_target.is_some() || b.transport_is_local,
            },
        }
    }

    /// Start (or fetch) this backend's change signal. Called once per backend
    /// at startup and after a hosts-panel reconnect; a local backend lazily
    /// creates its `sessions/` + agent-path watcher on the first call.
    pub(crate) fn subscribe(&mut self) -> BackendEvents {
        match self {
            Backend::Local(h) => {
                if h.watcher.is_none() {
                    h.watcher = start_local_watcher(h.changed.clone());
                    // Whatever the watcher's fate, the first pass must reload.
                    h.changed.store(true, Ordering::Relaxed);
                }
                BackendEvents {
                    changed: h.changed.clone(),
                }
            }
            Backend::Remote(b) => BackendEvents {
                changed: b.dirty.clone(),
            },
        }
    }

    /// The daemon version this host reported at handshake, for the hosts panel.
    /// `None` for a local backend (it *is* this build) or before a handshake.
    pub(crate) fn daemon_version(&self) -> Option<String> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => b.server_version.lock().unwrap().clone(),
        }
    }

    /// Round-trip time to this host, sampled opportunistically from real
    /// request/response traffic — there is deliberately **no `Ping` frame**
    /// (§9): every reply is already matched by `req_id`, so timing one costs
    /// nothing. `None` for local, or before any request has been answered.
    pub(crate) fn latency(&self) -> Option<Duration> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => *b.latency.lock().unwrap(),
        }
    }

    /// Live sessions on this host (those with a current state file).
    pub(crate) fn list_sessions(&self) -> Vec<LauncherState> {
        match self {
            Backend::Local(h) => h.inner.list_sessions(),
            Backend::Remote(b) => b.list_sessions(),
        }
    }

    /// Merge each agent backend's session-name shard into one index (today only
    /// Claude's manifest scan contributes — Codex titles arrive on
    /// `LauncherState.name` via the per-host overlay).
    pub(crate) fn session_index(&mut self) -> SessionIndex {
        match self {
            Backend::Local(h) => h.inner.session_index(),
            Backend::Remote(b) => b.session_index(),
        }
    }

    /// Resumable sessions across every agent backend, most-recent first, capped
    /// at `limit`. Returns the merged list plus any per-agent errors (the caller
    /// decides how to surface them). The walk reads file tails synchronously
    /// (local) or makes a blocking round-trip (remote), so an async caller
    /// should wrap this in `block_in_place`.
    pub(crate) fn list_resumable(&self, limit: usize) -> (Vec<ResumeCandidate>, Vec<String>) {
        match self {
            Backend::Local(h) => h.inner.list_resumable(limit),
            Backend::Remote(b) => b.list_resumable(limit),
        }
    }

    /// Tear the session down, naming it by its opaque [`SessionKey`]. The
    /// *owning host* resolves the key to a live pid immediately before
    /// signalling, so a mirror lagging the session's exit can't make it SIGTERM
    /// a recycled pid (§3). May block on a round-trip for a remote host, so an
    /// async caller should wrap this in `block_in_place`.
    pub(crate) fn kill_session(&self, key: &SessionKey) -> bool {
        match self {
            Backend::Local(h) => h.inner.kill_session(key),
            Backend::Remote(b) => b.kill_session(key),
        }
    }

    /// Record the host-owned flags for a session, so every dashboard watching
    /// that host agrees (§9). `false` when the host doesn't serve flags — a
    /// plain local backend, whose flags are the dashboard's own
    /// `dashboard-overrides.json` — which is the caller's signal to persist
    /// them locally instead. Blocks on a round-trip for a remote host.
    pub(crate) fn set_session_flags(&self, key: &SessionKey, flags: SessionFlags) -> bool {
        match self {
            Backend::Local(_) => false,
            Backend::Remote(b) => b.set_session_flags(key, flags),
        }
    }

    /// Plan how to open a session on this host (a fresh launch or a resume/fork).
    /// Local returns the argv for a Kitty window directly — pure metadata, no
    /// process starts until the client spawns the window. Remote RPCs the server
    /// to start the launcher inside its pty pool and returns an `AttachRemote`
    /// plan (an `ssh … attach` window). May block on the round-trip, so an async
    /// caller of the remote path should wrap this in `block_in_place`. (The
    /// client still routes its own spawns to the local backend for now — remote
    /// attach windows arrive with the 3d browser; see `App::local_backend`.)
    pub(crate) fn open_session(&self, spec: &OpenSpec) -> anyhow::Result<LaunchPlan> {
        match self {
            Backend::Local(h) => Ok(h.inner.open_session(spec)),
            Backend::Remote(b) => b.open_session(spec),
        }
    }

    /// How to open a window onto an *already-running* pooled session on this
    /// host. `force` steals it from whatever client currently holds it (the
    /// pool is one client at a time — §10.2).
    ///
    /// A `Result`, not an `Option` (§5): the old signature could only say
    /// "no", so every caller invented its own message for a case it couldn't
    /// distinguish. Now the host explains itself.
    pub(crate) fn attach_plan(
        &self,
        session_name: &str,
        force: bool,
    ) -> anyhow::Result<AttachPlan> {
        match self {
            Backend::Local(_) => anyhow::bail!(
                "sessions on this host aren't pooled — they own their window, so there is \
                 nothing to attach to"
            ),
            Backend::Remote(b) => Ok(AttachPlan {
                argv: attach_argv(
                    b.attach_target.as_deref(),
                    &b.remote_exe.lock().unwrap(),
                    session_name,
                    force,
                ),
            }),
        }
    }

    /// How to open an interactive login shell on this host in `cwd` (the `w`
    /// work tab): in process for this machine, over ssh for a remote.
    pub(crate) fn shell_plan(&self, cwd: &str) -> anyhow::Result<ShellPlan> {
        match self {
            Backend::Local(h) => Ok(ShellPlan::InProcess {
                // The row's cwd is host-canonical; a local chdir needs the real
                // path, and this backend's own home is the one to expand it by.
                cwd: cm_core::paths::expand_home(cwd, h.inner.home()),
            }),
            Backend::Remote(b) => match b.attach_target.as_deref() {
                Some(target) => Ok(ShellPlan::Spawn {
                    argv: remote_shell_argv(target, cwd),
                }),
                // Pooled localhost: the "remote" host is this machine, so the
                // shell is the ordinary local one. `$HOME` never crosses the
                // wire, so the expansion uses *our* home — correct precisely
                // because this transport is local-only by contract.
                None if b.transport_is_local => Ok(ShellPlan::InProcess {
                    cwd: cm_core::paths::expand_home(cwd, &cm_core::paths::host_home()),
                }),
                None => anyhow::bail!(
                    "cannot open a shell on {}: it is reached over a socket with no ssh target",
                    b.host.0
                ),
            },
        }
    }

    /// This host's recent working dirs, host-canonical (§3 — no `$HOME` on the
    /// wire, so what comes back is what the picker displays and submits). The
    /// remote path blocks on a round-trip, so wrap async callers in
    /// `block_in_place`.
    pub(crate) fn recent_dirs(&self) -> Vec<String> {
        match self {
            Backend::Local(h) => h.inner.recent_dirs(),
            Backend::Remote(b) => b.recent_dirs(),
        }
    }

    /// Directory completions for `prefix` on this host's filesystem
    /// (host-canonical, trailing `/`). Remote blocks — wrap in
    /// `block_in_place`.
    pub(crate) fn complete_path(&self, prefix: &str) -> Vec<String> {
        match self {
            Backend::Local(h) => h.inner.complete_path(prefix),
            Backend::Remote(b) => b.complete_path(prefix),
        }
    }

    /// Whether `path` is a directory on this host. Remote blocks — wrap in
    /// `block_in_place`.
    pub(crate) fn dir_exists(&self, path: &str) -> bool {
        match self {
            Backend::Local(h) => h.inner.dir_exists(path),
            Backend::Remote(b) => b.dir_exists(path),
        }
    }
}

/// Watch this host's session state for changes, feeding `changed`. Owned by the
/// local backend (§5), not the app: the `sessions/` dir where launchers write,
/// plus each agent backend's own nominated paths (Claude's session-name store,
/// Codex's title-store WAL — the wake for the throttled title overlay).
///
/// Best-effort throughout: a missing path simply isn't watched, and a watcher
/// that can't be created at all leaves the dashboard on its reload cadence
/// rather than failing to start.
fn start_local_watcher(changed: Arc<AtomicBool>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher as _;
    let sink = changed.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Skip Access (open/close/read): our own reads would otherwise wake us.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        sink.store(true, Ordering::Relaxed);
    })
    .ok()?;
    let dir = state::sessions_dir();
    if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!("could not watch {}: {e}", dir.display());
        return None;
    }
    for &agent in crate::agent::AgentControl::ALL {
        for path in agent.watch_paths() {
            let _ = watcher.watch(&path, notify::RecursiveMode::NonRecursive);
        }
    }
    Some(watcher)
}

// =============================================================================
// Remote backend (RPC to a `miao-server` over a socket)
// =============================================================================

/// How a [`RemoteBackend`] reaches its server.
pub(crate) enum Transport {
    /// Connect straight to a daemon socket **on this same machine** — no ssh
    /// hop. Local-only is part of the contract, not an accident: this is the
    /// pooled-localhost transport (§10.1), where the "remote" host is the
    /// machine the dashboard runs on, so an attach needs no ssh and a `w` shell
    /// is opened in process. (It doubles as the manual-forward / test path.)
    LocalSocket(PathBuf),
    /// Set up an ssh forward to `target`'s daemon and connect via `local_sock`:
    /// ensure the daemon is running + learn its socket path (`daemon ensure`),
    /// then run a forward-only `ssh -N -L <local_sock>:<remote_sock> target`
    /// child (the tunnel, killed when this backend drops; the daemon persists).
    Ssh { target: String, local_sock: PathBuf },
}

/// One in-flight request the connection task must answer by `req_id`.
struct PendingRequest {
    req_id: u64,
    frame: ClientFrame,
    reply: oneshot::Sender<ServerFrame>,
}

/// Backend for a session running on another host, reached over a (possibly
/// ssh-forwarded) unix socket. A background task owns the connection: it keeps
/// an in-memory **mirror** of the host's sessions current (driven by the
/// server's `Snapshot`/`Delta`/`Removed` push), and pumps request/response by
/// `req_id`. The synchronous [`Backend`] methods read the mirror (no round-trip)
/// or block on a oneshot for a reply — so callers should be inside
/// `block_in_place` when they might block (resume list, kill).
pub(crate) struct RemoteBackend {
    /// The host this backend speaks for; stamped onto every session it returns.
    host: HostId,
    /// ssh target for the attach window, learned from the transport: `Some` for
    /// an ssh host (`ssh -t <target> miao-server attach <name>`), `None` for a
    /// direct socket transport (a same-host `miao-server attach <name>`).
    attach_target: Option<String>,
    /// Whether this backend's transport is [`Transport::LocalSocket`], i.e. the
    /// daemon is on *this* machine. Distinguishes pooled-localhost (where a
    /// missing ssh target is correct and a shell is in-process) from a
    /// misconfigured remote.
    transport_is_local: bool,
    /// Latest known sessions on the remote host, keyed by their opaque
    /// [`SessionKey`] — the wire's only session identifier (§3).
    mirror: Arc<Mutex<HashMap<SessionKey, LauncherState>>>,
    /// Requests to the connection task; `None` once the task has exited.
    requests: mpsc::UnboundedSender<PendingRequest>,
    next_req_id: AtomicU64,
    /// The command to invoke the remote daemon, resolved at connect by
    /// `setup_ssh` (PATH `miao-server`, or a deployed cache path —
    /// open-decision #3). Defaults to `miao-server`, so before the task
    /// resolves it (or for a socket transport) the attach argv is unchanged.
    /// Never the dashboard binary (`miao`) — the remote runs the headless server.
    remote_exe: Arc<Mutex<String>>,
    /// Connection health the connection task updates as it dials / connects /
    /// loses the link, read by the header + hosts panel. Carries the `Failed`
    /// reason, so a diagnosable problem (server missing, version mismatch, ssh
    /// refused) is nameable rather than a silent ⚠ (§4).
    conn: Arc<Mutex<ConnState>>,
    /// The daemon version from `Welcome`, for the hosts panel.
    server_version: Arc<Mutex<Option<String>>>,
    /// Most recent request→reply round-trip. Sampled from ordinary traffic —
    /// there is no `Ping` frame, because every reply is already `req_id`-matched
    /// and timing one is free (§9).
    latency: Arc<Mutex<Option<Duration>>>,
    /// Set by the connection task whenever the mirror or connection state
    /// changes (a pushed `Snapshot`/`Delta`/`Removed`, or a connect/disconnect).
    /// Read through [`BackendEvents`], the same handle a local backend's fs
    /// watcher feeds — these off-thread updates fire no filesystem event.
    dirty: Arc<AtomicBool>,
    /// Bumped on each `Disconnected → Connected` transition. The dashboard
    /// compares it against what it last saw to fire the auto-reattach sweep
    /// (§7) exactly once per reconnect.
    reconnect_epoch: Arc<AtomicU64>,
}

impl RemoteBackend {
    /// Start mirroring a server over `transport`. Returns immediately; the
    /// mirror fills once the background task connects and receives the snapshot.
    /// Connection failure leaves an empty mirror (host shows as having no
    /// sessions); the task then retries with backoff, re-snapshotting on each
    /// reconnect, until the backend is dropped.
    pub(crate) fn connect(transport: Transport, host: HostId) -> Self {
        // Capture the ssh target before the transport is moved into the task —
        // `open_session` needs it to build the attach window's argv.
        let attach_target = match &transport {
            Transport::Ssh { target, .. } => Some(target.clone()),
            Transport::LocalSocket(_) => None,
        };
        let transport_is_local = matches!(transport, Transport::LocalSocket(_));
        let mirror = Arc::new(Mutex::new(HashMap::new()));
        let remote_exe = Arc::new(Mutex::new("miao-server".to_string()));
        let conn = Arc::new(Mutex::new(ConnState::Connecting));
        let dirty = Arc::new(AtomicBool::new(false));
        let server_version = Arc::new(Mutex::new(None));
        let latency = Arc::new(Mutex::new(None));
        let reconnect_epoch = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(connection_task(
            transport,
            ConnectionShared {
                mirror: mirror.clone(),
                remote_exe: remote_exe.clone(),
                conn: conn.clone(),
                dirty: dirty.clone(),
                server_version: server_version.clone(),
                latency: latency.clone(),
                reconnect_epoch: reconnect_epoch.clone(),
            },
            rx,
        ));
        Self {
            host,
            attach_target,
            transport_is_local,
            mirror,
            requests: tx,
            next_req_id: AtomicU64::new(1),
            remote_exe,
            conn,
            server_version,
            latency,
            dirty,
            reconnect_epoch,
        }
    }

    /// Current connection health, for the header surface.
    fn conn_state(&self) -> ConnState {
        self.conn.lock().unwrap().clone()
    }

    /// Send a request and block until its reply (or the task is gone). Returns
    /// `None` if the connection task has exited. Samples the round-trip time on
    /// the way through — the hosts panel's latency, with no dedicated frame.
    fn request(&self, make: impl FnOnce(u64) -> ClientFrame) -> Option<ServerFrame> {
        // A known-down host fails fast: queueing the request would block the
        // caller (it's on a `block_in_place`) through the whole reconnect
        // backoff. While merely dialing (Connecting) we still queue, so the very
        // first request right after `connect()` rides the pending connection.
        if matches!(
            self.conn_state(),
            ConnState::Disconnected | ConnState::Failed(_)
        ) {
            return None;
        }
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.requests
            .send(PendingRequest {
                req_id,
                frame: make(req_id),
                reply,
            })
            .ok()?;
        let sent_at = Instant::now();
        let reply = rx.blocking_recv().ok();
        if reply.is_some() {
            *self.latency.lock().unwrap() = Some(sent_at.elapsed());
        }
        reply
    }

    /// The reconnect counter behind the auto-reattach sweep (§7).
    pub(crate) fn reconnect_epoch(&self) -> u64 {
        self.reconnect_epoch.load(Ordering::Relaxed)
    }

    fn list_sessions(&self) -> Vec<LauncherState> {
        self.mirror.lock().unwrap().values().cloned().collect()
    }

    /// The remote Claude name-manifest index isn't served; remote rows get
    /// their titles from `name`/`first_prompt`, which the remote server stamps
    /// onto every session it pushes. So the index is empty for a remote host.
    fn session_index(&mut self) -> SessionIndex {
        SessionIndex::default()
    }

    fn list_resumable(&self, limit: usize) -> (Vec<ResumeCandidate>, Vec<String>) {
        match self.request(|req_id| ClientFrame::ListResumable { req_id, limit }) {
            Some(ServerFrame::Resumable {
                candidates, errors, ..
            }) => (candidates, errors),
            _ => (Vec::new(), vec!["remote host unreachable".to_string()]),
        }
    }

    fn kill_session(&self, key: &SessionKey) -> bool {
        let key = key.clone();
        matches!(
            self.request(|req_id| ClientFrame::KillSession { req_id, key }),
            Some(ServerFrame::Killed { ok: true, .. })
        )
    }

    fn set_session_flags(&self, key: &SessionKey, flags: SessionFlags) -> bool {
        let key = key.clone();
        matches!(
            self.request(|req_id| ClientFrame::SetSessionFlags { req_id, key, flags }),
            Some(ServerFrame::FlagsSet { ok: true, .. })
        )
    }

    /// Ask the server to start a launcher inside its pty pool, then build the
    /// plan for a *local* window that attaches to it. Blocks on the round-trip,
    /// so an async caller should wrap this in `block_in_place`.
    fn open_session(&self, spec: &OpenSpec) -> anyhow::Result<LaunchPlan> {
        let spec = spec.clone();
        match self.request(|req_id| ClientFrame::OpenSession { req_id, spec }) {
            Some(ServerFrame::Opened {
                session_name: Some(name),
                ..
            }) => Ok(LaunchPlan::AttachRemote {
                argv: attach_argv(
                    self.attach_target.as_deref(),
                    &self.remote_exe.lock().unwrap(),
                    &name,
                    // A session we just created can't already have a client, so
                    // the create path never steals.
                    false,
                ),
                session_name: name,
            }),
            Some(ServerFrame::Opened { error: Some(e), .. }) => anyhow::bail!(e),
            _ => anyhow::bail!("remote host unreachable"),
        }
    }

    /// The remote host's recent dirs, host-canonical. Blocks on the round-trip;
    /// empty if unreachable.
    fn recent_dirs(&self) -> Vec<String> {
        match self.request(|req_id| ClientFrame::ListRecentDirs { req_id }) {
            Some(ServerFrame::RecentDirs { cwds, .. }) => cwds,
            _ => Vec::new(),
        }
    }

    /// Directory completions on the remote fs. Blocks; empty if unreachable.
    fn complete_path(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_string();
        match self.request(|req_id| ClientFrame::CompletePath { req_id, prefix }) {
            Some(ServerFrame::PathCompletions { matches, .. }) => matches,
            _ => Vec::new(),
        }
    }

    /// Whether `path` is a directory on the remote fs. Blocks; `false` if
    /// unreachable (the picker surfaces the disconnect separately).
    fn dir_exists(&self, path: &str) -> bool {
        let path = path.to_string();
        matches!(
            self.request(|req_id| ClientFrame::CheckDir { req_id, path }),
            Some(ServerFrame::DirChecked { exists: true, .. })
        )
    }
}

/// The argv for the window that attaches to a pool session: over ssh for a
/// remote host (`ssh -t <target> miao-server attach <name>`), or directly for
/// a same-host socket transport (`miao-server attach <name>`). `-t` forces a
/// pty so the agent's TUI renders. `force` steals the session from whatever
/// client currently holds it (§10.2).
///
/// The ssh form rides the **same `ControlMaster`** the connection task already
/// established (§4), so opening an attach window skips authentication entirely
/// — instant, and no 2FA re-prompt. The deliberate cost is shared fate: OpenSSH
/// multiplexes every channel over the master's single TCP connection, so if the
/// master dies all of this host's attach windows detach at once. That's benign
/// (the pooled sessions survive; each window is one `Enter` to reattach) and
/// worth the latency.
fn attach_argv(
    target: Option<&str>,
    remote_exe: &str,
    session_name: &str,
    force: bool,
) -> Vec<String> {
    let mut argv = match target {
        Some(t) => {
            let mut v = vec!["ssh".to_string(), "-t".to_string()];
            v.extend(ssh_common_opts(&state::ssh_control_path(t)));
            v.push(t.to_string());
            v.push(remote_exe.to_string());
            v
        }
        None => vec![remote_exe.to_string()],
    };
    argv.push("attach".to_string());
    if force {
        argv.push("--force".to_string());
    }
    argv.push(session_name.to_string());
    argv
}

/// The argv for a window that opens an interactive login shell on a remote host
/// in `cwd`, over ssh: `ssh -t <target> "cd <cwd> && exec $SHELL -l"`, sharing
/// the ControlMaster like [`attach_argv`]. `-t` forces a pty so the shell is
/// interactive; the `cd` lands in the session's workdir, then we hand off to the
/// user's login shell (falling back to `/bin/sh`).
///
/// `cwd` is **host-canonical** (§3), so it may be a `~` form — which a plain
/// `'…'` quoting would render inert. `shell_quote_host_path` emits the tilde as
/// a `"$HOME"` the *remote* shell expands while keeping the rest quoted, so
/// spaces and glob chars are still safe. An empty `cwd` just drops the `cd`.
/// Pure + unit-tested.
fn remote_shell_argv(target: &str, cwd: &str) -> Vec<String> {
    let remote_cmd = if cwd.is_empty() {
        "exec \"${SHELL:-/bin/sh}\" -l".to_string()
    } else {
        format!(
            "cd {} && exec \"${{SHELL:-/bin/sh}}\" -l",
            cm_core::paths::shell_quote_host_path(cwd)
        )
    };
    let mut argv = vec!["ssh".to_string(), "-t".to_string()];
    argv.extend(ssh_common_opts(&state::ssh_control_path(target)));
    argv.push(target.to_string());
    argv.push(remote_cmd);
    argv
}

// =============================================================================
// Remote binary provisioning (next-step #1, open-decision #3)
//
// On connect, probe the remote for a version-matching `miao-server` and
// invoke whichever copy it finds: one on PATH first (a user install — never
// touched), else one at our cache path. If neither is usable and this build
// carries a payload the host could run, **upload it** and use that.
//
// The upload is the crate split's deferred "embed + auto-deploy" work, restored
// on the right footing. It died with the split because the dashboard stopped
// linking the pty pool, so the only binary it could upload — itself — wouldn't
// be a functional server. What it sends now is a real `miao-server`,
// cross-built and embedded by `build.rs` in the same command that builds the
// dashboard (`src/server_payload.rs`, `xtask/src/server.rs`). A dashboard built
// without a `bundle-*` feature behaves exactly
// as it did before: probe, don't upload, and name what's wrong.
//
// Ownership rule, and the reason `UsePath` sorts first: **PATH is the user's,
// the cache path is ours.** A version-matching binary the user installed always
// wins and is never overwritten; the cache path is refreshed to match our
// payload exactly whenever it doesn't.
// =============================================================================

/// The binary's name: what it's called on the remote's `PATH`, and what a
/// `--version` line starts with.
const SERVER_BIN: &str = "miao-server";

/// The directory a deployed miao-server lives in, relative to `$HOME`.
/// The three `REMOTE_*_REL` paths have to agree; they're literals rather than
/// `concat!`-derived because `concat!` takes literals, not consts.
const REMOTE_BIN_DIR_REL: &str = ".cache/captain-miao/bin";

/// Where a deployed miao-server lives on the remote, relative to `$HOME`.
/// Shared with `redeploy.sh`, which uploads to exactly this path.
const REMOTE_CACHE_REL: &str = ".cache/captain-miao/bin/miao-server";

/// Where an in-flight upload is staged before it's verified and published,
/// relative to `$HOME`.
const REMOTE_INCOMING_REL: &str = ".cache/captain-miao/bin/miao-server.incoming";

/// Marker beside the deployed binary recording the sha256 of the payload we put
/// there, relative to `$HOME`.
///
/// It exists because a version match is not identity: dev builds never bump the
/// version, so `0.2.1` on the host tells us nothing about *which* `0.2.1`. The
/// marker closes that — rebuild, reconnect, and the host gets the new server —
/// which is what makes `redeploy.sh`'s whole reason for existing go away for
/// payload-carrying builds.
const REMOTE_MARKER_REL: &str = ".cache/captain-miao/bin/miao-server.sha256";

/// How long a failed upload suppresses the next attempt for the same payload.
/// Without it, a host that accepts ssh but refuses the write (read-only `$HOME`,
/// full disk, no exec permission on the mount) would be re-sent multiple
/// megabytes on every reconnect — and the reconnect backoff caps at 30s.
const UPLOAD_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

/// Ceiling on one upload, so a stalled transfer can't wedge the reconnect loop
/// forever. Generous: this is multiple megabytes over whatever link the user has.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// One-shot probe of a remote host: its `$HOME`, `uname -sm`, the version of a
/// miao-server on PATH / at the cache path (if any), and the digest
/// marker we left beside the cached one (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteProbe {
    home: String,
    arch: String,
    path_version: Option<String>,
    cache_version: Option<String>,
    cache_sha: Option<String>,
}

/// The provisioning action a probe + local facts imply. Pure + unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provision {
    /// A version-matching binary is already on PATH; invoke `miao-server`.
    UsePath,
    /// A version-matching binary is already at the cache path; invoke it there.
    UseCache,
    /// Nothing usable is there, but we carry a payload this host can run: push it
    /// to the cache path, then use it. Carries the payload's digest, which is
    /// what the retry cooldown keys on.
    Upload { sha256: String },
    /// Nothing version-matching anywhere and nothing to upload; fall back to
    /// `miao-server` on PATH and let the connection fail loudly.
    FallBack,
}

/// The shell script the probe runs over ssh. Five lines out: `$HOME`, the
/// machine, a `--version` line (or our `-` sentinel) for the PATH binary and for
/// the cache-path binary, then the digest marker. `--version` errors and
/// "command not found" both land on stderr and a non-zero exit, so `|| echo -`
/// normalizes them.
fn probe_script() -> String {
    format!(
        "echo \"$HOME\"; uname -sm; \
         {SERVER_BIN} --version 2>/dev/null || echo -; \
         \"$HOME/{REMOTE_CACHE_REL}\" --version 2>/dev/null || echo -; \
         cat \"$HOME/{REMOTE_MARKER_REL}\" 2>/dev/null || echo -"
    )
}

/// Pull the version out of a remote `<binary> --version`, tolerating anything a
/// login shell's rc files printed around it — a `fish_greeting` or an `echo` in
/// `.bashrc` lands on the same stdout, so taking "the second word of the output"
/// would read the greeting instead. Pure.
fn reported_version(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next()? == SERVER_BIN)
            .then(|| words.next())?
            .map(str::to_string)
    })
}

/// Wrap a POSIX-sh script so it survives the remote's **login shell**.
///
/// `ssh host <command>` does not exec the command — it hands the whole string to
/// the account's login shell, which is regularly `fish` (and occasionally
/// `csh`), neither of which speaks `var=value`, `trap`, or `set -e`. Verified
/// the hard way: a `d="$HOME/…"` assignment came back as *"fish: Unsupported use
/// of '='"*.
///
/// So the command we send is `/bin/sh -c '<script>'`, and the wrapping survives
/// every dialect for one specific reason: a single-quoted string is literal in
/// sh, bash, zsh, fish, **and** csh. The catch is that only fish honours `\'` and
/// `\\` inside one, so the script must contain **neither a single quote nor a
/// backslash** — pinned by [`upload_script`]'s tests, and the reason the deploy
/// script writes its marker with `echo` rather than `printf '%s\n'`.
fn login_shell_safe(script: &str) -> String {
    debug_assert!(
        !script.contains('\'') && !script.contains('\\'),
        "a script wrapped for the login shell must contain no quote or backslash: {script}"
    );
    format!("/bin/sh -c '{script}'")
}

/// Parse [`probe_script`] output. A `--version` line is `miao-server
/// <ver>`; our `-` sentinel and a blank line map to `None`. Pure.
fn parse_probe(out: &str) -> Option<RemoteProbe> {
    let mut lines = out.lines();
    let home = lines.next()?.trim().to_string();
    let arch = lines.next()?.trim().to_string();
    if home.is_empty() || arch.is_empty() {
        return None;
    }
    // A plain fn, not a closure: closure lifetime elision can't express
    // "borrowed from the argument" for a `&str` in and a `&str` out.
    fn field(line: Option<&str>) -> Option<&str> {
        let l = line?.trim();
        (!l.is_empty() && l != "-").then_some(l)
    }
    // clap prints "<name> <version>"; take the version token.
    let version = |line: Option<&str>| -> Option<String> {
        field(line)?.split_whitespace().nth(1).map(str::to_string)
    };
    let path_version = version(lines.next());
    let cache_version = version(lines.next());
    let cache_sha = field(lines.next()).map(str::to_string);
    Some(RemoteProbe {
        home,
        arch,
        path_version,
        cache_version,
        cache_sha,
    })
}

/// Decide which remote binary to invoke. `payload` is `(target, sha256)` for the
/// embedded server this host could run, if we carry one — passed as plain
/// strings rather than a `&ServerPayload` so the decision stays testable in a
/// build carrying no payload — which is every test run, since the `bundle-*`
/// features are off. Pure + unit-tested.
fn decide_provision(
    local_version: &str,
    probe: &RemoteProbe,
    payload: Option<(&str, &str)>,
) -> Provision {
    // A user install always wins, and we never overwrite it.
    if probe.path_version.as_deref() == Some(local_version) {
        return Provision::UsePath;
    }
    if probe.cache_version.as_deref() == Some(local_version) {
        match payload {
            // The cache path is ours, so "right version" isn't enough once we
            // have a payload to compare against — it has to be *this* build.
            Some((_, sha)) if probe.cache_sha.as_deref() != Some(sha) => {}
            _ => return Provision::UseCache,
        }
    }
    match payload {
        Some((_, sha)) => Provision::Upload {
            sha256: sha.to_string(),
        },
        None => Provision::FallBack,
    }
}

/// The **loud** half of "assume it's there, verify, and fail loudly" (§4): turn
/// a fall-back decision into a sentence the hosts panel can show, instead of the
/// generic connection failure a missing or stale server used to produce.
/// `None` when the provision succeeded and there is nothing to report.
///
/// `upload_error` is the reason an attempted deploy didn't land; it takes
/// precedence, because "we tried to fix this for you and here's what stopped us"
/// is more actionable than "not found". `embedded` is what this build could have
/// deployed, so a `FallBack` on an arch we don't carry says *that* rather than
/// leaving the user to guess why nothing was pushed. Pure.
fn provision_failure(
    local_version: &str,
    probe: &RemoteProbe,
    action: &Provision,
    upload_error: Option<&str>,
    embedded: &[&str],
) -> Option<String> {
    if !matches!(action, Provision::FallBack) {
        return None;
    }
    if let Some(e) = upload_error {
        return Some(format!("could not deploy miao-server: {e}"));
    }
    let found: Vec<&str> = [
        probe.path_version.as_deref(),
        probe.cache_version.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Why we didn't just fix it ourselves: either this build ships no payloads
    // at all, or none for this host's arch.
    let cannot_deploy = if embedded.is_empty() {
        "this build carries no server payload".to_string()
    } else {
        format!(
            "no payload for {} (this build carries {})",
            probe.arch,
            embedded.join(", ")
        )
    };
    Some(match found.as_slice() {
        // No `redeploy.sh` in the advice: that script is a dev-loop convenience
        // in this repo, not something an installed user has.
        [] => format!(
            "miao-server not found (need {local_version}); {cannot_deploy} — \
             install it on the host"
        ),
        versions => format!(
            "miao-server version mismatch (found {}, need {local_version}); \
             {cannot_deploy}",
            versions.join(", ")
        ),
    })
}

/// The remote command an action resolves to: the absolute cache path for
/// `UseCache` (and for `Upload`, which lands there), else `miao-server`
/// from PATH.
fn remote_exe_for(action: &Provision, home: &str) -> String {
    match action {
        Provision::UseCache | Provision::Upload { .. } => format!("{home}/{REMOTE_CACHE_REL}"),
        Provision::UsePath | Provision::FallBack => "miao-server".to_string(),
    }
}

/// Remembers a failed upload so the next reconnect doesn't repeat it. Keyed on
/// the payload digest, so building a new server *does* get a fresh attempt
/// immediately — only re-sending the same bytes to the same host is suppressed.
/// Pure over an injected `now`, so the cooldown is unit-tested without sleeping.
#[derive(Default)]
struct UploadGate {
    last: Option<(String, Instant, String)>,
}

impl UploadGate {
    /// The remembered error, if uploading `sha` is still on cooldown.
    fn suppressed(&self, sha: &str, now: Instant) -> Option<&str> {
        let (failed_sha, at, error) = self.last.as_ref()?;
        (failed_sha == sha && now.duration_since(*at) < UPLOAD_RETRY_COOLDOWN)
            .then_some(error.as_str())
    }

    fn record_failure(&mut self, sha: &str, now: Instant, error: String) {
        self.last = Some((sha.to_string(), now, error));
    }

    /// Forget the last failure — called once a connection actually works, so a
    /// transient problem doesn't hold the cooldown past its usefulness.
    fn clear(&mut self) {
        self.last = None;
    }
}

/// The script the remote runs while we stream the binary into its stdin.
///
/// Staged through a temp file and moved into place only after the host itself
/// has run it: a truncated transfer or a payload for the wrong ABI fails the
/// `--version` line, `set -e` aborts, and nothing was ever visible at the path
/// the next connect will invoke. That check is also what covers the one thing
/// `uname` can't tell us, glibc vs musl.
///
/// Two constraints shape how it's written, both from [`login_shell_safe`]: no
/// single quote and no backslash anywhere in it. Hence `echo` for the marker
/// rather than `printf '%s\n'`, and hence clearing the temp file at the *start*
/// of the run rather than with an `EXIT` trap — a failed deploy leaves its temp
/// behind, which costs some cache-directory space until the next attempt and
/// buys a script that runs everywhere. Pure, so all of this is unit-tested.
fn upload_script(sha256: &str) -> String {
    format!(
        "set -e; \
         t=\"$HOME/{REMOTE_INCOMING_REL}\"; \
         mkdir -p \"$HOME/{REMOTE_BIN_DIR_REL}\"; \
         rm -f \"$t\"; \
         cat > \"$t\"; \
         chmod 0755 \"$t\"; \
         \"$t\" --version; \
         mv -f \"$t\" \"$HOME/{REMOTE_CACHE_REL}\"; \
         echo {sha256} > \"$HOME/{REMOTE_MARKER_REL}\""
    )
}

/// An ssh/scp `Command` detached from the TUI's terminal — stdin/stdout/stderr
/// all null'd. The dashboard owns the terminal (ratatui alt-screen); a child that
/// inherited it would paint over the display (scp's progress meter, ssh
/// diagnostics) and a long-lived one (the `-L` forward) would also compete for
/// stdin keystrokes. `.output()` callers don't need this — they already capture
/// out/err and null stdin — so this is for the fire-and-forget `.status()`/
/// `.spawn()` children.
fn detached(program: &str) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// The shared ssh `-o` options used for every ssh/scp invocation to a host:
/// key/agent auth only (BatchMode), a shared multiplexed connection
/// (ControlMaster/Persist over `ctl`), a keepalive so a half-open link is torn
/// down rather than hanging the UI, and a bounded initial-connect timeout so a
/// black-holed host can't wedge `setup_ssh` on the OS SYN timeout (~2 min) —
/// which would strand the reconnect task in `Connecting` (ServerAlive* only
/// governs an *established* link, not the initial `connect()`).
fn ssh_common_opts(ctl: &Path) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=120".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        format!("ControlPath={}", ctl.display()),
    ]
}

/// Run [`probe_script`] on the remote (this also primes the ControlMaster).
async fn probe_remote(target: &str, opts: &[String]) -> Option<RemoteProbe> {
    let out = Command::new("ssh")
        .args(opts)
        .arg(target)
        .arg(login_shell_safe(&probe_script()))
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

/// Stream an embedded server payload to the host's cache path over the ssh
/// connection the probe already opened (so it costs no extra authentication —
/// the ControlMaster is up by now).
///
/// The binary goes in over **stdin** rather than via `scp`: `scp` would need a
/// local temp file holding a multi-megabyte executable, and a second remote
/// command to chmod and move it, where `cat > tmp` is one round trip with no
/// local artifact. The payload is inflated here rather than shipped compressed,
/// which deliberately trades bandwidth for having no decompressor requirement on
/// a host whose entire distinguishing feature is that nothing is installed on it
/// yet.
async fn upload_server(
    target: &str,
    opts: &[String],
    payload: &'static ServerPayload,
) -> Result<(), String> {
    let bytes = payload
        .decompress()
        .map_err(|e| format!("inflating the embedded {} payload: {e}", payload.target))?;
    let len = bytes.len();
    tracing::info!(
        target: "captain_miao::provision",
        "{target}: deploying embedded {} server ({len} bytes) to ~/{REMOTE_CACHE_REL}",
        payload.target
    );

    let mut child = Command::new("ssh")
        .args(opts)
        .arg(target)
        .arg(login_shell_safe(&upload_script(payload.sha256)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The timeout below is enforced by dropping the future, which would
        // otherwise leave an ssh child holding a half-written temp file.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning ssh: {e}"))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    // Feed stdin from a task while `wait_with_output` drains stdout/stderr:
    // doing both from one task deadlocks the moment either pipe fills.
    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(&bytes).await?;
        stdin.shutdown().await
    });

    let out = tokio::time::timeout(UPLOAD_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("timed out after {}s", UPLOAD_TIMEOUT.as_secs()))?
        .map_err(|e| format!("ssh failed: {e}"))?;
    // A write error here is usually the *consequence* of the remote script
    // failing (it exited, closing the pipe), so the script's own stderr below is
    // the better message; only report this one if the script looked fine.
    let write_err = match writer.await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("sending the binary: {e}")),
        Err(e) => Some(format!("upload task: {e}")),
    };

    if !out.status.success() {
        let stderr: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(200)
            .collect();
        return Err(if stderr.is_empty() {
            write_err.unwrap_or_else(|| format!("host rejected it (rc={:?})", out.status.code()))
        } else {
            stderr
        });
    }
    if let Some(e) = write_err {
        return Err(e);
    }
    // The script echoed what the *host* got from `<binary> --version`, which is
    // the real proof it both landed intact and can run there.
    let expected = env!("CARGO_PKG_VERSION");
    let stdout = String::from_utf8_lossy(&out.stdout);
    if reported_version(&stdout).as_deref() != Some(expected) {
        return Err(format!(
            "deployed binary reported {:?}, expected {SERVER_BIN} {expected}",
            stdout.trim().chars().take(120).collect::<String>()
        ));
    }
    tracing::info!(target: "captain_miao::provision", "{target}: deployed {expected} ({} bytes)", len);
    Ok(())
}

/// Resolve the remote command to invoke: probe → decide → (deploy) → invoke.
/// Never errors — any failure resolves to `miao-server` on PATH so the
/// rest of `setup_ssh` behaves exactly as it did before provisioning existed.
/// The second half of the pair is the *diagnosis*: a `Some(reason)` names what's
/// wrong with the remote install, for `ConnState::Failed` to carry (§4).
async fn resolve_remote_exe(
    target: &str,
    opts: &[String],
    gate: &mut UploadGate,
) -> (String, Option<String>) {
    let Some(probe) = probe_remote(target, opts).await else {
        tracing::debug!(
            target: "captain_miao::provision",
            "{target}: probe failed (unreachable / no shell) → PATH miao-server"
        );
        return (
            "miao-server".to_string(),
            Some("host unreachable over ssh (or no shell)".to_string()),
        );
    };
    let local_version = env!("CARGO_PKG_VERSION");
    let payload = crate::server_payload::for_uname(&probe.arch);
    let mut action = decide_provision(local_version, &probe, payload.map(|p| (p.target, p.sha256)));
    tracing::debug!(
        target: "captain_miao::provision",
        "{target}: remote_arch={:?} path_ver={:?} cache_ver={:?} payload={:?} → {action:?}",
        probe.arch, probe.path_version, probe.cache_version, payload.map(|p| p.target)
    );

    // Deploy, if that's what the decision asked for. A failure demotes to
    // `FallBack` and is reported verbatim rather than retried here — the
    // reconnect loop is the retry mechanism, and `gate` keeps it from re-sending
    // megabytes on every pass.
    let mut upload_error = None;
    if let Provision::Upload { sha256 } = &action {
        let payload = payload.expect("Upload is only reachable with a payload");
        let now = Instant::now();
        let failure = match gate.suppressed(sha256, now) {
            Some(previous) => Some(previous.to_string()),
            None => match upload_server(target, opts, payload).await {
                Ok(()) => None,
                Err(e) => {
                    tracing::warn!(target: "captain_miao::provision", "{target}: deploy failed: {e}");
                    gate.record_failure(sha256, now, e.clone());
                    Some(e)
                }
            },
        };
        match failure {
            None => action = Provision::UseCache,
            Some(e) => {
                upload_error = Some(e);
                action = Provision::FallBack;
            }
        }
    }

    let exe = remote_exe_for(&action, &probe.home);
    tracing::debug!(target: "captain_miao::provision", "{target}: remote exe = {exe}");
    (
        exe,
        provision_failure(
            local_version,
            &probe,
            &action,
            upload_error.as_deref(),
            &crate::server_payload::embedded_targets(),
        ),
    )
}

/// Backoff bounds for reconnecting a dropped remote connection.
const RECONNECT_INITIAL: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// A connection must have lasted at least this long to count as "healthy" and
/// reset the backoff. Without this gate a link that drops right after subscribe
/// (a flapping tunnel, a crash-looping daemon) would reset to 500ms every cycle
/// and hammer the host with a reconnect storm (~4 ssh subprocesses per attempt).
const RECONNECT_HEALTHY: Duration = Duration::from_secs(20);

/// How one `serve` session ended, telling [`connection_task`] how to proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServeOutcome {
    /// The `RemoteBackend` was dropped (request channel closed) — stop for good.
    BackendDropped,
    /// A subscribed connection was lost (EOF / read / write error) — reconnect
    /// promptly (a healthy link just dropped; reset the backoff).
    ConnectionLost,
    /// The handshake/subscribe never completed — reconnect, but keep backing off
    /// (an incompatible or absent server shouldn't hot-loop). A `Some` reason is
    /// diagnosable and becomes the host's `ConnState::Failed` text.
    HandshakeFailed(Option<String>),
}

/// The handles [`connection_task`] shares with its [`RemoteBackend`]. Grouped
/// into a struct rather than passed as seven positional `Arc`s, where the two
/// `Arc<Mutex<Option<String>>>`s would be swappable at a call site.
struct ConnectionShared {
    mirror: Arc<Mutex<HashMap<SessionKey, LauncherState>>>,
    remote_exe: Arc<Mutex<String>>,
    conn: Arc<Mutex<ConnState>>,
    dirty: Arc<AtomicBool>,
    server_version: Arc<Mutex<Option<String>>>,
    latency: Arc<Mutex<Option<Duration>>>,
    reconnect_epoch: Arc<AtomicU64>,
}

/// Own a [`RemoteBackend`]'s connection for its whole lifetime, reconnecting on
/// loss. Each iteration establishes the transport, then [`serve`]s one connection
/// (handshake → subscribe → multiplex the pushed stream into the mirror with
/// request/response by `req_id`). On loss it clears the mirror, marks the host
/// disconnected, and retries with exponential backoff — until [`serve`] reports
/// the `RemoteBackend` was dropped, when the task exits.
async fn connection_task(
    transport: Transport,
    shared: ConnectionShared,
    mut requests: mpsc::UnboundedReceiver<PendingRequest>,
) {
    let ConnectionShared {
        mirror,
        remote_exe,
        conn,
        dirty,
        server_version,
        latency,
        reconnect_epoch,
    } = shared;
    // A connection-state change flips `dirty` alongside `conn` so the dashboard
    // reloads + redraws the header promptly on connect/disconnect, not only when
    // the mirror later changes.
    let store = |s: ConnState| {
        *conn.lock().unwrap() = s;
        dirty.store(true, Ordering::Relaxed);
    };
    let mut backoff = RECONNECT_INITIAL;
    let mut was_connected = false;
    // Lives for the whole task, so one host's refusal to accept a deployed
    // server isn't re-litigated (at multiple megabytes a go) on every reconnect.
    let mut upload_gate = UploadGate::default();
    // The diagnosis the last attempt reached, held across the wait *and* the
    // next attempt. Retrying doesn't make "no miao-server on the host" any less
    // true, so blinking the sentence off to `connecting` once per backoff tick
    // only makes it unreadable — the reason stands until an attempt concludes
    // something else. Every path that loops sets this beside the state it
    // stores, so at the top of each pass `Some` means the stored state is
    // already the matching `Failed` — which is why the re-dial can skip its own
    // store rather than re-announce the same sentence.
    let mut standing_failure: Option<String> = None;
    loop {
        if standing_failure.is_none() {
            store(ConnState::Connecting);
        }
        // Establish the transport; for ssh, (re)stand up the forward+server
        // child. Re-running `setup_ssh` on each attempt is deliberate: it also
        // re-cancels any stale ControlMaster forward, which is what makes a
        // reconnect actually bind its socket.
        let mut failure: Option<String> = None;
        let established = match &transport {
            Transport::LocalSocket(p) => Some((p.clone(), None)),
            Transport::Ssh { target, local_sock } => {
                match setup_ssh(
                    target,
                    local_sock,
                    &remote_exe,
                    &mut failure,
                    &mut upload_gate,
                )
                .await
                {
                    Some(child) => Some((local_sock.clone(), Some(child))),
                    None => {
                        tracing::warn!(target: "captain_miao::ssh", "{target}: ssh setup failed — will retry");
                        None
                    }
                }
            }
        };
        let Some((sock_path, ssh_child)) = established else {
            // A diagnosable cause (server missing, version mismatch, host
            // unreachable) is surfaced verbatim instead of a bare ⚠ (§4). The
            // task keeps retrying either way — `Failed` is a *label*, not a
            // terminal state, since deploying the binary should heal it without
            // the user restarting anything.
            standing_failure = failure;
            store(match &standing_failure {
                Some(reason) => ConnState::Failed(reason.clone()),
                None => ConnState::Disconnected,
            });
            if !wait_before_retry(&mut requests, &mut backoff).await {
                return;
            }
            continue;
        };
        // The ssh server binds its socket a beat after the forward is up, so
        // retry the first connect; a direct socket needs only a couple of tries.
        let attempts = if ssh_child.is_some() { 16 } else { 3 };
        let Some(stream) = connect_with_retry(&sock_path, attempts).await else {
            drop(ssh_child); // kill_on_drop tears ssh down
            // Setup got this far without a diagnosis, so an older one is stale.
            standing_failure = None;
            store(ConnState::Disconnected);
            if !wait_before_retry(&mut requests, &mut backoff).await {
                return;
            }
            continue;
        };
        tracing::debug!(target: "captain_miao::ssh", "connected to {}; serving", sock_path.display());
        // A Disconnected → Connected edge bumps the epoch, which is what the
        // dashboard's auto-reattach sweep watches (§7): after a laptop sleep or
        // a broken pipe, every session that *had* an attach window gets one
        // again, without the user re-Entering each row.
        if was_connected {
            reconnect_epoch.fetch_add(1, Ordering::Relaxed);
        }
        was_connected = true;
        // The host is demonstrably working, so any remembered deploy failure is
        // history — a later one gets a fresh attempt rather than inheriting an
        // old cooldown.
        upload_gate.clear();
        store(ConnState::Connected);
        let connected_at = Instant::now();
        let outcome = serve(stream, &mirror, &dirty, &server_version, &mut requests).await;
        drop(ssh_child); // explicit: kill the ssh child once the connection ends
        // The mirror is now stale; clear it so the host shows no (misleading)
        // rows while disconnected. A fresh `Snapshot` refills it on reconnect.
        // `store(Disconnected)` below flips `dirty` so the cleared rows redraw.
        mirror.lock().unwrap().clear();
        *latency.lock().unwrap() = None;
        standing_failure = match &outcome {
            ServeOutcome::HandshakeFailed(Some(reason)) => Some(reason.clone()),
            _ => None,
        };
        store(match &standing_failure {
            Some(reason) => ConnState::Failed(reason.clone()),
            None => ConnState::Disconnected,
        });
        tracing::debug!(
            target: "captain_miao::ssh",
            "serve loop ended for {} ({outcome:?})", sock_path.display()
        );
        match outcome {
            ServeOutcome::BackendDropped => return,
            // Reset the backoff only if the connection was actually healthy for a
            // while — a link that dropped seconds after subscribing keeps backing
            // off, so a flapping host doesn't trigger a reconnect storm.
            ServeOutcome::ConnectionLost if connected_at.elapsed() >= RECONNECT_HEALTHY => {
                backoff = RECONNECT_INITIAL;
            }
            ServeOutcome::ConnectionLost | ServeOutcome::HandshakeFailed(_) => {}
        }
        if !wait_before_retry(&mut requests, &mut backoff).await {
            return;
        }
    }
}

/// Wait out the current `backoff` before the next reconnect, then double it
/// (capped at [`RECONNECT_MAX`]). Returns `false` if the backend was dropped
/// meanwhile (its request channel closed) — the caller should terminate. Any
/// request that races in while we wait is failed immediately (its reply sender
/// is dropped → the caller sees the host as unreachable) rather than left to
/// hang for the whole backoff.
async fn wait_before_retry(
    requests: &mut mpsc::UnboundedReceiver<PendingRequest>,
    backoff: &mut Duration,
) -> bool {
    let this = *backoff;
    *backoff = (*backoff * 2).min(RECONNECT_MAX);
    let sleep = tokio::time::sleep(this);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return true,
            req = requests.recv() => {
                // A request racing in while we're down is failed immediately:
                // taking it here (`req` then drops) closes its reply sender, so
                // the caller sees the host as unreachable instead of blocking for
                // the whole backoff. `None` means the backend itself was dropped.
                if req.is_none() {
                    return false;
                }
            }
        }
    }
}

/// Try to connect to `sock` a few times, sleeping between attempts.
async fn connect_with_retry(sock: &Path, attempts: u32) -> Option<UnixStream> {
    let mut last_err = None;
    for i in 0..attempts {
        match UnixStream::connect(sock).await {
            Ok(s) => return Some(s),
            Err(e) => last_err = Some(e),
        }
        if i + 1 < attempts {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
    tracing::warn!(
        target: "captain_miao::ssh",
        "could not connect to forwarded socket {} after {attempts} attempts: {:?}",
        sock.display(), last_err
    );
    None
}

/// Stand up an ssh host: ensure the remote daemon is running (and learn its
/// socket path) with `daemon ensure`, then spawn a **forward-only** `ssh -N -L
/// <local>:<remote> target` child that just holds the tunnel. The daemon is
/// self-daemonizing and persistent, so it's fully decoupled from this child —
/// dropping the backend (or a reconnect) kills only the tunnel, never the daemon
/// or its sessions. The returned child is `kill_on_drop`. Returns None if ssh or
/// the remote binary fails. Requires key/agent auth (BatchMode).
async fn setup_ssh(
    target: &str,
    local_sock: &Path,
    remote_exe: &Arc<Mutex<String>>,
    failure: &mut Option<String>,
    upload_gate: &mut UploadGate,
) -> Option<tokio::process::Child> {
    let ctl = crate::state::ssh_control_path(target);
    // ssh's ControlMaster won't create ControlPath's parent dir, and the first
    // ssh below (the probe) already needs it — so ensure the short ssh-socket
    // dir exists (0700, so another user can't hijack the control socket).
    if let Some(dir) = ctl.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let opts = ssh_common_opts(&ctl);

    // Probe the host, auto-provision our binary if it's missing/stale and our
    // build can run there (open-decision #3), and resolve the command to invoke.
    // This also primes the ControlMaster, replacing the `--print-path` priming.
    // Non-fatal: a failure resolves to `miao-server` on PATH, the prior default.
    let (exe, diagnosis) = resolve_remote_exe(target, &opts, upload_gate).await;
    *remote_exe.lock().unwrap() = exe.clone();
    // Carry the diagnosis out even when we go on to try the fallback: if the
    // `daemon ensure` below fails, *this* is the reason the user needs, not
    // "connection failed".
    *failure = diagnosis;

    // Ensure the remote daemon is running AND learn its socket path in one call:
    // `daemon ensure` self-daemonizes if needed (idempotent — a no-op against a
    // live one) and prints the socket path on its first stdout line. This starts
    // the persistent daemon; the separate `-N -L` child below only forwards.
    let out = Command::new("ssh")
        .args(&opts)
        .arg(target)
        .arg(&exe)
        .args(["daemon", "ensure"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(
            target: "captain_miao::ssh",
            "{target}: `{exe} daemon ensure` failed (rc={:?}): {}",
            out.status.code(),
            stderr.trim()
        );
        // Keep a provisioning diagnosis if we have one (it's the root cause);
        // otherwise report what the remote actually said.
        if failure.is_none() {
            let detail: String = stderr.trim().chars().take(160).collect();
            *failure = Some(if detail.is_empty() {
                format!(
                    "`daemon ensure` failed on the host (rc={:?})",
                    out.status.code()
                )
            } else {
                format!("`daemon ensure` failed on the host: {detail}")
            });
        }
        return None;
    }
    // The daemon answered, so nothing is wrong with the install after all.
    *failure = None;
    let remote_sock = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if remote_sock.is_empty() {
        tracing::warn!(target: "captain_miao::ssh", "{target}: daemon ensure returned no socket path");
        return None;
    }
    tracing::debug!(target: "captain_miao::ssh", "{target}: remote daemon socket = {remote_sock}");

    // The persistent ControlMaster can retain a *stale forward* for this local
    // socket path from an earlier connection whose slave was SIGKILL'd (the
    // forward child's `kill_on_drop`), so it never told the master to tear the
    // forward down. A fresh `-L` request for an already-registered path is a
    // silent no-op — the master binds nothing — so every reconnect then fails
    // with ENOENT, self-perpetuating once the first disconnect poisons the
    // master. Cancel any such stale forward first; it's a quiet no-op when none
    // exists (or no master is up). Verified against a real host: without this the
    // forward socket never appears; with it, it binds on the first try.
    let _ = detached("ssh")
        .args(&opts)
        .arg("-O")
        .arg("cancel")
        .arg("-L")
        .arg(format!("{}:{}", local_sock.display(), remote_sock))
        .arg(target)
        .status()
        .await;

    // Clear any stale local socket and ensure its parent dir exists.
    if let Some(parent) = local_sock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(local_sock);

    // A forward-ONLY child: `-N` runs no remote command, it just holds the `-L`
    // tunnel open (the daemon is already running and persistent — the tunnel and
    // the daemon are now independent). Killed when the backend drops / on
    // reconnect, with no effect on the daemon. Detached stdin + stdout (must never
    // touch the TUI's terminal), but stderr → a per-host log file: ssh's
    // diagnostics for a failed forward are the only clue when the local socket
    // never appears, and a file (unlike the inherited terminal) can't corrupt the
    // display.
    let safe: String = target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let err_path = state::state_dir()
        .join("logs")
        .join(format!("ssh-forward-{safe}.log"));
    let stderr = std::fs::File::create(&err_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    detached("ssh")
        .args(&opts)
        .arg("-N")
        .arg("-L")
        .arg(format!("{}:{}", local_sock.display(), remote_sock))
        .arg(target)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .ok()
}

/// Handshake, subscribe, then multiplex the pushed session stream into the
/// mirror with request/response, until the peer hangs up or the backend drops.
/// The [`ServeOutcome`] tells the caller whether to reconnect and how fast.
async fn serve(
    stream: UnixStream,
    mirror: &Arc<Mutex<HashMap<SessionKey, LauncherState>>>,
    dirty: &Arc<AtomicBool>,
    server_version: &Arc<Mutex<Option<String>>>,
    requests: &mut mpsc::UnboundedReceiver<PendingRequest>,
) -> ServeOutcome {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    // Handshake + subscribe.
    let hello = ClientFrame::Hello {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROTOCOL_VERSION,
    };
    if write_frame(&mut wr, &hello).await.is_err() {
        tracing::warn!(target: "captain_miao::ssh", "failed to send Hello");
        return ServeOutcome::HandshakeFailed(None);
    }
    match read_frame::<_, ServerFrame>(&mut rd).await {
        Ok(Some(ServerFrame::Welcome {
            protocol,
            server_version: sv,
            ..
        })) => {
            // Only a server *below* the floor is refused — a newer one is fine,
            // since both sides decode unknown frames/fields tolerantly (§3).
            if !protocol_compatible(protocol) {
                tracing::warn!(
                    target: "captain_miao::ssh",
                    "server speaks protocol {protocol}, below our floor {PROTOCOL_MIN}"
                );
                return ServeOutcome::HandshakeFailed(Some(format!(
                    "daemon {sv} speaks protocol {protocol}; this build needs ≥ {PROTOCOL_MIN}"
                )));
            }
            tracing::debug!(target: "captain_miao::ssh", "handshake ok (protocol {protocol}, server {sv})");
            *server_version.lock().unwrap() = Some(sv);
        }
        // No usable Welcome at all: something is answering the socket that
        // isn't our daemon, or it hung up mid-handshake.
        other => {
            tracing::warn!(target: "captain_miao::ssh", "handshake failed, no usable Welcome: {other:?}");
            return ServeOutcome::HandshakeFailed(None);
        }
    }
    if write_frame(&mut wr, &ClientFrame::Subscribe).await.is_err() {
        tracing::warn!(target: "captain_miao::ssh", "failed to send Subscribe");
        return ServeOutcome::HandshakeFailed(None);
    }

    let mut pending: HashMap<u64, oneshot::Sender<ServerFrame>> = HashMap::new();
    loop {
        tokio::select! {
            frame = read_frame::<_, ServerFrame>(&mut rd) => {
                let frame = match frame {
                    Ok(Some(f)) => f,
                    Ok(None) => { tracing::debug!(target: "captain_miao::ssh", "server closed the stream (EOF)"); return ServeOutcome::ConnectionLost; }
                    Err(e) => { tracing::warn!(target: "captain_miao::ssh", "frame read/parse error: {e}"); return ServeOutcome::ConnectionLost; }
                };
                match frame {
                    ServerFrame::Snapshot { sessions } => {
                        tracing::debug!(target: "captain_miao::ssh", "snapshot: {} sessions", sessions.len());
                        let mut m = mirror.lock().unwrap();
                        m.clear();
                        for s in sessions {
                            m.insert(s.key(), s);
                        }
                        // The mirror changed off-thread; wake the dashboard loop.
                        dirty.store(true, Ordering::Relaxed);
                    }
                    ServerFrame::Delta { state } => {
                        mirror.lock().unwrap().insert(state.key(), *state);
                        dirty.store(true, Ordering::Relaxed);
                    }
                    ServerFrame::Removed { key } => {
                        mirror.lock().unwrap().remove(&key);
                        dirty.store(true, Ordering::Relaxed);
                    }
                    // Every reply routes by `req_id` through one accessor, so a
                    // future reply variant needs no change here (§3 tolerance).
                    // `None` covers the pushed stream and an unknown frame from
                    // a newer peer, both of which are simply ignored.
                    _ => {
                        if let Some(tx) = frame.req_id().and_then(|id| pending.remove(&id)) {
                            let _ = tx.send(frame);
                        }
                    }
                }
            }
            req = requests.recv() => {
                let Some(req) = req else { return ServeOutcome::BackendDropped };
                pending.insert(req.req_id, req.reply);
                if write_frame(&mut wr, &req.frame).await.is_err() {
                    return ServeOutcome::ConnectionLost;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::SessionStatus;
    use std::time::Duration;
    use tokio::net::UnixListener;

    fn test_state(pid: u32) -> LauncherState {
        LauncherState {
            agent: AgentControl::Claude,
            launcher_pid: pid,
            session_id: Some(format!("sess-{pid}")),
            window_id: None,
            tab_id: None,
            cwd: "/tmp".to_string(),
            status: SessionStatus::Idle,
            last_tool: None,
            updated_at: 0,
            active_since: None,
            last_prompt: None,
            child_pid: Some(pid + 1),
            last_error: None,
            context_tokens: None,
            model: None,
            name: None,
            first_prompt: None,
            pool_session: None,
            launch_id: None,
            terminal: None,
            flags: None,
            attached: None,
            host: crate::state::HostId::local(),
        }
    }

    /// The tail of an ssh argv after the `-o` option block, so the assertions
    /// stay about *shape* rather than restating `ssh_common_opts`.
    fn ssh_tail(argv: &[String]) -> Vec<String> {
        let start = argv
            .iter()
            .rposition(|a| a == "-o")
            .map(|i| i + 2)
            .unwrap_or(0);
        argv[start..].to_vec()
    }

    #[test]
    fn attach_argv_ssh_vs_direct() {
        let ssh = attach_argv(Some("user@box"), "miao-server", "s1", false);
        assert_eq!(ssh[0], "ssh");
        assert_eq!(ssh[1], "-t");
        assert_eq!(ssh_tail(&ssh), ["user@box", "miao-server", "attach", "s1"]);
        // Attach windows ride the connection task's ControlMaster (§4), so they
        // skip authentication entirely — that's the whole point of the options.
        assert!(ssh.iter().any(|a| a.starts_with("ControlPath=")));
        assert!(ssh.iter().any(|a| a == "ControlMaster=auto"));

        // A socket transport (pooled localhost) needs no ssh hop at all.
        assert_eq!(
            attach_argv(None, "miao-server", "s1", false),
            ["miao-server", "attach", "s1"]
        );
        // The steal is a flag on the attach, never on the create path.
        assert_eq!(
            attach_argv(None, "miao-server", "s1", true),
            ["miao-server", "attach", "--force", "s1"]
        );
        // A deployed cache path is invoked in place of `miao-server`.
        let cache = "/home/u/.cache/captain-miao/bin/miao-server";
        let ssh = attach_argv(Some("user@box"), cache, "s1", false);
        assert_eq!(ssh_tail(&ssh), ["user@box", cache, "attach", "s1"]);
    }

    #[test]
    fn remote_shell_argv_cds_and_execs_login_shell() {
        let argv = remote_shell_argv("user@box", "/home/u/proj");
        assert_eq!(
            ssh_tail(&argv),
            [
                "user@box",
                "cd '/home/u/proj' && exec \"${SHELL:-/bin/sh}\" -l"
            ]
        );
        // The landmine (§3): a host-canonical `~` path must reach the remote as
        // something the *remote* shell expands. Single-quoting it — the obvious
        // thing — would make `cd '~/proj'` fail on every host.
        let argv = remote_shell_argv("box", "~/proj");
        assert_eq!(
            ssh_tail(&argv),
            [
                "box",
                "cd \"$HOME\"/'proj' && exec \"${SHELL:-/bin/sh}\" -l"
            ]
        );
        // Empty cwd drops the `cd` and just opens a login shell.
        let argv = remote_shell_argv("box", "");
        assert_eq!(ssh_tail(&argv), ["box", "exec \"${SHELL:-/bin/sh}\" -l"]);
    }

    fn probe(arch: &str, path: Option<&str>, cache: Option<&str>) -> RemoteProbe {
        RemoteProbe {
            home: "/home/u".into(),
            arch: arch.into(),
            path_version: path.map(str::to_string),
            cache_version: cache.map(str::to_string),
            cache_sha: None,
        }
    }

    /// The payload a test dashboard carries: `(target, sha256)`, exactly the
    /// shape `decide_provision` takes.
    const PAYLOAD: (&str, &str) = ("x86_64-unknown-linux-gnu", "abc123");

    fn upload(sha: &str) -> Provision {
        Provision::Upload {
            sha256: sha.to_string(),
        }
    }

    #[test]
    fn parse_probe_extracts_home_arch_versions_and_marker() {
        let out = "/home/u\nLinux x86_64\nmiao-server 0.1.0\n-\n-\n";
        let p = parse_probe(out).unwrap();
        assert_eq!(p.home, "/home/u");
        assert_eq!(p.arch, "Linux x86_64");
        assert_eq!(p.path_version.as_deref(), Some("0.1.0"));
        assert_eq!(p.cache_version, None); // the "-" sentinel
        assert_eq!(p.cache_sha, None);
    }

    #[test]
    fn parse_probe_handles_cache_only_and_blank_lines() {
        // PATH binary missing ("-"), cache binary present, marker written.
        let p = parse_probe("/root\nDarwin arm64\n-\nmiao-server 0.2.0\ndeadbeef\n").unwrap();
        assert_eq!(p.path_version, None);
        assert_eq!(p.cache_version.as_deref(), Some("0.2.0"));
        assert_eq!(p.cache_sha.as_deref(), Some("deadbeef"));
        // A host deployed by an older build (or by redeploy.sh) has no marker.
        let p = parse_probe("/root\nDarwin arm64\n-\nmiao-server 0.2.0").unwrap();
        assert_eq!(p.cache_sha, None);
        // Truncated/garbage output → None rather than a half-built probe.
        assert!(parse_probe("/home/u").is_none());
        assert!(parse_probe("\n\n").is_none());
    }

    #[test]
    fn decide_prefers_path_install_over_cache() {
        let lx = "Linux x86_64";
        // PATH match wins outright — a user install beats our cache copy, and is
        // never overwritten even when we carry a payload.
        let p = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(decide_provision("0.1.0", &p, None), Provision::UsePath);
        assert_eq!(
            decide_provision("0.1.0", &p, Some(PAYLOAD)),
            Provision::UsePath
        );
        // No PATH match, but our cache copy matches → use it.
        let p = probe(lx, None, Some("0.1.0"));
        assert_eq!(decide_provision("0.1.0", &p, None), Provision::UseCache);
    }

    #[test]
    fn decide_falls_back_when_nothing_matches_and_we_carry_nothing() {
        let lx = "Linux x86_64";
        // Nothing deployed anywhere.
        assert_eq!(
            decide_provision("0.1.0", &probe(lx, None, None), None),
            Provision::FallBack
        );
        // Both present but stale — a version mismatch must not be invoked, since
        // the wire protocol isn't guaranteed compatible across versions.
        let stale = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(decide_provision("0.2.0", &stale, None), Provision::FallBack);
    }

    #[test]
    fn a_payload_turns_every_fallback_into_a_deploy() {
        let lx = "Linux x86_64";
        // Nothing there at all — the fresh-host case.
        assert_eq!(
            decide_provision("0.1.0", &probe(lx, None, None), Some(PAYLOAD)),
            upload("abc123")
        );
        // Everything there but stale.
        let stale = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(
            decide_provision("0.2.0", &stale, Some(PAYLOAD)),
            upload("abc123")
        );
    }

    #[test]
    fn a_same_version_cache_binary_is_refreshed_unless_it_is_this_exact_build() {
        // The dev loop: the version never moves between builds, so identity has
        // to come from the digest marker we left beside the binary.
        let mut p = probe("Linux x86_64", None, Some("0.1.0"));

        p.cache_sha = Some("abc123".into());
        assert_eq!(
            decide_provision("0.1.0", &p, Some(PAYLOAD)),
            Provision::UseCache
        );

        // A different build of the same version — re-deploy.
        p.cache_sha = Some("999999".into());
        assert_eq!(
            decide_provision("0.1.0", &p, Some(PAYLOAD)),
            upload("abc123")
        );

        // No marker at all (redeploy.sh, or a pre-marker dashboard). We own this
        // path, so we take it over rather than trusting an unlabelled binary.
        p.cache_sha = None;
        assert_eq!(
            decide_provision("0.1.0", &p, Some(PAYLOAD)),
            upload("abc123")
        );
        // …but a build carrying no payload has nothing better to offer, so it
        // keeps using what's there.
        assert_eq!(decide_provision("0.1.0", &p, None), Provision::UseCache);
    }

    #[test]
    fn remote_exe_resolves_cache_path_or_falls_back_to_path() {
        assert_eq!(
            remote_exe_for(&Provision::UsePath, "/home/u"),
            "miao-server"
        );
        assert_eq!(
            remote_exe_for(&Provision::FallBack, "/home/u"),
            "miao-server"
        );
        assert_eq!(
            remote_exe_for(&Provision::UseCache, "/root"),
            "/root/.cache/captain-miao/bin/miao-server"
        );
        // An upload lands at the cache path, so it resolves there too.
        assert_eq!(
            remote_exe_for(&upload("abc123"), "/root"),
            "/root/.cache/captain-miao/bin/miao-server"
        );
    }

    #[test]
    fn the_failure_text_says_which_of_the_three_things_went_wrong() {
        let lx = "Linux x86_64";
        let missing = probe(lx, None, None);
        let msg = provision_failure("0.2.0", &missing, &Provision::FallBack, None, &[]).unwrap();
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("carries no server payload"), "{msg}");
        // The advice has to be something an installed user can act on — this
        // repo's dev-loop script isn't on their machine.
        assert!(!msg.contains("redeploy.sh"), "{msg}");

        let stale = probe(lx, Some("0.1.0"), None);
        let msg = provision_failure("0.2.0", &stale, &Provision::FallBack, None, &[]).unwrap();
        assert!(msg.contains("version mismatch"), "{msg}");

        // A build that *does* carry payloads, just not for this host: say so,
        // and say what it has, so the fix (a build carrying that arch) is
        // obvious.
        let msg = provision_failure(
            "0.2.0",
            &probe("Linux riscv64", None, None),
            &Provision::FallBack,
            None,
            &["x86_64-unknown-linux-gnu"],
        )
        .unwrap();
        assert!(msg.contains("no payload for Linux riscv64"), "{msg}");
        assert!(msg.contains("x86_64-unknown-linux-gnu"), "{msg}");

        // A failed deploy outranks both: it's the more actionable sentence.
        let msg = provision_failure(
            "0.2.0",
            &missing,
            &Provision::FallBack,
            Some("disk full"),
            &[],
        )
        .unwrap();
        assert!(msg.contains("could not deploy"), "{msg}");
        assert!(msg.contains("disk full"), "{msg}");

        // Nothing to report when provisioning worked.
        assert!(provision_failure("0.2.0", &missing, &Provision::UseCache, None, &[]).is_none());
        assert!(provision_failure("0.2.0", &missing, &upload("x"), None, &[]).is_none());
    }

    #[test]
    fn a_failed_upload_is_not_retried_until_the_cooldown_or_a_new_payload() {
        let mut gate = UploadGate::default();
        let t0 = Instant::now();
        assert!(gate.suppressed("sha-a", t0).is_none());

        gate.record_failure("sha-a", t0, "read-only $HOME".into());
        // Same payload, still inside the window: reuse the remembered reason
        // rather than re-sending megabytes on every reconnect.
        assert_eq!(
            gate.suppressed("sha-a", t0 + Duration::from_secs(30)),
            Some("read-only $HOME")
        );
        // A *different* payload is a new fact — try immediately.
        assert!(gate.suppressed("sha-b", t0).is_none());
        // Past the cooldown, so is the same one.
        assert!(
            gate.suppressed("sha-a", t0 + UPLOAD_RETRY_COOLDOWN + Duration::from_secs(1))
                .is_none()
        );
        // A working connection wipes the memory outright.
        gate.clear();
        assert!(gate.suppressed("sha-a", t0).is_none());
    }

    #[test]
    fn the_upload_script_stages_verifies_then_moves() {
        let script = upload_script("d1g3st");
        // Order is the safety property: the binary is only visible at the path
        // the next connect invokes *after* the host itself has run it.
        let stage = script.find("cat > ").unwrap();
        let verify = script.find("--version").unwrap();
        let publish = script.find("mv -f").unwrap();
        let marker = script.find("miao-server.sha256").unwrap();
        assert!(stage < verify, "{script}");
        assert!(verify < publish, "{script}");
        assert!(publish < marker, "{script}");
        // The temp is cleared before it's written, not after — there is no trap
        // to clean up with (see the doc comment), so the next attempt does it.
        assert!(script.find("rm -f").unwrap() < stage, "{script}");
        // A failure anywhere aborts rather than publishing half a deploy.
        assert!(script.starts_with("set -e;"), "{script}");
        // The digest is what a later probe compares against.
        assert!(script.contains("echo d1g3st"), "{script}");
        // `$HOME` is expanded by the *remote* shell — the client is
        // home-ignorant (§3), so it must never splice its own in.
        assert!(
            script.contains("\"$HOME/.cache/captain-miao/bin\""),
            "{script}"
        );
    }

    #[test]
    fn the_deployed_version_is_read_past_whatever_the_login_shell_printed() {
        assert_eq!(
            reported_version("miao-server 0.2.1\n").as_deref(),
            Some("0.2.1")
        );
        // A `fish_greeting` or an `echo` in .bashrc shares this stdout.
        assert_eq!(
            reported_version("Welcome to box!\n\nmiao-server 0.2.1\n").as_deref(),
            Some("0.2.1")
        );
        assert_eq!(reported_version("Welcome to box!\n"), None);
        assert_eq!(reported_version("miao-server\n"), None);
        assert_eq!(reported_version(""), None);
    }

    #[test]
    fn every_script_we_send_survives_the_wrapping_that_defeats_a_login_shell() {
        // The constraint that makes `/bin/sh -c '<script>'` parse identically in
        // sh, bash, zsh, fish and csh. `login_shell_safe` debug-asserts it too,
        // but only for the scripts a given run happens to build.
        for script in [probe_script(), upload_script(&"a".repeat(64))] {
            let script = script.as_str();
            assert!(!script.contains('\''), "{script}");
            assert!(!script.contains('\\'), "{script}");
        }
        assert_eq!(login_shell_safe("echo hi"), "/bin/sh -c 'echo hi'");
    }

    /// Run the deploy command against a throwaway `$HOME` under a given shell,
    /// feeding it a stand-in binary on stdin — exactly as `ssh` would.
    ///
    /// This is the half of the deploy that exists only as a shell string, so
    /// there is nothing else to type-check it: the staging/verify/publish
    /// ordering and the quoting are only *actually* correct if a shell agrees.
    /// A stand-in executable rather than a real payload, so it runs in every
    /// checkout and on any arch — and needs no embedded server.
    fn run_deploy(shell: &str, home: &Path, stdin_bytes: &[u8], sha: &str) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new(shell)
            .arg("-c")
            .arg(login_shell_safe(&upload_script(sha)))
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {shell}: {e}"));
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_bytes)
            .expect("feeding the script");
        child.wait_with_output().expect("waiting for the shell")
    }

    fn run_upload_script(home: &Path, stdin_bytes: &[u8], sha: &str) -> std::process::Output {
        run_deploy("/bin/sh", home, stdin_bytes, sha)
    }

    fn scratch_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cm-upload-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_upload_script_deploys_a_binary_the_host_can_run() {
        let home = scratch_home("ok");
        let version = env!("CARGO_PKG_VERSION");
        let fake = format!("#!/bin/sh\necho 'miao-server {version}'\n");
        let out = run_upload_script(&home, fake.as_bytes(), "d1g3st");

        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The version the *host* reported is what `upload_server` verifies.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("miao-server {version}")
        );

        let deployed = home.join(REMOTE_CACHE_REL);
        assert_eq!(std::fs::read(&deployed).unwrap(), fake.as_bytes());
        assert_eq!(
            std::fs::metadata(&deployed).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // The marker is what makes the next probe recognise this exact build.
        assert_eq!(
            std::fs::read_to_string(home.join(REMOTE_MARKER_REL))
                .unwrap()
                .trim(),
            "d1g3st"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_deploy_lands_under_every_login_shell_installed_here() {
        // The bug this pins: `ssh host <cmd>` hands `<cmd>` to the *account's
        // login shell*, so a POSIX-sh script reached a fish user as
        // "fish: Unsupported use of '='" and no host with fish as its shell
        // could ever be provisioned. Whichever of these a machine has, they all
        // have to produce the same deploy.
        let version = env!("CARGO_PKG_VERSION");
        let fake = format!("#!/bin/sh\necho 'miao-server {version}'\n");
        for shell in ["/bin/sh", "bash", "zsh", "fish", "tcsh"] {
            if std::process::Command::new(shell)
                .arg("-c")
                .arg("exit 0")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err()
            {
                continue; // not installed here
            }
            let home = scratch_home(&format!("shell-{}", shell.replace('/', "_")));
            let out = run_deploy(shell, &home, fake.as_bytes(), "d1g3st");
            assert!(
                out.status.success(),
                "{shell}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                std::fs::read(home.join(REMOTE_CACHE_REL)).unwrap(),
                fake.as_bytes(),
                "{shell} did not deploy the binary"
            );
            assert_eq!(
                std::fs::read_to_string(home.join(REMOTE_MARKER_REL))
                    .unwrap()
                    .trim(),
                "d1g3st",
                "{shell} did not write the marker"
            );
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn a_binary_the_host_cannot_run_never_reaches_the_cache_path() {
        // The wrong-ABI / truncated-transfer case, which is the whole reason the
        // script verifies before it publishes: the previous deploy (if any) must
        // survive, and no temp file may be left behind.
        let home = scratch_home("bad");
        let bin_dir = home.join(".cache/captain-miao/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(home.join(REMOTE_CACHE_REL), b"the previous server").unwrap();

        let out = run_upload_script(&home, b"\x7fELF\x00 not runnable here", "d1g3st");
        assert!(!out.status.success());
        assert_eq!(
            std::fs::read(home.join(REMOTE_CACHE_REL)).unwrap(),
            b"the previous server"
        );
        assert!(!home.join(REMOTE_MARKER_REL).exists());
        let leftovers: Vec<_> = std::fs::read_dir(&bin_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "trap left debris: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The whole provisioning path against a **real** ssh host: probe, deploy
    /// the embedded payload, verify it runs there, then confirm a second connect
    /// recognises its own work and doesn't re-send it.
    ///
    /// Ignored by default because it needs a host, and a payload manifest so the
    /// test binary carries a server for that host's arch. It is the one part of
    /// §10.3's end-to-end checklist that
    /// doesn't need a *remote* machine — an sshd on localhost exercises every
    /// line of it — so run it whenever the deploy path changes:
    ///
    /// ```text
    /// # Obtain a server and note where its manifest landed:
    /// cargo xtask prepare-servers --out /tmp/srv
    /// printf '%s\t%s\t%s\n' x86_64-unknown-linux-gnu "$SHA" /tmp/srv/…/server.gz \
    ///   > /tmp/payloads.tsv
    ///
    /// CM_SERVER_PAYLOAD_MANIFEST=/tmp/payloads.tsv \
    ///   CM_TEST_SSH_TARGET=127.0.0.1 \
    ///   CM_TEST_SSH_OPTS="-p 2299 -i /tmp/id -o StrictHostKeyChecking=no" \
    ///   cargo test -p captain-miao --features remote -- \
    ///     --ignored provisions_a_real_host
    /// ```
    ///
    /// The manifest is what puts a payload in the test binary; without one there
    /// is nothing to deploy and the test says so.
    ///
    /// It deploys to `~/.cache/captain-miao/bin/` on the target, which is
    /// exactly where a normal connect would put it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs a real ssh host: set CM_TEST_SSH_TARGET"]
    async fn provisions_a_real_host_end_to_end() {
        let target = std::env::var("CM_TEST_SSH_TARGET").expect("CM_TEST_SSH_TARGET");
        let ctl = crate::state::ssh_control_path(&target);
        if let Some(dir) = ctl.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut opts = ssh_common_opts(&ctl);
        if let Ok(extra) = std::env::var("CM_TEST_SSH_OPTS") {
            opts.extend(extra.split_whitespace().map(str::to_string));
        }

        let probe = probe_remote(&target, &opts).await.expect("probe");
        let payload = crate::server_payload::for_uname(&probe.arch).unwrap_or_else(|| {
            panic!(
                "no embedded payload for {:?}; build with a bundle-* feature (have: {:?})",
                probe.arch,
                crate::server_payload::embedded_targets()
            )
        });

        // Start from a clean slate so this really is the fresh-host path.
        let wipe = format!("rm -f \"$HOME/{REMOTE_CACHE_REL}\" \"$HOME/{REMOTE_MARKER_REL}\"");
        assert!(
            Command::new("ssh")
                .args(&opts)
                .arg(&target)
                .arg(&wipe)
                .status()
                .await
                .unwrap()
                .success()
        );
        let fresh = probe_remote(&target, &opts).await.expect("probe");
        assert_eq!(fresh.cache_version, None);
        assert_eq!(
            decide_provision(
                env!("CARGO_PKG_VERSION"),
                &fresh,
                Some((payload.target, payload.sha256))
            ),
            upload(payload.sha256),
        );

        // First connect: deploys, and resolves to what it deployed.
        let mut gate = UploadGate::default();
        let (exe, failure) = resolve_remote_exe(&target, &opts, &mut gate).await;
        assert_eq!(failure, None, "deploy reported: {failure:?}");
        assert_eq!(exe, format!("{}/{REMOTE_CACHE_REL}", fresh.home));

        // The deployed binary is real: it answers `--version` on the host with
        // our version, and it left the marker that identifies this exact build.
        let after = probe_remote(&target, &opts).await.expect("probe");
        assert_eq!(
            after.cache_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(after.cache_sha.as_deref(), Some(payload.sha256));

        // Second connect: recognises its own deploy and re-sends nothing.
        assert_eq!(
            decide_provision(
                env!("CARGO_PKG_VERSION"),
                &after,
                Some((payload.target, payload.sha256))
            ),
            Provision::UseCache,
        );
        let (exe2, failure2) = resolve_remote_exe(&target, &opts, &mut gate).await;
        assert_eq!(failure2, None);
        assert_eq!(exe2, exe);

        // And the thing we deployed actually is the daemon, not just a binary
        // that parses `--version`.
        let out = Command::new("ssh")
            .args(&opts)
            .arg(&target)
            .arg(format!("\"$HOME/{REMOTE_CACHE_REL}\" daemon status"))
            .output()
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("daemon"), "daemon status said: {text:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_open_session_errs_when_unreachable() {
        // No server on the socket → the request never gets a reply, so
        // open_session reports the host as unreachable rather than hanging.
        let remote = RemoteBackend::connect(
            Transport::LocalSocket(PathBuf::from("/nonexistent/captain-miao.sock")),
            HostId::local(),
        );
        let spec = OpenSpec {
            agent: AgentControl::Claude,
            cwd: "/work".to_string(),
            resume: None,
        };
        let backend = Backend::Remote(remote);
        assert!(tokio::task::block_in_place(|| backend.open_session(&spec)).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_open_session_returns_attach_plan() {
        let sock = std::env::temp_dir().join(format!("cm-test-open-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(mock_server(listener, vec![]));
        let backend =
            RemoteBackend::connect(Transport::LocalSocket(sock.clone()), HostId("mock".into()));

        let spec = OpenSpec {
            agent: AgentControl::Claude,
            cwd: "/work".to_string(),
            resume: None,
        };
        let plan = tokio::task::block_in_place(|| backend.open_session(&spec)).unwrap();
        match plan {
            // A socket transport (no ssh target) yields a direct attach window.
            LaunchPlan::AttachRemote { argv, session_name } => {
                assert_eq!(argv, ["miao-server", "attach", "pool-claude"]);
                assert_eq!(session_name, "pool-claude");
            }
            LaunchPlan::SpawnLocal { .. } => panic!("expected AttachRemote from a remote backend"),
        }
        let _ = std::fs::remove_file(&sock);
    }

    /// A protocol-speaking stand-in for `miao-server`: one connection,
    /// handshake, snapshot, then canned replies to requests.
    async fn mock_server(listener: UnixListener, sessions: Vec<LauncherState>) {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (rd, mut wr) = stream.into_split();
        let mut rd = BufReader::new(rd);
        let _hello: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
        write_frame(
            &mut wr,
            &ServerFrame::Welcome {
                server_version: "test".into(),
                protocol: PROTOCOL_VERSION,
                host: "mock".into(),
            },
        )
        .await
        .unwrap();
        let _sub: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
        write_frame(&mut wr, &ServerFrame::Snapshot { sessions })
            .await
            .unwrap();
        while let Ok(Some(frame)) = read_frame::<_, ClientFrame>(&mut rd).await {
            match frame {
                ClientFrame::ListResumable { req_id, .. } => write_frame(
                    &mut wr,
                    &ServerFrame::Resumable {
                        req_id,
                        candidates: vec![],
                        errors: vec![],
                    },
                )
                .await
                .unwrap(),
                ClientFrame::KillSession { req_id, .. } => {
                    write_frame(&mut wr, &ServerFrame::Killed { req_id, ok: true })
                        .await
                        .unwrap()
                }
                ClientFrame::OpenSession { req_id, spec } => {
                    // Derive the pool name from the spec so the test also
                    // confirms the spec rode the wire intact.
                    let name = format!("pool-{}", spec.agent.cli_subcommand());
                    write_frame(
                        &mut wr,
                        &ServerFrame::Opened {
                            req_id,
                            session_name: Some(name),
                            error: None,
                        },
                    )
                    .await
                    .unwrap()
                }
                ClientFrame::ListRecentDirs { req_id } => write_frame(
                    &mut wr,
                    &ServerFrame::RecentDirs {
                        req_id,
                        // Host-canonical: the wire form IS the display form,
                        // and no `$HOME` rides along (§3).
                        cwds: vec!["~/proj".into(), "~/other".into()],
                    },
                )
                .await
                .unwrap(),
                ClientFrame::CompletePath { req_id, prefix } => write_frame(
                    &mut wr,
                    // Echo the prefix back so the test confirms it rode the wire.
                    &ServerFrame::PathCompletions {
                        req_id,
                        matches: vec![format!("{prefix}alpha/"), format!("{prefix}apple/")],
                    },
                )
                .await
                .unwrap(),
                ClientFrame::CheckDir { req_id, path } => write_frame(
                    &mut wr,
                    // Only `/home/u/proj` "exists" on this mock host.
                    &ServerFrame::DirChecked {
                        req_id,
                        exists: path == "/home/u/proj",
                    },
                )
                .await
                .unwrap(),
                _ => {}
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_backend_mirrors_snapshot_and_serves_requests() {
        let sock = std::env::temp_dir().join(format!("cm-test-remote-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(mock_server(
            listener,
            vec![test_state(101), test_state(102)],
        ));

        let backend = RemoteBackend::connect(
            Transport::LocalSocket(sock.clone()),
            HostId("mock".to_string()),
        );

        // The mirror fills asynchronously once the snapshot lands.
        let mut tries = 0;
        while backend.list_sessions().len() != 2 {
            tries += 1;
            assert!(tries < 100, "mirror never filled from snapshot");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut pids: Vec<u32> = backend
            .list_sessions()
            .iter()
            .map(|s| s.launcher_pid)
            .collect();
        pids.sort();
        assert_eq!(pids, vec![101, 102]);

        // Blocking request/response must run off the async worker.
        let (cands, errs) = tokio::task::block_in_place(|| backend.list_resumable(5));
        assert!(cands.is_empty() && errs.is_empty());
        assert!(tokio::task::block_in_place(
            || backend.kill_session(&SessionKey::from_launcher_pid(999))
        ));

        let _ = std::fs::remove_file(&sock);
    }

    /// A mock that serves one snapshot per connection, dropping between them on
    /// a signal so the test can drive a disconnect deterministically. The last
    /// connection is held open (reads until EOF) so the mirror stays populated
    /// while the test asserts against it.
    async fn scripted_mock(
        listener: UnixListener,
        snapshots: Vec<Vec<LauncherState>>,
        mut drop_between: mpsc::UnboundedReceiver<()>,
    ) {
        let n = snapshots.len();
        for (i, snap) in snapshots.into_iter().enumerate() {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            let mut rd = BufReader::new(rd);
            let _hello: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
            write_frame(
                &mut wr,
                &ServerFrame::Welcome {
                    server_version: "test".into(),
                    protocol: PROTOCOL_VERSION,
                    host: "mock".into(),
                },
            )
            .await
            .unwrap();
            let _sub: Option<ClientFrame> = read_frame(&mut rd).await.unwrap();
            write_frame(&mut wr, &ServerFrame::Snapshot { sessions: snap })
                .await
                .unwrap();
            if i + 1 < n {
                // Hold this connection until the test says "drop now", then let
                // the stream fall out of scope → the client sees EOF and reconnects.
                let _ = drop_between.recv().await;
            } else {
                // Last connection: keep it open so the mirror stays filled.
                while matches!(read_frame::<_, ClientFrame>(&mut rd).await, Ok(Some(_))) {}
            }
        }
    }

    async fn wait_for_len(backend: &RemoteBackend, want: usize) {
        let mut tries = 0;
        while backend.list_sessions().len() != want {
            tries += 1;
            assert!(
                tries < 300,
                "mirror never reached {want} sessions (have {})",
                backend.list_sessions().len()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_backend_serves_host_fs_queries() {
        let sock = std::env::temp_dir().join(format!("cm-test-hostfs-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(mock_server(listener, vec![]));
        let backend =
            RemoteBackend::connect(Transport::LocalSocket(sock.clone()), HostId("mock".into()));

        // recent_dirs: the remote's list arrives host-canonical, with no home.
        let cwds = tokio::task::block_in_place(|| backend.recent_dirs());
        assert_eq!(cwds, vec!["~/proj", "~/other"]);

        // complete_path: the prefix reaches the server and matches come back.
        let matches = tokio::task::block_in_place(|| backend.complete_path("/home/u/a"));
        assert_eq!(matches, vec!["/home/u/aalpha/", "/home/u/aapple/"]);

        // dir_exists: true only for the path the mock recognizes.
        assert!(tokio::task::block_in_place(
            || backend.dir_exists("/home/u/proj")
        ));
        assert!(!tokio::task::block_in_place(
            || backend.dir_exists("/home/u/nope")
        ));

        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_backend_reconnects_and_resnapshots_after_a_drop() {
        let sock = std::env::temp_dir().join(format!("cm-test-reconn-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let (drop_tx, drop_rx) = mpsc::unbounded_channel();
        // First connection snapshots one session; after we force a drop, the
        // second connection snapshots two — proving a re-Hello/re-Subscribe and
        // a fresh Snapshot on reconnect.
        tokio::spawn(scripted_mock(
            listener,
            vec![vec![test_state(1)], vec![test_state(1), test_state(2)]],
            drop_rx,
        ));

        let backend =
            RemoteBackend::connect(Transport::LocalSocket(sock.clone()), HostId("mock".into()));

        wait_for_len(&backend, 1).await;
        assert_eq!(backend.conn_state(), ConnState::Connected);

        // Force the server to drop the connection; the client must re-dial.
        drop_tx.send(()).unwrap();

        wait_for_len(&backend, 2).await;
        assert_eq!(backend.conn_state(), ConnState::Connected);

        let _ = std::fs::remove_file(&sock);
    }
}
