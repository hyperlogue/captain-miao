//! Per-backend implementations dispatched to from `crate::agent::AgentControl`.

use std::path::{Path, PathBuf};
use tokio::process::Command;

pub mod claude;
pub mod codex;

// Small helpers shared by the backend modules. They live here (rather than
// duplicated in each backend) because both `claude` and `codex` need byte-for-
// byte the same behaviour and had drifted as copies.

/// Collapse every run of whitespace to a single space and trim the ends, so a
/// transcript title / prompt rendered in the dashboard never carries stray
/// newlines or runs of spaces.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First entry on `$PATH` that is `name` and is a file, or `None`. Used to
/// resolve the agent binary (and `direnv`) without shelling out to `which`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
}

/// Put the clipboard shim farm at the front of the agent's `PATH`, when there is
/// one.
///
/// Scoped to the agent process — not the user's shell, which is what an rc-file
/// edit would change. That single difference is what removes a whole class of
/// questions: which rc file a login shell reads, whether the pool's shell is
/// interactive, and why an already-running session keeps a stale `PATH`.
///
/// Caveat worth knowing at the call site: `direnv` is in the spawn path, and an
/// `.envrc` that sets `PATH` absolutely can drop this dir again. The shim's
/// delegate rule makes that "paste stops working", not "the session breaks".
fn with_shim_path(cmd: &mut Command, shim_dir: Option<&Path>) {
    let Some(dir) = shim_dir else { return };
    if let Some(path) = shim_path(dir, std::env::var_os("PATH").as_deref()) {
        cmd.env("PATH", path);
    }
}

/// `dir`, then `existing`. Pure, so the ordering — the whole mechanism — is
/// pinned by a test rather than by reading the call site.
fn shim_path(dir: &Path, existing: Option<&std::ffi::OsStr>) -> Option<std::ffi::OsString> {
    let mut dirs = vec![dir.to_path_buf()];
    // An *empty* `PATH` splits into one empty component, and joining that back on
    // would append a trailing `:` — which POSIX reads as the cwd. Prepending a dir
    // must not quietly add the working directory to the agent's search path. A
    // non-empty `PATH` is carried over verbatim, empty components included: those
    // are the user's, not ours to reinterpret.
    if let Some(existing) = existing.filter(|e| !e.is_empty()) {
        dirs.extend(std::env::split_paths(existing));
    }
    std::env::join_paths(dirs).ok()
}

/// Single-quote a string for safe embedding in a `/bin/sh` command line, so an
/// exe or socket path with spaces or shell metacharacters can't word-split or
/// inject. Wraps in `'…'` and escapes any embedded `'` as `'\''`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// The shim dir goes **first**, which is the entire mechanism: the agent's
    /// `xclip` has to resolve to our symlink rather than to a real tool the host
    /// may also have.
    #[test]
    fn the_shim_dir_goes_ahead_of_the_inherited_path() {
        let dir = Path::new("/home/miao/.cache/captain-miao/shims");
        let path = shim_path(dir, Some(OsStr::new("/usr/bin:/bin"))).unwrap();
        assert_eq!(
            path,
            OsStr::new("/home/miao/.cache/captain-miao/shims:/usr/bin:/bin")
        );
        // Nothing inherited is dropped — the agent still finds its own binary.
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs[0], dir);
        // An empty or absent inherited PATH still yields a usable one.
        assert_eq!(shim_path(dir, None).unwrap(), dir.as_os_str());
        assert_eq!(
            shim_path(dir, Some(OsStr::new(""))).unwrap(),
            dir.as_os_str()
        );
    }

    /// `None` must leave `PATH` alone rather than setting it to the inherited
    /// value: a local windowed session is not shimmed, and re-exporting `PATH`
    /// would be a silent behaviour change for it.
    #[test]
    fn no_shim_dir_touches_no_environment() {
        let mut cmd = Command::new("/bin/true");
        with_shim_path(&mut cmd, None);
        assert!(
            cmd.as_std().get_envs().next().is_none(),
            "an unshimmed launch must set no env"
        );

        let dir = Path::new("/home/miao/.cache/captain-miao/shims");
        let mut cmd = Command::new("/bin/true");
        with_shim_path(&mut cmd, Some(dir));
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, OsStr::new("PATH"));
        let value = envs[0].1.expect("PATH is set, not cleared");
        assert!(
            std::env::split_paths(value).next().as_deref() == Some(dir),
            "the shim dir must come first: {value:?}"
        );
    }
}
