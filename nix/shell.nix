{
  pkgs,
  rustToolchain,
}: let
  inherit (pkgs) stdenv lib;
  mkShell =
    if stdenv.hostPlatform.isLinux
    then
      pkgs.mkShell.override {
        stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.clangStdenv;
      }
    else pkgs.mkShell;
in
  mkShell {
    name = "captain-miao-dev";

    nativeBuildInputs = [
      rustToolchain
    ];

    packages = with pkgs; [
      rust-analyzer
      cargo-edit
      cargo-watch

      # `cargo xtask prepare-servers` (and `dist`'s default `--from build`)
      # cross-compiles miao-server for the dashboards that carry one. zig
      # is what makes that work at all: the server pulls in bundled SQLite's C
      # amalgamation, so a cross needs a C cross-compiler and a target libc, and
      # zig ships both for every glibc version it supports. Without these two
      # only the *host* target builds, natively and against this machine's glibc,
      # and xtask says what to install for the others — or you sidestep the
      # question entirely with `--from release`, which needs neither.
      cargo-zigbuild
      zig

      # for kitty remote control during development
      kitty
    ];

    env = {};

    shellHook = ''
      export CARGO_TARGET_DIR="$PWD/target"
    '';
  }
