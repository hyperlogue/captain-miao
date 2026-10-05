//! Floating, in-memory notifications shared by dashboard operations.
//!
//! A running operation updates one notification by id. Informational results
//! expire; warnings and errors remain until dismissed. Dismissing progress
//! hides that notice, but its eventual result is still shown. The message log
//! retains every update independently of the popup's lifetime.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
};

use super::{App, InputMode, format::clear_overlay, keymap::Command};

const LIFETIME: Duration = Duration::from_secs(5);
const FADE_IN: Duration = Duration::from_millis(180);
const SPINNER_STEP: Duration = Duration::from_millis(100);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(super) type NotificationId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    Info,
    Progress,
    Success,
    Warning,
    Error,
}

impl Level {
    fn expires(self) -> bool {
        matches!(self, Self::Info | Self::Success)
    }

    fn needs_attention(self) -> bool {
        matches!(self, Self::Warning | Self::Error)
    }
}

#[derive(Debug)]
struct Notification {
    id: NotificationId,
    level: Level,
    text: String,
    created: Instant,
    updated: Instant,
}

#[derive(Default)]
pub(super) struct Notifications {
    entries: Vec<Notification>,
    next_id: NotificationId,
    /// Drawn rectangles only: a click on a popup must not reach the table.
    hitboxes: Vec<(NotificationId, Rect)>,
    was_fading: bool,
    spinner_phase: Option<usize>,
}

impl Notifications {
    pub(super) fn push(&mut self, level: Level, text: String, now: Instant) -> NotificationId {
        // Repeated failures stay visible without filling the entire viewport.
        if level != Level::Progress
            && let Some(entry) = self
                .entries
                .iter_mut()
                .find(|n| n.level == level && n.text == text)
        {
            entry.updated = now;
            return entry.id;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.update(id, level, text, now);
        id
    }

    pub(super) fn update(&mut self, id: NotificationId, level: Level, text: String, now: Instant) {
        if let Some(entry) = self.entries.iter_mut().find(|n| n.id == id) {
            entry.level = level;
            entry.text = text;
            entry.updated = now;
        } else {
            self.entries.push(Notification {
                id,
                level,
                text,
                created: now,
                updated: now,
            });
        }
    }

    pub(super) fn dismiss_latest(&mut self) {
        self.entries.pop();
        self.hitboxes
            .retain(|(id, _)| self.entries.iter().any(|n| n.id == *id));
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some((id, rect)) = self
            .hitboxes
            .iter()
            .copied()
            .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
        else {
            return false;
        };
        // Only the close button dismisses; all pointer events over a popup
        // are consumed so scrolling/clicking cannot affect obscured content.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && mouse.row == rect.y
            && mouse.column >= rect.right().saturating_sub(4)
        {
            self.entries.retain(|n| n.id != id);
            self.hitboxes.retain(|(other, _)| *other != id);
        }
        true
    }

    /// Returns true exactly when a timer needs a new frame, including the
    /// final fade frame and the removal of the last transient notification.
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let len = self.entries.len();
        self.entries
            .retain(|n| !n.level.expires() || now.duration_since(n.updated) < LIFETIME);
        let fading = self
            .entries
            .iter()
            .any(|n| now.duration_since(n.created) < FADE_IN);
        let phase = self
            .entries
            .iter()
            .find(|n| n.level == Level::Progress)
            .map(|n| {
                (now.duration_since(n.created).as_millis() / SPINNER_STEP.as_millis()) as usize
                    % SPINNER.len()
            });
        let changed =
            len != self.entries.len() || fading != self.was_fading || phase != self.spinner_phase;
        self.was_fading = fading;
        self.spinner_phase = phase;
        changed
    }

    pub(super) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.entries
            .iter()
            .flat_map(|n| {
                let fade = n.created + FADE_IN;
                [
                    (fade > now).then(|| fade.saturating_duration_since(now)),
                    n.level
                        .expires()
                        .then(|| (n.updated + LIFETIME).saturating_duration_since(now)),
                    (n.level == Level::Progress).then_some(SPINNER_STEP),
                ]
            })
            .flatten()
            .min()
    }

    fn draw(
        &mut self,
        frame: &mut ratatui::Frame,
        area: Rect,
        dismiss: Option<String>,
        history: Option<String>,
    ) {
        self.hitboxes.clear();
        if area.width < 12 || area.height < 4 {
            return;
        }
        let now = Instant::now();
        let ui = &crate::config::get().colors.ui;
        let width = 58.min(area.width.saturating_sub(2));
        let x = area.right() - width - 1;
        // Leave one blank row between the stack and the footer.
        let mut bottom = area.bottom().saturating_sub(1);
        for entry in self.entries.iter().rev() {
            // Reserve a row above the stack for its overflow count.
            let available = bottom.saturating_sub(area.y + 1);
            if available < 3 {
                break;
            }
            let (title, color) = match entry.level {
                Level::Info => ("Info".to_string(), ui.title_fg),
                Level::Progress => {
                    let phase =
                        now.duration_since(entry.created).as_millis() / SPINNER_STEP.as_millis();
                    (
                        format!("{} Working", SPINNER[phase as usize % SPINNER.len()]),
                        ui.title_fg,
                    )
                }
                Level::Success => ("✓ Done".to_string(), ui.title_fg),
                Level::Warning => ("! Warning".to_string(), ui.attention_fg),
                Level::Error => ("× Error".to_string(), ui.error_fg),
            };
            let paragraph = Paragraph::new(entry.text.as_str()).wrap(Wrap { trim: false });
            let lines = paragraph.line_count(width.saturating_sub(4));
            let height = (lines.saturating_add(2).min(10) as u16).min(available);
            let rect = Rect::new(x, bottom - height, width, height);
            let mut block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(color))
                .padding(Padding::horizontal(1))
                .title(Span::styled(format!(" {title} "), Style::default().bold()))
                .title(Line::from(" × ").alignment(Alignment::Right));
            let mut hints = Vec::new();
            if let Some(key) = &dismiss {
                hints.push(format!("{key} dismiss newest"));
            }
            if lines > height.saturating_sub(2) as usize
                && let Some(key) = &history
            {
                hints.push(format!("{key} more"));
            }
            if !hints.is_empty() {
                block = block.title_bottom(Line::from(format!(" {} ", hints.join(" · "))));
            }
            clear_overlay(frame, rect);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            frame.render_widget(paragraph, inner);
            // Native terminal dimming gives a brief fade-in without assuming
            // RGB values for the user's palette or adding an animation crate.
            if now.duration_since(entry.created) < FADE_IN {
                frame
                    .buffer_mut()
                    .set_style(rect, Style::default().add_modifier(Modifier::DIM));
            }
            self.hitboxes.push((entry.id, rect));
            bottom = rect.y.saturating_sub(1);
        }
        let hidden = self.entries.len().saturating_sub(self.hitboxes.len());
        if hidden > 0 {
            let hint = history
                .map(|key| format!(" · {key} history"))
                .unwrap_or_default();
            let rect = Rect::new(x, bottom.max(area.y), width, 1);
            clear_overlay(frame, rect);
            frame.render_widget(
                Paragraph::new(format!("+{hidden} more notifications{hint}")),
                rect,
            );
        }
    }
}

impl App {
    pub(super) fn notify(&mut self, level: Level, msg: String) -> NotificationId {
        self.record_notification(level, &msg);
        self.notifications.push(level, msg, Instant::now())
    }

    pub(super) fn update_notification(&mut self, id: NotificationId, level: Level, msg: String) {
        self.record_notification(level, &msg);
        self.notifications.update(id, level, msg, Instant::now());
    }

    fn record_notification(&mut self, level: Level, msg: &str) {
        self.messages.push(msg, level);
        self.status_msg = Some(msg.to_string());
        self.status_is_error = level.needs_attention();
    }

    pub(super) fn draw_notifications(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        // Keep history and the startup shortcut announcement unobscured.
        if matches!(self.input_mode, InputMode::Messages | InputMode::Help)
            || self.upgrade_notices.is_some()
        {
            self.notifications.hitboxes.clear();
            return;
        }
        let normal = self.input_mode == InputMode::Normal
            && !self.session_detail
            && !self.pending_g
            && self.pending_prefix.is_empty();
        let dismiss = normal
            .then(|| {
                let clear = self
                    .search_filter
                    .is_none()
                    .then(|| self.keymap.primary_key(Command::ClearSearch))
                    .flatten();
                let direct = self.keymap.primary_key(Command::DismissNotification);
                let keys: Vec<_> = [clear, direct].into_iter().flatten().collect();
                (!keys.is_empty()).then(|| keys.join("/"))
            })
            .flatten();
        let history = normal
            .then(|| self.keymap.primary_key(Command::MessageLog))
            .flatten();
        self.notifications.draw(frame, area, dismiss, history);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn transient_results_expire_but_progress_and_attention_require_resolution() {
        let now = Instant::now();
        let mut notices = Notifications::default();
        for level in [
            Level::Info,
            Level::Success,
            Level::Progress,
            Level::Warning,
            Level::Error,
        ] {
            notices.push(level, format!("{level:?}"), now);
        }
        notices.tick(now + LIFETIME);
        assert_eq!(
            notices.entries.iter().map(|n| n.level).collect::<Vec<_>>(),
            vec![Level::Progress, Level::Warning, Level::Error]
        );
        for _ in 0..3 {
            notices.dismiss_latest();
        }
        assert!(notices.entries.is_empty());
    }

    #[test]
    fn completing_progress_reuses_its_popup_and_starts_a_fresh_lifetime() {
        let now = Instant::now();
        let mut notices = Notifications::default();
        let id = notices.push(Level::Progress, "Preparing push".into(), now);
        notices.update(id, Level::Progress, "Pushing main".into(), now + LIFETIME);
        notices.tick(now + LIFETIME * 2);
        assert_eq!(notices.entries.len(), 1);
        notices.update(id, Level::Success, "Pushed".into(), now + LIFETIME * 2);
        notices.tick(now + LIFETIME * 2 + Duration::from_secs(1));
        assert_eq!(notices.entries.len(), 1);
        assert_eq!(notices.entries[0].id, id);
        assert_eq!(notices.entries[0].text, "Pushed");
        assert!(notices.tick(now + LIFETIME * 3));
        assert!(notices.entries.is_empty());
        assert!(!notices.tick(now + LIFETIME * 4));
        assert_eq!(notices.next_wakeup(now + LIFETIME * 4), None);
    }

    #[test]
    fn a_dismissed_operation_still_shows_its_result_and_repeated_errors_coalesce() {
        let now = Instant::now();
        let mut notices = Notifications::default();
        let id = notices.push(Level::Progress, "Pushing".into(), now);
        notices.dismiss_latest();
        notices.update(id, Level::Error, "Push failed".into(), now);
        assert_eq!(notices.entries.len(), 1);
        assert_eq!(notices.push(Level::Error, "Push failed".into(), now), id);
        assert_eq!(notices.entries.len(), 1);
    }

    #[test]
    fn fade_finishes_and_persistent_errors_stop_waking_the_loop() {
        let now = Instant::now();
        let mut notices = Notifications::default();
        notices.push(Level::Error, "Failed".into(), now);
        assert!(notices.tick(now));
        assert_eq!(notices.next_wakeup(now), Some(FADE_IN));
        assert!(notices.tick(now + FADE_IN));
        assert!(!notices.tick(now + FADE_IN));
        assert_eq!(notices.next_wakeup(now + FADE_IN), None);
    }

    #[test]
    fn stack_wraps_on_narrow_screens_and_keeps_overflow_until_dismissed() {
        let mut notices = Notifications::default();
        let now = Instant::now();
        for i in 0..8 {
            notices.push(
                Level::Error,
                format!(
                    "Failure {i}: a long message with unicode 界 and a long/path/to/a/checkout"
                ),
                now,
            );
        }
        for (width, height) in [(80, 30), (40, 18), (18, 8), (8, 3), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let area = Rect::new(0, 0, width, height);
            terminal
                .draw(|frame| notices.draw(frame, area, Some("x".into()), Some("Space m".into())))
                .unwrap();
            for (_, rect) in &notices.hitboxes {
                assert_eq!(rect.intersection(area), *rect);
            }
            for pair in notices.hitboxes.windows(2) {
                assert!(pair[1].1.bottom() < pair[0].1.y);
            }
            if width >= 18 {
                assert!(!notices.hitboxes.is_empty());
                assert_eq!(notices.hitboxes[0].0, 7);
                assert_eq!(notices.hitboxes[0].1.bottom(), area.bottom() - 1);
                assert!(notices.hitboxes.len() < notices.entries.len());
            }
        }
        notices.tick(now + LIFETIME * 10);
        assert_eq!(notices.entries.len(), 8, "hidden errors cannot expire");
        notices.dismiss_latest();
        assert_eq!(notices.entries.last().unwrap().id, 6);
    }

    #[test]
    fn pointer_events_on_a_popup_are_consumed_without_dismissing_its_body() {
        let now = Instant::now();
        let mut notices = Notifications::default();
        let id = notices.push(Level::Warning, "Warning".into(), now);
        notices.hitboxes.push((id, Rect::new(40, 10, 40, 5)));
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(notices.handle_mouse(mouse(MouseEventKind::ScrollDown, 45, 12)));
        assert!(notices.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 45, 12)));
        assert_eq!(notices.entries.len(), 1);
        assert!(!notices.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 20, 12)));
        assert!(notices.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 78, 10)));
        assert!(notices.entries.is_empty());
    }
}
