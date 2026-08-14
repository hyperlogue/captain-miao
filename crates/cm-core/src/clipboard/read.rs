//! The type gate and the clipboard read, behind one function surface.
//!
//! # The gate is structural
//!
//! Nothing here fetches "the clipboard" and then decides what it got. Every read
//! asks the platform's own type system for a specific image flavour, so text is
//! not filtered out — it is never requested, and a clipboard holding only text
//! answers nothing from the call we make. There is no sniffing step to get
//! wrong, and adding a format means adding a [`Format`] entry rather than
//! loosening a check.
//!
//! Two questions, and each platform answers the first without touching pixel
//! data:
//!
//! | | "is there an image?" | "give me the bytes" |
//! | --- | --- | --- |
//! | macOS | `availableType(from: [PNG, TIFF])` | `data(forType:)` |
//! | Wayland | `wl-paste --list-types` | `wl-paste --type image/png` |
//! | X11 | `xclip -t TARGETS -o` | `xclip -t image/png -o` |
//!
//! **What we advertise is what we can *produce*, not what the clipboard holds.**
//! macOS re-encodes through `NSBitmapImageRep`, so a TIFF-only pasteboard still
//! advertises `image/png` (and gets `bmp` for free). Linux has no converter, so a
//! type is advertised only if a tool actually offers it — a clipboard holding
//! only `image/jpeg` answers nothing rather than serving JPEG bytes under a PNG
//! header.
//!
//! # Committing to a header
//!
//! [`open`] returns `Some` only once there is a byte to serve, which is what
//! keeps the error path honest: the caller writes a response header *after* the
//! decision, so "nothing to serve" is always a clean `v1 none` and never a
//! header followed by an empty body.
//!
//! After that, an error is *deliberately* indistinguishable from a cut link:
//! [`Image::copy_framed`] failing means the caller must **not** write the chunk
//! terminator, so the shim exits non-zero and the agent's own fetch chain
//! re-truncates its file. That is why the tool's exit status is checked *after*
//! the copy — a tool that wrote some bytes and then died must not produce a body
//! that claims to be complete.
//!
//! # What can be streamed, and what cannot
//!
//! Linux streams properly: spawn, read the first chunk, then copy — peak memory
//! is 64 KB at any image size. macOS cannot: `data(forType:)` is `NSData`-based,
//! there is no incremental pasteboard reader, and lazy providers exist only on
//! the writing side. So the bytes materialize once in this process, which is a
//! large part of why the server is a **separate short-lived-ish process** rather
//! than a thread in the TUI: a 6K screenshot is ~80 MB of RGBA mid-conversion
//! and allocators rarely give that back.
//!
//! Two consequences of the same asymmetry, both correct:
//!
//! * macOS knows the length up front, so an oversized image is refused at
//!   [`open`] and degrades to a clean `v1 none`.
//! * Linux does not, so the cap trips mid-copy and degrades to a truncated body
//!   the shim rejects.
//!
//! # A note the serve loop has to honour
//!
//! `NSPasteboard` is not documented as thread-safe, and AppKit is main-thread
//! with exceptions. The macOS read here is therefore a plain **synchronous**
//! call, not `spawn_blocking` — which is only safe if the serve loop runs on a
//! **current-thread** tokio runtime, so every task is polled on the process's
//! main thread. Getting a fresh main thread was the whole reason this is a
//! separate process; a multi-thread runtime would throw it away.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::process::Child;

use super::{Format, MAX_CHUNK_BYTES, write_chunk};

#[cfg(not(target_os = "macos"))]
mod tools;

#[cfg(target_os = "macos")]
mod macos;

/// Cap on the **encoded** image we will serve.
///
/// Deliberately not applied to the raw flavour: a 6K screenshot is ~80 MB as
/// uncompressed pasteboard TIFF and 5–15 MB once encoded, so a cap on the raw
/// bytes would reject legitimate screenshots on exactly the hardware this
/// feature is for. This is a bound protecting us from ourselves, not a policy
/// control — the policy is that only images are served at all.
pub const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

/// How long a clipboard tool gets to produce its next bytes. A compositor that
/// died can leave `wl-paste` blocked forever, and this handler holds a child
/// process while it waits.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Image formats the clipboard can serve **right now**, in our own preference
/// order (png first — it is what the agent greps for). Empty means nothing
/// servable, which the caller answers as `v1 none`.
pub async fn available() -> Vec<Format> {
    #[cfg(target_os = "macos")]
    {
        macos::available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        tools::available().await
    }
}

/// Begin serving `fmt`, or `None` when there is nothing to serve.
///
/// `None` covers every reason at once — no image, the wrong format, no tool
/// installed, no display, an oversized image on macOS — because the caller has
/// exactly one thing to say about all of them.
pub async fn open(fmt: Format) -> Option<Image> {
    #[cfg(target_os = "macos")]
    {
        let bytes = macos::read(fmt)?;
        // `open` promises `Some` only once there *is* a byte to serve, and the
        // streaming path gets that from `peek_first`. Here it has to be checked:
        // an empty body frames as a **complete** one (header, no chunk, then the
        // terminator), so the shim would exit 0 and hand the agent a zero-byte
        // image its `||` chain never falls back from.
        if bytes.is_empty() {
            return None;
        }
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            tracing::warn!(
                bytes = bytes.len(),
                "clipboard image over the cap; answering none"
            );
            return None;
        }
        Some(Image::whole(bytes, "pasteboard"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        tools::open(fmt).await
    }
}

/// A read in progress: at least one byte exists, and the rest may still be
/// arriving.
pub struct Image {
    /// The chunk already read, which is what proved there was anything to serve.
    first: Vec<u8>,
    rest: Box<dyn AsyncRead + Send + Unpin>,
    /// The tool whose exit status is the verdict on completeness, when the body
    /// came from one.
    child: Option<Child>,
    /// Named in errors, so a log line says which reader stalled.
    source: &'static str,
}

impl Image {
    /// A body already in hand — macOS, where there is no incremental reader.
    #[cfg(target_os = "macos")]
    fn whole(bytes: Vec<u8>, source: &'static str) -> Self {
        Image {
            first: bytes,
            rest: Box::new(tokio::io::empty()),
            child: None,
            source,
        }
    }

    /// A body still arriving, with its first chunk already peeked.
    #[cfg(not(target_os = "macos"))]
    fn streaming<R>(first: Vec<u8>, rest: R, child: Child, source: &'static str) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        Image {
            first,
            rest: Box::new(rest),
            child: Some(child),
            source,
        }
    }

    /// Frame the body onto `w`, returning the byte count.
    ///
    /// On `Err` the caller must **not** write the chunk terminator: that is the
    /// signal the shim turns into a non-zero exit, and the agent's own fallback
    /// takes it from there.
    pub async fn copy_framed<W>(mut self, w: &mut W) -> io::Result<u64>
    where
        W: AsyncWrite + Unpin,
    {
        write_chunk(w, &self.first).await?;
        let mut total = self.first.len() as u64;
        let mut buf = vec![0u8; MAX_CHUNK_BYTES];
        loop {
            let n = match tokio::time::timeout(IDLE_TIMEOUT, self.rest.read(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("clipboard: {} stalled mid-image", self.source),
                    ));
                }
            };
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_IMAGE_BYTES {
                return Err(io::Error::other(format!(
                    "clipboard: {} exceeded the {MAX_IMAGE_BYTES} byte cap",
                    self.source
                )));
            }
            write_chunk(w, &buf[..n]).await?;
        }
        // The reader's own verdict, and it comes last on purpose: bytes followed
        // by a failure is a truncated image, not a short one.
        if let Some(child) = self.child.as_mut() {
            let status = child.wait().await?;
            if !status.success() {
                return Err(io::Error::other(format!(
                    "clipboard: {} exited with {status}",
                    self.source
                )));
            }
        }
        Ok(total)
    }
}

/// Read the first chunk of a body: `None` when the reader is empty, which is how
/// a tool says "I can't serve that" without us having to trust its exit status
/// first.
///
/// One `read` is enough — it returns 0 only at EOF — and it is the whole reason
/// [`open`] can decide before a header is written.
#[cfg(not(target_os = "macos"))]
async fn peek_first<R>(r: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0u8; MAX_CHUNK_BYTES];
    let n = match tokio::time::timeout(IDLE_TIMEOUT, r.read(&mut buf)).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "clipboard: reader produced nothing in time",
            ));
        }
    };
    if n == 0 {
        return Ok(None);
    }
    buf.truncate(n);
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::read_body;

    /// A body assembled from `first` plus a reader round-trips through the
    /// framing at the sizes that matter, and the caller's terminator is what
    /// completes it.
    #[tokio::test]
    async fn a_streamed_body_frames_and_reads_back() {
        for rest_len in [0usize, 1, MAX_CHUNK_BYTES, MAX_CHUNK_BYTES * 2 + 3] {
            let first = vec![9u8; 128];
            let rest: Vec<u8> = (0..rest_len).map(|i| (i % 251) as u8).collect();
            let img = Image {
                first: first.clone(),
                rest: Box::new(std::io::Cursor::new(rest.clone())),
                child: None,
                source: "test",
            };
            let mut wire = Vec::new();
            let n = img.copy_framed(&mut wire).await.unwrap();
            crate::clipboard::write_end(&mut wire).await.unwrap();
            assert_eq!(n, (first.len() + rest_len) as u64);

            let mut out = Vec::new();
            read_body(&mut wire.as_slice(), &mut out).await.unwrap();
            assert_eq!(out, [first, rest].concat(), "rest_len {rest_len}");
        }
    }

    /// The cap trips mid-copy on the streaming platform, and the body it leaves
    /// behind has no terminator — which is exactly how the shim learns to fail.
    #[tokio::test]
    async fn an_oversized_stream_fails_without_a_terminator() {
        let img = Image {
            first: vec![0u8; 16],
            rest: Box::new(OverCap {
                left: MAX_IMAGE_BYTES,
            }),
            child: None,
            source: "test",
        };
        let mut wire = Vec::new();
        let e = img.copy_framed(&mut wire).await.unwrap_err();
        assert!(e.to_string().contains("cap"), "{e}");
        // The reader on the other end sees a body that never terminates.
        let mut out = Vec::new();
        let e = read_body(&mut wire.as_slice(), &mut out).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// An endless reader, to trip the cap without allocating a real 128 MB.
    struct OverCap {
        left: u64,
    }

    impl AsyncRead for OverCap {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            let n = buf.remaining().min(self.left as usize);
            buf.initialize_unfilled_to(n);
            buf.advance(n);
            self.left -= n as u64;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn an_empty_reader_is_nothing_to_serve() {
        // A tool that prints nothing means "I can't serve that format" — decided
        // before any header is committed to, and without trusting an exit code.
        assert_eq!(peek_first(&mut &b""[..]).await.unwrap(), None);
        assert_eq!(
            peek_first(&mut &b"\x89PNG"[..]).await.unwrap(),
            Some(b"\x89PNG".to_vec())
        );
    }
}
