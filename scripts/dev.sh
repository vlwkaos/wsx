#!/usr/bin/env bash
# A separate wsx of this checkout's debug build, beside the installed one: its own wsxd, projects,
# chats, tasks and local web site under .work/dev (`WSX_HOME`, docs/daemon-discovery.md "Folders"). HOME stays
# yours, so every agent keeps its own settings and sign-in, and git its identity.
#   scripts/dev.sh          build, start wsxd and the web site, open the TUI
#   scripts/dev.sh web      the same, then open the web page in the browser (paired)
#   scripts/dev.sh env      print the exports; `eval "$(scripts/dev.sh env)"` runs wsx commands here
#   scripts/dev.sh stop     stop the web site and wsxd
#   scripts/dev.sh reset    stop, then delete .work/dev
# The installed wsx and its live sessions are never touched: a newer client against the installed
# wsxd would hand that daemon over to this build (AGENTS.md "Daemon lifecycle coordination").
# A device whose main wsx is the dev build itself deploys with scripts/deploy-web-live.sh instead.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DEV="$REPO/.work/dev"
PORT="${WSX_DEV_PORT:-4699}"
SITE=dev

exports() {
  cat <<EOF
# ^ WSX_SOCKET too: an agent hook started from this shell, outside a pane, finds the dev wsxd by it.
export WSX_HOME="$DEV" WSX_SOCKET="$DEV/state/wsx.sock" WSX_DAEMON_BIN="$REPO/target/debug/wsxd"
export PATH="$REPO/target/debug:$PATH"
unset WSX_PANE_ID
EOF
}

# One-time layout; each run repairs what is missing.
setup() {
  mkdir -p "$DEV"/{state,cache,project} "$DEV/config/web"
  chmod 700 "$DEV" "$DEV/state" "$DEV/config/web"
  local config="$DEV/config/config-v2.toml"
  [ -f "$config" ] || printf '[[projects]]\nname = "scratch"\npath = "%s"\n' "$DEV/project" >"$config"
  if [ ! -d "$DEV/project/.git" ]; then
    git -C "$DEV/project" init -q -b main
    printf '# scratch\n' >"$DEV/project/README.md"
    git -C "$DEV/project" add README.md
    git -C "$DEV/project" commit -q -m start
  fi
  local site="$DEV/config/web/$SITE.toml"
  cat >"$site" <<EOF
id = "$SITE"
name = "wsx dev"
port = $PORT
public_origin = "http://127.0.0.1:$PORT"
panels = ["chat", "files", "terminal"]
theme = "default"

[access]
projects = "all"
capabilities = [
  "chat_send", "chat_answer", "agent_write", "session_start",
  "files_read", "files_write", "terminal_view", "terminal_input",
  "media_upload", "media_download", "task_view", "task_act",
]
agent_command = ["claude", "--permission-mode", "auto"]
EOF
  chmod 600 "$site"
}

daemon_running() {
  # ^ Read whole first: `grep -q` stops early, and under pipefail the SIGPIPE fails the check.
  local status
  status="$(wsx daemon status 2>/dev/null || true)"
  [[ "$status" == "Runtime: running"* ]]
}

serving() {
  [ "$(curl -s --noproxy 127.0.0.1 -o /dev/null -w '%{http_code}' -H "Host: 127.0.0.1:$PORT" \
    "http://127.0.0.1:$PORT/manifest.webmanifest" || true)" = 200 ]
}

stop_web() {
  local pid
  pid="$(cat "$DEV/web.pid" 2>/dev/null || true)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid"
    while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
  fi
  rm -f "$DEV/web.pid"
}

start() {
  (cd "$REPO" && cargo build -q -p wsx -p wsx-daemon)
  setup
  eval "$(exports)"
  # ^ `web serve` never starts wsxd; `daemon recover` starts it through the normal bootstrap.
  # `daemon status` exits 0 also when wsxd is stopped: its Runtime line says which.
  daemon_running || wsx daemon recover >/dev/null
  # ^ The project's first chat is its main chat with no goal: the lead (docs/tasks.md "Lead and forks").
  wsx chat list -p scratch --json | grep -q '"main": true' || wsx chat new lead -p scratch >/dev/null
  # ^ The page files are built into wsx: a rebuilt binary needs a new server.
  stop_web
  if serving; then
    echo "port $PORT is taken by another process; set WSX_DEV_PORT" >&2
    exit 1
  fi
  nohup wsx web serve "$SITE" >"$DEV/web.log" 2>&1 &
  echo $! >"$DEV/web.pid"
  local tries=0
  until serving; do
    tries=$((tries + 1))
    if [ "$tries" -gt 60 ] || ! kill -0 "$(cat "$DEV/web.pid")" 2>/dev/null; then
      echo "the web site did not start; $DEV/web.log:" >&2
      tail -5 "$DEV/web.log" >&2
      exit 1
    fi
    sleep 0.25
  done
  echo "wsx dev $(wsx --version | cut -d' ' -f2): web http://127.0.0.1:$PORT, project scratch, chat lead"
}

case "${1:-tui}" in
  tui) start; exec wsx ;;
  web) start; wsx web open "$SITE" --pair ;;
  env) exports ;;
  stop)
    stop_web
    if [ -d "$DEV" ]; then
      eval "$(exports)"
      # ^ Every session here is this instance's own (the chats' hosts, sessions started in it):
      # --force ends them, which a plain stop refuses while one runs.
      if daemon_running; then wsx daemon stop --force >/dev/null; fi
    fi
    echo "wsx dev stopped"
    ;;
  reset)
    "$0" stop
    rm -rf "$DEV"
    echo "removed $DEV"
    ;;
  *) sed -n '2,10p' "$0" >&2; exit 2 ;;
esac
