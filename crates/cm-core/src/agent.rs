//! Serialized identity and shared agent facts. Immutable definitions live with
//! each integration; configured adapters own settings and opaque session tasks.
//! A single resolver selects an agent's definition, so adding an integration
//! does not extend every operation in this module.
//!
//! [`AgentKind::Unknown`] is the one variant that isn't a backend. `agent`
//! rides `LauncherState`, and `LauncherState` rides the `Snapshot` frame as a
//! `Vec` — so a single element that refuses to decode fails the *whole* frame,
//! and one session started under a backend this build predates would blank
//! every row on that host. The protocol's frame-level rule (an unknown frame
//! decodes to `Unknown` and is ignored) therefore extends one level down to
//! this field: an unrecognized name decodes here instead of erroring, every
//! method answers inertly, and the row stays visible, sortable and killable
//! while nothing can be launched from it.
//!
//! What keeps that safe is that **`Unknown` is a read-side-only state**. It
//! arises solely from decoding a value this build does not know: `from_cli`
//! never yields it (so `miao --agent nonsense` is still an honest error, not a
//! dimmed row), it is not in `ALL` (so no picker or `Ctrl-t` cycle can land on
//! it), and no launcher may ever write it into a state file — a host knows its
//! own backends, so `"agent":"unknown"` on the wire can only be a decoded value
//! being echoed back, which the launch path refuses rather than guessing. That
//! last clause is enforced by both the host launch plan and adapter
//! configuration: each refuses with [`UNKNOWN_AGENT_REFUSAL`], so no path can
//! turn an undecodable name into a process.
//!
//! It deliberately does not preserve the original name. `AgentKind` is
//! `Copy`, is used as a `HashMap` key, and every method takes `self` by value;
//! carrying a `String` would cost all of that repo-wide to hold a label nothing
//! is allowed to act on.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::agents;

pub(crate) mod session;
use crate::state::{HookEvent, HookMessage, LauncherState, ReattachPrime};
use session::AgentAdapter;

// =============================================================================
// Which backends exist, and what they can do
// =============================================================================

/// `Deserialize` is hand-written (see below) so an unrecognized name lands on
/// [`AgentKind::Unknown`]; `Serialize` stays derived — `rename_all` maps that
/// variant to `"unknown"`, which decodes back to itself.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
    Reasonix,
    Kimi,
    Grok,
    /// sst's `opencode`. Spelled lowercase everywhere, including
    /// [`Self::label`] — it is the project's own styling, not a typo.
    OpenCode,
    Pi,
    /// Google's Antigravity CLI. The variant, the on-disk name and the
    /// subcommand are all the product's name; `agy` is only what it installs
    /// as, and is spelled that way in one place ([`agents::antigravity::BIN`]).
    Antigravity,
    /// `can1357/oh-my-pi` ("omp"), a heavily-evolved fork of pi. Hooked with a
    /// generated extension passed as `omp -e`; nothing of yours is touched.
    /// omp's event surface has diverged from pi's in four load-bearing ways —
    /// see `agents::omp`'s module doc.
    Omp,
    /// A backend name this build doesn't know — a newer host's session seen by
    /// an older dashboard. Read-side only: produced by `Deserialize` alone,
    /// never by `from_cli`, never in `ALL`, never written by a launcher. Every
    /// method answers inertly so the row survives; launching from it errors.
    /// See the module doc for why it holds no name.
    Unknown,
}

/// Shared refusal for an unknown agent in host launch plans and adapter
/// configuration. Either entry point must explain how to resolve the mismatch.
pub const UNKNOWN_AGENT_REFUSAL: &str = "unknown agent backend: this build doesn't know it (the \
                                         request comes from a newer captain-miao); upgrade \
                                         captain-miao to launch it";

impl<'de> Deserialize<'de> for AgentKind {
    /// Accepts exactly the names [`AgentKind::cli_subcommand`] emits and maps
    /// everything else to [`AgentKind::Unknown`] — the whole point being that
    /// a value from a newer host must not fail the frame it arrived in. Matching
    /// is exact, as on-disk state has always been; the case-insensitive spelling
    /// belongs to `from_cli`, where a human types the name.
    ///
    /// Still string-only: a number or an object is a malformed field, not a
    /// backend this build predates, and is reported as the decode error it is.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct AgentVisitor;
        impl serde::de::Visitor<'_> for AgentVisitor {
            type Value = AgentKind;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an agent backend name")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<AgentKind, E> {
                Ok(match v {
                    "claude" => AgentKind::Claude,
                    "codex" => AgentKind::Codex,
                    "reasonix" => AgentKind::Reasonix,
                    "kimi" => AgentKind::Kimi,
                    "grok" => AgentKind::Grok,
                    "opencode" => AgentKind::OpenCode,
                    "pi" => AgentKind::Pi,
                    "antigravity" => AgentKind::Antigravity,
                    "omp" => AgentKind::Omp,
                    _ => AgentKind::Unknown,
                })
            }
        }
        d.deserialize_str(AgentVisitor)
    }
}

impl AgentKind {
    pub(crate) fn definition(self) -> &'static dyn agents::definition::AgentDefinition {
        match self {
            Self::Claude => &agents::claude::Definition,
            Self::Codex => &agents::codex::Definition,
            Self::Reasonix => &agents::reasonix::Definition,
            Self::Kimi => &agents::kimi::Definition,
            Self::Grok => &agents::grok::Definition,
            Self::OpenCode => &agents::opencode::Definition,
            Self::Pi => &agents::pi::Definition,
            Self::Antigravity => &agents::antigravity::Definition,
            Self::Omp => &agents::omp::Definition,
            Self::Unknown => agents::definition::UNKNOWN,
        }
    }

    pub(crate) fn configure(
        self,
        config: &crate::config::AgentConfig,
    ) -> Result<Box<dyn AgentAdapter>> {
        self.definition().configure(config)
    }

    /// Every backend this build can actually drive — `Unknown` is deliberately
    /// absent, which is what keeps it out of the `Space a` picker and the
    /// `Ctrl-t` cycle.
    ///
    /// **Append, never reorder**: this seeds the default `Ctrl-t` cycle and
    /// agent picker order. Preferences can reorder or disable entries.
    pub const ALL: &'static [AgentKind] = &[
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Reasonix,
        AgentKind::Kimi,
        AgentKind::Grok,
        AgentKind::OpenCode,
        AgentKind::Pi,
        AgentKind::Antigravity,
        AgentKind::Omp,
    ];

    /// CLI subcommand the dashboard launches to wrap this agent
    /// (e.g. `miao claude .`).
    pub fn cli_subcommand(self) -> &'static str {
        self.definition().cli_subcommand()
    }

    /// Parse the `--agent` flag / config value: the inverse of
    /// [`cli_subcommand`](Self::cli_subcommand), *derived* from it rather than
    /// restated, so a new backend cannot land with a name that renders but does
    /// not parse.
    ///
    /// Never yields `Unknown`: a name this build can't drive stays `None` so the
    /// caller reports it, rather than launching into an inert backend. That now
    /// falls out of the search space — [`ALL`](Self::ALL) omits `Unknown`.
    pub fn from_cli(s: &str) -> Option<AgentKind> {
        let s = s.to_ascii_lowercase();
        Self::ALL.iter().copied().find(|a| a.cli_subcommand() == s)
    }

    /// Input that attaches an image from this host when the agent's native
    /// clipboard reader cannot use the command shims. `None` leaves Ctrl+V to
    /// the agent. The encoder is selected before fetching anything, so other
    /// agents never make an extra clipboard request through this route.
    pub fn clipboard_paste_encoder(self) -> Option<fn(&Path) -> Vec<u8>> {
        self.definition().clipboard_paste_encoder()
    }

    /// Human-facing backend name for headers and status lines.
    pub fn label(self) -> &'static str {
        self.definition().label()
    }

    /// Extra args appended after the cwd to resume (or fork) `session_id`.
    ///
    /// The flag shapes differ per backend and each definition states its own; the
    /// three *kinds* are what matter here. Most decorate a resume with a fork
    /// flag (Claude and Grok share `--resume <id>` / `--fork-session`; Reasonix
    /// takes `-r <id>` / `--copy`; opencode `-s <id>` / `--fork`). Two swap the
    /// entry point instead of adding to it — Codex's `resume` / `fork`
    /// subcommands, and Pi's `--session` / `--fork`, which are alternatives
    /// rather than a flag and its modifier. One ignores `fork` entirely (Kimi),
    /// which is how a backend says it cannot branch: the two argvs come out
    /// equal and the declared fork capability stays false.
    pub fn resume_args(self, session_id: &str, fork: bool) -> Vec<String> {
        self.definition().resume_args(session_id, fork)
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
    /// as [`Self::session_watch_path`] and [`Self::bg_shells`].
    pub fn worktree_args(self, name: Option<&str>) -> Option<Vec<String>> {
        self.definition().worktree_args(name)
    }

    /// Whether this launch will put the agent's TUI on the terminal's
    /// **alternate screen**, judged for a *plain pty* — the pool's
    /// environment, the only place the answer is consumed
    /// ([`LauncherState::alt_screen`] has the full story).
    ///
    /// A method, not a [`Self::capabilities`] field, because it isn't
    /// structural: Grok decides per process, from config and argv, and this is
    /// the launch-time mirror of that decision. `false` is the safe answer for
    /// a backend nobody has verified on a pool pty — a missed prime keeps the
    /// terminal exactly as it is today, while a wrong prime strands an inline
    /// TUI in a cleared alternate screen and the eventual leave-toggle then
    /// discards everything on it.
    pub fn uses_alt_screen(self, agent_args: &[String]) -> bool {
        self.definition().uses_alt_screen(agent_args)
    }

    /// Whether this launch will push the **kitty keyboard protocol**,
    /// judged from the environment the agent will see — the
    /// companion to [`Self::uses_alt_screen`], and consumed in the same one
    /// place ([`LauncherState::kitty_keyboard`]). Grok's verified recipe
    /// requires `alt_screen`; Codex's input modes are independent of it.
    /// Same bargain as there:
    /// `false` for any backend nobody has probed, because a missed push
    /// keeps reattach exactly as it is today while a wrong push feeds
    /// CSI-u key encodings to a parser that never opted into them.
    pub fn uses_kitty_keyboard(self, term: &str, alt_screen: bool) -> bool {
        self.definition().uses_kitty_keyboard(term, alt_screen)
    }

    /// Compose the attach-time recipe from the launcher's recorded decisions.
    /// Input protocols are independent of the screen the agent renders on.
    pub(crate) fn reattach_prime(self, alt_screen: bool, kitty_keyboard: bool) -> ReattachPrime {
        self.definition().reattach_prime(alt_screen, kitty_keyboard)
    }

    /// Everything a backend may structurally lack, in one query — the
    /// `AgentKind` counterpart to the dashboard's terminal capabilities, and the
    /// same bargain: a limit the UI has to gate on is a *field* here, not a
    /// special case at the call site.
    ///
    /// **Constants, not derivations, because the dashboard reads this while
    /// drawing a frame.** Every field could be computed from the method that
    /// owns it — `fork` from the two argvs, `approval_gate` from the generated
    /// hook config — but three of the four would then allocate per row per
    /// redraw, and the dashboard's terminal capabilities set the precedent that this
    /// query does no work.
    ///
    /// What keeps a constant honest is that the derivation still runs, in
    /// `the_capability_matrix_agrees_with_the_implementations`: every field here
    /// is checked against the code that would have to change for it to be
    /// wrong. So a backend that loses its permission hook to an upstream rename,
    /// or grows a fork flag, fails that test rather than shipping a dashboard
    /// that promises the wrong thing. Declaring a capability is cheap; declaring
    /// one that isn't true is not possible.
    pub fn capabilities(self) -> AgentCapabilities {
        self.definition().capabilities()
    }

    /// Every [`HookEvent`] this backend actually subscribes to, read back out of
    /// the hook config it generates rather than declared a second time.
    ///
    /// This works uniformly because every backend renders
    /// [`HookEvent::as_kebab`] into what it writes: Claude and the four
    /// shell-hook backends as the last word of the `miao hook …` command, and
    /// opencode and Pi as a `[native, event]` row in the generated plugin or
    /// extension. The generated file *is* the subscription list, so reading it
    /// back cannot disagree with it — which is what lets
    /// [`Self::capabilities`] derive `approval_gate` instead of asserting it.
    ///
    /// Order follows [`HookEvent::ALL`], not the config.
    pub fn forwarded_events(self) -> Vec<HookEvent> {
        // Only Claude splices the socket in at all, and no event name can be
        // confused with this: it is a path, not a bare token.
        let config = self.hooks_settings_json("/dev/null");
        HookEvent::ALL
            .iter()
            .copied()
            .filter(|e| mentions_event(&config, e.as_kebab()))
            .collect()
    }

    // -- Dashboard-side: filesystem watching, transcript reading, naming --

    /// Filesystem paths whose changes should trigger a dashboard reload —
    /// session-name files, transcript directories, etc. Missing dirs are
    /// silently skipped by the caller; this just enumerates candidates.
    pub fn watch_paths(self) -> Vec<PathBuf> {
        self.definition().watch_paths()
    }

    /// Paths outside the launcher's own `sessions/` dir whose changes must still
    /// wake a host's subscribers — an agent's out-of-band store that no session
    /// event touches, so a change there reaches a remote dashboard only if the
    /// server wakes and re-diffs on it.
    ///
    /// Deliberately *not* [`Self::watch_paths`], which answers a different
    /// question: what a **dashboard** must watch to keep its own derived reads
    /// fresh. The two agree for Codex today and disagree for Claude, and the
    /// disagreement is the point — Claude's `~/.claude/sessions` feeds the local
    /// session-name index, but every fact in it that a *remote* dashboard needs
    /// is folded onto the state file by the launcher first, so watching it
    /// server-side would only wake subscribers with nothing new to push.
    pub fn out_of_band_watch_paths(self) -> Vec<PathBuf> {
        self.definition().out_of_band_watch_paths()
    }

    /// Refresh per-pid name and session-id maps from the agent's on-disk
    /// session-name store. The cache lets repeated reloads skip files whose
    /// mtime is unchanged.
    pub fn read_session_index(self, cache: &mut SessionIndexCache) -> SessionIndex {
        self.definition().read_session_index(cache)
    }

    /// JSON contents of the per-session hook-settings file the launcher
    /// drops on disk before spawning the agent. The file location is
    /// agent-specific and chosen by `build_launch_command`.
    pub fn hooks_settings_json(self, sock_path: &str) -> String {
        self.definition().hooks_settings_json(sock_path)
    }

    /// Construct independent hook state; observation starts only in the running task.
    pub(crate) fn hook_session(self) -> Box<dyn crate::agents::hook_session::HookSession> {
        self.definition().hook_session()
    }

    /// Parse the agent's stdin JSON hook payload into a normalized
    /// `HookMessage`. Used by the `miao hook` subcommand.
    pub fn parse_hook_payload(self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        self.definition().parse_hook_payload(event, stdin)
    }

    // =============================================================================
    // Probing a live session
    // =============================================================================

    /// Whether a session that has just ended a turn will open its **own** next
    /// one, with no hook to announce it — so its `Stop` is a turn boundary
    /// rather than rest, and parking the row at Idle would be wrong.
    ///
    /// Asked at the moment the launcher would park a row, which is what keeps
    /// this event-driven: nothing is watched, nothing is timed, the one backend
    /// that can answer `true` reads its own store and every other one answers
    /// from the shape of its turn model.
    pub fn self_continues(self, session_id: &str) -> bool {
        self.definition().self_continues(session_id)
    }

    /// Whether *any* session of this backend can self-continue — the question
    /// [`Self::self_continues`] answers about one session, asked of the backend.
    /// The agent declares this alongside its continuation reader: this is true
    /// exactly where that reader has a store to ask.
    ///
    /// The launcher needs the backend-level answer because a `Stop` that leaves a
    /// row **busy** no longer means one thing. Codex's is a *bet*: its goal store
    /// is asked before Codex itself has decided, so the hold is unconfirmed and
    /// the launcher arms a deadline to settle it (`confirm_hold_at`). Claude's is
    /// not a bet at all — `claude::status_after_stop` holds `Active` because
    /// Claude's own session file reads `busy`, i.e. a subagent is still running.
    /// That is evidence, not a prediction; it needs no confirming, and the file's
    /// next write revokes it. Arming the deadline on it parks a working row at
    /// `Idle` two seconds later and nothing wakes it again: the file stays frozen
    /// at `busy` for the whole subagent phase, so the mirror gets no event and the
    /// row reads at rest until some later tool hook happens to land.
    ///
    /// Deliberately per-*backend*: the launcher asks while dispatching the `Stop`,
    /// and asking [`Self::self_continues`] that early is exactly the stale read the
    /// confirmation exists to correct.
    pub fn may_self_continue(self) -> bool {
        self.definition().may_self_continue()
    }

    /// Scan new bytes of the transcript starting at `offset` for signals the
    /// launcher cares about (interrupt detection). Backends that don't expose
    /// such signals return an empty scan.
    pub fn scan_transcript_signals(self, path: &Path, offset: u64) -> TranscriptScan {
        self.definition().scan_transcript_signals(path, offset)
    }

    /// The agent's own report of what process `agent_pid` is doing, read from
    /// its status file. Authoritative on the coarse working/idle/background-shell
    /// axis, so the launcher can settle a hook-derived `Active` back to rest when
    /// a turn ends with no hook (an interrupt fires no `Stop`). `None` when it
    /// can't be determined (caller leaves the status unchanged). Backends without
    /// a status file return `None`.
    pub fn agent_activity(self, agent_pid: u32) -> Option<AgentActivity> {
        self.definition().agent_activity(agent_pid)
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
        self.definition().session_name(agent_pid)
    }

    /// File whose changes the launcher should watch to learn about
    /// working↔idle↔background-shell transitions (these fire no hook). For Claude
    /// this is its session-status file; `None` for backends without one.
    pub fn session_watch_path(self, agent_pid: u32) -> Option<PathBuf> {
        self.definition().session_watch_path(agent_pid)
    }

    /// Whether a watched transcript write can clear a pending approval after
    /// the launcher's grace period. Preserve that fast path for conversation
    /// logs; Grok watches summary/token metadata that can change during a
    /// permission wait (for example, a background memory flush). Its hooks
    /// must release the wait. Other backends do not watch a transcript.
    pub(crate) fn transcript_write_clears_approval(self) -> bool {
        self.definition().transcript_write_clears_approval()
    }

    /// Catch up persisted facts before dispatching a hook. Kimi and Antigravity
    /// report lifecycle changes themselves, so their metadata needs no timer.
    /// Codex's macOS poll also catches up here to preserve hook/rollout order.
    pub(crate) fn refresh_transcript_on_hook(self) -> bool {
        self.definition().refresh_transcript_on_hook()
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
        self.definition().transcript_poll_interval()
    }

    /// True when the watched transcript path is replaced (new inode) rather
    /// than appended in place. The event-driven file watch dies with the old
    /// inode, so the launcher re-arms it after every notify wake. This is not
    /// a poll: it runs only because an event just fired, and it still watches
    /// that one file — never the parent directory, where a chatty sibling
    /// (Grok's `updates.jsonl`) would keep the loop spinning.
    pub fn transcript_path_is_replaced(self) -> bool {
        self.definition().transcript_path_is_replaced()
    }

    /// Extra files rewritten the same way as the watched transcript path —
    /// Grok's `signals.json` beside `summary.json`. The launcher watches each
    /// only when it already exists: a missing sidecar must not fall back to
    /// the session directory (`updates.jsonl` lives there). Empty for every
    /// other backend.
    pub fn replaced_sidecar_paths(self, transcript: &Path) -> Vec<PathBuf> {
        self.definition().replaced_sidecar_paths(transcript)
    }

    /// Live background commands. `None` means unavailable; an empty list is
    /// positive evidence that no shell remains. Only read after a turn ends.
    pub fn bg_shells(self, agent_pid: u32) -> Option<Vec<BgShell>> {
        self.definition().bg_shells(agent_pid)
    }
}

/// What a backend structurally cannot do, queried in one place via
/// [`AgentKind::capabilities`]. Constants or derivations per backend — no IO.
///
/// **Only structural limits belong here.** A field means *the agent has no
/// mechanism for this at all*, never *we haven't wired it up yet*: the
/// dashboard's answer to a missing capability is to stop offering the thing, and
/// doing that to work that merely hasn't been done buries it as impossible.
/// Pi's approval gate is a structural limit: `security.md` says it runs shell
/// commands with its own permissions and has no per-tool prompt, so there is
/// nothing to reflect however much code we write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// A resume can **branch** rather than continue the session in place, so `f`
    /// has something to offer. Derived from [`AgentKind::resume_args`] —
    /// Kimi documents no fork flag, so its two argvs come out equal.
    pub fork: bool,
    /// The agent can launch itself into an isolated git worktree, so `Ctrl-g` in
    /// the workdir picker has something to arm. Derived from
    /// [`AgentKind::worktree_args`]; Claude and Grok are the two that can.
    pub worktrees: bool,
    /// The agent has a per-tool approval prompt it tells us about, so a row can
    /// reach [`crate::state::SessionStatus::WaitingForApproval`] and `s` can
    /// stop on it. Derived from [`AgentKind::forwarded_events`].
    ///
    /// **Pi is the one backend that answers `false`**, and it is not for want of
    /// a hook to subscribe to: `security.md` states it ships no sandbox and runs
    /// shell commands with the pi process's own permissions, and its `--approve`
    /// / trust machinery guards *loading settings and extensions*, not tool
    /// calls. So a blocked Pi session does not exist to be detected, and the
    /// only thing the dashboard owes the user is not to imply it looked.
    pub approval_gate: bool,
    /// A context-token total reaches the row at all, so an empty Context field
    /// means "not yet" rather than "not ever". Reasonix answers `false`;
    /// see [`AgentKind::capabilities`] for why that is the agent's limit
    /// rather than ours.
    pub context_tokens: bool,
}

/// Whether `config` names `kebab` as a **whole** event token.
///
/// Every generated hook config renders an event as the last word of a `miao hook
/// …` command or as a bare JSON/JS string, so the name always *opens* on a space
/// or a quote. What may follow it is open-ended: a closing quote, the end of the
/// input, or more shell — Antigravity's commands continue `>/dev/null; echo …`
/// because that agent requires JSON on a hook's stdout. The trailing side admits anything
/// that could not be part of the name itself.
///
/// The boundary check is load-bearing, not decoration: `stop` is a prefix of
/// `stop-failure`, `elicitation` of `elicitation-result` and `post-tool-use` of
/// `post-tool-use-failure`, so a plain `contains` would report three
/// subscriptions no backend ever registered.
fn mentions_event(config: &str, kebab: &str) -> bool {
    config.match_indices(kebab).any(|(at, _)| {
        let before = config[..at].chars().next_back();
        let after = config[at + kebab.len()..].chars().next();
        // Opening side stays exact rather than "not a name char": an agent
        // binary living under a path segment that happens to *be* an event name
        // would otherwise register every backend for it.
        matches!(before, Some(' ' | '"'))
            && after.is_none_or(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    })
}

/// One live background task, with its command extracted from the agent's
/// wrapper. The dashboard uses this identity for the user's idle-task list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgShell {
    pub key: String,
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

// =============================================================================
// Generic types shared across backends
// =============================================================================

// =============================================================================
// Types shared across backends
// =============================================================================

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
    pub by_pid_owner: HashMap<u32, AgentKind>,
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
/// What a backend's own session index knows about one session — enough to name
/// a row before any transcript has been read.
pub struct SessionIndexEntry {
    pub mtime: Option<SystemTime>,
    pub session_id: Option<String>,
    pub name: Option<String>,
}

/// One resumable session surfaced by the host's [`crate::resume::Inventory`].
/// `Serialize`/`Deserialize` so a `captain-miao server` can ship it to a remote
/// dashboard's resume picker over the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCandidate {
    pub agent: AgentKind,
    pub session_id: String,
    pub cwd: String,
    pub first_prompt: Option<String>,
    pub custom_title: Option<String>,
    pub git_branch: Option<String>,
    pub mtime: SystemTime,
}

/// Persisted facts for a known session resumed without an initial prompt.
/// These seed a newly spawned launcher's idle row, never replay old activity.
pub struct ResumeMetadata {
    pub session_id: String,
    pub stats: TranscriptStats,
}

/// Per-session facts pulled from one pass over the transcript. Fields come
/// from the same assistant entries (Claude) / the same rollout tail (Codex), so
/// reading them together avoids a second stat + file read per reload.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TranscriptStats {
    /// Latest context-window token total, in tokens.
    pub context_tokens: Option<u64>,
    /// The window those tokens sit in, when the agent persists one (Grok's
    /// `contextWindowTokens`). `None` for backends that only report a used
    /// count — the dashboard then colors against the configured absolute
    /// thresholds rather than a percentage of this.
    pub context_window: Option<u64>,
    /// Model id backing the latest turn (e.g. `claude-opus-4-8`, `gpt-5.5`).
    pub model: Option<String>,
    /// Reasoning effort reported for the latest turn, when available.
    pub effort: Option<String>,
    /// First real user prompt — the auto-title fallback shown before a rename
    /// (first-wins).
    pub first_prompt: Option<String>,
    /// The agent's own session title, when the fold reads one (Grok's
    /// `generated_title`). Stamped onto [`crate::state::LauncherState::name`]
    /// last-write-wins, same rule as a hook's `session_title`. `None` for
    /// backends whose title arrives some other way (Claude's session file,
    /// Codex sqlite, a hook payload).
    pub name: Option<String>,
    /// Latest prompt from Antigravity's log, or Grok's sidecar recap. The
    /// recap is applied only at rest to preserve a live hook's user prompt;
    /// Antigravity's actual prompt is also applied during a turn.
    pub last_prompt: Option<String>,
    /// The directory the agent's latest turn ran in, when the fold reads one
    /// (Claude stamps `cwd` on every transcript entry). Stamped onto
    /// [`crate::state::LauncherState::cwd`] last-write-wins, so a session that
    /// enters a worktree mid-flight moves its row there — and a Bash `cd`
    /// never does, because the transcript's `cwd` is the session's, not the
    /// shell's. That is the reason the fold owns the directory rather than a
    /// hook: Claude's `CwdChanged` fires for the shell (its `old_cwd`/`new_cwd`
    /// are the `cd`'s), and its `cwd` was seen carrying the `cd` target even
    /// under `CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR`, which parked a row on
    /// a subdirectory of its project for hours. `None` for backends whose
    /// transcript carries no directory.
    pub cwd: Option<String>,
}

impl TranscriptStats {
    /// Adopt a hook fact. Stateful readers update their own accumulators too.
    pub fn adopt_model(&mut self, model: &str) {
        self.model = Some(model.to_owned());
    }
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
    /// True if the new bytes *end* with a turn the agent opened itself — no
    /// prompt hook, no tool hook, nothing the hook arm will ever see (Codex
    /// under a goal). Ends-with rather than contains-any, because one delta
    /// routinely carries the end of one turn and the start of the next.
    ///
    /// The only **promoting** signal here, and deliberately without a
    /// demoting mirror: every ordinary turn *end* already arrives as a Stop
    /// hook, so a closed turn tells the launcher nothing it wasn't told.
    pub turn_started: bool,
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal state file / snapshot element: only the fields without a serde
    /// default, so the agent name is the variable under test.
    fn state_json(agent: &str, pid: u32) -> String {
        format!(
            r#"{{"agent":"{agent}","launcher_pid":{pid},"cwd":"/home/miao/p",
               "status":"idle","updated_at":0}}"#
        )
    }

    /// A name no build will ever know, standing in for the backend a *newer*
    /// captain-miao writes into a state file. Deliberately not a name off the
    /// roadmap: `"kimi"` was this fixture until Kimi shipped, and the tests then
    /// asserted the opposite of what they meant.
    const FUTURE_AGENT: &str = "a-later-backend";

    /// The hand-written `Deserialize` must not have moved the names it replaced.
    #[test]
    fn known_agent_names_still_decode() {
        // Every backend, by the name it advertises — so a `visit_str` arm that
        // is dropped, misspelled or reordered fails here rather than turning a
        // live session's rows into `Unknown` on the next dashboard.
        for &agent in AgentKind::ALL {
            let name = agent.cli_subcommand();
            let decoded: AgentKind = serde_json::from_str(&format!("\"{name}\""))
                .unwrap_or_else(|e| panic!("{name} must decode: {e}"));
            assert_eq!(decoded, agent, "{name} decoded to the wrong backend");
            // …and round-trips, which is what keeps a state file written by one
            // build readable by the next.
            assert_eq!(
                serde_json::to_string(&agent).expect("encodes"),
                format!("\"{name}\""),
                "{agent:?} does not serialize as the name it decodes from"
            );
        }
        // The one that could have needed a `#[serde(rename)]` and doesn't: the
        // derived `rename_all` spelling of `OpenCode` is one lowercase word,
        // which is also how the project spells it. The loop above would catch a
        // divergence, but this names the reason nobody has to re-derive it.
        assert_eq!(AgentKind::OpenCode.cli_subcommand(), "opencode");
    }

    /// A backend added after this build was cut decodes to `Unknown` instead of
    /// erroring, and re-serializes to a value that decodes back to itself (the
    /// name is gone, but it never silently becomes Claude).
    #[test]
    fn an_unrecognized_agent_name_decodes_to_unknown() {
        let a: AgentKind =
            serde_json::from_str(&format!("\"{FUTURE_AGENT}\"")).expect("decodes, not errors");
        assert_eq!(a, AgentKind::Unknown);

        let encoded = serde_json::to_string(&a).expect("encodes");
        assert_eq!(encoded, r#""unknown""#);
        let round: AgentKind = serde_json::from_str(&encoded).expect("round-trips");
        assert_eq!(round, AgentKind::Unknown);
    }

    /// The regression this variant exists for: `ServerFrame::Snapshot` carries a
    /// `Vec<LauncherState>`, so one element the build can't decode used to fail
    /// the *whole* frame — an older dashboard talking to a host running one new
    /// backend lost **every** row on that host. Every session must survive.
    #[test]
    fn one_unknown_agent_does_not_blank_the_whole_snapshot() {
        let snapshot = format!(
            "[{},{},{}]",
            state_json("claude", 1),
            state_json(FUTURE_AGENT, 2),
            state_json("codex", 3),
        );
        let sessions: Vec<LauncherState> =
            serde_json::from_str(&snapshot).expect("an unknown agent must not fail the frame");
        assert_eq!(
            sessions.len(),
            3,
            "every session in the snapshot must survive one undecodable agent name"
        );
        assert_eq!(
            sessions.iter().map(|s| s.agent).collect::<Vec<_>>(),
            [AgentKind::Claude, AgentKind::Unknown, AgentKind::Codex]
        );
        // The rest of the unknown row is intact, so it still renders and can
        // still be killed.
        assert_eq!(sessions[1].launcher_pid, 2);
        assert_eq!(sessions[1].cwd, "/home/miao/p");
    }

    /// `Unknown` out of `ALL` is what keeps it out of the `Space a` picker and
    /// the workdir picker's `Ctrl-t` cycle — both derive their lists from this.
    #[test]
    fn all_excludes_unknown() {
        assert!(!AgentKind::ALL.contains(&AgentKind::Unknown));
        // Order seeds the `Ctrl-t` cycle and picker defaults, so this pins it:
        // a new backend appends, it never inserts.
        assert_eq!(
            AgentKind::ALL,
            &[
                AgentKind::Claude,
                AgentKind::Codex,
                AgentKind::Reasonix,
                AgentKind::Kimi,
                AgentKind::Grok,
                AgentKind::OpenCode,
                AgentKind::Pi,
                AgentKind::Antigravity,
                AgentKind::Omp
            ]
        );
    }

    /// A typo on the command line is a user error, not a compatibility gap:
    /// `from_cli` still refuses rather than handing back an inert backend.
    #[test]
    fn from_cli_rejects_an_unrecognized_name_instead_of_unknown() {
        assert_eq!(AgentKind::from_cli(FUTURE_AGENT), None);
        assert_eq!(AgentKind::from_cli(""), None);
        assert_eq!(AgentKind::from_cli("claude"), Some(AgentKind::Claude));
        assert_eq!(
            AgentKind::from_cli("Reasonix"),
            Some(AgentKind::Reasonix),
            "the CLI spelling is case-insensitive, unlike the on-disk one"
        );
    }

    /// Nothing may be launched from a backend we can't name — guessing an argv
    /// would run the wrong agent in the user's cwd.
    #[test]
    fn launching_an_unknown_agent_is_an_error() {
        let config = crate::config::AgentConfig {
            approval_grace: Duration::from_secs(2),
            codex: Default::default(),
        };
        let Err(err) = AgentKind::Unknown.configure(&config) else {
            panic!("an unknown backend has no session adapter");
        };
        assert!(
            err.to_string().contains("upgrade"),
            "the error must name the fix: {err}"
        );
    }

    /// Tolerance is for *names*, not for malformed JSON: a number in the field
    /// is a broken payload, and reading it as `Unknown` would hide that.
    #[test]
    fn a_non_string_agent_is_still_a_decode_error() {
        assert!(serde_json::from_str::<AgentKind>("7").is_err());
        assert!(serde_json::from_str::<AgentKind>("null").is_err());
        assert!(serde_json::from_str::<AgentKind>(r#"{"name":"claude"}"#).is_err());
    }

    /// The gate is nothing but "the two argvs differ", in both directions.
    /// Kimi is the first backend on the false side and the reason the gate
    /// exists: its resume argv ignores `fork`, so its capability must be false.
    #[test]
    fn a_backend_forks_exactly_when_its_fork_argv_differs() {
        for &agent in AgentKind::ALL {
            let plain = agent.resume_args("id", false);
            let forked = agent.resume_args("id", true);
            assert_eq!(
                agent.capabilities().fork,
                plain != forked,
                "{agent:?}'s gate must be its argv, nothing else"
            );
        }
        // Named rather than only derived, so a backend quietly losing (or
        // growing) its fork flag fails here instead of just hiding a key.
        assert!(AgentKind::Claude.capabilities().fork);
        assert!(AgentKind::Codex.capabilities().fork);
        assert!(AgentKind::Reasonix.capabilities().fork);
        assert!(
            AgentKind::Pi.capabilities().fork,
            "Pi forks by swapping --session for --fork, which is still a differing argv"
        );
        assert!(
            !AgentKind::Kimi.capabilities().fork,
            "Kimi has no fork flag, so `f` must hide rather than resume in place"
        );
        assert!(
            AgentKind::OpenCode.capabilities().fork,
            "opencode has --fork"
        );
        assert!(
            AgentKind::Omp.capabilities().fork,
            "omp forks with --fork where it resumes with --resume"
        );

        // A backend this build can't drive contributes no argv either way, so
        // the two are equal and it reports no fork — rather than offering `f`
        // and resuming in place.
        assert_eq!(
            AgentKind::Unknown.resume_args("id", true),
            AgentKind::Unknown.resume_args("id", false)
        );
        assert!(!AgentKind::Unknown.capabilities().fork);
    }

    /// The whole subscription surface, backend by backend, read back out of the
    /// configs themselves. This is the test that makes `forwarded_events`
    /// trustworthy enough to derive a capability from: if the token scan ever
    /// stops matching — a backend rewrites its config in a shape the boundary
    /// rule doesn't cover, say — every set here collapses at once rather than
    /// one capability quietly turning false.
    #[test]
    fn every_backend_forwards_exactly_the_events_its_config_registers() {
        use HookEvent::*;
        let expected: &[(AgentKind, &[HookEvent])] = &[
            (
                AgentKind::Claude,
                &[
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
                    ModelSwitch,
                ],
            ),
            // Eight, not the twelve `HookEvent`s that appear in `agents::codex`:
            // `Elicitation`, `ElicitationResult`, `StopFailure` and `CwdChanged`
            // are *produced* there — refined out of a `PreToolUse` payload, or
            // synthesized — rather than registered with Codex. Reading the
            // config is what tells the two apart.
            (
                AgentKind::Codex,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PermissionRequest,
                    Stop,
                    PreCompact,
                    PostCompact,
                ],
            ),
            // No `PostCompact`: Reasonix has no such hook, which is why
            // `Compacting` is left by the next event of any kind.
            (
                AgentKind::Reasonix,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PostToolUseFailure,
                    PermissionRequest,
                    Stop,
                    StopFailure,
                    PreCompact,
                ],
            ),
            (
                AgentKind::Kimi,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PostToolUseFailure,
                    PermissionRequest,
                    ElicitationResult,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                ],
            ),
            // No `Elicitation`: opencode's `permission.updated` is the gate and
            // `permission.replied` releases it, so the approval pair carries
            // what a decision prompt would.
            // `PermissionRequest` is the lifecycle `Notification` /
            // `permission_prompt` matcher. `StopCancelled` forwards as `Stop`,
            // so it does not appear as its own variant.
            (
                AgentKind::Grok,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PostToolUseFailure,
                    PermissionRequest,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                ],
            ),
            (
                AgentKind::OpenCode,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PermissionRequest,
                    ElicitationResult,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                    CwdChanged,
                ],
            ),
            // The shortest list, and the one the capability matrix turns on:
            // **no `PermissionRequest` and no `Elicitation`**, because Pi has no
            // per-tool gate to subscribe to at all.
            (
                AgentKind::Pi,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                    ModelSwitch,
                ],
            ),
            // Lifecycle hooks plus the status-line observer. Failures are
            // synthesized from each lifecycle payload's error field.
            (
                AgentKind::Antigravity,
                &[SessionStart, PromptSubmit, PostToolUse, Stop, StatusUpdate],
            ),
            // No `Elicitation`: omp's `tool_approval_requested` is the gate and
            // `tool_approval_resolved` releases it, so the approval pair carries
            // what a separate decision prompt would. `PostToolUseFailure` is
            // absent (derived in Rust from `is_error`, never registered) — the
            // same shape opencode already has.
            (
                AgentKind::Omp,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PermissionRequest,
                    ElicitationResult,
                    Stop,
                    PreCompact,
                    PostCompact,
                ],
            ),
        ];
        assert_eq!(
            expected.len(),
            AgentKind::ALL.len(),
            "a backend without a row here would be pinned by nothing at all"
        );
        for (agent, events) in expected {
            assert_eq!(
                &agent.forwarded_events(),
                events,
                "{agent:?}'s generated hook config no longer registers what this pins"
            );
        }
        // The read-side-only variant writes no config, so it subscribes to
        // nothing — which is also what makes every one of its capabilities
        // false without a special case.
        assert!(AgentKind::Unknown.forwarded_events().is_empty());
    }

    /// The three prefix pairs that make the boundary rule load-bearing. Without
    /// it, every backend that registers `stop-failure` would also report a
    /// `Stop` it may not have — and Pi, which registers neither
    /// `post-tool-use-failure` nor `elicitation-result`, is the backend whose
    /// set would change shape.
    #[test]
    fn an_event_name_that_prefixes_another_is_not_read_as_that_other() {
        assert!(mentions_event(
            r#"{"c":"miao hook stop-failure"}"#,
            "stop-failure"
        ));
        assert!(!mentions_event(r#"{"c":"miao hook stop-failure"}"#, "stop"));
        assert!(!mentions_event(
            r#"{"c":"miao hook post-tool-use-failure"}"#,
            "post-tool-use"
        ));
        assert!(!mentions_event(
            r#"{"c":"miao hook elicitation-result"}"#,
            "elicitation"
        ));
        // A path that happens to spell an event is not a subscription: nothing
        // in the config puts a `"` straight after it.
        assert!(!mentions_event(
            r#"{"c":"/opt/stop/miao hook stop-failure"}"#,
            "stop"
        ));
        // A command that keeps going after the event name is still a
        // subscription — Antigravity's commands end in a stdout contract, and
        // the prefix guard has to survive that too.
        assert!(mentions_event(
            r#"{"c":"miao hook stop >/dev/null; echo {}"}"#,
            "stop"
        ));
        assert!(!mentions_event(
            r#"{"c":"miao hook stop-failure >/dev/null; echo {}"}"#,
            "stop"
        ));
    }

    /// Only Grok answers the alt-screen question with anything but `false`.
    /// Every other backend is either verified inline (Claude, Codex) or
    /// unverified — and unverified must stay `false`: a missed prime keeps the
    /// reattached window exactly as it is today, while a wrong prime strands an
    /// inline TUI in a cleared alternate screen. Grok's own resolution is
    /// pinned in its module (`alt_screen_resolution_follows_groks_config`);
    /// this guards the *default* for everyone else.
    #[test]
    fn no_backend_claims_the_alt_screen_without_verification() {
        for &agent in AgentKind::ALL {
            if agent == AgentKind::Grok {
                continue;
            }
            assert!(
                !agent.uses_alt_screen(&[]),
                "{agent:?} claims the alt screen; verify it on a pool pty and \
                 move it out of the unverified arm deliberately"
            );
            // Same guard for the keyboard push, and for the same reason —
            // even under the TERM that makes Grok push.
            if agent != AgentKind::Codex {
                assert!(
                    !agent.uses_kitty_keyboard("xterm-kitty", true),
                    "{agent:?} claims the kitty keyboard push; probe its startup \
                     bytes and move it out of the unverified arm deliberately"
                );
            }
        }
        assert!(!AgentKind::Unknown.uses_alt_screen(&[]));
        assert!(!AgentKind::Unknown.uses_kitty_keyboard("xterm-kitty", true));
        assert!(!AgentKind::Grok.uses_kitty_keyboard("xterm-kitty", false));
    }

    /// **The test the declared matrix is worth having.** `capabilities()` is a
    /// table of constants so the draw loop can read it for free; this runs the
    /// derivation it skipped and demands the two agree, for every backend and
    /// every field. Three of the four are then unfalsifiable by hand: a
    /// capability can only change by changing the argv, the worktree flag or the
    /// hook config that produced it.
    ///
    /// `context_tokens` is the one field with nothing to derive from — no method
    /// answers "could this backend ever report a total" without a live session —
    /// so it is pinned by the matrix test below and by the module docs that
    /// state, per backend, why the agent has no number to give.
    #[test]
    fn the_capability_matrix_agrees_with_the_implementations() {
        use crate::state::SessionStatus;

        for &agent in AgentKind::ALL {
            let c = agent.capabilities();
            assert_eq!(
                c.fork,
                agent.resume_args("id", true) != agent.resume_args("id", false),
                "{agent:?}: the declared fork capability is not what its resume argv does"
            );
            assert_eq!(
                c.worktrees,
                agent.worktree_args(None).is_some(),
                "{agent:?}: the declared worktree capability is not what its worktree argv does"
            );
            let forwarded = agent.forwarded_events();
            // Snapshot feeds may carry approval without a dedicated hook.
            // Exercise the actual parser/dispatcher as well as requiring the
            // subscription, so the declaration cannot outlive its wiring.
            let snapshot_gate = forwarded.contains(&HookEvent::StatusUpdate) && {
                let mut state = LauncherState::for_test(agent, SessionStatus::Active);
                let msg = agent
                    .parse_hook_payload(
                        HookEvent::StatusUpdate,
                        r#"{"agent_state":"tool_use","tool_confirmation_pending":true}"#,
                    )
                    .unwrap();
                agent.hook_session().dispatch_hook(&mut state, msg);
                state.status == SessionStatus::WaitingForApproval
            };
            let gates = forwarded.contains(&HookEvent::PermissionRequest)
                || forwarded.contains(&HookEvent::Elicitation)
                || snapshot_gate;
            assert_eq!(
                c.approval_gate, gates,
                "{agent:?}: the declared approval gate is not what its hook config registers \
                 (forwarded: {forwarded:?})"
            );
        }
        // Same three checks for the variant that is not in `ALL`, since it is
        // the one whose row is all-false by policy rather than by behaviour.
        let c = AgentKind::Unknown.capabilities();
        assert!(!c.fork && !c.worktrees && !c.approval_gate && !c.context_tokens);
    }

    /// The matrix itself, written out — what the README's per-agent notes
    /// promise, in one reviewable block. The test above proves each row is
    /// *consistent*; this one is where a deliberate change has to be seen and
    /// agreed to.
    #[test]
    fn the_capability_matrix_is_what_the_readme_promises() {
        let row = |a: AgentKind| {
            let c = a.capabilities();
            (c.fork, c.worktrees, c.approval_gate, c.context_tokens)
        };
        //                                       fork   tree   appr   ctx
        let promised: &[(AgentKind, _)] = &[
            (AgentKind::Claude, (true, true, true, true)),
            (AgentKind::Codex, (true, false, true, true)),
            (AgentKind::Reasonix, (true, false, true, false)),
            (AgentKind::Kimi, (false, false, true, true)),
            (AgentKind::Grok, (true, true, true, true)),
            (AgentKind::OpenCode, (true, false, true, true)),
            (AgentKind::Pi, (true, false, false, true)),
            (AgentKind::Antigravity, (false, false, true, true)),
            (AgentKind::Omp, (true, false, true, true)),
        ];
        assert_eq!(
            promised.len(),
            AgentKind::ALL.len(),
            "a backend missing here is a backend the README never promised anything about"
        );
        for &(agent, want) in promised {
            assert_eq!(row(agent), want, "{agent:?}");
        }
    }

    /// Grok rewrites `signals.json` independently of `summary.json` (also a
    /// replace). Every other backend's tokens ride a hook or the watched
    /// transcript itself, so they must not grow a sidecar watch — that watch
    /// is how we'd accidentally sit on Grok's session dir.
    #[test]
    fn grok_replaced_sidecars_are_signals_json_beside_summary() {
        let summary = PathBuf::from("/tmp/sess/summary.json");
        assert_eq!(
            AgentKind::Grok.replaced_sidecar_paths(&summary),
            vec![PathBuf::from("/tmp/sess/signals.json")]
        );
        for &agent in AgentKind::ALL {
            if agent == AgentKind::Grok {
                continue;
            }
            assert!(
                agent.replaced_sidecar_paths(&summary).is_empty(),
                "{agent:?} must not watch Grok sidecars"
            );
        }
    }
}
