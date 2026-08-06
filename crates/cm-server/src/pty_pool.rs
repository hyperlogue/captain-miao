//! Phase 3 pty pool — the `pty-daemon` and `attach` entrypoints that embed
//! libshpool. Only compiled with the `pty-pool` feature (the remote/Linux
//! build). The macOS dashboard client never runs these: it spawns
//! `ssh -t <host> miao-server attach <name>`, so `attach` (and the daemon the
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

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use libshpool::Args;
use serde::Serialize;
use serde::de::DeserializeOwned;
use shpool_protocol::{ConnectHeader, ListReply, SessionStatus, VersionHeader};

/// captain-miao's private pool socket — resolved in cm-core so the server and
/// the `miao-client` list/attach tool agree on the path. A dedicated
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

/// Serialize `value` the way libshpool's daemon expects (msgpack, struct-map).
/// Mirrors miao-client's `pool.rs`; the codec is pinned there by
/// `list_codec_roundtrips` and here by `busy_check_codec_roundtrips`, so a
/// libshpool protocol bump surfaces in both crates' tests.
fn shpool_encode<T: Serialize, W: Write>(value: &T, w: W) -> Result<()> {
    let mut ser = rmp_serde::Serializer::new(w).with_struct_map();
    value.serialize(&mut ser).context("encoding shpool frame")?;
    Ok(())
}

/// Read one msgpack value from `r` — rmp reads exactly one value's bytes, so
/// sequential calls on the same stream stay framed.
fn shpool_decode<T: DeserializeOwned, R: Read>(r: R) -> Result<T> {
    rmp_serde::from_read(r).context("decoding shpool frame")
}

/// The pool daemon's read-only `List`, as `session name → has a client
/// attached`. One connection, no side effects.
fn pool_attached_map() -> Result<std::collections::HashMap<String, bool>> {
    let stream =
        UnixStream::connect(pool_socket_path()).context("connecting to the pty pool socket")?;
    let _version: VersionHeader =
        shpool_decode(&stream).context("reading pool daemon version header")?;
    shpool_encode(&ConnectHeader::List, &stream).context("sending pool list request")?;
    let reply: ListReply = shpool_decode(&stream).context("reading pool session list")?;
    Ok(reply
        .sessions
        .into_iter()
        .map(|s| (s.name, matches!(s.status, SessionStatus::Attached)))
        .collect())
}

/// The attached-bit overlay the daemon's server-core stamps onto the rows it
/// serves (`docs/remote-sessions.md` §10.2), so a dashboard can show
/// attached/detached per row and offer a steal only when one actually applies.
/// A pool that can't be reached yields an empty map — every row's bit stays
/// `None` ("unknown"), never a false "detached".
pub(crate) fn attached_by_session() -> std::collections::HashMap<String, bool> {
    pool_attached_map().unwrap_or_else(|e| {
        tracing::debug!(target: "captain_miao::pool", "attached-bit probe failed: {e}");
        Default::default()
    })
}

/// Whether the named pool session currently has a client attached.
/// `Ok(None)` = the pool has no such session.
fn pool_session_attached(name: &str) -> Result<Option<bool>> {
    Ok(pool_attached_map()?.get(name).copied())
}

/// Pre-flight for a plain interactive reattach — never for the
/// `--background --cmd` create path, where the daemon is deliberately minting
/// a fresh session. Refuses (with a distinct exit code, so the held window and
/// anything watching the process can tell the cases apart) the two situations
/// libshpool itself handles badly:
///
/// * **Stale name** ([`crate::state::ATTACH_EXIT_STALE`]): no live launcher
///   owns `name`. shpool never drops a dead-detached session from its table,
///   and an attach to it — or to a name it has never seen — silently *creates*
///   a bare login shell wearing the `cm-…` name. The check retries briefly:
///   right after a create, the attach window can beat the launcher's first
///   state-file write.
/// * **Busy** ([`crate::state::ATTACH_EXIT_BUSY`]): the session already has a
///   client. libshpool's own refusal prints to stderr but **exits 0**,
///   indistinguishable from a clean detach (the dashboard treats a closed
///   attach window as a detach), so it must be caught before libshpool runs.
///   Best-effort: a `List` failure falls through to libshpool, whose own
///   connect surfaces the error; a client attaching between this check and
///   ours hits libshpool's busy path as before.
///
/// `force` (the steal) skips only the **busy** half: libshpool's own attach
/// client implements the whole steal — on a busy session it sends a `Detach`,
/// kicking the other client (whose attach process simply exits, which that
/// dashboard already treats as a window-closed detach), then retries the dial.
/// The **stale-name** half is never skipped: resurrecting a dead name as a bare
/// login shell is not something a user can mean to force.
fn guard_plain_reattach(name: &str, force: bool) {
    let live = (0..10).find_map(|i| {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(250));
        }
        let states = crate::state::read_all_launcher_states();
        crate::state::find_live_pool_session(&states, name, crate::state::is_process_alive)
            .map(|s| s.launcher_pid)
    });
    if live.is_none() {
        eprintln!(
            "no live captain-miao session owns pool session {name:?} (it likely exited); \
             refusing to attach — attaching would resurrect the name as a bare shell. \
             Resume the session from the dashboard instead."
        );
        std::process::exit(crate::state::ATTACH_EXIT_STALE);
    }
    match pool_session_attached(name) {
        // `force` means the caller has already decided to kick the other
        // client, so libshpool's own steal is allowed to run.
        Ok(Some(true)) if force => {
            eprintln!("stealing pool session {name:?} from its attached client");
        }
        Ok(Some(true)) => {
            eprintln!(
                "pool session {name:?} already has a terminal attached (the pool is one \
                 client at a time); detach the other client first, or attach with --force \
                 to steal it"
            );
            std::process::exit(crate::state::ATTACH_EXIT_BUSY);
        }
        // A live launcher whose session the pool doesn't know: the pool was
        // restarted out from under it (or the name never existed). Attaching
        // would create a bare shell under the name — refuse.
        Ok(None) => {
            eprintln!(
                "the pty pool has no session named {name:?} (was the pool restarted?); \
                 refusing to attach"
            );
            std::process::exit(crate::state::ATTACH_EXIT_STALE);
        }
        Ok(Some(false)) | Err(_) => {}
    }
}

/// Attach to a pool session, proxying its pty to this terminal. With
/// `cmd`/`background` set, instead *creates* a session running `cmd` and detaches
/// (how the server starts a launcher in the pool); otherwise a plain interactive
/// reattach (what the client's ssh window runs), pre-flighted by
/// [`guard_plain_reattach`]. Always `--no-daemonize`:
/// captain-miao manages its own daemon (named `pty-daemon`, not shpool's default
/// `daemon`), so shpool's auto-launch — which would re-exec `<exe> daemon` — must
/// not fire.
pub(crate) fn run_attach(
    name: String,
    cmd: Option<String>,
    dir: Option<String>,
    background: bool,
    force: bool,
    log_file: Option<String>,
) -> Result<()> {
    if cmd.is_none() && !background {
        guard_plain_reattach(&name, force);
    }
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
    // libshpool's own steal: busy → send `Detach` → retry the dial (up to
    // 20×100ms). The session itself is undisturbed — a detach is clean, nothing
    // restarts — and the kicked client's attach process just exits.
    if force {
        sub.push("--force");
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

    /// Pin this crate's copy of the shpool wire codec (struct-map msgpack,
    /// value-framed) — the busy pre-check's `List` must keep decoding what the
    /// daemon emits. Mirrors cm-client's `list_codec_roundtrips`.
    #[test]
    fn busy_check_codec_roundtrips() {
        let reply = ListReply {
            sessions: vec![shpool_protocol::Session {
                name: "cm-claude-1-1".into(),
                started_at_unix_ms: 0,
                last_connected_at_unix_ms: None,
                last_disconnected_at_unix_ms: None,
                status: SessionStatus::Attached,
            }],
        };
        let mut buf: Vec<u8> = Vec::new();
        shpool_encode(
            &VersionHeader {
                version: "0.0.0".into(),
            },
            &mut buf,
        )
        .unwrap();
        shpool_encode(&reply, &mut buf).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let _version: VersionHeader = shpool_decode(&mut cursor).unwrap();
        let got: ListReply = shpool_decode(&mut cursor).unwrap();
        assert_eq!(got.sessions[0].name, "cm-claude-1-1");
        assert!(matches!(got.sessions[0].status, SessionStatus::Attached));
    }

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
