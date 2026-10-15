//! Opt-in compatibility test using the real agy, a PTY and a loopback model.
//! Run with `CM_TEST_AGY_BIN=/path/to/agy nix develop -c cargo test -p cm-core
//! real_agy_tracks_startup_completion_cancellation_and_resume -- --ignored`.
use super::*;
use crate::agent::AgentControl;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
#[ignore = "requires agy or CM_TEST_AGY_BIN, Python 3, a PTY and loopback sockets"]
fn real_agy_tracks_startup_completion_cancellation_and_resume() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let managed = root.path().join("managed");
    let cli = real.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(cli.join("cache")).unwrap();
    std::fs::write(cli.join("cache/onboarding.json"), r#"{"onboardingComplete":true,"consumerOnboardingComplete":true,"enterpriseOnboardingComplete":true}"#).unwrap();
    let recorder = root.path().join("record-hook");
    std::fs::write(
        &recorder,
        r#"#!/usr/bin/env python3
import json, os, sys
body = json.load(sys.stdin)
line = json.dumps({'event': sys.argv[-1], 'body': body}) + '\n'
fd = os.open(os.environ['CM_AGY_TEST_EVENTS'], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
os.write(fd, line.encode())
os.close(fd)
print('custom-status-visible' if sys.argv[-1] == 'user-status' else '{}')
"#,
    )
    .unwrap();
    std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = serde_json::json!({
        "modelProvider":"gemini",
        "statusLine":{"command":format!("{} user-status", shell_quote(&recorder.to_string_lossy())),"stack_with_default":true}
    }).to_string();
    std::fs::write(cli.join("settings.json"), &settings).unwrap();
    let injection = serde_json::from_str(&build_settings_for(&recorder)).unwrap();
    ensure_synth_home_at(&real, &managed, &injection).unwrap();
    let mut child = Command::new("python3")
        .args(["-c", include_str!("live_test.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Python 3 is required for the optional agy compatibility test");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({
                "root":root.path(),"home":managed,
                "binary":std::env::var("CM_TEST_AGY_BIN").unwrap_or_else(|_| "agy".into())
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let checkpoints: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Starting);
    let mut conversation = None;
    for checkpoint in checkpoints.as_array().unwrap() {
        let phase = checkpoint["phase"].as_str().unwrap();
        if phase == "resume" {
            state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Starting);
        }
        let events = checkpoint["events"].as_array().unwrap();
        for event in events {
            if event["event"] == "user-status" {
                continue;
            }
            let kind = HookEvent::from_kebab(event["event"].as_str().unwrap()).unwrap();
            dispatch_hook(
                &mut state,
                parse_hook_payload(kind, &event["body"].to_string()).unwrap(),
            );
        }
        // Check against agy's independently reported percentage, not just a
        // second implementation of our input/output sum.
        if let Some(window) = events
            .iter()
            .rev()
            .find(|e| e["event"] == "status-update")
            .and_then(|e| e["body"].get("context_window"))
            && window["context_window_size"]
                .as_u64()
                .is_some_and(|size| size > 0)
        {
            let percentage =
                state.context_tokens.unwrap() as f64 * 100.0 / state.context_window.unwrap() as f64;
            assert!(
                (percentage - window["used_percentage"].as_f64().unwrap()).abs() < 0.01,
                "context percentage disagrees with agy during {phase}"
            );
        }
        match phase {
            "startup" => assert_eq!(state.status, SessionStatus::Idle),
            "rename" => assert_eq!(state.status, SessionStatus::Idle),
            "working" => assert_eq!(state.status, SessionStatus::Active),
            "complete" => {
                assert_eq!(state.status, SessionStatus::Idle);
                assert!(state.context_tokens.is_some_and(|n| n > 0));
                assert!(state.context_window.is_some_and(|n| n > 0));
                assert!(state.model.is_some());
                conversation = state.session_id.clone();
                assert!(conversation.is_some());
                assert!(events.iter().any(|e| e["event"] == "session-start"));
            }
            "cancel" => {
                assert!(
                    !events.iter().any(|e| e["event"] == "stop"),
                    "probe must exercise cancellation without Stop"
                );
                assert_eq!(state.status, SessionStatus::Idle, "{checkpoint}");
            }
            "approval" | "accept-approval" => {
                assert_eq!(state.status, SessionStatus::WaitingForApproval);
                assert!(AgentControl::Antigravity.capabilities().approval_gate);
            }
            "cancel-approval" => assert_eq!(state.status, SessionStatus::Idle),
            "accepted" => assert_eq!(state.status, SessionStatus::Active),
            "accepted-complete" => assert_eq!(state.status, SessionStatus::Idle),
            "clear" => {
                assert_eq!(state.status, SessionStatus::Idle);
                assert!(state.session_id.is_some());
                assert_ne!(state.session_id, conversation);
                assert_eq!(state.context_tokens, Some(0));
            }
            "resume" => {
                assert_eq!(state.status, SessionStatus::Idle);
                assert_eq!(state.session_id, conversation);
                assert!(state.context_tokens.is_some_and(|n| n > 0));
            }
            other => panic!("unexpected phase {other}"),
        }
        let snapshot = Path::new(checkpoint["snapshot"].as_str().unwrap());
        let stats = read_transcript_stats(&snapshot.join("transcript.jsonl"), None);
        let expected_prompt = match phase {
            "startup" | "clear" => None,
            "complete" | "rename" => Some("completion_fixture"),
            "working" | "cancel" => Some("cancel_fixture"),
            "approval" | "cancel-approval" => Some("approval_fixture"),
            _ => Some("accept_fixture"),
        };
        assert_eq!(stats.last_prompt.as_deref(), expected_prompt, "{phase}");
        if let Some(id) = state.session_id.as_ref() {
            let titles = metadata::read_titles_at(snapshot, std::slice::from_ref(id));
            let expected_title = match phase {
                "startup" | "clear" => None,
                "complete" => Some("Local fixture response."),
                _ => Some("Renamed fixture"),
            };
            assert_eq!(
                titles.get(id).map(String::as_str),
                expected_title,
                "{phase}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(cli.join("settings.json")).unwrap(),
        settings
    );
    let candidates = list_resumable_in(&cli, 10);
    assert!(
        candidates
            .iter()
            .any(|c| Some(&c.session_id) == conversation.as_ref()
                && c.first_prompt.as_deref() == Some("completion_fixture")
                && c.custom_title.as_deref() == Some("Renamed fixture"))
    );
}
