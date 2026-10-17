//! Review prepared pushes without interrupting another modal or changing targets.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::format::{clear_overlay, truncate_str};
use super::notifications::Level;
use super::{Action, App, InputMode};
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
            || self.pending_confirm.is_some()
            || self.vcs_pushes.is_empty()
        {
            return false;
        }
        self.vcs_scroll = 0;
        self.input_mode = InputMode::VcsConfirm;
        true
    }

    pub(super) fn scroll_vcs_confirm(&mut self, delta: isize) {
        let count = self
            .vcs_pushes
            .front()
            .map_or(0, |push| push.plan.outgoing_commits().len());
        self.vcs_scroll = self
            .vcs_scroll
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
    }

    pub(super) fn handle_vcs_confirm_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_vcs_confirm(1);
                    return None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_vcs_confirm(-1);
                    return None;
                }
                KeyCode::PageDown => {
                    self.scroll_vcs_confirm(10);
                    return None;
                }
                KeyCode::PageUp => {
                    self.scroll_vcs_confirm(-10);
                    return None;
                }
                KeyCode::Home => {
                    self.vcs_scroll = 0;
                    return None;
                }
                KeyCode::End => {
                    self.scroll_vcs_confirm(isize::MAX);
                    return None;
                }
                _ => {}
            }
        }
        self.input_mode = InputMode::Normal;
        let push = self.vcs_pushes.pop_front()?;
        if matches!(key.code, KeyCode::Char('y' | 'Y'))
            && (key.modifiers - KeyModifiers::SHIFT).is_empty()
        {
            return Some(Action::VcsRun(push));
        }
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
        None
    }

    pub(super) fn draw_vcs_confirm(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(push) = self.vcs_pushes.front() else {
            return;
        };
        let commits = push.plan.outgoing_commits();
        let width = area.width.saturating_sub(2).min(106);
        let content_width = width.saturating_sub(4) as usize;
        let times: Vec<_> = commits
            .iter()
            .map(|commit| commit_time(&commit.time))
            .collect();
        let time_width = times
            .iter()
            .map(|time| single_line(time, usize::MAX).width())
            .max()
            .unwrap_or(0)
            .max("Commit time".len());
        // Measure the whole table so scrolling does not move the columns. Give
        // names their natural display width, including wide Unicode glyphs.
        let author_width = commits
            .iter()
            .map(|commit| single_line(&commit.author, usize::MAX).width())
            .max()
            .unwrap_or(0)
            .max("Author".len());
        let message_width = content_width.saturating_sub(time_width + author_width + 4);
        let compact = message_width < 20;
        let row_height = if compact { 3 } else { 1 };
        let height = (commits.len().saturating_mul(row_height).saturating_add(8))
            .min(24)
            .min(area.height as usize) as u16;
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let ui = &config::get().colors.ui;
        let muted = Style::default().dim();
        let accent = Style::default().fg(ui.title_fg).bold();
        let attention = Style::default().fg(ui.attention_fg).bold();
        let message = Style::default().bold();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(muted)
            .padding(Padding::horizontal(1))
            .title(Span::styled(" Confirm push ", attention));
        let inner = block.inner(popup);
        clear_overlay(frame, popup);
        frame.render_widget(block, popup);
        let [summary, _, header, rows, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .areas(inner);
        let count = commits.len();
        let noun = if count == 1 { "commit" } else { "commits" };
        let prompt = format!("Push {count} {noun} to ");
        let destination = single_line(
            &push.plan.destination(),
            content_width.saturating_sub(prompt.width() + 1),
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::raw("Push "),
                    Span::styled(format!("{count} {noun}"), attention),
                    Span::styled(" to ", muted),
                    Span::styled(destination, accent),
                    Span::raw("?"),
                ]),
                Line::styled(
                    single_line(&format!("{}: {}", push.host.0, push.cwd), content_width),
                    muted,
                ),
            ]),
            summary,
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
        frame.render_widget(Paragraph::new(headings).style(muted), header);
        let visible = (rows.height as usize / row_height).max(1);
        self.vcs_scroll = self.vcs_scroll.min(count.saturating_sub(visible));
        let end = (self.vcs_scroll + visible).min(count);
        let mut lines = Vec::new();
        for (commit, time) in commits[self.vcs_scroll..end]
            .iter()
            .zip(&times[self.vcs_scroll..end])
        {
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
        frame.render_widget(Paragraph::new(lines), rows);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    single_line(
                        &format!(
                            "{}–{end} of {count} · ↑↓/PgUp/PgDn scroll",
                            self.vcs_scroll + 1
                        ),
                        content_width,
                    ),
                    muted,
                ),
                Line::from(vec![
                    Span::styled("y/Y push", attention),
                    Span::raw("   "),
                    Span::styled("Enter/Esc cancel", muted),
                ]),
            ]),
            footer,
        );
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
