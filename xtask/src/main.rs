//! `cargo xtask` — captain-miao's build chores.
//!
//! Three subcommands, and the split between them is the design:
//!
//! - `server` **obtains** `miao-server` binaries. Cross-builds them here,
//!   and is what release CI runs to publish them.
//! - `bundle` **writes** servers into a dashboard that is already linked. Needs no
//!   cargo, no cross toolchain, and not even the server's sources — just a `cm`
//!   built with the `bundle` feature and some server binaries.
//! - `dist` is the convenience that runs both plus the dashboard build, producing
//!   the named release variants in `dist/`.
//!
//! Where the servers come from is a `--servers` flag, not an assumption: build
//! them here, download them from a published release, or hand over paths. That
//! decoupling is why payloads are injected after linking rather than compiled in
//! — a compiled-in payload is an input to the dashboard's build, so changing the
//! source would mean recompiling the dashboard every time.
//!
//! What still has to be arranged is that a bundled dashboard reports *which*
//! server it carries: the workspace version is the only thing a released artifact
//! is keyed on and it doesn't move between dev builds, so `miao --version` prints
//! each payload's digest, and the deploy writes that digest to the host.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

/// The dashboard package, and the binary it installs (which is `miao`, not the
/// package name — see the `[[bin]]` note in the root Cargo.toml).
const DASHBOARD_PKG: &str = "captain-miao";
const DASHBOARD_BIN: &str = "miao";

/// The server package, and the file name `bundle`/`server` read and write. Same
/// string for both, and the same one the release assets carry inside them.
const SERVER_BIN: &str = "miao-server";

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

const X86: &str = "x86_64-unknown-linux-gnu";
const ARM: &str = "aarch64-unknown-linux-gnu";

/// The servers a release publishes, and what `bundle` carries when asked for
/// nothing in particular.
const LINUX_TARGETS: &[&str] = &[X86, ARM];

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
    /// Write server payloads into an already-linked dashboard binary.
    Bundle(BundleArgs),
    /// Obtain `miao-server` binaries (what release CI publishes).
    Server(ServerCmdArgs),
}

// ---------------------------------------------------------------------------
// Where servers come from
// ---------------------------------------------------------------------------

/// The seam. Every command that needs servers takes this and never learns which
/// arm answered — `cm_payload` returns the same `Payload` from all three.
#[derive(Args, Clone)]
struct ServerArgs {
    /// Where to get servers: `build` (cross-compile from this workspace) or
    /// `release[:<version>]` (download a published one; defaults to this
    /// workspace's version).
    #[arg(long = "servers", value_name = "SOURCE", default_value = "build")]
    source: String,

    /// Use this exact binary for one target, whatever `--servers` says.
    /// Repeatable: `--server x86_64-unknown-linux-gnu=path/to/miao-server`.
    #[arg(long = "server", value_name = "TARGET=PATH")]
    files: Vec<String>,

    /// Where `--servers release` downloads from.
    #[arg(long, value_name = "URL", default_value = cm_payload::RELEASE_BASE)]
    release_base: String,
}

/// The parsed form of `--servers`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Build,
    Release(String),
}

/// Parse `--servers`. Pure, so the accepted spellings are pinned by a test.
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
    /// Explicit `--server` paths win over `--servers`, so a CI job that already
    /// downloaded its artifacts doesn't have to fetch them again — and a path
    /// naming a target nothing asked for is an error rather than a silent no-op,
    /// since a typo'd triple would otherwise look like it worked.
    fn resolve(
        &self,
        ws: &Workspace,
        targets: &BTreeSet<&str>,
    ) -> Result<Vec<cm_payload::Payload>> {
        let files = parse_files(&self.files)?;
        if let Some(stray) = files.keys().find(|t| !targets.contains(t.as_str())) {
            bail!(
                "--server names {stray}, which nothing being built wants; wanted: {}",
                targets.iter().copied().collect::<Vec<_>>().join(", ")
            );
        }
        // Parsed even when nothing needs servers, so a typo'd `--servers` is
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
                cm_payload::host_triple()
                    .map(|host| (host, cm_payload::Tools::detect()))
                    .map_err(|e| anyhow!("{e}"))
            })
            .transpose()?;

        let mut out = Vec::new();
        for target in targets {
            println!("▶ {SERVER_BIN} for {target}");
            let payload = match (files.get(*target), &source) {
                (Some(path), _) => cm_payload::from_file(&dir, target, path),
                (None, Source::Build) => {
                    let (host, tools) = build_env.as_ref().expect("probed for the build source");
                    cm_payload::build(&ws.root, &dir, target, host, tools)
                }
                (None, Source::Release(version)) => {
                    cm_payload::fetch(&dir, target, version, &self.release_base)
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
fn report(p: &cm_payload::Payload) {
    println!(
        "  {} → {} via {}{}",
        cm_payload::human(p.raw_len),
        cm_payload::human(p.gz_len),
        p.provenance.label(),
        if p.repacked { "" } else { " (cached)" },
    );
    // Only a build we ran has a floor we chose; a fetched or supplied binary was
    // linked by somebody else, so there is nothing here to warn about.
    if let Some(floor) = p
        .provenance
        .strategy()
        .and_then(|s| cm_payload::unpinned_floor(s, &p.target))
    {
        println!(
            "  ! glibc floor is {floor} rather than the pinned {}; install cargo-zigbuild \
             + zig (`nix develop` provides both) for a payload that runs on older hosts",
            cm_payload::GLIBC_FLOOR
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
    /// Extra cargo features, beyond the bundle feature implied by `servers`.
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
        Cmd::Bundle(args) => bundle(&ws, &args),
        Cmd::Server(args) => server(&ws, &args),
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

        build_dashboard(ws, v, reserve_for(&payloads))?;

        let to = dist_dir.join(v.artifact());
        // Copy rather than rename: every variant's build reuses the same
        // `target/release/cm`, so leaving it in place would be a lie the moment
        // the next one overwrote it.
        let from = ws.target_dir.join("release").join(DASHBOARD_BIN);
        std::fs::copy(&from, &to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;

        if v.bundles() {
            inject_into(&to, &payloads)?;
        }
        // The artifact is always built for this host, so a dashboard that won't
        // run is a failure rather than something to note and move past.
        match verify(&to, v.servers)? {
            Verified::Ran => {}
            Verified::CouldNotRun(why) => {
                bail!("{} does not run after bundling: {why}", to.display())
            }
        }
        built.push((v.artifact(), std::fs::metadata(&to).map(|m| m.len())?));
    }

    println!("\n{}:", dist_dir.display());
    for (name, size) in &built {
        println!("  {name:<30} {}", cm_payload::human(*size));
    }
    Ok(())
}

/// The payloads for one variant, out of everything this run obtained.
fn pick_payloads<'a>(
    servers: &'a [cm_payload::Payload],
    targets: &[&str],
) -> Result<Vec<&'a cm_payload::Payload>> {
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
    println!("\nservers come from --servers build (default) or --servers release[:<version>],");
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

// ---------------------------------------------------------------------------
// bundle
// ---------------------------------------------------------------------------

#[derive(Args)]
struct BundleArgs {
    /// A `cm` built with the `bundle` feature. Its reserved slot is what this
    /// writes into, so a dashboard built without the feature is refused.
    #[arg(value_name = "DASHBOARD")]
    binary: PathBuf,
    /// Targets to carry. Defaults to whatever `--server` names, else both Linux
    /// arches.
    #[arg(long = "target", value_name = "TRIPLE")]
    targets: Vec<String>,
    /// Write the bundled dashboard here instead of patching it in place.
    #[arg(short, long, value_name = "PATH")]
    out: Option<PathBuf>,
    #[command(flatten)]
    servers: ServerArgs,
}

/// Write servers into an already-linked dashboard.
///
/// The command the whole after-linking design exists for: no cargo runs here, no
/// cross toolchain is consulted, and the dashboard's sources aren't read. Given a
/// released `cm` and `--servers release`, it needs nothing from this workspace at
/// all beyond xtask itself.
fn bundle(ws: &Workspace, args: &BundleArgs) -> Result<()> {
    // Checked before anything slow. Obtaining servers can be a cross-compile or
    // a multi-megabyte download, and learning only afterwards that the dashboard
    // path was a typo would waste all of it.
    if !args.binary.is_file() {
        bail!(
            "{} is not a file; `bundle` writes into a dashboard that already \
             exists (build one with `cargo xtask dist --variant bundle-linux`)",
            args.binary.display()
        );
    }

    let files = parse_files(&args.servers.files)?;
    // Three ways to say which targets, in decreasing explicitness. Falling back
    // to both Linux arches matches `bundle-linux`, the variant a release ships.
    let targets: BTreeSet<&str> = if !args.targets.is_empty() {
        args.targets.iter().map(String::as_str).collect()
    } else if !files.is_empty() {
        files.keys().map(String::as_str).collect()
    } else {
        LINUX_TARGETS.iter().copied().collect()
    };

    let servers = args.servers.resolve(ws, &targets)?;
    let ordered: Vec<&str> = targets.iter().copied().collect();
    let payloads = pick_payloads(&servers, &ordered)?;

    // `--out` naming the input is patching in place spelled the long way. It has
    // to be caught rather than obeyed: `fs::copy` opens the destination with
    // O_TRUNC, so copying a file onto itself empties it before a byte is read.
    let out = args
        .out
        .as_ref()
        .filter(|out| !same_file(&args.binary, out));

    let to = match out {
        Some(out) => {
            if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::copy(&args.binary, out).with_context(|| {
                format!("copying {} to {}", args.binary.display(), out.display())
            })?;
            out.clone()
        }
        None => args.binary.clone(),
    };

    println!("\n▶ {}", to.display());
    inject_into(&to, &payloads)?;

    // Unlike `dist`, the binary here may well be for another platform — bundling
    // a Linux `cm` on a mac is a thing this command is *for*. So a binary that
    // won't exec is reported rather than fatal; one that execs and reports the
    // wrong payloads is still an error, because that is a real bad patch.
    match verify(&to, &ordered)? {
        Verified::Ran => println!("  verified: it runs and reports what was injected"),
        Verified::CouldNotRun(why) => {
            println!("  ! not verified — could not run it here ({why})");
            println!("    that is expected when bundling for another platform; run it there.");
        }
    }
    Ok(())
}

/// Whether two paths name the same file on disk.
///
/// Canonicalised rather than compared textually, so `./cm` and `cm` and a path
/// through a symlink all answer the same. A path that can't be resolved is
/// simply not the other one — the caller has already established the source
/// exists, and a non-existent destination is the ordinary case.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn inject_into(binary: &Path, payloads: &[&cm_payload::Payload]) -> Result<()> {
    let done = cm_payload::inject(binary, payloads).map_err(|e| anyhow!("{e}"))?;
    println!(
        "  injected {} into a {} slot ({}% used)",
        cm_payload::human(done.used as u64),
        cm_payload::human(done.capacity as u64),
        done.used * 100 / done.capacity.max(1),
    );
    Ok(())
}

/// Outcome of running a bundled artifact to see what it reports.
enum Verified {
    Ran,
    CouldNotRun(String),
}

/// Run the artifact and check it reports what was just put in it.
///
/// The injector edits a linked binary in place, so "did that produce something
/// that still runs, and does it see its own payload" is exactly the question a
/// unit test can't answer. Failing to *start* is returned rather than raised, so
/// each caller can decide whether a foreign-platform binary is a problem.
fn verify(artifact: &Path, servers: &[&str]) -> Result<Verified> {
    let out = match Command::new(artifact).arg("--version").output() {
        Ok(out) => out,
        Err(e) => return Ok(Verified::CouldNotRun(e.to_string())),
    };
    if !out.status.success() {
        return Ok(Verified::CouldNotRun(out.status.to_string()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for target in servers {
        if !text.contains(target) {
            bail!(
                "{} was injected with {target} but does not report it:\n{}",
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
    Ok(Verified::Ran)
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

#[derive(Args)]
struct ServerCmdArgs {
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
fn server(ws: &Workspace, args: &ServerCmdArgs) -> Result<()> {
    let targets: BTreeSet<&str> = if args.targets.is_empty() {
        LINUX_TARGETS.iter().copied().collect()
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
            cm_payload::human(p.raw_len),
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

    /// `--servers` is the decoupling seam, so its spellings are a contract.
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
            release_base: cm_payload::RELEASE_BASE.into(),
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
    /// to want a server must not turn a typo'd `--servers` into a silent no-op.
    #[test]
    fn asking_for_no_servers_does_no_work_but_still_checks_the_flags() {
        let ws = Workspace {
            root: PathBuf::from("/w"),
            target_dir: PathBuf::from("/w/target"),
        };
        let args = |source: &str| ServerArgs {
            source: source.into(),
            files: Vec::new(),
            release_base: cm_payload::RELEASE_BASE.into(),
        };
        assert!(
            args("build")
                .resolve(&ws, &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
        assert!(args("nonsense").resolve(&ws, &BTreeSet::new()).is_err());
    }

    /// `bundle -o` naming its own input is patching in place spelled the long
    /// way, and it has to be *recognised* rather than obeyed — `fs::copy` opens
    /// the destination with `O_TRUNC`, so copying a file onto itself empties it
    /// before a byte is read. Textual comparison would miss every spelling but
    /// the exact one.
    #[test]
    fn a_path_is_recognised_as_itself_however_it_is_spelled() {
        let dir = std::env::temp_dir().join(format!("cm-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cm");
        std::fs::write(&file, b"binary").unwrap();

        assert!(same_file(&file, &file));
        assert!(same_file(&file, &dir.join(".").join("cm")));
        // `..` only resolves through a directory that exists — same as `open(2)`.
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(same_file(&file, &sub.join("..").join("cm")));
        // …and through a symlink, which a textual comparison would never catch.
        #[cfg(unix)]
        {
            let link = dir.join("cm-link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(same_file(&file, &link));
        }

        let other = dir.join("cm-bundled");
        std::fs::write(&other, b"binary").unwrap();
        // Same *contents* is not the same file — only identity counts.
        assert!(!same_file(&file, &other));
        // A destination that doesn't exist yet is the ordinary case.
        assert!(!same_file(&file, &dir.join("not-yet")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The variant that ships and the `bundle` default have to agree: they are
    /// two spellings of "what a release carries".
    #[test]
    fn bundle_defaults_to_what_the_shipping_variant_carries() {
        assert_eq!(find_variant("bundle-linux").unwrap().servers, LINUX_TARGETS);
    }
}
