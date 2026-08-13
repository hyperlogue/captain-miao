# A link farm of `miao-server` binaries, one per target, laid out as
# `<triple>/miao-server` — exactly what the dashboard's `CAPTAIN_MIAO_SERVER_DIR`
# expects.
#
# This is the Nix answer to carrying servers, and it is a better one than
# embedding them: the binaries become store paths shared between dashboard
# generations rather than megabytes compiled into one file, so adding an
# architecture costs a server build instead of relinking `miao`, and two
# generations that carry the same server share it on disk.
#
# `callPackage`d rather than defined inline so `targets` is overridable:
#
#     packages.captain-miao-servers.override {
#       targets = [ "x86_64-unknown-linux-musl" "aarch64-unknown-linux-gnu" ];
#     }
#
# CRITICAL: this must hold `prepare-servers` **cross** builds, never
# `packages.captain-miao-server`. That one is crane-built against the store's own
# glibc with an absolute `/nix/store/.../ld-linux` interpreter — filed under a
# generic triple it would look correct and fail on every non-Nix host, the
# inverse of the failure this whole design started from. It is the right binary
# for exactly one machine: the one that built it, where the Home Manager module
# puts it on PATH and no deploy happens at all. Delegating to `prepare-servers`
# makes the mistake impossible by construction; the dashboard's `PT_INTERP` check
# is the belt.
#
# Same delegation argument as the bundled variants: xtask owns the cross-compile
# strategy, the glibc floor and the arch check, so a nix expression restating them
# would be a second copy free to drift. Hence the same two needs — `devToolchain`
# for the cross `rust-std`s (via `craneLibBundled`), and a writable `HOME`, since
# cargo-zigbuild keeps a cache under it and nix points it at the non-existent
# `/homeless-shelter`.
{
  lib,
  cargo-zigbuild,
  zig,
  craneLibBundled,
  commonArgs,
  # Which servers to build. **The default is deliberately one target, not the
  # four a release publishes.** A static musl x86-64 build runs on any x86-64
  # Linux host whatever its libc — including the NixOS/Alpine/distroless boxes a
  # glibc build cannot start on at all — so it is the single binary that covers
  # the most fleet. Building all four here would triple the build for
  # architectures most people do not have; a host this cannot serve is one
  # `--override` away, and the dashboard can also fetch a published server at
  # deploy time.
  targets ? ["x86_64-unknown-linux-musl"],
}:
assert lib.assertMsg (targets != []) "captain-miao-servers: `targets` must name at least one triple";
  craneLibBundled.buildPackage (commonArgs
    // {
      pname = "captain-miao-servers";
      nativeBuildInputs = [cargo-zigbuild zig];
      preBuild = ''
        export HOME="$TMPDIR"
        export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
      '';
      # `prepare-servers --out` already writes `<target>/miao-server`, which is
      # exactly the layout the directory variable expects — so `$out` is the
      # output directory directly and there is nothing to install afterwards.
      buildPhaseCargoCommand = ''
        cargo run --release --locked -p xtask -- prepare-servers --out "$out" \
          ${lib.concatMapStringsSep " " (t: "--target ${lib.escapeShellArg t}") targets}
      '';
      installPhaseCommand = "true";
      # crane's default install reads a cargo build log to decide which binaries
      # to install. There is no dashboard binary here at all, and the servers are
      # already where they belong, so this has to be off rather than redundant.
      doNotPostBuildInstallCargoBinaries = true;
      doCheck = false;
      # So a consumer can report what it wrapped without re-deriving the default.
      passthru.serverTargets = targets;
      meta.description = "miao-server binaries for ${lib.concatStringsSep ", " targets}";
    })
