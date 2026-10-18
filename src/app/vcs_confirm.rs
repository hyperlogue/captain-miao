//! Caller-supplied commit review content and prepared-push lifecycle. The shared
//! confirmation owns the overlay, scrolling and accept/reject interaction.

use ratatui::{
    style::Style,
    text::{Line, Span, Text},
};
use unicode_width::UnicodeWidthStr;

use super::confirm::{Body, Confirmation, Content};
use super::format::truncate_str;
use super::notifications::Level;
use super::{Action, App, ConfirmAction, InputMode};
use crate::config;
use crate::state::HostId;

#[derive(Debug)]
pub(crate) struct PushConfirmation {
    pub host: HostId,
    pub cwd: String,
    pub plan: Box<cm_core::vcs::PreparedVcsPush>,
}

impl App {
    pub(super) fn show_vcs_confirmation(&mut self) -> bool {
        if self.input_mode != InputMode::Normal
            || self.upgrade_notices.is_some()
            || self.confirmation.is_some()
        {
            return false;
        }
        let Some(push) = self.vcs_pushes.pop_front() else {
            return false;
        };
        let review = PushReview {
            destination: push.plan.destination(),
            checkout: format!("{}: {}", push.host.0, push.cwd),
            commits: push.plan.outgoing_commits().to_vec(),
        };
        self.confirmation = Some(Confirmation::question(
            "Confirm push",
            review,
            "push",
            ConfirmAction::Run(Action::VcsRun(push)),
        ));
        true
    }

    pub(super) fn cancel_vcs_push(&mut self, push: PushConfirmation) {
        if let Some(id) = self
            .vcs_commands
            .remove(&(push.host.clone(), push.cwd.clone()))
        {
            self.update_notification(
                id,
                Level::Info,
                format!("Push cancelled\n{}: {}", push.host.0, push.cwd),
            );
        }
    }
}

#[derive(Debug)]
struct PushReview {
    destination: String,
    checkout: String,
    commits: Vec<cm_core::vcs::VcsCommit>,
}

impl Content for PushReview {
    fn layout(&self, width: u16) -> Body {
        let content_width = usize::from(width);
        let times: Vec<_> = self
            .commits
            .iter()
            .map(|commit| commit_time(&commit.time))
            .collect();
        let time_width = times
            .iter()
            .map(|time| single_line(time, usize::MAX).width())
            .max()
            .unwrap_or(0)
            .max("Commit time".len());
        // Measure every row so scrolling never moves the columns.
        let author_width = self
            .commits
            .iter()
            .map(|commit| single_line(&commit.author, usize::MAX).width())
            .max()
            .unwrap_or(0)
            .max("Author".len());
        let message_width = content_width.saturating_sub(time_width + author_width + 4);
        let compact = message_width < 20;
        let ui = &config::get().colors.ui;
        let muted = Style::default().dim();
        let accent = Style::default().fg(ui.title_fg).bold();
        let attention = Style::default().fg(ui.attention_fg).bold();
        let message = Style::default().bold();
        let count = self.commits.len();
        let noun = if count == 1 { "commit" } else { "commits" };
        let prompt = format!("Push {count} {noun} to ");
        let destination = single_line(
            &self.destination,
            content_width.saturating_sub(prompt.width() + 1),
        );
        let headings = if compact {
            "Commit time / author / message".into()
        } else {
            format!(
                "{}  {}  Message",
                column("Commit time", time_width),
                column("Author", author_width)
            )
        };
        let pinned = Text::from(vec![
            Line::from(vec![
                Span::raw("Push "),
                Span::styled(format!("{count} {noun}"), attention),
                Span::styled(" to ", muted),
                Span::styled(destination, accent),
                Span::raw("?"),
            ]),
            Line::styled(single_line(&self.checkout, content_width), muted),
            Line::default(),
            Line::styled(single_line(&headings, content_width), muted),
        ]);
        let mut lines = Vec::new();
        for (commit, time) in self.commits.iter().zip(times) {
            if compact {
                lines.push(Line::styled(single_line(time, content_width), muted));
                lines.push(Line::from(single_line(&commit.author, content_width)));
                lines.push(Line::styled(
                    single_line(&commit.subject, content_width),
                    message,
                ));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(column(time, time_width), muted),
                    Span::raw("  "),
                    Span::raw(column(&commit.author, author_width)),
                    Span::raw("  "),
                    Span::styled(single_line(&commit.subject, message_width), message),
                ]));
            }
        }
        Body {
            pinned,
            text: Text::from(lines),
        }
    }
}

fn single_line(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clean: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    truncate_str(&clean, width)
}

fn column(value: &str, width: usize) -> String {
    let text = single_line(value, width);
    let padding = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(padding))
}

/// Git records an offset rather than a timezone name. Compare it with the
/// dashboard's offset at that instant, including historical daylight saving.
fn commit_time(value: &str) -> &str {
    commit_time_with_offset(value, |seconds| {
        let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: the pointers are valid, and localtime_r initializes local on
        // success without using the shared buffer of localtime.
        unsafe {
            if libc::localtime_r(&seconds, local.as_mut_ptr()).is_null() {
                None
            } else {
                Some(local.assume_init().tm_gmtoff)
            }
        }
    })
}

fn commit_time_with_offset(
    value: &str,
    local_offset: impl FnOnce(libc::time_t) -> Option<i64>,
) -> &str {
    let Some((seconds, offset)) = parse_commit_time(value) else {
        return value;
    };
    if local_offset(seconds) == Some(offset) {
        &value[..19]
    } else {
        value
    }
}

/// Parse Git's fixed `%ci` format without changing the process timezone. Keep
/// unexpected or invalid dates verbatim instead of hiding their trailing text.
fn parse_commit_time(value: &str) -> Option<(libc::time_t, i64)> {
    if value.len() != 25 || !value.is_ascii() {
        return None;
    }
    let bytes = value.as_bytes();
    for (i, separator) in [
        (4, b'-'),
        (7, b'-'),
        (10, b' '),
        (13, b':'),
        (16, b':'),
        (19, b' '),
    ] {
        if bytes[i] != separator {
            return None;
        }
    }
    let number = |range: std::ops::Range<usize>| -> Option<i32> {
        let text = &value[range];
        text.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| text.parse().ok())
            .flatten()
    };
    let parts = [
        number(0..4)? - 1900,
        number(5..7)? - 1,
        number(8..10)?,
        number(11..13)?,
        number(14..16)?,
        number(17..19)?,
    ];
    let hours = number(21..23)?;
    let minutes = number(23..25)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let sign = match bytes[20] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let offset = i64::from(sign * (hours * 3600 + minutes * 60));
    // SAFETY: zero initializes valid integer and null pointer fields of tm.
    let mut utc: libc::tm = unsafe { std::mem::zeroed() };
    [
        utc.tm_year,
        utc.tm_mon,
        utc.tm_mday,
        utc.tm_hour,
        utc.tm_min,
        utc.tm_sec,
    ] = parts;
    // SAFETY: timegm accepts a valid, writable tm and interprets it as UTC;
    // it does not mutate the process timezone, unlike changing TZ for mktime.
    let seconds = unsafe { libc::timegm(&mut utc) };
    if [
        utc.tm_year,
        utc.tm_mon,
        utc.tm_mday,
        utc.tm_hour,
        utc.tm_min,
        utc.tm_sec,
    ] != parts
    {
        return None; // timegm normalized an invalid calendar date or time.
    }
    Some((seconds.checked_sub(offset)?, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_time_omits_only_the_local_offset() {
        for (value, offset) in [
            ("1970-01-01 00:00:00 +0000", 0),
            ("1970-01-01 05:45:00 +0545", 5 * 3600 + 45 * 60),
            ("1969-12-31 20:00:00 -0400", -4 * 3600),
        ] {
            assert_eq!(
                commit_time_with_offset(value, |seconds| {
                    assert_eq!(seconds, 0);
                    Some(offset)
                }),
                &value[..19]
            );
            assert_eq!(
                commit_time_with_offset(value, |_| Some(offset + 3600)),
                value
            );
            assert_eq!(commit_time_with_offset(value, |_| None), value);
        }
        let before_epoch = "1969-12-31 23:59:59 +0000";
        assert_eq!(
            commit_time_with_offset(before_epoch, |seconds| {
                assert_eq!(seconds, -1);
                Some(0)
            }),
            "1969-12-31 23:59:59"
        );
    }

    #[test]
    fn commit_time_uses_the_offset_at_the_commit_instant() {
        // The 2025 US spring transition is at 07:00 UTC. Inject the offset
        // lookup rather than changing TZ in the multithreaded test process.
        let local_offset = |seconds| {
            Some(if seconds < 1_741_503_600 {
                -5 * 3600
            } else {
                -4 * 3600
            })
        };
        for value in ["2025-03-09 01:59:59 -0500", "2025-03-09 03:00:00 -0400"] {
            assert_eq!(commit_time_with_offset(value, local_offset), &value[..19]);
        }
        let foreign = "2025-03-09 03:00:00 -0500";
        assert_eq!(commit_time_with_offset(foreign, local_offset), foreign);
    }

    #[test]
    fn commit_time_preserves_unrecognized_or_invalid_dates() {
        for value in [
            "",
            "unknown",
            "2025-03-05 14:15:16",
            "2025-02-30 14:15:16 +0000",
            "2025-13-01 14:15:16 +0000",
            "2025-03-05 25:00:00 +0000",
            "2025-03-05 14:15:16 +0060",
            "2025-03-05 14:15:16 +2400",
            "2025-03-05 14:15:16 Z0000",
            "2025-03-05 14:15:16 +00:00",
            "2025-03-05 14:15:16 日本語",
        ] {
            assert_eq!(
                commit_time_with_offset(value, |_| panic!("invalid time reached timezone lookup")),
                value
            );
        }
    }
}
