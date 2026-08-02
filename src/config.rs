//! The dashboard's configuration: the presentation sections (colors, ui,
//! thresholds, polling, keybinds) plus the launcher/debug sections it also reads,
//! which are reused from `cm-core`. Parses the same `config.toml` as the core
//! loader — serde ignores the sections each side doesn't know about.

use std::collections::HashMap;
use std::sync::OnceLock;

use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

// The `[launcher]`/`[debug]` sections + the loader path + `debug_enabled` live in
// core (the launcher/daemon read them too). Re-exported so `config::LauncherConfig`
// / `config::debug_enabled()` resolve unchanged across the dashboard.
pub use cm_core::config::{DebugConfig, LauncherConfig, config_path, debug_enabled};

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Lazily load the config from disk on first access, then reuse forever.
/// Any module can reach the config via `config::get()` without the loader
/// having to thread it through call sites.
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::load)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Terminal-backend selection. `backend` unset (the default) auto-detects;
    /// set it to pin Kitty or zellij. See `terminal::get`.
    pub terminal: TerminalConfig,
    pub kitty: KittyConfig,
    pub colors: ColorsConfig,
    pub ui: UiConfig,
    pub thresholds: ThresholdsConfig,
    pub polling: PollingConfig,
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
        let path = config_path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        // Parse errors fall back to defaults rather than killing the dashboard;
        // the user wouldn't see the error because the TUI takes over stderr.
        // Log via tracing for the launcher log, and also eprintln! because the
        // first config access can happen before a tracing subscriber exists
        // (and the dashboard with debug off never installs one).
        let mut cfg = match toml::from_str::<Self>(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("Failed to parse {}: {e}", path.display());
                eprintln!("captain-miao: failed to parse {}: {e}", path.display());
                // The whole file reverted to defaults — including [keybinds],
                // colors, and the kitty rc_password. Carry the reason so the
                // dashboard can surface it in its status line (see field doc).
                Self {
                    load_warning: Some(format!(
                        "config.toml failed to parse (using defaults): {e}"
                    )),
                    ..Self::default()
                }
            }
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

// -- terminal --

/// Terminal-backend selection. `backend` unset (the default) auto-detects:
/// zellij when `ZELLIJ_SESSION_NAME` is present, else Kitty. Pin it when the
/// env heuristic guesses wrong. Kitty-specific knobs (the remote-control
/// password) stay under `[kitty]`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    pub backend: Option<ConfiguredBackend>,
    /// Initial session layout: `"stacked"` (all sessions in the shared
    /// `cm:sessions` tab) or `"per-tab"` (one tab per session). Unset ⇒ stacked.
    /// Toggled at runtime with `Space l` and persisted in
    /// `dashboard-overrides.json`, which then wins over this value.
    pub sessions_layout: Option<crate::terminal::SessionsLayout>,
}

/// A `[terminal] backend` value. Serde-renamed so the config reads
/// `backend = "kitty"` / `"zellij"`; any other string fails the parse loudly
/// (the loader logs it and falls back to defaults) rather than silently picking
/// a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfiguredBackend {
    Kitty,
    Zellij,
}

// -- kitty --

#[derive(Debug, Deserialize)]
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

// -- colors --

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    pub ui: UiColors,
    pub picker: PickerColors,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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

// -- ui --

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub panels: PanelsConfig,
    pub table: TableConfig,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TableConfig {
    pub name_truncate: usize,
}

impl Default for TableConfig {
    fn default() -> Self {
        Self { name_truncate: 35 }
    }
}

// -- thresholds --

#[derive(Debug, Deserialize)]
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

// -- polling --

#[derive(Debug, Deserialize)]
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

// -- color parsing --

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
    fn terminal_backend_parses_and_defaults() {
        // Unset → None (get() auto-detects from the environment).
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.terminal.backend, None);
        // Explicit lowercase values pin a backend.
        let cfg: Config = toml::from_str("[terminal]\nbackend = \"zellij\"").unwrap();
        assert_eq!(cfg.terminal.backend, Some(ConfiguredBackend::Zellij));
        let cfg: Config = toml::from_str("[terminal]\nbackend = \"kitty\"").unwrap();
        assert_eq!(cfg.terminal.backend, Some(ConfiguredBackend::Kitty));
        // An unknown value fails the parse loudly rather than being ignored.
        assert!(toml::from_str::<Config>("[terminal]\nbackend = \"wezterm\"").is_err());
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
}
