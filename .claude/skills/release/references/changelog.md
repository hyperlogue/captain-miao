# Changelog entries

Use for writing release notes or preparing a release. `CHANGELOG.md` supplies
the existing voice; use the categories below even when older entries differ.
`.github/workflows/release.yml` publishes the selected version's section as the
GitHub release description.

## Review before editing

Review changes since the previous release with `git describe --tags --abbrev=0`
and `git log v<previous>..HEAD --oneline`, including relevant local commits.
Use the requested release range when it differs; inspect changes behind vague
subjects rather than copying commit messages.

Draft the complete proposed release section and compare link as
`release-notes.md` in a temporary directory outside the repository; set
`release_notes_dir` to that directory's path. Keep release files unchanged while
drafting and reviewing, including generated files and formatting or lockfile
updates. Separately requested work, such as release-skill maintenance, can
proceed independently.

Run `r3 guide` and `r3 guide files` with the available CLI, and follow that
executable's current publication and feedback workflow. Use `r3 --help` for
command details. When using a local checkout, check its executable version:
source changes do not establish that its compiled binary is current. Follow
the guide's version-mismatch instructions; a warning does not invalidate a
successful publication. Publish the draft:

```sh
r3 create --kind files --dir "$release_notes_dir" --file release-notes.md \
  --title 'Release notes'
```

Share the returned stable artifact URL and retain its ID and version sequence.
Obtain release-note sign-off before applying the release. Reuse approval already
given for those exact notes, or an explicit instruction to skip review.

Supported harnesses subscribe automatically on publication. Follow the guide
for subscription failures; while subscribed, continue independent work or end
the turn rather than polling or blocking on review. When feedback arrives,
fetch and claim its threads, inspect their recorded targets, and revise the
draft. Publish the complete file set under the same artifact ID:

```sh
r3 publish <artifact-id> --dir "$release_notes_dir" --file release-notes.md \
  --expected <latest-version-sequence>
```

On a sequence conflict, inspect the newer publication before retrying. Follow
the guide to reply to each thread and re-anchor verified locations after
publication; the human controls thread resolution. Thread resolution or
comment delivery alone is not release-note sign-off.

If r3 publication is unavailable, report the blocker and keep release files
unchanged until the user chooses an alternative.

After sign-off, apply the approved entry to `CHANGELOG.md`. Changelog-only work
ends under the normal repository commit rules. For a full release, continue
[release preparation](release.md#prepare-the-release) with the version bump and
checks; unchanged approved notes need no second sign-off.

## Entry format

For a dated release, insert `## [X.Y.Z] - YYYY-MM-DD` above the previous release
using the release date. For an unreleased entry, preserve the requested heading
without inventing a release version or date.

- Use only `### New`, `### Fixed` and `### Removed`, in that order. Omit empty
  categories. New covers additions and behavior changes; Fixed includes
  security fixes; Removed covers removed user-facing capabilities.
- Keep the changelog concise and focused on user-visible changes. Fold related
  commits into one bullet, lead with the project's bold feature label and use
  one short sentence describing the user impact. Include only caveats needed
  to use or upgrade the product. Omit internal refactors, test changes and
  build or documentation maintenance unless they change user-visible behavior.
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
heading and compare link match the intended version.
