//! Kitty backend: wraps `kitten @` remote control.
//!
//! Transport is a `kitten @ --to <socket> --password-env …` subprocess per call.
//! The password is passed out-of-band (env + `--password-env`) rather than as an
//! argv element so it isn't exposed through `ps` / `/proc/<pid>/cmdline`.

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

/// Title of the shared stack tab that hosts every session in the Stacked layout
/// — the Kitty analog of zellij's `cm:sessions` floating tab. Looked up by title
/// on each Stacked spawn (`SpawnTarget::SharedStackTab`) and created on first use.
const SESSIONS_TAB: &str = "cm:sessions";

pub struct KittyTerminal;

async fn kitten_cmd(args: &[&str]) -> Result<String> {
    let listen_on = std::env::var("KITTY_LISTEN_ON").context("KITTY_LISTEN_ON not set")?;

    let output = Command::new("kitten")
        .arg("@")
        .arg("--to")
        .arg(&listen_on)
        .arg("--password-env")
        .arg(RC_PASSWORD_ENV)
        .env(RC_PASSWORD_ENV, &config::get().kitty.rc_password)
        .args(args)
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
        // `cm:sessions` stack tab (`SharedStackTab`): join it if it exists, else
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
                // fixed-titled `cm:sessions`, unlike a per-project NewTab).
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
        // existing `cm:sessions` tab, whose id came out of the lookup snapshot
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
    use super::match_id;

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
