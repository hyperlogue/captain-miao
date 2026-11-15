//! The three Tower companions travel in one tar/XZ stream so compression can
//! share a dictionary. Extract only these regular files, entirely in memory;
//! the remote host still receives plain executables and needs no archiver.

use std::io::{self, Read};

use sha2::{Digest, Sha256};

/// The CLI goes first because deployment probes it before publishing the build.
pub(crate) const BINARIES: &[&str] = &["tower-cli", "tower", "tower-terminal-pool"];

pub(crate) fn decode(packed: &[u8], sha256: &str) -> io::Result<Vec<(&'static str, Vec<u8>)>> {
    let invalid = |reason| io::Error::new(io::ErrorKind::InvalidData, reason);
    let mut raw = Vec::new();
    lzma_rs::xz_decompress(&mut &packed[..], &mut raw)
        .map_err(|e| invalid(format!("unpacking Tower: {e}")))?;
    let digest: String = Sha256::digest(&raw)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if digest != sha256 {
        return Err(invalid("Tower archive digest mismatch".into()));
    }
    let mut archive = tar::Archive::new(raw.as_slice());
    let mut binaries = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = BINARIES
            .iter()
            .copied()
            .find(|name| entry.path_bytes().as_ref() == name.as_bytes())
            .ok_or_else(|| invalid("unexpected Tower archive member".into()))?;
        if !entry.header().entry_type().is_file()
            || binaries.iter().any(|(existing, _)| *existing == name)
        {
            return Err(invalid(format!("invalid or duplicate Tower member {name}")));
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            return Err(invalid(format!("empty Tower member {name}")));
        }
        binaries.push((name, bytes));
    }
    if binaries.len() != BINARIES.len() {
        return Err(invalid("incomplete Tower archive".into()));
    }
    binaries.sort_by_key(|(name, _)| BINARIES.iter().position(|expected| expected == name));
    Ok(binaries)
}
