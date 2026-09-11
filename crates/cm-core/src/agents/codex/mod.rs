//! Codex has two host-selected execution adapters. `native` owns hooks,
//! rollouts and SQLite; `app_server` owns JSON-RPC and the attached TUI relay.
//! Running launchers keep their selected mode, even when the host changes its
//! setting. The dashboard continues to consume launcher state in either mode.

pub mod app_server;
mod native;
mod settings;
mod tui;

#[cfg(test)]
pub(crate) use native::read_resume_metadata_at;
pub use native::{
    build_hooks_settings, build_launch_command, dispatch_hook, parse_hook_payload,
    read_resume_metadata, read_session_index, read_thread_titles, read_transcript_stats,
    scan_transcript_signals, session_activity, thread_self_continues, title_store_mtimes,
    title_watch_path,
};
pub use settings::{CodexConfig, CodexMode};
pub(crate) use tui::{BIN, clipboard_paste_input, reattach_prime, uses_kitty_keyboard};

/// A dashboard may retry cleanup against an already waiting replacement only
/// during this initial window. Measured before opening it; never reset on retry.
/// Keep a margin beyond the host's full confirmation budget.
pub const RESTART_RETRY_WINDOW: std::time::Duration = app_server::HANDOFF_TIMEOUT
    .saturating_sub(app_server::CONTROL_TIMEOUT)
    .saturating_sub(std::time::Duration::from_secs(5));

pub fn list_resumable(limit: usize) -> anyhow::Result<Vec<crate::agent::ResumeCandidate>> {
    let config = crate::config::read_codex()?;
    match config.mode {
        CodexMode::Native => native::list_resumable(limit),
        CodexMode::AppServer => app_server::list_resumable(&config, limit),
    }
}

/// Restart opens the replacement window before ending the old launcher, to
/// preserve its terminal tab. Delay acquiring the thread until the old owner
/// has finished teardown; otherwise its final interrupt could hit the new TUI.
pub(crate) async fn wait_for_handoff(args: &[String], launcher_pid: u32) -> anyhow::Result<()> {
    wait_for_handoff_with(args, launcher_pid, crate::state::read_all_launcher_states).await
}

async fn wait_for_handoff_with(
    args: &[String],
    launcher_pid: u32,
    mut states: impl FnMut() -> Vec<crate::state::LauncherState>,
) -> anyhow::Result<()> {
    let Some(id) = args
        .iter()
        .position(|arg| arg == "resume")
        .and_then(|i| args.get(i + 1))
        .filter(|id| !id.starts_with('-'))
    else {
        return Ok(());
    };
    tokio::time::timeout(app_server::HANDOFF_TIMEOUT, async {
        loop {
            let owned = states().iter().any(|state|
                state.launcher_pid != launcher_pid && state.agent == crate::agent::AgentControl::Codex
                    && state.session_id.as_deref() == Some(id));
            if !owned { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }).await.map_err(|_| anyhow::anyhow!("Codex thread is still managed by another session; attach to it or stop it before resuming"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::{LauncherState, SessionStatus};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn resume_handoff_outlasts_a_slow_successful_cleanup() {
        let mut owner = LauncherState::for_test(AgentControl::Codex, SessionStatus::Idle);
        owner.launcher_pid = 11;
        owner.session_id = Some("resumed-thread".into());
        let args = vec!["resume".into(), "resumed-thread".into()];
        let start = tokio::time::Instant::now();
        wait_for_handoff_with(&args, 12, || {
            if start.elapsed() < Duration::from_secs(40) {
                vec![owner.clone()]
            } else {
                vec![]
            }
        })
        .await
        .expect("a successful cleanup must finish before its replacement gives up");
    }

    #[tokio::test(start_paused = true)]
    async fn resume_waits_for_old_owner_but_ignores_self_and_other_threads() {
        let mut owner = LauncherState::for_test(AgentControl::Codex, SessionStatus::Idle);
        owner.launcher_pid = 11;
        owner.session_id = Some("resumed-thread".into());
        let args = vec!["resume".into(), "resumed-thread".into()];
        let start = tokio::time::Instant::now();
        wait_for_handoff_with(&args, 12, || {
            if start.elapsed() < Duration::from_secs(1) {
                vec![owner.clone()]
            } else {
                vec![]
            }
        })
        .await
        .unwrap();
        assert!(start.elapsed() >= Duration::from_secs(1));
        let now = tokio::time::Instant::now();
        wait_for_handoff_with(&args, 11, || vec![owner.clone()])
            .await
            .unwrap();
        owner.session_id = Some("other-thread".into());
        wait_for_handoff_with(&args, 12, || vec![owner.clone()])
            .await
            .unwrap();
        assert_eq!(now.elapsed(), Duration::ZERO);
        owner.session_id = Some("resumed-thread".into());
        assert!(
            wait_for_handoff_with(&args, 12, || vec![owner.clone()])
                .await
                .is_err()
        );
        assert_eq!(now.elapsed(), app_server::HANDOFF_TIMEOUT);
    }
}
