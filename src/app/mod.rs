mod bindings;
mod draw;
mod format;
mod hosts;
mod keybind_log;
mod keymap;
mod keys;
mod logo;
mod picker;
mod run;

pub use run::{read_dashboard_window_id, run};

use ratatui::layout::Rect;
use ratatui::widgets::TableState;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::agent::{AgentControl, ResumeCandidate, SessionIndex};
use crate::state::{self, HostId, LauncherState, SessionStatus};
use crate::terminal::{Capabilities, SessionsLayout, Tab, TabId, TabInfo, TabTarget, WindowId};

use self::format::{
    collapse_tilde, contains_ci, expand_tilde, format_coarse_age, format_relative_time,
    random_session_name, workdir_picker_title,
};
use self::picker::{Picker, PickerItem};
use crate::backend::{Backend, ConnState, RemoteBackend, Transport};

/// Whether remote (SSH) host support is compiled in — the `remote` cargo
/// feature, **off by default** because the feature is a work in progress
/// (`docs/remote-sessions.md`): the lifecycle is implemented but unverified
/// end-to-end against a real remote host, and restart/fork remain local-only.
///
/// A runtime const rather than `#[cfg]` scattered across the dashboard: the
/// remote code compiles in either configuration (so it stays type-checked and
/// tested), and this gate closes the only two doors that reach it — reading
/// `hosts.json` to build `Backend::Remote`s, and the `Space h` hosts editor.
/// With it false the dashboard is strictly local-only.
pub(super) const REMOTE_ENABLED: bool = cfg!(feature = "remote");

/// How long a `pending_focus_window` target may go unclaimed before
/// `reload_sessions` drops it. Generous — a slow launcher can take seconds to
/// write its first state file; the age-out only guards the pathological case
/// where the launcher dies first and a later, unrelated window reuses the id.
const PENDING_FOCUS_MAX_AGE: Duration = Duration::from_secs(30);

// -- Input mode --

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum InputMode {
    Normal,
    Search,
    Picker,
    Help,
    Confirm,
    /// Modal popup for editing the icon + color of the selected session's
    /// directory. See `DirEditState`.
    DirEdit,
    /// Modal popup for managing remote hosts. See `HostEditState`.
    HostEdit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum DragTarget {
    VerticalSplit,
    HorizontalSplit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum SessionFlag {
    Mute,
    Pin,
    FollowUp,
}

#[derive(Debug)]
pub(super) enum Action {
    FocusWindow(WindowId),
    NewSessionSplit {
        agent: AgentControl,
        cwd: String,
        /// Host to open on (local unless chosen with `Ctrl-h` in the picker).
        host: HostId,
    },
    FetchTabsForMove(WindowId),
    MoveWindow(WindowId, TabTarget),
    FetchResumeList,
    /// Gather running (across all hosts) + resumable (cross-host walk) sessions
    /// and open the unified browser picker.
    FetchBrowser,
    ResumeSession {
        agent: AgentControl,
        cwd: String,
        session_id: String,
        fork: bool,
        /// Host the session lives on; resume opens it there (local, or the
        /// remote's pty pool).
        host: HostId,
    },
    KillSession {
        host: HostId,
        child_pid: u32,
        window_id: Option<WindowId>,
    },
    /// Detach from a remote session: close its local `ssh attach` window but
    /// leave the pooled session running on the host (no kill). The binding is
    /// keyed by `(host, token=pool_session)`; the row stays and `Enter`
    /// re-attaches.
    DetachRemote {
        host: HostId,
        token: String,
        window_id: WindowId,
    },
    /// Switch to the `(host, cwd)` work tab, creating it if needed: local opens
    /// an in-process shell tab; a remote host opens an `ssh -t <target>` tab
    /// that cds into `cwd`. The tab is recorded in [`App::work_tabs`] so the
    /// next `w` on the same cwd switches back instead of spawning another.
    OpenShellTab {
        host: HostId,
        cwd: String,
    },
    /// Kill the existing agent process for one session and re-launch it with
    /// `--resume <session_id>` adjacent to the original window. Used to pick
    /// up out-of-process changes (e.g. agent binary upgrade, .envrc edits).
    RestartSession(RestartSpec),
    /// Restart every supplied session. The dashboard pre-filters this list to
    /// idle sessions only.
    RestartAll {
        sessions: Vec<RestartSpec>,
    },
    /// Copy the selected session's id to the system clipboard (via OSC 52).
    CopySessionId(String),
    /// Attach a local window to an already-running remote pool session (§5):
    /// spawn `ssh -t <host> captain-miao attach <pool_session>` and bind it.
    AttachRemoteRunning {
        host: HostId,
        pool_session: String,
    },
}

/// Inputs needed to restart a single session: kill the old child, then launch
/// a new captain-miao launcher with `--resume <session_id>` next to the old
/// window. `agent` is captured so the restart targets the same backend the
/// session was originally launched under.
#[derive(Debug, Clone)]
pub(super) struct RestartSpec {
    pub(super) agent: AgentControl,
    pub(super) child_pid: u32,
    pub(super) window_id: WindowId,
    pub(super) cwd: String,
    pub(super) session_id: String,
    /// Status flags to re-apply once the relaunched session appears under its
    /// new launcher pid. Default (all-false) means nothing to carry over.
    pub(super) flags: SessionFlags,
    /// Whether `restart_one` should SIGTERM the old `child_pid` and close
    /// `window_id` after relaunching. True for user-initiated restarts (the
    /// session is live, so the old child must be torn down). False for the
    /// crash-recovery path: the launcher_pid is already known dead, so the
    /// child_pid is gone too and may have been recycled by an unrelated
    /// process — and a relaunched kitty reissues small window ids that can
    /// collide with a live unrelated window. Signaling/closing in that case
    /// would kill a recycled pid and shut an innocent window.
    pub(super) kill_old: bool,
}

impl Action {
    /// Stable variant name used as the action column in `keybinds.log`.
    /// Hand-rolled so frequency analysis (`cut -f6 | sort | uniq -c`) gets
    /// clean values that don't change with `Debug` formatting tweaks.
    pub(super) fn name(&self) -> &'static str {
        match self {
            Action::FocusWindow(_) => "FocusWindow",
            Action::NewSessionSplit { .. } => "NewSessionSplit",
            Action::FetchTabsForMove(_) => "FetchTabsForMove",
            Action::MoveWindow(_, _) => "MoveWindow",
            Action::FetchResumeList => "FetchResumeList",
            Action::FetchBrowser => "FetchBrowser",
            Action::ResumeSession { .. } => "ResumeSession",
            Action::KillSession { .. } => "KillSession",
            Action::DetachRemote { .. } => "DetachRemote",
            Action::OpenShellTab { .. } => "OpenShellTab",
            Action::RestartSession(_) => "RestartSession",
            Action::RestartAll { .. } => "RestartAll",
            Action::CopySessionId(_) => "CopySessionId",
            Action::AttachRemoteRunning { .. } => "AttachRemoteRunning",
        }
    }
}

/// Interpretation of the current picker's submission. Holds the raw list the
/// picker items were built from so the picker's `Submit(idx)` event can be
/// mapped back to a concrete `Action`.
#[derive(Debug)]
pub(super) enum PickerKind {
    /// Move `window_id` into one of `tabs` — or into a new tab if the user
    /// picks the trailing "[New Tab]" synthetic entry.
    MoveTab {
        window_id: WindowId,
        tabs: Vec<TabInfo>,
    },
    /// Resume one of the listed sessions, each tagged with the host it lives on
    /// (cross-host: the picker unions every backend's resumable list).
    Resume {
        candidates: Vec<(HostId, ResumeCandidate)>,
    },
    /// Launch a new session. The picker shows recent cwds and also accepts
    /// a free-form path; Tab completes against the filesystem. `agent` is the
    /// backend this launch will use — seeded from the persistent default and
    /// overridable in-picker with `Ctrl-t` (per-launch only).
    Workdir {
        agent: AgentControl,
        /// Host this launch will open on — local by default, cycled in-picker
        /// with `Ctrl-h` (per-launch only). A remote host opens the session in
        /// that host's pty pool and attaches over ssh (§8).
        host: HostId,
    },
    /// Set the persistent default backend for new sessions (`Space a`).
    DefaultAgent,
    /// Cross-host browser (§5): every running session (focus/attach) and every
    /// resumable one (resume), across all hosts, in one searchable list.
    Browser { entries: Vec<BrowserEntry> },
    /// Pick an emoji to drop into the directory-mark editor's icon field.
    /// Opened with `Ctrl-E` from `Space i`; submit/cancel return to the editor
    /// (which stays live in `self.dir_edit`) rather than the normal view.
    Emoji,
}

/// One row of the cross-host browser. A running session is focused or attached;
/// a resumable one is resumed on its host.
#[derive(Debug)]
pub(super) enum BrowserEntry {
    /// A live session (local or remote). Carries its full state so submit can
    /// reuse the same focus-or-attach decision as `Enter` on a dashboard row.
    Running(Box<LauncherState>),
    /// A dormant session on `host`, resumable.
    Resumable(HostId, ResumeCandidate),
}

#[derive(Debug)]
pub(super) struct ActivePicker {
    pub(in crate::app) picker: Picker,
    pub(in crate::app) kind: PickerKind,
}

/// Persisted dashboard overrides (pin/mute/needs-input) so they survive restarts.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DashboardOverrides {
    #[serde(default)]
    muted: Vec<u32>,
    #[serde(default)]
    pinned: Vec<u32>,
    #[serde(default)]
    follow_up: Vec<u32>,
    /// Whether the OS-sleep inhibitor (Space z) is enabled. `None` for
    /// overrides files written before this field existed; the dashboard
    /// keeps its compiled-in default in that case.
    #[serde(default)]
    prevent_sleep: Option<bool>,
    /// Persisted default backend for new sessions (Space a), stored as the
    /// CLI subcommand (`"claude"` / `"codex"`). `None` for overrides written
    /// before this field existed; the `[launcher] default_agent` config value
    /// is kept in that case.
    #[serde(default)]
    default_agent: Option<String>,
    /// Persisted session layout (Space l), stored as its label (`"stacked"` /
    /// `"per-tab"`). `None` for overrides written before this field existed; the
    /// `[terminal] sessions_layout` config value is kept in that case.
    #[serde(default)]
    sessions_layout: Option<String>,
}

/// One row of the dashboard's session snapshot — everything `restart_one`
/// needs to relaunch a session whose launcher process is no longer alive.
/// Only sessions that already have a window id and a live session id are
/// snapshotted; everything else is unrestartable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SessionSnapshotEntry {
    /// Backend the original session ran under. Defaulted to Claude so
    /// snapshots written before this field existed still parse.
    #[serde(default)]
    pub agent: AgentControl,
    pub launcher_pid: u32,
    pub child_pid: u32,
    pub window_id: WindowId,
    pub cwd: String,
    pub session_id: String,
    /// Status flags (pinned / muted / follow-up) the session carried at
    /// snapshot time, so a crash-recovery restart can re-adopt them. Defaulted
    /// for snapshots written before this field existed.
    #[serde(default)]
    pub flags: SessionFlags,
}

/// On-disk override of the (icon, color) pair for a directory. Persisted in
/// `directory-marks.json`; absent paths fall back to a deterministic default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DirectoryMark {
    pub icon: String,
    pub color: String,
}

/// On-disk wrapper for `directory_marks`. Kept as a versioned envelope so
/// future fields (e.g. an explicit format version) can be added without
/// breaking users' saved data.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct DirectoryMarks {
    #[serde(default)]
    pub marks: HashMap<String, DirectoryMark>,
}

/// The live identity of a `(host, cwd)` work tab: the tab captain-miao spawned
/// plus the id of the window it created inside it. The window id pins the tab's
/// identity beyond its recycled-prone id + title — zellij pane ids never recycle,
/// so a matching window inside the tab proves it's the same tab, not an impostor
/// that inherited a closed tab's number. `None` for entries seeded from a
/// pre-window-id `work-tabs.json` (they fall back to id + title validation).
#[derive(Debug, Clone)]
pub(super) struct WorkTab {
    pub tab_id: TabId,
    pub window_id: Option<WindowId>,
}

/// One persisted work-tab binding — the `(host, cwd) → tab` mapping `w` records.
/// Serialized as a flat list because a `(HostId, String)` map key can't be a
/// JSON object key. See [`App::work_tabs`] and `work_tabs_path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WorkTabEntry {
    pub host: String,
    pub cwd: String,
    pub tab_id: TabId,
    /// Absent in work-tabs.json written before the window id joined the
    /// identity; such entries deserialize with `None` and keep working under
    /// the legacy id + title validation.
    #[serde(default)]
    pub window_id: Option<WindowId>,
}

/// Active state for the directory-mark popup editor. `Some` iff
/// `input_mode == InputMode::DirEdit`.
#[derive(Debug)]
pub(super) struct DirEditState {
    pub(in crate::app) cwd: String,
    pub(in crate::app) color_idx: usize,
    pub(in crate::app) custom: self::picker::TextInput,
    pub(in crate::app) focus: DirEditFocus,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum DirEditFocus {
    Custom,
    Color,
}

/// Active hosts popup (`input_mode == InputMode::HostEdit`). A working copy of
/// the host list edited in place; committed (and the backends rebuilt) on save,
/// discarded on cancel.
#[derive(Debug)]
pub(super) struct HostEditState {
    pub(in crate::app) rows: Vec<HostRow>,
    /// Selected row (`0..rows.len()`), or `rows.len()` for the "+ add" line.
    pub(in crate::app) cursor: usize,
    /// `true` while editing the selected row's fields; `false` in the list.
    pub(in crate::app) editing: bool,
    pub(in crate::app) focus: HostField,
}

/// One editable host row in the popup.
#[derive(Debug, Clone, Default)]
pub(super) struct HostRow {
    pub(in crate::app) label: String,
    /// ssh target (`user@host`) or, when `is_socket`, a socket path.
    pub(in crate::app) target: String,
    pub(in crate::app) is_socket: bool,
    pub(in crate::app) color_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum HostField {
    Label,
    Target,
    Color,
}

/// A pending y/N confirmation. Set when the user invokes a destructive
/// action (e.g. restart) — the action is fired only after Enter / y / Y.
#[derive(Debug)]
pub(super) struct PendingConfirm {
    pub(super) prompt: String,
    pub(super) action: Action,
}

/// Cached Tab-completion list for the workdir picker. Stored per-App so
/// repeated Tabs can cycle through the same options without re-reading the
/// parent directory (the current text after a completion ends with `/`, which
/// would otherwise switch the parent).
#[derive(Debug)]
pub(super) struct WorkdirCompletion {
    pub matches: Vec<String>,
    pub index: usize,
}

// -- App --

pub(super) struct App {
    pub(super) sessions: Vec<LauncherState>,
    pub(super) table_state: TableState,
    pub(super) should_quit: bool,
    pub(super) home_dir: String,
    pub(super) status_msg: Option<String>,
    pub(super) status_is_error: bool,
    pub(super) input_mode: InputMode,
    /// The Search-mode (`/`) text buffer: a cursor-aware input with readline
    /// editing, shared with the pickers. Only meaningful while `input_mode ==
    /// Search`. `pub(in crate::app)` to match `TextInput`'s visibility.
    pub(in crate::app) search_input: self::picker::TextInput,
    pub(super) search_filter: Option<String>,
    pub(super) pending_g: bool,
    /// Pending leader/prefix chord: `Some` after a prefix key (e.g. `Space`)
    /// is pressed in Normal mode, cleared by the next keypress. The following
    /// key either completes a two-chord binding or is swallowed. See
    /// `handle_normal_key`.
    pub(super) pending_prefix: Option<keymap::Chord>,
    /// Configurable Normal-mode keymap, built once from `[keybinds]` config.
    pub(super) keymap: keymap::Keymap,
    /// The backend's `Capabilities`, read once at startup (it's a process-wide
    /// constant) — every consumer reads this cache, never
    /// `terminal::get().capabilities()` again. `move_to_tab` gates the `t`
    /// command and hides its help/footer hint; `window_stacking` decides
    /// whether a session spawn anchors next to a window or gets its own tab.
    pub(super) capabilities: Capabilities,
    /// Backend used when starting a new session (`o` / `O`). Seeded from
    /// `launcher.default_agent`, cycled with `Space a`.
    pub(super) new_session_agent: AgentControl,
    /// How new sessions are arranged (`resolve_spawn_target`): the shared
    /// `cm:sessions` tab (Stacked) or one tab per session (Per-tab). Seeded from
    /// `[terminal] sessions_layout`, toggled with `Space l`, persisted. A
    /// spawn-time policy only — existing sessions migrate via `Space e`/`Space E`.
    pub(super) sessions_layout: SessionsLayout,
    pub(super) preview_text: Option<String>,
    /// Parsed `preview_text` cached so repeated redraws (e.g. an active
    /// session's churn) skip the ANSI parse. Cleared by `set_preview_text`
    /// every time the raw text is replaced.
    pub(super) preview_lines: Option<Vec<ratatui::text::Line<'static>>>,
    /// Widest cached preview line, in display cells. Measured once when
    /// `preview_lines` is filled so `draw_preview` doesn't unicode-width-scan
    /// the whole (~2000 line) cache every frame. Reset to 0 alongside the cache
    /// in `set_preview_text`; the placeholder branch computes its width inline.
    pub(super) preview_max_width: usize,
    pub(super) preview_window_id: Option<WindowId>,
    pub(super) preview_dirty_since: Option<Instant>,
    pub(super) preview_scroll: usize,
    /// Horizontal scroll for the preview panel, in cells. Useful when the
    /// previewed session's terminal is wider than the dashboard view.
    pub(super) preview_h_scroll: u16,
    /// Whether the dashboard's terminal window has focus, tracked via the
    /// terminal's focus-reporting events. Starts true: the dashboard launches
    /// into the foreground window, and a terminal without focus reporting
    /// never sends either event, so the auto-refresh still works there.
    pub(super) focused: bool,
    /// When the last preview fetch was *attempted* (success or failure) —
    /// the base for the periodic auto-refresh timer. Attempt-time rather
    /// than success-time so a window whose `get_text` fails is retried once
    /// per interval, not once per debounce window.
    pub(super) preview_fetched_at: Option<Instant>,
    /// When the displayed preview content last actually changed (successful
    /// fetches only — kept by `set_preview_text`). Drives the "updated Ns
    /// ago" staleness label; tracking success rather than attempt keeps the
    /// label growing when a re-fetch fails instead of lying that it's fresh.
    pub(super) preview_updated_at: Option<Instant>,
    /// Active picker popup (move-tab or resume) — `Some` iff
    /// `input_mode == InputMode::Picker`.
    pub(super) picker: Option<ActivePicker>,
    /// A queued action waiting on a y/N confirmation. `Some` iff
    /// `input_mode == InputMode::Confirm`.
    pub(super) pending_confirm: Option<PendingConfirm>,
    /// Active directory-mark popup. `Some` iff `input_mode == InputMode::DirEdit`.
    pub(super) dir_edit: Option<DirEditState>,
    /// Active hosts popup. `Some` iff `input_mode == InputMode::HostEdit`.
    pub(super) host_edit: Option<HostEditState>,
    /// Persisted (icon, color) overrides keyed by canonicalized cwd. Loaded
    /// once at startup and saved on every popup confirmation.
    pub(super) directory_marks: HashMap<String, DirectoryMark>,
    /// Recent cwds used for launching sessions, most-recent first. Populated
    /// from disk on startup and pushed-to whenever the user launches a new
    /// session or resumes one. Shown as suggestions in the workdir picker.
    pub(super) recent_cwds: Vec<String>,
    /// Cached filesystem completions for the workdir picker so repeated Tab
    /// presses cycle through the same list rather than re-reading directories
    /// (which would fail once the text is completed past the original prefix).
    pub(super) workdir_completion: Option<WorkdirCompletion>,
    /// `$HOME` of the workdir picker's currently-selected host, for `~`
    /// display/expansion. Local home by default; refreshed to the remote host's
    /// home when `Ctrl-h` switches the picker to a remote (so completion and
    /// validation resolve against *that* machine). Empty ⇒ no `~` handling.
    pub(super) workdir_host_home: String,
    /// A window a just-spawned session should get selection on once it appears,
    /// with the instant it was set. `reload_sessions` selects the matching row
    /// then clears it — but if the launcher dies before writing a state file the
    /// id would linger, so the instant lets an unclaimed target age out (see
    /// [`PENDING_FOCUS_MAX_AGE`]).
    pub(super) pending_focus_window: Option<(WindowId, Instant)>,
    /// Local windows whose launch just failed (`FailedToStart`), queued by
    /// `reload_sessions` for the run loop to bring to the foreground. The
    /// launcher can't focus its own window (it may be headless/remote — window
    /// control lives here, in the presentation layer), so it records the failed
    /// row and the dashboard focuses the held error window once, on the
    /// transition into `FailedToStart`.
    pub(super) failed_launch_focus_queue: Vec<WindowId>,
    /// Local windows to close because their session row departed without a clean
    /// kill (its state file vanished: crash, SIGKILL, or the file removed). Filled
    /// by `reload_sessions` (row removal) and the startup binding seed (dead-local
    /// -pid bindings), drained by the run loop, which calls `close_window`
    /// best-effort. Only populated on a `floating_sessions` backend (zellij), where
    /// a `hold: true` session's exited pane is an invisible leak buried in the
    /// shared sessions tab; on kitty the held window stays visible as crash
    /// forensics, so nothing is queued.
    pub(super) reap_window_queue: Vec<WindowId>,
    /// Cached window→tab map for resolving local sessions' (display-only)
    /// `tab_id`. The launcher no longer snapshots the terminal for this (window
    /// control is presentation-layer; it may be headless/remote), so the
    /// dashboard fills it from its own snapshot — refreshed lazily, only when a
    /// local window isn't yet resolved (e.g. a freshly launched session), so the
    /// steady state costs no `kitten @ ls`.
    pub(super) window_tab_cache: HashMap<WindowId, TabId>,
    /// `(host, cwd) → tab` (with the window `w` created inside it) for every
    /// work tab the dashboard opened via `w`. Persisted to `work-tabs.json` and
    /// re-seeded on startup so `w` returns to the tab an earlier dashboard
    /// opened (the terminal keeps it alive across a restart) instead of spawning
    /// a duplicate. `w` validates the recorded tab against a live snapshot
    /// before switching and prunes entries whose tab is gone, so a stale map
    /// self-heals. Deliberately *not* a cwd scan — `w` only ever returns to tabs
    /// captain-miao created, never to an unrelated shell that happens to sit in
    /// the directory.
    pub(super) work_tabs: HashMap<(HostId, String), WorkTab>,
    /// Cross-backend session-name index. Keyed by session id and child pid;
    /// each `AgentControl::ALL` entry's `read_session_index` populates a
    /// shard which is merged into this view per reload.
    pub(super) session_index: SessionIndex,
    /// Per-host session backends, aggregated into one view. `backends[0]` is
    /// always the local in-process backend; the rest are remote (SSH) hosts.
    /// Reload unions their sessions (tagging each with its host) and indexes.
    pub(super) backends: Vec<Backend>,
    /// `(host, token) → local window` for every session the dashboard has a
    /// window for — remote attaches (token = `pool_session`) and local spawns
    /// (token = `launch_id`). Populated when a session is opened, pruned when its
    /// window dies. See [`bindings`] and next-step #6 §15.
    pub(super) window_bindings: bindings::WindowBindings,
    /// The terminal instance this dashboard *drives* (`zellij:<session>` /
    /// `kitty:<socket|pid>`, from the active backend's
    /// [`Terminal::identity`](crate::terminal::Terminal::identity)), computed
    /// once at startup. Deliberately the backend's identity, not the
    /// ambient-env one: under the `[terminal] backend = "kitty"` override in a
    /// nested zellij the dashboard sits in a zellij pane but every window it
    /// spawns or drives lives in the outer Kitty.
    /// Kitty window ids and zellij pane ids overlap, so this namespaces every
    /// window binding: a local session or a persisted binding stamped with a
    /// *different* terminal is foreign — its window is inert to this backend
    /// ([`App::foreign_terminal`], [`App::window_id_for_session`]). `None` when
    /// the dashboard runs outside a managed terminal.
    pub(super) terminal_identity: Option<String>,
    /// Persisted window bindings from `window-bindings.json` whose `terminal`
    /// differs from [`App::terminal_identity`] — bindings another terminal's
    /// dashboard owns. Held out of `window_bindings` (never resolved, validated,
    /// or reaped here) and re-emitted verbatim by
    /// [`App::write_window_bindings_file`] so switching back to that terminal
    /// still finds each session's window. Seeded once at startup; static after.
    pub(super) foreign_bindings: Vec<state::WindowBinding>,
    /// Monotonic counter behind [`App::mint_launch_id`]; pid-namespaced into the
    /// token so it's unique across dashboards.
    pub(super) next_launch_id: u64,
    /// Per-host label color for the host column, from the hosts config.
    pub(super) host_colors: HashMap<HostId, ratatui::style::Color>,
    pub(super) last_table_rect: Option<Rect>,
    pub(super) last_preview_rect: Option<Rect>,
    pub(super) last_detail_rect: Option<Rect>,
    /// Cell pixel size when the terminal can render kitty graphics, else `None`
    /// (the header draws the emoji-paw fallback). Recomputed on resize.
    pub(super) logo_caps: Option<crate::terminal::graphics::CellSize>,
    /// Screen cells the header paw occupies; the click hit-test (M2) and the
    /// graphics placement both read it. Set by `draw_header` each frame.
    pub(super) logo_rect: Option<Rect>,
    /// Whether the three animated paws (one kitty image per status colour) are
    /// composed and uploaded. Done once; reset across terminal re-inits (which drop
    /// kitty images).
    pub(super) logo_composed: bool,
    /// Which status colour's paw image is currently placed, so an unrelated redraw
    /// doesn't re-place (which would disturb a running pulse) — only a genuine
    /// colour change swaps the displayed image. `None` = nothing placed yet.
    pub(super) logo_placed_color: Option<logo::PawState>,
    /// A click is waiting to fire its one-shot pulse on the next render. Set by the
    /// click handler, consumed (and cleared) by `render_logo_graphics`.
    pub(super) logo_pulse_pending: bool,
    /// The paw's RGB tints indexed by `PawState` (idle/active/attention), seeded
    /// from `DEFAULT_PAW_COLORS` and overlaid at startup with the terminal's own
    /// palette so the paw matches the Sessions status symbols. Baked into the frames.
    pub(super) paw_colors: [(u8, u8, u8); 3],
    /// Cats currently walking the padding row — each paw click spawns one (up to a
    /// pool cap), so several can trot at once. Client-driven: `render_cat_walk`
    /// advances them from wall-clock elapsed, and the run loop ticks fast while any
    /// are live (see `App::cat_walking`).
    pub(super) cats: Vec<logo::CatWalk>,
    /// The cat's four common tints (error/active/attention/selection), resolved from
    /// the terminal palette at startup; a walk picks one at random (or, rarely, a
    /// fixed special colour). See `logo::probe_logo_colors`.
    pub(super) cat_colors: [(u8, u8, u8); 4],
    /// The header's blank padding row (full width, one cell tall) the cat walks
    /// across. Set by `draw_header` each frame; `None` before the first draw.
    pub(super) cat_track: Option<Rect>,
    pub(super) detail_width: u16,
    pub(super) preview_height: u16,
    /// Whether the last frame used the narrow vertical-stack layout (body width
    /// below `panels.narrow_max_width`). Set each draw; read by the mouse
    /// handler so the split-resize drags (a wide-only affordance) stay inert in
    /// the stacked layout.
    pub(super) narrow_layout: bool,
    /// User toggle for the preview panel. Manual toggle always wins — the
    /// panel renders iff this flag is true.
    pub(super) preview_visible: bool,
    /// User toggle for the detail panel. Manual toggle always wins.
    pub(super) detail_visible: bool,
    /// First-draw defaults have been picked based on the initial viewport
    /// size. After this is set, viewport changes don't flip the visibility
    /// flags — user toggles are the only source of truth.
    pub(super) panels_initialized: bool,
    pub(super) drag: Option<DragTarget>,
    /// Timestamp + visible-row index of the last left-click in the table. A
    /// second click on the same row within `DOUBLE_CLICK_THRESHOLD` is treated
    /// as a double-click and focuses that session's window.
    pub(super) last_click: Option<(Instant, usize)>,
    /// Per-session override flags (muted / pinned / follow-up), sparse:
    /// sessions with no overrides are not represented. `follow_up` is
    /// auto-added on Active→Idle transitions; the other two are only set
    /// via explicit user toggles (`m` / `p`).
    pub(super) flags: HashMap<FlagKey, SessionFlags>,
    /// Monotonic counter for `SessionFlags::pin_seq`. Bumped each time a
    /// session is newly pinned so the most recent pin sorts to the top.
    pub(super) next_pin_seq: u64,
    /// Stable random names assigned per launcher_pid on first appearance.
    pub(super) random_names: HashMap<FlagKey, String>,
    /// Status flags awaiting re-application after a restart, keyed by the new
    /// window id (known the moment the replacement launches). When a session
    /// with that window id reappears under a fresh launcher pid,
    /// `reload_sessions` copies these flags onto it and drops the entry.
    pub(super) pending_flag_restores: HashMap<WindowId, SessionFlags>,
    /// Bumped on every mutation that could change `visible_sessions`'s result
    /// (session list, flags, search filter). Used as a cache key for the
    /// field below.
    mutation_version: u64,
    /// Indices into `self.sessions` in display order, cached until
    /// `mutation_version` advances. Avoids re-filtering and re-sorting on
    /// the many per-frame `visible_sessions` calls.
    cached_visible: RefCell<Option<(u64, Vec<usize>)>>,
    /// Last successfully written session snapshot. `save_session_snapshot`
    /// short-circuits when the next payload would be identical, avoiding a
    /// per-fs-event atomic write+rename on no-op reloads.
    last_snapshot: RefCell<Option<Vec<SessionSnapshotEntry>>>,
    /// Last-written `window-bindings.json` payload. `write_window_bindings_file`
    /// short-circuits when unchanged (same rationale as `last_snapshot`):
    /// bindings only move on spawn/attach/detach/prune, rare relative to the
    /// per-fs-event reloads that call it.
    last_bindings: RefCell<Option<Vec<state::WindowBinding>>>,
    /// User toggle for OS-sleep prevention. When true *and* at least one
    /// session is `Active`/`Compacting`, `sleep_inhibitor` runs `caffeinate`
    /// to keep the system awake. Defaults to true; persisted in overrides.
    pub(super) prevent_sleep_enabled: bool,
    /// Process handle for the running `caffeinate` subprocess. Driven by
    /// `update_sleep_inhibitor`, which is called after every reload and
    /// whenever the toggle flips.
    pub(super) sleep_inhibitor: crate::sleep::SleepInhibitor,
    /// Directory-existence predicate used to validate workdir-picker
    /// submissions. A seam so tests can stub which paths "exist" without
    /// touching the real filesystem; production uses `path_is_dir`.
    pub(super) dir_exists: fn(&str) -> bool,
}

/// Default `App::dir_exists`: does `path` name an existing directory?
fn path_is_dir(path: &str) -> bool {
    std::path::Path::new(path).is_dir()
}

/// The last path component of `cwd` (its basename), falling back to the whole
/// string when there is none. Shared by the work-tab title and the
/// session-tab-title template.
pub(super) fn cwd_basename(cwd: &str) -> &str {
    std::path::Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cwd)
}

/// Title stamped on a `(host, cwd)` work tab: the cwd's basename. Shared by the
/// spawn (which sets it) and [`App::live_work_tab`] (which requires it to still
/// match, guarding against zellij's recycled tab ids).
pub(super) fn work_tab_title(cwd: &str) -> String {
    cwd_basename(cwd).to_string()
}

/// The emoji-picker rows, built once from the static `emojis` data and cached.
/// `open_emoji_picker` clones the returned list per open instead of rebuilding
/// ~2k rows from scratch each time (`Ctrl-E`).
fn emoji_picker_items() -> Vec<PickerItem> {
    static ITEMS: OnceLock<Vec<PickerItem>> = OnceLock::new();
    ITEMS
        .get_or_init(|| {
            emojis::iter()
                .map(|e| {
                    let name = e.name();
                    let filter = match e.shortcode() {
                        Some(sc) => format!("{name} {sc}"),
                        None => name.to_string(),
                    };
                    PickerItem::new(name)
                        .with_filter_text(filter)
                        .with_prefix(e.as_str(), ratatui::style::Color::Reset)
                        .with_payload(e.as_str())
                })
                .collect()
        })
        .clone()
}

/// Key for the per-session override maps: `(host, launcher_pid)`. Host-qualified
/// so a remote pid can't collide with a local one (or another host's).
pub(super) type FlagKey = (HostId, u32);

/// The flag-map key for a session.
pub(super) fn flag_key(s: &LauncherState) -> FlagKey {
    (s.host.clone(), s.launcher_pid)
}

/// Whether a session matches a `FlagKey`, without allocating the session's own
/// key (which clones the host `String`). For the per-row `position`/`find`
/// scans that only need equality, not a key.
pub(super) fn matches_key(s: &LauncherState, key: &FlagKey) -> bool {
    s.host == key.0 && s.launcher_pid == key.1
}

/// Pluralize "session" for the restart-confirmation prompts.
fn plural_sessions(n: usize) -> &'static str {
    if n == 1 { "session" } else { "sessions" }
}

/// Resolve a host-label color: a hex / basic name via `config::parse_color`, or
/// one of the directory-palette names (orange/pink/teal/…) the popup offers but
/// `parse_color` doesn't know — so every palette pick round-trips to the table.
fn host_color(name: &str) -> Option<ratatui::style::Color> {
    crate::config::parse_color(name)
        .or_else(|| format::dir_color_index(name).map(|i| format::DIR_COLORS[i].1))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SessionFlags {
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub pinned: bool,
    /// Monotonic sequence number assigned when `pinned` was last set true.
    /// Higher = more recently pinned. Used to order pinned sessions so the
    /// last one pinned floats to the very top. Unused when `pinned == false`.
    /// Only meaningful within one dashboard run; on restart restore it is
    /// used solely for relative ordering and then re-issued a fresh value.
    #[serde(default)]
    pub pin_seq: u64,
    #[serde(default)]
    pub follow_up: bool,
}

impl SessionFlags {
    pub fn is_default(&self) -> bool {
        !self.muted && !self.pinned && !self.follow_up
    }
}

impl App {
    pub(super) fn new() -> Self {
        let home_dir = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default();
        let cfg = crate::config::get();
        let (keymap, keybind_warnings) = keymap::Keymap::from_config(&cfg.keybinds);
        // Surface config problems the TUI would otherwise hide (it swallows
        // stderr, so a status line is the only place the user would see them): a
        // whole-file parse failure that reverted everything to defaults, then any
        // malformed `[keybinds]` entries.
        let mut warnings: Vec<String> = cfg.load_warning.iter().cloned().collect();
        warnings.extend(keybind_warnings);
        let status_msg = (!warnings.is_empty()).then(|| warnings.join("; "));
        let status_is_error = status_msg.is_some();

        let (backends, host_colors) = Self::build_backends_from_config();

        Self {
            sessions: Vec::new(),
            table_state: TableState::default(),
            should_quit: false,
            home_dir,
            status_msg,
            status_is_error,
            input_mode: InputMode::Normal,
            search_input: self::picker::TextInput::new(),
            search_filter: None,
            pending_g: false,
            pending_prefix: None,
            keymap,
            capabilities: crate::terminal::get().capabilities(),
            new_session_agent: AgentControl::from_cli(&crate::config::get().launcher.default_agent)
                .unwrap_or_default(),
            sessions_layout: crate::config::get()
                .terminal
                .sessions_layout
                .unwrap_or_default(),
            preview_text: None,
            preview_lines: None,
            preview_max_width: 0,
            preview_window_id: None,
            preview_dirty_since: None,
            preview_scroll: 0,
            preview_h_scroll: 0,
            focused: true,
            preview_fetched_at: None,
            preview_updated_at: None,
            picker: None,
            pending_confirm: None,
            dir_edit: None,
            host_edit: None,
            directory_marks: HashMap::new(),
            recent_cwds: Vec::new(),
            workdir_completion: None,
            workdir_host_home: String::new(),
            pending_focus_window: None,
            failed_launch_focus_queue: Vec::new(),
            reap_window_queue: Vec::new(),
            window_tab_cache: HashMap::new(),
            work_tabs: HashMap::new(),
            session_index: SessionIndex::default(),
            backends,
            window_bindings: bindings::WindowBindings::default(),
            terminal_identity: crate::terminal::get().identity(),
            foreign_bindings: Vec::new(),
            next_launch_id: 0,
            host_colors,
            last_table_rect: None,
            last_preview_rect: None,
            last_detail_rect: None,
            logo_caps: crate::terminal::graphics::capability(),
            logo_rect: None,
            logo_composed: false,
            logo_placed_color: None,
            logo_pulse_pending: false,
            paw_colors: logo::probed_paw_colors(),
            cats: Vec::new(),
            cat_track: None,
            cat_colors: logo::probed_cat_colors(),
            detail_width: crate::config::get().ui.panels.detail_default_width,
            preview_height: 0,
            narrow_layout: false,
            preview_visible: true,
            detail_visible: true,
            panels_initialized: false,
            drag: None,
            last_click: None,
            flags: HashMap::new(),
            next_pin_seq: 0,
            random_names: HashMap::new(),
            pending_flag_restores: HashMap::new(),
            mutation_version: 0,
            cached_visible: RefCell::new(None),
            last_snapshot: RefCell::new(None),
            last_bindings: RefCell::new(None),
            // Default ON when an inhibitor backend is present, OFF otherwise.
            // This keeps the feature out of the way on systems where the
            // user couldn't act on it (no caffeinate, no systemd-inhibit).
            prevent_sleep_enabled: crate::sleep::supported(),
            sleep_inhibitor: crate::sleep::SleepInhibitor::new(),
            dir_exists: path_is_dir,
        }
    }

    pub(super) fn mark_dirty(&mut self) {
        self.mutation_version = self.mutation_version.wrapping_add(1);
    }

    pub(super) fn flags_of(&self, key: &FlagKey) -> SessionFlags {
        self.flags.get(key).copied().unwrap_or_default()
    }

    pub(super) fn is_muted(&self, key: &FlagKey) -> bool {
        self.flags_of(key).muted
    }

    pub(super) fn is_follow_up(&self, key: &FlagKey) -> bool {
        self.flags_of(key).follow_up
    }

    /// Whether a session is currently soliciting attention: an unmuted session
    /// that either needs a live response (approval / decision / failed-to-start,
    /// plus review-pending via `needs_attention`) or carries a user follow-up
    /// flag while at rest. This is the union of the attention sort-ranks in
    /// `compute_visible_indices` (which splits it into finer tiers for ordering);
    /// `jump_to_next_attention` uses it directly.
    pub(super) fn is_attention_row(&self, s: &LauncherState) -> bool {
        let flags = self.flags_of(&flag_key(s));
        !flags.muted && (s.status.needs_attention() || (flags.follow_up && !s.status.is_busy()))
    }

    /// Apply bell sentinels dropped into the sessions dir by
    /// `captain-miao focus --window-id <id>`. Each pid that still has a live
    /// session gets `follow_up = true`; entries for dead pids are silently
    /// dropped. Persists overrides only if at least one flag actually changed.
    pub(super) fn apply_bell_signals(&mut self, pids: Vec<u32>) {
        // Bell sentinels come from `captain-miao focus --window-id`, which only
        // ever targets local windows, so these pids are local.
        let alive: HashSet<u32> = self
            .sessions
            .iter()
            .filter(|s| s.host.is_local())
            .map(|s| s.launcher_pid)
            .collect();
        let mut changed = false;
        for pid in pids {
            let key = (HostId::local(), pid);
            if !alive.contains(&pid) || self.flags_of(&key).follow_up {
                continue;
            }
            self.update_flags(key, |f| {
                f.follow_up = true;
                f.muted = false;
            });
            changed = true;
        }
        if changed {
            self.save_overrides();
        }
    }

    /// Mutate a session's flags; removes the entry entirely if the result is
    /// all-false to keep the map sparse.
    pub(super) fn update_flags(&mut self, key: FlagKey, update: impl FnOnce(&mut SessionFlags)) {
        let mut f = self.flags_of(&key);
        update(&mut f);
        if f.is_default() {
            self.flags.remove(&key);
        } else {
            self.flags.insert(key, f);
        }
        self.mark_dirty();
    }

    /// Re-adopt status flags carried over from a restart. Each restart records
    /// the replacement window id in `pending_flag_restores`; when that window's
    /// fresh launcher shows up here, copy the saved flags onto its new pid and
    /// drop the pending entry. Match is by window id rather than pid (which
    /// changed across the restart) because kitty hands out a brand-new id for
    /// the relaunched window. Returns true if any flags were applied so the
    /// caller persists overrides.
    pub(super) fn apply_pending_flag_restores(&mut self) -> bool {
        if self.pending_flag_restores.is_empty() {
            return false;
        }
        let mut matched: Vec<(FlagKey, WindowId, SessionFlags)> = self
            .sessions
            .iter()
            .filter_map(|s| {
                let wid = self.window_id_for_session(s)?;
                self.pending_flag_restores
                    .get(&wid)
                    .map(|f| (flag_key(s), wid, *f))
            })
            .collect();
        if matched.is_empty() {
            return false;
        }
        // Apply oldest-pinned first so the re-issued pin sequence numbers
        // preserve the sessions' relative pin order.
        matched.sort_by_key(|(_, _, f)| f.pin_seq);
        for (key, wid, mut f) in matched {
            // The saved pin_seq lived in the previous run's sequence space; it
            // ordered the batch above but would clash with the live counter, so
            // re-issue a fresh one that sorts above existing pins.
            if f.pinned {
                self.next_pin_seq += 1;
                f.pin_seq = self.next_pin_seq;
            }
            self.flags.insert(key, f);
            self.pending_flag_restores.remove(&wid);
        }
        self.mark_dirty();
        true
    }

    /// Update the search filter. Wrapping this in a setter is important: it
    /// bumps `mutation_version`, invalidating the visible/dir-labels caches.
    pub(super) fn set_search_filter(&mut self, filter: Option<String>) {
        if self.search_filter != filter {
            self.search_filter = filter;
            self.mark_dirty();
        }
    }

    pub(super) fn save_overrides(&self) {
        let mut overrides = DashboardOverrides::default();
        // Only local flags persist (keyed by pid, the historical format); remote
        // flags are session-lifetime — a remote pid means nothing across runs.
        // Persist pinned in sequence order (oldest first, most-recent last) so
        // reload reconstructs the same ranking.
        let mut pinned: Vec<(u64, u32)> = self
            .flags
            .iter()
            .filter(|((host, _), f)| host.is_local() && f.pinned)
            .map(|((_, pid), f)| (f.pin_seq, *pid))
            .collect();
        pinned.sort_by_key(|(seq, _)| *seq);
        for (_, pid) in pinned {
            overrides.pinned.push(pid);
        }
        for ((host, pid), f) in &self.flags {
            if !host.is_local() {
                continue;
            }
            if f.muted {
                overrides.muted.push(*pid);
            }
            if f.follow_up {
                overrides.follow_up.push(*pid);
            }
        }
        overrides.prevent_sleep = Some(self.prevent_sleep_enabled);
        overrides.default_agent = Some(self.new_session_agent.cli_subcommand().to_string());
        overrides.sessions_layout = Some(self.sessions_layout.label().to_string());
        let _ = state::write_json_atomic(&state::dashboard_overrides_path(), &overrides);
    }

    pub(super) fn load_overrides(&mut self) {
        let Some(overrides): Option<DashboardOverrides> =
            state::read_json(&state::dashboard_overrides_path())
        else {
            return;
        };
        self.flags.clear();
        // Persisted flags are local (see save_overrides) — re-key under `local`.
        for pid in overrides.muted {
            self.flags.entry((HostId::local(), pid)).or_default().muted = true;
        }
        // The persisted order is oldest first — assign sequence numbers so the
        // last-pinned session keeps the highest seq.
        for (i, pid) in overrides.pinned.iter().enumerate() {
            let f = self.flags.entry((HostId::local(), *pid)).or_default();
            f.pinned = true;
            f.pin_seq = (i as u64) + 1;
        }
        self.next_pin_seq = overrides.pinned.len() as u64;
        for pid in overrides.follow_up {
            self.flags
                .entry((HostId::local(), pid))
                .or_default()
                .follow_up = true;
        }
        if let Some(v) = overrides.prevent_sleep {
            // A `true` persisted from a previous run on a system that no
            // longer has the backend (binary uninstalled, switched distros)
            // is silently downgraded — the user can re-enable via Space z
            // once the binary is back, and we won't keep printing errors at
            // them on every reload.
            self.prevent_sleep_enabled = v && crate::sleep::supported();
        }
        if let Some(a) = overrides
            .default_agent
            .as_deref()
            .and_then(AgentControl::from_cli)
        {
            self.new_session_agent = a;
        }
        if let Some(l) = overrides
            .sessions_layout
            .as_deref()
            .and_then(SessionsLayout::from_label)
        {
            self.sessions_layout = l;
        }
        self.mark_dirty();
    }

    /// True iff at least one tracked session is currently working — `Active`,
    /// `Compacting`, or `BackgroundActive` (a short-term background task the
    /// agent is waiting on, which can itself peg the CPU), via `is_busy`. Used to
    /// decide when to actually run caffeinate; sleep during Idle /
    /// WaitingForApproval / BackgroundServer is fine because the agent isn't
    /// working (a parked long-running dev server is deliberately not counted —
    /// see `is_busy`) and macOS just pauses the process either way.
    pub(super) fn has_active_session(&self) -> bool {
        self.sessions.iter().any(|s| s.status.is_busy())
    }

    /// Reconcile the caffeinate subprocess with the (toggle, active-session)
    /// state. Idempotent — `enable`/`disable` no-op when already in the
    /// desired state. Called after every `reload_sessions` and on toggle.
    pub(super) fn update_sleep_inhibitor(&mut self) {
        if self.prevent_sleep_enabled && self.has_active_session() {
            self.sleep_inhibitor.enable();
        } else {
            self.sleep_inhibitor.disable();
        }
    }

    /// Flip `prevent_sleep_enabled`, immediately reconcile caffeinate, persist
    /// the new value, and surface a status message. Bound to `Space z`.
    /// Refuses to enable when no backend is available — the user gets the
    /// missing-binary explanation as a status-line error so they know what to
    /// install. Disabling is always allowed (so a stale persisted `true`
    /// can still be cleared even if `supported()` would block re-enabling).
    pub(super) fn toggle_prevent_sleep(&mut self) {
        let want_on = !self.prevent_sleep_enabled;
        if want_on && !crate::sleep::supported() {
            self.set_status(
                format!(
                    "Cannot enable prevent-sleep: {}",
                    crate::sleep::missing_reason()
                ),
                true,
            );
            return;
        }
        self.prevent_sleep_enabled = want_on;
        self.update_sleep_inhibitor();
        self.save_overrides();
        let label = if self.prevent_sleep_enabled {
            "enabled"
        } else {
            "disabled"
        };
        let suffix = if self.prevent_sleep_enabled && !self.has_active_session() {
            " (idle — will activate when a session goes Active)"
        } else {
            ""
        };
        self.set_status(format!("Prevent sleep {label}{suffix}"), false);
    }

    /// Flip the session layout (Stacked ↔ Per-tab), persist it, and surface a
    /// status message. Bound to `Space l`. A spawn-time policy: this changes
    /// where *new* sessions land, not where running ones sit — the hint nudges
    /// the user to `Space e`/`Space E` (restart) to migrate existing sessions.
    pub(super) fn toggle_sessions_layout(&mut self) {
        self.sessions_layout = self.sessions_layout.toggled();
        self.save_overrides();
        self.set_status(
            format!(
                "Session layout: {} — restart sessions (Space e/E) to move existing ones",
                self.sessions_layout.label()
            ),
            false,
        );
    }

    /// Open a picker to set the persistent default backend for new sessions
    /// (`o` / `O`). Bound to `Space a`. The choice is saved to the overrides
    /// file and survives restart; an individual launch can still override it
    /// from inside the new-session picker (`Ctrl-t`). The cursor starts on the
    /// current default so it reads as "this is active, change it."
    pub(super) fn open_default_agent_picker(&mut self) {
        let items: Vec<PickerItem> = AgentControl::ALL
            .iter()
            .map(|a| {
                // `new` already sets the filter text to the lowercased label, so
                // no `with_filter_text` is needed.
                PickerItem::new(a.label().to_string()).with_payload(a.cli_subcommand().to_string())
            })
            .collect();
        let mut picker = Picker::new("Default backend for new sessions", items);
        if let Some(idx) = AgentControl::ALL
            .iter()
            .position(|a| *a == self.new_session_agent)
        {
            picker.cursor = idx;
        }
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::DefaultAgent,
        });
        self.input_mode = InputMode::Picker;
    }

    /// Open the searchable emoji picker over the directory-mark editor. Every
    /// emoji (one representative per skin-tone family, courtesy of
    /// `emojis::iter`) becomes a row keyed by its CLDR name + shortcode, so the
    /// user filters by typing e.g. "rocket". The glyph itself is the row
    /// prefix; the chosen emoji rides home in `payload`. Submitting writes it
    /// into the icon field and returns to the editor (see `handle_picker_key`).
    pub(super) fn open_emoji_picker(&mut self) {
        // Only meaningful while the editor is open; guard so a stray call
        // can't strand the picker with nowhere to deliver its result.
        if self.dir_edit.is_none() {
            return;
        }
        // The row list is built once from the static `emojis` data and cached;
        // each open clones it rather than re-allocating ~2k rows.
        let picker =
            Picker::new("Emoji", emoji_picker_items()).with_placeholder("Search emoji by name…");
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::Emoji,
        });
        self.input_mode = InputMode::Picker;
    }

    /// Write the emoji `payload` of a submitted emoji-picker row into the
    /// directory-mark editor's icon field, then hand control back to the
    /// editor. No-op (beyond returning to the editor) if the dir-edit state
    /// vanished while the picker was up.
    pub(super) fn apply_emoji_pick(&mut self, emoji: &str) {
        if let Some(s) = self.dir_edit.as_mut() {
            s.custom.set_text(emoji);
            s.focus = DirEditFocus::Custom;
        }
        self.input_mode = InputMode::DirEdit;
    }

    pub(super) fn load_recent_cwds(&mut self) {
        if let Some(recent) = state::read_json::<state::RecentCwds>(&state::recent_cwds_path()) {
            self.recent_cwds = recent.cwds;
        }
    }

    pub(super) fn load_directory_marks(&mut self) {
        if let Some(d) = state::read_json::<DirectoryMarks>(&state::directory_marks_path()) {
            self.directory_marks = d.marks;
        }
    }

    fn save_directory_marks(&self) {
        let payload = DirectoryMarks {
            marks: self.directory_marks.clone(),
        };
        let _ = state::write_json_atomic(&state::directory_marks_path(), &payload);
    }

    /// Seed `work_tabs` from `work-tabs.json` at startup so `w` returns to the
    /// work tab an earlier dashboard opened rather than spawning a duplicate.
    /// Seeded entries are validated lazily: [`App::live_work_tab`] checks each
    /// against a live snapshot on use and prunes any whose tab died while the
    /// dashboard was off, so a stale seed self-heals.
    pub(super) fn load_work_tabs(&mut self) {
        let entries: Vec<WorkTabEntry> =
            state::read_json(&state::work_tabs_path()).unwrap_or_default();
        self.work_tabs = entries
            .into_iter()
            .map(|e| {
                (
                    (HostId(e.host), e.cwd),
                    WorkTab {
                        tab_id: e.tab_id,
                        window_id: e.window_id,
                    },
                )
            })
            .collect();
    }

    /// Rewrite `work-tabs.json` from the in-memory map. Called whenever `w`
    /// records a new work tab or prunes a dead one.
    pub(super) fn save_work_tabs(&self) {
        let entries: Vec<WorkTabEntry> = self
            .work_tabs
            .iter()
            .map(|((host, cwd), work_tab)| WorkTabEntry {
                host: host.0.clone(),
                cwd: cwd.clone(),
                tab_id: work_tab.tab_id.clone(),
                window_id: work_tab.window_id.clone(),
            })
            .collect();
        let _ = state::write_json_atomic(&state::work_tabs_path(), &entries);
    }

    /// Effective `(icon, color, color_idx)` for `cwd`: the user's override
    /// when present, otherwise the deterministic default emoji + color seeded
    /// from the path. The index is needed by the popup to seed its color
    /// cursor; row rendering ignores it.
    pub(super) fn effective_dir_mark(&self, cwd: &str) -> (String, ratatui::style::Color, usize) {
        let key = cwd.trim_end_matches('/');
        let (default_icon, default_color_idx) = format::default_dir_emoji_and_color(key);
        let Some(mark) = self.directory_marks.get(key) else {
            return (
                default_icon.to_string(),
                format::DIR_COLORS[default_color_idx].1,
                default_color_idx,
            );
        };
        let icon = if mark.icon.is_empty() {
            default_icon.to_string()
        } else {
            mark.icon.clone()
        };
        let color_idx = format::dir_color_index(&mark.color).unwrap_or(default_color_idx);
        (icon, format::DIR_COLORS[color_idx].1, color_idx)
    }

    pub(super) fn open_dir_edit(&mut self) {
        let Some(s) = self.selected_session() else {
            return;
        };
        let cwd = s.cwd.trim_end_matches('/').to_string();
        let (_, _, color_idx) = self.effective_dir_mark(&cwd);
        let mut custom = self::picker::TextInput::new();
        if let Some(mark) = self.directory_marks.get(&cwd)
            && !mark.icon.is_empty()
        {
            custom.set_text(mark.icon.clone());
        }
        self.dir_edit = Some(DirEditState {
            cwd,
            color_idx,
            custom,
            focus: DirEditFocus::Custom,
        });
        self.input_mode = InputMode::DirEdit;
    }

    /// Build the backend set from `hosts.json`: `backends[0]` local, then one
    /// `RemoteBackend` per host with a `socket` (the ssh-target branch lands in
    /// the ssh-transport slice). Returns the backends plus the host-label colors.
    ///
    /// Without the `remote` feature ([`REMOTE_ENABLED`]) this stops at the local
    /// backend: `hosts.json` is never read, so no remote connection task is ever
    /// spawned and every row is local.
    fn build_backends_from_config() -> (Vec<Backend>, HashMap<HostId, ratatui::style::Color>) {
        let mut backends = vec![Backend::local()];
        let mut host_colors: HashMap<HostId, ratatui::style::Color> = HashMap::new();
        if !REMOTE_ENABLED {
            return (backends, host_colors);
        }
        for h in hosts::load_hosts() {
            let host = HostId(h.label.clone());
            // "local" is reserved for the in-process backend; a host that aliases
            // it would have its sessions misclassified as local everywhere the
            // `(host, pid)` keying relies on `is_local()`.
            if host.is_local() {
                continue;
            }
            if let Some(c) = h.color.as_deref().and_then(host_color) {
                host_colors.insert(host.clone(), c);
            }
            if let Some(sock) = h.socket {
                let t = Transport::Socket(std::path::PathBuf::from(sock));
                backends.push(Backend::Remote(RemoteBackend::connect(t, host)));
            } else if let Some(target) = h.ssh {
                // One short, OS-limit-safe local socket per host; ssh forwards
                // the remote server's socket onto it.
                let local_sock = crate::state::remote_forward_sock(&host.0);
                let t = Transport::Ssh { target, local_sock };
                backends.push(Backend::Remote(RemoteBackend::connect(t, host)));
            }
        }
        (backends, host_colors)
    }

    /// Tear down the remote backends and rebuild from the current `hosts.json`
    /// (dropping a `Backend::Remote` ends its connection task). Called after the
    /// hosts popup saves.
    fn rebuild_remote_backends(&mut self) {
        let (backends, host_colors) = Self::build_backends_from_config();
        self.backends = backends;
        self.host_colors = host_colors;
        self.mark_dirty();
    }

    pub(super) fn open_host_edit(&mut self) {
        let rows = hosts::load_hosts()
            .into_iter()
            .map(|h| HostRow {
                color_idx: h
                    .color
                    .as_deref()
                    .and_then(format::dir_color_index)
                    .unwrap_or(0),
                is_socket: h.socket.is_some(),
                target: h.socket.or(h.ssh).unwrap_or_default(),
                label: h.label,
            })
            .collect::<Vec<_>>();
        self.host_edit = Some(HostEditState {
            cursor: 0,
            editing: false,
            focus: HostField::Label,
            rows,
        });
        self.input_mode = InputMode::HostEdit;
    }

    /// Persist the popup's host rows and reconnect the backends.
    pub(super) fn commit_host_edit(&mut self) {
        let Some(state) = self.host_edit.take() else {
            return;
        };
        let configs: Vec<hosts::HostConfig> = state
            .rows
            .into_iter()
            // Drop blank rows and any that alias the reserved `local` host.
            .filter(|r| {
                !r.label.trim().is_empty()
                    && !r.target.trim().is_empty()
                    && !r.label.trim().eq_ignore_ascii_case("local")
            })
            .map(|r| {
                let target = r.target.trim().to_string();
                hosts::HostConfig {
                    label: r.label.trim().to_string(),
                    color: Some(format::DIR_COLORS[r.color_idx].0.to_string()),
                    socket: r.is_socket.then(|| target.clone()),
                    ssh: (!r.is_socket).then_some(target),
                }
            })
            .collect();
        hosts::save_hosts(&configs);
        self.rebuild_remote_backends();
        self.input_mode = InputMode::Normal;
    }

    pub(super) fn cancel_host_edit(&mut self) {
        self.host_edit = None;
        self.input_mode = InputMode::Normal;
    }

    pub(super) fn commit_dir_edit(&mut self) {
        let Some(state) = self.dir_edit.take() else {
            return;
        };
        let icon = state.custom.text().trim().to_string();
        let color = format::DIR_COLORS[state.color_idx].0.to_string();
        self.directory_marks
            .insert(state.cwd, DirectoryMark { icon, color });
        self.save_directory_marks();
        // No mark_dirty: directory marks affect rendering only (icon column),
        // not visible_sessions filtering or sort order.
        self.input_mode = InputMode::Normal;
    }

    pub(super) fn reset_dir_edit(&mut self) {
        let Some(cwd) = self.dir_edit.as_ref().map(|s| s.cwd.clone()) else {
            return;
        };
        if self.directory_marks.remove(&cwd).is_some() {
            self.save_directory_marks();
        }
        let (_, _, color_idx) = self.effective_dir_mark(&cwd);
        if let Some(s) = self.dir_edit.as_mut() {
            s.color_idx = color_idx;
            s.custom.clear();
            s.focus = DirEditFocus::Custom;
        }
    }

    pub(super) fn cancel_dir_edit(&mut self) {
        self.dir_edit = None;
        self.input_mode = InputMode::Normal;
    }

    /// Snapshot every currently-alive session that has the metadata needed to
    /// restart it. Called after every `reload_sessions`; removed entirely on
    /// clean exit by `cleanup_dashboard`. Skips the disk write when the
    /// snapshot hasn't changed since the last call — most fs-event reloads
    /// (status flips, prompt updates) don't touch the snapshotted fields.
    pub(super) fn save_session_snapshot(&self) {
        let sessions: Vec<SessionSnapshotEntry> = self
            .sessions
            .iter()
            .filter_map(|s| {
                // Crash recovery relaunches via the local spawn path only, so a
                // remote session here would be re-launched as a bogus *local*
                // `resume <remote-session-id>` in a cwd that may not exist
                // locally. Snapshot local sessions exclusively (matching
                // `restart_spec_for`, `save_overrides`, and `apply_bell_signals`).
                if !s.host.is_local() {
                    return None;
                }
                let window_id = self.window_id_for_session(s)?;
                let session_id = self.session_index.live_session_id(s)?.to_string();
                Some(SessionSnapshotEntry {
                    agent: s.agent,
                    launcher_pid: s.launcher_pid,
                    child_pid: s.child_pid.unwrap_or(s.launcher_pid),
                    window_id,
                    cwd: s.cwd.clone(),
                    session_id,
                    flags: self.flags_of(&flag_key(s)),
                })
            })
            .collect();
        if self.last_snapshot.borrow().as_ref() == Some(&sessions) {
            return;
        }
        let _ = state::write_json_atomic(&state::dashboard_sessions_snapshot_path(), &sessions);
        *self.last_snapshot.borrow_mut() = Some(sessions);
    }

    /// Pop a queued y/N confirmation that asks the user to restart sessions
    /// missing from the previous run. `specs` is pre-filtered to entries whose
    /// launcher pid is no longer alive; an empty list is a no-op.
    pub(super) fn prompt_restart_missing(&mut self, specs: Vec<RestartSpec>) {
        if specs.is_empty() {
            return;
        }
        let n = specs.len();
        let noun = plural_sessions(n);
        self.confirm_restart_all(
            specs,
            format!("Previous dashboard exited with {n} live {noun}. Restart them? [y/N]"),
        );
    }

    fn save_recent_cwds(&self) {
        let recent = state::RecentCwds {
            cwds: self.recent_cwds.clone(),
        };
        let _ = state::write_json_atomic(&state::recent_cwds_path(), &recent);
    }

    /// Push `cwd` onto the recent-cwds list (most-recent first). Deduplicates
    /// and caps at `RECENT_CWDS_MAX`. Persists after each update.
    pub(super) fn push_recent_cwd(&mut self, cwd: &str) {
        if cwd.is_empty() {
            return;
        }
        let cwd = cwd.trim_end_matches('/').to_string();
        self.recent_cwds.retain(|c| c.trim_end_matches('/') != cwd);
        self.recent_cwds.insert(0, cwd);
        let max = crate::config::get().launcher.max_recent_cwds;
        if self.recent_cwds.len() > max {
            self.recent_cwds.truncate(max);
        }
        self.save_recent_cwds();
    }

    /// Session windows whose `tab_id` isn't in `window_tab_cache` yet — the run
    /// loop snapshots the terminal only when this is non-empty (a new session
    /// appeared, or a moved window was invalidated), so a warm cache costs
    /// nothing. Resolved through `window_id_for_session`: a local session yields
    /// its own window, a remote *attached* one yields its local `ssh attach`
    /// window (§8); a remote session we aren't attached to has no local window
    /// and contributes nothing.
    pub(super) fn unresolved_local_tab_windows(&self) -> Vec<WindowId> {
        self.sessions
            .iter()
            .filter_map(|s| self.window_id_for_session(s))
            .filter(|w| !self.window_tab_cache.contains_key(w))
            .collect()
    }

    /// Replace the window→tab cache with a fresh snapshot. Authoritative, so it
    /// also picks up windows that moved tabs and drops closed ones.
    pub(super) fn refresh_tab_cache(&mut self, tabs: &[crate::terminal::Tab]) {
        self.window_tab_cache = crate::terminal::window_tab_map(tabs);
    }

    /// The recorded work tab for `(host, cwd)`, validated against the live tab
    /// tree in `tabs`. The tab must still exist, still carry the title the spawn
    /// stamped on it (the cwd basename), and — when a window id was recorded —
    /// still contain that window: zellij recycles a closed highest tab's id (its
    /// tab counter is max-plus-one over live tabs), so an id + title match alone
    /// could send `w` into an unrelated tab that inherited the number and was
    /// renamed to the same basename. zellij pane ids never recycle, so the
    /// window-in-tab check pins the identity. An entry with no window id (seeded
    /// from a pre-window-id `work-tabs.json`) falls back to the id + title check.
    /// A failed check prunes the entry and returns `None`, so the caller falls
    /// through to spawning a fresh work tab.
    pub(super) fn live_work_tab(&mut self, key: &(HostId, String), tabs: &[Tab]) -> Option<TabId> {
        let work_tab = self.work_tabs.get(key)?;
        let expected = work_tab_title(&key.1);
        let matched = tabs
            .iter()
            .find(|t| t.id == work_tab.tab_id && t.title == expected);
        let alive = matched.is_some_and(|t| match &work_tab.window_id {
            Some(wid) => t.windows.contains(wid),
            None => true,
        });
        if alive {
            Some(work_tab.tab_id.clone())
        } else {
            // In-memory prune only; the `w` handler persists the map (including
            // this removal) once it has resolved, so `live_work_tab` stays a
            // side-effect-free query as far as disk is concerned.
            self.work_tabs.remove(key);
            None
        }
    }

    /// Stamp each session's (display-only) `tab_id` from the cache, resolving the
    /// local window via `window_id_for_session` (own window for local, attach
    /// window for an attached remote). `reload_sessions` rebuilds sessions from
    /// disk with `tab_id == None` (the launcher no longer resolves it), so this
    /// runs after every reload. Resolve in one immutable pass before the mutable
    /// assignment so the cache read and the session write don't alias `self`.
    pub(super) fn fill_tab_ids_from_cache(&mut self) {
        let tabs: Vec<Option<TabId>> = self
            .sessions
            .iter()
            .map(|s| {
                self.window_id_for_session(s)
                    .and_then(|w| self.window_tab_cache.get(&w).cloned())
            })
            .collect();
        for (s, tab) in self.sessions.iter_mut().zip(tabs) {
            s.tab_id = tab;
        }
    }

    pub(super) fn reload_sessions(&mut self) {
        // Remember which session was selected so re-sorting (e.g. a new
        // notification bumping another row above it) doesn't yank focus onto
        // a different session just because the row index stayed the same.
        let prior_selected_key = self.selected_key();
        let prev_status: HashMap<FlagKey, SessionStatus> = self
            .sessions
            .iter()
            .map(|s| (flag_key(s), s.status.clone()))
            .collect();

        self.mark_dirty();
        let fresh = self.collect_sessions();
        // Keep the pre-reload rows so a departed one (its state file vanished —
        // crash / SIGKILL, not a clean kill) can have its held pane reaped: on a
        // floating-sessions backend the exited pane is an invisible leak. A no-op
        // on kitty (`reap_departed_windows` returns nothing there).
        let prev_sessions = std::mem::replace(&mut self.sessions, fresh);
        let reaped = self.reap_departed_windows(&prev_sessions);
        self.reap_window_queue.extend(reaped);
        self.session_index = self.refresh_session_index();
        // Auto-mark follow_up on Active→Idle and Compacting→Compacted
        // transitions, and clear it when a session goes back to Active — the
        // user has re-engaged, so any stale attention flag is obsolete.
        let mut overrides_changed = false;
        let transitions = self.follow_up_transitions(&prev_status, &self.sessions);
        for (key, want) in transitions {
            self.update_flags(key, |f| f.follow_up = want);
            overrides_changed = true;
        }
        // Bring a just-failed launch's held window to the foreground exactly
        // once — on the transition into `FailedToStart`. The run loop drains and
        // focuses (the launcher can't focus its own window). Computed before the
        // `extend` so the immutable resolve doesn't overlap the mutable push.
        let newly_failed = self.newly_failed_windows(&prev_status, &self.sessions);
        self.failed_launch_focus_queue.extend(newly_failed);
        // Status flips on the selected session usually mean the previewed
        // terminal just changed (assistant turn ended, approval popped,
        // /compact landed). Force a fetch so the preview reflects the new
        // state instead of waiting for the next selection move or focus
        // event.
        if let Some(key) = &prior_selected_key {
            let new_status = self
                .sessions
                .iter()
                .find(|s| matches_key(s, key))
                .map(|s| &s.status);
            if let (Some(new), Some(old)) = (new_status, prev_status.get(key))
                && new != old
            {
                self.request_preview_refresh();
            }
        }
        // Assign stable random names to new sessions (host-qualified so two
        // hosts' same-pid sessions don't share one fallback name).
        for s in &self.sessions {
            self.random_names
                .entry(flag_key(s))
                .or_insert_with(|| random_session_name(s.launcher_pid));
        }
        // Re-adopt status flags carried over from a restart now that the
        // relaunched sessions may have appeared under their new pids.
        overrides_changed |= self.apply_pending_flag_restores();
        // Drop flag / random-name entries for gone sessions.
        let alive_keys: HashSet<FlagKey> = self.sessions.iter().map(flag_key).collect();
        self.random_names.retain(|key, _| alive_keys.contains(key));
        let flags_before = self.flags.len();
        self.flags.retain(|key, _| alive_keys.contains(key));
        overrides_changed |= self.flags.len() != flags_before;
        if overrides_changed {
            self.save_overrides();
        }
        // Reconcile sleep inhibition with the new session set: a freshly
        // active session needs to spin caffeinate up; the last active one
        // returning to Idle needs to spin it down.
        self.update_sleep_inhibitor();
        // Refresh the on-disk window→(host, pid, token) projection the external
        // bell keybind and the next startup seed read (§15.4). Done here, before
        // the early-returning selection block below, so it always runs.
        self.write_window_bindings_file();
        // Drop a pending-focus target the spawned launcher never claimed (it died
        // before writing a state file): otherwise, after a kitty restart reissues
        // small window ids, a later unrelated session could match it and get
        // selection-yanked once. Done before `visible` borrows `self`.
        if self
            .pending_focus_window
            .as_ref()
            .is_some_and(|(_, set_at)| set_at.elapsed() > PENDING_FOCUS_MAX_AGE)
        {
            self.pending_focus_window = None;
        }
        let visible = self.visible_sessions();
        if let Some((target_wid, _)) = self.pending_focus_window.clone()
            && let Some(idx) = visible
                .iter()
                .position(|s| self.window_id_for_session(s).as_ref() == Some(&target_wid))
        {
            self.table_state.select(Some(idx));
            self.pending_focus_window = None;
            return;
        }
        if let Some(key) = &prior_selected_key
            && let Some(idx) = visible.iter().position(|s| matches_key(s, key))
        {
            self.table_state.select(Some(idx));
            return;
        }
        self.clamp_selection();
    }

    /// Run `f` over the cached visible-row indices without materializing a
    /// `Vec<&LauncherState>`. The index list is recomputed only when
    /// `mutation_version` advances. Per-tick accessors that need just a count or
    /// a single row (`visible_len`, `nth_visible`) go through here so they don't
    /// allocate a temporary Vec of references on every call.
    fn with_visible_indices<R>(&self, f: impl FnOnce(&[usize]) -> R) -> R {
        let version = self.mutation_version;
        {
            let cache = self.cached_visible.borrow();
            if let Some((cached_v, indices)) = cache.as_ref()
                && *cached_v == version
            {
                return f(indices);
            }
        }
        let indices = self.compute_visible_indices();
        let r = f(&indices);
        *self.cached_visible.borrow_mut() = Some((version, indices));
        r
    }

    pub(super) fn visible_sessions(&self) -> Vec<&LauncherState> {
        self.with_visible_indices(|indices| indices.iter().map(|&i| &self.sessions[i]).collect())
    }

    /// Number of visible rows. Cheaper than `visible_sessions().len()` — no
    /// `Vec<&LauncherState>` is built just to count.
    pub(super) fn visible_len(&self) -> usize {
        self.with_visible_indices(<[usize]>::len)
    }

    /// The `i`-th visible session, or `None` if out of range. Cheaper than
    /// `visible_sessions().get(i)` — indexes the cached list directly without
    /// allocating the reference Vec.
    pub(super) fn nth_visible(&self, i: usize) -> Option<&LauncherState> {
        let idx = self.with_visible_indices(|indices| indices.get(i).copied())?;
        Some(&self.sessions[idx])
    }

    /// Map a screen row (a mouse event's `row`) to the visible-session index it
    /// lands on, or `None` when the row is on the table's border/header chrome
    /// or past the last data row. `draw_table` renders the table with
    /// `Borders::ALL` + a header, so the first data row sits two rows below the
    /// rect's top; this is the single place that chrome offset is encoded for
    /// click hit-testing. Adds the `TableState` scroll offset so clicks resolve
    /// to the right row once the table has scrolled to keep the selection in
    /// view.
    pub(super) fn visible_index_at(&self, screen_row: u16, table_rect: Rect) -> Option<usize> {
        // Top border (1) + header (1) rows above the first data row.
        const CHROME_ROWS: u16 = 2;
        let first_row_y = table_rect.y + CHROME_ROWS;
        if screen_row < first_row_y {
            return None;
        }
        let idx = self.table_state.offset() + (screen_row - first_row_y) as usize;
        (idx < self.visible_len()).then_some(idx)
    }

    fn compute_visible_indices(&self) -> Vec<usize> {
        let query = self.search_filter.as_deref().filter(|q| !q.is_empty());
        let mut indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| match query {
                None => true,
                Some(q) => {
                    contains_ci(&s.cwd, q)
                        || s.last_prompt.as_deref().is_some_and(|p| contains_ci(p, q))
                        || contains_ci(s.status.label(), q)
                        || self
                            .session_index
                            .lookup(s)
                            .is_some_and(|n| contains_ci(n, q))
                }
            })
            .map(|(i, _)| i)
            .collect();
        // Pinned sessions always float to the top, even while actively working.
        // Among pinned, the most-recently pinned (highest pin_seq) comes first.
        // Within other ranks, most-recently-updated sessions come first.
        indices.sort_by_cached_key(|&i| {
            let s = &self.sessions[i];
            let active = s.status.is_busy();
            let flags = self.flags_of(&flag_key(s));
            // Approvals, decisions, and failed-to-start launches float to the
            // top attention rank. `ReviewPending` is needs-attention too, but
            // it's a *follow-up*: the human should get to the review, it isn't
            // the agent blocking on a live prompt. It ranks like a follow-up
            // idle session but sits in its own tier *below* the actual
            // follow-up-flagged rows (rank 3) — above plain idle, below
            // follow-up — sorted by updated_at like the other attention tiers.
            let review_pending = matches!(s.status, SessionStatus::ReviewPending);
            let attention = s.status.needs_attention() && !review_pending;
            // Ranks 1–3 partition exactly what `is_attention_row` unions (an
            // unmuted needs-attention or at-rest follow-up row); kept split here
            // because ordering needs the finer tiers. If the predicate there
            // changes, revisit this arithmetic to keep the jump target and the
            // sort in agreement.
            let rank: u8 = if flags.muted {
                6
            } else if flags.pinned {
                0
            } else if attention {
                1
            } else if flags.follow_up && !active {
                2
            } else if review_pending {
                3
            } else if !active {
                4
            } else {
                5
            };
            // Pinned: most-recently-pinned first (negate seq so larger sorts before smaller).
            // Attention groups (approval + follow_up + review-pending): oldest
            // first so the longest-waiting session surfaces at the top of its
            // tier. Active group: sort by when the session entered the active
            // state so the order stays stable during a turn (updated_at churns
            // on every tool event). Everything else: newest updated_at first.
            let time_key: i64 = if rank == 0 {
                -(flags.pin_seq as i64)
            } else if rank == 1 || rank == 2 || rank == 3 {
                s.updated_at as i64
            } else if active {
                -(s.active_since.unwrap_or(s.updated_at) as i64)
            } else {
                -(s.updated_at as i64)
            };
            (rank, time_key)
        });
        indices
    }

    /// Index of the last non-muted session in the current visible list.
    pub(super) fn last_unmuted_index(&self) -> Option<usize> {
        let visible = self.visible_sessions();
        visible.iter().rposition(|s| !self.is_muted(&flag_key(s)))
    }

    /// Live sessions across every backend, each tagged with its host so per-row
    /// keying can tell a remote pid from a local one. `backends[0]` (local)
    /// comes first, so a recycled-pid collision resolves in favour of local.
    fn collect_sessions(&self) -> Vec<LauncherState> {
        let mut out = Vec::new();
        for backend in &self.backends {
            let host = backend.host_id();
            for mut s in backend.list_sessions() {
                s.host = host.clone();
                out.push(s);
            }
        }
        out
    }

    /// The session-name index unioned across every backend. The merge + per-agent
    /// caching lives in each backend; the dashboard just unions the shards. Names
    /// no longer need a client-side overlay — Claude renames and Codex titles both
    /// ride `LauncherState.name`, folded by each session's launcher.
    fn refresh_session_index(&mut self) -> SessionIndex {
        let mut merged = SessionIndex::default();
        for backend in &mut self.backends {
            let shard = backend.session_index();
            merged.by_pid.extend(shard.by_pid);
            merged.by_pid_owner.extend(shard.by_pid_owner);
            merged.by_session_id.extend(shard.by_session_id);
            merged.session_id_by_pid.extend(shard.session_id_by_pid);
        }
        merged
    }

    /// The local in-process backend (`backends[0]`, always present). Spawn-based
    /// ops (new/resume/restart) and the resume list use it directly for now —
    /// they create local Kitty windows, which remote sessions don't have yet.
    pub(super) fn local_backend(&self) -> &Backend {
        &self.backends[0]
    }

    /// The backend that owns `host`, falling back to local — so a kill routes to
    /// the right host (an RPC for a remote, a signal for local).
    pub(super) fn backend_for(&self, host: &HostId) -> &Backend {
        self.backends
            .iter()
            .find(|b| &b.host_id() == host)
            .unwrap_or(&self.backends[0])
    }

    /// Remote hosts that aren't currently `Connected`, paired with their state,
    /// for the header's connection surface. A disconnected host clears its
    /// mirror (no rows), so this is the only place its state is visible. Local
    /// is always connected and never listed.
    pub(super) fn unhealthy_hosts(&self) -> Vec<(HostId, ConnState)> {
        self.backends
            .iter()
            .filter(|b| !b.host_id().is_local())
            .filter_map(|b| match b.conn_state() {
                ConnState::Connected => None,
                st => Some((b.host_id(), st)),
            })
            .collect()
    }

    /// Record the local window the dashboard just opened for a session, keyed by
    /// its binding token (a remote `pool_session` or a local `launch_id`, §15.2),
    /// so the dashboard can resolve and prune it. Used by both the remote attach
    /// path and the local spawn path.
    pub(super) fn record_window_binding(&mut self, host: HostId, token: String, window: WindowId) {
        self.window_bindings.record(host, token, window);
    }

    /// Mint a fresh, opaque `launch_id` for a local launcher the dashboard is
    /// about to spawn (next-step #6 §15.2). Pid-namespaced so two dashboards
    /// (or a restarted one) never collide, and monotonic within a run. Threaded
    /// onto the launcher as `--launch-id` and echoed back on its state file, so
    /// the appearing row resolves to the window the dashboard opened.
    pub(super) fn mint_launch_id(&mut self) -> String {
        self.next_launch_id += 1;
        format!("L{}-{}", std::process::id(), self.next_launch_id)
    }

    /// Drop bindings whose window is no longer among `live` (a Terminal
    /// snapshot's window ids), returning the dropped keys. Wired into the reload
    /// loop so a slept laptop / dropped ssh empties those *remote* rows cleanly
    /// (§5); for a *local* session, window death and launcher death coincide (the
    /// kitty window's SIGHUP kills the launcher), so the row already left via its
    /// state file and this just garbage-collects the stale binding (§15.5). A
    /// no-op until the dashboard holds a binding.
    pub(super) fn prune_detached_sessions(
        &mut self,
        live: &HashSet<WindowId>,
    ) -> Vec<bindings::BindingKey> {
        if self.window_bindings.is_empty() {
            return Vec::new();
        }
        self.window_bindings.prune_dead(live)
    }

    /// The follow-up flag auto-mark / auto-clear transitions to apply after a
    /// reload — a pure function of the previous status map and the freshly
    /// collected sessions, returning `(key, want)` pairs the caller feeds to
    /// `update_flags`. Sibling of `newly_failed_windows`; extracted so the
    /// transition is unit-testable (`reload_sessions` itself is driven only
    /// through fs events). A session that just entered a rest state and isn't
    /// muted or already flagged gets `true`; one back to Active that still
    /// carries the flag gets `false`.
    pub(super) fn follow_up_transitions(
        &self,
        prev_status: &HashMap<FlagKey, SessionStatus>,
        sessions: &[LauncherState],
    ) -> Vec<(FlagKey, bool)> {
        sessions
            .iter()
            .filter_map(|s| {
                let prev = prev_status.get(&flag_key(s));
                let flags = self.flags_of(&flag_key(s));
                let entered_rest = matches!(
                    (prev, &s.status),
                    (Some(SessionStatus::Active), SessionStatus::Idle)
                    | (Some(SessionStatus::Compacting), SessionStatus::Compacted)
                    // A turn that ended with a short-term background task lands in
                    // Idle only once the task finishes — surface that for attention
                    // too, so Active→Task→Idle gets the same follow-up as Active→Idle.
                    | (Some(SessionStatus::BackgroundActive), SessionStatus::Idle)
                    // Same for a parked server / review-watch that ends without the
                    // agent resuming (killed / timed out): Server/Review→Idle earns
                    // a follow-up.
                    | (Some(SessionStatus::BackgroundServer), SessionStatus::Idle)
                    | (Some(SessionStatus::ReviewPending), SessionStatus::Idle),
                );
                // Parking a long-running service (entering `BackgroundServer`) is
                // itself an at-rest "needs a look" event: the agent stopped working
                // and left a dev server/watcher running. Arm the bell the moment it
                // appears — but only on a real transition into it from a known other
                // state, so pre-existing Server rows don't all light up at dashboard
                // startup (prev is None then). (`BackgroundActive` — a busy
                // short-term task — is not armed on entry; it arms on its exit to
                // Idle above, like Active.)
                let parked_server = s.status == SessionStatus::BackgroundServer
                    && prev.is_some_and(|p| *p != SessionStatus::BackgroundServer);
                if (entered_rest || parked_server) && !flags.muted && !flags.follow_up {
                    Some((flag_key(s), true))
                } else if s.status == SessionStatus::Active && flags.follow_up {
                    Some((flag_key(s), false))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Window ids whose launch just transitioned into `FailedToStart` since the
    /// previous reload — the held error windows the dashboard should focus once.
    /// Restricted to local sessions that resolve to a window (via the binding
    /// the spawn recorded, or the self-reported `window_id` for a hand-launched
    /// one): a remote/headless launch failure has no local window to bring
    /// forward. A row whose previous status was already `FailedToStart` is
    /// excluded so the focus fires once, not every reload.
    pub(super) fn newly_failed_windows(
        &self,
        prev_status: &HashMap<FlagKey, SessionStatus>,
        sessions: &[LauncherState],
    ) -> Vec<WindowId> {
        sessions
            .iter()
            .filter(|s| {
                s.status == SessionStatus::FailedToStart
                    && prev_status.get(&flag_key(s)) != Some(&SessionStatus::FailedToStart)
                    && s.host.is_local()
            })
            .filter_map(|s| self.window_id_for_session(s))
            .collect()
    }

    /// Windows to close for rows that departed since the previous reload — a
    /// launcher whose state file vanished without a clean kill (crash, SIGKILL, or
    /// the file removed), or a remote pool session the host reported gone. Each
    /// such session spawned `hold: true`, so zellij holds its exited command pane
    /// open: buried in the shared `cm:sessions` tab, invisible (only the z-order
    /// top shows), unreachable except via zellij's floating-cycle keybinds, and
    /// counted in every `list-panes` (~20ms/pane). Resolve the window the dashboard
    /// bound to the departed row (through the still-lingering binding — a local
    /// `launch_id` binding has no other collector, `prune_dead` being remote-only),
    /// drop that stale binding, and return the window for the run loop to
    /// `close_window` best-effort.
    ///
    /// Gated to `floating_sessions` backends (D2): on kitty the held window is
    /// visible in a tab and deliberately kept as crash forensics, so nothing is
    /// reaped and bindings are left untouched. A departed *remote* row is only
    /// reaped when its host is still `Connected` — a disconnect clears the mirror
    /// (every row departs) yet the pool session and its local attach window live
    /// on, and reconnect brings the row back, so tearing that attach window down
    /// would be wrong. `FailedToStart` rows never reach here: their launcher is
    /// alive holding the error, so the row hasn't departed.
    pub(super) fn reap_departed_windows(
        &mut self,
        prev_sessions: &[LauncherState],
    ) -> Vec<WindowId> {
        if !self.capabilities.floating_sessions {
            return Vec::new();
        }
        let alive: HashSet<FlagKey> = self.sessions.iter().map(flag_key).collect();
        let mut reaped = Vec::new();
        for s in prev_sessions {
            if alive.contains(&flag_key(s)) {
                continue;
            }
            // A foreign-terminal row's window lives in another terminal instance;
            // closing its overlapping id through this backend would mis-target an
            // unrelated window. (Its binding isn't in `window_bindings` anyway —
            // it's carried in `foreign_bindings` — so `remove` would no-op, but be
            // explicit.)
            if self.foreign_terminal(s).is_some() {
                continue;
            }
            if !s.host.is_local() && self.backend_for(&s.host).conn_state() != ConnState::Connected
            {
                continue;
            }
            // Only windows the dashboard itself created are reaped, and the
            // binding (a local `--launch-id` spawn or a remote pool attach) is
            // that proof: removing it yields the window and retires the stale
            // entry in one step (a local `launch_id` binding has no other
            // collector — `prune_dead` runs only for remote attachments). A
            // token-less hand-launched row resolves only through its
            // self-reported window id, which names the user's own pane, not
            // dashboard terrain — never closed.
            let token = if s.host.is_local() {
                s.launch_id.clone()
            } else {
                s.pool_session.clone()
            };
            if let Some(token) = token
                && let Some(wid) = self.window_bindings.remove(&s.host, &token)
            {
                reaped.push(wid);
            }
        }
        reaped
    }

    /// Seed the in-memory `WindowBindings` from `window-bindings.json` at startup
    /// so a dashboard that restarts while sessions keep running can still resolve
    /// their windows (next-step #6 §15.7). A stale entry (its window died while
    /// the dashboard was off) is cleared by the first reload's `prune_dead`,
    /// which diffs against the live terminal snapshot. Idempotent; called once
    /// before the first reload.
    ///
    /// The `launcher_pid` liveness gate applies to **local** bindings only:
    /// `is_process_alive` probes the *local* process table, so it's meaningful
    /// only for a local launcher pid. A remote binding's `launcher_pid` lives on
    /// another host — checking it here would (almost) always read "dead" and drop
    /// the binding, so a restarted dashboard would forget it already has an attach
    /// window and spawn a *second* `ssh … attach`, which libshpool rejects with
    /// "already has a terminal attached". Remote bindings are therefore re-seeded
    /// unconditionally and left to `prune_dead` (the correct liveness signal for
    /// them: whether their local attach window still exists).
    pub(super) fn seed_window_bindings_from_disk(&mut self) {
        let entries: Vec<state::WindowBinding> =
            state::read_json(&state::window_bindings_path()).unwrap_or_default();
        self.seed_window_bindings(entries);
    }

    /// Seed the in-memory bindings from previously-persisted entries (split from
    /// the disk read so the liveness / reap logic is unit-testable).
    ///
    /// A binding from **another terminal instance** (its `terminal` differs from
    /// this dashboard's identity) is inert here — its window id names an
    /// overlapping foreign namespace, so it must never be validated or reaped
    /// through this backend. A live one is stashed in `foreign_bindings` to be
    /// carried through every rewrite verbatim (switch back to that terminal and it
    /// resolves again); a dead-local-pid one resolves nothing anywhere, so it's
    /// dropped entirely.
    ///
    /// For a **same-terminal** binding: a dead-local-pid entry is a leaked held
    /// pane from a previous dashboard's crashed launcher — on a floating-sessions
    /// backend it's queued for the run loop to close (same reason as
    /// [`App::reap_departed_windows`]); on kitty it's just dropped (the held
    /// window stays as forensics).
    pub(super) fn seed_window_bindings(&mut self, entries: Vec<state::WindowBinding>) {
        for b in entries {
            let host = HostId(b.host.clone());
            let alive = !host.is_local() || state::is_process_alive(b.launcher_pid);
            if b.terminal != self.terminal_identity {
                // Foreign terminal: preserve a live one for persistence; drop a
                // dead-local-pid one (it resolves nothing in any terminal). Never
                // reaped — closing another terminal's pane id here mis-targets.
                if alive {
                    self.foreign_bindings.push(b);
                }
                continue;
            }
            if alive {
                self.window_bindings.record(host, b.token, b.window_id);
            } else if self.capabilities.floating_sessions {
                self.reap_window_queue.push(b.window_id);
            }
        }
    }

    /// Rewrite `window-bindings.json` from the live rows + the in-memory bindings
    /// (next-step #6 §15.4): for each session that resolves to a window, emit
    /// `{ window_id, host, launcher_pid, token, terminal }`. The dashboard is the
    /// sole writer; the external `focus --window-id` bell keybind and the next
    /// startup seed read it back. Called each reload.
    ///
    /// The emitted set is the union of (a) the same-terminal bindings the current
    /// session list still resolves — each stamped with this dashboard's own
    /// identity — and (b) every *foreign*-terminal binding seeded from disk,
    /// carried through verbatim. A foreign binding's window lives in another
    /// terminal instance (Kitty window ids and zellij pane ids overlap), so it
    /// can't be validated against this backend's snapshot; dropping it would lose
    /// the window a session shows in that other terminal (switch back and it
    /// resolves again — the whole point of the isolation).
    pub(super) fn write_window_bindings_file(&self) {
        let mut entries: Vec<state::WindowBinding> = self
            .sessions
            .iter()
            .filter_map(|s| {
                // `window_id_for_session` already returns `None` for a foreign
                // local row, so only same-terminal (and remote-attach) rows reach
                // here — stamp each with this dashboard's own identity.
                let window_id = self.window_id_for_session(s)?;
                // The token the row carries home — its `launch_id` (local) or
                // `pool_session` (remote). A token-less (hand-launched) session
                // has none; key the bell projection on its self-reported window
                // with an empty token (the bell only needs window → pid).
                let token = if s.host.is_local() {
                    s.launch_id.clone().unwrap_or_default()
                } else {
                    s.pool_session.clone()?
                };
                Some(state::WindowBinding {
                    window_id,
                    host: s.host.0.clone(),
                    launcher_pid: s.launcher_pid,
                    token,
                    terminal: self.terminal_identity.clone(),
                })
            })
            .collect();
        entries.extend(self.foreign_bindings.iter().cloned());
        if self.last_bindings.borrow().as_ref() == Some(&entries) {
            return;
        }
        let _ = state::write_json_atomic(&state::window_bindings_path(), &entries);
        *self.last_bindings.borrow_mut() = Some(entries);
    }

    pub(super) fn clamp_selection(&mut self) {
        let len = self.visible_len();
        if len == 0 {
            self.table_state.select(None);
        } else {
            let i = self
                .table_state
                .selected()
                .map(|i| i.min(len - 1))
                .unwrap_or(0);
            self.table_state.select(Some(i));
        }
    }

    pub(super) fn reset_selection(&mut self) {
        let len = self.visible_len();
        self.table_state
            .select(if len == 0 { None } else { Some(0) });
    }

    pub(super) fn set_status(&mut self, msg: String, is_error: bool) {
        self.status_msg = Some(msg);
        self.status_is_error = is_error;
    }

    /// Borrow the selected session without cloning. Preferred for read-only
    /// access (status checks, field reads) — especially on the per-tick paths —
    /// since the reference points into `self.sessions`, not the temporary
    /// `visible_sessions` Vec, so it outlives that Vec.
    pub(super) fn selected_session_ref(&self) -> Option<&LauncherState> {
        let i = self.table_state.selected()?;
        self.nth_visible(i)
    }

    pub(super) fn selected_session(&self) -> Option<LauncherState> {
        self.selected_session_ref().cloned()
    }

    pub(super) fn selected_window_id(&self) -> Option<WindowId> {
        self.selected_session_ref()
            .and_then(|s| self.window_id_for_session(s))
    }

    /// The *local* window showing this session — the single choke point every
    /// `s.window_id` consumer routes through (next-step #6 §15.3). The dashboard
    /// owns the binding for any session it spawned: it resolves the session's
    /// **token** (a local session's `launch_id`, a remote one's `pool_session`,
    /// §15.2) to the window it opened. A token-less local session — hand-launched
    /// `captain-miao claude`, or a launcher predating `launch_id` — self-reported
    /// `window_id`, so fall back to that. Returns `None` for a remote session we
    /// aren't attached to (no local window); preview / focus / move-to-tab then
    /// no-op rather than target a phantom.
    pub(super) fn window_id_for_session(&self, s: &LauncherState) -> Option<WindowId> {
        // A local row whose stamped terminal differs from the dashboard's own
        // lives in a different terminal instance — Kitty window ids and zellij
        // pane ids overlap, so no window op here can safely target it. Return
        // `None` so preview / focus / move-to-tab no-op rather than drive a
        // foreign namespace's id. A terminal-less row (headless launch) keeps
        // today's behavior. The in-memory binding map holds only same-terminal
        // bindings (foreign ones are carried in `foreign_bindings` for
        // persistence, never resolved), so the lookup below can't reach a foreign
        // window either.
        if self.foreign_terminal(s).is_some() {
            return None;
        }
        let token = if s.host.is_local() {
            s.launch_id.as_deref()
        } else {
            s.pool_session.as_deref()
        };
        if let Some(t) = token {
            return self.window_bindings.window_for(&s.host, t).cloned();
        }
        // Token-less: only a local session can self-report a window to fall back
        // to; a remote session has no such path.
        s.host.is_local().then(|| s.window_id.clone()).flatten()
    }

    /// The terminal instance a *local* session lives in when it differs from the
    /// dashboard's own — `Some(identity)` marks a foreign, window-inert row (D6);
    /// `None` for a same-terminal, terminal-less, or remote session. Local rows
    /// only: a remote session's window is its local ssh-attach window (in *this*
    /// terminal), so the remote launcher's `terminal` field is irrelevant here.
    pub(super) fn foreign_terminal(&self, s: &LauncherState) -> Option<String> {
        if !s.host.is_local() {
            return None;
        }
        let term = s.terminal.as_ref()?;
        (Some(term) != self.terminal_identity.as_ref()).then(|| term.clone())
    }

    /// Decide how to act on a live session: focus its local window (a local
    /// session, or an already-attached remote), attach a new window to a running
    /// remote we aren't attached to, or — for a remote with no pool name yet —
    /// report it isn't attachable (setting a status). Shared by `Enter` on a
    /// dashboard row and the browser.
    pub(super) fn focus_or_attach(&mut self, s: &LauncherState) -> Option<Action> {
        // A foreign-terminal row has no window this backend can drive; say so
        // rather than silently no-op (kill still works — it signals by pid).
        if let Some(identity) = self.foreign_terminal(s) {
            self.set_status(
                format!("Session lives in {identity}; window actions unavailable here"),
                true,
            );
            return None;
        }
        if let Some(wid) = self.window_id_for_session(s) {
            return Some(Action::FocusWindow(wid));
        }
        if s.host.is_local() {
            return None; // local session with no window — nothing to focus
        }
        match &s.pool_session {
            Some(pool) => Some(Action::AttachRemoteRunning {
                host: s.host.clone(),
                pool_session: pool.clone(),
            }),
            None => {
                self.set_status(
                    "Remote session isn't attachable yet (no pool session)".to_string(),
                    true,
                );
                None
            }
        }
    }

    /// Focus (or attach to) the currently selected session — the shared body
    /// behind `Enter`, a row double-click, and the `Ctrl-<digit>` selector, so
    /// all three make the same decision. `focus_or_attach` resolves a remote
    /// session's local ssh-attach window (and attaches a running remote we
    /// aren't attached to yet); the follow-up bell is cleared when an action is
    /// produced, like `Enter`.
    pub(super) fn focus_selected(&mut self) -> Option<Action> {
        let s = self.selected_session()?;
        let action = self.focus_or_attach(&s);
        if action.is_some()
            && let Some(key) = self.selected_key()
        {
            self.clear_follow_up(key);
        }
        action
    }

    pub(super) fn selected_cwd(&self) -> Option<String> {
        self.selected_session_ref().map(|s| s.cwd.clone())
    }

    /// The selected session's flag-map key (host + pid).
    pub(super) fn selected_key(&self) -> Option<FlagKey> {
        self.selected_session_ref().map(flag_key)
    }

    #[cfg(test)]
    pub(super) fn selected_pid(&self) -> Option<u32> {
        self.selected_key().map(|k| k.1)
    }

    /// Clear the follow_up flag for `key` (no-op if it isn't set). The table
    /// cursor *follows this session* to wherever the re-sort floats it: this is
    /// the `Enter`/focus path, so the user is acting on this very session (they
    /// just focused its window) and expects to stay pointed at it, not to be
    /// bumped onto the next attention row. Clearing the bell drops the session
    /// from the attention rank to the idle rank, so it slides down the list —
    /// we re-select it by key at its new index.
    pub(super) fn clear_follow_up(&mut self, key: FlagKey) {
        if !self.is_follow_up(&key) {
            return;
        }
        self.update_flags(key.clone(), |f| f.follow_up = false);
        self.save_overrides();
        // Persist the flag change into the restart snapshot too, so a crash
        // before the next reload doesn't restore the stale flag on recovery.
        self.save_session_snapshot();
        // Re-select the same session at its new (lower) position rather than
        // leaving the cursor at the old index (which would land on whichever
        // row slid up into it).
        match self
            .visible_sessions()
            .iter()
            .position(|s| matches_key(s, &key))
        {
            Some(idx) => self.table_state.select(Some(idx)),
            None => self.clamp_selection(),
        }
    }

    pub(super) fn toggle_session_flag(&mut self, flag: SessionFlag) {
        let Some(key) = self.selected_key() else {
            return;
        };
        // `pid` (a copy) drives the cursor-follow logic below; `key` keys flags.
        let pid = key.1;
        let old_idx = self.table_state.selected();
        let was = self.flags_of(&key);
        // Clearing needs-input re-sorts the marked row down and away from the
        // cursor, so we capture which session sat just after it (or just before,
        // when it's at the end) *before* the toggle and move the cursor there
        // afterwards. Only that one arm needs the pre-mutation order — every other
        // toggle just follows the session itself — so compute it lazily instead of
        // snapshotting the whole visible order on every toggle.
        let follow_target: Option<FlagKey> =
            if matches!(flag, SessionFlag::FollowUp) && was.follow_up {
                old_idx.and_then(|idx| {
                    let visible_before = self.visible_sessions();
                    visible_before
                        .get(idx + 1)
                        .or_else(|| idx.checked_sub(1).and_then(|p| visible_before.get(p)))
                        .map(|s| flag_key(s))
                })
            } else {
                None
            };
        // Mute is mutually exclusive with pin/follow-up: turning any of them on
        // clears the others. Turning pin/follow-up on clears mute.
        let now_on = match flag {
            SessionFlag::Mute => {
                let on = !was.muted;
                self.update_flags(key.clone(), |f| {
                    f.muted = on;
                    if on {
                        f.pinned = false;
                        f.follow_up = false;
                    }
                });
                on
            }
            SessionFlag::Pin => {
                let on = !was.pinned;
                let seq = if on {
                    self.next_pin_seq = self.next_pin_seq.wrapping_add(1);
                    self.next_pin_seq
                } else {
                    0
                };
                self.update_flags(key.clone(), move |f| {
                    f.pinned = on;
                    f.pin_seq = seq;
                    if on {
                        f.muted = false;
                    }
                });
                on
            }
            SessionFlag::FollowUp => {
                let on = !was.follow_up;
                self.update_flags(key.clone(), |f| {
                    f.follow_up = on;
                    if on {
                        f.muted = false;
                    }
                });
                on
            }
        };

        let len = self.visible_len();
        if len > 0 {
            let target = match (flag, now_on) {
                // Mute: stay at the same row index so the next session slides up.
                (SessionFlag::Mute, true) => old_idx.unwrap_or(0).min(len - 1),
                // Clearing needs-input: the row drops out of the attention tier,
                // so don't keep the cursor on it. Move to the session that was
                // just after it (or the one just before it when it sat at the
                // end), following that session to wherever the re-sort floated it.
                (SessionFlag::FollowUp, false) => {
                    let visible = self.visible_sessions();
                    // Fallback chain kept identical: next → prev → self → 0.
                    follow_target
                        .as_ref()
                        .and_then(|tk| visible.iter().position(|s| matches_key(s, tk)))
                        .or_else(|| visible.iter().position(|s| matches_key(s, &key)))
                        .unwrap_or(0)
                }
                // Unmute / Pin / marking needs-input: follow the session itself.
                // Marking floats the row up to the attention tier and the cursor
                // rides with it, so the user stays on the session they just
                // flagged.
                (SessionFlag::Mute, false)
                | (SessionFlag::Pin, _)
                | (SessionFlag::FollowUp, true) => self
                    .visible_sessions()
                    .iter()
                    .position(|s| matches_key(s, &key))
                    .unwrap_or(0),
            };
            self.table_state.select(Some(target));
        }

        let label = match (flag, now_on) {
            (SessionFlag::Mute, true) => "Muted",
            (SessionFlag::Mute, false) => "Unmuted",
            (SessionFlag::Pin, true) => "Pinned",
            (SessionFlag::Pin, false) => "Unpinned",
            (SessionFlag::FollowUp, true) => "Marked needs input",
            (SessionFlag::FollowUp, false) => "Cleared needs input",
        };
        self.set_status(format!("{label} session {pid}"), false);
        self.save_overrides();
        // Persist the flag change into the restart snapshot too, so a crash
        // before the next reload doesn't restore the stale flag on recovery.
        self.save_session_snapshot();
    }

    /// Jump to the next session that needs attention (approval, decision, or
    /// needs-input). Pressing again cycles through all such sessions, wrapping
    /// around to the first one after the last.
    pub(super) fn jump_to_next_attention(&mut self) {
        let visible = self.visible_sessions();
        let current = self.table_state.selected().unwrap_or(usize::MAX);
        let attention_indices: Vec<usize> = visible
            .iter()
            .enumerate()
            .filter(|(_, s)| self.is_attention_row(s))
            .map(|(i, _)| i)
            .collect();
        if attention_indices.is_empty() {
            self.set_status("No sessions need attention".to_string(), false);
            return;
        }
        // Pick the first attention index strictly after current, or wrap to the first.
        let next = attention_indices
            .iter()
            .find(|&&i| i > current)
            .or_else(|| attention_indices.first())
            .copied()
            .unwrap();
        if next == current {
            self.set_status("Only one session needs attention".to_string(), false);
            return;
        }
        self.table_state.select(Some(next));
    }

    /// Select the N-th visible session (0-indexed). Returns None always —
    /// pure cursor move, no Kitty interaction.
    pub(super) fn select_visible_by_index(&mut self, idx: usize) -> Option<Action> {
        if idx < self.visible_len() {
            self.table_state.select(Some(idx));
        }
        None
    }

    /// Select the N-th visible session (0-indexed) and focus (or attach to) it,
    /// clearing the follow_up flag like Enter does. Routes through
    /// `focus_selected`, so a running remote row with no local window attaches
    /// over ssh exactly as Enter would. Returns None when the index is out of
    /// range or the session has nothing to focus/attach.
    pub(super) fn focus_visible_by_index(&mut self, idx: usize) -> Option<Action> {
        if idx >= self.visible_len() {
            return None;
        }
        self.table_state.select(Some(idx));
        self.focus_selected()
    }

    /// Build a `RestartSpec` for a session, or return None if it lacks the
    /// pieces we need to relaunch it (window id, live session id). Sessions
    /// that are still busy are also filtered out — restarting in the middle
    /// of a tool call would interrupt work the user hasn't reviewed.
    /// Build the spec to restart `s`, or the user-facing reason it can't be:
    /// a remote session, a non-idle one, or one missing the window/session id.
    /// `request_restart_selected` surfaces the `Err` verbatim; the restart-all
    /// path just filters on `.ok()`.
    pub(super) fn restart_spec_for(&self, s: &LauncherState) -> Result<RestartSpec, &'static str> {
        // Restart re-launches a *local* Kitty window and SIGTERMs a *local* pid;
        // a remote session's window_id/child_pid mean nothing here, so never
        // build a spec for one (guards both the selected and restart-all paths).
        if !s.host.is_local() {
            return Err("Only local sessions can be restarted");
        }
        if !matches!(s.status, SessionStatus::Idle | SessionStatus::Compacted) {
            return Err("Cannot restart: session must be idle (not active or waiting)");
        }
        let (Some(window_id), Some(session_id)) = (
            self.window_id_for_session(s),
            self.session_index.live_session_id(s).map(str::to_string),
        ) else {
            return Err("Cannot restart: session is missing window id or session id");
        };
        Ok(RestartSpec {
            agent: s.agent,
            child_pid: s.child_pid.unwrap_or(s.launcher_pid),
            window_id,
            cwd: s.cwd.clone(),
            session_id,
            flags: self.flags_of(&flag_key(s)),
            // User-initiated restart of a live session: tear the old child +
            // window down once the replacement launches.
            kill_old: true,
        })
    }

    /// Queue a confirmation to restart the currently-selected session. Returns
    /// without queuing if no session is selected, the session is busy, or it
    /// lacks the metadata we need (no window id / no live session id).
    pub(super) fn request_restart_selected(&mut self) {
        let Some(s) = self.selected_session() else {
            return;
        };
        // `restart_spec_for` owns the remote / not-idle / missing-metadata gates
        // and returns the reason, so we surface it directly instead of repeating
        // the checks here.
        let spec = match self.restart_spec_for(&s) {
            Ok(spec) => spec,
            Err(reason) => {
                self.set_status(reason.to_string(), true);
                return;
            }
        };
        // Clip to the table's name budget so the prompt matches the row label
        // (the panels that can afford the full name show it untruncated).
        let name = format::truncate_str(
            &format::session_display_name(&s, &self.session_index, &self.random_names),
            crate::config::get().ui.table.name_truncate,
        );
        self.pending_confirm = Some(PendingConfirm {
            prompt: format!("Restart session \"{name}\"? [y/N]"),
            action: Action::RestartSession(spec),
        });
        self.input_mode = InputMode::Confirm;
    }

    /// Queue a confirmation to restart every session. Refuses if any session
    /// is currently busy — partial restarts would leave the dashboard in a
    /// state where some agents picked up the new claude binary and others
    /// didn't, which is the exact problem this command exists to avoid.
    pub(super) fn request_restart_all(&mut self) {
        if self.sessions.is_empty() {
            self.set_status("No sessions to restart".to_string(), false);
            return;
        }
        // Only local sessions are restartable (see restart_spec_for); a busy
        // remote session must not block restarting the local ones.
        if self
            .sessions
            .iter()
            .filter(|s| s.host.is_local())
            .any(|s| !matches!(s.status, SessionStatus::Idle | SessionStatus::Compacted))
        {
            self.set_status(
                "Cannot restart all: every local session must be idle".to_string(),
                true,
            );
            return;
        }
        let specs: Vec<RestartSpec> = self
            .sessions
            .iter()
            .filter_map(|s| self.restart_spec_for(s).ok())
            .collect();
        if specs.is_empty() {
            self.set_status(
                "No restartable sessions (need window id and session id)".to_string(),
                true,
            );
            return;
        }
        let n = specs.len();
        let noun = plural_sessions(n);
        self.confirm_restart_all(specs, format!("Restart all {n} {noun}? [y/N]"));
    }

    /// Queue a y/N confirmation whose Yes restarts every session in `specs`.
    /// Shared by `request_restart_all` and `prompt_restart_missing` (each supplies
    /// its own prompt wording) so the `PendingConfirm` construction lives once.
    fn confirm_restart_all(&mut self, specs: Vec<RestartSpec>, prompt: String) {
        self.pending_confirm = Some(PendingConfirm {
            prompt,
            action: Action::RestartAll { sessions: specs },
        });
        self.input_mode = InputMode::Confirm;
    }

    /// Build one resume/browser picker row from a resumable candidate. Shared by
    /// the resume picker (`tag = None`) and the cross-host browser's resumable
    /// rows (`tag = Some("resumable")`), so the title precedence, meta assembly,
    /// and filter fields stay identical between the two. `tag`, when present,
    /// leads both the visible meta line and the filter text.
    fn resume_candidate_item(
        &self,
        host: &HostId,
        c: &ResumeCandidate,
        tag: Option<&str>,
    ) -> PickerItem {
        // Title precedence:
        // 1. Custom title from the transcript (e.g. user's rename).
        // 2. The session's name in the agent's session manifest.
        // 3. First real user prompt from the transcript.
        // 4. Synthetic `(session XXXXXXXX)` fallback.
        let saved_name = self.session_index.by_session_id.get(&c.session_id).cloned();
        let primary = c
            .custom_title
            .clone()
            .or_else(|| saved_name.clone())
            .or_else(|| c.first_prompt.clone())
            .unwrap_or_else(|| {
                format!(
                    "(session {})",
                    c.session_id.chars().take(8).collect::<String>(),
                )
            });
        let ago = format_relative_time(c.mtime);
        let cwd_short = self.shorten_path(&c.cwd).into_owned();
        let mut meta_parts: Vec<String> = Vec::new();
        if let Some(t) = tag {
            meta_parts.push(t.to_string());
        }
        meta_parts.push(format!("{ago} ago"));
        if let Some(b) = c.git_branch.as_deref() {
            meta_parts.push(b.to_string());
        }
        meta_parts.push(cwd_short);
        // Trail with the backend so a mixed Claude/Codex list is
        // distinguishable — two sessions can share a first prompt, cwd, and
        // branch but differ only by agent.
        meta_parts.push(c.agent.label().to_string());
        // In a cross-host list, lead the meta with the host so remote
        // candidates are distinguishable; omit it for local rows.
        if !host.is_local() {
            meta_parts.insert(0, host.0.clone());
        }
        let secondary = meta_parts.join("  ·  ");
        // Filter against tag + title + saved name + cwd + branch + agent + host
        // so users can search by any of them — saved names aren't shown for
        // entries that already have a custom title or first prompt, but they're
        // still useful for filtering.
        let mut filter_text = String::new();
        if let Some(t) = tag {
            filter_text.push_str(t);
            filter_text.push(' ');
        }
        filter_text.push_str(&format!(
            "{} {} {} {} {} {}",
            primary,
            saved_name.as_deref().unwrap_or(""),
            c.cwd,
            c.git_branch.as_deref().unwrap_or(""),
            c.agent.label(),
            host.0,
        ));
        PickerItem::new(primary)
            .with_secondary(secondary)
            .with_filter_text(filter_text)
    }

    /// Open the resume-session picker populated from the given candidates.
    /// Builds each `PickerItem`'s filter text from title + cwd + branch so the
    /// telescope-style filter matches the same fields the old direct filter did.
    pub(super) fn open_resume_picker(&mut self, candidates: Vec<(HostId, ResumeCandidate)>) {
        let items: Vec<PickerItem> = candidates
            .iter()
            .map(|(host, c)| self.resume_candidate_item(host, c, None))
            .collect();

        let picker = Picker::new("Resume Session", items)
            .with_placeholder("Search by title, path, or branch…")
            .with_size(80, 80);
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::Resume { candidates },
        });
        self.input_mode = InputMode::Picker;
    }

    /// Open the cross-host browser (§5): every running session (focus/attach)
    /// and every resumable one (resume), across all hosts, in one list. Each row
    /// is tagged `running`/`resumable` and (for remotes) its host.
    pub(super) fn open_browser_picker(&mut self, entries: Vec<BrowserEntry>) {
        let items: Vec<PickerItem> = entries
            .iter()
            .map(|e| match e {
                BrowserEntry::Running(s) => {
                    let title =
                        format::session_display_name(s, &self.session_index, &self.random_names);
                    let mut meta = vec!["running".to_string(), s.status.label().to_string()];
                    if !s.host.is_local() {
                        meta.insert(0, s.host.0.clone());
                    }
                    meta.push(self.shorten_path(&s.cwd).into_owned());
                    meta.push(s.agent.label().to_string());
                    // Include the agent label so a running row is filterable by
                    // backend too — matching the resumable rows and its own meta.
                    let filter =
                        format!("running {title} {} {} {}", s.cwd, s.host.0, s.agent.label());
                    PickerItem::new(title)
                        .with_secondary(meta.join("  ·  "))
                        .with_filter_text(filter)
                }
                // Shares the resume picker's row builder; the "resumable" tag
                // leads the meta and the filter. Widens the browser filter to
                // match on agent label + saved name too (the resume picker
                // already did).
                BrowserEntry::Resumable(host, c) => {
                    self.resume_candidate_item(host, c, Some("resumable"))
                }
            })
            .collect();

        let picker = Picker::new("Browse Sessions — all hosts", items)
            .with_placeholder("Search running + resumable across hosts…")
            .with_size(85, 80);
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::Browser { entries },
        });
        self.input_mode = InputMode::Picker;
    }

    /// Open the move-window-to-tab picker. The trailing `[New Tab]` entry is
    /// synthetic — selecting it maps to the Kitty `new` target.
    pub(super) fn open_move_tab_picker(&mut self, window_id: WindowId, tabs: Vec<TabInfo>) {
        let mut items: Vec<PickerItem> = tabs
            .iter()
            .map(|t| {
                let star = if t.is_focused { " *" } else { "" };
                let primary = format!("{}{star}", t.title);
                let secondary = format!(
                    "{} window{}",
                    t.window_count,
                    if t.window_count == 1 { "" } else { "s" }
                );
                // `with_secondary` no longer folds into the filter, so match the
                // old "primary + secondary" filter text explicitly (the window
                // count stays searchable, e.g. filtering by tab title).
                let filter = format!("{primary} {secondary}");
                PickerItem::new(primary)
                    .with_secondary(secondary)
                    .with_filter_text(filter)
            })
            .collect();
        items.push(PickerItem::new("[New Tab]"));

        let picker = Picker::new("Move to Tab", items)
            .with_placeholder("Filter tabs…")
            .with_size(60, 60);
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::MoveTab { window_id, tabs },
        });
        self.input_mode = InputMode::Picker;
    }

    pub(super) fn select_next(&mut self) {
        let len = self.visible_len();
        if len == 0 {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map_or(0, |i| (i + 1).min(len - 1));
        self.table_state.select(Some(i));
    }

    pub(super) fn select_prev(&mut self) {
        let len = self.visible_len();
        if len == 0 {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map_or(0, |i| i.saturating_sub(1));
        self.table_state.select(Some(i));
    }

    pub(super) fn scroll_preview_up(&mut self) {
        let max = self.preview_max_scroll();
        self.preview_scroll = (self.preview_scroll + 8).min(max);
    }

    pub(super) fn scroll_preview_down(&mut self) {
        self.preview_scroll = self.preview_scroll.saturating_sub(8);
    }

    pub(super) fn scroll_preview_left(&mut self) {
        self.preview_h_scroll = self.preview_h_scroll.saturating_sub(8);
    }

    /// Schedule a re-fetch of the current preview window. Goes through the
    /// same debounce path as a selection change, so rapid triggers (e.g. a
    /// burst of FocusGained events) coalesce into a single fetch.
    pub(super) fn request_preview_refresh(&mut self) {
        if self.preview_dirty_since.is_none() {
            self.preview_dirty_since = Some(Instant::now());
        }
    }

    /// Whether the periodic preview auto-refresh should fire: the dashboard
    /// has terminal focus (no `kitten @ get-text` churn while the user is
    /// away), the panel is visible and showing a live window, the selected
    /// session is busy (an at-rest session produces no new output to fetch;
    /// once a reload shows it busy again the timer resumes, and the fetch
    /// fires immediately since `preview_fetched_at` is stale by then), the
    /// user is following the live tail (any scroll offset means they're reading
    /// history — a refresh would yank the buffer out from under them), and
    /// the last fetch attempt is older than `interval`. A zero `interval`
    /// disables the timer.
    pub(super) fn wants_preview_auto_refresh(&self, interval: Duration) -> bool {
        !interval.is_zero()
            && self.focused
            && self.preview_visible
            && self.preview_window_id.is_some()
            && self.preview_scroll == 0
            && self.preview_h_scroll == 0
            && self
                .selected_session_ref()
                .is_some_and(|s| s.status.is_busy())
            && self
                .preview_fetched_at
                .is_some_and(|t| t.elapsed() >= interval)
    }

    /// Set or clear the cached preview text, invalidating the parsed-lines
    /// cache so the next `draw_preview` re-parses ANSI from the new content,
    /// and stamping/clearing `preview_updated_at` for the staleness label.
    /// `preview_updated_at` tracks when the content last actually *changed*, so
    /// a byte-identical re-fetch leaves the timestamp alone — otherwise a
    /// busy-but-stalled session's auto-refresh would keep resetting the age and
    /// the "updated <age> ago" label would never appear. Returns whether the
    /// previous value was `Some`.
    pub(super) fn set_preview_text(&mut self, text: Option<String>) -> bool {
        let was_some = self.preview_text.is_some();
        match &text {
            // Only re-stamp when the content differs from what's displayed.
            Some(t) if self.preview_text.as_deref() != Some(t.as_str()) => {
                self.preview_updated_at = Some(Instant::now());
            }
            Some(_) => {} // identical content: leave the age growing.
            None => self.preview_updated_at = None,
        }
        self.preview_text = text;
        self.preview_lines = None;
        self.preview_max_width = 0;
        was_some
    }

    /// Staleness label for the preview title: `Some("updated <1m ago")` /
    /// `Some("updated 3m ago")` once the displayed content is older than
    /// `thresholds.preview_stale_secs` (0 shows it whenever content is
    /// present). `None` while fresh or while nothing is displayed. Minute
    /// resolution, same rule as the table's "Updated" column. The run loop
    /// redraws whenever this string changes, so the age keeps ticking on an
    /// otherwise idle dashboard.
    pub(super) fn preview_age_label(&self) -> Option<String> {
        let age = self.preview_updated_at?.elapsed().as_secs();
        let threshold = crate::config::get().thresholds.preview_stale_secs;
        (age >= threshold).then(|| format!("updated {} ago", format_coarse_age(age)))
    }

    pub(super) fn scroll_preview_right(&mut self) {
        self.preview_h_scroll = self.preview_h_scroll.saturating_add(8);
    }

    pub(super) fn preview_max_scroll(&self) -> usize {
        match self.preview_text.as_deref() {
            Some(raw) => raw.lines().count().saturating_sub(4),
            None => 0,
        }
    }

    /// Open the workdir picker: recent cwds as suggestions, free-form path
    /// entry via the text input, Tab for filesystem completion. Where the new
    /// session lands is decided at spawn time by the current [`SessionsLayout`]
    /// (`resolve_spawn_target`), not by the selected window.
    pub(super) fn open_workdir_picker(&mut self) {
        // New sessions default to the local host; `Ctrl-h` cycles to a remote,
        // which re-seeds the list + home from that machine (`reseed_workdir_for_host`).
        let host = HostId::local();
        self.workdir_host_home = self.home_dir.clone();
        let items = self.workdir_items(&self.recent_cwds, &host);

        // Seed the launch backend from the persistent default; `Ctrl-t` in the
        // picker overrides it for this launch only. The title carries it so the
        // backend is visible at the moment of launching.
        let agent = self.new_session_agent;
        let picker = Picker::new(workdir_picker_title(agent, &host), items)
            .with_placeholder("Type a path or pick a recent one…")
            .with_size(70, 70)
            .with_free_input(true)
            .with_tab_completion(true);
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::Workdir { agent, host },
        });
        self.input_mode = InputMode::Picker;
    }

    /// Build picker items for a list of cwds shown against `host`'s home. Only a
    /// *local* dir gets a custom directory-mark icon (marks are a local concept,
    /// keyed by local path); remote dirs render plain.
    fn workdir_items(&self, cwds: &[String], host: &HostId) -> Vec<PickerItem> {
        let local = host.is_local();
        cwds.iter()
            .map(|cwd| {
                let display = collapse_tilde(cwd, &self.workdir_host_home);
                let mut item = PickerItem::new(display.clone())
                    .with_filter_text(format!("{display} {cwd}"))
                    .with_payload(cwd.clone());
                if local && self.directory_marks.contains_key(cwd.trim_end_matches('/')) {
                    let (icon, color, _) = self.effective_dir_mark(cwd);
                    item = item.with_prefix(icon, color);
                }
                item
            })
            .collect()
    }

    /// Directory completions for `prefix` on `host`'s filesystem. Local reads the
    /// fs in-process (no `block_in_place`, so it's usable outside a runtime — e.g.
    /// unit tests); a *connected* remote makes a blocking RPC off the async worker.
    /// A not-yet-connected remote returns no completions rather than blocking the
    /// TUI through the connect attempt (`request()` queues while Connecting).
    fn host_complete_path(&self, host: &HostId, prefix: &str) -> Vec<String> {
        if host.is_local() {
            self.backend_for(host).complete_path(prefix)
        } else if self.backend_for(host).conn_state() == ConnState::Connected {
            tokio::task::block_in_place(|| self.backend_for(host).complete_path(prefix))
        } else {
            Vec::new()
        }
    }

    /// Re-seed the open workdir picker for its currently-selected host: pull that
    /// host's recent dirs + `$HOME` (local in-process, remote over RPC — blocks,
    /// so wrap the call site in `block_in_place`), rebuild the item list, and
    /// reset the input/cursor. Called after `Ctrl-h` changes the host so the
    /// picker always reflects the machine the launch will land on.
    pub(super) fn reseed_workdir_for_host(&mut self) {
        let Some(active) = self.picker.as_ref() else {
            return;
        };
        let PickerKind::Workdir { host, .. } = &active.kind else {
            return;
        };
        let host = host.clone();
        // Local reads memory (authoritative — reflects in-picker deletes); a
        // *connected* remote fetches from its server. A remote that isn't fully
        // connected yet (Connecting/Disconnected) is skipped rather than RPC'd —
        // `request()` would queue through the whole connect attempt and freeze the
        // TUI (block_in_place on the event loop); an empty list is shown instead
        // and the user can re-`Ctrl-h` once it connects.
        let (cwds, home) = if host.is_local() {
            (self.recent_cwds.clone(), self.home_dir.clone())
        } else if self.backend_for(&host).conn_state() == ConnState::Connected {
            tokio::task::block_in_place(|| self.backend_for(&host).recent_dirs())
        } else {
            (Vec::new(), String::new())
        };
        self.workdir_host_home = home;
        let items = self.workdir_items(&cwds, &host);
        self.workdir_completion = None;
        if let Some(active) = self.picker.as_mut() {
            active.picker.items = items;
            // Clears the input, resets the item cursor, drops any stale error.
            active.picker.set_text("");
        }
    }

    /// Drop the highlighted recent-cwd from the workdir picker. Persists the
    /// shorter list and removes the matching item from the live picker so the
    /// user can keep deleting without reopening. No-op if the picker isn't a
    /// Workdir picker, the highlighted item has no cwd payload, or the
    /// filtered list is empty.
    pub(super) fn delete_selected_recent_cwd_in_picker(&mut self) {
        let Some(active) = self.picker.as_mut() else {
            return;
        };
        // Only the *local* recent list is the dashboard's to edit; a remote
        // host's list lives on that machine (deleting there would need an RPC —
        // out of scope), so Ctrl-D is a no-op while the picker targets a remote.
        if !matches!(&active.kind, PickerKind::Workdir { host, .. } if host.is_local()) {
            return;
        }
        let filtered = active.picker.filtered();
        if filtered.is_empty() {
            return;
        }
        let cursor = active.picker.cursor.min(filtered.len() - 1);
        let item_idx = filtered[cursor];
        let Some(payload) = active.picker.items[item_idx].payload.clone() else {
            return;
        };

        let key = payload.trim_end_matches('/').to_string();
        let before = self.recent_cwds.len();
        self.recent_cwds.retain(|c| c.trim_end_matches('/') != key);
        if self.recent_cwds.len() == before {
            return;
        }
        self.save_recent_cwds();

        let active = self.picker.as_mut().expect("checked above");
        active.picker.items.remove(item_idx);
        let new_total = active.picker.filtered().len();
        if new_total == 0 {
            active.picker.cursor = 0;
        } else if cursor >= new_total {
            active.picker.cursor = new_total - 1;
        } else {
            active.picker.cursor = cursor;
        }
    }

    /// Tab-complete the workdir picker's input against the *selected host's*
    /// filesystem (local in-process, remote over RPC). If the current text is
    /// still one of our cached candidates, cycle to the next; otherwise re-seed
    /// from the current prefix. No-op if no picker is active.
    pub(super) fn complete_workdir_in_picker(&mut self) {
        let Some(active) = self.picker.as_ref() else {
            return;
        };
        let PickerKind::Workdir { host, .. } = &active.kind else {
            return;
        };
        let host = host.clone();
        let current = active.picker.input.text().to_string();

        // Cycle through the cached list if the current text is still one of its
        // members. This covers the usual "Tab, Tab, Tab" flow where the input
        // keeps matching the last-completed entry.
        if let Some(state) = self.workdir_completion.as_mut()
            && let Some(pos) = state.matches.iter().position(|m| *m == current)
        {
            state.index = (pos + 1) % state.matches.len();
            let next = state.matches[state.index].clone();
            if let Some(active) = self.picker.as_mut() {
                active.picker.set_text(&next);
            }
            return;
        }

        // Re-seed against the current prefix. The backend returns absolute dir
        // paths on the host's filesystem; expansion/collapse use the host's home
        // (`workdir_host_home`) so `~` resolves against the *remote* machine when
        // the launch targets it. The remote path blocks on a round-trip.
        let expanded = expand_tilde(&current, &self.workdir_host_home);
        let matches_abs = self.host_complete_path(&host, &expanded);
        if matches_abs.is_empty() {
            return;
        }
        let matches: Vec<String> = matches_abs
            .iter()
            .map(|p| collapse_tilde(p, &self.workdir_host_home))
            .collect();

        if let Some(active) = self.picker.as_mut() {
            active.picker.set_text(&matches[0]);
        }
        self.workdir_completion = Some(WorkdirCompletion { matches, index: 0 });
    }

    pub(super) fn shorten_path<'a>(&self, path: &'a str) -> std::borrow::Cow<'a, str> {
        collapse_tilde(path, &self.home_dir).into()
    }

    /// The display color for a host label: its configured color, else the
    /// theme's `title_fg` (Cyan by default). Shared by the table's Host column
    /// and the picker footer's `Ctrl-h` host hint so an unconfigured host reads
    /// identically at both.
    pub(super) fn host_label_color(&self, host: &HostId) -> ratatui::style::Color {
        self.host_colors
            .get(host)
            .copied()
            .unwrap_or_else(|| crate::config::get().colors.ui.title_fg)
    }
}

#[cfg(test)]
mod tests;
