//! Two-column harness artwork in the session table.
//!
//! Private-use markers travel through ratatui's table layout and overlay clears.
//! At the end of `draw`, we replace surviving markers with spaces and record
//! their exact cells. Both cells must survive, so a partially covered logo stays
//! hidden. Markers never reach the terminal. This lets ratatui own column
//! widths, scrolling and occlusion without a second copy of its layout algorithm.
//! Only this module emits these markers, and collection is confined to the table.
//!
//! Graphics are flushed after the text frame. Images are shared by harness and
//! dim state; placements are reconciled on every draw and freed on teardown.

use std::io::Write;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::widgets::Cell;

use crate::agent::AgentControl;
use crate::terminal::graphics::{self, CellSize, Placement};

const SIZE: u32 = 32;
pub(super) const COLUMNS: u16 = 2;
const END_MARKER: &str = "\u{10fffd}";
const FIRST_IMAGE: u32 = 7200; // Separate from the header's paw/cat pool.
const SOURCES: [&[u8]; 9] = [
    include_bytes!("../../assets/agents/claude.rgba"),
    include_bytes!("../../assets/agents/codex.rgba"),
    include_bytes!("../../assets/agents/reasonix.rgba"),
    include_bytes!("../../assets/agents/kimi.rgba"),
    include_bytes!("../../assets/agents/opencode.rgba"),
    include_bytes!("../../assets/agents/pi.rgba"),
    include_bytes!("../../assets/agents/antigravity.rgba"),
    include_bytes!("../../assets/agents/omp.rgba"),
    include_bytes!("../../assets/agents/grok.rgba"),
];
const MARKERS: [&str; 9] = [
    "\u{100000}",
    "\u{100001}",
    "\u{100002}",
    "\u{100003}",
    "\u{100004}",
    "\u{100005}",
    "\u{100006}",
    "\u{100007}",
    "\u{100008}",
];

/// Both image cells are blank by the time ratatui flushes its frame.
pub(super) fn cell(agent: AgentControl) -> Cell<'static> {
    let marker = match agent {
        AgentControl::Claude => MARKERS[0],
        AgentControl::Codex => MARKERS[1],
        AgentControl::Reasonix => MARKERS[2],
        AgentControl::Kimi => MARKERS[3],
        AgentControl::OpenCode => MARKERS[4],
        AgentControl::Pi => MARKERS[5],
        AgentControl::Antigravity => MARKERS[6],
        AgentControl::Omp => MARKERS[7],
        AgentControl::Grok => MARKERS[8],
        AgentControl::Unknown => return Cell::from("?"),
    };
    // The tag prevents a matching private-use character in a user title from
    // being interpreted as one of our placeholders.
    Cell::from(format!("{marker}{END_MARKER}"))
        .style(Style::default().add_modifier(Modifier::HIDDEN))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    icon: usize,
    pos: Position,
}

impl Target {
    fn image(self) -> u32 {
        FIRST_IMAGE + self.icon as u32
    }

    fn placement(self) -> u32 {
        u32::from(self.pos.y) + 1
    }
}

#[derive(Default)]
pub(crate) struct AgentLogos {
    desired: Vec<Target>,
    placed: Vec<Target>,
    uploaded: [bool; SOURCES.len() * 2],
    size: Option<CellSize>,
    reset: bool,
}

impl AgentLogos {
    pub(super) fn collect(&mut self, buffer: &mut Buffer, area: Rect) {
        self.desired.clear();
        let area = area.intersection(buffer.area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let cell = &buffer[(x, y)];
                if !cell.modifier.contains(Modifier::HIDDEN) {
                    continue;
                }
                let kind = MARKERS.iter().position(|m| *m == cell.symbol());
                if let Some(kind) = kind {
                    let dim = cell.modifier.contains(Modifier::DIM);
                    let end_x = x.saturating_add(COLUMNS - 1);
                    if end_x < area.right()
                        && buffer[(end_x, y)].symbol() == END_MARKER
                        && buffer[(end_x, y)].modifier.contains(Modifier::HIDDEN)
                    {
                        self.desired.push(Target {
                            icon: kind * 2 + usize::from(dim),
                            pos: Position::new(x, y),
                        });
                    }
                }
                if kind.is_some() || cell.symbol() == END_MARKER {
                    let cell = &mut buffer[(x, y)];
                    cell.set_symbol(" ");
                    cell.modifier.remove(Modifier::HIDDEN);
                }
            }
        }
    }

    /// Screen clears on resize can free Kitty's image data as well as placements.
    pub(super) fn invalidate(&mut self) {
        self.reset |= self.size.is_some();
    }

    pub(super) fn render(
        &mut self,
        out: &mut impl Write,
        size: Option<CellSize>,
    ) -> std::io::Result<()> {
        let moves_cursor = size.is_some()
            && !self.desired.is_empty()
            && (self.reset || self.size != size || self.desired != self.placed);
        if moves_cursor {
            out.write_all(b"\x1b7")?;
        }
        let result = self.render_inner(out, size);
        // Image placements address screen cells. Preserve ratatui's cursor,
        // including an active search/editor cursor, even after a failed upload.
        let restored = if moves_cursor {
            out.write_all(b"\x1b8").and_then(|()| out.flush())
        } else {
            Ok(())
        };
        let result = result.and(restored);
        if result.is_err() {
            // A partial write may have created placements we could not record.
            // Clear our entire ID range before retrying, including partial uploads.
            self.invalidate();
        }
        result
    }

    fn render_inner(
        &mut self,
        out: &mut impl Write,
        size: Option<CellSize>,
    ) -> std::io::Result<()> {
        if self.reset || self.size != size {
            self.clear(out)?;
            self.size = size;
        }
        let Some(size) = size.filter(|s| s.w > 0 && s.h > 0) else {
            return Ok(());
        };
        for old in &self.placed {
            if !self.desired.contains(old) {
                graphics::delete_placement_to(out, old.image(), old.placement())?;
            }
        }
        for target in &self.desired {
            if !self.uploaded[target.icon] {
                let (w, h, pixels) = canvas(SOURCES[target.icon / 2], size, target.icon % 2 == 1);
                graphics::transmit_rgba_to(out, target.image(), w, h, &pixels)?;
                self.uploaded[target.icon] = true;
            }
            if !self.placed.contains(target) {
                graphics::place_to(
                    out,
                    &Placement {
                        image: target.image(),
                        placement: target.placement(),
                        col: target.pos.x,
                        row: target.pos.y,
                        cells: Some((COLUMNS, 1)),
                        z: 1,
                        crop: None,
                        offset: (0, 0),
                    },
                )?;
            }
        }
        out.flush()?;
        self.placed.clone_from(&self.desired);
        Ok(())
    }

    pub(super) fn clear(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        // Include partial uploads after a failed write, but never emit graphics
        // into an unsupported terminal that has not displayed any of our images.
        if self.size.is_some() || self.reset {
            for i in 0..self.uploaded.len() {
                graphics::free_image_to(out, FIRST_IMAGE + i as u32)?;
            }
            out.flush()?;
        }
        self.uploaded.fill(false);
        self.placed.clear();
        self.reset = false;
        Ok(())
    }
}

/// Remove transparent margins, then fit the artwork to two columns and one row.
/// Limit artwork to 75% of the row height so adjacent logos stay separated.
/// All source pixels retain their color; dim rows reduce alpha instead.
fn canvas(source: &[u8], cell: CellSize, dim: bool) -> (u32, u32, Vec<u8>) {
    let (mut left, mut top, mut right, mut bottom) = (SIZE, SIZE, 0, 0);
    for y in 0..SIZE {
        for x in 0..SIZE {
            if source[((y * SIZE + x) * 4 + 3) as usize] > 0 {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + 1);
                bottom = bottom.max(y + 1);
            }
        }
    }
    if right == 0 {
        return (1, 1, vec![0; 4]);
    }
    let (art_w, art_h) = (right - left, bottom - top);
    let (cell_w, cell_h) = (
        u32::from(cell.w).max(1) * u32::from(COLUMNS),
        u32::from(cell.h).max(1),
    );
    let min_h = (art_h * 4).div_ceil(3).max(art_h + 2);
    let (w, h) = if art_w * cell_h >= min_h * cell_w {
        (art_w, (art_w * cell_h).div_ceil(cell_w).min(1024))
    } else {
        ((min_h * cell_w).div_ceil(cell_h).min(1024), min_h)
    };
    let mut rgba = vec![0; (w * h * 4) as usize];
    let pad_x = (w - art_w) / 2;
    let pad_y = (h - art_h) / 2;
    for y in 0..art_h {
        for x in 0..art_w {
            let src = (((y + top) * SIZE + x + left) * 4) as usize;
            let dst = (((y + pad_y) * w + x + pad_x) * 4) as usize;
            rgba[dst..dst + 4].copy_from_slice(&source[src..src + 4]);
            if dim {
                rgba[dst + 3] /= 2;
            }
        }
    }
    (w, h, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::widgets::{Clear, Widget};

    const CELL: CellSize = CellSize { w: 8, h: 16 };

    fn mark(buf: &mut Buffer, x: u16, y: u16, kind: usize, dim: bool) {
        let end = &mut buf[(x + COLUMNS - 1, y)];
        end.set_symbol(END_MARKER);
        end.modifier = Modifier::HIDDEN;
        let c = &mut buf[(x, y)];
        c.set_symbol(MARKERS[kind]);
        c.modifier = Modifier::HIDDEN;
        if dim {
            c.modifier.insert(Modifier::DIM);
        }
    }

    #[test]
    fn overlays_remove_markers_and_dim_rows_select_dim_artwork() {
        let area = Rect::new(0, 0, 10, 5);
        let mut buffer = Buffer::empty(area);
        mark(&mut buffer, 2, 1, 0, false);
        mark(&mut buffer, 2, 2, 1, true);
        mark(&mut buffer, 2, 3, 2, false);
        mark(&mut buffer, 2, 4, 3, false);
        buffer[(8, 1)].set_symbol(MARKERS[0]); // A user title, without our tag.
        Clear.render(Rect::new(2, 3, 1, 1), &mut buffer); // Covered first cell.
        Clear.render(Rect::new(3, 4, 1, 1), &mut buffer); // Covered second cell.
        buffer[(3, 4)].set_symbol("M"); // Popup text must survive collection.
        buffer[(9, 0)].set_symbol(MARKERS[0]); // Second cell clipped offscreen.
        buffer[(9, 0)].modifier = Modifier::HIDDEN;
        let mut logos = AgentLogos::default();
        logos.collect(&mut buffer, area);
        assert_eq!(
            logos.desired,
            vec![
                Target {
                    icon: 0,
                    pos: Position::new(2, 1)
                },
                Target {
                    icon: 3,
                    pos: Position::new(2, 2)
                },
            ]
        );
        assert_eq!(buffer[(2, 1)].symbol(), " ");
        assert!(!buffer[(2, 1)].modifier.contains(Modifier::HIDDEN));
        assert_eq!(buffer[(8, 1)].symbol(), MARKERS[0]);
        for pos in [(3, 1), (3, 3), (2, 4), (9, 0)] {
            assert_eq!(buffer[pos].symbol(), " ");
            assert!(!buffer[pos].modifier.contains(Modifier::HIDDEN));
        }
        assert_eq!(buffer[(3, 4)].symbol(), "M");
        logos.collect(&mut buffer, area);
        assert!(logos.desired.is_empty());
    }

    #[test]
    fn uploads_are_shared_and_stale_placements_are_deleted() {
        let mut logos = AgentLogos {
            desired: vec![
                Target {
                    icon: 0,
                    pos: Position::new(20, 4),
                },
                Target {
                    icon: 0,
                    pos: Position::new(20, 5),
                },
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        logos.render(&mut out, Some(CELL)).unwrap();
        let wire = String::from_utf8(out.clone()).unwrap();
        assert_eq!(wire.matches("a=t").count(), 1);
        assert_eq!(wire.matches("a=p").count(), 2);
        assert!(wire.contains("c=2,r=1"));
        assert!(wire.starts_with("\x1b7") && wire.ends_with("\x1b8"));
        out.clear();
        logos.render(&mut out, Some(CELL)).unwrap();
        assert!(out.is_empty(), "an unchanged frame should emit no graphics");

        logos.desired.remove(0); // Scrolled, filtered or covered by an overlay.
        logos.desired[0].pos.x = 22; // Column moved after layout change.
        logos.render(&mut out, Some(CELL)).unwrap();
        let wire = String::from_utf8(out).unwrap();
        assert!(wire.contains("a=d,d=i,i=7200,p=5,q=2"));
        assert!(wire.contains("a=d,d=i,i=7200,p=6,q=2"));
        assert!(wire.contains("\x1b[6;23H"));
        assert_eq!(wire.matches("a=p").count(), 1);
        assert!(!wire.contains("a=t"));

        logos.desired.clear(); // Preference off / narrow layout / no rows.
        let mut out = Vec::new();
        logos.render(&mut out, Some(CELL)).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("a=d,d=i,i=7200,p=6")
        );
        assert!(logos.placed.is_empty());
    }

    #[test]
    fn resize_reuploads_and_unsupported_terminals_emit_nothing() {
        let mut logos = AgentLogos::default();
        let mut out = Vec::new();
        logos.invalidate();
        logos.render(&mut out, None).unwrap();
        logos.clear(&mut out).unwrap();
        assert!(out.is_empty());
        logos.desired.push(Target {
            icon: 2,
            pos: Position::new(10, 3),
        });
        logos.render(&mut out, Some(CELL)).unwrap();
        out.clear();
        logos.invalidate();
        logos.render(&mut out, Some(CELL)).unwrap();
        let wire = String::from_utf8(out).unwrap();
        assert!(wire.contains("a=d,d=I,i=7202"));
        assert!(wire.contains("i=7202,a=t"));
        assert!(wire.contains("a=p,i=7202"));
    }

    #[test]
    fn failed_upload_restores_cursor_and_retries_from_clean_images() {
        struct FailOnce(bool, Vec<u8>);
        impl Write for FailOnce {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.0 && self.1.len() > 100 {
                    self.0 = true;
                    return Err(std::io::Error::other("test upload interrupted"));
                }
                self.1.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut logos = AgentLogos {
            desired: vec![Target {
                icon: 0,
                pos: Position::new(20, 4),
            }],
            ..Default::default()
        };
        let mut broken = FailOnce(false, Vec::new());
        assert!(logos.render(&mut broken, Some(CELL)).is_err());
        assert!(broken.1.ends_with(b"\x1b8"));
        let mut out = Vec::new();
        logos.render(&mut out, Some(CELL)).unwrap();
        let wire = String::from_utf8(out).unwrap();
        assert!(wire.find("a=d,d=I,i=7200").unwrap() < wire.find("a=t").unwrap());
        assert!(wire.contains("a=p,i=7200"));
    }

    #[test]
    fn assets_keep_vertical_padding_proportions_and_row_dimming() {
        for source in SOURCES {
            assert_eq!(source.len(), (SIZE * SIZE * 4) as usize);
            assert!(source.as_chunks::<4>().0.iter().any(|px| px[3] > 0));
            for cell in [
                CELL,
                CellSize { w: 12, h: 24 },
                CellSize { w: 19, h: 38 },
                CellSize { w: 20, h: 10 },
            ] {
                let (w, h, pixels) = canvas(source, cell, false);
                let rows: Vec<_> = pixels
                    .chunks_exact(w as usize * 4)
                    .enumerate()
                    .filter(|(_, row)| row.as_chunks::<4>().0.iter().any(|px| px[3] > 0))
                    .map(|(y, _)| y as u32)
                    .collect();
                let top = *rows.first().unwrap();
                let bottom = *rows.last().unwrap();
                assert!(top > 0 && bottom < h - 1, "padding above and below");
                assert!((bottom - top + 1) * 4 <= h * 3, "at most 75% tall");
            }
        }
        let solid = vec![255; (SIZE * SIZE * 4) as usize];
        // Remove baked margins, then center the visible artwork with row padding.
        let mut padded = vec![0; solid.len()];
        for y in 6..26 {
            for x in 10..22 {
                let offset = ((y * SIZE + x) * 4) as usize;
                padded[offset..offset + 4].fill(255);
            }
        }
        for (cell, expected) in [(CELL, (27, 27)), (CellSize { w: 20, h: 10 }, (108, 27))] {
            let (w, h, pixels) = canvas(&padded, cell, false);
            assert_eq!((w, h), expected);
            let visible: Vec<_> = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .enumerate()
                .filter(|(_, p)| p[3] > 0)
                .map(|(i, _)| (i as u32 % w, i as u32 / w))
                .collect();
            assert_eq!(visible.len(), 12 * 20);
            assert_eq!(visible.first(), Some(&((w - 12) / 2, (h - 20) / 2)));
            assert_eq!(visible.last(), Some(&((w + 12) / 2 - 1, (h + 20) / 2 - 1)));
        }
        assert_eq!(
            canvas(&vec![0; solid.len()], CELL, false),
            (1, 1, vec![0; 4])
        );
        for (cell, expected) in [(CELL, (43, 43)), (CellSize { w: 20, h: 10 }, (172, 43))] {
            let (w, h, pixels) = canvas(&solid, cell, false);
            assert_eq!((w, h), expected);
            assert_eq!(
                w * u32::from(cell.h),
                h * u32::from(cell.w) * u32::from(COLUMNS)
            );
            assert_eq!(
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[3] > 0)
                    .count(),
                (SIZE * SIZE) as usize
            );
            let (_, _, dim) = canvas(&solid, cell, true);
            for (a, b) in pixels.as_chunks::<4>().0.iter().zip(dim.as_chunks::<4>().0) {
                assert_eq!(&a[..3], &b[..3]);
                assert_eq!(b[3], a[3] / 2);
            }
        }
    }
}
