//! One launcher per session: persistence and cooperative control around an
//! opaque configured adapter task. Agent execution modes stay behind the adapter.

use crate::agent::AgentKind;
use crate::agent::session::*;
use crate::cli::ClipboardShims;
use crate::state::{self, HostId, LauncherState, SessionStatus};
use anyhow::{Context, Result};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::UnixListener;
mod daemon_watch;
pub async fn run(
    agent: AgentKind,
    cwd: &str,
    agent_args: &[String],
    pool_session: Option<String>,
    launch_id: Option<String>,
    shims: ClipboardShims,
) -> Result<()> {
    // Ctrl-C reaches both processes in the terminal's foreground group. Catch
    // it in the launcher so only the agent decides whether to interrupt a turn
    // or exit; the default action would kill us without removing session state.
    // Install before spawning. A caught handler resets on exec, unlike SIG_IGN,
    // so the agent retains its own interrupt behavior. Do not forward it again.
    let _interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("Failed to install launcher interrupt handler")?;

    state::ensure_sessions_dir()?;

    let launcher_pid = std::process::id();
    // The dashboard (or server) owns the session↔window binding for any launch it
    // spawned — it threads a token (`--launch-id` locally, `--pool-session`
    // remotely) and records the window itself (§6). So self-report
    // `window_id` *only* when neither token is present — a hand-launched
    // `miao claude` in a real Kitty window, where nothing else can supply
    // it and the resolver falls back to this field. A headless/pooled launcher
    // (token set) never touches the terminal.
    let window_id = if launch_id.is_none() && pool_session.is_none() {
        crate::terminal::current_window()
    } else {
        None
    };
    // Stamp the terminal instance unconditionally (unlike `window_id`): even a
    // token-bearing (dashboard/pooled) launcher runs inside its own pane and
    // knows its terminal, and the dashboard needs it to classify the row's
    // window-op namespace either way — a Kitty window id and a zellij pane id
    // both look like a bare integer.
    let terminal = crate::terminal::current_terminal_identity();
    // …and the terminfo the agent will render against. Read here rather than
    // derived anywhere else: inside a pool pty this is the *creating* client's
    // `TERM` (possibly rewritten by the host's pool wrapper), which no other
    // process is in a position to know. See `LauncherState::terminfo`.
    let terminfo = std::env::var("TERM")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    // Resolved here because this is the same moment the agent makes the same
    // reads: the flag is a launch-time snapshot of whether its TUI will occupy
    // the alternate screen (see `LauncherState::alt_screen`). Pooled only —
    // nothing ever primes a non-pool window.
    let alt_screen = pool_session.is_some() && agent.uses_alt_screen(agent_args);
    // …and whether it will push the kitty keyboard protocol, from the same
    // environment the agent's own gate is about to read. Codex pushes on the
    // primary screen too; Grok's verified recipe still requires fullscreen.
    let kitty_keyboard = pool_session.is_some()
        && agent.uses_kitty_keyboard(terminfo.as_deref().unwrap_or_default(), alt_screen);

    let mut launcher_state = LauncherState {
        agent,
        codex_mode: Default::default(),
        codex_connected: None,
        codex_control: false,
        launcher_pid,
        session_id: None,
        child_session_ids: Vec::new(),
        window_id,
        tab_id: None,
        cwd: cwd.to_string(),
        status: SessionStatus::Starting,
        background_commands: Vec::new(),
        last_tool: None,
        updated_at: LauncherState::now(),
        active_since: None,
        last_prompt: None,
        child_pid: None,
        last_error: None,
        last_error_at: None,
        cleanup: None,
        context_tokens: None,
        context_window: None,
        model: None,
        name: None,
        first_prompt: None,
        pool_session,
        launch_id,
        terminal,
        terminfo,
        alt_screen,
        kitty_keyboard,
        // Host-owned overlays: the server-core stamps these onto the rows it
        // serves. The launcher never writes them (single-writer rule).
        flags: None,
        attached: None,
        host: HostId::local(),
    };
    launcher_state.write()?;

    // Launcher socket for receiving hook events. Create the dir 0700 and the
    // socket 0600 so a same-host other user can't connect to it and inject
    // hook events. `create` errors if the dir already exists, so fall back to
    // `create_dir_all` (which tolerates that) without imposing the mode.
    let sock_dir = state::runtime_dir().join("launchers");
    if std::fs::DirBuilder::new()
        .recursive(false)
        .mode(0o700)
        .create(&sock_dir)
        .is_err()
    {
        std::fs::create_dir_all(&sock_dir)?;
    }
    // Runtime files no longer live in a dir the OS clears for us (see
    // `state::runtime_dir`), so a launcher that died without unwinding — a hard
    // reboot, a SIGKILL — leaves its socket and settings file behind for good.
    // Reap those here, mirroring `sweep_dead_launcher_logs`.
    sweep_dead_launcher_runtime_files(&sock_dir);
    let sock_path = sock_dir.join(format!("{launcher_pid}.sock"));
    let _ = std::fs::remove_file(&sock_path);

    let listener = bind_hook_socket(&sock_path)?;

    let settings_path = sock_dir.join(format!("{launcher_pid}-settings.json"));
    let shim_dir = match shims {
        ClipboardShims::Install => match crate::clipboard::shim::ensure_farm() {
            Ok(dir) => Some(dir),
            Err(e) => {
                tracing::warn!("could not install the clipboard shims: {e:#}");
                None
            }
        },
        ClipboardShims::Skip => None,
    };

    let adapter = match agent.configure(&crate::config::AgentConfig::load()) {
        Ok(adapter) => adapter,
        Err(error) => {
            hold_failed_launch(
                launcher_pid,
                &sock_path,
                &settings_path,
                &mut launcher_state,
                error,
            )
            .await
        }
    };
    let (updates, incoming) = SessionPublisher::channel();
    let (commands, control) = tokio::sync::mpsc::unbounded_channel();
    let task = adapter.run(SessionContext {
        request: LaunchRequest {
            cwd: cwd.to_owned(),
            args: agent_args.to_vec(),
            socket: sock_path.clone(),
            settings: settings_path.clone(),
            shim: shim_dir,
        },
        state: launcher_state.clone(),
        listener,
        updates,
        commands: SessionCommands(control),
    });
    let cleanup = CleanupGuard {
        launcher_pid,
        sock_path: sock_path.clone(),
        settings_path: settings_path.clone(),
    };
    let code = match supervise(task, incoming, commands, &mut launcher_state).await {
        Ok(exit) => exit.code,
        Err(error) => {
            hold_failed_launch(
                launcher_pid,
                &sock_path,
                &settings_path,
                &mut launcher_state,
                error,
            )
            .await
        }
    };
    cleanup.into_inner();
    std::process::exit(code)
}

/// Drive an opaque adapter alongside persistence and cooperative stop requests.
/// A failed state write never cancels the adapter or loses its cleanup control.
pub(crate) async fn supervise(
    task: SessionTask,
    updates: tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    commands: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    state: &mut LauncherState,
) -> Result<SessionExit> {
    let pool = state.pool_session.clone();
    let shutdown = async {
        tokio::select! {
            _ = wait_for_termination_signal() => {},
            _ = wait_until_minting_daemon_gone(pool.as_deref()) => {},
        }
    };
    drive(task, updates, commands, state, shutdown, persist).await
}

async fn drive(
    task: SessionTask,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    commands: tokio::sync::mpsc::UnboundedSender<SessionCommand>,
    state: &mut LauncherState,
    shutdown: impl std::future::Future<Output = ()>,
    mut write: impl FnMut(&LauncherState) -> Result<()>,
) -> Result<SessionExit> {
    tokio::pin!(task, shutdown);
    let mut stopping = false;
    let mut updates_open = true;
    let mut retry = None;
    loop {
        tokio::select! {
            result = &mut task => {
                while let Ok(update) = updates.try_recv() {
                    apply_update_with(update, state, &mut write);
                }
                return result;
            },
            update = updates.recv(), if updates_open => {
                match update {
                    Some(update) => {
                        retry = (!apply_update_with(update, state, &mut write)).then(|| tokio::time::Instant::now() + std::time::Duration::from_secs(1));
                    },
                    None => updates_open = false,
                }
            },
            _ = &mut shutdown, if !stopping => {
                stopping = true;
                let _ = commands.send(SessionCommand::Stop);
            },
            _ = sleep_until_opt(retry) => {
                retry = write(state).err().map(|_| tokio::time::Instant::now() + std::time::Duration::from_secs(1));
            }
        }
    }
}

fn persist(state: &LauncherState) -> Result<()> {
    state::ensure_sessions_dir()?;
    state.write()
}

#[cfg(test)]
pub(crate) fn apply_update(update: SessionUpdate, state: &mut LauncherState) -> bool {
    apply_update_with(update, state, &mut persist)
}

fn apply_update_with(
    update: SessionUpdate,
    state: &mut LauncherState,
    write: &mut impl FnMut(&LauncherState) -> Result<()>,
) -> bool {
    *state = update.state;
    let result = write(state);
    let saved = result.is_ok();
    if let Some(receipt) = update.receipt {
        let _ = receipt.send(result);
    } else if let Err(error) = result {
        tracing::warn!("could not persist session; retaining task and retrying: {error:#}");
    }
    saved
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Resolve when the launcher is asked to terminate via SIGTERM or SIGHUP.
/// SIGINT is intentionally excluded — Ctrl-C in the shared terminal is the
/// agent's interrupt, not a request to tear the launcher down. Each signal
/// stream that can't be installed simply never fires (rather than busy-looping),
/// preserving the old default-action behaviour for that one signal.
pub(crate) async fn wait_for_termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    async fn recv(stream: &mut Option<tokio::signal::unix::Signal>) {
        match stream {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sighup = signal(SignalKind::hangup()).ok();
    tokio::select! {
        _ = recv(&mut sigterm) => {}
        _ = recv(&mut sighup) => {}
    }
}

/// Resolve when the daemon that minted this pooled session is gone.
///
/// A non-pooled launcher (or a name we cannot parse) waits forever — those
/// sessions do not live in the pool, so a daemon restart is not their
/// concern. PID-file notifications and process-exit observation are separate:
/// a crash need not rewrite the file, and a replacement can reuse its path.
pub(crate) async fn wait_until_minting_daemon_gone(pool_session: Option<&str>) {
    let Some(minting) = pool_session.and_then(state::pool_session_daemon_pid) else {
        std::future::pending::<()>().await;
        return;
    };
    daemon_watch::wait(minting, &state::server_pid_path()).await;
}

// =============================================================================
// The hook socket, and cleaning up after it
// =============================================================================

/// Bind the launcher's hook socket, owner-only and non-blocking. Factored out
/// because [`restore_hook_socket`] has to repeat it exactly when the socket is
/// removed mid-session; the two must not drift on the permission bits.
pub(crate) fn bind_hook_socket(sock_path: &Path) -> Result<UnixListener> {
    let listener = std::os::unix::net::UnixListener::bind(sock_path)?;
    let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600));
    listener.set_nonblocking(true)?;
    Ok(UnixListener::from_std(listener)?)
}

/// `(device, inode)` of `path`, the identity the socket health check compares.
/// Mere existence isn't enough: a path that was unlinked and re-bound by someone
/// else exists again while *our* listener is orphaned on the old inode.
pub(crate) fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// Whether the socket we bound is no longer reachable at its path — either
/// unlinked, or replaced by a different inode. `bound` is the identity recorded
/// at bind time; `None` there means the stat failed then, leaving nothing to
/// compare, so presence alone has to count as intact.
pub(crate) fn hook_socket_lost(sock_path: &Path, bound: Option<(u64, u64)>) -> bool {
    match file_identity(sock_path) {
        None => true,
        Some(now) => bound.is_some_and(|then| now != then),
    }
}

/// Re-bind the hook socket after something removed it, returning the new
/// identity to track (or `None` if the re-bind failed and we should keep
/// trying). Recreates the containing dir too, since whatever took the socket
/// may have taken the whole tree.
///
/// This is the half of the `$TMPDIR`-reaping fix that helps a session which is
/// *already* running: moving `runtime_dir` only protects launchers started
/// afterwards. The agent resolves the socket path from the `--settings` file it
/// read at startup, and that path is unchanged, so hooks fired after the re-bind
/// connect to the fresh listener and the session recovers on its own. The
/// settings file is deliberately not rewritten — the agent read it once at
/// startup, so restoring it would recover nothing.
pub(crate) fn restore_hook_socket(
    listener: &mut UnixListener,
    sock_path: &Path,
) -> Option<(u64, u64)> {
    if let Some(dir) = sock_path.parent() {
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    let _ = std::fs::remove_file(sock_path);
    match bind_hook_socket(sock_path) {
        Ok(fresh) => {
            *listener = fresh;
            tracing::warn!(
                "hook socket {} had been removed; re-bound it. Hook events fired \
                 while it was missing were lost, so this row's status may have \
                 been stale until now.",
                sock_path.display()
            );
            file_identity(sock_path)
        }
        Err(e) => {
            tracing::error!(
                "hook socket {} was removed and could not be re-bound: {e}. \
                 This session's status updates are being dropped.",
                sock_path.display()
            );
            None
        }
    }
}

/// Remove sockets, settings and pasted images left by launchers that have
/// exited. `$TMPDIR` used to clear these for us; [`state::runtime_dir`]'s
/// fallback now persists across reboots, so they need reaping on the same terms
/// as the per-launcher logs — on every launcher startup, leaving live (and
/// just-crashed) launchers' files alone.
pub(crate) fn sweep_dead_launcher_runtime_files(sock_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(sock_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid) = name
            .strip_suffix("-codex.sock")
            .or_else(|| name.strip_suffix(".sock"))
            .or_else(|| name.strip_suffix("-settings.json"))
            .or_else(|| name.strip_suffix("-images"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if !state::is_process_alive(pid) {
            if name.ends_with("-images") {
                let _ = std::fs::remove_dir_all(entry.path());
            } else {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Remove the per-launcher state file, socket, and hook-settings file. Called
/// on every `run` exit path (build/spawn failure, the hook loop's normal and
/// error exits, a termination signal) so a dead launcher never leaves a ghost
/// session for the dashboard or a stale socket for a later launcher to collide
/// with.
pub(crate) fn cleanup_launcher_files(launcher_pid: u32, sock_path: &Path, settings_path: &Path) {
    LauncherState::remove(launcher_pid);
    let _ = std::fs::remove_file(sock_path);
    let _ = std::fs::remove_file(settings_path);
    let _ = std::fs::remove_dir_all(crate::clipboard::shim::session_image_dir(launcher_pid));
}

/// Hold a launch that never produced an agent (direnv blocked on the session's
/// `.envrc`, a missing `claude`/`codex`, a spawn failure), or whose agent
/// failed before reporting any status ([`startup_exit_error`]), as a visible
/// `FailedToStart` row instead of letting it vanish. We print the error to the
/// window below any agent output, stamp it onto the state file as
/// `last_error`, and **block** until the user dismisses it — closing the window
/// (kitty SIGHUP) or killing the row (the dashboard SIGTERMs `launcher_pid`,
/// since there's no child) — then tear the files down.
///
/// **The block is what keeps the window**, not the terminal: the dashboard
/// spawns session windows `hold: false`, because a terminal's own hold is not a
/// freeze (kitty runs the user's login shell in the window once the command
/// exits). So a launcher that returns takes its window with it, and one that
/// wants to be read stays alive — which is what this does.
///
/// Recording the failure on the state file (rather than driving the terminal)
/// keeps the launcher terminal-free: the dashboard surfaces the row and focuses
/// the window, and because the row rides the normal `LauncherState` channel a
/// remote launcher's failure surfaces the same way (`docs/remote-sessions.md`
/// §3). Never returns — the row *is* the surfacing — so the caller's `?` paths
/// don't propagate the error up to a now-redundant top-level print.
pub(crate) async fn hold_failed_launch(
    launcher_pid: u32,
    sock_path: &Path,
    settings_path: &Path,
    state: &mut LauncherState,
    error: anyhow::Error,
) -> ! {
    let msg = format!("{error:#}");
    eprintln!("captain-miao: {msg}");
    tracing::warn!("launch failed; holding window as FailedToStart: {msg}");
    state.status = SessionStatus::FailedToStart;
    state.codex_control = false;
    state.set_last_error(Some(msg));
    state.child_pid = None;
    state.active_since = None;
    state.updated_at = LauncherState::now();
    let _ = state.write();
    // Idle until the window is closed, the row is killed, or (pooled) the
    // daemon that reserved this name is gone; then clean up so nothing leaks.
    tokio::select! {
        _ = wait_for_termination_signal() => {}
        _ = wait_until_minting_daemon_gone(state.pool_session.as_deref()) => {}
    }
    cleanup_launcher_files(launcher_pid, sock_path, settings_path);
    std::process::exit(1);
}

/// The error to hold for an agent that failed before reporting any status. A
/// row still `Starting` has seen no hook or transcript signal; Codex stays
/// there until its first prompt, so a startup failure (an unreachable backend)
/// exits from this state. A clean exit and an exit from a hangup, interrupt or
/// termination are the user's or terminal's doing, matching the attach
/// wrapper's refusal test.
pub(crate) fn startup_exit_error(
    agent: AgentKind,
    status: &SessionStatus,
    exit: std::process::ExitStatus,
) -> Option<anyhow::Error> {
    use std::os::unix::process::ExitStatusExt;
    const USER_ENDED_SIGNALS: [i32; 3] = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM];
    if *status != SessionStatus::Starting || exit.success() {
        return None;
    }
    let user_ended = match (exit.code(), exit.signal()) {
        (Some(code), _) => USER_ENDED_SIGNALS.iter().any(|sig| code == 128 + sig),
        (None, Some(sig)) => USER_ENDED_SIGNALS.contains(&sig),
        (None, None) => false,
    };
    (!user_ended).then(|| {
        anyhow::anyhow!(
            "{} exited during startup ({exit}); its output, if any, is above",
            agent.label()
        )
    })
}

/// Scope guard that runs `cleanup_launcher_files` on drop. Installed once the
/// agent child is live so an early `?` return from `run` (e.g. a `child.wait()`
/// error) still tears the files down. The happy path disarms it via
/// `into_inner`, which performs the cleanup and consumes the guard before the
/// `std::process::exit` that wouldn't otherwise run destructors.
pub(crate) struct CleanupGuard {
    pub(crate) launcher_pid: u32,
    pub(crate) sock_path: PathBuf,
    pub(crate) settings_path: PathBuf,
}

impl CleanupGuard {
    /// Perform the cleanup once and consume the guard (so `Drop` is a no-op).
    fn into_inner(self) {
        cleanup_launcher_files(self.launcher_pid, &self.sock_path, &self.settings_path);
        std::mem::forget(self);
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        cleanup_launcher_files(self.launcher_pid, &self.sock_path, &self.settings_path);
    }
}

#[cfg(test)]
mod tests;
