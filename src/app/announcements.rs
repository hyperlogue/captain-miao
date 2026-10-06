//! Release announcements, independent of startup UI and disk.
//!
//! Append an `Announcement` to `ANNOUNCEMENTS` for a feature, change or warning,
//! with a unique, permanent id, its introduction version, and its content.
//! Text, headings and code snippets cover general migrations; `Binding` resolves
//! a command through the active keymap when the notice concerns shortcuts.
//! Acknowledgements follow ids and control automatic opening. Each release
//! with an unseen item shows all its announcements, including acknowledged ones.
//! Keep old ids and versions unchanged so skipped releases work.
//! Dashboard state migrates the original version-only shortcut acknowledgement.

use anyhow::Context;
use semver::Version;

use super::keymap::Command;

pub(super) struct Announcement {
    pub id: &'static str,
    pub introduced: &'static str,
    pub title: &'static str,
    pub kind: Kind,
    pub content: &'static [Content],
}

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Update,
    Warning,
}

impl Kind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Update => "Update",
            Self::Warning => "Warning",
        }
    }
}

pub(super) enum Content {
    Text(&'static str),
    Heading(&'static str),
    Code(&'static str),
    Binding(Command, &'static str),
}

pub(super) const ANNOUNCEMENTS: &[Announcement] = &[
    Announcement {
        id: "session-shortcuts-x",
        introduced: "0.11.0",
        title: "Session shortcuts changed",
        kind: Kind::Update,
        content: &[
            Content::Text(
                "To avoid closing a session by mistake, the default close key moved from x to X (Shift+x).\nBy default, x now dismisses the notification.",
            ),
            Content::Text(""),
            Content::Heading("Your active shortcuts"),
            Content::Binding(Command::CloseSession, "Close selected session"),
            Content::Binding(Command::DismissNotification, "Dismiss newest notification"),
            Content::Text(""),
            Content::Text(
                "If you want to override and restore the previous default, edit these keys in config.toml.",
            ),
            Content::Code("[keybinds]\nclose_session = \"x\"\ndismiss_notification = []"),
            Content::Text(""),
            Content::Text("Restart miao to apply your changes."),
        ],
    },
    Announcement {
        id: "server-owned-attention",
        introduced: "0.11.0",
        title: "Upgrade servers for the notification yellow dot",
        kind: Kind::Warning,
        content: &[
            Content::Heading("Action required"),
            Content::Text(
                "Upgrade miao-server on every host, including pooled localhost, to the build matching your dashboard.",
            ),
            Content::Text(""),
            Content::Text(
                "The logic that marks a finished turn as needing attention (the notification yellow dot) moved from the dashboard to the server. With an older server, finished turns will not arm the yellow dot. Your dashboard will not arm it either. Therefore, you will not see the yellow dot on any session on that server when a turn finishes.",
            ),
            Content::Text(""),
            Content::Heading("How to upgrade"),
            Content::Binding(
                Command::ManageHosts,
                "Open Hosts; select a host, then press u to upgrade.",
            ),
            Content::Text(
                "If you manage miao-server yourself, update its installation and restart the daemon. Direct-local sessions do not need a server upgrade.",
            ),
        ],
    },
];

/// Select complete releases containing an unacknowledged item, oldest first.
/// Acknowledgement controls whether the inbox opens, not which items can be
/// browsed within a release. Build metadata does not affect precedence.
pub(super) fn pending(
    catalog: &'static [Announcement],
    acknowledged: &[String],
    current: &Version,
) -> anyhow::Result<Vec<&'static Announcement>> {
    let mut changes = Vec::new();
    for change in catalog {
        let introduced = Version::parse(change.introduced)
            .with_context(|| format!("invalid version for announcement '{}'", change.id))?;
        if !introduced.cmp_precedence(current).is_gt() {
            changes.push((introduced, change));
        }
    }
    let unseen_releases: Vec<_> = changes
        .iter()
        .filter(|(_, item)| !acknowledged.iter().any(|id| id == item.id))
        .map(|(version, _)| version.clone())
        .collect();
    changes.retain(|(version, _)| {
        unseen_releases
            .iter()
            .any(|unseen| unseen.cmp_precedence(version).is_eq())
    });
    changes.sort_by(|(a, _), (b, _)| a.cmp_precedence(b));
    Ok(changes.into_iter().map(|(_, change)| change).collect())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    // Intentionally out of release order, with two changes in one release.
    pub(in crate::app) const EXAMPLE_ANNOUNCEMENTS: &[Announcement] = &[
        Announcement {
            id: "config-format",
            introduced: "0.12.0",
            title: "Configuration format changed",
            kind: Kind::Warning,
            content: &[
                Content::Text("Update the renamed setting."),
                Content::Code("[example]\nsetting = \"new\""),
            ],
        },
        Announcement {
            id: "session-shortcuts-x",
            introduced: "0.11.0",
            title: "Session shortcuts changed",
            kind: Kind::Update,
            content: &[Content::Text("Shortcut migration")],
        },
        Announcement {
            id: "future-change",
            introduced: "0.13.0",
            title: "A later change",
            kind: Kind::Update,
            content: &[Content::Text("Not yet applicable")],
        },
        Announcement {
            id: "connection-policy",
            introduced: "0.12.0",
            title: "Connection policy changed",
            kind: Kind::Warning,
            content: &[
                Content::Heading("Action required"),
                Content::Text("Review connection settings."),
            ],
        },
    ];

    #[test]
    fn catalog_entries_are_valid_and_have_unique_permanent_ids() {
        let mut ids = std::collections::HashSet::new();
        for change in ANNOUNCEMENTS {
            assert!(!change.id.is_empty());
            assert!(ids.insert(change.id), "duplicate id: {}", change.id);
            Version::parse(change.introduced).unwrap();
            assert!(!change.title.is_empty());
            assert!(!change.content.is_empty());
        }
    }

    #[test]
    fn skipped_releases_queue_every_applicable_change_in_order() {
        let pending = pending(EXAMPLE_ANNOUNCEMENTS, &[], &Version::new(0, 12, 0)).unwrap();
        assert_eq!(
            pending.iter().map(|change| change.id).collect::<Vec<_>>(),
            ["session-shortcuts-x", "config-format", "connection-policy"]
        );
    }

    #[test]
    fn acknowledged_items_do_not_replay_across_versions_or_builds() {
        let acknowledged =
            ["session-shortcuts-x", "config-format", "connection-policy"].map(String::from);
        for current in ["0.12.0", "0.11.0", "0.12.0+second"] {
            assert!(
                pending(
                    EXAMPLE_ANNOUNCEMENTS,
                    &acknowledged,
                    &Version::parse(current).unwrap(),
                )
                .unwrap()
                .is_empty()
            );
        }
        let pending = pending(
            EXAMPLE_ANNOUNCEMENTS,
            &["session-shortcuts-x".into()],
            &Version::new(0, 12, 0),
        )
        .unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|change| change.introduced == "0.12.0"));
    }

    #[test]
    fn an_unseen_item_shows_its_whole_release_but_not_other_releases() {
        let pending = pending(
            EXAMPLE_ANNOUNCEMENTS,
            &["session-shortcuts-x".into(), "config-format".into()],
            &Version::new(0, 12, 0),
        )
        .unwrap();
        assert_eq!(
            pending.iter().map(|item| item.id).collect::<Vec<_>>(),
            ["config-format", "connection-policy"]
        );
    }
}
