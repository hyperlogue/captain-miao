//! Keep server-owned work supervised through cleanup and state-store failures.
use super::{Monitor, Relay, monitor::Observation};
use crate::state::{LauncherState, SessionStatus};
use anyhow::{Context, Result};
use std::time::Duration;
use tokio::time::Instant;

/// Persistence is secondary to control ownership. Retry a failed write without
/// losing the current facts, terminating the TUI, or suppressing an RPC reply.
#[derive(Default)]
struct StateWriter {
    retry_at: Option<Instant>,
}
impl StateWriter {
    fn write(&mut self, state: &LauncherState) {
        if self.retry_at.is_some_and(|at| at > Instant::now()) {
            return;
        }
        self.retry_at = match crate::state::ensure_sessions_dir().and_then(|_| state.write()) {
            Ok(()) => None,
            Err(error) => {
                tracing::warn!(
                    "Could not persist Codex session; retaining control and retrying: {error:#}"
                );
                Some(Instant::now() + Duration::from_secs(1))
            }
        };
    }
}
async fn retry_at(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
fn observe(
    monitor: &mut Monitor,
    state: &mut LauncherState,
    event: Observation,
    writer: &mut StateWriter,
) {
    let before = state.clone();
    monitor.apply(state, event);
    if *state != before {
        state.updated_at = LauncherState::now();
        writer.write(state);
    }
}

/// Responses must keep draining while the relay fences in-flight requests:
/// otherwise its bounded observation queue can prevent the acknowledgement.
async fn quiesce(
    paused: bool,
    relay: &mut Relay,
    monitor: &mut Monitor,
    state: &mut LauncherState,
    writer: &mut StateWriter,
) -> Result<()> {
    let pause = relay.pause_input(paused);
    tokio::pin!(pause);
    loop {
        tokio::select! {
            result = &mut pause => return result,
            event = relay.events.recv() => {
                let event = event.context("Codex relay stopped")?;
                observe(monitor, state, event, writer);
            }
        }
    }
}

pub(crate) async fn supervise(
    config: &crate::agents::codex::CodexConfig,
    state: &mut LauncherState,
    mut child: tokio::process::Child,
    mut relay: crate::agents::codex::app_server::Relay,
    mut control: crate::agents::codex::app_server::Control,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<i32> {
    use crate::agents::codex::app_server;
    use futures_util::future::BoxFuture;
    let mut monitor = app_server::Monitor::default();
    tokio::pin!(shutdown);
    let mut shutdown_seen = false;
    let mut relay_live = true;
    let mut control_live = true;
    let mut child_live = true;
    let mut exit_code = 0;
    let mut stopping: Option<BoxFuture<'static, Result<()>>> = None;
    let mut reply: Option<app_server::StopRequest> = None;
    let mut persistence = StateWriter::default();
    persistence.write(state);
    loop {
        let begin_stop = tokio::select! {
            _ = retry_at(persistence.retry_at) => {
                persistence.write(state);
                false
            }
            event = relay.events.recv(), if relay_live => {
                match event {
                    Some(event) => {
                        observe(&mut monitor, state, event, &mut persistence);
                        false
                    }
                    None => {
                        relay_live = false;
                        stopping.is_none()
                    }
                }
            }
            request = control.requests.recv(), if control_live => {
                let Some(request) = request else {
                    control_live = false;
                    continue;
                };
                if stopping.is_some() {
                    request.reply(Some("Codex cleanup is already in progress; retry shortly".into())).await;
                    false
                } else {
                    reply = Some(request);
                    true
                }
            }
            result = child.wait(), if child_live => {
                child_live = false;
                state.child_pid = None;
                exit_code = match result {
                    Ok(status) => status.code().unwrap_or(1),
                    Err(error) => {
                        tracing::warn!("Could not wait for Codex TUI: {error}");
                        1
                    }
                };
                stopping.is_none()
            }
            _ = &mut shutdown, if !shutdown_seen => {
                shutdown_seen = true;
                exit_code = 143;
                stopping.is_none()
            }
            result = async {
                match stopping.as_mut() {
                    Some(future) => future.await,
                    None => std::future::pending().await,
                }
            } => {
                stopping = None;
                match result {
                    Ok(()) => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        if let Some(reply) = reply.take() { reply.reply(None).await; }
                        return Ok(exit_code);
                    }
                    Err(error) => {
                        let message = format!("Codex cleanup failed: {error:#}");
                        tracing::warn!("{message}");
                        state.last_error = Some(message.clone());
                        if !child_live {
                            state.codex_connected = Some(false);
                            state.status = SessionStatus::Starting;
                            state.active_since = None;
                        }
                        state.updated_at = LauncherState::now();
                        persistence.write(state);
                        let _ = quiesce(false, &mut relay, &mut monitor, state, &mut persistence).await;
                        if let Some(reply) = reply.take() { reply.reply(Some(message)).await; }
                        // Preserve the launcher and its state for a retry, including
                        // when the TUI/window has already closed. Never resend input.
                        false
                    }
                }
            }
        };
        if begin_stop {
            let pause = quiesce(true, &mut relay, &mut monitor, state, &mut persistence).await;
            // A lifecycle reply can already have reached the TUI while its
            // observation is queued here. Adopt it before selecting cleanup's
            // thread; the paused input gate prevents a new turn overtaking us.
            while let Ok(event) = relay.events.try_recv() {
                observe(&mut monitor, state, event, &mut persistence);
            }
            let config = config.clone();
            let snapshot = state.clone();
            let turn = monitor.turn.clone();
            stopping = Some(Box::pin(async move {
                tokio::time::timeout(super::CLEANUP_TIMEOUT, async {
                    super::confirm_quiescence(&config, pause).await?;
                    app_server::stop(&config, &snapshot, turn.as_deref()).await
                })
                .await
                .context("Codex cleanup timed out")?
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_recreates_a_deleted_session_directory() {
        if std::env::var_os("CM_TEST_CODEX_STATE_RECOVERY").is_some() {
            let row =
                LauncherState::for_test(crate::agent::AgentControl::Codex, SessionStatus::Idle);
            let mut writer = StateWriter::default();
            writer.write(&row);
            assert!(writer.retry_at.is_none());
            std::fs::remove_dir_all(crate::state::sessions_dir()).unwrap();
            writer.write(&row);
            assert!(
                writer.retry_at.is_none(),
                "retry must recreate the parent, not fail forever"
            );
            assert!(
                crate::state::sessions_dir()
                    .join(format!("{}.json", row.launcher_pid))
                    .is_file()
            );
            return;
        }
        // Isolate directory deletion from every other test's session fixtures.
        let root = std::env::temp_dir().join(format!("miao-state-recovery-{}", std::process::id()));
        crate::state::create_dir_all_private(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agents::codex::app_server::lifecycle::tests::persistence_recreates_a_deleted_session_directory"])
            .env("CM_TEST_CODEX_STATE_RECOVERY", "1")
            .env("XDG_STATE_HOME", &root)
            .output().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "isolated persistence recovery failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
