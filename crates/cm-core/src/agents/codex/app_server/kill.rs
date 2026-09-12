//! Explicit removal belongs to the owning host, including for older launchers
//! that cannot exit after a failed cleanup. Restart never enters this fallback.
use super::control::{self, ForceRemoval, StopFailure};
use super::transport;
use crate::backend::CleanupPolicy;
use crate::state::{self, LauncherState};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) async fn force_removal_reason(
    config: &super::CodexConfig,
    error: &anyhow::Error,
    id: Option<&str>,
) -> Option<ForceRemoval> {
    if error.downcast_ref::<transport::Unavailable>().is_some() {
        return Some(ForceRemoval::Unreachable);
    }
    if let Some(rpc) = error.downcast_ref::<transport::RpcError>()
        && rpc.code == Some(-32600)
        && rpc.method == "thread/backgroundTerminals/clean"
        && id.is_some_and(|id| rpc.message == format!("thread not found: {id}"))
    {
        return Some(ForceRemoval::ThreadMissing);
    }
    if error.downcast_ref::<transport::RpcError>().is_some() {
        return None;
    }
    // Fencing can fail before any cleanup RPC reaches the daemon. Probe only
    // availability in that case; a blocked relay alone proves nothing.
    let path = config.socket_path().ok()?;
    match transport::Client::connect(&path).await {
        Err(error) if error.downcast_ref::<transport::Unavailable>().is_some() => {
            Some(ForceRemoval::Unreachable)
        }
        _ => None,
    }
}

/// Older control replies carry only the formatted error. Match the exact
/// errors their adapter emitted, including the selected thread's identity;
/// generic RPC failures and launcher-control timeouts never authorize removal.
fn legacy_force_removal(message: &str, id: Option<&str>) -> bool {
    let Some(message) = message.strip_prefix("Codex cleanup failed: ") else {
        return false;
    };
    if message.starts_with("connecting to Codex app-server socket: ")
        || message.starts_with("Codex app-server connection timed out: ")
        || message == "Codex initialize timed out: deadline has elapsed"
        || message == "Codex app-server disconnected"
    {
        return true;
    }
    id.is_some_and(|id| {
        message == format!("Codex thread/backgroundTerminals/clean: thread not found: {id}")
    })
}

pub(crate) fn kill(snapshot: &LauncherState, policy: CleanupPolicy) -> Result<()> {
    let Err(error) = control::request_stop(snapshot.launcher_pid) else {
        return Ok(());
    };
    let Some(failure) = error.downcast_ref::<StopFailure>() else {
        return Err(error);
    };
    let allowed = match failure.force_removal {
        Some(ForceRemoval::Unreachable | ForceRemoval::ThreadMissing) => true,
        Some(ForceRemoval::Denied) => false,
        None => legacy_force_removal(&failure.message, snapshot.session_id.as_deref()),
    };
    if policy != CleanupPolicy::ForceIfUnavailable || !allowed {
        return Err(error);
    }

    tracing::warn!("Forcing Codex session removal after cleanup failed: {error:#}");
    let Some(current) = current_owner(snapshot)? else {
        return Ok(());
    };
    // A structured reason also identifies launchers that implement ForceStop.
    // Let them reap their TUI and run normal destructors before using signals.
    if failure.force_removal.is_some() && control::force_stop(current.launcher_pid).is_ok() {
        wait_for_exit(current.launcher_pid)?;
    } else {
        let Some(current) = current_owner(snapshot)? else {
            return Ok(());
        };
        if let Some(pid) = current.child_pid {
            signal_kill(pid)?;
            // Give the launcher a chance to reap its TUI before it too exits.
            wait_for_exit(pid)?;
        }
        signal_kill(current.launcher_pid)?;
        wait_for_exit(current.launcher_pid)?;
    }
    // Normal exit may already have unlinked the state. If a new launcher now
    // owns the same pid, never remove its freshly created files.
    current_owner(snapshot)?;
    remove_runtime(snapshot.launcher_pid)
}

fn current_owner(snapshot: &LauncherState) -> Result<Option<LauncherState>> {
    let path = state::sessions_dir().join(format!("{}.json", snapshot.launcher_pid));
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let current: LauncherState = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        current.launcher_pid == snapshot.launcher_pid
            && current.binding_token() == snapshot.binding_token()
            && current.agent == snapshot.agent
            && current.codex_mode == snapshot.codex_mode
            && current.session_id == snapshot.session_id,
        "Codex session changed during cleanup; retry Kill"
    );
    Ok(Some(current))
}

fn signal_kill(pid: u32) -> Result<()> {
    anyhow::ensure!(
        pid > 1 && pid <= i32::MAX as u32 && pid != std::process::id(),
        "invalid Codex process id"
    );
    if unsafe { libc::kill(pid as i32, libc::SIGKILL) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

fn wait_for_exit(pid: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while state::is_process_alive(pid) {
        // An exited process can await its parent's waitpid briefly. We must
        // not reap a child owned by the terminal/pool's existing wait loop.
        let zombie = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|out| {
                out.status.success() && out.stdout.trim_ascii_start().starts_with(b"Z")
            });
        if zombie {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Codex process did not exit after forced cleanup"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn remove_runtime(pid: u32) -> Result<()> {
    fn unlink(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("removing Codex launcher runtime file"),
        }
    }
    let dir = state::runtime_dir().join("launchers");
    for suffix in [".sock", "-codex.sock", "-settings.json"] {
        unlink(&dir.join(format!("{pid}{suffix}")))?;
    }
    match std::fs::remove_dir_all(crate::clipboard::shim::session_image_dir(pid)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing Codex launcher images"),
    }
    unlink(&state::sessions_dir().join(format!("{pid}.json")))
}
