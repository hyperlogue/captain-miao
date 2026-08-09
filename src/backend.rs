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

use std::collections::{HashMap, VecDeque};
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
    ///
    /// "Short" is the caller's job to enforce: a `Failed` reason quotes what the
    /// host said, which is routinely a paragraph — a NixOS box refusing a
    /// glibc-linked binary answers in four lines. The panel flattens and
    /// truncates it to its row, and the full text lives in the connection log
    /// (`l`), which exists precisely because one row cannot hold it.
    pub(crate) fn label(&self) -> &str {
        match self {
            ConnState::Connecting => "connecting",
            ConnState::Connected => "connected",
            ConnState::Disconnected => "disconnected",
            ConnState::Failed(reason) => reason,
        }
    }
}

/// One line of a host's connection narrative.
#[derive(Debug, Clone)]
pub(crate) struct ConnLogEntry {
    /// When it happened. Monotonic, and rendered as an age, so neither a clock
    /// jump nor a timezone can make the sequence read wrong.
    pub(crate) at: Instant,
    /// Whether this line is a reason the connection didn't come up — the panel
    /// colors those.
    pub(crate) error: bool,
    /// Free text, **possibly multi-line and never elided**. The whole point of
    /// this log is that it holds what the one-line row cannot.
    pub(crate) text: String,
}

/// The rolling connection narrative for one host — what `l` opens in the hosts
/// panel.
///
/// It exists because the panel gives a failure one row while the reason is
/// routinely longer than that, and because the *sequence* diagnoses where the
/// surviving sentence only reports: "probed the host, decided to deploy, the
/// deploy came back with this" tells you what to fix, where "could not deploy
/// miao-server: …" truncated at the row edge does not. Every step of
/// probe → decide → deploy → ensure → forward → handshake writes here, so a
/// failure at any of them is legible after the fact rather than only in a debug
/// log the user has to know to enable.
///
/// Capped: a host that has been flapping for a week costs a bounded amount.
#[derive(Debug, Default)]
pub(crate) struct ConnLog {
    entries: Mutex<VecDeque<ConnLogEntry>>,
}

/// How many lines one host's log keeps. Two full connect attempts' worth of
/// narrative is ~15 lines, so this holds a long flap without growing.
const CONN_LOG_CAP: usize = 200;

impl ConnLog {
    fn push(&self, error: bool, text: String) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= CONN_LOG_CAP {
            entries.pop_front();
        }
        entries.push_back(ConnLogEntry {
            at: Instant::now(),
            error,
            text,
        });
    }

    /// A step that went as expected.
    fn info(&self, text: impl Into<String>) {
        self.push(false, text.into());
    }

    /// A step that didn't — the lines the user came here to read.
    fn error(&self, text: impl Into<String>) {
        self.push(true, text.into());
    }

    /// Oldest first, which is the order the story happened in.
    pub(crate) fn entries(&self) -> Vec<ConnLogEntry> {
        self.entries.lock().unwrap().iter().cloned().collect()
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

    /// What the connection task did and what came back, for the hosts panel's
    /// `l` view. Empty for the in-process backend, which never dials anything —
    /// the panel says so rather than showing a blank box.
    pub(crate) fn conn_log(&self) -> Vec<ConnLogEntry> {
        match self {
            Backend::Local(_) => Vec::new(),
            Backend::Remote(b) => b.conn_log(),
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
    /// Everything the connection task did and what came back, for the hosts
    /// panel's `l` view. See [`ConnLog`].
    log: Arc<ConnLog>,
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
        let log = Arc::new(ConnLog::default());
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(connection_task(
            transport,
            ConnectionShared {
                host: host.clone(),
                mirror: mirror.clone(),
                remote_exe: remote_exe.clone(),
                conn: conn.clone(),
                dirty: dirty.clone(),
                server_version: server_version.clone(),
                latency: latency.clone(),
                reconnect_epoch: reconnect_epoch.clone(),
                log: log.clone(),
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
            log,
        }
    }

    /// Current connection health, for the header surface.
    fn conn_state(&self) -> ConnState {
        self.conn.lock().unwrap().clone()
    }

    /// This host's connection narrative, oldest first.
    fn conn_log(&self) -> Vec<ConnLogEntry> {
        self.log.entries()
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

/// Marker beside the deployed binary recording `<sha256> <target>` — the payload
/// we put there and which build of it won — relative to `$HOME`.
///
/// The digest exists because a version match is not identity: dev builds never
/// bump the version, so `0.2.1` on the host tells us nothing about *which*
/// `0.2.1`. The marker closes that — rebuild, reconnect, and the host gets the
/// new server — which is what makes `redeploy.sh`'s whole reason for existing go
/// away for payload-carrying builds.
///
/// The **target** is what makes the candidate loop terminate. With more than one
/// candidate per arch, the digest on the host is whichever one the host proved
/// it could run, which is generally *not* the one we would offer first: a NixOS
/// box settles on musl, the next connect compares its marker against our
/// preferred gnu payload, sees a mismatch, re-deploys gnu, watches the host
/// refuse it, falls back to musl — and does all of that again on every
/// reconnect, forever, at 500ms → 30s. Recording the winner makes it sticky, so
/// the candidate order is re-litigated only when there is nothing usable on the
/// host, never on one that has already answered the question.
///
/// Written with a single `echo`, so it stays free of quotes and backslashes
/// (see [`login_shell_safe`]); target triples are alphanumerics, dashes and
/// underscores, so they need no quoting.
const REMOTE_MARKER_REL: &str = ".cache/captain-miao/bin/miao-server.sha256";

/// The per-host provisioning state a connect attempt threads through.
///
/// Grouped rather than passed as three more positional parameters: two
/// `&mut UploadGate`s side by side are trivially swappable at a call site, and
/// swapping them would silently cross the upload cooldown with the download one.
struct Provisioning<'a> {
    upload: &'a mut UploadGate,
    download: &'a mut UploadGate,
    host: &'a HostId,
}

/// Where a published server is downloaded from, minus the tag and filename.
///
/// The URL shape is a **three-way contract** — `xtask::server::release_url`
/// builds it, `build.yml`'s asset names produce it, and this fetches it — so
/// this copy is deliberately duplicated rather than shared through `cm-core`.
/// Sharing would hand a build-chore binary tokio, notify, tracing and a C
/// compile of the SQLite amalgamation on the path every bundled build runs
/// first, and would put a `curl`/`tar` shell-out into the portable data layer
/// that rides into `miao-server` on every host. The shared surface is a URL
/// shape and two flags — no logic — and it is already a three-way contract, so
/// sharing between two of the three never made it one implementation. Each copy
/// carries its own tests instead.
const RELEASE_BASE: &str = "https://github.com/hyperlogue/captain-miao/releases/download";

/// The published asset for one target. Mirrors `xtask::server::release_url`,
/// and pinned by a test on this side too. Pure.
fn release_url(base: &str, version: &str, target: &str) -> String {
    let version = version.trim().trim_start_matches('v');
    let base = base.trim_end_matches('/');
    format!("{base}/v{version}/{SERVER_BIN}-v{version}-{target}.tar.gz")
}

/// How long the user has to answer a download prompt before the attempt gives
/// up. Bounded because the connection task is *blocked* here: a sync `Backend`
/// call from the UI thread parks in `block_in_place` waiting on this host, so an
/// unanswered popup must not wedge it indefinitely. A lapse is treated exactly
/// like a decline, and remembered the same way.
const DOWNLOAD_CONSENT_TIMEOUT: Duration = Duration::from_secs(90);

/// A pending "may I download a server?" question, on its way to the UI.
///
/// The download is the only step in this design that leaves the machine, so it
/// asks first — through the same y/N machinery `Space e` and host removal use.
pub(crate) struct DownloadPrompt {
    pub(crate) host: HostId,
    pub(crate) target: String,
    pub(crate) url: String,
    /// Answered with `true` to allow. **Dropping it means no**, which is what
    /// makes every path that doesn't explicitly allow — pressing `n`, pressing
    /// Esc, closing the dashboard — decline safely. Nothing may add a
    /// reply-on-decline: the receiver treats a closed channel as a refusal.
    pub(crate) reply: oneshot::Sender<bool>,
}

/// The dashboard's end of the consent channel, set once at startup.
///
/// A process-wide `OnceLock` rather than a parameter threaded through every
/// backend constructor, mirroring `config::get()` and `terminal::get()`. Unset —
/// in tests, and anywhere there is no TUI to ask — consent is **denied**, which
/// is the safe direction: a download that nobody could have approved must not
/// happen silently.
static DOWNLOAD_CONSENT: std::sync::OnceLock<mpsc::UnboundedSender<DownloadPrompt>> =
    std::sync::OnceLock::new();

/// Hand the dashboard's consent channel to the backends. Called once, from
/// `App::new`.
pub(crate) fn set_download_consent(tx: mpsc::UnboundedSender<DownloadPrompt>) {
    let _ = DOWNLOAD_CONSENT.set(tx);
}

/// How long a failed upload suppresses the next attempt for the same payload.
/// Without it, a host that accepts ssh but refuses the write (read-only `$HOME`,
/// full disk, no exec permission on the mount) would be re-sent multiple
/// megabytes on every reconnect — and the reconnect backoff caps at 30s.
const UPLOAD_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

/// Ceiling on one upload, so a stalled transfer can't wedge the reconnect loop
/// forever. Generous: this is multiple megabytes over whatever link the user has.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Which binary answered `daemon status` with a live daemon, if either did.
///
/// A daemon that is *already running* outranks the whole provisioning ladder
/// (§3.3): `daemon ensure` never restarts one — it is the pty pool — so a
/// payload uploaded while it holds the singleton `flock` cannot take effect
/// until it exits. Knowing *which* binary answered is what lets `UseRunning`
/// name an exe: the running daemon's own path is not otherwise observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunningDaemon {
    /// A daemon is up, and `miao-server` on PATH reported it.
    OnPath,
    /// A daemon is up, and the deployed cache-path binary reported it.
    InCache,
}

/// One-shot probe of a remote host: its `$HOME`, `uname -sm`, the version and
/// protocol of a miao-server on PATH / at the cache path (if any), the digest
/// marker we left beside the cached one (if any), and whether a daemon is
/// already running.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteProbe {
    home: String,
    arch: String,
    path_version: Option<String>,
    /// Wire protocol the PATH binary announced. `None` for one too old to print
    /// it, which is what makes the exact-version fallback necessary.
    path_protocol: Option<u32>,
    cache_version: Option<String>,
    cache_protocol: Option<u32>,
    cache_sha: Option<String>,
    /// The target triple of the deployed binary, from the marker's second field.
    /// `None` for a marker written before this existed, which falls back to the
    /// single-candidate rule.
    cache_target: Option<String>,
    /// A daemon already serving on this host, and which binary reported it.
    running: Option<RunningDaemon>,
}

/// The provisioning action a probe + local facts imply. Pure + unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provision {
    /// A daemon is **already serving** on this host, so deploy nothing and just
    /// connect. Outranks the rest of the ladder because `daemon ensure` never
    /// restarts a live daemon — it *is* the pty pool — so anything we uploaded
    /// could not take effect until that daemon exited. Whether we can actually
    /// talk to it is settled by the handshake rather than by a `--version` on
    /// disk, which describes a binary that may have replaced the running
    /// process (§3.3).
    UseRunning(RunningDaemon),
    /// A protocol-compatible binary is already on PATH; invoke `miao-server`.
    UsePath,
    /// A version-matching binary is already at the cache path; invoke it there.
    UseCache,
    /// Nothing usable is there, but we can supply a payload this host might run:
    /// push it to the cache path, then use it. Carries the target as well as the
    /// digest — the target is needed to fetch the bytes and to write the marker,
    /// and the digest is what the retry cooldown keys on.
    Upload { target: String, sha256: String },
    /// Nothing version-matching anywhere and nothing to upload; fall back to
    /// `miao-server` on PATH and let the connection fail loudly.
    FallBack,
}

/// How a [`Provision`] decision reads in the connection log. Separate from
/// `Debug` because the digest an `Upload` carries is noise to the reader —
/// what they need is which of the four it chose. Pure.
fn provision_label(action: &Provision) -> &'static str {
    match action {
        Provision::UseRunning(_) => "use the daemon already running there",
        Provision::UsePath => "use the host's own miao-server",
        Provision::UseCache => "use the one already deployed",
        Provision::Upload { .. } => "deploy ours",
        Provision::FallBack => "nothing to deploy; try PATH anyway",
    }
}

/// The shell script the probe runs over ssh. Six lines out: `$HOME`, the
/// machine, a `--version` line (or our `-` sentinel) for the PATH binary and for
/// the cache-path binary, the digest marker, and which binary — if either —
/// reports a **running daemon**. `--version` errors and "command not found"
/// both land on stderr and a non-zero exit, so `|| echo -` normalizes them.
///
/// The daemon line cannot use that same `|| echo -` trick, and the reason is
/// worth stating: `daemon status` exits **0 whether or not a daemon is
/// running** (it is a report, not a test) and prints several lines when one is,
/// so both the exit code and the line count would lie. Matching its first line
/// is the only honest read — hence `grep -q`, whose exit status *is* the
/// question, with the classification left to [`parse_probe`].
///
/// Shell-variable assignment is safe here even though `ssh` hands this to the
/// account's login shell: [`login_shell_safe`] wraps the whole thing in
/// `/bin/sh -c '…'`, so the inner dialect is always POSIX sh. The rule that
/// still binds is the wrapper's — no single quote and no backslash anywhere.
fn probe_script() -> String {
    format!(
        "echo \"$HOME\"; uname -sm; \
         {SERVER_BIN} --version 2>/dev/null || echo -; \
         \"$HOME/{REMOTE_CACHE_REL}\" --version 2>/dev/null || echo -; \
         cat \"$HOME/{REMOTE_MARKER_REL}\" 2>/dev/null || echo -; \
         d=-; \
         if {SERVER_BIN} daemon status 2>/dev/null | grep -q \"{DAEMON_RUNNING_MARK}\"; \
         then d=path; \
         elif \"$HOME/{REMOTE_CACHE_REL}\" daemon status 2>/dev/null | grep -q \"{DAEMON_RUNNING_MARK}\"; \
         then d=cache; fi; \
         echo $d"
    )
}

/// The fragment of `miao-server daemon status`'s first line that means a daemon
/// is up — it prints `daemon:   running (pid 1234)` or `daemon:   not running`
/// (`crates/cm-server/src/server.rs`). Matched on the remote by `grep -q`, so it
/// must contain no single quote or backslash (see [`login_shell_safe`]).
const DAEMON_RUNNING_MARK: &str = "running (pid";

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
    // clap prints "<name> <version> protocol <n>" (the protocol rides the same
    // line deliberately — see the server's `version_string`), so the version is
    // the second word and the protocol the fourth. A server too old to announce
    // one yields `None`, which is what the exact-version fallback keys on.
    let split = |line: Option<&str>| -> (Option<String>, Option<u32>) {
        let Some(l) = field(line) else {
            return (None, None);
        };
        let mut words = l.split_whitespace();
        let version = words.nth(1).map(str::to_string);
        // `nth` consumed through the version, so the protocol number is one
        // past the "protocol" keyword from here.
        let protocol = (words.next() == Some("protocol"))
            .then(|| words.next())
            .flatten()
            .and_then(|n| n.parse().ok());
        (version, protocol)
    };
    let (path_version, path_protocol) = split(lines.next());
    let (cache_version, cache_protocol) = split(lines.next());
    // `<sha256> <target>`; a marker from before the target was recorded has just
    // the digest, and yields `None` for the target rather than a wrong guess.
    let marker = field(lines.next());
    let cache_sha = marker
        .and_then(|m| m.split_whitespace().next())
        .map(str::to_string);
    let cache_target = marker
        .and_then(|m| m.split_whitespace().nth(1))
        .map(str::to_string);
    let running = match field(lines.next()) {
        Some("path") => Some(RunningDaemon::OnPath),
        Some("cache") => Some(RunningDaemon::InCache),
        _ => None,
    };
    Some(RemoteProbe {
        home,
        arch,
        path_version,
        path_protocol,
        cache_version,
        cache_protocol,
        cache_sha,
        cache_target,
        running,
    })
}

/// Decide which remote binary to invoke.
///
/// `candidates` is `(target, sha256)` for every server we can supply **locally**
/// for this host, in preference order (glibc before musl) — passed as plain
/// strings rather than `&ServerPayload`s so the decision stays testable in a
/// build carrying no payload, which is every test run since the `bundle-*`
/// features are off.
///
/// "Locally" is load-bearing and not a shorthand: a payload that only the
/// downloader could supply has no digest until it has been fetched, so it cannot
/// be compared against the marker and must never appear here. Resolving one that
/// way would mean downloading a binary purely to answer a comparison — and, on a
/// host already running a perfectly good server, prompting to do it. The
/// downloader is an escalation the *caller* reaches for when this returns
/// nothing usable, not a resolution step.
///
/// `suppliable` is every target we could supply *at all*, including ones the
/// host has already refused this pass. It exists purely to keep the marker's two
/// cases apart, and conflating it with `candidates` is a real bug rather than a
/// tidiness point: "the marker names a target we cannot supply" (keep what is
/// deployed — it proved itself here) and "the marker names one we just watched
/// the host reject" (keep looking) are opposite conclusions, and `candidates`
/// alone cannot tell them apart once a refusal has filtered it.
///
/// Stays pure: the IO (env lookups, cache reads, downloads) happens at the
/// resolution edge, and the looping happens at the deploy site. Unit-tested.
fn decide_provision(
    local_version: &str,
    probe: &RemoteProbe,
    candidates: &[(&str, &str)],
    suppliable: &[&str],
) -> Provision {
    // A daemon already serving outranks the whole ladder: uploading anything
    // while it holds the singleton lock is megabytes for nothing. Note this
    // does *not* check compatibility — the handshake does, because it is the
    // only authoritative answer (§3.3), and an incompatible one is a loud
    // failure rather than a fallback (§6.11).
    if let Some(which) = probe.running {
        return Provision::UseRunning(which);
    }
    // A user install always wins, and we never overwrite it.
    if path_is_usable(local_version, probe) {
        return Provision::UsePath;
    }
    // What is already deployed at *our* path. The four cases below are what make
    // the candidate loop terminate; the ordering among them matters more than
    // any one of them.
    if probe.cache_version.as_deref() == Some(local_version) {
        match probe.cache_target.as_deref() {
            // (2) and (3): the marker names which build won here. Compare
            // against *that* target, never against the one we merely prefer.
            Some(target) => match candidates.iter().find(|(t, _)| *t == target) {
                // (2) We can still supply that target: same digest means it is
                // already this exact build; a different one is the dev loop, and
                // re-deploys the *same* target rather than restarting the race.
                Some((t, sha)) => {
                    if probe.cache_sha.as_deref() == Some(*sha) {
                        return Provision::UseCache;
                    }
                    return Provision::Upload {
                        target: (*t).to_string(),
                        sha256: (*sha).to_string(),
                    };
                }
                // (3) We can no longer supply it — a released dashboard whose
                // host runs a downloaded musl, now offline or declined. Keep it:
                // it is the right version, and it is the binary that proved
                // itself here. Churning it for one we merely prefer is exactly
                // how the every-reconnect loop starts.
                //
                // But only when it is genuinely beyond us. A target missing from
                // `candidates` merely because the host *just refused it* is the
                // opposite situation: keeping the deployed copy there would
                // strand us on a binary we have watched fail and skip every
                // remaining candidate — on a no-loader host, exactly the musl
                // fallback this design exists to reach.
                None if !suppliable.contains(&target) => return Provision::UseCache,
                None => {}
            },
            // (4) A marker written before targets were recorded: fall back to
            // the single-candidate rule this had before the loop existed.
            None => match candidates.first() {
                Some((_, sha)) if probe.cache_sha.as_deref() != Some(*sha) => {}
                _ => return Provision::UseCache,
            },
        }
    }
    // (1) Nothing usable is deployed, so the loop runs: offer our first choice
    // and let the host rule on it.
    match candidates.first() {
        Some((t, sha)) => Provision::Upload {
            target: (*t).to_string(),
            sha256: (*sha).to_string(),
        },
        None => Provision::FallBack,
    }
}

/// Whether the host's own `miao-server` is one we can talk to, and so should
/// defer to rather than deploy over.
///
/// **Protocol compatibility, not version equality.** `PROTOCOL_MIN` is 4,
/// decoding above it is forward-tolerant, and v4 is documented as the last
/// refusing bump — so a 0.2.1 dashboard refusing a 0.3.0 server it could talk to
/// perfectly well was a self-inflicted deploy. Loosening this is also what makes
/// the Home Manager module sufficient on its own: a Nix host whose server came
/// from a slightly older captain-miao keeps working across dashboard upgrades,
/// with no deploy and no version lockstep — which matters because a NixOS host
/// with LDAP/SSSD users cannot be served by *any* payload we could ship.
///
/// A server too old to announce a protocol falls back to the exact-version
/// rule, since nothing else can be inferred about it. Pure.
fn path_is_usable(local_version: &str, probe: &RemoteProbe) -> bool {
    match probe.path_protocol {
        Some(p) => cm_core::protocol::protocol_compatible(p),
        None => probe.path_version.as_deref() == Some(local_version),
    }
}

/// Why a connection to an **already-running** daemon cannot proceed, phrased for
/// the hosts panel.
///
/// This is a hard failure rather than a fallback, and deliberately never an
/// automatic restart: the daemon *is* the pty pool, so stopping it kills every
/// pooled session on the host — which is why `daemon stop` itself refuses
/// without `--force`. No upload can help either, since the running daemon holds
/// the singleton `flock` until it exits. So the honest outcome is to say what is
/// wrong and hand the user the one command that fixes it (§6.11).
///
/// **Word order is load-bearing.** The hosts-panel row flattens this and
/// truncates it to the row width, so the actionable clause has to come early:
/// the mismatch first, the remedy second, and the consequence last where only
/// the connection log (`l`) is guaranteed to carry it. The natural phrasing puts
/// the remedy at the end, which is exactly where the row cuts it off.
///
/// Note the severity: this is *not* the soft indicator a stale-but-compatible
/// PATH server gets. That one is an annotation on a working host; this one means
/// the connection cannot happen at all. Pure.
fn incompatible_daemon_reason(server_version: &str, protocol: u32) -> String {
    format!(
        "host runs miao-server {server_version} protocol {protocol}, need \u{2265} {PROTOCOL_MIN} \
         — run `miao-server daemon stop` there to upgrade \
         (this kills its pooled sessions)"
    )
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
        Provision::UseCache
        | Provision::Upload { .. }
        | Provision::UseRunning(RunningDaemon::InCache) => format!("{home}/{REMOTE_CACHE_REL}"),
        Provision::UsePath | Provision::FallBack | Provision::UseRunning(RunningDaemon::OnPath) => {
            "miao-server".to_string()
        }
    }
}

/// Remembers a failed upload so the next reconnect doesn't repeat it. Keyed on
/// the payload digest, so building a new server *does* get a fresh attempt
/// immediately — only re-sending the same bytes to the same host is suppressed.
/// Pure over an injected `now`, so the cooldown is unit-tested without sleeping.
#[derive(Default)]
struct UploadGate {
    /// digest → (when it failed, what the host said). **A map, not a single
    /// slot**, and that is the whole point: with more than one candidate per
    /// host, one remembered failure is evicted by the next. A NixOS box with
    /// LDAP/SSSD users refuses *both* payloads — gnu has no loader, musl fails
    /// the self-check — so a single slot would remember only musl, leave gnu
    /// unsuppressed on the next pass, and re-send both, forever, at a backoff
    /// that caps at 30s. Remembering each independently is what makes the wasted
    /// transfer once per host rather than once per reconnect.
    failed: HashMap<String, (Instant, String)>,
}

impl UploadGate {
    /// The remembered error, if uploading `sha` is still on cooldown.
    fn suppressed(&self, sha: &str, now: Instant) -> Option<&str> {
        let (at, error) = self.failed.get(sha)?;
        (now.duration_since(*at) < UPLOAD_RETRY_COOLDOWN).then_some(error.as_str())
    }

    fn record_failure(&mut self, sha: &str, now: Instant, error: String) {
        self.failed.insert(sha.to_string(), (now, error));
    }

    /// Forget every remembered failure — called once a connection actually
    /// works, so a transient problem doesn't hold the cooldown past its
    /// usefulness.
    fn clear(&mut self) {
        self.failed.clear();
    }
}

/// The script the remote runs while we stream the binary into its stdin.
///
/// Staged through a temp file and moved into place only after the host itself
/// has run it: a truncated transfer or a payload for the wrong ABI fails the
/// `self-check` line, `set -e` aborts, and nothing was ever visible at the path
/// the next connect will invoke. That check is also what covers the one thing
/// `uname` can't tell us, glibc vs musl.
///
/// **Why `self-check` and not `--version`.** `--version` proves the file loads
/// and matches; it never resolves user information, so a static-musl server on
/// a host whose users come from LDAP/SSSD passes it, installs, and then fails on
/// *first attach* — the pool resolves the user with `getpwuid_r` and errors when
/// NSS has nothing to answer with. `self-check` makes the host answer the
/// question that actually matters: can this binary host a session here? It
/// prints the same `miao-server <ver> …` shape, so the reply is parsed exactly
/// as before. (One consequence worth knowing: a binary predating `self-check` —
/// one handed over via the env vars, or fetched from an older release — fails
/// this as a clap usage error rather than a version mismatch. Acceptable; the
/// deploy refuses either way, which is the safe direction.)
///
/// Two constraints shape how it's written, both from [`login_shell_safe`]: no
/// single quote and no backslash anywhere in it. Hence `echo` for the marker
/// rather than `printf '%s\n'`, and hence clearing the temp file at the *start*
/// of the run rather than with an `EXIT` trap — a failed deploy leaves its temp
/// behind, which costs some cache-directory space until the next attempt and
/// buys a script that runs everywhere. Pure, so all of this is unit-tested.
fn upload_script(sha256: &str, target: &str) -> String {
    format!(
        "set -e; \
         t=\"$HOME/{REMOTE_INCOMING_REL}\"; \
         mkdir -p \"$HOME/{REMOTE_BIN_DIR_REL}\"; \
         rm -f \"$t\"; \
         cat > \"$t\"; \
         chmod 0755 \"$t\"; \
         \"$t\" self-check; \
         mv -f \"$t\" \"$HOME/{REMOTE_CACHE_REL}\"; \
         echo {sha256} {target} > \"$HOME/{REMOTE_MARKER_REL}\""
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
    payload: &crate::server_payload::Candidate,
) -> Result<(), String> {
    let bytes = payload
        .bytes()
        .map_err(|e| format!("reading the {} payload: {e}", payload.target))?;
    // A payload the *user* pointed us at is checked before it leaves: a
    // store-linked binary filed under a generic triple looks entirely correct
    // and fails on every host but the one that built it, and finding that out
    // after a multi-megabyte upload is a bad trade for one header read.
    if payload.is_locally_sourced() {
        crate::server_payload::check_interpreter(&bytes, &payload.target)?;
    }
    let len = bytes.len();
    tracing::info!(
        target: "captain_miao::provision",
        "{target}: deploying {} server from {} ({len} bytes) to ~/{REMOTE_CACHE_REL}",
        payload.target,
        payload.source.label()
    );

    let mut child = Command::new("ssh")
        .args(opts)
        .arg(target)
        .arg(login_shell_safe(&upload_script(
            &payload.sha256,
            &payload.target,
        )))
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

/// Ask the user whether we may fetch a server, and wait for the answer.
///
/// Returns `false` on a decline, on a lapse, and whenever there is no UI to ask
/// — every ambiguous outcome refuses, because this is the one step that leaves
/// the machine.
async fn ask_download_consent(host: &HostId, target: &str, url: &str) -> bool {
    let Some(tx) = DOWNLOAD_CONSENT.get() else {
        return false;
    };
    let (reply, rx) = oneshot::channel();
    if tx
        .send(DownloadPrompt {
            host: host.clone(),
            target: target.to_string(),
            url: url.to_string(),
            reply,
        })
        .is_err()
    {
        return false;
    }
    // A closed channel is a decline: the UI drops the sender on `n`, on Esc, and
    // on quit, so refusal needs no message of its own.
    matches!(
        tokio::time::timeout(DOWNLOAD_CONSENT_TIMEOUT, rx).await,
        Ok(Ok(true))
    )
}

/// Fetch a published server into the XDG cache, and return where it landed.
///
/// Two guards travel with the download, both mirroring `xtask`'s copy and both
/// tested here independently:
///
/// - `--proto =https` is re-asserted rather than trusted from the URL, because
///   `--location` is on and GitHub bounces release downloads to S3 — the scheme
///   has to hold on *every* hop, not just the first.
/// - the archive member is extracted **by name**, so a `../` entry has nothing
///   to land on, and the result is rejected unless it is a regular file: `tar`
///   will happily extract an entry recorded as a symlink, and reading through
///   one would pull in a file from outside the staging directory.
async fn download_server(target: &str, url: &str) -> Result<std::path::PathBuf, String> {
    let dest = crate::server_payload::cache_path_for(target)
        .ok_or_else(|| "no cache directory available".to_string())?;
    let dir = dest
        .parent()
        .ok_or_else(|| "bad cache path".to_string())?
        .to_path_buf();
    // A fresh directory per fetch: `tar` extracts over whatever is there, so a
    // failed download followed by an extract of a *previous* archive would
    // silently install a stale binary.
    // std, not tokio::fs: these are metadata-sized local operations, and the
    // dashboard's tokio deliberately does not enable the `fs` feature.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let tgz = dir.join("server.tar.gz");

    let out = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--output",
        ])
        .arg(&tgz)
        .arg(url)
        .output()
        .await
        .map_err(|e| format!("spawning curl: {e}"))?;
    if !out.status.success() {
        let err: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(200)
            .collect();
        return Err(if err.is_empty() {
            format!("download failed (rc={:?})", out.status.code())
        } else {
            err
        });
    }

    let out = Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&dir)
        .args(["--no-same-owner", "--no-same-permissions", SERVER_BIN])
        .output()
        .await
        .map_err(|e| format!("spawning tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "the archive did not contain {SERVER_BIN}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = std::fs::remove_file(&tgz);

    let meta = std::fs::symlink_metadata(&dest)
        .map_err(|e| format!("{url} did not yield {SERVER_BIN}: {e}"))?;
    if !meta.is_file() {
        let _ = std::fs::remove_file(&dest);
        return Err(format!(
            "{url} did not yield a regular file at {SERVER_BIN}"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }
    Ok(dest)
}

/// Source (5): ask, fetch, and cache a published server for the first target we
/// can't supply locally. `None` when there is nothing left to try.
///
/// Rate-limited by the same gate shape the upload uses, and for the same reason:
/// the reconnect backoff caps at 30s, so a 404 for an arch we never published —
/// or a user who said no — would otherwise be re-attempted twice a minute
/// forever. A **decline is remembered exactly like a failure**, which is the one
/// refinement "confirm every time" needs: without it the popup returns every 500
/// ms → 30 s and a declined host becomes unusable. A successful connection
/// clears the memory, so saying no is not permanent.
async fn try_download_candidate(
    arch: &str,
    available: &[crate::server_payload::Candidate],
    refused: &[String],
    gate: &mut UploadGate,
    host: &HostId,
    log: &ConnLog,
) -> Option<crate::server_payload::Candidate> {
    let version = env!("CARGO_PKG_VERSION");
    let wanted = crate::server_payload::target_candidates(arch)
        .iter()
        .find(|t| !refused.iter().any(|r| r == *t) && !available.iter().any(|c| c.target == **t))?;
    let url = release_url(RELEASE_BASE, version, wanted);
    let now = Instant::now();
    if let Some(previous) = gate.suppressed(&url, now) {
        log.info(format!("not fetching {wanted} again yet: {previous}"));
        return None;
    }
    log.info(format!("nothing local for {wanted}; asking to download it"));
    if !ask_download_consent(host, wanted, &url).await {
        log.info(format!("download of {wanted} declined"));
        gate.record_failure(&url, now, "you declined the download".to_string());
        return None;
    }
    log.info(format!("downloading {url}"));
    match download_server(wanted, &url).await {
        Ok(path) => {
            log.info(format!("cached {}", path.display()));
            // Re-resolve rather than hand-building a candidate: the download
            // wrote into cache source (4), so the chain now finds it with a real
            // digest, computed from the bytes that actually landed.
            crate::server_payload::resolve_candidates(arch)
                .into_iter()
                .find(|c| &c.target == wanted)
        }
        Err(e) => {
            log.error(format!("downloading {wanted} failed: {e}"));
            gate.record_failure(&url, now, e);
            None
        }
    }
}

/// Resolve the remote command to invoke: probe → decide → (deploy) → invoke.
/// Never errors — any failure resolves to `miao-server` on PATH so the
/// rest of `setup_ssh` behaves exactly as it did before provisioning existed.
/// The second half of the pair is the *diagnosis*: a `Some(reason)` names what's
/// wrong with the remote install, for `ConnState::Failed` to carry (§4).
async fn resolve_remote_exe(
    target: &str,
    opts: &[String],
    prov: &mut Provisioning<'_>,
    log: &ConnLog,
) -> (String, Option<String>) {
    let host = prov.host;
    let Some(probe) = probe_remote(target, opts).await else {
        tracing::debug!(
            target: "captain_miao::provision",
            "{target}: probe failed (unreachable / no shell) → PATH miao-server"
        );
        log.error("probe failed — the host is unreachable over ssh, or has no shell");
        return (
            "miao-server".to_string(),
            Some("host unreachable over ssh (or no shell)".to_string()),
        );
    };
    let local_version = env!("CARGO_PKG_VERSION");
    // The source chain: env vars, then what this build carries, then anything
    // downloaded earlier. Naming the source in the log matters — "deployed the
    // gnu server" is a different fact from "deployed the one you pointed
    // CAPTAIN_MIAO_SERVER_DIR at", and only one of them is our bug.
    let mut available = crate::server_payload::resolve_candidates(&probe.arch);
    log.info(format!(
        "probed {}: PATH {}, cache {}, need {local_version}; can supply {}",
        probe.arch,
        probe.path_version.as_deref().unwrap_or("none"),
        probe.cache_version.as_deref().unwrap_or("none"),
        if available.is_empty() {
            "nothing".to_string()
        } else {
            available
                .iter()
                .map(|p| format!("{} ({})", p.target, p.source.label()))
                .collect::<Vec<_>>()
                .join(", ")
        },
    ));

    // **The candidate loop.** `uname` cannot report a libc and neither can
    // anything else we can ask cheaply, so selection is verified rather than
    // guessed: offer a payload, let the host's own `self-check` rule on it, and
    // on a refusal drop that candidate and ask the decision again. What the host
    // accepts is recorded in the marker, so this race is run once per host and
    // not once per connect.
    //
    // A failure is reported verbatim rather than retried here — the reconnect
    // loop is the retry mechanism, and `gate` is what stops it re-sending
    // megabytes every pass.
    let mut refused: Vec<String> = Vec::new();
    let mut upload_error = None;
    let action = loop {
        let candidates: Vec<(&str, &str)> = available
            .iter()
            .filter(|p| !refused.contains(&p.target))
            .map(|p| (p.target.as_str(), p.sha256.as_str()))
            .collect();
        // `suppliable` is deliberately unfiltered: it separates "cannot supply"
        // from "just refused", which the marker's cases (3) and the loop below
        // read in opposite directions.
        let suppliable: Vec<&str> = available.iter().map(|p| p.target.as_str()).collect();
        let action = decide_provision(local_version, &probe, &candidates, &suppliable);
        tracing::debug!(
            target: "captain_miao::provision",
            "{target}: arch={:?} path={:?} cache={:?}/{:?} refused={refused:?} → {action:?}",
            probe.arch, probe.path_version, probe.cache_version, probe.cache_target
        );

        let Provision::Upload { target: t, sha256 } = &action else {
            // Nothing local left to offer. Before giving up, see whether a
            // *published* server exists for a target we carry nothing for —
            // this is source (5), and it is what lets a gnu-only released
            // dashboard reach a host with no generic loader at all.
            if matches!(action, Provision::FallBack)
                && let Some(fetched) = try_download_candidate(
                    &probe.arch,
                    &available,
                    &refused,
                    prov.download,
                    host,
                    log,
                )
                .await
            {
                available.push(fetched);
                continue;
            }
            log.info(format!("\u{2192} {}", provision_label(&action)));
            break action;
        };
        let payload = available
            .iter()
            .find(|p| &p.target == t)
            .expect("Upload names a candidate we offered");

        let now = Instant::now();
        let failure = match prov.upload.suppressed(sha256, now) {
            Some(previous) => {
                // Worth saying out loud: nothing was sent this time, so the
                // error below is a *remembered* one, not a fresh symptom.
                log.info(format!(
                    "deploy of {t} suppressed — the same payload failed recently"
                ));
                Some(previous.to_string())
            }
            None => {
                log.info(format!("deploying {t} from {}", payload.source.label()));
                match upload_server(target, opts, payload).await {
                    Ok(()) => None,
                    Err(e) => {
                        tracing::warn!(target: "captain_miao::provision", "{target}: deploy of {t} failed: {e}");
                        prov.upload.record_failure(sha256, now, e.clone());
                        Some(e)
                    }
                }
            }
        };
        match failure {
            None => {
                log.info(format!("deployed {t}, and the host ran it"));
                break Provision::UseCache;
            }
            Some(e) => {
                log.error(format!("{t} refused by the host:\n{e}"));
                // Keep the *first* refusal as the reported reason: it is the
                // candidate we preferred, so it is the one whose failure
                // explains the outcome. Later ones are fallbacks.
                upload_error.get_or_insert(e);
                refused.push(payload.target.clone());
            }
        }
    };

    let exe = remote_exe_for(&action, &probe.home);
    tracing::debug!(target: "captain_miao::provision", "{target}: remote exe = {exe}");
    log.info(format!("will invoke `{exe}` on the host"));
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
    /// Which host this task serves. Needed so a download prompt can name it —
    /// the user may have several, and "may I download a server?" is not a
    /// question worth asking without saying for whom.
    host: HostId,
    mirror: Arc<Mutex<HashMap<SessionKey, LauncherState>>>,
    remote_exe: Arc<Mutex<String>>,
    conn: Arc<Mutex<ConnState>>,
    dirty: Arc<AtomicBool>,
    server_version: Arc<Mutex<Option<String>>>,
    latency: Arc<Mutex<Option<Duration>>>,
    reconnect_epoch: Arc<AtomicU64>,
    log: Arc<ConnLog>,
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
        host,
        mirror,
        remote_exe,
        conn,
        dirty,
        server_version,
        latency,
        reconnect_epoch,
        log,
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
    // A separate gate for downloads, keyed by URL rather than digest — a
    // payload we have not fetched has no digest yet. Same shape and the same
    // reason: a decline or a 404 must not be re-attempted every backoff tick.
    let mut download_gate = UploadGate::default();
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
            Transport::LocalSocket(p) => {
                log.info(format!("connecting to local socket {}", p.display()));
                Some((p.clone(), None))
            }
            Transport::Ssh { target, local_sock } => {
                log.info(format!("connecting to {target} over ssh"));
                match setup_ssh(
                    target,
                    local_sock,
                    &remote_exe,
                    &mut failure,
                    &mut Provisioning {
                        upload: &mut upload_gate,
                        download: &mut download_gate,
                        host: &host,
                    },
                    &log,
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
            match &standing_failure {
                Some(reason) => log.error(format!("could not set the connection up: {reason}")),
                None => log.error("could not set the connection up (no diagnosis)"),
            }
            store(match &standing_failure {
                Some(reason) => ConnState::Failed(reason.clone()),
                None => ConnState::Disconnected,
            });
            log.info(format!("retrying in {}s", backoff.as_secs_f32().round()));
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
            log.error(format!(
                "the daemon socket never answered at {} ({attempts} attempts)",
                sock_path.display()
            ));
            store(ConnState::Disconnected);
            log.info(format!("retrying in {}s", backoff.as_secs_f32().round()));
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
        download_gate.clear();
        log.info("connected");
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
        match (&standing_failure, &outcome) {
            (Some(reason), _) => log.error(format!("handshake refused: {reason}")),
            (None, ServeOutcome::BackendDropped) => log.info("host removed; stopping"),
            (None, _) => log.error(format!(
                "connection lost after {}s",
                connected_at.elapsed().as_secs()
            )),
        }
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
    prov: &mut Provisioning<'_>,
    log: &ConnLog,
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
    let (exe, diagnosis) = resolve_remote_exe(target, &opts, prov, log).await;
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
        // The log gets it whole and unelided — this is the sentence the panel
        // row has to cut, and reading all of it is the entire reason `l` exists.
        log.error(format!(
            "`{exe} daemon ensure` failed (rc={:?}):\n{}",
            out.status.code(),
            stderr.trim()
        ));
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
        log.error("`daemon ensure` succeeded but printed no socket path");
        return None;
    }
    log.info(format!("daemon is up, socket {remote_sock}"));
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
                return ServeOutcome::HandshakeFailed(Some(incompatible_daemon_reason(
                    &sv, protocol,
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

    /// A probe of a host with no running daemon and no protocol announced by
    /// either binary — i.e. servers old enough to fall back to the
    /// exact-version rule, which is what most of these tests are about.
    fn probe(arch: &str, path: Option<&str>, cache: Option<&str>) -> RemoteProbe {
        RemoteProbe {
            home: "/home/u".into(),
            arch: arch.into(),
            path_version: path.map(str::to_string),
            path_protocol: None,
            cache_version: cache.map(str::to_string),
            cache_protocol: None,
            cache_sha: None,
            cache_target: None,
            running: None,
        }
    }

    /// The payload a test dashboard carries: `(target, sha256)`, exactly the
    /// shape `decide_provision` takes.
    const PAYLOAD: (&str, &str) = ("x86_64-unknown-linux-gnu", "abc123");

    fn upload(sha: &str) -> Provision {
        Provision::Upload {
            target: PAYLOAD.0.to_string(),
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
    fn parse_probe_reads_the_protocol_off_the_version_line() {
        // The server folds its protocol onto the same line as the version
        // precisely so this parse stays one-field-per-line.
        let out = "/home/u\nLinux x86_64\nmiao-server 0.3.0 protocol 4\n-\n-\n-\n";
        let p = parse_probe(out).unwrap();
        assert_eq!(p.path_version.as_deref(), Some("0.3.0"));
        assert_eq!(p.path_protocol, Some(4));
        assert_eq!(p.cache_protocol, None);
        assert_eq!(p.running, None);

        // A server too old to announce one still yields its version, which is
        // what the exact-version fallback needs.
        let old = parse_probe("/home/u\nLinux x86_64\nmiao-server 0.2.1\n-\n-\n-\n").unwrap();
        assert_eq!(old.path_version.as_deref(), Some("0.2.1"));
        assert_eq!(old.path_protocol, None);

        // Garbage where the number should be is "didn't announce one", never a
        // definite answer we'd then compare against the floor.
        let junk = parse_probe("/home/u\nLinux x86_64\nmiao-server 0.3.0 protocol wat\n-\n-\n-\n")
            .unwrap();
        assert_eq!(junk.path_protocol, None);
    }

    #[test]
    fn parse_probe_reads_the_winning_target_beside_the_digest() {
        let p = parse_probe(
            "/home/u\nLinux x86_64\n-\nmiao-server 0.2.0\ndead x86_64-unknown-linux-musl\n-\n",
        )
        .unwrap();
        assert_eq!(p.cache_sha.as_deref(), Some("dead"));
        assert_eq!(p.cache_target.as_deref(), Some("x86_64-unknown-linux-musl"));

        // A marker from a build that recorded only the digest yields no target
        // rather than a guess — which is what case (4) of the sticky rule keys
        // on, and why an upgrade doesn't churn every already-deployed host.
        let old =
            parse_probe("/home/u\nLinux x86_64\n-\nmiao-server 0.2.0\ndeadbeef\n-\n").unwrap();
        assert_eq!(old.cache_sha.as_deref(), Some("deadbeef"));
        assert_eq!(old.cache_target, None);
    }

    #[test]
    fn parse_probe_reads_which_binary_reported_a_running_daemon() {
        let mk = |d: &str| {
            parse_probe(&format!("/home/u\nLinux x86_64\n-\n-\n-\n{d}\n"))
                .unwrap()
                .running
        };
        assert_eq!(mk("path"), Some(RunningDaemon::OnPath));
        assert_eq!(mk("cache"), Some(RunningDaemon::InCache));
        assert_eq!(mk("-"), None);
        // A probe from before this field existed simply has no sixth line.
        assert_eq!(
            parse_probe("/home/u\nLinux x86_64\n-\n-\n-\n")
                .unwrap()
                .running,
            None
        );
    }

    #[test]
    fn a_running_daemon_outranks_everything_and_deploys_nothing() {
        // Uploading while a live daemon holds the singleton lock is megabytes
        // for nothing: it cannot take effect until that daemon exits, and we
        // never stop one (it *is* the pty pool).
        let mut p = probe("Linux x86_64", None, None);
        p.running = Some(RunningDaemon::OnPath);
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UseRunning(RunningDaemon::OnPath)
        );
        assert_eq!(
            remote_exe_for(&Provision::UseRunning(RunningDaemon::OnPath), "/home/u"),
            "miao-server"
        );

        // Which binary answered is what lets us name an exe — the running
        // daemon's own path is not otherwise observable.
        p.running = Some(RunningDaemon::InCache);
        assert_eq!(
            remote_exe_for(
                &decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
                "/home/u"
            ),
            format!("/home/u/{REMOTE_CACHE_REL}")
        );
    }

    #[test]
    fn a_path_server_wins_on_protocol_compatibility_not_version_equality() {
        use cm_core::protocol::{PROTOCOL_MIN, PROTOCOL_VERSION};

        // The self-inflicted deploy this removes: a different version we can
        // still talk to perfectly well used to be refused and overwritten.
        let mut p = probe("Linux x86_64", Some("0.9.9"), None);
        p.path_protocol = Some(PROTOCOL_VERSION);
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UsePath
        );

        // Below the floor is not talkable, so it does not win — the ladder
        // falls through to our payload.
        p.path_protocol = Some(PROTOCOL_MIN - 1);
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );

        // A server too old to announce a protocol keeps the old exact-version
        // rule: nothing else can be inferred about it.
        p.path_protocol = None;
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );
        let matching = probe("Linux x86_64", Some("0.1.0"), None);
        assert_eq!(
            decide_provision("0.1.0", &matching, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UsePath
        );
    }

    #[test]
    fn an_incompatible_running_daemon_leads_with_the_remedy_not_the_consequence() {
        let msg = incompatible_daemon_reason("0.1.0", 3);
        // The row flattens and truncates this, so what must survive the cut is
        // the mismatch and the command — the consequence can fall off the end.
        let stop = msg.find("daemon stop").expect("names the remedy");
        let kills = msg.find("kills").expect("states the consequence");
        assert!(msg.find("protocol 3").unwrap() < stop, "{msg}");
        assert!(stop < kills, "{msg}");
        assert!(msg.contains(&PROTOCOL_MIN.to_string()), "{msg}");
        // Never advertised as something we'd do for them: it would kill live
        // pooled sessions, which is why `daemon stop` itself refuses unforced.
        assert!(!msg.contains("automatic"), "{msg}");
    }

    #[test]
    fn decide_prefers_path_install_over_cache() {
        let lx = "Linux x86_64";
        // PATH match wins outright — a user install beats our cache copy, and is
        // never overwritten even when we carry a payload.
        let p = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::UsePath
        );
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UsePath
        );
        // No PATH match, but our cache copy matches → use it.
        let p = probe(lx, None, Some("0.1.0"));
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::UseCache
        );
    }

    #[test]
    fn decide_falls_back_when_nothing_matches_and_we_carry_nothing() {
        let lx = "Linux x86_64";
        // Nothing deployed anywhere.
        assert_eq!(
            decide_provision("0.1.0", &probe(lx, None, None), &[], &[]),
            Provision::FallBack
        );
        // Both present but stale — a version mismatch must not be invoked, since
        // the wire protocol isn't guaranteed compatible across versions.
        let stale = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(
            decide_provision("0.2.0", &stale, &[], &[]),
            Provision::FallBack
        );
    }

    #[test]
    fn a_payload_turns_every_fallback_into_a_deploy() {
        let lx = "Linux x86_64";
        // Nothing there at all — the fresh-host case.
        assert_eq!(
            decide_provision("0.1.0", &probe(lx, None, None), &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );
        // Everything there but stale.
        let stale = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(
            decide_provision("0.2.0", &stale, &[PAYLOAD], &[PAYLOAD.0]),
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
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            Provision::UseCache
        );

        // A different build of the same version — re-deploy.
        p.cache_sha = Some("999999".into());
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );

        // No marker at all (redeploy.sh, or a pre-marker dashboard). We own this
        // path, so we take it over rather than trusting an unlabelled binary.
        p.cache_sha = None;
        assert_eq!(
            decide_provision("0.1.0", &p, &[PAYLOAD], &[PAYLOAD.0]),
            upload("abc123")
        );
        // …but a build carrying no payload has nothing better to offer, so it
        // keeps using what's there.
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::UseCache
        );
    }

    const GNU: (&str, &str) = ("x86_64-unknown-linux-gnu", "gnu-sha");
    const MUSL: (&str, &str) = ("x86_64-unknown-linux-musl", "musl-sha");
    const BOTH_TARGETS: &[&str] = &[GNU.0, MUSL.0];

    fn upload_of(p: (&str, &str)) -> Provision {
        Provision::Upload {
            target: p.0.to_string(),
            sha256: p.1.to_string(),
        }
    }

    #[test]
    fn the_marker_makes_the_winning_target_sticky() {
        // The failure this prevents: a NixOS host settles on musl; the next
        // connect compares its marker against our *preferred* gnu payload, sees
        // a mismatch, re-deploys gnu, watches the host refuse it, falls back to
        // musl — and does the whole thing again every reconnect, forever.
        let both = [GNU, MUSL];
        let mut p = probe("Linux x86_64", None, Some("0.1.0"));

        // (2) The marker names musl and we can still supply it; same digest, so
        // it is already this exact build — keep it, even though gnu is what we
        // would otherwise offer first.
        p.cache_sha = Some(MUSL.1.into());
        p.cache_target = Some(MUSL.0.into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            Provision::UseCache
        );

        // (2) Same target, different digest: the dev loop. Re-deploy *that*
        // target rather than restarting the race from the top.
        p.cache_sha = Some("some-older-build".into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            upload_of(MUSL)
        );

        // (3) The marker names a target we can no longer supply — a released
        // dashboard whose host runs a downloaded musl, now offline or declined.
        // Keep what proved itself here; re-offering gnu is how the loop starts.
        assert_eq!(
            decide_provision("0.1.0", &p, &[GNU], &[GNU.0]),
            Provision::UseCache
        );

        // (4) A marker written before targets were recorded falls back to the
        // single-candidate rule this had before the loop existed.
        p.cache_target = None;
        p.cache_sha = Some(GNU.1.into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            Provision::UseCache
        );
        p.cache_sha = Some("a-different-build".into());
        assert_eq!(
            decide_provision("0.1.0", &p, &both, BOTH_TARGETS),
            upload_of(GNU)
        );

        // (1) A version mismatch runs the loop regardless of what the marker
        // says: stickiness is about *which* build, not about keeping a stale one.
        let mut old = probe("Linux x86_64", None, Some("0.0.9"));
        old.cache_sha = Some(MUSL.1.into());
        old.cache_target = Some(MUSL.0.into());
        assert_eq!(
            decide_provision("0.1.0", &old, &both, BOTH_TARGETS),
            upload_of(GNU)
        );
    }

    #[test]
    fn a_refusal_does_not_let_the_marker_strand_us_on_a_dead_binary() {
        // Regression. The marker's "we can no longer supply that target" case
        // must not fire for a target that is missing from the candidate list
        // only because the host *just refused it*. Those are opposite
        // situations, and `candidates` alone cannot tell them apart once a
        // refusal has filtered it.
        //
        // The scenario: a no-loader host with a same-version gnu binary already
        // deployed from an earlier build. We offer our gnu payload (the digest
        // differs), the host cannot run it, and gnu drops out of the running.
        // If we then read the marker as "gnu is beyond us", we keep the gnu
        // binary sitting there — which this host equally cannot run — and never
        // try musl, the one payload that would have worked.
        let mut p = probe("Linux x86_64", None, Some("0.1.0"));
        p.cache_sha = Some("an-earlier-gnu-build".into());
        p.cache_target = Some(GNU.0.into());

        // gnu refused, so only musl remains offerable — but both are suppliable.
        assert_eq!(
            decide_provision("0.1.0", &p, &[MUSL], BOTH_TARGETS),
            upload_of(MUSL),
            "a refused marker target must not short-circuit to UseCache"
        );

        // The genuine case (3) still holds: when the marker names something we
        // truly cannot supply, keep what proved itself there.
        assert_eq!(
            decide_provision("0.1.0", &p, &[MUSL], &[MUSL.0]),
            Provision::UseCache
        );
    }

    #[test]
    fn a_refused_candidate_lets_the_next_one_be_offered() {
        // What the deploy site does after the host's self-check refuses a
        // payload: drop that candidate and ask again. Preference order is only
        // a starting point — the host has the last word, since `uname` cannot
        // report a libc and nothing else can be asked cheaply.
        let p = probe("Linux x86_64", None, None);
        assert_eq!(
            decide_provision("0.1.0", &p, &[GNU, MUSL], BOTH_TARGETS),
            upload_of(GNU)
        );
        assert_eq!(
            decide_provision("0.1.0", &p, &[MUSL], BOTH_TARGETS),
            upload_of(MUSL)
        );
        // Both refused — the NixOS-with-LDAP row. No payload we could ship
        // serves it, so say so rather than install something that breaks later.
        assert_eq!(
            decide_provision("0.1.0", &p, &[], BOTH_TARGETS),
            Provision::FallBack
        );
    }

    #[test]
    fn the_release_url_matches_what_the_workflow_publishes() {
        // This copy is duplicated from xtask by decision, so it carries the
        // contract test too — the URL is already a three-way agreement between
        // `release_url`, this fetcher, and build.yml's asset names, and no
        // amount of code sharing between two of them would make it one.
        assert_eq!(
            release_url(RELEASE_BASE, "0.2.1", "aarch64-unknown-linux-musl"),
            "https://github.com/hyperlogue/captain-miao/releases/download/v0.2.1/\
             miao-server-v0.2.1-aarch64-unknown-linux-musl.tar.gz"
        );
        // Either spelling of the version resolves the same, so a `v` prefix
        // picked up from a tag can't produce a second, wrong URL.
        assert_eq!(
            release_url(RELEASE_BASE, "v0.2.1", "x86_64-unknown-linux-gnu"),
            release_url(RELEASE_BASE, "0.2.1", "x86_64-unknown-linux-gnu")
        );
        // A base with a trailing slash is the same base.
        assert_eq!(
            release_url("https://mirror.example/dl/", "0.2.1", "t"),
            release_url("https://mirror.example/dl", "0.2.1", "t")
        );
    }

    /// The download's extraction guards, exercised against archives built to
    /// abuse them. `tar` is the thing under test here, so these run it for real
    /// rather than asserting on the argv.
    #[test]
    fn a_hostile_archive_cannot_write_outside_the_cache_dir() {
        let root = scratch_home("tar");
        let stage = root.join("stage");
        std::fs::create_dir_all(&stage).unwrap();

        let tar_ok = std::process::Command::new("tar")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if !tar_ok {
            return; // no tar here; the deploy tests cover the rest
        }

        // An archive whose only member escapes the extraction directory. We ask
        // for `miao-server` **by name**, so there is nothing for it to land on.
        let evil = root.join("evil");
        std::fs::create_dir_all(evil.join("sub")).unwrap();
        std::fs::write(evil.join("sub/miao-server"), b"payload").unwrap();
        let tgz = root.join("evil.tar.gz");
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&evil)
            .arg("--transform=s|sub/miao-server|../escaped|")
            .arg("sub/miao-server")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let out = std::process::Command::new("tar")
                .arg("-xzf")
                .arg(&tgz)
                .arg("-C")
                .arg(&stage)
                .args(["--no-same-owner", "--no-same-permissions", SERVER_BIN])
                .output()
                .unwrap();
            // Naming the member is the guard: the escaping entry isn't it, so
            // the extraction finds nothing and nothing is written anywhere.
            assert!(!out.status.success() || !stage.join(SERVER_BIN).exists());
            assert!(!root.join("escaped").exists(), "escaped the staging dir");
        }

        // An archive whose `miao-server` is a *symlink*: tar extracts the link
        // happily, and reading through it would pull in a file we never fetched.
        let link_src = root.join("linky");
        std::fs::create_dir_all(&link_src).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", link_src.join(SERVER_BIN)).unwrap();
        let tgz2 = root.join("link.tar.gz");
        let ok2 = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz2)
            .arg("-C")
            .arg(&link_src)
            .arg(SERVER_BIN)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok2 {
            let stage2 = root.join("stage2");
            std::fs::create_dir_all(&stage2).unwrap();
            let _ = std::process::Command::new("tar")
                .arg("-xzf")
                .arg(&tgz2)
                .arg("-C")
                .arg(&stage2)
                .args(["--no-same-owner", "--no-same-permissions", SERVER_BIN])
                .output()
                .unwrap();
            let landed = stage2.join(SERVER_BIN);
            // This is exactly what `download_server` refuses on: the check is
            // `symlink_metadata(...).is_file()`, so a link never passes.
            if landed.exists() || landed.is_symlink() {
                let meta = std::fs::symlink_metadata(&landed).unwrap();
                assert!(
                    !meta.is_file(),
                    "a symlink wearing the member name must not read as a regular file"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_gate_remembers_each_payload_independently() {
        // A single remembered failure would be evicted by the next candidate's,
        // leaving the first unsuppressed on the following pass — so a host that
        // refuses *both* payloads gets both re-sent every reconnect, at a
        // backoff that caps at 30s. That is the loop this gate exists to stop.
        let mut gate = UploadGate::default();
        let now = Instant::now();
        gate.record_failure(GNU.1, now, "no loader".into());
        gate.record_failure(MUSL.1, now, "self-check failed".into());

        assert_eq!(gate.suppressed(GNU.1, now), Some("no loader"));
        assert_eq!(gate.suppressed(MUSL.1, now), Some("self-check failed"));
        // A payload we have never sent is not suppressed by either.
        assert_eq!(gate.suppressed("a-fresh-build", now), None);
        // The cooldown still expires, and a working connection still clears it.
        assert_eq!(gate.suppressed(GNU.1, now + UPLOAD_RETRY_COOLDOWN), None);
        gate.clear();
        assert_eq!(gate.suppressed(MUSL.1, now), None);
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
    fn the_conn_log_keeps_the_newest_lines_in_order() {
        let log = ConnLog::default();
        for i in 0..CONN_LOG_CAP + 10 {
            log.info(format!("line {i}"));
        }
        log.error("it broke");
        let entries = log.entries();
        // Bounded, oldest-first, and the tail is what survived — a host that has
        // been flapping for a week must not grow without limit, and what you
        // want when you open it is the most recent attempt.
        assert_eq!(entries.len(), CONN_LOG_CAP);
        assert_eq!(entries.last().unwrap().text, "it broke");
        assert!(entries.last().unwrap().error);
        assert_eq!(entries[0].text, format!("line {}", 11));
        assert!(!entries[0].error);
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
        let script = upload_script("d1g3st", "x86_64-unknown-linux-gnu");
        // Order is the safety property: the binary is only visible at the path
        // the next connect invokes *after* the host itself has run it.
        let stage = script.find("cat > ").unwrap();
        let verify = script.find("self-check").unwrap();
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
        for script in [
            probe_script(),
            upload_script(&"a".repeat(64), "aarch64-unknown-linux-musl"),
        ] {
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
    ///
    /// The marker these write is `<digest> <target>`: the target is what makes
    /// the candidate loop terminate, so it has to survive the round trip through
    /// a real shell like the digest does.
    const MARKER_TARGET: &str = "x86_64-unknown-linux-gnu";

    fn run_deploy(shell: &str, home: &Path, stdin_bytes: &[u8], sha: &str) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new(shell)
            .arg("-c")
            .arg(login_shell_safe(&upload_script(sha, MARKER_TARGET)))
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
        // Answers `self-check` *deliberately*, not by ignoring its argv: the
        // deploy's whole verification now hangs on that subcommand existing, so
        // a stand-in that replied to anything would pass this test while a real
        // binary predating `self-check` failed on a host.
        let fake = format!(
            "#!/bin/sh\ntest \"$1\" = self-check || exit 64\necho 'miao-server {version}'\n"
        );
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
            format!("d1g3st {MARKER_TARGET}")
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
        // Answers `self-check` *deliberately*, not by ignoring its argv: the
        // deploy's whole verification now hangs on that subcommand existing, so
        // a stand-in that replied to anything would pass this test while a real
        // binary predating `self-check` failed on a host.
        let fake = format!(
            "#!/bin/sh\ntest \"$1\" = self-check || exit 64\necho 'miao-server {version}'\n"
        );
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
                format!("d1g3st {MARKER_TARGET}"),
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
        let payload = crate::server_payload::resolve_candidates(&probe.arch)
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                panic!(
                    "no payload for {:?}; build with a bundle-* feature (have: {:?})",
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
                &[(payload.target.as_str(), payload.sha256.as_str())],
                &[payload.target.as_str()],
            ),
            Provision::Upload {
                target: payload.target.clone(),
                sha256: payload.sha256.clone(),
            },
        );

        // First connect: deploys, and resolves to what it deployed.
        let mut gate = UploadGate::default();
        let log = ConnLog::default();
        let host = HostId("test".into());
        let mut dl = UploadGate::default();
        let (exe, failure) = resolve_remote_exe(
            &target,
            &opts,
            &mut Provisioning {
                upload: &mut gate,
                download: &mut dl,
                host: &host,
            },
            &log,
        )
        .await;
        assert_eq!(failure, None, "deploy reported: {failure:?}");
        assert_eq!(exe, format!("{}/{REMOTE_CACHE_REL}", fresh.home));

        // The deployed binary is real: it answers `--version` on the host with
        // our version, and it left the marker that identifies this exact build.
        let after = probe_remote(&target, &opts).await.expect("probe");
        assert_eq!(
            after.cache_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(after.cache_sha.as_deref(), Some(payload.sha256.as_str()));
        // The marker now records which build won, not just its digest.
        assert_eq!(after.cache_target.as_deref(), Some(payload.target.as_str()));

        // Second connect: recognises its own deploy and re-sends nothing.
        assert_eq!(
            decide_provision(
                env!("CARGO_PKG_VERSION"),
                &after,
                &[(payload.target.as_str(), payload.sha256.as_str())],
                &[payload.target.as_str()],
            ),
            Provision::UseCache,
        );
        let (exe2, failure2) = resolve_remote_exe(
            &target,
            &opts,
            &mut Provisioning {
                upload: &mut gate,
                download: &mut dl,
                host: &host,
            },
            &log,
        )
        .await;
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
