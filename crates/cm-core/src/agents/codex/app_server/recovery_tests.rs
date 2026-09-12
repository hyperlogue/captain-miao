use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "miao-codex-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        crate::state::create_dir_all_private(&root).unwrap();
        Self(root)
    }
    fn server(&self) -> (CodexConfig, UnixListener) {
        let path = self.0.join("server.sock");
        (
            CodexConfig {
                mode: super::super::CodexMode::AppServer,
                endpoint: format!("unix://{}", path.display()),
            },
            UnixListener::bind(path).unwrap(),
        )
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn disconnected_row() -> LauncherState {
    let mut row =
        LauncherState::for_test(AgentControl::Codex, crate::state::SessionStatus::Starting);
    row.codex_mode = super::super::CodexMode::AppServer;
    row.codex_connected = Some(false);
    row.session_id = Some("saved-thread".into());
    row
}

async fn receive(socket: &mut transport::Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

async fn send(socket: &mut transport::Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn wait_for_disconnect(relay: &mut Relay) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match relay.events.recv().await.expect("relay must remain alive") {
                monitor::Observation::Disconnected => break,
                _ => continue,
            }
        }
    })
    .await
    .unwrap();
}

async fn serve_cleanup(mut socket: transport::Socket, pages: Vec<Value>) {
    let mut pages = pages.into_iter();
    let mut cursor = Value::Null;
    while let Some(Ok(Message::Text(text))) = socket.next().await {
        let request: Value = serde_json::from_str(&text).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        let response = match request["method"].as_str().unwrap() {
            "initialize" => json!({"id":id,"result":{}}),
            "thread/loaded/list" => {
                assert_eq!(request["params"]["cursor"], cursor);
                let page = pages.next().expect("unexpected inventory request");
                cursor = page["nextCursor"].clone();
                json!({"id":id,"result":page})
            }
            "thread/goal/get"
            | "thread/turns/list"
            | "turn/interrupt"
            | "thread/backgroundTerminals/clean" => {
                json!({"id":id,"error":{"code":-32600,"message":"thread not found: saved-thread"}})
            }
            method => panic!("unexpected recovery RPC: {method}"),
        };
        send(&mut socket, response).await;
    }
}

#[tokio::test]
async fn disconnected_codex_session_can_stop_after_daemon_restart() {
    let root = Scratch::new();
    let (config, listener) = root.server();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        serve_cleanup(socket, vec![json!({"data":[]})]).await;
    });
    let result = stop(&config, &disconnected_row(), Some("stale-turn")).await;
    server.await.unwrap();
    assert!(
        result.is_ok(),
        "an unloaded thread must be removable: {result:?}"
    );
}

#[tokio::test]
async fn cleanup_requires_complete_valid_inventory_to_confirm_absence() {
    for (pages, absent) in [
        (
            vec![
                json!({"data":["other-thread"],"nextCursor":"page-2"}),
                json!({"data":[]}),
            ],
            true,
        ),
        (
            vec![
                json!({"data":["other-thread"],"nextCursor":"page-2"}),
                json!({"data":["saved-thread"]}),
            ],
            false,
        ),
        (vec![json!({"data":[null]})], false),
        (vec![json!({})], false),
        (vec![json!({"data":[],"nextCursor":1})], false),
        (
            vec![
                json!({"data":[],"nextCursor":"repeated"}),
                json!({"data":[],"nextCursor":"repeated"}),
            ],
            false,
        ),
    ] {
        let root = Scratch::new();
        let (config, listener) = root.server();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_cleanup(
                tokio_tungstenite::accept_async(stream).await.unwrap(),
                pages,
            )
            .await;
        });
        let result = stop(&config, &disconnected_row(), None).await;
        server.await.unwrap();
        assert_eq!(result.is_ok(), absent, "{result:?}");
    }
    let root = Scratch::new();
    let (config, listener) = root.server();
    drop(listener);
    assert!(
        stop(&config, &disconnected_row(), None).await.is_err(),
        "an unreachable daemon does not prove that its work ended"
    );
}

#[tokio::test]
async fn lost_replies_do_not_let_cleanup_overtake_a_reconnected_request() {
    let root = Scratch::new();
    let (config, listener) = root.server();
    let (forwarded_tx, forwarded_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let init = receive(&mut socket).await;
        send(&mut socket, json!({"id":init["id"],"result":{}})).await;
        assert_eq!(receive(&mut socket).await["method"], "initialized");
        drop(socket);
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        assert_eq!(receive(&mut socket).await["method"], "turn/start");
        socket.close(None).await.unwrap();
        drop(socket);
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let request = receive(&mut socket).await;
        assert_eq!(request["method"], "thread/resume");
        forwarded_tx.send(()).unwrap();
        release_rx.await.unwrap();
        send(
            &mut socket,
            json!({"id":request["id"],"result":{"thread":{
                "id":"resumed-thread","status":{"type":"idle"}
            }}}),
        )
        .await;
        let _ = socket.next().await;
    });
    let proxy = root.0.join("relay.sock");
    let mut relay = Relay::start(&config, &proxy).await.unwrap();
    let mut tui = transport::connect(&proxy).await.unwrap();
    send(
        &mut tui,
        json!({"id":1,"method":"turn/start","params":{"threadId":"lost-thread"}}),
    )
    .await;
    wait_for_disconnect(&mut relay).await;
    drop(tui);
    let mut tui = transport::connect(&proxy).await.unwrap();
    send(
        &mut tui,
        json!({"id":2,"method":"thread/resume","params":{"threadId":"resumed-thread"}}),
    )
    .await;
    forwarded_rx.await.unwrap();
    let mut pause = relay.pause_input(true);
    let early = tokio::time::timeout(Duration::from_millis(30), &mut pause).await;
    release_tx.send(()).unwrap();
    assert_eq!(receive(&mut tui).await["id"], 2);
    if early.is_err() {
        assert!(
            pause.await.is_err(),
            "the older lost reply still needs accounting for"
        );
    }
    drop(tui);
    drop(relay);
    server.abort();
    assert!(
        early.is_err(),
        "cleanup must await the new thread identity even when an older reply was lost"
    );
}

#[tokio::test]
async fn disconnected_codex_launcher_recovers_lost_requests_after_daemon_restart() {
    if std::env::var_os("CM_TEST_CODEX_DISCONNECT_RECOVERY").is_none() {
        // Keep the supervisor's state writes separate from user sessions and
        // from other tests without changing this process's shared environment.
        let root = Scratch::new();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agents::codex::app_server::recovery_tests::disconnected_codex_launcher_recovers_lost_requests_after_daemon_restart", "--nocapture"])
            .env("CM_TEST_CODEX_DISCONNECT_RECOVERY", "1")
            .env("XDG_STATE_HOME", &root.0)
            .env("XDG_RUNTIME_DIR", &root.0)
            .output().await.unwrap();
        assert!(
            output.status.success(),
            "isolated recovery failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    // Drive the real relay and acknowledged Stop path. A lost reply can name
    // work on another thread, or a creation whose new identity never arrived.
    for (method, loaded, succeeds) in [
        ("turn/start", json!([]), true),
        ("thread/resume", json!(["unrelated-thread"]), true),
        ("thread/resume", json!(["lost-thread"]), false),
        ("thread/start", json!([]), true),
        ("thread/start", json!(["unrelated-thread"]), false),
    ] {
        let root = Scratch::new();
        let (config, listener) = root.server();
        let server = tokio::spawn(async move {
            // Relay startup's read-only preflight.
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init = receive(&mut socket).await;
            send(&mut socket, json!({"id":init["id"],"result":{}})).await;
            assert_eq!(receive(&mut socket).await["method"], "initialized");
            drop(socket);
            // The daemon dies after receiving a mutation but before replying.
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert_eq!(receive(&mut socket).await["method"], method);
            socket.close(None).await.unwrap();
            drop(socket);
            while let Ok((stream, _)) = listener.accept().await {
                serve_cleanup(
                    tokio_tungstenite::accept_async(stream).await.unwrap(),
                    vec![json!({"data":loaded})],
                )
                .await;
            }
        });
        let proxy = root.0.join("relay.sock");
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut tui = transport::connect(&proxy).await.unwrap();
        send(
            &mut tui,
            json!({"id":1,"method":method,"params":{"threadId":"lost-thread"}}),
        )
        .await;
        wait_for_disconnect(&mut relay).await;
        let control_path = root.0.join("control.sock");
        let control = Control::start(UnixListener::bind(&control_path).unwrap());
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let mut row = disconnected_row();
        row.launcher_pid = pid;
        row.child_pid = Some(pid);
        row.codex_control = true;
        let supervisor = tokio::spawn(async move {
            supervise(
                &config,
                &mut row,
                child,
                relay,
                control,
                std::future::pending(),
            )
            .await
        });
        let mut control = tokio::net::UnixStream::connect(&control_path)
            .await
            .unwrap();
        crate::protocol::write_frame(&mut control, &"Stop")
            .await
            .unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            crate::protocol::read_frame::<_, Value>(&mut control),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let success = reply["error"].is_null();
        if success {
            assert_eq!(supervisor.await.unwrap().unwrap(), 0);
            assert!(!crate::state::is_process_alive(pid));
        } else {
            assert!(crate::state::is_process_alive(pid));
            supervisor.abort();
            let _ = supervisor.await;
        }
        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_file(crate::state::sessions_dir().join(format!("{pid}.json")));
        assert_eq!(success, succeeds, "lost {method}: {reply}");
    }
}
