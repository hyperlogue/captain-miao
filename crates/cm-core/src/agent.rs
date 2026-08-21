//! `AgentControl` is the dashboard's interface to a coding-agent CLI. It is
//! per-session, not per-process: a single dashboard runs sessions from several
//! backends side by side, dispatching every backend-shaped operation through
//! the variant stored on each `LauncherState`.
//!
//! The variants carry no instance state — methods are pure functions of
//! `self` and forward to the matching `agents::<name>` module.
//!
//! Adding a new backend is meant to be: add a variant, add a module under
//! `agents/`, extend each `match` here. No registry, no dyn dispatch.
//!
//! [`AgentControl::Unknown`] is the one variant that isn't a backend. `agent`
//! rides `LauncherState`, and `LauncherState` rides the `Snapshot` frame as a
//! `Vec` — so a single element that refuses to decode fails the *whole* frame,
//! and one session started under a backend this build predates would blank
//! every row on that host. The protocol's frame-level rule (an unknown frame
//! decodes to `Unknown` and is ignored) therefore extends one level down to
//! this field: an unrecognized name decodes here instead of erroring, every
//! method answers inertly, and the row stays visible, sortable and killable
//! while nothing can be launched from it.
//!
//! What keeps that safe is that **`Unknown` is a read-side-only state**. It
//! arises solely from decoding a value this build does not know: `from_cli`
//! never yields it (so `miao --agent nonsense` is still an honest error, not a
//! dimmed row), it is not in `ALL` (so no picker or `Ctrl-t` cycle can land on
//! it), and no launcher may ever write it into a state file — a host knows its
//! own backends, so `"agent":"unknown"` on the wire can only be a decoded value
//! being echoed back, which the launch path refuses rather than guessing. That
//! last clause is *enforced*, not merely expected: the two places an argv or a
//! command gets built — [`crate::backend::LocalBackend::open_session`] and
//! [`AgentControl::build_launch_command`] — both refuse with
//! [`UNKNOWN_AGENT_REFUSAL`], so no path can turn an undecodable name into a
//! process.
//!
//! It deliberately does not preserve the original name. `AgentControl` is
//! `Copy`, is used as a `HashMap` key, and every method takes `self` by value;
//! carrying a `String` would cost all of that repo-wide to hold a label nothing
//! is allowed to act on.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::process::Command;

use crate::agents;
use crate::agents::{antigravity, claude, codex, grok, kimi, omp, opencode, pi, reasonix};
use crate::state::{HookEvent, HookMessage, LauncherState};

/// `Deserialize` is hand-written (see below) so an unrecognized name lands on
/// [`AgentControl::Unknown`]; `Serialize` stays derived — `rename_all` maps that
/// variant to `"unknown"`, which decodes back to itself.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentControl {
    #[default]
    Claude,
    Codex,
    Reasonix,
    Kimi,
    Grok,
    /// sst's `opencode`. Spelled lowercase everywhere, including
    /// [`Self::label`] — it is the project's own styling, not a typo.
    OpenCode,
    Pi,
    /// Google's Antigravity CLI. The variant, the on-disk name and the
    /// subcommand are all the product's name; `agy` is only what it installs
    /// as, and is spelled that way in one place ([`agents::antigravity::BIN`]).
    Antigravity,
    /// `can1357/oh-my-pi` ("omp"), a heavily-evolved fork of pi. Hooked with a
    /// generated extension passed as `omp -e`; nothing of yours is touched.
    /// omp's event surface has diverged from pi's in four load-bearing ways —
    /// see `agents::omp`'s module doc.
    Omp,
    /// A backend name this build doesn't know — a newer host's session seen by
    /// an older dashboard. Read-side only: produced by `Deserialize` alone,
    /// never by `from_cli`, never in `ALL`, never written by a launcher. Every
    /// method answers inertly so the row survives; launching from it errors.
    /// See the module doc for why it holds no name.
    Unknown,
}

/// What both write-side guards say when they refuse an [`AgentControl::Unknown`]
/// — [`AgentControl::build_launch_command`] and
/// [`crate::backend::LocalBackend::open_session`]. Shared so the two can't drift
/// into describing the same situation differently, and so a caller that only
/// ever sees one of them still learns the fix.
pub const UNKNOWN_AGENT_REFUSAL: &str = "unknown agent backend: this build doesn't know it (the \
                                         request comes from a newer captain-miao); upgrade \
                                         captain-miao to launch it";

impl<'de> Deserialize<'de> for AgentControl {
    /// Accepts exactly the names [`AgentControl::cli_subcommand`] emits and maps
    /// everything else to [`AgentControl::Unknown`] — the whole point being that
    /// a value from a newer host must not fail the frame it arrived in. Matching
    /// is exact, as on-disk state has always been; the case-insensitive spelling
    /// belongs to `from_cli`, where a human types the name.
    ///
    /// Still string-only: a number or an object is a malformed field, not a
    /// backend this build predates, and is reported as the decode error it is.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct AgentVisitor;
        impl serde::de::Visitor<'_> for AgentVisitor {
            type Value = AgentControl;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an agent backend name")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<AgentControl, E> {
                Ok(match v {
                    "claude" => AgentControl::Claude,
                    "codex" => AgentControl::Codex,
                    "reasonix" => AgentControl::Reasonix,
                    "kimi" => AgentControl::Kimi,
                    "grok" => AgentControl::Grok,
                    "opencode" => AgentControl::OpenCode,
                    "pi" => AgentControl::Pi,
                    "antigravity" => AgentControl::Antigravity,
                    "omp" => AgentControl::Omp,
                    _ => AgentControl::Unknown,
                })
            }
        }
        d.deserialize_str(AgentVisitor)
    }
}

impl AgentControl {
    /// Every backend this build can actually drive — `Unknown` is deliberately
    /// absent, which is what keeps it out of the `Space a` picker and the
    /// `Ctrl-t` cycle.
    ///
    /// **Append, never reorder**: this order *is* the `Ctrl-t` cycle order and
    /// the `Space a` picker order, both of which users learn by muscle memory.
    pub const ALL: &'static [AgentControl] = &[
        AgentControl::Claude,
        AgentControl::Codex,
        AgentControl::Reasonix,
        AgentControl::Kimi,
        AgentControl::Grok,
        AgentControl::OpenCode,
        AgentControl::Pi,
        AgentControl::Antigravity,
        AgentControl::Omp,
    ];

    /// CLI subcommand the dashboard launches to wrap this agent
    /// (e.g. `miao claude .`).
    pub fn cli_subcommand(self) -> &'static str {
        match self {
            AgentControl::Claude => "claude",
            AgentControl::Codex => "codex",
            AgentControl::Reasonix => "reasonix",
            AgentControl::Kimi => "kimi",
            AgentControl::Grok => "grok",
            AgentControl::OpenCode => "opencode",
            AgentControl::Pi => "pi",
            AgentControl::Antigravity => "antigravity",
            AgentControl::Omp => "omp",
            AgentControl::Unknown => "",
        }
    }

    /// Parse the `--agent` flag / config value. Mirrors `cli_subcommand`.
    ///
    /// Never yields `Unknown`: a name this build can't drive stays `None` so the
    /// caller reports it, rather than launching into an inert backend.
    pub fn from_cli(s: &str) -> Option<AgentControl> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Some(AgentControl::Claude),
            "codex" => Some(AgentControl::Codex),
            "reasonix" => Some(AgentControl::Reasonix),
            "kimi" => Some(AgentControl::Kimi),
            "grok" => Some(AgentControl::Grok),
            "opencode" => Some(AgentControl::OpenCode),
            "pi" => Some(AgentControl::Pi),
            "antigravity" => Some(AgentControl::Antigravity),
            "omp" => Some(AgentControl::Omp),
            _ => None,
        }
    }

    /// Whether this agent's binary resolves on `$PATH`.
    ///
    /// **Advisory, and it gates the cycle rather than the launch.** `Ctrl-t`
    /// steps through [`Self::ALL`] modulo its length: at two backends that was a
    /// toggle, but each new one adds a stop the user most likely has not
    /// installed, and an affordance that degrades with every agent we add is the
    /// kind of cost that should not ride along silently. Choosing an agent
    /// *deliberately* — `Space a`'s picker, a config value, `--agent` — is never
    /// filtered: the answer here can only be stale (a `PATH` we can't see, an
    /// agent installed a second ago), so it must not be able to make a session
    /// unlaunchable. A missing binary still errors honestly, and from the one
    /// place that actually knows: `build_launch_command`.
    pub fn is_available(self) -> bool {
        match self {
            AgentControl::Claude => agents::binary_available(claude::BIN),
            AgentControl::Codex => agents::binary_available(codex::BIN),
            AgentControl::Reasonix => agents::binary_available(reasonix::BIN),
            AgentControl::Kimi => agents::binary_available(kimi::BIN),
            AgentControl::Grok => agents::binary_available(grok::BIN),
            AgentControl::OpenCode => agents::binary_available(opencode::BIN),
            AgentControl::Pi => agents::binary_available(pi::BIN),
            AgentControl::Antigravity => agents::binary_available(antigravity::BIN),
            AgentControl::Omp => agents::binary_available(omp::BIN),
            // Not a backend this build can launch at all, so no binary could
            // make it available. It is absent from `ALL` besides.
            AgentControl::Unknown => false,
        }
    }

    /// Human-facing backend name for headers and status lines.
    pub fn label(self) -> &'static str {
        match self {
            AgentControl::Claude => "Claude",
            AgentControl::Codex => "Codex",
            AgentControl::Reasonix => "Reasonix",
            AgentControl::Kimi => "Kimi",
            AgentControl::Grok => "Grok",
            // Lowercase on purpose: sst styles the project `opencode`.
            AgentControl::OpenCode => "opencode",
            AgentControl::Pi => "Pi",
            AgentControl::Omp => "Oh My Pi (OMP)",
            AgentControl::Antigravity => "Antigravity",
            // Truthful and neutral: the row is an agent session, we just can't
            // say which backend.
            AgentControl::Unknown => "Agent",
        }
    }

    /// Extra args appended after the cwd to resume (or fork) `session_id`.
    ///
    /// The flag shapes differ per backend and each arm states its own; the
    /// three *kinds* are what matter here. Most decorate a resume with a fork
    /// flag (Claude and Grok share `--resume <id>` / `--fork-session`; Reasonix
    /// takes `-r <id>` / `--copy`; opencode `-s <id>` / `--fork`). Two swap the
    /// entry point instead of adding to it — Codex's `resume` / `fork`
    /// subcommands, and Pi's `--session` / `--fork`, which are alternatives
    /// rather than a flag and its modifier. One ignores `fork` entirely (Kimi),
    /// which is how a backend says it cannot branch: the two argvs come out
    /// equal and [`Self::supports_fork`] reads that off directly.
    pub fn resume_args(self, session_id: &str, fork: bool) -> Vec<String> {
        match self {
            // Grok's flags are byte-identical to Claude's here (`17-sessions.md`
            // documents `--resume <id>` and `--fork-session`), which is why the
            // two share an arm rather than repeating one.
            AgentControl::Claude | AgentControl::Grok => {
                let mut v = vec!["--resume".to_string(), session_id.to_string()];
                if fork {
                    v.push("--fork-session".to_string());
                }
                v
            }
            AgentControl::Codex => {
                let sub = if fork { "fork" } else { "resume" };
                vec![sub.to_string(), session_id.to_string()]
            }
            AgentControl::Reasonix => {
                // `--resume QUERY` also accepts a path or a unique title; the id
                // is the exact form. `--copy` is its fork: the original
                // transcript is left untouched and the copy is writable, which
                // is also how you resume a session another process holds.
                let mut v = vec!["-r".to_string(), session_id.to_string()];
                if fork {
                    v.push("--copy".to_string());
                }
                v
            }
            // **`fork` is ignored, deliberately.** Kimi documents no fork flag,
            // and ignoring the argument is how a backend says so: the two argvs
            // come out equal, [`Self::supports_fork`] is then false, and the
            // dashboard hides `f` — no second match to keep in sync, and no
            // chance of offering a key that silently resumes in place instead.
            //
            // `--session <id>` and `--continue` are mutually exclusive, so only
            // ever one of them goes on an argv. We always name the id, which is
            // also why the bare `--session` form (which opens Kimi's session
            // browser) can't be reached from here.
            AgentControl::Kimi => vec!["--session".to_string(), session_id.to_string()],
            // `-s <id>` plus `--fork`, both documented flags on the root TUI
            // command. The id reaches `LauncherState` on `session.created` and
            // on every direct hook (`agents::opencode`), so unlike the first cut
            // of that backend this is reachable. (`-c` / `--continue` resumes
            // the most recent session in a directory without an id, but this
            // seam is id-shaped and inventing an id-less path for one backend is
            // not the trade.)
            AgentControl::OpenCode => {
                let mut v = vec!["-s".to_string(), session_id.to_string()];
                if fork {
                    v.push("--fork".to_string());
                }
                v
            }
            // Pi's fork **replaces** the resume rather than decorating it:
            // `--session <path|id>` opens that session, `--fork <path|id>` forks
            // it into a new one, and they are alternative entry points rather
            // than a flag and its modifier. So the flag name swaps and the id
            // stays — the same shape as Codex's `resume`/`fork` subcommands, and
            // the reason `supports_fork()` needs no special case here.
            AgentControl::Pi => {
                let flag = if fork { "--fork" } else { "--session" };
                vec![flag.to_string(), session_id.to_string()]
            }
            // omp's fork **replaces** the resume rather than decorating it, the
            // Pi/Codex shape: `--resume <id>` opens that session, `--fork <id>`
            // forks it into a new one, and they are alternative entry points
            // rather than a flag and its modifier. `--resume` is in the public
            // flag schema and `--help`; `--fork` is in the argv map and
            // documented in `omp://session-operations-export-share-fork-resume.md`.
            // The undocumented `--session` alias is deliberately not used.
            AgentControl::Omp => {
                let flag = if fork { "--fork" } else { "--resume" };
                vec![flag.to_string(), session_id.to_string()]
            }
            // **`fork` is ignored**, the Kimi shape: `--conversation <id>`
            // resumes, and `agy`'s flag list offers nothing that branches one —
            // Antigravity forks a conversation from inside its own TUI, which is
            // not an argv this seam can reach. So the two argvs come out equal,
            // `supports_fork` reads that off, and `f` hides itself rather than
            // silently resuming in place. Probed against `agy` 1.1.11: the
            // resumed session reports the id it was given on its next hook.
            AgentControl::Antigravity => {
                vec!["--conversation".to_string(), session_id.to_string()]
            }
            AgentControl::Unknown => vec![],
        }
    }

    /// Extra args that launch this agent into an isolated git **worktree**, or
    /// `None` when the agent has no worktree concept.
    ///
    /// The worktree itself is entirely the *agent's* — captain-miao never runs
    /// `git worktree add`. Claude Code creates it under
    /// `.claude/worktrees/<name>/` on a new branch, honours `worktree.baseRef`
    /// and `.worktreeinclude`, blocks edits that would reach the main checkout,
    /// and cleans up when the session exits; a resume returns the session to it
    /// with no help from us. Owning any of that here would mean a second,
    /// disagreeing implementation of a thing the agent already does better.
    ///
    /// `name` is the worktree name; `None` lets the agent generate one (Claude
    /// mints e.g. `bright-running-fox`). Codex 0.147 has no equivalent flag, so
    /// it answers `None` and the dashboard hides the affordance — the same shape
    /// as [`Self::session_watch_path`] and [`Self::bg_shells`].
    pub fn worktree_args(self, name: Option<&str>) -> Option<Vec<String>> {
        match self {
            AgentControl::Claude => {
                let mut v = vec!["--worktree".to_string()];
                // A `#`-prefixed PR number is a legitimate name (`--worktree
                // "#1234"` branches from that PR), so nothing here inspects it.
                if let Some(name) = name.filter(|n| !n.is_empty()) {
                    v.push(name.to_string());
                }
                Some(v)
            }
            // Grok has `--worktree [NAME]` too (`tutorial/06-worktrees.md`), with
            // the same ownership story: it creates the worktree, names it, tracks
            // its age and collects it (`grok worktree gc`), and a resume re-enters
            // it without the flag.
            //
            // **The `=` form, one argv element**, which the tutorial is explicit
            // about: the value is *optional*, so a bare `--worktree` followed by
            // any positional swallows it as the name. Nothing captain-miao spawns
            // passes a trailing positional today — `split_cwd` consumes the cwd
            // before the agent sees it — so the separated form would work right
            // up until something appended one, and then fail silently by opening
            // a worktree named after the argument.
            AgentControl::Grok => Some(match name.filter(|n| !n.is_empty()) {
                Some(name) => vec![format!("--worktree={name}")],
                // Nothing follows it in our argv, so the bare flag is safe here —
                // and it is the only way to ask Grok to mint the name.
                None => vec!["--worktree".to_string()],
            }),
            AgentControl::Codex => None,
            // Reasonix has isolated "Delivery" workspaces of its own (they even
            // have a state dir), but no CLI flag launches into one, so there is
            // nothing to spend a worktree request on.
            AgentControl::Reasonix => None,
            // No documented worktree flag. `supports_worktrees()` is derived from
            // this, so `Ctrl-g` hides itself with nothing else to change.
            AgentControl::Kimi => None,
            // opencode's *plugin context* exposes a `worktree` field, so it has
            // the concept — but no CLI flag launches into one, and there is
            // nothing to spend a worktree request on. `Ctrl-g` hides itself.
            AgentControl::OpenCode => None,
            // Pi documents no worktree flag — its own isolation story is
            // `docs/containerization.md`, which is the agent sandboxing the
            // dashboard deliberately stays out of. `supports_worktrees()` is
            // derived from this, so `Ctrl-g` hides itself with nothing else to
            // change.
            AgentControl::Pi => None,
            // No `--worktree` launch flag exists (the single `--worktree` string
            // in the binary is `git restore --worktree`). omp's `omp worktree`
            // subcommand only lists/clears worktrees the agent made itself, so
            // there is nothing to pass at launch. `supports_worktrees()` is
            // derived from this, so `Ctrl-g` hides itself.
            AgentControl::Omp => None,
            // `agy` has no worktree flag. Its isolation story is `--sandbox`,
            // which is the agent sandboxing the dashboard deliberately stays out
            // of, so there is nothing to spend a worktree request on and
            // `Ctrl-g` hides itself.
            AgentControl::Antigravity => None,
            AgentControl::Unknown => None,
        }
    }

    /// Whether this agent can launch into an isolated worktree. Derived from
    /// [`Self::worktree_args`] rather than matched separately, so the UI gate
    /// and the argv can never disagree about which agents support it.
    pub fn supports_worktrees(self) -> bool {
        self.worktree_args(None).is_some()
    }

    /// Whether a resume can **branch** rather than continuing the session in
    /// place — what `f` offers, and the thing `--fork-session` / Codex's `fork`
    /// subcommand do.
    ///
    /// Derived from [`Self::resume_args`] the same way [`Self::supports_worktrees`]
    /// derives from `worktree_args`, and for the same reason: a backend with no
    /// fork flag simply ignores the `fork` argument, which makes the two argvs
    /// equal and this `false` with no second match to keep in sync. A backend
    /// that later grows the flag flips this by editing one arm.
    pub fn supports_fork(self) -> bool {
        // Any id works — both calls use the same one, so only the flag differs.
        self.resume_args("id", true) != self.resume_args("id", false)
    }

    /// Everything a backend may structurally lack, in one query — the
    /// `AgentControl` counterpart to [`crate::terminal::Capabilities`], and the
    /// same bargain: a limit the UI has to gate on is a *field* here, not a
    /// special case at the call site.
    ///
    /// **Constants, not derivations, because the dashboard reads this while
    /// drawing a frame.** Every field could be computed from the method that
    /// owns it — `fork` from the two argvs, `approval_gate` from the generated
    /// hook config — but three of the four would then allocate per row per
    /// redraw, and [`crate::terminal::Capabilities`] set the precedent that this
    /// query does no work.
    ///
    /// What keeps a constant honest is that the derivation still runs, in
    /// `the_capability_matrix_agrees_with_the_implementations`: every field here
    /// is checked against the code that would have to change for it to be
    /// wrong. So a backend that loses its permission hook to an upstream rename,
    /// or grows a fork flag, fails that test rather than shipping a dashboard
    /// that promises the wrong thing. Declaring a capability is cheap; declaring
    /// one that isn't true is not possible.
    pub fn capabilities(self) -> AgentCapabilities {
        // Each backend's row in full, so the matrix reads down the page rather
        // than being assembled from four separate matches.
        let caps = |fork, worktrees, approval_gate, context_tokens| AgentCapabilities {
            fork,
            worktrees,
            approval_gate,
            context_tokens,
        };
        match self {
            AgentControl::Claude => caps(true, true, true, true),
            AgentControl::Codex => caps(true, false, true, true),
            // No context total: Reasonix persists none we can read, and its own
            // docs warn that its nearest number reads zero on a rebound session
            // — a limit of the agent, not a fold nobody has written.
            AgentControl::Reasonix => caps(true, false, true, false),
            // No fork flag documented, so `f` hides itself.
            AgentControl::Kimi => caps(false, false, true, true),
            // Context total is `signals.json`'s `contextTokensUsed` — the
            // in-memory billing ledgers still aren't serialized, but this
            // sidecar is, and it is what `/session-info` shows.
            AgentControl::Grok => caps(true, true, true, true),
            AgentControl::OpenCode => caps(true, false, true, true),
            // The one backend with no approval gate — see
            // [`AgentCapabilities::approval_gate`].
            AgentControl::Pi => caps(true, false, false, true),
            // The one capability row that inverts pi's: omp has a per-tool
            // approval gate (`tool_approval_requested` / `tool_approval_resolved`),
            // so `approval_gate` is `true`. Fork via `--fork`, no worktree flag,
            // and tokens/model come off the hook — the opencode row.
            AgentControl::Omp => caps(true, false, true, true),
            // Every field false, and every one of them the *agent's* limit
            // rather than an unfinished wiring: no argv forks a conversation, no
            // flag opens a worktree, no hook fires while Antigravity blocks on
            // its own approval prompt, and neither a payload nor anything on
            // disk carries a token total. `agents::antigravity` has the evidence
            // for each.
            AgentControl::Antigravity => caps(false, false, false, false),
            // Nothing may be claimed for a backend this build can't drive: it
            // launches nothing, writes no hook config and fills no column.
            AgentControl::Unknown => caps(false, false, false, false),
        }
    }

    /// Every [`HookEvent`] this backend actually subscribes to, read back out of
    /// the hook config it generates rather than declared a second time.
    ///
    /// This works uniformly because every backend renders
    /// [`HookEvent::as_kebab`] into what it writes: Claude and the four
    /// shell-hook backends as the last word of the `miao hook …` command, and
    /// opencode and Pi as a `[native, event]` row in the generated plugin or
    /// extension. The generated file *is* the subscription list, so reading it
    /// back cannot disagree with it — which is what lets
    /// [`Self::capabilities`] derive `approval_gate` instead of asserting it.
    ///
    /// Order follows [`HookEvent::ALL`], not the config.
    pub fn forwarded_events(self) -> Vec<HookEvent> {
        // Only Claude splices the socket in at all, and no event name can be
        // confused with this: it is a path, not a bare token.
        let mut config = self.hooks_settings_json("/dev/null");
        for extra in self.extra_hook_registrations() {
            config.push(' ');
            config.push_str(&extra);
        }
        HookEvent::ALL
            .iter()
            .copied()
            .filter(|e| mentions_event(&config, e.as_kebab()))
            .collect()
    }

    /// Hook commands a backend installs **somewhere other than** the file
    /// [`Self::hooks_settings_json`] returns, so [`Self::forwarded_events`] can
    /// see the whole subscription rather than the largest part of it.
    ///
    /// Grok is the only backend with a second site and the reason this exists:
    /// its approval signal also lands as a `[[ui.notifications.hooks]]` entry
    /// merged into the synthetic `config.toml` at launch, kept as a fallback
    /// for grok versions whose lifecycle `Notification` event is missing. The
    /// settings file now registers `permission_prompt` too, so this is
    /// belt-and-braces rather than the only site.
    ///
    /// Still the real installed command, not a second declaration of it: this
    /// returns what the merge writes.
    fn extra_hook_registrations(self) -> Vec<String> {
        match self {
            AgentControl::Grok => vec![grok::notification_hook_command()],
            AgentControl::Claude
            | AgentControl::Codex
            | AgentControl::Reasonix
            | AgentControl::Kimi
            | AgentControl::OpenCode
            | AgentControl::Pi
            | AgentControl::Antigravity
            | AgentControl::Omp
            | AgentControl::Unknown => vec![],
        }
    }

    // -- Dashboard-side: filesystem watching, transcript reading, naming --

    /// Filesystem paths whose changes should trigger a dashboard reload —
    /// session-name files, transcript directories, etc. Missing dirs are
    /// silently skipped by the caller; this just enumerates candidates.
    pub fn watch_paths(self) -> Vec<PathBuf> {
        match self {
            AgentControl::Claude => claude::watch_paths(),
            AgentControl::Codex => codex::title_watch_path().into_iter().collect(),
            // Nothing: the dashboard derives no Reasonix fact from disk — no
            // session-name manifest, no title store, no transcript read — so
            // there is no file whose change could make a row stale.
            AgentControl::Reasonix => vec![],
            // Nothing, and for the same reason as Reasonix: no manifest, no
            // title store (the title rides the hook payload), no transcript
            // read. There is no file whose change could make a Kimi row stale.
            AgentControl::Kimi => vec![],
            // Nothing *yet*, and the blocker has moved: `list_resumable` now
            // reads `sessions/<cwd-key>/<id>/summary.json`, so both the cwd-key
            // encoding (walk every key — Grok's own resolver does) and the JSON
            // spellings are settled. What is left is a decision, not a schema
            // question: `summary.json` holds the title, model and git head, and
            // watching `sessions/` would mean folding a whole JSON file on every
            // append rather than advancing a byte cursor. `miao-y5m.5` carries
            // it; when that lands, `sessions/` is what belongs here.
            AgentControl::Grok => vec![],
            // Nothing: every fact an opencode row carries rides the plugin's
            // events. Its sessions do sit on disk — JSON blobs under
            // `~/.local/share/opencode/storage/session/` — but nothing here
            // reads them, and `list_resumable` deliberately goes through
            // `opencode session list` rather than that internal, twice-migrated
            // schema. So there is no file whose change could make a row stale.
            AgentControl::OpenCode => vec![],
            // Nothing, and for the strongest reason of any backend: every fact
            // a Pi row carries — status, title, tokens, model — rides a hook
            // payload, and nothing of ours reads a Pi file at all. There is no
            // file whose change could make a Pi row stale.
            AgentControl::Pi => vec![],
            // Nothing, on the same footing: an Antigravity row's id, model and
            // status all ride hook payloads. Its conversation store *does*
            // change constantly, and holds nothing the dashboard reads.
            AgentControl::Antigravity => vec![],
            // Nothing, and for the strongest reason of any backend, the same as
            // pi's: every fact an omp row carries — status, title, tokens,
            // model — rides a hook payload, and nothing of ours reads an omp
            // file at all. There is no file whose change could make a row stale.
            AgentControl::Omp => vec![],
            AgentControl::Unknown => vec![],
        }
    }

    /// Paths outside the launcher's own `sessions/` dir whose changes must still
    /// wake a host's subscribers — an agent's out-of-band store that no session
    /// event touches, so a change there reaches a remote dashboard only if the
    /// server wakes and re-diffs on it.
    ///
    /// Deliberately *not* [`Self::watch_paths`], which answers a different
    /// question: what a **dashboard** must watch to keep its own derived reads
    /// fresh. The two agree for Codex today and disagree for Claude, and the
    /// disagreement is the point — Claude's `~/.claude/sessions` feeds the local
    /// session-name index, but every fact in it that a *remote* dashboard needs
    /// is folded onto the state file by the launcher first, so watching it
    /// server-side would only wake subscribers with nothing new to push.
    pub fn out_of_band_watch_paths(self) -> Vec<PathBuf> {
        match self {
            // Nothing: names arrive via `session_name`/`session_watch_path` and
            // stats via the transcript, both folded onto the state file by the
            // launcher — which is a `sessions/` write the host already watches.
            AgentControl::Claude => vec![],
            // `state_5.sqlite`'s WAL. A `/rename` or auto-title lands there alone
            // — no hook, rollout line, or state-file write — so this wake is the
            // only thing that gets a rename onto the wire.
            AgentControl::Codex => codex::title_watch_path().into_iter().collect(),
            // Nothing, and for a stronger reason than Claude's: every fact a
            // Reasonix row carries arrives over a hook and is written to the
            // state file, which the host already watches. There is no store we
            // read out of band, so a wake here could only ever re-diff rows that
            // hadn't changed. If a rename or a token count is ever read from the
            // session sidecars, that sidecar dir belongs here.
            AgentControl::Reasonix => vec![],
            // Nothing, and this is the arm `session_title` was added for: Kimi's
            // rename arrives on the *next hook payload*, which the launcher
            // folds onto the state file — a `sessions/` write the host already
            // watches. Codex needs an entry here only because its rename lands
            // in sqlite with no hook at all.
            AgentControl::Kimi => vec![],
            // Nothing: every fact a Grok row carries arrives over a hook and is
            // written to the state file, which the host already watches. A wake
            // here could only re-diff rows that hadn't changed.
            AgentControl::Grok => vec![],
            // Nothing, and now for Kimi's reason rather than for want of a
            // title: `session.updated` carries `info.title`, so a rename arrives
            // on a hook the launcher folds onto the state file — a `sessions/`
            // write the host already watches. Codex needs an entry here only
            // because its rename lands in sqlite with no hook at all.
            AgentControl::OpenCode => vec![],
            // Nothing: the title is the only fact that would want an entry
            // here, and it arrives on the *next hook payload* — a `sessions/`
            // write the host already watches. Codex needs one only because its
            // rename lands in sqlite with no hook at all.
            AgentControl::Pi => vec![],
            // Nothing: Antigravity has no session rename at all, so no fact of
            // a row's can change without a hook firing.
            AgentControl::Antigravity => vec![],
            // Nothing, for pi's reason: the title arrives on the *next hook
            // payload* — a `sessions/` write the host already watches. Codex
            // needs an entry here only because its rename lands in sqlite with
            // no hook at all.
            AgentControl::Omp => vec![],
            AgentControl::Unknown => vec![],
        }
    }

    /// Refresh per-pid name and session-id maps from the agent's on-disk
    /// session-name store. The cache lets repeated reloads skip files whose
    /// mtime is unchanged.
    pub fn read_session_index(self, cache: &mut SessionIndexCache) -> SessionIndex {
        match self {
            AgentControl::Claude => claude::read_session_index(cache),
            AgentControl::Codex => codex::read_session_index(cache),
            // No per-pid manifest to scan: a Reasonix session's id arrives on
            // every hook payload, which is what this index is a fallback for.
            AgentControl::Reasonix => SessionIndex::default(),
            // No per-pid manifest: a Kimi session's id (and name) arrive on
            // every hook payload, which is what this index is a fallback for.
            AgentControl::Kimi => SessionIndex::default(),
            // Same as Reasonix: no per-pid manifest exists, and a Grok session's
            // id arrives on every hook payload — which is what this index is a
            // fallback for.
            AgentControl::Grok => SessionIndex::default(),
            // No per-pid manifest: an opencode session's id arrives on
            // `session.created` and on every direct hook (`agents::opencode`),
            // which is what this index is a fallback for.
            AgentControl::OpenCode => SessionIndex::default(),
            // No per-pid manifest: a Pi session's id (and name) arrive on every
            // hook payload, which is what this index is a fallback for.
            AgentControl::Pi => SessionIndex::default(),
            // No per-pid manifest. An Antigravity session's id arrives on its
            // first hook — which is the *first prompt*, not the launch, since
            // nothing fires at startup (`agents::antigravity`).
            AgentControl::Antigravity => SessionIndex::default(),
            // No per-pid manifest, for pi's reason: an omp session's id (and
            // name) arrive on every hook payload, which is what this index is a
            // fallback for.
            AgentControl::Omp => SessionIndex::default(),
            AgentControl::Unknown => SessionIndex::default(),
        }
    }

    /// Transcript-derived per-session facts in one pass: context-token total,
    /// model id, custom title (`/rename`), and first-prompt auto-title. `prior`
    /// is the previously folded value for this session, if any: Claude folds only
    /// the transcript bytes appended since `prior`'s cursor (so an active session
    /// isn't rescanned end-to-end), while Codex recomputes stats from a bounded
    /// tail but reuses `prior`'s first prompt once found. The launcher folds this
    /// and stamps the fields onto the session's state file, so the dashboard never
    /// reads a transcript itself. Fields are None before the first relevant entry
    /// (no assistant turn → no `context_tokens`/`model`; no rename → no `name`).
    pub fn read_transcript_stats(
        self,
        transcript: &Path,
        prior: Option<&TranscriptStats>,
    ) -> TranscriptStats {
        match self {
            AgentControl::Claude => claude::read_transcript_stats_incremental(transcript, prior),
            AgentControl::Codex => codex::read_transcript_stats(transcript, prior),
            // Unreachable rather than unimplemented: this runs only on a path a
            // hook payload supplied, and Reasonix's carries none. The sidecar
            // schema that once blocked this is settled — `list_resumable` reads
            // `<session>.jsonl.meta` for the title and cwd — but the sidecar
            // holds no token total, and `AgentCapabilities::context_tokens` is
            // `false` for this backend because the agent persists none: its own
            // docs warn that the nearest number reads zero on a rebound session.
            // So there is nothing here left to fold, not merely nothing folded.
            AgentControl::Reasonix => TranscriptStats::default(),
            // Kimi's payload names no transcript, so `agents::kimi` resolves
            // one from the session id and puts it on the message — which is
            // what makes this arm reachable at all. `prior` is unused: the
            // cursor is Claude's, and this is a whole-file read of a log of
            // small records.
            AgentControl::Kimi => kimi::read_transcript_stats(transcript),
            // Reachable because the envelope names `transcriptPath`; the fold
            // reads sibling `signals.json` / `summary.json` rather than the ACP
            // stream. `prior` is unused: both files are small whole-JSON
            // documents, not an appended log a byte cursor could follow.
            AgentControl::Grok => grok::read_transcript_stats(transcript),
            // Unreachable **and settled**, which is the difference between this
            // arm and Reasonix's: opencode reports its tokens and model on the
            // hook itself. `message.updated` carries an `AssistantMessage` whose
            // `tokens` and `modelID` the plugin forwards once the message
            // completes, so `common::adopt_session_facts` stamps both onto the
            // row and this fold has nothing left to do. Nor could it do it — the
            // sessions are per-message JSON blobs under `storage/`, not an
            // appended file a byte cursor could follow — and `adopt_session_facts`
            // is explicit that one fact gets one source.
            AgentControl::OpenCode => TranscriptStats::default(),
            // Unreachable, and the only one of these arms that is a *decision*
            // rather than a missing schema. Pi's JSONL is fully documented and
            // its path is exported as `PI_SESSION_FILE`, so this fold could be
            // written — but the file is a **tree**, not a log (`id`/`parentId`
            // with an active leaf), so a correct fold has to walk back from the
            // active leaf rather than tail the last append. The tokens and model
            // come off the hook instead, and `agents::pi` supplies no transcript
            // path, which is what makes this consistent rather than merely
            // unimplemented: one fact, one source.
            AgentControl::Pi => TranscriptStats::default(),
            // Nothing to fold: Antigravity's transcript records steps, not
            // usage, and its per-conversation store is protobuf blobs in SQLite
            // with no published schema. `agents::antigravity` supplies no
            // transcript path either, so this is consistent rather than merely
            // unimplemented.
            AgentControl::Antigravity => TranscriptStats::default(),
            // Unreachable, for pi's decision rather than his schema: omp's
            // session file is a tree, not a log (the same `id`/`parentId` shape
            // pi's docs describe), so a correct fold would walk back from the
            // active leaf. The tokens and model come off the hook instead, and
            // `agents::omp` supplies no transcript path — one fact, one source.
            AgentControl::Omp => TranscriptStats::default(),
            AgentControl::Unknown => TranscriptStats::default(),
        }
    }

    /// Resumable sessions across all of this agent's transcripts. Most-recent
    /// first, capped at `limit`. The returned candidates carry their source
    /// agent so a future picker can mix backends in one list.
    pub fn list_resumable(self, limit: usize) -> Result<Vec<ResumeCandidate>> {
        match self {
            AgentControl::Claude => claude::list_resumable(limit),
            AgentControl::Codex => codex::list_resumable(limit),
            // Read off disk rather than through Reasonix's own
            // `reasonix session list --json`, which is documented as *redacted*:
            // it deliberately exposes no transcript, label, path or host
            // content — none of the cwd, title or first prompt a candidate is
            // made of. The `.jsonl.meta` sidecars carry all three.
            AgentControl::Reasonix => reasonix::list_resumable(limit),
            AgentControl::Kimi => kimi::list_resumable(limit),
            AgentControl::Grok => grok::list_resumable(limit),
            AgentControl::OpenCode => opencode::list_resumable(limit),
            // Empty for now. Pi's sessions are plain JSONL under
            // `~/.pi/agent/sessions/`, grouped by working directory, so this is
            // reachable — but a candidate needs a cwd, a first prompt and an id,
            // and reading them means committing to the same active-branch walk
            // `read_transcript_stats` declines. `pi -r` opens Pi's own session
            // picker in the meantime, which is the usable answer until this and
            // that fold land together.
            AgentControl::Pi => Ok(vec![]),
            AgentControl::Antigravity => antigravity::list_resumable(limit),
            // Empty, and unlike pi's it is not blocked on an active-branch walk:
            // `omp -r` opens omp's own picker with a current-folder / all-projects
            // scope toggle, and sessions live at
            // `~/.omp/agent/sessions/<sanitized-cwd>/<timestamp>_<uuid>.jsonl`
            // (overridable by `PI_CODING_AGENT_DIR`, and per-profile under
            // `~/.omp/profiles/<name>/agent`). Reading them means committing to
            // the same `parentId` walk `read_transcript_stats` declines, so both
            // stay unimplemented together.
            AgentControl::Omp => Ok(vec![]),
            AgentControl::Unknown => Ok(vec![]),
        }
    }

    // -- Launcher-side: process launch, hooks, transcript signals --

    /// Build the subprocess command that runs this agent in `cwd` with hook
    /// callbacks pointing at `sock_path`. The launcher writes any per-session
    /// config files (Claude's `--settings` payload, Codex's owned profile, etc.)
    /// before spawning.
    ///
    /// `shim_dir` is the clipboard shim farm to prepend to the agent's `PATH`, and
    /// it is `Some` only for a pooled session — see
    /// [`crate::cli::ClipboardShims`]. An explicit parameter rather than something
    /// each backend decides for itself, so the two cannot disagree about when a
    /// session is shimmed.
    pub fn build_launch_command(
        self,
        cwd: &str,
        sock_path: &Path,
        settings_path: &Path,
        extra_args: &[String],
        shim_dir: Option<&Path>,
    ) -> Result<Command> {
        match self {
            AgentControl::Claude => {
                claude::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::Codex => {
                codex::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::Reasonix => {
                reasonix::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::Kimi => {
                kimi::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::Grok => {
                grok::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::OpenCode => {
                opencode::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::Pi => {
                pi::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::Omp => {
                omp::build_launch_command(cwd, sock_path, settings_path, extra_args, shim_dir)
            }
            AgentControl::Antigravity => antigravity::build_launch_command(
                cwd,
                sock_path,
                settings_path,
                extra_args,
                shim_dir,
            ),
            // One of the two places `Unknown` must be loud: there is no argv to
            // guess, and guessing Claude's would run the wrong agent in the
            // user's cwd. The other is `LocalBackend::open_session`, which
            // builds an argv without coming through here.
            AgentControl::Unknown => Err(anyhow!(UNKNOWN_AGENT_REFUSAL)),
        }
    }

    /// JSON contents of the per-session hook-settings file the launcher
    /// drops on disk before spawning the agent. The file location is
    /// agent-specific and chosen by `build_launch_command`.
    pub fn hooks_settings_json(self, sock_path: &str) -> String {
        match self {
            AgentControl::Claude => claude::build_hooks_settings(sock_path),
            AgentControl::Codex => codex::build_hooks_settings(sock_path),
            AgentControl::Reasonix => reasonix::build_hooks_settings(sock_path),
            // TOML, not JSON, despite the method name and the `-settings.json`
            // file the launcher writes it to: Kimi's hooks live in its
            // `config.toml`. The path is generic transport and the contents are
            // opaque to the launcher, so each backend puts its own format
            // through it.
            AgentControl::Kimi => kimi::build_hooks_settings(sock_path),
            AgentControl::Grok => grok::build_hooks_settings(sock_path),
            // **JavaScript**, not JSON — the furthest this method's name has
            // been stretched, and the seam holds: opencode has no shell-command
            // hooks at all, so its event surface is a plugin *module*, which
            // `build_launch_command` drops into a synthetic `plugins/` dir. The
            // path is generic transport and the contents are opaque to the
            // launcher, which is what lets each backend put its own format
            // through it.
            AgentControl::OpenCode => opencode::build_hooks_settings(sock_path),
            // **TypeScript**, not JSON — Pi has no shell-command hooks at all,
            // so what the launcher writes here is the extension module `pi -e`
            // loads. Same argument as Kimi's TOML: the path is generic
            // transport, the contents are opaque to the launcher, and each
            // backend puts its own format through it.
            AgentControl::Pi => pi::build_hooks_settings(sock_path),
            // **TypeScript**, not JSON — omp has no shell-command hooks at all,
            // so what the launcher writes here is the extension module `omp -e`
            // loads. Same argument as Kimi's TOML and pi's TypeScript: the path
            // is generic transport, the contents are opaque to the launcher, and
            // each backend puts its own format through it.
            AgentControl::Omp => omp::build_hooks_settings(sock_path),
            AgentControl::Antigravity => antigravity::build_hooks_settings(sock_path),
            AgentControl::Unknown => String::new(),
        }
    }

    /// Apply a hook event to the launcher state. Encapsulates per-agent
    /// status mapping (`PreToolUse` → `Active`, `PreCompact` → `Compacting`,
    /// etc.).
    pub async fn dispatch_hook(self, state: &mut LauncherState, msg: HookMessage) {
        match self {
            AgentControl::Claude => claude::dispatch_hook(state, msg).await,
            AgentControl::Codex => codex::dispatch_hook(state, msg).await,
            AgentControl::Reasonix => reasonix::dispatch_hook(state, msg).await,
            AgentControl::Kimi => kimi::dispatch_hook(state, msg).await,
            AgentControl::Grok => grok::dispatch_hook(state, msg).await,
            AgentControl::OpenCode => opencode::dispatch_hook(state, msg).await,
            AgentControl::Pi => pi::dispatch_hook(state, msg).await,
            AgentControl::Antigravity => antigravity::dispatch_hook(state, msg).await,
            AgentControl::Omp => omp::dispatch_hook(state, msg).await,
            AgentControl::Unknown => {}
        }
    }

    /// Parse the agent's stdin JSON hook payload into a normalized
    /// `HookMessage`. Used by the `miao hook` subcommand.
    pub fn parse_hook_payload(self, event: HookEvent, stdin: &str) -> Result<HookMessage> {
        match self {
            AgentControl::Claude => claude::parse_hook_payload(event, stdin),
            AgentControl::Codex => codex::parse_hook_payload(event, stdin),
            AgentControl::Reasonix => reasonix::parse_hook_payload(event, stdin),
            AgentControl::Kimi => kimi::parse_hook_payload(event, stdin),
            AgentControl::Grok => grok::parse_hook_payload(event, stdin),
            AgentControl::OpenCode => opencode::parse_hook_payload(event, stdin),
            AgentControl::Pi => pi::parse_hook_payload(event, stdin),
            AgentControl::Antigravity => antigravity::parse_hook_payload(event, stdin),
            AgentControl::Omp => omp::parse_hook_payload(event, stdin),
            AgentControl::Unknown => Err(anyhow!(
                "unknown agent backend (this hook came from a newer \
                 captain-miao); upgrade captain-miao to handle it"
            )),
        }
    }

    /// Whether a session that has just ended a turn will open its **own** next
    /// one, with no hook to announce it — so its `Stop` is a turn boundary
    /// rather than rest, and parking the row at Idle would be wrong.
    ///
    /// Asked at the moment the launcher would park a row, which is what keeps
    /// this event-driven: nothing is watched, nothing is timed, the one backend
    /// that can answer `true` reads its own store (see
    /// [`codex::thread_self_continues`]) and every other one answers from the
    /// shape of its turn model.
    pub fn self_continues(self, session_id: &str) -> bool {
        match self {
            AgentControl::Codex => codex::thread_self_continues(session_id),
            // Everything else: a turn ends at the user's next move. None of
            // these backends carries a standing objective that re-drives the
            // thread after its turn ends, so there is no store to ask and no
            // hookless turn to miss — a Stop here is rest, full stop. A backend
            // that grows one lands in this match and has to say so.
            AgentControl::Claude
            | AgentControl::Reasonix
            | AgentControl::Kimi
            | AgentControl::Grok
            | AgentControl::OpenCode
            | AgentControl::Pi
            | AgentControl::Antigravity
            | AgentControl::Omp
            | AgentControl::Unknown => false,
        }
    }

    /// Scan new bytes of the transcript starting at `offset` for signals the
    /// launcher cares about (interrupt detection). Backends that don't expose
    /// such signals return an empty scan.
    pub fn scan_transcript_signals(self, path: &Path, offset: u64) -> TranscriptScan {
        match self {
            AgentControl::Claude => claude::scan_transcript_signals(path, offset),
            AgentControl::Codex => codex::scan_transcript_signals(path, offset),
            // Empty because Reasonix needs no sentinel, not because none was
            // found: an interrupt is a payload field (`isInterrupt`) on a real
            // hook, so the case that forced Codex to read its rollout — Esc
            // firing nothing — cannot arise here.
            AgentControl::Reasonix => TranscriptScan::default(),
            // Empty for the strongest reason of any backend: `Interrupt` is a
            // first-class Kimi hook, so the exact case that forced Codex to read
            // its rollout — Esc firing nothing — cannot arise. Nothing is
            // missing here, and nothing would be gained by adding it.
            AgentControl::Kimi => TranscriptScan::default(),
            // Empty for Kimi's reason: `StopCancelled` is a first-class observe
            // hook (1.0.4), so the case that forced Codex to read its rollout —
            // Esc firing nothing — cannot arise. Nothing would be gained by
            // scanning `updates.jsonl` for a sentinel as well.
            AgentControl::Grok => TranscriptScan::default(),
            // Empty because there is no transcript to scan: opencode's sessions
            // are per-message JSON blobs under `storage/`, and no hook payload
            // names a path regardless. Whether a sentinel is even *wanted* is
            // still unsettled — the plugin takes its turn-end from
            // `session.status`'s `idle` edge, and nothing source-read says
            // whether an Esc-interrupted turn reaches it. If it doesn't, this
            // backend inherits Grok's stranded-`Active` row with nothing of the
            // agent's to read instead; that is on the probe list in
            // `agents::opencode`.
            AgentControl::OpenCode => TranscriptScan::default(),
            // Empty because Pi needs no sentinel, not because none was found —
            // the same standing as Reasonix's and Kimi's, reached differently.
            // Neither carries an interrupt *flag*; Pi instead has a turn-end
            // event that fires once the run will not continue at all
            // (`agent_settled`), and a cancelled run is exactly that. So the
            // case that forced Codex to read its rollout cannot arise, and no
            // transcript of Pi's is read for any purpose (`agents::pi` names
            // none).
            AgentControl::Pi => TranscriptScan::default(),
            // Empty for the reason Codex's is not: the one signal worth
            // reading a transcript for is an interrupt, and Antigravity's
            // records none — a cancelled turn is an ordinary `DONE` step. So
            // `agents::antigravity` supplies no transcript path, and there is
            // no file here to scan.
            AgentControl::Antigravity => TranscriptScan::default(),
            // Empty, for pi's reason reached via `agent_end`: omp's turn-end
            // event fires on an abort too (`stopReason === "aborted"` calls the
            // same emitter), so the case that forced Codex to read its rollout —
            // Esc firing nothing — cannot arise. No transcript of omp's is read
            // for any purpose (`agents::omp` names none).
            AgentControl::Omp => TranscriptScan::default(),
            AgentControl::Unknown => TranscriptScan::default(),
        }
    }

    /// The agent's own report of what process `agent_pid` is doing, read from
    /// its status file. Authoritative on the coarse working/idle/background-shell
    /// axis, so the launcher can settle a hook-derived `Active` back to rest when
    /// a turn ends with no hook (an interrupt fires no `Stop`). `None` when it
    /// can't be determined (caller leaves the status unchanged). Backends without
    /// a status file return `None`.
    pub fn agent_activity(self, agent_pid: u32) -> Option<AgentActivity> {
        match self {
            AgentControl::Claude => claude::session_activity(agent_pid),
            AgentControl::Codex => codex::session_activity(agent_pid),
            // No status file, so no second opinion on the working/idle axis —
            // Reasonix's transitions ride hooks alone, interrupts included.
            AgentControl::Reasonix => None,
            // No status file, and none needed: the interrupt that this exists to
            // catch for Claude arrives as a hook. Kimi's transitions ride hooks
            // alone, with no second opinion to reconcile against.
            AgentControl::Kimi => None,
            // No status file, so no second opinion on the working/idle axis. It
            // is the interrupt case above that would have wanted one.
            AgentControl::Grok => None,
            // No status file, and none wanted: `session.status` is the second
            // opinion this would have been, and it drives the row *directly* the
            // way Claude's session file does. The plugin forwards only its
            // `idle` edge — `busy` is coarser than our vocabulary and would knock
            // a row out of `WaitingForApproval` — so the signal arrives as a
            // `Stop`, not as an activity read.
            AgentControl::OpenCode => None,
            // No status file, and none needed: the interrupt this exists to
            // catch for Claude is covered by `agent_settled`, which fires when
            // Pi will not continue running — a cancelled run included.
            AgentControl::Pi => None,
            // No status file, and the gap one would cover is real: an
            // interrupted Antigravity turn fires no hook, so the row reads
            // `Active` until the next prompt. Nothing in the process tree
            // distinguishes that from a model call in flight, so nothing is
            // claimed here rather than something being guessed.
            AgentControl::Antigravity => None,
            // No status file, and none needed: the interrupt this exists to
            // catch for Claude is covered by `agent_end`, which fires on an
            // aborted run too — pi's reason, reached via omp's emitter.
            AgentControl::Omp => None,
            AgentControl::Unknown => None,
        }
    }

    /// The *user-set* display name from the agent's own session file — Claude
    /// writes both its auto-derived slug and the user's `/rename` to
    /// `~/.claude/sessions/<pid>.json`; only the rename is surfaced (the slug is
    /// dropped so the first prompt wins). The launcher folds this onto
    /// `LauncherState.name` so it reaches the dashboard (local *and* remote) over
    /// the state file, with no transcript read. `None` for backends without such a
    /// file (Codex — its sqlite title is overlaid per-host by
    /// [`crate::backend::LocalBackend`]).
    pub fn session_name(self, agent_pid: u32) -> Option<String> {
        match self {
            AgentControl::Claude => claude::read_session_name(agent_pid),
            AgentControl::Codex => None,
            // Reasonix titles exist and are now read — `list_resumable` takes
            // `custom_title` off the `.jsonl.meta` sidecar — but they are keyed
            // by session id, not by pid, so there is no per-pid file for *this*
            // method to open. The picker is where they surface; a running row
            // takes its name from the first prompt like every other backend
            // without a rename hook.
            AgentControl::Reasonix => None,
            // `None` because there is nothing left to read, not because Kimi has
            // no name: `session_title` rides every hook payload and is already
            // on `LauncherState.name` by the time this would be asked. That is
            // what makes this the one backend needing neither a session-file
            // fold (Claude) nor a sqlite overlay (Codex).
            AgentControl::Kimi => None,
            // Grok titles its own sessions (`summary.json`'s `session_summary`),
            // and `list_resumable` reads them — but keyed by session id, not by
            // pid, so there is no per-pid file for *this* method to open. Same
            // standing as Reasonix: the title surfaces in the picker, and a
            // running row falls back to the first prompt.
            AgentControl::Grok => None,
            // `None` because there is nothing left to read, not because
            // opencode has no name: `session.updated` carries `info.title`, the
            // plugin forwards it, and it is already on `LauncherState.name` by
            // the time this would be asked. The Kimi standing, reached from a
            // bus event rather than a documented hook field.
            AgentControl::OpenCode => None,
            // `None` because there is nothing left to read, not because Pi has
            // no name: `pi.getSessionName()` rides every hook payload as
            // `session_title` and is already on `LauncherState.name` by the time
            // this would be asked — the Kimi standing, reached from an
            // extension call rather than a documented payload field.
            AgentControl::Pi => None,
            // Antigravity has no session title of any kind — no rename command,
            // nothing on a payload, nothing in the store. The resume picker
            // shows a session's opening request instead.
            AgentControl::Antigravity => None,
            // `None` for pi's reason: `pi.getSessionName()` rides every hook
            // payload as `session_title` and is already on `LauncherState.name`
            // by the time this would be asked — the Kimi standing, reached from
            // the same extension call pi uses.
            AgentControl::Omp => None,
            AgentControl::Unknown => None,
        }
    }

    /// File whose changes the launcher should watch to learn about
    /// working↔idle↔background-shell transitions (these fire no hook). For Claude
    /// this is its session-status file; `None` for backends without one.
    pub fn session_watch_path(self, agent_pid: u32) -> Option<PathBuf> {
        match self {
            AgentControl::Claude => claude::session_file_path(agent_pid),
            AgentControl::Codex => None,
            AgentControl::Reasonix => None,
            // No status file to watch — see `agent_activity`.
            AgentControl::Kimi => None,
            AgentControl::Grok => None,
            AgentControl::OpenCode => None,
            // No status file to watch — see `agent_activity`.
            AgentControl::Pi => None,
            AgentControl::Antigravity => None,
            // No status file to watch — see `agent_activity`.
            AgentControl::Omp => None,
            AgentControl::Unknown => None,
        }
    }

    /// `Some(interval)` when the launcher's transcript watch must be a
    /// stat-polling one (`launcher::start_stat_poll`) rather than the
    /// platform's event-driven watcher, because the agent's writer defeats the
    /// platform events.
    ///
    /// Codex opens its rollout once and appends through that fd for the whole
    /// session, and **macOS FSEvents reports nothing for writes through a
    /// long-held fd until the file is closed** (measured: 12 flushed appends
    /// over 36s produced 0 events — on both a file-level and a directory-level
    /// watch, with or without fsync; the close produced 1). An event-driven
    /// watch therefore never wakes the launcher during a Codex session — no
    /// context tokens, no first-prompt fold, and an Esc-interrupt
    /// (`turn_aborted`, which fires **no hook** — verified against the codex
    /// source at 0.142.3: an aborted turn returns before `run_turn_stop_hooks`
    /// and the `notify` program) leaves the row Active forever. A stat poll
    /// sees each append immediately (`write(2)` updates size/mtime at write
    /// time; only the FSEvents notification waits for close). Linux inotify
    /// fires per write, so it stays event-driven there. Claude
    /// opens/writes/closes per line, so FSEvents works and it stays
    /// event-driven everywhere. The poll runs only while the session is off
    /// Idle — see the lifecycle gate in `launcher::process_hooks`.
    ///
    /// Returning `Some` also opts the agent into the launcher's hook-arm
    /// pre-dispatch rescan, which assumes the agent writes its transcript
    /// lines *before* firing the matching hook (true of Codex: `token_count`
    /// lands ~20ms ahead of `Stop`). An agent that wrote them after would
    /// merely make that read a no-op — the next poll tick still catches the
    /// bytes — so the assumption is a latency optimization, not a correctness
    /// requirement.
    pub fn transcript_poll_interval(self) -> Option<Duration> {
        match self {
            AgentControl::Claude => None,
            AgentControl::Codex if cfg!(target_os = "macos") => Some(Duration::from_secs(2)),
            AgentControl::Codex => None,
            // Moot: no hook payload names a transcript, so no watch of either
            // kind is ever started for a Reasonix session.
            AgentControl::Reasonix => None,
            // **Unverified, and set anyway.** Kimi appends
            // `agents/*/wire.jsonl` for the life of a session, which is the
            // long-held-fd shape that defeats macOS FSEvents entirely — so this
            // matches Codex. And it is live, not precautionary: `agents::kimi`
            // resolves the wire log from the session id and names it on every
            // payload, so without this a Kimi session on macOS would inherit an
            // event-driven watch FSEvents cannot deliver. This machine is Linux
            // and could not test it; no macOS behaviour is being claimed, only
            // the same defence Codex needed.
            AgentControl::Kimi if cfg!(target_os = "macos") => Some(Duration::from_secs(2)),
            AgentControl::Kimi => None,
            // Moot for the same reason today — `agents::grok` supplies no
            // transcript path. It will stop being moot the day it does: Grok's
            // `updates.jsonl` is a long-lived append stream, which is exactly the
            // shape that defeats macOS FSEvents, so whoever wires the fold should
            // arrive back here before assuming an event-driven watch works.
            AgentControl::Grok => None,
            // Moot, and structurally so rather than merely for now: opencode
            // keeps each message as its own JSON blob under `storage/`, so there
            // is no appended transcript for either kind of watch to follow,
            // whatever a payload might one day name.
            AgentControl::OpenCode => None,
            // Moot, and permanently so rather than pending like the other three:
            // `agents::pi` supplies no transcript path *by decision*, not for
            // want of one, so the launcher starts no watch of either kind for a
            // Pi session and there is no poll for this to configure.
            AgentControl::Pi => None,
            // Moot for the same reason, and by the same decision: no transcript
            // path reaches the launcher, so neither watch starts.
            AgentControl::Antigravity => None,
            // Moot, and by the same decision as pi's: `agents::omp` supplies no
            // transcript path, so the launcher starts no watch of either kind
            // for an omp session and there is no poll for this to configure.
            AgentControl::Omp => None,
            AgentControl::Unknown => None,
        }
    }

    /// The agent's currently-running `run_in_background` shells, read from the
    /// **live process tree** (see `claude::bg_shells`) and each classified by
    /// *what* it runs — the launcher's basis for refining a `BackgroundActive`
    /// row into `ReviewPending` (all review-watches), `BackgroundServer` (all
    /// long-running services), or a busy transient task.
    ///
    /// `None` means the tree could not be read, so nothing may be concluded; an
    /// empty `Some` is the positive fact that nothing is running, which is what
    /// lets the launcher retire a stale background status. Always `None` for
    /// Codex, which has no `run_in_background` concept — and therefore never
    /// reaches any of the states that reading would refine.
    pub fn bg_shells(self, agent_pid: u32) -> Option<Vec<BgShell>> {
        match self {
            AgentControl::Claude => claude::bg_shells(agent_pid),
            AgentControl::Codex => None,
            // Reasonix *does* have real background tasks — and deliberately
            // fires no `SubagentStop` for them — but the only view of them is a
            // subprocess (`reasonix task list --json`) over a cache the docs call
            // a projection, not a process tree we can walk. Until that is worth a
            // spawn per refresh, a `Stop` while a background task runs means "the
            // foreground turn ended" and the row reads Idle.
            AgentControl::Reasonix => None,
            // Kimi has subagents (`SubagentStart` / `SubagentStop`) and tasks
            // (`TaskStarted`), but neither is a `run_in_background` shell in a
            // process tree we can walk, and no documented payload enumerates
            // them the way Grok's `Stop` does. So a `Stop` while something else
            // is still running means "the foreground turn ended" and the row
            // reads Idle — never `BackgroundServer` or `ReviewPending`.
            AgentControl::Kimi => None,
            // `None` **for now, and the data is already in hand** — the one arm
            // here that is deferred rather than absent. Grok reports its live
            // background work on the `Stop` payload itself (`backgroundTasks`,
            // each with an `id`, a `type` of `shell` / `monitor` / `subagent`, a
            // status and its command text, plus `sessionCrons`), which is
            // strictly better than Claude's process-tree walk: it comes from the
            // agent that owns the tasks, at the moment it decides the turn is
            // over, and a `monitor` is at-rest by construction rather than by
            // command-text heuristic. Routing it here needs a new
            // `LauncherState` field to carry the list from the launcher's hook
            // arm, which is seam work and belongs in its own commit — so a `Stop`
            // while a background task runs currently reads as `Idle`.
            AgentControl::Grok => None,
            // No background-shell concept in anything design §9 names, so a
            // `session.idle` while something else is still running means "the
            // foreground turn ended" and the row reads `Idle` — never
            // `BackgroundServer` or `ReviewPending`.
            AgentControl::OpenCode => None,
            // Pi has no background-shell concept in its event surface at all —
            // no `run_in_background` tool, no background-task list on any
            // payload — so a `Stop` is simply the end of the work, and the row
            // never reaches `BackgroundActive`, `BackgroundServer` or
            // `ReviewPending`. An absence, with nothing to wire up later.
            AgentControl::Pi => None,
            // Antigravity *does* background long commands — every `run_command`
            // carries a `WaitMsBeforeAsync` — and `Stop` even reports whether
            // they have all finished (`fullyIdle`). What is missing is the list:
            // nothing enumerates the running ones, so there is no shell to name
            // and no tier to put the row in. Unlike Pi's, this one has something
            // to wire up if that list ever appears.
            AgentControl::Antigravity => None,
            // `None` for a sharper reason than pi's: omp *does* run background
            // work (async bash jobs, `task` spawns) and `session_stop` is even
            // deferred until they are idle — but nothing enumerates them on any
            // payload we receive, so there is no shell to name and no tier to
            // put the row in.
            AgentControl::Omp => None,
            AgentControl::Unknown => None,
        }
    }
}

/// What a backend structurally cannot do, queried in one place via
/// [`AgentControl::capabilities`]. Constants or derivations per backend — no IO.
///
/// **Only structural limits belong here.** A field means *the agent has no
/// mechanism for this at all*, never *we haven't wired it up yet*: the
/// dashboard's answer to a missing capability is to stop offering the thing, and
/// doing that to work that merely hasn't been done buries it as impossible.
/// Pi's absent resume-picker entries are an unfinished fold against data that
/// exists, so they are not a field here — they are a tracker item. Pi's missing
/// approval gate is the opposite: `security.md` says the agent runs shell
/// commands with its own permissions and has no per-tool prompt, so there is
/// nothing to reflect however much code we write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// A resume can **branch** rather than continue the session in place, so `f`
    /// has something to offer. Derived from [`AgentControl::resume_args`] —
    /// Kimi documents no fork flag, so its two argvs come out equal.
    pub fork: bool,
    /// The agent can launch itself into an isolated git worktree, so `Ctrl-g` in
    /// the workdir picker has something to arm. Derived from
    /// [`AgentControl::worktree_args`]; Claude and Grok are the two that can.
    pub worktrees: bool,
    /// The agent has a per-tool approval prompt it tells us about, so a row can
    /// reach [`crate::state::SessionStatus::WaitingForApproval`] and `s` can
    /// stop on it. Derived from [`AgentControl::forwarded_events`].
    ///
    /// **Pi is the one backend that answers `false`**, and it is not for want of
    /// a hook to subscribe to: `security.md` states it ships no sandbox and runs
    /// shell commands with the pi process's own permissions, and its `--approve`
    /// / trust machinery guards *loading settings and extensions*, not tool
    /// calls. So a blocked Pi session does not exist to be detected, and the
    /// only thing the dashboard owes the user is not to imply it looked.
    pub approval_gate: bool,
    /// A context-token total reaches the row at all, so an empty Context field
    /// means "not yet" rather than "not ever". Reasonix answers `false`;
    /// see [`AgentControl::capabilities`] for why that is the agent's limit
    /// rather than ours.
    pub context_tokens: bool,
}

/// Whether `config` names `kebab` as a **whole** event token.
///
/// Every generated hook config renders an event as the last word of a `miao hook
/// …` command or as a bare JSON/JS string, so the name always *opens* on a space
/// or a quote. What may follow it is open-ended: a closing quote, the end of the
/// input (where an [`AgentControl::extra_hook_registrations`] command stops), or
/// more shell — Antigravity's commands continue `>/dev/null; echo …` because that
/// agent requires JSON on a hook's stdout. So the trailing side admits anything
/// that could not be part of the name itself.
///
/// The boundary check is load-bearing, not decoration: `stop` is a prefix of
/// `stop-failure`, `elicitation` of `elicitation-result` and `post-tool-use` of
/// `post-tool-use-failure`, so a plain `contains` would report three
/// subscriptions no backend ever registered.
fn mentions_event(config: &str, kebab: &str) -> bool {
    config.match_indices(kebab).any(|(at, _)| {
        let before = config[..at].chars().next_back();
        let after = config[at + kebab.len()..].chars().next();
        // Opening side stays exact rather than "not a name char": an agent
        // binary living under a path segment that happens to *be* an event name
        // would otherwise register every backend for it.
        matches!(before, Some(' ' | '"'))
            && after.is_none_or(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    })
}

/// One of an agent's running `run_in_background` shells, reduced to what the
/// launcher's background-status refinement needs: a normalized command `key`
/// (the learning identity, stable across sessions) and the `kind` a *static*
/// classifier assigned it. "Static" means from the command text alone — the
/// launcher then overlays the learned store and per-command durations on top of
/// an `Other` to decide busy-vs-at-rest (see `launcher::classify_and_learn`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgShell {
    /// The normalized command (the agent's actual `run_in_background` command,
    /// extracted from the Bash-tool wrapper) — the key both the learning store
    /// and the duration tracker use to recognize "the same command" again.
    pub key: String,
    /// What the command text alone says this is.
    pub kind: BgSeedKind,
}

/// A background shell's classification from its command text alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgSeedKind {
    /// An r3 review-watch (`r3 watch <review-id>`) — the agent is blocked on a
    /// human review → `ReviewPending`.
    ReviewWatch,
    /// A recognized long-running service (dev server / watcher) per the seed
    /// heuristic → at-rest `BackgroundServer`, no waiting to learn it.
    LongRunning,
    /// Anything else — a finite build/test/step by default (busy), unless the
    /// learned store or a duration threshold later reclassifies it as
    /// long-running.
    Other,
}

/// The agent's own report of what it's doing, read from its status file
/// (Claude's `~/.claude/sessions/<pid>.json`). Coarser than `SessionStatus` — it
/// only distinguishes "still working" from the two at-rest shapes — and is used
/// to reconcile the working/idle/background-shell axis when a hook is missed
/// (e.g. an interrupt fires no `Stop`). The launcher only ever *demotes* a busy
/// hook status toward rest on this signal, never promotes — hook events own the
/// rest→active direction.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum AgentActivity {
    /// Mid-turn: the model is running or a foreground tool is executing.
    Working,
    /// The turn has ended and nothing it spawned is still running.
    Idle,
    /// The turn has ended but a `run_in_background` shell is still running.
    BackgroundShell,
}

// -- Generic types shared across backends --

/// Lookup tables derived from an agent's on-disk session manifest. The
/// dashboard merges entries from every active backend into one view; per-row
/// lookups dispatch via `state.agent`. Only Claude's manifest scan populates
/// the name maps today (renames only — its auto slug is dropped, and Codex's
/// title is overlaid onto `LauncherState.name` by the host's `LocalBackend`
/// instead), so the index's name contribution is a local-Claude fallback;
/// `session_id_by_pid` is its other, still-load-bearing job.
#[derive(Debug, Default, Clone)]
pub struct SessionIndex {
    /// Map child pid → display name.
    pub by_pid: HashMap<u32, String>,
    /// Owning backend for each pid in `by_pid`. Recorded at merge time (the
    /// per-backend shards don't know it) so the `by_pid` fallback in `lookup`
    /// can be gated on the row's own backend — a dead Claude session's pid can
    /// be reused by an unrelated Codex child, and without this the recycled pid
    /// would surface the stale Claude name on the Codex row.
    pub by_pid_owner: HashMap<u32, AgentControl>,
    /// Map session id → display name (a Claude `/rename` from its session-file
    /// manifest).
    pub by_session_id: HashMap<String, String>,
    /// Map child pid → live session id, used as a fallback when the launcher
    /// hasn't yet observed a session id from a hook event.
    pub session_id_by_pid: HashMap<u32, String>,
}

impl SessionIndex {
    /// Best display name for `state`, preferring the live session id (which
    /// covers renames) and falling back to the child-pid manifest entry.
    pub fn lookup(&self, state: &LauncherState) -> Option<&str> {
        if let Some(sid) = self.live_session_id(state)
            && let Some(name) = self.by_session_id.get(sid)
        {
            return Some(name.as_str());
        }
        // The pid maps only ever hold *local* sessions (a remote backend serves
        // an empty index), so a remote row must never borrow a name via a
        // colliding local pid — gate the by-pid fallback on the session's host.
        if state.host.is_local()
            && let Some(pid) = state.child_pid
            && self.by_pid_owner.get(&pid) == Some(&state.agent)
            && let Some(name) = self.by_pid.get(&pid)
        {
            return Some(name.as_str());
        }
        None
    }

    /// Live session id for `state`. The launcher updates `state.session_id`
    /// from every hook event, so it's authoritative when present; the manifest
    /// entry is only used as a startup-time fallback (local sessions only —
    /// `session_id_by_pid` holds no remote pids).
    pub fn live_session_id<'a>(&'a self, state: &'a LauncherState) -> Option<&'a str> {
        if let Some(sid) = state.session_id.as_deref() {
            return Some(sid);
        }
        if !state.host.is_local() {
            return None;
        }
        state
            .child_pid
            .and_then(|pid| self.session_id_by_pid.get(&pid).map(|s| s.as_str()))
    }
}

/// Per-pid mtime-keyed cache used by `read_session_index` to skip the JSON
/// parse for files that haven't changed since the last reload.
pub type SessionIndexCache = HashMap<u32, SessionIndexEntry>;

#[derive(Debug, Default, Clone)]
pub struct SessionIndexEntry {
    pub mtime: Option<SystemTime>,
    pub session_id: Option<String>,
    pub name: Option<String>,
}

/// One resumable session surfaced by `AgentControl::list_resumable`.
/// `Serialize`/`Deserialize` so a `captain-miao server` can ship it to a remote
/// dashboard's resume picker over the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCandidate {
    pub agent: AgentControl,
    pub session_id: String,
    pub cwd: String,
    pub first_prompt: Option<String>,
    pub custom_title: Option<String>,
    pub git_branch: Option<String>,
    pub mtime: SystemTime,
}

/// Per-session facts pulled from one pass over the transcript. Both fields come
/// from the same assistant entries (Claude) / the same rollout tail (Codex), so
/// reading them together avoids a second stat + file read per reload.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TranscriptStats {
    /// Latest context-window token total, in tokens.
    pub context_tokens: Option<u64>,
    /// Model id backing the latest turn (e.g. `claude-opus-4-8`, `gpt-5.5`).
    pub model: Option<String>,
    /// First real user prompt — the auto-title fallback shown before a rename
    /// (first-wins).
    pub first_prompt: Option<String>,
    /// Claude-only incremental-parse cursor: the byte offset reached plus the
    /// running accumulators, so the next reload folds only the lines appended
    /// since — instead of rescanning a multi-MB transcript on every keystroke
    /// the agent writes. `None` for Codex (which recomputes from a bounded
    /// tail) and before the first parse. Opaque to the dashboard, which reads
    /// only the two fields above.
    pub cursor: Option<claude::StatsCursor>,
}

/// Result of an incremental transcript scan — the launcher reads new bytes
/// since the last `new_offset` and the backend extracts whatever side-band
/// signals it cares about.
#[derive(Default)]
pub struct TranscriptScan {
    pub new_offset: u64,
    /// True if the new bytes contain an interrupt sentinel — agents that
    /// fire no hook on Esc need this so the launcher can leave Active.
    pub interrupted: bool,
    /// True if the new bytes contain a compact-command stderr — Claude fires
    /// no `PostCompact` when `/compact` itself errors (e.g. "Not enough
    /// messages to compact"), so without this the launcher would stay in
    /// `Compacting` forever.
    pub compact_aborted: bool,
    /// True if the new bytes *end* with a turn the agent opened itself — no
    /// prompt hook, no tool hook, nothing the hook arm will ever see (Codex
    /// under a goal). Ends-with rather than contains-any, because one delta
    /// routinely carries the end of one turn and the start of the next.
    ///
    /// The only **promoting** signal here, and deliberately without a
    /// demoting mirror: every ordinary turn *end* already arrives as a Stop
    /// hook, so a closed turn tells the launcher nothing it wasn't told.
    pub turn_started: bool,
}

/// The transcript bytes appended since `offset`, decoded lossily, plus the
/// offset a [`TranscriptScan`] should carry forward. Both backends read the
/// transcript tail identically — only the line-scan differs — so the byte
/// plumbing lives here to keep `claude` and `codex` from drifting.
pub struct TranscriptDelta {
    pub text: String,
    pub new_offset: u64,
}

/// Read the bytes appended to `path` since `offset`, lossily decoded.
///
/// `new_offset` advances past exactly the committed bytes that were read, so a
/// permanently-committed non-UTF-8 byte can't fail the read forever and freeze
/// the offset (which would lose later interrupt / compact-aborted signals).
/// Failure modes mirror the historical behaviour both backends relied on:
///   - open / metadata / seek failure, or `len < offset` (the file was
///     truncated or rotated) → `new_offset = 0`, empty text (re-read from the
///     start on the next scan);
///   - already at EOF (`len == offset`) or a read error → `new_offset = offset`,
///     empty text (hold position, surface no signals).
pub fn read_transcript_delta(path: &Path, offset: u64) -> TranscriptDelta {
    let reset = TranscriptDelta {
        text: String::new(),
        new_offset: 0,
    };
    let hold = TranscriptDelta {
        text: String::new(),
        new_offset: offset,
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return reset;
    };
    let Ok(meta) = file.metadata() else {
        return reset;
    };
    let len = meta.len();
    if len < offset {
        return reset;
    }
    if len == offset {
        return hold;
    }
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return reset;
    }
    let mut bytes: Vec<u8> = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return hold;
    }
    TranscriptDelta {
        new_offset: offset + bytes.len() as u64,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal state file / snapshot element: only the fields without a serde
    /// default, so the agent name is the variable under test.
    fn state_json(agent: &str, pid: u32) -> String {
        format!(
            r#"{{"agent":"{agent}","launcher_pid":{pid},"cwd":"/home/miao/p",
               "status":"idle","updated_at":0}}"#
        )
    }

    /// A name no build will ever know, standing in for the backend a *newer*
    /// captain-miao writes into a state file. Deliberately not a name off the
    /// roadmap: `"kimi"` was this fixture until Kimi shipped, and the tests then
    /// asserted the opposite of what they meant.
    const FUTURE_AGENT: &str = "a-later-backend";

    /// The hand-written `Deserialize` must not have moved the names it replaced.
    #[test]
    fn known_agent_names_still_decode() {
        // Every backend, by the name it advertises — so a `visit_str` arm that
        // is dropped, misspelled or reordered fails here rather than turning a
        // live session's rows into `Unknown` on the next dashboard.
        for &agent in AgentControl::ALL {
            let name = agent.cli_subcommand();
            let decoded: AgentControl = serde_json::from_str(&format!("\"{name}\""))
                .unwrap_or_else(|e| panic!("{name} must decode: {e}"));
            assert_eq!(decoded, agent, "{name} decoded to the wrong backend");
            // …and round-trips, which is what keeps a state file written by one
            // build readable by the next.
            assert_eq!(
                serde_json::to_string(&agent).expect("encodes"),
                format!("\"{name}\""),
                "{agent:?} does not serialize as the name it decodes from"
            );
        }
        // The one that could have needed a `#[serde(rename)]` and doesn't: the
        // derived `rename_all` spelling of `OpenCode` is one lowercase word,
        // which is also how the project spells it. The loop above would catch a
        // divergence, but this names the reason nobody has to re-derive it.
        assert_eq!(AgentControl::OpenCode.cli_subcommand(), "opencode");
    }

    /// A backend added after this build was cut decodes to `Unknown` instead of
    /// erroring, and re-serializes to a value that decodes back to itself (the
    /// name is gone, but it never silently becomes Claude).
    #[test]
    fn an_unrecognized_agent_name_decodes_to_unknown() {
        let a: AgentControl =
            serde_json::from_str(&format!("\"{FUTURE_AGENT}\"")).expect("decodes, not errors");
        assert_eq!(a, AgentControl::Unknown);

        let encoded = serde_json::to_string(&a).expect("encodes");
        assert_eq!(encoded, r#""unknown""#);
        let round: AgentControl = serde_json::from_str(&encoded).expect("round-trips");
        assert_eq!(round, AgentControl::Unknown);
    }

    /// The regression this variant exists for: `ServerFrame::Snapshot` carries a
    /// `Vec<LauncherState>`, so one element the build can't decode used to fail
    /// the *whole* frame — an older dashboard talking to a host running one new
    /// backend lost **every** row on that host. Every session must survive.
    #[test]
    fn one_unknown_agent_does_not_blank_the_whole_snapshot() {
        let snapshot = format!(
            "[{},{},{}]",
            state_json("claude", 1),
            state_json(FUTURE_AGENT, 2),
            state_json("codex", 3),
        );
        let sessions: Vec<LauncherState> =
            serde_json::from_str(&snapshot).expect("an unknown agent must not fail the frame");
        assert_eq!(
            sessions.len(),
            3,
            "every session in the snapshot must survive one undecodable agent name"
        );
        assert_eq!(
            sessions.iter().map(|s| s.agent).collect::<Vec<_>>(),
            [
                AgentControl::Claude,
                AgentControl::Unknown,
                AgentControl::Codex
            ]
        );
        // The rest of the unknown row is intact, so it still renders and can
        // still be killed.
        assert_eq!(sessions[1].launcher_pid, 2);
        assert_eq!(sessions[1].cwd, "/home/miao/p");
    }

    /// `Unknown` out of `ALL` is what keeps it out of the `Space a` picker and
    /// the workdir picker's `Ctrl-t` cycle — both iterate exactly this.
    #[test]
    fn all_excludes_unknown() {
        assert!(!AgentControl::ALL.contains(&AgentControl::Unknown));
        // Order is the `Ctrl-t` cycle and the picker order, so this pins it:
        // a new backend appends, it never inserts.
        assert_eq!(
            AgentControl::ALL,
            &[
                AgentControl::Claude,
                AgentControl::Codex,
                AgentControl::Reasonix,
                AgentControl::Kimi,
                AgentControl::Grok,
                AgentControl::OpenCode,
                AgentControl::Pi,
                AgentControl::Antigravity,
                AgentControl::Omp
            ]
        );
    }

    /// A typo on the command line is a user error, not a compatibility gap:
    /// `from_cli` still refuses rather than handing back an inert backend.
    #[test]
    fn from_cli_rejects_an_unrecognized_name_instead_of_unknown() {
        assert_eq!(AgentControl::from_cli(FUTURE_AGENT), None);
        assert_eq!(AgentControl::from_cli(""), None);
        assert_eq!(AgentControl::from_cli("claude"), Some(AgentControl::Claude));
        assert_eq!(
            AgentControl::from_cli("Reasonix"),
            Some(AgentControl::Reasonix),
            "the CLI spelling is case-insensitive, unlike the on-disk one"
        );
    }

    /// Nothing may be launched from a backend we can't name — guessing an argv
    /// would run the wrong agent in the user's cwd.
    #[test]
    fn launching_an_unknown_agent_is_an_error() {
        let err = AgentControl::Unknown
            .build_launch_command(
                "/home/miao/p",
                Path::new("/run/miao.sock"),
                Path::new("/run/miao-settings.json"),
                &[],
                None,
            )
            .expect_err("an unknown backend has no launch command");
        assert!(
            err.to_string().contains("upgrade"),
            "the error must name the fix: {err}"
        );
    }

    /// Tolerance is for *names*, not for malformed JSON: a number in the field
    /// is a broken payload, and reading it as `Unknown` would hide that.
    #[test]
    fn a_non_string_agent_is_still_a_decode_error() {
        assert!(serde_json::from_str::<AgentControl>("7").is_err());
        assert!(serde_json::from_str::<AgentControl>("null").is_err());
        assert!(serde_json::from_str::<AgentControl>(r#"{"name":"claude"}"#).is_err());
    }

    /// The gate is nothing but "the two argvs differ", in both directions.
    /// Kimi is the first backend on the false side and the reason the gate
    /// exists: its arm ignores `fork`, and that alone is what has to be true —
    /// no separate match, no flag invented to fill the gap.
    #[test]
    fn a_backend_forks_exactly_when_its_fork_argv_differs() {
        for &agent in AgentControl::ALL {
            let plain = agent.resume_args("id", false);
            let forked = agent.resume_args("id", true);
            assert_eq!(
                agent.supports_fork(),
                plain != forked,
                "{agent:?}'s gate must be its argv, nothing else"
            );
        }
        // Named rather than only derived, so a backend quietly losing (or
        // growing) its fork flag fails here instead of just hiding a key.
        assert!(AgentControl::Claude.supports_fork());
        assert!(AgentControl::Codex.supports_fork());
        assert!(AgentControl::Reasonix.supports_fork());
        assert!(
            AgentControl::Pi.supports_fork(),
            "Pi forks by swapping --session for --fork, which is still a differing argv"
        );
        assert!(
            !AgentControl::Kimi.supports_fork(),
            "Kimi has no fork flag, so `f` must hide rather than resume in place"
        );
        assert!(
            AgentControl::OpenCode.supports_fork(),
            "opencode has --fork"
        );
        assert!(
            AgentControl::Omp.supports_fork(),
            "omp forks with --fork where it resumes with --resume"
        );

        // A backend this build can't drive contributes no argv either way, so
        // the two are equal and it reports no fork — rather than offering `f`
        // and resuming in place.
        assert_eq!(
            AgentControl::Unknown.resume_args("id", true),
            AgentControl::Unknown.resume_args("id", false)
        );
        assert!(!AgentControl::Unknown.supports_fork());
    }

    /// The whole subscription surface, backend by backend, read back out of the
    /// configs themselves. This is the test that makes `forwarded_events`
    /// trustworthy enough to derive a capability from: if the token scan ever
    /// stops matching — a backend rewrites its config in a shape the boundary
    /// rule doesn't cover, say — every set here collapses at once rather than
    /// one capability quietly turning false.
    #[test]
    fn every_backend_forwards_exactly_the_events_its_config_registers() {
        use HookEvent::*;
        let expected: &[(AgentControl, &[HookEvent])] = &[
            (
                AgentControl::Claude,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PostToolUseFailure,
                    PermissionRequest,
                    Elicitation,
                    ElicitationResult,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                    CwdChanged,
                ],
            ),
            // Eight, not the twelve `HookEvent`s that appear in `agents::codex`:
            // `Elicitation`, `ElicitationResult`, `StopFailure` and `CwdChanged`
            // are *produced* there — refined out of a `PreToolUse` payload, or
            // synthesized — rather than registered with Codex. Reading the
            // config is what tells the two apart.
            (
                AgentControl::Codex,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PermissionRequest,
                    Stop,
                    PreCompact,
                    PostCompact,
                ],
            ),
            // No `PostCompact`: Reasonix has no such hook, which is why
            // `Compacting` is left by the next event of any kind.
            (
                AgentControl::Reasonix,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PostToolUseFailure,
                    PermissionRequest,
                    Stop,
                    StopFailure,
                    PreCompact,
                ],
            ),
            (
                AgentControl::Kimi,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PostToolUseFailure,
                    PermissionRequest,
                    ElicitationResult,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                ],
            ),
            // No `Elicitation`: opencode's `permission.updated` is the gate and
            // `permission.replied` releases it, so the approval pair carries
            // what a decision prompt would.
            // `PermissionRequest` is in the settings file via the lifecycle
            // `Notification` / `permission_prompt` matcher, and again via
            // `extra_hook_registrations` (the `[[ui.notifications.hooks]]`
            // fallback). `StopCancelled` forwards as `Stop`, so it does not
            // appear as its own variant.
            (
                AgentControl::Grok,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PostToolUseFailure,
                    PermissionRequest,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                ],
            ),
            (
                AgentControl::OpenCode,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PermissionRequest,
                    ElicitationResult,
                    Stop,
                    StopFailure,
                    PreCompact,
                    PostCompact,
                    CwdChanged,
                ],
            ),
            // The shortest list, and the one the capability matrix turns on:
            // **no `PermissionRequest` and no `Elicitation`**, because Pi has no
            // per-tool gate to subscribe to at all.
            (
                AgentControl::Pi,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    Stop,
                    PreCompact,
                    PostCompact,
                ],
            ),
            // Shorter still, and none of the three absences is a gap of ours:
            // Antigravity's entire vocabulary is five events, it fires none at
            // session start or on compaction, and its one pre-tool event is a
            // permission *gate* that cannot be observed without answering
            // (`agents::antigravity`). `PostToolUseFailure` and `StopFailure`
            // are absent here for the opposite reason to Codex's — the
            // dispatcher does produce them, out of the `error` field on these.
            (
                AgentControl::Antigravity,
                &[PromptSubmit, PostToolUse, Stop],
            ),
            // No `Elicitation`: omp's `tool_approval_requested` is the gate and
            // `tool_approval_resolved` releases it, so the approval pair carries
            // what a separate decision prompt would. `PostToolUseFailure` is
            // absent (derived in Rust from `is_error`, never registered) — the
            // same shape opencode already has.
            (
                AgentControl::Omp,
                &[
                    SessionStart,
                    PromptSubmit,
                    PreToolUse,
                    PostToolUse,
                    PermissionRequest,
                    ElicitationResult,
                    Stop,
                    PreCompact,
                    PostCompact,
                ],
            ),
        ];
        assert_eq!(
            expected.len(),
            AgentControl::ALL.len(),
            "a backend without a row here would be pinned by nothing at all"
        );
        for (agent, events) in expected {
            assert_eq!(
                &agent.forwarded_events(),
                events,
                "{agent:?}'s generated hook config no longer registers what this pins"
            );
        }
        // The read-side-only variant writes no config, so it subscribes to
        // nothing — which is also what makes every one of its capabilities
        // false without a special case.
        assert!(AgentControl::Unknown.forwarded_events().is_empty());
    }

    /// The three prefix pairs that make the boundary rule load-bearing. Without
    /// it, every backend that registers `stop-failure` would also report a
    /// `Stop` it may not have — and Pi, which registers neither
    /// `post-tool-use-failure` nor `elicitation-result`, is the backend whose
    /// set would change shape.
    #[test]
    fn an_event_name_that_prefixes_another_is_not_read_as_that_other() {
        assert!(mentions_event(
            r#"{"c":"miao hook stop-failure"}"#,
            "stop-failure"
        ));
        assert!(!mentions_event(r#"{"c":"miao hook stop-failure"}"#, "stop"));
        assert!(!mentions_event(
            r#"{"c":"miao hook post-tool-use-failure"}"#,
            "post-tool-use"
        ));
        assert!(!mentions_event(
            r#"{"c":"miao hook elicitation-result"}"#,
            "elicitation"
        ));
        // A path that happens to spell an event is not a subscription: nothing
        // in the config puts a `"` straight after it.
        assert!(!mentions_event(
            r#"{"c":"/opt/stop/miao hook stop-failure"}"#,
            "stop"
        ));
        // A command that keeps going after the event name is still a
        // subscription — Antigravity's commands end in a stdout contract, and
        // the prefix guard has to survive that too.
        assert!(mentions_event(
            r#"{"c":"miao hook stop >/dev/null; echo {}"}"#,
            "stop"
        ));
        assert!(!mentions_event(
            r#"{"c":"miao hook stop-failure >/dev/null; echo {}"}"#,
            "stop"
        ));
    }

    /// **The test the declared matrix is worth having.** `capabilities()` is a
    /// table of constants so the draw loop can read it for free; this runs the
    /// derivation it skipped and demands the two agree, for every backend and
    /// every field. Three of the four are then unfalsifiable by hand: a
    /// capability can only change by changing the argv, the worktree flag or the
    /// hook config that produced it.
    ///
    /// `context_tokens` is the one field with nothing to derive from — no method
    /// answers "could this backend ever report a total" without a live session —
    /// so it is pinned by the matrix test below and by the module docs that
    /// state, per backend, why the agent has no number to give.
    #[test]
    fn the_capability_matrix_agrees_with_the_implementations() {
        for &agent in AgentControl::ALL {
            let c = agent.capabilities();
            assert_eq!(
                c.fork,
                agent.supports_fork(),
                "{agent:?}: the declared fork capability is not what its resume argv does"
            );
            assert_eq!(
                c.worktrees,
                agent.supports_worktrees(),
                "{agent:?}: the declared worktree capability is not what its worktree argv does"
            );
            let forwarded = agent.forwarded_events();
            let gates = forwarded.contains(&HookEvent::PermissionRequest)
                || forwarded.contains(&HookEvent::Elicitation);
            assert_eq!(
                c.approval_gate, gates,
                "{agent:?}: the declared approval gate is not what its hook config registers \
                 (forwarded: {forwarded:?})"
            );
        }
        // Same three checks for the variant that is not in `ALL`, since it is
        // the one whose row is all-false by policy rather than by behaviour.
        let c = AgentControl::Unknown.capabilities();
        assert!(!c.fork && !c.worktrees && !c.approval_gate && !c.context_tokens);
    }

    /// The matrix itself, written out — what the README's per-agent notes
    /// promise, in one reviewable block. The test above proves each row is
    /// *consistent*; this one is where a deliberate change has to be seen and
    /// agreed to.
    #[test]
    fn the_capability_matrix_is_what_the_readme_promises() {
        let row = |a: AgentControl| {
            let c = a.capabilities();
            (c.fork, c.worktrees, c.approval_gate, c.context_tokens)
        };
        //                                       fork   tree   appr   ctx
        let promised: &[(AgentControl, _)] = &[
            (AgentControl::Claude, (true, true, true, true)),
            (AgentControl::Codex, (true, false, true, true)),
            (AgentControl::Reasonix, (true, false, true, false)),
            (AgentControl::Kimi, (false, false, true, true)),
            (AgentControl::Grok, (true, true, true, true)),
            (AgentControl::OpenCode, (true, false, true, true)),
            (AgentControl::Pi, (true, false, false, true)),
            (AgentControl::Antigravity, (false, false, false, false)),
            (AgentControl::Omp, (true, false, true, true)),
        ];
        assert_eq!(
            promised.len(),
            AgentControl::ALL.len(),
            "a backend missing here is a backend the README never promised anything about"
        );
        for &(agent, want) in promised {
            assert_eq!(row(agent), want, "{agent:?}");
        }
    }
}
