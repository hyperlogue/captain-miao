//! Shared filesystem wakeups for daemon subscribers and direct-local readers.

use std::path::{Path, PathBuf};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::state;

/// Watch launcher state and flag replacements, plus any caller-specific agent
/// stores. Missing agent stores are optional; session state must be watchable.
/// Keep the returned watcher alive for as long as notifications are needed.
pub fn watch_session_changes(
    extra_paths: impl IntoIterator<Item = PathBuf>,
    on_change: impl Fn() + Send + 'static,
) -> notify::Result<RecommendedWatcher> {
    watch_changes(state::state_dir(), extra_paths, on_change)
}

fn watch_changes(
    root: PathBuf,
    extra_paths: impl IntoIterator<Item = PathBuf>,
    on_change: impl Fn() + Send + 'static,
) -> notify::Result<RecommendedWatcher> {
    let root = root.canonicalize().unwrap_or(root);
    let watched_root = root.clone();
    // File watches die when SQLite removes/recreates its WAL. Watch their
    // parent directories, but filter to the named files so logs and other
    // conversations do not trigger database reads or subscriber wakeups.
    let extra: Vec<_> = extra_paths
        .into_iter()
        .map(|path| {
            let directory = path.is_dir();
            let path = path
                .canonicalize()
                .ok()
                .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))
                .unwrap_or(path);
            (path, directory)
        })
        .collect();
    let watched_extra = extra.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        if let Ok(event) = result
            && changes_sessions(&watched_root, &watched_extra, &event)
        {
            on_change();
        }
    })?;
    watcher.watch(&root.join("sessions"), RecursiveMode::NonRecursive)?;
    // Watch the directory because flag writes replace the file atomically.
    watcher.watch(&root, RecursiveMode::NonRecursive)?;
    for (path, directory) in extra {
        let target = if directory {
            Some(path.as_path())
        } else {
            path.parent()
        };
        if let Some(target) = target {
            let _ = watcher.watch(target, RecursiveMode::NonRecursive);
        }
    }
    Ok(watcher)
}

fn changes_sessions(root: &Path, extra: &[(PathBuf, bool)], event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    // Ignore preference writes, temporary files and lock metadata. Otherwise
    // a refresh's own filesystem activity would immediately trigger another.
    event.paths.is_empty()
        || event.paths.iter().any(|path| {
            path == &root.join("sessions")
                || path.parent() == Some(root.join("sessions").as_path())
                || path == &root.join("session-flags.json")
                || extra.iter().any(|(target, directory)| {
                    path == target || (*directory && path.parent() == Some(target.as_path()))
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, ModifyKind, RenameMode};

    #[test]
    fn title_watch_survives_a_missing_and_recreated_wal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        let store = dir.path().join("agent");
        std::fs::create_dir(&store).unwrap();
        let wal = store.join("summary.db-wal");
        let observed = wal.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = watch_changes(root, [wal.clone()], move || {
            if let Ok(text) = std::fs::read_to_string(&observed) {
                let _ = tx.send(text);
            }
        })
        .unwrap();
        for text in ["created", "recreated", "replaced"] {
            if text == "recreated" {
                std::fs::remove_file(&wal).unwrap();
            }
            if text == "replaced" {
                let tmp = store.join("replacement");
                std::fs::write(&tmp, text).unwrap();
                std::fs::rename(tmp, &wal).unwrap();
            } else {
                std::fs::write(&wal, text).unwrap();
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let actual = rx
                    .recv_timeout(remaining)
                    .unwrap_or_else(|e| panic!("waiting for {text} WAL notification: {e}"));
                if actual == text {
                    break;
                }
            }
        }
    }

    #[test]
    fn wakes_for_session_and_agent_changes_without_a_read_write_loop() {
        let root = Path::new("/state");
        let extra = [
            (PathBuf::from("/agent/title-store-wal"), false),
            (PathBuf::from("/agent/sessions"), true),
        ];
        for (paths, expected) in [
            (vec!["/state/sessions/42.json"], true),
            (vec!["/agent/title-store-wal"], true),
            (vec!["/agent/cli.log"], false),
            (vec!["/agent/title-store-shm"], false),
            (vec!["/agent/sessions/42.json"], true),
            (vec!["/state/session-flags.json"], true),
            (
                vec!["/state/session-flags.tmp", "/state/session-flags.json"],
                true,
            ),
            (vec!["/state/session-flags.tmp"], false),
            (vec!["/state/session-flags.lock"], false),
            (vec!["/state/dashboard-overrides.json"], false),
            (vec!["/state"], false),
            (vec![], true),
        ] {
            let mut event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)));
            event.paths = paths.iter().map(PathBuf::from).collect();
            assert_eq!(
                changes_sessions(root, &extra, &event),
                expected,
                "{paths:?}"
            );
            event.kind = EventKind::Access(AccessKind::Read);
            assert!(!changes_sessions(root, &extra, &event));
        }
    }
}
