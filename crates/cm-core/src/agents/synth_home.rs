//! A **synthetic agent home**: a directory that mirrors the agent's real home
//! through symlinks, but where a few entries are ours (a hooks file the agent
//! discovers) or a writable *copy* of the real one (a config the agent must be
//! able to write back to).
//!
//! Agents whose hooks can't be injected per-invocation discover them from a file
//! in their home directory, so the only way to hook them is to hand them a
//! different home. Codex is the first backend that needs this; every comment
//! below is a lesson paid for there, and a later backend gets it for free.
//!
//! The synthetic home holds **nothing of its own** but the owned files and the
//! copies — everything else in it belongs to the real home by construction. That
//! invariant is what makes the shadow-replacement below safe.

use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// An entry seeded into the synthetic home as a **writable copy** of the real
/// one instead of a symlink.
///
/// It must not be a symlink: the real file is frequently read-only (e.g. a
/// nix-store / home-manager symlink), and an agent that persists state into its
/// own home — Codex writes hook trust into `$CODEX_HOME/config.toml` — fails on a
/// write to a read-only target ("config/batchWrite failed while updating hook
/// trust"). Copying lets that write land.
pub(super) struct CopiedEntry {
    /// File name, the same in both homes.
    pub name: &'static str,
    /// Where to record the real file's content as of the last copy. The copy is
    /// refreshed only when that snapshot stops matching the real file — never
    /// when the *agent* mutates the copy itself — so agent-persisted state (hook
    /// trust) survives across launches while the user's own edits still
    /// propagate.
    pub snapshot: &'static str,
}

/// Where a synthetic home lives, what it mirrors, and which entries we don't
/// mirror. Keep `dir` a stable path with stable contents: an agent that gates on
/// a content hash (Codex's hook-trust prompt) then asks at most once per machine
/// rather than once per launch.
pub(super) struct SynthHome<'a> {
    /// The synthetic home itself — what gets handed to the agent.
    pub dir: PathBuf,
    /// The real home to mirror. `None` (unresolvable) still yields a usable
    /// synthetic home carrying just our own files.
    pub real: Option<PathBuf>,
    /// Entries we write ourselves; never linked, never copied.
    pub owned: &'a [&'a str],
    /// Entries copied writable rather than symlinked (see [`CopiedEntry`]).
    pub copied: &'a [CopiedEntry],
}

impl SynthHome<'_> {
    /// Create / refresh the synthetic home: symlink every entry of the real home
    /// except the ones we own or copy, then refresh the copies. Call
    /// [`SynthHome::write_owned`] afterwards for each owned file.
    pub(super) fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        // The synth home holds copies of (possibly 0600) real files, so restrict
        // the directory to the owner (0700) — best-effort, an older 0755 dir from
        // a previous build gets tightened here too.
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));

        if let Some(real) = &self.real
            && let Ok(entries) = std::fs::read_dir(real)
        {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if self.is_ours(&name) {
                    continue;
                }
                let link = self.dir.join(&name);
                match std::fs::read_link(&link) {
                    // Already a symlink: refresh only when it points elsewhere. A
                    // *dangling* one is fine — the target path is right, and the
                    // agent creating the file through it lands in the real home.
                    Ok(target) => {
                        if target != entry.path() {
                            atomic_symlink(&entry.path(), &link);
                        }
                    }
                    // Not a symlink: either nothing is here (link it), or a real
                    // file/dir is shadowing the real home's entry. That happens
                    // whenever the agent adds a new state file: it creates it
                    // *inside* the synthetic home before the name exists in the
                    // real one, so no symlink is ever made and the two copies then
                    // diverge — which an `!link.exists()` guard would make
                    // permanent. Worst case seen was a split-brain SQLite DB: the
                    // main file resolved to the stale synthetic copy while
                    // `-wal`/`-shm`, symlinked once the real home grew them,
                    // resolved to the real home's — and Codex refused to start
                    // ("local database appears to be damaged"). Everything
                    // mirrored here belongs to the real home by construction, so
                    // replacing a shadow is safe — the synthetic home holds
                    // nothing of its own but the owned files and the copies, both
                    // (re)written on every launch.
                    Err(_) => {
                        if let Ok(meta) = std::fs::symlink_metadata(&link) {
                            // rename(2) can't replace a directory, so clear it first.
                            let removed = if meta.is_dir() {
                                std::fs::remove_dir_all(&link)
                            } else {
                                std::fs::remove_file(&link)
                            };
                            if removed.is_ok() {
                                tracing::debug!(
                                    "replaced shadow entry {} with a link to the real home",
                                    link.display()
                                );
                            }
                        }
                        atomic_symlink(&entry.path(), &link);
                    }
                }
            }
        }

        for entry in self.copied {
            self.sync_copy(entry);
        }
        Ok(())
    }

    /// Write one of our own files into the synthetic home — but only when its
    /// contents would change, so concurrent launches never race a half-written
    /// file and a content-keyed trust hash stays put. `name` must be one of
    /// [`SynthHome::owned`], or [`SynthHome::ensure`] will have symlinked the
    /// real home's entry over the top of it.
    pub(super) fn write_owned(&self, name: &str, contents: &str) -> Result<()> {
        let path = self.dir.join(name);
        let unchanged = std::fs::read_to_string(&path)
            .map(|cur| cur == contents)
            .unwrap_or(false);
        if !unchanged {
            atomic_write(&path, contents.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }

    /// Is `name` an entry we own or copy (and therefore never symlink)?
    fn is_ours(&self, name: &OsStr) -> bool {
        self.owned.iter().any(|o| name == OsStr::new(o))
            || self.copied.iter().any(|c| name == OsStr::new(c.name))
    }

    /// Seed / refresh one copied entry from the real home (see [`CopiedEntry`]).
    fn sync_copy(&self, entry: &CopiedEntry) {
        let Some(real_home) = &self.real else {
            return;
        };
        let Ok(real_content) = std::fs::read_to_string(real_home.join(entry.name)) else {
            return; // nothing readable to copy; leave any existing synth copy as-is
        };
        let copy = self.dir.join(entry.name);
        let snapshot = self.dir.join(entry.snapshot);

        let is_symlink = std::fs::symlink_metadata(&copy)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let last_source = std::fs::read_to_string(&snapshot).unwrap_or_default();
        // In sync already (and a real file, not a leftover symlink from an older
        // build) → keep the agent's own writes to the copy intact.
        if copy.exists() && !is_symlink && last_source == real_content {
            return;
        }
        // Write a fresh writable copy atomically. The rename replaces a stale
        // symlink (or out-of-date copy) in one step, so a launching agent never
        // sees it missing or half-written.
        if atomic_write(&copy, real_content.as_bytes()).is_ok() {
            let _ = atomic_write(&snapshot, real_content.as_bytes());
        }
    }
}

/// Replace `link` with a symlink to `target` atomically: build it at a temp
/// path, then rename over `link` (rename is atomic on POSIX). Two concurrent
/// launches can each refresh the same link without ever exposing a window where
/// it's missing. Best-effort — failures leave the next launch to retry.
fn atomic_symlink(target: &Path, link: &Path) {
    let (Some(parent), Some(name)) = (link.parent(), link.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if std::os::unix::fs::symlink(target, &tmp).is_ok() && std::fs::rename(&tmp, link).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write `bytes` to `path` atomically (temp file in the same dir + rename), so a
/// concurrently-launching agent never reads a half-written file. Renaming a
/// regular file over a symlink replaces the link itself, which is what lets a
/// copied entry supersede a stale read-only symlink.
///
/// The temp file is created mode 0600 so the final file is 0600 too: these
/// writes copy files out of the user's real agent home (often 0600), and going
/// through the default umask (typically 0644) would silently downgrade a private
/// config to world/group-readable.
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent dir"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("bad file name"))?;
    let tmp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let write_tmp = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        // An existing temp left by a crashed run keeps its old mode through
        // `open`, so set it explicitly to be sure the final file is 0600.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    };
    if let Err(e) = write_tmp() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
