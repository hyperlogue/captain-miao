# Release preparation, publication and recovery

Use the section matching the request. A local release consists of one commit
with the notes and version bump, plus an annotated `vX.Y.Z` tag on that commit.
Pushing the tag starts `.github/workflows/release.yml`, which publishes GitHub
assets and npm packages.

## Prepare the release

1. Establish the requested version and previous release. Inspect the worktree
   and local tags so unrelated work or an existing tag is not swept into the
   release. Follow the [distribution rules](../../../../docs/agent-guides/distribution.md)
   when release preparation also changes builds or packaging.
2. Follow the [changelog review](changelog.md#review-before-editing): publish
   the proposed notes as an r3 files artifact and obtain sign-off before any
   working-directory changes. If review is still needed, cite that requirement,
   share the artifact URL and leave the working directory unchanged.
3. Apply the approved changelog and update the version sources described in
   [SKILL.md](../SKILL.md). Inspect `Cargo.lock` to confirm the inheriting member
   versions moved with `Cargo.toml`; leave unrelated dependency updates out of
   the release.
4. Complete verification: the tag version must match `[workspace.package]`,
   the lock must be current, and `CHANGELOG.md` must have a populated section
   for that version. Run the repository's fmt and clippy commands plus
   `cargo test --workspace --locked`. Reuse checks already passed on unchanged
   inputs; fix failures caused by the release preparation and rerun affected
   checks.
5. Follow the concurrent-committer check and path-scoped commit procedure in
   `AGENTS.md`. The usual paths are `CHANGELOG.md`, `Cargo.toml` and `Cargo.lock`;
   the subject is `Release vX.Y.Z`.
6. Create an annotated tag on that commit:
   `git tag -a vX.Y.Z -m "captain-miao vX.Y.Z"`. Verify the tag resolves to the
   release commit and read the version and notes from the tagged tree. The tag
   message is only a subject; CI takes release notes from `CHANGELOG.md`.

Preparation is complete with the verified local commit and tag, or the fully
drafted r3 artifact awaiting the required review with the working directory
unchanged. Report which state was reached. A request to prepare a release does
not require publishing it.

## Publish

Push only when the user has requested publication or pushing; honor that
existing authorization without asking again. Confirm the local tag points to
the intended release commit and that the requested push does not include
unrelated local commits.

Before a requested tag push, check the relevant pipeline prerequisites:

- npm Trusted Publishing links each platform package and the launcher to this
  repository and `release.yml`; a new package or renamed workflow needs its link
  configured. The workflow uses OIDC rather than a stored npm token.
- The repository must be public for the `ubuntu-22.04-arm` runner.
- The publish job uses the `release` environment. Reviewers gate it only if
  configured; follow existing environment requirements.

Push `main` and the specific release tag, then monitor the run. CI verifies the
version and notes, builds targets via `build.yml`, creates the GitHub Release,
and publishes the four npm platform packages before the launcher. Report
publication complete only after the requested workflow and registry results
confirm success. If credentials or environment approvals block it, report the
actual failure and prepared state; do not infer failure from this document.

## Recover a release

Inspect the existing tag, workflow run and published versions first.

- **Local tag only:** a requested correction can recreate that local tag after
  fixing and verifying the release commit. Preserve unrelated work.
- **Pushed, workflow failed:** fix the cause within the requested scope. Publish
  steps skip versions already on npm, so rerunning the workflow can finish a
  partial publish. After a failed retry, inspect the new failure before another
  mutation; stop for missing credentials, required external approval or a
  published version whose bytes would need changing.
- **Published bytes need changing:** cut a new patch version. npm versions are
  immutable. Moving a published tag requires explicit user authorization.
