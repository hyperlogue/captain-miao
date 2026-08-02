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

      # for kitty remote control during development
      kitty
    ];

    env = {};

    shellHook = ''
      export CARGO_TARGET_DIR="$PWD/target"
    '';
  }
