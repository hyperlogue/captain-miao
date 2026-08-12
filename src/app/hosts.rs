//! Remote-host configuration for the federated dashboard. Mutable runtime state
//! (managed in the TUI, persisted to `hosts.json`), not static config — each
//! entry is a host the dashboard mirrors over a `miao-server` socket.
//!
//! A host used to carry a `color` alongside its `icon`. It was dropped: the two
//! said the same thing, the icon says it better (an emoji is self-coloured and
//! distinguishes far more than a palette of eight), and one affordance per
//! concept is one fewer field to Tab past. `serde` ignores the leftover key, so
//! an older `hosts.json` still loads — the colour is simply forgotten. The same
//! is true of the short-lived `forwards`/`ssh_args` pair that [`HostConfig::options`]
//! replaced before either shipped.

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
    /// Suspended: the host stays configured and stays in the panel, but no
    /// backend is built for it, so nothing dials it and it contributes no rows.
    /// Toggled with `c` in the hosts panel — the answer to a host that is down,
    /// noisy, or simply not in use today, where the alternative was deleting it
    /// and retyping the target later.
    ///
    /// Spelled as the negative deliberately: `#[serde(default)]` on a `bool` is
    /// `false`, so both an older `hosts.json` and `HostConfig::default()` mean
    /// *enabled* — an `enabled` field would silently disable every host on
    /// upgrade.
    #[serde(default)]
    pub disabled: bool,
    /// ssh arguments for this host's connection, as the user typed them: passed
    /// through verbatim and in order, with no grammar of our own on top.
    ///
    /// This is a raw escape hatch on purpose. There are only two coherent shapes
    /// for the feature — a raw argument string, or a structured editor where a
    /// forward is a row with a type and two endpoints — and anything between the
    /// two is a bespoke syntax the user has to learn *and* a ceiling they hit
    /// anyway. Raw is the one that stays small.
    ///
    /// Most of what you'd reach for belongs in `~/.ssh/config` instead: a port,
    /// `ProxyJump`, an `IdentityFile` are properties of the machine, and a
    /// `Host` block there covers the attach windows and the `w` shell too. What
    /// this field adds is what ssh_config can't scope to captain-miao alone —
    /// tuning for *our* connection (`-C`, keepalives) and, above all, **port
    /// forwards**, which are not a property of the machine at all: they are
    /// something you want up while working on that host and gone when you're
    /// not. A `-L`/`-R`/`-D` here is lifted onto the tunnel child by
    /// [`crate::backend::split_connection_options`] so it lives and dies with
    /// the connection.
    ///
    /// Ignored for a `socket` host — there is no ssh hop there to carry any of
    /// it.
    #[serde(default)]
    pub options: Vec<String>,
}

/// Split the panel's `Options` field into tokens.
///
/// Whitespace only, with no quoting. The one common ssh option whose value
/// contains a space is `ProxyCommand`, which is exactly the kind that belongs in
/// `~/.ssh/config` — so a quoting grammar here would exist to serve the case the
/// field is documented as not being for, at the price of being one more syntax
/// to get wrong. Pure.
pub(super) fn split_options(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
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
