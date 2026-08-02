//! Remote-host configuration for the federated dashboard. Mutable runtime state
//! (managed in the TUI, persisted to `hosts.json`), not static config — each
//! entry is a host the dashboard mirrors over a `captain-miao server` socket.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct HostConfig {
    /// Display label, and the `HostId` sessions from this host are tagged with.
    pub label: String,
    /// ssh target (`user@host`) the dashboard forwards the server socket over.
    /// Wired in the ssh-transport slice; `socket` takes precedence today.
    #[serde(default)]
    pub ssh: Option<String>,
    /// Explicit socket path to connect to — for a manually-forwarded socket or
    /// local testing. Overrides `ssh` when set.
    #[serde(default)]
    pub socket: Option<String>,
    /// Color (name or `#rrggbb`) for this host's label in the table.
    #[serde(default)]
    pub color: Option<String>,
}

/// Load the configured hosts, or an empty list if none / unreadable.
pub(super) fn load_hosts() -> Vec<HostConfig> {
    crate::state::read_json::<Vec<HostConfig>>(&crate::state::hosts_path()).unwrap_or_default()
}

/// Persist the host list. Called from the hosts popup panel on save.
pub(super) fn save_hosts(hosts: &[HostConfig]) {
    let _ = crate::state::write_json_atomic(&crate::state::hosts_path(), &hosts);
}
