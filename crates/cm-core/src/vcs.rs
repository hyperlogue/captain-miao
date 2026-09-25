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

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const STATUS_LIMIT: Duration = Duration::from_secs(30);
const FOLLOWUP_LIMIT: Duration = Duration::from_secs(2);
const COMMAND_LIMIT: Duration = Duration::from_secs(60);
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
    let _guard = GIT.lock().unwrap_or_else(|err| err.into_inner());
    status_locked(&expand(cwd))
}

/// Publish the current branch. No `--force`. With no upstream and exactly one
/// remote, sets that upstream. Returns a short message either way.
pub fn push(cwd: &str) -> Result<String, String> {
    let _guard = GIT.lock().unwrap_or_else(|err| err.into_inner());
    push_locked(&expand(cwd))
}

/// `git pull --ff-only`. A non-fast-forward leaves the tree untouched.
pub fn pull(cwd: &str) -> Result<String, String> {
    let _guard = GIT.lock().unwrap_or_else(|err| err.into_inner());
    pull_locked(&expand(cwd))
}

fn expand(cwd: &str) -> PathBuf {
    PathBuf::from(crate::paths::expand_home(cwd, &crate::paths::host_home()))
}

fn status_locked(cwd: &Path) -> VcsSnapshot {
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
    let deadline = Instant::now() + STATUS_LIMIT;
    let output = match run_git(
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
    let mut snap = parse_status(&output.stdout);
    snap.system = Some("git".to_string());
    snap.outcome = VcsOutcome::Ready;
    let follow = Instant::now() + FOLLOWUP_LIMIT;
    snap.operation = operation(cwd, follow);
    snap.workspace = workspace(cwd, follow);
    snap
}

fn push_locked(cwd: &Path) -> Result<String, String> {
    let snap = status_locked(cwd);
    if snap.outcome != VcsOutcome::Ready {
        return Err(outcome_message(&snap));
    }
    if snap.detached {
        return Err("detached; there is no branch to push".to_string());
    }
    if snap.operation.is_some() {
        return Err(format!(
            "{} in progress",
            snap.operation.unwrap_or_default()
        ));
    }
    if snap.upstream.is_some() && snap.ahead == 0 {
        return Ok("nothing to push".to_string());
    }
    let deadline = Instant::now() + COMMAND_LIMIT;
    let result = if snap.upstream.is_some() {
        run_git(cwd, &["--no-optional-locks", "push"], deadline)
    } else {
        let remotes = remotes(cwd, deadline)?;
        match remotes.len() {
            0 => return Err("no remote".to_string()),
            1 => run_git(
                cwd,
                &["--no-optional-locks", "push", "-u", &remotes[0], "HEAD"],
                deadline,
            ),
            _ => return Err(format!("several remotes: {}", remotes.join(", "))),
        }
    };
    finish_command(result, "pushed")
}

fn pull_locked(cwd: &Path) -> Result<String, String> {
    let snap = status_locked(cwd);
    if snap.outcome != VcsOutcome::Ready {
        return Err(outcome_message(&snap));
    }
    if snap.upstream.is_none() {
        return Err("no upstream".to_string());
    }
    if snap.operation.is_some() {
        return Err(format!(
            "{} in progress",
            snap.operation.unwrap_or_default()
        ));
    }
    let deadline = Instant::now() + COMMAND_LIMIT;
    finish_command(
        run_git(cwd, &["--no-optional-locks", "pull", "--ff-only"], deadline),
        "pulled",
    )
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
        Err(RunFail::TimedOut) => Err("timed out".to_string()),
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

enum RunFail {
    NoTool,
    TimedOut,
    Denied,
    Message(String),
}

struct GitOut {
    status_ok: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_git(cwd: &Path, args: &[&str], deadline: Instant) -> Result<GitOut, RunFail> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                RunFail::NoTool
            } else if err.kind() == std::io::ErrorKind::PermissionDenied {
                RunFail::Denied
            } else {
                RunFail::Message(err.to_string())
            }
        })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = thread::spawn(move || read_capped(stdout));
    let err_thread = thread::spawn(move || read_capped(stderr));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_thread.join();
                let _ = err_thread.join();
                return Err(RunFail::TimedOut);
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => return Err(RunFail::Message(err.to_string())),
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(GitOut {
        status_ok: status.success(),
        stdout,
        stderr,
    })
}

fn read_capped(pipe: Option<impl Read>) -> Vec<u8> {
    let Some(pipe) = pipe else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.take(OUTPUT_CAP).read_to_end(&mut buf);
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

fn parse_status(bytes: &[u8]) -> VcsSnapshot {
    let mut snap = VcsSnapshot::outcome(VcsOutcome::Ready);
    let text = String::from_utf8_lossy(bytes);
    for record in text.split('\0') {
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
            if let Some(n) = part.strip_prefix("ahead ") {
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
        assert_eq!(again, "nothing to push");

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
}
