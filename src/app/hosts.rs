//! Remote-host configuration for the federated dashboard. Mutable runtime state
//! (managed in the TUI, persisted to `hosts.json`), not static config — each
//! entry is a host the dashboard mirrors over a `miao-server` socket.
//!
//! A host used to carry a `color` alongside its `icon`. It was dropped: the two
//! said the same thing, the icon says it better (an emoji is self-coloured and
//! distinguishes far more than a palette of eight), and one affordance per
//! concept is one fewer field to Tab past. `serde` ignores the leftover key, so
//! an older `hosts.json` still loads — the colour is simply forgotten.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct HostConfig {
    /// Display label, and the `HostId` sessions from this host are tagged with.
    pub label: String,
    /// ssh target (`user@host`) the dashboard forwards the server socket over.
    #[serde(default)]
    pub ssh: Option<String>,
    /// Explicit socket path to connect to — a manually-forwarded socket, or the
    /// local daemon under pooled-localhost. Overrides `ssh` when set.
    #[serde(default)]
    pub socket: Option<String>,
    /// Emoji shown beside the workdir icon — the same affordance workdir marks have
    /// (`Space i`), for the same reason: at a glance an icon separates hosts far
    /// faster than a truncated label, and it costs one cell instead of six.
    /// Empty/absent falls back to a deterministic emoji derived from the label,
    /// so a host always has *some* icon without the user configuring one.
    #[serde(default)]
    pub icon: Option<String>,
}

/// Load the configured hosts, or an empty list if none / unreadable.
pub(super) fn load_hosts() -> Vec<HostConfig> {
    crate::state::read_json::<Vec<HostConfig>>(&crate::state::hosts_path()).unwrap_or_default()
}

/// Persist the host list. Called from the hosts panel whenever it mutates —
/// adding a host persists (and connects) immediately, edits apply on commit,
/// removal after its confirm — so there is no separate Save step to forget.
pub(super) fn save_hosts(hosts: &[HostConfig]) {
    let _ = crate::state::write_json_atomic(&crate::state::hosts_path(), &hosts);
}
