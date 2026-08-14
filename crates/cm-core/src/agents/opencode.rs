//! opencode CLI backend. Owns every opencode-specific path, env var and hook
//! payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::OpenCode`'s match arms.
//!
//! **Neither run nor source-read — derived from this project's design note
//! (§9) alone, and that section is the one the note never marked
//! source-verified.** §5 (Grok), §6 (Reasonix) and §8 (Pi) each carry a
//! citation trail back to a vendor checkout; §9 does not. No `opencode` binary
//! and no `sst/opencode` checkout were available here either. So this is the
//! **weakest-footed backend in the tree** — weaker than Kimi, which at least
//! came from published vendor documentation — and it is built to fail *visibly
//! and cheaply* rather than to look complete: every fact §9 states is honoured
//! exactly, and every fact it does not state leaves the corresponding field
//! `None` with the missing name written down. Nothing here has been observed.
//!
//! **The delivery is a generated JavaScript plugin, and that is the one
//! genuinely new mechanism in this backend.** opencode has no shell-command
//! hooks at all (§9's facts table: *"hooks — none; extensibility is JS/TS
//! plugin modules only"*). Its event surface is a module exporting a function
//! that receives `{project, client, $, directory, worktree}` and returns a
//! handlers object. So [`plugin_source`] emits that module,
//! [`crate::agent::AgentControl::hooks_settings_json`] returns its **JS
//! source** — the seam calls it "the per-session hook-settings file" and
//! nothing requires it to be JSON; Kimi already puts TOML through it — and
//! [`build_launch_command`] relocates it into `<state>/opencode-config/plugins/`
//! behind `OPENCODE_CONFIG_DIR`. Structurally this is Codex's and Grok's
//! per-session-isolated injection; only the payload language differs.
//!
//! The cost is real and worth naming: captain-miao now ships generated
//! JavaScript in a tree with no JS toolchain and no way to execute it in a
//! test. §9.1's containment is therefore followed to the letter and is not
//! negotiable —
//!
//! - the plugin does **no logic**: no filtering, no state, no retries, no reads
//!   of the agent's own data. It serializes what it was handed and spawns
//!   `miao hook --agent opencode <event>`;
//! - the socket arrives via `$CAPTAIN_MIAO_SOCK`, never spliced into the file,
//!   so one plugin serves every session byte-for-byte (and nothing in the JS
//!   needs shell quoting: the child is spawned with an argv array, never
//!   through a shell);
//! - the tests cover **generation, not execution**: the source is byte-stable
//!   across sockets, every handler key maps to a real [`HookEvent`], and
//!   [`tests::the_generated_plugin_is_byte_for_byte_what_we_think_it_is`] is a
//!   full snapshot, so any edit to the template fails loudly instead of
//!   shipping unreviewed JS.
//!
//! **`plugins/` is owned *and* mirrored, which is the trap this backend shares
//! with Grok's `hooks/`.** `OPENCODE_CONFIG_DIR` relocates agents, commands,
//! modes **and** plugins together, so the synthetic dir is a symlink farm over
//! the real one ([`super::synth_home`]) — and `plugins/` is the one entry that
//! can be neither linked nor simply owned. Link it and
//! [`super::synth_home::SynthHome::write_owned`] writes our module **through the
//! symlink into the user's real `~/.config/opencode/plugins/`**, which is the
//! one thing a synthetic home exists to prevent. Own it without mirroring and
//! every plugin the user has stops loading inside a captain-miao session,
//! silently. [`ensure_synth_config`] therefore builds a **second [`SynthHome`]
//! inside the first**, exactly as `agents::grok` does for `hooks/`.
//!
//! Nothing is **copied**: opencode's config is `opencode.json`/`opencode.jsonc`
//! and we neither edit it nor need the agent to write back through us (Codex
//! and Kimi copy only because they persist hook trust / carry our hook block
//! *in* the config). A symlink is enough, and it keeps a `/model` change inside
//! a captain-miao session landing in the user's real file.
//!
//! **What this backend deliberately does not report, and why each is a name we
//! do not have rather than a feature out of reach.** §9.3 names the *events*;
//! it names no **field inside any payload**. A guessed field name would put a
//! wrong value in a column read as fact, so every one of these is `None` and
//! the raw envelope is forwarded whole so that a single captured payload turns
//! several of them on at once:
//!
//! - **No session id.** This is the expensive one: `LauncherState.session_id`
//!   is what `r` / `f` resume from, so an opencode row **cannot be resumed from
//!   the dashboard** — [`crate::agent::AgentControl::resume_args`] is correct
//!   (`-s <id>`, `--fork`) and simply never gets an id to use. `opencode -c` in
//!   the session's directory is the workaround until a payload field is known.
//! - **No title.** §9.3 says `session.updated` carries it, but names no field,
//!   so the event is not even registered: subscribing in order to discard the
//!   payload would spawn a subprocess per title change for nothing.
//! - **No tool name** — the Tool column stays empty on `PreToolUse`.
//! - **No tokens and no model.** §9.4 ships phase 1 without them deliberately.
//!   `HookMessage` now carries `context_tokens` / `model` (§9.4's option 2, and
//!   it is *built*), so the moment a usage field is named this is a two-line
//!   change with no transcript machinery at all. Reading `opencode.db` is
//!   explicitly **not** done here: the schema is unprobed, and cm-core linking
//!   SQLite already (for Codex titles) makes it tempting rather than justified.
//! - **No prompt, and so no rest→`Active` edge until the first tool call** —
//!   see [`EVENTS`], which is where that decision is argued. It is the largest
//!   behavioural gap and the top probe item.
//!
//! **The documented upgrade path is not the plugin.** §9.2: `opencode serve`
//! exposes an HTTP API with an SSE event stream and an official SDK, which
//! would give live, authoritative status, tokens, model and title with no
//! plugin at all — strictly better data. It is out of scope here because it
//! puts a long-lived HTTP connection and a port inside `launcher.rs`, a new
//! mechanism in the one file this whole design exists to keep small. Revisit if
//! the plugin proves unreliable; do not mistake the plugin for the optimum.
//!
//! What a probe against a real binary must settle, worst-breakage first:
//!
//! - **whether a handlers object keyed by §9.3's event names actually
//!   subscribes anything.** §9 states the module shape ("returns a handlers
//!   object") and lists the event names, but never states that the object's
//!   *keys* are those names. [`plugin_source`] bets that they are, on the
//!   strength of `tool.execute.before` / `.after` being handler-key-shaped in
//!   §9.3's own table. If opencode instead delivers everything through a single
//!   bus subscription, **no event ever fires and every row sits at
//!   `Starting`** — which is also what a wrong file name or an unloaded plugin
//!   looks like, so check this before believing any other symptom;
//! - **that a `plugins/` directory loads every module in it**, and that
//!   `captain-miao.js` (not `index.ts`, not a package) is a shape it accepts;
//!   and that the export is picked up — the file exports the same function both
//!   named and default, deliberately, because §9 does not say which convention
//!   applies and offering both costs nothing;
//! - **the turn-start signal.** §9.3 marks it `[PROBE]` itself. See [`EVENTS`];
//! - **whether `session.status` carries a busy/idle string.** If it does it is
//!   an *authoritative* status source and should drive the row directly, the way
//!   Claude's session file does, retiring the edge mapping below. §9.3 does not
//!   establish that it does, so the edges are what ships;
//! - **the payload field names**, in one go: session id, title, tool name,
//!   usage, model. Point `OPENCODE_CONFIG_DIR` at a scratch dir holding a
//!   hand-written plugin that appends `JSON.stringify(arguments)` to a file, run
//!   one turn, and every `None` above is answered at once;
//! - **whether `opencode session list` takes `--json`** (§9.4 marks the flag
//!   `[PROBE]`), which is the whole of `list_resumable`;
//! - **whether an interrupted turn still ends in `session.idle`.** §9.3 calls
//!   it "the turn-ended signal" without qualifying it. If Esc skips it, this
//!   backend inherits Grok's problem — a live row stranded at `Active` — and
//!   there is no transcript of ours to scan for a sentinel;
//! - **whether `$XDG_CONFIG_HOME` moves the real config dir.** §9 spells it
//!   `~/.config/opencode/`, the XDG default; [`config_dir`] honours the
//!   variable when it is set, which is a guess in the direction that fails
//!   *loudly* (a missing real dir yields a synthetic one holding only our
//!   plugin — the user's agents and commands visibly gone) rather than quietly.
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
use super::synth_home::SynthHome;
use crate::state::{HookEvent, HookMessage, LauncherState};

/// The executable this backend drives — see [`super::claude::BIN`].
pub(crate) const BIN: &str = "opencode";

/// The directory inside the synthetic config dir that holds plugin modules
/// (§9's facts table: `~/.config/opencode/plugins/`). Owned by us *and*
/// mirrored — see [`ensure_synth_config`].
const PLUGINS_DIR: &str = "plugins";

/// Our generated module inside `plugins/`. A whole file of our own rather than
/// a merged one, because `plugins/` is a directory of independent modules —
/// nothing of the user's is shadowed by it, and the nested mirror keeps
/// theirs loading beside ours.
///
/// `.js`, not `.ts`: opencode accepts either (§9: "JS/TS plugin modules") and
/// nothing we emit needs types, so the extension that cannot require a
/// transpile step is the one to pick.
const PLUGIN_FILE: &str = "captain-miao.js";

/// The opencode events we subscribe to, and the [`HookEvent`] each becomes.
/// Straight from §9.3, and the **only** vocabulary this backend has: no payload
/// field name in it is known, so an event's *name* is the entire signal.
///
/// Three rows of §9.3 are deliberately absent, each because subscribing would
/// cost more than the silence does:
///
/// - **`message.updated` → `PromptSubmit`**, §9.3's own `[PROBE]` row ("no
///   explicit 'prompt submitted' event; find the cleanest turn-start signal").
///   §9 qualifies it *"(user part)"* — i.e. the mapping is specified for user
///   messages only, and the field distinguishing those is one of the names we
///   do not have. §9.1 forbids the plugin from filtering anyway. Subscribing
///   **unfiltered** is therefore not the specified mapping but a superset of
///   it, and a dangerous one in three separate ways: it spawns one `miao hook`
///   process per message update — plausibly per streamed chunk, i.e. hundreds
///   per turn inside the agent's own process tree, which is a load captain-miao
///   must never put on a session; `dispatch_default`'s `PromptSubmit` arm
///   clears `last_tool`, so the Tool column would blank mid-tool; and one
///   update landing *after* `session.idle` re-`Active`s a settled row. The
///   honest cost of leaving it out is stated plainly: **a turn goes `Active` at
///   its first tool call rather than at submission, and a tool-free turn never
///   leaves rest at all.** This is the top probe item.
/// - **`session.updated`** — carries the title, under a field name §9 never
///   gives (module doc).
/// - **`session.status`** — if it carries a busy/idle string it is the
///   authoritative source that should replace this whole table (module doc),
///   but the string's field name is unknown, so subscribing today would
///   forward a payload nothing can read.
///
/// `experimental.session.compacting` is registered despite §9 flagging it
/// experimental: an event opencode does not emit costs one dead handler key,
/// while omitting one it does emit leaves a row stuck in `Compacting` until the
/// next event of any kind.
const EVENTS: &[(&str, HookEvent)] = &[
    ("session.created", HookEvent::SessionStart),
    ("tool.execute.before", HookEvent::PreToolUse),
    ("tool.execute.after", HookEvent::PostToolUse),
    ("permission.asked", HookEvent::PermissionRequest),
    ("permission.replied", HookEvent::ElicitationResult),
    ("session.idle", HookEvent::Stop),
    ("session.error", HookEvent::StopFailure),
    ("experimental.session.compacting", HookEvent::PreCompact),
    ("session.compacted", HookEvent::PostCompact),
];

// =============================================================================
// Filesystem locations
// =============================================================================

/// The real opencode config dir — what the synthetic one mirrors. It is *not*
/// what the launched agent is handed (see [`ensure_synth_config`]).
///
/// `$OPENCODE_CONFIG_DIR` first, because a user who already relocated it means
/// that dir and mirroring `~/.config/opencode` would silently ignore every
/// agent, command and mode they have. Then `$XDG_CONFIG_HOME/opencode`, then
/// `~/.config/opencode` (§9's spelling). The middle branch is the guess — see
/// the module doc's probe list — and it is the branch that fails visibly.
///
/// Note the two *sibling* overrides §9 lists are deliberately not touched:
/// `OPENCODE_CONFIG` names a config **file** and `OPENCODE_CONFIG_CONTENT` an
/// inline document, neither of which relocates `plugins/`, so neither affects
/// where our module has to land.
fn config_dir() -> Option<PathBuf> {
    for var in ["OPENCODE_CONFIG_DIR", "XDG_CONFIG_HOME"] {
        if let Some(v) = std::env::var_os(var) {
            let p = PathBuf::from(v);
            if !p.as_os_str().is_empty() {
                // `OPENCODE_CONFIG_DIR` *is* the config dir; `XDG_CONFIG_HOME`
                // is its parent.
                return Some(if var == "OPENCODE_CONFIG_DIR" {
                    p
                } else {
                    p.join(BIN)
                });
            }
        }
    }
    dirs::home_dir().map(|h| h.join(".config").join(BIN))
}

/// A single shared synthetic `$OPENCODE_CONFIG_DIR` for every opencode session:
/// the real config dir mirrored through symlinks, plus the `plugins/` subtree we
/// own. Shared rather than per-session because it is a symlink farm over the
/// user's config — one stable copy is cheaper to build and to reason about than
/// one per launch — and that sharing is exactly why the plugin may carry no
/// per-session data (see [`plugin_source`]).
fn synth_config_dir() -> PathBuf {
    crate::state::state_dir().join("opencode-config")
}

// =============================================================================
// Launcher: process spawn + synthetic OPENCODE_CONFIG_DIR
// =============================================================================

pub fn build_launch_command(
    cwd: &str,
    sock_path: &Path,
    settings_path: &Path,
    extra_args: &[String],
    shim_dir: Option<&Path>,
) -> Result<Command> {
    let opencode_bin = find_in_path(BIN).with_context(|| format!("{BIN} not found in PATH"))?;

    // The launcher already wrote our plugin source to `settings_path`;
    // relocate it into the synthetic config dir, which is the only place
    // opencode discovers global plugins (there is no per-invocation
    // `--settings` equivalent and no shell-command hook — that is the whole
    // reason this backend needs a config dir at all). Note the file the
    // launcher wrote is named `…-settings.json` and holds **JavaScript**: that
    // path is generic transport, opaque to the launcher, and every backend puts
    // its own format through it (Kimi's is TOML).
    let plugin_js = std::fs::read_to_string(settings_path).context("reading opencode plugin")?;
    let config = ensure_synth_config(&plugin_js)?;

    let has_envrc = Path::new(cwd).join(".envrc").is_file();
    let mut cmd = match has_envrc.then(|| find_in_path("direnv")).flatten() {
        Some(direnv) => {
            let mut c = Command::new(direnv);
            c.args(["exec", cwd]).arg(&opencode_bin);
            c
        }
        None => Command::new(&opencode_bin),
    };

    cmd.current_dir(cwd);
    cmd.env("OPENCODE_CONFIG_DIR", &config);
    super::with_shim_path(&mut cmd, shim_dir);
    // The hook subprocess reads the launcher socket from here rather than from
    // an argv flag: the synthetic config dir is shared by every session, so the
    // plugin cannot carry a per-session path. It reaches the forwarder by being
    // inherited twice — opencode inherits it from us, and the plugin's spawned
    // child inherits it from opencode. Whether opencode scrubs its plugins'
    // environment is unverified; if it does, every hook silently fails to find
    // its launcher (module doc).
    cmd.env("CAPTAIN_MIAO_SOCK", sock_path);
    cmd.args(launch_args(extra_args));
    Ok(cmd)
}

/// The agent-facing argv: **nothing for the working directory**, then whatever
/// the launcher forwarded (`-s <id>`, `--fork`).
///
/// This carried `--dir <path>` on the strength of the design note's facts table,
/// and that was wrong: the vendor documents the root command as
/// `opencode [project]` with no `--dir` among its flags — `--dir` belongs to the
/// `run` and `attach` *subcommands*. A flag the root command rejects fails at
/// startup, before any hook, so the window would have died with a flag error and
/// no row would ever have appeared.
///
/// So the cwd travels the way it does for Kimi, Grok and Pi: on the process
/// (`current_dir`), with nothing positional. The documented `[project]`
/// positional would also work and is the fallback if a probe ever shows opencode
/// ignoring its own working directory — but adding a positional we have not
/// watched a real binary accept is how this went wrong the first time, and
/// Reasonix is the standing reminder of the cost (its positional is a *prompt*,
/// so a bare `reasonix /work` opens a session whose first message is a path).
///
/// Pure and separately pinned because `current_dir` masks a mistake here in
/// every case except the one that matters.
fn launch_args(extra: &[String]) -> Vec<String> {
    extra.to_vec()
}

/// Create / refresh the synthetic config dir and return it. One owned entry,
/// and the nesting inside it is the whole subtlety (`agents::grok` solved the
/// same shape for `hooks/`):
///
/// - the **outer** mirror owns the `plugins` *directory*, so it is never
///   replaced by a symlink to the real one — which would send
///   [`SynthHome::write_owned`]'s write straight into the user's
///   `~/.config/opencode/plugins/`;
/// - the **inner** mirror rebuilds that directory's contents as symlinks to the
///   real `plugins/` entries, so the user's own plugins keep loading inside a
///   captain-miao session, and adds `captain-miao.js` beside them.
///
/// Everything else — `opencode.json`, `agent/`, `command/`, `mode/`, whatever
/// else the real dir holds — is symlinked by construction rather than by
/// enumeration, which is what makes this survive opencode growing a new config
/// entry without a change here.
///
/// A real config dir that does not exist yet yields a synthetic one holding
/// only our plugin. That is a working session with no agents, commands or modes
/// of the user's — visible immediately, and fixed by running `opencode` once
/// outside captain-miao.
fn ensure_synth_config(plugin_js: &str) -> Result<PathBuf> {
    let real = config_dir();
    let config = SynthHome {
        dir: synth_config_dir(),
        real: real.clone(),
        owned: &[PLUGINS_DIR],
        copied: &[],
    };
    config.ensure()?;

    let plugins = SynthHome {
        dir: config.dir.join(PLUGINS_DIR),
        real: real.map(|r| r.join(PLUGINS_DIR)),
        owned: &[PLUGIN_FILE],
        copied: &[],
    };
    plugins.ensure()?;
    plugins.write_owned(PLUGIN_FILE, plugin_js)?;
    Ok(config.dir)
}

// =============================================================================
// The generated plugin
// =============================================================================

/// Build the contents of `<state>/opencode-config/plugins/captain-miao.js`.
///
/// Named `build_hooks_settings` like every other backend's, because the seam
/// calls this "the per-session hook-settings file" and the contents are opaque
/// to it — this one is JavaScript.
///
/// Carries no `--sock`: the socket rides `$CAPTAIN_MIAO_SOCK`, because one
/// plugin serves every session.
pub fn build_hooks_settings(_sock_path: &str) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(BIN_FALLBACK));
    plugin_source(&exe.to_string_lossy())
}

/// What to spawn when our own path can't be resolved. Bare, so the plugin's
/// `spawn` resolves it on `PATH` the way `execvp` does — a wrong absolute path
/// would fail forever, while a bare name at least works for anyone who
/// installed `miao` normally.
const BIN_FALLBACK: &str = "miao";

/// The plugin module, as source. Pure and parameterized on the `miao` path so
/// the snapshot test can pin every other byte of it.
///
/// Three properties this file has to hold, none of which a JS test could check
/// here (this tree has no JS toolchain, and executing generated JavaScript is
/// not something a `cargo test` should start doing):
///
/// - **byte-identical across sessions.** `miao`'s path is per *machine*, not
///   per session; nothing session-shaped appears at all. That is what lets one
///   shared config dir serve every session, and it is asserted directly.
/// - **it cannot affect the agent.** stdout and stderr go to `ignore`, the exit
///   status is never read, `spawn` and `JSON.stringify` are each wrapped so a
///   throw cannot escape into opencode's handler, and both the child and its
///   stdin carry an `error` listener — an unhandled `EPIPE` on a pipe whose
///   reader died is otherwise a *process-level* crash in Node, which would take
///   the user's session down with it.
/// - **it needs no shell quoting.** The argv is an array, so no metacharacter
///   in the `miao` path can word-split or inject. The path is embedded as a
///   JSON string literal, which is also a valid JS one.
///
/// The export is emitted **twice**, named and default, because §9 states that a
/// plugin "export[s] a function" without saying under which convention. Both
/// point at the same function; the unused one costs a line.
fn plugin_source(miao: &str) -> String {
    let handlers: String = EVENTS
        .iter()
        .map(|(native, event)| {
            format!(
                "    {}: report({}),\n",
                js_string(native),
                js_string(event.as_kebab())
            )
        })
        .collect();

    format!(
        r#"// captain-miao session tracking for opencode. GENERATED — rewritten from
// captain-miao's `agents/opencode.rs` on every launch, so edits here are lost.
//
// It forwards opencode's lifecycle events to the captain-miao launcher that
// started this session and does nothing else: no filtering, no state, no
// retries, no reads of your data. The launcher socket arrives in
// $CAPTAIN_MIAO_SOCK — it is never written into this file, so the file is
// identical for every session. Delete it and sessions stop being tracked;
// nothing else changes.
import {{ spawn }} from "node:child_process";

const MIAO = {miao};

const CaptainMiao = (ctx) => {{
  const directory = ctx?.directory ?? null;
  const report =
    (event) =>
    (...args) => {{
      let body;
      try {{
        body = JSON.stringify({{ event, directory, payload: args }});
      }} catch {{
        body = JSON.stringify({{ event, directory }});
      }}
      let child;
      try {{
        child = spawn(MIAO, ["hook", "--agent", "opencode", event], {{
          stdio: ["pipe", "ignore", "ignore"],
        }});
      }} catch {{
        return;
      }}
      child.on("error", () => {{}});
      child.stdin.on("error", () => {{}});
      child.stdin.end(body);
    }};

  return {{
{handlers}  }};
}};

export {{ CaptainMiao }};
export default CaptainMiao;
"#,
        miao = js_string(miao),
    )
}

/// `s` as a JS string literal. JSON's string grammar is a subset of JS's, so a
/// `serde_json` string is a valid JS one — including the escaping of quotes,
/// backslashes and control characters, which is the whole reason to go through
/// it rather than to interpolate.
fn js_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

// =============================================================================
// Hook payload (stdin from the plugin → normalized HookMessage)
// =============================================================================

/// The envelope [`plugin_source`]'s `report` writes to `miao hook`'s stdin:
/// `{"event": "<our kebab name>", "directory": "…", "payload": [<handler args>]}`.
///
/// **This is our own contract, not opencode's** — which is the point. §9.3
/// names opencode's events but no field inside any of their payloads, so
/// nothing here may depend on one; the handler's arguments ride along whole
/// under `payload` so that the launcher's `raw` carries them verbatim and one
/// captured hook answers every open field name at once. The same shape Grok's
/// notification hook uses for the same reason: when there is no documented
/// payload, synthesize the one field you can defend and forward the rest
/// untouched.
///
/// `event` is not read back — the event rides our own argv, as it does for
/// every backend — but it is written into the body deliberately, so a captured
/// stdin is self-describing.
#[derive(Deserialize)]
struct HookPayload {
    /// The plugin context's `directory` (§9.1), the one field of the whole
    /// mechanism whose meaning §9 states. Reported as `cwd` for honesty and for
    /// a probe's benefit; nothing consumes it, since `dispatch_default` reads
    /// `msg.cwd` only on `CwdChanged` and opencode registers no such event.
    directory: Option<String>,
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse opencode hook JSON from stdin")?;
    Ok(HookMessage {
        event,
        // Every one of these is a field name §9 does not give (module doc). The
        // raw envelope below carries the agent's own payload, so none of this
        // is lost — only unread.
        session_id: None,
        tool_name: None,
        message: None,
        cwd: payload.directory,
        prompt: None,
        session_title: None,
        context_tokens: None,
        model: None,
        // No transcript path, and this is the field the launcher gates its
        // entire transcript watch on — so nothing reads an opencode transcript,
        // which is what makes the empty `read_transcript_stats` and
        // `scan_transcript_signals` consistent rather than merely
        // unimplemented. opencode's sessions live in `opencode.db` + `storage/`
        // besides, which is not a file the launcher's byte-offset fold could
        // follow even if it had the path.
        transcript_path: None,
        raw: Some(stdin.to_string()),
    })
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// opencode departs from [`common::dispatch_default`] nowhere.
///
/// That is a consequence of how little it reports, not a fit: the nine events
/// in [`EVENTS`] are nine of ours under different spellings, and the two places
/// another backend needed an arm — an interrupt arriving as something else
/// (Reasonix), a session-end `Stop` to tell from a turn-end one (Grok) — are
/// both distinctions that need a payload *field*, and we have none. The wrapper
/// stays so the seam keeps one callee per backend, and so the day a probe names
/// a field, it has a place to land.
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

    /// A stdin body in exactly the shape [`plugin_source`]'s `report` writes —
    /// **hand-written to match the template above, not captured from a running
    /// binary**. Its `payload` is deliberately opaque: no field in it is read,
    /// and that is the property under test.
    fn payload(event: &str, args: &str) -> String {
        format!(r#"{{"event":"{event}","directory":"/home/miao/p","payload":[{args}]}}"#)
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState {
            agent: AgentControl::OpenCode,
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

    /// Drive one hook end to end — parse the plugin's stdin JSON, then dispatch
    /// it — so the tests exercise the same path a live hook takes.
    fn feed(state: &mut LauncherState, event: HookEvent, stdin: &str) {
        let msg = parse_hook_payload(event, stdin).expect("payload parses");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(dispatch_hook(state, msg));
    }

    /// The turn as this backend can see it: `session.created` settles the row
    /// out of `Starting`, the first **tool call** is what makes it `Active`
    /// (there is no prompt event — see [`EVENTS`]), and `session.idle` ends it.
    #[test]
    fn a_turn_runs_from_the_first_tool_call_to_idle() {
        let mut state = state_at(SessionStatus::Starting);
        feed(
            &mut state,
            HookEvent::SessionStart,
            &payload("session-start", ""),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        // No session id rides any payload, so the launcher never learns one —
        // which is what makes an opencode row unresumable from the dashboard.
        assert_eq!(state.session_id, None);

        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload("pre-tool-use", r#"{"tool":"bash"}"#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        // The tool name is *in* the forwarded payload and deliberately not read
        // out of it: `"tool"` here is the fixture's invention, not a documented
        // field, and a wrong guess would fill the Tool column with fiction.
        assert_eq!(state.last_tool, None);

        feed(
            &mut state,
            HookEvent::PostToolUse,
            &payload("post-tool-use", ""),
        );
        assert_eq!(state.status, SessionStatus::Active);

        feed(&mut state, HookEvent::Stop, &payload("stop", ""));
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_tool, None);
    }

    /// Approval is reachable and needs no second mechanism (Grok reaches it
    /// only through a separate notification system), and the reply settles the
    /// row back to `Active` rather than leaving it parked.
    #[test]
    fn a_permission_gate_opens_and_closes() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &payload("permission-request", ""),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);

        feed(
            &mut state,
            HookEvent::ElicitationResult,
            &payload("elicitation-result", ""),
        );
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// Both compaction edges are registered, so a row leaves `Compacting` on
    /// its own — unlike Reasonix, which has no post-compaction signal at all.
    #[test]
    fn compaction_enters_and_leaves_on_its_own_events() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PreCompact,
            &payload("pre-compact", ""),
        );
        assert_eq!(state.status, SessionStatus::Compacting);
        feed(
            &mut state,
            HookEvent::PostCompact,
            &payload("post-compact", ""),
        );
        assert_eq!(state.status, SessionStatus::Compacted);
    }

    /// `session.error` ends the turn and puts *something* on the row. With no
    /// documented error field, `dispatch_default` falls back to the raw
    /// envelope — which is at least honest about what the agent actually sent,
    /// and is the payload a probe wants to see.
    #[test]
    fn a_session_error_settles_the_row_and_surfaces_the_raw_payload() {
        let mut state = state_at(SessionStatus::Active);
        let stdin = payload("stop-failure", r#"{"boom":true}"#);
        feed(&mut state, HookEvent::StopFailure, &stdin);
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_error.as_deref(), Some(stdin.as_str()));
    }

    /// The envelope is *ours*, so what it must guarantee is that nothing is
    /// read out of opencode's own payload by accident — a field that later
    /// turns out to mean something else is the failure this backend is built to
    /// avoid.
    #[test]
    fn nothing_is_read_out_of_the_agents_payload() {
        let stdin = payload(
            "pre-tool-use",
            r#"{"sessionID":"s1","title":"a title","tool":"bash","tokens":42,"model":"m"}"#,
        );
        let msg = parse_hook_payload(HookEvent::PreToolUse, &stdin).expect("parses");
        assert_eq!(msg.cwd.as_deref(), Some("/home/miao/p"));
        assert_eq!(msg.session_id, None, "no documented session-id field");
        assert_eq!(msg.session_title, None, "no documented title field");
        assert_eq!(msg.tool_name, None, "no documented tool-name field");
        assert_eq!(msg.context_tokens, None, "no documented usage field");
        assert_eq!(msg.model, None, "no documented model field");
        // No transcript path is derived, which is what keeps the launcher's
        // transcript machinery inert for opencode.
        assert_eq!(msg.transcript_path, None);
        // Everything the agent sent survives verbatim, so one captured hook
        // fills in every assertion above.
        assert_eq!(msg.raw.as_deref(), Some(stdin.as_str()));
    }

    /// One plugin serves every session, so it must carry no per-session data.
    #[test]
    fn the_plugin_never_embeds_the_per_session_socket() {
        let a = build_hooks_settings("/run/a.sock");
        let b = build_hooks_settings("/run/b.sock");
        assert_eq!(a, b, "the plugin must not embed the per-session socket");
        assert!(!a.contains(".sock"));
        assert!(!a.contains("--sock"));
        // The socket reaches the forwarder through the environment instead.
        assert!(a.contains("CAPTAIN_MIAO_SOCK"), "{a}");
    }

    /// Every handler key is an event name from §9.3 and every value is a
    /// [`HookEvent`] this build knows — the two halves of the mechanism live in
    /// different languages and nothing else pins them together. A kebab name
    /// that stopped round-tripping would make `miao hook` reject its own
    /// plugin's calls at runtime, with the row simply never moving.
    #[test]
    fn every_registered_event_round_trips_through_the_forwarder() {
        for (native, event) in EVENTS {
            assert_eq!(
                HookEvent::from_kebab(event.as_kebab()),
                Some(*event),
                "{native}"
            );
        }
        let names: Vec<&str> = EVENTS.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "session.created",
                "tool.execute.before",
                "tool.execute.after",
                "permission.asked",
                "permission.replied",
                "session.idle",
                "session.error",
                "experimental.session.compacting",
                "session.compacted",
            ],
            "the event names are the entire signal this backend has — no \
             payload field of opencode's is read, so a name changing here \
             changes what the dashboard can see"
        );
        // The three rows of §9.3 that are deliberately not subscribed. Named
        // rather than merely absent, so adding one is a deliberate act with the
        // reasoning in `EVENTS` in front of whoever does it.
        for skipped in ["message.updated", "session.updated", "session.status"] {
            assert!(!names.contains(&skipped), "{skipped}");
        }
    }

    /// **The snapshot.** captain-miao cannot execute this JavaScript — there is
    /// no JS toolchain in the tree — so the only defence against a careless
    /// edit is that every byte of it is pinned and any change has to be
    /// re-reviewed here deliberately.
    #[test]
    fn the_generated_plugin_is_byte_for_byte_what_we_think_it_is() {
        let expected = r#"// captain-miao session tracking for opencode. GENERATED — rewritten from
// captain-miao's `agents/opencode.rs` on every launch, so edits here are lost.
//
// It forwards opencode's lifecycle events to the captain-miao launcher that
// started this session and does nothing else: no filtering, no state, no
// retries, no reads of your data. The launcher socket arrives in
// $CAPTAIN_MIAO_SOCK — it is never written into this file, so the file is
// identical for every session. Delete it and sessions stop being tracked;
// nothing else changes.
import { spawn } from "node:child_process";

const MIAO = "/home/miao/.local/bin/miao";

const CaptainMiao = (ctx) => {
  const directory = ctx?.directory ?? null;
  const report =
    (event) =>
    (...args) => {
      let body;
      try {
        body = JSON.stringify({ event, directory, payload: args });
      } catch {
        body = JSON.stringify({ event, directory });
      }
      let child;
      try {
        child = spawn(MIAO, ["hook", "--agent", "opencode", event], {
          stdio: ["pipe", "ignore", "ignore"],
        });
      } catch {
        return;
      }
      child.on("error", () => {});
      child.stdin.on("error", () => {});
      child.stdin.end(body);
    };

  return {
    "session.created": report("session-start"),
    "tool.execute.before": report("pre-tool-use"),
    "tool.execute.after": report("post-tool-use"),
    "permission.asked": report("permission-request"),
    "permission.replied": report("elicitation-result"),
    "session.idle": report("stop"),
    "session.error": report("stop-failure"),
    "experimental.session.compacting": report("pre-compact"),
    "session.compacted": report("post-compact"),
  };
};

export { CaptainMiao };
export default CaptainMiao;
"#;
        assert_eq!(plugin_source("/home/miao/.local/bin/miao"), expected);
    }

    /// An install path with a quote or a backslash in it must not be able to
    /// break the module — the argv is an array so nothing can *inject*, but an
    /// unescaped literal would be a syntax error, and a plugin that fails to
    /// parse takes every event with it.
    #[test]
    fn an_awkward_install_path_stays_a_valid_string_literal() {
        let src = plugin_source(r#"/home/miao/we"ird\bin/miao"#);
        assert!(
            src.contains(r#"const MIAO = "/home/miao/we\"ird\\bin/miao";"#),
            "{src}"
        );
        // A newline (legal in a POSIX path, illegal raw in a JS string) is
        // escaped rather than ending the statement.
        let src = plugin_source("/home/miao/two\nlines/miao");
        assert!(
            src.contains(r#"const MIAO = "/home/miao/two\nlines/miao";"#),
            "{src}"
        );
    }

    /// **Nothing** is passed for the working directory — it rides `current_dir`.
    /// The root command is documented as `opencode [project]` with no `--dir`
    /// among its flags (that belongs to the `run` and `attach` subcommands), and
    /// a flag the root command rejects kills the window at startup before any
    /// hook fires. Pinned as an emptiness because `current_dir` would hide the
    /// mistake everywhere except the launch that matters.
    #[test]
    fn nothing_positional_carries_the_working_directory() {
        assert!(launch_args(&[]).is_empty());
        assert_eq!(
            launch_args(&["-s".to_string(), "s1".to_string()]),
            ["-s", "s1"]
        );
        // Whatever the launcher forwarded is passed through untouched, and no
        // path is ever spliced in beside it.
        assert!(!launch_args(&["-s".to_string(), "s1".to_string()]).contains(&"--dir".to_string()));
    }
}
