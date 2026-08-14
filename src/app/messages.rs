//! In-memory history of the footer's status messages.
//!
//! The footer holds exactly one status line and the next [`App::set_status`]
//! overwrites it, so anything said while the user was reading a preview — a
//! launch failure, a host's refusal, the `[keybinds]` warnings from startup — is
//! simply gone by the time they look down. This keeps the last [`MAX_ENTRIES`]
//! of them so the message-log popup can show what went past.
//!
//! **Memory only, on purpose.** These lines quote cwds, host targets and prompt
//! text — the reason the state files are `0600` in the first place — and a log
//! that survives a restart is one that then needs rotating, ageing out and
//! cleaning up, none of which a scrollback of transient UI notices is worth. The
//! cap keeps it bounded from the other end: a wedged loop repeating one error
//! doesn't grow the log at all, because a repeat of the newest entry only bumps
//! its counter.

use std::collections::VecDeque;
use std::time::Instant;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::format;

/// How many distinct messages are kept. Deep enough to cover a whole session's
/// worth of notices (they arrive at human pace, one per keystroke at worst),
/// small enough that the log is a few tens of KiB in the pathological case.
const MAX_ENTRIES: usize = 200;

/// One status message, as it was shown in the footer.
#[derive(Debug, Clone)]
pub(super) struct MessageEntry {
    /// When it was last set — refreshed by a repeat, so the age reads as "how
    /// long since this was last said" rather than "since it was first said".
    pub(super) at: Instant,
    pub(super) text: String,
    pub(super) error: bool,
    /// How many times in a row this exact message was set. 1 for the ordinary
    /// case; higher only for a message that repeated with nothing between.
    pub(super) repeats: u32,
}

/// The capped ring of [`MessageEntry`]s, oldest first.
///
/// `pub(crate)` only to match the visibility of the `App` field holding it (as
/// [`Keymap`](super::keymap::Keymap) is); nothing outside `app` touches it.
#[derive(Debug, Default)]
pub(crate) struct MessageLog {
    entries: VecDeque<MessageEntry>,
}

impl MessageLog {
    /// Record a message. An exact repeat of the newest entry — same text, same
    /// error flag — bumps that entry instead of appending: a retry loop that
    /// fails every tick would otherwise flush every other message out of the
    /// log, which is precisely the history the popup exists to keep.
    pub(super) fn push(&mut self, text: &str, error: bool) {
        if let Some(last) = self.entries.back_mut()
            && last.error == error
            && last.text == text
        {
            last.repeats = last.repeats.saturating_add(1);
            last.at = Instant::now();
            return;
        }
        self.entries.push_back(MessageEntry {
            at: Instant::now(),
            text: text.to_string(),
            error,
            repeats: 1,
        });
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Oldest first — the order the popup reads in, and the order the scroll
    /// offset counts in.
    pub(super) fn iter(&self) -> impl Iterator<Item = &MessageEntry> {
        self.entries.iter()
    }
}

/// The message-log popup's scroll state. `Some` on `App` iff
/// `input_mode == InputMode::Messages`.
///
/// Modelled on [`HostLogView`](super::HostLogView), down to the units: the
/// offset counts *physical* lines, because a long message wraps into several and
/// scrolling past a whole entry at a time would step over text unread.
#[derive(Debug)]
pub(crate) struct MessageLogView {
    pub(in crate::app) scroll: usize,
    /// Content rows the last draw had. Only a render knows the popup's size, and
    /// `G` / the page keys need it; 0 until the first frame.
    pub(in crate::app) rows: usize,
}

impl MessageLogView {
    /// Opens parked at the bottom. `usize::MAX` is the same "last page" request
    /// `G` makes — the draw clamps it once it knows the line count — and the
    /// newest message is the one the user opened this to read.
    pub(super) fn at_bottom() -> Self {
        Self {
            scroll: usize::MAX,
            rows: 0,
        }
    }
}

/// One rendered line of the message log — see [`App::message_log_lines`].
#[derive(Debug, Clone)]
pub(super) struct MessageLine {
    /// How long ago the message was set, on the **first** line of an entry only;
    /// `None` on the continuation lines of a wrapped one.
    pub(super) age: Option<String>,
    pub(super) error: bool,
    pub(super) text: String,
}

impl super::App {
    /// The log flattened to physical lines, wrapped to `width` cells.
    ///
    /// Flattened here rather than at render time for the same reason
    /// [`host_log_lines`](super::App::host_log_lines) is: the scroll offset and
    /// the renderer have to count in the same unit, and a status message is
    /// routinely wider than the popup (a refusal quoting a command line, the
    /// startup warnings joined with `;`).
    pub(super) fn message_log_lines(&self, width: usize) -> Vec<MessageLine> {
        let now = Instant::now();
        let mut out = Vec::new();
        for entry in self.messages.iter() {
            let age = format::format_log_age(now.saturating_duration_since(entry.at).as_secs());
            // The repeat count rides the text so it wraps with it, rather than
            // sitting in a column that a long message would push off the edge.
            let text = if entry.repeats > 1 {
                format!("{} \u{00d7}{}", entry.text, entry.repeats)
            } else {
                entry.text.clone()
            };
            for (i, line) in wrap(&text, width).into_iter().enumerate() {
                out.push(MessageLine {
                    age: (i == 0).then(|| age.clone()),
                    error: entry.error,
                    text: line,
                });
            }
        }
        out
    }
}

/// Break `text` into lines of at most `width` display cells, preferring
/// whitespace. A token too wide to fit on a line of its own — a path, a session
/// id — is hard-broken rather than allowed to run off the popup.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        for word in para.split_whitespace() {
            let w = word.width();
            if used > 0 && used + 1 + w > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if w > width {
                if used > 0 {
                    out.push(std::mem::take(&mut line));
                }
                let mut chunks = hard_break(word, width);
                // The tail becomes the current line so the next word can still
                // share it; everything before it is already full.
                line = chunks.pop().unwrap_or_default();
                used = line.width();
                out.extend(chunks);
                continue;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += w;
        }
        // Pushed even when empty: a blank line in the source is a blank line
        // here, and a whitespace-only message still occupies a row.
        out.push(line);
    }
    out
}

/// Split one over-wide token into `width`-cell chunks.
fn hard_break(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut used = 0usize;
    for c in word.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(c);
        used += w;
    }
    chunks.push(cur);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeat_of_the_newest_message_bumps_it_instead_of_appending() {
        let mut log = MessageLog::default();
        log.push("host is unreachable", true);
        log.push("host is unreachable", true);
        log.push("host is unreachable", true);
        assert_eq!(log.len(), 1);
        assert_eq!(log.iter().next().map(|e| e.repeats), Some(3));

        // Only *consecutive* repeats fold: an interleaved message means the two
        // occurrences are separate events and the history should show both.
        log.push("Launched window 42", false);
        log.push("host is unreachable", true);
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn the_log_is_capped_and_drops_the_oldest() {
        let mut log = MessageLog::default();
        for i in 0..MAX_ENTRIES + 10 {
            log.push(&format!("message {i}"), false);
        }
        assert_eq!(log.len(), MAX_ENTRIES);
        let first = log.iter().next().expect("non-empty").text.clone();
        assert_eq!(first, "message 10");
    }

    #[test]
    fn wrap_breaks_on_words_and_hard_breaks_an_over_wide_token() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        // A token wider than the box is chopped, and its tail keeps taking words.
        assert_eq!(wrap("aaaaaaaa bb", 6), vec!["aaaaaa", "aa bb"]);
        // Short input is left alone; an empty message still occupies a line.
        assert_eq!(wrap("fits", 10), vec!["fits"]);
        assert_eq!(wrap("", 10), vec![""]);
    }
}
