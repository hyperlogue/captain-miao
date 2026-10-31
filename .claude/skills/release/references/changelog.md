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
`release_notes_dir` to that directory's path. Keep the working directory
unchanged while drafting and reviewing, including generated files and
formatting or lockfile updates. Read `r3 guide` and `r3 guide files`, then publish
the draft:

```sh
r3 create --kind files --dir "$release_notes_dir" --file release-notes.md \
  --title 'Release notes'
```

Share the returned artifact URL and obtain release-note sign-off before making
any working-directory changes. Publish revisions to the same artifact with
`r3 publish <artifact-id> --dir "$release_notes_dir" --file release-notes.md`.
Reuse approval already given for those exact notes, or an explicit instruction
to skip review. If r3 publication is unavailable, report the blocker and keep
the working directory unchanged until the user chooses an alternative.

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
