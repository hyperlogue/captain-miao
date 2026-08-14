//! The hook-event mapping every backend starts from.
//!
//! Claude's and Codex's dispatchers were near-identical copies, and every
//! backend since would have been another. A backend now handles only the events
//! it genuinely treats differently and delegates the rest here, so the arms that
//! *are* common exist once — which is why five of the seven dispatchers are a
//! single delegating line.
//!
//! [`dispatch_default`]'s match is deliberately exhaustive (no `_` arm): a newly
//! added [`HookEvent`] variant must force a decision, and this is the one place
//! that stays true now that the per-agent dispatchers end in a catch-all.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// The first four lines of every backend's `build_launch_command`: resolve the
/// binary, put `direnv` in front of it when the session's directory has an
/// `.envrc`, enter that directory, and put the clipboard shim farm on `PATH`.
/// The caller adds its own environment and argv on top.
///
/// This existed seven times before it existed once, and the copies had drifted
/// in the way copies do: only Claude's checked whether direnv would *accept*
/// the `.envrc`. On the other six a blocked `.envrc` meant `direnv exec` printed
/// its refusal to a stderr nobody reads and exited non-zero, so the launch
/// produced no agent, no state file and no visible reason — the exact failure
/// [`check_direnv_allowed`] was written to prevent.
///
/// `direnv exec <cwd> <bin>` rather than `direnv exec . <bin>`: the child's
/// working directory is set below, but direnv resolves its own argument against
/// *our* cwd, which is the launcher's.
pub(super) fn agent_command(bin: &str, cwd: &str, shim_dir: Option<&Path>) -> Result<Command> {
    let exe = super::find_in_path(bin).with_context(|| format!("{bin} not found in PATH"))?;
    let has_envrc = Path::new(cwd).join(".envrc").is_file();
    let mut cmd = match has_envrc.then(|| super::find_in_path("direnv")).flatten() {
        Some(direnv) => {
            check_direnv_allowed(&direnv, cwd)?;
            let mut c = Command::new(direnv);
            c.args(["exec", cwd]).arg(&exe);
            c
        }
        None => Command::new(&exe),
    };
    cmd.current_dir(cwd);
    super::with_shim_path(&mut cmd, shim_dir);
    Ok(cmd)
}

/// Refuse to launch if direnv would block on the session's `.envrc`. Running
/// `direnv exec` against a blocked file just prints an error to stderr and
/// exits non-zero — the user typically misses that in a `--hold`'d kitty tab.
/// Surfacing it as a captain-miao error makes the fix (`direnv allow <cwd>`)
/// explicit. `direnv status --json` schema: `state.foundRC.allowed` is `0`
/// when approved, non-zero otherwise. Parse failures fall through so a
/// surprise direnv version still gets to produce its own native error.
fn check_direnv_allowed(direnv: &Path, cwd: &str) -> Result<()> {
    let output = std::process::Command::new(direnv)
        .args(["status", "--json"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(output) = output else { return Ok(()) };
    if !output.status.success() {
        return Ok(());
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let found = &parsed["state"]["foundRC"];
    if found.is_null() {
        return Ok(());
    }
    let Some(allowed) = found["allowed"].as_i64() else {
        return Ok(());
    };
    if allowed == 0 {
        return Ok(());
    }
    let envrc = found["path"].as_str().unwrap_or(".envrc");
    let reason = if allowed == 2 {
        "denied"
    } else {
        "not allowed"
    };
    anyhow::bail!(
        "direnv: {envrc} is {reason}. Run `direnv allow {cwd}` to approve, or remove the .envrc to skip direnv."
    );
}

/// The immediate subdirectories of `dir`, or nothing if it can't be read.
///
/// Three backends key their session store on a directory name derived from the
/// working directory — Grok's `<cwd-key>`, Kimi's bucket, opencode's
/// `projectID` — and **none of them is decoded here**. Each agent's own resolver
/// walks every key when it has only a session id, so a scan does the same: one
/// `read_dir` per level, and the authoritative cwd comes out of the session's
/// own metadata rather than out of its path.
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

/// Adopt everything the hook says about *the session* rather than about the
/// event — its id, its title, its context-token total and its model. All of it
/// rides every payload of the backends that report it, so all of it is taken
/// here regardless of `msg.event`.
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
    if let Some(sid) = msg.session_id.take() {
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

    /// A directory with no `.envrc`, so [`agent_command`] takes its direct
    /// branch rather than consulting whatever direnv the test machine has.
    fn plain_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cm-agentcmd-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join(".envrc"));
        dir
    }

    /// The three things every backend used to do for itself, now done once: the
    /// agent's directory is entered, the shim farm is on `PATH` ahead of the
    /// user's own, and the binary is the one resolved from `PATH`.
    #[test]
    fn one_call_enters_the_cwd_and_puts_the_shim_farm_on_path() {
        let cwd = plain_dir("cwd");
        let shim = plain_dir("shim");
        let cmd = agent_command("sh", &cwd.to_string_lossy(), Some(&shim))
            .expect("/bin/sh is on PATH everywhere this runs");
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_current_dir(), Some(cwd.as_path()));
        let path = std_cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("PATH"))
            .and_then(|(_, v)| v)
            .expect("PATH is set for the shim farm");
        assert!(
            std::path::Path::new(path).starts_with(&shim)
                || path.to_string_lossy().starts_with(&*shim.to_string_lossy()),
            "the shim dir must come first: {path:?}"
        );
    }

    /// A missing agent names itself, because this is the error a user sees when
    /// they pick a backend they have not installed.
    #[test]
    fn a_missing_binary_says_which_one() {
        let cwd = plain_dir("missing");
        let err = agent_command("cm-no-such-agent", &cwd.to_string_lossy(), None)
            .expect_err("the binary does not exist");
        assert!(
            err.to_string().contains("cm-no-such-agent"),
            "{err}, which does not name the agent"
        );
    }

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
        }
    }

    fn msg(session_title: Option<&str>) -> HookMessage {
        HookMessage {
            session_title: session_title.map(str::to_string),
            ..blank()
        }
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
