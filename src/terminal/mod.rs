//! Terminal-emulator abstraction.
//!
//! captain-miao controls windows and tabs through a [`Terminal`] backend —
//! Kitty (`terminal::kitty`) or zellij (`terminal::zellij`), picked by
//! [`get`]'s zellij-first detection. The trait is the set of *irreducible*
//! per-backend primitives. Everything derivable from a window/tab tree — the
//! window→tab map, the picker's tab summary — lives here as pure functions
//! over a [`snapshot`](Terminal::snapshot), so the policy is written once and
//! unit-tested without any backend.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{self, ConfiguredBackend};

pub mod graphics;
pub mod kitty;
pub mod zellij;

// `WindowId`/`TabId` (serialized into state + on the wire) and the launcher's
// `current_window` self-report live in `cm-core`; the `Terminal` trait, the
// Kitty backend, and the snapshot policy below are dashboard-only. Re-exported
// so `crate::terminal::…` paths across the dashboard resolve unchanged.
pub use cm_core::terminal::{TabId, WindowId, current_terminal_identity, current_window};

/// One node of a terminal [`snapshot`](Terminal::snapshot): a tab and the ids of
/// its windows. A window is just its id — everything captain-miao derives from a
/// snapshot (the window→tab map, tab summaries) needs nothing more.
#[derive(Clone, Debug)]
pub struct Tab {
    pub id: TabId,
    pub title: String,
    pub is_focused: bool,
    pub windows: Vec<WindowId>,
}

/// Flat per-tab summary for the move-to-tab picker (the picker wants a count,
/// not the whole window list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabInfo {
    pub id: TabId,
    pub title: String,
    pub window_count: usize,
    pub is_focused: bool,
}

/// Where a [`spawn`](Terminal::spawn) places its new window.
#[derive(Debug, Clone)]
pub enum SpawnTarget {
    /// A fresh tab.
    NewTab,
    /// A full-size floating pane in the backend's shared sessions tab (zellij:
    /// all sessions stack at identical 100% geometry in one `miao:sessions` tab;
    /// focusing one raises it). Only produced when
    /// [`Capabilities::floating_sessions`] is set; other backends reject it.
    Floating,
    /// A window in the backend's shared `miao:sessions` **stack** tab, created if
    /// absent — the stacking analog of [`Floating`](SpawnTarget::Floating) for a
    /// backend that stacks windows in a tab rather than floating them (Kitty).
    /// Produced only when [`Capabilities::window_stacking`] is set and the
    /// session layout is [`SessionsLayout::Stacked`]; zellij (floating) rejects
    /// it, and per-tab never yields it.
    SharedStackTab,
}

/// Which arrangement new sessions spawn into, toggled at runtime (`Space l`) and
/// persisted. A **spawn-time policy only** — switching does not relocate running
/// sessions (zellij can't reparent a live pane across tabs, so the two backends
/// stay symmetric), and the user migrates existing sessions by restarting them
/// (`Space e`/`Space E`), which respawns each into the current layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionsLayout {
    /// All sessions consolidated in one shared tab, one visible at a time:
    /// floating panes in the `miao:sessions` tab on zellij, a single `miao:sessions`
    /// stack-layout tab on Kitty. The default.
    #[default]
    Stacked,
    /// One session per tab ([`SpawnTarget::NewTab`] on both backends).
    PerTab,
}

impl SessionsLayout {
    /// The header/status label, and the exact string persisted in
    /// `dashboard-overrides.json` (round-tripped by [`from_label`](Self::from_label)).
    pub fn label(self) -> &'static str {
        match self {
            SessionsLayout::Stacked => "stacked",
            SessionsLayout::PerTab => "per-tab",
        }
    }

    /// Parse a persisted/config label back into a layout; `None` for an
    /// unrecognized value (so an old or hand-edited overrides file falls back to
    /// the default rather than failing the whole load).
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "stacked" => Some(SessionsLayout::Stacked),
            "per-tab" => Some(SessionsLayout::PerTab),
            _ => None,
        }
    }

    /// Flip to the other mode (the `Space l` toggle).
    pub fn toggled(self) -> Self {
        match self {
            SessionsLayout::Stacked => SessionsLayout::PerTab,
            SessionsLayout::PerTab => SessionsLayout::Stacked,
        }
    }
}

/// What a [`spawn`](Terminal::spawn) runs.
#[derive(Debug, Clone)]
pub enum SpawnCommand {
    /// The user's default shell.
    Shell,
    /// An explicit argv.
    Exec(Vec<String>),
}

/// Destination tab for [`move_window_to_tab`](Terminal::move_window_to_tab).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabTarget {
    New,
    Existing(TabId),
}

/// A request to create a new window/tab.
pub struct SpawnSpec {
    pub cwd: String,
    pub target: SpawnTarget,
    pub command: SpawnCommand,
    /// Tab title on a [`SpawnTarget::NewTab`] spawn, pane name on a
    /// [`SpawnTarget::Floating`] one, window title on a
    /// [`SpawnTarget::SharedStackTab`] one (whose tab stays `miao:sessions`).
    pub title: Option<String>,
    /// Keep the window open after the child process exits.
    pub hold: bool,
    /// Switch focus to the new window/tab on creation.
    pub take_focus: bool,
    /// Arrange the tab so one window is visible at a time. Honored only by the
    /// Kitty backend on a [`SpawnTarget::NewTab`] spawn (`goto-layout stack`);
    /// the zellij backend never reads it.
    pub stack: bool,
}

/// What a [`spawn`](Terminal::spawn) learned about the window/tab it created.
/// `window` is `None` only when the backend created the target but could not
/// recover the pane id (zellij's `new-tab` pane-id recovery is best-effort — the
/// tab and its command are already running regardless).
///
/// `tab` is reported only when the backend got the id for free: zellij's
/// `new-tab`/`new-pane` print it, and kitty's `SharedStackTab` join already
/// looked the tab up. `None` means "didn't come cheap", never "no tab" — a
/// kitty `NewTab` would need a second `ls`, so it says nothing.
///
/// **A reported `tab` must be the tab the reported `window` is actually in.**
/// The dashboard trusts it as authoritative and seeds `window_tab_cache` with
/// the pair, skipping the `snapshot()` it would otherwise run to learn the same
/// thing (~20ms/pane on zellij). A backend that can't answer precisely must
/// report `None` and let the snapshot resolve it rather than guess.
#[derive(Debug, Clone)]
pub struct SpawnResult {
    pub window: Option<WindowId>,
    pub tab: Option<TabId>,
}

/// Optional operations/arrangements a backend may lack, queried in one place
/// via [`Terminal::capabilities`]. Constants per backend — no IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// [`move_window_to_tab`](Terminal::move_window_to_tab) is a real
    /// operation. zellij has no CLI to reparent a pane across tabs
    /// (`BreakPane` is keybind/plugin-only), so the dashboard hides the `t`
    /// affordance rather than offer a key that only errors.
    pub move_to_tab: bool,
    /// Several session windows sharing one tab is a good arrangement (Kitty's
    /// stack layout shows one full-size window with the rest hidden at zero
    /// cost). When `false`, the dashboard gives every session a tab of its
    /// own — zellij's stacked panes cost a title-bar row per collapsed
    /// session and a pty resize (a slow agent repaint) on every switch.
    pub window_stacking: bool,
    /// The backend hosts sessions as full-size floating panes stacked at
    /// identical geometry in one shared sessions tab
    /// ([`SpawnTarget::Floating`]), which the dashboard prefers over both
    /// tab arrangements: switching is a pure z-order raise (no pty
    /// resize, no flicker), the tab bar stays one tab, and spawning into the
    /// non-active sessions tab moves nothing at all. zellij only; Kitty has
    /// no floating panes.
    pub floating_sessions: bool,
}

impl Default for Capabilities {
    /// Kitty's answer; backends opt in/out per field.
    fn default() -> Self {
        Self {
            move_to_tab: true,
            window_stacking: true,
            floating_sessions: false,
        }
    }
}

/// A terminal-emulator backend. The methods are the primitives that genuinely
/// differ per backend; derived reads are free functions in this module.
#[async_trait]
pub trait Terminal: Send + Sync {
    /// The window the current process is running in, from the backend's env var
    /// (Kitty: `KITTY_WINDOW_ID`). `None` outside a managed window.
    fn current_window(&self) -> Option<WindowId>;

    /// The instance identity of the terminal this backend *drives* (same
    /// `zellij:<session>` / `kitty:<socket>` forms launchers stamp via
    /// cm-core). Deliberately not the ambient-env identity: under the
    /// `[terminal] backend = "kitty"` override inside a nested zellij, the
    /// process *sits* in a zellij pane but *drives* the outer Kitty — and the
    /// windows it spawns (what identity scoping protects) live in the driven
    /// terminal.
    fn identity(&self) -> Option<String>;

    /// Prove this backend can actually *drive* its terminal instance — the
    /// startup half of detection: [`supported_terminal_present`] answers "is a
    /// terminal we know how to drive present?" from the env alone, this answers
    /// "does the control channel to it work?" with a real round-trip.
    ///
    /// Default: nothing to prove. That is zellij's answer — its `zellij action`
    /// CLI is trusted by the session it runs in, so there is no socket or
    /// password to get wrong (contrast Kitty, which overrides this: its whole
    /// channel is a socket + password pair spread across two config files, and a
    /// mismatch there doesn't even fail — it hangs).
    async fn verify_control(&self) -> Result<()> {
        Ok(())
    }

    /// The full window/tab tree (Kitty: parsed `kitten @ ls`).
    async fn snapshot(&self) -> Result<Vec<Tab>>;

    /// Create a window/tab per `spec`; returns what the spawn learned about the
    /// window/tab it created (see [`SpawnResult`]).
    async fn spawn(&self, spec: SpawnSpec) -> Result<SpawnResult>;

    /// Focus the window `id`.
    async fn focus_window(&self, id: &WindowId) -> Result<()>;

    /// Focus (switch to) the tab `id`.
    async fn focus_tab(&self, id: &TabId) -> Result<()>;

    /// Close the window `id`.
    ///
    /// Callers may invoke this **speculatively**, on an id whose window may
    /// already be gone (the restart/kill paths do, rather than pay a
    /// `snapshot()` to check first). So closing a dead id must be harmless —
    /// either a silent no-op (zellij's `close-pane` exits 0) or a plain error
    /// the caller ignores (kitty's `close-window` finds no match). Both current
    /// backends also have **non-recycling** window ids within an instance, which
    /// is what makes speculative close safe at all; a backend that recycled ids
    /// would need the caller to re-verify identity first.
    async fn close_window(&self, id: &WindowId) -> Result<()>;

    /// Capture the last `max_lines` lines of `id`'s output, styled (with SGR
    /// codes), anchored at the live bottom. Backends that support a line range
    /// fetch only that many; Kitty fetches the full scrollback and tails it.
    async fn capture_text(&self, id: &WindowId, max_lines: usize) -> Result<String>;

    /// Move window `id` into another tab.
    async fn move_window_to_tab(&self, id: &WindowId, to: TabTarget) -> Result<()>;

    /// What this backend can do beyond the required primitives, as one
    /// [`Capabilities`] value (per-flag rationale on its fields). A constant,
    /// not IO, hence sync; the default claims everything (Kitty).
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
}

// ---- backend-neutral policy over a snapshot (pure, unit-testable) ----

/// Map every window in a snapshot to the tab that contains it. The pure policy
/// the dashboard uses to resolve each local session's (display-only) `tab_id` —
/// this used to live in the launcher, but window/tab lookup is a presentation
/// concern and a launcher may be headless/remote (`docs/remote-sessions.md` §3),
/// so it moved here, keyed off the snapshot the dashboard already owns.
pub fn window_tab_map(tabs: &[Tab]) -> HashMap<WindowId, TabId> {
    let mut map = HashMap::new();
    for tab in tabs {
        for win in &tab.windows {
            map.insert(win.clone(), tab.id.clone());
        }
    }
    map
}

/// Flatten a snapshot into the picker's per-tab summaries.
pub fn list_tabs(tabs: &[Tab]) -> Vec<TabInfo> {
    tabs.iter()
        .map(|t| TabInfo {
            id: t.id.clone(),
            title: t.title.clone(),
            window_count: t.windows.len(),
            is_focused: t.is_focused,
        })
        .collect()
}

/// Validate a window/tab id as bare ASCII digits, failing closed with a
/// `backend`-named error. captain-miao's real ids are always non-negative
/// integers and the only untrusted source is its own state files, so anything
/// else is rejected rather than mis-targeted. The backend wrappers
/// ([`kitty::match_id`], [`zellij::digits_id`]) carry the per-backend rationale.
fn validate_id<'a>(id: &'a str, backend: &str) -> Result<&'a str> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        anyhow::bail!("refusing {backend} id with non-numeric characters: {id:?}");
    }
}

/// Return the suffix of `s` containing at most the last `n` lines. Shared by
/// backends whose capture primitive has no "last N lines" flag (both Kitty's
/// `get-text` and zellij's `dump-screen` return the full scrollback).
fn tail_lines(s: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    // The (n-1)th newline from the end leaves exactly n lines after it.
    s.match_indices('\n')
        .rev()
        .nth(n - 1)
        .map(|(i, _)| &s[i + 1..])
        .unwrap_or(s)
}

// ---- global accessor (mirrors config::get()) ----

static BACKEND: OnceLock<Box<dyn Terminal>> = OnceLock::new();

/// Which backend `get()` should build: a config override wins, then a live
/// zellij session, then Kitty as the status-quo fallback. Pure (env reads stay
/// at the `get()` edge) so the precedence is unit-tested without touching the
/// process-global env or the `OnceLock`.
///
/// Zellij must beat the ambient Kitty env: when zellij runs nested inside Kitty,
/// every zellij pane inherits the outer `KITTY_WINDOW_ID`, so a Kitty backend
/// would drive the wrong (outer) window. Only an explicit override overrides that.
fn detect_backend(over: Option<ConfiguredBackend>, in_zellij: bool) -> ConfiguredBackend {
    match over {
        Some(b) => b,
        None if in_zellij => ConfiguredBackend::Zellij,
        None => ConfiguredBackend::Kitty,
    }
}

/// Whether captain-miao is running inside a terminal it can actually drive,
/// honoring the `[terminal] backend` override — the one detection owner for the
/// startup gate. It mirrors what [`get`] resolves to (including `get`'s
/// chosen-zellij-but-no-session fallback to Kitty), reusing
/// [`ZellijTerminal::from_env`](zellij::ZellijTerminal::from_env)'s trim/empty
/// filter so the gate can't disagree with the runtime backend the way a raw
/// `ZELLIJ_SESSION_NAME`/`KITTY_PID` presence check did. Kitty is "present" when
/// it exported its process env (`KITTY_PID`).
pub fn supported_terminal_present() -> bool {
    let in_zellij = zellij::ZellijTerminal::from_env().is_some();
    match detect_backend(config::get().terminal.backend, in_zellij) {
        // `get()` builds the zellij backend only when a session is live; when it
        // isn't (config pinned zellij outside one) `get()` falls back to Kitty,
        // so the gate then requires Kitty just like the Kitty arm below.
        ConfiguredBackend::Zellij if in_zellij => true,
        ConfiguredBackend::Zellij | ConfiguredBackend::Kitty => {
            std::env::var_os("KITTY_PID").is_some()
        }
    }
}

/// Verify the backend detection settled on can actually drive its terminal —
/// the second half of the startup gate, kept beside the first so both live with
/// the detection they depend on. [`supported_terminal_present`] picks the
/// backend from the env; this asks *that* backend to prove the control channel
/// works ([`Terminal::verify_control`]), which is a no-op for zellij and a real
/// `kitten @` round-trip for Kitty.
///
/// The error is a ready-to-print, multi-line diagnosis naming the fix, because
/// its only caller (the dashboard, in `main`) prints it and exits: a dashboard
/// that can't reach kitty can't spawn, focus, preview, or move a window, and a
/// wrong password would freeze it on the first of those rather than fail it.
pub async fn verify_control() -> Result<()> {
    get().verify_control().await
}

/// The process-wide terminal backend, constructed once on first use. Detection
/// order is [`detect_backend`]; the `get()` edge supplies the env signal and
/// falls back to Kitty if zellij is chosen but no zellij session is actually
/// present (`get()` must always return a backend).
pub fn get() -> &'static dyn Terminal {
    &**BACKEND.get_or_init(|| {
        // Build the zellij backend up front so `is_some()` is the single
        // in-zellij signal (`from_env` already filters an empty session name).
        let zellij = zellij::ZellijTerminal::from_env();
        match detect_backend(config::get().terminal.backend, zellij.is_some()) {
            ConfiguredBackend::Zellij => match zellij {
                Some(z) => Box::new(z) as Box<dyn Terminal>,
                // Only reachable when the config forced zellij with no session
                // live; keep the process running by falling back to Kitty.
                None => {
                    tracing::warn!(
                        "[terminal] backend = \"zellij\" but ZELLIJ_SESSION_NAME is unset; \
                         falling back to Kitty"
                    );
                    Box::new(kitty::KittyTerminal)
                }
            },
            ConfiguredBackend::Kitty => Box::new(kitty::KittyTerminal),
        }
    })
}

#[cfg(test)]
mod tests;
