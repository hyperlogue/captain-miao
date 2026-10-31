//! Presentation policy for background work. Launchers report Task and command
//! identities; explicit user rules override built-in heuristics here, across
//! hosts, without editing launcher files. Unknown commands stay Task.

mod heuristics;

use serde::{Deserialize, Serialize};

use crate::state::{LauncherState, SessionStatus};

use super::{App, Cursor};

/// Last matching rule wins. A session toggle appends exact rules so it can
/// override a wildcard without changing other commands matched by that rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TaskRule {
    pub pattern: String,
    pub idle: bool,
}

impl TaskRule {
    pub(super) fn new(pattern: impl Into<String>, idle: bool) -> Self {
        Self {
            pattern: pattern.into(),
            idle,
        }
    }
}

/// Read the original Idle-only string list as explicit Idle rules. Keep this
/// compatibility at the storage boundary, outside the classification policy.
pub(super) fn deserialize_rules<'de, D>(deserializer: D) -> Result<Vec<TaskRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredRule {
        Rule(TaskRule),
        Legacy(String),
    }
    Ok(Vec::<StoredRule>::deserialize(deserializer)?
        .into_iter()
        .map(|rule| match rule {
            StoredRule::Rule(rule) => rule,
            StoredRule::Legacy(pattern) => TaskRule::new(pattern, true),
        })
        .collect())
}

fn command_is_idle(command: &str, rules: &[TaskRule]) -> bool {
    rules
        .iter()
        .rev()
        .find(|rule| matches(&rule.pattern, command))
        .map_or_else(|| heuristics::is_idle(command), |rule| rule.idle)
}

/// Only `*` is special. Backslash quotes the next character, so toggling a
/// shell command containing globs records that exact command, not a pattern.
pub(super) fn matches(pattern: &str, command: &str) -> bool {
    let mut chars = pattern.chars();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '*' => None,
            '\\' => Some(chars.next().unwrap_or('\\')),
            c => Some(c),
        });
    }
    let command: Vec<_> = command.chars().collect();
    let (mut p, mut c, mut star) = (0, 0, None);
    while c < command.len() {
        if tokens.get(p) == Some(&None) {
            star = Some((p, c));
            p += 1;
        } else if tokens.get(p) == Some(&Some(command[c])) {
            p += 1;
            c += 1;
        } else if let Some((s, consumed)) = star {
            c = consumed + 1;
            star = Some((s, c));
            p = s + 1;
        } else {
            return false;
        }
    }
    tokens[p..].iter().all(Option::is_none)
}

fn literal_pattern(command: &str) -> String {
    command.replace('\\', "\\\\").replace('*', "\\*")
}

pub(super) fn apply_idle_commands(session: &mut LauncherState, rules: &[TaskRule]) {
    // An empty/unsupported command list cannot establish that *all* work is
    // idle. Foreground turns and approval prompts always keep their status.
    if session.background_commands.is_empty()
        || !matches!(
            session.status,
            SessionStatus::BackgroundActive | SessionStatus::Idle
        )
    {
        return;
    }
    session.status = if session
        .background_commands
        .iter()
        .all(|command| command_is_idle(command, rules))
    {
        SessionStatus::Idle
    } else {
        SessionStatus::BackgroundActive
    };
}

impl App {
    pub(super) fn toggle_task_idle(&mut self) {
        let Some(session) = self.selected_session_ref() else {
            return;
        };
        if session.background_commands.is_empty()
            || !matches!(
                session.status,
                SessionStatus::BackgroundActive | SessionStatus::Idle
            )
        {
            self.set_status("No reported background commands to toggle".into(), true);
            return;
        }
        let commands = session.background_commands.clone();
        let was_idle = session.status == SessionStatus::Idle;
        for command in &commands {
            let pattern = literal_pattern(command);
            // Keep unrelated wildcard preferences intact. Move this exact
            // override to the end so the user's latest choice wins.
            self.extra_prefs
                .task_rules
                .retain(|rule| rule.pattern != pattern);
            self.extra_prefs
                .task_rules
                .push(TaskRule::new(pattern, !was_idle));
        }
        self.save_overrides();
        self.refresh_idle_tasks();
        self.set_status(
            if was_idle {
                "Background commands count as Task; edit overrides in Preferences → Idle tasks"
            } else {
                "Background commands count as Idle; edit them in Preferences → Idle tasks"
            }
            .into(),
            false,
        );
    }

    pub(super) fn refresh_idle_tasks(&mut self) {
        for session in &mut self.sessions {
            apply_idle_commands(session, &self.extra_prefs.task_rules);
        }
        self.mark_dirty(Cursor::FollowSession);
        self.update_sleep_inhibitor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;

    #[test]
    fn patterns_are_anchored_and_literal_commands_round_trip() {
        for command in [
            "carver view --port 8011",
            "printf '%s' *.rs",
            r"echo \*",
            "echo 日本語",
        ] {
            assert!(matches(&literal_pattern(command), command));
            assert!(!matches(
                &literal_pattern(command),
                &format!("{command} extra")
            ));
        }
        assert!(matches("*carver view *", "uv run carver view --port 8011"));
        assert!(matches("r3 watch review_*", "r3 watch review_abc"));
        assert!(matches("a*b*c", "abc"));
        assert!(matches("*", ""));
        assert!(!matches("*carver view *", "cargo test"));
        assert!(!matches("a*b*c", "abx"));
    }

    #[test]
    fn every_command_must_match_and_foreground_work_is_never_hidden() {
        let mut session =
            LauncherState::for_test(AgentControl::Claude, SessionStatus::BackgroundActive);
        session.background_commands = vec!["carver view --port 8011".into(), "cargo test".into()];
        let patterns = vec![TaskRule::new("carver view *", true)];
        apply_idle_commands(&mut session, &patterns);
        assert_eq!(session.status, SessionStatus::BackgroundActive);
        session.background_commands.pop();
        apply_idle_commands(&mut session, &patterns);
        assert_eq!(session.status, SessionStatus::Idle);
        apply_idle_commands(&mut session, &[]);
        assert_eq!(session.status, SessionStatus::BackgroundActive);
        for status in [
            SessionStatus::Active,
            SessionStatus::WaitingForApproval,
            SessionStatus::WaitingForDecision,
        ] {
            session.status = status.clone();
            apply_idle_commands(&mut session, &patterns);
            assert_eq!(session.status, status);
        }
        session.status = SessionStatus::BackgroundActive;
        session.background_commands.clear();
        apply_idle_commands(&mut session, &[TaskRule::new("*", true)]);
        assert_eq!(session.status, SessionStatus::BackgroundActive);
    }

    #[test]
    fn explicit_choices_override_defaults_and_later_rules_win() {
        assert!(command_is_idle("npm run dev", &[]));
        assert!(!command_is_idle("cargo test", &[]));
        let mut rules = vec![
            TaskRule::new("npm *", false),
            TaskRule::new("cargo test", true),
        ];
        assert!(!command_is_idle("npm run dev", &rules));
        assert!(command_is_idle("cargo test", &rules));
        rules.push(TaskRule::new("npm run dev", true));
        assert!(command_is_idle("npm run dev", &rules));
        assert!(!command_is_idle("npm run watch", &rules));
        rules.clear();
        assert!(command_is_idle("npm run dev", &rules));
        assert!(!command_is_idle("cargo test", &rules));
    }

    #[test]
    fn automatic_idle_requires_every_command_and_preserves_foreground_states() {
        let mut session =
            LauncherState::for_test(AgentControl::Claude, SessionStatus::BackgroundActive);
        session.background_commands = vec!["npm run dev".into(), "r3 watch review_abc".into()];
        apply_idle_commands(&mut session, &[]);
        assert_eq!(session.status, SessionStatus::Idle);
        session.background_commands.push("cargo test".into());
        apply_idle_commands(&mut session, &[]);
        assert_eq!(session.status, SessionStatus::BackgroundActive);
        session.background_commands.pop();
        for status in [
            SessionStatus::Active,
            SessionStatus::Compacting,
            SessionStatus::WaitingForApproval,
            SessionStatus::WaitingForDecision,
        ] {
            session.status = status.clone();
            apply_idle_commands(&mut session, &[]);
            assert_eq!(session.status, status);
        }
    }
}
