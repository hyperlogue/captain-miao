//! In-memory notification history.
//!
//! Notifications expire or are dismissed, while the last [`MAX_ENTRIES`]
//! messages remain available in the message-log popup. Every notification
//! update is recorded here, including an operation's progress and outcome.
//!
//! **Memory only, on purpose.** These lines quote cwds, host targets and prompt
//! text — the reason the state files are `0600` in the first place — and a log
//! that survives a restart is one that then needs rotating, ageing out and
//! cleaning up, none of which a scrollback of transient UI notices is worth. The
//! cap keeps it bounded from the other end: a wedged loop repeating one error
//! doesn't grow the log at all, because a repeat of the newest entry only bumps
//! its counter.

use std::collections::VecDeque;
use std::ops::Range;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::format::{centered_rect, clear_overlay};
use super::notifications::Level;
use super::{Action, App};
use crate::config;

const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone)]
pub(super) struct MessageEntry {
    id: u64,
    /// Repeats refresh both clocks. The monotonic clock keeps ages stable when
    /// the wall clock changes; the timestamp records local time at receipt.
    at: Instant,
    timestamp: String,
    pub(super) text: String,
    level: Level,
    repeats: u32,
}

#[derive(Debug, Default)]
pub(crate) struct MessageLog {
    entries: VecDeque<MessageEntry>,
    next_id: u64,
}

impl MessageLog {
    pub(super) fn push(&mut self, text: &str, level: Level) {
        let at = Instant::now();
        let timestamp = local_timestamp(SystemTime::now());
        if let Some(last) = self.entries.back_mut()
            && last.level == level
            && last.text == text
        {
            last.repeats = last.repeats.saturating_add(1);
            last.at = at;
            last.timestamp = timestamp;
            return;
        }
        self.entries.push_back(MessageEntry {
            id: self.next_id,
            at,
            timestamp,
            text: text.to_owned(),
            level,
            repeats: 1,
        });
        self.next_id += 1;
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &MessageEntry> {
        self.entries.iter()
    }

    /// Source offsets survive resizing, wrapping and new arrivals. Soft line
    /// breaks and all panel chrome are excluded from the clipboard payload.
    fn selected_text(&self, selection: Selection) -> String {
        let Some((start, end)) = selection.bounds() else {
            return String::new();
        };
        self.entries
            .iter()
            .filter_map(|entry| {
                if entry.id < start.id || entry.id > end.id {
                    return None;
                }
                let from = if entry.id == start.id { start.byte } else { 0 };
                let to = if entry.id == end.id {
                    end.byte
                } else {
                    entry.text.len()
                };
                entry.text.get(from..to)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// ISO 8601 with seconds and the local UTC offset, including daylight saving.
fn local_timestamp(at: SystemTime) -> String {
    let seconds = match at.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => libc::time_t::try_from(elapsed.as_secs()).ok(),
        Err(early) => {
            let duration = early.duration();
            let seconds = duration.as_secs() + u64::from(duration.subsec_nanos() != 0);
            libc::time_t::try_from(seconds)
                .ok()
                .and_then(|s| s.checked_neg())
        }
    };
    let Some(seconds) = seconds else {
        return "time unavailable".into();
    };
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for the call, and localtime_r initializes
    // the entire tm on success. Unlike localtime, it owns no shared buffer.
    let local = unsafe {
        if libc::localtime_r(&seconds, local.as_mut_ptr()).is_null() {
            return "time unavailable".into();
        }
        local.assume_init()
    };
    timestamp_from_local(&local)
}

fn timestamp_from_local(local: &libc::tm) -> String {
    let offset = local.tm_gmtoff;
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec,
        minutes / 60,
        minutes % 60,
    )
}

fn age(seconds: u64) -> String {
    let (value, unit) = if seconds >= 86400 {
        (seconds / 86400, "d")
    } else if seconds >= 3600 {
        (seconds / 3600, "h")
    } else if seconds >= 60 {
        (seconds / 60, "m")
    } else {
        (seconds, "s")
    };
    format!("(-{value}{unit})")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    id: u64,
    byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    start: Position,
    end: Position,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: Hit,
    focus: Hit,
    moved: bool,
    dragging: bool,
}

impl Selection {
    fn bounds(self) -> Option<(Position, Position)> {
        self.moved.then(|| {
            (
                self.anchor.start.min(self.focus.start),
                self.anchor.end.max(self.focus.end),
            )
        })
    }
}

/// One physical row and its source range. Metadata-only rows appear on narrow
/// terminals. They have no hit target, so timestamps can never be copied.
#[derive(Debug)]
struct MessageRow {
    id: u64,
    metadata: String,
    label: String,
    level: Level,
    source: Option<Range<usize>>,
    text: String,
}

#[derive(Debug)]
pub(crate) struct MessageLogView {
    pub(in crate::app) scroll: usize,
    pub(in crate::app) rows: usize,
    rendered: Vec<MessageRow>,
    viewport: Rect,
    text_x: u16,
    selection: Option<Selection>,
    pub(super) copy_feedback: Option<String>,
    last_draw: Instant,
}

impl MessageLogView {
    pub(super) fn at_bottom() -> Self {
        Self {
            scroll: usize::MAX,
            rows: 0,
            rendered: Vec::new(),
            viewport: Rect::default(),
            text_x: 0,
            selection: None,
            copy_feedback: None,
            last_draw: Instant::now(),
        }
    }

    pub(super) fn next_tick(&self) -> Duration {
        Duration::from_secs(1).saturating_sub(self.last_draw.elapsed())
    }

    fn hit(&self, column: u16, row: u16, clamp: bool) -> Option<Hit> {
        if self.viewport.is_empty() || self.rendered.is_empty() {
            return None;
        }
        if !clamp && (column < self.text_x || !self.viewport.contains((column, row).into())) {
            return None;
        }
        let row = row.clamp(self.viewport.y, self.viewport.bottom().saturating_sub(1));
        let index = self
            .scroll
            .saturating_add(usize::from(row - self.viewport.y));
        if !clamp && index >= self.rendered.len() {
            return None;
        }
        let index = index.min(self.rendered.len() - 1);
        let line = &self.rendered[index];
        let range = line.source.as_ref()?;
        let cell = usize::from(column.saturating_sub(self.text_x));
        let mut x = 0;
        for (byte, glyph) in line.text.grapheme_indices(true) {
            x += glyph.width();
            if cell < x {
                return Some(Hit {
                    start: Position {
                        id: line.id,
                        byte: range.start + byte,
                    },
                    end: Position {
                        id: line.id,
                        byte: range.start + byte + glyph.len(),
                    },
                });
            }
        }
        let end = Position {
            id: line.id,
            byte: range.end,
        };
        Some(Hit { start: end, end })
    }
}

fn severity(level: Level) -> (&'static str, Color) {
    let ui = &config::get().colors.ui;
    match level {
        Level::Warning => ("WARNING", ui.attention_fg),
        Level::Error => ("ERROR", ui.error_fg),
        Level::Info | Level::Success | Level::Progress => ("INFO", ui.header_fg),
    }
}

/// Wrap by display cells, retaining byte ranges into the original message.
/// Spaces hidden at soft breaks remain in the source when a selection crosses
/// that break. Hard newlines, indentation, wide glyphs and combining marks stay
/// intact as well.
fn wrap(text: &str, width: usize) -> Vec<Range<usize>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut base = 0;
    for para in text.split('\n') {
        let mut start = 0;
        loop {
            let mut end = start;
            let mut cells = 0;
            let mut word_break = None;
            for (byte, glyph) in para[start..].grapheme_indices(true) {
                if cells + glyph.width() > width && end > start {
                    if !glyph.chars().all(char::is_whitespace)
                        && let Some(boundary) = word_break
                    {
                        end = boundary;
                    }
                    break;
                }
                if glyph.chars().all(char::is_whitespace) && byte > 0 {
                    word_break = Some(start + byte);
                }
                cells += glyph.width();
                end = start + byte + glyph.len();
            }
            out.push(base + start..base + end);
            if end == para.len() {
                break;
            }
            start = end;
            // Skip only the separator at a soft break, never indentation at
            // the beginning of a source line.
            while let Some(c) = para[start..].chars().next().filter(|c| c.is_whitespace()) {
                start += c.len_utf8();
            }
            if start == para.len() {
                break;
            }
        }
        base += para.len() + 1;
    }
    out
}

impl App {
    pub(super) fn handle_message_log_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        let view = self.message_view.as_mut()?;
        match mouse.kind {
            MouseEventKind::ScrollUp => view.scroll = view.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => view.scroll = view.scroll.saturating_add(3),
            MouseEventKind::Down(MouseButton::Left) => {
                view.copy_feedback = None;
                view.selection = view
                    .hit(mouse.column, mouse.row, false)
                    .map(|anchor| Selection {
                        anchor,
                        focus: anchor,
                        moved: false,
                        dragging: true,
                    });
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                let hit = view.hit(mouse.column, mouse.row, true);
                let selection = view.selection.as_mut().filter(|s| s.dragging)?;
                if let Some(hit) = hit {
                    selection.moved |= selection.focus != hit;
                    selection.focus = hit;
                }
                if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                    selection.dragging = false;
                    let text = self.messages.selected_text(*selection);
                    if !text.is_empty() {
                        return Some(Action::CopyMessageSelection(text));
                    }
                } else if mouse.row < view.viewport.y {
                    view.scroll = view.scroll.saturating_sub(1);
                } else if mouse.row >= view.viewport.bottom() {
                    view.scroll = view.scroll.saturating_add(1);
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn draw_message_log(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(view) = self.message_view.as_mut() else {
            return;
        };
        let popup = centered_rect(if area.width > 100 { 90 } else { 96 }, 80, area);
        clear_overlay(frame, popup);
        let ui = &config::get().colors.ui;
        let muted = Style::default().dim();
        let hint = view
            .copy_feedback
            .as_deref()
            .unwrap_or("Drag text to select · release to copy");
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .title(Span::styled(
                format!(" Messages · {} ", self.messages.len()),
                Style::default().fg(ui.title_fg).bold(),
            ))
            .title_bottom(Line::from(format!(" {hint} ")).style(muted));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let now = Instant::now();
        let metadata: Vec<_> = self
            .messages
            .entries
            .iter()
            .map(|entry| {
                let age = age(now.saturating_duration_since(entry.at).as_secs());
                let repeats = if entry.repeats > 1 {
                    format!(" ×{}", entry.repeats)
                } else {
                    String::new()
                };
                format!("{} {age}{repeats}", entry.timestamp)
            })
            .collect();
        let metadata_width = metadata
            .iter()
            .map(|m| m.width())
            .max()
            .unwrap_or(31)
            .max(31);
        let columns = usize::from(inner.width) >= metadata_width + 3 + 8 + 20;
        let gutter = if columns { metadata_width + 3 } else { 0 };
        // Narrow terminals put time above the body, retaining a useful text
        // width instead of crushing messages into a handful of columns.
        let label_width = if inner.width > 12 { 8 } else { 0 };
        let text_width = usize::from(inner.width)
            .saturating_sub(gutter + label_width)
            .max(1);
        let mut rendered = Vec::new();
        for (entry, metadata) in self.messages.entries.iter().zip(metadata) {
            if !columns {
                rendered.push(MessageRow {
                    id: entry.id,
                    metadata: metadata.clone(),
                    label: String::new(),
                    level: entry.level,
                    source: None,
                    text: String::new(),
                });
            }
            for (i, range) in wrap(&entry.text, text_width).into_iter().enumerate() {
                rendered.push(MessageRow {
                    id: entry.id,
                    metadata: if columns && i == 0 {
                        metadata.clone()
                    } else {
                        String::new()
                    },
                    label: if i == 0 {
                        severity(entry.level).0.into()
                    } else {
                        String::new()
                    },
                    level: entry.level,
                    text: entry.text[range.clone()].to_owned(),
                    source: Some(range),
                });
            }
        }
        let header_height = u16::from(inner.height > 1);
        view.viewport = Rect::new(
            inner.x,
            inner.y + header_height,
            inner.width,
            inner.height.saturating_sub(header_height),
        );
        view.text_x = inner.x.saturating_add((gutter + label_width) as u16);
        view.rows = usize::from(view.viewport.height);
        view.scroll = view.scroll.min(rendered.len().saturating_sub(view.rows));
        view.last_draw = now;
        // An evicted endpoint must not silently turn a selection into a
        // different message. New entries and reflows otherwise preserve it.
        if view.selection.is_some_and(|s| {
            !self
                .messages
                .entries
                .iter()
                .any(|e| e.id == s.anchor.start.id)
                || !self
                    .messages
                    .entries
                    .iter()
                    .any(|e| e.id == s.focus.start.id)
        }) {
            view.selection = None;
        }
        if header_height > 0 {
            let header = if columns {
                format!("{:<metadata_width$} │ LEVEL   MESSAGE", "TIME (LOCAL)")
            } else {
                "TIME (LOCAL) / MESSAGE".into()
            };
            frame.render_widget(
                Paragraph::new(header).style(muted),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        }
        let selected = view.selection.and_then(Selection::bounds);
        let lines: Vec<Line> = rendered
            .iter()
            .skip(view.scroll)
            .take(view.rows)
            .map(|row| {
                let color = severity(row.level).1;
                let mut spans = Vec::new();
                if columns {
                    spans.push(Span::styled(
                        format!("{:<metadata_width$} │ ", row.metadata),
                        muted,
                    ));
                } else if row.source.is_none() {
                    return Line::from(Span::styled(row.metadata.clone(), muted));
                }
                if label_width > 0 {
                    spans.push(Span::styled(
                        format!("{:<label_width$}", row.label),
                        Style::default().fg(color).bold(),
                    ));
                }
                let style = Style::default().fg(color);
                if let Some(range) = &row.source {
                    for (byte, glyph) in row.text.grapheme_indices(true) {
                        let position = Position {
                            id: row.id,
                            byte: range.start + byte,
                        };
                        let highlight = selected
                            .is_some_and(|(start, end)| position >= start && position < end);
                        spans.push(Span::styled(
                            glyph.to_owned(),
                            if highlight {
                                style.add_modifier(Modifier::REVERSED)
                            } else {
                                style
                            },
                        ));
                    }
                }
                Line::from(spans)
            })
            .collect();
        if rendered.is_empty() {
            frame.render_widget(
                Paragraph::new("No messages yet").style(muted),
                view.viewport,
            );
        } else {
            frame.render_widget(Paragraph::new(lines), view.viewport);
        }
        view.rendered = rendered;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_preserve_severity_and_the_log_stays_bounded() {
        let mut log = MessageLog::default();
        log.push("host is unreachable", Level::Warning);
        log.push("host is unreachable", Level::Warning);
        assert_eq!(log.len(), 1);
        assert_eq!(log.iter().next().unwrap().repeats, 2);
        log.push("host is unreachable", Level::Error);
        assert_eq!(log.len(), 2);
        assert_eq!(log.iter().last().unwrap().level, Level::Error);
        for i in 0..MAX_ENTRIES + 10 {
            log.push(&format!("message {i}"), Level::Info);
        }
        assert_eq!(log.len(), MAX_ENTRIES);
        assert_eq!(log.iter().next().unwrap().text, "message 10");
    }

    #[test]
    fn timestamps_have_seconds_and_signed_local_offsets() {
        // SAFETY: every field of tm accepts zero; all displayed fields are set.
        let mut local: libc::tm = unsafe { std::mem::zeroed() };
        local.tm_year = 126;
        local.tm_mon = 9;
        local.tm_mday = 4;
        local.tm_hour = 15;
        local.tm_min = 4;
        local.tm_sec = 5;
        local.tm_gmtoff = -4 * 3600;
        assert_eq!(timestamp_from_local(&local), "2026-10-04T15:04:05-04:00");
        local.tm_gmtoff = 5 * 3600 + 45 * 60;
        assert_eq!(timestamp_from_local(&local), "2026-10-04T15:04:05+05:45");
        local.tm_gmtoff = 0;
        assert_eq!(timestamp_from_local(&local), "2026-10-04T15:04:05+00:00");
        let actual = local_timestamp(SystemTime::now());
        assert_eq!(actual.len(), 25);
        assert_eq!(
            &actual[19..20],
            if actual.contains('+') { "+" } else { "-" }
        );
    }

    #[test]
    fn age_refresh_waits_until_the_next_second() {
        let mut view = MessageLogView::at_bottom();
        assert!(!view.next_tick().is_zero());
        view.last_draw = Instant::now() - Duration::from_secs(2);
        assert!(view.next_tick().is_zero());
    }

    #[test]
    fn ages_use_one_whole_unit() {
        for (seconds, expected) in [
            (0, "(-0s)"),
            (59, "(-59s)"),
            (60, "(-1m)"),
            (3599, "(-59m)"),
            (3600, "(-1h)"),
            (8520, "(-2h)"),
            (86400, "(-1d)"),
        ] {
            assert_eq!(age(seconds), expected);
        }
    }

    #[test]
    fn wrapping_keeps_source_offsets_and_graphemes() {
        for (text, width, expected) in [
            ("one two three", 7, vec!["one two", "three"]),
            ("aaaaaaaa bb", 6, vec!["aaaaaa", "aa bb"]),
            ("  first\n\nsecond", 20, vec!["  first", "", "second"]),
            ("界界e\u{301}👩‍💻end", 4, vec!["界界", "e\u{301}👩‍💻e", "nd"]),
            ("", 10, vec![""]),
        ] {
            let lines: Vec<_> = wrap(text, width).into_iter().map(|r| &text[r]).collect();
            assert_eq!(lines, expected);
        }
    }

    #[test]
    fn selection_preserves_source_and_survives_eviction_of_other_entries() {
        let mut log = MessageLog::default();
        log.push("discarded", Level::Info);
        log.push("first  message\nsecond line", Level::Warning);
        log.push("last message", Level::Error);
        let selection = Selection {
            anchor: Hit {
                start: Position { id: 1, byte: 0 },
                end: Position { id: 1, byte: 1 },
            },
            focus: Hit {
                start: Position { id: 2, byte: 3 },
                end: Position { id: 2, byte: 4 },
            },
            moved: true,
            dragging: false,
        };
        assert_eq!(
            log.selected_text(selection),
            "first  message\nsecond line\nlast"
        );
        log.entries.pop_front();
        assert_eq!(
            log.selected_text(Selection {
                anchor: selection.focus,
                focus: selection.anchor,
                ..selection
            }),
            "first  message\nsecond line\nlast"
        );
        assert!(
            log.selected_text(Selection {
                moved: false,
                ..selection
            })
            .is_empty()
        );
    }
}
