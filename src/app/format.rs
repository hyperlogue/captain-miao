use std::collections::HashMap;

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Cell;

use crate::agent::SessionIndex;
use crate::state::{LauncherState, SessionStatus};
use cm_core::agents::grok::unwrap_user_query;

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

/// One icon slot — and, because the two must agree, the cap the host and
/// directory-mark editors enforce on a typed icon. Two cells is exactly one
/// emoji, or a 2-char text mark (`py`, `TS`).
///
/// Slot width and input cap are deliberately the same number. The icon column is
/// a constant [`ICON_COL_WIDTH`] so that nothing to its right moves as hosts
/// connect, foreign rows appear, the list scrolls or a mark changes — and a cap
/// wider than the slot would mean a legal icon that cannot fit the column it is
/// drawn in, which is how a fixed column starts clipping instead of shifting.
/// Raising this widens every row by twice the increase; it does not make the
/// column elastic again.
pub(super) const ICON_SLOT_WIDTH: usize = 2;

/// The icon column: the host slot then the workdir slot, adjacent, no divider.
/// Constant by design — see [`ICON_SLOT_WIDTH`]. The narrow layout carries the
/// workdir slot alone and so is one slot wide.
pub(super) const ICON_COL_WIDTH: u16 = 2 * ICON_SLOT_WIDTH as u16;

pub(super) fn dir_icon_width(icon: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    icon.width().clamp(1, ICON_SLOT_WIDTH)
}

/// Whether `icon` is an emoji, which is what decides whether a directory mark's
/// **colour is inert**: a colour emoji font paints its own hues and ignores the
/// fg we set (the same fact [`override_indicator_spans`] records). The palette
/// only ever reaches a *text* icon — including the default case, since the
/// derived mark is always an emoji — so the editor says so rather than leaving
/// the user to conclude the colour keys are broken.
///
/// An exact table lookup, not a codepoint-range guess: the emoji reaching this
/// field come from the same `emojis` crate the picker is built from, and a
/// heuristic would have to decide what a CJK text icon is. Pure.
pub(super) fn icon_is_emoji(icon: &str) -> bool {
    emojis::get(icon.trim()).is_some()
}

/// Why a row has no window on this screen — the two cases read differently, so
/// the override indicator draws them differently (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detached {
    /// Running on its host with nobody attached: `Enter` takes it, no questions.
    Free,
    /// Some other client holds the pty. The host's attached-bit overlay is what
    /// tells us; taking it back means a steal, which is why it can't wear the
    /// same glyph as a free one.
    HeldElsewhere,
}

/// The secondary indicator's slot: one emoji, 2 cells.
const SECONDARY_SLOT_WIDTH: u16 = 2;

/// Width of the override indicator: the secondary's emoji slot plus the 1-cell
/// follow-up slot after it. It is the *indent* on the status column rather than a
/// column of its own — see [`override_indicator_spans`] for why — so the status
/// column's constraint is this plus the widest status label.
pub(super) const OVERRIDE_COL_WIDTH: u16 = SECONDARY_SLOT_WIDTH + 1;

/// The override indicator, as the leading spans of the **status** cell: always
/// exactly [`OVERRIDE_COL_WIDTH`] cells.
///
/// Two *fixed slots*, not a right-packed run: the secondary indicator (pin or
/// detached) right-aligned in its emoji slot, then the follow-up dot in the
/// single cell before the status label. The dot's cell doubles as the gap — blank
/// when there is no follow-up — which is why the order is this way round. Packed
/// the other way the label's neighbour changed with the flags: a lone dot sat
/// flush against the label while a 2-cell emoji appeared to keep a gap (really
/// just the cell its glyph paints over), so the same column looked inconsistently
/// spaced row to row. Fixed slots give every row the same skyline, and the label
/// starts at the same offset whatever is set.
///
/// Why it rides *inside* the status cell instead of getting a column: ratatui's
/// `column_spacing` is one table-wide value, so a column of its own can only be
/// followed by the same gap every other column gets — a cell that is blank on
/// every row that has no flag, on top of the padding this already emits. Folding
/// it in drops that cell without touching the gap between any other pair. It
/// reads as a status modifier anyway: follow-up already recolours the status
/// label through [`status_fg`].
///
/// Padding here rather than at the call site is what keeps the constraint honest
/// — the return value *is* the width the layout reserves, so the two can't drift.
///
/// Layout note: the invariant is **measured width == painted width**, not "emoji
/// only". The slots pack tight (`[pad][S][S][dot]`) with no separator between
/// them, so any glyph whose paint disagrees with `unicode-width` is overpainted
/// by its neighbour — which is exactly what the configurable
/// `ui.selection_symbol` runs into, and why its default keeps a trailing cell.
/// That is what the Nerd Font PUA glyphs this column used to carry got wrong:
/// they measure 1 and paint 2, so ratatui's diff never skipped the following
/// cell and a neighbouring column's update clipped the glyph's right half —
/// which needed a post-render buffer fix-up in `draw_table` to undo. The
/// secondaries are emoji-presentation by default (measure 2, paint 2), so they
/// hold everywhere. Nothing here may need a VS16 to reach its presentation, for
/// the same reason.
///
/// The dot is a **knowing exception**, not a third safe class. `•` U+2022 is
/// East-Asian-Width *Ambiguous*: `unicode-width` measures it 1, and a terminal
/// configured ambiguous-as-wide paints it 2. Where that holds, the dot's cell
/// eats the one after it and everything right of it on that row — status label
/// included — slides a column, on flagged rows only. Kitty and Ghostty treat
/// ambiguous as narrow by default, which is why this is liveable; a CJK-locale
/// terminal is where it shows. `◉` U+25C9 is the nearest glyph that is
/// unambiguously narrow if that ever needs undoing. The test pins
/// `width_cjk() == 2` so this stays a decision on the record rather than a
/// surprise, and so a swap to some *other* ambiguous glyph still has to come
/// here and read this first.
pub(super) fn override_indicator_spans(
    follow_up: bool,
    pinned: bool,
    detached: Option<Detached>,
) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthStr;
    // The dot is a *text* glyph, so unlike the emoji below it the fg actually
    // paints — which is why it reads from `attention_fg` rather than a hardcoded
    // yellow: it marks the same state `status_fg` recolours the status text for,
    // and the two would drift apart under a remapped palette. The emoji styles
    // only reach a terminal that renders them monochrome; a color emoji font
    // paints its own hues and ignores the fg. Kept anyway so such a terminal
    // still gets the accent.
    let dot = Span::styled(
        "\u{2022}", // • bullet — ambiguous-width; read the layout note above
        Style::default().fg(crate::config::get().colors.ui.attention_fg),
    );
    let pin = Span::styled("\u{1F4CC}", Style::default().fg(Color::Blue)); // 📌 pushpin
    // A pooled session still running on its host with no window on this screen
    // (§9). It joins the existing icon set rather than getting a column of its
    // own, and ranks below the pin: a pin is something the *user* chose,
    // out-of-sight is just where the session happens to be. The glyph says
    // "you can't see this one" rather than naming a transport, because the
    // state is about *where the window is*, not about a link — a pooled
    // localhost session with no window here is detached on the same machine.
    let out_of_sight = Span::styled("\u{1F648}", Style::default().add_modifier(Modifier::DIM)); // 🙈 see-no-evil
    // The same state seen from the other side: out of *our* sight because
    // somebody else's terminal has it. Not dimmed — an out-of-sight row is
    // background, but one held by another client is the row where `Enter`
    // behaves differently (it needs a steal), so it earns the eye's stop. The
    // two glyphs are deliberately a pair: covered eyes vs. someone else's.
    let held_elsewhere = Span::raw("\u{1F440}"); // 👀 eyes
    let secondary = if pinned {
        Some(pin)
    } else {
        match detached {
            Some(Detached::Free) => Some(out_of_sight),
            Some(Detached::HeldElsewhere) => Some(held_elsewhere),
            None => None,
        }
    };

    // Slot one: the secondary, right-aligned so a narrower replacement glyph
    // still sits against the follow-up slot rather than drifting left.
    let used = secondary.as_ref().map_or(0, |s| s.content.width());
    let mut spans = vec![Span::raw(
        " ".repeat((SECONDARY_SLOT_WIDTH as usize).saturating_sub(used)),
    )];
    spans.extend(secondary);
    // Slot two: the follow-up dot, or the blank that separates the secondary
    // from the status label when there is no follow-up.
    spans.push(if follow_up { dot } else { Span::raw(" ") });
    spans
}

/// Display-cell budget for an auto-title folded from the first prompt. A
/// deliberate `/rename` is short by nature and shown in full; a first prompt is
/// unbounded, so it's clipped to a title's worth even in the panels/pickers that
/// otherwise show a name untruncated.
const AUTO_TITLE_MAX: usize = 60;

/// Glance-column / search text for a session's last prompt. Grok Build wraps
/// the typed prompt in `<user_query>` tags; strip those so a row captured
/// before the ingest unwrap still reads as what the user typed.
pub(super) fn last_prompt_text(prompt: &str) -> &str {
    unwrap_user_query(prompt)
}

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
///
/// When `window` is known (Grok's `contextWindowTokens`), pressure is a
/// percentage of that window (70% warning, 90% critical) so a 500k-window
/// row is not painted by Claude-sized absolute token cuts. Without a window
/// the configured token thresholds apply, as they always have.
pub(super) fn context_pressure_style(tokens: u64, window: Option<u64>) -> Style {
    let t = &crate::config::get().thresholds;
    let (warn, crit) = match window.filter(|&w| w > 0) {
        Some(w) => (w * 70 / 100, w * 90 / 100),
        None => (t.context_warning_tokens, t.context_critical_tokens),
    };
    if tokens >= crit {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if tokens >= warn {
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

/// Table cell: a percentage of `window` when the agent reports one (`70%`,
/// fits the 4-wide Ctx column), otherwise the used-token count.
pub(super) fn format_context_cell(tokens: u64, window: Option<u64>) -> String {
    match window.filter(|&w| w > 0) {
        Some(w) => format!("{}%", (tokens.saturating_mul(100) / w).min(100)),
        None => format_tokens(tokens),
    }
}

/// Detail-panel Context line: `351k/500k` when the window is known.
pub(super) fn format_context_detail(tokens: u64, window: Option<u64>) -> String {
    match window.filter(|&w| w > 0) {
        Some(w) => format!("{}/{}", format_tokens(tokens), format_tokens(w)),
        None => format_tokens(tokens),
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

/// Age for a connection-log line: **seconds** below a minute, then the same
/// coarse form everything else uses.
///
/// The extra resolution is the point here and nowhere else. A whole connect
/// attempt — probe, decide, deploy, ensure, forward, handshake — happens inside
/// one minute, so `format_coarse_age`'s `<1m` would stamp the entire story with
/// one identical label and say nothing about how long any step took.
pub(super) fn format_log_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
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

/// Whether `theirs` is an older release than `ours`, by SemVer-ish numeric
/// comparison of the dotted components.
///
/// Exists because a host's `miao-server` now wins on **protocol**
/// compatibility rather than version equality, which is what lets a Nix host's
/// natively-built server survive dashboard upgrades with no deploy. The cost of
/// that looseness is that a stale server on a host's PATH quietly outlives
/// upgrades — and the digest-marker dev loop, which refreshes the *cache* path,
/// never applies to a PATH install. We accept that ("PATH is the user's — you
/// own what you put there") and surface it here instead.
///
/// Deliberately an **annotation, not an error**: it is usually fine, and it is a
/// different severity from an incompatible *running* daemon, which fails the
/// connection outright. Do not conflate the two.
///
/// Numeric per component, because string ordering gets this exactly wrong at
/// every ten: `"0.10.0" < "0.9.0"` lexically. A component that isn't a number
/// (a `-rc.1` suffix, a git describe) compares as equal rather than guessing,
/// so a prerelease never reads as "older" on the strength of its suffix. Pure.
pub(super) fn version_is_older(theirs: &str, ours: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|c| c.parse::<u64>().ok())
            .collect()
    };
    let (t, o) = (parts(theirs), parts(ours));
    // Compare only as far as both actually parsed: a trailing component one
    // side lacks is not evidence of age.
    let n = t.len().min(o.len());
    t[..n] < o[..n]
}

#[cfg(test)]
mod tests {
    use super::{
        format_context_cell, format_context_detail, model_color, model_label, truncate_str,
        version_is_older,
    };
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

    #[test]
    fn an_older_host_server_is_recognised_numerically_not_lexically() {
        assert!(version_is_older("0.2.1", "0.3.0"));
        assert!(version_is_older("0.2.1", "0.2.2"));
        assert!(!version_is_older("0.3.0", "0.3.0"));
        assert!(!version_is_older("0.4.0", "0.3.0"));

        // The whole reason this isn't a string compare: lexically "0.10.0" sorts
        // *below* "0.9.0", so every tenth release would falsely read as stale.
        assert!(!version_is_older("0.10.0", "0.9.0"));
        assert!(version_is_older("0.9.0", "0.10.0"));

        // A component we can't read is not evidence of age — a prerelease
        // suffix must not make a same-version server look old.
        assert!(!version_is_older("0.3.0-rc.1", "0.3.0"));
        assert!(!version_is_older("weird", "0.3.0"));
        assert!(!version_is_older("0.3.0", "weird"));
    }

    #[test]
    fn context_cell_is_a_percentage_of_the_window_when_known() {
        assert_eq!(format_context_cell(350_960, Some(500_000)), "70%");
        assert_eq!(format_context_cell(500_000, Some(500_000)), "100%");
        assert_eq!(format_context_cell(48_100, None), "48k");
        assert_eq!(format_context_detail(350_960, Some(500_000)), "350k/500k");
        assert_eq!(format_context_detail(48_100, None), "48k");
    }
}
