//! Pi CLI backend: per-run extension injection, lifecycle events and resume
//! discovery. The dashboard reaches this through `AgentControl::Pi`.
//!
//! Verified with Pi 1.1.0 and upstream `earendil-works/pi` at `7bd01fb`.
//! The opt-in test in `live_tests` runs the real RPC CLI against a loopback
//! model fixture: startup, a read tool, drafting, rename, model selection,
//! interrupted inference, manual compaction, a new session, cold resume and
//! fork. It needs no account or external model API. See
//! `docs/testing-sessions.md` for the command and remaining coverage limits.
//!
//! # Extension ownership and delivery
//!
//! `pi -e <path.ts>` loads our generated extension for this process without
//! changing the user's configuration or home. Explicit CLI extensions load
//! before project trust is resolved. The launcher relocates its opaque settings
//! file to a shared `.ts` path through [`Extension::command`]. Only the miao
//! executable is embedded; the per-session socket arrives in
//! `$CAPTAIN_MIAO_SOCK` and is inherited by the spawned hook child.
//!
//! [`FORWARDED`] defines the observed events. The shared Pi/omp transport
//! captures metadata immediately and serializes hook subprocesses, including
//! notifications Pi does not await (`session_info_changed`). A five-second
//! child deadline prevents a failed launcher connection from blocking Pi.
//! Handlers always resolve without a value: they observe events without
//! modifying Pi's prompt, tool result or compaction decision.
//!
//! # Status and metadata
//!
//! `before_agent_start` starts a turn; `agent_settled` ends it only after
//! automatic retries, compaction and queued continuations have finished. An
//! aborted run settles through the same event. Manual compaction has its own
//! success/failure pair because no agent run need be active. A failed
//! compaction stays active only when `willRetry` is true.
//!
//! `ctx.getContextUsage()`, `ctx.model` and `pi.getSessionName()` supply live
//! metadata. Model selections and tree navigation refresh it even while idle.
//! Unknown token counts remain absent, and model references include the
//! provider. Changing session IDs clears metadata belonging to the old draft;
//! a rename in the same session preserves its prompt and token count.
//!
//! Live facts come exclusively from hooks. Pi's JSONL transcript is a tree,
//! so a naive tail can read usage from an abandoned branch. The resume picker
//! instead reads session-wide metadata: header ID/cwd, first user text and
//! latest `session_info` name. It honors `PI_CODING_AGENT_DIR` and the flat
//! `PI_CODING_AGENT_SESSION_DIR` override. Per-invocation `--session-dir` and
//! project-specific storage settings are not discovered by the dashboard.
//!
//! # Limits
//!
//! Pi has no built-in per-tool approval gate. Project trust can leave the row
//! at `Starting` until answered. Extension UI decision prompts and background
//! jobs are not tracked. Real terminal attach/detach, keyboard Escape and
//! pooled-session clipboard paste still need interactive validation; the RPC
//! abort test verifies lifecycle settlement, not terminal key handling.
//! `clipboard-paste` remains the pooled clipboard fallback.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::pi_extension::Extension;
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "pi";

mod sessions;

#[cfg(test)]
mod live_tests;

pub fn list_resumable(limit: usize) -> Result<Vec<crate::agent::ResumeCandidate>> {
    list_resumable_cached(limit, &mut crate::resume::AgentCache::default())
}

pub(crate) fn list_resumable_cached(
    limit: usize,
    cache: &mut crate::resume::AgentCache,
) -> Result<Vec<crate::agent::ResumeCandidate>> {
    sessions::list_resumable(limit, cache)
}

// =============================================================================
// The event table — the module's claim about what it subscribes to
// =============================================================================

/// Every Pi event the generated extension subscribes to, paired with the
/// [`HookEvent`] it is forwarded as. This const **is** the claim; the emitted
/// JavaScript renders it, so the two cannot disagree.
///
/// The choices in here that aren't 1:1:
///
/// - **`tool_execution_start` / `tool_execution_end`, not `tool_call` /
///   `tool_result`.** All four fire on every tool (`extensions.md`'s lifecycle
///   diagram brackets the inner pair with the outer one), but `tool_call`
///   *"can block"* — its return value is `{block, reason, terminate}` — and
///   `tool_result` *"can modify"* the result. Subscribing to the pair whose
///   return value cannot alter the user's turn is the safer half of a free
///   choice, and `tool_execution_end` carries `isError` besides.
/// - **`before_agent_start`, not `input`, for [`HookEvent::PromptSubmit`].**
///   `input` fires earlier but is an interception point: its handler contract is
///   `{action: "continue" | "transform" | "handled"}`, and it also sees text
///   that never becomes a turn. `before_agent_start` is documented as *"Fired
///   after user submits prompt, before agent loop"*, carries `event.prompt`, and
///   returns an optional patch — so it means exactly "the turn started" and
///   costs nothing to answer.
/// - **`session_info_changed` forwarded as [`HookEvent::SessionStart`].** It
///   carries a rename and no status of its own; every payload we send already
///   carries the title, so the point of registering it at all is to deliver a
///   rename that is followed by *nothing else*. `SessionStart` is the one arm in
///   [`common::dispatch_default`] that touches status only to settle a row out
///   of `Starting` — and a rename cannot happen while `Starting`, since
///   `session_start` has already fired by then. So it adopts the title and
///   moves nothing, which is precisely what is wanted.
/// - **`session_compact_failed` uses [`HookEvent::StopFailure`].** Manual
///   compaction has no later `agent_settled` to clear `Compacting`. Rust uses
///   its `aborted` and `willRetry` fields to distinguish cancellation, failure
///   and an automatic retry; only a terminal failure surfaces error text.
/// - **`model_select` and `session_tree` use [`HookEvent::ModelSwitch`].** Both
///   refresh metadata without making a claim about whether Pi is working.
///
/// Deliberately not registered: `agent_start` / `agent_end` / `turn_*` /
/// `message_*` / `context` / `before_provider_*` / `after_provider_response`
/// (too fine-grained — each would spawn a subprocess per LLM call to discard its
/// payload), `session_shutdown` (the launcher owns exit), `session_before_switch`
/// / `session_before_fork` / `session_before_tree` (a
/// replacement session re-emits `session_start`, which we do register),
/// `user_bash` and `thinking_level_select` (no corresponding dashboard field),
/// and `project_trust` / `resources_discover` (see the
/// module doc).
const FORWARDED: &[(&str, HookEvent)] = &[
    ("session_start", HookEvent::SessionStart),
    ("session_info_changed", HookEvent::SessionStart),
    ("model_select", HookEvent::ModelSwitch),
    ("session_tree", HookEvent::ModelSwitch),
    ("before_agent_start", HookEvent::PromptSubmit),
    ("tool_execution_start", HookEvent::PreToolUse),
    ("tool_execution_end", HookEvent::PostToolUse),
    ("agent_settled", HookEvent::Stop),
    ("session_before_compact", HookEvent::PreCompact),
    ("session_compact", HookEvent::PostCompact),
    ("session_compact_failed", HookEvent::StopFailure),
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
        ("aborted", "event?.aborted"),
        ("will_retry", "event?.willRetry"),
        ("error_message", "event?.errorMessage"),
    ],
};

/// The "hook settings" the launcher writes to its per-session file — for Pi,
/// **TypeScript source**, not JSON. The path is generic transport and its
/// contents are opaque to the launcher, so each backend puts its own format
/// through it (Kimi already puts TOML through the same channel).
///
/// `sock_path` is ignored, as it is for Codex, Reasonix and Kimi and for the
/// same reason: the file is shared by every session, so it cannot carry a
/// per-session path. The socket reaches the hook through the environment
/// instead — and unlike those three, that trip needs no faith. Our forwarder
/// spawns the child itself with `spawn`'s default inherited environment, from
/// inside the pi process we set `CAPTAIN_MIAO_SOCK` on, so there is no agent
/// hook-env scrubbing to survive.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    EXTENSION.source(&exe.to_string_lossy())
}

// =============================================================================
// Launcher: process spawn
// =============================================================================

/// Build the argv for a Pi session. Like omp, what the launcher wrote is a
/// generated TypeScript extension rather than a hook config, relocated to a
/// path Pi's loader will accept.
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
/// happens to emit — `Extension::source` builds it and this parses it, so the two
/// are pinned together by the tests below rather than by a vendor's docs.
///
/// snake_case, matching the launcher's own wire vocabulary; the JavaScript names
/// these keys explicitly rather than dumping a pi event object, because pi's
/// event payloads carry `AbortSignal`s and message graphs that `JSON.stringify`
/// would either flatten to `{}` or refuse outright.
///
/// The pi-side sources, one per field: `ctx.sessionManager.getSessionId()`,
/// `pi.getSessionName()`, `ctx.cwd`, `event.toolName` (the tool-execution
/// events), `event.prompt` (`before_agent_start`), `event.isError`
/// (`tool_execution_end`), `event.aborted` / `event.willRetry` /
/// `event.errorMessage` (`session_compact_failed`), `ctx.getContextUsage().tokens`
/// and `ctx.model.provider` / `ctx.model.id`.
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
    /// Compaction may stop, be cancelled, or leave an automatic retry running.
    #[serde(default)]
    aborted: bool,
    #[serde(default)]
    will_retry: bool,
    error_message: Option<String>,
}

/// Normalize one Pi hook payload, as sent by the generated extension.
pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse pi hook JSON from stdin")?;
    Ok(HookMessage {
        event: normalize_event(event, &payload),
        session_id: payload.session_id,
        tool_name: payload.tool_name,
        message: payload.error_message,
        cwd: payload.cwd,
        prompt: payload.prompt,
        // All three ride every payload — see the module doc for why they come
        // from the hook rather than from a transcript fold.
        session_title: payload.session_title,
        context_tokens: payload.context_tokens,
        model: payload.model,
        // Pi *does* export `PI_SESSION_FILE` and its JSONL schema is documented,
        // so this is a decision rather than a gap: one fact, one source, and the
        // tokens and model above are already that source. Leaving it `None` also
        // keeps the launcher's transcript pipeline inert, since it only ever
        // runs on a path a hook supplied.
        transcript_path: None,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

/// A tool that failed arrives as `tool_execution_end` with `isError`
/// (`extensions.md`: *"event.toolCallId, event.toolName, event.result,
/// event.isError"*), which our vocabulary spells
/// [`HookEvent::PostToolUseFailure`].
///
/// **Cosmetic today, and kept anyway.** `dispatch_default` settles the two
/// identically, so nothing on the row moves differently. It is here because the
/// fact is on the payload and dropping it would read as an oversight later, and
/// because this is where the correction belongs the day the two arms diverge.
///
/// Compaction failures settle unless Pi will retry automatically. Cancellation
/// is a clean stop; only a terminal failure reports an error. `PostToolUse` is
/// the shared active transition, and does not turn a retry into a new prompt.
fn normalize_event(event: HookEvent, payload: &HookPayload) -> HookEvent {
    match event {
        HookEvent::PostToolUse if payload.is_error => HookEvent::PostToolUseFailure,
        HookEvent::StopFailure if payload.will_retry => HookEvent::PostToolUse,
        HookEvent::StopFailure if payload.aborted => HookEvent::Stop,
        other => other,
    }
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Forget the old conversation's metadata on an in-process session switch,
/// then apply the shared status transitions. Renames retain the session ID and
/// therefore preserve its other facts.
pub fn dispatch_hook(state: &mut LauncherState, msg: HookMessage) {
    if msg.event == HookEvent::SessionStart
        && let Some(id) = msg.session_id.as_deref().filter(|id| !id.trim().is_empty())
        && state.session_id.as_deref().is_some_and(|old| old != id)
    {
        // /new, /resume and /fork reuse the process. Missing metadata belongs
        // to the new conversation, not to the draft previously on this row.
        state.name = None;
        state.last_prompt = None;
        state.last_tool = None;
        state.context_tokens = None;
        state.model = None;
        state.set_last_error(None);
        state.status = crate::state::SessionStatus::Idle;
    }
    common::dispatch_default(state, msg)
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
    /// [`Extension::source`]; the separate opt-in live test checks that Pi's
    /// actual extension context still supplies these fields.
    fn payload(extra: &str) -> String {
        format!(
            r#"{{"session_id":"s1","session_title":"wire up the parser",
                "cwd":"/home/miao/p","context_tokens":48100,"model":"some-model-1"{extra}}}"#
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::Pi, status)
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
    /// `miao hook --agent pi <name>` the CLI rejects is a status silently lost.
    #[test]
    fn the_registered_events_are_exactly_what_the_module_claims() {
        // The native names, in the order pi sees them registered.
        assert_eq!(
            FORWARDED.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [
                "session_start",
                "session_info_changed",
                "model_select",
                "session_tree",
                "before_agent_start",
                "tool_execution_start",
                "tool_execution_end",
                "agent_settled",
                "session_before_compact",
                "session_compact",
                "session_compact_failed",
            ]
        );
        // Only agent_settled unconditionally means Stop. Compaction failure
        // needs its payload checked before deciding whether Pi will continue.
        assert_eq!(
            FORWARDED
                .iter()
                .filter(|(_, e)| *e == HookEvent::Stop)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>(),
            ["agent_settled"]
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
        assert!(source.contains(r#"spawn(MIAO, ["hook", "--agent", "pi", forwarded]"#));
    }

    /// A turn runs prompt → tool → settle, and `agent_settled` is what ends it.
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

    /// `isError` on a `tool_execution_end` is the one payload-driven event
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
        // `Math.round(undefined)` serializes to when pi reports no usage yet,
        // and it must read as "not reported" rather than failing the payload —
        // which would take the *status* down with it, not just the number.
        let unusable = parse_hook_payload(HookEvent::Stop, r#"{"context_tokens":null}"#)
            .expect("a null token count parses");
        assert_eq!(unusable.context_tokens, None);
    }

    #[test]
    fn failed_compaction_settles_but_a_retry_stays_active() {
        let failure = FORWARDED
            .iter()
            .find(|(native, _)| *native == "session_compact_failed")
            .expect("compaction failures must reach the launcher")
            .1;
        for (extra, expected, error) in [
            (
                r#", "aborted": true, "will_retry": false"#,
                SessionStatus::Idle,
                None,
            ),
            (
                r#", "error_message": "summary failed", "will_retry": false"#,
                SessionStatus::Idle,
                Some("summary failed"),
            ),
            (
                r#", "error_message": "summary failed", "will_retry": true"#,
                SessionStatus::Active,
                None,
            ),
            (
                r#", "aborted": true, "will_retry": true"#,
                SessionStatus::Active,
                None,
            ),
        ] {
            let mut state = state_at(SessionStatus::Idle);
            feed(&mut state, HookEvent::PreCompact, &payload(""));
            feed(&mut state, failure, &payload(extra));
            assert_eq!(state.status, expected);
            assert_eq!(state.last_error.as_deref(), error);
        }
    }

    #[test]
    fn generated_forwarder_carries_compaction_outcomes() {
        let messages =
            super::super::forwarder_test::run(&EXTENSION.source("/path/to/miao"), "pi_compaction");
        assert_eq!(messages.len(), 4);
        let mut state = state_at(SessionStatus::Idle);
        for ((event, body), status) in messages.into_iter().zip([
            SessionStatus::Compacting,
            SessionStatus::Idle,
            SessionStatus::Idle,
            SessionStatus::Active,
        ]) {
            feed(&mut state, event, &body);
            assert_eq!(state.status, status);
        }
    }

    #[test]
    fn model_selection_and_tree_navigation_refresh_idle_metadata() {
        let messages =
            super::super::forwarder_test::run(&EXTENSION.source("/path/to/miao"), "pi_metadata");
        let mut state = state_at(SessionStatus::Idle);
        state.context_tokens = Some(9000);
        assert_eq!(messages.len(), 2);
        for (index, (event, body)) in messages.into_iter().enumerate() {
            feed(&mut state, event, &body);
            assert_eq!(state.status, SessionStatus::Idle);
            assert_eq!(
                state.model.as_deref(),
                Some("openrouter/google/gemini-3.8-flash")
            );
            assert_eq!(
                state.context_tokens,
                Some(if index == 0 { 9000 } else { 4321 })
            );
        }
    }

    #[test]
    fn a_new_session_does_not_inherit_the_previous_drafts_metadata() {
        let mut state = state_at(SessionStatus::Idle);
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload(r#", "prompt": "Old draft""#),
        );
        feed(&mut state, HookEvent::Stop, &payload(""));
        state.set_last_error(Some("An old failure".into()));
        feed(
            &mut state,
            HookEvent::SessionStart,
            r#"{"session_id":"new"}"#,
        );
        assert_eq!(state.session_id.as_deref(), Some("new"));
        assert_eq!(state.name, None);
        assert_eq!(state.last_prompt, None);
        assert_eq!(state.context_tokens, None);
        assert_eq!(state.model, None);
        assert_eq!(state.last_error, None);
        assert_eq!(state.status, SessionStatus::Idle);
    }

    /// No transcript path, ever — the field the launcher gates its whole
    /// transcript watch on. Pi has a readable one; not naming it is what keeps
    /// the hook the single source for tokens and model.
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
}
