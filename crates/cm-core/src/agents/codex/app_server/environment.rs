//! Capture direnv on the execution host, before Codex's command sandbox starts.
//!
//! Only startup-file paths cross the RPC boundary. Putting the environment in
//! `shell_environment_policy.set` makes Codex log its values for manual `!`
//! commands. Immutable, owner-only snapshots instead outlive the launcher: a
//! loaded Codex thread ignores resume overrides and may still use its old file.
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

use super::transport;
use crate::agents::{find_in_path, shell_quote};
use crate::state;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Environments {
    cwd: PathBuf,
    root: PathBuf,
    snapshots: HashMap<PathBuf, Option<Value>>,
}

impl Environments {
    pub(crate) async fn prepare(cwd: &str) -> Result<Self> {
        let mut environments = Self::new(
            Path::new(cwd),
            state::state_dir().join("codex-environments"),
        );
        environments.snapshot(Path::new(cwd)).await?;
        Ok(environments)
    }

    pub(super) fn new(cwd: &Path, root: PathBuf) -> Self {
        Self {
            cwd: cwd.to_owned(),
            root,
            snapshots: HashMap::new(),
        }
    }

    async fn snapshot(&mut self, cwd: &Path) -> Result<Option<Value>> {
        let cwd = std::fs::canonicalize(cwd).context("resolving Codex project directory")?;
        if let Some(snapshot) = self.snapshots.get(&cwd) {
            return Ok(snapshot.clone());
        }
        let snapshot = capture(&cwd, &self.root).await?;
        self.snapshots.insert(cwd, snapshot.clone());
        Ok(snapshot)
    }

    /// Resolve the actual project when the TUI's /resume picker switches cwd.
    /// Metadata reads use a separate, unsubscribed connection and never retry
    /// the user's operation. Unknown requests and internal helpers pass through.
    pub(super) async fn request(&mut self, value: &mut Value, upstream: &Path) -> Result<()> {
        if !needs_environment(value) {
            return Ok(());
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
        let Some(settings) = self.snapshot(&cwd).await? else {
            return Ok(());
        };
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
        Ok(())
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

async fn capture(cwd: &Path, root: &Path) -> Result<Option<Value>> {
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
        Command::new(direnv)
            .arg("exec")
            .arg(cwd)
            .arg(env)
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
    let script = script(
        &variables,
        std::env::vars_os().filter_map(|(key, _)| key.into_string().ok()),
    );
    Ok(Some(persist(root, &script)?))
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

fn variable_name(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

fn reserved(key: &str) -> bool {
    // Codex supplies these at execution time (including sandbox/proxy metadata).
    key.starts_with("CODEX_")
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

fn script(variables: &BTreeMap<String, String>, inherited: impl Iterator<Item = String>) -> String {
    let mut script =
        String::from("# Captured by captain-miao; contains private environment values.\n");
    let removed: std::collections::BTreeSet<_> = inherited
        .filter(|key| variable_name(key) && !reserved(key) && !variables.contains_key(key))
        .collect();
    for key in removed {
        assignment(&mut script, &key, &format!("unset {key}"));
    }
    for (key, value) in variables {
        assignment(
            &mut script,
            key,
            &format!("export {key}={}", shell_quote(value)),
        );
    }
    if !variables.contains_key("ZDOTDIR") {
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
    let broker = !matches!(key, "PATH" | "ZDOTDIR");
    if broker {
        script.push_str(&format!("if [ -z \"${{CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE+x}}\" ] || [ -z \"${{{key}+x}}\" ]; then\n"));
    }
    script.push_str(command);
    script.push('\n');
    if broker {
        script.push_str("fi\n");
    }
    if proxy {
        script.push_str("fi\n");
    }
}

fn persist(root: &Path, script: &str) -> Result<Value> {
    // Content addressing lets concurrent launchers share an identical immutable
    // snapshot, and leaves old loaded threads' environments intact on restart.
    let digest: String = Sha256::digest(script.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let dir = root.join(digest);
    state::create_dir_all_private(&dir)?;
    let path = dir.join("env.sh");
    write_once(&path, script)?;
    write_once(
        &dir.join(".zshenv"),
        &format!("unsetopt RCS\n. {}\n", shell_quote(&path.to_string_lossy())),
    )?;
    Ok(json!({"inherit":"none", "set":{"BASH_ENV":path,"ZDOTDIR":dir,"SHLVL":"1"}}))
}

fn write_once(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
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
