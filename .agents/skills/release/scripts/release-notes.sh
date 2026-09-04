#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  printf 'usage: %s CHANGELOG VERSION OUTPUT\n' "$0" >&2
  exit 64
fi
changelog=$1
version=$2
output=$3
[ -f "$changelog" ] || {
  printf 'release-notes: changelog not found: %s\n' "$changelog" >&2
  exit 66
}
[ -n "$version" ] || {
  printf '%s\n' 'release-notes: version must not be empty' >&2
  exit 64
}
case $version in v*) version=${version#v} ;; esac

outdir=$(dirname "$output")
[ -d "$outdir" ] || {
  printf 'release-notes: output directory does not exist: %s\n' "$outdir" >&2
  exit 73
}
tmp=$(mktemp "$outdir/.release-notes.XXXXXX") || exit 70
trap 'rm -f "$tmp"' EXIT HUP INT TERM

awk -v want="$version" '
function heading_version(line, x) {
  x=line
  sub(/^##[[:space:]]+/, "", x)
  sub(/[[:space:]]+-.*/, "", x)
  sub(/^\[/, "", x); sub(/\]$/, "", x); sub(/^v/, "", x)
  return x
}
/^##[[:space:]]+/ {
  if (found) exit
  if (heading_version($0) == want) { found=1; next }
}
found { print }
END { if (!found) exit 3 }
' "$changelog" >"$tmp" || {
  code=$?
  if [ "$code" -eq 3 ]; then
    printf 'release-notes: version section not found: %s\n' "$version" >&2
  else
    printf '%s\n' 'release-notes: extraction failed' >&2
  fi
  exit "$code"
}

# Trim trailing blank lines while preserving note content.
awk '{ lines[NR]=$0; if ($0 !~ /^[[:space:]]*$/) last=NR } END { for (i=1; i<=last; i++) print lines[i] }' "$tmp" >"$output"
if [ ! -s "$output" ]; then
  rm -f "$output"
  printf 'release-notes: version section is empty: %s\n' "$version" >&2
  exit 65
fi
rm -f "$tmp"
trap - EXIT HUP INT TERM
printf '%s\n' "$output"
