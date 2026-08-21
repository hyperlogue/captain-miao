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
//! **Isolation is `GROK_HOME`, and it has one extra wrinkle.**
//! `17-sessions.md`: *"Set `GROK_HOME` to override the base directory; when it
//! is unset, Grok uses `~/.grok`"* — and it moves everything, not just config:
//! sessions, auth, memory, skills, plugins, agents and logs. So the synthetic
//! home mirrors the real one through symlinks ([`super::synth_home`]) and we own
//! exactly two things:
//!
//! - **`hooks/captain-miao.json`.** `hooks/` is a *directory* of independent
//!   files, so unlike an agent with one global `hooks.json` there is no merge
//!   to get right — but there is a nested mirror to get right, which is why
//!   [`ensure_synth_home`] builds a second [`SynthHome`] *inside* the first. Own
//!   the directory without it and the user's own global hooks stop firing in
//!   every captain-miao session; leave the directory to the outer mirror and our
//!   file would be written **through a symlink into the user's real `~/.grok`**,
//!   which is the one thing a synthetic home exists to prevent.
//! - **`config.toml`, a writable copy** rather than a symlink. Needed so the
//!   pager-notify approval fallback can be merged in, and so Grok can persist
//!   into a file that is frequently a read-only home-manager symlink.
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
//! than Codex, which pre-trusts its command hooks inside captain-miao's owned
//! profile because it has no always-trusted user tier.
//!
//! **Approval has two sites, on purpose.** The lifecycle `Notification` event
//! with matcher `permission_prompt` is the 1.0.4 path (JSON on stdin, same
//! hooks file as everything else). The pager's `[[ui.notifications.hooks]]`
//! entry in `config.toml` is kept as a fallback for older grok, and is the
//! reason `config.toml` is still a writable copy: drop that merge and `hooks/`
//! alone would do. The pager path's contract still bites when it is the one
//! that fires (see [`notification_hook_command`] and [`with_notification_hook`]):
//! stdin is `/dev/null`, everything arrives in the environment, and
//! `only_unfocused` **defaults to `true`**.
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
use super::synth_home::{CopiedEntry, SynthHome, atomic_write};
use crate::agent::{ResumeCandidate, TranscriptStats};
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

/// Where Grok keeps its sessions: `$GROK_HOME/sessions/<cwd-key>/<id>/`, each
/// holding `summary.json`, `chat_history.jsonl` and `updates.jsonl`.
///
/// `<cwd-key>` is an encoding of the session's working directory, and this
/// module never decodes it — Grok's own resolver doesn't either when it has only
/// an id (`resolve_local_session_any_cwd_in_root` walks every key), and the cwd
/// we want is inside `summary.json` anyway. So the key is a directory to iterate,
/// never a string to parse.
///
/// Prefers the synthetic home's `sessions/` when that directory exists (a
/// symlink to the real one after [`ensure_synth_home`] has run, or a leftover
/// shadow from a launch that predates the seed). The dashboard process has no
/// `GROK_HOME` of its own, so walking only `~/.grok/sessions` would miss every
/// session minted inside captain-miao.
fn sessions_root() -> Option<PathBuf> {
    let synth = synth_home().join("sessions");
    if synth.is_dir() {
        return Some(synth);
    }
    Some(grok_home()?.join("sessions"))
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
// Resume picker
// =============================================================================

/// Grok's `summary.json`, of which four fields are wanted here. Grok writes
/// more (`num_messages`, `parent_session_id`, `forked_at`, cwd-relocation
/// bookkeeping); everything unnamed is ignored rather than refused, so a Grok
/// that grows a field still parses.
#[derive(Deserialize)]
struct SessionSummary {
    #[serde(default)]
    info: SummaryInfo,
    /// Grok's own label for the session — what its `grok sessions` listing
    /// shows and what its session search matches on. The dashboard treats it as
    /// the row's title.
    #[serde(default)]
    session_summary: String,
}

#[derive(Deserialize, Default)]
struct SummaryInfo {
    /// The session's authoritative working directory. Grok tracks moves through
    /// a generation counter beside it; this is always the current one.
    #[serde(default)]
    cwd: String,
    /// Branch checked out when the session last saved. Grok's worktrees live
    /// in its own registry rather than beside the repo, so this is the only
    /// branch name the picker can show.
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
/// No token count comes back with it, and that is Grok's design rather than a
/// gap here: its usage ledgers are explicitly not serialized, so the only
/// numbers on disk are per-request ones in `chat_history.jsonl` that would have
/// to be re-folded into a context total. The model *is* on disk, and rides
/// along.
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
        out.push(ResumeCandidate {
            agent: crate::agent::AgentControl::Grok,
            session_id: session_id.to_string(),
            cwd: summary.info.cwd,
            first_prompt: None,
            custom_title: Some(summary.session_summary).filter(|t| !t.trim().is_empty()),
            git_branch: Some(summary.info.head_branch).filter(|b| !b.trim().is_empty()),
            mtime,
        });
    }
    out
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
/// The cost of the copy is the same one Codex pays, and it is now limited to
/// *configuration*: a `/model` change made inside a captain-miao session lands
/// in the copy and is cleared the next time the user edits their real config.
/// Credentials are not affected — `auth.json` is linked rather than copied, and
/// a first login, which has no real file to link to yet, is moved back out by
/// [`SynthHome::adopt_agent_writes`].
/// State directories Grok writes on first launch. Seeded in the *real* home
/// so they exist to be linked — otherwise the agent mints them inside the
/// synthetic home as a shadow the dashboard (which resolves `~/.grok`) never
/// sees, and which [`SynthHome::ensure`] then quarantines the moment the real
/// home grows the name. Same lesson as Kimi's `credentials/`.
const SEEDED_STATE_DIRS: &[&str] = &["sessions", "logs", "relocations"];

/// Top-level files Grok creates in its home. A dangling symlink lets
/// `open(O_CREAT)` land the first write in the real home, the same trick
/// Kimi uses for `session_index.jsonl`.
const SEEDED_STATE_FILES: &[&str] = &[
    "worktrees.db",
    "trusted_folders.toml",
    "active_sessions.json",
];

fn seed_real_state(real: &Path, synth: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::symlink_metadata(real).is_err() && std::fs::create_dir_all(real).is_ok() {
        let _ = std::fs::set_permissions(real, std::fs::Permissions::from_mode(0o700));
    }
    for name in SEEDED_STATE_DIRS {
        // A real directory already in the synthetic home is a shadow; seeding
        // the real side first would make adopt skip it and the linking pass
        // quarantine the live session tree.
        let synth_p = synth.join(name);
        if let Ok(meta) = std::fs::symlink_metadata(&synth_p)
            && !meta.file_type().is_symlink()
        {
            continue;
        }
        let dest = real.join(name);
        if std::fs::symlink_metadata(&dest).is_err() && std::fs::create_dir_all(&dest).is_ok() {
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700));
        }
    }
}

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
        // Auto-managed credentials plus the state trees that a first-ever
        // captain-miao grok launch otherwise mints as shadows. Linking only
        // works once the name exists in the real home, so anything already
        // written into the synthetic copy is moved back out.
        adopted: &[
            "auth.json",
            "sessions",
            "logs",
            "relocations",
            "worktrees.db",
            "trusted_folders.toml",
            "active_sessions.json",
        ],
        prune: false,
    };
    if let Some(real) = &real {
        seed_real_state(real, &home.dir);
    }
    home.ensure()?;
    if let Some(real) = &real {
        for name in SEEDED_STATE_FILES {
            let link = home.dir.join(name);
            if std::fs::symlink_metadata(&link).is_err() {
                let _ = std::os::unix::fs::symlink(real.join(name), &link);
            }
        }
    }

    let hooks = SynthHome {
        dir: home.dir.join("hooks"),
        real: real.map(|r| r.join("hooks")),
        owned: &[HOOKS_FILE],
        copied: &[],
        // Hooks are configuration, never agent state.
        adopted: &[],
        // A loader-scanned collection: a hook the user deletes must not leave
        // a dangling import behind (see [`SynthHome::prune`]).
        prune: true,
    };
    hooks.ensure()?;
    hooks.write_owned(HOOKS_FILE, hooks_json)?;

    register_notification_hook(&home.dir);
    Ok(home.dir)
}

/// Merge our approval hook into the synthetic home's `config.toml`.
///
/// Best-effort throughout, like Codex's profile trust generation: a garbled or
/// unreadable config leaves the file alone, which costs the
/// `WaitingForApproval` state and nothing else. Re-run every launch (and
/// idempotent) so it survives the [`CopiedEntry`] reseed that follows any edit
/// to the user's real config.
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
pub(crate) fn notification_hook_command() -> String {
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
///   lifecycle hook that fires while a permission UI is waiting. The
///   `[[ui.notifications.hooks]]` entry in `config.toml` is kept as a fallback
///   for grok versions that only have the pager notify path.
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
    /// names one. Rewritten to sibling `signals.json` so the launcher watches
    /// the small stats file rather than every ACP append.
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
        // Empty is *absent*, not a new identity. The approval hook synthesizes
        // this field from `$GROK_SESSION_ID` in shell, so an unset variable would
        // otherwise arrive as `""` and overwrite the launcher's real session id
        // with nothing (`adopt_session_facts` takes the freshest id it is
        // given).
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
            .map(|p| signals_path_for(&p)),
        raw: Some(stdin.to_string()),
        session_is_child,
    })
}

/// Point the launcher at `signals.json` in the same session directory as
/// `transcript`. Grok's envelope names `updates.jsonl`, which appends on every
/// ACP event; the context total and model live in the sibling sidecar and
/// rewrite at turn boundaries.
fn signals_path_for(transcript: &str) -> String {
    let path = Path::new(transcript);
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    dir.join("signals.json").to_string_lossy().into_owned()
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

/// Context total and model from the session directory Grok names on the hook.
///
/// `path` is the `signals.json` [`parse_hook_payload`] rewrites `transcriptPath`
/// to. The context gauge is `contextTokensUsed` — the in-memory billing ledgers
/// still aren't serialized, but this sidecar is, and it is what `/session-info`
/// shows. `prior` is unused: both files are small whole-JSON documents.
pub fn read_transcript_stats(path: &Path) -> TranscriptStats {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
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

    #[derive(Deserialize, Default)]
    struct Summary {
        #[serde(default)]
        current_model_id: Option<String>,
    }
    if stats.model.is_none()
        && let Ok(body) = std::fs::read_to_string(dir.join("summary.json"))
        && let Ok(summary) = serde_json::from_str::<Summary>(&body)
    {
        stats.model = summary.current_model_id.filter(|m| !m.trim().is_empty());
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
                    "session_summary":"wire up the parser",
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
            Some("/home/miao/p/s1/signals.json")
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

    /// `StopCancelled` is registered as `stop`, so an interrupt settles the row
    /// the same way a genuine turn end does — the gap this backend used to ship.
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

    /// `signals.json` is the context gauge `/session-info` shows; the model
    /// falls through to `summary.json` when the sidecar has none.
    #[test]
    fn signals_json_folds_tokens_and_model() {
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
            r#"{"current_model_id":"ignored-when-signals-has-one"}"#,
        )
        .unwrap();
        let stats = read_transcript_stats(&dir.join("signals.json"));
        assert_eq!(stats.context_tokens, Some(8929));
        assert_eq!(stats.model.as_deref(), Some("grok-4.6"));
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
