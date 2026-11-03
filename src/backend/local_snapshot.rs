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
            rows: self.inner.list_sessions(),
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
            let rows = reader.list_sessions();
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
