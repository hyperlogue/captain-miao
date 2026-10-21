use super::*;
use std::io::Write;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

#[tokio::test]
async fn hook_only_transcripts_refresh_only_when_hooks_arrive() {
    if std::env::var_os("CM_TEST_HOOK_METADATA").is_none() {
        let root = tempfile::Builder::new()
            .prefix("cm-hook-metadata-")
            .tempdir_in("/tmp")
            .unwrap();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launcher::io_tests::hook_only_transcripts_refresh_only_when_hooks_arrive",
                "--nocapture",
            ])
            .env("CM_TEST_HOOK_METADATA", "1")
            .env("XDG_STATE_HOME", root.path())
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "isolated hook test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    state::ensure_sessions_dir().unwrap();
    let root = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let state_path = state::sessions_dir().join(format!("{}.json", std::process::id()));
    for agent in [AgentControl::Kimi, AgentControl::Antigravity] {
        let path = root.join(format!("{}.jsonl", agent.cli_subcommand()));
        let socket = root.join(format!("{}.sock", agent.cli_subcommand()));
        let mut listener = UnixListener::bind(&socket).unwrap();
        append(&path, agent, 1);
        let watched_socket = socket.clone();
        let task = tokio::spawn(async move {
            let mut state = LauncherState::for_test(agent, SessionStatus::Starting);
            state.launcher_pid = std::process::id();
            process_hooks(&mut listener, &watched_socket, &mut state).await;
        });
        let first = if agent == AgentControl::Kimi {
            HookEvent::PermissionRequest
        } else {
            HookEvent::PromptSubmit
        };
        send(&socket, &path, first).await;
        wait_for_value(&state_path, agent, 1, "first hook metadata").await;
        let before = std::fs::read(&state_path).unwrap();
        append(&path, agent, 2);
        // Longer than the former two-second poll and the write throttle.
        // Both real file events and a resurrected timer would change this row.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            std::fs::read(&state_path).unwrap(),
            before,
            "{agent:?}: metadata changed without a hook"
        );
        let next = if agent == AgentControl::Kimi {
            HookEvent::ElicitationResult
        } else {
            HookEvent::PromptSubmit
        };
        send(&socket, &path, next).await;
        let state = wait_for_value(&state_path, agent, 2, "subsequent hook metadata").await;
        assert_eq!(state.status, SessionStatus::Active);
        task.abort();
        let _ = task.await;
    }
}

fn append(path: &Path, agent: AgentControl, value: u64) {
    let line = if agent == AgentControl::Kimi {
        serde_json::json!({"type":"usage.record","model":"fixture-model","usage":{"inputOther":value}})
    } else {
        serde_json::json!({"type":"USER_INPUT","content":format!("<USER_REQUEST>prompt-{value}</USER_REQUEST>")})
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

async fn send(socket: &Path, transcript: &Path, event: HookEvent) {
    let message: HookMessage = serde_json::from_value(serde_json::json!({
        "event": event, "transcript_path":transcript, "session_id":"metadata-fixture"
    }))
    .unwrap();
    let mut stream = UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(&serde_json::to_vec(&message).unwrap())
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
}

async fn wait_for_value(
    path: &Path,
    agent: AgentControl,
    value: u64,
    phase: &str,
) -> LauncherState {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(state) = std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<LauncherState>(&bytes).ok())
                && state.agent == agent
                && if agent == AgentControl::Kimi {
                    state.context_tokens == Some(value)
                } else {
                    state.last_prompt.as_deref() == Some(format!("prompt-{value}").as_str())
                }
            {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{agent:?}: waiting for {phase}"))
}
