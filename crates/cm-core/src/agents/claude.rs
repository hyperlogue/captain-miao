//! Claude Code backend. Owns every Claude-specific path, JSON shape, and
//! hook event mapping. The dashboard reaches everything in here only via
//! `crate::agent::AgentControl::Claude`'s match arms.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::{collapse_whitespace, shell_quote};

use crate::agent::{
    AgentActivity, BgSeedKind, BgShell, ResumeCandidate, SessionIndex, SessionIndexCache,
    SessionIndexEntry, TranscriptScan, TranscriptStats,
};
use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// The executable this backend drives. Named once so the seam's availability
/// check and the launch path can't disagree about what to look for.
pub(crate) const BIN: &str = "claude";

// =============================================================================
// Filesystem locations
// =============================================================================

/// Claude Code's config directory. Honours `CLAUDE_CONFIG_DIR` — the same env
/// var Claude Code itself reads — so a caller that relocates the agent's home
/// (an isolated demo/test instance) gets a captain-miao that reads the *same*
/// transcripts, session manifests and resume list the agent is writing. Without
/// this the two disagree: the agent would write to the relocated home while the
/// dashboard kept scanning `~/.claude`, surfacing the user's real sessions.
/// Mirrors [`codex_home`](super::codex)'s `CODEX_HOME` handling, including
/// treating an empty value as unset.
fn claude_home() -> Option<PathBuf> {
    resolve_claude_home(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// The precedence itself, split from the env/home reads so it's unit-testable
/// without mutating process-global state (`set_var` is unsafe in edition 2024,
/// and env mutation races parallel tests). An empty `CLAUDE_CONFIG_DIR` is
/// treated as unset, per the same convention `codex_home` uses.
fn resolve_claude_home(configured: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = configured
        && !p.as_os_str().is_empty()
    {
        return Some(p);
    }
    home.map(|h| h.join(".claude"))
}

/// `~/.claude/sessions` — the per-pid session manifests Claude writes (names and
/// the live `status` field). Single source for the index scan, the dashboard
/// watch, and the per-pid status read.
fn sessions_dir() -> Option<PathBuf> {
    Some(claude_home()?.join("sessions"))
}

/// Dirs whose changes should trigger a dashboard reload. Only the session-name
/// store (`~/.claude/sessions`, read by [`read_session_index`]) — *not* the
/// transcript dir: transcript-derived fields are folded by the launcher and
/// reach the dashboard via the session state file, so `projects/` is no longer
/// watched.
pub fn watch_paths() -> Vec<PathBuf> {
    let Some(home) = claude_home() else {
        return Vec::new();
    };
    vec![home.join("sessions")]
}

// =============================================================================
// Session-name index (~/.claude/sessions/<pid>.json)
// =============================================================================

/// Scan `~/.claude/sessions/*.json`, reusing cached entries when the file's
/// mtime is unchanged. Returns a fresh `SessionIndex` built from the cache.
///
/// The `name` is filtered through [`parse_session_name`], so only a user
/// `/rename` lands in the index — Claude's auto-derived slug
/// (`nameSource:"derived"`) is dropped, matching the launcher's `state.name`
/// fold, so both precedence steps that read this file agree and an un-renamed
/// row falls through to the first prompt.
pub fn read_session_index(cache: &mut SessionIndexCache) -> SessionIndex {
    #[derive(Deserialize)]
    struct ClaudeSession {
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
    }

    let dir = match sessions_dir() {
        Some(d) => d,
        None => {
            cache.clear();
            return SessionIndex::default();
        }
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => {
            cache.clear();
            return SessionIndex::default();
        }
    };

    let mut seen: HashSet<u32> = HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        seen.insert(pid);

        let mtime = entry.metadata().and_then(|m| m.modified()).ok();
        if let Some(prev) = cache.get(&pid)
            && prev.mtime == mtime
        {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Ok(s) = serde_json::from_str::<ClaudeSession>(&content) else {
            continue;
        };
        cache.insert(
            pid,
            SessionIndexEntry {
                mtime,
                session_id: s.session_id,
                // Same rename-only filter as the launcher's `read_session_name`:
                // Claude's auto-derived `project-name-hash` slug is marked
                // `nameSource:"derived"` and dropped here so the dashboard's
                // `session_display_name` falls through to the folded first prompt;
                // only an explicit `/rename` surfaces in the index.
                name: parse_session_name(&content),
            },
        );
    }
    cache.retain(|pid, _| seen.contains(pid));

    let mut out = SessionIndex::default();
    for (&pid, entry) in cache.iter() {
        if let Some(sid) = entry.session_id.as_ref() {
            out.session_id_by_pid.insert(pid, sid.clone());
        }
        if let Some(name) = entry.name.as_ref() {
            out.by_pid.insert(pid, name.clone());
            if let Some(sid) = entry.session_id.as_ref() {
                out.by_session_id.insert(sid.clone(), name.clone());
            }
        }
    }
    out
}

// =============================================================================
// Transcript reading
// =============================================================================

/// Context-token total and model from a full pass over the transcript. The
/// derivation lives in [`StatsCursor`]; production reads go through
/// [`read_transcript_stats_incremental`], so this whole-file variant is kept
/// only as the test oracle the incremental path is checked against.
///
/// Context tokens: the latest assistant-message total. Skips API errors and
/// zero-usage `/branch` markers, and recovers a meaningful baseline across
/// `/compact`. `/compact` writes a `{type:"user", isCompactSummary:true}` entry
/// containing the summary text. Until the next assistant turn lands, the latest
/// assistant message still reflects pre-compact context (often near the limit).
/// To estimate post-compact context we remember the total of the *first*
/// assistant message after the most-recent prior compact in the same session;
/// sessions on their first compact fall back to the first assistant total plus a
/// rough summary estimate.
///
/// Model: `message.model` of the latest non-error assistant turn (which the same
/// loop already parses), skipping `<synthetic>` interrupt/error placeholders. The
/// model can change mid-session via `/model`, so last-wins; it survives
/// `/compact` (compaction doesn't change the model). None before the first turn.
#[cfg(test)]
pub fn read_transcript_stats(path: &Path) -> TranscriptStats {
    use std::io::{BufRead, BufReader};
    let Ok(file) = std::fs::File::open(path) else {
        return TranscriptStats::default();
    };
    let reader = BufReader::new(file);
    let mut cursor = StatsCursor::default();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        cursor.fold_line(&line);
    }
    cursor.into_stats()
}

/// Incremental sibling of [`read_transcript_stats`]: folds only the transcript
/// bytes appended since `prior`'s cursor offset, so an actively-growing session
/// isn't rescanned end-to-end on every reload. A trailing partial (not yet
/// newline-terminated) line is held back and re-read once complete, so a
/// half-written JSON line is never parsed and never skipped. Falls back to a
/// full reparse if the file shrank/rotated below the cursor or a seek fails.
pub fn read_transcript_stats_incremental(
    path: &Path,
    prior: Option<&TranscriptStats>,
) -> TranscriptStats {
    use std::io::{Read, Seek, SeekFrom};

    let mut cursor = prior.and_then(|p| p.cursor.clone()).unwrap_or_default();

    let Ok(mut file) = std::fs::File::open(path) else {
        return TranscriptStats::default();
    };
    let Ok(meta) = file.metadata() else {
        return TranscriptStats::default();
    };
    let len = meta.len();
    // Truncated / rotated below where we'd parsed: the offset is meaningless,
    // so discard the accumulators and reparse from the top.
    if len < cursor.offset {
        cursor = StatsCursor::default();
    }
    // No new bytes since the cached parse — return its derived stats unchanged.
    if len == cursor.offset {
        return cursor.into_stats();
    }
    if file.seek(SeekFrom::Start(cursor.offset)).is_err() {
        // Seek failed: reparse from the start rather than risk a torn offset.
        cursor = StatsCursor::default();
        if file.seek(SeekFrom::Start(0)).is_err() {
            return TranscriptStats::default();
        }
    }
    let mut bytes: Vec<u8> = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return cursor.into_stats();
    }
    // Consume only through the last complete (newline-terminated) line; the
    // trailing partial line, if any, stays unparsed for the next read.
    let Some(last_nl) = bytes.iter().rposition(|&b| b == b'\n') else {
        return cursor.into_stats();
    };
    let complete = &bytes[..=last_nl];
    for line in String::from_utf8_lossy(complete).lines() {
        cursor.fold_line(line);
    }
    cursor.offset += complete.len() as u64;
    cursor.into_stats()
}

#[derive(Deserialize)]
struct StatsUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}
#[derive(Deserialize)]
struct StatsMessage {
    #[serde(default)]
    usage: Option<StatsUsage>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct StatsEntry {
    #[serde(default, rename = "type")]
    entry_type: Option<String>,
    #[serde(default, rename = "isApiErrorMessage")]
    is_api_error: bool,
    #[serde(default, rename = "isCompactSummary")]
    is_compact_summary: bool,
    #[serde(default)]
    message: Option<StatsMessage>,
}

/// Running accumulators for the transcript-stats parse, persisted across reloads
/// so the dashboard can fold only newly-appended lines instead of rescanning the
/// whole file. `offset` is the byte position up to which lines have been folded.
/// Fields are private — the dashboard carries the whole cursor as opaque state.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StatsCursor {
    offset: u64,
    last_total: Option<u64>,
    first_total: Option<u64>,
    last_post_compact_total: Option<u64>,
    just_saw_compact: bool,
    compact_pending: bool,
    last_summary_chars: usize,
    last_model: Option<String>,
    /// First real user prompt (first-wins), the auto-title fallback.
    first_prompt: Option<String>,
}

impl StatsCursor {
    /// Fold one transcript line into the accumulators. A cheap substring
    /// pre-filter skips the lines that are neither an assistant-usage turn nor a
    /// compact summary before paying for a JSON parse.
    fn fold_line(&mut self, line: &str) {
        // The first-prompt auto-title is folded from its own line kind (a user
        // message, not an assistant-usage/compact line), so it's handled before the
        // stats pre-filter that would otherwise drop it. It's the display title
        // whenever the user hasn't `/rename`d — the launcher reads only a *rename*
        // from the session file (`read_session_name`, which ignores Claude's auto
        // slug) and rides it to remote rows, so first-prompt is the fallback shown
        // until then.
        if self.first_prompt.is_none()
            && line.contains("\"user\"")
            && let Some(p) = parse_first_user_prompt_line(line)
        {
            self.first_prompt = Some(p);
            // A user line is never an assistant-usage/compact line, so it falls
            // through to the stats pre-filter, which returns below.
        }
        let is_compact = line.contains("\"isCompactSummary\":true");
        // Every assistant turn carries both `usage` and `model`; the same line
        // filter feeds both the token total and the model.
        let is_assistant_usage = line.contains("\"assistant\"") && line.contains("\"usage\"");
        if !is_compact && !is_assistant_usage {
            return;
        }
        let entry: StatsEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => return,
        };
        if entry.is_compact_summary {
            self.last_summary_chars = entry
                .message
                .and_then(|m| m.content)
                .and_then(|c| c.as_str().map(|s| s.len()))
                .unwrap_or(0);
            self.last_total = None;
            self.just_saw_compact = true;
            self.compact_pending = true;
            return;
        }
        if entry.entry_type.as_deref() != Some("assistant") {
            return;
        }
        if entry.is_api_error {
            return;
        }
        let Some(message) = entry.message else {
            return;
        };
        if let Some(model) = message.model
            && !model.is_empty()
            && model != "<synthetic>"
        {
            self.last_model = Some(model);
        }
        if let Some(usage) = message.usage {
            let total = usage.input_tokens
                + usage.cache_creation_input_tokens
                + usage.cache_read_input_tokens;
            if total == 0 {
                return;
            }
            self.first_total.get_or_insert(total);
            if self.just_saw_compact {
                self.last_post_compact_total = Some(total);
                self.just_saw_compact = false;
            }
            self.last_total = Some(total);
            self.compact_pending = false;
        }
    }

    /// Derive the displayed context-token total from the current accumulators.
    /// After a compact and before the next assistant turn lands, fall back to
    /// the first post-compact baseline, then to first-total-plus-summary.
    fn context_tokens(&self) -> Option<u64> {
        if self.compact_pending {
            self.last_post_compact_total.or_else(|| {
                self.first_total
                    .map(|t| t + (self.last_summary_chars / 4) as u64)
            })
        } else {
            self.last_total
        }
    }

    fn into_stats(self) -> TranscriptStats {
        let context_tokens = self.context_tokens();
        let model = self.last_model.clone();
        let first_prompt = self.first_prompt.clone();
        TranscriptStats {
            context_tokens,
            model,
            first_prompt,
            name: None,
            last_prompt: None,
            cursor: Some(self),
        }
    }
}

/// Marker present in every shell the Bash tool spawns: the wrapper sources a
/// snapshot of the user's shell environment
/// (`…/.claude/shell-snapshots/snapshot-<shell>-<ts>-<rand>.sh`). Distinguishes
/// background-task shells from the agent's other children (MCP stdio servers,
/// helper processes), which never source one. Present in the *spawned* shell,
/// not necessarily in the surviving process image: a command that `exec`s
/// overwrites the wrapper's command line, which is why
/// [`classify_bg_shells`] can't treat it as the only way in.
const SHELL_SNAPSHOT_MARKER: &str = "/shell-snapshots/snapshot-";

/// The agent's currently-running `run_in_background` shells, each reduced to a
/// normalized command `key` and a static [`BgSeedKind`].
///
/// `None` means **the process tree could not be read**, and nothing may be
/// concluded from it. An empty `Some` means the tree read cleanly and the agent
/// has no background shell running — a positive fact, and the evidence the
/// launcher needs to retire a background status that has gone stale. Collapsing
/// the two (as this once did) makes "no shells" indistinguishable from "no
/// answer", which is why a row whose review-watch had ended could sit in
/// `ReviewPending` for the rest of its life. Read from the **live process tree** — the Bash
/// tool runs each background command in a wrapper shell that stays a direct
/// child of the agent for the task's lifetime (or that the command `exec`s over,
/// leaving itself as that same child — see [`classify_bg_shells`]) — so the
/// answer is present-tense truth and can't go stale: a task that ends with no
/// transcript marker
/// (stopped from the UI, a Monitor timeout, agent teardown, or a `--resume`
/// orphan from a previous process incarnation) simply isn't in the tree
/// anymore. (An earlier version folded launch/`<task-notification>` pairs from
/// the transcript instead, and leaked exactly those markerless stops.) Only
/// consulted while the session is at rest with a background shell
/// (`BackgroundActive`/`BackgroundServer`/`ReviewPending`), when no foreground
/// tool shell can be among the children.
pub fn bg_shells(agent_pid: u32) -> Option<Vec<BgShell>> {
    Some(classify_bg_shells(&child_cmdlines(agent_pid)?))
}

/// Reduce the agent's child command lines to its `run_in_background` shells,
/// each normalized to its eval'd command and classified by command text alone.
/// The wrapper embeds the eval'd command verbatim, so the classifiers match it
/// unchanged.
///
/// A child qualifies two ways. Normally it carries the
/// [`SHELL_SNAPSHOT_MARKER`] wrapper — the evidence separating a background task
/// from the agent's other children. But a command that `exec`s (`…; exec r3
/// watch review_…`, or a launcher script ending in `exec bun "$cli" "$@"`)
/// replaces that wrapper with itself, and the marker goes with it; the surviving
/// line is the command verbatim. So a review-watch is admitted **unwrapped**
/// too: `watch review_<hex>` / an r3 entrypoint is a distinctive enough form to
/// stand on its own, and it is the one classification whose loss is silent — the
/// row sits on `BackgroundActive` while a person is actually waiting.
///
/// Nothing else is admitted unwrapped, because nothing else can be told apart
/// from an MCP stdio server or helper by its text. An exec'd dev server is
/// therefore missed and its row stays busy, which is this module's standing bias
/// (see [`is_long_running_command`]) — and, more importantly, a child that isn't
/// a background task at all keeps leaving the **empty** set that
/// `promote_stale_background` needs to retire a stale row.
///
/// Total, since the caller has already read the tree: no background shell among
/// the children is an empty vec, never `None` (see [`bg_shells`]).
fn classify_bg_shells(cmdlines: &[String]) -> Vec<BgShell> {
    cmdlines
        .iter()
        .filter_map(|c| {
            // Classify the *normalized* command (the eval'd body), not the raw
            // wrapper: the wrapper's leading `bash -c source <snapshot> && …`
            // would shift a `npm run dev` off position 0 and hide it from the
            // token checks. The same normalized string is the learning key —
            // and for an exec'd child it is just the trimmed line itself.
            let key = normalize_bg_command(c);
            if is_r3_watch_command(&key) {
                return Some(BgShell {
                    key,
                    kind: BgSeedKind::ReviewWatch,
                });
            }
            // Past that form, the wrapper is the only evidence this child is a
            // background task rather than one of the agent's own helpers.
            if !c.contains(SHELL_SNAPSHOT_MARKER) {
                return None;
            }
            let kind = if is_long_running_command(&key) {
                BgSeedKind::LongRunning
            } else {
                BgSeedKind::Other
            };
            Some(BgShell { key, kind })
        })
        .collect()
}

/// Extract the agent's actual `run_in_background` command from the Bash-tool
/// wrapper, to serve as a stable learning key and as the text both classifiers
/// match on. Everything else in the wrapper (the per-session snapshot path with
/// its timestamp, the random cwd temp file) is volatile and must not be part of
/// the key, or "the same command" would never match across sessions.
///
/// The wrapper embeds the command as `… && eval '<cmd>' …` — but a bash
/// single-quoted string can't contain a `'`, so a command that *itself* holds
/// one is escaped as `'"'"'` (close the quote, emit a double-quoted quote,
/// reopen). The eval argument is therefore a **concatenation of quoted
/// segments**, not one quoted string, and the first `'` after `eval '` closes
/// only the first segment. Slicing there truncated
/// `nix develop <dir> --command bash -c '…r3 watch review_… '` down to
/// `nix develop <dir> --command bash -c`, which hid the r3 watch from
/// [`is_r3_watch_command`] (the row stuck at `BackgroundActive` instead of
/// `ReviewPending`) and collapsed every such command onto one learning key.
/// Parsing the argument as a whole shell word instead rebuilds the command the
/// agent asked to run, verbatim.
///
/// Falls back to the whole (trimmed) command line when no `eval '…'` is present
/// (or it parses to nothing), so an unusual wrapper still yields *some* stable
/// key rather than nothing.
pub fn normalize_bg_command(cmd: &str) -> String {
    const MARK: &str = "eval '";
    if let Some(start) = cmd.find(MARK) {
        // Resume *at* the opening quote (MARK includes it, and it's ASCII):
        // the argument is a sequence of quoted segments, so it has to be
        // parsed as a whole word rather than sliced at a quote.
        let word = unquote_shell_word(&cmd[start + MARK.len() - 1..]);
        if !word.is_empty() {
            return word;
        }
    }
    cmd.trim().to_string()
}

/// Unquote one shell word from the start of `s`, stopping at the first
/// *unquoted* whitespace. Covers the three quoting forms that can appear in the
/// wrapper's eval argument — `'…'` (everything literal), `"…"` (backslash
/// escapes a small set), and a bare backslash escape — and concatenates
/// adjacent segments, which is exactly how `'"'"'` rebuilds a literal `'`.
fn unquote_shell_word(s: &str) -> String {
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut out = String::new();
    let mut quote = Quote::None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (&quote, c) {
            // An unquoted space ends the argument (the wrapper's ` < /dev/null
            // && pwd -P >| …` tail starts here).
            (Quote::None, c) if c.is_whitespace() => break,
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::None, '\\') => out.extend(chars.next()),
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::Double, '"') => quote = Quote::None,
            // Inside double quotes a backslash escapes only these; before
            // anything else it stays a literal backslash (bash semantics).
            (Quote::Double, '\\') => match chars.next() {
                Some(e @ ('$' | '`' | '"' | '\\')) => out.push(e),
                // Line continuation: both characters vanish.
                Some('\n') => {}
                Some(e) => {
                    out.push('\\');
                    out.push(e);
                }
                None => out.push('\\'),
            },
            (_, c) => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Seed heuristic: does `cmd` look like a **long-running service** — a dev
/// server or file watcher that runs indefinitely and that the agent parked and
/// moved on from — rather than a finite build/test/step it's waiting to finish?
///
/// Deliberately conservative: an unrecognized command is *not* long-running, so
/// the caller keeps it busy (keep-awake). Keeping the machine awake for a
/// mystery task is safe; parking a real in-progress build is not. The learned
/// store (`crate::learned`) catches the long-running commands this list misses,
/// after they've been observed running past the threshold once — so this only
/// needs to cover the common cases well enough to avoid a cold-start wait, not
/// be exhaustive.
pub fn is_long_running_command(cmd: &str) -> bool {
    let lc = cmd.to_ascii_lowercase();
    // Trim wrapping quotes so a token like `dev'` (from the eval quoting) still
    // matches `dev`; take the path basename so `/usr/bin/nodemon` matches
    // `nodemon` and a dir named `.../myserver` doesn't leak a false `server`.
    fn base(t: &str) -> &str {
        t.trim_matches(|c| c == '\'' || c == '"' || c == '`')
            .rsplit('/')
            .next()
            .unwrap_or(t)
    }
    let tokens: Vec<&str> = lc.split_whitespace().map(base).collect();

    // A watch loop: a rebuild-/rerun-on-change watcher, almost always long-lived.
    if tokens.iter().any(|t| *t == "--watch" || *t == "--watchall") {
        return true;
    }

    // Explicit two-word server/watch forms that a bare-token check would miss.
    const LONG_PAIRS: &[(&str, &str)] = &[
        ("cargo", "watch"),
        ("next", "dev"),
        ("next", "start"),
        ("nuxt", "dev"),
        ("astro", "dev"),
        ("gatsby", "develop"),
        ("remix", "dev"),
        ("compose", "up"),
        ("docker-compose", "up"),
        ("flask", "run"),
        ("rails", "server"),
        ("rails", "s"),
        ("expo", "start"),
        ("react-native", "start"),
    ];
    if tokens
        .windows(2)
        .any(|w| LONG_PAIRS.iter().any(|(a, b)| w[0] == *a && w[1] == *b))
    {
        return true;
    }

    // A one-shot build/test/lint step overrides any server-ish tool name it
    // wraps (`vite build`, `next build`, `npm run build`) → treat as transient.
    // Checked *after* the watch/pair forms so `cargo watch -x build` stays long.
    const TRANSIENT_SUBCMDS: &[&str] = &[
        "build",
        "test",
        "check",
        "lint",
        "fmt",
        "format",
        "bench",
        "typecheck",
        "tsc",
    ];
    if tokens.iter().any(|t| TRANSIENT_SUBCMDS.contains(t)) {
        return false;
    }

    // Package-manager run-scripts whose name denotes a dev loop:
    //   npm|pnpm|yarn|bun [run] <dev|serve|start|watch|develop|server>
    const PKG_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun"];
    const LONG_SCRIPTS: &[&str] = &["dev", "serve", "start", "watch", "develop", "server"];
    if tokens.first().is_some_and(|t| PKG_MANAGERS.contains(t)) {
        let script = if tokens.get(1) == Some(&"run") {
            tokens.get(2)
        } else {
            tokens.get(1)
        };
        if script.is_some_and(|s| LONG_SCRIPTS.contains(s)) {
            return true;
        }
    }

    // Standalone server/watcher programs (matched on any token's basename).
    const LONG_PROGRAMS: &[&str] = &[
        "nodemon",
        "watchexec",
        "entr",
        "watchman",
        "vite",
        "nuxt",
        "gatsby",
        "astro",
        "webpack-dev-server",
        "http-server",
        "live-server",
        "json-server",
        "browser-sync",
        "livereload",
        "uvicorn",
        "gunicorn",
        "hypercorn",
        "daphne",
        "puma",
        "rackup",
        "foreman",
        "overmind",
        "jekyll",
        "hugo",
        "mkdocs",
        "docusaurus",
        "storybook",
        "start-storybook",
        "wrangler",
        "netlify",
        "vercel",
        "streamlit",
        "trunk",
        "bacon",
        "air",
        "ts-node-dev",
        "tsx",
        "metro",
        "cargo-watch",
    ];
    if tokens.iter().any(|t| LONG_PROGRAMS.contains(t)) {
        return true;
    }

    // Generic server tokens: a bare `serve` subcommand (`ng serve`, `webpack
    // serve`, `php artisan serve`) or a token ending in `server`/`runserver`
    // (`rails server`, `manage.py runserver`, `python -m http.server`). Reached
    // only after the transient guard, so these are dev servers.
    tokens
        .iter()
        .any(|t| *t == "serve" || t.ends_with("server"))
}

/// Command lines (argv joined with spaces) of every direct child of `pid`.
/// Linux: one `/proc` walk matching each process's ppid (field 4 of
/// `/proc/<pid>/stat` — more robust than `/proc/<pid>/task/*/children`, which
/// is per-*thread* and documented unreliable for running children). `None` when
/// `/proc` can't be read.
#[cfg(target_os = "linux")]
fn child_cmdlines(pid: u32) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(child) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{child}/stat")) else {
            continue;
        };
        if parse_stat_ppid(&stat) != Some(pid) {
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{child}/cmdline")) else {
            continue;
        };
        // argv is NUL-separated (with a trailing NUL).
        let cmd = raw
            .split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" ");
        if !cmd.is_empty() {
            out.push(cmd);
        }
    }
    Some(out)
}

/// macOS (and other non-Linux unix) has no `/proc`: one `ps` sweep lists every
/// process's ppid + full command line. `-ww` matters — without it ps truncates
/// the command to the window width, hiding the eval'd command the classifier
/// needs.
#[cfg(not(target_os = "linux"))]
fn child_cmdlines(pid: u32) -> Option<Vec<String>> {
    let output = std::process::Command::new("ps")
        .args(["-Aww", "-o", "ppid=,command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_ps_child_cmdlines(
        &String::from_utf8_lossy(&output.stdout),
        pid,
    ))
}

/// The ppid is field 4 of `/proc/<pid>/stat`, two fields past the parenthesized
/// comm — which can itself contain spaces and parens, so split at the *last*
/// `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the comm: state, ppid, pgrp, …
    rest.split_ascii_whitespace().nth(1)?.parse().ok()
}

/// Filter `ps -Aww -o ppid=,command=` output down to the command lines of
/// `pid`'s direct children. Each line is a right-aligned ppid, whitespace, then
/// the full command.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_child_cmdlines(ps_output: &str, pid: u32) -> Vec<String> {
    ps_output
        .lines()
        .filter_map(|line| {
            let (ppid, cmd) = line.trim_start().split_once(char::is_whitespace)?;
            (ppid.parse::<u32>().ok()? == pid).then(|| cmd.trim_start().to_string())
        })
        .collect()
}

/// True when `cmd` invokes the r3 review CLI's `watch` subcommand — the agent
/// blocking on a human review. r3 review ids are `review_<hex>`, so the
/// `watch review_` form is unambiguous; the entrypoint forms cover an
/// alternate id scheme. Stays narrow enough not to fire on unrelated watchers
/// (`cargo watch`, `npm run watch`, `watch -n1 …`). Matches the documented
/// invocations:
///   r3 watch <review-id>                        (alias / compiled binary)
///   bun <…>/r3/cli/index.ts watch <review-id>   (run from source)
fn is_r3_watch_command(cmd: &str) -> bool {
    // The distinctive review-id form: `watch review_<hex>`.
    if cmd.contains("watch review_") {
        return true;
    }
    // An r3 entrypoint token immediately followed by the `watch` subcommand.
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    tokens
        .windows(2)
        .any(|w| w[1] == "watch" && is_r3_entrypoint(w[0], cmd))
}

/// Whether `token` (a command word preceding `watch`) is an r3 entrypoint: the
/// `r3` binary/alias (bare or path-suffixed), or the run-from-source
/// `…/r3/cli/index.ts` script.
fn is_r3_entrypoint(token: &str, cmd: &str) -> bool {
    if token == "r3" || token.ends_with("/r3") {
        return true;
    }
    // `bun …/r3/cli/index.ts watch …`: the script lives under an `r3` checkout.
    token.ends_with("index.ts") && cmd.contains("/r3/")
}

/// Parse one `{"type":"custom-title","customTitle":"..."}` line into its trimmed
/// title, or None if the line isn't a (non-empty) custom-title entry. Shared by
/// the incremental [`StatsCursor`] fold and the standalone [`read_custom_title`].
fn parse_custom_title_line(line: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Entry {
        #[serde(default, rename = "type")]
        entry_type: Option<String>,
        #[serde(default, rename = "customTitle")]
        custom_title: Option<String>,
    }
    let entry: Entry = serde_json::from_str(line).ok()?;
    if entry.entry_type.as_deref() != Some("custom-title") {
        return None;
    }
    let trimmed = entry.custom_title?.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Parse one transcript line into the user prompt it carries, or None if it isn't
/// a real (non-sidechain, non-meta) user message. Shared by the incremental
/// [`StatsCursor`] fold and the standalone [`read_first_user_prompt`].
fn parse_first_user_prompt_line(line: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(line).ok()?;
    if val.get("type").and_then(|t| t.as_str()) != Some("user") {
        return None;
    }
    if val.get("isSidechain").and_then(|b| b.as_bool()) == Some(true) {
        return None;
    }
    if val.get("isMeta").and_then(|b| b.as_bool()) == Some(true) {
        return None;
    }
    extract_user_prompt(val.pointer("/message/content")?)
}

/// Latest `/rename` entry's title. Claude writes
/// `{"type":"custom-title","customTitle":"..."}` to the transcript the moment
/// `/rename` runs — the only place the new name lands until the session exits.
pub fn read_custom_title(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};

    // Scan the WHOLE file, not just the tail: an early `/rename` in a long
    // session lives past the 256KB tail window, and this only runs in the
    // resume/fork list path where reading the whole file is cheap enough.
    // `read_until` + lossy decode keeps a non-UTF-8 byte from panicking.
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut last: Option<String> = None;
    let mut raw: Vec<u8> = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let line = String::from_utf8_lossy(&raw);
        if !line.contains("\"custom-title\"") {
            continue;
        }
        if let Some(t) = parse_custom_title_line(&line) {
            last = Some(t);
        }
    }
    last
}

/// Scan `~/.claude/projects/*/*.jsonl` for resumable Claude Code sessions.
/// Returns up to `limit` candidates sorted by mtime (most recent first).
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    let projects_dir = claude_home()
        .ok_or_else(|| anyhow::anyhow!("no home dir"))?
        .join("projects");
    let proj_entries = std::fs::read_dir(&projects_dir)?;

    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for proj in proj_entries.flatten() {
        let proj_path = proj.path();
        if !proj_path.is_dir() {
            continue;
        }
        let Ok(transcripts) = std::fs::read_dir(&proj_path) else {
            continue;
        };
        for tr in transcripts.flatten() {
            let path = tr.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = tr.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            files.push((path, mtime));
        }
    }
    files.sort_by_key(|b| std::cmp::Reverse(b.1));
    files.truncate(limit);

    let mut out = Vec::with_capacity(files.len());
    for (path, mtime) in files {
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let header = read_transcript_header(&path);
        let cwd = match header.cwd {
            Some(c) => c,
            None => continue,
        };
        let custom_title = read_custom_title(&path);
        out.push(ResumeCandidate {
            agent: crate::agent::AgentControl::Claude,
            session_id,
            cwd,
            first_prompt: header.first_prompt,
            custom_title,
            git_branch: header.git_branch,
            mtime,
        });
    }
    Ok(out)
}

#[derive(Debug, Default)]
struct TranscriptHeader {
    cwd: Option<String>,
    first_prompt: Option<String>,
    git_branch: Option<String>,
}

/// Read up to ~200 lines and extract cwd, first real user prompt, and gitBranch.
fn read_transcript_header(path: &Path) -> TranscriptHeader {
    use std::io::{BufRead, BufReader};

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return TranscriptHeader::default(),
    };
    let reader = BufReader::new(file);
    let mut header = TranscriptHeader::default();
    for line in reader.lines().take(200).map_while(Result::ok) {
        let Ok(val): std::result::Result<serde_json::Value, _> = serde_json::from_str(&line) else {
            continue;
        };
        if header.cwd.is_none()
            && let Some(c) = val.get("cwd").and_then(|c| c.as_str())
        {
            header.cwd = Some(c.to_string());
        }
        if header.git_branch.is_none()
            && let Some(b) = val.get("gitBranch").and_then(|b| b.as_str())
            && !b.is_empty()
        {
            header.git_branch = Some(b.to_string());
        }
        if header.first_prompt.is_none()
            && val.get("type").and_then(|t| t.as_str()) == Some("user")
            && val.get("isSidechain").and_then(|b| b.as_bool()) != Some(true)
            && val.get("isMeta").and_then(|b| b.as_bool()) != Some(true)
            && let Some(content) = val.pointer("/message/content")
            && let Some(p) = extract_user_prompt(content)
        {
            header.first_prompt = Some(p);
        }
        if header.cwd.is_some() && header.first_prompt.is_some() && header.git_branch.is_some() {
            break;
        }
    }
    header
}

/// Extract a user-facing prompt string from a transcript `user` entry's
/// `message.content` field, or None if this entry should be skipped
/// (meta/caveat wrapper, tool_result payload, slash command, XML wrapper).
fn extract_user_prompt(content: &serde_json::Value) -> Option<String> {
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else {
        // A value that is neither a string nor an array carries no prompt, so
        // the `?` here is the same "skip this entry" exit as the empty and
        // wrapper cases below.
        let arr = content.as_array()?;
        let mut parts = Vec::new();
        for block in arr {
            let ty = block.get("type").and_then(|t| t.as_str());
            if ty == Some("text")
                && let Some(t) = block.get("text").and_then(|t| t.as_str())
            {
                parts.push(t.to_string());
            }
        }
        if parts.is_empty() {
            return None;
        }
        parts.join(" ")
    };

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Skip Claude's synthetic <local-command-*> wrappers: the caveat prepended
    // on /resume entry points (<local-command-caveat>) and the output a slash
    // command echoes back as a user entry (<local-command-stdout> /
    // <local-command-stderr> — e.g. `/model` writing "Set model to …", or a
    // compaction notice). These are system-control messages, not real prompts.
    if trimmed.contains("<local-command-") {
        return None;
    }

    // Skip slash-command invocations wrapped in <command-name>...</command-name>
    // (and their sibling <command-message>/<command-args> parts).
    if tag_contents(trimmed, "command-name").is_some()
        || tag_contents(trimmed, "command-message").is_some()
        || tag_contents(trimmed, "command-args").is_some()
    {
        return None;
    }

    // Skip XML system wrappers (e.g. stand-alone <system-reminder>) with no prose.
    if trimmed.starts_with('<') && trimmed.ends_with('>') && !trimmed.contains('\n') {
        let inner = trimmed.trim_start_matches('<').trim_end_matches('>');
        if inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c))
        {
            return None;
        }
    }

    let cleaned = strip_ansi_escapes(trimmed);
    let cleaned = collapse_whitespace(&cleaned);
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned)
}

/// Strip ANSI/CSI/OSC escape sequences. Recognises CSI (`ESC [ … final`),
/// OSC (`ESC ] … BEL|ST`), and 2-byte ESC introducers. Lone control bytes
/// other than tab/newline are stripped — they would otherwise mangle a
/// single-line picker row.
fn strip_ansi_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for p in chars.by_ref() {
                        if matches!(p, '\x40'..='\x7e') {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(p) = chars.next() {
                        if p == '\x07' {
                            break;
                        }
                        if p == '\x1b' {
                            if matches!(chars.peek(), Some('\\')) {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        if (c as u32) < 0x20 && c != '\t' && c != '\n' {
            continue;
        }
        out.push(c);
    }
    out
}

fn tag_contents<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end_rel = s[start..].find(&close)?;
    Some(&s[start..start + end_rel])
}

// =============================================================================
// Launcher: process spawn + hooks settings
// =============================================================================

/// The variables a *running* Claude Code session exports to its own
/// subprocesses — every one of them a fact about that session, none of them
/// true of a new one.
///
/// They reach us because a terminal hands its own environment to every command
/// it runs, and a terminal is routinely started from a shell: an emulator (or a
/// tmux/zellij server) launched from inside an agent session carries that
/// session's variables into every window the dashboard later opens in it. The
/// same inheritance `wrap_env` re-points `PATH` for, one variable further.
///
/// The visible cost is the marker Claude Code checks first:
/// `CLAUDE_CODE_CHILD_SESSION` says "you are a subagent of a session that is
/// already recording", so an inherited one turns transcript saving **off** and
/// the new session says so on its first screen. That is also *our* loss — this
/// backend folds that transcript for status, title and context tokens, and
/// treats the session file as authoritative — so a spawned session with no
/// transcript is a row that never fills in. The rest name the parent's identity
/// and its IPC channel, which a second session must never speak on.
///
/// Deliberately narrow: only variables scoped to one session's *run*. The
/// user's own settings (`CLAUDE_CONFIG_DIR`, effort, feature flags) are theirs
/// and are inherited on purpose. Other backends have no equivalent measured
/// yet; one that turns out to leak the same way adds its own list beside this.
const PARENT_SESSION_ENV: [&str; 5] = [
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
];

/// Drop [`PARENT_SESSION_ENV`] from a launch, so the session starts as its own
/// rather than as a continuation of whatever launched the terminal.
fn clear_parent_session_env(cmd: &mut Command) {
    for key in PARENT_SESSION_ENV {
        cmd.env_remove(key);
    }
}

pub fn build_launch_command(
    cwd: &str,
    _sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    clear_parent_session_env(&mut cmd);
    cmd.env("CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR", "1");
    cmd.arg("--settings").arg(settings_path);
    cmd.args(extra_args);
    Ok(cmd)
}

/// Build the `--settings` JSON. Claude Code's hook keys are PascalCase; we
/// keep the launcher's kebab-case internally and map here. `UserPromptSubmit`
/// is the one outlier — Claude uses that name but we surface it as
/// `prompt-submit`.
pub fn build_hooks_settings(sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    let exe_q = shell_quote(&exe.to_string_lossy());
    let sock_q = shell_quote(sock_path);

    let hook_cmd = |event: HookEvent| -> serde_json::Value {
        serde_json::json!([{
            "hooks": [{"command": format!("{exe_q} hook --sock {sock_q} {}", event.as_kebab()), "type": "command"}],
            "matcher": "*"
        }])
    };

    serde_json::json!({
        "hooks": {
            "SessionStart":       hook_cmd(HookEvent::SessionStart),
            "UserPromptSubmit":   hook_cmd(HookEvent::PromptSubmit),
            "PreToolUse":         hook_cmd(HookEvent::PreToolUse),
            "PostToolUse":        hook_cmd(HookEvent::PostToolUse),
            "PostToolUseFailure": hook_cmd(HookEvent::PostToolUseFailure),
            "PermissionRequest":  hook_cmd(HookEvent::PermissionRequest),
            "Elicitation":        hook_cmd(HookEvent::Elicitation),
            "ElicitationResult":  hook_cmd(HookEvent::ElicitationResult),
            "Stop":               hook_cmd(HookEvent::Stop),
            "StopFailure":        hook_cmd(HookEvent::StopFailure),
            "PreCompact":         hook_cmd(HookEvent::PreCompact),
            "PostCompact":        hook_cmd(HookEvent::PostCompact),
            "CwdChanged":         hook_cmd(HookEvent::CwdChanged),
        }
    })
    .to_string()
}

// =============================================================================
// Hook payload (stdin from Claude Code → normalized HookMessage)
// =============================================================================

#[derive(Deserialize)]
struct HookPayload {
    session_id: Option<String>,
    tool_name: Option<String>,
    message: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    transcript_path: Option<String>,
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse hook JSON from stdin")?;
    Ok(HookMessage {
        event,
        session_id: payload.session_id,
        tool_name: payload.tool_name,
        message: payload.message,
        cwd: payload.cwd,
        prompt: payload.prompt,
        // Claude's payload has no title; a `/rename` reaches `name` through the
        // session-file fold instead.
        session_title: None,
        // Claude's tokens and model come from the transcript fold, which is
        // richer here: it is incremental and yields the first prompt too.
        context_tokens: None,
        model: None,
        transcript_path: payload.transcript_path,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Claude's departures from [`common::dispatch_default`]; everything else maps
/// the way every backend maps it.
pub async fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    // Claude mints a new sessionId on `/resume`; always take the freshest.
    common::adopt_session_facts(state, &mut msg);

    match msg.event {
        // AskUserQuestion fires a PermissionRequest like any gated tool, but
        // it's a question/answer selection, not a permission grant. Route it
        // to WaitingForDecision ("Decision") — same bucket as Elicitation,
        // the other "agent is asking the user something" case — so the
        // dashboard can tell it apart from a real tool-approval gate. Every
        // other tool takes the shared mapping (WaitingForApproval).
        HookEvent::PermissionRequest if msg.tool_name.as_deref() == Some("AskUserQuestion") => {
            state.status = SessionStatus::WaitingForDecision;
        }
        HookEvent::Stop => {
            state.last_tool = None;
            // The model's turn ended — but it may have ended only because a
            // background *subagent* (the Agent tool) is still running, in which
            // case forcing `Idle` here would flash the row to rest while real
            // work continues (Claude re-drives the agent as subagents report).
            // Unlike a `run_in_background` shell a subagent is in-process (no
            // `"shell"` status, no child process to scan), so the one signal is
            // Claude's own session file, which it holds at `"busy"` throughout.
            // Defer to it: hold `Active` while it still reports work, and let the
            // file's eventual idle-write wake the launcher so the demote-only
            // reconciliation settles us to `Idle` exactly when Claude goes idle.
            // A missing/unreadable file (`None`) falls back to `Idle` — the
            // pre-subagent behaviour, and the safe default if the signal is gone.
            // (One small synchronous read per turn end, like the startup name
            // fold; the reconciliation's own reads stay offloaded on hot paths.)
            let activity = state.child_pid.and_then(session_activity);
            state.status = status_after_stop(&state.status, activity);
        }
        HookEvent::PostCompact => {
            // A manual `/compact` fires no `Stop`, so this hook is also the turn
            // end — and it must answer the same question `Stop` does: did a
            // `run_in_background` shell outlive it? If so the row's real shape
            // is that shell's (`Task`/`Server`/`Review`, resolved by the
            // launcher's classifier), not the compaction's.
            //
            // Paired with the `Compacted` arm of `reconcile_activity`, which
            // covers the opposite interleaving: Claude's status write and this
            // hook race, and whichever lands second is the one that converges
            // the row. Reading here catches a file already back to `"shell"`
            // (the reconciliation can't — it skips `Compacting`); the
            // reconciliation catches a file written after this hook.
            let activity = state.child_pid.and_then(session_activity);
            state.status = status_after_compact(activity);
        }
        _ => common::dispatch_default(state, msg),
    }
}

// =============================================================================
// Background-shell detection (session file)
// =============================================================================

/// `~/.claude/sessions/<pid>.json` carries Claude's own session status, keyed by
/// the agent's process id. It reads `"shell"` exactly when the turn has ended but
/// a `run_in_background` shell is still running — Claude maintains it, so there's
/// no edge-tracking and no staleness for us to manage.
pub fn session_file_path(agent_pid: u32) -> Option<PathBuf> {
    Some(sessions_dir()?.join(format!("{agent_pid}.json")))
}

/// What the Claude agent at `agent_pid` is doing, per its session file's `status`
/// field: `"busy"` → `Working`, `"shell"` → `BackgroundShell`, `"idle"` → `Idle`.
/// `None` if the file is missing/unreadable, its JSON doesn't parse, or its status
/// is one we don't recognize (caller leaves the current status untouched).
pub fn session_activity(agent_pid: u32) -> Option<AgentActivity> {
    let path = session_file_path(agent_pid)?;
    let content = std::fs::read_to_string(&path).ok()?;
    parse_session_activity(&content)
}

/// Resolve the status a `Stop` hook should settle to, given the agent's live
/// session-file activity (see the `HookEvent::Stop` arm). The model turn ended;
/// what happens next depends on whether Claude still reports work:
/// - `Active` + `Working` → **hold `Active`**: the turn ended only because a
///   background subagent is still running. The file stays frozen at `"busy"`
///   (no wake) so the row simply stays `Active`; when Claude finally goes idle
///   its idle-write wakes the launcher and the reconciliation settles us.
/// - any + `BackgroundShell` → `BackgroundActive`: a `run_in_background` shell
///   survives the turn (the review-pending refinement classifies it further).
/// - anything else — at rest (`Idle`), an unreadable/torn file (`None`), or a
///   non-`Active` starting point → `Idle`, exactly as before.
///
/// Only `Active`+`Working` holds; every other combination still settles to a
/// rest shape, so this can never strand a non-`Active` state or invent activity
/// from a missing file. Claude-only: Codex has no session file (its
/// `session_activity` is always `None`), so its `Stop` still settles to `Idle`.
fn status_after_stop(current: &SessionStatus, activity: Option<AgentActivity>) -> SessionStatus {
    match (current, activity) {
        (SessionStatus::Active, Some(AgentActivity::Working)) => SessionStatus::Active,
        (_, Some(AgentActivity::BackgroundShell)) => SessionStatus::BackgroundActive,
        _ => SessionStatus::Idle,
    }
}

/// Resolve the status a `PostCompact` hook should settle to (see its arm).
/// `Compacted` unless Claude reports a `run_in_background` shell still running,
/// in which case the row belongs to that shell — `BackgroundActive`, which the
/// launcher's classifier refines to `Server`/`Review` in the same wake.
///
/// Everything else keeps `Compacted`: a rest status the dashboard bells as
/// "compaction landed, look at me". In particular a mid-turn *auto*-compaction
/// reads `"busy"` (`Working`) and stays `Compacted`, which the turn's next hook
/// overwrites within milliseconds anyway — the pre-existing behaviour. An
/// unreadable/missing file (`None`) falls back the same way.
fn status_after_compact(activity: Option<AgentActivity>) -> SessionStatus {
    match activity {
        Some(AgentActivity::BackgroundShell) => SessionStatus::BackgroundActive,
        _ => SessionStatus::Compacted,
    }
}

/// Map the session file's `status` to an [`AgentActivity`]. `None` when the JSON
/// doesn't parse, carries no `status` (e.g. a torn read of a file Claude is
/// mid-rewrite on), or carries an *unknown* status. Mapping unknown/torn reads to
/// a definite state instead would let a stale/garbled read spuriously demote a
/// live `Active` turn to `Idle` (or `BackgroundActive` → `Idle` while the shell
/// is still running); `None` means "leave the status as-is".
fn parse_session_activity(json: &str) -> Option<AgentActivity> {
    #[derive(Deserialize)]
    struct Session {
        status: Option<String>,
    }
    let status = serde_json::from_str::<Session>(json).ok()?.status?;
    match status.as_str() {
        "busy" => Some(AgentActivity::Working),
        "shell" => Some(AgentActivity::BackgroundShell),
        "idle" => Some(AgentActivity::Idle),
        _ => None,
    }
}

/// The *user-set* display name from the session file (`~/.claude/sessions/<pid>.json`
/// `name`). Claude writes `name` in two cases, told apart by `nameSource`: its own
/// auto-*derived* `project-name-hash` slug early in the session (marked
/// `nameSource:"derived"`) and the user's `/rename` (which sets `name` and drops
/// `nameSource`). We surface **only the rename** — the auto slug is deliberately
/// dropped so the dashboard falls back to the folded first prompt (a far better
/// title than `captain-miao-da`), while an explicit `/rename` still wins. The
/// launcher folds this onto `LauncherState.name` (no transcript read), so it reaches
/// the dashboard's *remote* rows over the state file for free — the reason the name
/// is read here and not from the transcript's `custom-title` line (which the
/// dashboard could never see for a remote host). `None` if the file is
/// missing/unreadable, its JSON doesn't parse, the name is auto-derived, or `name`
/// is absent/blank.
pub fn read_session_name(agent_pid: u32) -> Option<String> {
    let path = session_file_path(agent_pid)?;
    let content = std::fs::read_to_string(&path).ok()?;
    parse_session_name(&content)
}

/// Extract the trimmed, non-empty user-set `name` from a session-file body. Split
/// from the IO so the parse is testable. Returns `None` on a parse failure, an
/// auto-derived name (`nameSource:"derived"`), or an absent/blank name — never an
/// empty string, so a torn read never stamps a blank title.
fn parse_session_name(json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Session {
        name: Option<String>,
        #[serde(rename = "nameSource")]
        name_source: Option<String>,
    }
    let session = serde_json::from_str::<Session>(json).ok()?;
    // Claude's own auto slug is marked `nameSource:"derived"`; a `/rename` drops the
    // field. Ignore the slug so the display falls through to the first prompt.
    if session.name_source.as_deref() == Some("derived") {
        return None;
    }
    let trimmed = session.name?.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Read new bytes from a Claude Code JSONL transcript starting at `offset`
/// and extract signals the launcher cares about. On read failure or a
/// shrunken/replaced file, returns offset=0 so the caller re-scans from the
/// start on the next tick.
pub fn scan_transcript_signals(path: &Path, offset: u64) -> TranscriptScan {
    let delta = crate::agent::read_transcript_delta(path, offset);
    let mut interrupted = false;
    let mut compact_aborted = false;
    for line in delta.text.lines() {
        if line.contains("[Request interrupted by user]") {
            interrupted = true;
        }
        // Claude writes `<local-command-stderr>...</local-command-stderr>`
        // when a slash-command errors. `/compact` failures (e.g. "Not enough
        // messages to compact") land here with no accompanying PostCompact
        // hook, so we surface them as a separate signal.
        if line.contains("<local-command-stderr>") && line.contains("compact") {
            compact_aborted = true;
        }
    }
    TranscriptScan {
        new_offset: delta.new_offset,
        interrupted,
        compact_aborted,
        // Claude opens every turn with a `UserPromptSubmit` hook, so there is
        // no hookless turn start to report and nothing that would make one.
        ..TranscriptScan::default()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// `CLAUDE_CONFIG_DIR` relocates the whole Claude tree we read — transcripts,
    /// the session manifests, and the resume list. An isolated instance (the
    /// screencast sandbox) depends on this: without it the agent writes to the
    /// relocated home while captain-miao keeps scanning `~/.claude`, and the
    /// user's real sessions leak into the resume picker.
    #[test]
    fn claude_home_prefers_configured_dir() {
        let home = Some(PathBuf::from("/home/miao"));

        // Set: wins outright, and is used verbatim (no `.claude` suffix — the
        // var names the config dir itself, exactly as Claude Code treats it).
        assert_eq!(
            resolve_claude_home(Some(PathBuf::from("/demo/claude-home")), home.clone()),
            Some(PathBuf::from("/demo/claude-home")),
        );

        // Unset and empty both fall back to `~/.claude`.
        assert_eq!(
            resolve_claude_home(None, home.clone()),
            Some(PathBuf::from("/home/miao/.claude")),
        );
        assert_eq!(
            resolve_claude_home(Some(PathBuf::new()), home),
            Some(PathBuf::from("/home/miao/.claude")),
        );

        // No home and no override: nothing to read.
        assert_eq!(resolve_claude_home(None, None), None);
    }

    /// A dashboard-spawned session must start as its own, not as a subagent of
    /// whatever launched the terminal — and must keep the user's settings while
    /// doing it. Asserted on the `Command` rather than through
    /// `build_launch_command`, which needs `claude` on `PATH`.
    #[test]
    fn a_spawned_session_sheds_the_launching_sessions_env() {
        let mut cmd = Command::new("/bin/sh");
        clear_parent_session_env(&mut cmd);
        let cleared: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();

        // The marker that turns transcript saving off, and with it the fold this
        // backend reads for status, title and context tokens.
        assert!(
            cleared.iter().any(|k| k == "CLAUDE_CODE_CHILD_SESSION"),
            "cleared: {cleared:?}"
        );
        assert_eq!(cleared.len(), PARENT_SESSION_ENV.len(), "{cleared:?}");

        // Settings are the user's, not one session's run — clearing
        // `CLAUDE_CONFIG_DIR` in particular would point the agent at a different
        // tree than the one `resolve_claude_home` above reads.
        for keep in ["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_ENTRYPOINT"] {
            assert!(
                !cleared.iter().any(|k| k == keep),
                "{keep} is the user's, not the launching session's"
            );
        }
    }

    fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "captain-miao-test-{}-{}.jsonl",
            std::process::id(),
            name,
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Synthesize a `{"type":"user","isCompactSummary":true,...}` transcript
    /// line carrying `summary` as its message content.
    fn compact_line(summary: &str) -> String {
        format!(r#"{{"type":"user","isCompactSummary":true,"message":{{"content":"{summary}"}}}}"#)
    }

    #[test]
    fn context_tokens_uses_last_assistant_usage() {
        let body = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":50}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":200,"cache_read_input_tokens":300,"cache_creation_input_tokens":50}}}"#,
            "\n",
        );
        let path = write_tmp("ctx_basic", body);
        assert_eq!(read_transcript_stats(&path).context_tokens, Some(550));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn context_tokens_skips_api_error_assistants() {
        let body = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1234}}}"#,
            "\n",
            r#"{"type":"assistant","isApiErrorMessage":true,"message":{"usage":{"input_tokens":0}}}"#,
            "\n",
        );
        let path = write_tmp("ctx_apierr", body);
        assert_eq!(read_transcript_stats(&path).context_tokens, Some(1234));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn context_tokens_uses_prior_post_compact_baseline() {
        let summary1 = compact_line(&"s".repeat(4000));
        let summary2 = compact_line(&"s".repeat(4000));
        let body = format!(
            "{}\n{summary1}\n{}\n{}\n{summary2}\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":6,"cache_creation_input_tokens":29481}}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":6,"cache_creation_input_tokens":29715,"cache_read_input_tokens":18187}}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"cache_creation_input_tokens":213,"cache_read_input_tokens":279769}}}"#,
        );
        let path = write_tmp("ctx_compact_baseline", &body);
        assert_eq!(read_transcript_stats(&path).context_tokens, Some(47908));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn context_tokens_first_compact_falls_back_to_first_total_plus_summary() {
        let summary = compact_line(&"x".repeat(4000));
        let body = format!(
            "{}\n{summary}\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":6,"cache_creation_input_tokens":29481}}}"#,
        );
        let path = write_tmp("ctx_compact_first", &body);
        assert_eq!(read_transcript_stats(&path).context_tokens, Some(30487));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn context_tokens_skips_zero_usage_assistant() {
        let body = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1234,"cache_read_input_tokens":42000}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
            "\n",
        );
        let path = write_tmp("ctx_zero_usage", body);
        assert_eq!(read_transcript_stats(&path).context_tokens, Some(43234));
        let _ = std::fs::remove_file(path);
    }

    // Two assistant turns: the first carries the model, the second only usage
    // (so the model must persist across folds). Last total = 550.
    const LINE_A: &str = r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":50},"model":"claude-opus-4-8"}}"#;
    const LINE_B: &str = r#"{"type":"assistant","message":{"usage":{"input_tokens":200,"cache_read_input_tokens":300,"cache_creation_input_tokens":50}}}"#;

    #[test]
    fn incremental_parse_matches_full_parse_across_appends() {
        let path = write_tmp("incr_match", "");
        // First append: only line A is present.
        std::fs::write(&path, format!("{LINE_A}\n")).unwrap();
        let s1 = read_transcript_stats_incremental(&path, None);
        assert_eq!(s1.context_tokens, Some(150));
        assert_eq!(s1.model.as_deref(), Some("claude-opus-4-8"));

        // Second append: line B lands. Folding only the delta must reach the
        // same result as a full parse — and carry A's model forward even though
        // B has none.
        std::fs::write(&path, format!("{LINE_A}\n{LINE_B}\n")).unwrap();
        let s2 = read_transcript_stats_incremental(&path, Some(&s1));
        let full = read_transcript_stats(&path);
        assert_eq!(s2.context_tokens, full.context_tokens);
        assert_eq!(s2.model, full.model);
        assert_eq!(s2.context_tokens, Some(550));
        assert_eq!(s2.model.as_deref(), Some("claude-opus-4-8"));
        // The cursor consumed the whole (newline-terminated) file.
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(s2.cursor.unwrap().offset, len);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_parse_holds_back_partial_trailing_line() {
        let path = write_tmp("incr_partial", "");
        // Line A is complete; line B is only half-written (no trailing newline).
        let (b_head, b_tail) = LINE_B.split_at(20);
        std::fs::write(&path, format!("{LINE_A}\n{b_head}")).unwrap();
        let s1 = read_transcript_stats_incremental(&path, None);
        // Only A folded; the partial B is held, not parsed as garbage.
        assert_eq!(s1.context_tokens, Some(150));
        // Offset stopped right after A's newline, before the partial line.
        assert_eq!(s1.clone().cursor.unwrap().offset, (LINE_A.len() + 1) as u64);

        // B is completed. The held bytes are re-read and folded exactly once.
        std::fs::write(&path, format!("{LINE_A}\n{b_head}{b_tail}\n")).unwrap();
        let s2 = read_transcript_stats_incremental(&path, Some(&s1));
        assert_eq!(s2.context_tokens, Some(550));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_parse_reparses_when_file_shrinks() {
        let path = write_tmp("incr_truncate", "");
        std::fs::write(&path, format!("{LINE_A}\n{LINE_B}\n")).unwrap();
        let s1 = read_transcript_stats_incremental(&path, None);
        assert_eq!(s1.context_tokens, Some(550));

        // File replaced by a shorter one: the stale offset is past EOF, so the
        // accumulators must be discarded and the new content parsed from zero.
        let line_c = r#"{"type":"assistant","message":{"usage":{"input_tokens":999}}}"#;
        std::fs::write(&path, format!("{line_c}\n")).unwrap();
        let s2 = read_transcript_stats_incremental(&path, Some(&s1));
        assert_eq!(s2.context_tokens, Some(999));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_fold_captures_first_prompt_first_wins() {
        // The `/rename` title is no longer folded from the transcript — it's read
        // from the session file (see `parse_session_name`) — so this pins only the
        // first-prompt fold: first-wins, and stable across a later user line.
        let user = r#"{"type":"user","message":{"content":"first question"}}"#;
        let path = write_tmp("incr_first_prompt", "");

        // First append: the user prompt lands (first-wins).
        std::fs::write(&path, format!("{user}\n{LINE_A}\n")).unwrap();
        let s1 = read_transcript_stats_incremental(&path, None);
        assert_eq!(s1.first_prompt.as_deref(), Some("first question"));

        // Second append: a later user line must NOT replace the first-wins prompt,
        // and folding only the delta must match a full parse.
        let later_user = r#"{"type":"user","message":{"content":"second question"}}"#;
        std::fs::write(&path, format!("{user}\n{LINE_A}\n{later_user}\n")).unwrap();
        let s2 = read_transcript_stats_incremental(&path, Some(&s1));
        let full = read_transcript_stats(&path);
        assert_eq!(s2.first_prompt.as_deref(), Some("first question"));
        assert_eq!(s2.first_prompt, full.first_prompt);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parse_session_name_takes_rename_ignores_derived_and_rejects_blank() {
        // Auto-derived slug (Claude's own, marked `nameSource:"derived"`) is
        // ignored, so the dashboard falls back to the first prompt.
        let derived = r#"{"pid":1,"sessionId":"abc","name":"captain-miao-6e","nameSource":"derived","status":"busy"}"#;
        assert_eq!(parse_session_name(derived), None);
        // User `/rename` — sets `name`, drops `nameSource`. Taken, trimmed.
        let renamed =
            r#"{"pid":1,"sessionId":"abc","name":"  session-name read  ","status":"idle"}"#;
        assert_eq!(
            parse_session_name(renamed).as_deref(),
            Some("session-name read")
        );
        // A non-"derived" nameSource is still a real name (defensive — surface it).
        let other_source = r#"{"name":"kept","nameSource":"user"}"#;
        assert_eq!(parse_session_name(other_source).as_deref(), Some("kept"));
        // Absent / blank / unparseable → None (never a blank title).
        assert_eq!(parse_session_name(r#"{"pid":1,"status":"idle"}"#), None);
        assert_eq!(parse_session_name(r#"{"name":"   "}"#), None);
        assert_eq!(parse_session_name(r#"{"name":null}"#), None);
        assert_eq!(parse_session_name("not json"), None);
    }

    #[test]
    fn strip_ansi_removes_csi_osc_and_control_chars() {
        let raw = "\x1b[31m+added\x1b[0m\x1b]0;title\x07 line\x07";
        assert_eq!(strip_ansi_escapes(raw), "+added line");
    }

    #[test]
    fn strip_ansi_keeps_tabs_and_newlines() {
        let raw = "a\tb\nc\x1b[1md\x1b[0m";
        assert_eq!(strip_ansi_escapes(raw), "a\tb\ncd");
    }

    #[test]
    fn extract_user_prompt_strips_ansi() {
        let content = serde_json::json!("\x1b[32mhello\x1b[0m \x1b[1mworld\x1b[0m");
        assert_eq!(
            extract_user_prompt(&content),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extract_user_prompt_collapses_multiline() {
        let content = serde_json::json!("first line\n\n  second line  ");
        assert_eq!(
            extract_user_prompt(&content),
            Some("first line second line".to_string())
        );
    }

    #[test]
    fn extract_user_prompt_skips_local_command_output() {
        // Slash commands (/model, /clear, …) echo their result back as a user
        // entry wrapped in <local-command-stdout>/<local-command-stderr>. These
        // are system-control messages, not prompts, so they must not become the
        // auto title. The stdout carries ANSI too — skip before it's cleaned.
        for content in [
            serde_json::json!(
                "<local-command-stdout>Set model to \u{1b}[1mFable 5\u{1b}[22m and saved as your default</local-command-stdout>"
            ),
            serde_json::json!("<local-command-stderr>some error</local-command-stderr>"),
            serde_json::json!("<local-command-caveat>resumed</local-command-caveat>"),
        ] {
            assert_eq!(extract_user_prompt(&content), None);
        }
    }

    #[test]
    fn extract_user_prompt_skips_slash_command_invocation() {
        // The invocation itself is <command-name>/model</command-name> with
        // sibling <command-message>/<command-args> parts.
        let content = serde_json::json!(
            "<command-message>model</command-message><command-name>/model</command-name><command-args>fable</command-args>"
        );
        assert_eq!(extract_user_prompt(&content), None);
    }

    #[test]
    fn scan_signals_flags_compact_stderr() {
        let body = concat!(
            r#"{"type":"system","subtype":"local_command","content":"<local-command-stderr>Error: Not enough messages to compact.</local-command-stderr>"}"#,
            "\n",
        );
        let path = write_tmp("scan_compact_aborted", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(scan.compact_aborted);
        assert!(!scan.interrupted);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scan_signals_ignores_unrelated_stderr() {
        let body = "{\"type\":\"system\",\"content\":\"<local-command-stderr>some other error</local-command-stderr>\"}\n";
        let path = write_tmp("scan_unrelated_stderr", body);
        let scan = scan_transcript_signals(&path, 0);
        assert!(!scan.compact_aborted);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_model_takes_last_real_assistant_model() {
        // Model can change mid-session via /model; take the latest. The trailing
        // <synthetic> entry (interrupt/error placeholder) must be skipped.
        let body = concat!(
            r#"{"type":"assistant","message":{"model":"claude-opus-4-7","usage":{"input_tokens":1}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"switch"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8[1m]","usage":{"input_tokens":1}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"<synthetic>"}}"#,
            "\n",
        );
        let path = write_tmp("read_model", body);
        assert_eq!(
            read_transcript_stats(&path).model,
            Some("claude-opus-4-8[1m]".to_string())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_model_none_without_assistant_turn() {
        let body = r#"{"type":"user","message":{"content":"hi"}}"#;
        let path = write_tmp("read_model_none", body);
        assert_eq!(read_transcript_stats(&path).model, None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn context_tokens_uses_post_compact_assistant_when_present() {
        let summary = compact_line(&"x".repeat(4000));
        let body = format!(
            "{}\n{summary}\n{}\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":190000}}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":3500,"cache_read_input_tokens":2000}}}"#,
        );
        let path = write_tmp("ctx_compact_then_asst", &body);
        assert_eq!(read_transcript_stats(&path).context_tokens, Some(5500));
        let _ = std::fs::remove_file(path);
    }

    fn active_state() -> LauncherState {
        LauncherState::for_test(crate::agent::AgentControl::Claude, SessionStatus::Active)
    }

    fn permission_request(tool: &str) -> HookMessage {
        HookMessage {
            event: HookEvent::PermissionRequest,
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
    fn permission_request_for_ask_user_question_is_decision() {
        // AskUserQuestion is a question/answer selection, not a tool-permission
        // gate, so it surfaces as "Decision" rather than "Approval".
        assert_eq!(
            dispatched(permission_request("AskUserQuestion")),
            SessionStatus::WaitingForDecision,
        );
    }

    #[test]
    fn permission_request_for_other_tools_is_approval() {
        assert_eq!(
            dispatched(permission_request("Bash")),
            SessionStatus::WaitingForApproval,
        );
    }

    #[test]
    fn session_status_maps_to_agent_activity() {
        // Real session-file shapes from Claude Code 2.1.159: "busy" mid-turn,
        // "shell" when idle-with-background-shell, "idle" at rest.
        let shell = r#"{"pid":25243,"sessionId":"abc","status":"shell","updatedAt":1}"#;
        let idle = r#"{"pid":3945,"sessionId":"def","status":"idle","updatedAt":1}"#;
        let busy = r#"{"pid":1,"sessionId":"x","status":"busy","updatedAt":1}"#;
        assert_eq!(
            parse_session_activity(shell),
            Some(AgentActivity::BackgroundShell)
        );
        assert_eq!(parse_session_activity(idle), Some(AgentActivity::Idle));
        assert_eq!(parse_session_activity(busy), Some(AgentActivity::Working));
        // A torn read of a mid-rewrite file (missing field / malformed JSON) and
        // any unrecognized status must read as "unknown" (None), so a stale/garbled
        // read can't demote a live Active turn to Idle (or BackgroundActive while
        // the shell is still running).
        assert_eq!(parse_session_activity(r#"{"pid":1}"#), None);
        assert_eq!(parse_session_activity("not json"), None);
        assert_eq!(parse_session_activity(r#"{"status":"compacting"}"#), None);
    }

    #[test]
    fn stop_holds_active_while_a_background_subagent_still_works() {
        use AgentActivity as A;
        use SessionStatus as S;
        // The turn ended but the session file still reports work: a background
        // subagent is running, so hold Active rather than flash the row to rest.
        assert_eq!(status_after_stop(&S::Active, Some(A::Working)), S::Active);
        // A genuinely finished turn (file idle) or an unreadable/torn file
        // (None) settles to Idle — the pre-subagent behaviour and safe default.
        assert_eq!(status_after_stop(&S::Active, Some(A::Idle)), S::Idle);
        assert_eq!(status_after_stop(&S::Active, None), S::Idle);
        // A surviving run_in_background shell surfaces as BackgroundActive (the
        // review-pending refinement then classifies it), regardless of the
        // starting status.
        assert_eq!(
            status_after_stop(&S::Active, Some(A::BackgroundShell)),
            S::BackgroundActive
        );
        assert_eq!(
            status_after_stop(&S::Idle, Some(A::BackgroundShell)),
            S::BackgroundActive
        );
    }

    #[test]
    fn stop_only_holds_active_never_a_non_active_state() {
        use AgentActivity as A;
        use SessionStatus as S;
        // Only Active+Working holds; a "busy" read from any other starting state
        // still settles to Idle, so Stop can never strand e.g. a stale Compacted.
        for st in [S::Idle, S::Compacted, S::WaitingForApproval, S::Compacting] {
            assert_eq!(status_after_stop(&st, Some(A::Working)), S::Idle);
        }
    }

    /// A `/compact` fires no `Stop`, so `PostCompact` is the turn end for a
    /// session at rest — and a `run_in_background` shell that outlived the
    /// compaction owns the row. Without this, compacting a session that is
    /// watching an r3 review reads `Compacted` until the watch ends, instead of
    /// going back to `Review`.
    #[test]
    fn post_compact_yields_to_a_surviving_background_shell() {
        use AgentActivity as A;
        use SessionStatus as S;
        assert_eq!(
            status_after_compact(Some(A::BackgroundShell)),
            S::BackgroundActive
        );
        // Everything else keeps the compaction signal: at rest, mid-turn (an
        // auto-compaction, whose next hook overwrites this anyway), and an
        // unreadable/torn file.
        assert_eq!(status_after_compact(Some(A::Idle)), S::Compacted);
        assert_eq!(status_after_compact(Some(A::Working)), S::Compacted);
        assert_eq!(status_after_compact(None), S::Compacted);
    }

    // ---- fixture vocabulary -------------------------------------------------
    //
    // The same handful of shapes recur across the background-shell tests: a
    // compiled `r3`, a run-from-source `r3`, and a review-watch behind a `nix
    // develop … bash -c '…'` runner. Each test used to spell them its own way,
    // which is how two of them drifted into two different transcriptions of one
    // command.
    //
    // Naming them keeps every case below about **shape** — which is all the
    // classifiers key on — rather than about paths, which they ignore entirely.
    // The home directory here is a placeholder for exactly that reason: no
    // assertion in this module depends on it, and a fixture carrying a real one
    // would be a real one published for no test value.

    /// Compiled binary, reached through `$PATH`.
    const R3_WATCH: &str = "r3 watch review_2b40f7 --session docs";
    /// Compiled binary, reached by absolute path.
    const R3_WATCH_ABS: &str = "/home/riteye/.local/bin/r3 watch review_abc";
    /// Run-from-source by full path — the form r3's own guide documents.
    const R3_WATCH_SRC: &str =
        "bun /home/riteye/projects/r3/cli/index.ts watch review_abc --agent-id x";
    /// Run-from-source from inside the checkout: relative script, so the `r3`
    /// evidence is the `cd` on the line before.
    const R3_WATCH_SRC_REL: &str =
        "cd /home/riteye/projects/r3\nbun cli/index.ts watch review_2b40f7";

    /// A review-watch behind a runner, from the report that produced
    /// `normalize_bg_command`'s shell-word parse. The inner `'` is the whole
    /// point: a bash single-quoted string can't contain one, so the Bash tool
    /// embeds this as a *concatenation* of quoted segments
    /// (`…bash -c '"'"'…'"'"'`) in which the first quote closes only the first
    /// segment. Slicing at that quote truncated the key to the wrapper prefix,
    /// which both hid the `r3 watch` from the classifier and collapsed every
    /// `<runner> --command bash -c '…'` onto one learning key.
    const QUOTED_RUNNER_WATCH: &str = r#"nix develop /home/riteye/projects/app --command bash -c '/home/riteye/.local/bin/r3 watch review_742a2e64ef72 --session "claude:app-prd"'"#;

    /// Both sides of `is_r3_watch_command`'s boundary in one table.
    ///
    /// They were two tests twenty lines apart, which is the wrong shape for a
    /// recognizer: the interesting cases are the *adjacent* ones — `r3 watch`
    /// against `cargo watch`, an `index.ts watch` with an r3 checkout on the
    /// line against one without — and you could not see them together. Each row
    /// carries why it lands where it does, so a failure names the rule it broke.
    #[test]
    fn is_r3_watch_command_draws_the_documented_boundary() {
        let cases: &[(bool, &str, &str)] = &[
            (true, R3_WATCH, "compiled binary on PATH"),
            (true, R3_WATCH_ABS, "compiled binary by absolute path"),
            (true, R3_WATCH_SRC, "run-from-source, full path"),
            (
                true,
                R3_WATCH_SRC_REL,
                "run-from-source, cd + relative script",
            ),
            (
                true,
                "r3 watch somethingelse",
                "the entrypoint token is enough; a review_ id is not required",
            ),
            (false, "cargo watch -x run", "a watcher, but not r3's"),
            (
                false,
                "npm run watch",
                "ditto — the seed heuristic's territory",
            ),
            (false, "watch -n1 ls", "the unrelated watch(1) utility"),
            (
                false,
                "tail -f log && r3 list",
                "r3 present, but not the watch subcommand",
            ),
            (
                false,
                "bun cli/index.ts watch foo",
                "an index.ts watch with no r3 evidence on the line stays ambiguous",
            ),
        ];
        for &(expect, cmd, why) in cases {
            assert_eq!(
                is_r3_watch_command(cmd),
                expect,
                "expected {expect} ({why}) for: {cmd}"
            );
        }
    }

    /// A real background-shell wrapper command line as `ps` reports it (captured
    /// live from Claude Code 2.1.220), embedding `cmd` the way the Bash tool
    /// does: single-quoted, with any `'` the command itself contains escaped as
    /// `'"'"'`. Pins the [`SHELL_SNAPSHOT_MARKER`] filter, that the `unalias`
    /// preamble's own quotes don't confuse the `eval '` search, and that both
    /// classifiers still match through the wrapper.
    fn wrapper(cmd: &str) -> String {
        let quoted = cmd.replace('\'', r#"'"'"'"#);
        format!(
            "/nix/store/gik3-bash-5.3p9/bin/bash -c source /home/riteye/.claude/shell-snapshots/snapshot-bash-1782967947153-jmpq9h.sh 2>/dev/null || true && shopt -u extglob 2>/dev/null || true && {{ \\builtin unalias -- 'unsetenv'; \\builtin unset -f -- 'unsetenv'; }} >/dev/null 2>&1 || true && eval '{quoted}' < /dev/null && pwd -P >| /tmp/claude-41bf-cwd"
        )
    }

    /// The seed kinds of the classified background shells, for terse assertions.
    fn kinds(cmds: &[String]) -> Vec<BgSeedKind> {
        classify_bg_shells(cmds)
            .into_iter()
            .map(|s| s.kind)
            .collect()
    }

    /// [`wrapper`] is load-bearing for every test below it, and nothing checked
    /// it. Its doc-comment claims three properties; if it drifted out of any of
    /// them — a renamed snapshot marker, a lost `eval '`, a mis-typed escape —
    /// the tests using it would quietly stop exercising the wrapper path and
    /// keep passing, because a line the filter *rejects* simply classifies as
    /// no shell at all. So pin the helper itself, once, here.
    #[test]
    fn wrapper_builds_a_line_the_bash_tool_filter_recognizes() {
        let line = wrapper("npm run dev");
        assert!(
            line.contains(SHELL_SNAPSHOT_MARKER),
            "no snapshot marker, so bg_shells would filter this line out: {line}"
        );
        // The `unalias` preamble contains quotes of its own; the eval body has
        // to survive them.
        assert_eq!(normalize_bg_command(&line), "npm run dev");

        // A command with its own `'` must come out as the multi-segment form,
        // not a single quoted string — otherwise QUOTED_RUNNER_WATCH below
        // silently degrades into the easy case it exists to rule out.
        let quoted = wrapper(QUOTED_RUNNER_WATCH);
        assert!(
            quoted.contains(r#"'"'"'"#),
            "wrapper lost the quote concatenation: {quoted}"
        );
        assert_eq!(normalize_bg_command(&quoted), QUOTED_RUNNER_WATCH);
    }

    /// Classification keys on the command's **form**, never on where the binary
    /// lives — the property that lets every path in this module be a
    /// placeholder. One table over the forms that reach us, replacing three
    /// near-identical single-form tests.
    #[test]
    fn classify_bg_shells_keys_on_command_form_not_on_paths() {
        let cases: &[(&str, BgSeedKind, &str)] = &[
            (R3_WATCH, BgSeedKind::ReviewWatch, "compiled binary"),
            (R3_WATCH_SRC, BgSeedKind::ReviewWatch, "run-from-source"),
            (
                QUOTED_RUNNER_WATCH,
                BgSeedKind::ReviewWatch,
                "review-watch behind a quoted runner",
            ),
            ("npm run dev", BgSeedKind::LongRunning, "seed heuristic"),
            (
                "cargo build 2>&1 | tail -40",
                BgSeedKind::Other,
                "transient, so the row stays busy",
            ),
        ];
        for &(cmd, kind, why) in cases {
            let line = wrapper(cmd);
            let seeds = classify_bg_shells(&[line]);
            assert_eq!(seeds.len(), 1, "{why}");
            // Assert the *pair*: the kind is only meaningful next to the key it
            // was derived from, and the key is what the learning store records.
            assert_eq!(seeds[0].kind, kind, "wrong kind for {why}: {cmd}");
            assert_eq!(seeds[0].key, cmd, "key not recovered for {why}");
        }
    }

    #[test]
    fn classify_bg_shells_mixed() {
        // A review-watch, a long-running dev server, and a plain build → each
        // gets its own kind (the launcher aggregates them).
        let cmds = vec![
            wrapper("r3 watch review_2b40f7"),
            wrapper("npm run dev"),
            wrapper("cargo build 2>&1 | tail -40"),
        ];
        assert_eq!(
            kinds(&cmds),
            vec![
                BgSeedKind::ReviewWatch,
                BgSeedKind::LongRunning,
                BgSeedKind::Other,
            ]
        );
    }

    /// A command that `exec`s leaves no wrapper behind, so the snapshot marker
    /// — the usual evidence that a child is a background task — is gone from
    /// the process image. From a live session whose row sat on
    /// `BackgroundActive` while its review really was waiting: the agent ran
    /// `export PATH=…; exec <dir>/r3 watch review_…`, and that launcher script's
    /// own `exec bun "$cli" "$@"` left the `bun` line below as the agent's
    /// direct child. The recognizer would have matched it all along; the filter
    /// in front of it never let it through.
    #[test]
    fn classify_bg_shells_admits_an_execd_review_watch_without_the_wrapper() {
        let execd = format!("{R3_WATCH_SRC} --auto-fetch-timeout 900");
        let seeds = classify_bg_shells(std::slice::from_ref(&execd));
        assert_eq!(seeds.len(), 1, "exec'd review-watch dropped: {execd}");
        assert_eq!(seeds[0].kind, BgSeedKind::ReviewWatch);
        assert_eq!(seeds[0].key, execd, "an unwrapped line is its own key");

        // The admission stays narrow. Past the review-watch form an unwrapped
        // child is still no shell at all: an exec'd dev server leaves the row
        // busy rather than parking it, which is the safe direction, and the
        // non-task children keep leaving the empty set that
        // `promote_stale_background` recovers on.
        assert!(kinds(&["npm run dev".to_string()]).is_empty());
    }

    #[test]
    fn classify_bg_shells_ignores_non_shell_children() {
        // MCP stdio servers / helpers never source a shell snapshot: they're not
        // background tasks and must not block the refinement…
        let mcp = "node /home/riteye/.local/lib/some-mcp-server/index.js --stdio".to_string();
        let cmds = vec![mcp.clone(), wrapper("r3 watch review_2b40f7")];
        assert_eq!(kinds(&cmds), vec![BgSeedKind::ReviewWatch]);
        // …and alone they leave an *empty* set, which is a different fact from
        // "the tree couldn't be read" (`bg_shells`' `None`). The launcher
        // promotes a stale background row on the strength of this emptiness, so
        // collapsing it back into `None` would silently disable that recovery.
        assert!(kinds(&[mcp]).is_empty());
        assert!(kinds(&[]).is_empty());
    }

    #[test]
    fn normalize_bg_command_extracts_eval_body_through_wrapper() {
        // The volatile snapshot path + cwd temp are stripped, leaving a stable
        // key that matches across sessions.
        assert_eq!(normalize_bg_command(&wrapper("npm run dev")), "npm run dev");
        assert_eq!(
            normalize_bg_command(&wrapper("cargo build 2>&1 | tail -40")),
            "cargo build 2>&1 | tail -40"
        );
        // No wrapper: the whole (trimmed) command is the key.
        assert_eq!(normalize_bg_command("  vite  "), "vite");
    }

    /// Quoting forms the eval body has to survive, beyond the round trip
    /// [`wrapper_builds_a_line_the_bash_tool_filter_recognizes`] already pins.
    #[test]
    fn normalize_bg_command_survives_quotes_inside_the_command() {
        // Double quotes inside the single-quoted argument stay literal, so a key
        // that already worked keeps exactly the same spelling.
        let quoted = r#"export PATH="/nix/store/bun/bin:$PATH" /home/riteye/.local/bin/r3 watch review_389337185556 --session "claude: readme review""#;
        assert_eq!(normalize_bg_command(&wrapper(quoted)), quoted);
    }

    #[test]
    fn long_running_command_recognizes_common_dev_servers() {
        for cmd in [
            "npm run dev",
            "yarn dev",
            "pnpm run start",
            "vite",
            "next dev",
            "nuxt dev",
            "cargo watch -x run",
            "cargo watch -x build", // still long: the watch loop, not the build
            "nodemon server.js",
            "jest --watch",
            "tsc --watch",
            "uvicorn app:main --reload",
            "python manage.py runserver",
            "rails server",
            "flask run",
            "docker compose up",
            "docker-compose up -d web",
            "php artisan serve",
            "python -m http.server 8000",
        ] {
            assert!(
                is_long_running_command(cmd),
                "should be long-running: {cmd}"
            );
        }
    }

    #[test]
    fn long_running_command_rejects_finite_steps() {
        for cmd in [
            "npm run build",
            "npm test",
            "yarn build",
            "cargo build",
            "cargo test",
            "cargo clippy",
            "pytest -q",
            "go build ./...",
            "make",
            "tsc",
            "vite build",
            "next build",
            "eslint .",
            "r3 watch review_2b40f7", // a watch, but classified separately
        ] {
            assert!(
                !is_long_running_command(cmd),
                "should NOT be long-running: {cmd}"
            );
        }
    }

    #[test]
    fn parse_stat_ppid_survives_hostile_comm() {
        // comm can contain spaces and parens; ppid is 2 fields past the LAST `)`.
        assert_eq!(parse_stat_ppid("123 (a b) c) R 4567 123 123 0"), Some(4567));
        assert_eq!(parse_stat_ppid("1 (init) S 0 1 1 0"), Some(0));
        assert_eq!(parse_stat_ppid("garbage"), None);
    }

    #[test]
    fn parse_ps_children_filters_by_ppid() {
        // Both shapes a background shell reaches us in, side by side: a parked
        // dev server still wearing its wrapper, and a review-watch that `exec`'d
        // out of one. The `999` line is the point of the test — a cmdline that
        // merely mentions the pid must not be mistaken for a child of it.
        let ps = "\
    1 /sbin/launchd
 4321 /bin/bash -c source /Users/riteye/.claude/shell-snapshots/snapshot-zsh-17.sh && eval 'npm run dev'
 4321 bun /Users/riteye/projects/r3/cli/index.ts watch review_1
  999 unrelated --ppid 4321
";
        let children = parse_ps_child_cmdlines(ps, 4321);
        assert_eq!(children.len(), 2);
        assert!(children[0].starts_with("/bin/bash -c source"));
        assert_eq!(
            kinds(&children),
            vec![BgSeedKind::LongRunning, BgSeedKind::ReviewWatch]
        );
    }

    /// End-to-end on the real `/proc`: a child we spawn ourselves shows up in
    /// [`child_cmdlines`] of our own pid (children are recorded per-process, so
    /// the test-runner thread doesn't matter).
    #[cfg(target_os = "linux")]
    #[test]
    fn child_cmdlines_sees_own_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let cmds = child_cmdlines(std::process::id()).expect("read /proc");
        assert!(
            cmds.iter().any(|c| c.contains("sleep 30")),
            "children: {cmds:?}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
