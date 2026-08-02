//! `Backend` is the dashboard's seam to *where sessions run and where their
//! files live*. `Local` is in-process (the dashboard and the agents share one
//! host); `Remote` reaches a `captain-miao-server` over a (possibly
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
//! [`LocalBackend`] is also the **server-core**: `captain-miao-server` wraps one
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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::agent::{ResumeCandidate, SessionIndex};
use crate::protocol::{ClientFrame, PROTOCOL_VERSION, ServerFrame, read_frame, write_frame};
use crate::state::{self, HostId, LauncherState};

// `LocalBackend` (the server-core), `OpenSpec`, and `LaunchPlan` live in cm-core;
// re-exported so `crate::backend::…` paths across the dashboard resolve unchanged.
pub use cm_core::backend::{LaunchPlan, LocalBackend, OpenSpec};

/// Per-host session management. `Local` is in-process; `Remote` speaks the wire
/// protocol to a `captain-miao-server` over a (possibly ssh-forwarded) socket.
pub(crate) enum Backend {
    Local(LocalBackend),
    Remote(RemoteBackend),
}

/// Connection health of a backend, surfaced in the header. `Local` is always
/// `Connected`; a `Remote`'s background task moves it Connecting → Connected →
/// Disconnected (then back to Connecting as it retries with backoff). Stored as
/// an `AtomicU8` on the backend (written by the task, read by the draw thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

impl ConnState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => ConnState::Connected,
            2 => ConnState::Disconnected,
            _ => ConnState::Connecting,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            ConnState::Connecting => 0,
            ConnState::Connected => 1,
            ConnState::Disconnected => 2,
        }
    }
}

impl Backend {
    pub(crate) fn local() -> Self {
        Backend::Local(LocalBackend::default())
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

    /// Live sessions on this host (those with a current state file).
    pub(crate) fn list_sessions(&self) -> Vec<LauncherState> {
        match self {
            Backend::Local(b) => b.list_sessions(),
            Backend::Remote(b) => b.list_sessions(),
        }
    }

    /// Merge each agent backend's session-name shard into one index (today only
    /// Claude's manifest scan contributes — Codex titles arrive on
    /// `LauncherState.name` via the per-host overlay).
    pub(crate) fn session_index(&mut self) -> SessionIndex {
        match self {
            Backend::Local(b) => b.session_index(),
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
            Backend::Local(b) => b.list_resumable(limit),
            Backend::Remote(b) => b.list_resumable(limit),
        }
    }

    /// SIGTERM the agent process so its launcher tears the session down. Returns
    /// whether the signal was delivered. May block on a round-trip for a remote
    /// host, so an async caller should wrap this in `block_in_place`.
    pub(crate) fn kill_session(&self, child_pid: u32) -> bool {
        match self {
            Backend::Local(b) => b.kill_session(child_pid),
            Backend::Remote(b) => b.kill_session(child_pid),
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
            Backend::Local(b) => Ok(b.open_session(spec)),
            Backend::Remote(b) => b.open_session(spec),
        }
    }

    /// The argv for a window that attaches to an *already-running* pool session
    /// on this host (`ssh -t <target> captain-miao-server attach <name>`). `None` for
    /// the local backend — local sessions aren't pooled, they keep their own
    /// window. Used by the client to attach to a running remote session it isn't
    /// already attached to (§5).
    pub(crate) fn attach_argv(&self, session_name: &str) -> Option<Vec<String>> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => Some(attach_argv(
                b.attach_target.as_deref(),
                &b.remote_exe.lock().unwrap(),
                session_name,
            )),
        }
    }

    /// The argv for a window that opens an interactive login shell on this
    /// host in `cwd`. `None` for the local backend (the client opens a local
    /// shell itself, in-process) and for a socket-transport remote (no ssh
    /// target to reach it by); `Some` ssh argv for an ssh remote — so `w` on a
    /// remote row lands a terminal on that server in the session's workdir.
    pub(crate) fn shell_argv(&self, cwd: &str) -> Option<Vec<String>> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(b) => b
                .attach_target
                .as_deref()
                .map(|t| remote_shell_argv(t, cwd)),
        }
    }

    /// Whether a change signal has arrived from this backend's mirror since the
    /// last check (and clears it). A remote backend's connection task updates
    /// its in-memory mirror off-thread — no filesystem event fires — so the
    /// dashboard loop polls this to know when to reload + redraw remote rows.
    /// Always `false` for the local backend (its changes ride fs notify).
    pub(crate) fn take_dirty(&self) -> bool {
        match self {
            Backend::Local(_) => false,
            Backend::Remote(b) => b.take_dirty(),
        }
    }

    /// This host's recent working dirs + its `$HOME`, for the workdir picker.
    /// The remote path blocks on a round-trip, so wrap async callers in
    /// `block_in_place`.
    pub(crate) fn recent_dirs(&self) -> (Vec<String>, String) {
        match self {
            Backend::Local(b) => b.recent_dirs(),
            Backend::Remote(b) => b.recent_dirs(),
        }
    }

    /// Directory completions for `prefix` on this host's filesystem (absolute
    /// paths, trailing `/`). Remote blocks — wrap in `block_in_place`.
    pub(crate) fn complete_path(&self, prefix: &str) -> Vec<String> {
        match self {
            Backend::Local(b) => b.complete_path(prefix),
            Backend::Remote(b) => b.complete_path(prefix),
        }
    }

    /// Whether `path` is a directory on this host. Remote blocks — wrap in
    /// `block_in_place`.
    pub(crate) fn dir_exists(&self, path: &str) -> bool {
        match self {
            Backend::Local(b) => b.dir_exists(path),
            Backend::Remote(b) => b.dir_exists(path),
        }
    }
}

// =============================================================================
// Remote backend (RPC to a `captain-miao-server` over a socket)
// =============================================================================

/// How a [`RemoteBackend`] reaches its server.
pub(crate) enum Transport {
    /// Connect straight to this socket (already reachable — a manual forward or
    /// local testing).
    Socket(PathBuf),
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
    /// an ssh host (`ssh -t <target> captain-miao-server attach <name>`), `None` for a
    /// direct socket transport (a same-host `captain-miao-server attach <name>`).
    attach_target: Option<String>,
    /// Latest known sessions on the remote host, keyed by launcher pid.
    mirror: Arc<Mutex<HashMap<u32, LauncherState>>>,
    /// Requests to the connection task; `None` once the task has exited.
    requests: mpsc::UnboundedSender<PendingRequest>,
    next_req_id: AtomicU64,
    /// The command to invoke captain-miao on the remote, resolved at connect by
    /// `setup_ssh` (PATH `captain-miao`, or an auto-provisioned cache path —
    /// open-decision #3). Defaults to `captain-miao`, so before the task resolves
    /// it (or for a socket transport) the attach argv is exactly as before.
    remote_exe: Arc<Mutex<String>>,
    /// Connection health (a [`ConnState`] as `u8`) the connection task updates
    /// as it dials / connects / loses the link, read by the header surface.
    conn: Arc<AtomicU8>,
    /// Set by the connection task whenever the mirror or connection state
    /// changes (a pushed `Snapshot`/`Delta`/`Removed`, or a connect/disconnect).
    /// The dashboard loop polls [`RemoteBackend::take_dirty`] to reload + redraw,
    /// since these off-thread updates fire no filesystem event.
    dirty: Arc<AtomicBool>,
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
            Transport::Socket(_) => None,
        };
        let mirror = Arc::new(Mutex::new(HashMap::new()));
        let remote_exe = Arc::new(Mutex::new("captain-miao-server".to_string()));
        let conn = Arc::new(AtomicU8::new(ConnState::Connecting.as_u8()));
        let dirty = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(connection_task(
            transport,
            mirror.clone(),
            remote_exe.clone(),
            conn.clone(),
            dirty.clone(),
            rx,
        ));
        Self {
            host,
            attach_target,
            mirror,
            requests: tx,
            next_req_id: AtomicU64::new(1),
            remote_exe,
            conn,
            dirty,
        }
    }

    /// Current connection health, for the header surface.
    fn conn_state(&self) -> ConnState {
        ConnState::from_u8(self.conn.load(Ordering::Relaxed))
    }

    /// Take (and clear) the pending change signal — see the `dirty` field.
    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// Send a request and block until its reply (or the task is gone). Returns
    /// `None` if the connection task has exited.
    fn request(&self, make: impl FnOnce(u64) -> ClientFrame) -> Option<ServerFrame> {
        // A known-down host fails fast: queueing the request would block the
        // caller (it's on a `block_in_place`) through the whole reconnect
        // backoff. While merely dialing (Connecting) we still queue, so the very
        // first request right after `connect()` rides the pending connection.
        if self.conn_state() == ConnState::Disconnected {
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
        rx.blocking_recv().ok()
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

    fn kill_session(&self, child_pid: u32) -> bool {
        matches!(
            self.request(|req_id| ClientFrame::KillSession { req_id, child_pid }),
            Some(ServerFrame::Killed { ok: true, .. })
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
                ),
                session_name: name,
            }),
            Some(ServerFrame::Opened { error: Some(e), .. }) => anyhow::bail!(e),
            _ => anyhow::bail!("remote host unreachable"),
        }
    }

    /// The remote host's recent dirs + its `$HOME`, for the workdir picker.
    /// Blocks on the round-trip; empty + no-home if unreachable.
    fn recent_dirs(&self) -> (Vec<String>, String) {
        match self.request(|req_id| ClientFrame::ListRecentDirs { req_id }) {
            Some(ServerFrame::RecentDirs { cwds, home, .. }) => (cwds, home),
            _ => (Vec::new(), String::new()),
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
/// remote host (`ssh -t <target> captain-miao-server attach <name>`), or directly for
/// a same-host socket transport (`captain-miao-server attach <name>`). `-t` forces a
/// pty so the agent's TUI renders.
fn attach_argv(target: Option<&str>, remote_exe: &str, session_name: &str) -> Vec<String> {
    let mut argv = match target {
        Some(t) => vec![
            "ssh".to_string(),
            "-t".to_string(),
            t.to_string(),
            remote_exe.to_string(),
        ],
        None => vec![remote_exe.to_string()],
    };
    argv.push("attach".to_string());
    argv.push(session_name.to_string());
    argv
}

/// The argv for a window that opens an interactive login shell on a remote host
/// in `cwd`, over ssh: `ssh -t <target> "cd <cwd> && exec $SHELL -l"`. `-t`
/// forces a pty so the shell is interactive; the `cd` lands in the session's
/// workdir, then we hand off to the user's login shell (falling back to
/// `/bin/sh`). The path is single-quoted so spaces and glob chars are safe. An
/// empty `cwd` just drops the `cd`. Pure + unit-tested.
fn remote_shell_argv(target: &str, cwd: &str) -> Vec<String> {
    let remote_cmd = if cwd.is_empty() {
        "exec \"${SHELL:-/bin/sh}\" -l".to_string()
    } else {
        format!(
            "cd {} && exec \"${{SHELL:-/bin/sh}}\" -l",
            shell_single_quote(cwd)
        )
    };
    vec!["ssh".into(), "-t".into(), target.to_string(), remote_cmd]
}

/// Single-quote `s` for a POSIX shell: wrap in `'…'` and rewrite each embedded
/// `'` as `'\''`, so an arbitrary path can't break out of the quoting.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// =============================================================================
// Remote binary provisioning (next-step #1, open-decision #3)
//
// On connect, probe the remote for a version-matching `captain-miao-server` and
// invoke whichever copy it finds: one on PATH first, else one at our cache path
// (where `redeploy.sh` and, later, the embed-and-deploy work put it). Read-only
// and never fatal — any failure (host unreachable, version mismatch, no binary)
// resolves to `captain-miao-server` on PATH, i.e. exactly the pre-provisioning
// behavior.
//
// NOTE: the dashboard used to *auto-upload itself* here when the arches matched.
// That died with the crate split — the dashboard no longer links the pty pool
// (that's `captain-miao-server`), so the binary it could upload wouldn't be a
// functional server. The scp/upload path and its `Provision::Upload` arm were
// removed rather than left unreachable; recover them from git history if the
// embed-and-deploy work wants them back (they'd need to ship the *server*
// binary, not self).
// =============================================================================

/// Where a deployed captain-miao-server lives on the remote, relative to `$HOME`.
/// Shared with `redeploy.sh`, which uploads to exactly this path.
const REMOTE_CACHE_REL: &str = ".cache/captain-miao/bin/captain-miao-server";

/// One-shot probe of a remote host: its `$HOME`, `uname -sm`, and the version of
/// a captain-miao on PATH / at the cache path (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteProbe {
    home: String,
    arch: String,
    path_version: Option<String>,
    cache_version: Option<String>,
}

/// The provisioning action a probe + local facts imply. Pure + unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provision {
    /// A version-matching binary is already on PATH; invoke `captain-miao`.
    UsePath,
    /// A version-matching binary is already at the cache path; invoke it there.
    UseCache,
    /// Nothing version-matching anywhere; fall back to `captain-miao-server` on
    /// PATH and let the connection fail loudly if it isn't there.
    FallBack,
}

/// The shell script the probe runs over ssh. Four lines out: `$HOME`, the
/// machine, then a `--version` line (or our `-` sentinel) for the PATH binary
/// and the cache-path binary. `--version` errors and "command not found" both
/// land on stderr and a non-zero exit, so `|| echo -` normalizes them.
const PROBE_SCRIPT: &str = "echo \"$HOME\"; uname -sm; \
captain-miao-server --version 2>/dev/null || echo -; \
\"$HOME/.cache/captain-miao/bin/captain-miao-server\" --version 2>/dev/null || echo -";

/// Parse [`PROBE_SCRIPT`] output. A `--version` line is `captain-miao <ver>`;
/// our `-` sentinel and a blank line map to `None`. Pure.
fn parse_probe(out: &str) -> Option<RemoteProbe> {
    let mut lines = out.lines();
    let home = lines.next()?.trim().to_string();
    let arch = lines.next()?.trim().to_string();
    if home.is_empty() || arch.is_empty() {
        return None;
    }
    let version = |line: Option<&str>| -> Option<String> {
        let l = line?.trim();
        if l.is_empty() || l == "-" {
            return None;
        }
        // clap prints "<name> <version>"; take the version token.
        l.split_whitespace().nth(1).map(str::to_string)
    };
    let path_version = version(lines.next());
    let cache_version = version(lines.next());
    Some(RemoteProbe {
        home,
        arch,
        path_version,
        cache_version,
    })
}

/// Decide which remote binary to invoke: prefer a version-matching one on PATH
/// (a user install), then one at our cache path, else fall back to PATH and let
/// it fail. Pure + unit-tested.
fn decide_provision(local_version: &str, probe: &RemoteProbe) -> Provision {
    if probe.path_version.as_deref() == Some(local_version) {
        return Provision::UsePath;
    }
    if probe.cache_version.as_deref() == Some(local_version) {
        return Provision::UseCache;
    }
    Provision::FallBack
}

/// The remote command an action resolves to: the absolute cache path for
/// `UseCache`, else `captain-miao-server` from PATH.
fn remote_exe_for(action: &Provision, home: &str) -> String {
    match action {
        Provision::UseCache => format!("{home}/{REMOTE_CACHE_REL}"),
        Provision::UsePath | Provision::FallBack => "captain-miao-server".to_string(),
    }
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

/// Run [`PROBE_SCRIPT`] on the remote (this also primes the ControlMaster).
async fn probe_remote(target: &str, opts: &[String]) -> Option<RemoteProbe> {
    let out = Command::new("ssh")
        .args(opts)
        .arg(target)
        .arg(PROBE_SCRIPT)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

/// Resolve the remote command to invoke: probe → decide. Never errors — any
/// failure resolves to `captain-miao-server` on PATH so the rest of `setup_ssh`
/// behaves exactly as it did before provisioning existed.
async fn resolve_remote_exe(target: &str, opts: &[String]) -> String {
    let Some(probe) = probe_remote(target, opts).await else {
        tracing::debug!(
            target: "captain_miao::provision",
            "{target}: probe failed (unreachable / no shell) → PATH captain-miao-server"
        );
        return "captain-miao-server".to_string();
    };
    let action = decide_provision(env!("CARGO_PKG_VERSION"), &probe);
    tracing::debug!(
        target: "captain_miao::provision",
        "{target}: remote_arch={:?} path_ver={:?} cache_ver={:?} → {action:?}",
        probe.arch, probe.path_version, probe.cache_version
    );
    let exe = remote_exe_for(&action, &probe.home);
    tracing::debug!(target: "captain_miao::provision", "{target}: remote exe = {exe}");
    exe
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeOutcome {
    /// The `RemoteBackend` was dropped (request channel closed) — stop for good.
    BackendDropped,
    /// A subscribed connection was lost (EOF / read / write error) — reconnect
    /// promptly (a healthy link just dropped; reset the backoff).
    ConnectionLost,
    /// The handshake/subscribe never completed — reconnect, but keep backing off
    /// (an incompatible or absent server shouldn't hot-loop).
    HandshakeFailed,
}

/// Own a [`RemoteBackend`]'s connection for its whole lifetime, reconnecting on
/// loss. Each iteration establishes the transport, then [`serve`]s one connection
/// (handshake → subscribe → multiplex the pushed stream into the mirror with
/// request/response by `req_id`). On loss it clears the mirror, marks the host
/// disconnected, and retries with exponential backoff — until [`serve`] reports
/// the `RemoteBackend` was dropped, when the task exits.
async fn connection_task(
    transport: Transport,
    mirror: Arc<Mutex<HashMap<u32, LauncherState>>>,
    remote_exe: Arc<Mutex<String>>,
    conn: Arc<AtomicU8>,
    dirty: Arc<AtomicBool>,
    mut requests: mpsc::UnboundedReceiver<PendingRequest>,
) {
    // A connection-state change flips `dirty` alongside `conn` so the dashboard
    // reloads + redraws the header (⟳/⚠) promptly on connect/disconnect, not
    // only when the mirror later changes.
    let store = |s: ConnState| {
        conn.store(s.as_u8(), Ordering::Relaxed);
        dirty.store(true, Ordering::Relaxed);
    };
    let mut backoff = RECONNECT_INITIAL;
    loop {
        store(ConnState::Connecting);
        // Establish the transport; for ssh, (re)stand up the forward+server
        // child. Re-running `setup_ssh` on each attempt is deliberate: it also
        // re-cancels any stale ControlMaster forward, which is what makes a
        // reconnect actually bind its socket.
        let established = match &transport {
            Transport::Socket(p) => Some((p.clone(), None)),
            Transport::Ssh { target, local_sock } => {
                match setup_ssh(target, local_sock, &remote_exe).await {
                    Some(child) => Some((local_sock.clone(), Some(child))),
                    None => {
                        tracing::warn!(target: "captain_miao::ssh", "{target}: ssh setup failed — will retry");
                        None
                    }
                }
            }
        };
        let Some((sock_path, ssh_child)) = established else {
            store(ConnState::Disconnected);
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
            store(ConnState::Disconnected);
            if !wait_before_retry(&mut requests, &mut backoff).await {
                return;
            }
            continue;
        };
        tracing::debug!(target: "captain_miao::ssh", "connected to {}; serving", sock_path.display());
        store(ConnState::Connected);
        let connected_at = Instant::now();
        let outcome = serve(stream, &mirror, &dirty, &mut requests).await;
        drop(ssh_child); // explicit: kill the ssh child once the connection ends
        // The mirror is now stale; clear it so the host shows no (misleading)
        // rows while disconnected. A fresh `Snapshot` refills it on reconnect.
        // `store(Disconnected)` below flips `dirty` so the cleared rows redraw.
        mirror.lock().unwrap().clear();
        store(ConnState::Disconnected);
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
            ServeOutcome::ConnectionLost | ServeOutcome::HandshakeFailed => {}
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
    // Non-fatal: a failure resolves to `captain-miao` on PATH, the prior default.
    let exe = resolve_remote_exe(target, &opts).await;
    *remote_exe.lock().unwrap() = exe.clone();

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
        tracing::warn!(
            target: "captain_miao::ssh",
            "{target}: `{exe} daemon ensure` failed (rc={:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
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
    mirror: &Arc<Mutex<HashMap<u32, LauncherState>>>,
    dirty: &Arc<AtomicBool>,
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
        return ServeOutcome::HandshakeFailed;
    }
    match read_frame::<_, ServerFrame>(&mut rd).await {
        Ok(Some(ServerFrame::Welcome { protocol, .. })) if protocol == PROTOCOL_VERSION => {
            tracing::debug!(target: "captain_miao::ssh", "handshake ok (protocol {protocol})");
        }
        // Mismatched/absent welcome: bail — the dashboard surfaces the host as
        // unreachable rather than risk talking an incompatible dialect.
        other => {
            tracing::warn!(target: "captain_miao::ssh", "handshake failed, no usable Welcome: {other:?}");
            return ServeOutcome::HandshakeFailed;
        }
    }
    if write_frame(&mut wr, &ClientFrame::Subscribe).await.is_err() {
        tracing::warn!(target: "captain_miao::ssh", "failed to send Subscribe");
        return ServeOutcome::HandshakeFailed;
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
                            m.insert(s.launcher_pid, s);
                        }
                        // The mirror changed off-thread; wake the dashboard loop.
                        dirty.store(true, Ordering::Relaxed);
                    }
                    ServerFrame::Delta { state } => {
                        mirror.lock().unwrap().insert(state.launcher_pid, *state);
                        dirty.store(true, Ordering::Relaxed);
                    }
                    ServerFrame::Removed { launcher_pid } => {
                        mirror.lock().unwrap().remove(&launcher_pid);
                        dirty.store(true, Ordering::Relaxed);
                    }
                    ServerFrame::Resumable { req_id, .. }
                    | ServerFrame::Killed { req_id, .. }
                    | ServerFrame::Opened { req_id, .. }
                    | ServerFrame::RecentDirs { req_id, .. }
                    | ServerFrame::PathCompletions { req_id, .. }
                    | ServerFrame::DirChecked { req_id, .. } => {
                        if let Some(tx) = pending.remove(&req_id) {
                            let _ = tx.send(frame);
                        }
                    }
                    ServerFrame::Welcome { .. } => {} // unexpected post-handshake
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
            host: crate::state::HostId::local(),
        }
    }

    #[test]
    fn attach_argv_ssh_vs_direct() {
        // PATH default for both transports.
        assert_eq!(
            attach_argv(Some("user@box"), "captain-miao-server", "s1"),
            [
                "ssh",
                "-t",
                "user@box",
                "captain-miao-server",
                "attach",
                "s1"
            ]
        );
        assert_eq!(
            attach_argv(None, "captain-miao-server", "s1"),
            ["captain-miao-server", "attach", "s1"]
        );
        // An auto-provisioned cache path is invoked over ssh in place of `captain-miao`.
        let cache = "/home/u/.cache/captain-miao/bin/captain-miao-server";
        assert_eq!(
            attach_argv(Some("user@box"), cache, "s1"),
            ["ssh", "-t", "user@box", cache, "attach", "s1"]
        );
    }

    #[test]
    fn remote_shell_argv_cds_and_execs_login_shell() {
        assert_eq!(
            remote_shell_argv("user@box", "/home/u/proj"),
            [
                "ssh",
                "-t",
                "user@box",
                "cd '/home/u/proj' && exec \"${SHELL:-/bin/sh}\" -l"
            ]
        );
        // Empty cwd drops the `cd` and just opens a login shell.
        assert_eq!(
            remote_shell_argv("box", ""),
            ["ssh", "-t", "box", "exec \"${SHELL:-/bin/sh}\" -l"]
        );
        // A path with a space / quote is single-quoted safely.
        assert_eq!(shell_single_quote("/a b/it's"), r#"'/a b/it'\''s'"#);
    }

    fn probe(arch: &str, path: Option<&str>, cache: Option<&str>) -> RemoteProbe {
        RemoteProbe {
            home: "/home/u".into(),
            arch: arch.into(),
            path_version: path.map(str::to_string),
            cache_version: cache.map(str::to_string),
        }
    }

    #[test]
    fn parse_probe_extracts_home_arch_and_versions() {
        let out = "/home/u\nLinux x86_64\ncaptain-miao 0.1.0\n-\n";
        let p = parse_probe(out).unwrap();
        assert_eq!(p.home, "/home/u");
        assert_eq!(p.arch, "Linux x86_64");
        assert_eq!(p.path_version.as_deref(), Some("0.1.0"));
        assert_eq!(p.cache_version, None); // the "-" sentinel
    }

    #[test]
    fn parse_probe_handles_cache_only_and_blank_lines() {
        // PATH binary missing ("-"), cache binary present.
        let p = parse_probe("/root\nDarwin arm64\n-\ncaptain-miao 0.2.0").unwrap();
        assert_eq!(p.path_version, None);
        assert_eq!(p.cache_version.as_deref(), Some("0.2.0"));
        // Truncated/garbage output → None rather than a half-built probe.
        assert!(parse_probe("/home/u").is_none());
        assert!(parse_probe("\n\n").is_none());
    }

    #[test]
    fn decide_prefers_path_install_over_cache() {
        let lx = "Linux x86_64";
        // PATH match wins outright — a user install beats our cache copy.
        let p = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(decide_provision("0.1.0", &p), Provision::UsePath);
        // No PATH match, but our cache copy matches → use it.
        let p = probe(lx, None, Some("0.1.0"));
        assert_eq!(decide_provision("0.1.0", &p), Provision::UseCache);
    }

    #[test]
    fn decide_falls_back_when_nothing_matches_the_local_version() {
        let lx = "Linux x86_64";
        // Nothing deployed anywhere.
        assert_eq!(
            decide_provision("0.1.0", &probe(lx, None, None)),
            Provision::FallBack
        );
        // Both present but stale — a version mismatch must not be invoked, since
        // the wire protocol isn't guaranteed compatible across versions.
        let stale = probe(lx, Some("0.1.0"), Some("0.1.0"));
        assert_eq!(decide_provision("0.2.0", &stale), Provision::FallBack);
    }

    #[test]
    fn remote_exe_resolves_cache_path_or_falls_back_to_path() {
        assert_eq!(
            remote_exe_for(&Provision::UsePath, "/home/u"),
            "captain-miao-server"
        );
        assert_eq!(
            remote_exe_for(&Provision::FallBack, "/home/u"),
            "captain-miao-server"
        );
        assert_eq!(
            remote_exe_for(&Provision::UseCache, "/root"),
            "/root/.cache/captain-miao/bin/captain-miao-server"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_open_session_errs_when_unreachable() {
        // No server on the socket → the request never gets a reply, so
        // open_session reports the host as unreachable rather than hanging.
        let remote = RemoteBackend::connect(
            Transport::Socket(PathBuf::from("/nonexistent/captain-miao.sock")),
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
            RemoteBackend::connect(Transport::Socket(sock.clone()), HostId("mock".into()));

        let spec = OpenSpec {
            agent: AgentControl::Claude,
            cwd: "/work".to_string(),
            resume: None,
        };
        let plan = tokio::task::block_in_place(|| backend.open_session(&spec)).unwrap();
        match plan {
            // A socket transport (no ssh target) yields a direct attach window.
            LaunchPlan::AttachRemote { argv, session_name } => {
                assert_eq!(argv, ["captain-miao-server", "attach", "pool-claude"]);
                assert_eq!(session_name, "pool-claude");
            }
            LaunchPlan::SpawnLocal { .. } => panic!("expected AttachRemote from a remote backend"),
        }
        let _ = std::fs::remove_file(&sock);
    }

    /// A protocol-speaking stand-in for `captain-miao-server`: one connection,
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
                        cwds: vec!["/home/u/proj".into(), "/home/u/other".into()],
                        home: "/home/u".into(),
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

        let backend =
            RemoteBackend::connect(Transport::Socket(sock.clone()), HostId("mock".to_string()));

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
        assert!(tokio::task::block_in_place(|| backend.kill_session(999)));

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
            RemoteBackend::connect(Transport::Socket(sock.clone()), HostId("mock".into()));

        // recent_dirs: the remote's list + home ride the reply.
        let (cwds, home) = tokio::task::block_in_place(|| backend.recent_dirs());
        assert_eq!(cwds, vec!["/home/u/proj", "/home/u/other"]);
        assert_eq!(home, "/home/u");

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
            RemoteBackend::connect(Transport::Socket(sock.clone()), HostId("mock".into()));

        wait_for_len(&backend, 1).await;
        assert_eq!(backend.conn_state(), ConnState::Connected);

        // Force the server to drop the connection; the client must re-dial.
        drop_tx.send(()).unwrap();

        wait_for_len(&backend, 2).await;
        assert_eq!(backend.conn_state(), ConnState::Connected);

        let _ = std::fs::remove_file(&sock);
    }
}
