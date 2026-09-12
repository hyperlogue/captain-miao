use super::*;
use crate::backend::{CleanupPolicy, LocalBackend};
use crate::state::{self, SessionStatus};

#[tokio::test]
async fn kill_cancels_handoff_and_connection_before_starting_codex() {
    const TEST: &str = "agents::codex::app_server::startup_tests::kill_cancels_handoff_and_connection_before_starting_codex";
    if std::env::var_os("CM_TEST_CODEX_STARTUP").is_none() {
        for phase in ["handoff", "connect"] {
            let root = Path::new("/tmp").join(format!("miao-start-{}-{phase}", std::process::id()));
            state::create_dir_all_private(&root).unwrap();
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("CM_TEST_CODEX_STARTUP", phase)
                .env("XDG_CONFIG_HOME", &root)
                .env("XDG_STATE_HOME", &root)
                .env("XDG_RUNTIME_DIR", &root)
                .output()
                .await
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let phase = std::env::var("CM_TEST_CODEX_STARTUP").unwrap();
    if std::env::var_os("CM_TEST_CODEX_STARTUP_LAUNCHER").is_some() {
        let args = if phase == "handoff" {
            vec!["resume".into(), "saved-thread".into()]
        } else {
            vec![]
        };
        crate::launcher::run(
            AgentControl::Codex,
            "/tmp",
            &args,
            None,
            Some("startup-test".into()),
            crate::cli::ClipboardShims::Skip,
        )
        .await
        .unwrap();
        return;
    }
    state::ensure_sessions_dir().unwrap();
    state::create_dir_all_private(&state::runtime_dir()).unwrap();
    let endpoint = state::runtime_dir().join("codex.sock");
    let config = CodexConfig {
        mode: super::super::CodexMode::AppServer,
        endpoint: format!("unix://{}", endpoint.display()),
    };
    crate::config::write_codex(&config).unwrap();
    // Retain a live owner for handoff, or a socket that never finishes the
    // WebSocket handshake for preflight. Neither may prevent cancellation.
    let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
    let mut owner = LauncherState::for_test(AgentControl::Codex, SessionStatus::Idle);
    owner.launcher_pid = std::process::id();
    owner.session_id = Some("saved-thread".into());
    owner.write().unwrap();
    let mut launcher = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env("CM_TEST_CODEX_STARTUP_LAUNCHER", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = launcher.id().unwrap();
    let path = state::sessions_dir().join(format!("{pid}.json"));
    let ready = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(row) = serde_json::from_slice::<LauncherState>(&bytes)
                && row.codex_control
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let stalled_connection = if phase == "connect" {
        Some(
            tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap()
                .0,
        )
    } else {
        None
    };
    let key = state::SessionKey::from_launcher_pid(pid);
    let result = tokio::task::spawn_blocking(move || {
        LocalBackend::kill_session(&key, CleanupPolicy::ForceIfUnavailable)
    })
    .await
    .unwrap();
    if result.is_err() || ready.is_err() {
        launcher.kill().await.unwrap();
    }
    let output = tokio::time::timeout(Duration::from_secs(5), launcher.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    drop(listener);
    drop(stalled_connection);
    assert!(result.is_ok(), "cannot cancel {phase}: {result:?}");
    assert!(ready.is_ok(), "startup never published control");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!path.exists());
    assert!(
        !state::runtime_dir()
            .join("launchers")
            .join(format!("{pid}.sock"))
            .exists()
    );
    assert!(state::is_process_alive(owner.launcher_pid));
}
