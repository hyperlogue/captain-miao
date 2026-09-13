//! Explain the evidence behind a session's health without polling its agent.
//! Host connectivity outranks a cached launcher observation. An old timestamp
//! alone is never a failure: idle sessions may have nothing new to report.
use super::keymap::{Command, Keymap};
use crate::backend::ConnState;
use crate::state::{CleanupStatus, LauncherState, SessionStatus};

pub(super) struct Diagnostic {
    pub label: &'static str,
    pub message: String,
    pub attention: bool,
}

pub(super) fn session_diagnostics(
    state: &LauncherState,
    host: ConnState,
    keys: &Keymap,
) -> Vec<Diagnostic> {
    let mut lines = Vec::new();
    let mut add = |label, message: String, attention| {
        lines.push(Diagnostic {
            label,
            message,
            attention,
        });
    };
    if !host.is_connected() {
        let message = match host {
            ConnState::Connecting => "Connecting to host; session state is unverified".into(),
            ConnState::Disconnected => "Host disconnected; session state is unverified".into(),
            ConnState::Failed(reason) => format!("Host connection failed: {reason}"),
            ConnState::Connected => unreachable!(),
        };
        add("Host", message, true);
        if let Some(key) = keys.primary_key(Command::ManageHosts) {
            add("Next", format!("{key}: inspect host connection"), true);
        }
        return lines;
    }
    if !state.codex_mode.is_native() {
        let message = match state.codex_connected {
            Some(true) => "Codex app-server connected",
            Some(false) => "Codex app-server disconnected",
            None if state.session_id.is_none() => "Waiting for Codex to connect",
            None => "Codex connection not reported by launcher",
        };
        add("Link", message.into(), state.codex_connected != Some(true));
    }
    match &state.cleanup {
        Some(CleanupStatus::InProgress) => {
            add(
                "Cleanup",
                "Waiting for the agent to confirm cleanup".into(),
                true,
            );
        }
        Some(CleanupStatus::Failed { message }) => {
            if let Some(key) = keys.primary_key(Command::KillSelected) {
                add(
                    "Next",
                    format!("{key}: retry Kill; session retained after failed cleanup"),
                    true,
                );
            }
            add("Cleanup", message.clone(), true);
        }
        _ if state.codex_connected == Some(false) => {
            // The Codex TUI owns reconnection. Do not promise a retry timer or
            // infer that server-owned execution stopped with the connection.
            let mut next = if state.child_pid.is_some() {
                "Reconnect in the Codex terminal".to_string()
            } else {
                "Codex terminal has exited; launcher retained for cleanup".to_string()
            };
            if state.is_restartable()
                && let Some(key) = keys.primary_key(Command::RestartSelected)
            {
                next.push_str(&format!("; {key}: restart saved conversation"));
            }
            if let Some(key) = keys.primary_key(Command::KillSelected) {
                next.push_str(&format!("; {key}: remove session"));
            }
            add("Next", next, true);
        }
        _ if state.status == SessionStatus::FailedToStart => {
            if let Some(key) = keys.primary_key(Command::KillSelected) {
                add(
                    "Next",
                    format!("Fix the launch error below; {key}: remove failed session"),
                    true,
                );
            }
        }
        _ => {}
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::config::KeyBinding;
    use std::collections::HashMap;

    #[test]
    fn failed_host_outranks_cached_agent_health() {
        let mut row = LauncherState::for_test(AgentControl::Codex, SessionStatus::Idle);
        row.codex_mode = cm_core::agents::codex::CodexMode::AppServer;
        row.codex_connected = Some(true);
        let lines = session_diagnostics(
            &row,
            ConnState::Failed("authentication failed".into()),
            &Keymap::defaults(),
        );
        assert!(lines[0].message.contains("authentication failed"));
        assert!(
            lines
                .iter()
                .all(|line| !line.message.contains("app-server connected"))
        );
        assert!(lines[1].message.contains("inspect host connection"));
    }

    #[test]
    fn cleanup_and_reconnect_hints_follow_remaps_and_unbindings() {
        let mut row = LauncherState::for_test(AgentControl::Codex, SessionStatus::Starting);
        row.codex_mode = cm_core::agents::codex::CodexMode::AppServer;
        row.codex_connected = Some(false);
        row.codex_control = true;
        let (keys, warnings) = Keymap::from_config(&HashMap::from([
            ("kill".into(), KeyBinding::One("X".into())),
            ("restart".into(), KeyBinding::Many(vec![])),
        ]));
        assert!(warnings.is_empty(), "{warnings:?}");
        let lines = session_diagnostics(&row, ConnState::Connected, &keys);
        let next = &lines
            .iter()
            .find(|line| line.label == "Next")
            .unwrap()
            .message;
        assert!(next.contains("X: remove session"));
        assert!(!next.contains("restart saved conversation"));
        row.cleanup = Some(CleanupStatus::Failed {
            message: "cleanup refused".into(),
        });
        let lines = session_diagnostics(&row, ConnState::Connected, &keys);
        assert!(
            lines
                .iter()
                .any(|line| line.message.contains("X: retry Kill"))
        );
        assert!(lines.iter().any(|line| line.message == "cleanup refused"));
    }

    #[test]
    fn quiet_native_session_is_not_reported_as_disconnected() {
        let mut row = LauncherState::for_test(AgentControl::Codex, SessionStatus::Idle);
        row.updated_at = 0;
        assert!(session_diagnostics(&row, ConnState::Connected, &Keymap::defaults()).is_empty());
    }
}
