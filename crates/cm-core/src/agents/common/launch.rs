//! Native agent launch composition: project approval, direnv, and clipboard PATH.
//!
//! Callers add their agent-specific environment and arguments to the returned
//! command. Resolve the executable before direnv runs; restore clipboard shim
//! priority after it runs. The shell wrapper uses positional arguments and exec
//! so it neither interprets agent arguments nor remains as a supervising process.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

use crate::agents::{find_in_path, with_shim_path};

/// Build a native agent command in `cwd`, loading its nearest approved `.envrc`
/// and making pooled clipboard tools available. Search the canonical directory's
/// ancestors through the filesystem root, as direnv does. This search never
/// executes a project script or bypasses approval of a nearer `.envrc`.
pub(in crate::agents) fn agent_command(
    bin: &str,
    cwd: &str,
    shim_dir: Option<&Path>,
) -> Result<Command> {
    let exe = find_in_path(bin).with_context(|| format!("{bin} not found in PATH"))?;
    let project = std::fs::canonicalize(cwd).context("resolving session directory for direnv")?;
    let has_envrc = project.ancestors().any(|dir| dir.join(".envrc").is_file());
    let mut cmd = if has_envrc {
        let direnv =
            find_in_path("direnv").context("project has an .envrc but direnv is not on PATH")?;
        check_direnv_allowed(&direnv, &project)?;
        let mut c = Command::new(direnv);
        c.arg("exec").arg(&project);
        if let Some(shims) = shim_dir {
            // direnv may prepend native clipboard tools or replace PATH
            // entirely. Restore the bridge after it loads the environment.
            // Positional arguments keep paths and agent argv out of shell code.
            c.args([
                "/bin/sh",
                "-c",
                "export PATH=\"$1${PATH:+:$PATH}\"; shift; exec \"$@\"",
                "miao-agent",
            ])
            .arg(shims);
        }
        c.arg(&exe);
        c
    } else {
        let mut c = Command::new(&exe);
        with_shim_path(&mut c, shim_dir);
        c
    };
    cmd.current_dir(cwd);
    Ok(cmd)
}

/// Refuse to launch unless direnv confirms approval of the nearest `.envrc`.
/// `state.foundRC.allowed` is 0 when approved, 1 when unapproved, and 2 when
/// denied. Failed or unrecognized responses are errors, never permission.
/// Keep raw status output (including its reported path) out of diagnostics;
/// values from the external process may contain private information.
fn check_direnv_allowed(direnv: &Path, cwd: &Path) -> Result<()> {
    let output = std::process::Command::new(direnv)
        .args(["status", "--json"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .context("could not check direnv approval")?;
    anyhow::ensure!(output.status.success(), "could not check direnv approval");
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("invalid direnv approval response"))?;
    let reason = match parsed["state"]["foundRC"]["allowed"].as_u64() {
        Some(0) => return Ok(()),
        Some(1) => "not allowed",
        Some(2) => "denied",
        _ => anyhow::bail!("invalid direnv approval response"),
    };
    anyhow::bail!(
        "direnv: nearest .envrc is {reason}. Review it and run `direnv allow` in the session directory to approve it."
    );
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
