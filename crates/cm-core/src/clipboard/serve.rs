//! `clipboard serve` — the one process that binds the clipboard socket.
//!
//! The dashboard does **not** bind, accept or serve anything. It spawns this as a
//! long-lived child and holds the handle, which is the same pattern as the `ssh
//! -N -L` tunnel child: a `kill_on_drop` process whose whole job is holding a
//! resource open for as long as the dashboard wants it. What the dashboard gains
//! is a `Child`; what it avoids is a listener, an accept loop, and any bound on
//! concurrent work inside the process that holds every host's ssh master.
//!
//! One process and one socket, because the clipboard is a property of *this
//! machine*: neither N processes reading the same pasteboard nor N sockets
//! carrying identical requests buys anything. Every enabled host forwards its own
//! remote path onto this one socket, and the hosts never meet because each
//! reaches it through its own ssh connection. So this takes **no arguments** —
//! there is nothing to reconfigure, and therefore no respawn-with-new-state path.
//!
//! # Three invariants, each carrying its own reason
//!
//! **Connections are served one at a time, and nothing here calls
//! `tokio::spawn`.** Two reasons, and it is worth being precise about which is
//! which, because they pull in different directions.
//!
//! The macOS pasteboard read is a synchronous AppKit call that expects the
//! process's main thread, and what guarantees that is [`run`]'s caller handing us
//! a **current-thread** runtime — getting a fresh main thread is most of why this
//! is a separate process at all. Note that serving inline is *not* what upholds
//! it: tasks on a current-thread runtime are all polled on the thread that drives
//! it, so a `tokio::spawn` here would stay on the main thread too. The thread rule
//! constrains the runtime, not the shape of this loop.
//!
//! What serving inline actually buys is a **bound on materialized images**. A
//! pasteboard read has no incremental form (see [`read`]), so each concurrent
//! paste would hold its own copy — up to `MAX_IMAGE_BYTES` each, plus the decode
//! peak — and nothing above would cap the count. One at a time makes the
//! high-water one image, whoever is asking.
//!
//! The cost is head-of-line blocking, which is why the *request line* has a
//! deadline of its own ([`REQUEST_TIMEOUT`]) well below [`CONNECTION_DEADLINE`]:
//! a peer that connects and never speaks is the one case that would hold the
//! single handler for no reason at all. Two hosts genuinely pasting at the same
//! instant still serialize, which is invisible at the scale of one keystroke.
//!
//! **The socket path is unlinked before binding, and never on the way out.** A
//! graceful-cleanup path would be dead code: the dashboard's `kill_on_drop` sends
//! **SIGKILL**, so even a clean quit gives this process no chance to unlink. So a
//! stale file is expected on *every* start, and removing it at bind time is the
//! one mechanism that covers orderly quit, crash and SIGKILL alike. That is safe
//! because a second dashboard on this machine is already impossible (`run.rs`
//! holds an flock singleton), so there is never a live peer to steal from — and
//! [`already_serving`] is the belt-and-braces for that reasoning being wrong.
//!
//! Note the symmetry with the transport: a stale unix socket blocks a bind at
//! **both** ends of this feature. Here the child unlinks before binding; on a
//! remote the dashboard `rm -f`s the path before ssh forwards it, because there we
//! cannot run code in the binding process at all.
//!
//! **The parent's death is learned from the kernel, not from the parent.** The
//! dashboard keeps the write end of this process's stdin; when it dies *for any
//! reason*, including SIGKILL, the kernel closes its descriptors and the read
//! below returns 0. `PR_SET_PDEATHSIG` could only ever be a Linux-only addition
//! on top — this has to work on macOS, which is the platform the feature is for.

use std::future::Future;
use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{MAX_REQUEST_BYTES, Request, Response, paths, read, read_ascii_line, write_end};

/// How long one connection gets, start to finish.
///
/// Not a security control — it bounds how long a single paste can hold a child
/// clipboard process and a task, against a peer that connects and then stalls
/// (or is stopped). Far beyond any legitimate paste: 128 MB over a slow link is
/// minutes, and this is five.
const CONNECTION_DEADLINE: Duration = Duration::from_secs(300);

/// How long a peer gets to send its request *line*, which is the one part of a
/// connection that can't legitimately be slow: the shim writes 13 bytes and
/// flushes before it waits for anything.
///
/// It needs a bound of its own precisely because connections are served inline
/// (see the module doc): under [`CONNECTION_DEADLINE`] alone, one shim whose ssh
/// link froze after `connect` would hold the only handler for five minutes, and
/// every other host's paste would meanwhile trip the shim's 30s header timeout
/// and silently degrade to "no image".
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause after a failed `accept`, so a persistent error (EMFILE) costs a slow
/// loop rather than a spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Bind and serve until the parent goes away.
///
/// Idempotent by construction: every start either wins the socket or finds a live
/// server and leaves, so the dashboard never has to track whether it already
/// spawned one.
pub async fn run() -> Result<()> {
    let path = paths::local_socket_path();
    // Belt-and-braces (see the module doc): if something answers, it is a live
    // server and this process is a duplicate. Exit 0 — the parent's supervision
    // reads a non-zero exit as something to respawn.
    if already_serving(&path).await {
        tracing::info!(path = %path.display(), "a clipboard server is already live; leaving it alone");
        return Ok(());
    }
    let listener = bind(&path)
        .with_context(|| format!("could not bind the clipboard socket at {}", path.display()))?;
    tracing::info!(path = %path.display(), "clipboard server listening");
    serve_loop(listener, parent_gone()).await;
    tracing::info!("parent went away; clipboard server exiting");
    Ok(())
}

/// Whether a **live** server holds `path`. A stale socket file refuses the
/// connect; only a process in an accept loop answers.
async fn already_serving(path: &Path) -> bool {
    UnixStream::connect(path).await.is_ok()
}

/// Unlink, bind, and tighten to owner-only.
///
/// The 0600 lands after the bind, matching `launcher::bind_hook_socket`. The gap
/// is closed by the parent dir rather than by ordering: `create_dir_all_private`
/// makes it 0700, so nothing else can reach the path in between.
fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        crate::state::create_dir_all_private(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    let _ = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    listener.set_nonblocking(true)?;
    Ok(UnixListener::from_std(listener)?)
}

/// Accept and serve, one connection at a time, until `shutdown` resolves.
///
/// `shutdown` is a single long-lived future polled across iterations rather than
/// one recreated per loop, so a partial read of the parent pipe is never
/// restarted.
async fn serve_loop<S>(listener: UnixListener, shutdown: S)
where
    S: Future<Output = ()>,
{
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let stream = tokio::select! {
            // The parent going away wins a tie: there is nobody left to serve.
            biased;
            () = &mut shutdown => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "clipboard accept failed");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
        };
        match tokio::time::timeout(CONNECTION_DEADLINE, handle(stream)).await {
            Ok(Ok(())) => {}
            // Both of these are ordinary: a shim that hung up, a link that
            // dropped, a read that failed past the response header. The last one
            // is *how* a truncated image is reported — the terminator is never
            // written, so the shim exits non-zero and the agent falls back.
            Ok(Err(e)) => tracing::debug!(error = %e, "clipboard request ended early"),
            Err(_) => tracing::warn!("clipboard request hit the connection deadline"),
        }
    }
}

/// One request, one response, then the connection closes.
async fn handle(stream: UnixStream) -> io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let read = tokio::time::timeout(REQUEST_TIMEOUT, read_ascii_line(&mut r, MAX_REQUEST_BYTES));
    let line = match read.await {
        Ok(line) => line?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "clipboard: a peer connected without sending a request",
            ));
        }
    };
    let Some(line) = line else {
        // Connected and hung up without asking anything.
        return Ok(());
    };
    match Request::parse(&line) {
        // Unknown verb, unknown version, unknown format, junk. One answer for all
        // of them, and the input is never echoed back.
        None => answer(&mut w, &Response::None).await,
        Some(Request::Targets) => {
            let formats = read::available().await;
            let response = if formats.is_empty() {
                Response::None
            } else {
                Response::Types(formats)
            };
            answer(&mut w, &response).await
        }
        Some(Request::Image(fmt)) => match read::open(fmt).await {
            None => answer(&mut w, &Response::None).await,
            Some(image) => {
                // Committed to a header only now that a byte exists.
                w.write_all(Response::Image(fmt).to_wire().as_bytes())
                    .await?;
                let n = image.copy_framed(&mut w).await?;
                // Reached only on a complete body: any `?` above leaves the
                // terminator unwritten, which is the signal for truncation.
                write_end(&mut w).await?;
                tracing::debug!(bytes = n, format = fmt.token(), "served a clipboard image");
                Ok(())
            }
        },
    }
}

async fn answer<W>(w: &mut W, response: &Response) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    w.write_all(response.to_wire().as_bytes()).await?;
    w.flush().await
}

/// Resolves when the parent's end of our stdin pipe closes — see the module doc.
///
/// A write is ignored rather than treated as a signal: nobody writes, and reading
/// on is what keeps this correct if anything ever does.
async fn parent_gone() {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 32];
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::Format;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cm-clipboard-serve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Ask over a real socket and read back one line.
    async fn ask(path: &Path, request: &str) -> String {
        let stream = UnixStream::connect(path).await.unwrap();
        let (r, mut w) = stream.into_split();
        w.write_all(request.as_bytes()).await.unwrap();
        w.flush().await.unwrap();
        let mut r = BufReader::new(r);
        read_ascii_line(&mut r, 256)
            .await
            .unwrap()
            .unwrap_or_default()
    }

    /// End to end over a bound socket: the protocol, the permissions, and the
    /// answer to every malformed thing a caller can send.
    #[tokio::test]
    async fn serves_the_protocol_over_a_real_socket() {
        let dir = scratch("protocol");
        let path = dir.join("clipboard.sock");
        let listener = bind(&path).unwrap();

        // Owner-only, per the policy: on a shared machine this is your uid's
        // clipboard and nobody else's.
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600, "socket must be owner-only");
        assert!(already_serving(&path).await, "nothing accepted the connect");

        let task = tokio::spawn(serve_loop(listener, std::future::pending()));

        // Every unrecognized request gets exactly `v1 none`, with nothing of the
        // input reflected back.
        for bogus in [
            "v2 targets\n",
            "v1 image gif\n",
            "v1 image ../etc/passwd\n",
            "v1 fetch png\n",
            "v1 image png extra\n",
            "garbage\n",
            "\n",
        ] {
            let answer = ask(&path, bogus).await;
            assert_eq!(answer, "v1 none", "request {bogus:?} answered {answer:?}");
        }

        // A well-formed request gets a well-formed answer. Which one depends on
        // whether this machine has a clipboard with an image on it — in CI it
        // does not — so the assertion is that it parses and is one of the two
        // legal shapes.
        let answer = ask(&path, &Request::Targets.to_wire()).await;
        let parsed = Response::parse(&answer).unwrap_or_else(|| panic!("unparsable: {answer:?}"));
        assert!(
            matches!(parsed, Response::None | Response::Types(_)),
            "targets answered {parsed:?}"
        );
        let answer = ask(&path, &Request::Image(Format::Png).to_wire()).await;
        let parsed = Response::parse(&answer).unwrap_or_else(|| panic!("unparsable: {answer:?}"));
        assert!(
            matches!(parsed, Response::None | Response::Image(Format::Png)),
            "image answered {parsed:?}"
        );

        // Connecting and hanging up without asking is not an error, and the
        // server stays up for the next caller.
        drop(UnixStream::connect(&path).await.unwrap());
        assert_eq!(ask(&path, "v1 nonsense\n").await, "v1 none");

        task.abort();
    }

    /// The parent's pipe closing ends the loop — the mechanism that keeps an
    /// orphan from holding the socket after a SIGKILL'd dashboard.
    #[tokio::test]
    async fn the_loop_ends_when_the_parent_goes_away() {
        let dir = scratch("shutdown");
        let path = dir.join("clipboard.sock");
        let listener = bind(&path).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(serve_loop(listener, async move {
            let _ = rx.await;
        }));
        assert!(already_serving(&path).await);
        drop(tx);
        // Ends on its own, without being cancelled.
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("serve loop did not exit when the parent went away")
            .unwrap();
    }

    /// Binding unlinks first, because `kill_on_drop`'s SIGKILL guarantees the
    /// previous run left its socket file behind.
    #[tokio::test]
    async fn a_stale_socket_file_does_not_block_a_bind() {
        let dir = scratch("stale");
        let path = dir.join("clipboard.sock");
        // A leftover file, as an orphaned run would leave.
        std::fs::write(&path, b"stale").unwrap();
        let listener = bind(&path).unwrap();
        let task = tokio::spawn(serve_loop(listener, std::future::pending()));
        assert!(
            already_serving(&path).await,
            "bind did not replace the file"
        );
        task.abort();

        // And a stale *socket* — the real case — refuses a connect rather than
        // answering, which is what `already_serving` distinguishes.
        let orphan = dir.join("orphan.sock");
        let held = bind(&orphan).unwrap();
        drop(held);
        assert!(orphan.exists(), "SIGKILL leaves the path behind");
        assert!(
            !already_serving(&orphan).await,
            "a dead socket must not answer"
        );
    }
}
