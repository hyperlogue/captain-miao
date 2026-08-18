//! Antigravity CLI backend — Google's `agy`, the terminal agent it shipped as
//! Gemini CLI's successor. Owns every Antigravity-specific path, env var and
//! hook payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Antigravity`'s match arms.
//!
//! **Probe-verified against `agy` 1.1.11**, not read off a repo: every payload
//! quoted below was captured from a real session, and the two places the
//! shipped documentation is *wrong* are called out where they bite. The doc
//! itself travels with the binary — `~/.gemini/antigravity-cli/builtin/skills/
//! agy-customizations/docs/hooks.md` — so a later version's contract is
//! readable from an installed copy rather than from the web.
//!
//! # Five events, and the useful ones are three
//!
//! Antigravity's whole hook vocabulary is `PreToolUse`, `PostToolUse`,
//! `PreInvocation`, `PostInvocation`, `Stop`. We register three:
//!
//! - **`PreInvocation` → [`HookEvent::PromptSubmit`].** It fires before each
//!   model call, so it is the row's "working" edge. It is *not* once per user
//!   prompt — a turn that calls tools fires it again after each — but every arm
//!   it reaches assigns `Active` unconditionally, so repeating is free.
//! - **`PostToolUse` → [`HookEvent::PostToolUse`]**, with `"*"` as the matcher.
//! - **`Stop` → [`HookEvent::Stop`]**, the turn-end signal.
//!
//! `PostInvocation` is deliberately unregistered: it fires between the other
//! three and would say only what they already said, at the price of a
//! subprocess per model call on a hook path that **blocks the agent loop**
//! (`hooks.md`, "Current Limitations").
//!
//! Two of our events are synthesized rather than subscribed, from a payload
//! field instead of a second registration: a non-empty `error` turns
//! `PostToolUse` into [`HookEvent::PostToolUseFailure`] and `Stop` into
//! [`HookEvent::StopFailure`] ([`normalize_event`]). That is the same shape as
//! Codex's four synthesized events, and it is why
//! `AgentControl::forwarded_events` — which reads the generated config — lists
//! three where the dispatcher handles five.
//!
//! # `PreToolUse` is left alone on purpose, and it costs the approval state
//!
//! [`crate::state::SessionStatus::WaitingForApproval`] is **unreachable**, and
//! not for want of an event: Antigravity blocks on its own permission prompt
//! without firing any hook at all (probed — a session sitting on "Do you want
//! to proceed?" had emitted `PreInvocation` and nothing since).
//!
//! `PreToolUse` fires before that prompt, so it looks like the way in. It is
//! not, because it is a *gate*, not an observer: its `decision` field is
//! **required** and every legal value changes what the agent does — `allow`
//! silently disables the user's permission preset, `ask`/`force_ask` add
//! prompts they didn't ask for, `deny` breaks the session. There is no
//! "no opinion" value, and an output that omits the field fails closed: cmux
//! injected a `PreToolUse` hook returning `{}` and every tool call in the
//! session was denied with `error (invalid_args)` (manaflow-ai/cmux#5358).
//! Antigravity's own bundled plugin registers `PostInvocation`, `PostToolUse`,
//! `PreInvocation` and `Stop` — everything except `PreToolUse` — which is the
//! same conclusion reached from the other side.
//!
//! So a blocked Antigravity row reads as working, `capabilities().approval_gate`
//! is false, and the dashboard says so rather than implying the sweep was
//! exhaustive. The cost is real and it is the agent's, not ours; the day
//! Antigravity grows a notification hook, this is a one-line registration.
//!
//! # There is no session-start event, so a row waits at `Starting`
//!
//! Nothing fires when `agy` starts — the first hook of a session is the
//! `PreInvocation` of its first turn. A launched Antigravity row therefore sits
//! at `Starting` until the user submits something, which is also when its
//! `conversationId` first reaches us and the row becomes resumable.
//!
//! `ANTIGRAVITY_CONVERSATION_ID` in the binary's strings looks like the escape
//! hatch and isn't: setting it at launch is ignored (probed — `agy` minted its
//! own id anyway), because the variable is *exported* to the tools the agent
//! runs rather than read from the environment it was given.
//!
//! # Hooks can only be discovered from `$HOME`, so the whole home is synthetic
//!
//! There is no `--settings` equivalent and no config-dir environment variable;
//! `hooks.json` is read from `.agents/` in the workspace (the user's repo — not
//! ours to write) or `~/.gemini/config/` (their global config — the one thing
//! this project never writes). `~` there is the process's `$HOME`, which is the
//! lever, so a session runs under a synthetic home that mirrors the real one
//! ([`super::synth_home`]), three levels deep: `$HOME` owns `.gemini`, which
//! owns `config`, which owns `hooks.json`.
//!
//! That is one level wider than Codex's `$CODEX_HOME` and the extra width is
//! the cost worth knowing: **every tool the agent runs inherits the synthetic
//! `$HOME` too.** Everything in it resolves through a symlink to the real
//! home, so reads and writes to existing paths land where they always did; a
//! brand-new top-level dotfile created inside a session lands in the synthetic
//! home instead, and the next launch quarantines it as `.shadow-…` with a
//! warning rather than deleting it.
//!
//! The user's own `hooks.json` is **merged, not shadowed** ([`merge_hooks`]).
//! `hooks.json` is a map of named hook bundles, so ours goes in under
//! [`HOOK_NAME`] beside theirs and everything else in the file keeps working.
//! Their workspace-level `.agents/hooks.json` is untouched by construction.
//!
//! # The stdout contract is why our commands don't end at the event name
//!
//! Antigravity reads a hook's stdout as JSON, and `miao hook` prints nothing.
//! Every generated command therefore ends `>/dev/null; echo '{}'`: the
//! forwarder's own output is discarded, the object the agent expects is
//! emitted, and a launcher that has gone away can neither wedge a turn nor
//! print into the session. Its stderr is left alone so a real failure is still
//! visible. (This suffix is what made `AgentControl::mentions_event` stop
//! requiring a quote right after the event name.)
//!
//! # Tokens, titles and the transcript
//!
//! `capabilities().context_tokens` is false: no hook payload carries a token
//! count and nothing on disk records one — the per-conversation store is
//! `conversations/<id>.db`, a SQLite file of protobuf blobs with no published
//! schema. The model *is* on every payload (`modelName`, e.g.
//! `gemini-3.6-flash-high`), so that column works.
//!
//! `transcriptPath` is on every payload too, and is deliberately **not**
//! returned in [`HookMessage::transcript_path`]: that field is what the
//! launcher gates its entire transcript pipeline on, and there is nothing in
//! the file for it to fold yet. [`list_resumable`] reads the same transcripts
//! directly, which keeps the one thing we do want out of a per-event path.
//!
//! # An interrupted turn keeps reading as working
//!
//! Esc ends the turn without firing `Stop` — probed, and the row sat at
//! `Active` indefinitely while the session was back at its prompt. The
//! transcript is no help either: the cancelled step lands as an ordinary
//! `PLANNER_RESPONSE` with `status: "DONE"` and a `thinking` field, carrying no
//! sentinel for [`crate::agent::AgentControl::scan_transcript_signals`] to find
//! the way Codex's rollout carries one.
//!
//! So the row stays `Active` until the next prompt — the Grok standing, and the
//! limit most likely to be felt day to day. Nothing here guesses at it from
//! timing or from an empty step: a status that is *usually* right costs more
//! than one that is late, because the dashboard exists to be trusted at a
//! glance.
//!
//! What a probe still has to settle:
//! - **whether `Stop`'s `fullyIdle: false` is worth a background tier.** It is
//!   documented as "all background tasks are done" and `run_command` really
//!   does background long commands (`WaitMsBeforeAsync` is in every tool call),
//!   but nothing enumerates those tasks, so there is no tier to put a row in
//!   yet.
//! - **whether `terminationReason` has a stable vocabulary.** The shipped doc
//!   says `model_stop` / `max_steps_exceeded` / `error`; the binary emits
//!   `NO_TOOL_CALL`. Nothing here branches on it for exactly that reason —
//!   failure is decided by `error` being non-empty.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::shell_quote;
use super::synth_home::SynthHome;
use crate::agent::ResumeCandidate;
use crate::state::{HookEvent, HookMessage, LauncherState};

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
/// The path is hardcoded relative to `$HOME` in the binary (no `GEMINI_HOME`,
/// no `XDG` override for it), which is what forces the synthetic home.
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

/// The global customization root: `hooks.json`, `mcp_config.json`, `config.json`,
/// `projects/`. This is the directory the synthetic home shadows.
fn config_dir() -> Option<PathBuf> {
    gemini_dir().map(|g| g.join("config"))
}

/// A single shared synthetic `$HOME` for every Antigravity session, for the same
/// reason Reasonix's is shared: it is a symlink farm over the user's home, and
/// one stable copy is cheaper to build and to reason about than one per launch.
/// That sharing is also why the hook command can carry no per-session data.
fn synth_home() -> PathBuf {
    crate::state::state_dir().join("antigravity-home")
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

// =============================================================================
// Launcher: process spawn + synthetic $HOME
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    // The launcher already wrote our hooks.json contents to `settings_path`;
    // relocate them into the synthetic home, which is the only place
    // Antigravity looks for global hooks.
    let hooks_json =
        std::fs::read_to_string(settings_path).context("reading antigravity hook settings")?;
    let home = ensure_synth_home(&hooks_json)?;

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    cmd.env("HOME", &home);
    // The hook subprocess reads the launcher socket from here rather than from
    // an argv flag: the synthetic home is shared by every session, so its
    // hooks.json cannot carry a per-session path.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    // No positional for the working directory: `agy` takes the workspace from
    // the process cwd, which `common::agent_command` has already set. Its
    // `--add-dir` *adds* one rather than choosing it.
    cmd.args(extra_args.iter().cloned());
    Ok(cmd)
}

/// Create / refresh the synthetic home and return it. Three nested mirrors,
/// each one owning the single entry the next level down needs to be ours:
/// `$HOME` owns `.gemini`, `.gemini` owns `config`, `config` owns `hooks.json`.
/// Everything else at every level is a symlink to the real thing, so a session's
/// conversations, credentials and MCP config are the user's own.
///
/// Nothing is **pruned** at any level. These are state mirrors: a dangling link
/// is load-bearing, because the agent recreating that file writes *through* the
/// link into the real home and the two stay converged.
fn ensure_synth_home(hooks_json: &str) -> Result<PathBuf> {
    let home = SynthHome {
        dir: synth_home(),
        real: dirs::home_dir(),
        owned: &[".gemini"],
        copied: &[],
        // Nothing at this level is Antigravity's alone — every file it writes
        // lives under `.gemini`, which the next mirror down handles.
        adopted: &[],
        prune: false,
    };
    home.ensure()?;

    let gemini = SynthHome {
        dir: home.dir.join(".gemini"),
        real: gemini_dir(),
        owned: &["config"],
        copied: &[],
        // The agent's entire state tree — conversations, transcripts and the
        // OAuth token — and ours by nothing. On a machine where `agy` has never
        // run, `~/.gemini/antigravity-cli` does not exist, so the linking pass
        // cannot mirror it and a first session would strand all of that here.
        adopted: &["antigravity-cli"],
        prune: false,
    };
    gemini.ensure()?;

    let config = SynthHome {
        dir: gemini.dir.join("config"),
        real: config_dir(),
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
    let theirs = config_dir()
        .map(|d| d.join(HOOKS_FILE))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    config.write_owned(HOOKS_FILE, &merge_hooks(&theirs, hooks_json))?;
    Ok(home.dir)
}

/// Put our hook bundle into the user's `hooks.json` under [`HOOK_NAME`],
/// replacing any previous copy of ours and leaving every other key alone.
///
/// Best-effort in both directions, like Codex's trust seeding and Grok's config
/// merge: if either side fails to parse as a JSON object we ship ours alone,
/// because a session with no status is worth less than a session whose unrelated
/// hooks are missing for one launch. `ours` failing to parse cannot happen —
/// [`build_hooks_settings`] built it — and yields their file untouched if it
/// somehow does.
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

/// Build the Antigravity `hooks.json`. Its shape is `{<name>: {<Event>: […]}}`,
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
        HOOK_NAME: {
            "PreInvocation": flat(HookEvent::PromptSubmit),
            "PostToolUse":   grouped(HookEvent::PostToolUse),
            "Stop":          flat(HookEvent::Stop),
        }
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
    let root = cli_dir().ok_or_else(|| anyhow::anyhow!("no antigravity state dir"))?;
    Ok(list_resumable_in(&root, limit))
}

/// The scan itself, split from `$HOME` resolution so a test can point it at a
/// fixture tree without touching the environment.
fn list_resumable_in(root: &Path, limit: usize) -> Vec<ResumeCandidate> {
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

    common::newest_first(found, limit)
        .into_iter()
        .map(|((session_id, cwd, path), mtime)| ResumeCandidate {
            agent: crate::agent::AgentControl::Antigravity,
            session_id,
            cwd,
            first_prompt: first_prompt(&path),
            // Antigravity has no rename: `/usage` and friends are slash
            // commands, not titles, and nothing on disk holds a user-chosen one.
            custom_title: None,
            git_branch: None,
            mtime,
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
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";

    #[derive(Deserialize)]
    struct Step {
        #[serde(default)]
        r#type: String,
        #[serde(default)]
        content: String,
    }

    let body = std::fs::read_to_string(transcript).ok()?;
    for line in body.lines() {
        let Ok(step) = serde_json::from_str::<Step>(line) else {
            continue;
        };
        if step.r#type != "USER_INPUT" {
            continue;
        }
        let Some(start) = step.content.find(OPEN) else {
            continue;
        };
        let rest = &step.content[start + OPEN.len()..];
        let end = rest.find(CLOSE)?;
        let prompt = super::collapse_whitespace(&rest[..end]);
        return (!prompt.is_empty()).then_some(prompt);
    }
    None
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

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse antigravity hook JSON from stdin")?;
    Ok(HookMessage {
        event: normalize_event(event, &payload),
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
        // Neither the payload nor anything on disk carries a token total — see
        // the module doc.
        context_tokens: None,
        model: payload.model_name,
        // Deliberately dropped rather than absent: `transcriptPath` is on every
        // payload, and returning it would start the launcher's transcript
        // pipeline over a file we fold nothing out of yet.
        transcript_path: None,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

/// Antigravity reports a failed tool and a failed turn through the *same* events
/// as their successes, with `error` set — there is no `PostToolUseFailure` or
/// `StopFailure` to register. Splitting them here is what lets the shared
/// dispatcher surface the text as `last_error`, and it is the same synthesis
/// Codex's backend does for four of its events.
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

/// Antigravity departs from [`common::dispatch_default`] nowhere: each of the
/// three events it registers is one of ours under a different spelling, and the
/// two failure variants are synthesized in [`parse_hook_payload`] before they
/// get here. The wrapper stays so the seam keeps one callee per backend.
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

    /// The generated config registers exactly the three events the module doc
    /// claims, in the two structural shapes Antigravity requires: `matcher` +
    /// `hooks` for the tool event, a flat handler list for the others. Getting
    /// the shape wrong is silent — the hook simply never fires.
    #[test]
    fn the_hook_config_uses_the_shape_each_event_requires() {
        let json: serde_json::Value =
            serde_json::from_str(&build_hooks_settings("/tmp/sock")).unwrap();
        let ours = &json[HOOK_NAME];

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
            "PostInvocation says nothing the other three don't, on a path that \
             blocks the agent loop"
        );
    }

    /// Antigravity parses a hook's stdout as JSON and `miao hook` writes none,
    /// so every command has to end by emitting an object of its own.
    #[test]
    fn every_hook_command_emits_the_object_antigravity_expects() {
        let json: serde_json::Value =
            serde_json::from_str(&build_hooks_settings("/tmp/sock")).unwrap();
        let commands: Vec<String> = json[HOOK_NAME]
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

        assert_eq!(commands.len(), 3, "one command per registered event");
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
        let merged: serde_json::Value =
            serde_json::from_str(&merge_hooks(theirs, &build_hooks_settings("/tmp/sock"))).unwrap();

        assert_eq!(
            merged["lint-checker"]["PostToolUse"][0]["matcher"],
            "run_command"
        );
        assert!(merged[HOOK_NAME]["Stop"][0]["command"].is_string());

        // Re-merging our own output leaves one copy of each.
        let again: serde_json::Value = serde_json::from_str(&merge_hooks(
            &merged.to_string(),
            &build_hooks_settings("/x"),
        ))
        .unwrap();
        assert_eq!(again.as_object().unwrap().len(), 2);
    }

    /// A file we can't parse must not cost the session its status: ours ships
    /// alone rather than the launch failing or the hooks going missing.
    #[test]
    fn an_unparseable_hooks_file_still_leaves_us_registered() {
        let merged: serde_json::Value = serde_json::from_str(&merge_hooks(
            "{not json",
            &build_hooks_settings("/tmp/sock"),
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
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(dispatch_hook(state, msg));
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
            msg.transcript_path.is_none(),
            "the path is on the payload and deliberately not forwarded"
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
