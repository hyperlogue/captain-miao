//! Configuration outside the dashboard's presentation layer: the `[launcher]`
//! and `[debug]` sections (read by the launcher and the daemon), and the shared
//! loader. The dashboard's presentation config —
//! colors, thresholds, polling, keybinds, all ratatui-y — lives in the
//! `captain-miao` crate and layers on top, parsing the *same* `config.toml`
//! (serde ignores each side's unknown keys).
//!
//! **Living here does not mean the host reads it.** [`get`] resolves
//! [`config_path`] on whatever machine the process runs on, so a remote host
//! with a deployed `miao-server` has its own `config.toml` and the two never
//! meet. Which machine supplies a key is decided by where it is *read*, not by
//! which crate declares it: `[launcher]` is read from `cm-core`'s own backend
//! and launcher, so the **host** supplies it. `[remote]` is the mirror case and
//! is declared in the dashboard's `config.rs` for exactly that reason — it is
//! read only there. What stays here is [`is_valid_env_name`], which the
//! dashboard's loader and `cm-server`'s attach path both check against.
//! `docs/remote-sessions.md` §8 has the full table.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use serde::Deserialize;

static CONFIG: OnceLock<RwLock<Arc<CoreConfig>>> = OnceLock::new();

fn slot() -> &'static RwLock<Arc<CoreConfig>> {
    CONFIG.get_or_init(|| RwLock::new(Arc::new(CoreConfig::load())))
}

/// Read the in-memory core config. Does not hit disk — call [`reload`] for that.
pub fn get() -> Arc<CoreConfig> {
    slot()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Re-read `config_path()` into the process slot.
#[allow(dead_code)] // dashboard watchers call this once prefs land
pub fn reload() -> Arc<CoreConfig> {
    reload_from(&config_path())
}

/// Re-read `path` into the process slot.
#[allow(dead_code)] // see [`reload`]
pub fn reload_from(path: &Path) -> Arc<CoreConfig> {
    let cfg = Arc::new(CoreConfig::load_from(path));
    *slot().write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&cfg);
    cfg
}

/// Path to `config.toml`. Public so the dashboard's fuller loader reuses it
/// rather than duplicating the XDG resolution.
pub fn config_path() -> PathBuf {
    crate::state::xdg_dir("XDG_CONFIG_HOME")
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
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CoreConfig {
    pub launcher: LauncherConfig,
    pub debug: DebugConfig,
    pub codex: crate::agents::codex::CodexConfig,
}

/// Launch policy is read afresh on the execution host. A malformed setting
/// must fail a launch rather than quietly select the native adapter.
pub fn read_codex() -> anyhow::Result<crate::agents::codex::CodexConfig> {
    read_codex_at(&config_path())
}

fn read_codex_at(path: &Path) -> anyhow::Result<crate::agents::codex::CodexConfig> {
    use anyhow::Context;
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e).context("reading host Codex configuration"),
    };
    let config: CoreConfig = toml::from_str(&contents).context("parsing host configuration")?;
    config.codex.socket_path()?;
    Ok(config.codex)
}

/// The host editor changes only this table, preserving comments and unrelated
/// configuration. Serialize writers with an advisory lock on a separate inode
/// because the configuration itself is replaced atomically.
pub fn write_codex(config: &crate::agents::codex::CodexConfig) -> anyhow::Result<()> {
    write_codex_at(&config_path(), config)
}

fn write_codex_at(path: &Path, config: &crate::agents::codex::CodexConfig) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    config.socket_path()?;
    crate::state::create_dir_all_private(path.parent().context("configuration has no parent")?)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path.with_extension("lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking host configuration");
    }
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context("reading host configuration"),
    };
    let _: CoreConfig = toml::from_str(&contents).context("parsing host configuration")?;
    let mut doc: toml_edit::DocumentMut = contents.parse().context("parsing host configuration")?;
    for (key, text) in [
        ("mode", config.mode.label()),
        ("endpoint", config.endpoint.as_str()),
    ] {
        let slot = &mut doc["codex"][key];
        let mut value = toml_edit::Value::from(text);
        if let Some(previous) = slot.as_value() {
            *value.decor_mut() = previous.decor().clone();
        }
        *slot = toml_edit::Item::Value(value);
    }
    let tmp = path.with_extension("toml.tmp");
    crate::state::write_private(&tmp, &doc.to_string())?;
    std::fs::rename(&tmp, path).context("saving host Codex configuration")?;
    Ok(())
}

/// What [`load_toml_file`] found. Three outcomes, because "no file" and
/// "malformed file" are different things that happen to share a fallback.
pub enum TomlLoad<T> {
    Loaded(T),
    /// No file, or one we cannot read. Not an error — the whole config is
    /// optional, and every section defaults.
    Absent,
    /// Malformed. Already reported; the message is carried for the callers that
    /// also surface it to the user (the dashboard's startup status line).
    Failed(String),
}

/// Read and parse one TOML config file, reporting a parse failure exactly once.
///
/// Both `config.toml` loaders — this crate's `CoreConfig` and the dashboard's
/// fuller `Config` — go through here so the failure policy is stated in one
/// place: a malformed file **falls back to defaults rather than killing the
/// process**, because the TUI takes over stderr and a hard failure is a
/// dashboard that dies with no visible reason.
///
/// The report goes to `tracing::warn!` *and* `eprintln!` on purpose: the first
/// config access can happen before a tracing subscriber exists, and a dashboard
/// with debug off never installs one.
pub fn load_toml_file<T: serde::de::DeserializeOwned>(path: &Path) -> TomlLoad<T> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return TomlLoad::Absent;
    };
    match toml::from_str::<T>(&content) {
        Ok(v) => TomlLoad::Loaded(v),
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            eprintln!("captain-miao: failed to parse {}: {e}", path.display());
            TomlLoad::Failed(e.to_string())
        }
    }
}

impl CoreConfig {
    fn load() -> Self {
        Self::load_from(&config_path())
    }

    fn load_from(path: &Path) -> Self {
        let mut cfg = match load_toml_file::<Self>(path) {
            TomlLoad::Loaded(cfg) => cfg,
            TomlLoad::Absent | TomlLoad::Failed(_) => Self::default(),
        };
        cfg.launcher.migrate_legacy_titles();
        cfg
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

// =============================================================================
// launcher
// =============================================================================

/// The current default session-tab title template, applied verbatim to every
/// agent (`{agent}`/`{basename}`/`{cwd}` placeholders, expanded at spawn).
const DEFAULT_TAB_TITLE: &str = "{agent}: {basename}";
/// The pre-template shipped default `new_tab_title` — a Claude-specific literal.
const LEGACY_NEW_TAB_TITLE: &str = "Claude (new)";
/// The pre-template shipped default `resume_tab_title` — a Claude-specific literal.
const LEGACY_RESUME_TAB_TITLE: &str = "Claude (resume)";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
/// `[launcher]` in `config.toml`: how a launched session is run.
pub struct LauncherConfig {
    pub approval_grace_secs: u64,
    pub max_recent_cwds: usize,
    /// How many resumable sessions the `r` picker asks a host for, most-recent
    /// first. Deliberately small: the list is a *recency* affordance — you
    /// resume something you were just working on. It was 200, which on a remote
    /// host meant serialising two hundred transcript scans across ssh before the
    /// popup could show anything.
    ///
    /// It is a hard truncation **at the source**, not a display cap: the
    /// picker's filter runs client-side over exactly these items, so a session
    /// past the limit cannot be typed into view — raising this value is the only
    /// way to reach it.
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
            resume_list_limit: 50,
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

// =============================================================================
// debug
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
/// `[debug]` in `config.toml`: tracing knobs, off unless asked for.
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

// =============================================================================
// env-name validation (shared: the dashboard's loader and cm-server's attach)
// =============================================================================

/// True if `name` is a POSIX-shape environment variable name:
/// `[A-Za-z_][A-Za-z0-9_]*`.
///
/// Every name in `[remote] inherit_env` is checked against this, and the check
/// is **load-bearing in two places at once**, which is why it lives here rather
/// than at either use site:
///
/// * The dashboard splices each name into the argv of an `ssh <target>
///   miao-server attach …`, and ssh joins its command words with spaces and
///   hands the result to the account's **login shell** (see `remote_shell_argv`)
///   — so an unchecked name is remote command execution on every configured
///   host, from a file (`config.toml`) that is routinely templated or shared.
/// * The host end writes the names into a TOML `forward_env` array. libshpool
///   *fails the whole attach* on a config it cannot parse (`config::Manager::load`
///   returns `Err`, it does not skip), so a name carrying a character TOML
///   cannot round-trip takes the session down with it.
///
/// Both are closed by admitting only the shape a shell and TOML agree on. This
/// is exactly the set POSIX reserves for environment variable names, so nothing
/// legitimate is turned away.
pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    #[test]
    fn codex_settings_preserve_other_config_and_are_owned_by_each_host() {
        use super::*;
        use crate::agents::codex::{CodexConfig, CodexMode};
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("miao-codex-config-{}", std::process::id()));
        crate::state::create_dir_all_private(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = "# keep this comment\n[launcher]\napproval_grace_secs = 9\n\n[codex]\nmode = 'native' # keep this too\n";
        crate::state::write_private(&path, original).unwrap();
        let config = CodexConfig {
            mode: CodexMode::AppServer,
            endpoint: "unix://~/codex.sock".into(),
        };
        write_codex_at(&path, &config).unwrap();
        assert_eq!(read_codex_at(&path).unwrap(), config);
        assert_eq!(
            read_codex_at(&dir.join("another-host.toml")).unwrap(),
            CodexConfig::default()
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# keep this comment"));
        assert!(contents.contains("# keep this too"));
        assert!(contents.contains("approval_grace_secs = 9"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let invalid = CodexConfig {
            endpoint: "ws://localhost:1234".into(),
            ..config
        };
        assert!(write_codex_at(&path, &invalid).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        crate::state::write_private(&path, "[codex]\nmode = 'typo'\n").unwrap();
        assert!(
            read_codex_at(&path).is_err(),
            "invalid policy must not silently launch native"
        );
        assert!(write_codex_at(&path, &CodexConfig::default()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    use super::*;

    #[test]
    fn env_name_shape_is_posix() {
        for ok in ["A", "_", "ANTHROPIC_API_KEY", "_x9", "a1_B2"] {
            assert!(is_valid_env_name(ok), "{ok:?} should be accepted");
        }
        for bad in [
            "",
            "9LEADING_DIGIT",
            "HAS-DASH",
            "HAS SPACE",
            "HAS.DOT",
            "HAS$DOLLAR",
            "UNPRINTABLE\u{7f}",
            "NON_ASCII_É",
        ] {
            assert!(!is_valid_env_name(bad), "{bad:?} should be rejected");
        }
    }

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
