//! Full-color walk sheets and their lane geometry. Artwork is prepared offline;
//! the dashboard expands lossless XZ sheets once per coat using its existing decoder.

use std::sync::OnceLock;

use ratatui::layout::Rect;

use crate::terminal::graphics::{CellSize, Placement};

const FRAME_W: u32 = 48;
const FRAME_H: u32 = 32;
const MAX_DISPLAY_H: u32 = 64;
const FRAMES: u32 = 8;
// Pose playback and travel are tuned independently. Travel is specified at the
// stored 32px height and scales with the displayed sprite.
const FRAMES_PER_SECOND: u128 = 14;
const TRAVEL_PX_PER_SECOND: u32 = 50;

const PACKED: [&[u8]; 4] = [
    include_bytes!("../../../assets/logo/cats/tabby.rgba.xz"),
    include_bytes!("../../../assets/logo/cats/tuxedo.rgba.xz"),
    include_bytes!("../../../assets/logo/cats/calico.rgba.xz"),
    include_bytes!("../../../assets/logo/cats/pink.rgba.xz"),
];
static SHEETS: [OnceLock<Vec<u8>>; 4] = [const { OnceLock::new() }; 4];

fn source_sheet(coat: usize) -> &'static [u8] {
    SHEETS[coat].get_or_init(|| {
        // Use the same reader/writer types as server-payload inflation so the
        // release binary can share the decoder's monomorphized implementation.
        let mut rgba = Vec::with_capacity((FRAME_W * FRAME_H * FRAMES * 4) as usize);
        let mut packed = PACKED[coat];
        lzma_rs::xz_decompress(&mut packed, &mut rgba)
            .expect("embedded kitten sheet must be valid XZ");
        assert_eq!(rgba.len(), (FRAME_W * FRAME_H * FRAMES * 4) as usize);
        rgba
    })
}

/// Select a coat once per summon; the pink kitten remains a rare surprise.
pub(super) fn select_coat(random: u64) -> usize {
    match random % 100 {
        0..=31 => 0,
        32..=63 => 1,
        64..=95 => 2,
        _ => 3,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameSize {
    pub width: u32,
    pub height: u32,
}

impl FrameSize {
    pub fn for_cell(cell: CellSize) -> Self {
        let height = u32::from(cell.h).clamp(1, MAX_DISPLAY_H);
        Self {
            width: (FRAME_W * height / FRAME_H).max(1),
            height,
        }
    }

    /// Resample each frame independently so filtering never bleeds between
    /// poses. Average premultiplied channels, then unpremultiply for kitty's
    /// straight-alpha protocol; transparent edges cannot produce dark halos.
    /// For larger terminal rows, repeat source pixels to retain the display size.
    pub fn sheet(self, coat: usize) -> Vec<u8> {
        let source = source_sheet(coat);
        let sheet_width = self.width * FRAMES;
        let mut rgba = vec![0; (sheet_width * self.height * 4) as usize];
        for frame in 0..FRAMES {
            for y in 0..self.height {
                for x in 0..self.width {
                    let mut channels = [0u32; 3];
                    let mut alpha = 0;
                    let mut samples = 0;
                    let top = y * FRAME_H / self.height;
                    let bottom = ((y + 1) * FRAME_H / self.height).max(top + 1);
                    let left = x * FRAME_W / self.width;
                    let right = ((x + 1) * FRAME_W / self.width).max(left + 1);
                    for sy in top..bottom {
                        for sx in left..right {
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
        let x = elapsed_ms.saturating_mul(u128::from(TRAVEL_PX_PER_SECOND * self.height))
            / (1000 * u128::from(FRAME_H));
        if x >= u128::from(track_px) {
            return None;
        }
        let x = x as u32;
        let frame =
            (elapsed_ms.saturating_mul(FRAMES_PER_SECOND) / 1000 % u128::from(FRAMES)) as u32;
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
        for coat in 0..PACKED.len() {
            let sheet = source_sheet(coat);
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
        let native = FrameSize::for_cell(CellSize { w: 16, h: 32 });
        assert_eq!(native.sheet(0), source_sheet(0));
    }

    #[test]
    fn enlarged_rows_repeat_pixels_without_changing_frame_boundaries() {
        let size = FrameSize::for_cell(CellSize { w: 32, h: 64 });
        let enlarged = size.sheet(2);
        let source = source_sheet(2);
        for y in 0..64 {
            for x in 0..96 * FRAMES {
                let from = (((y / 2) * FRAME_W * FRAMES + x / 2) * 4) as usize;
                let to = ((y * 96 * FRAMES + x) * 4) as usize;
                assert_eq!(&enlarged[to..to + 4], &source[from..from + 4]);
            }
        }
    }

    #[test]
    fn walk_advances_frames_and_clips_at_the_lane_edge() {
        let cell = CellSize { w: 8, h: 16 };
        let size = FrameSize::for_cell(cell);
        let track = Rect::new(3, 1, 10, 1);
        let start = size.placement(42, 0, track, cell).unwrap();
        assert_eq!((start.col, start.row), (3, 1));
        assert_eq!(start.crop, Some((0, 0, 24, 16)));
        let before_pose_change = size.placement(42, 71, track, cell).unwrap();
        assert_eq!(before_pose_change.offset, (1, 0));
        assert_eq!(before_pose_change.crop, Some((0, 0, 24, 16)));
        let moving = size.placement(42, 72, track, cell).unwrap();
        assert_eq!(moving.offset, (1, 0));
        assert_eq!(moving.crop, Some((24, 0, 24, 16)));
        let exiting = size.placement(42, 3000, track, cell).unwrap();
        assert_eq!(exiting.col, 12);
        assert_eq!(exiting.crop, Some((48, 0, 5, 16)));
        let last_pixel = size.placement(42, 3199, track, cell).unwrap();
        assert_eq!(last_pixel.crop, Some((96, 0, 1, 16)));
        assert!(size.placement(42, 3200, track, cell).is_none());
        assert!(size.placement(42, 0, Rect::default(), cell).is_none());
    }

    #[test]
    fn travel_scales_with_the_sprite_not_the_cell_width() {
        for height in [16, 32, 64, 96] {
            for width in [8, 16, 24] {
                let cell = CellSize {
                    w: width,
                    h: height,
                };
                let size = FrameSize::for_cell(cell);
                for (elapsed, distance, pose) in [(1000, 50, 6), (2000, 100, 4), (5000, 250, 6)] {
                    let p = size
                        .placement(42, elapsed, Rect::new(0, 1, 100, 1), cell)
                        .unwrap();
                    let traveled = u32::from(p.col) * u32::from(width) + u32::from(p.offset.0);
                    assert_eq!(traveled, distance * size.height / 32);
                    assert_eq!(p.crop.unwrap().0, pose * size.width);
                }
            }
        }
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
