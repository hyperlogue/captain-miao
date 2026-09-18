# Build and distribution rules

Use for build scripts, server payloads, packaging, executable installation and
CI changes. [Crate split](../crate-split.md) owns the architecture; the
[release skill](../../.claude/skills/release/SKILL.md) owns release execution.

## Embedded server payloads

- `CM_SERVER_PAYLOAD_MANIFEST` is the only switch for embedding servers.
  An ordinary build leaves it unset and the table empty; set but invalid is a
  hard build error. `build.rs` watches each archive, so avoid rewriting identical
  bytes and triggering a full LTO relink.
- Publish bundled dashboards: `SHIPPING_VARIANT` includes one x86-64 glibc
  server and supplies release dashboards and the four npm platform packages.
  `ALL_SERVER_VARIANT` is the extra GitHub-only download with all four servers.
  Shipping a plain build under `miao-v…` removes auto-deployment for its users;
  `no_default_variant_is_the_plain_build` pins this distinction.
- Install executables on a fresh inode using xtask's `install`. Copying over a
  previously run file breaks macOS vnode-bound signatures and subsequent execs
  die with `SIGKILL (Code Signature Invalid)`.

```sh
cargo build --workspace
cargo build --no-default-features   # local-only dashboard
cargo xtask dist --list
cargo xtask dist                    # default: shipping variants
cargo xtask prepare-servers --out dist/servers
miao --version                     # reports embedded servers
```

For a live provisioning test, designate a disposable ssh target and use the
full recipe in `src/backend/provision.rs`. `cargo xtask dist` writes one manifest
per variant; pass it via `CM_SERVER_PAYLOAD_MANIFEST` to the test build.
There is no `bundle-*` Cargo feature.

## Naming and publication

- Shipping binaries drop `captain-`; Cargo/npm packages, nix attrs and config,
  state and cache directories keep it. `xtask/src/server.rs` keeps `SERVER_PKG`
  and `SERVER_BIN` separate so a successful build can locate its output.
- Tarballs follow binary names: `miao-v…`, `miao-bundled-all-server-v…`,
  `miao-server-v…`. `scripts/stage-npm-packages.sh` must select the first by
  exact name; a loose glob can stage the daemon as the dashboard.
- Uploaded artifact names keep `captain-miao-`: the release job collects
  `pattern: captain-miao-*`.
- Bump `[workspace.package] version` and refresh `Cargo.lock` before tagging.
  The `verify` job rejects a tag that differs from that commit's version or
  lacks populated changelog notes. Tags use `v` followed by plain SemVer.
- Pass GitHub Actions expression values into shell scripts through `env:`.
  No `run:` body may interpolate a `${{ }}` expression.
- GitHub assets, npm platform packages and the launcher publish in separate
  jobs. Use **Re-run failed jobs** to resume a failed release; successful jobs
  stay complete and existing npm versions are skipped. npm jobs stage the
  published GitHub assets so retries use the same binary bytes.
- The launcher waits up to ten minutes for all exact platform pins to become
  visible, revalidating npm metadata on each poll. The deadline includes registry
  requests. Run `node --test scripts/wait-for-npm-packages.test.mjs` in
  `nix develop` when changing this wait.
