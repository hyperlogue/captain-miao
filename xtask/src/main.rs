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

/// The size-tuned dashboard profile, defined in the workspace root `Cargo.toml`.
/// Load-bearing twice, exactly like `server::SERVER_PROFILE`: it is the
/// `--profile` argument *and* the directory cargo writes the artifact into.
const DASHBOARD_SMALL_PROFILE: &str = "dashboard-small";

const X86: &str = "x86_64-unknown-linux-gnu";
const ARM: &str = "aarch64-unknown-linux-gnu";
const X86_MUSL: &str = "x86_64-unknown-linux-musl";
const ARM_MUSL: &str = "aarch64-unknown-linux-musl";

/// The glibc pair. Not a shipping set on its own any more — [`SHIPPING_VARIANT`]
/// carries [`X86`] alone — but kept as the set a `bundle-linux` build embeds and
/// as the floor [`PUBLISHED_TARGETS`] must stay a superset of.
///
/// Deliberately *not* the same set as [`PUBLISHED_TARGETS`], and the two must
/// not be re-merged. The default download embeds no musl: musl's audience is
/// hosts with no generic loader (NixOS, Alpine, distroless), and those have a
/// better answer already — a server built against their own libc, on their own
/// PATH, with no deploy at all. Making every downloader carry ~6 MiB aimed at
/// the one platform that doesn't need it is the wrong default. Such a host is
/// reached by *downloading* the published musl asset instead — what
/// [`PUBLISHED_TARGETS`] exists for — or by taking [`ALL_SERVER_VARIANT`].
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
    /// Build the *dashboard* for size as well as the servers it carries: the
    /// `dashboard-small` profile, plus the SQLite feature trim `cm-core` also
    /// benefits from.
    ///
    /// Off for everything a release publishes. The servers are size-tuned
    /// regardless — that happens in `server::build`, below this flag — so this
    /// is only ever a statement about the `miao` binary itself.
    size_tuned: bool,
    what: &'static str,
}

const VARIANTS: &[Variant] = &[
    Variant {
        name: "",
        features: &[],
        servers: &[],
        size_tuned: false,
        what: "the default: local sessions, plus remote hosts that already have miao-server",
    },
    // No `remote` variant: the feature is on by default since 0.3.0, so such a
    // build would be byte-identical to the plain one and only confuse `--list`.
    // The plain build reaches remote hosts that already have `miao-server`; the
    // bundles below are for hosts that don't.
    Variant {
        name: "bundle-linux-x86_64",
        features: &[],
        servers: &[X86],
        size_tuned: false,
        what: "remote hosts, deploying its own server to x86-64 Linux",
    },
    Variant {
        name: "bundle-linux-aarch64",
        features: &[],
        servers: &[ARM],
        size_tuned: false,
        what: "remote hosts, deploying its own server to arm64 Linux",
    },
    Variant {
        name: "bundle-linux",
        features: &[],
        servers: LINUX_TARGETS,
        size_tuned: false,
        what: "remote hosts, deploying its own server to either Linux arch",
    },
    Variant {
        name: "bundle-linux-all",
        features: &[],
        servers: PUBLISHED_TARGETS,
        size_tuned: false,
        what: "every published Linux server (gnu + musl) — reaches any host without a download",
    },
    // Not published, and not a default: `"s"` on the dashboard's own render and
    // session-parsing paths has never been benchmarked, so it is opt-in. It
    // exists for builds where the artifact's size *is* the point — the Nix
    // `captain-miao-bundle-small` package, where there is no download to amortise
    // a bigger binary against.
    Variant {
        name: "bundle-small",
        features: &[],
        servers: &[X86],
        size_tuned: true,
        what: "smallest bundled build: the dashboard is size-tuned too, not just its server",
    },
];

/// The variant **every published dashboard download is built from** — GitHub
/// tarballs and all four npm platform packages alike.
///
/// One server, x86-64 glibc, on every host platform including the macOS ones:
/// the payload's target is the *remote host's*, not the laptop's, and x86-64
/// glibc is overwhelmingly what a remote host runs. Anything else is a download
/// away at deploy time, which is the trade — ~2.8 MiB on every install against a
/// one-time fetch for the minority host.
const SHIPPING_VARIANT: &str = "bundle-linux-x86_64";

/// The extra GitHub-only artifact, carrying every server a release publishes.
///
/// For the air-gapped case and for anyone driving a mixed fleet: it deploys to
/// arm64 and to musl hosts with no network fetch at all. Roughly 2.5× the
/// shipping download, which is exactly why it is a separate asset rather than
/// the default — and why it never goes to npm.
const ALL_SERVER_VARIANT: &str = "bundle-linux-all";

/// What `dist` builds when asked for nothing in particular: precisely what a
/// release publishes. The plain build is deliberately absent — it is what a bare
/// `cargo build` already produces, and nothing has shipped it since 0.4.0.
const DEFAULT_VARIANTS: &[&str] = &[SHIPPING_VARIANT, ALL_SERVER_VARIANT];

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

    /// The cargo profile this variant's **dashboard** compiles under. Also names
    /// the directory cargo writes it to, which is why [`built_dashboard`] takes
    /// it rather than assuming `release`.
    fn dashboard_profile(&self) -> &'static str {
        if self.size_tuned {
            DASHBOARD_SMALL_PROFILE
        } else {
            "release"
        }
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
    /// Variant to build (repeatable). Defaults to what a release publishes.
    /// Use `--list` to see them all.
    #[arg(long = "variant", value_name = "NAME")]
    variants: Vec<String>,
    /// Build every variant.
    #[arg(long, conflicts_with = "variants")]
    all: bool,
    /// Print the variants and exit.
    #[arg(long)]
    list: bool,
    /// Compile the dashboard for this target instead of the host.
    ///
    /// Only the *dashboard* — the servers a variant carries are named by the
    /// variant and obtained through `--from`, and are unrelated to the machine
    /// the dashboard runs on. Release CI needs this for x86-64 macOS, which is
    /// cross-built on an arm64 runner.
    #[arg(long, value_name = "TRIPLE")]
    target: Option<String>,
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

    // Whether the artifact will run here. A cross-built dashboard cannot be
    // executed, so `verify` falls back to reading its bytes — see there.
    let runnable = match &args.target {
        None => true,
        Some(t) => server::host_triple().map(|h| &h == t).unwrap_or(false),
    };

    let mut built = Vec::new();
    for v in &wanted {
        println!("\n▶ {}", v.label());
        let payloads = pick_payloads(&servers, v.servers)?;

        build_dashboard(ws, v, &payloads, args.target.as_deref())?;

        let to = dist_dir.join(v.artifact());
        // Copy rather than rename: every variant's build reuses the same
        // `target/release/cm`, so leaving it in place would be a lie the moment
        // the next one overwrote it.
        let from = built_dashboard(
            &ws.target_dir,
            args.target.as_deref(),
            v.dashboard_profile(),
        );
        install(&from, &to)?;

        verify(&to, &payloads, runnable)?;
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

fn build_dashboard(
    ws: &Workspace,
    v: &Variant,
    payloads: &[&server::Payload],
    target: Option<&str>,
) -> Result<()> {
    let features = v.cargo_features();
    // `--profile <name>` rather than `--release`, which is only its alias and
    // cannot be passed alongside it.
    let mut argv = vec![
        "build".to_string(),
        "--profile".to_string(),
        v.dashboard_profile().to_string(),
        "--locked".to_string(),
        "-p".to_string(),
        DASHBOARD_PKG.to_string(),
    ];
    if let Some(t) = target {
        argv.push("--target".to_string());
        argv.push(t.to_string());
    }
    if !features.is_empty() {
        argv.push("--features".to_string());
        argv.push(features.join(","));
    }

    let manifest = write_manifest(ws, v, payloads)?;
    println!("  {} {}", cargo(), argv.join(" "));
    let mut cmd = Command::new(cargo());
    cmd.args(&argv)
        .current_dir(&ws.root)
        // Always set, even to the empty-manifest path: a stale value inherited
        // from the caller's environment would otherwise decide what a variant
        // carries, and a plain `miao` would quietly stop being plain.
        .env(MANIFEST_ENV, &manifest);
    // The dashboard links the same bundled SQLite as the server, through
    // `cm-core`, and reaches no more of it — one read-only SELECT for Codex
    // thread titles. Only for a size-tuned variant, so every other artifact
    // stays byte-for-byte what it was.
    if v.size_tuned {
        cmd.env("LIBSQLITE3_FLAGS", server::SQLITE_TRIM);
    }
    let status = cmd.status().context("spawning cargo")?;
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

/// Where cargo leaves the dashboard it just built.
///
/// Passing `--target` moves the output under a triple-named directory — even
/// when that triple *is* the host, which is the case worth stating: there is no
/// "same as host, so same path" shortcut, and reading `target/release/miao` after
/// a `--target` build silently picks up whatever an earlier plain build left
/// there. Pure, so the layout is pinned by a test rather than by a CI run that
/// packaged a stale binary.
fn built_dashboard(target_dir: &Path, target: Option<&str>, profile: &str) -> PathBuf {
    let mut p = target_dir.to_path_buf();
    if let Some(t) = target {
        p.push(t);
    }
    p.join(profile).join(DASHBOARD_BIN)
}

/// Check the artifact really carries what it was built to carry.
///
/// A manifest reaches the dashboard through an environment variable and a
/// generated file, which is exactly the kind of seam that fails silently: a
/// variable that didn't survive, a manifest naming an archive that moved, and the
/// build succeeds carrying nothing. Something has to look, or a hollow bundle
/// ships.
///
/// Running the artifact is the better check and is used whenever it *can* run —
/// it exercises the real accessor, not just the presence of some bytes. A
/// cross-built dashboard can't be executed here, so the fallback searches the
/// image for each payload's SHA-256, which survives `strip = true` because it is
/// a `&'static str` in `.rodata` rather than a symbol. That is weaker (it proves
/// the table was populated, not that the binary starts) but it is the difference
/// between checking the cross build and not checking it, and CI still runs the
/// native artifacts.
fn verify(artifact: &Path, payloads: &[&server::Payload], runnable: bool) -> Result<()> {
    if !runnable {
        if payloads.is_empty() {
            println!("  cross-built and carries nothing — nothing to verify");
            return Ok(());
        }
        let image =
            std::fs::read(artifact).with_context(|| format!("reading {}", artifact.display()))?;
        for p in payloads {
            let needle = p.sha256.as_bytes();
            if !image.windows(needle.len()).any(|w| w == needle) {
                bail!(
                    "{} was built to carry {} but its image does not contain that payload's \
                     digest ({}…) — the manifest did not reach the build",
                    artifact.display(),
                    p.target,
                    &p.sha256[..12],
                );
            }
        }
        println!(
            "  cross-built: {} payload digest(s) present",
            payloads.len()
        );
        return Ok(());
    }

    let out = Command::new(artifact)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", artifact.display()))?;
    if !out.status.success() {
        bail!("{} does not run ({})", artifact.display(), out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for p in payloads {
        if !text.contains(&p.target) {
            bail!(
                "{} was built to carry {} but does not report it:\n{}",
                artifact.display(),
                p.target,
                text.trim()
            );
        }
    }
    if payloads.is_empty() && !text.contains("none") {
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
        install(&p.bin_path, &to)?;
        println!(
            "  {}  {}  {}",
            to.display(),
            server::human(p.raw_len),
            &p.sha256[..12]
        );
    }
    Ok(())
}

/// Put `from` at `to` as a **new file**, never by rewriting the one already
/// there.
///
/// The rename is not about atomicity, it is about the inode. macOS binds an
/// executable's code signature to the vnode it ran from, and rewriting that same
/// inode with a differently-signed image can leave the stale blob attached: every
/// later exec of the path dies with `SIGKILL (Code Signature Invalid)` — kernel
/// log `load code signature error 2`, `codesign -v` still says the file is fine —
/// while the identical bytes run from any other path. Both callers hit exactly
/// the pattern that binds it: they rebuild onto a fixed name, and [`verify`] runs
/// each artifact the moment it lands. Landing a fresh inode each time sidesteps
/// the cache, and it also *cures* a path already poisoned by an older build, so
/// nobody has to learn any of this to get unstuck.
///
/// Both artifacts are executables, so this chmods as well: cargo already produces
/// 0755, but a fetched or hand-supplied binary may have arrived without it, and
/// `prepare-servers` output is what goes into a release tarball. Doing it before
/// the rename means the final path is never briefly non-executable.
fn install(from: &Path, to: &Path) -> Result<()> {
    let name = to
        .file_name()
        .ok_or_else(|| anyhow!("{} names no file", to.display()))?;
    let tmp = to.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
    set_executable(&tmp)?;
    std::fs::rename(&tmp, to)
        .with_context(|| format!("renaming {} to {}", tmp.display(), to.display()))?;
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
            // Carrying a server implies the gate that reaches it. Still asserted
            // now that `remote` is a default feature: `cargo_features` names it
            // explicitly so a bundle keeps working if the default ever changes.
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

    /// The default download carries **exactly one** server, x86-64 glibc.
    ///
    /// Every published artifact is built from this variant — four GitHub
    /// tarballs and four npm packages — so a second payload here is ~2.8 MiB
    /// added to every install captain-miao has, including the macOS ones that
    /// would be carrying a Linux binary purely on the chance a host needs it.
    /// Widening the fleet is [`ALL_SERVER_VARIANT`]'s job, not this one's.
    #[test]
    fn the_shipping_variant_carries_exactly_one_glibc_server() {
        assert_eq!(find_variant(SHIPPING_VARIANT).unwrap().servers, &[X86]);
    }

    /// Only the explicitly-named all-server artifact may embed a musl server.
    ///
    /// The two target sets were one const until musl arrived, and re-merging
    /// them is the easy mistake: it silently adds ~6 MiB to every download for
    /// a payload aimed at the one platform that is better served without a
    /// deploy at all. The reverse mistake — dropping musl from what we publish —
    /// is caught by the test below, since the shipping variant embeds none of
    /// them and the runtime downloader is the only other thing that can reach
    /// them.
    #[test]
    fn only_the_all_server_variant_embeds_a_musl_server() {
        for name in DEFAULT_VARIANTS {
            if *name == ALL_SERVER_VARIANT {
                continue;
            }
            let v = find_variant(name).unwrap();
            assert!(
                !v.servers.iter().any(|t| t.contains("-musl")),
                "release variant {:?} embeds a musl server: {:?}",
                v.label(),
                v.servers
            );
        }
        // The all-server artifact is the one place musl *is* carried, and it is
        // a separate download precisely so nobody pays for it by default.
        let all = find_variant(ALL_SERVER_VARIANT).unwrap();
        assert!(all.servers.iter().any(|t| t.contains("-musl")));
        assert_eq!(all.servers, PUBLISHED_TARGETS);
    }

    /// `--target` relocates cargo's output, and the packaged binary comes from
    /// wherever this says. Getting it wrong does not fail the build — it ships
    /// whatever an earlier plain build left in `target/release/`, which on a CI
    /// runner is nothing and locally is a stale unbundled dashboard.
    #[test]
    fn the_built_dashboard_moves_under_the_target_triple() {
        let td = Path::new("/w/target");
        assert_eq!(
            built_dashboard(td, None, "release"),
            Path::new("/w/target/release/miao")
        );
        // Including when the requested target *is* the host: cargo relocates on
        // the presence of the flag, not on whether it changes anything.
        assert_eq!(
            built_dashboard(td, Some(X86), "release"),
            Path::new("/w/target/x86_64-unknown-linux-gnu/release/miao")
        );
        // And the profile names the directory too, so a size-tuned variant is
        // read back from its own — never from a `release/` an earlier build left.
        assert_eq!(
            built_dashboard(td, Some(X86), DASHBOARD_SMALL_PROFILE),
            Path::new("/w/target/x86_64-unknown-linux-gnu/dashboard-small/miao")
        );
    }

    /// The size-tuned variant must differ from every shipping one in both ways
    /// that matter, and nothing a release publishes may pick either up.
    ///
    /// `"s"` on the dashboard is unbenchmarked, so it stays opt-in: a variant
    /// that quietly acquired it would ship a slower TUI to every npm install.
    #[test]
    fn only_the_opt_in_variant_is_size_tuned() {
        let small = find_variant("bundle-small").unwrap();
        assert!(small.size_tuned);
        assert_eq!(small.dashboard_profile(), DASHBOARD_SMALL_PROFILE);
        assert!(small.bundles(), "a size-tuned build is still a bundle");

        for name in DEFAULT_VARIANTS {
            let v = find_variant(name).unwrap();
            assert!(
                !v.size_tuned,
                "published variant {:?} is size-tuned",
                v.label()
            );
            assert_eq!(v.dashboard_profile(), "release");
        }
        assert!(!DEFAULT_VARIANTS.contains(&"bundle-small"));
    }

    /// The cross-build half of [`verify`], which nothing but release CI's
    /// x86-64 macOS leg exercises — and which fails *open* if it is wrong, since
    /// a scan that never matches and a scan that always matches both look like a
    /// green build until someone downloads the artifact.
    #[test]
    fn the_cross_check_finds_a_payload_digest_and_notices_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("xtask-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("miao");

        let payload = |sha: &str| server::Payload {
            target: X86.to_string(),
            sha256: sha.to_string(),
            bin_path: artifact.clone(),
            gz_path: artifact.clone(),
            raw_len: 0,
            gz_len: 0,
            provenance: server::Provenance::Local,
            repacked: false,
        };
        let present = payload(&"a1".repeat(32));
        let absent = payload(&"b2".repeat(32));

        // The digest lives in `.rodata` surrounded by unrelated bytes, so the
        // scan has to find it mid-image rather than at a known offset.
        let mut image = vec![0u8; 4096];
        image.extend_from_slice(present.sha256.as_bytes());
        image.extend(std::iter::repeat_n(0u8, 4096));
        std::fs::write(&artifact, &image).unwrap();

        verify(&artifact, &[&present], false).expect("digest is present");
        let err = verify(&artifact, &[&present, &absent], false)
            .expect_err("a payload whose digest never made it in must fail");
        assert!(format!("{err}").contains(&absent.sha256[..12]), "{err}");

        // Carrying nothing is the one case with nothing to look for; it must not
        // be reported as a failure.
        verify(&artifact, &[], false).expect("no payloads, nothing to verify");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The plain build must never come back as a published artifact: since 0.4.0
    /// every download is bundled, and a plain tarball under the same name would
    /// silently strip the deploy path from whoever grabbed it.
    #[test]
    fn no_default_variant_is_the_plain_build() {
        for name in DEFAULT_VARIANTS {
            let v = find_variant(name).unwrap();
            assert!(
                v.bundles(),
                "release variant {:?} carries no server",
                v.label()
            );
        }
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

    /// The whole point of [`install`]: the artifact that lands is a *different
    /// file* from the one it replaced, because macOS keeps a signature bound to
    /// the old inode and kills every exec of a path rewritten in place. Asserted
    /// on the inode rather than the bytes — the bytes were never the part that
    /// went wrong.
    #[test]
    fn installing_over_an_artifact_replaces_the_inode() {
        use std::os::unix::fs::MetadataExt;

        let dir = std::env::temp_dir().join(format!("xtask-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new, to) = (dir.join("old"), dir.join("new"), dir.join("artifact"));
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();

        install(&old, &to).unwrap();
        let first = std::fs::metadata(&to).unwrap().ino();
        install(&new, &to).unwrap();
        let second = std::fs::metadata(&to).unwrap();

        assert_ne!(first, second.ino(), "install rewrote the artifact in place");
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert_eq!(second.mode() & 0o777, 0o755, "artifact is not executable");
        // And it leaves nothing behind to be mistaken for an artifact.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n.to_string_lossy().starts_with('.'))
            .collect();
        assert!(strays.is_empty(), "install left {strays:?} behind");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
