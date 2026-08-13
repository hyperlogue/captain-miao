//! The header paw logo, rendered via the kitty graphics protocol when the
//! terminal supports it, falling back to a `🐾` emoji glyph otherwise (the
//! fallback text is drawn by `draw_header`; this module owns the image path).
//!
//! The paw is one baked alpha **mask**, tinted to a status colour that mirrors the
//! Sessions column — gray idle, green active, yellow attention — with the exact
//! green/yellow read from the terminal's own palette (OSC 4) so it matches the
//! status symbols under any theme.
//!
//! It's uploaded as **three kitty animation images** (one per status colour),
//! each a short **pulse**: fade to transparent, then a brightness bump, back to
//! rest (`o1b1 → o0b1 → o1b1.1 → o1b1`); both ends are the resting paw so it
//! settles cleanly. Clicking plays two loops of it (`play_loops`) on the shown
//! colour and kitty advances the frames **autonomously** — the dashboard sends
//! nothing per frame and its event loop stays idle during the pulse.
//!
//! A click also sends a **cat** trotting across the header's blank padding row
//! (the second row). Unlike the pulse, the cat *moves*, which kitty's in-place
//! frame animation can't do, so this one is **client-driven**: the sprite sheet is
//! tinted to a random colour (four common dashboard colours, plus a rare special
//! one) and uploaded at walk start, then each render re-places it at native size
//! with an advancing column + sub-cell offset, cropping the current walk-cycle
//! frame — the run loop ticks fast (`App::cat_walking`) until the cat leaves the
//! row.

use std::sync::OnceLock;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::config;
use crate::terminal::graphics::{self, PAW_IMAGE_ID, Placement};

use super::App;

/// Cells the resting logo occupies (width, height). One source of truth for the
/// header layout, the click hit-test, and the graphics placement.
pub(super) const LOGO_CELLS: (u16, u16) = (2, 1);

/// Fixed placement id (kitty keys placements by image + this).
const PAW_PLACEMENT_ID: u32 = 1;

/// Click-pulse: one loop is `PULSE_FRAMES` frames held `PULSE_GAP_MS` apart, and a
/// click plays `PULSE_LOOPS` of them back-to-back. A loop runs in three equal
/// thirds — opacity first dips to `PULSE_MIN_ALPHA` and recovers (fade out/in),
/// then brightness boosts up to `PULSE_PEAK` (10% brighter than the resting paw)
/// and back: `o1b1 → o0b1 → o1b1.1 → o1b1`. Frame 0 (the root, which kitty skips
/// during playback because it has no gap) and the last frame are both the resting
/// paw, so each loop starts and ends solid and the loops chain seamlessly. Timing:
/// the root is skipped, leaving `PULSE_FRAMES - 1` = 20 played frames × 25ms =
/// 500ms per loop, ×`PULSE_LOOPS` = ~1s for the two loops.
const PULSE_FRAMES: u32 = 21;
const PULSE_GAP_MS: u32 = 25;
const PULSE_PEAK: f32 = 0.10;
const PULSE_MIN_ALPHA: f32 = 0.0;
/// Loops played per click — each a full brightness-then-opacity pulse.
const PULSE_LOOPS: u32 = 2;

/// Which status tint the resting paw shows, mirroring the Sessions status column
/// (`format::status_color`): gray at rest, green when a session is busy, yellow
/// when one wants attention. Attention wins over busy. The discriminants index
/// `App::paw_colors` / `DEFAULT_PAW_COLORS` and offset the kitty image id.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PawState {
    Idle = 0,
    Active = 1,
    Attention = 2,
}

const PAW_STATES: [PawState; 3] = [PawState::Idle, PawState::Active, PawState::Attention];

/// The paw is baked once as an anti-aliased alpha **mask** (one coverage byte per
/// pixel, row-major); the runtime tints it to any RGB and sends it as raw RGBA.
/// Kept in sync with `examples/gen_logo_assets.rs`.
const PAW_MASK: &[u8] = include_bytes!("../../assets/logo/paw-mask.gray");
const PAW_MASK_DIM: u32 = 64;

/// The cat walk sprite **sheet**: `CAT_FRAMES` walk poses laid out horizontally,
/// each `CAT_FRAME_W`×`CAT_FRAME_H` px. One alpha mask (like the paw), tinted to a
/// per-walk random colour; the walk plays by cropping one frame per placement while
/// the placement slides across the row. Kept in sync with `gen_logo_assets.rs`.
const CAT_MASK: &[u8] = include_bytes!("../../assets/logo/cat-mask.gray");
const CAT_FRAME_W: u32 = 80;
const CAT_FRAME_H: u32 = 40;
const CAT_FRAMES: u32 = 4;
/// Walking speed in **cells per second** — a leisurely saunter. Speed (not a fixed
/// total duration) is the knob so the *visual* pace is constant on any width; a
/// fixed duration makes the cat sprint across a wide terminal. Lower = slower.
const CAT_SPEED_CELLS_PER_S: f32 = 9.0;
/// How long each walk-cycle frame (leg pose) is shown — the leg cadence. Kept
/// inversely proportional to the speed (a fixed distance per step) so the legs stay
/// in time with the travel rather than scrabbling or gliding.
const CAT_FRAME_MS: u128 = 167;
/// Base of the cat image-id **pool** (`CAT_IMAGE_ID .. CAT_IMAGE_ID + CAT_MAX`),
/// clear of the three paw ids (7101–7103). Each concurrent cat gets its own id from
/// this pool (they carry different random tints, so they can't share one image);
/// the placement id is shared, since the `(image, placement)` pair is already unique
/// per cat via the image id. `CAT_MAX` caps how many cats can walk at once — extra
/// clicks past that still pulse the paw, they just don't spawn another cat.
const CAT_IMAGE_ID: u32 = PAW_IMAGE_ID + 10;
const CAT_MAX: u32 = 12;
const CAT_PLACEMENT_ID: u32 = 2;

/// The four ANSI palette colours (error/active/attention/selection — the ones the
/// dashboard leans on) the cat is tinted from at random each walk, resolved to the
/// terminal's real RGB at startup so they match the theme (see `probe_logo_colors`).
const CAT_COMMON_ANSI: [Color; 4] = [Color::Red, Color::Green, Color::Yellow, Color::Blue];
/// Fallbacks for the four common tints when the palette can't be queried
/// (Catppuccin red/green/yellow/blue).
const CAT_COMMON_FALLBACK: [(u8, u8, u8); 4] = [
    (0xf3, 0x8b, 0xa8), // red
    (0xa6, 0xe3, 0xa1), // green
    (0xf9, 0xe2, 0xaf), // yellow
    (0x89, 0xb4, 0xfa), // blue
];
/// The rare "special" tint (Catppuccin pink), a fixed colour outside the common
/// four, chosen ~1 in `CAT_RARE_ONE_IN` walks — a little easter egg to spot.
const CAT_RARE_COLOR: (u8, u8, u8) = (0xf5, 0xc2, 0xe7);
const CAT_RARE_ONE_IN: u64 = 20;

/// One cat walk in progress (several can run at once — a click always spawns a new
/// one). Holds its start instant (the render derives position + walk-cycle frame
/// from monotonic elapsed, so playback is frame-rate independent), the
/// randomly-picked tint, the kitty image id it owns from the pool, and whether that
/// tinted sheet has been uploaded yet (done on the walk's first render, since the
/// colour isn't known until the click).
pub(crate) struct CatWalk {
    started: Instant,
    color: (u8, u8, u8),
    image_id: u32,
    transmitted: bool,
}

/// Fallback tints when the terminal palette can't be queried: a muted gray, plus
/// Catppuccin green / yellow (a close match for most dark themes). Indexed by
/// `PawState`. `App::paw_colors` overrides active/attention with the terminal's
/// actual `color2`/`color3` at startup.
pub(super) const DEFAULT_PAW_COLORS: [(u8, u8, u8); 3] = [
    (0x7f, 0x84, 0x9c), // idle — overlay1 gray
    (0xa6, 0xe3, 0xa1), // active — green
    (0xf9, 0xe2, 0xaf), // attention — yellow
];

/// kitty image id for a status colour's animated paw (base id + the colour index).
fn paw_image_id(state: PawState) -> u32 {
    PAW_IMAGE_ID + state as u32
}

impl App {
    /// Request a paw-click pulse on the next render. Only meaningful with kitty
    /// graphics (the emoji fallback doesn't animate); a no-op otherwise. The click
    /// event triggers a redraw, so `render_logo_graphics` fires it promptly.
    pub(super) fn start_logo_anim(&mut self) {
        if self.logo_caps.is_some() {
            self.logo_pulse_pending = true;
            // Same click also sends a cat trotting across the padding row, tinted to
            // a fresh random colour (its sheet is uploaded on the first render). A
            // click while earlier cats are still walking spawns *another* one — until
            // the pool is full, in which case the click just pulses the paw.
            if let Some(image_id) = self.alloc_cat_image_id() {
                self.cats.push(CatWalk {
                    started: Instant::now(),
                    color: self.pick_cat_color(),
                    image_id,
                    transmitted: false,
                });
            }
        }
    }

    /// Lowest free image id in the cat pool (`None` when all `CAT_MAX` are in use).
    fn alloc_cat_image_id(&self) -> Option<u32> {
        (CAT_IMAGE_ID..CAT_IMAGE_ID + CAT_MAX)
            .find(|id| self.cats.iter().all(|c| c.image_id != *id))
    }

    /// Pick this walk's cat tint: usually one of the four common dashboard colours
    /// (equal odds), and ~1 in `CAT_RARE_ONE_IN` the rare special pink. Entropy is
    /// the click's wall-clock nanos, whitened through `splitmix64`.
    fn pick_cat_color(&self) -> (u8, u8, u8) {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| (d.as_secs() << 32) ^ d.subsec_nanos() as u64);
        select_cat_color(splitmix64(seed), self.cat_colors)
    }

    /// Whether a cat is mid-walk, so the run loop ticks fast enough to animate it
    /// (it's client-driven — see the module docs). False once it leaves the row.
    pub(super) fn cat_walking(&self) -> bool {
        !self.cats.is_empty()
    }

    /// Render the paw on the header via kitty graphics, called once per frame
    /// *after* `terminal.draw()` has flushed. The three animated paws are composed
    /// once; each frame just (re)places the current status colour when it changes
    /// and, on a pending click, plays that colour's pulse once. No-op when the
    /// terminal can't do graphics (the header drew the emoji) or the rect is
    /// unknown.
    pub(super) fn render_logo_graphics(&mut self) {
        let Some(rect) = self.logo_rect else {
            return;
        };
        if self.logo_caps.is_none() {
            self.logo_pulse_pending = false;
            // No graphics → nothing to walk, and this early return skips
            // `render_cat_walk`; without clearing here `cat_walking()` would pin the
            // run loop at its fast walk tick forever.
            self.cats.clear();
            return;
        }

        // Compose the three animated paws once (a base frame plus the pulse frames,
        // per status colour, parked stopped on frame 1). The cat sheet isn't composed
        // here — its colour is random per walk, so it's uploaded in `render_cat_walk`.
        if !self.logo_composed {
            if PAW_STATES
                .into_iter()
                .all(|s| compose_paw(paw_image_id(s), self.paw_colors[s as usize]))
            {
                self.logo_composed = true;
                self.logo_placed_color = None;
            } else {
                return; // compose/upload failed — retry next frame
            }
        }

        // Show the current status colour, swapping which image is placed on a
        // change (and dropping the previous one so it doesn't linger underneath).
        let state = self.logo_state();
        if self.logo_placed_color != Some(state)
            && graphics::place(&paw_placement(paw_image_id(state), rect)).is_ok()
        {
            if let Some(prev) = self.logo_placed_color {
                let _ = graphics::delete_placements(paw_image_id(prev));
            }
            self.logo_placed_color = Some(state);
        }

        // Fire the pulse on the shown colour; kitty runs PULSE_LOOPS loops from
        // here and settles on the resting frame.
        if self.logo_pulse_pending {
            let _ = graphics::play_loops(paw_image_id(state), PULSE_LOOPS);
            self.logo_pulse_pending = false;
        }

        // Advance a walking cat across the padding row (client-driven).
        self.render_cat_walk();
    }

    /// Advance every walking cat for this frame, retiring any that have left the
    /// row. Client-driven: each cat derives its pixel position and walk-cycle frame
    /// from monotonic elapsed, re-placing its own image (a unique pool id) at an
    /// advancing column + sub-cell X offset with the current frame's source crop. A
    /// cat uploads its tinted sheet on its first render (the colour isn't known
    /// until the click). Called from `render_logo_graphics` (so `logo_caps`/
    /// `logo_composed` already hold). A no-op when no cats are walking.
    fn render_cat_walk(&mut self) {
        if self.cats.is_empty() {
            return;
        }
        // The padding row spans the full header width; without it (pre-first-draw)
        // there's nowhere to walk, so drop the walk rather than guess.
        let Some(track) = self.cat_track else {
            self.cats.clear();
            return;
        };
        let cell_w = self.logo_caps.map_or(1, |c| c.w).max(1) as u32;
        let track_px = track.width as u32 * cell_w;

        // Image ids of cats that finished this frame (to free after the loop).
        let mut finished: Vec<u32> = Vec::new();
        for cat in &mut self.cats {
            // Position first, from monotonic elapsed (cells/s → px/ms), split into a
            // whole cell column and a sub-cell offset for smooth motion between
            // cells. Deciding this *before* the upload means a cat always retires
            // after its walk duration even if its transmit never succeeds — so a
            // permanently-failing upload can't pin the fast render tick.
            let elapsed = cat.started.elapsed().as_millis();
            let x_px = (elapsed as f32 * CAT_SPEED_CELLS_PER_S * cell_w as f32 / 1000.0) as u32;
            if x_px >= track_px {
                finished.push(cat.image_id);
                continue;
            }
            // Upload this cat's tinted sheet on its first render; skip (retry next
            // frame) on a write error rather than placing a stale/empty image.
            if !cat.transmitted {
                let rgba = tint_sheet(CAT_MASK, cat.color);
                if graphics::transmit_rgba(
                    cat.image_id,
                    CAT_FRAME_W * CAT_FRAMES,
                    CAT_FRAME_H,
                    &rgba,
                )
                .is_err()
                {
                    continue;
                }
                cat.transmitted = true;
            }
            let col = track.x + (x_px / cell_w) as u16;
            let offset_x = (x_px % cell_w) as u16;
            // Which walk-cycle pose to show (cycles through the sheet's frames).
            let frame = (elapsed / CAT_FRAME_MS % CAT_FRAMES as u128) as u32;
            let _ = graphics::place(&Placement {
                image: cat.image_id,
                placement: CAT_PLACEMENT_ID,
                col,
                row: track.y,
                // Native pixel size (no c/r): scaling to a cell box makes kitty snap
                // the placement to the grid, so the sub-cell `offset` below is
                // ignored and the cat jumps cell-to-cell. Native size keeps the walk
                // smooth (the sheet is sized so native ≈ the header row height).
                cells: None,
                z: 1,
                crop: Some((frame * CAT_FRAME_W, 0, CAT_FRAME_W, CAT_FRAME_H)),
                offset: (offset_x, 0),
            });
        }
        // Retire finished cats: drop their placement + image (freeing the pool id)
        // and remove them from the list.
        if !finished.is_empty() {
            for id in &finished {
                let _ = graphics::free_image(*id);
            }
            self.cats.retain(|c| !finished.contains(&c.image_id));
        }
    }

    /// Drop everything we believe kitty is still holding for the logo, so the
    /// next `render_logo_graphics` re-uploads the three paws and re-places the
    /// shown one. Armed by a resize (`arm_logo_recompose`).
    ///
    /// A resize is not just a lost *placement*: ratatui clears the whole screen
    /// on one (`Terminal::resize` → `clear_viewport`), and kitty's `ESC[2J`
    /// handler deletes every placement on the screen and then frees the image
    /// data of anything left without one (`grman_clear` → `filter_refs` with
    /// `free_images`). That takes all three paws — the two colours that were
    /// never placed as surely as the one that was — plus any cat sheet. Placing
    /// a freed id answers `ENOENT`, which our `q=2` suppresses, so a re-place
    /// alone leaves the paw silently blank for the rest of the run.
    ///
    /// A cat mid-walk is only marked for re-upload, not retired: it re-transmits
    /// its sheet on the next frame and finishes its walk visibly.
    pub(super) fn invalidate_logo_graphics(&mut self) {
        self.logo_composed = false;
        self.logo_placed_color = None;
        for cat in &mut self.cats {
            cat.transmitted = false;
        }
    }

    /// Aggregate status tint for the paw: yellow if any session wants attention,
    /// else green if any is busy, else gray. Matches the Sessions column's
    /// attention/active/idle split (`is_attention_row` / `is_busy`).
    fn logo_state(&self) -> PawState {
        let mut active = false;
        for s in &self.sessions {
            if self.is_attention_row(s) {
                return PawState::Attention;
            }
            active |= s.status.is_busy();
        }
        if active {
            PawState::Active
        } else {
            PawState::Idle
        }
    }

    /// Remove the paw + cat placements and free the image data — teardown on quit.
    pub(super) fn clear_logo_graphics(&mut self) {
        // Free unconditionally when the terminal can do graphics, rather than
        // gating on `logo_composed`: a compose that failed partway (some ids
        // uploaded, the flag still false) would otherwise strand those images in
        // kitty until the window closes. `a=d` on an unknown id is silent (q=2).
        if self.logo_caps.is_some() {
            for s in PAW_STATES {
                let _ = graphics::free_image(paw_image_id(s));
            }
            // Free every id in the cat pool (each carries `d=I`, which drops the
            // image *and* its placement); ids with no image are silently ignored, so
            // this covers whatever cats are still walking without tracking them.
            for id in CAT_IMAGE_ID..CAT_IMAGE_ID + CAT_MAX {
                let _ = graphics::free_image(id);
            }
        }
        self.logo_composed = false;
        self.logo_placed_color = None;
        self.logo_pulse_pending = false;
        self.cats.clear();
    }
}

/// Choose a cat tint from a whitened random `rand`: usually one of the four common
/// dashboard colours (equal odds), and 1 in `CAT_RARE_ONE_IN` the rare special
/// pink. Pure (given `rand`) so the split is unit-testable; the low and high bits
/// of a `splitmix64` output are independent enough to reuse the one draw for both
/// the rare roll (`% CAT_RARE_ONE_IN`) and the common index (`>> 8 % 4`).
fn select_cat_color(rand: u64, common: [(u8, u8, u8); 4]) -> (u8, u8, u8) {
    if rand.is_multiple_of(CAT_RARE_ONE_IN) {
        CAT_RARE_COLOR
    } else {
        common[((rand >> 8) % 4) as usize]
    }
}

/// A `splitmix64` step — mixes a seed into a well-distributed 64-bit value. Enough
/// randomness for picking a cat colour without pulling in the `rand` crate.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Straight-alpha RGBA from a coverage mask: every pixel gets `color`, the mask's
/// coverage byte as its alpha. (The paw's `tint` also brightens/fades per pulse
/// frame; the cat is a flat silhouette, so this plain version is enough.)
fn tint_sheet(mask: &[u8], (r, g, b): (u8, u8, u8)) -> Vec<u8> {
    let mut out = Vec::with_capacity(mask.len() * 4);
    for &cov in mask {
        out.extend_from_slice(&[r, g, b, cov]);
    }
    out
}

/// A whole-image placement of `id` at the header logo cell (no crop). The paw is
/// static, so it scales into its `LOGO_CELLS` box — no sub-cell motion to quantize.
fn paw_placement(id: u32, rect: Rect) -> Placement {
    Placement {
        image: id,
        placement: PAW_PLACEMENT_ID,
        col: rect.x,
        row: rect.y,
        cells: Some(LOGO_CELLS),
        z: 1,
        crop: None,
        offset: (0, 0),
    }
}

/// Compose one colour's animated paw into kitty image `id`: frame 1 is the base
/// (full-opacity) paw, then the pulse frames, then park it stopped on frame 1.
/// Returns whether every upload succeeded.
fn compose_paw(id: u32, color: (u8, u8, u8)) -> bool {
    let frame_rgba = |frame| {
        let (boost, alpha) = pulse_frame_mods(frame);
        tint(color, boost, alpha)
    };
    // Frame 1 (base) = the resting paw.
    if graphics::transmit_rgba(id, PAW_MASK_DIM, PAW_MASK_DIM, &frame_rgba(0)).is_err() {
        return false;
    }
    for frame in 1..PULSE_FRAMES {
        if graphics::append_frame(
            id,
            PAW_MASK_DIM,
            PAW_MASK_DIM,
            PULSE_GAP_MS,
            &frame_rgba(frame),
        )
        .is_err()
        {
            return false;
        }
    }
    // Sit on the resting frame until a click plays it.
    let _ = graphics::stop_animation(id);
    true
}

/// The pulse's `(brightness boost, opacity)` for `frame`, in three equal thirds:
/// opacity dips `1 → PULSE_MIN_ALPHA` (fade out), then recovers to full while
/// brightness rises `0 → PULSE_PEAK` (fade back in + peak), then brightness dims
/// `PULSE_PEAK → 0` at full opacity — i.e. `o1b1 → o0b1 → o1b1.1 → o1b1`. Each
/// third eases with a raised cosine so the transitions are smooth and the segment
/// joins are continuous. Frame 0 (the skipped root) and the last frame are the
/// resting paw (no boost, full opacity).
fn pulse_frame_mods(frame: u32) -> (f32, f32) {
    // Smooth 0→1 ease (raised cosine) for one monotonic segment.
    fn ease(u: f32) -> f32 {
        0.5 - 0.5 * (std::f32::consts::PI * u).cos()
    }
    let t = frame as f32 / (PULSE_FRAMES - 1) as f32; // 0..=1
    let span = 1.0 - PULSE_MIN_ALPHA;
    if t < 1.0 / 3.0 {
        // Fade out: opacity 1 → MIN_ALPHA, brightness resting.
        (0.0, 1.0 - span * ease(t * 3.0))
    } else if t < 2.0 / 3.0 {
        // Fade back in while brightening: opacity → 1, boost 0 → PEAK.
        let u = ease(t * 3.0 - 1.0);
        (PULSE_PEAK * u, PULSE_MIN_ALPHA + span * u)
    } else {
        // Dim back to rest: boost PEAK → 0 at full opacity.
        (PULSE_PEAK * (1.0 - ease(t * 3.0 - 2.0)), 1.0)
    }
}

/// Build a straight-alpha RGBA buffer from the coverage mask: `color` brightened
/// by `boost` (fraction toward clipping), the mask's coverage scaled by `alpha`.
fn tint(color: (u8, u8, u8), boost: f32, alpha: f32) -> Vec<u8> {
    let (r, g, b) = brighten(color, boost);
    let mut out = Vec::with_capacity(PAW_MASK.len() * 4);
    for &cov in PAW_MASK {
        let a = (cov as f32 * alpha).round() as u8;
        out.extend_from_slice(&[r, g, b, a]);
    }
    out
}

/// Brighten `color` by `amount` (0 = unchanged; scales each channel by `1+amount`,
/// clamped to 255) so the peak reads as the paw glowing a little brighter.
fn brighten((r, g, b): (u8, u8, u8), amount: f32) -> (u8, u8, u8) {
    let scale = |c: u8| (c as f32 * (1.0 + amount)).round().min(255.0) as u8;
    (scale(r), scale(g), scale(b))
}

/// Caches of the startup-probed paw + cat tints, so `App::new` (which runs after
/// the terminal modes are armed) can read what `probe_logo_colors` resolved earlier.
static PROBED_PAW_COLORS: OnceLock<[(u8, u8, u8); 3]> = OnceLock::new();
static PROBED_CAT_COLORS: OnceLock<[(u8, u8, u8); 4]> = OnceLock::new();

/// Probe the paw's status tints and the cat's four common tints once at startup and
/// cache them, resolved from the terminal's own palette (OSC 4) so they match the
/// theme; any miss keeps the baked default. **Must** be called during setup — see
/// [`graphics::query_palette`] — after raw mode is on but before the event loop /
/// mouse / focus reporting start reading stdin. No-op (leaves defaults) without
/// kitty graphics.
pub(crate) fn probe_logo_colors() {
    let mut paw = DEFAULT_PAW_COLORS;
    let mut cat = CAT_COMMON_FALLBACK;
    if graphics::capability().is_some() {
        // Paw: active = the "Active" symbol colour (green); attention = the
        // configured attention foreground (yellow by default).
        if let Some(rgb) = resolve_terminal_color(Color::Green) {
            paw[PawState::Active as usize] = rgb;
        }
        let attention = config::get().colors.ui.attention_fg;
        if let Some(rgb) = resolve_terminal_color(attention) {
            paw[PawState::Attention as usize] = rgb;
        }
        // Cat: the four common ANSI dashboard colours, to the terminal's real RGB.
        for (slot, &ansi) in cat.iter_mut().zip(CAT_COMMON_ANSI.iter()) {
            if let Some(rgb) = resolve_terminal_color(ansi) {
                *slot = rgb;
            }
        }
    }
    let _ = PROBED_PAW_COLORS.set(paw);
    let _ = PROBED_CAT_COLORS.set(cat);
}

/// The probed paw tints, or `DEFAULT_PAW_COLORS` if `probe_logo_colors` hasn't run
/// (tests, non-kitty).
pub(super) fn probed_paw_colors() -> [(u8, u8, u8); 3] {
    PROBED_PAW_COLORS
        .get()
        .copied()
        .unwrap_or(DEFAULT_PAW_COLORS)
}

/// The probed cat common tints, or `CAT_COMMON_FALLBACK` if `probe_logo_colors`
/// hasn't run (tests, non-kitty).
pub(super) fn probed_cat_colors() -> [(u8, u8, u8); 4] {
    PROBED_CAT_COLORS
        .get()
        .copied()
        .unwrap_or(CAT_COMMON_FALLBACK)
}

/// Resolve a ratatui `Color` to concrete RGB: an explicit `Rgb` as-is; a named
/// ANSI / indexed colour via the terminal palette (OSC 4); otherwise `None` (the
/// caller keeps its baked default).
fn resolve_terminal_color(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        other => graphics::query_palette(ansi_palette_index(other)?),
    }
}

/// Palette index (0..=255) for a named ANSI / indexed ratatui colour; `None` for
/// `Reset`/`Rgb`, which have no palette slot.
fn ansi_palette_index(color: Color) -> Option<u8> {
    Some(match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Indexed(n) => n,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_matches_declared_dimensions() {
        // The raw `f=32` transmit trusts PAW_MASK_DIM; a regenerated asset that
        // changed size (without updating the const) would corrupt the image.
        assert_eq!(PAW_MASK.len(), (PAW_MASK_DIM * PAW_MASK_DIM) as usize);
    }

    #[test]
    fn pulse_fades_then_brightens() {
        // Both endpoints are the resting paw (no boost, full opacity).
        assert_eq!(pulse_frame_mods(0), (0.0, 1.0));
        let (bl, al) = pulse_frame_mods(PULSE_FRAMES - 1);
        assert!(bl.abs() < 1e-4 && (al - 1.0).abs() < 1e-4);
        // First third: opacity dips *first*, brightness still normal.
        let (boost, alpha) = pulse_frame_mods(PULSE_FRAMES / 4);
        assert!(boost.abs() < 1e-4 && alpha < 1.0);
        // Later: brightness peaks *after*, with opacity back to full.
        let (boost, alpha) = pulse_frame_mods(3 * PULSE_FRAMES / 4);
        assert!(boost > 0.0 && (alpha - 1.0).abs() < 1e-4);
    }

    #[test]
    fn tint_brightens_and_scales_alpha() {
        // No boost, full opacity: colour and coverage unchanged.
        let base = tint((0x10, 0x20, 0x30), 0.0, 1.0);
        assert_eq!(base.len(), PAW_MASK.len() * 4);
        assert_eq!(&base[0..4], &[0x10, 0x20, 0x30, PAW_MASK[0]]);
        // A boost lightens every channel.
        let bright = tint((0x10, 0x20, 0x30), 0.5, 1.0);
        assert!(bright[0] > 0x10 && bright[1] > 0x20 && bright[2] > 0x30);
        // Half opacity halves coverage; RGB unaffected by alpha.
        let dim = tint((0x10, 0x20, 0x30), 0.0, 0.5);
        assert_eq!(dim[3], (PAW_MASK[0] as f32 * 0.5).round() as u8);
    }

    #[test]
    fn image_ids_are_distinct_per_colour() {
        let paw_ids: Vec<u32> = PAW_STATES.into_iter().map(paw_image_id).collect();
        assert_eq!(
            paw_ids,
            vec![PAW_IMAGE_ID, PAW_IMAGE_ID + 1, PAW_IMAGE_ID + 2]
        );
        // No id in the whole cat pool may collide with any paw id (guards against a
        // future grown PAW_STATES creeping into the pool base at CAT_IMAGE_ID).
        for cat_id in CAT_IMAGE_ID..CAT_IMAGE_ID + CAT_MAX {
            assert!(
                !paw_ids.contains(&cat_id),
                "cat id {cat_id} collides with a paw"
            );
        }
    }

    #[test]
    fn cat_mask_matches_declared_dimensions() {
        // The raw `f=32` transmit trusts these dims; a regenerated sheet that
        // changed size (without updating the consts) would corrupt the image and
        // desync the per-frame crops.
        assert_eq!(
            CAT_MASK.len(),
            (CAT_FRAME_W * CAT_FRAMES * CAT_FRAME_H) as usize
        );
    }

    #[test]
    fn cat_color_selection_common_and_rare() {
        let common = [(1, 1, 1), (2, 2, 2), (3, 3, 3), (4, 4, 4)];
        // A multiple of CAT_RARE_ONE_IN rolls the rare special colour.
        assert_eq!(select_cat_color(0, common), CAT_RARE_COLOR);
        assert_eq!(
            select_cat_color(CAT_RARE_ONE_IN * 7, common),
            CAT_RARE_COLOR
        );
        // Otherwise the high bits index the four common colours. Each index is
        // reachable: `(rand >> 8) % 4`, with rand not a multiple of the rare floor.
        for idx in 0u64..4 {
            let rand = (idx << 8) | 1; // low byte 1 → not rare; high bits pick idx
            assert!(!rand.is_multiple_of(CAT_RARE_ONE_IN));
            assert_eq!(select_cat_color(rand, common), common[idx as usize]);
        }
    }

    #[test]
    fn splitmix64_spreads_sequential_seeds() {
        // Sequential seeds must not map to sequential/correlated outputs — a weak
        // mixer would bias the colour pick toward one bucket across nearby click
        // times. Check the low-bit rare roll and the 4-bucket index both vary.
        let mut buckets = [0u32; 4];
        let mut rare = 0;
        for seed in 0..4000u64 {
            let r = splitmix64(seed);
            if r.is_multiple_of(CAT_RARE_ONE_IN) {
                rare += 1;
            } else {
                buckets[((r >> 8) % 4) as usize] += 1;
            }
        }
        // Every common bucket got a healthy share, and the rare bucket is roughly
        // 4000/CAT_RARE_ONE_IN ≈ 200 (loose bounds — this pins "well-spread", not
        // exact).
        assert!(buckets.iter().all(|&b| b > 700), "buckets: {buckets:?}");
        assert!((140..=280).contains(&rare), "rare: {rare}");
    }

    #[test]
    fn tint_sheet_is_flat_rgba() {
        // Every pixel carries the tint colour; alpha is the coverage byte verbatim.
        let out = tint_sheet(&[0x00, 0x80, 0xff], (0x11, 0x22, 0x33));
        assert_eq!(
            out,
            vec![
                0x11, 0x22, 0x33, 0x00, //
                0x11, 0x22, 0x33, 0x80, //
                0x11, 0x22, 0x33, 0xff,
            ]
        );
    }
}
