//! The directory-mark popup (`Space i`): its state, its drawing and its keys.
//!
//! One file per modal, the shape `prefs.rs` and [`super::host_edit`] have. What
//! the popup edits — a workdir's icon and colour — is persisted by `App`, not
//! here; this is only the editor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::format::{DIR_COLORS, ICON_SLOT_WIDTH, centered_rect, clear_overlay};
use super::picker;
use super::picker::TextInputEvent;
use super::{Action, App};

/// Active state for the directory-mark popup editor. `Some` iff
/// `input_mode == InputMode::DirEdit`.
#[derive(Debug)]
pub(crate) struct DirEditState {
    pub(in crate::app) cwd: String,
    pub(in crate::app) color_idx: usize,
    pub(in crate::app) custom: picker::TextInput,
    pub(in crate::app) focus: DirEditFocus,
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// Which field of the directory-mark popup has the cursor.
pub(crate) enum DirEditFocus {
    Custom,
    Color,
}

// =============================================================================
// Drawing
// =============================================================================

impl App {
    pub(super) fn draw_dir_edit(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.dir_edit.as_ref() else {
            return;
        };
        // 35% height accounts for the 16-name color palette wrapping onto a
        // second visual line on narrow popups.
        let popup = centered_rect(80, 35, area);
        clear_overlay(frame, popup);

        let preview_color = DIR_COLORS[state.color_idx].1;
        let custom = state.custom.text();
        let preview_icon: String = if custom.trim().is_empty() {
            self.effective_dir_mark(&state.cwd).0
        } else {
            custom.to_string()
        };
        // Taken before the preview moves into the title: it is a property of the
        // icon the mark will actually wear, default included.
        let color_is_inert = super::format::icon_is_emoji(&preview_icon);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(vec![
                Span::styled(" Directory Mark  ", Style::default().bold()),
                Span::styled(
                    preview_icon,
                    Style::default()
                        .fg(preview_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let [path_area, custom_area, color_area, help_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .areas(inner);

        let path_display = self.shorten_path(&state.cwd).into_owned();
        let path_line = Line::from(vec![
            Span::styled("Path  ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(path_display),
        ]);
        frame.render_widget(Paragraph::new(path_line), path_area);

        let row_label = |focused: bool, label: &'static str| {
            Span::styled(
                if focused {
                    format!("\u{276F} {label}  ")
                } else {
                    format!("  {label}  ")
                },
                Style::default().add_modifier(Modifier::DIM),
            )
        };

        let custom_focus = state.focus == DirEditFocus::Custom;
        let custom_inner_color = if custom.trim().is_empty() {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(preview_color)
        };
        let inner_text = if custom.trim().is_empty() && !custom_focus {
            Span::styled(
                format!("(emoji or up to {ICON_SLOT_WIDTH} chars — empty = default)"),
                Style::default().add_modifier(Modifier::DIM),
            )
        } else {
            Span::styled(custom.to_string(), custom_inner_color)
        };
        let mut custom_spans = vec![
            row_label(custom_focus, "Icon "),
            Span::raw("[ "),
            inner_text,
        ];
        if custom_focus {
            custom_spans.push(Span::styled(
                "_",
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        }
        custom_spans.push(Span::raw(" ]"));
        // Advertise the emoji picker only while the icon field is focused,
        // since that's the only place Ctrl-E is bound.
        if custom_focus {
            custom_spans.push(Span::styled(
                "   ^E emoji picker",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(custom_spans)), custom_area);

        let color_focus = state.focus == DirEditFocus::Color;
        let mut color_spans: Vec<Span<'static>> = vec![row_label(color_focus, "Color")];
        // Said on the row it applies to, and only while it is true — switching
        // to a text icon is answered by the caveat going away. It rides the
        // label rather than taking a line of its own because this popup's
        // layout is already tight on a short terminal. The *default* mark is an
        // emoji too, so an untouched directory opens straight into this, which
        // is exactly when the colour keys would otherwise look broken.
        if color_is_inert {
            color_spans.push(Span::styled(
                "(no effect on emoji) ",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        for (i, (name, color)) in DIR_COLORS.iter().enumerate() {
            let mut style = Style::default().fg(*color);
            if i == state.color_idx {
                style = style.add_modifier(Modifier::REVERSED);
            }
            color_spans.push(Span::styled(format!(" {name} "), style));
        }
        frame.render_widget(
            Paragraph::new(Line::from(color_spans)).wrap(Wrap { trim: false }),
            color_area,
        );

        let help = Paragraph::new(vec![Line::from(Span::styled(
            "Tab/↑↓ switch row   ←→/h/l color   ^E emoji picker   Enter save   r reset   Esc cancel",
            Style::default().add_modifier(Modifier::DIM),
        ))]);
        frame.render_widget(help, help_area);
    }
}

// =============================================================================
// Keys
// =============================================================================

impl App {
    pub(super) fn handle_dir_edit_key(&mut self, key: KeyEvent) -> Option<Action> {
        // Esc and Enter are unconditional — even with the text row focused
        // they should close / commit, not get inserted as text.
        match key.code {
            KeyCode::Esc => {
                self.cancel_dir_edit();
                return None;
            }
            KeyCode::Enter => {
                self.commit_dir_edit();
                return None;
            }
            _ => {}
        }

        // `r` resets the override only when Color is focused, so a future
        // third focus mode is opt-in instead of inheriting the reset bind.
        let focus = self.dir_edit.as_ref()?.focus;
        if matches!(key.code, KeyCode::Char('r')) && focus == DirEditFocus::Color {
            self.reset_dir_edit();
            return None;
        }

        // Ctrl-E from the icon field opens the searchable emoji picker. The
        // field is at most a few cells, so shadowing readline's end-of-line
        // here costs nothing. Intercept before TextInput consumes it.
        if focus == DirEditFocus::Custom
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('e'))
        {
            self.open_emoji_picker();
            return None;
        }

        // Tab/↑/↓/^n/^p toggle focus. j/k are reserved for text input — binding
        // them here would let the user *enter* Custom but never *leave* it;
        // ^n/^p carry no such cost, since `TextInput` leaves them alone
        // precisely so a list around it can have them.
        let switches_row = matches!(
            key.code,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down
        ) || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n' | 'p')));
        let s = self.dir_edit.as_mut()?;
        if switches_row {
            s.focus = match s.focus {
                DirEditFocus::Custom => DirEditFocus::Color,
                DirEditFocus::Color => DirEditFocus::Custom,
            };
            return None;
        }

        match s.focus {
            DirEditFocus::Color => {
                let len = DIR_COLORS.len();
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        s.color_idx = if s.color_idx == 0 {
                            len - 1
                        } else {
                            s.color_idx - 1
                        };
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        s.color_idx = (s.color_idx + 1) % len;
                    }
                    _ => {}
                }
            }
            DirEditFocus::Custom => {
                // Post-hoc width cap (revert on overrun) instead of pre-check
                // so paste / multi-byte input still goes through TextInput's
                // normal handling first.
                let prev = s.custom.text().to_string();
                let evt = s.custom.handle_key(key);
                if matches!(evt, TextInputEvent::Changed) {
                    use unicode_width::UnicodeWidthStr;
                    if s.custom.text().width() > ICON_SLOT_WIDTH {
                        s.custom.set_text(prev);
                    }
                }
            }
        }
        None
    }

    // =============================================================================
    // Message log, help, confirm
    // =============================================================================
}
