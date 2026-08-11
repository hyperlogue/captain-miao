//! Zellij backend: wraps the `zellij action` CLI (zellij ≥ 0.44).
//!
//! Transport is one `zellij action` subprocess per call, pinned to the
//! `ZELLIJ_SESSION_NAME` captured at startup (the env var scopes the action to
//! that session, and works from outside a zellij pane too). Pane and tab ids
//! are zellij's *stable* integer ids (`tab_id` / pane `id`), not positions.
//!
//! Backend quirks this module encodes (probe-verified on 0.44.3):
//! - Plugin panes share the id namespace with terminal panes (a `plugin_0` and
//!   a `terminal_0` coexist), so `list-panes` output is always filtered on
//!   `is_plugin == false` and ids are passed to the CLI as `terminal_<n>`.
//! - Pane commands inherit the zellij *server*'s environment, not the caller's,
//!   so an `Exec` argv is wrapped in `/usr/bin/env PATH=<dashboard PATH> …`.
//! - An exited command pane is held open by default (the equivalent of kitty
//!   `--hold`); `--close-on-exit` is the inverse, mapped from `hold: false`.
//! - There is no CLI to reparent a pane into another tab (`BreakPane` is
//!   keybind/plugin-only), so `move_window_to_tab` is unsupported.
//! - Sessions live as full-size floating panes in one shared sessions tab
//!   (the `floating_sessions` capability): every session is a borderless
//!   100%×100% floating pane in the tab titled [`SESSIONS_TAB`], all at
//!   identical geometry, so the z-order top is the visible one. The floating
//!   layer gives exactly the primitives the tab arrangements lacked
//!   (probe-verified on 0.44.3): `focus-pane-id` on a floating pane raises it
//!   to the top AND shows the layer AND switches tabs as needed — one ~20ms
//!   action, no pty resize, no flicker; a borderless 100% pane gets the full
//!   viewport pty (only tab/status bars off), rescaled cleanly when the
//!   client resizes; hiding the layer never resizes; and `new-pane
//!   --floating --tab-id <non-active>` moves nothing — not the client, not
//!   the layer, not even the floating focus — so spawning from the dashboard
//!   is blink-free (contrast `new-tab`, which always drags the client). The
//!   earlier arrangements this replaced: native stacked panes cost a title-bar
//!   row per collapsed session and a pty resize per switch; fullscreen emulation
//!   flickered. (The one-tab-per-session arrangement is still available as the
//!   `SessionsLayout::PerTab` mode — a `NewTab` spawn per session; the Stacked
//!   default is these floating panes.) The `--stacked` new-pane join those
//!   stacked-pane arrangements used is in git history; on the `floating_sessions`
//!   backend `resolve_spawn_target` yields only `Floating` (Stacked) or `NewTab`
//!   (Per-tab), and the `SharedStackTab` Stacked target (a `window_stacking`
//!   backend's, i.e. Kitty's) is rejected outright.
//! - `list-panes` is kept OFF hot paths: the server collects per-pane metadata
//!   at ~20ms per pane (measured ~475ms at 22 panes, vs ~18ms for any other
//!   action), so it only runs where unavoidable (snapshot, pane-id recovery
//!   after a `new-tab` spawn), never on focus — a floating spawn needs none
//!   (`new-pane` prints the pane id).

use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

use super::{
    SpawnCommand, SpawnResult, SpawnSpec, SpawnTarget, Tab, TabId, TabTarget, Terminal, WindowId,
    tail_lines, wrap_env,
};

/// Title of the shared tab hosting every session's floating pane. Looked up
/// by name on each floating spawn (one `list-tabs`, per-tab cost) rather than
/// by a cached id: zellij recycles a closed highest tab's id onto the next
/// tab created, so an id could silently point at an impostor — the name is
/// the identity, and a user closing the tab just gets it recreated.
const SESSIONS_TAB: &str = "miao:sessions";

pub struct ZellijTerminal {
    /// Session every `zellij action` call is pinned to, captured at startup.
    session: String,
    /// The dashboard's own pane (`ZELLIJ_PANE_ID`), used to hand focus back
    /// after a `take_focus: false` spawn — zellij's `new-tab` always moves
    /// focus to the new tab; there is no `--dont-take-focus`.
    own_pane: Option<WindowId>,
    /// Where [`OwnTab`] resolution got to. Behind a `Mutex` because the title is
    /// set through `&self` like every other backend call.
    own_tab: Mutex<OwnTab>,
}

/// The tab holding the dashboard's own pane, for `set_own_tab_title`.
///
/// Resolved lazily and **once**: the lookup costs a `list-panes` (~20ms *per
/// pane*, the one expensive action) and the answer cannot change — zellij has no
/// way to reparent a pane across tabs, which is the same limitation that makes
/// `move_window_to_tab` unsupported here.
#[derive(Debug)]
enum OwnTab {
    /// Not looked up yet.
    Unknown,
    /// Looked up and not found. Kept distinct from `Unknown` so a dashboard
    /// whose pane isn't in the tree doesn't re-pay the scan on every change.
    Absent,
    /// The tab, and the name it had before the first rename.
    Known(TabId, String),
}

impl ZellijTerminal {
    /// Construct from the environment. `None` when not inside (or pointed at)
    /// a zellij session.
    pub fn from_env() -> Option<Self> {
        let session = std::env::var("ZELLIJ_SESSION_NAME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        let own_pane = std::env::var("ZELLIJ_PANE_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .map(WindowId);
        Some(Self {
            session,
            own_pane,
            own_tab: Mutex::new(OwnTab::Unknown),
        })
    }

    async fn zellij_cmd(&self, args: &[&str]) -> Result<String> {
        let started = std::time::Instant::now();
        let output = Command::new("zellij")
            .env("ZELLIJ_SESSION_NAME", &self.session)
            .arg("action")
            .args(args)
            .output()
            .await
            .context("Failed to run zellij")?;
        // Per-call timing in the debug log: zellij actions are ~18ms except
        // `list-panes`, which costs ~20ms *per pane* server-side — this is
        // how a hot path that regressed onto it gets spotted.
        tracing::debug!(
            "zellij action {} took {:?}",
            args.join(" "),
            started.elapsed()
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "zellij action {} failed: {}",
                args.first().unwrap_or(&""),
                stderr.trim()
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The full pane list as raw JSON values, terminal panes only (plugin panes
    /// share the id namespace and must never be addressed as windows).
    async fn terminal_panes(&self) -> Result<Vec<serde_json::Value>> {
        let stdout = self.zellij_cmd(&["list-panes", "-a", "-j"]).await?;
        parse_terminal_panes(&stdout)
    }

    /// Focus a pane, treating zellij's "already focused" refusal as success:
    /// `focus-pane-id` on the currently-focused pane exits non-zero ("Pane
    /// Terminal(n) is already focused"), but the pane holding focus is the
    /// outcome we wanted. Any other failure is a real error.
    async fn focus_pane(&self, arg: &str) -> Result<()> {
        match self.zellij_cmd(&["focus-pane-id", arg]).await {
            Err(e) if !e.to_string().contains("already focused") => Err(e),
            _ => Ok(()),
        }
    }

    /// Hand focus back to the dashboard's own pane after a spawn that
    /// shouldn't keep it. Best-effort — a failed refocus never fails the
    /// spawn.
    async fn refocus_own_pane(&self) {
        if let Some(own) = &self.own_pane
            && let Ok(arg) = pane_arg(own)
            && let Err(e) = self.focus_pane(&arg).await
        {
            tracing::debug!("refocus after spawn failed: {e}");
        }
    }

    /// The id of the shared sessions tab, creating it if absent. Looked up by
    /// title every call (see [`SESSIONS_TAB`] for why not a cached id) — one
    /// `list-tabs`, per-tab cost, on the rare spawn path only. Creation is
    /// the one place the client blinks (`new-tab` has no --dont-take-focus),
    /// so focus snaps back immediately; that's a one-time cost per zellij
    /// session. The snap-back is unconditional: the only caller spawns
    /// floating sessions with `take_focus: false`.
    async fn ensure_sessions_tab(&self) -> Result<u64> {
        let tabs = self.zellij_cmd(&["list-tabs", "-a", "-j"]).await?;
        if let Some(id) = find_tab_id_by_name(&tabs, SESSIONS_TAB)? {
            return Ok(id);
        }
        let name = format!("--name={SESSIONS_TAB}");
        let stdout = self.zellij_cmd(&["new-tab", &name]).await?;
        let tab_id = parse_tab_id(&stdout)?;
        self.refocus_own_pane().await;
        Ok(tab_id)
    }

    /// The tab holding the dashboard's own pane and the name it had when we first
    /// asked — memoized, including the negative answer. See [`OwnTab`] for why
    /// the answer can be cached for the life of the process.
    async fn own_tab(&self) -> Option<(TabId, String)> {
        match &*self.own_tab.lock().expect("own tab mutex") {
            OwnTab::Known(id, name) => return Some((id.clone(), name.clone())),
            OwnTab::Absent => return None,
            OwnTab::Unknown => {}
        }
        let resolved = self.resolve_own_tab().await;
        *self.own_tab.lock().expect("own tab mutex") = match &resolved {
            Some((id, name)) => OwnTab::Known(id.clone(), name.clone()),
            None => OwnTab::Absent,
        };
        resolved
    }

    /// One `list-panes` to find which tab holds `own_pane`, then one `list-tabs`
    /// for that tab's current name. `None` at any missing step — the tab label is
    /// cosmetic, so an unresolvable pane just goes unlabelled.
    async fn resolve_own_tab(&self) -> Option<(TabId, String)> {
        let own = self.own_pane.as_ref()?.as_str().parse::<u64>().ok()?;
        let panes = self.terminal_panes().await.ok()?;
        let tab_id = panes
            .iter()
            .find(|p| p["id"].as_u64() == Some(own))
            .and_then(|p| p["tab_id"].as_u64())?;
        let tabs = self.zellij_cmd(&["list-tabs", "-a", "-j"]).await.ok()?;
        let name = find_tab_name_by_id(&tabs, tab_id).ok()??;
        Some((TabId::from(tab_id), name))
    }
}

/// Validate a zellij pane/tab id: bare ASCII digits only (zellij's stable ids
/// are non-negative integers). The only untrusted source is captain-miao's own
/// state files; anything else fails closed rather than mis-target — same
/// posture as kitty's `match_id`.
fn digits_id(id: &str) -> Result<&str> {
    super::validate_id(id, "zellij")
}

/// Format a validated window id as the CLI's explicit terminal-pane form.
/// A bare integer is accepted by the CLI too, but `terminal_<n>` can never be
/// misread as a plugin pane.
fn pane_arg(id: &WindowId) -> Result<String> {
    Ok(format!("terminal_{}", digits_id(id.as_str())?))
}

/// Parse the pane id `new-pane`/`edit` print (`terminal_<n>`; a bare integer is
/// tolerated for robustness).
fn parse_pane_id(s: &str) -> Result<WindowId> {
    let s = s.trim();
    let n = s.strip_prefix("terminal_").unwrap_or(s);
    n.parse::<u64>()
        .with_context(|| format!("unexpected zellij pane id output: {s:?}"))
        .map(WindowId::from)
}

/// Parse the bare integer tab id `new-tab` prints on stdout.
fn parse_tab_id(stdout: &str) -> Result<u64> {
    stdout
        .trim()
        .parse::<u64>()
        .context("Failed to parse tab ID from new-tab output")
}

/// The id of the first terminal pane in `tab_id` (a fresh `new-tab` has exactly
/// one). `None` when no listed pane belongs to that tab — the `new-tab` pane-id
/// recovery treats that as best-effort (a `--close-on-exit` command can exit and
/// close its pane before the list runs), so the spawn still reports the tab.
fn first_pane_in_tab(panes: &[serde_json::Value], tab_id: u64) -> Option<u64> {
    panes
        .iter()
        .filter(|p| p["tab_id"].as_u64() == Some(tab_id))
        .filter_map(|p| p["id"].as_u64())
        .next()
}

/// Terminal panes (only) from `list-panes -a -j` output.
fn parse_terminal_panes(json: &str) -> Result<Vec<serde_json::Value>> {
    let panes: Vec<serde_json::Value> =
        serde_json::from_str(json).context("Failed to parse zellij list-panes JSON")?;
    Ok(panes
        .into_iter()
        .filter(|p| p["is_plugin"].as_bool() == Some(false))
        .collect())
}

/// Parse `list-tabs -a -j` output into its raw JSON values.
fn parse_tabs_json(json: &str) -> Result<Vec<serde_json::Value>> {
    serde_json::from_str(json).context("Failed to parse zellij list-tabs JSON")
}

/// Assemble the snapshot from the two list calls' JSON. Tab order follows
/// `list-tabs`; a pane whose `tab_id` matches no listed tab is dropped.
fn parse_snapshot(list_tabs_json: &str, list_panes_json: &str) -> Result<Vec<Tab>> {
    let tabs_json = parse_tabs_json(list_tabs_json)?;
    let mut tabs: Vec<Tab> = Vec::new();
    for t in &tabs_json {
        let Some(tab_id) = t["tab_id"].as_u64() else {
            continue;
        };
        tabs.push(Tab {
            id: TabId::from(tab_id),
            title: t["name"].as_str().unwrap_or("").to_string(),
            is_focused: t["active"].as_bool().unwrap_or(false),
            windows: Vec::new(),
        });
    }
    for p in parse_terminal_panes(list_panes_json)? {
        let (Some(pid), Some(tid)) = (p["id"].as_u64(), p["tab_id"].as_u64()) else {
            continue;
        };
        let tid = TabId::from(tid);
        if let Some(tab) = tabs.iter_mut().find(|t| t.id == tid) {
            tab.windows.push(WindowId::from(pid));
        }
    }
    Ok(tabs)
}

/// The id of the tab named `name`, from `list-tabs -a -j` output. First match
/// wins if the user duplicated the title.
fn find_tab_id_by_name(list_tabs_json: &str, name: &str) -> Result<Option<u64>> {
    let tabs = parse_tabs_json(list_tabs_json)?;
    Ok(tabs
        .iter()
        .find(|t| t["name"].as_str() == Some(name))
        .and_then(|t| t["tab_id"].as_u64()))
}

/// The current name of the tab with stable id `tab_id` — the inverse lookup of
/// [`find_tab_id_by_name`], for remembering what to put a renamed tab back to.
fn find_tab_name_by_id(list_tabs_json: &str, tab_id: u64) -> Result<Option<String>> {
    let tabs = parse_tabs_json(list_tabs_json)?;
    Ok(tabs
        .iter()
        .find(|t| t["tab_id"].as_u64() == Some(tab_id))
        .and_then(|t| t["name"].as_str())
        .map(str::to_string))
}

/// Append the trailing `[--close-on-exit] [-- <argv>]` shared by the `NewTab`
/// and `Floating` spawn arms: the hold-inverse flag (an exited command pane is
/// held open by default), then the argv separator and command. An absent `exec`
/// appends neither.
fn push_exec_tail(args: &mut Vec<String>, close_on_exit: bool, exec: Option<&[String]>) {
    if close_on_exit {
        args.push("--close-on-exit".into());
    }
    if let Some(cmd) = exec {
        args.push("--".into());
        args.extend(cmd.iter().cloned());
    }
}

/// The `new-pane` argv for a session's floating pane: full-viewport geometry
/// (`100%` is re-derived by zellij on every client resize) and borderless, so
/// the pty is exactly what a lone tiled pane would get — no frame rows, and
/// all sessions stack pixel-identical so the z-order top is the visible one.
///
/// Every value-bearing flag is a single joined `--flag=value` element: zellij's
/// clap rejects an option value passed as a separate argv element when it begins
/// with `-` ("Found argument '-x' which wasn't expected"), so a cwd/title basename
/// starting with `-` would fail the spawn; the joined form is immune whatever the
/// value. Digit/percent values can't start with `-`, but join for uniformity.
fn floating_new_pane_args(
    tab_id: u64,
    cwd: &str,
    title: Option<&str>,
    close_on_exit: bool,
    exec: Option<&[String]>,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "new-pane".into(),
        "--floating".into(),
        format!("--tab-id={tab_id}"),
        "--x=0".into(),
        "--y=0".into(),
        "--width=100%".into(),
        "--height=100%".into(),
        "--borderless=true".into(),
        format!("--cwd={cwd}"),
    ];
    if let Some(title) = title {
        args.push(format!("--name={title}"));
    }
    push_exec_tail(&mut args, close_on_exit, exec);
    args
}

/// The zellij backend's fixed capabilities: no cross-tab reparent CLI, so `t`
/// is hidden (`move_to_tab: false`); and sessions live as floating panes in one
/// shared tab instead of kitty-style stacked tabs (`window_stacking: false`,
/// `floating_sessions: true` — see the module doc). Exported so tests assert
/// against the real value rather than a hand-built literal that could silently
/// diverge when a capability field is added.
pub(crate) const CAPABILITIES: super::Capabilities = super::Capabilities {
    move_to_tab: false,
    window_stacking: false,
    floating_sessions: true,
};

#[async_trait]
impl Terminal for ZellijTerminal {
    fn current_window(&self) -> Option<WindowId> {
        self.own_pane.clone()
    }

    fn identity(&self) -> Option<String> {
        // Every `zellij action` call is pinned to `self.session` — that
        // session is the instance this backend drives.
        Some(cm_core::terminal::zellij_identity(&self.session))
    }

    async fn snapshot(&self) -> Result<Vec<Tab>> {
        // Independent list calls with no data dependency: run them concurrently
        // so the ~18ms list-tabs hides under the dominant (per-pane) list-panes.
        let (tabs, panes) = tokio::try_join!(
            self.zellij_cmd(&["list-tabs", "-a", "-j"]),
            self.zellij_cmd(&["list-panes", "-a", "-j"]),
        )?;
        parse_snapshot(&tabs, &panes)
    }

    async fn spawn(&self, spec: SpawnSpec) -> Result<SpawnResult> {
        let exec: Option<Vec<String>> = match &spec.command {
            SpawnCommand::Shell => None,
            SpawnCommand::Exec(argv) => Some(wrap_env(argv, std::env::var("PATH").ok().as_deref())),
        };

        let result = match &spec.target {
            SpawnTarget::NewTab => {
                // Joined `--flag=value` so a cwd/title basename starting with
                // `-` isn't rejected as a stray option (see `floating_new_pane_args`).
                let mut args: Vec<String> = vec!["new-tab".into()];
                if let Some(title) = &spec.title {
                    args.push(format!("--name={title}"));
                }
                args.push(format!("--cwd={}", spec.cwd));
                // An exited command pane is held by default; `hold: false`
                // requests the kitty `--hold`-less behavior.
                push_exec_tail(&mut args, !spec.hold && exec.is_some(), exec.as_deref());
                let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                let stdout = self.zellij_cmd(&arg_refs).await?;
                // `new-tab` prints the new TAB's bare integer id. If even that
                // fails to parse nothing sane can proceed, so it stays an Err.
                let tab_id = parse_tab_id(&stdout)?;
                // Past this point the tab AND its command are already running:
                // the spawn is atomic and MUST NOT become an Err, or the caller
                // would report failure and later spawn a duplicate tab. The
                // pane-id recovery is therefore best-effort — `new-tab` prints
                // only the tab id, so a pane list filtered by that tab recovers
                // it (a fresh tab has one terminal pane). Both a `list-panes`
                // error and an empty result (a `--close-on-exit` command can
                // exit and close its pane before the list runs) yield `window:
                // None` with the tab still reported. (The only NewTab spawn is
                // the work tab, `take_focus: true`, so `new-tab` dragging the
                // client to the new tab is the wanted outcome; nothing snaps
                // back.)
                let window = match self.terminal_panes().await {
                    Ok(panes) => {
                        let pane = first_pane_in_tab(&panes, tab_id);
                        if pane.is_none() {
                            tracing::debug!(
                                "new-tab {tab_id}: no terminal pane recovered (command may have exited)"
                            );
                        }
                        pane.map(WindowId::from)
                    }
                    Err(e) => {
                        tracing::debug!(
                            "new-tab {tab_id}: pane-id recovery list-panes failed: {e}"
                        );
                        None
                    }
                };
                // A per-tab *session* spawn passes `take_focus: false`, but
                // `new-tab` always drags the client to the new tab (no
                // `--dont-take-focus`), so snap focus back to the dashboard's
                // own pane. Work tabs pass `take_focus: true` — `w` means "go
                // there" — and keep the focus `new-tab` gave them.
                if !spec.take_focus {
                    self.refocus_own_pane().await;
                }
                SpawnResult {
                    window,
                    tab: Some(TabId::from(tab_id)),
                }
            }
            SpawnTarget::SharedStackTab => {
                // Unreachable by policy: `resolve_spawn_target` only yields
                // SharedStackTab for a `window_stacking` backend (Kitty), and
                // zellij is `floating_sessions` instead — its Stacked spawn is
                // Floating. zellij has no window-stacked tab arrangement.
                anyhow::bail!(
                    "SharedStackTab spawns are not used on the zellij backend (Stacked sessions float in the shared sessions tab)"
                )
            }
            SpawnTarget::Floating => {
                let tab_id = self.ensure_sessions_tab().await?;
                let args = floating_new_pane_args(
                    tab_id,
                    &spec.cwd,
                    spec.title.as_deref(),
                    !spec.hold && exec.is_some(),
                    exec.as_deref(),
                );
                let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                let stdout = self.zellij_cmd(&arg_refs).await?;
                // No refocus needed: both Floating producers pass
                // `take_focus: false`, and the dashboard spawns from its own
                // tab, so the sessions tab is non-active and the client never
                // budged. (If another client is parked on the sessions tab, the
                // pane does land on top of its view — yanking that client to
                // the dashboard would be worse, so leave it.)
                SpawnResult {
                    window: Some(parse_pane_id(&stdout)?),
                    tab: Some(TabId::from(tab_id)),
                }
            }
        };

        Ok(result)
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        // `focus-pane-id` does everything in one ~18ms action: switches the
        // client to the pane's tab, shows the floating layer if hidden, and
        // raises a floating pane to the top of the z-order — so a session in
        // the shared sessions tab surfaces with no arrangement calls and no
        // pty resize. (Embedded panes still work: focusing one auto-hides
        // the floating layer; a collapsed stack member expands natively.)
        self.focus_pane(&pane_arg(id)?).await
    }

    async fn focus_tab(&self, id: &TabId) -> Result<()> {
        let arg = digits_id(id.as_str())?;
        self.zellij_cmd(&["go-to-tab-by-id", arg]).await?;
        Ok(())
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        let arg = format!("--pane-id={}", pane_arg(id)?);
        self.zellij_cmd(&["close-pane", &arg]).await?;
        Ok(())
    }

    async fn capture_text(&self, id: &WindowId, max_lines: usize) -> Result<String> {
        // `--full` anchors at the live bottom of the scrollback; `--ansi`
        // preserves SGR styling. No "last N lines" flag, so tail here.
        let arg = format!("--pane-id={}", pane_arg(id)?);
        let raw = self
            .zellij_cmd(&["dump-screen", &arg, "--ansi", "--full"])
            .await?;
        Ok(tail_lines(&raw, max_lines).to_string())
    }

    async fn move_window_to_tab(&self, _id: &WindowId, _to: TabTarget) -> Result<()> {
        // zellij 0.44 has no CLI to reparent a pane into another tab
        // (`move-pane` is within-tab only; `BreakPane` is keybind/plugin-only).
        anyhow::bail!("moving a pane to another tab is not supported by the zellij backend");
    }

    /// Rename the tab holding the dashboard's pane. An OSC title reaches only the
    /// *pane* name here, which the tab bar never shows, so this is a real
    /// `rename-tab-by-id` — the stable-id addressing the rest of the backend uses
    /// (`go-to-tab-by-id`, `close-tab-by-id`), and the one form probe-verified to
    /// take a `list-tabs` `tab_id` rather than a position.
    async fn set_own_tab_title(&self, title: &str) -> Result<()> {
        let Some((tab, _)) = self.own_tab().await else {
            return Ok(());
        };
        self.zellij_cmd(&["rename-tab-by-id", digits_id(tab.as_str())?, title])
            .await?;
        Ok(())
    }

    async fn restore_own_tab_title(&self) -> Result<()> {
        // Read the memo rather than `own_tab()`: an unresolved tab means we never
        // renamed anything, and resolving one on the way out would be an
        // expensive `list-panes` for nothing.
        let known = match &*self.own_tab.lock().expect("own tab mutex") {
            OwnTab::Known(id, name) => Some((id.clone(), name.clone())),
            _ => None,
        };
        let Some((tab, name)) = known.filter(|(_, name)| !name.is_empty()) else {
            return Ok(());
        };
        // Renaming back is exact where `undo-rename-tab` is not: zellij freezes an
        // auto name (`Tab #3`) at creation and never renumbers it, so restoring
        // the captured string reproduces what the user saw either way.
        self.zellij_cmd(&["rename-tab-by-id", digits_id(tab.as_str())?, &name])
            .await?;
        Ok(())
    }

    fn capabilities(&self) -> super::Capabilities {
        CAPABILITIES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `zellij action list-tabs -a -j` on 0.44.3.
    const LIST_TABS: &str = r#"[
      {
        "position": 0, "name": "Tab #1", "active": true,
        "panes_to_hide": 0, "is_fullscreen_active": false,
        "viewport_rows": 22, "tab_id": 0, "has_bell_notification": false
      },
      {
        "position": 1, "name": "probe-tab", "active": false,
        "panes_to_hide": 0, "is_fullscreen_active": false,
        "viewport_rows": 22, "tab_id": 1, "has_bell_notification": false
      }
    ]"#;

    /// Trimmed from a real `zellij action list-panes -a -j` on 0.44.3. Keeps
    /// the id-namespace collision (a plugin pane and a terminal pane both with
    /// id 0) and a plugin pane on each tab.
    const LIST_PANES: &str = r#"[
      {
        "id": 0, "is_plugin": true, "is_focused": false, "title": "tab-bar",
        "exited": false, "is_held": false, "terminal_command": null,
        "plugin_url": "tab-bar", "tab_id": 0, "tab_position": 0, "tab_name": "Tab #1"
      },
      {
        "id": 0, "is_plugin": false, "is_focused": true, "title": "fish",
        "exited": false, "is_held": false, "terminal_command": null,
        "plugin_url": null, "tab_id": 0, "tab_position": 0, "tab_name": "Tab #1",
        "pane_command": "fish", "pane_cwd": "/tmp"
      },
      {
        "id": 2, "is_plugin": true, "is_focused": false, "title": "status-bar",
        "exited": false, "is_held": false, "terminal_command": null,
        "plugin_url": "status-bar", "tab_id": 1, "tab_position": 1, "tab_name": "probe-tab"
      },
      {
        "id": 1, "is_plugin": false, "is_focused": false, "title": "sh",
        "exited": true, "is_held": true,
        "terminal_command": "/usr/bin/env sh -c echo hello",
        "plugin_url": null, "tab_id": 1, "tab_position": 1, "tab_name": "probe-tab"
      }
    ]"#;

    #[test]
    fn snapshot_filters_plugin_panes_and_groups_by_tab() {
        let tabs = parse_snapshot(LIST_TABS, LIST_PANES).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, TabId::from(0));
        assert_eq!(tabs[0].title, "Tab #1");
        assert!(tabs[0].is_focused);
        // Only the terminal pane with id 0 — not the plugin pane sharing id 0.
        assert_eq!(tabs[0].windows.len(), 1);
        assert_eq!(tabs[0].windows[0], WindowId::from(0));
        assert_eq!(tabs[1].id, TabId::from(1));
        assert_eq!(tabs[1].title, "probe-tab");
        assert!(!tabs[1].is_focused);
        assert_eq!(tabs[1].windows.len(), 1);
        assert_eq!(tabs[1].windows[0], WindowId::from(1));
    }

    #[test]
    fn digits_id_accepts_only_digits() {
        assert_eq!(digits_id("0").unwrap(), "0");
        assert_eq!(digits_id("123").unwrap(), "123");
        assert!(digits_id("").is_err());
        assert!(digits_id("-1").is_err());
        assert!(digits_id("terminal_1").is_err());
        assert!(digits_id("1; rm -rf /").is_err());
    }

    #[test]
    fn pane_arg_formats_explicit_terminal_form() {
        assert_eq!(pane_arg(&WindowId::from(7)).unwrap(), "terminal_7");
        assert!(pane_arg(&WindowId("plugin_1".into())).is_err());
    }

    #[test]
    fn parse_pane_id_reads_new_pane_output() {
        assert_eq!(parse_pane_id("terminal_12\n").unwrap(), WindowId::from(12));
        assert_eq!(parse_pane_id("3").unwrap(), WindowId::from(3));
        assert!(parse_pane_id("plugin_2").is_err());
        assert!(parse_pane_id("").is_err());
    }

    /// The `new-tab` pane-id recovery: the first terminal pane in the new tab is
    /// the window, and a tab with no listed pane (a `--close-on-exit` command
    /// that already exited) recovers nothing — best-effort, never an error.
    #[test]
    fn first_pane_in_tab_picks_the_new_tabs_pane() {
        let panes = parse_terminal_panes(LIST_PANES).unwrap();
        // Tab 0's sole terminal pane is id 0; tab 1's is id 1.
        assert_eq!(first_pane_in_tab(&panes, 0), Some(0));
        assert_eq!(first_pane_in_tab(&panes, 1), Some(1));
        // A tab with no pane in the list → no window recovered.
        assert_eq!(first_pane_in_tab(&panes, 99), None);
        assert_eq!(first_pane_in_tab(&[], 0), None);
    }

    /// The inverse lookup `restore_own_tab_title` restores from — a tab renamed
    /// on the way in has to be put back by the name it *had*, and `undo-rename`
    /// can't do that for a tab the user named themselves.
    #[test]
    fn find_tab_name_by_id_reads_the_current_name() {
        assert_eq!(
            find_tab_name_by_id(LIST_TABS, 1).unwrap().as_deref(),
            Some("probe-tab")
        );
        assert_eq!(
            find_tab_name_by_id(LIST_TABS, 0).unwrap().as_deref(),
            Some("Tab #1")
        );
        assert_eq!(find_tab_name_by_id(LIST_TABS, 99).unwrap(), None);
        assert!(find_tab_name_by_id("not json", 1).is_err());
    }

    #[test]
    fn find_tab_id_by_name_matches_exact_title() {
        assert_eq!(
            find_tab_id_by_name(LIST_TABS, "probe-tab").unwrap(),
            Some(1)
        );
        assert_eq!(find_tab_id_by_name(LIST_TABS, "Tab #1").unwrap(), Some(0));
        assert_eq!(
            find_tab_id_by_name(LIST_TABS, "miao:sessions").unwrap(),
            None
        );
        assert!(find_tab_id_by_name("not json", "x").is_err());
    }

    /// Pins the floating-session pane shape: full-viewport percent geometry
    /// (re-derived by zellij on client resize) and borderless (no frame rows
    /// — the pty must be exactly what a lone tiled pane gets). Every value-bearing
    /// flag is a single joined `--flag=value` element (hyphen-immune, see below).
    #[test]
    fn floating_new_pane_args_full_size_borderless() {
        let exec = vec!["miao".to_string(), "claude".to_string()];
        let args = floating_new_pane_args(7, "/tmp/proj", Some("claude: proj"), false, Some(&exec));
        assert_eq!(
            args,
            vec![
                "new-pane",
                "--floating",
                "--tab-id=7",
                "--x=0",
                "--y=0",
                "--width=100%",
                "--height=100%",
                "--borderless=true",
                "--cwd=/tmp/proj",
                "--name=claude: proj",
                "--",
                "miao",
                "claude",
            ]
        );
        // hold:false → --close-on-exit, placed before the argv separator.
        let args = floating_new_pane_args(0, "/", None, true, Some(&exec));
        let close = args.iter().position(|a| a == "--close-on-exit").unwrap();
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert!(close < sep);
        assert!(!args.iter().any(|a| a.starts_with("--name")));

        // A cwd or title basename starting with `-` stays a single joined argv
        // element — zellij's clap would reject it as a stray option if it were
        // passed as a separate value.
        let args = floating_new_pane_args(1, "/tmp/-weird", Some("-x"), false, Some(&exec));
        assert!(args.contains(&"--cwd=/tmp/-weird".to_string()));
        assert!(args.contains(&"--name=-x".to_string()));
        // No bare value element that begins with `-` (the failure mode).
        assert!(!args.iter().any(|a| a == "-x" || a == "/tmp/-weird"));
    }

    /// `resolve_spawn_target` never yields SharedStackTab on zellij (it's the
    /// `window_stacking` backend's Stacked target; zellij floats instead); the
    /// arm bails before touching a subprocess (a `Shell` command reads no env),
    /// so the rejection is exercised without a running server.
    #[tokio::test]
    async fn spawn_rejects_shared_stack_tab() {
        let term = ZellijTerminal {
            session: "test".into(),
            own_pane: None,
            own_tab: Mutex::new(OwnTab::Unknown),
        };
        let spec = SpawnSpec {
            cwd: "/tmp".into(),
            target: SpawnTarget::SharedStackTab,
            command: SpawnCommand::Shell,
            title: None,
            hold: true,
            take_focus: false,
            stack: false,
        };
        let err = term.spawn(spec).await.unwrap_err();
        assert!(err.to_string().contains("SharedStackTab"));
    }
}
