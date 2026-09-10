use super::{CodexConfig, monitor::Observation, transport};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

pub(crate) struct Relay {
    pub(crate) events: mpsc::Receiver<Observation>,
    input_paused: watch::Sender<bool>,
    pause_ack: watch::Receiver<bool>,
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    pub(crate) async fn start(config: &CodexConfig, path: &Path) -> Result<Self> {
        let upstream = config.socket_path()?;
        // Fail before launching the TUI, with the ordinary FailedToStart row.
        // No fallback may change the host's selected execution mode.
        let _ = transport::Client::connect(&upstream).await?;
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).context("binding Codex TUI relay")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let (tx, events) = mpsc::channel(128);
        let (input_paused, mut pause_rx) = watch::channel(false);
        let (ack_tx, pause_ack) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    changed = pause_rx.changed() => {
                        if changed.is_err() { break; }
                        ack_tx.send_replace(*pause_rx.borrow_and_update());
                        continue;
                    }
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let result = async {
                    let mut client = tokio::time::timeout(transport::DEADLINE,
                        tokio_tungstenite::accept_async_with_config(stream, Some(transport::limits())))
                        .await??;
                    let mut server = transport::connect(&upstream).await?;
                    loop {
                        tokio::select! {
                            biased;
                            changed = pause_rx.changed() => {
                                if changed.is_err() { break; }
                                ack_tx.send_replace(*pause_rx.borrow_and_update());
                            }
                            frame = client.next(), if !*pause_rx.borrow() => {
                                let Some(frame) = frame else { break };
                                let frame = frame?;
                                if let Message::Text(text) = &frame
                                    && let Ok(value) = serde_json::from_str::<Value>(text) {
                                    // Requests are reduced before crossing the channel;
                                    // environment/configuration never enters monitor state.
                                    let request = serde_json::json!({"id":value["id"],"method":value["method"]});
                                    if tx.send(Observation::Client(request)).await.is_err() { break; }
                                }
                                let closed = frame.is_close();
                                server.send(frame).await?;
                                if closed { break; }
                            }
                            frame = server.next() => {
                                let Some(frame) = frame else { break };
                                let frame = frame?;
                                if let Message::Text(text) = &frame
                                    && let Ok(value) = serde_json::from_str::<Value>(text)
                                    && observable(&value)
                                    && tx.send(Observation::Server(value)).await.is_err() { break; }
                                let closed = frame.is_close();
                                client.send(frame).await?;
                                if closed { break; }
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }.await;
                if result.is_err() {
                    // Transport errors can contain endpoint/authentication data.
                    tracing::debug!("Codex relay connection ended");
                }
                if tx.send(Observation::Disconnected).await.is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            events,
            input_paused,
            pause_ack,
            path: path.to_owned(),
            task,
        })
    }
    /// Confirm the relay has stopped forwarding new TUI requests before cleanup
    /// starts. Replies/events continue flowing; a failed stop releases the gate.
    pub(crate) async fn pause_input(&mut self, paused: bool) -> Result<()> {
        self.input_paused
            .send(paused)
            .context("Codex relay stopped")?;
        tokio::time::timeout(
            transport::DEADLINE,
            self.pause_ack.wait_for(|ack| *ack == paused),
        )
        .await
        .context("Codex relay did not acknowledge input pause")?
        .context("Codex relay stopped")?;
        Ok(())
    }
}

fn observable(value: &Value) -> bool {
    match value["method"].as_str() {
        None => {
            value
                .get("result")
                .is_some_and(|v| v.get("thread").is_some())
                || value.get("error").is_some()
        }
        Some(method) => {
            method.starts_with("thread/")
                || method.starts_with("turn/")
                || matches!(
                    method,
                    "item/started" | "item/completed" | "serverRequest/resolved" | "error"
                )
                || method.ends_with("/requestApproval")
                || method.ends_with("/requestUserInput")
                || method == "mcpServer/elicitation/request"
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
