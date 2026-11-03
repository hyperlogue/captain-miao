//! Codex integration behavior and capability declarations.

use crate::agent::*;
use crate::agents::codex;
use crate::agents::definition::AgentDefinition;
use crate::state::{HookEvent, HookMessage, LauncherState, ReattachPrime};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

pub(crate) struct Definition;

impl AgentDefinition for Definition {
    fn title_source(&self) -> Option<crate::agents::definition::TitleSource> {
        Some(crate::agents::definition::TitleSource {
            stamp: codex::title_store_mtimes,
            read: codex::read_thread_titles,
            accepts: |row| row.codex_mode.is_native(),
        })
    }

    fn stop_session(
        &self,
        state: &LauncherState,
        policy: crate::backend::CleanupPolicy,
    ) -> Result<crate::backend::SessionRemoval> {
        if state.codex_mode.is_native() {
            crate::backend::signal_launcher(state.launcher_pid)
        } else if state.codex_control {
            super::app_server::kill(state, policy)
        } else if state.status == crate::state::SessionStatus::FailedToStart {
            crate::backend::signal_launcher(state.launcher_pid)
        } else {
            anyhow::bail!("This Codex launcher cannot confirm cleanup yet; use an updated launcher")
        }
    }

    fn configure(
        &self,
        config: &crate::config::AgentConfig,
    ) -> Result<Box<dyn crate::agent::session::AgentAdapter>> {
        let approval_grace = config.approval_grace;
        let config = config
            .codex
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))?
            .clone();
        Ok(Box::new(Adapter {
            config,
            approval_grace,
        }))
    }

    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }
    fn cli_subcommand(&self) -> &'static str {
        "codex"
    }
    fn clipboard_paste_encoder(&self) -> Option<fn(&Path) -> Vec<u8>> {
        Some(codex::clipboard_paste_input)
    }
    fn bin(&self) -> Option<&'static str> {
        Some(codex::BIN)
    }
    fn label(&self) -> &'static str {
        "Codex"
    }
    fn read_resume_metadata(&self, args: &[String]) -> Option<ResumeMetadata> {
        codex::read_resume_metadata(args)
    }
    fn resume_args(&self, session_id: &str, fork: bool) -> Vec<String> {
        let sub = if fork { "fork" } else { "resume" };
        vec![sub.to_string(), session_id.to_string()]
    }
    fn uses_kitty_keyboard(&self, _term: &str, _alt_screen: bool) -> bool {
        codex::uses_kitty_keyboard()
    }
    fn reattach_prime(&self, _alt_screen: bool, kitty_keyboard: bool) -> ReattachPrime {
        codex::reattach_prime(kitty_keyboard)
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            fork: true,
            worktrees: false,
            approval_gate: true,
            context_tokens: true,
        }
    }
    fn watch_paths(&self) -> Vec<PathBuf> {
        codex::title_watch_path().into_iter().collect()
    }
    fn out_of_band_watch_paths(&self) -> Vec<PathBuf> {
        codex::title_watch_path().into_iter().collect()
    }
    fn read_transcript_stats(
        &self,
        transcript: &Path,
        prior: Option<&TranscriptStats>,
    ) -> TranscriptStats {
        codex::read_transcript_stats(transcript, prior)
    }
    fn list_resumable_cached(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        super::native::list_resumable_cached(limit, cache)
    }
    fn build_launch_command(
        &self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
        shim_dir: Option<&Path>,
    ) -> Result<Command> {
        codex::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
    }
    fn hooks_settings_json(&self, sock_path: &str) -> String {
        codex::build_hooks_settings(sock_path)
    }
    fn hook_session(&self) -> Box<dyn crate::agents::hook_session::HookSession> {
        Box::new(crate::agents::hook_session::Stateless(codex::dispatch_hook))
    }
    fn parse_hook_payload(&self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        codex::parse_hook_payload(event, stdin)
    }
    fn self_continues(&self, session_id: &str) -> bool {
        codex::thread_self_continues(session_id)
    }
    fn may_self_continue(&self) -> bool {
        true
    }
    fn scan_transcript_signals(&self, path: &Path, offset: u64) -> TranscriptScan {
        codex::scan_transcript_signals(path, offset)
    }
    fn transcript_write_clears_approval(&self) -> bool {
        true
    }
    fn refresh_transcript_on_hook(&self) -> bool {
        self.transcript_poll_interval().is_some()
    }
    fn transcript_poll_interval(&self) -> Option<Duration> {
        if cfg!(target_os = "macos") {
            return Some(Duration::from_secs(2));
        }
        None
    }
}

/// Host configuration is injected once and captured by each opaque task.
struct Adapter {
    config: super::CodexConfig,
    approval_grace: Duration,
}

impl crate::agent::session::AgentAdapter for Adapter {
    fn run(
        &self,
        mut session: crate::agent::session::SessionContext,
    ) -> crate::agent::session::SessionTask {
        let config = self.config.clone();
        let grace = self.approval_grace;
        Box::pin(async move {
            session.state.codex_mode = config.mode;
            session.updates.write(&session.state).await?;
            match config.mode {
                super::CodexMode::Native => {
                    tokio::select! {
                        biased;
                        _ = session.commands.stop() => return Ok(crate::agent::session::SessionExit { code: 143 }),
                        ready = super::wait_for_handoff(&session.request.args, session.state.launcher_pid) => ready?,
                    }
                    crate::agents::hooked::run(&Definition, session, grace).await
                }
                super::CodexMode::AppServer => super::app_server::run(config, session).await,
            }
        })
    }

    fn list_resumable(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        match self.config.mode {
            super::CodexMode::Native => super::native::list_resumable_cached(limit, cache),
            super::CodexMode::AppServer => super::app_server::list_resumable(&self.config, limit),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::*;
    use crate::state::{LauncherState, SessionStatus};
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;

    async fn captured_mode(adapter: Box<dyn AgentAdapter>) -> super::super::CodexMode {
        let root = tempfile::Builder::new()
            .prefix("cm-adapter-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("session.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (updates, mut incoming) = SessionPublisher::channel();
        let (commands, receiver) = mpsc::unbounded_channel();
        let session = SessionContext {
            request: LaunchRequest {
                cwd: root.path().to_string_lossy().into_owned(),
                args: vec![],
                socket,
                settings: root.path().join("settings.json"),
                shim: None,
            },
            state: LauncherState::for_test(AgentKind::Codex, SessionStatus::Starting),
            listener,
            updates,
            commands: SessionCommands(receiver),
        };
        let task = tokio::spawn(adapter.run(session));
        drop(adapter);
        let first = incoming
            .recv()
            .await
            .expect("adapter publishes captured policy");
        let mode = first.state.codex_mode;
        // Cancel before acknowledging publication: no handoff or upstream
        // connection may start, and no real agent executable is required.
        commands.send(SessionCommand::Stop).unwrap();
        first.receipt.unwrap().send(Ok(())).unwrap();
        let exit = tokio::time::timeout(
            super::super::app_server::HANDOFF_TIMEOUT + Duration::from_secs(5),
            task,
        )
        .await
        .expect("adapter did not acknowledge startup cancellation")
        .unwrap()
        .unwrap();
        assert_eq!(exit.code, 143);
        mode
    }

    #[tokio::test]
    async fn configured_adapter_and_its_task_keep_their_configuration_snapshot() {
        let mut host = crate::config::AgentConfig {
            codex: Ok(super::super::CodexConfig::default()),
            approval_grace: Duration::from_secs(2),
        };
        let original = Definition.configure(&host).unwrap();
        host.codex.as_mut().unwrap().mode = super::super::CodexMode::AppServer;
        let replacement = Definition.configure(&host).unwrap();
        assert_eq!(
            captured_mode(original).await,
            super::super::CodexMode::Native
        );
        assert_eq!(
            captured_mode(replacement).await,
            super::super::CodexMode::AppServer
        );
    }

    #[test]
    fn malformed_codex_configuration_does_not_disable_unrelated_adapters() {
        let host = crate::config::AgentConfig {
            codex: Err("invalid Codex setting".into()),
            approval_grace: Duration::from_secs(2),
        };
        for &kind in AgentKind::ALL {
            assert_eq!(kind.configure(&host).is_err(), kind == AgentKind::Codex);
        }
        assert!(AgentKind::Unknown.configure(&host).is_err());
    }
}
