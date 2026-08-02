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

      perSystem = {
        pkgs,
        system,
        ...
      }: let
        rustToolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ];
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Root package: the `captain-miao` dashboard (crane infers it from the
        # workspace root, so this builds the dashboard + its `cm-core` dep).
        captain-miao = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
          });

        # The per-host daemon + pty pool, deployed to remote hosts. A separate
        # workspace member (libshpool lives only here), so scope the build/test
        # to it with `-p`; reuses the shared dependency artifacts.
        captain-miao-server = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            pname = "captain-miao-server";
            cargoExtraArgs = "--locked -p captain-miao-server";
          });
      in {
        packages = {
          default = captain-miao;
          inherit captain-miao captain-miao-server;
        };

        devShells.default = import ./nix/shell.nix {inherit pkgs rustToolchain;};

        formatter = pkgs.alejandra;
      };
    };
}
