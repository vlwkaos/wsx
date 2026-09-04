---
name: gh-pr
description: "[github] Create GitHub pull requests from the current Git branch, including review-ready Korean summaries, issue closing links, draft PRs, and fork-based upstream PRs. Use when asked to open, submit, or prepare a GitHub PR."
compatibility: Requires Git, the GitHub CLI (gh), an authenticated GitHub account, and a Git repository with a pushable branch.
user-invocable: false
---

# GitHub PR Create

Use this skill explicitly with `/skill:gh-pr` when the user wants a GitHub pull request created or prepared. Paths in this skill are relative to this skill directory.

## Scope and safeguards

Create a PR only from the current checked-out branch. Do not commit, amend, rebase, force-push, change remotes, or merge as part of this skill unless the user separately and explicitly requests that operation. Before the irreversible push/PR creation step, present the proposed title, base branch, draft state, body, issue link, and any notable uncommitted changes. Proceed without a further question only when the user has explicitly authorized creating the PR with those details or has supplied them.

Inspect large and unfamiliar changes directly. Use a read-only `delegate` reviewer only when the parent authored or materially influenced a high-impact change and a fresh implementation-blind judgment is necessary. Give it the frozen diff, contract, and exact review question. Do not delegate discovery, the PR decision, credentials, push, creation, or the whole workflow.

## Preflight and content

1. Confirm the repository and current branch, then inspect the change and choose the intended base branch. Use the repository's documented default branch when known; otherwise `main` is the helper's default.
   ```bash
   git status --short
   git branch --show-current
   git log --oneline -10
   git diff --stat <base>...HEAD
   git diff <base>...HEAD
   ```
   A detached HEAD cannot be used. Resolve an absent remote, missing GitHub authentication, push permission, failed checks, or an ambiguous base branch before creation; the helper reports these errors rather than guessing.

2. Generate a title and body from the commits and diff:
   - **Title:** imperative/verb-first conventional-commit style, one line, at most 72 characters.
   - **Body:** Korean by default, with 2–4 concrete bullets of 5–10 words under this heading. Use the user's requested language or template when provided.

   ```markdown
   ## 변경사항
   - 항목1
   - 항목2
   ```

3. Check the source branch for its first `#NNN` reference. Unless the body already contains `Closes`, the helper appends `Closes #NNN` so GitHub links and closes that issue when the PR merges. State this in the proposed PR content. Add any required test results, migration notes, risks, or issue references that are evident from the change; do not invent them.

## Create the PR

Use a body file for generated or multiline content. It prevents shell interpretation of backticks, command substitutions, dollar signs, quotes, and newlines.

```bash
body_file="$(mktemp)"
trap 'rm -f "$body_file"' EXIT
cat >"$body_file" <<'EOF'
## 변경사항
- 첫 번째 변경사항을 설명합니다
- 두 번째 변경사항을 설명합니다
EOF

bash scripts/gh-pr.sh \
  --title "feat: add release automation" \
  --body-file "$body_file" \
  --base main
```

The helper pushes `HEAD` to `origin` and creates the PR, printing the URL returned by GitHub. It accepts these input variants:

- `--title <title>`; when omitted in a terminal, it prompts for one.
- `--body <body>` for a single, safely quoted argument, or `--body-file <file>` for file content. When neither is supplied, it reads the body from standard input.
- `--base <branch>`; defaults to `GH_PR_BASE` when configured, otherwise `main`.
- `--draft` for a draft PR.
- `--remote <remote>`; defaults to `GH_PR_REMOTE` when configured, otherwise `origin`.
- `--no-push` when the branch has already been published.
- `--repo <owner>/<repo>` and `--head <account>:<branch>` for an upstream PR from a fork.

Run `bash scripts/gh-pr.sh --help` for the complete interface. The script stops on unknown or incomplete options, a missing title, an overlong or multiline title, a detached HEAD, missing dependencies/authentication, an unreadable body file, an unknown push remote, or a failed push or GitHub CLI request. It does not suppress GitHub's creation errors.

## Upstream repository without push access

Fork first, publish the current branch to the fork, then create the PR in the upstream repository. Supply the fork account and branch explicitly rather than assuming a remote layout.

```bash
gh repo fork <owner>/<repo> --remote
git push -u fork HEAD
bash scripts/gh-pr.sh \
  --title "fix: correct upstream behavior" \
  --body-file "$body_file" \
  --repo <owner>/<repo> \
  --head <account>:<branch> \
  --no-push
```

If the fork command used another remote name, use that name for the push. If the upstream has contribution, template, labeling, or check requirements, satisfy them before creating the PR. Report the resulting PR URL and any unresolved push, authentication, permissions, or validation failure to the user.

<!-- skiller:projection-policy -->
Treat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.
