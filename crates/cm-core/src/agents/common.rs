//! The hook-event mapping every backend starts from.
//!
//! Claude's and Codex's dispatchers were near-identical copies; a third, fourth
//! and fifth would be the wrong direction. A backend now handles only the events
//! it genuinely treats differently and delegates the rest here, so the arms that
//! *are* common exist once.
//!
//! [`dispatch_default`]'s match is deliberately exhaustive (no `_` arm): a newly
//! added [`HookEvent`] variant must force a decision, and this is the one place
//! that stays true now that the per-agent dispatchers end in a catch-all.

use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// Adopt the hook's session id, if it carries one. Agents mint a fresh id on
/// resume (Claude's `/resume`), so the freshest one always wins.
///
/// Split out because a backend that handles an event *itself* must still do
/// this; [`dispatch_default`] calls it too, so the delegating path is covered
/// exactly once either way.
pub(super) fn adopt_session_id(state: &mut LauncherState, msg: &mut HookMessage) {
    if let Some(sid) = msg.session_id.take() {
        state.session_id = Some(sid);
    }
}

/// Map one hook event onto the launcher state the way every backend agrees on.
/// Per-agent departures (a tool that means "blocked on the user", an agent whose
/// `Stop` defers to its own session file) are handled in the backend module
/// *before* it delegates here.
pub(super) fn dispatch_default(state: &mut LauncherState, mut msg: HookMessage) {
    adopt_session_id(state, &mut msg);

    match msg.event {
        HookEvent::SessionStart => {
            if state.status == SessionStatus::Starting {
                state.status = SessionStatus::Idle;
            }
        }
        HookEvent::PromptSubmit => {
            state.status = SessionStatus::Active;
            state.last_tool = None;
            state.last_error = None;
            if let Some(prompt) = msg.prompt {
                state.last_prompt = Some(prompt);
            }
        }
        HookEvent::PreToolUse => {
            state.status = SessionStatus::Active;
            state.last_tool = msg.tool_name;
        }
        // An agent that folds tool failures into its PostToolUse payload (Codex
        // reports them in `tool_response`) simply never emits the failure event;
        // both settle the same way regardless.
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            state.status = SessionStatus::Active;
            state.last_tool = None;
        }
        HookEvent::PermissionRequest => {
            state.status = SessionStatus::WaitingForApproval;
        }
        HookEvent::Elicitation => {
            state.status = SessionStatus::WaitingForDecision;
        }
        HookEvent::ElicitationResult => {
            state.status = SessionStatus::Active;
        }
        HookEvent::Stop => {
            state.status = SessionStatus::Idle;
            state.last_tool = None;
        }
        HookEvent::StopFailure => {
            state.status = SessionStatus::Idle;
            state.last_tool = None;
            state.last_error = msg
                .message
                .or(msg.raw)
                .or_else(|| Some("Stop hook failed".to_string()));
        }
        HookEvent::PreCompact => {
            state.status = SessionStatus::Compacting;
        }
        HookEvent::PostCompact => {
            // Manual `/compact` doesn't fire a Stop hook, so we must leave
            // Compacting ourselves. The dashboard treats Compacted as a rest
            // state and auto-marks it as needing follow-up. If compact was
            // triggered mid-turn, the next PreToolUse/Stop overwrites this
            // within milliseconds and the dashboard clears the bell.
            state.status = SessionStatus::Compacted;
        }
        HookEvent::CwdChanged => {
            if let Some(cwd) = msg.cwd {
                state.cwd = cwd;
            }
        }
    }
}
