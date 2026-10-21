use super::*;
use crate::agent::AgentControl;

fn message(event: HookEvent, extra: serde_json::Value) -> HookMessage {
    let mut body = serde_json::json!({"conversationId":"fixture","conversation_id":"fixture"});
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let mut message = super::super::parse_hook_payload(event, &body.to_string()).unwrap();
    // These tests never consult the user's transcript or global state paths.
    message.transcript_path = None;
    message
}

fn stop(idle: bool) -> HookMessage {
    message(HookEvent::Stop, serde_json::json!({"fullyIdle":idle}))
}

fn snapshot(activity: &str) -> HookMessage {
    message(
        HookEvent::StatusUpdate,
        serde_json::json!({"agent_state":activity}),
    )
}

fn details(command: &str) -> Vec<u8> {
    assert!(command.len() < 128);
    let mut bytes = vec![0x22, command.len() as u8];
    bytes.extend_from_slice(command.as_bytes());
    bytes
}

fn database(path: &Path) -> Connection {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, status INTEGER, task_details BLOB)",
    )
    .unwrap();
    conn
}

fn insert(conn: &Connection, idx: i64, status: i32, command: Option<&str>) {
    conn.execute(
        "INSERT INTO steps VALUES (?1, ?2, ?3)",
        rusqlite::params![idx, status, command.map(details)],
    )
    .unwrap();
}

#[tokio::test]
async fn background_stop_survives_idle_snapshot() {
    let mut monitor = Monitor::at(None);
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Active);
    monitor.dispatch_hook(&mut state, stop(false));
    assert_eq!(state.status, SessionStatus::BackgroundActive);
    monitor.dispatch_hook(&mut state, snapshot("idle"));
    assert_eq!(state.status, SessionStatus::BackgroundActive);
    monitor.dispatch_hook(&mut state, stop(true));
    assert_eq!(state.status, SessionStatus::Idle);
    monitor.dispatch_hook(&mut state, snapshot("idle"));
    assert_eq!(state.status, SessionStatus::Idle);
}

#[test]
fn reader_tracks_tasks_incrementally_and_retires_terminal_states() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("conversation.db");
    let conn = database(&path);
    for idx in 0..1000 {
        insert(&conn, idx, 3, None);
    }
    insert(&conn, 1000, 2, Some("r3 watch review"));
    insert(&conn, 1001, 1, None);
    let mut reader = Reader::new(path);
    assert_eq!(reader.refresh().unwrap(), ["r3 watch review"]);
    assert_eq!(reader.rows_read, 1002);
    reader.rows_read = 0;
    conn.execute(
        "UPDATE steps SET status=2, task_details=?1 WHERE idx=1001",
        [details("Timer: 20s")],
    )
    .unwrap();
    assert_eq!(reader.refresh().unwrap(), ["Timer: 20s", "r3 watch review"]);
    assert_eq!(
        reader.rows_read, 2,
        "only unfinished primary keys are revisited"
    );
    conn.execute("UPDATE steps SET status=3 WHERE idx=1000", [])
        .unwrap();
    assert_eq!(reader.refresh().unwrap(), ["Timer: 20s"]);
    for status in [3, 4, 5, 6, 7, 12] {
        insert(&conn, 2000 + status as i64, status, Some("finished"));
    }
    conn.execute("UPDATE steps SET status=6 WHERE idx=1001", [])
        .unwrap();
    assert!(reader.refresh().unwrap().is_empty());
    reader.rows_read = 0;
    assert!(reader.refresh().unwrap().is_empty());
    assert_eq!(reader.rows_read, 0, "completed history is never rescanned");
}

#[test]
fn unknown_or_locked_data_preserves_the_committed_cursor() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("conversation.db");
    let conn = database(&path);
    insert(&conn, 1, 2, Some("watch"));
    let mut reader = Reader::new(path);
    assert_eq!(reader.refresh().unwrap(), ["watch"]);
    conn.execute_batch("BEGIN EXCLUSIVE; UPDATE steps SET status=3 WHERE idx=1")
        .unwrap();
    assert!(reader.refresh().is_err());
    assert_eq!(reader.unfinished[&1].as_deref(), Some("watch"));
    conn.execute_batch("ROLLBACK").unwrap();
    insert(&conn, 2, 99, Some("unknown"));
    assert!(reader.refresh().is_err());
    assert_eq!(reader.through, 1);
    assert_eq!(reader.unfinished.len(), 1);
    conn.execute(
        "UPDATE steps SET status=2,task_details=?1 WHERE idx=2",
        [vec![0x22, 0xff]],
    )
    .unwrap();
    assert!(reader.refresh().is_err());
    assert_eq!(reader.through, 1);
    conn.execute(
        "UPDATE steps SET task_details=?1 WHERE idx=2",
        [details("new task")],
    )
    .unwrap();
    assert_eq!(reader.refresh().unwrap(), ["new task", "watch"]);
}

#[test]
fn reader_recovers_from_database_replacement_and_truncation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("conversation.db");
    let conn = database(&path);
    insert(&conn, 10, 2, Some("old task"));
    let mut reader = Reader::new(path.clone());
    assert_eq!(reader.refresh().unwrap(), ["old task"]);
    conn.execute_batch("DELETE FROM steps").unwrap();
    insert(&conn, 1, 2, Some("reset task"));
    assert_eq!(reader.refresh().unwrap(), ["reset task"]);
    let replacement = root.path().join("replacement.db");
    let replaced = database(&replacement);
    insert(&replaced, 1, 2, Some("replacement task"));
    drop(replaced);
    std::fs::rename(replacement, path).unwrap();
    assert_eq!(reader.refresh().unwrap(), ["replacement task"]);
}

#[test]
fn descriptions_skip_unrelated_fields_and_reject_partial_records() {
    let mut data = vec![0x08, 0x80, 0x01, 0x1a, 0x03, 0xff, 0xff, 0xff];
    data.extend(details("r3 watch review"));
    data.extend([0x3a, 4, b't', b'e', b's', b't']);
    assert_eq!(task_description(&data).unwrap(), "r3 watch review");
    assert_eq!(
        task_description(&[0x3a, 4, b't', b'e', b's', b't']).unwrap(),
        "test"
    );
    for data in [
        &[][..],
        &[0x22, 5, b'x'],
        &[0x22, 1, 0xff],
        &[0],
        &[0x08, 0x80],
        &[0xff; 12],
    ] {
        assert!(task_description(data).is_err(), "{data:?}");
    }
}

#[tokio::test]
async fn stale_observations_do_not_erase_new_turns_or_follow_session_switches() {
    let root = tempfile::tempdir().unwrap();
    let mut monitor = Monitor::at(Some(root.path().to_path_buf()));
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Active);
    monitor.dispatch_hook(&mut state, stop(false));
    let revision = monitor.request.as_ref().unwrap().revision;
    assert!(monitor.apply(
        Update {
            revision,
            commands: vec!["watch".into()]
        },
        &mut state
    ));
    monitor.dispatch_hook(&mut state, snapshot("idle"));
    assert_eq!(state.status, SessionStatus::BackgroundActive);
    assert_eq!(state.background_commands, ["watch"]);
    monitor.dispatch_hook(&mut state, snapshot("working"));
    assert_eq!(state.status, SessionStatus::Active);
    assert!(!monitor.apply(
        Update {
            revision,
            commands: Vec::new()
        },
        &mut state
    ));
    assert_eq!(state.status, SessionStatus::Active);
    monitor.dispatch_hook(
        &mut state,
        message(
            HookEvent::StatusUpdate,
            serde_json::json!({
                "agent_state":"tool_use", "tool_confirmation_pending":true
            }),
        ),
    );
    assert_eq!(state.status, SessionStatus::WaitingForApproval);
    assert!(!monitor.apply(
        Update {
            revision,
            commands: Vec::new()
        },
        &mut state
    ));
    monitor.dispatch_hook(&mut state, stop(true));
    assert!(state.background_commands.is_empty());
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(!monitor.apply(
        Update {
            revision,
            commands: vec!["old".into()]
        },
        &mut state
    ));
    let mut next = snapshot("idle");
    next.session_id = Some("another".into());
    monitor.dispatch_hook(&mut state, next);
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(state.background_commands.is_empty());
}

async fn observe_update(monitor: &mut Monitor, state: &mut LauncherState, phase: &str) {
    let update = tokio::time::timeout(Duration::from_secs(15), monitor.changed())
        .await
        .unwrap_or_else(|_| panic!("waiting for {phase}"));
    monitor.apply(update, state);
}

#[tokio::test]
#[cfg(not(target_os = "macos"))]
async fn wal_completion_is_seen_without_another_hook_and_foreground_reads_are_parked() {
    exercise_completion(WatchMode::Events).await;
}

#[tokio::test]
async fn hook_refresh_sees_completion_without_periodic_io() {
    exercise_completion(WatchMode::Hooks).await;
}

async fn exercise_completion(mode: WatchMode) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("conversations/fixture.db");
    let conn = database(&path);
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0")
        .unwrap();
    insert(&conn, 1, 2, Some("r3 watch review"));
    let mut monitor = Monitor::with_watch(Some(root.path().to_path_buf()), mode);
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Active);
    monitor.dispatch_hook(&mut state, stop(false));
    observe_update(&mut monitor, &mut state, "initial background task").await;
    assert_eq!(state.background_commands, ["r3 watch review"]);
    assert!(
        tokio::time::timeout(COALESCE * 3, monitor.changed())
            .await
            .is_err(),
        "quiet background tasks must not cause more database reads"
    );
    monitor.dispatch_hook(&mut state, snapshot("idle"));
    assert_eq!(state.status, SessionStatus::BackgroundActive);
    observe_update(&mut monitor, &mut state, "idle hook refresh").await;
    assert_eq!(state.status, SessionStatus::BackgroundActive);
    // The writer commits only to its held-open WAL. A DB-only watch misses it.
    conn.execute("UPDATE steps SET status=3 WHERE idx=1", [])
        .unwrap();
    if matches!(mode, WatchMode::Hooks) {
        assert!(
            tokio::time::timeout(Duration::from_secs(3), monitor.changed())
                .await
                .is_err(),
            "database changes alone must not trigger hook-only reads"
        );
        monitor.dispatch_hook(&mut state, snapshot("idle"));
    }
    observe_update(&mut monitor, &mut state, "WAL task completion").await;
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(state.background_commands.is_empty());
    monitor.dispatch_hook(
        &mut state,
        message(HookEvent::PromptSubmit, serde_json::json!({})),
    );
    insert(&conn, 2, 2, Some("next command"));
    // This is real-I/O coverage: wait longer than coalescing/read deadlines.
    // Any queued completion snapshot is allowed, but no fresh busy-turn read.
    tokio::time::sleep(COALESCE * 3).await;
    if monitor.updates.has_changed().unwrap() {
        let update = monitor.updates.borrow_and_update().clone().unwrap();
        assert_ne!(update.revision, monitor.request.as_ref().unwrap().revision);
    }
    assert_eq!(state.status, SessionStatus::Active);
    monitor.dispatch_hook(&mut state, stop(false));
    observe_update(&mut monitor, &mut state, "next foreground turn settling").await;
    assert_eq!(state.background_commands, ["next command"]);
}

/// Captures current SQLite records from a real background command, then folds
/// those snapshots through the same monitor the launcher uses. No credentials
/// or external inference; the model and held command use a loopback fixture.
#[tokio::test]
#[ignore = "requires agy or CM_TEST_AGY_BIN, Python 3, a PTY and loopback sockets"]
async fn real_agy_background_task_lifecycle() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new("python3")
        .args(["-c", include_str!("live_test.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({
                "root":root.path(),
                "binary":std::env::var("CM_TEST_AGY_BIN").unwrap_or_else(|_| "agy".into())
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let observed = root.path().join("observed");
    std::fs::create_dir_all(observed.join("conversations")).unwrap();
    let database = observed.join("conversations/fixture.db");
    std::fs::copy(root.path().join("running.db"), &database).unwrap();
    let mut monitor = Monitor::with_watch(Some(observed), WatchMode::Hooks);
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Starting);
    for event in events["background"].as_array().unwrap() {
        let kind = HookEvent::from_kebab(event["event"].as_str().unwrap()).unwrap();
        let mut message =
            super::super::parse_hook_payload(kind, &event["body"].to_string()).unwrap();
        message.session_id = Some("fixture".into());
        message.transcript_path = None;
        monitor.dispatch_hook(&mut state, message);
    }
    observe_update(&mut monitor, &mut state, "real task description").await;
    assert_eq!(state.status, SessionStatus::BackgroundActive);
    assert_eq!(state.background_commands.len(), 1);
    assert!(state.background_commands[0].contains("urllib.request"));
    monitor.dispatch_hook(&mut state, snapshot("idle"));
    assert_eq!(state.status, SessionStatus::BackgroundActive);
    std::fs::rename(root.path().join("completed.db"), &database).unwrap();
    for event in events["completed"].as_array().unwrap() {
        let kind = HookEvent::from_kebab(event["event"].as_str().unwrap()).unwrap();
        let mut message =
            super::super::parse_hook_payload(kind, &event["body"].to_string()).unwrap();
        message.session_id = Some("fixture".into());
        message.transcript_path = None;
        monitor.dispatch_hook(&mut state, message);
    }
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(state.background_commands.is_empty());
}
