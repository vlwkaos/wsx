#!/bin/sh
# managed by wsx
# WSX_INTEGRATION_VERSION=@VERSION@
set -eu
action="${1:-unknown}"
[ -n "${WSX_PANE_ID:-}" ] || exit 0
case "$action" in idle|working|blocked|done|error|unknown) state="$action";; session) state=unknown;; *) exit 0;; esac
input="$(cat 2>/dev/null || true)"
conversation=""
if command -v python3 >/dev/null 2>&1; then
  conversation="$(printf '%s' "$input" | python3 -c 'import json,sys
try:
 d=json.load(sys.stdin)
 print(next((d[k] for k in ("session_id","conversation_id","conversationId","sessionId") if isinstance(d.get(k),str)),""))
except Exception: pass' 2>/dev/null || true)"
fi
set -- agent report "$WSX_PANE_ID" --provider "@PROVIDER@" --state "$state"
[ "@LIFECYCLE@" = "yes" ] && set -- "$@" --lifecycle
[ -n "$conversation" ] && set -- "$@" --session-id "$conversation"
"${WSX_AGENT_REPORT_BIN:-wsx}" "$@" >/dev/null 2>&1 || true
