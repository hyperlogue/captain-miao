{
  inputs = {
    nixpkgs.url = "nixpkgs/nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs @ {
    self,
    nixpkgs,
    flake-parts,
    fenix,
    crane,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-darwin"];

      # System-independent, so outside `perSystem`: the module resolves its
      # package from `self.packages.${pkgs.system}` at evaluation time.
      flake.homeManagerModules = let
        module = import ./nix/home-manager.nix self;
      in {
        captain-miao = module;
        default = module;
      };

      perSystem = {
        pkgs,
        system,
        ...
      }: let
        inherit (pkgs) lib;

        rustToolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ];
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # The targets a bundled dashboard cross-compiles the server to
        # (docs/crate-split.md). Only `rust-std` comes from here — the C half of
        # the cross (bundled SQLite's amalgamation, and the link itself) is
        # `cargo-zigbuild`'s job, which ships its own libc headers and linker
        # per target.
        #
        # Deliberately layered on *top* of `rustToolchain` rather than folded
        # into it: crane's package builds take `rustToolchain`, so the artifacts
        # `nix build` produces stay byte-identical whether or not the cross
        # targets are installed.
        # The musl pair is here because a static build is the only server that
        # runs on a host with no generic loader (NixOS, Alpine, distroless),
        # where the gnu build cannot start at all. The default download does not
        # carry them — a release publishes them as assets and a dashboard fetches
        # one when it meets such a host — but `prepare-servers` builds all four
        # and the `bundle-linux-all` artifact embeds them, so the toolchain has to
        # be able to.
        crossTargets = [
          "x86_64-unknown-linux-gnu"
          "aarch64-unknown-linux-gnu"
          "x86_64-unknown-linux-musl"
          "aarch64-unknown-linux-musl"
        ];
        devToolchain = fenix.packages.${system}.combine (
          [rustToolchain]
          ++ map (t: fenix.packages.${system}.targets.${t}.stable.rust-std) crossTargets
        );

        # `craneLib.cleanCargoSource` keeps only `.rs` / `.toml` / `Cargo.lock`,
        # which drops `assets/logo/*.gray` — the masks `src/app/logo.rs` embeds
        # with `include_bytes!`, so the build fails at compile time without
        # them. A source filter can't see a compile-time file dependency, so
        # keep `assets/` explicitly alongside the Rust sources.
        #
        # Nothing extra is needed for the embedded server payloads: these
        # packages build with CM_SERVER_PAYLOAD_MANIFEST unset, so `build.rs`
        # embeds nothing. The bundled variants below cross-build servers and pass
        # a manifest naming them, which works offline — every cargo invocation
        # resolves from the vendored registry crane sets up for the outer build.
        src = let
          root = ./.;
          isAsset = path: lib.hasPrefix "${toString root}/assets/" path;
        in
          lib.cleanSourceWith {
            src = lib.cleanSource root;
            # Reproducible regardless of the checkout's directory name.
            name = "source";
            filter = path: type: isAsset path || craneLib.filterCargoSources path type;
          };

        commonArgs = {
          inherit src;
          strictDeps = true;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Root package: the `captain-miao` dashboard (crane infers it from the
        # workspace root, so this builds the dashboard + its `cm-core` dep).
        # Its binary is `miao`, not the package name, so `meta.mainProgram` has to
        # say so — otherwise `nix run` looks for `$out/bin/captain-miao`.
        captain-miao = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            meta.mainProgram = "miao";
          });

        # The per-host daemon + pty pool, deployed to remote hosts. A separate
        # workspace member (libshpool lives only here), so scope the build/test
        # to it with `-p`; reuses the shared dependency artifacts.
        #
        # Deliberately left on plain `release`, not the size-tuned
        # `server-release` that `xtask` builds published payloads with. That
        # profile exists to shrink a *download* — every npm install and GitHub
        # tarball carries a server — and a Nix user builds from source, so it
        # would be trading throughput for bytes nobody transfers. The two
        # binaries are otherwise identical; nothing about the wire protocol or
        # behaviour depends on the profile. (`captain-miao-servers` below goes
        # through `prepare-servers`, so those *are* `server-release` — they are
        # payloads a dashboard deploys, which is the case the profile is for.)
        captain-miao-server = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            pname = "captain-miao-server";
            cargoExtraArgs = "--locked -p captain-miao-server";
            meta.mainProgram = "miao-server";
          });
        # The dashboard variants that carry a `miao-server` to deploy to
        # a remote host — one package per variant `cargo xtask dist` knows about
        # (docs/crate-split.md).
        #
        # These delegate to `xtask` rather than reimplementing it in nix: it
        # cross-builds the servers, writes the manifest naming them, and builds a
        # dashboard that embeds it. A nix expression would be a second copy of
        # that, free to drift. The whole sequence runs offline — every cargo
        # invocation resolves from the vendored registry crane already set up,
        # which is also why these pin `--from build` (the default) rather than
        # offering the `release` source: a nix build has no network to fetch one
        # over.
        #
        # Two things they need that a plain build doesn't. `devToolchain`, for
        # the cross `rust-std`s — hence a second craneLib, leaving the plain
        # packages on `rustToolchain` so their output is untouched. And a
        # writable `HOME`: cargo-zigbuild keeps a cache under it and nix points it
        # at the non-existent `/homeless-shelter`, so the cross fails on a
        # permission error before zig is ever invoked.
        craneLibBundled = (crane.mkLib pkgs).overrideToolchain devToolchain;
        mkBundled = variant:
          craneLibBundled.buildPackage (commonArgs
            // {
              pname = "captain-miao-${variant}";
              nativeBuildInputs = [pkgs.cargo-zigbuild pkgs.zig];
              preBuild = ''
                export HOME="$TMPDIR"
                export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
              '';
              buildPhaseCargoCommand = ''
                cargo run --release --locked -p xtask -- dist --variant ${variant}
              '';
              # `dist` names its artifacts, and the binary inside is `miao`.
              installPhaseCommand = ''
                install -Dm755 dist/miao-${variant} "$out/bin/miao"
              '';
              # crane's default install reads a cargo build log to decide which
              # binaries to install. There isn't one here — the artifact comes
              # out of `dist/`, already patched — and installing straight from
              # `target/release` would give an *unbundled* `miao`, so this has to
              # be off rather than merely redundant.
              doNotPostBuildInstallCargoBinaries = true;
              # `xtask dist` already runs the artifact and checks it reports the
              # servers it was built to carry — the check that catches a manifest
              # that silently didn't reach the compile.
              doCheck = false;
              meta.mainProgram = "miao";
            });
        # `bundle-linux-x86_64` is what a release ships for every host target;
        # `bundle-linux-all` is the separate all-server download. The other two
        # are not published but stay buildable — a Nix user driving a fleet of one
        # architecture has no reason to carry the other.
        bundled = lib.genAttrs [
          "bundle-linux"
          "bundle-linux-x86_64"
          "bundle-linux-aarch64"
          "bundle-linux-all"
        ] (variant: mkBundled variant);

        # A link farm of servers, and a dashboard pointed at it.
        #
        # With the source chain's directory variable, a Nix-built dashboard needs
        # no embedded payloads at all — which is strictly better on this path than
        # embedding: the servers become store paths shared between dashboard
        # generations rather than megabytes recompiled into one binary, and adding
        # an architecture doesn't relink `miao`.
        #
        # CRITICAL: this must hold `prepare-servers` **cross** builds, never
        # `packages.captain-miao-server`. That one is crane-built against the
        # store's own glibc with an absolute /nix/store/.../ld-linux interpreter —
        # filed under a generic triple it would look correct and fail on every
        # non-Nix host, the inverse of the failure this whole design started from.
        # It is the right binary for exactly one machine: the one that built it,
        # where the Home Manager module puts it on PATH and no deploy happens at
        # all. Delegating to `prepare-servers` makes the mistake impossible by
        # construction; the dashboard's PT_INTERP check is the belt.
        #
        # Same delegation argument as mkBundled: xtask owns the cross-compile
        # strategy, the glibc floor and the arch check, so a nix expression
        # restating them would be a second copy free to drift. Hence the same two
        # needs — devToolchain for the cross rust-stds, and a writable HOME for
        # cargo-zigbuild's cache.
        captain-miao-servers = craneLibBundled.buildPackage (commonArgs
          // {
            pname = "captain-miao-servers";
            nativeBuildInputs = [pkgs.cargo-zigbuild pkgs.zig];
            preBuild = ''
              export HOME="$TMPDIR"
              export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
            '';
            # `prepare-servers --out` already writes <target>/miao-server, which
            # is exactly the layout the directory variable expects.
            buildPhaseCargoCommand = ''
              cargo run --release --locked -p xtask -- prepare-servers --out "$out"
            '';
            installPhaseCommand = "true";
            doNotPostBuildInstallCargoBinaries = true;
            doCheck = false;
          });

        # The dashboard with the remote-hosts gate on. Required by the wrapper
        # below and not merely nice to have: `REMOTE_ENABLED` is
        # `cfg!(feature = "remote")`, so a plain build never reads `hosts.json`
        # and never constructs a remote backend — it would carry a link farm it
        # has no code path to reach, and the whole package would be inert.
        captain-miao-remote = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            pname = "captain-miao-remote";
            cargoExtraArgs = "--locked --features remote";
            meta.mainProgram = "miao";
          });

        captain-miao-with-servers = pkgs.symlinkJoin {
          name = "captain-miao-with-servers";
          paths = [captain-miao-remote];
          nativeBuildInputs = [pkgs.makeWrapper];
          # `--set-default`, not `--set`: the chain's whole premise is that
          # explicit configuration beats a build-time default, so a user who
          # exports their own CAPTAIN_MIAO_SERVER_DIR must still win. The
          # per-target variable overrides either way.
          postBuild = ''
            wrapProgram $out/bin/miao \
              --set-default CAPTAIN_MIAO_SERVER_DIR ${captain-miao-servers}
          '';
          meta.mainProgram = "miao";
        };
      in {
        packages =
          {
            default = captain-miao;
            inherit captain-miao captain-miao-server captain-miao-remote captain-miao-servers captain-miao-with-servers;
          }
          # `captain-miao-bundle-linux`, and the two single-arch variants.
          // lib.mapAttrs' (feature: pkg:
            lib.nameValuePair "captain-miao-${feature}" pkg)
          bundled;

        devShells.default = import ./nix/shell.nix {
          inherit pkgs;
          rustToolchain = devToolchain;
        };

        formatter = pkgs.alejandra;
      };
    };
}
