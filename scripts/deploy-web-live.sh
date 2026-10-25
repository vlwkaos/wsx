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

# ^ The end-to-end gate runs here, once for the newest commit after a batch of lands, not in
# each task's checks. A failure stops the deploy and the manager sees it in the ledger.
cargo build --locked -p wsx -p wsx-daemon
python3 -B scripts/test-task.py
python3 -B scripts/test-web-client.py --browser
python3 -B scripts/runtime-smoke.py
cargo build --release --locked -p wsx -p wsx-daemon
# ^ wsxd sits next to wsx so the web build and the daemon it hands off to stay one revision.
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
