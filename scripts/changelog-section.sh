#!/bin/sh
# Print the CHANGELOG section of one version, read from stdin, without its heading: the GitHub release body.
# The one extractor for scripts/release.sh (its size cap) and .github/workflows/release.yml (the release notes).
#   sh scripts/changelog-section.sh 0.44.0 < CHANGELOG.md
set -eu
[ $# -eq 1 ] || { echo "usage: changelog-section.sh <version> < CHANGELOG.md" >&2; exit 2; }
awk -v v="$1" 'index($0, "## [" v "]") == 1 { found = 1; next } found && /^## \[/ { exit } found { print }'
