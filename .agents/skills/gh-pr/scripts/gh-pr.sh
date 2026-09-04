#!/usr/bin/env bash
# Push the current branch and create a GitHub pull request.
# Usage: gh-pr.sh --title <title> [--body <body> | --body-file <file>]
#                 [--base <branch>] [--draft] [--remote <remote>]
#                 [--repo <owner>/<repo>] [--head <owner>:<branch>] [--no-push]
set -euo pipefail

BASE="${GH_PR_BASE:-main}"
REMOTE="${GH_PR_REMOTE:-origin}"
TITLE=""
BODY=""
BODY_FILE=""
BODY_MODE=""
DRAFT=false
NO_PUSH=false
REPO=""
HEAD_REF=""
TEMP_BODY_FILE=""

usage() {
  cat <<'EOF'
Usage: gh-pr.sh --title <title> [--body <body> | --body-file <file>]
                [--base <branch>] [--draft] [--remote <remote>]
                [--repo <owner>/<repo>] [--head <owner>:<branch>] [--no-push]

Push the current branch to the selected remote (origin by default), then create
its pull request. GH_PR_BASE and GH_PR_REMOTE set default base branch and
remote. If --title is omitted, it is requested interactively. If neither body
option is supplied, the body is read from standard input.

Options:
  --title <title>             PR title (required unless interactive)
  --body <body>               PR body as one argument
  --body-file <file>          Read PR body from a file
  --base <branch>             Target branch (default: GH_PR_BASE or main)
  --draft                     Create a draft PR
  --remote <remote>           Git remote to push (default: GH_PR_REMOTE or origin)
  --repo <owner>/<repo>       Repository in which to create the PR
  --head <owner>:<branch>     Head ref for a fork PR
  --no-push                   Do not push; use an already-published branch
  -h, --help                  Show this help
EOF
}

die() {
  printf 'gh-pr: %s\n' "$*" >&2
  exit 1
}

need_value() {
  [[ $# -ge 2 && -n "$2" ]] || die "missing value for $1"
}

cleanup() {
  [[ -z "$TEMP_BODY_FILE" ]] || rm -f -- "$TEMP_BODY_FILE"
}
trap cleanup EXIT

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)
      need_value "$@"
      BASE="$2"
      shift 2
      ;;
    --draft)
      DRAFT=true
      shift
      ;;
    --title)
      need_value "$@"
      TITLE="$2"
      shift 2
      ;;
    --body)
      need_value "$@"
      [[ -z "$BODY_MODE" ]] || die "use only one of --body and --body-file"
      BODY="$2"
      BODY_MODE=inline
      shift 2
      ;;
    --body-file)
      need_value "$@"
      [[ -z "$BODY_MODE" ]] || die "use only one of --body and --body-file"
      BODY_FILE="$2"
      BODY_MODE=file
      shift 2
      ;;
    --remote)
      need_value "$@"
      REMOTE="$2"
      shift 2
      ;;
    --repo)
      need_value "$@"
      REPO="$2"
      shift 2
      ;;
    --head)
      need_value "$@"
      HEAD_REF="$2"
      shift 2
      ;;
    --no-push)
      NO_PUSH=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1 (use --help for usage)"
      ;;
  esac
done

command -v git >/dev/null 2>&1 || die "git is required but was not found in PATH"
command -v gh >/dev/null 2>&1 || die "GitHub CLI (gh) is required but was not found in PATH"
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "run this command inside a Git work tree"
CURRENT_BRANCH="$(git branch --show-current)"
[[ -n "$CURRENT_BRANCH" ]] || die "cannot create a PR from a detached HEAD; check out a branch first"

gh auth status >/dev/null 2>&1 || die "GitHub CLI is not authenticated; run 'gh auth login' and retry"

if [[ -z "$TITLE" ]]; then
  [[ -t 0 ]] || die "--title is required when standard input is not a terminal"
  read -r -p "PR title: " TITLE || die "could not read PR title"
fi
[[ "$TITLE" =~ [^[:space:]] ]] || die "PR title must not be empty"
[[ "$TITLE" != *$'\n'* && "$TITLE" != *$'\r'* ]] || die "PR title must be one line"
((${#TITLE} <= 72)) || die "PR title must be 72 characters or fewer"
[[ -n "$BASE" ]] || die "base branch must not be empty"

if [[ -z "$BODY_MODE" ]]; then
  if [[ -t 0 ]]; then
    printf 'PR body (Ctrl-D to end):\n' >&2
  fi
  BODY="$(cat)" || die "could not read PR body from standard input"
  BODY_MODE=inline
fi

if [[ "$BODY_MODE" == file ]]; then
  [[ -f "$BODY_FILE" && -r "$BODY_FILE" ]] || die "body file is not a readable regular file: $BODY_FILE"
fi

TICKET=""
if [[ "$CURRENT_BRANCH" =~ \#([0-9]+) ]]; then
  TICKET="#${BASH_REMATCH[1]}"
fi

# Preserve an explicit closing reference. Otherwise link the first #NNN in the
# branch name, matching the historical behavior of this helper.
if [[ -n "$TICKET" ]]; then
  if [[ "$BODY_MODE" == inline ]]; then
    BODY_TO_CHECK="$BODY"
  else
    BODY_TO_CHECK="$(cat -- "$BODY_FILE")" || die "could not read body file: $BODY_FILE"
  fi

  if ! grep -Fq 'Closes' <<<"$BODY_TO_CHECK"; then
    TEMP_BODY_FILE="$(mktemp)" || die "could not create a temporary PR body file"
    if [[ "$BODY_MODE" == file ]]; then
      cat -- "$BODY_FILE" >"$TEMP_BODY_FILE" || die "could not prepare PR body"
    else
      printf '%s' "$BODY" >"$TEMP_BODY_FILE"
    fi
    printf '\n\nCloses %s\n' "$TICKET" >>"$TEMP_BODY_FILE"
    BODY_FILE="$TEMP_BODY_FILE"
    BODY_MODE=file
  fi
fi

if [[ "$NO_PUSH" == false ]]; then
  git remote get-url "$REMOTE" >/dev/null 2>&1 || die "Git remote '$REMOTE' is not configured; use --remote or configure it"
  git push -u "$REMOTE" HEAD
fi

CREATE_ARGS=(pr create --title "$TITLE" --base "$BASE")
[[ "$DRAFT" == true ]] && CREATE_ARGS+=(--draft)
[[ -n "$REPO" ]] && CREATE_ARGS+=(--repo "$REPO")
[[ -n "$HEAD_REF" ]] && CREATE_ARGS+=(--head "$HEAD_REF")
if [[ "$BODY_MODE" == file ]]; then
  CREATE_ARGS+=(--body-file "$BODY_FILE")
else
  CREATE_ARGS+=(--body "$BODY")
fi

gh "${CREATE_ARGS[@]}"
