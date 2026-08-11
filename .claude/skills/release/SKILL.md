---
name: release
description: Cut a captain-miao release — draft the CHANGELOG entry, bump the workspace version (refreshing Cargo.lock with it), commit, then cut the annotated `vX.Y.Z` tag the release CI builds from. Use when the user wants to release a new version, bump the version, write or update the changelog, or tag a release.
---

# Releasing captain-miao

A release is one version-bump commit plus an annotated tag on it. The whole job
is: write the changelog, bump the version, refresh `Cargo.lock`, commit, tag that
commit, and push.

## The one rule: bump *before* you tag

The tag must point at the version-bump commit. Cutting the tag first and bumping
afterward leaves the tag on a commit whose in-code version is still the previous
release — and here the CI catches it rather than shipping it: the `verify` job
compares the tag against `Cargo.toml` and fails the run. Do the changelog **and**
the version bump in the release commit, then tag that commit.

## Version sources

**`Cargo.toml`'s `[workspace.package] version` is the single source.** All four
packages inherit it (`version.workspace = true`), the release workflow's `verify`
job greps it to check the tag, and `scripts/stage-npm-packages.sh` stamps every
npm package version and every launcher pin from it. Nothing else needs editing —
in particular `npm/package.json`'s version and its four `optionalDependencies`
pins are **generated at publish time**, so do not hand-edit them.

**But `Cargo.lock` is a second file that must move with it.** The lock records
each workspace member's version, and `build.yml` builds with `--locked`, which
fails on a stale lock. Bumping `Cargo.toml` without refreshing the lock produces
a release that dies in every build job. `cargo check` refreshes it; commit it
alongside.

## Steps

1. **Pick the version** (SemVer). Previous tag: `git describe --tags --abbrev=0`.

2. **Draft the CHANGELOG entry** (`CHANGELOG.md`, [Keep a Changelog] format).
   Survey what shipped since the last tag — `git log v<prev>..HEAD --oneline` —
   then:
   - Insert a new `## [X.Y.Z] - YYYY-MM-DD` section directly under the intro
     block, above the previous version. Use the release date.
   - Group bullets under `### Added` / `### Changed` / `### Fixed` / `### Removed`
     / `### Security` — only the groups that apply, in that order.
   - Write from the **user's** vantage point: what they can now do, or no longer
     run into — not the internal mechanics, refactors, or scaffolding that got it
     there. Fold related commits into one bullet. Match the existing voice: a bold
     lead-in (`**Feature.**`) then a sentence or two on the change and why it
     matters.
   - Add the compare link at the bottom, with the others:
     `[X.Y.Z]: https://github.com/hyperlogue/captain-miao/compare/v<prev>...vX.Y.Z`

   This entry is the release's public face — the workflow extracts exactly this
   section and makes it the GitHub release description. **Show the draft to the
   user and get their sign-off before you commit.**

3. **Bump the version and refresh the lock:**
   ```sh
   # edit Cargo.toml: [workspace.package] version = "X.Y.Z"
   cargo check --workspace          # rewrites Cargo.lock to match
   git diff --stat Cargo.lock       # must show the four member versions moved
   ```

4. **Verify, then run the checks:**
   ```sh
   grep -n '^version' Cargo.toml                     # reads X.Y.Z
   grep -A1 '^name = "captain-miao"$' Cargo.lock     # reads X.Y.Z
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo test --workspace --locked
   ```
   There is no local pre-commit hook — CI gates fmt + clippy, so run them here.

5. **Commit.** Follow `AGENTS.md`: check `git diff --cached --name-only` for a
   concurrent committer first and back off if it is non-empty, stage and commit
   **by path**, and use **no** `Co-Authored-By` trailer.
   ```sh
   git add CHANGELOG.md Cargo.toml Cargo.lock
   git commit -F - -- CHANGELOG.md Cargo.toml Cargo.lock <<'EOF'
   Release vX.Y.Z
   EOF
   ```

6. **Tag** — annotated, on the commit you just made. Unlike the sibling r3 repo,
   the tag message is **not** the release notes here: the workflow reads
   `CHANGELOG.md` out of the tagged tree, so there is no message-body parsing to
   get wrong and no `--cleanup` trap. A one-line subject is enough.
   ```sh
   git tag -a vX.Y.Z -m "captain-miao vX.Y.Z"
   git show --stat vX.Y.Z | head -20     # confirm it points at the bump commit
   ```

7. **Push** — leave the actual push to the user unless they ask. Note that this
   environment often has no push credentials (SSH key / `gh` auth may be absent);
   surface that rather than failing silently.
   ```sh
   git push origin main && git push origin vX.Y.Z
   ```

## Preconditions the pipeline needs

Check these before pushing a tag — each one fails the release rather than
degrading:

- **npm Trusted Publishing (OIDC)** — there is no `NPM_TOKEN`; each of the five
  packages is linked on npmjs.com to this repo and to `release.yml` by filename.
  If a package isn't linked (a newly added platform package, or the workflow was
  renamed) the GitHub Release still lands and the npm steps fail on auth; a re-run
  after fixing the link converges, since every publish step is idempotent.
- **The repo must be public** — the `ubuntu-22.04-arm` runner the aarch64 Linux
  build uses is free only for public repos; on a private repo that matrix leg
  never gets a runner.
- **The `release` environment** — the publish job requests it. GitHub creates it
  implicitly, so the run proceeds either way, but it is only actually gated once
  configured with required reviewers.

## What the pipeline does with the tag

`.github/workflows/release.yml` runs on any `v*` tag:

1. **`verify`** — the tag must be plain SemVer, must equal `Cargo.toml`'s
   version, and `CHANGELOG.md` must carry a populated `## [X.Y.Z]` section. All
   three checked before a build runner starts.
2. **`build`** — the four targets (`{x86_64,aarch64}` × `{linux-gnu,apple-darwin}`),
   each tarballed.
3. **`publish`** — GitHub Release first (reversible), then npm: the four
   per-platform packages, a poll until each is visible on the registry, then the
   `@hyperlogue/captain-miao` launcher that pins them.

## If you botch a release

- **Not pushed yet** — the tag is still local. `git tag -d vX.Y.Z`, fix, redo
  steps 3–6. Cheap, and the reason steps 1–6 all happen before any push.
- **Pushed, but the run failed** — every publish step is idempotent (each skips
  if that version already exists on npm), so fix the cause and re-run the
  workflow. It converges instead of double-publishing.
- **Pushed and published** — an npm version cannot be republished with different
  bytes. **Cut the next patch version** carrying the fix; that is the intended
  recovery. Moving a published tag is outward-facing and hard to reverse — confirm
  with the user first.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
