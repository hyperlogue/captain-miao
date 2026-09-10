//! Private Unix WebSocket transport. The relay never answers approvals or
//! retries user requests: the Codex TUI owns both, including reconnect policy.
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

pub(super) type Socket = WebSocketStream<UnixStream>;
pub(super) const DEADLINE: Duration = Duration::from_secs(5);

pub(super) fn limits() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(64 * 1024 * 1024))
        .max_frame_size(Some(64 * 1024 * 1024))
}

pub(super) async fn connect(path: &Path) -> Result<Socket> {
    tokio::time::timeout(DEADLINE, async {
        let stream = UnixStream::connect(path)
            .await
            .context("connecting to Codex app-server socket")?;
        let (socket, _) =
            tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(limits()))
                .await
                .context("Codex app-server WebSocket handshake")?;
        Ok(socket)
    })
    .await
    .context("Codex app-server connection timed out")?
}

pub(super) struct Client {
    socket: Socket,
    next_id: u64,
}

impl Client {
    pub(super) async fn connect(path: &Path) -> Result<Self> {
        let mut client = Self {
            socket: connect(path).await?,
            next_id: 0,
        };
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {"name": "captain_miao", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"experimentalApi": true}
                }),
            )
            .await?;
        client
            .socket
            .send(Message::Text(
                json!({"method":"initialized"}).to_string().into(),
            ))
            .await?;
        Ok(client)
    }

    pub(super) async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        tokio::time::timeout(DEADLINE, async {
            self.socket
                .send(Message::Text(
                    json!({"id":id,"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await?;
            while let Some(frame) = self.socket.next().await {
                let frame = frame?;
                match frame {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text)?;
                        if value.get("id") == Some(&json!(id)) && value.get("method").is_none() {
                            if let Some(error) = value.get("error") {
                                bail!(
                                    "Codex {method}: {}",
                                    error["message"].as_str().unwrap_or("request failed")
                                );
                            }
                            return value.get("result").cloned().context("missing Codex result");
                        }
                        // These short-lived control clients do not subscribe to
                        // threads and must never compete with a TUI for approvals.
                    }
                    Message::Ping(_) => self.socket.flush().await?,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            bail!("Codex app-server disconnected")
        })
        .await
        .with_context(|| format!("Codex {method} timed out"))?
    }
}

/// Backend inventory is a synchronous interface. A dedicated thread keeps its
/// short RPC runtime independent of the caller's Tokio runtime (including tests).
pub(super) fn blocking<T: Send + 'static>(
    f: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(f)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Codex inventory worker stopped"))?
}
