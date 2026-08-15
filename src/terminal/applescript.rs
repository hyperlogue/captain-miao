//! What every AppleScript-driven backend needs: the transport, the two quoting
//! rules, and the shape of a startup control probe.
//!
//! macOS emulators expose no socket and no CLI — the control channel is Apple
//! events, reached by piping a script into `osascript`. That makes the *script*
//! the wire format, so the pieces that decide what a script says are the pieces
//! most worth having one copy of: a value spliced in wrong doesn't fail loudly,
//! it changes what the script means.
//!
//! Backend-specific by design, and deliberately not here: the id validator (each
//! app mints its own shapes) and `diagnose` (the failures a user hits differ per
//! app, and each message has to name that app's fix).

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

/// Field separator inside a snapshot line. Free text (a tab title) always comes
/// last, so a title containing this can't shift the id fields — the discipline
/// tmux's `SNAPSHOT_FORMAT` follows for the same reason.
pub(super) const SEP: char = '\u{1f}';

/// The preamble every multi-value script carries: bind the separator to a
/// variable *before* the `tell` block, then concatenate `sep` rather than a
/// literal.
///
/// Two reasons it can't be written inline. AppleScript string literals have no
/// `\x` escape — only `\n`, `\t`, `\r`, `\"` and `\\` — so U+001F cannot be
/// spelled in one at all. And the obvious readable alternative, AppleScript's
/// built-in `tab` constant, is precisely the wrong word here: inside a
/// `tell application` block the term `tab` resolves against the application's
/// own dictionary, where both Ghostty and iTerm2 use it for a *class*. Binding
/// outside the block sidesteps both.
pub(super) const SEP_PREAMBLE: &str = "set sep to character id 31\nset lf to character id 10\n";

/// How long a startup control probe waits before declaring the channel unusable.
///
/// Generous, because the failure it guards is *user-paced*: the first Apple event
/// captain-miao sends makes macOS put up an Automation (TCC) consent dialog, and
/// `osascript` blocks on it until someone clicks. Timing that out at a few
/// seconds would fail the startup check on the one run where the user is being
/// asked to make it work. A permitted app's answer is a single Apple event.
pub(super) const CONTROL_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a startup control probe observed. Split from the probe so each backend's
/// user-facing diagnosis is a pure function of the outcome — testable without a
/// mis-permissioned Mac to reproduce against (the shape kitty's `diagnose` uses).
#[derive(Debug, Clone, Copy)]
pub(super) enum ProbeOutcome<'a> {
    /// The probe went out but nothing came back inside [`CONTROL_PROBE_TIMEOUT`].
    TimedOut,
    /// `osascript` ran and failed, with this error text.
    Failed { err: &'a str },
}

/// Quote `s` as an AppleScript string literal, escaping the two characters that
/// can end or continue one (`"` and `\`) and dropping the two that cannot appear
/// in one at all (CR and LF — AppleScript has no multi-line literal, so a raw
/// newline is a syntax error rather than a quoted character).
///
/// Every value captain-miao splices into a script goes through here or through
/// the backend's own id validator. The values are not hostile by nature — a cwd,
/// a tab title, a tty — but they are user text reaching a *parser*, and an
/// unescaped quote would at best fail the call and at worst change what the
/// script says.
pub(super) fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => {}
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Quote one argv element for `/bin/sh`. Single-quote wrapping, with the one
/// escape a single-quoted shell word admits (`'` → `'\''`), so the result is
/// safe for any byte sequence a path or a flag can hold.
///
/// This is POSIX quoting for a POSIX shell, and it must only ever be spent on
/// one: Ghostty hands its `command` to `/bin/sh -c`, and the iTerm2 backend
/// builds a script that a real `/bin/sh` reads. iTerm2's *own* command
/// tokenizer is a different language that this would be wrong for — see
/// `iterm::spawn_payload`.
pub(super) fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:=@,+".contains(&b))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Run `script` through `osascript`, returning its stdout.
///
/// The script is fed on **stdin** rather than `-e`: these scripts run to a dozen
/// lines and `osascript -` takes one whole, so there is no per-line argv assembly
/// to get wrong.
pub(super) async fn osascript(script: &str) -> Result<String> {
    let mut child = Command::new("osascript")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to run osascript")?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().context("osascript stdin unavailable")?;
        stdin.write_all(script.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "osascript failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_strings_close_over_their_own_content() {
        assert_eq!(applescript_string("plain"), "\"plain\"");
        assert_eq!(applescript_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(applescript_string("back\\slash"), "\"back\\\\slash\"");
        // AppleScript has no multi-line literal, so a newline is dropped rather
        // than escaped — it would otherwise end the statement mid-string.
        assert_eq!(applescript_string("a\nb\r\nc"), "\"abc\"");
    }

    #[test]
    fn shell_quoting_survives_a_quote_of_its_own() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("/home/miao/a-b_c.d"), "/home/miao/a-b_c.d");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    /// The separator has to be *bound*, never written into a literal:
    /// AppleScript strings have no `\x` escape at all, and its built-in `tab`
    /// constant is shadowed by the terminal's own `tab` class inside a `tell`
    /// block. Both mistakes produce a script that compiles as something else
    /// rather than one that fails loudly.
    #[test]
    fn the_separator_preamble_binds_rather_than_spells() {
        assert!(!SEP_PREAMBLE.contains(SEP));
        assert!(!SEP_PREAMBLE.contains("\\x"));
        assert!(SEP_PREAMBLE.contains("set sep to character id 31"));
        assert!(!SEP_PREAMBLE.contains("tell application"));
    }
}
