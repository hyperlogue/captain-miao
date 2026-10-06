//! Split announcement inbox: scrollable details above a selectable list.
//!
//! The overlay preserves the underlying input mode, including crash recovery.
//! Viewing details marks an item read for this popup only. Enter visits unread
//! items before acknowledging the batch; Escape postpones it. Only the saved
//! dashboard version persists, so leaving early keeps the whole batch pending.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

use super::{
    Action, App,
    announcements::{Announcement, Content, Kind},
    format::clear_overlay,
};
use crate::config;

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    List,
    Details,
}

pub(super) struct UpgradeNotices {
    items: Vec<&'static Announcement>,
    read: Vec<bool>,
    list: ListState,
    focus: Focus,
    list_area: Rect,
    detail_area: Rect,
    button: Rect,
    copy_button: Rect,
    pub(super) copy_feedback: Option<String>,
    scroll: u16,
}

impl UpgradeNotices {
    pub(super) fn new(mut items: Vec<&'static Announcement>) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        // Keep release order within each kind, with required actions first.
        items.sort_by_key(|item| !matches!(item.kind, Kind::Warning));
        Some(Self {
            read: vec![false; items.len()],
            items,
            list: ListState::default().with_selected(Some(0)),
            focus: Focus::List,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            button: Rect::default(),
            copy_button: Rect::default(),
            copy_feedback: None,
            scroll: 0,
        })
    }

    fn selected(&self) -> &'static Announcement {
        self.items[self.list.selected().unwrap_or(0)]
    }

    fn select(&mut self, index: usize) {
        let index = index.min(self.items.len() - 1);
        if self.list.selected() != Some(index) {
            self.list.select(Some(index));
            self.scroll = 0;
            self.copy_feedback = None;
            // Invalidate old action targets until the new details render.
            self.copy_button = Rect::default();
            self.button = Rect::default();
        }
    }

    fn move_selection(&mut self, delta: isize) {
        self.select(
            self.list
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(delta),
        );
    }

    /// Return true once every item has been displayed. Otherwise select the
    /// next unread item, wrapping around any items the user already browsed.
    fn advance(&mut self) -> bool {
        let selected = self.list.selected().unwrap_or(0);
        // Repeated input before a frame must not skip unseen details.
        if !self.read[selected] {
            return false;
        }
        if let Some(next) = (1..self.items.len())
            .map(|offset| (selected + offset) % self.items.len())
            .find(|&index| !self.read[index])
        {
            self.select(next);
            false
        } else {
            true
        }
    }

    fn copy_snippet(&self) -> Option<Action> {
        let snippets: Vec<_> = self
            .selected()
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

/// Style the table, key and value of small TOML examples; other snippets stay
/// plain. Presentation spacing is separate from the exact source copied out.
fn code_lines(text: &'static str, accent: Style, value: Style, muted: Style) -> Vec<Line<'static>> {
    let toml = toml::from_str::<toml::Value>(text).is_ok();
    let mut lines = vec![
        Line::default(),
        Line::styled(if toml { "  TOML" } else { "  Code" }, muted),
        Line::default(),
    ];
    for line in text.split('\n') {
        let mut spans = vec![Span::raw("  ")];
        if toml && line.trim().starts_with('[') && line.trim().ends_with(']') {
            spans.push(Span::styled(line, accent));
            lines.push(Line::from(spans));
            lines.push(Line::default());
            continue;
        } else if toml && let Some((key, rest)) = line.split_once('=') {
            spans.extend([
                Span::styled(key, accent),
                Span::styled("=", muted),
                Span::styled(rest, value),
            ]);
        } else {
            spans.push(Span::raw(line));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines
}

impl App {
    fn acknowledge_upgrade_notices(&mut self) {
        if self.upgrade_notices.take().is_none() {
            return;
        }
        if let Err(error) = self.dashboard_state.finish_startup() {
            self.set_status(
                format!("Could not save the dashboard version; updates may appear again: {error}"),
                true,
            );
        }
    }

    pub(super) fn handle_upgrade_notices_key(&mut self, key: KeyEvent) -> Option<Action> {
        let notice = self.upgrade_notices.as_mut()?;
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match key.code {
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if notice.advance() {
                    self.acknowledge_upgrade_notices();
                }
            }
            KeyCode::Esc if key.kind == KeyEventKind::Press => {
                self.upgrade_notices = None;
            }
            KeyCode::Tab | KeyCode::BackTab if key.kind == KeyEventKind::Press => {
                notice.focus = if notice.focus == Focus::List {
                    Focus::Details
                } else {
                    Focus::List
                };
            }
            KeyCode::Char('c') if key.kind == KeyEventKind::Press => return notice.copy_snippet(),
            KeyCode::Down | KeyCode::Char('j') => {
                if notice.focus == Focus::List {
                    notice.move_selection(1);
                } else {
                    notice.scroll = notice.scroll.saturating_add(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if notice.focus == Focus::List {
                    notice.move_selection(-1);
                } else {
                    notice.scroll = notice.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageDown => {
                notice.scroll = notice
                    .scroll
                    .saturating_add(notice.detail_area.height.max(1))
            }
            KeyCode::PageUp => {
                notice.scroll = notice
                    .scroll
                    .saturating_sub(notice.detail_area.height.max(1))
            }
            KeyCode::Home => {
                if notice.focus == Focus::List {
                    notice.select(0);
                } else {
                    notice.scroll = 0;
                }
            }
            KeyCode::End => {
                if notice.focus == Focus::List {
                    notice.select(notice.items.len() - 1);
                } else {
                    notice.scroll = u16::MAX;
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn handle_upgrade_notices_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        let notice = self.upgrade_notices.as_mut()?;
        let at = (mouse.column, mouse.row).into();
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if notice.copy_button.contains(at) => {
                return notice.copy_snippet();
            }
            MouseEventKind::Down(MouseButton::Left) if notice.button.contains(at) => {
                if notice.advance() {
                    self.acknowledge_upgrade_notices();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if notice.list_area.contains(at) => {
                notice.focus = Focus::List;
                let index = notice.list.offset() + usize::from(mouse.row - notice.list_area.y);
                if index < notice.items.len() {
                    notice.select(index);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if notice.detail_area.contains(at) => {
                notice.focus = Focus::Details
            }
            MouseEventKind::ScrollDown if notice.list_area.contains(at) => notice.move_selection(1),
            MouseEventKind::ScrollUp if notice.list_area.contains(at) => notice.move_selection(-1),
            MouseEventKind::ScrollDown if notice.detail_area.contains(at) => {
                notice.scroll = notice.scroll.saturating_add(3)
            }
            MouseEventKind::ScrollUp if notice.detail_area.contains(at) => {
                notice.scroll = notice.scroll.saturating_sub(3)
            }
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
        let muted = Style::default().dim();
        let item = notice.selected();
        let mut lines = vec![
            Line::styled(item.title, Style::default().bold()),
            Line::default(),
        ];
        for content in item.content {
            match content {
                Content::Text(text) => lines.extend(text.split('\n').map(Line::from)),
                Content::Code(text) => lines.extend(code_lines(
                    text,
                    accent,
                    Style::default().fg(ui.attention_fg),
                    muted,
                )),
                Content::Heading(text) => lines.push(Line::styled(*text, Style::default().bold())),
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
        let width = area.width.saturating_sub(2).min(80);
        let height = area.height.saturating_sub(2).min(36);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let hint = if notice.focus == Focus::List {
            " j/k: select · Tab: details · Esc: later "
        } else {
            " j/k: scroll · Tab: updates · Esc: later "
        };
        let copy_label = " c: Copy snippet ";
        let has_snippet = item.content.iter().any(|c| matches!(c, Content::Code(_)));
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .border_style(accent)
            .title(Span::styled(
                format!(" What's new in miao v{} ", env!("CARGO_PKG_VERSION")),
                accent,
            ));
        if has_snippet {
            block = block.title_bottom(Line::styled(copy_label, accent));
        }
        let hints_width = hint.chars().count() + if has_snippet { copy_label.len() + 2 } else { 0 };
        if hints_width <= usize::from(width.saturating_sub(2)) {
            block = block.title_bottom(Line::styled(hint, muted).alignment(Alignment::Right));
        }
        clear_overlay(frame, popup);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        notice.copy_button = if has_snippet && popup.height > 1 {
            Rect::new(
                popup.x + 1,
                popup.bottom() - 1,
                (copy_label.len() as u16).min(popup.width.saturating_sub(2)),
                1,
            )
        } else {
            Rect::default()
        };

        let button_rows = inner.height.min(1);
        let feedback = Paragraph::new(notice.copy_feedback.as_deref().unwrap_or(""))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false });
        let feedback_height = if notice.copy_feedback.is_some() {
            (feedback.line_count(inner.width).clamp(1, 3) as u16)
                .min(inner.height.saturating_sub(button_rows))
        } else {
            0
        };
        let content_height = inner.height.saturating_sub(button_rows + feedback_height);
        // Details get most of the space. Keep at least one list row on compact
        // terminals; both panes can scroll independently.
        let list_height = (notice.items.len().min(5) as u16 + 2)
            .min((content_height / 3).max(3))
            .min(content_height);
        let gap = u16::from(content_height >= 12);
        let details = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            content_height.saturating_sub(list_height + gap),
        );
        let mut detail_block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .border_style(if notice.focus == Focus::Details {
                accent
            } else {
                muted
            })
            .title(format!(
                " Details · {} · v{}{} ",
                item.kind.label(),
                item.introduced,
                if notice.focus == Focus::Details {
                    " · focused"
                } else {
                    ""
                }
            ));
        notice.detail_area = detail_block.inner(details);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let content_lines = paragraph.line_count(notice.detail_area.width);
        notice.scroll = notice.scroll.min(
            content_lines
                .saturating_sub(notice.detail_area.height as usize)
                .min(u16::MAX as usize) as u16,
        );
        if content_lines > usize::from(notice.detail_area.height) {
            let above = notice.scroll > 0;
            let below =
                usize::from(notice.scroll) + usize::from(notice.detail_area.height) < content_lines;
            let more = match (above, below) {
                (true, true) => " ↑ more ↓ ",
                (true, false) => " ↑ more ",
                _ => " more ↓ ",
            };
            detail_block =
                detail_block.title(Line::styled(more, muted).alignment(Alignment::Right));
        }
        frame.render_widget(detail_block, details);
        frame.render_widget(paragraph.scroll((notice.scroll, 0)), notice.detail_area);
        if notice.detail_area.width > 0 && notice.detail_area.height > 0 {
            notice.read[notice.list.selected().unwrap_or(0)] = true;
        }

        let read_count = notice.read.iter().filter(|&&read| read).count();
        let list_rect = Rect::new(inner.x, details.bottom() + gap, inner.width, list_height);
        let list_block = Block::default()
            .borders(Borders::ALL)
            .border_style(if notice.focus == Focus::List {
                accent
            } else {
                muted
            })
            .title(format!(
                " Updates · {}/{} · {read_count} read{} ",
                notice.list.selected().unwrap_or(0) + 1,
                notice.items.len(),
                if notice.focus == Focus::List {
                    " · focused"
                } else {
                    ""
                }
            ))
            .title_bottom(Line::styled(" ✓ read · • unread ", muted));
        notice.list_area = list_block.inner(list_rect);
        let rows: Vec<_> = notice
            .items
            .iter()
            .zip(&notice.read)
            .map(|(item, read)| {
                let kind_style = match item.kind {
                    Kind::Warning => Style::default().fg(ui.attention_fg),
                    Kind::Update => Style::default().fg(ui.title_fg),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(if *read { "✓ " } else { "• " }, muted),
                    Span::styled(format!("{:<7} ", item.kind.label()), kind_style),
                    Span::raw(item.title),
                    Span::styled(format!("  v{}", item.introduced), muted),
                ]))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(rows)
                .block(list_block)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(ui.highlight_bg).bold()),
            list_rect,
            &mut notice.list,
        );
        frame.render_widget(
            feedback,
            Rect::new(inner.x, list_rect.bottom(), inner.width, feedback_height),
        );

        let label = if read_count == notice.items.len() {
            " Enter: Got it "
        } else {
            " Enter: Next unread "
        };
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
