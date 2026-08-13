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

      # `drives_a_real_tmux_server`, the one backend test that drives a real
      # server rather than a fixture — and the only one that *can*, since a
      # server on a private socket is its whole dependency (kitty and Ghostty
      # both need a window system, Ghostty additionally a hand-clicked macOS
      # Automation grant). It is in the shell rather than left to the machine
      # because CI runs every check through `nix develop`, so an ambient tmux
      # would make the test pass or vanish depending on the runner image.
      tmux
    ];

    env = {};

    shellHook = ''
      export CARGO_TARGET_DIR="$PWD/target"
    '';
  }
