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
use crate::state::{LauncherState, SessionFlags, SessionKey};

/// The protocol version this build speaks. v2 added `OpenSession`/`Opened`
/// (remote spawn). v3 added the host-filesystem queries the workdir picker needs
/// (`ListRecentDirs`/`CompletePath`/`CheckDir`). **v4 is the last refusing
/// bump**: it replaces the leaked pid encoding with an opaque [`SessionKey`],
/// deletes `$HOME` from the wire (paths are host-canonical — see
/// [`crate::paths`]), and adds `SetSessionFlags`.
pub const PROTOCOL_VERSION: u32 = 4;

/// The oldest protocol this build will talk to. From v4 on, decoding is
/// **forward-tolerant** — unknown frame variants decode to `Unknown` and are
/// ignored, unknown fields are skipped, and new fields must be additive with a
/// `#[serde(default)]` — so a *newer* peer is fine and only a peer *below* this
/// floor is refused. That is what stops every later protocol change from
/// stranding a deployed daemon (§3).
pub const PROTOCOL_MIN: u32 = 4;

/// Whether a peer announcing `protocol` is one we can talk to: at or above the
/// floor, in either direction. Pure so the negotiation rule is pinned by tests
/// and can't drift between the client's and the server's copy of it.
pub fn protocol_compatible(peer: u32) -> bool {
    peer >= PROTOCOL_MIN
}

/// Cap on a single inbound frame, so a peer can't make us allocate unbounded.
const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

/// Client → server.
///
/// Every path in or out of these frames is in the **host-canonical `~` form**
/// ([`crate::paths`]): the server expands what it receives and collapses what it
/// returns, so the client never learns the host's `$HOME` and a path has one
/// spelling per host.
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
    /// Tear the session down. The server re-resolves `key` → the *current* agent
    /// pid from the live state file before signalling, so a stale mirror can
    /// never make it signal a recycled pid.
    KillSession { req_id: u64, key: SessionKey },
    /// Start a launcher inside the host's pty pool. The server creates the pool
    /// session, then replies `Opened` with its name.
    OpenSession { req_id: u64, spec: OpenSpec },
    /// Set the host-owned flags for a session, so every dashboard watching the
    /// host agrees on its pins/mutes. Reply: `FlagsSet`.
    SetSessionFlags {
        req_id: u64,
        key: SessionKey,
        flags: SessionFlags,
    },
    /// The host's recent working dirs, for the workdir picker when it targets
    /// this host. Reply: `RecentDirs`.
    ListRecentDirs { req_id: u64 },
    /// Directory completions on the host's filesystem for `prefix` (in the
    /// host-canonical form). Reply: `PathCompletions`.
    CompletePath { req_id: u64, prefix: String },
    /// Whether `path` is a directory on the host's filesystem — the picker's
    /// submit-time validation. Reply: `DirChecked`.
    CheckDir { req_id: u64, path: String },
    /// A frame this build doesn't know — a *newer* peer's addition. Decoded
    /// rather than erroring, so the connection survives; the handler ignores it.
    #[serde(other)]
    Unknown,
}

/// Server → client. Paths are host-canonical (see [`ClientFrame`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame")]
pub enum ServerFrame {
    /// Handshake reply. Sent even on an unusable version so the peer can report
    /// what it found; the connection then closes if it's below the floor.
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
    /// A session is gone (its launcher exited / its state file went away).
    Removed { key: SessionKey },
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
    /// Reply to `SetSessionFlags`.
    FlagsSet { req_id: u64, ok: bool },
    /// Reply to `ListRecentDirs`: the host's recent working dirs (most-recent
    /// first), host-canonical. Carries no `$HOME` — deliberately: the client is
    /// home-ignorant and displays the wire string verbatim (§3).
    RecentDirs { req_id: u64, cwds: Vec<String> },
    /// Reply to `CompletePath`: matching directories on the host fs (trailing
    /// `/`), host-canonical, sorted.
    PathCompletions { req_id: u64, matches: Vec<String> },
    /// Reply to `CheckDir`.
    DirChecked { req_id: u64, exists: bool },
    /// A frame this build doesn't know — see [`ClientFrame::Unknown`].
    #[serde(other)]
    Unknown,
}

impl ServerFrame {
    /// The request this frame answers, if it is a reply. `None` for the pushed
    /// stream (`Welcome`/`Snapshot`/`Delta`/`Removed`) and for `Unknown`, so
    /// the client's multiplexer routes replies without enumerating variants
    /// twice — and a future reply variant only has to be listed here.
    pub fn req_id(&self) -> Option<u64> {
        match self {
            ServerFrame::Resumable { req_id, .. }
            | ServerFrame::Killed { req_id, .. }
            | ServerFrame::Opened { req_id, .. }
            | ServerFrame::FlagsSet { req_id, .. }
            | ServerFrame::RecentDirs { req_id, .. }
            | ServerFrame::PathCompletions { req_id, .. }
            | ServerFrame::DirChecked { req_id, .. } => Some(*req_id),
            ServerFrame::Welcome { .. }
            | ServerFrame::Snapshot { .. }
            | ServerFrame::Delta { .. }
            | ServerFrame::Removed { .. }
            | ServerFrame::Unknown => None,
        }
    }
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
                key: SessionKey::from_launcher_pid(4242),
            },
            ClientFrame::SetSessionFlags {
                req_id: 9,
                key: SessionKey::from_launcher_pid(4242),
                flags: SessionFlags {
                    pinned: true,
                    muted: false,
                    follow_up: true,
                },
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

    /// The forward-tolerance contract v4 buys, in both directions: a frame
    /// variant this build has never heard of decodes to `Unknown` (so the
    /// connection survives a *newer* peer instead of dying on a parse error),
    /// and an unknown *field* inside a known frame is skipped. Without this,
    /// every later protocol addition would strand deployed daemons — the whole
    /// reason v4 is meant to be the last refusing bump.
    #[tokio::test]
    async fn unknown_frames_and_fields_decode_instead_of_erroring() {
        let future_client = br#"{"frame":"TeleportSession","req_id":1,"where":"mars"}"#;
        let mut buf = Vec::new();
        buf.extend((future_client.len() as u32).to_be_bytes());
        buf.extend_from_slice(future_client);
        let mut slice = buf.as_slice();
        let got: ClientFrame = read_frame(&mut slice).await.unwrap().unwrap();
        assert_eq!(got, ClientFrame::Unknown);

        let future_server = br#"{"frame":"Killed","req_id":7,"ok":true,"latency_us":12}"#;
        let mut buf = Vec::new();
        buf.extend((future_server.len() as u32).to_be_bytes());
        buf.extend_from_slice(future_server);
        let mut slice = buf.as_slice();
        let got: ServerFrame = read_frame(&mut slice).await.unwrap().unwrap();
        // The extra field is skipped, not fatal, and the frame still routes.
        assert!(matches!(got, ServerFrame::Killed { ok: true, .. }));
        assert_eq!(got.req_id(), Some(7));
    }

    #[test]
    fn version_floor_refuses_only_below_itself() {
        assert!(protocol_compatible(PROTOCOL_VERSION));
        assert!(protocol_compatible(PROTOCOL_MIN));
        // A *newer* peer is compatible — that's the point of the floor.
        assert!(protocol_compatible(PROTOCOL_VERSION + 7));
        // Anything predating the tolerant codec is not.
        assert!(!protocol_compatible(PROTOCOL_MIN - 1));
        assert!(!protocol_compatible(0));
    }

    #[test]
    fn only_reply_frames_carry_a_req_id() {
        assert_eq!(
            ServerFrame::DirChecked {
                req_id: 3,
                exists: true
            }
            .req_id(),
            Some(3)
        );
        // Pushed-stream frames aren't replies and must never claim a req_id
        // (that would steal a pending request's oneshot).
        assert_eq!(ServerFrame::Snapshot { sessions: vec![] }.req_id(), None);
        assert_eq!(
            ServerFrame::Removed {
                key: SessionKey::from_launcher_pid(1)
            }
            .req_id(),
            None
        );
        assert_eq!(ServerFrame::Unknown.req_id(), None);
    }

    #[tokio::test]
    async fn oversize_length_is_rejected() {
        // A 4-byte length of 0xFFFFFFFF with no body must error, not hang/alloc.
        let mut slice: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF];
        let r: std::io::Result<Option<ClientFrame>> = read_frame(&mut slice).await;
        assert!(r.is_err());
    }
}
