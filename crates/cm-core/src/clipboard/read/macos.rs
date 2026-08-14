//! The macOS clipboard read: `NSPasteboard` through `objc2`.
//!
//! **`pbpaste` cannot do this**, which is worth stating because it is the obvious
//! first guess: it emits text flavours only (`-Prefer txt|rtf|ps`) and will not
//! hand you a screenshot's PNG bytes. `osascript -e 'the clipboard as «class
//! PNGf»'` returns hex-encoded text — double the size, and needing a decode.
//! Neither is a path, so this is the one place the bridge links a platform
//! framework instead of spawning a tool.
//!
//! # Two flavours in, two formats out
//!
//! We ask for `[PNG, TIFF]` and take whichever the pasteboard prefers — a
//! screenshot lands as TIFF, a browser copy often as PNG. Because
//! `NSBitmapImageRep` re-encodes, **either source can produce either format**,
//! which is why [`available`] advertises the whole allowlist whenever anything is
//! there. Using AppKit's own encoder rather than the Rust `image` crate is
//! deliberate: no new dependency, and it is the platform's decoder for the
//! platform's data.
//!
//! `availableTypeFromArray` rather than walking `pasteboard.types()`: it returns
//! the first match and handles absence in one call.
//!
//! # No file-URL fast path, on purpose
//!
//! A pasteboard can also offer `NSPasteboardTypeFileURL` — you copied a PNG in
//! Finder rather than screenshotting — and streaming straight off that disk path
//! would skip the materialization below. It is deliberately not done. A file URL
//! is the one case the OS's type system cannot answer "is this an image" for: it
//! is a pointer to arbitrary bytes, so the gate would have to become an extension
//! check plus a magic-number check, both written by us and both failing open if
//! wrong. The failure mode is a copied `~/.ssh/id_ed25519` leaving the machine,
//! which is worse than the text leak the gate exists to prevent. If the
//! Finder-copy case ever matters it should come back as "ask AppKit to load that
//! URL as an image and re-encode it", never as a byte passthrough.
//!
//! # Why this materializes
//!
//! `dataForType:` is `NSData`-based, there is no incremental pasteboard reader,
//! and lazy providers exist only on the writing side — so the bytes exist in full
//! in this process, and the transient peak is the conversion (decoded RGBA for a
//! 6K screenshot is ~80 MB). That is a large part of why the server is a separate
//! process: the high-water is reclaimed when it exits instead of accumulating
//! across a day of pasting in the TUI.
//!
//! These calls are **synchronous and expect the process's main thread** — see
//! [`super`] on the current-thread runtime that guarantees it.

use objc2::rc::Retained;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardType, NSPasteboardTypePNG,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSArray, NSData, NSDictionary};

use crate::clipboard::Format;

/// The pasteboard flavours we will read, in the order we prefer them. PNG first
/// so a pasteboard already holding it needs no re-encode at all.
fn source_types() -> Retained<NSArray<NSPasteboardType>> {
    // SAFETY: AppKit's own string constants, valid for the process's lifetime.
    let (png, tiff) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    NSArray::from_slice(&[png, tiff])
}

/// The flavour actually on the pasteboard, or `None` — which is also the answer
/// for a pasteboard holding only text, since text is never in the array we ask
/// about.
fn available_source(pb: &NSPasteboard) -> Option<Retained<NSPasteboardType>> {
    pb.availableTypeFromArray(&source_types())
}

pub(super) fn available() -> Vec<Format> {
    let pb = NSPasteboard::generalPasteboard();
    match available_source(&pb) {
        // Either flavour re-encodes to either format, so what we advertise is
        // what we can *produce*, not what is on the pasteboard.
        Some(_) => Format::ALL.to_vec(),
        None => Vec::new(),
    }
}

/// The encoded image, or `None` for any reason at all (nothing there, or a
/// flavour `NSBitmapImageRep` won't decode).
pub(super) fn read(fmt: Format) -> Option<Vec<u8>> {
    let pb = NSPasteboard::generalPasteboard();
    let source = available_source(&pb)?;
    let data = pb.dataForType(&source)?;
    // SAFETY: AppKit's own constant.
    let already_png = unsafe { &*source == NSPasteboardTypePNG };
    if fmt == Format::Png && already_png {
        // Nothing to convert: hand over the pasteboard's own bytes.
        return Some(data.to_vec());
    }
    reencode(&data, fmt)
}

fn reencode(data: &NSData, fmt: Format) -> Option<Vec<u8>> {
    let rep = NSBitmapImageRep::imageRepWithData(data)?;
    let props = NSDictionary::new();
    // SAFETY: the properties dictionary is empty, so it holds no value whose
    // type could be wrong — which is the only precondition this call has.
    let out = unsafe { rep.representationUsingType_properties(file_type(fmt), &props) }?;
    Some(out.to_vec())
}

fn file_type(fmt: Format) -> NSBitmapImageFileType {
    match fmt {
        Format::Png => NSBitmapImageFileType::PNG,
        Format::Bmp => NSBitmapImageFileType::BMP,
    }
}
