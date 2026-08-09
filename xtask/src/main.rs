//! `cargo xtask` — captain-miao's build chores.
//!
//! Two subcommands:
//!
//! - `prepare-servers` **obtains** `miao-server` binaries — cross-built
//!   here, downloaded from a published release, or handed over as paths.
//!   Release CI runs this to publish them.
//! - `dist` builds the named release dashboard variants into `dist/`, obtaining
//!   whatever servers each one carries and handing the archives to its build.
//!
//! Where the servers come from is a `--from` flag, not an assumption, and
//! that seam is the point: it is orthogonal to how the dashboard is compiled.
//! The dashboard learns what to carry from one environment variable naming a
//! manifest (`build.rs`), so obtaining a server and building a dashboard stay
//! independent without the payload having to be patched in afterwards.
//!
//! What still has to be arranged is that a bundled dashboard reports *which*
//! server it carries: the workspace version is the only thing a released artifact
//! is keyed on and it doesn't move between dev builds, so `miao --version` prints
//! each payload's digest, and the deploy writes that digest to the host.

mod server;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

/// The dashboard package, and the binary it installs (which is `miao`, not the
/// package name — see the `[[bin]]` note in the root Cargo.toml).
const DASHBOARD_PKG: &str = "captain-miao";
const DASHBOARD_BIN: &str = "miao";

/// The server package, and the file name `server` writes. Same string for both,
/// and the same one the release assets carry inside them.
const SERVER_BIN: &str = "miao-server";

/// The environment variable `build.rs` reads to learn what to embed. Kept in
/// step with the constant of the same name there.
const MANIFEST_ENV: &str = "CM_SERVER_PAYLOAD_MANIFEST";

/// The feature a dashboard needs to reach an embedded server at all. A bundled
/// build implies it: the deploy path lives behind the remote-hosts gate, so a
/// server carried without it would be dead weight by construction.
const REMOTE_FEATURE: &str = "remote";

const X86: &str = "x86_64-unknown-linux-gnu";
const ARM: &str = "aarch64-unknown-linux-gnu";
const X86_MUSL: &str = "x86_64-unknown-linux-musl";
const ARM_MUSL: &str = "aarch64-unknown-linux-musl";

/// What a **shipping bundled variant embeds**: glibc only.
///
/// Deliberately *not* the same set as [`PUBLISHED_TARGETS`], and the two must
/// not be re-merged. A released dashboard embeds no musl: musl's audience is
/// hosts with no generic loader (NixOS, Alpine, distroless), and those have a
/// better answer already — a server built against their own libc, on their own
/// PATH, with no deploy at all. Making every downloader carry ~6 MiB aimed at
/// the one platform that doesn't need it is the wrong default. A released
/// dashboard reaches such a host by *downloading* the published musl asset
/// instead, which is what [`PUBLISHED_TARGETS`] exists for.
const LINUX_TARGETS: &[&str] = &[X86, ARM];

/// What a release **publishes** as assets, and what `prepare-servers` builds by
/// default: all four, musl included.
///
/// Nothing embeds the musl builds — they are fetched at runtime by a dashboard
/// that meets a host its glibc payload cannot run on. That is the whole reason
/// the two halves compose, and it is why this set is wider than the one above:
/// publishing is how a payload becomes reachable without being carried.
const PUBLISHED_TARGETS: &[&str] = &[X86, ARM, X86_MUSL, ARM_MUSL];

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
    /// Obtain `miao-server` binaries (what release CI publishes).
    PrepareServers(PrepareServersArgs),
}

// ---------------------------------------------------------------------------
// Where servers come from
// ---------------------------------------------------------------------------

/// The seam. Every command that needs servers takes this and never learns which
/// arm answered — `server::` returns the same `Payload` from all three.
#[derive(Args, Clone)]
struct ServerArgs {
    /// Where to get servers: `build` (cross-compile from this workspace) or
    /// `release[:<version>]` (download a published one; defaults to this
    /// workspace's version).
    #[arg(long = "from", value_name = "SOURCE", default_value = "build")]
    source: String,

    /// Use this exact binary for one target, whatever `--from` says.
    /// Repeatable: `--server x86_64-unknown-linux-gnu=path/to/miao-server`.
    #[arg(long = "server", value_name = "TARGET=PATH")]
    files: Vec<String>,

    /// Where `--from release` downloads from.
    #[arg(long, value_name = "URL", default_value = server::RELEASE_BASE)]
    release_base: String,
}

/// The parsed form of `--from`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Build,
    Release(String),
}

/// Parse `--from`. Pure, so the accepted spellings are pinned by a test.
///
/// A bare `release` means this workspace's version, which is the only defensible
/// default: the dashboard being built is that version, and a server from a
/// different release is a combination nobody asked for by omission.
fn parse_source(spec: &str) -> Result<Source> {
    let (kind, rest) = match spec.split_once(':') {
        Some((k, r)) => (k.trim(), Some(r.trim())),
        None => (spec.trim(), None),
    };
    match (kind, rest) {
        ("build", None) => Ok(Source::Build),
        ("build", Some(_)) => bail!("`build` takes no version — it builds this workspace"),
        ("release", None) => Ok(Source::Release(env!("CARGO_PKG_VERSION").to_string())),
        ("release", Some("")) => bail!("`release:` needs a version, or drop the colon"),
        ("release", Some(v)) => Ok(Source::Release(v.trim_start_matches('v').to_string())),
        _ => bail!("unknown server source {spec:?}; try `build` or `release[:<version>]`"),
    }
}

/// Parse the repeated `--server TARGET=PATH` pairs. Pure.
fn parse_files(pairs: &[String]) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    for pair in pairs {
        let (target, path) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("--server wants TARGET=PATH, got {pair:?}"))?;
        let (target, path) = (target.trim(), path.trim());
        if target.is_empty() || path.is_empty() {
            bail!("--server wants TARGET=PATH, got {pair:?}");
        }
        if out
            .insert(target.to_string(), PathBuf::from(path))
            .is_some()
        {
            bail!("--server names {target} twice");
        }
    }
    Ok(out)
}

impl ServerArgs {
    /// Resolve every requested target to a packed payload.
    ///
    /// Explicit `--server` paths win over `--from`, so a CI job that already
    /// downloaded its artifacts doesn't have to fetch them again — and a path
    /// naming a target nothing asked for is an error rather than a silent no-op,
    /// since a typo'd triple would otherwise look like it worked.
    fn resolve(&self, ws: &Workspace, targets: &BTreeSet<&str>) -> Result<Vec<server::Payload>> {
        let files = parse_files(&self.files)?;
        if let Some(stray) = files.keys().find(|t| !targets.contains(t.as_str())) {
            bail!(
                "--server names {stray}, which nothing being built wants; wanted: {}",
                targets.iter().copied().collect::<Vec<_>>().join(", ")
            );
        }
        // Parsed even when nothing needs servers, so a typo'd `--from` is
        // reported rather than shrugged off by whichever variant happened not to
        // want one. The *probe* below is what stays conditional — that one costs
        // a PATH walk per entry, where this costs a `split_once`.
        let source = parse_source(&self.source)?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        let dir = ws.server_build_dir();
        // Only the build source needs these, and probing them costs a PATH walk
        // per entry — so it happens once, and not at all when nothing is built.
        let build_env = matches!(source, Source::Build)
            .then(|| {
                server::host_triple()
                    .map(|host| (host, server::Tools::detect()))
                    .map_err(|e| anyhow!("{e}"))
            })
            .transpose()?;

        let mut out = Vec::new();
        for target in targets {
            println!("▶ {SERVER_BIN} for {target}");
            let payload = match (files.get(*target), &source) {
                (Some(path), _) => server::from_file(&dir, target, path),
                (None, Source::Build) => {
                    let (host, tools) = build_env.as_ref().expect("probed for the build source");
                    server::build(&ws.root, &dir, target, host, tools)
                }
                (None, Source::Release(version)) => {
                    server::fetch(&dir, target, version, &self.release_base)
                }
            }
            .map_err(|e| anyhow!("{e}"))?;
            report(&payload);
            out.push(payload);
        }
        Ok(out)
    }
}

/// One line per payload: what it cost, where it came from, and the one warning
/// worth interrupting for.
fn report(p: &server::Payload) {
    println!(
        "  {} → {} via {}{}",
        server::human(p.raw_len),
        server::human(p.gz_len),
        p.provenance.label(),
        if p.repacked { "" } else { " (cached)" },
    );
    // Only a build we ran has a floor we chose; a fetched or supplied binary was
    // linked by somebody else, so there is nothing here to warn about.
    if let Some(floor) = p
        .provenance
        .strategy()
        .and_then(|s| server::unpinned_floor(s, &p.target))
    {
        println!(
            "  ! glibc floor is {floor} rather than the pinned {}; install cargo-zigbuild \
             + zig (`nix develop` provides both) for a payload that runs on older hosts",
            server::GLIBC_FLOOR
        );
    }
}

// ---------------------------------------------------------------------------
// Variants
// ---------------------------------------------------------------------------

/// A named dashboard build: which servers it carries, and the file it lands on.
///
/// The dashboard isn't one artifact. A laptop driving a Linux fleet wants the
/// server binaries in the box; someone running captain-miao purely locally —
/// most users — should not pay ~5 MB for a code path they never reach.
struct Variant {
    /// Suffix of the artifact in `dist/`; the plain build has none.
    name: &'static str,
    /// Extra cargo features, beyond the `remote` gate implied by `servers`.
    features: &'static [&'static str],
    /// Server targets to obtain and inject.
    servers: &'static [&'static str],
    what: &'static str,
}

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
        servers: LINUX_TARGETS,
        what: "remote hosts, deploying its own server to either Linux arch",
    },
    // A dev variant, deliberately outside DEFAULT_VARIANTS so it is never a
    // release artifact: it exists so the candidate loop and the musl path are
    // exercised by a build we can actually run, rather than only on a host we
    // don't have. Released dashboards stay gnu-only — see LINUX_TARGETS.
    Variant {
        name: "bundle-linux-all",
        features: &[],
        servers: PUBLISHED_TARGETS,
        what: "dev: every Linux server (gnu + musl), for exercising the musl fallback",
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
            f.push(REMOTE_FEATURE);
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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = workspace_root()?;
    let ws = Workspace {
        target_dir: target_dir(&root),
        root,
    };
    match cli.command {
        Cmd::Dist(args) => dist(&ws, &args),
        Cmd::PrepareServers(args) => prepare_servers(&ws, &args),
    }
}

/// The two directories the commands need, resolved once.
struct Workspace {
    root: PathBuf,
    target_dir: PathBuf,
}

impl Workspace {
    /// Where obtained servers and their archives live. Under `target/` because
    /// that is what they are — build products, disposed by `cargo clean`.
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

// ---------------------------------------------------------------------------
// dist
// ---------------------------------------------------------------------------

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
    #[command(flatten)]
    servers: ServerArgs,
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

    // Obtain each server once even when several variants want it: this is by far
    // the slowest step, whether it is a cross-build or a download.
    let needed: BTreeSet<&str> = wanted
        .iter()
        .flat_map(|v| v.servers.iter().copied())
        .collect();
    let servers = args.servers.resolve(ws, &needed)?;

    let dist_dir = ws.root.join("dist");
    std::fs::create_dir_all(&dist_dir)
        .with_context(|| format!("creating {}", dist_dir.display()))?;

    let mut built = Vec::new();
    for v in &wanted {
        println!("\n▶ {}", v.label());
        let payloads = pick_payloads(&servers, v.servers)?;

        build_dashboard(ws, v, &payloads)?;

        let to = dist_dir.join(v.artifact());
        // Copy rather than rename: every variant's build reuses the same
        // `target/release/cm`, so leaving it in place would be a lie the moment
        // the next one overwrote it.
        let from = ws.target_dir.join("release").join(DASHBOARD_BIN);
        std::fs::copy(&from, &to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;

        verify(&to, v.servers)?;
        built.push((v.artifact(), std::fs::metadata(&to).map(|m| m.len())?));
    }

    println!("\n{}:", dist_dir.display());
    for (name, size) in &built {
        println!("  {name:<30} {}", server::human(*size));
    }
    Ok(())
}

/// The payloads for one variant, out of everything this run obtained.
fn pick_payloads<'a>(
    servers: &'a [server::Payload],
    targets: &[&str],
) -> Result<Vec<&'a server::Payload>> {
    targets
        .iter()
        .map(|t| {
            servers
                .iter()
                .find(|p| p.target == *t)
                .ok_or_else(|| anyhow!("no server obtained for {t}"))
        })
        .collect()
}

fn build_dashboard(ws: &Workspace, v: &Variant, payloads: &[&server::Payload]) -> Result<()> {
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

    let manifest = write_manifest(ws, v, payloads)?;
    println!("  {} {}", cargo(), argv.join(" "));
    let status = Command::new(cargo())
        .args(&argv)
        .current_dir(&ws.root)
        // Always set, even to the empty-manifest path: a stale value inherited
        // from the caller's environment would otherwise decide what a variant
        // carries, and a plain `miao` would quietly stop being plain.
        .env(MANIFEST_ENV, &manifest)
        .status()
        .context("spawning cargo")?;
    if !status.success() {
        bail!("building {} failed ({status})", v.label());
    }
    Ok(())
}

/// Write the manifest `build.rs` reads, and return its path.
///
/// One file per variant, under `target/`, so two variants built in one run can't
/// race on it and `cargo clean` disposes of them. The archives are named by
/// absolute path rather than copied here — `build.rs` stages its own copy into
/// `OUT_DIR`, which is what keeps the watched file and the embedded file distinct
/// (embedding the watched one ties its mtime to the build script's stamp and
/// re-runs the script on every build).
fn write_manifest(ws: &Workspace, v: &Variant, payloads: &[&server::Payload]) -> Result<PathBuf> {
    let dir = ws.target_dir.join("cm-server-payloads");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!(
        "{}.tsv",
        if v.name.is_empty() { "plain" } else { v.name }
    ));

    let mut text = String::new();
    for p in payloads {
        // Tab-separated because a path may contain spaces and an `=`, but not a
        // tab or a newline in any layout this produces.
        text.push_str(&format!(
            "{}\t{}\t{}\n",
            p.target,
            p.sha256,
            p.gz_path.display()
        ));
        println!("  embedding {} ({})", p.target, server::human(p.gz_len));
    }

    // Written only when the contents actually change. `build.rs` watches this
    // file, so rewriting it unconditionally would bump its mtime on every run,
    // re-run the build script, and recompile the dashboard — a full LTO relink
    // for a file whose bytes were identical. (Same shape as the `OUT_DIR` staging
    // over in `build.rs`: watch a file, and you must stop touching it.)
    let unchanged = std::fs::read_to_string(&path).is_ok_and(|old| old == text);
    if !unchanged {
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
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
    println!("\nservers come from --from build (default) or --from release[:<version>],");
    println!("or one at a time from --server <target>=<path>.");
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

/// Run the artifact and check it reports what it was built to carry.
///
/// A manifest reaches the dashboard through an environment variable and a
/// generated file, which is exactly the kind of seam that fails silently: a
/// variable that didn't survive, a manifest naming an archive that moved, and the
/// build succeeds carrying nothing. Running the thing is the only check that
/// catches it. The artifact is always built for this host, so failing to start is
/// a failure rather than something to note and move past.
fn verify(artifact: &Path, servers: &[&str]) -> Result<()> {
    let out = Command::new(artifact)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", artifact.display()))?;
    if !out.status.success() {
        bail!("{} does not run ({})", artifact.display(), out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for target in servers {
        if !text.contains(target) {
            bail!(
                "{} was built to carry {target} but does not report it:\n{}",
                artifact.display(),
                text.trim()
            );
        }
    }
    if servers.is_empty() && !text.contains("none") {
        bail!(
            "{} should carry nothing but reports:\n{}",
            artifact.display(),
            text.trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

#[derive(Args)]
struct PrepareServersArgs {
    /// Targets to produce. Defaults to both Linux arches — the ones a release
    /// publishes and a bundled dashboard deploys.
    #[arg(long = "target", value_name = "TRIPLE")]
    targets: Vec<String>,
    /// Where to write `<target>/miao-server`.
    #[arg(long, value_name = "DIR", default_value = "dist/servers")]
    out: PathBuf,
    #[command(flatten)]
    servers: ServerArgs,
}

/// Produce server binaries and lay them out for publishing.
///
/// This is what release CI runs, which is the point: the servers a release
/// publishes come out of the same code path — same strategy choice, same pinned
/// glibc floor, same architecture check — as the ones a developer cross-builds
/// locally. Two pipelines producing "the server" would eventually produce two
/// different ones.
fn prepare_servers(ws: &Workspace, args: &PrepareServersArgs) -> Result<()> {
    let targets: BTreeSet<&str> = if args.targets.is_empty() {
        PUBLISHED_TARGETS.iter().copied().collect()
    } else {
        args.targets.iter().map(String::as_str).collect()
    };
    let payloads = args.servers.resolve(ws, &targets)?;

    let out_dir = if args.out.is_absolute() {
        args.out.clone()
    } else {
        ws.root.join(&args.out)
    };
    println!();
    for p in &payloads {
        let dir = out_dir.join(&p.target);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let to = dir.join(SERVER_BIN);
        std::fs::copy(&p.bin_path, &to)
            .with_context(|| format!("copying {} to {}", p.bin_path.display(), to.display()))?;
        // cargo already produces 0755, but a fetched or hand-supplied binary may
        // have arrived without it, and this is the file that goes into a tarball.
        set_executable(&to)?;
        println!(
            "  {}  {}  {}",
            to.display(),
            server::human(p.raw_len),
            &p.sha256[..12]
        );
    }
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
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
            // Carrying a server implies the gate that reaches it — one-way, since
            // the `remote` variant enables it and deliberately carries nothing.
            assert!(
                !v.bundles() || v.cargo_features().contains(&REMOTE_FEATURE),
                "{} carries servers the build could never reach",
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

    /// The plain build is the one most people download, so it has to stay a
    /// plain build: no servers, no features, nothing extra linked.
    #[test]
    fn a_plain_variant_carries_and_enables_nothing() {
        let plain = find_variant("").unwrap();
        assert!(!plain.bundles());
        assert!(plain.cargo_features().is_empty());
    }

    /// `--from` is the decoupling seam, so its spellings are a contract.
    #[test]
    fn the_server_source_parses_every_spelling_it_advertises() {
        assert_eq!(parse_source("build").unwrap(), Source::Build);
        assert_eq!(
            parse_source("release:0.2.1").unwrap(),
            Source::Release("0.2.1".into())
        );
        // A tag and a bare version must not resolve to different releases.
        assert_eq!(
            parse_source("release:v0.2.1").unwrap(),
            parse_source("release:0.2.1").unwrap()
        );
        // A bare `release` means the version being built, which is the only
        // defensible reading of an omitted version.
        assert_eq!(
            parse_source("release").unwrap(),
            Source::Release(env!("CARGO_PKG_VERSION").to_string())
        );

        for bad in ["", "releases", "build:0.2.1", "release:", "fetch"] {
            assert!(parse_source(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn explicit_server_paths_parse_and_reject_the_ambiguous() {
        let ok = parse_files(&[format!("{X86}=/tmp/a"), format!("{ARM}=./b/miao-server")]).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[X86], PathBuf::from("/tmp/a"));

        // A path containing `=` still works: only the first one separates.
        let odd = parse_files(&[format!("{X86}=/tmp/a=b")]).unwrap();
        assert_eq!(odd[X86], PathBuf::from("/tmp/a=b"));

        assert!(parse_files(&["no-equals".into()]).is_err());
        assert!(parse_files(&[format!("{X86}=")]).is_err());
        assert!(parse_files(&["=/tmp/a".to_string()]).is_err());
        // Naming one target twice is a mistake, not a last-one-wins.
        assert!(parse_files(&[format!("{X86}=/a"), format!("{X86}=/b")]).is_err());
    }

    /// A `--server` for a target nothing wants is a typo'd triple nine times in
    /// ten, and silently ignoring it looks exactly like success.
    #[test]
    fn a_server_path_nothing_asked_for_is_an_error() {
        let ws = Workspace {
            root: PathBuf::from("/w"),
            target_dir: PathBuf::from("/w/target"),
        };
        let args = ServerArgs {
            source: "build".into(),
            files: vec![format!("x86_64-unknown-linux-gnuu=/tmp/a")],
            release_base: server::RELEASE_BASE.into(),
        };
        let wanted: BTreeSet<&str> = [X86].into_iter().collect();
        let Err(err) = args.resolve(&ws, &wanted) else {
            panic!("a --server for an unwanted target must not be accepted");
        };
        let msg = err.to_string();
        assert!(msg.contains("gnuu"), "{msg}");
        assert!(msg.contains(X86), "{msg}");
    }

    /// Nothing wanted means nothing obtained — no cross-toolchain probe, no
    /// download. But the flags are still checked: a plain variant happening not
    /// to want a server must not turn a typo'd `--from` into a silent no-op.
    #[test]
    fn asking_for_no_servers_does_no_work_but_still_checks_the_flags() {
        let ws = Workspace {
            root: PathBuf::from("/w"),
            target_dir: PathBuf::from("/w/target"),
        };
        let args = |source: &str| ServerArgs {
            source: source.into(),
            files: Vec::new(),
            release_base: server::RELEASE_BASE.into(),
        };
        assert!(
            args("build")
                .resolve(&ws, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        assert!(args("nonsense").resolve(&ws, &BTreeSet::new()).is_err());
    }

    /// `build.rs` watches the manifest, so writing it unconditionally would bump
    /// its mtime every run, re-run the build script, and force a full LTO relink
    /// of a dashboard whose inputs hadn't changed. This exact mistake has been
    /// made twice — once here, once embedding the watched archive directly — so
    /// pin it: identical payloads must leave the file untouched.
    #[test]
    fn rewriting_an_identical_manifest_does_not_touch_the_file() {
        let root = std::env::temp_dir().join(format!("cm-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ws = Workspace {
            target_dir: root.join("target"),
            root: root.clone(),
        };
        let v = find_variant("bundle-linux").unwrap();

        let gz = root.join("server.gz");
        std::fs::write(&gz, b"not really a gzip").unwrap();
        let payload = |target: &str| server::Payload {
            target: target.to_string(),
            sha256: "a".repeat(64),
            bin_path: gz.clone(),
            gz_path: gz.clone(),
            raw_len: 17,
            gz_len: 17,
            provenance: server::Provenance::Local,
            repacked: false,
        };
        let (a, b) = (payload(X86), payload(ARM));

        let path = write_manifest(&ws, v, &[&a, &b]).unwrap();
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "one line per payload");
        assert!(text.lines().all(|l| l.split('\t').count() == 3), "{text}");

        assert_eq!(write_manifest(&ws, v, &[&a, &b]).unwrap(), path);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            first,
            "an unchanged manifest must not be rewritten"
        );

        // A different payload set *must* land, or the dashboard would keep
        // whatever it was built with last.
        write_manifest(&ws, v, &[&a]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The variant that ships and the `bundle` default have to agree: they are
    /// two spellings of "what a release carries".
    #[test]
    fn bundle_defaults_to_what_the_shipping_variant_carries() {
        assert_eq!(find_variant("bundle-linux").unwrap().servers, LINUX_TARGETS);
    }

    /// No **release** artifact may embed a musl server.
    ///
    /// The two target sets were one const until musl arrived, and re-merging
    /// them is the easy mistake: it silently adds ~6 MiB to every download for
    /// a payload aimed at the one platform that is better served without a
    /// deploy at all. The reverse mistake — dropping musl from what we publish —
    /// is caught by the test below, since nothing embeds those builds and the
    /// runtime downloader is the only thing that can reach them.
    #[test]
    fn no_release_variant_embeds_a_musl_server() {
        for name in DEFAULT_VARIANTS {
            let v = find_variant(name).unwrap();
            assert!(
                !v.servers.iter().any(|t| t.contains("-musl")),
                "release variant {:?} embeds a musl server: {:?}",
                v.label(),
                v.servers
            );
        }
        // The dev variant is where musl *is* carried, and it is deliberately not
        // a default — that is what keeps the loop exercisable without shipping it.
        let all = find_variant("bundle-linux-all").unwrap();
        assert!(all.servers.iter().any(|t| t.contains("-musl")));
        assert!(!DEFAULT_VARIANTS.contains(&"bundle-linux-all"));
    }

    /// Publishing is how a payload becomes reachable without being embedded, so
    /// the published set has to be a strict superset of the embedded one.
    #[test]
    fn every_embedded_server_is_also_published() {
        for t in LINUX_TARGETS {
            assert!(
                PUBLISHED_TARGETS.contains(t),
                "{t} is embedded but never published"
            );
        }
        // And the musl builds exist *only* as published assets — the case the
        // runtime source chain fetches.
        assert!(PUBLISHED_TARGETS.iter().any(|t| t.contains("-musl")));
    }
}
