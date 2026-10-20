//! User-owned presentation policy for background work. Launchers report Task
//! and command identities; the dashboard alone decides which commands count
//! as Idle. Preferences apply across hosts without editing launcher files.

use crate::state::{LauncherState, SessionStatus};

use super::{App, Cursor};

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

pub(super) fn apply_idle_commands(session: &mut LauncherState, patterns: &[String]) {
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
        .all(|command| patterns.iter().any(|pattern| matches(pattern, command)))
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
        if was_idle {
            // Remove every matching rule, including edited patterns. Keeping
            // one would make the reverse toggle appear to do nothing.
            self.extra_prefs
                .idle_commands
                .retain(|pattern| !commands.iter().any(|command| matches(pattern, command)));
        } else {
            for command in &commands {
                if !self
                    .extra_prefs
                    .idle_commands
                    .iter()
                    .any(|p| matches(p, command))
                {
                    self.extra_prefs
                        .idle_commands
                        .push(literal_pattern(command));
                }
            }
        }
        self.save_overrides();
        self.refresh_idle_tasks();
        self.set_status(
            if was_idle {
                "Background commands count as Task; matching Idle rules removed"
            } else {
                "Background commands count as Idle; edit them in Preferences → Idle tasks"
            }
            .into(),
            false,
        );
    }

    pub(super) fn refresh_idle_tasks(&mut self) {
        for session in &mut self.sessions {
            apply_idle_commands(session, &self.extra_prefs.idle_commands);
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
        let patterns = vec!["carver view *".into()];
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
        apply_idle_commands(&mut session, &["*".into()]);
        assert_eq!(session.status, SessionStatus::BackgroundActive);
    }
}
