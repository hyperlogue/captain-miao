use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Host policy for new launches and explicit restarts. The running session
/// records the resolved mode separately; changing this never changes ownership.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexMode {
    #[default]
    Native,
    AppServer,
}

impl CodexMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::AppServer => "app-server",
        }
    }

    pub fn is_native(&self) -> bool {
        *self == Self::Native
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    pub mode: CodexMode,
    pub endpoint: String,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            mode: CodexMode::Native,
            endpoint: "unix://".into(),
        }
    }
}

impl CodexConfig {
    /// Endpoints belong to the execution host. Only its private Unix transport
    /// is supported; TCP authentication and cross-host filesystem semantics are
    /// deliberately outside this adapter.
    pub fn socket_path(&self) -> Result<PathBuf> {
        let path = self
            .endpoint
            .strip_prefix("unix://")
            .context("Codex endpoint must use unix:// or unix:///path/to/socket")?;
        if path.is_empty() {
            return Ok(super::tui::codex_home()
                .context("could not resolve Codex home")?
                .join("app-server-control/app-server-control.sock"));
        }
        let path = PathBuf::from(crate::paths::expand_home(path, &crate::paths::host_home()));
        if !path.is_absolute() {
            bail!("Codex socket path must be absolute or start with ~/");
        }
        Ok(path)
    }

    /// Collapse before crossing the host boundary, including the path inside
    /// the endpoint. No client resolves another machine's home directory.
    pub fn canonical(mut self, home: &str) -> Self {
        if let Some(path) = self.endpoint.strip_prefix("unix://") {
            self.endpoint = format!("unix://{}", crate::paths::collapse_home(path, home));
        }
        self
    }
}
