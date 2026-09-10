use super::monitor::Observation;
use super::*;
use crate::state::SessionStatus as S;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = crate::state::runtime_dir().join(format!(
            "codex-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        crate::state::create_dir_all_private(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state() -> LauncherState {
    LauncherState {
        codex_mode: super::super::CodexMode::AppServer,
        ..LauncherState::for_test(AgentControl::Codex, S::Starting)
    }
}
fn request(m: &mut Monitor, s: &mut LauncherState, id: u64, method: &str) {
    m.apply(s, Observation::Client(json!({"id":id,"method":method})));
}
fn notify(m: &mut Monitor, s: &mut LauncherState, method: &str, params: Value) {
    m.apply(
        s,
        Observation::Server(json!({"method":method,"params":params})),
    );
}
fn resume(m: &mut Monitor, s: &mut LauncherState) {
    request(m, s, 1, "thread/resume");
    m.apply(s,Observation::Server(json!({"id":1,"result":{"model":"test-model","thread":{
        "id":"thread-root","sessionId":"tree-id","cwd":"/work","name":"Saved title","preview":"First prompt","status":{"type":"idle"}
    }}})));
}

#[test]
fn resume_restores_identity_before_input_and_replays_context() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    assert_eq!(s.status, S::Idle);
    assert_eq!(s.session_id.as_deref(), Some("thread-root"));
    assert_eq!(s.name.as_deref(), Some("Saved title"));
    assert_eq!(s.model.as_deref(), Some("test-model"));
    notify(
        &mut m,
        &mut s,
        "thread/tokenUsage/updated",
        json!({"threadId":"thread-root","tokenUsage":{"last":{"totalTokens":1234},"total":{"totalTokens":99999},"modelContextWindow":32000}}),
    );
    assert_eq!(s.context_tokens, Some(1234));
    assert_eq!(s.context_window, Some(32000));
    assert_eq!(s.codex_connected, Some(true));
}

#[test]
fn status_approvals_tools_compaction_and_queued_turns_follow_server_events() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    for (method, params, expected) in [
        ("turn/started", json!({"turn":{"id":"turn-one"}}), S::Active),
        (
            "item/commandExecution/requestApproval",
            json!({}),
            S::WaitingForApproval,
        ),
        (
            "thread/status/changed",
            json!({"status":{"type":"active","activeFlags":["waitingOnUserInput"]}}),
            S::WaitingForDecision,
        ),
        ("serverRequest/resolved", json!({}), S::Active),
        (
            "item/started",
            json!({"item":{"type":"contextCompaction"}}),
            S::Compacting,
        ),
        (
            "item/completed",
            json!({"item":{"type":"contextCompaction"}}),
            S::Compacted,
        ),
        (
            "turn/completed",
            json!({"turn":{"status":"interrupted"}}),
            S::Idle,
        ),
        (
            "turn/started",
            json!({"turn":{"id":"queued-turn"}}),
            S::Active,
        ),
    ] {
        let mut params = params;
        params["threadId"] = json!("thread-root");
        notify(&mut m, &mut s, method, params);
        assert_eq!(s.status, expected, "{method}");
    }
    notify(
        &mut m,
        &mut s,
        "item/started",
        json!({"threadId":"thread-root","item":{"type":"userMessage","content":[{"type":"text","text":"Queued prompt"}]}}),
    );
    assert_eq!(s.last_prompt.as_deref(), Some("Queued prompt"));
    assert_eq!(s.first_prompt.as_deref(), Some("First prompt"));
    notify(
        &mut m,
        &mut s,
        "item/started",
        json!({"threadId":"thread-root","item":{"type":"mcpToolCall","tool":"lookup"}}),
    );
    assert_eq!(s.last_tool.as_deref(), Some("lookup"));
}

#[test]
fn subagents_and_inventory_reads_never_replace_the_selected_root() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    let before = s.clone();
    notify(
        &mut m,
        &mut s,
        "thread/status/changed",
        json!({"threadId":"child","status":{"type":"active"}}),
    );
    request(&mut m, &mut s, 2, "thread/resume");
    m.apply(&mut s,Observation::Server(json!({"id":2,"result":{"thread":{"id":"child","parentThreadId":"thread-root","status":{"type":"active"}}}})));
    request(&mut m, &mut s, 3, "thread/read");
    m.apply(
        &mut s,
        Observation::Server(
            json!({"id":3,"result":{"thread":{"id":"unrelated","status":{"type":"idle"}}}}),
        ),
    );
    assert_eq!(s, before);
}

#[test]
fn reconnect_replaces_stale_status_and_fork_changes_identity() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    m.apply(&mut s, Observation::Disconnected);
    assert_eq!(s.status_label(), "Disconnected");
    assert_eq!(s.session_id.as_deref(), Some("thread-root"));
    resume(&mut m, &mut s);
    assert_eq!(s.status, S::Idle);
    assert!(s.last_error.is_none());
    request(&mut m, &mut s, 4, "thread/fork");
    m.apply(&mut s,Observation::Server(json!({"id":4,"result":{"thread":{"id":"fork","forkedFromId":"thread-root","status":{"type":"idle"}}}})));
    assert_eq!(s.session_id.as_deref(), Some("fork"));
}

#[test]
fn an_active_goal_holds_busy_between_turns_and_releases_when_paused() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    notify(
        &mut m,
        &mut s,
        "thread/goal/updated",
        json!({"threadId":"thread-root","goal":{"status":"active"}}),
    );
    assert_eq!(s.status, S::Active);
    notify(
        &mut m,
        &mut s,
        "turn/started",
        json!({"threadId":"thread-root","turn":{"id":"goal-turn"}}),
    );
    notify(
        &mut m,
        &mut s,
        "turn/completed",
        json!({"threadId":"thread-root","turn":{"status":"completed"}}),
    );
    assert_eq!(s.status, S::Active);
    notify(
        &mut m,
        &mut s,
        "thread/goal/updated",
        json!({"threadId":"thread-root","goal":{"status":"paused"}}),
    );
    assert_eq!(s.status, S::Idle);
}

#[test]
fn live_metadata_updates_and_unloading_are_reflected_without_input() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    notify(
        &mut m,
        &mut s,
        "thread/name/updated",
        json!({"threadId":"thread-root","threadName":"New title"}),
    );
    notify(
        &mut m,
        &mut s,
        "thread/settings/updated",
        json!({"threadId":"thread-root","threadSettings":{"model":"new-model"}}),
    );
    notify(
        &mut m,
        &mut s,
        "item/completed",
        json!({"threadId":"thread-root","item":{"type":"commandExecution","exitCode":2}}),
    );
    assert_eq!(s.name.as_deref(), Some("New title"));
    assert_eq!(s.model.as_deref(), Some("new-model"));
    assert_eq!(
        s.last_error.as_deref(),
        Some("Command exited with status 2")
    );
    notify(
        &mut m,
        &mut s,
        "thread/status/changed",
        json!({"threadId":"thread-root","status":{"type":"notLoaded"}}),
    );
    assert_eq!(s.status_label(), "Disconnected");
    resume(&mut m, &mut s);
    assert_eq!(s.status_label(), "Idle");
    assert!(s.last_error.is_none());
    notify(
        &mut m,
        &mut s,
        "thread/closed",
        json!({"threadId":"thread-root"}),
    );
    assert_eq!(s.status_label(), "Disconnected");
}

#[test]
fn parallel_tools_and_resolving_one_request_do_not_hide_another_approval() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    for id in ["one", "two"] {
        m.apply(&mut s, Observation::Server(json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread-root"}})));
    }
    notify(
        &mut m,
        &mut s,
        "item/started",
        json!({"threadId":"thread-root","item":{"type":"webSearch"}}),
    );
    assert_eq!(s.status, S::WaitingForApproval);
    notify(
        &mut m,
        &mut s,
        "serverRequest/resolved",
        json!({"threadId":"thread-root","requestId":"one"}),
    );
    assert_eq!(s.status, S::WaitingForApproval);
    notify(
        &mut m,
        &mut s,
        "serverRequest/resolved",
        json!({"threadId":"thread-root","requestId":"two"}),
    );
    assert_eq!(s.status, S::Active);
}

async fn receive(socket: &mut transport::Socket) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}
async fn send(socket: &mut transport::Socket, value: &Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn relay_preserves_bidirectional_protocol_and_accepts_reconnection() {
    let scratch = Scratch::new();
    let upstream = scratch.0.join("server.sock");
    let proxy = scratch.0.join("relay.sock");
    let listener = UnixListener::bind(&upstream).unwrap();
    let config = CodexConfig {
        mode: super::super::CodexMode::AppServer,
        endpoint: format!("unix://{}", upstream.display()),
    };
    let server = tokio::spawn(async move {
        // Preflight owns no thread and never subscribes to one.
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let initialize = receive(&mut ws).await;
        assert_eq!(initialize["method"], "initialize");
        send(&mut ws, &json!({"id":initialize["id"],"result":{}})).await;
        assert_eq!(receive(&mut ws).await["method"], "initialized");
        drop(ws);
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let resume = receive(&mut ws).await;
            assert_eq!(
                resume,
                json!({"id":8,"method":"thread/resume","params":{"threadId":"thread-root","futureField":"preserved"}})
            );
            send(
                &mut ws,
                &json!({"id":8,"result":{"thread":{"id":"thread-root","status":{"type":"idle"}}}}),
            )
            .await;
            send(&mut ws,&json!({"id":"approval","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-root","command":"build"}})).await;
            assert_eq!(
                receive(&mut ws).await,
                json!({"id":"approval","result":{"decision":"decline"}})
            );
            send(
                &mut ws,
                &json!({"method":"future/notification","params":{"new":"field"}}),
            )
            .await;
            ws.close(None).await.unwrap();
        }
    });
    let mut relay = Relay::start(&config, &proxy).await.unwrap();
    assert_eq!(
        std::fs::metadata(&proxy).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let (mut monitor, mut state) = (Monitor::default(), state());
    for _ in 0..2 {
        let mut tui = transport::connect(&proxy).await.unwrap();
        send(&mut tui,&json!({"id":8,"method":"thread/resume","params":{"threadId":"thread-root","futureField":"preserved"}})).await;
        assert_eq!(receive(&mut tui).await["id"], 8);
        assert_eq!(receive(&mut tui).await["id"], "approval");
        send(
            &mut tui,
            &json!({"id":"approval","result":{"decision":"decline"}}),
        )
        .await;
        assert_eq!(receive(&mut tui).await["method"], "future/notification");
        loop {
            let observation = tokio::time::timeout(Duration::from_secs(2), relay.events.recv())
                .await
                .unwrap()
                .unwrap();
            let disconnected = matches!(observation, Observation::Disconnected);
            monitor.apply(&mut state, observation);
            if disconnected {
                break;
            }
        }
        assert_eq!(state.session_id.as_deref(), Some("thread-root"));
        assert_eq!(state.codex_connected, Some(false));
    }
    server.await.unwrap();
    drop(relay);
    assert!(!proxy.exists());
}

#[tokio::test]
async fn stopping_pauses_the_goal_and_cleans_shells_even_if_the_turn_just_finished() {
    let scratch = Scratch::new();
    let path = scratch.0.join("stop.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let config = CodexConfig {
        endpoint: format!("unix://{}", path.display()),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let init = receive(&mut socket).await;
        send(&mut socket, &json!({"id":init["id"],"result":{}})).await;
        assert_eq!(receive(&mut socket).await["method"], "initialized");
        for (method, result) in [
            ("thread/goal/get", json!({"goal":{"status":"active"}})),
            ("thread/goal/set", json!({})),
            (
                "thread/turns/list",
                json!({"data":[{"id":"live-turn","status":"inProgress"}]}),
            ),
            ("turn/interrupt", json!({})),
            ("thread/backgroundTerminals/clean", json!({})),
        ] {
            let request = receive(&mut socket).await;
            assert_eq!(request["method"], method);
            assert_eq!(request["params"]["threadId"], "thread-root");
            if method == "thread/goal/set" {
                assert_eq!(
                    request["params"],
                    json!({"threadId":"thread-root","status":"paused"})
                );
            }
            if method == "turn/interrupt" {
                assert_eq!(request["params"]["turnId"], "live-turn");
                send(&mut socket,&json!({"id":request["id"],"error":{"code":-32600,"message":"turn already finished"}})).await;
            } else {
                send(&mut socket, &json!({"id":request["id"],"result":result})).await;
            }
        }
    });
    let mut state = state();
    state.session_id = Some("thread-root".into());
    assert!(stop(&config, &state, Some("old-turn")).await.is_err());
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inventory_uses_paginated_rpc_without_loading_threads() {
    let scratch = Scratch::new();
    let path = scratch.0.join("inventory.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let config = CodexConfig {
        endpoint: format!("unix://{}", path.display()),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let init = receive(&mut socket).await;
        send(&mut socket, &json!({"id":init["id"],"result":{}})).await;
        assert_eq!(receive(&mut socket).await["method"], "initialized");
        for page in 0..2 {
            let request = receive(&mut socket).await;
            assert_eq!(request["method"], "thread/list");
            assert_eq!(
                request["params"]["cursor"],
                if page == 0 {
                    Value::Null
                } else {
                    json!("next")
                }
            );
            send(&mut socket,&json!({"id":request["id"],"result":{
                "data":[{"id":format!("thread-{page}"),"cwd":"/work","name":"Title","preview":"Prompt","updatedAt":10}],
                "nextCursor":if page==0 {Some("next")} else {None}
            }})).await;
        }
    });
    let candidates = list_resumable(&config, 2).unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].custom_title.as_deref(), Some("Title"));
    assert_eq!(candidates[0].agent, AgentControl::Codex);
    server.await.unwrap();
}

#[test]
#[ignore = "requires CM_TEST_CODEX_ENDPOINT pointing to a running app-server"]
fn live_app_server_supports_read_only_inventory() {
    let endpoint = std::env::var("CM_TEST_CODEX_ENDPOINT").expect("set CM_TEST_CODEX_ENDPOINT");
    let config = CodexConfig {
        endpoint,
        ..Default::default()
    };
    let candidates = list_resumable(&config, 3).unwrap();
    assert!(candidates.len() <= 3);
    assert!(
        candidates
            .iter()
            .all(|c| c.agent == AgentControl::Codex && !c.session_id.is_empty())
    );
}
