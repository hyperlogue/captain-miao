//! Catalog of user-visible breaking changes, independent of startup UI and disk.
//!
//! To add an announcement, append a `BreakingChange` to `BREAKING_CHANGES` with
//! a unique, permanent id, the exact version introducing it, and its content.
//! Text, headings and code snippets cover general migrations; `Binding` resolves
//! a command through the active keymap when the notice concerns shortcuts.
//! Keep old entries and their versions unchanged so skipped releases work.
//! No new input handling, state fields or receipt files are needed per notice.

use semver::Version;

use super::keymap::Command;

pub(super) struct BreakingChange {
    pub id: &'static str,
    pub introduced: &'static str,
    pub title: &'static str,
    pub content: &'static [Content],
}

pub(super) enum Content {
    Text(&'static str),
    Heading(&'static str),
    Code(&'static str),
    Binding(Command, &'static str),
}

pub(super) const SESSION_SHORTCUTS_ID: &str = "session-shortcuts-x";

pub(super) const BREAKING_CHANGES: &[BreakingChange] = &[BreakingChange {
    id: SESSION_SHORTCUTS_ID,
    introduced: "0.11.0",
    title: "Session shortcuts changed",
    content: &[
        Content::Text(
            "The default kill key moved from x to X (Shift+x).\nBy default, x now dismisses the newest notification.",
        ),
        Content::Text(""),
        Content::Heading("Your active shortcuts"),
        Content::Binding(Command::KillSelected, "Kill selected session"),
        Content::Binding(Command::DismissNotification, "Dismiss newest notification"),
        Content::Text(""),
        Content::Heading("Edit these keys in config.toml (defaults shown):"),
        Content::Code("[keybinds]\nkill = \"X\"\ndismiss_notification = \"x\""),
        Content::Text(""),
        Content::Text("Restart miao to apply your changes."),
    ],
}];

/// Select every crossed change, oldest first. Multiple changes in one release
/// retain their catalog order. Build metadata does not affect precedence.
pub(super) fn pending(
    catalog: &'static [BreakingChange],
    previous: Option<&Version>,
    current: &Version,
) -> anyhow::Result<Vec<&'static BreakingChange>> {
    let mut changes = Vec::new();
    for change in catalog {
        let introduced = Version::parse(change.introduced)?;
        if !introduced.cmp_precedence(current).is_gt()
            && previous.is_none_or(|v| introduced.cmp_precedence(v).is_gt())
        {
            changes.push((introduced, change));
        }
    }
    changes.sort_by(|(a, _), (b, _)| a.cmp_precedence(b));
    Ok(changes.into_iter().map(|(_, change)| change).collect())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    // Intentionally out of release order, with two changes in one release.
    pub(in crate::app) const EXAMPLE_CHANGES: &[BreakingChange] = &[
        BreakingChange {
            id: "config-format",
            introduced: "0.12.0",
            title: "Configuration format changed",
            content: &[
                Content::Text("Update the renamed setting."),
                Content::Code("[example]\nsetting = \"new\""),
            ],
        },
        BreakingChange {
            id: SESSION_SHORTCUTS_ID,
            introduced: "0.11.0",
            title: "Session shortcuts changed",
            content: &[Content::Text("Shortcut migration")],
        },
        BreakingChange {
            id: "future-change",
            introduced: "0.13.0",
            title: "A later change",
            content: &[Content::Text("Not yet applicable")],
        },
        BreakingChange {
            id: "connection-policy",
            introduced: "0.12.0",
            title: "Connection policy changed",
            content: &[
                Content::Heading("Action required"),
                Content::Text("Review connection settings."),
            ],
        },
    ];

    #[test]
    fn catalog_entries_are_valid_and_have_unique_permanent_ids() {
        let mut ids = std::collections::HashSet::new();
        for change in BREAKING_CHANGES {
            assert!(!change.id.is_empty());
            assert!(ids.insert(change.id), "duplicate id: {}", change.id);
            Version::parse(change.introduced).unwrap();
            assert!(!change.title.is_empty());
            assert!(!change.content.is_empty());
        }
    }

    #[test]
    fn skipped_releases_queue_every_applicable_change_in_order() {
        let pending = pending(
            EXAMPLE_CHANGES,
            Some(&Version::new(0, 10, 0)),
            &Version::new(0, 12, 0),
        )
        .unwrap();
        assert_eq!(
            pending.iter().map(|change| change.id).collect::<Vec<_>>(),
            [SESSION_SHORTCUTS_ID, "config-format", "connection-policy"]
        );
    }

    #[test]
    fn equal_versions_downgrades_and_build_metadata_do_not_replay_changes() {
        for (previous, current) in [
            ("0.12.0", "0.12.0"),
            ("0.12.0", "0.11.0"),
            ("0.12.0+first", "0.12.0+second"),
        ] {
            assert!(
                pending(
                    EXAMPLE_CHANGES,
                    Some(&Version::parse(previous).unwrap()),
                    &Version::parse(current).unwrap(),
                )
                .unwrap()
                .is_empty()
            );
        }
        let pending = pending(
            EXAMPLE_CHANGES,
            Some(&Version::parse("0.12.0-rc.1").unwrap()),
            &Version::new(0, 12, 0),
        )
        .unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|change| change.introduced == "0.12.0"));
    }
}
