//! App-server adapter. A per-launcher relay observes the real Codex TUI's RPC
//! stream, preserving its full input, approval, configuration and reconnect
//! behavior. Only the launcher reduces observations into its own state file.
//! No hook profile, rollout reader or SQLite connection belongs to this mode.
mod control;
mod lifecycle;
mod monitor;
mod relay;
mod transport;

use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use super::CodexConfig;
use crate::agent::{AgentControl, ResumeCandidate};
use crate::state::LauncherState;
pub(crate) use control::{Control, StopRequest, request_stop};
pub(crate) use lifecycle::supervise;
pub(crate) use monitor::Monitor;
pub(crate) use relay::Relay;

// One budget shared by launcher cleanup, host acknowledgement and replacement
// handoff. Leave room for fencing input, error recovery and flushing a reply.
pub(crate) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(35);
pub(crate) const CONTROL_TIMEOUT: Duration =
    CLEANUP_TIMEOUT.saturating_add(Duration::from_secs(25));
pub(super) const HANDOFF_TIMEOUT: Duration =
    CONTROL_TIMEOUT.saturating_add(Duration::from_secs(30));

pub fn list_resumable(config: &CodexConfig, limit: usize) -> Result<Vec<ResumeCandidate>> {
    let path = config.socket_path()?;
    transport::blocking(async move {
        let mut client = transport::Client::connect(&path).await?;
        let mut candidates = Vec::new();
        let mut cursor = None;
        while candidates.len() < limit {
            let result = client
                .request(
                    "thread/list",
                    json!({
                        "limit": (limit-candidates.len()).min(100), "cursor":cursor,
                        "sortKey":"updated_at", "useStateDbOnly":true, "archived":false,
                        "sourceKinds":["cli"]
                    }),
                )
                .await?;
            for thread in result["data"]
                .as_array()
                .context("invalid Codex thread list")?
            {
                let Some(id) = thread["id"].as_str() else {
                    continue;
                };
                let Some(cwd) = thread["cwd"].as_str() else {
                    continue;
                };
                candidates.push(ResumeCandidate {
                    agent: AgentControl::Codex,
                    session_id: id.into(),
                    cwd: cwd.into(),
                    first_prompt: thread["preview"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                    custom_title: thread["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                    git_branch: thread["gitInfo"]["branch"].as_str().map(str::to_owned),
                    mtime: UNIX_EPOCH
                        + Duration::from_secs(thread["updatedAt"].as_u64().unwrap_or(0)),
                });
            }
            let next = result["nextCursor"].as_str().map(str::to_owned);
            if next.is_none() || next == cursor {
                break;
            }
            cursor = next;
        }
        candidates.truncate(limit);
        Ok(candidates)
    })
}

/// The selected thread is the unit of termination. Never signal the shared
/// daemon, and never archive/delete conversation history as a way to stop work.
pub(crate) async fn stop(
    config: &CodexConfig,
    state: &LauncherState,
    turn: Option<&str>,
) -> Result<()> {
    let Some(id) = state.session_id.as_deref() else {
        return Ok(());
    };
    let mut client = transport::Client::connect(&config.socket_path()?).await?;
    // A goal can start another turn after an interrupt. Pause its continuation
    // before ending the turn, preserving its objective and budget for resume.
    let goal = client
        .request("thread/goal/get", json!({"threadId":id}))
        .await;
    let paused = if goal
        .as_ref()
        .is_ok_and(|goal| goal["goal"]["status"] == "active")
    {
        client
            .request("thread/goal/set", json!({"threadId":id,"status":"paused"}))
            .await
            .map(|_| ())
    } else {
        match goal {
            // Codex 0.153.4 returns this explicit absence when goals are disabled.
            // There is then no continuation to pause; other errors must surface.
            Err(error)
                if error
                    .downcast_ref::<transport::RpcError>()
                    .is_some_and(|rpc| {
                        rpc.code == Some(-32600) && rpc.message == "goals feature is disabled"
                    }) =>
            {
                Ok(())
            }
            other => other.map(|_| ()),
        }
    };
    // A rejoined thread can already be busy before this TUI receives events.
    let latest = client
        .request(
            "thread/turns/list",
            json!({"threadId":id,"limit":1,"sortDirection":"desc"}),
        )
        .await;
    let current = latest
        .as_ref()
        .ok()
        .and_then(|v| v["data"].as_array())
        .and_then(|turns| turns.iter().find(|t| t["status"] == "inProgress"))
        .and_then(|t| t["id"].as_str());
    let interrupt =
        if let Some(turn) = current.or_else(|| latest.is_err().then_some(turn).flatten()) {
            // Codex acknowledges this only after TurnAborted, so teardown
            // finishes before the replacement launcher passes its handoff gate.
            client
                .request("turn/interrupt", json!({"threadId":id,"turnId":turn}))
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
    // Cleanup still runs if the turn finished between the read and interrupt.
    client
        .request("thread/backgroundTerminals/clean", json!({"threadId":id}))
        .await?;
    paused.and(latest.map(|_| ())).and(interrupt)
}

pub(crate) fn command(
    cwd: &str,
    args: &[String],
    socket: &Path,
    shim: Option<&Path>,
) -> Result<tokio::process::Command> {
    anyhow::ensure!(
        !args
            .iter()
            .any(|arg| arg == "--remote" || arg.starts_with("--remote=")),
        "Codex --remote is owned by the host's app-server setting"
    );
    let mut command = crate::agents::common::agent_command(super::BIN, cwd, shim)?;
    command
        .arg("--remote")
        .arg(format!("unix://{}", socket.display()));
    command.arg("--cd").arg(cwd);
    command.args(args);
    Ok(command)
}

#[cfg(test)]
mod tests;
