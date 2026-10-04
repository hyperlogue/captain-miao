//! A prepared command describes one immutable Git target. Old wire commands cannot
//! express this contract and are rejected by the server.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::process::run_git_with_agent;
use super::{
    VcsOutcome, expand, finish_command, lock_until, outcome_message, remotes, run_git,
    status_locked,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsPlan {
    pub push: bool,
    pub branch: String,
    pub commit: String,
    pub remote: String,
    pub target_ref: String,
    /// Pull's fetched commit. Push always sends `commit`, never a moving HEAD.
    pub target_commit: Option<String>,
    /// An opaque digest, so host paths and credential-bearing URLs stay local.
    identity: String,
    set_upstream: bool,
}

impl VcsPlan {
    pub fn destination(&self) -> String {
        format!(
            "{}/{}",
            self.remote,
            self.target_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(&self.target_ref)
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(push: bool) -> Self {
        Self {
            push,
            branch: "main".into(),
            commit: "a".repeat(40),
            remote: "origin".into(),
            target_ref: "refs/heads/main".into(),
            target_commit: (!push).then(|| "b".repeat(40)),
            identity: "test".into(),
            set_upstream: false,
        }
    }
}

struct Checkout {
    plan: VcsPlan,
    url: String,
    tracking: Vec<String>,
}

const CHANGED: &str = "checkout or destination changed; run the command again";

pub fn prepare(cwd: &str, push: bool, deadline: Instant) -> Result<VcsPlan, String> {
    prepare_with_agent(cwd, push, None, deadline)
}

/// Prepare using this client's agent without changing the daemon environment.
pub fn prepare_with_agent(
    cwd: &str,
    push: bool,
    ssh_auth_sock: Option<&str>,
    deadline: Instant,
) -> Result<VcsPlan, String> {
    let ssh_auth_sock =
        ssh_auth_sock.map(|socket| crate::paths::expand_home(socket, &crate::paths::host_home()));
    let ssh_auth_sock = ssh_auth_sock.as_deref();
    let _guard = lock_until(deadline).map_err(failure)?;
    let cwd = expand(cwd)
        .canonicalize()
        .map_err(|_| "directory is gone")?;
    let mut checkout = inspect(&cwd, push, deadline)?;
    if !push {
        let commit = remote_tip(
            &cwd,
            &checkout.url,
            &checkout.plan.target_ref,
            ssh_auth_sock,
            deadline,
        )?;
        // Fetch only the announced object. Another agent's FETCH_HEAD cannot
        // replace the candidate, and no branch/worktree moves during preparation.
        finish_command(
            run_git_with_agent(
                &cwd,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--no-write-fetch-head",
                    "--",
                    &checkout.url,
                    &commit,
                ],
                ssh_auth_sock,
                deadline,
            ),
            "fetched",
        )?;
        if inspect(&cwd, push, deadline)?.plan != checkout.plan {
            return Err(CHANGED.into());
        }
        if !run_git(
            &cwd,
            &[
                "merge-base",
                "--is-ancestor",
                &checkout.plan.commit,
                &commit,
            ],
            deadline,
        )
        .map_err(failure)?
        .status_ok
        {
            return Err("branches have diverged; a fast-forward pull is not possible".into());
        }
        checkout.plan.target_commit = Some(commit);
    }
    Ok(checkout.plan)
}

pub fn execute(cwd: &str, expected: &VcsPlan, deadline: Instant) -> Result<String, String> {
    execute_with_agent(cwd, expected, None, deadline)
}

/// Execute using this client's current agent, which may change after reconnect.
pub fn execute_with_agent(
    cwd: &str,
    expected: &VcsPlan,
    ssh_auth_sock: Option<&str>,
    deadline: Instant,
) -> Result<String, String> {
    let ssh_auth_sock =
        ssh_auth_sock.map(|socket| crate::paths::expand_home(socket, &crate::paths::host_home()));
    let ssh_auth_sock = ssh_auth_sock.as_deref();
    let _guard = lock_until(deadline).map_err(failure)?;
    let cwd = expand(cwd).canonicalize().map_err(|_| CHANGED)?;
    let checkout = validate(&cwd, expected, deadline)?;
    let tracking: Vec<_> = checkout
        .tracking
        .iter()
        .map(|name| (name.as_str(), optional_ref(&cwd, name, deadline)))
        .collect();
    if expected.push {
        let refspec = format!("{}:{}", expected.commit, expected.target_ref);
        // Explicit URL and OID prevent config changes and moving local refs
        // from redirecting this invocation or publishing unprepared commits.
        finish_command(
            run_git_with_agent(
                &cwd,
                &[
                    "-c",
                    "push.followTags=false",
                    "-c",
                    "push.recurseSubmodules=no",
                    "push",
                    "--no-force",
                    "--no-follow-tags",
                    "--recurse-submodules=no",
                    "--",
                    &checkout.url,
                    &refspec,
                ],
                ssh_auth_sock,
                deadline,
            ),
            "pushed",
        )?;
        let refreshed = refresh_tracking(&cwd, &tracking, &expected.commit, deadline);
        if expected.set_upstream {
            let prefix = format!("branch.{}", expected.branch);
            let unchanged =
                matches!(
                    config(&cwd, &format!("{prefix}.remote"), deadline),
                    Ok(None)
                ) && matches!(config(&cwd, &format!("{prefix}.merge"), deadline), Ok(None));
            if !unchanged {
                return Ok("pushed; upstream configuration changed, so it was preserved".into());
            }
            for (key, value) in [
                (format!("{prefix}.remote"), &expected.remote),
                (format!("{prefix}.merge"), &expected.target_ref),
            ] {
                if success(&cwd, &["config", &key, value], deadline).is_err() {
                    return Ok(
                        "pushed; could not finish setting the upstream — check Git configuration"
                            .into(),
                    );
                }
            }
        }
        Ok(if refreshed {
            "pushed"
        } else {
            "pushed; refresh tracking status before another command"
        }
        .into())
    } else {
        let commit = expected
            .target_commit
            .as_deref()
            .filter(|oid| valid_oid(oid))
            .ok_or(CHANGED)?;
        if remote_tip(
            &cwd,
            &checkout.url,
            &expected.target_ref,
            ssh_auth_sock,
            deadline,
        )? != commit
        {
            return Err(CHANGED.into());
        }
        // The network wait is a second opportunity for an agent to change the
        // checkout. Revalidate after it, immediately before touching the tree.
        validate(&cwd, expected, deadline)?;
        finish_command(
            run_git(
                &cwd,
                &[
                    "-c",
                    "merge.autostash=false",
                    "merge",
                    "--ff-only",
                    "--no-edit",
                    "--no-autostash",
                    commit,
                ],
                deadline,
            ),
            "pulled",
        )?;
        let refreshed = refresh_tracking(&cwd, &tracking, commit, deadline);
        Ok(if refreshed {
            "pulled"
        } else {
            "pulled; refresh tracking status before another command"
        }
        .into())
    }
}

fn refresh_tracking(
    cwd: &Path,
    refs: &[(&str, Option<String>)],
    commit: &str,
    deadline: Instant,
) -> bool {
    let mut refreshed = true;
    for (name, old) in refs {
        let old = old.clone().unwrap_or_else(|| "0".repeat(commit.len()));
        // Do not overwrite a newer fetch by another process.
        refreshed &= success(cwd, &["update-ref", name, commit, &old], deadline).is_ok();
    }
    refreshed
}

fn validate(cwd: &Path, expected: &VcsPlan, deadline: Instant) -> Result<Checkout, String> {
    let mut current = inspect(cwd, expected.push, deadline)?;
    current.plan.target_commit = expected.target_commit.clone();
    if current.plan != *expected {
        return Err(CHANGED.into());
    }
    Ok(current)
}

fn inspect(cwd: &Path, push: bool, deadline: Instant) -> Result<Checkout, String> {
    let snapshot = status_locked(cwd, deadline);
    if snapshot.outcome != VcsOutcome::Ready {
        return Err(outcome_message(&snapshot));
    }
    if snapshot.detached {
        return Err("detached; there is no branch to update".into());
    }
    if let Some(operation) = snapshot.operation {
        return Err(format!("{operation} in progress"));
    }
    if snapshot.conflicts {
        return Err("resolve conflicts before pushing or pulling".into());
    }
    let branch_ref = line(cwd, &["symbolic-ref", "--quiet", "HEAD"], deadline)?;
    let branch = branch_ref
        .strip_prefix("refs/heads/")
        .ok_or("not on a local branch")?
        .to_string();
    let commit = line(cwd, &["rev-parse", "--verify", "HEAD"], deadline)?;
    if !valid_oid(&commit) {
        return Err("branch has no commit to publish".into());
    }
    let prefix = format!("branch.{branch}");
    let upstream_remote = config(cwd, &format!("{prefix}.remote"), deadline)?;
    let upstream_ref = config(cwd, &format!("{prefix}.merge"), deadline)?;
    let branch_push = config(cwd, &format!("{prefix}.pushRemote"), deadline)?;
    let default_push = config(cwd, "remote.pushDefault", deadline)?;
    let push_default = config(cwd, "push.default", deadline)?;
    let available = remotes(cwd, deadline)?;
    let remote = if push {
        branch_push
            .clone()
            .or(default_push.clone())
            .or(upstream_remote.clone())
            .or_else(|| {
                available
                    .iter()
                    .find(|name| name.as_str() == "origin")
                    .cloned()
            })
            .or_else(|| (available.len() == 1).then(|| available[0].clone()))
            .ok_or("choose a push remote in Git configuration")?
    } else {
        upstream_remote.clone().ok_or("no upstream")?
    };
    if !available.contains(&remote) {
        return Err("a configured named remote is required".into());
    }
    let push_specs = values(cwd, &format!("remote.{remote}.push"), deadline)?;
    let mirror = line_optional(
        cwd,
        &[
            "config",
            "--bool",
            "--get",
            &format!("remote.{remote}.mirror"),
        ],
        deadline,
    )?;
    let target_ref = if !push {
        upstream_ref.clone().ok_or("no upstream")?
    } else {
        if mirror.as_deref() == Some("true") {
            return Err("mirror pushes are not supported by the dashboard".into());
        }
        if !push_specs.is_empty() {
            if push_specs.len() != 1 {
                return Err("multi-branch push configuration is not supported".into());
            }
            let spec = &push_specs[0];
            if spec.contains(['+', '*']) {
                return Err("forced or wildcard push configuration is not supported".into());
            }
            let (source, target) = spec
                .split_once(':')
                .unwrap_or_else(|| (spec, if spec == "HEAD" { &branch_ref } else { spec }));
            if source != branch && source != branch_ref && source != "HEAD" {
                return Err("push configuration targets another branch".into());
            }
            if target.starts_with("refs/") {
                target.to_string()
            } else {
                format!("refs/heads/{target}")
            }
        } else {
            match push_default.as_deref().unwrap_or("simple") {
                "matching" | "nothing" => return Err("push.default must select one branch".into()),
                "upstream" | "tracking" => {
                    if upstream_remote.as_deref() != Some(&remote) {
                        return Err("push.default=upstream requires the upstream remote".into());
                    }
                    upstream_ref.clone().ok_or("no upstream")?
                }
                "simple" => {
                    if upstream_remote.as_deref() == Some(&remote)
                        && upstream_ref.as_ref().is_some_and(|r| r != &branch_ref)
                    {
                        return Err(
                            "upstream name differs; configure push.default explicitly".into()
                        );
                    }
                    branch_ref.clone()
                }
                "current" => branch_ref.clone(),
                _ => return Err("unsupported push.default".into()),
            }
        }
    };
    if !target_ref.starts_with("refs/heads/")
        || !run_git(cwd, &["check-ref-format", &target_ref], deadline)
            .map_err(failure)?
            .status_ok
    {
        return Err("destination must be one branch".into());
    }
    let url_args: Vec<&str> = if push {
        vec!["remote", "get-url", "--push", "--all", &remote]
    } else {
        vec!["remote", "get-url", "--all", &remote]
    };
    let urls = line(cwd, &url_args, deadline)?;
    if urls.lines().count() != 1 {
        return Err("multiple remote URLs are not supported by the dashboard".into());
    }
    let url = urls;
    let git_dir = line(cwd, &["rev-parse", "--absolute-git-dir"], deadline)?;
    let mut identity = Sha256::new();
    // A daemon restart or a different host must require a new preparation.
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    digest_part(
        &mut identity,
        INSTANCE
            .get_or_init(|| format!("{}:{:?}", std::process::id(), std::time::SystemTime::now()))
            .as_bytes(),
    );
    for path in [
        cwd.to_path_buf(),
        Path::new(&git_dir)
            .canonicalize()
            .map_err(|_| "checkout disappeared")?,
    ] {
        let metadata = path.metadata().map_err(|_| "checkout disappeared")?;
        digest_part(&mut identity, path.as_os_str().as_bytes());
        digest_part(&mut identity, &metadata.dev().to_le_bytes());
        digest_part(&mut identity, &metadata.ino().to_le_bytes());
    }
    for value in [
        &upstream_remote,
        &upstream_ref,
        &branch_push,
        &default_push,
        &push_default,
        &mirror,
    ] {
        digest_part(&mut identity, value.as_deref().unwrap_or("").as_bytes());
    }
    digest_part(&mut identity, url.as_bytes());
    for spec in &push_specs {
        digest_part(&mut identity, spec.as_bytes());
    }
    let fetch_specs = values(cwd, &format!("remote.{remote}.fetch"), deadline)?;
    let mut tracking = Vec::new();
    for spec in fetch_specs {
        digest_part(&mut identity, spec.as_bytes());
        if let Some(name) = tracking_ref(&spec, &target_ref) {
            tracking.push(name);
        }
    }
    // URL may differ from fetch URL (e.g. a separate publication repository).
    // In that case its success does not describe the fetch-tracking refs.
    let fetch_url = line(cwd, &["remote", "get-url", &remote], deadline)?;
    digest_part(&mut identity, fetch_url.as_bytes());
    if push && fetch_url != url {
        tracking.clear();
    }
    let plan = VcsPlan {
        push,
        branch,
        commit,
        remote,
        target_ref,
        target_commit: None,
        identity: identity
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        set_upstream: push && upstream_remote.is_none() && upstream_ref.is_none(),
    };
    Ok(Checkout {
        plan,
        url,
        tracking,
    })
}

fn digest_part(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

fn tracking_ref(spec: &str, branch: &str) -> Option<String> {
    let (source, target) = spec.trim_start_matches('+').split_once(':')?;
    if !target.starts_with("refs/remotes/") {
        return None;
    }
    if let Some((before, after)) = source.split_once('*') {
        let name = branch.strip_prefix(before)?.strip_suffix(after)?;
        Some(target.replacen('*', name, 1))
    } else {
        (source == branch).then(|| target.to_string())
    }
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remote_tip(
    cwd: &Path,
    url: &str,
    target: &str,
    ssh_auth_sock: Option<&str>,
    deadline: Instant,
) -> Result<String, String> {
    let output = run_git_with_agent(
        cwd,
        &["ls-remote", "--exit-code", "--refs", "--", url, target],
        ssh_auth_sock,
        deadline,
    )
    .map_err(failure)?;
    // Keep authentication failures actionable, as for fetch and push.
    if !output.status_ok {
        return finish_command(Ok(output), "checked remote");
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let mut lines = output.lines();
    let (oid, name) = lines
        .next()
        .and_then(|line| line.split_once('\t'))
        .ok_or("remote branch is gone")?;
    if lines.next().is_some() || name != target || !valid_oid(oid) {
        return Err("remote branch is ambiguous".into());
    }
    Ok(oid.to_string())
}

fn optional_ref(cwd: &Path, name: &str, deadline: Instant) -> Option<String> {
    line(cwd, &["rev-parse", "--verify", name], deadline)
        .ok()
        .filter(|oid| valid_oid(oid))
}

fn config(cwd: &Path, key: &str, deadline: Instant) -> Result<Option<String>, String> {
    let mut all = values(cwd, key, deadline)?;
    if all.len() > 1 {
        return Err(format!("multiple values for {key} are not supported"));
    }
    Ok(all.pop())
}

fn values(cwd: &Path, key: &str, deadline: Instant) -> Result<Vec<String>, String> {
    let result =
        run_git(cwd, &["config", "--null", "--get-all", key], deadline).map_err(failure)?;
    if !result.status_ok && !result.stderr.is_empty() {
        return Err("could not read Git configuration".into());
    }
    if result.stdout.len() >= super::OUTPUT_CAP as usize {
        return Err("Git configuration exceeds the command output limit".into());
    }
    if !result.status_ok {
        return Ok(Vec::new());
    }
    Ok(result
        .stdout
        .strip_suffix(&[0])
        .unwrap_or(&result.stdout)
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

fn line_optional(cwd: &Path, args: &[&str], deadline: Instant) -> Result<Option<String>, String> {
    let output = run_git(cwd, args, deadline).map_err(failure)?;
    if !output.status_ok {
        return if output.stderr.is_empty() {
            Ok(None)
        } else {
            Err("could not read Git configuration".into())
        };
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
    ))
}

fn line(cwd: &Path, args: &[&str], deadline: Instant) -> Result<String, String> {
    let output = run_git(cwd, args, deadline).map_err(failure)?;
    if !output.status_ok {
        return Err("could not resolve Git command target".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string())
}

fn success(cwd: &Path, args: &[&str], deadline: Instant) -> Result<(), String> {
    finish_command(run_git(cwd, args, deadline), "done").map(|_| ())
}

fn failure(error: super::RunFail) -> String {
    match error {
        super::RunFail::TimedOut => "Git deadline expired before the command could complete".into(),
        super::RunFail::NoTool => "git is not on PATH".into(),
        super::RunFail::Denied => "permission denied".into(),
        super::RunFail::Message(message) => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Repo {
        root: PathBuf,
        work: PathBuf,
        remote: PathBuf,
    }
    impl Repo {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "cm-vcs-plan-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let work = root.join("work");
            let remote = root.join("origin");
            std::fs::create_dir_all(&work).unwrap();
            git(&work, &["init", "-q", "-b", "main"]);
            git(&work, &["config", "user.name", "Test"]);
            git(&work, &["config", "user.email", "test@example.invalid"]);
            git(&work, &["config", "commit.gpgSign", "false"]);
            git(&work, &["config", "push.default", "simple"]);
            git(&work, &["config", "core.hooksPath", "/dev/null"]);
            git(&work, &["commit", "-q", "--allow-empty", "-m", "initial"]);
            git(&work, &["init", "-q", "--bare", remote.to_str().unwrap()]);
            git(
                &work,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            Self { root, work, remote }
        }
        fn plan(&self, push: bool) -> VcsPlan {
            prepare(self.work.to_str().unwrap(), push, deadline()).unwrap()
        }
        fn run(&self, plan: &VcsPlan) -> Result<String, String> {
            execute(self.work.to_str().unwrap(), plan, deadline())
        }
        fn publish(&self) {
            assert_eq!(self.run(&self.plan(true)).unwrap(), "pushed");
        }
        fn advance_remote(&self) -> String {
            let peer = self.root.join("peer");
            git(
                &self.work,
                &[
                    "clone",
                    "-q",
                    "-b",
                    "main",
                    self.remote.to_str().unwrap(),
                    peer.to_str().unwrap(),
                ],
            );
            git(
                &peer,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "remote change",
                ],
            );
            git(&peer, &["push", "-q", "origin", "main"]);
            git(&peer, &["rev-parse", "HEAD"])
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn deadline() -> Instant {
        Instant::now() + super::super::COMMAND_LIMIT
    }
    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn git_network_commands_use_the_request_agent_after_reconnect() {
        let repo = Repo::new();
        repo.publish();
        let remote_commit = repo.advance_remote();
        let script = repo.root.join("agent-ssh");
        let expected = repo.root.join("agent-ssh.expected");
        std::fs::write(
            &script,
            r#"
if [ "$SSH_AUTH_SOCK" != "$(cat "$0.expected")" ]; then
    echo "request agent missing" >&2
    exit 1
fi
for last do :; done
exec sh -c "$last"
"#,
        )
        .unwrap();
        git(
            &repo.work,
            &[
                "config",
                "core.sshCommand",
                &format!(
                    "sh {}",
                    crate::paths::shell_quote_host_path(script.to_str().unwrap())
                ),
            ],
        );
        git(&repo.work, &["config", "ssh.variant", "simple"]);
        git(
            &repo.work,
            &[
                "remote",
                "set-url",
                "origin",
                &format!("example:{}", repo.remote.display()),
            ],
        );
        let cwd = repo.work.to_str().unwrap();
        let process_agent = std::env::var_os("SSH_AUTH_SOCK");
        std::fs::write(&expected, "/tmp/agent-first.sock").unwrap();
        let pull =
            prepare_with_agent(cwd, false, Some("/tmp/agent-first.sock"), deadline()).unwrap();
        // Preparation authenticates both ls-remote and fetch. Execution uses a
        // replacement connection's agent and keeps the same prepared commit.
        std::fs::write(&expected, "/tmp/agent-second.sock").unwrap();
        assert!(
            execute_with_agent(cwd, &pull, Some("/tmp/agent-first.sock"), deadline())
                .unwrap_err()
                .contains("request agent missing")
        );
        assert_eq!(
            execute_with_agent(cwd, &pull, Some("/tmp/agent-second.sock"), deadline()).unwrap(),
            "pulled"
        );
        assert_eq!(git(&repo.work, &["rev-parse", "HEAD"]), remote_commit);
        git(
            &repo.work,
            &["commit", "-q", "--allow-empty", "-m", "local change"],
        );
        let push =
            prepare_with_agent(cwd, true, Some("/tmp/agent-second.sock"), deadline()).unwrap();
        assert_eq!(
            execute_with_agent(cwd, &push, Some("/tmp/agent-second.sock"), deadline()).unwrap(),
            "pushed"
        );
        assert_eq!(
            git(&repo.remote, &["rev-parse", "refs/heads/main"]),
            git(&repo.work, &["rev-parse", "HEAD"])
        );
        assert_eq!(std::env::var_os("SSH_AUTH_SOCK"), process_agent);
    }

    #[test]
    fn publication_sets_tracking_without_following_tags() {
        let repo = Repo::new();
        git(&repo.work, &["tag", "-a", "release-test", "-m", "tag"]);
        git(&repo.work, &["config", "push.followTags", "true"]);
        let plan = repo.plan(true);
        repo.publish();
        assert_eq!(
            git(&repo.remote, &["rev-parse", "refs/heads/main"]),
            plan.commit
        );
        assert_eq!(git(&repo.work, &["rev-parse", "@{upstream}"]), plan.commit);
        assert!(git(&repo.remote, &["tag"]).is_empty());
    }

    #[test]
    fn push_remote_precedence_and_explicit_branch_mapping() {
        let repo = Repo::new();
        repo.publish();
        let other = repo.root.join("other");
        git(
            &repo.work,
            &["init", "-q", "--bare", other.to_str().unwrap()],
        );
        git(
            &repo.work,
            &["remote", "add", "publish", other.to_str().unwrap()],
        );
        git(&repo.work, &["config", "remote.pushDefault", "publish"]);
        assert_eq!(repo.plan(true).remote, "publish");
        git(&repo.work, &["config", "branch.main.pushRemote", "origin"]);
        assert_eq!(repo.plan(true).remote, "origin");
        git(&repo.work, &["config", "branch.main.pushRemote", "publish"]);
        git(
            &repo.work,
            &["config", "remote.publish.push", "main:review"],
        );
        let plan = repo.plan(true);
        assert_eq!(plan.destination(), "publish/review");
        repo.run(&plan).unwrap();
        assert_eq!(
            git(&other, &["rev-parse", "refs/heads/review"]),
            plan.commit
        );
        assert_eq!(git(&repo.work, &["config", "branch.main.remote"]), "origin");
    }

    #[test]
    fn unsafe_push_configuration_is_refused() {
        let repo = Repo::new();
        for (key, value) in [
            ("remote.origin.mirror", "true"),
            ("remote.origin.push", "+main:main"),
            ("remote.origin.push", "refs/heads/*:refs/heads/*"),
            ("remote.origin.push", ":main"),
            ("remote.origin.push", "other:main"),
            ("remote.origin.push", "main:refs/tags/main"),
            ("push.default", "matching"),
            ("push.default", "nothing"),
        ] {
            git(&repo.work, &["config", key, value]);
            assert!(
                prepare(repo.work.to_str().unwrap(), true, deadline()).is_err(),
                "{key}={value}"
            );
            git(&repo.work, &["config", "--unset-all", key]);
        }
        git(
            &repo.work,
            &["config", "--add", "remote.origin.push", "main:main"],
        );
        git(
            &repo.work,
            &["config", "--add", "remote.origin.push", "main:other"],
        );
        assert!(prepare(repo.work.to_str().unwrap(), true, deadline()).is_err());
        git(&repo.work, &["config", "--unset-all", "remote.origin.push"]);
        git(
            &repo.work,
            &[
                "config",
                "--add",
                "remote.origin.pushurl",
                repo.remote.to_str().unwrap(),
            ],
        );
        git(
            &repo.work,
            &[
                "config",
                "--add",
                "remote.origin.pushurl",
                repo.root.join("other").to_str().unwrap(),
            ],
        );
        assert!(prepare(repo.work.to_str().unwrap(), true, deadline()).is_err());
        assert!(git(&repo.remote, &["for-each-ref"]).is_empty());
    }

    #[test]
    fn changed_branch_commit_destination_or_checkout_requires_preparation() {
        for change in ["branch", "commit", "destination", "checkout"] {
            let repo = Repo::new();
            let plan = repo.plan(true);
            match change {
                "branch" => {
                    git(&repo.work, &["checkout", "-q", "-b", "other"]);
                }
                "commit" => {
                    git(&repo.work, &["commit", "-q", "--allow-empty", "-m", "new"]);
                }
                "destination" => {
                    git(
                        &repo.work,
                        &[
                            "remote",
                            "set-url",
                            "origin",
                            repo.root.join("other").to_str().unwrap(),
                        ],
                    );
                }
                "checkout" => {
                    let old = repo.root.join("old");
                    std::fs::rename(&repo.work, &old).unwrap();
                    git(
                        &old,
                        &[
                            "clone",
                            "-q",
                            old.to_str().unwrap(),
                            repo.work.to_str().unwrap(),
                        ],
                    );
                    git(
                        &repo.work,
                        &["remote", "set-url", "origin", repo.remote.to_str().unwrap()],
                    );
                }
                _ => unreachable!(),
            }
            assert!(repo.run(&plan).is_err(), "{change}");
            assert!(git(&repo.remote, &["for-each-ref"]).is_empty(), "{change}");
        }
    }

    #[test]
    fn pull_uses_prepared_candidate_and_rejects_remote_movement() {
        let repo = Repo::new();
        repo.publish();
        let old = git(&repo.work, &["rev-parse", "HEAD"]);
        let target = repo.advance_remote();
        let plan = repo.plan(false);
        assert_eq!(plan.target_commit.as_deref(), Some(target.as_str()));
        assert_eq!(git(&repo.work, &["rev-parse", "HEAD"]), old);
        // An unrelated FETCH_HEAD cannot redirect the merge.
        std::fs::write(repo.work.join(".git/FETCH_HEAD"), "not the prepared commit").unwrap();
        assert_eq!(repo.run(&plan).unwrap(), "pulled");
        assert_eq!(git(&repo.work, &["rev-parse", "HEAD"]), target);
        assert_eq!(git(&repo.work, &["rev-parse", "@{upstream}"]), target);
        let status = super::super::status(repo.work.to_str().unwrap());
        assert_eq!((status.ahead, status.behind), (0, 0));

        let plan = repo.plan(false);
        let peer = repo.root.join("peer");
        git(
            &peer,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "later",
            ],
        );
        git(&peer, &["push", "-q", "origin", "main"]);
        assert_eq!(repo.run(&plan).unwrap_err(), CHANGED);
        assert_eq!(git(&repo.work, &["rev-parse", "HEAD"]), target);
    }

    #[test]
    fn rejected_push_never_forces_a_diverged_remote() {
        let repo = Repo::new();
        repo.publish();
        let remote = repo.advance_remote();
        git(
            &repo.work,
            &["commit", "-q", "--allow-empty", "-m", "local"],
        );
        assert!(repo.run(&repo.plan(true)).is_err());
        assert_eq!(git(&repo.remote, &["rev-parse", "refs/heads/main"]), remote);
        assert!(prepare(repo.work.to_str().unwrap(), false, deadline()).is_err());
    }

    #[test]
    fn lock_wait_counts_against_deadline() {
        let guard = super::super::lock_until(deadline()).unwrap();
        let start = Instant::now();
        let result = std::thread::spawn(move || {
            prepare("/tmp", true, start + std::time::Duration::from_millis(40))
        })
        .join()
        .unwrap();
        assert!(result.unwrap_err().contains("deadline"));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        drop(guard);
    }
}
