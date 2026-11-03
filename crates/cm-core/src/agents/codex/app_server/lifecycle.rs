//! Keep server-owned work supervised through cleanup and state-store failures.
use super::{Monitor, Relay, monitor::Observation};
use crate::agent::session::SessionPublisher;
use crate::state::{LauncherState, SessionStatus};
use anyhow::{Context, Result};

/// Persistence is secondary to control ownership: publish without waiting on
/// the filesystem. The launcher retains the latest row and retries a failed
/// write, so neither the TUI nor an RPC reply waits on the state store.
fn publish(updates: &SessionPublisher, state: &LauncherState) {
    if let Err(error) = updates.publish(state) {
        tracing::warn!("Could not publish Codex session state: {error:#}");
    }
}
async fn observe(
    monitor: &mut Monitor,
    state: &mut LauncherState,
    event: Observation,
    updates: &SessionPublisher,
) {
    let before = state.clone();
    monitor.apply(state, event);
    if *state != before {
        state.updated_at = LauncherState::now();
        publish(updates, state);
    }
}

/// Responses must keep draining while the relay fences in-flight requests:
/// otherwise its bounded observation queue can prevent the acknowledgement.
async fn quiesce(
    paused: bool,
    relay: &mut Relay,
    monitor: &mut Monitor,
    state: &mut LauncherState,
    updates: &SessionPublisher,
) -> Result<()> {
    let pause = relay.pause_input(paused);
    tokio::pin!(pause);
    loop {
        tokio::select! {
            result = &mut pause => return result,
            event = relay.events.recv() => {
                let event = event.context("Codex relay stopped")?;
                observe(monitor, state, event, updates).await;
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
    updates: SessionPublisher,
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
    publish(&updates, state);
    loop {
        let begin_stop = tokio::select! {
            event = relay.events.recv(), if relay_live => {
                match event {
                    Some(event) => {
                        observe(&mut monitor, state, event, &updates).await;
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
                if request.is_forced() {
                    // Explicit Kill already received a qualifying cleanup
                    // failure. End and reap our child before giving up control.
                    let _ = child.start_kill();
                    let result = child.wait().await.context("waiting for Codex TUI to exit");
                    request.reply(result.as_ref().err().map(|error| format!("{error:#}"))).await;
                    result?;
                    relay.cleanup_environment().await;
                    return Ok(exit_code);
                }
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
                        relay.cleanup_environment().await;
                        return Ok(exit_code);
                    }
                    Err(error) => {
                        let message = format!("Codex cleanup failed: {error:#}");
                        tracing::warn!("{message}");
                        state.set_last_error(Some(message.clone()));
                        state.cleanup = Some(crate::state::CleanupStatus::Failed { message: message.clone() });
                        if !child_live {
                            state.codex_connected = Some(false);
                            state.status = SessionStatus::Starting;
                            state.active_since = None;
                        }
                        state.updated_at = LauncherState::now();
                        publish(&updates, state);
                        let _ = quiesce(false, &mut relay, &mut monitor, state, &updates).await;
                        if let Some(reply) = reply.take() {
                            let reason = super::kill::force_removal_reason(config, &error, state.session_id.as_deref()).await;
                            reply.respond(Some(message), Some(reason.unwrap_or(super::control::ForceRemoval::Denied))).await;
                        }
                        // Preserve the launcher and its state for a retry, including
                        // when the TUI/window has already closed. Never resend input.
                        false
                    }
                }
            }
        };
        if begin_stop {
            state.cleanup = Some(crate::state::CleanupStatus::InProgress);
            state.updated_at = LauncherState::now();
            publish(&updates, state);
            let pause = quiesce(true, &mut relay, &mut monitor, state, &updates).await;
            // A lifecycle reply can already have reached the TUI while its
            // observation is queued here. Adopt it before selecting cleanup's
            // thread; the paused input gate prevents a new turn overtaking us.
            while let Ok(event) = relay.events.try_recv() {
                observe(&mut monitor, state, event, &updates).await;
            }
            let config = config.clone();
            let snapshot = state.clone();
            let turn = monitor.turn.clone();
            let explicit_kill = reply
                .as_ref()
                .is_some_and(|request| request.allows_internal_creations());
            stopping = Some(Box::pin(async move {
                tokio::time::timeout(super::CLEANUP_TIMEOUT, async {
                    app_server::stop_after_quiescence(
                        &config,
                        &snapshot,
                        turn.as_deref(),
                        pause,
                        explicit_kill,
                    )
                    .await
                })
                .await
                .context("Codex cleanup timed out")?
            }));
        }
    }
}
