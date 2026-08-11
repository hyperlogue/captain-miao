//! `miao` — the dashboard: the ratatui TUI that monitors and manages sessions, in
//! Kitty, locally and over ssh. It also carries the `claude`/`codex`/`hook`
//! entrypoints (so a local launch needs only this one binary) and `focus`. The
//! headless per-host daemon + pty pool is the separate `miao-server`
//! binary; shared logic lives in `cm-core`.
//!
//! The binary is `miao` while the package (and the project) stays `captain-miao` —
//! see the `[[bin]]` note in `Cargo.toml`.

mod app;
mod backend;
mod config;
mod port_forward;
mod server_payload;
mod sleep;
mod terminal;

// Core modules re-exported at the crate root so the dashboard's `crate::state`,
// `crate::agent`, `crate::protocol`, `crate::init_tracing` paths resolve unchanged.
pub use cm_core::logging::init_tracing;
pub use cm_core::{agent, protocol, state};

use anyhow::Result;
use clap::{Parser, Subcommand};

/// `--version`'s long form: the version, plus which `miao-server`
/// builds this binary can deploy to a remote host.
///
/// That inventory is decided at *build* time (by the `bundle-*` cargo features),
/// so no amount of looking at config or state can answer it — `--version` is the
/// only place it can honestly live, and with several dashboard variants shipping
/// it is also how you tell two of them apart. `-V` keeps the bare version for
/// scripts.
fn long_version() -> &'static str {
    static LONG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LONG.get_or_init(|| {
        format!(
            "{}\n{}",
            env!("CARGO_PKG_VERSION"),
            server_payload::describe()
        )
    })
}

#[derive(Parser)]
#[command(
    name = "miao",
    version,
    long_version = long_version(),
    about = "Monitor and manage Claude Code sessions in Kitty or zellij"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch Claude Code with hooks injected for session tracking.
    ///
    /// The first positional is the working directory (defaults to `.`) unless
    /// it begins with `-`, in which case it (and everything after) is forwarded
    /// to `claude` — so `miao claude --resume` works. See `cli::split_cwd`.
    Claude {
        /// Working directory (first positional, unless it starts with `-`)
        /// followed by any extra arguments passed straight to claude.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Launch Codex with hooks injected for session tracking. Argument handling
    /// matches the `claude` subcommand.
    Codex {
        /// Working directory (first positional, unless it starts with `-`)
        /// followed by any extra arguments passed straight to codex.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Handle an agent hook event (called by hook scripts)
    Hook {
        /// Hook event type
        event: String,

        /// Launcher socket path. Falls back to $CAPTAIN_MIAO_SOCK when omitted
        /// (Codex hooks read it from the environment to keep hooks.json stable).
        #[arg(long)]
        sock: Option<String>,

        /// Which backend's hook payload to parse.
        #[arg(long, default_value = "claude")]
        agent: String,
    },

    /// Focus the dashboard window and (with --window-id) flag the session
    /// running in that Kitty window so its bell indicator lights up. Bind
    /// to a Kitty key with `launch --type=background miao focus
    /// --window-id @active-kitty-window-id` to ring the bell from any
    /// Claude Code session.
    Focus {
        /// Kitty window id whose session should have its bell flag set.
        /// When omitted, just focuses the dashboard.
        #[arg(long)]
        window_id: Option<terminal::WindowId>,
    },

    /// Report that an attach window's session ended, so the dashboard can drop
    /// its window binding at once instead of waiting for a periodic window-tree
    /// snapshot to notice. Invoked by the wrapper the dashboard puts around
    /// every attach command — not something to run by hand, hence hidden.
    #[command(hide = true)]
    AttachExited {
        /// The host the attached session lives on.
        #[arg(long)]
        host: String,

        /// The session's binding token (its pool session name).
        #[arg(long)]
        token: String,

        /// The attach command's exit status, which tells a session that ran and
        /// ended from one that was refused on arrival — the wrapper holds the
        /// latter's window open so its error stays readable.
        #[arg(long)]
        status: Option<i32>,

        /// Wall-clock seconds the attach ran, measured by the wrapper. The
        /// dashboard's own binding age is monotonic and so stops during a
        /// suspend; this doesn't.
        #[arg(long)]
        held_secs: Option<u64>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // The dashboard is entirely async (the TUI, or a launcher/hook/focus). Build
    // the runtime and run it. (The pre-runtime daemon/pool dispatch that used to
    // live here moved to the `miao-server` binary.)
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main(cli))
}

/// Commands that don't drive a terminal window and so must skip the
/// supported-terminal gate: the `hook` forwarder (runs wherever the agent runs,
/// including a headless remote pool) and a pooled launcher still carrying
/// `--pool-session`. Everything else drives a local Kitty/zellij window.
fn requires_terminal(command: &Option<Commands>) -> bool {
    match command {
        // A hook is a thin forwarder (parse the agent's stdin JSON → send to the
        // launcher socket); it never touches a terminal and runs wherever the
        // agent runs.
        Some(Commands::Hook { .. }) => false,
        // A detach report is one file write, fired from a dying attach window's
        // trap. It drives no window, and gating it on the terminal would make it
        // fail exactly when the terminal is going away — the case it exists for.
        Some(Commands::AttachExited { .. }) => false,
        // A pooled launcher carries `--pool-session` in its passthrough args here
        // (stripped later); it runs on a headless pool host, not in a terminal.
        Some(Commands::Claude { args } | Commands::Codex { args }) => {
            !args.iter().any(|a| a == "--pool-session")
        }
        _ => true,
    }
}

async fn async_main(cli: Cli) -> Result<()> {
    // Most commands drive a terminal window and so require running inside a
    // supported terminal (Kitty or zellij); the `hook` forwarder and a pooled
    // launcher are exempt — see `requires_terminal`. Detection is delegated to
    // `terminal::supported_terminal_present` (the single detection owner,
    // honoring the `[terminal] backend` override) so the gate can't disagree
    // with the backend `terminal::get()` actually builds.
    if requires_terminal(&cli.command) && !terminal::supported_terminal_present() {
        eprintln!("captain-miao must be run inside a supported terminal (Kitty or zellij)");
        std::process::exit(1);
    }

    // Being *in* a supported terminal isn't the same as being able to drive it:
    // on Kitty every window op is a `kitten @` round-trip over a socket the user
    // has to enable, with a password that has to match across two config files.
    // So the backend detection settled on is asked to prove the channel works
    // (`terminal::verify_control`) before the dashboard commits to it. Fail
    // fast rather than start: without remote control the dashboard can't spawn,
    // focus, preview or move a window, and the password half doesn't even
    // *error* — kitty answers a password it doesn't accept by prompting in its
    // own window, so the first rc call would hang the loop forever with the TUI
    // already owning the screen. Here, stderr is still the user's terminal and
    // the diagnosis is readable.
    //
    // Dashboard-only: a launcher never touches the terminal (it self-reports its
    // window from the env), and `focus` is a single rc call whose own error
    // surfaces the same way — neither should be gated on a probe they'd pay for
    // and not need.
    if cli.command.is_none()
        && let Err(e) = terminal::verify_control().await
    {
        eprintln!("{e}");
        std::process::exit(1);
    }

    match cli.command {
        Some(Commands::Claude { args }) => {
            cm_core::cli::run_launch(agent::AgentControl::Claude, args).await
        }
        Some(Commands::Codex { args }) => {
            cm_core::cli::run_launch(agent::AgentControl::Codex, args).await
        }
        Some(Commands::Hook { event, sock, agent }) => {
            cm_core::cli::run_hook(&agent, &event, sock.as_deref()).await
        }
        Some(Commands::AttachExited {
            host,
            token,
            status,
            held_secs,
        }) => {
            // A sentinel drop and nothing else — the dashboard's watcher on the
            // sessions dir turns it into a binding retirement. Deliberately not
            // a socket or a signal: this runs from a dying window's trap, so it
            // must not block, must not need the dashboard to be reachable, and
            // must survive the dashboard being restarted between the write and
            // the read.
            state::write_detach_report(&host, &token, status, held_secs);
            Ok(())
        }
        Some(Commands::Focus { window_id }) => {
            // The terminal instance this focus process *drives* (the active
            // backend's identity, honoring the `[terminal] backend` override —
            // not the ambient env). The bell and the focus below are both
            // scoped to it: Kitty window ids and zellij pane ids overlap, so a
            // window/binding from another terminal names a different
            // namespace's window and must not be driven here.
            let my_identity = terminal::get().identity();
            // Drop the sentinel *before* focusing so the bell is already in
            // place by the time the dashboard's fs watcher reacts and the
            // user lands on it.
            if let Some(wid) = &window_id {
                state::write_bell_flag_for_window(wid, my_identity.as_deref());
            }
            // Verify the dashboard is actually alive before issuing a focus.
            // After an unclean exit the recorded window id is stale, so
            // `kitten focus-window` would fail with a kitten error and a
            // non-zero exit; surface a clean message instead and clear the
            // stale sentinels best-effort.
            let dashboard_pid = std::fs::read_to_string(state::dashboard_pid_path())
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            let alive = dashboard_pid.is_some_and(state::is_process_alive);
            match (alive, app::read_dashboard_window_id()) {
                (true, Some((dash_identity, dwid))) => {
                    // Only drive the dashboard's window when it was recorded in
                    // this same terminal instance; otherwise its id belongs to a
                    // foreign namespace and focusing it could grab an unrelated
                    // window. Skip cleanly (the bell, if any, was still dropped).
                    if dash_identity.is_some() && dash_identity != my_identity {
                        eprintln!(
                            "Dashboard runs in another terminal ({}); not focusing from here",
                            dash_identity.as_deref().unwrap_or("")
                        );
                        Ok(())
                    } else {
                        terminal::get().focus_window(&dwid).await
                    }
                }
                _ => {
                    let _ = std::fs::remove_file(state::dashboard_window_id_path());
                    let _ = std::fs::remove_file(state::dashboard_pid_path());
                    eprintln!("No dashboard running");
                    std::process::exit(1);
                }
            }
        }
        None => app::run().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_terminal_dashboard_needs_terminal() {
        // The default (no subcommand) is the dashboard, which drives a terminal.
        assert!(requires_terminal(&None));
    }

    #[test]
    fn requires_terminal_hook_is_exempt() {
        // Hooks run wherever the agent runs — including a headless remote pool
        // with no terminal — so `miao hook` must not gate on one.
        assert!(!requires_terminal(&Some(Commands::Hook {
            event: "stop".into(),
            sock: None,
            agent: "claude".into(),
        })));
    }

    #[test]
    fn requires_terminal_plain_launcher_needs_terminal() {
        let cmd = Some(Commands::Claude {
            args: vec!["/work".into(), "--resume".into(), "sid".into()],
        });
        assert!(requires_terminal(&cmd));
    }

    #[test]
    fn requires_terminal_pooled_claude_is_exempt() {
        let cmd = Some(Commands::Claude {
            args: vec![
                "/work".into(),
                "--pool-session".into(),
                "cm-claude-7-1".into(),
            ],
        });
        assert!(!requires_terminal(&cmd));
    }
}
