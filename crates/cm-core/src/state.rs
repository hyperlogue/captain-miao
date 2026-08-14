use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::agent::AgentControl;
use crate::terminal::{TabId, WindowId};

// -- Paths --

/// State files carry the user's own prompt text (`first_prompt`/`last_prompt`),
/// working directories, and session ids, so they are owner-only — matching the
/// launcher's 0700 socket dir and the synthetic `$CODEX_HOME`. Without this they
/// inherit the umask (0755/0644 under the common 022), leaving every prompt
/// readable by any other local user on a shared machine.
const OWNER_ONLY_DIR: u32 = 0o700;
const OWNER_ONLY_FILE: u32 = 0o600;

/// `create_dir_all` with the intermediate *and* final components owner-only.
/// `DirBuilder::mode` applies to every directory it creates, and it is a no-op
/// for one that already exists — so an existing world-readable state dir from an
/// older build is tightened separately by [`harden_dir`].
pub fn create_dir_all_private(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(OWNER_ONLY_DIR)
        .create(dir)?;
    harden_dir(dir);
    Ok(())
}

/// Drop group/other bits from an existing directory, so a state dir created by
/// an older captain-miao (0755) is tightened in place on the next run. Best
/// effort: a dir we don't own isn't ours to fix.
fn harden_dir(dir: &Path) {
    if let Ok(meta) = std::fs::metadata(dir) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode & !0o077));
        }
    }
}

pub fn state_dir() -> PathBuf {
    // Per the XDG spec an empty env var is treated as unset, not as a
    // relative path, so filter out the empty string before falling back.
    std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local/state")
        })
        .join("captain-miao")
}

/// Where sockets and other runtime-lifetime files live.
///
/// `$XDG_RUNTIME_DIR` when set (Linux: `/run/user/<uid>`, a per-user tmpfs the
/// OS clears at logout) — the right home for these, and used unchanged.
///
/// The fallback — macOS, where that variable is never set — is **not**
/// `$TMPDIR`, which is where it used to point. macOS reaps `/var/folders/…/T`
/// on a periodic sweep, and a launcher's hook socket living there is deleted out
/// from under a session that outlives the sweep. Nothing notices: the launcher's
/// listener stays bound to an unlinked inode, every hook silently fails to
/// connect, and the row freezes at whatever status it last held while the
/// session keeps running for hours. So the fallback goes under [`state_dir`],
/// which nothing reaps. That also drops the `captain-miao-<uid>` namespacing the
/// old path needed: a home directory is already per-user, so there is no shared
/// world-writable dir left for another user to win a race in.
///
/// The trade is that these files now outlive a reboot, where `$TMPDIR` got them
/// cleared for free — so a launcher killed without unwinding leaves an orphan
/// behind. `sweep_dead_launcher_runtime_files` reaps those on the next launch.
/// (ssh's control/forward sockets deliberately stay in [`ssh_sock_dir`]: they
/// live under a much tighter path-length limit, and ssh re-establishes a
/// control master whose socket went missing.)
pub fn runtime_dir() -> PathBuf {
    // Per the XDG spec an empty env var is treated as unset, not as a relative
    // path, so filter out the empty string before falling back.
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|d| PathBuf::from(d).join("captain-miao"))
        .unwrap_or_else(|| state_dir().join("run"))
}

pub fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

pub fn ensure_sessions_dir() -> Result<PathBuf> {
    let dir = sessions_dir();
    create_dir_all_private(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    // `sessions/` is created recursively, so this also tightens `state_dir()`
    // itself when an older build left it 0755.
    harden_dir(&state_dir());
    Ok(dir)
}

pub fn dashboard_window_id_path() -> PathBuf {
    state_dir().join("dashboard-window-id")
}

/// Format the `dashboard-window-id` file payload: the dashboard's window id,
/// prefixed with its terminal-instance identity as `<identity>|<window-id>` when
/// it has one, so the external `focus` process only drives a window in its own
/// namespace (Kitty window ids and zellij pane ids overlap). A bare
/// `<window-id>` when there is no identity.
pub fn format_dashboard_window_id(identity: Option<&str>, window_id: &WindowId) -> String {
    match identity {
        Some(id) => format!("{id}|{window_id}"),
        None => window_id.to_string(),
    }
}

/// Inverse of [`format_dashboard_window_id`], yielding `(identity, window_id)`.
/// Splits on the *last* `|`: a window id is a bare integer and never contains
/// one, while an identity (a zellij session name) might.
pub fn parse_dashboard_window_id(s: &str) -> (Option<String>, WindowId) {
    match s.rsplit_once('|') {
        Some((identity, wid)) => (Some(identity.to_string()), WindowId(wid.to_string())),
        None => (None, WindowId(s.to_string())),
    }
}

pub fn dashboard_pid_path() -> PathBuf {
    state_dir().join("dashboard.pid")
}

/// Singleton lock for `captain-miao server` (mirrors `dashboard.pid`): holds the
/// running server's pid so an `--ensure` start is idempotent.
pub fn server_pid_path() -> PathBuf {
    state_dir().join("server.pid")
}

/// The per-host server's control socket. A remote dashboard reaches it by
/// forwarding this path over ssh; a local one connects to it directly.
pub fn server_sock_path() -> PathBuf {
    runtime_dir().join("server.sock")
}

/// captain-miao's private pty-pool socket. A dedicated path (not shpool's
/// default `$XDG_RUNTIME_DIR/shpool/shpool.socket`) gives the pool its own
/// session namespace. Resolved here — in the shared core — so the server (which
/// binds it) and the client (which lists/attaches over it) can't drift on the
/// path.
pub fn pool_socket_path() -> PathBuf {
    runtime_dir().join("pty-pool.sock")
}

/// The base dir for ssh control/forward sockets, kept as short as possible.
/// These live under the tightest constraint in the codebase: ssh's
/// `ControlMaster` appends a ~17-char random suffix to `ControlPath`, on top of
/// our own hashed name, while macOS caps a `sockaddr_un` path near 104 bytes.
/// So they get their own flat `cm-<uid>` dir under `$TMPDIR` rather than
/// nesting under [`runtime_dir`], whose macOS fallback is a `$HOME`-relative
/// path — longer, and on a networked home directory unable to host a unix
/// socket at all. Being reaped from `$TMPDIR` is survivable here in a way it is
/// not for a launcher's hook socket: ssh simply re-establishes a control master
/// whose socket went missing. `$XDG_RUNTIME_DIR` (short on Linux:
/// `/run/user/<uid>`) is used when present.
pub fn ssh_sock_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|d| PathBuf::from(d).join("miao"))
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("cm-{uid}"))
        })
}

/// A short hex digest of `key`. Used to keep per-host ssh socket / control
/// paths well under the OS `sockaddr_un` limit (~104 bytes on macOS, where the
/// runtime/temp dir is itself a long `/var/folders/...` path) and collision-free
/// across distinct targets (the whole key is hashed, not lossily sanitized).
fn short_hash(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    // 48 bits (12 hex) — collision-free for any realistic host count, and short
    // enough to leave margin under the socket-path limit on a long temp dir.
    format!("{:012x}", h.finish() & 0xFFFF_FFFF_FFFF)
}

/// Local socket the dashboard forwards a remote host's server onto (one per
/// host). Hashed-short and under the flat [`ssh_sock_dir`] so the path can't
/// overflow the OS socket-path limit no matter how long the host label is.
pub fn remote_forward_sock(host_label: &str) -> PathBuf {
    ssh_sock_dir().join(format!("r{}.sock", short_hash(host_label)))
}

/// ssh `ControlMaster` socket path for `target`. Hashed (the full target) so two
/// targets never share a control connection, and under the flat [`ssh_sock_dir`]
/// so it + ssh's ~17-char random suffix stays within the socket-path limit.
pub fn ssh_control_path(target: &str) -> PathBuf {
    ssh_sock_dir().join(format!("c{}", short_hash(target)))
}

pub fn dashboard_overrides_path() -> PathBuf {
    state_dir().join("dashboard-overrides.json")
}

/// Snapshot of every restartable session the dashboard knew about. Written on
/// every reload while running and removed on clean exit, so its presence at
/// startup means the previous dashboard exited unexpectedly — and any entries
/// whose launcher pid is no longer alive are sessions that died with it.
pub fn dashboard_sessions_snapshot_path() -> PathBuf {
    state_dir().join("dashboard-sessions.json")
}

pub fn recent_cwds_path() -> PathBuf {
    state_dir().join("recent-cwds.json")
}

/// Persisted list of recently-used cwds for the workdir picker, most-recent
/// first. Shared by the dashboard (its own list) and the server (which serves
/// this host's list to a remote dashboard, and records into it on launch).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RecentCwds {
    #[serde(default)]
    pub cwds: Vec<String>,
}

/// Remote hosts the dashboard federates, managed in the TUI (add/remove/edit +
/// per-host name color) and persisted here. Mutable runtime state, not static
/// config — like `recent-cwds`/`directory-marks`.
pub fn hosts_path() -> PathBuf {
    state_dir().join("hosts.json")
}

/// User-set icon + color overrides keyed by canonicalized cwd. Sessions
/// without an override fall back to a deterministic (icon, color) pair
/// derived from the path. Edited via the `Space c` popup in the dashboard.
pub fn directory_marks_path() -> PathBuf {
    state_dir().join("directory-marks.json")
}

/// Dashboard-owned `(host, cwd) → tab` map for the work tabs `w` opened. The
/// terminal keeps those tabs alive across a dashboard restart, so persisting the
/// map lets `w` return to an existing work tab instead of spawning a duplicate.
/// Validated lazily against a live snapshot on use (stale entries self-heal), so
/// it's safe to delete (regenerated as `w` is used).
pub fn work_tabs_path() -> PathBuf {
    state_dir().join("work-tabs.json")
}

/// Dashboard-owned projection of `window → (host, launcher_pid, token)` for every
/// session the dashboard has a window for (next-step #6 §15.4). The dashboard is
/// the sole writer — rebuilt each reload from the live rows + `WindowBindings` —
/// and three readers consume it: the dashboard re-seeds its in-memory bindings
/// from it on startup (recovery across a restart, §15.7), the external
/// `miao focus --window-id` bell keybind resolves a window→pid through it
/// (replacing the old state-file scan), and the prune loop garbage-collects it.
/// Safe to delete (regenerated next reload).
pub fn window_bindings_path() -> PathBuf {
    state_dir().join("window-bindings.json")
}

/// The host-side per-session flags sidecar (`docs/remote-sessions.md` §9): a
/// `SessionKey → SessionFlags` map the **server-core** owns, so every dashboard
/// attached to a host sees the same pins/bells and they survive a dashboard
/// restart. Deliberately a sidecar rather than a field on the launcher's state
/// file: that file has exactly one writer (its launcher), and flags are set by
/// someone else entirely.
///
/// Last-writer-wins across concurrent dashboards, by decision (§8) — nothing
/// coordinates beyond the atomic replace. Safe to delete (flags reset).
pub fn session_flags_path() -> PathBuf {
    state_dir().join("session-flags.json")
}

// -- Process utilities --

pub fn is_process_alive(pid: u32) -> bool {
    // `kill(pid, 0)` returns 0 when the signal could be sent, but -1/EPERM
    // when the process exists yet is owned by another user. Treat EPERM as
    // alive so we never delete or "restart" a live session's state.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// -- Attach guards (shared by miao-server and miao-client) --

/// Exit code of `attach` when the pool session already has a client attached.
/// libshpool's own busy refusal exits 0 — indistinguishable from a clean
/// detach to anything watching the attach process — so the wrappers pre-check
/// and refuse with this code instead.
pub const ATTACH_EXIT_BUSY: i32 = 43;

/// Exit code of `attach` when no live captain-miao session owns the pool
/// name. Attaching anyway would make libshpool *resurrect* the name as a bare
/// login shell: a session whose command died while detached is never removed
/// from shpool's table, and a name-attach to it (or to an unknown name)
/// silently creates a fresh shell wearing the `cm-…` name.
pub const ATTACH_EXIT_STALE: i32 = 44;

/// The live captain-miao session bound to pool session `name`: a state file
/// carrying `pool_session == name` whose launcher process is still alive.
/// `alive` is injected ([`is_process_alive`] in production) so the policy is
/// testable without real processes.
pub fn find_live_pool_session<'a>(
    states: &'a [LauncherState],
    name: &str,
    alive: impl Fn(u32) -> bool,
) -> Option<&'a LauncherState> {
    states
        .iter()
        .find(|s| s.pool_session.as_deref() == Some(name) && alive(s.launcher_pid))
}

// -- JSON file helpers --

/// Atomic write: serialize to pretty JSON, write to `<path>.tmp`, then rename.
/// Returns Err if serialization, write, or rename fails.
///
/// The temp file is created 0600 (see [`OWNER_ONLY_FILE`]) and `rename` carries
/// the mode across, so the visible file is never briefly world-readable — and
/// `.mode()` only applies when *creating*, so an existing file from an older
/// build is re-chmod'd explicitly.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;

    let json = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(OWNER_ONLY_FILE)
        .open(&tmp)?;
    f.write_all(json.as_bytes())?;
    f.set_permissions(std::fs::Permissions::from_mode(OWNER_ONLY_FILE))?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read + parse JSON, returning None if the file is missing or malformed.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Truncate `s` to at most `max` characters (on a char boundary), appending an
/// ellipsis when it was shortened. Newlines are preserved within the budget.
fn snippet(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Identifies the host a session lives on. `"local"` for in-process sessions;
/// a user-chosen label per remote host. The dashboard tags each session with
/// its host during reload (it's never serialized — the launcher and server
/// don't know it), so flags/selection can key on `(host, launcher_pid)` and not
/// collide a remote pid with a local one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(pub String);

impl HostId {
    pub fn local() -> Self {
        HostId("local".to_string())
    }
    pub fn is_local(&self) -> bool {
        self.0 == "local"
    }
}

impl Default for HostId {
    fn default() -> Self {
        HostId::local()
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// -- Session status --

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Active,
    Compacting,
    Compacted,
    Idle,
    /// The agent's turn has ended (it would be `Idle`) but a **short-term**
    /// `run_in_background` shell it spawned is still running — a build/test/step
    /// the agent is waiting to finish before it resumes. Treated as **busy**
    /// (green "Task", keep-awake, active-grouped): finite work is genuinely in
    /// progress. Detected by the launcher mirroring Claude's own session-status
    /// file (`status == "shell"`), not from a hook (see
    /// `claude::session_activity`); a background shell that turns out to be a
    /// **long-running service** is refined away to `BackgroundServer` by
    /// classifying its command (see `BackgroundServer`).
    BackgroundActive,
    /// A refinement of `BackgroundActive`: the turn ended and every running
    /// `run_in_background` shell is a **long-running service** — a dev server or
    /// file watcher the agent parked and moved on from, not finite work it's
    /// waiting on. The agent is at rest, so unlike `BackgroundActive` this is
    /// **not** `is_busy()` (no keep-awake, idle-grouped), renders yellow, and
    /// *entering* it arms the row's follow-up bell so the parked session still
    /// draws a glance. The launcher classifies each background command against a
    /// seed heuristic (`claude::is_long_running_command`) **and** a self-learning
    /// store of commands previously observed to run long (`learned`), so a dev
    /// server is recognized either from a built-in list or from experience.
    BackgroundServer,
    /// A refinement of `BackgroundActive`: the turn ended and the *only*
    /// `run_in_background` shells still running are r3 review-watches
    /// (`r3 watch <review-id>`), so the agent is blocked waiting for a **human
    /// review**, not doing work. Surfaced as an attention state (the human is
    /// being asked to go review) rather than a busy one. The launcher promotes
    /// `BackgroundActive → ReviewPending` when a live process-tree scan reports
    /// every running background shell is a review-watch (`AgentControl::bg_shells`),
    /// and demotes back the moment a non-watch task joins them.
    ReviewPending,
    WaitingForApproval,
    WaitingForDecision,
    /// The launch never produced an agent: `direnv` blocked on the session's
    /// `.envrc`, the agent binary was missing, or the spawn failed. The launcher
    /// holds the (`--hold`'d) window open and keeps this row visible — carrying
    /// `last_error` — until the user closes the window or kills the row, so a
    /// failed start is a first-class, dismissable row rather than a silently
    /// reaped one. Has no `child_pid` (no agent ever ran).
    FailedToStart,
}

impl SessionStatus {
    /// Every status variant — kept exhaustive so the dashboard can size its
    /// Status column from the longest `label()` without hardcoding a width.
    /// Add new variants here when extending the enum.
    pub const ALL: &'static [SessionStatus] = &[
        SessionStatus::Starting,
        SessionStatus::Active,
        SessionStatus::Compacting,
        SessionStatus::Compacted,
        SessionStatus::Idle,
        SessionStatus::BackgroundActive,
        SessionStatus::BackgroundServer,
        SessionStatus::ReviewPending,
        SessionStatus::WaitingForApproval,
        SessionStatus::WaitingForDecision,
        SessionStatus::FailedToStart,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Active => "Active",
            Self::Compacting => "Compacting",
            Self::Compacted => "Compacted",
            Self::Idle => "Idle",
            Self::BackgroundActive => "Task",
            Self::BackgroundServer => "Server",
            Self::ReviewPending => "Review",
            Self::WaitingForApproval => "Approval",
            Self::WaitingForDecision => "Decision",
            Self::FailedToStart => "Failed",
        }
    }

    /// Longest `label()` across all variants — the only valid source of truth
    /// for laying out the dashboard's Status column.
    pub fn max_label_width() -> usize {
        Self::ALL
            .iter()
            .map(|s| s.label().chars().count())
            .max()
            .unwrap_or(0)
    }

    /// Whether this status requires user attention — the agent is actively
    /// asking (permission request, elicitation), blocked on a human review
    /// (`ReviewPending`), or the launch failed and the row is waiting to be
    /// dealt with — not just post-Stop idle.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Self::WaitingForApproval
                | Self::WaitingForDecision
                | Self::ReviewPending
                | Self::FailedToStart
        )
    }

    /// Whether the session is doing work: the agent is mid-turn
    /// (`Active`/`Compacting`) or its turn ended but a **short-term** background
    /// task it's waiting on is still running (`BackgroundActive`). Drives the
    /// active-group sort, the keep-awake inhibitor, and the launcher's
    /// `active_since` gate — everywhere a "busy vs at-rest" split is needed.
    /// `BackgroundServer` (a long-running dev server / watcher the agent parked)
    /// is deliberately **not** here: the agent isn't working, so it's at-rest —
    /// no keep-awake, idle-grouped.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Active | Self::Compacting | Self::BackgroundActive
        )
    }
}

// -- Hook events --

/// Normalized hook-event vocabulary the launcher acts on. The current shape
/// mirrors Claude Code's hook contract; other backends are responsible for
/// mapping their native events into these variants (or extending the enum
/// when they expose something genuinely new).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    PromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    Elicitation,
    ElicitationResult,
    Stop,
    StopFailure,
    PreCompact,
    PostCompact,
    CwdChanged,
}

impl HookEvent {
    pub const ALL: &'static [HookEvent] = &[
        Self::SessionStart,
        Self::PromptSubmit,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
        Self::PermissionRequest,
        Self::Elicitation,
        Self::ElicitationResult,
        Self::Stop,
        Self::StopFailure,
        Self::PreCompact,
        Self::PostCompact,
        Self::CwdChanged,
    ];

    pub fn as_kebab(&self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::PromptSubmit => "prompt-submit",
            Self::PreToolUse => "pre-tool-use",
            Self::PostToolUse => "post-tool-use",
            Self::PostToolUseFailure => "post-tool-use-failure",
            Self::PermissionRequest => "permission-request",
            Self::Elicitation => "elicitation",
            Self::ElicitationResult => "elicitation-result",
            Self::Stop => "stop",
            Self::StopFailure => "stop-failure",
            Self::PreCompact => "pre-compact",
            Self::PostCompact => "post-compact",
            Self::CwdChanged => "cwd-changed",
        }
    }

    pub fn from_kebab(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|e| e.as_kebab() == s)
    }
}

impl Serialize for HookEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_kebab())
    }
}

impl<'de> Deserialize<'de> for HookEvent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        HookEvent::from_kebab(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown hook event: {s}")))
    }
}

// -- Hook message (hooks -> launcher) --

#[derive(Debug, Serialize, Deserialize)]
pub struct HookMessage {
    pub event: HookEvent,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// The title the agent itself has already settled on for this session, when
    /// its payload carries one. Stamped straight onto [`LauncherState::name`],
    /// so a backend that reports it needs no title store, no sqlite reader and
    /// no per-host overlay — the two mechanisms Claude and Codex each had to
    /// grow are simply absent for it.
    ///
    /// Carried on `HookMessage` rather than handled per-event because it is not
    /// an event: it rides *every* payload of the backends that have it, so the
    /// adoption belongs with the session id's, in
    /// [`crate::agents::common::adopt_session_identity`].
    #[serde(default)]
    pub session_title: Option<String>,
    /// Path to the current session's transcript on disk, if the agent
    /// exposes one in its hook payload. The launcher watches it to detect
    /// out-of-band signals (approval dismissed, assistant message appended,
    /// interrupt) without polling. None for agents that don't surface a path.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// Raw stdin JSON from the agent — kept so failure events
    /// (stop-failure, post-tool-use-failure) can surface the full payload
    /// verbatim.
    #[serde(default)]
    pub raw: Option<String>,
}

// -- Launcher state file --

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LauncherState {
    /// Which backend produced this session. Per-session: a single dashboard
    /// can host sessions from multiple backends side by side and dispatches
    /// every backend-shaped operation through this field. Defaulted to
    /// Claude so state files written before the field existed still parse.
    #[serde(default)]
    pub agent: AgentControl,
    pub launcher_pid: u32,
    pub session_id: Option<String>,
    pub window_id: Option<WindowId>,
    pub tab_id: Option<TabId>,
    pub cwd: String,
    pub status: SessionStatus,
    pub last_tool: Option<String>,
    pub updated_at: u64,
    /// Timestamp of the most recent non-active → active transition. Reset to
    /// `None` the moment the session leaves Active/Compacting, so the dashboard
    /// can order the active group by when each session started working rather
    /// than by `updated_at`, which churns on every PreToolUse/PostToolUse.
    #[serde(default)]
    pub active_since: Option<u64>,
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub child_pid: Option<u32>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Transcript-derived facts the launcher folds and stamps so the dashboard
    /// (and, later, a remote server) reads only this file — never a transcript.
    /// Latest context-window token total.
    #[serde(default)]
    pub context_tokens: Option<u64>,
    /// Model id backing the latest turn (e.g. `claude-opus-4-8`, `gpt-5.5`).
    #[serde(default)]
    pub model: Option<String>,
    /// Display name. Claude: the `/rename`, folded from its session file by the
    /// launcher (so it's persisted in the state file). Codex: the sqlite title,
    /// stamped in-memory by the per-host `LocalBackend` overlay as sessions are
    /// served (never written to the state file).
    #[serde(default)]
    pub name: Option<String>,
    /// First real user prompt — the auto-title fallback shown before a rename.
    #[serde(default)]
    pub first_prompt: Option<String>,
    /// Pool session name when this launcher runs inside a remote pty pool (the
    /// server passes it via `--pool-session`); `None` for local sessions. Unlike
    /// `host`, this *is* serialized — it rides the subscription so the client can
    /// attach a window to an already-running remote session (§8).
    #[serde(default)]
    pub pool_session: Option<String>,
    /// Opaque token the dashboard mints and threads onto a *local* launcher it
    /// spawns (`--launch-id`), echoed back here so the dashboard can bind the
    /// appearing row to the window it opened — local's analog of `pool_session`
    /// (next-step #6 §15). `None` for a hand-launched session (`captain-miao
    /// claude` run directly); such a launcher self-reports `window_id` instead
    /// and the resolver falls back to it.
    #[serde(default)]
    pub launch_id: Option<String>,
    /// The terminal *instance* the launcher ran in
    /// (`zellij:<session>` / `kitty:<socket|pid>`, from
    /// [`crate::terminal::current_terminal_identity`]), stamped unconditionally at
    /// startup when the env yields one. Namespaces `window_id`: the dashboard only
    /// drives a window whose terminal matches its own, since Kitty window ids and
    /// zellij pane ids overlap. `None` for a headless launch. Serialized (unlike
    /// `host`) so it reaches the dashboard off the state file / wire.
    #[serde(default)]
    pub terminal: Option<String>,
    /// `TERM` as the launcher process sees it — the terminfo the agent's TUI is
    /// actually rendering against. Stamped once at startup (a process's env
    /// doesn't change) and never updated.
    ///
    /// Worth its own field because for a **pooled** session this is neither
    /// guessable nor stable-looking: libshpool takes `TERM` from the attach
    /// header of the client that *creates* the session and injects it into the
    /// pty's environment there and only there, so the value is frozen at the
    /// first attach and every later window — a different emulator, a steal,
    /// a reattach after a reboot — inherits it. On top of that the host's own
    /// wrapper rewrites a `dumb`/empty/unknown-terminfo value to
    /// `xterm-256color` (`server_pool::POOL_SHELL`), so a session opened from
    /// Kitty onto a host without kitty's terminfo is running as
    /// `xterm-256color` and nothing said so. This field is what says so.
    ///
    /// `None` from a launcher with no `TERM` at all, and from any host too old
    /// to send the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminfo: Option<String>,
    /// Per-session flags (pinned / follow-up) as the **owning host**
    /// knows them, overlaid by the server-core from its sidecar as sessions are
    /// served — never written by the launcher (single-writer rule). `None` from
    /// a backend that doesn't serve flags (a plain local dashboard, which keeps
    /// its own `dashboard-overrides.json`). Serialized so it rides the wire;
    /// part of `PartialEq`, so a flag change from another dashboard pushes a
    /// `Delta` like any other state change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<SessionFlags>,
    /// Whether a terminal is currently attached to this session's pool pty,
    /// overlaid by the server-core from libshpool's own session list. `None`
    /// when unknown (not a pool session, or the pool couldn't be queried).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached: Option<bool>,
    /// Which host this session lives on. Never serialized — the launcher and
    /// server don't know it; the dashboard stamps it during reload so per-row
    /// keying can disambiguate a remote pid from a local one. Defaults `local`.
    #[serde(skip)]
    pub host: HostId,
}

/// Per-session flags a host owns on behalf of every dashboard watching it
/// (`docs/remote-sessions.md` §9). Persisted in the daemon's sidecar
/// ([`session_flags_path`]), overlaid onto served rows, and updated by
/// `ClientFrame::SetSessionFlags` — so pins and bells are the same for every
/// dashboard attached to the host, and survive a dashboard restart.
///
/// This carried a third flag, `muted`, until it was dropped as unused. Both
/// directions of a version-mixed pair keep working: `#[serde(default)]` fills
/// it in for an old peer's frame that omits it, and serde drops the unknown
/// field from an old peer's frame that still sends it — the mute simply has no
/// effect anywhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionFlags {
    pub pinned: bool,
    pub follow_up: bool,
}

impl SessionFlags {
    /// Whether every flag is off — the value that means "drop the entry"
    /// rather than persist a row of `false`s.
    pub fn is_clear(&self) -> bool {
        *self == SessionFlags::default()
    }
}

/// The opaque identifier for a session on its owning host — the **only** thing
/// that crosses the backend seam or the wire (`docs/remote-sessions.md` §3).
///
/// Minted by the owning backend; no caller above the seam may parse it. The
/// encoding it happens to carry (the launcher pid, which names the state file)
/// is an implementation detail of [`crate::backend::LocalBackend`], and the
/// server **re-resolves key → current pid from the live state file at signal
/// time** rather than trusting a pid a possibly-stale mirror sent. That's the
/// mis-kill fix: the old wire carried the agent pid, so a mirror lagging a
/// session's exit plus OS pid reuse could SIGTERM an unrelated process.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionKey(pub String);

impl SessionKey {
    /// The key for a session identified by its launcher pid — the state file's
    /// own name, so key → file is a direct lookup on the owning host.
    pub fn from_launcher_pid(pid: u32) -> Self {
        SessionKey(pid.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl LauncherState {
    fn file_path(launcher_pid: u32) -> PathBuf {
        sessions_dir().join(format!("{launcher_pid}.json"))
    }

    /// This session's opaque [`SessionKey`] on its owning host.
    pub fn key(&self) -> SessionKey {
        SessionKey::from_launcher_pid(self.launcher_pid)
    }

    /// The binding **token** the dashboard keys this session's window by: its
    /// `pool_session` when it runs in a pty pool (the join key the attach
    /// window names), else the `launch_id` the dashboard minted for the local
    /// spawn. `None` for a hand-launched session, which self-reports its own
    /// `window_id` instead.
    ///
    /// The single accessor for a choice that used to be re-derived at four call
    /// sites (window resolution, binding GC, binding re-seed, launch bind).
    /// Keyed on *pooled-ness*, not on host: under pooled-localhost a local
    /// session is pooled too, and `pool_session` is then the right token.
    pub fn binding_token(&self) -> Option<&str> {
        self.pool_session.as_deref().or(self.launch_id.as_deref())
    }

    pub fn write(&self) -> Result<()> {
        tracing::debug!(
            target: "captain_miao::state",
            "write pid={} status={:?} active_since={:?} last_tool={:?}",
            self.launcher_pid,
            self.status,
            self.active_since,
            self.last_tool,
        );
        // Cap the free-text fields just before they hit disk so a pasted wall of
        // text in a prompt/title/error can't bloat every read/parse/push of the
        // state file. The in-memory copy keeps the full text (harmless).
        write_json_atomic(&Self::file_path(self.launcher_pid), &self.capped())
    }

    /// A clone with the unbounded text fields truncated to display-sized
    /// snippets. Titles are short by nature; prompts/errors get a generous bound
    /// that still defeats the pathological multi-KB paste.
    fn capped(&self) -> LauncherState {
        let cap = |o: &Option<String>, n: usize| o.as_deref().map(|s| snippet(s, n));
        LauncherState {
            last_prompt: cap(&self.last_prompt, 500),
            last_error: cap(&self.last_error, 1000),
            name: cap(&self.name, 120),
            first_prompt: cap(&self.first_prompt, 120),
            ..self.clone()
        }
    }

    pub fn remove(launcher_pid: u32) {
        let _ = std::fs::remove_file(Self::file_path(launcher_pid));
    }

    pub fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

/// Bell signaling — small sentinel files dropped into `sessions_dir()` by
/// `miao focus --window-id <id>` so external Kitty keybinds can ring
/// the bell on a session. The dashboard drains them on every reload via
/// `drain_bell_flag_pids()` and applies them as `follow_up = true`.
///
/// File naming: `bell-{launcher_pid}.flag`. The `.flag` extension keeps
/// these files invisible to `read_all_launcher_states` (which filters for
/// `*.json`) and avoids any collision with the `.tmp` files used during
/// atomic state writes.
fn bell_flag_path(launcher_pid: u32) -> PathBuf {
    sessions_dir().join(format!("bell-{launcher_pid}.flag"))
}

/// One entry of `window-bindings.json` — the dashboard's disk projection of which
/// local window shows which session (next-step #6 §15.4). `host == "local"` for
/// in-process sessions; `token` is the session's `launch_id` (local) or
/// `pool_session` (remote). See [`window_bindings_path`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowBinding {
    pub window_id: WindowId,
    pub host: String,
    pub launcher_pid: u32,
    pub token: String,
    /// The terminal-instance identity of the dashboard that recorded this binding
    /// (from [`crate::terminal::current_terminal_identity`]). A binding is only
    /// resolved, validated, or pruned by a dashboard whose own identity matches;
    /// one from another terminal is inert (its window id belongs to a different,
    /// overlapping namespace) and is carried through rewrites untouched. `None`
    /// for a binding written without an identity.
    #[serde(default)]
    pub terminal: Option<String>,
}

/// Look up which launcher owns Kitty `window_id` via the dashboard's
/// `window-bindings.json` projection, then drop the bell sentinel for that pid.
/// No-op when no *local* session is bound to the window — pressing the keybind in
/// a non-agent window simply focuses the dashboard. (A window holding a remote
/// `ssh attach` resolves to a non-`local` host here; ringing its bell needs the
/// per-host RPC that remote focus/bell will add — next-step #4(b) — so it's left
/// alone for now.) Reads the projection rather than scanning state files because
/// the launcher no longer self-reports `window_id` for dashboard-spawned sessions.
///
/// `terminal` is the focus process's own terminal-instance identity: only a
/// binding recorded by a dashboard in that same terminal is matched, since a
/// window id from another terminal names a different (overlapping) namespace's
/// window. A `None` identity matches only `None`-identity bindings.
pub fn write_bell_flag_for_window(window_id: &WindowId, terminal: Option<&str>) {
    let bindings: Vec<WindowBinding> = read_json(&window_bindings_path()).unwrap_or_default();
    let Some(pid) = bell_target_pid(&bindings, window_id, terminal) else {
        return;
    };
    let _ = std::fs::write(bell_flag_path(pid), b"");
}

/// The launcher pid whose bell to ring for `window_id` in terminal `terminal`:
/// the *local* binding whose window and terminal both match. Split from the IO so
/// the identity-scoped match is testable without disk. `None` when no such
/// binding exists — the keybind fired in a non-agent window, or the window
/// belongs to another terminal instance.
fn bell_target_pid(
    bindings: &[WindowBinding],
    window_id: &WindowId,
    terminal: Option<&str>,
) -> Option<u32> {
    bindings
        .iter()
        .find(|b| {
            b.host == "local" && b.terminal.as_deref() == terminal && &b.window_id == window_id
        })
        .map(|b| b.launcher_pid)
}

/// Pop every bell sentinel currently present, returning the launcher pids
/// they targeted. Files for dead pids are still cleaned up. Drains in one
/// pass so a burst of sentinels doesn't queue up redundant fs events.
pub fn drain_bell_flag_pids() -> Vec<u32> {
    let dir = sessions_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix("bell-") else {
            continue;
        };
        let Some(pid_str) = rest.strip_suffix(".flag") else {
            continue;
        };
        let _ = std::fs::remove_file(&path);
        if let Ok(pid) = pid_str.parse::<u32>() {
            pids.push(pid);
        }
    }
    pids
}

/// One attach window reporting that its attach process has ended — the
/// event that replaces polling the terminal for "is that window still there?".
///
/// The dashboard binds a pooled session to the local window it opened, and the
/// binding can only be retired by learning the window died. Nothing pushes that:
/// the pooled session keeps running, so no state file moves and no host delta
/// fires, and neither Kitty nor zellij has a window-closed callback — which is
/// why detection used to be a periodic `snapshot()` of the whole window tree.
/// So the attach command is wrapped in a shell that reports its own exit
/// (`report_on_exit_argv`), and the report lands here as a sentinel in the
/// sessions dir — a directory the dashboard already watches, so the wake is
/// inotify/FSEvents rather than a clock.
///
/// The window closing SIGHUPs the wrapper, and an in-session shpool detach or a
/// dropped ssh ends the attach process on its own, so the same report covers
/// every way an attach can end. What it can't cover is the terminal emulator
/// being killed outright (no trap runs), which is why the periodic prune stays
/// as a backstop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetachReport {
    /// The host whose session this attach window was showing.
    pub host: String,
    /// The session's binding token — its `pool_session` name.
    pub token: String,
    /// The attach command's exit status. `None` from a reporter that predates
    /// the field (or couldn't determine one), which reads as "unknown" and is
    /// treated as a clean end.
    #[serde(default)]
    pub status: Option<i32>,
    /// How many seconds the attach itself ran, measured by the wrapper around
    /// the attach command. The dashboard also knows how long the *binding*
    /// lived, but that is an `Instant` — CLOCK_MONOTONIC, which does not
    /// advance while the machine is suspended, so a laptop that slept through
    /// an eight-hour attach reports minutes. This is wall clock, from the one
    /// process that watched the attach start and finish, so it is what the
    /// refused-on-arrival test should read. `None` from a reporter that
    /// predates the field; the binding's age is the fallback.
    #[serde(default)]
    pub held_secs: Option<u64>,
}

/// `detach-{reporter pid}.flag`, in the sessions dir. The `.flag` extension
/// keeps it invisible to [`read_all_launcher_states`] (which reads `*.json`),
/// exactly like the bell sentinels; the pid only has to make the *name* unique
/// among concurrent reporters, since the identity that matters is in the body.
fn detach_report_path(dir: &Path, reporter_pid: u32) -> PathBuf {
    dir.join(format!("detach-{reporter_pid}.flag"))
}

/// Drop the sentinel for `(host, token)`. Best-effort by nature: the report is
/// a courtesy that makes the dashboard prompt, and the periodic prune is what
/// makes it *correct*, so a failure here costs latency and nothing else.
pub fn write_detach_report(host: &str, token: &str, status: Option<i32>, held_secs: Option<u64>) {
    write_detach_report_in(&sessions_dir(), host, token, status, held_secs);
}

/// [`write_detach_report`] against an explicit directory, so the round trip is
/// testable without redirecting the whole process's state dir.
fn write_detach_report_in(
    dir: &Path,
    host: &str,
    token: &str,
    status: Option<i32>,
    held_secs: Option<u64>,
) {
    let _ = create_dir_all_private(dir);
    let path = detach_report_path(dir, std::process::id());
    let report = DetachReport {
        host: host.to_string(),
        token: token.to_string(),
        status,
        held_secs,
    };
    if let Err(e) = write_json_atomic(&path, &report) {
        tracing::debug!("could not write detach report {}: {e}", path.display());
    }
}

/// Pop every detach report currently present. Drained in one pass like the bell
/// sentinels, so a burst (a host dropping five attach windows at once) costs one
/// readdir. An unparseable sentinel is still removed — it can only ever be a
/// torn write, and leaving it would make the dashboard re-read it forever.
pub fn drain_detach_reports() -> Vec<DetachReport> {
    drain_detach_reports_in(&sessions_dir())
}

fn drain_detach_reports_in(dir: &Path) -> Vec<DetachReport> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_report = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("detach-") && n.ends_with(".flag"));
        if !is_report {
            continue;
        }
        let report: Option<DetachReport> = read_json(&path);
        let _ = std::fs::remove_file(&path);
        if let Some(report) = report {
            reports.push(report);
        }
    }
    reports
}

pub fn read_all_launcher_states() -> Vec<LauncherState> {
    let dir = sessions_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut states = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Skip (don't delete) on a parse error: a reader must never destroy
        // data across version skew (e.g. the historical `cwd: Option<String>`
        // → `String` change), which could wipe a live session's state file.
        // Removal requires a successful parse *and* a confirmed-dead pid below.
        let state: LauncherState = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if !is_process_alive(state.launcher_pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }

        states.push(state);
    }

    states.sort_by_key(|s| s.launcher_pid);
    states
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The attach-guard predicate: only a state file whose `pool_session`
    /// matches AND whose launcher pid is alive counts — a matching name on a
    /// dead launcher is exactly the resurrection case the guard exists for.
    #[test]
    fn find_live_pool_session_requires_matching_name_and_live_pid() {
        let mk = |pid: u32, pool: Option<&str>| LauncherState {
            agent: crate::agent::AgentControl::Claude,
            launcher_pid: pid,
            session_id: None,
            window_id: None,
            tab_id: None,
            cwd: String::new(),
            status: SessionStatus::Idle,
            last_tool: None,
            updated_at: 0,
            active_since: None,
            last_prompt: None,
            child_pid: None,
            last_error: None,
            context_tokens: None,
            model: None,
            name: None,
            first_prompt: None,
            pool_session: pool.map(str::to_string),
            launch_id: None,
            terminal: None,
            terminfo: None,
            flags: None,
            attached: None,
            host: HostId::default(),
        };
        let states = vec![
            mk(10, None),                  // local session, no pool name
            mk(20, Some("cm-claude-1-1")), // dead launcher (alive=false below)
            mk(30, Some("cm-codex-2-1")),  // live, different name
            mk(40, Some("cm-claude-1-1")), // live match
        ];
        let alive = |pid: u32| pid >= 30;
        let hit = find_live_pool_session(&states, "cm-claude-1-1", alive);
        assert_eq!(hit.map(|s| s.launcher_pid), Some(40));
        assert!(find_live_pool_session(&states, "cm-codex-9-9", alive).is_none());
        // Liveness is judged per matching state file, not globally.
        assert!(find_live_pool_session(&states, "cm-claude-1-1", |pid| pid == 20).is_some());
        // The name exists but only on dead launchers → no hit (resurrection).
        assert!(find_live_pool_session(&states, "cm-claude-1-1", |_| false).is_none());
    }

    /// `term` was added late, so it has to be additive in both directions: a
    /// state file (or wire frame) from a host that predates it must still
    /// decode, and an absent value must read as "unknown" rather than as a
    /// terminfo named "". The field is also skipped when empty, so an old peer
    /// never sees it at all.
    #[test]
    fn a_state_without_a_term_still_decodes() {
        let old = r#"{"agent":"claude","launcher_pid":7,"cwd":"/home/miao/p",
            "status":"idle","updated_at":0}"#;
        let s: LauncherState = serde_json::from_str(old).expect("old state decodes");
        assert_eq!(s.terminfo, None);
        assert_eq!(s.launcher_pid, 7);

        let encoded = serde_json::to_string(&s).expect("encodes");
        assert!(
            !encoded.contains("\"term\""),
            "an unknown term must not go on the wire at all: {encoded}"
        );
        let with_term = LauncherState {
            terminfo: Some("xterm-kitty".into()),
            ..s
        };
        let round: LauncherState =
            serde_json::from_str(&serde_json::to_string(&with_term).expect("encodes"))
                .expect("decodes");
        assert_eq!(round.terminfo.as_deref(), Some("xterm-kitty"));
    }

    /// The detach sentinel is the whole event path's payload, so it has to
    /// survive the round trip and then *be gone* — a report read twice would
    /// retire a binding the user has since re-attached.
    #[test]
    fn detach_reports_round_trip_and_drain_once() {
        let dir = std::env::temp_dir().join(format!("cm-detach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            drain_detach_reports_in(&dir).is_empty(),
            "no dir, no reports"
        );
        write_detach_report_in(&dir, "box", "cm-claude-7-1", Some(0), Some(42));
        assert_eq!(
            drain_detach_reports_in(&dir),
            vec![DetachReport {
                host: "box".into(),
                token: "cm-claude-7-1".into(),
                status: Some(0),
                held_secs: Some(42),
            }]
        );
        assert!(drain_detach_reports_in(&dir).is_empty(), "drained once");

        // A torn write is removed rather than re-read forever, and doesn't take
        // a good sentinel down with it.
        std::fs::write(dir.join("detach-999999.flag"), b"{ truncated").unwrap();
        write_detach_report_in(&dir, "box", "cm-2", Some(255), Some(1));
        let drained = drain_detach_reports_in(&dir);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].token, "cm-2");
        assert!(drain_detach_reports_in(&dir).is_empty());

        // The sentinel must stay invisible to the state-file reader: it shares
        // the sessions dir with `{pid}.json`, and a `.flag` is not a session.
        write_detach_report_in(&dir, "box", "cm-3", None, None);
        let jsons = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .count();
        assert_eq!(jsons, 0);
    }

    /// State files carry the user's prompt text, so they must not be readable by
    /// other users on a shared machine — regardless of the ambient umask (the
    /// common 022 would otherwise yield 0755/0644).
    #[test]
    fn state_dirs_and_files_are_owner_only() {
        let root = std::env::temp_dir().join(format!("cm-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("a/b");
        create_dir_all_private(&nested).unwrap();

        // Every component create_dir_all_private made, not just the leaf.
        for d in [&root, &root.join("a"), &nested] {
            let mode = std::fs::metadata(d).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "{} is group/other-accessible: {mode:o}",
                d.display()
            );
        }

        let f = nested.join("s.json");
        write_json_atomic(&f, &"prompt text").unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "state file is group/other-readable: {mode:o}"
        );
        // The `.tmp` staging file must not survive the rename.
        assert!(!f.with_extension("tmp").exists());

        // An overwrite of a pre-existing 0644 file (written by an older build)
        // is re-hardened, not left as-is.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_json_atomic(&f, &"second write").unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "rewrite left it readable: {mode:o}");

        // A pre-existing world-readable dir is tightened in place.
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        harden_dir(&nested);
        let mode = std::fs::metadata(&nested).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "harden_dir left it open: {mode:o}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snippet_passes_short_text_through_unchanged() {
        assert_eq!(snippet("hello", 120), "hello");
        // Exactly at the limit is not truncated.
        assert_eq!(snippet("abcde", 5), "abcde");
    }

    #[test]
    fn dashboard_window_id_round_trips_with_and_without_identity() {
        let wid = WindowId("42".into());
        // With an identity: `<identity>|<window-id>`, and parse recovers both.
        let s = format_dashboard_window_id(Some("zellij:work"), &wid);
        assert_eq!(s, "zellij:work|42");
        assert_eq!(
            parse_dashboard_window_id(&s),
            (Some("zellij:work".into()), wid.clone())
        );
        // Without one: a bare window id, parsed back with no identity.
        let s = format_dashboard_window_id(None, &wid);
        assert_eq!(s, "42");
        assert_eq!(parse_dashboard_window_id(&s), (None, wid.clone()));
        // An identity containing `|` still parses (split on the last one — the
        // window id never contains a `|`).
        assert_eq!(
            parse_dashboard_window_id("kitty:unix:/a|b|42"),
            (Some("kitty:unix:/a|b".into()), wid)
        );
    }

    #[test]
    fn bell_target_matches_only_same_terminal_local_binding() {
        let bind = |win: &str, host: &str, pid: u32, term: Option<&str>| WindowBinding {
            window_id: WindowId(win.into()),
            host: host.into(),
            launcher_pid: pid,
            token: format!("t{pid}"),
            terminal: term.map(str::to_string),
        };
        // Same window id "3" under two different terminal instances plus a remote
        // row: only the local binding whose terminal matches the caller is rung.
        let bindings = vec![
            bind("3", "local", 10, Some("kitty:999")),
            bind("3", "local", 20, Some("zellij:work")),
            bind("3", "box", 30, Some("kitty:999")),
        ];
        assert_eq!(
            bell_target_pid(&bindings, &WindowId("3".into()), Some("kitty:999")),
            Some(10)
        );
        assert_eq!(
            bell_target_pid(&bindings, &WindowId("3".into()), Some("zellij:work")),
            Some(20)
        );
        // An identity the file doesn't carry (foreign focus process) rings nobody.
        assert_eq!(
            bell_target_pid(&bindings, &WindowId("3".into()), Some("kitty:other")),
            None
        );
        // A None-identity caller matches only None-identity bindings.
        assert_eq!(
            bell_target_pid(&bindings, &WindowId("3".into()), None),
            None
        );
    }

    #[test]
    fn review_pending_is_an_attention_state_not_a_busy_one() {
        // The agent is blocked on a human review — it floats to the attention
        // sort rank (`s` jumps to it) but must NOT keep the machine awake, so it
        // is `needs_attention()` yet not `is_busy()` (mirroring the Waiting*
        // states and its at-rest sibling `BackgroundServer`, unlike the busy
        // `BackgroundActive`).
        assert!(SessionStatus::ReviewPending.needs_attention());
        assert!(!SessionStatus::ReviewPending.is_busy());
        assert_eq!(SessionStatus::ReviewPending.label(), "Review");
        // Listed in ALL so the Status column widths itself correctly.
        assert!(SessionStatus::ALL.contains(&SessionStatus::ReviewPending));
    }

    #[test]
    fn background_task_is_busy_but_a_parked_server_is_at_rest() {
        // A short-term background task the agent is waiting on is genuine work:
        // busy (keeps the machine awake), green "Task".
        assert!(SessionStatus::BackgroundActive.is_busy());
        assert!(!SessionStatus::BackgroundActive.needs_attention());
        assert_eq!(SessionStatus::BackgroundActive.label(), "Task");
        // A parked long-running dev server/watcher is not the agent working:
        // at-rest (never keeps the machine awake), yellow "Server". It draws
        // attention via the follow-up bell (armed on entry), not needs_attention.
        assert!(!SessionStatus::BackgroundServer.is_busy());
        assert!(!SessionStatus::BackgroundServer.needs_attention());
        assert_eq!(SessionStatus::BackgroundServer.label(), "Server");
        assert!(SessionStatus::ALL.contains(&SessionStatus::BackgroundServer));
    }

    #[test]
    fn snippet_truncates_on_char_boundary_with_ellipsis() {
        assert_eq!(snippet("abcdef", 3), "abc…");
        // Multi-byte chars are counted by char, not byte, and never split.
        assert_eq!(snippet("héllo wörld", 5), "héllo…");
    }

    #[test]
    fn snippet_preserves_newlines_within_budget() {
        assert_eq!(snippet("a\nb\nc", 10), "a\nb\nc");
    }

    /// The ssh `ControlPath` overflowed macOS's ~104-byte `sockaddr_un` limit
    /// when nested under `runtime_dir` (the doubled `captain-miao-<uid>/
    /// captain-miao` on a ~49-char `/var/folders/...` $TMPDIR), so `ssh` failed
    /// with "path too long for Unix domain socket" and the host showed
    /// unreachable. Pin the control path short enough to survive that *plus*
    /// ssh's own ~17-char ControlMaster suffix, well under the 104 cap.
    #[test]
    fn ssh_control_path_fits_the_socket_limit() {
        // A deliberately long, realistic ssh target.
        let target = "deploy-user@build-box-42.internal.example.com";
        let path = ssh_control_path(target).to_string_lossy().into_owned();
        // 17 = ssh's ".<16 random chars>" ControlMaster suffix; 104 = macOS cap.
        assert!(
            path.len() + 17 < 104,
            "ControlPath too long ({} + 17 suffix): {path}",
            path.len()
        );
        // The forward socket (explicit path, no ssh suffix) must also fit.
        let fwd = remote_forward_sock("build-box-42.internal.example.com")
            .to_string_lossy()
            .into_owned();
        assert!(
            fwd.len() < 104,
            "forward socket too long ({}): {fwd}",
            fwd.len()
        );
    }

    /// Runtime files must never land back in `$TMPDIR`. macOS reaps that tree on
    /// a periodic sweep, and a launcher's hook socket removed from under a live
    /// session takes the session's whole status pipeline with it, silently: the
    /// listener stays bound to an unlinked inode, every hook fails to connect,
    /// and the dashboard row freezes at whatever it last said for as long as the
    /// session keeps running.
    ///
    /// Asserted on whichever branch this platform actually takes, so the macOS
    /// fallback — the one that broke — is covered where it is used, and the
    /// hook-socket path is checked against the same `sockaddr_un` cap as ssh's.
    #[test]
    fn runtime_files_never_live_in_the_reaped_temp_tree() {
        let dir = runtime_dir();
        match std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(xdg) => assert_eq!(dir, PathBuf::from(xdg).join("captain-miao")),
            None => assert_eq!(dir, state_dir().join("run")),
        }
        assert!(
            !dir.starts_with(std::env::temp_dir()),
            "runtime dir is back under the OS temp tree: {}",
            dir.display()
        );
        // A launcher socket is `<runtime>/launchers/<pid>.sock`; it binds like
        // any other and is subject to the same cap.
        let sock = dir.join("launchers").join("4294967295.sock");
        assert!(
            sock.to_string_lossy().len() < 104,
            "launcher socket path too long ({}): {}",
            sock.to_string_lossy().len(),
            sock.display()
        );
    }
}
