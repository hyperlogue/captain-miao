//! Turns a `miao-server` binary into a packed payload, and writes packed
//! payloads into a linked dashboard — the thing that dashboard then deploys to a
//! remote host that hasn't got a server (`docs/crate-split.md`).
//!
//! **Obtaining a server and building the dashboard are separate concerns**, and
//! this module keeps them that way. Three sources produce the same [`Payload`]:
//! [`build`] cross-compiles one from this workspace, [`from_file`] takes one
//! somebody already has, and [`fetch`] downloads one from a published release.
//! Everything downstream — packing, sizing, injection — is written once against
//! the result and never learns which it was.
//!
//! That seam is the reason the payload is written in *after* linking rather than
//! compiled in. A compiled-in payload is an input to the dashboard's build, so
//! changing where servers come from means recompiling the dashboard; an injected
//! one does not, so `cargo xtask bundle` can put a fetched server into a `cm`
//! that is already built (or already shipped).
//!
//! Where a payload came from still has to be *recorded*, because the workspace
//! version is the only thing a released artifact is keyed on and it does not move
//! between dev builds — so a version match cannot tell one `0.2.1` server from
//! another. [`Provenance`] names the source for the human, and the sha256 every
//! payload carries is what tells the builds apart on the wire.
//!
//! Cargo is left in charge of *whether* a rebuild is needed: [`build`] always
//! invokes it, and cargo does nothing when nothing changed. Only the packing
//! step is memoised here, keyed on the digest of the binary that arrived,
//! because gzipping ~9 MB at maximum compression is the one part that would
//! otherwise cost real time on a no-op run.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use crate::format;

/// The cargo package we build. Distinct from [`SERVER_BIN`] since the rename:
/// `cargo -p` still wants the package name, everything downstream wants the file.
const SERVER_PKG: &str = "captain-miao-server";

/// The binary that package produces — the artifact filename, the tar member, and
/// the release asset stem. Anything naming the *file* uses this, not the package.
const SERVER_BIN: &str = "miao-server";

/// The glibc floor `cargo-zigbuild` links `*-linux-gnu` targets against.
///
/// 2.28 is Debian 10 / RHEL 8 / Ubuntu 18.10, which covers every server distro
/// still receiving updates, and it is deliberately *lower* than the dashboard's
/// own floor (2.35, from release CI's ubuntu-22.04 runners): the dashboard runs
/// on a laptop you keep current, the server runs on whatever the fleet is.
/// Picking glibc over musl is the ruling in `docs/crate-split.md` — musl's static
/// NSS drops LDAP/SSSD and its utmp is stubbed, both of which mainstream server
/// fleets actually use.
pub const GLIBC_FLOOR: &str = "2.28";

/// Where [`fetch`] looks for a published server, minus the tag and filename.
///
/// Overridable so a fork, a private mirror, or a test can be pointed somewhere
/// else without a code change — the URL shape is the contract, not the host.
pub const RELEASE_BASE: &str = "https://github.com/hyperlogue/captain-miao/releases/download";

/// Where a payload came from. Recorded rather than inferred: the three sources
/// are indistinguishable by the time the bytes are packed, and "which server is
/// this" is the question a bundled dashboard exists to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// Cross-compiled from this workspace by [`build`].
    Built(Strategy),
    /// A binary the caller already had, taken as-is by [`from_file`].
    Local,
    /// Downloaded from a published release by [`fetch`].
    Fetched { version: String },
}

impl Provenance {
    pub fn label(&self) -> String {
        match self {
            Provenance::Built(s) => s.label().to_string(),
            Provenance::Local => "local file".to_string(),
            Provenance::Fetched { version } => format!("release v{version}"),
        }
    }

    /// The build strategy, when we were the ones who built it. `None` for a
    /// binary that arrived from elsewhere — we know nothing about how it was
    /// linked, and guessing is what [`unpinned_floor`] must not do.
    pub fn strategy(&self) -> Option<Strategy> {
        match self {
            Provenance::Built(s) => Some(*s),
            _ => None,
        }
    }
}

/// How a given target gets built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Plain `cargo build` — the target is this machine's own.
    Native,
    /// `cargo zigbuild`, which brings its own C cross-compiler and target libc.
    /// The only strategy that handles the server's bundled-SQLite amalgamation
    /// without a distro cross toolchain installed.
    Zigbuild,
    /// `cross`, which runs the build in a per-target container.
    Cross,
}

impl Strategy {
    pub fn label(self) -> &'static str {
        match self {
            Strategy::Native => "native",
            Strategy::Zigbuild => "zigbuild",
            Strategy::Cross => "cross",
        }
    }
}

/// Which cross-build tools are installed. Split out from the decision so
/// `choose_strategy` stays pure and testable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tools {
    /// Both `cargo-zigbuild` *and* `zig` — zigbuild is a thin driver and is
    /// useless without the compiler it drives.
    zigbuild: bool,
    cross: bool,
}

impl Tools {
    pub fn detect() -> Self {
        Tools {
            zigbuild: on_path("cargo-zigbuild") && on_path("zig"),
            cross: on_path("cross"),
        }
    }
}

fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

/// Pick how to build `target`. Pure — [`Tools`] carries the environment probe.
fn choose_strategy(target: &str, host: &str, tools: &Tools) -> Result<Strategy, String> {
    // Zigbuild first for glibc targets, *including this machine's own*. That
    // looks backwards until you measure it: a native release build on a
    // current-glibc distro links against whatever the builder has (2.39 on the
    // NixOS box this was written on), and that binary dies on the loader of any
    // server older than the builder. Since the payload's entire job is to run on
    // someone else's machine, pinning the floor matters more than avoiding a
    // cross — and zigbuild's output measured GLIBC_2.28 max for both arches
    // against the native build's 2.39.
    if tools.zigbuild && target.ends_with("-linux-gnu") {
        return Ok(Strategy::Zigbuild);
    }
    if target == host {
        return Ok(Strategy::Native);
    }
    if tools.zigbuild {
        return Ok(Strategy::Zigbuild);
    }
    if tools.cross {
        return Ok(Strategy::Cross);
    }
    Err(format!(
        "no way to cross-build for {target} from {host}: install cargo-zigbuild + zig \
         (`nix develop` provides both), or `cross` with a container runtime"
    ))
}

/// Where a strategy's glibc floor actually comes from, when it isn't the pinned
/// [`GLIBC_FLOOR`] — phrased for the warning `build.rs` prints. `None` means the
/// floor is pinned (or the target has no glibc to pin).
///
/// Only zigbuild pins it. The other two are worth naming separately rather than
/// lumping together as "the builder's glibc": `cross` compiles inside a Linux
/// container, so the floor is that image's, not this machine's — and on a macOS
/// host, where `cross` is the fallback for want of zig, this machine has no
/// glibc at all.
pub fn unpinned_floor(strategy: Strategy, target: &str) -> Option<&'static str> {
    if !target.ends_with("-linux-gnu") {
        return None;
    }
    match strategy {
        Strategy::Zigbuild => None,
        Strategy::Native => Some("this machine's glibc"),
        Strategy::Cross => Some("the glibc in cross's container image for this target"),
    }
}

/// The command line a strategy runs. Pure, so the zigbuild glibc suffix is
/// testable without zig installed.
///
/// The build gets its **own** `--target-dir` so cross artifacts for several
/// targets accumulate beside each other without disturbing the workspace's
/// ordinary `target/release`, which the dashboard build owns.
fn build_argv(strategy: Strategy, target: &str, build_dir: &Path) -> (String, Vec<String>) {
    let argv = |verb: &str, tgt: &str| {
        vec![
            verb.to_string(),
            "--release".to_string(),
            "--locked".to_string(),
            "-p".to_string(),
            SERVER_PKG.to_string(),
            "--target".to_string(),
            tgt.to_string(),
            "--target-dir".to_string(),
            build_dir.display().to_string(),
        ]
    };
    match strategy {
        Strategy::Native => (cargo(), argv("build", target)),
        Strategy::Zigbuild => (cargo(), argv("zigbuild", &zig_target(target))),
        Strategy::Cross => ("cross".to_string(), argv("build", target)),
    }
}

/// The cargo that invoked us, so a nested build stays on one toolchain.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// zigbuild's target spelling: a `*-linux-gnu` triple takes a `.<glibc>` suffix
/// naming the floor to link against. cargo-zigbuild strips the suffix before
/// handing the triple to cargo, so the artifact still lands under the bare
/// triple. Pure.
fn zig_target(target: &str) -> String {
    if target.ends_with("-linux-gnu") {
        format!("{target}.{GLIBC_FLOOR}")
    } else {
        target.to_string()
    }
}

/// The ELF `e_machine` a target must have, for the arch cross-check. `None` for
/// non-ELF targets (Darwin), where the check is skipped.
fn expected_elf_machine(target: &str) -> Option<u16> {
    if !target.contains("-linux-") {
        return None;
    }
    match target.split('-').next()? {
        "x86_64" => Some(0x3e),
        "aarch64" => Some(0xb7),
        _ => None,
    }
}

/// Read `e_machine` out of an ELF header, honouring `EI_DATA` for endianness.
/// `None` if the bytes aren't an ELF image at all. Pure.
fn elf_machine(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 20 || &bytes[..4] != b"\x7fELF" {
        return None;
    }
    let pair = [bytes[18], bytes[19]];
    Some(match bytes[5] {
        2 => u16::from_be_bytes(pair),
        _ => u16::from_le_bytes(pair),
    })
}

/// One packed payload, whatever produced it.
pub struct Payload {
    pub target: String,
    /// Digest of the **uncompressed** binary — what the deploy writes to the
    /// host as its marker, so it identifies the build rather than the archive.
    pub sha256: String,
    /// The uncompressed binary this was packed from. Kept because obtaining a
    /// server is now a step in its own right: `cargo xtask server` publishes
    /// these, and the archive is no use to anything but the injector.
    pub bin_path: PathBuf,
    /// The gzip the injector writes into the dashboard's slot.
    pub gz_path: PathBuf,
    pub raw_len: u64,
    pub gz_len: u64,
    pub provenance: Provenance,
    /// False when the binary was byte-identical to the last one packed and the
    /// existing archive was reused.
    pub repacked: bool,
}

/// This machine's target triple, from `rustc -vV`.
pub fn host_triple() -> Result<String, String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|e| format!("running `rustc -vV`: {e}"))?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|h| h.trim().to_string())
        .ok_or_else(|| "`rustc -vV` printed no host line".to_string())
}

/// Cross-build `SERVER_PKG` for `target` and pack the `SERVER_BIN` it produces.
///
/// `root` is the workspace root, `build_dir` a directory this owns entirely.
pub fn build(
    root: &Path,
    build_dir: &Path,
    target: &str,
    host: &str,
    tools: &Tools,
) -> Result<Payload, String> {
    let strategy = choose_strategy(target, host, tools)?;
    let (program, args) = build_argv(strategy, target, build_dir);

    println!("  {} {}", program, args.join(" "));
    run(root, &program, &args)?;

    let artifact = build_dir.join(target).join("release").join(SERVER_BIN);
    let raw = std::fs::read(&artifact).map_err(|e| {
        format!(
            "reading {} after a successful build: {e}",
            artifact.display()
        )
    })?;
    verify_arch(&raw, target, &artifact.display().to_string())?;
    pack_binary(
        build_dir,
        target,
        &artifact,
        &raw,
        Provenance::Built(strategy),
    )
}

/// Pack a server binary the caller already has.
///
/// The escape hatch for anything the other two sources don't cover — a binary
/// built by some other pipeline, one pulled from an internal artifact store, or
/// the ones a CI job just downloaded. Nothing is assumed about how it was
/// produced beyond it being for `target`, which the arch check confirms.
pub fn from_file(build_dir: &Path, target: &str, path: &Path) -> Result<Payload, String> {
    let raw = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    verify_arch(&raw, target, &path.display().to_string())?;
    pack_binary(build_dir, target, path, &raw, Provenance::Local)
}

/// Download a published `miao-server` for `target` and pack it.
///
/// The point of this source is that a bundled dashboard no longer needs a cross
/// toolchain — or the server's sources — to be built. It needs `curl` and `tar`,
/// which every macOS and Linux box has, and a release that published servers.
///
/// The bytes are **not** checksummed against an expected digest, because there
/// is nothing to check one against: the URL is the assertion. What guards the
/// far end instead is the deploy path, which stages the binary on the host and
/// runs it there before moving it into place, so a wrong-ABI or truncated
/// payload fails on the host rather than becoming the server it invokes.
pub fn fetch(build_dir: &Path, target: &str, version: &str, base: &str) -> Result<Payload, String> {
    let version = version.trim().trim_start_matches('v');
    let url = release_url(base, version, target);

    // A fresh directory per fetch: `tar` extracts over whatever is there, so a
    // failed download followed by a successful extract of the *previous* archive
    // would silently pack a stale binary.
    let dir = build_dir.join("fetched").join(target);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let tgz = dir.join("server.tar.gz");

    println!("  curl {url}");
    run_quiet(
        "curl",
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            // Redirects are followed, so pin the scheme on every hop rather than
            // trusting the one in the URL we started from.
            "--proto",
            "=https",
            "--tlsv1.2",
            "--output",
            &tgz.display().to_string(),
            &url,
        ],
    )?;

    let (bin, raw) = unpack_server(&tgz, &dir, target, &url)?;
    pack_binary(
        build_dir,
        target,
        &bin,
        &raw,
        Provenance::Fetched {
            version: version.to_string(),
        },
    )
}

/// Extract the server out of a release archive and read it back.
///
/// Split from [`fetch`] because this half is where the judgement is — what the
/// archive is allowed to contain, and what the bytes must be — while the download
/// is one `curl` invocation. A test can hand this a hostile tarball; it cannot
/// hand `curl` a hostile release.
///
/// `whence` names the archive's origin in errors (a URL, in the one real caller).
fn unpack_server(
    archive: &Path,
    into: &Path,
    target: &str,
    whence: &str,
) -> Result<(PathBuf, Vec<u8>), String> {
    // Naming the member explicitly is the extraction guard: only an entry called
    // exactly `miao-server` comes out, so a `../` path in the archive has
    // nothing to land on. The owner/permission flags are the non-root defaults,
    // stated so a run as root can't restore an archived uid or a setuid bit.
    run_quiet(
        "tar",
        &[
            "-xzf",
            &archive.display().to_string(),
            "-C",
            &into.display().to_string(),
            "--no-same-owner",
            "--no-same-permissions",
            SERVER_BIN,
        ],
    )?;

    let bin = into.join(SERVER_BIN);
    // tar extracts whatever kind of entry the archive names, symlinks included —
    // and reading through one would pack a file from outside the staging dir, or
    // from outside this machine's idea of what we asked for.
    if bin.is_symlink() || !bin.is_file() {
        return Err(format!(
            "{whence} did not yield a regular file at {SERVER_BIN}"
        ));
    }
    let raw = std::fs::read(&bin).map_err(|e| format!("reading {}: {e}", bin.display()))?;
    verify_arch(&raw, target, whence)?;
    Ok((bin, raw))
}

/// The release asset a [`fetch`] pulls. Pure, so the naming contract with the
/// release workflow is pinned by a test rather than by a successful download.
pub fn release_url(base: &str, version: &str, target: &str) -> String {
    let version = version.trim().trim_start_matches('v');
    let base = base.trim_end_matches('/');
    format!("{base}/v{version}/{SERVER_BIN}-v{version}-{target}.tar.gz")
}

/// Digest, compress, and record a server binary — the tail every source shares.
///
/// Split out so the three of them differ only in how the bytes arrive: anything
/// that produces a `miao-server` for `target` becomes a payload here,
/// and nothing downstream can tell which one did.
pub fn pack_binary(
    build_dir: &Path,
    target: &str,
    bin_path: &Path,
    raw: &[u8],
    provenance: Provenance,
) -> Result<Payload, String> {
    let sha256 = hex(&Sha256::digest(raw));
    let (gz_path, gz_len, repacked) = pack(build_dir, target, raw, &sha256)?;
    Ok(Payload {
        target: target.to_string(),
        sha256,
        bin_path: bin_path.to_path_buf(),
        gz_path,
        raw_len: raw.len() as u64,
        gz_len,
        provenance,
        repacked,
    })
}

/// Confirm a binary is for the architecture it claims.
///
/// A cross that silently produced a host binary — or a release asset fetched for
/// the wrong triple — would upload cleanly and then fail to exec on the remote,
/// which is a confusing place to learn about it. The ELF header says which arch
/// it is without running anything. `whence` names the source in the error, since
/// by this point it could be a build product, a path, or a URL.
fn verify_arch(raw: &[u8], target: &str, whence: &str) -> Result<(), String> {
    let Some(expected) = expected_elf_machine(target) else {
        return Ok(());
    };
    let found = elf_machine(raw).ok_or_else(|| format!("{whence} is not an ELF binary"))?;
    if found != expected {
        return Err(format!(
            "{whence} is ELF e_machine {found:#06x}, expected {expected:#06x} for {target} \
             (wrong architecture for this payload)"
        ));
    }
    Ok(())
}

/// What an [`inject`] call put where.
pub struct Injected {
    pub used: usize,
    pub capacity: usize,
}

/// How much slot a set of payloads needs, so a caller can size the reservation
/// before the dashboard is compiled. Pure over the sizes it is given.
pub fn slot_needed(entries: &[(&str, &str, usize)]) -> usize {
    // 4-byte count, then per entry: 2 + 2 + 8 of fixed header, the target and
    // digest strings, and the blob itself.
    entries.iter().fold(4, |n, (target, sha, gz_len)| {
        n + 12 + target.len() + sha.len() + gz_len
    })
}

/// Write `payloads` into an already-linked dashboard binary, in place.
///
/// The file's length never changes: every byte lands inside the reservation the
/// linker placed, which is what lets this survive `strip` and work identically on
/// ELF and Mach-O (see [`crate::format`]).
pub fn inject(binary: &Path, payloads: &[&Payload]) -> Result<Injected, String> {
    let blobs: Vec<Vec<u8>> = payloads
        .iter()
        .map(|p| {
            std::fs::read(&p.gz_path).map_err(|e| format!("reading {}: {e}", p.gz_path.display()))
        })
        .collect::<Result<_, _>>()?;
    let entries: Vec<format::Entry<'_>> = payloads
        .iter()
        .zip(&blobs)
        .map(|(p, gz)| format::Entry {
            target: &p.target,
            sha256: &p.sha256,
            gz,
        })
        .collect();
    let body = format::encode(&entries);

    let mut bin =
        std::fs::read(binary).map_err(|e| format!("reading {}: {e}", binary.display()))?;
    let before = bin.len();
    let slot = format::find(&bin)?;
    format::write(&mut bin, slot, &body)?;
    debug_assert_eq!(before, bin.len(), "injection must not resize the binary");

    write_replacing(binary, &bin)?;
    resign(binary)?;

    Ok(Injected {
        used: body.len(),
        capacity: slot.capacity,
    })
}

/// Replace a file's contents atomically, keeping its mode.
///
/// `cargo xtask bundle` patches a dashboard **in place**, and that dashboard may
/// be the `cm` on someone's PATH — possibly a running one. Truncate-and-write is
/// wrong on both counts: an interrupted write leaves a corrupt binary where a
/// working one was, and on Linux writing to a running executable fails outright
/// with `ETXTBSY`. Staging beside it and renaming over answers both — the swap is
/// atomic, and a process already running keeps the old inode until it exits.
///
/// The temp file is a sibling so the rename stays within one filesystem, and the
/// original's permissions are copied onto it before the swap so the executable
/// bit survives (a fresh file would be created by the umask instead).
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let perms = std::fs::metadata(path)
        .map_err(|e| format!("reading the mode of {}: {e}", path.display()))?
        .permissions();
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let tmp = path.with_file_name(format!(".{}.cm-inject", name.to_string_lossy()));

    let staged = (|| {
        std::fs::write(&tmp, bytes)?;
        std::fs::set_permissions(&tmp, perms)?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = staged {
        // The rename is what publishes it, so anything before that leaves only a
        // dotfile to clean up and the original untouched.
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {}: {e}", path.display()));
    }
    Ok(())
}

/// Re-sign a patched Mach-O.
///
/// Every arm64 macOS binary must carry a valid signature, and the bytes we
/// overwrite are inside the region the signature hashes — so a patched binary is
/// killed on exec until it is signed again. Ad-hoc (`-s -`) is what the toolchain
/// produces by default, so this restores the status quo rather than adding a
/// requirement; a release signed with a real identity has to be signed after
/// bundling regardless. A no-op everywhere else.
fn resign(binary: &Path) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let out = Command::new("codesign")
        .args(["-f", "-s", "-"])
        .arg(binary)
        .output()
        .map_err(|e| format!("running codesign (needed after patching a Mach-O): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "codesign failed on {} ({}); a patched Mach-O will not run unsigned\n{}",
            binary.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(())
}

/// Gzip `raw` into the build directory, reusing the previous archive when the
/// binary hasn't changed. Returns the archive, its size, and whether it was
/// (re)written.
fn pack(
    build_dir: &Path,
    target: &str,
    raw: &[u8],
    sha256: &str,
) -> Result<(PathBuf, u64, bool), String> {
    let dir = build_dir.join("packed");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let gz_path = dir.join(format!("{target}.gz"));
    let stamp = dir.join(format!("{target}.sha256"));

    // The stamp records the digest of the *binary* the archive was made from, so
    // a cache hit means the bytes we are about to embed are the bytes cargo just
    // produced — not merely that some archive exists.
    if std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == sha256)
        && let Ok(meta) = std::fs::metadata(&gz_path)
    {
        return Ok((gz_path, meta.len(), false));
    }

    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    enc.write_all(raw).map_err(|e| format!("gzip: {e}"))?;
    let gz = enc.finish().map_err(|e| format!("gzip: {e}"))?;

    // Write the archive first and the stamp only once it has landed, so an
    // interrupted run leaves a stale-looking cache rather than a stamp promising
    // an archive that isn't there.
    let tmp = dir.join(format!(".{target}.gz.tmp"));
    std::fs::write(&tmp, &gz).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &gz_path).map_err(|e| format!("renaming {}: {e}", tmp.display()))?;
    std::fs::write(&stamp, format!("{sha256}\n"))
        .map_err(|e| format!("writing {}: {e}", stamp.display()))?;

    Ok((gz_path, gz.len() as u64, true))
}

/// Run the nested cargo, with its output going straight to the terminal.
///
/// The environment is inherited except for the flags aimed at the *outer* build:
/// `RUSTFLAGS` meant for the dashboard (`-C target-cpu=native`, a lint level, a
/// custom linker) would be silently wrong for a cross, and `CARGO_BUILD_TARGET`
/// would override the `--target` just computed.
fn run(root: &Path, program: &str, args: &[String]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_TARGET_DIR")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("spawning `{program}`: {e}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!("`{program} {}` failed ({status})", args.join(" ")))
}

/// Run a helper that should say nothing when it works, capturing its output so a
/// failure reports what it actually said.
///
/// `curl` and `tar` are the only two, and both are quiet on success — so
/// inheriting stdio the way [`run`] does would print nothing useful and lose the
/// message that matters. A missing binary is reported as itself rather than as
/// an opaque spawn error, because "no curl on this machine" and "that release
/// has no server for this target" are different problems with different fixes.
fn run_quiet(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program).args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("`{program}` is not installed, and fetching a released server needs it")
        } else {
            format!("spawning `{program}`: {e}")
        }
    })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!(
        "`{program}` failed ({}){}",
        out.status,
        match stderr.trim() {
            "" => String::new(),
            msg => format!(": {msg}"),
        }
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn human(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "x86_64-unknown-linux-gnu";
    const OTHER: &str = "aarch64-unknown-linux-gnu";

    #[test]
    fn the_host_target_falls_back_to_native_without_zig() {
        let none = Tools::default();
        assert_eq!(
            choose_strategy(HOST, HOST, &none).unwrap(),
            Strategy::Native
        );
    }

    /// Only zigbuild pins the floor, and the other two miss it for different
    /// reasons — `cross` builds in a container, so blaming "this machine's
    /// glibc" would be wrong, and on a macOS host there is no such thing.
    #[test]
    fn the_floor_warning_names_where_the_floor_actually_came_from() {
        assert_eq!(unpinned_floor(Strategy::Zigbuild, OTHER), None);
        assert_eq!(
            unpinned_floor(Strategy::Native, HOST),
            Some("this machine's glibc")
        );
        assert!(
            unpinned_floor(Strategy::Cross, OTHER)
                .is_some_and(|s| s.contains("container") && !s.contains("this machine")),
            "cross builds in a container, not against the builder's libc"
        );
        // A target with no glibc has no floor to miss.
        assert_eq!(
            unpinned_floor(Strategy::Native, "aarch64-apple-darwin"),
            None
        );
        assert_eq!(
            unpinned_floor(Strategy::Zigbuild, "x86_64-unknown-linux-musl"),
            None
        );
    }

    /// The macOS case the `cross` fallback exists for: no zig, so a Linux
    /// payload has to come out of a container — and with neither tool installed
    /// the error names both.
    #[test]
    fn a_mac_building_for_linux_falls_back_to_cross_or_says_why_it_cannot() {
        const MAC: &str = "aarch64-apple-darwin";
        let only_cross = Tools {
            zigbuild: false,
            cross: true,
        };
        assert_eq!(
            choose_strategy(OTHER, MAC, &only_cross).unwrap(),
            Strategy::Cross
        );
        let msg = choose_strategy(OTHER, MAC, &Tools::default()).unwrap_err();
        assert!(msg.contains("cargo-zigbuild"), "{msg}");
        assert!(msg.contains("cross"), "{msg}");
    }

    #[test]
    fn zigbuild_wins_even_for_the_host_target_because_of_the_glibc_floor() {
        let zig = Tools {
            zigbuild: true,
            cross: false,
        };
        assert_eq!(
            choose_strategy(HOST, HOST, &zig).unwrap(),
            Strategy::Zigbuild
        );
        // …but only where a floor is the payoff: a non-glibc host target still
        // builds natively rather than routing through zig for nothing.
        assert_eq!(
            choose_strategy("aarch64-apple-darwin", "aarch64-apple-darwin", &zig).unwrap(),
            Strategy::Native
        );
    }

    #[test]
    fn a_cross_prefers_zigbuild_then_cross_then_explains_itself() {
        let both = Tools {
            zigbuild: true,
            cross: true,
        };
        assert_eq!(
            choose_strategy(OTHER, HOST, &both).unwrap(),
            Strategy::Zigbuild
        );
        let only_cross = Tools {
            zigbuild: false,
            cross: true,
        };
        assert_eq!(
            choose_strategy(OTHER, HOST, &only_cross).unwrap(),
            Strategy::Cross
        );
        let msg = choose_strategy(OTHER, HOST, &Tools::default()).unwrap_err();
        assert!(msg.contains("cargo-zigbuild"), "{msg}");
        assert!(msg.contains(OTHER), "{msg}");
    }

    #[test]
    fn only_linux_gnu_targets_take_a_glibc_floor() {
        assert_eq!(zig_target(OTHER), format!("{OTHER}.{GLIBC_FLOOR}"));
        assert_eq!(
            zig_target("x86_64-unknown-linux-musl"),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(zig_target("aarch64-apple-darwin"), "aarch64-apple-darwin");
    }

    #[test]
    fn the_build_argv_matches_the_strategy_and_keeps_its_own_build_dir() {
        let dir = Path::new("/w/target/cm-server-build");

        let (prog, args) = build_argv(Strategy::Zigbuild, OTHER, dir);
        assert!(prog.contains("cargo"));
        assert_eq!(args[0], "zigbuild");
        assert!(args.contains(&format!("{OTHER}.{GLIBC_FLOOR}")));

        let (prog, args) = build_argv(Strategy::Cross, OTHER, dir);
        assert_eq!(prog, "cross");
        assert_eq!(args[0], "build");
        // `cross` runs in its own container and knows nothing about glibc floors.
        assert!(args.contains(&OTHER.to_string()));

        let (_, args) = build_argv(Strategy::Native, HOST, dir);
        assert_eq!(args[0], "build");
        assert!(args.contains(&SERVER_PKG.to_string()));

        // Every strategy builds into the directory it was handed, so cross
        // artifacts never land in the workspace's own `target/release`.
        for s in [Strategy::Native, Strategy::Zigbuild, Strategy::Cross] {
            let (_, args) = build_argv(s, OTHER, dir);
            let at = args.iter().position(|a| a == "--target-dir").unwrap();
            assert_eq!(args[at + 1], dir.display().to_string());
        }
    }

    /// The URL shape is a contract with the release workflow, which names its
    /// server assets by hand. A test is the only thing that keeps the two ends
    /// together without a successful download to prove it.
    #[test]
    fn the_release_url_matches_what_the_workflow_publishes() {
        assert_eq!(
            release_url(RELEASE_BASE, "0.2.1", OTHER),
            "https://github.com/hyperlogue/captain-miao/releases/download/v0.2.1/\
             miao-server-v0.2.1-aarch64-unknown-linux-gnu.tar.gz"
        );
        // A version is accepted in either spelling, so `--server-source
        // release:v0.2.1` and `release:0.2.1` cannot resolve differently.
        assert_eq!(
            release_url(RELEASE_BASE, "v0.2.1", HOST),
            release_url(RELEASE_BASE, "0.2.1", HOST)
        );
        // A mirror is pointed at by base alone, with or without a trailing slash.
        assert_eq!(
            release_url("https://mirror.example/dl/", "0.2.1", HOST),
            release_url("https://mirror.example/dl", "0.2.1", HOST)
        );
        assert!(
            release_url("https://mirror.example/dl", "0.2.1", HOST)
                .starts_with("https://mirror.example/dl/v0.2.1/")
        );
    }

    /// Only a build we performed knows how it was linked. A fetched or
    /// caller-supplied binary must not be described as if we had chosen its
    /// strategy — which is what would happen if the floor warning defaulted.
    #[test]
    fn provenance_reports_a_strategy_only_for_builds_we_ran() {
        assert_eq!(
            Provenance::Built(Strategy::Zigbuild).strategy(),
            Some(Strategy::Zigbuild)
        );
        assert_eq!(Provenance::Local.strategy(), None);
        assert_eq!(
            Provenance::Fetched {
                version: "0.2.1".into()
            }
            .strategy(),
            None
        );
        assert!(
            Provenance::Fetched {
                version: "0.2.1".into()
            }
            .label()
            .contains("0.2.1"),
            "a fetched payload has to say which release it came from"
        );
    }

    /// The arch check is the one thing every source shares, and the reason it
    /// exists is that the failure it catches otherwise surfaces on someone
    /// else's machine.
    #[test]
    fn the_arch_check_names_its_source_and_skips_targets_it_cannot_judge() {
        let mut arm = vec![0u8; 24];
        arm[..4].copy_from_slice(b"\x7fELF");
        arm[5] = 1;
        arm[18] = 0xb7;

        assert!(verify_arch(&arm, OTHER, "x").is_ok());
        let msg = verify_arch(&arm, HOST, "https://example/asset.tar.gz").unwrap_err();
        assert!(msg.contains("https://example/asset.tar.gz"), "{msg}");
        assert!(msg.contains("0x00b7"), "{msg}");

        assert!(verify_arch(b"not an elf", HOST, "y").is_err());
        // Darwin has no ELF header to read, so there is nothing to check.
        assert!(verify_arch(b"not an elf", "aarch64-apple-darwin", "y").is_ok());
    }

    #[test]
    fn elf_machine_reads_both_endiannesses_and_rejects_non_elf() {
        let mut hdr = vec![0u8; 24];
        hdr[..4].copy_from_slice(b"\x7fELF");
        hdr[5] = 1; // little-endian
        hdr[18] = 0xb7;
        assert_eq!(elf_machine(&hdr), Some(0xb7));
        hdr[5] = 2; // big-endian
        hdr[18] = 0x00;
        hdr[19] = 0xb7;
        assert_eq!(elf_machine(&hdr), Some(0xb7));
        assert_eq!(elf_machine(b"MZ\x90\x00 not elf at all"), None);
        assert_eq!(elf_machine(b"\x7fELF"), None);
    }

    #[test]
    fn the_arch_cross_check_covers_the_targets_we_ship_and_skips_darwin() {
        assert_eq!(expected_elf_machine(HOST), Some(0x3e));
        assert_eq!(expected_elf_machine(OTHER), Some(0xb7));
        assert_eq!(expected_elf_machine("aarch64-apple-darwin"), None);
    }

    /// The half of `fetch` that makes decisions, against archives a test can
    /// actually build. What a release asset is allowed to contain is a security
    /// boundary — the bytes come off the network and end up executed on someone
    /// else's server — so the refusals are pinned rather than assumed.
    #[test]
    fn a_release_archive_yields_a_server_or_is_refused() {
        let root = std::env::temp_dir().join(format!("cm-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // An ELF header for OTHER, which is what a real asset would carry.
        let mut server = vec![0u8; 4096];
        server[..4].copy_from_slice(b"\x7fELF");
        server[5] = 1;
        server[18] = 0xb7;

        let tar_up = |name: &str, build: &dyn Fn(&Path)| -> PathBuf {
            let stage = root.join(name).join("stage");
            std::fs::create_dir_all(&stage).unwrap();
            build(&stage);
            let tgz = root.join(name).join("asset.tar.gz");
            let ok = Command::new("tar")
                .args(["-C", &stage.display().to_string(), "-czf"])
                .arg(&tgz)
                .arg(SERVER_BIN)
                .status()
                .expect("tar is required to fetch a released server");
            assert!(ok.success(), "packing the {name} fixture");
            tgz
        };
        let out = |name: &str| {
            let d = root.join(name).join("out");
            std::fs::create_dir_all(&d).unwrap();
            d
        };

        // The happy path: a flat archive holding exactly the binary, which is
        // what the release workflow's `tar -C <dir> … miao-server` makes.
        let good = tar_up("good", &|stage| {
            std::fs::write(stage.join(SERVER_BIN), &server).unwrap()
        });
        let (bin, raw) = unpack_server(&good, &out("good"), OTHER, "u").unwrap();
        assert_eq!(raw, server);
        assert!(bin.is_file());

        // An asset for the wrong architecture. It would upload cleanly and then
        // fail to exec on the remote, which is a terrible place to find out.
        let msg = unpack_server(&good, &out("arch"), HOST, "u").unwrap_err();
        assert!(msg.contains("architecture"), "{msg}");

        // A symlink wearing the member's name. tar extracts it happily; reading
        // through it would pack a file from anywhere on the machine.
        #[cfg(unix)]
        {
            let evil = tar_up("evil", &|stage| {
                std::os::unix::fs::symlink("/etc/passwd", stage.join(SERVER_BIN)).unwrap()
            });
            let msg = unpack_server(&evil, &out("evil"), OTHER, "u").unwrap_err();
            assert!(msg.contains("regular file"), "{msg}");
        }

        // An archive with no such member at all: tar fails, and the failure has
        // to surface rather than leaving an empty directory to read from.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let tgz = empty.join("asset.tar.gz");
        assert!(
            Command::new("tar")
                .args(["-C", &empty.display().to_string(), "-czf"])
                .arg(&tgz)
                .arg("--files-from")
                .arg("/dev/null")
                .status()
                .unwrap()
                .success()
        );
        assert!(unpack_server(&tgz, &out("empty"), OTHER, "u").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Packing is the one memoised step, so its cache has to key on the binary's
    /// content — not on the archive merely existing.
    #[test]
    fn packing_is_reused_only_while_the_binary_is_unchanged() {
        let dir = std::env::temp_dir().join(format!("cm-payload-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let raw = b"\x7fELF and then some payload bytes".repeat(64);
        let sha = hex(&Sha256::digest(&raw));
        let (path, len, repacked) = pack(&dir, "t", &raw, &sha).unwrap();
        assert!(repacked, "first pack must write");
        assert!(len > 0);

        let (_, _, again) = pack(&dir, "t", &raw, &sha).unwrap();
        assert!(!again, "an unchanged binary must reuse its archive");

        let other = b"a different server build entirely".repeat(64);
        let other_sha = hex(&Sha256::digest(&other));
        let (_, _, changed) = pack(&dir, "t", &other, &other_sha).unwrap();
        assert!(changed, "a new binary must be repacked");

        // A vanished archive must not be trusted just because the stamp agrees.
        std::fs::remove_file(&path).unwrap();
        let (_, _, refilled) = pack(&dir, "t", &other, &other_sha).unwrap();
        assert!(refilled);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
