//! Shared launch, transcript, and hook helpers for agent implementations.
//!
//! [`launch`] owns command construction. Backends handle their specific hook
//! events and delegate the rest to [`dispatch_default`], whose exhaustive match
//! requires an explicit decision for every new [`HookEvent`].

use std::path::Path;

mod launch;
pub(super) use launch::agent_command;

use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// The immediate subdirectories of `dir`, or nothing if it can't be read.
///
/// Grok and Kimi bucket sessions by working directory; Codex nests rollouts by
/// date. Walk each store one level at a time and read the authoritative cwd
/// from session metadata rather than attempting to decode the directory name.
pub(super) fn read_subdirs(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Newest first, capped at `limit` — the ordering every resume picker wants and
/// the cap that keeps a five-year-old session store off a keystroke path.
/// Applied to the *stat* results, before any file is opened, so the cost of a
/// picker is one `read_dir` walk plus `limit` reads rather than one read per
/// session that ever existed.
pub(super) fn newest_first<T>(
    mut found: Vec<(T, std::time::SystemTime)>,
    limit: usize,
) -> Vec<(T, std::time::SystemTime)> {
    found.sort_by_key(|f| std::cmp::Reverse(f.1));
    found.truncate(limit);
    found
}

/// Read the tail of a line-delimited file and return whole lines (dropping any
/// leading partial line from a mid-file seek).
///
/// We start with a ~256 KB window but grow it backward (doubling) until the
/// window either contains a newline — so at least one complete line survives the
/// leading-partial drop — or reaches the file start. Without this, a single line
/// longer than the window (a giant pasted message in Codex's rollout, a
/// multi-hundred-KB tool result in Grok's `updates.jsonl`) would leave the
/// window newline-free and the `split_once('\n')` drop would discard the whole
/// thing, losing every later record.
///
/// This is a *bounded one-shot read*, never a watch: a record older than the
/// window it reaches reads as absent, and callers treat that the same way
/// they treat a file that isn't there yet.
pub(super) fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    const INITIAL_WINDOW: u64 = 256 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();

    let mut window = INITIAL_WINDOW;
    loop {
        let start = size.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::with_capacity(window.min(size) as usize);
        file.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);

        if start == 0 {
            return Some(text.into_owned());
        }
        if let Some((_, rest)) = text.split_once('\n') {
            return Some(rest.to_string());
        }
        // No newline in this window: an oversized line precedes us. Grow and
        // retry so the preceding complete lines become reachable.
        window = window.saturating_mul(2);
    }
}

/// Adopt everything the hook says about *the session* rather than about the
/// event — its id, its title, its context-token total and its model. All of it
/// rides every payload of the backends that report it, so all of it is taken
/// here regardless of `msg.event`.
///
/// Session ids: agents mint a fresh one on resume (Claude's `/resume`), so the
/// freshest always wins. An **empty** id is absent, not a new identity — the
/// same rule as the title's, and load-bearing for any backend whose id is dug
/// out of a payload or a getter that can come up blank (Grok's approval hook
/// synthesizes it from `$GROK_SESSION_ID` in shell; Pi's extension calls a
/// getter): taking it would overwrite the real id `r` and `f` resume from.
///
/// Titles: an agent that puts its own title on the payload has already done the
/// work Claude's session-file fold and Codex's sqlite overlay exist to do, so
/// the value goes straight onto `name` — including a rename, which is just a
/// later payload carrying a different title. An **empty** title is not a rename
/// to nothing; it means the agent hasn't titled the session yet, and taking it
/// would clear a name the launcher already folded.
///
/// Tokens and model: the same argument one field further. An agent that reports
/// them needs no transcript read at all, which is the only route for a backend
/// whose sessions live in a database or an undocumented sidecar. **A backend
/// should report them here or fold them from a transcript, not both** — the two
/// are separate sources for one fact, and picking one is what keeps them from
/// disagreeing. Both are last-write-wins (unlike the title, whose empty case is
/// special): a token count that has genuinely dropped after a compaction is a
/// real new value, not a missing one.
///
/// One function rather than four because a backend that handles an event
/// *itself* must still do all of this, and the point of the grouping is that
/// there is no way to remember the id and forget the rest.
pub(super) fn adopt_session_facts(state: &mut LauncherState, msg: &mut HookMessage) {
    if let Some(sid) = msg.session_id.take().filter(|s| !s.trim().is_empty()) {
        state.session_id = Some(sid);
    }
    if let Some(title) = msg.session_title.take().filter(|t| !t.trim().is_empty()) {
        state.name = Some(title);
    }
    if let Some(tokens) = msg.context_tokens.take() {
        state.context_tokens = Some(tokens);
    }
    if let Some(model) = msg.model.take().filter(|m| !m.trim().is_empty()) {
        state.model = Some(model);
    }
}

/// Map one hook event onto the launcher state the way every backend agrees on.
/// Per-agent departures (a tool that means "blocked on the user", an agent whose
/// `Stop` defers to its own session file) are handled in the backend module
/// *before* it delegates here.
pub(super) fn dispatch_default(state: &mut LauncherState, mut msg: HookMessage) {
    adopt_session_facts(state, &mut msg);

    match msg.event {
        HookEvent::SessionStart => {
            if state.status == SessionStatus::Starting {
                state.status = SessionStatus::Idle;
            }
        }
        HookEvent::PromptSubmit => {
            state.status = SessionStatus::Active;
            state.last_tool = None;
            state.set_last_error(None);
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
            state.set_last_error(Some(
                msg.message
                    .or(msg.raw)
                    .unwrap_or_else(|| "Stop hook failed".to_string()),
            ));
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
        // Nothing to do here, and that is the mapping rather than an omission:
        // `adopt_session_facts` above has already stamped the new id, and the
        // switch itself proves nothing about the session's status. It fires
        // from a `/model` on an idle row just as readily as from an automatic
        // fallback mid-turn, so *any* status written here would be wrong half
        // the time — and would overwrite a live one with a guess.
        HookEvent::ModelSwitch => {}
        // The backend has already applied its snapshot's activity vocabulary.
        HookEvent::StatusUpdate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;

    fn state() -> LauncherState {
        LauncherState::for_test(AgentControl::Claude, SessionStatus::Idle)
    }

    fn blank() -> HookMessage {
        HookMessage {
            event: HookEvent::Stop,
            session_id: None,
            tool_name: None,
            message: None,
            cwd: None,
            prompt: None,
            session_title: None,
            context_tokens: None,
            model: None,
            transcript_path: None,
            raw: None,
            session_is_child: None,
        }
    }

    fn msg(session_title: Option<&str>) -> HookMessage {
        HookMessage {
            session_title: session_title.map(str::to_string),
            ..blank()
        }
    }

    #[test]
    fn errors_keep_their_occurrence_time_until_replaced_or_cleared() {
        let mut s = state();
        let fail = || HookMessage {
            event: HookEvent::StopFailure,
            message: Some("Provider unavailable".into()),
            ..blank()
        };
        let before = LauncherState::now();
        dispatch_default(&mut s, fail());
        assert_eq!(s.last_error.as_deref(), Some("Provider unavailable"));
        assert!((before..=LauncherState::now()).contains(&s.last_error_at.unwrap()));

        s.last_error_at = Some(1);
        dispatch_default(
            &mut s,
            HookMessage {
                event: HookEvent::PreToolUse,
                ..blank()
            },
        );
        assert_eq!(
            s.last_error_at,
            Some(1),
            "activity must not refresh an error"
        );
        dispatch_default(&mut s, fail());
        assert!(
            s.last_error_at.unwrap() >= before,
            "a repeated error is fresh"
        );

        dispatch_default(
            &mut s,
            HookMessage {
                event: HookEvent::PromptSubmit,
                ..blank()
            },
        );
        assert_eq!(s.last_error, None);
        assert_eq!(s.last_error_at, None);
    }

    /// A backend that reports its own title needs no title store at all: the
    /// value lands on `name` directly, and a later payload carrying a different
    /// one *is* the rename.
    #[test]
    fn a_reported_title_becomes_the_name_and_a_later_one_replaces_it() {
        let mut s = state();
        adopt_session_facts(&mut s, &mut msg(Some("wire up the parser")));
        assert_eq!(s.name.as_deref(), Some("wire up the parser"));

        adopt_session_facts(&mut s, &mut msg(Some("renamed by the user")));
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
            adopt_session_facts(&mut s, &mut msg(Some(blank)));
            assert_eq!(s.name.as_deref(), Some("folded from the session file"));
        }
        // As does a payload with no title field at all — the shape Claude and
        // Codex send on every hook.
        adopt_session_facts(&mut s, &mut msg(None));
        assert_eq!(s.name.as_deref(), Some("folded from the session file"));
    }

    /// A blank session id is absent, not a new identity — a backend whose id
    /// is synthesized from a shell variable (Grok) or read off a getter (Pi)
    /// can come up empty, and taking it would overwrite the real id `r` and
    /// `f` resume from.
    #[test]
    fn a_blank_session_id_never_overwrites_a_real_one() {
        let mut s = state();
        s.session_id = Some("real-id".to_string());

        for empty in ["", "   "] {
            let mut m = HookMessage {
                session_id: Some(empty.to_string()),
                ..blank()
            };
            adopt_session_facts(&mut s, &mut m);
            assert_eq!(s.session_id.as_deref(), Some("real-id"));
        }
        // A real one still wins — the freshest id is the resumable one.
        let mut m = HookMessage {
            session_id: Some("minted-on-resume".to_string()),
            ..blank()
        };
        adopt_session_facts(&mut s, &mut m);
        assert_eq!(s.session_id.as_deref(), Some("minted-on-resume"));
    }

    /// The route for a backend with no readable transcript: the agent reports
    /// the numbers itself and they land on the row without a fold.
    #[test]
    fn tokens_and_model_can_arrive_on_the_payload() {
        let mut s = state();
        let mut m = HookMessage {
            context_tokens: Some(48_100),
            model: Some("some-model-1".to_string()),
            ..blank()
        };
        adopt_session_facts(&mut s, &mut m);
        assert_eq!(s.context_tokens, Some(48_100));
        assert_eq!(s.model.as_deref(), Some("some-model-1"));

        // A **lower** count is a real value, not a missing one — that is what a
        // compaction looks like — so unlike the title these are last-write-wins
        // with no emptiness rule beyond a blank model string.
        let mut m = HookMessage {
            context_tokens: Some(9_000),
            ..blank()
        };
        adopt_session_facts(&mut s, &mut m);
        assert_eq!(s.context_tokens, Some(9_000));
        // The absent model on that payload left the known one alone.
        assert_eq!(s.model.as_deref(), Some("some-model-1"));
    }

    /// A payload that says nothing about tokens or model must not blank a row
    /// that already has them — the shape every hook of a transcript-backed
    /// backend sends.
    #[test]
    fn a_silent_payload_never_clears_tokens_or_model() {
        let mut s = state();
        s.context_tokens = Some(120_000);
        s.model = Some("some-model-1".to_string());

        adopt_session_facts(&mut s, &mut blank());
        assert_eq!(s.context_tokens, Some(120_000));
        assert_eq!(s.model.as_deref(), Some("some-model-1"));

        // An empty model string is "not reported", not "no model".
        let mut m = HookMessage {
            model: Some("   ".to_string()),
            ..blank()
        };
        adopt_session_facts(&mut s, &mut m);
        assert_eq!(s.model.as_deref(), Some("some-model-1"));
    }
}
