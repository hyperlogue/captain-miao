//! Observe one path and its directory lifetime without periodically reading it.
//! Used by launcher socket recovery and pool ownership. Parent watches survive
//! atomic file replacement; ancestor events re-arm watches after directory loss.
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::Watcher;
use tokio::sync::mpsc;

pub(crate) struct PathChanges {
    path: PathBuf,
    watcher: Option<notify::RecommendedWatcher>,
    signal: mpsc::Sender<()>,
    events: mpsc::Receiver<()>,
    initial: bool,
    retry_at: Option<tokio::time::Instant>,
}

impl PathChanges {
    pub(crate) fn new(path: &Path) -> Self {
        let (signal, events) = mpsc::channel(1);
        let mut changes = Self {
            path: path.to_path_buf(),
            watcher: None,
            signal,
            events,
            initial: true,
            retry_at: None,
        };
        changes.arm();
        changes
    }

    fn arm(&mut self) {
        self.watcher = match watch_path(&self.path, self.signal.clone()) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::debug!("Path notifications unavailable; retrying after 30s: {error}");
                None
            }
        };
        self.retry_at = self
            .watcher
            .is_none()
            .then(|| tokio::time::Instant::now() + Duration::from_secs(30));
    }

    /// The initial wake lets the caller check after registration, closing the
    /// setup race. Subsequent healthy waits do no I/O until an event arrives.
    pub(crate) async fn changed(&mut self) {
        if self.initial {
            self.initial = false;
            return;
        }
        // A caller can retry a failed repair even when that failure produces
        // no filesystem event (for example, temporary descriptor exhaustion).
        // Keep the deadline across select cancellation: socket traffic must
        // not keep postponing recovery when notifications are unavailable.
        let deadline = self.retry_at;
        tokio::select! {
            _ = self.events.recv(), if self.watcher.is_some() => {
                while self.events.try_recv().is_ok() {}
            }
            _ = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
        self.retry_at = None;
        self.arm();
    }

    pub(crate) fn retry(&mut self) {
        self.retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(1));
    }
}

fn watch_path(path: &Path, signal: mpsc::Sender<()>) -> notify::Result<notify::RecommendedWatcher> {
    let target = path.to_path_buf();
    let mut directory = path.parent().unwrap_or(Path::new("/"));
    while !directory.is_dir() {
        directory = directory
            .parent()
            .ok_or_else(notify::Error::path_not_found)?;
    }
    let canonical = std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    let real_target = canonical.join(path.strip_prefix(directory).unwrap_or(path));
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let changed = match result {
            // On Linux, our own stat/read/open must not wake us again.
            Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => false,
            Ok(event) => {
                event.paths.is_empty()
                    || event
                        .paths
                        .iter()
                        .any(|p| target.starts_with(p) || real_target.starts_with(p))
            }
            Err(_) => true,
        };
        if changed {
            let _ = signal.try_send(());
        }
    })?;
    watcher.watch(&canonical, notify::RecursiveMode::NonRecursive)?;
    if let Some(parent) = canonical.parent() {
        watcher.watch(parent, notify::RecursiveMode::NonRecursive)?;
    }
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn changed(changes: &mut PathChanges, phase: &str) {
        tokio::time::timeout(Duration::from_secs(15), changes.changed())
            .await
            .unwrap_or_else(|_| panic!("waiting for {phase}"));
    }

    #[tokio::test]
    async fn survives_file_replacement_and_parent_recreation() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("runtime/launchers");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("owner");
        std::fs::write(&path, "first").unwrap();
        let mut changes = PathChanges::new(&path);
        changed(&mut changes, "initial registration").await;
        let replacement = directory.join("replacement");
        std::fs::write(&replacement, "next").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        changed(&mut changes, "atomic replacement").await;
        std::fs::remove_dir_all(root.path().join("runtime")).unwrap();
        changed(&mut changes, "directory removal").await;
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, "restored").unwrap();
        changed(&mut changes, "directory recreation").await;
        // Re-registration now follows the recreated parent, so it keeps seeing
        // later changes too. The initial event may include creation and write.
        while changes.events.try_recv().is_ok() {}
        std::fs::write(&replacement, "third").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        changed(&mut changes, "replacement in the new directory").await;
    }

    #[tokio::test]
    async fn sibling_writes_and_own_reads_do_not_wake_the_observer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owner");
        std::fs::write(&path, "owner").unwrap();
        let mut changes = PathChanges::new(&path);
        changes.changed().await;
        for i in 0..20 {
            std::fs::write(root.path().join("other"), i.to_string()).unwrap();
            std::fs::read(&path).unwrap();
            std::fs::metadata(&path).unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(300), changes.changed())
                .await
                .is_err()
        );
    }
}
