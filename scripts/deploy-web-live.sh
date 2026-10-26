#!/bin/sh
# tasks.deploy.command for wsx: put this commit's wsx on the hosted web site that runs from
# <main checkout>/.work/web-live under pm2 app wsx-web (docs/tasks.md "Deploy and release").
# `wsx task deploy` runs it in a temporary copy of one default-branch commit.
# Back to the previous build:  mv .work/web-live/wsx.previous .work/web-live/wsx && pm2 restart wsx-web
set -eu
main=$(git worktree list --porcelain | sed -n '1s/^worktree //p')
live="$main/.work/web-live"
test -x "$live/wsx" || { echo "no live site at $live" >&2; exit 1; }
command -v pm2 >/dev/null || { echo "pm2 is not on PATH" >&2; exit 1; }

# ^ The whole output goes to $live/deploy.log (the run before stays as deploy.log.1) and to wsx,
# which keeps only its tail. The script runs again under tee; its exit goes through a file
# because sh has no pipefail.
if [ -z "${WSX_DEPLOY_LOGGED:-}" ]; then
  [ -f "$live/deploy.log" ] && mv "$live/deploy.log" "$live/deploy.log.1"
  status="$live/deploy.status"
  { code=0; WSX_DEPLOY_LOGGED=1 sh "$0" 2>&1 || code=$?; echo "$code" >"$status"; } | tee "$live/deploy.log"
  code=$(cat "$status"); rm -f "$status"
  exit "$code"
fi
step=start
step() { step=$1; echo "== deploy step: $1 ($(date '+%H:%M:%S'))"; }
trap 'code=$?; [ $code -eq 0 ] || echo "deploy failed at step: $step (exit $code); full output in $live/deploy.log"' EXIT

# ^ The deploy gate is the PTY and stream smoke test only, so a land reaches the live site in
# minutes. The task and web end-to-end scripts are `tasks.scenarios`: the worker whose change
# touches their area runs them by name (wsx.config.yml `workflow.build`).
# ^ The smoke test holds the host-wide check slot that task checks take (crates/wsx/src/task/slot.rs,
# an flock on tasks/check-slot/slot.lock), so it never runs beside another task's checks.
in_slot() {
  python3 -c 'import fcntl, os, subprocess, sys
state = os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state")
folder = os.path.join(state, "wsx", "tasks", "check-slot")
os.makedirs(folder, mode=0o700, exist_ok=True)
slot = open(os.path.join(folder, "slot.lock"), "a")
try:
    fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    print("waiting for the check slot", flush=True)
    fcntl.flock(slot, fcntl.LOCK_EX)
sys.exit(subprocess.call(sys.argv[1:]))' "$@"
}
step "debug build"
cargo build --locked -p wsx -p wsx-daemon
step "runtime-smoke.py"
in_slot python3 -B scripts/runtime-smoke.py
step "release build"
cargo build --release --locked -p wsx -p wsx-daemon
# ^ wsxd sits next to wsx so the web build and the daemon it hands off to stay one revision.
step install
cp target/release/wsxd "$live/wsxd.new"
mv "$live/wsxd.new" "$live/wsxd"
cp target/release/wsx "$live/wsx.new"
rm -f "$live/wsx.previous"
mv "$live/wsx" "$live/wsx.previous"
mv "$live/wsx.new" "$live/wsx"   # ^ replace, never overwrite in place: hosts exec the new file
echo "installed ${WSX_DEPLOY_COMMIT:-this commit} at $live/wsx"

# ^ The deploy may run under the web server itself (an approval tapped in the web client), and pm2
# kills the app's process tree on restart. Restart once `wsx task deploy` has recorded the deploy
# and removed this temporary copy (at most 120 s), from a process outside that tree.
copy=$(pwd)
log="$live/restart.log"
# ^ cd out first: pm2 (node) exits with ENOENT when its working directory, this copy, is gone.
cd "$live"
nohup sh -c 'i=0; while [ -d "$1" ] && [ $i -lt 120 ]; do sleep 1; i=$((i+1)); done
  date; echo "copy gone after ${i}s"; pm2 restart wsx-web; echo "pm2 exit $?"' \
  restart "$copy" >"$log" 2>&1 </dev/null &
echo "wsx-web restarts after the deploy is recorded; see $log"
