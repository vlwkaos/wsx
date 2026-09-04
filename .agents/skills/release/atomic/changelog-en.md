# English changelog

Apply the parent skill's changelog coverage invariant.

## Collect evidence

Resolve candidates and published releases before choosing the previous tag:

```bash
git tag --merged HEAD --sort=-version:refname
gh release list --limit 100 --json tagName,isDraft,isPrerelease,publishedAt 2>/dev/null || true
git remote get-url origin
```

Match the repository's tag convention, existing `CHANGELOG.md`, and published release metadata. Ignore unrelated, draft, unpublished, or unreachable tags. Verify the choice with `git merge-base --is-ancestor <previous-tag> HEAD`. If there is no previous release, use `git log --root` and `git diff-tree --root` to inventory complete history.

For an existing previous release, collect the complete range:

```bash
git log <previous-tag>..HEAD --format='%H %P %s' --name-status
git diff --name-status <previous-tag>..HEAD
git diff --stat <previous-tag>..HEAD
```

Read relevant diffs and merge commits—not only subjects. Build a coverage map from every in-range commit and changed path to a bullet before editing.

## Format

Follow the repository's established format. If none exists, prepend:

```markdown
## [VERSION] - YYYY-MM-DD

### Features
- Description ([`a1b2c3d`](https://github.com/owner/repo/commit/fullhash))

### Bug Fixes
- Description ([`f4e5d6c`](https://github.com/owner/repo/commit/fullhash))
```

Default categories: `Features`, `Bug Fixes`, `Refactor`, `Docs`, `UI`, `Tests`, `Security`, `Dependencies`, `Other`.

Rules:

- Use concise English without emojis.
- Include every change from the previous release, exclusive, through `HEAD`, inclusive.
- Combine related commits where useful while preserving full scope.
- Explicitly group mechanical, dependency, CI, and generated-file changes instead of omitting them.
- Link a representative commit as ``[`hash7`](https://github.com/owner/repo/commit/fullhash)``; include multiple links when one is insufficient for coverage.
- Do not claim user-visible behavior not supported by the diff.

Reconcile the draft against both complete inventories and resolve every unmapped item. Do not commit here; the selected release flow owns the commit.
