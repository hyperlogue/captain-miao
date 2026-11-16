//! Where this machine's user files live: the one owner of `HOME`, the XDG
//! base directories and the agents' own directory overrides.
//!
//! Every path to a user file — captain-miao's config, state and sockets, and
//! the agents' homes it reads transcripts from and writes hooks into — derives
//! from [`current`]. Nothing else reads those variables or asks `dirs` for a
//! home or cache directory (clippy's `disallowed-methods` enforces the `dirs`
//! half), so redirecting [`Roots`] redirects every such file.
//!
//! The roots are resolved from the environment once, on first use. Tests run
//! with private roots already (`.cargo/test-runner` gives every test binary a
//! private `HOME` and XDG directories); a test that needs roots of its own
//! installs them with [`install_for_test`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

/// The agents' directory overrides, captured with the rest so a test's
/// [`Roots`] has none and every agent directory falls under its home.
pub const AGENT_VARS: [&str; 10] = [
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "GROK_HOME",
    "KIMI_CODE_HOME",
    "OPENCODE_CONFIG_DIR",
    "PI_CODING_AGENT_DIR",
    "PI_CODING_AGENT_SESSION_DIR",
    "REASONIX_HOME",
    "REASONIX_STATE_HOME",
    "REASONIX_CACHE_HOME",
];

/// The user directories one process works with. Unset and empty variables are
/// both `None`: the XDG rule that an empty value means unset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roots {
    home: Option<PathBuf>,
    config: Option<PathBuf>,
    state: Option<PathBuf>,
    cache: Option<PathBuf>,
    runtime: Option<PathBuf>,
    agents: HashMap<&'static str, PathBuf>,
}

impl Roots {
    /// The roots this process's environment names.
    #[allow(clippy::disallowed_methods)] // The one place allowed to ask.
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            // `dirs` falls back to the password database without `HOME`.
            home: var("HOME").or_else(dirs::home_dir),
            config: var("XDG_CONFIG_HOME"),
            state: var("XDG_STATE_HOME"),
            cache: var("XDG_CACHE_HOME"),
            runtime: var("XDG_RUNTIME_DIR"),
            agents: AGENT_VARS
                .iter()
                .filter_map(|name| Some((*name, var(name)?)))
                .collect(),
        }
    }

    /// Roots entirely below `dir`, with no agent overrides.
    pub fn under(dir: &Path) -> Self {
        Self {
            home: Some(dir.join("home")),
            config: Some(dir.join("config")),
            state: Some(dir.join("state")),
            cache: Some(dir.join("cache")),
            runtime: Some(dir.join("run")),
            agents: HashMap::new(),
        }
    }

    /// `$HOME`, falling back to the account's home directory.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// `$XDG_CONFIG_HOME`, else `~/.config` (`/.config` with no home at all).
    pub fn config_home(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(|| {
            self.home
                .as_deref()
                .unwrap_or(Path::new("/"))
                .join(".config")
        })
    }

    /// `$XDG_STATE_HOME`, else `~/.local/state` (`/tmp/.local/state` with no
    /// home at all).
    pub fn state_home(&self) -> PathBuf {
        self.state.clone().unwrap_or_else(|| {
            self.home
                .as_deref()
                .unwrap_or(Path::new("/tmp"))
                .join(".local/state")
        })
    }

    /// The platform's per-user cache directory: `~/Library/Caches` on macOS,
    /// else `$XDG_CACHE_HOME` or `~/.cache`.
    pub fn cache_home(&self) -> Option<PathBuf> {
        if cfg!(target_os = "macos") {
            return self.home.as_deref().map(|h| h.join("Library/Caches"));
        }
        self.cache
            .clone()
            .or_else(|| self.home.as_deref().map(|h| h.join(".cache")))
    }

    /// `$XDG_RUNTIME_DIR`, which macOS never sets.
    pub fn runtime(&self) -> Option<&Path> {
        self.runtime.as_deref()
    }

    /// The directory an agent's override variable names, if the user set one.
    /// `name` must be one of [`AGENT_VARS`].
    pub fn agent_var(&self, name: &str) -> Option<&Path> {
        debug_assert!(AGENT_VARS.contains(&name), "{name} is not in AGENT_VARS");
        self.agents.get(name).map(PathBuf::as_path)
    }

    /// `rel` below the home directory.
    pub fn in_home(&self, rel: &str) -> Option<PathBuf> {
        self.home.as_deref().map(|h| h.join(rel))
    }
}

static CURRENT: RwLock<Option<Arc<Roots>>> = RwLock::new(None);

/// This process's roots, resolved from the environment on first use.
pub fn current() -> Arc<Roots> {
    if let Some(roots) = &*CURRENT.read().unwrap_or_else(PoisonError::into_inner) {
        return Arc::clone(roots);
    }
    let mut slot = CURRENT.write().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(slot.get_or_insert_with(|| Arc::new(Roots::from_env())))
}

/// [`Roots::home`] of the [`current`] roots.
pub fn home() -> Option<PathBuf> {
    current().home().map(Path::to_path_buf)
}

/// Replace the [`current`] roots until the guard drops. Tests installing roots
/// run one at a time; a test that does not install any reads whichever roots
/// are current, which are private either way.
#[cfg(any(test, feature = "test-support"))]
pub fn install_for_test(roots: Roots) -> TestRoots {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let previous = CURRENT
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(Arc::new(roots));
    TestRoots {
        previous,
        _lock: lock,
    }
}

/// Restores the previous roots on drop; see [`install_for_test`].
#[cfg(any(test, feature = "test-support"))]
pub struct TestRoots {
    previous: Option<Arc<Roots>>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestRoots {
    fn drop(&mut self) {
        *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = self.previous.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test runner isolated this process: every root it resolved is
    /// private. A failure here means tests ran without `.cargo/test-runner`
    /// and may have used the developer's own files.
    #[test]
    fn tests_run_with_private_roots() {
        let root = std::env::var_os("CAPTAIN_MIAO_TEST_ROOT")
            .map(PathBuf::from)
            .expect(
                "tests must run through cargo, whose runner (.cargo/test-runner) isolates them",
            );
        let roots = Roots::from_env();
        let private = |path: &Path| path.starts_with(&root);
        assert!(roots.home().is_some_and(private));
        assert!(private(&roots.config_home()));
        assert!(private(&roots.state_home()));
        assert!(roots.cache_home().is_some_and(|p| private(&p)));
        if cfg!(target_os = "linux") {
            assert!(roots.runtime().is_some_and(private));
        }
        assert!(roots.agents.is_empty(), "{:?}", roots.agents);
    }

    #[test]
    fn installed_roots_apply_until_the_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let before = current();
        {
            let _guard = install_for_test(Roots::under(dir.path()));
            assert_eq!(home().as_deref(), Some(dir.path().join("home").as_path()));
            assert_eq!(current().state_home(), dir.path().join("state"));
        }
        assert_eq!(current(), before);
    }

    #[test]
    fn fallbacks_follow_the_home() {
        let roots = Roots {
            home: Some("/h".into()),
            ..Roots::default()
        };
        assert_eq!(roots.config_home(), Path::new("/h/.config"));
        assert_eq!(roots.state_home(), Path::new("/h/.local/state"));
        assert_eq!(roots.in_home(".grok"), Some("/h/.grok".into()));
        assert_eq!(roots.runtime(), None);
        assert_eq!(Roots::default().config_home(), Path::new("/.config"));
        assert_eq!(
            Roots::default().state_home(),
            Path::new("/tmp/.local/state")
        );
    }
}
