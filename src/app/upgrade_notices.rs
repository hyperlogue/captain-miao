//! Shared, scrollable presentation of versioned breaking-change notices.
//!
//! This overlay sits above the current input mode so startup crash recovery
//! remains available after all notices close. The full queue must be acknowledged
//! before advancing the saved dashboard version; quitting partway replays it.
//! Dashboard state owns version tracking and fresh-install detection.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
};

use super::{
    Action, App,
    breaking_changes::{BreakingChange, Content},
    format::clear_overlay,
};
use crate::config;

pub(super) struct UpgradeNotices {
    changes: Vec<&'static BreakingChange>,
    position: usize,
    button: Rect,
    copy_button: Rect,
    pub(super) copy_feedback: Option<String>,
    scroll: u16,
}

impl UpgradeNotices {
    pub(super) fn new(changes: Vec<&'static BreakingChange>) -> Option<Self> {
        (!changes.is_empty()).then_some(Self {
            changes,
            position: 0,
            button: Rect::default(),
            copy_button: Rect::default(),
            copy_feedback: None,
            scroll: 0,
        })
    }

    fn copy_snippet(&self) -> Option<Action> {
        let snippets: Vec<_> = self.changes[self.position]
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Code(text) => Some(*text),
                _ => None,
            })
            .collect();
        (!snippets.is_empty()).then(|| Action::CopyUpgradeSnippet(snippets.join("\n\n")))
    }
}

impl App {
    fn acknowledge_upgrade_notices(&mut self) {
        let Some(notices) = self.upgrade_notices.as_mut() else {
            return;
        };
        notices.position += 1;
        if notices.position < notices.changes.len() {
            notices.scroll = 0;
            // Do not let a second click on the old page acknowledge the next
            // one before it has been rendered.
            notices.button = Rect::default();
            notices.copy_button = Rect::default();
            notices.copy_feedback = None;
            return;
        }
        self.upgrade_notices = None;
        if let Err(error) = self.dashboard_state.finish_startup() {
            self.set_status(
                format!("Could not save the dashboard version; upgrade notices may appear again: {error}"),
                true,
            );
        }
    }

    pub(super) fn handle_upgrade_notices_key(&mut self, key: KeyEvent) -> Option<Action> {
        let notice = self.upgrade_notices.as_mut()?;
        match key.code {
            KeyCode::Enter | KeyCode::Esc if key.kind == KeyEventKind::Press => {
                self.acknowledge_upgrade_notices();
            }
            KeyCode::Char('c') if key.kind == KeyEventKind::Press => return notice.copy_snippet(),
            KeyCode::Down | KeyCode::Char('j') => notice.scroll = notice.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => notice.scroll = notice.scroll.saturating_sub(1),
            _ => {}
        }
        None
    }

    pub(super) fn handle_upgrade_notices_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        let notice = self.upgrade_notices.as_mut()?;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if notice
                    .copy_button
                    .contains((mouse.column, mouse.row).into()) =>
            {
                return notice.copy_snippet();
            }
            MouseEventKind::Down(MouseButton::Left)
                if notice.button.contains((mouse.column, mouse.row).into()) =>
            {
                self.acknowledge_upgrade_notices();
            }
            MouseEventKind::ScrollDown => notice.scroll = notice.scroll.saturating_add(3),
            MouseEventKind::ScrollUp => notice.scroll = notice.scroll.saturating_sub(3),
            _ => {}
        }
        None
    }

    pub(super) fn draw_upgrade_notices(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(notice) = self.upgrade_notices.as_mut() else {
            return;
        };
        let ui = &config::get().colors.ui;
        let accent = Style::default().fg(ui.title_fg).bold();
        let change = notice.changes[notice.position];
        let mut lines = Vec::new();
        for content in change.content {
            match content {
                Content::Text(text) | Content::Code(text) => {
                    lines.extend(text.split('\n').map(Line::from));
                }
                Content::Heading(text) => lines.push(Line::styled(*text, Style::default().dim())),
                Content::Binding(command, description) => lines.push(Line::from(vec![
                    Span::styled(
                        self.keymap
                            .keys_for(*command)
                            .unwrap_or_else(|| "Unbound".into()),
                        accent,
                    ),
                    Span::raw(format!("  {description}")),
                ])),
            }
        }
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let width = area.width.saturating_sub(2).min(66);
        let content_width = width.saturating_sub(4);
        let content_lines = paragraph.line_count(content_width);
        let label = if notice.position + 1 < notice.changes.len() {
            " Enter: Next "
        } else {
            " Enter: Got it "
        };
        let copy_label = " c: Copy snippet ";
        let has_snippet = change.content.iter().any(|c| matches!(c, Content::Code(_)));
        let buttons_width = (label.len() + copy_label.len() + 2) as u16;
        let stack_buttons = has_snippet && buttons_width > content_width;
        let button_rows = 1 + u16::from(stack_buttons);
        let feedback = Paragraph::new(notice.copy_feedback.as_deref().unwrap_or(""))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false });
        let feedback_height = feedback.line_count(content_width).clamp(1, 3) as u16;
        let footer_height = button_rows + feedback_height;
        let height = (content_lines
            .saturating_add(footer_height as usize + 3)
            .min(u16::MAX as usize) as u16)
            .min(area.height.saturating_sub(2));
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .border_style(Style::default().fg(ui.title_fg))
            .title(Span::styled(format!(" {} ", change.title), accent))
            .title_bottom(Line::from(format!(
                " v{} · {}/{} ",
                change.introduced,
                notice.position + 1,
                notice.changes.len(),
            )));
        clear_overlay(frame, popup);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let button_rows = button_rows.min(inner.height);
        let feedback_height = feedback_height.min(inner.height.saturating_sub(button_rows));
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(button_rows + feedback_height),
        );
        notice.scroll = notice
            .scroll
            .min(content_lines.saturating_sub(body.height as usize) as u16);
        frame.render_widget(paragraph.scroll((notice.scroll, 0)), body);
        frame.render_widget(
            feedback,
            Rect::new(inner.x, body.bottom(), inner.width, feedback_height),
        );
        let button_width = (label.len() as u16).min(inner.width);
        notice.button = Rect::new(
            inner.x + (inner.width - button_width) / 2,
            inner.bottom().saturating_sub(1),
            button_width,
            u16::from(inner.height > 0),
        );
        notice.copy_button = Rect::default();
        if has_snippet && (!stack_buttons || button_rows == 2) {
            let copy_width = (copy_label.len() as u16).min(inner.width);
            notice.copy_button = if stack_buttons {
                Rect::new(
                    inner.x + (inner.width - copy_width) / 2,
                    notice.button.y.saturating_sub(1),
                    copy_width,
                    1,
                )
            } else {
                let left = inner.x + (inner.width - buttons_width) / 2;
                notice.button.x = left + copy_width + 2;
                Rect::new(left, notice.button.y, copy_width, notice.button.height)
            };
            frame.render_widget(
                Paragraph::new(copy_label)
                    .alignment(Alignment::Center)
                    .style(accent),
                notice.copy_button,
            );
        }
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(accent.reversed()),
            notice.button,
        );
    }
}
