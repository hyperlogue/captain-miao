//! Prevents the OS from auto-sleeping while at least one tracked session is
//! actively working. Per-platform backends:
//!
//!   - macOS: `caffeinate -dis -w <pid>` (the `-w` pins caffeinate's lifetime
//!     to ours so it self-terminates on a hard crash).
//!   - Linux: `systemd-inhibit --what=idle:sleep ... sleep infinity`, with
//!     `prctl(PR_SET_PDEATHSIG, SIGTERM)` set on the child so the inhibit
//!     lock is released if the dashboard dies without unwinding. Requires
//!     systemd; non-systemd setups (Alpine + OpenRC, slim containers) get
//!     a logged warning and no-op.
//!
//! Other platforms compile but the inhibitor is a no-op.

use std::cell::RefCell;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

/// Holds an optional subprocess that prevents idle / suspend while alive.
/// `Drop` reaps it on clean exit; the per-platform tricks above handle the
/// unclean cases. The child sits behind a `RefCell` so the read-only
/// `is_active(&self)` (called from the header render) can still reap a child
/// that died on its own.
pub struct SleepInhibitor {
    child: RefCell<Option<Child>>,
}

impl SleepInhibitor {
    pub fn new() -> Self {
        Self {
            child: RefCell::new(None),
        }
    }

    /// Drop a stale handle if the inhibitor child exited on its own (e.g.
    /// `pkill caffeinate`, OOM). Without this the dead handle would linger as a
    /// zombie, `is_active()` would lie, and `enable()` would refuse to respawn.
    fn reap_if_dead(&self) {
        let mut slot = self.child.borrow_mut();
        if let Some(c) = slot.as_mut()
            && matches!(c.try_wait(), Ok(Some(_)) | Err(_))
        {
            *slot = None;
        }
    }

    /// Whether an inhibitor subprocess is currently running on our behalf.
    pub fn is_active(&self) -> bool {
        self.reap_if_dead();
        self.child.borrow().is_some()
    }

    /// Spawn the platform-specific inhibitor if not already running. Failures
    /// are logged and swallowed; sleep prevention is best-effort.
    pub fn enable(&mut self) {
        self.reap_if_dead();
        if self.child.borrow().is_some() {
            return;
        }
        let child = spawn_inhibitor();
        if let Some(child) = child {
            tracing::debug!(
                target: "captain_miao::sleep",
                "spawned inhibitor pid={}",
                child.id(),
            );
            *self.child.borrow_mut() = Some(child);
        }
    }

    /// SIGTERM the inhibitor subprocess and reap it. No-op if not active.
    pub fn disable(&mut self) {
        let Some(mut child) = self.child.borrow_mut().take() else {
            return;
        };
        tracing::debug!(
            target: "captain_miao::sleep",
            "killing inhibitor pid={}",
            child.id(),
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.disable();
    }
}

/// Whether sleep inhibition is available on this system: the platform has a
/// backend AND its required binary is on `PATH`. Cached after first call so
/// the header can invoke it on every redraw without re-walking PATH. We
/// deliberately don't refresh on PATH changes — startup state is what
/// matters; any later install is picked up on next launch.
pub fn supported() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| match required_binary() {
        Some(name) => binary_exists(name),
        None => false,
    })
}

/// Human-readable explanation of why the inhibitor isn't usable. Only
/// meaningful when `supported()` is false; the toggle surfaces this as a
/// status-line error so the user knows what to install.
pub fn missing_reason() -> &'static str {
    match required_binary() {
        None => "Sleep prevention is not supported on this OS",
        Some("caffeinate") => {
            "`caffeinate` not found in PATH (ships in /usr/bin on macOS — \
             check your PATH)"
        }
        Some("systemd-inhibit") => {
            "`systemd-inhibit` not found in PATH (install systemd or use a \
             systemd-based distro)"
        }
        Some(other) => {
            // Future-proofing: if a new backend is added, fall back to a
            // generic message so we never show a stale binary name.
            tracing::warn!("missing_reason called for unknown backend: {other}");
            "Sleep-prevention backend unavailable"
        }
    }
}

/// Name of the binary `enable()` will try to spawn on this platform, or
/// `None` if no backend is implemented.
fn required_binary() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("caffeinate")
    } else if cfg!(target_os = "linux") {
        Some("systemd-inhibit")
    } else {
        None
    }
}

/// Probe `PATH` for an executable named `name`. Symlinks are followed by
/// `Path::is_file()` so /usr/bin/foo → /etc/alternatives/foo on Debian-likes
/// still resolves correctly.
fn binary_exists(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

#[cfg(target_os = "macos")]
fn spawn_inhibitor() -> Option<Child> {
    let pid = std::process::id().to_string();
    Command::new("caffeinate")
        .args(["-dis", "-w", &pid])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .inspect_err(|e| tracing::warn!("Failed to spawn caffeinate: {e}"))
        .ok()
}

#[cfg(target_os = "linux")]
fn spawn_inhibitor() -> Option<Child> {
    let mut cmd = Command::new("systemd-inhibit");
    cmd.args([
        "--what=idle:sleep",
        "--who=captain-miao",
        "--why=Active session",
        "--mode=block",
        "sleep",
        "infinity",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    // SAFETY: pre_exec runs in the forked child between fork and exec.
    // `prctl` is async-signal-safe. PR_SET_PDEATHSIG arranges for the kernel
    // to SIGTERM the child when its parent (us) dies, releasing the inhibit
    // lock even on hard crashes. Without this, a SIGKILL'd dashboard would
    // leave systemd holding the lock until the user logged out.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
        .inspect_err(|e| {
            tracing::warn!(
                "Failed to spawn systemd-inhibit: {e} \
             (sleep prevention requires systemd; install it or run on a \
             supported system)"
            )
        })
        .ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn spawn_inhibitor() -> Option<Child> {
    None
}

#[cfg(test)]
mod tests {
    use super::binary_exists;

    #[test]
    fn binary_exists_finds_real_tool() {
        // `sh` is on PATH on every Unix system this project targets. Used
        // as a "definitely exists" probe so we know the lookup logic works
        // independent of which sleep backend is in scope.
        assert!(binary_exists("sh"), "sh should be on PATH");
    }

    #[test]
    fn binary_exists_rejects_nonsense_name() {
        // A garbage name no one would have on PATH. Catches regressions
        // where a missing binary accidentally reports as present.
        assert!(!binary_exists("this-binary-does-not-exist-xyz123"));
    }
}
