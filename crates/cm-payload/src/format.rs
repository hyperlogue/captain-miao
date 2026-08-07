//! The reserved-slot format: how server payloads are written into an
//! already-linked dashboard, and how the dashboard reads them back.
//!
//! The dashboard reserves a run of bytes in a `static` (see
//! `src/server_payload.rs`) and `cargo xtask dist` overwrites them after linking.
//! Two properties follow from that choice, and both are why it is a reserved slot
//! rather than data appended to the file:
//!
//! - **It survives `strip`.** A static is allocated and loaded, so removing it
//!   would break the program; `strip` rewrites a binary from its own structure
//!   and simply does not carry trailing bytes across, which would wipe an
//!   appended payload silently.
//! - **It is one implementation for ELF and Mach-O.** Neither format lets you add
//!   a section after linking (Mach-O's `segedit -replace` is same-size-only for
//!   exactly this reason), but overwriting bytes the linker already placed
//!   disturbs nothing either format cares about.
//!
//! This module has no dependencies so both sides can share it: the writer is
//! `xtask`, the reader is the dashboard.

/// Marks the start of the slot. Long enough that a chance occurrence elsewhere
/// in a multi-megabyte binary is not a practical concern — and [`find`] confirms
/// a candidate against the capacity and the trailing [`SENTINEL`] anyway, so a
/// collision is detected rather than trusted.
pub const MAGIC: [u8; 16] = *b"cm-srv-payload\x00\x01";

/// Closes the slot, `capacity` bytes after the header.
pub const SENTINEL: [u8; 16] = *b"cm-srv-end\x00\x00\x00\x00\x00\x01";

/// `magic` + `used: u64` + `capacity: u64`.
pub const HEADER_LEN: usize = 16 + 8 + 8;

/// One payload in the slot: a target triple, the hex digest of the
/// **decompressed** binary, and the compressed bytes.
pub struct Entry<'a> {
    pub target: &'a str,
    pub sha256: &'a str,
    pub gz: &'a [u8],
}

/// Serialise entries into the slot body.
///
/// Metadata first, then the blobs, so a reader walks a compact run of
/// fixed-then-variable fields before touching megabytes.
pub fn encode(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        out.extend_from_slice(&(e.target.len() as u16).to_le_bytes());
        out.extend_from_slice(&(e.sha256.len() as u16).to_le_bytes());
        out.extend_from_slice(&(e.gz.len() as u64).to_le_bytes());
        out.extend_from_slice(e.target.as_bytes());
        out.extend_from_slice(e.sha256.as_bytes());
    }
    for e in entries {
        out.extend_from_slice(e.gz);
    }
    out
}

/// Parse a slot body written by [`encode`]. `None` on anything malformed — a
/// half-written or corrupted slot reads as "carries nothing", never as a
/// truncated payload that would then fail to exec on someone's server.
pub fn decode(body: &[u8]) -> Option<Vec<Entry<'_>>> {
    let mut cur = Reader { b: body, at: 0 };
    let count = cur.u32()? as usize;
    // A slot can only hold so many entries; refuse an absurd count before it
    // becomes an allocation.
    if count > 64 {
        return None;
    }
    let mut meta = Vec::with_capacity(count);
    for _ in 0..count {
        let target_len = cur.u16()? as usize;
        let sha_len = cur.u16()? as usize;
        let gz_len = cur.u64()? as usize;
        let target = std::str::from_utf8(cur.take(target_len)?).ok()?;
        let sha256 = std::str::from_utf8(cur.take(sha_len)?).ok()?;
        meta.push((target, sha256, gz_len));
    }
    let mut out = Vec::with_capacity(count);
    for (target, sha256, gz_len) in meta {
        out.push(Entry {
            target,
            sha256,
            gz: cur.take(gz_len)?,
        });
    }
    Some(out)
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.b.get(self.at..end)?;
        self.at = end;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// Where the slot sits in a linked binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// File offset of [`MAGIC`].
    pub at: usize,
    /// Bytes available for the body.
    pub capacity: usize,
}

impl Slot {
    /// File offset of the body.
    pub fn body_at(&self) -> usize {
        self.at + HEADER_LEN
    }
}

/// Locate the slot in a linked binary.
///
/// A candidate is only accepted when its capacity lands the [`SENTINEL`] exactly
/// where the header says it should, which is what makes this safe to run over a
/// binary whose slot already holds a payload: last round's compressed bytes could
/// in principle contain [`MAGIC`], but they will not also place the sentinel at
/// the implied distance. Two surviving candidates is an error rather than a
/// guess.
pub fn find(bin: &[u8]) -> Result<Slot, String> {
    let mut found: Option<Slot> = None;
    let mut from = 0;
    while let Some(rel) = window(&bin[from..], &MAGIC) {
        let at = from + rel;
        from = at + 1;
        let Some(cap) = bin
            .get(at + 24..at + 32)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
        else {
            continue;
        };
        let cap = cap as usize;
        let end = match at.checked_add(HEADER_LEN).and_then(|x| x.checked_add(cap)) {
            Some(e) => e,
            None => continue,
        };
        if bin.get(end..end + 16) != Some(&SENTINEL[..]) {
            continue;
        }
        if found.is_some() {
            return Err("this binary contains more than one payload slot".into());
        }
        found = Some(Slot { at, capacity: cap });
    }
    found.ok_or_else(|| {
        "no payload slot in this binary — build the dashboard with `--features bundle` \
         and CM_PAYLOAD_RESERVE set (`cargo xtask dist` does both)"
            .into()
    })
}

fn window(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Write `body` into the slot of an already-linked binary, in place.
///
/// The binary's length never changes — every byte written lands inside the
/// reservation the linker already placed.
pub fn write(bin: &mut [u8], slot: Slot, body: &[u8]) -> Result<(), String> {
    if body.len() > slot.capacity {
        return Err(format!(
            "payload is {} bytes but the slot reserves {} — rebuild the dashboard with a \
             larger CM_PAYLOAD_RESERVE",
            body.len(),
            slot.capacity
        ));
    }
    bin[slot.at + 16..slot.at + 24].copy_from_slice(&(body.len() as u64).to_le_bytes());
    let start = slot.body_at();
    bin[start..start + body.len()].copy_from_slice(body);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot_bytes(capacity: usize, used: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&MAGIC);
        v.extend_from_slice(&(used.len() as u64).to_le_bytes());
        v.extend_from_slice(&(capacity as u64).to_le_bytes());
        let mut body = vec![0xA5; capacity];
        body[..used.len()].copy_from_slice(used);
        v.extend_from_slice(&body);
        v.extend_from_slice(&SENTINEL);
        v
    }

    #[test]
    fn entries_round_trip() {
        let a = vec![1u8, 2, 3, 4];
        let b = vec![9u8; 300];
        let entries = vec![
            Entry {
                target: "x86_64-unknown-linux-gnu",
                sha256: "aa",
                gz: &a,
            },
            Entry {
                target: "aarch64-unknown-linux-gnu",
                sha256: "bb",
                gz: &b,
            },
        ];
        let body = encode(&entries);
        let back = decode(&body).expect("decodes");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].target, "x86_64-unknown-linux-gnu");
        assert_eq!(back[0].gz, &a[..]);
        assert_eq!(back[1].target, "aarch64-unknown-linux-gnu");
        assert_eq!(back[1].gz, &b[..]);
        assert_eq!(decode(&encode(&[])).unwrap().len(), 0);
    }

    /// A slot that is corrupt, truncated, or simply never written must read as
    /// "carries nothing". Half a payload would upload and then fail to exec on
    /// someone else's machine, which is a much worse failure than none.
    #[test]
    fn a_malformed_body_decodes_to_nothing_rather_than_half_a_payload() {
        let body = encode(&[Entry {
            target: "t",
            sha256: "s",
            gz: &[1, 2, 3, 4, 5, 6, 7, 8],
        }]);
        for cut in 1..body.len() {
            assert!(
                decode(&body[..cut]).is_none(),
                "truncation at {cut} decoded"
            );
        }
        assert!(decode(&[]).is_none());
        // An absurd count must not become an allocation.
        assert!(decode(&u32::MAX.to_le_bytes()).is_none());
    }

    #[test]
    fn find_locates_the_slot_and_write_stays_inside_it() {
        let mut bin = vec![0u8; 128];
        bin.extend_from_slice(&slot_bytes(1024, &[]));
        bin.extend_from_slice(&[0u8; 64]);
        let before = bin.len();

        let slot = find(&bin).expect("found");
        assert_eq!(slot.at, 128);
        assert_eq!(slot.capacity, 1024);

        let body = encode(&[Entry {
            target: "t",
            sha256: "s",
            gz: &[7; 100],
        }]);
        write(&mut bin, slot, &body).expect("fits");
        assert_eq!(bin.len(), before, "writing must not resize the binary");

        let used = u64::from_le_bytes(bin[slot.at + 16..slot.at + 24].try_into().unwrap()) as usize;
        let read = decode(&bin[slot.body_at()..slot.body_at() + used]).expect("reads back");
        assert_eq!(read[0].gz, &[7u8; 100][..]);
    }

    #[test]
    fn an_oversized_payload_is_refused_with_both_numbers() {
        let mut bin = slot_bytes(16, &[]);
        let slot = find(&bin).unwrap();
        let err = write(&mut bin, slot, &[0; 32]).unwrap_err();
        assert!(err.contains("32") && err.contains("16"), "{err}");
        assert!(err.contains("CM_PAYLOAD_RESERVE"), "{err}");
    }

    /// The reason the sentinel exists: re-injecting means scanning across bytes
    /// that already contain a payload, which could hold the magic by chance.
    #[test]
    fn a_magic_sequence_in_the_payload_is_not_mistaken_for_a_slot() {
        let mut decoy = Vec::new();
        decoy.extend_from_slice(&MAGIC);
        decoy.extend_from_slice(&0u64.to_le_bytes());
        decoy.extend_from_slice(&4096u64.to_le_bytes()); // a capacity with no sentinel
        let mut bin = decoy;
        bin.extend_from_slice(&[0u8; 64]);
        bin.extend_from_slice(&slot_bytes(256, &[]));
        let slot = find(&bin).expect("the real slot wins");
        assert_eq!(slot.capacity, 256);
    }

    #[test]
    fn two_real_slots_are_an_error_not_a_coin_flip() {
        let mut bin = slot_bytes(64, &[]);
        bin.extend_from_slice(&slot_bytes(64, &[]));
        assert!(find(&bin).unwrap_err().contains("more than one"));
    }

    #[test]
    fn a_binary_without_a_slot_says_how_to_get_one() {
        let err = find(&[0u8; 4096]).unwrap_err();
        assert!(err.contains("--features bundle"), "{err}");
        assert!(err.contains("xtask dist"), "{err}");
    }
}
