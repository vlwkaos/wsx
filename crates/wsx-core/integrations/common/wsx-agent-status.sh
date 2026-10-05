#!/bin/sh
# managed by wsx
# WSX_INTEGRATION_VERSION=@VERSION@
set -eu
action="${1:-unknown}"
[ -n "${WSX_PANE_ID:-}" ] || exit 0
# ^ docs/agent-reporting.md: recover only a removed absolute legacy reporter.
report_bin="${WSX_AGENT_REPORT_BIN:-wsx}"
case "$report_bin" in
  /*) if [ ! -e "$report_bin" ]; then
    # Refuse untrusted entries; resolve the installed CLI before daemon handoff.
    if command -v python3 >/dev/null 2>&1; then
      stable="$(python3 "$(dirname "$0")/wsx-reporter.py" "$report_bin" 2>/dev/null || true)"
      [ -z "$stable" ] || report_bin="$stable"
    fi
  fi ;;
esac
case "$action" in idle|working|blocked|done|error|unknown) state="$action";; session|detached) state=unknown;; heartbeat) state=working;; *) exit 0;; esac
input="$(cat 2>/dev/null || true)"
conversation=""
prompt_id=""
transcript=""
if command -v python3 >/dev/null 2>&1; then
  if ! metadata="$(printf '%s' "$input" | WSX_PROVIDER="@PROVIDER@" python3 -c 'import json,os,sys
try:
 d=json.load(sys.stdin)
 if os.environ.get("WSX_PROVIDER") == "claude" and d.get("agent_id"):
  raise SystemExit(1)
 conversation=next((d[k] for k in ("session_id","conversation_id","conversationId","sessionId") if isinstance(d.get(k),str)),"")
 prompt=d.get("prompt_id","")
 transcript=d.get("transcript_path","") if os.environ.get("WSX_PROVIDER") == "claude" else ""
 values=[conversation,prompt if isinstance(prompt,str) else "",transcript if isinstance(transcript,str) else ""]
 if any("|" in value or any(ord(c)<32 for c in value) for value in values): raise SystemExit(1)
 print("|".join(values))
except (TypeError, ValueError): pass' 2>/dev/null)"; then
    exit 0
  fi
  conversation="${metadata%%|*}"
  remainder="${metadata#*|}"
  prompt_id="${remainder%%|*}"
  transcript="${remainder#*|}"
fi
if [ "$action" = "heartbeat" ]; then
  [ "@PROVIDER@" = "claude" ] && [ -n "$prompt_id" ] || exit 0
  exec "$report_bin" agent wake-heartbeat "$WSX_PANE_ID" --prompt-id "$prompt_id"
fi
set -- agent report "$WSX_PANE_ID" --provider "@PROVIDER@" --state "$state"
[ "@LIFECYCLE@" = "yes" ] && set -- "$@" --lifecycle
[ "$action" = "detached" ] && set -- "$@" --detached
[ "@PROVIDER@" = "claude" ] && set -- "$@" --escape-interrupts --prompt
[ "@PROVIDER@" = "claude" ] && [ "$state" = "working" ] && [ -n "$prompt_id" ] && set -- "$@" --wake-token "$prompt_id"
[ -n "$conversation" ] && set -- "$@" --session-id "$conversation"
[ -n "$conversation" ] && [ -n "$transcript" ] && set -- "$@" --transcript-path "$transcript"
# ^ A rejected lifecycle report must not look like a healthy hook. Keep the
# diagnostic bounded and avoid echoing vendor input or terminal contents.
if result=$("$report_bin" "$@" 2>&1); then
  exit 0
fi
case "$result" in
  *stale_runtime*) reason=stale_runtime ;;
  *not_found*) reason=pane_not_found ;;
  *'Connection refused'*|*'No such file or directory'*) reason=daemon_unreachable ;;
  *) reason=report_failed ;;
esac
printf 'wsx %s %s report failed (%s)\n' '@PROVIDER@' "$action" "$reason" >&2
exit 1
