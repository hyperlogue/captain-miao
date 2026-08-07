//! `cargo xtask` — captain-miao's build chores.
//!
//! Its one job is **building the named dashboard variants**: the plain `cm`, and
//! the ones that carry a `miao-server` to deploy to a remote host
//! (`docs/crate-split.md`, "embed + auto-deploy").
//!
//! A bundled variant is four steps in one command: cross-build the servers,
//! measure what they need, compile a dashboard reserving exactly that, and write
//! them into the linked binary. Doing it in that order is what keeps the server
//! and the dashboard in step — there is no staged artifact that can be older than
//! the binary carrying it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

/// The dashboard package, and the binary it installs (which is `miao`, not the
/// package name — see the `[[bin]]` note in the root Cargo.toml).
const DASHBOARD_PKG: &str = "captain-miao";
const DASHBOARD_BIN: &str = "miao";

/// The one feature that reserves slot space. Which *servers* end up in it is
/// decided when they're injected, not when the dashboard is compiled.
const BUNDLE_FEATURE: &str = "bundle";

/// Round the reservation up to this, so variants needing similar amounts share a
/// dashboard compile instead of each forcing its own. The slack is free where it
/// matters: a run of identical filler bytes costs ~5 KB in a gzipped release
/// tarball, measured, so this trades on-disk footprint for build time.
const RESERVE_GRANULARITY: usize = 1 << 20;

/// Headroom on top of the rounded figure, so a server that grows slightly
/// between the measurement and the next release doesn't overflow a slot sized to
/// the byte.
const RESERVE_HEADROOM: usize = 1 << 20;

#[derive(Parser)]
#[command(name = "xtask", about = "captain-miao build chores", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build release dashboard variants into `dist/`.
    Dist(DistArgs),
}

/// A named dashboard build: which servers it carries, and the file it lands on.
///
/// The dashboard isn't one artifact. A laptop driving a Linux fleet wants the
/// server binaries in the box; someone running captain-miao purely locally —
/// most users — should not pay ~5 MB for a code path they never reach.
struct Variant {
    /// Suffix of the artifact in `dist/`; the plain build has none.
    name: &'static str,
    /// Extra cargo features, beyond the bundle feature implied by `servers`.
    features: &'static [&'static str],
    /// Server targets to cross-build and inject.
    servers: &'static [&'static str],
    what: &'static str,
}

const X86: &str = "x86_64-unknown-linux-gnu";
const ARM: &str = "aarch64-unknown-linux-gnu";

const VARIANTS: &[Variant] = &[
    Variant {
        name: "",
        features: &[],
        servers: &[],
        what: "local sessions only — the default, and what ships today",
    },
    Variant {
        name: "remote",
        features: &["remote"],
        servers: &[],
        what: "remote hosts, which must already have miao-server",
    },
    Variant {
        name: "bundle-linux-x86_64",
        features: &[],
        servers: &[X86],
        what: "remote hosts, deploying its own server to x86-64 Linux",
    },
    Variant {
        name: "bundle-linux-aarch64",
        features: &[],
        servers: &[ARM],
        what: "remote hosts, deploying its own server to arm64 Linux",
    },
    Variant {
        name: "bundle-linux",
        features: &[],
        servers: &[X86, ARM],
        what: "remote hosts, deploying its own server to either Linux arch",
    },
];

/// What `dist` builds when asked for nothing in particular: the two ends of the
/// range, which is what a release would carry.
const DEFAULT_VARIANTS: &[&str] = &["", "bundle-linux"];

impl Variant {
    /// How the variant is named on the command line and in output. The plain
    /// build has an empty name, which reads badly in a list.
    fn label(&self) -> &str {
        if self.name.is_empty() {
            "(plain)"
        } else {
            self.name
        }
    }

    fn bundles(&self) -> bool {
        !self.servers.is_empty()
    }

    /// The features this variant compiles with.
    fn cargo_features(&self) -> Vec<&'static str> {
        let mut f = self.features.to_vec();
        if self.bundles() {
            f.push(BUNDLE_FEATURE);
        }
        f
    }

    /// The file it lands on in `dist/`.
    fn artifact(&self) -> String {
        if self.name.is_empty() {
            DASHBOARD_BIN.to_string()
        } else {
            format!("{DASHBOARD_BIN}-{}", self.name)
        }
    }
}

#[derive(Args)]
struct DistArgs {
    /// Variant to build (repeatable). Defaults to the plain dashboard plus
    /// `bundle-linux`. Use `--list` to see them all.
    #[arg(long = "variant", value_name = "NAME")]
    variants: Vec<String>,
    /// Build every variant.
    #[arg(long, conflicts_with = "variants")]
    all: bool,
    /// Print the variants and exit.
    #[arg(long)]
    list: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = workspace_root()?;
    let ws = Workspace {
        target_dir: target_dir(&root),
        root,
    };
    match cli.command {
        Cmd::Dist(args) => dist(&ws, &args),
    }
}

/// The two directories `dist` needs, resolved once.
struct Workspace {
    root: PathBuf,
    target_dir: PathBuf,
}

impl Workspace {
    /// Where the cross-built servers and their archives live. Under `target/`
    /// because that is what they are — build products, disposed by `cargo clean`.
    fn server_build_dir(&self) -> PathBuf {
        self.target_dir.join("cm-server-build")
    }
}

/// Cargo's target directory.
fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

/// The workspace root: xtask's own manifest dir, one level up. Resolved from a
/// compile-time constant so `cargo xtask` works from any subdirectory.
fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("xtask manifest dir has no parent"))
}

/// Build the requested dashboard variants into `dist/`.
fn dist(ws: &Workspace, args: &DistArgs) -> Result<()> {
    if args.list {
        list();
        return Ok(());
    }

    let wanted: Vec<&Variant> = if args.all {
        VARIANTS.iter().collect()
    } else {
        let names: Vec<&str> = if args.variants.is_empty() {
            DEFAULT_VARIANTS.to_vec()
        } else {
            args.variants.iter().map(String::as_str).collect()
        };
        names
            .iter()
            .map(|n| find_variant(n))
            .collect::<Result<_>>()?
    };

    // Build each server once even when several variants want it: the cross is by
    // far the slowest step here.
    let needed: BTreeSet<&str> = wanted
        .iter()
        .flat_map(|v| v.servers.iter().copied())
        .collect();
    let servers = build_servers(ws, &needed)?;

    let dist_dir = ws.root.join("dist");
    std::fs::create_dir_all(&dist_dir)
        .with_context(|| format!("creating {}", dist_dir.display()))?;

    let mut built = Vec::new();
    for v in &wanted {
        println!("\n▶ {}", v.label());
        let payloads: Vec<&cm_payload::Payload> = v
            .servers
            .iter()
            .map(|t| {
                servers
                    .iter()
                    .find(|p| p.target == *t)
                    .ok_or_else(|| anyhow!("no server built for {t}"))
            })
            .collect::<Result<_>>()?;

        let reserve = reserve_for(&payloads);
        build_dashboard(ws, v, reserve)?;

        let to = dist_dir.join(v.artifact());
        // Copy rather than rename: every variant's build reuses the same
        // `target/release/cm`, so leaving it in place would be a lie the moment
        // the next one overwrote it.
        let from = ws.target_dir.join("release").join(DASHBOARD_BIN);
        std::fs::copy(&from, &to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;

        if v.bundles() {
            let done = cm_payload::inject(&to, &payloads).map_err(|e| anyhow!("{e}"))?;
            println!(
                "  injected {} into a {} slot ({}% used)",
                cm_payload::human(done.used as u64),
                cm_payload::human(done.capacity as u64),
                done.used * 100 / done.capacity.max(1),
            );
        }
        verify(&to, v)?;
        built.push((v.artifact(), std::fs::metadata(&to).map(|m| m.len())?));
    }

    println!("\n{}:", dist_dir.display());
    for (name, size) in &built {
        println!("  {name:<30} {}", cm_payload::human(*size));
    }
    Ok(())
}

/// Cross-build every server the run needs.
fn build_servers(ws: &Workspace, targets: &BTreeSet<&str>) -> Result<Vec<cm_payload::Payload>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let host = cm_payload::host_triple().map_err(|e| anyhow!("{e}"))?;
    let tools = cm_payload::Tools::detect();
    let dir = ws.server_build_dir();

    let mut out = Vec::new();
    for target in targets {
        println!("▶ miao-server for {target}");
        let p =
            cm_payload::build(&ws.root, &dir, target, &host, &tools).map_err(|e| anyhow!("{e}"))?;
        println!(
            "  {} → {} via {}{}",
            cm_payload::human(p.raw_len),
            cm_payload::human(p.gz_len),
            p.strategy.label(),
            if p.repacked { "" } else { " (cached)" },
        );
        if let Some(floor) = cm_payload::unpinned_floor(p.strategy, &p.target) {
            println!(
                "  ! glibc floor is {floor} rather than the pinned {}; install cargo-zigbuild \
                 + zig (`nix develop` provides both) for a payload that runs on older hosts",
                cm_payload::GLIBC_FLOOR
            );
        }
        out.push(p);
    }
    Ok(out)
}

/// How much slot to reserve for these payloads: what they need, plus headroom,
/// rounded up so similar variants share a compile.
fn reserve_for(payloads: &[&cm_payload::Payload]) -> usize {
    if payloads.is_empty() {
        return 0;
    }
    let sizes: Vec<(&str, &str, usize)> = payloads
        .iter()
        .map(|p| (p.target.as_str(), p.sha256.as_str(), p.gz_len as usize))
        .collect();
    round_up(
        cm_payload::slot_needed(&sizes) + RESERVE_HEADROOM,
        RESERVE_GRANULARITY,
    )
}

fn round_up(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

fn build_dashboard(ws: &Workspace, v: &Variant, reserve: usize) -> Result<()> {
    let features = v.cargo_features();
    let mut argv = vec![
        "build".to_string(),
        "--release".to_string(),
        "--locked".to_string(),
        "-p".to_string(),
        DASHBOARD_PKG.to_string(),
    ];
    if !features.is_empty() {
        argv.push("--features".to_string());
        argv.push(features.join(","));
    }
    if reserve > 0 {
        println!("  reserving {}", cm_payload::human(reserve as u64));
    }
    println!("  {} {}", cargo(), argv.join(" "));
    let status = Command::new(cargo())
        .args(&argv)
        .current_dir(&ws.root)
        .env("CM_PAYLOAD_RESERVE", reserve.to_string())
        .status()
        .context("spawning cargo")?;
    if !status.success() {
        bail!("building {} failed ({status})", v.label());
    }
    Ok(())
}

/// Run the artifact and check it reports what we just put in it.
///
/// The injector edits a linked binary in place, so "did that produce something
/// that still runs, and does it see its own payload" is exactly the question a
/// unit test can't answer. Skipped when the artifact can't run here, which today
/// means never — the dashboard is always built for the host.
fn verify(artifact: &Path, v: &Variant) -> Result<()> {
    let out = Command::new(artifact)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", artifact.display()))?;
    if !out.status.success() {
        bail!(
            "{} does not run after bundling ({})",
            artifact.display(),
            out.status
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for target in v.servers {
        if !text.contains(target) {
            bail!(
                "{} was injected with {target} but does not report it:\n{}",
                artifact.display(),
                text.trim()
            );
        }
    }
    if !v.bundles() && !text.contains("none") {
        bail!(
            "{} should carry nothing but reports:\n{}",
            artifact.display(),
            text.trim()
        );
    }
    Ok(())
}

fn list() {
    println!("variants (cargo xtask dist --variant <name>):");
    for v in VARIANTS {
        println!("  {:<22} {}", v.label(), v.what);
        if v.bundles() {
            println!("  {:<22} carries: {}", "", v.servers.join(", "));
        }
    }
    let defaults: Vec<&str> = DEFAULT_VARIANTS
        .iter()
        .filter_map(|n| find_variant(n).ok().map(Variant::label))
        .collect();
    println!("\ndefault: {}", defaults.join(", "));
}

fn find_variant(name: &str) -> Result<&'static Variant> {
    VARIANTS.iter().find(|v| v.name == name).ok_or_else(|| {
        anyhow!(
            "unknown variant {name:?}; try one of: {}",
            VARIANTS
                .iter()
                .map(|v| v.label())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The cargo that invoked us, so an xtask run stays on one toolchain.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant table names cargo features by string, so a typo would only
    /// surface as a cargo error partway through a long `dist`.
    #[test]
    fn the_variants_match_the_cargo_features() {
        let manifest = include_str!("../../Cargo.toml");
        for v in VARIANTS {
            for f in v.cargo_features() {
                assert!(
                    manifest.contains(&format!("\n{f} = [")),
                    "variant {:?} wants feature {f:?}, which the root Cargo.toml does not define",
                    v.label()
                );
            }
            // A bundling variant must actually enable the slot, and a plain one
            // must not pay for it.
            assert_eq!(
                v.cargo_features().contains(&BUNDLE_FEATURE),
                v.bundles(),
                "{} has the bundle feature and its servers out of step",
                v.label()
            );
        }
    }

    #[test]
    fn dist_variants_are_distinct_and_resolvable() {
        for v in VARIANTS {
            assert_eq!(find_variant(v.name).unwrap().label(), v.label());
        }
        let artifacts: BTreeSet<String> = VARIANTS.iter().map(Variant::artifact).collect();
        assert_eq!(
            artifacts.len(),
            VARIANTS.len(),
            "two variants share a filename"
        );
        for name in DEFAULT_VARIANTS {
            assert!(
                find_variant(name).is_ok(),
                "default variant {name:?} is unknown"
            );
        }
        assert!(find_variant("nope").is_err());
    }

    /// Rounding is what lets two single-arch variants share one dashboard
    /// compile; the headroom is what stops a slot sized to the byte overflowing
    /// the next time the server grows.
    #[test]
    fn the_reservation_rounds_up_and_leaves_headroom() {
        assert_eq!(round_up(1, 1 << 20), 1 << 20);
        assert_eq!(round_up(1 << 20, 1 << 20), 1 << 20);
        assert_eq!(round_up((1 << 20) + 1, 1 << 20), 2 << 20);
        assert_eq!(round_up(0, 1 << 20), 0);

        // Two payloads of similar size land in the same bucket, so the dashboard
        // is compiled once for both.
        let a =
            cm_payload::slot_needed(&[("x86_64-unknown-linux-gnu", &"a".repeat(64), 2_500_000)]);
        let b =
            cm_payload::slot_needed(&[("aarch64-unknown-linux-gnu", &"b".repeat(64), 2_400_000)]);
        assert_eq!(
            round_up(a + RESERVE_HEADROOM, RESERVE_GRANULARITY),
            round_up(b + RESERVE_HEADROOM, RESERVE_GRANULARITY),
        );
    }

    #[test]
    fn a_plain_variant_reserves_nothing() {
        let plain = find_variant("").unwrap();
        assert!(!plain.bundles());
        assert!(plain.cargo_features().is_empty());
        assert_eq!(reserve_for(&[]), 0);
    }
}
