//! The ratatui backend the dashboard draws through: `CrosstermBackend` plus the
//! one fix ratatui hasn't shipped yet for glyphs that are two cells wide.
//!
//! Nothing to do with [`crate::backend::Backend`], the seam that says *where a
//! session runs*. This is ratatui's own backend trait — buffer cells to bytes on
//! stdout — and this module owns terminal setup as a result, since
//! `ratatui::init` hardcodes the stock backend.
//!
//! ## The bug
//!
//! Terminals disagree on the width of an emoji-presentation sequence (one
//! carrying VS16, `U+FE0F` — the header's ☁️, half the host icons): most paint
//! two columns, some paint one. ratatui handles the one-column case by emitting
//! the column the glyph reserved but didn't cover, so it gets cleared. That
//! write comes *after* the glyph, and `CrosstermBackend` suppresses the `MoveTo`
//! before it, because it treats a cell at `x + 1` as contiguous with the one it
//! just printed at `x` — true only of a *narrow* glyph. On a two-column terminal
//! the cursor is already at `x + 2`, so the clear lands a column late: it eats
//! the character beside the glyph, and every following cell the backend believes
//! is contiguous is drawn a column right of where it belongs, until the next
//! `MoveTo` re-anchors it. The damage sticks, because the diff only rewrites
//! cells whose *buffer* content changed.
//!
//! The header is where it shows. Its right cluster is right-aligned, so ☁️ and
//! the host icon slide sideways whenever the tally, the layout label or the
//! default host changes width — onto a column that held a digit last frame,
//! which is what makes the reserved column's symbol differ and fires the clear.
//!
//! ## The fix
//!
//! Emit the reserved column *before* the glyph. A two-column terminal paints
//! over it; a one-column terminal leaves it cleared. Both get a `MoveTo` for the
//! glyph itself, since it now follows a write at a *higher* column, so a width
//! disagreement can no longer shift the rest of the row. This is ratatui#2686's
//! approach, applied from out here where we can only reorder what the diff hands
//! us — see ratatui#2651 for the backend-side report and ratatui#2357 for the
//! user-visible face of it, hit on kitty, wezterm, foot, Konsole and Ghostty.
//!
//! Retire this module when a ratatui release carries the fix: the reordering
//! becomes a no-op (the reserved column already arrives first, so the condition
//! never matches) rather than a conflict, so there is no rush and no breakage.
//! Correcting the *backend's* cursor arithmetic is not a substitute — ratatui#2686
//! measured that on a two-column terminal it turns the late write into an
//! explicit `MoveTo` onto the glyph's second column, destroying the glyph.

use std::io::{self, Stdout, Write, stdout};

use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellWidth};
use ratatui::layout::{Position, Size};

/// The dashboard's terminal — `ratatui::DefaultTerminal` with this module's
/// backend in place of the stock one.
pub(super) type DashboardTerminal = Terminal<WideGlyphBackend<Stdout>>;

/// Raw mode, alternate screen, restore-on-panic, then the terminal — the same
/// four steps `ratatui::init` takes, which can't be reused because it builds the
/// stock backend. Panics on the same failures it does; `ratatui::restore` is
/// backend-agnostic and still undoes all of it.
pub(super) fn init() -> DashboardTerminal {
    // Before raw mode, so a failure below still restores.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        hook(info);
    }));
    enable_raw_mode().expect("failed to enable raw mode");
    execute!(stdout(), EnterAlternateScreen).expect("failed to enter the alternate screen");
    Terminal::new(WideGlyphBackend(CrosstermBackend::new(stdout())))
        .expect("failed to size the terminal")
}

/// `CrosstermBackend` with the reordering in the module docs. Every other method
/// forwards untouched.
///
/// `Backend` gains two more required methods under ratatui's
/// `scrolling-regions` feature, which nothing in this workspace turns on. If
/// something ever does, this impl stops compiling for want of
/// `scroll_region_up`/`scroll_region_down` — forward them to `self.0` like the
/// rest. They can't be written ahead of time: the feature belongs to
/// `ratatui-core`, so there is no `cfg` we can name from here.
pub(super) struct WideGlyphBackend<W: Write>(CrosstermBackend<W>);

impl<W: Write> Backend for WideGlyphBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.0.draw(ReservedColumnFirst::new(content))
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.0.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.0.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.0.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.0.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.0.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.0.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.0.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        // `CrosstermBackend` is also a `Write`, whose `flush` means something
        // else — this is the one that drains the queued frame.
        Backend::flush(&mut self.0)
    }
}

/// Yields a wide cell's reserved column ahead of the cell itself, and passes
/// everything else through in order.
///
/// One slot of lookahead is enough: `unicode-width` never measures a grapheme
/// past two cells, so a wide cell reserves exactly one column, and the diff
/// emits it — when it emits it at all — directly after the cell it belongs to.
struct ReservedColumnFirst<I: Iterator> {
    inner: I,
    /// Pulled to look ahead, not yet yielded.
    peeked: Option<I::Item>,
    /// A wide cell held back behind the column it covers.
    deferred: Option<I::Item>,
}

impl<I: Iterator> ReservedColumnFirst<I> {
    const fn new(inner: I) -> Self {
        Self {
            inner,
            peeked: None,
            deferred: None,
        }
    }
}

impl<'a, I> Iterator for ReservedColumnFirst<I>
where
    I: Iterator<Item = (u16, u16, &'a Cell)>,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(deferred) = self.deferred.take() {
            return Some(deferred);
        }
        let current = self.peeked.take().or_else(|| self.inner.next())?;
        let (x, y, cell) = current;
        if cell.cell_width() > 1 {
            let ahead = self.inner.next();
            // The one write that has to go first: inside this glyph, on its row.
            if let Some((ax, ay, _)) = ahead
                && ay == y
                && ax == x + 1
            {
                self.deferred = Some(current);
                return ahead;
            }
            self.peeked = ahead;
        }
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Alignment, Rect};
    use ratatui::style::{Color, Style};
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};
    use unicode_width::UnicodeWidthStr;

    /// A screen that paints the way a real terminal does, so an update stream
    /// can be replayed onto it and compared with a full repaint of the buffer
    /// that produced it.
    ///
    /// `emoji_cols` is the width it paints a VS16 sequence at — the whole
    /// disagreement this module exists for. Every scenario runs against both.
    struct Screen {
        cols: Vec<String>,
        cursor: usize,
        last: Option<(u16, u16)>,
        emoji_cols: usize,
    }

    /// A column covered by the glyph to its left, which owns it.
    const COVERED: &str = "";

    impl Screen {
        fn new(width: u16, emoji_cols: usize) -> Self {
            Self {
                cols: vec![" ".to_string(); width as usize],
                cursor: 0,
                last: None,
                emoji_cols,
            }
        }

        /// What this terminal actually paints a symbol at, as opposed to what
        /// `unicode-width` measures it as (which is what the buffer reserved).
        fn painted(&self, symbol: &str) -> usize {
            if symbol.contains('\u{fe0f}') {
                self.emoji_cols
            } else {
                symbol.width().max(1)
            }
        }

        /// Blank the whole glyph occupying column `i` — from either half. A
        /// terminal has no way to draw part of one, so a write that touches a
        /// wide glyph takes all of it.
        fn clear_glyph_at(&mut self, i: usize) {
            let owner = if self.cols[i] == COVERED {
                self.cols[..i]
                    .iter()
                    .rposition(|col| col != COVERED)
                    .expect("a covered column has an owner")
            } else {
                i
            };
            self.cols[owner] = " ".to_string();
            let mut j = owner + 1;
            while j < self.cols.len() && self.cols[j] == COVERED {
                self.cols[j] = " ".to_string();
                j += 1;
            }
        }

        /// Paint at the cursor and advance it, taking out whatever glyphs the
        /// write lands on.
        fn paint(&mut self, symbol: &str) {
            if self.cursor >= self.cols.len() {
                return;
            }
            let width = self.painted(symbol);
            for k in 0..width {
                if self.cursor + k < self.cols.len() {
                    self.clear_glyph_at(self.cursor + k);
                }
            }
            self.cols[self.cursor] = symbol.to_string();
            for k in 1..width {
                if let Some(col) = self.cols.get_mut(self.cursor + k) {
                    *col = COVERED.to_string();
                }
            }
            self.cursor += width;
        }

        /// One update, positioned the way `CrosstermBackend::draw` positions it:
        /// a `MoveTo` unless this cell is the one right after the last.
        fn feed(&mut self, x: u16, y: u16, symbol: &str) {
            if !matches!(self.last, Some((lx, ly)) if x == lx + 1 && y == ly) {
                self.cursor = x as usize;
            }
            self.last = Some((x, y));
            self.paint(symbol);
        }

        /// A full repaint of `buf` (one row, which is all these cases need) —
        /// the reference every incremental update has to agree with. Skips the
        /// columns a wide cell reserved, since painting their blanks would erase
        /// the glyph that reserved them.
        fn repaint(buf: &Buffer, emoji_cols: usize) -> Self {
            let mut screen = Self::new(buf.area.width, emoji_cols);
            let mut x = 0;
            while x < buf.area.width {
                let symbol = buf[(x, 0)].symbol().to_string();
                screen.cursor = x as usize;
                screen.paint(&symbol);
                x += symbol.width().max(1) as u16;
            }
            screen
        }
    }

    /// A right-aligned bar on a filled background, like the header's. The
    /// background matters: it is what puts ratatui's diff on the path that
    /// force-clears a wide glyph's columns when narrower content replaces it,
    /// so these cases exercise that alongside the reordering.
    fn bar(text: &str, width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        Paragraph::new(Line::from(text.to_string()))
            .alignment(Alignment::Right)
            .style(Style::default().bg(Color::Rgb(49, 50, 68)))
            .render(area, &mut buf);
        buf
    }

    /// An incremental update has to leave the screen where a full repaint would,
    /// on a terminal that paints these emoji at either width. Without the
    /// reordering the two-column case drifts (the header artifact); with the
    /// reserved column simply dropped, the one-column case keeps stale debris in
    /// it — the case ratatui's own workaround exists for.
    #[test]
    fn an_update_lands_where_a_repaint_would_at_either_emoji_width() {
        const WIDTH: u16 = 44;
        // The header's right cluster, at every width its parts take: the tally
        // gaining a bucket, the host name growing, the keep-awake ☕ arriving.
        // Each slides the glyphs left of it onto columns that held a digit.
        let shifts = [
            "Claude  \u{2601}\u{fe0f}1  host: \u{1f4e6} polaris",
            "Claude  \u{2601}\u{fe0f}1 1  host: \u{1f4e6} polaris",
            "Claude  \u{2601}\u{fe0f}1  host: \u{1f4e6} pol",
            "Claude  \u{2601}\u{fe0f}1  host: \u{1f4e6} polaris  \u{2615}",
            "Codex  \u{2601}\u{fe0f}0 2  host: \u{1f5a5}\u{fe0f} polaris",
        ];
        for emoji_cols in [2, 1] {
            for a in shifts {
                for b in shifts {
                    let (prev, next) = (bar(a, WIDTH), bar(b, WIDTH));
                    let mut screen = Screen::repaint(&prev, emoji_cols);
                    screen.cursor = 0;
                    screen.last = None;
                    let updates = prev.diff(&next);
                    for (x, y, cell) in super::ReservedColumnFirst::new(updates.into_iter()) {
                        screen.feed(x, y, cell.symbol());
                    }
                    let want = Screen::repaint(&next, emoji_cols);
                    assert_eq!(
                        screen.cols.concat(),
                        want.cols.concat(),
                        "at {emoji_cols} cols per emoji\n  from {a:?}\n  to   {b:?}",
                    );
                }
            }
        }
    }

    /// The reordering only moves a write that lands *inside* a glyph; anything
    /// else keeps its order, including the trailing clears ratatui emits when a
    /// wide glyph is replaced by something narrower (there the cell it follows
    /// is narrow, so the backend's positioning was right all along).
    #[test]
    fn nothing_but_a_covered_column_changes_places() {
        let (prev, next) = (bar("a \u{2601}\u{fe0f} b", 12), bar("ab \u{1f4e6} c", 12));
        let updates = prev.diff(&next);
        let before: Vec<u16> = updates.iter().map(|(x, _, _)| *x).collect();
        let after: Vec<u16> = super::ReservedColumnFirst::new(updates.clone().into_iter())
            .map(|(x, _, _)| x)
            .collect();
        // Same set of writes, every time — reordering must never drop or add one.
        let (mut s_before, mut s_after) = (before.clone(), after.clone());
        s_before.sort_unstable();
        s_after.sort_unstable();
        assert_eq!(s_before, s_after, "{before:?} -> {after:?}");
        // And the only backwards step is the single column a swap moves: a
        // glyph landing right after the column it covers.
        for pair in after.windows(2) {
            let stepped_back = pair[0].saturating_sub(pair[1]);
            assert!(stepped_back <= 1, "{before:?} -> {after:?}");
        }
    }
}
