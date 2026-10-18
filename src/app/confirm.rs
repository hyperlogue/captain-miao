//! A confirmation overlay independent of the screen beneath it. Callers supply
//! content and a captured operation; this module owns layout, input and scrolling.
//! It never executes or drops an operation on its own: the caller consumes the
//! decision and payload, including any cancellation cleanup.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};

use super::format::clear_overlay;
use crate::config;

/// Content is laid out again on resize. A rich body can keep its summary and
/// column headings above the scrolling text; ordinary questions use only text.
#[derive(Debug, Default)]
pub(super) struct Body {
    pub pinned: Text<'static>,
    pub text: Text<'static>,
}

pub(super) trait Content: std::fmt::Debug {
    fn layout(&self, width: u16) -> Body;
}

impl Content for String {
    fn layout(&self, _width: u16) -> Body {
        Body {
            text: Text::from(self.clone()),
            ..Body::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    Accept,
    Reject,
}

#[derive(Debug)]
pub(super) struct Confirmation<T> {
    title: String,
    accept: Option<&'static str>,
    content: Box<dyn Content>,
    pub payload: Option<T>,
    scroll: u16,
    visible: u16,
    max_scroll: u16,
}

impl<T> Confirmation<T> {
    pub fn question(
        title: impl Into<String>,
        content: impl Content + 'static,
        accept: &'static str,
        payload: T,
    ) -> Self {
        Self {
            title: title.into(),
            accept: Some(accept),
            content: Box::new(content),
            payload: Some(payload),
            scroll: 0,
            visible: 1,
            max_scroll: 0,
        }
    }

    pub fn notice(title: impl Into<String>, text: String) -> Self {
        Self {
            title: title.into(),
            accept: None,
            content: Box::new(text),
            payload: None,
            scroll: 0,
            visible: 1,
            max_scroll: 0,
        }
    }

    pub fn hints(&self) -> Line<'static> {
        let attention = Style::default()
            .fg(config::get().colors.ui.attention_fg)
            .bold();
        let muted = Style::default().dim();
        if let Some(accept) = self.accept {
            Line::from(vec![
                Span::styled(format!("y/Enter {accept}"), attention),
                Span::raw("   "),
                Span::styled("Esc/Ctrl+C/n cancel", muted),
            ])
        } else {
            Line::styled("y/Enter/Esc/Ctrl+C/n close", muted)
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        self.scroll = (i32::from(self.scroll) + delta).clamp(0, i32::from(self.max_scroll)) as u16;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        // A held Enter must not accept the next queued question.
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('c') if key.kind == KeyEventKind::Press => {
                    return Some(Decision::Reject);
                }
                KeyCode::Char('d') => self.scroll_by(i32::from((self.visible / 2).max(1))),
                KeyCode::Char('u') => self.scroll_by(-i32::from((self.visible / 2).max(1))),
                _ => {}
            }
        } else if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter if key.kind == KeyEventKind::Press => {
                    return Some(Decision::Accept);
                }
                KeyCode::Char('n') | KeyCode::Esc if key.kind == KeyEventKind::Press => {
                    return Some(Decision::Reject);
                }
                KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
                KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
                KeyCode::PageDown => self.scroll_by(i32::from(self.visible)),
                KeyCode::PageUp => self.scroll_by(-i32::from(self.visible)),
                KeyCode::Home => self.scroll = 0,
                KeyCode::End => self.scroll = self.max_scroll,
                _ => {}
            }
        }
        None
    }

    pub fn handle_mouse(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            _ => {}
        }
    }

    pub fn draw(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let width = area.width.saturating_sub(2).min(106);
        let inner_width = width.saturating_sub(4);
        if inner_width == 0 || area.height < 3 {
            return;
        }
        let body = self.content.layout(inner_width);
        let pinned = Paragraph::new(body.pinned).wrap(Wrap { trim: false });
        let text = Paragraph::new(body.text).wrap(Wrap { trim: false });
        let pinned_height = pinned.line_count(inner_width).min(u16::MAX as usize) as u16;
        let text_height = text.line_count(inner_width).min(u16::MAX as usize) as u16;
        let footer = Paragraph::new(self.hints()).wrap(Wrap { trim: false });
        let footer_height = footer.line_count(inner_width).min(u16::MAX as usize) as u16;
        let height = pinned_height
            .saturating_add(text_height)
            .saturating_add(footer_height)
            .saturating_add(3)
            .min(24)
            .min(area.height);
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.buffer_mut().set_style(area, Style::default().dim());
        clear_overlay(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().dim())
            .padding(Padding::horizontal(1))
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default()
                    .fg(config::get().colors.ui.attention_fg)
                    .bold(),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        // Reserve the controls first so long questions cannot push them out of
        // the viewport. The remaining body always supports visual-line scrolling.
        let [body_area, gap, footer_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(footer_height),
        ])
        .areas(inner);
        let [pinned_area, text_area] = Layout::vertical([
            Constraint::Length(pinned_height.min(body_area.height.saturating_sub(1))),
            Constraint::Min(1),
        ])
        .areas(body_area);
        self.visible = text_area.height;
        self.max_scroll = text_height.saturating_sub(self.visible);
        self.scroll = self.scroll.min(self.max_scroll);
        frame.render_widget(pinned, pinned_area);
        frame.render_widget(text.scroll((self.scroll, 0)), text_area);
        if self.max_scroll > 0 {
            frame.render_widget(
                Paragraph::new("↑↓/PgUp/PgDn/Ctrl+U/D scroll").style(Style::default().dim()),
                gap,
            );
        }
        frame.render_widget(footer, footer_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render<T>(confirmation: &mut Confirmation<T>, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| confirmation.draw(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width.max(1)))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wrapped_questions_scroll_and_resize_without_losing_the_controls() {
        let body = format!("FIRST {} LAST", "long question ".repeat(100));
        let mut confirmation = Confirmation::question("Confirm", body, "allow", ());
        let initial = render(&mut confirmation, 40, 12);
        assert!(initial.contains("FIRST"));
        assert!(initial.contains("y/Enter allow"));
        assert!(initial.contains("Esc/Ctrl+C/n"), "{initial}");
        assert!(initial.contains("cancel"), "{initial}");
        assert!(!initial.contains("LAST"));
        confirmation.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        let next = render(&mut confirmation, 40, 12);
        assert!(!next.contains("FIRST"));
        confirmation.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(render(&mut confirmation, 40, 12).contains("FIRST"));
        confirmation.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(render(&mut confirmation, 40, 12).contains("LAST"));
        let wider = render(&mut confirmation, 120, 30);
        assert!(
            wider.contains("LAST"),
            "resize clamps scroll to the new end"
        );
        confirmation.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert!(render(&mut confirmation, 120, 30).contains("FIRST"));
    }

    #[test]
    fn tiny_viewports_do_not_break_the_decision() {
        let mut confirmation =
            Confirmation::question("Confirm", "Continue?".to_string(), "allow", 7);
        for (width, height) in [(0, 0), (1, 1), (4, 3), (8, 5), (20, 8)] {
            render(&mut confirmation, width, height);
        }
        assert_eq!(
            confirmation.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Decision::Accept)
        );
        assert_eq!(confirmation.payload, Some(7));
    }
}
