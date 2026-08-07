//! Cross-builds `miao-server`, compresses it, and writes it into a
//! linked dashboard — the thing that dashboard then deploys to a remote host
//! that hasn't got a server (`docs/crate-split.md`).
//!
//! Driven by `cargo xtask dist`, which runs both halves in one command, so the
//! server a dashboard carries is always compiled from the sources beside it.
//! That alignment has to be arranged rather than assumed: the workspace version
//! is the only thing a released artifact is keyed on, and it does not move
//! between dev builds, so nothing else could tell a current server from an old
//! one.
//!
//! Cargo is left in charge of *whether* a rebuild is needed: [`build`] always
//! invokes it, and cargo does nothing when nothing changed. Only the packing
//! step is memoised here, keyed on the digest of the binary cargo produced,
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

/// One built, packed payload.
pub struct Payload {
    pub target: String,
    /// Digest of the **uncompressed** binary — what the deploy writes to the
    /// host as its marker, so it identifies the build rather than the archive.
    pub sha256: String,
    /// The gzip the dashboard `include_bytes!`es.
    pub gz_path: PathBuf,
    pub raw_len: u64,
    pub gz_len: u64,
    pub strategy: Strategy,
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

/// Build `SERVER_PKG` for `target` and return the packed payload.
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

    // A cross that silently produced a host binary would upload cleanly and then
    // fail to exec on the remote, which is a confusing place to learn about it.
    // The ELF header says which arch it is without running anything.
    if let Some(expected) = expected_elf_machine(target) {
        let found = elf_machine(&raw)
            .ok_or_else(|| format!("{} is not an ELF binary", artifact.display()))?;
        if found != expected {
            return Err(format!(
                "{} is ELF e_machine {found:#06x}, expected {expected:#06x} for {target} \
                 (the build fell back to the host arch)",
                artifact.display()
            ));
        }
    }

    let sha256 = hex(&Sha256::digest(&raw));
    let (gz_path, gz_len, repacked) = pack(build_dir, target, &raw, &sha256)?;

    Ok(Payload {
        target: target.to_string(),
        sha256,
        gz_path,
        raw_len: raw.len() as u64,
        gz_len,
        strategy,
        repacked,
    })
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

    // Rewriting in place keeps the existing mode, so the executable bit survives.
    std::fs::write(binary, &bin).map_err(|e| format!("writing {}: {e}", binary.display()))?;
    resign(binary)?;

    Ok(Injected {
        used: body.len(),
        capacity: slot.capacity,
    })
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
