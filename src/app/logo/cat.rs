//! Full-color walk sheets and their lane geometry. Artwork is prepared offline;
//! the dashboard embeds straight-alpha RGBA without an image-decoder dependency.

use ratatui::layout::Rect;

use crate::terminal::graphics::{CellSize, Placement};

const FRAME_W: u32 = 96;
const FRAME_H: u32 = 64;
const FRAMES: u32 = 8;
const FRAME_MS: u128 = 100;
const SPEED_CELLS_PER_S: f64 = 6.0;
pub(super) const RARE_ONE_IN: u64 = 20;

const SHEETS: [&[u8]; 3] = [
    include_bytes!("../../../assets/logo/cats/tabby.rgba"),
    include_bytes!("../../../assets/logo/cats/tuxedo.rgba"),
    include_bytes!("../../../assets/logo/cats/pink.rgba"),
];

/// Select a coat once per summon; the pink kitten remains a rare surprise.
pub(super) fn select_coat(random: u64) -> usize {
    if random.is_multiple_of(RARE_ONE_IN) {
        2
    } else {
        ((random >> 8) % 2) as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameSize {
    pub width: u32,
    pub height: u32,
}

impl FrameSize {
    pub fn for_cell(cell: CellSize) -> Self {
        let height = u32::from(cell.h).clamp(1, FRAME_H);
        Self {
            width: (FRAME_W * height / FRAME_H).max(1),
            height,
        }
    }

    /// Downsample each frame independently so filtering never bleeds between
    /// poses. Average premultiplied channels, then unpremultiply for kitty's
    /// straight-alpha protocol; transparent edges cannot produce dark halos.
    pub fn sheet(self, coat: usize) -> Vec<u8> {
        let source = SHEETS[coat];
        let sheet_width = self.width * FRAMES;
        let mut rgba = vec![0; (sheet_width * self.height * 4) as usize];
        for frame in 0..FRAMES {
            for y in 0..self.height {
                for x in 0..self.width {
                    let mut channels = [0u32; 3];
                    let mut alpha = 0;
                    let mut samples = 0;
                    for sy in y * FRAME_H / self.height..(y + 1) * FRAME_H / self.height {
                        for sx in x * FRAME_W / self.width..(x + 1) * FRAME_W / self.width {
                            let at = ((sy * FRAME_W * FRAMES + frame * FRAME_W + sx) * 4) as usize;
                            let a = u32::from(source[at + 3]);
                            for c in 0..3 {
                                channels[c] += u32::from(source[at + c]) * a;
                            }
                            alpha += a;
                            samples += 1;
                        }
                    }
                    let at = ((y * sheet_width + frame * self.width + x) * 4) as usize;
                    for c in 0..3 {
                        rgba[at + c] =
                            (channels[c] + alpha / 2).checked_div(alpha).unwrap_or(0) as u8;
                    }
                    rgba[at + 3] = ((alpha + samples / 2) / samples) as u8;
                }
            }
        }
        rgba
    }

    pub fn sheet_width(self) -> u32 {
        self.width * FRAMES
    }

    /// A native-pixel placement preserves smooth sub-cell movement. Crop the
    /// outgoing pose at the lane edge instead of letting kitty wrap or rescale it.
    pub fn placement(
        self,
        image: u32,
        elapsed_ms: u128,
        track: Rect,
        cell: CellSize,
    ) -> Option<Placement> {
        if track.is_empty() {
            return None;
        }
        let cell_w = u32::from(cell.w).max(1);
        let track_px = u32::from(track.width) * cell_w;
        let x = (elapsed_ms as f64 * SPEED_CELLS_PER_S * f64::from(cell_w) / 1000.0) as u32;
        if x >= track_px {
            return None;
        }
        let frame = (elapsed_ms / FRAME_MS % u128::from(FRAMES)) as u32;
        Some(Placement {
            image,
            placement: 2,
            col: track.x + (x / cell_w) as u16,
            row: track.y,
            cells: None,
            z: 1,
            crop: Some((
                frame * self.width,
                0,
                self.width.min(track_px - x),
                self.height,
            )),
            offset: (
                (x % cell_w) as u16,
                cell.h.saturating_sub(self.height as u16),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheets_have_distinct_poses_color_and_transparency() {
        for sheet in SHEETS {
            assert_eq!(sheet.len(), (FRAME_W * FRAME_H * FRAMES * 4) as usize);
            let frame_bytes = |frame: u32| -> Vec<u8> {
                (0..FRAME_H)
                    .flat_map(|y| {
                        let start = ((y * FRAME_W * FRAMES + frame * FRAME_W) * 4) as usize;
                        sheet[start..start + (FRAME_W * 4) as usize].iter().copied()
                    })
                    .collect()
            };
            let first = frame_bytes(0);
            for frame in 1..FRAMES {
                assert_ne!(first, frame_bytes(frame), "repeated pose {frame}");
            }
            assert!(sheet.as_chunks::<4>().0.iter().any(|p| p[3] == 0));
            assert!(
                sheet
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| p[3] == 255 && p[0] != p[1])
            );
        }
    }

    #[test]
    fn scaled_sheets_fit_small_and_large_font_sizes() {
        for height in [1, 16, 24, 40, 64, 96] {
            let size = FrameSize::for_cell(CellSize { w: 8, h: height });
            assert!(size.height <= u32::from(height));
            for coat in 0..SHEETS.len() {
                let sheet = size.sheet(coat);
                assert_eq!(sheet.len(), (size.sheet_width() * size.height * 4) as usize);
                assert!(sheet.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
            }
        }
        let native = FrameSize::for_cell(CellSize { w: 32, h: 64 });
        assert_eq!(native.sheet(0), SHEETS[0]);
    }

    #[test]
    fn walk_advances_frames_and_clips_at_the_lane_edge() {
        let cell = CellSize { w: 8, h: 16 };
        let size = FrameSize::for_cell(cell);
        let track = Rect::new(3, 1, 10, 1);
        let start = size.placement(42, 0, track, cell).unwrap();
        assert_eq!((start.col, start.row), (3, 1));
        assert_eq!(start.crop, Some((0, 0, 24, 16)));
        let moving = size.placement(42, 100, track, cell).unwrap();
        assert_eq!(moving.offset, (4, 0));
        assert_eq!(moving.crop, Some((24, 0, 24, 16)));
        let exiting = size.placement(42, 1600, track, cell).unwrap();
        assert_eq!(exiting.col, 12);
        assert_eq!(exiting.crop, Some((0, 0, 4, 16)));
        assert!(size.placement(42, 1700, track, cell).is_none());
        assert!(size.placement(42, 0, Rect::default(), cell).is_none());
    }

    #[test]
    fn tiny_lane_clips_from_the_first_frame() {
        let cell = CellSize { w: 8, h: 96 };
        let size = FrameSize::for_cell(cell);
        let p = size.placement(42, 0, Rect::new(0, 1, 1, 1), cell).unwrap();
        assert_eq!(p.crop, Some((0, 0, 8, 64)));
        assert_eq!(p.offset, (0, 32));
    }
}
