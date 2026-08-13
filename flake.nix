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

        # The targets `captain-miao-servers` can cross-compile a `miao-server`
        # to (docs/crate-split.md). Only `rust-std` comes from here — the C half
        # of the cross (bundled SQLite's amalgamation, and the link itself) is
        # `cargo-zigbuild`'s job, which ships its own libc headers and linker
        # per target.
        #
        # Deliberately layered on *top* of `rustToolchain` rather than folded
        # into it: crane's package builds take `rustToolchain`, so the artifacts
        # `nix build` produces stay byte-identical whether or not the cross
        # targets are installed.
        # The musl pair is here because a static build is the only server that
        # runs on a host with no generic loader (NixOS, Alpine, distroless),
        # where the gnu build cannot start at all — and musl x86-64 is what
        # `captain-miao-servers` builds *by default*, so this is not a
        # contingency but the common path here. All four are listed because that
        # package's `targets` is overridable to any of them.
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
        # Nothing here embeds a server payload: every package builds with
        # CM_SERVER_PAYLOAD_MANIFEST unset, so `build.rs` embeds nothing. Nix
        # reaches remote hosts through `captain-miao-with-servers` instead, which
        # points the dashboard at a directory of servers rather than compiling
        # them in — see `nix/servers.nix` for why that is the better trade here.
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

        # The per-host daemon + pty pool, **for this machine only**. A separate
        # workspace member (libshpool lives only here), so scope the build to it
        # with `-p`; reuses the shared dependency artifacts.
        #
        # This and `captain-miao-servers` are the same program built two ways,
        # and confusing them is the mistake worth naming. This one is an ordinary
        # nixpkgs build: `rustToolchain`, plain `cargo build --release`, linked
        # against the store's own glibc with an absolute `/nix/store/…/ld-linux`
        # interpreter. That is exactly right for its job — the Home Manager
        # module putting `miao-server` on *this* machine's PATH, where a
        # dashboard finds it locally and no deploy happens at all — and exactly
        # wrong anywhere else, because that loader exists on no other host.
        #
        # Anything a dashboard *deploys* must come from `captain-miao-servers`
        # instead, which cross-builds through zigbuild against a pinned glibc
        # floor and asserts it did (`nix/servers.nix`). Filed under a generic
        # triple, this binary would look correct and fail on every non-Nix host —
        # the inverse of the failure the whole deploy design started from.
        #
        # It also stays on plain `release`, not the size-tuned `server-release`
        # that `xtask` builds published payloads with: that profile exists to
        # shrink a *download*, and there is no download here.
        captain-miao-server = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            pname = "captain-miao-server";
            cargoExtraArgs = "--locked -p captain-miao-server";
            meta.mainProgram = "miao-server";
          });
        # `devToolchain`, for the cross `rust-std`s a deployable server needs — a
        # second craneLib, leaving the plain packages on `rustToolchain` so their
        # output stays byte-identical. Those builds also need a writable `HOME`:
        # cargo-zigbuild keeps a cache under it and nix points it at the
        # non-existent `/homeless-shelter`, so the cross fails on a permission
        # error before zig is ever invoked.
        craneLibCross = (crane.mkLib pkgs).overrideToolchain devToolchain;

        # A link farm of servers, and (below) a dashboard pointed at it — the
        # recommended way to drive remote hosts from Nix.
        #
        # Both are `callPackage`d rather than defined here so their server list is
        # overridable; the reasoning and the default live in `nix/servers.nix`.
        #
        #     packages.captain-miao-with-servers.override {
        #       targets = [ "x86_64-unknown-linux-musl" "aarch64-unknown-linux-gnu" ];
        #     }
        captain-miao-servers = pkgs.callPackage ./nix/servers.nix {
          inherit craneLibCross commonArgs;
        };

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

        captain-miao-with-servers = pkgs.callPackage ./nix/with-servers.nix {
          inherit captain-miao-remote captain-miao-servers;
        };
      in {
        packages = {
          default = captain-miao;
          inherit captain-miao captain-miao-server captain-miao-remote captain-miao-servers captain-miao-with-servers;
        };

        devShells.default = import ./nix/shell.nix {
          inherit pkgs;
          rustToolchain = devToolchain;
        };

        formatter = pkgs.alejandra;
      };
    };
}
