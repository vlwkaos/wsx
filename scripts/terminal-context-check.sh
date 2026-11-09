#!/bin/sh
# The live handoff scenario (docs/daemon-live-upgrade.md), in the host-wide scenario test slot.
# `scripts/deploy-web-live.sh` runs it after every land and `scripts/release.sh` before a release.
# It opens PTYs and daemons, so a task cannot run it inside wsx's boundary.
# Usage: sh scripts/terminal-context-check.sh [<version to leave out of the older releases>]
# With an older wsx installed under Homebrew the scenario includes the upgrade from it; with
# none it runs without that leg and says why. The caller built the debug wsx and wsxd.
set -eu
cellar=$(brew --cellar wsx 2>/dev/null || true)
older=$( { [ -n "$cellar" ] && ls "$cellar" 2>/dev/null || true; } | grep -Fvx "${1:-}" \
  | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)
if [ -n "$older" ] && [ -x "$cellar/$older/bin/wsx" ] && [ -x "$cellar/$older/bin/wsxd" ]; then
  echo "upgrade leg: from the installed release $older"
  exec python3 -B scripts/check-slot.py python3 -B scripts/test-terminal-context.py \
    --released-wsx "$cellar/$older/bin/wsx" --released-daemon "$cellar/$older/bin/wsxd"
fi
echo "upgrade leg skipped: no older wsx release is installed under Homebrew (${cellar:-no brew})"
exec python3 -B scripts/check-slot.py python3 -B scripts/test-terminal-context.py
