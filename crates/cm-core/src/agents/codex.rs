//! Codex CLI backend. Owns every Codex-specific path, rollout JSON shape, and
//! hook event mapping. The dashboard reaches everything in here only via
//! `crate::agent::AgentControl::Codex`'s match arms.
//!
//! Codex's hook system is a near-clone of Claude Code's: the same event names
//! (minus a few) and an identical snake_case stdin payload, so the launcher
//! loop and `HookMessage` are reused unchanged. The two genuine differences
//! are (1) a command hook has to be **trusted** from a real config layer, so we
//! install one owned `captain-miao` profile in the user's real `CODEX_HOME` and
//! select it on every managed launch; and (2) Codex records a far richer rollout
//! JSONL than Claude's transcript, so context tokens and lifecycle signals come
//! straight from typed events.
//!
//! We write only our inline hooks and their trust hashes into it; the user's
//! `config.toml` and global `hooks.json` are never parsed or changed, and bare
//! Codex runs never load the profile. This became possible when Codex 0.134
//! moved named profiles into separate `<name>.config.toml` layers. A profile
//! does not compose with another named profile, so `--profile` / `-p` is
//! deliberately reserved on a captain-miao launch and rejected before Codex
//! starts.
//!
//! The file is not ours alone, though: **Codex persists into the profile it was
//! launched with**, not into the base config, so a managed session's answers —
//! directory trust, a `/model` change — land there. Refreshing the profile is
//! therefore a merge, never a rewrite ([`build_profile`]). The user-visible
//! corner of that: a directory trusted inside a managed session stays untrusted
//! for a bare `codex` run, and vice versa.
//!
//! Trust cannot be moved to `-c` beside an injected hook. Codex registers that
//! definition under `HookSource::sessionFlags`, but ignores a trust entry passed
//! through the same ephemeral layer; an untrusted hook is skipped and the row
//! never leaves `Starting`. Nor do we use `--dangerously-bypass-hook-trust`,
//! because that would disable the gate for the user's own hooks too. The owned
//! profile instead gives both the hook and its precomputed trust hash one real,
//! writable config layer without touching the base config.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::process::Command;

use super::common;
use super::synth_home::atomic_write;
use super::{collapse_whitespace, shell_quote};
use crate::agent::{
    AgentActivity, ResumeCandidate, ResumeMetadata, SessionIndex, SessionIndexCache,
    TranscriptScan, TranscriptStats,
};
use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "codex";

/// Codex's composer recognizes a bracketed paste containing an image path as
/// an attachment, retaining the current draft. Verified against 0.153.4's
/// `ChatComposer::handle_paste_image_path` and the real TUI. A file URL keeps
/// whitespace, quotes, non-UTF-8 bytes and terminal controls out of the input
/// stream; Codex decodes it back to a host path before reading the image.
pub(crate) fn clipboard_paste_input(path: &Path) -> Vec<u8> {
    use std::fmt::Write;
    use std::os::unix::ffi::OsStrExt;

    let mut input = String::from("\x1b[200~file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            input.push(char::from(byte));
        } else {
            let _ = write!(input, "%{byte:02X}");
        }
    }
    input.push_str("\x1b[201~");
    input.into_bytes()
}

/// Codex 0.153.4 pushes keyboard enhancements before querying support, on the
/// primary screen and regardless of TERM. Mirror its opt-out at launch, where
/// the environment is still the agent's; an attaching client cannot recover
/// that decision from its own environment. See upstream `tui/keyboard_modes.rs`.
pub(crate) fn uses_kitty_keyboard() -> bool {
    keyboard_enhancement_enabled(
        std::env::var("CODEX_TUI_DISABLE_KEYBOARD_ENHANCEMENT")
            .ok()
            .as_deref(),
        running_in_vscode_wsl,
    )
}

fn keyboard_enhancement_enabled(disable: Option<&str>, vscode_wsl: impl FnOnce() -> bool) -> bool {
    match disable.map(str::trim) {
        Some(value)
            if value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes") =>
        {
            false
        }
        Some(value)
            if value == "0"
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no") =>
        {
            true
        }
        _ => !vscode_wsl(),
    }
}

fn running_in_vscode_wsl() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let wsl = std::fs::read_to_string("/proc/version")
        .ok()
        .is_some_and(|version| {
            let version = version.to_ascii_lowercase();
            version.contains("microsoft") || version.contains("wsl")
        })
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some();
    if !wsl {
        return false;
    }
    if std::env::var("TERM_PROGRAM").is_ok_and(|term| term.eq_ignore_ascii_case("vscode")) {
        return true;
    }
    // WSL interop can hide TERM_PROGRAM from the Linux environment. Codex
    // also checks the Windows side; do so only at launch and only under WSL.
    std::process::Command::new("cmd.exe")
        .args(["/d", "/s", "/c", "set TERM_PROGRAM"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.trim_end_matches('\r')
                    .strip_prefix("TERM_PROGRAM=")
                    .is_some_and(|term| term.eq_ignore_ascii_case("vscode"))
            })
        })
}

/// The common keyboard flags Codex requests on every supported transport:
/// disambiguate escape codes + report alternate keys (`CSI > 5 u`). Avoid
/// report-event-types: Codex itself omits it for Ghostty, iTerm2 and tmux's
/// xterm key format, and a reattach can land in a different emulator. This
/// preserves modified keys without reproducing Codex's terminal detection or
/// querying a tmux server on the attach path. Verified against 0.153.4 startup
/// bytes and `tui/keyboard_modes.rs`; no mouse tracking belongs to its inline
/// view. Reset modifyOtherKeys so it cannot compete with CSI-u reporting.
pub(crate) fn reattach_prime(kitty_keyboard: bool) -> crate::state::ReattachPrime {
    crate::state::ReattachPrime {
        input_modes: true,
        keyboard_flags: if kitty_keyboard { 5 } else { 0 },
        reset_modify_other_keys: kitty_keyboard,
        ..crate::state::ReattachPrime::default()
    }
}

/// The one named-profile slot captain-miao reserves on managed Codex launches.
const PROFILE_NAME: &str = "captain-miao";
const PROFILE_FILE: &str = "captain-miao.config.toml";
/// First line of the file and the whole ownership probe, so it has to stay
/// byte-stable: every profile already on disk would read as someone else's the
/// moment this string changes, and a launch refuses those.
const PROFILE_MARKER: &str = "# Managed by captain-miao; changes are overwritten.\n";
/// What a refresh actually writes. The second line is the honest version of
/// the first: [`build_profile`] regenerates only the `hooks` tables.
const PROFILE_HEADER: &str = concat!(
    "# Managed by captain-miao; changes are overwritten.\n",
    "# Only [hooks] is regenerated; Codex's own writes here are preserved.\n",
);

// =============================================================================
// Filesystem locations
// =============================================================================

/// The real Codex home — `$CODEX_HOME` if the user set one globally, else
/// `~/.codex`. Resolve a relative override now and hand the same absolute path
/// back to Codex at launch, so creating the profile and loading it cannot
/// disagree when the agent's cwd differs from the launcher's.
fn codex_home() -> Option<PathBuf> {
    resolve_codex_home(
        std::env::var_os("CODEX_HOME").map(PathBuf::from),
        dirs::home_dir(),
        std::env::current_dir().ok(),
    )
}

/// Home resolution split from environment reads so it is testable without
/// mutating process-global variables. Empty overrides are unset; relative ones
/// resolve against the launcher's cwd and require that cwd to be available.
fn resolve_codex_home(
    configured: Option<PathBuf>,
    home: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = configured
        && !path.as_os_str().is_empty()
    {
        return if path.is_absolute() {
            Some(path)
        } else {
            Some(cwd?.join(path))
        };
    }
    home.map(|h| h.join(".codex"))
}

fn codex_path(name: &str) -> Option<PathBuf> {
    Some(codex_home()?.join(name))
}

fn read_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

// =============================================================================
// Session-name index
// =============================================================================

/// Codex has no per-pid session-name manifest like Claude's
/// `~/.claude/sessions/<pid>.json`; a session's identity is the rollout UUID,
/// which the launcher learns from every hook payload and stores on
/// `state.session_id`. Names — both user renames and Codex's own auto-titles —
/// live in `state_5.sqlite`, read by the **per-host title overlay** in
/// [`crate::backend::LocalBackend`] (one cached reader per host, keyed by
/// session id — see [`read_thread_titles`]) and stamped onto
/// `LauncherState.name`, so the title reaches the dashboard exactly like
/// Claude's, local *and* remote. This index therefore stays empty: the name
/// reaches `session_display_name` via `name`, not here.
pub fn read_session_index(_cache: &mut SessionIndexCache) -> SessionIndex {
    SessionIndex::default()
}

// =============================================================================
// Thread titles (state_5.sqlite) — renames + Codex's own auto-titles
// =============================================================================

fn state_db_path() -> Option<PathBuf> {
    codex_path("state_5.sqlite")
}

/// The title store's WAL sidecar — the file whose change means "a title may
/// have moved". `state_5` runs in WAL mode: writes land here (the main db
/// updates only on checkpoint), so this is what the host's out-of-band watcher
/// registers. Watching just this file, not `~/.codex`, keeps the churny
/// `logs_2.sqlite-wal` telemetry sibling from waking anything. If the WAL is
/// momentarily absent (before the first write or just after a checkpoint), the
/// watch fails silently and the title overlay refreshes on the next session
/// event instead.
pub fn title_watch_path() -> Option<PathBuf> {
    // Derive it beside `state_5.sqlite` rather than resolving the sidecar on its
    // own: a checkpoint regularly deletes the wal, and an absent sidecar must
    // still have a stable path for the watcher to register.
    Some(state_db_path()?.with_file_name("state_5.sqlite-wal"))
}

/// Stat stamp of the title store — `(main db mtime, wal mtime)` — the cheap
/// change gate the per-host overlay checks before touching sqlite. Writes land
/// in the WAL, and a checkpoint folds them into the main DB (possibly deleting
/// it). `None` per missing/unstattable file.
pub fn title_store_mtimes() -> (Option<SystemTime>, Option<SystemTime>) {
    fn mtime(path: Option<PathBuf>) -> Option<SystemTime> {
        std::fs::metadata(path?).ok()?.modified().ok()
    }
    (mtime(state_db_path()), mtime(title_watch_path()))
}

/// Batch-read the current titles for `ids` from `state_5.sqlite` over one
/// read-only connection. Called by the per-host overlay in
/// [`crate::backend::LocalBackend`] — a single cached reader serving every
/// Codex session on the host. Returns only the ids that have a (non-empty)
/// title; an absent id simply has no title row yet. Empty on any open failure
/// (the overlay tries again next pass).
pub fn read_thread_titles(ids: &[String]) -> HashMap<String, String> {
    let Some(db) = state_db_path() else {
        return HashMap::new();
    };
    read_thread_titles_at(&db, ids)
}

fn read_thread_titles_at(db: &Path, ids: &[String]) -> HashMap<String, String> {
    if ids.is_empty() {
        return HashMap::new();
    }
    // Read-only so the live WAL DB is never written. URI + NO_MUTEX match
    // rusqlite's default open flags (we only swap READ_WRITE|CREATE for
    // READ_ONLY); the connection is single-threaded and short-lived.
    let Ok(conn) = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return HashMap::new();
    };
    query_thread_titles(&conn, ids)
}

/// The batch lookup against an open connection, split from the IO so it's
/// testable against an in-memory DB.
fn query_thread_titles(conn: &Connection, ids: &[String]) -> HashMap<String, String> {
    ids.iter()
        .filter_map(|id| Some((id.clone(), query_thread_title(conn, id)?)))
        .collect()
}

/// A rename and an auto-title are **two different columns**, and the rename is
/// the later addition: `threads.title` is Codex's own — seeded from the first
/// user message and refined by its titler — while `/rename` writes
/// `threads.name`, added by a migration alongside `is_pinned` and
/// `thread_section_id`. Reading `title` alone therefore shows the auto-title
/// forever, however many times the user renames.
///
/// `COALESCE(NULLIF(TRIM(name), ''), title)` prefers the rename and keeps the
/// auto-title as the fallback, which is what the dashboard wants either way:
/// a session the user has not named still reads better as Codex's summary than
/// as nothing. The SQL `TRIM` only guards the NULLIF — the real cleaning is
/// [`collapse_whitespace`] below.
const THREAD_TITLE_SQL: &str =
    "SELECT COALESCE(NULLIF(TRIM(name), ''), title) FROM threads WHERE id = ?1 LIMIT 1";

/// The same lookup against a Codex predating the `name` column. Tried only when
/// the query above fails to prepare, so an older install degrades to
/// auto-titles instead of to no titles at all.
const THREAD_TITLE_SQL_LEGACY: &str = "SELECT title FROM threads WHERE id = ?1 LIMIT 1";

/// Run the title lookup against an open connection and clean the result. Split
/// out so the cleaning/empty-handling logic is testable against an in-memory DB
/// without touching the real `state_5.sqlite`. The id is passed as a bound
/// parameter, so it can never alter the query regardless of its contents (no
/// shape validation needed). Returns None when the row is missing, both columns
/// are SQL NULL, or the value collapses to empty whitespace.
fn query_thread_title(conn: &Connection, session_id: &str) -> Option<String> {
    let lookup = |sql: &str| {
        conn.query_row(sql, [session_id], |row| row.get::<_, Option<String>>(0))
            .optional()
    };
    let title: Option<String> = match lookup(THREAD_TITLE_SQL) {
        Ok(found) => found,
        // No `name` column — this database is older than the rename feature.
        Err(_) => lookup(THREAD_TITLE_SQL_LEGACY).ok()?,
    }
    .flatten();
    let clean = collapse_whitespace(title?.trim());
    if clean.is_empty() {
        return None;
    }
    // Cap so a huge first-message title (Codex's default before a rename)
    // doesn't bloat the cache or the wire; the dashboard truncates for display.
    Some(clean.chars().take(200).collect())
}

// =============================================================================
// Goal store (threads that re-drive themselves)
// =============================================================================

/// Whether this thread will open its **own** next turn once the current one
/// ends — i.e. whether its `Stop` hook is a turn boundary rather than rest.
///
/// Codex's `/goal` puts a standing objective on a thread and re-drives it after
/// every turn: `task_complete`, then a `<codex_internal_context source="goal">`
/// user message and `task_started` ~10ms later. **No hook announces that
/// turn** — Codex's hook set has no turn-start event at all (its app-server
/// schema enumerates the eleven it does have: the tool pair, the compact pair,
/// permission, session start/end, subagent start/stop, prompt submit and stop)
/// — so a row the Stop hook parked at Idle sits there, mid-work, until the new
/// turn happens to call a tool. Asking the store at the moment the launcher
/// would park the row is what keeps it off Idle without anything having to
/// watch a file at rest (the launcher confirms the hold once, since Codex
/// decides whether to continue *after* it runs the stop hooks — see
/// `launcher`'s `confirm_hold_at`).
///
/// The store is Codex's own `goals_1.sqlite`, read-only from the real Codex
/// home. A thread re-drives itself when its goal is `active` **and** its
/// continuation isn't deferred —
/// Codex's other statuses (`paused`, `blocked`, `usage_limited`,
/// `budget_limited`, `complete`) all mean the next move is the user's.
///
/// Answers `false` to everything it cannot read: no store, no row, a table
/// renamed under a later `goals_N`. That is exactly the pre-goal behaviour —
/// the row parks at Idle and the rollout's own `task_started` promotes it back
/// a beat later ([`scan_transcript_signals`]) — so drift here costs latency,
/// never a wrong answer that sticks.
pub fn thread_self_continues(session_id: &str) -> bool {
    let Some(db) = goals_db_path() else {
        return false;
    };
    // Read-only, matching `read_thread_titles`: the live WAL db is Codex's to
    // write. Deliberately *not* `immutable`, which would skip the -wal and read
    // a goal set minutes ago as absent.
    let Ok(conn) = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return false;
    };
    query_thread_self_continues(&conn, session_id)
}

fn goals_db_path() -> Option<PathBuf> {
    codex_path("goals_1.sqlite")
}

/// The lookup against an open connection, split from the IO so it is testable
/// against an in-memory db. The id is a bound parameter, so it can never alter
/// the query whatever it contains.
fn query_thread_self_continues(conn: &Connection, session_id: &str) -> bool {
    conn.query_row(SELF_CONTINUES_SQL, [session_id], |_| Ok(()))
        .optional()
        .unwrap_or(None)
        .is_some()
}

/// A row comes back only for a thread that will re-drive itself: the goal is
/// `active` and no continuation deferral is parked against it (Codex records
/// "not this turn" as a row in the second table rather than as a status).
const SELF_CONTINUES_SQL: &str = "SELECT 1 FROM thread_goals g \
     LEFT JOIN thread_goal_continuation_deferrals d ON d.thread_id = g.thread_id \
     WHERE g.thread_id = ?1 AND g.status = 'active' AND d.thread_id IS NULL LIMIT 1";

// =============================================================================
// Rollout reading
// =============================================================================

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(default)]
    total_tokens: u64,
}
#[derive(Deserialize)]
struct TokenInfo {
    #[serde(default)]
    last_token_usage: Option<TokenUsage>,
}

/// Context-token total and model, from one pass over the rollout tail.
///
/// Context tokens: Codex emits `event_msg/token_count` events whose
/// `info.last_token_usage.total_tokens` is the size of the most recent request
/// — i.e. how full the context window currently is. We take the last such event
/// in the tail; unlike Claude there's no compaction estimate to do because Codex
/// reports the post-compaction total directly on the next turn.
///
/// Model: Codex writes a `turn_context` event per turn whose `payload.model`
/// names the active model (e.g. `gpt-5.5`); the model can change between turns,
/// so last-wins. None before the first turn.
pub fn read_transcript_stats(path: &Path, prior: Option<&TranscriptStats>) -> TranscriptStats {
    // First prompt is first-wins and stable, so reuse a previously folded one
    // and only read the rollout head when we don't have it yet.
    let first_prompt = prior
        .and_then(|p| p.first_prompt.clone())
        .or_else(|| read_first_user_prompt(path));
    let Some(tail) = common::read_tail(path) else {
        return TranscriptStats {
            first_prompt,
            ..TranscriptStats::default()
        };
    };
    let mut last_tokens: Option<u64> = None;
    let mut last_model: Option<String> = None;
    for line in tail.split('\n') {
        let is_token_count = line.contains("\"token_count\"");
        let is_turn_context = line.contains("\"turn_context\"");
        if !is_token_count && !is_turn_context {
            continue;
        }
        let Ok(val): std::result::Result<serde_json::Value, _> = serde_json::from_str(line) else {
            continue;
        };
        match val.get("type").and_then(|t| t.as_str()) {
            Some("event_msg") => {
                let Some(payload) = val.get("payload") else {
                    continue;
                };
                if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
                    continue;
                }
                let Some(info) = payload.get("info") else {
                    continue;
                };
                let Ok(info): std::result::Result<TokenInfo, _> =
                    serde_json::from_value(info.clone())
                else {
                    continue;
                };
                if let Some(usage) = info.last_token_usage
                    && usage.total_tokens > 0
                {
                    last_tokens = Some(usage.total_tokens);
                }
            }
            Some("turn_context") => {
                if let Some(model) = val.pointer("/payload/model").and_then(|m| m.as_str())
                    && !model.is_empty()
                {
                    last_model = Some(model.to_string());
                }
            }
            _ => {}
        }
    }
    TranscriptStats {
        context_tokens: last_tokens,
        model: last_model,
        first_prompt,
        name: None,
        last_prompt: None,
        context_window: None,
        cwd: None,
        // Codex recomputes from a bounded tail each refresh — no incremental
        // cursor to carry.
        cursor: None,
    }
}

/// First real user prompt — the `event_msg/user_message` payload's `message`.
/// Used as the fallback display title before any rename.
pub fn read_first_user_prompt(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(400).map_while(Result::ok) {
        if !line.contains("\"user_message\"") {
            continue;
        }
        if let Some(p) = parse_user_message(&line) {
            return Some(p);
        }
    }
    None
}

/// Pull a cleaned prompt out of one rollout line if it is an
/// `event_msg/user_message`. Skips empty / whitespace-only messages.
fn parse_user_message(line: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(line).ok()?;
    if val.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = val.get("payload")?;
    if payload.get("type").and_then(|t| t.as_str()) != Some("user_message") {
        return None;
    }
    let msg = payload.get("message").and_then(|m| m.as_str())?;
    let cleaned = collapse_whitespace(msg.trim());
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Scan `sessions/**/rollout-*.jsonl` for resumable Codex sessions. Returns up
/// to `limit` candidates sorted by mtime (most recent first). Titles come from
/// the host's Codex store, just like the live-session overlay: rollouts do not
/// carry renames, and a title must survive even when no prompt is in the header.
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let home = codex_home().ok_or_else(|| anyhow::anyhow!("no codex home"))?;
    list_resumable_at(&home, limit)
}

fn list_resumable_at(home: &Path, limit: usize) -> Result<Vec<ResumeCandidate>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let root = home.join("sessions");

    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for year in read_subdirs(&root) {
        for month in read_subdirs(&year) {
            for day in read_subdirs(&month) {
                let Ok(entries) = std::fs::read_dir(&day) else {
                    continue;
                };
                for tr in entries.flatten() {
                    let path = tr.path();
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                        continue;
                    }
                    let Ok(meta) = tr.metadata() else { continue };
                    let Ok(mtime) = meta.modified() else {
                        continue;
                    };
                    files.push((path, mtime));
                }
            }
        }
    }
    files.sort_by_key(|b| std::cmp::Reverse(b.1));

    let mut out = Vec::with_capacity(limit.min(files.len()));
    for (path, mtime) in files {
        let header = read_rollout_header(&path);
        let (Some(session_id), Some(cwd)) = (header.session_id, header.cwd) else {
            continue;
        };
        out.push(ResumeCandidate {
            agent: crate::agent::AgentControl::Codex,
            session_id,
            cwd,
            first_prompt: header.first_prompt,
            custom_title: None,
            git_branch: header.git_branch,
            mtime,
        });
        if out.len() == limit {
            break;
        }
    }
    let ids = out.iter().map(|c| c.session_id.clone()).collect::<Vec<_>>();
    let titles = read_thread_titles_at(&home.join("state_5.sqlite"), &ids);
    for candidate in &mut out {
        candidate.custom_title = titles.get(&candidate.session_id).cloned();
    }
    Ok(out)
}

#[derive(Debug, Default)]
struct RolloutHeader {
    session_id: Option<String>,
    cwd: Option<String>,
    first_prompt: Option<String>,
    git_branch: Option<String>,
}

/// Read the `session_meta` (first line) plus the first `user_message` to build
/// a resume candidate without parsing the whole rollout.
fn read_rollout_header(path: &Path) -> RolloutHeader {
    use std::io::{BufRead, BufReader};
    let mut header = RolloutHeader::default();
    let Ok(file) = std::fs::File::open(path) else {
        return header;
    };
    let reader = BufReader::new(file);
    for line in reader.lines().take(400).map_while(Result::ok) {
        if header.session_id.is_none() && line.contains("\"session_meta\"") {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line)
                && let Some(p) = val.get("payload")
            {
                header.session_id = p.get("id").and_then(|v| v.as_str()).map(str::to_string);
                header.cwd = p.get("cwd").and_then(|v| v.as_str()).map(str::to_string);
                header.git_branch = p
                    .pointer("/git/branch")
                    .and_then(|v| v.as_str())
                    .filter(|b| !b.is_empty())
                    .map(str::to_string);
            }
        } else if header.first_prompt.is_none() && line.contains("\"user_message\"") {
            header.first_prompt = parse_user_message(&line);
        }
        if header.session_id.is_some() && header.first_prompt.is_some() {
            break;
        }
    }
    header
}

// =============================================================================
// Launcher: process spawn + owned Codex profile
// =============================================================================

/// Codex can defer `SessionStart` until immediately before the first prompt.
/// A promptless `resume <id>` already identifies the idle session, so recover
/// its persisted facts without waiting for that hook. Titles remain the host
/// overlay's job; this read supplies the id it needs plus the saved details.
pub fn read_resume_metadata(args: &[String]) -> Option<ResumeMetadata> {
    read_resume_metadata_at(&codex_home()?, args)
}

pub(crate) fn read_resume_metadata_at(home: &Path, args: &[String]) -> Option<ResumeMetadata> {
    // Accept exactly the argv our resume/restart planner emits. A fork mints a
    // different id, `--last`/names need Codex's resolution, and extra arguments
    // may include an initial prompt: none can be assumed to be an idle resume.
    let [subcommand, id] = args else { return None };
    if subcommand != "resume" || id.is_empty() || id.starts_with('-') {
        return None;
    }
    let conn = Connection::open_with_flags(
        home.join("state_5.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let path: String = conn
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .ok()?;
    let path = Path::new(&path);
    let header = read_rollout_header(path);
    // An unavailable or mismatched rollout is no evidence. In particular, a
    // stale index entry must never stamp some other thread onto this launcher.
    if header.session_id.as_deref() != Some(id.as_str()) {
        return None;
    }
    Some(ResumeMetadata {
        session_id: id.clone(),
        stats: read_transcript_stats(path, None),
    })
}

/// Build the argv for a Codex session. Codex will not load the launcher's JSON
/// directly, so the event table is rewritten as the inline TOML of a profile we
/// own — which is also why that profile's trust hashes must stay stable.
pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    let args = managed_launch_args(extra_args)?;

    // The generic launcher already wrote the backend's hook payload to this
    // per-session path. Codex does not load that path directly: turn its JSON
    // event table into the inline TOML carried by our named profile.
    let hooks_json =
        std::fs::read_to_string(settings_path).context("reading codex hooks settings")?;
    let home = codex_home().context("could not resolve Codex home")?;
    ensure_profile_at(&home, &hooks_json)?;

    // The helper remains available on PATH. The pool handles Ctrl+V through
    // `clipboard_paste_input`, since Codex's native read bypasses these shims.
    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    // Set the real path explicitly. This is a no-op for the normal absolute
    // `$CODEX_HOME`, and makes a relative override resolve the same way here as
    // it did while writing the profile above.
    cmd.env("CODEX_HOME", &home);
    // The hook subprocess reads the launcher socket from here rather than from
    // an argv flag — that keeps the profile byte-identical across sessions so
    // its trust hash never changes.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    // Root options, so they must precede Codex's `resume` / `fork` subcommands
    // in `extra_args`. The profile pre-trusts only our hooks; the CLI feature
    // gate keeps a project config from disabling them for this managed launch.
    // No bypass flag weakens the user's other hook sources.
    cmd.args(args);
    Ok(cmd)
}

fn managed_launch_args(extra_args: &[String]) -> Result<Vec<String>> {
    reject_profile_arg(extra_args)?;
    let mut args = Vec::with_capacity(extra_args.len() + 4);
    args.push("--profile".to_string());
    args.push(PROFILE_NAME.to_string());
    args.push("--enable".to_string());
    args.push("hooks".to_string());
    args.extend_from_slice(extra_args);
    Ok(args)
}

/// Reject the one Codex root option captain-miao consumes. Letting two
/// `--profile` flags reach clap is version-dependent (error vs last-one-wins),
/// and either answer is worse than naming the limitation before the agent
/// starts. The attached long/short forms are included because clap accepts
/// them too.
fn reject_profile_arg(extra_args: &[String]) -> Result<()> {
    if extra_args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            arg == "--profile"
                || arg.starts_with("--profile=")
                || arg == "-p"
                || (arg.starts_with("-p") && arg.len() > 2)
        })
    {
        anyhow::bail!(concat!(
            "Codex --profile/-p is reserved by captain-miao; ",
            "move those settings into the base config for managed sessions"
        ));
    }
    Ok(())
}

/// Create or refresh the one captain-owned profile in the **real** Codex home.
/// Existing real homes are left mode-for-mode alone; a missing one is created
/// 0700, and the profile itself is written atomically 0600. Refuse every
/// pre-existing non-owned entry, including a symlink, so a user profile can
/// never be replaced merely because it chose the same name.
///
/// A refresh **merges** into whatever is already there — see [`build_profile`].
fn ensure_profile_at(home: &Path, hooks_json: &str) -> Result<PathBuf> {
    if !home.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(home)
            .with_context(|| format!("creating Codex home {}", home.display()))?;
    }

    let path = home.join(PROFILE_FILE);
    let existing = match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                anyhow::bail!(
                    "refusing to replace non-owned Codex profile {}",
                    path.display()
                );
            }
            let current = std::fs::read_to_string(&path)
                .with_context(|| format!("reading Codex profile {}", path.display()))?;
            if !current.starts_with(PROFILE_MARKER) {
                anyhow::bail!(
                    "Codex profile {} already exists and is not owned by captain-miao",
                    path.display()
                );
            }
            Some(current)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("inspecting Codex profile {}", path.display()));
        }
    };

    let contents = build_profile(&path, hooks_json, existing.as_deref())?;
    let unchanged = existing.is_some_and(|current| current == contents);
    if !unchanged {
        atomic_write(&path, contents.as_bytes())
            .with_context(|| format!("writing Codex profile {}", path.display()))?;
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing Codex profile {}", path.display()))?;
    Ok(path)
}

/// PascalCase Codex event key (as it appears in hook config) → the snake_case
/// label Codex uses inside its hook-trust key. None for keys we don't emit.
/// Mirrors Codex's `hook_event_key_label`.
fn codex_event_label(pascal: &str) -> Option<&'static str> {
    Some(match pascal {
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "PreToolUse" => "pre_tool_use",
        "PostToolUse" => "post_tool_use",
        "PermissionRequest" => "permission_request",
        "Stop" => "stop",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        _ => return None,
    })
}

/// Reproduce Codex's hook-trust hash for one command hook.
///
/// Codex hashes a *normalized identity* — `{event_name, matcher, hooks:[{type,
/// command, timeout, async}]}` — by routing it through TOML, then canonical
/// JSON (recursively key-sorted, compact `serde_json`), then SHA-256, prefixed
/// `sha256:` (see `version_for_toml` in codex-rs `config/src/fingerprint.rs`).
/// We build that canonical JSON directly: the TOML round-trip's only effect on
/// our hooks is dropping the always-`None` `commandWindows`/`statusMessage`,
/// while keeping `timeout` (Codex's `unwrap_or(600)`) and `async` (false).
/// Verified byte-for-byte against a real Codex-persisted hash (see tests).
///
/// A `None` matcher drops the key rather than hashing a null, for the same
/// reason: TOML has no null, so the round-trip omits it. Which events carry a
/// matcher at all is decided in [`build_hooks_settings`].
fn command_hook_hash(label: &str, matcher: Option<&str>, command: &str) -> String {
    let mut identity = serde_json::json!({
        "event_name": label,
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 600,
            "async": false,
        }],
    });
    if let (Some(matcher), Some(map)) = (matcher, identity.as_object_mut()) {
        map.insert("matcher".to_string(), serde_json::json!(matcher));
    }
    let serialized = serde_json::to_vec(&canonical_json(&identity)).unwrap_or_default();
    let digest = Sha256::digest(&serialized);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Recursively sort object keys so serialization is deterministic regardless of
/// `serde_json`'s map ordering — mirrors Codex's `canonical_json`.
fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonical_json(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

/// Convert the launcher's JSON event table into a complete named profile:
/// inline hooks and the exact trust hashes Codex would write after an
/// interactive review. Inline hooks are keyed by the profile file path
/// itself (`<profile>:<event>:<group>:<handler>`), so every byte that controls
/// their identity and every byte that trusts it remain in the same owned file.
///
/// `existing` is the profile as it stands, and **only the `hooks` tables are
/// ours to regenerate**. Codex persists its own writes into the *selected*
/// profile layer, not the base config: the answer to its startup "do you trust
/// the contents of this directory" prompt lands in `[projects."<cwd>"]` here,
/// as does a `/model` change. Rebuilding the file from scratch dropped those on
/// the next launch, so every managed session asked to trust the directory
/// again — the same shape of bug as replacing a whole `hooks.state` table, and
/// the reason foreign trust entries are kept below too.
fn build_profile(profile_path: &Path, hooks_json: &str, existing: Option<&str>) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(hooks_json).context("parsing Codex hooks settings")?;
    let events = parsed
        .get("hooks")
        .and_then(|h| h.as_object())
        .context("Codex hooks settings have no hooks object")?;

    let mut hooks = toml::map::Map::new();
    let mut state = toml::map::Map::new();
    for (pascal, groups) in events {
        let label = codex_event_label(pascal)
            .with_context(|| format!("unknown Codex hook event {pascal}"))?;
        let groups_array = groups
            .as_array()
            .with_context(|| format!("Codex hook event {pascal} is not an array"))?;
        for (gi, group) in groups_array.iter().enumerate() {
            let matcher = group.get("matcher").and_then(|m| m.as_str());
            let handlers = group
                .get("hooks")
                .and_then(|h| h.as_array())
                .with_context(|| format!("Codex hook event {pascal} group {gi} has no handlers"))?;
            for (hi, handler) in handlers.iter().enumerate() {
                let command = handler
                    .get("command")
                    .and_then(|c| c.as_str())
                    .with_context(|| {
                        format!("Codex hook event {pascal} handler {gi}:{hi} has no command")
                    })?;
                let hash = command_hook_hash(label, matcher, command);
                let key = format!("{}:{label}:{gi}:{hi}", profile_path.display());
                let mut entry = toml::map::Map::new();
                entry.insert("trusted_hash".to_string(), toml::Value::String(hash));
                state.insert(key, toml::Value::Table(entry));
            }
        }
        hooks.insert(
            pascal.clone(),
            toml::Value::try_from(groups.clone())
                .with_context(|| format!("converting Codex hook event {pascal} to TOML"))?,
        );
    }
    // Anything in the file that is not a hook definition of ours stays. A
    // profile that no longer parses is past preserving — Codex could not load
    // it either — so that one case regenerates from nothing.
    let mut profile: toml::Table = existing
        .and_then(|current| current.parse().ok())
        .unwrap_or_default();
    let previous = match profile.remove("hooks") {
        Some(toml::Value::Table(t)) => t,
        _ => toml::map::Map::new(),
    };

    // Two kinds of entry in the file survive a refresh. One is a trust entry
    // keyed to some *other* config file: approving a user or project hook
    // mid-session writes it into whichever profile is selected, which is ours.
    //
    // The other is one of ours that Codex has since rewritten. It writes an
    // approval back into the profile it was launched with, so a value on disk
    // under our prefix is either what we last wrote or Codex's own correction
    // of it — and while the definition it trusts is the definition we are
    // writing, its answer beats ours. That is what keeps a future Codex
    // normalizing an identity differently from [`command_hook_hash`] to a
    // single prompt: the user is asked once, and the answer sticks instead of
    // being re-seeded away on the next launch. Keys under our prefix that we
    // no longer emit are retired either way.
    let prefix = format!("{}:", profile_path.display());
    let same_definitions = previous
        .iter()
        .filter(|(key, _)| key.as_str() != "state")
        .eq(hooks.iter());
    if let Some(toml::Value::Table(carried)) = previous.get("state") {
        for (key, value) in carried {
            let ours = key.starts_with(&prefix);
            if !ours || (same_definitions && state.contains_key(key)) {
                state.insert(key.clone(), value.clone());
            }
        }
    }
    hooks.insert("state".to_string(), toml::Value::Table(state));

    profile.insert("hooks".to_string(), toml::Value::Table(hooks));
    let serialized = toml::to_string(&profile).context("serializing Codex profile")?;
    Ok(format!("{PROFILE_HEADER}{serialized}"))
}

/// Build Codex's hook event table as JSON for the launcher's generic settings
/// channel. [`build_profile`] converts it to inline TOML before Codex starts.
/// The structure mirrors Claude's settings
/// (`{event: [{matcher, hooks:[{type,command}]}]}`) but uses Codex's PascalCase
/// event keys. The command is intentionally free of per-session data — the
/// socket arrives via `$CAPTAIN_MIAO_SOCK` — so the owned profile is identical
/// for every session and its trust hashes stay stable.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    let exe_q = shell_quote(&exe.to_string_lossy());

    let hook = |event: HookEvent, matcher: Option<&str>| -> serde_json::Value {
        let mut group = serde_json::Map::new();
        if let Some(matcher) = matcher {
            group.insert("matcher".to_string(), serde_json::json!(matcher));
        }
        group.insert(
            "hooks".to_string(),
            serde_json::json!([{
                "type": "command",
                "command": format!("{exe_q} hook --agent codex {}", event.as_kebab()),
            }]),
        );
        serde_json::Value::Array(vec![serde_json::Value::Object(group)])
    };
    // A matcher on `UserPromptSubmit` or `Stop` is not merely useless, it
    // silently costs the hook its trust: Codex drops the matcher of an event
    // that has none while parsing, so the identity it hashes has no matcher
    // *key* at all, and a seeded hash computed with one reads back as
    // `modified` — the "N hooks are new or changed" prompt, on every launch,
    // for as long as we keep re-seeding it. Probed with `hooks/list` over
    // `codex app-server` on 0.149.0: every other event reports the `"*"` it
    // was given, these two report `null`.
    let matcherless = None;

    serde_json::json!({
        "hooks": {
            "SessionStart":      hook(HookEvent::SessionStart, Some("*")),
            "UserPromptSubmit":  hook(HookEvent::PromptSubmit, matcherless),
            "PreToolUse":        hook(HookEvent::PreToolUse, Some("*")),
            "PostToolUse":       hook(HookEvent::PostToolUse, Some("*")),
            "PermissionRequest": hook(HookEvent::PermissionRequest, Some("*")),
            "Stop":              hook(HookEvent::Stop, matcherless),
            "PreCompact":        hook(HookEvent::PreCompact, Some("*")),
            "PostCompact":       hook(HookEvent::PostCompact, Some("*")),
        }
    })
    .to_string()
}

// =============================================================================
// Hook payload (stdin from Codex → normalized HookMessage)
// =============================================================================

#[derive(Deserialize)]
struct HookPayload {
    session_id: Option<String>,
    tool_name: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    transcript_path: Option<String>,
}

/// Normalize one Codex hook payload. Codex's event names are already ours, so
/// this is a straight field rename.
pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse codex hook JSON from stdin")?;
    Ok(HookMessage {
        event,
        session_id: payload.session_id,
        tool_name: payload.tool_name,
        message: None,
        cwd: payload.cwd,
        prompt: payload.prompt,
        // Codex's payload has no title either; its titles live in
        // `state_5.sqlite` and reach `name` through the per-host overlay.
        session_title: None,
        // Codex folds both from its rollout, which carries typed `token_count`
        // events — strictly more than a payload field would give.
        context_tokens: None,
        model: None,
        transcript_path: payload.transcript_path,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

// =============================================================================
// Agent activity (session-status file)
// =============================================================================

/// Codex has no session-status file we read, so it never reports a coarse
/// working/idle/background-shell activity — its `Active`↔`Idle` transitions ride
/// hooks (plus the rollout's own turn lifecycle, which settles both the
/// interrupt the Stop hook never reports and the goal continuation no prompt
/// hook announces — see [`scan_transcript_signals`]), and its sessions are
/// never refined into `BackgroundActive`.
pub fn session_activity(_agent_pid: u32) -> Option<AgentActivity> {
    None
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Codex's departures from [`common::dispatch_default`]; everything else maps
/// the way every backend maps it.
pub fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    common::adopt_session_facts(state, &mut msg);

    match msg.event {
        // request_user_input is Codex's AskUserQuestion analog: a function tool
        // that blocks waiting for the user, not an approval. It never fires
        // PermissionRequest (it's outside the approval path) and its
        // RequestUserInput event isn't persisted to the rollout, so this
        // PreToolUse hook is the only signal it's waiting. Surface it as
        // "Decision" (needs attention) — the paired PostToolUse, which fires
        // once the user answers, resets it to Active. Any other tool takes the
        // shared PreToolUse mapping (Active + last_tool).
        HookEvent::PreToolUse if msg.tool_name.as_deref() == Some("request_user_input") => {
            state.status = SessionStatus::WaitingForDecision;
            state.last_tool = msg.tool_name;
        }
        // A thread under a goal re-drives itself, so this Stop is a turn
        // boundary and not rest: Codex opens the next turn ~10ms later and
        // fires no hook for it (see [`thread_self_continues`]). Holding the row
        // Active is the whole fix for a goal run reading Idle between turns —
        // and it holds *nothing* open, since the store answers `false` the
        // moment the objective is met, paused or deferred. `last_tool` still
        // clears: the turn's tools really are done.
        HookEvent::Stop
            if state
                .session_id
                .as_deref()
                .is_some_and(thread_self_continues) =>
        {
            state.status = SessionStatus::Active;
            state.last_tool = None;
        }
        // Events Codex never emits — no profile hook registers them, so they
        // never reach this dispatcher. Ignored rather than mapped defensively,
        // which is why they're intercepted here instead of falling through to
        // the shared defaults. (The exhaustive match that forces a decision on a
        // newly-added `HookEvent` variant is `common::dispatch_default`'s.)
        HookEvent::Elicitation
        | HookEvent::ElicitationResult
        | HookEvent::StopFailure
        | HookEvent::CwdChanged => {}
        _ => common::dispatch_default(state, msg),
    }
}

// =============================================================================
// Transcript signal scan (turn lifecycle)
// =============================================================================

/// Read new bytes from the rollout starting at `offset` and settle the turn
/// lifecycle the hooks alone get wrong at both ends.
///
/// Codex brackets every turn with typed `event_msg`es — `task_started`,
/// `task_complete`, `turn_aborted` — and the hooks track only the middle of
/// that. An **interrupt** (Esc) writes `turn_aborted` and fires no Stop hook,
/// so without this the row would stay Active forever. The other end is the
/// hookless *start*: a thread under a goal opens its own next turn with
/// nothing to announce it ([`thread_self_continues`] is what normally keeps
/// such a row off Idle in the first place; `task_started` is the recovery when
/// the store couldn't be read, and the general statement of the rule for any
/// other hookless start). Compaction stays event-driven (PostCompact), so
/// there is no `compact_aborted` analog.
///
/// The start is reported as the **last** marker in the delta, not as any
/// marker: one delta routinely carries the end of one turn and the start of
/// the next, and an interrupt is regularly followed by the resubmit that
/// answers it.
pub fn scan_transcript_signals(path: &Path, offset: u64) -> TranscriptScan {
    let delta = crate::agent::read_transcript_delta(path, offset);
    let mut interrupted = false;
    let mut turn_open = false;
    for line in delta.text.lines() {
        // Reject on the cheap substring before parsing: a delta can carry
        // megabytes of tool output, and these three markers are short and rare.
        // The parse is not belt-and-braces — a rollout line *quoting* a marker
        // (an agent reading its own transcript, or this file) is exactly the
        // shape that would otherwise strand a working session at Idle.
        if !MARKERS.iter().any(|m| line.contains(m)) {
            continue;
        }
        match event_msg_kind(line) {
            Some("task_started") => turn_open = true,
            Some("task_complete") => turn_open = false,
            Some("turn_aborted") => {
                interrupted = true;
                turn_open = false;
            }
            _ => {}
        }
    }
    TranscriptScan {
        new_offset: delta.new_offset,
        interrupted,
        compact_aborted: false,
        turn_started: turn_open,
    }
}

/// The rollout markers [`scan_transcript_signals`] cares about, as they appear
/// in the raw line — the substring prefilter ahead of the parse.
const MARKERS: [&str; 3] = ["task_started", "task_complete", "turn_aborted"];

/// The `payload.type` of one rollout line, if it is an `event_msg` at all.
fn event_msg_kind(line: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let kind = value.get("payload")?.get("type")?.as_str()?;
    // Borrowed back out of the table so the caller can match on `&'static str`
    // rather than juggle the parsed value's lifetime.
    MARKERS.into_iter().find(|m| *m == kind)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_paste_encodes_a_path_without_terminal_or_shell_syntax() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(
            b"/work/a b/'\"\x1b[201~\n\xff.png",
        ));
        assert_eq!(
            clipboard_paste_input(path),
            b"\x1b[200~file:///work/a%20b/%27%22%1B%5B201~%0A%FF.png\x1b[201~"
        );
    }

    #[test]
    fn keyboard_reattach_honors_codex_opt_out() {
        for value in ["1", "true", "YES", " True "] {
            assert!(!keyboard_enhancement_enabled(Some(value), || false));
        }
        for value in ["0", "false", "NO", " False "] {
            assert!(keyboard_enhancement_enabled(Some(value), || true));
        }
        for value in [None, Some(""), Some("unexpected")] {
            assert!(keyboard_enhancement_enabled(value, || false));
            assert!(!keyboard_enhancement_enabled(value, || true));
        }
    }

    fn write_tmp(name: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "captain-miao-codex-test-{}-{}.jsonl",
            std::process::id(),
            name,
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn context_tokens_uses_last_token_count() {
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":1200},"total_token_usage":{"total_tokens":999999}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hi"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":4900},"total_token_usage":{"total_tokens":1000000}}}}"#,
            "\n",
        );
        let path = write_tmp("ctx", body);
        assert_eq!(
            read_transcript_stats(&path, None).context_tokens,
            Some(4900)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn context_tokens_skips_zero_total() {
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":777}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":0}}}}"#,
            "\n",
        );
        let path = write_tmp("ctx_zero", body);
        assert_eq!(read_transcript_stats(&path, None).context_tokens, Some(777));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_model_takes_last_turn_context() {
        let body = concat!(
            r#"{"type":"turn_context","payload":{"turn_id":"t1","model":"gpt-5"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"turn_id":"t2","model":"gpt-5.5"}}"#,
            "\n",
        );
        let path = write_tmp("codex_model", body);
        assert_eq!(
            read_transcript_stats(&path, None).model,
            Some("gpt-5.5".to_string())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn first_user_prompt_from_user_message_event() {
        let body = concat!(
            r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/tmp"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system stuff"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"  fix the   bug  please  "}}"#,
            "\n",
        );
        let path = write_tmp("prompt", body);
        assert_eq!(
            read_first_user_prompt(&path),
            Some("fix the bug please".to_string())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rollout_header_extracts_meta_and_branch() {
        let body = concat!(
            r#"{"type":"session_meta","payload":{"id":"019e-uuid","cwd":"/home/p","git":{"branch":"main","commit_hash":"deadbeef"}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"hello world"}}"#,
            "\n",
        );
        let path = write_tmp("header", body);
        let h = read_rollout_header(&path);
        assert_eq!(h.session_id.as_deref(), Some("019e-uuid"));
        assert_eq!(h.cwd.as_deref(), Some("/home/p"));
        assert_eq!(h.git_branch.as_deref(), Some("main"));
        assert_eq!(h.first_prompt.as_deref(), Some("hello world"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resumable_codex_session_uses_its_stored_title_without_a_prompt() {
        let home = scratch_home("resume-title");
        let day = home.join("sessions/2026/01/01");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-titled.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-titled\",\"cwd\":\"/work/repo\"}}\n",
        )
        .unwrap();
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, name TEXT);
             INSERT INTO threads VALUES ('thread-titled', 'Auto title', 'Saved session title');",
        )
        .unwrap();

        let candidates = list_resumable_at(&home, 10).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].first_prompt, None);
        assert_eq!(
            candidates[0].custom_title.as_deref(),
            Some("Saved session title"),
            "the picker needs the title or it falls back to (session XXXXXXXX)"
        );

        // The same scan must notice a subsequent rename without a rollout write.
        conn.execute("UPDATE threads SET name = 'New session title'", [])
            .unwrap();
        assert_eq!(
            list_resumable_at(&home, 10).unwrap()[0]
                .custom_title
                .as_deref(),
            Some("New session title")
        );
        conn.execute("UPDATE threads SET name = NULL", []).unwrap();
        assert_eq!(
            list_resumable_at(&home, 10).unwrap()[0]
                .custom_title
                .as_deref(),
            Some("Auto title")
        );
        drop(conn);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn resumable_codex_sessions_keep_the_prompt_when_the_title_store_is_unavailable() {
        let home = scratch_home("resume-without-store");
        let day = home.join("sessions/2026/01/01");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-prompt.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-prompt\",\"cwd\":\"/work/repo\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Fix the parser\"}}\n",
            ),
        )
        .unwrap();
        let db = home.join("state_5.sqlite");
        let check = || {
            let candidates = list_resumable_at(&home, 1).unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(
                candidates[0].first_prompt.as_deref(),
                Some("Fix the parser")
            );
            assert_eq!(candidates[0].custom_title, None);
        };
        check();
        assert!(
            !db.exists(),
            "a resume scan must never create Codex's database"
        );
        std::fs::write(&db, "unreadable database").unwrap();
        check();
        assert!(list_resumable_at(&home, 0).unwrap().is_empty());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn resume_metadata_requires_the_exact_promptless_resume_and_matching_rollout() {
        let home = scratch_home("resume-metadata");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("rollout.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"saved-id\"}}\n",
        )
        .unwrap();
        let conn = Connection::open(home.join("state_5.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT);")
            .unwrap();
        conn.execute(
            "INSERT INTO threads VALUES (?1, ?2)",
            ["saved-id", path.to_str().unwrap()],
        )
        .unwrap();
        for args in [
            vec![],
            vec!["fork", "saved-id"],
            vec!["resume"],
            vec!["resume", "--last"],
            vec!["resume", "saved-id", "Continue"],
            vec!["resume", "saved-id", "--model", "gpt-5.5"],
            vec!["resume", "missing-id"],
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_string).collect();
            assert!(read_resume_metadata_at(&home, &args).is_none(), "{args:?}");
        }
        let args = crate::agent::AgentControl::Codex.resume_args("saved-id", false);
        assert_eq!(
            read_resume_metadata_at(&home, &args).unwrap().session_id,
            "saved-id"
        );
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"different-id\"}}\n",
        )
        .unwrap();
        assert!(read_resume_metadata_at(&home, &args).is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(read_resume_metadata_at(&home, &args).is_none());
        drop(conn);
        std::fs::remove_file(home.join("state_5.sqlite")).unwrap();
        assert!(read_resume_metadata_at(&home, &args).is_none());
        assert!(
            !home.join("state_5.sqlite").exists(),
            "startup reads never create the store"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn scan_flags_turn_aborted() {
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
            "\n",
        );
        let path = write_tmp("abort", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(scan.interrupted);
        assert!(!scan.compact_aborted);
        // The abort closed the turn it opened — nothing left running to promote.
        assert!(!scan.turn_started);
        assert_eq!(scan.new_offset, std::fs::metadata(&path).unwrap().len());
        let _ = std::fs::remove_file(path);
    }

    /// The goal continuation this whole signal exists for: the turn ends and
    /// the next one starts in the same breath, with no hook between them. The
    /// Stop hook has parked the row at Idle by the time these bytes land, so
    /// reading the delta as "a turn is running" is the only thing that keeps a
    /// working session off Idle.
    #[test]
    fn scan_flags_a_turn_that_reopened_after_it_closed() {
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"a","last_agent_message":"done"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"b","started_at":1}}"#,
            "\n",
        );
        let path = write_tmp("continuation", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(scan.turn_started);
        assert!(!scan.interrupted);
        let _ = std::fs::remove_file(path);
    }

    /// …and the same two lines the other way round is an ordinary end of turn,
    /// which the Stop hook already settled. Last marker wins, not any marker.
    #[test]
    fn scan_holds_when_the_turn_ended_last() {
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"a"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"a"}}"#,
            "\n",
        );
        let path = write_tmp("ended", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(!scan.turn_started);
        let _ = std::fs::remove_file(path);
    }

    /// An interrupt answered by a fresh turn (the user hits Esc, then
    /// resubmits) settles Active: the launcher applies the interrupt first and
    /// the reopened turn last, in that order.
    #[test]
    fn scan_reports_both_when_an_abort_is_followed_by_a_new_turn() {
        let body = concat!(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"b"}}"#,
            "\n",
        );
        let path = write_tmp("abort-then-start", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(scan.interrupted);
        assert!(scan.turn_started);
        let _ = std::fs::remove_file(path);
    }

    /// A rollout line that merely *quotes* a marker is not that marker — an
    /// agent reading its own rollout (or this file) writes exactly this, and a
    /// substring match on it would strand a working session at Idle.
    #[test]
    fn scan_ignores_a_marker_quoted_inside_a_tool_output() {
        let quoted = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "output": "grep: {\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_aborted\"}} and task_started",
            },
        })
        .to_string();
        let path = write_tmp("quoted", &format!("{quoted}\n"));
        let scan = scan_transcript_signals(&path, 0);
        assert!(!scan.interrupted);
        assert!(!scan.turn_started, "a quoted marker settles nothing");
        let _ = std::fs::remove_file(path);
    }

    /// The goal store is what decides whether a Stop is rest, so it has to read
    /// Codex's real schema — including the deferral table, which records "not
    /// this turn" as a row rather than as a status.
    #[test]
    fn a_thread_self_continues_only_while_its_goal_drives_it() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE thread_goals (
                 thread_id TEXT PRIMARY KEY NOT NULL,
                 goal_id TEXT NOT NULL,
                 objective TEXT NOT NULL,
                 status TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE thread_goal_continuation_deferrals (
                 thread_id TEXT PRIMARY KEY NOT NULL
                     REFERENCES thread_goals(thread_id) ON DELETE CASCADE
             );
             INSERT INTO thread_goals VALUES ('driving','g1','keep going','active',0,0);
             INSERT INTO thread_goals VALUES ('done','g2','finished','complete',0,0);
             INSERT INTO thread_goals VALUES ('paused','g3','held','paused',0,0);
             INSERT INTO thread_goals VALUES ('deferred','g4','later','active',0,0);
             INSERT INTO thread_goal_continuation_deferrals VALUES ('deferred');",
        )
        .expect("schema");

        assert!(query_thread_self_continues(&conn, "driving"));
        assert!(!query_thread_self_continues(&conn, "done"));
        assert!(!query_thread_self_continues(&conn, "paused"));
        assert!(
            !query_thread_self_continues(&conn, "deferred"),
            "a deferred continuation is not one that is coming"
        );
        assert!(
            !query_thread_self_continues(&conn, "never-had-a-goal"),
            "the ordinary session: no row, no hold"
        );
    }

    /// A store this build doesn't recognise — a later `goals_N` that renamed
    /// the table — answers `false` rather than erroring, which parks the row
    /// the way it did before goals existed.
    #[test]
    fn an_unrecognised_goal_store_holds_nothing() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        assert!(!query_thread_self_continues(&conn, "driving"));
    }

    #[test]
    fn scan_no_signal_when_quiet() {
        let body = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"working\"}}\n";
        let path = write_tmp("quiet", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(!scan.interrupted);
        assert!(!scan.turn_started);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hooks_settings_is_stable_and_sockless() {
        let a = build_hooks_settings("/run/a.sock");
        let b = build_hooks_settings("/run/b.sock");
        assert_eq!(a, b, "hook config must not embed the per-session socket");
        assert!(a.contains("hook --agent codex"));
        assert!(a.contains("PreToolUse"));
        assert!(!a.contains(".sock"));
    }

    /// Build an in-memory `threads` table mirroring Codex's `state_5.sqlite`
    /// schema for the cases we care about. Both title columns are present, as
    /// they are in a current Codex: `name` is the `/rename`, `title` the
    /// auto-title it has to beat.
    fn titles_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                 id TEXT PRIMARY KEY,
                 title TEXT,
                 name TEXT
             );
             -- renamed: `name` wins over the auto-title beside it
             INSERT INTO threads VALUES ('019e5252-aaa', 'Do a deep review of x', 'test');
             -- never renamed: the whitespace-y auto-title stands in
             INSERT INTO threads VALUES ('019e5024-bbb', '  Please   do  analysis  ', NULL);
             INSERT INTO threads VALUES ('019e0000-ccc', '', NULL);
             INSERT INTO threads VALUES ('019e1111-ddd', NULL, NULL);
             -- renamed to blank: not a name, so the auto-title stands
             INSERT INTO threads VALUES ('019e2222-eee', 'Auto title', '   ');",
        )
        .unwrap();
        conn
    }

    /// The `name` column exists only on a Codex new enough to have `/rename`.
    fn legacy_titles_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT);
             INSERT INTO threads VALUES ('019e5252-aaa', 'Auto title');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn thread_title_reads_rename_and_autotitle() {
        let conn = titles_db();
        // `/rename` lands in `name`, which beats the auto-title in `title`.
        assert_eq!(
            query_thread_title(&conn, "019e5252-aaa").as_deref(),
            Some("test")
        );
        // Un-renamed: the auto-title stands, whitespace collapsed.
        assert_eq!(
            query_thread_title(&conn, "019e5024-bbb").as_deref(),
            Some("Please do analysis")
        );
        // A blank rename is not a name; the auto-title stands.
        assert_eq!(
            query_thread_title(&conn, "019e2222-eee").as_deref(),
            Some("Auto title")
        );
        // empty title → None (falls back to the rollout first-prompt auto-title)
        assert_eq!(query_thread_title(&conn, "019e0000-ccc"), None);
        // SQL NULL in both columns → None
        assert_eq!(query_thread_title(&conn, "019e1111-ddd"), None);
        // missing row → None
        assert_eq!(query_thread_title(&conn, "no-such-id"), None);
    }

    /// A database with no `name` column must degrade to the auto-title, not to
    /// nothing: the failing query is a prepare error, which `.optional()` does
    /// not absorb.
    #[test]
    fn a_database_without_the_name_column_still_reads_auto_titles() {
        let conn = legacy_titles_db();
        assert_eq!(
            query_thread_title(&conn, "019e5252-aaa").as_deref(),
            Some("Auto title")
        );
        assert_eq!(query_thread_title(&conn, "no-such-id"), None);
    }

    #[test]
    fn thread_title_id_is_bound_not_interpolated() {
        // The id is a bound parameter, so SQL metacharacters are matched as a
        // literal id (matching no row) rather than executed — no shape guard
        // needed. Confirm an injection attempt neither errors nor drops data.
        let conn = titles_db();
        assert_eq!(query_thread_title(&conn, "'; DROP TABLE threads;--"), None);
        // The table survived: a normal lookup still works afterward.
        assert_eq!(
            query_thread_title(&conn, "019e5252-aaa").as_deref(),
            Some("test")
        );
    }

    #[test]
    fn thread_titles_batch_returns_only_titled_ids() {
        // The per-host overlay's batch read: one connection, one entry per id
        // that actually has a usable title — empty/NULL/missing ids are simply
        // absent (the overlay marks them "known, untitled" itself).
        let conn = titles_db();
        let ids = vec![
            "019e5252-aaa".to_string(), // titled
            "019e0000-ccc".to_string(), // empty title
            "019e1111-ddd".to_string(), // NULL title
            "no-such-id".to_string(),   // no row
        ];
        let map = query_thread_titles(&conn, &ids);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("019e5252-aaa").map(String::as_str), Some("test"));
    }

    #[test]
    fn command_hook_hash_matches_codex_persisted_value() {
        // Hermetic regression anchors for our reproduction of Codex's hook-trust
        // hashing (TOML-normalized identity → canonical JSON → sha256). No
        // `codex` binary, file, or network involved. The algorithm was validated
        // during development against a real `$CODEX_HOME/config.toml` that Codex
        // wrote after an interactive "Trust all and continue", and each value
        // below was re-read from a live `hooks/list` on 0.149.0; these frozen
        // input→hash pairs then guard against *us* regressing that reproduction.
        // They do NOT detect a future *Codex* changing its algorithm — the
        // values are frozen, so that case slips past here and instead
        // resurfaces the trust prompt in the field, which
        // `build_profile` then heals by carrying Codex's own correction
        // forward. The commands are placeholder paths; each hash covers exactly
        // that literal, so re-freeze it if you edit one.
        let cmd = "/usr/local/bin/captain-miao hook --agent codex permission-request";
        assert_eq!(
            command_hook_hash("permission_request", Some("*"), cmd),
            "sha256:ede30d21fa951d0bb9bc60a12e12755ee1a789566aab412f398596e0f2d6302b",
        );
        // A matcher-less event hashes an identity with no matcher key — the
        // difference that cost `Stop` and `UserPromptSubmit` their trust on
        // every launch.
        let cmd = "/usr/local/bin/captain-miao hook --agent codex stop";
        assert_eq!(
            command_hook_hash("stop", None, cmd),
            "sha256:90fd1d6853d72d9d677685e0d64a0dafcbce96d50962de1dabaa6065823ec50c",
        );
        assert_ne!(
            command_hook_hash("stop", Some("*"), cmd),
            command_hook_hash("stop", None, cmd),
        );
    }

    fn scratch_home(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let home = std::env::temp_dir().join(format!(
            "captain-miao-codex-profile-{}-{tag}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    #[test]
    fn codex_home_resolves_overrides_without_global_env_mutation() {
        let cwd = PathBuf::from("/work");
        assert_eq!(
            resolve_codex_home(
                Some(PathBuf::from("relative-home")),
                Some(PathBuf::from("/users/me")),
                Some(cwd)
            ),
            Some(PathBuf::from("/work/relative-home"))
        );
        assert_eq!(
            resolve_codex_home(
                Some(PathBuf::from("/custom/codex")),
                Some(PathBuf::from("/users/me")),
                None
            ),
            Some(PathBuf::from("/custom/codex"))
        );
        assert_eq!(
            resolve_codex_home(Some(PathBuf::new()), Some(PathBuf::from("/users/me")), None),
            Some(PathBuf::from("/users/me/.codex"))
        );
    }

    #[test]
    fn profile_contains_only_inline_hooks_and_own_trust() {
        let path = PathBuf::from("/users/me/.codex/captain-miao.config.toml");
        let profile = build_profile(&path, &build_hooks_settings("/run/x.sock"), None).unwrap();
        assert!(profile.starts_with(PROFILE_MARKER));
        let doc: toml::Table = profile.parse().unwrap();
        assert_eq!(doc["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);

        let state = doc["hooks"]["state"].as_table().unwrap();
        assert_eq!(state.len(), 8);
        let key = format!("{}:pre_tool_use:0:0", path.display());
        let hash = state[&key]["trusted_hash"].as_str().unwrap();
        assert!(hash.starts_with("sha256:") && hash.len() == 71);
        assert!(doc.get("features").is_none());
        assert!(doc.get("model").is_none());
        assert!(doc.get("projects").is_none());
    }

    #[test]
    fn profile_refresh_keeps_what_codex_wrote_into_the_profile() {
        // Codex persists into the *selected* profile layer, so a managed
        // session's answers land in our file: directory trust from its startup
        // prompt, a `/model` change, and a trust entry for someone else's hook.
        // A refresh that rebuilt the file from scratch dropped all three, which
        // is what brought the trust prompt back on every managed launch.
        let home = scratch_home("merge");
        let hooks = build_hooks_settings("/run/x.sock");
        let path = ensure_profile_at(&home, &hooks).unwrap();
        let stale = format!("{}:removed_event:0:0", path.display());
        let mut doc: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        doc.insert("model".to_string(), toml::Value::String("gpt-5.5".into()));
        doc.insert(
            "projects".to_string(),
            toml::Value::try_from(std::collections::BTreeMap::from([(
                "/work/repo".to_string(),
                std::collections::BTreeMap::from([(
                    "trust_level".to_string(),
                    "trusted".to_string(),
                )]),
            )]))
            .unwrap(),
        );
        let state = doc["hooks"]["state"].as_table_mut().unwrap();
        state.insert(
            "/work/repo/.codex/hooks.json:stop:0:0".to_string(),
            toml::Value::try_from(std::collections::BTreeMap::from([(
                "trusted_hash".to_string(),
                "sha256:their-hook".to_string(),
            )]))
            .unwrap(),
        );
        state.insert(
            stale.clone(),
            toml::Value::try_from(std::collections::BTreeMap::from([(
                "trusted_hash".to_string(),
                "sha256:ours-but-retired".to_string(),
            )]))
            .unwrap(),
        );
        atomic_write(
            &path,
            format!("{PROFILE_HEADER}{}", toml::to_string(&doc).unwrap()).as_bytes(),
        )
        .unwrap();

        ensure_profile_at(&home, &hooks).unwrap();
        let doc: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(
            doc["projects"]["/work/repo"]["trust_level"].as_str(),
            Some("trusted")
        );
        let state = doc["hooks"]["state"].as_table().unwrap();
        assert_eq!(
            state["/work/repo/.codex/hooks.json:stop:0:0"]["trusted_hash"].as_str(),
            Some("sha256:their-hook"),
            "a trust entry for another config file survives our refresh"
        );
        assert!(
            !state.contains_key(&stale),
            "an entry for an event we no longer emit is retired"
        );
        // Our own hooks are still there, still trusted, still exactly ours.
        assert_eq!(doc["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(state.len(), 9);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn profile_refresh_carries_a_correction_codex_made_to_our_own_trust() {
        // Codex rewrites the trust entry in place when the user answers its
        // "hooks are new or changed" prompt. Ours is the same file, so a
        // refresh must not put the rejected hash back — otherwise a Codex that
        // hashes an identity differently than we do asks again on every single
        // launch, which is exactly how `Stop` and `UserPromptSubmit` behaved.
        let home = scratch_home("carry");
        let hooks = build_hooks_settings("/run/x.sock");
        let path = ensure_profile_at(&home, &hooks).unwrap();
        let key = format!("{}:stop:0:0", path.display());
        let corrected = "sha256:what-this-codex-actually-computes";

        let approve = |path: &Path, key: &str| {
            let mut doc: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
            doc["hooks"]["state"][key]["trusted_hash"] = toml::Value::String(corrected.into());
            atomic_write(
                path,
                format!("{PROFILE_HEADER}{}", toml::to_string(&doc).unwrap()).as_bytes(),
            )
            .unwrap();
        };
        approve(&path, &key);

        ensure_profile_at(&home, &hooks).unwrap();
        let doc: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            doc["hooks"]["state"][&key]["trusted_hash"].as_str(),
            Some(corrected),
            "an approval for an unchanged hook survives the next launch"
        );

        // A *changed* command is a different hook, so the carried answer no
        // longer applies and our own hash takes over again.
        let moved = hooks.replace("hook --agent codex", "hook --moved --agent codex");
        assert_ne!(moved, hooks);
        ensure_profile_at(&home, &moved).unwrap();
        let doc: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            doc["hooks"]["state"][&key]["trusted_hash"].as_str(),
            Some(
                command_hook_hash(
                    "stop",
                    None,
                    &format!(
                        "{} hook --moved --agent codex stop",
                        shell_quote(&std::env::current_exe().unwrap().to_string_lossy())
                    )
                )
                .as_str()
            ),
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn profile_write_is_private_owned_and_idempotent() {
        let home = scratch_home("write");
        let hooks = build_hooks_settings("/run/x.sock");
        let path = ensure_profile_at(&home, &hooks).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        let first_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        ensure_profile_at(&home, &hooks).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            first_mtime
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn profile_write_refuses_an_unowned_collision() {
        let home = scratch_home("collision");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join(PROFILE_FILE);
        std::fs::write(&path, "model = \"user-choice\"\n").unwrap();

        let err = ensure_profile_at(&home, &build_hooks_settings("/run/x.sock")).unwrap_err();
        assert!(format!("{err:#}").contains("is not owned by captain-miao"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "model = \"user-choice\"\n"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn user_profile_args_are_reserved() {
        for args in [
            vec!["--profile".to_string(), "review".to_string()],
            vec!["--profile=review".to_string()],
            vec!["-p".to_string(), "review".to_string()],
            vec!["-preview".to_string()],
        ] {
            assert!(reject_profile_arg(&args).is_err(), "accepted {args:?}");
        }
        assert!(reject_profile_arg(&["resume".to_string(), "thread-id".to_string()]).is_ok());
        assert!(
            reject_profile_arg(&["--".to_string(), "--profile".to_string()]).is_ok(),
            "a literal prompt after -- is not a profile selector"
        );
        assert_eq!(
            managed_launch_args(&["resume".to_string(), "thread-id".to_string()]).unwrap(),
            [
                "--profile",
                "captain-miao",
                "--enable",
                "hooks",
                "resume",
                "thread-id"
            ]
        );
    }

    #[test]
    fn codex_event_label_covers_emitted_hooks() {
        // Every event build_hooks_settings writes must map to a Codex label, or
        // build_profile would fail instead of producing a half-trusted profile.
        let json = build_hooks_settings("/run/x.sock");
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        for key in val["hooks"].as_object().unwrap().keys() {
            assert!(codex_event_label(key).is_some(), "no Codex label for {key}");
        }
    }

    #[test]
    fn parse_hook_payload_maps_codex_fields() {
        let stdin = r#"{"session_id":"s1","cwd":"/w","tool_name":"Bash","prompt":"go","transcript_path":"/t.jsonl","hook_event_name":"PreToolUse","turn_id":"t9"}"#;
        let msg = parse_hook_payload(HookEvent::PreToolUse, stdin).unwrap();
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
        assert_eq!(msg.cwd.as_deref(), Some("/w"));
        assert_eq!(msg.tool_name.as_deref(), Some("Bash"));
        assert_eq!(msg.transcript_path.as_deref(), Some("/t.jsonl"));
    }

    fn active_state() -> LauncherState {
        LauncherState::for_test(crate::agent::AgentControl::Codex, SessionStatus::Active)
    }

    fn pre_tool_use(tool: &str) -> HookMessage {
        HookMessage {
            event: HookEvent::PreToolUse,
            session_id: None,
            tool_name: Some(tool.to_string()),
            message: None,
            cwd: None,
            prompt: None,
            session_title: None,
            context_tokens: None,
            model: None,
            transcript_path: None,
            raw: None,
            session_is_child: None,
        }
    }

    fn dispatched(msg: HookMessage) -> SessionStatus {
        let mut state = active_state();
        dispatch_hook(&mut state, msg);
        state.status
    }

    #[test]
    fn pre_tool_use_request_user_input_is_decision() {
        // request_user_input blocks waiting for the user (Codex's AskUserQuestion
        // analog), so it surfaces as "Decision" rather than plain "Active".
        assert_eq!(
            dispatched(pre_tool_use("request_user_input")),
            SessionStatus::WaitingForDecision,
        );
    }

    #[test]
    fn pre_tool_use_other_tool_stays_active() {
        assert_eq!(dispatched(pre_tool_use("Bash")), SessionStatus::Active,);
    }
}
