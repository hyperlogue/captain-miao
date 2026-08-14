//! The clipboard bridge: paste a screenshot from the machine running the
//! dashboard into an agent running in a **pooled** session, local pool or remote
//! host.
//!
//! This module is the bridge's pure core — the line protocol both ends speak,
//! the argv matching that decides whether a shimmed `xclip`/`wl-paste` call is
//! ours to answer, and the lookup of the real tool to hand the call to when it
//! isn't. The platform clipboard read, the server, and the shim's own main sit
//! on top of it.
//!
//! # The shape
//!
//! A pooled agent has no clipboard: its `Ctrl+V` shells out to `xclip` or
//! `wl-paste`, which on a remote either aren't installed or answer for the wrong
//! machine. So the launcher prepends a dir of **symlinks back to `miao-server`**
//! to the agent's `PATH` under those two names, and the shadow asks the
//! dashboard's machine over a unix socket that `ssh -R` forwarded into place
//! ([`paths`]).
//!
//! # Invariants, and why each is here
//!
//! **Every failure answers [`Response::None`]; there is no error frame.** The
//! agent invokes the tools with `2>/dev/null` and the fetch redirected to a
//! file, so the shim has no channel to the user at all: a refusal and an empty
//! clipboard are the same event as far as anyone can see. Collapsing them
//! removes states without losing information — and the request is never echoed
//! back, so a malformed one teaches its sender nothing.
//!
//! **The [`Format`] allowlist is also the injection gate.** A format token
//! arrives off a socket that anything on the remote can open. It is *compared*
//! against a two-entry table and then discarded; the platform read is driven by
//! the resulting enum, so no byte from the wire is ever interpolated into an
//! argv.
//!
//! **We only ever name image types.** The type gate is structural rather than a
//! filter: nothing here can ask for "the clipboard" and then decide what it got,
//! so text is not rejected — it is never requested. `text/plain` and every
//! unrecognized call [delegate](ShimCall::Delegate) to the remote's own tools,
//! which is also what keeps ordinary remote text paste working.
//!
//! **`0\n` is the only proof that a body was complete.** EOF alone would make
//! truncation indistinguishable from success: kill the dashboard mid-stream and
//! the shim would see bytes, then EOF, exit 0, and hand the agent a truncated
//! PNG that fails to decode far from its cause. With a terminator the shim knows
//! it was cut off and exits non-zero — and the agent's own fetch is a single
//! `||` chain whose every link re-truncates the output file, so a non-zero exit
//! degrades to its own "no image in clipboard". The framing is not defending a
//! hypothetical; it is making a recovery the agent already implements reachable.
//!
//! **Delegation is always safe, so the argv matching is deliberately strict.**
//! Anything we don't recognize exactly runs the real tool, which is the
//! behaviour of not being installed at all. That is why no effort goes into
//! liberal flag parsing: being liberal has no upside and a loose `-s` prefix
//! match would read `-s primary` as the clipboard selection.

pub mod paths;
pub mod read;
pub mod serve;
pub mod shim;

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The only protocol version this build speaks.
///
/// A peer announcing anything else gets [`Response::None`], so a skewed shim
/// degrades to native behaviour rather than misparsing. Skew is reachable: the
/// provisioning ladder accepts a protocol-compatible server on the host that
/// isn't our exact build.
const VERSION: &str = "v1";

// -- Formats --

/// An image format the bridge will serve.
///
/// Two entries, because these are the only image types the agent is ever
/// observed to ask for — `png` from its availability probe, `bmp` from the next
/// link of its fetch chain after a png fetch fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Bmp,
}

impl Format {
    /// The allowlist, and therefore the injection gate. Adding a format is
    /// adding an entry here, never loosening a check.
    pub const ALL: [Format; 2] = [Format::Png, Format::Bmp];

    /// The bare token the protocol carries (`png`).
    pub fn token(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Bmp => "bmp",
        }
    }

    /// The MIME name the tools speak (`image/png`).
    pub fn mime(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Bmp => "image/bmp",
        }
    }

    pub fn from_token(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.token() == s)
    }

    pub fn from_mime(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.mime() == s)
    }
}

// -- The line protocol --

/// Cap on a request line, excluding its newline. The longest we generate is 13
/// bytes; the cap exists so a client that connects and streams without ever
/// sending a newline can't make us allocate.
pub const MAX_REQUEST_BYTES: usize = 64;

/// Cap on a response line, excluding its newline. Larger than the request cap
/// because `types` grows with the allowlist.
pub const MAX_RESPONSE_BYTES: usize = 256;

/// Ceiling on one body chunk. Also the serving side's read buffer, so the Linux
/// streaming path writes one chunk per read with no repacking.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// Shim → server. One per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// "What image types can you serve?" — the agent's availability probe.
    Targets,
    /// "Give me the bytes." The body follows the response header.
    Image(Format),
}

impl Request {
    /// The wire form, newline included.
    pub fn to_wire(self) -> String {
        match self {
            Request::Targets => format!("{VERSION} targets\n"),
            Request::Image(f) => format!("{VERSION} image {}\n", f.token()),
        }
    }

    /// Parse one request line (newline already stripped). `None` for *anything*
    /// unrecognized — wrong version, unknown verb, unknown format, trailing
    /// junk — and the caller answers [`Response::None`] without echoing a byte
    /// of it.
    pub fn parse(line: &str) -> Option<Self> {
        let mut it = line.split_whitespace();
        if it.next()? != VERSION {
            return None;
        }
        let out = match it.next()? {
            "targets" => Request::Targets,
            "image" => Request::Image(Format::from_token(it.next()?)?),
            _ => return None,
        };
        // Trailing junk is a different request than the one we'd be answering.
        it.next().is_none().then_some(out)
    }
}

/// Server → shim. One per connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Nothing servable, for any reason at all. The shim delegates.
    None,
    /// Answer to [`Request::Targets`]: the image types we can produce right now.
    Types(Vec<Format>),
    /// Header for [`Request::Image`]; the body follows as chunks
    /// ([`write_chunk`] / [`read_body`]).
    Image(Format),
}

impl Response {
    /// The wire form, newline included. `types` carries MIME names because the
    /// shim prints them straight through to a caller that greps for
    /// `image/png`; `image` carries the bare token, mirroring the request.
    pub fn to_wire(&self) -> String {
        match self {
            Response::None => format!("{VERSION} none\n"),
            Response::Types(fs) => {
                let mut s = format!("{VERSION} types");
                for f in fs {
                    s.push(' ');
                    s.push_str(f.mime());
                }
                s.push('\n');
                s
            }
            Response::Image(f) => format!("{VERSION} image {}\n", f.token()),
        }
    }

    /// Parse one response line. `None` for anything unrecognized, which the shim
    /// treats exactly as [`Response::None`].
    ///
    /// A type in `types` that this build can't fetch is **dropped** rather than
    /// failing the parse: advertising it would have the caller grep it, match,
    /// ask for it, and get `none` — a dead end instead of a fallback.
    pub fn parse(line: &str) -> Option<Self> {
        let mut it = line.split_whitespace();
        if it.next()? != VERSION {
            return None;
        }
        match it.next()? {
            "none" => it.next().is_none().then_some(Response::None),
            "types" => Some(Response::Types(it.filter_map(Format::from_mime).collect())),
            "image" => {
                let f = Format::from_token(it.next()?)?;
                it.next().is_none().then_some(Response::Image(f))
            }
            _ => None,
        }
    }
}

/// Read one line, bounded, from a buffered reader. `max` excludes the newline.
///
/// `Ok(None)` is a clean EOF *before any byte* — the peer hung up without
/// speaking. Bytes followed by EOF is an error, not a line: a request or header
/// only means what it says once terminated.
pub async fn read_ascii_line<R>(r: &mut R, max: usize) -> io::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut out: Vec<u8> = Vec::new();
    loop {
        // Scoped so the `fill_buf` borrow ends before `consume`.
        let (consumed, complete) = {
            let available = r.fill_buf().await?;
            if available.is_empty() {
                return if out.is_empty() {
                    Ok(None)
                } else {
                    Err(err(
                        io::ErrorKind::UnexpectedEof,
                        "line ended without a newline",
                    ))
                };
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    out.extend_from_slice(&available[..i]);
                    (i + 1, true)
                }
                None => {
                    out.extend_from_slice(available);
                    (available.len(), false)
                }
            }
        };
        r.consume(consumed);
        if out.len() > max {
            return Err(err(io::ErrorKind::InvalidData, "line too long"));
        }
        if complete {
            return String::from_utf8(out)
                .map(Some)
                .map_err(|_| err(io::ErrorKind::InvalidData, "line was not utf-8"));
        }
    }
}

/// Frame `buf` as body chunks. A buffer over [`MAX_CHUNK_BYTES`] is split, so no
/// caller can produce an out-of-range header; an empty one writes nothing, since
/// a zero-length chunk is the terminator and must never fall out of a short
/// read.
pub async fn write_chunk<W>(w: &mut W, buf: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    for part in buf.chunks(MAX_CHUNK_BYTES) {
        w.write_all(format!("{}\n", part.len()).as_bytes()).await?;
        w.write_all(part).await?;
    }
    Ok(())
}

/// Write the terminator that proves the body was complete, and flush.
pub async fn write_end<W>(w: &mut W) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    w.write_all(b"0\n").await?;
    w.flush().await
}

/// A chunk header: the byte count, or `0` for the terminator. `None` for
/// anything out of range or unparseable.
pub fn parse_chunk_header(line: &str) -> Option<usize> {
    let n: usize = line.trim().parse().ok()?;
    (n <= MAX_CHUNK_BYTES).then_some(n)
}

/// Copy a chunked body from `r` to `out`, returning the byte count.
///
/// The whole point is the error path: a stream that ends without the terminator
/// is [`io::ErrorKind::UnexpectedEof`], which is what lets the shim exit
/// non-zero and hand the recovery back to the agent. Bytes already written stay
/// written — the agent's next fallback re-truncates the file.
pub async fn read_body<R, W>(r: &mut R, out: &mut W) -> io::Result<u64>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Enough for `65536` and any plausible junk; a header line longer than this
    /// is not one we would have written.
    const HEADER_MAX: usize = 16;

    let mut total: u64 = 0;
    let mut buf = vec![0u8; MAX_CHUNK_BYTES];
    loop {
        let line = read_ascii_line(r, HEADER_MAX).await?.ok_or_else(|| {
            err(
                io::ErrorKind::UnexpectedEof,
                "body ended before its terminator",
            )
        })?;
        let len = parse_chunk_header(&line)
            .ok_or_else(|| err(io::ErrorKind::InvalidData, "bad chunk header"))?;
        if len == 0 {
            out.flush().await?;
            return Ok(total);
        }
        // A short read here is the truncation this framing exists to catch.
        r.read_exact(&mut buf[..len]).await?;
        out.write_all(&buf[..len]).await?;
        total += len as u64;
    }
}

fn err(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, format!("clipboard: {msg}"))
}

// -- The shim's argv --

/// The names the shim farm mints, as symlinks to the binary that already ran.
///
/// The third is deliberately **not** `paste`: that is a POSIX utility, and
/// putting ours first on the agent's `PATH` would shadow it for the agent and
/// anything it shells out to. Shadowing `xclip`/`wl-paste` *is* the mechanism;
/// shadowing `paste` would be an accident.
pub const SHIM_NAMES: [&str; 3] = ["xclip", "wl-paste", PASTE_HELPER_NAME];

/// The helper that prints a path to a written-out clipboard image, for an agent
/// that reads the clipboard in-process and can't be shimmed at all.
pub const PASTE_HELPER_NAME: &str = "clipboard-paste";

/// A tool whose name we shadow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimTool {
    Xclip,
    WlPaste,
}

impl ShimTool {
    /// Which tool we were invoked as, from `argv[0]`. `None` means we were
    /// invoked under our own name and this is an ordinary subcommand run.
    pub fn from_argv0(argv0: &str) -> Option<Self> {
        match Path::new(argv0).file_name()?.to_str()? {
            "xclip" => Some(ShimTool::Xclip),
            "wl-paste" => Some(ShimTool::WlPaste),
            _ => None,
        }
    }

    /// The name to look up on `PATH` when delegating.
    pub fn binary(self) -> &'static str {
        match self {
            ShimTool::Xclip => "xclip",
            ShimTool::WlPaste => "wl-paste",
        }
    }
}

/// What a shimmed invocation turns out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShimCall {
    /// An availability probe: answer with the servable types, one per line.
    Targets,
    /// A fetch: stream the bytes to stdout.
    Fetch(Format),
    /// Not ours. Exec the real tool, so we behave exactly as if we weren't
    /// installed.
    Delegate,
}

/// Classify an invocation of a shimmed tool. `args` excludes `argv[0]`.
///
/// Everything not matched exactly is [`ShimCall::Delegate`] — see the module
/// doc on why strictness is the safe direction here.
pub fn classify(tool: ShimTool, args: &[&str]) -> ShimCall {
    match tool {
        ShimTool::Xclip => classify_xclip(args),
        ShimTool::WlPaste => classify_wl_paste(args),
    }
}

/// `xclip -selection clipboard -t <target> -o`.
///
/// All three parts are required. Without `-o` xclip is *copying*, which we never
/// serve; without an explicit clipboard selection it reads `PRIMARY`, a
/// different selection than the one we hold; without `-t` its target defaults to
/// text.
fn classify_xclip(args: &[&str]) -> ShimCall {
    let mut out_mode = false;
    let mut clipboard = false;
    let mut target: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let Some((name, inline)) = split_flag(args[i]) else {
            // A positional is a filename, i.e. input mode.
            return ShimCall::Delegate;
        };
        match name {
            "o" | "out" => {
                out_mode = true;
                i += 1;
            }
            "sel" | "selection" => {
                let Some(v) = flag_value(args, &mut i, inline) else {
                    return ShimCall::Delegate;
                };
                // xclip prefix-matches the value, so `-selection c` is the
                // clipboard. `primary`/`secondary` are not.
                if v.is_empty() || !"clipboard".starts_with(v) {
                    return ShimCall::Delegate;
                }
                clipboard = true;
            }
            "t" | "target" => {
                let Some(v) = flag_value(args, &mut i, inline) else {
                    return ShimCall::Delegate;
                };
                target = Some(v);
            }
            _ => return ShimCall::Delegate,
        }
    }
    if !out_mode || !clipboard {
        return ShimCall::Delegate;
    }
    match target {
        Some("TARGETS") => ShimCall::Targets,
        Some(t) => Format::from_mime(t).map_or(ShimCall::Delegate, ShimCall::Fetch),
        None => ShimCall::Delegate,
    }
}

/// `wl-paste -l` / `wl-paste --type <mime>`, and **bare `wl-paste` is text** —
/// the agent's text fallback takes no arguments at all, so it must delegate.
fn classify_wl_paste(args: &[&str]) -> ShimCall {
    let mut list = false;
    let mut mime: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let Some((name, inline)) = split_flag(args[i]) else {
            // A positional means `--watch <cmd>`.
            return ShimCall::Delegate;
        };
        match name {
            "l" | "list-types" => {
                list = true;
                i += 1;
            }
            // Harmless for image data either way: current wl-clipboard forces
            // `no_newline` for non-text MIME types regardless.
            "n" | "no-newline" => i += 1,
            "t" | "type" => {
                let Some(v) = flag_value(args, &mut i, inline) else {
                    return ShimCall::Delegate;
                };
                mime = Some(v);
            }
            // `--primary`, `--watch`, `--seat`, anything new.
            _ => return ShimCall::Delegate,
        }
    }
    if list {
        return ShimCall::Targets;
    }
    match mime {
        Some(m) => Format::from_mime(m).map_or(ShimCall::Delegate, ShimCall::Fetch),
        None => ShimCall::Delegate,
    }
}

/// Split a flag into its name and any `=`-attached value, stripping one or two
/// leading dashes. `None` for a positional argument.
fn split_flag(arg: &str) -> Option<(&str, Option<&str>)> {
    let body = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'))?;
    if body.is_empty() {
        return None;
    }
    Some(match body.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (body, None),
    })
}

/// The value of the flag at `*i`: attached with `=`, else the next slot.
/// Advances `*i` past what it consumed.
fn flag_value<'a>(args: &[&'a str], i: &mut usize, inline: Option<&'a str>) -> Option<&'a str> {
    match inline {
        Some(v) => {
            *i += 1;
            Some(v)
        }
        None => {
            *i += 2;
            args.get(*i - 1).copied()
        }
    }
}

/// The availability answer as both tools print it: one MIME name per line.
/// `xclip -t TARGETS -o` and `wl-paste --list-types` agree on this, which is why
/// one renderer serves both.
pub fn render_targets(formats: &[Format]) -> String {
    formats.iter().fold(String::new(), |mut s, f| {
        s.push_str(f.mime());
        s.push('\n');
        s
    })
}

// -- Delegation --

/// A file's identity on disk: `(st_dev, st_ino)`.
pub type FileId = (u64, u64);

/// The real `name` on `path_var`, skipping any candidate that **is** us.
///
/// Identity, not string filtering. Filtering our own dir out of `PATH` misses an
/// aliased or duplicated spelling of it, and what it misses is the shim exec'ing
/// *itself* in a tight loop. One `stat` per candidate closes that outright:
/// every farm entry is a symlink to the running binary, so following it lands on
/// the same inode as [`self_id`].
///
/// `me` of `None` disables the guard rather than filtering everything out, which
/// is the right degradation — `current_exe` not resolving is not a reason to
/// stop delegating. An empty `PATH` entry (POSIX: the cwd) is skipped, since
/// exec'ing `./xclip` out of a directory the agent happens to sit in is not
/// something a `PATH` lookup should do.
pub fn resolve_delegate(name: &str, path_var: &OsStr, me: Option<FileId>) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|c| match executable_id(c) {
            Some(id) => me != Some(id),
            None => false,
        })
}

/// [`resolve_delegate`] against the live environment.
pub fn resolve_delegate_from_env(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    resolve_delegate(name, &path, self_id())
}

/// This process's own executable identity, for the self-guard above.
pub fn self_id() -> Option<FileId> {
    executable_id(&std::env::current_exe().ok()?)
}

/// `(dev, ino)` of `path` if it is an executable regular file. Follows symlinks,
/// which is the point: a farm entry resolves to the binary it points at.
fn executable_id(path: &Path) -> Option<FileId> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() || m.permissions().mode() & 0o111 == 0 {
        return None;
    }
    Some((m.dev(), m.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Formats and the protocol --

    #[test]
    fn every_format_round_trips_through_both_spellings() {
        for f in Format::ALL {
            assert_eq!(Format::from_token(f.token()), Some(f));
            assert_eq!(Format::from_mime(f.mime()), Some(f));
            assert_eq!(f.mime(), format!("image/{}", f.token()));
        }
        assert_eq!(Format::from_token("gif"), None);
        assert_eq!(Format::from_mime("image/gif"), None);
        assert_eq!(Format::from_mime("text/plain"), None);
        // The gate is the allowlist, so a token that merely *looks* like one of
        // ours must not slip through.
        assert_eq!(Format::from_token("png "), None);
        assert_eq!(Format::from_token("PNG"), None);
        assert_eq!(Format::from_token("../png"), None);
    }

    #[test]
    fn requests_round_trip() {
        let mut all = vec![Request::Targets];
        all.extend(Format::ALL.map(Request::Image));
        for r in all {
            let wire = r.to_wire();
            assert!(wire.ends_with('\n'));
            assert_eq!(Request::parse(wire.trim_end()), Some(r), "{wire:?}");
            assert!(
                wire.len() - 1 <= MAX_REQUEST_BYTES,
                "{wire:?} exceeds the request cap"
            );
        }
    }

    #[test]
    fn responses_round_trip() {
        let mut all = vec![Response::None, Response::Types(Format::ALL.to_vec())];
        all.extend(Format::ALL.map(Response::Image));
        for r in &all {
            let wire = r.to_wire();
            assert!(wire.ends_with('\n'));
            assert_eq!(
                Response::parse(wire.trim_end()),
                Some(r.clone()),
                "{wire:?}"
            );
            assert!(
                wire.len() - 1 <= MAX_RESPONSE_BYTES,
                "{wire:?} exceeds the response cap"
            );
        }
        assert_eq!(
            Response::Types(vec![Format::Png]).to_wire(),
            "v1 types image/png\n"
        );
        assert_eq!(Response::None.to_wire(), "v1 none\n");
        assert_eq!(Response::Image(Format::Bmp).to_wire(), "v1 image bmp\n");
    }

    #[test]
    fn an_unrecognized_request_parses_to_nothing_at_all() {
        // Every one of these is answered `v1 none`, with the input never echoed.
        for line in [
            "",
            " ",
            "v1",
            "v1 ",
            "v1 image",
            "v1 image gif",
            "v1 image png png",
            "v1 image image/png", // the mime spelling is the response's, not the request's
            "v1 targets extra",
            "v1 TARGETS",
            "v1 fetch png",
            "v2 targets", // a skewed peer degrades, it does not misparse
            "v10 targets",
            "targets",
            "image png",
        ] {
            assert_eq!(Request::parse(line), None, "{line:?}");
        }
    }

    #[test]
    fn whitespace_around_the_tokens_is_tolerated() {
        // `\r` is whitespace, so a peer terminating with CRLF is understood
        // rather than silently degraded — and the split is on runs, so odd
        // spacing costs nothing to accept.
        for line in ["v1 targets\r", "  v1   targets  ", "v1\ttargets"] {
            assert_eq!(Request::parse(line), Some(Request::Targets), "{line:?}");
        }
        for line in ["v1 image png\r", "v1\timage\tpng"] {
            assert_eq!(
                Request::parse(line),
                Some(Request::Image(Format::Png)),
                "{line:?}"
            );
        }
    }

    #[test]
    fn an_unfetchable_advertised_type_is_dropped_not_fatal() {
        // A newer server offering gif: the shim must not print a type it would
        // then be unable to fetch, or the caller greps it, asks, and dead-ends.
        assert_eq!(
            Response::parse("v1 types image/png image/gif"),
            Some(Response::Types(vec![Format::Png]))
        );
        assert_eq!(
            Response::parse("v1 types image/gif"),
            Some(Response::Types(vec![]))
        );
        assert_eq!(Response::parse("v2 types image/png"), None);
        assert_eq!(Response::parse("v1 none extra"), None);
    }

    // -- Chunk framing --

    /// Framing round trip at the sizes that matter: empty, one byte, exactly one
    /// chunk, one over, and several chunks.
    #[tokio::test]
    async fn a_framed_body_round_trips_at_every_boundary() {
        for len in [
            0,
            1,
            1024,
            MAX_CHUNK_BYTES - 1,
            MAX_CHUNK_BYTES,
            MAX_CHUNK_BYTES + 1,
            MAX_CHUNK_BYTES * 3 + 7,
        ] {
            let src: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut wire = Vec::new();
            write_chunk(&mut wire, &src).await.unwrap();
            write_end(&mut wire).await.unwrap();

            let mut out = Vec::new();
            let n = read_body(&mut wire.as_slice(), &mut out).await.unwrap();
            assert_eq!(n, len as u64, "len {len}");
            assert_eq!(out, src, "len {len}");
        }
    }

    #[tokio::test]
    async fn a_chunk_header_never_exceeds_the_ceiling() {
        // The splitting in `write_chunk` is what makes an out-of-range header
        // unreachable from any caller.
        let src = vec![7u8; MAX_CHUNK_BYTES * 2 + 5];
        let mut wire = Vec::new();
        write_chunk(&mut wire, &src).await.unwrap();
        write_end(&mut wire).await.unwrap();
        let headers: Vec<usize> = {
            let mut hs = Vec::new();
            let mut rest = wire.as_slice();
            loop {
                let nl = rest.iter().position(|&b| b == b'\n').unwrap();
                let n: usize = std::str::from_utf8(&rest[..nl]).unwrap().parse().unwrap();
                hs.push(n);
                if n == 0 {
                    break;
                }
                rest = &rest[nl + 1 + n..];
            }
            hs
        };
        assert_eq!(headers, vec![MAX_CHUNK_BYTES, MAX_CHUNK_BYTES, 5, 0]);
    }

    /// The failure the terminator exists for: the dashboard is killed
    /// mid-stream, so the shim must be able to tell truncation from success.
    #[tokio::test]
    async fn a_body_cut_off_mid_stream_is_an_error_not_a_short_success() {
        let src = vec![1u8; 5000];
        let mut full = Vec::new();
        write_chunk(&mut full, &src).await.unwrap();
        write_end(&mut full).await.unwrap();

        // Cut at every plausible point: inside the header, inside the body, and
        // exactly at the body's end with the terminator missing.
        for cut in [0, 1, 3, 4, 100, 2000, full.len() - 2] {
            let mut out = Vec::new();
            let e = read_body(&mut &full[..cut], &mut out).await.unwrap_err();
            assert_eq!(
                e.kind(),
                io::ErrorKind::UnexpectedEof,
                "cut at {cut} reported {e:?}"
            );
        }
        // And the complete stream is not an error.
        let mut out = Vec::new();
        read_body(&mut full.as_slice(), &mut out).await.unwrap();
        assert_eq!(out, src);
    }

    #[tokio::test]
    async fn a_bad_chunk_header_is_rejected() {
        for wire in [
            "99999999\n".to_string(),             // over the ceiling
            format!("{}\n", MAX_CHUNK_BYTES + 1), // one over
            "-1\n".to_string(),
            "abc\n".to_string(),
            "1 2\n".to_string(),
            "0x10\n".to_string(),
        ] {
            let mut out = Vec::new();
            let e = read_body(&mut wire.as_bytes(), &mut out).await.unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{wire:?}");
        }
        assert_eq!(parse_chunk_header("0"), Some(0));
        assert_eq!(parse_chunk_header(" 65536 "), Some(MAX_CHUNK_BYTES));
        assert_eq!(parse_chunk_header("65537"), None);
    }

    #[tokio::test]
    async fn a_line_is_bounded_and_eof_is_distinguishable() {
        // A clean hangup before any byte.
        assert_eq!(read_ascii_line(&mut &b""[..], 16).await.unwrap(), None);
        // Bytes then EOF is not a line.
        let e = read_ascii_line(&mut &b"v1 targ"[..], 64).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        // A peer that streams without ever sending a newline is cut off rather
        // than allocated for.
        let flood = vec![b'x'; 4096];
        let e = read_ascii_line(&mut flood.as_slice(), 64)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        // Exactly at the cap is fine; the cap excludes the newline.
        let at_cap = format!("{}\n", "x".repeat(64));
        assert_eq!(
            read_ascii_line(&mut at_cap.as_bytes(), 64).await.unwrap(),
            Some("x".repeat(64))
        );
        // One over is not.
        let over = format!("{}\n", "x".repeat(65));
        assert!(read_ascii_line(&mut over.as_bytes(), 64).await.is_err());
        // An empty line is a line, not an EOF.
        assert_eq!(
            read_ascii_line(&mut &b"\nrest"[..], 16).await.unwrap(),
            Some(String::new())
        );
    }

    // -- The shim's argv --

    /// §1's call list, grepped out of the agent's own binary, as assertions.
    /// This is the entire shim contract; if any row here changes, the shim is
    /// answering a question nobody is asking.
    #[test]
    fn the_calls_the_agent_actually_makes() {
        use Format::{Bmp, Png};
        use ShimCall::{Delegate, Fetch, Targets};
        use ShimTool::{WlPaste, Xclip};

        let cases: &[(ShimTool, &[&str], ShimCall)] = &[
            // availability — xclip first, wl-paste as the fallback
            (
                Xclip,
                &["-selection", "clipboard", "-t", "TARGETS", "-o"],
                Targets,
            ),
            (WlPaste, &["-l"], Targets),
            // the fetch chain, in the order its `||` links run
            (
                Xclip,
                &["-selection", "clipboard", "-t", "image/png", "-o"],
                Fetch(Png),
            ),
            (WlPaste, &["--type", "image/png"], Fetch(Png)),
            (
                Xclip,
                &["-selection", "clipboard", "-t", "image/bmp", "-o"],
                Fetch(Bmp),
            ),
            (WlPaste, &["--type", "image/bmp"], Fetch(Bmp)),
            // text — never ours, and the wl-paste fallback is BARE
            (
                Xclip,
                &["-selection", "clipboard", "-t", "text/plain", "-o"],
                Delegate,
            ),
            (WlPaste, &[], Delegate),
        ];
        for (tool, args, want) in cases {
            assert_eq!(&classify(*tool, args), want, "{tool:?} {args:?}");
        }
    }

    #[test]
    fn anything_but_a_clipboard_image_read_delegates() {
        use ShimCall::{Delegate, Fetch, Targets};
        use ShimTool::{WlPaste, Xclip};

        let cases: &[(ShimTool, &[&str], ShimCall)] = &[
            // Wrong selection: PRIMARY is a different clipboard than the one we
            // hold, and a loose prefix match would have read this as ours.
            (
                Xclip,
                &["-selection", "primary", "-t", "image/png", "-o"],
                Delegate,
            ),
            (Xclip, &["-sel", "p", "-t", "image/png", "-o"], Delegate),
            // No selection at all: xclip's default is PRIMARY.
            (Xclip, &["-t", "image/png", "-o"], Delegate),
            // No `-o`: this is a copy, and we never serve copies.
            (
                Xclip,
                &["-selection", "clipboard", "-t", "image/png"],
                Delegate,
            ),
            (
                Xclip,
                &["-i", "-selection", "clipboard", "-t", "image/png"],
                Delegate,
            ),
            // No target: xclip's default is text.
            (Xclip, &["-selection", "clipboard", "-o"], Delegate),
            // A format we cannot produce.
            (
                Xclip,
                &["-selection", "clipboard", "-t", "image/jpeg", "-o"],
                Delegate,
            ),
            (WlPaste, &["--type", "image/jpeg"], Delegate),
            (WlPaste, &["--type", "text/plain"], Delegate),
            // A positional: `xclip file` is input mode, `wl-paste --watch cmd`
            // is a subscription.
            (
                Xclip,
                &["-selection", "clipboard", "-o", "file.png"],
                Delegate,
            ),
            (WlPaste, &["--watch", "cat"], Delegate),
            // Not the clipboard selection.
            (WlPaste, &["--primary", "--type", "image/png"], Delegate),
            (WlPaste, &["-p", "-l"], Delegate),
            // A flag we have never seen: we do not know what it changes.
            (
                Xclip,
                &["-selection", "clipboard", "-t", "TARGETS", "-o", "-d", ":1"],
                Delegate,
            ),
            (WlPaste, &["--seat", "seat0", "-l"], Delegate),
            // A flag whose value is missing.
            (Xclip, &["-selection", "clipboard", "-t"], Delegate),
            (WlPaste, &["--type"], Delegate),
            (Xclip, &["-selection"], Delegate),
            // Spellings that are still exactly the read we serve.
            (
                Xclip,
                &["-selection", "c", "-t", "TARGETS", "-out"],
                Targets,
            ),
            (
                Xclip,
                &["--selection=clipboard", "--target=image/png", "-o"],
                Fetch(Format::Png),
            ),
            (
                Xclip,
                &["-o", "-t", "image/png", "-sel", "clip"],
                Fetch(Format::Png),
            ),
            (WlPaste, &["--list-types"], Targets),
            (WlPaste, &["-l", "-n"], Targets),
            (WlPaste, &["--type=image/png"], Fetch(Format::Png)),
            (WlPaste, &["-t", "image/png", "-n"], Fetch(Format::Png)),
        ];
        for (tool, args, want) in cases {
            assert_eq!(&classify(*tool, args), want, "{tool:?} {args:?}");
        }
    }

    #[test]
    fn argv0_decides_whether_we_are_a_shim_at_all() {
        assert_eq!(ShimTool::from_argv0("xclip"), Some(ShimTool::Xclip));
        assert_eq!(
            ShimTool::from_argv0("/home/miao/.cache/captain-miao/shims/wl-paste"),
            Some(ShimTool::WlPaste)
        );
        // Our own name, and the helper, are ordinary runs — not shimmed calls.
        assert_eq!(ShimTool::from_argv0("miao-server"), None);
        assert_eq!(ShimTool::from_argv0(PASTE_HELPER_NAME), None);
        assert_eq!(ShimTool::from_argv0("xclip-extra"), None);
        assert_eq!(ShimTool::from_argv0(""), None);
        assert_eq!(ShimTool::from_argv0("/"), None);
    }

    #[test]
    fn the_farm_never_shadows_the_posix_paste() {
        assert!(
            !SHIM_NAMES.contains(&"paste"),
            "`paste` merges lines of files"
        );
        assert!(SHIM_NAMES.contains(&PASTE_HELPER_NAME));
        for t in [ShimTool::Xclip, ShimTool::WlPaste] {
            assert!(SHIM_NAMES.contains(&t.binary()));
        }
    }

    #[test]
    fn targets_render_one_mime_per_line() {
        // Both tools print it this way, which is what the caller's `grep -E`
        // expects.
        assert_eq!(render_targets(&[]), "");
        assert_eq!(render_targets(&[Format::Png]), "image/png\n");
        assert_eq!(render_targets(&Format::ALL), "image/png\nimage/bmp\n");
    }

    // -- Delegation --

    struct Farm {
        root: PathBuf,
    }

    impl Farm {
        /// A shim dir whose `xclip` is a symlink to `bin/miao-server`, plus a
        /// `usr` dir holding the real thing — the exact layout the guard exists
        /// for.
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "cm-clipboard-delegate-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            for d in ["bin", "shims", "usr"] {
                std::fs::create_dir_all(root.join(d)).unwrap();
            }
            let me = root.join("bin/miao-server");
            write_exe(&me, b"#!/bin/sh\n");
            for name in SHIM_NAMES {
                std::os::unix::fs::symlink(&me, root.join("shims").join(name)).unwrap();
            }
            write_exe(&root.join("usr/xclip"), b"#!/bin/sh\nexit 0\n");
            Farm { root }
        }

        fn me(&self) -> Option<FileId> {
            executable_id(&self.root.join("bin/miao-server"))
        }

        fn path(&self, dirs: &[&str]) -> std::ffi::OsString {
            std::env::join_paths(dirs.iter().map(|d| {
                if d.is_empty() {
                    PathBuf::new()
                } else {
                    self.root.join(d)
                }
            }))
            .unwrap()
        }
    }

    impl Drop for Farm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn write_exe(path: &Path, body: &[u8]) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The whole reason the guard is `(dev, ino)` and not a string compare on
    /// the shim dir: the farm entry is a *symlink*, so following it lands on the
    /// running binary's inode and is skipped however `PATH` spells the dir.
    #[test]
    fn the_self_guard_skips_the_farm_and_finds_the_real_tool() {
        let farm = Farm::new("guard");
        let found = resolve_delegate("xclip", &farm.path(&["shims", "usr"]), farm.me());
        assert_eq!(found, Some(farm.root.join("usr/xclip")));

        // An aliased spelling of the same dir — what string filtering misses.
        let aliased = std::env::join_paths([
            farm.root.join("shims"),
            farm.root.join("./shims"),
            farm.root.join("usr"),
        ])
        .unwrap();
        assert_eq!(
            resolve_delegate("xclip", &aliased, farm.me()),
            Some(farm.root.join("usr/xclip"))
        );
    }

    #[test]
    fn without_a_self_identity_the_guard_is_off_rather_than_total() {
        // `current_exe` failing is not a reason to stop delegating, so `None`
        // must not filter every candidate out.
        let farm = Farm::new("noself");
        assert_eq!(
            resolve_delegate("xclip", &farm.path(&["shims", "usr"]), None),
            Some(farm.root.join("shims/xclip"))
        );
    }

    #[test]
    fn nothing_to_delegate_to_is_none() {
        let farm = Farm::new("missing");
        // Only the farm on PATH: every candidate is us.
        assert_eq!(
            resolve_delegate("xclip", &farm.path(&["shims"]), farm.me()),
            None
        );
        // A tool nobody has.
        assert_eq!(
            resolve_delegate("wl-paste", &farm.path(&["shims", "usr"]), farm.me()),
            None
        );
        assert_eq!(
            resolve_delegate("xclip", &std::ffi::OsString::new(), farm.me()),
            None
        );
    }

    #[test]
    fn a_candidate_must_be_an_executable_file() {
        let farm = Farm::new("nonexec");
        std::fs::write(farm.root.join("bin/xclip"), b"not executable").unwrap();
        std::fs::set_permissions(
            farm.root.join("bin/xclip"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        // A directory named `xclip` must not win either.
        std::fs::create_dir_all(farm.root.join("usr/dir/xclip")).unwrap();
        assert_eq!(
            resolve_delegate("xclip", &farm.path(&["bin", "usr/dir", "usr"]), farm.me()),
            Some(farm.root.join("usr/xclip"))
        );
    }

    #[test]
    fn an_empty_path_entry_is_not_the_cwd() {
        // POSIX reads an empty entry as `.`; exec'ing whatever sits in the
        // agent's working directory is not what a PATH lookup should do.
        let farm = Farm::new("emptyentry");
        assert_eq!(
            resolve_delegate("xclip", &farm.path(&["", "usr"]), farm.me()),
            Some(farm.root.join("usr/xclip"))
        );
    }
}
