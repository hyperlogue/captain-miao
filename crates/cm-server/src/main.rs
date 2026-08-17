//! `miao-server` — the headless per-host runtime: the persistent daemon
//! (pty pool + wire protocol) plus the `claude`/`codex`/`hook` entrypoints used
//! by launchers running *inside* the pool. A remote dashboard reaches this over
//! ssh; it never drives a terminal, so there is no Kitty gate. All the shared
//! logic (state, protocol, agents, launcher, hooks) lives in `cm-core`.

#[cfg(feature = "pty-pool")]
mod pty_pool;
mod server;
#[cfg(feature = "pty-pool")]
mod server_pool;

use anyhow::Result;
use clap::{Parser, Subcommand};

use cm_core::agent::AgentControl;
// Re-exported at the crate root so the server modules' `crate::backend` /
// `crate::protocol` / `crate::state` / `crate::init_tracing` paths resolve.
pub use cm_core::logging::init_tracing;
pub use cm_core::{backend, protocol, state};

/// `<version> protocol <n>` — what `--version` prints after the binary name.
///
/// The protocol number rides the **same line** as the version rather than a
/// second one, and that is load-bearing: the dashboard's probe runs
/// `--version` over ssh and parses its output positionally, one field per line
/// (`parse_probe` in the dashboard's `backend.rs`), so an extra line would
/// shift every field after it. Keeping the shape `miao-server <ver> protocol
/// <n>` also leaves "the second word is the version" true, which is how both
/// the probe and the deploy's post-upload check read it.
///
/// A fn rather than a literal because `PROTOCOL_VERSION` is a `u32` const —
/// `concat!` takes literals only — and `&'static str` because that is what
/// clap's `version` attribute wants.
fn version_string() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        format!(
            "{} protocol {}",
            env!("CARGO_PKG_VERSION"),
            protocol::PROTOCOL_VERSION
        )
    })
}

#[derive(Parser)]
#[command(
    name = "miao-server",
    version = version_string(),
    about = "captain-miao per-host daemon: pty pool + wire protocol"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch Claude Code with hooks injected, inside the pty pool (headless).
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Codex with hooks injected, inside the pty pool (headless).
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Reasonix with hooks injected, inside the pty pool (headless).
    Reasonix {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Kimi Code with hooks injected, inside the pty pool (headless).
    Kimi {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Grok Build with hooks injected, inside the pty pool (headless).
    Grok {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch opencode with a tracking plugin injected, inside the pty pool
    /// (headless). `name` is explicit because clap would derive `open-code`.
    #[command(name = "opencode")]
    OpenCode {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Pi with hooks injected, inside the pty pool (headless).
    Pi {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Google's Antigravity CLI with hooks injected, inside the pty
    /// pool (headless). Named for the product, not for its `agy` binary — the
    /// subcommand has to match `cli_subcommand()`.
    Antigravity {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Handle an agent hook event (called by hook scripts).
    Hook {
        /// Hook event type.
        event: String,
        /// Launcher socket path. Falls back to $CAPTAIN_MIAO_SOCK when omitted.
        #[arg(long)]
        sock: Option<String>,
        /// Which backend's hook payload to parse.
        #[arg(long, default_value = "claude")]
        agent: String,
    },

    /// The per-host daemon a remote dashboard connects to over ssh: one
    /// persistent, self-daemonizing process that hosts the pty pool AND answers
    /// the wire protocol on a unix socket. Singleton per host; survives ssh
    /// disconnects; auto-exits when idle. Manage it with the subcommands.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Run the libshpool pty-pool daemon (Phase 3). Headless; the per-host daemon
    /// hosts it on a thread. Dispatched before the async runtime starts.
    #[cfg(feature = "pty-pool")]
    PtyDaemon,

    /// Attach to (or create) a named pool session, proxying its pty to this
    /// terminal. Run by the dashboard's `ssh -t <host> miao-server attach
    /// <name>` window (plain reattach), or by the daemon with `--cmd`/`--background`
    /// to create a session running the launcher.
    #[cfg(feature = "pty-pool")]
    Attach {
        /// Pool session name (the join key bound to the local window).
        name: String,
        /// Create the session running this command instead of a shell, then
        /// detach (server-side launcher start). Shell-words quoted.
        #[arg(long)]
        cmd: Option<String>,
        /// Working directory for the created session.
        #[arg(long)]
        dir: Option<String>,
        /// Create/attach then immediately detach (background create).
        #[arg(long)]
        background: bool,
        /// Steal the session from whatever client currently holds it: the pool
        /// is one client at a time, so a busy session otherwise declines. The
        /// kicked client's attach process exits cleanly (its dashboard reads
        /// that as a detach) and the session itself is undisturbed.
        #[arg(long)]
        force: bool,
        /// Write libshpool's client-side log to this file. The daemon passes it
        /// for `--background` creates: without it the attach client's logs —
        /// including the error it prints before a silent `exit(1)` — go to
        /// `io::empty()`, making a failed create undebuggable.
        #[arg(long)]
        log_file: Option<String>,
    },

    /// Clipboard-bridge helpers for the machine an agent runs on.
    Clipboard {
        #[command(subcommand)]
        action: ClipboardAction,
    },

    /// Prove this binary can actually host a session on this machine, then print
    /// the same `miao-server <ver> protocol <n>` line `--version` does.
    ///
    /// Run by the dashboard's deploy on the staged binary, *before* it is moved
    /// into place. It exists because `--version` is too weak a check: it proves
    /// the file loads and matches, which catches a wrong architecture, a missing
    /// loader and a truncated transfer — but not a binary whose `getpwuid` finds
    /// nothing, which is exactly the static-musl-on-LDAP/SSSD trap. Such a
    /// server installs cleanly and then fails on *first attach*, because
    /// libshpool resolves the user with `getpwuid_r` and errors when the lookup
    /// comes back empty, taking `home_dir` and the shell with it.
    ///
    /// A loud refusal at deploy time is fine. A silent trap is not.
    SelfCheck,
}

impl Commands {
    /// `Some((agent, args))` for the launcher subcommands, `None` for everything
    /// else — the single place a clap variant maps to an [`AgentControl`], so a
    /// new backend is a new variant plus one arm below.
    ///
    /// The dashboard's `Commands` (`src/main.rs`) carries a twin. They are
    /// separate clap enums over different subcommand sets — the server has no
    /// `focus`, the dashboard no `daemon` — so this is duplicated on purpose
    /// rather than hoisted into `cm-core`.
    fn launcher(&self) -> Option<(AgentControl, &[String])> {
        match self {
            Commands::Claude { args } => Some((AgentControl::Claude, args)),
            Commands::Codex { args } => Some((AgentControl::Codex, args)),
            Commands::Reasonix { args } => Some((AgentControl::Reasonix, args)),
            Commands::Kimi { args } => Some((AgentControl::Kimi, args)),
            Commands::Grok { args } => Some((AgentControl::Grok, args)),
            Commands::OpenCode { args } => Some((AgentControl::OpenCode, args)),
            Commands::Pi { args } => Some((AgentControl::Pi, args)),
            Commands::Antigravity { args } => Some((AgentControl::Antigravity, args)),
            _ => None,
        }
    }
}

/// Clipboard-bridge actions on the machine an agent runs on. The *server* half
/// (`clipboard serve`) belongs to the dashboard's binary, since that is the
/// machine that owns a clipboard.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
enum ClipboardAction {
    /// Write the dashboard machine's clipboard image to a file here and print its
    /// path.
    ///
    /// For an agent that reads the clipboard in-process and so cannot be shimmed
    /// (Codex). Reachable as `clipboard-paste` too — the shim farm carries a
    /// symlink for it, so the agent needs no binary name.
    Paste,
}

/// Lifecycle actions for the per-host daemon (tmux/zellij-style). `ensure` is
/// what the dashboard fires over ssh; `status`/`stop` are the management surface.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
enum DaemonAction {
    /// Start the daemon if it isn't already running (self-daemonizes + detaches),
    /// then print its socket path. Idempotent. The dashboard fires this on connect.
    Ensure,
    /// Print this host's daemon socket path and exit (don't start).
    PrintPath,
    /// Report whether the daemon is running (pid, socket, live session count).
    Status,
    /// Stop the daemon. This tears down the pty pool, so **all of this host's
    /// pooled sessions die with it**. `--force` skips the are-you-sure guard.
    Stop {
        #[arg(long)]
        force: bool,
    },
}

/// Resolve this process's own passwd entry — the check `--version` cannot make.
///
/// This is deliberately the *same call* libshpool makes (`getpwuid_r` for the
/// effective uid), not a proxy for it: `$HOME` being set proves nothing, since
/// the pool re-resolves the user itself and fails on the lookup rather than on
/// the environment. Anything weaker would pass on precisely the hosts this
/// exists to refuse — a static-musl build on LDAP/SSSD, where NSS is compiled
/// out and no `passwd` entry is visible for a perfectly valid uid.
///
/// `Ok(name)` when the entry resolves. `Err` names the uid, because on such a
/// host the uid is the only identifying thing left to print.
fn resolve_user() -> Result<String> {
    // SAFETY: `getpwuid_r` writes into the caller's buffer and reports through
    // `result`, so there is no shared static to race. `buf` outlives every
    // pointer `pwd` can hold into it, and the `CStr` is read before either is
    // dropped.
    unsafe {
        let uid = libc::geteuid();
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // 16 KiB: `sysconf(_SC_GETPW_R_SIZE_MAX)` is merely a hint and returns
        // -1 on some libcs, so a fixed generous buffer is both simpler and more
        // portable. ERANGE would surface as a failure below, not a wrong answer.
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
        if rc != 0 || result.is_null() {
            anyhow::bail!(
                "no passwd entry for uid {uid} — this build cannot resolve users on this host \
                 (a static build has no NSS, so LDAP/SSSD users are invisible to it); \
                 sessions would fail to attach"
            );
        }
        Ok(std::ffi::CStr::from_ptr(pwd.pw_name)
            .to_string_lossy()
            .into_owned())
    }
}

/// `self-check`: can this binary host a session *here*?
///
/// Prints the same first-word-`miao-server` line as `--version`, so the deploy's
/// existing "read the version past whatever the login shell printed" parse works
/// unchanged on its output.
fn self_check() -> Result<()> {
    let user = resolve_user()?;
    println!("miao-server {} user {user}", version_string());
    Ok(())
}

fn main() -> Result<()> {
    // **Before clap.** We may have been invoked through the clipboard shim farm,
    // under another tool's name and with an argv clap has never heard of — and
    // clap exits 2 on one of those, so asking it first would turn every shimmed
    // `xclip -selection clipboard -t TARGETS -o` into a usage error instead of a
    // paste. This is the only `argv[0]` dispatch in the tree; the daemon and
    // pty-pool dispatches below are ordinary clap subcommands that merely have to
    // precede the runtime.
    if let Some(invocation) = cm_core::clipboard::shim::from_argv0() {
        invocation.run();
    }

    let cli = Cli::parse();

    // The daemon self-daemonizes (a double-fork that must precede any thread) and,
    // with pty-pool, starts the pool on a thread — so it's dispatched *here*,
    // before the tokio runtime spawns a thread. `ensure` builds its own runtime
    // and serves forever; `status`/`stop`/`print-path` are quick and synchronous.
    if let Commands::Daemon { action } = &cli.command {
        return server::dispatch(action.clone());
    }

    // Synchronous, and deliberately ahead of the runtime: it must answer even on
    // a binary too broken to stand up tokio, since the whole point is that the
    // *deploy* runs it on a host we know nothing about.
    if matches!(cli.command, Commands::SelfCheck) {
        return self_check();
    }

    // The pty-pool client entrypoints embed libshpool, whose `run` must likewise
    // be called while single-threaded (its daemon path can double-fork). Dispatch
    // them here too, before the runtime.
    #[cfg(feature = "pty-pool")]
    match cli.command {
        Commands::PtyDaemon => return pty_pool::run_daemon(),
        Commands::Attach {
            name,
            cmd,
            dir,
            background,
            force,
            log_file,
        } => return pty_pool::run_attach(name, cmd, dir, background, force, log_file),
        _ => {}
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Hook { event, sock, agent } => {
            cm_core::cli::run_hook(&agent, &event, sock.as_deref()).await
        }
        Commands::Daemon { .. } => {
            unreachable!("daemon is dispatched in main() before the runtime")
        }
        Commands::SelfCheck => {
            unreachable!("self-check is dispatched in main() before the runtime")
        }
        #[cfg(feature = "pty-pool")]
        Commands::PtyDaemon | Commands::Attach { .. } => {
            unreachable!("pty-pool commands are dispatched in main() before the runtime")
        }
        // Every launcher subcommand, dispatched through `Commands::launcher` so
        // the variant→`AgentControl` mapping stays in that one place. Named here
        // rather than swept up by a catch-all so the match stays exhaustive —
        // see the twin in the dashboard's `main.rs`.
        Commands::Clipboard {
            action: ClipboardAction::Paste,
        } => cm_core::clipboard::shim::paste().await,
        cmd @ (Commands::Claude { .. }
        | Commands::Codex { .. }
        | Commands::Reasonix { .. }
        | Commands::Kimi { .. }
        | Commands::Grok { .. }
        | Commands::OpenCode { .. }
        | Commands::Pi { .. }
        | Commands::Antigravity { .. }) => {
            let (agent, args) = cmd.launcher().expect("the launcher variants");
            // `run_launch_pooled`, not `run_launch`: every `miao-server` launcher
            // runs inside the pty pool, so its agent gets the clipboard shims on
            // its PATH. The dashboard's own launcher arms call the other one.
            cm_core::cli::run_launch_pooled(agent, args.to_vec()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_subcommands_parse() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.command);
        assert!(matches!(
            parse(&["miao-server", "daemon", "ensure"]).unwrap(),
            Commands::Daemon {
                action: DaemonAction::Ensure
            }
        ));
        assert!(matches!(
            parse(&["miao-server", "daemon", "print-path"]).unwrap(),
            Commands::Daemon {
                action: DaemonAction::PrintPath
            }
        ));
        assert!(matches!(
            parse(&["miao-server", "daemon", "status"]).unwrap(),
            Commands::Daemon {
                action: DaemonAction::Status
            }
        ));
        assert!(matches!(
            parse(&["miao-server", "daemon", "stop"]).unwrap(),
            Commands::Daemon {
                action: DaemonAction::Stop { force: false }
            }
        ));
        assert!(matches!(
            parse(&["miao-server", "daemon", "stop", "--force"]).unwrap(),
            Commands::Daemon {
                action: DaemonAction::Stop { force: true }
            }
        ));
        // An unknown action is rejected, not silently accepted.
        assert!(parse(&["miao-server", "daemon", "frobnicate"]).is_err());
    }

    /// The twin of the dashboard's own check: every backend's
    /// `cli_subcommand()` must name a subcommand *this* binary has too, because
    /// a pooled launch spawns `miao-server <cli_subcommand()> <cwd>` on the
    /// host. The two clap enums are separate on purpose, which is exactly why
    /// each needs its own guard — clap kebab-cases a two-word variant
    /// (`OpenCode` → `open-code`), and a name that diverges here fails only on
    /// a *remote* launch, where nobody is watching.
    #[test]
    fn every_backend_launches_under_the_name_it_advertises() {
        for &want in AgentControl::ALL {
            let name = want.cli_subcommand();
            let cli = Cli::try_parse_from(["miao-server", name, "/work"])
                .unwrap_or_else(|e| panic!("`miao-server {name}` must parse: {e}"));
            let (got, args) = cli
                .command
                .launcher()
                .unwrap_or_else(|| panic!("`miao-server {name}` must map to a backend"));
            assert_eq!(got, want, "`miao-server {name}` launched the wrong backend");
            assert_eq!(args, ["/work"]);
        }
    }
}
