//! Remote-host configuration for the federated dashboard. Mutable runtime state
//! (managed in the TUI, persisted to `hosts.json`), not static config — each
//! entry is a host the dashboard mirrors over a `miao-server` socket.
//!
//! A host used to carry a `color` alongside its `icon`. It was dropped: the two
//! said the same thing, the icon says it better (an emoji is self-coloured and
//! distinguishes far more than a palette of eight), and one affordance per
//! concept is one fewer field to Tab past. `serde` ignores the leftover key, so
//! an older `hosts.json` still loads — the colour is simply forgotten.
//! Legacy forwarding switches in `options` migrate into structured rows in
//! memory; the next explicit edit persists them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
/// One entry of `hosts.json`: a host the dashboard federates, and how to reach
/// it. Every field is additive and defaulted — see the module doc on why the
/// file must keep decoding across versions.
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
    /// Advanced SSH arguments. Forwarding switches migrate into `forwards` on
    /// load; only connection options belong here.
    #[serde(default)]
    pub options: Vec<String>,
    /// Command run in a new SSH work tab (`w`) with MIAO_WORKDIR and
    /// MIAO_WORKSPACE. Success closes the tab; failure opens a login shell.
    /// Missing or blank keeps the ordinary shell. This dashboard setting is
    /// independent of the daemon connection and pooled agent sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command: Option<String>,
    #[serde(default)]
    pub forwards: Vec<crate::ssh_forward::Rule>,
    /// Offer this host the dashboard machine's clipboard, so an agent in a
    /// pooled session there can paste a screenshot. The row editor's `Clipboard`
    /// field.
    ///
    /// Off by default and per-host, which is the whole security posture: while a
    /// host is connected, anything running as you there — including the agent,
    /// which runs arbitrary code by design — can read your clipboard when it
    /// holds an image. Only images are ever served
    /// ([`cm_core::clipboard`]), so text can't leak; the toggle is what bounds
    /// *where* even that applies. `#[serde(default)]` gives `false`, so an older
    /// `hosts.json` offers nothing until asked.
    ///
    /// Implemented as one synthesized `-R` on the tunnel child, so it lives and
    /// dies with the connection exactly like a user-typed forward.
    #[serde(default)]
    pub clipboard: bool,
    /// Use Tower for new SSH sessions when the dashboard enables the integration.
    #[serde(default)]
    pub tower: bool,
}

impl HostConfig {
    /// Normalize legacy hosts without writing on read. Unknown options retain
    /// their original argv, including arguments containing spaces.
    pub fn migrate_forwards(&mut self) {
        let (options, forwards) = crate::ssh_forward::split_options(&self.options);
        self.options = options;
        for forward in forwards {
            if !self.forwards.iter().any(|rule| rule.forward == forward) {
                self.forwards.push(forward.into());
            }
        }
    }
}

pub(super) fn split_options(text: &str) -> Vec<String> {
    shell_words::split(text).unwrap_or_else(|_| vec![text.to_owned()])
}

/// Load the configured hosts, or an empty list if none / unreadable.
pub(super) fn load_hosts() -> Vec<HostConfig> {
    let mut hosts =
        crate::state::read_json::<Vec<HostConfig>>(&crate::state::hosts_path()).unwrap_or_default();
    for host in &mut hosts {
        host.migrate_forwards();
    }
    hosts
}

/// Persist the host list. Called from the hosts panel whenever it mutates —
/// adding a host persists (and connects) immediately, edits apply on commit,
/// removal after its confirm — so there is no separate Save step to forget.
pub(super) fn save_hosts(hosts: &[HostConfig]) {
    let _ = try_save_hosts(hosts);
}

/// Rule acknowledgements save on a background task; ordinary host edits save
/// on the UI thread. Serialize access to the atomic writer's temporary file.
pub(super) fn try_save_hosts(hosts: &[HostConfig]) -> anyhow::Result<()> {
    static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _write = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    crate::state::create_dir_all_private(&crate::state::state_dir())?;
    crate::state::write_json_atomic(&crate::state::hosts_path(), &hosts)
}

/// Resolve the panel order, including the synthetic localhost row. Unknown
/// labels and duplicates are discarded; newly configured hosts go at the end.
/// The old default is migrated only when no explicit order has been saved.
pub(super) fn resolve_order(
    hosts: &[HostConfig],
    saved: Option<&[String]>,
    legacy_default: Option<&str>,
) -> Vec<String> {
    let local = crate::state::HostId::local();
    let available: Vec<&str> = std::iter::once(local.0.as_str())
        .chain(
            hosts
                .iter()
                .map(|h| h.label.as_str())
                .filter(|label| !label.is_empty() && !label.eq_ignore_ascii_case("local")),
        )
        .collect();
    let preferred: Vec<&str> = match saved {
        Some(order) => order.iter().map(String::as_str).collect(),
        None => legacy_default.into_iter().collect(),
    };
    let mut order = Vec::new();
    for label in preferred.into_iter().chain(available.iter().copied()) {
        if available.contains(&label) && !order.iter().any(|h| h == label) {
            order.push(label.to_string());
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obsolete_agent_toggle_is_ignored_and_not_saved() {
        for enabled in [false, true] {
            let host: HostConfig = serde_json::from_value(serde_json::json!({
                "label": "example", "ssh": "example.invalid", "forward_agent": enabled,
            }))
            .unwrap();
            assert_eq!(host.ssh.as_deref(), Some("example.invalid"));
            assert!(
                serde_json::to_value(host)
                    .unwrap()
                    .get("forward_agent")
                    .is_none()
            );
        }
    }

    #[test]
    fn shell_command_is_optional_and_round_trips_without_losing_quotes() {
        for value in [
            serde_json::json!({"label": "example", "ssh": "example-target"}),
            serde_json::json!({"label": "example", "ssh": "example-target", "shell_command": null}),
        ] {
            let host: HostConfig = serde_json::from_value(value).unwrap();
            assert!(host.shell_command.is_none());
            assert!(
                serde_json::to_value(host)
                    .unwrap()
                    .get("shell_command")
                    .is_none()
            );
        }
        for command in [
            "",
            " \t ",
            "tmux new-session -A -s 'work session'",
            "zellij",
        ] {
            let host: HostConfig = serde_json::from_value(serde_json::json!({
                "label": "example", "ssh": "example-target", "shell_command": command,
            }))
            .unwrap();
            let restored: HostConfig =
                serde_json::from_value(serde_json::to_value(host).unwrap()).unwrap();
            assert_eq!(restored.shell_command.as_deref(), Some(command));
        }
    }

    #[test]
    fn legacy_forward_migration_keeps_quoted_arguments_and_disabled_rules() {
        let mut host: HostConfig = serde_json::from_value(serde_json::json!({
            "label": "example", "ssh": "example-target",
            "options": ["-o", "ProxyCommand=helper with spaces", "-L3000:localhost:3000", "-R", "/path/to/remote socket:/path/to/local socket"],
            "forwards": [{"name": "Web", "disabled": true, "flag": "-L", "spec": "3000:localhost:3000"}]
        })).unwrap();
        host.migrate_forwards();
        assert_eq!(host.options, ["-o", "ProxyCommand=helper with spaces"]);
        assert_eq!(host.forwards.len(), 2);
        assert!(host.forwards[0].disabled);
        assert_eq!(
            host.forwards[1].forward.spec,
            "/path/to/remote socket:/path/to/local socket"
        );
        assert_eq!(
            split_options(&shell_words::join(&host.options)),
            host.options
        );
        let saved = serde_json::to_value(&host).unwrap();
        host.migrate_forwards();
        assert_eq!(serde_json::to_value(host).unwrap(), saved);
    }
}
