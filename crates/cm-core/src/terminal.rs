//! Terminal window/tab identifiers, shared across captain-miao.
//!
//! These opaque ids live in `cm-core` — not the dashboard's `terminal` backend —
//! because they are serialized into [`crate::state::LauncherState`] and ride the
//! wire protocol, and because the launcher self-reports its own window via
//! [`current_window`], the one terminal touch a (possibly headless) launcher
//! needs. The full `Terminal` trait, the Kitty backend, and the snapshot policy
//! stay in the dashboard crate, which re-exports these types.

use serde::{Deserialize, Serialize};

/// Opaque handle to one window/pane. Stored as a string so numeric-id backends
/// (Kitty, WezTerm) and string-id backends (iTerm UUIDs, tmux `%3`) both fit.
///
/// Serializes as a JSON string; deserializes from *either* a string or an
/// integer (see [`deserialize_id`]). The integer path keeps pre-abstraction
/// state files — which wrote `window_id`/`tab_id` as JSON numbers — readable, so
/// upgrading captain-miao while a session is live doesn't make the row vanish.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WindowId(pub String);

/// Opaque handle to one tab. Same string-or-integer deserialization as
/// [`WindowId`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TabId(pub String);

/// Deserialize an opaque id from either a JSON string (current format) or a JSON
/// integer (pre-abstraction format, where ids were `u64`). Coercing the integer
/// to its decimal string keeps old state/snapshot files parseable.
fn deserialize_id<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct IdVisitor;
    impl serde::de::Visitor<'_> for IdVisitor {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a window/tab id as a string or integer")
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<String, E> {
            Ok(v)
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<String, E> {
            Ok(v.to_string())
        }
    }
    deserializer.deserialize_any(IdVisitor)
}

impl<'de> Deserialize<'de> for WindowId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        deserialize_id(d).map(WindowId)
    }
}

impl<'de> Deserialize<'de> for TabId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        deserialize_id(d).map(TabId)
    }
}

impl WindowId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TabId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<u64> for WindowId {
    fn from(n: u64) -> Self {
        WindowId(n.to_string())
    }
}

impl From<u64> for TabId {
    fn from(n: u64) -> Self {
        TabId(n.to_string())
    }
}

impl std::str::FromStr for WindowId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(WindowId(s.to_string()))
    }
}

/// The terminal *instance* identity and the window/pane of the current process,
/// read from one env snapshot so the two can never disagree. `current_window`
/// and `current_terminal_identity` each expose one half; sharing this read is
/// what keeps a window id and the namespace it belongs to from drifting apart.
///
/// zellij is checked *first*: when zellij runs nested inside Kitty, every zellij
/// pane inherits the outer `KITTY_WINDOW_ID`, so a launcher must self-report its
/// own zellij pane, not the shared outer Kitty window.
fn terminal_env() -> (Option<String>, Option<WindowId>) {
    let read = |k: &str| std::env::var(k).ok();
    resolve_terminal_env(
        read("ZELLIJ_PANE_ID"),
        read("ZELLIJ_SESSION_NAME"),
        read("KITTY_WINDOW_ID"),
        read("KITTY_LISTEN_ON"),
        read("KITTY_PID"),
    )
}

/// Pure core of [`terminal_env`], taking the five env values so the precedence is
/// unit-testable without touching process env. Returns `(identity, window)`:
/// - a zellij pane *with* a session name ⇒ (`zellij:<session>`, pane id). Without
///   a session name a pane can't be namespaced to an instance, so the zellij env
///   is treated as absent and Kitty is consulted (both halves fall through).
/// - else a Kitty window ⇒ (`kitty:<KITTY_LISTEN_ON>`, falling back to
///   `kitty:<KITTY_PID>`; `None` when neither is set — window id).
/// - else `(None, None)`.
///
/// Env values are trimmed (both vars are bare integer strings, so trimming is
/// purely defensive) and an empty value counts as absent.
fn resolve_terminal_env(
    zellij_pane: Option<String>,
    zellij_session: Option<String>,
    kitty_window: Option<String>,
    kitty_listen: Option<String>,
    kitty_pid: Option<String>,
) -> (Option<String>, Option<WindowId>) {
    let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let (Some(pane), Some(session)) = (clean(zellij_pane), clean(zellij_session)) {
        return (Some(zellij_identity(&session)), Some(WindowId(pane)));
    }
    if let Some(window) = clean(kitty_window) {
        return (
            kitty_identity(kitty_listen, kitty_pid),
            Some(WindowId(window)),
        );
    }
    (None, None)
}

/// The window/pane the current process is running in, from the terminal's env
/// var (zellij: `ZELLIJ_PANE_ID`; Kitty: `KITTY_WINDOW_ID`). `None` outside a
/// managed window.
///
/// A free function rather than a `Terminal` trait method so a headless/core
/// context (the launcher) can self-report its window without the backend. Keeps
/// the trimmed env value verbatim rather than round-tripping through `u64` —
/// `WindowId` is an opaque string, and this matches how the dashboard
/// reconstructs the same id from disk.
pub fn current_window() -> Option<WindowId> {
    terminal_env().1
}

/// This process's terminal *instance* identity — `zellij:<ZELLIJ_SESSION_NAME>`
/// or `kitty:<KITTY_LISTEN_ON|KITTY_PID>`, `None` outside a managed terminal.
/// Instance-granular so two zellij sessions (each numbering panes 1,2,3…) or two
/// Kitty instances never collide: it namespaces the otherwise-overlapping
/// [`WindowId`]s so window ops from one terminal can't target another's windows.
/// Same zellij-first precedence as [`current_window`], derived from the same env
/// read, so an id and its namespace stay in lock-step.
pub fn current_terminal_identity() -> Option<String> {
    terminal_env().0
}

/// The identity of a zellij session instance. The single constructor for the
/// `zellij:<session>` form — used by the env read above and by the dashboard's
/// zellij backend (which knows the session it drives), so the two can't drift.
pub fn zellij_identity(session: &str) -> String {
    format!("zellij:{session}")
}

/// The identity of a Kitty instance, keyed by its remote-control socket (what a
/// backend actually drives), falling back to the kitty pid. The single
/// constructor for the `kitty:<key>` form — see [`zellij_identity`].
pub fn kitty_identity(listen_on: Option<String>, pid: Option<String>) -> Option<String> {
    let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    clean(listen_on)
        .or_else(|| clean(pid))
        .map(|key| format!("kitty:{key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zellij_beats_kitty_and_namespaces_by_session() {
        // A zellij pane wins over the ambient (possibly outer-Kitty) env, and the
        // identity carries the session so two sessions' pane ids don't collide.
        let (id, win) = resolve_terminal_env(
            Some("3".into()),
            Some("work".into()),
            Some("7".into()),
            Some("unix:/tmp/k".into()),
            Some("999".into()),
        );
        assert_eq!(id.as_deref(), Some("zellij:work"));
        assert_eq!(win, Some(WindowId("3".into())));
    }

    #[test]
    fn zellij_pane_without_session_falls_through_to_kitty() {
        // No session name ⇒ the pane can't be namespaced to an instance ⇒ the
        // whole zellij env is treated as absent (both halves fall through).
        let (id, win) = resolve_terminal_env(
            Some("3".into()),
            None,
            Some("7".into()),
            Some("unix:/tmp/k".into()),
            None,
        );
        assert_eq!(id.as_deref(), Some("kitty:unix:/tmp/k"));
        assert_eq!(win, Some(WindowId("7".into())));

        // An empty/whitespace session name counts as missing.
        let (id, _) = resolve_terminal_env(
            Some("3".into()),
            Some("   ".into()),
            Some("7".into()),
            None,
            Some("999".into()),
        );
        assert_eq!(id.as_deref(), Some("kitty:999"));
    }

    #[test]
    fn kitty_identity_prefers_listen_socket_then_pid() {
        let (id, win) = resolve_terminal_env(
            None,
            None,
            Some("7".into()),
            Some("unix:/tmp/k".into()),
            Some("999".into()),
        );
        assert_eq!(id.as_deref(), Some("kitty:unix:/tmp/k"));
        assert_eq!(win, Some(WindowId("7".into())));

        // No listen socket ⇒ fall back to the pid.
        let (id, _) = resolve_terminal_env(None, None, Some("7".into()), None, Some("999".into()));
        assert_eq!(id.as_deref(), Some("kitty:999"));

        // A Kitty window with neither key yields a window but no identity.
        let (id, win) = resolve_terminal_env(None, None, Some("7".into()), None, None);
        assert_eq!(id, None);
        assert_eq!(win, Some(WindowId("7".into())));
    }

    #[test]
    fn no_terminal_env_is_none() {
        assert_eq!(
            resolve_terminal_env(None, None, None, None, None),
            (None, None)
        );
    }
}
