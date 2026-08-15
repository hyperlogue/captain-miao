//! opencode CLI backend. Owns every opencode-specific path, env var and hook
//! payload shape; the dashboard reaches all of it only via
//! `crate::agent::AgentControl::OpenCode`'s match arms.
//!
//! **Source-read against opencode itself** — `packages/plugin/src/index.ts`
//! for the hook interface and `packages/sdk/js/src/gen/types.gen.ts` for every
//! payload shape. It is not run against a binary yet; the probe list at the end
//! says what that would still settle.
//!
//! The first cut of this backend was derived from this project's design note
//! (§9) alone, which named opencode's *events* but no field inside any payload,
//! and guessed that a handlers object keyed by those event names would
//! subscribe them. **It does not.** `Hooks` is a closed interface, and six of
//! the nine keys that cut registered — `session.created`, `session.idle`,
//! `session.error`, `session.compacted`, `permission.replied`, and
//! `permission.asked` (whose hook is spelled `permission.ask`) — are bus event
//! names opencode never looks for. Only `tool.execute.before` / `.after` and
//! `experimental.session.compacting` happened to coincide with real hook keys,
//! so a row went `Active` at its first tool call and **never came back**: the
//! turn-end signal was one of the six that fired nothing.
//!
//! **The delivery is a generated JavaScript plugin, and that is the one
//! genuinely new mechanism in this backend.** opencode has no shell-command
//! hooks at all (§9's facts table: *"hooks — none; extensibility is JS/TS
//! plugin modules only"*). Its event surface is a module exporting
//! `(input: PluginInput, options?) => Promise<Hooks>`, where `PluginInput` is
//! `{client, project, directory, worktree, serverUrl, $, …}`. So
//! [`plugin_source`] emits that module,
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
//! - the plugin holds **no state, no retries and no reads of the agent's own
//!   data**. It serializes what it was handed and spawns
//!   `miao hook --agent opencode <event>`. What it does decide is the *event
//!   name*, which is our argv and therefore cannot be deferred to Rust — see
//!   [`BUS_EVENTS`] for the two places that decision needs a payload field, and
//!   why forwarding those two unfiltered would be a denial of service against
//!   the user's own session. Every other field is dug out in
//!   [`parse_hook_payload`], where it is testable, rather than in JavaScript
//!   this tree cannot execute;
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
//! ## Two mechanisms, because `Hooks` is two mechanisms
//!
//! - **Direct hooks** — `chat.message`, `tool.execute.before` / `.after`,
//!   `permission.ask`, `experimental.session.compacting`, … Each is a named key
//!   on `Hooks` taking `(input, output)`, and `output` is **mutable**: these are
//!   decision points opencode waits on, not notifications.
//! - **The bus** — one `event(input: {event: Event})` key receiving the whole
//!   typed union, where `event.type` names it and `event.properties` carries it.
//!   Observation only; opencode does not act on the return.
//!
//! We take the direct hooks only for the facts opencode states *nowhere else* —
//! the tool name, and the turn-start that `chat.message` is — and read the rest
//! off the bus, because an observer on the bus cannot delay a turn.
//! `permission.ask` is deliberately **not** used despite existing and being the
//! obvious fit: it can set `output.status`, so a plugin sitting in it is in the
//! path of the user's own approval prompt. The bus's `permission.updated`
//! carries the same `Permission` and cannot block.
//!
//! `session.status` is the one genuinely authoritative signal here — its
//! `{type: "idle" | "busy" | "retry"}` is opencode's own view of whether the
//! session is working — and it is still **not** what drives the row, for the
//! reason a capability gate usually exists: it is strictly *coarser* than our
//! vocabulary. A session waiting on an approval is `busy`, and a `busy` arriving
//! after `permission.updated` would knock the row out of `WaitingForApproval`
//! back to `Active`. Only its `idle` edge is forwarded (as `Stop`), where it
//! costs nothing and buys the one thing the finer events might miss — an
//! **interrupted** turn that never reaches `session.idle`. That asymmetry is the
//! whole of [`BUS_EVENTS`]'s special-casing besides `message.updated`.
//!
//! **The documented upgrade path is not the plugin.** §9.2: `opencode serve`
//! exposes an HTTP API with an SSE event stream and an official SDK, which
//! would give live, authoritative status, tokens, model and title with no
//! plugin at all — strictly better data. It is out of scope here because it
//! puts a long-lived HTTP connection and a port inside `launcher.rs`, a new
//! mechanism in the one file this whole design exists to keep small. Revisit if
//! the plugin proves unreliable; do not mistake the plugin for the optimum.
//!
//! What a probe against a real binary must still settle — the payload shapes
//! are no longer among them, since the SDK's generated types give every one:
//!
//! - **that a `plugins/` directory loads every module in it**, and that
//!   `captain-miao.js` (not `index.ts`, not a package) is a shape it accepts;
//!   and which export convention applies. The file exports the same function
//!   named *and* default; `PluginModule` in `packages/plugin/src/index.ts` also
//!   describes a `{id?, server}` object form, which we do **not** emit — if
//!   nothing fires and the module is demonstrably loaded, that is the next
//!   thing to try;
//! - **whether an interrupted turn still ends in `session.idle`.** If Esc skips
//!   it, the `session.status` → `idle` edge above is what saves the row from
//!   stranding at `Active` — which is why it is subscribed even though the
//!   finer events would normally cover it;
//! - **whether `message.updated` fires with `time.completed` set exactly once
//!   per assistant turn.** [`BUS_EVENTS`] gates on that field to keep one
//!   subprocess per turn rather than one per streamed chunk; if the completed
//!   message is re-emitted, the token column simply updates twice with the same
//!   number, but if it is *never* emitted the column stays empty;
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
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::common;
use super::synth_home::SynthHome;
use crate::agent::ResumeCandidate;
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

/// Bus event `type` → the [`HookEvent`] it becomes, for every event forwarded
/// by name alone. Types absent from this table are dropped in the plugin, which
/// is the difference between subscribing to opencode's bus and drowning in it:
/// `file.watcher.updated` and `message.part.updated` alone would spawn hundreds
/// of processes a turn.
///
/// Two types are handled *outside* this table because forwarding them by name
/// alone is wrong, and both are the reason the plugin reads one field:
///
/// - **`session.status`** — forwarded only on `properties.status.type ===
///   "idle"`, as `Stop`. `busy` is dropped rather than mapped, because it is
///   coarser than the state the finer events have already established (module
///   doc). This is the safety net for an interrupted turn.
/// - **`message.updated`** — forwarded only for an assistant message with
///   `time.completed` set, i.e. **once per turn** rather than once per streamed
///   chunk. It is not a status edge at all; it is how the token and model
///   columns arrive (see [`parse_hook_payload`]), which is why it maps to
///   `CwdChanged` — the one arm of `common::dispatch_default` that touches no
///   status, while `adopt_session_facts` still takes the tokens, the model and
///   the id off the payload.
///
/// `session.updated` maps to `CwdChanged` for the same reason: it is a title
/// change (and a `directory`), never a status edge, so it must reach
/// `adopt_session_facts` without disturbing a row mid-turn.
const BUS_EVENTS: &[(&str, HookEvent)] = &[
    ("session.created", HookEvent::SessionStart),
    ("session.updated", HookEvent::CwdChanged),
    ("permission.updated", HookEvent::PermissionRequest),
    ("permission.replied", HookEvent::ElicitationResult),
    ("session.idle", HookEvent::Stop),
    ("session.error", HookEvent::StopFailure),
    ("session.compacted", HookEvent::PostCompact),
];

/// The `Hooks` keys we implement directly, and the [`HookEvent`] each becomes.
/// These are the facts opencode states nowhere else:
///
/// - `chat.message` is the **turn start** — a real "the user submitted this"
///   signal, carrying `sessionID` and the `{providerID, modelID}` of the model
///   about to run. This is what makes a tool-free turn show as working, which
///   the bus alone cannot do.
/// - `tool.execute.before` / `.after` carry `tool`, the only source of the Tool
///   column.
/// - `experimental.session.compacting` is the only pre-compaction signal;
///   `session.compacted` on the bus is the matching end, so a row leaves
///   `Compacting` on its own.
///
/// Every one of these also carries `sessionID`, which is what `r` and `f`
/// resume from.
const DIRECT_HOOKS: &[(&str, HookEvent)] = &[
    ("chat.message", HookEvent::PromptSubmit),
    ("tool.execute.before", HookEvent::PreToolUse),
    ("tool.execute.after", HookEvent::PostToolUse),
    ("experimental.session.compacting", HookEvent::PreCompact),
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

    let mut cmd = common::agent_command(BIN, cwd, shim_dir)?;
    cmd.env("OPENCODE_CONFIG_DIR", &config);
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
        prune: false,
    };
    config.ensure()?;

    let plugins = SynthHome {
        dir: config.dir.join(PLUGINS_DIR),
        real: real.map(|r| r.join(PLUGINS_DIR)),
        owned: &[PLUGIN_FILE],
        copied: &[],
        // A loader-scanned collection: a plugin the user deletes must not
        // leave a dangling import behind for opencode's loader to trip on
        // (see [`SynthHome::prune`]).
        prune: true,
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
/// Four properties this file has to hold, none of which a JS test could check
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
/// - **ordering is offered, not assumed.** Every handler returns `send`'s
///   promise, which settles when the child exits. At 1.18.18 opencode awaits
///   the **direct hooks** in registration order (`plugin/index.ts`), so those
///   arrive serialized; the bus `event` hook's return is discarded (`void`),
///   so bus events race each other and the direct hooks freely. Returning the
///   promise costs nothing where it is dropped, keeps the direct hooks in
///   order today, and means any future opencode that does await the bus gets
///   ordering for free — but nothing downstream may *depend* on cross-event
///   order, which is why the child-session gate in [`dispatch_hook`] is
///   denylist-shaped rather than sequence-shaped.
///
/// The export is emitted **twice**, named and default, because §9 states that a
/// plugin "export[s] a function" without saying under which convention. Both
/// point at the same function; the unused one costs a line.
fn plugin_source(miao: &str) -> String {
    let bus: String = BUS_EVENTS
        .iter()
        .map(|(native, event)| {
            format!(
                "  {}: {},\n",
                js_string(native),
                js_string(event.as_kebab())
            )
        })
        .collect();
    let handlers: String = DIRECT_HOOKS
        .iter()
        .map(|(key, event)| {
            format!(
                "    {}: report({}),\n",
                js_string(key),
                js_string(event.as_kebab())
            )
        })
        .collect();

    format!(
        r#"// captain-miao session tracking for opencode. GENERATED — rewritten from
// captain-miao's `agents/opencode.rs` on every launch, so edits here are lost.
//
// It forwards opencode's lifecycle events to the captain-miao launcher that
// started this session and does nothing else: no state, no retries, no reads of
// your data. Payloads are forwarded whole and picked apart on the other side;
// the only fields read here are the two that decide whether to forward at all.
// The launcher socket arrives in $CAPTAIN_MIAO_SOCK — it is never written into
// this file, so the file is identical for every session. Delete it and sessions
// stop being tracked; nothing else changes.
import {{ spawn }} from "node:child_process";

const MIAO = {miao};

// opencode bus event type -> captain-miao event name. Anything not listed is
// dropped: the bus carries per-chunk and per-file events that would otherwise
// spawn hundreds of processes inside your session.
const BUS = {{
{bus}}};

const CaptainMiao = async (ctx) => {{
  const directory = ctx?.directory ?? null;
  // The returned promise settles when the child exits, so an opencode that
  // awaits its hooks delivers these events in the order it fired them; one
  // that does not await loses nothing. It never rejects.
  const send = (event, args) => {{
    let body;
    try {{
      body = JSON.stringify({{ event, directory, payload: args }});
    }} catch {{
      body = JSON.stringify({{ event, directory }});
    }}
    return new Promise((resolve) => {{
      let child;
      try {{
        child = spawn(MIAO, ["hook", "--agent", "opencode", event], {{
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
  }};
  const report =
    (event) =>
    (...args) =>
      send(event, args);

  return {{
    event: async (input) => {{
      const e = input?.event;
      const type = e?.type;
      if (!type) return;
      // Authoritative but coarser than the dashboard's states, so only the
      // settle edge is taken: a session waiting on an approval is "busy" too.
      if (type === "session.status") {{
        if (e.properties?.status?.type === "idle") return send("stop", [input]);
        return;
      }}
      // Once per assistant turn, not once per streamed chunk. This is the
      // token and model column, not a status edge.
      if (type === "message.updated") {{
        const info = e.properties?.info;
        if (info?.role !== "assistant" || !info?.time?.completed) return;
        return send("cwd-changed", [input]);
      }}
      const name = BUS[type];
      if (name) return send(name, [input]);
    }},
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
    /// The plugin context's `directory` — the session's own working directory,
    /// and the fallback `cwd` for a payload that names none itself.
    directory: Option<String>,
    /// The handler's arguments, verbatim. `[{event}]` for a bus event,
    /// `[input, output]` for a direct hook.
    #[serde(default)]
    payload: Vec<Value>,
}

/// Follow `path` through nested objects. Every lookup below is one of these, so
/// a payload that is a different shape than expected yields `None` rather than
/// an error — the plugin forwards several event shapes under one of our names,
/// and a miss on one shape is how the next is reached.
fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |acc, key| acc.get(key))
}

fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    at(v, path)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    at(v, path).and_then(Value::as_u64)
}

/// `providerID/modelID`, the spelling opencode itself uses for a model
/// reference — kept whole rather than reduced to `modelID`, because the same
/// model id is served by several providers and the pair is what a `/model`
/// switch actually changes.
fn model_ref(v: &Value, path: &[&str]) -> Option<String> {
    let scope = at(v, path)?;
    let model = str_at(scope, &["modelID"])?;
    match str_at(scope, &["providerID"]) {
        Some(provider) => Some(format!("{provider}/{model}")),
        None => Some(model),
    }
}

/// Pull everything the launcher can use out of one handler's arguments.
///
/// The field names all come from opencode's generated SDK types
/// (`packages/sdk/js/src/gen/types.gen.ts`): `Session` for `session.*`,
/// `AssistantMessage` / `UserMessage` for `message.updated`, `Permission` for
/// `permission.updated`, and `Hooks` in `packages/plugin/src/index.ts` for the
/// direct hooks' `input`. Several event shapes arrive under one of our event
/// names, so each fact is a short ordered list of the places it can be, not a
/// single path — the ordering is what keeps a `Message.id` from being read as a
/// session id.
fn parse_hook_payload_inner(event: HookEvent, payload: &HookPayload) -> HookMessage {
    let first = payload.payload.first().cloned().unwrap_or(Value::Null);
    let props = at(&first, &["event", "properties"])
        .cloned()
        .unwrap_or(Value::Null);
    let info = at(&props, &["info"]).cloned().unwrap_or(Value::Null);
    // A `Session` has a `directory`; a `Message` does not. That one field is
    // what tells the two `info` shapes apart, and every lookup keyed on
    // "is this a Session" below uses it.
    let info_is_session = at(&info, &["directory"]).and_then(Value::as_str).is_some();
    let assistant = at(&info, &["role"]).and_then(Value::as_str) == Some("assistant");

    let session_id = str_at(&info, &["sessionID"])
        .or_else(|| info_is_session.then(|| str_at(&info, &["id"])).flatten())
        .or_else(|| str_at(&props, &["sessionID"]))
        .or_else(|| str_at(&first, &["sessionID"]));

    // Only a `Session` proves lineage: `parentID` present means this event is a
    // subagent child session's (the task tool sets it — `tool/task.ts`), absent
    // on a full `Session.Info` proves the root. A bare `sessionID` proves
    // nothing (`None`). `Assistant.parentID` is a *message* id, which is why
    // this is gated on `info_is_session` rather than on the field existing.
    let session_is_child = info_is_session.then(|| str_at(&info, &["parentID"]).is_some());

    // Only a `Session` has a session title. `UserMessage.summary.title`
    // summarises one message and is deliberately not taken for it.
    let session_title = info_is_session.then(|| str_at(&info, &["title"])).flatten();

    // Input-side only, matching `agents::claude`'s fold: this column is "how
    // full is the context window", and completion tokens are not in the next
    // request. Cache reads and writes are, so both are counted. Keyed on
    // `tokens.input` being present at all: an assistant message in a shape
    // this doesn't recognize must report *nothing*, never a fabricated
    // `Some(0)` that `adopt_session_facts`'s last-write-wins would stamp over
    // a real number.
    let context_tokens = assistant
        .then(|| {
            u64_at(&info, &["tokens", "input"]).map(|input| {
                input
                    + u64_at(&info, &["tokens", "cache", "read"]).unwrap_or(0)
                    + u64_at(&info, &["tokens", "cache", "write"]).unwrap_or(0)
            })
        })
        .flatten();

    let model = model_ref(&first, &["model"]).or_else(|| model_ref(&info, &[]));

    HookMessage {
        event,
        session_id,
        tool_name: str_at(&first, &["tool"]),
        // `session.error`'s error is a tagged union whose payloads all nest a
        // `data.message`; anything else falls through to the raw envelope in
        // `dispatch_default`'s `StopFailure` arm.
        message: str_at(&props, &["error", "data", "message"]),
        cwd: str_at(&info, &["directory"])
            .or_else(|| str_at(&info, &["path", "cwd"]))
            .or_else(|| payload.directory.clone()),
        // `chat.message`'s `output.parts` — the text the user just submitted.
        prompt: payload.payload.get(1).and_then(first_text_part),
        session_title,
        context_tokens,
        model,
        // No transcript path, and this is the field the launcher gates its
        // entire transcript watch on — so nothing reads an opencode transcript.
        // Nothing needs to: tokens, model and title all arrive here by push,
        // which is strictly better than a fold (`common::adopt_session_facts`
        // is explicit that a backend picks one source, not both).
        transcript_path: None,
        raw: None,
        session_is_child,
    }
}

/// The first text part of a `chat.message` `output`, which is the prompt as the
/// user typed it. Parts are a tagged union (`text`, `reasoning`, `file`, …) and
/// only `text` has a body worth showing on a row.
fn first_text_part(output: &Value) -> Option<String> {
    at(output, &["parts"])?
        .as_array()?
        .iter()
        .find(|p| at(p, &["type"]).and_then(Value::as_str) == Some("text"))
        .and_then(|p| str_at(p, &["text"]))
}

pub fn parse_hook_payload(event: HookEvent, stdin: &str) -> Result<HookMessage> {
    let payload: HookPayload =
        serde_json::from_str(stdin).context("Failed to parse opencode hook JSON from stdin")?;
    Ok(HookMessage {
        raw: Some(stdin.to_string()),
        ..parse_hook_payload_inner(event, &payload)
    })
}

// =============================================================================
// Resume picker
// =============================================================================

/// `opencode session list --format json -n <limit>`, which is the one place
/// opencode publishes a *stable* shape for this rather than an internal one.
///
/// The alternative — reading `~/.local/share/opencode/storage/session/…` — is
/// faster and needs no subprocess, and is still not what this does: those files
/// are opencode's own schema, versioned and migrated (`storage.ts` carries the
/// migrations), so a reader of them is a reader of an internal format that has
/// already changed shape at least twice. `formatSessionJSON` exists to be
/// consumed and projects exactly six fields, four of which are the four this
/// needs. One subprocess on picker-open is the cheaper side of that trade, and
/// it runs host-side on a remote host like every other read.
///
/// `--format json` never paginates (opencode pages only the table form, and
/// only on a tty), so this cannot hang waiting on a pager.
pub fn list_resumable(limit: usize) -> Result<Vec<ResumeCandidate>> {
    let exe = super::find_in_path(BIN).with_context(|| format!("{BIN} not found in PATH"))?;
    let out = std::process::Command::new(exe)
        .args(["session", "list", "--format", "json", "-n"])
        .arg(limit.to_string())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("running `opencode session list`")?;
    if !out.status.success() {
        anyhow::bail!("`opencode session list` failed: {}", out.status);
    }
    Ok(parse_session_list(&String::from_utf8_lossy(&out.stdout)))
}

/// One entry of `formatSessionJSON`'s array. `updated` and `created` are
/// epoch milliseconds; `directory` is the session's working directory.
#[derive(Deserialize)]
struct ListedSession {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    updated: u64,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    directory: String,
}

/// Split from the subprocess so the shape is testable without opencode
/// installed. An empty session list prints **nothing at all** rather than `[]`
/// (`if (sessions.length === 0) return`), so an empty body is a valid, empty
/// answer and not a parse failure.
fn parse_session_list(stdout: &str) -> Vec<ResumeCandidate> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let listed: Vec<ListedSession> = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    listed
        .into_iter()
        .filter(|s| !s.directory.trim().is_empty())
        .map(|s| ResumeCandidate {
            agent: crate::agent::AgentControl::OpenCode,
            session_id: s.id,
            cwd: s.directory,
            // opencode titles every session itself, so there is no "untitled,
            // show me the first prompt" case for `first_prompt` to cover — and
            // no branch on the row either, which is opencode's to record and it
            // does not.
            first_prompt: None,
            custom_title: Some(s.title).filter(|t| !t.trim().is_empty()),
            git_branch: None,
            mtime: std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(if s.updated > 0 {
                    s.updated
                } else {
                    s.created
                }),
        })
        .collect()
}

// =============================================================================
// Hook event → status mapping
// =============================================================================

/// [`common::dispatch_default`] behind one opencode-specific gate: **subagent
/// child sessions**. The bus is process-wide — a child created by the task tool
/// runs the identical lifecycle as the root through the same plugin — so
/// without this gate a child's `session.idle` settles the row mid-turn, its
/// `message.updated` stamps the child's tokens and model over the row's, and
/// its id gets adopted as what `r` and `f` resume.
///
/// The gate is a **denylist of proven children**, not an allowlist of the root,
/// because one opencode process holds many root sessions (`<leader>n` /
/// `<leader>l`) and switching to one emits no `session.created` — an allowlist
/// would go deaf on the first switch, while the denylist follows whatever root
/// is active. Children are learned from the two payloads that carry lineage at
/// all ([`HookMessage::session_is_child`]: `session.created` and
/// `session.updated`, both of which a child emits on every prompt), so a child
/// resumed from an earlier process re-teaches itself within one event.
///
/// Two deliberate asymmetries:
///
/// - **A child's approval request still blocks the row.** opencode stops the
///   whole turn on a permission ask whichever session raised it, so the one
///   pair of events a child can address to the *user* — `permission.asked` /
///   `permission.replied` — passes the gate.
/// - **A bare session id never displaces a known one.** Only a payload that
///   proves rootness re-points `state.session_id`; a bare id is adopted only
///   when none is known yet (first events of a fresh launch). This closes the
///   window where a resumed child's first event arrives before the lineage
///   that would condemn it — and the root's own `session.updated` re-proves
///   the root id on every prompt regardless.
pub async fn dispatch_hook(state: &mut LauncherState, mut msg: HookMessage) {
    let blocks_on_user = matches!(
        msg.event,
        HookEvent::PermissionRequest | HookEvent::ElicitationResult
    );
    if msg.session_is_child == Some(true) {
        if let Some(id) = msg.session_id.take()
            && !state.child_session_ids.contains(&id)
        {
            state.child_session_ids.push(id);
        }
        return;
    }
    if let Some(id) = &msg.session_id
        && state.child_session_ids.contains(id)
        && !blocks_on_user
    {
        return;
    }
    if msg.session_is_child != Some(false) && state.session_id.is_some() {
        msg.session_id = None;
    }
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

    /// A stdin body in exactly the shape [`plugin_source`]'s `send` writes —
    /// **hand-written to match the template above, not captured from a running
    /// binary**, but with `args` in the shapes opencode's generated SDK types
    /// declare, which is what makes the field lookups testable at all.
    fn payload(event: &str, args: &str) -> String {
        format!(r#"{{"event":"{event}","directory":"/home/miao/p","payload":[{args}]}}"#)
    }

    /// One bus event, wrapped the way the `event` hook receives it:
    /// `{event: {type, properties}}`.
    fn bus(name: &str, ty: &str, properties: &str) -> String {
        payload(
            name,
            &format!(r#"{{"event":{{"type":"{ty}","properties":{properties}}}}}"#),
        )
    }

    fn state_at(status: SessionStatus) -> LauncherState {
        LauncherState::for_test(AgentControl::OpenCode, status)
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

    /// A whole turn, in the order a live session produces it: `session.created`
    /// settles the row out of `Starting` and names it, `chat.message` starts the
    /// turn (the thing the first cut of this backend could not see at all), a
    /// tool runs, and `session.idle` ends it.
    #[test]
    fn a_turn_runs_from_the_prompt_to_idle() {
        let mut state = state_at(SessionStatus::Starting);
        feed(
            &mut state,
            HookEvent::SessionStart,
            &bus(
                "session-start",
                "session.created",
                r#"{"info":{"id":"ses_1","title":"wire up the parser","directory":"/home/miao/p"}}"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        // The id is what `r` and `f` resume from, and it rides the very first
        // event of the session.
        assert_eq!(state.session_id.as_deref(), Some("ses_1"));
        assert_eq!(state.name.as_deref(), Some("wire up the parser"));

        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload(
                "prompt-submit",
                r#"{"sessionID":"ses_1","model":{"providerID":"anthropic","modelID":"some-model-1"}},
                   {"parts":[{"type":"text","text":"add a test"}]}"#,
            ),
        );
        // A turn is working from the prompt, not from its first tool call — the
        // whole point of `chat.message`, and what makes a tool-free turn visible.
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.model.as_deref(), Some("anthropic/some-model-1"));
        assert_eq!(state.last_prompt.as_deref(), Some("add a test"));

        feed(
            &mut state,
            HookEvent::PreToolUse,
            &payload("pre-tool-use", r#"{"tool":"bash","sessionID":"ses_1"}"#),
        );
        assert_eq!(state.status, SessionStatus::Active);
        assert_eq!(state.last_tool.as_deref(), Some("bash"));

        feed(
            &mut state,
            HookEvent::PostToolUse,
            &payload("post-tool-use", r#"{"tool":"bash","sessionID":"ses_1"}"#),
        );
        assert_eq!(state.status, SessionStatus::Active);

        feed(
            &mut state,
            HookEvent::Stop,
            &bus("stop", "session.idle", r#"{"sessionID":"ses_1"}"#),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_tool, None);
    }

    /// The token and model columns, which arrive on a *completed* assistant
    /// message and nowhere else. `CwdChanged` is the carrier because this is not
    /// a status edge: a settled row must stay settled.
    #[test]
    fn a_completed_assistant_message_fills_the_token_and_model_columns() {
        let mut state = state_at(SessionStatus::Idle);
        feed(
            &mut state,
            HookEvent::CwdChanged,
            &bus(
                "cwd-changed",
                "message.updated",
                r#"{"info":{"id":"msg_1","sessionID":"ses_1","role":"assistant",
                    "providerID":"anthropic","modelID":"some-model-1",
                    "path":{"cwd":"/home/miao/p","root":"/home/miao/p"},
                    "time":{"created":1,"completed":2},
                    "tokens":{"input":100,"output":900,"reasoning":10,
                              "cache":{"read":50,"write":25}}}}"#,
            ),
        );
        // Input side only — 100 + 50 + 25. The 900 output tokens are not in the
        // next request, so they are not in the context gauge; this is the same
        // fold `agents::claude` does.
        assert_eq!(state.context_tokens, Some(175));
        assert_eq!(state.model.as_deref(), Some("anthropic/some-model-1"));
        assert_eq!(state.session_id.as_deref(), Some("ses_1"));
        assert_eq!(state.status, SessionStatus::Idle, "not a status edge");
        // `Message.id` is a *message* id and must never be read as the session's
        // — the ordering in `parse_hook_payload_inner` is what prevents it.
        assert_ne!(state.session_id.as_deref(), Some("msg_1"));
    }

    /// A rename mid-session lands on the row without disturbing a running turn.
    #[test]
    fn a_session_update_renames_without_touching_the_status() {
        let mut state = state_at(SessionStatus::Active);
        state.name = Some("old".to_string());
        feed(
            &mut state,
            HookEvent::CwdChanged,
            &bus(
                "cwd-changed",
                "session.updated",
                r#"{"info":{"id":"ses_1","title":"renamed","directory":"/home/miao/q"}}"#,
            ),
        );
        assert_eq!(state.name.as_deref(), Some("renamed"));
        assert_eq!(state.cwd, "/home/miao/q");
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// Approval is reachable and needs no second mechanism (Grok reaches it
    /// only through a separate notification system), and the reply settles the
    /// row back to `Active` rather than leaving it parked. Both edges come off
    /// the **bus**, never from the `permission.ask` hook, which opencode waits
    /// on — see the module doc.
    #[test]
    fn a_permission_gate_opens_and_closes() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::PermissionRequest,
            // `EventPermissionUpdated.properties` *is* the `Permission`, spread
            // rather than nested under `info` — hence the `props.sessionID` step.
            &bus(
                "permission-request",
                "permission.updated",
                r#"{"id":"per_1","type":"bash","sessionID":"ses_1",
                    "messageID":"msg_1","title":"run tests","metadata":{},
                    "time":{"created":1}}"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
        assert_eq!(state.session_id.as_deref(), Some("ses_1"));

        feed(
            &mut state,
            HookEvent::ElicitationResult,
            &bus(
                "elicitation-result",
                "permission.replied",
                r#"{"sessionID":"ses_1","requestID":"per_1","reply":"once"}"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// The child-session gate in [`dispatch_hook`]: a subagent runs the
    /// identical lifecycle through the same process-wide bus, and none of it is
    /// the row's business — not the settle, not the tokens, not the title, not
    /// the id `r` and `f` resume from.
    #[test]
    fn a_child_sessions_events_never_touch_the_row() {
        let mut state = state_at(SessionStatus::Active);
        state.session_id = Some("ses_root".to_string());
        state.context_tokens = Some(175);
        state.model = Some("anthropic/some-model-1".to_string());
        state.name = Some("the real work".to_string());

        // The task tool creates the child: lineage arrives, the id is learned,
        // and the event itself is dropped.
        feed(
            &mut state,
            HookEvent::SessionStart,
            &bus(
                "session-start",
                "session.created",
                r#"{"info":{"id":"ses_child","parentID":"ses_root",
                    "directory":"/home/miao/p","title":"probe (@scout subagent)"}}"#,
            ),
        );
        assert!(state.child_session_ids.contains(&"ses_child".to_string()));

        // The child's whole turn, by bare session id: prompt, completed
        // assistant message, settle. Every one is ignored.
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload("prompt-submit", r#"{"sessionID":"ses_child"}"#),
        );
        feed(
            &mut state,
            HookEvent::CwdChanged,
            &bus(
                "cwd-changed",
                "message.updated",
                r#"{"info":{"id":"msg_c","sessionID":"ses_child","role":"assistant",
                    "providerID":"anthropic","modelID":"cheap-model",
                    "time":{"created":1,"completed":2},
                    "tokens":{"input":9000,"cache":{"read":0,"write":0}}}}"#,
            ),
        );
        feed(
            &mut state,
            HookEvent::Stop,
            &bus("stop", "session.idle", r#"{"sessionID":"ses_child"}"#),
        );

        assert_eq!(state.status, SessionStatus::Active, "settled mid-turn");
        assert_eq!(state.session_id.as_deref(), Some("ses_root"));
        assert_eq!(state.context_tokens, Some(175));
        assert_eq!(state.model.as_deref(), Some("anthropic/some-model-1"));
        assert_eq!(state.name.as_deref(), Some("the real work"));
    }

    /// The one thing a child *can* address to the user: opencode stops the
    /// whole turn on a permission ask whichever session raised it, so a child's
    /// ask and its reply both pass the gate.
    #[test]
    fn a_childs_approval_request_still_blocks_the_row() {
        let mut state = state_at(SessionStatus::Active);
        state.session_id = Some("ses_root".to_string());
        state.child_session_ids = vec!["ses_child".to_string()];

        feed(
            &mut state,
            HookEvent::PermissionRequest,
            &bus(
                "permission-request",
                "permission.asked",
                r#"{"id":"per_9","sessionID":"ses_child","permission":"bash",
                    "patterns":["rm *"],"metadata":{},"always":false}"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::WaitingForApproval);
        assert_eq!(state.session_id.as_deref(), Some("ses_root"));

        feed(
            &mut state,
            HookEvent::ElicitationResult,
            &bus(
                "elicitation-result",
                "permission.replied",
                r#"{"sessionID":"ses_child","requestID":"per_9","reply":"once"}"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Active);
    }

    /// A bare session id never displaces a known one — only a payload proving
    /// rootness re-points the row. This closes the window where a child
    /// resumed from an earlier process (whose lineage this launcher never saw)
    /// speaks before the `session.updated` that would condemn it; and an
    /// in-process switch to another root still lands, because the root's own
    /// `session.updated` re-proves its id on every prompt.
    #[test]
    fn only_a_rootness_proving_payload_repoints_the_session_id() {
        let mut state = state_at(SessionStatus::Idle);
        // Fresh launch: the first bare id is adopted — nothing is known yet.
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload("prompt-submit", r#"{"sessionID":"ses_root"}"#),
        );
        assert_eq!(state.session_id.as_deref(), Some("ses_root"));

        // A resumed child's first event, by bare id: dispatched (the parent
        // *is* mid-turn) but not adopted.
        feed(
            &mut state,
            HookEvent::PromptSubmit,
            &payload("prompt-submit", r#"{"sessionID":"ses_stale_child"}"#),
        );
        assert_eq!(state.session_id.as_deref(), Some("ses_root"));

        // Switching to another root session: full info, no parent — adopted.
        feed(
            &mut state,
            HookEvent::CwdChanged,
            &bus(
                "cwd-changed",
                "session.updated",
                r#"{"info":{"id":"ses_root2","title":"other work",
                    "directory":"/home/miao/q"}}"#,
            ),
        );
        assert_eq!(state.session_id.as_deref(), Some("ses_root2"));
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

    /// `session.error` ends the turn and puts the agent's own message on the
    /// row. Every arm of opencode's error union (`ProviderAuthError`,
    /// `UnknownError`, `MessageOutputLengthError`, …) nests its text at
    /// `data.message`, so one path serves them all.
    #[test]
    fn a_session_error_settles_the_row_with_the_agents_own_message() {
        let mut state = state_at(SessionStatus::Active);
        feed(
            &mut state,
            HookEvent::StopFailure,
            &bus(
                "stop-failure",
                "session.error",
                r#"{"sessionID":"ses_1","error":{"name":"ProviderAuthError",
                    "data":{"providerID":"anthropic","message":"missing api key"}}}"#,
            ),
        );
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_error.as_deref(), Some("missing api key"));
    }

    /// An error shape we do not recognise still settles the row, and falls back
    /// to the raw envelope rather than to nothing — which is also the payload a
    /// probe wants to see.
    #[test]
    fn an_unrecognised_error_falls_back_to_the_raw_envelope() {
        let mut state = state_at(SessionStatus::Active);
        let stdin = bus("stop-failure", "session.error", r#"{"boom":true}"#);
        feed(&mut state, HookEvent::StopFailure, &stdin);
        assert_eq!(state.status, SessionStatus::Idle);
        assert_eq!(state.last_error.as_deref(), Some(stdin.as_str()));
    }

    /// A payload in a shape none of the lookups match must yield an empty
    /// `HookMessage` rather than a wrong one — the ordered fallbacks in
    /// `parse_hook_payload_inner` all end in `None`, and this is what stops a
    /// future opencode event shape from filling a column with fiction.
    #[test]
    fn an_unfamiliar_payload_reads_as_nothing_rather_than_as_something() {
        let stdin = payload("stop", r#"{"unexpected":{"id":"x","title":"y"}}"#);
        let msg = parse_hook_payload(HookEvent::Stop, &stdin).expect("parses");
        assert_eq!(msg.session_id, None);
        assert_eq!(msg.session_title, None);
        assert_eq!(msg.tool_name, None);
        assert_eq!(msg.context_tokens, None);
        assert_eq!(msg.model, None);
        // The plugin context's directory is the one fact every payload carries.
        assert_eq!(msg.cwd.as_deref(), Some("/home/miao/p"));
        // No transcript path is derived, which is what keeps the launcher's
        // transcript machinery inert for opencode.
        assert_eq!(msg.transcript_path, None);
        // Everything the agent sent survives verbatim regardless.
        assert_eq!(msg.raw.as_deref(), Some(stdin.as_str()));
    }

    /// A `UserMessage` carries `summary.title`, which summarises *that message*
    /// and is not the session's name. Taking it would rename the row on every
    /// turn.
    #[test]
    fn a_message_summary_is_never_read_as_the_session_title() {
        let stdin = bus(
            "cwd-changed",
            "message.updated",
            r#"{"info":{"id":"msg_1","sessionID":"ses_1","role":"user",
                "summary":{"title":"a summary of one message"},
                "time":{"created":1}}}"#,
        );
        let msg = parse_hook_payload(HookEvent::CwdChanged, &stdin).expect("parses");
        assert_eq!(msg.session_title, None);
        assert_eq!(msg.session_id.as_deref(), Some("ses_1"));
        // Not an assistant message, so no token total is invented for it.
        assert_eq!(msg.context_tokens, None);
    }

    /// An assistant message whose `tokens` is missing or reshaped reports no
    /// token count at all — never `Some(0)`, which `adopt_session_facts`'s
    /// last-write-wins would stamp over a real number on the row.
    #[test]
    fn an_assistant_message_without_tokens_reports_none_not_zero() {
        let stdin = bus(
            "cwd-changed",
            "message.updated",
            r#"{"info":{"id":"msg_1","sessionID":"ses_1","role":"assistant",
                "time":{"created":1,"completed":2}}}"#,
        );
        let msg = parse_hook_payload(HookEvent::CwdChanged, &stdin).expect("parses");
        assert_eq!(msg.context_tokens, None);
        assert_eq!(msg.session_id.as_deref(), Some("ses_1"));
    }

    /// The picker's rows, in the shape `formatSessionJSON` prints them.
    #[test]
    fn the_session_list_becomes_resume_candidates() {
        let out = parse_session_list(
            r#"[
              {"id":"ses_1","title":"wire up the parser","updated":1700000002000,
               "created":1700000001000,"projectId":"prj_1","directory":"/home/miao/p"},
              {"id":"ses_2","title":"","updated":0,
               "created":1700000000000,"projectId":"prj_1","directory":"/home/miao/q"}
            ]"#,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].session_id, "ses_1");
        assert_eq!(out[0].cwd, "/home/miao/p");
        assert_eq!(out[0].custom_title.as_deref(), Some("wire up the parser"));
        assert_eq!(
            out[0].mtime,
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_002_000)
        );
        // An untitled session is untitled, not titled the empty string — the
        // dashboard falls back to its own label for that.
        assert_eq!(out[1].custom_title, None);
        // `updated` is 0 on a session that has never been written since it was
        // created, and a row dated 1970 sorts to the bottom of a picker that
        // orders by recency.
        assert_eq!(
            out[1].mtime,
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000)
        );
    }

    /// opencode prints **nothing** rather than `[]` when there are no sessions,
    /// so an empty body has to read as an empty list and not as a broken CLI.
    #[test]
    fn no_sessions_is_an_empty_list_not_an_error() {
        assert!(parse_session_list("").is_empty());
        assert!(parse_session_list("   \n").is_empty());
        // And a shape we do not recognise yields nothing rather than a panic or
        // a half-filled row.
        assert!(parse_session_list("not json at all").is_empty());
        // A session with no directory cannot be resumed into anywhere, so it is
        // dropped rather than offered with an empty cwd.
        assert!(parse_session_list(r#"[{"id":"ses_1","title":"t","directory":""}]"#).is_empty());
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

    /// Every registered name round-trips through the forwarder — the two halves
    /// of the mechanism live in different languages and nothing else pins them
    /// together. A kebab name that stopped round-tripping would make `miao hook`
    /// reject its own plugin's calls at runtime, with the row simply never
    /// moving.
    #[test]
    fn every_registered_event_round_trips_through_the_forwarder() {
        for (native, event) in BUS_EVENTS.iter().chain(DIRECT_HOOKS) {
            assert_eq!(
                HookEvent::from_kebab(event.as_kebab()),
                Some(*event),
                "{native}"
            );
        }
    }

    /// **The distinction the first cut of this backend got wrong**, and the one
    /// worth a test of its own: [`DIRECT_HOOKS`] must contain only keys that
    /// exist on opencode's `Hooks` interface, and [`BUS_EVENTS`] only names that
    /// do *not*. A bus event name registered as a handler key subscribes
    /// nothing at all — silently, since a plugin returning an object with extra
    /// keys is not an error — and the row it was meant to move never moves.
    #[test]
    fn direct_hooks_are_hook_keys_and_bus_events_are_not() {
        // Verbatim from `packages/plugin/src/index.ts`'s `Hooks` interface.
        const HOOK_KEYS: &[&str] = &[
            "dispose",
            "event",
            "config",
            "tool",
            "auth",
            "provider",
            "chat.message",
            "chat.params",
            "chat.headers",
            "permission.ask",
            "command.execute.before",
            "tool.execute.before",
            "shell.env",
            "tool.execute.after",
            "experimental.chat.messages.transform",
            "experimental.chat.system.transform",
            "experimental.provider.small_model",
            "experimental.session.compacting",
            "experimental.compaction.autocontinue",
            "experimental.text.complete",
            "tool.definition",
        ];
        for (key, _) in DIRECT_HOOKS {
            assert!(HOOK_KEYS.contains(key), "{key} is not a Hooks key");
        }
        for (name, _) in BUS_EVENTS {
            assert!(
                !HOOK_KEYS.contains(name),
                "{name} is a Hooks key and must not be routed through the bus"
            );
        }
        // The specific six that were registered as handler keys and fired
        // nothing. Named so a future edit that re-adds one has to argue with
        // this test rather than rediscover the symptom.
        let direct: Vec<&str> = DIRECT_HOOKS.iter().map(|(k, _)| *k).collect();
        for was_wrong in [
            "session.created",
            "session.idle",
            "session.error",
            "session.compacted",
            "permission.asked",
            "permission.replied",
        ] {
            assert!(!direct.contains(&was_wrong), "{was_wrong}");
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
// started this session and does nothing else: no state, no retries, no reads of
// your data. Payloads are forwarded whole and picked apart on the other side;
// the only fields read here are the two that decide whether to forward at all.
// The launcher socket arrives in $CAPTAIN_MIAO_SOCK — it is never written into
// this file, so the file is identical for every session. Delete it and sessions
// stop being tracked; nothing else changes.
import { spawn } from "node:child_process";

const MIAO = "/home/miao/.local/bin/miao";

// opencode bus event type -> captain-miao event name. Anything not listed is
// dropped: the bus carries per-chunk and per-file events that would otherwise
// spawn hundreds of processes inside your session.
const BUS = {
  "session.created": "session-start",
  "session.updated": "cwd-changed",
  "permission.updated": "permission-request",
  "permission.replied": "elicitation-result",
  "session.idle": "stop",
  "session.error": "stop-failure",
  "session.compacted": "post-compact",
};

const CaptainMiao = async (ctx) => {
  const directory = ctx?.directory ?? null;
  // The returned promise settles when the child exits, so an opencode that
  // awaits its hooks delivers these events in the order it fired them; one
  // that does not await loses nothing. It never rejects.
  const send = (event, args) => {
    let body;
    try {
      body = JSON.stringify({ event, directory, payload: args });
    } catch {
      body = JSON.stringify({ event, directory });
    }
    return new Promise((resolve) => {
      let child;
      try {
        child = spawn(MIAO, ["hook", "--agent", "opencode", event], {
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
  };
  const report =
    (event) =>
    (...args) =>
      send(event, args);

  return {
    event: async (input) => {
      const e = input?.event;
      const type = e?.type;
      if (!type) return;
      // Authoritative but coarser than the dashboard's states, so only the
      // settle edge is taken: a session waiting on an approval is "busy" too.
      if (type === "session.status") {
        if (e.properties?.status?.type === "idle") return send("stop", [input]);
        return;
      }
      // Once per assistant turn, not once per streamed chunk. This is the
      // token and model column, not a status edge.
      if (type === "message.updated") {
        const info = e.properties?.info;
        if (info?.role !== "assistant" || !info?.time?.completed) return;
        return send("cwd-changed", [input]);
      }
      const name = BUS[type];
      if (name) return send(name, [input]);
    },
    "chat.message": report("prompt-submit"),
    "tool.execute.before": report("pre-tool-use"),
    "tool.execute.after": report("post-tool-use"),
    "experimental.session.compacting": report("pre-compact"),
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
