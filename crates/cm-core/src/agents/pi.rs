//! Pi CLI backend. Owns every Pi-specific path, argv flag and hook payload
//! shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Pi`'s match arms.
//!
//! **Source-verified against `earendil-works/pi@main`, never run.** No `pi`
//! binary was available when this was written. Every claim below is cited to the
//! doc it came from — `packages/coding-agent/docs/{extensions,usage,sessions,
//! session-format,security}.md` — so a later probe knows which file to re-read
//! rather than which guess to re-derive. What a probe still has to settle is at
//! the bottom of this doc.
//!
//! Pi is the first backend that is **neither a synthetic home nor a shell
//! hook**, and the two facts that follow are the whole reason it exists here.
//!
//! # Injection is an argv flag, so there is no synthetic home
//!
//! `pi -e <path.ts>` loads an extension from an arbitrary path for that run
//! (`usage.md`: *"`-e, --extension <source>` — Load an extension from path, npm,
//! or git; repeatable"*). That is Claude's `--settings` shape, not Codex's
//! `CODEX_HOME` one: per-run, torn down with the process, nothing of the user's
//! touched. **Pi needs no [`super::synth_home::SynthHome`] at all** — the only
//! backend of the five that doesn't.
//!
//! Trust is not in the way either, which is the fact that makes the flag usable
//! rather than merely available. `project_trust` fires *before* project-local
//! extensions load, and `extensions.md` is explicit: *"Only user/global
//! extensions and CLI `-e` extensions participate; project-local extensions are
//! not loaded until after trust is resolved."* A `-e` extension is trusted by
//! virtue of being on the command line, so there is **no seeding step** — no
//! `seed_hook_trust`, no content hash to keep stable for its sake, none of what
//! Codex pays for.
//!
//! # The file is generated TypeScript, and it is contained on purpose
//!
//! Pi has no shell-command hooks; its event surface is an extension module. So
//! [`build_hooks_settings`] returns **TypeScript source** rather than JSON —
//! the seam calls it "the per-session hook-settings file", and nothing requires
//! its contents to be JSON (Kimi already puts TOML through the same channel).
//! captain-miao therefore ships generated JavaScript in a tree with no JS
//! toolchain, which is a real, nameable cost. It is contained three ways:
//!
//! - **The extension carries no logic.** It holds one table ([`FORWARDED`],
//!   rendered into the source) and one function that serializes a fixed payload
//!   and spawns `miao hook --agent pi <event>`. No filtering, no state, no
//!   retries. Every decision — what a status means, how a failed tool is
//!   classified — is made in Rust, below.
//! - **The socket arrives via `$CAPTAIN_MIAO_SOCK`**, never spliced into the
//!   source, so the file is byte-identical for every session on a machine.
//!   That is what lets [`ensure_extension`] keep **one shared file** instead of
//!   one per launch (see it for why that matters more here than elsewhere), and
//!   it means nothing in the TypeScript needs shell quoting: `spawn(MIAO,
//!   [args])` runs no shell at all.
//! - **Tests cover generation, not execution.** This tree cannot run the file,
//!   so [`extension_source`] is pure and pinned by a full-text snapshot; a
//!   changed template fails loudly rather than silently shipping.
//!
//! # `agent_settled` is the turn-end signal, and it removes two mechanisms
//!
//! `extensions.md` documents it for exactly this use: *"`agent_end` fires when
//! that run ends, but Pi may still auto-retry, auto-compact and retry, or
//! continue with queued follow-up messages. Use `agent_settled` for status
//! integrations that need to know Pi will not continue running automatically."*
//!
//! So `agent_settled` maps to [`HookEvent::Stop`] and **nothing else does**.
//! Two absences fall out of that, and both are absences rather than gaps:
//!
//! - **No interrupt detection.** A settled interrupt is still "Pi will not
//!   continue", so it settles too — which is the case that costs Codex a
//!   rollout sentinel and Claude a session-file fold.
//!   [`crate::agent::AgentControl::scan_transcript_signals`] is the empty
//!   default and `agent_activity` is `None` with nothing lost. (That
//!   `agent_settled` really does fire on a cancelled run is the one claim here
//!   the docs imply rather than state — see the probe list.)
//! - **[`crate::state::SessionStatus::WaitingForApproval`] is structurally
//!   unreachable.** `security.md`: *"Pi does not include a built-in sandbox.
//!   Built-in tools can read files, write files, edit files, and run shell
//!   commands with the permissions of the pi process."* Its `--approve` / trust
//!   machinery is **project-trust** — a guard on loading settings and
//!   extensions — not a per-tool gate. There is no approval prompt to reflect
//!   and no second mechanism to go find, so nothing is being left on the table.
//!   Noted here so nobody re-derives it.
//!
//! The one moment a Pi session *does* block on the user is the built-in project
//! trust prompt at startup, and it is deliberately not surfaced: it fires before
//! `session_start`, so the row is still `Starting` and there is no paired
//! "answered" event to leave that state on. A Pi row that never leaves
//! `Starting` is therefore more likely waiting on that prompt than missing a
//! hook — which the README says out loud, because the two look identical from
//! the dashboard.
//!
//! # Tokens, model and title come off the hook — not the transcript
//!
//! `ctx.getContextUsage()`, `ctx.model` and `pi.getSessionName()` are all
//! reachable inside a handler, so the extension puts the numbers on every
//! payload and [`common::adopt_session_facts`] stamps them onto the row. No
//! title store, no sqlite overlay, no `out_of_band_watch_paths` entry.
//!
//! That is a *choice against* a transcript fold, not an inability to do one:
//! `session-format.md` documents the JSONL schema outright (assistant entries
//! carry `provider`, `model`, `usage.totalTokens` and `stopReason`). The reason
//! is that **the session file is a tree, not a log** — `sessions.md`: *"Every
//! entry has an `id` and `parentId`, and the current position is the active
//! leaf."* A naive tail reads the last *appended* entry, which after a `/tree`
//! jump belongs to an abandoned branch, so a correct fold has to walk `parentId`
//! back from the active leaf on every read. The hook route sidesteps that
//! entirely, and `adopt_session_facts` is explicit that one fact gets one
//! source: a backend reports them **or** folds them, never both. Hence
//! [`read_transcript_stats`] returns the default and `transcript_path` is
//! `None`, which also keeps the launcher's whole transcript pipeline (stats
//! fold, signal scan, stat poll) inert for Pi by construction.
//!
//! [`read_transcript_stats`]: crate::agent::AgentControl::read_transcript_stats
//!
//! # What a probe against a real binary must settle
//!
//! - **That every name in [`FORWARDED`] is still a live Pi event.** The
//!   registration loop swallows a throwing `pi.on` so one renamed event costs
//!   one transition instead of the whole extension — which is the right trade
//!   and also the reason a rename is *invisible*. Run a full session (prompt →
//!   tool → compact → settle) with the launcher log open and check all eight
//!   arrive.
//! - **That `agent_settled` fires on an interrupted run.** Press Esc mid-turn.
//!   The whole no-interrupt-detection claim rests on it; if it doesn't fire, a
//!   Pi row stays `Active` until the next prompt (Grok's standing defect).
//! - **That `ctx.sessionManager.getSessionId()` is the id `--session <id>`
//!   accepts.** `usage.md` documents `--session <path|id>` as taking a "partial
//!   UUID"; that the SessionManager's id is that UUID is implied by the naming
//!   and not stated. If they differ, resume is aimed at the wrong namespace.
//! - **That a `-e` extension outside any package resolves `node:child_process`.**
//!   `extensions.md` says extensions run on Node with builtins available and
//!   *"No native Bun.spawn: uses Node.js `child_process` API only"*, and jiti
//!   loads the file without compilation — but our file has no `package.json`
//!   beside it, which no doc explicitly blesses.
//! - **Whether an `undefined` return from `before_agent_start` /
//!   `session_before_compact` is accepted as "no change".** Both are documented
//!   as returning an optional patch object, so it should be; a wrong guess here
//!   would break the user's turn rather than merely lose a status.
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
use super::find_in_path;
use super::synth_home::atomic_write;
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "pi";

// =============================================================================
// The event table — the module's claim about what it subscribes to
// =============================================================================

/// Every Pi event the generated extension subscribes to, paired with the
/// [`HookEvent`] it is forwarded as. This const **is** the claim; the emitted
/// JavaScript renders it, so the two cannot disagree.
///
/// The four choices in here that aren't 1:1:
///
/// - **`tool_execution_start` / `tool_execution_end`, not `tool_call` /
///   `tool_result`.** All four fire on every tool (`extensions.md`'s lifecycle
///   diagram brackets the inner pair with the outer one), but `tool_call`
///   *"can block"* — its return value is `{block, reason, terminate}` — and
///   `tool_result` *"can modify"* the result. Subscribing to the pair whose
///   return value cannot alter the user's turn is the safer half of a free
///   choice, and `tool_execution_end` carries `isError` besides.
/// - **`before_agent_start`, not `input`, for [`HookEvent::PromptSubmit`].**
///   `input` fires earlier but is an interception point: its handler contract is
///   `{action: "continue" | "transform" | "handled"}`, and it also sees text
///   that never becomes a turn. `before_agent_start` is documented as *"Fired
///   after user submits prompt, before agent loop"*, carries `event.prompt`, and
///   returns an optional patch — so it means exactly "the turn started" and
///   costs nothing to answer.
/// - **`session_info_changed` forwarded as [`HookEvent::SessionStart`].** It
///   carries a rename and no status of its own; every payload we send already
///   carries the title, so the point of registering it at all is to deliver a
///   rename that is followed by *nothing else*. `SessionStart` is the one arm in
///   [`common::dispatch_default`] that touches status only to settle a row out
///   of `Starting` — and a rename cannot happen while `Starting`, since
///   `session_start` has already fired by then. So it adopts the title and
///   moves nothing, which is precisely what is wanted.
/// - **No [`HookEvent::StopFailure`].** Pi's surface has no run-failed event; a
///   run that ends in an error still ends, so `agent_settled` covers it and the
///   row goes `Idle` with no error text. Inventing an event to carry one is
///   exactly what the "never invent" rule forbids.
///
/// Deliberately not registered: `agent_start` / `agent_end` / `turn_*` /
/// `message_*` / `context` / `before_provider_*` / `after_provider_response`
/// (too fine-grained — each would spawn a subprocess per LLM call to discard its
/// payload), `session_shutdown` (the launcher owns exit), `session_before_switch`
/// / `session_before_fork` / `session_before_tree` / `session_tree` (a
/// replacement session re-emits `session_start`, which we do register),
/// `user_bash`, `model_select` and `thinking_level_select` (the model rides
/// every payload already), and `project_trust` / `resources_discover` (see the
/// module doc).
const FORWARDED: &[(&str, HookEvent)] = &[
    ("session_start", HookEvent::SessionStart),
    ("session_info_changed", HookEvent::SessionStart),
    ("before_agent_start", HookEvent::PromptSubmit),
    ("tool_execution_start", HookEvent::PreToolUse),
    ("tool_execution_end", HookEvent::PostToolUse),
    ("agent_settled", HookEvent::Stop),
    ("session_before_compact", HookEvent::PreCompact),
    ("session_compact", HookEvent::PostCompact),
];

// =============================================================================
// The generated extension
// =============================================================================

/// Where the generated extension lives: **one shared file per machine**, not one
/// per session.
///
/// Shared because it *can* be — the socket rides `$CAPTAIN_MIAO_SOCK`, so two
/// sessions want byte-identical files — and because it must not be per-session
/// here specifically. `pi -e` requires the path to end in **`.ts`**
/// (`extensions.md`: *"the `-e` flag requires `.ts` extension for
/// auto-discovery via jiti TypeScript loader"*), and the launcher's own
/// per-session payload is named `<pid>-settings.json`. A sibling `<pid>.ts`
/// would work for one launch and then leak forever: the launcher's cleanup and
/// its dead-launcher sweep both key on `.sock` / `-settings.json` by name.
///
/// So the launcher's file stays the transport it always was — it carries the
/// source, [`build_launch_command`] reads it back — and the copy the agent is
/// actually handed lands here, under a name that never accumulates. Same shape
/// as Reasonix relocating its `settings.json` into a synthetic home, minus the
/// home.
fn extension_path() -> PathBuf {
    crate::state::state_dir().join("pi-extension.ts")
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

/// The extension source itself. Pure, and the **only** thing spliced into it is
/// `miao_exe` — which is why the file is identical for every session on a
/// machine, and why the snapshot test in this module is worth as much as it is.
///
/// The splice is JSON-encoded rather than shell-quoted: a JSON string literal is
/// a JavaScript string literal, and nothing here reaches a shell (`spawn` runs
/// the binary directly, no `shell: true`), so a path with a quote, a backslash
/// or a space needs no other handling.
///
/// It is written as **plain JavaScript that happens to be valid TypeScript** —
/// no type annotations, no imports beyond a Node builtin. The `.ts` suffix is
/// Pi's loader requirement (see [`extension_path`]), not a request for type
/// syntax we cannot check.
///
/// The one arithmetic in the file is `Math.round` on the token count, and it is
/// there rather than in Rust because JSON has no integer type to round *to* on
/// the way in. `getContextUsage()` is documented as an **estimate** ("uses last
/// assistant usage when available, then estimates tokens for trailing
/// messages"), so a fractional value is possible — and it would fail
/// [`HookMessage::context_tokens`]'s `u64` and take the whole payload, i.e. the
/// status, down with it. `Math.round(undefined)` is `NaN`, which serializes to
/// `null` and reads back as "not reported", so the absent case still behaves.
fn extension_source(miao_exe: &str) -> String {
    let exe = serde_json::to_string(miao_exe).unwrap_or_else(|_| "\"miao\"".to_string());
    let table = FORWARDED
        .iter()
        .map(|(native, forwarded)| format!("  [\"{native}\", \"{}\"],\n", forwarded.as_kebab()))
        .collect::<String>();
    format!(
        r#"// captain-miao's Pi session forwarder — GENERATED. Edits are overwritten on
// the next launch.
//
// Loaded with `pi -e <this file>`: a CLI extension, trusted by virtue of being
// on the command line and scoped to the run that named it.
//
// It carries no logic of its own. For each pi event in FORWARD it builds one
// fixed payload and runs `miao hook --agent pi <event>`, writing that payload to
// the child's stdin. What a status means, and how a failed tool is classified,
// are decided in captain-miao — never here. The launcher socket arrives in the
// environment as $CAPTAIN_MIAO_SOCK, so this file is identical for every
// session on this machine.
import {{ spawn }} from "node:child_process";

// captain-miao's own executable, resolved when this file was written.
const MIAO = {exe};

// pi event -> the captain-miao hook event it is forwarded as.
const FORWARD = [
{table}];

export default function (pi) {{
  for (const [name, forwarded] of FORWARD) {{
    // A pi that renamed or dropped an event must cost that one transition, not
    // the whole extension: an exception here would leave the session untracked
    // with nothing anywhere to read.
    try {{
      pi.on(name, (event, ctx) => send(forwarded, pi, event, ctx));
    }} catch {{}}
  }}
}}

// One shape for every event: fields the event doesn't carry come out undefined,
// and JSON.stringify drops them. The returned promise settles when the child
// exits, so pi delivers our events in the order it fired them, and it never
// rejects — a forwarder that threw would surface on the user's turn.
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
      child = spawn(MIAO, ["hook", "--agent", "pi", forwarded], {{
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

/// The "hook settings" the launcher writes to its per-session file — for Pi,
/// **TypeScript source**, not JSON. The path is generic transport and its
/// contents are opaque to the launcher, so each backend puts its own format
/// through it (Kimi already puts TOML through the same channel).
///
/// `sock_path` is ignored, as it is for Codex, Reasonix and Kimi and for the
/// same reason: the file is shared by every session, so it cannot carry a
/// per-session path. The socket reaches the hook through the environment
/// instead — and unlike those three, that trip needs no faith. Our forwarder
/// spawns the child itself with `spawn`'s default inherited environment, from
/// inside the pi process we set `CAPTAIN_MIAO_SOCK` on, so there is no agent
/// hook-env scrubbing to survive.
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
    let pi_bin = find_in_path(BIN).with_context(|| format!("{BIN} not found in PATH"))?;

    // The launcher already wrote our extension source to `settings_path`;
    // relocate it to a `.ts` path Pi's loader will accept (see
    // [`extension_path`]). Note the file the launcher wrote is named
    // `…-settings.json` and holds **TypeScript** — that path is generic
    // transport, opaque to the launcher.
    let source = std::fs::read_to_string(settings_path).context("reading pi hook extension")?;
    let extension = ensure_extension(&source)?;

    let has_envrc = Path::new(cwd).join(".envrc").is_file();
    let mut cmd = match has_envrc.then(|| find_in_path("direnv")).flatten() {
        Some(direnv) => {
            let mut c = Command::new(direnv);
            c.args(["exec", cwd]).arg(&pi_bin);
            c
        }
        None => Command::new(&pi_bin),
    };

    cmd.current_dir(cwd);
    super::with_shim_path(&mut cmd, shim_dir);
    // Read by `miao hook` (spawned from the extension, which inherits this
    // process's environment). The extension file is shared by every session and
    // so cannot carry the path itself.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    cmd.args(launch_args(&extension, extra_args));
    Ok(cmd)
}

/// The agent-facing argv: our extension, then whatever the launcher forwarded
/// (`--session <id>`, `--fork <id>`).
///
/// **No directory argument of any kind.** `usage.md` documents the shape as
/// `pi [options] [@files...] [messages...]` — the trailing positionals are
/// *prompts* — and there is no `--dir` or `--cwd` flag. `cwd` reaches Pi as the
/// spawned process's working directory, which is also how it organizes sessions
/// on disk. Reasonix is the standing reminder of what the alternative costs: a
/// session whose first user message is a path.
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
/// happens to emit — `extension_source` builds it and this parses it, so the two
/// are pinned together by the tests below rather than by a vendor's docs.
///
/// snake_case, matching the launcher's own wire vocabulary; the JavaScript names
/// these keys explicitly rather than dumping a pi event object, because pi's
/// event payloads carry `AbortSignal`s and message graphs that `JSON.stringify`
/// would either flatten to `{}` or refuse outright.
///
/// The pi-side sources, one per field: `ctx.sessionManager.getSessionId()`,
/// `pi.getSessionName()`, `ctx.cwd`, `event.toolName` (the tool-execution
/// events), `event.prompt` (`before_agent_start`), `event.isError`
/// (`tool_execution_end`), `ctx.getContextUsage().tokens` and `ctx.model.id`.
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
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse pi hook JSON from stdin")?;
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
        // Pi *does* export `PI_SESSION_FILE` and its JSONL schema is documented,
        // so this is a decision rather than a gap: one fact, one source, and the
        // tokens and model above are already that source. Leaving it `None` also
        // keeps the launcher's transcript pipeline inert, since it only ever
        // runs on a path a hook supplied.
        transcript_path: None,
        raw: Some(stdin.to_string()),
    })
}

/// A tool that failed arrives as `tool_execution_end` with `isError`
/// (`extensions.md`: *"event.toolCallId, event.toolName, event.result,
/// event.isError"*), which our vocabulary spells
/// [`HookEvent::PostToolUseFailure`].
///
/// **Cosmetic today, and kept anyway.** `dispatch_default` settles the two
/// identically, so nothing on the row moves differently. It is here because the
/// fact is on the payload and dropping it would read as an oversight later, and
/// because this is where the correction belongs the day the two arms diverge.
fn normalize_event(event: HookEvent, payload: &HookPayload) -> HookEvent {
    match event {
        HookEvent::PostToolUse if payload.is_error => HookEvent::PostToolUseFailure,
        other => other,
    }
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Pi departs from [`common::dispatch_default`] nowhere. The native → normalized
/// renaming is done in the generated table ([`FORWARDED`]) rather than here, the
/// one payload-driven correction is done in [`parse_hook_payload`], and
/// `agent_settled` means the shared `Stop` arm needs no help from a session file
/// or a rollout scan.
///
/// The wrapper stays so the seam keeps one callee per backend, and so the day Pi
/// grows a case of its own it has a place to land.
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
    /// [`extension_source`] rather than captured, because no `pi` was available
    /// — but unlike the other backends' fixtures, the thing it mirrors is *our*
    /// code, so it can only drift by someone editing the template.
    fn payload(extra: &str) -> String {
        format!(
            r#"{{"session_id":"s1","session_title":"wire up the parser",
                "cwd":"/home/miao/p","context_tokens":48100,"model":"some-model-1"{extra}}}"#
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState {
            agent: AgentControl::Pi,
            launcher_pid: 0,
            session_id: None,
            window_id: None,
            tab_id: None,
            cwd: String::new(),
            status,
            last_tool: None,
            updated_at: 0,
            active_since: None,
            last_prompt: None,
            child_pid: None,
            last_error: None,
            context_tokens: None,
            model: None,
            name: None,
            first_prompt: None,
            pool_session: None,
            launch_id: None,
            terminal: None,
            terminfo: None,
            flags: None,
            attached: None,
            host: crate::state::HostId::local(),
        }
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
        let expected = r#"// captain-miao's Pi session forwarder — GENERATED. Edits are overwritten on
// the next launch.
//
// Loaded with `pi -e <this file>`: a CLI extension, trusted by virtue of being
// on the command line and scoped to the run that named it.
//
// It carries no logic of its own. For each pi event in FORWARD it builds one
// fixed payload and runs `miao hook --agent pi <event>`, writing that payload to
// the child's stdin. What a status means, and how a failed tool is classified,
// are decided in captain-miao — never here. The launcher socket arrives in the
// environment as $CAPTAIN_MIAO_SOCK, so this file is identical for every
// session on this machine.
import { spawn } from "node:child_process";

// captain-miao's own executable, resolved when this file was written.
const MIAO = "/home/miao/.local/bin/miao";

// pi event -> the captain-miao hook event it is forwarded as.
const FORWARD = [
  ["session_start", "session-start"],
  ["session_info_changed", "session-start"],
  ["before_agent_start", "prompt-submit"],
  ["tool_execution_start", "pre-tool-use"],
  ["tool_execution_end", "post-tool-use"],
  ["agent_settled", "stop"],
  ["session_before_compact", "pre-compact"],
  ["session_compact", "post-compact"],
];

export default function (pi) {
  for (const [name, forwarded] of FORWARD) {
    // A pi that renamed or dropped an event must cost that one transition, not
    // the whole extension: an exception here would leave the session untracked
    // with nothing anywhere to read.
    try {
      pi.on(name, (event, ctx) => send(forwarded, pi, event, ctx));
    } catch {}
  }
}

// One shape for every event: fields the event doesn't carry come out undefined,
// and JSON.stringify drops them. The returned promise settles when the child
// exits, so pi delivers our events in the order it fired them, and it never
// rejects — a forwarder that threw would surface on the user's turn.
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
      child = spawn(MIAO, ["hook", "--agent", "pi", forwarded], {
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
    /// `miao hook --agent pi <name>` the CLI rejects is a status silently lost.
    #[test]
    fn the_registered_events_are_exactly_what_the_module_claims() {
        // The native names, in the order pi sees them registered.
        assert_eq!(
            FORWARDED.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [
                "session_start",
                "session_info_changed",
                "before_agent_start",
                "tool_execution_start",
                "tool_execution_end",
                "agent_settled",
                "session_before_compact",
                "session_compact",
            ]
        );
        // **Only `agent_settled` becomes `Stop`.** That is the whole turn-end
        // design: nothing else may claim the turn is over.
        assert_eq!(
            FORWARDED
                .iter()
                .filter(|(_, e)| *e == HookEvent::Stop)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>(),
            ["agent_settled"]
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
        assert!(source.contains(r#"spawn(MIAO, ["hook", "--agent", "pi", forwarded]"#));
    }

    /// A turn runs prompt → tool → settle, and `agent_settled` is what ends it.
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

    /// `isError` on a `tool_execution_end` is the one payload-driven event
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
        // `Math.round(undefined)` serializes to when pi reports no usage yet,
        // and it must read as "not reported" rather than failing the payload —
        // which would take the *status* down with it, not just the number.
        let unusable = parse_hook_payload(HookEvent::Stop, r#"{"context_tokens":null}"#)
            .expect("a null token count parses");
        assert_eq!(unusable.context_tokens, None);
    }

    /// No transcript path, ever — the field the launcher gates its whole
    /// transcript watch on. Pi has a readable one; not naming it is what keeps
    /// the hook the single source for tokens and model.
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
    /// pi's trailing positionals are prompts, so a cwd there would open a
    /// session whose first user message is a path.
    #[test]
    fn the_argv_names_the_extension_and_no_directory() {
        let ext = Path::new("/home/miao/.local/state/captain-miao/pi-extension.ts");
        assert_eq!(
            launch_args(ext, &[]),
            ["-e", "/home/miao/.local/state/captain-miao/pi-extension.ts"]
        );
        assert_eq!(
            launch_args(ext, &["--session".to_string(), "s1".to_string()]),
            [
                "-e",
                "/home/miao/.local/state/captain-miao/pi-extension.ts",
                "--session",
                "s1"
            ]
        );
    }

    /// pi's loader dispatches on the suffix, so the relocated path must end in
    /// `.ts` — the single fact that stops the launcher's own
    /// `<pid>-settings.json` from being handed over directly.
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
}
