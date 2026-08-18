use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};

use super::format::{centered_rect, truncate_str};

// -- TextInput: cursor-aware buffer with readline-style keybinds --

/// A single-line text input with a cursor and readline-style keybinds
/// (Ctrl-A/E/B/F/D/U/K/W, Alt-B/F/D, Home/End, arrow keys).
#[derive(Debug, Default, Clone)]
pub(crate) struct TextInput {
    buf: String,
    /// Byte offset into `buf`. Always at a char boundary.
    cursor: usize,
}

/// Outcome of feeding a key into `TextInput` — lets the caller notice
/// printable edits without a string diff, and catch Enter/Esc without
/// hardcoding char checks at every call site.
#[derive(Debug, PartialEq)]
pub(crate) enum TextInputEvent {
    /// Key didn't touch the buffer (e.g. an arrow pressed at the edge).
    Noop,
    /// Buffer changed.
    Changed,
    /// Cursor moved but buffer didn't.
    Moved,
    /// Key wasn't bound — passed through unhandled.
    Unhandled,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// A field pre-filled with `text`, cursor at the end — what a popup wants
    /// when it seeds a form from stored config.
    pub fn with_text(text: impl Into<String>) -> Self {
        let mut input = Self::new();
        input.set_text(text);
        input
    }

    pub fn text(&self) -> &str {
        &self.buf
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.buf = text.into();
        self.cursor = self.buf.len();
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    fn prev_char_boundary(&self, idx: usize) -> usize {
        if idx == 0 {
            return 0;
        }
        let mut i = idx - 1;
        while i > 0 && !self.buf.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next_char_boundary(&self, idx: usize) -> usize {
        if idx >= self.buf.len() {
            return self.buf.len();
        }
        let mut i = idx + 1;
        while i < self.buf.len() && !self.buf.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    /// Byte offset at the start of the word before `idx`. A "word" is a run of
    /// non-whitespace, non-`/` characters — matches readline's default word
    /// boundary behavior well enough for path editing.
    fn prev_word_boundary(&self, idx: usize) -> usize {
        let bytes = self.buf.as_bytes();
        let mut i = idx;
        // Skip trailing separators.
        while i > 0 {
            let prev = self.prev_char_boundary(i);
            let c = bytes[prev] as char;
            if c.is_whitespace() || c == '/' {
                i = prev;
            } else {
                break;
            }
        }
        // Skip the word body.
        while i > 0 {
            let prev = self.prev_char_boundary(i);
            let c = bytes[prev] as char;
            if c.is_whitespace() || c == '/' {
                break;
            }
            i = prev;
        }
        i
    }

    fn next_word_boundary(&self, idx: usize) -> usize {
        let bytes = self.buf.as_bytes();
        let len = self.buf.len();
        let mut i = idx;
        while i < len {
            let c = bytes[i] as char;
            if c.is_whitespace() || c == '/' {
                i = self.next_char_boundary(i);
            } else {
                break;
            }
        }
        while i < len {
            let c = bytes[i] as char;
            if c.is_whitespace() || c == '/' {
                break;
            }
            i = self.next_char_boundary(i);
        }
        i
    }

    fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = self.prev_char_boundary(self.cursor);
        self.buf.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let next = self.next_char_boundary(self.cursor);
        self.buf.replace_range(self.cursor..next, "");
        true
    }

    fn delete_word_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = self.prev_word_boundary(self.cursor);
        self.buf.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    fn delete_word_forward(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        let end = self.next_word_boundary(self.cursor);
        self.buf.replace_range(self.cursor..end, "");
        true
    }

    fn kill_to_start(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.buf.replace_range(0..self.cursor, "");
        self.cursor = 0;
        true
    }

    fn kill_to_end(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.buf.truncate(self.cursor);
        true
    }

    /// Feed a key. Returns what happened so callers can e.g. reset the item
    /// cursor on `Changed` but leave it alone on `Moved`.
    pub fn handle_key(&mut self, key: KeyEvent) -> TextInputEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Readline-ish control bindings. Ctrl-N/P are reserved by Picker for
        // item navigation and handled earlier.
        if ctrl {
            match key.code {
                KeyCode::Char('a') => {
                    if self.cursor == 0 {
                        return TextInputEvent::Noop;
                    }
                    self.cursor = 0;
                    return TextInputEvent::Moved;
                }
                KeyCode::Char('e') => {
                    if self.cursor == self.buf.len() {
                        return TextInputEvent::Noop;
                    }
                    self.cursor = self.buf.len();
                    return TextInputEvent::Moved;
                }
                KeyCode::Char('b') => {
                    if self.cursor == 0 {
                        return TextInputEvent::Noop;
                    }
                    self.cursor = self.prev_char_boundary(self.cursor);
                    return TextInputEvent::Moved;
                }
                KeyCode::Char('f') => {
                    if self.cursor >= self.buf.len() {
                        return TextInputEvent::Noop;
                    }
                    self.cursor = self.next_char_boundary(self.cursor);
                    return TextInputEvent::Moved;
                }
                KeyCode::Char('d') => {
                    return if self.delete_forward() {
                        TextInputEvent::Changed
                    } else {
                        TextInputEvent::Noop
                    };
                }
                KeyCode::Char('h') => {
                    return if self.backspace() {
                        TextInputEvent::Changed
                    } else {
                        TextInputEvent::Noop
                    };
                }
                KeyCode::Char('w') => {
                    return if self.delete_word_back() {
                        TextInputEvent::Changed
                    } else {
                        TextInputEvent::Noop
                    };
                }
                KeyCode::Char('u') => {
                    return if self.kill_to_start() {
                        TextInputEvent::Changed
                    } else {
                        TextInputEvent::Noop
                    };
                }
                KeyCode::Char('k') => {
                    return if self.kill_to_end() {
                        TextInputEvent::Changed
                    } else {
                        TextInputEvent::Noop
                    };
                }
                _ => {}
            }
        }

        if alt {
            match key.code {
                KeyCode::Char('b') => {
                    let new = self.prev_word_boundary(self.cursor);
                    if new == self.cursor {
                        return TextInputEvent::Noop;
                    }
                    self.cursor = new;
                    return TextInputEvent::Moved;
                }
                KeyCode::Char('f') => {
                    let new = self.next_word_boundary(self.cursor);
                    if new == self.cursor {
                        return TextInputEvent::Noop;
                    }
                    self.cursor = new;
                    return TextInputEvent::Moved;
                }
                KeyCode::Char('d') => {
                    return if self.delete_word_forward() {
                        TextInputEvent::Changed
                    } else {
                        TextInputEvent::Noop
                    };
                }
                KeyCode::Backspace => {
                    return if self.delete_word_back() {
                        TextInputEvent::Changed
                    } else {
                        TextInputEvent::Noop
                    };
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Left => {
                if self.cursor == 0 {
                    return TextInputEvent::Noop;
                }
                self.cursor = self.prev_char_boundary(self.cursor);
                TextInputEvent::Moved
            }
            KeyCode::Right => {
                if self.cursor >= self.buf.len() {
                    return TextInputEvent::Noop;
                }
                self.cursor = self.next_char_boundary(self.cursor);
                TextInputEvent::Moved
            }
            KeyCode::Home => {
                if self.cursor == 0 {
                    return TextInputEvent::Noop;
                }
                self.cursor = 0;
                TextInputEvent::Moved
            }
            KeyCode::End => {
                if self.cursor == self.buf.len() {
                    return TextInputEvent::Noop;
                }
                self.cursor = self.buf.len();
                TextInputEvent::Moved
            }
            KeyCode::Backspace => {
                if self.backspace() {
                    TextInputEvent::Changed
                } else {
                    TextInputEvent::Noop
                }
            }
            KeyCode::Delete => {
                if self.delete_forward() {
                    TextInputEvent::Changed
                } else {
                    TextInputEvent::Noop
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                TextInputEvent::Changed
            }
            _ => TextInputEvent::Unhandled,
        }
    }
}

// -- PickerItem / Picker --

#[derive(Debug, Clone)]
pub(in crate::app) struct PickerItem {
    pub primary: String,
    pub secondary: Option<String>,
    pub filter_text: String,
    /// Opaque payload the caller can stash — for the workdir picker we put
    /// the raw (un-tilde-collapsed) path here so submission yields the exact
    /// string to launch.
    pub payload: Option<String>,
    /// Colored glyph rendered before the primary line. Reserved-width across
    /// items: rows without a prefix get blank padding so titles stay aligned.
    pub prefix: Option<(String, Color)>,
}

impl PickerItem {
    pub fn new(primary: impl Into<String>) -> Self {
        let p = primary.into();
        let filter_text = p.to_lowercase();
        Self {
            primary: p,
            secondary: None,
            filter_text,
            payload: None,
            prefix: None,
        }
    }

    /// Set the secondary (meta) line shown beneath the primary. Display-only —
    /// it does *not* fold into `filter_text`. Filter text is set exactly once
    /// (the `new` default of the lowercased primary, or an explicit
    /// `with_filter_text`), so builder call order can't silently pollute the
    /// filter.
    pub fn with_secondary(mut self, s: impl Into<String>) -> Self {
        self.secondary = Some(s.into());
        self
    }

    pub fn with_filter_text(mut self, t: impl Into<String>) -> Self {
        self.filter_text = t.into().to_lowercase();
        self
    }

    pub fn with_payload(mut self, p: impl Into<String>) -> Self {
        self.payload = Some(p.into());
        self
    }

    pub fn with_prefix(mut self, icon: impl Into<String>, color: Color) -> Self {
        self.prefix = Some((icon.into(), color));
        self
    }
}

#[derive(Debug)]
pub(in crate::app) enum PickerEvent {
    Noop,
    Cancel,
    /// User picked the item at the given index (into `self.items`).
    Submit(usize),
    /// Free-input mode: user pressed Enter with no matching item selected, so
    /// the raw filter text is the accepted value. The consumer re-reads that
    /// text off the picker (`input.text()`), so the variant carries no payload.
    /// Only emitted when `free_input` is enabled.
    SubmitFree,
    /// Tab pressed. The caller should run custom completion logic, then call
    /// `Picker::set_text` with the completed text. Only emitted when
    /// `handles_tab` is enabled.
    TabComplete,
}

/// Telescope-style filterable picker popup: a search line on top, a list of
/// `PickerItem`s below. Owns input state and key handling; the caller
/// interprets submit events against its own item list.
#[derive(Debug)]
pub(in crate::app) struct Picker {
    pub title: String,
    pub placeholder: String,
    pub input: TextInput,
    pub cursor: usize,
    pub items: Vec<PickerItem>,
    pub size_percent: (u16, u16),
    /// True once the user has explicitly moved the cursor (Up/Down/Ctrl-N/P).
    /// Reset whenever the filter text changes. In free-input mode the caller
    /// uses this to let explicit navigation always win over the typed text (see
    /// `App::submit_workdir`).
    pub user_selected: bool,
    /// If true, Enter reports the highlighted item via `Submit` (so the caller
    /// can decide between it and the typed text) and falls back to `SubmitFree`
    /// with the raw input only when nothing matches the filter. Used by the
    /// workdir picker.
    pub free_input: bool,
    /// If true, Tab emits `PickerEvent::TabComplete` instead of being ignored.
    pub handles_tab: bool,
    /// Transient error shown under the search line (e.g. "not a directory").
    /// Cleared on the next edit. Set by the caller via `set_error`.
    pub error: Option<String>,
    /// A status line pinned to the **top of the popup**, above the search
    /// input, on a lifted background so it reads as chrome rather than as
    /// another list row.
    ///
    /// This is where a picker's live *settings* live — the agent and host a new
    /// session will open on, the host a resume list is scoped to. They used to
    /// ride the dashboard's footer ribbon beside the key hints, which put a
    /// changing value inside a strip of fixed labels: the eye had to leave the
    /// popup to find what the popup was about, and `Ctrl-t` appeared to do
    /// nothing until you looked away from it. The dashboard's bar keeps only
    /// the static hints now.
    ///
    /// It sits at the *top* rather than the bottom because on a tall popup the
    /// two things that decide what Enter does — the settings and the path being
    /// typed — were a whole screen apart, with an empty list between them. The
    /// title, the settings and the input now read as one block.
    pub status_bar: Option<Line<'static>>,
    /// The items are still being fetched (a remote `ListResumable` round trip).
    /// Only changes the empty-list message — the picker is fully interactive
    /// meanwhile, so a slow host can't hold the UI.
    pub loading: bool,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self {
            title: title.into(),
            placeholder: "Search…".to_string(),
            input: TextInput::new(),
            cursor: 0,
            items,
            size_percent: (70, 70),
            user_selected: false,
            free_input: false,
            handles_tab: false,
            error: None,
            status_bar: None,
            loading: false,
        }
    }

    pub fn with_placeholder(mut self, p: impl Into<String>) -> Self {
        self.placeholder = p.into();
        self
    }

    pub fn with_size(mut self, w: u16, h: u16) -> Self {
        self.size_percent = (w, h);
        self
    }

    pub fn with_free_input(mut self, v: bool) -> Self {
        self.free_input = v;
        self
    }

    pub fn with_tab_completion(mut self, v: bool) -> Self {
        self.handles_tab = v;
        self
    }

    pub fn set_text(&mut self, t: impl Into<String>) {
        self.input.set_text(t);
        self.cursor = 0;
        self.user_selected = false;
        self.error = None;
    }

    /// Show a transient error under the search line. Cleared on the next edit.
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
    }

    /// Indices (into `self.items`) that pass the current filter, in order.
    pub fn filtered(&self) -> Vec<usize> {
        if self.input.is_empty() {
            return (0..self.items.len()).collect();
        }
        let q = self.input.text().to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.filter_text.contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    /// Advance the item cursor by one, wrapping to the top. `total` is the
    /// filtered item count. Marks the selection as user-driven.
    fn move_cursor_down(&mut self, total: usize) {
        if total > 0 {
            self.cursor = (self.cursor + 1) % total;
            self.user_selected = true;
        }
    }

    /// Move the item cursor up by one, wrapping to the bottom.
    fn move_cursor_up(&mut self, total: usize) {
        if total > 0 {
            self.cursor = if self.cursor == 0 {
                total - 1
            } else {
                self.cursor - 1
            };
            self.user_selected = true;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerEvent {
        let filtered = self.filtered();
        let total = filtered.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Item-list navigation. Ctrl-N/P/J/K are intercepted before the
        // TextInput gets them so they can't conflict with readline Ctrl-K.
        if ctrl {
            match key.code {
                KeyCode::Char('n') => {
                    self.move_cursor_down(total);
                    return PickerEvent::Noop;
                }
                KeyCode::Char('p') => {
                    self.move_cursor_up(total);
                    return PickerEvent::Noop;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                if self.input.is_empty() {
                    return PickerEvent::Cancel;
                }
                self.input.clear();
                self.cursor = 0;
                self.user_selected = false;
                self.error = None;
                return PickerEvent::Noop;
            }
            KeyCode::Down => {
                self.move_cursor_down(total);
                return PickerEvent::Noop;
            }
            KeyCode::Up => {
                self.move_cursor_up(total);
                return PickerEvent::Noop;
            }
            KeyCode::Enter => {
                // A highlighted item is reported via `Submit`. Clamp the cursor
                // to the filtered range exactly as `draw` does, so the
                // highlighted row and the submitted item can never disagree even
                // if a future item-mutation path forgets to fix up the cursor.
                // In free-input mode the caller decides between that item and the
                // typed text (it can stat the filesystem, which the picker
                // can't); the picker only falls back to `SubmitFree` with the raw
                // text when the filtered list is empty.
                if !filtered.is_empty() {
                    return PickerEvent::Submit(filtered[self.cursor.min(filtered.len() - 1)]);
                }
                if self.free_input {
                    return PickerEvent::SubmitFree;
                }
                return PickerEvent::Noop;
            }
            KeyCode::Tab if self.handles_tab => {
                return PickerEvent::TabComplete;
            }
            _ => {}
        }

        match self.input.handle_key(key) {
            TextInputEvent::Changed => {
                self.cursor = 0;
                self.user_selected = false;
                self.error = None;
                PickerEvent::Noop
            }
            TextInputEvent::Moved | TextInputEvent::Noop | TextInputEvent::Unhandled => {
                PickerEvent::Noop
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let (w, h) = self.size_percent;
        let popup = centered_rect(w, h, area);
        frame.render_widget(Clear, popup);

        let filtered = self.filtered();
        let total = self.items.len();
        let visible = filtered.len();
        let sel_display = if visible == 0 {
            0
        } else {
            self.cursor.min(visible - 1) + 1
        };

        // Position is within the filtered list, so the denominator is `visible`.
        // When a filter is hiding items, also surface the unfiltered total.
        let counter = if visible == total {
            format!("{sel_display} of {visible}")
        } else {
            format!("{sel_display} of {visible} ({total} total)")
        };
        let title = format!(" {} ({counter}) ", self.title);
        let block = Block::default().borders(Borders::ALL).title(title);
        let mut inner = block.inner(popup);
        frame.render_widget(block, popup);

        // Carve the status line off the top before the list is laid out, so it
        // never overlaps a row. Skipped on a popup too short to spare the row —
        // the list is the point.
        let status_area = match &self.status_bar {
            Some(_) if inner.height >= 4 => {
                let area = Rect { height: 1, ..inner };
                inner.y += 1;
                inner.height -= 1;
                Some(area)
            }
            _ => None,
        };
        // Painted now rather than after the list, so the `visible == 0` early
        // return below can't drop it.
        // No background of its own: the line reads as part of the popup, not as
        // a selected row, so it inherits whatever the popup sits on.
        if let (Some(area), Some(line)) = (status_area, &self.status_bar) {
            frame.render_widget(Paragraph::new(line.clone()), area);
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);

        let prompt_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let search_line = if self.input.is_empty() {
            Line::from(vec![
                Span::styled("/ ", prompt_style),
                Span::styled(self.placeholder.clone(), dim),
            ])
        } else {
            let text = self.input.text();
            let cursor = self.input.cursor();
            let before: String = text[..cursor].to_string();
            let at: String = text[cursor..]
                .chars()
                .next()
                .map(|c| c.to_string())
                .unwrap_or_default();
            let after: String = if at.is_empty() {
                String::new()
            } else {
                text[cursor + at.len()..].to_string()
            };
            // Render the cursor with `Modifier::REVERSED` so the terminal's own
            // theme picks the fg/bg combo — hardcoding colors breaks on themes
            // where e.g. `white` maps to a near-black shade.
            let reversed = Style::default().add_modifier(Modifier::REVERSED);
            let cursor_span = if at.is_empty() {
                Span::styled(" ", reversed)
            } else {
                Span::styled(at, reversed)
            };
            let mut spans = vec![
                Span::styled("/ ", prompt_style),
                Span::raw(before),
                cursor_span,
                Span::raw(after),
            ];
            if !self.items.is_empty() {
                spans.push(Span::styled(format!("   {visible}/{total}"), dim));
            }
            Line::from(spans)
        };
        frame.render_widget(Paragraph::new(search_line), chunks[0]);

        // Spacer line (chunks[1]) doubles as the error slot.
        if let Some(err) = &self.error {
            let err_style = Style::default().fg(crate::config::get().colors.ui.error_fg);
            frame.render_widget(
                Paragraph::new(Span::styled(format!("⚠ {err}"), err_style)),
                chunks[1],
            );
        }

        let list_area = chunks[2];
        if visible == 0 {
            let msg = if self.loading {
                "Loading…"
            } else if total == 0 {
                if self.free_input {
                    "Type a path and press Enter."
                } else {
                    "No items available."
                }
            } else {
                "No items match filter."
            };
            frame.render_widget(Paragraph::new(Span::styled(msg, dim)), list_area);
            return;
        }

        // Two-line rows when any item carries secondary meta, otherwise single-line rows.
        let has_secondary = self.items.iter().any(|i| i.secondary.is_some());
        let row_h: u16 = if has_secondary { 2 } else { 1 };
        let max_rows = ((list_area.height as usize) / row_h as usize).max(1);
        let sel = self.cursor.min(visible - 1);
        let start = if sel >= max_rows {
            sel + 1 - max_rows
        } else {
            0
        };
        let end = (start + max_rows).min(visible);

        let title_width = (list_area.width as usize).saturating_sub(4);
        // Reserve a constant prefix slot wide enough for the widest icon plus
        // a trailing space; rows without a prefix render blanks of the same
        // width so titles line up. Zero when no item carries a prefix.
        use unicode_width::UnicodeWidthStr;
        let max_prefix_width = self
            .items
            .iter()
            .filter_map(|i| i.prefix.as_ref().map(|(s, _)| s.as_str().width()))
            .max()
            .unwrap_or(0);
        let prefix_slot = if max_prefix_width > 0 {
            max_prefix_width + 1
        } else {
            0
        };
        // Subtle highlight for the selected row: a bg lift (which renders as
        // "slightly brighter than default" on dark themes and "slightly darker
        // than default" on light ones) plus a chevron on the left. No REVERSED
        // / BOLD so it stays calm.
        let picker = &crate::config::get().colors.picker;
        let sel_row_style = Style::default().bg(picker.highlight_bg);
        let chevron_style = Style::default()
            .fg(picker.chevron_fg)
            .bg(picker.highlight_bg);
        let mut rows: Vec<Row> = Vec::with_capacity(end - start);
        for (i, &idx) in filtered[start..end].iter().enumerate() {
            let i = i + start;
            let item = &self.items[idx];
            let is_sel = i == sel;
            let title_text = truncate_str(
                item.primary.trim(),
                title_width.saturating_sub(2 + prefix_slot),
            );
            let mut spans: Vec<Span> = Vec::with_capacity(4);
            if is_sel {
                spans.push(Span::styled("\u{276F} ", chevron_style));
            } else {
                spans.push(Span::raw("  "));
            }
            if prefix_slot > 0 {
                if let Some((icon, color)) = &item.prefix {
                    let pad = " ".repeat(prefix_slot - icon.as_str().width());
                    spans.push(Span::styled(icon.clone(), Style::default().fg(*color)));
                    spans.push(Span::raw(pad));
                } else {
                    spans.push(Span::raw(" ".repeat(prefix_slot)));
                }
            }
            spans.push(Span::raw(title_text));
            let title_line = Line::from(spans);

            let mut lines = vec![title_line];
            if let Some(meta) = &item.secondary {
                let meta = truncate_str(meta, title_width.saturating_sub(prefix_slot));
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::raw(" ".repeat(prefix_slot)),
                    Span::styled(meta, dim),
                ]));
            } else if has_secondary {
                lines.push(Line::from(""));
            }
            let row = Row::new(vec![Cell::from(lines)]).height(row_h);
            rows.push(if is_sel {
                row.style(sel_row_style)
            } else {
                row
            });
        }

        let table = Table::new(rows, [Constraint::Min(1)]);
        frame.render_widget(table, list_area);
    }
}
