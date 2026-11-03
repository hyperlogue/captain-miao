//! Shared mechanics for hook-driven agents. Agent definitions supply behavior;
//! each opaque task owns its child and observation state and publishes facts.
use crate::agent::session::*;
use crate::agent::{
    AgentActivity, AgentKind, BgShell, ResumeMetadata, TranscriptScan, TranscriptStats,
};
use crate::agents::definition::AgentDefinition;
use crate::launcher::*;
use crate::state::{self, HookEvent, HookMessage, LauncherState, SessionStatus};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
#[path = "../launcher/watch.rs"]
mod watch;
use watch::{TranscriptWatch, arm_replaced_sidecars, start_file_watcher, start_stat_poll};
#[cfg(test)]
#[path = "../launcher/io_tests.rs"]
mod io_tests;

pub(crate) async fn run(
    definition: &'static dyn AgentDefinition,
    session: SessionContext,
    approval_grace: std::time::Duration,
) -> Result<SessionExit> {
    let SessionContext {
        request,
        mut state,
        mut listener,
        updates,
        mut commands,
    } = session;
    let agent = definition.kind();
    write_hook_settings(agent, &request.socket, &request.settings)?;
    let mut command = definition.build_launch_command(
        &request.cwd,
        &request.socket,
        &request.settings,
        &request.args,
        request.shim.as_deref(),
    )?;
    command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Failed to launch agent")?;
    state.child_pid = child.id();
    seed_resumed_session(&mut state, definition.read_resume_metadata(&request.args));
    state.updated_at = LauncherState::now();
    updates.publish(&state)?;
    let exit = tokio::select! {
        status = child.wait() => status.context("Failed to wait on agent")?,
        _ = process_hooks(&mut listener, &request.socket, &mut state, &updates, approval_grace) => child.wait().await.context("Failed to wait on agent")?,
        _ = commands.stop() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(SessionExit { code: 143 });
        }
    };
    if let Some(error) = startup_exit_error(agent, &state.status, exit) {
        return Err(error);
    }
    Ok(SessionExit {
        code: exit.code().unwrap_or(1),
    })
}

fn write_hook_settings(agent: AgentKind, socket: &Path, settings: &Path) -> Result<()> {
    state::write_private(
        settings,
        &agent.hooks_settings_json(&socket.to_string_lossy()),
    )
}

// =============================================================================
// Hook events -> state
// =============================================================================

/// Coalesces state-file writes: an observable change schedules a trailing flush
/// [`WRITE_THROTTLE`](Self::WRITE_THROTTLE) out, so a burst of hook/transcript
/// updates lands as one write — and one dashboard fan-out — rather than one per
/// event. Real-time-ness isn't a requirement here; a sub-second settle is fine.
///
/// The pair is a type because it carries an invariant the loop used to maintain
/// by hand across three separate sites: **a deadline is only ever set on a dirty
/// throttle**. Set one without the flag and the elapsed deadline flushes
/// nothing while still counting as armed, so the *next* change rides a deadline
/// that already passed and writes immediately — silently defeating the
/// coalescing this exists for. Going through [`mark`](Self::mark) is what makes
/// that unrepresentable.
#[derive(Default)]
struct WriteThrottle {
    dirty: bool,
    /// Read directly by the `select!` timer arm, which has to await it; every
    /// *mutation* goes through a method, which is what holds the invariant.
    flush_at: Option<tokio::time::Instant>,
}

impl WriteThrottle {
    const WRITE_THROTTLE: std::time::Duration = std::time::Duration::from_millis(500);

    /// Record an observable change. `now` forces an immediate flush, overriding
    /// a pending trailing deadline; otherwise a later change inside the window
    /// rides the deadline already set (that is the coalescing).
    fn mark(&mut self, now: bool) {
        self.dirty = true;
        if now {
            self.flush_at = Some(tokio::time::Instant::now());
        } else if self.flush_at.is_none() {
            self.flush_at = Some(tokio::time::Instant::now() + Self::WRITE_THROTTLE);
        }
    }

    /// Whether to write now — the deadline has passed, whether the timer arm
    /// woke us or a later event arrived after it elapsed.
    fn flush_due(&self) -> bool {
        self.dirty
            && self
                .flush_at
                .is_some_and(|d| tokio::time::Instant::now() >= d)
    }

    /// Called after a successful write; re-arms the throttle for the next change.
    fn flushed(&mut self) {
        self.dirty = false;
        self.flush_at = None;
    }
}

/// Seed a promptless resume after its child spawned, before hooks are serviced.
/// The persisted transcript contributes facts only: its old turn markers do
/// not describe activity in the newly started process.
fn seed_resumed_session(state: &mut LauncherState, metadata: Option<ResumeMetadata>) {
    if state.status != SessionStatus::Starting
        || state.child_pid.is_none()
        || state.session_id.is_some()
    {
        return;
    }
    if let Some(metadata) = metadata {
        state.session_id = Some(metadata.session_id);
        apply_transcript_data(state, &metadata.stats);
        state.status = SessionStatus::Idle;
    }
}

async fn process_hooks(
    listener: &mut UnixListener,
    sock_path: &Path,
    state: &mut LauncherState,
    updates: &SessionPublisher,
    approval_grace: std::time::Duration,
) {
    let agent = state.agent;
    let mut hooks = agent.hook_session();
    hooks.start();
    // Watch the agent's transcript file instead of polling. For Claude,
    // a transcript modification while we're in WaitingForApproval is a
    // near-instant "approval granted" signal (no need to wait for the
    // approved tool's PostToolUse hook, which can be many seconds away).
    let (fs_tx, mut fs_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Session-status-file changes get their OWN channel, kept separate from the
    // transcript's `fs_rx`. For agents whose transcript writes clear approvals,
    // `on_transcript_changed` treats a wake past the grace window as "the
    // permission dialog was dismissed → Active". A session-file write (e.g. a
    // background job finishing during a later turn's approval prompt) is not
    // that signal and must not reach it.
    let (sess_tx, mut sess_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut transcript_watcher: Option<TranscriptWatch> = None;
    let mut transcript_path: Option<PathBuf> = None;
    // Grok's `signals.json`: rewritten independently of `summary.json` (also
    // a replace, not an in-place edit) and never named on the hook. Watched
    // only once the file exists so we never fall back to the session dir.
    let mut sidecar_watchers: Vec<TranscriptWatch> = Vec::new();
    // Byte offset into the transcript for incremental scanning. Reset to 0
    // whenever `transcript_path` changes so we pick up existing signals in
    // a resumed session.
    let mut transcript_offset: u64 = 0;
    // Incremental fold of the transcript-derived fields (context tokens, model,
    // title, first prompt) the launcher stamps onto the state file so the
    // dashboard never reads a transcript. Carries Claude's byte cursor; reset to
    // None on a path change so a resumed session's new transcript is folded from
    // the top.
    let mut transcript_data: Option<TranscriptRead> = None;
    // Whether the last Stop was held off a rest state because the session
    // re-drives itself (`AgentKind::self_continues` — a Codex goal). The
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
    // the initial publication in `run` happened before this).
    if let Some(cpid) = state.child_pid
        && apply_session_name(state, agent.session_name(cpid))
    {
        let _ = updates.publish(state);
    }
    // Grace period: when we enter WaitingForApproval, ignore transcript
    // changes for a short window. The assistant message containing the
    // tool_use is written to the transcript in the same turn, and notify
    // can deliver that event AFTER the PermissionRequest hook fires —
    // without the grace period on_transcript_changed would immediately
    // clear WaitingForApproval back to Active.
    let mut approval_entered_at: Option<Instant> = None;

    let mut throttle = WriteThrottle::default();

    // Watch the pathname and its parent lifetime: removal/replacement must
    // recover even while the agent is quiet, without periodic socket stats.
    let mut bound_identity = file_identity(sock_path);
    let mut socket_changes = crate::fs_events::PathChanges::new(sock_path);

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
                // Event-driven watches start here. Kimi/Antigravity read only
                // on hooks; Codex on macOS uses a poll managed at the bottom of
                // the loop, running only while the session is off Idle. Those
                // paths are adopted without a watcher, dropping any old one.
                //
                // A child session's hook shares this socket and often names its
                // own transcript. Adopting that path folds the child's title
                // (and tokens) onto the parent row, then the parent's next hook
                // folds them back — the dashboard flickering between the two.
                // `dispatch_hook` is what ignores the child; this has to happen
                // first because the watch is armed before dispatch.
                if msg.session_is_child != Some(true)
                    && let Some(ref p) = msg.transcript_path
                {
                    let path = PathBuf::from(p);
                    if transcript_path.as_ref() != Some(&path) {
                        hooks.transcript_changed(state);
                        transcript_data = None;
                        let adopted = if agent.refresh_transcript_on_hook() {
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
                            sidecar_watchers.clear();
                            arm_replaced_sidecars(
                                agent,
                                &path,
                                fs_tx.clone(),
                                &mut sidecar_watchers,
                            );
                            // Seed the offset to the new transcript's end so
                            // historical interrupt/compact signals from a
                            // resumed session aren't replayed as fresh.
                            let scan = agent.scan_transcript_signals(&path, 0);
                            transcript_offset = scan.new_offset;
                            // Seed the derived fields so a
                            // resumed/idle session shows context, model,
                            // title, and first prompt before its next write.
                            // (Synchronous one-shot initialization; the backend
                            // chooses a full scan or head/tail reads.)
                            let mut data = TranscriptRead::new(agent);
                            data.read(&path);
                            apply_transcript_data(state, &data);
                            transcript_data = Some(data);
                        }
                    } else if sidecar_watchers.is_empty() {
                        // `signals.json` is often minted after the first
                        // summary; a later hook is when we first see the file,
                        // still event-driven (no poll, no parent-dir watch).
                        arm_replaced_sidecars(
                            agent,
                            &path,
                            fs_tx.clone(),
                            &mut sidecar_watchers,
                        );
                    }
                }

                // Hook-driven metadata (Kimi/Antigravity), and the poll-watched
                // Codex rollout on macOS, catch up before dispatch. Codex writes
                // the rollout lines *before* firing the matching hook
                // (`token_count` lands ~20ms ahead of
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
                if agent.refresh_transcript_on_hook()
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
                // Taken before the dispatch consumes the message, and handed to
                // the fold after it: `dispatch_hook` stamps the model onto the
                // state, but the fold's own accumulator still remembers the last
                // turn's, and the next transcript write would put that back —
                // for Claude a `/model` echoes itself into the transcript, so
                // "next" is milliseconds away. See `TranscriptStats::adopt_model`.
                let hook_model = msg.model.clone();
                hooks.dispatch_hook(state, msg);
                if let Some(model) = hook_model.filter(|m| !m.trim().is_empty())
                    && let Some(data) = transcript_data.as_mut()
                {
                    data.adopt_model(&model);
                }
                tracing::debug!(
                    target: "captain_miao::hook",
                    "dispatch pid={} event={:?} status={:?}",
                    std::process::id(),
                    dispatched_event,
                    state.status,
                );
                // A Stop that did *not* land on a rest state may be a session
                // that re-drives itself (`codex::dispatch_hook`, under a goal).
                // Note it, and arm the one confirmation the bet needs — see
                // `confirm_hold_at` and `stop_holds_for_goal`.
                if dispatched_event == HookEvent::Stop {
                    held_for_goal = stop_holds_for_goal(agent, &state.status);
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
                // Grok replaces `summary.json` and `signals.json` (new inodes).
                // The wake above is the old inode's last event — re-arm both
                // so the next `/rename` or token rewrite is seen too. Still
                // event-driven: this runs only because notify just fired,
                // never on a timer, and never by watching the session dir
                // (`updates.jsonl` lives there).
                if agent.transcript_path_is_replaced()
                    && matches!(transcript_watcher, Some(TranscriptWatch::Event(_)))
                    && let Some(path) = &transcript_path
                {
                    match start_file_watcher(path, fs_tx.clone()) {
                        Ok(w) => transcript_watcher = Some(TranscriptWatch::Event(w)),
                        Err(e) => tracing::debug!("transcript rewatch failed: {e}"),
                    }
                    sidecar_watchers.clear();
                    arm_replaced_sidecars(agent, path, fs_tx.clone(), &mut sidecar_watchers);
                }
                if state.status != prev_status
                    || state.last_tool != prev_last_tool
                {
                    state_changed = true;
                }
            }
            _ = hooks.changed() => {
                if hooks.apply_update(state) {
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
            _ = sleep_until_opt(throttle.flush_at) => {}
            // Held-Stop confirmation deadline. The block below re-asks the
            // agent whether the turn it promised is still coming; nothing else
            // would ever wake a row holding Active on a promise that lapsed.
            _ = sleep_until_opt(confirm_hold_at) => {}
            // Socket-path changes are independent of session activity.
            _ = socket_changes.changed() => {
                if hook_socket_lost(sock_path, bound_identity) {
                    bound_identity = restore_hook_socket(listener, sock_path);
                    if bound_identity.is_none() {
                        socket_changes.retry();
                    }
                }
                // Deliberately the whole iteration: this wake carries no state
                // signal, and falling through would run the reconciliation — and
                // on a background row its process-tree scan, a `ps` exec on
                // macOS — purely as a side effect of restoring a socket.
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
        // lagging file read (see the flag's declaration). Three jobs:
        //   - A turn can end with NO hook — an interrupt (Esc) fires no `Stop`, so
        //     `Active` would otherwise stick forever. When the file reports the
        //     agent is at rest, settle `Active` to the rest shape it reports.
        //   - A turn can also end while a `run_in_background` shell keeps running
        //     (`"shell"`), which reads as `BackgroundActive` rather than `Idle`.
        //   - A turn can equally *start* with no hook: a queued prompt's
        //     `UserPromptSubmit` fires when it is queued (mid-turn, on a row
        //     already `Active`), never when it is dequeued, so only the file's
        //     `busy` marks the flushed turn — which is why `Idle` promotes.
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
            )
        {
            // Offload the blocking status-file read off the runtime thread. A
            // join error is treated like the read failing (`None`), so the
            // refinement holds the status unchanged.
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

        // Report live commands for the dashboard's explicit idle-task policy.
        // Hooks report Grok/omp jobs themselves; their process scan is None.
        if state.status == SessionStatus::BackgroundActive
            && let Some(cpid) = state.child_pid
        {
            let shells = tokio::task::spawn_blocking(move || agent.bg_shells(cpid))
                .await
                .unwrap_or(None);
            if promote_stale_background(state, shells.as_deref(), activity) {
                state_changed = true;
            }
            if let Some(shells) = shells {
                let mut commands: Vec<_> = shells.into_iter().map(|s| s.key).collect();
                commands.sort();
                commands.dedup();
                if state.background_commands != commands {
                    state.background_commands = commands;
                    state_changed = true;
                }
            }
        }
        if state.status != SessionStatus::BackgroundActive && !state.background_commands.is_empty()
        {
            state.background_commands.clear();
            state_changed = true;
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
            // Switching into an actionable state flushes now, overriding any
            // pending trailing deadline, so the prompt isn't held back.
            throttle.mark(state.status.needs_attention() && !was_attention);
        }
        if throttle.flush_due() {
            let _ = updates.publish(state);
            throttle.flushed();
        }
        // Lifecycle of the poll-backed transcript watch (Codex on macOS — see
        // `AgentKind::transcript_poll_interval`): it runs only while the
        // session is off Idle. An idle session's rollout doesn't change without
        // a hook firing first (`UserPromptSubmit` wakes this loop and the next
        // pass lands here with the status already Active), so parking the
        // watcher at Idle makes an at-rest session cost nothing — no 2s stat
        // cadence while a session sits parked for hours. A session that
        // re-drives itself does not weaken that premise, because it never
        // reaches Idle to be parked at: its Stop is held Active by
        // `AgentKind::self_continues` precisely so no watch has to stay
        // awake waiting for the turn it starts next. For Codex, Idle is the
        // only parked state: WaitingForApproval/WaitingForDecision need the
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
        // `transcript_watcher` / `sidecar_watchers` are otherwise held purely
        // for their side effect — a live fs watch that stops when the value
        // drops. The binding (not this line) is what keeps the watch alive
        // until `run` returns.
        let _ = &transcript_watcher;
        let _ = &sidecar_watchers;
    }
}

#[derive(Clone)]
struct TranscriptRead {
    reader: std::sync::Arc<std::sync::Mutex<Box<dyn super::transcript::TranscriptReader>>>,
    stats: TranscriptStats,
}
impl TranscriptRead {
    fn new(agent: AgentKind) -> Self {
        Self {
            reader: std::sync::Arc::new(std::sync::Mutex::new(
                agent.definition().transcript_reader(),
            )),
            stats: TranscriptStats::default(),
        }
    }
    fn read(&mut self, path: &Path) {
        self.stats = self
            .reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read(path);
    }
    fn adopt_model(&mut self, model: &str) {
        self.stats.adopt_model(model);
        self.reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .adopt_model(model);
    }
}
impl std::ops::Deref for TranscriptRead {
    type Target = TranscriptStats;
    fn deref(&self) -> &TranscriptStats {
        &self.stats
    }
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
/// arm, and — for hook-driven or poll-watched metadata — the pre-dispatch
/// catch-up (see the call site there for why it must run *before*
/// `dispatch_hook`).
async fn rescan_transcript(
    agent: AgentKind,
    path: &Path,
    offset: &mut u64,
    data: &mut Option<TranscriptRead>,
    held_for_goal: &mut bool,
    state: &mut LauncherState,
) -> bool {
    let scan_path = path.to_path_buf();
    let scan_offset = *offset;
    let mut prior = data.clone().unwrap_or_else(|| TranscriptRead::new(agent));
    let (scan, fresh) = tokio::task::spawn_blocking(move || {
        let scan = agent.scan_transcript_signals(&scan_path, scan_offset);
        prior.read(&scan_path);
        (scan, prior)
    })
    .await
    .unwrap_or_else(|_| {
        (
            TranscriptScan {
                new_offset: scan_offset,
                ..Default::default()
            },
            data.clone().unwrap_or_else(|| TranscriptRead::new(agent)),
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
        //
        // This state set is deliberately **wider** than the one
        // `reconcile_activity` promotes, and the difference is the evidence,
        // not an oversight. A turn-open marker is specific: it cannot be
        // anything but a new turn, so it may promote `Compacted`. The session
        // file's `busy` is coarse — a mid-turn auto-compaction reads `busy` by
        // construction — so it may not. Keep them different for that reason or
        // change both, but do not "align" them.
        if matches!(state.status, SessionStatus::Idle | SessionStatus::Compacted) {
            state.status = SessionStatus::Active;
            state.last_tool = None;
        }
    }
    let changed = apply_transcript_data(state, &fresh);
    *data = Some(fresh);
    changed
}

fn on_transcript_changed(
    state: &mut LauncherState,
    approval_entered_at: Option<Instant>,
    grace: std::time::Duration,
) {
    // The only state transition driven by transcript changes today is leaving
    // WaitingForApproval — Claude has no "approval granted" hook, so a later
    // transcript write serves as evidence that execution resumed. This depends
    // on the agent: Grok's watched metadata can change while permission still
    // waits, so its hooks alone own this transition.
    //
    // However, the assistant message that *contains* the tool_use is also
    // written to the transcript around the same time the PermissionRequest
    // hook fires. FSEvents/notify can deliver that write event after we've
    // already set WaitingForApproval, so we ignore transcript changes within
    // a short grace window after entering the state.
    if state.status == SessionStatus::WaitingForApproval
        && state.agent.transcript_write_clears_approval()
    {
        let past_grace = approval_entered_at
            .map(|t| t.elapsed() >= grace)
            .unwrap_or(true);
        if past_grace {
            state.status = SessionStatus::Active;
        }
    }
}

// =============================================================================
// Folding a transcript read into the row
// =============================================================================

/// Stamp a freshly-read session name onto `state.name`, returning whether it
/// changed. Forward-only: `None` — a transient unreadable/absent session file, or a
/// backend without one — leaves an already-shown name untouched, so a torn read
/// never blanks the row. The name is the agent's own (Claude writes both its
/// auto-derived title and the user's `/rename` to `~/.claude/sessions/<pid>.json`,
/// read via `AgentKind::session_name`); folding it here rides the state file to
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

/// Whether two directory strings name the same directory — equal spellings, or
/// two that resolve to one place (a launch through a symlink against the
/// physical path Claude records, which is `process.cwd()`'s). Resolution runs
/// only when the spellings differ, on the host that owns both paths.
fn same_directory(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
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
    // The session's directory, read off the entries the fold parsed. Last-wins
    // like `name` — a worktree entry is a real move — but the spelling stays
    // the launch's while the two are the same directory: Claude records the
    // physical path, and a row launched through a symlink must not flip to it
    // on its first turn.
    if let Some(cwd) = data.cwd.as_deref().filter(|c| !c.is_empty())
        && !same_directory(&state.cwd, cwd)
    {
        state.cwd = cwd.to_string();
        changed = true;
    }
    // Grok's `last_turn_summary`: the glance column on an idle/resumed row.
    // Skip while the turn is in flight — `PromptSubmit` just wrote the user's
    // question, and a sidecar rewrite from the *previous* turn must not
    // clobber it. Waiting/compacting count as in-flight too. Antigravity folds
    // the actual user input, so its prompt also updates during a turn.
    if let Some(prompt) = data
        .last_prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && (state.agent.definition().last_prompt_is_live()
            || !matches!(
                state.status,
                SessionStatus::Active
                    | SessionStatus::Compacting
                    | SessionStatus::WaitingForApproval
                    | SessionStatus::WaitingForDecision
                    | SessionStatus::BackgroundActive
            ))
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

/// The status a fresh session-file read settles `current` to, or `None` to hold
/// it. The agent's own file is authoritative on the coarse
/// working/idle/background-shell axis; this is the table that mirrors it (the
/// call site owns *when* it runs — only on a session-file wake).
///
/// **`Idle` mirrors both ways.** Hooks normally own the rest→`Active` direction,
/// but a *queued* prompt has no hook to own it: Claude fires `UserPromptSubmit`
/// when the message is **queued** — mid-turn, on a row that is already `Active`
/// — and nothing at all when it is later dequeued. So the turn that starts when
/// the queue flushes (the previous turn ending, or the user cancelling it with
/// Esc) announces itself only in the session file, and the boundary is a ~20ms
/// `idle` blip immediately followed by `busy`. Demoting on the blip and then
/// refusing the `busy` that corrects it left the row `Idle` for the whole
/// queued turn — until some later hook (a `PreToolUse`, or the turn's own
/// `Stop`) happened to repair it. The file is the authority on this axis in
/// *both* directions, so `busy` promotes `Idle` back, and the write throttle
/// swallows the blip entirely.
///
/// The other rest states still refuse to promote, each for its own reason: a
/// background row needs the process tree to corroborate (that is
/// `promote_stale_background`), and `Compacted` would trade away the compaction
/// signal — a mid-turn auto-compaction reads `"busy"` by construction. An
/// unknown/torn read (`None`) always holds.
///
/// Only the working/idle/shell axis is consulted; the fine-grained,
/// hook/transcript-backed states (Approval, Decision, `Compacting`) are left
/// untouched so the file can't clobber them. `Compacted` is the one that also
/// takes a `BackgroundShell`, for the same reason `Idle` does: it is a **rest**
/// status, so a `run_in_background` shell that outlives the compaction is the
/// truer shape of the row — otherwise a session compacted while an r3
/// review-watch runs reads `Compacted` until the watch ends, rather than the
/// `Task` its shells report. It is never demoted to `Idle`, though: both
/// are at rest, so that trades away the compaction signal (and the follow-up
/// bell armed on entering it) for nothing.
/// Whether the `Stop` just dispatched left an *unconfirmed* hold — a busy row
/// resting on a prediction, which `confirm_hold_at` then has to settle.
///
/// Busy alone is not enough, and treating it as enough is a live bug: two
/// backends now hold `Active` through a `Stop` for opposite reasons. Codex bets
/// on its goal store, which answers before Codex has decided, so the hold is a
/// prediction and needs the deadline. Claude holds because its own session file
/// reads `busy` — a subagent still running — which is *evidence*, revoked by
/// that file's next write and by nothing else. Confirming Claude's hold demotes
/// a working row to `Idle` two seconds in, and the file (frozen at `busy` for
/// the whole subagent phase) then fires no wake to undo it: the row reads at
/// rest until some later tool hook lands. `may_self_continue` is what tells the
/// two apart.
fn stop_holds_for_goal(agent: AgentKind, status: &SessionStatus) -> bool {
    status.is_busy() && agent.may_self_continue()
}

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
        // Queue flush: a dequeued prompt fires no hook, so the file's `busy` is
        // the only word that the row is working again.
        (SessionStatus::Idle, Some(AgentActivity::Working)) => Some(SessionStatus::Active),
        // At rest: track the background shell appearing / clearing.
        (SessionStatus::Idle | SessionStatus::Compacted, Some(AgentActivity::BackgroundShell)) => {
            Some(SessionStatus::BackgroundActive)
        }
        (SessionStatus::BackgroundActive, Some(AgentActivity::Idle)) => Some(SessionStatus::Idle),
        // Working on a background/compacted row, unknown/torn read (None), or
        // already-consistent: hold.
        _ => None,
    }
}

/// Retire a background-shell status the process tree has disproved, promoting
/// the row to `Active`. Returns whether the status changed.
///
/// A background status asserts *"a `run_in_background` shell is
/// running"*. The live process tree is the authority on that — the same
/// authority that put the row into the state to begin with — so when it reads
/// cleanly and holds no background shell, the assertion is false and the row is
/// stale by its own definition. It must be `Active` or `Idle`, and the session
/// file says which.
///
/// The mirror above promotes on the file alone only from `Idle`, where a queued
/// turn leaves it no choice; a background row asserts something the file cannot
/// see, so this needs two independent signals to agree. Both halves are
/// load-bearing:
///
/// - `Some(&[])` is "tree read fine, nothing running"; `None` is "couldn't read
///   the tree" and must never promote — acting on an unreadable tree is exactly
///   the spurious bounce this corroboration exists to prevent.
/// - `activity` is `None` on any wake that didn't re-read the session file, so a
///   stale value can never be mistaken for fresh evidence.
///
/// Without this, a session whose background shell ends *while the agent goes
/// back to work* has no path back to `Active`: the mirror only exits these
/// states toward `Idle`. Hooks normally cover the gap in milliseconds, so the hole is
/// invisible until hooks are missing — a lost socket, a hook binary that can't
/// run — at which point the row reads `Task` for every working
/// turn until the agent next comes to rest.
fn promote_stale_background(
    state: &mut LauncherState,
    shells: Option<&[BgShell]>,
    activity: Option<AgentActivity>,
) -> bool {
    if !matches!(state.status, SessionStatus::BackgroundActive) {
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

/// Sleep until `deadline`, or await forever when there's no pending flush — so
/// the select! treats "no scheduled write" as an inert arm.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn only_an_unprompted_startup_failure_holds_the_window() {
        use std::os::unix::process::ExitStatusExt;
        let code = |c: i32| std::process::ExitStatus::from_raw(c << 8);
        let signal = std::process::ExitStatus::from_raw;
        let holds = |status, exit| startup_exit_error(AgentKind::Codex, &status, exit).is_some();

        assert!(holds(SessionStatus::Starting, code(1)));
        assert!(holds(SessionStatus::Starting, signal(libc::SIGABRT)));
        // The user quit or closed the window, or the agent already reported.
        assert!(!holds(SessionStatus::Starting, code(0)));
        for ended in [129, 130, 143] {
            assert!(!holds(SessionStatus::Starting, code(ended)), "{ended}");
        }
        for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
            assert!(!holds(SessionStatus::Starting, signal(sig)), "{sig}");
        }
        assert!(!holds(SessionStatus::Idle, code(1)));

        let error = startup_exit_error(AgentKind::Codex, &SessionStatus::Starting, code(1));
        assert_eq!(
            format!("{:#}", error.unwrap()),
            "Codex exited during startup (exit status: 1); its output, if any, is above"
        );
    }

    #[test]
    fn runtime_sweep_removes_dead_launchers_images_and_keeps_live_ones() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cm-image-sweep-{nonce}"));
        let dead = dir.join("2000000000-images");
        let live = dir.join(format!("{}-images", std::process::id()));
        for image_dir in [&dead, &live] {
            std::fs::create_dir_all(image_dir).unwrap();
            std::fs::write(image_dir.join("image.png"), b"image fixture").unwrap();
        }
        let dead_relay = dir.join("2000000000-codex.sock");
        let live_relay = dir.join(format!("{}-codex.sock", std::process::id()));
        std::fs::File::create(&dead_relay).unwrap();
        std::fs::File::create(&live_relay).unwrap();
        sweep_dead_launcher_runtime_files(&dir);
        assert!(!dead_relay.exists());
        assert!(live_relay.exists());
        assert!(!dead.exists());
        assert!(live.join("image.png").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The invariant the type exists for: a marked throttle always has a
    /// deadline, so an armed deadline can never outlive the flag. Left to the
    /// loop, setting one without the other made the next change ride an
    /// already-elapsed deadline and write immediately, defeating the coalescing.
    #[tokio::test]
    async fn marking_always_arms_a_deadline() {
        let mut t = WriteThrottle::default();
        assert!(!t.dirty && t.flush_at.is_none());
        assert!(!t.flush_due(), "a clean throttle never flushes");

        t.mark(false);
        assert!(t.dirty && t.flush_at.is_some());
        assert!(!t.flush_due(), "the trailing deadline has not passed yet");

        // A later change inside the window rides the same deadline.
        let first = t.flush_at;
        t.mark(false);
        assert_eq!(t.flush_at, first, "coalesced onto the existing deadline");

        // An actionable state overrides it and flushes now.
        t.mark(true);
        assert!(t.flush_due());

        t.flushed();
        assert!(!t.dirty && t.flush_at.is_none());
        assert!(!t.flush_due());
    }
    use crate::agent::AgentKind;

    /// A local (non-pool) launcher must not exit because a daemon somewhere
    /// restarted — that daemon never owned this process.
    #[tokio::test]
    async fn a_local_launcher_does_not_watch_the_pool_daemon() {
        tokio::select! {
            _ = wait_until_minting_daemon_gone(None) => {
                panic!("a non-pooled launcher must not exit on daemon death")
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        tokio::select! {
            _ = wait_until_minting_daemon_gone(Some("cm-away")) => {
                panic!("an unparseable pool name must not look like a dead daemon")
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    /// The minting pid is dead (a 2e9 pid is outside the usual allocation
    /// range and fits in a positive `i32`, so `kill` is ESRCH rather than a
    /// process-group broadcast), so the watch must resolve on the first check
    /// rather than wait out a poll.
    #[tokio::test]
    async fn a_pooled_launcher_exits_once_its_minting_pid_is_dead() {
        wait_until_minting_daemon_gone(Some("cm-grok-2000000000-1")).await;
    }

    /// Grok rewrites `summary.json` by replace (new inode). The file watch
    /// fires for that replace, then dies with the old inode; the launcher
    /// re-arms it — still event-driven, not a poll, and not a watch on the
    /// session directory (`updates.jsonl` lives there). A second replace is
    /// visible only after that re-arm.
    #[test]
    fn file_watcher_sees_a_replace_and_the_next_one_after_rewatch() {
        let base = std::env::temp_dir().join(format!("cm-watch-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("summary.json");
        std::fs::write(&path, b"{\"generated_title\":\"first\"}\n").unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let mut _w = start_file_watcher(&path, tx.clone()).expect("watch starts");
            // Let the backend register before the first replace.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            fn replace(path: &std::path::Path, body: &[u8]) {
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, body).unwrap();
                std::fs::rename(&tmp, path).unwrap();
            }

            replace(&path, b"{\"generated_title\":\"renamed\"}\n");
            let woke = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
            assert!(woke.is_ok(), "first atomic replace never woke the watcher");
            while rx.try_recv().is_ok() {}

            // Same contract as `process_hooks` after an fs wake for Grok.
            _w = start_file_watcher(&path, tx.clone()).expect("rewatch");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            replace(&path, b"{\"generated_title\":\"renamed again\"}\n");
            let woke = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
            assert!(woke.is_ok(), "replace after re-arm never woke the watcher");
        });

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The stat-polling transcript watch (the Codex-on-macOS case — see
    /// `AgentKind::transcript_poll_interval`) must report appends made
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
            let mut data: Option<TranscriptRead> = None;
            let mut held_for_goal = false;

            rescan_transcript(
                AgentKind::Codex,
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
                AgentKind::Codex,
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

    /// A memory flush rewrites Grok's summary while a command still waits for
    /// permission. The notification reached Approval, then that metadata wake
    /// incorrectly restored Active after the grace period.
    #[test]
    fn grok_metadata_writes_preserve_a_pending_approval() {
        let agent = AgentKind::Grok;
        let mut state = LauncherState::for_test(agent, SessionStatus::Active);
        state.last_tool = Some("run_terminal_command".into());
        agent.hook_session().dispatch_hook(
            &mut state,
            agent
                .parse_hook_payload(
                    HookEvent::PermissionRequest,
                    r#"{"notificationType":"permission_prompt"}"#,
                )
                .unwrap(),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);

        let entered_at = Some(Instant::now() - Duration::from_secs(21));
        on_transcript_changed(&mut state, entered_at, Duration::from_secs(2));
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
        assert_eq!(state.last_tool.as_deref(), Some("run_terminal_command"));

        // An actual tool completion still releases the wait through the hook.
        agent.hook_session().dispatch_hook(
            &mut state,
            agent
                .parse_hook_payload(
                    HookEvent::PostToolUse,
                    r#"{"toolName":"run_terminal_command"}"#,
                )
                .unwrap(),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool, None);
    }

    #[test]
    fn conversation_writes_clear_approval_only_after_the_grace_period() {
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            let mut state = LauncherState::for_test(agent, SessionStatus::WaitingForApproval);
            let grace = Duration::from_secs(2);
            on_transcript_changed(&mut state, Some(Instant::now()), grace);
            assert_eq!(state.status, SessionStatus::WaitingForApproval, "{agent:?}");

            let entered_at = Some(Instant::now() - Duration::from_secs(21));
            on_transcript_changed(&mut state, entered_at, grace);
            assert_eq!(state.status, SessionStatus::Active, "{agent:?}");

            state.status = SessionStatus::WaitingForDecision;
            on_transcript_changed(&mut state, entered_at, grace);
            assert_eq!(state.status, SessionStatus::WaitingForDecision, "{agent:?}");
        }
    }

    #[test]
    fn kimi_metadata_does_not_override_its_permission_result_hook() {
        let mut state = LauncherState::for_test(AgentKind::Kimi, SessionStatus::WaitingForApproval);
        on_transcript_changed(
            &mut state,
            Some(Instant::now() - Duration::from_secs(21)),
            Duration::from_secs(2),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
        AgentKind::Kimi.hook_session().dispatch_hook(
            &mut state,
            serde_json::from_value(serde_json::json!({
                "event": HookEvent::ElicitationResult
            }))
            .unwrap(),
        );
        assert_eq!(state.status, SessionStatus::Active);
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
                let mut data: Option<TranscriptRead> = None;
                let mut held_for_goal = false;
                rescan_transcript(
                    AgentKind::Codex,
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
            let mut data: Option<TranscriptRead> = None;
            let mut held_for_goal = true;
            rescan_transcript(
                AgentKind::Codex,
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
        assert!(!AgentKind::Codex.self_continues("no-goal-was-ever-set-for-this-id"));
        assert!(!AgentKind::Claude.self_continues("no-goal-was-ever-set-for-this-id"));
    }

    #[test]
    fn a_codex_resume_has_its_id_and_details_before_the_first_prompt() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("cm-resume-startup-{nonce}"));
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("rollout.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"resumed-thread\",\"cwd\":\"/work/repo\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Fix the parser\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.5\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":4242}}}}\n",
                // Even a previous process's unfinished turn is history.
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            ),
        )
        .unwrap();
        let conn = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            ["resumed-thread", path.to_str().unwrap()],
        )
        .unwrap();
        let agent = AgentKind::Codex;
        let args = agent.resume_args("resumed-thread", false);
        let mut state = LauncherState::for_test(agent, SessionStatus::Starting);
        state.child_pid = Some(42);
        let metadata = crate::agents::codex::read_resume_metadata_at(&home, &args);
        seed_resumed_session(&mut state, metadata);
        assert_eq!(
            state.status,
            SessionStatus::Idle,
            "no prompt or hook is needed"
        );
        assert_eq!(state.session_id.as_deref(), Some("resumed-thread"));
        assert_eq!(state.first_prompt.as_deref(), Some("Fix the parser"));
        assert_eq!(state.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(state.context_tokens, Some(4242));
        assert_eq!(
            state.name, None,
            "the host overlays the title by session id"
        );

        let msg = agent
            .parse_hook_payload(
                HookEvent::PromptSubmit,
                r#"{"session_id":"resumed-thread","prompt":"Continue"}"#,
            )
            .unwrap();
        agent.hook_session().dispatch_hook(&mut state, msg);
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_prompt.as_deref(), Some("Continue"));
        drop(conn);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn resume_metadata_never_replaces_a_failed_or_already_reported_start() {
        for (status, child_pid, session_id) in [
            (SessionStatus::Starting, None, None),
            (SessionStatus::FailedToStart, None, None),
            (SessionStatus::Active, Some(42), None),
            (SessionStatus::Starting, Some(42), Some("reported-thread")),
        ] {
            let mut state = LauncherState::for_test(AgentKind::Codex, status);
            state.child_pid = child_pid;
            state.session_id = session_id.map(str::to_string);
            let before = state.clone();
            seed_resumed_session(
                &mut state,
                Some(ResumeMetadata {
                    session_id: "resumed-thread".into(),
                    stats: TranscriptStats::default(),
                }),
            );
            assert_eq!(state, before);
        }
        let mut state = LauncherState::for_test(AgentKind::Codex, SessionStatus::Starting);
        state.child_pid = Some(42);
        let before = state.clone();
        seed_resumed_session(&mut state, None);
        assert_eq!(state, before, "an unreadable store is no startup evidence");
    }

    fn state_with(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentKind::Claude, status)
    }

    fn shell(key: &str) -> BgShell {
        BgShell {
            key: key.to_string(),
        }
    }

    /// Only a backend that can re-drive itself leaves an *unconfirmed* hold.
    /// Claude's busy `Stop` is `status_after_stop` mirroring a session file that
    /// reads `busy` (a subagent is still running) — arming the confirmation on it
    /// parks a working row at `Idle` two seconds later, and the file, frozen at
    /// `busy` until that subagent phase ends, then fires no wake to undo it.
    #[test]
    fn only_a_self_continuing_backend_leaves_a_hold_to_confirm() {
        use AgentKind as C;
        use SessionStatus as S;

        assert!(stop_holds_for_goal(C::Codex, &S::Active));
        // The bug this pins: Claude holds `Active` through a Stop too, on
        // evidence rather than a bet, so there is nothing to confirm.
        assert!(!stop_holds_for_goal(C::Claude, &S::Active));
        assert!(!stop_holds_for_goal(C::Claude, &S::BackgroundActive));
        // A Stop that landed at rest never held anything, whatever the backend.
        assert!(!stop_holds_for_goal(C::Codex, &S::Idle));
    }

    /// The mirror of the agent's session file: demote-only apart from `Idle`,
    /// plus the one rest status that also takes a background shell.
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
        // the background state.
        assert_eq!(
            reconcile_activity(&S::Idle, Some(A::BackgroundShell)),
            Some(S::BackgroundActive)
        );
        assert_eq!(
            reconcile_activity(&S::BackgroundActive, Some(A::Idle)),
            Some(S::Idle)
        );

        // `Idle` is the one rest status a "busy" read promotes: the turn a
        // queued prompt flushes into announces itself nowhere else.
        assert_eq!(
            reconcile_activity(&S::Idle, Some(A::Working)),
            Some(S::Active)
        );
        // Everywhere else a "busy" read still holds — a background row needs
        // `promote_stale_background`'s corroborating tree, and `Compacted` would
        // lose the compaction signal to a mid-turn auto-compaction's own `busy`.
        // An unknown/torn read holds everything, `Idle` included.
        for st in [S::Compacted, S::BackgroundActive] {
            assert_eq!(reconcile_activity(&st, Some(A::Working)), None);
        }
        for st in [S::Idle, S::Compacted, S::BackgroundActive] {
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
    /// the watch does, instead of the `Task` its shells report. It is only
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

    /// The queue flush, walked end to end. Claude fires `UserPromptSubmit` when
    /// a prompt is **queued** — mid-turn, on a row that is already `Active` —
    /// and fires nothing when the queue is later flushed (the turn ending, or
    /// the user cancelling it with Esc). All the flushed turn writes is the
    /// session file, and at that boundary it writes `idle` and then `busy`
    /// ~20ms apart. Mirroring only the first left the row `Idle` for the whole
    /// queued turn — the reported bug — because the `busy` that corrects it
    /// arrives as its own wake, and nothing else speaks until that turn's own
    /// `Stop`.
    #[test]
    fn a_queued_turn_recovers_from_the_boundary_idle_blip() {
        use AgentActivity as A;
        use SessionStatus as S;

        // Turn 1 ends: the `Stop` hook has already settled the row, and the
        // file's `idle` write agrees.
        let mut status = S::Active;
        if let Some(next) = reconcile_activity(&status, Some(A::Idle)) {
            status = next;
        }
        assert_eq!(status, S::Idle);
        // ~20ms later the dequeued turn's `busy` write lands. This is the only
        // notice it gives.
        if let Some(next) = reconcile_activity(&status, Some(A::Working)) {
            status = next;
        }
        assert_eq!(status, S::Active);
        // Its own end still settles normally.
        assert_eq!(reconcile_activity(&status, Some(A::Idle)), Some(S::Idle));
    }

    /// The recovery path: a background shell ended while the agent went back to
    /// work, so the row's background status is disproved by the tree and the
    /// session file says which rest-or-work shape replaces it.
    #[test]
    fn a_disproved_background_status_promotes_to_active() {
        let mut s = state_with(SessionStatus::BackgroundActive);
        assert!(promote_stale_background(
            &mut s,
            Some(&[]),
            Some(AgentActivity::Working)
        ));
        assert_eq!(s.status, SessionStatus::Active);
    }

    /// The half that protects the corroboration rule. `bg_shells` returns `None`
    /// when the process tree could not be read at all — promoting on that would
    /// be acting on no evidence, which is precisely the spurious bounce into
    /// `Active` the surrounding reconciliation is written to prevent. This is
    /// the assertion that is easy to leave out and expensive to lose.
    #[test]
    fn an_unreadable_process_tree_never_promotes() {
        let mut s = state_with(SessionStatus::BackgroundActive);
        assert!(!promote_stale_background(
            &mut s,
            None,
            Some(AgentActivity::Working)
        ));
        assert_eq!(s.status, SessionStatus::BackgroundActive);
    }

    #[test]
    fn promotion_needs_a_working_agent_and_a_background_row() {
        // A shell is still running → the status is not stale; classification,
        // not promotion, owns this row.
        let mut s = state_with(SessionStatus::BackgroundActive);
        let running = [shell("r3 watch review_abc")];
        assert!(!promote_stale_background(
            &mut s,
            Some(&running),
            Some(AgentActivity::Working)
        ));
        assert_eq!(s.status, SessionStatus::BackgroundActive);

        // Tree is empty, but the agent is at rest or the session file wasn't
        // re-read this wake (`None`) — the mirror path owns the exit to
        // `Idle`; promotion must not invent work.
        for activity in [None, Some(AgentActivity::Idle)] {
            let mut s = state_with(SessionStatus::BackgroundActive);
            assert!(!promote_stale_background(&mut s, Some(&[]), activity));
            assert_eq!(s.status, SessionStatus::BackgroundActive);
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

    /// The row follows the transcript's directory — a worktree entry moves it,
    /// last-wins — while a fold with no opinion leaves the launch directory
    /// alone, and one naming the same directory by another spelling (the
    /// physical path behind a symlinked launch) keeps the launch's.
    #[test]
    fn a_cwd_fold_moves_the_row_only_to_a_different_directory() {
        let mut state = state_with(SessionStatus::Idle);
        state.cwd = "/home/miao/proj".to_string();
        assert!(!apply_transcript_data(
            &mut state,
            &TranscriptStats::default()
        ));
        assert_eq!(state.cwd, "/home/miao/proj");

        let moved = TranscriptStats {
            cwd: Some("/home/miao/proj/.claude/worktrees/fox".to_string()),
            ..TranscriptStats::default()
        };
        assert!(apply_transcript_data(&mut state, &moved));
        assert_eq!(state.cwd, "/home/miao/proj/.claude/worktrees/fox");
        assert!(!apply_transcript_data(&mut state, &moved));

        let base = std::env::temp_dir().join(format!("cm-cwd-alias-{}", std::process::id()));
        let real = base.join("real");
        let link = base.join("link");
        std::fs::create_dir_all(&real).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        state.cwd = link.to_string_lossy().into_owned();
        let physical = TranscriptStats {
            cwd: Some(
                std::fs::canonicalize(&real)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..TranscriptStats::default()
        };
        assert!(!apply_transcript_data(&mut state, &physical));
        assert_eq!(state.cwd, link.to_string_lossy());
        let _ = std::fs::remove_dir_all(&base);
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

    #[tokio::test]
    async fn antigravity_prompt_appends_update_working_and_waiting_rows() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        let mut data = None;
        let mut offset = 0;
        let mut held = false;
        let mut state = LauncherState::for_test(AgentKind::Antigravity, SessionStatus::Active);
        for (status, prompt) in [
            (SessionStatus::Active, "first"),
            (SessionStatus::WaitingForApproval, "next"),
        ] {
            state.status = status.clone();
            writeln!(file, "{}", serde_json::json!({"type":"USER_INPUT", "content":format!("<USER_REQUEST>{prompt}</USER_REQUEST>")})).unwrap();
            assert!(
                rescan_transcript(
                    AgentKind::Antigravity,
                    &path,
                    &mut offset,
                    &mut data,
                    &mut held,
                    &mut state
                )
                .await
            );
            assert_eq!(state.last_prompt.as_deref(), Some(prompt));
            assert_eq!(state.first_prompt.as_deref(), Some("first"));
            assert_eq!(state.status, status, "prompt reads do not change activity");
        }
    }

    /// The settings file carries every backend's format, so it must be
    /// written through verbatim. Guards against a JSON round-trip here
    /// breaking non-JSON backends' launches.
    #[test]
    fn settings_transport_carries_every_backend_payload_verbatim() {
        let dir =
            std::env::temp_dir().join(format!("cm-settings-transport-{}", std::process::id()));
        state::create_dir_all_private(&dir).unwrap();
        let sock = dir.join("launcher.sock");
        for &agent in AgentKind::ALL {
            let payload = agent.hooks_settings_json(&sock.to_string_lossy());
            let path = dir.join("settings.json");
            write_hook_settings(agent, &sock, &path)
                .unwrap_or_else(|e| panic!("{agent:?}'s settings must be written: {e}"));
            let back = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                back, payload,
                "{agent:?}'s payload must survive the transport"
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "{agent:?}'s settings must stay private"
            );
            // The payload must stay readable by the consumer the launcher's
            // `build_launch_command` hands it to.
            match agent {
                AgentKind::Claude
                | AgentKind::Codex
                | AgentKind::Reasonix
                | AgentKind::Grok
                | AgentKind::Antigravity => {
                    serde_json::from_str::<serde_json::Value>(&back)
                        .unwrap_or_else(|e| panic!("{agent:?}'s payload must be JSON: {e}"));
                }
                AgentKind::Kimi => {
                    back.parse::<toml::Table>()
                        .unwrap_or_else(|e| panic!("Kimi's payload must be TOML: {e}"));
                }
                AgentKind::OpenCode | AgentKind::Pi | AgentKind::Omp => {
                    assert!(
                        !back.is_empty(),
                        "{agent:?}'s script payload must not be empty"
                    );
                }
                AgentKind::Unknown => {
                    assert!(back.is_empty(), "Unknown carries no payload");
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
