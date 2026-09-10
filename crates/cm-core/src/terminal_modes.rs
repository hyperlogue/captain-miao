//! Live terminal modes for pooled reattachment, independent of agent identity.
//!
//! The pool's output thread owns this bounded parser and feeds every PTY byte,
//! including output while detached. Restoration precedes subsequent live bytes
//! on that same thread, so there is no snapshot/read/attach race. Text, screen
//! cells, OSC payloads (including clipboard data), and graphics are not stored.
//!
//! This recognizes the seven-bit ECMA-48 sequences emitted by our agent TUIs:
//! alternate-screen selection, DEC input modes, application keypad, xterm
//! modifyOtherKeys, and kitty keyboard stacks. Unrecognized controls pass
//! through the relay unchanged but are never replayed. CSI prefixes and stacks
//! are bounded; string payloads are discarded as they arrive.
//!
//! Keyboard stacks are independent per screen, including their saved levels:
//! restoring just the active flags breaks the next pop or screen switch. See
//! <https://sw.kovidgoyal.net/kitty/keyboard-protocol/#progressive-enhancement>.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_CSI: usize = 256;
const MAX_STACK: usize = 32;
const DEC_MODES: [u16; 15] = [
    1, 6, 7, 25, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016, 2004, 2026,
];

// Cancel a partial control, finish any synchronized frame, clear both keyboard
// stacks, return to the primary screen, and restore ordinary shell input. A
// screen switch is necessary even when already inline: the inactive alternate
// screen can still contain a stack reconstructed by an earlier restore.
const RESET: &[u8] = b"\x18\x1b[?2026l\x1b[?1049h\x1b[<65535u\x1b[=0u\
    \x1b[?1049l\x1b[<65535u\x1b[=0u\x1b[>4;0m\x1b>\x1b[4l\
    \x1b[?1l\x1b[?6l\x1b[?7h\x1b[?25h\x1b[?1000l\x1b[?1002l\
    \x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1007l\
    \x1b[?1015l\x1b[?1016l\x1b[?2004l";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Keyboard {
    flags: u16,
    saved: Vec<u16>,
}

impl Keyboard {
    fn push(&mut self, flags: u16) {
        if self.saved.len() == MAX_STACK {
            self.saved.remove(0);
        }
        self.saved.push(self.flags);
        self.flags = flags;
    }

    fn pop(&mut self, count: u16) {
        for _ in 0..usize::from(count).min(MAX_STACK + 1) {
            self.flags = self.saved.pop().unwrap_or(0);
        }
    }

    fn restore(&self, out: &mut String) {
        if let Some(first) = self.saved.first() {
            let _ = write!(out, "\x1b[={first}u");
            for flags in self
                .saved
                .iter()
                .skip(1)
                .chain(std::iter::once(&self.flags))
            {
                let _ = write!(out, "\x1b[>{flags}u");
            }
        } else {
            let _ = write!(out, "\x1b[={}u", self.flags);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Modes {
    alternate: bool,
    dec: [bool; DEC_MODES.len()],
    keyboard: [Keyboard; 2],
    modify_other_keys: u16,
    keypad: bool,
    insert: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            alternate: false,
            dec: DEC_MODES.map(|mode| matches!(mode, 7 | 25)),
            keyboard: Default::default(),
            modify_other_keys: 0,
            keypad: false,
            insert: false,
        }
    }
}

#[derive(Default)]
enum ParseState {
    #[default]
    Ground,
    Escape,
    Csi,
    IgnoreCsi,
    String {
        osc: bool,
    },
}

/// Bounded terminal-mode memory. Feed it in PTY output order and restore from
/// that same thread before forwarding a newly attached client's live output.
#[derive(Default)]
pub struct TerminalModes {
    modes: Modes,
    parser: ParseState,
    prefix: Vec<u8>,
}

impl TerminalModes {
    /// Observe bytes without retaining text or control-string payloads.
    pub fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == 0x1b {
                self.prefix.clear();
                self.prefix.push(byte);
                self.parser = ParseState::Escape;
                continue;
            }
            if matches!(byte, 0x18 | 0x1a) {
                self.finish();
                continue;
            }
            match self.parser {
                ParseState::Ground => {}
                ParseState::String { osc } => {
                    if osc && byte == 7 {
                        self.finish();
                    }
                }
                ParseState::Escape => match byte {
                    b'[' if self.prefix.len() == 1 => {
                        self.prefix.push(byte);
                        self.parser = ParseState::Csi;
                    }
                    b']' | b'P' | b'X' | b'^' | b'_' if self.prefix.len() == 1 => {
                        self.prefix.clear();
                        self.parser = ParseState::String { osc: byte == b']' };
                    }
                    0x20..=0x2f if self.prefix.len() < MAX_CSI => self.prefix.push(byte),
                    0x30..=0x7e => {
                        if self.prefix.len() == 1 {
                            match byte {
                                b'c' => self.modes = Modes::default(),
                                b'=' => self.modes.keypad = true,
                                b'>' => self.modes.keypad = false,
                                _ => {}
                            }
                        }
                        self.finish();
                    }
                    _ => {}
                },
                ParseState::Csi => match byte {
                    0x20..=0x3f if self.prefix.len() < MAX_CSI => self.prefix.push(byte),
                    0x20..=0x3f => {
                        self.prefix.clear();
                        self.parser = ParseState::IgnoreCsi;
                    }
                    0x40..=0x7e => {
                        self.apply_csi(byte);
                        self.finish();
                    }
                    _ => {}
                },
                ParseState::IgnoreCsi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.finish();
                    }
                }
            }
        }
    }

    fn finish(&mut self) {
        self.prefix.clear();
        self.parser = ParseState::Ground;
    }

    fn apply_csi(&mut self, action: u8) {
        let mut body = &self.prefix[2..];
        let private = body
            .first()
            .copied()
            .filter(|byte| matches!(byte, b'?' | b'>' | b'<' | b'='));
        if private.is_some() {
            body = &body[1..];
        }
        // SGR and cursor-addressing dominate TUI output. They carry no mode
        // state we restore, so don't allocate parameters for every painted cell.
        if !matches!(
            (private, action),
            (Some(b'?') | None, b'h' | b'l')
                | (Some(b'>'), b'm')
                | (Some(b'>' | b'<' | b'='), b'u')
        ) {
            return;
        }
        let Ok(body) = std::str::from_utf8(body) else {
            return;
        };
        let Some(params) = body
            .split(';')
            .map(|part| {
                if part.is_empty() {
                    Some(0)
                } else {
                    part.parse::<u16>().ok()
                }
            })
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        match (private, action) {
            (Some(b'?'), b'h' | b'l') => {
                for mode in params {
                    if matches!(mode, 47 | 1047 | 1049) {
                        self.modes.alternate = action == b'h';
                    } else if let Some(index) = DEC_MODES.iter().position(|&known| known == mode) {
                        if action == b'h' && matches!(mode, 1000 | 1002 | 1003) {
                            for (known, enabled) in DEC_MODES.iter().zip(&mut self.modes.dec) {
                                if matches!(known, 1000 | 1002 | 1003) {
                                    *enabled = false;
                                }
                            }
                        }
                        self.modes.dec[index] = action == b'h';
                    }
                }
            }
            (None, b'h' | b'l') if params.contains(&4) => self.modes.insert = action == b'h',
            (Some(b'>'), b'm') if params.first() == Some(&4) => {
                let mode = params.get(1).copied().unwrap_or(0);
                if mode <= 2 {
                    self.modes.modify_other_keys = mode;
                }
            }
            (Some(kind @ (b'>' | b'<' | b'=')), b'u') => {
                let keyboard = &mut self.modes.keyboard[usize::from(self.modes.alternate)];
                let flags = params[0] & 31;
                match kind {
                    b'>' => keyboard.push(flags),
                    b'<' => keyboard.pop(params[0].max(1)),
                    b'=' => match params.get(1).copied().unwrap_or(1).max(1) {
                        1 => keyboard.flags = flags,
                        2 => keyboard.flags |= flags,
                        3 => keyboard.flags &= !flags,
                        _ => {}
                    },
                    _ => unreachable!(),
                }
            }
            _ => {}
        }
    }

    /// Restore both keyboard stacks, select the current screen, then restore
    /// input modes. This replaces any launch-time fallback applied by an older
    /// attach client. No screen contents or terminal queries are replayed.
    pub fn restore_buffer(&self) -> Vec<u8> {
        let mut out = String::from_utf8(RESET.to_vec()).expect("ASCII terminal reset");
        self.modes.keyboard[0].restore(&mut out);
        out.push_str("\x1b[?1049h");
        self.modes.keyboard[1].restore(&mut out);
        if !self.modes.alternate {
            out.push_str("\x1b[?1049l");
        }
        for (mode, enabled) in DEC_MODES.iter().zip(self.modes.dec) {
            // A half-drawn synchronized frame must not hold the new window
            // indefinitely while the application is idle. Its next frame can
            // enable synchronization again through the ordinary byte relay.
            if *mode != 2026 {
                let _ = write!(out, "\x1b[?{mode}{}", if enabled { 'h' } else { 'l' });
            }
        }
        let _ = write!(
            out,
            "\x1b[>4;{}m\x1b{}\x1b[4{}",
            self.modes.modify_other_keys,
            if self.modes.keypad { '=' } else { '>' },
            if self.modes.insert { 'h' } else { 'l' }
        );
        match self.parser {
            ParseState::Escape | ParseState::Csi => {
                out.push_str(std::str::from_utf8(&self.prefix).expect("ASCII control prefix"))
            }
            // Resume an unfinished string as an unknown command, consuming
            // its tail without replaying a clipboard write or other payload.
            ParseState::String { osc: true } => out.push_str("\x1b]99999;"),
            ParseState::String { osc: false } => out.push_str("\x1bP99999z"),
            ParseState::IgnoreCsi => out.push_str("\x1b[??"),
            ParseState::Ground => {}
        }
        out.into_bytes()
    }
}

static CLEANUP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Restore ordinary terminal modes even when libshpool exits the process from
/// inside its relay. Its exit paths bypass Rust destructors; libc atexit runs
/// for those exits too. The callback uses only raw, non-allocating tty writes.
pub struct AttachTerminalGuard;

impl AttachTerminalGuard {
    pub fn new() -> std::io::Result<Self> {
        // The attach entrypoints are single-threaded here and acquire one guard.
        if unsafe { libc::atexit(cleanup_terminal) } != 0 {
            return Err(std::io::Error::other("registering terminal cleanup"));
        }
        CLEANUP_ACTIVE.store(true, Ordering::Release);
        Ok(Self)
    }
}

impl Drop for AttachTerminalGuard {
    fn drop(&mut self) {
        cleanup_terminal();
    }
}

extern "C" fn cleanup_terminal() {
    if !CLEANUP_ACTIVE.swap(false, Ordering::AcqRel)
        || unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1
    {
        return;
    }
    let mut remaining = RESET;
    while !remaining.is_empty() {
        let written = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                remaining.as_ptr().cast(),
                remaining.len(),
            )
        };
        if written < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        }
        if written <= 0 {
            break;
        }
        remaining = &remaining[written as usize..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};

    fn observe(bytes: &[u8]) -> TerminalModes {
        let mut terminal = TerminalModes::default();
        terminal.process(bytes);
        terminal
    }

    #[test]
    fn a_codex_overlay_restores_both_keyboard_stacks_and_its_scroll_mode() {
        let original = observe(
            b"\x1b[>7u\x1b[?2004h\x1b[?1004h\
            \x1b[?1049h\x1b[?1007h\x1b[>5u\x1b[>1u",
        );
        assert!(original.modes.alternate);
        assert_eq!(original.modes.keyboard[0].flags, 7);
        assert_eq!(original.modes.keyboard[1].flags, 1);

        // Start from an old attach client's inaccurate launch-time prime.
        let mut restored = observe(b"\x1b[>5u\x1b[?1000h");
        restored.process(&original.restore_buffer());
        assert_eq!(restored.modes, original.modes);
        restored.process(b"\x1b[<u");
        assert_eq!(restored.modes.keyboard[1].flags, 5);
        restored.process(b"\x1b[<u\x1b[?1049l");
        assert_eq!(restored.modes.keyboard[0].flags, 7);
        assert_eq!(restored.modes.keyboard[1].flags, 0);
    }

    #[test]
    fn every_chunk_boundary_including_reattach_inside_a_csi_preserves_modes() {
        let trace = b"\x1b[>7u\x1b[?1049h\x1b[?1007h\x1b[>5u\x1b[=2;2u\x1b[>4;2m";
        let expected = observe(trace);
        for split in 0..=trace.len() {
            let mut pool = observe(&trace[..split]);
            let mut attached = observe(&pool.restore_buffer());
            pool.process(&trace[split..]);
            attached.process(&trace[split..]);
            assert_eq!(pool.modes, expected.modes, "pool split {split}");
            assert_eq!(attached.modes, expected.modes, "attach split {split}");
        }
    }

    #[test]
    fn detached_mode_changes_replace_the_launch_time_snapshot() {
        let mut pool = observe(b"\x1b[>7u\x1b[?1049h\x1b[>5u\x1b[?1007h");
        // The overlay closes while detached, then enhancements are disabled.
        pool.process(b"\x1b[<u\x1b[?1049l\x1b[?1007l\x1b[<u\x1b[?2004l");
        let restored = observe(&pool.restore_buffer());
        assert_eq!(restored.modes, pool.modes);
        assert!(!restored.modes.alternate);
        assert_eq!(restored.modes.keyboard[0].flags, 0);
        assert_eq!(restored.modes.keyboard[1].flags, 0);
    }

    #[test]
    fn keyboard_set_clear_pop_and_overflow_follow_the_protocol() {
        let mut terminal = observe(b"\x1b[>7u\x1b[=2;3u");
        assert_eq!(terminal.modes.keyboard[0].flags, 5);
        terminal.process(b"\x1b[>1u\x1b[<2u");
        assert_eq!(terminal.modes.keyboard[0], Keyboard::default());
        for _ in 0..1000 {
            terminal.process(b"\x1b[>7u");
        }
        assert_eq!(terminal.modes.keyboard[0].saved.len(), MAX_STACK);
        terminal.process(b"\x1b[<65535u");
        assert_eq!(terminal.modes.keyboard[0], Keyboard::default());
    }

    #[test]
    fn replay_is_bounded_and_never_contains_text_or_string_payloads() {
        let mut terminal = observe(b"ordinary output\x1b]52;c;");
        terminal.process(&vec![b'A'; 1_000_000]);
        let restore = terminal.restore_buffer();
        assert!(restore.len() < 1024);
        assert!(!restore.windows(4).any(|part| part == b"AAAA"));
        assert!(!restore.windows(4).any(|part| part == b"52;c"));
        let mut attached = observe(&restore);
        for t in [&mut terminal, &mut attached] {
            t.process(b"tail\x07\x1b[?1049h");
        }
        assert_eq!(terminal.modes, attached.modes);

        terminal.process(b"\x1b[");
        terminal.process(&vec![b'1'; 100_000]);
        assert!(terminal.prefix.len() <= MAX_CSI);
        terminal.process(b"h\x1b[?1049l");
        assert!(!terminal.modes.alternate);
    }

    #[test]
    fn reset_and_repeated_restores_leave_no_stale_modes() {
        let pool = observe(
            b"\x1b[>3u\x1b[?1049h\x1b[?1000;1002;1003;1006h\
            \x1b[?1004;2004h\x1b[>5u\x1b[>4;2m\x1b=",
        );
        let mut terminal = TerminalModes::default();
        for _ in 0..3 {
            terminal.process(&pool.restore_buffer());
            assert_eq!(terminal.modes, pool.modes);
        }
        terminal.process(RESET);
        assert_eq!(terminal.modes, Modes::default());
        terminal.process(b"\x1b[>7u\x1b[?1049h\x1bc");
        assert_eq!(terminal.modes, Modes::default());
    }

    #[test]
    fn synchronized_output_is_released_on_reattach() {
        let pool = observe(b"\x1b[?2026h");
        let restored = observe(&pool.restore_buffer());
        assert!(!restored.modes.dec[DEC_MODES.iter().position(|&mode| mode == 2026).unwrap()]);
    }

    // A child test process models libshpool's std::process::exit path. The
    // outer test supplies a private PTY and checks the actual cleanup bytes.
    #[test]
    fn cleanup_child() {
        if std::env::var_os("CM_TEST_TERMINAL_EXIT").is_none() {
            return;
        }
        let _guard = AttachTerminalGuard::new().unwrap();
        std::io::stdout()
            .write_all(b"\x1b[>7u\x1b[?1049h\x1b[>5u")
            .unwrap();
        std::io::stdout().flush().unwrap();
        std::process::exit(7);
    }

    #[test]
    fn process_exit_cleans_both_screens_even_without_running_rust_destructors() {
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal_modes::tests::cleanup_child",
                "--nocapture",
            ])
            .env("CM_TEST_TERMINAL_EXIT", "1")
            .stdin(Stdio::null())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave)
            .spawn()
            .unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
        let mut output = Vec::new();
        let mut buf = [0; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("read cleanup PTY: {error}"),
            }
        }
        assert!(
            output.ends_with(RESET),
            "exit must end with terminal cleanup"
        );
        assert_eq!(observe(&output).modes, Modes::default());
    }
}
