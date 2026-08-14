mod bindings;
mod draw;
mod format;
mod hosts;
mod keybind_log;
mod keymap;
mod keys;
mod logo;
mod picker;
mod render_backend;
mod run;

pub use run::{read_dashboard_window_id, run};

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::TableState;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::agent::{AgentControl, ResumeCandidate, SessionIndex};
use crate::state::{
    self, HostId, LauncherState, SessionFlags as HostSessionFlags, SessionKey, SessionStatus,
};
use crate::terminal::{Capabilities, SessionsLayout, Tab, TabId, TabInfo, TabTarget, WindowId};

use self::format::{
    contains_ci, format_coarse_age, format_relative_time, random_session_name, workdir_picker_title,
};
use self::picker::{Picker, PickerItem};
use crate::backend::{Backend, BackendEvents, ConnState, KillOutcome, RemoteBackend, Transport};
use crate::config;

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
        /// `Some(name)` launches into a fresh agent-created git worktree rather
        /// than in `cwd` itself (`Ctrl-g` in the picker); an empty name lets the
        /// agent generate one.
        worktree: Option<String>,
    },
    FetchTabsForMove(WindowId),
    MoveWindow(WindowId, TabTarget),
    /// Fetch `host`'s resumable list and open the resume picker on it. Scoped
    /// to one host (§9): the old cross-host union made every picker's scope
    /// implicit, and `Ctrl-h` inside the picker switches hosts explicitly.
    FetchResumeList {
        host: HostId,
    },
    /// Re-scope the *open* resume picker to another host — the `Ctrl-h` switch.
    /// A separate action from `FetchResumeList` so a failed switch can keep the
    /// picker open on its previous host instead of dismissing it.
    SwitchResumeHost {
        host: HostId,
    },
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
        /// Opaque session identity; the owning host resolves it to a live pid
        /// at signal time, so a stale row can't make it kill a recycled pid.
        key: SessionKey,
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
    /// spawn `ssh -t <host> miao-server attach <pool_session>` and bind it.
    AttachRemoteRunning {
        host: HostId,
        pool_session: String,
        /// Steal the session from the client currently attached to it. Only ever
        /// set behind an explicit y/N confirm — the pool is one client at a
        /// time, so attaching otherwise declines rather than kicking someone.
        force: bool,
    },
    /// Attach a local window to each `(host, pool_session)` in turn — the
    /// dashboard pre-filters the list to detached sessions nobody else holds,
    /// so every one of these is a plain, non-stealing attach.
    AttachAll {
        targets: Vec<(HostId, String)>,
    },
    /// Deploy this dashboard's server to `host` and restart its daemon, then
    /// resume everything the restart killed. Raised by `u` in the hosts panel,
    /// past both its gate and its confirm.
    UpgradeHost {
        host: HostId,
    },
    /// Allow a host's connection task to download a published `miao-server`.
    ///
    /// The only action that carries a reply channel, because it answers a
    /// question the *backend* asked rather than starting something the user
    /// did. Firing it sends `true`; **dropping it is a refusal**, which is what
    /// makes `n`, Esc and quitting all decline without a branch of their own —
    /// `handle_confirm_key` already drops the pending action on anything but
    /// `y`, so the safe answer is the default one.
    GrantConsent(tokio::sync::oneshot::Sender<bool>),
}

/// Inputs needed to restart a single session: kill the old child, then launch
/// a new captain-miao launcher with `--resume <session_id>` next to the old
/// window. `agent` is captured so the restart targets the same backend the
/// session was originally launched under.
#[derive(Debug, Clone)]
pub(super) struct RestartSpec {
    pub(super) agent: AgentControl,
    /// Host the session lives on; the replacement opens there too, so a remote
    /// restart lands in that host's pool rather than silently moving the session
    /// to the laptop.
    pub(super) host: HostId,
    /// The session to tear down, named opaquely — its host resolves it to a pid
    /// at signal time. `None` for crash recovery, where nothing is left to kill.
    pub(super) key: SessionKey,
    /// The local window to close after relaunching, if the dashboard has one. A
    /// detached pooled session has none.
    pub(super) window_id: Option<WindowId>,
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
            Action::FetchResumeList { .. } => "FetchResumeList",
            Action::SwitchResumeHost { .. } => "SwitchResumeHost",
            Action::ResumeSession { .. } => "ResumeSession",
            Action::KillSession { .. } => "KillSession",
            Action::DetachRemote { .. } => "DetachRemote",
            Action::OpenShellTab { .. } => "OpenShellTab",
            Action::RestartSession(_) => "RestartSession",
            Action::RestartAll { .. } => "RestartAll",
            Action::CopySessionId(_) => "CopySessionId",
            Action::AttachRemoteRunning { .. } => "AttachRemoteRunning",
            Action::AttachAll { .. } => "AttachAll",
            Action::UpgradeHost { .. } => "UpgradeHost",
            Action::GrantConsent(_) => "GrantConsent",
        }
    }
}

/// An armed worktree request inside the workdir picker.
///
/// The name is edited **in place** rather than in a second popup: the workdir
/// picker *is* `self.picker`, so a sub-picker would have to stash and restore
/// it, and a modal that hides the path list to ask for a name would take the
/// two decisions apart when they're made together. Instead `naming` moves the
/// keyboard onto this field and back, leaving the path input's text, filter and
/// highlight untouched the whole time.
#[derive(Debug, Default)]
pub(crate) struct WorktreeArm {
    /// Worktree name; empty means "let the agent generate one". Passed through
    /// verbatim, so a `#1234` PR reference reaches the agent intact.
    pub(crate) name: self::picker::TextInput,
    /// While set, picker keys edit [`Self::name`] instead of the path.
    pub(crate) naming: bool,
}

impl WorktreeArm {
    /// The name as the launch should carry it, trimmed. Empty ⇒ agent-generated.
    fn requested_name(&self) -> String {
        self.name.text().trim().to_string()
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
    /// Resume one of `host`'s dormant sessions. One host at a time — `Ctrl-h`
    /// switches, exactly like `Ctrl-t` switches the agent — so the list's scope
    /// is always visible in the title instead of being an implicit union (§9).
    Resume {
        host: HostId,
        candidates: Vec<ResumeCandidate>,
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
        /// `Some` to launch into a fresh git worktree instead of the cwd
        /// itself, armed in-picker with `Ctrl-g` (per-launch only, never
        /// persisted — unlike the agent and host defaults, isolation is a
        /// property of the *task* you're starting, not a standing preference).
        ///
        /// Always `None` for an agent that can't do it
        /// (`AgentControl::supports_worktrees`); a `Ctrl-t` onto such an agent
        /// clears it rather than carrying a request that would be silently
        /// dropped at launch.
        worktree: Option<WorktreeArm>,
    },
    /// Set the persistent default backend for new sessions (`Space a`).
    DefaultAgent,
    /// Set the persistent default host for new sessions (`Space H`).
    DefaultHost,
    /// Pick an emoji to drop into the directory-mark editor's icon field.
    /// Opened with `Ctrl-E` from `Space i`; submit/cancel return to the editor
    /// (which stays live in `self.dir_edit`) rather than the normal view.
    Emoji,
    /// The same picker, opened from the hosts panel's Icon field; submit/cancel
    /// return to the panel.
    HostEmoji,
}

#[derive(Debug)]
pub(super) struct ActivePicker {
    pub(in crate::app) picker: Picker,
    pub(in crate::app) kind: PickerKind,
}

/// One host's resumable list, delivered back to the run loop from the
/// background fetch that [`App::start_resume_load`] kicked off.
#[derive(Debug)]
pub(super) struct ResumeLoad {
    /// The `App::resume_seq` value the request was issued under. A reply whose
    /// seq is no longer current belongs to a host the user has already switched
    /// away from and is dropped — otherwise a slow host's answer would land on
    /// top of a fast one the user is already reading.
    pub(super) seq: u64,
    /// Whether the request re-scoped an already-open picker (`Ctrl-h`) rather
    /// than opening one. Decides what an *empty* answer does: a fresh open
    /// closes the popup and reports on the status line (there is nothing to act
    /// on, and the popup only ever existed to hold the pending list), while a
    /// switch keeps it open on the error so the user can try another host
    /// instead of being dumped back to the table.
    pub(super) reseed: bool,
    pub(super) host: HostId,
    pub(super) candidates: Vec<ResumeCandidate>,
    pub(super) errors: Vec<String>,
}

/// Why a session was killed, which is what decides whether its outcome is worth
/// saying out loud once it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KillOrigin {
    /// The user pressed `x`. They are owed an answer, including the one that
    /// arrives a round trip after the row already left.
    Asked,
    /// The window-close policy (`[remote] on_window_close`). Silent by design —
    /// see `run::close_reported_sessions`, whose doc explains why: the queue is
    /// filled from a report that can only ever race the session's own end, and
    /// there is nothing the user could do about a failure anyway. A row coming
    /// back is its own report.
    WindowClosed,
}

/// What became of a kill the dashboard sent, delivered back to the run loop from
/// the background round trip [`run::start_kill`] kicked off.
///
/// [`run::start_kill`]: super::app::run
#[derive(Debug)]
pub(super) struct KillResult {
    pub(super) host: HostId,
    pub(super) key: state::SessionKey,
    pub(super) outcome: KillOutcome,
    pub(super) origin: KillOrigin,
}

/// Persisted dashboard overrides (pin/needs-input) so they survive restarts.
/// A file written before mute was removed still carries a `muted` list; serde
/// drops it as an unknown field, which is exactly the wanted migration.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DashboardOverrides {
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
    /// Persisted default host for new-session operations (`Space H`), stored as
    /// the host label. The exact analog of `default_agent`: `O`, a bare `o`, and
    /// `r` all target it, so every picker's scope is explicit instead of an
    /// implicit cross-host union (§9). `None` (or a label no longer configured)
    /// falls back to localhost.
    #[serde(default)]
    default_host: Option<String>,
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
    /// Status flags (pinned / follow-up) the session carried at
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

/// What the table cursor does across a mutation that changes a **sort key**.
///
/// The session list is filtered and sorted, and the selection is an index into
/// that projection — so pinning, muting, a status flip, an attach or a detach
/// all move rows past a cursor that cannot feel it. Every such mutation goes
/// through [`App::mark_dirty`], which takes one of these; there is no default,
/// because all four are genuinely in use and picking one silently is how the
/// cursor ends up on a session the user never chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cursor {
    /// Stay on the session the cursor is on, wherever the re-sort moves it.
    /// The common case, and what a user means by "I was looking at this one".
    FollowSession,
    /// Move to a specific session, identified **before** the mutation. Clearing
    /// a follow-up bell uses this to advance to the next triage target: the row
    /// being cleared is exactly the one you're done with.
    Follow(FlagKey),
    /// Leave the index alone (clamped) and let whatever re-sorts into it come
    /// to the cursor. Muting wants this — you're working down a list, so the
    /// next row should arrive under your finger. It is also the honest answer
    /// when a mutation invalidates the caches for *rendering* reasons without
    /// reordering anything.
    HoldIndex,
    /// Back to the top, because the list is now a different list (search).
    Top,
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
    /// `Some` while the selected row's fields have the keyboard — see
    /// [`RowEdit`]. `None` in the list.
    pub(in crate::app) edit: Option<RowEdit>,
    /// The row a `d` press is asking about — the removal confirm (§9). `None`
    /// when nothing is pending.
    pub(in crate::app) pending_remove: Option<usize>,
    /// What a `u` press put on screen — a question to answer, or a refusal to
    /// acknowledge. Kept beside `pending_remove` rather than folded into the
    /// global [`PendingConfirm`] because that one switches `InputMode`, which
    /// would tear this panel down mid-question.
    pub(in crate::app) pending_upgrade: Option<UpgradePrompt>,
    /// The connection log open over the list (`l`). `Some` replaces the list
    /// view entirely — it wants the whole popup, since the text it exists to
    /// show is what didn't fit on a row.
    pub(in crate::app) log_view: Option<HostLogView>,
}

/// The hosts panel's row editor: which field has the keyboard, and what `Esc`
/// puts back.
///
/// One `Option` rather than an `editing` flag beside a focus and a snapshot: an
/// entry point that set two of the three and forgot the third would compile,
/// and the one it would forget is the snapshot — which is the difference
/// between `Esc` restoring a mistyped target and losing the old one. There are
/// three entry points (`a`, `e`/`Enter`, and the `^`-key that opens the editor
/// on a named field), so that is a live risk rather than a hypothetical one.
#[derive(Debug)]
pub(in crate::app) struct RowEdit {
    pub(in crate::app) focus: HostField,
    pub(in crate::app) origin: EditOrigin,
}

/// What `Esc` undoes in the hosts panel's row editor.
///
/// The panel has no Save step — a commit persists immediately (§9) — so its
/// counterpart has to be a real cancel, and a cancel needs the pre-edit
/// contents from somewhere. A row the edit *created* has none: abandoning it
/// removes it again, which is also what stops a half-typed `(unnamed)` row from
/// lingering in the list until the panel is reopened.
#[derive(Debug)]
pub(in crate::app) enum EditOrigin {
    Existing(HostRow),
    Added,
}

impl HostEditState {
    /// Start editing the selected row on `focus`, recording what `Esc` restores.
    pub(in crate::app) fn begin_edit(&mut self, focus: HostField) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        self.edit = Some(RowEdit {
            focus,
            origin: EditOrigin::Existing(row.clone()),
        });
    }

    /// Append a blank row and edit it from the Label field. `Esc` removes it
    /// again — an empty row is not a host, and never became one on disk
    /// ([`App::apply_host_edits`] filters it), so leaving it in the list would
    /// only be a lie about what is configured.
    pub(in crate::app) fn begin_new_row(&mut self) {
        self.rows.push(HostRow::default());
        self.cursor = self.rows.len() - 1;
        self.edit = Some(RowEdit {
            focus: HostField::Label,
            origin: EditOrigin::Added,
        });
    }

    /// Abandon the edit in progress, restoring what was there before it.
    /// Persists nothing: no mutation reaches disk between `begin_edit` and the
    /// commit, so putting the row back is the whole of the undo.
    pub(in crate::app) fn cancel_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        match edit.origin {
            EditOrigin::Existing(row) => {
                if let Some(slot) = self.rows.get_mut(self.cursor) {
                    *slot = row;
                }
            }
            EditOrigin::Added => {
                if self.cursor < self.rows.len() {
                    self.rows.remove(self.cursor);
                }
                self.cursor = self.cursor.min(self.rows.len());
            }
        }
    }

    /// The field with the keyboard, or `None` in the list.
    pub(in crate::app) fn focus(&self) -> Option<HostField> {
        self.edit.as_ref().map(|e| e.focus)
    }
}

/// One session an upgrade will kill, recorded so it can be brought back on the
/// other side of the restart.
///
/// A resume, not a re-launch: the agent's transcript on the host outlives the
/// pool session dying, so the restored row continues the same conversation. The
/// `SessionKey`, the pid and the pool name are all newly minted — only
/// `session_id` crosses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RestoreSpec {
    pub(super) agent: AgentControl,
    /// Host-canonical (`~`-collapsed), exactly as it arrived and exactly as
    /// [`OpenSpec`](cm_core::backend::OpenSpec) wants it back.
    pub(super) cwd: String,
    pub(super) session_id: String,
}

/// The outcome of one host's upgrade, delivered back to the run loop from the
/// background ssh that performed it.
#[derive(Debug)]
pub(super) struct UpgradeReport {
    pub(super) host: HostId,
    /// `None` on success. On failure this is what the host said, and the host
    /// comes back up on whatever it was already running.
    pub(super) error: Option<String>,
}

/// The line a `u` press leaves in the hosts panel.
///
/// One type for both outcomes because they render identically and are dismissed
/// identically; only `actionable` decides whether `y` does anything. Keeping the
/// refusal on screen matters — this panel has no status line (its footer is key
/// hints), so a message set anywhere else would surface stale, after the panel
/// closed, or not at all.
#[derive(Debug)]
pub(super) struct UpgradePrompt {
    pub(in crate::app) row: usize,
    pub(in crate::app) text: String,
    /// `false` for a refusal: any key dismisses it and nothing happens.
    pub(in crate::app) actionable: bool,
}

/// One rendered line of a host's connection log — see [`App::host_log_lines`].
#[derive(Debug, Clone)]
pub(super) struct HostLogLine {
    /// How long ago the entry happened, on its **first** line only; `None` on
    /// the continuation lines of a multi-line entry.
    pub(super) age: Option<String>,
    pub(super) error: bool,
    pub(super) text: String,
}

/// The hosts panel's connection-log view (`l`), scrolled over one host's
/// [`ConnLogEntry`](crate::backend::ConnLogEntry) list.
#[derive(Debug)]
pub(super) struct HostLogView {
    pub(in crate::app) host: HostId,
    /// First visible line, counted in *physical* lines — a host's multi-line
    /// refusal scrolls like the paragraph it is, not as one indivisible entry.
    pub(in crate::app) scroll: usize,
    /// Content rows the last draw had. Recorded there because `G` and PageDown
    /// need a viewport height, and the popup's size is only known while
    /// rendering; 0 until the first frame, which just makes those keys no-ops
    /// for one frame.
    pub(in crate::app) rows: usize,
}

/// One editable host row in the popup.
///
/// The four text fields are [`TextInput`](picker::TextInput)s rather than bare
/// `String`s. They hold ssh targets and argument lines long enough that fixing a
/// typo in the middle has to be possible, which needs a cursor — and the widget
/// that has one already backs every picker's query and the directory-mark
/// editor's icon field, so the readline keys are the same ones here.
#[derive(Debug, Clone, Default)]
pub(super) struct HostRow {
    pub(in crate::app) label: picker::TextInput,
    /// ssh target (`user@host`) or, when `is_socket`, a socket path.
    pub(in crate::app) target: picker::TextInput,
    pub(in crate::app) is_socket: bool,
    /// Per-host emoji shown beside the workdir icon, picked with the same
    /// searchable picker as the workdir marks. Empty = derive one from the label.
    pub(in crate::app) icon: picker::TextInput,
    /// Suspended — see [`hosts::HostConfig::disabled`]. Toggled with `c`.
    pub(in crate::app) disabled: bool,
    /// ssh arguments as one line of text — see [`hosts::HostConfig::options`].
    /// Edited as text rather than as a list of rows because the whole set is
    /// nearly always one or two arguments, and a sub-list inside a popup row
    /// would need its own cursor, its own add/remove keys and its own footer.
    pub(in crate::app) options: picker::TextInput,
    /// Offer this host the clipboard — see [`hosts::HostConfig::clipboard`].
    /// A form field, toggled with `Space`: the panel's plain letters are for
    /// things you do *to* a row (connect, delete, upgrade), and this is part of
    /// what a host **is**, like its options. Being a field also means it shows its
    /// own state — `[off]` is visible the moment the editor opens, where a list
    /// key was only discoverable from the footer.
    pub(in crate::app) clipboard: bool,
}

impl HostRow {
    /// The `HostId` this row configures — its label, trimmed exactly as
    /// [`App::apply_host_edits`] trims it on the way to disk, so a lookup
    /// against the live backends matches a row still being typed.
    pub(in crate::app) fn host(&self) -> HostId {
        HostId(self.label.text().trim().to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum HostField {
    Label,
    Target,
    Options,
    Icon,
    /// The one field with nothing to type — see [`HostRow::clipboard`].
    Clipboard,
}

impl HostField {
    /// Form order — the order the fields are drawn in, which is the order the
    /// focus keys walk.
    ///
    /// `Clipboard` is last rather than beside `Options`, where it belongs by
    /// meaning: the four text fields keep the Tab positions fingers already know,
    /// and `^e`'s "open the editor on Icon" stays the fourth stop it names.
    const ORDER: [HostField; 5] = [
        HostField::Label,
        HostField::Target,
        HostField::Options,
        HostField::Icon,
        HostField::Clipboard,
    ];

    /// The next field, forwards or back. Wraps: the form is a ring, so
    /// overshooting the last field costs one more press either way.
    pub(in crate::app) fn step(self, forward: bool) -> Self {
        let n = Self::ORDER.len();
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let next = if forward { i + 1 } else { i + n - 1 };
        Self::ORDER[next % n]
    }
}

/// What one configured host's backend is built *from* — see
/// [`App::conn_identities`], which is the gate deciding whether committing a
/// panel row reconnects anything. A named struct rather than a tuple because
/// every field is either a `String` or an `Option<String>`: two of them being
/// swapped at a call site would compile, and would then reconnect on the wrong
/// edits forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct ConnIdentity {
    label: String,
    ssh: Option<String>,
    socket: Option<String>,
    disabled: bool,
    /// The arguments as typed. Raw rather than split into options + forwards,
    /// because a rebuild is what re-runs the split — gating on its *output*
    /// would miss an edit that only moves a token between the two.
    options: Vec<String>,
    /// Part of the identity because the clipboard is one more `-R` on the tunnel
    /// child: toggling it has to re-dial, or the forward would not appear (or
    /// disappear) until the next reconnect for some other reason.
    clipboard: bool,
}

/// Supervision for the clipboard bridge's server child (`miao clipboard serve`).
///
/// The same shape as the ssh tunnel child: a `kill_on_drop` process the dashboard
/// holds for as long as it wants the resource. It runs only while some host is
/// actually offered the clipboard, so a user who never enables it never has a
/// second process — and toggling the last host off stops it, which is the
/// direction that matters.
///
/// Two mechanisms tie the child's life to ours, and the split is deliberate:
/// `kill_on_drop` covers an orderly drop, and the **stdin pipe** covers everything
/// else. The child reads that pipe and exits when the kernel closes our end, so a
/// SIGKILL'd dashboard — which never gets to run any cleanup — still takes the
/// server with it. Which is why `child.stdin` is deliberately left in place and
/// `Child::wait` is never called on it: `wait` closes stdin, so it would tell a
/// perfectly healthy child that we had died. Only `try_wait` is used.
#[derive(Default)]
pub(super) struct ClipboardSupervisor {
    child: Option<tokio::process::Child>,
    /// Whether any host wants it, remembered so the per-tick [`Self::poll`] needs
    /// no re-read of `hosts.json`.
    wanted: bool,
    /// Earliest next spawn attempt after a failure.
    retry_at: Option<Instant>,
    backoff: Duration,
}

/// Retry bounds for a clipboard server that won't start or won't stay up,
/// mirroring the connection task's. It retries indefinitely at the cap for the
/// same reason that one does: the cause is usually transient, and the alternative
/// is a feature that silently stays dead until the dashboard is restarted.
const CLIPBOARD_RETRY_INITIAL: Duration = Duration::from_millis(500);
const CLIPBOARD_RETRY_MAX: Duration = Duration::from_secs(30);

impl ClipboardSupervisor {
    /// Called when the host list changes.
    pub(super) fn set_wanted(&mut self, wanted: bool) {
        self.wanted = wanted;
        self.poll();
    }

    /// Called every run-loop iteration: notices a child that died and respawns
    /// it. One non-blocking `waitpid` when the server is wanted, nothing at all
    /// when it isn't.
    pub(super) fn poll(&mut self) {
        if !self.wanted {
            if self.child.take().is_some() {
                tracing::info!("no host is offered the clipboard; stopping the server");
            }
            self.retry_at = None;
            self.backoff = Duration::ZERO;
            return;
        }
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => return,
                Ok(Some(status)) => {
                    // Exit 0 is the child finding a live server already on the
                    // socket and standing down. Either way we respawn on the
                    // backoff, and either way that spawn will stand down too — so
                    // the loop is bounded by the cap, not by us being clever.
                    tracing::warn!(%status, "clipboard server exited; respawning");
                    self.child = None;
                    self.back_off();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not check on the clipboard server");
                    return;
                }
            }
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        match spawn_clipboard_server() {
            Ok(child) => {
                tracing::info!(pid = child.id(), "clipboard server started");
                self.child = Some(child);
                self.retry_at = None;
                self.backoff = Duration::ZERO;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not start the clipboard server");
                self.back_off();
            }
        }
    }

    fn back_off(&mut self) {
        self.backoff = if self.backoff.is_zero() {
            CLIPBOARD_RETRY_INITIAL
        } else {
            (self.backoff * 2).min(CLIPBOARD_RETRY_MAX)
        };
        self.retry_at = Some(Instant::now() + self.backoff);
    }
}

/// Spawn `miao clipboard serve` — this same binary, so there is nothing to
/// install or find.
fn spawn_clipboard_server() -> std::io::Result<tokio::process::Child> {
    let exe = std::env::current_exe()?;
    // Truncated on every spawn, exactly like the ssh-forward log: this file is
    // the child's only channel out, and one that only grew would accumulate a
    // year of pastes for the sake of the last one.
    let log_dir = state::state_dir().join("logs");
    let _ = state::create_dir_all_private(&log_dir);
    let stderr = std::fs::File::create(log_dir.join("clipboard-serve.log"))
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    tokio::process::Command::new(exe)
        .args(["clipboard", "serve"])
        // The pipe *is* the parent-death signal — see [`ClipboardSupervisor`].
        .stdin(std::process::Stdio::piped())
        // Must never touch the TUI's terminal; the alt-screen owns it.
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
}

/// The remote hosts, counted by how usable each one is — see
/// [`App::remote_host_tally`]. Separate numbers rather than one "unhealthy"
/// count because the header colors them apart: a host that is *failing* (a
/// diagnosis waiting in the hosts panel) is a different call to action than one
/// merely re-dialing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct HostTally {
    pub(super) good: usize,
    pub(super) error: usize,
    pub(super) down: usize,
    /// Hosts still dialing — deliberately *not* folded into `down`, and the one
    /// bucket the header prints no number for. A number appearing beside the
    /// cloud reads as a problem, and a host mid-handshake isn't one yet; it
    /// blinks the cloud instead (`draw::host_tally_spans`).
    pub(super) connecting: usize,
}

impl HostTally {
    /// No remote hosts configured at all (every bucket empty).
    pub(super) fn is_empty(&self) -> bool {
        self.good == 0 && self.error == 0 && self.down == 0 && self.connecting == 0
    }
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
    /// whether a session spawn anchors next to a window or gets its own tab;
    /// `capture` gates the preview fetch and its auto-refresh timer.
    pub(super) capabilities: Capabilities,
    /// Backend used when starting a new session (`o` / `O`). Seeded from
    /// `launcher.default_agent`, cycled with `Space a`.
    pub(super) new_session_agent: AgentControl,
    /// How new sessions are arranged (`resolve_spawn_target`): the shared
    /// `miao:sessions` tab (Stacked) or one tab per session (Per-tab). Seeded from
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
    /// Download-consent questions from the hosts' connection tasks.
    ///
    /// The backends run on their own tasks and cannot open a popup; this is how
    /// they ask. Drained by the run loop only while nothing else owns the
    /// screen, so a question can never displace an open picker — an unread one
    /// simply waits in the channel.
    pub(super) consent_prompts: tokio::sync::mpsc::UnboundedReceiver<crate::backend::ConsentPrompt>,
    /// Resumable lists arriving from a background fetch, and the sequence number
    /// that tells a live one from a stale one.
    ///
    /// A remote `ListResumable` is a blocking round trip over ssh; running it on
    /// the UI thread froze the dashboard for its whole duration, which is what
    /// made `Ctrl-h` in the resume picker feel broken. The picker now opens
    /// empty and interactive, and the list lands here when it lands. `seq` is
    /// bumped per request, so a user who switches hosts twice in a second gets
    /// the *second* answer, not whichever host replied last.
    pub(super) resume_loads: tokio::sync::mpsc::UnboundedReceiver<ResumeLoad>,
    pub(super) resume_tx: tokio::sync::mpsc::UnboundedSender<ResumeLoad>,
    pub(super) resume_seq: u64,
    /// Hosts held down for the duration of a server upgrade: no backend, no
    /// connection task, no redial. Deliberately **not** the persisted `disabled`
    /// flag — a dashboard that dies mid-upgrade must not leave a host suspended
    /// in the user's config file.
    pub(super) upgrading: HashSet<HostId>,
    /// What each upgrading host owed its user: the sessions it killed, waiting
    /// for that host to come back so they can be resumed. Held until the
    /// reconnect edge fires, or until the upgrade reports a failure.
    pub(super) upgrade_restores: HashMap<HostId, Vec<RestoreSpec>>,
    pub(super) upgrade_reports: tokio::sync::mpsc::UnboundedReceiver<UpgradeReport>,
    pub(super) upgrade_tx: tokio::sync::mpsc::UnboundedSender<UpgradeReport>,
    /// Kills coming back from the round trip that carried them, for the same
    /// reason the resume list does: a remote `KillSession` is an ssh round trip,
    /// and running it on the UI thread meant `x` froze the dashboard until the
    /// host answered — the whole span in which the row it killed sat there
    /// looking alive. The row now goes at the keystroke
    /// (`Backend::presume_killed`) and the answer lands here, where it is either
    /// nothing to do or grounds to put the row back.
    ///
    /// No sequence number, unlike `resume_loads`: each result names the session
    /// it belongs to, so two kills in flight can't be confused for one another.
    pub(super) kill_results: tokio::sync::mpsc::UnboundedReceiver<KillResult>,
    pub(super) kill_tx: tokio::sync::mpsc::UnboundedSender<KillResult>,
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
    /// A window a just-spawned session should get selection on once it appears,
    /// with the instant it was set. `reload_sessions` selects the matching row
    /// then clears it — but if the launcher dies before writing a state file the
    /// id would linger, so the instant lets an unclaimed target age out (see
    /// [`PENDING_FOCUS_MAX_AGE`]).
    pub(super) pending_focus_window: Option<(WindowId, Instant)>,
    /// The pool session an attach is running for *right now* — drives the
    /// "Attaching…" overlay.
    ///
    /// An attach runs inline in the run loop: it plans the argv, spawns a window
    /// and (for a remote host) waits on the terminal backend, so the frame is
    /// frozen for the whole call. Set from the action *before* the pre-action
    /// draw and cleared when the attach returns, so `Enter` on a detached row
    /// acknowledges the keypress immediately instead of reading as a dead key
    /// for the round trip (§9).
    pub(super) attaching: Option<String>,
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
    /// an exited session's pane is an invisible leak buried in the shared
    /// sessions tab; elsewhere a window on screen is one whose occupant chose to
    /// stay, so nothing is queued.
    pub(super) reap_window_queue: Vec<WindowId>,
    /// What closing a session's window does to the session — `[remote]
    /// on_window_close`, resolved once at construction. Held rather than read
    /// from `config::get()` at the report, so the behaviour is a property of
    /// this dashboard rather than of whatever config file the test runner's
    /// machine happens to have.
    pub(super) on_window_close: config::OnWindowClose,
    /// Sessions to end on their host because the user closed the window showing
    /// them and [`Self::on_window_close`] says `close` (the default), each held
    /// until its [`CLOSE_ON_WINDOW_CLOSE_DELAY`] is up. Filled by
    /// `apply_detach_reports`, drained by the run loop via
    /// [`Self::take_due_session_closes`], which does the RPC.
    ///
    /// Queued rather than done inline for the reason every host call is: a
    /// remote kill is a blocking round trip, and `apply_detach_reports` runs on
    /// the UI thread outside `block_in_place`. The *delay* on top is what makes
    /// a terminal quitting non-destructive — see the constant.
    pub(super) pending_session_close: Vec<PendingClose>,
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
    /// Session-name index **per host**, never merged (§3): the shards are keyed
    /// by bare pid, so unioning them let a remote pid collide with a local one
    /// and hand a local row the remote's session id — which then flowed into
    /// restart, fork, and crash recovery. Look one up with [`App::index_for`],
    /// always keyed by the row's own host.
    pub(super) session_indexes: HashMap<HostId, SessionIndex>,
    /// Per-host session backends, aggregated into one view. `backends[0]` is
    /// this machine — the in-process [`Backend::Local`], or, under
    /// pooled-localhost, a `Remote` over a socket to the local daemon (never
    /// both: they read the same `sessions/` dir and `collect_sessions` doesn't
    /// dedup). The rest are remote (SSH) hosts. Reload unions their sessions,
    /// tagging each with its host.
    pub(super) backends: Vec<Backend>,
    /// One change-signal handle per entry of `backends`, in the same order.
    /// Rebuilt whenever `backends` is — every backend, local included, now
    /// reports its own changes (§5), so the run loop has no filesystem
    /// knowledge of its own.
    pub(super) backend_events: Vec<BackendEvents>,
    /// The clipboard bridge's server child — see [`ClipboardSupervisor`].
    pub(super) clipboard_server: ClipboardSupervisor,
    /// Last-seen reconnect counter per remote host. A bump means the host went
    /// Disconnected → Connected, which fires the auto-reattach sweep (§7).
    pub(super) reconnect_epochs: HashMap<HostId, u64>,
    /// `(host, pool_session)` pairs whose attach window the run loop should
    /// respawn — filled by the reconnect sweep, drained like
    /// `failed_launch_focus_queue` (the reload has no terminal access).
    pub(super) pending_reattach: Vec<(HostId, String)>,
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
    /// This dashboard's own `TERM`, read once at startup. The yardstick the
    /// detail panel measures a row's [`LauncherState::terminfo`] against: same value
    /// means the session renders against the same terminfo we do and there is
    /// nothing to say, so the panel dims it.
    pub(super) terminfo: Option<String>,
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
    /// Per-host emoji for the icon column, from the hosts config. A host with
    /// none configured falls back to a deterministic emoji derived from its
    /// label, so the column always reads as icons rather than truncated names.
    pub(super) host_icons: HashMap<HostId, String>,
    /// The host every new-session operation targets by default (`Space H`) —
    /// `O`, a bare `o` with nothing selected, and `r`. `o` on a row and a fork
    /// still follow *that row's* host; this is only the no-context default.
    /// Persisted in `dashboard-overrides.json`.
    pub(super) default_host: HostId,
    /// Per-host recent-dir cache for the workdir picker, seeded at connect and
    /// invalidated when a launch records a new cwd. The picker is cache-first
    /// (§9): switching hosts must render instantly, and the rule the whole
    /// picker follows is *never put a round trip between a keystroke and its
    /// echo*.
    pub(super) recent_dirs_cache: HashMap<HostId, Vec<String>>,
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
    /// Per-session override flags (pinned / follow-up), sparse:
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

/// Path segment the agent puts its worktrees under, relative to the repo root.
const WORKTREE_SEGMENT: &str = "/.claude/worktrees/";

/// Split a cwd into `(repo root, worktree name)` when it sits inside the
/// agent's default worktree layout, else `(cwd, None)`.
///
/// Pure string work on purpose — no `git rev-parse`, no filesystem. The
/// dashboard is a viewer that must answer this for **remote** rows too, whose
/// filesystem it cannot touch, and it answers it on render paths where a
/// subprocess per row is out of the question. The cost is that a worktree
/// relocated by a `WorktreeCreate` hook, or one made by hand with
/// `git worktree add ../elsewhere`, isn't recognized — it just reads as an
/// ordinary directory, which is the same behaviour as before this existed.
///
/// The name may itself contain `/` (the agent allows `feature/auth`), so the
/// whole remainder is the name rather than its first segment.
pub(super) fn split_worktree(cwd: &str) -> (&str, Option<&str>) {
    let trimmed = cwd.trim_end_matches('/');
    // Filesystem root: trimming ate the whole string, and `""` is not a path.
    if trimmed.is_empty() {
        return (cwd, None);
    }
    match trimmed.find(WORKTREE_SEGMENT) {
        Some(i) => {
            let name = &trimmed[i + WORKTREE_SEGMENT.len()..];
            let root = &trimmed[..i];
            // A trailing `/.claude/worktrees` with nothing after it is the
            // container, not a worktree.
            if name.is_empty() {
                (trimmed, None)
            } else {
                (root, Some(name))
            }
        }
        None => (trimmed, None),
    }
}

/// The key a cwd's directory mark (icon + colour) is stored and looked up
/// under: the **repo root**, so every worktree of a project inherits the mark
/// the project was given and `Space i` on a worktree row edits that one mark
/// rather than minting a per-worktree copy nothing else reads. A mark answers
/// "which project is this row", and a worktree does not change the answer.
pub(super) fn dir_mark_key(cwd: &str) -> &str {
    split_worktree(cwd).0
}

/// Basename for display, naming both halves inside a worktree
/// (`captain-miao@feature-auth`) and the plain basename elsewhere. Used for tab
/// titles, where the worktree name alone would be an orphan — several repos can
/// hold a `feature-auth`, and the tab bar is the one place with no other clue
/// which checkout a tab belongs to.
pub(super) fn display_basename(cwd: &str) -> std::borrow::Cow<'_, str> {
    match split_worktree(cwd) {
        (root, Some(name)) => format!("{}@{}", cwd_basename(root), name).into(),
        (path, None) => cwd_basename(path).into(),
    }
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

/// How long an attach has to survive before a failure is read as "the link
/// died" rather than "it never got going". Sized for the slow half of a real
/// attach — an ssh handshake plus shpool's connect — not for a refusal, which
/// comes back at once.
///
/// Also handed to the attach wrapper (`backend::report_on_exit_argv`), which
/// applies the same test to decide whether to hold its window open, so the two
/// can't drift.
pub(crate) const ATTACH_STARTUP_GRACE: Duration = Duration::from_secs(10);

/// Whether a finished attach's window has anything left worth looking at, given
/// how long it ran and how it exited.
///
/// The wrapper applies this same test to decide whether to keep its window (see
/// [`ATTACH_STARTUP_GRACE`], which it is passed), so the two outcomes agree on
/// which windows are on screen; this side decides what the dashboard does about
/// them:
///
/// * **Spent** — it attached, ran, and ended (a clean detach, a broken pipe, a
///   dead ControlMaster). The wrapper exits, so the window is already closing;
///   the close here is the backstop for a wrapper that never ran (no resolvable
///   reporter exe) or a backend that held the window anyway.
/// * **Refused** — it exited non-zero almost immediately: the busy guard, a
///   stale name, an ssh that couldn't authenticate. The wrapper is holding the
///   window at its "press Enter" prompt, because it has the only copy of that
///   error (the dashboard never sees the attach's stderr), so the dashboard
///   leaves it alone and points at it instead.
///
/// 129/130/143 are 128 + HUP/INT/TERM — exactly the signals the wrapper traps,
/// and each one means the window was torn down under it (closing a window
/// SIGHUPs its foreground group). That is never a refusal, so it counts as spent
/// whatever the duration says; without the carve out, closing a window within
/// the grace would be announced as a failed attach, pointing at a window that no
/// longer exists. The range is spelled out rather than tested as `>= 128`
/// because **ssh exits 255**, which that would swallow — and 255 is precisely
/// the ambiguous status the duration is there to resolve.
///
/// Both halves of the remaining test are load-bearing. Status alone can't
/// separate the rest: ssh reports a mid-session drop and a failure to connect
/// with the same 255. Duration alone can't either: it would keep a window for
/// every session someone detaches inside the grace. A missing status (a reporter
/// that couldn't determine one) reads as clean — closing a window that had an
/// error in it is a milder failure than leaving a corpse on screen after every
/// dropped link.
/// How long a session waits between its window being closed and being ended
/// (`[remote] on_window_close = "close"`).
///
/// The delay is the guard, not a nicety. A report says "this window's pty went
/// away", and the case that must not be mistaken for a deliberate close is a
/// terminal *quitting*, which tears down every window at once — the dashboard's
/// own among them. `ReportOrigin::Backlog` covers the reports that arrive after
/// we're gone; this covers the sliver where we're still running. The dashboard
/// installs no SIGHUP handler and its `event::poll` fails on a dead pty, so it
/// dies within milliseconds of that teardown: outliving a whole second of it is
/// not something a quitting terminal does, while a session ending a second after
/// you close its window reads as immediate.
///
/// A queued close is therefore **dropped if the dashboard exits first**, quit
/// included. That is the right way round: the cost of dropping one is a session
/// left running, which `x` fixes in a keystroke; the cost of the reverse is a
/// terminal quit that ends every session on the host — the exact thing pooling
/// exists to prevent.
const CLOSE_ON_WINDOW_CLOSE_DELAY: Duration = Duration::from_secs(1);

/// One session waiting out [`CLOSE_ON_WINDOW_CLOSE_DELAY`] before it is ended.
pub(super) struct PendingClose {
    pub(super) host: HostId,
    pub(super) key: state::SessionKey,
    /// When the kill may go out. Compared against a caller-supplied `now`, so
    /// the wait is testable without one.
    pub(super) due: Instant,
}

/// Where a batch of detach reports came from, which decides whether ending the
/// session behind a closed window is on the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReportOrigin {
    /// Drained while the dashboard was running, so it watched the window die:
    /// a close is the user's doing and `[remote] on_window_close` applies.
    Live,
    /// Drained at startup — reports left by windows that died while the
    /// dashboard was down. **Never** ends a session, because the biggest batch
    /// of these is the one a quitting terminal produces: it SIGHUPs every attach
    /// window on its way out, and the dashboard (living in that same terminal)
    /// dies with them. By status alone that is indistinguishable from the user
    /// closing each window by hand, so quitting the terminal would end every
    /// session on the host.
    Backlog,
}

/// Whether an attach ended because the **window** was taken away from it, rather
/// than because the attach itself finished or failed.
///
/// This is the whole basis for `[remote] on_window_close`, so it is deliberately
/// narrow — 129 and nothing else:
///
/// * `129` (128 + SIGHUP) — the terminal tore the pty down under a live attach.
///   That is what closing a window (or its tab) does, and nothing else routinely
///   produces it.
/// * `255` — ssh, for a dropped link *and* a failure to connect alike. The
///   window goes away here too, but the session is precisely what survived the
///   failure; ending it would turn every flaky link, and every laptop resume,
///   into lost work.
/// * `0` — the attach returned: an in-session shpool detach, which already means
///   "leave it running".
/// * `130`/`143` — SIGINT/SIGTERM. Spent, so the window closes, but they reach
///   the wrapper by routes that aren't a window closing (a Ctrl-C in the pane, a
///   stray `kill`), and the default here ends a session. Not worth the guess.
fn closed_by_the_user(status: Option<i32>) -> bool {
    status == Some(129)
}

fn attach_window_is_spent(held_for: Duration, status: Option<i32>) -> bool {
    match status {
        None | Some(0) => true,
        // 128 + HUP / INT / TERM.
        Some(129 | 130 | 143) => true,
        Some(_) => held_for >= ATTACH_STARTUP_GRACE,
    }
}

/// Whether a session matches a `FlagKey`, without allocating the session's own
/// key (which clones the host `String`). For the per-row `position`/`find`
/// scans that only need equality, not a key.
pub(super) fn matches_key(s: &LauncherState, key: &FlagKey) -> bool {
    s.host == key.0 && s.launcher_pid == key.1
}

/// The dashboard's own tab label: the binary's name, carrying the attention
/// count when there is one.
///
/// `0` renders as a bare `miao` rather than `miao (0)`. The number exists to
/// catch the eye from a tab bar the dashboard isn't on; one that is *always*
/// there stops doing that, and a parenthesised zero reads like a defect besides.
/// Pure, so the one thing about this feature that is worth pinning is testable
/// without a terminal.
pub(super) fn dashboard_tab_title(attention: usize) -> String {
    if attention == 0 {
        "miao".to_string()
    } else {
        format!("miao ({attention})")
    }
}

/// Pluralize "session" for the restart-confirmation prompts and the hosts
/// panel's per-host count.
pub(super) fn plural_sessions(n: usize) -> &'static str {
    if n == 1 { "session" } else { "sessions" }
}

/// What [`App::build_backends_from_config`] produces: the backends plus the
/// per-host display attributes the table reads. A named struct rather than a
/// tuple so a second attribute can join without every call site shifting.
pub(super) struct HostSetup {
    pub backends: Vec<Backend>,
    pub host_icons: HashMap<HostId, String>,
}

/// Start this host's daemon if it isn't already up, for pooled-localhost
/// (§10.1). `daemon ensure` is idempotent and self-daemonizing, so this is safe
/// to run on every dashboard start; it prints the socket path, which we ignore
/// (the path is a shared constant — `state::server_sock_path`).
///
/// Errors when the server binary isn't installed, which is the case worth
/// reporting: pooled mode is opt-in, so a user who set the flag and has no
/// `miao-server` wants to know rather than silently get direct-local.
fn ensure_local_daemon() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let out = Command::new("miao-server")
        .args(["daemon", "ensure"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run `miao-server daemon ensure`: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`miao-server daemon ensure` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The `HostId` this machine's pooled backend is tagged with. Not `"local"` —
/// that label is reserved for the in-process backend and gates behaviour
/// (`is_local()`) that a pooled session genuinely doesn't want.
fn local_host_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "this-host".to_string())
}

/// Title for the resume picker, naming the host it is listing — the scope is
/// part of the question, since `Ctrl-h` switches it (§9).
fn resume_picker_title(host: &HostId) -> String {
    if host.is_local() {
        "Resume Session".to_string()
    } else {
        format!("Resume Session on {}", host.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SessionFlags {
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
        !self.pinned && !self.follow_up
    }
}

impl App {
    pub(super) fn new() -> Self {
        let home_dir = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default();
        // Give the backends somewhere to ask about downloads before any of them
        // is constructed below — a connection task can start probing
        // immediately, and with no channel set it would (safely) refuse.
        let (consent_tx, consent_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::backend::set_consent_channel(consent_tx);
        let (resume_tx, resume_rx) = tokio::sync::mpsc::unbounded_channel();
        let (upgrade_tx, upgrade_rx) = tokio::sync::mpsc::unbounded_channel();
        let (kill_tx, kill_rx) = tokio::sync::mpsc::unbounded_channel();
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

        let HostSetup {
            mut backends,
            host_icons,
        } = Self::build_backends_from_config(&HashSet::new());
        // Every backend reports its own changes now (§5), so the run loop needs
        // no filesystem watcher of its own; subscribe once, here.
        let backend_events = backends.iter_mut().map(Backend::subscribe).collect();

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
            consent_prompts: consent_rx,
            resume_loads: resume_rx,
            resume_tx,
            upgrading: HashSet::new(),
            upgrade_restores: HashMap::new(),
            upgrade_reports: upgrade_rx,
            upgrade_tx,
            kill_results: kill_rx,
            kill_tx,
            resume_seq: 0,
            dir_edit: None,
            host_edit: None,
            directory_marks: HashMap::new(),
            recent_cwds: Vec::new(),
            workdir_completion: None,
            pending_focus_window: None,
            attaching: None,
            failed_launch_focus_queue: Vec::new(),
            reap_window_queue: Vec::new(),
            on_window_close: cfg.remote.on_window_close,
            pending_session_close: Vec::new(),
            window_tab_cache: HashMap::new(),
            work_tabs: HashMap::new(),
            session_indexes: HashMap::new(),
            backends,
            backend_events,
            clipboard_server: ClipboardSupervisor::default(),
            reconnect_epochs: HashMap::new(),
            pending_reattach: Vec::new(),
            window_bindings: bindings::WindowBindings::default(),
            terminal_identity: crate::terminal::get().identity(),
            terminfo: std::env::var("TERM")
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            foreign_bindings: Vec::new(),
            next_launch_id: 0,
            host_icons,
            default_host: HostId::local(),
            recent_dirs_cache: HashMap::new(),
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

    /// Invalidate the derived caches (visible order, dir labels) after a
    /// mutation, and apply `cursor` to the table selection.
    ///
    /// The cursor argument is **required, with no default**, because the
    /// selection is a bare index into the *sorted* projection: any mutation
    /// that touches a sort key slides rows past it, so the index survives while
    /// the session it names does not. Bumping the version invalidates the order
    /// cache and says nothing about the index derived from it — which is
    /// precisely how four binding paths ended up re-iconing a row and leaving
    /// the cursor on whichever session took its slot. Every answer in [`Cursor`]
    /// is deliberately in use here, so there is no safe default to pick
    /// silently; making the caller name one is the whole point.
    ///
    /// **Call this only once the mutation has fully landed.** Unlike the bare
    /// version bump it replaced, every arm below *reads* the visible order —
    /// `clamp_selection`/`reset_selection`/`reselect` all resolve it, which
    /// recomputes the index list and re-caches it under the freshly bumped
    /// version. Invalidating mid-mutation therefore doesn't invalidate anything:
    /// it re-caches the *stale* projection as current, and since `cached_visible`
    /// holds indices into `sessions`, a later shrink of that Vec then indexes out
    /// of bounds. `reload_sessions` did exactly that and panicked on every reload
    /// that dropped a row.
    pub(super) fn mark_dirty(&mut self, cursor: Cursor) {
        // Read the anchor *before* the bump, while the cached order still
        // describes the pre-mutation list.
        let anchor = matches!(cursor, Cursor::FollowSession)
            .then(|| self.anchored_key())
            .flatten();
        self.mutation_version = self.mutation_version.wrapping_add(1);
        match cursor {
            // No anchor (cold cache — see `anchored_key`) degrades to holding
            // the index, which is what this did before the cursor existed.
            Cursor::FollowSession => match anchor {
                Some(key) => self.reselect(&key),
                None => self.clamp_selection(),
            },
            Cursor::Follow(key) => self.reselect(&key),
            Cursor::HoldIndex => self.clamp_selection(),
            Cursor::Top => self.reset_selection(),
        }
    }

    /// The selected session's key as of the **last computed** visible order, or
    /// `None` when that cache is cold.
    ///
    /// Deliberately does not recompute on a miss, unlike every other reader.
    /// [`mark_dirty`](Self::mark_dirty) calls this after the mutation has
    /// already landed, so a recompute would sort the *new* list and hand back
    /// the session that just took the cursor's slot — the exact wrong answer,
    /// and one indistinguishable from the right one. `None` instead lets the
    /// caller fall back to holding the index. In the running dashboard the
    /// cache is warm whenever this runs (every frame draws the list, and
    /// mutations land between frames); a cold read is a test that mutated
    /// before anything rendered.
    fn anchored_key(&self) -> Option<FlagKey> {
        let i = self.table_state.selected()?;
        let cache = self.cached_visible.borrow();
        let (version, indices) = cache.as_ref()?;
        if *version != self.mutation_version {
            return None;
        }
        self.sessions.get(*indices.get(i)?).map(flag_key)
    }

    pub(super) fn flags_of(&self, key: &FlagKey) -> SessionFlags {
        self.flags.get(key).copied().unwrap_or_default()
    }

    pub(super) fn is_follow_up(&self, key: &FlagKey) -> bool {
        self.flags_of(key).follow_up
    }

    /// How many sessions are soliciting attention right now — the number the
    /// dashboard's own tab label carries (see [`dashboard_tab_title`]).
    ///
    /// Counted over *every* session, not the visible projection: a search filter
    /// narrows what you are looking at, never what wants you, and the tab label is
    /// read precisely when the dashboard is not on screen.
    pub(super) fn attention_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| self.is_attention_row(s))
            .count()
    }

    /// Whether a session is currently soliciting attention: one that either
    /// needs a live response (approval / decision / failed-to-start, plus
    /// review-pending via `needs_attention`) or carries a user follow-up flag
    /// while at rest. This is the union of the attention sort-ranks in
    /// `compute_visible_indices` (which splits it into finer tiers for
    /// ordering); `jump_to_next_attention` uses it minus the detached rows,
    /// which it can't act on from here.
    pub(super) fn is_attention_row(&self, s: &LauncherState) -> bool {
        let flags = self.flags_of(&flag_key(s));
        s.status.needs_attention() || (flags.follow_up && !s.status.is_busy())
    }

    /// Apply bell sentinels dropped into the sessions dir by
    /// `miao focus --window-id <id>`. Each pid that still has a live
    /// session gets `follow_up = true`; entries for dead pids are silently
    /// dropped. Persists overrides only if at least one flag actually changed.
    pub(super) fn apply_bell_signals(&mut self, pids: Vec<u32>) {
        // Bell sentinels come from `miao focus --window-id`, which only
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
            self.update_flags(key, Cursor::FollowSession, |f| {
                f.follow_up = true;
            });
            changed = true;
        }
        if changed {
            self.save_overrides();
        }
    }

    /// Adopt the flags a host serves for its own sessions (§9).
    ///
    /// Pins and bells for a pooled host live in that host's sidecar, not in this
    /// dashboard's `dashboard-overrides.json`, so every dashboard attached to
    /// the host — and a phone-ssh user on the box itself — sees the same ones,
    /// and they survive a dashboard restart. `App.flags` stays the single
    /// in-memory source the sort and filter read; this just keeps it in step
    /// with what the host says. `pin_seq` is deliberately *not* adopted: pin
    /// ordering is a local presentation concern, so a locally-issued sequence
    /// number is kept when the flag itself doesn't change.
    fn adopt_host_flags(&mut self) {
        let served: Vec<(FlagKey, HostSessionFlags)> = self
            .sessions
            .iter()
            .filter_map(|s| s.flags.map(|f| (flag_key(s), f)))
            .collect();
        for (key, host_flags) in served {
            let mine = self.flags_of(&key);
            if mine.pinned == host_flags.pinned && mine.follow_up == host_flags.follow_up {
                continue;
            }
            // Newly pinned by someone else: issue a local sequence number so it
            // sorts among this dashboard's pins like any other.
            let seq = if host_flags.pinned && !mine.pinned {
                self.next_pin_seq = self.next_pin_seq.wrapping_add(1);
                self.next_pin_seq
            } else {
                mine.pin_seq
            };
            self.update_flags(key, Cursor::FollowSession, move |f| {
                f.pinned = host_flags.pinned;
                f.follow_up = host_flags.follow_up;
                f.pin_seq = seq;
            });
        }
    }

    /// Push a session's flags to its host when that host owns them, so the
    /// change reaches every other dashboard watching it. Returns whether the
    /// host took them — `false` means "this host doesn't serve flags", the
    /// caller's signal to persist them in `dashboard-overrides.json` instead.
    fn publish_flags(&self, key: &FlagKey, flags: SessionFlags) -> bool {
        let (host, pid) = key;
        let Some(backend) = self.backend_for(host) else {
            return false;
        };
        if !backend.capabilities().pooled {
            return false;
        }
        let wire = HostSessionFlags {
            pinned: flags.pinned,
            follow_up: flags.follow_up,
        };
        tokio::task::block_in_place(|| {
            backend.set_session_flags(&SessionKey::from_launcher_pid(*pid), wire)
        })
    }

    /// Mutate a session's flags; removes the entry entirely if the result is
    /// all-false to keep the map sparse.
    /// Every flag here (pinned / follow-up) is a sort key, so this takes a
    /// [`Cursor`] like any other re-sorting mutation rather than picking one
    /// for its callers — clearing a bell deliberately wants a different answer
    /// from setting one.
    pub(super) fn update_flags(
        &mut self,
        key: FlagKey,
        cursor: Cursor,
        update: impl FnOnce(&mut SessionFlags),
    ) {
        let mut f = self.flags_of(&key);
        update(&mut f);
        if f.is_default() {
            self.flags.remove(&key);
        } else {
            self.flags.insert(key, f);
        }
        self.mark_dirty(cursor);
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
        // Restored pins float rows up; the user keeps whatever they were on.
        self.mark_dirty(Cursor::FollowSession);
        true
    }

    /// Update the search filter. Wrapping this in a setter is important: it
    /// bumps `mutation_version`, invalidating the visible/dir-labels caches —
    /// and, since a filter change makes this a different list, sends the cursor
    /// back to the top. Owning that here rather than at the four key handlers is
    /// what fixed `ClearSearch`, the one of them that had forgotten to do it.
    pub(super) fn set_search_filter(&mut self, filter: Option<String>) {
        if self.search_filter != filter {
            self.search_filter = filter;
            self.mark_dirty(Cursor::Top);
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
            if f.follow_up {
                overrides.follow_up.push(*pid);
            }
        }
        overrides.prevent_sleep = Some(self.prevent_sleep_enabled);
        overrides.default_agent = Some(self.new_session_agent.cli_subcommand().to_string());
        overrides.sessions_layout = Some(self.sessions_layout.label().to_string());
        overrides.default_host = Some(self.default_host.0.clone());
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
        if let Some(h) = overrides.default_host.filter(|h| !h.is_empty()) {
            // Kept even when that host isn't currently configured — the user may
            // re-add it. `default_host_or_local` resolves the fallback at use.
            self.default_host = HostId(h);
        }
        if let Some(l) = overrides
            .sessions_layout
            .as_deref()
            .and_then(SessionsLayout::from_label)
        {
            self.sessions_layout = l;
        }
        // Startup: nothing is selected yet, and none of these reorder the list.
        self.mark_dirty(Cursor::HoldIndex);
    }

    /// Whether a session runs on the machine the dashboard runs on.
    ///
    /// Keyed on `backends[0]`'s host rather than [`HostId::is_local`]: under
    /// pooled-localhost this machine's own sessions are served by a
    /// remote-*shaped* backend tagged with the hostname, and they are still ours
    /// — they die with this machine exactly like direct-local ones.
    pub(super) fn runs_on_this_machine(&self, s: &LauncherState) -> bool {
        self.backends.first().is_some_and(|b| b.host_id() == s.host)
    }

    /// True iff at least one session **on this machine** is currently working —
    /// `Active`, `Compacting`, or `BackgroundActive` (a short-term background task
    /// the agent is waiting on, which can itself peg the CPU), via `is_busy`. Used
    /// to decide when to actually run caffeinate; sleep during Idle /
    /// WaitingForApproval / BackgroundServer is fine because the agent isn't
    /// working (a parked long-running dev server is deliberately not counted —
    /// see `is_busy`) and macOS just pauses the process either way.
    ///
    /// **Remote sessions never keep this machine awake.** They run in the far
    /// host's pty pool, wholly detached from this dashboard — suspending the
    /// laptop watching them costs them nothing (the attach window reconnects on
    /// wake, which is what the pool is *for*), so caffeinating a machine for work
    /// happening on another one is pure battery burn.
    pub(super) fn has_active_session(&self) -> bool {
        self.sessions
            .iter()
            .any(|s| s.status.is_busy() && self.runs_on_this_machine(s))
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

    /// Open the default-host picker (`Space H`) — the exact analog of the
    /// default-agent one. Every new-session operation with no row context (`O`,
    /// a bare `o`, `r`) targets whatever this selects, which is what let the
    /// cross-host unions go away: each picker's scope is now a stated default
    /// rather than "everything, merged" (§9).
    pub(super) fn open_default_host_picker(&mut self) {
        let current = self.default_host_or_local();
        let hosts: Vec<(HostId, ConnState)> = self.host_states();
        let items: Vec<PickerItem> = hosts
            .iter()
            .map(|(host, state)| {
                let mut item = PickerItem::new(host.0.clone()).with_payload(host.0.clone());
                if !state.is_connected() {
                    item = item.with_secondary(state.label().to_string());
                }
                item.with_prefix(
                    self.host_icon(host),
                    crate::config::get().colors.ui.title_fg,
                )
            })
            .collect();
        let mut picker = Picker::new("Default host for new sessions", items);
        if let Some(idx) = hosts.iter().position(|(h, _)| h == &current) {
            picker.cursor = idx;
        }
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::DefaultHost,
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

    /// The same emoji picker, opened from the **hosts panel**'s Icon field.
    /// Per-host icons are configured exactly like the workdir ones (§9), so they
    /// share the picker rather than growing a second, near-identical one; the
    /// only difference is where the result lands and which mode we return to.
    pub(super) fn open_emoji_picker_for_host(&mut self) {
        if self.host_edit.is_none() {
            return;
        }
        let picker =
            Picker::new("Emoji", emoji_picker_items()).with_placeholder("Search emoji by name…");
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::HostEmoji,
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

    /// The hosts-panel counterpart of [`App::apply_emoji_pick`]: drop the glyph
    /// into the selected host row's icon field and return to the panel.
    pub(super) fn apply_host_emoji_pick(&mut self, emoji: &str) {
        if let Some(state) = self.host_edit.as_mut() {
            let cursor = state.cursor;
            if let Some(r) = state.rows.get_mut(cursor) {
                r.icon.set_text(emoji);
            }
            // The picker is only reachable from inside the row editor (from the
            // Icon field, or from the list via the key that opens the editor
            // *on* it), so there is always an edit to hand the glyph back to.
            if let Some(edit) = state.edit.as_mut() {
                edit.focus = HostField::Icon;
            }
        }
        self.input_mode = InputMode::HostEdit;
    }

    /// Seed this machine's recent-dir list from its backend (which collapses
    /// each entry to the host-canonical `~` form on the way out, so a list
    /// written by an older build is migrated on read). Only meaningful for a
    /// direct-local backend — under pooled-localhost the list lives behind the
    /// daemon and is fetched through the per-host cache like any other host's.
    pub(super) fn load_recent_cwds(&mut self) {
        if matches!(self.backends.first(), Some(Backend::Local(_))) {
            self.recent_cwds = self.backends[0].recent_dirs();
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
        // Keyed on the repo root, so a worktree row wears its project's mark
        // (and its *default* emoji/colour, which are seeded from the path — an
        // unmarked repo would otherwise change appearance per worktree).
        let key = dir_mark_key(cwd);
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
        // The editor edits the *project's* mark, so a worktree row resolves to
        // its repo root here too — otherwise `Space i` would write under a key
        // `effective_dir_mark` never reads back.
        let cwd = dir_mark_key(&s.cwd).to_string();
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

    /// Build the backend set: `backends[0]` is **this machine**, then one
    /// `RemoteBackend` per configured host. Returns the backends plus the
    /// per-host colors and icons.
    ///
    /// `backends[0]` is normally the in-process [`Backend::local`]. Under
    /// **pooled-localhost** (`[launcher] pooled = true`) it is instead a
    /// `RemoteBackend` over a `LocalSocket` to this host's own daemon, which
    /// **replaces** the local backend rather than joining it — both would read
    /// the same `sessions/` dir and `collect_sessions` doesn't dedup, so every
    /// row would appear twice. That one substitution is what closes the
    /// on-server-zellij attach gap (§10.1): every session then starts in the
    /// pool, so a zellij pane and a laptop dashboard are both just attach
    /// clients, and a session survives the zellij server and the seat logout.
    ///
    /// Without the `remote` feature ([`REMOTE_ENABLED`]) this stops at the local
    /// backend: `hosts.json` is never read, so no remote connection task is ever
    /// spawned and every row is local. Pooled-localhost is deliberately *not*
    /// gated by it — it uses no ssh and is opt-in by its own config flag.
    fn build_backends_from_config(suspended: &HashSet<HostId>) -> HostSetup {
        let mut host_icons: HashMap<HostId, String> = HashMap::new();
        let mut backends = vec![Self::this_machine_backend()];
        if !REMOTE_ENABLED {
            return HostSetup {
                backends,
                host_icons,
            };
        }
        for h in hosts::load_hosts() {
            let host = HostId(h.label.clone());
            // "local" is reserved for the in-process backend; a host that aliases
            // it would have its sessions misclassified as local everywhere the
            // `(host, pid)` keying relies on `is_local()`.
            if host.is_local() {
                continue;
            }
            if let Some(icon) = h.icon.filter(|i| !i.trim().is_empty()) {
                host_icons.insert(host.clone(), icon);
            }
            // A suspended host keeps its row in the panel (and its icon above) but
            // gets no backend at all: no connection task, no ssh, no rows, and
            // nothing in the header tally to explain. `c` in the panel brings it
            // back — which is the whole point of not making the user delete it.
            if h.disabled {
                continue;
            }
            // Held down for an upgrade. Same treatment as `disabled` — the row
            // stays, reads "not connected", and gets no connection task — but
            // from memory rather than from the config file, so a dashboard that
            // dies here leaves nothing behind to un-suspend.
            if suspended.contains(&host) {
                continue;
            }
            if let Some(sock) = h.socket {
                // A configured socket path is a daemon on *this* machine (a
                // manual forward, or a test rig) — see `Transport::LocalSocket`.
                let t = Transport::LocalSocket(std::path::PathBuf::from(sock));
                backends.push(Backend::Remote(RemoteBackend::connect(t, host)));
            } else if let Some(target) = h.ssh {
                // One short, OS-limit-safe local socket per host; ssh forwards
                // the remote server's socket onto it.
                let local_sock = crate::state::remote_forward_sock(&host.0);
                // Verbatim: the transport splits the forwards out of them (they
                // can ride only one of its calls), and everything else reaches
                // every ssh this host takes.
                let t = Transport::Ssh {
                    target,
                    local_sock,
                    options: h.options,
                    clipboard: h.clipboard,
                };
                backends.push(Backend::Remote(RemoteBackend::connect(t, host)));
            }
        }
        HostSetup {
            backends,
            host_icons,
        }
    }

    /// Whether any host that will actually be dialled wants the clipboard.
    ///
    /// A `socket` host counts: pooled-localhost needs no forward (the socket is
    /// already on that machine), but it does need the server *running* for the
    /// shim to find. A suspended host does not — nothing is connected to it, so
    /// nothing can ask.
    fn any_host_wants_clipboard(hosts: &[hosts::HostConfig]) -> bool {
        REMOTE_ENABLED && hosts.iter().any(|h| h.clipboard && !h.disabled)
    }

    /// Re-read the host list and bring the clipboard server in line with it.
    /// Called at startup and on every host-list change.
    pub(super) fn refresh_clipboard_server(&mut self) {
        self.clipboard_server
            .set_wanted(Self::any_host_wants_clipboard(&hosts::load_hosts()));
    }

    /// The backend for the machine the dashboard runs on: in-process by default,
    /// or a client of this host's own daemon under pooled-localhost. See
    /// [`App::build_backends_from_config`] for why the two never coexist.
    ///
    /// The pooled arm bootstraps the daemon first (`daemon ensure`, idempotent
    /// and self-daemonizing) so the very first connect finds it listening; if
    /// the server binary isn't installed we fall back to the direct-local
    /// backend rather than leaving the user with an empty dashboard.
    fn this_machine_backend() -> Backend {
        if !crate::config::get().launcher.pooled {
            return Backend::local();
        }
        if let Err(e) = ensure_local_daemon() {
            tracing::warn!("pooled mode requested but the local daemon is unavailable: {e}");
            return Backend::local();
        }
        // A hostname-based id, since "local" is reserved for the in-process
        // backend and `is_local()` gates behaviour that no longer applies (these
        // sessions *are* pooled, so they detach, reattach, and can be stolen).
        let host = HostId(local_host_label());
        Backend::Remote(RemoteBackend::connect(
            Transport::LocalSocket(state::server_sock_path()),
            host,
        ))
    }

    /// Tear down the backends and rebuild from the current `hosts.json`
    /// (dropping a `Backend::Remote` ends its connection task), then re-subscribe
    /// to each one's change signal. Called after the hosts panel mutates.
    fn rebuild_remote_backends(&mut self) {
        // Before the rebuild, so the server is already listening when the first
        // reconnect asks ssh to forward onto its socket. Order is not actually
        // load-bearing — ssh binds the remote end whether or not the local socket
        // exists yet, and a request arriving in a gap costs one delegated paste —
        // but there is no reason to leave the gap open.
        self.refresh_clipboard_server();
        let HostSetup {
            mut backends,
            host_icons,
        } = Self::build_backends_from_config(&self.upgrading);
        self.backend_events = backends.iter_mut().map(Backend::subscribe).collect();
        self.backends = backends;
        self.host_icons = host_icons;
        // Stale per-host caches would otherwise outlive the host they describe.
        self.recent_dirs_cache.clear();
        self.reconnect_epochs.clear();
        // The rows themselves arrive on the next reload, which re-anchors then.
        self.mark_dirty(Cursor::HoldIndex);
    }

    pub(super) fn open_host_edit(&mut self) {
        let rows = hosts::load_hosts()
            .into_iter()
            .map(|h| HostRow {
                is_socket: h.socket.is_some(),
                target: picker::TextInput::with_text(h.socket.or(h.ssh).unwrap_or_default()),
                icon: picker::TextInput::with_text(h.icon.unwrap_or_default()),
                disabled: h.disabled,
                clipboard: h.clipboard,
                // Round-trips exactly: a spec can hold neither a comma nor a
                // space, so this join is the inverse of `parse_list`'s split.
                options: picker::TextInput::with_text(h.options.join(" ")),
                label: picker::TextInput::with_text(h.label),
            })
            .collect::<Vec<_>>();
        self.host_edit = Some(HostEditState {
            cursor: 0,
            edit: None,
            pending_remove: None,
            pending_upgrade: None,
            log_view: None,
            rows,
        });
        self.input_mode = InputMode::HostEdit;
    }

    /// One host's connection log, flattened to one item per **physical** line.
    ///
    /// Flattened here rather than at render time because the scroll offset and
    /// the renderer have to count in the same unit, and entries are not
    /// uniformly one line: the text this view exists to show — a host's refusal,
    /// quoted whole — is routinely a paragraph. The age rides only the first
    /// line of an entry, so a wrapped block reads as one event.
    pub(super) fn host_log_lines(&self, host: &HostId) -> Vec<HostLogLine> {
        let Some(backend) = self.backend_for(host) else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut out = Vec::new();
        for entry in backend.conn_log() {
            let age = now.saturating_duration_since(entry.at).as_secs();
            for (i, text) in entry.text.lines().enumerate() {
                out.push(HostLogLine {
                    age: (i == 0).then(|| format::format_log_age(age)),
                    error: entry.error,
                    text: text.to_string(),
                });
            }
        }
        out
    }

    /// Persist the panel's host rows **without closing the panel** (§9), and
    /// reconnect only if the *connections* actually changed.
    ///
    /// The old flow staged every edit behind a separate `s` Save, which was one
    /// more thing to forget — and, worse, meant a freshly added host showed no
    /// connection state until you'd saved and reopened. Now every mutation
    /// persists as it happens (add, edit-commit, confirmed remove), so the
    /// panel's live conn-state column animates the new host connecting in place.
    ///
    /// The catch that made that unpleasant: committing a row rebuilt *every*
    /// backend, so opening the panel, changing an emoji, and pressing Enter
    /// dropped every connection task and re-dialled every host — a multi-second
    /// storm for a cosmetic edit, and one that reset the auto-reattach epochs
    /// besides. So the rebuild is gated on [`Self::conn_identities`]: what a
    /// backend is *built from* is the label and the target, and nothing else in
    /// this panel touches those. An icon-only edit just re-reads the icon map.
    pub(super) fn apply_host_edits(&mut self) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        let configs: Vec<hosts::HostConfig> = state
            .rows
            .iter()
            // Drop blank rows (a half-typed one being added) and any that alias
            // the reserved `local` host.
            .filter(|r| {
                !r.label.text().trim().is_empty()
                    && !r.target.text().trim().is_empty()
                    && !r.label.text().trim().eq_ignore_ascii_case("local")
            })
            .map(|r| {
                let target = r.target.text().trim().to_string();
                let icon = r.icon.text().trim().to_string();
                hosts::HostConfig {
                    icon: (!icon.is_empty()).then_some(icon),
                    label: r.label.text().trim().to_string(),
                    socket: r.is_socket.then(|| target.clone()),
                    ssh: (!r.is_socket).then_some(target),
                    disabled: r.disabled,
                    clipboard: r.clipboard,
                    options: hosts::split_options(r.options.text()),
                }
            })
            .collect();
        let before = Self::conn_identities(&hosts::load_hosts());
        hosts::save_hosts(&configs);
        if before == Self::conn_identities(&configs) {
            // An icon-only edit: rendering changed, the order did not.
            self.refresh_host_icons(&configs);
            self.mark_dirty(Cursor::HoldIndex);
            return;
        }
        // A host that just left the ssh set — deleted, suspended, renamed, or
        // switched to a socket — still holds its port forwards on the shared
        // ControlMaster, which outlives the backend about to be dropped (and
        // which an open attach window keeps alive indefinitely). Retire them
        // before the rebuild, while there is still a record of what they were.
        crate::backend::retire_unlisted_forwards(&Self::forward_keys(&configs));
        self.rebuild_remote_backends();
    }

    /// `(label, ssh target)` for every host that will still get an ssh backend —
    /// the live set [`crate::backend::retire_unlisted_forwards`] measures
    /// against. A suspended or socket host is *absent*, not present-and-empty:
    /// it dials nothing, so nothing should be forwarding on its behalf.
    fn forward_keys(hosts: &[hosts::HostConfig]) -> Vec<(String, String)> {
        hosts
            .iter()
            .filter(|h| !h.disabled && h.socket.is_none())
            .filter_map(|h| Some((h.label.clone(), h.ssh.clone()?)))
            .collect()
    }

    /// What each configured host's backend is *built from*, in order: the label
    /// (which becomes its `HostId`), the transport it dials, and whether it is
    /// dialled at all. Two host lists with equal identities produce
    /// byte-identical backends, so a rebuild between them would only churn live
    /// connections — see [`Self::apply_host_edits`]. Pure, so the gate is
    /// unit-testable.
    pub(in crate::app) fn conn_identities(hosts: &[hosts::HostConfig]) -> Vec<ConnIdentity> {
        hosts
            .iter()
            .map(|h| ConnIdentity {
                label: h.label.clone(),
                ssh: h.ssh.clone(),
                socket: h.socket.clone(),
                disabled: h.disabled,
                options: h.options.clone(),
                clipboard: h.clipboard,
            })
            .collect()
    }

    /// Re-derive just the per-host emoji, leaving every connection task alone.
    /// The cheap half of [`Self::rebuild_remote_backends`]. Takes the configs the
    /// caller already holds rather than re-reading the file it just wrote.
    fn refresh_host_icons(&mut self, hosts: &[hosts::HostConfig]) {
        self.host_icons = hosts
            .iter()
            .filter_map(|h| {
                let icon = h.icon.as_ref().filter(|i| !i.trim().is_empty())?;
                let host = HostId(h.label.clone());
                (!host.is_local()).then_some((host, icon.clone()))
            })
            .collect();
    }

    /// Close the hosts panel. Edits are already persisted (see
    /// [`App::apply_host_edits`]), so this is just "I'm done looking".
    pub(super) fn close_host_edit(&mut self) {
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
                // Only direct-local sessions are worth snapshotting. A session
                // on another host — or a pooled one on this machine — outlives
                // the dashboard by construction: it keeps running in its pool
                // and simply reappears on reconnect, so "recovering" it would
                // mean resuming a session that never stopped. (Which is also
                // why crash recovery is inert under pooled-localhost, and
                // rightly so.)
                if !s.host.is_local() {
                    return None;
                }
                let window_id = self.window_id_for_session(s)?;
                let session_id = self.index_of(s).live_session_id(s)?.to_string();
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

    /// Title stamped on a `(host, cwd)` work tab: the cwd's basename, prefixed
    /// with the host's icon in brackets (`[🖥️] proj`) for every host but this
    /// machine. The prefix is the only way the host reaches the tab bar — a work
    /// tab is spawned with an explicit tab title, and on both backends an
    /// explicit title permanently overrides the follow-the-active-window's-title
    /// default, so the `[hostname]` an ssh login shell emits over OSC 0/2
    /// updates the *window* title and can never reach the *tab* label. It is the
    /// emoji rather than the label for the same reason the table's host column
    /// is ([`App::host_icon`]): a tab bar has a handful of cells per tab, and
    /// one glyph the eye already associates with the host beats six characters
    /// of `box:` eaten out of the basename. The brackets are what keep it
    /// reading as a prefix — an emoji and a word separated by one space run
    /// together at tab-bar size.
    ///
    /// Keeping the title static rather than letting ssh own it is deliberate:
    /// [`App::live_work_tab`] validates a recorded tab by requiring this exact
    /// title, which is one of the three checks that defeat zellij's recycled tab
    /// ids. Hence it takes the work-tab map key's two halves and nothing else —
    /// the spawn and the validation can't derive different answers. Changing a
    /// host's icon in the hosts panel *does* invalidate its open work tabs: the
    /// recorded ones fail the title check, are pruned, and the next `w` spawns a
    /// fresh tab wearing the new icon — the same self-healing path a pre-icon
    /// `work-tabs.json` entry takes.
    ///
    /// A pooled-localhost host is iconed too (its `HostId` is the machine's
    /// hostname, not `local`): its `w` opens an in-process shell, but such a
    /// dashboard is federating other hosts as well, so marking every host
    /// uniformly beats leaving exactly one of them unmarked.
    ///
    /// A worktree cwd is titled `<repo>@<worktree>`: the map stays keyed on the
    /// real cwd, so each worktree gets its own shell rather than sharing the
    /// checkout's — they are different branches, and a test run in the wrong one
    /// is worse than an extra tab — and the title is what keeps the tab bar
    /// readable once two of them are open.
    pub(super) fn work_tab_title(&self, host: &HostId, cwd: &str) -> String {
        let base = display_basename(cwd);
        if host.is_local() {
            base.into_owned()
        } else {
            format!("[{}] {base}", self.host_icon(host))
        }
    }

    /// The recorded work tab for `(host, cwd)`, validated against the live tab
    /// tree in `tabs`. The tab must still exist, still carry the title the spawn
    /// stamped on it ([`App::work_tab_title`]), and — when a window id was
    /// recorded — still contain that window: zellij recycles a closed highest
    /// tab's id (its tab counter is max-plus-one over live tabs), so an id +
    /// title match alone
    /// could send `w` into an unrelated tab that inherited the number and was
    /// renamed to the same basename. zellij pane ids never recycle, so the
    /// window-in-tab check pins the identity. An entry with no window id (seeded
    /// from a pre-window-id `work-tabs.json`) falls back to the id + title check.
    /// A failed check prunes the entry and returns `None`, so the caller falls
    /// through to spawning a fresh work tab.
    pub(super) fn live_work_tab(&mut self, key: &(HostId, String), tabs: &[Tab]) -> Option<TabId> {
        let expected = self.work_tab_title(&key.0, &key.1);
        let work_tab = self.work_tabs.get(key)?;
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

        let fresh = self.collect_sessions();
        // Keep the pre-reload rows so a departed one (its state file vanished —
        // crash / SIGKILL, not a clean kill) can have its held pane reaped: on a
        // floating-sessions backend the exited pane is an invisible leak. A no-op
        // on kitty (`reap_departed_windows` returns nothing there).
        let prev_sessions = std::mem::replace(&mut self.sessions, fresh);
        // Invalidate **after** the swap, never before. `mark_dirty` doesn't just
        // bump the version any more — every cursor policy *reads* the visible
        // order, so calling it while `self.sessions` still held the old rows
        // recomputed the pre-reload index list and re-cached it under the
        // post-reload version. Nothing then recomputed it (a plain reload with no
        // status transition bumps nothing else), so the frame below indexed the
        // new, shorter `sessions` with old indices and panicked.
        //
        // The one site that re-anchors by hand rather than through a `Cursor`:
        // the restore at the end of this function has priority rules a cursor
        // can't express — a just-spawned session claims the selection over the
        // prior one (`pending_focus_window`).
        self.mark_dirty(Cursor::HoldIndex);
        let reaped = self.reap_departed_windows(&prev_sessions);
        self.reap_window_queue.extend(reaped);
        self.session_indexes = self.refresh_session_indexes();
        // Adopt the host-owned flags for rows whose host serves them, so every
        // dashboard watching that host agrees about pins and bells (§9). Done
        // before the follow-up transitions below, which read `flags_of`.
        self.adopt_host_flags();
        // Forget attach expectations for sessions their host no longer reports,
        // then queue reattaches for any host that just came back (§7).
        let live_bindings: HashSet<bindings::BindingKey> = self
            .sessions
            .iter()
            .filter_map(|s| {
                Some(bindings::BindingKey {
                    host: s.host.clone(),
                    token: s.binding_token()?.to_string(),
                })
            })
            .collect();
        self.window_bindings.retain_expected(&live_bindings);
        self.sweep_reconnected_hosts();
        // Auto-mark follow_up on Active→Idle and Compacting→Compacted
        // transitions, and clear it when a session goes back to Active — the
        // user has re-engaged, so any stale attention flag is obsolete.
        let mut overrides_changed = false;
        let transitions = self.follow_up_transitions(&prev_status, &self.sessions);
        for (key, want) in transitions {
            self.update_flags(key, Cursor::HoldIndex, |f| f.follow_up = want);
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
                            .index_of(s)
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
            // A pooled session with no window on this screen sinks to the very
            // bottom — it's running somewhere else, so it shouldn't compete for
            // the eye with what's in front of you (§9).
            //
            // Detachment is the **first** key after an explicit pin: no status
            // lifts a row out of the tier, not even a live approval or decision
            // prompt. Those are urgent, but they are urgent *elsewhere* — the
            // prompt can't be answered until you attach, so seating it above
            // the sessions actually on this screen buries the work you can do
            // now. `follow_up` is the same argument twice over, since it is
            // auto-armed on every Active→Idle, so a detached session that
            // merely finished a turn would otherwise homestead the attention
            // block. The one thing that still wins is `pinned`: that's a
            // deliberate per-row "keep this in front of me", and honouring it
            // is the whole point of the key.
            let detached = self.is_detached_row(s);
            // Ranks 1–3 cover what `is_attention_row` unions (a needs-attention
            // or at-rest follow-up row); kept split here because ordering needs
            // the finer tiers, and with the detached tier taking precedence over
            // all three an attention row that is also detached lands in 6.
            // `jump_to_next_attention` skips that same row for the same reason
            // it sinks here — the prompt can't be answered until you attach —
            // so the sort and the jump target stay in agreement. Changing the
            // predicate on either side means revisiting the other.
            let rank: u8 = if flags.pinned {
                0
            } else if detached {
                6
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

    /// Live sessions across every backend, each tagged with its host so per-row
    /// keying can tell a remote pid from a local one. `backends[0]` (local)
    /// comes first, so a recycled-pid collision resolves in favour of local.
    fn collect_sessions(&self) -> Vec<LauncherState> {
        let mut out = Vec::new();
        for backend in &self.backends {
            let host = backend.host_id();
            for mut s in backend.list_sessions() {
                s.host = host.clone();
                // Drop rows this dashboard could only stare at — see
                // `is_actionable_row`. Filtered here, at the single point every
                // row enters the dashboard, so nothing downstream has to know.
                if self.is_actionable_row(&s) {
                    out.push(s);
                }
            }
        }
        out
    }

    /// Refresh each backend's session-name index, keyed **by host**.
    ///
    /// Deliberately not merged (§3): the shards map bare pids to names and
    /// session ids, so unioning them made a remote pid that happened to match a
    /// local one hand the local row the remote's identity — which then flowed
    /// straight into restart, fork, and crash recovery. Keeping them separate
    /// makes the collision unrepresentable rather than merely unlikely.
    fn refresh_session_indexes(&mut self) -> HashMap<HostId, SessionIndex> {
        self.backends
            .iter_mut()
            .map(|b| (b.host_id(), b.session_index()))
            .collect()
    }

    /// The session-name index for `host` — the only correct way to read one,
    /// since an index is meaningful solely against its own host's pids. An
    /// unknown host yields a shared empty index, which degrades to "no cached
    /// name", never to another host's.
    pub(super) fn index_for(&self, host: &HostId) -> &SessionIndex {
        static EMPTY: OnceLock<SessionIndex> = OnceLock::new();
        self.session_indexes
            .get(host)
            .unwrap_or_else(|| EMPTY.get_or_init(SessionIndex::default))
    }

    /// The index for a session's own host — the common case, spelled once.
    pub(super) fn index_of(&self, s: &LauncherState) -> &SessionIndex {
        self.index_for(&s.host)
    }

    /// The backend that owns `host`, or `None` when no such host is configured.
    ///
    /// Deliberately not a fallback to `backends[0]` (§9's one correctness-grade
    /// leak): a row carrying a `HostId` for a host that has since been removed
    /// would silently target *localhost* instead — a kill or an open aimed at
    /// the wrong machine. Callers surface the miss; there is no safe guess.
    pub(super) fn backend_for(&self, host: &HostId) -> Option<&Backend> {
        self.backends.iter().find(|b| &b.host_id() == host)
    }

    /// Remote hosts bucketed by how usable each one is right now — the header's
    /// whole connection surface. A disconnected host clears its mirror (no
    /// rows), so this is the only place its state shows outside the hosts panel,
    /// and per-host detail (including a `Failed` reason) deliberately stays one
    /// `Space h` away so the header is glanceable at any host count (§9).
    ///
    /// Every remote host lands in exactly one bucket, so an all-zero tally means
    /// "no remote hosts at all" — which is what the header hides its whole host
    /// cluster (default host included) on.
    pub(super) fn remote_host_tally(&self) -> HostTally {
        let mut tally = HostTally::default();
        // backends[0] is this machine; it is never "unreachable".
        for backend in self.backends.iter().skip(1) {
            match backend.conn_state() {
                ConnState::Connected => tally.good += 1,
                // Reachable but unusable, with a diagnosis behind it.
                ConnState::Failed(_) => tally.error += 1,
                // Link dropped — the reconnect loop is on it.
                ConnState::Disconnected => tally.down += 1,
                // Not up *yet*: nothing has gone wrong, so this is the one
                // bucket the header animates rather than counts.
                ConnState::Connecting => tally.connecting += 1,
            }
        }
        tally
    }

    /// Remote hosts whose first snapshot is still in flight, in `backends`
    /// order. A dialing host mirrors no rows yet, so the session table looks
    /// complete while sessions are still on their way — this is what the
    /// table's trailing "loading" line (`draw::connecting_row_label`) and the
    /// header's blinking cloud both hang off.
    pub(super) fn connecting_hosts(&self) -> Vec<HostId> {
        self.backends
            .iter()
            .skip(1)
            .filter(|b| matches!(b.conn_state(), ConnState::Connecting))
            .map(|b| b.host_id())
            .collect()
    }

    /// Every configured host paired with its live connection state — the hosts
    /// panel's rows, in `backends` order (this machine first).
    pub(super) fn host_states(&self) -> Vec<(HostId, ConnState)> {
        self.backends
            .iter()
            .map(|b| (b.host_id(), b.conn_state()))
            .collect()
    }

    /// Sessions currently on `host`, split into `(running, attached)` — the
    /// counts the hosts panel shows. `attached` counts rows this dashboard holds
    /// a window for, which is the number that answers "what am I actually
    /// looking at on that box".
    pub(super) fn host_session_counts(&self, host: &HostId) -> (usize, usize) {
        let rows: Vec<&LauncherState> = self.sessions.iter().filter(|s| &s.host == host).collect();
        let attached = rows
            .iter()
            .filter(|s| self.window_id_for_session(s).is_some())
            .count();
        (rows.len(), attached)
    }

    /// Record the local window the dashboard just opened for a session, keyed by
    /// its binding token (a remote `pool_session` or a local `launch_id`, §15.2),
    /// so the dashboard can resolve and prune it. Used by both the remote attach
    /// path and the local spawn path.
    pub(super) fn record_window_binding(&mut self, host: HostId, token: String, window: WindowId) {
        self.window_bindings.record(host, token, window);
        // Bindings feed `is_detached_row`, which is a *sort key*: binding a
        // window lifts the row out of the detached tier. This is the
        // `Enter`-on-a-detached-row path, so the row that just climbed the list
        // is the very one the user is acting on — and a background auto-reattach
        // must not drag the cursor off whatever they were looking at either.
        self.mark_dirty(Cursor::FollowSession);
    }

    /// Retire the binding for `(host, token)` — the explicit `D` detach, where
    /// the user closes the attach window but leaves the pooled session running.
    /// The row stays; it just sinks into the detached tier, so this goes through
    /// the same invalidate-and-follow dance as every other binding change rather
    /// than poking `window_bindings` directly.
    pub(super) fn retire_window_binding(&mut self, host: &HostId, token: &str) {
        self.window_bindings.remove(host, token);
        self.mark_dirty(Cursor::FollowSession);
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
        let dropped = self.window_bindings.prune_dead(live);
        if !dropped.is_empty() {
            // The dropped rows just became detached, and detachment is a sort
            // key — without invalidating the visible cache they'd draw the
            // unplugged icon (computed live) while staying put in the old order
            // until some unrelated reload happened to bump the version. Nothing
            // reloads when an attach window closes, so "until" could be minutes.
            // The re-sort then sinks them to the detached tier, so the cursor
            // stays on the session it was on, whether or not it is one of them.
            self.mark_dirty(Cursor::FollowSession);
        }
        dropped
    }

    /// Retire the bindings named by a batch of detach reports — the attach
    /// windows that just told us their session ended (§5). Spent windows are
    /// queued for closing; see [`attach_window_is_spent`].
    ///
    /// This is the event that replaces polling for the common case. It is
    /// deliberately allowed to be *wrong in one direction only*: a report can
    /// arrive for a binding we no longer hold (the snapshot prune beat it, or the
    /// user pressed `D`), which is a no-op; it can never invent a window, because
    /// only a live attach can send one. The snapshot prune stays as the backstop
    /// for the reverse case — a report that never came because the terminal was
    /// killed outright.
    ///
    /// Returns whether anything changed, so the caller can redraw. The
    /// expected-attached memory survives (see `prune_token`), so a host coming
    /// back still restores the window.
    ///
    /// `origin` gates the one destructive thing this does — ending the session
    /// behind a window the user closed (see [`closed_by_the_user`]). A
    /// dashboard-initiated close must therefore **retire the binding before
    /// closing the window** (`D` does). A close-without-kill path that skipped
    /// the retire would read as the user's.
    ///
    /// `x` and restart don't retire, and don't need to: they close the window
    /// *because* they are ending that session, so the queued close asks for
    /// exactly what is already happening. It is not merely harmless but the
    /// better of the two orderings — since the kill now goes out optimistically
    /// (`run::start_kill`), the one case where it lands on something still alive
    /// is a host that never took the first request, and there a second attempt a
    /// second later is a free retry rather than a duplicate.
    pub(super) fn apply_detach_reports(
        &mut self,
        reports: Vec<state::DetachReport>,
        origin: ReportOrigin,
    ) -> bool {
        let mut changed = false;
        for report in reports {
            let host = HostId(report.host.clone());
            let Some(retired) = self.window_bindings.prune_token(&host, &report.token) else {
                continue;
            };
            changed = true;
            // Closing the window is a request to end the session, unless the
            // user asked otherwise — but only when it was the *user* who closed
            // it, and only for a report we saw arrive (§`ReportOrigin`).
            if origin == ReportOrigin::Live
                && closed_by_the_user(report.status)
                && self.on_window_close == config::OnWindowClose::Close
                && let Some(key) = self.pooled_session_key(&host, &report.token)
            {
                self.pending_session_close.push(PendingClose {
                    host: host.clone(),
                    key,
                    due: Instant::now() + CLOSE_ON_WINDOW_CLOSE_DELAY,
                });
            }
            // The wrapper's own wall-clock measurement wins over the binding's
            // age, which is an `Instant` and so does not advance while the
            // machine is suspended: a laptop that slept through an eight-hour
            // attach would otherwise judge it by the minutes it was awake for.
            let held_for = report
                .held_secs
                .map_or(retired.held_for, Duration::from_secs);
            if attach_window_is_spent(held_for, report.status) {
                // The wrapper exits on a spent attach, so the window is closing
                // under its own steam and this is usually a no-op — as it is for
                // a window the user closed themselves. It still earns its place
                // for the attach that ran *unwrapped* (no resolvable reporter
                // exe, so this report came from somewhere else) and for any
                // backend that holds an exited command pane regardless: left
                // behind, the window is a corpse wearing a session's clothes —
                // `Enter` on the row opens a *second* window beside it, and on
                // zellij it sits invisible in the shared sessions tab inflating
                // every `list-panes`.
                self.reap_window_queue.push(retired.window);
            } else {
                let msg = self.refused_attach(&host, &report);
                self.set_status(msg, true);
            }
        }
        if changed {
            // Detachment is a sort key, and this path runs outside any reload.
            // A report arrives on its own schedule — the user is looking at the
            // list, not pressing anything — so a row sinking into the detached
            // tier must not take the cursor with it, nor hand it to whichever
            // row rises into the vacated index.
            self.mark_dirty(Cursor::FollowSession);
            self.write_window_bindings_file();
        }
        changed
    }

    /// Take the queued closes whose [`CLOSE_ON_WINDOW_CLOSE_DELAY`] has elapsed
    /// by `now`, leaving the rest queued. `now` is passed rather than read so
    /// the wait is testable without one.
    pub(super) fn take_due_session_closes(
        &mut self,
        now: Instant,
    ) -> Vec<(HostId, state::SessionKey)> {
        let mut due = Vec::new();
        self.pending_session_close.retain(|p| {
            if p.due <= now {
                due.push((p.host.clone(), p.key.clone()));
                false
            } else {
                true
            }
        });
        due
    }

    /// When the earliest queued close comes due, so the run loop's input wait
    /// doesn't sleep past it (the same clamp `settle_reload_at` gets). Without
    /// it an idle dashboard would fire the kill up to one `event_poll_ms` late.
    pub(super) fn next_session_close_due(&self) -> Option<Instant> {
        self.pending_session_close.iter().map(|p| p.due).min()
    }

    /// What to say about an attach that came back refused, and — for the two
    /// refusals captain-miao mints itself — the correction it is worth applying
    /// to the row it was about.
    ///
    /// An attach is the only operation that actually takes the pty's lock, so
    /// its answer is a transaction's, not an observation's: authoritative for
    /// the instant it happened, in a way no query about the same session can
    /// be. `ATTACH_EXIT_BUSY` therefore settles the attached bit and
    /// `ATTACH_EXIT_STALE` settles the row's existence, and both are spent here
    /// rather than left in a window the user is not looking at. The host says
    /// the same thing a round trip later — a refusal fires the pool's `on_busy`
    /// hook, a dead session its `Removed` — which is what ends the presumption.
    ///
    /// Every other status keeps the old text. The reason for those (ssh auth, a
    /// missing server, a shell that died on the way) exists only as the output
    /// held in that window, so pointing at it is the whole of what we know.
    fn refused_attach(&self, host: &HostId, report: &state::DetachReport) -> String {
        // The correction, where there is still a row and a backend to apply it
        // to. A refusal for a row that has since gone needs none — the message
        // is still worth saying, since it explains a window the user watched
        // open and close.
        let correct = |apply: fn(&Backend, &SessionKey)| {
            if let (Some(key), Some(backend)) = (
                self.pooled_session_key(host, &report.token),
                self.backend_for(host),
            ) {
                apply(backend, &key);
            }
        };
        match report.status {
            Some(state::ATTACH_EXIT_BUSY) => {
                correct(Backend::presume_attached);
                // Name the steal by its live binding, so a remap shows through.
                match self.keymap.primary_key(keymap::Command::StealAttach) {
                    Some(steal) => format!(
                        "{} is attached in another terminal — {steal} steals it",
                        report.token
                    ),
                    None => format!("{} is attached in another terminal", report.token),
                }
            }
            Some(state::ATTACH_EXIT_STALE) => {
                // Not a live session any more, so the row is the stale thing —
                // the same presumption `x` makes, reached by evidence rather
                // than by intent.
                correct(Backend::presume_killed);
                format!("{} is no longer a live session", report.token)
            }
            _ => format!("Attach to {} failed — see its window", report.token),
        }
    }

    /// The `SessionKey` of the pooled session `token` names on `host`, for the
    /// host to re-resolve to a pid at signal time (§the key is opaque here).
    /// `None` once the row is gone — a session that already ended needs no
    /// ending, which is what makes a report arriving after `x` or a restart a
    /// no-op rather than a signal at a recycled pid.
    fn pooled_session_key(&self, host: &HostId, token: &str) -> Option<state::SessionKey> {
        self.sessions
            .iter()
            .find(|s| &s.host == host && s.pool_session.as_deref() == Some(token))
            .map(|s| s.key())
    }

    /// The follow-up flag auto-mark / auto-clear transitions to apply after a
    /// reload — a pure function of the previous status map and the freshly
    /// collected sessions, returning `(key, want)` pairs the caller feeds to
    /// `update_flags`. Sibling of `newly_failed_windows`; extracted so the
    /// transition is unit-testable (`reload_sessions` itself is driven only
    /// through fs events). A session that just entered a rest state and isn't
    /// already flagged gets `true`; one back to Active that still carries the
    /// flag gets `false`.
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
                if (entered_rest || parked_server) && !flags.follow_up {
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
    /// open: buried in the shared `miao:sessions` tab, invisible (only the z-order
    /// top shows), unreachable except via zellij's floating-cycle keybinds, and
    /// counted in every `list-panes` (~20ms/pane). Resolve the window the dashboard
    /// bound to the departed row (through the still-lingering binding — a local
    /// `launch_id` binding has no other collector, `prune_dead` being remote-only),
    /// drop that stale binding, and return the window for the run loop to
    /// `close_window` best-effort.
    ///
    /// Gated to `floating_sessions` backends (D2): zellij is where a departed
    /// session's pane can linger *invisibly*. Elsewhere the window is a visible
    /// tab whose occupant now decides its own fate — sessions spawn `hold:
    /// false`, so a launcher or attach that returns takes the window with it,
    /// and one still on screen is one still running or deliberately held. A
    /// departed *remote* row is only
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
            // A departed row on a host that is merely *unreachable* is not
            // evidence the session died: a disconnect clears the mirror (every
            // row departs) while the pooled session and its local attach window
            // live on, and reconnect brings the row back. A host that's gone
            // from the configuration entirely can't bring anything back, so its
            // orphaned window is reaped.
            if self
                .backend_for(&s.host)
                .is_some_and(|b| !b.conn_state().is_connected())
            {
                continue;
            }
            // Only windows the dashboard itself created are reaped, and the
            // binding (a local `--launch-id` spawn or a pool attach) is that
            // proof: removing it yields the window and retires the stale entry
            // in one step (a local `launch_id` binding has no other collector —
            // `prune_dead` runs only for attachments). A token-less
            // hand-launched row resolves only through its self-reported window
            // id, which names the user's own pane, not dashboard terrain —
            // never closed.
            if let Some(token) = s.binding_token().map(str::to_string)
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
                // The token the row carries home. A token-less (hand-launched)
                // session has none; key the bell projection on its self-reported
                // window with an empty token (the bell only needs window → pid).
                let token = s.binding_token().unwrap_or_default().to_string();
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
    /// `miao claude`, or a launcher predating `launch_id` — self-reported
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
        if let Some(t) = s.binding_token() {
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
        // A detached pooled session: attach-then-focus *immediately*, so the
        // user watches the ssh progress in the window rather than staring at a
        // frozen dashboard (§9 — today's behavior, now the explicit contract).
        // A row with no pool session can't be here: `collect_sessions` filters
        // those out entirely (see `is_actionable_row`).
        // A local, unpooled session with no window yields `None` — nothing to
        // focus and nothing to attach to.
        let pool = s.pool_session.as_ref()?;
        // …unless another client holds the pty, in which case attaching is a
        // *steal* and has to be asked rather than done. Left to the attach
        // wrapper, this was a window that opened, printed libshpool's refusal,
        // and closed — the answer arriving in the one place the user isn't
        // looking. Asking here also makes `Enter` agree with what the row
        // already says: the confirm appears on exactly the rows drawn with the
        // held-elsewhere glyph, since both key on the same bit. An *unknown*
        // bit attaches as before and lets the wrapper refuse if it must —
        // guessing "held" from a pool we couldn't read would put a confirm in
        // front of every row.
        if s.attached == Some(true) {
            self.pending_confirm = Some(PendingConfirm {
                prompt: "Another terminal is attached — kick it? [y/N]".to_string(),
                action: Action::AttachRemoteRunning {
                    host: s.host.clone(),
                    pool_session: pool.clone(),
                    force: true,
                },
            });
            self.input_mode = InputMode::Confirm;
            return None;
        }
        Some(Action::AttachRemoteRunning {
            host: s.host.clone(),
            pool_session: pool.clone(),
            force: false,
        })
    }

    /// Whether a row is one this dashboard can act on, and therefore worth a
    /// slot in the list (§9).
    ///
    /// The case this excludes: a session on a remote host that isn't in that
    /// host's pty pool — one the *server's own* dashboard spawned into a zellij
    /// pane. The daemon's snapshot is every state file, pooled or not, so such
    /// rows do arrive; but there is no pool session to attach to, so `Enter`
    /// dead-ends. The review challenged hiding them (an attention state on a
    /// hidden row goes invisible remotely) and reaffirmed it: **the dashboard is
    /// for actionable sessions** — a row this dashboard can neither attach nor
    /// act on doesn't earn a slot, the hosts panel's session count keeps them
    /// countable, and the host's own dashboard remains their surface.
    ///
    /// (If this ever needs softening, the recorded refinement is: hide *unless*
    /// the row has an attention state.)
    fn is_actionable_row(&self, s: &LauncherState) -> bool {
        // Sessions on this machine are always actionable: an unpooled one *is*
        // its window, and a pooled one is reachable through the local pool.
        if s.host == self.backends[0].host_id() {
            return true;
        }
        s.pool_session.is_some()
    }

    /// A pooled session on another host that this dashboard currently holds no
    /// window for. It sorts into its own tier at the bottom of the list and
    /// carries its own icon, so "still running over there, just not on my
    /// screen" reads differently from "idle in front of me" (§9).
    pub(super) fn is_detached_row(&self, s: &LauncherState) -> bool {
        s.pool_session.is_some() && self.window_id_for_session(s).is_none()
    }

    /// Which kind of detached a row is: free to take, or already held by another
    /// client. `None` for a row with a window here.
    ///
    /// Presentation only — the *tier* stays one tier (a row is out of sight
    /// either way, and the sort has no business flapping on another client's
    /// comings and goings). The split matters because `Enter` doesn't behave the
    /// same: a free row attaches, a held one needs the steal.
    ///
    /// `attached` is the host's overlay of libshpool's live bit, so `None` there
    /// means *unknown* (the pool couldn't be read) and must not read as "someone
    /// has it" — an unreadable pool would otherwise put every row behind an
    /// implied steal. Unknown falls back to `Free`, matching how the steal
    /// confirm treats it: offer the ordinary action, let the attach itself
    /// refuse if it must.
    pub(super) fn detached_kind(&self, s: &LauncherState) -> Option<format::Detached> {
        if !self.is_detached_row(s) {
            return None;
        }
        Some(if s.attached == Some(true) {
            format::Detached::HeldElsewhere
        } else {
            format::Detached::Free
        })
    }

    /// Every detached pooled session that is free to take, as the
    /// `(host, pool_session)` pairs an attach needs. The manual half of the
    /// reconnect sweep's work list, and deliberately built from the same
    /// `detached_kind` the rows are drawn from: what the list attaches is
    /// exactly what the table marks as detached-and-free.
    ///
    /// Filtered on the whole session set rather than the visible one — a search
    /// filter narrows what you're *looking at*, not what "all" means (`Space E`
    /// restart-all reads the same way).
    pub(super) fn attach_all_targets(&self) -> Vec<(HostId, String)> {
        self.sessions
            .iter()
            .filter(|s| self.detached_kind(s) == Some(format::Detached::Free))
            .filter_map(|s| Some((s.host.clone(), s.pool_session.clone()?)))
            .collect()
    }

    /// Why this host's server cannot be upgraded right now, phrased for the
    /// panel — `None` when it can.
    ///
    /// Two refusals, and they are the same rule seen from two sides: an upgrade
    /// ends every session on the host and brings each one back as a window
    /// *here*. A session that isn't resting would lose work to that, and a
    /// session another terminal is attached to would be taken from whoever is
    /// using it rather than restored to them.
    ///
    /// "Resting" is the restart-all whitelist (`Idle | Compacted`), deliberately
    /// not [`SessionStatus::is_busy`] — `Starting`, `WaitingForApproval` and
    /// `ReviewPending` all read as at-rest by that narrower test, and all three
    /// are states you would hate to have silently restarted.
    pub(super) fn upgrade_blocker(&self, host: &HostId) -> Option<String> {
        let mine: Vec<&LauncherState> = self.sessions.iter().filter(|s| &s.host == host).collect();
        let busy = mine
            .iter()
            .filter(|s| !matches!(s.status, SessionStatus::Idle | SessionStatus::Compacted))
            .count();
        if busy > 0 {
            return Some(format!("{busy} {} not idle", plural_sessions(busy)));
        }
        let held = mine
            .iter()
            .filter(|s| self.detached_kind(s) == Some(format::Detached::HeldElsewhere))
            .count();
        if held > 0 {
            return Some(format!(
                "{held} {} attached in another terminal",
                plural_sessions(held)
            ));
        }
        None
    }

    /// Everything an upgrade of `host` will kill, in the form that brings it
    /// back. Snapshotted **before** the host is suspended — taking the backend
    /// down clears its mirror, and these rows are the only record of what was
    /// there.
    ///
    /// A session with no `session_id` yet is dropped rather than restored: there
    /// is nothing to resume it *to*. The gate already refuses such a host (a
    /// session that young is `Starting`, which is not idle), so this is a belt.
    pub(super) fn upgrade_restore_list(&self, host: &HostId) -> Vec<RestoreSpec> {
        self.sessions
            .iter()
            .filter(|s| &s.host == host)
            .filter_map(|s| {
                Some(RestoreSpec {
                    agent: s.agent,
                    cwd: s.cwd.clone(),
                    session_id: s.session_id.clone()?,
                })
            })
            .collect()
    }

    /// The upgrade offer for the row the hosts panel is sitting on, if any —
    /// what decides whether `u` is advertised, and the one place the panel's
    /// cursor is turned into a host.
    pub(super) fn selected_host_upgrade(&self) -> Option<crate::backend::UpgradeOffer> {
        let state = self.host_edit.as_ref()?;
        let host = state.rows.get(state.cursor)?.host();
        self.backend_for(&host)
            .and_then(crate::backend::Backend::upgrade_offer)
    }

    /// Hosts whose post-upgrade restore can run now: they owe sessions and they
    /// are connected again.
    ///
    /// Connected is a sufficient test here, with no need to inspect the mirror
    /// first. A restore list only survives a *successful* upgrade, and success
    /// means the daemon was stopped — so every pooled session this host had is
    /// gone, and nothing that could be resumed twice is left to race.
    pub(super) fn hosts_ready_to_restore(&self) -> Vec<HostId> {
        self.upgrade_restores
            .keys()
            .filter(|h| {
                self.backends
                    .iter()
                    .any(|b| &&b.host_id() == h && b.conn_state().is_connected())
            })
            .cloned()
            .collect()
    }

    /// Take `host` off the air for the duration of its upgrade: no backend, no
    /// connection task, no redial into the window where the daemon is stopped
    /// but the new binary is not yet published.
    pub(super) fn suspend_for_upgrade(&mut self, host: &HostId) {
        self.upgrading.insert(host.clone());
        self.rebuild_remote_backends();
    }

    /// Put `host` back on the air, whichever way its upgrade went. The fresh
    /// dial re-probes: on success it finds our digest at the cache path and no
    /// daemon, so it resolves to `UseCache` and starts one.
    pub(super) fn resume_after_upgrade(&mut self, host: &HostId) {
        self.upgrading.remove(host);
        self.rebuild_remote_backends();
    }

    /// `Space A`: attach a window to every free detached session at once.
    ///
    /// Held-elsewhere rows are skipped rather than stolen — a steal takes
    /// someone else's terminal away, which stays a per-session decision behind
    /// its own confirm (§10.2). Saying how many were skipped keeps that from
    /// reading as a silent partial job.
    ///
    /// No confirm of its own: this only opens windows for sessions already
    /// running, `D` puts any of them back, and it is the same thing the
    /// reconnect sweep does unprompted.
    pub(super) fn request_attach_all(&mut self) -> Option<Action> {
        let targets = self.attach_all_targets();
        if targets.is_empty() {
            let held = self
                .sessions
                .iter()
                .filter(|s| self.detached_kind(s) == Some(format::Detached::HeldElsewhere))
                .count();
            let msg = match (held, self.keymap.primary_key(keymap::Command::StealAttach)) {
                (0, _) => "Nothing to attach — every session already has a window here".to_string(),
                (n, Some(key)) => format!(
                    "{n} detached {} attached in another terminal — {key} steals one",
                    plural_sessions(n),
                ),
                (n, None) => format!(
                    "{n} detached {} attached in another terminal",
                    plural_sessions(n),
                ),
            };
            self.set_status(msg, false);
            return None;
        }
        Some(Action::AttachAll { targets })
    }

    /// What the preview panel says when it has no captured text.
    ///
    /// The preview is a `capture_text` of the row's **local** window, so a row
    /// without one has nothing to show — ever, not "yet". Saying `(loading…)`
    /// there is a lie that never resolves, and the old fallback for a
    /// window-less row said `(no session selected)`, which is wrong in the one
    /// case it fires most: a detached pooled session, where a row very much *is*
    /// selected. Each window-less case names its own reason instead, so the
    /// panel explains the emptiness rather than implying a stuck fetch.
    pub(super) fn preview_placeholder(&self) -> String {
        let Some(s) = self.selected_session_ref() else {
            return "(no session selected)".to_string();
        };
        if let Some(identity) = self.foreign_terminal(s) {
            return format!("(session lives in {identity} — no preview here)");
        }
        match self.detached_kind(s) {
            Some(format::Detached::Free) => {
                return "(detached — attach with Enter to preview)".to_string();
            }
            Some(format::Detached::HeldElsewhere) => {
                // Name the steal by its live binding, so a remap shows through.
                return match self.keymap.primary_key(keymap::Command::StealAttach) {
                    Some(key) => format!("(attached in another terminal — {key} to steal it)"),
                    None => "(attached in another terminal)".to_string(),
                };
            }
            None => {}
        }
        if self.window_id_for_session(s).is_none() {
            return "(no window to preview)".to_string();
        }
        // Last, so the more specific reasons above still win: the row *has* a
        // live local window, this backend simply cannot read one.
        if !self.capabilities.capture {
            return "(this terminal exposes no way to read a window — no preview)".to_string();
        }
        "(loading…)".to_string()
    }

    /// Queue an attach window for every session on a just-reconnected host that
    /// the dashboard expects to be attached to but isn't (§7).
    ///
    /// Fires on the `Disconnected → Connected` edge only, tracked by each
    /// backend's reconnect epoch, so a laptop-sleep or broken-pipe reconnect
    /// restores the whole working set in one go — while a session the user
    /// deliberately detached with `D` stays detached (the `D` cleared its
    /// expectation).
    pub(super) fn sweep_reconnected_hosts(&mut self) {
        let epochs: Vec<(HostId, u64)> = self
            .backends
            .iter()
            .filter_map(|b| match b {
                Backend::Remote(r) => Some((b.host_id(), r.reconnect_epoch())),
                Backend::Local(_) => None,
            })
            .collect();
        for (host, epoch) in epochs {
            let previous = self.reconnect_epochs.insert(host.clone(), epoch);
            let targets = self.reattach_targets(&host, previous, epoch);
            self.pending_reattach
                .extend(targets.into_iter().map(|t| (host.clone(), t)));
        }
    }

    /// The reattach work list for one host: empty unless its reconnect epoch
    /// actually advanced, then every session the dashboard expects to be
    /// attached to, holds no window for, and the host still reports as running.
    ///
    /// Split out from [`App::sweep_reconnected_hosts`] so the edge condition is
    /// unit-testable without a live connection task — `previous == None` is the
    /// host's *first* sighting (the initial connect, whose recovery is the
    /// binding re-seed's job), not a reconnect.
    pub(super) fn reattach_targets(
        &self,
        host: &HostId,
        previous: Option<u64>,
        epoch: u64,
    ) -> Vec<String> {
        if previous.is_none_or(|p| p == epoch) {
            return Vec::new();
        }
        let live: HashSet<&str> = self
            .sessions
            .iter()
            .filter(|s| &s.host == host)
            .filter_map(|s| s.pool_session.as_deref())
            .collect();
        self.window_bindings
            .expected_without_window(host)
            .into_iter()
            .filter(|t| live.contains(t.as_str()))
            .collect()
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

    /// Re-point the cursor at `key`'s row and re-decide what `Enter` should do
    /// on it — the retry half of the focus-failure path.
    ///
    /// Finding the row by **identity** rather than reusing the old index is the
    /// whole point: the retry runs after a prune, detachment is a sort key, so
    /// the row the user acted on has usually just sunk to the detached tier and
    /// its old index now belongs to some other session. Re-deciding by index
    /// would attach a window to whichever row slid into that slot.
    pub(super) fn refocus_key(&mut self, key: &FlagKey) -> Option<Action> {
        let idx = self
            .visible_sessions()
            .iter()
            .position(|s| matches_key(s, key))?;
        self.table_state.select(Some(idx));
        let s = self.selected_session()?;
        self.focus_or_attach(&s)
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
        // The row drops from the attention rank to the idle rank and slides
        // down; the cursor goes with it rather than staying at an index that
        // now names whichever row rose into it.
        self.update_flags(key, Cursor::FollowSession, |f| f.follow_up = false);
        self.save_overrides();
        // Persist the flag change into the restart snapshot too, so a crash
        // before the next reload doesn't restore the stale flag on recovery.
        self.save_session_snapshot();
    }

    /// The session sitting just after the selected row, or just before it when
    /// the selection is last. Read *before* a mutation, to name the row the
    /// cursor should advance to once the selected one re-sorts away
    /// ([`Cursor::Follow`]).
    fn neighbor_of_selected(&self) -> Option<FlagKey> {
        let idx = self.table_state.selected()?;
        let visible = self.visible_sessions();
        visible
            .get(idx + 1)
            .or_else(|| idx.checked_sub(1).and_then(|p| visible.get(p)))
            .map(|s| flag_key(s))
    }

    /// Re-anchor the table cursor on `key`'s row at its (possibly new) index,
    /// clamping when that session is no longer visible.
    ///
    /// The list is sorted, so any mutation that changes a **sort key** slides
    /// rows past a cursor that is only an index — the index survives, the
    /// session it names does not. Every caller that changes one and wants the
    /// user to keep pointing at the same session goes through here. Two do:
    /// clearing a follow-up bell drops a row from the attention rank to the
    /// idle rank, and binding or retiring a window flips `is_detached_row`,
    /// which sorts a row into (or out of) the detached tier at the bottom.
    pub(super) fn reselect(&mut self, key: &FlagKey) {
        match self
            .visible_sessions()
            .iter()
            .position(|s| matches_key(s, key))
        {
            Some(idx) => self.table_state.select(Some(idx)),
            None => self.clamp_selection(),
        }
    }

    pub(super) fn toggle_session_flag(&mut self, flag: SessionFlag) {
        let Some(key) = self.selected_key() else {
            return;
        };
        // `pid` (a copy) labels the status line; `key` keys flags.
        let pid = key.1;
        let was = self.flags_of(&key);
        // Every flag is a sort key, and this one operation wants different
        // cursor policies depending on which way it's going — which is exactly
        // why `mark_dirty` makes the choice explicit instead of assuming one.
        // Decided here, before the toggle, because the clearing arm needs the
        // pre-mutation order to name its target.
        let cursor = match flag {
            // Clearing needs-input drops the row out of the attention tier, so
            // don't ride it down — advance to the session that sat just after
            // it (or just before, when it sat at the end). Falls back to the row
            // itself when it has no neighbour.
            SessionFlag::FollowUp if was.follow_up => {
                Cursor::Follow(self.neighbor_of_selected().unwrap_or_else(|| key.clone()))
            }
            // Pin / marking needs-input: the row floats up and the cursor rides
            // with it, so the user stays on what they just flagged.
            _ => Cursor::FollowSession,
        };
        let now_on = match flag {
            SessionFlag::Pin => {
                let on = !was.pinned;
                let seq = if on {
                    self.next_pin_seq = self.next_pin_seq.wrapping_add(1);
                    self.next_pin_seq
                } else {
                    0
                };
                self.update_flags(key.clone(), cursor, move |f| {
                    f.pinned = on;
                    f.pin_seq = seq;
                });
                on
            }
            SessionFlag::FollowUp => {
                let on = !was.follow_up;
                self.update_flags(key.clone(), cursor, |f| {
                    f.follow_up = on;
                });
                on
            }
        };

        // If the row's host owns its flags, push the change there so every other
        // dashboard watching that host sees it too; otherwise it's ours to
        // persist locally. Either way the in-memory value above already applied,
        // so the UI responds immediately and a failed push just isn't shared.
        if !self.publish_flags(&key, self.flags_of(&key)) {
            self.save_overrides();
        }

        // No cursor work here: the `Cursor` handed to `update_flags` above
        // already placed it, which is the point of routing the policy through
        // `mark_dirty` rather than re-deriving a target after the fact.
        let label = match (flag, now_on) {
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
    ///
    /// **Detached rows are not targets.** `s` is a "take me to the work waiting
    /// on me" key, and the work on a row with no window here can't be done from
    /// here: the prompt is unanswerable until the row is attached, so landing
    /// the cursor on it costs a keypress and gives back nothing. It is the same
    /// argument that sinks the row to its own tier in `compute_visible_indices`
    /// — the two now agree. When *only* detached rows want attention the key
    /// says so rather than claiming nothing does, since the icons on screen say
    /// otherwise.
    pub(super) fn jump_to_next_attention(&mut self) {
        let visible = self.visible_sessions();
        let current = self.table_state.selected().unwrap_or(usize::MAX);
        let mut skipped_detached = false;
        let attention_indices: Vec<usize> = visible
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                if !self.is_attention_row(s) {
                    return false;
                }
                if self.is_detached_row(s) {
                    skipped_detached = true;
                    return false;
                }
                true
            })
            .map(|(i, _)| i)
            .collect();
        if attention_indices.is_empty() {
            self.set_status(
                if skipped_detached {
                    "Only detached sessions need attention".to_string()
                } else {
                    "No sessions need attention".to_string()
                },
                false,
            );
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
        // Restart is kill + reopen **on the row's own host** — the seam already
        // carries `resume: (session_id, fork)`, so the old local-only gate was
        // never a limitation of the design, just a gap in the plumbing (§9).
        // A remote restart lands in that host's pool and auto-attaches like any
        // open.
        if !matches!(s.status, SessionStatus::Idle | SessionStatus::Compacted) {
            return Err("Cannot restart: session must be idle (not active or waiting)");
        }
        let Some(session_id) = self.index_of(s).live_session_id(s).map(str::to_string) else {
            return Err("Cannot restart: session has no session id yet");
        };
        Ok(RestartSpec {
            agent: s.agent,
            host: s.host.clone(),
            key: s.key(),
            // `None` for a detached pooled session: there is no local window to
            // close, and the old pty is torn down by the kill on its own host.
            window_id: self.window_id_for_session(s),
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
            &format::session_display_name(&s, self.index_of(&s), &self.random_names),
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
        // Restart-all exists to get every agent onto a new binary at once, so a
        // partial run defeats the point: refuse while anything is busy.
        if self
            .sessions
            .iter()
            .any(|s| !matches!(s.status, SessionStatus::Idle | SessionStatus::Compacted))
        {
            self.set_status(
                "Cannot restart all: every session must be idle".to_string(),
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
        let saved_name = self
            .index_for(host)
            .by_session_id
            .get(&c.session_id)
            .cloned();
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
    pub(super) fn open_resume_picker(&mut self, host: HostId, candidates: Vec<ResumeCandidate>) {
        let items: Vec<PickerItem> = candidates
            .iter()
            .map(|c| self.resume_candidate_item(&host, c, None))
            .collect();

        let picker = Picker::new(resume_picker_title(&host), items)
            .with_placeholder("Search by title, path, or branch…")
            .with_size(80, 80);
        self.picker = Some(ActivePicker {
            picker,
            kind: PickerKind::Resume { host, candidates },
        });
        self.input_mode = InputMode::Picker;
        self.refresh_picker_footer();
    }

    /// Repopulate the open resume picker for a different host — the `Ctrl-h`
    /// switch, the exact analog of `Ctrl-t`'s agent switch in the workdir
    /// picker. Scoping to one host at a time is what replaced the cross-host
    /// union (§9), so the list's scope is always readable in the title.
    pub(super) fn reseed_resume_picker(&mut self, host: HostId, candidates: Vec<ResumeCandidate>) {
        let items: Vec<PickerItem> = candidates
            .iter()
            .map(|c| self.resume_candidate_item(&host, c, None))
            .collect();
        if let Some(active) = self.picker.as_mut() {
            active.picker.title = resume_picker_title(&host);
            active.picker.items = items;
            active.picker.set_text("");
            active.kind = PickerKind::Resume { host, candidates };
        }
        self.refresh_picker_footer();
    }

    /// Mark the open picker as still fetching its items (or done). Only affects
    /// the empty-list message and the footer's trailing note — the picker stays
    /// interactive throughout, which is the whole point of loading off the UI
    /// thread.
    pub(super) fn set_picker_loading(&mut self, loading: bool) {
        if let Some(active) = self.picker.as_mut() {
            active.picker.loading = loading;
        }
        self.refresh_picker_footer();
    }

    /// Rebuild the open picker's bottom status line from its kind. Called
    /// wherever the values it shows can change: opening, `Ctrl-t`, `Ctrl-h`, and
    /// the arrival of an async item list.
    ///
    /// Only the two pickers that carry per-launch settings get one — the rest
    /// have nothing to say that their title doesn't already.
    pub(super) fn refresh_picker_footer(&mut self) {
        let ui = &crate::config::get().colors.ui;
        let dim = Style::default().add_modifier(Modifier::DIM);
        let value = Style::default().fg(ui.title_fg).bold();
        let host_span = |app: &Self, host: &HostId| {
            Span::styled(format!("{} {}", app.host_icon(host), host.0), value)
        };
        let Some(active) = self.picker.as_ref() else {
            return;
        };
        let spans: Vec<Span<'static>> = match &active.kind {
            PickerKind::Workdir {
                agent,
                host,
                worktree,
            } => {
                let mut spans = vec![
                    Span::styled(" Agent ", dim),
                    Span::styled(agent.label().to_string(), value),
                ];
                // The host half only exists when there's more than one to be on.
                if self.backends.len() > 1 {
                    spans.push(Span::styled("   Host ", dim));
                    spans.push(host_span(self, host));
                }
                // Shown only when armed, and only where it's possible. An
                // agent without worktrees says nothing rather than showing a
                // permanent "off" for a thing it can't do.
                if let Some(arm) = worktree.as_ref().filter(|_| agent.supports_worktrees()) {
                    spans.push(Span::styled("   Worktree ", dim));
                    let name = arm.name.text();
                    if arm.naming {
                        // A block cursor, since the real one sits in the path
                        // input above and can't be in two places.
                        spans.push(Span::styled(name.to_string(), value));
                        spans.push(Span::styled("▏", value));
                        spans.push(Span::styled(" Enter done  Esc cancel", dim));
                    } else if name.trim().is_empty() {
                        // Named by the agent, so say that rather than showing an
                        // empty value that reads like a field we failed to fill.
                        spans.push(Span::styled("auto-named".to_string(), value));
                    } else {
                        spans.push(Span::styled(name.trim().to_string(), value));
                    }
                }
                spans
            }
            // No loading note here: the list area already says so, and while a
            // fetch is in flight the items are always empty, so the two would
            // only ever appear together.
            PickerKind::Resume { host, .. } => {
                vec![Span::styled(" Host ", dim), host_span(self, host)]
            }
            _ => Vec::new(),
        };
        if let Some(active) = self.picker.as_mut() {
            active.picker.footer = (!spans.is_empty()).then(|| Line::from(spans));
        }
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

    /// Whether the periodic preview auto-refresh should fire: the backend can
    /// capture at all, the dashboard has terminal focus (no `kitten @ get-text`
    /// churn while the user is
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
            && self.capabilities.capture
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
        // New sessions target the persisted default host (`Space H`); `Ctrl-h`
        // cycles per-launch, re-seeding the list from that machine.
        let host = self.default_host_or_local();
        let cwds = self.host_recent_dirs(&host);
        let items = self.workdir_items(&cwds, &host);

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
            // Worktrees start off on every launch. There is no persisted
            // default for it on purpose: `Space a`/`Space H` answer "what do I
            // usually use", while isolation answers "is *this* task one that
            // should not touch my checkout" — a question with a different
            // answer nearly every time.
            kind: PickerKind::Workdir {
                agent,
                host,
                worktree: None,
            },
        });
        self.input_mode = InputMode::Picker;
        self.refresh_picker_footer();
    }

    /// Build picker items for a list of cwds shown against `host`'s home. Only a
    /// *local* dir gets a custom directory-mark icon (marks are a local concept,
    /// keyed by local path); remote dirs render plain.
    /// Build picker items for a list of cwds. The strings are already in the
    /// host-canonical `~` form (§3) — the wire *is* the display form — so there
    /// is nothing to collapse and no host `$HOME` to know. Directory marks now
    /// key on that same form, which is why the same repo path on two machines
    /// shares its icon.
    fn workdir_items(&self, cwds: &[String], _host: &HostId) -> Vec<PickerItem> {
        cwds.iter()
            .map(|cwd| {
                let mut item = PickerItem::new(cwd.clone())
                    .with_filter_text(cwd.clone())
                    .with_payload(cwd.clone());
                if self.directory_marks.contains_key(cwd.trim_end_matches('/')) {
                    let (icon, color, _) = self.effective_dir_mark(cwd);
                    item = item.with_prefix(icon, color);
                }
                item
            })
            .collect()
    }

    /// The default host for new-session operations, falling back to localhost
    /// when the persisted one is no longer configured.
    pub(super) fn default_host_or_local(&self) -> HostId {
        if self.backend_for(&self.default_host).is_some() {
            self.default_host.clone()
        } else {
            self.backends[0].host_id()
        }
    }

    /// A host's recent dirs, **cache-first** (§9). A host switch must render
    /// instantly, so this never blocks: it returns the cached list (seeded on
    /// first use and refreshed in the background by `refresh_recent_dirs`) and
    /// only pays a round-trip when there is nothing cached at all and the host
    /// is connected. The governing rule is simply *never put an RTT between a
    /// keystroke and its echo*.
    pub(super) fn host_recent_dirs(&mut self, host: &HostId) -> Vec<String> {
        // A direct-local backend's list is held in memory and is authoritative:
        // it reflects in-picker deletes (`Ctrl-d`) that haven't been re-read.
        if self.is_direct_local(host) {
            return self.recent_cwds.clone();
        }
        if let Some(cached) = self.recent_dirs_cache.get(host) {
            return cached.clone();
        }
        let fetched = self.fetch_recent_dirs(host);
        self.recent_dirs_cache.insert(host.clone(), fetched.clone());
        fetched
    }

    /// Read a host's recent dirs straight from its backend. Blocks for a remote,
    /// so callers go through [`App::host_recent_dirs`] unless they *are* the
    /// refresh. A not-yet-connected host yields an empty list rather than
    /// freezing the TUI through the connect attempt (`request()` would queue).
    fn fetch_recent_dirs(&self, host: &HostId) -> Vec<String> {
        let Some(backend) = self.backend_for(host) else {
            return Vec::new();
        };
        match backend {
            Backend::Local(_) => backend.recent_dirs(),
            Backend::Remote(_) if backend.conn_state().is_connected() => {
                tokio::task::block_in_place(|| backend.recent_dirs())
            }
            Backend::Remote(_) => Vec::new(),
        }
    }

    /// Whether `host` is served by the in-process backend — i.e. this machine,
    /// *not* under pooled-localhost (where it's reached through the daemon like
    /// any other host). The few places that legitimately differ — the in-memory
    /// recent-dir list and the picker's `Ctrl-d` delete — ask this rather than
    /// `is_local()`, which no longer answers the question.
    pub(super) fn is_direct_local(&self, host: &HostId) -> bool {
        matches!(self.backend_for(host), Some(Backend::Local(_)))
    }

    /// Record a launch's cwd into the recent list of the host it landed on. A
    /// remote (or pooled) host records its own server-side inside
    /// `open_in_pool`, so a mac path never pollutes a Linux box's list; only the
    /// direct-local list is ours to write. Either way the cached copy is stale.
    pub(super) fn record_launch_cwd(&mut self, host: &HostId, cwd: &str) {
        if self.is_direct_local(host) {
            self.push_recent_cwd(cwd);
        }
        self.invalidate_recent_dirs(host);
    }

    /// Invalidate a host's cached recent dirs — after a launch records a new
    /// cwd there, which is the only thing that changes the list. The next
    /// picker open re-seeds it.
    pub(super) fn invalidate_recent_dirs(&mut self, host: &HostId) {
        self.recent_dirs_cache.remove(host);
    }

    /// Directory completions for `prefix` on `host`'s filesystem. Local reads the
    /// fs in-process (no `block_in_place`, so it's usable outside a runtime — e.g.
    /// unit tests); a *connected* remote makes a blocking RPC off the async worker.
    /// A not-yet-connected remote returns no completions rather than blocking the
    /// TUI through the connect attempt (`request()` queues while Connecting).
    fn host_complete_path(&self, host: &HostId, prefix: &str) -> Vec<String> {
        let Some(backend) = self.backend_for(host) else {
            return Vec::new();
        };
        match backend {
            // In-process: no `block_in_place`, so it works outside a runtime too
            // (the unit tests call this directly).
            Backend::Local(_) => backend.complete_path(prefix),
            Backend::Remote(_) if backend.conn_state().is_connected() => {
                tokio::task::block_in_place(|| backend.complete_path(prefix))
            }
            Backend::Remote(_) => Vec::new(),
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
        // Cache-first: a host switch renders from memory, so `Ctrl-h` is
        // instant even against a distant box (§9). Only a host never seen this
        // run pays a round-trip, and one that isn't connected yet yields an
        // empty list rather than freezing the TUI through the connect attempt.
        let cwds = self.host_recent_dirs(&host);
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
        // Only the in-process list is the dashboard's to edit; any other host's
        // lives on that machine (deleting there would need an RPC — out of
        // scope), so Ctrl-D is a no-op while the picker targets one.
        let PickerKind::Workdir { host, .. } = &active.kind else {
            return;
        };
        let host = host.clone();
        if !self.is_direct_local(&host) {
            return;
        }
        let Some(active) = self.picker.as_mut() else {
            return;
        };
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

        // Re-seed against the current prefix. Both the prefix we send and the
        // matches we get back are in the host-canonical `~` form (§3), so a `~`
        // resolves against the *host's* home with no home ever reaching the
        // client. Completion is inherently a live filesystem read, so this one
        // does cross the wire — the remote path blocks on a round-trip.
        let matches = self.host_complete_path(&host, &current);
        if matches.is_empty() {
            return;
        }

        if let Some(active) = self.picker.as_mut() {
            active.picker.set_text(&matches[0]);
        }
        self.workdir_completion = Some(WorkdirCompletion { matches, index: 0 });
    }

    /// A path as it should read on screen.
    ///
    /// Almost a no-op now: every path the dashboard holds arrived in the
    /// host-canonical `~` form (§3), because the backend that produced it
    /// collapsed it. The one remaining job is a *local absolute* path that never
    /// crossed the seam — a `ResumeCandidate.cwd` read straight off disk, say —
    /// which is still worth abbreviating against our own home. A `~` form passes
    /// straight through (`collapse_home` is idempotent and leaves it alone), so
    /// this can't mangle another host's path with our home.
    pub(super) fn shorten_path<'a>(&self, path: &'a str) -> std::borrow::Cow<'a, str> {
        cm_core::paths::collapse_home(path, &self.home_dir).into()
    }

    /// The emoji shown for a host in the table's icon column and the host
    /// pickers: the configured one, else a deterministic emoji derived from the
    /// label — so a host always reads as an icon (§9) without the user having
    /// configured anything, and the same host keeps the same glyph run to run.
    pub(super) fn host_icon(&self, host: &HostId) -> String {
        if let Some(icon) = self.host_icons.get(host) {
            return icon.clone();
        }
        // This machine is the one host the user never chose a name for, so it
        // gets a fixed glyph rather than a hash of an arbitrary hostname.
        if self.backends.first().is_some_and(|b| &b.host_id() == host) {
            return "🏠".to_string();
        }
        // Each glyph that has a text presentation carries an explicit variation
        // selector (`U+FE0F`), for the same reason the header's ☁️ tally does:
        // bare `U+1F5A5` / `U+1F6F0` / `U+2699` render as hairline monochrome
        // outlines in the row's foreground colour, and this icon is now the row's
        // *only* host marker (the Host column's name and the per-host colour both
        // went away), so one that washes out leaves the row saying nothing about
        // where it lives. The selector also makes `unicode-width` agree with the
        // two cells the terminal paints, which is what the icon column pads to.
        const FALLBACK: [&str; 8] = [
            "🖥\u{FE0F}",
            "🛰\u{FE0F}",
            "🐧",
            "📦",
            "⚙\u{FE0F}",
            "🌐",
            "🔷",
            "🧊",
        ];
        FALLBACK[format::stable_index(&host.0, FALLBACK.len())].to_string()
    }
}

#[cfg(test)]
mod tests;
