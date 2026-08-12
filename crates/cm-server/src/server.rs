//! `captain-miao daemon` — the single per-host daemon a *remote* dashboard
//! connects to over ssh (and a local one could connect to directly). One
//! persistent, self-daemonizing process that:
//!
//!   - **hosts the pty pool** (the libshpool daemon, on a dedicated thread — with
//!     the `pty-pool` feature), so pooled sessions survive ssh disconnects; and
//!   - wraps a [`LocalBackend`] (the server-core) and **answers the
//!     [`crate::protocol`]**: a live session subscription (`Snapshot` then
//!     `Delta`/`Removed`, driven by the `sessions/` notify watch) plus
//!     request/response for `ListResumable`/`KillSession`/`OpenSession` and the
//!     host-fs queries the remote picker needs.
//!
//! **Lifetime.** A singleton per host (`server.pid`). `daemon ensure`
//! self-daemonizes (double-fork + setsid) so it detaches from the ssh session
//! that started it and survives disconnects — the ssh tunnel is a separate,
//! disposable `ssh -N -L` child (see `backend::setup_ssh`). Idempotent: a second
//! `ensure` against a live daemon just prints the socket path and exits. It
//! **auto-exits when idle** (no pool sessions and no connected clients for a
//! grace window), and `daemon stop` SIGTERMs it (killing the pool + all its
//! sessions). See `docs/remote-sessions.md` §8/§11.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use notify::Watcher;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::backend::{LocalBackend, OpenSpec};
use crate::protocol::{
    ClientFrame, PROTOCOL_MIN, PROTOCOL_VERSION, ServerFrame, protocol_compatible, read_frame,
    write_frame,
};
use crate::state::{self, LauncherState, SessionKey};
use cm_core::agent::AgentControl;
use cm_core::vitals::VitalsSampler;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Auto-exit after this long with no pool sessions and no connected clients.
const IDLE_GRACE: Duration = Duration::from_secs(300);
/// How often the idle watchdog samples liveness.
const IDLE_CHECK: Duration = Duration::from_secs(30);
/// How often the daemon checks that its control socket still exists on disk
/// (the logind socket-gone wedge — see [`rebind_if_socket_vanished`]). One
/// stat, so it can be frequent enough that a reconnect isn't left waiting.
const SOCKET_CHECK: Duration = Duration::from_secs(5);
/// Pause after a failed `accept` so a persistent fd exhaustion can't spin the
/// serve loop hot while it drains.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(200);
/// How often a subscribed connection is pushed a fresh CPU/memory sample, and
/// so also the window each CPU percentage covers. Slow enough that an idle
/// dashboard costs one tiny frame every few seconds, fast enough that the hosts
/// panel reads as live while someone is looking at it.
const VITALS_INTERVAL: Duration = Duration::from_secs(5);

/// Build the `Opened` reply for an `OpenSession` request. With the `pty-pool`
/// feature, starts the launcher in this host's pool and returns its session
/// name; without it, declines cleanly. May spawn/await child processes, so call
/// it under `block_in_place`.
fn open_session_reply(req_id: u64, spec: OpenSpec) -> ServerFrame {
    #[cfg(feature = "pty-pool")]
    match crate::server_pool::open_in_pool(&spec) {
        Ok(session_name) => ServerFrame::Opened {
            req_id,
            session_name: Some(session_name),
            error: None,
        },
        Err(e) => ServerFrame::Opened {
            req_id,
            session_name: None,
            error: Some(e.to_string()),
        },
    }
    #[cfg(not(feature = "pty-pool"))]
    {
        let _ = spec;
        ServerFrame::Opened {
            req_id,
            session_name: None,
            error: Some("server built without pty-pool support".into()),
        }
    }
}

/// Synchronous entry for `captain-miao daemon <action>`, dispatched from `main`
/// *before* the tokio runtime exists — the daemonize double-fork must precede
/// any thread. `ensure` daemonizes then builds a runtime and serves forever; the
/// others are quick synchronous management calls.
pub(crate) fn dispatch(action: crate::DaemonAction) -> Result<()> {
    use crate::DaemonAction::*;
    match action {
        PrintPath => {
            println!("{}", state::server_sock_path().display());
            Ok(())
        }
        Status => status(),
        Stop { force } => stop(force),
        Ensure => ensure(),
    }
}

/// The pid of a live daemon, if one is running (a fresh, alive `server.pid`).
fn running_pid() -> Option<u32> {
    std::fs::read_to_string(state::server_pid_path())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&pid| state::is_process_alive(pid))
}

/// Where the detached daemon's stdout/stderr land (it has no terminal).
fn daemon_log_path() -> PathBuf {
    state::state_dir().join("logs").join("daemon.log")
}

/// `daemon ensure`: start the daemon if it isn't running, then serve forever.
/// Idempotent — a live daemon just gets the socket path printed. Self-daemonizes
/// so it outlives the ssh session that fired it.
fn ensure() -> Result<()> {
    // Print the socket path *first*, while stdout still reaches the ssh channel
    // (before `daemonize` redirects it). The dashboard reads this to set up its
    // `-L` forward. Flush explicitly since we're about to fork.
    println!("{}", state::server_sock_path().display());
    let _ = std::io::stdout().flush();

    // Acquire the singleton lock BEFORE forking. An `flock` on the pid file is the
    // atomic gate that closes the check-then-bind race between two concurrent
    // `ensure`s (a mere `running_pid()` read would let both pass and both bind,
    // the second stealing the socket). If held, a daemon is up/starting → this is
    // the idempotent no-op (path already printed). The lock lives on the open file
    // description, so it rides the daemonize forks into the grandchild and is held
    // for the daemon's life (released only when `lock` drops on a clean exit).
    let Some(lock) = acquire_singleton_lock() else {
        // A live daemon holds the lock — the idempotent no-op. Unless it's
        // *wedged*: systemd-logind removes `/run/user/<uid>` on last logout
        // (without `loginctl enable-linger`), which unlinks the control socket
        // out from under a daemon that survives holding the deleted inode and
        // the flock. It then answers nothing forever while `ensure` keeps
        // printing a socket path nothing binds. Detect that (lock held, socket
        // unreachable) and clear it, so the next `ensure` — the reconnect loop
        // fires one per attempt — starts a working daemon.
        if !heal_wedged_daemon() {
            eprintln!("captain-miao daemon already running");
            return Ok(());
        }
        // The wedged daemon is gone; fall through and take the lock ourselves.
        let Some(lock) = acquire_singleton_lock() else {
            eprintln!("captain-miao daemon already running");
            return Ok(());
        };
        return run_daemon(lock);
    };
    run_daemon(lock)
}

/// Whether the daemon's control socket currently accepts a connection. A bare
/// connect is enough — it's our own 0600 socket in a 0700 dir, so anything that
/// answers there is the daemon.
fn control_socket_is_live() -> bool {
    std::os::unix::net::UnixStream::connect(state::server_sock_path()).is_ok()
}

/// Recover from the **socket-gone wedge**: a daemon that still holds the
/// singleton lock but whose control socket no longer exists. systemd-logind
/// removes `/run/user/<uid>` on last logout unless the user has
/// `loginctl enable-linger`, taking the socket (and the pool socket) with it
/// while the daemon itself survives — holding deleted inodes and the flock —
/// so every later `daemon ensure` no-ops and prints a path nothing binds.
///
/// The daemon rebinds itself when the runtime dir comes back (see
/// [`rebind_if_socket_vanished`]), so first give it a grace window to do that:
/// a rebind keeps every pooled session alive, which killing would not. Only if
/// it is *still* unreachable do we SIGTERM it and report that the lock is free
/// for a fresh daemon. Returns whether the caller should now try to start one.
fn heal_wedged_daemon() -> bool {
    if control_socket_is_live() {
        return false;
    }
    let Some(pid) = running_pid() else {
        // Lock held but no live pid on file: whoever holds it is mid-startup.
        return false;
    };
    // Grace: the holder may be a daemon still binding, or one about to rebind.
    let deadline = Instant::now() + WEDGE_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if control_socket_is_live() {
            return false;
        }
    }
    tracing::warn!(
        "daemon pid {pid} holds the lock but its control socket is unreachable \
         (runtime dir removed? see `loginctl enable-linger`); restarting it"
    );
    eprintln!(
        "captain-miao: daemon pid {pid} is wedged (control socket gone); restarting it. \
         Run `loginctl enable-linger` to stop the runtime dir being removed at logout."
    );
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    // Wait for it to release the flock so our own acquire can succeed.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if !state::is_process_alive(pid) {
            return true;
        }
    }
    // Still alive after 5s — SIGKILL, then give the kernel a moment to reap it.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(300));
    true
}

/// How long a lock-holding daemon with an unreachable socket is given to come
/// back (bind, or rebind after the runtime dir returns) before it's restarted.
const WEDGE_GRACE: Duration = Duration::from_secs(3);

/// Daemonize, set the process up, and serve forever. Split from [`ensure`] so
/// the wedge-recovery path can re-enter it with a freshly acquired lock.
fn run_daemon(lock: std::fs::File) -> Result<()> {
    // Detach from the ssh session so the daemon survives its disconnect. Returns
    // only in the final grandchild; the parent (which the ssh command awaits) and
    // the intermediate both `exit(0)` inside here. MUST precede any thread.
    daemonize(&daemon_log_path())?;

    // --- From here we are the detached daemon (holding `lock`). ---
    // With the pty pool, libshpool installs the *global* tracing subscriber on
    // its own thread (`start_pool_thread`); a second `set_global` would panic and
    // kill the pool thread, so we must NOT install one here. libshpool's
    // subscriber captures our events too, and stdio is redirected to `daemon.log`.
    // Without the pool there's no libshpool, so set ours up (honors debug mode).
    #[cfg(not(feature = "pty-pool"))]
    crate::init_tracing("daemon");
    state::ensure_sessions_dir()?;
    std::fs::write(state::server_pid_path(), std::process::id().to_string())
        .with_context(|| format!("write {}", state::server_pid_path().display()))?;

    // Host the pty pool on a dedicated thread (blocking libshpool daemon) and
    // wait for its socket before serving, since `open_in_pool` needs it. Started
    // while the process is otherwise idle (no tokio yet), so libshpool's own
    // startup is effectively serialized.
    #[cfg(feature = "pty-pool")]
    start_pool_thread();

    // Build the runtime and serve until SIGTERM / idle-exit. `lock` stays bound
    // across the whole run so the flock is held until the daemon actually exits.
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve());
    drop(lock);
    result
}

/// Acquire the exclusive singleton lock — a non-blocking `flock` on the pid file.
/// Returns the held `File` on success, or `None` if another daemon already holds
/// it. The lock is associated with the open file description, so it survives the
/// daemonize forks (the grandchild inherits the fd) and releases when the returned
/// handle finally drops (a clean daemon exit) or the process dies.
fn acquire_singleton_lock() -> Option<std::fs::File> {
    let path = state::server_pid_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // Don't truncate: the pid content is written separately, and truncating
        // would race a concurrent holder's content.
        .truncate(false)
        .open(&path)
        .ok()?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(file)
}

/// Sessions hosted in *this daemon's pty pool* (those carrying a `pool_session`),
/// as opposed to unrelated non-pool launchers a user may also run on the host.
/// This — not the full `list_sessions()` — is what `stop` kills and what the idle
/// watchdog waits on, so a stray local session neither blocks `stop` nor pins the
/// daemon awake forever. Reads the raw state files (not through a `LocalBackend`):
/// only `pool_session` matters here, and a throwaway backend would re-run the
/// Codex title overlay (a fresh cache = an sqlite read) on every watchdog tick.
fn pool_session_count() -> usize {
    state::read_all_launcher_states()
        .iter()
        .filter(|s| s.pool_session.is_some())
        .count()
}

/// Detach the current (single-threaded) process from its controlling ssh
/// session: double-fork + `setsid` so it's reparented to init and immune to the
/// channel's SIGHUP, `chdir` to `/`, and redirect stdio to `log` (so the
/// `ssh … ensure` command sees EOF and returns). Returns only in the final
/// grandchild — the original and intermediate processes `exit(0)`. **MUST run
/// before any thread is spawned** (fork in a threaded process is unsafe).
fn daemonize(log: &Path) -> Result<()> {
    // First fork: the parent (the ssh command) returns; the child continues.
    match unsafe { libc::fork() } {
        -1 => bail!("fork failed: {}", std::io::Error::last_os_error()),
        0 => {}                     // child
        _ => std::process::exit(0), // parent
    }
    // New session → detached from the ssh channel's session and process group.
    if unsafe { libc::setsid() } == -1 {
        bail!("setsid failed: {}", std::io::Error::last_os_error());
    }
    // Second fork so the daemon can never re-acquire a controlling terminal.
    match unsafe { libc::fork() } {
        -1 => bail!("fork failed: {}", std::io::Error::last_os_error()),
        0 => {}                     // grandchild — the daemon
        _ => std::process::exit(0), // intermediate
    }
    unsafe { libc::chdir(c"/".as_ptr()) };
    redirect_stdio(log)
}

/// Point stdin at `/dev/null` and stdout/stderr at `log`. dup2 copies onto fds
/// 0/1/2, so the temporary `File`s can drop (their fds close) while 0/1/2 keep
/// referring to the underlying files.
fn redirect_stdio(log: &Path) -> Result<()> {
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("open daemon log {}", log.display()))?;
    unsafe {
        libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO);
        libc::dup2(out.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(out.as_raw_fd(), libc::STDERR_FILENO);
    }
    Ok(())
}

/// Start the libshpool pool daemon on a dedicated thread and wait (bounded) for
/// its socket to bind. The thread blocks in libshpool forever; we never enable
/// libshpool's own daemonize, so it doesn't fork — running it alongside tokio is
/// sound (shpool's daemon is itself multi-threaded).
#[cfg(feature = "pty-pool")]
fn start_pool_thread() {
    // The pool lives in *this* process, so a daemon only now starting hosts no
    // sessions — every session reservation left by a previous incarnation is
    // unredeemable (names carry the minting daemon's pid, so nothing will ever
    // attach to them again). Inert, but drop them rather than let them pile up.
    crate::server_pool::prune_pending();
    if let Err(e) = std::thread::Builder::new()
        .name("pty-pool".into())
        .spawn(|| {
            if let Err(e) = crate::pty_pool::run_daemon() {
                tracing::error!("pty-pool daemon exited: {e}");
            }
        })
    {
        tracing::error!("failed to spawn pty-pool thread: {e}");
        return;
    }
    for _ in 0..50 {
        if crate::pty_pool::daemon_is_live() {
            tracing::info!("pty pool socket is live");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tracing::warn!("pty pool socket did not bind in ~5s; open_session will retry");
}

/// `daemon status`: report whether a daemon is running, its socket, and how many
/// pool sessions it holds (read straight from the state files — no round-trip).
fn status() -> Result<()> {
    let sock = state::server_sock_path();
    match running_pid() {
        Some(pid) => {
            println!("daemon:   running (pid {pid})");
            println!("socket:   {}", sock.display());
            println!("sessions: {} (pool)", pool_session_count());
        }
        None => println!("daemon:   not running"),
    }
    Ok(())
}

/// `daemon stop`: SIGTERM the running daemon. Since the daemon *is* the pool,
/// this kills every *pooled* session on the host — so it's guarded unless
/// `--force` when pool sessions are live.
fn stop(force: bool) -> Result<()> {
    let Some(pid) = running_pid() else {
        println!("daemon: not running");
        return Ok(());
    };
    let n = pool_session_count();
    if n > 0 && !force {
        bail!(
            "daemon has {n} live pool session(s); stopping it kills them all. Re-run with --force."
        );
    }
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0 {
        println!("daemon (pid {pid}) stopped");
        Ok(())
    } else {
        bail!(
            "failed to signal daemon pid {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// The async server core: bind the control socket, watch `sessions/`, and serve
/// connections until SIGTERM or idle-exit. Assumes the singleton check + pid file
/// + (with pty-pool) the pool thread are already set up by `ensure`.
async fn serve() -> Result<()> {
    let sock_path = state::server_sock_path();
    let mut listener = bind_control_socket(&sock_path)?;

    // One notify watch on `sessions/`, fanned out to every subscriber via a
    // broadcast channel — each connection diffs the change against what it last
    // sent (so late joiners stay correct after their own snapshot).
    let (changes_tx, _) = broadcast::channel::<()>(64);
    let _watcher = start_sessions_watcher(changes_tx.clone()).context("watch sessions dir")?;

    // The daemon is the host's server-core: on top of the reads it owns the
    // per-session flags sidecar and overlays the pool's live attached bit, so
    // every dashboard watching this host agrees about both.
    let backend = Arc::new(build_server_core());
    let host = host_label();
    // Count of live protocol connections; feeds the idle-exit check below.
    let conns = Arc::new(AtomicUsize::new(0));

    tracing::info!("daemon listening on {} (host {host})", sock_path.display());

    // SIGTERM → clean shutdown. Since the process *is* the pool, exiting tears
    // down every pooled session with it (`daemon stop`'s contract).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;

    // The idle-exit check runs in *this* task (not a separate one) so an accept
    // and an idle tick can't diverge; `biased` polls the accept arm first, so a
    // connection already in the queue always wins over a same-instant idle tick
    // (the residual sub-tick race is self-healed by the client's idempotent
    // `daemon ensure` + reconnect).
    let mut idle_tick = tokio::time::interval(IDLE_CHECK);
    let mut idle_since: Option<Instant> = None;
    let mut socket_tick = tokio::time::interval(SOCKET_CHECK);

    loop {
        tokio::select! {
            biased;
            accepted = listener.accept() => {
                // An accept error must NEVER propagate: this process *is* the
                // pool, so returning here would tear down every session on the
                // host over one transient EMFILE/ECONNABORTED. Log, pause a beat
                // (so a persistent fd exhaustion doesn't spin the loop hot), and
                // keep serving.
                let (stream, _) = match accepted {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("accept failed ({e}); continuing to serve");
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    }
                };
                let backend = backend.clone();
                let changes = changes_tx.subscribe();
                let flag_changes = changes_tx.clone();
                let host = host.clone();
                // Panic-safe connection count: the guard decrements on drop, so a
                // panic inside handle_conn can't leak the count (which would pin
                // the daemon awake forever).
                let guard = ConnGuard::new(conns.clone());
                tokio::spawn(async move {
                    let _guard = guard;
                    if let Err(e) = handle_conn(stream, backend, changes, flag_changes, host).await {
                        tracing::debug!("connection ended: {e}");
                    }
                });
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received; daemon shutting down");
                return Ok(());
            }
            _ = socket_tick.tick() => {
                if let Some(fresh) = rebind_if_socket_vanished(&sock_path) {
                    listener = fresh;
                }
            }
            _ = idle_tick.tick() => {
                // Idle = no connected clients and no *pool* sessions (unrelated
                // local launchers don't count — see `pool_session_count`).
                let busy = conns.load(Ordering::SeqCst) > 0 || pool_session_count() > 0;
                if busy {
                    idle_since = None;
                } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= IDLE_GRACE {
                    tracing::info!("idle {IDLE_GRACE:?} with no pool sessions or clients; exiting");
                    return Ok(());
                }
            }
        }
    }
}

/// The daemon's [`LocalBackend`], wired with the pool's attached-bit probe when
/// this build hosts a pool. Without the pool the bit stays `None` ("unknown"),
/// which the dashboard reads as "don't offer a steal".
fn build_server_core() -> LocalBackend {
    let backend = LocalBackend::server_core();
    #[cfg(feature = "pty-pool")]
    let backend = backend.with_attached_probe(crate::pty_pool::attached_by_session);
    backend
}

/// Bind the control socket: parent dir 0700, socket 0600, so another user on
/// the host can't connect (mirrors the launcher socket). Any stale socket file
/// is cleared first — a SIGKILLed daemon never unlinks its own.
fn bind_control_socket(sock_path: &Path) -> Result<UnixListener> {
    if let Some(parent) = sock_path.parent()
        && std::fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(parent)
            .is_err()
    {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(sock_path);
    let listener =
        UnixListener::bind(sock_path).with_context(|| format!("bind {}", sock_path.display()))?;
    let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

/// Re-bind the control socket if its path no longer exists — the **socket-gone
/// wedge** seen from the daemon's side. systemd-logind removes
/// `/run/user/<uid>` at last logout unless the user has `loginctl
/// enable-linger`; the daemon survives, but its listener now refers to a
/// deleted inode that nothing can dial, so it would answer nothing forever
/// while `daemon ensure` kept printing a path nothing binds.
///
/// Rebinding here is strictly better than the `ensure`-side restart
/// ([`heal_wedged_daemon`], the backstop): the pty pool lives in *this*
/// process, so every pooled session survives. It only succeeds once the
/// runtime dir is back (a non-root user can't recreate `/run/user/<uid>`
/// itself), which is exactly the next login — until then the tick is a cheap
/// stat that fails and retries.
fn rebind_if_socket_vanished(sock_path: &Path) -> Option<UnixListener> {
    if sock_path.exists() {
        return None;
    }
    match bind_control_socket(sock_path) {
        Ok(l) => {
            tracing::warn!(
                "control socket {} had vanished (runtime dir removed?); rebound it. \
                 `loginctl enable-linger` prevents this",
                sock_path.display()
            );
            Some(l)
        }
        Err(e) => {
            tracing::debug!(
                "control socket {} is gone and cannot be rebound yet: {e}",
                sock_path.display()
            );
            None
        }
    }
}

/// RAII connection counter: increments on construction, decrements on drop — so
/// the count is correct even if `handle_conn` panics (a bare `fetch_sub` after
/// the call would leak on unwind, pinning the idle watchdog's `conns > 0`).
struct ConnGuard(Arc<AtomicUsize>);

impl ConnGuard {
    fn new(conns: Arc<AtomicUsize>) -> Self {
        conns.fetch_add(1, Ordering::SeqCst);
        Self(conns)
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve one dashboard connection: handshake, then multiplex the subscription
/// push with request/response until the peer hangs up.
async fn handle_conn(
    stream: tokio::net::UnixStream,
    backend: Arc<LocalBackend>,
    mut changes: broadcast::Receiver<()>,
    flag_changes: broadcast::Sender<()>,
    host: String,
) -> std::io::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    // Expect Hello first; reply Welcome. An unusable version still gets a
    // Welcome (so the client can read our version and report), then we close.
    let Some(ClientFrame::Hello { protocol, .. }) = read_frame(&mut rd).await? else {
        return Ok(());
    };
    write_frame(
        &mut wr,
        &ServerFrame::Welcome {
            server_version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            host,
        },
    )
    .await?;
    // Only a client *below* the floor is refused — a newer one is fine, since
    // both sides now decode unknown frames/fields tolerantly (protocol §3).
    if !protocol_compatible(protocol) {
        tracing::warn!("refusing client protocol {protocol} (floor is {PROTOCOL_MIN})");
        return Ok(());
    }

    let mut subscribed = false;
    let mut last_sent: HashMap<SessionKey, LauncherState> = HashMap::new();
    // One sampler per connection, so each client's CPU percentage is measured
    // over its own push interval and the server keeps no cross-connection state
    // (the same rule the session diff follows). The first tick fires
    // immediately: memory is a straight reading and shows at once, while the
    // CPU figure needs a second sample and appears one interval later.
    let mut vitals = VitalsSampler::new();
    let mut vitals_tick = tokio::time::interval(VITALS_INTERVAL);
    // A client that was unreachable for a while (a suspended laptop) must not
    // be handed a burst of back-dated samples on its return.
    vitals_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            frame = read_frame::<_, ClientFrame>(&mut rd) => {
                let Some(frame) = frame? else { return Ok(()) };
                match frame {
                    ClientFrame::Hello { .. } => {} // already handshaken; ignore
                    ClientFrame::Subscribe => {
                        let sessions = backend.list_sessions();
                        last_sent = sessions.iter().map(|s| (s.key(), s.clone())).collect();
                        write_frame(&mut wr, &ServerFrame::Snapshot { sessions }).await?;
                        subscribed = true;
                    }
                    ClientFrame::ListResumable { req_id, limit } => {
                        let (candidates, errors) =
                            tokio::task::block_in_place(|| backend.list_resumable(limit));
                        write_frame(&mut wr, &ServerFrame::Resumable { req_id, candidates, errors }).await?;
                    }
                    ClientFrame::KillSession { req_id, key } => {
                        // The key is re-resolved to a live pid inside the
                        // backend, so a stale mirror can't make us signal a
                        // recycled pid.
                        let ok = backend.kill_session(&key);
                        write_frame(&mut wr, &ServerFrame::Killed { req_id, ok }).await?;
                    }
                    ClientFrame::OpenSession { req_id, spec } => {
                        let reply = tokio::task::block_in_place(|| open_session_reply(req_id, spec));
                        write_frame(&mut wr, &reply).await?;
                    }
                    ClientFrame::SetSessionFlags { req_id, key, flags } => {
                        let ok = backend.set_session_flags(&key, flags);
                        // Wake every subscriber (this one included) so the new
                        // flags reach each dashboard watching the host as an
                        // ordinary Delta, not just the one that set them.
                        if ok {
                            let _ = flag_changes.send(());
                        }
                        write_frame(&mut wr, &ServerFrame::FlagsSet { req_id, ok }).await?;
                    }
                    ClientFrame::ListRecentDirs { req_id } => {
                        let cwds = backend.recent_dirs();
                        write_frame(&mut wr, &ServerFrame::RecentDirs { req_id, cwds }).await?;
                    }
                    ClientFrame::CompletePath { req_id, prefix } => {
                        let matches = backend.complete_path(&prefix);
                        write_frame(&mut wr, &ServerFrame::PathCompletions { req_id, matches }).await?;
                    }
                    ClientFrame::CheckDir { req_id, path } => {
                        let exists = backend.dir_exists(&path);
                        write_frame(&mut wr, &ServerFrame::DirChecked { req_id, exists }).await?;
                    }
                    // A newer client's frame we don't know. Ignoring it keeps
                    // the connection alive (protocol §3 forward tolerance); a
                    // request-shaped one simply never gets its reply, which the
                    // client already treats as unreachable.
                    ClientFrame::Unknown => {
                        tracing::debug!("ignoring an unknown client frame (newer peer?)");
                    }
                }
            }
            _ = vitals_tick.tick(), if subscribed => {
                // A host whose counters we can't read reports nothing at all
                // rather than a frame of nulls — and costs no traffic for it.
                let sample = vitals.sample();
                if !sample.is_empty() {
                    write_frame(&mut wr, &ServerFrame::Vitals { vitals: sample }).await?;
                }
            }
            recv = changes.recv(), if subscribed => {
                match recv {
                    // A real change, or a lag (we dropped some signals): either
                    // way re-read and diff, which is self-correcting.
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        push_changes(&mut wr, &backend, &mut last_sent).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

/// Re-read the live sessions and push a `Delta` for each new/changed one and a
/// `Removed` for each gone, against what this connection last saw.
async fn push_changes(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    backend: &LocalBackend,
    last_sent: &mut HashMap<SessionKey, LauncherState>,
) -> std::io::Result<()> {
    let cur: HashMap<SessionKey, LauncherState> = backend
        .list_sessions()
        .into_iter()
        .map(|s| (s.key(), s))
        .collect();
    for key in last_sent.keys() {
        if !cur.contains_key(key) {
            write_frame(wr, &ServerFrame::Removed { key: key.clone() }).await?;
        }
    }
    for (pid, state) in &cur {
        if last_sent.get(pid) != Some(state) {
            write_frame(
                wr,
                &ServerFrame::Delta {
                    state: Box::new(state.clone()),
                },
            )
            .await?;
        }
    }
    *last_sent = cur;
    Ok(())
}

/// Watch the `sessions/` dir; signal the broadcast on any non-Access change.
/// Also watches Codex's title-store WAL (`AgentControl::Codex.watch_paths()`) —
/// a rename touches only that sqlite, so without this wake a remote rename
/// wouldn't reach subscribers until some other session event fired. The wake
/// just triggers the normal re-read + diff; the actual sqlite read is heavily
/// throttled inside `LocalBackend`'s title overlay, and an unchanged diff
/// pushes nothing. Best-effort: a missing wal simply isn't watched.
fn start_sessions_watcher(tx: broadcast::Sender<()>) -> notify::Result<notify::RecommendedWatcher> {
    let dir = state::sessions_dir();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Skip Access (open/close/read) — our own reads would otherwise spin us.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let _ = tx.send(());
    })?;
    w.watch(&dir, notify::RecursiveMode::NonRecursive)?;
    for path in AgentControl::Codex.watch_paths() {
        let _ = w.watch(&path, notify::RecursiveMode::NonRecursive);
    }
    Ok(w)
}

/// Best-effort human label for this host, surfaced in the handshake. The
/// dashboard sets its own display label per the hosts list, so this is only a
/// diagnostic hint.
fn host_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "captain-miao-host".to_string())
}
