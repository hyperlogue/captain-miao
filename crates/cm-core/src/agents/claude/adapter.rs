//! Claude integration behavior and capability declarations.

use crate::agent::*;
use crate::agents::claude;
use crate::agents::definition::AgentDefinition;
use crate::state::{HookEvent, HookMessage};
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub(crate) struct Definition;

impl AgentDefinition for Definition {
    fn transcript_reader(&self) -> Box<dyn crate::agents::transcript::TranscriptReader> {
        Box::new(super::Reader::default())
    }
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }
    fn cli_subcommand(&self) -> &'static str {
        "claude"
    }
    fn bin(&self) -> Option<&'static str> {
        Some(claude::BIN)
    }
    fn label(&self) -> &'static str {
        "Claude"
    }
    fn resume_args(&self, session_id: &str, fork: bool) -> Vec<String> {
        let mut v = vec!["--resume".to_string(), session_id.to_string()];
        if fork {
            v.push("--fork-session".to_string());
        }
        v
    }
    fn worktree_args(&self, name: Option<&str>) -> Option<Vec<String>> {
        let mut v = vec!["--worktree".to_string()];
        // A `#`-prefixed PR number is a legitimate name (`--worktree
        // "#1234"` branches from that PR), so nothing here inspects it.
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            v.push(name.to_string());
        }
        Some(v)
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            fork: true,
            worktrees: true,
            approval_gate: true,
            context_tokens: true,
        }
    }
    fn watch_paths(&self) -> Vec<PathBuf> {
        claude::watch_paths()
    }
    fn read_session_index(&self, cache: &mut SessionIndexCache) -> SessionIndex {
        claude::read_session_index(cache)
    }
    fn read_transcript_stats(
        &self,
        transcript: &Path,
        _prior: Option<&TranscriptStats>,
    ) -> TranscriptStats {
        claude::read_transcript_stats_incremental(transcript, None).stats
    }
    fn list_resumable_cached(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        claude::list_resumable_cached(limit, cache)
    }
    fn build_launch_command(
        &self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
        shim_dir: Option<&Path>,
    ) -> Result<Command> {
        claude::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
    }
    fn hooks_settings_json(&self, sock_path: &str) -> String {
        claude::build_hooks_settings(sock_path)
    }
    fn hook_session(&self) -> Box<dyn crate::agents::hook_session::HookSession> {
        Box::new(crate::agents::hook_session::Stateless(
            claude::dispatch_hook,
        ))
    }
    fn parse_hook_payload(&self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        claude::parse_hook_payload(event, stdin)
    }
    fn scan_transcript_signals(&self, path: &Path, offset: u64) -> TranscriptScan {
        claude::scan_transcript_signals(path, offset)
    }
    fn agent_activity(&self, agent_pid: u32) -> Option<AgentActivity> {
        claude::session_activity(agent_pid)
    }
    fn session_name(&self, agent_pid: u32) -> Option<String> {
        claude::read_session_name(agent_pid)
    }
    fn session_watch_path(&self, agent_pid: u32) -> Option<PathBuf> {
        claude::session_file_path(agent_pid)
    }
    fn transcript_write_clears_approval(&self) -> bool {
        true
    }
    fn bg_shells(&self, agent_pid: u32) -> Option<Vec<BgShell>> {
        claude::bg_shells(agent_pid)
    }
}
