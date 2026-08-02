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
use crate::learned;
use crate::state::{self, HookMessage, HostId, LauncherState, SessionStatus};

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
) -> Result<()> {
    state::ensure_sessions_dir()?;

    let launcher_pid = std::process::id();
    // The dashboard (or server) owns the session↔window binding for any launch it
    // spawned — it threads a token (`--launch-id` locally, `--pool-session`
    // remotely) and records the window itself (next-step #6 §15). So self-report
    // `window_id` *only* when neither token is present — a hand-launched
    // `captain-miao claude` in a real Kitty window, where nothing else can supply
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

    let mut launcher_state = LauncherState {
        agent,
        launcher_pid,
        session_id: None,
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
        model: None,
        name: None,
        first_prompt: None,
        pool_session,
        launch_id,
        terminal,
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
    let sock_path = sock_dir.join(format!("{launcher_pid}.sock"));
    let _ = std::fs::remove_file(&sock_path);

    let listener = std::os::unix::net::UnixListener::bind(&sock_path)?;
    let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;

    // Per-session hooks settings file. Path is generic; contents are
    // backend-specific JSON the agent will read on launch.
    let hooks_settings_json = agent.hooks_settings_json(&sock_path.to_string_lossy());
    let settings_path = sock_dir.join(format!("{launcher_pid}-settings.json"));
    std::fs::write(&settings_path, &hooks_settings_json)?;

    let mut cmd = match agent.build_launch_command(cwd, &sock_path, &settings_path, agent_args) {
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
        _ = process_hooks(&listener, &mut launcher_state) => {
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
/// `FailedToStart` row instead of letting it vanish. The new-session window is
/// `--hold`'d, so we print the error there (the agent never got to), stamp it
/// onto the state file as `last_error`, and **block** until the user dismisses
/// it — closing the window (kitty SIGHUP) or killing the row (the dashboard
/// SIGTERMs `launcher_pid`, since there's no child) — then tear the files down.
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

async fn process_hooks(listener: &UnixListener, state: &mut LauncherState) {
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
    let mut transcript_watcher: Option<notify::RecommendedWatcher> = None;
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

                // (Re)start the transcript watcher when the path changes.
                // Claude mints a new transcript on `/resume`, so the path can
                // shift mid-session.
                if let Some(ref p) = msg.transcript_path {
                    let path = PathBuf::from(p);
                    if transcript_path.as_ref() != Some(&path) {
                        match start_file_watcher(&path, fs_tx.clone()) {
                            Ok(w) => {
                                transcript_watcher = Some(w);
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
                            Err(e) => tracing::debug!("transcript watch failed: {e}"),
                        }
                    }
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
                if let Some(path) = &transcript_path {
                    // Offload the blocking File::open+read off the runtime thread
                    // that also drives child.wait()/accept(). One read serves both
                    // the interrupt/compact signal scan and the derived-field fold
                    // (context tokens, model, title, first prompt). A join error is
                    // treated like the read failing: hold the offset + prior fold.
                    let scan_path = path.clone();
                    let scan_offset = transcript_offset;
                    let prior = transcript_data.clone();
                    let (scan, data) = tokio::task::spawn_blocking(move || {
                        let scan = agent.scan_transcript_signals(&scan_path, scan_offset);
                        let data = agent.read_transcript_stats(&scan_path, prior.as_ref());
                        (scan, data)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        (
                            TranscriptScan {
                                new_offset: scan_offset,
                                ..Default::default()
                            },
                            transcript_data.clone().unwrap_or_default(),
                        )
                    });
                    transcript_offset = scan.new_offset;
                    if scan.interrupted {
                        // Some agents (Claude) fire no hook on Esc/interrupt
                        // — without this, the session stays Active forever.
                        state.status = SessionStatus::Idle;
                        state.last_tool = None;
                    }
                    if scan.compact_aborted && state.status == SessionStatus::Compacting {
                        // Claude fires no PostCompact when `/compact` errors,
                        // so the transcript-side stderr is the only signal
                        // the user is back at the prompt.
                        state.status = SessionStatus::Idle;
                        state.last_tool = None;
                    }
                    if apply_transcript_data(state, &data) {
                        state_changed = true;
                    }
                    transcript_data = Some(data);
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
            // Learn-long-running deadline. A parked session with an unrecognized
            // background command otherwise never wakes; this fires when the
            // oldest such command crosses the threshold so the classification
            // block below can learn it and flip the row to `BackgroundServer`.
            _ = sleep_until_opt(learn_at) => {}
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
        // We only ever *demote* toward rest here; hook events own the rest→`Active`
        // direction, so a momentary `"busy"` read can never bounce us into `Active`.
        // Only the working/idle/shell statuses are consulted; the fine-grained,
        // hook/transcript-backed states (Approval, Decision, Compacting, Compacted)
        // are left untouched so the file can't clobber them.
        if session_file_event
            && let Some(cpid) = state.child_pid
            && matches!(
                state.status,
                SessionStatus::Active
                    | SessionStatus::Idle
                    | SessionStatus::BackgroundActive
                    | SessionStatus::BackgroundServer
                    | SessionStatus::ReviewPending
            )
        {
            // Offload the blocking status-file read off the runtime thread. A
            // join error is treated like the read failing (`None`), so the
            // demote-only refinement holds the status unchanged.
            let activity = tokio::task::spawn_blocking(move || agent.agent_activity(cpid))
                .await
                .unwrap_or(None);
            let next = match (&state.status, activity) {
                // Interrupt: a busy turn ended with no hook.
                (SessionStatus::Active, Some(AgentActivity::Idle)) => Some(SessionStatus::Idle),
                (SessionStatus::Active, Some(AgentActivity::BackgroundShell)) => {
                    Some(SessionStatus::BackgroundActive)
                }
                // At rest: track the background shell appearing / clearing.
                (SessionStatus::Idle, Some(AgentActivity::BackgroundShell)) => {
                    Some(SessionStatus::BackgroundActive)
                }
                (SessionStatus::BackgroundActive, Some(AgentActivity::Idle)) => {
                    Some(SessionStatus::Idle)
                }
                // A long-running server / review-watch is a background shell too —
                // when it ends (killed, human submitted, timed out) the shell is
                // gone, so settle to rest just like `BackgroundActive`.
                // Staying-in-shell is left to the classification refinement below
                // (which decides transient-task vs server vs review).
                (SessionStatus::BackgroundServer, Some(AgentActivity::Idle)) => {
                    Some(SessionStatus::Idle)
                }
                (SessionStatus::ReviewPending, Some(AgentActivity::Idle)) => {
                    Some(SessionStatus::Idle)
                }
                // Working, unknown/torn read (None), or already-consistent: hold.
                _ => None,
            };
            if let Some(next) = next {
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
        } else {
            // Not in a background-shell state: drop the per-command timers so a
            // later, unrelated background job starts its clock fresh.
            bg_first_seen.clear();
            learn_at = None;
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
        // `transcript_watcher` is held purely for its side effect — a live fs
        // watch that stops when the value drops — and is never read, so it trips
        // the assigned-but-never-read lint. Touch it to silence that; the binding
        // (not this line) is what keeps the watch alive until `run` returns.
        let _ = &transcript_watcher;
    }
}

/// Watch a single file and signal `tx` on every non-Access change. Used for both
/// the transcript and the agent's session-status file. Falls back to watching the
/// parent directory (filtering to `path`) when the file doesn't exist yet.
fn start_file_watcher(
    path: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> notify::Result<notify::RecommendedWatcher> {
    let target = path.to_path_buf();
    // The path we register and the path the backend reports back are not always
    // the same string: macOS FSEvents resolves symlinks (and `/var` → `/private/var`)
    // before reporting, while Linux inotify echoes the path as registered. Codex's
    // `transcript_path` points into the *synthetic* `$CODEX_HOME`, whose `sessions`
    // entry is a symlink to the real `~/.codex/sessions`, so on macOS every event
    // arrives under the real path and a raw-string filter drops all of them —
    // silently freezing the transcript fold (no context tokens, no interrupt scan).
    // Accept either spelling.
    let real = canonical_watch_target(path).filter(|p| *p != target);
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
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
    })?;

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
/// (first-wins prompt). The session *name* is not folded here — it comes from the
/// agent's session file (`fold_session_name`), not the transcript.
fn apply_transcript_data(state: &mut LauncherState, data: &TranscriptStats) -> bool {
    let mut changed = false;
    if state.context_tokens != data.context_tokens {
        state.context_tokens = data.context_tokens;
        changed = true;
    }
    if state.model != data.model {
        state.model = data.model.clone();
        changed = true;
    }
    if data.first_prompt.is_some() && state.first_prompt != data.first_prompt {
        state.first_prompt = data.first_prompt.clone();
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

    /// A transcript reached through a symlinked directory — Codex's rollout under
    /// the synthetic `$CODEX_HOME`, whose `sessions` entry links to `~/.codex/sessions`
    /// — must resolve to the real path, because that is the spelling macOS FSEvents
    /// reports and the watch filter compares against. Covers both the file-exists
    /// case and the parent-directory fallback (file not created yet).
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

    fn state_with(status: SessionStatus) -> LauncherState {
        LauncherState {
            agent: AgentControl::Claude,
            launcher_pid: 0,
            session_id: None,
            window_id: None,
            tab_id: None,
            cwd: String::new(),
            status,
            last_tool: None,
            updated_at: 0,
            active_since: None,
            last_prompt: None,
            child_pid: None,
            last_error: None,
            context_tokens: None,
            model: None,
            name: None,
            first_prompt: None,
            pool_session: None,
            launch_id: None,
            terminal: None,
            host: HostId::local(),
        }
    }

    fn shell(key: &str, kind: BgSeedKind) -> BgShell {
        BgShell {
            key: key.to_string(),
            kind,
        }
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
}
