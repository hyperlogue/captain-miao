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

pub fn runtime_dir() -> PathBuf {
    // XDG_RUNTIME_DIR is already per-user; when it's unset (or empty, which
    // the XDG spec treats as unset) we fall back to the shared, world-writable
    // temp dir, so namespace by uid to keep two users from colliding (and to
    // avoid binding the launcher's control socket in a path another user could
    // hijack).
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("captain-miao-{uid}"))
        })
        .join("captain-miao")
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
/// `ControlMaster` appends a ~17-char random suffix to `ControlPath`, and macOS
/// caps a `sockaddr_un` path near 104 bytes while its `$TMPDIR` is already a
/// ~49-char `/var/folders/...` path. The general [`runtime_dir`] nests
/// `captain-miao-<uid>/captain-miao` (30 chars) under that, which pushes a
/// hashed ControlPath + ssh's suffix to ~109 bytes — over the limit. So ssh
/// sockets get their own flat `cm-<uid>` dir (no doubled app name) instead.
/// `$XDG_RUNTIME_DIR` (short on Linux: `/run/user/<uid>`) is used when present.
pub fn ssh_sock_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|d| PathBuf::from(d).join("cm"))
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
/// `captain-miao focus --window-id` bell keybind resolves a window→pid through it
/// (replacing the old state-file scan), and the prune loop garbage-collects it.
/// Safe to delete (regenerated next reload).
pub fn window_bindings_path() -> PathBuf {
    state_dir().join("window-bindings.json")
}

// -- Process utilities --

pub fn is_process_alive(pid: u32) -> bool {
    // `kill(pid, 0)` returns 0 when the signal could be sent, but -1/EPERM
    // when the process exists yet is owned by another user. Treat EPERM as
    // alive so we never delete or "restart" a live session's state.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
    /// Which host this session lives on. Never serialized — the launcher and
    /// server don't know it; the dashboard stamps it during reload so per-row
    /// keying can disambiguate a remote pid from a local one. Defaults `local`.
    #[serde(skip)]
    pub host: HostId,
}

impl LauncherState {
    fn file_path(launcher_pid: u32) -> PathBuf {
        sessions_dir().join(format!("{launcher_pid}.json"))
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
/// `captain-miao focus --window-id <id>` so external Kitty keybinds can ring
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
}
