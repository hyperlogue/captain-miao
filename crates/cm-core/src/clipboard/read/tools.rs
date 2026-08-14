//! The Linux (and any other non-macOS unix) clipboard read: `wl-paste` and
//! `xclip`, spawned as children.
//!
//! Two tools rather than one because a dashboard machine is Wayland or X11 and we
//! do not get to choose. Both are tried for every question, which is also what
//! the agent's own clipboard chain does — so a machine with only one of them
//! installed works, and a compositor that answers for neither degrades to
//! "nothing servable".
//!
//! **No byte off the wire reaches an argv.** Every argument here is a fixed
//! `&'static str`, including the MIME name, which comes from the [`Format`] enum
//! the protocol's allowlist produced.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{Image, peek_first};
use crate::clipboard::Format;

/// Cap on a type listing. The real answers are a few hundred bytes; this only
/// stops a broken tool from streaming into our buffer forever.
const LIST_CAP: u64 = 64 * 1024;

/// How long the availability probe gets. It sits on the paste keystroke path, so
/// this is a stall the user would feel — but a shorter one risks a false "no
/// image" on a loaded machine.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// A clipboard tool we can ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    WlPaste,
    Xclip,
}

impl Tool {
    fn bin(self) -> &'static str {
        match self {
            Tool::WlPaste => "wl-paste",
            Tool::Xclip => "xclip",
        }
    }

    /// "What types are on the clipboard?" — one MIME name per line from both.
    fn list_args(self) -> &'static [&'static str] {
        match self {
            Tool::WlPaste => &["--list-types"],
            // The selection is explicit because xclip's default is PRIMARY,
            // which is a different clipboard than the one anyone means.
            Tool::Xclip => &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        }
    }

    /// "Give me the bytes." `--no-newline` is belt-and-braces: current
    /// wl-clipboard forces it for non-text MIME types anyway, and older versions
    /// appended a byte decoders ignore.
    fn fetch_args(self, fmt: Format) -> Vec<&'static str> {
        match self {
            Tool::WlPaste => vec!["--no-newline", "--type", fmt.mime()],
            Tool::Xclip => vec!["-selection", "clipboard", "-t", fmt.mime(), "-o"],
        }
    }

    fn command(self, args: &[&str]) -> Command {
        let mut c = Command::new(self.bin());
        c.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // These tools are chatty on a machine with no display and there is
            // nobody to read it; the exit status is our diagnosis.
            .stderr(Stdio::null())
            .kill_on_drop(true);
        c
    }

    /// The tool's type listing, or `None` if it isn't installed, failed, or
    /// answered nothing. Dropping the future on timeout kills the child.
    async fn list(self) -> Option<String> {
        match tokio::time::timeout(LIST_TIMEOUT, self.list_inner()).await {
            Ok(r) => r,
            Err(_) => {
                tracing::debug!(tool = self.bin(), "clipboard type listing timed out");
                None
            }
        }
    }

    async fn list_inner(self) -> Option<String> {
        let mut child = self.command(self.list_args()).spawn().ok()?;
        let mut stdout = child.stdout.take()?;
        let mut buf = Vec::new();
        (&mut stdout)
            .take(LIST_CAP)
            .read_to_end(&mut buf)
            .await
            .ok()?;
        let status = child.wait().await.ok()?;
        if !status.success() {
            return None;
        }
        String::from_utf8(buf).ok()
    }

    /// Start a fetch. `None` means this tool has nothing for that format — a
    /// decision made on the first read, before any header is committed to.
    async fn fetch(self, fmt: Format) -> Option<Image> {
        let args = self.fetch_args(fmt);
        let mut child = self.command(&args).spawn().ok()?;
        let mut stdout = child.stdout.take()?;
        match peek_first(&mut stdout).await {
            Ok(Some(first)) => Some(Image::streaming(first, stdout, child, self.bin())),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(tool = self.bin(), error = %e, "clipboard fetch produced nothing");
                None
            }
        }
    }
}

/// Which tool to believe first.
///
/// Both are tried either way, so this only decides which clipboard wins under
/// XWayland — where the Wayland one is the real one.
fn order(wayland: bool) -> [Tool; 2] {
    if wayland {
        [Tool::WlPaste, Tool::Xclip]
    } else {
        [Tool::Xclip, Tool::WlPaste]
    }
}

fn preference() -> [Tool; 2] {
    order(std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()))
}

/// The image formats named in a type listing, in our preference order rather
/// than the clipboard's.
///
/// A listing carries plenty we can't serve — X11 atoms like `TIMESTAMP`, and the
/// `image/jpeg` a browser copy leaves — and the intersection with [`Format::ALL`]
/// is the gate: we advertise only what we can actually hand over.
fn formats_in(listing: &str) -> Vec<Format> {
    Format::ALL
        .into_iter()
        .filter(|f| listing.lines().any(|l| l.trim() == f.mime()))
        .collect()
}

pub(super) async fn available() -> Vec<Format> {
    for tool in preference() {
        let Some(listing) = tool.list().await else {
            continue;
        };
        let formats = formats_in(&listing);
        // An empty answer is not "ask the other tool" — it is a clipboard with no
        // image on it. But we do keep going, because the tools can be looking at
        // different selections and the agent's own chain does the same.
        if !formats.is_empty() {
            tracing::debug!(tool = tool.bin(), ?formats, "clipboard offers images");
            return formats;
        }
    }
    Vec::new()
}

pub(super) async fn open(fmt: Format) -> Option<Image> {
    for tool in preference() {
        if let Some(img) = tool.fetch(fmt).await {
            return Some(img);
        }
    }
    None
}

/// A live end-to-end read, for a machine that actually has a clipboard.
///
/// Recipe: copy an image (a screenshot, or `wl-copy --type image/png < x.png`),
/// then
///
/// ```sh
/// cargo test -p cm-core -- --ignored reads_the_real_clipboard --nocapture
/// ```
#[cfg(test)]
#[tokio::test]
#[ignore = "needs a display and an image on the clipboard"]
async fn reads_the_real_clipboard() {
    let formats = available().await;
    assert!(
        !formats.is_empty(),
        "no image on the clipboard — copy one first"
    );
    let fmt = formats[0];
    let img = open(fmt).await.expect("advertised but not servable");
    let mut wire = Vec::new();
    let n = img.copy_framed(&mut wire).await.unwrap();
    crate::clipboard::write_end(&mut wire).await.unwrap();
    let mut body = Vec::new();
    crate::clipboard::read_body(&mut wire.as_slice(), &mut body)
        .await
        .unwrap();
    assert_eq!(body.len() as u64, n);
    if fmt == Format::Png {
        assert_eq!(&body[..4], b"\x89PNG", "not a PNG");
    }
    println!("{} bytes of {}", n, fmt.mime());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The injection gate at the call site: every argument is a fixed string, and
    /// the MIME name comes from the enum the protocol's allowlist produced — so
    /// there is no path from a socket byte into an argv.
    #[test]
    fn the_argv_is_fixed_strings_and_nothing_else() {
        assert_eq!(
            Tool::Xclip.fetch_args(Format::Png),
            ["-selection", "clipboard", "-t", "image/png", "-o"]
        );
        assert_eq!(
            Tool::Xclip.fetch_args(Format::Bmp),
            ["-selection", "clipboard", "-t", "image/bmp", "-o"]
        );
        assert_eq!(
            Tool::WlPaste.fetch_args(Format::Png),
            ["--no-newline", "--type", "image/png"]
        );
        assert_eq!(
            Tool::Xclip.list_args(),
            ["-selection", "clipboard", "-t", "TARGETS", "-o"]
        );
        assert_eq!(Tool::WlPaste.list_args(), ["--list-types"]);
        // Every argument is `&'static str` by type, so the only variable is the
        // format — and there are two of those.
        for tool in [Tool::Xclip, Tool::WlPaste] {
            for fmt in Format::ALL {
                let args = tool.fetch_args(fmt);
                assert!(args.iter().any(|a| *a == fmt.mime()));
                assert!(args.iter().all(|a| !a.is_empty()));
            }
        }
    }

    /// Real listings: xclip's TARGETS carries X11 atoms, wl-paste's carries the
    /// extra flavours a browser copy leaves behind.
    #[test]
    fn only_servable_types_are_picked_out_of_a_listing() {
        let xclip = "TIMESTAMP\nTARGETS\nMULTIPLE\nSAVE_TARGETS\nimage/png\nimage/bmp\ntext/html\n";
        assert_eq!(formats_in(xclip), vec![Format::Png, Format::Bmp]);

        let wayland = "image/png\ntext/plain;charset=utf-8\n";
        assert_eq!(formats_in(wayland), vec![Format::Png]);

        // A browser JPEG we have no converter for: advertising it would have the
        // caller grep it, ask for it, and dead-end.
        assert_eq!(formats_in("image/jpeg\nimage/webp\n"), vec![]);
        // Text only, and a clipboard with nothing on it.
        assert_eq!(formats_in("text/plain\nUTF8_STRING\n"), vec![]);
        assert_eq!(formats_in(""), vec![]);
        // A near-miss must not match: the gate is equality, not `contains`.
        assert_eq!(formats_in("x-image/png\nimage/png-alt\n"), vec![]);
        // Trailing whitespace on a line is the tool's, not a different type.
        assert_eq!(formats_in("  image/png  \n"), vec![Format::Png]);
    }

    #[test]
    fn png_is_advertised_first() {
        // The agent's availability grep takes the first image type it sees, and
        // its fetch chain asks for png before bmp.
        assert_eq!(
            formats_in("image/bmp\nimage/png\n"),
            vec![Format::Png, Format::Bmp]
        );
    }

    #[test]
    fn wayland_wins_when_there_is_a_wayland_display() {
        // Under XWayland both tools answer; the Wayland one is the real
        // clipboard. Both are still tried, so this only decides the tie.
        assert_eq!(order(true), [Tool::WlPaste, Tool::Xclip]);
        assert_eq!(order(false), [Tool::Xclip, Tool::WlPaste]);
        for wayland in [true, false] {
            let o = order(wayland);
            assert_ne!(o[0], o[1], "both tools must be tried");
        }
    }
}
