//! A host asks the owning launcher to stop. The launcher keeps its selected
//! endpoint and TUI alive until cleanup succeeds; a rejection is retryable.
//! Qualified failures let explicit Kill request ForceStop to reap the TUI and
//! release local control. Strict restart clients never send that follow-up.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

#[derive(Serialize, Deserialize)]
enum Request {
    Stop,
    ForceStop,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    force_removal: Option<ForceRemoval>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ForceRemoval {
    Unreachable,
    ThreadMissing,
    #[serde(other)]
    Denied,
}

#[derive(Debug)]
pub(super) struct StopFailure {
    pub message: String,
    pub force_removal: Option<ForceRemoval>,
}
impl std::fmt::Display for StopFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for StopFailure {}

pub(crate) struct StopRequest {
    stream: UnixStream,
    pub(super) force: bool,
}

impl StopRequest {
    /// Flush the acknowledgement before the launcher exits and drops its tasks.
    pub(crate) async fn reply(self, error: Option<String>) {
        self.respond(error, None).await;
    }

    pub(super) async fn respond(
        mut self,
        error: Option<String>,
        force_removal: Option<ForceRemoval>,
    ) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            crate::protocol::write_frame(
                &mut self.stream,
                &Reply {
                    error,
                    force_removal,
                },
            ),
        )
        .await;
    }
}

pub(crate) struct Control {
    pub(crate) requests: mpsc::Receiver<StopRequest>,
    task: tokio::task::JoinHandle<()>,
}

impl Control {
    /// App-server launchers use the otherwise unused hook socket for control.
    /// It is already private and owned by the launcher's ordinary cleanup.
    pub(crate) fn start(listener: UnixListener) -> Self {
        let (tx, requests) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                if let Ok(Ok(Some(request))) = tokio::time::timeout(
                    Duration::from_secs(2),
                    crate::protocol::read_frame::<_, Request>(&mut stream),
                )
                .await
                    && tx
                        .send(StopRequest {
                            stream,
                            force: matches!(request, Request::ForceStop),
                        })
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });
        Self { requests, task }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) fn request_stop(pid: u32) -> Result<()> {
    request(pid, Request::Stop, super::CONTROL_TIMEOUT)
}

pub(super) fn force_stop(pid: u32) -> Result<()> {
    request(pid, Request::ForceStop, Duration::from_secs(3))
}

fn request(pid: u32, request: Request, timeout: Duration) -> Result<()> {
    let path = crate::state::runtime_dir()
        .join("launchers")
        .join(format!("{pid}.sock"));
    super::transport::blocking(async move {
        tokio::time::timeout(timeout, async {
            let mut stream = UnixStream::connect(path)
                .await
                .context("connecting to Codex launcher control")?;
            crate::protocol::write_frame(&mut stream, &request).await?;
            let reply: Reply = crate::protocol::read_frame(&mut stream)
                .await?
                .context("Codex launcher disconnected before confirming cleanup")?;
            match reply.error {
                Some(message) => Err(StopFailure {
                    message,
                    force_removal: reply.force_removal,
                }
                .into()),
                None => Ok(()),
            }
        })
        .await
        .context("Codex launcher did not confirm cleanup in time")?
    })
}
