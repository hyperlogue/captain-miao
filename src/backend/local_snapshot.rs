//! Local filesystem scans are owned by one worker; the UI reads the last
//! completed snapshot. No UI lock is held while a transcript or state file is
//! read. Completion wakes the same backend change signal as filesystem events.
use super::*;

pub(super) struct LocalSnapshot {
    reader: Arc<Mutex<LocalBackend>>,
    pending: Option<oneshot::Receiver<(Vec<LauncherState>, SessionIndex)>>,
    rows: Vec<LauncherState>,
    index: SessionIndex,
    again: bool,
}

impl LocalHost {
    pub(super) fn enable_background_reads(&mut self) {
        let changed = self.changed.clone();
        let reader = LocalBackend::new().with_change_notifier(move || {
            changed.store(true, Ordering::Relaxed);
        });
        self.snapshot = Some(LocalSnapshot {
            reader: Arc::new(Mutex::new(reader)),
            pending: None,
            rows: Self::read_sessions(&self.inner),
            index: self.inner.session_index(),
            again: false,
        });
    }

    pub(super) fn refresh_background_reads(&mut self) {
        let Some(snapshot) = &mut self.snapshot else {
            return;
        };
        if let Some(reply) = &mut snapshot.pending {
            match reply.try_recv() {
                Ok((rows, index)) => {
                    snapshot.rows = rows;
                    snapshot.index = index;
                    snapshot.pending = None;
                    if !std::mem::take(&mut snapshot.again) {
                        return;
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    snapshot.again = true;
                    return;
                }
                Err(oneshot::error::TryRecvError::Closed) => snapshot.pending = None,
            }
        }
        let reader = snapshot.reader.clone();
        let changed = self.changed.clone();
        let (tx, rx) = oneshot::channel();
        snapshot.pending = Some(rx);
        tokio::task::spawn_blocking(move || {
            let mut reader = reader.lock().unwrap();
            let rows = Self::read_sessions(&reader);
            let index = reader.session_index();
            let _ = tx.send((rows, index));
            changed.store(true, Ordering::Relaxed);
        });
    }
}

impl LocalSnapshot {
    pub(super) fn rows(&self) -> Vec<LauncherState> {
        self.rows.clone()
    }
    pub(super) fn index(&self) -> SessionIndex {
        self.index.clone()
    }
}

impl Backend {
    pub(crate) fn enable_background_reads(&mut self) {
        if let Self::Local(host) = self {
            host.enable_background_reads();
        }
    }

    pub(crate) fn refresh_background_reads(&mut self) {
        if let Self::Local(host) = self {
            host.refresh_background_reads();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn background_reads_preserve_the_selected_launcher() {
        const CHILD: &str = "CM_TEST_TOWER_SNAPSHOT";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "backend::local_snapshot::tests::background_reads_preserve_the_selected_launcher",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHILD, "1")
                .env("HOME", dir.path())
                .env("XDG_CONFIG_HOME", dir.path().join("config"))
                .env("XDG_STATE_HOME", dir.path().join("state"))
                .env("XDG_RUNTIME_DIR", dir.path().join("run"))
                .env(crate::tower::FLAG, "1")
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "isolated snapshot fixture failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let mut row = LauncherState::for_test(
            crate::agent::AgentKind::Claude,
            crate::state::SessionStatus::Idle,
        );
        row.launcher_pid = std::process::id();
        state::create_dir_all_private(&state::sessions_dir()).unwrap();
        row.write().unwrap();
        let Backend::Local(mut host) = Backend::local() else {
            unreachable!()
        };
        assert!(host.list_sessions()[0].needs_restart);
        host.enable_background_reads();
        assert!(
            host.list_sessions()[0].needs_restart,
            "the initial background snapshot bypassed launcher selection"
        );
        host.refresh_background_reads();
        // Wait for the actual worker reply, not an assumed scan duration.
        let snapshot = host.snapshot.as_mut().unwrap();
        let (rows, index) =
            tokio::time::timeout(Duration::from_secs(60), snapshot.pending.take().unwrap())
                .await
                .expect("awaiting background session scan")
                .unwrap();
        snapshot.rows = rows;
        snapshot.index = index;
        assert!(
            host.list_sessions()[0].needs_restart,
            "the refreshed background snapshot bypassed launcher selection"
        );
    }
}
