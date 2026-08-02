//! Per-backend implementations dispatched to from `crate::agent::AgentControl`.

use std::path::PathBuf;

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

/// Single-quote a string for safe embedding in a `/bin/sh` command line, so an
/// exe or socket path with spaces or shell metacharacters can't word-split or
/// inject. Wraps in `'…'` and escapes any embedded `'` as `'\''`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
