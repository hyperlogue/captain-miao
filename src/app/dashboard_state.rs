//! Dashboard preferences and announcement acknowledgements share one file.
//!
//! Check prior use before startup creates dashboard state. Fresh installs skip
//! existing announcements. Other users acknowledge the entire displayed batch;
//! browsing or saving preferences never dismisses it. Stable ids allow new items
//! within an already seen version, including feature promotions and warnings.

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
        overrides.acknowledged_announcements = document
            .get("acknowledged_announcements")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .context("reading announcement acknowledgements")?;
        self.ensure_parent()?;
        state::write_json_atomic(&self.path, &overrides)
    }

    /// Collect every unseen, released announcement. The caller must do
    /// this before the first session reload writes window bindings.
    pub(super) fn begin_startup(
        &self,
        window_bindings: &Path,
        catalog: &'static [Announcement],
    ) -> anyhow::Result<Vec<&'static Announcement>> {
        let document = self.read_document()?;
        let prior_use = self.path.is_file() || window_bindings.is_file();
        let current = Version::parse(self.version)?;
        let notices = announcements::pending(catalog, &acknowledged(&document)?, &current)?;
        if !prior_use || notices.is_empty() {
            self.finish_startup(&notices)?;
            return Ok(Vec::new());
        }
        Ok(notices)
    }

    pub(super) fn finish_startup(&self, notices: &[&Announcement]) -> anyhow::Result<()> {
        // Patch only metadata. Startup can precede preference loading, and
        // acknowledgement must not reset pins, preferences or unknown fields.
        let mut document = self.read_document()?;
        let mut ids = acknowledged(&document)?;
        for notice in notices {
            if !ids.iter().any(|id| id == notice.id) {
                ids.push(notice.id.to_owned());
            }
        }
        document.insert(
            "acknowledged_announcements".into(),
            serde_json::to_value(ids)?,
        );
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

fn acknowledged(document: &Map<String, Value>) -> anyhow::Result<Vec<String>> {
    if let Some(ids) = document.get("acknowledged_announcements") {
        return serde_json::from_value(ids.clone())
            .context("reading announcement acknowledgements");
    }
    // The only announcement shipped with version-only tracking was the x/X
    // change. Preserve that acknowledgement without hiding later additions to
    // the same release. This migration list must not grow with the catalog.
    let previous = document
        .get("last_dashboard_version")
        .and_then(Value::as_str)
        .and_then(|v| Version::parse(v).ok());
    Ok(
        if previous.is_some_and(|v| !v.cmp_precedence(&Version::new(0, 11, 0)).is_lt()) {
            vec!["session-shortcuts-x".into()]
        } else {
            Vec::new()
        },
    )
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
            dashboard
                .finish_startup(&ANNOUNCEMENTS.iter().collect::<Vec<_>>())
                .unwrap();
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
    fn legacy_version_only_acknowledges_the_original_shortcut_item() {
        for (previous, expected) in [
            ("0.10.10", 2),
            ("0.11.0-rc.1", 2),
            ("unrecognized", 2),
            ("0.11.0", 1),
            ("0.11.0+local", 1),
            ("0.12.0", 1),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let dashboard = dashboard(temp.path(), "0.11.0");
            state::write_json_atomic(
                &dashboard.path,
                &json!({"last_dashboard_version": previous}),
            )
            .unwrap();
            let pending = dashboard
                .begin_startup(&temp.path().join("window-bindings.json"), ANNOUNCEMENTS)
                .unwrap();
            assert_eq!(pending.len(), expected, "{previous}");
            assert_eq!(
                dashboard.load().unwrap().last_dashboard_version.as_deref(),
                Some(previous)
            );
        }
    }

    #[test]
    fn fresh_install_can_receive_a_later_addition_to_the_same_release() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        let bindings = temp.path().join("window-bindings.json");
        assert!(
            dashboard
                .begin_startup(&bindings, &ANNOUNCEMENTS[..1])
                .unwrap()
                .is_empty()
        );
        let notices = dashboard.begin_startup(&bindings, ANNOUNCEMENTS).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].id, "server-owned-attention");
        dashboard.save(DashboardOverrides::default()).unwrap();
        assert_eq!(
            dashboard
                .begin_startup(&bindings, ANNOUNCEMENTS)
                .unwrap()
                .len(),
            1
        );
        dashboard.finish_startup(&notices).unwrap();
        assert!(
            dashboard
                .begin_startup(&bindings, ANNOUNCEMENTS)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn acknowledgements_survive_a_downgrade_and_preference_save() {
        let temp = tempfile::tempdir().unwrap();
        let bindings = temp.path().join("window-bindings.json");
        let latest = dashboard(temp.path(), "0.12.0");
        latest
            .begin_startup(&bindings, EXAMPLE_ANNOUNCEMENTS)
            .unwrap();
        let older = dashboard(temp.path(), "0.11.0");
        assert!(
            older
                .begin_startup(&bindings, &EXAMPLE_ANNOUNCEMENTS[1..2])
                .unwrap()
                .is_empty()
        );
        older.save(DashboardOverrides::default()).unwrap();
        assert!(
            latest
                .begin_startup(&bindings, EXAMPLE_ANNOUNCEMENTS)
                .unwrap()
                .is_empty()
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
            "future_preference": "preserve"
        });
        state::write_json_atomic(&dashboard.path, &saved).unwrap();
        dashboard
            .finish_startup(&ANNOUNCEMENTS.iter().collect::<Vec<_>>())
            .unwrap();
        saved["last_dashboard_version"] = json!("0.11.0");
        saved["acknowledged_announcements"] =
            json!(["session-shortcuts-x", "server-owned-attention"]);
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
        next.finish_startup(&notices).unwrap();
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
        assert!(dashboard.finish_startup(&[]).is_err());
        assert_eq!(state::read_json::<Value>(&dashboard.path), Some(saved));
    }

    #[test]
    fn version_tracking_does_not_replace_malformed_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let dashboard = dashboard(temp.path(), "0.11.0");
        std::fs::write(&dashboard.path, "{broken").unwrap();
        assert!(dashboard.finish_startup(&[]).is_err());
        assert_eq!(std::fs::read_to_string(&dashboard.path).unwrap(), "{broken");
    }
}
