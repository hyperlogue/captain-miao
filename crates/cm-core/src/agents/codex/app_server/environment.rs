//! Capture direnv on the execution host, before Codex's command sandbox starts.
//!
//! Only startup-file paths cross the RPC boundary. Putting the environment in
//! `shell_environment_policy.set` makes Codex log its values for manual `!`
//! commands. Immutable, owner-only snapshots instead outlive the launcher: a
//! loaded Codex thread ignores resume overrides and may still use its old file.
//! Store leases and loaded-thread inventory govern automatic collection. Only
//! direnv changes and overlapping Codex overrides are serialized, after filtering.
//! Bash reads BASH_ENV after login profiles; zsh's .zshenv disables further rc
//! processing. Both load the snapshot once, preserving deliberate changes in
//! child shells. Other execution shells are outside this integration's scope.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use super::{environment_policy::Policy, environment_store::Store, transport};
use crate::agents::{find_in_path, shell_quote};
use crate::state;

pub(super) type Delta = BTreeMap<String, Option<String>>;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Environments {
    cwd: PathBuf,
    root: PathBuf,
    snapshots: HashMap<PathBuf, Option<Delta>>,
    store: Option<Store>,
}

impl Environments {
    pub(crate) fn for_project(cwd: &str) -> Self {
        // The client TUI does not execute project commands. Capture only when
        // it actually starts, resumes or forks a user thread (possibly elsewhere).
        Self::new(
            Path::new(cwd),
            state::state_dir().join("codex-environments"),
        )
    }

    pub(super) fn new(cwd: &Path, root: PathBuf) -> Self {
        Self {
            cwd: cwd.to_owned(),
            root,
            snapshots: HashMap::new(),
            store: None,
        }
    }

    async fn snapshot(&mut self, cwd: &Path) -> Result<Option<Delta>> {
        let cwd = std::fs::canonicalize(cwd).context("resolving Codex project directory")?;
        if let Some(snapshot) = self.snapshots.get(&cwd) {
            return Ok(snapshot.clone());
        }
        let snapshot = capture(&cwd).await?;
        self.snapshots.insert(cwd, snapshot.clone());
        Ok(snapshot)
    }

    /// Resolve the actual project when the TUI's /resume picker switches cwd.
    /// Metadata reads use a separate, unsubscribed connection and never retry
    /// the user's operation. Unknown requests and internal helpers pass through.
    pub(super) async fn request(
        &mut self,
        value: &mut Value,
        upstream: &Path,
        daemon: Option<i32>,
    ) -> Result<bool> {
        if !needs_environment(value) {
            return Ok(false);
        }
        let cwd = if let Some(cwd) = value["params"]["cwd"].as_str() {
            let path = Path::new(cwd);
            if path.is_absolute() {
                path.to_owned()
            } else {
                self.cwd.join(path)
            }
        } else if let Some(id) = value["params"]["threadId"].as_str() {
            let mut client = transport::Client::connect(upstream).await?;
            let result = client
                .request("thread/read", json!({"threadId":id,"includeTurns":false}))
                .await?;
            PathBuf::from(
                result["thread"]["cwd"]
                    .as_str()
                    .context("Codex thread has no project directory")?,
            )
        } else {
            self.cwd.clone()
        };
        let Some(delta) = self.snapshot(&cwd).await? else {
            return Ok(false);
        };
        let mut client = transport::Client::connect(upstream).await?;
        ensure!(
            daemon.is_none() || client.peer_pid() == daemon,
            "Codex daemon changed during environment preparation; reconnect the TUI"
        );
        let result = client
            .request("config/read", json!({"cwd":cwd,"includeLayers":false}))
            .await?;
        let (policy, effective) = Policy::resolve(&result["config"], &value["params"]["config"])?;
        let filtered = policy.apply(&delta);
        if self.store.is_none() {
            self.store = Some(Store::new(&self.root, upstream)?);
        }
        let hooks = self
            .store
            .as_mut()
            .unwrap()
            .prepare(&(policy.scrub_script() + &script(&filtered)), daemon)?;
        let settings = policy.bootstrap(effective, &hooks)?;
        let params = value["params"]
            .as_object_mut()
            .context("invalid Codex thread parameters")?;
        let config = params.entry("config").or_insert_with(|| json!({}));
        if config.is_null() {
            *config = json!({});
        }
        let config = config
            .as_object_mut()
            .context("invalid Codex thread configuration")?;
        // Codex accepts dotted config keys. Remove both spellings of the
        // managed keys so an existing override cannot accidentally win.
        config.retain(|key, _| {
            !key.starts_with("shell_environment_policy.") && key != "features.shell_snapshot"
        });
        let features = config.entry("features").or_insert_with(|| json!({}));
        features
            .as_object_mut()
            .context("invalid Codex feature configuration")?
            .insert("shell_snapshot".into(), json!(false));
        config.insert("allow_login_shell".into(), json!(false));
        config.insert("shell_environment_policy".into(), settings);
        Ok(true)
    }
    pub(super) fn response(&mut self, response: &Value, daemon: Option<i32>) {
        if let Some(store) = &mut self.store
            && store.response(response, daemon).is_err()
        {
            tracing::warn!("Could not update Codex environment ownership; retaining snapshots");
        }
    }

    pub(super) fn root(&self) -> PathBuf {
        self.root.clone()
    }

    pub(super) async fn collect(root: &Path, upstream: &Path) {
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(10),
                super::environment_store::collect(root, upstream)
            )
            .await,
            Ok(Ok(()))
        ) {
            tracing::debug!("Codex environment cleanup deferred until inventory is available");
        }
    }

    pub(super) async fn finish(&mut self, upstream: &Path) {
        self.store = None;
        Self::collect(&self.root, upstream).await;
    }
}

pub(super) fn needs_environment(value: &Value) -> bool {
    matches!(
        value["method"].as_str(),
        Some("thread/start" | "thread/resume" | "thread/fork")
    ) && value.get("id").is_some()
        && value["params"]["threadSource"]
            .as_str()
            .is_none_or(|source| source == "user")
}

async fn output(command: &mut Command, phase: &str) -> Result<std::process::Output> {
    // .envrc may print secrets. Never include captured stdout/stderr in errors.
    tokio::time::timeout(
        CAPTURE_TIMEOUT,
        command.stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .with_context(|| format!("direnv timed out while {phase}"))?
    .with_context(|| format!("could not run direnv while {phase}"))
}

async fn capture(cwd: &Path) -> Result<Option<Delta>> {
    if !cwd.ancestors().any(|dir| dir.join(".envrc").is_file()) {
        return Ok(None);
    }
    let direnv =
        find_in_path("direnv").context("project has an .envrc but direnv is not on PATH")?;
    let status = output(
        Command::new(&direnv)
            .args(["status", "--json"])
            .current_dir(cwd),
        "checking approval",
    )
    .await?;
    ensure!(status.status.success(), "could not check direnv approval");
    let status: Value = serde_json::from_slice(&status.stdout)
        .map_err(|_| anyhow::anyhow!("invalid direnv approval response"))?;
    ensure!(
        status["state"]["foundRC"]["allowed"] == 0,
        "direnv .envrc is not approved; run direnv allow in {}",
        cwd.display()
    );
    let env = find_in_path("env").context("env is not on PATH")?;
    let captured = output(
        Command::new(&direnv)
            .arg("exec")
            .arg(cwd)
            .arg(&env)
            .arg("-0")
            .current_dir(cwd),
        "capturing the project environment",
    )
    .await?;
    ensure!(
        captured.status.success(),
        "direnv environment capture failed in {}; check its .envrc",
        cwd.display()
    );
    let variables = parse(&captured.stdout)?;
    // `direnv exec` reverses DIRENV_DIFF even if the launcher was started in
    // an already activated project. Comparing with std::env would lose those
    // project's changes (or import changes from a previously active project).
    let empty = tempfile::tempdir_in("/tmp")?;
    ensure!(
        !empty
            .path()
            .ancestors()
            .any(|dir| dir.join(".envrc").is_file() || dir.join(".env").is_file()),
        "cannot capture a clean direnv baseline beneath an .envrc or .env"
    );
    let baseline = output(
        Command::new(&direnv)
            .arg("exec")
            .arg(empty.path())
            .arg(&env)
            .arg("-0")
            .current_dir(empty.path()),
        "unloading the inherited project environment",
    )
    .await?;
    ensure!(
        baseline.status.success(),
        "could not unload inherited direnv environment"
    );
    let baseline = parse(&baseline.stdout)?;
    let mut delta = Delta::new();
    for key in baseline.keys().chain(variables.keys()) {
        if baseline.get(key) != variables.get(key) {
            delta.insert(key.clone(), variables.get(key).cloned());
        }
    }
    Ok(Some(delta))
}

fn parse(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut variables = BTreeMap::new();
    for entry in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let entry = std::str::from_utf8(entry)
            .map_err(|_| anyhow::anyhow!("direnv environment is not UTF-8"))?;
        let (key, value) = entry
            .split_once('=')
            .context("invalid direnv environment record")?;
        // Shell functions and names that cannot be exported by Bash/zsh are
        // not shell environment variables. Never interpolate them into code.
        if variable_name(key) && !reserved(key) {
            variables.insert(key.to_owned(), value.to_owned());
        }
    }
    ensure!(
        variables.contains_key("PATH"),
        "direnv environment capture returned no PATH"
    );
    Ok(variables)
}

pub(super) fn variable_name(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

pub(super) fn reserved(key: &str) -> bool {
    if matches!(
        key.to_ascii_uppercase().as_str(),
        "NODE_REPL_AUTH_TOKEN"
            | "OPENAI_FEDERATION_RULE_ID"
            | "OPENAI_IDENTITY_TOKEN_FILE"
            | "OPENAI_WORKLOAD_IDENTITY_CONTEXT"
    ) {
        return true;
    }
    // Codex supplies these at execution time (including sandbox/proxy metadata).
    key.starts_with("CODEX_")
        || key.starts_with("DIRENV_")
        || key.starts_with("_CM_DIRENV_")
        || matches!(
            key,
            "BASH_ENV"
                | "SHLVL"
                | "_"
                | "PWD"
                | "OLDPWD"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "UID"
                | "EUID"
                | "PPID"
        )
}

pub(super) fn script(delta: &Delta) -> String {
    let mut script =
        String::from("# Captured direnv changes; contains private environment values.\n");
    for (key, value) in delta {
        let command = match value {
            Some(value) => format!("export {key}={}", shell_quote(value)),
            None => format!("unset {key}"),
        };
        assignment(&mut script, key, &command);
    }
    if !delta.contains_key("ZDOTDIR") {
        script.push_str("unset ZDOTDIR\n");
    }
    script.push_str("export BASH_ENV=/dev/null\n");
    script
}

fn assignment(script: &mut String, key: &str, command: &str) {
    // A managed network proxy is injected after Codex applies its environment
    // policy. The startup hook must not replace it with launcher-side settings.
    let proxy = key.to_ascii_uppercase().contains("PROXY")
        || matches!(
            key,
            "GIT_SSH_COMMAND"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "REQUESTS_CA_BUNDLE"
                | "CURL_CA_BUNDLE"
                | "NODE_EXTRA_CA_CERTS"
                | "GIT_SSL_CAINFO"
                | "CARGO_HTTP_CAINFO"
                | "PIP_CERT"
                | "BUNDLE_SSL_CA_CERT"
                | "npm_config_cafile"
                | "NPM_CONFIG_CAFILE"
        );
    if proxy {
        script.push_str("if [ -z \"${CODEX_NETWORK_PROXY_ACTIVE+x}\" ]; then\n");
    }
    script.push_str(command);
    script.push('\n');
    if proxy {
        script.push_str("fi\n");
    }
}

pub(super) fn persist(root: &Path, script: &str) -> Result<Value> {
    // Broker policy can change after thread creation. These startup files run
    // after brokering, so injecting any raw value would bypass it. Stop the
    // shell itself (returning from BASH_ENV would still execute the command).
    let script = format!(
        "if [ -n \"${{CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE+x}}\" ]; then\nprintf '%s\\n' 'miao: direnv is unsupported with the Codex credential broker' >&2\nexit 1\nfi\n{script}"
    );
    // Identical scripts share a file within one launcher's lease. Different
    // launchers retain independent ownership, even when their values match.
    let digest: String = Sha256::digest(script.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let dir = root.join(digest);
    state::create_dir_all_private(&dir)?;
    let path = dir.join("env.sh");
    write_once(&path, &script)?;
    write_once(
        &dir.join(".zshenv"),
        &format!("unsetopt RCS\n. {}\n", shell_quote(&path.to_string_lossy())),
    )?;
    Ok(json!({"set":{"BASH_ENV":path,"ZDOTDIR":dir,"SHLVL":"1"}}))
}

fn write_once(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // The directory is content-addressed and privately owned by this launcher.
    // Repeated resumes still need cold-load overrides, but not fresh copies.
    if path.try_exists()? {
        return Ok(());
    }
    // Publish complete files atomically without a shared temporary filename.
    // A racing writer has identical bytes. No thread sees a partial script.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}-{nonce}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        file.write_all(contents.as_bytes())?;
        std::fs::rename(&tmp, path)
    })();
    let _ = std::fs::remove_file(tmp);
    result.context("writing private Codex environment snapshot")
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
