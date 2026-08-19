//! Oh My Pi (`omp`) CLI backend. Owns every omp-specific path, argv flag and
//! hook payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Omp`'s match arms.
//!
//! **Source-verified against the installed binary**, `omp` v17.3.7 at
//! `/opt/homebrew/bin/omp` — a Bun-compiled single executable whose JS source
//! and Markdown docs are embedded and readable (`strings -a` on the binary;
//! `omp read omp://<doc>`). `pi` 0.84.2 is separately installed at
//! `/opt/homebrew/bin/pi`; the two are distinct binaries with distinct state
//! dirs (`~/.omp` vs `~/.pi`), so nothing about the pi backend changes. This
//! module is built on the same `-e <extension.ts>` injection pi uses — **not**
//! a parameterization of `pi.rs`: omp's event surface has diverged from pi's
//! in four load-bearing ways (below), so the two modules share only what
//! already lives in `agents::common`.
//!
//! omp is, like pi, **neither a synthetic home nor a shell hook**, and for
//! pi's reason: `-e` is per-run, torn down with the process, nothing of the
//! user's touched. See `agents::pi`'s module doc for the trust argument — a
//! `-e` extension is trusted by virtue of being on the command line, so there
//! is no seeding step. omp needs no [`super::synth_home::SynthHome`] at all.
//!
//! # The four divergences from pi
//!
//! ## 1. `agent_end` + `willContinue`, not `agent_settled`
//!
//! `agent_settled` does not exist in omp (`strings -a omp | grep -cF
//! agent_settled` → 0). pi's whole turn-end design rested on it. omp's
//! replacement is **`agent_end` + `willContinue`**: the emitter is
//! `await this.#d?.emit({ type: "agent_end", messages: w, willContinue:
//! u?.willContinue })`, and omp's own wire encoder uses exactly
//! `w.type === "agent_end" && w.willContinue !== true` as its terminal test,
//! while the internal notifier names the same value `isTerminal:
//! !b?.willContinue`. Every auto-retry, auto-compaction, queued follow-up and
//! `session_stop` continuation branch calls `n({ willContinue: true })`; only
//! the genuinely-final paths call `n()` with no argument. It also fires on the
//! abort path (`if (o.stopReason === "aborted") { … await n(h ?
//! { willContinue: true } : undefined); }`), so an interrupted turn settles —
//! the gap that costs Claude a session-file fold.
//!
//! So `agent_end` maps to [`HookEvent::Stop`] and **nothing else does**, and a
//! `Stop` whose payload carries `will_continue: true` is renamed in
//! [`normalize_event`] to [`HookEvent::PostToolUse`] — the arm that means
//! exactly "a unit of work ended, the session is still `Active`, nothing is in
//! flight" — rather than settling the row while omp keeps working. Unlike
//! pi's `is_error` correction this one is **not cosmetic**; a wrong answer
//! here is a wrong row.
//!
//! ## 2. `session_stop` exists but must not be used
//!
//! It is guarded by `if (this.#c0 === "sub" || !this.#d?.hasHandlers
//! ("session_stop"))` and by two abort flags before it emits, so it is skipped
//! on interrupt; it fires *before* the final `agent_end`; and its return value
//! (`{ continue: true, additionalContext }`) can extend the user's turn.
//! Subscribing to it would settle a row that is about to continue, and miss
//! every interrupt. It is in the "deliberately not registered" list below.
//!
//! ## 3. A per-tool approval gate, which pi has none of
//!
//! `tool_approval_requested` fires before the approval wait with
//! `{ sessionId, toolName, toolCallId, reason?, approvalMode }`, and
//! `tool_approval_resolved` fires "after approve, deny, or approval prompt
//! failure". Both are emitted whenever a handler is registered
//! (`const K = this.runner.hasHandlers("tool_approval_requested") ||
//! this.runner.hasHandlers("tool_approval_resolved")`). So `Omp` gets
//! `approval_gate: true` — pi's `false` is the one capability row that
//! inverts. The pair maps to [`HookEvent::PermissionRequest`] and
//! [`HookEvent::ElicitationResult`], the same shape opencode already has, and
//! neither return value can alter the turn.
//!
//! ## 4. `session_switch`, not `session_info_changed`
//!
//! `session_info_changed` does not exist in omp (grep → 0). Its role (deliver
//! a fact with no status move) is taken by **`session_switch`**, which omp
//! emits on `/new`, `/fork` and `/resume` and which is where a mid-session
//! session-id change becomes visible to `r`/`f`. It maps to
//! [`HookEvent::SessionStart`], the one arm in [`common::dispatch_default`]
//! that touches status only to settle a row out of `Starting` — so it adopts
//! the new session id and moves nothing, which is precisely what is wanted.
//!
//! # Unchanged from pi
//!
//! `-e/--extension <path>` (repeatable), `before_agent_start` carrying
//! `prompt`, `tool_execution_start`/`tool_execution_end` carrying
//! `toolName`/`isError`, `session_before_compact`/`session_compact` (emitted
//! on both manual *and* automatic compaction paths), `session_start`, and the
//! whole `ExtensionAPI` surface the payload reads — `pi.on`,
//! `pi.getSessionName()`, `ctx.sessionManager.getSessionId()`, `ctx.cwd`,
//! `ctx.getContextUsage()`, `ctx.model`. The default export still takes the
//! API object named `pi` in omp's own docs, so the generated extension keeps
//! that parameter name.
//!
//! # Resume and fork
//!
//! omp's argv handler map registers `"--resume": R2w`, `"-r": R2w`,
//! `"--session": R2w` (an undocumented alias) and `"--fork": (w, u) => { w.fork
//! = u; }`. [`AgentControl::resume_args`] uses `--resume` / `--fork` —
//! `--resume` is in the public flag schema and `--help`, `--fork` is in the
//! argv map and documented in `omp://session-operations-export-share-fork-
//! resume.md`. `--session` is deliberately not used: it is absent from both
//! `--help` and the flag schema.
//!
//! No `--worktree` launch flag exists (the single `--worktree` string in the
//! binary is `git restore --worktree`). omp's `omp worktree` subcommand only
//! lists/clears worktrees the agent made itself, so there is nothing to pass
//! at launch and [`AgentControl::worktree_args`] answers `None`.
//!
//! # Loaded with Bun, not jiti
//!
//! omp loads this extension with **Bun**, not pi's jiti — omp's
//! `extension-loading.md` accepts explicit `.ts`/`.js`/`.mjs`/`.cjs` — so the
//! `.ts` suffix is no longer a loader requirement. It is kept anyway for the
//! reason [`extension_path`]'s doc gives: the launcher's cleanup and
//! dead-launcher sweep key on `.sock` / `-settings.json` by name, so the
//! relocated copy must live under a name that never accumulates. The file sits
//! in `~/.local/state/captain-miao/`, which is none of omp's auto-discovery
//! roots (`<cwd>/.omp/extensions`, `~/.omp/agent/extensions`, plugin
//! manifests), so it is loaded exactly once, by our `-e`.
//!
//! # Known ordering caveat
//!
//! omp emits `agent_end` fire-and-forget (`this.#t1([...W],
//! b).catch(…)`, not awaited), unlike `session_stop` which it awaits. So a
//! `Stop` is not ordered against a following `PromptSubmit` the way pi's
//! awaited events were. It needs a user to submit a new prompt inside the
//! ~15ms a `miao hook` spawn takes, so it is a real but unreachable race;
//! naming it here stops the next reader re-deriving it.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::synth_home::atomic_write;
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "omp";

// =============================================================================
// The event table — the module's claim about what it subscribes to
// =============================================================================

/// Every omp event the generated extension subscribes to, paired with the
/// [`HookEvent`] it is forwarded as. This const **is** the claim; the emitted
/// JavaScript renders it, so the two cannot disagree.
///
/// The four choices in here that are omp's rather than pi's:
///
/// - **`agent_end`, not `agent_settled` (gone) and not `session_stop`
///   (skipped on abort, fires early, can extend the turn).** `agent_end` is
///   omp's turn-end signal, and a `will_continue: true` on it is corrected in
///   [`normalize_event`] to [`HookEvent::PostToolUse`] rather than settling
///   the row — see the module doc for why that correction is load-bearing.
/// - **`session_switch`, not `session_info_changed` (gone).** omp emits it on
///   `/new`, `/fork` and `/resume`, and it is where a mid-session session-id
///   change becomes visible to `r`/`f`.
/// - **`tool_approval_requested` / `tool_approval_resolved` — omp's per-tool
///   approval gate, which pi has none of.** Both fire whenever a handler is
///   registered, and neither return value can alter the turn, so the pair is
///   what makes `approval_gate: true` honest.
/// - **`tool_execution_start` / `tool_execution_end`, not `tool_call` /
///   `tool_result`** for pi's reason: the pair whose return value cannot block
///   or rewrite a tool, and `tool_execution_end` carries `isError` besides.
///
/// No [`HookEvent::StopFailure`], for pi's reason: omp's surface has no
/// run-failed event, so `agent_end` covers it and the row goes `Idle` with no
/// error text.
///
/// Deliberately not registered: `agent_start` / `turn_*` / `message_*` /
/// `context` / `before_provider_*` / `after_provider_response` (too
/// fine-grained), `session_shutdown` (the launcher owns exit),
/// `session_before_switch` / `session_before_fork` / `session_before_tree` /
/// `session_branch` / `session_tree` (a replacement session re-emits
/// `session_start`, which we do register), `model_select` and
/// `thinking_level_select` (the model rides every payload already),
/// `project_trust` / `resources_discover` (never invoked by any session
/// callsite), `user_bash` / `user_python`, `todo_reminder`,
/// `ttsr_triggered`, `credential_disabled`, `mcp_notification`, and omp's
/// new fine-grained events `auto_compaction_*` and `auto_retry_*` (each is a
/// `willContinue: true` branch that `agent_end` already covers). And
/// **`session_stop`** — see the module doc for why it must not be subscribed
/// to despite existing.
const FORWARDED: &[(&str, HookEvent)] = &[
    ("session_start", HookEvent::SessionStart),
    ("session_switch", HookEvent::SessionStart),
    ("before_agent_start", HookEvent::PromptSubmit),
    ("tool_execution_start", HookEvent::PreToolUse),
    ("tool_execution_end", HookEvent::PostToolUse),
    ("tool_approval_requested", HookEvent::PermissionRequest),
    ("tool_approval_resolved", HookEvent::ElicitationResult),
    ("agent_end", HookEvent::Stop),
    ("session_before_compact", HookEvent::PreCompact),
    ("session_compact", HookEvent::PostCompact),
];

// =============================================================================
// The generated extension
// =============================================================================

/// Where the generated extension lives: **one shared file per machine**, not
/// one per session.
///
/// Shared because it *can* be — the socket rides `$CAPTAIN_MIAO_SOCK`, so two
/// sessions want byte-identical files — and because it must not be per-session
/// here specifically. The launcher's own per-session payload is named
/// `<pid>-settings.json`, and a sibling `<pid>.ts` would work for one launch
/// and then leak forever: the launcher's cleanup and its dead-launcher sweep
/// both key on `.sock` / `-settings.json` by name.
///
/// So the launcher's file stays the transport it always was — it carries the
/// source, [`build_launch_command`] reads it back — and the copy the agent is
/// actually handed lands here, under a name that never accumulates. A distinct
/// name from `pi-extension.ts`, which already exists in that directory on a
/// machine that has run pi.
///
/// omp loads this with **Bun**, not pi's jiti — so the `.ts` suffix is no
/// longer a loader requirement — but it is kept anyway for the naming reason
/// above. See the module doc.
fn extension_path() -> PathBuf {
    crate::state::state_dir().join("omp-extension.ts")
}

/// Write the extension to [`extension_path`] and return it, **only when its
/// bytes would change**. Two concurrent launches then never race a half-written
/// file, and an ordinary launch does no write at all.
fn ensure_extension(source: &str) -> Result<PathBuf> {
    let path = extension_path();
    if let Some(parent) = path.parent() {
        crate::state::create_dir_all_private(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let unchanged = std::fs::read_to_string(&path)
        .map(|cur| cur == source)
        .unwrap_or(false);
    if !unchanged {
        atomic_write(&path, source.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

/// The extension source itself. Pure, and the **only** thing spliced into it
/// is `miao_exe` — which is why the file is identical for every session on a
/// machine, and why the snapshot test in this module is worth as much as it is.
///
/// The splice is JSON-encoded rather than shell-quoted: a JSON string literal
/// is a JavaScript string literal, and nothing here reaches a shell (`spawn`
/// runs the binary directly, no `shell: true`), so a path with a quote, a
/// backslash or a space needs no other handling.
///
/// It is written as **plain JavaScript that happens to be valid TypeScript** —
/// no type annotations, no imports beyond a Node builtin. The `.ts` suffix is
/// kept for [`extension_path`]'s naming reason, not a request for type syntax
/// we cannot check.
///
/// The one arithmetic in the file is `Math.round` on the token count, for
/// pi's reason: `getContextUsage()` is an estimate, a fractional value would
/// fail [`HookMessage::context_tokens`]'s `u64`, and `Math.round(undefined)`
/// is `NaN`, which serializes to `null` and reads back as "not reported".
///
/// Three content changes from pi's template: the header names omp and `omp -e`;
/// the argv is `["hook", "--agent", "omp", forwarded]`; and the payload gains
/// one field, `will_continue: event?.willContinue` — the value the
/// [`normalize_event`] correction keys on, and the one thing the extension
/// forwards that pi's does not.
fn extension_source(miao_exe: &str) -> String {
    let exe = serde_json::to_string(miao_exe).unwrap_or_else(|_| "\"miao\"".to_string());
    let table = FORWARDED
        .iter()
        .map(|(native, forwarded)| format!("  [\"{native}\", \"{}\"],\n", forwarded.as_kebab()))
        .collect::<String>();
    format!(
        r#"// captain-miao's Oh My Pi (omp) session forwarder — GENERATED. Edits are
// overwritten on the next launch.
//
// Loaded with `omp -e <this file>`: a CLI extension, trusted by virtue of being
// on the command line and scoped to the run that named it.
//
// It carries no logic of its own. For each omp event in FORWARD it builds one
// fixed payload and runs `miao hook --agent omp <event>`, writing that payload to
// the child's stdin. What a status means, how a failed tool is classified, and
// whether an `agent_end` settles the row are decided in captain-miao — never
// here. The launcher socket arrives in the environment as $CAPTAIN_MIAO_SOCK, so
// this file is identical for every session on this machine.
import {{ spawn }} from "node:child_process";

// captain-miao's own executable, resolved when this file was written.
const MIAO = {exe};

// omp event -> the captain-miao hook event it is forwarded as.
const FORWARD = [
{table}];

export default function (pi) {{
  for (const [name, forwarded] of FORWARD) {{
    // An omp that renamed or dropped an event must cost that one transition, not
    // the whole extension: an exception here would leave the session untracked
    // with nothing anywhere to read.
    try {{
      pi.on(name, (event, ctx) => send(forwarded, pi, event, ctx));
    }} catch {{}}
  }}
}}

// One shape for every event: fields the event doesn't carry come out undefined,
// and JSON.stringify drops them. It resolves with no value, so an event whose
// handler contract is an optional patch (`before_agent_start`) reads it as "no
// change", and it never rejects — a forwarder that threw would surface on the
// user's turn.
function send(forwarded, pi, event, ctx) {{
  let body = "{{}}";
  try {{
    body = JSON.stringify({{
      session_id: ctx?.sessionManager?.getSessionId?.(),
      session_title: pi?.getSessionName?.(),
      cwd: ctx?.cwd,
      tool_name: event?.toolName,
      prompt: event?.prompt,
      is_error: event?.isError,
      will_continue: event?.willContinue,
      context_tokens: Math.round(ctx?.getContextUsage?.()?.tokens),
      model: ctx?.model?.id,
    }});
  }} catch {{
    // A getter that threw must not cost the event: the event name is in the
    // argv, so an empty payload still moves the row.
  }}
  return new Promise((resolve) => {{
    let child;
    try {{
      child = spawn(MIAO, ["hook", "--agent", "omp", forwarded], {{
        stdio: ["pipe", "ignore", "ignore"],
      }});
    }} catch {{
      resolve();
      return;
    }}
    child.on("error", () => resolve());
    child.on("close", () => resolve());
    child.stdin.on("error", () => {{}});
    child.stdin.end(body);
  }});
}}
"#
    )
}

/// The "hook settings" the launcher writes to its per-session file — for omp,
/// **TypeScript source**, not JSON. The path is generic transport and its
/// contents are opaque to the launcher, so each backend puts its own format
/// through it (Kimi already puts TOML through the same channel).
///
/// `sock_path` is ignored, as it is for pi and for the same reason: the file
/// is shared by every session, so it cannot carry a per-session path. The
/// socket reaches the hook through the environment instead, and that trip
/// needs no faith — our forwarder spawns the child itself with `spawn`'s
/// default inherited environment, from inside the omp process we set
/// `CAPTAIN_MIAO_SOCK` on.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    extension_source(&exe.to_string_lossy())
}

// =============================================================================
// Launcher: process spawn
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    // The launcher already wrote our extension source to `settings_path`;
    // relocate it to a `.ts` path omp's loader will accept (see
    // [`extension_path`]). Note the file the launcher wrote is named
    // `…-settings.json` and holds **TypeScript** — that path is generic
    // transport, opaque to the launcher.
    let source = std::fs::read_to_string(settings_path).context("reading omp hook extension")?;
    let extension = ensure_extension(&source)?;

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    // Read by `miao hook` (spawned from the extension, which inherits this
    // process's environment). The extension file is shared by every session and
    // so cannot carry the path itself.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    cmd.args(launch_args(&extension, extra_args));
    Ok(cmd)
}

/// The agent-facing argv: our extension, then whatever the launcher forwarded
/// (`--resume <id>`, `--fork <id>`).
///
/// **No directory argument of any kind.** omp's trailing positionals are
/// prompt messages (`omp [COMMAND] MESSAGES`), so `cwd` reaches omp as the
/// spawned process's working directory. omp *does* have a `--cwd` flag pi
/// lacked; it is deliberately not used, because the process cwd already says
/// it and a second source could disagree — Reasonix is the standing reminder
/// of what a positional cwd costs: a session whose first user message is a
/// path.
fn launch_args(extension: &Path, extra: &[String]) -> Vec<String> {
    let mut v = vec!["-e".to_string(), extension.to_string_lossy().into_owned()];
    v.extend(extra.iter().cloned());
    v
}

// =============================================================================
// Hook payload (stdin from the extension → normalized HookMessage)
// =============================================================================

/// The payload our own forwarder sends. Unlike every other backend's, this
/// struct describes a shape captain-miao **writes** rather than one an agent
/// happens to emit — `extension_source` builds it and this parses it, so the
/// two are pinned together by the tests below rather than by a vendor's docs.
///
/// snake_case, matching the launcher's own wire vocabulary; the JavaScript
/// names these keys explicitly rather than dumping an omp event object, because
/// omp's event payloads carry `AbortSignal`s and message graphs that
/// `JSON.stringify` would either flatten to `{}` or refuse outright.
///
/// The omp-side sources, one per field: `ctx.sessionManager.getSessionId()`,
/// `pi.getSessionName()`, `ctx.cwd`, `event.toolName` (the tool-execution
/// events), `event.prompt` (`before_agent_start`), `event.isError`
/// (`tool_execution_end`), `event.willContinue` (`agent_end`),
/// `ctx.getContextUsage().tokens` and `ctx.model.id`.
#[derive(Deserialize)]
struct HookPayload {
    session_id: Option<String>,
    session_title: Option<String>,
    cwd: Option<String>,
    tool_name: Option<String>,
    prompt: Option<String>,
    context_tokens: Option<u64>,
    model: Option<String>,
    /// Set on a `tool_execution_end` whose tool failed.
    #[serde(default)]
    is_error: bool,
    /// Set on an `agent_end` that omp will follow with more work of its own.
    #[serde(default)]
    will_continue: bool,
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse omp hook JSON from stdin")?;
    Ok(HookMessage {
        event: normalize_event(event, &payload),
        session_id: payload.session_id,
        tool_name: payload.tool_name,
        // No error text is read. `tool_execution_end` carries a `result`, but
        // its shape is per-tool and the failure arm doesn't surface a message
        // anyway; `raw` holds the whole payload for anyone who needs it.
        message: None,
        cwd: payload.cwd,
        prompt: payload.prompt,
        // All three ride every payload — see the module doc for why they come
        // from the hook rather than from a transcript fold.
        session_title: payload.session_title,
        context_tokens: payload.context_tokens,
        model: payload.model,
        // The only omp event carrying a transcript path is `session_stop`'s
        // `session_file`, and we do not subscribe to it — so no path reaches
        // the launcher and its whole transcript pipeline (stats fold, signal
        // scan, stat poll) stays inert by construction, with tokens and model
        // coming off the hook as their single source.
        transcript_path: None,
        raw: Some(stdin.to_string()),
        session_is_child: None,
    })
}

/// Two payload-driven event corrections, one cosmetic and one load-bearing.
///
/// The `is_error` arm is pi's, cosmetic today and kept anyway: a failed
/// `tool_execution_end` is spelled [`HookEvent::PostToolUseFailure`], and
/// `dispatch_default` settles the two identically so nothing on the row moves
/// differently — but the fact is on the payload and this is where the
/// correction belongs the day the two arms diverge.
///
/// The `will_continue` arm is the load-bearing one and the reason this module
/// exists separately from pi's. `agent_end` fires at the end of *every* agent
/// loop, and omp reports on the event whether it will keep running (auto-retry,
/// auto-compaction, a queued follow-up, a `session_stop` continuation). A
/// settle on one of those would read `Idle` while the agent works.
/// [`HookEvent::PostToolUse`] is the arm that means exactly "a unit of work
/// ended, the session is still `Active`, nothing is in flight" —
/// `dispatch_default` sets `status = Active; last_tool = None` — so the
/// correction is a rename in Rust rather than a conditional in the generated
/// JavaScript, which keeps "the extension carries no logic" true. A wrong
/// answer here is a wrong row.
fn normalize_event(event: HookEvent, payload: &HookPayload) -> HookEvent {
    match event {
        HookEvent::PostToolUse if payload.is_error => HookEvent::PostToolUseFailure,
        HookEvent::Stop if payload.will_continue => HookEvent::PostToolUse,
        other => other,
    }
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// omp departs from [`common::dispatch_default`] nowhere. The native →
/// normalized renaming is done in the generated table ([`FORWARDED`]) rather
/// than here, and the two payload-driven corrections are done in
/// [`normalize_event`]. `agent_end` with a falsy `willContinue` means the
/// shared `Stop` arm needs no help from a session file or a rollout scan, and
/// the abort path settles too.
///
/// The wrapper stays so the seam keeps one callee per backend, and so the day
/// omp grows a case of its own it has a place to land.
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

    /// A sanitized stand-in for the resolved `miao` path. Every snapshot below
    /// is written against this, so the tests are independent of where the test
    /// binary happens to live.
    const EXE: &str = "/home/miao/.local/bin/miao";

    /// A payload in the shape our own forwarder builds. Hand-written from
    /// [`extension_source`] rather than captured — but unlike the other
    /// backends' fixtures, the thing it mirrors is *our* code, so it can only
    /// drift by someone editing the template.
    fn payload(extra: &str) -> String {
        format!(
            r#"{{"session_id":"s1","session_title":"wire up the parser",
                "cwd":"/home/miao/p","context_tokens":48100,"model":"some-model-1"{extra}}}"#
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::Omp, status)
    }

    /// Drive one hook end to end — parse the extension's stdin JSON, then
    /// dispatch it — so the tests exercise the same path a live hook takes,
    /// including the event normalization that only happens in the parser.
    fn feed(state: &mut LauncherState, event: HookEvent, stdin: &str) {
        let msg = parse_hook_payload(event, stdin).expect("payload parses");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(dispatch_hook(state, msg));
    }

    /// **The snapshot.** captain-miao ships this JavaScript into a tree that
    /// cannot run it, so the only defence against a template edit that breaks
    /// the file is that changing it at all fails here — loudly, with the diff in
    /// the assertion. Read the new text before updating the expectation.
    #[test]
    fn the_extension_source_is_byte_stable() {
        let expected = r#"// captain-miao's Oh My Pi (omp) session forwarder — GENERATED. Edits are
// overwritten on the next launch.
//
// Loaded with `omp -e <this file>`: a CLI extension, trusted by virtue of being
// on the command line and scoped to the run that named it.
//
// It carries no logic of its own. For each omp event in FORWARD it builds one
// fixed payload and runs `miao hook --agent omp <event>`, writing that payload to
// the child's stdin. What a status means, how a failed tool is classified, and
// whether an `agent_end` settles the row are decided in captain-miao — never
// here. The launcher socket arrives in the environment as $CAPTAIN_MIAO_SOCK, so
// this file is identical for every session on this machine.
import { spawn } from "node:child_process";

// captain-miao's own executable, resolved when this file was written.
const MIAO = "/home/miao/.local/bin/miao";

// omp event -> the captain-miao hook event it is forwarded as.
const FORWARD = [
  ["session_start", "session-start"],
  ["session_switch", "session-start"],
  ["before_agent_start", "prompt-submit"],
  ["tool_execution_start", "pre-tool-use"],
  ["tool_execution_end", "post-tool-use"],
  ["tool_approval_requested", "permission-request"],
  ["tool_approval_resolved", "elicitation-result"],
  ["agent_end", "stop"],
  ["session_before_compact", "pre-compact"],
  ["session_compact", "post-compact"],
];

export default function (pi) {
  for (const [name, forwarded] of FORWARD) {
    // An omp that renamed or dropped an event must cost that one transition, not
    // the whole extension: an exception here would leave the session untracked
    // with nothing anywhere to read.
    try {
      pi.on(name, (event, ctx) => send(forwarded, pi, event, ctx));
    } catch {}
  }
}

// One shape for every event: fields the event doesn't carry come out undefined,
// and JSON.stringify drops them. It resolves with no value, so an event whose
// handler contract is an optional patch (`before_agent_start`) reads it as "no
// change", and it never rejects — a forwarder that threw would surface on the
// user's turn.
function send(forwarded, pi, event, ctx) {
  let body = "{}";
  try {
    body = JSON.stringify({
      session_id: ctx?.sessionManager?.getSessionId?.(),
      session_title: pi?.getSessionName?.(),
      cwd: ctx?.cwd,
      tool_name: event?.toolName,
      prompt: event?.prompt,
      is_error: event?.isError,
      will_continue: event?.willContinue,
      context_tokens: Math.round(ctx?.getContextUsage?.()?.tokens),
      model: ctx?.model?.id,
    });
  } catch {
    // A getter that threw must not cost the event: the event name is in the
    // argv, so an empty payload still moves the row.
  }
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(MIAO, ["hook", "--agent", "omp", forwarded], {
        stdio: ["pipe", "ignore", "ignore"],
      });
    } catch {
      resolve();
      return;
    }
    child.on("error", () => resolve());
    child.on("close", () => resolve());
    child.stdin.on("error", () => {});
    child.stdin.end(body);
  });
}
"#;
        assert_eq!(extension_source(EXE), expected);
    }

    /// The generated table must be the module's [`FORWARDED`] claim rendered,
    /// and every event name in it must be one the launcher can parse back — a
    /// `miao hook --agent omp <name>` the CLI rejects is a status silently
    /// lost.
    #[test]
    fn the_registered_events_are_exactly_what_the_module_claims() {
        // The native names, in the order omp sees them registered.
        assert_eq!(
            FORWARDED.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [
                "session_start",
                "session_switch",
                "before_agent_start",
                "tool_execution_start",
                "tool_execution_end",
                "tool_approval_requested",
                "tool_approval_resolved",
                "agent_end",
                "session_before_compact",
                "session_compact",
            ]
        );
        // **Only `agent_end` becomes `Stop`.** That is the whole turn-end
        // design: nothing else may claim the turn is over.
        assert_eq!(
            FORWARDED
                .iter()
                .filter(|(_, e)| *e == HookEvent::Stop)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>(),
            ["agent_end"]
        );

        let source = extension_source(EXE);
        for (native, forwarded) in FORWARDED {
            let row = format!("  [\"{native}\", \"{}\"],", forwarded.as_kebab());
            assert!(source.contains(&row), "missing {row} in the emitted source");
            // The argv spelling has to survive the round trip the hook CLI does.
            assert_eq!(
                HookEvent::from_kebab(forwarded.as_kebab()),
                Some(*forwarded)
            );
        }
        // Nothing else is registered: the table has exactly these rows, and the
        // single `pi.on` in the file is the loop over it.
        let rows = source
            .lines()
            .filter(|l| l.starts_with("  [\"") && l.ends_with("\"],"))
            .count();
        assert_eq!(rows, FORWARDED.len());
        assert_eq!(source.matches("pi.on(").count(), 1);
    }

    /// One file serves every session, so it must carry no per-session data. The
    /// socket is the thing that would be tempting to splice and must not be.
    #[test]
    fn the_source_carries_no_per_session_data() {
        let a = build_hooks_settings("/run/user/1000/captain-miao/launchers/1.sock");
        let b = build_hooks_settings("/run/user/1000/captain-miao/launchers/2.sock");
        assert_eq!(a, b, "the extension must not embed the per-session socket");
        assert!(!a.contains(".sock"));
        // …and it reads the socket from the environment instead, which is the
        // only reason the file can be shared at all.
        assert!(a.contains("$CAPTAIN_MIAO_SOCK"));
    }

    /// The one spliced value. It is JSON-encoded because a JSON string literal
    /// is a JavaScript one — and it never reaches a shell, so this is the whole
    /// of the quoting story.
    #[test]
    fn the_exe_path_is_spliced_as_a_javascript_string_literal() {
        let source = extension_source(r#"/home/miao/od"d\path/miao"#);
        assert!(
            source.contains(r#"const MIAO = "/home/miao/od\"d\\path/miao";"#),
            "{source}"
        );
        // The argv is built as an array and handed to `spawn` directly, so
        // nothing is ever concatenated into a command line.
        assert!(!source.contains("shell: true"));
        assert!(source.contains(r#"spawn(MIAO, ["hook", "--agent", "omp", forwarded]"#));
    }

    /// A turn runs prompt → tool → settle, and a final `agent_end` is what ends
    /// it.
    #[test]
    fn a_turn_runs_from_prompt_to_settled() {
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

        feed(&mut state, HookEvent::PostToolUse, &payload(""));
        assert_eq!(state.last_tool, None);

        feed(&mut state, HookEvent::Stop, &payload(""));
        assert_eq!(state.status, SessionStatus::Idle);
    }

    /// Title, tokens and model ride *every* payload, which is what replaces a
    /// transcript fold, a title store and a sqlite overlay all at once.
    #[test]
    fn every_payload_carries_the_title_tokens_and_model() {
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::PreToolUse, &payload(""));
        assert_eq!(state.name.as_deref(), Some("wire up the parser"));
        assert_eq!(state.context_tokens, Some(48_100));
        assert_eq!(state.model.as_deref(), Some("some-model-1"));

        // A rename is just a later payload carrying a different title.
        let renamed = r#"{"session_id":"s1","session_title":"renamed by the user"}"#;
        feed(&mut state, HookEvent::Stop, renamed);
        assert_eq!(state.name.as_deref(), Some("renamed by the user"));
        // …and that payload said nothing about tokens, which must not blank them.
        assert_eq!(state.context_tokens, Some(48_100));
    }

    /// `isError` on a `tool_execution_end` is one payload-driven event
    /// correction, and the empty-payload fallback the forwarder falls back to
    /// must still parse.
    #[test]
    fn a_failed_tool_is_normalized_to_the_failure_event() {
        let failed = parse_hook_payload(
            HookEvent::PostToolUse,
            &payload(r#","tool_name":"bash","is_error":true"#),
        )
        .expect("parses");
        assert_eq!(failed.event, HookEvent::PostToolUseFailure);

        // Without the flag it stays the plain event…
        let ok = parse_hook_payload(HookEvent::PostToolUse, &payload(r#","tool_name":"bash""#))
            .expect("parses");
        assert_eq!(ok.event, HookEvent::PostToolUse);
        // …and the correction is confined to that one event.
        let stopped =
            parse_hook_payload(HookEvent::Stop, &payload(r#","is_error":true"#)).expect("parses");
        assert_eq!(stopped.event, HookEvent::Stop);

        // The forwarder sends `{}` when building the payload throws, so the
        // status still lands even with nothing else to say.
        let bare = parse_hook_payload(HookEvent::Stop, "{}").expect("an empty payload parses");
        assert_eq!(bare.event, HookEvent::Stop);
        assert_eq!(bare.session_id, None);

        // The two shapes a token count actually arrives in. `null` is what
        // `Math.round(undefined)` serializes to when omp reports no usage yet,
        // and it must read as "not reported" rather than failing the payload —
        // which would take the *status* down with it, not just the number.
        let unusable = parse_hook_payload(HookEvent::Stop, r#"{"context_tokens":null}"#)
            .expect("a null token count parses");
        assert_eq!(unusable.context_tokens, None);
    }

    /// No transcript path, ever — the field the launcher gates its whole
    /// transcript watch on. The only omp event carrying one is `session_stop`,
    /// which we do not subscribe to, so no path reaches the launcher and the
    /// hook stays the single source for tokens and model.
    #[test]
    fn no_payload_ever_names_a_transcript() {
        for stdin in [
            payload(""),
            payload(r#","transcript_path":"/home/miao/s.jsonl""#),
        ] {
            let msg = parse_hook_payload(HookEvent::Stop, &stdin).expect("parses");
            assert_eq!(msg.transcript_path, None);
        }
    }

    /// The extension goes in as `-e`, and **nothing positional follows it**:
    /// omp's trailing positionals are prompts, so a cwd there would open a
    /// session whose first user message is a path.
    #[test]
    fn the_argv_names_the_extension_and_no_directory() {
        let ext = Path::new("/home/miao/.local/state/captain-miao/omp-extension.ts");
        assert_eq!(
            launch_args(ext, &[]),
            [
                "-e",
                "/home/miao/.local/state/captain-miao/omp-extension.ts"
            ]
        );
        assert_eq!(
            launch_args(ext, &["--resume".to_string(), "s1".to_string()]),
            [
                "-e",
                "/home/miao/.local/state/captain-miao/omp-extension.ts",
                "--resume",
                "s1"
            ]
        );
    }

    /// The relocated path ends in `.ts` — kept for the naming reason in
    /// [`extension_path`]'s doc, not a loader requirement (omp loads with Bun,
    /// not jiti) — and is shared, not per-session.
    #[test]
    fn the_extension_path_is_a_ts_file() {
        let path = extension_path();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("ts"));
        // Shared, not per-session: nothing in the name may vary per launch, or
        // the launcher's cleanup (which knows only `.sock` and
        // `-settings.json`) would leak one file per session forever.
        assert!(
            !path
                .to_string_lossy()
                .contains(&std::process::id().to_string())
        );
    }

    /// **The one that pins divergence 1.** An `agent_end` with
    /// `will_continue: true` is omp saying "I will keep running", so it must
    /// not settle the row. It normalizes to `PostToolUse` — the arm that means
    /// "work ended, still `Active`, nothing in flight" — and the correction is
    /// confined to that event.
    #[test]
    fn a_continuing_agent_end_is_not_a_settle() {
        // `will_continue: true` → the event is renamed to `PostToolUse`.
        let continuing =
            parse_hook_payload(HookEvent::Stop, r#"{"will_continue":true}"#).expect("parses");
        assert_eq!(continuing.event, HookEvent::PostToolUse);

        // Without the flag it stays `Stop` — the genuinely-final turn end.
        let final_end =
            parse_hook_payload(HookEvent::Stop, r#"{"will_continue":false}"#).expect("parses");
        assert_eq!(final_end.event, HookEvent::Stop);

        // The correction is confined to `Stop`: a `PostToolUse` carrying the
        // flag is not rewritten (it is already the arm the correction targets).
        let tool = parse_hook_payload(HookEvent::PostToolUse, r#"{"will_continue":true}"#)
            .expect("parses");
        assert_eq!(tool.event, HookEvent::PostToolUse);

        // Driven through `dispatch_hook` from `Active`: the continuing one
        // leaves the row `Active`, the final one lands `Idle`.
        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, r#"{"will_continue":true}"#);
        assert_eq!(
            state.status,
            SessionStatus::Active,
            "a continuing agent_end must not settle the row"
        );

        let mut state = state_at(SessionStatus::Active);
        feed(&mut state, HookEvent::Stop, r#"{"will_continue":false}"#);
        assert_eq!(
            state.status,
            SessionStatus::Idle,
            "a final agent_end settles the row"
        );
    }

    /// **The one that makes `approval_gate: true` honest.** omp has a per-tool
    /// approval gate pi has none of: `tool_approval_requested` holds the row,
    /// and `tool_approval_resolved` releases it — on approve *and* on deny,
    /// since the resolved event fires after either.
    #[test]
    fn an_approval_holds_the_row_and_its_resolution_releases_it() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &payload(r#","tool_name":"bash""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);

        // Resolution fires after approve or deny alike, and either releases the
        // row back to `Active`.
        feed(&mut state, HookEvent::ElicitationResult, &payload(""));
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// **The point of registering `session_switch`.** omp emits it on `/new`,
    /// `/fork` and `/resume`, and an in-session `/resume` or `/fork` must move
    /// the session id — `r`/`f` resume from `state.session_id`. A `SessionStart`
    /// payload carrying a different id while the row is `Idle` replaces it and
    /// leaves `status` at `Idle`.
    #[test]
    fn a_session_switch_delivers_the_new_session_id() {
        let mut state = state_at(SessionStatus::Idle);
        state.session_id = Some("old-id".to_string());
        feed(
            &mut state,
            HookEvent::SessionStart,
            r#"{"session_id":"new-id","session_title":"forked"}"#,
        );
        assert_eq!(state.session_id.as_deref(), Some("new-id"));
        assert_eq!(
            state.status,
            SessionStatus::Idle,
            "a session_switch out of Idle moves the id, not the status"
        );
        // The title rides the same payload, so it is adopted too.
        assert_eq!(state.name.as_deref(), Some("forked"));
    }
}
