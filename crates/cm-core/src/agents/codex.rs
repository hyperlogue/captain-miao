//! Codex CLI backend. Owns every Codex-specific path, rollout JSON shape, and
//! hook event mapping. The dashboard reaches everything in here only via
//! `crate::agent::AgentControl::Codex`'s match arms.
//!
//! Codex's hook system is a near-clone of Claude Code's: the same event names
//! (minus a few) and an identical snake_case stdin payload, so the launcher
//! loop and `HookMessage` are reused unchanged. The two genuine differences
//! are (1) hooks can't be injected per-invocation — they're discovered from
//! `$CODEX_HOME/hooks.json` — so we point the agent at a synthetic, shared
//! `CODEX_HOME` that symlinks the real one and adds our `hooks.json`; and
//! (2) Codex records a far richer rollout JSONL than Claude's transcript, so
//! context tokens and lifecycle signals come straight from typed events.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::process::Command;

use super::common;
use super::synth_home::{CopiedEntry, SynthHome, atomic_write};
use super::{collapse_whitespace, shell_quote};
use crate::agent::{
    AgentActivity, ResumeCandidate, SessionIndex, SessionIndexCache, TranscriptScan,
    TranscriptStats,
};
use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "codex";

// =============================================================================
// Filesystem locations
// =============================================================================

/// The real Codex home — `$CODEX_HOME` if the user set one globally, else
/// `~/.codex`. This is what the synthetic home mirrors; it is *not* what the
/// launched agent is handed (see `synth_home`), and it is not where a session's
/// state is read from either — that goes through [`read_path`], which knows the
/// mirror can be one-sided.
fn codex_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

/// Resolve `name` in the home that actually holds it — **the synthetic one
/// first**, the real one as a fallback.
///
/// Reading the real home is only right while the mirror holds, and it does not
/// hold for a state file the *agent* minted. `SynthHome::ensure` links what the
/// real home already has; a name it has never had gets no link, so Codex
/// creates the file inside the synthetic home and the real one never grows it.
/// On a machine where `~/.codex` is managed (home-manager ships a `config.toml`
/// symlink and nothing else) and `codex` has never been run bare, *every* state
/// file is in that position — `state_5.sqlite` and the whole `sessions/` tree
/// included. Reading the real home there finds nothing at all, which is how a
/// `/rename` could land in sqlite and never reach a row.
///
/// The synthetic path is the safe one to prefer because it is the *same file*
/// wherever the mirror did hold: an entry the real home has is reached through
/// the link. The fallback covers the one case the synthetic home can't answer —
/// a host where captain-miao has never launched Codex, whose bare sessions
/// [`list_resumable`] should still offer.
///
/// Resolved per call rather than cached: the first launch on a fresh host
/// creates these entries *after* the dashboard is already running.
fn read_path(name: &str) -> Option<PathBuf> {
    resolve_read_path(&synth_home(), codex_home(), name)
}

/// The resolution itself, split from home discovery so a test can point it at
/// fixture trees without touching the environment.
///
/// `exists()` follows links, which is what makes the one rule cover both
/// mirrored and agent-minted entries — and a *dangling* link (the real entry
/// deleted since the last launch) correctly falls through to the real path,
/// where the agent's next write will land through that same link.
fn resolve_read_path(synth: &Path, real: Option<PathBuf>, name: &str) -> Option<PathBuf> {
    let candidate = synth.join(name);
    if candidate.exists() {
        return Some(candidate);
    }
    Some(real?.join(name))
}

fn sessions_root() -> Option<PathBuf> {
    read_path("sessions")
}

/// Paths whose changes should wake the host process (a dashboard reload / a
/// server re-push). Only the title store's WAL sidecar: a `/rename` (or Codex's
/// own auto-title) lands in `state_5.sqlite` alone — no hook, no rollout line,
/// no state-file write — so this wake is what lets the per-host title overlay
/// ([`crate::backend::LocalBackend`]'s throttled sqlite read) surface it while
/// the sessions are otherwise idle. The rollout tree is folded by the launcher
/// and arrives via the state file, so it's deliberately not watched; nor is
/// `~/.codex` itself (its `logs_2.sqlite-wal` telemetry churns far too much).
/// Best-effort: if the wal is momentarily absent (pre-first-write / just
/// checkpointed) the watch fails silently and the overlay's refresh piggybacks
/// on the next session event instead.
pub fn watch_paths() -> Vec<PathBuf> {
    // Both homes' wal, not just the one [`title_watch_path`] resolves to now.
    // This runs once, at watcher setup, while the resolution can still move —
    // a host whose first Codex session starts later mints the db in the
    // synthetic home, and a watcher latched onto the real home's path would
    // never wake again. Watching a path that never materializes costs nothing
    // (the registration just fails), and the duplicate collapses to one entry
    // on a host where the mirror does hold.
    let mut paths: Vec<PathBuf> = [
        Some(synth_home().join("state_5.sqlite-wal")),
        codex_home().map(|h| h.join("state_5.sqlite-wal")),
    ]
    .into_iter()
    .flatten()
    .collect();
    paths.dedup();
    paths
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
/// [`crate::backend::LocalBackend`] (one throttled reader per host, keyed by
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
    read_path("state_5.sqlite")
}

/// The title store's WAL sidecar — the file whose change means "a title may
/// have moved". `state_5` runs in WAL mode: writes land here (the main db
/// updates only on checkpoint), so this is what [`watch_paths`] nominates for
/// the host-process wake. Watching just this file, not `~/.codex`, keeps the
/// churny `logs_2.sqlite-wal` telemetry sibling from waking anything.
pub fn title_watch_path() -> Option<PathBuf> {
    // Beside whichever `state_5.sqlite` [`read_path`] resolved to, rather than
    // resolved on its own: a checkpoint deletes the wal, so it is regularly
    // absent from *both* homes and can't point at its own.
    Some(state_db_path()?.with_file_name("state_5.sqlite-wal"))
}

/// Stat stamp of the title store — `(main db mtime, wal mtime)` — the cheap
/// change gate the per-host overlay checks before touching sqlite: if the stamp
/// hasn't moved since the last read, no title can have changed and the read is
/// skipped. Both files matter: writes land in the wal, and a checkpoint folds
/// them into the main db (possibly deleting the wal). `None` per
/// missing/unstattable file.
pub fn title_store_mtimes() -> (Option<SystemTime>, Option<SystemTime>) {
    fn mtime(p: Option<PathBuf>) -> Option<SystemTime> {
        std::fs::metadata(p?).ok()?.modified().ok()
    }
    (mtime(state_db_path()), mtime(title_watch_path()))
}

/// Batch-read the current titles for `ids` from `state_5.sqlite` over one
/// read-only connection. Called by the per-host overlay in
/// [`crate::backend::LocalBackend`] — a single throttled reader serving every
/// Codex session on the host. Returns only the ids that have a (non-empty)
/// title; an absent id simply has no title row yet. Empty on any open failure
/// (the overlay just tries again next pass).
pub fn read_thread_titles(ids: &[String]) -> HashMap<String, String> {
    let Some(db) = state_db_path() else {
        return HashMap::new();
    };
    // Read-only so the live WAL DB is never written. URI + NO_MUTEX match
    // rusqlite's default open flags (we only swap READ_WRITE|CREATE for
    // READ_ONLY); the connection is single-threaded and short-lived.
    let Ok(conn) = Connection::open_with_flags(
        &db,
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
// Rollout reading
// =============================================================================

/// Read the tail of a rollout and return whole lines (dropping any leading
/// partial line from a mid-file seek).
///
/// We start with a ~256 KB window but grow it backward (doubling) until the
/// window either contains a newline — so at least one complete line survives the
/// leading-partial drop — or reaches the file start. Without this, a single
/// rollout line longer than the window (e.g. a giant pasted message) would leave
/// the window newline-free and the `split_once('\n')` drop would discard the
/// whole thing, losing every later `token_count`/`turn_context` line.
fn read_rollout_tail(path: &Path) -> Option<String> {
    const INITIAL_WINDOW: u64 = 256 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();

    let mut window = INITIAL_WINDOW;
    loop {
        let start = size.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::with_capacity(window.min(size) as usize);
        file.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);

        if start == 0 {
            return Some(text.into_owned());
        }
        if let Some((_, rest)) = text.split_once('\n') {
            return Some(rest.to_string());
        }
        // No newline in this window: an oversized line precedes us. Grow and
        // retry so the preceding complete lines become reachable.
        window = window.saturating_mul(2);
    }
}

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
    let Some(tail) = read_rollout_tail(path) else {
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
/// to `limit` candidates sorted by mtime (most recent first).
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    let root = sessions_root().ok_or_else(|| anyhow::anyhow!("no codex home"))?;

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
                    let Ok(mtime) = meta.modified() else { continue };
                    files.push((path, mtime));
                }
            }
        }
    }
    files.sort_by_key(|b| std::cmp::Reverse(b.1));
    files.truncate(limit);

    let mut out = Vec::with_capacity(files.len());
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
// Launcher: process spawn + synthetic CODEX_HOME
// =============================================================================

/// A single shared synthetic `$CODEX_HOME` used by every Codex session. It
/// mirrors the real home via symlinks and adds our `hooks.json`. Keeping it a
/// stable path with stable contents means Codex's hook-trust prompt fires at
/// most once per machine (trust is keyed by content), instead of every launch.
fn synth_home() -> PathBuf {
    crate::state::state_dir().join("codex-home")
}

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    // The launcher already wrote our hooks.json contents to `settings_path`;
    // relocate them into the synthetic home where Codex will discover them.
    let hooks_json =
        std::fs::read_to_string(settings_path).context("reading codex hooks settings")?;
    let home = ensure_synth_home(&hooks_json)?;

    // `agent_command` puts the shim farm on `PATH`, which for Codex buys only
    // `clipboard-paste`: it reads the clipboard in-process, so no shim can serve
    // its `Ctrl+V`.
    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    cmd.env("CODEX_HOME", &home);
    // The hook subprocess reads the launcher socket from here rather than from
    // an argv flag — that keeps hooks.json byte-identical across sessions so
    // its trust hash never changes.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    // Turn the lifecycle-hook feature on. We do NOT pass
    // `--dangerously-bypass-hook-trust`: `seed_hook_trust` pre-writes the exact
    // trust hash Codex would persist on approval into the synth home's
    // config.toml, so our own hooks are already trusted and no interactive
    // "Trust all and continue" prompt fires.
    cmd.args(["-c", "features.hooks=true"]);
    cmd.args(extra_args);
    Ok(cmd)
}

/// Create / refresh the synthetic home and return it: mirror the real Codex home
/// (all but `hooks.json`, which is ours, and `config.toml`, which is copied
/// writable because Codex persists hook trust into it), write our `hooks.json`,
/// then pre-trust it. The mirroring itself — and the hard-won rules about
/// dangling links, shadowing entries and file modes — lives in
/// [`super::synth_home`].
fn ensure_synth_home(hooks_json: &str) -> Result<PathBuf> {
    let home = SynthHome {
        dir: synth_home(),
        real: codex_home(),
        owned: &["hooks.json"],
        copied: &[CopiedEntry {
            name: "config.toml",
            snapshot: ".config-source.toml",
        }],
        // `codex login` writes its credentials here. On a machine that has never
        // logged in, the file does not exist to be mirrored, so a login *inside*
        // a captain-miao session would leave the credentials in the synthetic
        // home where a bare `codex` can't see them. This is the documented
        // location rather than one verified against a login here.
        adopted: &["auth.json"],
        prune: false,
    };
    home.ensure()?;
    home.write_owned("hooks.json", hooks_json)?;

    // Pre-trust our own hooks: compute the hash Codex persists on interactive
    // approval and write it into config.toml's [hooks.state]. Recomputed every
    // launch so it tracks any change to hooks.json (e.g. the embedded exe path
    // shifting on a rebuild) and never goes stale — which is what lets us drop
    // `--dangerously-bypass-hook-trust`.
    seed_hook_trust(&home.dir);
    Ok(home.dir)
}

/// PascalCase Codex event key (as it appears in hooks.json) → the snake_case
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
fn command_hook_hash(label: &str, matcher: &str, command: &str) -> String {
    let identity = serde_json::json!({
        "event_name": label,
        "matcher": matcher,
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 600,
            "async": false,
        }],
    });
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

/// Merge hook-trust entries into the synth home's config.toml so Codex runs our
/// hooks without the interactive "Trust all and continue" prompt — and without
/// `--dangerously-bypass-hook-trust`. We parse hooks.json, compute the trust
/// hash for every command hook, and write them under `[hooks.state]` keyed
/// `"<hooks.json path>:<label>:<group>:<handler>"` (Codex's `hook_key` format).
///
/// Best-effort: any failure (missing/garbled config or hooks, a `hooks` key
/// that isn't a table) just leaves trust unseeded, which at worst restores the
/// one-time prompt rather than breaking the launch. Re-runs each launch and is
/// idempotent, so a config reseed (on real-config change) is immediately
/// re-trusted.
fn seed_hook_trust(home: &Path) {
    let hooks_path = home.join("hooks.json");
    let config_path = home.join("config.toml");

    let Ok(hooks_text) = std::fs::read_to_string(&hooks_path) else {
        return;
    };
    let Ok(hooks_json) = serde_json::from_str::<serde_json::Value>(&hooks_text) else {
        return;
    };
    let Some(events) = hooks_json.get("hooks").and_then(|h| h.as_object()) else {
        return;
    };

    let mut state = toml::map::Map::new();
    for (pascal, groups) in events {
        let Some(label) = codex_event_label(pascal) else {
            continue;
        };
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for (gi, group) in groups.iter().enumerate() {
            let matcher = group.get("matcher").and_then(|m| m.as_str()).unwrap_or("*");
            let Some(handlers) = group.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            for (hi, handler) in handlers.iter().enumerate() {
                let Some(command) = handler.get("command").and_then(|c| c.as_str()) else {
                    continue;
                };
                let hash = command_hook_hash(label, matcher, command);
                let key = format!("{}:{label}:{gi}:{hi}", hooks_path.display());
                let mut entry = toml::map::Map::new();
                entry.insert("trusted_hash".to_string(), toml::Value::String(hash));
                state.insert(key, toml::Value::Table(entry));
            }
        }
    }

    // Merge into [hooks][state], starting from the existing copy (or empty) so
    // the user's own config settings survive the rewrite.
    let mut doc = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .unwrap_or_default();
    let hooks_tbl = doc
        .entry("hooks".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let toml::Value::Table(hooks_tbl) = hooks_tbl else {
        return;
    };
    hooks_tbl.insert("state".to_string(), toml::Value::Table(state));

    if let Ok(serialized) = toml::to_string(&doc) {
        let _ = atomic_write(&config_path, serialized.as_bytes());
    }
}

/// Build the Codex `hooks.json`. The structure mirrors Claude's settings
/// (`{event: [{matcher, hooks:[{type,command}]}]}`) but uses Codex's PascalCase
/// event keys. The command is intentionally free of per-session data — the
/// socket arrives via `$CAPTAIN_MIAO_SOCK` — so the file is identical for every
/// session and Codex only ever asks to trust it once.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    let exe_q = shell_quote(&exe.to_string_lossy());

    let hook = |event: HookEvent| -> serde_json::Value {
        serde_json::json!([{
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": format!("{exe_q} hook --agent codex {}", event.as_kebab()),
            }],
        }])
    };

    serde_json::json!({
        "hooks": {
            "SessionStart":      hook(HookEvent::SessionStart),
            "UserPromptSubmit":  hook(HookEvent::PromptSubmit),
            "PreToolUse":        hook(HookEvent::PreToolUse),
            "PostToolUse":       hook(HookEvent::PostToolUse),
            "PermissionRequest": hook(HookEvent::PermissionRequest),
            "Stop":              hook(HookEvent::Stop),
            "PreCompact":        hook(HookEvent::PreCompact),
            "PostCompact":       hook(HookEvent::PostCompact),
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
/// hooks (plus the rollout's `turn_aborted` for interrupts), and its sessions are
/// never refined into `BackgroundActive`.
pub fn session_activity(_agent_pid: u32) -> Option<AgentActivity> {
    None
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Codex's departures from [`common::dispatch_default`]; everything else maps
/// the way every backend maps it.
pub async fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
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
        // Events Codex never emits — no hooks.json entry registers them, so they
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
// Transcript signal scan (interrupt detection)
// =============================================================================

/// Read new bytes from the rollout starting at `offset` and flag a user
/// interrupt. Codex writes a `turn_aborted` event when the user hits Esc; it
/// fires no Stop hook in that case, so without this the session would stay
/// Active. Compaction is event-driven (PostCompact) so there is no
/// `compact_aborted` analog.
pub fn scan_transcript_signals(path: &Path, offset: u64) -> TranscriptScan {
    let delta = crate::agent::read_transcript_delta(path, offset);
    let interrupted = delta
        .text
        .lines()
        .any(|line| line.contains("\"turn_aborted\""));
    TranscriptScan {
        new_offset: delta.new_offset,
        interrupted,
        compact_aborted: false,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(scan.new_offset, std::fs::metadata(&path).unwrap().len());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scan_no_signal_when_quiet() {
        let body = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"working\"}}\n";
        let path = write_tmp("quiet", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(!scan.interrupted);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hooks_settings_is_stable_and_sockless() {
        let a = build_hooks_settings("/run/a.sock");
        let b = build_hooks_settings("/run/b.sock");
        assert_eq!(a, b, "hooks.json must not embed the per-session socket");
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
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, name TEXT);
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
        // Hermetic regression anchor for our reproduction of Codex's hook-trust
        // hashing (TOML-normalized identity → canonical JSON → sha256). No
        // `codex` binary, file, or network involved. The algorithm was validated
        // during development against a real `$CODEX_HOME/config.toml` that Codex
        // wrote after an interactive "Trust all and continue"; this frozen
        // input→hash pair then guards against *us* regressing that reproduction.
        // It does NOT detect a future *Codex* changing its algorithm — the value
        // is frozen, so that case slips past here and instead resurfaces the
        // one-time trust prompt in the field. The command is a placeholder path;
        // the hash covers exactly this literal, so re-freeze it if you edit it.
        let cmd = "/usr/local/bin/captain-miao hook --agent codex permission-request";
        assert_eq!(
            command_hook_hash("permission_request", "*", cmd),
            "sha256:ede30d21fa951d0bb9bc60a12e12755ee1a789566aab412f398596e0f2d6302b",
        );
    }

    /// A pair of throwaway homes — `(synth, real)`, both created.
    fn home_pair(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "captain-miao-codex-homes-{}-{tag}",
            std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&base);
        let (synth, real) = (base.join("synth"), base.join("real"));
        std::fs::create_dir_all(&synth).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        (synth, real)
    }

    /// The case that broke renames: `~/.codex` is managed and holds nothing but
    /// a config, so Codex minted `state_5.sqlite` inside the synthetic home and
    /// the mirror never linked it. Reading the real home finds no file at all.
    #[test]
    fn an_agent_minted_entry_is_read_from_the_synthetic_home() {
        let (synth, real) = home_pair("minted");
        std::fs::write(synth.join("state_5.sqlite"), b"db").unwrap();

        let got = resolve_read_path(&synth, Some(real.clone()), "state_5.sqlite");
        assert_eq!(got, Some(synth.join("state_5.sqlite")));
        let _ = std::fs::remove_dir_all(synth.parent().unwrap());
    }

    /// Where the mirror *did* hold, the synthetic path is a link onto the real
    /// file — the same bytes, so preferring it is never the wrong answer.
    #[test]
    fn a_mirrored_entry_resolves_through_its_link() {
        let (synth, real) = home_pair("mirrored");
        std::fs::write(real.join("state_5.sqlite"), b"db").unwrap();
        std::os::unix::fs::symlink(real.join("state_5.sqlite"), synth.join("state_5.sqlite"))
            .unwrap();

        let got = resolve_read_path(&synth, Some(real.clone()), "state_5.sqlite").unwrap();
        assert_eq!(got, synth.join("state_5.sqlite"));
        assert_eq!(std::fs::read(&got).unwrap(), b"db");
        let _ = std::fs::remove_dir_all(synth.parent().unwrap());
    }

    /// A host where captain-miao has never launched Codex: nothing is in the
    /// synthetic home, and the real home's bare sessions must still be found.
    #[test]
    fn an_unmirrored_entry_falls_back_to_the_real_home() {
        let (synth, real) = home_pair("bare");
        std::fs::create_dir_all(real.join("sessions")).unwrap();

        let got = resolve_read_path(&synth, Some(real.clone()), "sessions");
        assert_eq!(got, Some(real.join("sessions")));
        let _ = std::fs::remove_dir_all(synth.parent().unwrap());
    }

    /// A dangling link is not a hit: the real entry was deleted since the last
    /// launch, and the agent's next write goes *through* the link into the real
    /// home — so the real path is where to look.
    #[test]
    fn a_dangling_link_falls_back_to_the_real_home() {
        let (synth, real) = home_pair("dangling");
        std::os::unix::fs::symlink(real.join("state_5.sqlite"), synth.join("state_5.sqlite"))
            .unwrap();

        let got = resolve_read_path(&synth, Some(real.clone()), "state_5.sqlite");
        assert_eq!(got, Some(real.join("state_5.sqlite")));
        let _ = std::fs::remove_dir_all(synth.parent().unwrap());
    }

    /// No real home resolves at all (no `$HOME`): the synthetic one still
    /// answers for anything it holds, and nothing else has a path.
    #[test]
    fn without_a_real_home_only_the_synthetic_one_answers() {
        let (synth, _real) = home_pair("nohome");
        std::fs::write(synth.join("state_5.sqlite"), b"db").unwrap();

        assert_eq!(
            resolve_read_path(&synth, None, "state_5.sqlite"),
            Some(synth.join("state_5.sqlite"))
        );
        assert_eq!(resolve_read_path(&synth, None, "sessions"), None);
        let _ = std::fs::remove_dir_all(synth.parent().unwrap());
    }

    /// Both homes' wal are watched: this runs once, at watcher setup, and the
    /// first Codex launch on a fresh host can still move which home holds the
    /// db. A watcher latched onto one path would then never wake.
    #[test]
    fn both_homes_wal_are_watched() {
        let paths = watch_paths();
        assert!(
            paths.iter().any(|p| p.starts_with(synth_home())),
            "synthetic home's wal is watched: {paths:?}"
        );
        assert!(paths.iter().all(|p| p.ends_with("state_5.sqlite-wal")));
    }

    #[test]
    fn seed_hook_trust_writes_state_and_preserves_config() {
        // Build a throwaway synth home with our real hooks.json + a user config.
        let home = std::env::temp_dir().join(format!(
            "captain-miao-seed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .elapsed()
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("hooks.json"), build_hooks_settings("/run/x.sock")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "model = \"gpt-5.5\"\n\n[projects.\"/tmp/x\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();

        seed_hook_trust(&home);

        let written = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let doc: toml::Table = written.parse().unwrap();
        // User settings survive the rewrite.
        assert_eq!(doc.get("model").and_then(|v| v.as_str()), Some("gpt-5.5"));
        assert!(doc.get("projects").is_some(), "project trust preserved");
        // One trust entry per emitted hook, each a sha256, keyed by the
        // hooks.json path + Codex label + group/handler indices.
        let state = doc["hooks"]["state"].as_table().unwrap();
        assert_eq!(state.len(), 8);
        let hooks_path = home.join("hooks.json");
        let key = format!("{}:pre_tool_use:0:0", hooks_path.display());
        let hash = state[&key]["trusted_hash"].as_str().unwrap();
        assert!(hash.starts_with("sha256:") && hash.len() == 71);

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_event_label_covers_emitted_hooks() {
        // Every event build_hooks_settings writes must map to a Codex label, or
        // seed_hook_trust would skip it and that hook would prompt.
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
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(dispatch_hook(&mut state, msg));
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
