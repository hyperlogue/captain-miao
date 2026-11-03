//! Pi integration behavior and capability declarations.

use crate::agent::*;
use crate::agents::definition::AgentDefinition;
use crate::agents::pi;
use crate::state::{HookEvent, HookMessage};
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

pub(crate) struct Definition;

impl AgentDefinition for Definition {
    fn kind(&self) -> AgentKind {
        AgentKind::Pi
    }
    fn cli_subcommand(&self) -> &'static str {
        "pi"
    }
    fn label(&self) -> &'static str {
        "Pi"
    }
    fn resume_args(&self, session_id: &str, fork: bool) -> Vec<String> {
        let flag = if fork { "--fork" } else { "--session" };
        vec![flag.to_string(), session_id.to_string()]
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            fork: true,
            worktrees: false,
            approval_gate: false,
            context_tokens: true,
        }
    }
    fn list_resumable_cached(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        pi::list_resumable_cached(limit, cache)
    }
    fn build_launch_command(
        &self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
        shim_dir: Option<&Path>,
    ) -> Result<Command> {
        pi::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
    }
    fn hooks_settings_json(&self, sock_path: &str) -> String {
        pi::build_hooks_settings(sock_path)
    }
    fn hook_session(&self) -> Box<dyn crate::agents::hook_session::HookSession> {
        Box::new(crate::agents::hook_session::Stateless(pi::dispatch_hook))
    }
    fn parse_hook_payload(&self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        pi::parse_hook_payload(event, stdin)
    }
    fn last_prompt_is_live(&self) -> bool {
        false
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
    fn refresh_transcript_on_hook(&self) -> bool {
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
