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
    let root = state::state_dir();
    let watched_root = root.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        if let Ok(event) = result
            && changes_sessions(&watched_root, &event)
        {
            on_change();
        }
    })?;
    watcher.watch(&root.join("sessions"), RecursiveMode::NonRecursive)?;
    // Watch the directory because flag writes replace the file atomically.
    watcher.watch(&root, RecursiveMode::NonRecursive)?;
    for path in extra_paths {
        let _ = watcher.watch(&path, RecursiveMode::NonRecursive);
    }
    Ok(watcher)
}

fn changes_sessions(root: &Path, event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    // Ignore preference writes, temporary files and lock metadata. Otherwise
    // a refresh's own filesystem activity would immediately trigger another.
    event.paths.is_empty()
        || event.paths.iter().any(|path| {
            path != root
                && (path.parent() != Some(root)
                    || path
                        .file_name()
                        .is_some_and(|name| name == "session-flags.json"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, ModifyKind, RenameMode};

    #[test]
    fn wakes_for_session_and_agent_changes_without_a_read_write_loop() {
        let root = Path::new("/state");
        for (paths, expected) in [
            (vec!["/state/sessions/42.json"], true),
            (vec!["/agent/title-store-wal"], true),
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
            assert_eq!(changes_sessions(root, &event), expected, "{paths:?}");
            event.kind = EventKind::Access(AccessKind::Read);
            assert!(!changes_sessions(root, &event));
        }
    }
}
