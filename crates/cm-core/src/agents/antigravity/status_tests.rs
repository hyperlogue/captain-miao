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
    assert_eq!(state.context_tokens, Some(17288));
    assert_eq!(state.session_id.as_deref(), Some("conversation-a"));
    assert_eq!(state.model.as_deref(), Some("Gemini 3.1 Pro (Low)"));
    assert!(
        snapshot("idle", 42)
            .transcript_path
            .unwrap()
            .ends_with("brain/conversation-a/.system_generated/logs/transcript.jsonl")
    );
    dispatch_hook(&mut state, snapshot("idle", 42));
    assert_eq!(
        state.context_tokens,
        Some(54),
        "context may shrink after compaction"
    );
}

#[test]
fn context_usage_includes_outputs_to_match_agys_reported_percentage() {
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Idle);
    let payload = r#"{"agent_state":"idle","context_window":{"total_input_tokens":88244,"total_output_tokens":61074,"context_window_size":1048576,"used_percentage":14.24}}"#;
    dispatch_hook(&mut state, parse_status_payload(payload).unwrap());
    assert_eq!(state.context_tokens, Some(149_318));
    let percentage =
        state.context_tokens.unwrap() as f64 * 100.0 / state.context_window.unwrap() as f64;
    assert!((percentage - 14.24).abs() < 0.01);
    dispatch_hook(&mut state, snapshot("idle", 42));
    assert_eq!(
        state.context_tokens,
        Some(54),
        "usage can shrink after compaction"
    );
}

#[test]
fn context_capacity_tracks_the_model_and_ignores_uninitialized_snapshots() {
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Idle);
    for (reported, expected) in [(1_048_576, 1_048_576), (200_000, 200_000), (0, 200_000)] {
        dispatch_hook(
            &mut state,
            parse_status_payload(
                &serde_json::json!({
                    "agent_state": "idle",
                    "context_window": {"total_input_tokens": 42, "context_window_size": reported}
                })
                .to_string(),
            )
            .unwrap(),
        );
        assert_eq!(state.context_window, Some(expected));
    }
}

#[test]
fn approval_snapshots_distinguish_waiting_from_tool_execution() {
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Active);
    for after in ["working", "tool_use", "idle"] {
        dispatch_hook(
            &mut state,
            parse_status_payload(r#"{"agent_state":"tool_use","tool_confirmation_pending":true}"#)
                .unwrap(),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
        dispatch_hook(&mut state, snapshot(after, 10));
        assert_eq!(
            state.status,
            if after == "idle" {
                SessionStatus::Idle
            } else {
                SessionStatus::Active
            }
        );
    }
}

fn append_step(path: &Path, step: serde_json::Value) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{step}").unwrap();
}

fn append_prompt(path: &Path, prompt: &str) {
    append_step(
        path,
        serde_json::json!({
            "type": "USER_INPUT", "status": "DONE",
            "content": format!("<USER_REQUEST>\n{prompt}\n</USER_REQUEST>")
        }),
    );
}

fn snapshot_at(path: &Path, activity: &str, tokens: u64) -> HookMessage {
    let mut message = snapshot(activity, tokens);
    message.transcript_path = Some(path.to_string_lossy().into_owned());
    message
}

#[test]
fn compact_snapshots_use_the_transcript_and_withhold_stale_usage_until_inference() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    append_prompt(&path, "original task");
    append_step(
        &path,
        serde_json::json!({"type":"PLANNER_RESPONSE", "status":"DONE", "input_tokens":9000}),
    );
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Idle);
    dispatch_hook(&mut state, snapshot_at(&path, "idle", 9000));
    assert_eq!(state.context_tokens, Some(9012));

    // agy 1.4.0's status command says only "working" during /compact.
    append_prompt(&path, "/compact");
    dispatch_hook(&mut state, snapshot_at(&path, "working", 9000));
    assert_eq!(state.status, SessionStatus::Compacting);
    assert_eq!(state.context_tokens, Some(9012));

    append_step(
        &path,
        serde_json::json!({"type":"CHECKPOINT", "status":"DONE", "content":"summary"}),
    );
    // Its idle snapshot repeats the old usage, even after a fresh resume.
    for initial in [
        SessionStatus::Compacting,
        SessionStatus::Compacted,
        SessionStatus::Starting,
    ] {
        state.status = initial;
        dispatch_hook(&mut state, snapshot_at(&path, "idle", 9000));
        assert_eq!(state.status, SessionStatus::Compacted);
        assert_eq!(state.context_tokens, None);
        assert_eq!(state.context_window, Some(1048576));
    }

    append_prompt(&path, "continue");
    dispatch_hook(&mut state, snapshot_at(&path, "working", 9000));
    assert_eq!(state.status, SessionStatus::Active);
    assert_eq!(
        state.context_tokens, None,
        "a new prompt is not fresh usage"
    );
    append_step(
        &path,
        serde_json::json!({"type":"PLANNER_RESPONSE", "status":"DONE", "input_tokens":0}),
    );
    dispatch_hook(&mut state, snapshot_at(&path, "idle", 9000));
    assert_eq!(state.status, SessionStatus::Idle);
    assert_eq!(
        state.context_tokens, None,
        "cancelled inference has no usage"
    );

    let response = serde_json::json!({
        "type":"PLANNER_RESPONSE", "status":"DONE", "input_tokens":42
    })
    .to_string();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(response.as_bytes()).unwrap();
    dispatch_hook(&mut state, snapshot_at(&path, "idle", 9000));
    assert_eq!(
        state.context_tokens, None,
        "partial records are not evidence"
    );
    file.write_all(b"\n").unwrap();
    dispatch_hook(&mut state, snapshot_at(&path, "idle", 42));
    assert_eq!(state.status, SessionStatus::Idle);
    assert_eq!(state.context_tokens, Some(54));
}

#[test]
fn cancelled_compaction_keeps_usage_and_does_not_reactivate_on_an_ordinary_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    append_prompt(&path, "/compact");
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Active);
    dispatch_hook(&mut state, snapshot_at(&path, "working", 9000));
    assert_eq!(state.status, SessionStatus::Compacting);
    dispatch_hook(&mut state, snapshot_at(&path, "idle", 9000));
    assert_eq!(state.status, SessionStatus::Idle);
    assert_eq!(state.context_tokens, Some(9012));
    for prompt in ["explain /compact", "/compaction", "continue"] {
        append_prompt(&path, prompt);
        dispatch_hook(&mut state, snapshot_at(&path, "working", 9000));
        assert_eq!(state.status, SessionStatus::Active);
    }
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

#[tokio::test]
async fn launch_redirects_gemini_without_mirroring_or_replacing_home() {
    use std::os::unix::fs::PermissionsExt;

    const TEST: &str = "agents::antigravity::status_tests::launch_redirects_gemini_without_mirroring_or_replacing_home";
    let Some(root) = std::env::var_os("CM_TEST_AGY_HOME") else {
        // The production launcher resolves global state paths. Keep every
        // write in a child process with its own HOME and XDG roots.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let agy = bin.join("agy");
        std::fs::write(
            &agy,
            "#!/bin/sh\nprintf '%s\\n' \"$HOME\" \"$PWD\" \"$CAPTAIN_MIAO_SOCK\" \"$@\"\nprintf 'real home' > \"$HOME/new-agent-file\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env("CM_TEST_AGY_HOME", &root)
            .env("PATH", &bin)
            .env("HOME", root.join("real"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let root = PathBuf::from(root);
    let real = root.join("real");
    let config = real.join(".gemini/config");
    std::fs::create_dir_all(&config).unwrap();
    let original_hooks = r#"{"user":{"Stop":[{"type":"command","command":"true"}]}}"#;
    std::fs::write(config.join(HOOKS_FILE), original_hooks).unwrap();
    std::fs::write(real.join("unrelated-home-file"), "unrelated").unwrap();
    let legacy_home = crate::state::state_dir().join("antigravity-home");
    std::fs::create_dir_all(&legacy_home).unwrap();
    std::fs::write(legacy_home.join("legacy-user-file"), "keep me").unwrap();
    let settings = root.join("injected.json");
    std::fs::write(&settings, injected().to_string()).unwrap();
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    let socket = root.join("launcher.sock");
    let output = build_launch_command(
        work.to_str().unwrap(),
        &socket,
        &settings,
        &["--conversation".into(), "existing-conversation".into()],
        None,
    )
    .unwrap()
    .output()
    .await
    .unwrap();
    assert!(output.status.success());
    let gemini = legacy_home.join(".gemini");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}\n{}\n{}\n--gemini_dir\n{}\n--conversation\nexisting-conversation\n",
            real.display(),
            work.display(),
            socket.display(),
            gemini.display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(real.join("new-agent-file")).unwrap(),
        "real home"
    );
    assert!(!legacy_home.join("new-agent-file").exists());
    assert!(!legacy_home.join("unrelated-home-file").exists());
    assert_eq!(
        std::fs::read_to_string(legacy_home.join("legacy-user-file")).unwrap(),
        "keep me"
    );
    assert_eq!(
        std::fs::read_to_string(config.join(HOOKS_FILE)).unwrap(),
        original_hooks
    );
    let hooks: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(gemini.join("config/hooks.json")).unwrap())
            .unwrap();
    assert!(hooks.get("user").is_some());
    assert!(hooks.get(HOOK_NAME).is_some());
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
fn removing_global_settings_resets_the_managed_copy_once() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let dir = root.path().join("managed");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(
        real.join("settings.json"),
        r#"{"modelProvider":"gemini","model":"old-model"}"#,
    )
    .unwrap();
    ensure_cli_home(&real, &dir, &injected()).unwrap();
    std::fs::remove_file(real.join("settings.json")).unwrap();
    ensure_cli_home(&real, &dir, &injected()).unwrap();
    let mut settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap();
    assert!(settings.get("modelProvider").is_none());
    assert!(settings.get("model").is_none());
    settings["model"] = "managed-model".into();
    std::fs::write(dir.join("settings.json"), settings.to_string()).unwrap();
    ensure_cli_home(&real, &dir, &injected()).unwrap();
    let again: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        again, settings,
        "unchanged absent source preserves managed settings"
    );
    assert!(!real.join("settings.json").exists());
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

#[cfg(target_os = "linux")]
#[test]
fn failed_cross_filesystem_adoption_keeps_legacy_conversations_visible() {
    use std::os::unix::fs::MetadataExt;

    let source = tempfile::tempdir().unwrap();
    let Ok(destination) = tempfile::tempdir_in("/dev/shm") else {
        eprintln!("skipping cross-filesystem migration: /dev/shm is not writable");
        return;
    };
    if source.path().metadata().unwrap().dev() == destination.path().metadata().unwrap().dev() {
        eprintln!("skipping cross-filesystem migration: temp and /dev/shm share a filesystem");
        return;
    }
    let real = destination.path().join("real");
    let dir = source.path().join("managed");
    std::fs::create_dir_all(dir.join("conversations")).unwrap();
    std::fs::write(
        dir.join("conversations/session.db"),
        "existing conversation",
    )
    .unwrap();
    let result = ensure_cli_home(&real, &dir, &injected());
    assert!(
        result.is_err(),
        "a failed migration must not silently start an empty store"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("conversations/session.db")).unwrap(),
        "existing conversation"
    );
    assert!(!real.join("conversations").exists());
}
