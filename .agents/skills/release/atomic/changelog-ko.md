# Korean changelog

Apply the parent skill's changelog coverage invariant.

## Resolve the writing style first

Style precedence, highest first:

1. A repository house style: a `release.changelog.style:` marker in `AGENTS.md`/`CLAUDE.md` and the style document it names. Read that document and the newest existing `CHANGELOG.md` entry as the worked example.
2. `$writing:ko-reader-brief` when no house style exists.
3. The default rules below, for anything neither source specifies.

If a marker names a style document that is missing, report that instead of guessing the conventions.

## Collect evidence

```bash
git tag --merged HEAD --sort=-version:refname
gh release list --limit 100 --json tagName,isDraft,isPrerelease,publishedAt 2>/dev/null || true
git rev-parse --abbrev-ref HEAD
git remote get-url origin
```

Choose the previous published release by matching the repository's tag convention, existing `CHANGELOG.md`, and release metadata. Ignore unrelated, draft, unpublished, or unreachable tags; verify with `git merge-base --is-ancestor <previous-tag> HEAD`. If none exists, inventory complete history through `HEAD`.

For an existing release:

```bash
git log <previous-tag>..HEAD --format='%H %P %s' --name-status
git diff --name-status <previous-tag>..HEAD
git diff --stat <previous-tag>..HEAD
```

Read relevant diffs and merge commits. Extract a ticket from the branch only when the branch actually identifies one (for example `feature/#123`), and verify its repository URL. Build a commit/path-to-bullet coverage map before editing.

## Format

Follow the repository's format. If none exists, prepend:

```markdown
## [VERSION] - YYYY-MM-DD

### 새로운 기능
- Portal CRUD 구현 ([#123](https://github.com/owner/repo/issues/123), [`a1b2c3d`](https://github.com/owner/repo/commit/fullhash))

### 버그 수정
- 재연결 로직 수정 ([`f4e5d6c`](https://github.com/owner/repo/commit/fullhash))
```

Default categories: `새로운 기능`, `버그 수정`, `리팩토링`, `문서`, `제거`, `UI 개선`, `테스트`, `보안`, `의존성`, `기타`.

Rules:

- Write Korean only, normally 3–5 words per bullet; clarity and complete scope take precedence when that limit would hide information.
- Use no emojis.
- Cover every change from the previous release, exclusive, through `HEAD`, inclusive.
- Combine related work without losing scope; explicitly group mechanical, CI, dependency, and generated-file changes.
- Use ``[`hash7`](https://github.com/owner/repo/commit/fullhash)`` commit links and verified issue links.
- Do not infer unsupported user-visible behavior.

Reconcile the draft against complete commit and path inventories. Do not commit here; the selected flow owns the commit.
