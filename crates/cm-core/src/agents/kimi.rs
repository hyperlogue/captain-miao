//! Kimi Code CLI backend. Owns every Kimi-specific path, env var and hook
//! payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::Kimi`'s match arms.
//!
//! **Documented, never run, never source-read.** No `kimi` binary and no vendor
//! checkout were available when this was written, so every claim below comes
//! from MoonshotAI/kimi-code's published CLI and hooks documentation as recorded
//! in this project's design note (§7). That is a weaker footing than the other
//! three backends have — Claude and Codex were built against running binaries,
//! Reasonix against its source — so each claim is marked **documented** or
//! **assumed** where it matters, and everything a probe must settle is listed at
//! the bottom of this doc. Nothing here has been observed.
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
//! **Isolation is the Codex pattern, unchanged.** `KIMI_CODE_HOME` relocates the
//! entire data dir, so `<state>/kimi-home/` is a symlink farm over the real
//! `~/.kimi-code/` with `config.toml` a writable **copy** carrying our
//! `[[hooks]]` entries. Copy rather than symlink is the lesson Codex already
//! paid for (AGENTS.md; a symlinked config gave a split-brain database), and it
//! buys a second thing here: the config we edit is *ours*, so a `[[hooks]]`
//! block Kimi refuses can only break a captain-miao session's config, never the
//! user's real one. That matters more than usual, because a Kimi hook table
//! carrying **any fifth key fails the whole config load** rather than that one
//! entry — see [`build_hooks_settings`], which emits exactly `event`, `matcher`,
//! `command`, `timeout` for that reason.
//!
//! Two risks carried over from Codex:
//!
//! - **`sessions/` is reached through a symlink**, and on macOS the notify event
//!   reports the resolved path while Linux echoes the registered one. Kimi's tree
//!   has the same shape, so it inherits the same handling — inert today, since
//!   nothing of ours watches a Kimi transcript (below).
//! - **`sessions/<workDirKey>/<sessionId>/agents/*/wire.jsonl` is almost
//!   certainly appended through a long-held fd**, the condition that defeats
//!   macOS FSEvents entirely, so [`crate::agent::AgentControl::transcript_poll_interval`]
//!   answers `Some(2s)` there, matching Codex. **This machine is Linux and cannot
//!   test it** — that is unverified, and no macOS support is claimed. It is also
//!   *moot* today and set anyway: no hook payload names a transcript, so the
//!   launcher starts no watch of either kind. It is set now so that whoever wires
//!   a transcript path up does not silently inherit an event-driven watch macOS
//!   cannot deliver.
//!
//! **What is deliberately not read.** The token and model columns and the resume
//! picker are empty for Kimi, and that is a stated gap rather than an oversight:
//! `wire.jsonl` is documented to carry "message history, request traces, tool
//! schemas and parameters", but *no field name* for token usage or model is
//! documented, and `<workDirKey>`'s derivation is undocumented too. A plausible
//! guess at either would put a wrong number in a column users read as fact. The
//! arms return `TranscriptStats::default()` / `Ok(vec![])` and say what would
//! fill them. (If a session directory ever does need resolving, glob
//! `sessions/*/<sessionId>/` rather than recomputing the key.)
//!
//! What a probe against a real binary must settle, in the order the failures
//! hurt:
//! - **that a copied `config.toml` with our appended `[[hooks]]` loads at all**,
//!   and specifically that `matcher = "*"` is accepted. The matcher's grammar is
//!   undocumented; `"*"` is what Claude, Codex and Reasonix all spell "every
//!   tool", and it is an *assumption* here. If Kimi validates it as a regex, or
//!   requires the key absent rather than wildcarded, the whole config fails to
//!   load and every row sits at `Starting`.
//! - **whether Kimi content-hashes `config.toml` for trust** the way Codex does.
//!   If it does, an equivalent of Codex's `seed_hook_trust` is needed, and the
//!   regression test pinning the hash must land with it. [`merge_hooks`] already
//!   keeps the file byte-stable across launches, which is the precondition.
//! - **whether `CAPTAIN_MIAO_SOCK` survives into the hook subprocess.** The
//!   socket rides the environment rather than the command (the config is shared
//!   by every session), so an agent that scrubs its hook env would leave every
//!   hook unable to find its launcher — silently, since a hook that cannot
//!   connect is not an error Kimi reports.
//! - **the per-event field names** `tool_name`, `prompt` and `hook_event_name`.
//!   Only the session-wide fields are documented; these three are assumed from
//!   the payload being snake_case and the event vocabulary being Claude's. A
//!   wrong guess costs the Tool column and the prompt preview, never a wrong
//!   value (see [`HookPayload`]).
//! - **`<workDirKey>`** — hash or slug — and `wire.jsonl`'s field names for token
//!   usage and model. Together these are the whole token/model/resume gap.
//! - **that there is genuinely no fork** (`kimi --help | grep -i fork`). If one
//!   appears, [`crate::agent::AgentControl::resume_args`] grows a flag under
//!   `fork` and `supports_fork()` flips itself — nothing else changes.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::synth_home::{CopiedEntry, SynthHome, atomic_write};
use super::{find_in_path, shell_quote};
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
// Launcher: process spawn + synthetic KIMI_CODE_HOME
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    let kimi_bin = find_in_path(BIN).with_context(|| format!("{BIN} not found in PATH"))?;

    // The launcher already wrote our hook block to `settings_path`; merge it
    // into the synthetic home's config.toml, which is the only place Kimi looks
    // for hooks (there is no override flag and no per-invocation injection).
    // Note the file the launcher wrote is named `…-settings.json` and holds
    // **TOML** — that path is generic transport, opaque to the launcher, and
    // every backend puts its own format through it.
    let hooks_toml =
        std::fs::read_to_string(settings_path).context("reading kimi hook settings")?;
    let home = ensure_synth_home(&hooks_toml)?;

    let has_envrc = Path::new(cwd).join(".envrc").is_file();
    let mut cmd = match has_envrc.then(|| find_in_path("direnv")).flatten() {
        Some(direnv) => {
            let mut c = Command::new(direnv);
            c.args(["exec", cwd]).arg(&kimi_bin);
            c
        }
        None => Command::new(&kimi_bin),
    };

    cmd.current_dir(cwd);
    cmd.env("KIMI_CODE_HOME", &home);
    super::with_shim_path(&mut cmd, shim_dir);
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
/// `config.toml` is **copied, not symlinked**, for the reason Codex established:
/// the real file is frequently read-only (a nix-store / home-manager symlink),
/// and an agent that writes back into its own home then fails on that write. It
/// also has to be a copy here for a second reason — we *edit* it, and editing
/// the user's real config to inject our hooks is not ours to do.
///
/// `owned` is empty: unlike Claude's `settings.json`, Codex's `hooks.json` and
/// Reasonix's `settings.json`, Kimi has no separate hooks file to own. The hooks
/// live inside the config, which is why this backend needs the copy mechanism
/// that Reasonix did not.
fn ensure_synth_home(hooks_toml: &str) -> Result<PathBuf> {
    let home = SynthHome {
        dir: synth_home(),
        real: kimi_home(),
        owned: &[],
        copied: &[CopiedEntry {
            name: "config.toml",
            snapshot: ".config-source.toml",
        }],
    };
    home.ensure()?;
    merge_hooks(&home.dir.join("config.toml"), hooks_toml);
    Ok(home.dir)
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
/// Best-effort, like Codex's trust seeding, and asymmetric on purpose: a config
/// that is merely **absent** is created, but one that is present and doesn't
/// parse — or whose `hooks` key holds something that isn't an array of tables —
/// is left byte-for-byte alone. Treating an unparseable config as an empty one
/// would replace the user's file with nothing but our hooks, and the file we
/// hold is their whole configuration mirrored. Losing the hooks (the session
/// runs untracked) is the cheaper failure by a wide margin.
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
/// **Exactly four keys per entry — `event`, `matcher`, `command`, `timeout`.**
/// A fifth fails Kimi's *entire* config load, not just that entry, so this is
/// pinned by a test rather than left to review. `timeout` is seconds, range
/// 1–600, default 30; it is set explicitly and small because three of the events
/// we register (`UserPromptSubmit`, `PreToolUse`, `Stop`) are blockable, so a
/// timeout there stalls the turn for its full duration. A socket write needs
/// none of 30s.
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
        // Assumed, not documented — see the module doc's probe list. "*" is what
        // all three shipping backends spell "every tool".
        t.insert("matcher".to_string(), toml::Value::String("*".to_string()));
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
        // Kimi's payload names neither; both wait on `wire.jsonl`'s field
        // names (module doc).
        context_tokens: None,
        model: None,
        transcript_path: None,
        raw: Some(stdin.to_string()),
    })
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
        LauncherState {
            agent: AgentControl::Kimi,
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

    /// The constraint that would take a whole config down: a Kimi hook table
    /// accepts **exactly** `event`, `matcher`, `command`, `timeout`, and a fifth
    /// key fails the entire load rather than that one entry.
    #[test]
    fn every_hook_entry_has_exactly_the_four_documented_keys() {
        let entries = hook_entries(&build_hooks_settings("/run/x.sock"));
        assert!(!entries.is_empty());
        for entry in &entries {
            let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["command", "event", "matcher", "timeout"],
                "a fifth key fails Kimi's whole config load: {entry:?}"
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
