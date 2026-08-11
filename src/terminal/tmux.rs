//! tmux backend: wraps the `tmux` CLI (tmux ≥ 3.2).
//!
//! Transport is one `tmux -S <socket>` subprocess per call, pinned to the socket
//! and session captured from `TMUX`/`TMUX_PANE` at startup — the analog of the
//! zellij backend's `ZELLIJ_SESSION_NAME` pinning. Window and pane ids are tmux's
//! own `@N`/`%N`, which are opaque strings to captain-miao and serialize as-is.
//!
//! Vocabulary: a captain-miao **tab** is a tmux *window* (`@N`), a captain-miao
//! **window** (one session's pane) is a tmux *pane* (`%N`). tmux's own "session"
//! has no captain-miao analog; like zellij, the backend pins the one it started
//! inside and scopes `snapshot` and spawns to it.
//!
//! Backend quirks this module encodes (probe-verified on 3.7b):
//! - **`list-panes -s` is cheap** — ~4ms at 28 panes, the same order as any other
//!   command, because formats render from the server's in-memory state. So there
//!   is no zellij-style hot-path discipline to keep here: `snapshot` is an
//!   ordinary call.
//! - **`new-window -P -F` prints both ids atomically**, with `-d`, `-c`, `-n` and
//!   a command, so *every* spawn returns a fully-populated [`SpawnResult`] and
//!   seeds the dashboard's window→tab cache. There is no pane-id recovery path.
//! - **Chained (`\;`) option-sets target the session's *current* pane/window, not
//!   the one `new-window -d` just created** — so `hold` cannot ride along on the
//!   spawn invocation and needs a second call with an explicit `-t %N`. Getting
//!   this wrong silently sets the option on whatever pane the user was looking at.
//! - **Ids reset when the server restarts on the same socket path** (`%1`/`@1`
//!   again), which is why the terminal identity carries the server pid — see
//!   `cm_core::terminal::tmux_identity_parts`.
//! - **`join-pane`/`break-pane` drag the client unless `-d` is passed**
//!   (measured: the client followed the pane to the target window), so both
//!   `move_window_to_tab` arms pass it.
//! - **Pane commands inherit the tmux *server*'s environment**, not the caller's —
//!   a server may predate the dashboard's shell by days, and a variable exported
//!   for the `tmux` client process does not reach the pane. Same failure class as
//!   zellij, same fix: wrap an `Exec` argv in `/usr/bin/env PATH=<dashboard PATH> …`.
//! - **A pane closes when its command exits**; `hold: true` maps to the
//!   `remain-on-exit` pane option. A held dead pane keeps the command's output and
//!   draws a `Pane is dead (status N, …)` notice on its **last row** — so the
//!   output stays where it was, above a run of blank rows, and a short preview of
//!   such a pane can show mostly padding. The `hold` case that matters most,
//!   `FailedToStart`, is unaffected: the launcher *blocks* rather than exiting, so
//!   its pane is alive and its error is the last thing on screen.
//! - **`capture-pane` returns the whole visible screen**, blank rows included, so
//!   a capture is trimmed of trailing blanks before being tailed — otherwise every
//!   pane that hasn't filled its screen previews as empty lines.
//! - **`-n` titles need renaming pinned off.** An explicit `-n` already disables
//!   `automatic-rename`, but `allow-rename` lets the *application* retitle the
//!   window with an OSC escape — which agents emit — silently invalidating the
//!   work-tab map's title check. Both are set off on windows captain-miao creates.
//! - **Stacked is not implementable here** (`window_stacking: false,
//!   floating_sessions: false`): `display-popup` is client-bound and transient, and
//!   the zoom emulation costs a real pty resize per switch (measured 80x12 → 80x24)
//!   while a background split *unzooms* the window. See the design doc §6.

use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

use super::{
    Capabilities, SpawnCommand, SpawnResult, SpawnSpec, SpawnTarget, Tab, TabId, TabTarget,
    Terminal, WindowId, tail_lines,
};

/// The format `snapshot` requests, one line per pane. The free-text field
/// (`window_name`) goes **last** so a name containing the separator can't shift
/// the id fields; everything before it is a fixed, validated shape.
const SNAPSHOT_FORMAT: &str = "#{window_id}\t#{pane_id}\t#{window_active}\t#{window_name}";

/// The format every spawn requests: the new pane and the window holding it, both
/// printed by the one atomic `new-window`.
const SPAWN_FORMAT: &str = "#{pane_id} #{window_id}";

pub struct TmuxTerminal {
    /// Socket every call is pinned to (`tmux -S <socket>`), from `TMUX`.
    socket: String,
    /// The server pid from `TMUX`, carried only to build the instance identity
    /// (see `tmux_identity_parts` for why the pid is part of the key).
    server_pid: String,
    /// The tmux session this backend scopes itself to, captured at startup.
    session: String,
    /// The dashboard's own pane (`TMUX_PANE`). Reported as `current_window`; the
    /// backend never needs it to restore focus (tmux creates without focusing
    /// natively, via `-d`).
    own_pane: Option<WindowId>,
    /// What the dashboard's window was called before the first
    /// `set_own_tab_title`, plus whether `automatic-rename` was on — restored on
    /// the way out. `None` until we have actually renamed it, so
    /// `restore_own_tab_title` knows there is nothing to undo.
    saved_window_name: Mutex<Option<(String, bool)>>,
}

impl TmuxTerminal {
    /// Construct from the environment. `None` when not inside a tmux pane, or
    /// when `TMUX` doesn't parse into a socket + server pid (a pane we can't
    /// namespace to a server is one we refuse to drive — see
    /// [`cm_core::terminal::tmux_identity`]).
    pub fn from_env() -> Option<Self> {
        let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let tmux = clean(std::env::var("TMUX").ok())?;
        let (socket, server_pid, session) = parse_tmux_env(&tmux)?;
        let own_pane = clean(std::env::var("TMUX_PANE").ok())
            .filter(|s| pane_id(s).is_ok())
            .map(WindowId);
        Some(Self {
            socket,
            server_pid,
            session,
            own_pane,
            saved_window_name: Mutex::new(None),
        })
    }

    async fn tmux_cmd(&self, args: &[&str]) -> Result<String> {
        let started = std::time::Instant::now();
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .await
            .context("Failed to run tmux")?;
        // Per-call timing in the debug log, mirroring the zellij backend: this is
        // how a hot path that regressed onto an expensive command gets spotted.
        // On tmux nothing measured worse than ~4ms, `list-panes` included.
        tracing::debug!("tmux {} took {:?}", args.join(" "), started.elapsed());

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "tmux {} failed: {}",
                args.first().unwrap_or(&""),
                stderr.trim()
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Pin a window's title against later renaming. An explicit `-n` already
    /// turns `automatic-rename` off, but `allow-rename` lets the application
    /// inside the pane retitle the window with an OSC escape — which agents emit —
    /// and the work-tab map validates on that title. Best-effort: the window and
    /// its command already exist, so a failed pin must not fail the spawn.
    async fn pin_title(&self, tab: &TabId) {
        let Ok(id) = window_id(tab.as_str()) else {
            return;
        };
        for opt in ["automatic-rename", "allow-rename"] {
            if let Err(e) = self
                .tmux_cmd(&["set-option", "-w", "-t", id, opt, "off"])
                .await
            {
                tracing::debug!("pinning {opt} on {id} failed: {e}");
            }
        }
    }
}

/// Split a raw `TMUX` value (`<socket_path>,<server_pid>,<session_id>`) into its
/// three parts. Splits from the **right**: a socket path may contain a comma,
/// the two trailing fields cannot. Rejects a non-numeric server pid, so a value
/// we don't understand fails closed instead of minting a bogus identity.
fn parse_tmux_env(tmux: &str) -> Option<(String, String, String)> {
    let mut parts = tmux.rsplitn(3, ',');
    let session = parts.next()?.trim();
    let server_pid = parts.next()?.trim();
    let socket = parts.next()?.trim();
    if socket.is_empty() || server_pid.is_empty() || !server_pid.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((
        socket.to_string(),
        server_pid.to_string(),
        session.to_string(),
    ))
}

/// Validate a tmux id of the form `<sigil><digits>`, failing closed. The shared
/// [`super::validate_id`] doesn't fit (tmux ids carry a sigil), but the rationale
/// is the same as `kitty::match_id`/`zellij::digits_id`: the only untrusted source
/// is captain-miao's own state files, and anything unexpected is refused rather
/// than mis-targeted — these strings become `-t` arguments.
fn sigil_id<'a>(id: &'a str, sigil: char, what: &str) -> Result<&'a str> {
    let digits = id.strip_prefix(sigil).filter(|d| !d.is_empty());
    match digits {
        Some(d) if d.bytes().all(|b| b.is_ascii_digit()) => Ok(id),
        _ => anyhow::bail!("refusing malformed tmux {what} id: {id:?} (expected {sigil}<digits>)"),
    }
}

/// Validate a pane id (`%N`) — captain-miao's [`WindowId`] on this backend.
fn pane_id(id: &str) -> Result<&str> {
    sigil_id(id, '%', "pane")
}

/// Validate a window id (`@N`) — captain-miao's [`TabId`] on this backend.
fn window_id(id: &str) -> Result<&str> {
    sigil_id(id, '@', "window")
}

/// Parse the `#{pane_id} #{window_id}` pair a spawn prints. Both ids are
/// validated, so a garbled line is an error rather than a bad `-t` target later.
fn parse_spawn_ids(stdout: &str) -> Result<(WindowId, TabId)> {
    let line = stdout.trim();
    let (pane, window) = line
        .split_once(' ')
        .with_context(|| format!("unexpected tmux new-window output: {line:?}"))?;
    Ok((
        WindowId(pane_id(pane.trim())?.to_string()),
        TabId(window_id(window.trim())?.to_string()),
    ))
}

/// Assemble the snapshot from `list-panes -s -F` output ([`SNAPSHOT_FORMAT`]).
///
/// Tab order follows first appearance, and a line whose ids don't validate is
/// skipped rather than failing the whole snapshot — one malformed row must not
/// blind the dashboard to every other window. `splitn(4, …)` keeps a
/// separator-bearing window name intact in the final field.
fn parse_snapshot(stdout: &str) -> Vec<Tab> {
    let mut tabs: Vec<Tab> = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let mut fields = line.splitn(4, '\t');
        let (Some(win), Some(pane), Some(active)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let name = fields.next().unwrap_or("");
        let (Ok(win), Ok(pane)) = (window_id(win.trim()), pane_id(pane.trim())) else {
            continue;
        };
        let id = TabId(win.to_string());
        let tab = match tabs.iter_mut().find(|t| t.id == id) {
            Some(t) => t,
            None => {
                tabs.push(Tab {
                    id,
                    title: name.to_string(),
                    is_focused: active.trim() == "1",
                    windows: Vec::new(),
                });
                tabs.last_mut().expect("just pushed")
            }
        };
        tab.windows.push(WindowId(pane.to_string()));
    }
    tabs
}

/// The `new-window` argv for a spawn. `-P -F` prints the new pane and window in
/// one atomic create; `-d` is the native "don't take focus" (no zellij-style
/// focus snap-back is ever needed).
///
/// The command is passed as a **single** trailing argument because tmux parses it
/// with its own command-string parser; splitting an argv across multiple
/// arguments would have tmux re-join and re-split it under different rules.
fn new_window_args(
    session: &str,
    cwd: &str,
    title: Option<&str>,
    take_focus: bool,
    command: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "new-window".into(),
        "-P".into(),
        "-F".into(),
        SPAWN_FORMAT.into(),
        // Scope the new window to the session this backend is pinned to, so a
        // server hosting several sessions still puts it where the dashboard is.
        "-t".into(),
        format!("{session}:"),
        "-c".into(),
        cwd.into(),
    ];
    if !take_focus {
        args.push("-d".into());
    }
    if let Some(title) = title {
        args.push("-n".into());
        args.push(title.into());
    }
    if let Some(cmd) = command {
        args.push(cmd.into());
    }
    args
}

/// Quote an argv into the single command string tmux's parser expects. tmux
/// applies shell-like quoting to the command it is handed, so every element is
/// wrapped in single quotes with embedded `'` escaped the POSIX way
/// (`'\''`) — an unquoted join would let a path with a space or a quote in it
/// split into extra words.
fn quote_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Drop trailing blank lines from a capture.
///
/// `capture-pane` always returns the **entire visible screen**, whose unused
/// rows below the cursor are empty. Tailing that raw would answer a request for
/// "the last 5 lines" with 5 blank ones for any pane that hasn't filled its
/// screen — every freshly-started session, and every idle one — so the blank
/// rows come off first and the tail is taken over real content.
fn trim_trailing_blank_lines(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r', ' ', '\t'])
}

/// Wrap an exec argv so the pane gets the dashboard's `PATH`: tmux pane commands
/// inherit the tmux *server*'s environment (whatever shell started the server,
/// possibly days ago), not the caller's, so a bare `miao …` argv may not resolve.
/// `/usr/bin/env` is POSIX-placed and needs no PATH itself. Verbatim the zellij
/// fix, for the identical reason.
fn wrap_env(argv: &[String], path: Option<&str>) -> Vec<String> {
    let Some(path) = path else {
        return argv.to_vec();
    };
    let mut wrapped = Vec::with_capacity(argv.len() + 2);
    wrapped.push("/usr/bin/env".to_string());
    wrapped.push(format!("PATH={path}"));
    wrapped.extend(argv.iter().cloned());
    wrapped
}

/// The tmux backend's fixed capabilities. `move_to_tab` is **true** — the first
/// multiplexer backend where it is, since `break-pane`/`join-pane` are real CLI
/// commands (contrast zellij, whose `BreakPane` is keybind-only). Neither Stacked
/// arrangement exists here: no floating panes that survive a client switch, and
/// no non-tiling layout (design doc §6), so `resolve_spawn_target` falls back to
/// a tab per session. Exported so tests assert against the real value rather than
/// a hand-built literal that could silently diverge when a field is added.
pub(crate) const CAPABILITIES: Capabilities = Capabilities {
    move_to_tab: true,
    window_stacking: false,
    floating_sessions: false,
};

#[async_trait]
impl Terminal for TmuxTerminal {
    fn current_window(&self) -> Option<WindowId> {
        self.own_pane.clone()
    }

    fn identity(&self) -> Option<String> {
        Some(cm_core::terminal::tmux_identity_parts(
            &self.socket,
            &self.server_pid,
        ))
    }

    async fn snapshot(&self) -> Result<Vec<Tab>> {
        // One subprocess, and a cheap one: formats render from the server's
        // in-memory state (~4ms at 28 panes), so unlike zellij there is no
        // per-pane cliff to keep off hot paths.
        let stdout = self
            .tmux_cmd(&[
                "list-panes",
                "-s",
                "-t",
                &self.session,
                "-F",
                SNAPSHOT_FORMAT,
            ])
            .await?;
        Ok(parse_snapshot(&stdout))
    }

    async fn spawn(&self, spec: SpawnSpec) -> Result<SpawnResult> {
        let exec: Option<String> = match &spec.command {
            SpawnCommand::Shell => None,
            SpawnCommand::Exec(argv) => Some(quote_argv(&wrap_env(
                argv,
                std::env::var("PATH").ok().as_deref(),
            ))),
        };

        match &spec.target {
            SpawnTarget::NewTab => {}
            // Unreachable by policy: `resolve_spawn_target` yields these only for
            // a `window_stacking`/`floating_sessions` backend, and tmux reports
            // neither (see CAPABILITIES) — its Stacked answer is NewTab.
            SpawnTarget::Floating | SpawnTarget::SharedStackTab => anyhow::bail!(
                "the tmux backend hosts every session in its own window; \
                 stacked/floating spawns are not used here"
            ),
        }

        let args = new_window_args(
            &self.session,
            &spec.cwd,
            spec.title.as_deref(),
            spec.take_focus,
            exec.as_deref(),
        );
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let stdout = self.tmux_cmd(&arg_refs).await?;
        // Past this point the window AND its command are already running, so the
        // spawn must not become an Err — the caller would report failure and
        // later spawn a duplicate. Only the id parse can still fail (a garbled
        // pair is worse than none: it would become a `-t` target).
        let (window, tab) = parse_spawn_ids(&stdout)?;

        // Both follow-ups are best-effort for that reason.
        if spec.title.is_some() {
            self.pin_title(&tab).await;
        }
        if spec.hold {
            // A second call with an explicit `-t`: chaining `set-option -p` onto
            // the spawn with `\;` targets the session's *current* pane, not the
            // one `-d` just created (probe-verified), which would set the option
            // on whatever the user was looking at. The race this opens — the
            // command exiting before the option lands, closing the pane — is
            // unreachable for the callers that ask for `hold`: the launcher
            // blocks in `hold_failed_launch`, and an attach runs under a wrapper
            // shell that traps its exit.
            if let Err(e) = self
                .tmux_cmd(&[
                    "set-option",
                    "-p",
                    "-t",
                    window.as_str(),
                    "remain-on-exit",
                    "on",
                ])
                .await
            {
                tracing::debug!("setting remain-on-exit on {window} failed: {e}");
            }
        }

        Ok(SpawnResult {
            window: Some(window),
            tab: Some(tab),
        })
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        // One invocation, two chained commands: tmux resolves a pane id to its
        // containing window, so the pair switches the window and focuses the
        // pane. (Chaining is safe here precisely because both commands carry an
        // explicit `-t` — see the `hold` note in `spawn`.)
        let pane = pane_id(id.as_str())?;
        self.tmux_cmd(&["select-window", "-t", pane, ";", "select-pane", "-t", pane])
            .await?;
        Ok(())
    }

    async fn focus_tab(&self, id: &TabId) -> Result<()> {
        self.tmux_cmd(&["select-window", "-t", window_id(id.as_str())?])
            .await?;
        Ok(())
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        // Speculative closes are fine: pane ids never recycle within a server's
        // lifetime, and a dead id is a plain `can't find pane: %N` error (exit 1,
        // no prompt) that the speculative callers already ignore.
        self.tmux_cmd(&["kill-pane", "-t", pane_id(id.as_str())?])
            .await?;
        Ok(())
    }

    async fn capture_text(&self, id: &WindowId, max_lines: usize) -> Result<String> {
        // `-p` to stdout, `-e` to keep SGR styling (matching kitty's `get-text`
        // and zellij's `--ansi`), `-J` to rejoin wrapped lines. `-S -N` reaches N
        // lines *into history*, so the result is N plus the whole visible screen
        // — still far less than fetching the entire scrollback, which is what
        // both other backends must do. The screen's unused rows are blank, so
        // they come off before `tail_lines` trims to exactly `max_lines`;
        // otherwise a half-empty pane previews as blank lines.
        let start = format!("-{max_lines}");
        let raw = self
            .tmux_cmd(&[
                "capture-pane",
                "-p",
                "-e",
                "-J",
                "-t",
                pane_id(id.as_str())?,
                "-S",
                &start,
            ])
            .await?;
        Ok(tail_lines(trim_trailing_blank_lines(&raw), max_lines).to_string())
    }

    async fn move_window_to_tab(&self, id: &WindowId, to: TabTarget) -> Result<()> {
        // `-d` on both arms: without it tmux drags the attached client to the
        // target window (probe-verified), which would yank the user away from the
        // dashboard for what is a background rearrangement.
        let pane = pane_id(id.as_str())?;
        match to {
            TabTarget::New => {
                self.tmux_cmd(&["break-pane", "-d", "-s", pane]).await?;
            }
            TabTarget::Existing(tab) => {
                self.tmux_cmd(&[
                    "join-pane",
                    "-d",
                    "-s",
                    pane,
                    "-t",
                    window_id(tab.as_str())?,
                ])
                .await?;
            }
        }
        Ok(())
    }

    /// Rename the tmux *window* holding the dashboard's pane. An OSC title would
    /// reach only the pane title here — `allow-rename` is off by default, and
    /// turning it on for the user's window to route a label through it would be a
    /// far bigger footprint than a rename.
    ///
    /// `-t <pane>` is deliberate: tmux resolves a pane id to the window that holds
    /// it (probe-verified), so the dashboard's window needs no separate lookup and
    /// stays correct if the pane is moved between windows.
    async fn set_own_tab_title(&self, title: &str) -> Result<()> {
        let Some(pane) = self.own_pane.as_ref() else {
            return Ok(());
        };
        let pane = pane_id(pane.as_str())?;
        // Capture what to put back, once, before the first rename overwrites it.
        // `automatic-rename` rides along because tmux turns it off itself on any
        // explicit rename — leave that behind and the window stays pinned to its
        // last count long after the dashboard is gone.
        let unsaved = self
            .saved_window_name
            .lock()
            .expect("saved window name mutex")
            .is_none();
        if unsaved {
            let probed = self
                .tmux_cmd(&["display-message", "-p", "-t", pane, WINDOW_NAME_FORMAT])
                .await?;
            *self
                .saved_window_name
                .lock()
                .expect("saved window name mutex") = Some(parse_window_name(&probed));
        }
        self.tmux_cmd(&["rename-window", "-t", pane, title]).await?;
        Ok(())
    }

    async fn restore_own_tab_title(&self) -> Result<()> {
        let saved = self
            .saved_window_name
            .lock()
            .expect("saved window name mutex")
            .take();
        // Nothing saved = never renamed, so there is nothing to undo.
        let (Some((name, automatic)), Some(pane)) = (saved, self.own_pane.as_ref()) else {
            return Ok(());
        };
        let pane = pane_id(pane.as_str())?;
        self.tmux_cmd(&["rename-window", "-t", pane, &name]).await?;
        if automatic {
            // Re-derives the name from the running command, which is exactly what
            // would have happened had we never renamed it.
            self.tmux_cmd(&["set-option", "-w", "-t", pane, "automatic-rename", "on"])
                .await?;
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }
}

/// What `set_own_tab_title` asks about the dashboard's window before renaming it.
const WINDOW_NAME_FORMAT: &str = "#{window_name}\t#{automatic-rename}";

/// Parse a [`WINDOW_NAME_FORMAT`] reply into the name and whether tmux was
/// auto-naming the window. A window name may contain anything, so the flag —
/// rendered as `1`/`0` — goes last and the split is on the *last* tab.
fn parse_window_name(reply: &str) -> (String, bool) {
    let line = reply.trim_end_matches(['\n', '\r']);
    match line.rsplit_once('\t') {
        Some((name, automatic)) => (name.to_string(), automatic.trim() == "1"),
        // No separator at all means a tmux that didn't render the format; keep
        // the text as the name and assume auto-naming was off (the conservative
        // half — restoring it on would rename a window we never touched).
        None => (line.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> TmuxTerminal {
        TmuxTerminal {
            socket: "/tmp/tmux-1000/default".into(),
            server_pid: "4242".into(),
            session: "work".into(),
            own_pane: Some(WindowId("%3".into())),
            saved_window_name: Mutex::new(None),
        }
    }

    /// Real `list-panes -s -F` output from tmux 3.7b, with the active window
    /// second and a two-pane window.
    const LIST_PANES: &str = "@0\t%0\t0\ttmux\n\
                              @1\t%1\t1\tprobe-tab\n\
                              @1\t%4\t1\tprobe-tab\n\
                              @2\t%2\t0\tprobe2\n";

    #[test]
    fn snapshot_groups_panes_by_window() {
        let tabs = parse_snapshot(LIST_PANES);
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].id, TabId("@0".into()));
        assert_eq!(tabs[0].title, "tmux");
        assert!(!tabs[0].is_focused);
        assert_eq!(tabs[0].windows, vec![WindowId("%0".into())]);
        // Both panes of @1 land in one tab, and it is the focused one.
        assert!(tabs[1].is_focused);
        assert_eq!(
            tabs[1].windows,
            vec![WindowId("%1".into()), WindowId("%4".into())]
        );
        assert_eq!(tabs[2].id, TabId("@2".into()));
    }

    #[test]
    fn snapshot_keeps_separator_bearing_titles_and_skips_bad_rows() {
        // The free-text window name is last, so a tab in it can't shift the ids.
        let tabs = parse_snapshot("@0\t%0\t1\tmy\tweird\ttitle\n");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].title, "my\tweird\ttitle");
        assert_eq!(tabs[0].windows, vec![WindowId("%0".into())]);

        // A malformed row is skipped, never fatal: one bad line must not blind
        // the dashboard to every other window.
        let tabs = parse_snapshot("garbage\n@0\t%0\t1\tok\n\t\t\t\n5\t7\t1\tno-sigils\n");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].title, "ok");
        assert!(parse_snapshot("").is_empty());
    }

    #[test]
    fn ids_must_carry_their_sigil() {
        assert!(pane_id("%0").is_ok());
        assert!(pane_id("%12").is_ok());
        assert!(window_id("@7").is_ok());
        // Wrong sigil, no sigil, empty, and injection-shaped values all fail.
        assert!(pane_id("@7").is_err());
        assert!(window_id("%7").is_err());
        assert!(pane_id("7").is_err());
        assert!(pane_id("%").is_err());
        assert!(pane_id("").is_err());
        assert!(pane_id("%1; kill-server").is_err());
        assert!(pane_id("%-1").is_err());
    }

    #[test]
    fn parse_spawn_ids_reads_the_atomic_pair() {
        let (w, t) = parse_spawn_ids("%1 @1\n").unwrap();
        assert_eq!(w, WindowId("%1".into()));
        assert_eq!(t, TabId("@1".into()));
        // A garbled pair is an error, not a bad `-t` target later.
        assert!(parse_spawn_ids("%1").is_err());
        assert!(parse_spawn_ids("1 2").is_err());
        assert!(parse_spawn_ids("").is_err());
    }

    /// What the dashboard's window has to be put back to on the way out. A window
    /// name is free text (an OSC-renamed one can hold a tab), so the split is on
    /// the *last* separator and the `1`/`0` flag rides last.
    #[test]
    fn parse_window_name_splits_off_the_trailing_flag() {
        assert_eq!(parse_window_name("zsh\t1\n"), ("zsh".into(), true));
        assert_eq!(parse_window_name("miao\t0"), ("miao".into(), false));
        // A name carrying the separator keeps all of it.
        assert_eq!(
            parse_window_name("build\tstage\t1"),
            ("build\tstage".into(), true)
        );
        assert_eq!(parse_window_name("\t1"), ("".into(), true));
        // No flag rendered at all: keep the text, and don't turn auto-naming on
        // for a window we may never have renamed.
        assert_eq!(parse_window_name("plain"), ("plain".into(), false));
    }

    #[test]
    fn tmux_env_splits_from_the_right() {
        assert_eq!(
            parse_tmux_env("/tmp/tmux-1000/default,4242,0"),
            Some(("/tmp/tmux-1000/default".into(), "4242".into(), "0".into()))
        );
        // A socket path with a comma still parses.
        assert_eq!(
            parse_tmux_env("/tmp/od,d/s,4242,0").map(|t| t.0),
            Some("/tmp/od,d/s".into())
        );
        assert_eq!(parse_tmux_env("/tmp/s,notapid,0"), None);
        assert_eq!(parse_tmux_env("/tmp/s,4242"), None);
        assert_eq!(parse_tmux_env(""), None);
    }

    #[test]
    fn new_window_args_shape() {
        let args = new_window_args(
            "work",
            "/tmp/proj",
            Some("proj"),
            false,
            Some("'miao' 'claude'"),
        );
        assert_eq!(
            args,
            vec![
                "new-window",
                "-P",
                "-F",
                SPAWN_FORMAT,
                "-t",
                "work:",
                "-c",
                "/tmp/proj",
                "-d",
                "-n",
                "proj",
                "'miao' 'claude'",
            ]
        );
        // take_focus: true drops `-d` — tmux's native "create without focusing"
        // means there is never a focus snap-back to undo (contrast zellij).
        let args = new_window_args("work", "/tmp", None, true, None);
        assert!(!args.iter().any(|a| a == "-d"));
        assert!(!args.iter().any(|a| a == "-n"));
    }

    #[test]
    fn quote_argv_survives_spaces_and_quotes() {
        // tmux parses the command string itself, so every word is quoted.
        let argv = vec!["miao".into(), "claude".into(), "/a dir/x".into()];
        assert_eq!(quote_argv(&argv), "'miao' 'claude' '/a dir/x'");
        // An embedded single quote is escaped the POSIX way rather than ending
        // the quoted word.
        assert_eq!(quote_argv(&["it's".to_string()]), r"'it'\''s'");
    }

    #[test]
    fn trailing_blank_screen_rows_come_off_before_the_tail() {
        // capture-pane returns the whole visible screen; a pane that printed two
        // lines and sits idle has 20-odd blank rows under them. Tailing raw would
        // preview as blanks.
        let screen = "hello\nworld\n\n\n   \n\n";
        assert_eq!(trim_trailing_blank_lines(screen), "hello\nworld");
        assert_eq!(
            tail_lines(trim_trailing_blank_lines(screen), 5),
            "hello\nworld"
        );
        // Interior blanks are content and stay.
        assert_eq!(trim_trailing_blank_lines("a\n\nb\n\n"), "a\n\nb");
        assert_eq!(trim_trailing_blank_lines("\n\n"), "");
        assert_eq!(trim_trailing_blank_lines(""), "");
    }

    #[test]
    fn wrap_env_prefixes_env_path() {
        let argv = vec!["miao".to_string(), "claude".to_string()];
        assert_eq!(
            wrap_env(&argv, Some("/a:/b")),
            vec!["/usr/bin/env", "PATH=/a:/b", "miao", "claude"]
        );
        assert_eq!(wrap_env(&argv, None), argv);
    }

    #[test]
    fn identity_is_keyed_by_socket_and_server_pid() {
        assert_eq!(
            terminal().identity().as_deref(),
            Some("tmux:/tmp/tmux-1000/default,4242")
        );
        assert_eq!(terminal().current_window(), Some(WindowId("%3".into())));
    }

    /// Drive a real tmux server end to end, on a private socket with no user
    /// config (`-f /dev/null`), so every claim in the module doc is checked
    /// against the binary rather than against documentation. `#[ignore]`d
    /// because it needs a `tmux` on PATH and starts a server:
    ///
    /// ```sh
    /// cargo test -p captain-miao -- --ignored drives_a_real_tmux_server
    /// ```
    #[tokio::test]
    #[ignore = "needs a tmux binary; starts a real server"]
    async fn drives_a_real_tmux_server() {
        // A short socket path on purpose: a unix socket is capped near 104 bytes
        // and tmux fails with "File name too long" well inside a tempdir under a
        // deep prefix.
        let socket = format!("/tmp/cm-it-{}.sock", std::process::id());
        let base = ["-S", socket.as_str(), "-f", "/dev/null"];
        let run = |args: Vec<String>| {
            let base: Vec<String> = base.iter().map(|s| s.to_string()).collect();
            async move {
                let out = tokio::process::Command::new("tmux")
                    .args(base)
                    .args(&args)
                    .output()
                    .await
                    .expect("tmux");
                assert!(
                    out.status.success(),
                    "tmux {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
        };
        let argv = |s: &str| s.split(' ').map(|w| w.to_string()).collect::<Vec<_>>();

        // No pre-emptive kill-server: the socket is pid-unique, so this test
        // always starts its own server and never inherits a stale one.
        //
        // A session whose window holds a long-lived command, so the server can't
        // exit under the test.
        run(vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            "it".into(),
            "-x".into(),
            "80".into(),
            "-y".into(),
            "24".into(),
            "sleep 600".into(),
        ])
        .await;
        let server_pid = run(argv("display-message -p #{pid}")).await;

        let term = TmuxTerminal {
            socket: socket.clone(),
            server_pid: server_pid.clone(),
            session: "it".into(),
            own_pane: None,
            saved_window_name: Mutex::new(None),
        };

        // -- spawn: one atomic create reports BOTH ids, which is the contract
        // `SpawnResult` documents (the dashboard seeds its window→tab cache from
        // the pair and skips a snapshot).
        let spawned = term
            .spawn(SpawnSpec {
                cwd: "/tmp".into(),
                target: SpawnTarget::NewTab,
                command: SpawnCommand::Exec(argv("sleep 600")),
                title: Some("it-proj".into()),
                hold: false,
                take_focus: false,
                stack: false,
            })
            .await
            .expect("spawn");
        let pane = spawned.window.expect("pane id");
        let win = spawned.tab.expect("window id");
        assert!(pane.as_str().starts_with('%') && win.as_str().starts_with('@'));

        // -- snapshot: the pair really names a pane inside that window, the title
        // survived, and `take_focus: false` did not move the client.
        let tabs = term.snapshot().await.expect("snapshot");
        let tab = tabs
            .iter()
            .find(|t| t.id == win)
            .expect("spawned window in snapshot");
        assert!(tab.windows.contains(&pane));
        assert_eq!(tab.title, "it-proj");
        assert!(!tab.is_focused, "-d must not move the client");
        assert_eq!(
            crate::terminal::window_tab_map(&tabs).get(&pane),
            Some(&win)
        );

        // -- the title is pinned against an OSC rename from inside the pane,
        // which is what the work-tab map's title check depends on.
        assert_eq!(
            run(vec![
                "show-options".into(),
                "-w".into(),
                "-t".into(),
                win.to_string(),
                "allow-rename".into()
            ])
            .await,
            "allow-rename off"
        );

        // -- focus: a pane id resolves to its window, so one call switches both.
        term.focus_window(&pane).await.expect("focus_window");
        assert_eq!(
            run(vec![
                "display-message".into(),
                "-p".into(),
                // One argument: `argv`'s space-split would make it two.
                "#{window_id} #{pane_id}".into(),
            ])
            .await,
            format!("{win} {pane}")
        );

        // -- capture: the pane's own output comes back, trimmed to max_lines.
        let echo = term
            .spawn(SpawnSpec {
                cwd: "/tmp".into(),
                target: SpawnTarget::NewTab,
                command: SpawnCommand::Exec(vec![
                    "sh".into(),
                    "-c".into(),
                    "echo cm-probe-marker; sleep 600".into(),
                ]),
                title: None,
                hold: false,
                take_focus: false,
                stack: false,
            })
            .await
            .expect("spawn echo");
        let echo_pane = echo.window.expect("pane id");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let text = term.capture_text(&echo_pane, 5).await.expect("capture");
        assert!(
            text.contains("cm-probe-marker"),
            "capture missed the pane's output: {text:?}"
        );
        assert!(text.lines().count() <= 5, "capture not trimmed: {text:?}");

        // -- move_to_tab: the real capability this backend claims (zellij can't).
        // `-d` keeps the client where it is.
        let before = run(argv("display-message -p #{window_id}")).await;
        term.move_window_to_tab(&echo_pane, TabTarget::Existing(win.clone()))
            .await
            .expect("join-pane");
        let tabs = term.snapshot().await.expect("snapshot");
        let tab = tabs.iter().find(|t| t.id == win).expect("target window");
        assert!(tab.windows.contains(&echo_pane), "pane did not move");
        assert_eq!(
            run(argv("display-message -p #{window_id}")).await,
            before,
            "join-pane -d must not drag the client"
        );

        // -- own-tab title: the dashboard labels the window holding its own pane
        // with the attention count and hands the name back on the way out. Both
        // halves are probed on a window tmux is *auto*-naming, because an explicit
        // rename silently turns `automatic-rename` off — restore the name without
        // the flag and the window stays frozen on a dead dashboard's count.
        let own_pane = WindowId(run(argv("new-window -d -P -F #{pane_id} sleep 600")).await);
        let dashboard = TmuxTerminal {
            socket: socket.clone(),
            server_pid: server_pid.clone(),
            session: "it".into(),
            own_pane: Some(own_pane.clone()),
            saved_window_name: Mutex::new(None),
        };
        let probe_name = || {
            run(vec![
                "display-message".into(),
                "-p".into(),
                "-t".into(),
                own_pane.to_string(),
                WINDOW_NAME_FORMAT.into(),
            ])
        };
        let before = probe_name().await;
        assert!(
            before.ends_with('1'),
            "expected tmux to be auto-naming the fresh window: {before:?}"
        );
        dashboard
            .set_own_tab_title("miao (2)")
            .await
            .expect("set_own_tab_title");
        assert_eq!(
            probe_name().await,
            "miao (2)\t0",
            "the rename must land on the pane's window, and tmux pins it"
        );
        dashboard
            .restore_own_tab_title()
            .await
            .expect("restore_own_tab_title");
        assert_eq!(
            probe_name().await,
            before,
            "the name AND its auto-rename flag must both come back"
        );

        // -- close, then close again: a speculative close of a dead id is what
        // the restart/kill paths do, and it must stay a plain ignorable error.
        term.close_window(&pane).await.expect("close_window");
        let err = term.close_window(&pane).await.unwrap_err().to_string();
        assert!(err.contains("can't find pane"), "unexpected: {err}");

        // -- hold: a pane whose command exits survives, keeping its output plus
        // tmux's own dead-pane line — the FailedToStart row's whole point.
        let held = term
            .spawn(SpawnSpec {
                cwd: "/tmp".into(),
                target: SpawnTarget::NewTab,
                command: SpawnCommand::Exec(vec![
                    "sh".into(),
                    "-c".into(),
                    "echo cm-held-error; exit 7".into(),
                ]),
                title: None,
                hold: true,
                take_focus: false,
                stack: false,
            })
            .await
            .expect("spawn held");
        let held_pane = held.window.expect("pane id");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // Asked for more lines than the pane is tall, so the whole screen comes
        // back: tmux draws its notice on the pane's *last* row, which leaves the
        // command's output above a run of blank rows (module doc). A `hold` that
        // matters most — `FailedToStart` — doesn't hit that, because the launcher
        // blocks instead of exiting, so its pane is alive and its error is the
        // last thing on screen.
        let text = term
            .capture_text(&held_pane, 40)
            .await
            .expect("capture held");
        assert!(
            text.contains("cm-held-error") && text.contains("Pane is dead"),
            "held pane lost its error: {text:?}"
        );

        // -- identity: keyed by socket AND server pid, matching what a launcher
        // in one of these panes self-reports from `TMUX` via cm-core.
        let tmux_env = format!("{socket},{server_pid},0");
        assert_eq!(
            term.identity(),
            cm_core::terminal::tmux_identity(&tmux_env),
            "backend identity must match the launcher's env-derived one"
        );

        run(argv("kill-server")).await;
    }

    /// `resolve_spawn_target` never yields the shared-tab targets on tmux (it
    /// reports neither capability); the arm bails before touching a subprocess,
    /// so the rejection is exercised with no tmux server running.
    #[tokio::test]
    async fn spawn_rejects_stacked_targets() {
        for target in [SpawnTarget::Floating, SpawnTarget::SharedStackTab] {
            let spec = SpawnSpec {
                cwd: "/tmp".into(),
                target,
                command: SpawnCommand::Shell,
                title: None,
                hold: true,
                take_focus: false,
                stack: false,
            };
            let err = terminal().spawn(spec).await.unwrap_err();
            assert!(err.to_string().contains("own window"));
        }
    }
}
