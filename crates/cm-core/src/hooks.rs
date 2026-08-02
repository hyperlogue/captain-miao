use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::agent::AgentControl;
use crate::state::HookEvent;

/// Parse hook payload from stdin and forward to the launcher socket. `agent`
/// selects which backend's stdin parser to run (passed via `--agent`). The
/// socket comes from `--sock` when given, else `$CAPTAIN_MIAO_SOCK` — Codex
/// hooks rely on the env form so their `hooks.json` carries no per-session data
/// and Codex's trust prompt fires at most once.
pub async fn handle_event(agent: AgentControl, event: &str, sock_path: Option<&str>) -> Result<()> {
    let event = HookEvent::from_kebab(event)
        .ok_or_else(|| anyhow::anyhow!("Unknown hook event: {event}"))?;

    let sock_owned;
    let sock_path = match sock_path {
        Some(s) => s,
        None => {
            sock_owned = std::env::var("CAPTAIN_MIAO_SOCK")
                .map_err(|_| anyhow::anyhow!("no --sock and CAPTAIN_MIAO_SOCK unset"))?;
            &sock_owned
        }
    };

    let mut buf = String::new();
    tokio::io::stdin().read_to_string(&mut buf).await?;

    let msg = agent.parse_hook_payload(event, &buf)?;

    let json = serde_json::to_vec(&msg)?;
    tracing::debug!(
        "hook send pid={} agent={:?} event={:?} session={:?} tool={:?} bytes={} sock={}",
        std::process::id(),
        agent,
        msg.event,
        msg.session_id,
        msg.tool_name,
        json.len(),
        sock_path,
    );
    if let Ok(mut stream) = UnixStream::connect(sock_path).await {
        let _ = stream.write_all(&json).await;
        let _ = stream.shutdown().await;
    }

    Ok(())
}
