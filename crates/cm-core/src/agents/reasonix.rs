//! Reasonix CLI backend. Owns every Reasonix-specific path, env var and hook
//! payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Reasonix`'s match arms.
//!
//! **Source-verified, never run.** No `reasonix` binary was available when this
//! was written, so every claim below comes from reading
//! `esengine/DeepSeek-Reasonix@main-v2` — `internal/hook/{hook,runner}.go`,
//! `internal/config/{paths,path_access}.go`, `docs/CLI.md`. Each is cited where
//! it matters so a later probe knows which file to re-read rather than which
//! guess to re-derive. What a probe still has to settle is listed at the bottom
//! of this doc.
//!
//! **The event vocabulary is a 1:1 fit**, closer than Claude's. Reasonix defines
//! 13 events; nine map straight onto a [`HookEvent`] and are the nine we
//! register, so the whole dispatcher is [`common::dispatch_default`] with no
//! per-agent arm at all. Two consequences are worth stating because they are
//! *absences*:
//!
//! - **`PermissionRequest` is native**, so `WaitingForApproval` needs no second
//!   mechanism (Grok reaches the same state only through a separate notify
//!   system).
//! - **`isInterrupt` is a payload field**, so an Esc-interrupted turn *is* a
//!   hook — Codex's transcript sentinel has no counterpart here and
//!   `scan_transcript_signals` stays the empty default. Reasonix reports an
//!   interrupt by ending the turn in `StopFailure` with the flag set
//!   (`Runner.StopResult` → `errors.Is(err, context.Canceled)`), so
//!   [`parse_hook_payload`] normalizes that back to a plain [`HookEvent::Stop`]:
//!   a turn the user stopped is over, not failed, and letting it through as
//!   `StopFailure` would stamp "context canceled" on the row as an error.
//!
//! The four events we do **not** register: `SessionEnd` (the launcher owns
//! exit), `SubagentStop` (no state of ours to move — and it deliberately does
//! *not* fire for backgrounded tasks, `internal/agent/hooks_test.go`),
//! `Notification` (carries `notificationType`; approval already arrives as
//! `PermissionRequest`) and `PostLLMCall` (the model's raw reasoning text, once
//! per model turn — registering it would spawn a subprocess per turn to discard
//! its payload).
//!
//! **There is no `PostCompact`** — Reasonix's const block stops at `PreCompact`.
//! So a row enters `Compacting` and leaves it on the next event, which needs no
//! code of ours: eight of the nine events we register assign a status
//! unconditionally in `dispatch_default`. The ninth is `SessionStart`, whose arm
//! only settles a row out of `Starting` — so it does *not* clear `Compacting`,
//! and since Reasonix fires it on `/new` as well as at startup, a `/compact`
//! followed by `/new` holds the row until the next prompt. An auto-compaction is
//! mid-turn, so the next `PreToolUse` / `Stop` lands within seconds; a manual
//! `/compact` waits for the user. Cosmetic and self-correcting either way, and
//! the honest cost of an agent with no post-compaction signal — clearing
//! `Compacting` on `SessionStart` would be a change to the shared dispatcher,
//! and so Claude's and Codex's business too.
//!
//! **Isolation is three env vars and almost no symlink farm.** Reasonix splits
//! its roots (`internal/config/paths.go`): the *home* holds config,
//! credentials and `settings.json`; `REASONIX_STATE_HOME` moves sessions,
//! archive, memory and projects; `REASONIX_CACHE_HOME` moves the rebuildable
//! catalogs. Setting all three points the state and cache roots at their **real**
//! locations while only the config root is ours — so no mutable directory is
//! reached through a symlink we created (the cause of the macOS FSEvents caveat
//! Codex's `sessions` link produces), and the catalogs aren't rebuilt per
//! session. Leaving the cache unset is the easy mistake: it would land under
//! `$REASONIX_HOME/cache`, i.e. inside the synthetic home.
//!
//! **The payload carries no transcript path**, so the launcher's whole
//! transcript pipeline (stats fold, signal scan, stat-poll) is inert for
//! Reasonix by construction — it only ever runs on a path a hook supplied.
//! That is why the token/model columns are empty, not an oversight; the session
//! sidecar schema that would fill them is the open item below.
//!
//! What a probe against a real binary must confirm:
//! - that `reasonix hook list --json --home-dir <synth>` reports our nine hooks
//!   as `active` — malformed settings yield *no* hooks and no error
//!   (`readSettings`, "a typo shouldn't take down the CLI"), so the failure mode
//!   is every row stuck at `Starting` with nothing in any log;
//! - that the payload's `sessionId` is the id `-r <id>` accepts (it is set from
//!   `Runner.SetSessionID`, documented as "the Claude-compatible session
//!   identifier", while `docs/CLI.md` also describes an opaque *machine* session
//!   id — if they differ, resume is aimed at the wrong namespace);
//! - the session sidecar schema under `<state root>/sessions/`, which is what
//!   [`crate::agent::AgentControl::read_transcript_stats`] would need;
//! - that the three roots resolve as `paths.go` reads under one launch, with a
//!   **symlinked `.env` honoured** — under `REASONIX_HOME` every fallback path
//!   is skipped, so a synthetic home that fails to expose the credentials file
//!   doesn't degrade, it fails to start.
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
use super::synth_home::SynthHome;
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "reasonix";

// =============================================================================
// Filesystem locations — the three real roots (internal/config/paths.go)
// =============================================================================

/// The real Reasonix home — `$REASONIX_HOME` if the user set one globally, else
/// `~/.reasonix`. Config, credentials (`.env`) and `settings.json` live here.
/// This is what the synthetic home mirrors; it is *not* what the launched agent
/// is handed (see [`ensure_synth_home`]).
fn reasonix_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("REASONIX_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    dirs::home_dir().map(|h| h.join(".reasonix"))
}

/// The real **state** root — `sessions/`, `archive/`, `memory/`, `projects/`,
/// `worktrees/`. `$REASONIX_STATE_HOME` if set, else the home (`userSupportDir`).
/// Passed through verbatim so a captain-miao session writes its transcripts
/// where every other Reasonix session writes them.
fn state_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("REASONIX_STATE_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    reasonix_home()
}

/// The real **cache** root — the rebuildable session / history / usage / task
/// catalogs. Mirrors `userCacheDir()`: `$REASONIX_CACHE_HOME`, else
/// `$REASONIX_HOME/cache` **when the user set one** (so their own isolated
/// install keeps its own cache), else the OS cache dir. The middle branch is why
/// this reads the env var rather than calling [`reasonix_home`], which would
/// substitute the `~/.reasonix` default and send the cache somewhere Reasonix
/// would never have put it.
fn cache_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("REASONIX_CACHE_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    if let Some(h) = std::env::var_os("REASONIX_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p.join("cache"));
        }
    }
    dirs::cache_dir().map(|c| c.join("reasonix"))
}

/// A single shared synthetic `$REASONIX_HOME` for every Reasonix session: the
/// real home mirrored through symlinks, plus our `settings.json`. Shared (rather
/// than per-session) because it is a symlink farm over the user's home — one
/// stable copy is cheaper to build and to reason about than one per launch — and
/// that sharing is exactly why the hook command can carry no per-session data
/// (see [`build_hooks_settings`]).
fn synth_home() -> PathBuf {
    crate::state::state_dir().join("reasonix-home")
}

// =============================================================================
// Launcher: process spawn + synthetic REASONIX_HOME
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    // The launcher already wrote our settings.json contents to `settings_path`;
    // relocate them into the synthetic home, which is the only place Reasonix
    // looks for global hooks (`hook.Load` reads the project file, the installed
    // plugins' manifests, and `<home>/settings.json` — there is no override
    // flag and no `REASONIX_HOOKS`).
    let settings_json =
        std::fs::read_to_string(settings_path).context("reading reasonix hook settings")?;
    let home = ensure_synth_home(&settings_json)?;

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    cmd.env("REASONIX_HOME", &home);
    // Only the *config* root is ours. Without these two the state and cache
    // roots would follow `REASONIX_HOME` into the synthetic dir, putting the
    // user's sessions behind a symlink we made and rebuilding every catalog per
    // launch.
    if let Some(state) = state_home() {
        cmd.env("REASONIX_STATE_HOME", state);
    }
    if let Some(cache) = cache_home() {
        cmd.env("REASONIX_CACHE_HOME", cache);
    }
    // The hook subprocess reads the launcher socket from here rather than from
    // an argv flag: the synthetic home is shared by every session, so its
    // settings.json cannot carry a per-session path. It survives the trip —
    // Reasonix hands hooks `secrets.ProcessEnv()`, which drops only its own
    // stored credentials and (opt-in) names matching a secret-ish pattern.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    cmd.args(launch_args(cwd, extra_args));
    Ok(cmd)
}

/// The agent-facing argv: the workspace root, then whatever the launcher
/// forwarded (`-r <id>`, `--copy`).
///
/// **`--dir` rather than a positional**, which is where Reasonix's argv shape
/// departs from both other backends. `reasonix`'s optional positional is a
/// *prompt* (`docs/CLI.md`: "Flags may appear before or after the prompt"), so a
/// bare `reasonix /work` would open a session whose first user message is the
/// path. Pure and separately pinned, because the process cwd is set too and
/// would mask the mistake in every case except the one that matters.
fn launch_args(cwd: &str, extra: &[String]) -> Vec<String> {
    let mut v = vec!["--dir".to_string(), cwd.to_string()];
    v.extend(extra.iter().cloned());
    v
}

/// Create / refresh the synthetic home and return it: mirror the real Reasonix
/// home through symlinks and add our `settings.json`. The mirroring — dangling
/// links, shadowing entries, file modes — lives in [`super::synth_home`].
///
/// Nothing is **copied**: Reasonix canonicalizes a config path through its
/// symlinks before editing (`resolveConfigAccessPath` → `EvalSymlinks`,
/// `internal/config/path_access.go`) and writes the resolved file, so a `/model`
/// change or a `reasonix setup` inside a captain-miao session lands in the
/// user's real `config.toml` / `.env` instead of in a private copy that then
/// silently diverges. Codex copies its config only because it persists *our*
/// hook trust into it; Reasonix has no trust gate to seed (`LoadOptions.Trusted`
/// is documented as retained for source compatibility).
///
/// A home that doesn't exist yet yields a synthetic home holding only our
/// `settings.json` — and under `REASONIX_HOME` every credential fallback is
/// skipped, so that session won't start. Run `reasonix setup` outside
/// captain-miao once first; the mirror picks its files up on the next launch.
fn ensure_synth_home(settings_json: &str) -> Result<PathBuf> {
    let home = SynthHome {
        dir: synth_home(),
        real: reasonix_home(),
        owned: &["settings.json"],
        copied: &[],
    };
    home.ensure()?;
    home.write_owned("settings.json", settings_json)?;
    Ok(home.dir)
}

/// Build the Reasonix `settings.json`. Its shape is `{"hooks": {<Event>:
/// [HookConfig…]}}` — a **flat** array per event (`Settings` in
/// `internal/hook/hook.go`), not Claude's nested `{matcher, hooks:[…]}`.
///
/// `match` is an anchored regex over tool names, honoured on the tool events and
/// ignored elsewhere; `"*"` is every tool. No `timeout` is set: the defaults
/// (5s on the gating events, 30s otherwise) are generous for a socket write, and
/// Reasonix's timeout is in **milliseconds** unlike every other agent here — a
/// unit worth not having to remember. A timed-out hook on `PreToolUse` /
/// `UserPromptSubmit` *blocks* the turn, which is the other reason not to
/// tighten it; our forwarder can only exit 0 or 1, and only exit **2** blocks
/// (`decideOutcome`), so a missing launcher can never wedge a session.
///
/// Like Codex's, the command carries no per-session data — the socket arrives
/// via `$CAPTAIN_MIAO_SOCK` — because one settings.json serves every session.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    let exe_q = shell_quote(&exe.to_string_lossy());

    let hook = |event: HookEvent| -> serde_json::Value {
        serde_json::json!([{
            "match": "*",
            "command": format!("{exe_q} hook --agent reasonix {}", event.as_kebab()),
        }])
    };

    serde_json::json!({
        "hooks": {
            "SessionStart":       hook(HookEvent::SessionStart),
            "UserPromptSubmit":   hook(HookEvent::PromptSubmit),
            "PreToolUse":         hook(HookEvent::PreToolUse),
            "PostToolUse":        hook(HookEvent::PostToolUse),
            "PostToolUseFailure": hook(HookEvent::PostToolUseFailure),
            "PermissionRequest":  hook(HookEvent::PermissionRequest),
            "Stop":               hook(HookEvent::Stop),
            "StopFailure":        hook(HookEvent::StopFailure),
            "PreCompact":         hook(HookEvent::PreCompact),
        }
    })
    .to_string()
}

// =============================================================================
// Hook payload (stdin from Reasonix → normalized HookMessage)
// =============================================================================

/// Reasonix's native hook payload (`Payload`, `internal/hook/hook.go`), reduced
/// to the fields we act on. **camelCase**, where Claude and Codex are
/// snake_case: Reasonix also ships a Claude-compatibility payload format, and it
/// is deliberately not used — it is a shim for hooks imported from someone
/// else's Claude config, populated only for those, and depending on its fidelity
/// would buy nothing that this struct doesn't.
///
/// The fields left out are real but unused: `toolArgs`, `toolResult`, `subject`,
/// `lastAssistantText`, `turn`, `message` (Notification), `trigger`
/// (`"auto"`/`"manual"` on PreCompact), `reasoning` (PostLLMCall), `source`
/// (SessionStart), `reason` (SessionEnd), `notificationType`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookPayload {
    session_id: Option<String>,
    cwd: Option<String>,
    tool_name: Option<String>,
    prompt: Option<String>,
    /// The turn's / tool's error text, on `StopFailure` and `PostToolUseFailure`.
    error: Option<String>,
    /// Set when that error is a cancellation — i.e. the user interrupted.
    #[serde(default)]
    is_interrupt: bool,
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse reasonix hook JSON from stdin")?;
    Ok(HookMessage {
        event: normalize_event(event, &payload),
        session_id: payload.session_id,
        tool_name: payload.tool_name,
        // The failure events carry their error here; `dispatch_default` surfaces
        // it as `last_error` rather than falling back to the raw payload.
        message: payload.error,
        cwd: payload.cwd,
        prompt: payload.prompt,
        // Reasonix's payload carries no title; `subject` is the turn's, not the
        // session's, and it is one of the fields deliberately left unread.
        session_title: None,
        // Reasonix's payload names neither; both wait on the session sidecar
        // schema (module doc).
        context_tokens: None,
        model: None,
        // Reasonix's payload has no transcript path, and this is the field the
        // launcher gates its entire transcript watch on — so nothing reads a
        // Reasonix transcript, which is what makes the empty `read_transcript_stats`
        // and `scan_transcript_signals` consistent rather than merely unimplemented.
        transcript_path: None,
        raw: Some(stdin.to_string()),
    })
}

/// A turn the **user** interrupted arrives as `StopFailure` with `isInterrupt`
/// (`Runner.StopResult`: `errors.Is(err, context.Canceled)`). In our vocabulary
/// that is a plain [`HookEvent::Stop`] — the turn is over, nothing failed — and
/// mapping it as a failure would put "context canceled" on the row as an error
/// every time someone pressed Esc.
///
/// `PostToolUseFailure` carries the same flag and is deliberately left alone:
/// it and `PostToolUse` settle identically, so there is nothing to correct.
fn normalize_event(event: HookEvent, payload: &HookPayload) -> HookEvent {
    match event {
        HookEvent::StopFailure if payload.is_interrupt => HookEvent::Stop,
        other => other,
    }
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// Reasonix departs from [`common::dispatch_default`] nowhere: its nine
/// registered events are nine of ours under different spellings, the one case
/// that would have needed an arm (an interrupt arriving as a failure) is
/// normalized in [`parse_hook_payload`], and the missing `PostCompact` is
/// handled by every other arm assigning a status unconditionally. The wrapper
/// stays so the seam keeps one callee per backend — and so the day Reasonix
/// grows a case of its own, it has a place to land.
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

    /// **Hand-written from `internal/hook/hook.go`, not captured from a running
    /// binary** — no `reasonix` was installed when these were written. A probe
    /// that captures real payloads (point the hook command at `tee`) should
    /// diff them against these and correct them here first.
    fn payload(event: &str, extra: &str) -> String {
        format!(r#"{{"event":"{event}","sessionId":"s1","cwd":"/home/miao/p"{extra}}}"#)
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::Reasonix, status)
    }

    /// Drive one hook end to end — parse the agent's stdin JSON, then dispatch it
    /// — so the tests exercise the same path a live hook takes, including the
    /// event normalization that only happens in the parser.
    fn feed(state: &mut LauncherState, event: HookEvent, stdin: &str) {
        let msg = parse_hook_payload(event, stdin).expect("payload parses");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(dispatch_hook(state, msg));
    }

    #[test]
    fn a_turn_runs_from_prompt_to_stop() {
        let mut state = state_at(SessionStatus::Idle);
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload("UserPromptSubmit", r#","prompt":"go""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_prompt.as_deref(), Some("go"));
        // The session id rides every payload, so the launcher learns it here
        // rather than from a session file.
        assert_eq!(state.session_id.as_deref(), Some("s1"));

        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload("PreToolUse", r#","toolName":"bash""#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool.as_deref(), Some("bash"));

        feed(&mut state, HookEvent::Stop, &payload("Stop", ""));
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_tool, None);
    }

    /// `PermissionRequest` is a native Reasonix event, so the approval state is
    /// reached with no second mechanism — the thing that makes this the cheapest
    /// backend to get right.
    #[test]
    fn permission_request_is_an_approval_gate() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &payload("PermissionRequest", r#","toolName":"bash""#),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
    }

    /// Esc arrives as `StopFailure` + `isInterrupt`. It must end the turn like
    /// any other stop and leave **no error** on the row: an interrupt is the user
    /// getting what they asked for.
    #[test]
    fn an_interrupted_turn_ends_without_an_error() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::StopFailure,
            &payload(
                "StopFailure",
                r#","error":"context canceled","isInterrupt":true"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(
            state.last_error, None,
            "an interrupt is not a failure to report"
        );
    }

    /// The same event *without* the flag is a real failure and keeps its message
    /// — which is what stops the normalization above from swallowing errors.
    #[test]
    fn a_failed_turn_keeps_its_error() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::StopFailure,
            &payload("StopFailure", r#","error":"provider 500""#),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_error.as_deref(), Some("provider 500"));
    }

    /// Reasonix has no `PostCompact`, so `Compacting` is left by the next event
    /// of any kind — the property that stands in for one, and the reason it needs
    /// a test rather than a comment.
    #[test]
    fn compacting_is_left_by_the_next_event_of_any_kind() {
        for (event, native, expected) in [
            (HookEvent::Stop, "Stop", SessionStatus::Idle),
            (HookEvent::PreToolUse, "PreToolUse", SessionStatus::Active),
            (
                HookEvent::PromptSubmit,
                "UserPromptSubmit",
                SessionStatus::Active,
            ),
        ] {
            let mut state = state_at(SessionStatus::Active);
            feed(
                &mut state,
                HookEvent::PreCompact,
                &payload("PreCompact", ""),
            );
            assert_eq!(state.status, SessionStatus::Compacting);
            feed(&mut state, event, &payload(native, ""));
            assert_eq!(state.status, expected, "after {native}");
        }
    }

    /// The payload is camelCase, where both other backends are snake_case — the
    /// single most likely thing to be silently wrong if the source moves.
    #[test]
    fn the_payload_is_camel_case() {
        let stdin = payload(
            "PostToolUseFailure",
            r#","toolName":"read_file","error":"no such file","isInterrupt":false"#,
        );
        let msg = parse_hook_payload(HookEvent::PostToolUseFailure, &stdin).expect("parses");
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
        assert_eq!(msg.cwd.as_deref(), Some("/home/miao/p"));
        assert_eq!(msg.tool_name.as_deref(), Some("read_file"));
        assert_eq!(msg.message.as_deref(), Some("no such file"));
        // No transcript path exists in this payload, which is what keeps the
        // launcher's transcript machinery inert for Reasonix.
        assert_eq!(msg.transcript_path, None);
        // A snake_case reading would find none of the above; guard the one field
        // whose absence would otherwise look like "the agent didn't send it".
        assert!(
            parse_hook_payload(HookEvent::Stop, r#"{"tool_name":"bash"}"#)
                .expect("parses")
                .tool_name
                .is_none()
        );
    }

    /// One settings.json serves every session, so it must carry no per-session
    /// data; and it must register exactly the events Reasonix actually emits —
    /// an unknown key is silently dropped, and a spelling error is therefore
    /// invisible until a row never leaves `Starting`.
    #[test]
    fn hooks_settings_registers_the_native_event_names_and_no_socket() {
        let a = build_hooks_settings("/run/a.sock");
        let b = build_hooks_settings("/run/b.sock");
        assert_eq!(a, b, "settings.json must not embed the per-session socket");
        assert!(!a.contains(".sock"));

        let json: serde_json::Value = serde_json::from_str(&a).expect("valid JSON");
        let hooks = json["hooks"].as_object().expect("a hooks object");
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "PermissionRequest",
                "PostToolUse",
                "PostToolUseFailure",
                "PreCompact",
                "PreToolUse",
                "SessionStart",
                "Stop",
                "StopFailure",
                "UserPromptSubmit",
            ],
        );
        // Each event's entry is a flat array of HookConfig — not Claude's nested
        // matcher/hooks shape — and forwards that event by name.
        let stop = &hooks["Stop"][0];
        assert_eq!(stop["match"], "*");
        assert!(
            stop["command"]
                .as_str()
                .expect("a command string")
                .ends_with("hook --agent reasonix stop"),
            "{stop:?}"
        );
    }

    /// The cwd goes in as `--dir`, because Reasonix's positional is a prompt.
    #[test]
    fn the_workspace_root_is_passed_as_dir() {
        assert_eq!(launch_args("/work", &[]), ["--dir", "/work"]);
        assert_eq!(
            launch_args("/work", &["-r".to_string(), "s1".to_string()]),
            ["--dir", "/work", "-r", "s1"]
        );
    }
}
