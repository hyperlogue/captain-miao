//! Immutable agent behavior, selected once by identity. Optional operations
//! have inert defaults; each agent owns its overrides beside its implementation.

use crate::agent::*;
use crate::state::{HookEvent, HookMessage, LauncherState, ReattachPrime};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

pub(crate) struct TitleSource {
    pub stamp: fn() -> (Option<std::time::SystemTime>, Option<std::time::SystemTime>),
    pub read: fn(&[String]) -> std::collections::HashMap<String, String>,
    pub accepts: fn(&LauncherState) -> bool,
}

pub(crate) trait AgentDefinition: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn title_source(&self) -> Option<TitleSource> {
        None
    }
    fn transcript_reader(&self) -> Box<dyn super::transcript::TranscriptReader> {
        Box::new(super::transcript::BasicReader::new(
            self.kind().definition(),
        ))
    }

    fn stop_session(
        &self,
        state: &LauncherState,
        _policy: crate::backend::CleanupPolicy,
    ) -> Result<crate::backend::SessionRemoval> {
        crate::backend::signal_launcher(state.launcher_pid)
    }

    fn configure(
        &self,
        config: &crate::config::AgentConfig,
    ) -> Result<Box<dyn crate::agent::session::AgentAdapter>> {
        if self.kind() == AgentKind::Unknown {
            return crate::agent::session::refuse_unknown();
        }
        Ok(Box::new(crate::agent::session::NativeAdapter {
            definition: self.kind().definition(),
            approval_grace: config.approval_grace,
        }))
    }

    fn cli_subcommand(&self) -> &'static str {
        ""
    }
    fn clipboard_paste_encoder(&self) -> Option<fn(&Path) -> Vec<u8>> {
        None
    }
    fn bin(&self) -> Option<&'static str> {
        None
    }
    fn label(&self) -> &'static str {
        "Agent"
    }
    fn read_resume_metadata(&self, _args: &[String]) -> Option<ResumeMetadata> {
        None
    }
    fn resume_args(&self, _session_id: &str, _fork: bool) -> Vec<String> {
        vec![]
    }
    fn worktree_args(&self, _name: Option<&str>) -> Option<Vec<String>> {
        None
    }
    fn uses_alt_screen(&self, _agent_args: &[String]) -> bool {
        false
    }
    fn uses_kitty_keyboard(&self, _term: &str, _alt_screen: bool) -> bool {
        false
    }
    fn reattach_prime(&self, alt_screen: bool, kitty_keyboard: bool) -> ReattachPrime {
        ReattachPrime {
            alt_screen,
            input_modes: alt_screen,
            keyboard_flags: if kitty_keyboard { 3 } else { 0 },
            ..ReattachPrime::default()
        }
    }
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            fork: false,
            worktrees: false,
            approval_gate: false,
            context_tokens: false,
        }
    }
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![]
    }
    fn out_of_band_watch_paths(&self) -> Vec<PathBuf> {
        vec![]
    }
    fn read_session_index(&self, _cache: &mut SessionIndexCache) -> SessionIndex {
        SessionIndex::default()
    }
    fn read_transcript_stats(
        &self,
        _transcript: &Path,
        _prior: Option<&TranscriptStats>,
    ) -> TranscriptStats {
        TranscriptStats::default()
    }
    fn list_resumable_cached(
        &self,
        _limit: usize,
        _cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        Ok(vec![])
    }
    fn build_launch_command(
        &self,
        _cwd: &str,
        _sock_path: &Path,
        _settings_path: &Path,
        _extra_args: &[String],
        _shim_dir: Option<&Path>,
    ) -> Result<Command> {
        Err(anyhow::anyhow!(UNKNOWN_AGENT_REFUSAL))
    }
    fn hooks_settings_json(&self, _sock_path: &str) -> String {
        String::new()
    }
    fn hook_session(&self) -> Box<dyn crate::agents::hook_session::HookSession> {
        Box::new(crate::agents::hook_session::Stateless(|_, _| {}))
    }
    fn last_prompt_is_live(&self) -> bool {
        false
    }

    fn parse_hook_payload(&self, _event: HookEvent, _stdin: &str) -> Result<HookMessage> {
        Err(anyhow::anyhow!(
            "unknown agent backend (this hook came from a newer \
                 captain-miao); upgrade captain-miao to handle it"
        ))
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
    fn session_name(&self, _agent_pid: u32) -> Option<String> {
        None
    }
    fn session_watch_path(&self, _agent_pid: u32) -> Option<PathBuf> {
        None
    }
    fn transcript_write_clears_approval(&self) -> bool {
        false
    }
    fn refresh_transcript_on_hook(&self) -> bool {
        false
    }
    fn transcript_poll_interval(&self) -> Option<Duration> {
        None
    }
    fn transcript_path_is_replaced(&self) -> bool {
        false
    }
    fn replaced_sidecar_paths(&self, _transcript: &Path) -> Vec<PathBuf> {
        vec![]
    }
    fn bg_shells(&self, _agent_pid: u32) -> Option<Vec<BgShell>> {
        None
    }
}

struct Unknown;
impl AgentDefinition for Unknown {
    fn kind(&self) -> AgentKind {
        AgentKind::Unknown
    }
}

pub(crate) static UNKNOWN: &dyn AgentDefinition = &Unknown;
