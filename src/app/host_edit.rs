//! The hosts panel (`Space h`): its state, its drawing and its keys.
//!
//! One file per modal, the shape `prefs.rs` already has. The panel used to be
//! spread across four — state in `mod.rs`, the four `draw_host_*` in `draw.rs`,
//! two handlers in `keys.rs`, persistence in `hosts.rs` — which meant reading it
//! end to end meant holding all of them open. Persistence stays where it is:
//! [`super::hosts`] is the `hosts.json` model, shared with the backend
//! reconcile, and is not part of this popup.
//!
//! The panel has **no Save step** (§9): every mutation persists as it happens,
//! so `Esc` has to be a real cancel — which is what [`RowEdit`] exists to make
//! possible, and why it carries a snapshot rather than a flag.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

use crate::backend::{ConnState, VitalsView};
use crate::config;
use crate::state::HostId;

use super::draw::{one_line, vitals_spinner_glyph, wrap_ranges};
use super::format::{ICON_SLOT_WIDTH, centered_rect, clear_overlay};
use super::picker::{TextInput, TextInputEvent};
use super::{Action, App};
use super::{hosts, picker};

/// Active hosts popup (`input_mode == InputMode::HostEdit`). A working copy of
/// the host list edited in place; committed (and the backends rebuilt) on save,
/// discarded on cancel.
#[derive(Debug)]
pub(crate) struct HostEditState {
    pub(in crate::app) rows: Vec<HostRow>,
    pub(in crate::app) message: Option<String>,
    /// Selected row (`0..rows.len()`), or `rows.len()` for the "+ add" line.
    pub(in crate::app) cursor: usize,
    /// `Some` while the selected row's fields have the keyboard — see
    /// [`RowEdit`]. `None` in the list. Drawn as a card over the list rather
    /// than inside it, so this is what dims the panel behind it too.
    pub(in crate::app) edit: Option<RowEdit>,
    /// The row a `d` press is asking about — the removal confirm (§9). `None`
    /// when nothing is pending.
    pub(in crate::app) pending_remove: Option<usize>,
    /// What a `u` press put on screen — a question to answer, or a refusal to
    /// acknowledge. Kept beside `pending_remove` rather than folded into the
    /// global [`PendingConfirm`] because that one switches `InputMode`, which
    /// would tear this panel down mid-question.
    pub(in crate::app) pending_upgrade: Option<UpgradePrompt>,
    /// The connection log open over the list (`l`). `Some` replaces the list
    /// view entirely — it wants the whole popup, since the text it exists to
    /// show is what didn't fit on a row.
    pub(in crate::app) log_view: Option<HostLogView>,
    pub(in crate::app) forward_view: Option<super::port_forwards::ForwardView>,
}

/// The hosts panel's row editor: which field has the keyboard, and what `Esc`
/// puts back.
///
/// One `Option` rather than an `editing` flag beside a focus and a snapshot: an
/// entry point that set two of the three and forgot the third would compile,
/// and the one it would forget is the snapshot — which is the difference
/// between `Esc` restoring a mistyped target and losing the old one. There are
/// three entry points (`a`, `e`/`Enter`, and the `^`-key that opens the editor
/// on a named field), so that is a live risk rather than a hypothetical one.
#[derive(Debug)]
pub(in crate::app) struct RowEdit {
    pub(in crate::app) focus: HostField,
    pub(in crate::app) origin: EditOrigin,
}

/// What `Esc` undoes in the hosts panel's row editor.
///
/// The panel has no Save step — a commit persists immediately (§9) — so its
/// counterpart has to be a real cancel, and a cancel needs the pre-edit
/// contents from somewhere. A row the edit *created* has none: abandoning it
/// removes it again, which is also what stops a half-typed `(unnamed)` row from
/// lingering in the list until the panel is reopened.
#[derive(Debug)]
pub(in crate::app) enum EditOrigin {
    Existing(Box<HostRow>),
    Added,
}

impl HostEditState {
    /// Only committed, usable rows participate in the persisted order.
    pub(in crate::app) fn host_order(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| r.is_local || r.config().is_some())
            .map(|r| r.host().0)
            .collect()
    }

    /// Start editing the selected row on `focus`, recording what `Esc` restores.
    pub(in crate::app) fn begin_edit(&mut self, focus: HostField) {
        let Some(row) = self.rows.get(self.cursor) else {
            return;
        };
        let focus = if row.is_local {
            HostField::CodexMode
        } else {
            focus
        };
        self.edit = Some(RowEdit {
            focus,
            origin: EditOrigin::Existing(Box::new(row.clone())),
        });
    }

    /// Append a blank row and edit it from the Label field. `Esc` removes it
    /// again — an empty row is not a host, and never became one on disk
    /// ([`App::apply_host_edits`] filters it), so leaving it in the list would
    /// only be a lie about what is configured.
    pub(in crate::app) fn begin_new_row(&mut self) {
        self.rows.push(HostRow::default());
        self.cursor = self.rows.len() - 1;
        self.edit = Some(RowEdit {
            focus: HostField::Label,
            origin: EditOrigin::Added,
        });
    }

    /// Abandon the edit in progress, restoring what was there before it.
    /// Persists nothing: no mutation reaches disk between `begin_edit` and the
    /// commit, so putting the row back is the whole of the undo.
    pub(in crate::app) fn cancel_edit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        match edit.origin {
            EditOrigin::Existing(row) => {
                if let Some(slot) = self.rows.get_mut(self.cursor) {
                    *slot = *row;
                }
            }
            EditOrigin::Added => {
                if self.cursor < self.rows.len() {
                    self.rows.remove(self.cursor);
                }
                self.cursor = self.cursor.min(self.rows.len());
            }
        }
    }

    /// The field with the keyboard, or `None` in the list.
    pub(in crate::app) fn focus(&self) -> Option<HostField> {
        self.edit.as_ref().map(|e| e.focus)
    }
}

/// The line a `u` press leaves in the hosts panel.
///
/// One type for both outcomes because they render identically and are dismissed
/// identically; only `actionable` decides whether `y` does anything. Keeping the
/// refusal on screen matters — this panel has no status line (its footer is key
/// hints), so a message set anywhere else would surface stale, after the panel
/// closed, or not at all.
#[derive(Debug)]
pub(crate) struct UpgradePrompt {
    pub(in crate::app) row: usize,
    pub(in crate::app) text: String,
    /// `false` for a refusal: any key dismisses it and nothing happens.
    pub(in crate::app) actionable: bool,
}

/// One rendered line of a host's connection log — see [`App::host_log_lines`].
#[derive(Debug, Clone)]
pub(crate) struct HostLogLine {
    /// How long ago the entry happened, on its **first** line only; `None` on
    /// the continuation lines of a multi-line entry.
    pub(crate) age: Option<String>,
    pub(crate) error: bool,
    pub(crate) text: String,
}

/// The hosts panel's connection-log view (`l`), scrolled over one host's
/// [`ConnLogEntry`](crate::backend::ConnLogEntry) list.
#[derive(Debug)]
pub(crate) struct HostLogView {
    pub(in crate::app) host: HostId,
    /// First visible line, counted in *physical* lines — a host's multi-line
    /// refusal scrolls like the paragraph it is, not as one indivisible entry.
    pub(in crate::app) scroll: usize,
    /// Content rows the last draw had. Recorded there because `G` and PageDown
    /// need a viewport height, and the popup's size is only known while
    /// rendering; 0 until the first frame, which just makes those keys no-ops
    /// for one frame.
    pub(in crate::app) rows: usize,
}

/// One editable host row in the popup.
///
/// The four text fields are [`TextInput`](picker::TextInput)s rather than bare
/// `String`s. They hold ssh targets and argument lines long enough that fixing a
/// typo in the middle has to be possible, which needs a cursor — and the widget
/// that has one already backs every picker's query and the directory-mark
/// editor's icon field, so the readline keys are the same ones here.
#[derive(Debug, Clone, Default)]
pub(crate) struct HostRow {
    pub(in crate::app) is_local: bool,
    pub(in crate::app) codex: Option<cm_core::agents::codex::CodexConfig>,
    pub(in crate::app) codex_error: Option<String>,
    pub(in crate::app) codex_endpoint: picker::TextInput,
    pub(in crate::app) label: picker::TextInput,
    /// ssh target (`user@host`) or, when `is_socket`, a socket path.
    pub(in crate::app) target: picker::TextInput,
    pub(in crate::app) is_socket: bool,
    /// Per-host emoji shown beside the workdir icon, picked with the same
    /// searchable picker as the workdir marks. Empty = derive one from the label.
    pub(in crate::app) icon: picker::TextInput,
    /// Suspended — see [`hosts::HostConfig::disabled`]. Toggled with `c`.
    pub(in crate::app) disabled: bool,
    /// Advanced SSH arguments, parsed without shell expansion.
    pub(in crate::app) options: picker::TextInput,
    pub(in crate::app) forwards: Vec<crate::ssh_forward::Rule>,
    /// Offer this host the clipboard — see [`hosts::HostConfig::clipboard`].
    /// A form field, toggled with `Space`: the panel's plain letters are for
    /// things you do *to* a row (connect, delete, upgrade), and this is part of
    /// what a host **is**, like its options. Being a field also means it shows its
    /// own state — `[off]` is visible the moment the editor opens, where a list
    /// key was only discoverable from the footer.
    pub(in crate::app) clipboard: bool,
}

impl HostRow {
    /// Localhost is synthetic; incomplete rows and the reserved local label
    /// never become remote connection records.
    pub(in crate::app) fn config(&self) -> Option<hosts::HostConfig> {
        let label = self.label.text().trim();
        let target = self.target.text().trim();
        if self.is_local
            || label.is_empty()
            || target.is_empty()
            || label.eq_ignore_ascii_case("local")
        {
            return None;
        }
        let icon = self.icon.text().trim();
        let mut config = hosts::HostConfig {
            label: label.to_string(),
            icon: (!icon.is_empty()).then(|| icon.to_string()),
            socket: self.is_socket.then(|| target.to_string()),
            ssh: (!self.is_socket).then(|| target.to_string()),
            disabled: self.disabled,
            clipboard: self.clipboard,
            options: hosts::split_options(self.options.text()),
            forwards: self.forwards.clone(),
        };
        config.migrate_forwards();
        Some(config)
    }

    /// The `HostId` this row configures — its label, trimmed exactly as
    /// [`App::apply_host_edits`] trims it on the way to disk, so a lookup
    /// against the live backends matches a row still being typed.
    pub(in crate::app) fn host(&self) -> HostId {
        if self.is_local {
            HostId::local()
        } else {
            HostId(self.label.text().trim().to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
/// One editable field of a host's row in the hosts panel. The order here is the
/// order Tab walks them in.
pub(crate) enum HostField {
    Label,
    Target,
    Options,
    Icon,
    /// The one field with nothing to type — see [`HostRow::clipboard`].
    Clipboard,
    CodexMode,
    CodexEndpoint,
    Forwards,
}

impl HostField {
    /// Form order — the order the fields are drawn in, which is the order the
    /// focus keys walk, and what the editor's card measures itself from: the
    /// widest hint over these fields sets its width and the count sets its
    /// height, so a sixth field changes the box without anyone resizing it.
    ///
    /// `Clipboard` is last rather than beside `Options`, where it belongs by
    /// meaning: the four text fields keep the Tab positions fingers already know,
    /// and `^e`'s "open the editor on Icon" stays the fourth stop it names.
    const ORDER: [HostField; 8] = [
        HostField::Label,
        HostField::Target,
        HostField::Options,
        HostField::Icon,
        HostField::Clipboard,
        HostField::CodexMode,
        HostField::CodexEndpoint,
        HostField::Forwards,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Label => "Label",
            Self::Target => "Target",
            Self::Options => "Advanced SSH options",
            Self::Forwards => "Port forwards",
            Self::Icon => "Icon",
            Self::Clipboard => "Clipboard",
            Self::CodexMode => "Codex connection",
            Self::CodexEndpoint => "Codex endpoint",
        }
    }

    fn visible_for(self, row: &HostRow) -> bool {
        match self {
            Self::Forwards => !row.is_local && !row.is_socket && row.config().is_some(),
            Self::Options => !row.is_local && !row.is_socket,
            Self::CodexMode => row.codex.is_some(),
            Self::CodexEndpoint => row
                .codex
                .as_ref()
                .is_some_and(|config| !config.mode.is_native()),
            _ => !row.is_local,
        }
    }

    /// The next field, forwards or back. Wraps: the form is a ring, so
    /// overshooting the last field costs one more press either way.
    pub(in crate::app) fn step(self, forward: bool) -> Self {
        let n = Self::ORDER.len();
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let next = if forward { i + 1 } else { i + n - 1 };
        Self::ORDER[next % n]
    }
}

// =============================================================================
// Drawing
// =============================================================================

fn utilisation_style(percent: f32, ui: &config::UiColors) -> Style {
    if percent >= 90.0 {
        Style::default().fg(ui.error_fg).bold()
    } else if percent >= 80.0 {
        Style::default().fg(ui.attention_fg).bold()
    } else {
        Style::default().dim()
    }
}

fn vitals_spans(vitals: cm_core::vitals::HostVitals, ui: &config::UiColors) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if !vitals.is_empty() {
        for (label, percent) in [
            ("cpu", vitals.cpu_percent),
            ("mem", vitals.mem_percent()),
            ("disk", vitals.disk_percent()),
        ] {
            spans.push(Span::styled(format!("  {label} "), Style::default().dim()));
            spans.push(match percent.filter(|value| value.is_finite()) {
                Some(value) => Span::styled(format!("{value:.0}%"), utilisation_style(value, ui)),
                None => Span::styled("n/a", Style::default().dim()),
            });
        }
    }
    spans
}

impl App {
    /// The live status spans for one host row in the panel: connection state
    /// (green when connected, the `Failed` reason verbatim when there is one),
    /// running/attached session counts, the daemon version from `Welcome`, and
    /// the opportunistic latency sample. A host that isn't connected yet — or
    /// isn't in the backend set at all (a row the user is still typing) — shows
    /// only what's known.
    fn host_status_spans(&self, host: &HostId, max_width: usize) -> Vec<Span<'static>> {
        let cfg = config::get();
        let ui = &cfg.colors.ui;
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
            // Keep utilisation before the dim annotations so high readings
            // remain visible even when the panel clips a long host row.
            let dim = Style::default().add_modifier(Modifier::DIM);
            let mut trailer = format!(
                "  {running} {}, {attached} attached",
                super::plural_sessions(running)
            );
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
            // These numbers stand or fall together, and none of them is ever
            // a held one (see [`VitalsView`]): they arrive with a reading — the
            // poll refreshes the latency sample on its way through — and until
            // one does, a spinner sits in their place. A row that has none
            // coming (a local backend, a host that isn't connected) shows
            // neither, spinner included.
            match backend.vitals() {
                Some(VitalsView::Reading(v)) => {
                    spans.extend(vitals_spans(v, ui));
                    if let Some(rtt) = backend.latency() {
                        trailer.push_str(&format!("  latency {}ms", rtt.as_millis()));
                    }
                }
                // A frame of the spinner, which the run loop keeps turning. The
                // wait is a round trip, so what this really says is "asked" —
                // and on a host that has stopped answering it turns until the
                // poll's deadline hands it to the arm below.
                Some(VitalsView::Loading) => {
                    spans.push(Span::styled(format!("  {}", vitals_spinner_glyph()), dim));
                }
                // Said rather than left blank, and in the attention colour: the
                // host is connected and everything else about it is on the row,
                // so numbers quietly missing reads as "nothing worth mentioning"
                // rather than "we asked and got nothing back".
                Some(VitalsView::Unavailable) => {
                    spans.push(Span::styled(
                        "  cpu/mem/disk unavailable".to_string(),
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

    /// The hosts popup's geometry, as a percentage of the frame. Shared by the
    /// list and by the row-editor card that floats inside it — the card insets
    /// from *this*, so neither can drift off the other's edge.
    const HOSTS_POPUP: (u16, u16) = (72, 60);

    /// The hosts popup: the host list, or — while `l` is open — one host's
    /// connection log in its place. The row editor is a card over the list
    /// ([`Self::draw_host_form`]), so both are drawn.
    pub(super) fn draw_host_edit(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        if state.forward_view.is_some() {
            self.draw_port_forwards(frame, area);
        } else if state.log_view.is_some() {
            self.draw_host_log(frame, area);
        } else {
            self.draw_host_list(frame, area);
            self.draw_host_form(frame, area);
        }
    }

    /// One host's connection narrative, oldest first — everything the panel row
    /// had to cut, plus the steps that led to it.
    ///
    /// Takes `&mut self` only to record the viewport height, which `G` and the
    /// page keys need and which nothing but a render knows.
    pub(super) fn draw_host_log(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(view) = self.host_edit.as_ref().and_then(|s| s.log_view.as_ref()) else {
            return;
        };
        let host = view.host.clone();
        let scroll = view.scroll;
        // Wider and taller than the list: these lines are quoted host output,
        // and wrapping a loader error at 72 cells helps nobody.
        let popup = centered_rect(88, 76, area);
        clear_overlay(frame, popup);
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            format!(" {host} \u{00b7} connection log ", host = host.0),
            Style::default().bold(),
        ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let lines = self.host_log_lines(&host);
        let rows = inner.height as usize;
        let cfg = config::get();
        let ui = &cfg.colors.ui;
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

    pub(super) fn draw_host_list(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        let popup = centered_rect(Self::HOSTS_POPUP.0, Self::HOSTS_POPUP.1, area);
        clear_overlay(frame, popup);
        // No key hints on the border: the footer bar already renders this
        // mode's bindings, and two copies of the same list disagree eventually.
        let cfg = config::get();
        let ui = &cfg.colors.ui;
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " Hosts · first is default ",
                Style::default().bold(),
            ))
            .title_bottom(Line::from(vec![
                Span::styled(" Disk: home · ", Style::default().dim()),
                Span::styled("≥80% high", utilisation_style(80.0, ui)),
                Span::raw(" · "),
                Span::styled("≥90% critical ", utilisation_style(90.0, ui)),
            ]));
        let list_area = block.inner(popup);
        frame.render_widget(block, popup);

        // The panel proper: one line per host, showing what you'd actually go
        // here to find out — live connection state (with a `Failed` reason
        // spelled out), how many sessions it holds and how many you're attached
        // to, the daemon version it reported at handshake, and a latency sample.
        // The header only carries the aggregate, so this is where the detail
        // lives (§9).
        let mut lines: Vec<Line> = Vec::new();
        for (i, r) in state.rows.iter().enumerate() {
            // Kept while the row editor is open, unlike the "+ add" line's: the
            // card covers the middle of the list, and this is what says which
            // row it belongs to once the label field is no longer the only clue.
            let marker = if i == state.cursor { "\u{276F} " } else { "  " };
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
            // connection state. The editor's `Clipboard` field is what sets it.
            let mut detail = format!(
                "      {} {} {}",
                if r.is_socket { "socket" } else { "ssh" },
                r.target.text(),
                r.options.text().trim()
            )
            .trim_end()
            .to_string();
            if r.is_local {
                detail = "      this machine".into();
            }
            // Appended after the trim, so a host with no options gets one space
            // before the marker rather than two.
            if !r.forwards.is_empty() {
                detail.push_str(&format!("  {} port forwards", r.forwards.len()));
            }
            if r.clipboard {
                detail.push_str(" \u{1f4cb}");
            }
            let codex = r
                .codex
                .as_ref()
                .map(|c| c.mode.label())
                .unwrap_or("unavailable");
            detail.push_str(&format!("  Codex: {codex}"));
            if let Some(error) = &r.codex_error {
                detail.push_str(&format!(" ({error})"));
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
        if let Some(message) = &state.message {
            lines.push(Line::from(message.clone()));
        }
        let selected_line = if state.pending_remove.is_some()
            || state.pending_upgrade.is_some()
            || state.message.is_some()
        {
            lines.len().saturating_sub(1)
        } else {
            (state.cursor * 2 + 1).min(lines.len().saturating_sub(1))
        };
        let scroll = selected_line.saturating_sub(list_area.height.saturating_sub(1) as usize);
        frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), list_area);
    }

    /// The selected row's fields, as a card floating over the list.
    ///
    /// Its own popup rather than the form pinned under the list that this was.
    /// Two things were wrong with that: the list lost eight of its rows the
    /// moment you pressed `e` — on a short terminal most of it — and a form
    /// sharing a box with a list it does *not* share a cursor with reads as one
    /// more part of the same view, when in fact every key means something
    /// different while it is up. A card that covers the list, dims it and names
    /// itself says that in the shape of the thing.
    pub(super) fn draw_host_form(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(state) = self.host_edit.as_ref() else {
            return;
        };
        if let Some(focus) = state.focus()
            && let Some(r) = state.rows.get(state.cursor)
        {
            let host_popup = centered_rect(Self::HOSTS_POPUP.0, Self::HOSTS_POPUP.1, area);
            // Sized to the *widest* hint over every field, not to the focused
            // field's: the hints differ by tens of cells, so a card measured
            // from the current one would resize under the cursor as Tab walks
            // the form. Plus six — two borders, their padding, and a cell of
            // margin so the longest hint doesn't sit against the frame.
            let hint_w = {
                use unicode_width::UnicodeWidthStr;
                HostField::ORDER
                    .iter()
                    .filter_map(|f| host_field_hint(*f))
                    .map(|h| h.width() as u16)
                    .max()
                    .unwrap_or(0)
            };
            // Never wider than the popup behind it less two cells a side, so it
            // stays inside that frame and reads as floating over the list: a card
            // exactly as wide lands its own border on the same columns, and the
            // two then look like one panel with a divider drawn across it.
            let width = (hint_w + 6).min(host_popup.width.saturating_sub(4));
            // What a field's text has to fit in: the card's inner width — the
            // frame and its padding are two cells a side — less the fixed
            // columns ahead of the value, less one cell for the end-of-text
            // cursor, which needs somewhere to sit on an otherwise full line.
            let label_w = HostField::ORDER
                .iter()
                .filter(|field| field.visible_for(r))
                .map(|field| field.label().len())
                .max()
                .unwrap_or(0)
                + 1;
            let value_col = 2 + label_w;
            let value_w = (width as usize).saturating_sub(4 + value_col + 1);
            // A field's rows: the mark and label on the first, continuation lines
            // indented to the value column so a value that wrapped still reads as
            // one field rather than as a nameless new one.
            let field_rows = |field: HostField, values: Vec<Vec<Span<'static>>>| {
                let focused = focus == field;
                let label = field.label();
                values
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| {
                        let mut spans = if i == 0 {
                            vec![
                                if focused {
                                    Span::styled("\u{276F} ", Style::default().bold())
                                } else {
                                    Span::raw("  ")
                                },
                                // Reserve a gap after the widest visible label.
                                Span::styled(
                                    format!("{label:<label_w$}"),
                                    Style::default().add_modifier(Modifier::DIM),
                                ),
                            ]
                        } else {
                            vec![Span::raw(" ".repeat(value_col))]
                        };
                        spans.extend(value);
                        Line::from(spans)
                    })
                    .collect::<Vec<_>>()
            };
            let label_lines = text_field_lines(&r.label, focus == HostField::Label, value_w);
            let kind = if r.is_socket { "socket" } else { "ssh" };
            let prefix = format!("[{kind}] ");
            // The kind sits ahead of the target on its first line, so that line
            // has that much less room — and every line of the field is wrapped to
            // it, rather than the continuation lines silently running wider than
            // the one above them.
            let mut target_lines = text_field_lines(
                &r.target,
                focus == HostField::Target,
                value_w.saturating_sub(prefix.chars().count()),
            );
            target_lines[0].insert(
                0,
                Span::styled(prefix, Style::default().add_modifier(Modifier::DIM)),
            );
            let options_lines = text_field_lines(&r.options, focus == HostField::Options, value_w);
            // The derived emoji stands where the field's text would be, dim, so
            // an empty field says what it will *do* rather than reading as one
            // the user forgot. It follows the cursor rather than replacing it.
            let mut icon_lines = text_field_lines(&r.icon, focus == HostField::Icon, value_w);
            if r.icon.text().trim().is_empty()
                && let Some(last) = icon_lines.last_mut()
            {
                last.push(Span::styled(
                    format!("{} (auto)", self.host_icon(&r.host())),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            // The one field with no cursor, so it has to say its state in words:
            // `[off]` on an untouched row is what tells you the setting is here at
            // all. The marker doubles as the tie to the row's own `📋`.
            let clipboard_line = vec![
                Span::styled(
                    if r.clipboard { "[on] " } else { "[off]" },
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::raw(if r.clipboard { "\u{1f4cb}" } else { "" }),
            ];
            let mut form_lines = field_rows(HostField::Label, label_lines);
            form_lines.extend(field_rows(HostField::Target, target_lines));
            if HostField::Options.visible_for(r) {
                form_lines.extend(field_rows(HostField::Options, options_lines));
            }
            form_lines.extend(field_rows(HostField::Icon, icon_lines));
            form_lines.extend(field_rows(HostField::Clipboard, vec![clipboard_line]));
            if r.is_local {
                form_lines.clear();
            }
            if let Some(codex) = &r.codex {
                form_lines.extend(field_rows(
                    HostField::CodexMode,
                    vec![vec![Span::raw(format!("[{}]", codex.mode.label()))]],
                ));
                if HostField::CodexEndpoint.visible_for(r) {
                    form_lines.extend(field_rows(
                        HostField::CodexEndpoint,
                        text_field_lines(
                            &r.codex_endpoint,
                            focus == HostField::CodexEndpoint,
                            value_w,
                        ),
                    ));
                }
            } else if r.is_local {
                form_lines.push(Line::from(
                    r.codex_error
                        .clone()
                        .unwrap_or_else(|| "Loading Codex settings…".into()),
                ));
            }
            if HostField::Forwards.visible_for(r) {
                form_lines.extend(field_rows(
                    HostField::Forwards,
                    vec![vec![Span::raw(format!(
                        "{} enabled · {} total  [manage]",
                        r.forwards.iter().filter(|f| !f.disabled).count(),
                        r.forwards.len()
                    ))]],
                ));
            }
            // The field rows — one per field until a value wraps — a blank, the
            // hint line (held whether this field has a hint or not, for the same
            // reason the width is), and the two borders. The card grows down as a
            // value wraps rather than the value being cut off at the frame: this
            // is text being *edited*, and what you cannot see you cannot tell you
            // typed twice.
            let height = (form_lines.len() as u16 + 4).min(host_popup.height);
            // A terminal too small to draw a frame around anything. The dashboard
            // as a whole is unusable well before this, so it is a guard against a
            // degenerate `Rect`, not a layout for a narrow screen.
            if width < 8 || height < 3 {
                return;
            }
            let popup = Rect {
                x: host_popup.x + (host_popup.width - width) / 2,
                y: host_popup.y + (host_popup.height - height) / 2,
                width,
                height,
            };
            // The list goes quiet under the card. It is modal — every key belongs
            // to the form while it is up, including the `j`/`k`/`d` that move and
            // delete rows out there — and dimming what stopped listening is the
            // same cue the preview pane uses when it is no longer live.
            frame
                .buffer_mut()
                .set_style(host_popup, Style::default().add_modifier(Modifier::DIM));
            clear_overlay(frame, popup);
            // Which of the two things Esc will do: put a row back, or drop one
            // that was never on disk. The old inline form couldn't say.
            let title = match state.edit.as_ref().map(|e| &e.origin) {
                Some(EditOrigin::Added) => " Add Host ",
                _ => " Edit Host ",
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .padding(Padding::horizontal(1))
                .title(Span::styled(title, Style::default().bold()));
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            // The blank goes in whether or not this field has a hint, so the one
            // line the card reserves for it doesn't shunt the fields up and down.
            form_lines.push(Line::from(""));
            if let Some(hint) = state.message.as_deref().or_else(|| host_field_hint(focus)) {
                form_lines.push(Line::from(Span::styled(
                    hint,
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            let focus_line = form_lines
                .iter()
                .rposition(|line| {
                    line.spans
                        .iter()
                        .any(|span| span.style.add_modifier.contains(Modifier::REVERSED))
                })
                .or_else(|| {
                    form_lines.iter().position(|line| {
                        line.spans
                            .first()
                            .is_some_and(|span| span.content.starts_with('❯'))
                    })
                })
                .unwrap_or(0);
            let scroll = focus_line.saturating_sub(inner.height.saturating_sub(2) as usize);
            frame.render_widget(
                Paragraph::new(form_lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
                inner,
            );
        }
    }
}

// =============================================================================
// Keys
// =============================================================================

impl App {
    pub(super) fn selected_host_has_log(&self) -> bool {
        self.host_edit
            .as_ref()
            .and_then(|panel| panel.rows.get(panel.cursor))
            .is_some_and(|row| {
                !row.is_local
                    || self
                        .backend_for(&row.host())
                        .is_some_and(|backend| backend.capabilities().pooled)
            })
    }

    /// The hosts panel (`Space h`). A list view with live per-host state, not a
    /// staged edit form (§9): there is no Save step, because every mutation
    /// persists as it happens — adding a host connects it immediately (so you
    /// watch its state animate in the list), an edit applies when you commit the
    /// row, and a removal takes a `d`-then-`y` confirm.
    pub(super) fn handle_host_edit_key(&mut self, key: KeyEvent) -> Option<Action> {
        let has_log = self.selected_host_has_log();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.host_edit.as_ref()?.forward_view.is_some() {
            self.handle_port_forward_key(key);
            return None;
        }
        if (!ctrl
            && !alt
            && key.code == KeyCode::Char('f')
            && self.host_edit.as_ref()?.edit.is_none()
            && self.host_edit.as_ref()?.log_view.is_none()
            && self.host_edit.as_ref()?.pending_remove.is_none()
            && self.host_edit.as_ref()?.pending_upgrade.is_none())
            || (matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
                && self.host_edit.as_ref()?.focus() == Some(HostField::Forwards))
        {
            self.open_port_forwards();
            return None;
        }
        let editing = self.host_edit.as_ref()?.edit.is_some();

        // The log view owns the keyboard while it's open — it replaces the list,
        // so none of the list's keys are reachable behind it.
        if self.host_edit.as_ref()?.log_view.is_some() {
            self.handle_host_log_key(key);
            return None;
        }

        // A pending upgrade owns the keyboard until answered — or, when it is a
        // refusal rather than a question, until acknowledged.
        if let Some(prompt) = self.host_edit.as_mut()?.pending_upgrade.take() {
            if prompt.actionable && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let host = self.host_edit.as_ref()?.rows.get(prompt.row)?.host();
                return Some(Action::UpgradeHost { host });
            }
            return None;
        }

        // A pending removal owns the keyboard until answered.
        if let Some(idx) = self.host_edit.as_ref()?.pending_remove {
            let state = self.host_edit.as_mut()?;
            state.pending_remove = None;
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                if idx < state.rows.len() {
                    state.rows.remove(idx);
                    state.cursor = state.cursor.min(state.rows.len());
                }
                self.apply_host_edits();
            }
            return None;
        }

        // A settings result stays visible until the next interaction, then
        // scrolling follows the selected host again instead of the old message.
        self.host_edit.as_mut()?.message = None;

        // List-mode globals (in field-edit these are text / Esc-back).
        if !editing && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.close_host_edit();
            return None;
        }

        // Ctrl-E opens the same searchable emoji picker the directory marks use
        // — one affordance, learned once. From the Icon field, and from the list
        // as the shortcut that opens the editor *on* that field: the picker was
        // otherwise five keys away from a row whose emoji you wanted to change.
        if ctrl && matches!(key.code, KeyCode::Char('e')) {
            let state = self.host_edit.as_mut()?;
            let opens_picker = match state.focus() {
                // In the Icon field the picker *is* the editor, so it shadows
                // readline's end-of-line — a field of at most four cells has
                // nowhere to jump to anyway.
                Some(HostField::Icon) => {
                    !state.rows.get(state.cursor).is_some_and(|row| row.is_local)
                }
                // In a text field ^e keeps that readline meaning and falls
                // through to the input below.
                Some(_) => false,
                // From the list, on a row: open the editor on Icon and go
                // straight where the key would have gone from there.
                None => {
                    let on_row =
                        state.cursor < state.rows.len() && !state.rows[state.cursor].is_local;
                    if on_row {
                        state.begin_edit(HostField::Icon);
                    }
                    on_row
                }
            };
            if opens_picker {
                self.open_emoji_picker_for_host();
                return None;
            }
        }

        let state = self.host_edit.as_mut()?;
        if let Some(focus) = state.focus() {
            // Field focus, by all three idioms the dashboard already uses: Tab
            // walks the form, ↑↓ walk it as the vertical list it looks like, and
            // ^n/^p are what the pickers bind. Backwards matters as much as
            // forwards — a form you can only cycle one way makes overshooting
            // Options cost three more presses.
            let step = match key.code {
                KeyCode::Tab | KeyCode::Down => Some(true),
                KeyCode::BackTab | KeyCode::Up => Some(false),
                KeyCode::Char('n') if ctrl => Some(true),
                KeyCode::Char('p') if ctrl => Some(false),
                _ => None,
            };
            if let Some(forward) = step {
                if let Some(edit) = state.edit.as_mut() {
                    let row = &state.rows[state.cursor];
                    let mut next = focus.step(forward);
                    for _ in 0..HostField::ORDER.len() {
                        if next.visible_for(row) {
                            break;
                        }
                        next = next.step(forward);
                    }
                    edit.focus = next;
                }
                return None;
            }
            match key.code {
                // Committing a row applies it: persist + reconnect right away.
                KeyCode::Enter => {
                    let row = state.rows.get(state.cursor)?;
                    if let Err(error) = shell_words::split(row.options.text()) {
                        state.message = Some(format!("Invalid SSH options: {error}"));
                        return None;
                    }
                    let config = row.codex.clone().map(|mut config| {
                        config.endpoint = row.codex_endpoint.text().trim().to_owned();
                        config
                    });
                    let original = state.edit.as_ref().and_then(|edit| match &edit.origin {
                        EditOrigin::Existing(row) => row.codex.as_ref(),
                        EditOrigin::Added => None,
                    });
                    let action = config
                        .filter(|config| Some(config) != original)
                        .map(|config| Action::ConfigureCodex {
                            host: row.host(),
                            config,
                        });
                    state.edit = None;
                    self.apply_host_edits();
                    return action;
                }
                // And Esc abandons it — the snapshot the edit carries is what
                // makes that a real cancel rather than a second commit.
                KeyCode::Esc => {
                    state.cancel_edit();
                    return None;
                }
                KeyCode::Char('t') if ctrl && focus == HostField::Target => {
                    if let Some(r) = state.rows.get_mut(state.cursor) {
                        r.is_socket = !r.is_socket;
                    }
                    return None;
                }
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                    if focus == HostField::CodexMode =>
                {
                    if let Some(config) = state
                        .rows
                        .get_mut(state.cursor)
                        .and_then(|r| r.codex.as_mut())
                    {
                        use cm_core::agents::codex::CodexMode;
                        config.mode = if config.mode == CodexMode::Native {
                            CodexMode::AppServer
                        } else {
                            CodexMode::Native
                        };
                    }
                    return None;
                }
                // The one field with no text in it, so the keys that would move a
                // cursor have nothing to do and flip the value instead. `Enter`
                // deliberately isn't one of them: it commits the row everywhere
                // else in this form, and a key that means "save" on four fields
                // must not mean "change" on the fifth.
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
                    if focus == HostField::Clipboard =>
                {
                    if let Some(r) = state.rows.get_mut(state.cursor) {
                        r.clipboard = !r.clipboard;
                    }
                    return None;
                }
                _ => {}
            }
            // Everything else is text. The fields are `TextInput`s, so the
            // readline keys, the arrows and Home/End all come for free — and a
            // key none of them claim is simply dropped.
            let r = state.rows.get_mut(state.cursor)?;
            match focus {
                HostField::Label => {
                    r.label.handle_key(key);
                }
                HostField::Target => {
                    r.target.handle_key(key);
                }
                HostField::Options => {
                    r.options.handle_key(key);
                }
                // Capped like the directory-mark icon, and for the same reason
                // now that the two share one table column: past ~4 cells an
                // "icon" stops reading as a mark and just widens the column for
                // every row. Post-hoc revert rather than a pre-check, so paste
                // and multi-byte input still go through `TextInput` first.
                HostField::Icon => {
                    use unicode_width::UnicodeWidthStr;
                    let prev = r.icon.text().to_string();
                    if matches!(r.icon.handle_key(key), TextInputEvent::Changed)
                        && r.icon.text().width() > ICON_SLOT_WIDTH
                    {
                        r.icon.set_text(prev);
                    }
                }
                // Nothing to type into: its own keys are handled above, and a key
                // none of them claim is dropped rather than falling through to a
                // `TextInput` this field does not have.
                HostField::Clipboard | HostField::CodexMode | HostField::Forwards => {}
                HostField::CodexEndpoint => {
                    r.codex_endpoint.handle_key(key);
                }
            }
        } else {
            let n = state.rows.len();
            // A modified key never falls through to the plain-letter commands
            // below: a stray `^d` in the list must not reach the removal
            // confirm. What Ctrl *does* mean here is "open the editor on this
            // key's field" — `^e` above, `^t` here — plus the pickers' own
            // ^n/^p, which are the list's ↑↓ under another name.
            if ctrl || alt {
                if ctrl {
                    match key.code {
                        KeyCode::Char('n') => state.cursor = (state.cursor + 1).min(n),
                        KeyCode::Char('p') => state.cursor = state.cursor.saturating_sub(1),
                        KeyCode::Char('t') if state.cursor < n => {
                            state.begin_edit(HostField::Target)
                        }
                        _ => {}
                    }
                }
                return None;
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => state.cursor = state.cursor.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => state.cursor = (state.cursor + 1).min(n),
                KeyCode::Char('J' | 'K') if state.cursor < n => {
                    let next = if key.code == KeyCode::Char('J') {
                        state.cursor + 1
                    } else {
                        state.cursor.wrapping_sub(1)
                    };
                    if next < n {
                        state.rows.swap(state.cursor, next);
                        state.cursor = next;
                        self.extra_prefs.host_order = Some(state.host_order());
                        // Ordering changes presentation and launch defaults only.
                        // Keep every existing backend and connection in place.
                        self.save_overrides();
                    }
                }
                KeyCode::Char('a') => state.begin_new_row(),
                KeyCode::Char('e') | KeyCode::Enter => {
                    if state.cursor == n {
                        state.begin_new_row();
                    } else {
                        state.begin_edit(HostField::Label);
                    }
                }
                // Suspend / resume the host. No confirm: unlike `d` it destroys
                // nothing — the row, its target and its icon all stay — and the
                // same key puts it straight back. No status line either: this
                // mode's footer renders key hints, so a message would only
                // surface, stale, once the panel closed — and the row itself
                // answers immediately (dimmed, reading `disconnected`, or
                // animating back through `connecting`).
                KeyCode::Char('c') if state.cursor < n && !state.rows[state.cursor].is_local => {
                    let row = &mut state.rows[state.cursor];
                    row.disabled = !row.disabled;
                    // Persists and rebuilds: `disabled` is part of what a backend
                    // is built from, so this drops (or dials) the connection now.
                    self.apply_host_edits();
                }
                // Removal is destructive (it drops the host and its mirror), so
                // it asks first.
                KeyCode::Char('d') if state.cursor < n && !state.rows[state.cursor].is_local => {
                    state.pending_remove = Some(state.cursor);
                }
                // Upgrade the host's server. Offered only where it would land on
                // something else — the row's `↑` says so, and the footer hint
                // appears with it — so a press here always has a decision to
                // report, either the cost or the reason there isn't one.
                KeyCode::Char('u') if state.cursor < n => {
                    let row = state.cursor;
                    let host = state.rows[row].host();
                    let offer = self.selected_host_upgrade()?;
                    let prompt = match self.upgrade_blocker(&host) {
                        Some(why) => super::UpgradePrompt {
                            row,
                            text: format!("  Cannot upgrade \"{}\": {why}", host.0),
                            actionable: false,
                        },
                        None => {
                            let n = self.host_session_counts(&host).0;
                            super::UpgradePrompt {
                                row,
                                text: format!(
                                    "  Upgrade \"{}\" to {}? {} [y/N]",
                                    host.0,
                                    offer.version,
                                    match n {
                                        0 => "The daemon restarts.".to_string(),
                                        n => format!(
                                            "{n} idle {} restart with it.",
                                            super::plural_sessions(n)
                                        ),
                                    }
                                ),
                                actionable: true,
                            }
                        }
                    };
                    self.host_edit.as_mut()?.pending_upgrade = Some(prompt);
                }
                // The row shows one truncated line of a failure; `l` is where
                // the whole thing — and the steps before it — is readable.
                KeyCode::Char('l') if state.cursor < n && has_log => {
                    let host = state.rows[state.cursor].host();
                    state.log_view = Some(HostLogView {
                        host,
                        scroll: 0,
                        rows: 0,
                    });
                }
                _ => {}
            }
        }
        None
    }

    /// Scroll keys for the connection log (`l`). Reading, not editing, so the
    /// bindings are the pager ones: `j`/`k`, the arrows, page keys, `g`/`G`.
    ///
    /// Everything else is swallowed rather than falling through to the list
    /// underneath — the same rule the `Space` prefix follows, and for the same
    /// reason: a mistyped key here must not reach `d`.
    pub(super) fn handle_host_log_key(&mut self, key: KeyEvent) {
        let Some(view) = self.host_edit.as_mut().and_then(|s| s.log_view.as_mut()) else {
            return;
        };
        // The draw clamps against the live line count; a page is the viewport
        // minus one line of overlap, so you never step over a line unread.
        let page = view.rows.saturating_sub(1).max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => {
                if let Some(state) = self.host_edit.as_mut() {
                    state.log_view = None;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => view.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char('f') => view.scroll += page,
            KeyCode::PageUp | KeyCode::Char('b') => view.scroll = view.scroll.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
            // The draw clamps this down to the real last page — it knows the
            // line count, and it has to re-clamp on every frame anyway.
            KeyCode::Char('G') | KeyCode::End => view.scroll = usize::MAX,
            _ => {}
        }
    }

    // =============================================================================
    // Directory marks
    // =============================================================================
}

// =============================================================================
// Rendering helpers -- host-only, so they live with the panel they serve
// =============================================================================

/// The hosts row-editor hint for one field, or `None` for a field that speaks for
/// itself. Indented to the label column, so it reads as belonging to the form
/// rather than to the card's frame.
///
/// A function over the field rather than a `match` inside the draw, because the
/// card is sized to the widest of these and that needs them enumerable — a hint
/// that only exists inside the arm that renders it can't be measured before it is
/// the focused one, which is how a form ends up resizing under the cursor.
fn host_field_hint(field: HostField) -> Option<&'static str> {
    match field {
        // The label is a name. Nothing to explain.
        HostField::Label => None,
        HostField::CodexMode => {
            Some("  Space toggle   applies to new launches and explicit restarts")
        }
        HostField::CodexEndpoint => {
            Some("  Unix socket on this host; unix:// uses the Codex default")
        }
        HostField::Target => Some("  ^t toggle ssh / socket"),
        // Point port setup toward the dedicated manager beside this field.
        HostField::Options => Some("  Quoted SSH arguments; use Port forwards below for tunnels"),
        HostField::Forwards => {
            Some("  Enter manage ports or import -L/-R/-D; save host edits first")
        }
        HostField::Icon => Some("  ^e pick emoji   empty = auto"),
        // Names the key, then the direction — "clipboard" on a host row could as
        // easily mean the host's own, and *whose* it is is the whole point. It is
        // the longest of these, so it is what the card's width is set by.
        HostField::Clipboard => {
            Some("  Space toggle   offer this machine's clipboard — paste a screenshot there")
        }
    }
}

/// One form field's value, wrapped to `width` cells, with the cursor drawn where
/// it actually is.
///
/// A block parked after the text was honest while a field could only be appended
/// to. Now that the hosts panel's fields are [`TextInput`]s, the cursor is the
/// only thing on screen saying where the next character lands — so the cell
/// under it is reversed, with a reversed space standing in at end-of-text. An
/// unfocused field renders as plain text: two cursors in one form would be a
/// lie about which one the keyboard is in.
///
/// Wrapped rather than truncated because the value is one being *edited*: three
/// `-L` forwards outrun the card, and the tail the frame cut off was still there
/// on save, editable by a cursor nothing on screen could show. Always yields at
/// least one line, so an empty field still has a row. Pure.
pub(super) fn text_field_lines(
    input: &TextInput,
    focused: bool,
    width: usize,
) -> Vec<Vec<Span<'static>>> {
    let text = input.text();
    // `TextInput` keeps the cursor on a char boundary, so no split below can cut
    // a multi-byte glyph.
    let cursor = focused.then(|| input.cursor().min(text.len()));
    let ranges = wrap_ranges(text, width);
    let last = ranges.len() - 1;
    ranges
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let seg = &text[r.start..r.end];
            // The cursor belongs to the line holding its byte, so at a wrap point
            // it draws at the head of the *next* line — which is where the
            // character it is about to insert would be pushed anyway. End-of-text
            // has no next line, so the last one carries it as a reversed space.
            match cursor {
                Some(c) if c >= r.start && (c < r.end || i == last) => {
                    let (head, rest) = seg.split_at(c - r.start);
                    let mut chars = rest.chars();
                    let under = chars.next().map(String::from).unwrap_or_else(|| " ".into());
                    vec![
                        Span::raw(head.to_string()),
                        Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)),
                        Span::raw(chars.as_str().to_string()),
                    ]
                }
                _ => vec![Span::raw(seg.to_string())],
            }
        })
        .collect()
}
