//! Shared CLI helpers used by both binaries' `main`: extraction of the
//! captain-miao-owned launcher flags, and the `claude`/`codex`/`hook` entrypoint
//! bodies, so the dashboard and the server dispatch them identically.

use anyhow::Result;

use crate::agent::AgentControl;
use crate::logging::init_tracing;

/// Split a `claude`/`codex` positional list into `(cwd, passthrough_args)`.
///
/// The dashboard always invokes us as `[exe, <subcommand>, <cwd>, <extra...>]`,
/// so the first positional is the cwd. Manual invocations may instead lead with
/// a flag (`miao claude --resume`); a clap-level defaulted `cwd`
/// positional would swallow that flag, so we route it here: the first element
/// is treated as the cwd only when it does NOT begin with `-`, otherwise the
/// cwd defaults to `.` and every argument is forwarded to the agent.
pub fn split_cwd(args: Vec<String>) -> (String, Vec<String>) {
    match args.split_first() {
        Some((first, rest)) if !first.starts_with('-') => (first.clone(), rest.to_vec()),
        _ => (".".to_string(), args),
    }
}

/// Pull a `--pool-session <name>` pair out of the launcher args (the server adds
/// it when starting a launcher in a remote pty pool), returning the name and the
/// remaining args. captain-miao owns this flag, so it must be removed before the
/// rest is forwarded to the agent.
pub fn take_pool_session(args: Vec<String>) -> (Option<String>, Vec<String>) {
    take_flag_value(args, "--pool-session")
}

/// Pull a `--launch-id <token>` pair out of the launcher args (the dashboard adds
/// it when it spawns a *local* launcher so the appearing row can be matched back
/// to the window it opened — next-step #6 §15). captain-miao owns this flag, so it
/// must be removed before the rest is forwarded to the agent.
pub fn take_launch_id(args: Vec<String>) -> (Option<String>, Vec<String>) {
    take_flag_value(args, "--launch-id")
}

/// Pull the first `<flag> <value>` pair out of `args`, returning the value and
/// the remaining args. A trailing flag with no value just clears it. Shared by
/// the captain-miao-owned launcher flags (`--pool-session`, `--launch-id`) that
/// must be stripped before the rest is forwarded to the agent.
fn take_flag_value(args: Vec<String>, flag: &str) -> (Option<String>, Vec<String>) {
    let mut rest = Vec::with_capacity(args.len());
    let mut found = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == flag {
            found = it.next();
        } else {
            rest.push(a);
        }
    }
    (found, rest)
}

/// Run the launcher for `agent` from a raw positional arg list (as both binaries'
/// `claude`/`codex` arms receive it): strip the captain-miao-owned flags, split
/// off the cwd, canonicalize it, and hand the rest to the agent.
pub async fn run_launch(agent: AgentControl, args: Vec<String>) -> Result<()> {
    init_tracing("launcher");
    // The server injects `--pool-session <name>` when it starts a launcher inside
    // a remote pty pool, and the dashboard injects `--launch-id <token>` for a
    // local launcher it spawns; pull both out before the rest is split into cwd +
    // agent passthrough so neither reaches the agent.
    let (pool_session, args) = take_pool_session(args);
    let (launch_id, args) = take_launch_id(args);
    let (cwd, args) = split_cwd(args);
    let cwd = std::fs::canonicalize(&cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(cwd);
    crate::launcher::run(agent, &cwd, &args, pool_session, launch_id).await
}

/// Handle an agent hook event: parse the backend from the `--agent` value, then
/// forward the event to the launcher socket.
pub async fn run_hook(agent_cli: &str, event: &str, sock: Option<&str>) -> Result<()> {
    init_tracing("hook");
    let agent = AgentControl::from_cli(agent_cli)
        .ok_or_else(|| anyhow::anyhow!("unknown --agent: {agent_cli}"))?;
    crate::hooks::handle_event(agent, event, sock).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_pool_session_extracts_and_strips() {
        // `--pool-session <name>` is pulled out wherever it sits; the rest (cwd +
        // resume flags) is left untouched for split_cwd / the agent.
        let args = vec![
            "/work".into(),
            "--resume".into(),
            "sid".into(),
            "--pool-session".into(),
            "cm-claude-7-1".into(),
        ];
        let (pool, rest) = take_pool_session(args);
        assert_eq!(pool.as_deref(), Some("cm-claude-7-1"));
        assert_eq!(rest, vec!["/work", "--resume", "sid"]);
    }

    #[test]
    fn take_pool_session_absent_is_noop() {
        let args = vec!["/work".into(), "--resume".into(), "sid".into()];
        let (pool, rest) = take_pool_session(args.clone());
        assert!(pool.is_none());
        assert_eq!(rest, args);
    }
}
