//! Ghostty backend (macOS only): wraps Ghostty's AppleScript dictionary.
//!
//! Transport is one `osascript` subprocess per call, with the script fed on
//! **stdin** rather than `-e` — the snapshot script is a dozen lines and
//! `osascript -` takes it whole, so there is no per-line argv assembly to get
//! wrong. Requires Ghostty ≥ 1.3, which is where the scripting dictionary
//! landed; older builds expose nothing at all and fail
//! [`verify_control`](Terminal::verify_control) with a message saying so.
//!
//! Vocabulary. Ghostty's object model is `application > window > tab >
//! terminal`, where a *terminal* is one surface (a split). captain-miao's is a
//! flat list of tabs each holding windows, so a captain-miao **window** is a
//! Ghostty *terminal* and a captain-miao **tab** is a Ghostty *tab*; the OS
//! window is flattened away. That is the same reduction the Kitty backend
//! performs on `os-window > tab > window`, and for the same reason: captain-miao
//! addresses the thing an agent runs in, and the OS window it happens to sit in
//! is presentation.
//!
//! Backend properties this module encodes (read off the shipped `Ghostty.sdef`
//! and the Swift behind it, **not** measured against a live Ghostty — this
//! backend is written on Linux and its live test is `#[ignore]`d):
//! - **Ids are opaque strings of three different shapes.** A terminal id is a
//!   UUID (`SurfaceView.id.uuidString`); a tab id is `tab-<hex>`; a window id is
//!   `window-<hex>` / `tab-group-<hex>` / `controller-<hex>`. So the digit-only
//!   [`super::validate_id`] every other backend uses does not apply here, and
//!   [`script_id`] validates the union instead.
//! - **Surface ids never recycle**, being UUIDs, which is what makes the
//!   speculative `close_window` the restart/kill paths rely on safe.
//! - **Ghostty exports no per-surface environment variable.** There is no
//!   `KITTY_WINDOW_ID` analog, so `current_window` cannot be an env read. It is
//!   recovered instead from the one thing that *is* both knowable locally and
//!   exposed to script — the tty: `ttyname(0)` here, `tty of terminal` there.
//!   Resolved once, lazily, and cached ([`GhosttyTerminal::own_surface`]).
//! - **Creation always activates Ghostty** (upstream ghostty#11457:
//!   `new window` / `new tab` bring the app forward with no way to opt out), so
//!   `SpawnSpec::take_focus` cannot be honoured in its `false` direction. Left
//!   as-is deliberately rather than papered over with a re-focus, which would
//!   trade a steal for a flicker and still not restore a *different* app.
//! - **Titles are read-only as properties but settable as an action.** `name` of
//!   a tab has `access="r"`, so `SpawnSpec::title` goes through
//!   `perform action "set_tab_title:…"`. That sets an *override*, which is what
//!   we want: agents emit OSC title escapes that would otherwise reclaim the tab
//!   (the same hazard tmux's `allow-rename` posed).
//! - **There is no way to read a screen or a scrollback.** The dictionary has no
//!   contents property and no command that returns text; the only paths to a
//!   window's output (`write_scrollback_file:copy`, `select_all` +
//!   `copy_to_clipboard`) go through the general pasteboard, which the preview's
//!   auto-refresh would then clobber on a timer. So `capture: false` and
//!   `capture_text` refuses — see `Capabilities::capture` for why that has to be
//!   a capability rather than a failing call.
//! - **There is no way to move a surface between tabs**, so `move_to_tab: false`
//!   and the `t` affordance hides itself, exactly as on zellij.
//! - **Neither Stacked arrangement is worth having.** `floating_sessions` is
//!   clear-cut: the quick terminal is one app-wide dropdown bound to a hotkey,
//!   not a per-session floating pane, and nothing else floats.
//!
//!   `window_stacking` is the closer call, because Ghostty *does* have
//!   `toggle_split_zoom` ("take up the entire space in the current tab, hiding
//!   other splits"), reachable through `perform action`. So sessions-as-splits
//!   with one zoomed is constructible. It is rejected for the reason
//!   `design/tmux-backend.md` §6 rejects the same emulation there, plus two
//!   Ghostty-specific costs:
//!   - **Switching costs two pty resizes per session, not zero.** There is no
//!     "zoom *this* one" — only a toggle — so moving from A to B is unzoom A
//!     (every split in the tab re-tiles), focus B, zoom B (they all resize
//!     back). Kitty's `stack` layout switches by showing a different full-size
//!     window and resizes nothing.
//!   - **Splits are a binary tree, so spawning resizes the whole tab.** Each
//!     `split` halves a sibling, so the Nth session in a shared tab resizes the
//!     N-1 already there — a repaint storm across every running agent. Joining a
//!     kitty stack tab costs the existing windows nothing.
//!
//!   A toggle with no idempotent "set" form also can't be driven by a pure
//!   viewer: captain-miao would have to track which surface it believes is
//!   zoomed and would desync the moment the user hit the keybind themselves.
//!
//!   Both flags false makes `layout_is_a_choice()` false, which hides `Space l`
//!   and resolves every spawn to `NewTab` — tmux's shape.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

use super::{
    Capabilities, SpawnCommand, SpawnResult, SpawnSpec, SpawnTarget, Tab, TabId, TabTarget,
    Terminal, WindowId,
};

/// How long [`verify_control`](Terminal::verify_control)'s probe waits before
/// declaring the channel unusable.
///
/// Generous, because the failure it guards is *user-paced*: the first Apple event
/// captain-miao sends makes macOS put up an Automation (TCC) consent dialog, and
/// `osascript` blocks on it until someone clicks. Timing that out at a few
/// seconds would fail the startup check on the one run where the user is being
/// asked to make it work. Ghostty's own answer, once permitted, is a single
/// Apple event.
const CONTROL_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Field separator inside a [`snapshot`](Terminal::snapshot) line. Free text (a
/// tab title) always comes last, so a title containing this can't shift the id
/// fields — the discipline tmux's `SNAPSHOT_FORMAT` follows for the same reason.
const SEP: char = '\u{1f}';

/// The preamble every multi-value script carries: bind the separator to a
/// variable *before* the `tell` block, then concatenate `sep` rather than a
/// literal.
///
/// Two reasons it can't be written inline. AppleScript string literals have no
/// `\x` escape — only `\n`, `\t`, `\r`, `\"` and `\\` — so U+001F cannot be
/// spelled in one at all. And the obvious readable alternative, AppleScript's
/// built-in `tab` constant, is precisely the wrong word here: inside
/// `tell application "Ghostty"` the term `tab` resolves against Ghostty's own
/// dictionary, where it names a *class*. Binding outside the block sidesteps
/// both.
const SEP_PREAMBLE: &str = "set sep to character id 31\nset lf to character id 10\n";

pub struct GhosttyTerminal {
    /// The surface the dashboard itself runs in, resolved lazily from
    /// `ttyname` on first use and then cached. `Some(None)` is a *settled*
    /// failure (no tty, or Ghostty named no surface for it) and is not retried:
    /// the answer can't change for the life of the process, and the caller
    /// (`write_dashboard_pid_and_window`) treats it as best-effort.
    own_surface: OnceLock<Option<WindowId>>,
}

impl GhosttyTerminal {
    /// Construct from the environment. `None` when this process is not inside a
    /// Ghostty surface — or when it is, but on a platform where that surface
    /// can't be driven.
    ///
    /// **The macOS gate is not incidental.** Ghostty runs on Linux too, and
    /// there the AppleScript dictionary this whole backend is does not exist:
    /// there is no `osascript`, no Apple events, and no equivalent control
    /// channel (the GTK build exposes none). A Linux Ghostty user therefore
    /// falls through to the same detection they get today rather than to a
    /// backend that would fail every call.
    pub fn from_env() -> Option<Self> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let term_program = std::env::var("TERM_PROGRAM").ok();
        cm_core::terminal::is_ghostty(term_program.as_deref().map(str::trim)).then(Self::new)
    }

    /// A backend not built from the env — the live-Ghostty test's constructor,
    /// and what `from_env` defers to once the env has been vouched for.
    fn new() -> Self {
        Self {
            own_surface: OnceLock::new(),
        }
    }

    /// The dashboard's own surface id, resolved once.
    ///
    /// Blocking on purpose: this is one `osascript` at most per process, and
    /// [`Terminal::current_window`] is sync on every backend because every other
    /// one answers it from the env. Making the whole seam async to accommodate
    /// the single backend that can't would be the tail wagging the dog; paying
    /// one blocking subprocess, once, at dashboard startup, is not.
    fn resolve_own_surface(&self) -> Option<WindowId> {
        let tty = own_tty()?;
        let script = format!(
            "tell application \"Ghostty\"\n\
             \x20 repeat with s in terminals\n\
             \x20   if (tty of s) is {} then return (id of s)\n\
             \x20 end repeat\n\
             end tell\n\
             return \"\"",
            applescript_string(&tty)
        );
        let out = std::process::Command::new("osascript")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .map(|mut w| w.write_all(script.as_bytes()))
                    .transpose()?;
                child.wait_with_output()
            })
            .ok()?;
        if !out.status.success() {
            tracing::debug!(
                "ghostty: could not resolve own surface from tty {tty}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return None;
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        script_id(&id).ok()?;
        Some(WindowId(id))
    }
}

/// The controlling terminal's device path (`/dev/ttys004`), for the surface
/// lookup above. Tries stdin, stdout, then stderr: the dashboard's stdin is
/// normally the tty, but a `miao` invoked with stdin redirected still has one on
/// another descriptor.
fn own_tty() -> Option<String> {
    for fd in [0, 1, 2] {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a live 256-byte allocation and its true length is
        // passed, so `ttyname_r` writes within it and NUL-terminates. The
        // `_r` form is the one that can be called at all here — plain
        // `ttyname` returns a pointer into a shared static buffer.
        let rc = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            continue;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..end])
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

/// Quote `s` as an AppleScript string literal, escaping the two characters that
/// can end or continue one (`"` and `\`) and dropping the two that cannot appear
/// in one at all (CR and LF — AppleScript has no multi-line literal, so a raw
/// newline is a syntax error rather than a quoted character).
///
/// Every value captain-miao splices into a script goes through here or through
/// [`script_id`]. The values are not hostile by nature — a cwd, a tab title, a
/// tty — but they are user text reaching a *parser*, and an unescaped quote
/// would at best fail the call and at worst change what the script says.
fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => {}
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Validate an id before it is interpolated into a script, failing closed.
///
/// Ghostty mints three shapes — a surface UUID, `tab-<hex>`, and
/// `window-<hex>` / `tab-group-<hex>` / `controller-<hex>` — whose union is
/// exactly `[A-Za-z0-9-]`. That charset contains neither of AppleScript's string
/// terminators, so a validated id can never break out of the literal it lands
/// in; the length cap keeps a corrupt state file from building an absurd script.
/// The only untrusted source is captain-miao's own state, so this rejects rather
/// than sanitizes — mis-targeting a window is worse than refusing one.
fn script_id(id: &str) -> Result<&str> {
    if !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(id)
    } else {
        anyhow::bail!("refusing ghostty id with unexpected characters: {id:?}");
    }
}

/// Run `script` through `osascript`, returning its stdout.
async fn osascript(script: &str) -> Result<String> {
    let mut child = Command::new("osascript")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to run osascript")?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().context("osascript stdin unavailable")?;
        stdin.write_all(script.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---- startup control check ----

/// What the startup control probe observed. Split from the probe so the
/// user-facing diagnosis is a pure function of the outcome — testable without a
/// mis-permissioned Mac to reproduce against (the shape kitty's `diagnose` uses).
#[derive(Debug, Clone, Copy)]
enum ProbeOutcome<'a> {
    /// The probe went out but nothing came back inside [`CONTROL_PROBE_TIMEOUT`].
    TimedOut,
    /// `osascript` ran and failed, with this error text.
    Failed { err: &'a str },
}

/// Turn a failed probe into an actionable message. The dashboard prints this and
/// exits, so it is the user's one chance to be told which of the three quite
/// different things went wrong.
fn diagnose(outcome: ProbeOutcome<'_>) -> String {
    let problem = match outcome {
        ProbeOutcome::TimedOut => format!(
            "Ghostty did not answer within {}s.\n\nThe first request makes macOS ask for \
             Automation permission in a dialog, and osascript waits for the answer — if no \
             dialog appeared, grant it under System Settings → Privacy & Security → \
             Automation.",
            CONTROL_PROBE_TIMEOUT.as_secs()
        ),
        ProbeOutcome::Failed { err } => {
            let lower = err.to_ascii_lowercase();
            // Ordered most-specific first. The missing-binary case carries our
            // own "Failed to run osascript" context; -1743 is TCC's refusal and
            // -1728 ("can't get …") is what a pre-1.3 Ghostty answers, since it
            // has no scripting dictionary for the term to resolve against.
            if lower.contains("failed to run osascript") {
                return format!(
                    "Ghostty automation check failed: osascript could not be run ({err}).\n\n\
                     It ships with macOS at /usr/bin/osascript — this backend only works there."
                );
            } else if err.contains("-1743") || lower.contains("not authorized") {
                "macOS denied captain-miao permission to control Ghostty.\n\nGrant it under \
                 System Settings → Privacy & Security → Automation, in the entry for the \
                 terminal or launcher captain-miao runs from."
                    .to_string()
            } else if lower.contains("expected end of line")
                || lower.contains("doesn't understand")
                || err.contains("-2741")
                || err.contains("-1753")
                || err.contains("-1728")
                || err.contains("-1708")
            {
                // The probe names `terminals`, which only exists in Ghostty's own
                // dictionary — so a terminology or compile failure here means
                // there is no dictionary to resolve it against.
                format!(
                    "Ghostty did not understand the request ({err}).\n\nThe scripting dictionary \
                     arrived in Ghostty 1.3 — older builds expose no automation at all. Check \
                     `ghostty +version`, and that Ghostty is running."
                )
            } else if lower.contains("-600") || lower.contains("application isn't running") {
                "Ghostty does not appear to be running.\n\nStart it, and run captain-miao from a \
                 Ghostty window."
                    .to_string()
            } else {
                format!("Ghostty rejected the request: {err}")
            }
        }
    };
    format!("Ghostty automation check failed: {problem}")
}

/// Ghostty's answer to [`Terminal::capabilities`]. Exported so tests assert
/// against the real value rather than a hand-built literal that could silently
/// diverge when a field is added. Rationale for each `false` is in the module doc.
pub(crate) const CAPABILITIES: Capabilities = Capabilities {
    move_to_tab: false,
    window_stacking: false,
    floating_sessions: false,
    capture: false,
};

/// The one snapshot script, run per [`snapshot`](Terminal::snapshot).
///
/// Walks `window > tab > terminal` and prints one line per tab:
/// `<tab id> SEP <focused> SEP <surface id,…> SEP <title>`. A tab counts as
/// focused only when it is `selected` in its window *and* that window is the
/// front one — `selected` alone is true once per window, which would report a
/// focused tab in every OS window.
///
/// One `osascript` process, but not one Apple event: each property read inside
/// the loops is its own event, so the cost grows with tabs × surfaces. That is
/// why it stays a `snapshot()`-only script and nothing on a hot path reaches for
/// it — the discipline zellij's `list-panes` forced, arrived at here from the
/// shape of the API rather than from a measurement.
const SNAPSHOT_SCRIPT: &str = concat!(
    "set sep to character id 31\n",
    "set lf to character id 10\n",
    "tell application \"Ghostty\"\n",
    "  set frontID to \"\"\n",
    // `front window` raises rather than answering when nothing is open, and a
    // Ghostty with no windows is an ordinary state (the dashboard may be the
    // only surface, or none may be). An empty `frontID` then matches no window,
    // which is the right answer: no tab is focused.
    "  try\n",
    "    set frontID to id of front window\n",
    "  end try\n",
    "  set out to \"\"\n",
    "  repeat with w in windows\n",
    "    set isFront to ((id of w) is frontID)\n",
    "    repeat with t in tabs of w\n",
    "      set ids to \"\"\n",
    "      repeat with s in terminals of t\n",
    "        set ids to ids & (id of s) & \",\"\n",
    "      end repeat\n",
    "      set focusedText to \"0\"\n",
    "      if isFront and (selected of t) then set focusedText to \"1\"\n",
    "      set out to out & (id of t) & sep & focusedText & sep & ids",
    " & sep & (name of t) & lf\n",
    "    end repeat\n",
    "  end repeat\n",
    "  return out\n",
    "end tell",
);

/// Parse [`SNAPSHOT_SCRIPT`]'s output into the trait's tab list.
///
/// Lenient by line: a tab whose id doesn't validate, or a line with too few
/// fields, is skipped rather than failing the whole snapshot — one unparseable
/// row must not blind the dashboard to every other window. Surface ids are
/// filtered the same way, so a malformed one drops out of its tab instead of
/// taking the tab with it.
fn parse_snapshot(out: &str) -> Vec<Tab> {
    let mut tabs = Vec::new();
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        // The title is the remainder: it is free text and goes last precisely so
        // it cannot shift the fields before it.
        let mut fields = line.splitn(4, SEP);
        let (Some(tab_id), Some(focused), Some(surfaces), Some(title)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(tab_id) = script_id(tab_id.trim()) else {
            continue;
        };
        let windows = surfaces
            .split(',')
            .map(str::trim)
            .filter(|s| script_id(s).is_ok())
            .map(|s| WindowId(s.to_string()))
            .collect();
        tabs.push(Tab {
            id: TabId(tab_id.to_string()),
            title: title.to_string(),
            is_focused: focused.trim() == "1",
            windows,
        });
    }
    tabs
}

/// Build the `surface configuration` prelude every spawn shares — the lines that
/// set a fresh `cfg` up from `spec`, ready for `new window`/`new tab`.
///
/// `command` is emitted with Ghostty's **`shell:`** prefix and a shell-quoted
/// argv rather than the cheaper `direct:`. `direct:` skips the `/bin/sh`
/// round-trip but splits on whitespace, and captain-miao's argvs routinely carry
/// elements that contain spaces — a cwd with one, and the whole
/// `/bin/sh -c '<script>'` the remote attach wrapper is. Splitting those would
/// silently run something else.
fn spawn_config_script(spec: &SpawnSpec) -> String {
    let mut s = String::from("  set cfg to new surface configuration\n");
    if !spec.cwd.is_empty() {
        s.push_str(&format!(
            "  set initial working directory of cfg to {}\n",
            applescript_string(&spec.cwd)
        ));
    }
    if let SpawnCommand::Exec(argv) = &spec.command
        && !argv.is_empty()
    {
        let joined = argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        s.push_str(&format!(
            "  set command of cfg to {}\n",
            applescript_string(&format!("shell:{joined}"))
        ));
    }
    // `SpawnCommand::Shell` sets nothing — Ghostty launches the configured shell.
    if spec.hold {
        s.push_str("  set wait after command of cfg to true\n");
    }
    // Ghostty launches from the *app*, whose environment is the login one macOS
    // handed the GUI — not the dashboard's. So a bare `miao …` argv may not
    // resolve, the same failure both multiplexer backends fix with
    // `/usr/bin/env PATH=…`; here the configuration carries it natively.
    if let Ok(path) = std::env::var("PATH") {
        s.push_str(&format!(
            "  set environment variables of cfg to {{{}}}\n",
            applescript_string(&format!("PATH={path}"))
        ));
    }
    s
}

/// Quote one argv element for `/bin/sh`. Single-quote wrapping, with the one
/// escape a single-quoted shell word admits (`'` → `'\''`), so the result is
/// safe for any byte sequence a path or a flag can hold.
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:=@,+".contains(&b))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[async_trait]
impl Terminal for GhosttyTerminal {
    fn current_window(&self) -> Option<WindowId> {
        self.own_surface
            .get_or_init(|| self.resolve_own_surface())
            .clone()
    }

    fn identity(&self) -> Option<String> {
        Some(cm_core::terminal::ghostty_identity())
    }

    /// Prove the Apple-event channel to Ghostty works, with the cheapest real
    /// request there is — one event, counting the surfaces.
    ///
    /// It counts *terminals* rather than asking for `version` on purpose, even
    /// though version is cheaper still. `version` is standard-suite terminology
    /// that resolves against any application; `terminals` is Ghostty's own, so a
    /// build without the scripting dictionary fails to compile the script at all
    /// — which is exactly the signal that separates "too old" from "not
    /// permitted", the two failures a user will actually hit.
    ///
    /// The timeout is not belt-and-braces: the *first* request a new install
    /// makes is the one macOS interrupts with an Automation consent dialog, and
    /// `osascript` blocks until it is answered. Bounding that at all is what
    /// keeps a denied-and-never-shown dialog from hanging startup forever; see
    /// [`CONTROL_PROBE_TIMEOUT`] for why the bound is a minute rather than
    /// kitty's three seconds.
    async fn verify_control(&self) -> Result<()> {
        let probe = osascript("tell application \"Ghostty\" to count of terminals");
        match tokio::time::timeout(CONTROL_PROBE_TIMEOUT, probe).await {
            Ok(Ok(_)) => Ok(()),
            // `{e:#}` flattens the anyhow chain onto one line — the context
            // ("Failed to run osascript") is what `diagnose` classifies on.
            Ok(Err(e)) => {
                let err = format!("{e:#}");
                anyhow::bail!("{}", diagnose(ProbeOutcome::Failed { err: &err }))
            }
            Err(_elapsed) => anyhow::bail!("{}", diagnose(ProbeOutcome::TimedOut)),
        }
    }

    async fn snapshot(&self) -> Result<Vec<Tab>> {
        Ok(parse_snapshot(&osascript(SNAPSHOT_SCRIPT).await?))
    }

    async fn spawn(&self, spec: SpawnSpec) -> Result<SpawnResult> {
        // Both Stacked arrangements are unsupported here (`CAPABILITIES`), so
        // `resolve_spawn_target` only ever yields `NewTab`; reaching either other
        // arm is a policy bug upstream rather than something to approximate.
        let create = match spec.target {
            SpawnTarget::NewTab => "  set t to new tab with configuration cfg\n",
            SpawnTarget::Floating => {
                anyhow::bail!("floating session panes are not supported by the ghostty backend")
            }
            SpawnTarget::SharedStackTab => {
                anyhow::bail!("stacked session tabs are not supported by the ghostty backend")
            }
        };
        // `new tab` returns the tab and `terminal 1 of t` its surface, so one
        // script yields a fully-populated `SpawnResult` — the tab is genuinely
        // the one holding the window, which is what lets the dashboard trust it
        // and skip the resolving snapshot.
        let script = format!(
            "{SEP_PREAMBLE}\
             tell application \"Ghostty\"\n{}{}\
             \x20 set s to terminal 1 of t\n\
             \x20 return (id of s) & sep & (id of t)\n\
             end tell",
            spawn_config_script(&spec),
            create,
        );
        let out = osascript(&script).await?;
        let (window, tab) = out
            .trim()
            .split_once(SEP)
            .context("Failed to parse ids from ghostty spawn output")?;
        let window = WindowId(script_id(window.trim())?.to_string());
        let tab = TabId(script_id(tab.trim())?.to_string());

        // The tab title is an *action*, not a settable property, and it runs
        // after creation rather than riding the configuration. Best-effort: a
        // session whose tab kept its default title is cosmetically wrong, and
        // failing the spawn over it would throw away a live agent.
        if let Some(title) = &spec.title {
            let set_title = format!(
                "tell application \"Ghostty\" to perform action {} on (first terminal whose id is {})",
                applescript_string(&format!("set_tab_title:{title}")),
                applescript_string(window.as_str()),
            );
            if let Err(e) = osascript(&set_title).await {
                tracing::debug!("ghostty: could not set tab title: {e}");
            }
        }

        Ok(SpawnResult {
            window: Some(window),
            tab: Some(tab),
        })
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        let id = script_id(id.as_str())?;
        osascript(&format!(
            "tell application \"Ghostty\" to focus (first terminal whose id is {})",
            applescript_string(id)
        ))
        .await?;
        Ok(())
    }

    async fn focus_tab(&self, id: &TabId) -> Result<()> {
        // Tabs are elements of a *window*, not of the application, so there is
        // no `first tab whose id is …` to filter on — the walk is the lookup.
        //
        // Both commands run, and in this order. `select tab` makes the tab
        // current within its own window but says nothing about which window is
        // in front; `focus` on a terminal is documented to bring its window
        // forward. The caller asked to be *looking at* the tab, which takes
        // both.
        let id = applescript_string(script_id(id.as_str())?);
        osascript(&format!(
            "tell application \"Ghostty\"\n\
             \x20 repeat with w in windows\n\
             \x20   repeat with t in tabs of w\n\
             \x20     if (id of t) is {id} then\n\
             \x20       select tab t\n\
             \x20       focus (focused terminal of t)\n\
             \x20       return\n\
             \x20     end if\n\
             \x20   end repeat\n\
             \x20 end repeat\n\
             end tell"
        ))
        .await?;
        Ok(())
    }

    /// Close the surface `id`.
    ///
    /// Safe to call speculatively, as the restart/kill paths do: surface ids are
    /// UUIDs and so never recycle, and a `whose id is …` that matches nothing
    /// raises a plain AppleScript error the caller ignores rather than closing
    /// something else.
    async fn close_window(&self, id: &WindowId) -> Result<()> {
        let id = script_id(id.as_str())?;
        osascript(&format!(
            "tell application \"Ghostty\" to close (first terminal whose id is {})",
            applescript_string(id)
        ))
        .await?;
        Ok(())
    }

    /// Always an error: Ghostty's dictionary exposes no way to read a window's
    /// screen or scrollback.
    ///
    /// Unreachable from the dashboard, which gates on
    /// [`Capabilities::capture`] — the gate exists because a *failing* capture
    /// is read as evidence that the binding is stale, so this must never be the
    /// path the preview actually takes. Kept honest rather than returning `""`,
    /// which would render as an empty window instead of an explanation.
    async fn capture_text(&self, _id: &WindowId, _max_lines: usize) -> Result<String> {
        anyhow::bail!(
            "ghostty exposes no way to read a window's contents (its AppleScript dictionary has \
             no screen or scrollback property)"
        )
    }

    async fn move_window_to_tab(&self, _id: &WindowId, _to: TabTarget) -> Result<()> {
        anyhow::bail!("moving a surface between tabs is not supported by the ghostty backend")
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_of_every_shape_ghostty_mints_are_accepted() {
        // A surface: `SurfaceView.id.uuidString`.
        assert!(script_id("B3D1F0A2-7C4E-4A19-9F53-2E6C8D0B1A44").is_ok());
        // A tab, and the three window forms: `<kind>-<hex>`, lowercase, no `0x`.
        for id in [
            "tab-600002a1c4d0",
            "window-7ffee3b41a20",
            "tab-group-600002a1c4d0",
            "controller-10f2a3b40",
        ] {
            assert!(script_id(id).is_ok(), "{id}");
        }
    }

    #[test]
    fn ids_that_could_steer_a_script_are_refused() {
        // Neither AppleScript string terminator can appear in a real id, so both
        // are rejected outright rather than escaped — a mis-targeted window op
        // is worse than a refused one.
        assert!(script_id("").is_err());
        assert!(script_id("tab-1\" or true or \"").is_err());
        assert!(script_id("tab-1\\").is_err());
        assert!(script_id("tab 1").is_err());
        assert!(script_id("tab-1\nclose window 1").is_err());
        assert!(script_id(&"a".repeat(129)).is_err());
    }

    #[test]
    fn applescript_strings_close_over_their_own_content() {
        assert_eq!(applescript_string("plain"), "\"plain\"");
        assert_eq!(applescript_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(applescript_string("back\\slash"), "\"back\\\\slash\"");
        // AppleScript has no multi-line literal, so a newline is dropped rather
        // than escaped — it would otherwise end the statement mid-string.
        assert_eq!(applescript_string("a\nb\r\nc"), "\"abc\"");
    }

    /// The separator has to be *bound*, never written into a literal.
    /// AppleScript strings have no `\x` escape at all, and its built-in `tab`
    /// constant is shadowed by Ghostty's `tab` class inside the `tell` block —
    /// both mistakes produce a script that compiles as something else rather
    /// than one that fails loudly, so they get a test.
    #[test]
    fn scripts_bind_the_separator_instead_of_spelling_it() {
        let spec = SpawnSpec {
            cwd: "/home/miao".into(),
            target: SpawnTarget::NewTab,
            command: SpawnCommand::Shell,
            title: None,
            hold: false,
            take_focus: false,
            stack: false,
        };
        let spawn = format!("{SEP_PREAMBLE}{}", spawn_config_script(&spec));
        for script in [SNAPSHOT_SCRIPT, SEP_PREAMBLE, spawn.as_str()] {
            assert!(
                !script.contains("\\x"),
                "no AppleScript \\x escape: {script}"
            );
            assert!(!script.contains(SEP), "no raw separator byte: {script}");
        }
        // And the binding itself is outside the `tell`, where `character` and
        // `id` still mean what AppleScript says they mean.
        let preamble_end = SNAPSHOT_SCRIPT
            .find("tell application")
            .expect("a tell block");
        assert!(SNAPSHOT_SCRIPT[..preamble_end].contains("set sep to character id 31"));
    }

    #[test]
    fn snapshot_parses_tabs_surfaces_and_the_one_focused_tab() {
        let out = "tab-aa\u{1f}0\u{1f}B3D1F0A2-7C4E-4A19-9F53-2E6C8D0B1A44,\u{1f}~/src\n\
                   tab-bb\u{1f}1\u{1f}11111111-2222-3333-4444-555555555555,\
                   66666666-7777-8888-9999-000000000000,\u{1f}miao\n";
        let tabs = parse_snapshot(out);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, TabId("tab-aa".into()));
        assert_eq!(tabs[0].title, "~/src");
        assert!(!tabs[0].is_focused);
        assert_eq!(tabs[0].windows.len(), 1);
        // Only the selected tab of the *front* window is focused, which the
        // script has already resolved to the `1` here.
        assert!(tabs[1].is_focused);
        assert_eq!(tabs[1].windows.len(), 2);
        assert_eq!(
            tabs[1].windows[1],
            WindowId("66666666-7777-8888-9999-000000000000".into())
        );
    }

    #[test]
    fn snapshot_survives_a_title_carrying_the_separator_or_a_bad_row() {
        // The title is the remainder of the line, so a separator inside it can't
        // shift the id fields — the reason it is emitted last.
        let out = format!(
            "tab-aa\u{1f}1\u{1f}B3D1F0A2-7C4E-4A19-9F53-2E6C8D0B1A44,\u{1f}we{SEP}ird\n\
             \n\
             not-enough-fields\n\
             tab-cc\u{1f}0\u{1f},\u{1f}empty\n"
        );
        let tabs = parse_snapshot(&out);
        assert_eq!(tabs.len(), 2, "the malformed row is skipped, not fatal");
        assert_eq!(tabs[0].title, format!("we{SEP}ird"));
        // A tab whose surfaces list is empty is still a tab.
        assert!(tabs[1].windows.is_empty());
    }

    #[test]
    fn snapshot_drops_a_bad_surface_without_dropping_its_tab() {
        let out = "tab-aa\u{1f}0\u{1f}B3D1F0A2-7C4E-4A19-9F53-2E6C8D0B1A44,bad id,\u{1f}t\n";
        let tabs = parse_snapshot(out);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].windows.len(), 1);
    }

    #[test]
    fn an_exec_spawn_is_shell_prefixed_and_quoted() {
        let spec = SpawnSpec {
            cwd: "/home/miao/my code".into(),
            target: SpawnTarget::NewTab,
            command: SpawnCommand::Exec(vec![
                "miao".into(),
                "claude".into(),
                "/home/miao/my code".into(),
                "--settings".into(),
                "{\"a\":1}".into(),
            ]),
            title: None,
            hold: false,
            take_focus: false,
            stack: true,
        };
        let script = spawn_config_script(&spec);
        // `shell:`, not `direct:` — `direct:` splits on whitespace, which would
        // tear both the cwd and the JSON blob into separate argv elements.
        assert!(
            script.contains("shell:miao claude '/home/miao/my code'"),
            "{script}"
        );
        assert!(script.contains(r#"--settings '{\"a\":1}'"#), "{script}");
        assert!(
            script.contains("set initial working directory of cfg to \"/home/miao/my code\""),
            "{script}"
        );
        // `hold` is off, so the surface closes with its command.
        assert!(!script.contains("wait after command"), "{script}");
    }

    #[test]
    fn a_shell_spawn_sets_no_command_and_hold_sets_the_wait() {
        let spec = SpawnSpec {
            cwd: "/home/miao".into(),
            target: SpawnTarget::NewTab,
            command: SpawnCommand::Shell,
            title: None,
            hold: true,
            take_focus: true,
            stack: false,
        };
        let script = spawn_config_script(&spec);
        assert!(!script.contains("set command of cfg"), "{script}");
        assert!(
            script.contains("set wait after command of cfg to true"),
            "{script}"
        );
    }

    #[test]
    fn shell_quoting_survives_a_quote_of_its_own() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("/home/miao/a-b_c.d"), "/home/miao/a-b_c.d");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn every_configuration_failure_names_its_own_fix() {
        // Automation denial and a pre-1.3 Ghostty are the two failures a user
        // will actually hit, and they need opposite fixes — one is a System
        // Settings toggle, the other an upgrade. Neither may be described in the
        // other's terms.
        let denied = diagnose(ProbeOutcome::Failed {
            err: "osascript failed: execution error: Not authorized to send Apple events to \
                  Ghostty. (-1743)",
        });
        assert!(denied.contains("Automation"), "{denied}");
        assert!(!denied.contains("1.3"), "{denied}");

        // A Ghostty with no dictionary can't resolve `terminals`, so it fails at
        // *compile* time with a syntax error rather than at runtime — which is
        // the whole reason the probe names Ghostty's own terminology.
        for err in [
            "osascript failed: 51:60: syntax error: Expected end of line but found identifier. \
             (-2741)",
            "osascript failed: execution error: Ghostty got an error: Can't get terminals. (-1728)",
        ] {
            let too_old = diagnose(ProbeOutcome::Failed { err });
            assert!(too_old.contains("1.3"), "{too_old}");
            assert!(!too_old.contains("Automation"), "{too_old}");
        }

        // A missing osascript is the one failure no Ghostty setting can fix.
        let no_binary = diagnose(ProbeOutcome::Failed {
            err: "Failed to run osascript: No such file or directory (os error 2)",
        });
        assert!(no_binary.contains("/usr/bin/osascript"), "{no_binary}");
        assert!(!no_binary.contains("Automation"), "{no_binary}");

        // A hang is the *consent dialog*, so the timeout message must point at
        // it rather than suggest retrying.
        let timed_out = diagnose(ProbeOutcome::TimedOut);
        assert!(timed_out.contains("Automation"), "{timed_out}");
    }

    /// Ghostty is macOS-only as far as this backend is concerned: the Linux
    /// build ships no AppleScript and no equivalent, so detection must not claim
    /// a surface it cannot drive.
    #[test]
    fn from_env_is_macos_only() {
        if !cfg!(target_os = "macos") {
            // Safe to assert unconditionally: on a non-macOS host the gate short
            // -circuits before any env read, so no `TERM_PROGRAM` value matters.
            assert!(GhosttyTerminal::from_env().is_none());
        }
    }

    /// Drive a real Ghostty. Needs macOS, Ghostty ≥ 1.3 running, and captain-miao
    /// granted Automation permission for it — so it is `#[ignore]`d, like the
    /// live tmux and ssh tests.
    ///
    /// ```sh
    /// cargo test -p captain-miao -- --ignored drives_a_real_ghostty
    /// ```
    #[tokio::test]
    #[ignore = "needs a live Ghostty >= 1.3 on macOS with Automation permission granted"]
    async fn drives_a_real_ghostty() {
        let term = GhosttyTerminal::new();
        term.verify_control().await.expect("control channel");

        let before = term.snapshot().await.expect("snapshot");
        let spawned = term
            .spawn(SpawnSpec {
                cwd: std::env::var("HOME").expect("HOME"),
                target: SpawnTarget::NewTab,
                command: SpawnCommand::Exec(vec!["sleep".into(), "30".into()]),
                title: Some("miao test".into()),
                hold: false,
                take_focus: false,
                stack: true,
            })
            .await
            .expect("spawn");
        let window = spawned.window.expect("a spawn always reports its surface");
        let tab = spawned.tab.expect("a spawn always reports its tab");

        // The reported tab must be the one actually holding the reported
        // surface: the dashboard trusts it and skips the resolving snapshot.
        let after = term.snapshot().await.expect("snapshot");
        assert_eq!(after.len(), before.len() + 1);
        let holder = after
            .iter()
            .find(|t| t.windows.contains(&window))
            .expect("the new surface is in the snapshot");
        assert_eq!(holder.id, tab);
        assert_eq!(holder.title, "miao test", "the title action took effect");

        term.focus_window(&window).await.expect("focus");
        term.close_window(&window).await.expect("close");
        // Closing an id that is already gone must be harmless, not a panic —
        // the restart/kill paths close speculatively.
        let _ = term.close_window(&window).await;
    }
}
