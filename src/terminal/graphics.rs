//! Kitty graphics-protocol primitives, used to overlay the header paw logo, its
//! click pulse, and the walking cat as real raw-RGBA images on top of the ratatui
//! frame.
//!
//! This is presentation-only and kitty-specific, so it lives in the dashboard's
//! terminal layer rather than `cm-core`. Every operation degrades to a no-op /
//! `None` when the terminal can't do graphics (multiplexed under zellij/tmux, or
//! not kitty at all) — the caller draws an emoji glyph instead.
//!
//! Escapes are written straight to stdout *after* `terminal.draw()` has flushed
//! its frame, so there is no interleaving with ratatui's own writes (the event
//! loop is single-threaded). Every command carries `q=2` to suppress kitty's
//! `OK`/error acknowledgements, which would otherwise land in stdin and corrupt
//! crossterm's event stream.

use std::io::Write;
use std::sync::OnceLock;

use base64::Engine as _;
use crossterm::{cursor::MoveTo, queue};

/// Pixel dimensions of one terminal cell, as reported by the kernel/terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSize {
    pub w: u16,
    pub h: u16,
}

/// Image id for the header paw (kitty keys placements by image + placement id).
pub const PAW_IMAGE_ID: u32 = 7101;

/// A single image placement: where on the grid, how big (in cells), which slice
/// of the source (for sprite sheets), and a sub-cell pixel offset (for smooth
/// motion). Built by the logo/animation layer and handed to [`place`].
pub struct Placement {
    pub image: u32,
    pub placement: u32,
    /// 0-based top-left cell.
    pub col: u16,
    pub row: u16,
    /// Cell box to scale the image into (`c`/`r`), or `None` to display at the
    /// image's **native pixel size**. Scaling to a cell box makes kitty snap the
    /// placement to the grid, quantizing the sub-cell `offset` — so a smoothly
    /// *moving* placement (the walking cat) must use native size; a static one (the
    /// paw) can scale.
    pub cells: Option<(u16, u16)>,
    /// Stacking order: negative draws under text, >= 0 over it.
    pub z: i32,
    /// Source crop `(x, y, w, h)` in source pixels — a sprite-sheet frame. `None`
    /// shows the whole image.
    pub crop: Option<(u32, u32, u32, u32)>,
    /// Sub-cell pixel offset `(X, Y)` applied to the top-left cell — lets a
    /// placement glide smoothly between cells (honored only at native `cells: None`
    /// size; a scaled placement snaps to the grid and ignores it).
    pub offset: (u16, u16),
}

/// Whether this process is talking to kitty and not through a multiplexer that
/// swallows the graphics protocol. Cached — the terminal identity is fixed for
/// the process lifetime (a window resize keeps kitty kitty).
///
/// The kitty variables alone are **not** the question, because they are
/// inherited: another emulator launched from a kitty shell exports them into
/// every session it opens, so a `KITTY_PID` can outlive the kitty that set it.
/// [`Capabilities::graphics`](super::Capabilities::graphics) is what settles who
/// is actually drawing these cells; the env read stays as the second half of the
/// `and`, since the resolved backend falls back to Kitty when nothing claims the
/// process.
fn graphics_env_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let in_kitty = std::env::var_os("KITTY_PID").is_some()
            || std::env::var_os("KITTY_WINDOW_ID").is_some();
        // zellij/tmux don't forward the kitty graphics protocol, so even inside a
        // kitty window an image would land nowhere. Treat them as incapable.
        // Their backends answer `graphics: false` too — this stays because the
        // config can pin `backend = "kitty"` from inside a multiplexer, to drive
        // the outer window, and the escapes would still land in the pane.
        let multiplexed =
            std::env::var_os("ZELLIJ").is_some() || std::env::var_os("TMUX").is_some();
        in_kitty && !multiplexed && super::get().capabilities().graphics
    })
}

/// Cell pixel size from `ioctl(TIOCGWINSZ)`. Kitty fills in `ws_xpixel`/
/// `ws_ypixel`; terminals that leave them zero (and non-ttys) yield `None`.
/// Re-queried live so a font-size change is picked up.
pub fn cell_size() -> Option<CellSize> {
    // SAFETY: an all-zero `winsize` is a valid starting value; the ioctl fills it
    // and we check its return code before trusting the fields.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    Some(CellSize {
        w: ws.ws_xpixel / ws.ws_col,
        h: ws.ws_ypixel / ws.ws_row,
    })
}

/// The terminal's cell size iff it can render kitty graphics, else `None`. This
/// single call is the capability gate the logo layer branches on.
pub fn capability() -> Option<CellSize> {
    if graphics_env_ok() { cell_size() } else { None }
}

/// Query terminal palette entry `index` (0..=255) via an **OSC 4** escape and
/// parse the reply off stdin. Unlike `kitten @ get-colors` this is a terminal
/// query, so it works regardless of kitty's remote-control auth. Used to tint the
/// header paw to the terminal's *actual* green/yellow so it matches the Sessions
/// status symbols under any theme.
///
/// Must be called during setup — raw mode on (so the reply isn't line-buffered),
/// but **before** the event loop or mouse/focus reporting start consuming stdin,
/// or an unrelated input escape could be read instead. Returns `None` on any
/// timeout / unsupported terminal (the caller keeps its baked default).
pub fn query_palette(index: u8) -> Option<(u8, u8, u8)> {
    {
        let mut out = std::io::stdout().lock();
        out.write_all(format!("\x1b]4;{index};?\x1b\\").as_bytes())
            .ok()?;
        out.flush().ok()?;
    }
    read_osc_reply(index)
}

/// Read stdin until the OSC 4 reply for `index` arrives (or a short budget
/// elapses). Bytes that aren't the reply are discarded.
fn read_osc_reply(index: u8) -> Option<(u8, u8, u8)> {
    let needle = format!("4;{index};rgb:");
    let start = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(150);
    let mut acc: Vec<u8> = Vec::with_capacity(128);
    loop {
        let elapsed = start.elapsed();
        if elapsed >= budget {
            return None;
        }
        let remaining_ms = (budget - elapsed).as_millis() as libc::c_int;
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd; kernel only reads `remaining_ms`.
        if unsafe { libc::poll(&mut pfd, 1, remaining_ms) } <= 0 {
            return None;
        }
        let mut tmp = [0u8; 128];
        // SAFETY: reading into a local buffer of the length we pass.
        let n = unsafe { libc::read(libc::STDIN_FILENO, tmp.as_mut_ptr().cast(), tmp.len()) };
        if n <= 0 {
            return None;
        }
        acc.extend_from_slice(&tmp[..n as usize]);
        if let Some(rgb) = parse_osc_rgb(&acc, &needle) {
            return Some(rgb);
        }
        if acc.len() > 1024 {
            return None; // runaway / noise — give up, use the default
        }
    }
}

/// Parse an `…4;<index>;rgb:RRRR/GGGG/BBBB<ST>` reply out of `acc`. Requires the
/// terminator so a half-read reply isn't parsed early; `None` until then.
fn parse_osc_rgb(acc: &[u8], needle: &str) -> Option<(u8, u8, u8)> {
    let s = std::str::from_utf8(acc).ok()?;
    let after = &s[s.find(needle)? + needle.len()..];
    let end = after.find(['\x07', '\x1b'])?; // ST (ESC \) or BEL terminates
    let mut groups = after[..end].split('/');
    let r = scale_hex(groups.next()?)?;
    let g = scale_hex(groups.next()?)?;
    let b = scale_hex(groups.next()?)?;
    Some((r, g, b))
}

/// Scale a 1–4 hex-digit channel (terminals usually report 16-bit) down to 8-bit.
fn scale_hex(h: &str) -> Option<u8> {
    let v = u16::from_str_radix(h, 16).ok()?;
    Some(match h.len() {
        1 => (v * 0x11) as u8,
        2 => v as u8,
        3 => (v >> 4) as u8,
        _ => (v >> 8) as u8,
    })
}

/// Write one `ESC _ G <control> ; <payload> ESC \` graphics command. `payload`
/// (already base64) is omitted along with its `;` when empty.
fn write_cmd(out: &mut impl Write, control: &str, payload: &str) -> std::io::Result<()> {
    out.write_all(b"\x1b_G")?;
    out.write_all(control.as_bytes())?;
    if !payload.is_empty() {
        out.write_all(b";")?;
        out.write_all(payload.as_bytes())?;
    }
    out.write_all(b"\x1b\\")
}

/// Base64 chars per transmission chunk. Kitty caps a chunk at 4096 bytes of
/// payload; a multiple of 4 keeps each chunk a whole number of base64 groups.
const CHUNK: usize = 4096;

/// Transmit a raw straight-alpha RGBA image (kitty `f=32`, `w`x`h` px, 4
/// bytes/pixel) **without** displaying it (`a=t`); the caller places it
/// separately. When frames are later appended with [`append_frame`], this becomes
/// **frame 1** of the animation.
pub fn transmit_rgba(id: u32, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    transmit_to(&mut out, &format!("f=32,t=d,s={w},v={h},i={id},a=t"), rgba)?;
    out.flush()
}

/// Append an animation frame to image `id` (`a=f`). `X=1` makes it a **full
/// replacement** of the canvas (each frame is independent, not alpha-blended onto
/// the previous), and `z=gap_ms` is how long kitty holds this frame before
/// advancing. Raw straight-alpha RGBA, `w`x`h` px.
///
/// The frame is sent in **one unchunked escape**, unlike the chunked
/// [`transmit_rgba`] base. Chunked `a=f` is broken in kitty (verified against
/// 0.47.4): each chunked frame overwrites frame 2 instead of appending — kitty's
/// ack reports `r=2` for every frame — so the whole animation collapses to a
/// single frame and never moves. A single escape (even one over kitty's 4096-byte
/// per-chunk cap, which it accepts) appends correctly, ack `r` counting up
/// 2,3,4,…. Our paw frames are small (64×64 → 16 KiB), so one escape is fine.
pub fn append_frame(id: u32, w: u32, h: u32, gap_ms: u32, rgba: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    transmit_unchunked(
        &mut out,
        &format!("a=f,f=32,t=d,s={w},v={h},i={id},X=1,z={gap_ms}"),
        rgba,
    )?;
    out.flush()
}

/// Park image `id`'s animation stopped on frame 1 — the resting state, so a
/// placement shows the (full-opacity) base frame until a click plays it.
pub fn stop_animation(id: u32) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    write_cmd(&mut out, &format!("a=a,i={id},s=1,c=1,q=2"), "")?;
    out.flush()
}

/// Play image `id`'s animation `loops` times, then stop on the last frame: reset
/// to frame 1 (`c=1`), run (`s=3`) with kitty's `v = loops + 1` (v=2 → 1 loop,
/// v=3 → 2 loops; v=1 would be infinite, so `loops` must be ≥ 1). Kitty advances
/// the frames autonomously — the client sends nothing more.
pub fn play_loops(id: u32, loops: u32) -> std::io::Result<()> {
    let v = loops.max(1) + 1;
    let mut out = std::io::stdout().lock();
    write_cmd(&mut out, &format!("a=a,i={id},c=1,s=3,v={v},q=2"), "")?;
    out.flush()
}

/// Remove every placement of `id` (keep the image data) — used to drop the
/// previously-displayed colour when swapping which paw is shown.
pub fn delete_placements(id: u32) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    write_cmd(&mut out, &format!("a=d,d=i,i={id},q=2"), "")?;
    out.flush()
}

/// Transmit an entire payload in a **single** graphics escape — no `m=1`
/// continuation chunking. Kitty caps a *chunk* at 4096 base64 bytes but accepts a
/// larger single escape; [`append_frame`] depends on this because chunked `a=f`
/// frame appends are broken in kitty (see there). Writes to an arbitrary sink so
/// the wire format stays unit-testable.
fn transmit_unchunked(out: &mut impl Write, header: &str, data: &[u8]) -> std::io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    write_cmd(out, &format!("{header},q=2,m=0"), &b64)
}

/// Wire-format core of the transmit fns, writing to an arbitrary sink so the
/// exact escape bytes are unit-testable without a terminal. `header` is the first
/// chunk's control data (format/size/id/action); every chunk adds `q=2` and the
/// `m` continuation flag.
fn transmit_to(out: &mut impl Write, header: &str, data: &[u8]) -> std::io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let bytes = b64.as_bytes();
    if bytes.is_empty() {
        return Ok(());
    }
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + CHUNK).min(bytes.len());
        let more = u8::from(end < bytes.len());
        let piece = std::str::from_utf8(&bytes[i..end]).expect("base64 is ascii");
        // First chunk declares format/id/action; continuations carry only `m`.
        let control = if i == 0 {
            format!("{header},q=2,m={more}")
        } else {
            format!("q=2,m={more}")
        };
        write_cmd(out, &control, piece)?;
        i = end;
    }
    Ok(())
}

/// Display a placement of an already-transmitted image. Re-issuing with the same
/// `(image, placement)` id replaces it in place — no stacking, no flicker — so
/// callers can just re-place every frame. `C=1` keeps the cursor from advancing
/// (we don't want ratatui's next diff to fight a moved cursor).
pub fn place(p: &Placement) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    queue!(out, MoveTo(p.col, p.row))?;
    write_cmd(&mut out, &place_control(p), "")?;
    out.flush()
}

/// The `a=p` control-data string for a placement (everything after `_G`, minus
/// the cursor move). Split out so the exact key ordering/values are testable.
fn place_control(p: &Placement) -> String {
    // `C=1` keeps the cursor from advancing so ratatui's next diff isn't fighting
    // a moved cursor; `q=2` suppresses the ack.
    let mut control = format!("a=p,i={},p={},", p.image, p.placement);
    // `c`/`r` scale into a cell box; omitting them shows the image at native pixel
    // size (which is what lets the sub-cell `X`/`Y` offset actually glide).
    if let Some((cols, rows)) = p.cells {
        control.push_str(&format!("c={cols},r={rows},"));
    }
    control.push_str(&format!("z={},C=1,q=2", p.z));
    if let Some((x, y, w, h)) = p.crop {
        control.push_str(&format!(",x={x},y={y},w={w},h={h}"));
    }
    let (ox, oy) = p.offset;
    if ox > 0 {
        control.push_str(&format!(",X={ox}"));
    }
    if oy > 0 {
        control.push_str(&format!(",Y={oy}"));
    }
    control
}

/// Remove placements of `id` *and* free the image data (teardown).
pub fn free_image(id: u32) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    write_cmd(&mut out, &format!("a=d,d=I,i={id},q=2"), "")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(image: u32, cols: u16, rows: u16) -> Placement {
        Placement {
            image,
            placement: 1,
            col: 0,
            row: 0,
            cells: Some((cols, rows)),
            z: 1,
            crop: None,
            offset: (0, 0),
        }
    }

    #[test]
    fn transmit_single_chunk_envelope() {
        // A payload short enough to fit one chunk carries full control data and
        // m=0, wrapped in the APC envelope, and no trailing continuation.
        let mut buf = Vec::new();
        transmit_to(&mut buf, "f=100,i=7101,t=d,a=t", b"hi").unwrap();
        let s = String::from_utf8(buf).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hi");
        assert_eq!(s, format!("\x1b_Gf=100,i=7101,t=d,a=t,q=2,m=0;{b64}\x1b\\"));
    }

    #[test]
    fn transmit_rgba_header_carries_size() {
        // The raw-RGBA path declares f=32 with pixel dimensions.
        let mut buf = Vec::new();
        transmit_to(&mut buf, "f=32,s=2,v=1,i=7101,t=d,a=t", &[0u8; 8]).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("\x1b_Gf=32,s=2,v=1,i=7101,t=d,a=t,q=2,m=0;"));
    }

    #[test]
    fn transmit_unchunked_is_one_escape() {
        // An animation frame is one escape carrying m=0 and the whole payload —
        // never chunked, because chunked a=f mis-appends in kitty (each chunked
        // frame overwrites frame 2). A regression here would silently kill the
        // click pulse (the animation collapses to a single frame).
        let mut buf = Vec::new();
        transmit_unchunked(&mut buf, "a=f,f=32,s=2,v=1,i=7101,X=1,z=40", &[0u8; 8]).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode([0u8; 8]);
        assert_eq!(
            s,
            format!("\x1b_Ga=f,f=32,s=2,v=1,i=7101,X=1,z=40,q=2,m=0;{b64}\x1b\\")
        );
    }

    #[test]
    fn transmit_chunks_large_payload() {
        // > one chunk: the first frame declares format/id and m=1, the middle is
        // m=1 with only q=, and the last flips to m=0. Each frame is its own APC.
        let png = vec![0u8; CHUNK * 2]; // base64 expands to > 2 chunks
        let mut buf = Vec::new();
        transmit_to(&mut buf, "f=100,i=42,t=d,a=t", &png).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let frames: Vec<&str> = s.split("\x1b\\").filter(|f| !f.is_empty()).collect();
        assert!(frames.len() >= 3, "expected chunking, got {}", frames.len());
        assert!(frames[0].starts_with("\x1b_Gf=100,i=42,t=d,a=t,q=2,m=1;"));
        assert!(frames[1].starts_with("\x1b_Gq=2,m=1;"));
        assert!(frames.last().unwrap().starts_with("\x1b_Gq=2,m=0;"));
    }

    #[test]
    fn parses_osc4_palette_reply() {
        // xterm-style 16-bit reply for color2 (green): take the high byte of each.
        let reply = b"\x1b]4;2;rgb:a6a6/e3e3/a1a1\x1b\\";
        assert_eq!(parse_osc_rgb(reply, "4;2;rgb:"), Some((0xa6, 0xe3, 0xa1)));
        // BEL-terminated, 8-bit channels.
        let reply = b"\x1b]4;3;rgb:f9/e2/af\x07";
        assert_eq!(parse_osc_rgb(reply, "4;3;rgb:"), Some((0xf9, 0xe2, 0xaf)));
        // Half-read (no terminator yet) → None, so we wait for the rest.
        assert_eq!(parse_osc_rgb(b"\x1b]4;2;rgb:a6a6/e3", "4;2;rgb:"), None);
    }

    #[test]
    fn place_control_basic() {
        assert_eq!(
            place_control(&place(7101, 2, 1)),
            "a=p,i=7101,p=1,c=2,r=1,z=1,C=1,q=2"
        );
    }

    #[test]
    fn place_control_with_crop_and_offset() {
        // Sprite-sheet frame (M3): a source crop plus a sub-cell glide offset.
        let p = Placement {
            crop: Some((64, 0, 32, 32)),
            offset: (5, 0),
            ..place(7300, 3, 2)
        };
        assert_eq!(
            place_control(&p),
            "a=p,i=7300,p=1,c=3,r=2,z=1,C=1,q=2,x=64,y=0,w=32,h=32,X=5"
        );
    }

    #[test]
    fn place_control_native_size_omits_cells() {
        // A moving placement (the walking cat) uses native pixel size — no c/r — so
        // kitty honours the sub-cell X offset instead of snapping it to the grid.
        let p = Placement {
            cells: None,
            crop: Some((160, 0, 80, 40)),
            offset: (7, 0),
            ..place(7111, 0, 0)
        };
        assert_eq!(
            place_control(&p),
            "a=p,i=7111,p=1,z=1,C=1,q=2,x=160,y=0,w=80,h=40,X=7"
        );
    }
}
