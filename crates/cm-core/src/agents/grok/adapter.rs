//! Grok integration behavior and capability declarations.

use crate::agent::*;
use crate::agents::definition::AgentDefinition;
use crate::agents::grok;
use crate::state::{HookEvent, HookMessage};
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub(crate) struct Definition;

impl AgentDefinition for Definition {
    fn kind(&self) -> AgentKind {
        AgentKind::Grok
    }
    fn cli_subcommand(&self) -> &'static str {
        "grok"
    }
    fn label(&self) -> &'static str {
        "Grok Build"
    }
    fn resume_args(&self, session_id: &str, fork: bool) -> Vec<String> {
        let mut v = vec!["--resume".to_string(), session_id.to_string()];
        if fork {
            v.push("--fork-session".to_string());
        }
        v
    }
    fn worktree_args(&self, name: Option<&str>) -> Option<Vec<String>> {
        Some(match name.filter(|n| !n.is_empty()) {
            Some(name) => vec![format!("--worktree={name}")],
            // Nothing follows it in our argv, so the bare flag is safe here —
            // and it is the only way to ask Grok to mint the name.
            None => vec!["--worktree".to_string()],
        })
    }
    fn uses_alt_screen(&self, agent_args: &[String]) -> bool {
        grok::uses_alt_screen(agent_args)
    }
    fn uses_kitty_keyboard(&self, term: &str, alt_screen: bool) -> bool {
        alt_screen && grok::uses_kitty_keyboard(term)
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            fork: true,
            worktrees: true,
            approval_gate: true,
            context_tokens: true,
        }
    }
    fn read_transcript_stats(
        &self,
        transcript: &Path,
        _prior: Option<&TranscriptStats>,
    ) -> TranscriptStats {
        grok::read_transcript_stats(transcript)
    }
    fn list_resumable_cached(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        grok::list_resumable_cached(limit, cache)
    }
    fn build_launch_command(
        &self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
        shim_dir: Option<&Path>,
    ) -> Result<Command> {
        grok::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
    }
    fn hooks_settings_json(&self, sock_path: &str) -> String {
        grok::build_hooks_settings(sock_path)
    }
    fn hook_session(&self) -> Box<dyn crate::agents::hook_session::HookSession> {
        Box::new(grok::Hooks::default())
    }
    fn parse_hook_payload(&self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        grok::parse_hook_payload(event, stdin)
    }
    fn transcript_path_is_replaced(&self) -> bool {
        true
    }
    fn replaced_sidecar_paths(&self, transcript: &Path) -> Vec<PathBuf> {
        let dir = transcript.parent().unwrap_or(transcript);
        vec![dir.join("signals.json")]
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
    fn bg_shells(&self, _agent_pid: u32) -> Option<Vec<BgShell>> {
        None
    }
}
