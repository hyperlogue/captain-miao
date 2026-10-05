//! Dashboard preferences and the last completed startup version share one file.
//!
//! Version checks happen before startup creates dashboard state. A fresh install
//! records the current version immediately; an upgrade with a notice records it
//! only after acknowledgement. Ordinary preference saves preserve that version.

use std::path::{Path, PathBuf};

use anyhow::Context;
use semver::Version;
use serde_json::{Map, Value};

use super::{
    DashboardOverrides,
    breaking_changes::{self, BreakingChange, SESSION_SHORTCUTS_ID},
};
use crate::state;

pub(super) struct DashboardState {
    path: PathBuf,
    legacy_notice: PathBuf,
    version: &'static str,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self::new(
            state::dashboard_overrides_path(),
            state::keybinding_notice_path(),
            env!("CARGO_PKG_VERSION"),
        )
    }
}

impl DashboardState {
    pub(super) fn new(path: PathBuf, legacy_notice: PathBuf, version: &'static str) -> Self {
        Self {
            path,
            legacy_notice,
            version,
        }
    }

    pub(super) fn load(&self) -> Option<DashboardOverrides> {
        state::read_json(&self.path)
    }

    pub(super) fn save(&self, mut overrides: DashboardOverrides) -> anyhow::Result<()> {
        let document = self.read_document()?;
        if document.contains_key("pinned") || document.contains_key("follow_up") {
            // Only discard the legacy flag arrays after the shared store has
            // imported them durably. A failed import leaves preferences intact.
            let root = self
                .path
                .parent()
                .context("dashboard state has no parent")?;
            cm_core::session_flags::SessionFlagsStore::new(root.to_path_buf()).migrate_legacy()?;
        }
        // Read the latest version rather than carrying a stale copy in App:
        // startup acknowledgement can advance it after preferences were loaded.
        overrides.last_dashboard_version = state::read_json::<Value>(&self.path)
            .and_then(|v| v.get("last_dashboard_version")?.as_str().map(str::to_owned));
        self.ensure_parent()?;
        state::write_json_atomic(&self.path, &overrides)
    }

    /// Queue every applicable breaking change. The caller must do
    /// this before the first session reload writes window bindings.
    pub(super) fn begin_startup(
        &self,
        window_bindings: &Path,
        catalog: &'static [BreakingChange],
    ) -> anyhow::Result<Vec<&'static BreakingChange>> {
        let document = self.read_document()?;
        let prior_use = self.path.is_file() || window_bindings.is_file();
        let acknowledged_legacy = state::read_json::<bool>(&self.legacy_notice) == Some(true);
        let previous = document
            .get("last_dashboard_version")
            .and_then(Value::as_str)
            .and_then(|v| Version::parse(v).ok());
        let current = Version::parse(self.version)?;
        let mut notices = if prior_use {
            breaking_changes::pending(catalog, previous.as_ref(), &current)?
        } else {
            Vec::new()
        };
        // The legacy receipt acknowledged only the x/X announcement, not all
        // changes through the version being launched today.
        if acknowledged_legacy {
            notices.retain(|change| change.id != SESSION_SHORTCUTS_ID);
        }
        if notices.is_empty() {
            self.finish_startup()?;
        }
        Ok(notices)
    }

    pub(super) fn finish_startup(&self) -> anyhow::Result<()> {
        // Patch only metadata. Startup can precede preference loading, and
        // acknowledgement must not reset pins, preferences or unknown fields.
        let mut document = self.read_document()?;
        document.insert(
            "last_dashboard_version".into(),
            Value::String(self.version.into()),
        );
        self.ensure_parent()?;
        state::write_json_atomic(&self.path, &document)?;
        // Never delete the old receipt until its replacement is safely saved.
        match std::fs::remove_file(&self.legacy_notice) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("removing the legacy shortcut notice receipt"),
        }
    }

    fn read_document(&self) -> anyhow::Result<Map<String, Value>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading dashboard state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(error) => Err(error).context("reading dashboard state"),
        }
    }

    fn ensure_parent(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            state::create_dir_all_private(parent)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::breaking_changes::{BREAKING_CHANGES, tests::EXAMPLE_CHANGES};
    use super::*;
    use serde_json::json;

    fn dashboard(dir: &Path, version: &'static str) -> DashboardState {
        DashboardState::new(
            dir.join("dashboard-overrides.json"),
            dir.join("keybinding-notice-x-v1.json"),
            version,
        )
    }

    #[test]
    fn fresh_dashboard_records_its_version_without_a_notice_or_extra_file() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let bindings = temp.path().join("window-bindings.json");
        // Launcher and server use alone do not establish previous dashboard use.
        std::fs::create_dir(temp.path().join("sessions")).unwrap();
        std::fs::write(temp.path().join("server.pid"), "1").unwrap();
        assert!(
            dashboard
                .begin_startup(&bindings, BREAKING_CHANGES)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            dashboard.load().unwrap().last_dashboard_version.as_deref(),
            Some("0.11.0")
        );
        assert!(!dashboard.legacy_notice.exists());
        std::fs::write(&bindings, "[]").unwrap();
        assert!(
            dashboard
                .begin_startup(&bindings, BREAKING_CHANGES)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unversioned_existing_users_see_the_notice_until_acknowledged() {
        for preferences_exist in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let dashboard = dashboard(temp.path(), "0.11.0");
            let bindings = temp.path().join("window-bindings.json");
            if preferences_exist {
                std::fs::write(&dashboard.path, "{}").unwrap();
            } else {
                std::fs::write(&bindings, "[]").unwrap();
            }
            assert!(
                !dashboard
                    .begin_startup(&bindings, BREAKING_CHANGES)
                    .unwrap()
                    .is_empty()
            );
            // Startup migrations and later preference saves cannot silently
            // acknowledge an open notice, even if the dashboard then quits.
            dashboard.save(DashboardOverrides::default()).unwrap();
            assert!(dashboard.load().unwrap().last_dashboard_version.is_none());
            assert!(
                !dashboard
                    .begin_startup(&bindings, BREAKING_CHANGES)
                    .unwrap()
                    .is_empty()
            );
            dashboard.finish_startup().unwrap();
            dashboard.save(DashboardOverrides::default()).unwrap();
            assert_eq!(
                dashboard.load().unwrap().last_dashboard_version.as_deref(),
                Some("0.11.0")
            );
            assert!(
                dashboard
                    .begin_startup(&bindings, BREAKING_CHANGES)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn only_crossing_the_change_version_triggers_the_notice() {
        for (previous, current, expected) in [
            ("0.9.0", "0.11.0", true),
            ("0.10.10", "0.11.0", true),
            ("0.11.0-rc.1", "0.11.0", true),
            ("0.11.0", "0.11.0", false),
            ("0.11.0+local", "0.11.0", false),
            ("0.11.0", "0.12.0", false),
            ("0.12.0", "0.11.0", false),
            ("1.0.0", "0.11.0", false),
            ("0.9.0", "0.10.0", false),
            ("unrecognized", "0.11.0", true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dashboard = dashboard(temp.path(), current);
            state::write_json_atomic(
                &dashboard.path,
                &json!({"last_dashboard_version": previous}),
            )
            .unwrap();
            assert_eq!(
                dashboard
                    .begin_startup(&temp.path().join("window-bindings.json"), BREAKING_CHANGES)
                    .unwrap()
                    .is_empty(),
                !expected,
                "{previous} -> {current}"
            );
            assert_eq!(
                dashboard.load().unwrap().last_dashboard_version.as_deref(),
                Some(if expected { previous } else { current })
            );
        }
    }

    #[test]
    fn legacy_acknowledgement_migrates_without_resetting_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let mut saved = json!({
            "pinned": [42],
            "follow_up": [43],
            "prefs": {"prevent_sleep": false},
            "future_preference": "preserve"
        });
        state::write_json_atomic(&dashboard.path, &saved).unwrap();
        state::write_json_atomic(&dashboard.legacy_notice, &true).unwrap();
        assert!(
            dashboard
                .begin_startup(&temp.path().join("window-bindings.json"), BREAKING_CHANGES)
                .unwrap()
                .is_empty()
        );
        assert!(!dashboard.legacy_notice.exists());
        saved["last_dashboard_version"] = json!("0.11.0");
        assert_eq!(state::read_json::<Value>(&dashboard.path), Some(saved));
        dashboard.save(dashboard.load().unwrap()).unwrap();
        let overrides = dashboard.load().unwrap();
        assert_eq!(overrides.pin_order, vec![42]);
        let document: Value = state::read_json(&dashboard.path).unwrap();
        assert!(document.get("pinned").is_none());
        assert!(document.get("follow_up").is_none());
        assert_eq!(overrides.prefs.prevent_sleep, Some(false));
        assert_eq!(overrides.last_dashboard_version.as_deref(), Some("0.11.0"));
    }

    #[test]
    fn preference_save_removes_legacy_flags_only_after_a_durable_import() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let pid = std::process::id();
        let saved = json!({"pinned": [pid], "follow_up": [pid]});
        state::write_json_atomic(&dashboard.path, &saved).unwrap();
        let row = state::LauncherState {
            launcher_pid: pid,
            ..state::LauncherState::for_test(
                crate::agent::AgentControl::Claude,
                state::SessionStatus::Idle,
            )
        };
        let sessions = temp.path().join("sessions");
        state::create_dir_all_private(&sessions).unwrap();
        state::write_json_atomic(&sessions.join(format!("{pid}.json")), &row).unwrap();

        let blocked = temp.path().join("session-flags.tmp");
        std::fs::create_dir(&blocked).unwrap();
        assert!(dashboard.save(dashboard.load().unwrap()).is_err());
        assert_eq!(state::read_json::<Value>(&dashboard.path), Some(saved));

        std::fs::remove_dir(blocked).unwrap();
        dashboard.save(dashboard.load().unwrap()).unwrap();
        let document: Value = state::read_json(&dashboard.path).unwrap();
        assert!(document.get("pinned").is_none());
        assert!(document.get("follow_up").is_none());
        assert_eq!(document["pin_order"], json!([pid]));
        let flags: std::collections::HashMap<state::SessionKey, state::SessionFlags> =
            state::read_json(&temp.path().join("session-flags.json")).unwrap();
        assert_eq!(
            flags[&row.key()],
            state::SessionFlags {
                pinned: true,
                follow_up: true
            }
        );
    }

    #[test]
    fn legacy_receipt_never_acknowledges_other_changes_in_skipped_releases() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.12.0");
        let bindings = temp.path().join("window-bindings.json");
        std::fs::write(&bindings, "[]").unwrap();
        state::write_json_atomic(&dashboard.legacy_notice, &true).unwrap();
        for _ in 0..2 {
            let notices = dashboard.begin_startup(&bindings, EXAMPLE_CHANGES).unwrap();
            assert_eq!(
                notices.iter().map(|change| change.id).collect::<Vec<_>>(),
                ["config-format", "connection-policy"]
            );
            assert!(dashboard.legacy_notice.exists());
            assert!(!dashboard.path.exists(), "no acknowledgement yet");
        }
        dashboard.finish_startup().unwrap();
        assert!(!dashboard.legacy_notice.exists());
        assert!(
            dashboard
                .begin_startup(&bindings, EXAMPLE_CHANGES)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn new_install_skips_the_entire_catalog_but_later_upgrades_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let first = dashboard(temp.path(), "0.11.0");
        let bindings = temp.path().join("window-bindings.json");
        assert!(
            first
                .begin_startup(&bindings, EXAMPLE_CHANGES)
                .unwrap()
                .is_empty()
        );
        let next = dashboard(temp.path(), "0.12.0");
        let notices = next.begin_startup(&bindings, EXAMPLE_CHANGES).unwrap();
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().all(|change| change.introduced == "0.12.0"));
        assert_eq!(
            next.load().unwrap().last_dashboard_version.as_deref(),
            Some("0.11.0")
        );
        next.finish_startup().unwrap();
        assert_eq!(
            next.load().unwrap().last_dashboard_version.as_deref(),
            Some("0.12.0")
        );
        assert!(
            next.begin_startup(&bindings, EXAMPLE_CHANGES)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_unacknowledged_legacy_receipt_is_removed_after_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let bindings = temp.path().join("window-bindings.json");
        std::fs::write(&bindings, "[]").unwrap();
        state::write_json_atomic(&dashboard.legacy_notice, &false).unwrap();
        assert!(
            !dashboard
                .begin_startup(&bindings, BREAKING_CHANGES)
                .unwrap()
                .is_empty()
        );
        assert!(dashboard.legacy_notice.exists());
        dashboard.finish_startup().unwrap();
        assert!(!dashboard.legacy_notice.exists());
        assert!(
            dashboard
                .begin_startup(&bindings, BREAKING_CHANGES)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn failed_migration_preserves_the_old_receipt_and_state() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let saved = json!({"pinned": [42]});
        state::write_json_atomic(&dashboard.path, &saved).unwrap();
        state::write_json_atomic(&dashboard.legacy_notice, &true).unwrap();
        // Prevent the atomic write without relying on Unix user permissions.
        std::fs::create_dir(dashboard.path.with_extension("tmp")).unwrap();
        assert!(
            dashboard
                .begin_startup(&temp.path().join("window-bindings.json"), BREAKING_CHANGES)
                .is_err()
        );
        assert_eq!(
            state::read_json::<bool>(&dashboard.legacy_notice),
            Some(true)
        );
        assert_eq!(state::read_json::<Value>(&dashboard.path), Some(saved));
    }

    #[test]
    fn version_tracking_does_not_replace_malformed_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        std::fs::write(&dashboard.path, "{broken").unwrap();
        assert!(dashboard.finish_startup().is_err());
        assert_eq!(std::fs::read_to_string(&dashboard.path).unwrap(), "{broken");
    }
}
