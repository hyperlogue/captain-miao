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
    let loaded: Vec<_> = pages
        .iter()
        .filter_map(|page| page["data"].as_array())
        .flatten()
        .cloned()
        .collect();
    let mut pages = pages.into_iter();
    let mut cursor = Value::Null;
    while let Some(Ok(Message::Text(text))) = socket.next().await {
        let request: Value = serde_json::from_str(&text).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        let response = match request["method"].as_str().unwrap() {
            "initialize" => json!({"id":id,"result":{"userAgent":"codex_cli_rs/0.163.0"}}),
            "thread/read" => json!({"id":id,"result":{"thread":{
                "id":request["params"]["threadId"],
                "status":{"type":if loaded.contains(&request["params"]["threadId"]) { "idle" } else { "notLoaded" }}
            }}}),
            "thread/loaded/list" => {
                assert_eq!(request["params"]["cursor"], cursor);
                let page = pages.next().expect("unexpected inventory request");
                cursor = page["nextCursor"].clone();
                json!({"id":id,"result":page})
            }
            "thread/goal/get"
            | "thread/turns/list"
            | "turn/interrupt"
            | "thread/backgroundTerminals/clean"
            | "thread/backgroundTerminals/list" => {
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
async fn disconnected_launcher_requires_ordered_absence_for_lost_requests() {
    if std::env::var_os("CM_TEST_CODEX_DISCONNECT_RECOVERY").is_none() {
        // Keep the supervisor's state writes separate from user sessions and
        // from other tests without changing this process's shared environment.
        let root = Scratch::new();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agents::codex::app_server::recovery_tests::disconnected_launcher_requires_ordered_absence_for_lost_requests", "--nocapture"])
            .env("CM_TEST_CODEX_DISCONNECT_RECOVERY", "1")
            .env("XDG_STATE_HOME", &root.0)
            .env("XDG_RUNTIME_DIR", &root.0)
            .output().await.unwrap();
        assert!(
            output.status.success(),
            "isolated recovery failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Drive the real relay and acknowledged Stop path. A lost reply can name
    // work on another thread, or a creation whose new identity never arrived.
    for (method, loaded, succeeds) in [
        ("turn/start", json!([]), true),
        ("thread/resume", json!(["unrelated-thread"]), true),
        ("thread/resume", json!(["lost-thread"]), false),
        // Inventory is unsequenced: an in-flight creation can load a runtime
        // after this empty snapshot, even after its connection has closed.
        ("thread/start", json!([]), false),
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
            // The connection closes after receiving a mutation but before replying.
            // The daemon remains alive; only an ordered read can settle known work.
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

#[tokio::test]
async fn kill_and_restart_verify_cleanup_after_a_lost_reply() {
    use crate::backend::CleanupPolicy;

    const TEST: &str = "agents::codex::app_server::recovery_tests::kill_and_restart_verify_cleanup_after_a_lost_reply";
    if std::env::var_os("CM_TEST_CODEX_LOST_REPLY_KILL").is_none() {
        let root = Scratch::new();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("CM_TEST_CODEX_LOST_REPLY_KILL", "1")
            .env("XDG_STATE_HOME", &root.0)
            .env("XDG_RUNTIME_DIR", &root.0)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for (method, lost_thread, reject_cleanup, removes, policy) in [
        ("thread/resume", "saved-thread", false, true),
        ("turn/start", "saved-thread", false, true),
        ("turn/steer", "saved-thread", false, true),
        ("thread/goal/set", "saved-thread", false, true),
        ("thread/compact/start", "saved-thread", false, false),
        ("thread/resume", "saved-thread", true, false),
        ("turn/start", "other-thread", false, false),
        ("thread/start", "saved-thread", false, false),
    ]
    .into_iter()
    .flat_map(|(method, thread, reject, removes)| {
        [CleanupPolicy::Required, CleanupPolicy::ForceIfUnavailable]
            .map(move |policy| (method, thread, reject, removes, policy))
    }) {
        let root = Scratch::new();
        let (config, listener) = root.server();
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let mut cleaned = false;
            // Preflight, followed by a TUI request whose reply is lost.
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init = receive(&mut socket).await;
            send(&mut socket, json!({"id":init["id"],"result":{}})).await;
            assert_eq!(receive(&mut socket).await["method"], "initialized");
            drop(socket);
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert_eq!(receive(&mut socket).await["method"], method);
            // Also exercise a daemon-side EOF without a WebSocket close frame.
            if method != "turn/steer" {
                socket.close(None).await.unwrap();
            }
            drop(socket);
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let Some(id) = request.get("id") else {
                        continue;
                    };
                    let method = request["method"].as_str().unwrap();
                    let result = match method {
                        // A future major version gets the same recovery and
                        // cleanup checks as the currently audited releases.
                        "initialize" => json!({"userAgent":"codex_cli_rs/1.0.0"}),
                        "thread/read" => json!({"thread":{
                            "id":request["params"]["threadId"],
                            "status":{"type":if cleaned { "idle" } else { "active" }}
                        }}),
                        "thread/loaded/list" => json!({"data":["saved-thread","other-thread"]}),
                        "thread/goal/get" => json!({"goal":{"status":"active"}}),
                        "thread/goal/set" => {
                            assert_eq!(request["params"]["status"], "paused");
                            json!({})
                        }
                        "thread/turns/list" => {
                            json!({"data":[{"id":"current-turn","status":"inProgress"}]})
                        }
                        "turn/interrupt" => {
                            assert_eq!(request["params"]["turnId"], "current-turn");
                            json!({})
                        }
                        "thread/backgroundTerminals/terminate" => {
                            cleaned = true;
                            json!({"terminated":!reject_cleanup})
                        }
                        "thread/backgroundTerminals/list" => {
                            if cleaned {
                                json!({"data":[]})
                            } else {
                                json!({"data":[{"processId":"42"}]})
                            }
                        }
                        other => panic!("unexpected cleanup RPC: {other}"),
                    };
                    if !matches!(
                        method,
                        "initialize"
                            | "thread/loaded/list"
                            | "thread/read"
                            | "thread/backgroundTerminals/list"
                    ) {
                        assert_eq!(request["params"]["threadId"], "saved-thread");
                        calls_tx.send(method.to_owned()).unwrap();
                    }
                    let response = json!({"id":id,"result":result});
                    send(&mut socket, response).await;
                }
            }
        });
        let proxy = root.0.join("relay.sock");
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut tui = transport::connect(&proxy).await.unwrap();
        send(
            &mut tui,
            json!({"id":1,"method":method,"params":{"threadId":lost_thread}}),
        )
        .await;
        wait_for_disconnect(&mut relay).await;
        drop(tui);
        let child = tokio::process::Command::new("sleep")
            .arg("120")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let runtime = crate::state::runtime_dir().join("launchers");
        crate::state::create_dir_all_private(&runtime).unwrap();
        let control_path = runtime.join(format!("{pid}.sock"));
        let control = Control::start(UnixListener::bind(&control_path).unwrap());
        let mut row = disconnected_row();
        row.launcher_pid = pid;
        row.child_pid = Some(pid);
        row.codex_control = true;
        crate::state::ensure_sessions_dir().unwrap();
        row.write().unwrap();
        let snapshot = row.clone();
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
        // Both Kill and Restart require the same verified normal cleanup.
        let result = tokio::time::timeout(
            CONTROL_TIMEOUT + Duration::from_secs(10),
            tokio::task::spawn_blocking(move || kill(&snapshot, policy)),
        )
        .await
        .expect("awaiting verified cleanup")
        .unwrap();
        if result.is_ok() {
            supervisor.await.unwrap().unwrap();
            assert!(
                !crate::state::is_process_alive(pid),
                "successful cleanup must reap the TUI"
            );
        } else {
            assert!(
                crate::state::is_process_alive(pid),
                "failed cleanup must preserve control"
            );
            supervisor.abort();
            let _ = supervisor.await;
        }
        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_file(control_path);
        let _ = std::fs::remove_file(crate::state::sessions_dir().join(format!("{pid}.json")));
        assert_eq!(
            result.is_ok(),
            removes,
            "lost {method} on {lost_thread}, reject={reject_cleanup}, policy={policy:?}: {result:?}"
        );
        let mut calls = Vec::new();
        while let Ok(call) = calls_rx.try_recv() {
            calls.push(call);
        }
        if removes || reject_cleanup {
            assert_eq!(
                calls,
                [
                    "thread/goal/get",
                    "thread/goal/set",
                    "thread/turns/list",
                    "turn/interrupt",
                    "thread/backgroundTerminals/terminate"
                ]
            );
        } else {
            assert!(
                calls.is_empty(),
                "unrelated or unidentified work must block cleanup"
            );
        }
    }
}

#[tokio::test]
async fn downstream_disconnect_waits_for_upstream_receipt() {
    for graceful in [false, true] {
        let root = Scratch::new();
        let (config, listener) = root.server();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init = receive(&mut socket).await;
            send(&mut socket, json!({"id":init["id"],"result":{}})).await;
            assert_eq!(receive(&mut socket).await["method"], "initialized");
            drop(socket);
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            accepted_tx.send(()).unwrap();
            read_rx.await.unwrap();
            assert_eq!(receive(&mut socket).await["method"], "thread/resume");
            let frame = socket.next().await.unwrap().unwrap();
            let Message::Ping(receipt) = frame else {
                panic!("missing upstream receipt request: {frame:?}")
            };
            // Withhold the mutation's reply. A receipt permits an ordered read,
            // not a claim that the mutation completed.
            socket.send(Message::Pong(receipt)).await.unwrap();
            let _ = socket.next().await;
        });
        let proxy = root.0.join("relay.sock");
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut tui = transport::connect(&proxy).await.unwrap();
        accepted_rx.await.unwrap();
        send(
            &mut tui,
            json!({"id":1,"method":"thread/resume","params":{"threadId":"saved-thread"}}),
        )
        .await;
        // Observation is emitted before the relay writes upstream. This keeps
        // the test independent of whether the daemon has read its socket yet.
        tokio::time::timeout(Duration::from_secs(10), relay.events.recv())
            .await
            .unwrap()
            .unwrap();
        if graceful {
            tui.close(None).await.unwrap();
        }
        drop(tui);
        let disconnected =
            tokio::time::timeout(Duration::from_millis(50), relay.events.recv()).await;
        read_tx.send(()).unwrap();
        assert!(
            disconnected.is_err(),
            "relay declared a disconnect before upstream consumed the mutation"
        );
        wait_for_disconnect(&mut relay).await;
        let error = relay.pause_input(true).await.unwrap_err();
        let unsettled = error.downcast_ref::<relay::UnsettledThreads>().unwrap();
        assert_eq!(
            unsettled
                .threads_to_fence(Some(std::process::id() as i32))
                .unwrap(),
            ["saved-thread".to_owned()].into()
        );
        drop(relay);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn cleanup_waits_for_confirmed_termination_and_rescans_shifted_pages() {
    let root = Scratch::new();
    let (config, listener) = root.server();
    let (listed_tx, listed_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut listed = Some(listed_tx);
        let mut release = Some(release_rx);
        let mut remaining = std::collections::VecDeque::from(["42", "43"]);
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let request: Value = serde_json::from_str(&text).unwrap();
            let Some(id) = request.get("id") else {
                continue;
            };
            let result = match request["method"].as_str().unwrap() {
                "initialize" => json!({"userAgent":"codex_cli_rs/0.163.0"}),
                "thread/goal/get" => json!({"goal":null}),
                "thread/turns/list" => json!({"data":[]}),
                "thread/read" => json!({"thread":{"id":"saved-thread","status":{"type":"idle"}}}),
                "thread/backgroundTerminals/list" => {
                    assert!(
                        request["params"]["cursor"].is_null(),
                        "deletion invalidates the previous page cursor"
                    );
                    if let Some(id) = remaining.front() {
                        json!({"data":[{"processId":id}],"nextCursor":if remaining.len() > 1 {json!("next")} else {Value::Null}})
                    } else {
                        json!({"data":[]})
                    }
                }
                "thread/backgroundTerminals/terminate" => {
                    assert_eq!(
                        request["params"]["processId"],
                        remaining.pop_front().unwrap()
                    );
                    if let Some(listed) = listed.take() {
                        listed.send(()).unwrap();
                        if let Some(release) = release.take() {
                            release.await.unwrap();
                        }
                    }
                    json!({"terminated":true})
                }
                method => panic!("unexpected RPC: {method}"),
            };
            send(&mut socket, json!({"id":id,"result":result})).await;
        }
    });
    let cleanup = tokio::spawn(async move { stop(&config, &disconnected_row(), None).await });
    tokio::time::timeout(Duration::from_secs(10), listed_rx)
        .await
        .expect("waiting for background-terminal termination request")
        .unwrap();
    assert!(
        !cleanup.is_finished(),
        "removal from the list does not replace a termination acknowledgement"
    );
    release_tx.send(()).unwrap();
    tokio::time::timeout(CLEANUP_TIMEOUT + Duration::from_secs(10), cleanup)
        .await
        .expect("waiting for actual terminal cleanup")
        .unwrap()
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cleanup_accounts_for_pending_and_lost_picker_mutations() {
    for lost in [false, true] {
        let root = Scratch::new();
        let (config, listener) = root.server();
        let (primary_tx, primary_rx) = tokio::sync::oneshot::channel();
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
            let mut primary = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = receive(&mut primary).await;
            send(&mut primary, json!({"id":request["id"],"result":{"thread":{"id":"main","status":{"type":"idle"}}}})).await;
            primary_tx.send(()).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let mut picker = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = receive(&mut picker).await;
            assert_eq!(request["method"], "thread/resume");
            forwarded_tx.send(()).unwrap();
            release_rx.await.unwrap();
            if lost {
                picker.close(None).await.unwrap();
            } else {
                send(&mut picker, json!({"id":request["id"],"result":{"thread":{"id":"other","status":{"type":"idle"}}}})).await;
            }
            let _ = primary.next().await;
        });
        let proxy = root.0.join("relay.sock");
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut primary = transport::connect(&proxy).await.unwrap();
        send(
            &mut primary,
            json!({"id":1,"method":"thread/resume","params":{"threadId":"main"}}),
        )
        .await;
        primary_rx.await.unwrap();
        assert_eq!(receive(&mut primary).await["id"], 1);
        let mut picker = transport::connect(&proxy).await.unwrap();
        send(
            &mut picker,
            json!({"id":1,"method":"thread/resume","params":{"threadId":"other"}}),
        )
        .await;
        forwarded_rx.await.unwrap();
        let mut pause = relay.pause_input(true);
        let early = tokio::time::timeout(Duration::from_millis(50), &mut pause).await;
        release_tx.send(()).unwrap();
        assert!(
            early.is_err(),
            "primary pause must not overtake a pending auxiliary mutation"
        );
        let result = tokio::time::timeout(Duration::from_secs(10), pause)
            .await
            .expect("waiting for picker fence");
        if lost {
            let error = result.unwrap_err();
            let threads = error.downcast_ref::<relay::UnsettledThreads>().unwrap();
            assert_eq!(
                threads
                    .threads_to_fence(Some(std::process::id() as i32))
                    .unwrap(),
                ["other".to_owned()].into()
            );
        } else {
            result.unwrap();
        }
        let mut monitor = Monitor::default();
        let mut row = disconnected_row();
        while let Ok(event) = relay.events.try_recv() {
            monitor.apply(&mut row, event);
        }
        assert_eq!(
            row.session_id.as_deref(),
            Some("main"),
            "picker responses must not retarget the row"
        );
        drop(primary);
        drop(picker);
        drop(relay);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn recovery_rejects_unsupported_versions_and_invalid_thread_reads() {
    for (agent, response) in [
        (
            "unknown",
            json!({"result":{"thread":{"id":"saved-thread","status":{"type":"idle"}}}}),
        ),
        (
            "codex_cli_rs/0.153.3",
            json!({"result":{"thread":{"id":"saved-thread","status":{"type":"idle"}}}}),
        ),
        (
            "codex_cli_rs/0.163.0",
            json!({"result":{"thread":{"id":"wrong-thread","status":{"type":"notLoaded"}}}}),
        ),
        (
            "codex_cli_rs/0.163.0",
            json!({"result":{"thread":{"id":"saved-thread","status":{"type":"unknown"}}}}),
        ),
        (
            "codex_cli_rs/1.0.0",
            json!({"error":{"code":-32601,"message":"method not found"}}),
        ),
    ] {
        let root = Scratch::new();
        let (config, listener) = root.server();
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
            let init = receive(&mut socket).await;
            send(
                &mut socket,
                json!({"id":init["id"],"result":{"userAgent":agent}}),
            )
            .await;
            assert_eq!(receive(&mut socket).await["method"], "initialized");
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    request["method"], "thread/read",
                    "unverified recovery must not attempt cleanup"
                );
                let mut reply = response.clone();
                reply["id"] = request["id"].clone();
                send(&mut socket, reply).await;
            }
        });
        let proxy = root.0.join("relay.sock");
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut tui = transport::connect(&proxy).await.unwrap();
        send(
            &mut tui,
            json!({"id":1,"method":"turn/start","params":{"threadId":"saved-thread"}}),
        )
        .await;
        wait_for_disconnect(&mut relay).await;
        let paused = relay.pause_input(true).await;
        assert!(
            stop_after_quiescence(&config, &disconnected_row(), None, paused, true)
                .await
                .is_err(),
            "unverified recovery succeeded for {agent}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn ordered_reads_confirm_only_the_exact_unmaterialized_thread_is_gone() {
    for (missing, code, absent) in [
        ("saved-thread", -32600, true),
        ("other-thread", -32600, false),
        ("saved-thread", -32603, false),
    ] {
        let root = Scratch::new();
        let (_, listener) = root.server();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init = receive(&mut socket).await;
            send(
                &mut socket,
                json!({"id":init["id"],"result":{"userAgent":"codex_cli_rs/0.163.0"}}),
            )
            .await;
            assert_eq!(receive(&mut socket).await["method"], "initialized");
            let read = receive(&mut socket).await;
            assert_eq!(read["method"], "thread/read");
            send(&mut socket, json!({"id":read["id"],"error":{"code":code,"message":format!("thread not loaded: {missing}")}})).await;
        });
        let mut client = transport::Client::connect(&root.0.join("server.sock"))
            .await
            .unwrap();
        let result = read_thread_status(&mut client, "saved-thread").await;
        assert_eq!(result.is_ok_and(|status| status == "notLoaded"), absent);
        server.await.unwrap();
    }
}
