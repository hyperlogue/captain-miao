# The dashboard, wrapped so it finds a link farm of servers to deploy.
#
# The recommended Nix package for driving remote hosts. Overriding the fleet it
# can reach costs a server build, not a dashboard relink:
#
#     packages.captain-miao-with-servers.override {
#       targets = [ "x86_64-unknown-linux-musl" "aarch64-unknown-linux-gnu" ];
#     }
{
  lib,
  symlinkJoin,
  makeWrapper,
  captain-miao-remote,
  captain-miao-server-payloads,
  # Passed straight through to `captain-miao-server-payloads`. `null` means "whatever
  # that package defaults to" — the default lives there and only there, so the
  # two cannot drift into disagreeing about what a plain build carries.
  targets ? null,
}: let
  servers =
    if targets == null
    then captain-miao-server-payloads
    else captain-miao-server-payloads.override {inherit targets;};
in
  symlinkJoin {
    name = "captain-miao-with-servers";
    # `captain-miao-remote`, not `captain-miao`: `REMOTE_ENABLED` is
    # `cfg!(feature = "remote")`, so a plain build never reads `hosts.json` and
    # never constructs a remote backend — it would carry a link farm it has no
    # code path to reach, and the whole package would be inert.
    paths = [captain-miao-remote];
    nativeBuildInputs = [makeWrapper];
    # `--set-default`, not `--set`: the chain's whole premise is that explicit
    # configuration beats a build-time default, so a user who exports their own
    # CAPTAIN_MIAO_SERVER_DIR must still win. The per-target variable overrides
    # either way.
    postBuild = ''
      wrapProgram $out/bin/miao \
        --set-default CAPTAIN_MIAO_SERVER_DIR ${servers}
    '';
    meta = {
      mainProgram = "miao";
      description = "captain-miao carrying servers for ${lib.concatStringsSep ", " servers.serverTargets}";
    };
  }
