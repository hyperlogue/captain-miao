//! Host-owned session flags for both direct-local dashboards and the daemon.
//!
//! Status observation, acknowledgements and legacy imports use one locked
//! transaction. Persisting the last observed status beside the flags prevents
//! another process from replaying a completion after it was acknowledged.
//! Launcher state remains single-writer; this module only writes the sidecar.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::{self, LauncherState, SessionFlags, SessionKey, SessionStatus};

/// Shared filesystem-backed flag ownership. Readers receive launcher rows with
/// flags attached; writers acknowledge against the latest launcher status.
pub struct SessionFlagsStore {
    root: PathBuf,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    flags: SessionFlags,
    // Additive to the original SessionKey -> SessionFlags file format. This
    // bookkeeping stays on disk and never enters SessionFlags or the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed_status: Option<SessionStatus>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct LegacyFlags {
    pinned: Vec<u32>,
    follow_up: Vec<u32>,
}

impl Default for SessionFlagsStore {
    fn default() -> Self {
        Self::new(state::state_dir())
    }
}

impl SessionFlagsStore {
    pub fn new(state_root: PathBuf) -> Self {
        Self { root: state_root }
    }

    /// Observe current status and return rows with durable flags. Failed
    /// writes serve the previous flags and leave the transition retryable.
    /// The boolean asks the caller to notify its other readers of a change.
    pub fn snapshot(&self) -> (Vec<LauncherState>, bool) {
        match self.update(None) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!("could not refresh session flags: {error:#}");
                let mut sessions = state::read_launcher_states_in(&self.root.join("sessions"));
                if let Ok(entries) = self.read_entries() {
                    overlay(&mut sessions, &entries);
                }
                (sessions, false)
            }
        }
    }

    pub fn set(&self, key: &SessionKey, flags: SessionFlags) -> Result<()> {
        self.update(Some((key, flags))).map(|_| ())
    }

    /// Commit legacy dashboard flags before a preference save removes its old
    /// arrays. Existing sidecar entries, including explicit clears, win.
    pub fn migrate_legacy(&self) -> Result<()> {
        self.update(None).map(|_| ())
    }

    fn update(
        &self,
        requested: Option<(&SessionKey, SessionFlags)>,
    ) -> Result<(Vec<LauncherState>, bool)> {
        let _lock = self.lock()?;
        // Read after locking: two processes must never commit observations
        // collected in the opposite order from their writes.
        let mut sessions = state::read_launcher_states_in(&self.root.join("sessions"));
        let before = self.read_entries()?;
        let legacy: LegacyFlags =
            state::read_json(&self.root.join("dashboard-overrides.json")).unwrap_or_default();
        let mut entries = before.clone();
        for &pid in legacy.pinned.iter().chain(&legacy.follow_up) {
            let key = SessionKey::from_launcher_pid(pid);
            if !self.launcher_file_exists(&key) {
                continue;
            }
            entries.entry(key).or_insert_with(|| Entry {
                flags: SessionFlags {
                    pinned: legacy.pinned.contains(&pid),
                    follow_up: legacy.follow_up.contains(&pid),
                },
                observed_status: None,
            });
        }
        for session in &sessions {
            let entry = entries.entry(session.key()).or_default();
            if let Some(want) = follow_up_change(
                &session.status,
                entry.observed_status.as_ref(),
                entry.flags.follow_up,
            ) {
                entry.flags.follow_up = want;
            }
            entry.observed_status = Some(session.status.clone());
        }
        if let Some((key, flags)) = requested {
            // Apply after observation so the next reader cannot re-arm a
            // completion the human just acknowledged.
            entries.entry(key.clone()).or_default().flags = flags;
        }
        // An unreadable launcher row is not proof of departure. Preserve its
        // flags until the file is gone, including across version skew.
        entries.retain(|key, _| {
            requested.is_some_and(|(k, _)| k == key) || self.launcher_file_exists(key)
        });
        let changed = entries
            .iter()
            .any(|(key, entry)| before.get(key).map(|e| e.flags) != Some(entry.flags))
            || before.keys().any(|key| !entries.contains_key(key));
        if entries != before {
            state::write_json_atomic(&self.root.join("session-flags.json"), &entries)?;
        }
        overlay(&mut sessions, &entries);
        Ok((sessions, changed))
    }

    fn read_entries(&self) -> Result<HashMap<SessionKey, Entry>> {
        match std::fs::read(self.root.join("session-flags.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading session flags"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(error) => Err(error).context("reading session flags"),
        }
    }

    fn launcher_file_exists(&self, key: &SessionKey) -> bool {
        let Ok(pid) = key.as_str().parse::<u32>() else {
            return false;
        };
        self.root
            .join("sessions")
            .join(format!("{pid}.json"))
            .try_exists()
            .unwrap_or(true)
    }

    fn lock(&self) -> Result<File> {
        state::create_dir_all_private(&self.root)?;
        // Lock a stable inode: the JSON itself is replaced atomically. Every
        // transaction opens its own descriptor so threads and processes share
        // the same exclusion. Closing the descriptor releases the lock.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join("session-flags.lock"))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(file);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("locking session flags");
            }
        }
    }
}

fn overlay(sessions: &mut [LauncherState], entries: &HashMap<SessionKey, Entry>) {
    for session in sessions {
        session.flags = Some(
            entries
                .get(&session.key())
                .map(|e| e.flags)
                .unwrap_or_default(),
        );
    }
}

fn follow_up_change(
    status: &SessionStatus,
    previous: Option<&SessionStatus>,
    follow_up: bool,
) -> Option<bool> {
    use SessionStatus::*;
    let entered_rest = matches!(
        (previous, status),
        (
            Some(Active | BackgroundActive | BackgroundServer | ReviewPending),
            Idle
        ) | (Some(Compacting), Compacted)
    );
    let parked_server =
        *status == BackgroundServer && previous.is_some_and(|p| *p != BackgroundServer);
    if (entered_rest || parked_server) && !follow_up {
        Some(true)
    } else if *status == Active && follow_up {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(SessionFlagsStore);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "cm-flags-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            state::create_dir_all_private(&root.join("sessions")).unwrap();
            Self(SessionFlagsStore::new(root))
        }

        fn status(&self, status: SessionStatus) -> LauncherState {
            let row = LauncherState {
                launcher_pid: std::process::id(),
                ..LauncherState::for_test(AgentControl::Claude, status)
            };
            self.write(&row);
            row
        }

        fn write(&self, row: &LauncherState) {
            state::write_json_atomic(
                &self
                    .0
                    .root
                    .join(format!("sessions/{}.json", row.launcher_pid)),
                row,
            )
            .unwrap();
        }

        fn flags(&self) -> SessionFlags {
            self.0.snapshot().0[0].flags.unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }

    #[test]
    fn status_transitions_are_observed_and_persisted_by_the_store() {
        use SessionStatus::*;
        for (before, after, wanted) in [
            (Active, Idle, true),
            (BackgroundActive, Idle, true),
            (BackgroundServer, Idle, true),
            (ReviewPending, Idle, true),
            (Compacting, Compacted, true),
            (Active, BackgroundServer, true),
            (Active, BackgroundActive, false),
            (Idle, Idle, false),
        ] {
            let f = Fixture::new();
            f.status(before);
            assert!(!f.flags().follow_up, "a first observation must not ring");
            let row = f.status(after);
            assert_eq!(f.flags().follow_up, wanted, "{:?}", row.status);
            let another = SessionFlagsStore::new(f.0.root.clone());
            assert_eq!(another.snapshot().0[0].flags.unwrap().follow_up, wanted);
        }
    }

    #[test]
    fn an_independent_reader_cannot_replay_an_acknowledged_completion() {
        let f = Fixture::new();
        let row = f.status(SessionStatus::Active);
        f.flags();
        let other = SessionFlagsStore::new(f.0.root.clone());
        f.status(SessionStatus::Idle);
        assert!(other.snapshot().0[0].flags.unwrap().follow_up);
        let cleared = SessionFlags {
            pinned: true,
            follow_up: false,
        };
        other.set(&row.key(), cleared).unwrap();
        assert_eq!(f.flags(), cleared);
        f.status(SessionStatus::Active);
        f.flags();
        f.status(SessionStatus::Idle);
        assert!(f.flags().follow_up);
        assert!(f.flags().pinned);
    }

    #[test]
    fn legacy_flags_migrate_without_overwriting_sidecar_acknowledgements() {
        let f = Fixture::new();
        let row = f.status(SessionStatus::Idle);
        state::write_json_atomic(&f.0.root.join("dashboard-overrides.json"),
            &serde_json::json!({"pinned": [row.launcher_pid], "follow_up": [row.launcher_pid], "prefs": {"prevent_sleep": false}})).unwrap();
        let armed = SessionFlags {
            pinned: true,
            follow_up: true,
        };
        f.0.migrate_legacy().unwrap();
        assert_eq!(f.flags(), armed);
        f.0.set(&row.key(), SessionFlags::default()).unwrap();
        // Migration can be repeated if the dashboard crashed before rewriting
        // preferences; an explicit clear in the sidecar remains authoritative.
        SessionFlagsStore::new(f.0.root.clone())
            .migrate_legacy()
            .unwrap();
        assert_eq!(f.flags(), SessionFlags::default());
        let legacy_wire: HashMap<SessionKey, SessionFlags> =
            state::read_json(&f.0.root.join("session-flags.json")).unwrap();
        assert_eq!(legacy_wire[&row.key()], SessionFlags::default());
    }

    #[test]
    fn old_sidecar_flags_and_first_seen_idle_rows_are_preserved() {
        let f = Fixture::new();
        let row = f.status(SessionStatus::BackgroundServer);
        assert!(!f.flags().follow_up);
        let flags = SessionFlags {
            pinned: false,
            follow_up: true,
        };
        state::write_json_atomic(
            &f.0.root.join("session-flags.json"),
            &HashMap::from([(row.key(), flags)]),
        )
        .unwrap();
        f.status(SessionStatus::Idle);
        assert_eq!(f.flags(), flags);
        f.status(SessionStatus::Active);
        assert!(!f.flags().follow_up);
    }

    #[test]
    fn independent_writers_keep_each_others_flags() {
        let f = Fixture::new();
        let first = f.status(SessionStatus::Idle);
        let second = LauncherState {
            launcher_pid: unsafe { libc::getppid() } as u32,
            ..first.clone()
        };
        f.write(&second);
        let keys = [first.key(), second.key()];
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for key in &keys {
                let store = SessionFlagsStore::new(f.0.root.clone());
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for i in 0..30 {
                        store
                            .set(
                                key,
                                SessionFlags {
                                    pinned: i % 2 == 1,
                                    follow_up: true,
                                },
                            )
                            .unwrap();
                    }
                });
            }
        });
        let rows = f.0.snapshot().0;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|s| s.flags
            == Some(SessionFlags {
                pinned: true,
                follow_up: true
            })));
    }

    #[test]
    fn an_unreadable_session_keeps_its_flags_until_its_file_is_gone() {
        let f = Fixture::new();
        let row = f.status(SessionStatus::Idle);
        let path = f.0.root.join(format!("sessions/{}.json", row.launcher_pid));
        std::fs::write(&path, "unrecognized launcher format").unwrap();
        state::write_json_atomic(
            &f.0.root.join("dashboard-overrides.json"),
            &serde_json::json!({"follow_up": [row.launcher_pid]}),
        )
        .unwrap();
        f.0.migrate_legacy().unwrap();
        assert!(f.0.snapshot().0.is_empty());
        assert!(f.0.read_entries().unwrap()[&row.key()].flags.follow_up);
        f.write(&row);
        assert!(f.flags().follow_up);
        std::fs::remove_file(path).unwrap();
        assert!(f.0.snapshot().0.is_empty());
        assert!(f.0.read_entries().unwrap().is_empty());
    }
}
