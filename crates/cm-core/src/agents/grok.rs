//! Grok Build CLI backend. Owns every Grok-specific path, env var and hook
//! payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Grok`'s match arms.
//!
//! Written from `xai-org/grok-build` (`10-hooks.md`, `17-sessions.md`,
//! `crates/codegen/xai-grok-hooks/src/{event,matcher}.rs`) and checked against a
//! live **1.0.4** binary: the hook JSON schema, the camelCase envelope, and
//! `StopCancelled` are no longer guesses. Remaining limits are named at the
//! point they still bite.
//!
//! **Hooks live in the real `~/.grok/hooks/captain-miao.json`.** Interactive
//! `grok` has no per-invocation `--settings` (and `--plugin-dir` exists only on
//! `grok agent`), so the file is always-on: it also fires in grok sessions the
//! user starts outside captain-miao. That is fine — `miao hook` exits 0 when
//! `$CAPTAIN_MIAO_SOCK` is unset, so those spawns are a no-op rather than a
//! failed turn. Global `~/.grok/hooks/*.json` is always trusted
//! (`custom-hooks.md`), so there is no prompt and no hash to precompute.
//! Codex's equivalent is an owned profile selected with `--profile`; this is
//! the same owned-file-in-the-real-home idea, without a selector.
//!
//! **Approval is the lifecycle `Notification` / `permission_prompt` matcher**
//! in that same hooks file. There is no second site in `config.toml`.
//!
//! **What this module still does not do**, and why:
//!
//! - **No background-task tiers.** `Stop` carries `backgroundTasks` and
//!   `sessionCrons` — strictly better data than Claude's process-tree walk —
//!   but routing it to the dashboard needs a new `LauncherState` field, which
//!   is seam work and belongs in its own commit.
//!   [`crate::agent::AgentControl::bg_shells`] answers `None` until then.
//! - **The worktree name isn't shown on the row.** Grok keeps worktrees in
//!   `worktrees.db` rather than beside the repo; `summary.json`'s `head_branch`
//!   is what the resume picker can show today.
//!
//! Interrupt, prompt, tokens and the hook-file schema are settled as of 1.0.4:
//! `StopCancelled` is a first-class observe hook (Kimi's `Interrupt` standing),
//! `UserPromptSubmit` carries `prompt`, the envelope carries `transcriptPath`,
//! and `signals.json` persists `contextTokensUsed`. Unrecognized event names
//! are still skipped, which is why `StopCancelled` is free on an older grok.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::shell_quote;
use super::synth_home::atomic_write;
use crate::agent::{ResumeCandidate, TranscriptStats};
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "grok";

/// Our hook file inside `~/.grok/hooks/`. A whole file of our own rather than
/// a merged one, because `hooks/` is a directory of independent files that Grok
/// globs — nothing of the user's is shadowed by it.
const HOOKS_FILE: &str = "captain-miao.json";

// =============================================================================
// Filesystem locations
// =============================================================================

/// `$GROK_HOME` if set, else `~/.grok` (`17-sessions.md`).
fn grok_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("GROK_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    dirs::home_dir().map(|h| h.join(".grok"))
}

/// Where Grok keeps its sessions: `$GROK_HOME/sessions/<cwd-key>/<id>/`, each
/// holding `summary.json`, `chat_history.jsonl` and `updates.jsonl`.
///
/// `<cwd-key>` is an encoding of the session's working directory, and this
/// module never decodes it — Grok's own resolver doesn't either when it has only
/// an id (`resolve_local_session_any_cwd_in_root` walks every key), and the cwd
/// we want is inside `summary.json` anyway. So the key is a directory to iterate,
/// never a string to parse.
fn sessions_root() -> Option<PathBuf> {
    Some(grok_home()?.join("sessions"))
}

// =============================================================================
// Resume picker
// =============================================================================

/// Grok's `summary.json`, of which the fields we act on are named. Grok writes
/// more (`num_messages`, `parent_session_id`, `forked_at`, cwd-relocation
/// bookkeeping); everything unnamed is ignored rather than refused, so a Grok
/// that grows a field still parses.
#[derive(Deserialize, Default)]
struct SessionSummary {
    #[serde(default)]
    info: SummaryInfo,
    /// Longer recap of the session. Fallback title only when
    /// [`Self::generated_title`] is empty.
    #[serde(default)]
    session_summary: String,
    /// The session's display name: auto-generated, then overwritten by `/rename`.
    #[serde(default)]
    generated_title: String,
    #[serde(default)]
    current_model_id: String,
    /// Branch checked out when the session last saved. 1.0.4 writes this at the
    /// **top level**; older grok put it on [`SummaryInfo`]. Both are read.
    /// Grok's worktrees live in its own registry rather than beside the repo,
    /// so this is the only branch name the picker can show.
    #[serde(default)]
    head_branch: String,
}

impl SessionSummary {
    /// Prefer the short title Grok shows in `grok sessions`; fall back to the
    /// recap only when that is still empty (a brand-new session).
    fn title(&self) -> Option<String> {
        Some(self.generated_title.clone())
            .filter(|t| !t.trim().is_empty())
            .or_else(|| Some(self.session_summary.clone()).filter(|t| !t.trim().is_empty()))
    }

    /// 1.0.4's top-level field, then the older `info.head_branch` spelling.
    fn git_branch(&self) -> Option<String> {
        Some(self.head_branch.clone())
            .filter(|b| !b.trim().is_empty())
            .or_else(|| Some(self.info.head_branch.clone()).filter(|b| !b.trim().is_empty()))
    }
}

#[derive(Deserialize, Default)]
struct SummaryInfo {
    /// The session's authoritative working directory. Grok tracks moves through
    /// a generation counter beside it; this is always the current one.
    #[serde(default)]
    cwd: String,
    /// Pre-1.0.4 location of [`SessionSummary::head_branch`].
    #[serde(default)]
    head_branch: String,
}

/// Every session under `$GROK_HOME/sessions/`, newest first.
///
/// A directory counts as a session exactly when it holds a `summary.json` —
/// which is Grok's own test (`is_persisted_session_dir`), and the reason a
/// half-written or salvaged directory never becomes a picker row. The session
/// **id is the directory's name**, not a field: that is how Grok resolves one,
/// so it cannot disagree with the store the way a copied id inside the file
/// could.
///
/// The picker does not fold a token count: that lives on the running row via
/// `signals.json`, not on a resume candidate.
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    let root = sessions_root().ok_or_else(|| anyhow::anyhow!("no grok home"))?;
    Ok(list_resumable_in(&root, limit))
}

/// The scan itself, split from `$GROK_HOME` resolution so a test can point it
/// at a fixture tree without touching the environment.
fn list_resumable_in(root: &Path, limit: usize) -> Vec<ResumeCandidate> {
    let mut found = Vec::new();
    for cwd_key in common::read_subdirs(root) {
        for session_dir in common::read_subdirs(&cwd_key) {
            let summary = session_dir.join("summary.json");
            let Ok(mtime) = std::fs::metadata(&summary).and_then(|m| m.modified()) else {
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
        let Ok(body) = std::fs::read_to_string(dir.join("summary.json")) else {
            continue;
        };
        let Ok(summary) = serde_json::from_str::<SessionSummary>(&body) else {
            continue;
        };
        if summary.info.cwd.trim().is_empty() {
            continue;
        }
        let custom_title = summary.title();
        let git_branch = summary.git_branch();
        out.push(ResumeCandidate {
            agent: crate::agent::AgentControl::Grok,
            session_id: session_id.to_string(),
            cwd: summary.info.cwd,
            first_prompt: None,
            custom_title,
            git_branch,
            mtime,
        });
    }
    out
}

// =============================================================================
// Launcher: process spawn + real ~/.grok hooks file
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    let hooks_json =
        std::fs::read_to_string(settings_path).context("reading grok hook settings")?;
    install_hooks_file(&hooks_json)?;

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    // Shared hooks file, so the socket cannot ride argv. Sessions the user
    // starts outside captain-miao have this unset; `miao hook` then exits 0.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    // Only what the launcher forwarded (`--resume <id>`, `--worktree=<name>`).
    // **No cwd positional**: nothing in the sources says `grok` takes a directory
    // argument, and its optional positional is the kind a bare `--worktree` is
    // documented to swallow (`06-worktrees.md`), so the working directory is set
    // on the process and nowhere else.
    cmd.args(extra_args);
    Ok(cmd)
}

/// Write `$GROK_HOME/hooks/captain-miao.json` (creating the directory). The
/// file is rewritten only when its contents would change, so concurrent
/// launches never race a half-written hook file.
fn install_hooks_file(contents: &str) -> Result<()> {
    let home = grok_home().ok_or_else(|| anyhow::anyhow!("no grok home"))?;
    let dir = home.join("hooks");
    crate::state::create_dir_all_private(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(HOOKS_FILE);
    let unchanged = std::fs::read_to_string(&path)
        .map(|cur| cur == contents)
        .unwrap_or(false);
    if !unchanged {
        atomic_write(&path, contents.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Build the contents of `$GROK_HOME/hooks/captain-miao.json`.
///
/// The schema is Claude's — `{"hooks": {<Event>: [{matcher, hooks: [{type,
/// command}]}]}}` — which is what Grok loads from `~/.claude/settings.json` and
/// from `~/.grok/hooks/*.json` (`xai-grok-hooks`, 1.0.4). Unrecognized event
/// names are skipped, so a name an older grok lacks is inert.
///
/// **Which events are registered**, and as what:
/// - `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `StopFailure`,
///   `SessionStart`, `UserPromptSubmit`, `PreCompact`, `PostCompact` forward
///   under their own names.
/// - **`StopCancelled` → `Stop`.** 1.0.4's observe hook for an interrupt,
///   declined permission, max-turns or no-progress bail-out. A turn the user
///   stopped is over, not failed — Kimi's `Interrupt` standing. The matcher is
///   tested against `reason`; omitted, it fires for every cancel.
/// - **`Notification` / `permission_prompt` → `PermissionRequest`.** The
///   lifecycle hook that fires while a permission UI is waiting.
/// - **`Notification` / `idle_prompt` → `Stop`.** Grok's documented backstop
///   for turns that report none of Stop / StopFailure / StopCancelled (bash
///   mode, rewind, a superseded report). Delayed ~1 minute; cancelled if the
///   next prompt arrives first.
/// - `PermissionDenied` fires *after* a refusal, when there is no state of
///   ours left to move. `Elicitation`, `ElicitationResult` and `CwdChanged`
///   are Claude affordances Grok does not emit.
///
/// **No matcher on the match-all events.** Grok treats an omitted matcher as
/// fire-all (`matcher_allows`); `"*"` also works (special-cased, not compiled
/// as regex) but is the form that silently disarms Kimi, so we spell absence.
///
/// **`Stop` carries an explicit `timeout` of 5 seconds.** It is the one event
/// where the default is **600s** rather than 5 (Stop gates commonly run test
/// suites), so a hung socket write would hold the user's turn end for ten
/// minutes. The matching hazard — Grok's `Stop` is *blocking*, and a hook that
/// exits **2** blocks the stop and feeds stderr back to the model as a new user
/// message, capped at 8 continuations — needs no guard here beyond saying why:
/// `miao hook` writes nothing to stdout and can only exit 0 or 1 (`hooks.rs`),
/// so it can neither print a `decision` nor reach the blocking status. Anything
/// that changes those two properties has to re-read this paragraph.
///
/// Like Codex's and Reasonix's, the command carries no per-session data — the
/// socket arrives via `$CAPTAIN_MIAO_SOCK` — because one file serves every
/// session.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    let exe_q = shell_quote(&exe.to_string_lossy());

    let group =
        |forwarded: HookEvent, matcher: Option<&str>, timeout: Option<u64>| -> serde_json::Value {
            let mut hook = serde_json::json!({
                "type": "command",
                "command": format!("{exe_q} hook --agent grok {}", forwarded.as_kebab()),
            });
            if let Some(timeout) = timeout {
                hook["timeout"] = serde_json::json!(timeout);
            }
            let mut group = serde_json::json!({ "hooks": [hook] });
            if let Some(matcher) = matcher {
                group["matcher"] = serde_json::json!(matcher);
            }
            group
        };
    let hook =
        |event: HookEvent| -> serde_json::Value { serde_json::json!([group(event, None, None)]) };

    serde_json::json!({
        "hooks": {
            "SessionStart":     hook(HookEvent::SessionStart),
            "UserPromptSubmit": hook(HookEvent::PromptSubmit),
            "PreToolUse":       hook(HookEvent::PreToolUse),
            "PostToolUse":      hook(HookEvent::PostToolUse),
            "PostToolUseFailure": hook(HookEvent::PostToolUseFailure),
            "Stop":             serde_json::json!([group(HookEvent::Stop, None, Some(5))]),
            "StopCancelled":    hook(HookEvent::Stop),
            "StopFailure":      hook(HookEvent::StopFailure),
            "PreCompact":       hook(HookEvent::PreCompact),
            "PostCompact":      hook(HookEvent::PostCompact),
            "Notification": serde_json::json!([
                group(HookEvent::PermissionRequest, Some("permission_prompt"), None),
                group(HookEvent::Stop, Some("idle_prompt"), None),
            ]),
        }
    })
    .to_string()
}

// =============================================================================
// Hook payload (stdin from Grok → normalized HookMessage)
// =============================================================================

/// Grok's native hook payload, reduced to the fields we act on.
///
/// **Field names are camelCase; the `hookEventName` *value* is snake_case**
/// (`{"hookEventName": "pre_tool_use", …}`). We never read that value — the
/// event rides our own argv, as it does for every backend — but the casing rule
/// governs everything else here. Confirmed against
/// `xai-grok-hooks/src/event.rs` (1.0.4).
///
/// Documented fields deliberately left out: `workspaceRoot` (the repo root;
/// `cwd` is what the row shows), `timestamp`, `permissionMode`, `toolInput`,
/// `toolUseId`, `toolInputTruncated`, and `Stop`'s `backgroundTasks` /
/// `sessionCrons` (see the module doc).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookPayload {
    session_id: Option<String>,
    cwd: Option<String>,
    /// **Grok's own tool name**, not the Claude alias its matchers accept — a
    /// `Bash` matcher fires but the payload says `run_terminal_command`. Surfaced
    /// verbatim; a display alias table would be exactly the drift the dashboard's
    /// formatting layer avoids.
    tool_name: Option<String>,
    /// `Stop` only: `end_turn` for a real turn end, `channel_closed` / `shutdown`
    /// for the one that fires as the session goes away. See [`is_session_end_stop`].
    reason: Option<String>,
    /// `UserPromptSubmit` only.
    prompt: Option<String>,
    /// Envelope field; the session directory's `updates.jsonl`, when Grok
    /// names one. Rewritten to sibling `summary.json` so the launcher watches
    /// the title file (and folds sibling `signals.json` from the same dir)
    /// rather than every ACP append.
    transcript_path: Option<String>,
    /// Present on events that fire inside a subagent. Those must not move the
    /// parent row — a child's `StopCancelled` is not the session going idle.
    subagent_type: Option<String>,
    /// `SessionStart` only.
    model_id: Option<String>,
    /// `StopFailure` class (`rate_limit`, …) or `PostToolUseFailure` text.
    error: Option<String>,
    error_details: Option<String>,
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse grok hook JSON from stdin")?;
    let session_is_child = payload
        .subagent_type
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|_| true);
    Ok(HookMessage {
        event,
        // Empty is *absent*, not a new identity — taking it would overwrite
        // the id every later hook depends on.
        session_id: payload.session_id.filter(|s| !s.trim().is_empty()),
        tool_name: payload.tool_name,
        message: payload
            .error_details
            .filter(|s| !s.trim().is_empty())
            .or(payload.error.filter(|s| !s.trim().is_empty())),
        cwd: payload.cwd,
        prompt: payload.prompt.filter(|s| !s.trim().is_empty()),
        session_title: None,
        context_tokens: None,
        model: payload.model_id.filter(|s| !s.trim().is_empty()),
        transcript_path: payload
            .transcript_path
            .filter(|s| !s.trim().is_empty())
            .map(|p| summary_path_for(&p)),
        raw: Some(stdin.to_string()),
        session_is_child,
    })
}

/// Point the launcher at `summary.json` in the same session directory as
/// `transcript`. Grok's envelope names `updates.jsonl`, which appends on every
/// ACP event; the title, context total and model live in small sibling JSON
/// files that rewrite at turn boundaries and on `/rename`.
fn summary_path_for(transcript: &str) -> String {
    sidecar_dir(Path::new(transcript))
        .join("summary.json")
        .to_string_lossy()
        .into_owned()
}

fn sidecar_dir(path: &Path) -> &Path {
    if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    }
}

/// Whether a `Stop` payload is the one Grok fires as the **session** ends rather
/// than at the end of a turn (`reason` = `channel_closed` / `shutdown`, against
/// `end_turn` for a real turn end).
///
/// Anything else — including a missing `reason`, an unparseable payload, or no
/// raw payload at all — counts as a turn end. That direction is deliberate: a
/// misread session-end `Stop` costs one `Idle` on a row that is going away,
/// while a misread turn-end `Stop` strands a live row at `Active` forever.
///
/// Reads the raw payload rather than a `HookMessage` field because the reason is
/// Grok-specific and the normalized message has no room for it; `raw` crosses
/// the launcher socket with everything else, so it is available here.
fn is_session_end_stop(raw: Option<&str>) -> bool {
    let Some(raw) = raw else { return false };
    let Ok(payload) = serde_json::from_str::<HookPayload>(raw) else {
        return false;
    };
    matches!(
        payload.reason.as_deref(),
        Some("channel_closed" | "shutdown")
    )
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Grok's departures from [`common::dispatch_default`]; everything else maps
/// the way every backend maps it.
pub async fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    // A subagent's hooks share this process's socket. Adopting their session
    // id would rename the parent row, and their Stop/StopCancelled would Idle
    // a session that is still working. `10-hooks.md` is explicit: exit early
    // when `subagentType` is present.
    if msg.session_is_child == Some(true) {
        return;
    }

    // A session-end `Stop` is not a turn end. Harmless for status either way
    // (the row is on its way out), but it is also the payload that will carry
    // `backgroundTasks` once those are wired, and reading *that* list from a
    // shutdown is how a session ends up looking like it has live background
    // work. Getting it right now costs one branch.
    if msg.event == HookEvent::Stop && is_session_end_stop(msg.raw.as_deref()) {
        common::adopt_session_facts(state, &mut msg);
        return;
    }

    match msg.event {
        // Events no hook of ours registers, so they never reach this
        // dispatcher (see `build_hooks_settings`). Ignored explicitly rather
        // than mapped defensively — the exhaustive match that forces a
        // decision on a newly-added `HookEvent` variant is
        // `common::dispatch_default`'s.
        HookEvent::Elicitation | HookEvent::ElicitationResult | HookEvent::CwdChanged => {}
        _ => common::dispatch_default(state, msg),
    }
}

// =============================================================================
// Transcript fold (signals.json + summary.json)
// =============================================================================

/// Title, context total and model from the session directory Grok names on the
/// hook.
///
/// `path` is the `summary.json` [`parse_hook_payload`] rewrites `transcriptPath`
/// to. The title is `generated_title` (auto or `/rename`); the context gauge is
/// sibling `signals.json`'s `contextTokensUsed`. `prior` is unused: both files
/// are small whole-JSON documents.
pub fn read_transcript_stats(path: &Path) -> TranscriptStats {
    let dir = sidecar_dir(path);
    let mut stats = TranscriptStats::default();

    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Signals {
        #[serde(default)]
        context_tokens_used: Option<u64>,
        #[serde(default)]
        primary_model_id: Option<String>,
    }
    if let Ok(body) = std::fs::read_to_string(dir.join("signals.json"))
        && let Ok(signals) = serde_json::from_str::<Signals>(&body)
    {
        stats.context_tokens = signals.context_tokens_used.filter(|&n| n > 0);
        stats.model = signals.primary_model_id.filter(|m| !m.trim().is_empty());
    }

    if let Ok(body) = std::fs::read_to_string(dir.join("summary.json"))
        && let Ok(summary) = serde_json::from_str::<SessionSummary>(&body)
    {
        stats.name = summary.title();
        if stats.model.is_none() {
            stats.model = Some(summary.current_model_id).filter(|m| !m.trim().is_empty());
        }
    }
    stats
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::SessionStatus;

    /// A `$GROK_HOME/sessions/` tree: one directory per cwd-key, one per session
    /// inside it, `summary.json` inside that.
    fn sessions_fixture(tag: &str, sessions: &[(&str, &str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cm-grok-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (key, id, body) in sessions {
            let dir = root.join(key).join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("summary.json"), body).unwrap();
        }
        root
    }

    /// The picker's rows come off `summary.json`, and the session id is the
    /// **directory's** name rather than anything inside the file — which is how
    /// Grok itself resolves one.
    #[test]
    fn sessions_become_resume_candidates() {
        let root = sessions_fixture(
            "ok",
            &[(
                "cwd-key-1",
                "abc123",
                r#"{"info":{"cwd":"/home/miao/p","head_branch":"main"},
                    "session_summary":"a longer recap of the work",
                    "generated_title":"wire up the parser",
                    "num_messages":12,"current_model_id":"grok-build-0.1"}"#,
            )],
        );
        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "abc123");
        assert_eq!(out[0].cwd, "/home/miao/p");
        assert_eq!(out[0].custom_title.as_deref(), Some("wire up the parser"));
        assert_eq!(out[0].git_branch.as_deref(), Some("main"));
        assert_eq!(out[0].agent, AgentControl::Grok);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 1.0.4 writes `head_branch` at the top level and `info` as `{id, cwd}`
    /// only. The picker has to follow that, or every live session shows no
    /// branch even though the file has one.
    #[test]
    fn a_1_0_4_summary_puts_the_branch_at_the_top_level() {
        let root = sessions_fixture(
            "v104",
            &[(
                "cwd-key-1",
                "01a02249-40a6-7301-b339-cad83f5046cd",
                r#"{"info":{"id":"01a02249-40a6-7301-b339-cad83f5046cd","cwd":"/home/miao/p"},
                    "generated_title":"miao hooks","session_summary":"a longer recap",
                    "head_branch":"main","current_model_id":"grok-4.6"}"#,
            )],
        );
        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "01a02249-40a6-7301-b339-cad83f5046cd");
        assert_eq!(out[0].custom_title.as_deref(), Some("miao hooks"));
        assert_eq!(out[0].git_branch.as_deref(), Some("main"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory with no `summary.json` is not a session — Grok's own
    /// `is_persisted_session_dir` says so, and it is what keeps a half-written
    /// or salvaged directory out of the picker. A summary with no cwd is
    /// likewise dropped rather than offered: `r` would resume it into nowhere.
    #[test]
    fn only_directories_grok_calls_sessions_are_offered() {
        let root = sessions_fixture(
            "partial",
            &[
                (
                    "k",
                    "good",
                    r#"{"info":{"cwd":"/home/miao/p"},"session_summary":"t"}"#,
                ),
                ("k", "no-cwd", r#"{"info":{},"session_summary":"t"}"#),
            ],
        );
        std::fs::create_dir_all(root.join("k").join("not-a-session")).unwrap();
        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "good");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cap is applied to the stat results, before any summary is opened, so
    /// a picker over a long-lived session store reads `limit` files and not one
    /// per session that ever existed.
    #[test]
    fn the_limit_caps_what_is_read() {
        let bodies: Vec<(String, String)> = (0..5)
            .map(|i| {
                (
                    format!("s{i}"),
                    format!(r#"{{"info":{{"cwd":"/home/miao/p{i}"}},"session_summary":"t{i}"}}"#),
                )
            })
            .collect();
        let sessions: Vec<(&str, &str, &str)> = bodies
            .iter()
            .map(|(id, body)| ("k", id.as_str(), body.as_str()))
            .collect();
        let root = sessions_fixture("limit", &sessions);
        assert_eq!(list_resumable_in(&root, 2).len(), 2);
        assert_eq!(list_resumable_in(&root, 99).len(), 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty or absent store is an empty picker, not an error.
    #[test]
    fn a_missing_sessions_root_is_empty_rather_than_an_error() {
        let root = std::env::temp_dir().join(format!("cm-grok-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(list_resumable_in(&root, 10).is_empty());
    }

    fn payload(event: &str, extra: &str) -> String {
        format!(
            r#"{{"hookEventName":"{event}","sessionId":"s1","cwd":"/home/miao/p",
               "workspaceRoot":"/home/miao/p","permissionMode":"default",
               "timestamp":"2026-04-14T12:00:00Z"{extra}}}"#
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::Grok, status)
    }

    /// Drive one hook end to end — parse the agent's stdin JSON, then dispatch it
    /// — so the tests exercise the same path a live hook takes, including the
    /// `Stop`-reason branch that only reads the raw payload.
    fn feed(state: &mut LauncherState, event: HookEvent, stdin: &str) {
        let msg = parse_hook_payload(event, stdin).expect("payload parses");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(dispatch_hook(state, msg));
    }

    #[test]
    fn a_turn_runs_from_prompt_to_stop() {
        let mut state = state_at(SessionStatus::Starting);
        feed(
            &mut state,
            HookEvent::SessionStart,
            &payload("session_start", ""),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        // The session id rides every payload, so the launcher learns it here
        // rather than from a session file.
        assert_eq!(state.session_id.as_deref(), Some("s1"));

        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload("user_prompt_submit", r#","prompt":"wire up the parser""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_prompt.as_deref(), Some("wire up the parser"));

        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload(
                "pre_tool_use",
                r#","toolName":"run_terminal_command","toolInput":{"command":"npm test"}"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Active);
        // Grok's own tool name, not the `Bash` alias its matchers accept.
        assert_eq!(state.last_tool.as_deref(), Some("run_terminal_command"));

        feed(
            &mut state,
            HookEvent::Stop,
            &payload("stop", r#","reason":"end_turn""#),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_tool, None);
    }

    /// The `Stop` that fires as the session goes away must not read as a turn
    /// end — and, far more importantly, anything that is *not* a known
    /// session-end reason must, or a live row strands at `Active` forever.
    #[test]
    fn only_a_known_session_end_reason_stops_a_stop_from_ending_the_turn() {
        for reason in ["channel_closed", "shutdown"] {
            let mut state = state_at(SessionStatus::Active);
            feed(
                &mut state,
                HookEvent::Stop,
                &payload("stop", &format!(r#","reason":"{reason}""#)),
            );
            assert_eq!(state.status, SessionStatus::Active, "reason {reason}");
            // The identity is still adopted on the way past.
            assert_eq!(state.session_id.as_deref(), Some("s1"));
        }

        // A turn end, an unknown reason and a payload with no reason at all all
        // settle the row — the fail-safe direction.
        for extra in [r#","reason":"end_turn""#, r#","reason":"whatever""#, ""] {
            let mut state = state_at(SessionStatus::Active);
            feed(&mut state, HookEvent::Stop, &payload("stop", extra));
            assert_eq!(state.status, SessionStatus::Idle, "extra {extra:?}");
        }
    }

    /// A `permission_prompt` Notification is forwarded as `PermissionRequest`.
    #[test]
    fn a_permission_request_reaches_waiting_for_approval() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            r#"{"sessionId":"s1"}"#,
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
        assert_eq!(state.session_id.as_deref(), Some("s1"));
    }

    /// An empty id is *absent*, never a rename of the session to nothing.
    #[test]
    fn an_empty_session_id_never_clobbers_a_known_one() {
        let mut state = state_at(SessionStatus::Active);
        state.session_id = Some("s1".to_string());
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            r#"{"sessionId":""}"#,
        );
        assert_eq!(state.session_id.as_deref(), Some("s1"));
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
    }

    /// The payload is camelCase where Claude's and Codex's are snake_case — the
    /// single most likely thing to be silently wrong if the source moves.
    #[test]
    fn the_payload_is_camel_case() {
        let stdin = payload(
            "post_tool_use",
            r#","toolName":"search_replace","transcriptPath":"/home/miao/p/s1/updates.jsonl""#,
        );
        let msg = parse_hook_payload(HookEvent::PostToolUse, &stdin).expect("parses");
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
        assert_eq!(msg.cwd.as_deref(), Some("/home/miao/p"));
        assert_eq!(msg.tool_name.as_deref(), Some("search_replace"));
        assert_eq!(
            msg.transcript_path.as_deref(),
            Some("/home/miao/p/s1/summary.json")
        );
        // A snake_case reading would find none of the above; guard the one field
        // whose absence would otherwise look like "the agent didn't send it".
        assert!(
            parse_hook_payload(HookEvent::Stop, r#"{"tool_name":"run_terminal_command"}"#)
                .expect("parses")
                .tool_name
                .is_none()
        );
    }

    /// `StopCancelled` is registered as `stop`, so an interrupt settles the row.
    #[test]
    fn an_interrupt_stop_cancelled_settles_the_row() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::Stop,
            &payload(
                "stop_cancelled",
                r#","reason":"user_interrupt","cancelledBy":"user""#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Idle);
    }

    /// A subagent's turn-end must not Idle the parent or steal its session id.
    #[test]
    fn a_subagent_payload_is_ignored() {
        let mut state = state_at(SessionStatus::Active);
        state.session_id = Some("parent".to_string());
        feed(
            &mut state,
            HookEvent::Stop,
            &payload(
                "stop_cancelled",
                r#","reason":"max_turns","subagentType":"explore""#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.session_id.as_deref(), Some("parent"));
    }

    /// `signals.json` is the context gauge `/session-info` shows; the title
    /// and the model-fallback come off `summary.json`. `generated_title` is
    /// the name, even when a longer `session_summary` recap is also present.
    #[test]
    fn sidecars_fold_tokens_model_and_title() {
        let dir = std::env::temp_dir().join(format!("cm-grok-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("signals.json"),
            r#"{"contextTokensUsed":8929,"primaryModelId":"grok-4.6"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("summary.json"),
            r#"{"generated_title":"miao hooks","session_summary":"a longer recap",
                "current_model_id":"ignored-when-signals-has-one"}"#,
        )
        .unwrap();
        let stats = read_transcript_stats(&dir.join("summary.json"));
        assert_eq!(stats.context_tokens, Some(8929));
        assert_eq!(stats.model.as_deref(), Some("grok-4.6"));
        assert_eq!(stats.name.as_deref(), Some("miao hooks"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One hooks file serves every session, so it must carry no per-session data;
    /// and `Stop` must carry the explicit timeout, whose default there is 600s
    /// rather than 5 and would hold a turn end for ten minutes on a hung write.
    #[test]
    fn hooks_settings_registers_the_native_event_names_and_no_socket() {
        let a = build_hooks_settings("/run/a.sock");
        let b = build_hooks_settings("/run/b.sock");
        assert_eq!(a, b, "the hooks file must not embed the per-session socket");
        assert!(!a.contains(".sock"));

        let json: serde_json::Value = serde_json::from_str(&a).expect("valid JSON");
        let hooks = json["hooks"].as_object().expect("a hooks object");
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "Notification",
                "PostCompact",
                "PostToolUse",
                "PostToolUseFailure",
                "PreCompact",
                "PreToolUse",
                "SessionStart",
                "Stop",
                "StopCancelled",
                "StopFailure",
                "UserPromptSubmit",
            ]
        );

        let stop = &hooks["Stop"][0]["hooks"][0];
        assert_eq!(stop["timeout"], 5);
        assert!(
            stop["command"]
                .as_str()
                .expect("a command string")
                .ends_with("hook --agent grok stop"),
            "{stop:?}"
        );
        // Every other event takes the 5s default, so none of them says so.
        assert!(hooks["PreToolUse"][0]["hooks"][0].get("timeout").is_none());
        // Match-all is the matcher's absence — `"*"` is Grok-safe but the
        // form that silently disarms Kimi, so we don't spell it.
        assert!(hooks["PreToolUse"][0].get("matcher").is_none());
        assert!(
            hooks["StopCancelled"][0]["hooks"][0]["command"]
                .as_str()
                .expect("a command string")
                .ends_with("hook --agent grok stop"),
            "StopCancelled forwards as Stop"
        );
        let notify = hooks["Notification"]
            .as_array()
            .expect("notification groups");
        assert_eq!(notify.len(), 2);
        assert_eq!(notify[0]["matcher"], "permission_prompt");
        assert!(
            notify[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook --agent grok permission-request")
        );
        assert_eq!(notify[1]["matcher"], "idle_prompt");
        assert!(
            notify[1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook --agent grok stop")
        );
    }
}
