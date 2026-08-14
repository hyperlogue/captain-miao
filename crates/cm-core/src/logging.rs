//! Tracing setup shared by both binaries. Routes logs to files under the state
//! dir (never stderr, which the launcher shares with the agent's TUI and the
//! dashboard takes over via the alt-screen) — with one documented exception,
//! [`init_stderr_tracing`].

use crate::{config, state};

/// Initialize tracing straight to **stderr**, for a role whose stderr is already
/// a dedicated file its parent opened for it.
///
/// This inverts the rule in the module doc, and only one role qualifies: the
/// clipboard server, a child the dashboard spawns with `stderr` pointed at
/// `logs/clipboard-serve.log`. Every other role shares stderr with something
/// that would be corrupted by it — the launcher with the agent's TUI, the
/// dashboard with its own alt-screen — which is why they get files of their own
/// and this one does not need to.
///
/// The parent truncates that file on each spawn, so `DEBUG` is affordable: a
/// paste is a couple of lines, and the log resets with the dashboard.
pub fn init_stderr_tracing(role: &str) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let initialized = tracing_subscriber::registry()
        .with(log_filter(std::env::var("RUST_LOG").ok()))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .try_init()
        .is_ok();
    if initialized {
        tracing::info!(
            target: "captain_miao::launch",
            "===== {} START pid={} =====",
            role.to_uppercase(),
            std::process::id()
        );
    }
}

/// Initialize tracing for one of: "launcher", "dashboard", "hook", "daemon". The
/// launcher always gets its own per-pid file (so its routine logs don't drown
/// out the shared sink). When `[debug]` mode is on, every role also appends to a
/// single shared `debug.log` so events from multiple processes can be correlated.
/// A "launch separator" line is emitted right after init so a reader can find
/// where each process started.
pub fn init_tracing(role: &str) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // The launcher shares stderr with the Claude process it spawns
    // (Stdio::inherit), so any log written to stderr would paint over Claude's
    // TUI. Route tracing to files under the state dir instead.
    // Owner-only: with `[debug]` on these logs carry prompt text and command
    // lines (see `state::create_dir_all_private`).
    let log_dir = state::state_dir().join("logs");
    if state::create_dir_all_private(&log_dir).is_err() {
        return;
    }

    // Per-pid launcher log layer (existing behavior).
    let launcher_layer = if role == "launcher" {
        sweep_dead_launcher_logs(&log_dir);
        let log_path = log_dir.join(format!("launcher-{}.log", std::process::id()));
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok()
            .map(|f| {
                tracing_subscriber::fmt::layer()
                    .with_writer(std::sync::Mutex::new(f))
                    .with_ansi(false)
            })
    } else {
        None
    };

    // NB: the daemon deliberately does NOT install its own subscriber here. With
    // the pty-pool feature it runs the libshpool daemon on a thread, and libshpool
    // installs the *global* tracing subscriber itself — a second `set_global`
    // would panic (killing the pool thread). libshpool's subscriber captures our
    // `captain_miao::*` events too, and the daemon's stdio is redirected to
    // `daemon.log`, so the daemon's logs land there without a layer of our own.

    // Shared debug.log layer for every role when debug mode is on.
    let debug_layer = if config::debug_enabled() {
        let path = log_dir.join(&config::get().debug.log_file);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|f| {
                tracing_subscriber::fmt::layer()
                    .with_writer(std::sync::Mutex::new(f))
                    .with_ansi(false)
            })
    } else {
        None
    };

    if launcher_layer.is_none() && debug_layer.is_none() {
        return;
    }

    let initialized = tracing_subscriber::registry()
        .with(log_filter(std::env::var("RUST_LOG").ok()))
        .with(launcher_layer)
        .with(debug_layer)
        .try_init()
        .is_ok();
    if !initialized {
        return;
    }

    // Launch separator. Across-process appends to debug.log are atomic for
    // POSIX writes <= PIPE_BUF (4 KiB), so this line lands intact even when
    // multiple processes start near-simultaneously.
    tracing::info!(
        target: "captain_miao::launch",
        "===== {} START pid={} =====",
        role.to_uppercase(),
        std::process::id()
    );
}

/// The level filter for our own log files, from `RUST_LOG` plus two pinned
/// directives.
///
/// Both crate roots are pinned: most launcher/hook events carry an explicit
/// `target: "captain_miao::…"`, but any plain `tracing::debug!` in cm-core
/// defaults to its own module path (`cm_core::…`), and the crate split left
/// those silently filtered out — including the watcher/transcript diagnostics.
///
/// [`Targets`] rather than `EnvFilter`: both parse the same `target=level`
/// `RUST_LOG` syntax, but `EnvFilter` additionally supports span-field
/// predicates (`span[field=value]`), which nothing here uses and which cost a
/// whole regex engine in the dependency tree (matchers → regex-automata →
/// regex-syntax). An unparseable `RUST_LOG` falls back to the two directives
/// rather than to silence, since these files are the only place this process
/// logs at all.
///
/// [`Targets`]: tracing_subscriber::filter::Targets
fn log_filter(rust_log: Option<String>) -> tracing_subscriber::filter::Targets {
    rust_log
        .and_then(|v| v.parse::<tracing_subscriber::filter::Targets>().ok())
        .unwrap_or_default()
        .with_target("captain_miao", tracing::Level::DEBUG)
        .with_target("cm_core", tracing::Level::DEBUG)
}

/// Remove `launcher-{pid}.log` files for launchers that have exited. Runs
/// on every launcher startup so logs don't accumulate indefinitely, while
/// logs for active (or very recently crashed) launchers stay available for
/// inspection until the next one starts.
fn sweep_dead_launcher_logs(log_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid_str) = name
            .strip_prefix("launcher-")
            .and_then(|s| s.strip_suffix(".log"))
        else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if !state::is_process_alive(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn our_two_crate_roots_always_log_at_debug() {
        let f = log_filter(None);
        assert!(f.would_enable("captain_miao", &Level::DEBUG));
        assert!(f.would_enable("captain_miao::launch", &Level::DEBUG));
        assert!(f.would_enable("cm_core::launcher", &Level::DEBUG));
        // Nothing else, so a chatty dependency can't fill the log by default.
        assert!(!f.would_enable("notify::inotify", &Level::ERROR));
    }

    #[test]
    fn rust_log_widens_but_never_narrows_our_own_targets() {
        // A directive for someone else is honoured…
        let f = log_filter(Some("notify=trace".into()));
        assert!(f.would_enable("notify::inotify", &Level::TRACE));
        assert!(f.would_enable("cm_core::launcher", &Level::DEBUG));

        // …and one aimed at us loses to the pinned directives, exactly as the
        // `EnvFilter::add_directive` calls this replaced did.
        let f = log_filter(Some("captain_miao=error".into()));
        assert!(f.would_enable("captain_miao::launch", &Level::DEBUG));
    }

    #[test]
    fn an_unparseable_rust_log_falls_back_rather_than_silencing_us() {
        // EnvFilter's `span[field=value]` syntax is the thing `Targets` won't
        // take; a user carrying one in their environment must not lose our logs.
        let f = log_filter(Some("captain_miao[request]=debug".into()));
        assert!(f.would_enable("captain_miao::launch", &Level::DEBUG));
        assert!(f.would_enable("cm_core::launcher", &Level::DEBUG));
    }
}
