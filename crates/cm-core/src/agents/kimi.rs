//! Kimi Code CLI backend. Owns every Kimi-specific path, env var and hook
//! payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Kimi`'s match arms.
//!
//! **Source-read, never run.** First written from MoonshotAI/kimi-code's
//! published documentation alone (design note §7); since re-read against the
//! source itself — `apps/kimi-code` for the CLI surface and
//! `packages/agent-core-v2` for the engine (bootstrap, config, hooks runner,
//! session store). The read verified nearly every assumption the first cut
//! marked — payload field spellings, env passthrough into hooks, the
//! `session_index.jsonl` and `wire.jsonl` layouts, `TurnStarted`'s per-turn
//! granularity, the absence of a fork flag — and found one it had wrong, the
//! `matcher` grammar (see [`build_hooks_settings`]). Beware the sibling repo:
//! `MoonshotAI/kimi-cli` is a wound-down *predecessor* whose facts (data dir,
//! env vars, hook events, flags) do not transfer. Still not run against a
//! binary; what remains for a probe is listed at the bottom of this doc.
//!
//! **`Interrupt` is a first-class hook, and that is the interesting fact about
//! this backend.** Codex has to scan its rollout for `turn_aborted` and Claude
//! has to defer `Stop` to its session file, both because pressing Esc fires no
//! hook. Kimi reports the interrupt directly, so it is registered like any other
//! event and normalized to [`HookEvent::Stop`] *in the config* — which is why
//! [`crate::agent::AgentControl::scan_transcript_signals`] is the empty default
//! and `agent_activity` is `None` with no loss at all, rather than as an
//! unimplemented gap. It is the single biggest simplification available to any
//! backend here.
//!
//! **`session_title` rides every payload**, so [`parse_hook_payload`] fills
//! [`HookMessage::session_title`] and [`common::adopt_session_facts`] stamps
//! it onto `LauncherState.name`. No title store, no sqlite reader, no per-host
//! overlay, no `out_of_band_watch_paths` entry — the two mechanisms Claude and
//! Codex each had to grow are simply absent. A rename is just a later payload
//! carrying a different title, and an empty one is already dropped by the shared
//! adopter (it means "not titled yet", not "renamed to nothing").
//!
//! **Isolation is the Codex pattern, hardened.** `KIMI_CODE_HOME` relocates the
//! entire data dir — config, `sessions/`, **credentials** — and kimi ships no
//! narrower knob (no config flag, no config env var; the `configPath` seam in
//! its bootstrap is SDK-only). So `<state>/kimi-home/` is a symlink farm over
//! the real `~/.kimi-code/` with `config.toml` a writable **copy** carrying our
//! `[[hooks]]` entries — and because the farm relocates *state*, the state
//! entries must exist to be linked before the agent can mint them as shadows:
//! [`ensure_synth_home`] seeds the real home's state dirs and dangling-links
//! `session_index.jsonl` first, so a first-ever session's transcripts, index
//! line and login all land in the real home rather than in a shadow the mirror
//! later quarantines.
//!
//! The one divergence the copy cannot avoid: **kimi writes back to
//! `config.toml`** (`/model`, OAuth token refresh, `/update-config`), and those
//! writes land in the copy. They survive across launches (the reseed is keyed
//! on the *real* file changing) but are clobbered the next time the user edits
//! their real config, and never propagate back. The fix worth wanting is
//! upstream — a `KIMI_CODE_CONFIG` wired to the `configPath` its bootstrap
//! already takes would let the hooks travel without the copy at all.
//!
//! Copy rather than symlink is the lesson Codex already
//! paid for (AGENTS.md; a symlinked config gave a split-brain database), and it
//! buys a second thing here: the config we edit is *ours*, so a `[[hooks]]`
//! block Kimi rejects can only break a captain-miao session's config, never the
//! user's real one. That matters more than usual, because Kimi validates the
//! hook table as **one array under a strict schema**: a single entry with an
//! unknown key drops the whole `hooks` section — every hook in the file — with
//! only a warning diagnostic. See [`build_hooks_settings`], which emits exactly
//! `event`, `command`, `timeout` for that reason (and no `matcher`, for a
//! sharper one documented there).
//!
//! Two risks carried over from Codex:
//!
//! - **`sessions/` is reached through a symlink**, and on macOS the notify event
//!   reports the resolved path while Linux echoes the registered one. Kimi's tree
//!   has the same shape, so it inherits the same handling — live, since the
//!   launcher watches the wire log named on every payload (below).
//! - **`sessions/<workDirKey>/<sessionId>/agents/*/wire.jsonl` is almost
//!   certainly appended through a long-held fd**, the condition that defeats
//!   macOS FSEvents entirely, so [`crate::agent::AgentControl::transcript_poll_interval`]
//!   answers `Some(2s)` there, matching Codex. **This machine is Linux and cannot
//!   test it** — that is unverified, and no macOS support is claimed. And it is
//!   live, not precautionary: [`parse_hook_payload`] resolves the wire log from
//!   the session id and names it on every payload, so the launcher does start
//!   this watch.
//!
//! **The token and model columns come off `wire.jsonl`, and the schema is not
//! guessed.** `agent-core-v2` ships a *generated* manifest of its whole wire
//! protocol (`docs/wire-manifest.d.ts`, `protocol_version 1.5`) in which
//! `usage.record` is flagged `persisted` and carries `model` plus
//! `usage: {inputOther, output, inputCacheRead, inputCacheCreation}`. The one
//! thing that manifest does not tell you is the part worth getting right:
//! `usage.record`'s `apply` **accumulates** into per-model lifetime totals, so
//! each record is one generation's delta. [`read_transcript_stats`] therefore
//! takes the *last* record rather than the sum — summing is a billing figure
//! that only grows, and as a context gauge would show a long session at several
//! hundred percent.
//!
//! `<workDirKey>` is never decoded anywhere in this module. The picker doesn't
//! need it (`session_index.jsonl` records each session's working directory
//! outright), and [`wire_log_for`] walks the buckets to find a session by id,
//! which is what Kimi's own readers do.
//!
//! What the source read settled, so a probe need not (each was on this list):
//! the hook schema and the `matcher` grammar (strict, regex — see
//! [`build_hooks_settings`]); no content-keyed trust gate exists, so
//! [`merge_hooks`]'s byte-stability now buys only race-freedom across
//! concurrent launches; hooks inherit the full process environment, so
//! `CAPTAIN_MIAO_SOCK` survives; the per-event field names `tool_name`,
//! `prompt`, `hook_event_name` are the runner's own camel→snake spellings;
//! `session_index.jsonl` is appended on every session create; there is no fork
//! flag on the whole CLI option list; and `TurnStarted` fires per user-visible
//! turn (its matcher value is the origin kind), not per LLM call — the Pi
//! worry was unfounded.
//!
//! What a probe against a real binary must still settle:
//! - **whether `Ctrl+V` reaches the dashboard's clipboard in a pooled session.**
//!   The launch is shimmed like every backend's ([`super::with_shim_path`]), so
//!   this works if the agent reads the clipboard by shelling out to
//!   `xclip`/`wl-paste`, and silently does nothing if it reads it in-process the
//!   way Codex does — the one case no shim can serve. Untested either way, and
//!   the only unknown here a *user* meets rather than a probe runner.
//!   `clipboard-paste` in the session is the fallback that works regardless.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::shell_quote;
use super::synth_home::{CopiedEntry, SynthHome, atomic_write};
use crate::agent::{ResumeCandidate, TranscriptStats};
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "kimi";

// =============================================================================
// Filesystem locations
// =============================================================================

/// The real Kimi home — `$KIMI_CODE_HOME` if the user set one globally, else
/// `~/.kimi-code`. It holds config, credentials and `sessions/`: the env var
/// relocates the **entire** data dir, not just the config, which is what makes
/// the symlink farm below both possible and necessary.
///
/// This is what the synthetic home mirrors; it is *not* what the launched agent
/// is handed (see [`ensure_synth_home`]).
fn kimi_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("KIMI_CODE_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    dirs::home_dir().map(|h| h.join(".kimi-code"))
}

/// A single shared synthetic `$KIMI_CODE_HOME` for every Kimi session: the real
/// home mirrored through symlinks, with `config.toml` copied writable and
/// carrying our hooks. Shared rather than per-session, and with byte-stable
/// contents, so that a content-keyed trust check (if Kimi has one — see the
/// module doc) is satisfied at most once per machine instead of once per launch.
fn synth_home() -> PathBuf {
    crate::state::state_dir().join("kimi-home")
}

// =============================================================================
// Transcript stats (wire.jsonl → context tokens + model)
// =============================================================================

/// One `usage.record` line of `wire.jsonl`. Every op record is written as
/// `{"type": <name>, ...payload, "time": …}` — the payload spread at the top
/// level rather than nested — so these fields sit beside `type`.
///
/// Field names and types are from `agent-core-v2`'s **generated** wire manifest
/// (`docs/wire-manifest.d.ts`, `protocol_version 1.5`), which flags
/// `usage.record` `persisted`. This is not a guessed schema.
#[derive(Deserialize)]
struct UsageRecord {
    model: String,
    usage: TokenUsage,
}

/// `kosong/contract`'s "usage breakdown for a **single LLM generation**".
/// camelCase on the wire, which is why the rename is explicit — the rest of
/// Kimi's hook payload is snake_case, and the two conventions meet in this file.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsage {
    #[serde(default)]
    input_other: u64,
    #[serde(default)]
    input_cache_read: u64,
    #[serde(default)]
    input_cache_creation: u64,
}

impl TokenUsage {
    /// Kimi's own `inputTotal` — everything that went *into* the model,
    /// cache hits and writes included. Output tokens are excluded, matching
    /// `agents::claude`'s fold and for the same reason: this column asks how
    /// full the context window is, and a completion is not in the next request.
    fn input_total(&self) -> u64 {
        self.input_other + self.input_cache_read + self.input_cache_creation
    }
}

/// The context gauge and model, from the **last** `usage.record` in the log.
///
/// The last one, emphatically not the sum. Each record is one generation's
/// usage, and Kimi's persisted model folds them with `addUsage` into lifetime
/// per-model totals — so summing gives a billing figure that only ever grows,
/// which as a context gauge would show a long session at several hundred
/// percent. The last record's input side is the size of the prompt that was
/// actually sent, which is what the column means.
///
/// Recomputed from a whole-file read rather than an incremental one: `prior`'s
/// cursor is Claude-only, and Kimi's records are small. If that ever costs too
/// much, a bounded tail is the fix (Codex's approach), not a cursor.
pub fn read_transcript_stats(transcript: &Path) -> TranscriptStats {
    let Ok(body) = std::fs::read_to_string(transcript) else {
        return TranscriptStats::default();
    };
    let mut stats = TranscriptStats::default();
    for line in body.lines() {
        // Cheap pre-filter: the log carries 49 record types and only one of
        // them is worth deserializing.
        if !line.contains("\"usage.record\"") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<UsageRecord>(line) else {
            continue;
        };
        stats.context_tokens = Some(record.usage.input_total());
        if !record.model.trim().is_empty() {
            stats.model = Some(record.model);
        }
    }
    stats
}

// =============================================================================
// Resume picker
// =============================================================================

/// `<home>/session_index.jsonl` — one JSON object per line mapping a session to
/// its directory and its working directory. Appended to rather than rewritten,
/// so a session may appear more than once and the **last** line wins, which is
/// also how Kimi's own reader folds it.
///
/// This file is the whole reason the bucket directory under `sessions/` never
/// needs decoding: the cwd is recorded here explicitly, so the bucket is a
/// directory to walk and not a string to parse.
const SESSION_INDEX: &str = "session_index.jsonl";

/// The session directory Kimi keeps under `<home>/sessions/<bucket>/`.
const SESSIONS_DIR: &str = "sessions";

/// The conversation's own agent, as opposed to the `agent-N` subagents that can
/// appear beside it in a session's `agents/` directory.
const MAIN_AGENT: &str = "main";

/// One agent's append-only journal, `agents/<id>/wire.jsonl`.
const WIRE_LOG: &str = "wire.jsonl";

/// Kimi's per-session `state.json`. It carries more than this (`agents`,
/// `custom`, `isCustomTitle`); everything unnamed is ignored so a Kimi that
/// grows a field still parses.
#[derive(Deserialize, Default)]
struct SessionState {
    #[serde(default)]
    title: Option<String>,
    /// The most recent prompt, not the first — Kimi records no first prompt.
    /// It stands in as the row's label for a session Kimi has not titled, which
    /// is the same job `first_prompt` does for Claude and Codex even though the
    /// prompt it names is a different one.
    #[serde(default, rename = "lastPrompt")]
    last_prompt: Option<String>,
}

fn read_session_index(home: &Path) -> std::collections::HashMap<String, String> {
    #[derive(Deserialize)]
    struct Entry {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(default, rename = "workDir")]
        work_dir: String,
    }
    let mut out = std::collections::HashMap::new();
    let Ok(body) = std::fs::read_to_string(home.join(SESSION_INDEX)) else {
        return out;
    };
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(entry) = serde_json::from_str::<Entry>(line) {
            out.insert(entry.session_id, entry.work_dir);
        }
    }
    out
}

/// Every session under `<home>/sessions/`, newest first.
///
/// A session is a directory holding a `state.json`, two levels down. Its id is
/// the directory's name, and its **cwd comes from `session_index.jsonl`** —
/// state.json does not record one, which is why a session missing from the
/// index is skipped rather than offered with a blank cwd that `r` would then
/// resume into the wrong place.
///
/// No tokens come back with the row even though Kimi does record them: they
/// live in `agents/<id>/wire.jsonl` as `usage.record` entries, which is a fold
/// over an append-only log rather than a field, and belongs to
/// `read_transcript_stats` rather than to a picker that stats one file per
/// candidate.
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    let home = kimi_home().ok_or_else(|| anyhow::anyhow!("no kimi home"))?;
    Ok(list_resumable_in(&home, limit))
}

/// The scan itself, split from `$KIMI_CODE_HOME` resolution so a test can point
/// it at a fixture tree without touching the environment.
fn list_resumable_in(home: &Path, limit: usize) -> Vec<ResumeCandidate> {
    let index = read_session_index(home);

    let mut found = Vec::new();
    for bucket in common::read_subdirs(&home.join(SESSIONS_DIR)) {
        for session_dir in common::read_subdirs(&bucket) {
            let state = session_dir.join("state.json");
            let Ok(mtime) = std::fs::metadata(&state).and_then(|m| m.modified()) else {
                continue;
            };
            found.push((session_dir, mtime));
        }
    }

    let mut out = Vec::new();
    for (dir, mtime) in common::newest_first(found, limit) {
        let Some(session_id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(cwd) = index.get(session_id).filter(|c| !c.trim().is_empty()) else {
            continue;
        };
        let state = std::fs::read_to_string(dir.join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_str::<SessionState>(&b).ok())
            .unwrap_or_default();
        out.push(ResumeCandidate {
            agent: crate::agent::AgentControl::Kimi,
            session_id: session_id.to_string(),
            cwd: cwd.clone(),
            first_prompt: state.last_prompt.filter(|p| !p.trim().is_empty()),
            custom_title: state.title.filter(|t| !t.trim().is_empty()),
            git_branch: None,
            mtime,
        });
    }
    out
}

// =============================================================================
// Launcher: process spawn + synthetic KIMI_CODE_HOME
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    // The launcher already wrote our hook block to `settings_path`; merge it
    // into the synthetic home's config.toml, which is the only place Kimi looks
    // for hooks (there is no override flag and no per-invocation injection).
    // Note the file the launcher wrote is named `…-settings.json` and holds
    // **TOML** — that path is generic transport, opaque to the launcher, and
    // every backend puts its own format through it.
    let hooks_toml =
        std::fs::read_to_string(settings_path).context("reading kimi hook settings")?;
    let home = ensure_synth_home(&hooks_toml)?;

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    cmd.env("KIMI_CODE_HOME", &home);
    // The hook subprocess reads the launcher socket from here rather than from
    // an argv flag: the synthetic home is shared by every session, so its
    // config.toml cannot carry a per-session path — and must not, if Kimi hashes
    // it for trust. Whether Kimi passes its own environment through to hooks is
    // the one thing about this that a probe has to confirm (module doc).
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    cmd.args(launch_args(extra_args));
    Ok(cmd)
}

/// The agent-facing argv: whatever the launcher forwarded (`--session <id>`),
/// and **nothing else**.
///
/// No directory argument of any kind. `cwd` reaches Kimi as the spawned
/// process's working directory (`current_dir` above), which every CLI agent
/// honours; what `kimi`'s own positional means is undocumented, and Reasonix is
/// the standing reminder that guessing costs a session whose first user message
/// is a path. A flag or positional can be added the day one is documented —
/// adding it now would be inventing argv.
fn launch_args(extra: &[String]) -> Vec<String> {
    extra.to_vec()
}

/// Create / refresh the synthetic home and return it: mirror the real Kimi home
/// through symlinks, copy `config.toml` writable, and merge our hooks into that
/// copy. The mirroring — dangling links, shadowing entries, file modes — lives
/// in [`super::synth_home`].
///
/// `config.toml` is **copied, not symlinked** because the real file is frequently
/// read-only (a nix-store / home-manager symlink), and an agent that writes back
/// into its own home then fails on that write. It also has to be a copy here for
/// a second reason — we *edit* it, and editing the user's real config to inject
/// our hooks is not ours to do.
///
/// `owned` is empty: unlike Claude's and Reasonix's separate settings files,
/// Kimi has no hooks file to own. The hooks live inside the config, which is why
/// this backend needs the copy mechanism that Reasonix did not.
fn ensure_synth_home(hooks_toml: &str) -> Result<PathBuf> {
    let real = kimi_home();
    // Pre-seed the real home's state tree before mirroring, so every entry the
    // agent writes state into exists to be linked **from the first launch**.
    // Without this, a first-ever Kimi session mints `sessions/` (or, worse,
    // `credentials/` during a first login) *inside* the synthetic home as a
    // shadow: the dashboard, resolving the real home, never sees it, and the
    // shadow is quarantined the moment the real home grows the name. The dirs
    // and modes are exactly what kimi itself creates (0700; its storage
    // service is seeded 0700/0600), so seeding them early changes nothing the
    // agent would not have done.
    if let Some(real) = &real {
        seed_real_state(real);
    }
    let home = SynthHome {
        dir: synth_home(),
        real: real.clone(),
        owned: &[],
        copied: &[CopiedEntry {
            name: "config.toml",
            snapshot: ".config-source.toml",
        }],
        // Nothing to adopt: `credentials/` is a *directory*, and `seed_real_state`
        // above has already created it in the real home — so a login writes
        // through the link rather than into a shadow. A seeded directory is the
        // stronger fix wherever it is available, because the agent's temporary
        // file and its rename both land inside the real directory.
        adopted: &[],
        prune: false,
    };
    home.ensure()?;
    // `session_index.jsonl` is a top-level *file* appended on every session
    // create, so it can't be pre-created as a directory and pre-creating an
    // empty file would be writing state kimi didn't. A **dangling** link is
    // the right tool instead — `open(O_CREAT)` follows it, so the agent's
    // first append lands the file in the real home (the property
    // `super::synth_home` documents on its linking pass).
    if let Some(real) = &real {
        let link = home.dir.join(SESSION_INDEX);
        if std::fs::symlink_metadata(&link).is_err() {
            let _ = std::os::unix::fs::symlink(real.join(SESSION_INDEX), &link);
        }
    }
    merge_hooks(&home.dir.join("config.toml"), hooks_toml);
    Ok(home.dir)
}

/// The real-home state directories seeded by [`ensure_synth_home`] — the ones
/// kimi is documented to write into (`data-locations.md`), each of which would
/// otherwise be minted as a synthetic-home shadow on a machine where kimi has
/// not created it yet. Config-shaped entries (`config.toml`, `tui.toml`,
/// `mcp.json`, `AGENTS.md`) are deliberately absent: creating those speaks for
/// the user.
const SEEDED_STATE_DIRS: &[&str] = &["sessions", "credentials", "logs", "updates", "user-history"];

/// Create the seeded dirs (and the real home itself) owner-only, matching the
/// modes kimi mints. Only ever *creates* — an existing dir keeps whatever
/// modes the user or agent gave it. Best-effort: a failure here reproduces the
/// pre-seeding status quo rather than adding a new failure mode.
fn seed_real_state(real: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for dir in
        std::iter::once(real.to_path_buf()).chain(SEEDED_STATE_DIRS.iter().map(|d| real.join(d)))
    {
        if std::fs::symlink_metadata(&dir).is_err() && std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
    }
}

/// The substring that identifies a `[[hooks]]` entry as ours. Every command we
/// emit contains it and no plausible hook of the user's does, so a re-merge can
/// drop our previous entries without touching theirs.
const HOOK_MARKER: &str = "hook --agent kimi";

/// Seconds Kimi waits for one of our hooks. Well inside the documented 1–600
/// range and deliberately at the bottom of it: the hook is a single socket
/// write, and on the three blockable events (`UserPromptSubmit`, `PreToolUse`,
/// `Stop`) the turn waits this long before giving up.
const HOOK_TIMEOUT_SECS: i64 = 5;

/// Merge our `[[hooks]]` entries into the synth home's `config.toml`: drop the
/// entries we wrote last time, append the current ones, keep everything else.
///
/// Re-run on every launch and idempotent, so the file tracks a changed exe path
/// after a rebuild and is re-applied immediately after
/// [`super::synth_home::SynthHome`] reseeds the copy from a changed real config.
/// The write is skipped when the bytes would not change — a content-keyed trust
/// check (if Kimi has one) must see a stable file, and concurrent launches must
/// not race a rewrite.
///
/// Best-effort, like Codex's profile trust generation, and asymmetric on
/// purpose: a config that is merely **absent** is created, but one that is
/// present and doesn't parse — or whose `hooks` key holds something that isn't
/// an array of tables — is left byte-for-byte alone. Treating an unparseable
/// config as an empty one would replace the user's file with nothing but our
/// hooks, and the file we hold is their whole configuration mirrored. Losing
/// the hooks (the session runs untracked) is the cheaper failure by a wide margin.
///
/// The rewrite is a full TOML re-serialization, so the copy loses the real
/// file's comments and key order. That is confined to the copy: the user's real
/// `config.toml` is never opened for writing, and the copy is reseeded from it
/// whenever it changes.
fn merge_hooks(config_path: &Path, hooks_toml: &str) {
    // Every bail is logged, because the symptom of an unhooked session is a row
    // that sits at `Starting` and nothing else — there is no error Kimi reports
    // and no output to read. This log is the only place the reason exists.
    let bail = |why: &str| tracing::warn!("kimi hooks not installed ({why}); sessions won't track");

    let ours = match hooks_toml.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => return bail(&format!("our own hook block didn't parse: {e}")),
    };
    let Some(ours) = ours.get("hooks").and_then(|h| h.as_array()) else {
        return bail("our own hook block carried no `hooks` array");
    };

    // A config that is *absent* starts from an empty document. One that is
    // present and unreadable, or present and unparseable, is left strictly alone.
    let existing = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return bail(&format!("{} is unreadable: {e}", config_path.display())),
    };
    let mut doc = match existing.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => return bail(&format!("{} is not valid TOML: {e}", config_path.display())),
    };

    let mut hooks: Vec<toml::Value> = match doc.get("hooks") {
        Some(toml::Value::Array(a)) => a.clone(),
        // The user's config uses `hooks` for something we don't understand.
        Some(other) => return bail(&format!("`hooks` is a {}, not an array", other.type_str())),
        None => Vec::new(),
    };
    hooks.retain(|h| !is_ours(h));
    hooks.extend(ours.iter().cloned());
    doc.insert("hooks".to_string(), toml::Value::Array(hooks));

    let serialized = match toml::to_string(&doc) {
        Ok(s) => s,
        Err(e) => return bail(&format!("the merged config wouldn't serialize: {e}")),
    };
    // Only when the bytes would actually change: a content-keyed trust check (if
    // Kimi has one) must see a stable file, and two concurrent launches must not
    // race a rewrite.
    if serialized != existing
        && let Err(e) = atomic_write(config_path, serialized.as_bytes())
    {
        bail(&format!(
            "{} couldn't be written: {e}",
            config_path.display()
        ));
    }
}

/// Is this `[[hooks]]` entry one we wrote? Keyed on the command containing
/// [`HOOK_MARKER`], so a hook of the user's — even one on the same event — is
/// never dropped.
fn is_ours(hook: &toml::Value) -> bool {
    hook.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains(HOOK_MARKER))
}

/// Build the `[[hooks]]` block merged into the synth home's `config.toml`.
///
/// **Exactly three keys per entry — `event`, `command`, `timeout`.** The
/// schema is strict and validated as one array, so a single entry with an
/// unknown key drops the whole `hooks` section — ours *and the user's* — with
/// only a warning diagnostic; this is pinned by a test rather than left to
/// review. `matcher` is deliberately absent (see the closure below). `timeout`
/// is seconds, range 1–600, default 30; it is set explicitly and small because
/// three of the events we register (`UserPromptSubmit`, `PreToolUse`, `Stop`)
/// are blockable, so a timeout there stalls the turn for its full duration. A
/// socket write needs none of 30s.
///
/// The **native event name → our [`HookEvent`] mapping happens here**, in the
/// config, not in the parser: the command already names the event it forwards,
/// so `Interrupt` registering `stop` and `PermissionResult` registering
/// `elicitation-result` costs no code at all. That is the same technique
/// Reasonix uses for `UserPromptSubmit` → `prompt-submit`.
///
/// The three events registered under a name that isn't their own:
/// - **`Interrupt` → `Stop`.** A turn the user interrupted is over, not failed.
///   This is what replaces Codex's rollout sentinel entirely.
/// - **`PermissionResult` → `ElicitationResult`**, which is the shared "the user
///   answered, resume" arm; the paired `PermissionRequest` is the gate.
/// - **`TurnStarted` → `PromptSubmit`**, the turn-boundary `Active` marker. It
///   covers the turn that begins without a user prompt (a `--session` resume
///   that continues on its own), and re-asserting `Active` for a turn that did
///   have one is free — the arm keeps `last_prompt` when the payload carries no
///   prompt.
///
/// Not registered, and why:
/// - **`TaskStarted`** — no paired completion event is documented, so mapping it
///   to `Active` would strand a row there until the next foreground event, and
///   what a "task" is (foreground step? subagent? background unit?) is exactly
///   the thing this backend has no way to check. `TurnStarted` already carries
///   the foreground turn boundary.
/// - **`UserPromptQueued`** and **`SessionHeartbeat`** — no state of ours moves,
///   and the heartbeat is periodic, so registering it would spawn a subprocess
///   per tick to discard its payload.
/// - **`SessionEnd`** — the launcher owns exit.
/// - **`SubagentStart` / `SubagentStop`** — nothing here distinguishes a
///   subagent's `Stop` from the session's yet; that is a separate piece of work.
///
/// Like Codex's and Reasonix's, the block carries **no per-session data** — the
/// socket arrives via `$CAPTAIN_MIAO_SOCK` — because one config serves every
/// session and its bytes must not move between launches.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    let exe_q = shell_quote(&exe.to_string_lossy());

    let hook = |event: &str, forwarded: HookEvent| -> toml::Value {
        let mut t = toml::map::Map::new();
        t.insert("event".to_string(), toml::Value::String(event.to_string()));
        // **No `matcher` key: "match everything" is the matcher's absence.**
        // Kimi compiles the matcher as a JS regex and treats one that fails to
        // compile as matching *nothing* (`runner.ts`'s `matches` catches the
        // throw and returns false) — and `new RegExp("*")` throws. So the `"*"`
        // every other backend spells "every tool" here silently disarms the
        // hook it is on. The key is `.optional()` and an absent matcher reads
        // as `""`, whose `RegExp` matches everything.
        t.insert(
            "command".to_string(),
            toml::Value::String(format!(
                "{exe_q} hook --agent kimi {}",
                forwarded.as_kebab()
            )),
        );
        t.insert(
            "timeout".to_string(),
            toml::Value::Integer(HOOK_TIMEOUT_SECS),
        );
        toml::Value::Table(t)
    };

    let hooks = toml::Value::Array(vec![
        hook("SessionStart", HookEvent::SessionStart),
        hook("UserPromptSubmit", HookEvent::PromptSubmit),
        hook("TurnStarted", HookEvent::PromptSubmit),
        hook("PreToolUse", HookEvent::PreToolUse),
        hook("PostToolUse", HookEvent::PostToolUse),
        hook("PostToolUseFailure", HookEvent::PostToolUseFailure),
        hook("PermissionRequest", HookEvent::PermissionRequest),
        hook("PermissionResult", HookEvent::ElicitationResult),
        hook("PreCompact", HookEvent::PreCompact),
        hook("PostCompact", HookEvent::PostCompact),
        hook("Stop", HookEvent::Stop),
        hook("StopFailure", HookEvent::StopFailure),
        hook("Interrupt", HookEvent::Stop),
    ]);

    let mut doc = toml::map::Map::new();
    doc.insert("hooks".to_string(), hooks);
    toml::to_string(&toml::Value::Table(doc)).unwrap_or_default()
}

// =============================================================================
// Hook payload (stdin from Kimi → normalized HookMessage)
// =============================================================================

/// Kimi's hook payload, reduced to the fields we act on. **snake_case**, like
/// Claude's and Codex's.
///
/// `session_id`, `session_title` and `cwd` are documented as riding *every*
/// payload (alongside `hook_event_name` and `client_type`, which we don't read —
/// the event is already in our argv, and the client type says nothing about a
/// session's state).
///
/// `tool_name` and `prompt` are the **assumed** spellings of two per-event
/// fields; the docs say "plus per-event fields — snake_case" without naming
/// them, and these are what Claude and Codex call the same two. The guess is
/// deliberately fail-safe rather than merely convenient: a wrong name
/// deserializes to `None`, which is exactly what omitting the field entirely
/// would produce, so the worst case is an empty Tool column and no prompt
/// preview — never a wrong value on a row.
///
/// **No error field is read**, for the opposite reason. `StopFailure` and
/// `PostToolUseFailure` carry the failure text under a name we don't know, and
/// guessing wrong there would drop the message silently. Leaving `message` as
/// `None` makes `dispatch_default` fall back to `raw` — the whole payload,
/// verbatim — which is what `raw` exists for and shows the user the real text
/// under whatever key it actually has.
///
/// **No transcript path**, which no documented field supplies. That is what
/// keeps the launcher's entire transcript pipeline (stats fold, signal scan,
/// stat poll) inert for Kimi by construction: it only ever runs on a path a hook
/// gave it.
#[derive(Deserialize)]
struct HookPayload {
    session_id: Option<String>,
    session_title: Option<String>,
    cwd: Option<String>,
    tool_name: Option<String>,
    prompt: Option<String>,
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse kimi hook JSON from stdin")?;
    // Resolved before the message is built so the id needn't be cloned.
    let transcript_path = payload
        .session_id
        .as_deref()
        .and_then(wire_log_for)
        .map(|p| p.to_string_lossy().into_owned());
    Ok(HookMessage {
        event,
        session_id: payload.session_id,
        tool_name: payload.tool_name,
        // See the struct doc: the failure events' error field has no documented
        // name, so the raw payload stands in rather than a guess that silently
        // reports nothing.
        message: None,
        cwd: payload.cwd,
        prompt: payload.prompt,
        // The field this whole backend is cheap because of.
        session_title: payload.session_title,
        // Folded from `wire.jsonl` rather than reported here — one fact, one
        // source (`common::adopt_session_facts`).
        context_tokens: None,
        model: None,
        transcript_path,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

/// The main agent's wire log for `session_id`, or `None` if its session
/// directory can't be found.
///
/// **Kimi's payload names no transcript**, so the path is resolved from the
/// session id instead: `sessions/*/<session_id>/agents/main/wire.jsonl`. The
/// bucket is walked rather than derived — the same reason [`list_resumable`]
/// walks it, and Kimi's own readers do too.
///
/// `main` specifically, out of the several agents a session can hold
/// (`state.json`'s `agents` map is keyed `main` / `agent-N`): the row's context
/// gauge is the *conversation's*, and a subagent's log is a separate one whose
/// numbers would overwrite it on whichever hook fired last.
///
/// This runs inside the `miao hook` subprocess, once per event, so it is one
/// `read_dir` over the buckets — deliberately not a glob crate or a recursive
/// walk. A miss just leaves the columns empty.
fn wire_log_for(session_id: &str) -> Option<PathBuf> {
    if session_id.trim().is_empty() {
        return None;
    }
    let root = kimi_home()?.join(SESSIONS_DIR);
    for bucket in super::common::read_subdirs(&root) {
        let candidate = bucket
            .join(session_id)
            .join("agents")
            .join(MAIN_AGENT)
            .join(WIRE_LOG);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Kimi departs from [`common::dispatch_default`] nowhere. Every event it emits
/// is one of ours under a different spelling, and the three renamings are done
/// in the config ([`build_hooks_settings`]) rather than here, so there is no
/// per-agent arm to keep in sync — including for `Interrupt`, the case that
/// costs both other backends a whole transcript-reading mechanism.
///
/// The wrapper stays so the seam keeps one callee per backend, and so the day
/// Kimi grows a case of its own (a subagent-aware `Stop` is the likely first) it
/// has a place to land.
pub async fn dispatch_hook(state: &mut LauncherState, msg: HookMessage) {
    common::dispatch_default(state, msg)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::SessionStatus;

    /// A `$KIMI_CODE_HOME` tree: `sessions/<bucket>/<id>/state.json`, plus the
    /// home-level `session_index.jsonl` that is the only record of a session's
    /// working directory.
    fn home_fixture(tag: &str, sessions: &[(&str, &str, &str)], index: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("cm-kimi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        for (bucket, id, body) in sessions {
            let dir = home.join(SESSIONS_DIR).join(bucket).join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("state.json"), body).unwrap();
        }
        std::fs::write(home.join(SESSION_INDEX), index).unwrap();
        home
    }

    #[test]
    fn sessions_become_resume_candidates() {
        let home = home_fixture(
            "ok",
            &[(
                "bucket-1",
                "session_abc",
                r#"{"createdAt":1,"updatedAt":2,"title":"wire up the parser",
                    "isCustomTitle":true,"lastPrompt":"add a test","agents":{"main":{"type":"main"}}}"#,
            )],
            r#"{"sessionId":"session_abc","sessionDir":"/x","workDir":"/home/miao/p"}
"#,
        );
        let out = list_resumable_in(&home, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "session_abc");
        // The cwd is the index's, never the bucket name decoded.
        assert_eq!(out[0].cwd, "/home/miao/p");
        assert_eq!(out[0].custom_title.as_deref(), Some("wire up the parser"));
        assert_eq!(out[0].first_prompt.as_deref(), Some("add a test"));
        assert_eq!(out[0].agent, AgentControl::Kimi);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The index is appended to rather than rewritten, so a session can appear
    /// more than once — and the last line is the current one. Kimi's own reader
    /// folds it the same way.
    #[test]
    fn the_last_index_line_wins() {
        let home = home_fixture(
            "moved",
            &[("b", "session_abc", r#"{"title":"t"}"#)],
            "{\"sessionId\":\"session_abc\",\"sessionDir\":\"/x\",\"workDir\":\"/home/miao/old\"}\n\
             {\"sessionId\":\"session_abc\",\"sessionDir\":\"/x\",\"workDir\":\"/home/miao/new\"}\n",
        );
        let out = list_resumable_in(&home, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cwd, "/home/miao/new");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A session the index has never heard of has no working directory anywhere
    /// on disk, so it is dropped rather than offered with a blank cwd that `r`
    /// would then resume into the wrong place.
    #[test]
    fn a_session_missing_from_the_index_is_not_offered() {
        let home = home_fixture(
            "unindexed",
            &[
                ("b", "session_known", r#"{"title":"t"}"#),
                ("b", "session_unknown", r#"{"title":"t"}"#),
            ],
            "{\"sessionId\":\"session_known\",\"sessionDir\":\"/x\",\"workDir\":\"/home/miao/p\"}\n\
             not json at all\n",
        );
        let out = list_resumable_in(&home, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "session_known");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// An unreadable `state.json` still yields a row — the id and the cwd, the
    /// two things a resume actually needs, come from the directory name and the
    /// index rather than from that file.
    #[test]
    fn a_corrupt_state_file_still_yields_a_resumable_row() {
        let home = home_fixture(
            "corrupt",
            &[("b", "session_abc", "{ this is not json")],
            "{\"sessionId\":\"session_abc\",\"sessionDir\":\"/x\",\"workDir\":\"/home/miao/p\"}\n",
        );
        let out = list_resumable_in(&home, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cwd, "/home/miao/p");
        assert_eq!(out[0].custom_title, None);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// **The fold that matters.** Each `usage.record` is one generation's usage,
    /// and Kimi's own reducer sums them into lifetime totals — so the context
    /// gauge is the *last* record's input side, never the running total.
    #[test]
    fn the_context_gauge_is_the_last_generation_not_the_sum() {
        let path = std::env::temp_dir().join(format!("cm-kimi-wire-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"metadata","protocol_version":"1.5","created_at":1}"#,
                "\n",
                r#"{"type":"turn.prompt","prompt":"hi","time":2}"#,
                "\n",
                r#"{"type":"usage.record","model":"kimi-k2","usage":{"inputOther":100,"output":500,"inputCacheRead":50,"inputCacheCreation":25},"time":3}"#,
                "\n",
                r#"{"type":"usage.record","model":"kimi-k2","usage":{"inputOther":200,"output":900,"inputCacheRead":300,"inputCacheCreation":0},"time":4}"#,
                "\n",
            ),
        )
        .unwrap();
        let stats = read_transcript_stats(&path);
        // 200 + 300 + 0 — the last generation's prompt. Not 675 (the sum), and
        // not 1400 (the sum with completions).
        assert_eq!(stats.context_tokens, Some(500));
        assert_eq!(stats.model.as_deref(), Some("kimi-k2"));
        let _ = std::fs::remove_file(&path);
    }

    /// A log with no usage yet — every session's first moments — leaves both
    /// columns empty rather than showing a zero.
    #[test]
    fn a_log_with_no_usage_yet_reports_nothing() {
        let path =
            std::env::temp_dir().join(format!("cm-kimi-nousage-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"metadata","protocol_version":"1.5"}"#,
                "\n",
                r#"{"type":"turn.prompt","prompt":"hi"}"#,
                "\n",
                "not json at all\n",
            ),
        )
        .unwrap();
        let stats = read_transcript_stats(&path);
        assert_eq!(stats.context_tokens, None);
        assert_eq!(stats.model, None);
        let _ = std::fs::remove_file(&path);

        // And an absent log is the same answer, not an error.
        let stats = read_transcript_stats(Path::new("/nonexistent/wire.jsonl"));
        assert_eq!(stats.context_tokens, None);
    }

    /// The path is resolved from the session id by walking the buckets, so a
    /// session id that matches nothing simply leaves the transcript unset.
    #[test]
    fn an_unknown_session_id_resolves_to_no_transcript() {
        assert_eq!(wire_log_for(""), None);
        assert_eq!(wire_log_for("   "), None);
    }

    /// An empty or absent home is an empty picker, not an error.
    #[test]
    fn a_missing_home_is_empty_rather_than_an_error() {
        let home = std::env::temp_dir().join(format!("cm-kimi-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(list_resumable_in(&home, 10).is_empty());
    }

    /// **Hand-written from the vendor's documented payload shape, not captured
    /// from a running binary** — no `kimi` was installed when these were
    /// written. A probe that captures real payloads (point a hook command at
    /// `tee`) should diff them against these and correct them here first.
    fn payload(extra: &str) -> String {
        format!(
            r#"{{"hook_event_name":"Stop","session_id":"s1","session_title":"wire up the parser",
                "client_type":"cli","cwd":"/home/miao/p"{extra}}}"#
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::Kimi, status)
    }

    /// Drive one hook end to end — parse the agent's stdin JSON, then dispatch
    /// it — so the tests take the same path a live hook takes.
    fn feed(state: &mut LauncherState, event: HookEvent, stdin: &str) {
        let msg = parse_hook_payload(event, stdin).expect("payload parses");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(dispatch_hook(state, msg));
    }

    /// Parse the emitted config back into its `[[hooks]]` entries.
    fn hook_entries(toml_text: &str) -> Vec<toml::Table> {
        toml_text
            .parse::<toml::Table>()
            .expect("the hook block is valid TOML")["hooks"]
            .as_array()
            .expect("an array of hook tables")
            .iter()
            .map(|v| v.as_table().expect("a hook table").clone())
            .collect()
    }

    /// The `command` an entry forwards, as the kebab event our `hook`
    /// subcommand receives.
    fn forwarded(entry: &toml::Table) -> &str {
        entry["command"]
            .as_str()
            .expect("a command string")
            .rsplit(' ')
            .next()
            .expect("a trailing event name")
    }

    fn entry_for<'a>(entries: &'a [toml::Table], event: &str) -> &'a toml::Table {
        entries
            .iter()
            .find(|e| e["event"].as_str() == Some(event))
            .unwrap_or_else(|| panic!("no {event} hook is registered"))
    }

    #[test]
    fn a_turn_runs_from_prompt_to_stop() {
        let mut state = state_at(SessionStatus::Idle);
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload(r#","prompt":"go""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_prompt.as_deref(), Some("go"));
        // The session id rides every payload, so the launcher learns it here
        // rather than from a session file.
        assert_eq!(state.session_id.as_deref(), Some("s1"));

        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload(r#","tool_name":"bash""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool.as_deref(), Some("bash"));

        feed(&mut state, HookEvent::Stop, &payload(""));
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_tool, None);
    }

    /// `PermissionRequest` gates and `PermissionResult` releases — a native
    /// pair, so the approval state needs no second mechanism.
    #[test]
    fn a_permission_request_gates_and_its_result_releases() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &payload(r#","tool_name":"bash""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);

        feed(&mut state, HookEvent::ElicitationResult, &payload(""));
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// The title rides every payload straight onto `name`, which is what makes
    /// the title store, the sqlite reader and the per-host overlay all
    /// unnecessary for this backend. A later payload carrying a different title
    /// *is* the rename.
    #[test]
    fn the_session_title_lands_on_the_name_from_any_event() {
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::PreToolUse, &payload(""));
        assert_eq!(state.name.as_deref(), Some("wire up the parser"));

        let renamed = payload("").replace("wire up the parser", "renamed by the user");
        feed(&mut state, HookEvent::Stop, &renamed);
        assert_eq!(state.name.as_deref(), Some("renamed by the user"));
    }

    /// An interrupt is a real Kimi hook, and it is registered as a plain `Stop`
    /// — the whole reason this backend needs no transcript sentinel. Pinned
    /// here because the mapping lives in the config's `command`, where a review
    /// of the dispatcher would never see it.
    #[test]
    fn an_interrupt_is_registered_as_a_plain_stop() {
        let entries = hook_entries(&build_hooks_settings("/run/x.sock"));
        assert_eq!(forwarded(entry_for(&entries, "Interrupt")), "stop");
        assert_eq!(forwarded(entry_for(&entries, "Stop")), "stop");
        // And it ends the turn without leaving an error on the row: the shared
        // `Stop` arm, reached with nothing Kimi-specific in the way.
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, &payload(""));
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_error, None);
    }

    /// The constraint that would disarm every hook in the file: Kimi's hook
    /// schema is strict and validated as one array, so a single entry with an
    /// unknown key drops the **whole `hooks` section** — ours and the user's —
    /// with only a warning diagnostic. And `matcher` must be absent: Kimi
    /// compiles it as a JS regex and a failed compile matches *nothing*, so
    /// the `"*"` every other backend spells "every tool" silently disarms the
    /// hook it is on (see [`build_hooks_settings`]).
    #[test]
    fn every_hook_entry_has_exactly_the_three_emitted_keys() {
        let entries = hook_entries(&build_hooks_settings("/run/x.sock"));
        assert!(!entries.is_empty());
        for entry in &entries {
            let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["command", "event", "timeout"],
                "an unknown key drops Kimi's whole hooks section: {entry:?}"
            );
            // Documented range 1–600s. Ours is small because three of the
            // registered events block the turn while a hook runs.
            let timeout = entry["timeout"].as_integer().expect("an integer timeout");
            assert!(
                (1..=600).contains(&timeout),
                "timeout {timeout} out of range"
            );
        }
    }

    /// One config serves every session, so it must carry no per-session data —
    /// and it must register exactly the native event names Kimi emits, since the
    /// mapping onto our vocabulary happens in the `command` rather than in the
    /// dispatcher.
    #[test]
    fn the_hook_block_registers_the_native_names_and_no_socket() {
        let a = build_hooks_settings("/run/a.sock");
        let b = build_hooks_settings("/run/b.sock");
        assert_eq!(a, b, "the hook block must not embed the per-session socket");
        assert!(!a.contains(".sock"));

        let entries = hook_entries(&a);
        let mut names: Vec<&str> = entries
            .iter()
            .map(|e| e["event"].as_str().expect("an event name"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "Interrupt",
                "PermissionRequest",
                "PermissionResult",
                "PostCompact",
                "PostToolUse",
                "PostToolUseFailure",
                "PreCompact",
                "PreToolUse",
                "SessionStart",
                "Stop",
                "StopFailure",
                "TurnStarted",
                "UserPromptSubmit",
            ],
        );
        // The two other renamings that only exist in the config.
        assert_eq!(
            forwarded(entry_for(&entries, "PermissionResult")),
            "elicitation-result"
        );
        assert_eq!(
            forwarded(entry_for(&entries, "TurnStarted")),
            "prompt-submit"
        );
    }

    /// The payload is snake_case and the launcher reads five fields out of it.
    /// The two undocumented ones (`tool_name`, `prompt`) are assumed spellings,
    /// so this also pins the fail-safe half of that bet: a payload without them
    /// yields `None`, never a wrong value.
    #[test]
    fn the_payload_is_snake_case_and_a_missing_field_is_just_absent() {
        let msg = parse_hook_payload(
            HookEvent::PostToolUseFailure,
            &payload(r#","tool_name":"read_file","prompt":"go""#),
        )
        .expect("parses");
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
        assert_eq!(msg.session_title.as_deref(), Some("wire up the parser"));
        assert_eq!(msg.cwd.as_deref(), Some("/home/miao/p"));
        assert_eq!(msg.tool_name.as_deref(), Some("read_file"));
        assert_eq!(msg.prompt.as_deref(), Some("go"));
        // No documented field names a transcript, which is what keeps the
        // launcher's transcript machinery inert for Kimi.
        assert_eq!(msg.transcript_path, None);
        // A camelCase reading would find none of the above; guard the one field
        // whose absence would otherwise look like "the agent didn't send it".
        assert!(
            parse_hook_payload(HookEvent::Stop, r#"{"toolName":"bash"}"#)
                .expect("parses")
                .tool_name
                .is_none()
        );
    }

    /// The failure events keep the whole payload rather than a guessed error
    /// field, so the text the user needs is on the row under whatever key Kimi
    /// actually uses.
    #[test]
    fn a_failed_turn_reports_the_raw_payload() {
        let mut state = state_at(SessionStatus::Active);
        let stdin = payload(r#","error":"provider 500""#);
        feed(&mut state, HookEvent::StopFailure, &stdin);
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_error.as_deref(), Some(stdin.as_str()));
    }

    /// Seeding creates exactly the agent-written state dirs, owner-only, and
    /// never touches one that already exists — the user's own modes stand.
    #[test]
    fn seeding_creates_missing_state_dirs_and_leaves_existing_ones_alone() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("cm-kimi-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::set_permissions(
            home.join("sessions"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        seed_real_state(&home);

        for dir in SEEDED_STATE_DIRS {
            let meta = std::fs::metadata(home.join(dir)).expect(dir);
            assert!(meta.is_dir());
        }
        let mode = |p: &str| {
            std::fs::metadata(home.join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("credentials"), 0o700, "created owner-only");
        assert_eq!(mode("sessions"), 0o755, "an existing dir keeps its mode");
        let _ = std::fs::remove_dir_all(&home);
    }

    fn scratch_config(name: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "captain-miao-kimi-test-{}-{}.toml",
            std::process::id(),
            name,
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The merge has to survive a launch loop: our entries appear exactly once
    /// however many times it runs, the user's config and their own hooks are
    /// untouched, and the bytes stop moving — the last of which is what a
    /// content-keyed trust check (if Kimi has one) would depend on.
    #[test]
    fn merging_our_hooks_is_idempotent_and_keeps_the_users_own() {
        let user_hook = "'/usr/local/bin/notify' fired";
        let path = scratch_config(
            "merge",
            &format!(
                "model = \"kimi-k2\"\n\n[[hooks]]\nevent = \"Stop\"\nmatcher = \"*\"\n\
                 command = \"{user_hook}\"\ntimeout = 30\n"
            ),
        );
        let block = build_hooks_settings("/run/x.sock");

        merge_hooks(&path, &block);
        let once = std::fs::read_to_string(&path).unwrap();
        merge_hooks(&path, &block);
        let twice = std::fs::read_to_string(&path).unwrap();
        assert_eq!(once, twice, "a second launch must not move the bytes");

        let doc: toml::Table = twice.parse().expect("still valid TOML");
        assert_eq!(
            doc["model"].as_str(),
            Some("kimi-k2"),
            "the user's settings survive the rewrite"
        );
        let hooks = doc["hooks"].as_array().expect("an array of hooks");
        // Theirs, kept verbatim — including one on an event we also register.
        assert_eq!(
            hooks
                .iter()
                .filter(|h| h["command"].as_str() == Some(user_hook))
                .count(),
            1,
        );
        // Ours, exactly one copy of each.
        let ours = hook_entries(&block);
        assert_eq!(
            hooks.iter().filter(|h| is_ours(h)).count(),
            ours.len(),
            "our entries must not accumulate across launches"
        );

        let _ = std::fs::remove_file(path);
    }

    /// A config we can't understand is left exactly as it is: the hooks are lost
    /// (the session runs untracked) rather than the file being replaced, because
    /// a config Kimi then refuses takes down the *whole* load.
    #[test]
    fn an_unreadable_config_is_left_alone() {
        let block = build_hooks_settings("/run/x.sock");

        let broken = scratch_config("broken", "this is not = = toml\n");
        merge_hooks(&broken, &block);
        assert_eq!(
            std::fs::read_to_string(&broken).unwrap(),
            "this is not = = toml\n"
        );

        // `hooks` used for something that isn't an array of tables — not ours to
        // reinterpret.
        let odd = scratch_config("odd", "hooks = \"disabled\"\n");
        merge_hooks(&odd, &block);
        assert_eq!(
            std::fs::read_to_string(&odd).unwrap(),
            "hooks = \"disabled\"\n"
        );

        let _ = std::fs::remove_file(broken);
        let _ = std::fs::remove_file(odd);
    }

    /// A first launch on a machine with no Kimi config yet still gets hooks:
    /// the merge creates the file rather than requiring one to edit.
    #[test]
    fn an_absent_config_is_created_with_our_hooks() {
        let path = std::env::temp_dir().join(format!(
            "captain-miao-kimi-test-{}-absent.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        merge_hooks(&path, &build_hooks_settings("/run/x.sock"));
        let doc: toml::Table = std::fs::read_to_string(&path)
            .expect("the config was created")
            .parse()
            .expect("valid TOML");
        assert!(doc["hooks"].as_array().expect("hooks").iter().all(is_ours));

        let _ = std::fs::remove_file(path);
    }

    /// Nothing is passed as a directory: the cwd reaches Kimi as the spawned
    /// process's working directory, because what its own positional means is
    /// undocumented. Pinned separately from the launch command, which sets the
    /// process cwd too and would mask the mistake.
    #[test]
    fn the_argv_carries_only_what_the_launcher_forwarded() {
        assert!(launch_args(&[]).is_empty());
        assert_eq!(
            launch_args(&["--session".to_string(), "s1".to_string()]),
            ["--session", "s1"]
        );
    }
}
