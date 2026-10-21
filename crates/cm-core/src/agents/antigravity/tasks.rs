//! Live task tracking for agy 1.4.0. The transcript keeps the original RUNNING
//! snapshot after a task finishes; the conversation SQLite store updates it.
//!
//! Hooks own foreground activity. Only at rest do we watch the conversation's
//! database and WAL, coalesce writes, and read new or unfinished steps. Reads
//! run off the async runtime, with no database writes or periodic checks. macOS
//! relies on subsequent hooks because FSEvents misses held-open WALs; Linux
//! also refreshes on filesystem events. Idle hooks refresh outstanding tasks. A
//! failed/unknown read preserves the last observation. The launcher owns this
//! monitor, so dropping a session drops its watcher and SQLite connection.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::Watcher;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

const COALESCE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
enum WatchMode {
    Events,
    Hooks,
}

#[derive(Clone, PartialEq)]
struct Request {
    database: PathBuf,
    generation: u64,
    watching: bool,
    revision: u64,
}

#[derive(Clone)]
pub(crate) struct Update {
    revision: u64,
    commands: Vec<String>,
}

/// Foreground hooks plus live task observations, folded through one interface.
/// No filesystem reads occur on the hook path. Unknown task state can still
/// show Task when Stop explicitly reports fullyIdle: false.
pub(crate) struct Monitor {
    root: Option<PathBuf>,
    session: Option<String>,
    generation: u64,
    fully_idle: Option<bool>,
    commands: Option<Vec<String>>,
    request: Option<Request>,
    control: watch::Sender<Option<Request>>,
    updates: watch::Receiver<Option<Update>>,
    worker: tokio::task::JoinHandle<()>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl Monitor {
    pub(crate) fn new() -> Self {
        Self::at(super::cli_dir())
    }

    fn at(root: Option<PathBuf>) -> Self {
        Self::with_watch(
            root,
            if cfg!(target_os = "macos") {
                WatchMode::Hooks
            } else {
                WatchMode::Events
            },
        )
    }

    fn with_watch(root: Option<PathBuf>, mode: WatchMode) -> Self {
        let (control, requests) = watch::channel(None);
        let (updates_tx, updates) = watch::channel(None);
        Self {
            root,
            session: None,
            generation: 0,
            fully_idle: None,
            commands: None,
            request: None,
            control,
            updates,
            worker: tokio::spawn(observe(requests, updates_tx, mode)),
        }
    }

    pub(crate) fn dispatch_hook(&mut self, state: &mut LauncherState, message: HookMessage) {
        let session = message.session_id.as_ref().filter(|id| {
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if let Some(session) = session
            && (self.session.as_ref() != Some(session) || message.event == HookEvent::SessionStart)
        {
            self.session = Some(session.clone());
            self.generation += 1;
            self.fully_idle = None;
            self.commands = None;
            state.background_commands.clear();
        }
        let stopped = matches!(message.event, HookEvent::Stop | HookEvent::StopFailure);
        let fully_idle = stopped.then(|| {
            serde_json::from_str::<serde_json::Value>(message.raw.as_deref()?).ok()?["fullyIdle"].as_bool()
        }).flatten();
        super::dispatch_hook(state, message);
        if let Some(idle) = fully_idle {
            self.fully_idle = Some(idle);
            // A new Stop is fresher than the previous database observation.
            // Keep named unfinished tasks, but don't let an old empty result
            // hide a newly backgrounded command before its SQLite commit.
            if idle {
                self.commands = Some(Vec::new());
            } else if self.commands.as_ref().is_some_and(Vec::is_empty) {
                self.commands = None;
            }
        } else if !at_rest(&state.status) {
            self.fully_idle = None;
        }
        self.apply_commands(state);
        self.request_refresh(
            state,
            stopped || state.status == SessionStatus::BackgroundActive,
        );
    }

    fn request_refresh(&mut self, state: &LauncherState, force: bool) {
        let Some((root, session)) = self.root.as_ref().zip(self.session.as_ref()) else {
            return;
        };
        let mut request = Request {
            database: root.join("conversations").join(format!("{session}.db")),
            generation: self.generation,
            watching: at_rest(&state.status) && self.fully_idle != Some(true),
            revision: self.request.as_ref().map_or(0, |r| r.revision),
        };
        if !force && self.request.as_ref() == Some(&request) {
            return;
        }
        request.revision += 1;
        self.control.send_replace(Some(request.clone()));
        self.request = Some(request);
    }

    pub(crate) async fn changed(&mut self) -> Update {
        loop {
            if self.updates.changed().await.is_err() {
                // Lost observations must not turn select into a busy loop.
                std::future::pending::<()>().await;
            }
            if let Some(update) = self.updates.borrow_and_update().clone() {
                return update;
            }
        }
    }

    pub(crate) fn apply(&mut self, update: Update, state: &mut LauncherState) -> bool {
        if !self
            .request
            .as_ref()
            .is_some_and(|r| r.watching && r.revision == update.revision)
        {
            return false;
        }
        self.commands = Some(update.commands);
        self.apply_commands(state)
    }

    fn apply_commands(&self, state: &mut LauncherState) -> bool {
        if !at_rest(&state.status) {
            return false;
        }
        let background = match &self.commands {
            Some(commands) => !commands.is_empty(),
            None => self.fully_idle == Some(false),
        };
        let status = if background {
            SessionStatus::BackgroundActive
        } else if state.status == SessionStatus::BackgroundActive {
            SessionStatus::Idle
        } else {
            state.status.clone()
        };
        let commands = self.commands.clone().unwrap_or_default();
        let changed = state.status != status || state.background_commands != commands;
        state.status = status;
        state.background_commands = commands;
        changed
    }
}

fn at_rest(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Idle | SessionStatus::Compacted | SessionStatus::BackgroundActive
    )
}

async fn observe(
    mut control: watch::Receiver<Option<Request>>,
    updates: watch::Sender<Option<Update>>,
    mut mode: WatchMode,
) {
    let (events_tx, mut events) = mpsc::channel(1);
    let mut request: Option<Request> = None;
    let mut reader: Option<Reader> = None;
    let mut watcher = None;
    let mut deadline = None;
    let mut retried = false;
    loop {
        tokio::select! {
            changed = control.changed() => {
                if changed.is_err() { return; }
                let next = control.borrow_and_update().clone();
                if let Some(next) = &next {
                    let changed_session = request.as_ref().is_none_or(|old| old.database != next.database || old.generation != next.generation);
                    if changed_session {
                        reader = None;
                        watcher = None;
                        deadline = None;
                    }
                    if next.watching {
                        // Hook bursts share a fixed deadline rather than
                        // postponing the read indefinitely with each hook.
                        deadline.get_or_insert_with(|| Instant::now() + COALESCE);
                    } else {
                        watcher = None;
                        deadline = None;
                    }
                }
                request = next;
                retried = false;
                while events.try_recv().is_ok() {}
            }
            Some(()) = events.recv() => {
                if request.as_ref().is_some_and(|r| r.watching) {
                    deadline.get_or_insert_with(|| Instant::now() + COALESCE);
                    retried = false;
                }
            }
            () = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                deadline = None;
                let Some(current) = request.as_ref().filter(|r| r.watching) else { continue };
                // Arm before reading so completion during setup cannot be lost.
                // A directory watch survives WAL creation/removal and replaces.
                if watcher.is_none() && matches!(mode, WatchMode::Events) {
                    match watch_database(&current.database, events_tx.clone()) {
                        Ok(watch) => watcher = Some(watch),
                        Err(error) => {
                            tracing::debug!("Antigravity task watch unavailable: {error}");
                            mode = WatchMode::Hooks;
                        }
                    }
                }
                let path = current.database.clone();
                let prior = reader.take();
                let result = tokio::task::spawn_blocking(move || {
                    let mut reader = prior.unwrap_or_else(|| Reader::new(path));
                    let commands = reader.refresh();
                    (reader, commands)
                }).await;
                if let Ok((next, commands)) = result {
                    reader = Some(next);
                    match commands {
                        Ok(commands) => {
                            updates.send_replace(Some(Update { revision: current.revision, commands }));
                        }
                        Err(error) => {
                            tracing::debug!("Antigravity task read unavailable: {error}");
                            // One retry handles locks and initial creation races;
                            // quiet sessions never poll.
                            if !retried {
                                deadline = Some(Instant::now() + COALESCE);
                                retried = true;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn watch_database(
    path: &Path,
    events: mpsc::Sender<()>,
) -> notify::Result<notify::RecommendedWatcher> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let target = parent.join(path.file_name().unwrap_or_default());
    let mut wal = target.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && !matches!(event.kind, notify::EventKind::Access(_))
            && event.paths.iter().any(|p| p == &target || p == &wal)
        {
            // Bounded even while busy; ignore shared memory and other sessions.
            let _ = events.try_send(());
        }
    })?;
    watcher.watch(&parent, notify::RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

struct Reader {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    connection: Option<Connection>,
    through: i64,
    unfinished: BTreeMap<i64, Option<String>>,
    #[cfg(test)]
    rows_read: usize,
}

impl Reader {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            identity: None,
            connection: None,
            through: -1,
            unfinished: BTreeMap::new(),
            #[cfg(test)]
            rows_read: 0,
        }
    }

    fn refresh(&mut self) -> anyhow::Result<Vec<String>> {
        let meta = std::fs::metadata(&self.path)?;
        let identity = (meta.dev(), meta.ino());
        if self.identity != Some(identity) || self.connection.is_none() {
            let connection =
                Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            connection.busy_timeout(Duration::ZERO)?;
            self.connection = Some(connection);
            self.identity = Some(identity);
            self.through = -1;
            self.unfinished.clear();
        }
        let transaction = self.connection.as_mut().unwrap().transaction()?;
        let end: i64 =
            transaction.query_row("SELECT coalesce(max(idx), -1) FROM steps", [], |r| r.get(0))?;
        let (through, mut unfinished) = if end < self.through {
            (-1, BTreeMap::new())
        } else {
            (self.through, self.unfinished.clone())
        };
        // Revisit unfinished rows by PRIMARY KEY. A step can acquire task_details
        // after insertion, so pending non-task rows stay candidates too. Never
        // deserialize tool output or model payloads.
        let mut existing = transaction.prepare_cached(
            "SELECT status, CASE WHEN ?2 THEN task_details END FROM steps WHERE idx = ?1",
        )?;
        for idx in unfinished.keys().copied().collect::<Vec<_>>() {
            let needs_description = unfinished[&idx].is_none();
            let row = existing
                .query_row(rusqlite::params![idx, needs_description], |r| {
                    Ok((r.get::<_, i32>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
                })
                .optional()?;
            #[cfg(test)]
            {
                self.rows_read += 1;
            }
            if let Some((status, details)) = row {
                fold_step(&mut unfinished, idx, status, details.as_deref())?;
            } else {
                anyhow::bail!("unfinished task step disappeared");
            }
        }
        let mut new = transaction.prepare_cached("SELECT idx, status, CASE WHEN status IN (0, 1, 2, 8, 9, 11) THEN task_details END FROM steps WHERE idx > ?1 ORDER BY idx")?;
        let mut rows = new.query([through])?;
        while let Some(row) = rows.next()? {
            #[cfg(test)]
            {
                self.rows_read += 1;
            }
            let details: Option<Vec<u8>> = row.get(2)?;
            fold_step(
                &mut unfinished,
                row.get(0)?,
                row.get(1)?,
                details.as_deref(),
            )?;
        }
        let mut commands: Vec<_> = unfinished.values().filter_map(Clone::clone).collect();
        commands.sort();
        commands.dedup();
        self.through = end;
        self.unfinished = unfinished;
        Ok(commands)
    }
}

fn fold_step(
    unfinished: &mut BTreeMap<i64, Option<String>>,
    idx: i64,
    status: i32,
    details: Option<&[u8]>,
) -> anyhow::Result<()> {
    // CortexStepStatus from agy 1.4.0's protobuf descriptor. A real CLI probe
    // verifies RUNNING=2 -> DONE=3 for a command held past foreground Stop.
    match status {
        0 | 1 | 2 | 8 | 9 | 11 => {
            let command = details
                .filter(|b| !b.is_empty())
                .map(task_description)
                .transpose()?
                .or_else(|| unfinished.get(&idx).cloned().flatten());
            unfinished.insert(idx, command);
        }
        3..=7 | 12 => {
            unfinished.remove(&idx);
        }
        _ => anyhow::bail!("unknown Antigravity step status {status}"),
    }
    Ok(())
}

/// task_details is a protobuf: field 4 is the command/timer description, field
/// 7 its summary. Skip unrelated fields without decoding them. A missing or
/// malformed description is unknown, never evidence that no task exists.
fn task_description(mut bytes: &[u8]) -> anyhow::Result<String> {
    let mut command = None;
    let mut summary = None;
    while !bytes.is_empty() {
        let tag = varint(&mut bytes)?;
        anyhow::ensure!(tag >> 3 != 0, "invalid task field");
        let size = match tag & 7 {
            0 => {
                varint(&mut bytes)?;
                continue;
            }
            1 => 8,
            2 => usize::try_from(varint(&mut bytes)?)?,
            5 => 4,
            _ => anyhow::bail!("unsupported task wire type"),
        };
        anyhow::ensure!(size <= bytes.len(), "truncated task field");
        let (value, rest) = bytes.split_at(size);
        bytes = rest;
        if tag & 7 == 2 && matches!(tag >> 3, 4 | 7) {
            let text = std::str::from_utf8(value)?.trim();
            if !text.is_empty() {
                if tag >> 3 == 4 {
                    command = Some(text.to_owned());
                } else {
                    summary = Some(text.to_owned());
                }
            }
        }
    }
    command
        .or(summary)
        .ok_or_else(|| anyhow::anyhow!("task description unavailable"))
}

fn varint(bytes: &mut &[u8]) -> anyhow::Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = bytes
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("truncated task varint"))?;
        *bytes = rest;
        anyhow::ensure!(shift != 63 || byte <= 1, "task varint overflow");
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("task varint overflow")
}

#[cfg(test)]
mod tests;
