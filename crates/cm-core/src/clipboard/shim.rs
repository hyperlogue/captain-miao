//! The shim: `miao-server` invoked under another tool's name, plus the
//! `clipboard paste` helper for an agent that can't be shimmed at all.
//!
//! # How we get here
//!
//! The launcher prepends a dir of symlinks to the agent's `PATH`
//! ([`ensure_farm`]) and `miao-server`'s `main` checks [`from_argv0`] **before
//! clap sees anything** — clap exits 2 on an argv it doesn't recognize, and
//! `xclip -selection clipboard -t TARGETS -o` is exactly such an argv.
//!
//! Only a *pooled* launcher installs the farm. A local windowed session runs on
//! the machine that owns the clipboard, in a terminal that inherited the user's
//! `DISPLAY`, so `xclip` already works there and routing it through us would be
//! pure indirection with a new failure mode.
//!
//! # One rule
//!
//! **When we can't serve, behave exactly as if we weren't installed.** Every path
//! that isn't a clipboard image read ends in [`Shim::delegate`], which `exec`s the
//! real tool so its stdio and exit status are its own. That is what makes the shim
//! safe on every pooled session — including pooled-localhost, where it fixes a
//! pool daemon with no `DISPLAY` and is merely redundant when there is one — and
//! it is why a future change in how the agent probes the clipboard degrades
//! instead of breaking.
//!
//! The real tool is found by walking `PATH` and skipping any candidate whose
//! `(dev, ino)` is our own ([`super::resolve_delegate`]) — never by filtering our
//! dir out of `PATH`, which would miss an aliased spelling of it and leave the
//! shim exec'ing itself in a loop.
//!
//! # Exit codes are the whole error channel
//!
//! The agent invokes these tools with `2>/dev/null` and the fetch redirected to a
//! file, so nothing we print to stderr is ever seen. What it *does* read is the
//! exit status, and its fetch is a single `||` chain whose every link re-truncates
//! that file — so a non-zero exit degrades cleanly to its own "no image in
//! clipboard". That is why a body cut off mid-stream exits **1** rather than
//! pretending success: the partial bytes on stdout are about to be overwritten by
//! the next link.

use std::ffi::OsString;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::{
    Format, MAX_RESPONSE_BYTES, Request, Response, SHIM_NAMES, ShimCall, ShimTool, paths,
    read_ascii_line, read_body, render_targets, resolve_delegate_from_env,
};

/// How long the server gets to answer with a response *header*.
///
/// Bounded because nothing has been written to stdout yet, so a timeout can still
/// degrade to a clean delegate. The body deliberately has no deadline of its own:
/// a large image over a slow link is legitimate, and the server's idle timeout
/// plus the link erroring already bound it.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// The shim farm, relative to `$HOME` on the machine the agent runs on.
const FARM_REL: &str = ".cache/captain-miao/shims";

/// Where `clipboard paste` writes, relative to `$HOME`.
const PASTE_REL: &str = ".cache/captain-miao/paste";

/// A shimmed invocation: which tool we were called as, and its arguments.
pub struct Shim {
    tool: ShimTool,
    /// Verbatim, because [`Shim::delegate`] has to `exec` the real tool with
    /// exactly what we were given. Classification uses a lossy copy — a
    /// non-UTF-8 argument matches none of our flags and so delegates, which is
    /// the right answer for it anyway.
    args: Vec<OsString>,
}

/// What the farm can invoke us as.
pub enum Invocation {
    /// A shadowed tool: `xclip` or `wl-paste`.
    Tool(Shim),
    /// The helper name, equivalent to `miao-server clipboard paste`.
    Paste,
}

/// Whether this process was invoked through the farm rather than under its own
/// name. `None` means an ordinary subcommand run, and `main` should go on to
/// clap.
pub fn from_argv0() -> Option<Invocation> {
    let mut argv = std::env::args_os();
    let argv0 = argv.next()?;
    let name = argv0.to_string_lossy();
    let base = Path::new(name.as_ref())
        .file_name()?
        .to_string_lossy()
        .into_owned();
    if base == super::PASTE_HELPER_NAME {
        return Some(Invocation::Paste);
    }
    let tool = ShimTool::from_argv0(&name)?;
    Some(Invocation::Tool(Shim {
        tool,
        args: argv.collect(),
    }))
}

impl Invocation {
    /// Do it, then exit. Never returns.
    ///
    /// Owns the exit code because a shimmed call has no other error channel: the
    /// agent reads the status and nothing else (see the module doc).
    pub fn run(self) -> ! {
        match self {
            Invocation::Tool(shim) => shim.run(),
            Invocation::Paste => {
                // A runtime of its own, because this is reached before `main` has
                // built one — the argv[0] dispatch has to precede clap.
                let code = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|rt| rt.block_on(paste()))
                {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("{e:#}");
                        1
                    }
                };
                std::process::exit(code)
            }
        }
    }
}

impl Shim {
    /// Answer or delegate, then exit. Never returns — the delegate path `exec`s.
    pub fn run(self) -> ! {
        let code = match self.classify() {
            ShimCall::Delegate => self.delegate(),
            ShimCall::Targets => self.answer(Request::Targets),
            ShimCall::Fetch(fmt) => self.answer(Request::Image(fmt)),
        };
        std::process::exit(code)
    }

    fn classify(&self) -> ShimCall {
        let lossy: Vec<String> = self
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let refs: Vec<&str> = lossy.iter().map(String::as_str).collect();
        super::classify(self.tool, &refs)
    }

    fn answer(self, request: Request) -> i32 {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return self.delegate();
        };
        match rt.block_on(ask(request)) {
            Served::Yes => 0,
            // Bytes may already be on stdout; the next link of the agent's chain
            // re-truncates the file, so being detectably broken is the point.
            Served::Truncated => 1,
            Served::No => self.delegate(),
        }
    }

    /// `exec` the real tool, so its stdio and exit status are its own.
    fn delegate(self) -> i32 {
        let Some(real) = resolve_delegate_from_env(self.tool.binary()) else {
            // Not installed here at all — which is the common case on a remote,
            // and is why the agent has a fallback chain. Non-zero, so it takes
            // it.
            return 1;
        };
        let e = std::process::Command::new(&real).args(&self.args).exec();
        // `exec` returns only on failure. Nothing reads this (the agent sends
        // stderr to /dev/null); it is for a human running the shim by hand.
        eprintln!("miao: could not exec {}: {e}", real.display());
        1
    }
}

/// What one exchange with the server came to.
enum Served {
    /// Answered in full; stdout carries it.
    Yes,
    /// Nothing to serve, nothing listening, or an answer we didn't ask for —
    /// delegate.
    No,
    /// The response header arrived but the body was cut off. Stdout may hold a
    /// prefix of it.
    Truncated,
}

async fn ask(request: Request) -> Served {
    let Some(stream) = connect().await else {
        return Served::No;
    };
    let (r, mut w) = stream.into_split();
    if w.write_all(request.to_wire().as_bytes()).await.is_err() || w.flush().await.is_err() {
        return Served::No;
    }
    let mut r = BufReader::new(r);
    let line =
        match tokio::time::timeout(HEADER_TIMEOUT, read_ascii_line(&mut r, MAX_RESPONSE_BYTES))
            .await
        {
            Ok(Ok(Some(line))) => line,
            // A hangup, a malformed line, or a server that never answered. All the
            // same event as far as the caller can tell.
            _ => return Served::No,
        };
    let mut out = tokio::io::stdout();
    match (request, Response::parse(&line)) {
        (Request::Targets, Some(Response::Types(formats))) if !formats.is_empty() => {
            if out
                .write_all(render_targets(&formats).as_bytes())
                .await
                .is_err()
                || out.flush().await.is_err()
            {
                return Served::Truncated;
            }
            Served::Yes
        }
        // A header for a format we didn't ask for is not ours to write out.
        (Request::Image(want), Some(Response::Image(got))) if got == want => {
            match read_body(&mut r, &mut out).await {
                Ok(_) => Served::Yes,
                Err(_) => Served::Truncated,
            }
        }
        // `v1 none`, an unparsable line, an empty type list: not servable.
        _ => Served::No,
    }
}

/// First candidate socket that answers — see [`paths::shim_socket_candidates`]
/// for why there are two and why this order.
async fn connect() -> Option<UnixStream> {
    for path in paths::shim_socket_candidates() {
        if let Ok(stream) = UnixStream::connect(&path).await {
            return Some(stream);
        }
    }
    None
}

/// Mint (or repair) the symlink farm and return its dir.
///
/// Symlinks to the binary that already ran, not scripts: nothing to template,
/// nothing to rot, and **no `bash` on the remote** — which was one of the real
/// fragilities of the tool this replaced.
///
/// Idempotent, and self-healing: a link already pointing at this exe is left
/// alone, and one pointing elsewhere (an upgrade moved the binary) is re-pointed.
/// A concurrent launcher minting the same farm is tolerated — losing the race
/// just means the link already says what we were going to write.
pub fn ensure_farm() -> Result<PathBuf> {
    let dir = home_rel(FARM_REL).context("no $HOME to put the clipboard shims in")?;
    crate::state::create_dir_all_private(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    let exe = std::env::current_exe().context("could not resolve our own path")?;
    for name in SHIM_NAMES {
        let link = dir.join(name);
        if std::fs::read_link(&link).is_ok_and(|t| t == exe) {
            continue;
        }
        let _ = std::fs::remove_file(&link);
        if let Err(e) = std::os::unix::fs::symlink(&exe, &link)
            && !std::fs::read_link(&link).is_ok_and(|t| t == exe)
        {
            return Err(e).with_context(|| format!("could not link {}", link.display()));
        }
    }
    Ok(dir)
}

/// `clipboard paste`: write the dashboard machine's clipboard image to a file
/// **here** and print its path.
///
/// For an agent that reads the clipboard in-process rather than by shelling out,
/// so no shim can help it — Codex today. This is the one place the bridge writes
/// clipboard bytes to disk, and it is the exception the policy names: printing a
/// path on this machine is the entire purpose.
///
/// One file per format, overwritten, so there is **no history on disk** — the
/// thing that made a clipboard-syncing daemon a liability. Written to a `.part`
/// and renamed, so a reader never sees half an image, and 0600 because it holds
/// the user's screenshot.
pub async fn paste() -> Result<()> {
    // No `targets` round trip: asking for png and falling back to bmp is the same
    // two requests in the worst case and one in the common one.
    for fmt in Format::ALL {
        if let Some(path) = fetch_to_file(fmt).await? {
            println!("{}", path.display());
            return Ok(());
        }
    }
    anyhow::bail!(
        "no image on the clipboard — or this host is not offered one \
         (turn it on with `p` in the dashboard's hosts panel)"
    )
}

async fn fetch_to_file(fmt: Format) -> Result<Option<PathBuf>> {
    let Some(stream) = connect().await else {
        return Ok(None);
    };
    let (r, mut w) = stream.into_split();
    w.write_all(Request::Image(fmt).to_wire().as_bytes())
        .await?;
    w.flush().await?;
    let mut r = BufReader::new(r);
    let Some(line) = read_ascii_line(&mut r, MAX_RESPONSE_BYTES).await? else {
        return Ok(None);
    };
    if Response::parse(&line) != Some(Response::Image(fmt)) {
        return Ok(None);
    }

    let dir = home_rel(PASTE_REL).context("no $HOME to write the clipboard image to")?;
    crate::state::create_dir_all_private(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    let final_path = dir.join(format!("clipboard.{}", fmt.token()));
    let part = dir.join(format!("clipboard.{}.part", fmt.token()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&part)
        .with_context(|| format!("could not write {}", part.display()))?;
    let mut file = tokio::fs::File::from_std(file);
    // A body that ends without its terminator is an error, and the `.part` is
    // left behind rather than published — the caller must not be handed a path
    // to a truncated image.
    read_body(&mut r, &mut file).await.context(
        "the clipboard image was cut off before it finished (the dashboard may have gone away)",
    )?;
    file.flush().await?;
    drop(file);
    std::fs::rename(&part, &final_path)
        .with_context(|| format!("could not publish {}", final_path.display()))?;
    Ok(Some(final_path))
}

/// `$HOME`-relative path on *this* machine, or `None` with no `$HOME`.
fn home_rel(rel: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(Path::new(&home).join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_farm_and_the_paste_dir_live_under_the_cache() {
        // The same `~/.cache/captain-miao` the deployed server already uses, so a
        // host accumulates one captain-miao dir rather than three.
        for rel in [FARM_REL, PASTE_REL] {
            assert!(rel.starts_with(".cache/captain-miao/"), "{rel}");
            assert!(!rel.starts_with('/'), "{rel} must be home-relative");
        }
        assert_ne!(FARM_REL, PASTE_REL);
    }

    /// Minting is idempotent, repairs a link left pointing at an older binary,
    /// and produces exactly the names the farm is documented to carry.
    #[test]
    fn the_farm_is_idempotent_and_self_healing() {
        let root = std::env::temp_dir().join(format!("cm-clipboard-farm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("shims");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().unwrap();

        // The body of `ensure_farm`, against a scratch dir rather than `$HOME`
        // (which the test must not touch).
        let mint = || {
            for name in SHIM_NAMES {
                let link = dir.join(name);
                if std::fs::read_link(&link).is_ok_and(|t| t == exe) {
                    continue;
                }
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&exe, &link).unwrap();
            }
        };
        mint();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["clipboard-paste", "wl-paste", "xclip"]);
        for name in SHIM_NAMES {
            assert_eq!(std::fs::read_link(dir.join(name)).unwrap(), exe);
            // Following the link lands on the running binary, which is what makes
            // the delegate guard skip it.
            assert_eq!(
                super::super::self_id(),
                {
                    let m = std::fs::metadata(dir.join(name)).unwrap();
                    use std::os::unix::fs::MetadataExt;
                    Some((m.dev(), m.ino()))
                },
                "{name} does not resolve to us"
            );
        }

        // A link an upgrade left pointing elsewhere is re-pointed, not skipped.
        let stale = root.join("old-miao-server");
        std::fs::write(&stale, b"old").unwrap();
        std::fs::remove_file(dir.join("xclip")).unwrap();
        std::os::unix::fs::symlink(&stale, dir.join("xclip")).unwrap();
        mint();
        assert_eq!(std::fs::read_link(dir.join("xclip")).unwrap(), exe);

        // And a second pass changes nothing.
        mint();
        assert_eq!(std::fs::read_link(dir.join("wl-paste")).unwrap(), exe);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_utf8_argument_delegates_rather_than_being_mangled() {
        use std::os::unix::ffi::OsStringExt;
        // The verbatim args are what `exec` gets, so classification must not be
        // the thing that decides they are valid — a lossy byte matches none of
        // our flags and lands on Delegate.
        let shim = Shim {
            tool: ShimTool::Xclip,
            args: vec![
                OsString::from("-selection"),
                OsString::from_vec(vec![0xff, 0xfe]),
                OsString::from("-t"),
                OsString::from("image/png"),
                OsString::from("-o"),
            ],
        };
        assert_eq!(shim.classify(), ShimCall::Delegate);
        // …and the bytes survive for the exec.
        assert_eq!(shim.args[1].as_encoded_bytes(), &[0xff, 0xfe]);
    }

    #[test]
    fn argv0_is_what_decides_we_are_a_shim() {
        // `from_argv0` reads the real process argv, so the decision itself is
        // pinned through `ShimTool` instead — this only checks the shape the
        // dispatcher relies on: our own name is not a shim.
        assert!(ShimTool::from_argv0("miao-server").is_none());
        assert!(ShimTool::from_argv0("/x/shims/xclip").is_some());
    }
}
