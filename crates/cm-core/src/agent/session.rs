//! Agent-neutral execution. A task owns its processes and observations; the
//! launcher owns persistence and sends cooperative termination requests.

use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use super::{ResumeCandidate, UNKNOWN_AGENT_REFUSAL};
use crate::agents::definition::AgentDefinition;
use crate::state::LauncherState;

pub(crate) type SessionTask = BoxFuture<'static, Result<SessionExit>>;

/// Configuration belongs to the adapter. Running tasks capture it when run is
/// called, so replacing an adapter cannot redirect an existing session.
pub(crate) trait AgentAdapter: Send + Sync {
    fn run(&self, session: SessionContext) -> SessionTask;

    fn list_resumable(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>>;
}

pub(crate) struct LaunchRequest {
    pub cwd: String,
    pub args: Vec<String>,
    pub socket: PathBuf,
    pub settings: PathBuf,
    pub shim: Option<PathBuf>,
}

pub(crate) struct SessionContext {
    pub request: LaunchRequest,
    pub state: LauncherState,
    pub listener: UnixListener,
    pub updates: SessionPublisher,
    pub commands: SessionCommands,
}

pub(crate) struct SessionExit {
    pub code: i32,
}

pub(crate) enum SessionCommand {
    Stop,
}

pub(crate) struct SessionCommands(pub mpsc::UnboundedReceiver<SessionCommand>);

impl SessionCommands {
    /// Closing the control channel is not evidence that agent work stopped.
    pub async fn stop(&mut self) {
        match self.0.recv().await {
            Some(SessionCommand::Stop) => (),
            None => std::future::pending().await,
        }
    }
}

pub(crate) struct SessionUpdate {
    pub state: LauncherState,
    pub receipt: Option<oneshot::Sender<Result<()>>>,
}

#[derive(Clone)]
pub(crate) struct SessionPublisher(mpsc::UnboundedSender<SessionUpdate>);

impl SessionPublisher {
    #[cfg(test)]
    pub fn test_writer() -> Self {
        // Run the launcher's own update loop, retries included. The
        // placeholder row is never written: drive replaces it with the first
        // update before any write or retry.
        let (writer, updates) = Self::channel();
        let (commands, _) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut state = LauncherState::for_test(
                crate::agent::AgentKind::Unknown,
                crate::state::SessionStatus::Idle,
            );
            let _ = crate::launcher::drive_updates(updates, commands, &mut state).await;
        });
        writer
    }

    pub fn channel() -> (Self, mpsc::UnboundedReceiver<SessionUpdate>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }

    /// Publish facts without waiting on the filesystem. The launcher retains
    /// and retries a failed write; this never cancels the adapter task.
    pub fn publish(&self, state: &LauncherState) -> Result<()> {
        self.0
            .send(SessionUpdate {
                state: state.clone(),
                receipt: None,
            })
            .map_err(|_| anyhow::anyhow!("session writer disconnected"))
    }

    /// Acknowledged persistence for startup and cleanup paths that need to
    /// distinguish a saved row from an unavailable state store.
    pub async fn write(&self, state: &LauncherState) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(SessionUpdate {
                state: state.clone(),
                receipt: Some(tx),
            })
            .map_err(|_| anyhow::anyhow!("session writer disconnected"))?;
        rx.await.context("session writer lost its receipt")?
    }
}

pub(crate) struct NativeAdapter {
    pub definition: &'static dyn AgentDefinition,
    pub approval_grace: std::time::Duration,
}

impl AgentAdapter for NativeAdapter {
    fn run(&self, session: SessionContext) -> SessionTask {
        let definition = self.definition;
        let grace = self.approval_grace;
        Box::pin(async move { crate::agents::hooked::run(definition, session, grace).await })
    }

    fn list_resumable(
        &self,
        limit: usize,
        cache: &mut crate::resume::AgentCache,
    ) -> Result<Vec<ResumeCandidate>> {
        self.definition.list_resumable_cached(limit, cache)
    }
}

pub(crate) fn refuse_unknown() -> Result<Box<dyn AgentAdapter>> {
    anyhow::bail!(UNKNOWN_AGENT_REFUSAL)
}
