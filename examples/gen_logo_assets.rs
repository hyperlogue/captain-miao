//! Generates the header-logo PNG assets committed under `assets/logo/`.
//!
//! Run once (and re-run whenever the artwork changes):
//!
//! ```sh
//! cargo run --example gen_logo_assets
//! ```
//!
//! The dashboard embeds the finished paw alpha **mask** (`paw-mask.gray`) with
//! `include_bytes!` and streams them to kitty via the graphics protocol, tinting
//! at runtime — so `tiny-skia` is a **dev-only** dependency that never ships in the
//! binary. Everything here is drawn from ellipses, so
//! there is no SVG/rasteriser toolchain dependency either. The `.png` outputs are
//! eyeball previews only; the runtime never reads them.
//!
//! The paw is baked as a single anti-aliased alpha mask; the runtime tints it to a
//! status colour and pulses it on click. The full-color kitten artwork has its
//! own preparation pipeline; see `assets/logo/cats/README.md`.

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

/// The paw is baked once as an anti-aliased **alpha mask** at this size; the
/// runtime tints it to the terminal's own green/yellow (and a muted gray) and
/// sends it as raw RGBA. 64px covers the ~2-cell header size (kitty downscales)
/// while keeping each upload small — it's re-sent every frame during the click
/// pulse. Keep in sync with `PAW_MASK_DIM` in `logo.rs`.
const PAW_MASK_DIM: u32 = 64;

fn main() {
    let out_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/logo");
    std::fs::create_dir_all(out_dir).expect("create assets/logo");

    // Single paw asset: the alpha coverage mask (one byte per pixel), row-major.
    let mask = render_paw_mask(PAW_MASK_DIM);
    let path = format!("{out_dir}/paw-mask.gray");
    std::fs::write(&path, &mask).expect("write paw mask");
    println!("wrote {path} ({} bytes)", mask.len());
    // Also drop a viewable white-on-transparent PNG for eyeballing the shape.
    let png = render_paw(PAW_MASK_DIM, [0xff, 0xff, 0xff, 0xff]);
    let ppath = format!("{out_dir}/paw-preview.png");
    std::fs::write(&ppath, png).expect("write paw preview");
    println!("wrote {ppath}");
}

/// The paw's alpha coverage as `size*size` bytes (row-major), extracted from a
/// solid-white render — this is the mask the runtime tints.
fn render_paw_mask(size: u32) -> Vec<u8> {
    let pm = render_paw_pixmap(size, [0xff, 0xff, 0xff, 0xff]);
    // Pixmap data is premultiplied RGBA; the alpha byte (every 4th) is coverage.
    pm.data().iter().skip(3).step_by(4).copied().collect()
}

/// Draw the paw in `rgba` and return it as an encoded PNG (viewable preview).
fn render_paw(size: u32, rgba: [u8; 4]) -> Vec<u8> {
    render_paw_pixmap(size, rgba)
        .encode_png()
        .expect("encode png")
}

/// Draw a paw (four toe beans in a gentle arc over one palm pad) in `rgba` into a
/// transparent `size`x`size` pixmap.
fn render_paw_pixmap(size: u32, rgba: [u8; 4]) -> Pixmap {
    let mut pm = Pixmap::new(size, size).expect("pixmap");
    let s = size as f32;

    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);

    // Four toe beans (a cat's print — the raised dewclaw leaves no mark):
    // (cx, cy, rx, ry) in normalised [0,1] coords. Evenly spaced across the width
    // with a clear gap between each, and the inner pair riding higher than the
    // outer pair for the classic paw arc.
    let toes = [
        (0.150, 0.47, 0.100, 0.120),
        (0.383, 0.30, 0.105, 0.125),
        (0.617, 0.30, 0.105, 0.125),
        (0.850, 0.47, 0.100, 0.120),
    ];
    for (cx, cy, rx, ry) in toes {
        fill_ellipse(&mut pm, &paint, cx * s, cy * s, rx * s, ry * s);
    }
    // Palm pad: a broad ellipse below the toes.
    fill_ellipse(&mut pm, &paint, 0.50 * s, 0.71 * s, 0.285 * s, 0.245 * s);

    pm
}

/// Fill an axis-aligned ellipse by scaling a unit circle into place.
fn fill_ellipse(pm: &mut Pixmap, paint: &Paint, cx: f32, cy: f32, rx: f32, ry: f32) {
    let mut pb = PathBuilder::new();
    pb.push_circle(0.0, 0.0, 1.0);
    let unit = pb.finish().expect("unit circle");
    let t = Transform::from_row(rx, 0.0, 0.0, ry, cx, cy);
    pm.fill_path(&unit, paint, FillRule::Winding, t, None);
}
