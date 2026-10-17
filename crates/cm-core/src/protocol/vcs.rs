//! Compatibility representation for the original PrepareVcs/RunVcs frames.
//!
//! Keep this flat JSON shape stable for deployed peers. Callers convert it to
//! the operation-specific prepared types before review or execution.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsPlan {
    pub(crate) push: bool,
    pub(crate) branch: String,
    pub(crate) commit: String,
    pub(crate) remote: String,
    pub(crate) target_ref: String,
    pub(crate) target_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outgoing_commits: Option<Vec<crate::vcs::VcsCommit>>,
    pub(crate) identity: String,
    pub(crate) set_upstream: bool,
}

impl VcsPlan {
    pub fn is_push(&self) -> bool {
        self.push
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(push: bool) -> Self {
        if push {
            crate::vcs::PreparedVcsPush::for_test(Vec::new()).into()
        } else {
            crate::vcs::PreparedVcsPull::for_test().into()
        }
    }
}
