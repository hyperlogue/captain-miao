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
        let compact = content_width < 60;
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
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .title(Span::styled(
                " Confirm push ",
                Style::default().fg(ui.attention_fg).bold(),
            ));
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
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    single_line(
                        &format!("Push {count} {noun} to {}?", push.plan.destination()),
                        content_width,
                    ),
                    Style::default().bold(),
                )),
                Line::from(single_line(
                    &format!("{}: {}", push.host.0, push.cwd),
                    content_width,
                )),
            ]),
            summary,
        );
        let author_width = (content_width / 5).clamp(10, 20);
        let message_width = content_width.saturating_sub(25 + author_width + 4);
        let headings = if compact {
            "Commit time / author / message".into()
        } else {
            format!(
                "{:<25}  {}  Message",
                "Commit time",
                column("Author", author_width)
            )
        };
        frame.render_widget(
            Paragraph::new(headings).style(Style::default().bold()),
            header,
        );
        let visible = (rows.height as usize / row_height).max(1);
        self.vcs_scroll = self.vcs_scroll.min(count.saturating_sub(visible));
        let end = (self.vcs_scroll + visible).min(count);
        let mut lines = Vec::new();
        for commit in &commits[self.vcs_scroll..end] {
            if compact {
                lines.push(Line::from(single_line(&commit.time, content_width)));
                lines.push(Line::from(Span::styled(
                    single_line(&commit.author, content_width),
                    Style::default().bold(),
                )));
                lines.push(Line::from(single_line(&commit.subject, content_width)));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(column(&commit.time, 25)),
                    Span::raw("  "),
                    Span::styled(
                        column(&commit.author, author_width),
                        Style::default().bold(),
                    ),
                    Span::raw("  "),
                    Span::raw(single_line(&commit.subject, message_width)),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines), rows);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(single_line(
                    &format!(
                        "{}–{end} of {count} · ↑↓/PgUp/PgDn scroll",
                        self.vcs_scroll + 1
                    ),
                    content_width,
                )),
                Line::from(vec![
                    Span::styled("y/Y", Style::default().bold()),
                    Span::raw(" push   "),
                    Span::styled("Enter/Esc", Style::default().bold()),
                    Span::raw(" cancel"),
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
