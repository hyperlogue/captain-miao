//! Host-canonical path spelling — the **wire format** for every path that
//! crosses the backend seam (`docs/remote-sessions.md` §3).
//!
//! `$HOME` does not ride the wire. Instead the server collapses every path it
//! returns (`~`-prefixed when under the host home, absolute otherwise) and
//! expands `~` in every path it receives, so a path has exactly **one**
//! identity per host: `~/abc` simply *is* that path, never an alternate
//! spelling of an absolute twin. The client is fully home-ignorant — what it
//! displays is the wire string, and a submit round-trips it back verbatim. The
//! local backend applies the identical collapse/expand, so the in-process and
//! the wire arm are indistinguishable.
//!
//! The underlying assumption, now explicit: **single-user servers** — one
//! account, one home. A path under another user's home is left absolute.
//!
//! Two care points the design review flagged:
//!
//! * A `~`-form path must never be handed to a shell *single-quoted* — the
//!   quotes make the tilde inert. [`shell_quote_host_path`] is the one way to
//!   splice a host-canonical path into a remote command line.
//! * The collapse is a pure string operation on an already-absolute path, not a
//!   filesystem call, so it is safe on a path that doesn't exist yet.

/// Collapse `path` to its host-canonical spelling: `~`-prefixed when it lies
/// under `home`, unchanged otherwise. Idempotent — a path already in `~` form
/// passes through, which is what makes this safe to apply at every boundary.
///
/// The match is on a **path-component** boundary, so `/home/us` never collapses
/// against a `/home/user` home (a plain `starts_with` would have produced the
/// nonsense `~er`).
pub fn collapse_home(path: &str, home: &str) -> String {
    if home.is_empty() || path.starts_with('~') {
        return path.to_string();
    }
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Expand a host-canonical path against `home`: `~` → `home`, `~/x` →
/// `home/x`, anything else unchanged. Idempotent on an already-absolute path.
/// An empty `home` leaves `~` alone (there is nothing to expand it to).
pub fn expand_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    let home = home.trim_end_matches('/');
    if path == "~" {
        return home.to_string();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => path.to_string(),
    }
}

/// This host's `$HOME`, or empty when unset (then `~` is never expanded or
/// produced and paths stay absolute — the degraded-but-correct behavior).
pub fn host_home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Splice a host-canonical path into a POSIX shell command line so the remote
/// shell resolves `~` itself.
///
/// The landmine this closes: the natural `'{path}'` quoting makes a `~` inert,
/// so `cd '~/proj'` fails on every host. A `~`-form path is emitted as
/// `"$HOME"'/proj'` — the tilde becomes a shell expansion the *remote* performs,
/// while the remainder stays single-quoted, so spaces, globs and quotes in the
/// path are still inert. An absolute path is plain single-quoted.
pub fn shell_quote_host_path(path: &str) -> String {
    if path == "~" {
        return "\"$HOME\"".to_string();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", single_quote(rest)),
        None => single_quote(path),
    }
}

/// Single-quote `s` for a POSIX shell: wrap in `'…'` and rewrite each embedded
/// `'` as `'\''`, so an arbitrary path can't break out of the quoting.
pub fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_only_on_component_boundaries() {
        assert_eq!(collapse_home("/home/user/proj", "/home/user"), "~/proj");
        assert_eq!(collapse_home("/home/user", "/home/user"), "~");
        // A sibling dir sharing a prefix must NOT collapse into `~er/...`.
        assert_eq!(
            collapse_home("/home/username", "/home/user"),
            "/home/username"
        );
        assert_eq!(collapse_home("/etc", "/home/user"), "/etc");
        // A trailing slash on home is tolerated.
        assert_eq!(collapse_home("/home/user/x", "/home/user/"), "~/x");
        // No home → nothing collapses.
        assert_eq!(collapse_home("/home/user/x", ""), "/home/user/x");
    }

    #[test]
    fn collapse_is_idempotent_and_expand_inverts_it() {
        let home = "/home/user";
        for p in ["/home/user/proj", "/home/user", "/etc/passwd", "/"] {
            let once = collapse_home(p, home);
            assert_eq!(
                collapse_home(&once, home),
                once,
                "collapse not idempotent: {p}"
            );
            // The round trip is exact: collapse∘expand returns the original.
            assert_eq!(expand_home(&once, home), p, "round trip lost {p}");
        }
    }

    #[test]
    fn expand_is_idempotent_on_absolute_paths() {
        let home = "/home/user";
        for p in ["~", "~/proj", "/etc"] {
            let once = expand_home(p, home);
            assert_eq!(expand_home(&once, home), once);
        }
        // An unset home leaves `~` untouched rather than producing "/proj".
        assert_eq!(expand_home("~/proj", ""), "~/proj");
    }

    /// The canonical-spelling property the design leans on: for any path under
    /// home, `collapse` yields the single wire identity, and expanding it back
    /// on the host reproduces the original byte-for-byte — in both directions,
    /// for the awkward inputs (spaces, quotes, unicode, trailing slashes).
    #[test]
    fn collapse_expand_round_trips_for_awkward_paths() {
        let homes = ["/home/user", "/Users/Ada", "/root"];
        let tails = [
            "proj",
            "a b/c",
            "it's/a dir",
            "ünïcødé/paw",
            "deep/nest/ing/x",
            "trailing/",
            ".hidden",
        ];
        for home in homes {
            for tail in tails {
                let abs = format!("{home}/{tail}");
                let wire = collapse_home(&abs, home);
                assert!(wire.starts_with("~/"), "{abs} did not collapse: {wire}");
                assert_eq!(expand_home(&wire, home), abs);
                // And a path outside home survives both passes unchanged.
                let outside = format!("/opt/{tail}");
                assert_eq!(collapse_home(&outside, home), outside);
                assert_eq!(expand_home(&outside, home), outside);
            }
        }
    }

    #[test]
    fn shell_quoting_lets_the_remote_expand_the_tilde() {
        // The landmine: a single-quoted `~` would never expand.
        assert_eq!(shell_quote_host_path("~/proj"), "\"$HOME\"/'proj'");
        assert_eq!(shell_quote_host_path("~"), "\"$HOME\"");
        assert_eq!(shell_quote_host_path("/opt/x"), "'/opt/x'");
        // Quotes inside the path stay inert either way.
        assert_eq!(shell_quote_host_path("~/it's"), "\"$HOME\"/'it'\\''s'");
        assert_eq!(single_quote("/a b/it's"), r#"'/a b/it'\''s'"#);
    }
}
