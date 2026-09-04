//! Watching a transcript for changes — the mechanism only.
//!
//! Everything here is about *noticing* a write: which watcher can see it, what
//! an event has to look like to count as ours, and what to do where the platform
//! cannot report one at all. Nothing here reads a transcript or touches a
//! `LauncherState`; folding a read into the row is `launcher.rs`'s job, and the
//! seam between the two is a bare `tx.send(())` — a wake, carrying nothing.
//!
//! Two watchers, because one does not cover the ground: the platform's
//! event-driven one (FSEvents/inotify) wherever it fires, and a stat poll where
//! it cannot (Codex on macOS — see [`AgentControl::transcript_poll_interval`]).
//! [`TranscriptWatch`] is what makes them interchangeable to the caller, and
//! dropping either is what stops it.

use std::path::{Path, PathBuf};

use notify::Watcher as _;

use crate::agent::AgentControl;

/// A live transcript watch. Dropping either variant stops it — the binding's
/// lifetime is the watch's lifetime, which is how the poll's engaged-only
/// lifecycle (the gate at the bottom of the loop) turns it on and off.
pub(super) enum TranscriptWatch {
    /// The platform's event-driven watcher (FSEvents/inotify) — Claude
    /// everywhere, Codex on Linux.
    #[allow(dead_code)] // held for its Drop; never read
    Event(notify::RecommendedWatcher),
    /// The stat poll standing in where the platform events can't fire (Codex
    /// on macOS — see [`AgentControl::transcript_poll_interval`]).
    #[allow(dead_code)]
    Poll(StatPoll),
}

/// Handle to a [`start_stat_poll`] task; aborts the task on drop, mirroring
/// how dropping a notify watcher stops its watch.
pub(super) struct StatPoll(tokio::task::JoinHandle<()>);

impl Drop for StatPoll {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Stat `path` every `interval` and signal `tx` whenever `(size, mtime)` moves
/// — the stand-in transcript watch for a writer that defeats the platform's
/// fs events (Codex holds its rollout fd open for the whole session, which
/// macOS FSEvents reports nothing for until close; `write(2)` updates the stat
/// metadata immediately). The first stat is a silent baseline: what's already
/// on disk is covered by the creation wake the gate fires, not by this task.
///
/// Hand-rolled rather than notify's `PollWatcher` deliberately: that watcher
/// compares mtime **truncated to whole seconds** and nothing else (size only
/// enters via the opt-in whole-file content hash), so the second of two
/// appends landing within one wall-clock second never fires — and a rollout
/// is exactly that write pattern (e.g. a `turn_aborted` written sub-second
/// after the previous line would be missed forever, sticking the row at
/// Active). Comparing the full-precision mtime *and* the size catches every
/// append to an append-only file. Pinned by `stat_poll_sees_held_fd_appends`.
pub(super) fn start_stat_poll(
    path: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
    interval: std::time::Duration,
) -> StatPoll {
    StatPoll(tokio::spawn(async move {
        // `Some((size, mtime))` per stat; `None` = missing/unreadable. The
        // outer Option distinguishes "no baseline yet" from "file was absent".
        let mut prev: Option<Option<(u64, std::time::SystemTime)>> = None;
        loop {
            let cur = std::fs::metadata(&path)
                .ok()
                .map(|m| (m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH)));
            if let Some(p) = &prev
                && *p != cur
            {
                let _ = tx.send(());
            }
            prev = Some(cur);
            tokio::time::sleep(interval).await;
        }
    }))
}

/// Watch Grok's `signals.json` (and any later replaced sibling) only when the
/// file already exists. [`start_file_watcher`] would otherwise fall back to the
/// session directory, which also holds `updates.jsonl`.
pub(super) fn arm_replaced_sidecars(
    agent: AgentControl,
    transcript: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
    dest: &mut Vec<TranscriptWatch>,
) {
    for path in agent.replaced_sidecar_paths(transcript) {
        if !path.is_file() {
            continue;
        }
        match start_file_watcher(&path, tx.clone()) {
            Ok(w) => dest.push(TranscriptWatch::Event(w)),
            Err(e) => tracing::debug!("sidecar watch failed: {e}"),
        }
    }
}

/// Watch a single file and signal `tx` on every non-Access change, via the
/// platform's event-driven watcher (FSEvents/inotify). Used for the transcript
/// (except where the poll stands in — see [`start_stat_poll`]) and the agent's
/// session-status file. Falls back to watching the parent directory (filtering
/// to `path`) when the file doesn't exist yet.
pub(super) fn start_file_watcher(
    path: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> notify::Result<notify::RecommendedWatcher> {
    let target = path.to_path_buf();
    // The path we register and the path the backend reports back are not always
    // the same string: macOS FSEvents resolves symlinks (and `/var` → `/private/var`)
    // before reporting, while Linux inotify echoes the path as registered. An
    // agent can report a transcript through any symlinked config/session tree;
    // on macOS the event then arrives under the real path and a raw-string
    // filter silently freezes the transcript fold. Accept either spelling.
    let real = canonical_watch_target(path).filter(|p| *p != target);
    // MOVE_SELF on a file-inode watch often arrives with an empty `paths`
    // vec. Dropping those is how a Grok `/rename` (atomic replace) silently
    // killed the watch. Only a *file* watch may treat empty paths as ours —
    // the parent fallback shares the watch with siblings (Grok's
    // `updates.jsonl` is the long-held-fd stream we refuse to follow).
    let file_present = path.is_file();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Drop Access events (open/close/read). On Linux notify uses inotify
        // with a mask that includes IN_OPEN, and `scan_transcript_signals`
        // opens the transcript on every wakeup — without this filter, our own
        // open fires our own watch and the loop spins at 100% CPU.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        // When we fall back to watching the parent directory (because the file
        // doesn't exist yet), events will fire for sibling transcripts too —
        // filter to just our file.
        let ours = event
            .paths
            .iter()
            .any(|p| p == &target || Some(p) == real.as_ref());
        if ours || (file_present && event.paths.is_empty()) {
            let _ = tx.send(());
        }
    };
    let mut w = notify::recommended_watcher(handler)?;

    // Try watching the file directly; fall back to its parent directory if
    // the file doesn't exist yet (notify requires the path to exist on some
    // platforms). Do *not* also watch the parent when the file exists: for
    // Grok that directory holds `updates.jsonl`, the append stream a file
    // watch exists to ignore.
    if w.watch(path, notify::RecursiveMode::NonRecursive).is_err()
        && let Some(parent) = path.parent()
    {
        w.watch(parent, notify::RecursiveMode::NonRecursive)?;
    }
    Ok(w)
}

/// The real path a watch event will carry for `path`, for comparison against the
/// path we registered. Resolves symlinks in the file itself when it exists, and
/// otherwise in its parent (the parent-directory fallback watches a file that
/// hasn't been created yet, but its directory is already real). `None` when
/// neither resolves — there is then nothing to match beyond the raw path.
fn canonical_watch_target(path: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(path) {
        return Some(p);
    }
    let parent = path.parent()?;
    let name = path.file_name()?;
    Some(std::fs::canonicalize(parent).ok()?.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transcript reached through a symlinked directory must resolve to the
    /// real path, because that is the spelling macOS FSEvents reports and the
    /// watch filter compares against. Covers both the file-exists case and the
    /// parent-directory fallback (file not created yet).
    #[test]
    fn canonical_watch_target_resolves_symlinked_dirs() {
        let base = std::env::temp_dir().join(format!("cm-watch-target-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let existing = link.join("rollout.jsonl");
        std::fs::write(&existing, b"{}\n").unwrap();
        let want = std::fs::canonicalize(&real).unwrap();
        assert_eq!(
            canonical_watch_target(&existing),
            Some(want.join("rollout.jsonl"))
        );

        // Not yet created: resolved via the parent, which does exist.
        let pending = link.join("not-yet.jsonl");
        assert_eq!(
            canonical_watch_target(&pending),
            Some(want.join("not-yet.jsonl"))
        );

        // Neither the file nor its parent exists: nothing to resolve.
        assert_eq!(
            canonical_watch_target(&base.join("gone").join("x.jsonl")),
            None
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
