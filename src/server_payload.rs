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
    /// The xz-compressed binary. `xtask` packs it at preset 6, whose 8 MiB
    /// dictionary is what bounds the allocation below.
    pub(crate) packed: &'static [u8],
}

impl ServerPayload {
    /// Inflate to the bytes to upload.
    ///
    /// Pure-Rust xz, so a cross-compiled dashboard needs no system liblzma. Peak
    /// cost is the dictionary the stream header asks for — 9 MiB at the preset
    /// `xtask` uses, which is why that preset is pinned there rather than left
    /// to whatever compresses best.
    pub(crate) fn decompress(&self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        lzma_rs::xz_decompress(&mut &self.packed[..], &mut out)
            .map_err(|e| std::io::Error::other(format!("unpacking {}: {e}", self.target)))?;
        Ok(out)
    }
}

// The table `build.rs` generates: one `ServerPayload` per line of the manifest,
// each `packed` an `include_bytes!` of the archive. Empty — a `&[]` costing nothing —
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
/// mainstream server libc — its NSS is load-bearing rather than a nicety, since
/// a static build cannot see LDAP/SSSD users at all and its sessions would fail
/// to attach.
///
/// Both are offered because **selection has to be verified, not guessed**:
/// nothing we can ask a host cheaply reports its libc, so the deploy tries them
/// in order and keeps the first the host proves it can actually run. A glibc
/// binary has no loader on NixOS/Alpine/distroless; a musl one runs there but
/// fails the self-check on an LDAP host. Every combination resolves without a
/// guess, and the one host neither serves — NixOS with LDAP/SSSD users — is told
/// so plainly, because no payload we could ship serves it. Pure.
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

// ---------------------------------------------------------------------------
// The source chain
// ---------------------------------------------------------------------------

/// Environment variable prefix for both the per-target and the directory form.
const ENV_PREFIX: &str = "CAPTAIN_MIAO_SERVER";

/// Where a downloaded server is cached, under the user's cache dir. Versioned,
/// because a server is only interchangeable with others of the same version.
const CACHE_REL: &str = "captain-miao/servers";

/// Where a payload came from — for the connection log, which is the only place
/// a user can see *why* a particular binary was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PayloadSource {
    /// `$CAPTAIN_MIAO_SERVER_<TARGET>` named this exact file.
    EnvTarget,
    /// Found under `$CAPTAIN_MIAO_SERVER_DIR/<target>/`.
    EnvDir,
    /// Compiled into this binary.
    Embedded,
    /// Previously downloaded, sitting in the XDG cache.
    Cache,
}

impl PayloadSource {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            PayloadSource::EnvTarget => "env",
            PayloadSource::EnvDir => "env dir",
            PayloadSource::Embedded => "embedded",
            PayloadSource::Cache => "cache",
        }
    }
}

/// One server we can supply for one target.
///
/// Holds a *path* rather than the bytes for file-backed sources: the digest is
/// what the decision needs, and a multi-megabyte binary should only be read when
/// it is actually being sent.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) target: String,
    pub(crate) sha256: String,
    pub(crate) source: PayloadSource,
    /// `None` for the embedded table, whose bytes are already in the binary.
    path: Option<std::path::PathBuf>,
    /// Index into [`payloads`] for an embedded candidate.
    embedded: Option<usize>,
}

impl Candidate {
    /// The bytes to upload, inflating or reading as needed.
    pub(crate) fn bytes(&self) -> std::io::Result<Vec<u8>> {
        match (&self.path, self.embedded) {
            (Some(p), _) => std::fs::read(p),
            (None, Some(i)) => payloads()[i].decompress(),
            _ => Err(std::io::Error::other("candidate has no source")),
        }
    }

    /// Whether this came from somewhere the *user* pointed us at, and so is
    /// worth checking before it is sent. An embedded payload was built by
    /// `xtask`, which already verified its architecture.
    pub(crate) fn is_locally_sourced(&self) -> bool {
        matches!(
            self.source,
            PayloadSource::EnvTarget | PayloadSource::EnvDir
        )
    }
}

/// The two spellings of the per-target variable, in the order they are read.
///
/// Cargo's own convention is the uppercased, underscored one
/// (`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER`), so that is what we
/// document — but the verbatim triple is accepted too, because someone who has
/// just typed a target triple will try it, and a variable that silently does
/// nothing is the worst possible failure. Two `getenv`s is a cheap price for
/// not repeating the afternoon that `BINDGEN_EXTRA_CLANG_ARGS_<target>` cost.
/// Pure.
pub(crate) fn env_names_for(target: &str) -> [String; 2] {
    [
        format!("{ENV_PREFIX}_{}", target.to_uppercase().replace('-', "_")),
        format!("{ENV_PREFIX}_{target}"),
    ]
}

/// Where a downloaded server for `target` is cached.
///
/// `<cache>/captain-miao/servers/<version>/<target>/miao-server` — the same
/// `<target>/miao-server` layout `cargo xtask prepare-servers --out <dir>`
/// writes, so the directory env var and this cache are the same shape and a
/// developer can point one straight at the other.
pub(crate) fn cache_path_for(target: &str) -> Option<std::path::PathBuf> {
    Some(
        dirs::cache_dir()?
            .join(CACHE_REL)
            .join(env!("CARGO_PKG_VERSION"))
            .join(target)
            .join("miao-server"),
    )
}

/// Hex sha256 of a file's contents — the identity a marker records.
fn digest_of(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        }))
}

/// Every server we can supply **locally** for a host, in preference order.
///
/// The chain, per target, is:
///
/// ```text
/// 1. $CAPTAIN_MIAO_SERVER_<TARGET>                 one exact binary, by target
/// 2. $CAPTAIN_MIAO_SERVER_DIR/<target>/miao-server a directory of them
/// 3. embedded payload                              the offline guarantee
/// 4. <cache>/captain-miao/servers/<ver>/<target>/miao-server
/// ```
///
/// Explicit configuration beats a build-time default, so the env vars come
/// first — and (1) overrides (2) rather than sitting beside it, so you can point
/// at a whole directory and still redirect *one* target out of it. Embedded
/// beats the cache because it is the only source that works with no network and
/// no prior state; that property is the entire reason to keep embedding once a
/// downloader exists, and demoting it for freshness would trade an offline
/// guarantee for a marginal win.
///
/// **Downloading is deliberately not in here.** It is source (5) in the design,
/// but a payload that only the network could supply has no digest until it has
/// been fetched, so it cannot be compared against the host's marker and must not
/// influence the decision. The caller escalates to it only when everything here
/// is exhausted or refused; the download then writes into (4), and re-resolving
/// picks it up with a real digest.
///
/// Staleness is not validated here. The deploy stages the binary, runs it on the
/// host and refuses a mismatch — strictly better than any local inspection,
/// since it catches wrong arch, wrong libc, truncated transfer and wrong version
/// in one step, on the machine that matters.
pub(crate) fn resolve_candidates(uname_sm: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for target in target_candidates(uname_sm) {
        if let Some(c) = resolve_target(target) {
            out.push(c);
        }
    }
    out
}

/// The chain for a single target. Separated so the ordering is readable, and
/// reachable on its own because the hosts-panel upgrade already knows which
/// target it is deploying — it has no `uname` to re-derive one from.
pub(crate) fn resolve_target(target: &str) -> Option<Candidate> {
    let from_file = |path: std::path::PathBuf, source: PayloadSource| -> Option<Candidate> {
        if !path.is_file() {
            // Say so for a variable the user *set*: silently falling through to
            // the embedded payload is precisely the "a variable that does
            // nothing" failure this module accepts two spellings to avoid, and
            // it is worse here — the deploy appears to work, with the wrong
            // binary. A missing directory entry is ordinary (that is how the
            // per-target override coexists with a partial farm), so only the
            // explicitly-named file is worth warning about.
            if matches!(source, PayloadSource::EnvTarget) {
                tracing::warn!(
                    target: "captain_miao::provision",
                    "{ENV_PREFIX}_… names {}, which is not a file — ignoring it",
                    path.display()
                );
            }
            return None;
        }
        match digest_of(&path) {
            Ok(sha256) => Some(Candidate {
                target: target.to_string(),
                sha256,
                source,
                path: Some(path),
                embedded: None,
            }),
            Err(e) => {
                tracing::warn!(
                    target: "captain_miao::provision",
                    "cannot read server payload {}: {e}", path.display()
                );
                None
            }
        }
    };

    // (1) A binary named outright, by target.
    for name in env_names_for(target) {
        if let Some(v) = std::env::var_os(&name)
            && let Some(c) = from_file(v.into(), PayloadSource::EnvTarget)
        {
            return Some(c);
        }
    }
    // (2) A directory of them, laid out as `prepare-servers --out` writes.
    if let Some(dir) = std::env::var_os(format!("{ENV_PREFIX}_DIR"))
        && let Some(c) = from_file(
            std::path::PathBuf::from(dir)
                .join(target)
                .join("miao-server"),
            PayloadSource::EnvDir,
        )
    {
        return Some(c);
    }
    // (3) What this build carries — the offline guarantee.
    if let Some(i) = payloads().iter().position(|p| p.target == target) {
        return Some(Candidate {
            target: target.to_string(),
            sha256: payloads()[i].sha256.to_string(),
            source: PayloadSource::Embedded,
            path: None,
            embedded: Some(i),
        });
    }
    // (4) Something we downloaded earlier.
    from_file(cache_path_for(target)?, PayloadSource::Cache)
}

/// What this build carries.
///
/// Test-only now: the hosts panel's diagnosis reports what the *source chain*
/// could supply instead, which stopped being the same question once env vars,
/// the cache and the downloader joined it. `miao --version` uses [`describe`].
#[cfg(test)]
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
            format!("{:.1} MiB", p.packed.len() as f64 / (1u64 << 20) as f64),
            &p.sha256[..12.min(p.sha256.len())],
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Interpreter check
// ---------------------------------------------------------------------------

/// The only two dynamic loaders a portable Linux payload may ask for.
const GENERIC_INTERPS: &[&str] = &["/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"];

/// Refuse a locally-sourced payload that can only run on the machine that built
/// it.
///
/// The realistic mistake this catches: pointing `CAPTAIN_MIAO_SERVER_DIR` (or
/// the per-target variable) at a `symlinkJoin` that happens to contain
/// `packages.captain-miao-server` — the *native* nix build, linked against the
/// store's glibc with an absolute `/nix/store/…/ld-linux-x86-64.so.2`
/// interpreter. Filed under `x86_64-unknown-linux-gnu` it looks entirely
/// correct and fails on every non-Nix host: the inverse of the failure that
/// started all this, and just as confusing.
///
/// Same shape as `xtask`'s `verify_arch`, which already reads `e_machine` off
/// the ELF header for this class of mistake — one more header field, one more
/// refusal, raised on the machine that can explain it rather than after a
/// multi-megabyte upload.
///
/// **A static musl binary has no `PT_INTERP` at all, and that is the correct
/// answer for it, not a missing one** — so absence passes. The host-run check
/// remains the backstop for everything a local read cannot see (glibc version,
/// NSS, a truncated transfer); this just stops the plausible-looking local
/// mistake from getting that far. Pure.
pub(crate) fn check_interpreter(bytes: &[u8], target: &str) -> Result<(), String> {
    let Some(interp) = elf_interpreter(bytes) else {
        return Ok(()); // static, or not an ELF we can read — the host decides.
    };
    if GENERIC_INTERPS.contains(&interp.as_str()) {
        return Ok(());
    }
    if interp.starts_with("/nix/store/") {
        return Err(format!(
            "{target}: this is a Nix-store-linked server ({interp}) — it runs only on the \
             machine that built it. That build belongs on its own host's PATH via \
             `programs.captain-miao.server.enable`; a payload for *other* hosts must come \
             from `cargo xtask prepare-servers`"
        ));
    }
    Err(format!(
        "{target}: server wants a non-generic loader ({interp}), so it would not run on a \
         stock host"
    ))
}

/// Read `PT_INTERP` out of a 64-bit ELF image. `None` when there is no such
/// segment (a static binary) or the bytes aren't an ELF we understand. Pure.
fn elf_interpreter(bytes: &[u8]) -> Option<String> {
    // e_ident: magic(4) class(1) data(1) ...; only ELF64 is worth parsing here,
    // since every target we build is 64-bit.
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 {
        return None;
    }
    let le = bytes[5] != 2;
    let u16_at = |o: usize| -> u16 {
        let p = [bytes[o], bytes[o + 1]];
        if le {
            u16::from_le_bytes(p)
        } else {
            u16::from_be_bytes(p)
        }
    };
    let u64_at = |o: usize| -> u64 {
        let mut p = [0u8; 8];
        p.copy_from_slice(&bytes[o..o + 8]);
        if le {
            u64::from_le_bytes(p)
        } else {
            u64::from_be_bytes(p)
        }
    };
    let e_phoff = u64_at(0x20) as usize;
    let e_phentsize = u16_at(0x36) as usize;
    let e_phnum = u16_at(0x38) as usize;
    const PT_INTERP: u32 = 3;
    for i in 0..e_phnum {
        // Every one of these is attacker- or mistake-controlled: `e_phoff` comes
        // straight off a file someone pointed an env var at. `ph + 56` unchecked
        // panics on add-overflow in debug and, in release, wraps to a small
        // number that sails past the bounds check and panics on the slice
        // instead — killing the connection task for that host.
        let ph = e_phoff.checked_add(i.checked_mul(e_phentsize)?)?;
        if ph.checked_add(56)? > bytes.len() {
            return None;
        }
        let p_type = {
            let mut p = [0u8; 4];
            p.copy_from_slice(&bytes[ph..ph + 4]);
            if le {
                u32::from_le_bytes(p)
            } else {
                u32::from_be_bytes(p)
            }
        };
        if p_type != PT_INTERP {
            continue;
        }
        let off = u64_at(ph + 0x08) as usize;
        let size = u64_at(ph + 0x20) as usize;
        let end = off.checked_add(size)?;
        let seg = bytes.get(off..end.min(bytes.len()))?;
        let s = seg.split(|b| *b == 0).next()?;
        return Some(String::from_utf8_lossy(s).into_owned());
    }
    None
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
                packed: b"",
            },
            ServerPayload {
                target: "x86_64-unknown-linux-gnu",
                sha256: "g",
                packed: b"",
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
    fn glibc_is_offered_before_musl() {
        // Both are offered — the host has the last word — but glibc is tried
        // first, since a static build cannot see LDAP/SSSD users and its
        // sessions would fail to attach on a host that has them.
        let c = target_candidates("Linux x86_64");
        assert_eq!(c[0], "x86_64-unknown-linux-gnu");
        assert_eq!(c[1], "x86_64-unknown-linux-musl");
    }

    /// A minimal ELF64 image carrying one `PT_INTERP` segment, or none at all.
    /// Synthesized rather than read off disk so the test says the same thing on
    /// a Nix machine (where *every* binary is store-linked) as on a stock one.
    fn elf_with_interp(interp: Option<&str>) -> Vec<u8> {
        let mut v = vec![0u8; 120];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELF64
        v[5] = 1; // little-endian
        v[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        v[0x36..0x38].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        let Some(interp) = interp else {
            v[0x38..0x3a].copy_from_slice(&0u16.to_le_bytes()); // e_phnum = 0
            return v;
        };
        v[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        let ph = 64;
        v[ph..ph + 4].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
        v[ph + 0x08..ph + 0x10].copy_from_slice(&120u64.to_le_bytes()); // p_offset
        let bytes = interp.as_bytes();
        v[ph + 0x20..ph + 0x28].copy_from_slice(&((bytes.len() + 1) as u64).to_le_bytes());
        v.extend_from_slice(bytes);
        v.push(0);
        v
    }

    #[test]
    fn a_store_linked_server_is_refused_before_it_is_sent() {
        // The realistic mistake: pointing the env var at a symlinkJoin holding
        // the *native* nix package. Filed under a generic triple it looks
        // entirely correct and runs on exactly one machine.
        let nix = elf_with_interp(Some(
            "/nix/store/57iz36553175g3178pvxjij8z5rcsd4n-glibc-2.42-61/lib/ld-linux-x86-64.so.2",
        ));
        let err = check_interpreter(&nix, "x86_64-unknown-linux-gnu").unwrap_err();
        assert!(err.contains("/nix/store/"), "{err}");
        // It has to point at the answer, not just refuse.
        assert!(err.contains("programs.captain-miao.server.enable"), "{err}");

        // A generic loader is what a portable payload asks for.
        for ok in ["/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"] {
            assert!(
                check_interpreter(&elf_with_interp(Some(ok)), "t").is_ok(),
                "{ok} should pass"
            );
        }

        // Static musl has NO PT_INTERP, and that is the correct answer for it
        // rather than a missing one — refusing it here would reject the very
        // payload that reaches a no-loader host.
        assert!(check_interpreter(&elf_with_interp(None), "t").is_ok());

        // Anything else is named rather than guessed at.
        let odd = check_interpreter(&elf_with_interp(Some("/opt/weird/ld.so")), "t").unwrap_err();
        assert!(odd.contains("/opt/weird/ld.so"), "{odd}");

        // Not an ELF we can read → let the host decide; it is the backstop.
        assert!(check_interpreter(b"not an elf at all", "t").is_ok());

        // Malformed headers must return, never panic: every offset here comes
        // off the file itself, so a hostile or truncated one reaches the parser.
        let mut evil = elf_with_interp(Some("/lib64/ld-linux-x86-64.so.2"));
        evil[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes()); // e_phoff
        evil[0x36..0x38].copy_from_slice(&0u16.to_le_bytes()); // e_phentsize
        evil[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        assert!(check_interpreter(&evil, "t").is_ok());

        // A header claiming more program headers than the file can hold.
        let mut short = elf_with_interp(Some("/lib64/ld-linux-x86-64.so.2"));
        short[0x38..0x3a].copy_from_slice(&u16::MAX.to_le_bytes());
        let _ = check_interpreter(&short, "t");

        // Truncated to the header, and to nothing.
        let _ = check_interpreter(&elf_with_interp(None)[..64], "t");
        assert!(check_interpreter(b"", "t").is_ok());
        assert!(check_interpreter(b"\x7fELF", "t").is_ok());
    }

    #[test]
    fn both_spellings_of_the_per_target_variable_are_accepted() {
        // A variable that silently does nothing is the worst failure mode here,
        // and someone who has just typed a target triple will try it verbatim.
        let names = env_names_for("x86_64-unknown-linux-musl");
        assert_eq!(names[0], "CAPTAIN_MIAO_SERVER_X86_64_UNKNOWN_LINUX_MUSL");
        assert_eq!(names[1], "CAPTAIN_MIAO_SERVER_x86_64-unknown-linux-musl");
    }

    #[test]
    fn the_cache_layout_matches_what_prepare_servers_writes() {
        // `<dir>/<target>/miao-server` is not invented here — it is exactly what
        // `cargo xtask prepare-servers --out <dir>` produces, so the directory
        // env var and this cache are the same shape and one can be pointed at
        // the other.
        let p = cache_path_for("aarch64-unknown-linux-musl").expect("a cache dir");
        let s = p.to_string_lossy();
        assert!(s.ends_with("aarch64-unknown-linux-musl/miao-server"), "{s}");
        assert!(s.contains("captain-miao/servers"), "{s}");
        // Versioned: servers are only interchangeable within a version.
        assert!(s.contains(env!("CARGO_PKG_VERSION")), "{s}");
    }

    /// `xz -6` of `b"\x7fELF not really, but it compresses".repeat(64)`,
    /// produced by liblzma — the encoder `xtask` packs with.
    ///
    /// A literal rather than something the test compresses itself, because the
    /// dashboard has no encoder: `lzma-rs` ships decode-only (its `xz_compress`
    /// does not compress), and pulling liblzma in as a dev-dependency would put a
    /// C build in the way of `cargo test` on all four cross targets. The
    /// end-to-end guard against a preset or filter change that this decoder
    /// cannot read lives in `xtask`, where both codecs are available.
    const XZ6_FIXTURE: &[u8] = b"\xfd\x37\x7a\x58\x5a\x00\x00\x04\xe6\xd6\xb4\x46\x04\xc0\x3b\x80\
\x11\x21\x01\x16\x00\x00\x00\x00\x00\x00\x00\x00\x1b\x77\xc5\x18\
\xe0\x08\x7f\x00\x33\x5d\x00\x3f\x91\x45\x84\x69\x4d\x9b\xc1\xaa\
\x27\x31\xba\xc1\x4c\x13\x09\x59\x13\x78\xac\x90\xa7\xef\x8b\xe1\
\xba\x5d\x0c\x43\xd2\x99\x4b\xd2\x9f\xbf\xcd\x4b\x8b\x98\xb5\x0e\
\x9f\xb1\xc0\xc3\xb1\x9a\xef\x71\x19\x40\x00\x00\x36\x56\x1c\x14\
\x5e\x76\x8d\x0b\x00\x01\x57\x80\x11\x00\x00\x00\xfa\x6c\x9d\x8b\
\xb1\xc4\x67\xfb\x02\x00\x00\x00\x00\x04\x59\x5a";

    #[test]
    fn a_payload_round_trips_through_xz() {
        // Fixture bytes compressed by `lzma-rs` would prove nothing: its encoder
        // does not actually compress, and the streams this has to read are
        // liblzma's. So this is a real `xz -6` stream, produced by liblzma and
        // committed as bytes, decoded by the pure-Rust decoder that ships.
        let raw = b"\x7fELF not really, but it compresses".repeat(64);
        let payload = ServerPayload {
            target: "t",
            sha256: "s",
            packed: XZ6_FIXTURE,
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
            assert!(!p.packed.is_empty(), "{}: empty payload", p.target);
            // xz magic: FD 37 7A 58 5A 00.
            assert_eq!(
                &p.packed[..6],
                b"\xfd7zXZ\x00",
                "{}: not an xz stream",
                p.target
            );
            // Actually unpack it, and check the bytes are the ones the digest
            // names. Magic alone would still pass for a stream this decoder
            // cannot read — a filter it lacks, or a truncated embed — and the
            // first place that would otherwise surface is a failed deploy on
            // someone else's machine. The digest is of the *decompressed* binary
            // (it becomes the marker on the host), so this checks the whole
            // chain: manifest, `include_bytes!`, codec, and identity.
            let raw = p
                .decompress()
                .unwrap_or_else(|e| panic!("{}: does not unpack: {e}", p.target));
            use sha2::Digest as _;
            let got: String = sha2::Sha256::digest(&raw)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(got, p.sha256, "{}: unpacks to the wrong bytes", p.target);
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
