//! Version-control status and the two commands the dashboard is willing to run.
//!
//! The snapshot is only what changes a push or pull decision. It does not list
//! paths or commits. v1 fills it for git. A checkout owned by another system
//! comes back [`VcsOutcome::Unsupported`] with that system's name, so the panel
//! can say so without learning the system.
//!
//! Every git process is off the caller's thread only in the sense that the
//! caller must not be the UI thread: these functions block. They also share one
//! lock, so a slow status cannot overlap a push in this process. The lock is
//! not held by anything but these functions.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

mod command;
mod process;
pub use command::{
    PreparedVcsPull, PreparedVcsPush, VcsCommit, execute_pull, execute_pull_with_agent,
    execute_push, execute_push_with_agent, prepare_pull, prepare_pull_with_agent, prepare_push,
};
use process::{GitOut, RunFail, run_git, run_git_read};

pub const STATUS_LIMIT: Duration = Duration::from_secs(30);
const FOLLOWUP_LIMIT: Duration = Duration::from_secs(2);
pub const COMMAND_LIMIT: Duration = Duration::from_secs(60);
const OUTPUT_CAP: u64 = 64 * 1024;

static GIT: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsOutcome {
    Ready,
    NotACheckout,
    Unsupported,
    Missing,
    Denied,
    TimedOut,
    NoTool,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsSnapshot {
    pub outcome: VcsOutcome,
    /// Set when a checkout was recognized, including [`VcsOutcome::Unsupported`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Branch or bookmark. Absent unless the outcome is [`VcsOutcome::Ready`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default)]
    pub detached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The tracking branch is configured, but its ref no longer exists.
    #[serde(default)]
    pub upstream_gone: bool,
    /// Commits here that the upstream does not have. Zero when there is no upstream.
    #[serde(default)]
    pub ahead: u32,
    /// Commits the upstream has that are not here. Zero when there is no upstream.
    #[serde(default)]
    pub behind: u32,
    /// `"merge"`, `"rebase"`, and so on, when one is in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// Worktree or workspace name, when this is not the main checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub dirty: bool,
    #[serde(default)]
    pub conflicts: bool,
}

impl Default for VcsSnapshot {
    fn default() -> Self {
        Self::outcome(VcsOutcome::Error)
    }
}

impl VcsSnapshot {
    fn outcome(outcome: VcsOutcome) -> Self {
        Self {
            outcome,
            system: None,
            head: None,
            detached: false,
            upstream: None,
            upstream_gone: false,
            ahead: 0,
            behind: 0,
            operation: None,
            workspace: None,
            dirty: false,
            conflicts: false,
        }
    }

    fn unsupported(system: &str) -> Self {
        let mut snap = Self::outcome(VcsOutcome::Unsupported);
        snap.system = Some(system.to_string());
        snap
    }
}

/// Status of `cwd`. `cwd` may be host-canonical (`~/…`); it is expanded here.
pub fn status(cwd: &str) -> VcsSnapshot {
    status_with_deadline(cwd, Instant::now() + STATUS_LIMIT)
}

/// The caller starts the budget before queueing its worker.
pub fn status_with_deadline(cwd: &str, deadline: Instant) -> VcsSnapshot {
    let Ok(_guard) = lock_until(deadline) else {
        return VcsSnapshot::outcome(VcsOutcome::TimedOut);
    };
    status_locked(&expand(cwd), deadline)
}

fn lock_until(deadline: Instant) -> Result<MutexGuard<'static, ()>, RunFail> {
    loop {
        if Instant::now() >= deadline {
            return Err(RunFail::TimedOut);
        }
        match GIT.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(err)) => return Ok(err.into_inner()),
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn expand(cwd: &str) -> PathBuf {
    PathBuf::from(crate::paths::expand_home(cwd, &crate::paths::host_home()))
}

fn status_locked(cwd: &Path, deadline: Instant) -> VcsSnapshot {
    let Some(system) = detect(cwd) else {
        return if cwd.exists() {
            VcsSnapshot::outcome(VcsOutcome::NotACheckout)
        } else if cwd
            .metadata()
            .is_err_and(|err| err.kind() == std::io::ErrorKind::PermissionDenied)
        {
            VcsSnapshot::outcome(VcsOutcome::Denied)
        } else {
            VcsSnapshot::outcome(VcsOutcome::Missing)
        };
    };
    if system != "git" {
        return VcsSnapshot::unsupported(system);
    }
    let output = match run_git_read(
        cwd,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-b",
            "--ignore-submodules=dirty",
            "-z",
        ],
        deadline,
        read_status,
    ) {
        Ok(output) => output,
        Err(RunFail::NoTool) => return VcsSnapshot::outcome(VcsOutcome::NoTool),
        Err(RunFail::TimedOut) => return VcsSnapshot::outcome(VcsOutcome::TimedOut),
        Err(RunFail::Denied) => return VcsSnapshot::outcome(VcsOutcome::Denied),
        Err(RunFail::Message(_)) => return VcsSnapshot::outcome(VcsOutcome::Error),
    };
    if !output.status_ok {
        return VcsSnapshot::outcome(VcsOutcome::Error);
    }
    let mut snap = output.stdout;
    if snap.outcome != VcsOutcome::Ready {
        return snap;
    }
    snap.system = Some("git".to_string());
    snap.outcome = VcsOutcome::Ready;
    let follow = deadline.min(Instant::now() + FOLLOWUP_LIMIT);
    snap.operation = operation(cwd, follow);
    snap.workspace = workspace(cwd, follow);
    snap
}

fn outcome_message(snap: &VcsSnapshot) -> String {
    match snap.outcome {
        VcsOutcome::Unsupported => format!(
            "{} is not supported",
            snap.system.as_deref().unwrap_or("this system")
        ),
        VcsOutcome::NotACheckout => "not a version-control checkout".to_string(),
        VcsOutcome::Missing => "directory is gone".to_string(),
        VcsOutcome::Denied => "permission denied".to_string(),
        VcsOutcome::TimedOut => "timed out".to_string(),
        VcsOutcome::NoTool => "git is not on PATH".to_string(),
        VcsOutcome::Error => "git failed".to_string(),
        VcsOutcome::Ready => "ready".to_string(),
    }
}

fn finish_command(result: Result<GitOut, RunFail>, ok_word: &str) -> Result<String, String> {
    match result {
        Ok(output) if output.status_ok => Ok(ok_word.to_string()),
        Ok(output) => Err(first_line(&output.stderr).unwrap_or_else(|| "git failed".to_string())),
        Err(RunFail::TimedOut) => Err(
            "timed out; outcome unknown — verify the checkout and remote before retrying"
                .to_string(),
        ),
        Err(RunFail::NoTool) => Err("git is not on PATH".to_string()),
        Err(RunFail::Denied) => Err("permission denied".to_string()),
        Err(RunFail::Message(message)) => Err(message),
    }
}

fn first_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let home = crate::paths::host_home();
    Some(crate::paths::collapse_home(line.trim(), &home))
}

fn read_capped(pipe: Option<impl Read>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.by_ref().take(OUTPUT_CAP).read_to_end(&mut buf);
    // Keep draining after the retention cap: closing early gives Git SIGPIPE.
    let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    buf
}

/// Nearest checkout, preferring `.jj` over `.git` in the same directory.
fn detect(cwd: &Path) -> Option<&'static str> {
    if !cwd.exists() {
        return None;
    }
    let mut dir = cwd.to_path_buf();
    loop {
        for (name, system) in [
            (".jj", "jj"),
            (".git", "git"),
            (".hg", "hg"),
            (".sl", "sapling"),
        ] {
            if dir.join(name).exists() {
                return Some(system);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
fn parse_status(bytes: &[u8]) -> VcsSnapshot {
    read_status(bytes)
}

/// Summarize every record without retaining the checkout's complete file list.
fn read_status(reader: impl Read) -> VcsSnapshot {
    let mut snap = VcsSnapshot::outcome(VcsOutcome::Ready);
    let mut records = BufReader::new(reader).split(0);
    while let Some(record) = records.next() {
        let Ok(record) = record else {
            return VcsSnapshot::outcome(VcsOutcome::Error);
        };
        let record = String::from_utf8_lossy(&record);
        let record = record.trim_end_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        if let Some(header) = record.strip_prefix("## ") {
            apply_header(&mut snap, header);
            continue;
        }
        snap.dirty = true;
        let mut chars = record.chars();
        let a = chars.next().unwrap_or(' ');
        let b = chars.next().unwrap_or(' ');
        if is_unmerged(a, b) {
            snap.conflicts = true;
        }
        // Porcelain -z encodes a rename/copy as status + destination + NUL +
        // source + NUL. The source is a pathname, even if it starts with "UU".
        if (matches!(a, 'R' | 'C') || matches!(b, 'R' | 'C'))
            && !matches!(records.next(), Some(Ok(_)))
        {
            return VcsSnapshot::outcome(VcsOutcome::Error);
        }
    }
    snap
}

fn apply_header(snap: &mut VcsSnapshot, header: &str) {
    let (name, tracking) = header
        .split_once(" [")
        .map(|(name, rest)| (name, Some(rest.trim_end_matches(']'))))
        .unwrap_or((header, None));
    if name.starts_with("HEAD (no branch)") {
        snap.detached = true;
        snap.head = None;
    } else if let Some((head, upstream)) = name.split_once("...") {
        snap.head = Some(head.to_string());
        if !upstream.is_empty() {
            snap.upstream = Some(upstream.to_string());
        }
    } else if !name.is_empty() {
        snap.head = Some(name.to_string());
    }
    if let Some(tracking) = tracking {
        for part in tracking.split(',') {
            let part = part.trim();
            if part == "gone" {
                snap.upstream_gone = true;
            } else if let Some(n) = part.strip_prefix("ahead ") {
                snap.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                snap.behind = n.parse().unwrap_or(0);
            }
        }
    }
}

fn is_unmerged(a: char, b: char) -> bool {
    matches!(
        (a, b),
        ('U', 'U') | ('A', 'A') | ('D', 'D') | ('A', 'U') | ('U', 'A') | ('D', 'U') | ('U', 'D')
    )
}

fn operation(cwd: &Path, deadline: Instant) -> Option<String> {
    for (path, name) in [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
    ] {
        if let Ok(output) = run_git(cwd, &["rev-parse", "--git-path", path], deadline)
            && output.status_ok
        {
            let rel = String::from_utf8_lossy(&output.stdout);
            let rel = rel.trim();
            if !rel.is_empty() && cwd.join(rel).exists() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn workspace(cwd: &Path, deadline: Instant) -> Option<String> {
    let git_dir = git_line(cwd, &["rev-parse", "--git-dir"], deadline)?;
    let common = git_line(cwd, &["rev-parse", "--git-common-dir"], deadline)?;
    if git_dir == common {
        return None;
    }
    let top = git_line(cwd, &["rev-parse", "--show-toplevel"], deadline)?;
    Path::new(&top)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn git_line(cwd: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    let output = run_git(cwd, args, deadline).ok()?;
    if !output.status_ok {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

fn remotes(cwd: &Path, deadline: Instant) -> Result<Vec<String>, String> {
    let output = run_git(cwd, &["remote"], deadline).map_err(|err| match err {
        RunFail::NoTool => "git is not on PATH".to_string(),
        RunFail::TimedOut => "timed out".to_string(),
        RunFail::Denied => "permission denied".to_string(),
        RunFail::Message(message) => message,
    })?;
    if !output.status_ok {
        return Err("git remote failed".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(cwd: &str) -> Result<String, String> {
        let plan = prepare_push(cwd, Instant::now() + COMMAND_LIMIT)?;
        execute_push(cwd, &plan, Instant::now() + COMMAND_LIMIT)
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn status_reports_branch_ahead_and_dirty() {
        let root = std::env::temp_dir().join(format!("cm-vcs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-m", "start"]);
        let bare = std::env::temp_dir().join(format!("cm-vcs-remote-{}.git", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare);
        git(&root, &["init", "--bare", bare.to_str().unwrap()]);
        git(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&root, &["push", "-u", "origin", "HEAD"]);

        let clean = status(root.to_str().unwrap());
        assert_eq!(clean.outcome, VcsOutcome::Ready);
        assert_eq!(clean.system.as_deref(), Some("git"));
        assert_eq!(clean.head.as_deref(), Some("main"));
        assert_eq!(clean.upstream.as_deref(), Some("origin/main"));
        assert_eq!(clean.ahead, 0);
        assert!(!clean.dirty);

        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        git(&root, &["commit", "-am", "edit"]);
        let ahead = status(root.to_str().unwrap());
        assert_eq!(ahead.ahead, 1);
        assert_eq!(ahead.behind, 0);
        assert!(!ahead.dirty);

        std::fs::write(root.join("a.txt"), "three\n").unwrap();
        let dirty = status(root.to_str().unwrap());
        assert!(dirty.dirty);
        assert_eq!(dirty.ahead, 1);

        let pushed = push(root.to_str().unwrap()).unwrap();
        assert_eq!(pushed, "pushed");
        let again = push(root.to_str().unwrap()).unwrap();
        assert_eq!(again, "pushed");

        git(&bare, &["update-ref", "-d", "refs/heads/main"]);
        git(&root, &["fetch", "--prune"]);
        let gone = status(root.to_str().unwrap());
        assert!(gone.upstream_gone);
        assert_eq!(push(root.to_str().unwrap()).unwrap(), "pushed");
        assert!(!status(root.to_str().unwrap()).upstream_gone);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn jj_checkout_is_unsupported() {
        let root = std::env::temp_dir().join(format!("cm-vcs-jj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let snap = status(root.to_str().unwrap());
        assert_eq!(snap.outcome, VcsOutcome::Unsupported);
        assert_eq!(snap.system.as_deref(), Some("jj"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_directory_is_not_a_timeout() {
        let snap = status("/no/such/captain-miao-vcs-dir");
        assert_eq!(snap.outcome, VcsOutcome::Missing);
    }

    #[test]
    fn header_parses_ahead_and_behind() {
        let snap = parse_status(b"## main...origin/main [ahead 2, behind 1]\0");
        assert_eq!(snap.head.as_deref(), Some("main"));
        assert_eq!(snap.upstream.as_deref(), Some("origin/main"));
        assert_eq!(snap.ahead, 2);
        assert_eq!(snap.behind, 1);
    }

    #[test]
    fn renamed_paths_are_not_status_records() {
        for source in ["AUTHORS", "## misleading...origin/other", "UU file"] {
            let bytes = format!("## main\0R  renamed\0{source}\0");
            let snap = parse_status(bytes.as_bytes());
            assert_eq!(snap.head.as_deref(), Some("main"));
            assert!(snap.dirty);
            assert!(!snap.conflicts, "rename source: {source}");
        }
        let snap = parse_status(b"## main\0R  renamed\0AUTHORS\0UU real-conflict\0");
        assert!(snap.conflicts);
    }

    #[test]
    fn capped_output_is_fully_drained() {
        let bytes = vec![b'x'; OUTPUT_CAP as usize + 4096];
        let mut reader = std::io::Cursor::new(&bytes);
        let retained = read_capped(Some(&mut reader));
        assert_eq!(retained.len(), OUTPUT_CAP as usize);
        assert_eq!(reader.position(), bytes.len() as u64);
    }

    #[test]
    fn large_status_output_remains_readable() {
        let root = std::env::temp_dir().join(format!("cm-vcs-large-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        for i in 0..1024 {
            std::fs::write(root.join(format!("{i:04}-{}", "x".repeat(90))), "").unwrap();
        }
        let snap = status(root.to_str().unwrap());
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(snap.outcome, VcsOutcome::Ready);
        assert!(snap.dirty);
    }

    #[test]
    fn gone_upstream_is_not_in_sync() {
        let snap = parse_status(b"## main...origin/main [gone]\0");
        assert!(snap.upstream_gone);
        let old: VcsSnapshot = serde_json::from_str(r#"{"outcome":"ready"}"#).unwrap();
        assert!(!old.upstream_gone);
    }

    #[test]
    fn status_keeps_conflicts_beyond_the_output_cap() {
        let mut bytes = b"## main\0".to_vec();
        for _ in 0..10_000 {
            bytes.extend_from_slice(b" M changed-file\0");
        }
        bytes.extend_from_slice(b"UU conflict\0");
        let snap = parse_status(&bytes);
        assert!(snap.conflicts);
    }
}
