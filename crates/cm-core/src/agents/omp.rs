//! Oh My Pi (`omp`) CLI backend. Owns every omp-specific path, argv flag and
//! hook payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Omp`'s match arms.
//!
//! **Source-verified against the installed binary**, `omp` v18.4.8 at
//! `/opt/homebrew/bin/omp` — a Bun-compiled single executable whose JS source
//! and Markdown docs are embedded and readable (`strings -a` on the binary;
//! `omp read omp://<doc>`, e.g. `omp://extensions.md` for
//! `getAsyncJobSnapshot()`). `pi` 0.84.2 is separately installed at
//! `/opt/homebrew/bin/pi`; the two are distinct binaries with distinct state
//! dirs (`~/.omp` vs `~/.pi`), so nothing about the pi backend changes. This
//! module is built on the same `-e <extension.ts>` injection pi uses — **not**
//! a parameterization of `pi.rs`: omp's event surface has diverged from pi's
//! in five load-bearing ways (below), so the two modules share only what
//! already lives in `agents::common`.
//!
//! omp is, like pi, **neither a synthetic home nor a shell hook**, and for
//! pi's reason: `-e` is per-run, torn down with the process, nothing of the
//! user's touched. See `agents::pi`'s module doc for the trust argument — a
//! `-e` extension is trusted by virtue of being on the command line, so there
//! is no seeding step. omp needs no [`super::synth_home::SynthHome`] at all.
//!
//! # The four divergences from pi
//!
//! ## 1. `agent_end` + `willContinue`, not `agent_settled`
//!
//! `agent_settled` does not exist in omp (`strings -a omp | grep -cF
//! agent_settled` → 0). pi's whole turn-end design rested on it. omp's
//! replacement is **`agent_end` + `willContinue`**: the emitter is
//! `await this.#d?.emit({ type: "agent_end", messages: w, willContinue:
//! u?.willContinue })`, and omp's own wire encoder uses exactly
//! `w.type === "agent_end" && w.willContinue !== true` as its terminal test,
//! while the internal notifier names the same value `isTerminal:
//! !b?.willContinue`. Every auto-retry, auto-compaction, queued follow-up and
//! `session_stop` continuation branch calls `n({ willContinue: true })`; only
//! the genuinely-final paths call `n()` with no argument. It also fires on the
//! abort path (`if (o.stopReason === "aborted") { … await n(h ?
//! { willContinue: true } : undefined); }`), so an interrupted turn settles —
//! the gap that costs Claude a session-file fold.
//!
//! So `agent_end` maps to [`HookEvent::Stop`] and **nothing else does**, and a
//! `Stop` whose payload carries `will_continue: true` is renamed in
//! [`normalize_event`] to [`HookEvent::PostToolUse`] — the arm that means
//! exactly "a unit of work ended, the session is still `Active`, nothing is in
//! flight" — rather than settling the row while omp keeps working. Unlike
//! pi's `is_error` correction this one is **not cosmetic**; a wrong answer
//! here is a wrong row.
//!
//! ## 2. `session_stop` exists but must not be used
//!
//! It is guarded by `if (this.#c0 === "sub" || !this.#d?.hasHandlers
//! ("session_stop"))` and by two abort flags before it emits, so it is skipped
//! on interrupt; it fires *before* the final `agent_end`; and its return value
//! (`{ continue: true, additionalContext }`) can extend the user's turn.
//! Subscribing to it would settle a row that is about to continue, and miss
//! every interrupt. It is in the "deliberately not registered" list below.
//!
//! ## 3. A per-tool approval gate, which pi has none of
//!
//! `tool_approval_requested` fires before the approval wait with
//! `{ sessionId, toolName, toolCallId, reason?, approvalMode }`, and
//! `tool_approval_resolved` fires "after approve, deny, or approval prompt
//! failure". Both are emitted whenever a handler is registered
//! (`const K = this.runner.hasHandlers("tool_approval_requested") ||
//! this.runner.hasHandlers("tool_approval_resolved")`). So `Omp` gets
//! `approval_gate: true` — pi's `false` is the one capability row that
//! inverts. The pair maps to [`HookEvent::PermissionRequest`] and
//! [`HookEvent::ElicitationResult`], the same shape opencode already has, and
//! neither return value can alter the turn.
//!
//! ## 4. `session_switch`, not `session_info_changed`
//!
//! `session_info_changed` does not exist in omp (grep → 0). Its role (deliver
//! a fact with no status move) is taken by **`session_switch`**, which omp
//! emits on `/new`, `/fork` and `/resume` and which is where a mid-session
//! session-id change becomes visible to `r`/`f`. It maps to
//! [`HookEvent::SessionStart`], the one arm in [`common::dispatch_default`]
//! that touches status only to settle a row out of `Starting` — so it adopts
//! the new session id and moves nothing, which is precisely what is wanted.
//!
//! ## 5. `ctx.getAsyncJobSnapshot()` is the only enumerator of live async work
//!
//! omp tracks async jobs **in-process**: `getAsyncJobSnapshot()` returns
//! `{ running, recent, delivery }`, and `running` is already filtered to the
//! live set, so the extension reads it and rides it on the `agent_end` payload
//! as `async_jobs` (beside `will_continue`). `omp ps` **is** machine-readable
//! (`--json`), but it enumerates a different tier: per-project *named services*,
//! read from a runtime directory of `meta.json` files or from the live broker,
//! whose only session linkage is one `owner` field (a session id, or an
//! agent-registry id such as `"Main"`). Async jobs never appear there, and
//! never touch the disk — so no tree walk and no file read could find them, and
//! a terminal `Stop` that still has bash jobs or `task` spawns running has no
//! other witness. The payload list is the source. The `Stop` arm turns it into
//! a tier — `Task` / `Server` / `Review` — instead of `Idle`.
//!
//! An `agent_end` with `willContinue` (omp's `awaitingAsyncWork`) is *not* this
//! case: it is already remapped to `PostToolUse` (divergence 1), so the tier is
//! decided only at the genuine terminal `Stop` — the one that can arrive with
//! work still running. And because a `bash` job's label *is* its command text,
//! an `r3 watch` run as a bash job still reads `Review` through the shared
//! text classifier, exactly as Grok's payload list does.
//!
//! # Unchanged from pi
//!
//! `-e/--extension <path>` (repeatable), `before_agent_start` carrying
//! `prompt`, `tool_execution_start`/`tool_execution_end` carrying
//! `toolName`/`isError`, `session_before_compact`/`session_compact` (emitted
//! on both manual *and* automatic compaction paths), `session_start`, and the
//! whole `ExtensionAPI` surface the payload reads — `pi.on`,
//! `pi.getSessionName()`, `ctx.sessionManager.getSessionId()`, `ctx.cwd`,
//! `ctx.getContextUsage()`, `ctx.model`. The default export still takes the
//! API object named `pi` in omp's own docs, so the generated extension keeps
//! that parameter name.
//!
//! # Resume and fork
//!
//! omp's argv handler map registers `"--resume": R2w`, `"-r": R2w`,
//! `"--session": R2w` (an undocumented alias) and `"--fork": (w, u) => { w.fork
//! = u; }`. [`AgentControl::resume_args`] uses `--resume` / `--fork` —
//! `--resume` is in the public flag schema and `--help`, `--fork` is in the
//! argv map and documented in `omp://session-operations-export-share-fork-
//! resume.md`. `--session` is deliberately not used: it is absent from both
//! `--help` and the flag schema.
//!
//! No `--worktree` launch flag exists (the single `--worktree` string in the
//! binary is `git restore --worktree`). omp's `omp worktree` subcommand only
//! lists/clears worktrees the agent made itself, so there is nothing to pass
//! at launch and [`AgentControl::worktree_args`] answers `None`.
//!
//! # Loaded with Bun, not jiti
//!
//! omp loads this extension with **Bun**, not pi's jiti — omp's
//! `extension-loading.md` accepts explicit `.ts`/`.js`/`.mjs`/`.cjs` — so the
//! `.ts` suffix is no longer a loader requirement. It is kept anyway for the
//! reason [`Extension::path`]'s doc gives: the launcher's cleanup and
//! dead-launcher sweep key on `.sock` / `-settings.json` by name, so the
//! relocated copy must live under a name that never accumulates. The file sits
//! in `~/.local/state/captain-miao/`, which is none of omp's auto-discovery
//! roots (`<cwd>/.omp/extensions`, `~/.omp/agent/extensions`, plugin
//! manifests), so it is loaded exactly once, by our `-e`.
//!
//! # Known ordering caveat
//!
//! omp emits `agent_end` fire-and-forget (`this.#t1([...W],
//! b).catch(…)`, not awaited), unlike `session_stop` which it awaits. So a
//! `Stop` is not ordered against a following `PromptSubmit` the way pi's
//! awaited events were. It needs a user to submit a new prompt inside the
//! ~15ms a `miao hook` spawn takes, so it is a real but unreachable race;
//! naming it here stops the next reader re-deriving it.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::claude;
use super::common;
use super::pi_extension::Extension;
use crate::agent::{BgSeedKind, BgShell, ResumeCandidate};
use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "omp";

/// omp's user-question tool — the `AskUserQuestion` analog. Verified in the
/// installed binary: `packages/coding-agent/src/tools/ask.ts` defines
/// `AskTool` with `name = "ask"` and `concurrency = "exclusive"`, so its
/// `tool_execution_start` arrives as [`HookEvent::PreToolUse`] carrying
/// `tool_name = "ask"`. That `"exclusive"` concurrency makes it a barrier in
/// omp's tool loop (`Tgn`'s `V`/`ee` promise chains): every prior tool settles
/// before `ask` starts and every later one waits on it, so no other tool can
/// run while the question is up — this arm needs no Grok-style hold against a
/// concurrent tool's `PostToolUse`.
const ASK_TOOL: &str = "ask";

// =============================================================================
// The event table — the module's claim about what it subscribes to
// =============================================================================

/// Every omp event the generated extension subscribes to, paired with the
/// [`HookEvent`] it is forwarded as. This const **is** the claim; the emitted
/// JavaScript renders it, so the two cannot disagree.
///
/// The four choices in here that are omp's rather than pi's:
///
/// - **`agent_end`, not `agent_settled` (gone) and not `session_stop`
///   (skipped on abort, fires early, can extend the turn).** `agent_end` is
///   omp's turn-end signal, and a `will_continue: true` on it is corrected in
///   [`normalize_event`] to [`HookEvent::PostToolUse`] rather than settling
///   the row — see the module doc for why that correction is load-bearing.
/// - **`session_switch`, not `session_info_changed` (gone).** omp emits it on
///   `/new`, `/fork` and `/resume`, and it is where a mid-session session-id
///   change becomes visible to `r`/`f`.
/// - **`tool_approval_requested` / `tool_approval_resolved` — omp's per-tool
///   approval gate, which pi has none of.** Both fire whenever a handler is
///   registered, and neither return value can alter the turn, so the pair is
///   what makes `approval_gate: true` honest.
/// - **`tool_execution_start` / `tool_execution_end`, not `tool_call` /
///   `tool_result`** for pi's reason: the pair whose return value cannot block
///   or rewrite a tool, and `tool_execution_end` carries `isError` besides.
///
/// No [`HookEvent::StopFailure`], for pi's reason: omp's surface has no
/// run-failed event, so `agent_end` covers it and the row goes `Idle` with no
/// error text.
///
/// Deliberately not registered: `agent_start` / `turn_*` / `message_*` /
/// `context` / `before_provider_*` / `after_provider_response` (too
/// fine-grained), `session_shutdown` (the launcher owns exit),
/// `session_before_switch` / `session_before_fork` / `session_before_tree` /
/// `session_branch` / `session_tree` (a replacement session re-emits
/// `session_start`, which we do register), `model_select` and
/// `thinking_level_select` (the model rides every payload already),
/// `project_trust` / `resources_discover` (never invoked by any session
/// callsite), `user_bash` / `user_python`, `todo_reminder`,
/// `ttsr_triggered`, `credential_disabled`, `mcp_notification`, and omp's
/// new fine-grained events `auto_compaction_*` and `auto_retry_*` (each is a
/// `willContinue: true` branch that `agent_end` already covers). And
/// **`session_stop`** — see the module doc for why it must not be subscribed
/// to despite existing.
const FORWARDED: &[(&str, HookEvent)] = &[
    ("session_start", HookEvent::SessionStart),
    ("session_switch", HookEvent::SessionStart),
    ("before_agent_start", HookEvent::PromptSubmit),
    ("tool_execution_start", HookEvent::PreToolUse),
    ("tool_execution_end", HookEvent::PostToolUse),
    ("tool_approval_requested", HookEvent::PermissionRequest),
    ("tool_approval_resolved", HookEvent::ElicitationResult),
    ("agent_end", HookEvent::Stop),
    ("session_before_compact", HookEvent::PreCompact),
    ("session_compact", HookEvent::PostCompact),
];

// =============================================================================
// The generated extension
// =============================================================================

// The shared transport owns installation and delivery; this adapter keeps its
// event vocabulary and payload differences beside its normalization rules.
const EXTENSION: Extension = Extension {
    agent: BIN,
    events: FORWARDED,
    extra_fields: &[
        ("will_continue", "event?.willContinue"),
        // The session's live async-job list, read from the extension context.
        // `running` is already the live-only set (see `shells_from_stop`).
        ("async_jobs", "ctx?.getAsyncJobSnapshot?.()?.running"),
    ],
};

/// The "hook settings" the launcher writes to its per-session file — for omp,
/// **TypeScript source**, not JSON. The path is generic transport and its
/// contents are opaque to the launcher, so each backend puts its own format
/// through it (Kimi already puts TOML through the same channel).
///
/// `sock_path` is ignored, as it is for pi and for the same reason: the file
/// is shared by every session, so it cannot carry a per-session path. The
/// socket reaches the hook through the environment instead, and that trip
/// needs no faith — our forwarder spawns the child itself with `spawn`'s
/// default inherited environment, from inside the omp process we set
/// `CAPTAIN_MIAO_SOCK` on.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    EXTENSION.source(&exe.to_string_lossy())
}

// =============================================================================
// Launcher: process spawn
// =============================================================================

/// Build the argv for an omp session. omp has no shell-command hook, so what
/// the launcher wrote is a generated TypeScript extension, relocated to a path
/// omp's loader will accept.
pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    EXTENSION.command(cwd, sock_path, settings_path, extra_args, shim_dir)
}

// =============================================================================
// Hook payload (stdin from the extension → normalized HookMessage)
// =============================================================================

/// The payload our own forwarder sends. Unlike every other backend's, this
/// struct describes a shape captain-miao **writes** rather than one an agent
/// happens to emit — `Extension::source` builds it and this parses it, so the
/// two are pinned together by the tests below rather than by a vendor's docs.
///
/// snake_case, matching the launcher's own wire vocabulary; the JavaScript
/// names these keys explicitly rather than dumping an omp event object, because
/// omp's event payloads carry `AbortSignal`s and message graphs that
/// `JSON.stringify` would either flatten to `{}` or refuse outright.
///
/// The omp-side sources, one per field: `ctx.sessionManager.getSessionId()`,
/// `pi.getSessionName()`, `ctx.cwd`, `event.toolName` (the tool-execution
/// events), `event.prompt` (`before_agent_start`), `event.isError`
/// (`tool_execution_end`), `event.willContinue` (`agent_end`),
/// `ctx.getAsyncJobSnapshot()` (the live async-job list),
/// `ctx.getContextUsage().tokens` and `ctx.model.id`.
#[derive(Deserialize)]
struct HookPayload {
    session_id: Option<String>,
    session_title: Option<String>,
    cwd: Option<String>,
    tool_name: Option<String>,
    prompt: Option<String>,
    context_tokens: Option<u64>,
    model: Option<String>,
    /// Set on a `tool_execution_end` whose tool failed.
    #[serde(default)]
    is_error: bool,
    /// Set on an `agent_end` that omp will follow with more work of its own.
    #[serde(default)]
    will_continue: bool,
    /// The session's running async jobs at a turn end, when the extension
    /// could read them — absent on an omp that predates `getAsyncJobSnapshot`.
    #[serde(default)]
    async_jobs: Option<Vec<AsyncJob>>,
}

/// One entry of the extension's `async_jobs` array — omp's
/// `AsyncJobSnapshot.running` item, of which only these two fields bear on the
/// tier decision. `label` is a bash job's own command text (omp truncates it
/// at 120 chars), and a description for the other kinds.
#[derive(Deserialize)]
struct AsyncJob {
    #[serde(rename = "type")]
    job_type: Option<String>,
    label: Option<String>,
}

/// Normalize one omp hook payload, as sent by the generated extension.
pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse omp hook JSON from stdin")?;
    Ok(HookMessage {
        event: normalize_event(event, &payload),
        session_id: payload.session_id,
        tool_name: payload.tool_name,
        // No error text is read. `tool_execution_end` carries a `result`, but
        // its shape is per-tool and the failure arm doesn't surface a message
        // anyway; `raw` holds the whole payload for anyone who needs it.
        message: None,
        cwd: payload.cwd,
        prompt: payload.prompt,
        // All three ride every payload — see the module doc for why they come
        // from the hook rather than from a transcript fold.
        session_title: payload.session_title,
        context_tokens: payload.context_tokens,
        model: payload.model,
        // The only omp event carrying a transcript path is `session_stop`'s
        // `session_file`, and we do not subscribe to it — so no path reaches
        // the launcher and its whole transcript pipeline (stats fold, signal
        // scan, stat poll) stays inert by construction, with tokens and model
        // coming off the hook as their single source.
        transcript_path: None,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

/// Two payload-driven event corrections, one cosmetic and one load-bearing.
///
/// The `is_error` arm is pi's, cosmetic today and kept anyway: a failed
/// `tool_execution_end` is spelled [`HookEvent::PostToolUseFailure`], and
/// `dispatch_default` settles the two identically so nothing on the row moves
/// differently — but the fact is on the payload and this is where the
/// correction belongs the day the two arms diverge.
///
/// The `will_continue` arm is the load-bearing one and the reason this module
/// exists separately from pi's. `agent_end` fires at the end of *every* agent
/// loop, and omp reports on the event whether it will keep running (auto-retry,
/// auto-compaction, a queued follow-up, a `session_stop` continuation). A
/// settle on one of those would read `Idle` while the agent works.
/// [`HookEvent::PostToolUse`] is the arm that means exactly "a unit of work
/// ended, the session is still `Active`, nothing is in flight" —
/// `dispatch_default` sets `status = Active; last_tool = None` — so the
/// correction is a rename in Rust rather than a conditional in the generated
/// JavaScript, which keeps "the extension carries no logic" true. A wrong
/// answer here is a wrong row.
fn normalize_event(event: HookEvent, payload: &HookPayload) -> HookEvent {
    match event {
        HookEvent::PostToolUse if payload.is_error => HookEvent::PostToolUseFailure,
        HookEvent::Stop if payload.will_continue => HookEvent::PostToolUse,
        other => other,
    }
}

// =============================================================================
// The Stop payload's async-job list
// =============================================================================

/// The session's running background work as the Stop payload named it, or
/// `None` when the payload named no list at all.
///
/// Mirror Grok's `shells_from_stop`: `Some` (even empty) is the turn end
/// reporting what is in flight, `None` is a payload with nothing to say —
/// either an omp that predates `getAsyncJobSnapshot`, or an extension context
/// no session owns. The distinction is load-bearing for the Stop arm below.
fn shells_from_stop(raw: Option<&str>) -> Option<Vec<BgShell>> {
    let payload: HookPayload = serde_json::from_str(raw?).ok()?;
    let jobs = payload.async_jobs?;
    Some(jobs.iter().map(shell_from_job).collect())
}

/// One async job as a shell. A `bash` job's label *is* its command, so the
/// shared command-form heuristic applies and an `r3 watch` still reads
/// `Review`; every other kind — `task`, `eval`, or a type omp adds later — is
/// busy work by construction. A job is never dropped for having an unfamiliar
/// type: dropping a running job is the silent failure this exists to prevent.
fn shell_from_job(job: &AsyncJob) -> BgShell {
    let label = job
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let kind = match job.job_type.as_deref() {
        Some("bash") => {
            let command = label.unwrap_or("bash");
            if claude::is_r3_watch_command(command) {
                BgSeedKind::ReviewWatch
            } else if claude::is_long_running_command(command) {
                BgSeedKind::LongRunning
            } else {
                BgSeedKind::Other
            }
        }
        _ => BgSeedKind::Other,
    };
    BgShell {
        key: label
            .or(job.job_type.as_deref())
            .unwrap_or("job")
            .to_string(),
        kind,
    }
}

/// Same precedence as the launcher's `classify_and_learn` (and Grok's `Stop`
/// arm): any finite job keeps the row busy (`Task`); else any parked server is
/// at-rest (`Server`); else every remaining job is an r3 review-watch (`Review`).
fn status_from_shells(shells: &[BgShell]) -> SessionStatus {
    if shells.iter().any(|s| s.kind == BgSeedKind::Other) {
        SessionStatus::BackgroundActive
    } else if shells.iter().any(|s| s.kind == BgSeedKind::LongRunning) {
        SessionStatus::BackgroundServer
    } else {
        SessionStatus::ReviewPending
    }
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// omp departs from [`common::dispatch_default`] in two places: the `Stop` arm
/// and [`ASK_TOOL`]. The native → normalized renaming is done in the generated
/// table ([`FORWARDED`]) rather than here, and the two payload-driven
/// corrections are done in [`normalize_event`]. `agent_end` with a falsy
/// `willContinue` means the shared `Stop` arm needs no help from a session file
/// or a rollout scan, and the abort path settles too.
///
/// The `Stop` arm is omp's background tier, the shape Grok's `Stop` arm takes.
/// A genuine terminal turn end can arrive with async jobs still running, and
/// [`shells_from_stop`] decides from the payload's live list whether the row is
/// `Task` / `Server` / `Review` rather than `Idle`. A `Stop` that names no list
/// (`None`) while the row already holds a background tier keeps that tier —
/// absent evidence is not evidence the work ended, and the previous `Stop`
/// already named it. An empty list is a turn end saying nothing is in flight.
///
/// The other departure is `ask` — a blocking multiple-choice card, not work.
/// Like Grok's `ask_user_question` and Reasonix's `ask` it is surfaced as
/// [`SessionStatus::WaitingForDecision`] ("Decision"), the same bucket as
/// Claude's `AskUserQuestion` and Codex's `request_user_input`; without an arm
/// the row sits at plain `Active` for as long as the card is up. Its
/// `PreToolUse` is the only signal — omp blocks on the answer — and a gated
/// `ask` that also fires `tool_approval_requested` lands here too, so it reads
/// as a question rather than a tool-approval gate. The question's own
/// `PostToolUse` settles the row back to `Active` through the shared mapping,
/// and [`ASK_TOOL`] explains why no hold is needed for another tool's events.
pub fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    // `ask` is a question, not a tool call — see the doc comment. Both the
    // tool-execution event and an approval gate on it read as a decision.
    if matches!(
        msg.event,
        HookEvent::PreToolUse | HookEvent::PermissionRequest
    ) && msg.tool_name.as_deref() == Some(ASK_TOOL)
    {
        common::adopt_session_facts(state, &mut msg);
        state.status = SessionStatus::WaitingForDecision;
        state.last_tool = msg.tool_name;
        return;
    }

    if msg.event == HookEvent::Stop {
        match shells_from_stop(msg.raw.as_deref()) {
            Some(shells) if !shells.is_empty() => {
                common::adopt_session_facts(state, &mut msg);
                state.last_tool = None;
                state.status = status_from_shells(&shells);
                return;
            }
            None if matches!(
                state.status,
                SessionStatus::BackgroundActive
                    | SessionStatus::BackgroundServer
                    | SessionStatus::ReviewPending
            ) =>
            {
                // No list at all is not evidence the live work ended — the
                // previous Stop already named it. Hold the background row
                // rather than flashing Idle.
                common::adopt_session_facts(state, &mut msg);
                return;
            }
            // An empty list is a turn end saying nothing is in flight; absent
            // on a non-background row is an ordinary settle.
            Some(_) | None => {}
        }
    }

    common::dispatch_default(state, msg)
}

// =============================================================================
// Resume candidates
// =============================================================================

/// The sessions directory: `$PI_CODING_AGENT_DIR/sessions` when omp's documented
/// override is set (omp calls it the "Session storage directory"; `--profile`
/// rewrites it too), else `~/.omp/agent/sessions`. `None` means there is nowhere
/// to look, which [`list_resumable`] answers as an empty list rather than an
/// error.
fn sessions_root() -> Option<PathBuf> {
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".omp/agent")))?;
    Some(agent_dir.join("sessions"))
}

/// Every omp session under [`sessions_root`], newest first.
///
/// A session is a top-level `*.jsonl` file directly inside a `<sanitized-cwd>/`
/// bucket. The same-stem **directory** beside each file is omp's artifact and
/// subagent store, not another session, so only files are walked. Everything a
/// [`ResumeCandidate`] needs sits in the file's first lines — the `session`
/// header's `id` and `cwd`, the pad-rewritten `title` record omp keeps at line
/// 1, and the first `message` whose role is `user` — so the `parentId` tree walk
/// the old comment feared is not needed. That walk belongs to
/// [`crate::agent::AgentControl::read_transcript_stats`], whose fold really does
/// need the active leaf; a head read does not.
///
/// **A missing store is an empty list, not an error.** `LocalBackend` asks every
/// backend in `AgentControl::ALL` whenever the picker opens and surfaces whatever
/// errors come back (see `opencode::list_resumable` for the same argument), so
/// refusing here would put an error in front of every user who never ran omp.
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    let Some(root) = sessions_root() else {
        return Ok(Vec::new());
    };
    Ok(list_resumable_in(&root, limit))
}

/// The scan itself, split from `$PI_CODING_AGENT_DIR` resolution so a test can
/// point it at a fixture tree without touching the environment.
fn list_resumable_in(root: &Path, limit: usize) -> Vec<ResumeCandidate> {
    if limit == 0 {
        return Vec::new();
    }
    let mut found = Vec::new();
    for cwd_dir in common::read_subdirs(root) {
        let Ok(entries) = std::fs::read_dir(&cwd_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // The same-stem directory beside a session file is not a session.
            if !path.is_file() {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            found.push((path, mtime));
        }
    }

    // Newest first, decided before any file is opened so a picker costs one
    // `read_dir` walk plus the head reads rather than a read per session that
    // ever existed. `usize::MAX` because the cap applies to *valid candidates*:
    // a headerless file must be skipped without spending one of the `limit`
    // slots.
    let mut out = Vec::new();
    for (path, mtime) in common::newest_first(found, usize::MAX) {
        if out.len() >= limit {
            break;
        }
        let Some(head) = read_session_head(&path) else {
            continue;
        };
        out.push(ResumeCandidate {
            agent: crate::agent::AgentControl::Omp,
            session_id: head.session_id,
            cwd: head.cwd,
            first_prompt: head.first_prompt,
            custom_title: head.title,
            // omp records no branch anywhere a candidate can read.
            git_branch: None,
            mtime,
        });
    }
    out
}

/// How far into a file [`read_session_head`] goes before giving up. The header
/// and first user message sit within the first handful of lines in practice;
/// the cap is for the pathological file — a session with no user message at all
/// — so opening the picker never reads a whole multi-megabyte transcript.
const MAX_HEAD_LINES: usize = 400;
const MAX_HEAD_BYTES: u64 = 512 * 1024;

/// The parts of a session's opening lines a [`ResumeCandidate`] is built from.
#[derive(Default)]
struct SessionHead {
    session_id: String,
    cwd: String,
    /// The `title` record's title (line 1, normally), else the session header's.
    title: Option<String>,
    first_prompt: Option<String>,
}

/// Read one session file's head, line by line, stopping at the first `user`
/// message (or the cap). Unknown and malformed lines are skipped rather than
/// abandoning the file: `thinking_level_change`, `model_usage`, `custom_message`
/// and friends come before the first prompt.
///
/// `None` when the file has no `session` header, or the header names an empty
/// `id`/`cwd` — the same policy as the other backends, since a candidate that
/// resumes into the wrong place is worse than no row.
fn read_session_head(path: &Path) -> Option<SessionHead> {
    use std::io::{BufRead, BufReader};

    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut head = SessionHead::default();
    let mut bytes: u64 = 0;
    let mut line = Vec::new();
    for _ in 0..MAX_HEAD_LINES {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        bytes += line.len() as u64;
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) {
            match value.get("type").and_then(|t| t.as_str()) {
                Some("title") => {
                    if head.title.is_none() {
                        head.title = title_field(&value);
                    }
                }
                Some("session") => {
                    head.session_id = string_field(&value, "id");
                    head.cwd = string_field(&value, "cwd");
                    if head.title.is_none() {
                        head.title = title_field(&value);
                    }
                }
                Some("message")
                    if value.pointer("/message/role").and_then(|r| r.as_str()) == Some("user") =>
                {
                    head.first_prompt = user_prompt(&value);
                    break;
                }
                _ => {}
            }
        }
        if bytes >= MAX_HEAD_BYTES {
            break;
        }
    }
    if head.session_id.trim().is_empty() || head.cwd.trim().is_empty() {
        return None;
    }
    Some(head)
}

/// A plain string field, or `""` when absent or not a string.
fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A non-blank `title` from a `title` record or a `session` header.
fn title_field(value: &serde_json::Value) -> Option<String> {
    value
        .get("title")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// The text of the first `user` message: every `text` part concatenated and
/// whitespace-collapsed the way the other backends collapse a prompt. `None`
/// for a user message that carries no text (an image-only turn).
fn user_prompt(value: &serde_json::Value) -> Option<String> {
    let text = match value.pointer("/message/content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let prompt = super::collapse_whitespace(&text);
    (!prompt.is_empty()).then_some(prompt)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::SessionStatus;

    /// A stand-in for the executable, independent of the test binary location.
    const EXE: &str = "/home/miao/.local/bin/miao";

    /// A payload in the shape our own forwarder builds. Hand-written from
    /// [`Extension::source`] rather than captured — but unlike the other
    /// backends' fixtures, the thing it mirrors is *our* code, so it can only
    /// drift by someone editing the template.
    fn payload(extra: &str) -> String {
        format!(
            r#"{{"session_id":"s1","session_title":"wire up the parser",
                "cwd":"/home/miao/p","context_tokens":48100,"model":"some-model-1"{extra}}}"#
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::Omp, status)
    }

    /// Drive one hook end to end — parse the extension's stdin JSON, then
    /// dispatch it — so the tests exercise the same path a live hook takes,
    /// including the event normalization that only happens in the parser.
    fn feed(state: &mut LauncherState, event: HookEvent, stdin: &str) {
        let msg = parse_hook_payload(event, stdin).expect("payload parses");
        dispatch_hook(state, msg);
    }

    /// The generated table must be the module's [`FORWARDED`] claim rendered,
    /// and every event name in it must be one the launcher can parse back — a
    /// `miao hook --agent omp <name>` the CLI rejects is a status silently
    /// lost.
    #[test]
    fn the_registered_events_are_exactly_what_the_module_claims() {
        // The native names, in the order omp sees them registered.
        assert_eq!(
            FORWARDED.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [
                "session_start",
                "session_switch",
                "before_agent_start",
                "tool_execution_start",
                "tool_execution_end",
                "tool_approval_requested",
                "tool_approval_resolved",
                "agent_end",
                "session_before_compact",
                "session_compact",
            ]
        );
        // **Only `agent_end` becomes `Stop`.** That is the whole turn-end
        // design: nothing else may claim the turn is over.
        assert_eq!(
            FORWARDED
                .iter()
                .filter(|(_, e)| *e == HookEvent::Stop)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>(),
            ["agent_end"]
        );

        let source = EXTENSION.source(EXE);
        for (native, forwarded) in FORWARDED {
            let row = format!("  [\"{native}\", \"{}\"],", forwarded.as_kebab());
            assert!(source.contains(&row), "missing {row} in the emitted source");
            // The argv spelling has to survive the round trip the hook CLI does.
            assert_eq!(
                HookEvent::from_kebab(forwarded.as_kebab()),
                Some(*forwarded)
            );
        }
        // Nothing else is registered: the table has exactly these rows, and the
        // single `pi.on` in the file is the loop over it.
        let rows = source
            .lines()
            .filter(|l| l.starts_with("  [\"") && l.ends_with("\"],"))
            .count();
        assert_eq!(rows, FORWARDED.len());
        assert_eq!(source.matches("pi.on(").count(), 1);
    }

    /// One file serves every session, so it must carry no per-session data. The
    /// socket is the thing that would be tempting to splice and must not be.
    #[test]
    fn the_source_carries_no_per_session_data() {
        let a = build_hooks_settings("/run/user/1000/captain-miao/launchers/1.sock");
        let b = build_hooks_settings("/run/user/1000/captain-miao/launchers/2.sock");
        assert_eq!(a, b, "the extension must not embed the per-session socket");
        assert!(!a.contains(".sock"));
        // …and it reads the socket from the environment instead, which is the
        // only reason the file can be shared at all.
        assert!(a.contains("$CAPTAIN_MIAO_SOCK"));
    }

    /// The one spliced value. It is JSON-encoded because a JSON string literal
    /// is a JavaScript one — and it never reaches a shell, so this is the whole
    /// of the quoting story.
    #[test]
    fn the_exe_path_is_spliced_as_a_javascript_string_literal() {
        let source = EXTENSION.source(r#"/home/miao/od"d\path/miao"#);
        assert!(
            source.contains(r#"const MIAO = "/home/miao/od\"d\\path/miao";"#),
            "{source}"
        );
        // The argv is built as an array and handed to `spawn` directly, so
        // nothing is ever concatenated into a command line.
        assert!(!source.contains("shell: true"));
        assert!(source.contains(r#"spawn(MIAO, ["hook", "--agent", "omp", forwarded]"#));
    }

    /// A turn runs prompt → tool → settle, and a final `agent_end` is what ends
    /// it.
    #[test]
    fn a_turn_runs_from_prompt_to_settled() {
        let mut state = state_at(SessionStatus::Idle);
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload(r#","prompt":"go""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_prompt.as_deref(), Some("go"));
        // The session id rides every payload, so the launcher learns it here
        // rather than from a session file.
        assert_eq!(state.session_id.as_deref(), Some("s1"));

        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload(r#","tool_name":"bash""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool.as_deref(), Some("bash"));

        feed(&mut state, HookEvent::PostToolUse, &payload(""));
        assert_eq!(state.last_tool, None);

        feed(&mut state, HookEvent::Stop, &payload(""));
        assert_eq!(state.status, SessionStatus::Idle);
    }

    /// Title, tokens and model ride *every* payload, which is what replaces a
    /// transcript fold, a title store and a sqlite overlay all at once.
    #[test]
    fn every_payload_carries_the_title_tokens_and_model() {
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::PreToolUse, &payload(""));
        assert_eq!(state.name.as_deref(), Some("wire up the parser"));
        assert_eq!(state.context_tokens, Some(48_100));
        assert_eq!(state.model.as_deref(), Some("some-model-1"));

        // A rename is just a later payload carrying a different title.
        let renamed = r#"{"session_id":"s1","session_title":"renamed by the user"}"#;
        feed(&mut state, HookEvent::Stop, renamed);
        assert_eq!(state.name.as_deref(), Some("renamed by the user"));
        // …and that payload said nothing about tokens, which must not blank them.
        assert_eq!(state.context_tokens, Some(48_100));
    }

    /// `isError` on a `tool_execution_end` is one payload-driven event
    /// correction, and the empty-payload fallback the forwarder falls back to
    /// must still parse.
    #[test]
    fn a_failed_tool_is_normalized_to_the_failure_event() {
        let failed = parse_hook_payload(
            HookEvent::PostToolUse,
            &payload(r#","tool_name":"bash","is_error":true"#),
        )
        .expect("parses");
        assert_eq!(failed.event, HookEvent::PostToolUseFailure);

        // Without the flag it stays the plain event…
        let ok = parse_hook_payload(HookEvent::PostToolUse, &payload(r#","tool_name":"bash""#))
            .expect("parses");
        assert_eq!(ok.event, HookEvent::PostToolUse);
        // …and the correction is confined to that one event.
        let stopped =
            parse_hook_payload(HookEvent::Stop, &payload(r#","is_error":true"#)).expect("parses");
        assert_eq!(stopped.event, HookEvent::Stop);

        // The forwarder sends `{}` when building the payload throws, so the
        // status still lands even with nothing else to say.
        let bare = parse_hook_payload(HookEvent::Stop, "{}").expect("an empty payload parses");
        assert_eq!(bare.event, HookEvent::Stop);
        assert_eq!(bare.session_id, None);

        // The two shapes a token count actually arrives in. `null` is what
        // `Math.round(undefined)` serializes to when omp reports no usage yet,
        // and it must read as "not reported" rather than failing the payload —
        // which would take the *status* down with it, not just the number.
        let unusable = parse_hook_payload(HookEvent::Stop, r#"{"context_tokens":null}"#)
            .expect("a null token count parses");
        assert_eq!(unusable.context_tokens, None);
    }

    /// No transcript path, ever — the field the launcher gates its whole
    /// transcript watch on. The only omp event carrying one is `session_stop`,
    /// which we do not subscribe to, so no path reaches the launcher and the
    /// hook stays the single source for tokens and model.
    #[test]
    fn no_payload_ever_names_a_transcript() {
        for stdin in [
            payload(""),
            payload(r#","transcript_path":"/home/miao/s.jsonl""#),
        ] {
            let msg = parse_hook_payload(HookEvent::Stop, &stdin).expect("parses");
            assert_eq!(msg.transcript_path, None);
        }
    }

    /// **The one that pins divergence 1.** An `agent_end` with
    /// `will_continue: true` is omp saying "I will keep running", so it must
    /// not settle the row. It normalizes to `PostToolUse` — the arm that means
    /// "work ended, still `Active`, nothing in flight" — and the correction is
    /// confined to that event.
    #[test]
    fn a_continuing_agent_end_is_not_a_settle() {
        // `will_continue: true` → the event is renamed to `PostToolUse`.
        let continuing =
            parse_hook_payload(HookEvent::Stop, r#"{"will_continue":true}"#).expect("parses");
        assert_eq!(continuing.event, HookEvent::PostToolUse);

        // Without the flag it stays `Stop` — the genuinely-final turn end.
        let final_end =
            parse_hook_payload(HookEvent::Stop, r#"{"will_continue":false}"#).expect("parses");
        assert_eq!(final_end.event, HookEvent::Stop);

        // The correction is confined to `Stop`: a `PostToolUse` carrying the
        // flag is not rewritten (it is already the arm the correction targets).
        let tool = parse_hook_payload(HookEvent::PostToolUse, r#"{"will_continue":true}"#)
            .expect("parses");
        assert_eq!(tool.event, HookEvent::PostToolUse);

        // Driven through `dispatch_hook` from `Active`: the continuing one
        // leaves the row `Active`, the final one lands `Idle`.
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, r#"{"will_continue":true}"#);
        assert_eq!(
            state.status,
            SessionStatus::Active,
            "a continuing agent_end must not settle the row"
        );

        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, r#"{"will_continue":false}"#);
        assert_eq!(
            state.status,
            SessionStatus::Idle,
            "a final agent_end settles the row"
        );
    }

    /// The extension reads omp's live async-job list from the context — the
    /// only enumerator (divergence 5) — as a plain `ctx` expression, so no
    /// per-session data is spliced into the shared file.
    #[test]
    fn the_source_reads_the_async_jobs_from_the_context() {
        let source = EXTENSION.source(EXE);
        assert!(
            source.contains("async_jobs: ctx?.getAsyncJobSnapshot?.()?.running,"),
            "{source}"
        );
        // Same file for every session, so this too is context, not spliced data
        // — `the_source_carries_no_per_session_data` pins that separately.
    }

    /// The Stop list's classification: a bash job's label *is* its command, so
    /// the shared text heuristic decides the tier; every other kind is busy
    /// work; and a job is never dropped for a missing label or unknown type —
    /// dropping a running job is the silent failure this exists to prevent.
    #[test]
    fn shells_from_stop_classifies_each_job_and_never_drops_one() {
        let raw = r#"{"async_jobs":[
            {"type":"bash","label":"r3 watch review_abc123"},
            {"type":"bash","label":"npm run dev"},
            {"type":"bash","label":"cargo build"},
            {"type":"task","label":"refactor the parser"},
            {"type":"eval","label":""},
            {"type":"future-kind","label":"something new"},
            {"type":"bash"}
        ]}"#;
        let shells = shells_from_stop(Some(raw)).expect("the key is present");
        let kinds: Vec<BgSeedKind> = shells.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                BgSeedKind::ReviewWatch,
                BgSeedKind::LongRunning,
                BgSeedKind::Other,
                BgSeedKind::Other,
                BgSeedKind::Other,
                BgSeedKind::Other,
                BgSeedKind::Other,
            ]
        );
        // A bash job's key is its command text; a label-less bash still yields a
        // non-empty key so the job is named rather than dropped.
        assert_eq!(shells[0].key, "r3 watch review_abc123");
        assert_eq!(shells[2].key, "cargo build");
        assert_eq!(shells[4].key, "eval");
        assert_eq!(shells[5].key, "something new");
        assert_eq!(shells[6].key, "bash");
    }

    /// `Some` even when empty is the turn end reporting nothing in flight;
    /// `None` is a payload that named no list (an older omp, or an extension
    /// context no session owns) — the distinction the Stop arm leans on.
    #[test]
    fn shells_from_stop_distinguishes_an_empty_list_from_no_list() {
        assert_eq!(shells_from_stop(Some(r#"{"async_jobs":[]}"#)), Some(vec![]));
        // An absent key, an absent payload and an unparseable body are all
        // "nothing to say".
        assert_eq!(shells_from_stop(Some(r#"{"will_continue":false}"#)), None);
        assert_eq!(shells_from_stop(None), None);
        assert_eq!(shells_from_stop(Some("not json")), None);
    }

    /// **The feature, end to end.** A terminal `Stop` that still has async jobs
    /// running must land the row on the tier the list implies — `Server` /
    /// `Review` / `Task` — not `Idle`; an empty list settles; and a `Stop`
    /// naming no list holds an already-background row rather than flashing
    /// `Idle`.
    #[test]
    fn a_stop_with_live_async_jobs_lands_on_the_matching_tier() {
        let server = r#"{"async_jobs":[{"type":"bash","label":"npm run dev"}]}"#;
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, server);
        assert_eq!(state.status, SessionStatus::BackgroundServer);

        let review = r#"{"async_jobs":[{"type":"bash","label":"r3 watch review_abc123"}]}"#;
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, review);
        assert_eq!(state.status, SessionStatus::ReviewPending);

        let task = r#"{"async_jobs":[{"type":"task","label":"refactor the parser"}]}"#;
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, task);
        assert_eq!(state.status, SessionStatus::BackgroundActive);

        // An empty list is a turn end saying nothing is in flight.
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, r#"{"async_jobs":[]}"#);
        assert_eq!(state.status, SessionStatus::Idle);

        // No list at all while already on a background tier: hold it there — the
        // previous Stop already named the live work.
        let mut state = state_at(SessionStatus::BackgroundServer);
        feed(&mut state, HookEvent::Stop, r#"{"will_continue":false}"#);
        assert_eq!(state.status, SessionStatus::BackgroundServer);
    }

    /// A `Stop` carrying `will_continue: true` is still the `PostToolUse`
    /// remap (divergence 1), so its async-job list must not settle or classify
    /// the row — it stays `Active`.
    #[test]
    fn a_continuing_stop_does_not_classify_its_async_jobs() {
        let raw = r#"{"will_continue":true,"async_jobs":[{"type":"bash","label":"npm run dev"}]}"#;
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, raw);
        assert_eq!(
            state.status,
            SessionStatus::Active,
            "a continuing agent_end must not take the async-job tier"
        );
    }

    /// **The one that makes `approval_gate: true` honest.** omp has a per-tool
    /// approval gate pi has none of: `tool_approval_requested` holds the row,
    /// and `tool_approval_resolved` releases it — on approve *and* on deny,
    /// since the resolved event fires after either.
    #[test]
    fn an_approval_holds_the_row_and_its_resolution_releases_it() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &payload(r#","tool_name":"bash""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);

        // Resolution fires after approve or deny alike, and either releases the
        // row back to `Active`.
        feed(&mut state, HookEvent::ElicitationResult, &payload(""));
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// omp's `ask` tool renders a blocking multiple-choice card and blocks
    /// until the user answers; its `tool_execution_start` is the only signal
    /// the session is waiting. Without the dispatch arm the row sits at plain
    /// `Active` for as long as the card is up.
    #[test]
    fn an_ask_question_holds_the_row_until_answered() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload(r#","tool_name":"ask""#),
        );
        assert_eq!(
            state.status,
            SessionStatus::WaitingForDecision,
            "the question card must read as a decision, not as work"
        );
        assert_eq!(state.last_tool.as_deref(), Some("ask"));

        // The paired `tool_execution_end` fires once the user answers and
        // settles the row back to `Active` through the shared mapping.
        feed(
            &mut state,
            HookEvent::PostToolUse,
            &payload(r#","tool_name":"ask""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool, None);
    }

    /// A gated `ask` — one that fires `tool_approval_requested` before its
    /// card — must still read as a question, not as a tool-approval gate.
    #[test]
    fn a_gated_ask_reads_as_a_question_not_an_approval() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &payload(r#","tool_name":"ask""#),
        );
        assert_eq!(
            state.status,
            SessionStatus::WaitingForDecision,
            "an approval gate on `ask` is the question, not WaitingForApproval"
        );
        assert_eq!(state.last_tool.as_deref(), Some("ask"));
    }

    /// An interrupted question — the user aborts while the card is up — must
    /// still settle the row; `agent_end` on the abort path arrives as `Stop`
    /// and the shared arm takes it to `Idle`, never stranding Decision.
    #[test]
    fn an_interrupted_question_still_settles() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload(r#","tool_name":"ask""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForDecision);

        feed(&mut state, HookEvent::Stop, &payload(""));
        assert_eq!(
            state.status,
            SessionStatus::Idle,
            "an aborted question must not strand the row in Decision"
        );
    }

    /// **The point of registering `session_switch`.** omp emits it on `/new`,
    /// `/fork` and `/resume`, and an in-session `/resume` or `/fork` must move
    /// the session id — `r`/`f` resume from `state.session_id`. A `SessionStart`
    /// payload carrying a different id while the row is `Idle` replaces it and
    /// leaves `status` at `Idle`.
    #[test]
    fn a_session_switch_delivers_the_new_session_id() {
        let mut state = state_at(SessionStatus::Idle);
        state.session_id = Some("old-id".to_string());
        feed(
            &mut state,
            HookEvent::SessionStart,
            r#"{"session_id":"new-id","session_title":"forked"}"#,
        );
        assert_eq!(state.session_id.as_deref(), Some("new-id"));
        assert_eq!(
            state.status,
            SessionStatus::Idle,
            "a session_switch out of Idle moves the id, not the status"
        );
        // The title rides the same payload, so it is adopted too.
        assert_eq!(state.name.as_deref(), Some("forked"));
    }

    /// Write one session file under `<root>/<bucket>/<name>`, creating the
    /// bucket, and pin its mtime so "newest first" is a fact of the test rather
    /// than a race between two writes.
    fn write_session(root: &Path, bucket: &str, name: &str, body: &str, secs: u64) -> PathBuf {
        let dir = root.join(bucket);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
        path
    }

    /// A `session` header line, with an optional `title` the way omp writes one
    /// on a session it has already titled.
    fn header(id: &str, cwd: &str, title: Option<&str>) -> String {
        let title = title
            .map(|t| format!(r#","title":"{t}""#))
            .unwrap_or_default();
        format!(r#"{{"type":"session","version":3,"id":"{id}","cwd":"{cwd}"{title}}}"#)
    }

    /// The pad-rewritten `title` record omp keeps at line 1.
    fn title_record(title: &str) -> String {
        format!(r#"{{"type":"title","v":1,"title":"{title}","source":"auto"}}"#)
    }

    /// A `message` line whose role is `user`.
    fn user_message(text: &str) -> String {
        format!(
            r#"{{"type":"message","id":"m1","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    /// The picker's rows come off each session file's head: the `session`
    /// header's id and cwd, the `title` record's title, and the first user
    /// message's text (whitespace-collapsed).
    #[test]
    fn sessions_become_resume_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let body = [
            title_record("wire up the parser"),
            header("01a1065d-e96c-77d4-97b9-1522081134c0", "/home/miao/p", None),
            user_message("  add   a test\\nfor the parser  "),
        ]
        .join("\n");
        write_session(
            &root,
            "-home-miao-p",
            "2026-01-01T00-00-00Z_x.jsonl",
            &body,
            100,
        );

        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "01a1065d-e96c-77d4-97b9-1522081134c0");
        assert_eq!(out[0].cwd, "/home/miao/p");
        assert_eq!(out[0].custom_title.as_deref(), Some("wire up the parser"));
        assert_eq!(
            out[0].first_prompt.as_deref(),
            Some("add a test for the parser")
        );
        assert_eq!(out[0].git_branch, None);
        assert_eq!(out[0].agent, AgentControl::Omp);
    }

    /// The `title` record is optional — a brand-new session has none — and the
    /// header's own `title` is the fallback, so a header-only file still yields
    /// a row.
    #[test]
    fn the_title_record_is_optional_and_the_header_title_is_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_session(
            &root,
            "k",
            "a.jsonl",
            &[
                header("id-a", "/home/miao/p", Some("header title")),
                user_message("first"),
            ]
            .join("\n"),
            200,
        );
        write_session(
            &root,
            "k",
            "b.jsonl",
            &[header("id-b", "/home/miao/p", None), user_message("second")].join("\n"),
            100,
        );

        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].session_id, "id-a");
        assert_eq!(out[0].custom_title.as_deref(), Some("header title"));
        assert_eq!(out[1].session_id, "id-b");
        assert_eq!(out[1].custom_title, None);
    }

    /// A file with no `session` header, or a header naming no id or cwd, is
    /// skipped — a candidate that resumes into the wrong place is worse than no
    /// row, the same policy as every other backend.
    #[test]
    fn a_file_without_a_usable_session_header_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_session(
            &root,
            "k",
            "no-header.jsonl",
            &[title_record("t"), user_message("prompt")].join("\n"),
            300,
        );
        write_session(
            &root,
            "k",
            "no-cwd.jsonl",
            &[header("id-x", "", None), user_message("prompt")].join("\n"),
            200,
        );
        write_session(
            &root,
            "k",
            "no-id.jsonl",
            &[header("", "/home/miao/p", None), user_message("prompt")].join("\n"),
            100,
        );

        assert!(list_resumable_in(&root, 10).is_empty());
    }

    /// The same-stem **directory** beside each session file is omp's artifact
    /// and subagent store, not another session; neither it nor a `.jsonl` inside
    /// it may become a picker row.
    #[test]
    fn the_sibling_artifact_directory_is_not_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let body = [header("stem", "/home/miao/p", None), user_message("prompt")].join("\n");
        let file = write_session(&root, "-home-miao-p", "stem.jsonl", &body, 100);
        let sibling = file.with_extension("");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("sub.jsonl"), &body).unwrap();

        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "stem");
    }

    /// `thinking_level_change`, `model_usage`, `custom_message` and an earlier
    /// assistant message all precede the first prompt and must be skipped;
    /// the first `user` message is the prompt.
    #[test]
    fn leading_non_user_lines_are_skipped_to_the_first_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let body = [
            title_record("t"),
            header("id", "/home/miao/p", None),
            r#"{"type":"thinking_level_change","thinkingLevel":"high"}"#.to_string(),
            r#"{"type":"model_usage","role":"judge"}"#.to_string(),
            r#"{"type":"custom_message","customType":"prelude","content":"ignore me"}"#
                .to_string(),
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#
                .to_string(),
            user_message("the real prompt"),
        ]
        .join("\n");
        write_session(&root, "k", "s.jsonl", &body, 100);

        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].first_prompt.as_deref(), Some("the real prompt"));
    }

    /// Candidates come out newest first and stop at `limit` **valid** rows — a
    /// newer headerless file is skipped without spending one of the slots.
    #[test]
    fn candidates_come_out_newest_first_and_respect_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        for (id, secs) in [("old", 100), ("mid", 200), ("new", 300)] {
            let body = [header(id, "/home/miao/p", None), user_message("prompt")].join("\n");
            write_session(&root, "k", &format!("{id}.jsonl"), &body, secs);
        }
        // Newer than all three, and no session header: must not take a slot.
        write_session(
            &root,
            "k",
            "junk.jsonl",
            &[user_message("no header")].join("\n"),
            400,
        );

        let ids: Vec<String> = list_resumable_in(&root, 10)
            .into_iter()
            .map(|c| c.session_id)
            .collect();
        assert_eq!(ids, ["new", "mid", "old"]);

        let two = list_resumable_in(&root, 2);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].session_id, "new");
        assert_eq!(two[1].session_id, "mid");
        assert_eq!(
            list_resumable_in(&root, 3).len(),
            3,
            "a headerless file must not consume one of the limit's slots"
        );
        assert!(list_resumable_in(&root, 0).is_empty());
    }
}
