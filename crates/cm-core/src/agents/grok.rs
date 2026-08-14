//! Grok Build CLI backend. Owns every Grok-specific path, env var and hook
//! payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Grok`'s match arms.
//!
//! **Source-verified, never run.** No `grok` binary was available when this was
//! written, so every claim below comes from reading `xai-org/grok-build@main` —
//! the shipped user guide (`user-guide/{05-configuration,10-hooks,17-sessions,
//! 22-permissions-and-safety}.md`, `custom-hooks.md`, `tutorial/06-worktrees.md`)
//! and the sources `xai-grok-pager/src/notifications/{config,hooks}.rs`,
//! `xai-grok-shell/src/session/storage/summary_write.rs`,
//! `xai-grok-shell/src/extensions/notification.rs`. Each is cited where it
//! matters so a later probe knows which file to re-read rather than which guess
//! to re-derive. **Claims that source reading could not settle are marked
//! _unverified_ at the point they are used and listed again at the bottom** —
//! there are more of them here than in any other backend, and three of them are
//! why this module ships deliberately smaller than the agent can support.
//!
//! **Isolation is `GROK_HOME`, the Codex pattern with one extra wrinkle.**
//! `17-sessions.md`: *"Set `GROK_HOME` to override the base directory; when it
//! is unset, Grok uses `~/.grok`"* — and it moves everything, not just config:
//! sessions, auth, memory, skills, plugins, agents and logs. So the synthetic
//! home mirrors the real one through symlinks ([`super::synth_home`]) and we own
//! exactly two things:
//!
//! - **`hooks/captain-miao.json`.** `hooks/` is a *directory* of independent
//!   files, so unlike Codex's single `hooks.json` there is no merge to get
//!   right — but there is a nested mirror to get right, which is why
//!   [`ensure_synth_home`] builds a second [`SynthHome`] *inside* the first. Own
//!   the directory without it and the user's own global hooks stop firing in
//!   every captain-miao session; leave the directory to the outer mirror and our
//!   file would be written **through a symlink into the user's real `~/.grok`**,
//!   which is the one thing a synthetic home exists to prevent.
//! - **`config.toml`, a writable copy** rather than a symlink — Codex's
//!   hard-won rule, and the *only* reason it is needed here is the approval hook
//!   below. Drop approval state and `hooks/` alone would do.
//!
//! Everything else is a symlink, and that is the invariant rather than an
//! enumeration: **`auth.json` in particular must be linked, never copied** — it
//! is auto-managed, so a copy would strand a token refresh inside the synthetic
//! home.
//!
//! **No trust seeding.** `custom-hooks.md`'s scope table is explicit: global
//! `~/.grok/hooks/*.json` and config-file hooks are *always* trusted; only
//! `<project>/.grok/hooks/` needs `/hooks-trust`. We inject at the global tier,
//! so no prompt can fire and there is no hash to precompute — strictly simpler
//! than Codex, whose `seed_hook_trust` exists because it has no always-trusted
//! tier.
//!
//! **Approval arrives over a second, unrelated hook system.** Grok's lifecycle
//! hooks have no approval event at all (the closest, `PermissionDenied`, fires
//! *after* a refusal), so `WaitingForApproval` — the single most valuable state
//! on the dashboard — would be unreachable. `notifications/config.rs` defines an
//! independent mechanism whose `NotificationEventKind::ApprovalRequired` is
//! exactly the missing signal, configured as `[[ui.notifications.hooks]]` in
//! `config.toml`. Its contract differs from the lifecycle hooks in three ways
//! that all bite (see [`notification_hook_command`] and
//! [`with_notification_hook`]): stdin is `/dev/null` so there is no payload,
//! everything arrives in the environment, and `only_unfocused` **defaults to
//! `true`** — which would work in casual testing and silently stop working
//! whenever the user was actually looking at the row.
//!
//! **What this module deliberately does not do**, each because the shape of the
//! data is unknown rather than because the feature is out of reach:
//!
//! - **No transcript path, and so no transcript pipeline at all.** The path *is*
//!   derivable — `$GROK_HOME/sessions/<url-encoded-cwd>/<session-id>/` per
//!   `17-sessions.md` — but the encoder and reserved set are unverified, and
//!   every consumer of the path needs a line schema that source reading did not
//!   settle: `updates.jsonl` is an ACP update stream carrying
//!   `TurnCompleted.usage` and `AutoCompactStarted { tokens_used, context_window
//!   }` (`extensions/notification.rs`) under an envelope nobody has seen, and
//!   `summary.json` carries the title, model and git head under JSON spellings
//!   that were read as *Rust field names* (`summary_write.rs`). Deriving the
//!   path now would start a watch that folds nothing on every append. So the
//!   token and model columns are empty, `list_resumable` is empty, and the fold
//!   lands with the schema, in one commit, once a real session dir has been
//!   read.
//! - **No interrupt detection.** `10-hooks.md` is explicit that *"Interrupted
//!   (Esc / Ctrl+C), refused, and max-turns turns skip Stop hooks entirely"*, so
//!   Grok has Codex's problem — and Codex solves it by matching `turn_aborted`
//!   in its rollout. The equivalent sentinel in `updates.jsonl` is **not named
//!   in any source read**, and guessing one that never matches is
//!   indistinguishable from not scanning while looking like it works. The
//!   consequence is concrete and belongs on the row: **an interrupted turn stays
//!   `Active` until the next prompt.** This is the top probe item.
//! - **No background-task tiers.** `Stop` carries `backgroundTasks` and
//!   `sessionCrons` — strictly better data than Claude's process-tree walk,
//!   since it comes from the agent that owns the tasks at the moment the
//!   decision is made — but routing it to the dashboard needs a new
//!   `LauncherState` field, which is seam work and belongs in its own commit.
//!   [`crate::agent::AgentControl::bg_shells`] answers `None` until then. The
//!   payload already carries the data; nothing here has to be re-derived.
//! - **No `prompt` on the row.** Every documented payload field is used, and
//!   none of them is the user's prompt text (see [`HookPayload`]).
//!
//! What a probe against a real binary must settle, worst-breakage first:
//! - **the interrupt sentinel in `updates.jsonl`** — run a turn, hit Esc, diff
//!   the appended lines. Blocks correct status;
//! - **the schema of a `$GROK_HOME/hooks/*.json` file.** It is documented as a
//!   *location*, never as a shape. [`build_hooks_settings`] writes Claude's,
//!   inferred from Grok reading `~/.claude/settings.json` and `.cursor/hooks.json`
//!   directly and from config-file hooks being spelled `[[hooks.<Event>]]`. If
//!   that inference is wrong **no hook fires and every row sits at `Starting`**,
//!   which is also what a typo looks like — check this before believing any
//!   other symptom. **A second implementation exists to compare against**:
//!   `manaflow-ai/cmux` ships a Grok adapter that writes
//!   `~/.grok/hooks/cmux-session.json` and relocates the tree with `GROK_HOME`,
//!   which independently corroborates the *location* and the one-JSON-file-per-
//!   integration form — not the shape inside it, which is the part still
//!   inferred. Read that adapter before running a probe; it is cheaper than a
//!   capture and settles most of this item;
//! - **which lifecycle events exist.** `PreToolUse`, `PostToolUse`, `Stop` and
//!   `StopFailure` are named in the sources; `SessionStart`, `UserPromptSubmit`,
//!   `PreCompact` and `PostCompact` are registered on the strength of
//!   `10-hooks.md`'s *"unrecognized event names are skipped"*, which makes a
//!   wrong name inert rather than fatal;
//! - **the field carrying the user's prompt**, if there is one. One captured
//!   `user_prompt_submit` payload settles it and is a one-line change here;
//! - **whether the notification hook's `sh -c` inherits `$CAPTAIN_MIAO_SOCK`.**
//!   It is `setsid`'d into its own process group and given three `GROK_*` vars;
//!   whether it inherits the rest of the agent's environment is not stated. If
//!   it does not, approval state silently never arrives and the fix is to embed
//!   `--sock` — at the cost of a per-session `config.toml`;
//! - **whether `only_unfocused = false` defeats the section-level
//!   `condition = "unfocused"` / `idle_threshold_secs`** on `[ui.notifications]`.
//!   Trigger an approval with the window focused;
//! - **the session-directory layout end to end** — the cwd encoder byte-for-byte,
//!   the `.cwd` fallback for names over 255 bytes, and the JSON key spellings in
//!   `summary.json` / `updates.jsonl`. That one probe unlocks tokens, model,
//!   titles and the resume picker together;
//! - **cheap confirmations**: that `--worktree=<name>` launches into a worktree
//!   from a non-TTY spawn, and that a `Stop` hook exiting 0 with empty stdout
//!   never blocks the turn. Also that **`--resume <id>` is right for an
//!   interactive spawn** — `10-hooks.md` gives `-r` as the *headless* spelling
//!   and cmux's adapter uses `-r` even for a terminal pane, so the two forms may
//!   simply be synonyms. If they are not, this arm is the one that has to move,
//!   and it is shared with Claude.
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
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "grok";

/// Our hook file inside `$GROK_HOME/hooks/`. A whole file of our own rather than
/// a merged one, because `hooks/` is a directory of independent files that Grok
/// globs — nothing of the user's is shadowed by it.
const HOOKS_FILE: &str = "captain-miao.json";

/// The substring that identifies *our* `[[ui.notifications.hooks]]` entry in a
/// config we did not write alone. Keyed on the invocation rather than on the
/// executable path, which moves between builds and installs — a marker that
/// stopped matching would leave one stale entry per rebuild in the user's copy.
const NOTIFY_HOOK_MARKER: &str = "hook --agent grok";

/// The one notification event worth registering (`notifications/config.rs`'s
/// `NotificationEventKind::ApprovalRequired`, spelled snake_case in config).
/// `turn_complete`, `session_ready` and `task_complete` are redundant with the
/// lifecycle hooks and would only spawn a second subprocess per turn to discard;
/// `agent_error` duplicates `StopFailure`.
const NOTIFY_EVENT_APPROVAL: &str = "approval_required";

// =============================================================================
// Filesystem locations
// =============================================================================

/// The real Grok home — `$GROK_HOME` if the user set one globally, else
/// `~/.grok` (`17-sessions.md`). This is what the synthetic home mirrors; it is
/// *not* what the launched agent is handed (see [`ensure_synth_home`]).
fn grok_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("GROK_HOME") {
        let p = PathBuf::from(h);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    dirs::home_dir().map(|h| h.join(".grok"))
}

/// A single shared synthetic `$GROK_HOME` for every Grok session: the real home
/// mirrored through symlinks, plus the two entries we own. Shared rather than
/// per-session because it is a symlink farm over the user's home — one stable
/// copy is cheaper to build and to reason about than one per launch — and that
/// sharing is exactly why neither our hooks file nor our notification hook may
/// carry per-session data (see [`build_hooks_settings`]).
fn synth_home() -> PathBuf {
    crate::state::state_dir().join("grok-home")
}

// =============================================================================
// Launcher: process spawn + synthetic GROK_HOME
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    // The launcher already wrote our hook-file contents to `settings_path`;
    // relocate them into the synthetic home, which is where Grok discovers
    // global hooks (there is no per-invocation `--settings` equivalent — that is
    // the whole reason this backend needs a home at all).
    let hooks_json =
        std::fs::read_to_string(settings_path).context("reading grok hook settings")?;
    let home = ensure_synth_home(&hooks_json)?;

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    cmd.env("GROK_HOME", &home);
    // The hook subprocess reads the launcher socket from here rather than from an
    // argv flag: the synthetic home is shared by every session, so neither the
    // hooks file nor `config.toml` can carry a per-session path. The notification
    // hook depends on the same variable surviving into a `setsid`'d `sh -c` —
    // unverified, and the first thing to check if approval state never appears.
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    // Only what the launcher forwarded (`--resume <id>`, `--worktree=<name>`).
    // **No cwd positional**: nothing in the sources says `grok` takes a directory
    // argument, and its optional positional is the kind a bare `--worktree` is
    // documented to swallow (`06-worktrees.md`), so the working directory is set
    // on the process and nowhere else.
    cmd.args(extra_args);
    Ok(cmd)
}

/// Create / refresh the synthetic home and return it. Two owned entries, and the
/// nesting between them is the whole subtlety:
///
/// - the **outer** mirror owns the `hooks` *directory*, so it is never replaced
///   by a symlink to the real one (which would send [`SynthHome::write_owned`]'s
///   write straight into the user's `~/.grok/hooks/`);
/// - the **inner** mirror rebuilds that directory's contents as symlinks to the
///   real `hooks/` entries, so the user's own global hooks keep firing inside a
///   captain-miao session, and adds `captain-miao.json` beside them.
///
/// `config.toml` is copied writable rather than linked: the real file is
/// frequently read-only (a nix-store / home-manager symlink), the agent persists
/// its own state into it, and we have to write the approval hook into it
/// ([`with_notification_hook`]). The copy is reseeded from the real file only
/// when *that* changes ([`CopiedEntry`]), so a `/model` change inside a session
/// survives; the reseed is why the merge below re-runs on every launch instead
/// of once.
///
/// The cost of the copy is the same one Codex pays: a first-run `grok` setup
/// performed *inside* a captain-miao session lands in the copy and is cleared
/// the next time the user edits their real config. Authenticate outside once.
fn ensure_synth_home(hooks_json: &str) -> Result<PathBuf> {
    let real = grok_home();
    let home = SynthHome {
        dir: synth_home(),
        real: real.clone(),
        owned: &["hooks"],
        copied: &[CopiedEntry {
            name: "config.toml",
            snapshot: ".config-source.toml",
        }],
    };
    home.ensure()?;

    let hooks = SynthHome {
        dir: home.dir.join("hooks"),
        real: real.map(|r| r.join("hooks")),
        owned: &[HOOKS_FILE],
        copied: &[],
    };
    hooks.ensure()?;
    hooks.write_owned(HOOKS_FILE, hooks_json)?;

    register_notification_hook(&home.dir);
    Ok(home.dir)
}

/// Merge our approval hook into the synthetic home's `config.toml`.
///
/// Best-effort throughout, like Codex's trust seeding: a garbled or unreadable
/// config leaves the file alone, which costs the `WaitingForApproval` state and
/// nothing else. Re-run every launch (and idempotent) so it survives the
/// [`CopiedEntry`] reseed that follows any edit to the user's real config.
fn register_notification_hook(home: &Path) {
    let path = home.join("config.toml");
    // A missing file is not a failure: the user may have no config at all, and a
    // config holding only our hook is a valid one.
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let Some(updated) = with_notification_hook(&current, &notification_hook_command()) else {
        tracing::warn!(
            "grok config.toml could not be updated; approval state will not be reported"
        );
        return;
    };
    if updated != current {
        let _ = atomic_write(&path, updated.as_bytes());
    }
}

/// The shell command Grok runs when an approval is pending.
///
/// The notification system runs `sh -c "<command>"` with **stdin on
/// `/dev/null`** — there is no JSON payload, and everything arrives in the
/// environment (`GROK_EVENT`, a *display* string like `"Approval required"`;
/// `GROK_MESSAGE`; `GROK_SESSION_ID`). Rather than teach `miao hook` a second
/// stdin contract, the registered command synthesizes the one payload field we
/// need and pipes it in — the shape [`parse_hook_payload`] already reads.
///
/// Output and exit status are discarded and the child is killed on timeout, so
/// nothing here can affect the session.
///
/// Carries no `--sock`: the socket rides `$CAPTAIN_MIAO_SOCK`, because one
/// `config.toml` serves every session (see [`synth_home`]).
fn notification_hook_command() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("miao"));
    let exe_q = shell_quote(&exe.to_string_lossy());
    format!(
        r#"printf '{{"sessionId":"%s"}}' "$GROK_SESSION_ID" | {exe_q} hook --agent grok {}"#,
        HookEvent::PermissionRequest.as_kebab()
    )
}

/// Insert (or replace) our `[[ui.notifications.hooks]]` entry in `config`,
/// returning the new document — or `None` when the config can't be parsed or its
/// `ui.notifications.hooks` isn't the shape Grok defines, in which case the
/// caller leaves the file untouched.
///
/// Pure so the three things that are easy to get silently wrong are pinned by
/// tests rather than by a live approval:
///
/// - **`only_unfocused = false`, set explicitly.** It defaults to `true`
///   (`notifications/config.rs`), which is close to the worst possible failure
///   mode for this feature: captain-miao would be told about a pending approval
///   only while the user's terminal is unfocused, so it would work in casual
///   testing and silently not work whenever they were watching the row.
/// - **`timeout_secs = 5`, set explicitly** — a hung socket write must not hold
///   a notification child open.
/// - **idempotence.** The entry is keyed by [`NOTIFY_HOOK_MARKER`] and replaced
///   in place, so relaunching (or rebuilding to a new exe path) can never
///   accumulate duplicates in a file the user also owns.
///
/// Everything else in the document is carried through, including the user's own
/// notification hooks. The round-trip through `toml::Table` does drop comments
/// and reflow the file — acceptable only because this is *our copy*, never the
/// user's real config.
fn with_notification_hook(config: &str, command: &str) -> Option<String> {
    use toml::Value;

    let mut doc: toml::Table = config.parse().ok()?;
    let ui = doc
        .entry("ui".to_string())
        .or_insert_with(|| Value::Table(toml::map::Map::new()));
    let Value::Table(ui) = ui else { return None };
    let notifications = ui
        .entry("notifications".to_string())
        .or_insert_with(|| Value::Table(toml::map::Map::new()));
    let Value::Table(notifications) = notifications else {
        return None;
    };
    let hooks = notifications
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(hooks) = hooks else {
        return None;
    };

    hooks.retain(|hook| {
        !hook
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c.contains(NOTIFY_HOOK_MARKER))
    });

    let mut entry = toml::map::Map::new();
    entry.insert("command".to_string(), Value::String(command.to_string()));
    entry.insert(
        "events".to_string(),
        Value::Array(vec![Value::String(NOTIFY_EVENT_APPROVAL.to_string())]),
    );
    entry.insert("only_unfocused".to_string(), Value::Boolean(false));
    entry.insert("timeout_secs".to_string(), Value::Integer(5));
    hooks.push(Value::Table(entry));

    toml::to_string(&doc).ok()
}

/// Build the contents of `$GROK_HOME/hooks/captain-miao.json`.
///
/// **The file's schema is the largest unverified thing in this module.** Grok
/// documents the *location* of lifecycle hooks (`custom-hooks.md`) but no source
/// read stated the JSON shape. This writes Claude's — `{"hooks": {<Event>:
/// [{matcher, hooks: [{type, command}]}]}}` — on three pieces of evidence: Grok
/// reads `~/.claude/settings.json` and `.cursor/hooks.json` directly, both of
/// which are that shape under a top-level `hooks` key; its config-file hooks are
/// spelled `[[hooks.<Event>]]`, i.e. an array of tables per PascalCase event;
/// and its matchers are written in *Claude's* tool vocabulary (`Bash`, `Task`,
/// `Edit`), which it rewrites to its own names internally.
///
/// **Which events are registered**, and on what basis:
/// - `PreToolUse`, `PostToolUse`, `Stop`, `StopFailure` are named in the sources.
/// - `SessionStart`, `UserPromptSubmit`, `PreCompact`, `PostCompact` are not, and
///   are registered anyway because `10-hooks.md` states that **unrecognized
///   event names are skipped** — so a name Grok lacks costs nothing, while
///   omitting one it has costs a row that never settles.
/// - `PermissionRequest` is deliberately absent: the lifecycle system has no
///   approval event, and the state arrives over the notification hook instead
///   ([`notification_hook_command`]). `PermissionDenied` exists but fires *after*
///   a refusal, when there is no state of ours left to move.
/// - `PostToolUseFailure`, `Elicitation`, `ElicitationResult` and `CwdChanged`
///   are Claude affordances with no evidence behind them here; each either
///   settles identically to an event we do register or moves nothing.
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

    let hook = |event: HookEvent| -> serde_json::Value {
        serde_json::json!([{
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": format!("{exe_q} hook --agent grok {}", event.as_kebab()),
            }],
        }])
    };
    // Same, plus the explicit timeout that only `Stop` needs.
    let hook_with_timeout = |event: HookEvent, timeout: u64| -> serde_json::Value {
        serde_json::json!([{
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": format!("{exe_q} hook --agent grok {}", event.as_kebab()),
                "timeout": timeout,
            }],
        }])
    };

    serde_json::json!({
        "hooks": {
            "SessionStart":     hook(HookEvent::SessionStart),
            "UserPromptSubmit": hook(HookEvent::PromptSubmit),
            "PreToolUse":       hook(HookEvent::PreToolUse),
            "PostToolUse":      hook(HookEvent::PostToolUse),
            "Stop":             hook_with_timeout(HookEvent::Stop, 5),
            "StopFailure":      hook(HookEvent::StopFailure),
            "PreCompact":       hook(HookEvent::PreCompact),
            "PostCompact":      hook(HookEvent::PostCompact),
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
/// governs everything else here and is the single most likely thing to be
/// silently wrong if the payload moves.
///
/// Documented fields deliberately left out: `workspaceRoot` (the repo root;
/// `cwd` is what the row shows), `timestamp`, `permissionMode` (`default` |
/// `auto` | `plan` | `bypassPermissions` — free plan-mode detection, noted and
/// not built), `toolInput`, `toolUseId`, `toolInputTruncated`, and `Stop`'s
/// `backgroundTasks` / `sessionCrons` (see the module doc).
///
/// **No prompt field is read, because none is documented.** Every field above is
/// accounted for and none of them carries the user's prompt text, so a Grok row
/// shows no prompt until a real `user_prompt_submit` payload is captured. A
/// plausible guess (`prompt`, Claude's spelling) would be indistinguishable from
/// the agent not sending one, which is precisely the failure this module refuses
/// to build in. The raw payload is forwarded regardless, so nothing is lost.
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
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse grok hook JSON from stdin")?;
    Ok(HookMessage {
        event,
        // Empty is *absent*, not a new identity. The approval hook synthesizes
        // this field from `$GROK_SESSION_ID` in shell, so an unset variable would
        // otherwise arrive as `""` and overwrite the launcher's real session id
        // with nothing (`adopt_session_facts` takes the freshest id it is
        // given).
        session_id: payload.session_id.filter(|s| !s.trim().is_empty()),
        tool_name: payload.tool_name,
        // No documented error field on `StopFailure`; `dispatch_default` falls
        // back to the raw payload for `last_error`, which is at least honest
        // about what the agent actually said.
        message: None,
        cwd: payload.cwd,
        // Not in the payload — see [`HookPayload`].
        prompt: None,
        // Grok's title is per-session in `summary.json`, not on the hook.
        session_title: None,
        // Grok records both per session, but on disk rather than on the
        // payload; see the module doc's probe list.
        context_tokens: None,
        model: None,
        // Derivable but deliberately not derived — see the module doc. This is
        // the field the launcher gates its entire transcript watch on, so `None`
        // is what keeps the empty stats fold and the absent interrupt scan
        // consistent rather than merely unimplemented.
        transcript_path: None,
        raw: Some(stdin.to_string()),
    })
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

/// Grok's two departures from [`common::dispatch_default`]; everything else maps
/// the way every backend maps it.
pub async fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    // 1. A session-end `Stop` is not a turn end. Harmless for status either way
    //    (the row is on its way out), but it is also the payload that will carry
    //    `backgroundTasks` once those are wired, and reading *that* list from a
    //    shutdown is how a session ends up looking like it has live background
    //    work. Getting it right now costs one branch.
    if msg.event == HookEvent::Stop && is_session_end_stop(msg.raw.as_deref()) {
        common::adopt_session_facts(state, &mut msg);
        return;
    }

    match msg.event {
        // 2. Events no hook of ours registers, so they never reach this
        //    dispatcher (see `build_hooks_settings`). Ignored explicitly rather
        //    than mapped defensively — the exhaustive match that forces a
        //    decision on a newly-added `HookEvent` variant is
        //    `common::dispatch_default`'s.
        //
        //    `PermissionRequest` is *not* in this list: it is the one event that
        //    arrives from outside the lifecycle system, over the notification
        //    hook, and it takes the shared arm.
        HookEvent::PostToolUseFailure
        | HookEvent::Elicitation
        | HookEvent::ElicitationResult
        | HookEvent::CwdChanged => {}
        _ => common::dispatch_default(state, msg),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentControl;
    use crate::state::SessionStatus;

    /// **Hand-written from the payload documented in `10-hooks.md`, not captured
    /// from a running binary** — no `grok` was installed when these were written.
    /// A probe that captures real payloads (point a hook command at `tee`) should
    /// diff them against these and correct them here first.
    fn payload(event: &str, extra: &str) -> String {
        format!(
            r#"{{"hookEventName":"{event}","sessionId":"s1","cwd":"/home/miao/p",
               "workspaceRoot":"/home/miao/p","permissionMode":"default",
               "timestamp":"2026-04-14T12:00:00Z"{extra}}}"#
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState {
            agent: AgentControl::Grok,
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
            &payload("user_prompt_submit", ""),
        );
        assert_eq!(state.status, SessionStatus::Active);

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

    /// Approval is the one state that arrives from outside the lifecycle hooks,
    /// carrying nothing but the session id the shell command synthesized. It must
    /// still reach `WaitingForApproval`.
    #[test]
    fn the_notification_hooks_minimal_payload_reaches_waiting_for_approval() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            r#"{"sessionId":"s1"}"#,
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
        assert_eq!(state.session_id.as_deref(), Some("s1"));
    }

    /// `$GROK_SESSION_ID` unset makes the approval hook print `"sessionId":""`.
    /// An empty id is *absent*, never a rename of the session to nothing — taking
    /// it would blank the id every hook after it depends on.
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
        let stdin = payload("post_tool_use", r#","toolName":"search_replace""#);
        let msg = parse_hook_payload(HookEvent::PostToolUse, &stdin).expect("parses");
        assert_eq!(msg.session_id.as_deref(), Some("s1"));
        assert_eq!(msg.cwd.as_deref(), Some("/home/miao/p"));
        assert_eq!(msg.tool_name.as_deref(), Some("search_replace"));
        // No transcript path is derived, which is what keeps the launcher's
        // transcript machinery inert for Grok (see the module doc).
        assert_eq!(msg.transcript_path, None);
        // A snake_case reading would find none of the above; guard the one field
        // whose absence would otherwise look like "the agent didn't send it".
        assert!(
            parse_hook_payload(HookEvent::Stop, r#"{"tool_name":"run_terminal_command"}"#)
                .expect("parses")
                .tool_name
                .is_none()
        );
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
                "PostCompact",
                "PostToolUse",
                "PreCompact",
                "PreToolUse",
                "SessionStart",
                "Stop",
                "StopFailure",
                "UserPromptSubmit",
            ],
            "approval is not a lifecycle event here — it arrives over the \
             notification hook"
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
        assert_eq!(hooks["PreToolUse"][0]["matcher"], "*");
    }

    /// The approval hook's whole contract in one place: it synthesizes the JSON
    /// `parse_hook_payload` reads (there is no stdin), names the event our
    /// forwarder expects, and carries no socket — because `config.toml`, like the
    /// hooks file, is shared by every session.
    #[test]
    fn the_notification_hook_command_synthesizes_a_payload_we_can_parse() {
        let cmd = notification_hook_command();
        assert!(
            cmd.contains(r#"printf '{"sessionId":"%s"}' "$GROK_SESSION_ID""#),
            "{cmd}"
        );
        assert!(
            cmd.ends_with("hook --agent grok permission-request"),
            "{cmd}"
        );
        assert!(!cmd.contains("--sock"), "{cmd}");
        // The format string above, filled in, must be exactly what the parser
        // accepts — the two halves of this mechanism live in different processes
        // and nothing else pins them together.
        let msg = parse_hook_payload(HookEvent::PermissionRequest, r#"{"sessionId":"abc-123"}"#)
            .expect("the synthesized payload parses");
        assert_eq!(msg.session_id.as_deref(), Some("abc-123"));
    }

    /// The three things about the config merge that are easy to get silently
    /// wrong: the explicit `only_unfocused = false` (its default of `true` would
    /// report approvals only while the user is looking away), the explicit
    /// timeout, and the event list.
    #[test]
    fn the_approval_hook_is_registered_with_the_gates_set_explicitly() {
        let merged = with_notification_hook("", "miao-cmd hook --agent grok permission-request")
            .expect("an empty config merges");
        let doc: toml::Table = merged.parse().expect("valid TOML");
        let hooks = doc["ui"]["notifications"]["hooks"]
            .as_array()
            .expect("an array of hook tables");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["only_unfocused"], toml::Value::Boolean(false));
        assert_eq!(hooks[0]["timeout_secs"], toml::Value::Integer(5));
        assert_eq!(
            hooks[0]["events"].as_array().expect("an events array"),
            &[toml::Value::String("approval_required".to_string())]
        );
    }

    /// The merge runs on every launch and the copy it edits is reseeded whenever
    /// the user's real config changes, so it has to be idempotent *and* has to
    /// leave everything it didn't write alone — including the user's own
    /// notification hooks, which live in the same array.
    #[test]
    fn the_merge_is_idempotent_and_keeps_the_users_config() {
        // `zz_model` is deliberately a root-level scalar whose key sorts *after*
        // every table's: TOML forbids a bare key following a table header, so a
        // serializer that wrote this document in key order would fail on it and
        // the merge would silently give up on precisely the configs that have
        // one (`05-configuration.md` puts `model` at the root).
        let user = r#"
zz_model = "grok-build-0.1"

[ui]
theme = "dark"

[ui.notifications]
condition = "unfocused"

[[ui.notifications.hooks]]
command = "notify-send Grok"
events = ["turn_complete"]

[permissions]
allow = ["read"]
"#;
        let once = with_notification_hook(user, "/old/miao hook --agent grok permission-request")
            .expect("merges");
        // A rebuild moves the exe path; the entry must be replaced, not doubled.
        let twice = with_notification_hook(&once, "/new/miao hook --agent grok permission-request")
            .expect("merges again");
        let thrice =
            with_notification_hook(&twice, "/new/miao hook --agent grok permission-request")
                .expect("merges a third time");
        assert_eq!(twice, thrice, "the merge must be idempotent");

        let doc: toml::Table = twice.parse().expect("valid TOML");
        assert_eq!(doc["zz_model"].as_str(), Some("grok-build-0.1"));
        assert_eq!(doc["ui"]["theme"].as_str(), Some("dark"));
        assert_eq!(doc["permissions"]["allow"].as_array().unwrap().len(), 1);
        assert_eq!(
            doc["ui"]["notifications"]["condition"].as_str(),
            Some("unfocused")
        );
        let hooks = doc["ui"]["notifications"]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 2, "{twice}");
        assert_eq!(hooks[0]["command"].as_str(), Some("notify-send Grok"));
        assert_eq!(
            hooks[1]["command"].as_str(),
            Some("/new/miao hook --agent grok permission-request")
        );
    }

    /// A config we can't read is left alone: losing approval state is a bad
    /// afternoon, and rewriting the file the agent reads its model and
    /// permissions from is a worse one.
    #[test]
    fn an_unparseable_config_is_left_untouched() {
        assert!(with_notification_hook("this is not = = toml", "c").is_none());
        // Same when the key exists but isn't the shape Grok defines.
        assert!(with_notification_hook("[ui]\nnotifications = 3\n", "c").is_none());
        assert!(
            with_notification_hook("[ui.notifications]\nhooks = \"one\"\n", "c").is_none(),
            "a scalar where the array of hook tables belongs"
        );
    }
}
