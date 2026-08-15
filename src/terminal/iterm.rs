//! iTerm2 backend (macOS only): wraps iTerm2's AppleScript dictionary.
//!
//! Transport, quoting and the probe shape are [`super::applescript`]'s; what is
//! here is what is iTerm2's. Needs iTerm2 ≥ 3.0, where `create tab with
//! default profile` and the session `id` arrived.
//!
//! Vocabulary. iTerm2's object model is `window > tab > session`, which is
//! Kitty's `os-window > tab > window` exactly — so a captain-miao **window** is
//! an iTerm2 *session* (one split), a captain-miao **tab** is an iTerm2 *tab*,
//! and the OS window is flattened away, the same reduction the Kitty and Ghostty
//! backends perform.
//!
//! What is *verified*. Everything covered by a test here is pure (script
//! building, snapshot parsing, id validation, the spawn payload, the startup
//! diagnosis); driving a real iTerm2 needs a Mac with a GUI session and a
//! hand-clicked Automation grant, which no CI can supply, so nothing below runs
//! in the suite. Every claim in this doc was measured by hand against iTerm2
//! 3.6.11, because the dictionary is wrong about four separate things.
//!
//! Backend properties this module encodes:
//! - **A process knows its own session from the environment.** `ITERM_SESSION_ID`
//!   is `w<win>t<tab>p<pane>:<UUID>`, and that UUID *is* `id of session` —
//!   verified by reading both for the same session. This is the whole reason
//!   iTerm2 is a cheaper backend than Ghostty: `current_window` is an env read,
//!   so a hand-typed `miao claude .` binds to its window like a Kitty one, and
//!   nothing has to write a nonce title to find itself.
//! - **Session ids are UUIDs, so they never recycle** — which is what makes the
//!   speculative `close_window` the restart/kill paths rely on safe, and what
//!   lets [`cm_core::terminal::iterm_identity`] stay non-instance-granular.
//! - **A tab has no id.** `index` is declared on the class and is not readable
//!   (`-1728`, measured), so there is no per-tab handle at all. [`TabId`] is
//!   therefore the id of the tab's *first session* — stable while that session
//!   lives, unique across the tree, and re-derived by every snapshot. A spawn
//!   makes a one-session tab, so the tab it reports is genuinely the one holding
//!   the window, which is what lets the dashboard trust it.
//! - **There is no way to set a title.** `set name of session` is accepted and
//!   silently ignored; `set title of tab` fails the event outright (`-10000`).
//!   Both measured. So `SpawnSpec::title` is applied from *inside* the session
//!   instead, as an OSC escape in [`spawn_payload`] — which iTerm2 does honour,
//!   showing it with the running job appended (`miao — src (node)`).
//! - **A spawn carries no working directory and a minimal `PATH`.** The
//!   `command` parameter is the only thing `create tab` takes: there is no
//!   directory parameter, the command is exec'd by `iTermServer` rather than by
//!   a login shell, and the `PATH` it inherits is
//!   `/usr/bin:/bin:/usr/sbin:/sbin:<iTerm.app>/Contents/Resources/utilities`
//!   (measured). Both are fixed in the payload — the `cd` explicitly, the `PATH`
//!   through the same `wrap_env` the multiplexer backends use for the same
//!   reason.
//! - **The `command` tokenizer is not a shell and not POSIX**, which is why the
//!   payload is base64'd rather than quoted — [`spawn_command`] carries the
//!   measurements.
//! - **Creating does not steal focus** (measured: the app stays not-frontmost),
//!   so `SpawnSpec::take_focus` is honoured in *both* directions here, unlike
//!   Ghostty. `select` alone does not raise the app either, so focusing takes an
//!   `activate` as well.
//! - **`contents of session` is the visible screen, unstyled.** No SGR survives
//!   (measured against red/bold/background output), and there is no scrollback
//!   property at all — so `capture: true`, but a `max_lines` larger than the
//!   window cannot be honoured. Plain text is a worse preview than Kitty's, not
//!   an absent one, so it stays a capability rather than a refusal.
//! - **There is no way to move a session between tabs**, so `move_to_tab: false`
//!   and the `t` affordance hides itself, exactly as on zellij and Ghostty.
//! - **Neither Stacked arrangement is worth having.** `floating_sessions` is
//!   clear-cut: the hotkey window is one app-wide dropdown, not a per-session
//!   floating pane. `window_stacking` fails for the reason it fails on Ghostty —
//!   sessions-as-splits is constructible, but iTerm2's splits are a binary tree,
//!   so the Nth session in a shared tab resizes the N-1 already there, and the
//!   dictionary exposes no "zoom this one" to switch between them with. Both
//!   false makes `layout_is_a_choice()` false, which hides `Space l` and
//!   resolves every spawn to `NewTab` — tmux's shape.
//!
//! **The one defect worth knowing about.** A `create tab`/`create window` whose
//! command exits immediately does not answer: the first such spawn takes ~4s,
//! and every later one — *including* ones whose command is long-lived — never
//! returns at all, while ordinary reads keep answering instantly. Only
//! restarting iTerm2 clears it. Measured repeatedly on 3.6.11, on a fresh
//! instance each time.
//!
//! captain-miao is largely insulated from it by accident: both spawn paths hold
//! themselves open on failure (a launcher through `hold_failed_launch`, an
//! attach through `ATTACH_REPORT_SCRIPT`), so the reachable case is a `miao`
//! binary that cannot be exec'd at all. [`SPAWN_TIMEOUT`] is what keeps that from
//! hanging the dashboard forever, and [`diagnose`] names the restart.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine as _;

use super::applescript::{
    CONTROL_PROBE_TIMEOUT, ProbeOutcome, SEP, SEP_PREAMBLE, applescript_string, osascript,
    shell_quote,
};
use super::{
    Capabilities, SpawnCommand, SpawnResult, SpawnSpec, SpawnTarget, Tab, TabId, TabTarget,
    Terminal, WindowId, tail_lines, wrap_env,
};

/// How long a [`spawn`](Terminal::spawn) waits for iTerm2 to answer before
/// giving up on it.
///
/// Not defensive padding: it bounds the wedge in the module doc, where the
/// create event never returns. Twenty seconds is far longer than a healthy
/// create (measured at well under one) and short enough that a wedged iTerm2
/// produces an error a user can act on rather than a dashboard that has stopped
/// responding to `o`.
const SPAWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// iTerm2's answer to [`Terminal::capabilities`]. Exported so tests assert
/// against the real value rather than a hand-built literal that could silently
/// diverge when a field is added. Rationale for each flag is in the module doc.
pub(crate) const CAPABILITIES: Capabilities = Capabilities {
    move_to_tab: false,
    window_stacking: false,
    floating_sessions: false,
    capture: true,
    graphics: false,
};

pub struct ItermTerminal {
    /// The session the dashboard itself runs in, read once from
    /// `ITERM_SESSION_ID`. `None` only if iTerm2 stopped exporting it — the
    /// dashboard then runs with no known window, which it reports honestly.
    session: Option<WindowId>,
}

impl ItermTerminal {
    /// Construct from the environment. `None` when this process is not inside an
    /// iTerm2 session — or when it is, but on a platform where that session
    /// can't be driven.
    ///
    /// The macOS gate is the same one [`super::ghostty::GhosttyTerminal::from_env`]
    /// carries, for a stronger reason: iTerm2 is macOS-only software, so a
    /// `TERM_PROGRAM=iTerm.app` seen anywhere else is a shell profile copied
    /// between machines rather than an iTerm2 to drive.
    pub fn from_env() -> Option<Self> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let term_program = std::env::var("TERM_PROGRAM").ok();
        cm_core::terminal::is_iterm(term_program.as_deref().map(str::trim)).then(|| Self {
            // Deliberately *not* `cm_core::terminal::current_window()`, which
            // runs the whole precedence: under a Kitty variable left stale in
            // this session that would answer with an outer Kitty window, and the
            // backend must name the session it is actually in.
            session: std::env::var("ITERM_SESSION_ID")
                .ok()
                .as_deref()
                .and_then(cm_core::terminal::iterm_surface)
                .map(WindowId),
        })
    }
}

/// Validate an id before it is interpolated into a script, failing closed.
///
/// The only ids this backend ever spends are session ids, which are UUIDs — so
/// the charset is `[A-Za-z0-9-]`, containing neither of AppleScript's string
/// terminators, and a validated id can never break out of the literal it lands
/// in. (Window ids are integers and tab ids are session ids, so neither adds a
/// shape.) The length cap keeps a corrupt state file from building an absurd
/// script. The only untrusted source is captain-miao's own state, so this
/// rejects rather than sanitizes — mis-targeting a session is worse than
/// refusing one.
fn script_id(id: &str) -> Result<&str> {
    if !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(id)
    } else {
        anyhow::bail!("refusing iterm id with unexpected characters: {id:?}");
    }
}

/// Turn a failed probe into an actionable message. The dashboard prints this and
/// exits, so it is the user's one chance to be told which of these quite
/// different things went wrong.
fn diagnose(outcome: ProbeOutcome<'_>) -> String {
    let problem = match outcome {
        ProbeOutcome::TimedOut => format!(
            "iTerm2 did not answer within {}s.\n\nThe first request makes macOS ask for \
             Automation permission in a dialog, and osascript waits for the answer — if no \
             dialog appeared, grant it under System Settings → Privacy & Security → \
             Automation.",
            CONTROL_PROBE_TIMEOUT.as_secs()
        ),
        ProbeOutcome::Failed { err } => {
            let lower = err.to_ascii_lowercase();
            // Ordered most-specific first. The missing-binary case carries our
            // own "Failed to run osascript" context; -1743 is TCC's refusal, and
            // a compile failure on `current window` means there is no iTerm2
            // dictionary to resolve it against.
            if lower.contains("failed to run osascript") {
                return format!(
                    "iTerm2 automation check failed: osascript could not be run ({err}).\n\n\
                     It ships with macOS at /usr/bin/osascript — this backend only works there."
                );
            } else if err.contains("-1743") || lower.contains("not authorized") {
                "macOS denied captain-miao permission to control iTerm2.\n\nGrant it under \
                 System Settings → Privacy & Security → Automation, in the entry for the \
                 terminal or launcher captain-miao runs from."
                    .to_string()
            } else if lower.contains("expected end of line")
                || lower.contains("doesn't understand")
                || err.contains("-2741")
                || err.contains("-1753")
                || err.contains("-1728")
                || err.contains("-1708")
            {
                format!(
                    "iTerm2 did not understand the request ({err}).\n\nThis backend needs the \
                     scripting interface iTerm2 3.0 introduced. Check iTerm2 → About, and that \
                     iTerm2 is running."
                )
            } else if lower.contains("-600") || lower.contains("application isn't running") {
                "iTerm2 does not appear to be running.\n\nStart it, and run captain-miao from an \
                 iTerm2 session."
                    .to_string()
            } else {
                format!("iTerm2 rejected the request: {err}")
            }
        }
    };
    format!("iTerm2 automation check failed: {problem}")
}

// ---- snapshot ----

/// The one snapshot script, run per [`snapshot`](Terminal::snapshot).
///
/// Walks `window > tab > session` and prints one line per tab:
/// `<tab id> SEP <focused> SEP <session id,…> SEP <title>`, where the tab id is
/// its first session's (the class has none of its own — module doc).
///
/// Three details are load-bearing:
/// - **A closed window lingers.** iTerm2 keeps the object in `windows` with zero
///   tabs and `visible` false for a while after a close (measured), and asking
///   such a window for `current session` raises. Both the `try` and the
///   "no sessions ⇒ no line" guard exist for that, not for a race.
/// - **A tab reference cannot be compared to `current tab`** — `t is ct` is
///   false even for the current tab (measured) — so the focused tab is found by
///   matching its sessions against the window's `current session` id instead.
/// - **`current window` is `missing value`** when nothing is open, which is an
///   ordinary state rather than an error. An empty `frontID` then matches no
///   window, which is the right answer: no tab is focused.
const SNAPSHOT_SCRIPT: &str = concat!(
    "set sep to character id 31\n",
    "set lf to character id 10\n",
    "tell application \"iTerm\"\n",
    "  set frontID to \"\"\n",
    "  try\n",
    "    set frontID to (id of current window) as text\n",
    "  end try\n",
    "  set out to \"\"\n",
    "  repeat with w in windows\n",
    "    set isFront to (((id of w) as text) is frontID)\n",
    "    set curSess to \"\"\n",
    "    try\n",
    "      set curSess to id of current session of w\n",
    "    end try\n",
    "    repeat with t in tabs of w\n",
    "      set ids to \"\"\n",
    "      set tabKey to \"\"\n",
    "      set isCur to false\n",
    "      repeat with s in sessions of t\n",
    "        set sid to id of s\n",
    "        if tabKey is \"\" then set tabKey to sid\n",
    "        if sid is curSess then set isCur to true\n",
    "        set ids to ids & sid & \",\"\n",
    "      end repeat\n",
    "      set focusedText to \"0\"\n",
    "      if isFront and isCur then set focusedText to \"1\"\n",
    "      if tabKey is not \"\" then\n",
    "        set out to out & tabKey & sep & focusedText & sep & ids",
    " & sep & (title of t) & lf\n",
    "      end if\n",
    "    end repeat\n",
    "  end repeat\n",
    "  return out\n",
    "end tell",
);

/// Parse [`SNAPSHOT_SCRIPT`]'s output into the trait's tab list.
///
/// Lenient by line: a tab whose id doesn't validate, or a line with too few
/// fields, is skipped rather than failing the whole snapshot — one unparseable
/// row must not blind the dashboard to every other window. Session ids are
/// filtered the same way, so a malformed one drops out of its tab instead of
/// taking the tab with it.
fn parse_snapshot(out: &str) -> Vec<Tab> {
    let mut tabs = Vec::new();
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        // The title is the remainder: it is free text and goes last precisely so
        // it cannot shift the fields before it.
        let mut fields = line.splitn(4, SEP);
        let (Some(tab_id), Some(focused), Some(sessions), Some(title)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(tab_id) = script_id(tab_id.trim()) else {
            continue;
        };
        let windows = sessions
            .split(',')
            .map(str::trim)
            .filter(|s| script_id(s).is_ok())
            .map(|s| WindowId(s.to_string()))
            .collect();
        tabs.push(Tab {
            id: TabId(tab_id.to_string()),
            title: title.to_string(),
            is_focused: focused.trim() == "1",
            windows,
        });
    }
    tabs
}

// ---- spawn ----

/// The `/bin/sh` script a spawn actually runs: chdir, title, then the command.
///
/// Three things iTerm2's `create tab` cannot do itself, done here because a real
/// shell can do all of them:
/// - **`cd`**, because the command parameter is the only one there is; a session
///   otherwise starts wherever `iTermServer` did (measured: `$HOME`). It exits
///   rather than continuing in the wrong directory, which would put an agent in
///   someone else's project.
/// - **the title**, as an OSC 0 escape, because both scripted setters are broken
///   (module doc). `printf`'s format is a fixed literal and the title is an
///   *argument*, so a title containing `%s` can't reformat anything.
/// - **`PATH`**, via the shared [`wrap_env`] — the same fix zellij and tmux
///   apply for the same reason, that the spawning server's environment is not
///   the dashboard's.
///
/// `exec` is the last statement so the agent replaces this shell rather than
/// leaving one waiting on it: the session's process tree is then `sh` → agent,
/// matching what every other backend produces. `hold` is the one case that
/// cannot `exec`, since something has to outlive the command.
fn spawn_payload(spec: &SpawnSpec) -> String {
    let mut s = String::new();
    if !spec.cwd.is_empty() {
        s.push_str(&format!("cd {} || exit 1\n", shell_quote(&spec.cwd)));
    }
    if let Some(title) = &spec.title {
        s.push_str(&format!(
            "printf '\\033]0;%s\\007' {}\n",
            shell_quote(title)
        ));
    }
    let argv = match &spec.command {
        SpawnCommand::Exec(argv) if !argv.is_empty() => {
            wrap_env(argv, std::env::var("PATH").ok().as_deref())
        }
        // `SpawnCommand::Shell` and a degenerate empty argv both mean "the
        // user's shell". iTerm2's own default would be the profile's command,
        // but taking that would cost the cd and the title above — so the shell
        // is named explicitly, from the dashboard's own environment, the way its
        // `PATH` is. `-l` because a terminal window is a login shell everywhere
        // else.
        _ => vec![
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
            "-l".to_string(),
        ],
    };
    let cmd = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    if spec.hold {
        // iTerm2 has no `--hold` of its own — a session ends with its command —
        // so holding means not letting the command *be* the last thing. `cat`
        // rather than a shell for the reason kitty's `--hold` is a hazard: a
        // held window must be a readable corpse, never a live login shell
        // wearing a session's title.
        s.push_str(&format!(
            "{cmd}\nprintf '\\n[captain-miao] exited (%s)\\n' \"$?\"\nexec /bin/cat\n"
        ));
    } else {
        s.push_str(&format!("exec {cmd}\n"));
    }
    s
}

/// Wrap a payload as the single `command` string iTerm2 will accept.
///
/// **iTerm2's tokenizer is neither a shell nor POSIX**, which is the whole reason
/// this is base64 rather than quoting. It splits the string itself and `execv`s
/// the result — there is no shell, so redirects and pipes mean nothing — and it
/// applies C-style backslash escapes *inside single quotes*, where POSIX says a
/// backslash is literal. All measured on 3.6.11 by round-tripping argv through a
/// probe: `'a\b'` arrived as `a<BS>`, `'a\\b'` also as `a<BS>`, `'a\033b'` as
/// `a<ESC>b`, and an unquoted `bare\\back` as `bareback`. So
/// [`shell_quote`]'s output is *not* safe here: any argument holding a backslash
/// would be silently rewritten, and the title, the cwd and the argv are all user
/// text that may.
///
/// Base64 sidesteps the tokenizer entirely: its alphabet is `[A-Za-z0-9+/=]`, so
/// the only characters this command string carries beyond the payload are the
/// fixed ones written here, and every byte of the payload reaches a real
/// `/bin/sh` intact. Verified by round-tripping a payload containing a
/// backslash, a double quote and a single quote — all three survived.
///
/// `printf %s` and `/usr/bin/base64` rather than a here-doc because the whole
/// thing must survive as *one* token: `eval` gets the decoded script, and the
/// decoding happens inside the shell that will run it.
fn spawn_command(payload: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
    format!("/bin/sh -c 'eval \"$(printf %s {b64} | /usr/bin/base64 -d)\"'")
}

/// The whole of what a [`spawn`](Terminal::spawn) sends: create the session and
/// read its id back.
///
/// `current window` is `missing value` when iTerm2 holds no live window, which
/// is ordinary — it stays running with every window closed, and the dashboard's
/// own session may be the one just closed. That arm takes `create window`, the
/// one creation command that needs nothing to already exist.
///
/// The nested `tell w` is not stylistic: `tell w to set t to …` would parse as
/// setting a `t` *property of the window*, and the one-line form that does work
/// needs a literal window id, which this doesn't have.
///
/// Pure, and separated from the call for that reason: the script is the part
/// that can be wrong, and it is the part no CI can run.
fn spawn_script(spec: &SpawnSpec) -> Result<String> {
    // Both Stacked arrangements are unsupported here (`CAPABILITIES`), so
    // `resolve_spawn_target` only ever yields `NewTab`; reaching either other
    // arm is a policy bug upstream rather than something to approximate.
    match spec.target {
        SpawnTarget::NewTab => {}
        SpawnTarget::Floating => {
            anyhow::bail!("floating session panes are not supported by the iterm backend")
        }
        SpawnTarget::SharedStackTab => {
            anyhow::bail!("stacked session tabs are not supported by the iterm backend")
        }
    }
    let cmd = applescript_string(&spawn_command(&spawn_payload(spec)));
    // Creating never raises the app (measured), so `take_focus: false` costs
    // nothing to honour and `true` is an explicit selection — the one direction
    // Ghostty cannot offer.
    let focus = if spec.take_focus {
        "  select s\n  select t\n  select w\n  activate\n"
    } else {
        ""
    };
    Ok(format!(
        "{SEP_PREAMBLE}\
         tell application \"iTerm\"\n\
         \x20 set w to current window\n\
         \x20 if w is missing value then\n\
         \x20   set w to (create window with default profile command {cmd})\n\
         \x20   set t to current tab of w\n\
         \x20 else\n\
         \x20   tell w\n\
         \x20     set t to (create tab with default profile command {cmd})\n\
         \x20   end tell\n\
         \x20 end if\n\
         \x20 set s to current session of t\n\
         {focus}\
         \x20 return (id of s)\n\
         end tell",
    ))
}

/// A script that walks the whole tree for the session `id` and runs `body` on
/// it, with `s`, `t` and `w` bound.
///
/// The walk *is* the lookup: there is no `session id "…"` at application level
/// (measured, `-1728`) and no `whose` filter that reaches across two levels of
/// containment, so every operation on a known session pays this. It is one Apple
/// event over a tree the size of the user's open windows.
///
/// `missing` is what runs when nothing matched — the one thing callers disagree
/// about. A close wants silence (the id may already be gone, which is exactly
/// when a speculative close is called); a capture wants an error, because the
/// preview reads a failed read as evidence the binding is stale and an empty
/// string would render as a live-but-blank window instead.
fn walk_script(id: &str, body: &str, missing: &str) -> Result<String> {
    let id = applescript_string(script_id(id)?);
    Ok(format!(
        "tell application \"iTerm\"\n\
         \x20 repeat with w in windows\n\
         \x20   repeat with t in tabs of w\n\
         \x20     repeat with s in sessions of t\n\
         \x20       if (id of s) is {id} then\n\
         {body}\
         \x20       end if\n\
         \x20     end repeat\n\
         \x20   end repeat\n\
         \x20 end repeat\n\
         {missing}\
         end tell"
    ))
}

#[async_trait]
impl Terminal for ItermTerminal {
    fn current_window(&self) -> Option<WindowId> {
        self.session.clone()
    }

    fn identity(&self) -> Option<String> {
        Some(cm_core::terminal::iterm_identity())
    }

    /// Prove the Apple-event channel to iTerm2 works, with the cheapest real
    /// request there is.
    ///
    /// It asks about `current window` rather than the standard-suite `version`
    /// on purpose: `version` resolves against any application, while
    /// `current window` is iTerm2's own terminology, so a build without the
    /// scripting interface fails to compile the script at all — which is exactly
    /// the signal that separates "too old" from "not permitted". Comparing it
    /// against `missing value` rather than reading a property off it keeps the
    /// probe valid when no window is open.
    ///
    /// The timeout is not belt-and-braces: the *first* request a new install
    /// makes is the one macOS interrupts with an Automation consent dialog, and
    /// `osascript` blocks until it is answered — see [`CONTROL_PROBE_TIMEOUT`].
    async fn verify_control(&self) -> Result<()> {
        let probe =
            osascript("tell application \"iTerm\" to return (current window is missing value)");
        match tokio::time::timeout(CONTROL_PROBE_TIMEOUT, probe).await {
            Ok(Ok(_)) => Ok(()),
            // `{e:#}` flattens the anyhow chain onto one line — the context
            // ("Failed to run osascript") is what `diagnose` classifies on.
            Ok(Err(e)) => {
                let err = format!("{e:#}");
                anyhow::bail!("{}", diagnose(ProbeOutcome::Failed { err: &err }))
            }
            Err(_elapsed) => anyhow::bail!("{}", diagnose(ProbeOutcome::TimedOut)),
        }
    }

    async fn snapshot(&self) -> Result<Vec<Tab>> {
        Ok(parse_snapshot(&osascript(SNAPSHOT_SCRIPT).await?))
    }

    /// Create a session per `spec`.
    ///
    /// Bounded by [`SPAWN_TIMEOUT`] because this is the call the module doc's
    /// wedge stops answering. The tab is created either way, so a timeout is not
    /// a clean failure — but the launcher inside it self-reports its own session
    /// from `ITERM_SESSION_ID`, so the row still arrives bound. What the error
    /// buys is telling the user *why* the next spawn will be slow too, and that
    /// restarting iTerm2 is the fix.
    async fn spawn(&self, spec: SpawnSpec) -> Result<SpawnResult> {
        let script = spawn_script(&spec)?;
        let out = match tokio::time::timeout(SPAWN_TIMEOUT, osascript(&script)).await {
            Ok(res) => res?,
            Err(_elapsed) => anyhow::bail!(
                "iTerm2 did not answer the spawn within {}s.\n\nThe window was probably still \
                 created — check for it. iTerm2 stops answering `create tab` after any spawn \
                 whose command exits immediately, and only a restart of iTerm2 clears that.",
                SPAWN_TIMEOUT.as_secs()
            ),
        };
        let id = script_id(out.trim())
            .context("Failed to parse a session id from iterm spawn output")?
            .to_string();
        // A fresh tab holds exactly one session, so that session is also the
        // tab's first — which is what a `TabId` is on this backend. Reporting it
        // is what keeps the next reload off a resolving snapshot.
        Ok(SpawnResult {
            window: Some(WindowId(id.clone())),
            tab: Some(TabId(id)),
        })
    }

    /// Focus the session `id`.
    ///
    /// All four commands run, and in this order. `select` on a session, tab and
    /// window makes each current within its container but does *not* raise
    /// iTerm2 itself (measured: the app stays not-frontmost), so `activate` is
    /// what actually puts the session in front of the user — which is what the
    /// caller asked for.
    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        let script = walk_script(
            id.as_str(),
            "select s\nselect t\nselect w\nactivate\nreturn\n",
            "",
        )?;
        osascript(&script).await?;
        Ok(())
    }

    /// Focus the tab `id` — which is a session id (module doc), so the same walk
    /// finds it. The tab's own current session is left alone: the caller asked to
    /// be looking at the tab, not to move the selection inside it.
    async fn focus_tab(&self, id: &TabId) -> Result<()> {
        let script = walk_script(id.as_str(), "select t\nselect w\nactivate\nreturn\n", "")?;
        osascript(&script).await?;
        Ok(())
    }

    /// Close the session `id`.
    ///
    /// Safe to call speculatively, as the restart/kill paths do: session ids are
    /// UUIDs and so never recycle, and a walk that matches nothing simply ends.
    /// `close` on a session with a running job returns immediately and takes the
    /// job with it — no confirmation sheet, verified against a live one.
    async fn close_window(&self, id: &WindowId) -> Result<()> {
        let script = walk_script(id.as_str(), "close s\nreturn\n", "")?;
        osascript(&script).await?;
        Ok(())
    }

    /// Capture the session's screen.
    ///
    /// `contents` is the *visible* screen and nothing more — iTerm2 exposes no
    /// scrollback property — so `max_lines` can only ever trim, never reach
    /// further back. Unstyled, too: no SGR survives the round trip.
    ///
    /// A session that isn't there is an error rather than an empty string, which
    /// is the signal the preview loop actually wants: it reads a failed capture
    /// as evidence the binding is stale, and `""` would render as a live window
    /// showing nothing.
    async fn capture_text(&self, id: &WindowId, max_lines: usize) -> Result<String> {
        let script = walk_script(
            id.as_str(),
            "return contents of s\n",
            "  error \"no such iTerm2 session\"\n",
        )?;
        let out = osascript(&script).await?;
        Ok(tail_lines(&out, max_lines).to_string())
    }

    async fn move_window_to_tab(&self, _id: &WindowId, _to: TabTarget) -> Result<()> {
        anyhow::bail!("moving a session between tabs is not supported by the iterm backend")
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: SpawnCommand) -> SpawnSpec {
        SpawnSpec {
            cwd: "/home/miao/my code".into(),
            target: SpawnTarget::NewTab,
            command,
            title: Some("miao — my code".into()),
            hold: false,
            take_focus: false,
            stack: true,
        }
    }

    #[test]
    fn session_uuids_are_accepted_and_steering_ids_are_not() {
        assert!(script_id("D36E57AE-D742-4DB5-8EB7-32F6AE35D8A2").is_ok());
        // Neither AppleScript string terminator can appear in a UUID, so both
        // are rejected outright rather than escaped — a mis-targeted session op
        // is worse than a refused one.
        assert!(script_id("").is_err());
        assert!(script_id("aa\" or true or \"").is_err());
        assert!(script_id("aa\\").is_err());
        assert!(
            script_id("w0t0p0:D36E57AE").is_err(),
            "the prefix is stripped before it gets here"
        );
        assert!(script_id("aa\nclose s").is_err());
        assert!(script_id(&"a".repeat(129)).is_err());
    }

    #[test]
    fn snapshot_parses_tabs_sessions_and_the_one_focused_tab() {
        let out = "AAAAAAAA-1111\u{1f}0\u{1f}AAAAAAAA-1111,\u{1f}~/src\n\
                   BBBBBBBB-2222\u{1f}1\u{1f}BBBBBBBB-2222,CCCCCCCC-3333,\u{1f}miao\n";
        let tabs = parse_snapshot(out);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, TabId("AAAAAAAA-1111".into()));
        assert_eq!(tabs[0].title, "~/src");
        assert!(!tabs[0].is_focused);
        // A tab's id is its first session's, so the two agree by construction.
        assert_eq!(tabs[0].windows, vec![WindowId("AAAAAAAA-1111".into())]);
        // Only the current tab of the *front* window is focused, which the
        // script has already resolved to the `1` here.
        assert!(tabs[1].is_focused);
        assert_eq!(tabs[1].windows.len(), 2);
    }

    #[test]
    fn snapshot_survives_a_title_carrying_the_separator_or_a_bad_row() {
        // The title is the remainder of the line, so a separator inside it can't
        // shift the id fields — the reason it is emitted last.
        let out = format!(
            "AAAA-1111\u{1f}1\u{1f}AAAA-1111,\u{1f}we{SEP}ird\n\
             \n\
             not-enough-fields\n\
             CCCC-3333\u{1f}0\u{1f}bad id,\u{1f}dropped\n"
        );
        let tabs = parse_snapshot(&out);
        assert_eq!(tabs.len(), 2, "the malformed row is skipped, not fatal");
        assert_eq!(tabs[0].title, format!("we{SEP}ird"));
        // A bad session id drops out of its tab rather than taking the tab.
        assert!(tabs[1].windows.is_empty());
    }

    #[test]
    fn the_payload_chdirs_titles_and_execs_in_that_order() {
        let payload = spawn_payload(&spec(SpawnCommand::Exec(vec![
            "miao".into(),
            "claude".into(),
            "/home/miao/my code".into(),
        ])));
        let at = |needle: &str| payload.find(needle).unwrap_or_else(|| panic!("{payload}"));
        // The cd must land before anything that depends on it, and the exec must
        // be last — it replaces this shell.
        assert!(at("cd '/home/miao/my code' || exit 1") < at("printf '\\033]0;%s\\007'"));
        assert!(at("printf '\\033]0;%s\\007'") < at("exec "));
        assert!(
            payload.trim_end().ends_with("'/home/miao/my code'"),
            "{payload}"
        );
        // The title is an *argument*, never part of the format string, so a
        // title holding `%s` cannot reformat anything.
        assert!(
            payload.contains("printf '\\033]0;%s\\007' 'miao — my code'"),
            "{payload}"
        );
        // The spawning server's environment is not the dashboard's, so the argv
        // is re-pointed at ours — the same fix zellij and tmux apply.
        assert!(payload.contains("/usr/bin/env PATH="), "{payload}");
    }

    #[test]
    fn a_held_payload_outlives_its_command_without_becoming_a_shell() {
        let mut s = spec(SpawnCommand::Exec(vec!["miao".into()]));
        s.hold = true;
        let payload = spawn_payload(&s);
        // Not `exec miao`: something has to still be there afterwards.
        assert!(!payload.contains("exec /usr/bin/env"), "{payload}");
        assert!(payload.contains("exec /bin/cat"), "{payload}");
        // A held window is a readable corpse, never a live login shell wearing
        // the session's title — the hazard kitty's `--hold` documents.
        assert!(!payload.contains("-l\n"), "{payload}");
    }

    #[test]
    fn the_command_reaches_iterm_as_base64_and_nothing_else() {
        // iTerm2's tokenizer rewrites backslashes even inside single quotes, so
        // the payload must not travel as text. Every byte of a payload that is
        // *made* of backslashes and quotes has to survive.
        let payload = "cd '/a\\b' || exit 1\nprintf 'it\\'s'\nexec x\n";
        let cmd = spawn_command(payload);
        let b64 = cmd
            .split_whitespace()
            .find(|w| {
                w.len() > 16
                    && w.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
            })
            .unwrap_or_else(|| panic!("{cmd}"));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), payload);
        // Nothing of the payload leaks into the command string itself.
        assert!(!cmd.contains('\\'), "{cmd}");
        assert!(!cmd.contains("exit 1"), "{cmd}");
    }

    #[test]
    fn a_spawn_script_creates_reads_the_id_back_and_refuses_what_it_cannot_do() {
        let script = spawn_script(&spec(SpawnCommand::Shell)).unwrap();
        let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{script}"));
        // Both arms leave the new tab in `t`, and the session is read off it.
        assert!(at("if w is missing value then") < at("set s to current session of t"));
        assert!(
            script.contains("create window with default profile command"),
            "{script}"
        );
        assert!(
            script.contains("create tab with default profile command"),
            "{script}"
        );
        assert!(script.ends_with("return (id of s)\nend tell"), "{script}");
        // `tell w to set t to …` would set a property *of the window*; the
        // nested block is what makes the assignment ours.
        assert!(script.contains("tell w\n"), "{script}");
        assert!(!script.contains("tell w to set"), "{script}");
        // take_focus is off here, so nothing raises the app.
        assert!(!script.contains("activate"), "{script}");

        // The two arrangements `CAPABILITIES` denies are refused rather than
        // approximated, so a policy bug upstream is loud.
        for target in [SpawnTarget::Floating, SpawnTarget::SharedStackTab] {
            let mut s = spec(SpawnCommand::Shell);
            s.target = target;
            assert!(spawn_script(&s).is_err());
        }
    }

    #[test]
    fn take_focus_is_honoured_in_both_directions() {
        // The one thing Ghostty cannot do: creating does not raise iTerm2, so
        // `false` costs nothing and `true` is an explicit selection.
        let mut s = spec(SpawnCommand::Shell);
        s.take_focus = true;
        assert!(spawn_script(&s).unwrap().contains("activate"));
        s.take_focus = false;
        assert!(!spawn_script(&s).unwrap().contains("activate"));
    }

    #[test]
    fn a_walk_says_what_to_do_when_nothing_matched() {
        // A close wants silence — the id may already be gone, which is exactly
        // when the restart/kill paths call it.
        let close = walk_script("AAAA-1111", "  close s\n", "").unwrap();
        assert!(
            close.contains("if (id of s) is \"AAAA-1111\" then"),
            "{close}"
        );
        assert!(!close.contains("error"), "{close}");
        // A capture wants an error, or the preview reads an empty string as a
        // live-but-blank window instead of a stale binding.
        let capture =
            walk_script("AAAA-1111", "  return contents of s\n", "  error \"x\"\n").unwrap();
        assert!(capture.contains("error \"x\""), "{capture}");
        // An id that could steer the script never reaches one.
        assert!(walk_script("aa\" or true or \"", "", "").is_err());
    }

    #[test]
    fn every_configuration_failure_names_its_own_fix() {
        // Automation denial and a too-old iTerm2 need opposite fixes — a System
        // Settings toggle versus an upgrade — so neither may be described in the
        // other's terms.
        let denied = diagnose(ProbeOutcome::Failed {
            err: "osascript failed: execution error: Not authorized to send Apple events to \
                  iTerm. (-1743)",
        });
        assert!(denied.contains("Automation"), "{denied}");
        assert!(!denied.contains("3.0"), "{denied}");

        let too_old = diagnose(ProbeOutcome::Failed {
            err: "osascript failed: execution error: iTerm got an error: Can't get current \
                  window. (-1728)",
        });
        assert!(too_old.contains("3.0"), "{too_old}");
        assert!(!too_old.contains("Automation"), "{too_old}");

        // A missing osascript is the one failure no iTerm2 setting can fix.
        let no_binary = diagnose(ProbeOutcome::Failed {
            err: "Failed to run osascript: No such file or directory (os error 2)",
        });
        assert!(no_binary.contains("/usr/bin/osascript"), "{no_binary}");
        assert!(!no_binary.contains("Automation"), "{no_binary}");

        // A hang is the *consent dialog*, so the timeout message must point at
        // it rather than suggest retrying.
        let timed_out = diagnose(ProbeOutcome::TimedOut);
        assert!(timed_out.contains("Automation"), "{timed_out}");
    }

    /// iTerm2 is macOS-only software, so detection must not claim a session it
    /// cannot drive on the strength of a `TERM_PROGRAM` copied between machines.
    #[test]
    fn from_env_is_macos_only() {
        if !cfg!(target_os = "macos") {
            assert!(ItermTerminal::from_env().is_none());
        }
    }
}
