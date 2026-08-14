use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Padding, Paragraph, Row, Table,
        Wrap,
    },
};

use crate::backend::{ConnState, VitalsView};
use crate::config;
use crate::state::{HostId, LauncherState, SessionStatus};

use super::format::{
    DIR_COLORS, DIR_ICON_MAX_CHARS, ELAPSED_MAX_WIDTH, ansi_to_lines, bar_segments, bar_style,
    centered_rect, context_pressure_style, dir_icon_width, elapsed_cell, fade_style,
    format_elapsed, format_tokens, hint_badge, hint_pair, model_color, model_label,
    override_indicator_cell, pill, session_display_name, truncate_str,
};
use super::keymap::Command;
use super::picker::TextInput;
use super::{App, DirEditFocus, HostField, HostTally, InputMode, PickerKind};

impl App {
    pub(super) fn draw(&mut self, frame: &mut ratatui::Frame) {
        let footer_h: u16 = 1;
        // The header is a borderless single-row zellij-style ribbon plus one
        // blank padding row beneath it, so it sits clear of the Sessions panel
        // title below (which the ribbon's first pill lines up with).
        let [header, body, footer_rect] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(footer_h),
        ])
        .areas(frame.area());

        // At or below `narrow_max_width` the side-by-side layout can't breathe,
        // so we stack the panels vertically (session list → detail → preview)
        // with a trimmed table and a compact detail panel instead.
        let panels = &config::get().ui.panels;
        let narrow = body.width <= panels.narrow_max_width;

        // One-shot auto-hide: on the first draw we pick defaults based on the
        // initial viewport. After that, the user's toggle is the only source
        // of truth — viewport changes never flip visibility.
        if !self.panels_initialized {
            self.preview_visible = body.height >= panels.preview_auto_min_height;
            // In the vertical stack the detail panel doesn't steal width from
            // the table, so it defaults visible regardless of the wide-mode
            // width gate.
            self.detail_visible = narrow || frame.area().width >= panels.detail_auto_min_width;
            self.panels_initialized = true;
        }
        self.narrow_layout = narrow;

        self.draw_header(frame, header);
        self.draw_footer(frame, footer_rect);

        if narrow {
            self.draw_narrow_body(frame, body);
        } else {
            self.draw_wide_body(frame, body);
        }

        if self.input_mode == InputMode::Picker
            && let Some(active) = &self.picker
        {
            active.picker.draw(frame, body);
        }
        if self.input_mode == InputMode::Help {
            self.draw_help(frame, frame.area());
        }
        if self.input_mode == InputMode::Confirm {
            self.draw_confirm(frame, frame.area());
        }
        if self.input_mode == InputMode::DirEdit {
            self.draw_dir_edit(frame, frame.area());
        }
        if self.input_mode == InputMode::HostEdit {
            self.draw_host_edit(frame, frame.area());
        }
        // Last, and independent of `input_mode`: an attach freezes the loop for
        // its whole round trip, so this is the only feedback the keypress gets
        // until the window comes up.
        if self.attaching.is_some() {
            self.draw_attaching(frame, frame.area());
        }
    }

    /// The "Attaching…" overlay shown while an attach is in flight.
    ///
    /// `Enter` on a detached row plans the attach, spawns a window and waits on
    /// the terminal backend — all inline in the run loop, so nothing repaints
    /// until it returns. Without this the keypress looked ignored and the user
    /// pressed `Enter` again (§9). Painted by the pre-action frame, cleared when
    /// the attach returns.
    fn draw_attaching(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(session) = self.attaching.as_deref() else {
            return;
        };
        // Sized to the message, not to a percentage of the viewport: it is two
        // short lines, so a proportional box would be mostly empty on a tall
        // terminal and clip its own content on a short one.
        let title = format!("Attaching to {session}…");
        let width = (title.chars().count() as u16 + 6).clamp(20, area.width.max(20));
        let height: u16 = 5;
        if area.width < width || area.height < height {
            return;
        }
        let popup = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, popup);
        let ui = &config::get().colors.ui;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ui.title_fg))
            .title(Span::styled(" Attaching ", Style::default().bold()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(title, Style::default().fg(ui.title_fg).bold())),
            Line::from(Span::styled(
                "opening the session window",
                Style::default().add_modifier(Modifier::DIM),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    /// The default wide layout: an optional preview panel across the bottom, and
    /// the detail panel occupying the right column of the remaining top area.
    fn draw_wide_body(&mut self, frame: &mut ratatui::Frame, body: Rect) {
        let (top_area, preview_area) = if self.preview_visible {
            // Preview chrome eats 4 rows: top border, top padding, bottom
            // padding, bottom border. We need at least one content row on top.
            let min_preview: u16 = 6;
            let max_preview = body.height.saturating_sub(5).max(min_preview);
            if self.preview_height == 0 {
                self.preview_height = body.height / 2;
            }
            self.preview_height = self.preview_height.clamp(min_preview, max_preview);
            let table_h = body.height.saturating_sub(self.preview_height);
            let [top, prev] = Layout::vertical([
                Constraint::Length(table_h),
                Constraint::Length(self.preview_height),
            ])
            .areas(body);
            (top, Some(prev))
        } else {
            self.last_preview_rect = None;
            (body, None)
        };

        // Detail occupies the right column of top_area when visible.
        if self.detail_visible {
            let min_detail: u16 = 20;
            let min_table: u16 = 30;
            let max_detail = top_area.width.saturating_sub(min_table).max(min_detail);
            self.detail_width = self.detail_width.clamp(min_detail, max_detail);
            let [table_area, detail_area] =
                Layout::horizontal([Constraint::Min(30), Constraint::Length(self.detail_width)])
                    .areas(top_area);
            self.draw_table(frame, table_area, false);
            self.draw_detail(frame, detail_area, false);
            // Search mode focuses the eye on the session names: dim the whole
            // detail panel alongside the table's non-name columns.
            if self.input_mode == InputMode::Search {
                frame
                    .buffer_mut()
                    .set_style(detail_area, Style::default().add_modifier(Modifier::DIM));
            }
        } else {
            self.last_detail_rect = None;
            self.draw_table(frame, top_area, false);
        }

        if let Some(prev) = preview_area {
            self.draw_preview(frame, prev);
            if self.input_mode == InputMode::Search {
                frame
                    .buffer_mut()
                    .set_style(prev, Style::default().add_modifier(Modifier::DIM));
            }
        }
    }

    /// The narrow layout: session list, detail, and preview stacked vertically.
    /// The detail panel is a fixed, compact height; the preview takes whatever's
    /// left and is dropped entirely when the viewport is too short to spare it.
    fn draw_narrow_body(&mut self, frame: &mut ratatui::Frame, body: Rect) {
        // Compact detail: border (2 rows) + the four fields it shows.
        const DETAIL_H: u16 = 6;
        // Keep the session list usable before spending rows on the preview.
        const TABLE_MIN: u16 = 6;
        // Preview chrome (top/bottom border + vertical padding = 4) + 2 content rows.
        const PREVIEW_MIN: u16 = 6;

        let detail_h = if self.detail_visible {
            DETAIL_H.min(body.height)
        } else {
            0
        };
        let rest = body.height.saturating_sub(detail_h);
        // Preview is dynamic-height and simply disappears when the viewport
        // can't spare room for both a usable table and a usable preview.
        let show_preview = self.preview_visible && rest >= TABLE_MIN + PREVIEW_MIN;
        let preview_h = if show_preview {
            let want = if self.preview_height == 0 {
                rest / 2
            } else {
                self.preview_height
            };
            want.clamp(PREVIEW_MIN, rest.saturating_sub(TABLE_MIN))
        } else {
            0
        };

        let [table_area, detail_area, preview_area] = Layout::vertical([
            Constraint::Min(TABLE_MIN),
            Constraint::Length(detail_h),
            Constraint::Length(preview_h),
        ])
        .areas(body);

        self.draw_table(frame, table_area, true);

        if detail_h > 0 {
            self.draw_detail(frame, detail_area, true);
            if self.input_mode == InputMode::Search {
                frame
                    .buffer_mut()
                    .set_style(detail_area, Style::default().add_modifier(Modifier::DIM));
            }
        } else {
            self.last_detail_rect = None;
        }

        if preview_h > 0 {
            self.draw_preview(frame, preview_area);
            if self.input_mode == InputMode::Search {
                frame
                    .buffer_mut()
                    .set_style(preview_area, Style::default().add_modifier(Modifier::DIM));
            }
        } else {
            self.last_preview_rect = None;
        }
    }

    fn draw_dir_edit(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.dir_edit.as_ref() else {
            return;
        };
        // 35% height accounts for the 16-name color palette wrapping onto a
        // second visual line on narrow popups.
        let popup = centered_rect(80, 35, area);
        frame.render_widget(Clear, popup);

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
                format!("(emoji or up to {DIR_ICON_MAX_CHARS} chars — empty = default)"),
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

    /// The live status spans for one host row in the panel: connection state
    /// (green when connected, the `Failed` reason verbatim when there is one),
    /// running/attached session counts, the daemon version from `Welcome`, and
    /// the opportunistic latency sample. A host that isn't connected yet — or
    /// isn't in the backend set at all (a row the user is still typing) — shows
    /// only what's known.
    fn host_status_spans(&self, host: &HostId, max_width: usize) -> Vec<Span<'static>> {
        let ui = &config::get().colors.ui;
        let Some(backend) = self.backend_for(host) else {
            return vec![Span::styled(
                "not connected".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            )];
        };
        let state = backend.conn_state();
        let style = match &state {
            ConnState::Connected => Style::default().fg(Color::Green),
            ConnState::Connecting => Style::default().add_modifier(Modifier::DIM),
            ConnState::Disconnected | ConnState::Failed(_) => Style::default().fg(ui.attention_fg),
        };
        let mut spans = vec![Span::styled(one_line(state.label(), max_width), style)];
        if state.is_connected() {
            let (running, attached) = self.host_session_counts(host);
            spans.push(Span::styled(
                format!(
                    "  {running} {}, {attached} attached",
                    super::plural_sessions(running)
                ),
                Style::default().add_modifier(Modifier::DIM),
            ));
            // The trailer is one dim run of annotations, accumulated as a string
            // and pushed as a span — except where a failed probe has to carry
            // its own colour, which closes the run early and opens another.
            let dim = Style::default().add_modifier(Modifier::DIM);
            let mut trailer = String::new();
            if let Some(v) = backend.daemon_version() {
                trailer.push_str(&format!("  v{v}"));
                match backend.upgrade_offer() {
                    // A restart here would genuinely land on something else, so
                    // name what — the offer is only worth reading if it says
                    // where it goes.
                    Some(o) => trailer.push_str(&format!(" \u{2191}{}", o.version)),
                    // The cost of preferring a host's own server on protocol
                    // compatibility rather than version equality: a stale one
                    // outlives our upgrades silently, and the digest marker that
                    // refreshes the *cache* path never applies to a PATH install.
                    // Stated here rather than left to be discovered — but as an
                    // annotation, since it usually works fine, and *without* an
                    // upgrade arrow, because there is nothing we could deploy
                    // that this host would then choose.
                    None if super::format::version_is_older(&v, env!("CARGO_PKG_VERSION")) => {
                        trailer.push_str(" (older than ours)");
                    }
                    None => {}
                }
            }
            // What the host says about itself, beside what the link says about
            // it: utilisation answers "does this box have room for another
            // session?", which is the other half of the question the latency
            // starts. Percentages rather than absolutes because the row is a
            // scannable line, not a monitor — `l` is where detail goes.
            //
            // All three numbers stand or fall together, and none of them is ever
            // a held one (see [`VitalsView`]): they arrive with a reading — the
            // poll refreshes the latency sample on its way through — and until
            // one does, a spinner sits in their place. A row that has none
            // coming (a local backend, a host that isn't connected) shows
            // neither, spinner included.
            match backend.vitals() {
                Some(VitalsView::Reading(v)) => {
                    if let Some(cpu) = v.cpu_percent {
                        trailer.push_str(&format!("  cpu {cpu:.0}%"));
                    }
                    if let Some(mem) = v.mem_percent() {
                        trailer.push_str(&format!("  mem {mem:.0}%"));
                    }
                    // Labelled, unlike the bare `12ms` it used to be: three
                    // numbers in a row need saying which is which, and a
                    // duration on its own beside two percentages reads as
                    // whatever the eye guesses.
                    if let Some(rtt) = backend.latency() {
                        trailer.push_str(&format!("  latency {}ms", rtt.as_millis()));
                    }
                }
                // A frame of the spinner, which the run loop keeps turning. The
                // wait is a round trip, so what this really says is "asked" —
                // and on a host that has stopped answering it turns until the
                // poll's deadline hands it to the arm below.
                Some(VitalsView::Loading) => {
                    trailer.push_str(&format!("  {}", vitals_spinner_glyph()));
                }
                // Said rather than left blank, and in the attention colour: the
                // host is connected and everything else about it is on the row,
                // so numbers quietly missing reads as "nothing worth mentioning"
                // rather than "we asked and got nothing back". The one span here
                // that isn't dim, which is why the run is closed early.
                Some(VitalsView::Unavailable) => {
                    if !trailer.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut trailer), dim));
                    }
                    spans.push(Span::styled(
                        "  cpu/mem unavailable".to_string(),
                        Style::default().fg(ui.attention_fg),
                    ));
                }
                None => {}
            }
            if !trailer.is_empty() {
                spans.push(Span::styled(trailer, dim));
            }
        }
        spans
    }

    /// The hosts popup: the host list, or — while `l` is open — one host's
    /// connection log in its place.
    fn draw_host_edit(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        if state.log_view.is_some() {
            self.draw_host_log(frame, area);
        } else {
            self.draw_host_list(frame, area);
        }
    }

    /// One host's connection narrative, oldest first — everything the panel row
    /// had to cut, plus the steps that led to it.
    ///
    /// Takes `&mut self` only to record the viewport height, which `G` and the
    /// page keys need and which nothing but a render knows.
    fn draw_host_log(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(view) = self.host_edit.as_ref().and_then(|s| s.log_view.as_ref()) else {
            return;
        };
        let host = view.host.clone();
        let scroll = view.scroll;
        // Wider and taller than the list: these lines are quoted host output,
        // and wrapping a loader error at 72 cells helps nobody.
        let popup = centered_rect(88, 76, area);
        frame.render_widget(Clear, popup);
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            format!(" {host} \u{00b7} connection log ", host = host.0),
            Style::default().bold(),
        ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let lines = self.host_log_lines(&host);
        let rows = inner.height as usize;
        let ui = &config::get().colors.ui;
        let rendered: Vec<Line> = if lines.is_empty() {
            vec![Line::from(Span::styled(
                // Two ways to get here, and they aren't the same thing.
                if self.backend_for(&host).is_some() {
                    "(nothing logged yet)"
                } else {
                    "(this host isn't connected — add or edit it first)"
                },
                Style::default().add_modifier(Modifier::DIM),
            ))]
        } else {
            lines
                .iter()
                .skip(scroll)
                .take(rows)
                .map(|l| {
                    // The age column is fixed-width so the text starts on one
                    // margin; a continuation line pays the same indent and so
                    // reads as part of the entry above it.
                    let age = Span::styled(
                        format!("{:>5} ", l.age.as_deref().unwrap_or("")),
                        Style::default().add_modifier(Modifier::DIM),
                    );
                    let style = if l.error {
                        Style::default().fg(ui.attention_fg)
                    } else {
                        Style::default()
                    };
                    Line::from(vec![age, Span::styled(l.text.clone(), style)])
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(rendered), inner);

        // Record what the keys need, and re-clamp: the log grows underneath a
        // parked scroll offset, and the popup resizes with the terminal.
        if let Some(view) = self.host_edit.as_mut().and_then(|s| s.log_view.as_mut()) {
            view.rows = rows;
            view.scroll = view.scroll.min(lines.len().saturating_sub(rows));
        }
    }

    fn draw_host_list(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        let popup = centered_rect(72, 60, area);
        frame.render_widget(Clear, popup);
        // No key hints on the border: the footer bar already renders this
        // mode's bindings, and two copies of the same list disagree eventually.
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" Hosts ", Style::default().bold()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        // Five field rows + at most one per-field hint, plus a little slack.
        let form_h: u16 = if state.edit.is_some() { 8 } else { 0 };
        let [list_area, form_area] =
            Layout::vertical([Constraint::Min(2), Constraint::Length(form_h)]).areas(inner);

        // The panel proper: one line per host, showing what you'd actually go
        // here to find out — live connection state (with a `Failed` reason
        // spelled out), how many sessions it holds and how many you're attached
        // to, the daemon version it reported at handshake, and a latency sample.
        // The header only carries the aggregate, so this is where the detail
        // lives (§9).
        let mut lines: Vec<Line> = Vec::new();
        for (i, r) in state.rows.iter().enumerate() {
            let on = i == state.cursor;
            let marker = if on && state.edit.is_none() {
                "\u{276F} "
            } else {
                "  "
            };
            let label = if r.label.text().trim().is_empty() {
                "(unnamed)".to_string()
            } else {
                r.label.text().to_string()
            };
            let host = r.host();
            let icon = if r.icon.text().trim().is_empty() {
                self.host_icon(&host)
            } else {
                r.icon.text().to_string()
            };
            // A suspended host is dimmed whole: it has no backend, so every live
            // number the row would otherwise carry is simply absent, and the row
            // should read as parked rather than as broken.
            let label_style = if r.disabled {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(config::get().colors.ui.title_fg).bold()
            };
            let mut spans = vec![
                Span::raw(marker),
                Span::raw(format!("{icon} ")),
                Span::styled(format!("{label:<14}"), label_style),
            ];
            // Everything before the status: marker (2) + icon and its space (3)
            // + the padded label (14). A `Failed` reason quotes the host and can
            // run for paragraphs, so it is truncated to what's left rather than
            // being allowed to run off the popup — `l` is where it's read whole.
            let status_width = (list_area.width as usize).saturating_sub(2 + 3 + 14);
            if r.disabled {
                // Not `host_status_spans`' "not connected", which means "there is
                // no backend for this row *yet*" — this one is a decision.
                spans.push(Span::styled(
                    "disconnected",
                    Style::default().add_modifier(Modifier::DIM),
                ));
            } else {
                spans.extend(self.host_status_spans(&host, status_width));
            }
            lines.push(Line::from(spans));
            // The target is secondary detail — one indented dim line, so the
            // status line above stays scannable across many hosts. The options
            // ride that same line rather than earning one of their own: they
            // *are* the rest of the ssh command the target ends, and a port
            // forward among them is otherwise completely invisible — nothing
            // else in the dashboard says a local port is answered by another
            // machine.
            //
            // The clipboard marker lands here for exactly that reason: it *is*
            // one more forward on the same child, so it belongs beside the ones
            // the user typed rather than on the status line, which reports live
            // connection state. `p` in the footer is what names the key.
            let mut detail = format!(
                "      {} {} {}",
                if r.is_socket { "socket" } else { "ssh" },
                r.target.text(),
                r.options.text().trim()
            )
            .trim_end()
            .to_string();
            // Appended after the trim, so a host with no options gets one space
            // before the marker rather than two.
            if r.clipboard {
                detail.push_str(" \u{1f4cb}");
            }
            lines.push(Line::from(Span::styled(
                detail,
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        let add_on = state.cursor == state.rows.len() && state.edit.is_none();
        lines.push(Line::from(Span::styled(
            format!("{}+ add host", if add_on { "\u{276F} " } else { "  " }),
            Style::default().add_modifier(Modifier::DIM),
        )));
        // Removing a host drops it and its mirror, so it asks first.
        if let Some(idx) = state.pending_remove {
            let label = state
                .rows
                .get(idx)
                .map(|r| r.label.text().to_string())
                .unwrap_or_default();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  Remove host \"{label}\"? [y/N]"),
                Style::default()
                    .fg(config::get().colors.ui.attention_fg)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        // The upgrade's question, or its refusal. Both render here rather than
        // on a status line the panel doesn't have — a refusal the user never
        // sees is indistinguishable from a key that does nothing.
        if let Some(prompt) = &state.pending_upgrade {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                prompt.text.clone(),
                Style::default()
                    .fg(config::get().colors.ui.attention_fg)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        frame.render_widget(Paragraph::new(lines), list_area);

        // The field form for the row being edited.
        if let Some(focus) = state.focus()
            && let Some(r) = state.rows.get(state.cursor)
        {
            let field_row = |focused: bool, label: &str, value: Line<'static>| {
                let mark = if focused {
                    Span::styled("\u{276F} ", Style::default().bold())
                } else {
                    Span::raw("  ")
                };
                let mut spans = vec![
                    mark,
                    // 10, not 9: `Clipboard` is exactly nine cells, and a label
                    // that fills its own column runs into the value.
                    Span::styled(
                        format!("{label:<10}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ];
                spans.extend(value.spans);
                Line::from(spans)
            };
            let label_line = Line::from(text_field_spans(&r.label, focus == HostField::Label));
            let kind = if r.is_socket { "socket" } else { "ssh" };
            let mut target_spans = vec![Span::styled(
                format!("[{kind}] "),
                Style::default().add_modifier(Modifier::DIM),
            )];
            target_spans.extend(text_field_spans(&r.target, focus == HostField::Target));
            let target_line = Line::from(target_spans);
            let options_line =
                Line::from(text_field_spans(&r.options, focus == HostField::Options));
            // The derived emoji stands where the field's text would be, dim, so
            // an empty field says what it will *do* rather than reading as one
            // the user forgot. It follows the cursor rather than replacing it.
            let mut icon_spans = text_field_spans(&r.icon, focus == HostField::Icon);
            if r.icon.text().trim().is_empty() {
                icon_spans.push(Span::styled(
                    format!("{} (auto)", self.host_icon(&r.host())),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            let icon_line = Line::from(icon_spans);
            // The one field with no cursor, so it has to say its state in words:
            // `[off]` on an untouched row is what tells you the setting is here at
            // all. The marker doubles as the tie to the row's own `📋`.
            let clipboard_line = Line::from(vec![
                Span::styled(
                    if r.clipboard { "[on] " } else { "[off]" },
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::raw(if r.clipboard { "\u{1f4cb}" } else { "" }),
            ]);
            let mut form_lines = vec![
                field_row(focus == HostField::Label, "Label", label_line),
                field_row(focus == HostField::Target, "Target", target_line),
                field_row(focus == HostField::Options, "Options", options_line),
                field_row(focus == HostField::Icon, "Icon", icon_line),
                field_row(focus == HostField::Clipboard, "Clipboard", clipboard_line),
            ];
            // Per-field hints for the non-obvious affordances. The Ports one is
            // the syntax itself: the field accepts more forms than a label can
            // carry, and examples teach it in less room than a grammar would.
            match focus {
                super::HostField::Target => form_lines.push(Line::from(Span::styled(
                    "  ^t toggle ssh / socket",
                    Style::default().add_modifier(Modifier::DIM),
                ))),
                // An example of the one thing this field is really for, and a
                // pointer to where the rest belongs — which is the question the
                // field raises rather than answers.
                super::HostField::Options => form_lines.push(Line::from(Span::styled(
                    "  ssh args, e.g. -L 8080:localhost:3000   host setup: ~/.ssh/config",
                    Style::default().add_modifier(Modifier::DIM),
                ))),
                super::HostField::Icon => form_lines.push(Line::from(Span::styled(
                    "  ^e pick emoji   empty = auto",
                    Style::default().add_modifier(Modifier::DIM),
                ))),
                // Names the key, then the direction — "clipboard" on a host row
                // could as easily mean the host's own, and *whose* it is is the
                // whole point. Kept to the length of the `Options` hint above, so
                // it survives the same popup width that one does.
                super::HostField::Clipboard => form_lines.push(Line::from(Span::styled(
                    "  Space toggle   offer this machine's clipboard — paste a screenshot there",
                    Style::default().add_modifier(Modifier::DIM),
                ))),
                _ => {}
            }
            frame.render_widget(Paragraph::new(form_lines), form_area);
        }
    }

    fn draw_confirm(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(pending) = self.pending_confirm.as_ref() else {
            return;
        };
        let popup = centered_rect(60, 20, area);
        frame.render_widget(Clear, popup);
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            " Confirm ",
            Style::default()
                .fg(config::get().colors.ui.attention_fg)
                .bold(),
        ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let lines = vec![
            Line::from(""),
            Line::from(Span::raw(pending.prompt.clone())),
            Line::from(""),
            Line::from(vec![
                Span::styled("y/Y/Enter ", Style::default().bold()),
                Span::raw("confirm   "),
                Span::styled("any other key ", Style::default().bold()),
                Span::raw("cancel"),
            ]),
        ];
        let para = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }

    fn draw_header(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let total = self.sessions.len();
        let noun = if total == 1 { "session" } else { "sessions" };
        let ui = &config::get().colors.ui;

        // The header area is two rows: the bar itself plus a blank padding row
        // beneath it (the gap off the Sessions panel). The flat bar background
        // paints only the top row so the padding stays a true gap, not a
        // two-row-tall block.
        let bar = Rect { height: 1, ..area };

        // Left cluster: the paw logo, then the brand as a highlighted pill (the
        // header's call-out, mirroring the footer's key pills), then the
        // (search-independent) session count as flat text on the shared bar
        // background. The active name filter rides the Sessions panel label
        // instead, so the header stays a stable count regardless of search.
        //
        // Logo: two cells. With kitty graphics we reserve blank, bar-coloured
        // cells and overlay the PNG paw after the frame flushes
        // (`render_logo_graphics`); without, we draw the `🐾` emoji in the same
        // width — so the layout and the click hit-test are identical either way.
        let logo_w = super::logo::LOGO_CELLS.0;
        let mut left: Vec<Span> = Vec::with_capacity(6);
        // One cell of left padding so the paw isn't jammed against the edge.
        left.push(Span::styled(" ", bar_style()));
        let logo_x = bar.x + 1;
        if self.logo_caps.is_some() {
            left.push(Span::styled(" ".repeat(logo_w as usize), bar_style()));
        } else {
            left.push(Span::raw("🐾"));
        }
        left.push(Span::raw(" "));
        self.logo_rect = Some(Rect {
            x: logo_x,
            y: bar.y,
            width: logo_w,
            height: 1,
        });
        // The blank padding row beneath the bar is the cat's walking track (full
        // width, one cell tall) — empty, so the cat never collides with text.
        self.cat_track = Some(Rect {
            x: area.x,
            y: bar.y + 1,
            width: area.width,
            height: 1,
        });
        left.extend(pill(vec![Span::styled(
            "captain-miao",
            Style::default().fg(ui.title_fg).bold(),
        )]));
        left.extend(bar_segments(vec![vec![Span::styled(
            format!("{total} {noun}"),
            Style::default().add_modifier(Modifier::DIM),
        )]]));

        // Right cluster: the session layout and the default new-session backend
        // (always visible so the `Space l` / `Space a` choices are never hidden
        // state), then the host cluster — default host, then the ☁ tally beside
        // it, since both answer "which machines am I working across" and read as
        // one group — and finally the keep-awake ☕ indicator when sleep is
        // actively being inhibited. Flat text on the bar, with a trailing space
        // so it clears the terminal edge.
        let mut right_segs: Vec<Vec<Span<'static>>> = Vec::new();
        // The layout indicator names a *choice*; on a backend that has only one
        // arrangement (tmux — a tab per session either way) it would report state
        // the user can't change, so it hides with its `Space l` key.
        if self.capabilities.layout_is_a_choice() {
            right_segs.push(vec![
                Span::styled("Layout: ", Style::default().add_modifier(Modifier::DIM)),
                Span::styled(
                    self.sessions_layout.label(),
                    Style::default().fg(ui.title_fg),
                ),
            ]);
        }
        right_segs.push(vec![
            Span::styled(
                "Default agent: ",
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                self.new_session_agent.label(),
                Style::default().fg(ui.title_fg),
            ),
        ]);
        // The host cluster — the ☁️ tally, then the default host — appears only
        // once there's a choice to make. Both hang off the *same* emptiness
        // check, so a zero-remote user sees neither: naming a default host is
        // meaningless when localhost is the only one. The tally leads because it
        // is the alarm: a number *appearing* there is what should catch the eye,
        // and the default host beside it is the steady-state label.
        let tally = self.remote_host_tally();
        if !tally.is_empty() {
            let host = self.default_host_or_local();
            // Lit unless a host is mid-dial and this is the blink's dark half.
            right_segs.push(host_tally_spans(
                &tally,
                ui,
                self.connect_blink_phase().unwrap_or(true),
            ));
            right_segs.push(vec![
                Span::styled(
                    "Default host: ",
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("{} {}", self.host_icon(&host), host.0),
                    Style::default().fg(ui.title_fg),
                ),
            ]);
        }
        if self.sleep_inhibitor.is_active() {
            right_segs.push(vec![Span::styled(
                "\u{2615}",
                Style::default()
                    .fg(ui.attention_fg)
                    .add_modifier(Modifier::BOLD),
            )]);
        }
        let mut right = bar_segments(right_segs);
        right.push(Span::raw(" "));

        // The left paragraph fills the whole row with the flat bar background and
        // paints the left cluster on top; the right paragraph renders on top
        // *without* a base fill, so it can't clobber the left pill's highlight,
        // and its flat spans inherit the bar background already laid down.
        frame.render_widget(Paragraph::new(Line::from(left)).style(bar_style()), bar);
        frame.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            bar,
        );
    }

    /// The header ☁️'s blink phase right now: `Some(lit)` while any host is
    /// still dialing, `None` when none is — which is also the run loop's
    /// "nothing to animate" signal, so an idle dashboard keeps drawing no frames
    /// at all.
    ///
    /// The blink is client-driven rather than `SLOW_BLINK`: the attribute is a
    /// terminal's option to ignore (and the multiplexers in the middle another
    /// chance to drop it), and this indicator has to survive all of them.
    pub(super) fn connect_blink_phase(&self) -> Option<bool> {
        if self.remote_host_tally().connecting == 0 {
            return None;
        }
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Some(connect_blink_lit(since_epoch))
    }

    /// The utilisation spinner's frame right now, or `None` when nothing is
    /// waiting on a reading — the run loop's "nothing to animate" signal, on the
    /// same terms as [`connect_blink_phase`]. `None` whenever the hosts panel is
    /// shut or covered by the connection log, since only its rows draw the
    /// spinner and a dashboard nobody is looking at must still cost no frames.
    ///
    /// [`connect_blink_phase`]: App::connect_blink_phase
    pub(super) fn vitals_spinner_phase(&self) -> Option<usize> {
        let showing_rows = self
            .host_edit
            .as_ref()
            .is_some_and(|s| s.log_view.is_none());
        if !showing_rows
            || !self
                .backends
                .iter()
                .any(|b| matches!(b.vitals(), Some(VitalsView::Loading)))
        {
            return None;
        }
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Some(vitals_spinner_frame(since_epoch))
    }

    fn draw_detail(&mut self, frame: &mut ratatui::Frame, area: Rect, narrow: bool) {
        self.last_detail_rect = Some(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" Detail ", Style::default().bold()));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(s) = self.selected_session_ref() else {
            let msg = Paragraph::new(Text::styled(
                "(no session selected)",
                Style::default().add_modifier(Modifier::DIM),
            ));
            frame.render_widget(msg, inner);
            return;
        };

        let ctx_tokens = s.context_tokens;
        let ctx = ctx_tokens
            .map(format_tokens)
            .unwrap_or_else(|| "—".to_string());
        let model = s
            .model
            .as_deref()
            .map(model_label)
            .unwrap_or_else(|| "—".to_string());
        let model_style = s
            .model
            .as_deref()
            .map(|id| Style::default().fg(model_color(id)))
            .unwrap_or_default();
        let ctx_style = ctx_tokens.map(context_pressure_style).unwrap_or_default();
        let elapsed = format_elapsed(LauncherState::now().saturating_sub(s.updated_at));

        let label = |k: &'static str| {
            Span::styled(
                format!("{k:<9}"),
                Style::default().add_modifier(Modifier::DIM),
            )
        };

        // In the narrow stack the detail panel is a fixed, compact height, so it
        // shows only the four fields that don't fit in the trimmed table row.
        if narrow {
            let lines = vec![
                Line::from(vec![label("Agent"), Span::raw(s.agent.label())]),
                Line::from(vec![label("Model"), Span::styled(model, model_style)]),
                Line::from(vec![label("Context"), Span::styled(ctx, ctx_style)]),
                Line::from(vec![label("Updated"), Span::raw(format!("{elapsed} ago"))]),
            ];
            frame.render_widget(Paragraph::new(lines), inner);
            return;
        }

        let name = session_display_name(s, self.index_of(s), &self.random_names);
        let status_text = match (&s.status, &s.last_tool) {
            (SessionStatus::Active, Some(tool)) => format!("{} ({tool})", s.status.label()),
            _ => s.status.label().to_string(),
        };
        let ui = &config::get().colors.ui;
        let status_fg = super::format::status_fg(&s.status, self.is_follow_up(&super::flag_key(s)));
        let live_sid = self.index_of(s).live_session_id(s);
        let sid_short = live_sid
            .map(|sid| sid.split('-').next().unwrap_or(sid).to_string())
            .unwrap_or_else(|| "—".to_string());
        let cwd = self.shorten_path(&s.cwd).into_owned();
        let child = s
            .child_pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "—".to_string());
        // Resolve through the binding so the detail panel shows the real local
        // window even when the launcher self-reports none (dashboard-spawned /
        // remote-attached sessions, §15.3). A foreign-terminal row has no window
        // here — surface where it does live instead.
        let window = if let Some(identity) = self.foreign_terminal(s) {
            format!("in {identity}")
        } else if let Some(w) = self.window_id_for_session(s) {
            w.to_string()
        } else if self.detached_kind(s) == Some(super::format::Detached::HeldElsewhere) {
            // No window *here*, but the host says the pty has a client — say so
            // rather than the bare `—` a free detached row gets, which reads as
            // "nowhere" and is exactly the case this isn't.
            "elsewhere".to_string()
        } else {
            "—".to_string()
        };
        let tab = s
            .tab_id
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "—".to_string());
        // The terminfo the session renders against. Interesting mostly for a
        // *pooled* row, where libshpool froze it at the first attach and every
        // window since has inherited it — so a session opened from Kitty onto a
        // host without kitty's terminfo has been `xterm-256color` all along,
        // and this is the only place that says so.
        //
        // Matching this dashboard's own `TERM` says nothing, so it draws dim
        // (the same "this is background" device the detached tier uses) and a
        // value that differs is what stays bright. No badge, no colour: a
        // mismatch is usually benign, and yellow would cry wolf on every remote
        // row.
        let terminfo = match s.terminfo.as_deref() {
            Some(t) if Some(t) == self.terminfo.as_deref() => {
                Span::styled(t.to_string(), Style::default().add_modifier(Modifier::DIM))
            }
            Some(t) => Span::raw(t.to_string()),
            None => Span::raw("—"),
        };
        // …and when it isn't ours, a second line naming what this terminal is,
        // in the attention colour. The bright value above says *that* they
        // differ; this says what to do about it, because the remedy is the
        // same whichever way the difference arose — the host lacking our
        // terminfo, or another emulator having created the session: install
        // this name there (`infocmp -x <name> | ssh <host> tic -x -`) and the
        // sessions this terminal opens keep it.
        //
        // Derived rather than reported: the host *could* tell us it had to
        // substitute, but only through a wire field carrying a value that is
        // this one in every case worth acting on, so the comparison earns its
        // keep and the field doesn't.
        let terminfo_mismatch = self
            .terminfo
            .as_deref()
            .filter(|ours| s.terminfo.is_some() && s.terminfo.as_deref() != Some(*ours))
            .map(|ours| {
                Line::from(vec![
                    label(""),
                    Span::styled(
                        format!("not yours ({ours})"),
                        Style::default().fg(ui.attention_fg),
                    ),
                ])
            });
        let prompt = s.last_prompt.as_deref().unwrap_or("—");
        // Truncate the first prompt to a single line so a long opener doesn't
        // wrap into a wall of text above the last prompt.
        let first_prompt = truncate_str(
            s.first_prompt.as_deref().unwrap_or("—"),
            inner.width as usize,
        );

        let name_style = Style::default().add_modifier(Modifier::BOLD);
        let mut lines: Vec<Line> = vec![
            Line::from(vec![label("Name"), Span::styled(name, name_style)]),
            Line::from(vec![label("Agent"), Span::raw(s.agent.label())]),
            Line::from(vec![label("Model"), Span::styled(model, model_style)]),
            Line::from(vec![
                label("Status"),
                Span::styled(status_text, Style::default().fg(status_fg)),
            ]),
            Line::from(vec![label("Session"), Span::raw(sid_short)]),
            Line::from(vec![
                label("PID"),
                Span::raw(format!("{child} (win {window}, tab {tab})")),
            ]),
            Line::from(vec![label("Terminfo"), terminfo]),
        ];
        lines.extend(terminfo_mismatch);
        lines.extend([
            Line::from(vec![label("Context"), Span::styled(ctx, ctx_style)]),
            Line::from(vec![label("Updated"), Span::raw(format!("{elapsed} ago"))]),
            Line::from(vec![label("Dir"), Span::raw(cwd)]),
        ]);

        if let Some(err) = &s.last_error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Last error",
                Style::default().fg(ui.error_fg).add_modifier(Modifier::DIM),
            )));
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(ui.error_fg),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "First prompt",
            Style::default().add_modifier(Modifier::DIM),
        )));
        lines.push(Line::from(first_prompt));

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Last prompt",
            Style::default().add_modifier(Modifier::DIM),
        )));
        lines.push(Line::from(prompt.to_string()));

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    /// The host half of a row's icon cell, and whether it's a *foreign-terminal*
    /// marker rather than a host: the host's **emoji** (§9) — configurable per
    /// host in the hosts panel exactly like the workdir icons, with a
    /// deterministic fallback — or the "lives in another terminal instance"
    /// glyph for a local row this backend can't drive.
    ///
    /// This used to be its own `Host` column. It now shares the workdir-icon
    /// column as `<host>│<workdir>`: both answer "where is this?", they read
    /// better as one glyph pair than as two columns a table apart, and the merge
    /// hands the freed width back to the elastic last-prompt column.
    ///
    /// An icon rather than a name because it's a glance-level "which box is
    /// this?", and a name either truncates to noise or eats six cells. The
    /// foreign case loses its `kitty`/`zellij` wording in the trade — the row is
    /// already dimmed, and the detail panel names the instance in full. `None`
    /// for a row on this machine with nothing unusual about it. Drives both the
    /// column width and the cell so the two can't disagree.
    fn host_icon_cell(&self, s: &LauncherState) -> Option<(String, bool)> {
        if self.foreign_terminal(s).is_some() {
            return Some((FOREIGN_TERMINAL_GLYPH.to_string(), true));
        }
        if self.runs_on_this_machine(s) {
            return None;
        }
        Some((self.host_icon(&s.host), false))
    }

    fn draw_table(&mut self, frame: &mut ratatui::Frame, area: Rect, narrow: bool) {
        self.last_table_rect = Some(area);
        // Chrome above the data rows is the top rule + the table header (2
        // rows); there's no bottom border to subtract now.
        let visible_rows = area.height.saturating_sub(2) as usize;
        let total_visible = self.visible_len();
        let has_overflow = total_visible > visible_rows;

        // Re-clamp the scroll offset to the last full page. ratatui's Table only
        // ever scrolls the offset *down* to keep the selected row visible and
        // never back up to reclaim blank space (`visible_rows` in
        // ratatui-widgets: `start = min(offset, selected)` + a scroll-down loop,
        // no scroll-up-to-fill). So a transient panel *shrink* — e.g. the small
        // intermediate height a zellij detach/attach delivers — pushes the offset
        // down to hold the selection in view, and when the panel grows back the
        // offset stays stranded, scrolling the top rows out of sight while the
        // header still counts every session (looks like "8 sessions" but only 6
        // rows drawn). Pinning it to `total - page` here lets those rows return
        // on the next frame without needing a selection move. Done before the
        // `visible` borrow below so the mutable `table_state` access doesn't
        // overlap it.
        let max_offset = total_visible.saturating_sub(visible_rows);
        if self.table_state.offset() > max_offset {
            *self.table_state.offset_mut() = max_offset;
        }

        let visible = self.visible_sessions();

        let ui = &config::get().colors.ui;

        let title = {
            let mut spans = vec![Span::styled(" Sessions", Style::default().bold())];
            // Surface the name filter on the panel label (moved here from the
            // header) so it sits next to the list it filters. It appears the
            // moment Search mode opens — showing the live buffer, even while
            // empty — and persists while an applied filter is active back in
            // Normal mode. The brackets and query text get a vibrant color so
            // they pop out; the fixed "filter by name" words are dimmed.
            let filter_query: Option<&str> = if self.input_mode == InputMode::Search {
                Some(self.search_input.text())
            } else {
                self.search_filter.as_deref().filter(|q| !q.is_empty())
            };
            if let Some(q) = filter_query {
                let bright = Style::default().fg(ui.attention_fg).bold();
                let dim = Style::default()
                    .fg(ui.attention_fg)
                    .add_modifier(Modifier::DIM);
                spans.push(Span::styled(" [", bright));
                spans.push(Span::styled("filter by name", dim));
                if !q.is_empty() {
                    spans.push(Span::styled(format!(" {q}"), bright));
                }
                spans.push(Span::styled("]", bright));
            }
            if has_overflow {
                spans.push(Span::styled(
                    format!(" ({total_visible}, showing {visible_rows})"),
                    Style::default().bold(),
                ));
            }
            spans.push(Span::raw(" "));
            Line::from(spans)
        };
        // Show the host half of the icon column when remote hosts are federated,
        // or when a local row lives in another terminal instance (it doubles as
        // a "lives elsewhere" tag). A pure-local dashboard with no foreign rows
        // looks exactly as before. Width fits the widest glyph.
        // The narrow layout trims the table to status / workdir icon / name, so
        // the host half is dropped there along with the other extra columns.
        let any_foreign = visible.iter().any(|s| self.foreign_terminal(s).is_some());
        let show_host = !narrow && (self.backends.len() > 1 || any_foreign);

        let header_cells = if narrow {
            vec![
                Cell::from(""),
                Cell::from("Status"),
                Cell::from(""),
                Cell::from("Name"),
            ]
        } else {
            vec![
                Cell::from(""),
                Cell::from("Status"),
                Cell::from(""),
                Cell::from("Name"),
                Cell::from(Line::from("Ctx").alignment(Alignment::Right)),
                Cell::from("Last prompt"),
                Cell::from(Line::from("Updated").alignment(Alignment::Right)),
            ]
        };
        let header = Row::new(header_cells).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(ui.header_fg),
        );

        // Resolve every visible row's icon up front so we can size the icon
        // column from the widest one before building cells. Custom 2- or
        // 3-char icons (e.g. "py", "TS") need to fit without truncation.
        let now = LauncherState::now();
        let icon_marks: Vec<(String, Color)> = visible
            .iter()
            .map(|s| {
                let (icon, color, _) = self.effective_dir_mark(&s.cwd);
                (icon, color)
            })
            .collect();
        let icon_width = icon_marks
            .iter()
            .map(|(icon, _)| dir_icon_width(icon))
            .max()
            .unwrap_or(1)
            .max(1) as u16;
        // The host glyphs share that column, ahead of a divider (see
        // `host_icon_cell`). Sized off the widest so the divider — and therefore
        // every workdir icon behind it — lines up down the table.
        let host_icons: Vec<Option<(String, bool)>> = if show_host {
            visible.iter().map(|s| self.host_icon_cell(s)).collect()
        } else {
            Vec::new()
        };
        // Measured with `dir_icon_width`, the same ruler the workdir half uses —
        // it shares the column, so a second ruler would only let the divider
        // disagree with itself, and its `[1, DIR_ICON_MAX_CHARS]` clamp is what
        // keeps an over-long configured "emoji" from widening the whole column.
        let host_width = host_icons
            .iter()
            .flatten()
            .map(|(g, _)| dir_icon_width(g))
            .max()
            .unwrap_or(1)
            .max(1) as u16;
        // host glyph + the `│` divider, or nothing at all when there's no host
        // half to show.
        let host_slot = if show_host { host_width + 1 } else { 0 };

        // The Name column is a fixed max-width column (a dynamic fill looked
        // untidy): the truncate width plus 10 cells of headroom. The title is
        // truncated to the *same* width so the ellipsis lands at the column edge
        // rather than short of it.
        let name_col_max = crate::config::get().ui.table.name_truncate as u16 + 10;

        // In search mode, dim every column except the Name column so the eye
        // lands on the titles being filtered. The row-level DIM below covers
        // all cells; the Name cell removes it to stay bright.
        let search_active = self.input_mode == InputMode::Search;
        let mut rows: Vec<Row> = visible
            .iter()
            // Zip the pre-resolved icon marks in by value so each row's icon
            // String isn't cloned a second time.
            .zip(icon_marks)
            .enumerate()
            .map(|(row_idx, (s, (icon, icon_color)))| {
                let flags = self.flags_of(&super::flag_key(s));
                let important = flags.pinned;
                let follow_up = flags.follow_up;
                // A row that lives in another terminal instance is visible but
                // window-inert (D6) — dimmed, and flagged with
                // `FOREIGN_TERMINAL_GLYPH` in the icon column's host half.
                let foreign = self.foreign_terminal(s).is_some();
                // Running on its host with no window on this screen (§9). It
                // already sinks to its own sort tier; dimming says the same
                // thing where the eye lands first, so a screenful of detached
                // rows reads as background rather than as a list you're behind
                // on. The override glyph alone was too quiet for that.
                // …and *which* detached it is picks the glyph: nobody there, or
                // another client holding it.
                let detached_kind = self.detached_kind(s);
                let detached = detached_kind.is_some();
                let status_text = s.status.label();
                let name = truncate_str(
                    &session_display_name(s, self.index_of(s), &self.random_names),
                    name_col_max as usize,
                );

                let override_cell = override_indicator_cell(follow_up, important, detached_kind);
                let status_fg = super::format::status_fg(&s.status, follow_up);
                let status_cell = Cell::from(status_text).style(Style::default().fg(status_fg));
                let name_cell = if search_active {
                    // Cancel the row-level DIM so the name column stays bright.
                    Cell::from(name).style(Style::default().remove_modifier(Modifier::DIM))
                } else {
                    Cell::from(name)
                };
                // `<host>│<workdir>` when a host half applies, else the workdir
                // icon alone. Each half is right-aligned inside its own slot, so
                // 1-cell defaults sit flush against wider custom labels and the
                // divider lines up down the table. A row with no host glyph pads
                // the divider away rather than drawing a bar with nothing on its
                // left.
                let mut icon_spans: Vec<Span<'static>> = Vec::new();
                if show_host {
                    let host_glyph = host_icons.get(row_idx).and_then(|o| o.as_ref());
                    let glyph_w = host_glyph.map_or(0, |(g, _)| dir_icon_width(g));
                    icon_spans.push(Span::raw(
                        " ".repeat((host_width as usize).saturating_sub(glyph_w)),
                    ));
                    match host_glyph {
                        Some((glyph, foreign)) => {
                            let style = if *foreign {
                                Style::default().add_modifier(Modifier::DIM)
                            } else {
                                Style::default()
                            };
                            icon_spans.push(Span::styled(glyph.clone(), style));
                            icon_spans.push(Span::styled(
                                "\u{2502}",
                                Style::default().add_modifier(Modifier::DIM),
                            ));
                        }
                        None => icon_spans.push(Span::raw(" ")),
                    }
                }
                icon_spans.push(Span::raw(
                    " ".repeat((icon_width as usize).saturating_sub(dir_icon_width(&icon))),
                ));
                icon_spans.push(Span::styled(icon, Style::default().fg(icon_color)));
                let icon_cell = Cell::from(Line::from(icon_spans));
                // The narrow layout keeps only status / workdir icon / name; the
                // context, last-prompt and updated columns are dropped.
                let mut row_cells = vec![override_cell, status_cell, icon_cell, name_cell];
                if !narrow {
                    let ctx_tokens = s.context_tokens;
                    let ctx = ctx_tokens.map(format_tokens).unwrap_or_default();
                    let ctx_style = ctx_tokens.map(context_pressure_style).unwrap_or_default();
                    let last_prompt = s
                        .last_prompt
                        .as_deref()
                        .map(|p| p.replace('\n', " "))
                        .unwrap_or_default();
                    let elapsed = elapsed_cell(now.saturating_sub(s.updated_at));
                    row_cells.push(
                        Cell::from(Line::from(ctx).alignment(Alignment::Right)).style(ctx_style),
                    );
                    row_cells.push(
                        Cell::from(last_prompt).style(Style::default().add_modifier(Modifier::DIM)),
                    );
                    row_cells.push(elapsed);
                }
                let row = Row::new(row_cells);
                if search_active || foreign || detached {
                    row.style(Style::default().add_modifier(Modifier::DIM))
                } else {
                    row
                }
            })
            .collect();

        // A host still dialing mirrors no sessions yet, so the table would
        // otherwise read as complete while rows are still on their way — worst
        // at startup, where an empty list looks like an answer. The line goes
        // *in the table*, where the missing rows will appear, rather than in the
        // panel title. It is chrome, not a session: dim, unselectable
        // (`visible_index_at` bounds clicks by `visible_len`), and uncounted by
        // the title's total, so a full list simply clips it like any other
        // trailing row.
        if let Some(label) = connecting_row_label(&self.connecting_hosts()) {
            rows.push(
                Row::new(vec![
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    // The Name column: indented under the names it is standing
                    // in for. Trailing columns are simply absent — ratatui draws
                    // the cells a row has and leaves the rest blank.
                    Cell::from(label),
                ])
                .style(Style::default().add_modifier(Modifier::DIM)),
            );
        }

        // Status column fits the longest enum label (and the "Status" header
        // floor) — recomputed from `SessionStatus::ALL` so adding a variant
        // automatically resizes the column.
        let status_width = (SessionStatus::max_label_width() as u16).max("Status".len() as u16);

        // Status / context / elapsed are fixed-width (their content is
        // bounded), so they get hard `Length` constraints. Name and prompt
        // share the leftover space — name caps at the truncate width but
        // shrinks before status does on narrow viewports.
        let icon_col_width = host_slot + icon_width;
        let constraints = if narrow {
            // Status / icon stay fixed-width; the name column fills the rest and
            // the ratatui table clips it when it doesn't fit.
            vec![
                Constraint::Length(OVERRIDE_COL_WIDTH),
                Constraint::Length(status_width),
                Constraint::Length(icon_col_width),
                Constraint::Min(10),
            ]
        } else {
            vec![
                Constraint::Length(OVERRIDE_COL_WIDTH),
                Constraint::Length(status_width),
                Constraint::Length(icon_col_width),
                // Name is a fixed max-width column (see `name_col_max`). Last
                // prompt is the elastic column: `Fill` soaks up the slack when
                // there's room and yields (truncates) first when there isn't, so
                // a tight viewport never collapses the session title.
                Constraint::Max(name_col_max),
                Constraint::Length(4),
                Constraint::Fill(1),
                Constraint::Length(ELAPSED_MAX_WIDTH),
            ]
        };
        let table = Table::new(rows, constraints)
            .header(header)
            // Top rule only — no side or bottom borders, so the list runs flush
            // to the edges (and down to the footer / preview) like the
            // header/footer ribbons. The title rides the top rule. Right padding
            // opens a one-cell gap between the last column and the detail panel;
            // padding insets only the content, so the top rule still spans the
            // full width and meets the detail panel's border.
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .padding(Padding::right(1))
                    .title(title),
            )
            .row_highlight_style(Style::default().bg(ui.highlight_bg))
            .highlight_symbol(Span::styled(
                ui.selection_symbol.clone(),
                Style::default().fg(ui.selection_fg),
            ))
            .highlight_spacing(HighlightSpacing::Always);

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    fn draw_preview(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.last_preview_rect = Some(area);

        // Lazily fill the parsed-lines cache. ANSI parsing over the ~2000 line
        // tail isn't trivial, and the preview text only changes on selection
        // moves or explicit refresh — every other redraw (status flips, fs
        // events) leaves it intact, so the cache absorbs them for free. Fade
        // each span in place (no per-line rebuild) and stamp the widest line
        // here, once per fill, so the per-frame draw never re-scans the cache.
        if self.preview_lines.is_none()
            && let Some(raw) = self.preview_text.as_deref()
        {
            let mut lines = ansi_to_lines(raw);
            for line in &mut lines {
                for span in &mut line.spans {
                    span.style = fade_style(span.style);
                }
            }
            self.preview_max_width = lines.iter().map(|l| l.width()).max().unwrap_or(0);
            self.preview_lines = Some(lines);
        }

        let placeholder: Vec<Line<'static>>;
        let (lines, max_line_width): (&[Line<'static>], usize) = match &self.preview_lines {
            Some(cached) => (cached.as_slice(), self.preview_max_width),
            None => {
                placeholder = vec![Line::from(Span::styled(
                    self.preview_placeholder(),
                    Style::default().add_modifier(Modifier::DIM),
                ))];
                let w = placeholder.iter().map(|l| l.width()).max().unwrap_or(0);
                (placeholder.as_slice(), w)
            }
        };

        // Top + bottom borders only — captain-miao usually shares the OS window
        // (and thus the cell width) with the previewed session, so left/right
        // borders would clip two cells off every line. A thick rule plus a row
        // of vertical padding visually separates the snapshot from real
        // terminal content above and below it.
        let border_style = Style::default().fg(Color::Blue);
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_type(BorderType::Thick)
            .border_style(border_style)
            .padding(Padding::vertical(1));
        // The title rides the top border, so `inner` doesn't depend on it —
        // measure the content area first so the `↑` indicator can report the
        // scroll actually applied rather than the raw (possibly over-large)
        // field.
        let inner = block.inner(area);

        // Vertical scroll actually applied, clamped to the last screenful.
        let total_lines = lines.len();
        let visible_rows = inner.height as usize;
        let max_scroll = total_lines.saturating_sub(visible_rows);
        let scroll = self.preview_scroll.min(max_scroll);

        // Horizontal scroll clamped to the real clip point, then written back:
        // `scroll_preview_right` saturating-adds unbounded, so without this the
        // field inflates past the clip point — leftward scroll looks dead until
        // the excess drains, and the `preview_h_scroll == 0` auto-refresh gate
        // stays silently defeated even once the view is back at the left edge.
        let view_width = area.width as usize;
        let h_scroll = self.preview_h_scroll.min(
            max_line_width
                .saturating_sub(view_width)
                .min(u16::MAX as usize) as u16,
        );
        self.preview_h_scroll = h_scroll;

        // Surface vertical scroll, clipping, and horizontal scroll in the title.
        // When a line's content is wider than the view, the right edge gets
        // clipped — `<` / `>` pan horizontally so content past the edge is
        // still reachable.
        let mut title_parts: Vec<String> = Vec::new();
        if let Some(age) = self.preview_age_label() {
            title_parts.push(age);
        }
        if self.preview_text.is_some() && scroll > 0 {
            title_parts.push(format!("↑{scroll}"));
        }
        if h_scroll > 0 {
            title_parts.push(format!("→{h_scroll}"));
        }
        if max_line_width > view_width + h_scroll as usize {
            title_parts.push(format!("clipped {max_line_width}→{view_width}w"));
        }
        let title = if title_parts.is_empty() {
            " Terminal Preview ".to_string()
        } else {
            format!(" Terminal Preview  ({}) ", title_parts.join(", "))
        };

        let block = block.title(Span::styled(title, Style::default().bold()));
        frame.render_widget(block, area);

        // Show a window of `visible_rows` lines ending at `total - scroll`.
        // We clone only the visible slice — the rest of the cache stays put for
        // the next redraw.
        let end = total_lines.saturating_sub(scroll);
        let start = end.saturating_sub(visible_rows);
        let window: Vec<Line> = lines[start..end].to_vec();
        frame.render_widget(Paragraph::new(window).scroll((0, h_scroll)), inner);
    }

    fn draw_help(&self, frame: &mut ratatui::Frame, area: Rect) {
        let popup = centered_rect(70, 90, area);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" Keybindings ", Style::default().bold()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let section = |title: &'static str| {
            Line::from(Span::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
        };
        let row = |keys: String, desc: &str| {
            Line::from(vec![
                Span::styled(format!("  {keys:<14}"), Style::default().fg(Color::Yellow)),
                Span::raw(desc.to_string()),
            ])
        };
        // Render the live binding(s) for a configurable command, so remaps in
        // `[keybinds]` show through. `(unbound)` when the user cleared it.
        let cmd = |c: Command| -> Line {
            let keys = self
                .keymap
                .keys_for(c)
                .unwrap_or_else(|| "(unbound)".to_string());
            row(keys, c.description())
        };

        let mut lines: Vec<Line> = vec![
            section("Navigation"),
            cmd(Command::SelectNext),
            cmd(Command::SelectPrev),
            row("g g".to_string(), "jump to top"),
            cmd(Command::JumpBottom),
            row("1..9".to_string(), "select Nth visible session"),
            row("C-1..C-9".to_string(), "select Nth and focus its window"),
        ];
        // Scrolling and refreshing a preview presuppose a preview to scroll. On
        // a backend that can't read a window at all (Ghostty) the panel only
        // ever holds the one-line explanation, so drop the keys rather than list
        // five that do nothing — the same treatment the unsupported `t` gets.
        if self.capabilities.capture {
            lines.extend([
                cmd(Command::ScrollPreviewUp),
                cmd(Command::ScrollPreviewDown),
                cmd(Command::ScrollPreviewLeft),
                cmd(Command::ScrollPreviewRight),
                cmd(Command::RefreshPreview),
            ]);
        }
        lines.extend([
            Line::from(""),
            section("Actions"),
            cmd(Command::FocusSelected),
            cmd(Command::NewSession),
            cmd(Command::NewSessionPrompt),
            cmd(Command::ResumePicker),
        ]);
        // `f` is the one action that turns on the *selected row's* backend
        // rather than on a global capability: a resume with no way to branch
        // has no fork to offer (`AgentControl::supports_fork`). Hide it there
        // instead of listing a key that does nothing, as the unsupported `t`
        // and the pool-only detach keys already do. With no row selected the
        // list is generic, so the key stays.
        if self
            .selected_session_ref()
            .is_none_or(|s| s.agent.supports_fork())
        {
            lines.push(cmd(Command::ForkSession));
        }
        lines.extend([
            cmd(Command::CopySessionId),
            cmd(Command::KillSelected),
            cmd(Command::RestartSelected),
            cmd(Command::RestartAll),
        ]);
        // Detach and steal only mean anything once some host pools its sessions
        // (a remote, or pooled-localhost) — otherwise a session *is* its window.
        // Hide them rather than list keys that only report they don't apply,
        // mirroring how the unsupported `t` is hidden on zellij.
        if self.backends.iter().any(|b| b.capabilities().pooled) {
            lines.push(cmd(Command::DetachRemote));
            lines.push(cmd(Command::StealAttach));
            lines.push(cmd(Command::AttachAll));
        }
        // zellij can't reparent a pane across tabs; drop the hint rather than
        // list a key that only errors.
        if self.capabilities.move_to_tab {
            lines.push(cmd(Command::MoveToTab));
        }
        lines.extend([
            cmd(Command::ShellTab),
            cmd(Command::JumpAttention),
            Line::from(""),
            section("Flags"),
            cmd(Command::TogglePin),
            cmd(Command::ToggleFollowUp),
            Line::from(""),
            section("Layout (leader: Space)"),
            cmd(Command::TogglePreview),
            cmd(Command::ToggleDetail),
            cmd(Command::EditDir),
            cmd(Command::ToggleKeepAwake),
            cmd(Command::DefaultAgent),
        ]);
        // Both layouts spawn a tab per session on a backend with no shared-tab
        // arrangement (tmux), so the toggle has nothing to switch between.
        if self.capabilities.layout_is_a_choice() {
            lines.push(cmd(Command::SessionsLayout));
        }
        // The default-host choice only exists once there's more than one host.
        if self.backends.len() > 1 {
            lines.push(cmd(Command::DefaultHost));
        }
        // Remote hosts are gated behind the `remote` feature (work in progress);
        // hide the key rather than list one that only reports it's unavailable.
        if super::REMOTE_ENABLED {
            lines.push(cmd(Command::ManageHosts));
        }
        lines.extend([
            row("drag borders".to_string(), "resize splits"),
            Line::from(""),
            section("Modes"),
            cmd(Command::Search),
            cmd(Command::ClearSearch),
            cmd(Command::Help),
            row(
                self.keymap
                    .keys_for(Command::Quit)
                    .map(|k| format!("{k} / C-c"))
                    .unwrap_or_else(|| "C-c".to_string()),
                "quit",
            ),
            Line::from(""),
            Line::from(Span::styled(
                "  Rebind via [keybinds] in config.toml",
                Style::default().add_modifier(Modifier::DIM),
            )),
        ]);

        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }

    fn draw_footer(&self, frame: &mut ratatui::Frame, area: Rect) {
        // The footer is a zellij-style ribbon of "<key> <label>" hints, but the
        // colour alternation lands at the key/label seam rather than per hint:
        // every shortcut key shares one background (bright + bold, so it pops)
        // and every label the other (dim, so it recedes). A pending prefix
        // (Space / g) or the search `/` marker gets a distinct yellow badge pill.
        let spans = match &self.input_mode {
            InputMode::Normal if self.pending_prefix.is_some() => {
                // which-key: a prefix (e.g. Space) is pending — show what the
                // next key can do, straight from the live keymap. The prefix
                // itself leads as a badge pill.
                let prefix = self.pending_prefix.expect("is_some checked");
                let mut spans = hint_badge(prefix.display());
                for (key, command) in self.keymap.continuations(prefix) {
                    // Same gate as the `?` overlay: don't advertise a layout
                    // toggle on a backend where both layouts are the same thing.
                    if command == Command::SessionsLayout && !self.capabilities.layout_is_a_choice()
                    {
                        continue;
                    }
                    spans.extend(hint_pair(&key, command.short_label()));
                }
                spans
            }
            InputMode::Normal if self.pending_g => {
                // The bespoke `g` prefix has a single continuation (`g g`).
                let mut spans = hint_badge("g".to_string());
                spans.extend(hint_pair("g", "top"));
                spans
            }
            InputMode::Normal => {
                // Build the hint strip from the live keymap so remaps show
                // through; an unbound command's hint is dropped entirely.
                let hints = [
                    (Command::FocusSelected, "focus"),
                    (Command::NewSession, "new"),
                    (Command::ResumePicker, "resume"),
                    (Command::JumpAttention, "next attention"),
                    (Command::Search, "search"),
                    (Command::Help, "help"),
                    (Command::Quit, "quit"),
                ];
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (c, label) in hints {
                    if let Some(key) = self.keymap.primary_key(c) {
                        spans.extend(hint_pair(&key, label));
                    }
                }
                // Advertise the leader itself (not one of its commands) so the
                // whole which-key menu is discoverable. Tracks a remapped leader.
                if let Some(prefix) = self.keymap.primary_prefix() {
                    spans.extend(hint_pair(&prefix, "more…"));
                }
                // The status message trails the ribbon as plain (un-pilled) text.
                if let Some(msg) = &self.status_msg {
                    let style = if self.status_is_error {
                        Style::default().fg(config::get().colors.ui.error_fg)
                    } else {
                        Style::default().add_modifier(Modifier::DIM)
                    };
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(msg.clone(), style));
                }
                spans
            }
            InputMode::Help => hint_pair("any key", "dismiss help"),
            InputMode::Search => {
                // Render the buffer with a block cursor at the edit position,
                // reusing the picker's REVERSED-cursor approach so it tracks
                // readline motion (Ctrl-A/E, arrows) instead of always sitting
                // at the end. The buffer stays plain text between a leading `/`
                // badge pill and the trailing apply/cancel pills.
                let text = self.search_input.text();
                let cursor = self.search_input.cursor();
                let before = text[..cursor].to_string();
                let at: String = text[cursor..]
                    .chars()
                    .next()
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                let after = if at.is_empty() {
                    String::new()
                } else {
                    text[cursor + at.len()..].to_string()
                };
                let reversed = Style::default().add_modifier(Modifier::REVERSED);
                let cursor_span = if at.is_empty() {
                    Span::styled(" ", reversed)
                } else {
                    Span::styled(at, reversed)
                };
                // The `/` badge pill's trailing pad already separates it from
                // the buffer, so the buffer follows directly.
                let mut spans = hint_badge("/".to_string());
                spans.push(Span::raw(before));
                spans.push(cursor_span);
                spans.push(Span::raw(after));
                spans.push(Span::raw("  "));
                spans.extend(hint_pair("Enter", "apply"));
                spans.extend(hint_pair("Esc", "cancel"));
                spans
            }
            InputMode::Picker => {
                // Static labels only. The *values* these keys change — the
                // agent, the host — live on the popup's own bottom line now
                // (`App::refresh_picker_footer`), where they sit beside the list
                // they govern instead of down here among fixed hint text.
                let mut spans = hint_pair("type", "filter");
                spans.extend(hint_pair("↑/↓", "navigate"));
                spans.extend(hint_pair("Enter", "select"));
                spans.extend(hint_pair("Esc", "clear/cancel"));
                let multi_host = self.backends.len() > 1;
                match self.picker.as_ref().map(|a| &a.kind) {
                    // Naming a worktree owns the keyboard, so the ribbon shows
                    // only what works there rather than a list of keys the
                    // intercept is swallowing.
                    Some(PickerKind::Workdir {
                        worktree: Some(arm),
                        ..
                    }) if arm.naming => {
                        spans = hint_pair("type", "worktree name (blank = auto)");
                        spans.extend(hint_pair("Enter", "done"));
                        spans.extend(hint_pair("Esc", "no worktree"));
                    }
                    Some(PickerKind::Workdir { agent, .. }) => {
                        spans.extend(hint_pair("Ctrl-d", "drop dir"));
                        spans.extend(hint_pair("Ctrl-t", "agent"));
                        // Hidden for an agent that has no worktrees, the same
                        // rule as `t` on zellij and `Space l` on tmux: don't
                        // offer a key that can only report it does nothing.
                        if agent.supports_worktrees() {
                            spans.extend(hint_pair("Ctrl-g", "worktree"));
                        }
                        if multi_host {
                            spans.extend(hint_pair("Ctrl-h", "host"));
                        }
                    }
                    Some(PickerKind::Resume { .. }) if multi_host => {
                        spans.extend(hint_pair("Ctrl-h", "host"));
                    }
                    _ => {}
                }
                spans
            }
            InputMode::Confirm => {
                let mut spans = hint_pair("y/Y/Enter", "confirm");
                spans.extend(hint_pair("any other key", "cancel"));
                spans
            }
            InputMode::DirEdit => {
                let mut spans = hint_pair("Tab/↑↓", "row");
                spans.extend(hint_pair("←→", "change"));
                spans.extend(hint_pair("^E", "emoji"));
                spans.extend(hint_pair("Enter", "save"));
                spans.extend(hint_pair("r", "reset"));
                spans.extend(hint_pair("Esc", "cancel"));
                spans
            }
            InputMode::HostEdit => {
                let host_edit = self.host_edit.as_ref();
                if host_edit.is_some_and(|h| h.log_view.is_some()) {
                    let mut spans = hint_pair("j/k", "scroll");
                    spans.extend(hint_pair("g/G", "top/bottom"));
                    spans.extend(hint_pair("Esc", "back"));
                    spans
                } else if host_edit.is_some_and(|h| h.edit.is_some()) {
                    // `Esc cancel`, not the old `back`: it puts the row as it was
                    // and Enter is what keeps the change, so the two keys have to
                    // read as the opposites they now are.
                    let mut spans = hint_pair("Tab/↑↓", "field");
                    spans.extend(hint_pair("^t", "ssh/socket"));
                    spans.extend(hint_pair("^e", "emoji"));
                    spans.extend(hint_pair("Enter", "save"));
                    spans.extend(hint_pair("Esc", "cancel"));
                    spans
                } else {
                    // No `s save`: the panel has no Save step — every mutation
                    // persists as it happens (§9) — and `Esc` closes rather than
                    // cancelling anything, so both old hints named keys that do
                    // not exist.
                    let mut spans = hint_pair("a", "add");
                    spans.extend(hint_pair("e", "edit"));
                    // The two shortcuts into a *named* field, where `e` always
                    // lands on Label. Only worth a hint for the fields you'd open
                    // the editor specifically to change.
                    spans.extend(hint_pair("^e", "icon"));
                    spans.extend(hint_pair("^t", "target"));
                    spans.extend(hint_pair("c", "connect/disconnect"));
                    spans.extend(hint_pair("d", "delete"));
                    // Shown only on a row that has somewhere to go, which is the
                    // same condition the row's `↑` marker draws under: a hint
                    // for a key that would silently do nothing is worse than no
                    // hint, and every other key here works on every row.
                    if self.selected_host_upgrade().is_some() {
                        spans.extend(hint_pair("u", "upgrade server"));
                    }
                    spans.extend(hint_pair("l", "log"));
                    spans.extend(hint_pair("Esc", "close"));
                    spans
                }
            }
        };
        // Fill the whole footer row with the flat bar background; the hint spans
        // render on top, keys as `KEY_BG` pills and labels inheriting the bar.
        frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style()), area);
    }
}

/// Stands in for a host emoji on a local row that lives in **another terminal
/// instance** — window-inert here, so it reads as "elsewhere" rather than as a
/// machine. One dim glyph, since the row is already dimmed and the detail panel
/// names the instance in full; the `kitty`/`zellij` wording it replaced needed a
/// column of its own, which is exactly what merging into the icon column gave
/// up.
const FOREIGN_TERMINAL_GLYPH: &str = "\u{29C9}";

/// Width of the leading override column: two emoji slots, each 2 cells wide.
/// Sized here rather than inline because both the narrow and wide constraint
/// lists have to agree with what `override_indicator_cell` builds — a column
/// narrower than the line right-aligned into it silently clips the bell.
const OVERRIDE_COL_WIDTH: u16 = 4;

/// One form field's spans, with the cursor drawn where it actually is.
///
/// A block parked after the text was honest while a field could only be appended
/// to. Now that the hosts panel's fields are [`TextInput`]s, the cursor is the
/// only thing on screen saying where the next character lands — so the cell
/// under it is reversed, with a reversed space standing in at end-of-text. An
/// unfocused field renders as plain text: two cursors in one form would be a
/// lie about which one the keyboard is in. Pure.
fn text_field_spans(input: &TextInput, focused: bool) -> Vec<Span<'static>> {
    let text = input.text();
    if !focused {
        return vec![Span::raw(text.to_string())];
    }
    // `TextInput` keeps the cursor on a char boundary, so this can't split a
    // multi-byte glyph.
    let (head, rest) = text.split_at(input.cursor().min(text.len()));
    let mut chars = rest.chars();
    let under = chars.next().map(String::from).unwrap_or_else(|| " ".into());
    vec![
        Span::raw(head.to_string()),
        Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)),
        Span::raw(chars.as_str().to_string()),
    ]
}

/// Squeeze arbitrary text — up to and including a host's multi-line refusal —
/// onto one row of `max` cells.
///
/// All three parts matter. **Flattening** is a correctness fix, not cosmetics: a
/// `\n` inside a `Span` doesn't wrap, it corrupts the row, and a `ConnState`
/// reason quotes host output verbatim. **Sanitizing** is the same argument
/// carried to its end: `split_whitespace` drops the whitespace controls but not
/// `ESC`, which a terminal executes rather than prints, so a host's stderr could
/// otherwise repaint the dashboard around its own error message (see
/// [`crate::backend::host_text_safe`]). **Truncating** with `truncate_str`'s `…`
/// then says the text was cut, where letting it run to the popup edge looks
/// like the whole message. Pure.
pub(super) fn one_line(text: &str, max: usize) -> String {
    let safe = crate::backend::host_text_safe(text);
    let flat = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_str(&flat, max)
}

/// The session table's trailing "still loading" line, or `None` when nothing is
/// dialing. Names the host while there's exactly one — the common case, and the
/// answer to "which box am I waiting on" without a trip to `Space h` — and falls
/// back to a count rather than a list, which would outgrow the Name column. The
/// `…` is the same "there's more coming" mark the truncations use. Pure.
pub(super) fn connecting_row_label(hosts: &[HostId]) -> Option<String> {
    match hosts {
        [] => None,
        [one] => Some(format!("loading sessions from {}…", one.0)),
        many => Some(format!(
            "loading sessions from {} remote hosts…",
            many.len()
        )),
    }
}

/// How long the connecting ☁️ stays lit, then dark, in one blink. Slow on
/// purpose and lit for most of the cycle: this says "hold on", not "look here",
/// so the cloud must still read as a steady header glyph out of the corner of an
/// eye — a fast or half-dark blink turns a routine handshake into an alarm.
const CONNECT_BLINK_LIT: Duration = Duration::from_millis(900);
const CONNECT_BLINK_DARK: Duration = Duration::from_millis(500);

/// The blink phase at `since_epoch` — `true` while the cloud is lit. Pure, and a
/// function of the wall clock rather than a stored anchor so every draw agrees on
/// the phase without the App carrying animation state; a clock step just moves
/// the blink along, which nothing depends on.
pub(super) fn connect_blink_lit(since_epoch: Duration) -> bool {
    let period = (CONNECT_BLINK_LIT + CONNECT_BLINK_DARK).as_millis();
    since_epoch.as_millis() % period < CONNECT_BLINK_LIT.as_millis()
}

/// The frames a host row turns while its utilisation figures are on their way.
///
/// Braille rather than the ASCII `|/-\`: it occupies one cell whichever frame is
/// up, so the numbers that replace it don't shift the rest of the trailer along,
/// and the dots read as motion at a glance without the aggression of a spinning
/// slash. Every frame has the same dot count for the same reason — an even
/// weight is a spin, an uneven one is a flicker.
pub(super) const VITALS_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long one frame holds. A tenth of a second reads as steady motion, and
/// matches the run loop's default idle tick so a spin costs one frame per
/// wake-up rather than forcing extra ones.
pub(super) const VITALS_SPINNER_STEP: Duration = Duration::from_millis(100);

/// The spinner's frame index at `since_epoch`. Pure, and off the wall clock
/// rather than a stored anchor for the same reason as [`connect_blink_lit`]:
/// every draw agrees on the phase without the App carrying animation state.
pub(super) fn vitals_spinner_frame(since_epoch: Duration) -> usize {
    let step = VITALS_SPINNER_STEP.as_millis().max(1);
    (since_epoch.as_millis() / step) as usize % VITALS_SPINNER.len()
}

/// The frame to draw right now.
fn vitals_spinner_glyph() -> &'static str {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    VITALS_SPINNER[vitals_spinner_frame(since_epoch)]
}

/// The header's ☁️ host tally: one colored number per bucket, good → error →
/// down. It is an **aggregate**, never per-host detail — which host, and *why*
/// (including a `Failed` reason), lives one `Space h` away in the hosts panel,
/// so the header stays glanceable at any host count (§9).
///
/// An empty bucket is dropped rather than printed as a `0`: the all-healthy
/// case then reads as one green number, and a problem announces itself by a
/// second number *appearing* beside it. The numbers carry no labels at this
/// width, so color is what tells the buckets apart — which is also why a
/// failing host (a diagnosis waiting to be read) is loud where a merely
/// re-dialing one, expected to clear on its own, is dim.
///
/// The one exception is a host still *dialing*, which prints no number of its
/// own and instead forces the connected count on screen at zero: the alternative
/// is a header that reads "one host, and it's fine" through the whole handshake,
/// with the number then not moving when the link finally lands.
///
/// The cloud carries an explicit **variation selector** (`U+2601 U+FE0F`) and is
/// *not* dimmed. Bare `U+2601` is a text-presentation glyph: terminals render it
/// as a hairline outline in the foreground colour, which DIM then washes out to
/// invisible — reported as "I cannot see the cloud icon". The emoji
/// presentation is a filled, self-coloured 2-cell glyph that reads at a glance.
///
/// `lit` is the blink phase from [`connect_blink_lit`], and only a *dialing*
/// tally ever passes `false`: the cloud drops out for the dark half, replaced by
/// the two spaces `unicode-width` measures the VS16 sequence as, so the
/// right-aligned cluster beside it doesn't step sideways once a second. Blinking
/// the glyph rather than the numbers keeps the animation on the thing that means
/// "link", and off the counts a user is trying to read. Pure.
pub(super) fn host_tally_spans(
    tally: &HostTally,
    ui: &config::UiColors,
    lit: bool,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(if lit { "\u{2601}\u{FE0F}" } else { "  " })];
    let mut first = true;
    for (count, shown, style) in [
        // The connected count is printed even at zero while a host is still
        // dialing — "☁️0" is the honest reading of a link that hasn't come up,
        // and it's the number that ticks up as hosts land. Everywhere else an
        // empty bucket stays silent.
        (
            tally.good,
            tally.good > 0 || tally.connecting > 0,
            Style::default().fg(Color::Green),
        ),
        (
            tally.error,
            tally.error > 0,
            Style::default()
                .fg(ui.attention_fg)
                .add_modifier(Modifier::BOLD),
        ),
        (
            tally.down,
            tally.down > 0,
            Style::default().add_modifier(Modifier::DIM),
        ),
    ] {
        if shown {
            // No separator before the *first* number. `unicode-width` measures
            // the VS16 sequence as 2 cells and ratatui reserves them, but a
            // terminal that paints the glyph 1 cell wide then leaves the second
            // blank — so an explicit space on top read as a two-cell gulf
            // between the cloud and the count. Dropping it gives one visual
            // space in that case and a tight `☁️1` where the glyph really is two
            // cells; either way at most one space. Numbers still separate from
            // each other, where nothing else does the job.
            if !first {
                spans.push(Span::raw(" "));
            }
            first = false;
            spans.push(Span::styled(count.to_string(), style));
        }
    }
    spans
}
