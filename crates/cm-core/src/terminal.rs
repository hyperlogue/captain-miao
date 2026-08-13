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

/// The terminal-identifying environment variables, as one snapshot. A named
/// struct rather than a positional argument list: every field is an
/// `Option<String>`, so a mis-ordered call would type-check and then silently
/// namespace a window id to the wrong terminal.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalEnv {
    pub zellij_pane: Option<String>,
    pub zellij_session: Option<String>,
    /// `TMUX`, whose value is `<socket_path>,<server_pid>,<session_id>`.
    pub tmux: Option<String>,
    /// `TMUX_PANE` (`%N`).
    pub tmux_pane: Option<String>,
    pub kitty_window: Option<String>,
    pub kitty_listen: Option<String>,
    pub kitty_pid: Option<String>,
    /// `TERM_PROGRAM` — Ghostty's only self-identifying variable. It exports no
    /// per-surface id at all, which is why the Ghostty arm below yields an
    /// identity and no window (see [`ghostty_identity`]).
    pub term_program: Option<String>,
}

/// The terminal *instance* identity and the window/pane of the current process,
/// read from one env snapshot so the two can never disagree. `current_window`
/// and `current_terminal_identity` each expose one half; sharing this read is
/// what keeps a window id and the namespace it belongs to from drifting apart.
///
/// zellij is checked *first*, then tmux: when either runs nested inside Kitty,
/// every pane inherits the outer `KITTY_WINDOW_ID`, so a launcher must
/// self-report its own pane, not the shared outer Kitty window.
fn terminal_env() -> (Option<String>, Option<WindowId>) {
    let read = |k: &str| std::env::var(k).ok();
    resolve_terminal_env(TerminalEnv {
        zellij_pane: read("ZELLIJ_PANE_ID"),
        zellij_session: read("ZELLIJ_SESSION_NAME"),
        tmux: read("TMUX"),
        tmux_pane: read("TMUX_PANE"),
        kitty_window: read("KITTY_WINDOW_ID"),
        kitty_listen: read("KITTY_LISTEN_ON"),
        kitty_pid: read("KITTY_PID"),
        term_program: read("TERM_PROGRAM"),
    })
}

/// Pure core of [`terminal_env`], taking the env values so the precedence is
/// unit-testable without touching process env. Returns `(identity, window)`:
/// - a zellij pane *with* a session name ⇒ (`zellij:<session>`, pane id). Without
///   a session name a pane can't be namespaced to an instance, so the zellij env
///   is treated as absent and the next backend is consulted (both halves fall
///   through).
/// - else a tmux pane *with* a parseable `TMUX` ⇒ (`tmux:<socket>,<pid>`, `%N`),
///   with the same both-halves-or-neither rule.
/// - else a Kitty window ⇒ (`kitty:<KITTY_LISTEN_ON>`, falling back to
///   `kitty:<KITTY_PID>`; `None` when neither is set — window id).
/// - else `TERM_PROGRAM=ghostty` ⇒ (`ghostty`, **no window**). Ghostty is the
///   one supported terminal that exports nothing per surface, so this half is
///   structurally `None`; the dashboard's backend recovers *its own* surface a
///   different way (`ttyname` → the AppleScript `tty` property) and every
///   dashboard-spawned session is bound from its `SpawnResult` instead.
/// - else `(None, None)`.
///
/// Env values are trimmed (most are bare integer strings, so trimming is
/// purely defensive) and an empty value counts as absent.
fn resolve_terminal_env(env: TerminalEnv) -> (Option<String>, Option<WindowId>) {
    let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let (Some(pane), Some(session)) = (clean(env.zellij_pane), clean(env.zellij_session)) {
        return (Some(zellij_identity(&session)), Some(WindowId(pane)));
    }
    if let (Some(pane), Some(identity)) = (
        clean(env.tmux_pane),
        clean(env.tmux).as_deref().and_then(tmux_identity),
    ) {
        return (Some(identity), Some(WindowId(pane)));
    }
    if let Some(window) = clean(env.kitty_window) {
        return (
            kitty_identity(env.kitty_listen, env.kitty_pid),
            Some(WindowId(window)),
        );
    }
    // Last, because it is the weakest signal: `TERM_PROGRAM` is set by many
    // emulators and carries no window, so anything that *can* name one wins.
    if is_ghostty(clean(env.term_program).as_deref()) {
        return (Some(ghostty_identity()), None);
    }
    (None, None)
}

/// Whether a `TERM_PROGRAM` value names Ghostty. Compared case-insensitively:
/// Ghostty writes it lowercase, and this is a display-ish string that costs
/// nothing to be lenient about.
pub fn is_ghostty(term_program: Option<&str>) -> bool {
    term_program.is_some_and(|s| s.eq_ignore_ascii_case("ghostty"))
}

/// The identity of a Ghostty instance — the single constructor for the form, as
/// [`zellij_identity`] is for its own.
///
/// **Deliberately not instance-granular**, which every other backend here is.
/// The reason that rule exists is that window ids overlap between instances: two
/// zellij sessions each number panes 1,2,3…, so a binding from one would resolve
/// to a live-but-wrong pane in the other. Ghostty's surface ids are UUIDs
/// (`SurfaceView.id.uuidString`), so they collide with nothing — not another
/// instance's, and not a restarted Ghostty's. A binding left over from a dead
/// Ghostty simply fails to resolve and is pruned, which is the failure direction
/// captain-miao already handles everywhere.
///
/// The narrower reason not to reach for a key anyway: there is nothing cheap to
/// key on. The scripting dictionary exposes no application pid, and `TERM_PROGRAM`
/// is all a launcher gets — so any instance key would cost a process scan in
/// `cm-core`, on a path a headless launcher also runs.
pub fn ghostty_identity() -> String {
    "ghostty".to_string()
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

/// The identity of a tmux **server**, from its socket path and server pid — the
/// single constructor for the `tmux:<socket>,<pid>` form (see
/// [`zellij_identity`]).
///
/// The server pid is part of the key, not decoration: `%N`/`@N` are minted from
/// counters that never recycle *while the server lives*, but a server restarted
/// on the same socket path re-mints from `%1`/`@1` (probe-verified on 3.7b). Keyed
/// by socket alone, a binding persisted before the restart would name a
/// live-but-wrong pane; keyed by socket **and** pid, the old server's bindings are
/// simply *foreign* — carried verbatim, never resolved — which is the failure
/// direction captain-miao already handles everywhere else. The cost is that
/// foreign entries from dead servers accumulate in `window-bindings.json` rather
/// than being pruned; correctness over tidiness.
pub fn tmux_identity_parts(socket: &str, server_pid: &str) -> String {
    format!("tmux:{socket},{server_pid}")
}

/// What a raw `TMUX` value names. Parsed here, in cm-core, because both readers
/// must agree byte for byte: the launcher stamps `LauncherState.terminal` from
/// this env, and the dashboard's tmux backend builds the identity it matches
/// against from the same string. Two copies of the parser would let a binding
/// read as foreign — i.e. inert — the moment they drifted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxEnv {
    /// The server socket, the `-S` every call is pinned to.
    pub socket: String,
    /// The server pid — part of the instance identity, see [`tmux_identity_parts`].
    pub server_pid: String,
    /// The **session target**, already in tmux's `$N` id form.
    ///
    /// `TMUX` carries the session id *bare* (`…,4242,0`), and a bare `0` is a
    /// session **name** to every tmux target lookup — so `-t 0` finds a session
    /// called `0` if one exists, silently falls through to the current session
    /// otherwise, and `new-window -t 0:` just fails with `can't find session: 0`
    /// (all three probe-verified on 3.7b). Sigil it here, once, so no caller can
    /// spend the raw field as a target.
    pub session: String,
}

/// Parse a raw `TMUX` value (`<socket_path>,<server_pid>,<session_id>`), or
/// `None` when it doesn't parse.
///
/// Split from the **right**, because a socket path may itself contain a comma
/// while the two trailing fields cannot. A non-numeric server pid or session id
/// means this isn't a `TMUX` value we understand, so it fails closed rather than
/// minting an identity that could collide with a real one — or a `-t` target
/// that names the wrong session.
pub fn parse_tmux_env(tmux_env: &str) -> Option<TmuxEnv> {
    // rsplitn yields right-to-left: session id, server pid, then the remainder
    // (the socket path, commas and all).
    let mut parts = tmux_env.rsplitn(3, ',');
    let session = parts.next()?.trim();
    let server_pid = parts.next()?.trim();
    let socket = parts.next()?.trim();
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if socket.is_empty() || !numeric(server_pid) || !numeric(session) {
        return None;
    }
    Some(TmuxEnv {
        socket: socket.to_string(),
        server_pid: server_pid.to_string(),
        session: format!("${session}"),
    })
}

/// The identity of the tmux server named by a raw `TMUX` value, or `None` when
/// it doesn't parse. Thin wrapper over [`parse_tmux_env`].
pub fn tmux_identity(tmux_env: &str) -> Option<String> {
    parse_tmux_env(tmux_env).map(|e| tmux_identity_parts(&e.socket, &e.server_pid))
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

    /// A full ambient Kitty env, so every precedence test below is really
    /// asserting that the inner multiplexer *beats* an outer Kitty.
    fn kitty_env() -> TerminalEnv {
        TerminalEnv {
            kitty_window: Some("7".into()),
            kitty_listen: Some("unix:/tmp/k".into()),
            kitty_pid: Some("999".into()),
            ..Default::default()
        }
    }

    #[test]
    fn zellij_beats_kitty_and_namespaces_by_session() {
        // A zellij pane wins over the ambient (possibly outer-Kitty) env, and the
        // identity carries the session so two sessions' pane ids don't collide.
        let (id, win) = resolve_terminal_env(TerminalEnv {
            zellij_pane: Some("3".into()),
            zellij_session: Some("work".into()),
            ..kitty_env()
        });
        assert_eq!(id.as_deref(), Some("zellij:work"));
        assert_eq!(win, Some(WindowId("3".into())));
    }

    #[test]
    fn zellij_pane_without_session_falls_through_to_kitty() {
        // No session name ⇒ the pane can't be namespaced to an instance ⇒ the
        // whole zellij env is treated as absent (both halves fall through).
        let (id, win) = resolve_terminal_env(TerminalEnv {
            zellij_pane: Some("3".into()),
            ..kitty_env()
        });
        assert_eq!(id.as_deref(), Some("kitty:unix:/tmp/k"));
        assert_eq!(win, Some(WindowId("7".into())));

        // An empty/whitespace session name counts as missing.
        let (id, _) = resolve_terminal_env(TerminalEnv {
            zellij_pane: Some("3".into()),
            zellij_session: Some("   ".into()),
            kitty_listen: None,
            ..kitty_env()
        });
        assert_eq!(id.as_deref(), Some("kitty:999"));
    }

    #[test]
    fn tmux_beats_kitty_and_is_keyed_by_socket_and_server_pid() {
        // Same argument as zellij: a tmux inside Kitty leaks the outer
        // KITTY_WINDOW_ID into every pane, so the pane must win.
        let (id, win) = resolve_terminal_env(TerminalEnv {
            tmux: Some("/tmp/tmux-1000/default,4242,0".into()),
            tmux_pane: Some("%5".into()),
            ..kitty_env()
        });
        assert_eq!(id.as_deref(), Some("tmux:/tmp/tmux-1000/default,4242"));
        assert_eq!(win, Some(WindowId("%5".into())));
    }

    #[test]
    fn zellij_still_beats_tmux_when_both_are_set() {
        // Nested, and the env alone can't say which is inner. Keeping zellij
        // first means adding tmux changes nothing for existing zellij users; the
        // wrong guess is corrected with `[terminal] backend`.
        let (id, win) = resolve_terminal_env(TerminalEnv {
            zellij_pane: Some("3".into()),
            zellij_session: Some("work".into()),
            tmux: Some("/tmp/s,4242,0".into()),
            tmux_pane: Some("%5".into()),
            ..kitty_env()
        });
        assert_eq!(id.as_deref(), Some("zellij:work"));
        assert_eq!(win, Some(WindowId("3".into())));
    }

    #[test]
    fn tmux_without_a_parseable_env_falls_through_to_kitty() {
        // A pane id we can't namespace to a server is no better than none, so
        // both halves fall through (the zellij rule, verbatim).
        for tmux in ["", "garbage", "/tmp/s,notapid,0", "/tmp/s"] {
            let (id, win) = resolve_terminal_env(TerminalEnv {
                tmux: Some(tmux.into()),
                tmux_pane: Some("%5".into()),
                ..kitty_env()
            });
            assert_eq!(id.as_deref(), Some("kitty:unix:/tmp/k"), "TMUX={tmux:?}");
            assert_eq!(win, Some(WindowId("7".into())), "TMUX={tmux:?}");
        }
    }

    #[test]
    fn tmux_identity_splits_from_the_right() {
        // A socket path containing a comma still parses: only the two trailing
        // fields are fixed.
        assert_eq!(
            tmux_identity("/tmp/od,d/sock,4242,3").as_deref(),
            Some("tmux:/tmp/od,d/sock,4242")
        );
        // The session id is deliberately NOT part of the identity — one server
        // mints one id namespace across all its sessions.
        assert_eq!(
            tmux_identity("/tmp/s,4242,0"),
            tmux_identity("/tmp/s,4242,9")
        );
        // A server restarted on the same socket is a *different* instance.
        assert_ne!(
            tmux_identity("/tmp/s,4242,0"),
            tmux_identity("/tmp/s,4243,0")
        );
        assert_eq!(tmux_identity(""), None);
        assert_eq!(tmux_identity("/tmp/s,4242"), None);
        assert_eq!(tmux_identity(",4242,0"), None);
    }

    #[test]
    fn the_session_target_carries_tmux_s_id_sigil() {
        // `TMUX` holds the session id bare, and a bare `0` is a session *name* to
        // every tmux target lookup — `new-window -t 0:` fails outright unless a
        // session happens to be called `0`, and `list-panes -t 0` silently falls
        // through to the current session. The `$` is what makes it an id.
        let env = parse_tmux_env("/tmp/tmux-1000/default,4242,0").expect("parses");
        assert_eq!(env.socket, "/tmp/tmux-1000/default");
        assert_eq!(env.server_pid, "4242");
        assert_eq!(env.session, "$0");
        // A socket path with a comma still parses (split from the right).
        assert_eq!(
            parse_tmux_env("/tmp/od,d/s,4242,7").map(|e| (e.socket, e.session)),
            Some(("/tmp/od,d/s".into(), "$7".into()))
        );
        // A non-numeric or empty session id is not a `TMUX` we understand:
        // sigilling it would mint a target that names nothing.
        assert_eq!(parse_tmux_env("/tmp/s,4242,"), None);
        assert_eq!(parse_tmux_env("/tmp/s,4242,not-an-id"), None);
    }

    #[test]
    fn kitty_identity_prefers_listen_socket_then_pid() {
        let (id, win) = resolve_terminal_env(kitty_env());
        assert_eq!(id.as_deref(), Some("kitty:unix:/tmp/k"));
        assert_eq!(win, Some(WindowId("7".into())));

        // No listen socket ⇒ fall back to the pid.
        let (id, _) = resolve_terminal_env(TerminalEnv {
            kitty_listen: None,
            ..kitty_env()
        });
        assert_eq!(id.as_deref(), Some("kitty:999"));

        // A Kitty window with neither key yields a window but no identity.
        let (id, win) = resolve_terminal_env(TerminalEnv {
            kitty_listen: None,
            kitty_pid: None,
            ..kitty_env()
        });
        assert_eq!(id, None);
        assert_eq!(win, Some(WindowId("7".into())));
    }

    #[test]
    fn ghostty_yields_an_identity_but_never_a_window() {
        // Ghostty exports no per-surface variable, so the window half is
        // structurally absent — a launcher inside one can still be *classified*
        // (the row isn't foreign), it just carries no window to drive.
        let (id, win) = resolve_terminal_env(TerminalEnv {
            term_program: Some("ghostty".into()),
            ..Default::default()
        });
        assert_eq!(id.as_deref(), Some("ghostty"));
        assert_eq!(win, None);

        // Case-insensitive, and any other TERM_PROGRAM is not ours.
        for other in ["Ghostty", "GHOSTTY"] {
            let (id, _) = resolve_terminal_env(TerminalEnv {
                term_program: Some(other.into()),
                ..Default::default()
            });
            assert_eq!(id.as_deref(), Some("ghostty"), "TERM_PROGRAM={other}");
        }
        for other in ["Apple_Terminal", "iTerm.app", "WezTerm", "", "  "] {
            let (id, _) = resolve_terminal_env(TerminalEnv {
                term_program: Some(other.into()),
                ..Default::default()
            });
            assert_eq!(id, None, "TERM_PROGRAM={other}");
        }
    }

    #[test]
    fn anything_that_names_a_window_beats_ghostty() {
        // `TERM_PROGRAM` is the weakest signal on offer: it survives into a
        // multiplexer's panes, and it can't name a window. A backend that *can*
        // must win, or a zellij session inside Ghostty would report no pane.
        let cases = [
            (
                TerminalEnv {
                    zellij_pane: Some("3".into()),
                    zellij_session: Some("work".into()),
                    term_program: Some("ghostty".into()),
                    ..Default::default()
                },
                "zellij:work",
                "3",
            ),
            (
                TerminalEnv {
                    tmux: Some("/tmp/s,4242,0".into()),
                    tmux_pane: Some("%5".into()),
                    term_program: Some("ghostty".into()),
                    ..Default::default()
                },
                "tmux:/tmp/s,4242",
                "%5",
            ),
            (
                TerminalEnv {
                    term_program: Some("ghostty".into()),
                    ..kitty_env()
                },
                "kitty:unix:/tmp/k",
                "7",
            ),
        ];
        for (env, want_id, want_win) in cases {
            let (id, win) = resolve_terminal_env(env);
            assert_eq!(id.as_deref(), Some(want_id));
            assert_eq!(win, Some(WindowId(want_win.into())));
        }
    }

    #[test]
    fn no_terminal_env_is_none() {
        assert_eq!(resolve_terminal_env(TerminalEnv::default()), (None, None));
    }
}
