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
    observe_request(m, s, json!({"id":id,"method":method}));
}
fn observe_request(m: &mut Monitor, s: &mut LauncherState, value: Value) {
    if let Some(observation) = Observation::client(&value) {
        m.apply(s, observation);
    }
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
fn model_output_after_compaction_restores_active_status() {
    for kind in ["reasoning", "agentMessage", "plan"] {
        for completed in [false, true] {
            let (mut m, mut s) = (Monitor::default(), state());
            resume(&mut m, &mut s);
            notify(
                &mut m,
                &mut s,
                "turn/started",
                json!({"threadId":"thread-root","turn":{"id":"turn-one"}}),
            );
            notify(
                &mut m,
                &mut s,
                "item/started",
                json!({"threadId":"thread-root","turnId":"turn-one","item":{"id":"compact-one","type":"contextCompaction"}}),
            );
            assert_eq!(s.status, S::Compacting);
            if completed {
                notify(
                    &mut m,
                    &mut s,
                    "item/completed",
                    json!({"threadId":"thread-root","turnId":"turn-one","item":{"id":"compact-one","type":"contextCompaction"}}),
                );
                assert_eq!(s.status, S::Compacted);
            }
            notify(
                &mut m,
                &mut s,
                "item/started",
                json!({"threadId":"thread-root","turnId":"turn-one","item":{"id":"output-one","type":kind}}),
            );
            assert_eq!(s.status, S::Active, "{kind}, completed={completed}");
            assert!(s.last_tool.is_none());
        }
    }
}

#[test]
fn model_output_does_not_hide_pending_requests() {
    for (method, expected) in [
        (
            "item/commandExecution/requestApproval",
            S::WaitingForApproval,
        ),
        ("item/tool/requestUserInput", S::WaitingForDecision),
    ] {
        let (mut m, mut s) = (Monitor::default(), state());
        resume(&mut m, &mut s);
        m.apply(
            &mut s,
            Observation::Server(
                json!({"id":"request-one","method":method,"params":{"threadId":"thread-root"}}),
            ),
        );
        for kind in ["reasoning", "agentMessage", "plan"] {
            notify(
                &mut m,
                &mut s,
                "item/started",
                json!({"threadId":"thread-root","item":{"id":"output-one","type":kind}}),
            );
            assert_eq!(s.status, expected, "{kind}, {method}");
        }
    }
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
fn catchup_thread_cannot_replace_the_managed_conversation() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    notify(
        &mut m,
        &mut s,
        "item/started",
        json!({"threadId":"thread-root","item":{"type":"userMessage","content":[{"type":"text","text":"Actual user prompt"}]}}),
    );
    notify(
        &mut m,
        &mut s,
        "turn/started",
        json!({"threadId":"thread-root","turn":{"id":"user-turn"}}),
    );
    let before = s.clone();
    observe_request(
        &mut m,
        &mut s,
        json!({"id":2,"method":"thread/start","params":{"ephemeral":true,"threadSource":"system"}}),
    );
    m.apply(
        &mut s,
        Observation::Server(json!({"id":2,"result":{"thread":{
            "id":"catchup-thread","parentThreadId":null,"ephemeral":true,"threadSource":"system",
            "name":null,"preview":"","status":{"type":"idle"}
        }}})),
    );
    notify(
        &mut m,
        &mut s,
        "item/started",
        json!({"threadId":"catchup-thread","item":{"type":"userMessage","content":[{"type":"text","text":"Write a brief catch-up"}]}}),
    );
    notify(
        &mut m,
        &mut s,
        "thread/tokenUsage/updated",
        json!({"threadId":"catchup-thread","tokenUsage":{"last":{"totalTokens":8000}}}),
    );
    notify(
        &mut m,
        &mut s,
        "thread/closed",
        json!({"threadId":"catchup-thread"}),
    );
    assert_eq!(s, before);
    assert_eq!(m.turn.as_deref(), Some("user-turn"));
}

#[test]
fn background_thread_snapshots_and_failed_starts_leave_the_row_unchanged() {
    let (mut m, mut s) = (Monitor::default(), state());
    resume(&mut m, &mut s);
    s.last_error = Some("Existing conversation error".into());
    let before = s.clone();
    // A read/resume request need not carry the source; the response still must
    // identify a user conversation before it can replace the row.
    for source in [
        "system",
        "subagent",
        "guardian_review",
        "memory_consolidation",
    ] {
        for method in [
            "thread/start",
            "thread/resume",
            "thread/fork",
            "thread/read",
        ] {
            request(&mut m, &mut s, 2, method);
            m.apply(
                &mut s,
                Observation::Server(json!({"id":2,"result":{"thread":{
                    "id":"thread-root","threadSource":source,"parentThreadId":null,
                    "name":"Internal task","status":{"type":"idle"}
                }}})),
            );
            assert_eq!(s, before, "{source} {method}");
        }
        observe_request(
            &mut m,
            &mut s,
            json!({"id":"helper","method":"thread/start","params":{"threadSource":source}}),
        );
        m.apply(
            &mut s,
            Observation::Server(
                json!({"id":"helper","error":{"code":-32600,"message":"Internal task failed"}}),
            ),
        );
        assert_eq!(s, before, "{source} error");
    }
}

#[test]
fn user_and_legacy_lifecycle_responses_can_select_ephemeral_conversations() {
    for source in [json!("user"), Value::Null] {
        for ephemeral in [true, false] {
            for method in ["thread/start", "thread/resume", "thread/fork"] {
                let (mut m, mut s) = (Monitor::default(), state());
                resume(&mut m, &mut s);
                m.apply(&mut s, Observation::Disconnected);
                observe_request(
                    &mut m,
                    &mut s,
                    json!({"id":2,"method":method,"params":{"threadSource":source,"ephemeral":ephemeral}}),
                );
                m.apply(&mut s, Observation::Server(json!({"id":2,"result":{"thread":{
                    "id":"selected-thread","threadSource":source,"ephemeral":ephemeral,
                    "name":"Selected conversation","preview":"Selected prompt","status":{"type":"idle"}
                }}})));
                assert_eq!(s.session_id.as_deref(), Some("selected-thread"));
                assert_eq!(s.name.as_deref(), Some("Selected conversation"));
                assert_eq!(s.codex_connected, Some(true));
                assert_eq!(s.status, S::Idle);
                assert!(s.last_error.is_none());
            }
        }
    }
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
                &json!({"id":8,"result":{"thread":{
                    "id":"thread-root","name":"Saved title","status":{"type":"idle"},
                    "turns":[{"items":[{"type":"userMessage","content":[{"type":"text","text":"Actual user prompt"}]}]}]
                }}}),
            )
            .await;
            send(&mut ws,&json!({"id":"approval","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-root","command":"build"}})).await;
            assert_eq!(
                receive(&mut ws).await,
                json!({"id":"approval","result":{"decision":"decline"}})
            );
            // Helper traffic must reach Codex unchanged, but never select the
            // row. Omit response classification to exercise request filtering
            // through the real relay (where parameters used to be stripped).
            assert_eq!(
                receive(&mut ws).await,
                json!({"id":"temporary-structured","method":"thread/start","params":{
                    "threadSource":"system","ephemeral":true,"config":{"futureField":true}
                }})
            );
            for frame in [
                json!({"id":"temporary-structured","result":{"thread":{"id":"catchup-thread","status":{"type":"idle"}}}}),
                json!({"method":"item/started","params":{"threadId":"catchup-thread","item":{"type":"userMessage","content":[{"type":"text","text":"Write a brief catch-up"}]}}}),
                json!({"method":"thread/closed","params":{"threadId":"catchup-thread"}}),
            ] {
                send(&mut ws, &frame).await;
            }
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
        relay.pause_input(true).await.unwrap();
        send(&mut tui,&json!({"id":8,"method":"thread/resume","params":{"threadId":"thread-root","futureField":"preserved"}})).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), tui.next())
                .await
                .is_err(),
            "a pending cleanup must prevent new TUI requests from reaching the server"
        );
        relay.pause_input(false).await.unwrap();
        assert_eq!(receive(&mut tui).await["id"], 8);
        assert_eq!(receive(&mut tui).await["id"], "approval");
        send(
            &mut tui,
            &json!({"id":"approval","result":{"decision":"decline"}}),
        )
        .await;
        send(
            &mut tui,
            &json!({"id":"temporary-structured","method":"thread/start","params":{
                "threadSource":"system","ephemeral":true,"config":{"futureField":true}
            }}),
        )
        .await;
        assert_eq!(receive(&mut tui).await["id"], "temporary-structured");
        assert_eq!(receive(&mut tui).await["method"], "item/started");
        assert_eq!(receive(&mut tui).await["method"], "thread/closed");
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
        assert_eq!(state.name.as_deref(), Some("Saved title"));
        assert_eq!(state.last_prompt.as_deref(), Some("Actual user prompt"));
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

#[tokio::test]
async fn disabled_goals_allow_cleanup_but_real_goal_errors_are_reported() {
    for message in ["goals feature is disabled", "goal store unavailable"] {
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
            for method in [
                "thread/goal/get",
                "thread/turns/list",
                "thread/backgroundTerminals/clean",
            ] {
                let request = receive(&mut socket).await;
                assert_eq!(request["method"], method);
                let reply = if method == "thread/goal/get" {
                    json!({"id":request["id"],"error":{"code":-32600,"message":message}})
                } else {
                    json!({"id":request["id"],"result":{"data":[]}})
                };
                send(&mut socket, &reply).await;
            }
        });
        let mut row = state();
        row.session_id = Some("test-thread".into());
        let result = stop(&config, &row, None).await;
        if message == "goals feature is disabled" {
            assert!(result.is_ok());
        } else {
            assert!(result.unwrap_err().to_string().contains(message));
        }
        server.await.unwrap();
    }
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

#[tokio::test]
async fn pause_waits_for_forwarded_lifecycle_replies() {
    for method in ["thread/resume", "turn/start", "thread/goal/set"] {
        let scratch = Scratch::new();
        let path = scratch.0.join("server.sock");
        let proxy = scratch.0.join("relay.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let config = CodexConfig {
            endpoint: format!("unix://{}", path.display()),
            ..Default::default()
        };
        let (received_tx, received_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut probe = tokio_tungstenite::accept_async(stream).await.unwrap();
            let init = receive(&mut probe).await;
            send(&mut probe, &json!({"id":init["id"],"result":{}})).await;
            assert_eq!(receive(&mut probe).await["method"], "initialized");
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = receive(&mut socket).await;
            received_tx.send(()).unwrap();
            release_rx.await.unwrap();
            send(
                &mut socket,
                &json!({"id":request["id"],"result":{"thread":{
                    "id":"resumed-thread","status":{"type":"idle"},"cwd":"/work"
                }}}),
            )
            .await;
            // Keep the connection alive through the pause acknowledgement.
            let _ = socket.next().await;
        });
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut tui = transport::connect(&proxy).await.unwrap();
        send(
            &mut tui,
            &json!({"id":1,"method":method,"params":{"threadId":"resumed-thread"}}),
        )
        .await;
        received_rx.await.unwrap();
        {
            let pause = relay.pause_input(true);
            tokio::pin!(pause);
            let early = tokio::time::timeout(Duration::from_millis(30), &mut pause).await;
            assert!(
                early.is_err(),
                "{method} is still in flight; cleanup must not select a stale thread or turn"
            );
            release_tx.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(2), &mut pause)
                .await
                .unwrap()
                .unwrap();
        }
        let (mut monitor, mut row) = (Monitor::default(), state());
        while let Ok(event) = relay.events.try_recv() {
            monitor.apply(&mut row, event);
        }
        if method == "thread/resume" {
            assert_eq!(row.session_id.as_deref(), Some("resumed-thread"));
        }
        drop(tui);
        drop(relay);
        server.abort();
    }
}

async fn answer_probe(listener: &UnixListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let init = receive(&mut socket).await;
    send(&mut socket, &json!({"id":init["id"],"result":{}})).await;
    assert_eq!(receive(&mut socket).await["method"], "initialized");
}

#[tokio::test]
async fn lost_mutations_remain_uncertain_until_their_own_thread_is_reconnected() {
    for (lost_method, reconnect, rejected) in [
        ("turn/start", Some("thread-b"), true),
        ("turn/start", Some("thread-a"), false),
        ("thread/start", Some("thread-b"), true),
        ("thread/read", None, false),
    ] {
        let scratch = Scratch::new();
        let path = scratch.0.join("server.sock");
        let proxy = scratch.0.join("relay.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let config = CodexConfig {
            endpoint: format!("unix://{}", path.display()),
            ..Default::default()
        };
        let server = tokio::spawn(async move {
            answer_probe(&listener).await;
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert_eq!(receive(&mut socket).await["method"], lost_method);
            socket.close(None).await.unwrap();
            drop(socket);
            if let Some(thread) = reconnect {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let request = receive(&mut socket).await;
                send(
                    &mut socket,
                    &json!({"id":request["id"],"result":{"thread":{
                        "id":thread,"status":{"type":"idle"},"cwd":"/work"
                    }}}),
                )
                .await;
                let _ = socket.next().await;
            }
        });
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut tui = transport::connect(&proxy).await.unwrap();
        send(
            &mut tui,
            &json!({"id":1,"method":lost_method,"params":{"threadId":"thread-a"}}),
        )
        .await;
        loop {
            if matches!(relay.events.recv().await, Some(Observation::Disconnected)) {
                break;
            }
        }
        drop(tui);
        let mut tui = None;
        if let Some(thread) = reconnect {
            let mut connection = transport::connect(&proxy).await.unwrap();
            send(
                &mut connection,
                &json!({"id":2,"method":"thread/resume","params":{"threadId":thread}}),
            )
            .await;
            assert_eq!(receive(&mut connection).await["id"], 2);
            tui = Some(connection);
        }
        assert_eq!(
            relay.pause_input(true).await.is_err(),
            rejected,
            "lost {lost_method}, reconnected {reconnect:?}"
        );
        drop(tui);
        drop(relay);
        server.abort();
    }
}

#[tokio::test]
async fn lost_internal_creation_allows_kill_only_after_main_thread_cleanup() {
    use crate::backend::CleanupPolicy;

    for (source, ephemeral, other_mutation, reject_cleanup, removes) in [
        (json!("system"), true, false, false, true),
        (json!("system"), true, false, true, false),
        (json!("user"), true, false, false, false),
        (Value::Null, true, false, false, false),
        (json!("future-source"), true, false, false, false),
        (json!("system"), false, false, false, false),
        (json!("system"), true, true, false, false),
    ] {
        let scratch = Scratch::new();
        let socket = scratch.0.join("server.sock");
        let proxy = scratch.0.join("relay.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let config = CodexConfig {
            endpoint: format!("unix://{}", socket.display()),
            ..Default::default()
        };
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            answer_probe(&listener).await;
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert_eq!(receive(&mut socket).await["method"], "thread/start");
            if other_mutation {
                assert_eq!(receive(&mut socket).await["method"], "turn/start");
            }
            // Codex accepted creation, but the TUI never learns the helper ID.
            socket.close(None).await.unwrap();
            drop(socket);
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let init = receive(&mut socket).await;
                send(&mut socket, &json!({"id":init["id"],"result":{}})).await;
                assert_eq!(receive(&mut socket).await["method"], "initialized");
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "thread/loaded/list" => {
                            json!({"data":["thread-root","helper","other-thread"]})
                        }
                        "thread/goal/get" => json!({"goal":{"status":"active"}}),
                        "thread/goal/set" => {
                            assert_eq!(request["params"]["status"], "paused");
                            json!({})
                        }
                        "thread/turns/list" => {
                            json!({"data":[{"id":"main-turn","status":"inProgress"}]})
                        }
                        "turn/interrupt" => {
                            assert_eq!(request["params"]["turnId"], "main-turn");
                            json!({})
                        }
                        "thread/backgroundTerminals/clean" => json!({}),
                        method => panic!("unexpected cleanup method {method}"),
                    };
                    if request["method"] != "thread/loaded/list" {
                        assert_eq!(request["params"]["threadId"], "thread-root");
                        calls_tx
                            .send(request["method"].as_str().unwrap().to_owned())
                            .unwrap();
                    }
                    let reply = if reject_cleanup
                        && request["method"] == "thread/backgroundTerminals/clean"
                    {
                        json!({"id":request["id"],"error":{"code":-32603,"message":"cleanup denied"}})
                    } else {
                        json!({"id":request["id"],"result":result})
                    };
                    send(&mut socket, &reply).await;
                }
            }
        });
        let mut relay = Relay::start(&config, &proxy).await.unwrap();
        let mut tui = transport::connect(&proxy).await.unwrap();
        send(
            &mut tui,
            &json!({"id":1,"method":"thread/start","params":{
                "threadSource":source,"ephemeral":ephemeral
            }}),
        )
        .await;
        if other_mutation {
            send(
                &mut tui,
                &json!({"id":2,"method":"turn/start","params":{"threadId":"other-thread"}}),
            )
            .await;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(relay.events.recv().await, Some(Observation::Disconnected)) {}
        })
        .await
        .unwrap();
        drop(tui);

        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let runtime = crate::state::runtime_dir().join("launchers");
        crate::state::create_dir_all_private(&runtime).unwrap();
        let control_path = runtime.join(format!("{pid}.sock"));
        let control = Control::start(UnixListener::bind(&control_path).unwrap());
        let mut row = state();
        row.launcher_pid = pid;
        row.child_pid = Some(pid);
        row.session_id = Some("thread-root".into());
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
        let strict = snapshot.clone();
        assert!(
            tokio::task::spawn_blocking(move || kill(&strict, CleanupPolicy::Required))
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            crate::state::is_process_alive(pid),
            "Restart must retain control"
        );
        assert!(
            calls_rx.try_recv().is_err(),
            "uncertain Restart must not bypass the fence"
        );

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || kill(&snapshot, CleanupPolicy::ForceIfUnavailable)),
        )
        .await
        .unwrap()
        .unwrap();
        let mut calls = Vec::new();
        while let Ok(call) = calls_rx.try_recv() {
            calls.push(call);
        }
        if result.is_ok() {
            supervisor.await.unwrap().unwrap();
            assert!(
                !crate::state::is_process_alive(pid),
                "Kill must reap the TUI"
            );
        } else {
            assert!(
                crate::state::is_process_alive(pid),
                "a rejected cleanup must retain the TUI"
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
            "{source:?}, ephemeral={ephemeral}, other_mutation={other_mutation}, reject_cleanup={reject_cleanup}: {result:?}"
        );
        if removes || reject_cleanup {
            assert_eq!(
                calls,
                [
                    "thread/goal/get",
                    "thread/goal/set",
                    "thread/turns/list",
                    "turn/interrupt",
                    "thread/backgroundTerminals/clean"
                ]
            );
        } else {
            assert!(calls.is_empty(), "unknown work must still prevent cleanup");
        }
    }
}

#[tokio::test]
async fn cleanup_waits_for_resume_identity_while_draining_a_full_observation_queue() {
    let scratch = Scratch::new();
    let path = scratch.0.join("server.sock");
    let proxy = scratch.0.join("relay.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let config = CodexConfig {
        endpoint: format!("unix://{}", path.display()),
        ..Default::default()
    };
    let (forwarded_tx, forwarded_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        answer_probe(&listener).await;
        let (stream, _) = listener.accept().await.unwrap();
        let mut tui = tokio_tungstenite::accept_async(stream).await.unwrap();
        let request = receive(&mut tui).await;
        forwarded_tx.send(()).unwrap();
        release_rx.await.unwrap();
        // More than the relay channel's capacity. Waiting for its fence while
        // not draining these observations would deadlock until timeout.
        for _ in 0..256 {
            send(
                &mut tui,
                &json!({"method":"thread/status/changed","params":{
                    "threadId":"unrelated-thread","status":{"type":"idle"}
                }}),
            )
            .await;
        }
        send(
            &mut tui,
            &json!({"id":request["id"],"result":{"thread":{
                "id":"resumed-thread","status":{"type":"active"},"cwd":"/work"
            }}}),
        )
        .await;
        let (stream, _) = listener.accept().await.unwrap();
        let mut cleanup = tokio_tungstenite::accept_async(stream).await.unwrap();
        let init = receive(&mut cleanup).await;
        send(&mut cleanup, &json!({"id":init["id"],"result":{}})).await;
        assert_eq!(receive(&mut cleanup).await["method"], "initialized");
        for (method, result) in [
            ("thread/goal/get", json!({"goal":{"status":"active"}})),
            ("thread/goal/set", json!({})),
            (
                "thread/turns/list",
                json!({"data":[{"id":"running-turn","status":"inProgress"}]}),
            ),
            ("turn/interrupt", json!({})),
            ("thread/backgroundTerminals/clean", json!({})),
        ] {
            let request = receive(&mut cleanup).await;
            assert_eq!(request["method"], method);
            assert_eq!(request["params"]["threadId"], "resumed-thread");
            send(&mut cleanup, &json!({"id":request["id"],"result":result})).await;
        }
    });
    let relay = Relay::start(&config, &proxy).await.unwrap();
    let control_path = scratch.0.join("control.sock");
    let control = Control::start(UnixListener::bind(&control_path).unwrap());
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut row = state();
    row.launcher_pid = pid;
    row.child_pid = Some(pid);
    row.codex_control = true;
    crate::state::ensure_sessions_dir().unwrap();
    row.write().unwrap();
    let state_path = crate::state::sessions_dir().join(format!("{pid}.json"));
    // A later metadata write fails after durable control ownership exists.
    std::fs::remove_file(&state_path).unwrap();
    std::fs::create_dir(&state_path).unwrap();
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
    let mut tui = transport::connect(&proxy).await.unwrap();
    send(
        &mut tui,
        &json!({"id":1,"method":"thread/resume","params":{"threadId":"resumed-thread"}}),
    )
    .await;
    forwarded_rx.await.unwrap();
    let mut stop = tokio::net::UnixStream::connect(&control_path)
        .await
        .unwrap();
    crate::protocol::write_frame(&mut stop, &"Stop")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            crate::protocol::read_frame::<_, Value>(&mut stop)
        )
        .await
        .is_err(),
        "cleanup must not acknowledge success before learning the resumed thread"
    );
    release_tx.send(()).unwrap();
    // A real TUI keeps reading while cleanup fences its input. Without this
    // reader, small Unix socket buffers block the relay before the resume
    // response, independently of whether observations are being drained.
    let (reply, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(crate::protocol::read_frame::<_, Value>(&mut stop), async {
            for _ in 0..256 {
                assert_eq!(receive(&mut tui).await["method"], "thread/status/changed");
            }
            assert_eq!(receive(&mut tui).await["id"], 1);
        })
    })
    .await
    .unwrap();
    let reply = reply.unwrap().unwrap();
    assert!(
        reply["error"].is_null(),
        "cleanup should survive failed state persistence: {reply}"
    );
    assert_eq!(supervisor.await.unwrap().unwrap(), 0);
    assert!(!crate::state::is_process_alive(pid));
    server.await.unwrap();
    std::fs::remove_dir(state_path).unwrap();
    let _ = std::fs::remove_file(crate::state::sessions_dir().join(format!("{pid}.tmp")));
}
