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

#[derive(Parser)]
#[command(
    name = "miao-server",
    version,
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    // The daemon self-daemonizes (a double-fork that must precede any thread) and,
    // with pty-pool, starts the pool on a thread — so it's dispatched *here*,
    // before the tokio runtime spawns a thread. `ensure` builds its own runtime
    // and serves forever; `status`/`stop`/`print-path` are quick and synchronous.
    if let Commands::Daemon { action } = &cli.command {
        return server::dispatch(action.clone());
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
        Commands::Claude { args } => cm_core::cli::run_launch(AgentControl::Claude, args).await,
        Commands::Codex { args } => cm_core::cli::run_launch(AgentControl::Codex, args).await,
        Commands::Hook { event, sock, agent } => {
            cm_core::cli::run_hook(&agent, &event, sock.as_deref()).await
        }
        Commands::Daemon { .. } => {
            unreachable!("daemon is dispatched in main() before the runtime")
        }
        #[cfg(feature = "pty-pool")]
        Commands::PtyDaemon | Commands::Attach { .. } => {
            unreachable!("pty-pool commands are dispatched in main() before the runtime")
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
}
