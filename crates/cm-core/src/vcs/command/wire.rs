//! Translate the stable wire record at the seam, keeping compatibility states
//! out of checkout validation and the dashboard's prepared push queue.

use super::{CheckoutExpectation, PreparedVcsPull, PreparedVcsPush, valid_oid};
use crate::protocol::VcsPlan;

impl VcsPlan {
    fn expectation(&self) -> CheckoutExpectation {
        CheckoutExpectation {
            branch: self.branch.clone(),
            commit: self.commit.clone(),
            remote: self.remote.clone(),
            target_ref: self.target_ref.clone(),
            identity: self.identity.clone(),
        }
    }

    /// A preparation reply must include the preview, even when it is empty.
    pub fn into_prepared_push(self) -> Result<PreparedVcsPush, String> {
        if !self.push {
            return Err("host prepared a pull instead of a push".into());
        }
        if self.outgoing_commits.is_none() {
            return Err("host cannot preview outgoing commits; update its server and retry".into());
        }
        self.into_push_command()
    }

    /// Old execution requests lack preview metadata. It is not needed to run
    /// an already prepared push, so accept its absence only on this path.
    pub fn into_push_command(self) -> Result<PreparedVcsPush, String> {
        if !self.push {
            return Err("expected a prepared push".into());
        }
        Ok(PreparedVcsPush {
            expected: self.expectation(),
            set_upstream: self.set_upstream,
            outgoing_commits: self.outgoing_commits.unwrap_or_default(),
        })
    }

    pub fn into_prepared_pull(self) -> Result<PreparedVcsPull, String> {
        if self.push {
            return Err("host prepared a push instead of a pull".into());
        }
        let expected = self.expectation();
        let fetched_commit = self
            .target_commit
            .filter(|oid| valid_oid(oid))
            .ok_or("prepared pull has no valid fetched commit")?;
        Ok(PreparedVcsPull {
            expected,
            fetched_commit,
        })
    }
}

impl From<PreparedVcsPush> for VcsPlan {
    fn from(prepared: PreparedVcsPush) -> Self {
        let CheckoutExpectation {
            branch,
            commit,
            remote,
            target_ref,
            identity,
        } = prepared.expected;
        Self {
            push: true,
            branch,
            commit,
            remote,
            target_ref,
            identity,
            target_commit: None,
            outgoing_commits: Some(prepared.outgoing_commits),
            set_upstream: prepared.set_upstream,
        }
    }
}

impl From<PreparedVcsPull> for VcsPlan {
    fn from(prepared: PreparedVcsPull) -> Self {
        let CheckoutExpectation {
            branch,
            commit,
            remote,
            target_ref,
            identity,
        } = prepared.expected;
        Self {
            push: false,
            branch,
            commit,
            remote,
            target_ref,
            identity,
            target_commit: Some(prepared.fetched_commit),
            outgoing_commits: None,
            set_upstream: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // These are the original deployed record shapes, independent of the new
    // prepared types. Keep both directions compatible with them.
    fn legacy(push: bool) -> serde_json::Value {
        let mut value = json!({
            "push": push,
            "branch": "main",
            "commit": "a".repeat(40),
            "remote": "origin",
            "target_ref": "refs/heads/main",
            "target_commit": if push { None } else { Some("b".repeat(40)) },
            "identity": "test",
            "set_upstream": false,
        });
        if push {
            value["outgoing_commits"] = json!([]);
        }
        value
    }

    #[test]
    fn prepared_types_preserve_the_deployed_wire_format() {
        let push = PreparedVcsPush::for_test(Vec::new());
        let pull = PreparedVcsPull::for_test();
        assert_eq!(
            serde_json::to_value(VcsPlan::from(push.clone())).unwrap(),
            legacy(true)
        );
        assert_eq!(
            serde_json::to_value(VcsPlan::from(pull.clone())).unwrap(),
            legacy(false)
        );
        let wire: VcsPlan = serde_json::from_value(legacy(true)).unwrap();
        assert_eq!(wire.into_prepared_push().unwrap(), push);
        let wire: VcsPlan = serde_json::from_value(legacy(false)).unwrap();
        assert_eq!(wire.into_prepared_pull().unwrap(), pull);
    }

    #[test]
    fn missing_preview_is_accepted_only_for_execution() {
        for preview in [None, Some(serde_json::Value::Null)] {
            let mut value = legacy(true);
            value.as_object_mut().unwrap().remove("outgoing_commits");
            if let Some(preview) = preview {
                value["outgoing_commits"] = preview;
            }
            let wire: VcsPlan = serde_json::from_value(value).unwrap();
            assert!(
                wire.clone()
                    .into_prepared_push()
                    .unwrap_err()
                    .contains("cannot preview")
            );
            assert!(wire.into_push_command().is_ok());
        }
        let wire: VcsPlan = serde_json::from_value(legacy(true)).unwrap();
        assert!(
            wire.into_prepared_push()
                .unwrap()
                .outgoing_commits()
                .is_empty()
        );
    }

    #[test]
    fn wire_operations_cannot_be_confused() {
        let push = VcsPlan::for_test(true);
        let pull = VcsPlan::for_test(false);
        assert!(push.into_prepared_pull().is_err());
        assert!(pull.clone().into_prepared_push().is_err());
        assert!(pull.into_push_command().is_err());
    }

    #[test]
    fn pull_requires_a_fetched_commit() {
        for commit in [None, Some(""), Some("HEAD"), Some("bad-oid")] {
            let mut value = legacy(false);
            value["target_commit"] = json!(commit);
            let wire: VcsPlan = serde_json::from_value(value).unwrap();
            assert!(
                wire.into_prepared_pull()
                    .unwrap_err()
                    .contains("fetched commit")
            );
        }
    }
}
