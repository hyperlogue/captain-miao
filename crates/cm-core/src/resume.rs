//! Demand-driven, in-memory resume inventory shared by a host's clients.
//!
//! Snapshot reads never perform I/O. Refreshes run on blocking workers, reuse
//! unchanged file metadata, and retain each agent's last successful result on
//! failure. There is no timer. Concurrent refreshes share the completed scan.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use crate::agent::{AgentControl, ResumeCandidate};

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub candidates: Vec<ResumeCandidate>,
    pub errors: Vec<String>,
}

#[derive(Default)]
struct State {
    generation: u64,
    limit: usize,
    agents: HashMap<AgentControl, Vec<ResumeCandidate>>,
    errors: Vec<String>,
}

pub struct Inventory {
    state: Mutex<State>,
    refresh: Mutex<()>,
    parsers: Vec<Mutex<AgentCache>>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            refresh: Mutex::default(),
            parsers: AgentControl::ALL.iter().map(|_| Mutex::default()).collect(),
        }
    }
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self, limit: usize, per_agent: bool) -> Option<Snapshot> {
        let state = lock(&self.state);
        (state.generation > 0).then(|| snapshot(&state, limit, per_agent))
    }

    pub fn refresh(&self, limit: usize, per_agent: bool) -> Snapshot {
        self.refresh_with(limit, per_agent, |agent, limit, cache| {
            agent.list_resumable_cached(limit, cache)
        })
    }

    fn refresh_with(
        &self,
        limit: usize,
        per_agent: bool,
        lookup: impl Fn(AgentControl, usize, &mut AgentCache) -> anyhow::Result<Vec<ResumeCandidate>>
        + Sync,
    ) -> Snapshot {
        if limit == 0 {
            return Snapshot::default();
        }
        let before = lock(&self.state).generation;
        self.refresh_from(before, limit, per_agent, lookup)
    }

    fn refresh_from(
        &self,
        before: u64,
        limit: usize,
        per_agent: bool,
        lookup: impl Fn(AgentControl, usize, &mut AgentCache) -> anyhow::Result<Vec<ResumeCandidate>>
        + Sync,
    ) -> Snapshot {
        let _refresh = lock(&self.refresh);
        {
            let state = lock(&self.state);
            if state.generation != before && state.limit >= limit {
                return snapshot(&state, limit, per_agent);
            }
        }
        // Bound parallel CLI/filesystem work; neither one slow agent nor a
        // second dashboard should turn a refresh into unbounded fan-out.
        let mut results = Vec::new();
        for group in AgentControl::ALL
            .iter()
            .zip(&self.parsers)
            .collect::<Vec<_>>()
            .chunks(3)
        {
            std::thread::scope(|scope| {
                let jobs: Vec<_> = group
                    .iter()
                    .map(|(agent, parser)| {
                        let lookup = &lookup;
                        scope.spawn(move || {
                            let mut cache = lock(parser);
                            cache.begin();
                            let result = lookup(**agent, limit, &mut cache);
                            if result.is_ok() {
                                cache.finish();
                            }
                            (**agent, result)
                        })
                    })
                    .collect();
                for job in jobs {
                    results.push(job.join().expect("resume inventory worker panicked"));
                }
            });
        }
        let mut state = lock(&self.state);
        state.errors.clear();
        for (agent, result) in results {
            match result {
                Ok(candidates) => {
                    state.agents.insert(agent, candidates);
                }
                Err(error) => state.errors.push(format!("{}: {error}", agent.label())),
            }
        }
        state.limit = limit;
        state.generation += 1;
        snapshot(&state, limit, per_agent)
    }
}

fn snapshot(state: &State, limit: usize, per_agent: bool) -> Snapshot {
    let mut candidates: Vec<_> = AgentControl::ALL
        .iter()
        .filter_map(|agent| state.agents.get(agent))
        .flat_map(|rows| rows.iter().take(limit).cloned())
        .collect();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.mtime));
    if !per_agent {
        candidates.truncate(limit);
    }
    Snapshot {
        candidates,
        errors: state.errors.clone(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Default)]
pub(crate) struct AgentCache {
    pub candidates: FileCache<ResumeCandidate>,
    pub index: FileCache<HashMap<String, String>>,
}

impl AgentCache {
    fn begin(&mut self) {
        self.candidates.begin();
        self.index.begin();
    }
    fn finish(&mut self) {
        self.candidates.finish();
        self.index.finish();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    size: u64,
    inode: u64,
    device: u64,
    changed: (i64, i64),
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok()?,
        size: meta.len(),
        inode: meta.ino(),
        device: meta.dev(),
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}

struct FileEntry<T> {
    stamps: Vec<Option<Stamp>>,
    value: Option<T>,
    seen: bool,
}

pub(crate) struct FileCache<T> {
    entries: HashMap<PathBuf, FileEntry<T>>,
    #[cfg(test)]
    reads: usize,
}

impl<T> Default for FileCache<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            #[cfg(test)]
            reads: 0,
        }
    }
}

impl<T: Clone> FileCache<T> {
    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.reads
    }
    fn begin(&mut self) {
        for entry in self.entries.values_mut() {
            entry.seen = false;
        }
    }
    fn finish(&mut self) {
        self.entries.retain(|_, entry| entry.seen);
    }

    /// Dependencies cover sidecars; titles stored in separate databases can
    /// instead be overlaid after this lookup. Do not cache a read raced by a
    /// writer. A temporarily unreadable changed file retains its last value.
    pub fn get(
        &mut self,
        path: &Path,
        dependencies: &[&Path],
        read: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        let stamps = || {
            std::iter::once(path)
                .chain(dependencies.iter().copied())
                .map(stamp)
                .collect::<Vec<_>>()
        };
        let before = stamps();
        if before[0].is_none() {
            if matches!(std::fs::metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                self.entries.remove(path);
                return None;
            }
            if let Some(entry) = self.entries.get_mut(path) {
                entry.seen = true;
                return entry.value.clone();
            }
            return read();
        }
        if let Some(entry) = self.entries.get_mut(path) {
            entry.seen = true;
            if entry.stamps == before {
                return entry.value.clone();
            }
        }
        #[cfg(test)]
        {
            self.reads += 1;
        }
        let value = read();
        if value.is_none()
            && let Some(previous) = self.entries.get(path)
        {
            return previous.value.clone();
        }
        if before == stamps() && (self.entries.len() < 4096 || self.entries.contains_key(path)) {
            self.entries.insert(
                path.to_owned(),
                FileEntry {
                    stamps: before,
                    value: value.clone(),
                    seen: true,
                },
            );
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn candidate(agent: AgentControl, name: &str) -> ResumeCandidate {
        ResumeCandidate {
            agent,
            session_id: agent.cli_subcommand().into(),
            cwd: "~/project".into(),
            first_prompt: Some(name.into()),
            custom_title: None,
            git_branch: None,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn inventory_keeps_failed_agents_but_replaces_successful_empty_results() {
        let inventory = Inventory::new();
        assert!(inventory.snapshot(50, true).is_none());
        inventory.refresh_with(50, true, |agent, _, _| Ok(vec![candidate(agent, "old")]));
        let updated = inventory.refresh_with(50, true, |agent, _, _| match agent {
            AgentControl::Claude => anyhow::bail!("temporarily unavailable"),
            AgentControl::Codex => Ok(Vec::new()),
            _ => Ok(vec![candidate(agent, "new")]),
        });
        assert_eq!(updated.candidates.len(), AgentControl::ALL.len() - 1);
        assert_eq!(
            updated
                .candidates
                .iter()
                .find(|c| c.agent == AgentControl::Claude)
                .unwrap()
                .first_prompt
                .as_deref(),
            Some("old")
        );
        assert!(
            !updated
                .candidates
                .iter()
                .any(|c| c.agent == AgentControl::Codex)
        );
        assert!(
            updated
                .errors
                .iter()
                .any(|e| e.contains("temporarily unavailable"))
        );
        assert_eq!(inventory.snapshot(1, false).unwrap().candidates.len(), 1);
        assert_eq!(
            inventory.snapshot(1, true).unwrap().candidates.len(),
            AgentControl::ALL.len() - 1
        );
    }

    #[test]
    fn concurrent_requests_share_a_scan_but_a_larger_request_can_extend_it() {
        let inventory = Inventory::new();
        let calls = AtomicUsize::new(0);
        let lookup = |agent, _: usize, _: &mut AgentCache| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![candidate(agent, "cached")])
        };
        // Both requests began at generation zero. Completing the first one
        // before admitting the second models the refresh lock deterministically.
        let queued_at = lock(&inventory.state).generation;
        inventory.refresh_from(queued_at, 10, true, lookup);
        inventory.refresh_from(queued_at, 10, true, lookup);
        assert_eq!(calls.load(Ordering::SeqCst), AgentControl::ALL.len());
        inventory.refresh_from(queued_at, 20, true, lookup);
        assert_eq!(calls.load(Ordering::SeqCst), 2 * AgentControl::ALL.len());
        for _ in 0..100 {
            inventory.snapshot(20, true).unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2 * AgentControl::ALL.len(),
            "snapshot reads never trigger refreshes"
        );
    }

    #[test]
    fn slow_refresh_does_not_lock_out_cached_reads() {
        let inventory = Inventory::new();
        inventory.refresh_with(10, true, |agent, _, _| Ok(vec![candidate(agent, "old")]));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        std::thread::scope(|scope| {
            let refresh = scope.spawn(|| {
                inventory.refresh_with(10, true, |agent, _, _| {
                    if agent == AgentControl::Claude {
                        started_tx.send(()).unwrap();
                        lock(&release_rx).recv().unwrap();
                    }
                    Ok(vec![candidate(agent, "new")])
                })
            });
            started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("refresh lookup did not start");
            let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
            let cached_inventory = &inventory;
            scope.spawn(move || {
                snapshot_tx
                    .send(cached_inventory.snapshot(10, true))
                    .unwrap();
            });
            let observed = snapshot_rx.recv_timeout(Duration::from_secs(5));
            release_tx.send(()).unwrap();
            refresh.join().unwrap();
            let snapshot = observed
                .expect("cached snapshot blocked behind the refresh")
                .unwrap();
            assert!(
                snapshot
                    .candidates
                    .iter()
                    .all(|c| c.first_prompt.as_deref() == Some("old"))
            );
        });
    }

    #[test]
    fn unchanged_files_are_not_read_and_sidecars_replacements_and_deletions_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let sidecar = dir.path().join("session.meta");
        std::fs::write(&path, "session").unwrap();
        std::fs::write(&sidecar, "old").unwrap();
        let mut cache = FileCache::default();
        let read = || std::fs::read_to_string(&sidecar).ok();
        assert_eq!(cache.get(&path, &[&sidecar], read).as_deref(), Some("old"));
        assert_eq!(cache.get(&path, &[&sidecar], read).as_deref(), Some("old"));
        assert_eq!(cache.read_count(), 1);
        std::fs::write(&sidecar, "renamed").unwrap();
        assert_eq!(
            cache.get(&path, &[&sidecar], read).as_deref(),
            Some("renamed")
        );
        assert_eq!(cache.read_count(), 2);
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, "session").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        cache.get(&path, &[&sidecar], read);
        assert_eq!(
            cache.read_count(),
            3,
            "same-length replacement must be reread"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(cache.get(&path, &[&sidecar], read).is_none());
    }

    #[test]
    fn interrupted_writes_retain_the_previous_value_and_are_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        std::fs::write(&path, "old").unwrap();
        let mut cache = FileCache::default();
        assert_eq!(cache.get(&path, &[], || Some("old")), Some("old"));
        std::fs::write(&path, "incomplete").unwrap();
        assert_eq!(cache.get(&path, &[], || None), Some("old"));
        assert_eq!(cache.get(&path, &[], || Some("new")), Some("new"));
        assert_eq!(cache.read_count(), 3);
        cache.begin();
        cache.finish();
        assert!(
            cache.entries.is_empty(),
            "entries not revisited are evicted"
        );
    }

    #[test]
    fn a_file_changed_during_read_is_not_memoized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        std::fs::write(&path, "old").unwrap();
        let mut cache = FileCache::default();
        cache.get(&path, &[], || {
            std::fs::write(&path, "new content").unwrap();
            Some("old")
        });
        assert_eq!(cache.get(&path, &[], || Some("new")), Some("new"));
        assert_eq!(cache.read_count(), 2);
    }
}
