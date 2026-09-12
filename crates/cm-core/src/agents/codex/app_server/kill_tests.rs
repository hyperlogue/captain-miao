use crate::agent::AgentControl;
use crate::backend::{CleanupPolicy, LocalBackend};
use crate::state::{self, LauncherState, SessionStatus};
use serde_json::{Value, json};
use tokio::net::UnixListener;

#[tokio::test]
async fn a_slow_cleanup_rpc_does_not_make_a_responsive_server_unreachable() {
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;
    let root =
        std::path::Path::new("/tmp").join(format!("miao-kill-timeout-{}", std::process::id()));
    state::create_dir_all_private(&root).unwrap();
    let socket = root.join("server.sock");
    let config = super::super::CodexConfig {
        mode: super::super::CodexMode::AppServer,
        endpoint: format!("unix://{}", socket.display()),
    };
    let listener = UnixListener::bind(&socket).unwrap();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut received = Some(received_tx);
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"id":init["id"],"result":{}}).to_string().into(),
            ))
            .await
            .unwrap();
            let initialized: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
            if let Some(received) = received.take() {
                let request: Value =
                    serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(request["method"], "thread/backgroundTerminals/clean");
                received.send(()).unwrap();
                // Keep this request pending until its client times out.
                let _ = ws.next().await;
            }
        }
    });
    let request = tokio::spawn(async move {
        let mut client = super::transport::Client::connect(&socket).await.unwrap();
        client
            .request(
                "thread/backgroundTerminals/clean",
                json!({"threadId":"saved-thread"}),
            )
            .await
            .unwrap_err()
    });
    received_rx.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();
    let error = request.await.unwrap();
    let reason = super::kill::force_removal_reason(&config, &error, Some("saved-thread")).await;
    if reason.is_none() {
        server.await.unwrap();
    } else {
        server.abort();
        let _ = server.await;
    }
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        reason.is_none(),
        "a responsive daemon must retain failed cleanup: {reason:?}"
    );
}

#[tokio::test]
async fn kill_qualifies_cleanup_failures_and_reaps_the_tui() {
    use super::super::{CodexConfig, CodexMode};
    use futures_util::{SinkExt, StreamExt};
    use std::time::Duration;

    const TEST: &str =
        "agents::codex::app_server::kill_tests::kill_qualifies_cleanup_failures_and_reaps_the_tui";
    if std::env::var_os("CM_TEST_CODEX_KILL_RUNTIME").is_none() {
        for scenario in ["unreachable", "missing", "rejected"] {
            let root =
                std::path::Path::new("/tmp").join(format!("miao-kill-live-{}", std::process::id()));
            state::create_dir_all_private(&root).unwrap();
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("CM_TEST_CODEX_KILL_RUNTIME", "1")
                .env("CM_TEST_CODEX_KILL_SCENARIO", scenario)
                .env("XDG_STATE_HOME", &root)
                .env("XDG_RUNTIME_DIR", &root)
                .output()
                .await
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let runtime = state::runtime_dir().join("launchers");
    state::create_dir_all_private(&runtime).unwrap();
    let socket = state::runtime_dir().join("codex.sock");
    if std::env::var_os("CM_TEST_CODEX_KILL_LAUNCHER").is_some() {
        let config = CodexConfig {
            mode: CodexMode::AppServer,
            endpoint: format!("unix://{}", socket.display()),
        };
        let pid = std::process::id();
        let relay = super::Relay::start(&config, &runtime.join(format!("{pid}-codex.sock")))
            .await
            .unwrap();
        let control =
            super::Control::start(UnixListener::bind(runtime.join(format!("{pid}.sock"))).unwrap());
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let tui = child.id().unwrap();
        let mut row = LauncherState::for_test(AgentControl::Codex, SessionStatus::Starting);
        row.launcher_pid = pid;
        row.child_pid = Some(tui);
        row.launch_id = Some("supervised-kill-fixture".into());
        row.session_id = Some("saved-thread".into());
        row.codex_mode = CodexMode::AppServer;
        row.codex_control = true;
        row.codex_connected = Some(false);
        super::supervise(
            &config,
            &mut row,
            child,
            relay,
            control,
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(
            !state::is_process_alive(tui),
            "the launcher must reap its TUI before exiting"
        );
        return;
    }
    let scenario = std::env::var("CM_TEST_CODEX_KILL_SCENARIO").unwrap();
    let removes = scenario != "rejected";
    let listener = UnixListener::bind(&socket).unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut ready = Some(ready_tx);
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"id":request["id"], "result":{}}).to_string().into(),
            ))
            .await
            .unwrap();
            let request: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "initialized");
            if let Some(ready) = ready.take() {
                if scenario == "unreachable" {
                    // Leave a refused socket, just as a daemon crash can do.
                    drop(listener);
                    ready.send(()).unwrap();
                    return;
                }
                ready.send(()).unwrap();
                continue;
            }
            while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                ws.next().await
            {
                let request: Value = serde_json::from_str(&text).unwrap();
                let id = &request["id"];
                let reply = match request["method"].as_str().unwrap() {
                    "thread/goal/get" => json!({"id":id,"result":{"goal":null}}),
                    "thread/turns/list" => json!({"id":id,"result":{"data":[]}}),
                    "thread/backgroundTerminals/clean" => json!({"id":id,"error":{
                        "code":if scenario == "missing" { -32600 } else { -32603 },
                        "message":"thread not found: saved-thread"
                    }}),
                    // Older Codex daemons may not expose loaded-thread inventory.
                    "thread/loaded/list" => {
                        json!({"id":id,"error":{"code":-32601,"message":"method not found"}})
                    }
                    method => panic!("unexpected cleanup method {method}"),
                };
                ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    reply.to_string().into(),
                ))
                .await
                .unwrap();
            }
        }
    });
    let launcher = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env("CM_TEST_CODEX_KILL_LAUNCHER", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = launcher.id().unwrap();
    ready_rx.await.unwrap();
    let path = state::sessions_dir().join(format!("{pid}.json"));
    let row: LauncherState = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(row) = serde_json::from_slice(&bytes)
            {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let key = row.key();
    let strict = tokio::task::spawn_blocking(move || {
        LocalBackend::kill_session(&key, CleanupPolicy::Required)
    })
    .await
    .unwrap();
    assert!(
        strict.is_err(),
        "restart must not abandon unconfirmed cleanup"
    );
    assert!(state::is_process_alive(pid) && state::is_process_alive(row.child_pid.unwrap()));
    assert!(path.exists());
    let key = row.key();
    let forced = tokio::task::spawn_blocking(move || {
        LocalBackend::kill_session(&key, CleanupPolicy::ForceIfUnavailable)
    })
    .await
    .unwrap();
    assert_eq!(forced.is_ok(), removes, "{forced:?}");
    if !removes {
        assert!(state::is_process_alive(pid) && state::is_process_alive(row.child_pid.unwrap()));
        assert!(path.exists());
        // End only this test fixture after checking that Kill preserved it.
        tokio::task::spawn_blocking(move || super::control::force_stop(pid))
            .await
            .unwrap()
            .unwrap();
    }
    let output = launcher.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!state::is_process_alive(row.child_pid.unwrap()));
    assert_eq!(!path.exists(), removes);
    assert_eq!(!runtime.join(format!("{pid}.sock")).exists(), removes);
    assert!(!runtime.join(format!("{pid}-codex.sock")).exists());
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn kill_removes_a_missing_thread_with_an_older_launcher() {
    if std::env::var_os("CM_TEST_CODEX_FORCE_KILL").is_none() {
        // Isolate persisted state and signals from other tests and real sessions.
        // Keep Unix socket paths short on macOS too.
        let root = std::path::Path::new("/tmp").join(format!("miao-kill-{}", std::process::id()));
        state::create_dir_all_private(&root).unwrap();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agents::codex::app_server::kill_tests::kill_removes_a_missing_thread_with_an_older_launcher", "--nocapture"])
            .env("CM_TEST_CODEX_FORCE_KILL", "1")
            .env("XDG_STATE_HOME", &root)
            .env("XDG_RUNTIME_DIR", &root)
            .output().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    for (message, policy, remove, replace_owner) in [
        (
            "Codex cleanup failed: Codex thread/backgroundTerminals/clean: thread not found: saved-thread",
            CleanupPolicy::ForceIfUnavailable,
            true,
            false,
        ),
        (
            "Codex cleanup failed: connecting to Codex app-server socket: Connection refused (os error 111)",
            CleanupPolicy::ForceIfUnavailable,
            true,
            false,
        ),
        (
            "Codex cleanup failed: Codex thread/backgroundTerminals/clean timed out: deadline has elapsed",
            CleanupPolicy::ForceIfUnavailable,
            false,
            false,
        ),
        (
            "Codex cleanup failed: Codex thread/backgroundTerminals/clean: thread not found: saved-thread",
            CleanupPolicy::Required,
            false,
            false,
        ),
        (
            "Codex cleanup failed: connecting to Codex app-server socket: Connection refused (os error 111)",
            CleanupPolicy::Required,
            false,
            false,
        ),
        (
            "Codex cleanup failed: Codex thread/backgroundTerminals/clean: thread not found: other-thread",
            CleanupPolicy::ForceIfUnavailable,
            false,
            false,
        ),
        (
            "Codex cleanup failed: Codex thread/backgroundTerminals/clean: cleanup denied",
            CleanupPolicy::ForceIfUnavailable,
            false,
            false,
        ),
        (
            "Codex launcher did not confirm cleanup in time",
            CleanupPolicy::ForceIfUnavailable,
            false,
            false,
        ),
        (
            "Codex cleanup failed: Codex thread/backgroundTerminals/clean: thread not found: saved-thread",
            CleanupPolicy::ForceIfUnavailable,
            false,
            true,
        ),
    ] {
        let mut launcher = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut tui = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut row = LauncherState::for_test(AgentControl::Codex, SessionStatus::Starting);
        row.launcher_pid = launcher.id().unwrap();
        row.child_pid = tui.id();
        row.launch_id = Some("kill-fixture".into());
        row.session_id = Some("saved-thread".into());
        row.codex_mode = super::super::CodexMode::AppServer;
        row.codex_connected = Some(false);
        row.codex_control = true;
        state::ensure_sessions_dir().unwrap();
        row.write().unwrap();
        let runtime = state::runtime_dir().join("launchers");
        state::create_dir_all_private(&runtime).unwrap();
        let control_path = runtime.join(format!("{}.sock", row.launcher_pid));
        let control = UnixListener::bind(&control_path).unwrap();
        let relay_path = runtime.join(format!("{}-codex.sock", row.launcher_pid));
        let _relay = UnixListener::bind(&relay_path).unwrap();
        let changed = row.clone();
        let responder = tokio::spawn(async move {
            let (mut stream, _) = control.accept().await.unwrap();
            let request: Value = crate::protocol::read_frame(&mut stream)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request, "Stop");
            if replace_owner {
                LauncherState {
                    launch_id: Some("replacement-fixture".into()),
                    ..changed
                }
                .write()
                .unwrap();
            }
            crate::protocol::write_frame(&mut stream, &json!({"error":message}))
                .await
                .unwrap();
        });
        let key = row.key();
        let result = tokio::task::spawn_blocking(move || LocalBackend::kill_session(&key, policy))
            .await
            .unwrap();
        responder.await.unwrap();
        let removed = !state::sessions_dir()
            .join(format!("{}.json", row.launcher_pid))
            .exists()
            && !control_path.exists()
            && !relay_path.exists();
        let exited = launcher.try_wait().unwrap().is_some() && tui.try_wait().unwrap().is_some();
        // Always reap the fixtures, including on a failing regression assertion.
        let _ = launcher.kill().await;
        let _ = tui.kill().await;
        assert_eq!(
            result.is_ok(),
            remove,
            "cleanup result for {policy:?}: {result:?}"
        );
        assert_eq!(removed, remove, "state and sockets for {message}");
        assert_eq!(exited, remove, "launcher and TUI for {message}");
    }
}
