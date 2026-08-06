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

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::backend::{LocalBackend, OpenSpec};
use crate::pty_pool::{daemon_is_live, pool_socket_path};

/// Monotonic suffix so two sessions opened by one server never collide.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

/// The login-shell wrapper every pool launcher runs through (see the PATH/TERM
/// rationale in [`open_in_pool`]).
///
/// CONSTRAINT: must stay free of `{`/`}`. libshpool runs `--cmd` (and `--dir`)
/// through its session-name template parser, which treats **every** bare `{`
/// as a `#{var}` substitution and rejects non-alphanumeric names — so a
/// `${TERM:-}`-style expansion fails the parse, and the attach client exits 1
/// *silently* (its logs go to `io::empty()` without `--log-file`; 2026-07-02
/// host-verification finding). `case "$TERM"` behaves identically for an unset
/// TERM under plain sh (no `set -u`): it expands empty. Pinned by
/// `pool_shell_has_no_template_braces`.
const POOL_SHELL: &str =
    r#"case "$TERM" in ""|dumb) TERM=xterm-256color; export TERM;; esac; exec "$@""#;

/// Last `n` chars of `s` — for log tails, where the operative error is final.
fn tail_chars(s: &str, n: usize) -> String {
    let start = s.chars().count().saturating_sub(n);
    s.chars().skip(start).collect()
}

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

/// Start a launcher inside the pool and return the pool session name. The
/// session runs `captain-miao <agent> <cwd> [resume]` (reusing the local
/// open_session argv); `attach --background --cmd` creates it detached. The
/// launcher then writes its state file, so the dashboard discovers the new
/// session through the normal subscription — discovery and spawning share one
/// path (§3).
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
    let plan = backend.open_session(spec);
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
    //   * Upgrade a `dumb`/empty `TERM` to `xterm-256color` (universally-present
    //     terminfo) so the agent has color. (The pool session is created detached,
    //     before any client attaches, so we can't yet know the attaching
    //     terminal's TERM; a color-capable default is the safe choice.)
    // The launcher argv is passed positionally and `exec`'d (`exec "$@"`), so
    // nothing is re-quoted or re-parsed and no extra process lingers.
    let mut cmd_argv: Vec<String> = vec!["sh".into(), "-lc".into(), POOL_SHELL.into(), "_".into()];
    cmd_argv.extend(argv);
    let cmd = shell_words::join(&cmd_argv);

    // Capture stdout/stderr rather than discarding them: when libshpool's attach
    // fails, its error (the reason the create didn't take) is the only clue, and
    // it must reach both the server log and the dashboard's "Launch failed:" line.
    // stderr alone isn't enough: the attach *client*'s logs — including the
    // error it prints just before its silent `exit(1)` — go to `io::empty()`
    // unless `--log-file` is passed (libshpool's stderr writer is daemon-only).
    // Point it at a per-attempt file, surface its tail on failure, remove it on
    // success.
    let attach_log = crate::state::state_dir()
        .join("logs")
        .join(format!("attach-{name}.log"));
    if let Some(parent) = attach_log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let attach_log_arg = attach_log.display().to_string();
    let exe = std::env::current_exe().context("resolve current exe for attach")?;
    let out = Command::new(exe)
        .args([
            "attach",
            "--log-file",
            &attach_log_arg,
            "--background",
            "--cmd",
            &cmd,
            "--dir",
            &cwd,
            &name,
        ])
        .stdin(Stdio::null())
        .output()
        .context("spawn attach --background")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let log = std::fs::read_to_string(&attach_log).unwrap_or_default();
        tracing::warn!(
            target: "captain_miao::pool",
            "attach --background for {name} failed ({}); cwd={}; stderr={:?}; stdout={:?}; log tail={:?}",
            out.status, spec.cwd, stderr.trim(), stdout.trim(), tail_chars(log.trim(), 600)
        );
        // Prefer stderr (where an anyhow error from our own wrapper lands), then
        // stdout, then the tail of libshpool's log — its error lands *last*,
        // just before the exit. Cap either way so a long chain can't blow up
        // the state file / row.
        let head = |s: &str| -> String { s.chars().take(300).collect() };
        let detail = if !stderr.trim().is_empty() {
            head(stderr.trim())
        } else if !stdout.trim().is_empty() {
            head(stdout.trim())
        } else {
            tail_chars(log.trim(), 300)
        };
        if detail.is_empty() {
            bail!("attach --background exited with {}", out.status);
        }
        bail!("attach --background failed: {detail}");
    }
    let _ = std::fs::remove_file(&attach_log);
    // Record the cwd into this host's recent-dirs so the client's picker shows
    // it next time it targets this host (a remote launch's dir wouldn't
    // otherwise land in the remote list — the dashboard records only local ones).
    LocalBackend::new().record_recent_cwd(&spec.cwd);
    Ok(name)
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

    #[test]
    fn tail_chars_takes_the_end() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 5), "ab");
        assert_eq!(tail_chars("", 5), "");
    }
}
