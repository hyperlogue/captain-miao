use std::collections::HashMap;

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Cell;

use crate::agent::{AgentControl, SessionIndex};
use crate::state::{HostId, LauncherState, SessionStatus};

/// The foreground color for a status label. Presentation policy, so it lives in
/// the dashboard rather than on `SessionStatus` in core (which stays ratatui-free).
pub(super) fn status_color(status: &SessionStatus) -> Color {
    match status {
        // The attention states are all yellow — matching a follow-up Idle
        // (which renders in `attention_fg`, itself yellow by default).
        // `ReviewPending` (blocked on a human review) reads the same as the
        // "agent is asking" states rather than getting its own color.
        // `BackgroundServer` ("Server") joins them: a parked long-running dev
        // server/watcher isn't the agent working, so it reads as an at-rest
        // state that wants a glance, not busy green.
        SessionStatus::WaitingForApproval
        | SessionStatus::WaitingForDecision
        | SessionStatus::ReviewPending
        | SessionStatus::BackgroundServer => Color::Yellow,
        SessionStatus::FailedToStart => Color::Red,
        // `BackgroundActive` ("Task") is green with Active: a short-term
        // background step the agent is waiting to finish is genuine work.
        SessionStatus::Active | SessionStatus::Compacting | SessionStatus::BackgroundActive => {
            Color::Green
        }
        _ => Color::Reset,
    }
}

/// The status-label foreground for a row: the attention yellow when a follow-up
/// flag rides an otherwise-quiet (`Idle`/`Compacted`) row, else the plain
/// `status_color`. Shared by the table rows and the detail panel so the two
/// can't drift.
pub(super) fn status_fg(status: &SessionStatus, follow_up: bool) -> Color {
    if follow_up && matches!(status, SessionStatus::Idle | SessionStatus::Compacted) {
        crate::config::get().colors.ui.attention_fg
    } else {
        status_color(status)
    }
}

// The status-bar palette. Both bars (header + footer) paint their *whole* row a
// single flat `BAR_BG`, so each reads as one continuous surface rather than a
// ribbon that stops at the last pill. The call-outs — footer shortcut keys, the
// header brand, a pending-prefix / search badge — sit on the lighter `KEY_BG` so
// they stand off the flat bar. `BAR_FG` is the default bar text; `LABEL_FG` (a
// touch dimmer) is the footer hint labels.
const BAR_BG: Color = Color::Rgb(49, 50, 68);
const KEY_BG: Color = Color::Rgb(69, 71, 90);
const BAR_FG: Color = Color::Rgb(205, 214, 244);
const LABEL_FG: Color = Color::Rgb(186, 194, 222);

/// The base style both bars fill their whole row with, so the bar reads as one
/// continuous surface. Callers render their content spans on top; any span with
/// no explicit background inherits this flat bar colour.
pub(super) fn bar_style() -> Style {
    Style::default().bg(BAR_BG)
}

/// A highlighted pill: `spans` padded a cell on each side on the `KEY_BG`
/// surface, so the call-out stands off the flat bar. Every enclosed span is
/// forced onto `KEY_BG` while keeping its own foreground. Shared by the footer
/// keys, the header brand, and the pending-prefix / search badge.
pub(super) fn pill(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let pad = Style::default().bg(KEY_BG);
    let mut out = Vec::with_capacity(spans.len() + 2);
    out.push(Span::styled(" ", pad));
    for mut span in spans {
        span.style = span.style.bg(KEY_BG);
        out.push(span);
    }
    out.push(Span::styled(" ", pad));
    out
}

/// Join header segments as flat text on the shared bar background — no
/// per-segment fill — separated by a two-space gap and led by a one-space left
/// margin. Each segment keeps its own foreground; a span with none takes the
/// default bar text colour. Empty segments are skipped, so a caller can push
/// conditionally without emitting a stray gap.
pub(super) fn bar_segments(segments: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for seg in segments {
        if seg.is_empty() {
            continue;
        }
        out.push(Span::raw(if out.is_empty() { " " } else { "  " }));
        for mut span in seg {
            if span.style.fg.is_none() {
                span.style = span.style.fg(BAR_FG);
            }
            out.push(span);
        }
    }
    out
}

/// A footer shortcut-key pill: ` key ` on the `KEY_BG` highlight, bright white +
/// bold so the key pops off the flat bar. The pill's trailing pad is the single
/// space that separates it from the label that follows.
pub(super) fn hint_key(key: &str) -> Vec<Span<'static>> {
    pill(vec![Span::styled(
        key.to_string(),
        Style::default().fg(Color::White).bold(),
    )])
}

/// A footer hint label, meant to trail a `hint_key` pill: flat dim text directly
/// on the bar background (no pill), with a two-space trailing gap before the next
/// hint. A single leading space on the flat bar background follows the pill's own
/// `KEY_BG` trailing pad, so the key/label seam is two-tone (` key `·KEY_BG then
/// ` `·BAR_BG) — a `<key><space><space><label>` gap that changes colour mid-gap.
/// Spans that already carry a foreground keep it (a coloured value like an agent
/// or host label); plain spans take the dim label foreground.
pub(super) fn hint_label(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + 2);
    out.push(Span::raw(" "));
    for mut span in spans {
        if span.style.fg.is_none() {
            span.style = span.style.fg(LABEL_FG);
        }
        out.push(span);
    }
    out.push(Span::raw("  "));
    out
}

/// A `key + label` footer hint: a `hint_key` pill followed by dim `hint_label`
/// text on the bar. The highlighted key against the flat label is the point.
pub(super) fn hint_pair(key: &str, label: &str) -> Vec<Span<'static>> {
    let mut spans = hint_key(key);
    spans.extend(hint_label(vec![Span::styled(
        label.to_string(),
        Style::default().add_modifier(Modifier::DIM),
    )]));
    spans
}

/// A footer mode badge — a pending prefix (`Space`/`g`) or the search `/` — as a
/// yellow `KEY_BG` pill, standing apart from the white key pills. The pill's own
/// trailing pad is the single space before whatever follows (a key pill or the
/// search buffer).
pub(super) fn hint_badge(text: String) -> Vec<Span<'static>> {
    pill(vec![Span::styled(
        text,
        Style::default().fg(Color::Yellow).bold(),
    )])
}

/// ASCII case-insensitive substring search. Allocation-free, unlike
/// `a.to_lowercase().contains(&b.to_lowercase())`. Non-ASCII bytes only
/// match exactly, which is fine for our filter: the haystacks are paths,
/// prompts, and status labels — case folding only matters for ASCII.
pub(super) fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Parse a (possibly ANSI-coded) terminal-text dump into styled ratatui lines.
/// Handles SGR (`\x1b[…m`), OSC (`\x1b]…BEL` or `…ST`), and the charset-designator
/// triplet `\x1b ( B` etc. Cursor-movement and other CSI commands are dropped —
/// `kitten @ get-text --ansi` doesn't emit them, but we'd rather strip than print
/// stray chars if they ever appear. Stripped of escapes only, not visually wrapped.
pub(super) fn ansi_to_lines(input: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut buffer = String::new();
    let mut style = Style::default();
    let mut chars = input.chars().peekable();

    fn flush(buf: &mut String, style: Style, line: &mut Vec<Span<'static>>) {
        if !buf.is_empty() {
            line.push(Span::styled(std::mem::take(buf), style));
        }
    }

    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = '\0';
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&n) {
                            final_byte = n;
                            break;
                        }
                        params.push(n);
                    }
                    if final_byte == 'm' {
                        flush(&mut buffer, style, &mut current_line);
                        style = apply_sgr(style, &params);
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('(') | Some(')') | Some('*') | Some('+') => {
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\n' => {
                flush(&mut buffer, style, &mut current_line);
                lines.push(Line::from(std::mem::take(&mut current_line)));
            }
            '\r' => {}
            _ => buffer.push(c),
        }
    }
    flush(&mut buffer, style, &mut current_line);
    if !current_line.is_empty() {
        lines.push(Line::from(current_line));
    }
    lines
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    // Treat ';' and ':' interchangeably. ECMA-48 reserves ':' for sub-parameters
    // (e.g. the modern `38:2:R:G:B` form that Kitty's `get-text --ansi`
    // actually emits), but for the SGR codes we care about the layout is
    // identical either way — flattening lets one path handle both forms.
    let codes: Vec<u32> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            // An omitted parameter defaults to 0 (ECMA-48: `[;m` is a reset).
            // A *non-empty* unparseable parameter, by contrast, is garbage — map
            // it to a sentinel the match ignores rather than to 0, so a corrupt
            // escape neither silently resets the style nor shifts the positions
            // of an extended-colour (`38;2;…`) sequence.
            .map(|p| {
                if p.is_empty() {
                    0
                } else {
                    p.parse::<u32>().unwrap_or(u32::MAX)
                }
            })
            .collect()
    };
    let mut i = 0;
    while i < codes.len() {
        let c = codes[i];
        match c {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style = style.fg(basic_color(c - 30, false)),
            38 => {
                if let Some(color) = parse_extended_color(&codes, &mut i) {
                    style = style.fg(color);
                }
            }
            39 => style = style.fg(Color::Reset),
            40..=47 => style = style.bg(basic_color(c - 40, false)),
            48 => {
                if let Some(color) = parse_extended_color(&codes, &mut i) {
                    style = style.bg(color);
                }
            }
            49 => style = style.bg(Color::Reset),
            90..=97 => style = style.fg(basic_color(c - 90, true)),
            100..=107 => style = style.bg(basic_color(c - 100, true)),
            _ => {}
        }
        i += 1;
    }
    style
}

fn basic_color(idx: u32, bright: bool) -> Color {
    match (idx, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => Color::Reset,
    }
}

fn parse_extended_color(codes: &[u32], i: &mut usize) -> Option<Color> {
    let next = *codes.get(*i + 1)?;
    if next == 5 {
        let n = *codes.get(*i + 2)? as u8;
        *i += 2;
        Some(Color::Indexed(n))
    } else if next == 2 {
        let r = *codes.get(*i + 2)? as u8;
        let g = *codes.get(*i + 3)? as u8;
        let b = *codes.get(*i + 4)? as u8;
        *i += 4;
        Some(Color::Rgb(r, g, b))
    } else {
        None
    }
}

/// Tone down preview colors so the snapshot reads as a snapshot rather than
/// as a live terminal next to the dashboard. Mixes RGB toward the cell's own
/// luminance (desaturate) and darkens a little. For non-RGB colors we fall
/// back to `Modifier::DIM`, since we can't math against a terminal palette
/// the user could re-theme at any time.
pub(super) fn fade_style(mut style: Style) -> Style {
    let has_rgb_fg = matches!(style.fg, Some(Color::Rgb(..)));
    let has_rgb_bg = matches!(style.bg, Some(Color::Rgb(..)));
    if let Some(Color::Rgb(r, g, b)) = style.fg {
        let (r, g, b) = fade_rgb(r, g, b);
        style = style.fg(Color::Rgb(r, g, b));
    }
    if let Some(Color::Rgb(r, g, b)) = style.bg {
        let (r, g, b) = fade_rgb(r, g, b);
        style = style.bg(Color::Rgb(r, g, b));
    }
    if !has_rgb_fg && !has_rgb_bg {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

/// Mix 20% toward luminance gray (desaturate), then multiply by 0.85 (darken).
fn fade_rgb(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let gray = (r as u32 * 30 + g as u32 * 59 + b as u32 * 11) / 100;
    let mix = |c: u8| {
        let desat = (c as u32 * 80 + gray * 20) / 100;
        (desat * 85 / 100) as u8
    };
    (mix(r), mix(g), mix(b))
}

/// Single-codepoint emojis only (no ZWJ sequences or skin tones) so wide
/// renderers stay consistent across terminals.
const DEFAULT_DIR_EMOJIS: &[&str] = &[
    "\u{1F4C1}", // 📁 file folder
    "\u{1F4C2}", // 📂 open file folder
    "\u{1F4E6}", // 📦 package
    "\u{1F4DA}", // 📚 books
    "\u{1F4DD}", // 📝 memo
    "\u{2B50}",  // ⭐ star
    "\u{1F525}", // 🔥 fire
    "\u{26A1}",  // ⚡ high voltage
    "\u{1F48E}", // 💎 gem stone
    "\u{1F3AF}", // 🎯 direct hit
    "\u{1F680}", // 🚀 rocket
    "\u{1F3A8}", // 🎨 artist palette
    "\u{1F31F}", // 🌟 glowing star
    "\u{1F3B5}", // 🎵 musical note
    "\u{1F333}", // 🌳 deciduous tree
    "\u{1FA90}", // 🪐 ringed planet
];

/// Color palette for directory marks. Name is the on-disk override value.
pub(super) const DIR_COLORS: &[(&str, Color)] = &[
    ("red", Color::Red),
    ("orange", Color::Rgb(255, 165, 0)),
    ("yellow", Color::Yellow),
    ("green", Color::Green),
    ("cyan", Color::Cyan),
    ("blue", Color::Blue),
    ("purple", Color::Magenta),
    ("pink", Color::Rgb(255, 105, 180)),
    ("lime", Color::Rgb(170, 230, 90)),
    ("teal", Color::Rgb(0, 170, 170)),
    ("sky", Color::Rgb(120, 200, 255)),
    ("magenta", Color::Rgb(220, 60, 200)),
    ("gold", Color::Rgb(255, 215, 0)),
    ("coral", Color::Rgb(255, 127, 80)),
    ("white", Color::White),
    ("gray", Color::Gray),
];

/// FNV-1a (constant seed) so a given cwd maps to the same `(emoji, color)`
/// across dashboard restarts and across machines — the user's mental model
/// of "the blue rocket dir" survives reboots without needing an explicit
/// override. `std::hash::DefaultHasher` would reseed per process and flip.
pub(super) fn default_dir_emoji_and_color(cwd: &str) -> (&'static str, usize) {
    let key = cwd.trim_end_matches('/');
    let n = fnv1a_64(key.as_bytes());
    let icon = DEFAULT_DIR_EMOJIS[(n as usize) % DEFAULT_DIR_EMOJIS.len()];
    let color = ((n >> 32) as usize) % DIR_COLORS.len();
    (icon, color)
}

/// A stable index into a fixed table for `key`, on the same FNV-1a seed as
/// [`default_dir_emoji_and_color`] — so a derived choice (a host's fallback
/// emoji) is identical across restarts and across machines.
pub(super) fn stable_index(key: &str, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (fnv1a_64(key.as_bytes()) as usize) % len
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub(super) fn dir_color_index(name: &str) -> Option<usize> {
    DIR_COLORS.iter().position(|(n, _)| *n == name)
}

/// Past ~4 cells icons stop reading as marks and start eating row width.
pub(super) const DIR_ICON_MAX_CHARS: usize = 4;

pub(super) fn dir_icon_width(icon: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    icon.width().clamp(1, DIR_ICON_MAX_CHARS)
}

/// Renders the override column using Nerd Font glyphs. Bell sits on the left
/// and a secondary indicator (pin or mute) sits on the right; without a bell,
/// pin or mute occupies the left position alone. Pin wins over mute when both
/// flags are set. See `WIDE_PUA_GLYPHS` for the post-render fix-up that keeps
/// these wide glyphs from being clipped by neighboring writes.
///
/// Layout note: each glyph claims 1 ratatui cell but Kitty paints it 2 cells
/// wide. With the column at `Length(3)` and the WIDE_PUA_GLYPHS post-process,
/// a left-glyph at buf N auto-skips buf N+1; a right-glyph at buf N+2
/// auto-skips buf N+3 (the column spacer), giving a tight `[L][L][R][R]`
/// visual layout with no gap between glyphs.
pub(super) fn override_indicator_cell(
    follow_up: bool,
    pinned: bool,
    muted: bool,
    detached: bool,
) -> Cell<'static> {
    // nf-oct-bell, nf-cod-pinned, nf-md-sleep, nf-md-link_off
    let bell = Span::styled("\u{f49a}", Style::default().fg(Color::Yellow));
    let pin = Span::styled("\u{eba0}", Style::default().fg(Color::Blue));
    let mute = Span::styled("\u{f04b2}", Style::default().add_modifier(Modifier::DIM));
    // A pooled session still running on its host with no window on this screen
    // (§9). It joins the existing icon set rather than getting a column of its
    // own, and ranks last among the secondaries: pin and mute are things the
    // *user* chose, detached is just where the session happens to be.
    let unplugged = Span::styled("\u{f0d1d}", Style::default().add_modifier(Modifier::DIM));
    let secondary = if pinned {
        Some(pin)
    } else if muted {
        Some(mute)
    } else if detached {
        Some(unplugged)
    } else {
        None
    };

    let line = match (follow_up, secondary) {
        (true, Some(s)) => Line::from(vec![bell, Span::raw(" "), s]),
        (true, None) => Line::from(vec![bell]),
        (false, Some(s)) => Line::from(vec![s]),
        (false, None) => Line::from(""),
    };
    Cell::from(line.alignment(Alignment::Right))
}

/// Pairs of (narrow, wide) symbols for Nerd Font glyphs that suffer a width
/// disagreement between `unicode-width` (says 1) and Kitty's actual rendering
/// with a Nerd Font (paints 2 cells). `draw_table` post-processes the buffer
/// after the Table widget renders: any cell whose symbol matches the narrow
/// form is rewritten in place to the wide form (glyph + trailing space).
///
/// Why post-process instead of constructing the Cell with the wide string up
/// front: ratatui's Table writes Cell content via `Buffer::set_string`, which
/// iterates *graphemes* and stores each in its own buffer cell — so
/// `Cell::from("\u{f49a} ")` ends up as two width-1 buffer cells (glyph at N,
/// space at N+1), not one width-2 cell. The width-2 trick only works if a
/// single buffer cell's `symbol()` is the multi-grapheme string, which is
/// only reachable via `Cell::set_symbol` directly on the buffer.
///
/// What the width-2 symbol fixes: ratatui's `Buffer::diff` (see
/// ratatui-0.30 `src/buffer/buffer.rs`) sets `to_skip = symbol.width() - 1`
/// after each emitted cell. With a width-2 symbol the next buffer cell is
/// auto-skipped, so its padding-space write never reaches the terminal — and
/// Kitty's wide-rendered glyph keeps its right half intact even when adjacent
/// columns (Status text, elapsed time) update on later frames. The trailing
/// space is harmless: it gets painted at visual column N+2 and is immediately
/// overwritten by the Status column's first character.
///
/// Why not `Cell::set_skip(true)` on the cell after the glyph: that excludes
/// the cell from *all* updates including bg-style changes. Highlighted rows
/// then leave that cell with the prior frame's bg, producing a visible
/// dark-cell gap inside the selection highlight. Going the wide-symbol route
/// keeps the cell on the normal update path so row highlights paint cleanly.
pub(super) const WIDE_PUA_GLYPHS: &[(&str, &str)] = &[
    ("\u{f49a}", "\u{f49a} "),
    ("\u{eba0}", "\u{eba0} "),
    ("\u{f04b2}", "\u{f04b2} "),
];

/// Display-cell budget for an auto-title folded from the first prompt. A
/// deliberate `/rename` is short by nature and shown in full; a first prompt is
/// unbounded, so it's clipped to a title's worth even in the panels/pickers that
/// otherwise show a name untruncated.
const AUTO_TITLE_MAX: usize = 60;

/// Resolve a session's display name. Lookup order:
/// 1. `LauncherState.name` — Claude's `/rename` (folded by its launcher from the
///    session file) or Codex's sqlite title (overlaid by the host's
///    `LocalBackend`, one throttled reader per host). Both paths ride
///    `LauncherState`, so remote rows get them too.
/// 2. Backend session-name index (`session_index`) — Claude's `session_id → name`
///    manifest scan; empty for Codex, whose title arrives via 1.
/// 3. Auto-title — first user prompt, folded by the launcher.
/// 4. Stable random name keyed on `launcher_pid`.
/// 5. Final fallback `session-<pid>`.
///
/// 1 and 3 arrive on `LauncherState` (the backend does the reading, not this
/// code). 2 is the only one still read from disk here, and only the Claude
/// session-name manifest — never a transcript.
///
/// Returns the *untruncated* name for a deliberate title (steps 1–2) — clipping
/// to a column budget is a per-render-site concern, so the table cell (and the
/// restart-confirm prompt) apply `truncate_str` themselves while the detail
/// panel and pickers show it in full. The **auto-title** (step 3) is the one
/// exception: it's unbounded free text (the launcher caps it at 120 on disk), so
/// it's clipped here to a title-sized budget, `AUTO_TITLE_MAX`, and returned
/// short even to the full-display sites — a first prompt is a paragraph, not a
/// title.
pub(super) fn session_display_name(
    s: &LauncherState,
    session_index: &SessionIndex,
    random_names: &HashMap<super::FlagKey, String>,
) -> String {
    // A name candidate is usable only if it's non-empty once trimmed. Returns a
    // borrow of the argument so an accepted candidate allocates just once.
    fn nonblank(o: &Option<String>) -> Option<&str> {
        o.as_deref().map(str::trim).filter(|t| !t.is_empty())
    }
    if let Some(name) = nonblank(&s.name) {
        return name.to_string();
    }
    if let Some(name) = session_index.lookup(s) {
        return name.to_string();
    }
    if let Some(prompt) = nonblank(&s.first_prompt) {
        // Flatten newlines and clip to a title-sized budget so a long opener
        // doesn't spill across the detail panel's Name field or a picker row.
        return truncate_str(&prompt.replace('\n', " "), AUTO_TITLE_MAX);
    }
    random_names
        .get(&super::flag_key(s))
        .cloned()
        .unwrap_or_else(|| format!("session-{}", s.launcher_pid))
}

pub(super) fn random_session_name(pid: u32) -> String {
    const ADJECTIVES: &[&str] = &[
        "amber", "bold", "calm", "dark", "eager", "fair", "glad", "hazy", "keen", "lush", "mild",
        "neat", "pale", "quick", "rare", "soft", "tidy", "vast", "warm", "zesty", "blue", "cool",
        "dusk", "epic", "fond", "grim", "jade", "lazy", "nova", "pure", "ruby", "sage", "true",
        "vivid", "wild", "airy", "cozy", "deep", "fine", "gold",
    ];
    const NOUNS: &[&str] = &[
        "fox", "owl", "elm", "bay", "dew", "fin", "gem", "hue", "ivy", "jet", "koi", "log", "mist",
        "oak", "paw", "reef", "sun", "tide", "vine", "wolf", "arc", "cove", "dawn", "echo", "fern",
        "glen", "hare", "iris", "lark", "moth", "nest", "orca", "pine", "quail", "rose", "seal",
        "thorn", "umber", "vale", "wren",
    ];
    let hash = pid.wrapping_mul(2654435761);
    let adj = ADJECTIVES[(hash as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((hash >> 16) as usize) % NOUNS.len()];
    format!("{adj}-{noun}")
}

pub(super) fn format_relative_time(since: std::time::SystemTime) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(since)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Color the context-length cell by pressure: warning (yellow) and critical
/// (red) thresholds come from config — the yellow/red is semantic here, so it
/// stays hardcoded rather than coming from colors.ui.
pub(super) fn context_pressure_style(tokens: u64) -> Style {
    let t = &crate::config::get().thresholds;
    if tokens >= t.context_critical_tokens {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if tokens >= t.context_warning_tokens {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

/// Split a raw model id into its core and an optional trailing bracketed
/// variant marker: `claude-opus-4-8[1m]` → `("claude-opus-4-8", Some("1m"))`,
/// `claude-opus-4-8` → `("claude-opus-4-8", None)`.
fn split_variant(id: &str) -> (&str, Option<&str>) {
    match id.strip_suffix(']').and_then(|s| s.rsplit_once('[')) {
        Some((core, var)) => (core, Some(var)),
        None => (id, None),
    }
}

/// The Claude family token of a model core (`claude-sonnet-4-6` → `Some("sonnet")`).
/// `None` for a prefix-less / non-Claude id, or an empty family.
fn claude_family(core: &str) -> Option<&str> {
    core.strip_prefix("claude-")
        .and_then(|rest| rest.split('-').next())
        .filter(|f| !f.is_empty())
}

/// Prettify a raw model id for display. Claude ids (`claude-opus-4-8`,
/// `claude-opus-4-8[1m]`, `claude-haiku-4-5-20251001`) become `Opus 4.8`,
/// `Opus 4.8 [1m]`, `Haiku 4.5` — family capitalized, the short numeric tokens
/// joined as the version, any long date-like suffix dropped, and the `[…]`
/// variant marker (e.g. the 1M-context `[1m]`) preserved. Anything else (a bare
/// alias, a Codex `gpt-5.5`) is returned unchanged.
pub(super) fn model_label(id: &str) -> String {
    let (core, variant) = split_variant(id);
    let Some(family) = claude_family(core) else {
        return id.to_string();
    };
    let mut out = String::new();
    let mut chars = family.chars();
    if let Some(c) = chars.next() {
        out.extend(c.to_uppercase());
        out.push_str(chars.as_str());
    }
    // Version = the short numeric tokens after the family (`4`, `8`); a long
    // date stamp like `20251001` is dropped.
    let version: Vec<&str> = core
        .strip_prefix("claude-")
        .unwrap_or_default()
        .split('-')
        .skip(1)
        .filter(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if !version.is_empty() {
        out.push(' ');
        out.push_str(&version.join("."));
    }
    if let Some(var) = variant {
        out.push_str(" [");
        out.push_str(var);
        out.push(']');
    }
    out
}

/// Color for a raw Claude model id, keyed on the family: Sonnet → blue,
/// Fable → purple (magenta), Opus → white. Anything else (a bare alias, a
/// Codex `gpt-*`, an unknown family) stays the default `Reset`. Standard
/// terminal colors only.
pub(super) fn model_color(id: &str) -> Color {
    let (core, _) = split_variant(id);
    match claude_family(core) {
        Some("sonnet") => Color::Blue,
        Some("fable") => Color::Magenta,
        Some("opus") => Color::White,
        _ => Color::Reset,
    }
}

pub(super) fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{n}")
    }
}

/// Truncate `s` to fit within `max` terminal cells (not chars), appending a
/// 1-cell `…` when content is dropped. Wide chars (CJK, 2-cell emoji) count
/// their full display width and zero-width combining marks count nothing, so a
/// trailing wide glyph is never clipped past the slot. Char boundaries are
/// always respected.
pub(super) fn truncate_str(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if s.width() <= max {
        return s.to_string();
    }
    // Reserve one cell for the ellipsis so the result still fits in `max`.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Minimal standard-base64 encoder (RFC 4648, with `=` padding). Kept
/// dependency-free since the crate's only base64 need is wrapping a session id
/// for the OSC 52 clipboard escape, and those inputs are tiny.
pub(super) fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Title for the new-session (workdir) picker, naming the backend the launch
/// will use. Shared by the picker opener and the in-picker `Ctrl-t` toggle so
/// the two never drift.
pub(super) fn workdir_picker_title(agent: AgentControl, host: &HostId) -> String {
    if host.is_local() {
        format!("New {} Session — Directory", agent.label())
    } else {
        format!("New {} Session on {} — Directory", agent.label(), host.0)
    }
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [_, v_center, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);
    let [_, h_center, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(v_center);
    h_center
}

/// Worst-case width of `format_elapsed` output (`>99h00m` = 7 chars). Used by
/// the dashboard to size the "Updated" column tightly — every other variant
/// is shorter and right-aligns within this width.
pub(super) const ELAPSED_MAX_WIDTH: u16 = 7;

pub(super) fn format_elapsed(secs: u64) -> String {
    if secs < 3600 {
        format!("{}m", (secs / 60).max(1))
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if hours > 99 {
            ">99h00m".to_string()
        } else {
            format!("{hours}h{mins:02}m")
        }
    }
}

/// Minute-resolution age for the preview staleness label, matching the
/// "Updated" column's coarseness: sub-minute ages read `<1m` rather than
/// ticking per second, everything longer is `format_elapsed` (3m, 1h05m).
pub(super) fn format_coarse_age(secs: u64) -> String {
    if secs < 60 {
        "<1m".to_string()
    } else {
        format_elapsed(secs)
    }
}

/// Build the "Updated" cell with the unit suffixes tinted: `h` yellow, `m`
/// blue. Digits and the `>` overflow marker stay default — coloring the units
/// alone makes the magnitude scannable without flooding the row in color.
/// Right-aligned so values stack flush against the column edge regardless of
/// width.
pub(super) fn elapsed_cell(secs: u64) -> Cell<'static> {
    let text = format_elapsed(secs);
    // Emit runs, not a span per char: a default-styled digit run then the
    // tinted unit letter, at most twice (`1h05m` → "1", "h", "05", "m"). Same
    // rendered output as a per-char split, ≤4 spans instead of up to 7.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run_start = 0;
    for (i, c) in text.char_indices() {
        let unit = match c {
            'h' => Some(Color::Yellow),
            'm' => Some(Color::Blue),
            _ => None,
        };
        if let Some(color) = unit {
            if run_start < i {
                spans.push(Span::raw(text[run_start..i].to_string()));
            }
            spans.push(Span::styled(c.to_string(), Style::default().fg(color)));
            run_start = i + c.len_utf8();
        }
    }
    if run_start < text.len() {
        spans.push(Span::raw(text[run_start..].to_string()));
    }
    Cell::from(Line::from(spans).alignment(Alignment::Right))
}

#[cfg(test)]
mod tests {
    use super::{model_color, model_label, truncate_str};
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn truncate_str_is_width_aware() {
        // Fits: returned unchanged.
        assert_eq!(truncate_str("abc", 5), "abc");
        assert_eq!(truncate_str("abc", 3), "abc");

        // ASCII overflow truncates with a trailing ellipsis, staying within max.
        let t = truncate_str("abcdef", 4);
        assert_eq!(t, "abc…");
        assert!(t.width() <= 4);

        // Wide (2-cell) chars: a trailing wide glyph must not overflow the slot.
        // "日本語" is 6 cells; with max=5 we get one wide char + ellipsis = 3 cells.
        let t = truncate_str("日本語", 5);
        assert!(t.width() <= 5, "got width {} for {t:?}", t.width());
        assert!(t.ends_with('…'));
    }

    #[test]
    fn model_label_prettifies_claude_ids() {
        assert_eq!(model_label("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_label("claude-fable-5"), "Fable 5");
        assert_eq!(model_label("claude-sonnet-4-6"), "Sonnet 4.6");
        // Long trailing date stamp is dropped.
        assert_eq!(model_label("claude-haiku-4-5-20251001"), "Haiku 4.5");
    }

    #[test]
    fn model_label_preserves_variant_marker() {
        assert_eq!(model_label("claude-opus-4-8[1m]"), "Opus 4.8 [1m]");
    }

    #[test]
    fn model_label_passes_through_unknown() {
        // Codex / provider ids and bare aliases are returned unchanged.
        assert_eq!(model_label("gpt-5.5"), "gpt-5.5");
        assert_eq!(model_label("sonnet"), "sonnet");
    }

    #[test]
    fn model_color_keys_on_family() {
        assert_eq!(model_color("claude-sonnet-4-6"), Color::Blue);
        assert_eq!(model_color("claude-fable-5"), Color::Magenta);
        assert_eq!(model_color("claude-opus-4-8"), Color::White);
        // Variant marker doesn't disturb the family match.
        assert_eq!(model_color("claude-opus-4-8[1m]"), Color::White);
        // Other families and non-Claude ids stay default.
        assert_eq!(model_color("claude-haiku-4-5-20251001"), Color::Reset);
        assert_eq!(model_color("gpt-5.5"), Color::Reset);
    }
}
