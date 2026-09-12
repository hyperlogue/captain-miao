//! Private Unix WebSocket transport. The relay never answers approvals or
//! retries user requests: the Codex TUI owns both, including reconnect policy.
use anyhow::{Context, Result};
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

/// An endpoint that cannot be reached is distinct from a rejected RPC or an
/// invalid protocol response. Only the former permits explicit force removal.
#[derive(Debug)]
pub(super) struct Unavailable;
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Codex app-server is unreachable")
    }
}
impl std::error::Error for Unavailable {}

fn classify_transport(error: anyhow::Error) -> anyhow::Error {
    let io = error.downcast_ref::<std::io::Error>().or_else(|| {
        match error.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
            Some(tokio_tungstenite::tungstenite::Error::Io(io)) => Some(io),
            _ => None,
        }
    });
    if io.is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::UnexpectedEof
        )
    }) || matches!(
        error.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
        Some(
            tokio_tungstenite::tungstenite::Error::ConnectionClosed
                | tokio_tungstenite::tungstenite::Error::AlreadyClosed
        )
    ) {
        error.context(Unavailable)
    } else {
        error
    }
}

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
    .context("Codex app-server connection timed out")
    .map_err(|error| error.context(Unavailable))?
    .map_err(classify_transport)
}

/// Preserve the protocol error code so callers can distinguish an unavailable
/// optional feature from a failed operation without parsing formatted errors.
#[derive(Debug)]
pub(super) struct RpcError {
    pub(super) method: String,
    pub(super) code: Option<i64>,
    pub(super) message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex {}: {}", self.method, self.message)
    }
}
impl std::error::Error for RpcError {}

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
            .await
            .map_err(|error| {
                if error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some()
                {
                    error.context(Unavailable)
                } else {
                    error
                }
            })?;
        client
            .socket
            .send(Message::Text(
                json!({"method":"initialized"}).to_string().into(),
            ))
            .await
            .map_err(anyhow::Error::from)
            .map_err(classify_transport)?;
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
                                return Err(RpcError {
                                    method: method.into(),
                                    code: error["code"].as_i64(),
                                    message: error["message"]
                                        .as_str()
                                        .unwrap_or("request failed")
                                        .into(),
                                }
                                .into());
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
            Err(anyhow::anyhow!("Codex app-server disconnected").context(Unavailable))
        })
        .await
        .with_context(|| format!("Codex {method} timed out"))?
        .map_err(classify_transport)
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
