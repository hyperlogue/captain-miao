//! Antigravity CLI integration, verified with `agy` 1.4.5. Only the current
//! CLI contract is supported; no compatibility layer for older versions.
//!
//! # Hooks and status snapshots
//!
//! `SessionStart`, `PreInvocation`, `PostToolUse` and `Stop` provide identity,
//! normal turn transitions and failures (`error` on a tool or Stop payload).
//! SessionStart still waits until a conversation is created: a fresh TUI can
//! sit at its empty prompt without a conversation id.
//!
//! The CLI's `statusLine` command supplies the missing observations. It runs
//! at initial idle, after Escape cancellation (which still emits no Stop),
//! when the model changes, and when context usage changes. Its `agent_state`
//! distinguishes idle from working/thinking/tool_use. Unknown states leave
//! status unchanged; snapshots do not reset prompt facts or failure messages.
//! The sum of `context_window.total_input_tokens` and `total_output_tokens`,
//! divided by `context_window_size`, tracks its reported usage percentage.
//! `current_usage` instead describes the last API call.
//! During manual `/compact`, `agent_state` is still `working`; the transcript's
//! user request distinguishes it. A completed `CHECKPOINT` marks `Compacted`.
//! Status snapshots retain pre-compaction usage even on resume, so context is
//! unknown until a measured `PLANNER_RESPONSE` follows that checkpoint.
//! The snapshot's transcript_path incorrectly names the GUI's state directory
//! in 1.3.5, so we resolve the CLI log from the conversation id instead.
//! The launcher incrementally folds first/last prompts from that log; the host
//! overlays titles from `conversation_summaries.db` through its shared cache.
//!
//! `tool_confirmation_pending` identifies a native permission prompt; its
//! absence on the next busy snapshot releases the approval state. The coarse
//! `agent_state` alone cannot distinguish approval from ordinary tool use.
//! PreToolUse is a permission gate, so we leave it alone. The launcher's task
//! monitor reads live commands/timers from the conversation database at rest;
//! Linux also uses a coalesced DB/WAL watch; macOS refreshes on hooks without
//! periodic checks. Stop's
//! `fullyIdle: false` retains Task through coarse idle snapshots. Automatic
//! compaction starts have no verified separate signals. Fork and
//! worktree flags are absent from the argv interface, although `/fork` exists
//! in the TUI.
//!
//! # Configuration and state
//!
//! The hidden `--gemini_dir` flag selects a managed Gemini root while agy,
//! hooks and child tools retain the real HOME. Verified with 1.3.4 and 1.3.5;
//! the flag is absent from `--help`. `--app_data_dir` alone is insufficient:
//! hooks still use the Gemini root, and legacy hook migration can overwrite
//! its global hooks file.
//!
//! Hooks are merged into the managed root's `config/hooks.json`.
//! A second mirror shadows `antigravity-cli/settings.json` to wrap the
//! user's status-line command: both receive the same JSON and the user's
//! stdout remains visible. Without a custom command the built-in line stays.
//! Real global and workspace hook/config files are never changed.
//!
//! The writable settings copy retains changes made inside managed sessions
//! until the real settings file changes or is deleted. Edits made inside those
//! sessions remain local. The original status-line configuration is reread
//! from the real file on each launch, preventing nested observers.
//! Conversations and state directories are linked to the real tree, including
//! on a first launch. Legacy synthetic-home state is adopted before seeding
//! those directories.
//! A failed migration aborts launch with the original state still in place.
//! Agent-created root files are adopted on subsequent launches, excluding
//! the settings copy and its source snapshot.
//!
//! The managed root reuses `antigravity-home/.gemini` from the old synthetic
//! home. Its parent is no longer mirrored; existing entries there are left
//! untouched because earlier sessions may have written user files into it.
//!
//! `Ctrl+V` was verified with 1.1.11: agy shells out to xclip for TARGETS and
//! image/png without requiring DISPLAY, matching the pooled clipboard bridge.
//! The opt-in live test uses a disposable home, a loopback Gemini fixture and
//! a PTY to check startup, completion, cancellation, approval, compaction,
//! clear, managed and bare resume, real HOME and global config preservation
//! without credentials or external inference.

mod metadata;
pub(crate) mod tasks;
pub use metadata::{
    PromptCursor, read_titles, read_transcript_stats, title_store_mtimes, title_watch_paths,
};

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::shell_quote;
use super::synth_home::{CopiedEntry, SynthHome};
use crate::agent::ResumeCandidate;
use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

/// The executable this backend drives — see [`super::claude::BIN`]. The product
/// is "Antigravity"; `agy` is what it installs as.
pub(crate) const BIN: &str = "agy";

/// The file Antigravity reads hooks from, in whichever customization root it is
/// looking at.
const HOOKS_FILE: &str = "hooks.json";

/// Our key in that file's map of named hook bundles. Named rather than
/// anonymous because the file is shared with the user's own hooks and with any
/// plugin's — see [`merge_hooks`].
const HOOK_NAME: &str = "captain-miao";

// =============================================================================
// Filesystem locations
// =============================================================================

/// The real `~/.gemini` — Antigravity's root for both its config and its state.
fn gemini_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini"))
}

/// Where the CLI keeps its state: `brain/<id>/` (transcripts and artifacts),
/// `conversations/<id>.db`, `history.jsonl`, `cache/`. Shared with the user's
/// own sessions on purpose — a captain-miao session is resumable from a bare
/// `agy` and the other way round.
fn cli_dir() -> Option<PathBuf> {
    gemini_dir().map(|g| g.join("antigravity-cli"))
}

/// Keep the previous mirror's path so managed settings and existing sessions
/// survive the switch from a synthetic HOME to an explicit Gemini root.
fn managed_gemini_dir() -> PathBuf {
    crate::state::state_dir()
        .join("antigravity-home")
        .join(".gemini")
}

/// A conversation's readable transcript. `transcript.jsonl` rather than the
/// `transcript_full.jsonl` beside it: the two carry the same steps, and the
/// truncated one is what Antigravity itself points its own summaries at.
fn transcript_path(cli_dir: &Path, id: &str) -> PathBuf {
    cli_dir
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl")
}

/// Both hook schemas report the conversation id. Resolve the CLI's actual
/// log even before it exists; the launcher retries arming on subsequent hooks.
fn live_transcript_path(id: Option<&str>) -> Option<String> {
    let id = id.filter(|id| {
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })?;
    Some(
        transcript_path(&cli_dir()?, id)
            .to_string_lossy()
            .into_owned(),
    )
}

// =============================================================================
// Launcher: process spawn + managed Gemini directory
// =============================================================================

/// Build the argv for an Antigravity session, redirecting only its Gemini
/// directory so hooks and tools inherit the real HOME.
pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    // The launcher wrote our hook and status-line configuration here;
    // install each part at the path the CLI actually reads.
    let hooks_json =
        std::fs::read_to_string(settings_path).context("reading antigravity hook settings")?;
    let settings: serde_json::Value = serde_json::from_str(&hooks_json)?;
    let gemini = ensure_managed_gemini_dir(&settings)?;

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    cmd.arg("--gemini_dir").arg(gemini);
    // The hook subprocess reads the launcher socket from here rather than from
    // an argv flag: the managed root is shared by every session, so its
    // hooks.json cannot carry a per-session path.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    // No positional for the working directory: `agy` takes the workspace from
    // the process cwd, which `common::agent_command` has already set. Its
    // `--add-dir` *adds* one rather than choosing it.
    cmd.args(extra_args.iter().cloned());
    Ok(cmd)
}

/// Mirror only the Gemini directories. Hooks and the CLI settings copy are
/// ours; the rest resolves to the real Gemini root.
///
/// Nothing is **pruned** at any level. These are state mirrors: a dangling link
/// is load-bearing, because the agent recreating that file writes *through* the
/// link into the real home and the two stay converged.
fn ensure_managed_gemini_dir(settings: &serde_json::Value) -> Result<PathBuf> {
    let real = gemini_dir().context("no home directory for Antigravity")?;
    let dir = managed_gemini_dir();
    ensure_managed_gemini_dir_at(&real, &dir, settings)?;
    Ok(dir)
}

fn ensure_managed_gemini_dir_at(
    real: &Path,
    dir: &Path,
    settings: &serde_json::Value,
) -> Result<()> {
    let gemini = SynthHome {
        dir: dir.to_path_buf(),
        real: Some(real.to_path_buf()),
        owned: &["config", "antigravity-cli"],
        copied: &[],
        adopted: &[],
        prune: false,
    };
    gemini.ensure()?;

    let config = SynthHome {
        dir: gemini.dir.join("config"),
        real: Some(real.join("config")),
        owned: &[HOOKS_FILE],
        copied: &[],
        // `hooks.json` is ours and the rest of this directory is configuration
        // the user writes, never the agent.
        adopted: &[],
        prune: false,
    };
    config.ensure()?;
    // Merge rather than replace: `hooks.json` is a map of named bundles, and
    // the user's own live in the same file. Re-read every launch so an edit to
    // the real file shows up in the next session.
    let theirs = std::fs::read_to_string(real.join("config").join(HOOKS_FILE)).unwrap_or_default();
    config.write_owned(
        HOOKS_FILE,
        &merge_hooks(&theirs, &settings["hooks"].to_string()),
    )?;
    ensure_cli_home(
        &real.join("antigravity-cli"),
        &gemini.dir.join("antigravity-cli"),
        settings,
    )
}

/// Shadow only CLI settings, keeping conversations, credentials and caches in
/// the real state tree. Settings stay writable in managed sessions; a changed
/// real settings file refreshes the copy on the next launch.
fn ensure_cli_home(real: &Path, dir: &Path, injected: &serde_json::Value) -> Result<()> {
    const SNAPSHOT: &str = ".captain-miao-settings-source";
    // Cannot collide with a valid JSON settings file.
    const ABSENT_SOURCE: &str = "\0absent";
    crate::state::create_dir_all_private(real)?;
    // Older launches linked the entire CLI directory. Remove only the link,
    // never its target, before creating the settings mirror.
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        match std::fs::remove_file(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    // Every other entry in this dedicated agent directory is agent-owned.
    // Adopt first, before seeding directories, so first-login state left by
    // the previous layout can move back without becoming a shadow.
    let names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name != "settings.json" && name != SNAPSHOT && !name.starts_with('.'))
        .collect();
    let adopted: Vec<&str> = names.iter().map(String::as_str).collect();
    let home = SynthHome {
        dir: dir.to_path_buf(),
        real: Some(real.to_path_buf()),
        owned: &[SNAPSHOT],
        copied: &[CopiedEntry {
            name: "settings.json",
            snapshot: SNAPSHOT,
        }],
        adopted: &adopted,
        prune: false,
    };
    home.ensure()?;
    // Adoption is best-effort in the generic mirror. Here seeding a missing
    // destination would make the next mirror pass quarantine the only copy
    // of a conversation tree after a failed rename (e.g. across filesystems).
    // Leave that tree in place and report the migration failure instead.
    for name in &names {
        let source = dir.join(name);
        let destination = real.join(name);
        if std::fs::symlink_metadata(&source).is_ok_and(|m| !m.file_type().is_symlink())
            && std::fs::symlink_metadata(&destination)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            anyhow::bail!(
                "could not migrate Antigravity state from {} to {}; existing state was preserved",
                source.display(),
                destination.display()
            );
        }
    }
    // Seed directories before agy creates them so atomic renames, SQLite WALs
    // and first-login state within them all land directly in the real tree.
    for name in [
        "annotations",
        "bin",
        "brain",
        "builtin",
        "cache",
        "conversations",
        "crashes",
        "implicit",
        "log",
        "presence",
        "updater",
    ] {
        crate::state::create_dir_all_private(&real.join(name))?;
    }
    home.ensure()?;
    // Root-level stores need links even on a fresh install. In particular,
    // never split a SQLite database from its WAL and shared-memory companions.
    for name in [
        "conversation_summaries.db",
        "conversation_summaries.db-wal",
        "conversation_summaries.db-shm",
        "history.jsonl",
        "installation_id",
        "jetski_state.pbtxt",
        "jetbox_summaries_proto.pb",
        "last_check.timestamp",
        "cli.log",
    ] {
        let link = dir.join(name);
        if std::fs::symlink_metadata(&link).is_err() {
            match std::os::unix::fs::symlink(real.join(name), &link) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    let read = |path: &Path| -> Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(body) => Ok(Some(body)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    let parse = |body: Option<&str>| -> Result<serde_json::Value> {
        match body.filter(|body| !body.trim().is_empty()) {
            Some(body) => serde_json::from_str(body).context("reading Antigravity settings"),
            None => Ok(serde_json::json!({})),
        }
    };
    let source = read(&real.join("settings.json"))?;
    let source_removed =
        source.is_none() && read(&dir.join(SNAPSHOT))?.is_some_and(|last| last != ABSENT_SOURCE);
    let mut settings = if source_removed {
        serde_json::json!({})
    } else {
        parse(read(&dir.join("settings.json"))?.as_deref())?
    };
    let original = parse(source.as_deref())?;
    let object = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Antigravity settings must be a JSON object"))?;
    let forwarder = injected["statusLine"]["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing Antigravity status observer"))?;
    object.insert(
        "statusLine".into(),
        status_line_settings(&original["statusLine"], forwarder),
    );
    // Settings is a copied entry. Replace it atomically without writing
    // through to the user's real configuration.
    super::synth_home::write_if_changed(&dir.join("settings.json"), &settings.to_string())?;
    if source.is_none() {
        super::synth_home::write_if_changed(&dir.join(SNAPSHOT), ABSENT_SOURCE)?;
    }
    Ok(())
}

/// Feed the same JSON to our observer and the user's command. Keep its stdout,
/// padding and stacking behavior; absent/disabled custom output leaves the
/// built-in status line visible. The real config is always the source, so
/// repeated launches cannot nest our wrapper.
fn status_line_settings(original: &serde_json::Value, forwarder: &str) -> serde_json::Value {
    let command = original
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .filter(|_| original["enabled"] != false);
    let mut settings = original.as_object().cloned().unwrap_or_default();
    settings.insert("type".into(), "command".into());
    settings.insert("enabled".into(), true.into());
    if command.is_none() {
        settings.insert("stack_with_default".into(), true.into());
    }
    let output = command
        .map(|cmd| {
            format!(
                "printf '%s' \"$cm_agy_status_payload\" | /bin/sh -c {}",
                shell_quote(cmd)
            )
        })
        .unwrap_or_else(|| ":".into());
    settings.insert("command".into(), format!(
        "cm_agy_status_payload=$(cat); printf '%s' \"$cm_agy_status_payload\" | {forwarder} >/dev/null; {output}"
    ).into());
    serde_json::Value::Object(settings)
}

/// Put our hook bundle into the user's `hooks.json` under [`HOOK_NAME`],
/// replacing any previous copy of ours and leaving every other key alone.
///
/// Best-effort in both directions, like Codex's profile generation and Grok's
/// config merge: if either side fails to parse as a JSON object we ship ours
/// alone, because a session with no status is worth less than a session whose
/// unrelated hooks are missing for one launch. `ours` failing to parse cannot
/// happen — [`build_hooks_settings`] built it — and yields their file untouched
/// if it somehow does.
fn merge_hooks(theirs: &str, ours: &str) -> String {
    let Ok(ours_val) = serde_json::from_str::<serde_json::Value>(ours) else {
        return theirs.to_string();
    };
    let Some(ours_obj) = ours_val.as_object() else {
        return theirs.to_string();
    };
    let mut merged = serde_json::from_str::<serde_json::Value>(theirs)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (k, v) in ours_obj {
        merged.insert(k.clone(), v.clone());
    }
    serde_json::Value::Object(merged).to_string()
}

/// Build launch settings containing hooks and the status observer. The hook
/// bundle shape is `{<name>: {<Event>: […]}}`,
/// where the tool events wrap their handlers in a `{matcher, hooks}` group and
/// the rest list handlers directly — a split Antigravity's own doc calls
/// "Grouped" vs "Flat".
///
/// The 10s timeout replaces a 30s default on a path that blocks the agent loop.
/// A socket write does not take ten seconds, and a launcher that is gone fails
/// its connect immediately, so this only ever bounds a pathological case.
///
/// Like Codex's and Reasonix's, the command carries no per-session data — the
/// socket arrives via `$CAPTAIN_MIAO_SOCK` — because one file serves every
/// session. It ends in the stdout contract the module doc explains.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    build_settings_for(&exe)
}

fn build_settings_for(exe: &Path) -> String {
    let exe_q = shell_quote(&exe.to_string_lossy());

    let command = |event: HookEvent| -> String {
        format!(
            "{exe_q} hook --agent antigravity {} >/dev/null; echo '{{}}'",
            event.as_kebab()
        )
    };
    let flat = |event: HookEvent| -> serde_json::Value {
        serde_json::json!([{ "type": "command", "command": command(event), "timeout": 10 }])
    };
    let grouped = |event: HookEvent| -> serde_json::Value {
        serde_json::json!([{
            "matcher": "*",
            "hooks": [{ "type": "command", "command": command(event), "timeout": 10 }],
        }])
    };

    serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("{exe_q} hook --agent antigravity status-update"),
        },
        "hooks": {HOOK_NAME: {
            "SessionStart": flat(HookEvent::SessionStart),
            "PreInvocation": flat(HookEvent::PromptSubmit),
            "PostToolUse":   grouped(HookEvent::PostToolUse),
            "Stop":          flat(HookEvent::Stop),
        }}
    })
    .to_string()
}

// =============================================================================
// Resume picker
// =============================================================================

/// One line of `history.jsonl` — Antigravity's prompt-recall log, and the only
/// record on disk that ties a conversation to the directory it ran in.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryLine {
    #[serde(default)]
    conversation_id: String,
    #[serde(default)]
    workspace: String,
}

/// Every conversation we can name a working directory for, newest first.
///
/// The bound worth stating: **a conversation reaches the picker only if some
/// record ties it to a directory.** Antigravity stores conversations flat under
/// `brain/<id>/` with no project nesting and no cwd inside the transcript, so
/// the cwd comes from `history.jsonl` (which records `conversationId` alongside
/// `workspace`, but not for every line) or from `cache/last_conversations.json`
/// (which holds the most recent conversation per workspace). A conversation
/// missing from both is skipped rather than guessed at — a resume aimed at the
/// wrong directory is worse than a resume the picker didn't offer.
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    list_resumable_cached(limit, &mut crate::resume::AgentCache::default())
}

pub(crate) fn list_resumable_cached(
    limit: usize,
    cache: &mut crate::resume::AgentCache,
) -> Result<Vec<ResumeCandidate>> {
    let root = cli_dir().ok_or_else(|| anyhow::anyhow!("no antigravity state dir"))?;
    Ok(list_resumable_in_cached(&root, limit, cache))
}

/// The scan itself, split from `$HOME` resolution so a test can point it at a
/// fixture tree without touching the environment.
#[cfg(test)]
fn list_resumable_in(root: &Path, limit: usize) -> Vec<ResumeCandidate> {
    list_resumable_in_cached(root, limit, &mut crate::resume::AgentCache::default())
}

fn list_resumable_in_cached(
    root: &Path,
    limit: usize,
    cache: &mut crate::resume::AgentCache,
) -> Vec<ResumeCandidate> {
    let history = root.join("history.jsonl");
    let last = root.join("cache/last_conversations.json");
    let key = if history.exists() { &history } else { &last };
    let cwd_of = cache
        .index
        .get(key, &[&history, &last], || {
            let mut cwd_of: HashMap<String, String> = HashMap::new();
            // `last_conversations.json` first, so a history line naming the same id wins
            // — it is the per-prompt record, where this is a per-workspace cache.
            if let Ok(body) = std::fs::read_to_string(root.join("cache/last_conversations.json"))
                && let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&body)
            {
                for (workspace, id) in map {
                    cwd_of.insert(id, workspace);
                }
            }
            if let Ok(body) = std::fs::read_to_string(root.join("history.jsonl")) {
                for line in body.lines() {
                    let Ok(entry) = serde_json::from_str::<HistoryLine>(line) else {
                        continue;
                    };
                    if entry.conversation_id.is_empty() || entry.workspace.is_empty() {
                        continue;
                    }
                    cwd_of.insert(entry.conversation_id, entry.workspace);
                }
            }
            Some(cwd_of)
        })
        .unwrap_or_default();

    // Stat before read, so the cost is one stat per known conversation plus
    // `limit` transcript reads rather than a read per conversation that ever
    // existed.
    let mut found = Vec::new();
    for (id, cwd) in cwd_of {
        let path = transcript_path(root, &id);
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        found.push(((id, cwd, path), mtime));
    }

    let found = common::newest_first(found, limit);
    let ids: Vec<_> = found.iter().map(|((id, _, _), _)| id.clone()).collect();
    let titles = metadata::read_titles_at(root, &ids);
    found
        .into_iter()
        .filter_map(|((session_id, cwd, path), mtime)| {
            let mut candidate = cache.candidates.get(&path, &[], || {
                Some(ResumeCandidate {
                    agent: crate::agent::AgentControl::Antigravity,
                    custom_title: None,
                    session_id: session_id.clone(),
                    cwd: cwd.clone(),
                    first_prompt: first_prompt(&path),
                    git_branch: None,
                    mtime,
                })
            })?;
            candidate.custom_title = titles.get(&session_id).cloned();
            candidate.cwd = cwd;
            Some(candidate)
        })
        .collect()
}

/// The conversation's opening request, read from the first `USER_INPUT` step.
///
/// The step's `content` is not the prompt: Antigravity wraps it in
/// `<USER_REQUEST>` and appends `<ADDITIONAL_METADATA>` / `<USER_SETTINGS_CHANGE>`
/// blocks of its own, so the tags are what to read between. A step without them
/// is a synthetic one (`CONVERSATION_HISTORY`, `CHECKPOINT`) and yields nothing.
fn first_prompt(transcript: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(std::fs::File::open(transcript).ok()?);
    reader
        .split(b'\n')
        .map_while(Result::ok)
        .find_map(|line| metadata::user_prompt(&line))
}

// =============================================================================
// Hook payload (stdin from Antigravity → normalized HookMessage)
// =============================================================================

/// The tool call `PostToolUse` reports. Undocumented — `hooks.md` shows only
/// `stepIdx` and `error` on that event — but present in every captured payload,
/// carrying the same `{name, args}` the `PreToolUse` contract documents.
#[derive(Deserialize)]
struct ToolCall {
    #[serde(default)]
    name: String,
}

/// Antigravity's hook payload, reduced to the fields we act on. **camelCase**,
/// which `hooks.md` states outright is protojson encoding rather than a style
/// choice — so a field that arrives absent is the proto default, not a
/// malformed payload, and every field here tolerates being missing.
///
/// Left out and real: `stepIdx`, `invocationNum`, `initialNumSteps`,
/// `executionNum`, `artifactDirectoryPath`, `terminationReason` (see the module
/// doc for why nothing branches on it), `fullyIdle`, and `toolCall.args`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookPayload {
    conversation_id: Option<String>,
    model_name: Option<String>,
    /// Every directory in the session's workspace. Populated in an interactive
    /// session and **empty under `-p`**, which is why nothing downstream may
    /// treat it as authoritative for the row's cwd — the launcher owns that.
    #[serde(default)]
    workspace_paths: Vec<String>,
    /// The tool's or turn's failure text. Present-and-empty on success, which is
    /// what [`normalize_event`] keys on.
    #[serde(default)]
    error: String,
    tool_call: Option<ToolCall>,
}

/// The separate status-line schema, captured from agy 1.3.5. Its input plus
/// output totals match `used_percentage`; `current_usage` is only the last
/// model call and does not describe the whole context window.
#[derive(Deserialize)]
struct StatusPayload {
    #[serde(default)]
    agent_state: String,
    #[serde(default)]
    tool_confirmation_pending: bool,
    conversation_id: Option<String>,
    model: Option<StatusModel>,
    context_window: Option<StatusContext>,
}

#[derive(Deserialize)]
struct StatusModel {
    id: Option<String>,
}

#[derive(Deserialize)]
struct StatusContext {
    total_input_tokens: Option<u64>,
    total_output_tokens: Option<u64>,
    context_window_size: Option<u64>,
}

fn parse_status_payload(stdin: &str) -> Result<HookMessage> {
    let payload: StatusPayload =
        serde_json::from_str(stdin).context("parsing Antigravity status-line JSON")?;
    Ok(HookMessage {
        event: HookEvent::StatusUpdate,
        transcript_path: live_transcript_path(payload.conversation_id.as_deref()),
        session_id: payload.conversation_id.filter(|s| !s.trim().is_empty()),
        model: payload.model.and_then(|m| m.id),
        context_tokens: payload
            .context_window
            .filter(|ctx| ctx.context_window_size.is_some_and(|size| size > 0))
            .and_then(|ctx| {
                ctx.total_input_tokens
                    .map(|input| input.saturating_add(ctx.total_output_tokens.unwrap_or(0)))
            }),
        tool_name: None,
        message: None,
        cwd: None,
        prompt: None,
        session_title: None,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

/// Normalize one Antigravity hook payload. Its event names differ from ours and
/// two of our variants have no event of their own, so both are synthesized here
/// from the payload rather than mapped later.
pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    if event == HookEvent::StatusUpdate {
        return parse_status_payload(stdin);
    }
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse antigravity hook JSON from stdin")?;
    Ok(HookMessage {
        event: normalize_event(event, &payload),
        transcript_path: live_transcript_path(payload.conversation_id.as_deref()),
        session_id: payload.conversation_id,
        tool_name: payload
            .tool_call
            .map(|t| t.name)
            .filter(|n| !n.trim().is_empty()),
        message: Some(payload.error).filter(|e| !e.trim().is_empty()),
        cwd: payload.workspace_paths.into_iter().next(),
        // No hook of Antigravity's carries the prompt text: `PreInvocation`
        // describes the model call, not what asked for it.
        prompt: None,
        session_title: None,
        // Usage arrives through the status-line command, not lifecycle hooks.
        context_tokens: None,
        model: payload.model_name,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

/// Antigravity reports a failed tool and a failed turn through the *same* events
/// as their successes, with `error` set — there is no `PostToolUseFailure` or
/// `StopFailure` to register. The shared dispatcher retains turn failures as
/// `last_error`; tool failures keep the turn active while the agent recovers.
fn normalize_event(event: HookEvent, payload: &HookPayload) -> HookEvent {
    if payload.error.trim().is_empty() {
        return event;
    }
    match event {
        HookEvent::PostToolUse => HookEvent::PostToolUseFailure,
        HookEvent::Stop => HookEvent::StopFailure,
        other => other,
    }
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Apply the observed activity before adopting snapshot facts. Ordinary
/// lifecycle hooks retain the shared dispatcher's semantics.
pub fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    if msg.event == HookEvent::StatusUpdate
        && let Some(payload) = msg
            .raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<StatusPayload>(raw).ok())
    {
        let compaction = msg
            .transcript_path
            .as_deref()
            .and_then(|path| metadata::read_compaction(Path::new(path)))
            .unwrap_or_default();
        if compaction.usage_stale {
            // This snapshot repeats usage from before the checkpoint. None
            // means unknown, not zero; the next measured response restores it.
            msg.context_tokens = None;
            state.context_tokens = None;
        }
        if let Some(capacity) = payload
            .context_window
            .and_then(|window| window.context_window_size)
            .filter(|capacity| *capacity > 0)
        {
            state.context_window = Some(capacity);
        }
        // A snapshot is not a new prompt: preserve errors and prompt/tool
        // facts. Coarse busy states must not erase a more specific hook state.
        match (
            payload.agent_state.as_str(),
            payload.tool_confirmation_pending,
        ) {
            (_, true) => state.status = SessionStatus::WaitingForApproval,
            ("idle", false) => {
                state.status = if compaction.completed {
                    SessionStatus::Compacted
                } else {
                    SessionStatus::Idle
                };
                state.last_tool = None;
            }
            ("working" | "thinking" | "tool_use", false) if compaction.requested => {
                state.status = SessionStatus::Compacting;
            }
            ("working" | "thinking" | "tool_use", false)
                if matches!(
                    state.status,
                    SessionStatus::Starting
                        | SessionStatus::Idle
                        | SessionStatus::Active
                        | SessionStatus::Compacting
                        | SessionStatus::Compacted
                        | SessionStatus::BackgroundActive
                        | SessionStatus::WaitingForApproval
                ) =>
            {
                state.status = SessionStatus::Active;
            }
            _ => {}
        }
    }
    common::dispatch_default(state, msg)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod status_tests;

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::SessionStatus;

    /// An `antigravity-cli/` state tree: whichever of the two cwd records and
    /// transcripts the case needs.
    fn state_fixture(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cm-agy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, body) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        root
    }

    /// A transcript's opening step, in the shape `agy` writes it.
    fn user_input(prompt: &str) -> String {
        format!(
            r#"{{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\n{prompt}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-01-01T00:00:00Z.\n</ADDITIONAL_METADATA>"}}"#
        )
    }

    fn transcript_at(id: &str) -> String {
        format!("brain/{id}/.system_generated/logs/transcript.jsonl")
    }

    /// The generated config registers exactly the four events the module doc
    /// claims, in the two structural shapes Antigravity requires: `matcher` +
    /// `hooks` for the tool event, a flat handler list for the others. Getting
    /// the shape wrong is silent — the hook simply never fires.
    #[test]
    fn the_hook_config_uses_the_shape_each_event_requires() {
        let json: serde_json::Value =
            serde_json::from_str(&build_hooks_settings("/tmp/sock")).unwrap();
        let ours = &json["hooks"][HOOK_NAME];

        assert!(ours["SessionStart"][0]["command"].is_string());
        assert!(ours["PreInvocation"][0]["command"].is_string());
        assert!(ours["Stop"][0]["command"].is_string());
        assert!(
            ours["PreInvocation"][0]["matcher"].is_null(),
            "a flat event takes no matcher"
        );

        assert_eq!(ours["PostToolUse"][0]["matcher"], "*");
        assert!(ours["PostToolUse"][0]["hooks"][0]["command"].is_string());

        assert!(
            ours["PreToolUse"].is_null(),
            "PreToolUse is a gate, not an observer: registering it denies every \
             tool call the hook does not explicitly allow"
        );
        assert!(
            ours["PostInvocation"].is_null(),
            "PostInvocation says nothing the other hooks don't, on a path that \
             blocks the agent loop"
        );
    }

    /// Antigravity parses a hook's stdout as JSON and `miao hook` writes none,
    /// so every command has to end by emitting an object of its own.
    #[test]
    fn every_hook_command_emits_the_object_antigravity_expects() {
        let json: serde_json::Value =
            serde_json::from_str(&build_hooks_settings("/tmp/sock")).unwrap();
        let commands: Vec<String> = json["hooks"][HOOK_NAME]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|handlers| {
                handlers.as_array().unwrap().iter().flat_map(|h| {
                    match h.get("hooks").and_then(|g| g.as_array()) {
                        Some(group) => group.iter().collect::<Vec<_>>(),
                        None => vec![h],
                    }
                })
            })
            .map(|h| h["command"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(commands.len(), 4, "one command per registered event");
        for command in commands {
            assert!(
                command.ends_with(">/dev/null; echo '{}'"),
                "{command} does not end in the stdout contract"
            );
        }
    }

    /// The user's own hooks survive a captain-miao launch: ours is one more key
    /// in their file, not a replacement for it. The reverse also holds — a
    /// second launch replaces our stale bundle rather than accumulating copies.
    #[test]
    fn our_bundle_joins_the_users_hooks_instead_of_replacing_them() {
        let theirs = r#"{"lint-checker":{"PostToolUse":[{"matcher":"run_command","hooks":[]}]}}"#;
        let merged: serde_json::Value = serde_json::from_str(&merge_hooks(
            theirs,
            &serde_json::from_str::<serde_json::Value>(&build_hooks_settings("/tmp/sock")).unwrap()
                ["hooks"]
                .to_string(),
        ))
        .unwrap();

        assert_eq!(
            merged["lint-checker"]["PostToolUse"][0]["matcher"],
            "run_command"
        );
        assert!(merged[HOOK_NAME]["Stop"][0]["command"].is_string());

        // Re-merging our own output leaves one copy of each.
        let again: serde_json::Value =
            serde_json::from_str(
                &merge_hooks(
                    &merged.to_string(),
                    &serde_json::from_str::<serde_json::Value>(&build_hooks_settings("/x"))
                        .unwrap()["hooks"]
                        .to_string(),
                ),
            )
            .unwrap();
        assert_eq!(again.as_object().unwrap().len(), 2);
    }

    /// A file we can't parse must not cost the session its status: ours ships
    /// alone rather than the launch failing or the hooks going missing.
    #[test]
    fn an_unparseable_hooks_file_still_leaves_us_registered() {
        let merged: serde_json::Value = serde_json::from_str(&merge_hooks(
            "{not json",
            &serde_json::from_str::<serde_json::Value>(&build_hooks_settings("/tmp/sock")).unwrap()
                ["hooks"]
                .to_string(),
        ))
        .unwrap();
        assert!(merged[HOOK_NAME]["PreInvocation"][0]["command"].is_string());
    }

    /// The two cwd records feed one picker, and the per-prompt one wins where
    /// they disagree — `last_conversations.json` is a per-workspace cache that
    /// a later session in another directory can leave stale.
    #[test]
    fn the_picker_takes_its_cwd_from_whichever_record_names_one() {
        let root = state_fixture(
            "picker",
            &[
                (
                    "cache/last_conversations.json",
                    r#"{"/home/miao/old":"aaa"}"#,
                ),
                (
                    "history.jsonl",
                    "{\"display\":\"hi\",\"timestamp\":1,\"workspace\":\"/home/miao/new\",\
                     \"conversationId\":\"aaa\"}\n\
                     {\"display\":\"x\",\"timestamp\":2,\"workspace\":\"/home/miao/two\",\
                     \"conversationId\":\"bbb\"}\n",
                ),
                (&transcript_at("aaa"), &user_input("first thing")),
                (&transcript_at("bbb"), &user_input("second thing")),
            ],
        );

        let got = list_resumable_in(&root, 10);
        assert_eq!(got.len(), 2);
        let aaa = got.iter().find(|c| c.session_id == "aaa").unwrap();
        assert_eq!(
            aaa.cwd, "/home/miao/new",
            "the per-prompt record outranks the per-workspace cache"
        );
        assert_eq!(aaa.first_prompt.as_deref(), Some("first thing"));
        assert!(got.iter().all(|c| c.agent == AgentControl::Antigravity));
    }

    /// A conversation nothing ties to a directory is skipped, not guessed at:
    /// resuming into the wrong repo is worse than not offering the row.
    #[test]
    fn a_conversation_with_no_recorded_directory_is_skipped() {
        let root = state_fixture(
            "nocwd",
            &[
                (
                    "history.jsonl",
                    "{\"display\":\"hi\",\"timestamp\":1,\"workspace\":\"/home/miao/w\"}\n",
                ),
                (&transcript_at("ccc"), &user_input("orphan")),
            ],
        );
        assert!(list_resumable_in(&root, 10).is_empty());
    }

    /// A conversation whose transcript hasn't been written yet is skipped too —
    /// there is nothing to date it by and nothing to title it with.
    #[test]
    fn a_conversation_with_no_transcript_is_skipped() {
        let root = state_fixture(
            "notranscript",
            &[("cache/last_conversations.json", r#"{"/home/miao/w":"ddd"}"#)],
        );
        assert!(list_resumable_in(&root, 10).is_empty());
    }

    /// The prompt is what's between the tags — not the metadata blocks
    /// Antigravity appends to the same string.
    #[test]
    fn the_title_is_the_request_without_the_metadata_antigravity_appends() {
        let root = state_fixture(
            "title",
            &[
                ("cache/last_conversations.json", r#"{"/home/miao/w":"eee"}"#),
                (
                    &transcript_at("eee"),
                    &format!(
                        "{}\n{}\n",
                        r#"{"step_index":0,"source":"SYSTEM","type":"CONVERSATION_HISTORY"}"#,
                        user_input("fix   the\\nflaky test")
                    ),
                ),
            ],
        );
        let got = list_resumable_in(&root, 10);
        assert_eq!(got[0].first_prompt.as_deref(), Some("fix the flaky test"));
    }

    fn payload(event: HookEvent, body: &str) -> HookMessage {
        parse_hook_payload(event, body).expect("a captured payload parses")
    }

    fn feed(state: &mut LauncherState, msg: HookMessage) {
        dispatch_hook(state, msg);
    }

    /// A real `PostToolUse` payload, captured from `agy` 1.1.11. The fields that
    /// matter are the id (the row's identity and its resume handle), the model,
    /// and the tool name the shipped doc doesn't mention.
    #[test]
    fn a_captured_payload_yields_the_session_id_model_and_tool() {
        let msg = payload(
            HookEvent::PostToolUse,
            r#"{"artifactDirectoryPath":"/home/miao/.gemini/antigravity-cli/brain/65a4b172",
                "conversationId":"65a4b172-9aaa-41cf-b921-bb0bf9f1fe6c","error":"",
                "modelName":"gemini-3.6-flash-high","stepIdx":3,
                "toolCall":{"args":{"CommandLine":"ls -la"},"name":"run_command"},
                "transcriptPath":"/home/miao/.gemini/antigravity-cli/brain/65a4b172/t.jsonl",
                "workspacePaths":["/home/miao/work"]}"#,
        );
        assert_eq!(
            msg.session_id.as_deref(),
            Some("65a4b172-9aaa-41cf-b921-bb0bf9f1fe6c")
        );
        assert_eq!(msg.model.as_deref(), Some("gemini-3.6-flash-high"));
        assert_eq!(msg.tool_name.as_deref(), Some("run_command"));
        assert_eq!(
            msg.event,
            HookEvent::PostToolUse,
            "an empty error is success"
        );
        assert!(
            msg.transcript_path.as_deref().unwrap().ends_with(
                "brain/65a4b172-9aaa-41cf-b921-bb0bf9f1fe6c/.system_generated/logs/transcript.jsonl"
            ),
            "the CLI path is derived from the id"
        );
        assert!(msg.context_tokens.is_none());
    }

    /// A `Stop` payload, captured the same way. `terminationReason` carries a
    /// value the shipped doc doesn't list, which is exactly why nothing reads
    /// it: an empty `error` is a clean end however the turn was described.
    #[test]
    fn a_clean_stop_settles_the_row_whatever_it_calls_its_reason() {
        let mut state = LauncherState::for_test(AgentControl::Antigravity, SessionStatus::Active);
        let msg = payload(
            HookEvent::Stop,
            r#"{"conversationId":"65a4b172","error":"","executionNum":0,"fullyIdle":true,
                "modelName":"gemini-3.6-flash-high","terminationReason":"NO_TOOL_CALL",
                "workspacePaths":["/home/miao/work"]}"#,
        );
        assert_eq!(msg.event, HookEvent::Stop);
        feed(&mut state, msg);
        assert_eq!(state.status, SessionStatus::Idle);
    }

    /// Failure arrives on the success event with `error` set — there is no
    /// failure event to register — so the split has to happen here or the row
    /// never shows what went wrong.
    #[test]
    fn a_failure_is_the_same_event_with_error_set() {
        let tool = payload(
            HookEvent::PostToolUse,
            r#"{"conversationId":"a","error":"exit status 1","stepIdx":5}"#,
        );
        assert_eq!(tool.event, HookEvent::PostToolUseFailure);
        assert_eq!(tool.message.as_deref(), Some("exit status 1"));

        let stop = payload(
            HookEvent::Stop,
            r#"{"conversationId":"a","error":"context deadline exceeded",
                "terminationReason":"error"}"#,
        );
        assert_eq!(stop.event, HookEvent::StopFailure);

        // The model call itself has no failure counterpart, so it stays put.
        let pre = payload(
            HookEvent::PromptSubmit,
            r#"{"conversationId":"a","error":"whatever"}"#,
        );
        assert_eq!(pre.event, HookEvent::PromptSubmit);
    }

    /// Print mode sends an empty `workspacePaths`, and a proto default must
    /// decode as an absence rather than as an empty string the row could adopt.
    #[test]
    fn an_empty_workspace_list_is_no_cwd_at_all() {
        let msg = payload(
            HookEvent::PromptSubmit,
            r#"{"conversationId":"a","invocationNum":0,"workspacePaths":[]}"#,
        );
        assert!(msg.cwd.is_none());
        assert!(msg.tool_name.is_none());
    }
}
