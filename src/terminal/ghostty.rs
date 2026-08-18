//! Ghostty backend (macOS only): wraps Ghostty's AppleScript dictionary.
//!
//! Transport, quoting and the probe shape are [`super::applescript`]'s; what is
//! here is what is Ghostty's. Requires Ghostty ≥ 1.3, which is where the
//! scripting dictionary landed; older builds expose nothing at all and fail
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
//! What is *verified*. Everything covered by a test here is pure (script
//! building, snapshot parsing, id validation, shell quoting, the startup
//! diagnosis); driving a real Ghostty needs a Mac with a GUI session and a
//! hand-clicked Automation grant, which no CI can supply, so nothing below runs
//! in the suite. The **spawn** and **own-surface** paths have been driven by
//! hand against Ghostty 1.3.1, which contradicted the dictionary reading three
//! times — the `command` property does not parse the `shell:` prefix the config
//! file accepts ([`spawn_config_script`]), `new tab` must name its window or it
//! creates the tab and *then* fails the event ([`CREATE_TAB_SCRIPT`]), and there
//! is no `tty` on a `terminal` to find yourself by
//! ([`GhosttyTerminal::resolve_own_surface`]). All three are documented where
//! they live.
//!
//! Backend properties this module encodes, read off the shipped `Ghostty.sdef`
//! and the Swift behind it — so, per the above, what the dictionary says rather
//! than what a Ghostty was seen doing:
//! - **Ids are opaque strings of three different shapes.** A terminal id is a
//!   UUID (`SurfaceView.id.uuidString`); a tab id is `tab-<hex>`; a window id is
//!   `window-<hex>` / `tab-group-<hex>` / `controller-<hex>`. So the digit-only
//!   [`super::validate_id`] every other backend uses does not apply here, and
//!   [`script_id`] validates the union instead.
//! - **Surface ids never recycle**, being UUIDs, which is what makes the
//!   speculative `close_window` the restart/kill paths rely on safe.
//! - **A process cannot ask which surface it is in, so it says.** There is no
//!   `KITTY_WINDOW_ID` analog in the environment and no `tty` on the `terminal`
//!   class to match `ttyname(0)` against (both measured). The one property a
//!   process can set from inside is its title, so `current_window` writes a
//!   nonce one and looks for the surface wearing it —
//!   [`GhosttyTerminal::resolve_own_surface`] carries the rest, including why
//!   nothing has to put the title back.
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

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::applescript::{
    CONTROL_PROBE_TIMEOUT, ProbeOutcome, SEP, SEP_PREAMBLE, applescript_string, osascript,
    shell_quote,
};
use super::{
    Capabilities, SpawnCommand, SpawnResult, SpawnSpec, SpawnTarget, Tab, TabId, TabTarget,
    Terminal, WindowId,
};

/// How long [`GhosttyTerminal::resolve_own_surface`] waits for the title it just
/// wrote to reach Ghostty's model, as `(tries, seconds between)` — the two halves
/// of one AppleScript `repeat`, so the whole wait costs a single `osascript`.
///
/// Measured at well under one round on 1.3.1 (the escape is parsed by the same
/// process that answers the query, and there is nothing between them), so this
/// bound exists for the case where the title *never* lands — the one where a
/// tight retry would spin and a generous one would hold up startup for nothing.
const OWN_SURFACE_SETTLE: (u32, f32) = (10, 0.05);

pub struct GhosttyTerminal {
    /// The surface the dashboard itself runs in, resolved lazily on first use
    /// and then cached. `Some(None)` is a *settled* failure (no tty, or Ghostty
    /// named no surface) and is not retried: the answer can't change for the
    /// life of the process, and the caller (`write_dashboard_pid_and_window`)
    /// treats it as best-effort.
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

    /// The bare backend, once `from_env` has vouched for the environment.
    fn new() -> Self {
        Self {
            own_surface: OnceLock::new(),
        }
    }

    /// The dashboard's own surface id, resolved once — by **saying** which
    /// surface it is in, because nothing lets it ask.
    ///
    /// Ghostty tells a process nothing about its surface: no `KITTY_WINDOW_ID`
    /// analog in the environment (1.3.1 exports `GHOSTTY_RESOURCES_DIR`,
    /// `GHOSTTY_BIN_DIR` and `GHOSTTY_SHELL_FEATURES`, and that is all), and no
    /// `tty` on the `terminal` class to match `ttyname(0)` against — the class
    /// carries `id`, `name` and `working directory`, so asking for `tty of s`
    /// fails to compile the script at all (-1700). Both measured on 1.3.1.
    ///
    /// So the one property a process *can* set from inside is the one used:
    /// write a nonce title to the tty, then look for the surface wearing it.
    /// `working directory` was the other candidate and is not an identifier —
    /// it is empty without shell integration, and shared by every surface in the
    /// same project when there is.
    ///
    /// Nothing has to put the title back. The run loop labels this tab `miao`
    /// (with the attention count) on its first frame, moments later and every
    /// time the count moves, so the nonce is overwritten by the mechanism that
    /// owns the title anyway — leaving one frame of a stray title in the worst
    /// case, and none in the ordinary one.
    ///
    /// Blocking on purpose: this is one `osascript` at most per process, and
    /// [`Terminal::current_window`] is sync on every backend because every other
    /// one answers it from the env. Making the whole seam async to accommodate
    /// the single backend that can't would be the tail wagging the dog; paying
    /// one blocking subprocess, once, at dashboard startup, is not.
    fn resolve_own_surface(&self) -> Option<WindowId> {
        let tty = own_tty()?;
        let nonce = surface_nonce();
        // Straight to the tty rather than to stdout, which is where the trait's
        // own `set_own_tab_title` puts it: this runs before the render backend
        // takes stdout over, but it is the *terminal* that must see the escape,
        // and a dashboard started with its output redirected still has one.
        {
            use std::io::Write;
            let mut dev = std::fs::OpenOptions::new().write(true).open(&tty).ok()?;
            dev.write_all(format!("\x1b]2;{nonce}\x07").as_bytes())
                .ok()?;
            dev.flush().ok()?;
        }
        let script = own_surface_script(&nonce);
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
                "ghostty: could not resolve own surface from the title on {tty}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return None;
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        script_id(&id).ok()?;
        Some(WindowId(id))
    }
}

/// The lookup half of [`GhosttyTerminal::resolve_own_surface`]: the surface
/// wearing `nonce`, or `""`.
///
/// The wait is a `repeat` *inside* the script rather than a retry around it, so
/// the whole settle costs one `osascript` process and one Apple-event connection
/// instead of [`OWN_SURFACE_SETTLE`]`.0` of each. `delay` is Standard Additions
/// terminology, which resolves inside a `tell` block for any term the
/// application's own dictionary doesn't claim — and Ghostty's doesn't.
fn own_surface_script(nonce: &str) -> String {
    let (tries, delay) = OWN_SURFACE_SETTLE;
    format!(
        "tell application \"Ghostty\"\n\
         \x20 repeat {tries} times\n\
         \x20   repeat with s in terminals\n\
         \x20     if (name of s) is {} then return (id of s)\n\
         \x20   end repeat\n\
         \x20   delay {delay}\n\
         \x20 end repeat\n\
         end tell\n\
         return \"\"",
        applescript_string(nonce)
    )
}

/// A title no other surface can be wearing, for the lookup above.
///
/// The pid alone would do among *live* surfaces, but the thing being matched is
/// a title, and a title outlives the process that wrote it: a surface left
/// showing an older dashboard's nonce, under a pid the OS has since recycled,
/// would answer for a window this process is not in. The clock settles that.
///
/// Shaped like what replaces it a moment later (`miao`, `miao (2)`) so the one
/// frame it may be visible for reads as this program rather than as garbage.
fn surface_nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("miao [{}.{nanos}]", std::process::id())
}

/// The controlling terminal's device path (`/dev/ttys004`), which the surface
/// lookup above writes its nonce title to. Tries stdin, stdout, then stderr: the
/// dashboard's stdin is normally the tty, but a `miao` invoked with stdin
/// redirected still has one on another descriptor.
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

// ---- startup control check ----

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
    graphics: false,
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
/// `command` is a shell-quoted argv carrying **no prefix**. Ghostty's *config
/// file* accepts `shell:` and `direct:` there, but the scripting property is
/// plain text that reaches the shell whole: a `shell:` prefix is not stripped,
/// it becomes part of the command, and Ghostty dies trying to exec a file named
/// `shell:/bin/sh`. Measured on 1.3.1.
///
/// Unprefixed is what we would want even if the property did learn to parse
/// them. A command carrying arguments takes the documented `/bin/sh -c` path,
/// which is the one captain-miao needs — its argvs routinely hold elements that
/// contain spaces (a cwd with one, and the whole `/bin/sh -c '<script>'` the
/// remote attach wrapper is), and `direct:`'s whitespace split would silently
/// run something else. The quoting is what makes that round-trip lossless.
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
            applescript_string(&joined)
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

/// The creating half of a spawn: make the surface `cfg` describes and leave its
/// tab in `t`.
///
/// **The window is named, and that is not a nicety.** Ghostty's handler for the
/// parameterless `new tab` creates the tab and *then* fails the Apple event with
/// `errAEEventNotHandled` (-1708) — measured on 1.3.1 — so the spawn reports
/// failure while leaving a live agent in a surface captain-miao never learned
/// the id of, and the user sees only the error. `in front window` asks for the
/// same tab the bare form means, spelled the way the handler answers.
///
/// `front window` is `missing value` when Ghostty holds no window, which is an
/// ordinary state rather than a race — Ghostty stays running with every window
/// closed, and captain-miao's own surface may be the one the user just closed.
/// That case takes `new window`, the one creation command that needs nothing to
/// already exist; it answers with a *window*, so the tab comes back off
/// `selected tab` — the tab it just made is the selected one.
const CREATE_TAB_SCRIPT: &str = concat!(
    "  set w to front window\n",
    "  if w is missing value then\n",
    "    set t to selected tab of (new window with configuration cfg)\n",
    "  else\n",
    "    set t to new tab in w with configuration cfg\n",
    "  end if\n",
);

/// The whole of what a [`spawn`](Terminal::spawn) sends: configure, create, and
/// read the two ids back.
///
/// One script, because both arms of [`CREATE_TAB_SCRIPT`] leave the new tab in
/// `t` and `terminal 1 of t` is its surface — so a single round-trip yields a
/// fully-populated `SpawnResult` whose tab is genuinely the one holding the
/// window, which is what lets the dashboard trust it and skip the resolving
/// snapshot.
///
/// Pure, and separated from the call for that reason: the script is the part
/// that can be wrong, and it is the part no CI can run.
fn spawn_script(spec: &SpawnSpec) -> Result<String> {
    // Both Stacked arrangements are unsupported here (`CAPABILITIES`), so
    // `resolve_spawn_target` only ever yields `NewTab`; reaching either other
    // arm is a policy bug upstream rather than something to approximate.
    let create = match spec.target {
        SpawnTarget::NewTab => CREATE_TAB_SCRIPT,
        SpawnTarget::Floating => {
            anyhow::bail!("floating session panes are not supported by the ghostty backend")
        }
        SpawnTarget::SharedStackTab => {
            anyhow::bail!("stacked session tabs are not supported by the ghostty backend")
        }
    };
    Ok(format!(
        "{SEP_PREAMBLE}\
         tell application \"Ghostty\"\n{}{create}\
         \x20 set s to terminal 1 of t\n\
         \x20 return (id of s) & sep & (id of t)\n\
         end tell",
        spawn_config_script(spec),
    ))
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
        let out = osascript(&spawn_script(&spec)?).await?;
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
    fn an_exec_spawn_is_quoted_and_carries_no_prefix() {
        let spec = SpawnSpec {
            cwd: "/home/miao/my code".into(),
            target: SpawnTarget::NewTab,
            command: SpawnCommand::Exec(vec![
                "miao".into(),
                "launch".into(),
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
        // Quoted, so the `/bin/sh -c` Ghostty runs a multi-argument command
        // through cannot tear the cwd or the JSON blob into separate elements…
        assert!(
            script.contains("set command of cfg to \"miao launch claude '/home/miao/my code'"),
            "{script}"
        );
        assert!(script.contains(r#"--settings '{\"a\":1}'"#), "{script}");
        // …and unprefixed, because the scripting property takes plain text: a
        // `shell:` would be exec'd as part of the command rather than stripped.
        assert!(!script.contains("shell:"), "{script}");
        assert!(!script.contains("direct:"), "{script}");
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
    fn a_spawn_never_asks_for_a_tab_without_saying_where() {
        // A parameterless `new tab` creates the tab and *then* fails the event,
        // so a spawn that used one would report failure over a live agent. Every
        // `new tab` here names its window, and the no-window arm — where there
        // is no window to name — reaches for `new window` rather than skipping
        // the parameter.
        for line in CREATE_TAB_SCRIPT.lines() {
            if line.contains("new tab") {
                assert!(line.contains(" in w "), "{line}");
            }
        }
        assert!(
            CREATE_TAB_SCRIPT.contains("if w is missing value then"),
            "{CREATE_TAB_SCRIPT}"
        );
        assert!(
            CREATE_TAB_SCRIPT.contains("new window with configuration cfg"),
            "{CREATE_TAB_SCRIPT}"
        );
        // Both arms leave the tab in `t`, which is what `spawn` reads back.
        assert_eq!(CREATE_TAB_SCRIPT.matches("set t to").count(), 2);
    }

    #[test]
    fn a_spawn_script_configures_creates_and_reads_both_ids_back() {
        let spec = |target| SpawnSpec {
            cwd: "/home/miao".into(),
            target,
            command: SpawnCommand::Shell,
            title: None,
            hold: false,
            take_focus: true,
            stack: false,
        };
        let script = spawn_script(&spec(SpawnTarget::NewTab)).unwrap();
        // The order is the contract: `cfg` has to be complete before the create
        // consumes it, and `t` has to exist before the surface is read off it.
        let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{script}"));
        assert!(at("set cfg to new surface configuration") < at("new tab in w"));
        assert!(at("new tab in w") < at("set s to terminal 1 of t"));
        assert!(
            script.contains("return (id of s) & sep & (id of t)"),
            "{script}"
        );

        // The two arrangements `CAPABILITIES` denies are refused rather than
        // approximated, so a policy bug upstream is loud.
        for target in [SpawnTarget::Floating, SpawnTarget::SharedStackTab] {
            assert!(spawn_script(&spec(target)).is_err());
        }
    }

    #[test]
    fn the_own_surface_lookup_waits_inside_one_script() {
        let script = own_surface_script("miao [1.2]");
        // The nonce is matched on `name`, the only property a process can set
        // from inside — `tty` isn't on the class at all.
        assert!(
            script.contains("if (name of s) is \"miao [1.2]\" then return (id of s)"),
            "{script}"
        );
        // The settle is a `repeat`/`delay` pair in the script, so waiting costs
        // one osascript rather than one per try.
        let (tries, delay) = OWN_SURFACE_SETTLE;
        assert!(
            script.contains(&format!("repeat {tries} times")),
            "{script}"
        );
        assert!(script.contains(&format!("delay {delay}")), "{script}");
        // Finding nothing is an empty answer, not a failure: the caller has to
        // tell "not in a Ghostty surface" from "Ghostty refused the request".
        assert!(script.ends_with("return \"\""), "{script}");
    }

    #[test]
    fn a_surface_nonce_names_this_process_and_needs_no_escaping() {
        let nonce = surface_nonce();
        assert!(nonce.contains(&std::process::id().to_string()), "{nonce}");
        // Shaped like the `miao (2)` label that replaces it, so the one frame it
        // can be visible for reads as this program rather than as garbage.
        assert!(nonce.starts_with("miao "), "{nonce}");
        // It reaches the script through `applescript_string`. Nothing in it may
        // need escaping — the escaped literal has to still spell the title that
        // went out to the tty verbatim, or the lookup asks for a name no surface
        // wears.
        assert_eq!(applescript_string(&nonce), format!("\"{nonce}\""));
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
}
