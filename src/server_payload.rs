//! The `miao-server` binaries this dashboard carries, and the mapping
//! from a remote's `uname -sm` to the one it can run.
//!
//! A dashboard built carrying a server can **deploy it**: on
//! connect, a host with no matching `miao-server` gets one pushed over
//! the ssh connection the probe just opened, instead of a "not found — go deploy
//! it" message. That is the zero-touch provisioning the crate split gave up when
//! the dashboard stopped linking the pty pool, restored against an embedded
//! *server* build rather than against the dashboard binary itself
//! (`docs/crate-split.md`).
//!
//! **What a build carries comes from one environment variable.** `cargo xtask`
//! obtains the servers — cross-compiled here, downloaded from a release, or
//! handed over as paths — and points `CM_SERVER_PAYLOAD_MANIFEST` at the
//! archives; `build.rs` `include_bytes!`es them into [`PAYLOADS`]. Unset, which
//! is every ordinary `cargo build`, the table is empty, nothing extra is linked,
//! and `backend.rs` behaves exactly as it did before any of this existed: probe,
//! don't deploy, and say what's wrong.
//!
//! The archives are `include_bytes!`d rather than patched into the linked binary
//! afterwards. The alternative was measured and dropped: a reserved slot bought
//! only the ability to re-bundle a `miao` without recompiling it, which saves 58
//! seconds and still needs cargo (to build `xtask`), and it cost a binary format,
//! two `unsafe` blocks, a fixed reservation, and a `codesign` step on macOS.
//! `include_bytes!` data is allocated and referenced, so — like the slot it
//! replaced — it survives `strip`.

use std::io::Read;

/// One embedded server build.
pub(crate) struct ServerPayload {
    /// The Rust target triple it was built for.
    pub(crate) target: &'static str,
    /// Hex sha256 of the **decompressed** bytes — i.e. of the file that lands on
    /// the remote. The dashboard writes this into a marker beside the deployed
    /// binary so a later connect can tell *this* build from a same-versioned
    /// one, which is what makes the dev loop (rebuild, reconnect, get the new
    /// server) work without a version bump.
    pub(crate) sha256: &'static str,
    /// The gzip'd binary.
    pub(crate) gz: &'static [u8],
}

impl ServerPayload {
    /// Inflate to the bytes to upload.
    pub(crate) fn decompress(&self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(self.gz).read_to_end(&mut out)?;
        Ok(out)
    }
}

// The table `build.rs` generates: one `ServerPayload` per line of the manifest,
// each `gz` an `include_bytes!` of the archive. Empty — a `&[]` costing nothing —
// whenever `CM_SERVER_PAYLOAD_MANIFEST` was unset, which is every ordinary build.
include!(concat!(env!("OUT_DIR"), "/payloads.rs"));

/// The payloads this binary carries.
pub(crate) fn payloads() -> &'static [ServerPayload] {
    PAYLOADS
}

/// The targets, in preference order, that a host reporting `uname -sm` could
/// run. Empty for anything unrecognised, which simply means "no upload" — never
/// a guess, since uploading a binary for the wrong ABI produces a confusing
/// exec failure on someone else's machine.
///
/// glibc first, musl second: `uname` cannot distinguish them, and glibc is the
/// mainstream server libc. Nothing builds a musl server today, so those entries
/// exist only so that one handed over by `xtask --server <musl-triple>=<path>` is
/// selectable rather than silently unreachable.
///
/// Note what does **not** happen: only the first matching candidate is ever tried.
/// If a glibc payload turns out not to run on the host (Alpine), the post-upload
/// `--version` check rejects it and the connection falls back to whatever the host
/// already has; `UploadGate` then suppresses re-sends of that digest, so a musl
/// payload sitting right behind it in this list would never be reached. Closing
/// that means looping candidates at the deploy site, not reordering here. Pure.
pub(crate) fn target_candidates(uname_sm: &str) -> &'static [&'static str] {
    let mut fields = uname_sm.split_whitespace();
    let (Some(os), Some(machine)) = (fields.next(), fields.next()) else {
        return &[];
    };
    let arch = match machine {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return &[],
    };
    match (os, arch) {
        ("Linux", "x86_64") => &["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"],
        ("Linux", "aarch64") => &["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"],
        ("Darwin", "x86_64") => &["x86_64-apple-darwin"],
        ("Darwin", "aarch64") => &["aarch64-apple-darwin"],
        _ => &[],
    }
}

/// The best embedded payload for a host reporting `uname -sm`, or `None` when we
/// carry nothing it could run.
pub(crate) fn for_uname(uname_sm: &str) -> Option<&'static ServerPayload> {
    pick(target_candidates(uname_sm), payloads())
}

/// Candidate-order lookup, over an injected table so the preference order is
/// testable in a build that bundles nothing (which is the default). Pure.
fn pick<'a>(candidates: &[&str], table: &'a [ServerPayload]) -> Option<&'a ServerPayload> {
    candidates
        .iter()
        .find_map(|c| table.iter().find(|p| p.target == *c))
}

/// What this build carries, for the startup log and the hosts panel's diagnosis.
pub(crate) fn embedded_targets() -> Vec<&'static str> {
    payloads().iter().map(|p| p.target).collect()
}

/// Human-readable inventory, for `miao --version`.
///
/// Whether a given `miao` can deploy a server is otherwise invisible until you
/// connect to a host and it either works or explains itself — and since the
/// answer depends on what the binary was *built with*, not on any config, there is
/// nothing else to look at. Always says something, including "none", so the
/// absence of payloads reads as an answer rather than as an old build that
/// doesn't report. The digest is what distinguishes two same-version builds.
pub(crate) fn describe() -> String {
    describe_table(payloads())
}

/// [`describe`] over an injected table, so both branches are testable in a
/// binary whose own table is whatever it was built with. Pure.
fn describe_table(table: &[ServerPayload]) -> String {
    if table.is_empty() {
        return "embedded miao-server: none \
                (remote hosts need one installed; `cargo xtask dist` builds a bundled miao)"
            .to_string();
    }
    let mut out = String::from("embedded miao-server:");
    for p in table {
        out.push_str(&format!(
            "\n  {:<28} {:>8}  {}",
            p.target,
            format!("{:.1} MiB", p.gz.len() as f64 / (1u64 << 20) as f64),
            &p.sha256[..12.min(p.sha256.len())],
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in table, so these tests say the same thing whether or not the
    /// binary running them was bundled.
    fn table() -> Vec<ServerPayload> {
        vec![
            ServerPayload {
                target: "x86_64-unknown-linux-musl",
                sha256: "m",
                gz: b"",
            },
            ServerPayload {
                target: "x86_64-unknown-linux-gnu",
                sha256: "g",
                gz: b"",
            },
        ]
    }

    #[test]
    fn uname_maps_to_the_targets_that_host_could_run() {
        assert_eq!(
            target_candidates("Linux x86_64"),
            ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"]
        );
        assert_eq!(
            target_candidates("Linux aarch64"),
            ["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"]
        );
        // `uname -m` spells the same arches differently across kernels.
        assert_eq!(
            target_candidates("Linux arm64"),
            target_candidates("Linux aarch64")
        );
        assert_eq!(
            target_candidates("Linux amd64"),
            target_candidates("Linux x86_64")
        );
        assert_eq!(target_candidates("Darwin arm64"), ["aarch64-apple-darwin"]);
    }

    #[test]
    fn an_unrecognised_host_offers_no_candidate_rather_than_a_guess() {
        // Wrong ABI is worse than no upload: it lands and then fails to exec.
        assert!(target_candidates("Linux riscv64").is_empty());
        assert!(target_candidates("FreeBSD amd64").is_empty());
        assert!(target_candidates("Linux").is_empty());
        assert!(target_candidates("").is_empty());
    }

    #[test]
    fn glibc_is_preferred_over_musl_regardless_of_table_order() {
        let t = table();
        assert_eq!(
            pick(target_candidates("Linux x86_64"), &t).unwrap().sha256,
            "g"
        );
    }

    #[test]
    fn a_host_we_carry_nothing_for_picks_nothing() {
        let t = table();
        assert!(pick(target_candidates("Linux aarch64"), &t).is_none());
        assert!(pick(target_candidates("Linux riscv64"), &t).is_none());
    }

    #[test]
    fn a_payload_round_trips_through_gzip() {
        use std::io::Write;
        let raw = b"\x7fELF not really, but it compresses".repeat(64);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(&raw).unwrap();
        let gz = enc.finish().unwrap();
        // `gz` has to outlive the borrow in `ServerPayload`, hence the leak; this
        // is the one place a payload isn't `'static` from the embedded table.
        let payload = ServerPayload {
            target: "t",
            sha256: "s",
            gz: Box::leak(gz.into_boxed_slice()),
        };
        assert_eq!(payload.decompress().unwrap(), raw);
    }

    #[test]
    fn version_output_answers_the_question_either_way() {
        // "none" is stated rather than implied by silence: an empty section and
        // a build too old to report one would otherwise look identical.
        let empty = describe_table(&[]);
        assert!(empty.contains("none"), "{empty}");
        assert!(empty.contains("xtask dist"), "{empty}");
        assert_eq!(empty.lines().count(), 1, "{empty}");

        let listed = describe_table(&table());
        for p in table() {
            assert!(listed.contains(p.target), "{listed}");
        }
        // The digest is there to tell two same-version builds apart.
        assert!(listed.contains('m') && listed.contains('g'), "{listed}");
        assert_eq!(listed.lines().count(), 3, "{listed}");
    }

    #[test]
    fn whatever_this_binary_carries_is_well_formed() {
        // Vacuously true in a regular build (nothing is embedded); in a bundled
        // one this pins that the manifest produced usable entries.
        for p in payloads() {
            assert_eq!(p.sha256.len(), 64, "{}: bad digest", p.target);
            assert!(!p.gz.is_empty(), "{}: empty payload", p.target);
            assert_eq!(&p.gz[..2], b"\x1f\x8b", "{}: not gzip", p.target);
        }
        let mut targets = embedded_targets();
        let before = targets.len();
        targets.sort_unstable();
        targets.dedup();
        assert_eq!(
            targets.len(),
            before,
            "duplicate target in the payload table"
        );
    }
}
