#![cfg(feature = "pty-pool")]

//! Process-level lifecycle coverage: real launchers, daemon, state watcher and
//! host protocol; a scripted Codex peer supplies deterministic failure timing.
//! No agent login, terminal emulator, SSH host or user session is involved.
mod support;

use cm_core::backend::{CleanupPolicy, ForcedRemoval};
use cm_core::protocol::{ClientFrame, ServerFrame};
use cm_core::state::{CleanupStatus, SessionStatus};
use serde_json::json;
use support::{CodexServer, Host, Peer, WAIT, until};

#[tokio::test]
async fn sessions_recover_and_remain_controllable_across_connection_and_cleanup_failures() {
    let mut host = Host::new();
    let mut codex = CodexServer::start(host.codex_socket()).await;
    host.start();
    let alpha = host.launch("alpha", true);
    let beta = host.launch("beta", true);
    let native = host.launch("native", false);
    let mut peer = Peer::connect(host.socket()).await;
    peer.rows_until(|rows| {
        rows.len() == 3
            && rows.values().all(|s| {
                s.session_id.is_some()
                    && s.status == SessionStatus::Idle
                    && (s.codex_mode.is_native() || s.codex_connected == Some(true))
            })
    })
    .await;
    let alpha_tui = peer.row("alpha").child_pid.unwrap();
    let beta_tui = peer.row("beta").child_pid.unwrap();
    let native_tui = peer.row("native").child_pid.unwrap();
    let alpha_key = peer.row("alpha").key();
    let beta_key = peer.row("beta").key();
    assert_eq!(peer.row("alpha").name.as_deref(), Some("Title alpha"));
    assert_eq!(peer.row("native").status, SessionStatus::Idle);

    // A helper shares the TUI connection. Its identity, prompt and closure
    // must never replace any managed conversation, including while idle.
    codex.helpers();
    peer.rows_until(|rows| {
        rows.values().filter(|s| !s.codex_mode.is_native()).count() == 2
            && rows
                .values()
                .filter(|s| !s.codex_mode.is_native())
                .all(|s| {
                    s.name
                        .as_deref()
                        .is_some_and(|name| name.starts_with("After helper "))
                        && s.codex_connected == Some(true)
                })
    })
    .await;
    assert_eq!(
        peer.row("alpha").name.as_deref(),
        Some("After helper alpha")
    );
    assert_eq!(
        peer.row("alpha").last_prompt.as_deref(),
        Some("Prompt alpha")
    );
    assert_eq!(peer.row("beta").last_prompt.as_deref(), Some("Prompt beta"));

    // Dropping the host connection does not end any launcher. A new
    // subscription must give the complete, correctly identified snapshot.
    drop(peer);
    let mut peer = Peer::connect(host.socket()).await;
    peer.rows_until(|rows| rows.len() == 3).await;
    assert_eq!(peer.row("alpha").key(), alpha_key);
    assert_eq!(peer.row("beta").key(), beta_key);
    host.assert_alive(&[alpha, beta, native]);

    codex.activate("alpha");
    codex.activate("beta");
    peer.rows_until(|rows| {
        rows.values()
            .filter(|s| !s.codex_mode.is_native())
            .all(|s| s.status == SessionStatus::Active)
    })
    .await;

    // Hold one cleanup reply until unrelated changes and a fragmented host
    // request have both arrived. Readiness comes from the actual RPC arrival.
    let release = codex.hold_cleanup("alpha");
    peer.send(ClientFrame::KillSession {
        req_id: 10,
        key: alpha_key.clone(),
        cleanup: CleanupPolicy::Required,
    })
    .await;
    codex
        .wait_for("thread/backgroundTerminals/clean", "alpha")
        .await;
    let requests = codex.requests();
    let position = |method| {
        requests
            .iter()
            .position(|(m, id)| m == method && id == "alpha")
            .unwrap()
    };
    assert!(position("thread/goal/set") < position("turn/interrupt"));
    assert!(position("turn/interrupt") < position("thread/backgroundTerminals/clean"));
    codex.rename("beta", "Still responsive");
    peer.fragmented_check(11).await;
    peer.rows_until(|rows| {
        rows.values()
            .any(|s| s.name.as_deref() == Some("Still responsive"))
    })
    .await;
    peer.finish_fragment().await;
    assert_eq!(peer.row("beta").status, SessionStatus::Active);
    assert!(matches!(
        peer.reply(11).await,
        ServerFrame::DirChecked { exists: true, .. }
    ));
    release.notify_one();
    assert!(matches!(
        peer.reply(10).await,
        ServerFrame::Killed {
            ok: false,
            error: Some(_),
            forced: None,
            ..
        }
    ));
    peer.rows_until(|rows| {
        rows.get(&alpha_key)
            .is_some_and(|s| matches!(s.cleanup, Some(CleanupStatus::Failed { .. })))
    })
    .await;
    host.assert_alive(&[alpha, beta, native]);

    // A rejected explicit Kill must also retain the session; then a retry can
    // succeed. Failure is never quietly turned into a forced removal.
    codex.reject_cleanup("alpha");
    peer.send(ClientFrame::KillSession {
        req_id: 12,
        key: alpha_key.clone(),
        cleanup: CleanupPolicy::ForceIfUnavailable,
    })
    .await;
    assert!(matches!(
        peer.reply(12).await,
        ServerFrame::Killed {
            ok: false,
            error: Some(_),
            ..
        }
    ));
    host.assert_alive(&[alpha, beta, native]);
    codex.allow_cleanup("alpha");

    // Lose the entire app-server, wait for observed disconnection, then bring
    // it back. The fixture reconnects/resumes exactly as a client would, with
    // no turn/start replay. The native session is unaffected.
    codex.stop().await;
    peer.rows_until(|rows| {
        rows.values()
            .filter(|s| !s.codex_mode.is_native())
            .all(|s| s.codex_connected == Some(false))
    })
    .await;
    assert_eq!(peer.row("native").status, SessionStatus::Idle);
    codex = CodexServer::start(host.codex_socket()).await;
    peer.rows_until(|rows| {
        rows.values()
            .filter(|s| !s.codex_mode.is_native())
            .all(|s| s.codex_connected == Some(true))
    })
    .await;
    assert_eq!(peer.row("alpha").session_id.as_deref(), Some("alpha"));
    assert!(
        matches!(
            peer.row("alpha").cleanup,
            Some(CleanupStatus::Failed { .. })
        ),
        "reconnection must not erase the previous cleanup failure"
    );
    assert_eq!(peer.row("beta").last_prompt.as_deref(), Some("Prompt beta"));
    assert!(
        !codex
            .requests()
            .iter()
            .any(|(method, _)| method == "turn/start")
    );

    // Restart uses strict cleanup, then launches a replacement on the saved
    // conversation. Observe removal before replacement to expose stale rows.
    peer.send(ClientFrame::KillSession {
        req_id: 13,
        key: alpha_key.clone(),
        cleanup: CleanupPolicy::Required,
    })
    .await;
    assert!(matches!(
        peer.reply(13).await,
        ServerFrame::Killed {
            ok: true,
            forced: None,
            error: None,
            ..
        }
    ));
    peer.rows_until(|rows| !rows.contains_key(&alpha_key)).await;
    host.wait_exited(alpha).await;
    assert!(
        !cm_core::state::is_process_alive(alpha_tui),
        "stopped TUI was not reaped"
    );
    let replacement = host.launch("alpha", true);
    peer.rows_until(|rows| {
        rows.values()
            .any(|s| s.session_id.as_deref() == Some("alpha") && s.launcher_pid == replacement)
    })
    .await;
    assert_eq!(
        peer.row("alpha").last_prompt.as_deref(),
        Some("Prompt alpha")
    );
    assert_ne!(peer.row("alpha").key(), alpha_key);
    let replacement_tui = peer.row("alpha").child_pid.unwrap();
    host.assert_alive(&[replacement, beta, native]);

    // Missing and unreachable are distinct successful removals on the wire.
    codex.missing_cleanup("beta");
    peer.send(ClientFrame::KillSession {
        req_id: 14,
        key: beta_key.clone(),
        cleanup: CleanupPolicy::ForceIfUnavailable,
    })
    .await;
    assert!(matches!(
        peer.reply(14).await,
        ServerFrame::Killed {
            ok: true,
            forced: Some(ForcedRemoval::ThreadMissing),
            error: None,
            ..
        }
    ));
    peer.rows_until(|rows| !rows.contains_key(&beta_key)).await;
    host.wait_exited(beta).await;
    assert!(
        !cm_core::state::is_process_alive(beta_tui),
        "removed TUI was not reaped"
    );
    codex.stop().await;
    let replacement_key = peer.row("alpha").key();
    peer.rows_until(|rows| {
        rows.get(&replacement_key)
            .is_some_and(|s| s.codex_connected == Some(false))
    })
    .await;
    peer.send(ClientFrame::KillSession {
        req_id: 15,
        key: replacement_key.clone(),
        cleanup: CleanupPolicy::ForceIfUnavailable,
    })
    .await;
    assert!(matches!(
        peer.reply(15).await,
        ServerFrame::Killed {
            ok: true,
            forced: Some(ForcedRemoval::AppServerUnreachable),
            error: None,
            ..
        }
    ));
    peer.rows_until(|rows| !rows.contains_key(&replacement_key))
        .await;
    host.wait_exited(replacement).await;
    assert!(
        !cm_core::state::is_process_alive(replacement_tui),
        "force-removed TUI was not reaped"
    );

    // Restarting the host daemon must rediscover a surviving direct launcher.
    // Pool upgrades intentionally use the existing stop-and-resume workflow.
    drop(peer);
    host.restart();
    let mut peer = Peer::connect(host.socket()).await;
    peer.rows_until(|rows| {
        rows.len() == 1
            && rows
                .values()
                .any(|s| s.session_id.as_deref() == Some("native"))
    })
    .await;
    let native_key = peer.row("native").key();
    peer.send(ClientFrame::KillSession {
        req_id: 16,
        key: native_key,
        cleanup: CleanupPolicy::Required,
    })
    .await;
    assert!(matches!(
        peer.reply(16).await,
        ServerFrame::Killed {
            ok: true,
            forced: None,
            ..
        }
    ));
    peer.rows_until(|rows| rows.is_empty()).await;
    host.wait_exited(native).await;
    assert!(
        !cm_core::state::is_process_alive(native_tui),
        "native TUI was not reaped"
    );
    until("launcher sockets removed", || host.launcher_sockets_gone()).await;
}

// Spawned only by the temporary codex executable in Host. The normal test
// invocation returns immediately; no real agent executable is ever invoked.
#[tokio::test]
async fn codex_fixture() {
    let Ok(id) = std::env::var("CM_LIFECYCLE_THREAD") else {
        return;
    };
    let endpoint = std::env::var("CM_LIFECYCLE_RELAY").unwrap();
    if endpoint.is_empty() {
        use std::io::Write;
        for event in ["session-start", "stop"] {
            let mut hook = std::process::Command::new(env!("CARGO_BIN_EXE_miao-server"))
                .args(["hook", event, "--agent", "codex"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            hook.stdin
                .take()
                .unwrap()
                .write_all(json!({"session_id":id}).to_string().as_bytes())
                .unwrap();
            assert!(hook.wait().unwrap().success());
        }
        std::future::pending::<()>().await;
    }
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let connection = tokio::time::timeout(WAIT, async {
            let stream =
                tokio::net::UnixStream::connect(endpoint.strip_prefix("unix://").unwrap()).await?;
            tokio_tungstenite::client_async("ws://localhost/", stream)
                .await
                .map(|(ws, _)| ws)
                .map_err(std::io::Error::other)
        })
        .await;
        if let Ok(Ok(mut ws)) = connection {
            let result = async {
                ws.send(Message::Text(json!({"id":1,"method":"initialize","params":{}}).to_string().into())).await?;
                ws.next().await.ok_or(tokio_tungstenite::tungstenite::Error::ConnectionClosed)??;
                ws.send(Message::Text(json!({"method":"initialized"}).to_string().into())).await?;
                ws.send(Message::Text(json!({"id":2,"method":"thread/resume","params":{"threadId":id}}).to_string().into())).await?;
                // Internal-helper requests originate on this same connection.
                while let Some(message) = ws.next().await {
                    let message = message?;
                    if let Message::Text(text) = message {
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if value["method"] == "fixture/helpers" {
                            ws.send(Message::Text(json!({"id":3,"method":"thread/start","params":{"ephemeral":true,"threadSource":"system"}}).to_string().into())).await?;
                        }
                    }
                }
                Ok::<(), tokio_tungstenite::tungstenite::Error>(())
            }.await;
            let _ = result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
