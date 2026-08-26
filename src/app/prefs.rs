//! Preferences overlay (`InputMode::Prefs`): the handful of tunables worth
//! changing without leaving the dashboard.
//!
//! **It writes `dashboard-overrides.json` and never `config.toml`.** That file
//! is the user's — hand-edited, Home Manager-generated, or a dotfiles symlink —
//! and a TUI that rewrote it would fight whatever generates it. So settings
//! stack in three layers, and this overlay only ever touches the top one:
//!
//! 1. the compiled default,
//! 2. `~/.config/captain-miao/config.toml`, the declarative file,
//! 3. `dashboard-overrides.json`'s `prefs`, what this overlay writes.
//!
//! Every field of [`PrefsOverrides`] is therefore an `Option`, and `None` means
//! *inherit* rather than "off" — which is why **resetting a row writes `None`
//! rather than the default value**. Writing the default would freeze today's
//! compiled-in number into the user's state file, and a later config.toml edit
//! would then appear to do nothing. [`apply_prefs_to_config`] is the one place
//! layer 3 is folded onto 1+2, and [`App::reapply_live_config`] re-runs the
//! whole stack from disk after any write.
//!
//! Two rows are deliberately absent, because they are not single values: the
//! default agent is the first entry of the ordered agent list in this overlay,
//! and the default host is the first entry of the one behind `Space h`.
//!
//! The overlay itself is two panes ([`PrefsPane`]) — categories on the left,
//! that category's rows on the right — with a row's editor opening in place.
//! `docs/prefs-overlay.md` carries the design; this file is the implementation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use serde::{Deserialize, Serialize};

use crate::agent::AgentControl;
use crate::config::{self, OnWindowClose, parse_color};
use crate::terminal::SessionsLayout;

use super::format::{centered_rect, clear_overlay};
use super::picker::TextInput;
use super::{Action, App, InputMode};

/// The left pane's tabs. `ALL` is the order they cycle in; [`Self::visible`]
/// is what hides one whose rows this terminal cannot offer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrefsCategory {
    Agents,
    General,
    Display,
    Colors,
    Terminal,
}

impl PrefsCategory {
    const ALL: &'static [Self] = &[
        Self::Agents,
        Self::General,
        Self::Display,
        Self::Colors,
        Self::Terminal,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
            Self::General => "General",
            Self::Display => "Display",
            Self::Colors => "Colors",
            Self::Terminal => "Terminal",
        }
    }

    fn visible(self, kitty: bool) -> bool {
        !matches!(self, Self::Terminal) || kitty
    }

    fn next(self, kitty: bool) -> Self {
        let mut c = self;
        for _ in 0..Self::ALL.len() {
            let i = Self::ALL.iter().position(|x| *x == c).unwrap_or(0);
            c = Self::ALL[(i + 1) % Self::ALL.len()];
            if c.visible(kitty) {
                return c;
            }
        }
        self
    }

    fn prev(self, kitty: bool) -> Self {
        let mut c = self;
        for _ in 0..Self::ALL.len() {
            let i = Self::ALL.iter().position(|x| *x == c).unwrap_or(0);
            c = Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()];
            if c.visible(kitty) {
                return c;
            }
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Which pane has the cursor. The overlay opens on `Items`, because landing on
/// the category list would make every visit start with an extra keystroke.
enum PrefsPane {
    Categories,
    Items,
}

/// The overlay's whole state, `Some` exactly while `input_mode == Prefs`.
///
/// Only the *cursor* lives here. An edited value is written straight through to
/// `App` and `dashboard-overrides.json` as it is committed, so closing the
/// overlay — by any route, including a crash — never loses or half-applies a
/// change, and there is no "unsaved" state to reconcile.
#[derive(Debug)]
pub(crate) struct PrefsState {
    category: PrefsCategory,
    cursor: usize,
    pane: PrefsPane,
    /// The in-place editor for a text row, `Some` only while it is open.
    field: Option<TextInput>,
    /// "Reset everything" is armed by one keypress and fired by a second, since
    /// it is the only row here that cannot be undone by re-typing a value.
    pending_reset_all: bool,
}

impl PrefsState {
    fn new() -> Self {
        Self {
            category: PrefsCategory::Agents,
            cursor: 0,
            pane: PrefsPane::Items,
            field: None,
            pending_reset_all: false,
        }
    }
}

/// Layer 3: what this overlay has overridden, stored under
/// `dashboard-overrides.json`'s `prefs` key.
///
/// Every field is `Option` and skipped when `None`, so the file records only
/// what the user actually changed — and `DashboardOverrides` skips the whole
/// `prefs` key via [`Self::is_empty`], leaving no trace at all until something
/// is set. Absence is what makes the layer below show through; see the module
/// doc on why a reset writes `None` rather than the current default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PrefsOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_window_close: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pooled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kitty_rc_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentPref>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_order: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_warning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_critical_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_auto_refresh_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_stale_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_fg: Option<String>,
    /// `None` = TOML / compiled default (stacked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions_layout: Option<String>,
    /// `None` = `sleep::supported()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prevent_sleep: Option<bool>,
}

impl PrefsOverrides {
    /// Whether nothing is overridden — the `skip_serializing_if` for the whole
    /// `prefs` key. Spelled out field by field rather than derived from
    /// `PartialEq` with the default, so adding a field is a compile error here
    /// rather than a silently-always-written key.
    pub(super) fn is_empty(&self) -> bool {
        self.on_window_close.is_none()
            && self.pooled.is_none()
            && self.kitty_rc_password.is_none()
            && self.agents.is_none()
            && self.host_order.is_none()
            && self.context_warning_tokens.is_none()
            && self.context_critical_tokens.is_none()
            && self.preview_auto_refresh_secs.is_none()
            && self.preview_stale_secs.is_none()
            && self.highlight_bg.is_none()
            && self.selection_fg.is_none()
            && self.attention_fg.is_none()
            && self.error_fg.is_none()
            && self.sessions_layout.is_none()
            && self.prevent_sleep.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AgentPref {
    pub id: String,
    pub enabled: bool,
}

/// Every known backend, in declaration order, all enabled — what a dashboard
/// that has never opened this overlay shows.
pub(super) fn default_agent_list() -> Vec<(AgentControl, bool)> {
    AgentControl::ALL
        .iter()
        .copied()
        .map(|a| (a, true))
        .collect()
}

/// Merge the stored agent order with the backends this build knows about.
///
/// The stored list leads, so the user's order and enable/disable survive; a
/// stored id this build no longer recognises is dropped rather than kept as a
/// dead row; and a backend added since the list was written is appended
/// **enabled**, so upgrading surfaces new agents instead of hiding them behind
/// a preference the user never expressed.
pub(super) fn resolve_agent_list(prefs: Option<&[AgentPref]>) -> Vec<(AgentControl, bool)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(list) = prefs {
        for p in list {
            if let Some(a) = AgentControl::from_cli(&p.id)
                && seen.insert(a)
            {
                out.push((a, p.enabled));
            }
        }
    }
    for a in AgentControl::ALL {
        if seen.insert(*a) {
            out.push((*a, true));
        }
    }
    out
}

/// When `prefs.agents` was never written, put `preferred` first so a TOML or
/// typed-override default is not clobbered by compile-order Claude.
pub(super) fn prefer_agent_first(
    mut list: Vec<(AgentControl, bool)>,
    preferred: AgentControl,
) -> Vec<(AgentControl, bool)> {
    if let Some(i) = list.iter().position(|(a, _)| *a == preferred) {
        let mut item = list.remove(i);
        item.1 = true;
        list.insert(0, item);
    }
    list
}

/// The default agent: the first enabled entry of the ordered list. This is why
/// there is no "default agent" row — reordering *is* the setting.
pub(super) fn first_enabled_agent(list: &[(AgentControl, bool)]) -> AgentControl {
    list.iter()
        .find(|(_, on)| *on)
        .map(|(a, _)| *a)
        .unwrap_or_default()
}

/// Fold layer 3 onto a `Config` already carrying layers 1 and 2. One direction
/// only: this reads the overrides and writes the config, never the reverse, so
/// a `None` leaves whatever `config.toml` and the compiled default settled on.
pub(super) fn apply_prefs_to_config(cfg: &mut config::Config, prefs: &PrefsOverrides) {
    if let Some(ref s) = prefs.sessions_layout
        && let Some(l) = SessionsLayout::from_label(s)
    {
        cfg.terminal.sessions_layout = Some(l);
    }
    if let Some(ref s) = prefs.on_window_close {
        cfg.remote.on_window_close = match s.as_str() {
            "detach" => OnWindowClose::Detach,
            _ => OnWindowClose::Close,
        };
    }
    if let Some(v) = prefs.pooled {
        cfg.launcher.pooled = v;
    }
    if let Some(ref pw) = prefs.kitty_rc_password {
        cfg.kitty.rc_password = pw.clone();
    }
    if let Some(v) = prefs.context_warning_tokens {
        cfg.thresholds.context_warning_tokens = v;
    }
    if let Some(v) = prefs.context_critical_tokens {
        cfg.thresholds.context_critical_tokens = v;
    }
    if let Some(v) = prefs.preview_auto_refresh_secs {
        cfg.polling.preview_auto_refresh_secs = v;
    }
    if let Some(v) = prefs.preview_stale_secs {
        cfg.thresholds.preview_stale_secs = v;
    }
    if let Some(ref s) = prefs.highlight_bg
        && let Some(c) = parse_color(s)
    {
        cfg.colors.ui.highlight_bg = c;
    }
    if let Some(ref s) = prefs.selection_fg
        && let Some(c) = parse_color(s)
    {
        cfg.colors.ui.selection_fg = c;
    }
    if let Some(ref s) = prefs.attention_fg
        && let Some(c) = parse_color(s)
    {
        cfg.colors.ui.attention_fg = c;
    }
    if let Some(ref s) = prefs.error_fg
        && let Some(c) = parse_color(s)
    {
        cfg.colors.ui.error_fg = c;
    }
}

#[derive(Clone, Copy)]
// Each category's rows are an enum plus a `const` slice giving their order.
// The slice is what the cursor indexes and what `prefs_item_count` measures, so
// a row that exists but is left out of the slice simply never appears — which
// is the intended way to stage one, not an oversight to fix.
enum GeneralRow {
    Layout,
    KeepAwake,
    OnWindowClose,
}

const GENERAL_ROWS: &[GeneralRow] = &[
    GeneralRow::Layout,
    GeneralRow::KeepAwake,
    GeneralRow::OnWindowClose,
];

#[derive(Clone, Copy)]
enum DisplayRow {
    WarnTokens,
    CritTokens,
    PreviewRefresh,
    PreviewStale,
}

const DISPLAY_ROWS: &[DisplayRow] = &[
    DisplayRow::WarnTokens,
    DisplayRow::CritTokens,
    DisplayRow::PreviewRefresh,
    DisplayRow::PreviewStale,
];

#[derive(Clone, Copy)]
enum ColorRow {
    Highlight,
    Selection,
    Attention,
    Error,
    Reset,
}

const COLOR_ROWS: &[ColorRow] = &[
    ColorRow::Highlight,
    ColorRow::Selection,
    ColorRow::Attention,
    ColorRow::Error,
    ColorRow::Reset,
];

impl App {
    pub(super) fn open_prefs(&mut self) {
        self.prefs = Some(PrefsState::new());
        self.input_mode = InputMode::Prefs;
    }

    pub(super) fn close_prefs(&mut self) {
        self.prefs = None;
        if self.input_mode == InputMode::Prefs {
            self.input_mode = InputMode::Normal;
        }
    }

    /// Kitty remote-control password is a kitty-only row. `graphics` is
    /// currently kitty-only too, but another backend growing the kitty
    /// graphics protocol must not reveal this field.
    fn kitty_rc_prefs(&self) -> bool {
        self.terminal_identity
            .as_deref()
            .is_some_and(|id| id.starts_with("kitty:"))
    }

    fn prefs_item_count(&self, cat: PrefsCategory) -> usize {
        match cat {
            PrefsCategory::Agents => self.agent_order.len().max(1),
            PrefsCategory::General => GENERAL_ROWS.len(),
            PrefsCategory::Display => DISPLAY_ROWS.len(),
            PrefsCategory::Colors => COLOR_ROWS.len(),
            PrefsCategory::Terminal => {
                if self.kitty_rc_prefs() {
                    1
                } else {
                    0
                }
            }
        }
    }

    fn clamp_prefs_cursor(&mut self) {
        let Some(cat) = self.prefs.as_ref().map(|p| p.category) else {
            return;
        };
        let n = self.prefs_item_count(cat);
        let Some(st) = self.prefs.as_mut() else {
            return;
        };
        if n == 0 {
            st.cursor = 0;
        } else {
            st.cursor = st.cursor.min(n - 1);
        }
    }

    /// The overlay's key dispatch. Fixed keys, not the remappable table — this
    /// is a text-input mode like the pickers, and an open field must be able to
    /// swallow any character.
    pub(super) fn handle_prefs_key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.prefs.as_ref().is_some_and(|p| p.pending_reset_all) {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.reset_all_prefs();
                    if let Some(p) = self.prefs.as_mut() {
                        p.pending_reset_all = false;
                    }
                }
                _ => {
                    if let Some(p) = self.prefs.as_mut() {
                        p.pending_reset_all = false;
                    }
                }
            }
            return None;
        }

        if self.prefs.as_ref().is_some_and(|p| p.field.is_some()) {
            match key.code {
                KeyCode::Enter => {
                    let (cat, cur, text) = {
                        let st = self.prefs.as_ref().expect("prefs");
                        (
                            st.category,
                            st.cursor,
                            st.field
                                .as_ref()
                                .map(|f| f.text().to_string())
                                .unwrap_or_default(),
                        )
                    };
                    if let Some(st) = self.prefs.as_mut() {
                        st.field = None;
                    }
                    self.commit_prefs_field(cat, cur, text);
                }
                KeyCode::Esc => {
                    if let Some(st) = self.prefs.as_mut() {
                        st.field = None;
                    }
                }
                _ => {
                    if let Some(field) = self.prefs.as_mut().and_then(|p| p.field.as_mut()) {
                        let _ = field.handle_key(key);
                    }
                }
            }
            return None;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close_prefs(),
            KeyCode::Char('R') => {
                if let Some(p) = self.prefs.as_mut() {
                    p.pending_reset_all = true;
                }
            }
            KeyCode::Tab => {
                if let Some(p) = self.prefs.as_mut() {
                    p.pane = match p.pane {
                        PrefsPane::Categories => PrefsPane::Items,
                        PrefsPane::Items => PrefsPane::Categories,
                    };
                }
            }
            KeyCode::BackTab => {
                if let Some(p) = self.prefs.as_mut() {
                    p.pane = match p.pane {
                        PrefsPane::Categories => PrefsPane::Items,
                        PrefsPane::Items => PrefsPane::Categories,
                    };
                }
            }
            KeyCode::Char('h') | KeyCode::Left if !ctrl => {
                let kitty = self.kitty_rc_prefs();
                if let Some(p) = self.prefs.as_mut() {
                    match p.pane {
                        PrefsPane::Items => p.pane = PrefsPane::Categories,
                        PrefsPane::Categories => {
                            p.category = p.category.prev(kitty);
                            p.cursor = 0;
                        }
                    }
                }
            }
            KeyCode::Char('l') | KeyCode::Right if !ctrl => {
                let kitty = self.kitty_rc_prefs();
                if let Some(p) = self.prefs.as_mut() {
                    match p.pane {
                        PrefsPane::Categories => p.pane = PrefsPane::Items,
                        PrefsPane::Items => {
                            p.category = p.category.next(kitty);
                            p.cursor = 0;
                        }
                    }
                }
            }
            KeyCode::Char('j') | KeyCode::Down if !ctrl => self.prefs_move(1),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => self.prefs_move(-1),
            KeyCode::Char('g') => {
                if let Some(p) = self.prefs.as_mut() {
                    p.cursor = 0;
                }
            }
            KeyCode::Char('G') => {
                let cat = self.prefs.as_ref().map(|p| p.category);
                if let Some(cat) = cat {
                    let n = self.prefs_item_count(cat);
                    if let Some(p) = self.prefs.as_mut() {
                        p.cursor = n.saturating_sub(1);
                    }
                }
            }
            KeyCode::Char('J') => self.prefs_reorder(1),
            KeyCode::Char('K') => self.prefs_reorder(-1),
            KeyCode::Char('x') | KeyCode::Char('d') => self.prefs_toggle_disable(),
            KeyCode::Char('r') => self.prefs_reset_row(),
            KeyCode::Char('-') | KeyCode::Char('[') => self.prefs_step(-1),
            KeyCode::Char('+') | KeyCode::Char(']') => self.prefs_step(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.prefs_activate(),
            _ => {}
        }
        self.clamp_prefs_cursor();
        None
    }

    fn prefs_move(&mut self, delta: i32) {
        let Some(st) = self.prefs.as_ref() else {
            return;
        };
        let pane = st.pane;
        let cat = st.category;
        match pane {
            PrefsPane::Categories => {
                let kitty = self.kitty_rc_prefs();
                if let Some(st) = self.prefs.as_mut() {
                    st.category = if delta > 0 {
                        st.category.next(kitty)
                    } else {
                        st.category.prev(kitty)
                    };
                    st.cursor = 0;
                }
            }
            PrefsPane::Items => {
                let n = self.prefs_item_count(cat) as i32;
                if n == 0 {
                    return;
                }
                if let Some(st) = self.prefs.as_mut() {
                    let next = st.cursor as i32 + delta;
                    st.cursor = next.rem_euclid(n) as usize;
                }
            }
        }
    }

    /// Move the highlighted agent up or down. Reordering is the only edit here
    /// that changes another setting as a side effect: the first enabled entry is
    /// the default agent.
    fn prefs_reorder(&mut self, delta: i32) {
        let Some(st) = self.prefs.as_ref() else {
            return;
        };
        if st.category != PrefsCategory::Agents || st.pane != PrefsPane::Items {
            return;
        }
        let i = st.cursor;
        let j = if delta > 0 { i + 1 } else { i.wrapping_sub(1) };
        if j >= self.agent_order.len() {
            return;
        }
        self.agent_order.swap(i, j);
        if let Some(st) = self.prefs.as_mut() {
            st.cursor = j;
        }
        self.persist_agent_order();
    }

    fn prefs_toggle_disable(&mut self) {
        let Some(st) = self.prefs.as_ref() else {
            return;
        };
        if st.category != PrefsCategory::Agents || st.pane != PrefsPane::Items {
            return;
        }
        let i = st.cursor;
        if i >= self.agent_order.len() {
            return;
        }
        self.agent_order[i].1 = !self.agent_order[i].1;
        if self.agent_order.iter().all(|(_, on)| !*on) {
            self.agent_order[i].1 = true;
            self.set_status("at least one agent must stay enabled".into(), true);
            return;
        }
        self.persist_agent_order();
    }

    pub(super) fn persist_agent_order(&mut self) {
        self.extra_prefs.agents = Some(
            self.agent_order
                .iter()
                .map(|(a, on)| AgentPref {
                    id: a.cli_subcommand().to_string(),
                    enabled: *on,
                })
                .collect(),
        );
        self.new_session_agent = first_enabled_agent(&self.agent_order);
        self.save_overrides();
        self.reapply_live_config();
    }

    /// Enter on the highlighted row. What that means is per-row: a toggle
    /// flips, an enumerated value advances, and a free-form one opens its editor
    /// seeded with the value currently in force — which may come from any of the
    /// three layers, so what you see is what you are about to override.
    fn prefs_activate(&mut self) {
        let Some(st) = self.prefs.as_ref() else {
            return;
        };
        if st.pane != PrefsPane::Items {
            if let Some(p) = self.prefs.as_mut() {
                p.pane = PrefsPane::Items;
            }
            return;
        }
        match st.category {
            PrefsCategory::Agents => {}
            PrefsCategory::General => match GENERAL_ROWS.get(st.cursor) {
                Some(GeneralRow::Layout) => {
                    self.sessions_layout = match self.sessions_layout {
                        SessionsLayout::Stacked => SessionsLayout::PerTab,
                        SessionsLayout::PerTab => SessionsLayout::Stacked,
                    };
                    self.extra_prefs.sessions_layout =
                        Some(self.sessions_layout.label().to_string());
                    self.save_overrides();
                    self.reapply_live_config();
                }
                Some(GeneralRow::KeepAwake) => {
                    self.toggle_prevent_sleep();
                }
                Some(GeneralRow::OnWindowClose) => {
                    self.on_window_close = match self.on_window_close {
                        OnWindowClose::Close => OnWindowClose::Detach,
                        OnWindowClose::Detach => OnWindowClose::Close,
                    };
                    self.extra_prefs.on_window_close = Some(
                        match self.on_window_close {
                            OnWindowClose::Close => "close",
                            OnWindowClose::Detach => "detach",
                        }
                        .into(),
                    );
                    self.save_overrides();
                    self.reapply_live_config();
                }
                None => {}
            },
            PrefsCategory::Display => {
                let seed = self.display_row_value(st.cursor);
                self.open_prefs_field(seed);
            }
            PrefsCategory::Colors => match COLOR_ROWS.get(st.cursor) {
                Some(ColorRow::Reset) => {
                    self.extra_prefs.highlight_bg = None;
                    self.extra_prefs.selection_fg = None;
                    self.extra_prefs.attention_fg = None;
                    self.extra_prefs.error_fg = None;
                    self.save_overrides();
                    self.reapply_live_config();
                }
                Some(_) => {
                    let seed = self.color_row_value(st.cursor);
                    self.open_prefs_field(seed);
                }
                None => {}
            },
            PrefsCategory::Terminal => {
                if !self.kitty_rc_prefs() {
                    return;
                }
                let seed = config::get().kitty.rc_password.clone();
                self.open_prefs_field(seed);
            }
        }
    }

    fn open_prefs_field(&mut self, seed: String) {
        if let Some(st) = self.prefs.as_mut() {
            st.field = Some(TextInput::with_text(seed));
        }
    }

    fn display_row_value(&self, cursor: usize) -> String {
        let cfg = config::get();
        match DISPLAY_ROWS.get(cursor) {
            Some(DisplayRow::WarnTokens) => cfg.thresholds.context_warning_tokens.to_string(),
            Some(DisplayRow::CritTokens) => cfg.thresholds.context_critical_tokens.to_string(),
            Some(DisplayRow::PreviewRefresh) => cfg.polling.preview_auto_refresh_secs.to_string(),
            Some(DisplayRow::PreviewStale) => cfg.thresholds.preview_stale_secs.to_string(),
            None => String::new(),
        }
    }

    fn color_row_value(&self, cursor: usize) -> String {
        let ui = &config::get().colors.ui;
        match COLOR_ROWS.get(cursor) {
            Some(ColorRow::Highlight) => config::format_color(ui.highlight_bg),
            Some(ColorRow::Selection) => config::format_color(ui.selection_fg),
            Some(ColorRow::Attention) => config::format_color(ui.attention_fg),
            Some(ColorRow::Error) => config::format_color(ui.error_fg),
            _ => String::new(),
        }
    }

    /// Accept an edited text row. An unparseable or empty value clears the
    /// override rather than storing junk, so a typo degrades to "inherit"
    /// instead of pinning a nonsense number.
    fn commit_prefs_field(&mut self, cat: PrefsCategory, cursor: usize, raw: String) {
        match cat {
            PrefsCategory::Display => {
                let Ok(n) = raw.trim().parse::<u64>() else {
                    self.set_status("invalid number".into(), true);
                    return;
                };
                match DISPLAY_ROWS.get(cursor) {
                    Some(DisplayRow::WarnTokens) => {
                        let warn = n.clamp(1_000, 2_000_000);
                        self.extra_prefs.context_warning_tokens = Some(warn);
                        let crit = config::get().thresholds.context_critical_tokens;
                        if warn >= crit {
                            let bumped = (warn + 5_000).min(2_000_000);
                            self.extra_prefs.context_critical_tokens = Some(bumped);
                        }
                    }
                    Some(DisplayRow::CritTokens) => {
                        let crit = n.clamp(1_000, 2_000_000);
                        self.extra_prefs.context_critical_tokens = Some(crit);
                        let warn = config::get().thresholds.context_warning_tokens;
                        if warn >= crit {
                            let lowered = crit.saturating_sub(5_000).max(1_000);
                            self.extra_prefs.context_warning_tokens = Some(lowered);
                        }
                    }
                    Some(DisplayRow::PreviewRefresh) => {
                        self.extra_prefs.preview_auto_refresh_secs = Some(n.min(120));
                    }
                    Some(DisplayRow::PreviewStale) => {
                        self.extra_prefs.preview_stale_secs = Some(n.min(600));
                    }
                    None => {}
                }
            }
            PrefsCategory::Colors => {
                if parse_color(&raw).is_none() {
                    self.set_status("invalid color".into(), true);
                    return;
                }
                match COLOR_ROWS.get(cursor) {
                    Some(ColorRow::Highlight) => self.extra_prefs.highlight_bg = Some(raw),
                    Some(ColorRow::Selection) => self.extra_prefs.selection_fg = Some(raw),
                    Some(ColorRow::Attention) => self.extra_prefs.attention_fg = Some(raw),
                    Some(ColorRow::Error) => self.extra_prefs.error_fg = Some(raw),
                    _ => {}
                }
            }
            PrefsCategory::Terminal => {
                if !self.kitty_rc_prefs() {
                    return;
                }
                if raw.trim().is_empty() {
                    return;
                }
                self.extra_prefs.kitty_rc_password = Some(raw);
                self.set_status("kitty password saved — restart miao to apply".into(), false);
            }
            _ => {}
        }
        self.save_overrides();
        self.reapply_live_config();
    }

    /// Nudge a numeric row without opening its editor. Steps are relative to the
    /// value currently in force, so the first nudge on an un-overridden row
    /// starts from the inherited value rather than from zero.
    fn prefs_step(&mut self, dir: i64) {
        let Some(st) = self.prefs.as_ref() else {
            return;
        };
        if st.category != PrefsCategory::Display || st.pane != PrefsPane::Items {
            return;
        }
        let cursor = st.cursor;
        let cfg = config::get();
        let (cur, step, min, max) = match DISPLAY_ROWS.get(cursor) {
            Some(DisplayRow::WarnTokens) => (
                cfg.thresholds.context_warning_tokens,
                5_000,
                1_000,
                2_000_000,
            ),
            Some(DisplayRow::CritTokens) => (
                cfg.thresholds.context_critical_tokens,
                5_000,
                1_000,
                2_000_000,
            ),
            Some(DisplayRow::PreviewRefresh) => (cfg.polling.preview_auto_refresh_secs, 1, 0, 120),
            Some(DisplayRow::PreviewStale) => (cfg.thresholds.preview_stale_secs, 5, 0, 600),
            None => return,
        };
        let next = (cur as i64 + dir * step as i64).clamp(min as i64, max as i64) as u64;
        self.commit_prefs_field(PrefsCategory::Display, cursor, next.to_string());
    }

    /// `r` on a row: clear that override so the layer below shows through
    /// again. Note every arm assigns `None`, never a value — see the module doc.
    fn prefs_reset_row(&mut self) {
        let Some(st) = self.prefs.as_ref() else {
            return;
        };
        match st.category {
            PrefsCategory::Agents => {
                self.agent_order = default_agent_list();
                self.extra_prefs.agents = None;
                self.new_session_agent = first_enabled_agent(&self.agent_order);
            }
            PrefsCategory::General => match GENERAL_ROWS.get(st.cursor) {
                Some(GeneralRow::Layout) => {
                    self.extra_prefs.sessions_layout = None;
                }
                Some(GeneralRow::KeepAwake) => {
                    self.extra_prefs.prevent_sleep = None;
                    self.prevent_sleep_enabled = crate::sleep::supported();
                    self.update_sleep_inhibitor();
                }
                Some(GeneralRow::OnWindowClose) => {
                    self.on_window_close = OnWindowClose::default();
                    self.extra_prefs.on_window_close = None;
                }
                None => {}
            },
            PrefsCategory::Display => match DISPLAY_ROWS.get(st.cursor) {
                Some(DisplayRow::WarnTokens) => self.extra_prefs.context_warning_tokens = None,
                Some(DisplayRow::CritTokens) => self.extra_prefs.context_critical_tokens = None,
                Some(DisplayRow::PreviewRefresh) => {
                    self.extra_prefs.preview_auto_refresh_secs = None
                }
                Some(DisplayRow::PreviewStale) => self.extra_prefs.preview_stale_secs = None,
                None => {}
            },
            PrefsCategory::Colors => match COLOR_ROWS.get(st.cursor) {
                Some(ColorRow::Highlight) => self.extra_prefs.highlight_bg = None,
                Some(ColorRow::Selection) => self.extra_prefs.selection_fg = None,
                Some(ColorRow::Attention) => self.extra_prefs.attention_fg = None,
                Some(ColorRow::Error) => self.extra_prefs.error_fg = None,
                Some(ColorRow::Reset) => {
                    self.extra_prefs.highlight_bg = None;
                    self.extra_prefs.selection_fg = None;
                    self.extra_prefs.attention_fg = None;
                    self.extra_prefs.error_fg = None;
                }
                None => {}
            },
            PrefsCategory::Terminal => self.extra_prefs.kitty_rc_password = None,
        }
        self.save_overrides();
        self.reapply_live_config();
    }

    /// The armed half of "reset everything": drop every override at once and
    /// re-derive what depends on them, so the dashboard matches a fresh install
    /// reading this user's `config.toml`.
    fn reset_all_prefs(&mut self) {
        self.extra_prefs = PrefsOverrides::default();
        self.agent_order = default_agent_list();
        self.new_session_agent = first_enabled_agent(&self.agent_order);
        self.prevent_sleep_enabled = crate::sleep::supported();
        self.update_sleep_inhibitor();
        self.save_overrides();
        self.reapply_live_config();
        self.set_status("preference overrides cleared".into(), false);
    }

    /// Rebuild the live `Config` from disk and re-fold the overrides onto it,
    /// then re-read the two settings `App` caches out of it. Called after any
    /// write here: re-reading from disk rather than patching the in-memory copy
    /// is what keeps a hand-edit of `config.toml` from being shadowed by a stale
    /// value this overlay happened to be holding.
    pub(super) fn reapply_live_config(&mut self) {
        let mut cfg = config::Config::from_disk();
        apply_prefs_to_config(&mut cfg, &self.extra_prefs);
        let _ = config::replace(cfg);
        let cfg = config::get();
        self.on_window_close = cfg.remote.on_window_close;
        self.sessions_layout = cfg.terminal.sessions_layout.unwrap_or_default();
    }

    pub(super) fn draw_prefs(&mut self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let Some(st) = self.prefs.as_ref() else {
            return;
        };
        let popup = centered_rect(88, 76, area);
        clear_overlay(frame, popup);
        let title = if st.pending_reset_all {
            " Preferences  ·  clear all overrides? y/N "
        } else {
            " Preferences "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, Style::default().bold()));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let [cols, help] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(inner);
        let [left, right] =
            Layout::horizontal([Constraint::Length(14), Constraint::Min(20)]).areas(cols);

        let kitty = self.kitty_rc_prefs();
        let cats: Vec<PrefsCategory> = PrefsCategory::ALL
            .iter()
            .copied()
            .filter(|c| c.visible(kitty))
            .collect();
        let cat_items: Vec<ListItem> = cats
            .iter()
            .map(|c| {
                let mut style = Style::default();
                if *c == st.category {
                    style = style.fg(config::get().colors.ui.title_fg).bold();
                } else {
                    style = style.add_modifier(Modifier::DIM);
                }
                ListItem::new(Line::from(Span::styled(c.label(), style)))
            })
            .collect();
        let mut cat_state = ListState::default();
        cat_state.select(Some(
            cats.iter().position(|c| *c == st.category).unwrap_or(0),
        ));
        frame.render_stateful_widget(List::new(cat_items), left, &mut cat_state);

        let (rows, help_text) = self.prefs_right_lines(st);
        let items: Vec<ListItem> = rows
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let prefix = if st.pane == PrefsPane::Items && i == st.cursor {
                    config::get().colors.ui.selection_symbol.clone()
                } else {
                    "  ".into()
                };
                ListItem::new(Line::from(vec![Span::raw(prefix), Span::raw(line)]))
            })
            .collect();
        frame.render_widget(List::new(items), right);

        let mut help_lines = vec![Line::from(Span::styled(
            help_text,
            Style::default().add_modifier(Modifier::DIM),
        ))];
        if let Some(field) = st.field.as_ref() {
            help_lines.push(Line::from(format!("edit: {}", field.text())));
        }
        frame.render_widget(Paragraph::new(help_lines).wrap(Wrap { trim: true }), help);
    }

    fn prefs_right_lines(&self, st: &PrefsState) -> (Vec<String>, String) {
        let cfg = config::get();
        match st.category {
            PrefsCategory::Agents => {
                let rows = self
                    .agent_order
                    .iter()
                    .enumerate()
                    .map(|(i, (a, on))| {
                        let mark = if !*on {
                            "disabled"
                        } else if i == self.agent_order.iter().position(|(_, e)| *e).unwrap_or(0) {
                            "default"
                        } else {
                            ""
                        };
                        format!("{:<16} {mark}", a.label())
                    })
                    .collect();
                (
                    rows,
                    "First enabled is o/O/r default. J/K reorder. x disable. Ctrl-t cycles this list.".into(),
                )
            }
            PrefsCategory::General => {
                let layout_warn = if !self.capabilities.layout_is_a_choice() {
                    "  (this terminal only has per-tab)"
                } else {
                    ""
                };
                let sleep_warn = if !crate::sleep::supported() {
                    "  (no inhibitor on this machine)"
                } else {
                    ""
                };
                let pooled_warn = if !self.pooling_in_play() {
                    "  (no pooled sessions right now)"
                } else {
                    ""
                };
                let rows = vec![
                    format!(
                        "Session layout              {}{layout_warn}",
                        self.sessions_layout.label()
                    ),
                    format!(
                        "Keep-awake                  {}{sleep_warn}",
                        if self.prevent_sleep_enabled {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                    format!(
                        "On window close             {}{pooled_warn}",
                        match self.on_window_close {
                            OnWindowClose::Close => "close",
                            OnWindowClose::Detach => "detach",
                        }
                    ),
                ];
                (
                    rows,
                    "Writes ~/.local/state/captain-miao/dashboard-overrides.json. r resets a row."
                        .into(),
                )
            }
            PrefsCategory::Display => {
                let cap_warn = if !self.capabilities.capture {
                    "  (this terminal cannot capture preview)"
                } else {
                    ""
                };
                let rows = vec![
                    format!(
                        "Context warning             {}",
                        cfg.thresholds.context_warning_tokens
                    ),
                    format!(
                        "Context critical            {}",
                        cfg.thresholds.context_critical_tokens
                    ),
                    format!(
                        "Preview auto-refresh (s)    {}{cap_warn}",
                        cfg.polling.preview_auto_refresh_secs
                    ),
                    format!(
                        "Preview stale after (s)     {}{cap_warn}",
                        cfg.thresholds.preview_stale_secs
                    ),
                ];
                (
                    rows,
                    "-/+ step. Enter types a value. Absolute tokens only.".into(),
                )
            }
            PrefsCategory::Colors => {
                let rows = vec![
                    format!("Table highlight             {}", self.color_row_value(0)),
                    format!("Selection fg                {}", self.color_row_value(1)),
                    format!("Attention                   {}", self.color_row_value(2)),
                    format!("Error                       {}", self.color_row_value(3)),
                    "Reset colors to defaults".into(),
                ];
                (
                    rows,
                    "Enter edits named or #rrggbb. Reset clears color overrides.".into(),
                )
            }
            PrefsCategory::Terminal => {
                if !self.kitty_rc_prefs() {
                    return (Vec::new(), String::new());
                }
                let label = if self.extra_prefs.kitty_rc_password.is_none()
                    && cfg.kitty.rc_password == "i-am-the-captain-miao"
                {
                    "(default)"
                } else {
                    "(set)"
                };
                (
                    vec![format!("Kitty remote-control password  {label}")],
                    "Enter to edit in plaintext. Restart miao after changing.".into(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;

    #[test]
    fn prefer_agent_first_promotes_and_enables() {
        let preferred = AgentControl::from_cli("codex").expect("codex");
        let list = prefer_agent_first(default_agent_list(), preferred);
        assert_eq!(list[0].0, preferred);
        assert!(list[0].1, "typed default must be enabled");
        assert_eq!(first_enabled_agent(&list), preferred);
    }

    #[test]
    fn resolve_agent_list_keeps_unknown_out_and_appends_new_agents() {
        let prefs = [AgentPref {
            id: "codex".into(),
            enabled: false,
        }];
        let list = resolve_agent_list(Some(&prefs));
        assert_eq!(list[0].0, AgentControl::from_cli("codex").unwrap());
        assert!(!list[0].1);
        assert!(
            list.iter().any(|(a, on)| *a == AgentControl::Claude && *on),
            "agents missing from the persisted list stay enabled"
        );
        assert_eq!(list.len(), AgentControl::ALL.len());
    }
}
