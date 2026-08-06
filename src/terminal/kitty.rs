//! Kitty backend: wraps `kitten @` remote control.
//!
//! Transport is a `kitten @ --to <socket> --password-env …` subprocess per call.
//! The password is passed out-of-band (env + `--password-env`) rather than as an
//! argv element so it isn't exposed through `ps` / `/proc/<pid>/cmdline`.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

use super::{
    SpawnCommand, SpawnResult, SpawnSpec, SpawnTarget, Tab, TabId, TabTarget, Terminal, WindowId,
    tail_lines,
};
use crate::config;

/// Env var carrying the Kitty remote-control password to the `kitten @` child.
const RC_PASSWORD_ENV: &str = "CAPTAIN_MIAO_RC_PASSWORD";

/// How long [`verify_control`](Terminal::verify_control)'s probe waits for kitty
/// to answer before declaring the channel unusable. Generous beside a healthy
/// `kitten @ ls` (~20ms), because the failure it guards is *unbounded*: with
/// `allow_remote_control password`, a password kitty doesn't accept makes it ask
/// the user for permission **in its own window** rather than reply, so the
/// request never returns on its own (verified against kitty 0.47).
const CONTROL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Title of the shared stack tab that hosts every session in the Stacked layout
/// — the Kitty analog of zellij's `miao:sessions` floating tab. Looked up by title
/// on each Stacked spawn (`SpawnTarget::SharedStackTab`) and created on first use.
const SESSIONS_TAB: &str = "miao:sessions";

pub struct KittyTerminal;

/// The socket `kitten @` talks to, from the env kitty exports into its windows.
/// `None` when kitty isn't listening on one (no `listen_on` in kitty.conf), in
/// which case remote control would fall back to the in-terminal escape
/// channel — which the dashboard can't use: it would write control sequences
/// into its own alt-screen. An empty value is treated as unset.
fn listen_on() -> Option<String> {
    std::env::var("KITTY_LISTEN_ON")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Build the `kitten @ <args>` invocation, with the socket and the out-of-band
/// password wired in. Split from [`kitten_cmd`] so the startup probe can impose
/// a timeout by dropping the future: `kill_on_drop` is what makes that reap the
/// child instead of leaving an orphaned `kitten` blocked on kitty's permission
/// prompt. Every other call site awaits to completion, where it's inert.
fn kitten_command(args: &[&str]) -> Result<Command> {
    let listen_on = listen_on().context("KITTY_LISTEN_ON not set")?;

    let mut cmd = Command::new("kitten");
    cmd.arg("@")
        .arg("--to")
        .arg(&listen_on)
        .arg("--password-env")
        .arg(RC_PASSWORD_ENV)
        .env(RC_PASSWORD_ENV, &config::get().kitty.rc_password)
        .args(args)
        .kill_on_drop(true);
    Ok(cmd)
}

async fn kitten_cmd(args: &[&str]) -> Result<String> {
    let output = kitten_command(args)?
        .output()
        .await
        .context("Failed to run kitten")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "kitten @ {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Validate a window/tab id before it is interpolated into a kitty `--match`
/// value (`id:<n>` / `window_id:<n>` / `--target-tab id:<n>`).
///
/// kitty's match mini-language *parses* that value — it supports `field:query`
/// pairs, boolean `and`/`or`/`not`, and regex queries — so a string argv element
/// like `1 or title:.*`, though passed as a single arg (no shell is involved),
/// is read by kitty as a broadened match. A tampered id could thereby close the
/// wrong window, or leak another window's scrollback into the preview via
/// `get-text`. Real kitty ids are always non-negative integers and the only
/// untrusted source is captain-miao's own state files, so reject anything that
/// isn't pure ASCII digits and fail the operation closed rather than mis-target.
fn match_id(id: &str) -> Result<&str> {
    super::validate_id(id, "kitty")
}

// ---- startup control check ----

/// What the startup control probe observed. Split from the probe itself so the
/// user-facing diagnosis below is a *pure* function of the outcome — testable
/// without a misconfigured kitty to reproduce against.
#[derive(Debug, Clone, Copy)]
enum ProbeOutcome<'a> {
    /// Kitty exports no socket to talk to (`KITTY_LISTEN_ON` unset/empty).
    NoSocket,
    /// The request went out but nothing came back inside
    /// [`CONTROL_PROBE_TIMEOUT`].
    TimedOut { socket: &'a str },
    /// `kitten @ ls` ran and failed, with this error text.
    Failed { socket: &'a str, err: &'a str },
}

/// The kitty.conf + config.toml lines a working setup needs. Appended to every
/// diagnosis that is actually a configuration problem (i.e. all but a missing
/// `kitten` binary), since the fix is the same block in each case.
const SETUP_HINT: &str = "\
Remote control must be enabled over a socket in kitty.conf:

    allow_remote_control password
    remote_control_password \"choose-your-own-secret\"
    listen_on unix:/tmp/mykitty

and [kitty] rc_password in captain-miao's config.toml must be that same secret.
kitty only opens the socket at startup, so restart kitty after editing it.";

/// Turn a failed probe into an actionable message: what is broken, then the fix.
/// The dashboard prints this and exits, so it is the only chance the user gets
/// to be told *which* half of the setup (kitty.conf vs config.toml) is wrong.
fn diagnose(outcome: ProbeOutcome<'_>) -> String {
    let problem = match outcome {
        ProbeOutcome::NoSocket => "KITTY_LISTEN_ON is not set, so kitty is not listening on a \
             remote-control socket (or this window predates the setting)."
            .to_string(),
        ProbeOutcome::TimedOut { socket } => format!(
            "kitty at {socket} did not answer within {}s.\n\nThat is what a password kitty does \
             not accept looks like: rather than refuse, kitty asks the user to approve the \
             request in its own window, so the request never returns. Check that [kitty] \
             rc_password matches remote_control_password in kitty.conf.",
            CONTROL_PROBE_TIMEOUT.as_secs()
        ),
        ProbeOutcome::Failed { socket, err } => {
            let lower = err.to_ascii_lowercase();
            // Ordered most-specific first: the missing-binary case carries our
            // own "Failed to run kitten" context and would otherwise be caught
            // by the connect arm (both mention "no such file or directory").
            if lower.contains("failed to run kitten") {
                return format!(
                    "Kitty remote control check failed: the `kitten` binary could not be run \
                     ({err}).\n\nIt ships with kitty — put it on captain-miao's PATH."
                );
            } else if lower.contains("failed to connect") || lower.contains("connection refused") {
                format!(
                    "nothing is listening on {socket} ({err}).\n\nThe socket belongs to the kitty \
                     instance that exported it; if kitty has been restarted since this window was \
                     created, start captain-miao from a window of the current instance."
                )
            } else {
                format!("kitty at {socket} rejected the request: {err}")
            }
        }
    };
    format!("Kitty remote control check failed: {problem}\n\n{SETUP_HINT}")
}

#[async_trait]
impl Terminal for KittyTerminal {
    fn current_window(&self) -> Option<WindowId> {
        // The env read lives in cm-core (so the launcher can self-report its
        // window without a backend); delegate to it here.
        super::current_window()
    }

    fn identity(&self) -> Option<String> {
        // The instance this backend drives is the kitty behind
        // KITTY_LISTEN_ON (every `kitten @` call targets that socket).
        cm_core::terminal::kitty_identity(
            std::env::var("KITTY_LISTEN_ON").ok(),
            std::env::var("KITTY_PID").ok(),
        )
    }

    /// Prove the `kitten @` channel to *this* kitty actually works, by making
    /// the cheapest real request there is (`ls` — the same call `snapshot` runs,
    /// so a pass means every other rc call has a working transport).
    ///
    /// The timeout is not belt-and-braces: a password kitty doesn't accept
    /// produces no reply at all (it prompts the user in its own window), so the
    /// probe must bound its own wait or the check would hang exactly where the
    /// misconfiguration it looks for hangs.
    async fn verify_control(&self) -> Result<()> {
        let Some(socket) = listen_on() else {
            anyhow::bail!("{}", diagnose(ProbeOutcome::NoSocket));
        };
        match tokio::time::timeout(CONTROL_PROBE_TIMEOUT, kitten_cmd(&["ls"])).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                // `{e:#}` flattens the anyhow chain onto one line — the context
                // ("Failed to run kitten") is what `diagnose` classifies on.
                let err = format!("{e:#}");
                anyhow::bail!(
                    "{}",
                    diagnose(ProbeOutcome::Failed {
                        socket: &socket,
                        err: &err
                    })
                );
            }
            Err(_elapsed) => {
                anyhow::bail!("{}", diagnose(ProbeOutcome::TimedOut { socket: &socket }))
            }
        }
    }

    async fn snapshot(&self) -> Result<Vec<Tab>> {
        let stdout = kitten_cmd(&["ls"]).await?;
        let data: serde_json::Value = serde_json::from_str(&stdout)?;
        let mut tabs = Vec::new();
        for oswin in data.as_array().unwrap_or(&vec![]) {
            for tab in oswin["tabs"].as_array().unwrap_or(&vec![]) {
                // Skip a tab with no integer id rather than coalescing to 0 —
                // a fabricated id 0 would collide every id-less tab in
                // `window_tab_map` and make it a bogus move-to-tab target.
                let Some(tab_id) = tab["id"].as_u64() else {
                    continue;
                };
                let id = TabId::from(tab_id);
                let title = tab["title"].as_str().unwrap_or("").to_string();
                let is_focused = tab["is_focused"].as_bool().unwrap_or(false);
                let mut windows = Vec::new();
                for win in tab["windows"].as_array().unwrap_or(&vec![]) {
                    let Some(wid) = win["id"].as_u64() else {
                        continue;
                    };
                    windows.push(WindowId::from(wid));
                }
                tabs.push(Tab {
                    id,
                    title,
                    is_focused,
                    windows,
                });
            }
        }
        Ok(tabs)
    }

    async fn spawn(&self, spec: SpawnSpec) -> Result<SpawnResult> {
        // The Stacked layout on Kitty puts every session in one shared
        // `miao:sessions` stack tab (`SharedStackTab`): join it if it exists, else
        // create it. Finding it needs a snapshot (`launch` prints only a window
        // id). We match on a *window* already in that tab, not the tab id: `-m
        // window_id:<w>` selects the target tab by a window it contains (the same
        // proven match the old AdjacentTo spawn used), sidestepping the
        // context-dependence of a bare `id:` in kitty's match language. The tab's
        // own id rides along for `SpawnResult.tab` — the snapshot already yielded
        // it, so reporting it saves the caller a second `ls` to learn where the
        // window landed. Only computed for that target.
        let shared_tab: Option<(TabId, WindowId)> =
            if matches!(spec.target, SpawnTarget::SharedStackTab) {
                self.snapshot().await.ok().and_then(|tabs| {
                    tabs.into_iter()
                        .find(|t| t.title.as_str() == SESSIONS_TAB)
                        .and_then(|t| {
                            let id = t.id.clone();
                            t.windows.into_iter().next().map(|w| (id, w))
                        })
                })
            } else {
                None
            };

        // `--type`, plus whether this spawn creates a fresh tab (which we then
        // default to the stack layout).
        let (window_type, creates_tab) = match &spec.target {
            SpawnTarget::NewTab => ("tab", true),
            // Only produced when `floating_sessions` is set, which kitty
            // never claims — reaching here is a policy bug upstream.
            SpawnTarget::Floating => {
                anyhow::bail!("floating session panes are not supported by the kitty backend")
            }
            // Join the shared tab as a window; create it as a tab if absent.
            SpawnTarget::SharedStackTab => {
                if shared_tab.is_some() {
                    ("window", false)
                } else {
                    ("tab", true)
                }
            }
        };
        let mut args: Vec<String> = vec![
            "launch".into(),
            format!("--type={window_type}"),
            format!("--cwd={}", spec.cwd),
        ];
        if spec.hold {
            args.push("--hold".into());
        }
        // `--dont-take-focus` keeps the caller (the dashboard) focused; omit it
        // when the spawn should pull focus (e.g. an interactive shell tab).
        if !spec.take_focus {
            args.push("--dont-take-focus".into());
        }
        match &spec.target {
            SpawnTarget::NewTab => {
                if let Some(title) = &spec.title {
                    args.push(format!("--tab-title={title}"));
                }
            }
            SpawnTarget::SharedStackTab => {
                // Label the window with the per-session title (the tab stays
                // fixed-titled `miao:sessions`, unlike a per-project NewTab).
                if let Some(title) = &spec.title {
                    args.push(format!("--window-title={title}"));
                }
                match &shared_tab {
                    // Join the existing shared tab, selected by a window it
                    // contains (`-m window_id:` — kitty launches the new window
                    // in that window's tab). The tab is already a stack layout,
                    // so the new window stacks in.
                    Some((_, w)) => {
                        let w = match_id(w.as_str())?;
                        args.push("-m".into());
                        args.push(format!("window_id:{w}"));
                    }
                    // Create the shared tab, fixed-titled so it's found next time.
                    None => args.push(format!("--tab-title={SESSIONS_TAB}")),
                }
            }
            SpawnTarget::Floating => unreachable!("rejected above"),
        }
        if let Ok(path) = std::env::var("PATH") {
            args.push(format!("--env=PATH={path}"));
        }
        if let SpawnCommand::Exec(cmd) = &spec.command {
            for c in cmd {
                args.push(c.clone());
            }
        }
        // SpawnCommand::Shell appends no argv — kitty launches the default shell.

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let stdout = kitten_cmd(&arg_refs).await?;
        let window_id: WindowId = stdout
            .trim()
            .parse::<u64>()
            .context("Failed to parse window ID from launch output")?
            .into();

        // A fresh tab starts with one window; default it to the stack layout so
        // that as more windows get added to the tab they stack — one full-size
        // window visible at a time — instead of tiling. A `NewTab` honors
        // `spec.stack`; a `SharedStackTab` we just created is always stacked
        // (that's the arrangement). Best-effort and non-fatal: kitty's remote
        // control can only switch to a layout in `enabled_layouts` (the default
        // `*` includes stack); if the user disabled stack, goto-layout is a
        // no-op and the spawn must still succeed, so the error is swallowed.
        let stack_new_tab =
            creates_tab && (spec.stack || matches!(spec.target, SpawnTarget::SharedStackTab));
        if stack_new_tab {
            let m = format!("window_id:{window_id}");
            if let Err(e) = kitten_cmd(&["goto-layout", "-m", &m, "stack"]).await {
                tracing::debug!(
                    "goto-layout stack on new tab failed (stack likely not enabled): {e}"
                );
            }
        }

        // `launch` prints only the window id, so the tab is reported only where
        // we already know it for free: a `SharedStackTab` spawn that *joined* an
        // existing `miao:sessions` tab, whose id came out of the lookup snapshot
        // above. A spawn that created a tab (`NewTab`, or the first
        // `SharedStackTab`) would need a second `ls` to learn it — not worth it,
        // so the caller falls back to resolving that one from a later snapshot.
        // (`shared_tab.is_some()` is exactly the "joined, didn't create" case —
        // it's what set `creates_tab` false above.)
        Ok(SpawnResult {
            window: Some(window_id),
            tab: shared_tab.map(|(t, _)| t),
        })
    }

    async fn focus_window(&self, id: &WindowId) -> Result<()> {
        let m = format!("id:{}", match_id(id.as_str())?);
        kitten_cmd(&["focus-window", "--match", &m]).await?;
        Ok(())
    }

    async fn focus_tab(&self, id: &TabId) -> Result<()> {
        let m = format!("id:{}", match_id(id.as_str())?);
        kitten_cmd(&["focus-tab", "--match", &m]).await?;
        Ok(())
    }

    async fn close_window(&self, id: &WindowId) -> Result<()> {
        let m = format!("id:{}", match_id(id.as_str())?);
        kitten_cmd(&["close-window", "--match", &m]).await?;
        Ok(())
    }

    async fn capture_text(&self, id: &WindowId, max_lines: usize) -> Result<String> {
        // `--extent screen` honors the source window's scrollback position, so a
        // scrolled-up source would yield a stale view; `all` always ends at the
        // live bottom. Kitty has no "last N lines" flag, so we fetch the full
        // in-memory scrollback (bounded by `scrollback_lines`) and tail it.
        let m = format!("id:{}", match_id(id.as_str())?);
        let raw = kitten_cmd(&["get-text", "--match", &m, "--ansi", "--extent", "all"]).await?;
        Ok(tail_lines(&raw, max_lines).to_string())
    }

    async fn move_window_to_tab(&self, id: &WindowId, to: TabTarget) -> Result<()> {
        let m = format!("id:{}", match_id(id.as_str())?);
        let target = match to {
            TabTarget::New => "new".to_string(),
            TabTarget::Existing(tab) => format!("id:{}", match_id(tab.as_str())?),
        };
        kitten_cmd(&["detach-window", "--match", &m, "--target-tab", &target]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ProbeOutcome, diagnose, match_id};

    /// Every configuration failure has to name the config block that fixes it —
    /// the dashboard prints this once and exits, so a message that only says
    /// "remote control failed" strands the user.
    #[test]
    fn diagnosis_carries_the_setup_fix() {
        for outcome in [
            ProbeOutcome::NoSocket,
            ProbeOutcome::TimedOut {
                socket: "unix:/tmp/mykitty",
            },
            ProbeOutcome::Failed {
                socket: "unix:/tmp/mykitty",
                err: "kitten @ ls failed: something unexpected",
            },
        ] {
            let msg = diagnose(outcome);
            assert!(msg.contains("allow_remote_control"), "{msg}");
            assert!(msg.contains("rc_password"), "{msg}");
        }
    }

    #[test]
    fn diagnosis_names_the_specific_cause() {
        // No socket: kitty.conf is missing `listen_on` (or the window predates it).
        assert!(diagnose(ProbeOutcome::NoSocket).contains("KITTY_LISTEN_ON"));

        // A hang is the *password* symptom, not a slow kitty, so the timeout
        // message has to point at the password rather than suggest retrying.
        let timed_out = diagnose(ProbeOutcome::TimedOut {
            socket: "unix:/tmp/mykitty",
        });
        assert!(timed_out.contains("remote_control_password"), "{timed_out}");

        // A missing binary is the one failure the kitty.conf block can't fix, so
        // it gets its own message (and must not be mistaken for the connect
        // failure below — both mention "no such file or directory").
        let no_kitten = diagnose(ProbeOutcome::Failed {
            socket: "unix:/tmp/mykitty",
            err: "Failed to run kitten: No such file or directory (os error 2)",
        });
        assert!(no_kitten.contains("PATH"), "{no_kitten}");
        assert!(!no_kitten.contains("allow_remote_control"), "{no_kitten}");

        // A dead socket is usually a restarted kitty, not a config error.
        let dead_socket = diagnose(ProbeOutcome::Failed {
            socket: "unix:/tmp/kitty-715",
            err: "kitten @ ls failed: Error: Failed to connect to unix:/tmp/kitty-715 with \
                  error: dial unix /tmp/kitty-715: connect: no such file or directory",
        });
        assert!(dead_socket.contains("unix:/tmp/kitty-715"), "{dead_socket}");
        assert!(dead_socket.contains("restarted"), "{dead_socket}");
    }

    #[test]
    fn match_id_accepts_only_digits() {
        assert_eq!(match_id("123").unwrap(), "123");
        // Empty, and anything that could steer kitty's match mini-language, is
        // rejected so it can never broaden/redirect a `--match`.
        assert!(match_id("").is_err());
        assert!(match_id("1 or title:.*").is_err());
        assert!(match_id("1 and recent:0").is_err());
        assert!(match_id("-1").is_err());
        assert!(match_id("12a").is_err());
        assert!(match_id("id:1").is_err());
    }
}
