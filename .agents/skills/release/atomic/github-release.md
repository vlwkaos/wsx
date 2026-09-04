# GitHub Release

## Preconditions

Determine the configured release branch rather than assuming `main`.

```bash
git rev-parse --show-toplevel
git branch --show-current
git status --porcelain
git remote get-url origin
command -v gh
gh auth status -h github.com
git fetch --prune --tags origin
```

Require the expected branch, clean tree at push time, authenticated `gh`, a reachable GitHub remote, and a version/tag absent locally, remotely, and from `gh release view` (unless following explicitly approved recovery). Ensure local branch is not behind/diverged.

## Prepare commit and tag

If changelog/version edits are not committed, stage only intended release files, review `git diff --cached`, and create the repository's release commit. If a prior flow already created the release commit, do not create an empty duplicate.

After final confirmation, push the release branch alone and capture its exact commit:

```bash
git push origin <release-branch>
release_commit=$(git rev-parse HEAD)
```

Inspect workflow triggers. If required CI runs on pushes to the release branch, poll for every run at `release_commit`, wait for terminal results, and require success before tagging. A bounded discovery window that finds no run is acceptable only when the inspected workflows do not expect one; an expected missing, failed, cancelled, or timed-out run stops the release.

Then create and push only the explicit tag:

```bash
git tag -a "<tag>" -m "Release <tag>"
git push origin "refs/tags/<tag>"
```

Never use `git push --tags`. Verify the remote tag resolves to the intended commit before creating a release. If the tag starts validation-only CI, wait for its exact-commit runs before registry publication or release creation. If tag CI owns publication or release creation, use the corresponding CI flow instead of racing it.

## Extract notes and create release

Change to the directory containing the parent `SKILL.md`, then use the bundled script to extract the exact version section. Do not resolve the script from the project repository:

```sh
notes_file=<safe-temporary-directory>/release-notes-<version>.md
./scripts/release-notes.sh <project-root>/CHANGELOG.md <version> "$notes_file"
```

Inspect the output. Prefer an environment-provided temporary directory; if neither `TMPDIR` nor `TMP` is configured, choose a safe project-local temporary file. In either case, remove it afterward.

```sh
gh release create "<tag>" --verify-tag --title "<tag>" --notes-file "$notes_file"
```

Add `--prerelease` for prerelease versions. Add assets only after validating their names, architectures, checksums/signatures, and successful builds. Do not create a second release if one already exists.

## Verify and report

```bash
gh release view "<tag>" --json url,tagName,isDraft,isPrerelease,assets
```

Verify tag commit, release state, notes, and all expected assets. Remove the temporary notes file. Report version, commit, tag, URL, assets, and any downstream workflow still running or failed.
