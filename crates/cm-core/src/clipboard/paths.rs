//! Where the clipboard bridge's sockets live. There are **two** paths, and the
//! split is load-bearing rather than incidental.
//!
//! * The machine that *owns* the clipboard runs `clipboard serve`, which binds
//!   under [`crate::state::runtime_dir`] like every other runtime socket in the
//!   project ([`local_socket_path`]).
//! * The machine the *agent* runs on sees the far end of an `ssh -R` forward,
//!   and that one is **`$HOME`-based** ([`REMOTE_SOCKET_REL`]) — explicitly not
//!   the runtime dir.
//!
//! The natural implementation would have used `runtime_dir()` for both, and it
//! would have been silently dead on the primary target: on a systemd remote
//! `runtime_dir()` resolves under `$XDG_RUNTIME_DIR` while the dashboard, which
//! only learns the host's `$HOME` (from the connect probe), would have derived a
//! home-relative path — so the two ends would never meet. `/run/user/<uid>` is
//! also reaped when a non-lingering user's last login ends, which this tree
//! already warns about, and the pool outliving logins is the entire point.
//!
//! Keeping them *distinct* matters too, and the reason is a machine that is both
//! a dashboard and somebody's remote host — an ordinary dev box. If the local
//! bind used the home-relative path, an inbound `-R` from another dashboard
//! would try to bind the path our own live socket already holds.

use std::path::{Path, PathBuf};

/// The socket's basename, shared by both ends.
pub const SOCKET_NAME: &str = "clipboard.sock";

/// The dir holding the agent-side socket, relative to that host's `$HOME`. The
/// dashboard has to `mkdir -p` this before ssh can bind into it — ssh creates no
/// parent, as `ControlPath` taught us.
pub const REMOTE_DIR_REL: &str = ".cache/captain-miao";

/// The socket the agent's machine sees, relative to that host's `$HOME`: the far
/// end of the `-R` forward, and what the shim connects to. Named once here and
/// referenced from both the dashboard's transport and the shim.
pub const REMOTE_SOCKET_REL: &str = ".cache/captain-miao/clipboard.sock";

/// [`REMOTE_DIR_REL`] resolved against a specific host's `$HOME`.
///
/// Takes `home` rather than reading the environment because the caller that
/// needs it most is the *dashboard*, splicing a remote home the connect probe
/// reported into a remote command line.
pub fn remote_dir_for_home(home: &str) -> String {
    join_home(home, REMOTE_DIR_REL)
}

/// [`REMOTE_SOCKET_REL`] resolved against a specific host's `$HOME`. This is the
/// remote half of the `-R` forward spec, which must be **absolute**: ssh does no
/// `~` expansion in a forward spec.
pub fn remote_socket_for_home(home: &str) -> String {
    join_home(home, REMOTE_SOCKET_REL)
}

fn join_home(home: &str, rel: &str) -> String {
    format!("{}/{rel}", home.trim_end_matches('/'))
}

/// Where `clipboard serve` binds, on the machine that owns the clipboard.
pub fn local_socket_path() -> PathBuf {
    crate::state::runtime_dir().join(SOCKET_NAME)
}

/// Sockets the shim tries, in order, and the order is the interesting part.
///
/// The *local* runtime socket comes first so that a machine which is both a
/// dashboard and someone else's remote host answers with **its own** clipboard
/// rather than with whatever a foreign dashboard forwarded in. That ordering is
/// also what makes a pooled-localhost session work with no forward at all: the
/// pool daemon has no `DISPLAY`, but the socket is right there on the same
/// machine.
///
/// On an ordinary remote the first candidate simply doesn't exist, which costs
/// one failed `connect` before the real one.
pub fn shim_socket_candidates() -> Vec<PathBuf> {
    candidates_in(
        &crate::state::runtime_dir(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// [`shim_socket_candidates`] with both environment reads hoisted out, so the
/// ordering can be tested without touching process-wide state.
pub fn candidates_in(runtime_dir: &Path, home: Option<&str>) -> Vec<PathBuf> {
    let mut out = vec![runtime_dir.join(SOCKET_NAME)];
    if let Some(home) = home.filter(|h| !h.is_empty()) {
        let remote = PathBuf::from(remote_socket_for_home(home));
        if !out.contains(&remote) {
            out.push(remote);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_rel_paths_agree() {
        // They are separate literals (a `const` can't be `concat!`ed), so the
        // agreement is pinned here rather than by construction.
        assert_eq!(REMOTE_SOCKET_REL, format!("{REMOTE_DIR_REL}/{SOCKET_NAME}"));
        assert!(!REMOTE_SOCKET_REL.starts_with('/'), "must be home-relative");
    }

    #[test]
    fn resolving_against_a_home_yields_an_absolute_path() {
        assert_eq!(
            remote_socket_for_home("/home/miao"),
            "/home/miao/.cache/captain-miao/clipboard.sock"
        );
        assert_eq!(
            remote_dir_for_home("/home/miao"),
            "/home/miao/.cache/captain-miao"
        );
        // A trailing slash on the probed home must not double up: the path is
        // spliced into a remote command line, where `//` is legal but ugly and
        // would defeat any later string comparison against the forward spec.
        assert_eq!(
            remote_socket_for_home("/home/miao/"),
            "/home/miao/.cache/captain-miao/clipboard.sock"
        );
    }

    #[test]
    fn the_local_socket_is_never_the_remote_one() {
        // The collision this rules out is a dev box that is both a dashboard and
        // somebody's remote host: an inbound `-R` must not land on the path our
        // own server already holds.
        let home = "/home/miao";
        for runtime in [
            "/run/user/1000/captain-miao",
            "/home/miao/.local/state/captain-miao/run",
        ] {
            let c = candidates_in(Path::new(runtime), Some(home));
            assert_eq!(c.len(), 2, "both candidates expected: {c:?}");
            assert_ne!(c[0], c[1]);
            assert_eq!(c[0], Path::new(runtime).join(SOCKET_NAME));
            assert_eq!(c[1], PathBuf::from(remote_socket_for_home(home)));
        }
    }

    #[test]
    fn the_local_socket_comes_first() {
        // Our own clipboard wins over one a foreign dashboard forwarded in.
        let c = candidates_in(Path::new("/run/user/1000/captain-miao"), Some("/home/miao"));
        assert!(c[0].starts_with("/run/user/1000"));
    }

    #[test]
    fn a_missing_home_still_leaves_the_local_candidate() {
        for home in [None, Some("")] {
            let c = candidates_in(Path::new("/run/user/1000/captain-miao"), home);
            assert_eq!(
                c,
                vec![PathBuf::from("/run/user/1000/captain-miao/clipboard.sock")]
            );
        }
    }

    #[test]
    fn the_live_local_path_is_under_the_runtime_dir() {
        let p = local_socket_path();
        assert_eq!(p.file_name().unwrap(), SOCKET_NAME);
        assert_eq!(p.parent().unwrap(), crate::state::runtime_dir());
    }
}
