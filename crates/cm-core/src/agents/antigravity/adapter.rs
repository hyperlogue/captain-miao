//! Antigravity integration behavior and capability declarations.

use crate::agent::*;
use crate::agents::antigravity;
use crate::agents::definition::AgentDefinition;
use crate::state::{HookEvent, HookMessage, LauncherState};
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub(crate) struct Definition;

impl AgentDefinition for Definition {
    fn title_source(&self) -> Option<crate::agents::definition::TitleSource> {
        Some(crate::agents::definition::TitleSource {
            stamp: antigravity::title_store_mtimes,
            read: antigravity::read_titles,
            accepts: |_| true,
        })
    }

    fn transcript_reader(&self) -> Box<dyn crate::agents::transcript::TranscriptReader> {
        Box::new(super::metadata::Reader::default())
    }
    fn last_prompt_is_live(&self) -> bool {
        true
    }
    fn kind(&self) -> AgentKind {
        AgentKind::Antigravity
    }
    fn cli_subcommand(&self) -> &'static str {
        "antigravity"
    }
    fn label(&self) -> &'static str {
        "Antigravity"
    }
    fn resume_args(&self, session_id: &str, _fork: bool) -> Vec<String> {
        vec!["--conversation".to_string(), session_id.to_string()]
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            fork: false,
            worktrees: false,
            approval_gate: true,
            context_tokens: true,
        }
    }
    fn watch_paths(&self) -> Vec<PathBuf> {
        antigravity::title_watch_paths()
    }
    fn out_of_band_watch_paths(&self) -> Vec<PathBuf> {
        antigravity::title_watch_paths()
    }
    fn list_resumable_cached(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        antigravity::list_resumable_cached(limit, cache)
    }
    fn build_launch_command(
        &self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
        shim_dir: Option<&Path>,
    ) -> Result<Command> {
        antigravity::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
    }
    fn hooks_settings_json(&self, sock_path: &str) -> String {
        antigravity::build_hooks_settings(sock_path)
    }
    fn hook_session(&self) -> Box<dyn crate::agents::hook_session::HookSession> {
        Box::new(Session::default())
    }
    fn parse_hook_payload(&self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        antigravity::parse_hook_payload(event, stdin)
    }
    fn refresh_transcript_on_hook(&self) -> bool {
        true
    }
    fn self_continues(&self, _session_id: &str) -> bool {
        false
    }
    fn may_self_continue(&self) -> bool {
        false
    }
    fn scan_transcript_signals(&self, _path: &Path, _offset: u64) -> TranscriptScan {
        TranscriptScan::default()
    }
    fn agent_activity(&self, _agent_pid: u32) -> Option<AgentActivity> {
        None
    }
    fn transcript_write_clears_approval(&self) -> bool {
        false
    }
    fn transcript_poll_interval(&self) -> Option<std::time::Duration> {
        None
    }
    fn transcript_path_is_replaced(&self) -> bool {
        false
    }
    fn bg_shells(&self, _agent_pid: u32) -> Option<Vec<BgShell>> {
        None
    }
}

#[derive(Default)]
struct Session {
    tasks: Option<super::tasks::Monitor>,
    pending: Option<super::tasks::Update>,
}
impl crate::agents::hook_session::HookSession for Session {
    fn start(&mut self) {
        self.tasks = Some(super::tasks::Monitor::new());
    }
    fn dispatch_hook(&mut self, state: &mut LauncherState, message: HookMessage) {
        match &mut self.tasks {
            Some(tasks) => tasks.dispatch_hook(state, message),
            None => super::dispatch_hook(state, message),
        }
    }
    fn changed(&mut self) -> futures_util::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            match &mut self.tasks {
                Some(tasks) => self.pending = Some(tasks.changed().await),
                None => std::future::pending().await,
            }
        })
    }
    fn apply_update(&mut self, state: &mut LauncherState) -> bool {
        match (self.tasks.as_mut(), self.pending.take()) {
            (Some(tasks), Some(update)) => tasks.apply(update, state),
            _ => false,
        }
    }
    fn transcript_changed(&mut self, state: &mut LauncherState) {
        state.first_prompt = None;
        state.last_prompt = None;
    }
}
