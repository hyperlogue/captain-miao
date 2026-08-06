use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use notify::{RecursiveMode, Watcher};
use ratatui::DefaultTerminal;
use std::time::{Duration, Instant};

use crate::agent::{AgentControl, ResumeCandidate};
use crate::backend::{LaunchPlan, OpenSpec};
use crate::config;
use crate::state::{self, HostId};
use crate::terminal::{
    self, Capabilities, SessionsLayout, SpawnCommand, SpawnSpec, SpawnTarget, WindowId,
};

use std::collections::HashSet;

use super::{Action, App, BrowserEntry, RestartSpec, SessionSnapshotEntry};

/// Lines of terminal output captured for the preview panel — its vertical
/// scroll-up depth. The draw side (`draw_preview`) parses/scrolls within this.
const PREVIEW_CAPTURE_LINES: usize = 2000;

/// Floor between remote-detach-prune terminal snapshots. Detach detection is
/// not latency-critical, but on the zellij backend a `snapshot()` is
/// `list-panes` (~20ms/pane, ~475ms at 22 panes), so while a remote attach
/// binding exists it must not force one on *every* debounced reload as busy
/// sessions churn their state files. The prune runs at most once per interval
/// — but a tab-cache refresh that needs the snapshot anyway may still prune off
/// the same data, so the throttle never starves the tab cache (see the reload
/// loop's snapshot gate).
const DETACH_PRUNE_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// Whether the detach prune's floor (`DETACH_PRUNE_MIN_INTERVAL`) has elapsed
/// since its last snapshot. `None` (never pruned) is always due. Pure so the
/// throttle is unit-tested without a wall clock.
fn detach_prune_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= DETACH_PRUNE_MIN_INTERVAL)
}

/// Union every backend's resumable list into one host-tagged, most-recent-first,
/// capped list, collecting per-host errors. Each backend walks transcript dirs
/// (local) or makes a blocking RPC (remote), so the whole aggregation runs inside
/// `block_in_place`. Shared by the resume picker and the browser's resumable rows.
fn list_resumable_all_hosts(
    app: &App,
    limit: usize,
) -> (Vec<(HostId, ResumeCandidate)>, Vec<String>) {
    tokio::task::block_in_place(|| {
        let mut all: Vec<(HostId, ResumeCandidate)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for backend in &app.backends {
            let host = backend.host_id();
            let (cands, errs) = backend.list_resumable(limit);
            all.extend(cands.into_iter().map(|c| (host.clone(), c)));
            errors.extend(errs);
        }
        // Most-recent first across all hosts, capped.
        all.sort_by_key(|(_, c)| std::cmp::Reverse(c.mtime));
        all.truncate(limit);
        (all, errors)
    })
}

// -- Dashboard singleton --

fn write_dashboard_pid_and_window() {
    let dir = state::state_dir();
    let _ = state::create_dir_all_private(&dir);
    let _ = std::fs::write(state::dashboard_pid_path(), std::process::id().to_string());
    if let Some(wid) = terminal::get().current_window() {
        // Prefix the dashboard's terminal-instance identity so the external
        // `focus` process only drives this window inside its own namespace
        // (Kitty window ids and zellij pane ids overlap).
        let identity = terminal::current_terminal_identity();
        let payload = state::format_dashboard_window_id(identity.as_deref(), &wid);
        let _ = std::fs::write(state::dashboard_window_id_path(), payload);
    }
}

/// Holds the dashboard's exclusive advisory lock for the whole process
/// lifetime. The `OwnedFd` must outlive `run()` — stored here so the kernel
/// keeps the `flock` held until the process exits (cleanly or via crash), at
/// which point it's auto-released. Dropping the fd would release the lock early.
static DASHBOARD_LOCK: std::sync::OnceLock<std::os::fd::OwnedFd> = std::sync::OnceLock::new();

/// Ensure this is the only dashboard by taking an exclusive advisory file lock
/// held for the process lifetime, instead of trusting the pid in
/// `dashboard.pid`. A bare pid check has a false-positive after a crash + OS
/// pid reuse: the stale file names a pid that now belongs to some unrelated
/// live process, so the launch wrongly bails "already running" and locks the
/// user out. `flock` has no such failure mode — the lock is released by the
/// kernel the instant the holding process dies, so a crash never strands it.
fn check_existing_dashboard() -> Result<()> {
    use std::os::fd::AsRawFd;
    // Lock a sibling file so the pid file stays a plain writable display value.
    let lock_path = state::dashboard_pid_path().with_extension("lock");
    let dir = state::state_dir();
    let _ = state::create_dir_all_private(&dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("Failed to open dashboard lock {}", lock_path.display()))?;
    let fd = std::os::fd::OwnedFd::from(file);
    // Non-blocking exclusive lock: EWOULDBLOCK/EAGAIN means another dashboard
    // holds it (genuinely running). Any other error is reported as-is.
    let rc = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // EWOULDBLOCK (== EAGAIN on most platforms) means another dashboard
        // holds the lock — i.e. genuinely running. Compare values rather than
        // match patterns so the two equal-on-some-platforms constants don't
        // trip an unreachable-pattern lint.
        let code = err.raw_os_error();
        if code == Some(libc::EWOULDBLOCK) || code == Some(libc::EAGAIN) {
            bail!("Dashboard already running");
        }
        return Err(err).context("Failed to lock dashboard");
    }
    // Hold the fd for the process lifetime so the lock stays taken.
    let _ = DASHBOARD_LOCK.set(fd);
    Ok(())
}

fn cleanup_dashboard() {
    let _ = std::fs::remove_file(state::dashboard_pid_path());
    // The lock fd is auto-released on exit; remove the file too on clean exit.
    let _ = std::fs::remove_file(state::dashboard_pid_path().with_extension("lock"));
    let _ = std::fs::remove_file(state::dashboard_window_id_path());
    let _ = std::fs::remove_file(state::dashboard_sessions_snapshot_path());
}

/// Pop the previous run's session snapshot off disk. Returns the entries
/// whose launcher pid is no longer alive — those are the sessions the
/// previous dashboard knew about that died with it.
fn take_missing_from_snapshot(alive_pids: &HashSet<u32>) -> Vec<RestartSpec> {
    let path = state::dashboard_sessions_snapshot_path();
    let prior: Option<Vec<SessionSnapshotEntry>> = state::read_json(&path);
    // Even on a malformed file, drop it so we don't keep prompting.
    let _ = std::fs::remove_file(&path);
    let Some(prior) = prior else {
        return Vec::new();
    };
    prior
        .into_iter()
        .filter(|e: &SessionSnapshotEntry| !alive_pids.contains(&e.launcher_pid))
        .map(|e| RestartSpec {
            agent: e.agent,
            child_pid: e.child_pid,
            window_id: e.window_id,
            cwd: e.cwd,
            session_id: e.session_id,
            flags: e.flags,
            // Crash recovery: the launcher_pid is already dead, so child_pid is
            // gone (and may be recycled) and window_id may collide with an
            // unrelated live window after a kitty relaunch. Never signal/close.
            kill_old: false,
        })
        .collect()
}

/// The dashboard's recorded window plus the terminal-instance identity it was
/// written in (`None` when the payload carries no identity). The `focus` command
/// compares that identity against its own before driving the window.
pub fn read_dashboard_window_id() -> Option<(Option<String>, WindowId)> {
    let raw = std::fs::read_to_string(state::dashboard_window_id_path()).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (identity, wid) = state::parse_dashboard_window_id(raw);
    (!wid.as_str().is_empty()).then_some((identity, wid))
}

/// How a clipboard copy actually landed, so the status line can be truthful
/// about it rather than optimistic.
enum CopyOutcome {
    /// Written via a system clipboard tool, which exited 0.
    Cli,
    /// No clipboard binary was found, so the OSC 52 escape was emitted as a
    /// best-effort fallback. The terminal sends no acknowledgement, so this
    /// means "the bytes went out", not "the terminal accepted them".
    Osc52Fallback,
}

/// Copy `text` to the system clipboard. Prefers a platform CLI (`pbcopy` on
/// macOS; `wl-copy`/`xclip`/`xsel` on Linux) because its exit code gives a
/// truthful success/failure signal and matches how the rest of the crate shells
/// out (`sleep.rs`, `kitten @`). Falls back to the OSC 52 terminal escape only
/// when no such binary is installed, so the copy still works in a terminal that
/// honours it (e.g. Kitty's default) without depending on one being present.
fn copy_to_clipboard(text: &str) -> std::io::Result<CopyOutcome> {
    // Preference order per platform. pbcopy is always present on macOS; Linux
    // varies by display server (Wayland: wl-copy; X11: xclip/xsel) and may have
    // none installed.
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };

    for &(bin, args) in candidates {
        match try_cli_copy(bin, args, text) {
            Some(Ok(())) => return Ok(CopyOutcome::Cli),
            // The tool ran but failed — surface it rather than masking the
            // problem behind the OSC 52 fallback.
            Some(Err(e)) => return Err(e),
            // Binary absent: try the next candidate.
            None => continue,
        }
    }

    emit_osc52(text)?;
    Ok(CopyOutcome::Osc52Fallback)
}

/// Run `bin args`, piping `text` to its stdin. Returns `None` if the binary
/// isn't available (so the caller tries the next candidate), `Some(Ok(()))` if
/// it ran and exited 0, or `Some(Err(_))` if it ran but failed.
fn try_cli_copy(bin: &str, args: &[&str], text: &str) -> Option<std::io::Result<()>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let spawned = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // Any spawn error (NotFound or otherwise) means "fall through to the next
    // candidate", not "fail the whole copy".
    let mut child = spawned.ok()?;
    // Write the payload, then drop the stdin handle so the tool sees EOF and
    // exits instead of blocking on more input.
    let write_res = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("clipboard tool stdin unavailable"))
        .and_then(|mut stdin| stdin.write_all(text.as_bytes()));
    if let Err(e) = write_res {
        let _ = child.wait();
        return Some(Err(e));
    }
    Some(match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(std::io::Error::other(format!("{bin} exited with {status}"))),
        Err(e) => Err(e),
    })
}

/// Emit the OSC 52 clipboard-write escape to stdout. The escape produces no
/// visible output, so writing it between the key event and the next ratatui
/// frame doesn't disturb the rendered screen.
fn emit_osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let encoded = super::format::base64_encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    // OSC 52: ESC ] 52 ; c ; <base64> BEL  — `c` targets the clipboard selection.
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()
}

struct LaunchCopy {
    progress: &'static str,  // "Launching"  / "Resuming"
    succeeded: &'static str, // "Launched"   / "Resumed"
    failed: &'static str,    // "Launch"     / "Resume"
    is_resume: bool,         // selects new_tab_title vs resume_tab_title from config
}

const LAUNCH_COPY_NEW: LaunchCopy = LaunchCopy {
    progress: "Launching",
    succeeded: "Launched",
    failed: "Launch",
    is_resume: false,
};

const LAUNCH_COPY_RESUME: LaunchCopy = LaunchCopy {
    progress: "Resuming",
    succeeded: "Resumed",
    failed: "Resume",
    is_resume: true,
};

const LAUNCH_COPY_RESTART: LaunchCopy = LaunchCopy {
    progress: "Restarting",
    succeeded: "Restarted",
    failed: "Restart",
    is_resume: true,
};

/// Where a session spawn lands, from the current [`SessionsLayout`] and the
/// backend's capabilities. This is a **spawn-time policy over the mode**, not the
/// selected window: the per-session anchor is gone (both Stacked arrangements put
/// every session in one shared tab, so there is nothing to anchor to).
///
/// - **Per-tab** ⇒ a fresh tab per session, on both backends.
/// - **Stacked** ⇒ the shared `miao:sessions` tab: floating panes on a backend that
///   floats sessions (zellij, `floating_sessions`), a single `miao:sessions`
///   stack-layout tab on one that stacks windows (Kitty, `window_stacking`). A
///   backend that does neither falls back to a fresh tab per session.
pub(super) fn resolve_spawn_target(caps: Capabilities, layout: SessionsLayout) -> SpawnTarget {
    match layout {
        SessionsLayout::PerTab => SpawnTarget::NewTab,
        SessionsLayout::Stacked => {
            if caps.floating_sessions {
                SpawnTarget::Floating
            } else if caps.window_stacking {
                SpawnTarget::SharedStackTab
            } else {
                SpawnTarget::NewTab
            }
        }
    }
}

/// Expand a session-tab title template: `{agent}` → the backend label,
/// `{basename}` → the cwd's last path component, `{cwd}` → the full path.
pub(super) fn expand_tab_title(template: &str, agent: AgentControl, cwd: &str) -> String {
    let basename = super::cwd_basename(cwd);
    template
        .replace("{agent}", agent.label())
        .replace("{basename}", basename)
        .replace("{cwd}", cwd)
}

async fn launch_agent(
    app: &mut App,
    agent: AgentControl,
    cwd: &str,
    resume: Option<(&str, bool)>,
    copy: &LaunchCopy,
    host: Option<&HostId>,
) {
    // Only a *local* launch's cwd belongs in the local recent list; a remote
    // launch records into the remote host's list server-side (see
    // `server_pool::open_in_pool`), so a mac path never pollutes it and vice versa.
    if host.is_none_or(|h| h.is_local()) {
        app.push_recent_cwd(cwd);
    }
    app.set_status(
        format!("{} in {}", copy.progress, app.shorten_path(cwd)),
        false,
    );

    // Ask the backend how to open the session. `host` selects it: a remote host
    // RPCs its server to start the launcher in the pty pool and returns an
    // `AttachRemote` plan (an `ssh … attach` window); the default `None` is the
    // local backend, whose plan is pure metadata — the argv for a Kitty window
    // that *is* the launcher. The remote RPC blocks, so run it off the worker;
    // bind the plan before matching so the backend borrow ends before we touch
    // `app` again. (Today's callers all pass `None`; remote routing comes from
    // the 3d browser.)
    let open_spec = OpenSpec {
        agent,
        cwd: cwd.to_string(),
        resume: resume.map(|(id, fork)| (id.to_string(), fork)),
    };
    let plan = {
        let backend = match host {
            Some(h) => app.backend_for(h),
            None => app.local_backend(),
        };
        tokio::task::block_in_place(|| backend.open_session(&open_spec))
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(e) => {
            app.set_status(format!("{} failed: {e}", copy.failed), true);
            return;
        }
    };
    let mut argv = plan.argv().to_vec();
    // The (host, token) the appearing row will carry home, so we can bind the
    // window we're about to open to it (§15.2). A remote attach reuses the
    // server-minted pool session name; a local spawn gets a fresh
    // dashboard-minted `launch_id` threaded onto the launcher as `--launch-id`.
    let (bind_host, bind_token) = match &plan {
        LaunchPlan::AttachRemote { session_name, .. } => (
            host.cloned().unwrap_or_else(HostId::local),
            session_name.clone(),
        ),
        LaunchPlan::SpawnLocal { .. } => {
            let launch_id = app.mint_launch_id();
            argv.push("--launch-id".to_string());
            argv.push(launch_id.clone());
            (HostId::local(), launch_id)
        }
    };

    let launcher_cfg = &config::get().launcher;
    // The configured titles are templates ({agent}/{basename}/{cwd}), so the
    // tab names the session's project instead of a generic "Claude (new)".
    let template = if copy.is_resume {
        &launcher_cfg.resume_tab_title
    } else {
        &launcher_cfg.new_tab_title
    };
    let tab_title = expand_tab_title(template, agent, cwd);
    let target = resolve_spawn_target(app.capabilities, app.sessions_layout);
    // The expanded title names a NewTab spawn's tab, a Floating spawn's pane, or
    // a SharedStackTab window (whose shared tab stays fixed-titled `miao:sessions`).
    let wants_title = matches!(
        target,
        SpawnTarget::NewTab | SpawnTarget::Floating | SpawnTarget::SharedStackTab
    );
    let spec = SpawnSpec {
        cwd: cwd.to_string(),
        target,
        command: SpawnCommand::Exec(argv),
        title: wants_title.then_some(tab_title),
        hold: true,
        take_focus: false,
        // Kitty reads the flag on a NewTab / SharedStackTab spawn (`goto-layout
        // stack` on the tab it creates). zellij session spawns are Floating and
        // ignore it.
        stack: true,
    };
    match terminal::get().spawn(spec).await {
        Ok(result) => {
            // A kitty/Floating spawn always recovers its window; a `None` here
            // would mean the backend created the target but lost the id, which
            // this path can't bind — surface it rather than proceed.
            let Some(id) = result.window else {
                app.set_status(format!("{} failed: no window id", copy.failed), true);
                return;
            };
            app.set_status(format!("{} (window {id})", copy.succeeded), false);
            // Bind the window to the session's token so the dashboard resolves it
            // (preview / focus / move-to-tab) and prunes it when the window dies —
            // local and remote uniformly (§8, §15).
            app.record_window_binding(bind_host, bind_token, id.clone());
            // Seed the display-only window→tab cache from the spawn itself when
            // the backend reported the tab (zellij does; kitty's `launch` prints
            // only a window id). Otherwise the next reload sees an unresolved
            // local window and pays a full `snapshot()` for a fact we just
            // learned for free — on zellij that's a `list-panes`, ~20ms per pane.
            // A snapshot still overwrites the whole map when one does run, so a
            // seeded entry can't outlive the truth.
            if let Some(tab) = result.tab {
                app.window_tab_cache.insert(id.clone(), tab);
            }
            app.pending_focus_window = Some((id, Instant::now()));
        }
        Err(e) => app.set_status(format!("{} failed: {e}", copy.failed), true),
    }
}

/// How long an action that races a launcher's own state-file write (kill,
/// detach, restart) waits before the forced reload: enough for the launcher to
/// die (a closed window SIGHUPs it, which can beat its own state-file cleanup)
/// or to rewrite its state.
const ACTION_SETTLE: Duration = Duration::from_millis(200);

/// Arm the post-action reload deadline the main loop drains: on expiry it sets
/// `fs_dirty` and clears `last_reload`, so the dead/changed row is picked up
/// promptly (bypassing the debounce) instead of lingering until some unrelated
/// fs event fires.
///
/// Deliberately a deadline, not a sleep: an action handler runs *inline* in the
/// loop, so `await`ing the settle inside it froze the frame for the whole 200ms
/// — the loop can't draw until the handler returns. Arming instead lets the
/// spawned window and the status line paint immediately.
fn arm_settle_reload(settle_reload_at: &mut Option<Instant>) {
    *settle_reload_at = Some(Instant::now() + ACTION_SETTLE);
}

/// Replace one running session with a fresh launcher resumed at the same
/// transcript, spawned into the current [`SessionsLayout`] (the shared
/// `miao:sessions` tab in Stacked, its own tab in Per-tab) — this is how a layout
/// switch migrates an existing session. Returns true on a successful relaunch.
///
/// Order still matters on the reuse path: launch the replacement first, then
/// close the old window, so a Stacked restart doesn't momentarily empty (and
/// destroy) the shared `miao:sessions` tab between the close and the respawn.
///
/// `spec.kill_old` gates the SIGTERM + close_window post-step. It's true for
/// user-initiated restarts (the session is live, so the old child must be torn
/// down) and false for crash-recovery specs: there the launcher pid is already
/// dead, so the child pid is gone (and possibly reused by an unrelated process)
/// and the recorded window id may collide with an innocent live window after a
/// relaunch — signaling/closing either would hit the wrong target. We also
/// re-check `child_pid` liveness before signaling even on the kill path.
///
/// The close is *unconditional* on the kill path — best-effort, since closing an
/// already-gone window is a no-op (zellij) or an ignored error (kitty) — rather
/// than gated on a terminal snapshot. That snapshot cost a `list-panes`, ~20ms
/// **per pane** on zellij (~780ms in a 30-pane session), and bought nothing: an
/// id merely being present proves *some* window has it, which is exactly what a
/// recycled id would look like too, so it never guarded the collision its gate
/// was named for. The real guard is `kill_old`, already false on every path
/// where recycling is possible.
async fn restart_one(app: &mut App, spec: RestartSpec) -> bool {
    let agent = spec.agent;
    let session_id = spec.session_id;
    let cwd = spec.cwd;
    let window_id = spec.window_id;
    let child_pid = spec.child_pid;
    let flags = spec.flags;
    let kill_old = spec.kill_old;

    // The replacement's placement comes from the layout policy, not the old
    // window, so no anchor is threaded.
    launch_agent(
        app,
        agent,
        &cwd,
        Some((session_id.as_str(), false)),
        &LAUNCH_COPY_RESTART,
        None,
    )
    .await;
    // Detect launch failure: launch_agent flips `status_is_error` to true on
    // failure. If it failed, leave the old session running rather than killing
    // it — half-restarted is worse than not restarted.
    if app.status_is_error {
        return false;
    }

    // Carry the old session's status flags onto the relaunched window so the
    // restart preserves pinned / muted / follow-up. `launch_agent` set
    // `pending_focus_window` to the new window id on success; the actual flag
    // copy happens in `reload_sessions` once the new launcher appears.
    if !flags.is_default()
        && let Some((new_wid, _)) = app.pending_focus_window.clone()
    {
        app.pending_flag_restores.insert(new_wid, flags);
    }

    if kill_old {
        // Live session we own: tear the old child + window down. Guard against
        // a recycled pid even here — the child should still be alive for a user
        // restart, but a liveness check costs nothing and avoids signaling a pid
        // that exited between the row's last reload and now. The window close is
        // best-effort; an already-closed window just errors.
        if state::is_process_alive(child_pid) {
            let _ = app.local_backend().kill_session(child_pid);
        }
        let _ = terminal::get().close_window(&window_id).await;
    }
    // A crash-recovery window is left untouched: the old child is dead and may
    // be recycled, and the window id may belong to an innocent live window.
    true
}

// -- Terminal modes --

/// Enable the terminal modes the dashboard relies on, beyond what
/// `ratatui::init` sets up (raw mode + alt screen): mouse capture, focus
/// reporting, and the kitty keyboard protocol. Returns whether the keyboard
/// enhancement push succeeded, so the caller can pop it symmetrically.
///
/// Armed once at startup and popped once at exit: the terminal stays in these
/// modes (and ratatui stays up) across window focuses and tab switches. They
/// only affect the dashboard's own window, so driving the terminal backend
/// never requires dropping them.
fn enter_terminal_modes() -> bool {
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    // Focus reporting (CSI ?1004h): Kitty sends FocusGained when the user
    // switches to the dashboard's window/tab, which we use to auto-refresh
    // the preview so it reflects the latest output of whatever the user
    // glanced away from. Terminals that don't support it ignore the request.
    let _ = execute!(std::io::stdout(), EnableFocusChange);
    // Opt into the kitty keyboard protocol (DISAMBIGUATE_ESCAPE_CODES) so we
    // receive distinct events for Ctrl+<digit>, which in the legacy encoding
    // either collide with other control codes (Ctrl+3 ≡ Esc) or aren't sent
    // at all (Ctrl+1, Ctrl+9). Terminals that don't support it ignore the
    // request; we pop it on exit.
    execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
}

/// Disable the modes `enter_terminal_modes` enabled, in mirror order. Pass the
/// `kb_enhanced` flag it returned so the keyboard push is popped only when it
/// took.
fn leave_terminal_modes(kb_enhanced: bool) {
    if kb_enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(std::io::stdout(), DisableFocusChange);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
}

// -- Entry point --

pub async fn run() -> Result<()> {
    check_existing_dashboard()?;

    let sessions_dir = state::ensure_sessions_dir()?;
    write_dashboard_pid_and_window();

    crate::init_tracing("dashboard");
    super::keybind_log::init();

    // Watch sessions directory for state file changes
    let (fs_tx, fs_rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::RecommendedWatcher::new(fs_tx, notify::Config::default())
        .context("Failed to create file watcher")?;
    watcher.watch(&sessions_dir, RecursiveMode::NonRecursive)?;

    // Each backend nominates the paths whose changes should trigger a reload:
    // Claude's flat session-name-store dir, Codex's title-store WAL file (the
    // wake for `LocalBackend`'s throttled title overlay — a rename touches only
    // that sqlite). Non-recursive either way; transcript-derived updates arrive
    // via the launcher's state file (the `sessions_dir` watch above) rather
    // than a transcript-dir watch. Best-effort: a missing path (e.g. a
    // checkpointed-away wal) just isn't watched — the overlay refreshes on the
    // next session event instead.
    for &agent in AgentControl::ALL {
        for path in agent.watch_paths() {
            let _ = watcher.watch(&path, RecursiveMode::NonRecursive);
        }
    }

    let mut terminal = ratatui::init();
    // Advertise our tab/window title; kitty's default tab_title_template
    // surfaces it as the tab label. User templates may ignore it.
    let _ = execute!(std::io::stdout(), SetTitle("miao"));
    // Probe the terminal palette for the paw's status tints and the cat's colours
    // now: raw mode is on (ratatui::init) so the OSC-4 reply isn't line-buffered,
    // but mouse/focus reporting and the event loop haven't started reading stdin
    // yet, so the reply can't be mistaken for input. Cached for `App::new`.
    super::logo::probe_logo_colors();
    let kb_enhanced = enter_terminal_modes();

    // ratatui::init's panic hook only disables raw mode and leaves the alt
    // screen — it doesn't know about the modes we just enabled above. Chain
    // a hook that pops them first, then delegates. Without this, a crash
    // leaves focus reporting / mouse capture / kitty keyboard stuck on, and
    // the shell starts receiving focus-out events as ^[[O after exit.
    // (Each `ratatui::init()` restacks its own panic hook on top; the leaked
    // extra hooks are harmless — they only run on a panic, which exits anyway.)
    let prior_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave_terminal_modes(kb_enhanced);
        prior_hook(info);
    }));

    // Teardown must run even on an error path.
    let result = run_app(&mut terminal, fs_rx).await;
    leave_terminal_modes(kb_enhanced);
    ratatui::restore();

    drop(watcher);
    cleanup_dashboard();
    result
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    fs_rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
) -> Result<()> {
    let mut app = App::new();
    // Recover window bindings a previous dashboard left behind so live sessions
    // resolve their windows across a restart (§15.7). Before the first reload, so
    // that reload's resolves (preview select, tab fill, snapshot) see them.
    app.seed_window_bindings_from_disk();
    app.reload_sessions();
    app.load_overrides();
    app.load_recent_cwds();
    app.load_directory_marks();
    app.load_work_tabs();
    // Drain bell sentinels accumulated while the dashboard wasn't running.
    app.apply_bell_signals(state::drain_bell_flag_pids());
    // Don't auto-focus pre-existing failed-launch rows at startup — yanking
    // focus the instant the dashboard opens would be jarring. The initial
    // `reload_sessions` above already queued any, so clear it; focusing only
    // happens for failures that occur while the dashboard is live (the loop).
    app.failed_launch_focus_queue.clear();
    // Unlike failed-launch focus, orphaned held panes ARE reaped at startup: the
    // binding seed above queued each dead-local-pid binding's window (a previous
    // dashboard's crashed launcher), and a leaked pane should go regardless of
    // when it leaked. Floating-sessions backend only (the seed gates it).
    for wid in std::mem::take(&mut app.reap_window_queue) {
        if let Err(e) = terminal::get().close_window(&wid).await {
            tracing::debug!("startup reap of orphaned pane {wid:?} failed: {e}");
        }
    }

    // Crash recovery: if the previous dashboard left a session snapshot on
    // disk, every entry whose launcher pid is no longer alive is a session
    // that died with it. Offer to restart them in one shot. Always start
    // fresh from this point — `save_session_snapshot` below replaces the file.
    // The snapshot is local-only (crash recovery relaunches via the local spawn
    // path); build the alive set from local pids only, matching, so a remote
    // launcher pid can't numerically alias a dead local one and mask it.
    let alive_pids: HashSet<u32> = app
        .sessions
        .iter()
        .filter(|s| s.host.is_local())
        .map(|s| s.launcher_pid)
        .collect();
    let missing = take_missing_from_snapshot(&alive_pids);
    app.prompt_restart_missing(missing);
    app.save_session_snapshot();

    let polling = &config::get().polling;
    let reload_min_interval = Duration::from_millis(polling.fs_reload_debounce_ms);
    let preview_debounce = Duration::from_millis(polling.preview_debounce_ms);
    let event_poll = Duration::from_millis(polling.event_poll_ms);
    let preview_auto_refresh = Duration::from_secs(polling.preview_auto_refresh_secs);

    let mut fs_dirty = false;
    let mut last_reload: Option<Instant> = None;
    let mut last_detach_prune: Option<Instant> = None;
    let mut needs_redraw = true;
    let mut last_age_label: Option<String> = None;
    // Deadline armed by an action that races a launcher's state-file write
    // (`arm_settle_reload`); drained at the top of the loop so the action itself
    // never blocks a frame on it.
    let mut settle_reload_at: Option<Instant> = None;
    loop {
        // A settle deadline that has come due forces the next reload: re-arm
        // `fs_dirty` and clear `last_reload` so the debounce doesn't defer it.
        if settle_reload_at.is_some_and(|t| Instant::now() >= t) {
            settle_reload_at = None;
            fs_dirty = true;
            last_reload = None;
        }
        // Drain filesystem events and coalesce. A busy session rewrites its
        // state file on every hook/status change, and each write wakes the
        // (non-recursive) sessions-dir watch, so without debouncing a burst can
        // queue reloads faster than we can service them, lagging the UI by
        // seconds. Only reload once per RELOAD_MIN_INTERVAL regardless of how
        // many events arrived.
        while fs_rx.try_recv().is_ok() {
            fs_dirty = true;
        }
        // Remote backends update their in-memory mirror off-thread (a pushed
        // Snapshot/Delta/Removed, or a connect/disconnect) — no filesystem event
        // fires, so `fs_rx` alone would leave a new remote session invisible
        // until some *local* event happened to trigger a reload. Poll each
        // backend's change signal (cleared on read) each loop iteration (~every
        // `event_poll_ms`) and fold it into the same debounced reload path.
        for backend in &app.backends {
            if backend.take_dirty() {
                fs_dirty = true;
            }
        }
        if fs_dirty && last_reload.is_none_or(|t| t.elapsed() >= reload_min_interval) {
            fs_dirty = false;
            last_reload = Some(Instant::now());
            // `reload_sessions` walks the sessions dir and reads each backend's
            // transcripts synchronously. `block_in_place` hands the current
            // worker thread to those blocking reads while letting the
            // multi-threaded runtime keep servicing other tasks, so the loop's
            // own awaits (kitty rc, title pulls) aren't starved during a large
            // reload.
            tokio::task::block_in_place(|| app.reload_sessions());
            // Drain bell sentinels written by `miao focus --window-id`
            // *after* reload_sessions so we know which pids are alive.
            app.apply_bell_signals(state::drain_bell_flag_pids());
            // Bring just-failed launch windows (direnv blocked, missing agent) to
            // the foreground. `reload_sessions` queued them on the transition
            // into `FailedToStart`; the launcher can't focus its own window (it
            // may be headless/remote), so the dashboard does it here.
            for wid in std::mem::take(&mut app.failed_launch_focus_queue) {
                let _ = terminal::get().focus_window(&wid).await;
            }
            // Close held panes orphaned by rows that just departed without a clean
            // kill (crash / SIGKILL / state-file gone) — and dead remote-attach
            // panes. `reload_sessions` queues them on row removal, gated on the
            // `floating_sessions` capability (zellij): the held exited pane is an
            // invisible leak buried in the shared sessions tab, inflating every
            // `list-panes`. Best-effort — the pane may already be gone.
            for wid in std::mem::take(&mut app.reap_window_queue) {
                if let Err(e) = terminal::get().close_window(&wid).await {
                    tracing::debug!("reap of departed session pane {wid:?} failed: {e}");
                }
            }
            // Both consumers below want a terminal snapshot: the tab-cache
            // refresh (a new/moved local window is unresolved) and the remote
            // detach prune (a live remote attachment whose window may have died).
            // Fetch it at most once so a reload needing both pays a single
            // `kitten @ ls`. The detach prune is gated on holding a *remote*
            // binding — a local `launch_id` binding GCs via the row's own state
            // file, so it must not drive a per-reload snapshot — and further
            // floored to `DETACH_PRUNE_MIN_INTERVAL` so a snapshot-cheap backend
            // aside, a busy zellij tree doesn't pay `list-panes` on every
            // debounced reload. The tab cache is never floored: if it needs the
            // snapshot this pass, the prune rides along on the same data.
            let need_tab_cache = !app.unresolved_local_tab_windows().is_empty();
            let detach_prune = app.window_bindings.has_remote()
                && (need_tab_cache || detach_prune_due(last_detach_prune, Instant::now()));
            let tabs = if need_tab_cache || detach_prune {
                terminal::get().snapshot().await.ok()
            } else {
                None
            };
            if need_tab_cache && let Some(tabs) = &tabs {
                app.refresh_tab_cache(tabs);
            }
            app.fill_tab_ids_from_cache();
            // Refresh the snapshot so a crash from this point sees the
            // current session set, not a stale older one.
            app.save_session_snapshot();
            // Detach detection (§5): if a remote attach window died (laptop
            // slept, ssh dropped), drop its binding so the row leaves cleanly.
            if detach_prune {
                last_detach_prune = Some(Instant::now());
                let live: HashSet<WindowId> = tabs
                    .iter()
                    .flatten()
                    .flat_map(|t| &t.windows)
                    .cloned()
                    .collect();
                app.prune_detached_sessions(&live);
            }
            needs_redraw = true;
        }

        // Preview debounce: mark dirty when selection differs from cached preview,
        // then fetch after a short settle period so rapid navigation doesn't block.
        let selected_wid = app.selected_window_id();
        if selected_wid != app.preview_window_id {
            // Clearing on the transition only — subsequent loop iterations
            // during the 200ms debounce window would otherwise redraw
            // continuously without any new content.
            if app.set_preview_text(None) {
                needs_redraw = true;
            }
            app.preview_scroll = 0;
            app.preview_h_scroll = 0;
            if app.preview_dirty_since.is_none() {
                app.preview_dirty_since = Some(Instant::now());
            }
        }
        // Periodic auto-refresh: while the dashboard has terminal focus and
        // the user is following the live tail, re-arm the debounced fetch
        // once per interval so the preview tracks the selected session's
        // output without manual `R` presses.
        if app.wants_preview_auto_refresh(preview_auto_refresh) {
            app.request_preview_refresh();
        }
        if let Some(dirty_at) = app.preview_dirty_since
            && dirty_at.elapsed() >= preview_debounce
        {
            app.preview_dirty_since = None;
            if let Some(wid) = selected_wid {
                app.preview_fetched_at = Some(Instant::now());
                match terminal::get()
                    .capture_text(&wid, PREVIEW_CAPTURE_LINES)
                    .await
                {
                    Ok(text) => {
                        app.set_preview_text(Some(text));
                        app.preview_window_id = Some(wid);
                        app.preview_scroll = 0;
                        app.preview_h_scroll = 0;
                    }
                    // Record the attempted window id even on failure so the
                    // selection-mismatch check above doesn't re-arm
                    // `preview_dirty_since` every loop iteration — which would
                    // re-spawn a `kitten get-text` subprocess every ~200ms for
                    // as long as a dead window stays selected. Clear any stale
                    // text so nothing outdated is shown for the dead window.
                    Err(_) => {
                        app.set_preview_text(None);
                        app.preview_window_id = Some(wid);
                    }
                }
            } else {
                app.preview_window_id = None;
                app.set_preview_text(None);
            }
            needs_redraw = true;
        }

        // Redraw when the preview staleness label changes (at most once a
        // minute at its resolution). Nothing else triggers a draw on an
        // otherwise idle dashboard, so the age would freeze on screen.
        let age_label = app.preview_age_label();
        if age_label != last_age_label {
            last_age_label = age_label;
            needs_redraw = true;
        }

        // A walking cat is client-driven (kitty can't move a placement on its
        // own), so keep the frame ticking — each redraw calls render_logo_graphics,
        // which advances the cat — until it leaves the row and clears the flag.
        if app.cat_walking() {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| app.draw(frame))?;
            // Place/recolour the paw and fire a pending click pulse, after ratatui
            // has flushed the frame so there's no write interleaving. No-op when
            // the terminal can't do graphics. The pulse itself is played by kitty
            // autonomously, so the loop needn't drive frames; the cat walk is the
            // one thing this repaint drives (an advancing placement).
            app.render_logo_graphics();
            needs_redraw = false;
        }

        // Tick fast while a cat walks so its motion stays smooth; otherwise idle at
        // the configured poll interval, waking only on input/timers.
        let mut poll_timeout = if app.cat_walking() {
            event_poll.min(Duration::from_millis(30))
        } else {
            event_poll
        };
        // Never sleep past an armed settle deadline, or the post-action reload
        // would land up to a poll interval late (100ms by default) on top of the
        // settle itself. A zero timeout just makes `poll` a non-blocking check.
        if let Some(t) = settle_reload_at {
            poll_timeout = poll_timeout.min(t.saturating_duration_since(Instant::now()));
        }
        if event::poll(poll_timeout)? {
            let evt = event::read()?;
            // Any input event can mutate state (selection, status, input mode,
            // scroll offsets, etc.), so redraw on the next iteration — except a
            // bare mouse-motion event, which `handle_mouse` ignores entirely.
            // `EnableMouseCapture` turns on any-motion tracking, so the pointer
            // crossing the dashboard streams `Moved` events; repainting a full
            // frame for each is pure waste. A split-resize `Drag` still repaints,
            // and so does any motion seen while a drag is in progress.
            if !matches!(&evt, Event::Mouse(m)
                if matches!(m.kind, MouseEventKind::Moved) && app.drag.is_none())
            {
                needs_redraw = true;
            }
            let maybe_action = match evt {
                Event::Key(key) => {
                    // Capture the mode *before* dispatch — handlers can
                    // change input_mode (e.g. `/` -> Search), and we want
                    // to attribute the keystroke to the mode it was pressed
                    // in for frequency analysis.
                    let mode_before = app.input_mode;
                    let action = app.handle_key(key);
                    super::keybind_log::record(mode_before, key, action.as_ref());
                    action
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::FocusGained => {
                    app.focused = true;
                    app.request_preview_refresh();
                    None
                }
                Event::FocusLost => {
                    app.focused = false;
                    None
                }
                Event::Resize(_, _) => {
                    // Refresh the cell size (a kitty font-zoom, Ctrl+Shift+=, alters
                    // it — it feeds the cat's sub-cell offset) and force one cheap
                    // re-place in case the resize cleared the placement. The uploaded
                    // images are a fixed-size mask, independent of terminal geometry,
                    // so a resize *never* needs a recompose: kitty rescales the
                    // source into the cell box on its own. Skipping it avoids both the
                    // ~1.4 MB re-upload storm during a drag-resize and the one-frame
                    // paw blink each recompose caused, and lets a running pulse/walk
                    // ride through the resize. (Graphics capability is fixed for the
                    // process on kitty, so this never needs to compose/tear down.)
                    app.logo_caps = terminal::graphics::capability();
                    app.logo_placed_color = None;
                    None
                }
                _ => None,
            };
            if let Some(action) = maybe_action {
                // Paint the keystroke's own effect (the confirm prompt closing,
                // the picker dismissing) *before* running the action: an action
                // handler runs inline in this loop and shells out to the terminal
                // backend, so anything it leaves undrawn reads as a frozen UI for
                // the whole round-trip. `needs_redraw` deliberately stays set —
                // the action's own status line and row changes still get the
                // regular frame at the top of the next iteration.
                terminal.draw(|frame| app.draw(frame))?;
                app.render_logo_graphics();
                match action {
                    Action::FocusWindow(id) => {
                        // The TUI stays up: tearing ratatui down around the
                        // focus (as an early build did) drops the dashboard
                        // pane out of the alt screen for the whole call — on
                        // zellij a visible flash of the underlying shell
                        // before the client switches tabs.
                        if let Err(e) = terminal::get().focus_window(&id).await {
                            app.set_status(format!("Focus failed: {e}"), true);
                        }
                    }
                    Action::NewSessionSplit { agent, cwd, host } => {
                        launch_agent(&mut app, agent, &cwd, None, &LAUNCH_COPY_NEW, Some(&host))
                            .await;
                    }
                    Action::FetchTabsForMove(window_id) => match terminal::get().snapshot().await {
                        Ok(tabs) => app.open_move_tab_picker(window_id, terminal::list_tabs(&tabs)),
                        Err(e) => app.set_status(format!("Failed to list tabs: {e}"), true),
                    },
                    Action::MoveWindow(wid, target) => {
                        let result = terminal::get().move_window_to_tab(&wid, target).await;
                        if let Some(dash_wid) = terminal::get().current_window() {
                            let _ = terminal::get().focus_window(&dash_wid).await;
                        }
                        match result {
                            Ok(_) => {
                                // The window's tab changed; drop its cached tab id
                                // so the next reload re-resolves it from a fresh
                                // snapshot (instead of showing the old tab).
                                app.window_tab_cache.remove(&wid);
                                app.set_status("Window moved".to_string(), false);
                            }
                            Err(e) => app.set_status(format!("Move failed: {e}"), true),
                        }
                    }
                    Action::FetchResumeList => {
                        let limit = config::get().launcher.resume_list_limit;
                        // Union every host's resumable list into one cross-host
                        // picker, tagging each candidate with its host so resume
                        // opens it there.
                        let (all, errors) = list_resumable_all_hosts(&app, limit);
                        if !all.is_empty() {
                            app.open_resume_picker(all);
                        } else if !errors.is_empty() {
                            app.set_status(
                                format!("List sessions failed: {}", errors.join("; ")),
                                true,
                            );
                        } else {
                            app.set_status("No resumable sessions found".to_string(), true);
                        }
                    }
                    Action::FetchBrowser => {
                        let limit = config::get().launcher.resume_list_limit;
                        // Running: every aggregated session (already host-tagged).
                        let mut entries: Vec<BrowserEntry> = app
                            .sessions
                            .iter()
                            .map(|s| BrowserEntry::Running(Box::new(s.clone())))
                            .collect();
                        // Resumable: the same cross-host walk as the resume picker.
                        let (resumable, errors) = list_resumable_all_hosts(&app, limit);
                        entries.extend(
                            resumable
                                .into_iter()
                                .map(|(h, c)| BrowserEntry::Resumable(h, c)),
                        );
                        if entries.is_empty() {
                            app.set_status("No sessions to browse".to_string(), true);
                        } else {
                            if !errors.is_empty() {
                                app.set_status(
                                    format!("Some hosts failed: {}", errors.join("; ")),
                                    true,
                                );
                            }
                            app.open_browser_picker(entries);
                        }
                    }
                    Action::KillSession {
                        host,
                        child_pid,
                        window_id,
                    } => {
                        // Route to the session's host — a signal locally, an RPC
                        // (blocking) remotely, so it rides `block_in_place`.
                        let killed = tokio::task::block_in_place(|| {
                            app.backend_for(&host).kill_session(child_pid)
                        });
                        if killed {
                            app.set_status(format!("Sent SIGTERM to pid {child_pid}"), false);
                        } else {
                            app.set_status(format!("kill({child_pid}) failed"), true);
                        }
                        if let Some(wid) = window_id {
                            let _ = terminal::get().close_window(&wid).await;
                        }
                        arm_settle_reload(&mut settle_reload_at);
                    }
                    Action::DetachRemote {
                        host,
                        token,
                        window_id,
                    } => {
                        // Detach ≠ kill: close the local ssh-attach window but send
                        // no signal/RPC, so the pooled session keeps running on the
                        // host. Drop the binding now for instant feedback (the row
                        // stays — still running remotely — just window-less, and
                        // Enter re-attaches). closing the window would also trip the
                        // reload's `prune_detached_sessions`, but doing it here makes
                        // the UI update without waiting for the snapshot.
                        app.window_bindings.remove(&host, &token);
                        let _ = terminal::get().close_window(&window_id).await;
                        app.set_status("Detached (session still running)".to_string(), false);
                        // Re-render the now window-less row and rewrite the
                        // bindings file with the dropped entry.
                        arm_settle_reload(&mut settle_reload_at);
                    }
                    Action::OpenShellTab { host, cwd } => 'shell_tab: {
                        // `w` is a deterministic per-(host, cwd) work tab: switch
                        // to the one recorded in `App::work_tabs` if its tab is
                        // still alive, else spawn a fresh shell tab and record it.
                        // It never scans for an unrelated shell that happens to
                        // sit in the cwd — only tabs captain-miao created count.
                        let label = app.shorten_path(&cwd).into_owned();
                        let key = (host.clone(), cwd.clone());
                        // What the new tab runs: a local shell in `cwd`, or an
                        // `ssh -t <target>` that cds into the remote cwd (a remote
                        // path never enters the *local* recent-cwd list).
                        let command = if host.is_local() {
                            app.push_recent_cwd(&cwd);
                            Some(SpawnCommand::Shell)
                        } else {
                            let argv = app.backend_for(&host).shell_argv(&cwd);
                            if argv.is_none() {
                                app.set_status(
                                    format!("Cannot open a shell on {}: no ssh target", host.0),
                                    true,
                                );
                            }
                            argv.map(SpawnCommand::Exec)
                        };
                        let Some(command) = command else {
                            break 'shell_tab;
                        };
                        // Only take the validation snapshot when there's an entry
                        // to validate — a first `w` on this cwd has nothing to
                        // check, so it skips straight to the spawn. And a failed
                        // snapshot bails the whole action: `live_work_tab` must
                        // only ever see a genuinely observed tab list (an empty
                        // default would spuriously prune a live entry, and the
                        // prune is persisted now).
                        let existing = if app.work_tabs.contains_key(&key) {
                            let tabs = match terminal::get().snapshot().await {
                                Ok(tabs) => tabs,
                                Err(e) => {
                                    app.set_status(format!("Work tab check failed: {e}"), true);
                                    break 'shell_tab;
                                }
                            };
                            app.live_work_tab(&key, &tabs)
                        } else {
                            None
                        };
                        let outcome: Result<&'static str, anyhow::Error> =
                            if let Some(tab_id) = existing {
                                terminal::get()
                                    .focus_tab(&tab_id)
                                    .await
                                    .map(|_| "Switched to work tab")
                            } else {
                                let title = super::work_tab_title(&cwd);
                                match terminal::get()
                                    .spawn(SpawnSpec {
                                        // The ssh child launches from the dashboard's
                                        // own cwd; the remote cwd is applied by the
                                        // `cd` in the argv.
                                        cwd: if host.is_local() {
                                            cwd.clone()
                                        } else {
                                            app.home_dir.clone()
                                        },
                                        target: SpawnTarget::NewTab,
                                        command,
                                        title: Some(title),
                                        hold: false,
                                        take_focus: true,
                                        stack: false,
                                    })
                                    .await
                                {
                                    Ok(result) => {
                                        // Record the tab so the next `w` on this cwd
                                        // switches back. Prefer the tab id the backend
                                        // returned (zellij prints it — no second
                                        // snapshot); else resolve the new window's tab
                                        // from one snapshot (kitty). Best-effort: an
                                        // unresolved tab just means the next `w`
                                        // spawns again.
                                        let tab_id = match result.tab.clone() {
                                            Some(tab_id) => Some(tab_id),
                                            None => match &result.window {
                                                Some(wid) => {
                                                    let tabs = terminal::get()
                                                        .snapshot()
                                                        .await
                                                        .unwrap_or_default();
                                                    crate::terminal::window_tab_map(&tabs)
                                                        .get(wid)
                                                        .cloned()
                                                }
                                                None => None,
                                            },
                                        };
                                        if let Some(tab_id) = tab_id {
                                            app.work_tabs.insert(
                                                key,
                                                super::WorkTab {
                                                    tab_id,
                                                    window_id: result.window,
                                                },
                                            );
                                        }
                                        Ok("Opened work tab")
                                    }
                                    Err(e) => Err(e),
                                }
                            };
                        match outcome {
                            Ok(verb) => app.set_status(format!("{verb} in {label}"), false),
                            Err(e) => app.set_status(format!("Work tab failed: {e}"), true),
                        }
                        // Persist the map (a fresh insert above, and/or a stale
                        // entry `live_work_tab` pruned) so a dashboard restart
                        // returns to these tabs instead of duplicating them.
                        app.save_work_tabs();
                    }
                    Action::ResumeSession {
                        agent,
                        cwd,
                        session_id,
                        fork,
                        host,
                    } => {
                        launch_agent(
                            &mut app,
                            agent,
                            &cwd,
                            Some((session_id.as_str(), fork)),
                            &LAUNCH_COPY_RESUME,
                            Some(&host),
                        )
                        .await;
                    }
                    Action::RestartSession(spec) => {
                        let _ = restart_one(&mut app, spec).await;
                        arm_settle_reload(&mut settle_reload_at);
                    }
                    Action::CopySessionId(sid) => {
                        match copy_to_clipboard(&sid) {
                            Ok(CopyOutcome::Cli) => {
                                app.set_status(format!("Copied session id {sid}"), false)
                            }
                            // No clipboard tool found; the OSC 52 fallback is
                            // unverified, so say so rather than claim success.
                            Ok(CopyOutcome::Osc52Fallback) => app.set_status(
                                format!("Sent session id {sid} to clipboard (no clipboard tool found; via terminal)"),
                                false,
                            ),
                            Err(e) => app.set_status(format!("Copy failed: {e}"), true),
                        }
                    }
                    Action::AttachRemoteRunning { host, pool_session } => {
                        // Spawn a local window that attaches to the running
                        // remote pool session, and bind it so the dashboard
                        // tracks (and later prunes) it. Bind the argv before the
                        // match so the backend borrow ends before `app` is used.
                        let argv = app.backend_for(&host).attach_argv(&pool_session);
                        match argv {
                            Some(argv) => {
                                let spec = SpawnSpec {
                                    cwd: app.home_dir.clone(),
                                    // An attach window is a session view, so it
                                    // gets the current session arrangement: the
                                    // shared sessions tab in Stacked (floating on
                                    // zellij, the `cm:sessions` stack tab on
                                    // Kitty), its own tab in Per-tab.
                                    target: resolve_spawn_target(
                                        app.capabilities,
                                        app.sessions_layout,
                                    ),
                                    command: SpawnCommand::Exec(argv),
                                    title: Some(format!("{} attach", host.0)),
                                    hold: true,
                                    take_focus: false,
                                    stack: true,
                                };
                                match terminal::get().spawn(spec).await {
                                    // A Floating/NewTab attach spawn always
                                    // recovers its window; a `None` can't be
                                    // bound, so report it rather than proceed.
                                    Ok(result) => match result.window {
                                        Some(id) => {
                                            app.set_status(
                                                format!("Attached to {pool_session} (window {id})"),
                                                false,
                                            );
                                            app.record_window_binding(
                                                host,
                                                pool_session,
                                                id.clone(),
                                            );
                                            // Same free tab id as a launch spawn —
                                            // keeps the next reload off a snapshot.
                                            if let Some(tab) = result.tab {
                                                app.window_tab_cache.insert(id.clone(), tab);
                                            }
                                            app.pending_focus_window = Some((id, Instant::now()));
                                        }
                                        None => app.set_status(
                                            "Attach failed: no window id".to_string(),
                                            true,
                                        ),
                                    },
                                    Err(e) => app.set_status(format!("Attach failed: {e}"), true),
                                }
                            }
                            None => app.set_status(
                                "Cannot attach: selected session has no remote host".to_string(),
                                true,
                            ),
                        }
                    }
                    Action::RestartAll { sessions } => {
                        let total = sessions.len();
                        let mut ok = 0usize;
                        for spec in sessions {
                            if restart_one(&mut app, spec).await {
                                ok += 1;
                            }
                        }
                        let msg = if ok == total {
                            format!("Restarted {ok} sessions")
                        } else {
                            format!("Restarted {ok}/{total} sessions")
                        };
                        app.set_status(msg, ok != total);
                        arm_settle_reload(&mut settle_reload_at);
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Free the paw image from kitty before we drop the alt screen.
    app.clear_logo_graphics();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_prune_floor() {
        let now = Instant::now();
        // Never pruned yet → always due.
        assert!(detach_prune_due(None, now));
        // Just pruned → held until the floor elapses.
        assert!(!detach_prune_due(Some(now), now));
        assert!(!detach_prune_due(
            Some(now - DETACH_PRUNE_MIN_INTERVAL + Duration::from_secs(1)),
            now
        ));
        // Floor elapsed → due again.
        assert!(detach_prune_due(Some(now - DETACH_PRUNE_MIN_INTERVAL), now));
    }
}
