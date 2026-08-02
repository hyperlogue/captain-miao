//! Generates the header-logo PNG assets committed under `assets/logo/`.
//!
//! Run once (and re-run whenever the artwork changes):
//!
//! ```sh
//! cargo run --example gen_logo_assets
//! ```
//!
//! The dashboard embeds the finished alpha **masks** (`*.gray`) with
//! `include_bytes!` and streams them to kitty via the graphics protocol, tinting
//! at runtime — so `tiny-skia` is a **dev-only** dependency that never ships in the
//! binary. Everything here is drawn from primitives (ellipses + triangles), so
//! there is no SVG/rasteriser toolchain dependency either. The `.png` outputs are
//! eyeball previews only; the runtime never reads them.
//!
//! The paw is baked as a single anti-aliased alpha mask; the runtime tints it to a
//! status colour and pulses it on click. The cat walk is a horizontal sprite sheet
//! of walk-cycle frames, baked the same way (`render_cat_sheet`).

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

/// The paw is baked once as an anti-aliased **alpha mask** at this size; the
/// runtime tints it to the terminal's own green/yellow (and a muted gray) and
/// sends it as raw RGBA. 64px covers the ~2-cell header size (kitty downscales)
/// while keeping each upload small — it's re-sent every frame during the click
/// pulse. Keep in sync with `PAW_MASK_DIM` in `logo.rs`.
const PAW_MASK_DIM: u32 = 64;

/// The cat walk is baked as a horizontal sprite **sheet** of `CAT_FRAMES` frames,
/// each `CAT_FRAME_W`×`CAT_FRAME_H` px, laid out left-to-right. Like the paw it's a
/// single alpha **mask**; the runtime tints it and crops one frame per placement to
/// play the walk cycle while sliding the placement across the row. 2:1 frames match
/// the wide-cell aspect of the header row. Keep in sync with `logo.rs`.
const CAT_FRAME_W: u32 = 80;
const CAT_FRAME_H: u32 = 40;
const CAT_FRAMES: u32 = 4;

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

    // Cat walk sprite sheet: the alpha mask of all frames side by side.
    let sheet = render_cat_sheet(CAT_FRAME_W, CAT_FRAME_H, CAT_FRAMES);
    let cat_mask: Vec<u8> = sheet.data().iter().skip(3).step_by(4).copied().collect();
    let cpath = format!("{out_dir}/cat-mask.gray");
    std::fs::write(&cpath, &cat_mask).expect("write cat mask");
    println!(
        "wrote {cpath} ({} bytes, {}x{} sheet of {} frames)",
        cat_mask.len(),
        CAT_FRAME_W * CAT_FRAMES,
        CAT_FRAME_H,
        CAT_FRAMES
    );
    let cprev = format!("{out_dir}/cat-preview.png");
    std::fs::write(&cprev, sheet.encode_png().expect("encode cat png")).expect("write cat preview");
    println!("wrote {cprev}");
}

/// Render the whole cat walk sheet (white-on-transparent) as a pixmap: `frames`
/// walk poses side by side, each `fw`×`fh` px.
fn render_cat_sheet(fw: u32, fh: u32, frames: u32) -> Pixmap {
    let mut pm = Pixmap::new(fw * frames, fh).expect("cat sheet pixmap");
    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color_rgba8(0xff, 0xff, 0xff, 0xff);
    for f in 0..frames {
        // Phase around the gait cycle for this frame (0..1).
        let phase = f as f32 / frames as f32;
        draw_cat(&mut pm, &paint, f * fw, fw, fh, phase);
    }
    pm
}

/// Draw one walking-cat silhouette (facing right) into `pm`, its frame's left edge
/// at pixel `ox`. `phase` (0..1) drives the leg gait. Built from ellipses (body,
/// head, tail, four legs) plus two triangle ears — no rasteriser dependency.
fn draw_cat(pm: &mut Pixmap, paint: &Paint, ox: u32, fw: u32, fh: u32, phase: f32) {
    let w = fw as f32;
    let h = fh as f32;
    // Normalised (within-frame) coords → absolute pixels.
    let px = |x: f32| ox as f32 + x * w;
    let py = |y: f32| y * h;

    // Four legs first (drawn under the body). Diagonal pairs move together (a
    // trot): legs 0 & 3 share a phase, 1 & 2 the opposite, so the sheet reads as
    // a walk cycle as the crop advances.
    let leg_base = [0.30f32, 0.44, 0.60, 0.72];
    let leg_phase = [0.0f32, 0.5, 0.5, 0.0];
    for i in 0..4 {
        let swing = 0.05 * (std::f32::consts::TAU * (phase + leg_phase[i])).sin();
        fill_ellipse(
            pm,
            paint,
            px(leg_base[i] + swing),
            py(0.80),
            0.03 * w,
            0.18 * h,
        );
    }
    // Raised tail at the back (left), curving up.
    fill_ellipse(pm, paint, px(0.13), py(0.34), 0.045 * w, 0.22 * h);
    // Body: a long horizontal ellipse.
    fill_ellipse(pm, paint, px(0.46), py(0.54), 0.32 * w, 0.16 * h);
    // Head at the front (right).
    fill_ellipse(pm, paint, px(0.80), py(0.44), 0.13 * w, 0.20 * h);
    // Two triangle ears atop the head.
    fill_triangle(
        pm,
        paint,
        (px(0.72), py(0.30)),
        (px(0.76), py(0.10)),
        (px(0.81), py(0.30)),
    );
    fill_triangle(
        pm,
        paint,
        (px(0.81), py(0.30)),
        (px(0.86), py(0.10)),
        (px(0.90), py(0.30)),
    );
}

/// Fill a triangle through three pixel points.
fn fill_triangle(pm: &mut Pixmap, paint: &Paint, a: (f32, f32), b: (f32, f32), c: (f32, f32)) {
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(b.0, b.1);
    pb.line_to(c.0, c.1);
    pb.close();
    let path = pb.finish().expect("triangle path");
    pm.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
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
