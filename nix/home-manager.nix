# Home Manager module: put captain-miao's binaries on the user's PATH.
#
# The server half is the one that earns this module. A remote dashboard needs a
# `miao-server` it can run on the host, and the deploy path — push our own
# build to `~/.cache/captain-miao/bin/` — cannot serve a NixOS host at all: a
# generic-glibc binary has no loader there, which the deploy's run-it-on-the-host
# check correctly refuses to install. A server the host builds *for itself* has
# none of that problem, and the provisioning rule already prefers it: **PATH is
# the user's, the cache path is ours** (docs/crate-split.md). So on a
# Home-Manager host, `programs.captain-miao.server.enable = true` is the whole
# remote-host setup.
#
# Two things this deliberately does not do:
#
#   * **No systemd user service for the daemon.** It self-daemonizes on demand —
#     the dashboard's connect runs `miao-server daemon ensure`, which is
#     idempotent — and it auto-exits when idle. A unit would only duplicate a
#     lifecycle the protocol already owns.
#   * **No lingering.** For the daemon (and so every pooled session) to survive
#     your last logout, the *system* needs `users.users.<name>.linger = true`
#     — systemd-logind otherwise tears down `/run/user/<uid>` behind it. That is
#     a NixOS option, not a Home-Manager one, so it stays your call to set.
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.captain-miao;
  system = pkgs.stdenv.hostPlatform.system;
  ours = self.packages.${system} or {};
  # The flake builds for the systems it declares; anything else should say so
  # plainly rather than surface as "attribute 'captain-miao' missing".
  pick = name:
    ours.${
      name
    }
    or (throw ''
      captain-miao has no ${name} package for ${system}.
      Set programs.captain-miao.${
        if name == "captain-miao-server"
        then "server.package"
        else "package"
      } to one you built yourself.
    '');
in {
  options.programs.captain-miao = {
    enable = lib.mkEnableOption "the captain-miao dashboard (the `miao` TUI)";

    package = lib.mkOption {
      type = lib.types.package;
      default = pick "captain-miao";
      defaultText = lib.literalExpression "captain-miao.packages.\${system}.captain-miao";
      description = ''
        The dashboard package. Override to use one of the bundled variants
        (`captain-miao-bundle-linux`, …) if you want a dashboard that carries
        servers to deploy to non-Nix hosts.
      '';
    };

    server = {
      enable = lib.mkEnableOption ''
        the captain-miao per-host daemon (`miao-server`) on this machine's PATH.

        Enable this on a host you want to reach *from* another machine's
        dashboard. Because the dashboard prefers a protocol-compatible
        `miao-server` already on the host's PATH over deploying its own, this is
        all the setup a Nix host needs — and unlike a deployed binary, it is
        built against this machine's own libc, so it works where a generic
        glibc build cannot run at all
      '';

      package = lib.mkOption {
        type = lib.types.package;
        default = pick "captain-miao-server";
        defaultText = lib.literalExpression "captain-miao.packages.\${system}.captain-miao-server";
        description = "The `miao-server` package to put on PATH.";
      };
    };
  };

  config = lib.mkIf (cfg.enable || cfg.server.enable) {
    home.packages =
      lib.optional cfg.enable cfg.package
      ++ lib.optional cfg.server.enable cfg.server.package;
  };
}
