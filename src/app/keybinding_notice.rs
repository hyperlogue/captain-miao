//! Acknowledged once per state directory, independently of dashboard prefs.
//!
//! This overlay sits above the current input mode so startup crash recovery
//! remains available after the shortcut notice closes. Merely displaying it
//! does not count as acknowledgement: quitting before reading it shows it again.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
};

use super::{App, format::clear_overlay, keymap::Command};
use crate::{config, state};

pub(super) struct KeybindingNotice {
    receipt: PathBuf,
    button: Rect,
    scroll: u16,
}

impl KeybindingNotice {
    pub(super) fn load(receipt: PathBuf) -> Option<Self> {
        (state::read_json::<bool>(&receipt) != Some(true)).then_some(Self {
            receipt,
            button: Rect::default(),
            scroll: 0,
        })
    }

    fn acknowledge(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.receipt.parent() {
            state::create_dir_all_private(parent)?;
        }
        state::write_json_atomic(&self.receipt, &true)
    }
}

impl App {
    fn acknowledge_keybinding_notice(&mut self) {
        if let Some(notice) = self.keybinding_notice.take()
            && let Err(error) = notice.acknowledge()
        {
            self.set_status(
                format!("Could not remember the shortcut notice; it may appear again: {error}"),
                true,
            );
        }
    }

    pub(super) fn handle_keybinding_notice_key(&mut self, key: KeyEvent) {
        let Some(notice) = self.keybinding_notice.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter | KeyCode::Esc => self.acknowledge_keybinding_notice(),
            KeyCode::Down | KeyCode::Char('j') => notice.scroll = notice.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => notice.scroll = notice.scroll.saturating_sub(1),
            _ => {}
        }
    }

    pub(super) fn handle_keybinding_notice_mouse(&mut self, mouse: MouseEvent) {
        let Some(notice) = self.keybinding_notice.as_mut() else {
            return;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if notice.button.contains((mouse.column, mouse.row).into()) =>
            {
                self.acknowledge_keybinding_notice();
            }
            MouseEventKind::ScrollDown => notice.scroll = notice.scroll.saturating_add(3),
            MouseEventKind::ScrollUp => notice.scroll = notice.scroll.saturating_sub(3),
            _ => {}
        }
    }

    pub(super) fn draw_keybinding_notice(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(notice) = self.keybinding_notice.as_mut() else {
            return;
        };
        let ui = &config::get().colors.ui;
        let accent = Style::default().fg(ui.title_fg).bold();
        let muted = Style::default().dim();
        let active = |command, description| {
            Line::from(vec![
                Span::styled(
                    self.keymap
                        .keys_for(command)
                        .unwrap_or_else(|| "Unbound".into()),
                    accent,
                ),
                Span::raw(format!("  {description}")),
            ])
        };
        let paragraph = Paragraph::new(vec![
            Line::from("The default kill key moved from x to X (Shift+x)."),
            Line::from("By default, x now dismisses the newest notification."),
            Line::from(""),
            Line::styled("Your active shortcuts", muted),
            active(Command::KillSelected, "Kill selected session"),
            active(Command::DismissNotification, "Dismiss newest notification"),
            Line::from(""),
            Line::styled("Customize [keybinds] in config.toml. Defaults:", muted),
            Line::from("kill = \"X\""),
            Line::from("dismiss_notification = \"x\""),
        ])
        .wrap(Wrap { trim: false });
        let width = area.width.saturating_sub(2).min(66);
        let content_lines = paragraph.line_count(width.saturating_sub(4));
        let height = (content_lines.saturating_add(5).min(u16::MAX as usize) as u16)
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
            .title(Span::styled(" Session shortcuts changed ", accent));
        clear_overlay(frame, popup);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let body = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(2),
        );
        notice.scroll = notice
            .scroll
            .min(content_lines.saturating_sub(body.height as usize) as u16);
        frame.render_widget(paragraph.scroll((notice.scroll, 0)), body);
        let label = " Enter: Got it ";
        let button_width = (label.len() as u16).min(inner.width);
        notice.button = Rect::new(
            inner.x + (inner.width - button_width) / 2,
            inner.bottom().saturating_sub(1),
            button_width,
            u16::from(inner.height > 0),
        );
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(accent.reversed()),
            notice.button,
        );
    }
}
