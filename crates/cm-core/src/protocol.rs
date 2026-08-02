//! Wire protocol between a `captain-miao server` (one per host) and a
//! dashboard's `RemoteBackend`. Length-prefixed JSON frames over a (possibly
//! ssh-forwarded) unix socket — see `docs/remote-sessions.md` §14.5.
//!
//! Framing: a 4-byte big-endian length followed by that many JSON bytes. JSON
//! (not a compact binary codec) keeps frames debuggable and rides serde's
//! existing derives on `LauncherState` / `ResumeCandidate`; the payloads are
//! small (state is snippet-capped) so the overhead is irrelevant.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::agent::ResumeCandidate;
use crate::backend::OpenSpec;
use crate::state::LauncherState;

/// Bumped on any incompatible frame change; the handshake refuses on mismatch.
/// v2 added `OpenSession`/`Opened` (Phase 3 remote spawn). v3 added the
/// host-filesystem queries the workdir picker needs for a remote launch
/// (`ListRecentDirs`/`CompletePath`/`CheckDir` + replies).
pub const PROTOCOL_VERSION: u32 = 3;

/// Cap on a single inbound frame, so a peer can't make us allocate unbounded.
const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

/// Client → server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame")]
pub enum ClientFrame {
    /// First frame: announce versions. The server replies `Welcome`.
    Hello {
        client_version: String,
        protocol: u32,
    },
    /// Start the live session stream (server pushes `Snapshot`, then
    /// `Delta`/`Removed` as sessions change).
    Subscribe,
    /// Resumable (dormant) sessions on this host, capped at `limit`.
    ListResumable { req_id: u64, limit: usize },
    /// SIGTERM the agent process `child_pid`.
    KillSession { req_id: u64, child_pid: u32 },
    /// Start a launcher inside the host's pty pool (Phase 3). The server creates
    /// the pool session, then replies `Opened` with its name.
    OpenSession { req_id: u64, spec: OpenSpec },
    /// The host's recent working dirs + its `$HOME`, for the workdir picker when
    /// it targets this (remote) host. Reply: `RecentDirs`.
    ListRecentDirs { req_id: u64 },
    /// Directory completions on the host's filesystem for `prefix` (an absolute
    /// path already `~`-expanded against the host's home). Reply:
    /// `PathCompletions`.
    CompletePath { req_id: u64, prefix: String },
    /// Whether `path` is a directory on the host's filesystem — the picker's
    /// submit-time validation for a remote launch. Reply: `DirChecked`.
    CheckDir { req_id: u64, path: String },
}

/// Server → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame")]
pub enum ServerFrame {
    /// Handshake reply. The dashboard refuses/warns on a `protocol` mismatch.
    Welcome {
        server_version: String,
        protocol: u32,
        host: String,
    },
    /// Full session list, sent once right after `Subscribe`.
    Snapshot { sessions: Vec<LauncherState> },
    /// One session appeared or changed; carries its full (snippet-capped, small)
    /// new state. Per-session granularity — field-level deltas are a later
    /// optimization the small capped state doesn't yet warrant. Boxed so this
    /// variant doesn't bloat every `ServerFrame` (serde treats `Box<T>` as `T`).
    Delta { state: Box<LauncherState> },
    /// A launcher exited / its pid went dead.
    Removed { launcher_pid: u32 },
    /// Reply to `ListResumable`.
    Resumable {
        req_id: u64,
        candidates: Vec<ResumeCandidate>,
        errors: Vec<String>,
    },
    /// Reply to `KillSession`.
    Killed { req_id: u64, ok: bool },
    /// Reply to `OpenSession`: `session_name` is the pool join key on success;
    /// `error` carries a message instead (pool unavailable / server built
    /// without pty-pool / spawn failed). Exactly one is `Some`.
    Opened {
        req_id: u64,
        session_name: Option<String>,
        error: Option<String>,
    },
    /// Reply to `ListRecentDirs`: the host's recent working dirs (most-recent
    /// first) and its `$HOME` (for the client's `~` display/expansion).
    RecentDirs {
        req_id: u64,
        cwds: Vec<String>,
        home: String,
    },
    /// Reply to `CompletePath`: matching directories on the host fs, as absolute
    /// paths (trailing `/`), sorted. The client collapses them against the host
    /// home for display.
    PathCompletions { req_id: u64, matches: Vec<String> },
    /// Reply to `CheckDir`.
    DirChecked { req_id: u64, exists: bool },
}

/// Serialize one frame as `u32 big-endian length` + JSON. Pure, so the codec is
/// unit-testable without any I/O.
pub fn encode_frame<T: Serialize>(frame: &T) -> std::io::Result<Vec<u8>> {
    let json = serde_json::to_vec(frame)?;
    let len = u32::try_from(json.len()).map_err(|_| invalid("frame too large to encode"))?;
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Write one frame to an async sink and flush.
pub async fn write_frame<W, T>(w: &mut W, frame: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let buf = encode_frame(frame)?;
    w.write_all(&buf).await?;
    w.flush().await
}

/// Read one frame from an async source. `Ok(None)` on a clean EOF at a frame
/// boundary (peer hung up); an error on a torn frame or oversize length.
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(invalid("inbound frame exceeds size cap"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let val = serde_json::from_slice(&body).map_err(|e| invalid(&e.to_string()))?;
    Ok(Some(val))
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn client_frames_round_trip_and_eof() {
        let frames = vec![
            ClientFrame::Hello {
                client_version: "1.2.3".into(),
                protocol: PROTOCOL_VERSION,
            },
            ClientFrame::Subscribe,
            ClientFrame::ListResumable {
                req_id: 7,
                limit: 50,
            },
            ClientFrame::KillSession {
                req_id: 8,
                child_pid: 4242,
            },
        ];
        let mut buf = Vec::new();
        for f in &frames {
            buf.extend(encode_frame(f).unwrap());
        }
        let mut slice = buf.as_slice();
        for expected in &frames {
            let got: ClientFrame = read_frame(&mut slice).await.unwrap().unwrap();
            assert_eq!(&got, expected);
        }
        // Clean EOF at a frame boundary yields None, not an error.
        let end: Option<ClientFrame> = read_frame(&mut slice).await.unwrap();
        assert!(end.is_none());
    }

    #[tokio::test]
    async fn server_frame_round_trips() {
        let frame = ServerFrame::Welcome {
            server_version: "9.9".into(),
            protocol: PROTOCOL_VERSION,
            host: "build-box".into(),
        };
        let buf = encode_frame(&frame).unwrap();
        let mut slice = buf.as_slice();
        let got: ServerFrame = read_frame(&mut slice).await.unwrap().unwrap();
        match got {
            ServerFrame::Welcome { host, protocol, .. } => {
                assert_eq!(host, "build-box");
                assert_eq!(protocol, PROTOCOL_VERSION);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversize_length_is_rejected() {
        // A 4-byte length of 0xFFFFFFFF with no body must error, not hang/alloc.
        let mut slice: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF];
        let r: std::io::Result<Option<ClientFrame>> = read_frame(&mut slice).await;
        assert!(r.is_err());
    }
}
