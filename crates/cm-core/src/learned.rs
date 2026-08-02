//! Self-learning set of background commands observed to run long enough to
//! count as long-running services (dev servers, watchers) rather than finite
//! build/test steps.
//!
//! The launcher classifies a session's `run_in_background` shells to decide
//! whether a `BackgroundActive` row is a **busy** transient task or an
//! **at-rest** long-running server (see `launcher::classify_and_learn`). A
//! curated seed heuristic (`claude::is_long_running_command`) catches the
//! common dev servers immediately, but it can't know every command. So when a
//! background command the seed *didn't* recognize has been running past a
//! threshold (default 1h), the launcher records it here — and every future
//! session running that same command is treated as at-rest from the first
//! moment, no waiting.
//!
//! **Storage: one file per command, never a shared JSON.** captain-miao's whole
//! premise is many concurrent agent sessions, so several launchers may learn at
//! once. A single JSON would need read-modify-write and lose updates under that
//! concurrency; instead each learned command is its own file named by a hash of
//! the normalized command, created atomically (temp + rename) and never
//! rewritten. A lookup is one `stat`; learning is one idempotent create. No
//! locking, no races. The file's contents are the normalized command itself,
//! purely for `grep`-ability when debugging — presence is the only signal read.

use std::path::{Path, PathBuf};

use crate::state::state_dir;

fn learned_dir() -> PathBuf {
    state_dir().join("long-running-commands")
}

/// FNV-1a (constant seed) of the normalized command → a short, filesystem-safe,
/// stable filename. A raw command is unbounded and full of `/ ' &` that don't
/// belong in a filename; a 64-bit hash is plenty for this set's cardinality
/// (a handful of distinct dev-server command lines per machine).
fn command_hash(key: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Whether `key` (a normalized background command) has previously been learned
/// as long-running. One `stat`; a missing dir / unreadable path reads as "not
/// learned", the safe default (the caller then keeps timing it).
pub fn is_long_running(key: &str) -> bool {
    is_long_running_in(&learned_dir(), key)
}

/// Record `key` as long-running so every future session treats it as at-rest.
/// Idempotent and best-effort: a create race between two launchers is harmless
/// (both write the same content to the same path via a pid-suffixed temp), and
/// any IO error is swallowed — learning is an optimization, never load-bearing.
pub fn record_long_running(key: &str) {
    record_long_running_in(&learned_dir(), key);
}

// The store dir is a parameter of the internals so the round-trip is testable
// against a temp dir — no `state_dir()` / env manipulation that could race
// other tests reading the same env.
fn is_long_running_in(dir: &Path, key: &str) -> bool {
    dir.join(command_hash(key)).exists()
}

fn record_long_running_in(dir: &Path, key: &str) {
    // Owner-only: each file's contents are a command line the agent ran.
    if crate::state::create_dir_all_private(dir).is_err() {
        return;
    }
    let path = dir.join(command_hash(key));
    if path.exists() {
        return;
    }
    // temp + rename so a reader never sees a half-written file; the pid suffix
    // keeps two concurrent learners from clobbering each other's temp.
    let tmp = dir.join(format!("{}.{}.tmp", command_hash(key), std::process::id()));
    if std::fs::write(&tmp, key).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_hex() {
        let h = command_hash("npm run dev");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Same input → same hash (the whole point: a stable key across sessions).
        assert_eq!(h, command_hash("npm run dev"));
        assert_ne!(h, command_hash("npm run build"));
    }

    #[test]
    fn record_then_read_round_trips() {
        // Isolated temp dir passed directly — no env manipulation that could
        // race a parallel test reading the same var.
        let dir = std::env::temp_dir().join(format!("cm-learned-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let key = "some-custom-dev-server --port 9999";
        assert!(
            !is_long_running_in(&dir, key),
            "unknown command starts unlearned"
        );
        record_long_running_in(&dir, key);
        assert!(is_long_running_in(&dir, key), "learned after recording");
        // Idempotent: a second record doesn't error or change the answer.
        record_long_running_in(&dir, key);
        assert!(is_long_running_in(&dir, key));
        assert!(!is_long_running_in(&dir, "a different command"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
