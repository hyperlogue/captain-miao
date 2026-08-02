//! Append-only TSV log of every dashboard keystroke. Off unless `[debug]`
//! mode is on. Format per line:
//!
//! ```text
//! <unix_secs>\tdashboard\t<pid>\t<input_mode>\t<key_repr>\t<action_or_underscore>
//! ```
//!
//! Basic cursor-movement keys (`j/k/h/l`, arrows, `Ctrl+n/p/u/d/f/b`,
//! `PageUp/Down`, `Home/End`, `g`, `Shift+G`) are skipped — they dominate
//! the log and we won't be re-binding them, so they only dilute the
//! frequency stats we actually care about.
//!
//! Lines stay well under the 4 KiB POSIX append-atomicity limit, so the
//! launcher and dashboard can append concurrently without interleaving.
//! Errors are swallowed — instrumentation must never crash the dashboard.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config;
use crate::state::{self, LauncherState};

use super::{Action, InputMode};

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// Rotate the log when it grows past this size so a long-lived debug session
/// can't fill the disk. One generation (`<path>.1`) is kept.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub(super) fn init() {
    SINK.get_or_init(|| {
        if !config::debug_enabled() {
            return None;
        }
        let dir = state::state_dir().join("logs");
        if state::create_dir_all_private(&dir).is_err() {
            return None;
        }
        let path = dir.join(&config::get().debug.keybind_log_file);
        rotate_if_oversized(&path);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(Mutex::new)
    });
}

/// If `path` already exceeds `MAX_LOG_BYTES`, rename it to `<path>.1`
/// (replacing any previous generation) before it's reopened for append.
/// Best-effort: any failure leaves the existing file in place.
fn rotate_if_oversized(path: &std::path::Path) {
    let oversized = std::fs::metadata(path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    if oversized {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, rotated);
    }
}

pub(super) fn record(mode: InputMode, key: KeyEvent, action: Option<&Action>) {
    let Some(Some(sink)) = SINK.get() else { return };
    if is_basic_movement(key) {
        return;
    }
    let line = format!(
        "{ts}\tdashboard\t{pid}\t{mode}\t{key}\t{action}\n",
        ts = LauncherState::now(),
        pid = std::process::id(),
        mode = mode_repr(mode),
        key = key_repr(key),
        action = action.map(|a| a.name()).unwrap_or("_"),
    );
    if let Ok(mut f) = sink.lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

fn is_basic_movement(key: KeyEvent) -> bool {
    let m = key.modifiers;
    let plain = m.is_empty();
    let ctrl_only = m == KeyModifiers::CONTROL;
    let shift_only = m == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Home
        | KeyCode::End => plain,
        KeyCode::Char('j')
        | KeyCode::Char('k')
        | KeyCode::Char('h')
        | KeyCode::Char('l')
        | KeyCode::Char('g') => plain,
        KeyCode::Char('G') => shift_only,
        KeyCode::Char('n')
        | KeyCode::Char('p')
        | KeyCode::Char('u')
        | KeyCode::Char('d')
        | KeyCode::Char('f')
        | KeyCode::Char('b') => ctrl_only,
        _ => false,
    }
}

fn mode_repr(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Normal => "Normal",
        InputMode::Search => "Search",
        InputMode::Picker => "Picker",
        InputMode::Help => "Help",
        InputMode::Confirm => "Confirm",
        InputMode::DirEdit => "DirEdit",
        InputMode::HostEdit => "HostEdit",
    }
}

/// Stable, parseable spelling of a key event. Avoids relying on the
/// `Display` impl of `KeyCode`/`KeyEvent` — those are not guaranteed stable
/// across crossterm versions and would skew frequency stats.
fn key_repr(key: KeyEvent) -> String {
    let mut prefix = String::new();
    let m = key.modifiers;
    if m.contains(KeyModifiers::CONTROL) {
        prefix.push_str("Ctrl+");
    }
    if m.contains(KeyModifiers::ALT) {
        prefix.push_str("Alt+");
    }
    if m.contains(KeyModifiers::SHIFT) {
        prefix.push_str("Shift+");
    }
    let body = match key.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    format!("{prefix}{body}")
}
