# Changelog entries

Use for writing release notes or preparing a release. `CHANGELOG.md` supplies
the existing voice; `.github/workflows/release.yml` publishes the selected
version's section as the GitHub release description.

Review changes since the previous release with `git describe --tags --abbrev=0`
and `git log v<previous>..HEAD --oneline`, including relevant local commits.
Use the requested release range when it differs; inspect changes behind vague
subjects rather than copying commit messages.

For a dated release, insert `## [X.Y.Z] - YYYY-MM-DD` above the previous release
using the release date. For an unreleased entry, preserve the requested heading
without inventing a release version or date.

- Group entries under Added, Changed, Fixed, Removed or Security, in that order,
  including only populated groups.
- Describe user-visible behavior, fold related commits together and lead each
  bullet with the project's bold feature label. Keep one sentence per bullet;
  fold a necessary caveat into it. Detailed mechanics belong in module docs,
  behavior documentation in the README, and motivation in commit messages.
- Where other people contributed, open the section with a short thank-you above
  the first group. Verify public attribution from commits and relevant issue,
  PR and review discussions in the release window, omitting the maintainer and
  bots and deduplicating identities. Respect applicable privacy rules; omit
  attribution that would disclose the operator's identity. Omit an empty thanks.
  Local git history includes unpushed work; API results do not. If discussion
  access is unavailable, use verified history and disclose that limitation.
- Add the version compare link with the existing links at the bottom, using
  the repository URL from `Cargo.toml` and `v<previous>...vX.Y.Z`.

An entry is ready when each claim is supported by the release range and the
heading and compare link match the intended version. Changelog-only work ends
under the normal repository commit rules. For a full release, preparation and
release-note review are in [the release procedure](release.md#prepare-the-release).
