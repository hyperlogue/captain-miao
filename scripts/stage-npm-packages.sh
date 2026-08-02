#!/usr/bin/env bash
# Stage the four per-platform npm packages (@hyperlogue/captain-miao-<os>-<arch>)
# that carry the prebuilt binaries, and stamp the launcher's version +
# optionalDependencies to match. Run AFTER the build workflow has produced the
# release tarballs.
#
# The launcher (npm/launch.mjs) resolves the matching package at runtime via
# createRequire — no download — so each package's version must equal the
# launcher's own, and the launcher must be published only after these packages
# exist (release.yml enforces that order). This script is the single authority
# for that lockstep: every version is stamped from ONE source, Cargo.toml's
# [workspace.package] version, so a release bumps that and the rest follows.
#
# Deliberately bash + jq rather than a Node/Bun script: this is a Rust workspace
# with no JS toolchain, and jq is preinstalled on GitHub runners. Adding a
# package manager just to stamp four JSON files would be the larger dependency.
#
# Usage:
#   scripts/stage-npm-packages.sh [tarball-dir]      # default: artifacts/
#   EXPECT_VERSION=0.2.0 scripts/stage-npm-packages.sh
#
# Output: dist/npm/<pkg>/{package.json,bin/captain-miao,LICENSE,README.md}
set -euo pipefail

cd "$(dirname "$0")/.."

SRC="${1:-artifacts}"
OUT="dist/npm"
SCOPE="@hyperlogue"
BIN="captain-miao"

command -v jq >/dev/null || { echo "error: jq is required" >&2; exit 1; }

# The one version source. Parsed straight out of Cargo.toml rather than via
# `cargo metadata` so this runs on a publish job with no Rust toolchain. Matches
# `version = "x"` with or without surrounding spaces (`$1 == "version"` used to
# miss the unspaced form and silently yield nothing). Keep in sync with the
# identical parse in the release workflow's `verify` job.
VERSION=$(awk '
  /^\[workspace\.package\]/     { ws = 1; next }
  /^\[/                         { ws = 0 }
  ws && /^[ \t]*version[ \t]*=/ {
    if (match($0, /"[^"]*"/)) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
  }
' Cargo.toml)
[ -n "$VERSION" ] || { echo "error: no [workspace.package] version in Cargo.toml" >&2; exit 1; }

# Caller (release.yml) passes the git tag's version so a mistagged release dies
# here rather than publishing a version nobody asked for.
if [ -n "${EXPECT_VERSION:-}" ] && [ "$EXPECT_VERSION" != "$VERSION" ]; then
    echo "error: expected version $EXPECT_VERSION but Cargo.toml says $VERSION" >&2
    exit 1
fi

# slug | cargo target triple | npm os | npm cpu | npm libc ("-" = omit)
# Keep in sync with the PACKAGES map in npm/launch.mjs (the consumer) and the
# build matrix in .github/workflows/build.yml (the producer of these triples).
PLATFORMS="
darwin-arm64|aarch64-apple-darwin|darwin|arm64|-
darwin-x64|x86_64-apple-darwin|darwin|x64|-
linux-x64|x86_64-unknown-linux-gnu|linux|x64|glibc
linux-arm64|aarch64-unknown-linux-gnu|linux|arm64|glibc
"

rm -rf "$OUT"
mkdir -p "$OUT"

# Ship the same README/LICENSE with every package. npm always includes a README
# regardless of `files`, and a package page with no readme looks abandoned.
cp README.md LICENSE npm/

PINS="{}"
COUNT=0

while IFS='|' read -r slug target os cpu libc; do
    [ -n "$slug" ] || continue

    name="$SCOPE/$BIN-$slug"
    pkgdir="$OUT/$BIN-$slug"
    tarball="$SRC/$BIN-v$VERSION-$target.tar.gz"

    [ -f "$tarball" ] || { echo "error: missing $tarball" >&2; exit 1; }

    # No checksum step here. In CI the tarball arrives via download-artifact,
    # which verifies the SHA-256 digest upload-artifact recorded — re-checking a
    # sidecar that travelled with the tarball would prove integrity a second time
    # and provenance neither time (an attacker who rewrote one would rewrite
    # both). tar itself is the remaining integrity check: gzip carries a CRC and
    # a truncated archive fails extraction below.
    mkdir -p "$pkgdir/bin"
    # The tarball holds <name>/captain-miao; --strip-components lands the binary
    # directly in bin/. Extract only the binary — never the README/LICENSE copies
    # the release tarball also carries, which would shadow the ones staged above.
    # --no-same-owner/--no-same-permissions are the non-root defaults, stated
    # explicitly so a run as root can't restore an archived uid or setuid bit.
    tar -xzf "$tarball" -C "$pkgdir/bin" --strip-components=1 \
        --no-same-owner --no-same-permissions \
        "$BIN-v$VERSION-$target/$BIN"

    # tar extracts whatever kind of entry the archive names. A member recorded as
    # a symlink extracts as one (verified: exit 0, no warning), and the chmod
    # below would then follow it out of the staging dir — so assert the extracted
    # path is a plain regular file before touching its mode or packing it.
    [ -f "$pkgdir/bin/$BIN" ] && [ ! -L "$pkgdir/bin/$BIN" ] \
        || { echo "error: $tarball did not yield a regular file at bin/$BIN" >&2; exit 1; }

    # npm preserves tarball file modes, so a 0644 binary would EACCES on every
    # install. Set it explicitly rather than trusting what tar restored.
    chmod 0755 "$pkgdir/bin/$BIN"

    # Notes on the object below — kept OUT of the jq program on purpose: the
    # program is bash-single-quoted, so one apostrophe in a comment (an "it's",
    # a "launch.mjs's") silently ends the quote and turns the rest into bash.
    #
    #   libc  — lets npm >=9.6 skip a glibc package on musl. The isMusl() check
    #           in launch.mjs is the real guard, for older npm / Bun that ignore
    #           this field. Omitted ("-") on darwin, which has no libc variants.
    #   bin   — deliberately absent: the launcher package owns the captain-miao
    #           command, and a second one here would collide on install.
    #   files — no `exports` either; it would block the bin/ subpath that
    #           launch.mjs resolves through createRequire.
    jq -n \
        --arg name "$name" --arg version "$VERSION" \
        --arg os "$os" --arg cpu "$cpu" --arg libc "$libc" \
        --arg bin "$BIN" \
        '{
            name: $name,
            version: $version,
            description: "Prebuilt captain-miao binary for \($os)-\($cpu).",
            license: "MIT",
            os: [$os],
            cpu: [$cpu]
          }
          + (if $libc == "-" then {} else { libc: [$libc] } end)
          + {
            repository: { type: "git", url: "git+https://github.com/hyperlogue/captain-miao.git" },
            homepage: "https://github.com/hyperlogue/captain-miao#readme",
            bugs: "https://github.com/hyperlogue/captain-miao/issues",
            files: ["bin/\($bin)"],
            publishConfig: { access: "public" }
          }' > "$pkgdir/package.json"

    cp LICENSE README.md "$pkgdir/"

    PINS=$(jq --arg n "$name" --arg v "$VERSION" '. + {($n): $v}' <<<"$PINS")
    COUNT=$((COUNT + 1))
    echo "  staged $name"
done <<<"$PLATFORMS"

# Stamp the launcher from the same single version source, so its pins can never
# drift from the packages just built.
#
# The chmod is load-bearing: mktemp creates 0600 and mv preserves it, so without
# it the launcher ships a package.json only its owner can read — which breaks a
# global install for every other user on the box.
tmp=$(mktemp)
jq --arg v "$VERSION" --argjson pins "$PINS" \
   '.version = $v | .optionalDependencies = $pins' npm/package.json > "$tmp"
chmod 0644 "$tmp"
mv "$tmp" npm/package.json

echo "✓ staged $COUNT platform packages in $OUT/ and stamped launcher pins to v$VERSION"
