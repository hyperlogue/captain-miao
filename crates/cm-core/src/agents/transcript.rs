//! Stateful readers keep parser memory out of the facts sent to callers.

use super::definition::AgentDefinition;
use crate::agent::TranscriptStats;
use std::path::Path;

pub(crate) trait TranscriptReader: Send {
    fn read(&mut self, path: &Path) -> TranscriptStats;
    fn adopt_model(&mut self, _model: &str) {}
}

pub(crate) struct BasicReader {
    definition: &'static dyn AgentDefinition,
    prior: Option<TranscriptStats>,
}

impl BasicReader {
    pub fn new(definition: &'static dyn AgentDefinition) -> Self {
        Self {
            definition,
            prior: None,
        }
    }
}

impl TranscriptReader for BasicReader {
    fn read(&mut self, path: &Path) -> TranscriptStats {
        let stats = self
            .definition
            .read_transcript_stats(path, self.prior.as_ref());
        self.prior = Some(stats.clone());
        stats
    }
    fn adopt_model(&mut self, model: &str) {
        if let Some(stats) = &mut self.prior {
            stats.adopt_model(model);
        }
    }
}
