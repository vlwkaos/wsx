#!/bin/sh
# managed by wsx
# WSX_INTEGRATION_VERSION=@VERSION@
set -eu
action="${1:-unknown}"
[ -n "${WSX_PANE_ID:-}" ] || exit 0
case "$action" in idle|working|blocked|done|error|unknown) state="$action";; session|detached) state=unknown;; heartbeat) state=working;; *) exit 0;; esac
input="$(cat 2>/dev/null || true)"
conversation=""
prompt_id=""
if command -v python3 >/dev/null 2>&1; then
  if ! metadata="$(printf '%s' "$input" | WSX_PROVIDER="@PROVIDER@" python3 -c 'import json,os,sys
try:
 d=json.load(sys.stdin)
 if os.environ.get("WSX_PROVIDER") == "claude" and d.get("agent_id"):
  raise SystemExit(1)
 conversation=next((d[k] for k in ("session_id","conversation_id","conversationId","sessionId") if isinstance(d.get(k),str)),"")
 prompt=d.get("prompt_id","")
 print(conversation+"|"+(prompt if isinstance(prompt,str) else ""))
except (TypeError, ValueError): pass' 2>/dev/null)"; then
    exit 0
  fi
  conversation="${metadata%%|*}"
  prompt_id="${metadata#*|}"
fi
if [ "$action" = "heartbeat" ]; then
  [ "@PROVIDER@" = "claude" ] && [ -n "$prompt_id" ] || exit 0
  exec "${WSX_AGENT_REPORT_BIN:-wsx}" agent wake-heartbeat "$WSX_PANE_ID" --prompt-id "$prompt_id"
fi
set -- agent report "$WSX_PANE_ID" --provider "@PROVIDER@" --state "$state"
[ "@LIFECYCLE@" = "yes" ] && set -- "$@" --lifecycle
[ "$action" = "detached" ] && set -- "$@" --detached
[ "@PROVIDER@" = "claude" ] && set -- "$@" --escape-interrupts
[ "@PROVIDER@" = "claude" ] && [ "$state" = "working" ] && [ -n "$prompt_id" ] && set -- "$@" --wake-token "$prompt_id"
[ -n "$conversation" ] && set -- "$@" --session-id "$conversation"
"${WSX_AGENT_REPORT_BIN:-wsx}" "$@" >/dev/null 2>&1 || true
