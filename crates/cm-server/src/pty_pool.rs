//! Phase 3 pty pool — the `pty-daemon` and `attach` entrypoints that embed
//! libshpool. Only compiled with the `pty-pool` feature (the remote/Linux
//! build). The macOS dashboard client never runs these: it spawns
//! `ssh -t <host> captain-miao attach <name>`, so `attach` (and the daemon the
//! server supervises) run on the *remote* after the ssh hop. See
//! `docs/remote-sessions.md` §8.
//!
//! Both dispatch through `libshpool::run`, which is `unsafe` because its daemon
//! path can double-fork to daemonize and is only sound while the process is
//! still single-threaded. So `main` routes these here *before* it builds the
//! tokio runtime (which spawns threads). captain-miao never sets libshpool's
//! autodaemonize env var — the server supervises the daemon as a foreground
//! child (§8), so no double-fork actually happens — but honoring the
//! pre-threads contract costs nothing and keeps the call sound.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use libshpool::Args;

/// captain-miao's private pool socket — resolved in cm-core so the server and
/// the `captain-miao-client` list/attach tool agree on the path. A dedicated
/// path (not shpool's default) gives the pool its own session namespace, so
/// captain-miao sessions never collide with a user's own shpool/tmux — §8
/// isolation-by-construction.
pub(crate) fn pool_socket_path() -> PathBuf {
    crate::state::pool_socket_path()
}

/// The libshpool config captain-miao authors for its pool daemon. We pin
/// **simple** session-restore mode: on reattach libshpool just reconnects the
/// pty and issues SIGWINCH — it does **not** spool output and replay
/// scrollback. The agent TUIs (Claude/Codex) own the whole screen and redraw on
/// SIGWINCH, so a replayed screenful would only paint stale output over the
/// live redraw, and the spool costs memory per session for no benefit.
/// libshpool's default is `screen` (restore a screenful), so `simple` has to be
/// set explicitly. Pinned by `pool_config_is_simple_restore`.
const POOL_CONFIG: &str = "session_restore_mode = \"simple\"\n";

/// Path to the libshpool config captain-miao writes for its pool daemon. Lives
/// next to the pool socket in the per-user runtime dir; regenerated on every
/// daemon start, so it's safe to lose across reboots.
fn pool_config_path() -> PathBuf {
    crate::state::runtime_dir().join("pool-config.toml")
}

/// Write [`POOL_CONFIG`] to [`pool_config_path`] and return the path (to pass
/// via `--config-file`). Must run before starting the daemon: libshpool
/// *skips* a missing config file and silently falls back to its `screen`
/// default, so the file has to exist first.
fn write_pool_config() -> Result<PathBuf> {
    let path = pool_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, POOL_CONFIG)
        .with_context(|| format!("write pool config {}", path.display()))?;
    Ok(path)
}

/// True if a pty-daemon is already listening on `socket`. A bare connect is
/// enough: it's captain-miao's private socket in its per-user runtime dir, so
/// anything that accepts a connection there is our daemon. This is the liveness
/// half of zellij's socket-as-identity model; zellij additionally handshakes
/// because it scans a *shared* session-socket namespace, whereas we own exactly
/// one private socket.
fn socket_is_live(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Liveness of the pool socket. Used by `server_pool::ensure_daemon` to wait for
/// a freshly-spawned daemon to bind.
pub(crate) fn daemon_is_live() -> bool {
    socket_is_live(&pool_socket_path())
}

enum DaemonStart {
    /// A live daemon already owns the socket — the entrypoint is a no-op.
    AlreadyRunning,
    /// No live daemon; safe to bind (any stale socket file has been cleared).
    Proceed,
}

/// Decide whether `run_daemon` should start, guarding the singleton the way
/// zellij guards its per-session server — the socket *is* the identity:
///
/// - a live daemon answers → don't race a second listener onto the same socket;
/// - the socket file lingers but nothing listens → it's **stale** (a daemon that
///   was SIGKILLed never unlinked it), so remove it, else libshpool's bind would
///   fail `EADDRINUSE`;
/// - no socket file → a normal cold start.
fn prepare_daemon_socket(socket: &Path) -> Result<DaemonStart> {
    if socket_is_live(socket) {
        return Ok(DaemonStart::AlreadyRunning);
    }
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("remove stale pool socket {}", socket.display()))?;
    }
    Ok(DaemonStart::Proceed)
}

/// Run a libshpool subcommand by parsing a synthetic argv with clap — the same
/// path the `shpool` binary uses. We can't construct `Commands::Attach` directly
/// (it's `#[non_exhaustive]`), and parsing also stays correct if libshpool adds
/// optional flags. The leading element is the (ignored) program name; `global`
/// are flags before the subcommand, `sub` is the subcommand + its flags. We pin
/// our private socket via `--socket`; libshpool namespaces its own runtime
/// scratch dir off the socket string, so the explicit path keeps both private to
/// us. Must be called before the tokio runtime is built.
fn run_shpool(global: &[&str], sub: &[&str]) -> Result<()> {
    // libshpool binds the socket but doesn't create our custom socket's parent.
    let socket = pool_socket_path();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let socket = socket.to_string_lossy().into_owned();
    let mut argv: Vec<String> = vec!["captain-miao-pty".into(), "--socket".into(), socket];
    argv.extend(global.iter().chain(sub).map(|s| s.to_string()));
    let args = Args::try_parse_from(&argv).context("building libshpool args")?;
    // Safety: callers run before the tokio runtime (hence any thread) is built;
    // see the module docs.
    unsafe { libshpool::run(args, None) }
}

/// Run the libshpool daemon in the foreground. Blocks until it exits.
///
/// Deduplicated at the entrypoint itself (not only via
/// `server_pool::ensure_daemon`): **at most one pty-daemon per user**. The pool
/// socket lives in the per-user `runtime_dir()` (`$XDG_RUNTIME_DIR`, 0700), so a
/// single live socket there *is* the per-user singleton. If one already answers
/// we no-op; libshpool's own bind is the final backstop against a cold-start
/// race.
pub(crate) fn run_daemon() -> Result<()> {
    let socket = pool_socket_path();
    match prepare_daemon_socket(&socket)? {
        DaemonStart::AlreadyRunning => {
            eprintln!(
                "captain-miao: a pty-daemon is already running on {}",
                socket.display()
            );
            Ok(())
        }
        // `--no-daemonize` is load-bearing now: the merged daemon runs this on a
        // *thread* alongside the tokio runtime, so libshpool must never take its
        // own daemonize double-fork (a fork in a multithreaded process is unsafe).
        // captain-miao does its own detaching (`server::daemonize`, pre-threads).
        // `--config-file` pins simple session-restore (no scrollback replay) —
        // see `POOL_CONFIG`.
        DaemonStart::Proceed => {
            let config = write_pool_config()?;
            let config = config.to_string_lossy().into_owned();
            run_shpool(&["--no-daemonize", "--config-file", &config], &["daemon"])
        }
    }
}

/// Attach to a pool session, proxying its pty to this terminal. With
/// `cmd`/`background` set, instead *creates* a session running `cmd` and detaches
/// (how the server starts a launcher in the pool); otherwise a plain interactive
/// reattach (what the client's ssh window runs). Always `--no-daemonize`:
/// captain-miao manages its own daemon (named `pty-daemon`, not shpool's default
/// `daemon`), so shpool's auto-launch — which would re-exec `<exe> daemon` — must
/// not fire.
pub(crate) fn run_attach(
    name: String,
    cmd: Option<String>,
    dir: Option<String>,
    background: bool,
    log_file: Option<String>,
) -> Result<()> {
    // `--log-file` is the only way to see the attach *client*'s logs: libshpool
    // writes non-daemon logs to `io::empty()` without it (its stderr writer is
    // daemon-only), and it `error!`s + exits 1 on failure — so a background
    // create that fails leaves its reason nowhere unless this is set.
    let mut global: Vec<&str> = vec!["--no-daemonize"];
    if let Some(lf) = &log_file {
        global.push("--log-file");
        global.push(lf);
    }
    let mut sub: Vec<&str> = vec!["attach"];
    if background {
        sub.push("--background");
    }
    if let Some(c) = &cmd {
        sub.push("--cmd");
        sub.push(c);
    }
    if let Some(d) = &dir {
        sub.push("--dir");
        sub.push(d);
    }
    sub.push(&name);
    run_shpool(&global, &sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cm-pty-{}-{}.sock", tag, std::process::id()))
    }

    #[test]
    fn proceeds_when_socket_absent() {
        let sock = temp_sock("absent");
        let _ = std::fs::remove_file(&sock);
        assert!(matches!(
            prepare_daemon_socket(&sock).unwrap(),
            DaemonStart::Proceed
        ));
    }

    #[test]
    fn reports_running_when_socket_live() {
        let sock = temp_sock("live");
        let _ = std::fs::remove_file(&sock);
        // A bound listener accepts connections (even without calling accept), so
        // the probe sees a live daemon.
        let _listener = UnixListener::bind(&sock).unwrap();
        assert!(matches!(
            prepare_daemon_socket(&sock).unwrap(),
            DaemonStart::AlreadyRunning
        ));
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn pool_config_is_simple_restore() {
        // The pool must not manage/restore scrollback. libshpool's default is
        // `screen`, so the authored config has to opt into `simple` explicitly.
        assert_eq!(POOL_CONFIG.trim(), r#"session_restore_mode = "simple""#);
    }

    #[test]
    fn clears_stale_socket_and_proceeds() {
        let sock = temp_sock("stale");
        let _ = std::fs::remove_file(&sock);
        // Bind then drop: Rust's UnixListener leaves the socket file on disk but
        // nothing listens — exactly the leftover a SIGKILLed daemon leaves.
        let listener = UnixListener::bind(&sock).unwrap();
        drop(listener);
        assert!(sock.exists(), "dropped UnixListener should leave the file");
        assert!(matches!(
            prepare_daemon_socket(&sock).unwrap(),
            DaemonStart::Proceed
        ));
        assert!(!sock.exists(), "stale socket should have been removed");
    }
}
