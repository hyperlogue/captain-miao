//! A host asks the owning launcher to stop. The launcher keeps its selected
//! endpoint and TUI alive until cleanup succeeds; a rejection is retryable.
//! Qualified failures let explicit Kill retry main-thread cleanup or request
//! ForceStop to reap the TUI. Strict restart clients send neither follow-up.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

#[derive(Serialize, Deserialize)]
enum Request {
    Stop,
    // Sent only after a new launcher advertises InternalCreation. Older
    // launchers continue receiving the original strict Stop request.
    Kill,
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
    // Allows a Kill retry that still cleans the main thread, never ForceStop.
    InternalCreation,
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
    request: Request,
}

impl StopRequest {
    pub(super) fn is_forced(&self) -> bool {
        matches!(self.request, Request::ForceStop)
    }

    pub(super) fn allows_internal_creations(&self) -> bool {
        matches!(self.request, Request::Kill)
    }

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
        let mut listener = super::listener::Listener::new(listener);
        let (tx, requests) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            while let Ok(mut stream) = listener.accept().await {
                if let Ok(Ok(Some(request))) = tokio::time::timeout(
                    Duration::from_secs(2),
                    crate::protocol::read_frame::<_, Request>(&mut stream),
                )
                .await
                    && tx.send(StopRequest { stream, request }).await.is_err()
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

pub(super) fn request_kill(pid: u32) -> Result<()> {
    request(pid, Request::Kill, super::CONTROL_TIMEOUT)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[tokio::test]
    async fn control_recovers_deleted_and_replaced_sockets() {
        for replace in [false, true] {
            // Short even when macOS supplies a long per-user TMPDIR.
            let root = std::path::Path::new("/tmp")
                .join(format!("miao-control-{}-{replace}", std::process::id()));
            crate::state::create_dir_all_private(&root).unwrap();
            let path = root.join("control.sock");
            let mut control = Control::start(UnixListener::bind(&path).unwrap());
            let replacement = if replace {
                let other = root.join("other.sock");
                let listener = UnixListener::bind(&other).unwrap();
                std::fs::rename(other, &path).unwrap();
                Some(listener)
            } else {
                std::fs::remove_dir_all(&root).unwrap();
                None
            };
            let replaced_inode = std::fs::metadata(&path).ok().map(|m| m.ino());
            let recovered = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(metadata) = std::fs::metadata(&path)
                        && Some(metadata.ino()) != replaced_inode
                    {
                        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let mut client = UnixStream::connect(&path).await.unwrap();
                crate::protocol::write_frame(&mut client, &Request::Stop)
                    .await
                    .unwrap();
                let request = control.requests.recv().await.unwrap();
                assert!(!request.is_forced());
                request.reply(None).await;
                let reply: Reply = crate::protocol::read_frame(&mut client)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(reply.error.is_none());
            })
            .await;
            drop(control);
            drop(replacement);
            let _ = std::fs::remove_dir_all(root);
            assert!(
                recovered.is_ok(),
                "control did not recover; replacement={replace}"
            );
        }
    }
}
