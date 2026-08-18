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
//! The synthetic home holds **nothing of its own** but the owned files, the
//! copies, and any `.shadow-*` quarantine a past launch set aside — everything
//! else in it belongs to the real home by construction. That invariant is what
//! makes the shadow-replacement below safe.
//!
//! One thing moves the *other* way. An entry the **agent** created in here
//! because the real home had no such name to mirror — a credential file written
//! by a first login inside a session — is moved back out on the next launch
//! ([`SynthHome::adopt_agent_writes`]), so the agent's own state never ends up
//! stranded behind a mirror it cannot see out of. Nothing captain-miao writes
//! is eligible, and that is enforced by the code rather than left to whoever
//! adds the next backend: an entry named in `owned` or `copied` carries our
//! hook config, and installing that in the real home would fire our hooks in
//! every session the user runs *outside* captain-miao.

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
    /// Entries the **agent** owns outright, moved back into the real home if a
    /// session created them here — see [`SynthHome::adopt_agent_writes`].
    ///
    /// **Nothing captain-miao injects or updates may appear here.** An entry
    /// listed in `owned` or `copied` carries our hook config, and moving one
    /// into the real home would install our hooks globally: every session the
    /// user runs *outside* captain-miao would then shell out to `miao hook`
    /// with no launcher socket, on every event. The rule is enforced rather
    /// than remembered — [`SynthHome::adopt_agent_writes`] skips any name
    /// [`SynthHome::is_ours`] claims.
    pub adopted: &'a [&'a str],
    /// Remove links we minted whose real-home entry has since been deleted.
    ///
    /// Set it on a mirror of a **loader-scanned collection** — opencode's
    /// `plugins/`, Grok's `hooks/` — where every entry is read by the agent and
    /// a dangling import is at best noise and at worst fails the whole load.
    ///
    /// Leave it off for a **state mirror** (the homes themselves), where a
    /// dangling link is load-bearing: the agent recreating the file writes
    /// *through* the link into the real home, keeping both sides converged. A
    /// prune there would turn that recreation into a shadow file the real home
    /// never sees — the exact divergence this module exists to prevent.
    pub prune: bool,
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

        // Before the linking pass, not after: an adopted entry lands in the real
        // home, so the loop below then finds the name in `read_dir(real)` and
        // links it with no special case of its own.
        self.adopt_agent_writes();

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
                    // ("local database appears to be damaged"). The shadow must
                    // go, but not its *contents*: a shadow can hold state the
                    // agent minted before the real home grew the name — at worst
                    // a whole session tree — so it is set aside under a dotted
                    // quarantine name the mirror never touches, and only deleted
                    // when even the rename fails. Recovering a quarantined
                    // shadow is a manual merge; losing it silently was the
                    // defect.
                    Err(_) => {
                        if std::fs::symlink_metadata(&link).is_ok() {
                            let secs = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            let quarantine = self.dir.join(format!(
                                ".shadow-{}-{}-{secs}",
                                name.to_string_lossy(),
                                std::process::id()
                            ));
                            if std::fs::rename(&link, &quarantine).is_ok() {
                                tracing::warn!(
                                    "quarantined shadow entry {} to {} and linked the real \
                                     home's; merge it back by hand if it held anything",
                                    link.display(),
                                    quarantine.display()
                                );
                            } else {
                                let meta = std::fs::symlink_metadata(&link);
                                let _ = if meta.map(|m| m.is_dir()).unwrap_or(false) {
                                    std::fs::remove_dir_all(&link)
                                } else {
                                    std::fs::remove_file(&link)
                                };
                            }
                        }
                        atomic_symlink(&entry.path(), &link);
                    }
                }
            }
        }

        if self.prune {
            self.prune_stale_links();
        }

        for entry in self.copied {
            self.sync_copy(entry);
        }
        Ok(())
    }

    /// Drop symlinks we minted for real-home entries that no longer exist (see
    /// [`SynthHome::prune`]). Only exact mints are touched — a symlink pointing
    /// anywhere but the real home's entry of the same name, or a plain file,
    /// is not ours to judge and is left alone. Best-effort, like the linking.
    fn prune_stale_links(&self) {
        let Some(real) = &self.real else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if self.is_ours(&name) {
                continue;
            }
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            if target != real.join(&name) {
                continue;
            }
            // lstat, not stat: a real-home entry that is itself a dangling
            // symlink is still present, and mirroring it is not our call to
            // revisit here.
            if std::fs::symlink_metadata(&target).is_err() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
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

    /// Move entries the agent minted **here** into the real home, so its own
    /// state stops living behind a mirror it cannot see out of.
    ///
    /// The case this exists for is a first-ever login. The linking pass walks
    /// `read_dir(real)`, so it only ever considers names the real home already
    /// has; a credential file that has never existed there gets no link, the
    /// agent's `open`/`rename` therefore creates a *real* file inside the
    /// synthetic home, and the user's credentials stay invisible to a bare
    /// `codex` / `grok` run outside captain-miao. Worse, the moment they log in
    /// outside, that file becomes a shadow the mirror quarantines.
    ///
    /// Every write here is the **agent's own**, never ours: `adopted` may not
    /// name anything in [`SynthHome::owned`] or [`SynthHome::copied`], and the
    /// skip below is what enforces it rather than a comment asking nicely.
    ///
    /// Torn state is not a hazard: these agents write credentials by staging a
    /// temporary file and renaming it, so anything found at the synthetic path
    /// is a complete file. That is also why this can run at *launch* — a moment
    /// another session of the same agent may be mid-flight, since the synthetic
    /// home is shared per agent — rather than needing a session to end.
    ///
    /// Rules, in the order they are checked:
    /// - a symlink is already pointing at the real home; nothing to do;
    /// - the real home lacking the name is the whole point — move it, whether
    ///   it is a file or a directory;
    /// - the real home *having* it means the agent replaced a link of ours by
    ///   rename, so the newer of the two wins and only for a plain file. An
    ///   older one is left for the linking pass to quarantine, which keeps it
    ///   recoverable instead of overwriting a login made outside since.
    fn adopt_agent_writes(&self) {
        let Some(real) = &self.real else {
            return;
        };
        for name in self.adopted {
            if self.is_ours(OsStr::new(name)) {
                tracing::error!(
                    "refusing to adopt {name}: captain-miao writes it, and moving it into the \
                     real home would install our hooks globally"
                );
                continue;
            }
            let synth = self.dir.join(name);
            let Ok(meta) = std::fs::symlink_metadata(&synth) else {
                continue; // the agent never wrote here
            };
            if meta.file_type().is_symlink() {
                continue; // already resolves into the real home
            }
            let dest = real.join(name);
            let adopt = match std::fs::symlink_metadata(&dest) {
                Err(_) => true,
                Ok(_) => meta.is_file() && is_newer(&meta, &dest),
            };
            if !adopt {
                continue;
            }
            // The real home may not exist at all yet — a directory-only
            // creation, and only ever on the path where we have the agent's own
            // state to put in it.
            let _ = std::fs::create_dir_all(real);
            match std::fs::rename(&synth, &dest) {
                Ok(()) => tracing::info!(
                    "adopted {} into {}: the agent created it inside the synthetic home",
                    name,
                    dest.display()
                ),
                // Nothing is lost by failing: the entry stays where the agent
                // put it and the next launch tries again.
                Err(e) => tracing::warn!("could not adopt {} into {}: {e}", name, dest.display()),
            }
        }
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
/// Whether `meta` is strictly newer than whatever is at `other`. Conservative
/// on any unreadable timestamp: "not newer" leaves the real home's copy alone,
/// and the entry stays recoverable.
fn is_newer(meta: &std::fs::Metadata, other: &Path) -> bool {
    let Ok(ours) = meta.modified() else {
        return false;
    };
    let Ok(theirs) = std::fs::metadata(other).and_then(|m| m.modified()) else {
        return false;
    };
    ours > theirs
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch (real, synth) pair under the OS temp dir, cleaned on drop.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("cm-synth-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("real")).unwrap();
            Scratch { root }
        }
        fn real(&self) -> PathBuf {
            self.root.join("real")
        }
        fn synth(&self) -> PathBuf {
            self.root.join("synth")
        }
        fn home(&self, prune: bool) -> SynthHome<'static> {
            self.adopting(prune, &[])
        }
        fn adopting(&self, prune: bool, adopted: &'static [&'static str]) -> SynthHome<'static> {
            SynthHome {
                dir: self.synth(),
                real: Some(self.real()),
                owned: &["captain-miao.js"],
                copied: &[],
                adopted,
                prune,
            }
        }
        /// Write `body` into the synthetic home as a **real** entry, the way an
        /// agent that found no symlink there would.
        fn agent_wrote(&self, name: &str, body: &str) -> PathBuf {
            std::fs::create_dir_all(self.synth()).unwrap();
            let path = self.synth().join(name);
            std::fs::write(&path, body).unwrap();
            path
        }
    }

    /// Stamp an explicit mtime, so the newer-wins rule is tested by intent
    /// rather than by how fast two writes happened to land.
    fn set_mtime(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The loader-scanned-collection case ([`SynthHome::prune`]): a plugin the
    /// user deletes must not leave a dangling import behind in the mirror.
    #[test]
    fn a_pruning_mirror_drops_links_to_deleted_real_entries() {
        let s = Scratch::new("prune");
        std::fs::write(s.real().join("user.js"), "x").unwrap();
        let home = s.home(true);
        home.ensure().unwrap();
        home.write_owned("captain-miao.js", "ours").unwrap();
        assert!(s.synth().join("user.js").symlink_metadata().is_ok());

        std::fs::remove_file(s.real().join("user.js")).unwrap();
        home.ensure().unwrap();
        assert!(
            s.synth().join("user.js").symlink_metadata().is_err(),
            "stale link survived the prune"
        );
        // Our own file is never a prune candidate.
        assert_eq!(
            std::fs::read_to_string(s.synth().join("captain-miao.js")).unwrap(),
            "ours"
        );
    }

    /// Only exact mints are pruned: a foreign symlink (even dangling) and a
    /// plain file the agent shadowed in are not ours to judge.
    #[test]
    fn a_pruning_mirror_keeps_foreign_links_and_shadows() {
        let s = Scratch::new("keep");
        let home = s.home(true);
        home.ensure().unwrap();
        let foreign = s.synth().join("elsewhere.js");
        std::os::unix::fs::symlink(s.root.join("nowhere"), &foreign).unwrap();
        std::fs::write(s.synth().join("shadow.js"), "agent-made").unwrap();

        home.ensure().unwrap();
        assert!(foreign.symlink_metadata().is_ok());
        assert!(s.synth().join("shadow.js").symlink_metadata().is_ok());
    }

    /// A shadow is replaced by the link but its contents survive under a
    /// quarantine name — a whole session tree minted into the synthetic home is
    /// recoverable by hand, where the old `remove_dir_all` destroyed it.
    #[test]
    fn a_replaced_shadow_is_quarantined_not_destroyed() {
        let s = Scratch::new("quarantine");
        let home = s.home(false);
        home.ensure().unwrap();

        // The agent minted a directory in the synthetic home before the real
        // home had the name; then the real home grew it.
        std::fs::create_dir_all(s.synth().join("sessions")).unwrap();
        std::fs::write(s.synth().join("sessions/transcript.jsonl"), "turns").unwrap();
        std::fs::create_dir_all(s.real().join("sessions")).unwrap();

        home.ensure().unwrap();
        // The name now resolves through a link into the real home…
        assert_eq!(
            std::fs::read_link(s.synth().join("sessions")).unwrap(),
            s.real().join("sessions")
        );
        // …and the shadow's contents sit in a `.shadow-*` sibling.
        let quarantined: Vec<_> = std::fs::read_dir(s.synth())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".shadow-sessions-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(
            std::fs::read_to_string(quarantined[0].path().join("transcript.jsonl")).unwrap(),
            "turns"
        );
    }

    /// The state-mirror case ([`SynthHome::prune`] off): a dangling link stays,
    /// so an agent recreating the file writes through it into the real home.
    /// The first-login case. The real home never had the file, so the linking
    /// pass — which walks the *real* home — could not have mirrored it, and the
    /// agent's own write landed here. It has to end up where a bare run of that
    /// agent will find it.
    #[test]
    fn an_agent_file_the_real_home_never_had_is_adopted_into_it() {
        let s = Scratch::new("adopt-new");
        s.agent_wrote("auth.json", "token-1");

        s.adopting(false, &["auth.json"]).ensure().unwrap();

        assert_eq!(
            std::fs::read_to_string(s.real().join("auth.json")).unwrap(),
            "token-1",
            "the agent's credentials belong in the real home"
        );
        assert_eq!(
            std::fs::read_link(s.synth().join("auth.json")).unwrap(),
            s.real().join("auth.json"),
            "and the mirror must point at them, so the next session writes through"
        );
    }

    /// A token refreshed inside a session is the newer one, and the real home's
    /// copy is stale by definition — the agent replaced our symlink by rename.
    #[test]
    fn a_newer_agent_write_replaces_the_real_one() {
        let s = Scratch::new("adopt-newer");
        std::fs::write(s.real().join("auth.json"), "old").unwrap();
        set_mtime(&s.real().join("auth.json"), 1_000);
        let synth = s.agent_wrote("auth.json", "refreshed");
        set_mtime(&synth, 2_000);

        s.adopting(false, &["auth.json"]).ensure().unwrap();

        assert_eq!(
            std::fs::read_to_string(s.real().join("auth.json")).unwrap(),
            "refreshed"
        );
    }

    /// The other order, which is a login made *outside* captain-miao since. The
    /// real home wins and the stale entry is left to the quarantine pass, so it
    /// is recoverable rather than overwritten.
    #[test]
    fn an_older_agent_write_never_overwrites_a_newer_real_one() {
        let s = Scratch::new("adopt-older");
        std::fs::write(s.real().join("auth.json"), "logged-in-outside").unwrap();
        set_mtime(&s.real().join("auth.json"), 2_000);
        let synth = s.agent_wrote("auth.json", "stale");
        set_mtime(&synth, 1_000);

        s.adopting(false, &["auth.json"]).ensure().unwrap();

        assert_eq!(
            std::fs::read_to_string(s.real().join("auth.json")).unwrap(),
            "logged-in-outside"
        );
        let quarantined: Vec<_> = std::fs::read_dir(s.synth())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".shadow-auth.json")
            })
            .collect();
        assert_eq!(
            quarantined.len(),
            1,
            "the stale copy is set aside, not deleted"
        );
    }

    /// Antigravity's shape: the agent's whole state tree, minted here because
    /// the real home had no such directory to mirror.
    #[test]
    fn a_directory_the_real_home_lacks_is_adopted_whole() {
        let s = Scratch::new("adopt-dir");
        std::fs::create_dir_all(s.synth().join("state/logs")).unwrap();
        std::fs::write(s.synth().join("state/logs/a.log"), "entry").unwrap();

        s.adopting(false, &["state"]).ensure().unwrap();

        assert_eq!(
            std::fs::read_to_string(s.real().join("state/logs/a.log")).unwrap(),
            "entry"
        );
        assert!(s.synth().join("state").is_symlink());
    }

    /// The rule, enforced rather than documented: a backend that lists one of
    /// *our* files as adopted must not get it, because moving it would install
    /// captain-miao's hooks into the user's real home — where every session run
    /// outside captain-miao would then fire them at a socket that isn't there.
    #[test]
    fn a_file_captain_miao_writes_is_never_adopted() {
        let s = Scratch::new("adopt-ours");
        // `captain-miao.js` is this fixture's `owned` entry.
        s.agent_wrote("captain-miao.js", "our plugin");

        s.adopting(false, &["captain-miao.js"]).ensure().unwrap();

        assert!(
            !s.real().join("captain-miao.js").exists(),
            "an entry captain-miao owns must never reach the real home"
        );
        assert_eq!(
            std::fs::read_to_string(s.synth().join("captain-miao.js")).unwrap(),
            "our plugin",
            "and it stays where it was"
        );
    }

    #[test]
    fn a_state_mirror_keeps_dangling_links() {
        let s = Scratch::new("state");
        std::fs::write(s.real().join("sessions.json"), "x").unwrap();
        let home = s.home(false);
        home.ensure().unwrap();

        std::fs::remove_file(s.real().join("sessions.json")).unwrap();
        home.ensure().unwrap();
        assert!(s.synth().join("sessions.json").symlink_metadata().is_ok());
    }
}
