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

/// Adopt what the hook says about *which session this is* — its id, and its
/// title where the backend reports one. Both ride every payload rather than a
/// particular event, so both are taken here regardless of `msg.event`.
///
/// Session ids: agents mint a fresh one on resume (Claude's `/resume`), so the
/// freshest always wins.
///
/// Titles: an agent that puts its own title on the payload has already done the
/// work Claude's session-file fold and Codex's sqlite overlay exist to do, so
/// the value goes straight onto `name` — including a rename, which is just a
/// later payload carrying a different title. An **empty** title is not a rename
/// to nothing; it means the agent hasn't titled the session yet, and taking it
/// would clear a name the launcher already folded.
///
/// One function rather than two because a backend that handles an event
/// *itself* must still do this, and the point of the pairing is that there is
/// no way to remember the id and forget the title.
pub(super) fn adopt_session_identity(state: &mut LauncherState, msg: &mut HookMessage) {
    if let Some(sid) = msg.session_id.take() {
        state.session_id = Some(sid);
    }
    if let Some(title) = msg.session_title.take().filter(|t| !t.trim().is_empty()) {
        state.name = Some(title);
    }
}

/// Map one hook event onto the launcher state the way every backend agrees on.
/// Per-agent departures (a tool that means "blocked on the user", an agent whose
/// `Stop` defers to its own session file) are handled in the backend module
/// *before* it delegates here.
pub(super) fn dispatch_default(state: &mut LauncherState, mut msg: HookMessage) {
    adopt_session_identity(state, &mut msg);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;

    fn state() -> LauncherState {
        LauncherState {
            agent: AgentControl::Claude,
            launcher_pid: 0,
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
            pool_session: None,
            launch_id: None,
            terminal: None,
            terminfo: None,
            flags: None,
            attached: None,
            host: crate::state::HostId::local(),
        }
    }

    fn msg(session_title: Option<&str>) -> HookMessage {
        HookMessage {
            event: HookEvent::Stop,
            session_id: None,
            tool_name: None,
            message: None,
            cwd: None,
            prompt: None,
            session_title: session_title.map(str::to_string),
            transcript_path: None,
            raw: None,
        }
    }

    /// A backend that reports its own title needs no title store at all: the
    /// value lands on `name` directly, and a later payload carrying a different
    /// one *is* the rename.
    #[test]
    fn a_reported_title_becomes_the_name_and_a_later_one_replaces_it() {
        let mut s = state();
        adopt_session_identity(&mut s, &mut msg(Some("wire up the parser")));
        assert_eq!(s.name.as_deref(), Some("wire up the parser"));

        adopt_session_identity(&mut s, &mut msg(Some("renamed by the user")));
        assert_eq!(s.name.as_deref(), Some("renamed by the user"));
    }

    /// The case that would silently erase a name: an agent that hasn't titled
    /// the session yet reports the field empty rather than omitting it, and
    /// "not titled yet" is not a rename to nothing.
    #[test]
    fn an_empty_title_never_clears_a_name_we_already_have() {
        let mut s = state();
        s.name = Some("folded from the session file".to_string());

        for blank in ["", "   "] {
            adopt_session_identity(&mut s, &mut msg(Some(blank)));
            assert_eq!(s.name.as_deref(), Some("folded from the session file"));
        }
        // As does a payload with no title field at all — the shape Claude and
        // Codex send on every hook.
        adopt_session_identity(&mut s, &mut msg(None));
        assert_eq!(s.name.as_deref(), Some("folded from the session file"));
    }
}
