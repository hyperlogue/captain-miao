//! Network evidence is separate from local tracking refs. Fetching an object
//! never moves a branch, writes FETCH_HEAD, or changes another tool's refs.
//! Cache by repository and upstream, so worktrees and clients share the budget.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use command::{line, remote_tip};
use process::run_git_with_agent;

pub const REMOTE_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCheck {
    #[default]
    NotChecked,
    Checked {
        at: u64,
    },
    Failed,
}

impl RemoteCheck {
    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::NotChecked => Some("not checked"),
            Self::Failed => Some("check failed"),
            Self::Checked { at }
                if now().saturating_sub(*at) >= REMOTE_REFRESH_INTERVAL.as_secs() =>
            {
                Some("stale")
            }
            Self::Checked { .. } => None,
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct Upstream {
    common_dir: String,
    url: String,
    target: String,
}

impl Upstream {
    fn read(cwd: &Path, deadline: Instant) -> Result<Self, String> {
        let branch = line(cwd, &["symbolic-ref", "--quiet", "HEAD"], deadline)?;
        let fields = line(
            cwd,
            &[
                "for-each-ref",
                "--format=%(upstream:remotename)%00%(upstream:remoteref)",
                &branch,
            ],
            deadline,
        )?;
        let (remote, target) = fields.split_once('\0').ok_or("no upstream")?;
        if remote.is_empty() || !target.starts_with("refs/heads/") {
            return Err("no upstream branch".into());
        }
        let url = if remote == "." {
            ".".into()
        } else {
            line(cwd, &["remote", "get-url", remote], deadline)?
        };
        // Relative filesystem URLs resolve from this worktree, unlike SSH or
        // HTTP URLs. Do not share their evidence with a different directory.
        let url = if !url.contains(':') && !Path::new(&url).is_absolute() {
            cwd.join(&url).to_string_lossy().into_owned()
        } else {
            url
        };
        let common_dir = line(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            deadline,
        )?;
        Ok(Self {
            common_dir,
            url,
            target: target.into(),
        })
    }

    fn fetch(&self, cwd: &Path, socket: Option<&str>, deadline: Instant) -> Result<String, String> {
        let tip = remote_tip(cwd, &self.url, &self.target, socket, deadline)?;
        finish_command(
            run_git_with_agent(
                cwd,
                &[
                    "-c",
                    "gc.auto=0",
                    "fetch",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--no-write-fetch-head",
                    "--",
                    &self.url,
                    &tip,
                ],
                socket,
                deadline,
            ),
            "fetched",
        )?;
        Ok(tip)
    }
}

struct Checked {
    attempted: Instant,
    at: u64,
    tip: Result<String, String>,
}

static CACHE: OnceLock<Mutex<HashMap<Upstream, Checked>>> = OnceLock::new();

// Commands already hold GIT. Their outcome may have changed remote history,
// even when a network error made that outcome uncertain.
pub(super) fn invalidate(cwd: &Path, deadline: Instant) {
    let Some(cache) = CACHE.get() else { return };
    let Ok(common_dir) = line(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        deadline,
    ) else {
        return;
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|upstream, _| upstream.common_dir != common_dir);
}

/// Read local state on every poll, but contact the upstream only when requested
/// and outside its shared cooldown. Old clients retain the local-only API.
pub fn dashboard_status(
    cwd: &str,
    refresh_remote: bool,
    ssh_auth_sock: Option<&str>,
    deadline: Instant,
) -> VcsSnapshot {
    let Ok(_guard) = lock_until(deadline) else {
        return VcsSnapshot::outcome(VcsOutcome::TimedOut);
    };
    let cwd = expand(cwd);
    let mut snap = status_locked(&cwd, deadline);
    if snap.outcome != VcsOutcome::Ready || snap.detached || snap.upstream.is_none() {
        return snap;
    }
    let Ok(upstream) = Upstream::read(&cwd, deadline) else {
        snap.remote_check = RemoteCheck::Failed;
        return snap;
    };
    let mut cache = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Bound retained entries without affecting a currently watched repository.
    cache.retain(|_, checked| checked.attempted.elapsed() < Duration::from_secs(3600));
    let due = cache
        .get(&upstream)
        .is_none_or(|checked| checked.attempted.elapsed() >= REMOTE_REFRESH_INTERVAL);
    if refresh_remote && due {
        let socket = ssh_auth_sock.map(expand);
        let socket = socket.as_ref().map(|path| path.to_string_lossy());
        let attempted = Instant::now();
        // Preserve the local result even if the network consumes its budget.
        let tip = upstream.fetch(&cwd, socket.as_deref(), deadline);
        cache.insert(
            upstream,
            Checked {
                attempted,
                at: now(),
                tip,
            },
        );
        if Instant::now() >= deadline {
            snap.remote_check = RemoteCheck::Failed;
            return snap;
        }
        snap = status_locked(&cwd, deadline);
        if snap.outcome != VcsOutcome::Ready || snap.detached || snap.upstream.is_none() {
            return snap;
        }
    }
    // Other processes can switch branches/configuration during a network call.
    // Resolve again before attaching evidence to the newly read local snapshot.
    let Ok(upstream) = Upstream::read(&cwd, deadline) else {
        return snap;
    };
    let Some(checked) = cache.get(&upstream) else {
        return snap;
    };
    let Ok(tip) = &checked.tip else {
        snap.remote_check = RemoteCheck::Failed;
        return snap;
    };
    let counts = line(
        &cwd,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{tip}"),
            "--",
        ],
        deadline,
    );
    let counts = counts.ok().and_then(|counts| {
        let (ahead, behind) = counts.split_once('\t')?;
        Some((ahead.parse::<u32>().ok()?, behind.parse::<u32>().ok()?))
    });
    if let Some((ahead, behind)) = counts {
        snap.ahead = ahead;
        snap.behind = behind;
        snap.upstream_gone = false;
        snap.remote_check = RemoteCheck::Checked { at: checked.at };
    } else {
        snap.remote_check = RemoteCheck::Failed;
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_expires_and_old_snapshots_are_unverified() {
        assert_eq!(RemoteCheck::Checked { at: now() }.label(), None);
        assert_eq!(
            RemoteCheck::Checked { at: now() - 61 }.label(),
            Some("stale")
        );
        let old: VcsSnapshot =
            serde_json::from_str(r#"{"outcome":"ready","upstream":"origin/main"}"#).unwrap();
        assert_eq!(old.remote_check.label(), Some("not checked"));
    }
}
