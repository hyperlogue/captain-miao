//! A host asks the owning launcher to stop. The launcher keeps its selected
//! endpoint and TUI alive until cleanup succeeds; a rejection is retryable.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

#[derive(Serialize, Deserialize)]
enum Request {
    Stop,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    error: Option<String>,
}

pub(crate) struct StopRequest(UnixStream);

impl StopRequest {
    /// Flush the acknowledgement before the launcher exits and drops its tasks.
    pub(crate) async fn reply(mut self, error: Option<String>) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            crate::protocol::write_frame(&mut self.0, &Reply { error }),
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
                if matches!(
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        crate::protocol::read_frame::<_, Request>(&mut stream)
                    )
                    .await,
                    Ok(Ok(Some(Request::Stop)))
                ) && tx.send(StopRequest(stream)).await.is_err()
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
    let path = crate::state::runtime_dir()
        .join("launchers")
        .join(format!("{pid}.sock"));
    super::transport::blocking(async move {
        tokio::time::timeout(super::CONTROL_TIMEOUT, async {
            let mut stream = UnixStream::connect(path)
                .await
                .context("connecting to Codex launcher control")?;
            crate::protocol::write_frame(&mut stream, &Request::Stop).await?;
            let reply: Reply = crate::protocol::read_frame(&mut stream)
                .await?
                .context("Codex launcher disconnected before confirming cleanup")?;
            match reply.error {
                Some(error) => anyhow::bail!("{error}"),
                None => Ok(()),
            }
        })
        .await
        .context("Codex launcher did not confirm cleanup in time")?
    })
}
