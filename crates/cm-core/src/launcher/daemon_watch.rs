//! Pool ownership: read the daemon PID file only when its path changes, and
//! observe process death independently. Linux pidfds need no liveness polling.
use std::path::Path;
use std::time::Duration;

use crate::{fs_events::PathChanges, state};

pub(super) async fn wait(minting: u32, path: &Path) {
    wait_with(minting, path, || {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    })
    .await;
}

async fn wait_with(minting: u32, path: &Path, mut read_pid: impl FnMut() -> Option<u32>) {
    let mut changes = PathChanges::new(path);
    changes.changed().await;
    let mut current = read_pid();
    let exited = process_exited(minting);
    tokio::pin!(exited);
    loop {
        if state::minting_pool_daemon_is_gone(minting, state::is_process_alive, current) {
            return;
        }
        tokio::select! {
            _ = &mut exited => return,
            _ = changes.changed() => current = read_pid(),
        }
    }
}

async fn process_exited(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{FromRawFd, OwnedFd};
        // A pidfd identifies this process even if its numeric PID is reused.
        // It becomes readable at exit, including before a zombie is reaped.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd >= 0 {
            let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
            if let Ok(fd) = tokio::io::unix::AsyncFd::new(fd)
                && fd.readable().await.is_ok()
            {
                return;
            }
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
    }
    // macOS's liveness probe uses libproc, not filesystem reads. Also preserve
    // supervision on Linux kernels/sandboxes without pidfd support. The PID
    // file stays event-driven on every platform; no repeated server.pid reads.
    loop {
        if !state::is_process_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn child() -> tokio::process::Child {
        tokio::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn quiet_daemon_needs_no_repeated_pid_file_reads_and_exit_is_seen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("server.pid");
        let mut process = child();
        let pid = process.id().unwrap();
        std::fs::write(&path, pid.to_string()).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&reads);
        let (ready, received) = tokio::sync::oneshot::channel();
        let mut ready = Some(ready);
        let watcher = tokio::spawn(async move {
            wait_with(pid, &path, || {
                count.fetch_add(1, Ordering::SeqCst);
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
                std::fs::read_to_string(&path).ok()?.parse().ok()
            })
            .await;
        });
        tokio::time::timeout(Duration::from_secs(15), received)
            .await
            .expect("waiting for daemon-watch registration")
            .unwrap();
        // FSEvents can deliver the pid file's creation after registration,
        // costing one harmless extra read. Count only reads after it settles.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let settled = reads.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            reads.load(Ordering::SeqCst),
            settled,
            "quiet ownership must not reread server.pid"
        );
        process.start_kill().unwrap();
        tokio::time::timeout(Duration::from_secs(15), watcher)
            .await
            .expect("waiting for daemon process exit")
            .unwrap();
        process.wait().await.unwrap();
    }

    #[tokio::test]
    async fn replacement_pid_is_seen_while_the_original_process_is_alive() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("server.pid");
        let mut original = child();
        let mut replacement = child();
        let pid = original.id().unwrap();
        std::fs::write(&path, pid.to_string()).unwrap();
        let watched = path.clone();
        let (ready, received) = tokio::sync::oneshot::channel();
        let mut ready = Some(ready);
        let watcher = tokio::spawn(async move {
            wait_with(pid, &watched, || {
                let value = std::fs::read_to_string(&watched).ok()?.parse().ok();
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
                value
            })
            .await;
        });
        tokio::time::timeout(Duration::from_secs(15), received)
            .await
            .expect("waiting for replacement-watch registration")
            .unwrap();
        let next = root.path().join("new.pid");
        std::fs::write(&next, replacement.id().unwrap().to_string()).unwrap();
        std::fs::rename(next, path).unwrap();
        tokio::time::timeout(Duration::from_secs(15), watcher)
            .await
            .expect("waiting for daemon PID replacement")
            .unwrap();
        assert!(state::is_process_alive(pid));
        original.kill().await.unwrap();
        replacement.kill().await.unwrap();
    }
}
