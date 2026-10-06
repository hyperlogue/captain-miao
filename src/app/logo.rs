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
//! (the second row), with a three-second cooldown between summons.
//! Unlike the pulse, the cat *moves*, which kitty's in-place
//! frame animation can't do, so this one is **client-driven**: a full-color anime
//! walk sheet is selected on each summon, sized to the terminal's row height, and
//! uploaded once. Each render crops the next pose and advances the placement by
//! a column + sub-cell offset. The run loop ticks fast (`App::cat_walking`) until
//! all kittens leave the lane. See `assets/logo/cats/README.md` for the artwork.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::config;
use crate::terminal::graphics::{self, PAW_IMAGE_ID, Placement};

use super::App;

mod cat;

// =============================================================================
// The paw: masks, colours, pulse
// =============================================================================

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

// =============================================================================
// The walking cat
// =============================================================================

/// Base of the cat image-id **pool** (`CAT_IMAGE_ID .. CAT_IMAGE_ID + CAT_MAX`),
/// clear of the three paw ids (7101–7103). Each concurrent cat gets its own id from
/// this pool (each walk owns its uploaded sheet);
/// the placement id is shared, since the `(image, placement)` pair is already unique
/// per cat via the image id. `CAT_MAX` caps how many cats can walk at once — extra
/// clicks past that still pulse the paw, they just don't spawn another cat.
const CAT_IMAGE_ID: u32 = PAW_IMAGE_ID + 10;
const CAT_MAX: u32 = 12;
const CAT_SPAWN_COOLDOWN: Duration = Duration::from_secs(3);
/// One summoned kitten: its start time drives both position and pose, independent
/// of redraw frequency. The coat persists across resize and graphics re-uploads.
pub(crate) struct CatWalk {
    started: Instant,
    coat: usize,
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

// =============================================================================
// Rendering, and invalidation
// =============================================================================

/// kitty image id for a status colour's animated paw (base id + the colour index).
fn paw_image_id(state: PawState) -> u32 {
    PAW_IMAGE_ID + state as u32
}

impl App {
    /// Request a paw-click pulse on the next render. Only meaningful with kitty
    /// graphics (the emoji fallback doesn't animate); a no-op otherwise. The click
    /// event triggers a redraw, so `render_logo_graphics` fires it promptly.
    pub(super) fn start_logo_anim(&mut self, now: Instant) {
        if self.logo.caps.is_some() {
            self.logo.pulse_pending = true;
            // Keep the paw responsive while spacing out successful summons.
            // Ignored clicks neither queue a kitten nor extend the cooldown.
            if self
                .logo
                .last_cat_spawn
                .is_some_and(|last| now.duration_since(last) < CAT_SPAWN_COOLDOWN)
            {
                return;
            }
            if let Some(image_id) = self.alloc_cat_image_id() {
                self.logo.cats.push(CatWalk {
                    started: now,
                    coat: self.pick_cat_coat(),
                    image_id,
                    transmitted: false,
                });
                self.logo.last_cat_spawn = Some(now);
            }
        }
    }

    /// Lowest free image id in the cat pool (`None` when all `CAT_MAX` are in use).
    fn alloc_cat_image_id(&self) -> Option<u32> {
        (CAT_IMAGE_ID..CAT_IMAGE_ID + CAT_MAX)
            .find(|id| self.logo.cats.iter().all(|c| c.image_id != *id))
    }

    /// Click-time entropy is sufficient for a cosmetic coat choice.
    fn pick_cat_coat(&self) -> usize {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| (d.as_secs() << 32) ^ d.subsec_nanos() as u64);
        cat::select_coat(splitmix64(seed))
    }

    /// Whether a cat is mid-walk, so the run loop ticks fast enough to animate it
    /// (it's client-driven — see the module docs). False once it leaves the row.
    pub(super) fn cat_walking(&self) -> bool {
        !self.logo.cats.is_empty()
    }

    /// Render the paw on the header via kitty graphics, called once per frame
    /// *after* `terminal.draw()` has flushed. The three animated paws are composed
    /// once; each frame just (re)places the current status colour when it changes
    /// and, on a pending click, plays that colour's pulse once. No-op when the
    /// terminal can't do graphics (the header drew the emoji) or the rect is
    /// unknown.
    pub(super) fn render_logo_graphics(&mut self) {
        let Some(rect) = self.logo.rect else {
            self.clear_cat_walks();
            return;
        };
        if self.logo.caps.is_none() {
            self.logo.pulse_pending = false;
            // No graphics → nothing to walk, and this early return skips
            // `render_cat_walk`; without clearing here `cat_walking()` would pin the
            // run loop at its fast walk tick forever.
            self.logo.cats.clear();
            return;
        }

        // Compose the three animated paws once (a base frame plus the pulse frames,
        // per status colour, parked stopped on frame 1). Cat sheets are uploaded
        // on demand by `render_cat_walk`.
        if !self.logo.composed {
            if PAW_STATES
                .into_iter()
                .all(|s| compose_paw(paw_image_id(s), self.logo.paw_colors[s as usize]))
            {
                self.logo.composed = true;
                self.logo.placed_color = None;
            } else {
                return; // compose/upload failed — retry next frame
            }
        }

        // Show the current status colour, swapping which image is placed on a
        // change (and dropping the previous one so it doesn't linger underneath).
        let state = self.logo_state();
        if self.logo.placed_color != Some(state)
            && graphics::place(&paw_placement(paw_image_id(state), rect)).is_ok()
        {
            if let Some(prev) = self.logo.placed_color {
                let _ = graphics::delete_placements(paw_image_id(prev));
            }
            self.logo.placed_color = Some(state);
        }

        // Fire the pulse on the shown colour; kitty runs PULSE_LOOPS loops from
        // here and settles on the resting frame.
        if self.logo.pulse_pending {
            let _ = graphics::play_loops(paw_image_id(state), PULSE_LOOPS);
            self.logo.pulse_pending = false;
        }

        // Advance a walking cat across the padding row (client-driven).
        self.render_cat_walk();
    }

    /// Advance all kittens and release their image IDs when they leave the lane.
    /// Position is checked before upload so a failed write cannot keep the fast
    /// animation tick alive indefinitely.
    fn render_cat_walk(&mut self) {
        if self.logo.cats.is_empty() {
            return;
        }
        let (Some(track), Some(cell)) = (self.logo.cat_track, self.logo.caps) else {
            self.clear_cat_walks();
            return;
        };
        let size = cat::FrameSize::for_cell(cell);
        self.logo.cats.retain_mut(|cat| {
            let Some(placement) =
                size.placement(cat.image_id, cat.started.elapsed().as_millis(), track, cell)
            else {
                let _ = graphics::free_image(cat.image_id);
                return false;
            };
            if !cat.transmitted {
                if graphics::transmit_rgba(
                    cat.image_id,
                    size.sheet_width(),
                    size.height,
                    &size.sheet(cat.coat),
                )
                .is_err()
                {
                    return true;
                }
                cat.transmitted = true;
            }
            let _ = graphics::place(&placement);
            true
        });
    }

    fn clear_cat_walks(&mut self) {
        for cat in self.logo.cats.drain(..) {
            if self.logo.caps.is_some() {
                let _ = graphics::free_image(cat.image_id);
            }
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
        self.logo.composed = false;
        self.logo.placed_color = None;
        for cat in &mut self.logo.cats {
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
        if self.logo.caps.is_some() {
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
        self.logo.composed = false;
        self.logo.placed_color = None;
        self.logo.pulse_pending = false;
        self.logo.cats.clear();
    }
}

// =============================================================================
// Tinting and easing
// =============================================================================

/// A `splitmix64` step — mixes a seed into a well-distributed 64-bit value. Enough
/// randomness for picking a cat coat without pulling in the `rand` crate.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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

// =============================================================================
// Probing the terminal's palette
// =============================================================================

/// Caches of the startup-probed paw tints, so `App::new` (which runs after
/// the terminal modes are armed) can read what `probe_logo_colors` resolved earlier.
static PROBED_PAW_COLORS: OnceLock<[(u8, u8, u8); 3]> = OnceLock::new();

/// Probe the paw's status tints once at startup and
/// cache them, resolved from the terminal's own palette (OSC 4) so they match the
/// theme; any miss keeps the baked default. **Must** be called during setup — see
/// [`graphics::query_palette`] — after raw mode is on but before the event loop /
/// mouse / focus reporting start reading stdin. No-op (leaves defaults) without
/// kitty graphics.
pub(crate) fn probe_logo_colors() {
    let mut paw = DEFAULT_PAW_COLORS;
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
    }
    let _ = PROBED_PAW_COLORS.set(paw);
}

/// The probed paw tints, or `DEFAULT_PAW_COLORS` if `probe_logo_colors` hasn't run
/// (tests, non-kitty).
pub(super) fn probed_paw_colors() -> [(u8, u8, u8); 3] {
    PROBED_PAW_COLORS
        .get()
        .copied()
        .unwrap_or(DEFAULT_PAW_COLORS)
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

// =============================================================================
// The header paw's own state
// =============================================================================

/// Everything the header paw and its cats need, held as one `App` field.
pub(crate) struct LogoState {
    /// Cell pixel size when the terminal can render kitty graphics, else `None`
    /// (the header draws the emoji-paw fallback). Recomputed on resize.
    pub(in crate::app) caps: Option<crate::terminal::graphics::CellSize>,
    /// Screen cells the header paw occupies; the click hit-test (M2) and the
    /// graphics placement both read it. Set by `draw_header` each frame.
    pub(in crate::app) rect: Option<Rect>,
    /// Whether the three animated paws (one kitty image per status colour) are
    /// composed and uploaded. Done once; reset across terminal re-inits (which drop
    /// kitty images).
    pub(in crate::app) composed: bool,
    /// Which status colour's paw image is currently placed, so an unrelated redraw
    /// doesn't re-place (which would disturb a running pulse) — only a genuine
    /// colour change swaps the displayed image. `None` = nothing placed yet.
    pub(in crate::app) placed_color: Option<PawState>,
    /// A click is waiting to fire its one-shot pulse on the next render. Set by the
    /// click handler, consumed (and cleared) by `render_logo_graphics`.
    pub(in crate::app) pulse_pending: bool,
    /// The paw's RGB tints indexed by `PawState` (idle/active/attention), seeded
    /// from `DEFAULT_PAW_COLORS` and overlaid at startup with the terminal's own
    /// palette so the paw matches the Sessions status symbols. Baked into the frames.
    pub(in crate::app) paw_colors: [(u8, u8, u8); 3],
    /// Cats currently walking the padding row — paw clicks spawn one after the
    /// cooldown (up to a pool cap). Client-driven: `render_cat_walk`
    /// advances them from wall-clock elapsed, and the run loop ticks fast while any
    /// are live (see `App::cat_walking`).
    pub(in crate::app) cats: Vec<CatWalk>,
    /// Last successful summon; preserved across resize and cats leaving the lane.
    last_cat_spawn: Option<Instant>,
    /// The header's blank padding row (full width, one cell tall) the cat walks
    /// across. Set by `draw_header` each frame; `None` before the first draw.
    pub(in crate::app) cat_track: Option<Rect>,
}

impl LogoState {
    /// Probes the terminal: graphics capability and the paw's status palette. Not `Default` for that reason — it is a startup
    /// action, not a zero value.
    pub(crate) fn new() -> Self {
        Self {
            caps: crate::terminal::graphics::capability(),
            rect: None,
            composed: false,
            placed_color: None,
            pulse_pending: false,
            paw_colors: probed_paw_colors(),
            cats: Vec::new(),
            last_cat_spawn: None,
            cat_track: None,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

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
    fn coat_selection_gives_three_regular_cats_32_percent_and_pink_4_percent() {
        let mut counts = [0; 4];
        for roll in 0..10_000 {
            counts[cat::select_coat(roll)] += 1;
        }
        assert_eq!(counts, [3200, 3200, 3200, 400]);
        for (roll, coat) in [
            (0, 0),
            (31, 0),
            (32, 1),
            (63, 1),
            (64, 2),
            (95, 2),
            (96, 3),
            (99, 3),
        ] {
            assert_eq!(cat::select_coat(roll), coat);
        }
    }
}
