//! `AgentControl` is the dashboard's interface to a coding-agent CLI
//! (Claude Code and Codex). It is per-session, not
//! per-process: a single dashboard runs sessions from several backends side
//! by side, dispatching every backend-shaped operation through the variant
//! stored on each `LauncherState`.
//!
//! The variants carry no instance state — methods are pure functions of
//! `self` and forward to the matching `agents::<name>` module.
//!
//! Adding a new backend is meant to be: add a variant, add a module under
//! `agents/`, extend each `match` here. No registry, no dyn dispatch.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::process::Command;

use crate::agents::{claude, codex};
use crate::state::{HookEvent, HookMessage, LauncherState};

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentControl {
    #[default]
    Claude,
    Codex,
}

impl AgentControl {
    pub const ALL: &'static [AgentControl] = &[AgentControl::Claude, AgentControl::Codex];

    /// CLI subcommand the dashboard launches to wrap this agent
    /// (e.g. `miao claude .`).
    pub fn cli_subcommand(self) -> &'static str {
        match self {
            AgentControl::Claude => "claude",
            AgentControl::Codex => "codex",
        }
    }

    /// Parse the `--agent` flag / config value. Mirrors `cli_subcommand`.
    pub fn from_cli(s: &str) -> Option<AgentControl> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Some(AgentControl::Claude),
            "codex" => Some(AgentControl::Codex),
            _ => None,
        }
    }

    /// Human-facing backend name for headers and status lines.
    pub fn label(self) -> &'static str {
        match self {
            AgentControl::Claude => "Claude",
            AgentControl::Codex => "Codex",
        }
    }

    /// Extra args appended after the cwd to resume (or fork) `session_id`. The
    /// flag shapes differ per backend: Claude takes `--resume <id>` plus an
    /// optional `--fork-session`; Codex uses the `resume` / `fork` subcommands.
    pub fn resume_args(self, session_id: &str, fork: bool) -> Vec<String> {
        match self {
            AgentControl::Claude => {
                let mut v = vec!["--resume".to_string(), session_id.to_string()];
                if fork {
                    v.push("--fork-session".to_string());
                }
                v
            }
            AgentControl::Codex => {
                let sub = if fork { "fork" } else { "resume" };
                vec![sub.to_string(), session_id.to_string()]
            }
        }
    }

    /// Extra args that launch this agent into an isolated git **worktree**, or
    /// `None` when the agent has no worktree concept.
    ///
    /// The worktree itself is entirely the *agent's* — captain-miao never runs
    /// `git worktree add`. Claude Code creates it under
    /// `.claude/worktrees/<name>/` on a new branch, honours `worktree.baseRef`
    /// and `.worktreeinclude`, blocks edits that would reach the main checkout,
    /// and cleans up when the session exits; a resume returns the session to it
    /// with no help from us. Owning any of that here would mean a second,
    /// disagreeing implementation of a thing the agent already does better.
    ///
    /// `name` is the worktree name; `None` lets the agent generate one (Claude
    /// mints e.g. `bright-running-fox`). Codex 0.147 has no equivalent flag, so
    /// it answers `None` and the dashboard hides the affordance — the same shape
    /// as [`Self::session_watch_path`] and [`Self::bg_shells`], which are
    /// likewise Claude-only.
    pub fn worktree_args(self, name: Option<&str>) -> Option<Vec<String>> {
        match self {
            AgentControl::Claude => {
                let mut v = vec!["--worktree".to_string()];
                // A `#`-prefixed PR number is a legitimate name (`--worktree
                // "#1234"` branches from that PR), so nothing here inspects it.
                if let Some(name) = name.filter(|n| !n.is_empty()) {
                    v.push(name.to_string());
                }
                Some(v)
            }
            AgentControl::Codex => None,
        }
    }

    /// Whether this agent can launch into an isolated worktree. Derived from
    /// [`Self::worktree_args`] rather than matched separately, so the UI gate
    /// and the argv can never disagree about which agents support it.
    pub fn supports_worktrees(self) -> bool {
        self.worktree_args(None).is_some()
    }

    // -- Dashboard-side: filesystem watching, transcript reading, naming --

    /// Filesystem paths whose changes should trigger a dashboard reload —
    /// session-name files, transcript directories, etc. Missing dirs are
    /// silently skipped by the caller; this just enumerates candidates.
    pub fn watch_paths(self) -> Vec<PathBuf> {
        match self {
            AgentControl::Claude => claude::watch_paths(),
            AgentControl::Codex => codex::watch_paths(),
        }
    }

    /// Refresh per-pid name and session-id maps from the agent's on-disk
    /// session-name store. The cache lets repeated reloads skip files whose
    /// mtime is unchanged.
    pub fn read_session_index(self, cache: &mut SessionIndexCache) -> SessionIndex {
        match self {
            AgentControl::Claude => claude::read_session_index(cache),
            AgentControl::Codex => codex::read_session_index(cache),
        }
    }

    /// Transcript-derived per-session facts in one pass: context-token total,
    /// model id, custom title (`/rename`), and first-prompt auto-title. `prior`
    /// is the previously folded value for this session, if any: Claude folds only
    /// the transcript bytes appended since `prior`'s cursor (so an active session
    /// isn't rescanned end-to-end), while Codex recomputes stats from a bounded
    /// tail but reuses `prior`'s first prompt once found. The launcher folds this
    /// and stamps the fields onto the session's state file, so the dashboard never
    /// reads a transcript itself. Fields are None before the first relevant entry
    /// (no assistant turn → no `context_tokens`/`model`; no rename → no `name`).
    pub fn read_transcript_stats(
        self,
        transcript: &Path,
        prior: Option<&TranscriptStats>,
    ) -> TranscriptStats {
        match self {
            AgentControl::Claude => claude::read_transcript_stats_incremental(transcript, prior),
            AgentControl::Codex => codex::read_transcript_stats(transcript, prior),
        }
    }

    /// Resumable sessions across all of this agent's transcripts. Most-recent
    /// first, capped at `limit`. The returned candidates carry their source
    /// agent so a future picker can mix backends in one list.
    pub fn list_resumable(self, limit: usize) -> Result<Vec<ResumeCandidate>> {
        match self {
            AgentControl::Claude => claude::list_resumable(limit),
            AgentControl::Codex => codex::list_resumable(limit),
        }
    }

    // -- Launcher-side: process launch, hooks, transcript signals --

    /// Build the subprocess command that runs this agent in `cwd` with hook
    /// callbacks pointing at `sock_path`. The launcher writes any per-session
    /// config files (Claude's `--settings` payload, Codex's synth `$CODEX_HOME`,
    /// etc.) before spawning.
    pub fn build_launch_command(
        self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
    ) -> Result<Command> {
        match self {
            AgentControl::Claude => {
                claude::build_launch_command(cwd, sock_path, settings_path, extra_args)
            }
            AgentControl::Codex => {
                codex::build_launch_command(cwd, sock_path, settings_path, extra_args)
            }
        }
    }

    /// JSON contents of the per-session hook-settings file the launcher
    /// drops on disk before spawning the agent. The file location is
    /// agent-specific and chosen by `build_launch_command`.
    pub fn hooks_settings_json(self, sock_path: &str) -> String {
        match self {
            AgentControl::Claude => claude::build_hooks_settings(sock_path),
            AgentControl::Codex => codex::build_hooks_settings(sock_path),
        }
    }

    /// Apply a hook event to the launcher state. Encapsulates per-agent
    /// status mapping (`PreToolUse` → `Active`, `PreCompact` → `Compacting`,
    /// etc.).
    pub async fn dispatch_hook(self, state: &mut LauncherState, msg: HookMessage) {
        match self {
            AgentControl::Claude => claude::dispatch_hook(state, msg).await,
            AgentControl::Codex => codex::dispatch_hook(state, msg).await,
        }
    }

    /// Parse the agent's stdin JSON hook payload into a normalized
    /// `HookMessage`. Used by the `miao hook` subcommand.
    pub fn parse_hook_payload(self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        match self {
            AgentControl::Claude => claude::parse_hook_payload(event, stdin),
            AgentControl::Codex => codex::parse_hook_payload(event, stdin),
        }
    }

    /// Scan new bytes of the transcript starting at `offset` for signals the
    /// launcher cares about (interrupt detection). Backends that don't expose
    /// such signals return an empty scan.
    pub fn scan_transcript_signals(self, path: &Path, offset: u64) -> TranscriptScan {
        match self {
            AgentControl::Claude => claude::scan_transcript_signals(path, offset),
            AgentControl::Codex => codex::scan_transcript_signals(path, offset),
        }
    }

    /// The agent's own report of what process `agent_pid` is doing, read from
    /// its status file. Authoritative on the coarse working/idle/background-shell
    /// axis, so the launcher can settle a hook-derived `Active` back to rest when
    /// a turn ends with no hook (an interrupt fires no `Stop`). `None` when it
    /// can't be determined (caller leaves the status unchanged). Backends without
    /// a status file return `None`.
    pub fn agent_activity(self, agent_pid: u32) -> Option<AgentActivity> {
        match self {
            AgentControl::Claude => claude::session_activity(agent_pid),
            AgentControl::Codex => codex::session_activity(agent_pid),
        }
    }

    /// The *user-set* display name from the agent's own session file — Claude
    /// writes both its auto-derived slug and the user's `/rename` to
    /// `~/.claude/sessions/<pid>.json`; only the rename is surfaced (the slug is
    /// dropped so the first prompt wins). The launcher folds this onto
    /// `LauncherState.name` so it reaches the dashboard (local *and* remote) over
    /// the state file, with no transcript read. `None` for backends without such a
    /// file (Codex — its sqlite title is overlaid per-host by
    /// [`crate::backend::LocalBackend`]).
    pub fn session_name(self, agent_pid: u32) -> Option<String> {
        match self {
            AgentControl::Claude => claude::read_session_name(agent_pid),
            AgentControl::Codex => None,
        }
    }

    /// File whose changes the launcher should watch to learn about
    /// working↔idle↔background-shell transitions (these fire no hook). For Claude
    /// this is its session-status file; `None` for backends without one.
    pub fn session_watch_path(self, agent_pid: u32) -> Option<PathBuf> {
        match self {
            AgentControl::Claude => claude::session_file_path(agent_pid),
            AgentControl::Codex => None,
        }
    }

    /// `Some(interval)` when the launcher's transcript watch must be a
    /// stat-polling one (`launcher::start_stat_poll`) rather than the
    /// platform's event-driven watcher, because the agent's writer defeats the
    /// platform events.
    ///
    /// Codex opens its rollout once and appends through that fd for the whole
    /// session, and **macOS FSEvents reports nothing for writes through a
    /// long-held fd until the file is closed** (measured: 12 flushed appends
    /// over 36s produced 0 events — on both a file-level and a directory-level
    /// watch, with or without fsync; the close produced 1). An event-driven
    /// watch therefore never wakes the launcher during a Codex session — no
    /// context tokens, no first-prompt fold, and an Esc-interrupt
    /// (`turn_aborted`, which fires **no hook** — verified against the codex
    /// source at 0.142.3: an aborted turn returns before `run_turn_stop_hooks`
    /// and the `notify` program) leaves the row Active forever. A stat poll
    /// sees each append immediately (`write(2)` updates size/mtime at write
    /// time; only the FSEvents notification waits for close). Linux inotify
    /// fires per write, so it stays event-driven there. Claude
    /// opens/writes/closes per line, so FSEvents works and it stays
    /// event-driven everywhere. The poll runs only while the session is off
    /// Idle — see the lifecycle gate in `launcher::process_hooks`.
    ///
    /// Returning `Some` also opts the agent into the launcher's hook-arm
    /// pre-dispatch rescan, which assumes the agent writes its transcript
    /// lines *before* firing the matching hook (true of Codex: `token_count`
    /// lands ~20ms ahead of `Stop`). An agent that wrote them after would
    /// merely make that read a no-op — the next poll tick still catches the
    /// bytes — so the assumption is a latency optimization, not a correctness
    /// requirement.
    pub fn transcript_poll_interval(self) -> Option<Duration> {
        match self {
            AgentControl::Claude => None,
            AgentControl::Codex if cfg!(target_os = "macos") => Some(Duration::from_secs(2)),
            AgentControl::Codex => None,
        }
    }

    /// The agent's currently-running `run_in_background` shells, read from the
    /// **live process tree** (see `claude::bg_shells`) and each classified by
    /// *what* it runs — the launcher's basis for refining a `BackgroundActive`
    /// row into `ReviewPending` (all review-watches), `BackgroundServer` (all
    /// long-running services), or a busy transient task. `None` when nothing is
    /// running or the tree can't be read (the caller leaves the status
    /// unrefined). Always `None` for Codex, which has no `run_in_background`
    /// concept.
    pub fn bg_shells(self, agent_pid: u32) -> Option<Vec<BgShell>> {
        match self {
            AgentControl::Claude => claude::bg_shells(agent_pid),
            AgentControl::Codex => None,
        }
    }
}

/// One of an agent's running `run_in_background` shells, reduced to what the
/// launcher's background-status refinement needs: a normalized command `key`
/// (the learning identity, stable across sessions) and the `kind` a *static*
/// classifier assigned it. "Static" means from the command text alone — the
/// launcher then overlays the learned store and per-command durations on top of
/// an `Other` to decide busy-vs-at-rest (see `launcher::classify_and_learn`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgShell {
    /// The normalized command (the agent's actual `run_in_background` command,
    /// extracted from the Bash-tool wrapper) — the key both the learning store
    /// and the duration tracker use to recognize "the same command" again.
    pub key: String,
    /// What the command text alone says this is.
    pub kind: BgSeedKind,
}

/// A background shell's classification from its command text alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgSeedKind {
    /// An r3 review-watch (`r3 watch <review-id>`) — the agent is blocked on a
    /// human review → `ReviewPending`.
    ReviewWatch,
    /// A recognized long-running service (dev server / watcher) per the seed
    /// heuristic → at-rest `BackgroundServer`, no waiting to learn it.
    LongRunning,
    /// Anything else — a finite build/test/step by default (busy), unless the
    /// learned store or a duration threshold later reclassifies it as
    /// long-running.
    Other,
}

/// The agent's own report of what it's doing, read from its status file
/// (Claude's `~/.claude/sessions/<pid>.json`). Coarser than `SessionStatus` — it
/// only distinguishes "still working" from the two at-rest shapes — and is used
/// to reconcile the working/idle/background-shell axis when a hook is missed
/// (e.g. an interrupt fires no `Stop`). The launcher only ever *demotes* a busy
/// hook status toward rest on this signal, never promotes — hook events own the
/// rest→active direction.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum AgentActivity {
    /// Mid-turn: the model is running or a foreground tool is executing.
    Working,
    /// The turn has ended and nothing it spawned is still running.
    Idle,
    /// The turn has ended but a `run_in_background` shell is still running.
    BackgroundShell,
}

// -- Generic types shared across backends --

/// Lookup tables derived from an agent's on-disk session manifest. The
/// dashboard merges entries from every active backend into one view; per-row
/// lookups dispatch via `state.agent`. Only Claude's manifest scan populates
/// the name maps today (renames only — its auto slug is dropped, and Codex's
/// title is overlaid onto `LauncherState.name` by the host's `LocalBackend`
/// instead), so the index's name contribution is a local-Claude fallback;
/// `session_id_by_pid` is its other, still-load-bearing job.
#[derive(Debug, Default, Clone)]
pub struct SessionIndex {
    /// Map child pid → display name.
    pub by_pid: HashMap<u32, String>,
    /// Owning backend for each pid in `by_pid`. Recorded at merge time (the
    /// per-backend shards don't know it) so the `by_pid` fallback in `lookup`
    /// can be gated on the row's own backend — a dead Claude session's pid can
    /// be reused by an unrelated Codex child, and without this the recycled pid
    /// would surface the stale Claude name on the Codex row.
    pub by_pid_owner: HashMap<u32, AgentControl>,
    /// Map session id → display name (a Claude `/rename` from its session-file
    /// manifest).
    pub by_session_id: HashMap<String, String>,
    /// Map child pid → live session id, used as a fallback when the launcher
    /// hasn't yet observed a session id from a hook event.
    pub session_id_by_pid: HashMap<u32, String>,
}

impl SessionIndex {
    /// Best display name for `state`, preferring the live session id (which
    /// covers renames) and falling back to the child-pid manifest entry.
    pub fn lookup(&self, state: &LauncherState) -> Option<&str> {
        if let Some(sid) = self.live_session_id(state)
            && let Some(name) = self.by_session_id.get(sid)
        {
            return Some(name.as_str());
        }
        // The pid maps only ever hold *local* sessions (a remote backend serves
        // an empty index), so a remote row must never borrow a name via a
        // colliding local pid — gate the by-pid fallback on the session's host.
        if state.host.is_local()
            && let Some(pid) = state.child_pid
            && self.by_pid_owner.get(&pid) == Some(&state.agent)
            && let Some(name) = self.by_pid.get(&pid)
        {
            return Some(name.as_str());
        }
        None
    }

    /// Live session id for `state`. The launcher updates `state.session_id`
    /// from every hook event, so it's authoritative when present; the manifest
    /// entry is only used as a startup-time fallback (local sessions only —
    /// `session_id_by_pid` holds no remote pids).
    pub fn live_session_id<'a>(&'a self, state: &'a LauncherState) -> Option<&'a str> {
        if let Some(sid) = state.session_id.as_deref() {
            return Some(sid);
        }
        if !state.host.is_local() {
            return None;
        }
        state
            .child_pid
            .and_then(|pid| self.session_id_by_pid.get(&pid).map(|s| s.as_str()))
    }
}

/// Per-pid mtime-keyed cache used by `read_session_index` to skip the JSON
/// parse for files that haven't changed since the last reload.
pub type SessionIndexCache = HashMap<u32, SessionIndexEntry>;

#[derive(Debug, Default, Clone)]
pub struct SessionIndexEntry {
    pub mtime: Option<SystemTime>,
    pub session_id: Option<String>,
    pub name: Option<String>,
}

/// One resumable session surfaced by `AgentControl::list_resumable`.
/// `Serialize`/`Deserialize` so a `captain-miao server` can ship it to a remote
/// dashboard's resume picker over the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCandidate {
    pub agent: AgentControl,
    pub session_id: String,
    pub cwd: String,
    pub first_prompt: Option<String>,
    pub custom_title: Option<String>,
    pub git_branch: Option<String>,
    pub mtime: SystemTime,
}

/// Per-session facts pulled from one pass over the transcript. Both fields come
/// from the same assistant entries (Claude) / the same rollout tail (Codex), so
/// reading them together avoids a second stat + file read per reload.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TranscriptStats {
    /// Latest context-window token total, in tokens.
    pub context_tokens: Option<u64>,
    /// Model id backing the latest turn (e.g. `claude-opus-4-8`, `gpt-5.5`).
    pub model: Option<String>,
    /// First real user prompt — the auto-title fallback shown before a rename
    /// (first-wins).
    pub first_prompt: Option<String>,
    /// Claude-only incremental-parse cursor: the byte offset reached plus the
    /// running accumulators, so the next reload folds only the lines appended
    /// since — instead of rescanning a multi-MB transcript on every keystroke
    /// the agent writes. `None` for Codex (which recomputes from a bounded
    /// tail) and before the first parse. Opaque to the dashboard, which reads
    /// only the two fields above.
    pub cursor: Option<claude::StatsCursor>,
}

/// Result of an incremental transcript scan — the launcher reads new bytes
/// since the last `new_offset` and the backend extracts whatever side-band
/// signals it cares about.
#[derive(Default)]
pub struct TranscriptScan {
    pub new_offset: u64,
    /// True if the new bytes contain an interrupt sentinel — agents that
    /// fire no hook on Esc need this so the launcher can leave Active.
    pub interrupted: bool,
    /// True if the new bytes contain a compact-command stderr — Claude fires
    /// no `PostCompact` when `/compact` itself errors (e.g. "Not enough
    /// messages to compact"), so without this the launcher would stay in
    /// `Compacting` forever.
    pub compact_aborted: bool,
}

/// The transcript bytes appended since `offset`, decoded lossily, plus the
/// offset a [`TranscriptScan`] should carry forward. Both backends read the
/// transcript tail identically — only the line-scan differs — so the byte
/// plumbing lives here to keep `claude` and `codex` from drifting.
pub struct TranscriptDelta {
    pub text: String,
    pub new_offset: u64,
}

/// Read the bytes appended to `path` since `offset`, lossily decoded.
///
/// `new_offset` advances past exactly the committed bytes that were read, so a
/// permanently-committed non-UTF-8 byte can't fail the read forever and freeze
/// the offset (which would lose later interrupt / compact-aborted signals).
/// Failure modes mirror the historical behaviour both backends relied on:
///   - open / metadata / seek failure, or `len < offset` (the file was
///     truncated or rotated) → `new_offset = 0`, empty text (re-read from the
///     start on the next scan);
///   - already at EOF (`len == offset`) or a read error → `new_offset = offset`,
///     empty text (hold position, surface no signals).
pub fn read_transcript_delta(path: &Path, offset: u64) -> TranscriptDelta {
    let reset = TranscriptDelta {
        text: String::new(),
        new_offset: 0,
    };
    let hold = TranscriptDelta {
        text: String::new(),
        new_offset: offset,
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return reset;
    };
    let Ok(meta) = file.metadata() else {
        return reset;
    };
    let len = meta.len();
    if len < offset {
        return reset;
    }
    if len == offset {
        return hold;
    }
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return reset;
    }
    let mut bytes: Vec<u8> = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return hold;
    }
    TranscriptDelta {
        new_offset: offset + bytes.len() as u64,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}
