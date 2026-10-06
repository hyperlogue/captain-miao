//! Dashboard preferences and the last completed startup version share one file.
//!
//! Check prior use before startup creates dashboard state. Fresh installs skip
//! existing announcements. Other users acknowledge the entire displayed batch;
//! browsing or saving preferences never dismisses it. Announcements are selected
//! strictly between the last dashboard version (exclusive) and current version
//! (inclusive). Acknowledging the batch records the current version.

use std::path::{Path, PathBuf};

use anyhow::Context;
use semver::Version;
use serde_json::{Map, Value};

use super::{
    DashboardOverrides,
    announcements::{self, Announcement},
};
use crate::state;

pub(super) struct DashboardState {
    path: PathBuf,
    version: &'static str,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self::new(state::dashboard_overrides_path(), env!("CARGO_PKG_VERSION"))
    }
}

impl DashboardState {
    pub(super) fn new(path: PathBuf, version: &'static str) -> Self {
        Self { path, version }
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
        // Startup acknowledgement can advance metadata after preferences load.
        overrides.last_dashboard_version = document
            .get("last_dashboard_version")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.ensure_parent()?;
        state::write_json_atomic(&self.path, &overrides)
    }

    /// Collect announcements introduced since the last dashboard version. Do
    /// this before the first session reload writes window bindings.
    pub(super) fn begin_startup(
        &self,
        window_bindings: &Path,
        catalog: &'static [Announcement],
    ) -> anyhow::Result<Vec<&'static Announcement>> {
        let document = self.read_document()?;
        let prior_use = self.path.is_file() || window_bindings.is_file();
        let previous = document
            .get("last_dashboard_version")
            .and_then(Value::as_str)
            .and_then(|version| Version::parse(version).ok());
        let current = Version::parse(self.version)?;
        let notices = announcements::pending(catalog, previous.as_ref(), &current)?;
        if !prior_use || notices.is_empty() {
            self.finish_startup()?;
            return Ok(Vec::new());
        }
        Ok(notices)
    }

    pub(super) fn finish_startup(&self) -> anyhow::Result<()> {
        // Patch only metadata. Startup can precede preference loading, and
        // acknowledgement must not reset pins, preferences or unknown fields.
        let mut document = self.read_document()?;
        // Retire the former per-item receipts without consulting them.
        document.remove("acknowledged_announcements");
        document.insert(
            "last_dashboard_version".into(),
            Value::String(self.version.into()),
        );
        self.ensure_parent()?;
        state::write_json_atomic(&self.path, &document)
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
    use super::super::announcements::{ANNOUNCEMENTS, tests::EXAMPLE_ANNOUNCEMENTS};
    use super::*;
    use serde_json::json;

    fn dashboard(dir: &Path, version: &'static str) -> DashboardState {
        DashboardState::new(dir.join("dashboard-overrides.json"), version)
    }

    #[test]
    fn fresh_dashboard_records_its_version_without_a_notice() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let bindings = temp.path().join("window-bindings.json");
        // Launcher and server use alone do not establish previous dashboard use.
        std::fs::create_dir(temp.path().join("sessions")).unwrap();
        std::fs::write(temp.path().join("server.pid"), "1").unwrap();
        assert!(
            dashboard
                .begin_startup(&bindings, ANNOUNCEMENTS)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            dashboard.load().unwrap().last_dashboard_version.as_deref(),
            Some("0.11.0")
        );
        std::fs::write(&bindings, "[]").unwrap();
        assert!(
            dashboard
                .begin_startup(&bindings, ANNOUNCEMENTS)
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
                    .begin_startup(&bindings, ANNOUNCEMENTS)
                    .unwrap()
                    .is_empty()
            );
            // Startup migrations and later preference saves cannot silently
            // acknowledge an open notice, even if the dashboard then quits.
            dashboard.save(DashboardOverrides::default()).unwrap();
            assert!(dashboard.load().unwrap().last_dashboard_version.is_none());
            assert!(
                !dashboard
                    .begin_startup(&bindings, ANNOUNCEMENTS)
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
                    .begin_startup(&bindings, ANNOUNCEMENTS)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn startup_uses_only_the_last_version_and_retires_old_item_receipts() {
        for (previous, expected) in [
            ("0.10.10", 2),
            ("0.11.0-rc.1", 2),
            ("unrecognized", 2),
            ("0.11.0", 0),
            ("0.11.0+local", 0),
            ("0.12.0", 0),
        ] {
            for receipts in [
                json!([]),
                json!(["session-shortcuts-x", "server-owned-attention"]),
                json!("obsolete"),
            ] {
                let temp = tempfile::tempdir().unwrap();
                let dashboard = dashboard(temp.path(), "0.11.0");
                let bindings = temp.path().join("window-bindings.json");
                state::write_json_atomic(
                    &dashboard.path,
                    &json!({
                        "last_dashboard_version": previous,
                        "acknowledged_announcements": receipts,
                    }),
                )
                .unwrap();
                let notices = dashboard.begin_startup(&bindings, ANNOUNCEMENTS).unwrap();
                assert_eq!(notices.len(), expected, "{previous}");
                assert_eq!(
                    dashboard.load().unwrap().last_dashboard_version.as_deref(),
                    Some(if expected == 0 { "0.11.0" } else { previous })
                );
                dashboard.save(DashboardOverrides::default()).unwrap();
                assert!(
                    !dashboard
                        .read_document()
                        .unwrap()
                        .contains_key("acknowledged_announcements")
                );
                assert_eq!(
                    dashboard
                        .begin_startup(&bindings, ANNOUNCEMENTS)
                        .unwrap()
                        .len(),
                    expected
                );
                dashboard.finish_startup().unwrap();
                assert!(
                    dashboard
                        .begin_startup(&bindings, ANNOUNCEMENTS)
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn later_additions_to_the_same_release_do_not_reopen_the_inbox() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let bindings = temp.path().join("window-bindings.json");
        assert!(
            dashboard
                .begin_startup(&bindings, &ANNOUNCEMENTS[..1])
                .unwrap()
                .is_empty()
        );
        assert!(
            dashboard
                .begin_startup(&bindings, ANNOUNCEMENTS)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_launch_without_announcements_records_the_current_version() {
        let temp = tempfile::tempdir().unwrap();
        let bindings = temp.path().join("window-bindings.json");
        let first = dashboard(temp.path(), "0.11.0");
        first
            .begin_startup(&bindings, EXAMPLE_ANNOUNCEMENTS)
            .unwrap();
        let next = dashboard(temp.path(), "0.11.1");
        assert!(
            next.begin_startup(&bindings, EXAMPLE_ANNOUNCEMENTS)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            next.load().unwrap().last_dashboard_version.as_deref(),
            Some("0.11.1")
        );
    }

    #[test]
    fn recording_the_dashboard_version_preserves_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let mut saved = json!({
            "pinned": [42],
            "follow_up": [43],
            "prefs": {"prevent_sleep": false},
            "future_preference": "preserve",
            "acknowledged_announcements": ["session-shortcuts-x"]
        });
        state::write_json_atomic(&dashboard.path, &saved).unwrap();
        dashboard.finish_startup().unwrap();
        saved["last_dashboard_version"] = json!("0.11.0");
        saved
            .as_object_mut()
            .unwrap()
            .remove("acknowledged_announcements");
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
    fn new_install_skips_the_entire_catalog_but_later_upgrades_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let first = dashboard(temp.path(), "0.11.0");
        let bindings = temp.path().join("window-bindings.json");
        assert!(
            first
                .begin_startup(&bindings, EXAMPLE_ANNOUNCEMENTS)
                .unwrap()
                .is_empty()
        );
        let next = dashboard(temp.path(), "0.12.0");
        let notices = next
            .begin_startup(&bindings, EXAMPLE_ANNOUNCEMENTS)
            .unwrap();
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
            next.begin_startup(&bindings, EXAMPLE_ANNOUNCEMENTS)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn failed_version_save_preserves_existing_state() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let saved = json!({"last_dashboard_version": "0.10.0", "pin_order": [42]});
        state::write_json_atomic(&dashboard.path, &saved).unwrap();
        // Prevent the atomic write without relying on Unix user permissions.
        std::fs::create_dir(dashboard.path.with_extension("tmp")).unwrap();
        assert!(dashboard.finish_startup().is_err());
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
