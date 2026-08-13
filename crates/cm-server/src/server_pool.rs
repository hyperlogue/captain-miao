//! Server-side pty-pool control (Phase 3c). Only compiled with the `pty-pool`
//! feature (the remote/Linux build). The per-host `captain-miao server`
//! supervises the libshpool daemon and starts launchers *inside* the pool, so a
//! remote session's shell survives ssh drops / sleep — `docs/remote-sessions.md`
//! §8.
//!
//! **Persistence vs. a session-scoped server.** The Phase 2 ssh server is
//! session-scoped (it dies with the ssh channel). The pool daemon must *outlive*
//! it for sessions to persist across disconnects, so `ensure_daemon` starts the
//! daemon **detached** (own process group, stdio to /dev/null, not killed on the
//! server's exit). A later server reconnect finds it already bound and reuses
//! it. This is the open lifecycle question from §11; revisit during host
//! verification.
//!
//! Runs from `handle_conn` via `block_in_place` (it spawns child processes and
//! blocks on them), so everything here is synchronous `std::process`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::backend::{LocalBackend, OpenSpec};
use crate::pty_pool::{daemon_is_live, pool_socket_path};

/// Monotonic suffix so two sessions opened by one server never collide.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

/// The login-shell wrapper every pool launcher runs through (see the
/// PATH/TERM/COLORTERM rationale in [`open_in_pool`]).
///
/// CONSTRAINT: must stay free of `{`/`}`. libshpool runs `--cmd` (and `--dir`)
/// through its session-name template parser, which treats **every** bare `{`
/// as a `#{var}` substitution and rejects non-alphanumeric names — so a
/// `${TERM:-}`-style expansion fails the parse, and the attach client exits 1
/// *silently* (its logs go to `io::empty()` without `--log-file`; 2026-07-02
/// host-verification finding). `case "$TERM"` behaves identically for an unset
/// TERM under plain sh (no `set -u`): it expands empty. Pinned by
/// `pool_shell_has_no_template_braces`.
const POOL_SHELL: &str = r#"case "$TERM" in ""|dumb) TERM=xterm-256color;; *) infocmp "$TERM" >/dev/null 2>&1 || TERM=xterm-256color;; esac; export TERM; case "$COLORTERM" in "") COLORTERM=truecolor; export COLORTERM;; esac; exec "$@""#;

/// Verify the pool is live before opening a session in it. The pool now runs
/// **in this same process** (the daemon starts it on a thread — see
/// `server::start_pool_thread`), so this is a bounded liveness wait, not a
/// spawn: by the time the daemon accepts protocol connections the pool socket is
/// already bound, but we re-check defensively (the pool thread could have died).
fn ensure_daemon() -> Result<()> {
    for _ in 0..30 {
        if daemon_is_live() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "pty pool socket {} is not live (the daemon's pool thread may have died)",
        pool_socket_path().display()
    )
}

/// Reserve a pool session for a launcher and return its name. The session runs
/// `captain-miao <agent> <cwd> [resume]` (reusing the local open_session argv);
/// the launcher then writes its state file, so the dashboard discovers the new
/// session through the normal subscription — discovery and spawning share one
/// path (§3).
///
/// **This reserves, it does not create.** All it puts on disk is a
/// [`PendingSession`] record; the pty and the launcher inside it are created by
/// the *first attach*, out of that record ([`claim_pending`], from
/// `pty_pool::run_attach`). The eager `attach --background --cmd` this replaced
/// created the session with no client on the other end, and that turned out to
/// be the root of both terminal complaints from remote mode:
///
/// * the agent's TUI ran its capability **queries** (the kitty keyboard
///   protocol's `CSI ? u`, truecolor probes) into a pty nobody was reading, got
///   no reply, fell back to the legacy key encoding, and stayed there for the
///   whole session — shpool never re-negotiates on the app's behalf when a
///   client later connects. Shift+Enter arrived as a bare CR;
/// * the environment (`TERM`, tty size) had to be *guessed* at create time,
///   because there was no attaching terminal to copy from — libshpool applies
///   the attach header's env only when it spawns the session's command.
///
/// Creating from the first attach makes all of that fall out for free: the
/// client is on the far end from byte zero, its real `TERM` and window size are
/// what libshpool spawns the command with, and the queries are answered by the
/// terminal the user is actually looking at.
///
/// Two consequences worth knowing. A window that never reaches its attach (ssh
/// refused, the terminal failed to spawn it) now leaves **no session** rather
/// than an agent running headless that nobody asked to keep — a window closed
/// *after* the create still just detaches, as it always did. And a create
/// failure surfaces in that held window instead of as the dashboard's "Launch
/// failed:" line, since there is no longer a server-side create whose stderr we
/// could capture; what the reservation step can still refuse locally (a dead
/// pool) it does, via [`ensure_daemon`].
pub(crate) fn open_in_pool(spec: &OpenSpec) -> Result<String> {
    ensure_daemon()?;

    let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "cm-{}-{}-{seq}",
        spec.agent.cli_subcommand(),
        std::process::id()
    );

    // The local-window argv *is* the launcher argv: [exe, agent, cwd, resume…].
    // Append `--pool-session <name>` so the launcher folds the pool name into its
    // state file (the client uses it to attach to this running session later).
    let backend = LocalBackend::new();
    // The spec's cwd arrives in the host-canonical `~` form (§3); a process
    // needs the real path — both in the argv (`open_session` expands it) and in
    // libshpool's `--dir`, which is a chdir, not a shell word.
    let cwd = cm_core::paths::expand_home(&spec.cwd, backend.home());
    let plan = backend.open_session(spec)?;
    let mut argv: Vec<String> = plan.argv().to_vec();
    argv.push("--pool-session".to_string());
    argv.push(name.clone());

    // libshpool starts pool sessions with a minimal, sanitized environment,
    // bypassing both PAM and shell-profile setup, so the launcher inherits two
    // broken things: a bare `PATH=/usr/bin:/bin:/usr/sbin:/sbin` (missing the
    // profile bin dirs where `claude`/`codex` live → "not found in PATH") and
    // `TERM=dumb` (→ the agent's TUI renders with no color). Fix both at the
    // boundary:
    //   * Run the launcher through a **login shell** (`sh -l`), the standard way
    //     to establish a user session: it sources `/etc/profile` (and the user
    //     profile), which is where the login PATH — and anything else a real
    //     login sets — comes from, so the launcher gets the environment a user's
    //     own session would, not a hand-copied PATH.
    //   * Upgrade a `dumb`/empty `TERM` — or one this host has no terminfo entry
    //     for — to `xterm-256color` (universally present) so the agent has color.
    //     The create *is* the first attach, so `$TERM` here is that client's real
    //     value (libshpool injects the attach header's env when it spawns the
    //     command): the rewrite fires only where the host genuinely couldn't
    //     render what the client sent, e.g. `xterm-kitty` on a box without
    //     kitty's terminfo. Whatever survives this is the terminfo the session
    //     keeps for life — later windows inherit it, whatever *they* are — which
    //     is why the launcher records it (`LauncherState::terminfo`) and the
    //     dashboard's detail panel shows it.
    //   * Export `COLORTERM=truecolor`. TERM alone caps the agent at the 256-color
    //     palette — 24-bit support is gated on `COLORTERM` by every library that
    //     detects it, and the pool strips it like everything else, so a pooled
    //     session rendered its whole UI in 256-color approximations of the colors
    //     a local one gets. Hard-coding it is safe here rather than a guess: the
    //     dashboard refuses to start outside Kitty or zellij (`requires_terminal`),
    //     and both are 24-bit, so every terminal that can ever attach supports it.
    //     Set only when empty, so a host that publishes its own value via
    //     `/etc/environment` (which libshpool loads into the session) still wins.
    // The launcher argv is passed positionally and `exec`'d (`exec "$@"`), so
    // nothing is re-quoted or re-parsed and no extra process lingers.
    let mut cmd_argv: Vec<String> = vec!["sh".into(), "-lc".into(), POOL_SHELL.into(), "_".into()];
    cmd_argv.extend(argv);
    let cmd = shell_words::join(&cmd_argv);

    write_pending(&name, &PendingSession { cmd, dir: cwd })?;
    // Record the cwd into this host's recent-dirs so the client's picker shows
    // it next time it targets this host (a remote launch's dir wouldn't
    // otherwise land in the remote list — the dashboard records only local ones).
    // Recorded at reservation rather than at create: the user asked for this
    // directory, which is what the picker's list is about, and the create now
    // happens in another process we don't wait on.
    LocalBackend::new().record_recent_cwd(&spec.cwd);
    Ok(name)
}

// -- Pending sessions (the reservation half of create-on-first-attach) --

/// A reserved-but-not-yet-created pool session: everything the first attach
/// needs to bring it into being. Written by [`open_in_pool`], claimed by
/// [`claim_pending`].
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingSession {
    /// libshpool `--cmd`: the login-shell wrapper plus the launcher argv, joined
    /// into one shell-words string (libshpool re-splits it and execs the result
    /// directly — no shell is involved on its side).
    pub(crate) cmd: String,
    /// libshpool `--dir`: the session's working directory, **expanded** (it is a
    /// chdir, not a shell word, so a host-canonical `~` would be a literal
    /// directory name — §3).
    pub(crate) dir: String,
}

/// Where reservations live. Inside the 0700 state tree, and each record is
/// written 0600 by `write_json_atomic` — it holds a command line naming the
/// user's working directory, same sensitivity as a launcher state file.
///
/// It is host-local state, deliberately *not* wire protocol: reserving and
/// attaching both happen inside `miao-server` on the same host as the pool, so
/// nothing about this needs to reach (or be understood by) the dashboard. That
/// is also what makes the change compatible in both directions — an old
/// dashboard drives a new server fine (it just runs the attach argv, which now
/// creates), and a new dashboard against an old server finds no reservation and
/// falls through to a plain reattach of the session that server created eagerly.
fn pending_dir() -> std::path::PathBuf {
    crate::state::state_dir().join("pending-sessions")
}

/// The record path for `name`. Pool names are minted by [`open_in_pool`] as
/// `cm-<agent>-<pid>-<seq>`, so they are always a single safe path component —
/// but this is also reached from `run_attach` with a name that arrived over
/// ssh, so refuse anything that isn't. libshpool applies the same rule to its
/// own session names (`handle_attach` rejects `/`, whitespace, `.` and `..`).
fn pending_path(name: &str) -> Option<std::path::PathBuf> {
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_whitespace);
    safe.then(|| pending_dir().join(format!("{name}.json")))
}

/// Record a reservation for `name`.
fn write_pending(name: &str, pending: &PendingSession) -> Result<()> {
    let path = pending_path(name)
        .ok_or_else(|| anyhow::anyhow!("refusing to reserve unsafe session name {name:?}"))?;
    cm_core::state::create_dir_all_private(&pending_dir())
        .with_context(|| format!("create {}", pending_dir().display()))?;
    cm_core::state::write_json_atomic(&path, pending)
        .with_context(|| format!("write reservation {}", path.display()))
}

/// Take the reservation for `name`, if there is one — the caller then owns the
/// job of creating that session.
///
/// **Claiming is the `remove_file`, not the read.** Only one unlinker can win,
/// so two attaches racing the same fresh name can't both decide they are the
/// creator: the loser sees `Err` and falls through to a plain reattach, which is
/// the correct handling for a session the winner is bringing up. That is also
/// why the record is consumed *before* libshpool runs rather than after a
/// successful create: holding it for the session's lifetime would let a later
/// attach (a steal, a second window) re-enter the create path and skip the
/// stale-name guard. The cost of consume-first is that a crash between claim and
/// create loses the reservation — the user reopens, which beats a reservation
/// that can be redeemed twice.
pub(crate) fn claim_pending(name: &str) -> Option<PendingSession> {
    let path = pending_path(name)?;
    let pending: PendingSession = cm_core::state::read_json(&path)?;
    std::fs::remove_file(&path).ok()?;
    Some(pending)
}

/// Drop every reservation. Called when the daemon starts: the pool lives *in*
/// this process, so a daemon that is only now starting hosts no sessions and
/// every record from a previous incarnation is unredeemable — its name can never
/// be attached again (names carry the minting daemon's pid). Inert litter
/// either way; this just keeps it from accumulating.
pub(crate) fn prune_pending() {
    let Ok(entries) = std::fs::read_dir(pending_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|e| e == "json") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the libshpool-template coupling (2026-07-02 host-verification
    /// finding): every bare `{` in `--cmd` is parsed as a `#{var}` substitution
    /// and an invalid name (`${TERM:-}`) makes the attach client exit 1 with no
    /// output at all. The wrapper must stay brace-free.
    #[test]
    fn pool_shell_has_no_template_braces() {
        assert!(
            !POOL_SHELL.contains(['{', '}']),
            "POOL_SHELL must not contain braces — libshpool templates `--cmd`"
        );
    }

    /// The pool strips the environment, so `COLORTERM` never survives into a
    /// pooled session and the agent renders 256-color approximations of its
    /// real palette. The wrapper is the only place to put it back — and it must
    /// defer to a value the host already publishes (libshpool loads
    /// `/etc/environment` into the session).
    #[test]
    fn pool_shell_exports_truecolor() {
        assert!(
            POOL_SHELL.contains("COLORTERM=truecolor"),
            "POOL_SHELL must export COLORTERM — TERM alone caps the agent at 256 colors"
        );
        assert!(
            POOL_SHELL.contains(r#"case "$COLORTERM" in """#),
            "COLORTERM must be set only when empty, so a host-provided value wins"
        );
    }

    /// The wrapper's whole job is to fix the environment and then get out of the
    /// way: the launcher argv is `exec`'d, so nothing is re-quoted or re-parsed
    /// and no extra shell process lingers under the agent (which would show up
    /// in the `bg_shells` process-tree scan).
    #[test]
    fn pool_shell_execs_its_positional_args() {
        assert!(POOL_SHELL.trim_end().ends_with(r#"exec "$@""#));
    }

    /// Run the wrapper the way libshpool actually runs it and read the resulting
    /// environment back out.
    ///
    /// The reproduction matters more than the assertions: libshpool does **not**
    /// hand `--cmd` to a shell. It `shell_words::split`s the string and execs the
    /// argv directly (`daemon/server.rs`, the `header.cmd` branch), so this
    /// splits the same joined string and execs the parts — which also pins that
    /// `join` → `split` round-trips, the property the whole wrapper rides on.
    ///
    /// Three environments, one per thing the wrapper decides:
    /// * the stripped pool environment → both variables get sane values;
    /// * a `TERM` the host's terminfo knows → passed through, since with
    ///   create-on-first-attach that value is the *real* attaching terminal's;
    /// * a `TERM` it doesn't → downgraded, because a session whose apps abort on
    ///   "unknown terminal type" is worse than one that under-reports.
    #[test]
    fn the_wrapper_fixes_the_environment_libshpool_hands_it() {
        let joined = shell_words::join(["sh", "-lc", POOL_SHELL, "_", "/usr/bin/env"]);
        let parts = shell_words::split(&joined).expect("libshpool re-splits --cmd");
        assert_eq!(parts.first().map(String::as_str), Some("sh"));
        let read_env = |vars: &[(&str, &str)]| -> Vec<String> {
            let mut cmd = std::process::Command::new(&parts[0]);
            cmd.args(&parts[1..])
                .env_clear()
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            for (k, v) in vars {
                cmd.env(k, v);
            }
            let out = cmd.output().expect("running the pool wrapper");
            assert!(out.status.success(), "wrapper failed for {vars:?}");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.starts_with("TERM=") || l.starts_with("COLORTERM="))
                .map(str::to_string)
                .collect()
        };

        let stripped = read_env(&[("TERM", "dumb")]);
        assert!(
            stripped.contains(&"TERM=xterm-256color".to_string()),
            "a dumb TERM must be upgraded: {stripped:?}"
        );
        assert!(
            stripped.contains(&"COLORTERM=truecolor".to_string()),
            "COLORTERM must be restored: {stripped:?}"
        );

        let provided = read_env(&[("TERM", "xterm-256color"), ("COLORTERM", "8bit")]);
        assert!(
            provided.contains(&"TERM=xterm-256color".to_string())
                && provided.contains(&"COLORTERM=8bit".to_string()),
            "a known TERM and a host-set COLORTERM must both survive: {provided:?}"
        );

        // Only meaningful where `infocmp` can actually answer; without ncurses
        // every TERM downgrades, which is the same safe direction but proves
        // nothing about the check.
        if std::process::Command::new("infocmp")
            .arg("xterm-256color")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            let unknown = read_env(&[("TERM", "xterm-kitty-not-a-real-terminfo")]);
            assert!(
                unknown.contains(&"TERM=xterm-256color".to_string()),
                "a TERM this host has no terminfo for must be downgraded: {unknown:?}"
            );
            // The downgrade is otherwise invisible for the session's whole
            // life: it is `LauncherState::terminfo` — the value the launcher
            // stamps from this very environment — that carries it to the
            // dashboard, which is why the wrapper must leave a *usable* name
            // here rather than unset the variable.
            assert!(
                !unknown.contains(&"TERM=".to_string()),
                "the wrapper must never leave TERM empty: {unknown:?}"
            );
        }
    }

    /// A reservation round-trips, and claiming it is *consuming* it — the second
    /// claim must come back empty, which is what stops a steal or a second window
    /// from re-entering the create path and skipping the stale-name guard.
    #[test]
    fn a_reservation_is_claimable_exactly_once() {
        let name = format!("cm-test-{}-{}", std::process::id(), line!());
        write_pending(
            &name,
            &PendingSession {
                cmd: "sh -lc true".into(),
                dir: "/tmp".into(),
            },
        )
        .unwrap();
        let claimed = claim_pending(&name).expect("first claim wins");
        assert_eq!(claimed.cmd, "sh -lc true");
        assert_eq!(claimed.dir, "/tmp");
        assert!(
            claim_pending(&name).is_none(),
            "a reservation must not be redeemable twice"
        );
    }

    /// The name reaches `claim_pending` from an ssh-supplied argv, so it must
    /// never be able to name a path of its own — the same rule libshpool applies
    /// to its session names.
    #[test]
    fn a_reservation_name_cannot_escape_its_directory() {
        for bad in ["", ".", "..", "../evil", "a/b", "has space", "tab\there"] {
            assert!(
                pending_path(bad).is_none(),
                "{bad:?} should be refused as a session name"
            );
        }
        let good = pending_path("cm-claude-42-1").expect("a minted name is accepted");
        assert_eq!(good.parent(), Some(pending_dir().as_path()));
    }

    /// Claiming a name nothing reserved is the *plain reattach* signal, so it has
    /// to be a quiet `None` rather than an error — every window after the first
    /// takes this path.
    #[test]
    fn claiming_an_unreserved_name_is_none() {
        assert!(claim_pending("cm-claude-does-not-exist-0").is_none());
    }
}
