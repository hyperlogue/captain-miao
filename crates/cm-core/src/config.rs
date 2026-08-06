//! Configuration the non-UI side reads: the `[launcher]` and `[debug]` sections
//! (used by the launcher and the daemon) plus the shared loader. The dashboard's
//! presentation config — colors, thresholds, polling, keybinds, all ratatui-y —
//! lives in the `captain-miao` crate and layers on top, parsing the *same*
//! `config.toml` (serde ignores each side's unknown keys).

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

static CONFIG: OnceLock<CoreConfig> = OnceLock::new();

/// Lazily load the core config from disk on first access, then reuse forever.
/// Any core module reaches it via `config::get()` without threading it through.
pub fn get() -> &'static CoreConfig {
    CONFIG.get_or_init(CoreConfig::load)
}

/// Path to `config.toml`. Public so the dashboard's fuller loader reuses it
/// rather than duplicating the XDG resolution.
pub fn config_path() -> PathBuf {
    // Per the XDG spec an empty env var is treated as unset, not as a
    // relative path, so filter out the empty string before falling back.
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join(".config")
        })
        .join("captain-miao")
        .join("config.toml")
}

/// The launcher/daemon's view of `config.toml`: only the sections they read.
/// The dashboard's `[colors]`/`[ui]`/… are unknown keys here and serde skips them.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    pub launcher: LauncherConfig,
    pub debug: DebugConfig,
}

impl CoreConfig {
    fn load() -> Self {
        let path = config_path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        // Parse errors fall back to defaults rather than killing the process;
        // log via tracing and also eprintln! because the first config access can
        // happen before a tracing subscriber exists.
        match toml::from_str::<Self>(&content) {
            Ok(mut cfg) => {
                cfg.launcher.migrate_legacy_titles();
                cfg
            }
            Err(e) => {
                tracing::warn!("Failed to parse {}: {e}", path.display());
                eprintln!("captain-miao: failed to parse {}: {e}", path.display());
                Self::default()
            }
        }
    }
}

/// `CAPTAIN_MIAO_DEBUG=1` (or `true`) flips debug mode on regardless of
/// config so we can poke at a one-off run without editing config.toml.
pub fn debug_enabled() -> bool {
    matches!(
        std::env::var("CAPTAIN_MIAO_DEBUG").as_deref(),
        Ok("1") | Ok("true")
    ) || get().debug.enabled
}

// -- launcher --

/// The current default session-tab title template, applied verbatim to every
/// agent (`{agent}`/`{basename}`/`{cwd}` placeholders, expanded at spawn).
const DEFAULT_TAB_TITLE: &str = "{agent}: {basename}";
/// The pre-template shipped default `new_tab_title` — a Claude-specific literal.
const LEGACY_NEW_TAB_TITLE: &str = "Claude (new)";
/// The pre-template shipped default `resume_tab_title` — a Claude-specific literal.
const LEGACY_RESUME_TAB_TITLE: &str = "Claude (resume)";

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct LauncherConfig {
    pub approval_grace_secs: u64,
    pub max_recent_cwds: usize,
    pub resume_list_limit: usize,
    /// Title templates for the tab a new / resumed session opens in.
    /// Placeholders: `{agent}` → the backend label ("Claude"/"Codex"),
    /// `{basename}` → the session cwd's last path component, `{cwd}` → the
    /// full cwd.
    pub new_tab_title: String,
    pub resume_tab_title: String,
    /// Backend used for new sessions (`o` / `O`) until toggled with `Space a`.
    /// One of "claude" or "codex"; unknown values fall back to claude.
    pub default_agent: String,
    /// **Pooled-localhost** (`docs/remote-sessions.md` §10.1): run this
    /// machine's sessions inside the local pty pool instead of spawning them
    /// directly into terminal windows, and have the dashboard reach them
    /// through its own daemon like any other host.
    ///
    /// Opt-in, and the two modes are permanent, chosen by machine role:
    ///
    /// * **Laptops stay direct-local** (the default). Nobody remotes into a
    ///   laptop, so the pool buys no persistence there — only an extra process
    ///   hop, no scrollback replay on reattach, and single-attach.
    /// * **Dev servers want pooled-local.** They have two kinds of consumer
    ///   needing the *same* attachable sessions: a laptop dashboard over the
    ///   protocol, and someone who sshs in from a phone and runs captain-miao
    ///   inside zellij on the box. Pooling makes both of them ordinary attach
    ///   clients, and sessions then survive a zellij crash and a seat logout.
    ///
    /// Needs `miao-server` on PATH; without it the dashboard logs the
    /// problem and falls back to direct-local rather than starting empty.
    pub pooled: bool,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            approval_grace_secs: 2,
            max_recent_cwds: 50,
            resume_list_limit: 200,
            new_tab_title: DEFAULT_TAB_TITLE.to_string(),
            resume_tab_title: DEFAULT_TAB_TITLE.to_string(),
            default_agent: "claude".to_string(),
            pooled: false,
        }
    }
}

impl LauncherConfig {
    /// Migrate the pre-template shipped default tab titles to the current
    /// template.
    ///
    /// Before the titles were templated, the shipped defaults were the
    /// Claude-specific literals "Claude (new)" / "Claude (resume)", and the old
    /// spawn code special-cased non-Claude agents so a Codex tab was never
    /// actually titled "Claude". The template rework applies the configured
    /// value verbatim to *every* agent, so a user config still carrying those
    /// copied-in literals would title Codex sessions "Claude". Treat an exact
    /// legacy literal as unset and restore the default template; a genuinely
    /// custom title is respected verbatim (and, as documented, applies to all
    /// agents). Idempotent and run on every load, so both loaders (core here,
    /// the dashboard's presentation config) share the one migration.
    pub fn migrate_legacy_titles(&mut self) {
        if self.new_tab_title == LEGACY_NEW_TAB_TITLE {
            self.new_tab_title = DEFAULT_TAB_TITLE.to_string();
        }
        if self.resume_tab_title == LEGACY_RESUME_TAB_TITLE {
            self.resume_tab_title = DEFAULT_TAB_TITLE.to_string();
        }
    }
}

// -- debug --

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    /// Master switch for verbose debug logging. When on, the launcher,
    /// dashboard, and per-event hook subprocess all append to a shared
    /// `debug.log` and the dashboard records every keystroke to
    /// `keybinds.log` for frequency analysis. Both files live in
    /// `~/.local/state/captain-miao/logs/` next to the existing
    /// `launcher-{pid}.log` files.
    pub enabled: bool,
    pub log_file: String,
    pub keybind_log_file: String,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            log_file: "debug.log".to_string(),
            keybind_log_file: "keybinds.log".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_literal_titles_migrate_to_default() {
        let mut cfg = LauncherConfig {
            new_tab_title: LEGACY_NEW_TAB_TITLE.to_string(),
            resume_tab_title: LEGACY_RESUME_TAB_TITLE.to_string(),
            ..LauncherConfig::default()
        };
        cfg.migrate_legacy_titles();
        assert_eq!(cfg.new_tab_title, DEFAULT_TAB_TITLE);
        assert_eq!(cfg.resume_tab_title, DEFAULT_TAB_TITLE);
    }

    #[test]
    fn custom_and_template_titles_pass_through_untouched() {
        let mut cfg = LauncherConfig {
            new_tab_title: "my title".to_string(),
            resume_tab_title: "{agent}: {basename}".to_string(),
            ..LauncherConfig::default()
        };
        cfg.migrate_legacy_titles();
        assert_eq!(cfg.new_tab_title, "my title");
        assert_eq!(cfg.resume_tab_title, "{agent}: {basename}");
    }
}
