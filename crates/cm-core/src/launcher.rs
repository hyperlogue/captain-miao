use anyhow::{Context, Result};
use notify::Watcher;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;

use crate::agent::{
    AgentActivity, AgentControl, BgSeedKind, BgShell, TranscriptScan, TranscriptStats,
};
use crate::cli::ClipboardShims;
use crate::learned;
use crate::state::{self, HookEvent, HookMessage, HostId, LauncherState, SessionStatus};

/// Run an agent under captain-miao supervision. The launcher is single-backend
/// per process: it picks `agent` once, threads it through every hook dispatch
/// and transcript scan, and writes it onto the session's state file so the
/// dashboard can render mixed-backend rows.
pub async fn run(
    agent: AgentControl,
    cwd: &str,
    agent_args: &[String],
    pool_session: Option<String>,
    launch_id: Option<String>,
    shims: ClipboardShims,
) -> Result<()> {
    state::ensure_sessions_dir()?;

    let launcher_pid = std::process::id();
    // The dashboard (or server) owns the session↔window binding for any launch it
    // spawned — it threads a token (`--launch-id` locally, `--pool-session`
    // remotely) and records the window itself (next-step #6 §15). So self-report
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

    let mut launcher_state = LauncherState {
        agent,
        launcher_pid,
        session_id: None,
        child_session_ids: Vec::new(),
        window_id,
        tab_id: None,
        cwd: cwd.to_string(),
        status: SessionStatus::Starting,
        last_tool: None,
        updated_at: LauncherState::now(),
        active_since: None,
        last_prompt: None,
        child_pid: None,
        last_error: None,
        context_tokens: None,
        context_window: None,
        model: None,
        name: None,
        first_prompt: None,
        pool_session,
        launch_id,
        terminal,
        terminfo,
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

    let mut listener = bind_hook_socket(&sock_path)?;

    // Per-session hooks settings file. Path is generic; contents are
    // backend-specific JSON the agent will read on launch.
    let hooks_settings_json = agent.hooks_settings_json(&sock_path.to_string_lossy());
    let settings_path = sock_dir.join(format!("{launcher_pid}-settings.json"));
    std::fs::write(&settings_path, &hooks_settings_json)?;

    // The clipboard shims, for a pooled session only. Minted here rather than by
    // the dashboard because the farm has to exist on the machine the *agent* runs
    // on, and this is the one process that is already there. A failure to mint
    // costs the paste and nothing else — hence `.ok()` rather than `?`.
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

    let mut cmd = match agent.build_launch_command(
        cwd,
        &sock_path,
        &settings_path,
        agent_args,
        shim_dir.as_deref(),
    ) {
        Ok(c) => c,
        Err(e) => {
            // The agent never started (direnv blocked, binary missing). Hold the
            // window as a FailedToStart row carrying the error rather than
            // vanishing — see `hold_failed_launch`.
            let e = e.context(format!("Failed to build launch command for {agent:?}"));
            hold_failed_launch(
                launcher_pid,
                &sock_path,
                &settings_path,
                &mut launcher_state,
                e,
            )
            .await
        }
    };
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    tracing::info!("Launching {agent:?} in {cwd}");
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Same as the build-command failure: hold the window as a
            // FailedToStart row instead of leaving nothing behind.
            let e = anyhow::Error::from(e).context("Failed to launch agent");
            hold_failed_launch(
                launcher_pid,
                &sock_path,
                &settings_path,
                &mut launcher_state,
                e,
            )
            .await
        }
    };
    launcher_state.child_pid = child.id();
    // `tab_id` (display-only) is resolved by the dashboard from its own terminal
    // snapshot (`window_tab_map`), not here: window/tab lookup is a presentation
    // concern and a launcher may be headless/remote, so it stays terminal-free
    // beyond reading its own window id from the environment.
    launcher_state.updated_at = LauncherState::now();
    let _ = launcher_state.write();

    // From here on the state file, socket, and settings file must be torn down
    // on EVERY exit path, including a `child.wait()` error propagated by `?`.
    // `std::process::exit` doesn't run destructors, so the happy path below
    // calls `into_inner()` to perform the cleanup explicitly and disarm the
    // guard; any early `?` return instead drops the guard, which cleans up.
    let cleanup = CleanupGuard {
        launcher_pid,
        sock_path: sock_path.clone(),
        settings_path: settings_path.clone(),
    };

    let exit_status = tokio::select! {
        status = child.wait() => match status {
            Ok(s) => s,
            Err(e) => {
                // The wait failed — make a best effort to not leak the child
                // before propagating (the guard handles the file cleanup).
                let _ = child.start_kill();
                return Err(e).context("Failed to wait on agent");
            }
        },
        _ = process_hooks(&mut listener, &sock_path, &mut launcher_state) => {
            match child.wait().await {
                Ok(s) => s,
                Err(e) => {
                    let _ = child.start_kill();
                    return Err(e).context("Failed to wait on agent");
                }
            }
        }
        // The launcher process itself was asked to terminate (e.g. kitty closed
        // the window/tab hosting it, or a `kill`). The signal's default action
        // would kill us without unwinding, so the `CleanupGuard` Drop never runs
        // and we'd leak the state file, socket, and settings file. Catch it,
        // kill the now-orphaned agent, clean up explicitly, and exit. (SIGINT is
        // deliberately *not* caught — Ctrl-C belongs to the agent's own
        // interrupt handling, not to tearing down the session.)
        _ = wait_for_termination_signal() => {
            tracing::info!("Launcher received termination signal; cleaning up");
            let _ = child.start_kill();
            cleanup_launcher_files(launcher_pid, &sock_path, &settings_path);
            std::process::exit(143); // 128 + SIGTERM
        }
    };

    tracing::info!("Agent exited with {exit_status}");

    // Remove state file (dashboard will see the deletion via notify and reset
    // visuals), socket, and settings file. `into_inner` runs that cleanup once
    // and disarms the guard so the upcoming `process::exit` (which skips Drop)
    // doesn't matter.
    cleanup.into_inner();

    std::process::exit(exit_status.code().unwrap_or(1))
}

/// Resolve when the launcher is asked to terminate via SIGTERM or SIGHUP.
/// SIGINT is intentionally excluded — Ctrl-C in the shared terminal is the
/// agent's interrupt, not a request to tear the launcher down. Each signal
/// stream that can't be installed simply never fires (rather than busy-looping),
/// preserving the old default-action behaviour for that one signal.
async fn wait_for_termination_signal() {
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

/// Bind the launcher's hook socket, owner-only and non-blocking. Factored out
/// because [`restore_hook_socket`] has to repeat it exactly when the socket is
/// removed mid-session; the two must not drift on the permission bits.
fn bind_hook_socket(sock_path: &Path) -> Result<UnixListener> {
    let listener = std::os::unix::net::UnixListener::bind(sock_path)?;
    let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600));
    listener.set_nonblocking(true)?;
    Ok(UnixListener::from_std(listener)?)
}

/// `(device, inode)` of `path`, the identity the socket health check compares.
/// Mere existence isn't enough: a path that was unlinked and re-bound by someone
/// else exists again while *our* listener is orphaned on the old inode.
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// Whether the socket we bound is no longer reachable at its path — either
/// unlinked, or replaced by a different inode. `bound` is the identity recorded
/// at bind time; `None` there means the stat failed then, leaving nothing to
/// compare, so presence alone has to count as intact.
fn hook_socket_lost(sock_path: &Path, bound: Option<(u64, u64)>) -> bool {
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
fn restore_hook_socket(listener: &mut UnixListener, sock_path: &Path) -> Option<(u64, u64)> {
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

/// Remove `{pid}.sock` / `{pid}-settings.json` left by launchers that have
/// exited. `$TMPDIR` used to clear these for us; [`state::runtime_dir`]'s
/// fallback now persists across reboots, so they need reaping on the same terms
/// as the per-launcher logs — on every launcher startup, leaving live (and
/// just-crashed) launchers' files alone.
fn sweep_dead_launcher_runtime_files(sock_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(sock_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid) = name
            .strip_suffix(".sock")
            .or_else(|| name.strip_suffix("-settings.json"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if !state::is_process_alive(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Remove the per-launcher state file, socket, and hook-settings file. Called
/// on every `run` exit path (build/spawn failure, the hook loop's normal and
/// error exits, a termination signal) so a dead launcher never leaves a ghost
/// session for the dashboard or a stale socket for a later launcher to collide
/// with.
fn cleanup_launcher_files(launcher_pid: u32, sock_path: &Path, settings_path: &Path) {
    LauncherState::remove(launcher_pid);
    let _ = std::fs::remove_file(sock_path);
    let _ = std::fs::remove_file(settings_path);
}

/// Hold a launch that never produced an agent (direnv blocked on the session's
/// `.envrc`, a missing `claude`/`codex`, a spawn failure) as a visible
/// `FailedToStart` row instead of letting it vanish. We print the error to the
/// window (the agent never got to), stamp it onto the state file as
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
async fn hold_failed_launch(
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
    state.last_error = Some(msg);
    state.child_pid = None;
    state.active_since = None;
    state.updated_at = LauncherState::now();
    let _ = state.write();
    // Idle until the window is closed or the row is killed; then clean up the
    // state file, socket, and settings file so nothing is leaked.
    wait_for_termination_signal().await;
    cleanup_launcher_files(launcher_pid, sock_path, settings_path);
    std::process::exit(1);
}

/// Scope guard that runs `cleanup_launcher_files` on drop. Installed once the
/// agent child is live so an early `?` return from `run` (e.g. a `child.wait()`
/// error) still tears the files down. The happy path disarms it via
/// `into_inner`, which performs the cleanup and consumes the guard before the
/// `std::process::exit` that wouldn't otherwise run destructors.
struct CleanupGuard {
    launcher_pid: u32,
    sock_path: PathBuf,
    settings_path: PathBuf,
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

async fn process_hooks(listener: &mut UnixListener, sock_path: &Path, state: &mut LauncherState) {
    let agent = state.agent;
    // Watch the agent's transcript file instead of polling. For Claude,
    // a transcript modification while we're in WaitingForApproval is a
    // near-instant "approval granted" signal (no need to wait for the
    // approved tool's PostToolUse hook, which can be many seconds away).
    let (fs_tx, mut fs_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Session-status-file changes get their OWN channel, kept separate from the
    // transcript's `fs_rx`. `on_transcript_changed` treats any `fs_rx` wake past
    // the approval-grace window as "the permission dialog was dismissed → Active";
    // a session-file write (e.g. a background job finishing during a later turn's
    // approval prompt) is not that signal and must not reach it.
    let (sess_tx, mut sess_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut transcript_watcher: Option<TranscriptWatch> = None;
    let mut transcript_path: Option<PathBuf> = None;
    // Byte offset into the transcript for incremental scanning. Reset to 0
    // whenever `transcript_path` changes so we pick up existing signals in
    // a resumed session.
    let mut transcript_offset: u64 = 0;
    // Incremental fold of the transcript-derived fields (context tokens, model,
    // title, first prompt) the launcher stamps onto the state file so the
    // dashboard never reads a transcript. Carries Claude's byte cursor; reset to
    // None on a path change so a resumed session's new transcript is folded from
    // the top.
    let mut transcript_data: Option<TranscriptStats> = None;
    // Whether the last Stop was held off a rest state because the session
    // re-drives itself (`AgentControl::self_continues` — a Codex goal). The
    // hold is a bet that another turn is already starting, and this is the
    // outstanding-bet flag: the turn's own `task_started` settles it, and
    // `confirm_hold_at` is what settles it when that turn never comes.
    let mut held_for_goal = false;
    // How long a held Stop goes unconfirmed. Long enough that the continuation
    // it is waiting for (Codex opens the next turn ~10ms after the Stop hook)
    // has landed many times over, short enough that a goal which ended instead
    // doesn't leave a finished session reading Active while the user looks at
    // it.
    const CONFIRM_HELD_STOP_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
    // When to confirm that hold, once. Codex decides whether to continue
    // *after* it runs the stop hooks, so the answer we held on can already be
    // stale by the time it lands — and if it turned out to be "no", nothing
    // further is coming: no turn, no hook, no transcript byte. This deadline is
    // that one missing edge, not a cadence — it is armed by a hold, disarmed by
    // the turn that vindicates it, and never re-arms itself.
    let mut confirm_hold_at: Option<tokio::time::Instant> = None;
    // Also watch the agent's own session-status file (Claude:
    // `~/.claude/sessions/<pid>.json`). Its `status` reads `"shell"` exactly when
    // the turn has ended but a `run_in_background` shell is still running, and a
    // non-shell value otherwise — Claude maintains it, so we just mirror the field
    // into `BackgroundActive` (see the refinement below). Changes there fire no
    // hook, so we watch the file to wake on the shell↔idle transition. It signals
    // `sess_tx` (its own channel — see above) so a session-file write is never
    // mistaken for an approval-dismissing transcript write. The pid is fixed for
    // the session's lifetime, so this watcher is started once.
    let _session_watcher = state
        .child_pid
        .and_then(|pid| agent.session_watch_path(pid))
        .and_then(|p| start_file_watcher(&p, sess_tx.clone()).ok());
    // Fold the session name from the agent's own session file up front, so a
    // resumed/idle session that was previously `/rename`d shows that name before the
    // file next changes — the file may already carry the rename (Claude writes it at
    // startup / on resume) and, if the session then sits idle, no later `sess_rx`
    // wake would re-fold it. (`session_name` returns `None` for an un-renamed
    // session's auto slug, so this only stamps a real rename.) Ongoing renames are
    // caught by the `session_file_event` fold in the loop. One-shot small read,
    // synchronous like the rest of startup; persist it if it landed (the initial
    // `state.write()` in `run` ran before this).
    if let Some(cpid) = state.child_pid
        && apply_session_name(state, agent.session_name(cpid))
    {
        let _ = state.write();
    }
    // Grace period: when we enter WaitingForApproval, ignore transcript
    // changes for a short window. The assistant message containing the
    // tool_use is written to the transcript in the same turn, and notify
    // can deliver that event AFTER the PermissionRequest hook fires —
    // without the grace period on_transcript_changed would immediately
    // clear WaitingForApproval back to Active.
    let mut approval_entered_at: Option<Instant> = None;
    let approval_grace =
        std::time::Duration::from_secs(crate::config::get().launcher.approval_grace_secs);

    // Coalesce state-file writes: an observable change schedules a trailing flush
    // `WRITE_THROTTLE` out, so a burst of hook/transcript updates lands as one
    // write — and one dashboard fan-out — rather than one per event. Real-time-ness
    // isn't a requirement here; a sub-second settle is fine.
    const WRITE_THROTTLE: std::time::Duration = std::time::Duration::from_millis(500);
    let mut dirty = false;
    let mut flush_at: Option<tokio::time::Instant> = None;

    // A background command the seed heuristic didn't recognize is treated as a
    // busy transient task — but if it keeps running past this threshold it's
    // clearly a long-running service, so the launcher learns it (persisting to
    // the shared `learned` store for every future session) and re-classifies
    // this row to the at-rest `BackgroundServer`. `bg_first_seen` times each
    // still-unrecognized command since it was first observed running; `learn_at`
    // is the one-shot wake that lets a *parked* session cross the threshold and
    // flip without needing some unrelated event to wake the loop.
    const LEARN_LONG_RUNNING_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);
    let mut bg_first_seen: std::collections::HashMap<String, tokio::time::Instant> =
        std::collections::HashMap::new();
    let mut learn_at: Option<tokio::time::Instant> = None;

    // Losing the hook socket fires no event and produces no traffic — a session
    // whose hooks have stopped looks exactly like one that is simply quiet — so
    // the only way to notice is to look. One `stat` per tick, and the interval
    // bounds how long a session can run blind before it recovers itself.
    const SOCKET_HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    let mut bound_identity = file_identity(sock_path);
    // Start one interval out rather than immediately, so the loop's first pass is
    // always a real event and the `Starting` → `Idle` settle below isn't skipped
    // by a tick that `continue`s.
    let mut socket_health = tokio::time::interval_at(
        tokio::time::Instant::now() + SOCKET_HEALTH_INTERVAL,
        SOCKET_HEALTH_INTERVAL,
    );
    // After a laptop sleep, don't replay every tick that elapsed while suspended
    // — one check is as good as fifty.
    socket_health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let was_active = state.status.is_busy();
        // A transition *into* an actionable state (Approval/Decision) bypasses
        // the write throttle below, so a permission/decision prompt surfaces
        // without waiting out the debounce.
        let was_attention = state.status.needs_attention();
        // Track whether the iteration produced an observable state change.
        // The fs branch usually doesn't (transcript appends without interrupt
        // sentinels), so skipping the write avoids a fan-out of fs events to
        // the dashboard for a no-op.
        let mut state_changed = false;
        // Set when this iteration woke on a session-status-file change. The
        // activity reconciliation below runs ONLY then: a hook wake just set the
        // status authoritatively, and reconciling against the file in the same
        // iteration would let a lagging read (Claude writes the file slightly
        // after firing the hook) clobber the hook's decision — e.g. demote a
        // just-started `Active` turn back to `Idle`. The file's own write fires
        // `sess_rx`, so a real working↔idle↔shell change is never missed.
        let mut session_file_event = false;
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((mut stream, _)) = accepted else { continue };
                // Read the whole hook payload, but defend the single-threaded
                // loop against a hung (e.g. SIGSTOP'd) same-user client: bound
                // the wait with a timeout and the buffer with a size cap so one
                // bad connection can't freeze the launcher or balloon memory.
                const HOOK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
                const MAX_HOOK_BYTES: u64 = 1024 * 1024; // 1 MiB
                let mut buf = Vec::new();
                let read = tokio::time::timeout(
                    HOOK_READ_TIMEOUT,
                    (&mut stream).take(MAX_HOOK_BYTES).read_to_end(&mut buf),
                ).await;
                match read {
                    Ok(Ok(_)) => {}
                    // Read error or timeout: drop this connection and move on.
                    _ => continue,
                }
                let Ok(msg) = serde_json::from_slice::<HookMessage>(&buf) else {
                    continue;
                };
                tracing::debug!(
                    target: "captain_miao::hook",
                    "recv pid={} event={:?} session={:?} tool={:?} prompt_len={} bytes={}",
                    std::process::id(),
                    msg.event,
                    msg.session_id,
                    msg.tool_name,
                    msg.prompt.as_ref().map(|p| p.len()).unwrap_or(0),
                    buf.len(),
                );

                // (Re)adopt the transcript when the path changes. Claude mints a
                // new transcript on `/resume`, so the path can shift mid-session.
                // The watcher backend is the agent's call: an event-driven watch
                // (Claude; Linux) starts here, once; a poll-backed watch (Codex
                // on macOS, whose held-open rollout fd defeats FSEvents — see
                // `transcript_poll_interval`) is instead lifecycle-managed at the
                // bottom of the loop, running only while the session is off Idle,
                // so the path is adopted here without a watcher (any stale
                // old-path watcher is dropped).
                if let Some(ref p) = msg.transcript_path {
                    let path = PathBuf::from(p);
                    if transcript_path.as_ref() != Some(&path) {
                        let adopted = if agent.transcript_poll_interval().is_some() {
                            transcript_watcher = None;
                            true
                        } else {
                            match start_file_watcher(&path, fs_tx.clone()) {
                                Ok(w) => {
                                    transcript_watcher = Some(TranscriptWatch::Event(w));
                                    true
                                }
                                Err(e) => {
                                    tracing::debug!("transcript watch failed: {e}");
                                    false
                                }
                            }
                        };
                        if adopted {
                            transcript_path = Some(path.clone());
                            // Seed the offset to the new transcript's end so
                            // historical interrupt/compact signals from a
                            // resumed session aren't replayed as fresh.
                            let scan = agent.scan_transcript_signals(&path, 0);
                            transcript_offset = scan.new_offset;
                            // Fold the derived fields from the top so a
                            // resumed/idle session shows context, model,
                            // title, and first prompt before its next write.
                            // (Synchronous like the seed scan above — the same
                            // one-shot whole-file read at path set.)
                            let data = agent.read_transcript_stats(&path, None);
                            apply_transcript_data(state, &data);
                            transcript_data = Some(data);
                        }
                    }
                }

                // A poll-watched transcript (Codex on macOS) catches up here,
                // before the dispatch: Codex writes the rollout lines *before*
                // firing the matching hook (`token_count` lands ~20ms ahead of
                // `Stop`), so the bytes behind this state change are already on
                // disk and deserve to fold now, not a poll tick later. The
                // ordering is load-bearing — transcript first, hook second: the
                // scan may consume a stale `turn_aborted` from an Esc no tick
                // had read yet, which must settle the *old* turn before
                // `dispatch_hook` applies this hook's status (scanning after
                // the dispatch would clobber a fresh `UserPromptSubmit`'s
                // Active with Idle and stick the row there for the whole turn).
                // Event-watched agents (Claude; Codex on Linux) skip this:
                // their writes already woke the fs arm directly.
                if agent.transcript_poll_interval().is_some()
                    && let Some(path) = &transcript_path
                {
                    rescan_transcript(
                        agent,
                        path,
                        &mut transcript_offset,
                        &mut transcript_data,
                        &mut held_for_goal,
                        state,
                    )
                    .await;
                }

                let was_approval = state.status == SessionStatus::WaitingForApproval;
                let dispatched_event = msg.event;
                agent.dispatch_hook(state, msg).await;
                tracing::debug!(
                    target: "captain_miao::hook",
                    "dispatch pid={} event={:?} status={:?}",
                    std::process::id(),
                    dispatched_event,
                    state.status,
                );
                // A Stop that did *not* land on a rest state is a session
                // that re-drives itself (only `codex::dispatch_hook` does
                // this, and only under a goal). Note it, and arm the one
                // confirmation the bet needs — see `confirm_hold_at`.
                if dispatched_event == HookEvent::Stop {
                    held_for_goal = state.status.is_busy();
                    confirm_hold_at = held_for_goal
                        .then(|| tokio::time::Instant::now() + CONFIRM_HELD_STOP_AFTER);
                }
                // Track when we first enter WaitingForApproval so we can
                // ignore same-turn transcript noise.
                if state.status == SessionStatus::WaitingForApproval && !was_approval {
                    approval_entered_at = Some(Instant::now());
                } else if state.status != SessionStatus::WaitingForApproval {
                    approval_entered_at = None;
                }
                // Hook events always carry observable info (status, last_tool,
                // last_prompt, ...). Mark dirty unconditionally so the dashboard
                // sees the new state — pinning down which hooks are no-ops adds
                // fragility for negligible gain.
                state_changed = true;
            }
            Some(_) = fs_rx.recv() => {
                // Drain any coalesced writes so we only react once per burst.
                while fs_rx.try_recv().is_ok() {}
                let prev_status = state.status.clone();
                let prev_last_tool = state.last_tool.clone();
                on_transcript_changed(state, approval_entered_at, approval_grace);
                if state.status != SessionStatus::WaitingForApproval {
                    approval_entered_at = None;
                }
                if let Some(path) = &transcript_path
                    && rescan_transcript(
                        agent,
                        path,
                        &mut transcript_offset,
                        &mut transcript_data,
                        &mut held_for_goal,
                        state,
                    )
                    .await
                {
                    state_changed = true;
                }
                if state.status != prev_status
                    || state.last_tool != prev_last_tool
                {
                    state_changed = true;
                }
            }
            Some(_) = sess_rx.recv() => {
                // The session-status file changed. Unlike a transcript write this
                // is NOT an "approval dismissed" signal, so we deliberately skip
                // `on_transcript_changed` and the interrupt/compact scan — we just
                // drain the burst and let the activity reconciliation below re-read
                // the file and mirror the new status.
                while sess_rx.try_recv().is_ok() {}
                session_file_event = true;
            }
            // Debounced-write deadline. Wakes the loop with no event so the flush
            // block below can persist a pending change once the burst has settled.
            _ = sleep_until_opt(flush_at) => {}
            // Held-Stop confirmation deadline. The block below re-asks the
            // agent whether the turn it promised is still coming; nothing else
            // would ever wake a row holding Active on a promise that lapsed.
            _ = sleep_until_opt(confirm_hold_at) => {}
            // Learn-long-running deadline. A parked session with an unrecognized
            // background command otherwise never wakes; this fires when the
            // oldest such command crosses the threshold so the classification
            // block below can learn it and flip the row to `BackgroundServer`.
            _ = sleep_until_opt(learn_at) => {}
            // Socket health. Nothing else can detect a socket removed out from
            // under us, and a session that has lost its hooks is silent by
            // definition — so this arm is the only thing standing between a
            // reaped socket and a row that is stale for the rest of the
            // session's life.
            _ = socket_health.tick() => {
                if hook_socket_lost(sock_path, bound_identity) {
                    bound_identity = restore_hook_socket(listener, sock_path);
                }
                // Deliberately the whole iteration: this wake carries no state
                // signal, and falling through would run the reconciliation — and
                // on a background row its process-tree scan, a `ps` exec on
                // macOS — every 30s for the life of every parked session, purely
                // as a side effect of watching a socket.
                continue;
            }
        }

        if state.status == SessionStatus::Starting {
            state.status = SessionStatus::Idle;
            state_changed = true;
        }

        // Activity reconciliation against the agent's own status file, which is
        // authoritative on the coarse working/idle/background-shell axis. Runs
        // ONLY on a session-file wake (`session_file_event`) — never in the same
        // iteration as a hook, which just set the status and outranks a possibly
        // lagging file read (see the flag's declaration). Two jobs:
        //   - A turn can end with NO hook — an interrupt (Esc) fires no `Stop`, so
        //     `Active` would otherwise stick forever. When the file reports the
        //     agent is at rest, settle `Active` to the rest shape it reports.
        //   - A turn can also end while a `run_in_background` shell keeps running
        //     (`"shell"`), which reads as `BackgroundActive` rather than `Idle`.
        // The table itself is `reconcile_activity`; the gate below is only an
        // optimization, keeping the read (and, on a background row, the
        // process-tree scan under it) off wakes that could not change anything.
        //
        // Kept for the classification block, which needs the same read: `None`
        // whenever this didn't run, which is exactly when there is no fresh
        // evidence and nothing downstream should act.
        let mut activity = None;
        if session_file_event
            && let Some(cpid) = state.child_pid
            && matches!(
                state.status,
                SessionStatus::Active
                    | SessionStatus::Idle
                    | SessionStatus::Compacted
                    | SessionStatus::BackgroundActive
                    | SessionStatus::BackgroundServer
                    | SessionStatus::ReviewPending
            )
        {
            // Offload the blocking status-file read off the runtime thread. A
            // join error is treated like the read failing (`None`), so the
            // demote-only refinement holds the status unchanged.
            activity = tokio::task::spawn_blocking(move || agent.agent_activity(cpid))
                .await
                .unwrap_or(None);
            if let Some(next) = reconcile_activity(&state.status, activity) {
                // Settling out of `Active` here means the turn ended with no hook
                // (an interrupt). Clear `last_tool` to match the Stop hook and the
                // transcript-scan interrupt path, so a rest row doesn't carry the
                // tool that was running when it was interrupted.
                if state.status == SessionStatus::Active {
                    state.last_tool = None;
                }
                state.status = next;
                state_changed = true;
            }
        }

        // Fold the session name from the agent's own session file. Claude rewrites
        // that file when it auto-derives a name and when the user `/rename`s;
        // `session_name` surfaces only the *rename* (the auto slug is dropped so the
        // dashboard shows the first prompt instead), and mirroring it onto
        // `state.name` (which rides the state file to the dashboard, local *and*
        // remote) is the single source for a user-set live-session name. Ungated by
        // status — a rename can land at rest — and offloaded off the runtime thread
        // like the activity read above. Forward-only via `apply_session_name`.
        // (Codex's `session_name` is `None` — its sqlite title is overlaid per-host
        // by `LocalBackend`.)
        if session_file_event && let Some(cpid) = state.child_pid {
            let name = tokio::task::spawn_blocking(move || agent.session_name(cpid))
                .await
                .unwrap_or(None);
            if apply_session_name(state, name) {
                state_changed = true;
            }
        }

        // Refine a background-shell status by *what* is running: an r3
        // review-watch → `ReviewPending`, a long-running server/watcher →
        // at-rest `BackgroundServer`, anything else → busy `BackgroundActive`.
        // Classified from the **live process tree** (each background shell stays
        // a direct child of the agent), not the transcript — a task that ends
        // with no transcript marker (stopped from the UI, a Monitor timeout, a
        // `--resume` orphan) is simply gone from the tree, so the classification
        // can't go stale. Gated to the three at-rest/background states, so a busy
        // foreground turn never pays for the scan and no foreground tool shell
        // can be among the children; within those states it runs on every wake
        // (it's idempotent) so it converges whichever event woke us — e.g. the
        // session-file wake that produced `BackgroundActive` just above, or the
        // `learn_at` deadline that fires when an unrecognized command crosses the
        // long-running threshold.
        if matches!(
            state.status,
            SessionStatus::BackgroundActive
                | SessionStatus::BackgroundServer
                | SessionStatus::ReviewPending
        ) && let Some(cpid) = state.child_pid
        {
            // Offload the scan (a /proc walk, or one `ps` exec on macOS) off the
            // runtime thread like the session-file reads above.
            let shells = tokio::task::spawn_blocking(move || agent.bg_shells(cpid))
                .await
                .unwrap_or(None);
            if promote_stale_background(state, shells.as_deref(), activity) {
                // Left the background states entirely; the per-command timers
                // belong to shells that are gone.
                bg_first_seen.clear();
                learn_at = None;
                state_changed = true;
            } else {
                let (class, next_deadline) = classify_and_learn(
                    shells.as_deref(),
                    &mut bg_first_seen,
                    LEARN_LONG_RUNNING_AFTER,
                    tokio::time::Instant::now(),
                    learned::is_long_running,
                    learned::record_long_running,
                );
                learn_at = next_deadline;
                if refine_background_kind(state, class) {
                    state_changed = true;
                }
            }
        } else {
            // Not in a background-shell state: drop the per-command timers so a
            // later, unrelated background job starts its clock fresh.
            bg_first_seen.clear();
            learn_at = None;
        }

        // Settle a held Stop, exactly once. Either the agent still means to
        // re-drive the session — in which case the row is right where it should
        // be and the bookkeeping is spent, the next Stop arming its own
        // confirmation — or the objective ended with that turn and this is the
        // only chance to park the row.
        if held_for_goal && confirm_hold_at.is_some_and(|at| tokio::time::Instant::now() >= at) {
            confirm_hold_at = None;
            held_for_goal = false;
            let still_driving = state
                .session_id
                .as_deref()
                .is_some_and(|id| agent.self_continues(id));
            if !still_driving && state.status == SessionStatus::Active {
                state.status = SessionStatus::Idle;
                state.last_tool = None;
                state_changed = true;
            }
        }
        if !held_for_goal {
            confirm_hold_at = None;
        }

        let is_active = state.status.is_busy();
        if is_active && !was_active {
            state.active_since = Some(LauncherState::now());
            state_changed = true;
        } else if !is_active && state.active_since.is_some() {
            state.active_since = None;
            state_changed = true;
        }

        if state_changed {
            state.updated_at = LauncherState::now();
            dirty = true;
            if state.status.needs_attention() && !was_attention {
                // Switched into an actionable state — flush now (overriding any
                // pending trailing deadline) so the prompt isn't held back.
                flush_at = Some(tokio::time::Instant::now());
            } else if flush_at.is_none() {
                // Schedule a trailing flush; a later change within the window
                // rides the same deadline (coalesced).
                flush_at = Some(tokio::time::Instant::now() + WRITE_THROTTLE);
            }
        }
        // Flush once the debounce deadline has passed — whether the timer arm
        // woke us or a later event arrived after it elapsed.
        if dirty && flush_at.is_some_and(|d| tokio::time::Instant::now() >= d) {
            let _ = state.write();
            dirty = false;
            flush_at = None;
        }
        // Lifecycle of the poll-backed transcript watch (Codex on macOS — see
        // `AgentControl::transcript_poll_interval`): it runs only while the
        // session is off Idle. An idle session's rollout doesn't change without
        // a hook firing first (`UserPromptSubmit` wakes this loop and the next
        // pass lands here with the status already Active), so parking the
        // watcher at Idle makes an at-rest session cost nothing — no 2s stat
        // cadence while a session sits parked for hours. A session that
        // re-drives itself does not weaken that premise, because it never
        // reaches Idle to be parked at: its Stop is held Active by
        // `AgentControl::self_continues` precisely so no watch has to stay
        // awake waiting for the turn it starts next. Idle is deliberately the
        // *only* at-rest state: WaitingForApproval/WaitingForDecision need the
        // rollout wake (approval-granted fast path; an Esc there writes
        // `turn_aborted` with no hook), and a Compacted row can still take that
        // same hookless Esc. Dropping the watcher on the way to rest fires one
        // synthetic fs wake so the next iteration folds whatever the last tick
        // hadn't seen — Codex writes its final `token_count` ~20ms before
        // `Stop` fires, which is inside the tick window essentially always.
        // Creation fires the same wake, as the backstop for the baseline gap:
        // the poll only signals *changes* from its first stat, so a write that
        // lands between this iteration's reads and that baseline would
        // otherwise surface only on the next write. (The bulk of parked-era
        // bytes were already consumed by the hook arm's pre-dispatch rescan —
        // re-engaging off Idle always rides a hook — so this wake usually
        // reads nothing; it exists for that racing-write window and for any
        // future off-Idle path that doesn't come through a hook.)
        if let Some(interval) = agent.transcript_poll_interval() {
            let engaged = state.status != SessionStatus::Idle;
            if engaged && transcript_watcher.is_none() {
                if let Some(path) = &transcript_path {
                    transcript_watcher = Some(TranscriptWatch::Poll(start_stat_poll(
                        path.clone(),
                        fs_tx.clone(),
                        interval,
                    )));
                    let _ = fs_tx.send(());
                }
            } else if !engaged && transcript_watcher.is_some() {
                transcript_watcher = None;
                let _ = fs_tx.send(());
            }
        }
        // `transcript_watcher` is otherwise held purely for its side effect — a
        // live fs watch that stops when the value drops. The binding (not this
        // line) is what keeps the watch alive until `run` returns.
        let _ = &transcript_watcher;
    }
}

/// A live transcript watch. Dropping either variant stops it — the binding's
/// lifetime is the watch's lifetime, which is how the poll's engaged-only
/// lifecycle (the gate at the bottom of the loop) turns it on and off.
enum TranscriptWatch {
    /// The platform's event-driven watcher (FSEvents/inotify) — Claude
    /// everywhere, Codex on Linux.
    #[allow(dead_code)] // held for its Drop; never read
    Event(notify::RecommendedWatcher),
    /// The stat poll standing in where the platform events can't fire (Codex
    /// on macOS — see [`AgentControl::transcript_poll_interval`]).
    #[allow(dead_code)]
    Poll(StatPoll),
}

/// Handle to a [`start_stat_poll`] task; aborts the task on drop, mirroring
/// how dropping a notify watcher stops its watch.
struct StatPoll(tokio::task::JoinHandle<()>);

impl Drop for StatPoll {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Stat `path` every `interval` and signal `tx` whenever `(size, mtime)` moves
/// — the stand-in transcript watch for a writer that defeats the platform's
/// fs events (Codex holds its rollout fd open for the whole session, which
/// macOS FSEvents reports nothing for until close; `write(2)` updates the stat
/// metadata immediately). The first stat is a silent baseline: what's already
/// on disk is covered by the creation wake the gate fires, not by this task.
///
/// Hand-rolled rather than notify's `PollWatcher` deliberately: that watcher
/// compares mtime **truncated to whole seconds** and nothing else (size only
/// enters via the opt-in whole-file content hash), so the second of two
/// appends landing within one wall-clock second never fires — and a rollout
/// is exactly that write pattern (e.g. a `turn_aborted` written sub-second
/// after the previous line would be missed forever, sticking the row at
/// Active). Comparing the full-precision mtime *and* the size catches every
/// append to an append-only file. Pinned by `stat_poll_sees_held_fd_appends`.
fn start_stat_poll(
    path: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
    interval: std::time::Duration,
) -> StatPoll {
    StatPoll(tokio::spawn(async move {
        // `Some((size, mtime))` per stat; `None` = missing/unreadable. The
        // outer Option distinguishes "no baseline yet" from "file was absent".
        let mut prev: Option<Option<(u64, std::time::SystemTime)>> = None;
        loop {
            let cur = std::fs::metadata(&path)
                .ok()
                .map(|m| (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
            if let Some(p) = &prev
                && *p != cur
            {
                let _ = tx.send(());
            }
            prev = Some(cur);
            tokio::time::sleep(interval).await;
        }
    }))
}

/// Re-read the transcript from the incremental offset: the signal scan
/// (interrupt / aborted compact) **and** the derived-field fold (context
/// tokens, model, title, first prompt), in one blocking read offloaded off the
/// runtime thread that also drives `child.wait()`/`accept()`. Advances
/// `offset`, replaces `data`, applies both onto `state`, and returns whether
/// the fold changed anything (a signal-driven status change is visible to the
/// caller through `state`). A join error is treated like the read failing: the
/// offset and the prior fold are held.
///
/// Shared by the two wakes that can find unread transcript bytes: the fs-watch
/// arm, and — for a poll-watched transcript — the hook arm's pre-dispatch
/// catch-up (see the call site there for why it must run *before*
/// `dispatch_hook`).
async fn rescan_transcript(
    agent: AgentControl,
    path: &Path,
    offset: &mut u64,
    data: &mut Option<TranscriptStats>,
    held_for_goal: &mut bool,
    state: &mut LauncherState,
) -> bool {
    let scan_path = path.to_path_buf();
    let scan_offset = *offset;
    let prior = data.clone();
    let (scan, fresh) = tokio::task::spawn_blocking(move || {
        let scan = agent.scan_transcript_signals(&scan_path, scan_offset);
        let stats = agent.read_transcript_stats(&scan_path, prior.as_ref());
        (scan, stats)
    })
    .await
    .unwrap_or_else(|_| {
        (
            TranscriptScan {
                new_offset: scan_offset,
                ..Default::default()
            },
            data.clone().unwrap_or_default(),
        )
    });
    *offset = scan.new_offset;
    if scan.interrupted {
        // Some agents (Claude) fire no hook on Esc/interrupt — without this,
        // the session stays Active forever. (Codex writes `turn_aborted`.)
        state.status = SessionStatus::Idle;
        state.last_tool = None;
    }
    if scan.compact_aborted && state.status == SessionStatus::Compacting {
        // Claude fires no PostCompact when `/compact` errors, so the
        // transcript-side stderr is the only signal the user is back at
        // the prompt.
        state.status = SessionStatus::Idle;
        state.last_tool = None;
    }
    // …and last, because the same delta that closed a turn can open the next
    // one: the agent started a turn nothing will fire a hook for.
    if scan.turn_started {
        // The turn a held Stop was betting on has arrived — the bet is settled
        // and needs no confirming.
        *held_for_goal = false;
        // Only the two at-rest states promote: `Waiting*` is the user's own
        // turn to act and outranks a transcript read, and every busy state
        // already says what this would.
        if matches!(state.status, SessionStatus::Idle | SessionStatus::Compacted) {
            state.status = SessionStatus::Active;
            state.last_tool = None;
        }
    }
    let changed = apply_transcript_data(state, &fresh);
    *data = Some(fresh);
    changed
}

/// Watch a single file and signal `tx` on every non-Access change, via the
/// platform's event-driven watcher (FSEvents/inotify). Used for the transcript
/// (except where the poll stands in — see [`start_stat_poll`]) and the agent's
/// session-status file. Falls back to watching the parent directory (filtering
/// to `path`) when the file doesn't exist yet.
fn start_file_watcher(
    path: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> notify::Result<notify::RecommendedWatcher> {
    let target = path.to_path_buf();
    // The path we register and the path the backend reports back are not always
    // the same string: macOS FSEvents resolves symlinks (and `/var` → `/private/var`)
    // before reporting, while Linux inotify echoes the path as registered. An
    // agent can report a transcript through any symlinked config/session tree;
    // on macOS the event then arrives under the real path and a raw-string
    // filter silently freezes the transcript fold. Accept either spelling.
    let real = canonical_watch_target(path).filter(|p| *p != target);
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Drop Access events (open/close/read). On Linux notify uses inotify
        // with a mask that includes IN_OPEN, and `scan_transcript_signals`
        // opens the transcript on every wakeup — without this filter, our own
        // open fires our own watch and the loop spins at 100% CPU.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        // When we fall back to watching the parent directory (because the file
        // doesn't exist yet), events will fire for sibling transcripts too —
        // filter to just our file.
        if event
            .paths
            .iter()
            .any(|p| p == &target || Some(p) == real.as_ref())
        {
            let _ = tx.send(());
        }
    };
    let mut w = notify::recommended_watcher(handler)?;

    // Try watching the file directly; fall back to its parent directory if
    // the file doesn't exist yet (notify requires the path to exist on some
    // platforms).
    if w.watch(path, notify::RecursiveMode::NonRecursive).is_err()
        && let Some(parent) = path.parent()
    {
        w.watch(parent, notify::RecursiveMode::NonRecursive)?;
    }
    Ok(w)
}

/// The real path a watch event will carry for `path`, for comparison against the
/// path we registered. Resolves symlinks in the file itself when it exists, and
/// otherwise in its parent (the parent-directory fallback watches a file that
/// hasn't been created yet, but its directory is already real). `None` when
/// neither resolves — there is then nothing to match beyond the raw path.
fn canonical_watch_target(path: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(path) {
        return Some(p);
    }
    let parent = path.parent()?;
    let name = path.file_name()?;
    Some(std::fs::canonicalize(parent).ok()?.join(name))
}

fn on_transcript_changed(
    state: &mut LauncherState,
    approval_entered_at: Option<Instant>,
    grace: std::time::Duration,
) {
    // The only state transition driven by transcript changes today is leaving
    // WaitingForApproval — Claude has no "approval granted" hook, and any
    // transcript write while the permission dialog is up means the user just
    // dismissed it and the agent is back to executing the tool.
    //
    // However, the assistant message that *contains* the tool_use is also
    // written to the transcript around the same time the PermissionRequest
    // hook fires. FSEvents/notify can deliver that write event after we've
    // already set WaitingForApproval, so we ignore transcript changes within
    // a short grace window after entering the state.
    if state.status == SessionStatus::WaitingForApproval {
        let past_grace = approval_entered_at
            .map(|t| t.elapsed() >= grace)
            .unwrap_or(true);
        if past_grace {
            state.status = SessionStatus::Active;
        }
    }
}

/// Stamp a freshly-read session name onto `state.name`, returning whether it
/// changed. Forward-only: `None` — a transient unreadable/absent session file, or a
/// backend without one — leaves an already-shown name untouched, so a torn read
/// never blanks the row. The name is the agent's own (Claude writes both its
/// auto-derived title and the user's `/rename` to `~/.claude/sessions/<pid>.json`,
/// read via `AgentControl::session_name`); folding it here rides the state file to
/// the dashboard — local *and* remote — in place of the transcript `custom-title`
/// read the dashboard could never see across the ssh boundary.
fn apply_session_name(state: &mut LauncherState, name: Option<String>) -> bool {
    match name {
        Some(name) if state.name.as_deref() != Some(name.as_str()) => {
            state.name = Some(name);
            true
        }
        _ => false,
    }
}

/// Stamp the transcript-folded fields onto the state, returning whether any
/// changed (so the caller can mark the state dirty). `context_tokens`/`model`
/// track the fold (which may report None — matching the dashboard's old tail
/// read), but `first_prompt` is only ever set *forward*: a fold that transiently
/// lacks it must not clear a value we already have, since it's monotonic once found
/// (first-wins prompt). `name` is Some-only too, but last-write-wins: a `/rename`
/// is a real new value.
fn apply_transcript_data(state: &mut LauncherState, data: &TranscriptStats) -> bool {
    let mut changed = false;
    // `Some`-only, like `first_prompt` below and for the same reason, now that a
    // hook can carry these too (`common::adopt_session_facts`): a fold that
    // produced no token count has not learned the count is zero, it has learned
    // nothing, and overwriting with `None` would blank a column from a fact we
    // do not have. That is the house rule for an unreadable read — leave it
    // unchanged, never assert a definite value — and it also means a Codex tail
    // that has scrolled past its last `token_count` keeps the number instead of
    // dropping it.
    if data.context_tokens.is_some() && state.context_tokens != data.context_tokens {
        state.context_tokens = data.context_tokens;
        changed = true;
    }
    if data.context_window.is_some() && state.context_window != data.context_window {
        state.context_window = data.context_window;
        changed = true;
    }
    if data.model.is_some() && state.model != data.model {
        state.model = data.model.clone();
        changed = true;
    }
    if data.first_prompt.is_some() && state.first_prompt != data.first_prompt {
        state.first_prompt = data.first_prompt.clone();
        changed = true;
    }
    // Grok's `last_turn_summary`: the glance column on an idle/resumed row.
    // Skip while the turn is in flight — `PromptSubmit` just wrote the user's
    // question, and a sidecar rewrite from the *previous* turn must not
    // clobber it. Waiting/compacting count as in-flight too.
    if let Some(prompt) = data
        .last_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && !matches!(
            state.status,
            SessionStatus::Active
                | SessionStatus::Compacting
                | SessionStatus::WaitingForApproval
                | SessionStatus::WaitingForDecision
                | SessionStatus::BackgroundActive
        )
        && state.last_prompt.as_deref() != Some(prompt)
    {
        state.last_prompt = Some(prompt.to_string());
        changed = true;
    }
    // Last-write-wins and Some-only, like the token count: an empty fold has
    // not learned the session is untitled, and a `/rename` (or Grok's auto
    // refresh) is a real new value.
    if let Some(name) = data
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && state.name.as_deref() != Some(name)
    {
        state.name = Some(name.to_string());
        changed = true;
    }
    changed
}

/// The aggregate background-shell classification a session's running shells
/// reduce to, in precedence order — see [`classify_and_learn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgClass {
    /// At least one shell is a finite/short task (or an as-yet-unlearned
    /// command): finite work is in progress → busy `BackgroundActive`.
    Transient,
    /// No transient shell, and at least one long-running server/watcher (seed
    /// heuristic or learned) → at-rest `BackgroundServer`.
    LongRunning,
    /// Every running shell is an r3 review-watch → `ReviewPending`.
    ReviewWatch,
}

/// Fold a session's current background shells into an aggregate [`BgClass`],
/// learning as a side effect. Each shell is already statically classed by
/// [`BgSeedKind`]; here we overlay the persistent learned store and per-command
/// durations onto the `Other` shells:
///
/// - A `LongRunning` seed, or an `Other` whose command `is_learned`, counts as
///   long-running immediately.
/// - An unrecognized `Other` is a *candidate*: `first_seen` times it, and once
///   it has been running `threshold` or longer it is `learn`ed (persisted for
///   every future session) and promoted to long-running. Until then it keeps
///   the row busy.
///
/// Precedence: any transient shell → `Transient` (finite work dominates — keep
/// the machine awake); else any long-running shell → `LongRunning`; else (≥1
/// shell, all watches) → `ReviewWatch`. Returns the class (`None` = no shells /
/// unreadable tree → leave the status to the session-file idle transition, so
/// we don't flicker on the way out) and the next learn deadline to schedule
/// (the earliest candidate's `first_seen + threshold`).
///
/// `is_learned` / `learn` are injected (the launcher passes the `learned` store;
/// tests pass an in-memory set) so the pure timing/precedence logic is testable
/// without touching the filesystem.
fn classify_and_learn(
    shells: Option<&[BgShell]>,
    first_seen: &mut std::collections::HashMap<String, tokio::time::Instant>,
    threshold: std::time::Duration,
    now: tokio::time::Instant,
    is_learned: impl Fn(&str) -> bool,
    mut learn: impl FnMut(&str),
) -> (Option<BgClass>, Option<tokio::time::Instant>) {
    let Some(shells) = shells.filter(|s| !s.is_empty()) else {
        first_seen.clear();
        return (None, None);
    };

    // Forget timers for commands no longer running (prevents a re-run of the
    // same command from inheriting a stale, already-elapsed clock).
    first_seen.retain(|k, _| {
        shells
            .iter()
            .any(|s| s.kind == BgSeedKind::Other && s.key == *k)
    });

    let mut any_transient = false;
    let mut any_long = false;
    let mut next_deadline: Option<tokio::time::Instant> = None;
    for shell in shells {
        match shell.kind {
            BgSeedKind::ReviewWatch => {}
            BgSeedKind::LongRunning => any_long = true,
            BgSeedKind::Other => {
                if is_learned(&shell.key) {
                    any_long = true;
                    continue;
                }
                let started = *first_seen.entry(shell.key.clone()).or_insert(now);
                if now.duration_since(started) >= threshold {
                    learn(&shell.key);
                    any_long = true;
                } else {
                    any_transient = true;
                    let due = started + threshold;
                    next_deadline = Some(next_deadline.map_or(due, |cur| cur.min(due)));
                }
            }
        }
    }

    let class = if any_transient {
        BgClass::Transient
    } else if any_long {
        BgClass::LongRunning
    } else {
        BgClass::ReviewWatch
    };
    (Some(class), next_deadline)
}

/// The status a fresh session-file read settles `current` to, or `None` to hold
/// it. The agent's own file is authoritative on the coarse
/// working/idle/background-shell axis; this is the table that mirrors it (the
/// call site owns *when* it runs — only on a session-file wake).
///
/// **Demote-only**: hook events own the rest→`Active` direction, so a momentary
/// or lagging `"busy"` read can never bounce a resting row into `Active`. The
/// one promotion in the loop is `promote_stale_background`, which turns on
/// corroborating process-tree evidence rather than on this read alone. An
/// unknown/torn read (`None`) always holds.
///
/// Only the working/idle/shell axis is consulted; the fine-grained,
/// hook/transcript-backed states (Approval, Decision, `Compacting`) are left
/// untouched so the file can't clobber them. `Compacted` is the one that also
/// takes a `BackgroundShell`, for the same reason `Idle` does: it is a **rest**
/// status, so a `run_in_background` shell that outlives the compaction is the
/// truer shape of the row — otherwise a session compacted while an r3
/// review-watch runs reads `Compacted` until the watch ends, rather than the
/// `Review` its shells say it is. It is never demoted to `Idle`, though: both
/// are at rest, so that trades away the compaction signal (and the follow-up
/// bell armed on entering it) for nothing.
fn reconcile_activity(
    current: &SessionStatus,
    activity: Option<AgentActivity>,
) -> Option<SessionStatus> {
    match (current, activity) {
        // Interrupt: a busy turn ended with no hook.
        (SessionStatus::Active, Some(AgentActivity::Idle)) => Some(SessionStatus::Idle),
        (SessionStatus::Active, Some(AgentActivity::BackgroundShell)) => {
            Some(SessionStatus::BackgroundActive)
        }
        // At rest: track the background shell appearing / clearing.
        (SessionStatus::Idle | SessionStatus::Compacted, Some(AgentActivity::BackgroundShell)) => {
            Some(SessionStatus::BackgroundActive)
        }
        (SessionStatus::BackgroundActive, Some(AgentActivity::Idle)) => Some(SessionStatus::Idle),
        // A long-running server / review-watch is a background shell too — when
        // it ends (killed, human submitted, timed out) the shell is gone, so
        // settle to rest just like `BackgroundActive`. Staying-in-shell is left
        // to the classification refinement (which decides transient-task vs
        // server vs review).
        (
            SessionStatus::BackgroundServer | SessionStatus::ReviewPending,
            Some(AgentActivity::Idle),
        ) => Some(SessionStatus::Idle),
        // Working, unknown/torn read (None), or already-consistent: hold.
        _ => None,
    }
}

/// Retire a background-shell status the process tree has disproved, promoting
/// the row to `Active`. Returns whether the status changed.
///
/// Each of the three background states asserts *"a `run_in_background` shell is
/// running"*. The live process tree is the authority on that — the same
/// authority that put the row into the state to begin with — so when it reads
/// cleanly and holds no background shell, the assertion is false and the row is
/// stale by its own definition. It must be `Active` or `Idle`, and the session
/// file says which.
///
/// This is the sole exception to the demote-only rule above it, and it does not
/// weaken it: that rule guards against a *momentary or lagging* `"busy"` read
/// bouncing a resting row into `Active`, whereas this needs two independent
/// signals to agree. Both halves are load-bearing:
///
/// - `Some(&[])` is "tree read fine, nothing running"; `None` is "couldn't read
///   the tree" and must never promote — acting on an unreadable tree is exactly
///   the spurious bounce the demote-only rule exists to prevent.
/// - `activity` is `None` on any wake that didn't re-read the session file, so a
///   stale value can never be mistaken for fresh evidence.
///
/// Without this, a session whose background shell ends *while the agent goes
/// back to work* has no path back to `Active`: the demote-only table only exits
/// these states toward `Idle`, and `refine_background_kind` declines to act on
/// an empty tree. Hooks normally cover the gap in milliseconds, so the hole is
/// invisible until hooks are missing — a lost socket, a hook binary that can't
/// run — at which point the row reads `Review`/`Task`/`Server` for every working
/// turn until the agent next comes to rest.
fn promote_stale_background(
    state: &mut LauncherState,
    shells: Option<&[BgShell]>,
    activity: Option<AgentActivity>,
) -> bool {
    if !matches!(
        state.status,
        SessionStatus::BackgroundActive
            | SessionStatus::BackgroundServer
            | SessionStatus::ReviewPending
    ) {
        return false;
    }
    if activity != Some(AgentActivity::Working) {
        return false;
    }
    if !shells.is_some_and(<[BgShell]>::is_empty) {
        return false;
    }
    state.status = SessionStatus::Active;
    true
}

/// Apply an aggregate [`BgClass`] to a background-shell status. The session file
/// still owns whether a background shell is running *at all* (it produces
/// `BackgroundActive` first); this only classifies it into the right sub-state.
/// A `None` class leaves the status untouched (the way out is the session-file
/// idle transition). Returns whether the status changed.
fn refine_background_kind(state: &mut LauncherState, class: Option<BgClass>) -> bool {
    if !matches!(
        state.status,
        SessionStatus::BackgroundActive
            | SessionStatus::BackgroundServer
            | SessionStatus::ReviewPending
    ) {
        return false;
    }
    let Some(class) = class else { return false };
    let next = match class {
        BgClass::Transient => SessionStatus::BackgroundActive,
        BgClass::LongRunning => SessionStatus::BackgroundServer,
        BgClass::ReviewWatch => SessionStatus::ReviewPending,
    };
    if state.status == next {
        return false;
    }
    state.status = next;
    true
}

/// Sleep until `deadline`, or await forever when there's no pending flush — so
/// the select! treats "no scheduled write" as an inert arm.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;

    /// A transcript reached through a symlinked directory must resolve to the
    /// real path, because that is the spelling macOS FSEvents reports and the
    /// watch filter compares against. Covers both the file-exists case and the
    /// parent-directory fallback (file not created yet).
    #[test]
    fn canonical_watch_target_resolves_symlinked_dirs() {
        let base = std::env::temp_dir().join(format!("cm-watch-target-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let existing = link.join("rollout.jsonl");
        std::fs::write(&existing, b"{}\n").unwrap();
        let want = std::fs::canonicalize(&real).unwrap();
        assert_eq!(
            canonical_watch_target(&existing),
            Some(want.join("rollout.jsonl"))
        );

        // Not yet created: resolved via the parent, which does exist.
        let pending = link.join("not-yet.jsonl");
        assert_eq!(
            canonical_watch_target(&pending),
            Some(want.join("not-yet.jsonl"))
        );

        // Neither the file nor its parent exists: nothing to resolve.
        assert_eq!(
            canonical_watch_target(&base.join("gone").join("x.jsonl")),
            None
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The stat-polling transcript watch (the Codex-on-macOS case — see
    /// `AgentControl::transcript_poll_interval`) must report appends made
    /// through a fd the writer holds open — which the event-driven FSEvents
    /// watch cannot do — **including a second append landing within the same
    /// wall-clock second as the first**. The same-second case is what
    /// disqualified notify's `PollWatcher` (it compares mtime truncated to
    /// whole seconds and nothing else, so it misses it — a `turn_aborted`
    /// written sub-second after the previous rollout line would stick the row
    /// at Active forever) and is why `start_stat_poll` compares the size too.
    #[test]
    fn stat_poll_sees_held_fd_appends() {
        use std::io::Write;

        let base = std::env::temp_dir().join(format!("cm-stat-poll-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("rollout.jsonl");
        let mut writer = std::fs::File::create(&path).unwrap();
        writer.write_all(b"{}\n").unwrap();
        writer.flush().unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let _w = start_stat_poll(path.clone(), tx, std::time::Duration::from_millis(50));
            // Let the first stat (the silent baseline) land.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            // Append through the still-open fd; never close it.
            writer.write_all(b"{\"more\":1}\n").unwrap();
            writer.flush().unwrap();
            let woke = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
            assert!(woke.is_ok(), "stat poll never fired for a held-fd append");
            // Drain the burst, then a follow-up append — typically within the
            // same wall-clock second — must fire again.
            while rx.try_recv().is_ok() {}
            writer.write_all(b"{\"more\":2}\n").unwrap();
            writer.flush().unwrap();
            let woke = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
            assert!(
                woke.is_ok(),
                "stat poll missed a same-second follow-up append"
            );
        });

        drop(writer);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `rescan_transcript` must consume a transcript signal exactly once — the
    /// property the hook arm's transcript-before-dispatch ordering relies on:
    /// a `turn_aborted` from an Esc settles the row to Idle on the read that
    /// first sees it (folding the stats alongside), and a later rescan of the
    /// same file must NOT re-settle a row that has since started a new turn
    /// (the offset advanced past the signal). If a refactor made the scan
    /// re-read consumed signals, an Esc followed by a quick resubmit would
    /// flip the new turn's Active back to Idle and stick there.
    #[test]
    fn rescan_consumes_a_transcript_signal_exactly_once() {
        let base = std::env::temp_dir().join(format!("cm-rescan-once-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("rollout.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"p\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",",
                "\"info\":{\"last_token_usage\":{\"total_tokens\":4242}}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_aborted\"}}\n",
            ),
        )
        .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut state = state_with(SessionStatus::Active);
            let mut offset = 0u64;
            let mut data: Option<TranscriptStats> = None;
            let mut held_for_goal = false;

            rescan_transcript(
                AgentControl::Codex,
                &path,
                &mut offset,
                &mut data,
                &mut held_for_goal,
                &mut state,
            )
            .await;
            assert_eq!(state.status, SessionStatus::Idle, "abort settles the turn");
            assert_eq!(state.context_tokens, Some(4242), "stats fold alongside");
            assert_eq!(
                offset,
                std::fs::metadata(&path).unwrap().len(),
                "offset advances past the consumed signal"
            );

            // A new turn started (hook set Active); the same file re-read must
            // not replay the stale abort.
            state.status = SessionStatus::Active;
            rescan_transcript(
                AgentControl::Codex,
                &path,
                &mut offset,
                &mut data,
                &mut held_for_goal,
                &mut state,
            )
            .await;
            assert_eq!(
                state.status,
                SessionStatus::Active,
                "a consumed signal never re-settles a later turn"
            );
        });

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The hookless turn start, end to end through the launcher: a turn opened
    /// that no hook will announce, so the row has to leave a rest state on the
    /// transcript alone. A row waiting on the *user* is the one thing that
    /// outranks the read.
    #[test]
    fn rescan_promotes_a_hookless_turn_but_never_over_a_waiting_row() {
        let base = std::env::temp_dir().join(format!("cm-rescan-open-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("rollout.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            ),
        )
        .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            for (before, after) in [
                (SessionStatus::Idle, SessionStatus::Active),
                (SessionStatus::Compacted, SessionStatus::Active),
                (
                    SessionStatus::WaitingForApproval,
                    SessionStatus::WaitingForApproval,
                ),
            ] {
                let mut state = state_with(before.clone());
                let mut offset = 0u64;
                let mut data: Option<TranscriptStats> = None;
                let mut held_for_goal = false;
                rescan_transcript(
                    AgentControl::Codex,
                    &path,
                    &mut offset,
                    &mut data,
                    &mut held_for_goal,
                    &mut state,
                )
                .await;
                assert_eq!(state.status, after, "{before:?} settled wrong");
            }
        });

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The turn a held Stop was waiting for arrives: the bet is settled, so the
    /// hold is spent and nothing needs confirming.
    #[test]
    fn a_hookless_turn_settles_the_hold_that_predicted_it() {
        let base = std::env::temp_dir().join(format!("cm-rescan-held-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("rollout.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
        )
        .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut state = state_with(SessionStatus::Active);
            let mut offset = 0u64;
            let mut data: Option<TranscriptStats> = None;
            let mut held_for_goal = true;
            rescan_transcript(
                AgentControl::Codex,
                &path,
                &mut offset,
                &mut data,
                &mut held_for_goal,
                &mut state,
            )
            .await;
            assert!(!held_for_goal, "the promised turn started");
            assert_eq!(state.status, SessionStatus::Active);
        });

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A session with no goal anywhere is not one that re-drives itself, so its
    /// Stop is rest — the hold only ever exists for a backend whose store says
    /// otherwise.
    #[test]
    fn a_session_without_a_goal_never_self_continues() {
        assert!(!AgentControl::Codex.self_continues("no-goal-was-ever-set-for-this-id"));
        assert!(!AgentControl::Claude.self_continues("no-goal-was-ever-set-for-this-id"));
    }

    fn state_with(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::Claude, status)
    }

    fn shell(key: &str, kind: BgSeedKind) -> BgShell {
        BgShell {
            key: key.to_string(),
            kind,
        }
    }

    /// The demote-only mirror of the agent's session file, plus the one rest
    /// status that also takes a background shell.
    #[test]
    fn reconcile_activity_mirrors_the_session_file() {
        use AgentActivity as A;
        use SessionStatus as S;

        // A turn that ended with no hook (an interrupt) settles to the shape the
        // file reports.
        assert_eq!(reconcile_activity(&S::Active, Some(A::Idle)), Some(S::Idle));
        assert_eq!(
            reconcile_activity(&S::Active, Some(A::BackgroundShell)),
            Some(S::BackgroundActive)
        );
        // A background shell appearing on a resting row, and clearing again from
        // each of the three background states.
        assert_eq!(
            reconcile_activity(&S::Idle, Some(A::BackgroundShell)),
            Some(S::BackgroundActive)
        );
        for st in [S::BackgroundActive, S::BackgroundServer, S::ReviewPending] {
            assert_eq!(reconcile_activity(&st, Some(A::Idle)), Some(S::Idle));
        }

        // Demote-only: a "busy" read never promotes a resting row to Active
        // (that's `promote_stale_background`'s job, on corroborated evidence),
        // and an unknown/torn read holds everything.
        for st in [S::Idle, S::Compacted, S::BackgroundActive, S::ReviewPending] {
            assert_eq!(reconcile_activity(&st, Some(A::Working)), None);
            assert_eq!(reconcile_activity(&st, None), None);
        }
        // The fine-grained hook/transcript-backed states are none of the file's
        // business, whatever it reports.
        for st in [S::Compacting, S::WaitingForApproval, S::WaitingForDecision] {
            for act in [Some(A::Idle), Some(A::Working), Some(A::BackgroundShell)] {
                assert_eq!(reconcile_activity(&st, act), None);
            }
        }
    }

    /// `Compacted` is a **rest** status, so a `run_in_background` shell that
    /// outlived the compaction has to show through it — otherwise a session
    /// compacted while an r3 review-watch runs reads `Compacted` for as long as
    /// the watch does, instead of the `Review` its shells say it is. It is only
    /// ever moved by a shell, never demoted to `Idle`: both are at rest, so that
    /// would drop the compaction signal for nothing.
    #[test]
    fn a_background_shell_shows_through_compacted() {
        use AgentActivity as A;
        use SessionStatus as S;
        assert_eq!(
            reconcile_activity(&S::Compacted, Some(A::BackgroundShell)),
            Some(S::BackgroundActive)
        );
        assert_eq!(reconcile_activity(&S::Compacted, Some(A::Idle)), None);
    }

    #[test]
    fn refine_background_kind_maps_each_class() {
        let mut s = state_with(SessionStatus::BackgroundActive);
        assert!(refine_background_kind(&mut s, Some(BgClass::ReviewWatch)));
        assert_eq!(s.status, SessionStatus::ReviewPending);
        assert!(refine_background_kind(&mut s, Some(BgClass::LongRunning)));
        assert_eq!(s.status, SessionStatus::BackgroundServer);
        assert!(refine_background_kind(&mut s, Some(BgClass::Transient)));
        assert_eq!(s.status, SessionStatus::BackgroundActive);
        // Same class again → no change.
        assert!(!refine_background_kind(&mut s, Some(BgClass::Transient)));
        // A `None` class leaves it be — the clean exit is the session-file idle
        // transition, not a bounce through a background state.
        assert!(!refine_background_kind(&mut s, None));
        assert_eq!(s.status, SessionStatus::BackgroundActive);
    }

    #[test]
    fn refine_ignores_non_background_statuses() {
        for st in [
            SessionStatus::Active,
            SessionStatus::Idle,
            SessionStatus::WaitingForApproval,
        ] {
            let mut s = state_with(st.clone());
            assert!(!refine_background_kind(&mut s, Some(BgClass::ReviewWatch)));
            assert_eq!(s.status, st);
        }
    }

    /// The recovery path: a background shell ended while the agent went back to
    /// work, so the row's background status is disproved by the tree and the
    /// session file says which rest-or-work shape replaces it. Every background
    /// state has to escape, since the bug reproduced from `ReviewPending` but
    /// the hole is in all three.
    #[test]
    fn a_disproved_background_status_promotes_to_active() {
        for st in [
            SessionStatus::BackgroundActive,
            SessionStatus::BackgroundServer,
            SessionStatus::ReviewPending,
        ] {
            let mut s = state_with(st.clone());
            assert!(
                promote_stale_background(&mut s, Some(&[]), Some(AgentActivity::Working)),
                "{st:?} survived a clean tree read with no background shells"
            );
            assert_eq!(s.status, SessionStatus::Active);
        }
    }

    /// The half that protects the demote-only rule. `bg_shells` returns `None`
    /// when the process tree could not be read at all — promoting on that would
    /// be acting on no evidence, which is precisely the spurious bounce into
    /// `Active` the surrounding reconciliation is written to prevent. This is
    /// the assertion that is easy to leave out and expensive to lose.
    #[test]
    fn an_unreadable_process_tree_never_promotes() {
        let mut s = state_with(SessionStatus::ReviewPending);
        assert!(!promote_stale_background(
            &mut s,
            None,
            Some(AgentActivity::Working)
        ));
        assert_eq!(s.status, SessionStatus::ReviewPending);
    }

    #[test]
    fn promotion_needs_a_working_agent_and_a_background_row() {
        // A shell is still running → the status is not stale; classification,
        // not promotion, owns this row.
        let mut s = state_with(SessionStatus::ReviewPending);
        let running = [shell("r3 watch review_abc", BgSeedKind::ReviewWatch)];
        assert!(!promote_stale_background(
            &mut s,
            Some(&running),
            Some(AgentActivity::Working)
        ));
        assert_eq!(s.status, SessionStatus::ReviewPending);

        // Tree is empty, but the agent is at rest or the session file wasn't
        // re-read this wake (`None`) — the demote-only path owns the exit to
        // `Idle`; promotion must not invent work.
        for activity in [None, Some(AgentActivity::Idle)] {
            let mut s = state_with(SessionStatus::ReviewPending);
            assert!(!promote_stale_background(&mut s, Some(&[]), activity));
            assert_eq!(s.status, SessionStatus::ReviewPending);
        }

        // Not a background row at all: `Active`/`Idle` and the fine-grained
        // hook-backed states are none of this function's business.
        for st in [
            SessionStatus::Idle,
            SessionStatus::WaitingForApproval,
            SessionStatus::Compacting,
        ] {
            let mut s = state_with(st.clone());
            assert!(!promote_stale_background(
                &mut s,
                Some(&[]),
                Some(AgentActivity::Working)
            ));
            assert_eq!(s.status, st);
        }
    }

    /// An empty-but-readable tree must keep behaving like "nothing to classify"
    /// everywhere *else*, or teaching `bg_shells` the distinction would change
    /// the meaning of a parked row. `None` class → `refine_background_kind`
    /// leaves the status alone.
    #[test]
    fn an_empty_tree_still_classifies_as_nothing() {
        let mut first_seen = std::collections::HashMap::new();
        let (class, deadline) = classify_and_learn(
            Some(&[]),
            &mut first_seen,
            std::time::Duration::from_secs(3600),
            tokio::time::Instant::now(),
            |_| false,
            |_| panic!("nothing running must never be learned"),
        );
        assert_eq!(class, None);
        assert_eq!(deadline, None);
    }

    /// The socket health check keys on inode identity, not mere existence: a
    /// path unlinked and re-bound by someone else is present again while our
    /// listener is stranded on the old inode.
    #[test]
    fn hook_socket_loss_is_detected_by_identity_not_existence() {
        let dir = std::env::temp_dir().join(format!("cm-sock-health-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.sock");
        let successor = dir.join("successor.sock");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&successor);
        std::fs::write(&path, b"").unwrap();
        // Mint the replacement now, while the original still holds its inode, so
        // the filesystem cannot issue the same number twice. Creating it after
        // the unlink instead leaves the case at the mercy of the allocator: ext4
        // hands the just-freed inode straight back to the next create in that
        // directory, so the replacement read as intact and this test failed on
        // CI while passing on a btrfs checkout, where numbers only ever climb.
        std::fs::write(&successor, b"").unwrap();

        let bound = file_identity(&path);
        assert!(bound.is_some());
        assert!(
            !hook_socket_lost(&path, bound),
            "untouched file reads intact"
        );

        // Unlinked — the observed failure.
        std::fs::remove_file(&path).unwrap();
        assert!(hook_socket_lost(&path, bound));

        // Replaced: present again, different inode, our listener orphaned.
        // `rename` carries the successor's own inode across, so this stays a
        // real replacement however the allocator recycles numbers.
        std::fs::rename(&successor, &path).unwrap();
        assert!(std::fs::metadata(&path).is_ok());
        assert_ne!(
            file_identity(&path),
            bound,
            "fixture must replace the inode"
        );
        assert!(hook_socket_lost(&path, bound));

        // No identity recorded at bind time leaves nothing to compare, so
        // presence alone has to count as intact — otherwise we would re-bind on
        // every single tick.
        assert!(!hook_socket_lost(&path, None));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recovery itself, end to end: reap a live launcher's socket the way
    /// macOS's `$TMPDIR` sweep did, then confirm the re-bound listener actually
    /// serves a client connecting to the *same path* — which is all the agent
    /// ever knows, since it read that path from `--settings` at startup and
    /// never re-reads it. A re-bind that didn't accept again would look fine in
    /// the log and still leave the session mute.
    #[tokio::test]
    async fn a_reaped_socket_is_rebound_and_serves_again() {
        let dir = std::env::temp_dir().join(format!("cm-sock-rebind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rebind.sock");
        let _ = std::fs::remove_file(&path);

        let mut listener = bind_hook_socket(&path).unwrap();
        let bound = file_identity(&path);
        assert!(!hook_socket_lost(&path, bound));

        // The failure: the socket is unlinked while the listener stays bound to
        // the now-unreachable inode.
        std::fs::remove_file(&path).unwrap();
        assert!(hook_socket_lost(&path, bound));

        let restored = restore_hook_socket(&mut listener, &path);
        assert!(restored.is_some(), "re-bind failed");
        assert!(!hook_socket_lost(&path, restored));

        let client = tokio::net::UnixStream::connect(&path)
            .await
            .expect("a hook must be able to reach the re-bound socket");
        drop(client);
        let accepted = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            <UnixListener>::accept(&listener),
        )
        .await
        .expect("re-bound listener never accepted");
        assert!(accepted.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn classify_precedence_transient_over_server_over_watch() {
        let threshold = std::time::Duration::from_secs(3600);
        let now = tokio::time::Instant::now();
        let never = |_: &str| false;
        let noop = |_: &str| {};
        let mut seen = std::collections::HashMap::new();

        // Every shell a watch → ReviewWatch (no learn deadline).
        let shells = [shell("r3 watch review_1", BgSeedKind::ReviewWatch)];
        let (class, due) =
            classify_and_learn(Some(&shells), &mut seen, threshold, now, never, noop);
        assert_eq!(class, Some(BgClass::ReviewWatch));
        assert!(due.is_none());

        // A server alongside a watch → LongRunning (no transient present).
        let shells = [
            shell("r3 watch review_1", BgSeedKind::ReviewWatch),
            shell("npm run dev", BgSeedKind::LongRunning),
        ];
        let (class, _) = classify_and_learn(Some(&shells), &mut seen, threshold, now, never, noop);
        assert_eq!(class, Some(BgClass::LongRunning));

        // A fresh unrecognized command alongside a server → Transient (finite
        // work dominates; keep the row busy) plus a learn deadline for it.
        seen.clear();
        let shells = [
            shell("npm run dev", BgSeedKind::LongRunning),
            shell("cargo build", BgSeedKind::Other),
        ];
        let (class, due) =
            classify_and_learn(Some(&shells), &mut seen, threshold, now, never, noop);
        assert_eq!(class, Some(BgClass::Transient));
        assert_eq!(due, Some(now + threshold));

        // No shells / unreadable tree → unrefined, and the timers reset.
        let (class, due) = classify_and_learn(None, &mut seen, threshold, now, never, noop);
        assert_eq!(class, None);
        assert!(due.is_none());
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn classify_learns_a_command_that_outlives_the_threshold() {
        let threshold = std::time::Duration::from_secs(3600);
        let start = tokio::time::Instant::now();
        let store = std::cell::RefCell::new(std::collections::HashSet::<String>::new());
        let is_learned = |k: &str| store.borrow().contains(k);
        let learn = |k: &str| {
            store.borrow_mut().insert(k.to_string());
        };
        let shells = [shell("my-custom-server --port 9", BgSeedKind::Other)];
        let mut seen = std::collections::HashMap::new();

        // First sighting: unrecognized → busy Transient, deadline scheduled.
        let (class, due) = classify_and_learn(
            Some(&shells),
            &mut seen,
            threshold,
            start,
            is_learned,
            learn,
        );
        assert_eq!(class, Some(BgClass::Transient));
        assert_eq!(due, Some(start + threshold));
        assert!(store.borrow().is_empty(), "not learned yet");

        // Halfway: still busy, not yet learned.
        let mid = start + std::time::Duration::from_secs(1800);
        let (class, _) =
            classify_and_learn(Some(&shells), &mut seen, threshold, mid, is_learned, learn);
        assert_eq!(class, Some(BgClass::Transient));
        assert!(store.borrow().is_empty());

        // Past the threshold: learn it and flip to at-rest LongRunning; no more
        // candidate, so no further deadline.
        let (class, due) = classify_and_learn(
            Some(&shells),
            &mut seen,
            threshold,
            start + threshold,
            is_learned,
            learn,
        );
        assert_eq!(class, Some(BgClass::LongRunning));
        assert!(due.is_none());
        assert!(store.borrow().contains("my-custom-server --port 9"));

        // A brand-new session (fresh timers) now recognizes it immediately from
        // the learned store — no waiting.
        let mut fresh = std::collections::HashMap::new();
        let (class, due) = classify_and_learn(
            Some(&shells),
            &mut fresh,
            threshold,
            tokio::time::Instant::now(),
            is_learned,
            learn,
        );
        assert_eq!(class, Some(BgClass::LongRunning));
        assert!(due.is_none());
    }
    /// A fold that produced no token count has learned nothing, not that the
    /// count is zero — so it must leave the row's number alone. This became
    /// load-bearing once a hook could carry the value too
    /// (`common::adopt_session_facts`): without it, the next transcript fold of
    /// a backend that reports tokens on the payload would blank them again.
    /// It also fixes a pre-existing case — a Codex tail that has scrolled past
    /// its last `token_count` used to drop the number.
    #[test]
    fn a_silent_transcript_fold_never_clears_tokens_or_model() {
        let mut state = state_with(SessionStatus::Idle);
        state.context_tokens = Some(64_000);
        state.model = Some("some-model-1".to_string());

        let changed = apply_transcript_data(&mut state, &TranscriptStats::default());
        assert!(!changed, "a fold with nothing in it changes nothing");
        assert_eq!(state.context_tokens, Some(64_000));
        assert_eq!(state.model.as_deref(), Some("some-model-1"));

        // A fold that *does* have an opinion still wins, including a lower
        // count — that is what a compaction looks like.
        let folded = TranscriptStats {
            context_tokens: Some(12_000),
            model: Some("some-model-2".to_string()),
            ..TranscriptStats::default()
        };
        assert!(apply_transcript_data(&mut state, &folded));
        assert_eq!(state.context_tokens, Some(12_000));
        assert_eq!(state.model.as_deref(), Some("some-model-2"));

        let windowed = TranscriptStats {
            context_window: Some(500_000),
            ..TranscriptStats::default()
        };
        assert!(apply_transcript_data(&mut state, &windowed));
        assert_eq!(state.context_window, Some(500_000));
        assert!(!apply_transcript_data(
            &mut state,
            &TranscriptStats::default()
        ));
        assert_eq!(state.context_window, Some(500_000));

        let renamed = TranscriptStats {
            name: Some("miao hooks".to_string()),
            ..TranscriptStats::default()
        };
        assert!(apply_transcript_data(&mut state, &renamed));
        assert_eq!(state.name.as_deref(), Some("miao hooks"));
        assert!(!apply_transcript_data(
            &mut state,
            &TranscriptStats::default()
        ));
        assert_eq!(state.name.as_deref(), Some("miao hooks"));
    }

    /// Grok's `last_turn_summary` lands on `last_prompt` at rest (resume, idle)
    /// and is left alone while the turn is in flight — `PromptSubmit` just
    /// wrote the user's question.
    #[test]
    fn a_last_prompt_fold_writes_at_rest_and_spares_an_active_turn() {
        let recap = TranscriptStats {
            last_prompt: Some("Pinned GrokNight".to_string()),
            ..TranscriptStats::default()
        };

        let mut idle = state_with(SessionStatus::Idle);
        assert!(apply_transcript_data(&mut idle, &recap));
        assert_eq!(idle.last_prompt.as_deref(), Some("Pinned GrokNight"));

        let mut active = state_with(SessionStatus::Active);
        active.last_prompt = Some("make it darker".into());
        assert!(!apply_transcript_data(&mut active, &recap));
        assert_eq!(active.last_prompt.as_deref(), Some("make it darker"));
    }
}
