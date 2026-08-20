use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::agent::AgentControl;
use crate::state::HookEvent;

/// Parse hook payload from stdin and forward to the launcher socket. `agent`
/// selects which backend's stdin parser to run (passed via `--agent`). The
/// socket comes from `--sock` when given, else `$CAPTAIN_MIAO_SOCK` — Codex
/// hooks rely on the env form so their owned profile carries no per-session
/// data and its trust hashes stay stable.
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
    match UnixStream::connect(sock_path).await {
        Ok(mut stream) => {
            let _ = stream.write_all(&json).await;
            let _ = stream.shutdown().await;
        }
        // Never propagated: a hook that can't reach its launcher must not fail
        // the agent's turn. But it is no longer silent — see below.
        Err(e) => report_unreachable_launcher(sock_path, &e),
    }

    Ok(())
}

/// Record a hook that couldn't reach its launcher.
///
/// Failing to connect is usually benign: the session ended and the launcher
/// removed its socket while the agent was still firing teardown events. It is a
/// *fault* only when the launcher is **still alive** — the socket was unlinked
/// out from under a live session — because from that moment every status update
/// is dropped, the launcher keeps writing a state file that no longer reflects
/// reality, and the dashboard freezes on whatever the row last said. That
/// failure went undiagnosed for hours precisely because this path swallowed the
/// error, so the live-launcher case is written where someone chasing a stuck row
/// is already reading: that launcher's own log.
///
/// Written with plain file IO rather than `tracing` on purpose. The `hook` role
/// installs no file layer unless `[debug]` is on (see `logging::init_tracing`),
/// so a `warn!` alone would be discarded in every ordinary run — which is the
/// whole defect being fixed here. Best-effort throughout: diagnostics must never
/// be able to break a hook.
fn report_unreachable_launcher(sock_path: &str, err: &std::io::Error) {
    use std::io::Write as _;

    tracing::warn!("hook could not reach launcher socket {sock_path}: {err}");

    // Both the `--sock` argument and the `$CAPTAIN_MIAO_SOCK` fallback carry the
    // `<launcher-pid>.sock` form, so the owning launcher is recoverable here.
    let Some(pid) = std::path::Path::new(sock_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };
    // A dead launcher is the expected, uninteresting case — and its log is
    // already queued for sweeping by the next launcher's startup.
    if !crate::state::is_process_alive(pid) {
        return;
    }
    let path = crate::state::state_dir()
        .join("logs")
        .join(format!("launcher-{pid}.log"));
    let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) else {
        return;
    };
    // Epoch seconds rather than the surrounding RFC3339 lines: this crate has no
    // date-formatting dependency, and a distinct shape makes the line easy to
    // spot in a log that is otherwise all launcher-side events.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(
        f,
        "[t={ts}] HOOK DELIVERY FAILED: launcher {pid} is alive but {sock_path} \
         is unreachable ({err}). Status updates are being dropped; this row's \
         state is stale from here on."
    );
}
