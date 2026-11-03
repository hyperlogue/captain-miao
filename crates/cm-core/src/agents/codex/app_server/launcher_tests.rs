use crate::agent::AgentKind;
use crate::launcher::*;
use crate::state::{self, LauncherState, SessionStatus};
use std::time::Duration;
use tokio::net::UnixListener;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_cleanup_keeps_the_launcher_and_tui_alive_until_a_successful_retry() {
    cleanup_retry_survives(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_cleanup_remains_retryable_when_state_cannot_be_written() {
    cleanup_retry_survives(true).await;
}

async fn cleanup_retry_survives(block_state_write: bool) {
    use crate::agents::codex::{CodexConfig, CodexMode, app_server};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    if std::env::var_os("CM_TEST_CLEANUP_RETRY").is_none() {
        // State writes and failed-write fixtures belong to this child only.
        // A shared state reader can otherwise reap our synthetic launcher.
        // Keep socket paths short even with macOS's long default TMPDIR.
        let root = tempfile::Builder::new()
            .prefix("cm-cleanup-")
            .tempdir_in("/tmp")
            .unwrap();
        let test = if block_state_write {
            "agents::codex::app_server::launcher_tests::rejected_cleanup_remains_retryable_when_state_cannot_be_written"
        } else {
            "agents::codex::app_server::launcher_tests::rejected_cleanup_keeps_the_launcher_and_tui_alive_until_a_successful_retry"
        };
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("CM_TEST_CLEANUP_RETRY", "1")
            .env("XDG_STATE_HOME", root.path())
            .env("XDG_RUNTIME_DIR", root.path())
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "isolated cleanup retry failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let watchdog = app_server::CONTROL_TIMEOUT + Duration::from_secs(5);
    state::ensure_sessions_dir().unwrap();
    let root = state::runtime_dir().join(format!(
        "cleanup-test-{}-{block_state_write}",
        std::process::id()
    ));
    state::create_dir_all_private(&root).unwrap();
    let upstream = root.join("server.sock");
    let listener = UnixListener::bind(&upstream).unwrap();
    let config = CodexConfig {
        mode: CodexMode::AppServer,
        endpoint: format!("unix://{}", upstream.display()),
    };
    let server = tokio::spawn(async move {
        for attempt in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap();
                if method == "initialized" {
                    if attempt == 0 {
                        break;
                    }
                    continue;
                }
                let reply = if method == "thread/backgroundTerminals/clean" && attempt == 1 {
                    json!({"id":request["id"],"error":{"code":-32603,"message":"cleanup denied"}})
                } else {
                    let result = match method {
                        "initialize" | "thread/backgroundTerminals/clean" => json!({}),
                        "thread/goal/get" => json!({"goal":null}),
                        "thread/turns/list" => json!({"data":[]}),
                        _ => panic!("unexpected cleanup method {method}"),
                    };
                    json!({"id":request["id"],"result":result})
                };
                socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .unwrap();
                if method == "thread/backgroundTerminals/clean" {
                    break;
                }
            }
        }
    });
    let relay = app_server::Relay::start(&config, &root.join("relay.sock"))
        .await
        .unwrap();
    let control_path = root.join("control.sock");
    let control = app_server::Control::start(UnixListener::bind(&control_path).unwrap());
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut row = LauncherState::for_test(AgentKind::Codex, SessionStatus::Idle);
    row.launcher_pid = std::process::id();
    let state_path = state::sessions_dir().join(format!("{}.json", row.launcher_pid));
    if block_state_write {
        std::fs::create_dir(&state_path).unwrap();
    }
    row.child_pid = Some(pid);
    row.codex_mode = CodexMode::AppServer;
    row.codex_control = true;
    row.session_id = Some("test-thread".into());
    let _cleanup = CleanupGuard {
        launcher_pid: row.launcher_pid,
        sock_path: control_path.clone(),
        settings_path: root.join("unused.json"),
    };
    let supervisor = tokio::spawn(async move {
        let pool = row.pool_session.clone();
        app_server::supervise(
            &config,
            &mut row,
            child,
            relay,
            control,
            crate::agent::session::SessionPublisher::test_writer(),
            async move { crate::launcher::wait_until_minting_daemon_gone(pool.as_deref()).await },
        )
        .await
    });
    for attempt in 0..2 {
        let mut stream = tokio::net::UnixStream::connect(&control_path)
            .await
            .unwrap();
        crate::protocol::write_frame(&mut stream, &"Stop")
            .await
            .unwrap();
        let reply: Value = tokio::time::timeout(watchdog, crate::protocol::read_frame(&mut stream))
            .await
            .expect("cleanup control reply did not arrive before the watchdog expired")
            .unwrap()
            .unwrap();
        if attempt == 0 {
            assert!(reply["error"].as_str().unwrap().contains("cleanup denied"));
            assert!(!supervisor.is_finished());
            assert!(
                state::is_process_alive(pid),
                "a rejected stop must preserve the TUI"
            );
            if block_state_write {
                std::fs::remove_dir(&state_path).unwrap();
                tokio::time::timeout(watchdog, async {
                    loop {
                        if let Ok(bytes) = std::fs::read(&state_path)
                            && let Ok(saved) = serde_json::from_slice::<LauncherState>(&bytes)
                        {
                            assert!(saved.codex_control);
                            assert!(
                                saved
                                    .last_error
                                    .as_deref()
                                    .unwrap()
                                    .contains("cleanup denied")
                            );
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("dirty state must persist after the store recovers, without another event");
            }
        } else {
            assert!(reply["error"].is_null());
        }
    }
    assert_eq!(supervisor.await.unwrap().unwrap(), 0);
    assert!(!state::is_process_alive(pid));
    server.await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
