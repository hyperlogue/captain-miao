//! Terminal behavior and home resolution shared by both execution modes.
use std::path::{Path, PathBuf};

/// The executable this backend drives — see [`crate::agents::claude::BIN`].
pub(crate) const BIN: &str = "codex";

/// Codex's composer recognizes a bracketed paste containing an image path as
/// an attachment, retaining the current draft. Verified against 0.153.4's
/// `ChatComposer::handle_paste_image_path` and the real TUI. A file URL keeps
/// whitespace, quotes, non-UTF-8 bytes and terminal controls out of the input
/// stream; Codex decodes it back to a host path before reading the image.
pub(crate) fn clipboard_paste_input(path: &Path) -> Vec<u8> {
    use std::fmt::Write;
    use std::os::unix::ffi::OsStrExt;

    let mut input = String::from("\x1b[200~file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            input.push(char::from(byte));
        } else {
            let _ = write!(input, "%{byte:02X}");
        }
    }
    input.push_str("\x1b[201~");
    input.into_bytes()
}

/// Codex 0.153.4 pushes keyboard enhancements before querying support, on the
/// primary screen and regardless of TERM. Mirror its opt-out at launch, where
/// the environment is still the agent's; an attaching client cannot recover
/// that decision from its own environment. See upstream `tui/keyboard_modes.rs`.
pub(crate) fn uses_kitty_keyboard() -> bool {
    keyboard_enhancement_enabled(
        std::env::var("CODEX_TUI_DISABLE_KEYBOARD_ENHANCEMENT")
            .ok()
            .as_deref(),
        running_in_vscode_wsl,
    )
}

fn keyboard_enhancement_enabled(disable: Option<&str>, vscode_wsl: impl FnOnce() -> bool) -> bool {
    match disable.map(str::trim) {
        Some(value)
            if value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes") =>
        {
            false
        }
        Some(value)
            if value == "0"
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no") =>
        {
            true
        }
        _ => !vscode_wsl(),
    }
}

fn running_in_vscode_wsl() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let wsl = std::fs::read_to_string("/proc/version")
        .ok()
        .is_some_and(|version| {
            let version = version.to_ascii_lowercase();
            version.contains("microsoft") || version.contains("wsl")
        })
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some();
    if !wsl {
        return false;
    }
    if std::env::var("TERM_PROGRAM").is_ok_and(|term| term.eq_ignore_ascii_case("vscode")) {
        return true;
    }
    // WSL interop can hide TERM_PROGRAM from the Linux environment. Codex
    // also checks the Windows side; do so only at launch and only under WSL.
    std::process::Command::new("cmd.exe")
        .args(["/d", "/s", "/c", "set TERM_PROGRAM"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.trim_end_matches('\r')
                    .strip_prefix("TERM_PROGRAM=")
                    .is_some_and(|term| term.eq_ignore_ascii_case("vscode"))
            })
        })
}

/// The common keyboard flags Codex requests on every supported transport:
/// disambiguate escape codes + report alternate keys (`CSI > 5 u`). Avoid
/// report-event-types: Codex itself omits it for Ghostty, iTerm2 and tmux's
/// xterm key format, and a reattach can land in a different emulator. This
/// preserves modified keys without reproducing Codex's terminal detection or
/// querying a tmux server on the attach path. Verified against 0.153.4 startup
/// bytes and `tui/keyboard_modes.rs`; no mouse tracking belongs to its inline
/// view. Reset modifyOtherKeys so it cannot compete with CSI-u reporting.
pub(crate) fn reattach_prime(kitty_keyboard: bool) -> crate::state::ReattachPrime {
    crate::state::ReattachPrime {
        input_modes: true,
        keyboard_flags: if kitty_keyboard { 5 } else { 0 },
        reset_modify_other_keys: kitty_keyboard,
        ..crate::state::ReattachPrime::default()
    }
}

/// The real Codex home — `$CODEX_HOME` if the user set one globally, else
/// `~/.codex`. Resolve a relative override now and hand the same absolute path
/// back to Codex at launch, so creating the profile and loading it cannot
/// disagree when the agent's cwd differs from the launcher's.
pub(super) fn codex_home() -> Option<PathBuf> {
    resolve_codex_home(
        std::env::var_os("CODEX_HOME").map(PathBuf::from),
        dirs::home_dir(),
        std::env::current_dir().ok(),
    )
}

/// Home resolution split from environment reads so it is testable without
/// mutating process-global variables. Empty overrides are unset; relative ones
/// resolve against the launcher's cwd and require that cwd to be available.
fn resolve_codex_home(
    configured: Option<PathBuf>,
    home: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = configured
        && !path.as_os_str().is_empty()
    {
        return if path.is_absolute() {
            Some(path)
        } else {
            Some(cwd?.join(path))
        };
    }
    home.map(|h| h.join(".codex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_paste_encodes_a_path_without_terminal_or_shell_syntax() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(
            b"/work/a b/'\"\x1b[201~\n\xff.png",
        ));
        assert_eq!(
            clipboard_paste_input(path),
            b"\x1b[200~file:///work/a%20b/%27%22%1B%5B201~%0A%FF.png\x1b[201~"
        );
    }

    #[test]
    fn keyboard_reattach_honors_codex_opt_out() {
        for value in ["1", "true", "YES", " True "] {
            assert!(!keyboard_enhancement_enabled(Some(value), || false));
        }
        for value in ["0", "false", "NO", " False "] {
            assert!(keyboard_enhancement_enabled(Some(value), || true));
        }
        for value in [None, Some(""), Some("unexpected")] {
            assert!(keyboard_enhancement_enabled(value, || false));
            assert!(!keyboard_enhancement_enabled(value, || true));
        }
    }

    #[test]
    fn codex_home_resolves_overrides_without_global_env_mutation() {
        let cwd = PathBuf::from("/work");
        assert_eq!(
            resolve_codex_home(
                Some(PathBuf::from("relative-home")),
                Some(PathBuf::from("/users/me")),
                Some(cwd)
            ),
            Some(PathBuf::from("/work/relative-home"))
        );
        assert_eq!(
            resolve_codex_home(
                Some(PathBuf::from("/custom/codex")),
                Some(PathBuf::from("/users/me")),
                None
            ),
            Some(PathBuf::from("/custom/codex"))
        );
        assert_eq!(
            resolve_codex_home(Some(PathBuf::new()), Some(PathBuf::from("/users/me")), None),
            Some(PathBuf::from("/users/me/.codex"))
        );
    }
}
