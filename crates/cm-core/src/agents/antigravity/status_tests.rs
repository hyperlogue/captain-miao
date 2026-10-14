use super::*;
use crate::agent::AgentControl;
use std::io::Write;
use std::process::{Command, Stdio};

fn snapshot(activity: &str, tokens: u64) -> HookMessage {
    parse_status_payload(
        &serde_json::json!({
            "agent_state": activity,
            "conversation_id": "conversation-a",
            "model": {"id": "Gemini 3.1 Pro (Low)", "display_name": "Gemini 3.1 Pro (Low)"},
            "context_window": {
                "total_input_tokens": tokens, "total_output_tokens": 12,
                "context_window_size": 1048576,
                "current_usage": {"input_tokens": 1234, "output_tokens": 12}
            },
            "transcript_path": "/wrong/gui/path"
        })
        .to_string(),
    )
    .unwrap()
}

#[test]
fn startup_and_cancelled_turns_settle_without_a_stop_hook() {
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Starting);
    dispatch_hook(
        &mut state,
        parse_status_payload(r#"{"agent_state":"idle","conversation_id":""}"#).unwrap(),
    );
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(
        state.session_id.is_none(),
        "the empty TUI has no conversation yet"
    );
    dispatch_hook(&mut state, snapshot("working", 17212));
    assert_eq!(state.status, SessionStatus::Active);
    state.last_prompt = Some("keep this prompt".into());
    state.last_tool = Some("run_command".into());
    state.set_last_error(Some("keep this failure".into()));
    dispatch_hook(&mut state, snapshot("idle", 17276));
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(state.last_tool.is_none());
    assert_eq!(state.last_prompt.as_deref(), Some("keep this prompt"));
    assert_eq!(state.last_error.as_deref(), Some("keep this failure"));
    assert_eq!(state.context_tokens, Some(17276));
    assert_eq!(state.session_id.as_deref(), Some("conversation-a"));
    assert_eq!(state.model.as_deref(), Some("Gemini 3.1 Pro (Low)"));
    assert!(snapshot("idle", 42).transcript_path.is_none());
    dispatch_hook(&mut state, snapshot("idle", 42));
    assert_eq!(
        state.context_tokens,
        Some(42),
        "context may shrink after compaction"
    );
}

#[test]
fn unknown_and_coarse_states_preserve_more_specific_hook_status() {
    let mut state =
        LauncherState::for_test(AgentControl::Antigravity, SessionStatus::WaitingForDecision);
    for activity in [
        "authenticating",
        "initializing",
        "future_state",
        "working",
        "thinking",
        "tool_use",
    ] {
        dispatch_hook(&mut state, snapshot(activity, 10));
        assert_eq!(
            state.status,
            SessionStatus::WaitingForDecision,
            "{activity}"
        );
    }
    let before = state.context_tokens;
    dispatch_hook(&mut state, parse_status_payload(r#"{"agent_state":"initializing","model":null,"context_window":{"total_input_tokens":0,"context_window_size":0}}"#).unwrap());
    assert_eq!(
        state.context_tokens, before,
        "uninitialized usage is absent"
    );
}

#[test]
fn status_wrapper_preserves_the_original_input_and_output_when_forwarding_fails() {
    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("observer's input");
    let forwarder = format!(
        "/bin/sh -c {}",
        shell_quote(&format!(
            "cat > {}; exit 1",
            shell_quote(&capture.to_string_lossy())
        ))
    );
    let original = serde_json::json!({"type":"command", "command":"cat; printf '\\ncustom output'", "padding":2, "stack_with_default":false});
    let wrapped = status_line_settings(&original, &forwarder);
    assert_eq!(wrapped["padding"], 2);
    assert_eq!(wrapped["stack_with_default"], false);
    let mut child = Command::new("/bin/sh")
        .args(["-c", wrapped["command"].as_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = r#"{"literal":"$(echo unsafe) `echo unsafe` ' $HOME","agent_state":"idle"}"#;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!("{input}\ncustom output")
    );
    assert_eq!(std::fs::read_to_string(capture).unwrap(), input);
    let default = status_line_settings(&serde_json::Value::Null, "true");
    assert_eq!(default["stack_with_default"], true);
    let disabled = status_line_settings(
        &serde_json::json!({"enabled":false, "command":"exit 99"}),
        "true",
    );
    assert_eq!(disabled["enabled"], true);
    assert!(!disabled["command"].as_str().unwrap().contains("exit 99"));
}

fn injected() -> serde_json::Value {
    serde_json::from_str(&build_settings_for(Path::new("miao"))).unwrap()
}

#[test]
fn cli_mirror_keeps_state_shared_and_settings_private_across_relaunches() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let dir = root.path().join("managed");
    std::fs::create_dir(&real).unwrap();
    let original = r#"{"modelProvider":"gemini","statusLine":{"command":"cat","padding":1}}"#;
    std::fs::write(real.join("settings.json"), original).unwrap();
    // Migrate the whole-directory link used by the previous integration.
    std::os::unix::fs::symlink(&real, &dir).unwrap();
    ensure_cli_home(&real, &dir, &injected()).unwrap();
    assert!(!dir.symlink_metadata().unwrap().file_type().is_symlink());
    std::fs::write(dir.join("brain/conversation"), "shared state").unwrap();
    assert_eq!(
        std::fs::read_to_string(real.join("brain/conversation")).unwrap(),
        "shared state"
    );
    // A root-level database and its WAL must refer to one real store, including
    // the first creation before any entry existed in the real directory.
    let db = rusqlite::Connection::open(dir.join("conversation_summaries.db")).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE probe (id INTEGER); INSERT INTO probe VALUES (42)",
    )
    .unwrap();
    let reader = rusqlite::Connection::open(real.join("conversation_summaries.db")).unwrap();
    assert_eq!(
        reader
            .query_row("SELECT id FROM probe", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        42
    );
    let mut settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap();
    settings["model"] = "managed-model".into();
    std::fs::write(dir.join("settings.json"), settings.to_string()).unwrap();
    std::fs::write(dir.join("new-agent-credential"), "private fixture").unwrap();
    ensure_cli_home(&real, &dir, &injected()).unwrap();
    let again: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        again, settings,
        "settings and observer do not drift or nest"
    );
    assert_eq!(
        std::fs::read_to_string(real.join("settings.json")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_to_string(real.join("new-agent-credential")).unwrap(),
        "private fixture"
    );
    assert!(!real.join(".captain-miao-settings-source").exists());
    std::fs::write(real.join("settings.json"), r#"{"model":"external-model"}"#).unwrap();
    ensure_cli_home(&real, &dir, &injected()).unwrap();
    let refreshed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(refreshed["model"], "external-model");
    assert_eq!(refreshed["statusLine"]["stack_with_default"], true);
}

#[test]
fn legacy_first_login_directories_are_adopted_before_seeding() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let dir = root.path().join("managed");
    std::fs::create_dir_all(dir.join("conversations")).unwrap();
    std::fs::write(dir.join("conversations/session.db"), "conversation fixture").unwrap();
    ensure_cli_home(&real, &dir, &injected()).unwrap();
    assert_eq!(
        std::fs::read_to_string(real.join("conversations/session.db")).unwrap(),
        "conversation fixture"
    );
    assert!(
        dir.join("conversations")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!real.join("settings.json").exists());
}
