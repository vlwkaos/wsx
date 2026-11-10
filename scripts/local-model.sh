#!/bin/sh
# The local fallback model server: llama-server with LFM3-1.7B Q8_0 on loopback, API key in a
# mode-600 file. Recipe and reasons: docs/local-model.md, docs/design/local-model-fallback.md "2.6".
#   local-model.sh start|stop|status|health   run it by hand (background, pid file)
#   local-model.sh run                        foreground; what launchd runs
#   local-model.sh install [--no-load]        write ~/Library/LaunchAgents/com.wsx.local-model.plist, load it
#   local-model.sh uninstall                  unload and remove it
# Settings come from the environment; the defaults are the measured ones.
set -eu

port=${LOCAL_MODEL_PORT:-8090}
dir=${LOCAL_MODEL_DIR:-$HOME/.local/state/local-model}
ctx=${LOCAL_MODEL_CTX:-32768}
idle=${LOCAL_MODEL_IDLE_SECONDS:-600}
# ^ The file is the Hugging Face cache copy of LiquidAI/LFM3-1.7B-GGUF, sha256 8f6b2418...20b6.
model=${LOCAL_MODEL_FILE:-$HOME/models/hub/models--LiquidAI--LFM3-1.7B-GGUF/snapshots/bad5729bba61d076fff20d2548e62526665af215/LFM3-1.7B-Q8_0.gguf}
server=${LOCAL_MODEL_SERVER:-/opt/homebrew/bin/llama-server}
key_file=$dir/api-key
pid_file=$dir/server.pid
log_file=$dir/server.log
label=com.wsx.local-model
plist=$HOME/Library/LaunchAgents/$label.plist
base=http://127.0.0.1:$port

key() { cat "$key_file"; }
alive() { [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null; }
get() { curl -sS -m 5 -H "Authorization: Bearer $(key)" "$base$1"; }

prepare() {
  [ -f "$model" ] || { echo "model file missing: $model" >&2; exit 1; }
  [ -x "$server" ] || { echo "llama-server missing: $server" >&2; exit 1; }
  umask 077
  mkdir -p "$dir"
  [ -s "$key_file" ] || { openssl rand -hex 24 >"$key_file"; }
  chmod 600 "$key_file"
}

# ^ --jinja: the model's own template makes tool calls. --reasoning off: LFM3 thinks by default and
# a weak model should not (a per-request reasoning_effort "none" works too). -np 1 --cache-ram 1024:
# one slot, prefix reuse on (default), a bounded side cache. --sleep-idle-seconds: the model is
# unloaded after that idle time and loads again on the next request.
serve() {
  exec "$server" -m "$model" --host 127.0.0.1 --port "$port" --api-key-file "$key_file" \
    --jinja --reasoning off -c "$ctx" -np 1 --cache-ram 1024 --sleep-idle-seconds "$idle" \
    --no-webui --alias lfm3-1.7b
}

case ${1:-} in
  run)
    prepare
    serve
    ;;
  start)
    prepare
    alive && { echo "already running, pid $(cat "$pid_file")"; exit 0; }
    nohup "$0" run >>"$log_file" 2>&1 &
    echo $! >"$pid_file"
    n=0
    until curl -sS -m 2 -f -o /dev/null "$base/health" 2>/dev/null; do
      alive || { echo "server exited; see $log_file" >&2; rm -f "$pid_file"; exit 1; }
      n=$((n + 1)); [ $n -lt 60 ] || { echo "no answer after 60 s; see $log_file" >&2; exit 1; }
      sleep 1
    done
    echo "started, pid $(cat "$pid_file"), $base/v1"
    ;;
  stop)
    alive || { echo "not running (by this script)"; rm -f "$pid_file"; exit 0; }
    pid=$(cat "$pid_file")
    # ^ A stale pid file after a reboot may name an unrelated process: kill only the listener of our port.
    lsof -nP -t -iTCP:"$port" -sTCP:LISTEN 2>/dev/null | grep -qx "$pid" ||
      { echo "pid $pid does not listen on $port; removed the stale pid file"; rm -f "$pid_file"; exit 0; }
    kill "$pid"
    n=0
    while kill -0 "$pid" 2>/dev/null; do
      n=$((n + 1)); [ $n -lt 20 ] || { kill -9 "$pid"; break; }
      sleep 1
    done
    rm -f "$pid_file"
    echo stopped
    ;;
  status)
    if alive; then echo "pid file: running, pid $(cat "$pid_file")"; else echo "pid file: not running"; fi
    echo "port: $port  context: $ctx  model: $model"
    echo "key file: $(stat -f '%Sp %N' "$key_file" 2>/dev/null || echo missing)"
    echo "launchd: $(launchctl print "gui/$(id -u)/$label" 2>/dev/null | sed -n 's/^[[:space:]]*state = /state /p' | head -1)"
    [ -f "$key_file" ] && curl -sS -m 3 "$base/props" -H "Authorization: Bearer $(key)" 2>/dev/null |
      jq -r '"server: up, n_ctx \(.default_generation_settings.n_ctx), slots \(.total_slots), sleeping \(.is_sleeping), \(.build_info)"' 2>/dev/null || echo "server: no answer"
    ;;
  health)
    # ^ Exit 0 only when the server answers /health and lists the model with the key. A sleeping
    # server answers /health and wakes on the next chat call.
    curl -sS -m 5 -o /dev/null -f "$base/health" || { echo "health: no answer on $base"; exit 1; }
    get /v1/models | jq -e '.data[0].id' >/dev/null || { echo "health: /v1/models failed with the key"; exit 1; }
    code=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$base/v1/models")
    [ "$code" = 401 ] || { echo "health: /v1/models without a key answered $code, want 401"; exit 1; }
    echo "health: ok ($(get /v1/models | jq -r '.data[0].id'))"
    ;;
  install)
    prepare
    script=${LOCAL_MODEL_SCRIPT:-$(cd "$(dirname "$0")" && pwd)/$(basename "$0")}
    # ^ RunAtLoad: starts at login. KeepAlive on a non-zero exit only, and a missing script exits 0,
    # so a plist written before the script is merged does not loop.
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array><string>/bin/sh</string><string>-c</string><string>[ -x '$script' ] || exit 0; exec '$script' run</string></array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>LOCAL_MODEL_PORT</key><string>$port</string>
    <key>LOCAL_MODEL_CTX</key><string>$ctx</string>
    <key>LOCAL_MODEL_DIR</key><string>$dir</string>
    <key>LOCAL_MODEL_FILE</key><string>$model</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>$log_file</string>
  <key>StandardErrorPath</key><string>$log_file</string>
</dict>
</plist>
EOF
    plutil -lint "$plist"
    if [ "${2:-}" != --no-load ]; then
      launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
      launchctl bootstrap "gui/$(id -u)" "$plist"
      echo "loaded $label"
    fi
    ;;
  uninstall)
    launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    rm -f "$plist"
    echo "removed $label"
    ;;
  *)
    echo "usage: $0 start|stop|status|health|run|install [--no-load]|uninstall" >&2
    exit 2
    ;;
esac
