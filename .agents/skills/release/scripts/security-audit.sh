#!/bin/sh
set -eu

root=${1:-.}
if [ ! -d "$root" ]; then
  printf 'security-audit: project root is not a directory: %s\n' "$root" >&2
  exit 64
fi
cd "$root"
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  printf 'security-audit: not inside a Git work tree: %s\n' "$root" >&2
  exit 69
fi

status=0
path_file=$(mktemp "${TMPDIR:-${TMP:-.}}/release-audit-paths.XXXXXX") || exit 70
content_file=$(mktemp "${TMPDIR:-${TMP:-.}}/release-audit-content.XXXXXX") || {
  rm -f "$path_file"
  exit 70
}
trap 'rm -f "$path_file" "$content_file"' EXIT HUP INT TERM

# Print paths only so the audit itself does not echo potential secret values.
git ls-files | grep -Ei '(^|/)(\.env($|\.)|[^/]*(secret|credential)[^/]*)$|\.(pem|key|p12|pfx)$' >"$path_file" || true
git grep -IilE '(api[_-]?key|client[_-]?secret|password|private[_-]?key|AWS_[A-Z_]+|GITHUB_TOKEN|auth[_-]?token)[[:space:]]*[:=][[:space:]]*["'"''][^"'"'']{8,}' -- >"$content_file" 2>/dev/null || true

if [ -s "$path_file" ]; then
  printf '%s\n' 'security-audit: sensitive-looking tracked paths:' >&2
  sed 's/^/  /' "$path_file" >&2
  status=2
fi
if [ -s "$content_file" ]; then
  printf '%s\n' 'security-audit: tracked files containing credential-like assignments (values redacted):' >&2
  sed 's/^/  /' "$content_file" >&2
  status=2
fi
if [ "$status" -eq 0 ]; then
  printf '%s\n' 'security-audit: no heuristic findings'
fi
exit "$status"
