#!/usr/bin/env bash
# Push the current branch and create a GitHub pull request.
# Usage: gh-pr.sh --title <title> [--body <body> | --body-file <file>]
#                 [--base <branch>] [--draft] [--remote <remote>]
#                 [--repo <owner>/<repo>] [--head <owner>:<branch>] [--no-push]
#                 [--closes <n[,n...]>]... [--refs <n[,n...]>]... [--dry-run]
set -euo pipefail

BASE="${GH_PR_BASE:-main}"
REMOTE="${GH_PR_REMOTE:-origin}"
TITLE=""
BODY=""
BODY_FILE=""
BODY_MODE=""
DRAFT=false
NO_PUSH=false
DRY_RUN=false
REPO=""
HEAD_REF=""
TEMP_BODY_FILE=""
CLOSES=()
REFS=()

usage() {
  cat <<'EOF'
Usage: gh-pr.sh --title <title> [--body <body> | --body-file <file>]
                [--base <branch>] [--draft] [--remote <remote>]
                [--repo <owner>/<repo>] [--head <owner>:<branch>] [--no-push]
                [--closes <n[,n...]>]... [--refs <n[,n...]>]... [--dry-run]

Push the current branch to the selected remote (origin by default), then create
its pull request. GH_PR_BASE and GH_PR_REMOTE set default base branch and
remote. If --title is omitted, it is requested interactively. If neither body
option is supplied, the body is read from standard input.

Issue links: every --closes issue gets a "Closes #n" line and every --refs issue
a "Refs #n" line appended to the body, unless the body already carries that
exact line. With no --closes at all, the first #NNN in the branch name is
closed (historical default) unless the body already contains "Closes".

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
  --closes <n[,n...]>         Issue(s) this PR closes; "#" optional; repeatable
  --refs <n[,n...]>           Related issue(s) to link without closing; repeatable
  --dry-run                   Print the final body and gh command; push nothing
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

# Split "317,#315 318" into bare numbers, validating each.
add_issue_numbers() {
  local target="$1" raw="$2" item
  IFS=', ' read -r -a items <<<"$raw"
  for item in "${items[@]}"; do
    [[ -n "$item" ]] || continue
    item="${item#\#}"
    [[ "$item" =~ ^[0-9]+$ ]] || die "invalid issue number: $item"
    if [[ "$target" == closes ]]; then CLOSES+=("$item"); else REFS+=("$item"); fi
  done
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
    --closes)
      need_value "$@"
      add_issue_numbers closes "$2"
      shift 2
      ;;
    --refs)
      need_value "$@"
      add_issue_numbers refs "$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=true
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

if [[ "$DRY_RUN" == false ]]; then
  gh auth status >/dev/null 2>&1 || die "GitHub CLI is not authenticated; run 'gh auth login' and retry"
fi

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
  BODY_TO_CHECK="$(cat -- "$BODY_FILE")" || die "could not read body file: $BODY_FILE"
else
  BODY_TO_CHECK="$BODY"
fi

# Historical default: with no explicit --closes, close the first #NNN in the
# branch name unless the body already carries a closing reference.
if ((${#CLOSES[@]} == 0)) && [[ "$CURRENT_BRANCH" =~ \#([0-9]+) ]]; then
  if ! grep -Fq 'Closes' <<<"$BODY_TO_CHECK"; then
    CLOSES+=("${BASH_REMATCH[1]}")
  fi
fi

# Build the issue-link appendix, skipping lines the body already has.
APPENDIX=""
for n in "${CLOSES[@]+"${CLOSES[@]}"}"; do
  grep -Eq "(^|[[:space:]])Closes #${n}([^0-9]|$)" <<<"$BODY_TO_CHECK" || APPENDIX+="Closes #${n}"$'\n'
done
for n in "${REFS[@]+"${REFS[@]}"}"; do
  grep -Eq "(^|[[:space:]])(Refs|Ref|Related|Relates to|Part of) #${n}([^0-9]|$)" <<<"$BODY_TO_CHECK" || APPENDIX+="Refs #${n}"$'\n'
done

if [[ -n "$APPENDIX" ]]; then
  TEMP_BODY_FILE="$(mktemp)" || die "could not create a temporary PR body file"
  printf '%s\n\n%s' "$BODY_TO_CHECK" "$APPENDIX" >"$TEMP_BODY_FILE"
  BODY_FILE="$TEMP_BODY_FILE"
  BODY_MODE=file
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

if [[ "$DRY_RUN" == true ]]; then
  printf -- '--- PR body ---\n'
  if [[ "$BODY_MODE" == file ]]; then cat -- "$BODY_FILE"; else printf '%s\n' "$BODY"; fi
  printf -- '\n--- command ---\n'
  [[ "$NO_PUSH" == true ]] || printf 'git push -u %q HEAD\n' "$REMOTE"
  printf 'gh'; printf ' %q' "${CREATE_ARGS[@]}"; printf '\n'
  exit 0
fi

if [[ "$NO_PUSH" == false ]]; then
  git remote get-url "$REMOTE" >/dev/null 2>&1 || die "Git remote '$REMOTE' is not configured; use --remote or configure it"
  git push -u "$REMOTE" HEAD
fi

gh "${CREATE_ARGS[@]}"
