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
    /// ssh port forwards held open for exactly as long as this host is
    /// connected — `8080:3000` to reach its dev server, `R:9000` to expose one
    /// of yours to it. Stored as the user typed them, not as parsed forwards:
    /// the panel is where a typo is visible and fixable, and canonicalising on
    /// load would rewrite their text out from under them. Parsed (and the
    /// malformed ones dropped) at backend-build time by
    /// [`crate::port_forward::valid`].
    ///
    /// Ignored for a `socket` host — there is no ssh hop there to carry them.
    #[serde(default)]
    pub forwards: Vec<String>,
    /// Extra ssh arguments for the dashboard's **own** connection to this host:
    /// the probe, `daemon ensure` and the `-N -L` tunnel child, which together
    /// are what establish the ControlMaster every later hop rides. Passed
    /// verbatim, in order, ahead of the target.
    ///
    /// Not the place for host identity — port, `ProxyJump`, `IdentityFile` — all
    /// of which belong in a `~/.ssh/config` `Host` block, since captain-miao
    /// reaches a host by plain `ssh <target>` and a block there also covers the
    /// attach windows and the `w` shell. What is left for this field is tuning
    /// ssh_config can't scope to us alone: `-C`, keepalive and window sizes, an
    /// extra `-o`.
    ///
    /// Ignored for a `socket` host, like [`Self::forwards`] and for the same
    /// reason.
    #[serde(default)]
    pub ssh_args: Vec<String>,
}

/// Split the panel's `Args` field into tokens.
///
/// Whitespace only, with no quoting: the arguments this field is for are single
/// tokens or `-o key=value` pairs, and the one common option whose value has a
/// space in it — `ProxyCommand` — is exactly the kind that belongs in
/// `~/.ssh/config` instead. A quoting grammar here would exist to serve the
/// case the field is documented as not being for.
pub(super) fn split_ssh_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Pair each token with whether it is actually passed to ssh, in order.
///
/// The one thing refused is a **port forward** (`-L`/`-R`/`-D`, glued or with
/// its value in the next token). Not because it wouldn't work — it would, once —
/// but because it would work *outside* the lifecycle the `Ports` field exists to
/// give it: nothing would cancel it when the row is edited, suspended or
/// removed, so a forward added here would quietly outlive the host it belongs
/// to. Two ways to ask for the same thing, one of which silently leaks, is worse
/// than one way and a red token saying so.
///
/// A rejected flag takes its **value token with it**. Dropping `-L` alone would
/// leave `8080:localhost:3000` as a bare positional, which ssh reads as the
/// *hostname* — turning a refused forward into a connection to a machine that
/// doesn't exist. Pure.
pub(super) fn classify_ssh_args(args: &[String]) -> Vec<(String, bool)> {
    let mut out = Vec::with_capacity(args.len());
    let mut drop_value = false;
    for a in args {
        // Case matters: `-L` is a local forward, `-l` is the login name.
        let separated = matches!(a.as_str(), "-L" | "-R" | "-D");
        let glued = !separated
            && a.len() > 2
            && (a.starts_with("-L") || a.starts_with("-R") || a.starts_with("-D"));
        let keep = !(separated || glued || drop_value);
        drop_value = separated;
        out.push((a.clone(), keep));
    }
    out
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
