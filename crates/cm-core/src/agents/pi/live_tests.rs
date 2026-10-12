//! Opt-in compatibility test with an installed Pi, isolated settings and a
//! loopback model fixture. No account or external inference is used.

use super::*;
use crate::agent::AgentControl;
use crate::state::SessionStatus;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
#[ignore = "requires Pi on PATH or CM_TEST_PI_BIN, plus a loopback listener"]
fn real_pi_loads_extension_and_tracks_conversations() {
    let mut child = Command::new("node")
        .args(["--input-type=module", "-e", include_str!("live_test.mjs")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run through nix develop for Node.js");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({"source": EXTENSION.source("__MIAO_TEST_HOOK__")})
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
    let mut state = LauncherState::for_test(AgentControl::Pi, SessionStatus::Starting);
    let mut original_id = None;
    for checkpoint in checkpoints.as_array().unwrap() {
        let phase = checkpoint["phase"].as_str().unwrap();
        let events = checkpoint["events"].as_array().unwrap();
        assert!(
            !events.is_empty(),
            "{phase}: the real Pi must invoke our extension"
        );
        for event in events {
            let kind = HookEvent::from_kebab(event["event"].as_str().unwrap()).unwrap();
            let message = parse_hook_payload(kind, &event["body"].to_string()).unwrap();
            dispatch_hook(&mut state, message);
        }
        match phase {
            "startup" => {
                assert_eq!(state.status, SessionStatus::Idle);
                original_id = state.session_id.clone();
                assert!(original_id.is_some());
            }
            "draft" => {
                for expected in ["prompt-submit", "pre-tool-use", "post-tool-use", "stop"] {
                    assert!(
                        events.iter().any(|event| event["event"] == expected),
                        "missing {expected}"
                    );
                }
                assert_eq!(state.status, SessionStatus::Idle);
                assert_eq!(
                    state.last_prompt.as_deref(),
                    Some("Read notes.md and draft a paragraph.")
                );
                assert!(state.context_tokens.is_some_and(|tokens| tokens > 0));
            }
            "rename" => assert_eq!(state.name.as_deref(), Some("Essay draft")),
            "model" => {
                assert_eq!(state.model.as_deref(), Some("miao-test/reviewer"));
                assert_eq!(state.status, SessionStatus::Idle);
            }
            "abort" => {
                assert_eq!(state.status, SessionStatus::Idle);
                assert_eq!(state.last_error, None);
            }
            "compact" => assert_eq!(state.status, SessionStatus::Compacted),
            "new" => {
                assert_ne!(state.session_id, original_id);
                assert_eq!(state.name, None);
                assert_eq!(state.last_prompt, None);
                assert_eq!(state.status, SessionStatus::Idle);
            }
            "resume" => {
                assert_eq!(state.session_id, original_id);
                assert_eq!(state.name.as_deref(), Some("Essay draft"));
            }
            "fork" => {
                assert_ne!(state.session_id, original_id);
                assert_eq!(state.status, SessionStatus::Idle);
            }
            _ => panic!("unknown checkpoint {phase}"),
        }
    }
}
