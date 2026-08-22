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
//! - **The worktree name isn't shown on the row.** `summary.json` has
//!   `git_root_dir` (the repo) beside `info.cwd` (often a worktree under
//!   `~/.grok/worktrees/`), which is enough to label one without opening
//!   `worktrees.db` — but the dashboard's worktree split is a cwd-path parse
//!   (Claude's `.claude/worktrees/<name>`), and putting `git_root_dir` on the
//!   row needs a `LauncherState` field. `head_branch` is what the resume
//!   picker can show today.
//!
//! Interrupt, prompt, tokens and the hook-file schema are settled as of 1.0.4:
//! `StopCancelled` is a first-class observe hook (Kimi's `Interrupt` standing),
//! `UserPromptSubmit` carries `prompt`, `summary.json` is resolved from the
//! session id (1.0.4's documented common fields do not include `transcriptPath`),
//! and `signals.json` persists `contextTokensUsed` / `contextWindowTokens`.
//! A `Stop` with in-flight `backgroundTasks` / `sessionCrons` lands on
//! Task / Server / Review (an r3 watch is Review) rather than Idle.
//! Unrecognized event names are still skipped, which is why `StopCancelled` is
//! free on an older grok.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::shell_quote;
use super::synth_home::atomic_write;
use crate::agent::{BgSeedKind, BgShell, ResumeCandidate, TranscriptStats};
use crate::state::{HookEvent, HookMessage, LauncherState, SessionStatus};

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
    /// One-line recap of the latest turn. The glance column on an idle / resumed
    /// row, and the resume-picker search text — not the title (`generated_title`)
    /// and not the user's prompt.
    #[serde(default)]
    last_turn_summary: String,
    /// `"subagent"` on a child session. Grok stores those as siblings of the
    /// parent under the same cwd-key (`16-subagents.md`); they share this
    /// process's hook socket and must not become a picker row or a transcript
    /// watch — their `generated_title` is a different session's name.
    #[serde(default)]
    session_kind: String,
}

impl SessionSummary {
    /// Prefer the short title Grok shows in `grok sessions`; fall back to the
    /// recap only when that is still empty (a brand-new session).
    fn title(&self) -> Option<String> {
        Some(self.generated_title.clone())
            .filter(|t| !t.trim().is_empty())
            .or_else(|| Some(self.session_summary.clone()).filter(|t| !t.trim().is_empty()))
    }

    fn is_subagent(&self) -> bool {
        self.session_kind.eq_ignore_ascii_case("subagent")
    }

    /// 1.0.4's top-level field, then the older `info.head_branch` spelling.
    fn git_branch(&self) -> Option<String> {
        Some(self.head_branch.clone())
            .filter(|b| !b.trim().is_empty())
            .or_else(|| Some(self.info.head_branch.clone()).filter(|b| !b.trim().is_empty()))
    }

    fn last_turn_summary(&self) -> Option<String> {
        Some(self.last_turn_summary.clone()).filter(|t| !t.trim().is_empty())
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
/// `signals.json`, not on a resume candidate. `last_turn_summary` rides
/// `first_prompt` so a recap is searchable even when the title is already set.
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
        if summary.info.cwd.trim().is_empty() || summary.is_subagent() {
            continue;
        }
        let custom_title = summary.title();
        let git_branch = summary.git_branch();
        let first_prompt = summary.last_turn_summary();
        out.push(ResumeCandidate {
            agent: crate::agent::AgentControl::Grok,
            session_id: session_id.to_string(),
            cwd: summary.info.cwd,
            first_prompt,
            custom_title,
            git_branch,
            mtime,
        });
    }
    out
}

// =============================================================================
// Alt-screen resolution (pool reattach priming)
// =============================================================================

/// Whether this launch will put Grok's TUI on the terminal's **alternate
/// screen** — the same reads Grok itself makes at startup, judged for a
/// *plain pty* (the pool's environment; `LauncherState::alt_screen` has what
/// consumes the answer). Grok's decision tree, per `user-guide/05-configuration.md`
/// and `06-theming.md` (1.0.4):
///
/// * `--no-alt-screen` on argv → inline, whatever config says.
/// * `screen_mode` (`config.toml`, top level): `"minimal"` renders inline;
///   any other explicit value is sticky non-minimal. *Unset* falls back to the
///   legacy `[terminal] minimal = true` in `pager.toml`, which an explicit
///   `screen_mode` overrides.
/// * `[terminal] alt_screen` (`pager.toml`): `"never"` → inline; `"always"`
///   → alt screen; `"auto"` — and no file, no key, or a value Grok wouldn't
///   recognize either — lands on the default, which on a plain pty is the alt
///   screen ("fullscreen in plain terminals and normal tmux; inline in tmux
///   control mode and Zellij" — a pool pty is the plain case by construction).
///
/// A mid-session `/fullscreen` ↔ minimal switch is invisible to a launch-time
/// read; that staleness is accepted where the answer is consumed.
pub fn uses_alt_screen(agent_args: &[String]) -> bool {
    match grok_home() {
        Some(home) => uses_alt_screen_in(&home, agent_args),
        // Nowhere to read config from — Grok in the same spot runs on
        // defaults, and the default is the alt screen.
        None => !agent_args.iter().any(|a| a == "--no-alt-screen"),
    }
}

fn uses_alt_screen_in(home: &Path, agent_args: &[String]) -> bool {
    if agent_args.iter().any(|a| a == "--no-alt-screen") {
        return false;
    }
    let read = |name: &str| -> Option<toml::Table> {
        std::fs::read_to_string(home.join(name))
            .ok()?
            .parse::<toml::Table>()
            .ok()
    };
    let config = read("config.toml");
    let pager = read("pager.toml");
    let terminal_key = |t: &Option<toml::Table>, key: &str| -> Option<toml::Value> {
        t.as_ref()?.get("terminal")?.get(key).cloned()
    };
    match config.as_ref().and_then(|c| c.get("screen_mode")) {
        Some(toml::Value::String(mode)) if mode == "minimal" => return false,
        // Any other explicit value: sticky non-minimal, fall through to the
        // alt-screen policy.
        Some(_) => {}
        None => {
            if terminal_key(&pager, "minimal").and_then(|v| v.as_bool()) == Some(true) {
                return false;
            }
        }
    }
    // Only an explicit "never" opts out; "always", "auto", an unrecognized
    // value and no key at all are Grok's default on a plain pty.
    !matches!(
        terminal_key(&pager, "alt_screen"),
        Some(toml::Value::String(p)) if p == "never"
    )
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
/// `toolUseId` and `toolInputTruncated`. `Stop`'s `backgroundTasks` /
/// `sessionCrons` *are* read — see [`shells_from_stop`].
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
    /// Optional. 1.0.4's documented common fields do not include it
    /// (`sessionId`, `cwd`, …); when it is present it names `updates.jsonl`
    /// and we rewrite it to sibling `summary.json`. When it is absent we
    /// resolve the same file from `sessionId` — see [`summary_for`].
    transcript_path: Option<String>,
    /// Present on events that fire inside a subagent. Those must not move the
    /// parent row — a child's `StopCancelled` is not the session going idle,
    /// and a child's `generated_title` is not the session's name.
    subagent_type: Option<String>,
    /// `SessionStart` only.
    model_id: Option<String>,
    /// `StopFailure` class (`rate_limit`, …) or `PostToolUseFailure` text.
    error: Option<String>,
    error_details: Option<String>,
    /// `Stop` only. `None` when the event is not a Stop (or an older grok
    /// omitted the field); `Some` even when empty, which is how a real turn
    /// end says "nothing in flight". See [`shells_from_stop`].
    #[serde(default)]
    background_tasks: Option<Vec<BackgroundTask>>,
    /// `Stop` only. Scheduled `/loop` / `scheduler_create` wakeups. Same
    /// presence rule as [`Self::background_tasks`].
    #[serde(default)]
    session_crons: Option<Vec<SessionCron>>,
}

/// One in-flight task from `Stop.backgroundTasks` (`10-hooks.md`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundTask {
    #[serde(default, rename = "type")]
    task_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
    /// Shell tasks only: the command the agent asked to run.
    #[serde(default)]
    command: Option<String>,
    /// A monitor's watched command line, or a subagent's task description.
    #[serde(default)]
    description: Option<String>,
    /// Subagents only.
    #[serde(default)]
    agent_type: Option<String>,
}

/// One scheduled wakeup from `Stop.sessionCrons`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionCron {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse grok hook JSON from stdin")?;
    let mut session_is_child = payload
        .subagent_type
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|_| true);
    // Empty is *absent*, not a new identity — taking it would overwrite
    // the id every later hook depends on.
    let session_id = payload.session_id.filter(|s| !s.trim().is_empty());
    // Prefer a named path (rewritten to `summary.json`); otherwise find the
    // session dir from the id. 1.0.4's documented common fields do not include
    // `transcriptPath`, and without a path the launcher never watches, so
    // title / last-turn recap / tokens never leave the sidecars.
    let mut transcript_path = payload
        .transcript_path
        .filter(|s| !s.trim().is_empty())
        .map(|p| summary_path_for(&p))
        .or_else(|| {
            session_id
                .as_deref()
                .and_then(summary_for)
                .map(|p| p.to_string_lossy().into_owned())
        });
    // A child session is a sibling under the same cwd-key, with its own
    // `summary.json` and `generated_title`. The launcher adopts
    // `transcript_path` *before* `dispatch_hook` can ignore the payload, so a
    // background explore's title would otherwise land on the parent row and
    // flip back on the next parent hook. `subagentType` is the documented
    // signal (`10-hooks.md`); `session_kind` is what the file itself says,
    // for a payload that omitted the field.
    if transcript_path
        .as_deref()
        .is_some_and(|p| summary_is_subagent(Path::new(p)))
    {
        session_is_child = Some(true);
    }
    let session_title = if session_is_child == Some(true) {
        // Drop the path too: otherwise the launcher still switches its watch
        // to the child's sidecars, even after dispatch returns early.
        transcript_path = None;
        None
    } else {
        // Same file the transcript fold reads. Putting the title on the payload
        // means a later hook (the prompt after a `/rename`) stamps it even if a
        // replace landed before the launcher re-armed its file watch. An empty
        // summary is absent, not a rename to nothing — `adopt_session_facts`
        // already drops that.
        transcript_path
            .as_deref()
            .and_then(|p| title_from_summary(Path::new(p)))
    };
    Ok(HookMessage {
        event,
        session_id,
        tool_name: payload.tool_name,
        message: payload
            .error_details
            .filter(|s| !s.trim().is_empty())
            .or(payload.error.filter(|s| !s.trim().is_empty())),
        cwd: payload.cwd,
        prompt: payload
            .prompt
            .as_deref()
            .map(unwrap_user_query)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        session_title,
        context_tokens: None,
        model: payload.model_id.filter(|s| !s.trim().is_empty()),
        transcript_path,
        raw: Some(stdin.to_string()),
        session_is_child,
    })
}

/// Point the launcher at `summary.json` in the same session directory as
/// `transcript`. When Grok does name `transcriptPath` it is `updates.jsonl`,
/// which appends on every ACP event; the title, recap, context total and model
/// live in small sibling JSON files that rewrite at turn boundaries and on
/// `/rename`.
fn summary_path_for(transcript: &str) -> String {
    sidecar_dir(Path::new(transcript))
        .join("summary.json")
        .to_string_lossy()
        .into_owned()
}

/// Grok Build wraps `UserPromptSubmit`'s `prompt` in `<user_query>` …
/// `</user_query>` (the same harness chrome the TUI injects). Those tags are
/// not the prompt; strip them so the glance column shows what the user typed.
/// A prompt that isn't wrapped is returned trimmed, unchanged. A truncated
/// payload that still has the opener (no closer) loses the opener only.
pub fn unwrap_user_query(prompt: &str) -> &str {
    let s = prompt.trim();
    let Some(inner) = s.strip_prefix("<user_query>") else {
        return s;
    };
    inner.strip_suffix("</user_query>").unwrap_or(inner).trim()
}

fn read_summary(path: &Path) -> Option<SessionSummary> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// `generated_title` (or the recap fallback) off a `summary.json`, or `None`
/// when the file is missing, unreadable, or still untitled. `/rename` writes
/// that field and fires no hook, so the hook process re-reads it whenever it
/// already knows the path.
fn title_from_summary(path: &Path) -> Option<String> {
    read_summary(path).and_then(|s| s.title())
}

fn summary_is_subagent(path: &Path) -> bool {
    read_summary(path).is_some_and(|s| s.is_subagent())
}

/// `$GROK_HOME/sessions/<cwd-key>/<session_id>/summary.json`, or `None` if
/// that directory isn't a persisted session. Same walk as [`list_resumable`]:
/// the cwd-key is an encoding we never decode, and Grok's own id resolver
/// walks every key when it has only an id.
fn summary_for(session_id: &str) -> Option<PathBuf> {
    summary_in(&sessions_root()?, session_id)
}

fn summary_in(root: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.trim().is_empty() {
        return None;
    }
    for cwd_key in common::read_subdirs(root) {
        let summary = cwd_key.join(session_id).join("summary.json");
        if summary.is_file() {
            return Some(summary);
        }
    }
    None
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

/// Grok's departures from [`common::dispatch_default`]: a session-end `Stop`,
/// a `Stop` that names live background work, and `ask_user_question` (a
/// blocking choice card, not work). Everything else maps the way every
/// backend maps it.
pub async fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    // A subagent's hooks share this process's socket. Adopting their session
    // id would rename the parent row, their Stop/StopCancelled would Idle a
    // session that is still working, and their `generated_title` would wear
    // the parent's name. `10-hooks.md` is explicit: exit early when
    // `subagentType` is present. Parse also drops `transcript_path` on these
    // so the launcher cannot switch its watch before we get here.
    if msg.session_is_child == Some(true) {
        return;
    }

    // A session-end `Stop` is not a turn end. Harmless for status either way
    // (the row is on its way out), but it is also the payload that carries
    // `backgroundTasks`, and reading *that* list from a shutdown is how a
    // session ends up looking like it has live background work.
    if msg.event == HookEvent::Stop && is_session_end_stop(msg.raw.as_deref()) {
        common::adopt_session_facts(state, &mut msg);
        return;
    }

    if msg.event == HookEvent::Stop {
        match shells_from_stop(msg.raw.as_deref()) {
            Some(shells) if !shells.is_empty() => {
                common::adopt_session_facts(state, &mut msg);
                state.last_tool = None;
                state.status = status_from_shells(&shells);
                return;
            }
            None if matches!(
                state.status,
                SessionStatus::BackgroundActive
                    | SessionStatus::BackgroundServer
                    | SessionStatus::ReviewPending
            ) =>
            {
                // `idle_prompt` (and any Stop that omitted the list) is not
                // evidence the live work ended — the previous Stop already
                // named it. Hold the background row rather than flashing Idle.
                common::adopt_session_facts(state, &mut msg);
                return;
            }
            Some(_) | None => {
                // Empty list, or absent on a non-background row: the turn ended.
            }
        }
    }

    match msg.event {
        // Events no hook of ours registers, so they never reach this
        // dispatcher (see `build_hooks_settings`). Ignored explicitly rather
        // than mapped defensively — the exhaustive match that forces a
        // decision on a newly-added `HookEvent` variant is
        // `common::dispatch_default`'s.
        HookEvent::Elicitation | HookEvent::ElicitationResult | HookEvent::CwdChanged => {}
        // `ask_user_question` is Grok's AskUserQuestion analog — a tool that
        // renders a multiple-choice card and blocks until the user picks. It
        // is auto-allowed (the permission_prompt Notification, when it fires
        // at all, resolves in 0ms), so this `PreToolUse` is the only signal
        // the session is waiting. Without an arm here the row sits at plain
        // `Active` for as long as the card is up. Surface it as
        // `WaitingForDecision` ("Decision"), the same bucket as Claude's
        // `AskUserQuestion`, Codex's `request_user_input` and Reasonix's
        // `ask`. A gated `ask_user_question` that does fire
        // `PermissionRequest` lands here too, so it reads as a question
        // rather than a tool-approval gate.
        HookEvent::PreToolUse | HookEvent::PermissionRequest
            if msg.tool_name.as_deref() == Some("ask_user_question") =>
        {
            common::adopt_session_facts(state, &mut msg);
            state.status = SessionStatus::WaitingForDecision;
            state.last_tool = msg.tool_name;
        }
        // Grok issues tools in parallel. A `search_replace` PostToolUse 35ms
        // after `ask_user_question`'s PreToolUse is the shared mapping
        // snapping Decision back to Active while the card is still up. Hold
        // the row until the question tool itself completes — that
        // PostToolUse takes the shared mapping through the catch-all.
        HookEvent::PreToolUse | HookEvent::PostToolUse | HookEvent::PostToolUseFailure
            if state.status == SessionStatus::WaitingForDecision
                && msg.tool_name.as_deref() != Some("ask_user_question") =>
        {
            common::adopt_session_facts(state, &mut msg);
        }
        _ => common::dispatch_default(state, msg),
    }
}

/// Live background work named on a `Stop` payload, or `None` when the payload
/// did not carry the arrays (a Notification forwarded as Stop, an older grok,
/// an unparseable body). `Some` even when empty: that is a real turn end saying
/// nothing is in flight.
///
/// Classified the same way Claude's process-tree walk is, except the *type* is
/// Grok's rather than inferred from command text: a `monitor` is at-rest by
/// construction, a `subagent` is busy work, a `/loop` cron is a parked wakeup.
fn shells_from_stop(raw: Option<&str>) -> Option<Vec<BgShell>> {
    let raw = raw?;
    let payload: HookPayload = serde_json::from_str(raw).ok()?;
    if payload.background_tasks.is_none() && payload.session_crons.is_none() {
        return None;
    }
    let mut shells = Vec::new();
    if let Some(tasks) = &payload.background_tasks {
        shells.extend(tasks.iter().filter_map(shell_from_task));
    }
    if let Some(crons) = &payload.session_crons {
        shells.extend(crons.iter().filter_map(shell_from_cron));
    }
    Some(shells)
}

fn shell_from_task(task: &BackgroundTask) -> Option<BgShell> {
    if !is_live_task_status(task.status.as_deref()) {
        return None;
    }
    let ty = task.task_type.as_deref().unwrap_or("");
    match ty {
        "shell" | "bash" => {
            let key = task
                .command
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            Some(classify_command(key))
        }
        "monitor" => Some(BgShell {
            key: task
                .description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("monitor")
                .to_string(),
            kind: BgSeedKind::LongRunning,
        }),
        "subagent" => Some(BgShell {
            key: task
                .description
                .as_deref()
                .or(task.agent_type.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("subagent")
                .to_string(),
            kind: BgSeedKind::Other,
        }),
        _ => None,
    }
}

fn shell_from_cron(cron: &SessionCron) -> Option<BgShell> {
    let key = cron
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(cron
            .schedule
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()))
        .unwrap_or("loop");
    Some(BgShell {
        key: key.to_string(),
        kind: BgSeedKind::LongRunning,
    })
}

fn classify_command(key: &str) -> BgShell {
    let kind = if super::claude::is_r3_watch_command(key) {
        BgSeedKind::ReviewWatch
    } else if super::claude::is_long_running_command(key) {
        BgSeedKind::LongRunning
    } else {
        BgSeedKind::Other
    };
    BgShell {
        key: key.to_string(),
        kind,
    }
}

/// Grok's docs don't enumerate `status` values; the live payload uses
/// `"running"`. Treat a missing or unknown value as in-flight — dropping a
/// running task is the silent failure — and only skip statuses that are
/// clearly terminal.
fn is_live_task_status(status: Option<&str>) -> bool {
    !matches!(
        status.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some(
            "completed"
                | "complete"
                | "failed"
                | "cancelled"
                | "canceled"
                | "exited"
                | "stopped"
                | "success"
                | "error"
        )
    )
}

/// Same precedence as the launcher's `classify_and_learn`: any finite task
/// keeps the row busy (`Task`); else any parked server/monitor/loop is at-rest
/// (`Server`); else every remaining shell is an r3 review-watch (`Review`).
fn status_from_shells(shells: &[BgShell]) -> SessionStatus {
    let any_transient = shells.iter().any(|s| s.kind == BgSeedKind::Other);
    let any_long = shells.iter().any(|s| s.kind == BgSeedKind::LongRunning);
    if any_transient {
        SessionStatus::BackgroundActive
    } else if any_long {
        SessionStatus::BackgroundServer
    } else {
        SessionStatus::ReviewPending
    }
}

// =============================================================================
// Transcript fold (signals.json + summary.json)
// =============================================================================

/// Title, context total and model from the session directory Grok names on the
/// hook.
///
/// `path` is the `summary.json` [`parse_hook_payload`] rewrites `transcriptPath`
/// to. The title is `generated_title` (auto or `/rename`); `/rename` fires no
/// hook and replaces the file, so the launcher re-arms its file watch after
/// each wake rather than polling or watching the session directory. The
/// context gauge is sibling `signals.json`'s `contextTokensUsed` over
/// `contextWindowTokens` — that file is replaced independently, so it has the
/// same file watch (started once it exists, never via the session dir).
/// `prior` is unused: both files are small whole-JSON documents.
pub fn read_transcript_stats(path: &Path) -> TranscriptStats {
    let dir = sidecar_dir(path);
    // A child's sidecars must not fold onto the parent row. `apply_transcript_data`
    // is Some-only, so an empty return leaves the parent's title and tokens.
    if summary_is_subagent(&dir.join("summary.json")) {
        return TranscriptStats::default();
    }
    let mut stats = TranscriptStats::default();

    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Signals {
        #[serde(default)]
        context_tokens_used: Option<u64>,
        #[serde(default)]
        context_window_tokens: Option<u64>,
        #[serde(default)]
        primary_model_id: Option<String>,
    }
    if let Ok(body) = std::fs::read_to_string(dir.join("signals.json"))
        && let Ok(signals) = serde_json::from_str::<Signals>(&body)
    {
        stats.context_tokens = signals.context_tokens_used.filter(|&n| n > 0);
        stats.context_window = signals.context_window_tokens.filter(|&n| n > 0);
        stats.model = signals.primary_model_id.filter(|m| !m.trim().is_empty());
    }

    if let Ok(body) = std::fs::read_to_string(dir.join("summary.json"))
        && let Ok(summary) = serde_json::from_str::<SessionSummary>(&body)
    {
        stats.name = summary.title();
        stats.last_prompt = summary.last_turn_summary();
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
        assert_eq!(out[0].first_prompt, None);
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
                    "last_turn_summary":"Resume/fork already work; picker now shows branch",
                    "head_branch":"main","current_model_id":"grok-4.6"}"#,
            )],
        );
        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "01a02249-40a6-7301-b339-cad83f5046cd");
        assert_eq!(out[0].custom_title.as_deref(), Some("miao hooks"));
        assert_eq!(out[0].git_branch.as_deref(), Some("main"));
        assert_eq!(
            out[0].first_prompt.as_deref(),
            Some("Resume/fork already work; picker now shows branch")
        );
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

    /// Child sessions live as siblings under the same cwd-key. Offering one
    /// from `r` would resume a subagent as a top-level grok, and its title is
    /// what the parent row was flickering to.
    #[test]
    fn a_subagent_session_is_not_a_resume_candidate() {
        let root = sessions_fixture(
            "subagent-picker",
            &[
                (
                    "k",
                    "parent",
                    r#"{"info":{"cwd":"/home/miao/p"},"generated_title":"the real work"}"#,
                ),
                (
                    "k",
                    "child",
                    r#"{"info":{"cwd":"/home/miao/p"},"generated_title":"Catchlight Bevy editor review bugs","session_kind":"subagent"}"#,
                ),
            ],
        );
        let out = list_resumable_in(&root, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "parent");
        assert_eq!(out[0].custom_title.as_deref(), Some("the real work"));
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

    /// The glance column is the user's text, not the harness wrapper — including
    /// a truncated payload that still has the opener, and a prompt that was
    /// never wrapped.
    #[test]
    fn user_query_tags_are_not_the_prompt() {
        assert_eq!(
            unwrap_user_query("<user_query>\nlook at the last prompt\n</user_query>"),
            "look at the last prompt"
        );
        assert_eq!(
            unwrap_user_query("<user_query>\nlook at the last prompt"),
            "look at the last prompt"
        );
        assert_eq!(
            unwrap_user_query("wire up the parser"),
            "wire up the parser"
        );
        assert_eq!(unwrap_user_query("  <user_query></user_query>  "), "");
    }

    /// An empty or absent store is an empty picker, not an error.
    #[test]
    fn a_missing_sessions_root_is_empty_rather_than_an_error() {
        let root = std::env::temp_dir().join(format!("cm-grok-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(list_resumable_in(&root, 10).is_empty());
    }

    /// A `$GROK_HOME` carrying the given `config.toml` / `pager.toml` bodies
    /// (`None` = no such file).
    fn home_fixture(tag: &str, config: Option<&str>, pager: Option<&str>) -> PathBuf {
        let home = std::env::temp_dir().join(format!("cm-grok-alt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        if let Some(c) = config {
            std::fs::write(home.join("config.toml"), c).unwrap();
        }
        if let Some(p) = pager {
            std::fs::write(home.join("pager.toml"), p).unwrap();
        }
        home
    }

    /// The launch-time mirror of Grok's own startup decision, over the config
    /// surface `05-configuration.md` / `06-theming.md` document: the
    /// `alt_screen` policy in `pager.toml`, `screen_mode` (and the legacy
    /// `[terminal] minimal`) picking minimal-vs-fullscreen, `--no-alt-screen`
    /// on argv. Only an explicit opt-out may yield `false`: a wrong `false`
    /// keeps today's behavior, a wrong `true` breaks the window.
    #[test]
    fn alt_screen_resolution_follows_groks_config() {
        let no_args: &[String] = &[];
        let cases: &[(&str, Option<&str>, Option<&str>, bool)] = &[
            ("defaults", None, None, true),
            (
                "auto",
                None,
                Some("[terminal]\nalt_screen = \"auto\"\n"),
                true,
            ),
            (
                "always",
                None,
                Some("[terminal]\nalt_screen = \"always\"\n"),
                true,
            ),
            (
                "never",
                None,
                Some("[terminal]\nalt_screen = \"never\"\n"),
                false,
            ),
            // A value Grok wouldn't recognize lands on its default (auto).
            (
                "odd",
                None,
                Some("[terminal]\nalt_screen = \"sometimes\"\n"),
                true,
            ),
            ("minimal", Some("screen_mode = \"minimal\"\n"), None, false),
            // Minimal renders inline even when the policy says always.
            (
                "minimal-beats-always",
                Some("screen_mode = \"minimal\"\n"),
                Some("[terminal]\nalt_screen = \"always\"\n"),
                false,
            ),
            // Legacy `[terminal] minimal` forces minimal only while
            // `screen_mode` is unset…
            (
                "legacy-minimal",
                None,
                Some("[terminal]\nminimal = true\n"),
                false,
            ),
            // …and an explicit `screen_mode` overrides it.
            (
                "fullscreen-overrides-legacy",
                Some("screen_mode = \"fullscreen\"\n"),
                Some("[terminal]\nminimal = true\n"),
                true,
            ),
            // An unparseable file is not an opt-out.
            ("mangled", Some("screen_mode = [not toml"), None, true),
        ];
        for (tag, config, pager, want) in cases {
            let home = home_fixture(tag, *config, *pager);
            assert_eq!(uses_alt_screen_in(&home, no_args), *want, "case {tag:?}");
        }
        // The argv opt-out beats everything, including an `always` policy.
        let home = home_fixture("argv", None, Some("[terminal]\nalt_screen = \"always\"\n"));
        assert!(!uses_alt_screen_in(&home, &["--no-alt-screen".to_string()]));
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

        // Grok Build wraps the typed prompt in harness tags; those are not
        // the prompt, and a later turn must replace the row with the inner
        // text rather than showing `<user_query>` in the glance column.
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload(
                "user_prompt_submit",
                r#","prompt":"<user_query>\nlook at the last prompt\n</user_query>""#,
            ),
        );
        assert_eq!(
            state.last_prompt.as_deref(),
            Some("look at the last prompt")
        );

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

    /// `ask_user_question` renders a multiple-choice card and blocks on the
    /// answer. Grok auto-allows it (no `PermissionRequest` in the launcher
    /// log), so this `PreToolUse` is the only evidence the session is waiting,
    /// and it must not read as `Active`. Captured from a live session: the
    /// launcher logged `PreToolUse tool=Some("ask_user_question")` and the
    /// row stayed Active while the card was up.
    #[test]
    fn the_ask_user_question_tool_is_a_decision_not_plain_work() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload("pre_tool_use", r#","toolName":"ask_user_question""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForDecision);
        assert_eq!(state.last_tool.as_deref(), Some("ask_user_question"));

        // The answer arrives as the paired PostToolUse, which settles the row
        // back to Active through the shared mapping.
        feed(
            &mut state,
            HookEvent::PostToolUse,
            &payload("post_tool_use", r#","toolName":"ask_user_question""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool, None);
    }

    /// A gated `ask_user_question` that does fire `permission_prompt` is
    /// still a question, not a tool-approval gate.
    #[test]
    fn a_permission_request_for_ask_user_question_is_decision() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &payload("notification", r#","toolName":"ask_user_question""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForDecision);
        assert_eq!(state.last_tool.as_deref(), Some("ask_user_question"));
    }

    /// Grok issued `search_replace` and `ask_user_question` in the same
    /// burst; the edit's PostToolUse landed 35ms after the question's
    /// PreToolUse. The shared mapping would have flashed Decision then
    /// snapped back to Active while the card was still up.
    #[test]
    fn a_parallel_tool_does_not_clear_a_question_card() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload("pre_tool_use", r#","toolName":"search_replace""#),
        );
        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload("pre_tool_use", r#","toolName":"ask_user_question""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForDecision);

        feed(
            &mut state,
            HookEvent::PostToolUse,
            &payload("post_tool_use", r#","toolName":"search_replace""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForDecision);
        assert_eq!(state.last_tool.as_deref(), Some("ask_user_question"));

        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload("pre_tool_use", r#","toolName":"read_file""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForDecision);

        feed(
            &mut state,
            HookEvent::PostToolUse,
            &payload("post_tool_use", r#","toolName":"ask_user_question""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool, None);
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

    /// 1.0.4's documented common fields omit `transcriptPath`. The session id
    /// is enough: the summary sits at `sessions/<cwd-key>/<id>/summary.json`,
    /// which is also how Grok itself resolves an id.
    #[test]
    fn the_session_id_finds_summary_json_when_the_envelope_names_no_transcript() {
        let root = sessions_fixture(
            "from-id",
            &[("k", "abc123", r#"{"info":{"cwd":"/home/miao/p"}}"#)],
        );
        assert_eq!(
            summary_in(&root, "abc123"),
            Some(root.join("k").join("abc123").join("summary.json"))
        );
        assert!(summary_in(&root, "nope").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An id that matches nothing must not invent a path — the launcher would
    /// watch a file that never appears and fold nothing, which looks the same
    /// as "the agent sent no title" rather than "we guessed wrong".
    #[test]
    fn a_payload_without_transcript_path_leaves_the_path_unset_when_the_id_is_unknown() {
        let msg = parse_hook_payload(HookEvent::Stop, r#"{"sessionId":"no-such-session"}"#)
            .expect("parses");
        assert_eq!(msg.session_id.as_deref(), Some("no-such-session"));
        assert_eq!(msg.transcript_path, None);
        assert_eq!(msg.session_title, None);
    }

    /// `/rename` writes `generated_title` (and `title_is_manual`) and fires no
    /// hook. The next payload that names the session directory must carry that
    /// title, or the row stays on whatever the first fold saw.
    #[test]
    fn a_rename_in_summary_json_arrives_on_the_hook_as_session_title() {
        let dir = std::env::temp_dir().join(format!("cm-grok-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("summary.json"),
            r#"{"generated_title":"fix session name grok","title_is_manual":true}"#,
        )
        .unwrap();
        let transcript = dir.join("updates.jsonl");
        let stdin = payload(
            "user_prompt_submit",
            &format!(
                r#","transcriptPath":{}"#,
                serde_json::to_string(&transcript.to_string_lossy()).unwrap()
            ),
        );
        let msg = parse_hook_payload(HookEvent::PromptSubmit, &stdin).expect("parses");
        assert_eq!(
            msg.transcript_path.as_deref(),
            Some(dir.join("summary.json").to_str().unwrap())
        );
        assert_eq!(msg.session_title.as_deref(), Some("fix session name grok"));
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The live `r3 watch` from session `01a0254c-…`: `Stop` names it as a
    /// shell task, so the row is `Review` rather than `Idle`.
    #[test]
    fn a_stop_with_an_r3_watch_is_review() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::Stop,
            &payload(
                "stop",
                r#","reason":"end_turn","backgroundTasks":[{
                    "id":"01a02559-f9f7-7760-8bd5-ab655a564e7c",
                    "type":"shell","status":"running",
                    "command":"/home/liteye/projects/hovo/r3/r3 watch review_a130e24bc728 --session grok-deep-review"
                }]"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::ReviewPending);
        assert_eq!(state.last_tool, None);
    }

    /// A finite background command is busy `Task`; a recognized dev server is
    /// at-rest `Server`; a `monitor` is at-rest by construction; a background
    /// subagent is busy work; a `/loop` cron is a parked wakeup.
    #[test]
    fn stop_background_tasks_pick_the_tier_from_type_and_command() {
        let cases: &[(&str, SessionStatus)] = &[
            (
                r#","reason":"end_turn","backgroundTasks":[{"type":"shell","status":"running","command":"cargo test"}]"#,
                SessionStatus::BackgroundActive,
            ),
            (
                r#","reason":"end_turn","backgroundTasks":[{"type":"shell","status":"running","command":"npm run dev"}]"#,
                SessionStatus::BackgroundServer,
            ),
            (
                r#","reason":"end_turn","backgroundTasks":[{"type":"monitor","status":"running","description":"tail -f log"}]"#,
                SessionStatus::BackgroundServer,
            ),
            (
                r#","reason":"end_turn","backgroundTasks":[{"type":"subagent","status":"running","description":"explore the repo","agentType":"explore"}]"#,
                SessionStatus::BackgroundActive,
            ),
            (
                r#","reason":"end_turn","sessionCrons":[{"id":"loop-1","schedule":"every 5 minutes","prompt":"check CI"}]"#,
                SessionStatus::BackgroundServer,
            ),
            (
                r#","reason":"end_turn","backgroundTasks":[]"#,
                SessionStatus::Idle,
            ),
        ];
        for (extra, want) in cases {
            let mut state = state_at(SessionStatus::Active);
            feed(&mut state, HookEvent::Stop, &payload("stop", extra));
            assert_eq!(state.status, *want, "extra {extra}");
        }
    }

    /// Finite work dominates: an r3 watch plus a `cargo test` is `Task`, not
    /// `Review`, matching the launcher's classify-and-learn precedence.
    #[test]
    fn a_transient_task_outranks_an_r3_watch() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::Stop,
            &payload(
                "stop",
                r#","reason":"end_turn","backgroundTasks":[
                    {"type":"shell","status":"running","command":"r3 watch review_abc"},
                    {"type":"shell","status":"running","command":"cargo test"}
                ]"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::BackgroundActive);
    }

    /// `idle_prompt` is forwarded as `Stop` but is a Notification payload, so
    /// it has no `backgroundTasks` field. That omission must not retire a
    /// review-watch the previous real Stop already named.
    #[test]
    fn an_idle_prompt_does_not_clear_a_background_row() {
        let mut state = state_at(SessionStatus::ReviewPending);
        state.session_id = Some("s1".to_string());
        feed(
            &mut state,
            HookEvent::Stop,
            r#"{"hookEventName":"notification","sessionId":"s1","notificationType":"idle_prompt"}"#,
        );
        assert_eq!(state.status, SessionStatus::ReviewPending);
        assert_eq!(state.session_id.as_deref(), Some("s1"));
    }

    /// A completed shell is not in-flight; a session-end Stop still must not
    /// adopt leftover tasks as live work.
    #[test]
    fn a_completed_task_and_a_shutdown_stop_are_not_live_background_work() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::Stop,
            &payload(
                "stop",
                r#","reason":"end_turn","backgroundTasks":[{"type":"shell","status":"completed","command":"r3 watch review_abc"}]"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Idle);

        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::Stop,
            &payload(
                "stop",
                r#","reason":"shutdown","backgroundTasks":[{"type":"shell","status":"running","command":"r3 watch review_abc"}]"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// A subagent's turn-end must not Idle the parent or steal its session id.
    #[test]
    fn a_subagent_payload_is_ignored() {
        let mut state = state_at(SessionStatus::Active);
        state.session_id = Some("parent".to_string());
        state.name = Some("Resume Claude Session".to_string());
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
        assert_eq!(state.name.as_deref(), Some("Resume Claude Session"));
    }

    /// A child's `transcriptPath` must not ride the hook. The launcher adopts
    /// that path *before* dispatch, so a named child summary would stamp its
    /// `generated_title` onto the parent row even though dispatch ignores the
    /// event.
    #[test]
    fn a_subagent_hook_does_not_name_its_own_transcript() {
        let dir = std::env::temp_dir().join(format!("cm-grok-child-tx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("summary.json"),
            r#"{"generated_title":"Catchlight Bevy editor review bugs","session_kind":"subagent"}"#,
        )
        .unwrap();
        let stdin = payload(
            "pre_tool_use",
            &format!(
                r#","subagentType":"explore","transcriptPath":{}"#,
                serde_json::to_string(&dir.join("updates.jsonl")).unwrap()
            ),
        );
        let msg = parse_hook_payload(HookEvent::PreToolUse, &stdin).expect("parses");
        assert_eq!(msg.session_is_child, Some(true));
        assert_eq!(msg.transcript_path, None);
        assert_eq!(msg.session_title, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `session_kind` on the file is enough when `subagentType` is missing —
    /// a torn payload must not steal the row either.
    #[test]
    fn a_subagent_summary_without_subagent_type_is_still_a_child() {
        let dir = std::env::temp_dir().join(format!("cm-grok-child-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("summary.json"),
            r#"{"generated_title":"Catchlight Bevy editor review bugs","session_kind":"subagent"}"#,
        )
        .unwrap();
        let stdin = payload(
            "pre_tool_use",
            &format!(
                r#","transcriptPath":{}"#,
                serde_json::to_string(&dir.join("updates.jsonl")).unwrap()
            ),
        );
        let msg = parse_hook_payload(HookEvent::PreToolUse, &stdin).expect("parses");
        assert_eq!(msg.session_is_child, Some(true));
        assert_eq!(msg.transcript_path, None);
        assert_eq!(msg.session_title, None);
        let _ = std::fs::remove_dir_all(&dir);
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
            r#"{"contextTokensUsed":8929,"contextWindowTokens":500000,"primaryModelId":"grok-4.6"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("summary.json"),
            r#"{"generated_title":"miao hooks","session_summary":"a longer recap",
                "last_turn_summary":"Pinned GrokNight; no custom palettes",
                "current_model_id":"ignored-when-signals-has-one"}"#,
        )
        .unwrap();
        let stats = read_transcript_stats(&dir.join("summary.json"));
        assert_eq!(stats.context_tokens, Some(8929));
        assert_eq!(stats.context_window, Some(500_000));
        assert_eq!(stats.model.as_deref(), Some("grok-4.6"));
        assert_eq!(stats.name.as_deref(), Some("miao hooks"));
        assert_eq!(
            stats.last_prompt.as_deref(),
            Some("Pinned GrokNight; no custom palettes")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Folding a child's sidecars would stamp its title and tokens onto the
    /// parent row. An empty fold is Some-only at apply time, so the parent
    /// keeps what it already has.
    #[test]
    fn a_subagent_summary_does_not_fold_onto_the_row() {
        let dir = std::env::temp_dir().join(format!("cm-grok-child-fold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("signals.json"),
            r#"{"contextTokensUsed":111,"contextWindowTokens":500000,"primaryModelId":"grok-4.6"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("summary.json"),
            r#"{"generated_title":"Catchlight Bevy editor review bugs","session_kind":"subagent","current_model_id":"grok-4.6"}"#,
        )
        .unwrap();
        let stats = read_transcript_stats(&dir.join("summary.json"));
        assert_eq!(stats, TranscriptStats::default());
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
