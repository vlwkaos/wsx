#!/bin/sh
# managed by wsx
# WSX_INTEGRATION_VERSION=@VERSION@
set -eu
action="${1:-unknown}"
if [ -z "${WSX_PANE_ID:-}" ]; then
  exit 0
fi
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
compaction=""
received=""
compacting=""
if command -v python3 >/dev/null 2>&1; then
  if ! metadata="$(printf '%s' "$input" | WSX_PROVIDER="@PROVIDER@" python3 -c 'import json,os,sys
try:
 d=json.load(sys.stdin)
 if os.environ.get("WSX_PROVIDER") == "claude" and d.get("agent_id"):
  raise SystemExit(1)
 conversation=next((d[k] for k in ("session_id","conversation_id","conversationId","sessionId") if isinstance(d.get(k),str)),"")
 prompt=d.get("prompt_id","")
 transcript=d.get("transcript_path","") if os.environ.get("WSX_PROVIDER") == "claude" else ""
 # The session start after any compaction (Claude Code 2.1.300 sends source "compact").
 compaction=os.environ.get("WSX_PROVIDER") == "claude" and d.get("source") == "compact"
 # The prompt a Claude turn takes: a wsx round by its exchange line, as typed or inside the
 # paste wrapper Claude saves it in, else another prompt (docs/agent-orchestration.md "Taken, not typed").
 received=""
 if os.environ.get("WSX_PROVIDER") == "claude" and d.get("hook_event_name") == "UserPromptSubmit" and isinstance(d.get("prompt"),str):
  import re
  m=re.match(r"\s*(?:<pasted_content [^>]*>\s*)?\[wsx exchange (\d+), round (\d+), ",d["prompt"])
  received=m.group(1)+":"+m.group(2) if m else "other"
 # Every compaction runs from its PreCompact to its PostCompact: a /compact a person types, an automatic
 # one inside a turn, and an idle one between turns (Claude Code 2.1.310 "Compacted while idle").
 compacting=d.get("hook_event_name") if os.environ.get("WSX_PROVIDER") == "claude" and d.get("hook_event_name") in ("PreCompact","PostCompact") else ""
 values=[conversation,prompt if isinstance(prompt,str) else "",transcript if isinstance(transcript,str) else "","1" if compaction else "",received,compacting]
 if any("|" in value or any(ord(c)<32 for c in value) for value in values): raise SystemExit(1)
 print("|".join(values))
except (TypeError, ValueError): pass' 2>/dev/null)"; then
    exit 0
  fi
  conversation="${metadata%%|*}"
  remainder="${metadata#*|}"
  prompt_id="${remainder%%|*}"
  remainder="${remainder#*|}"
  transcript="${remainder%%|*}"
  remainder="${remainder#*|}"
  compaction="${remainder%%|*}"
  remainder="${remainder#*|}"
  received="${remainder%%|*}"
  compacting="${remainder#*|}"
fi
# ^ docs/agent-reporting.md "Claude compaction": Claude starts its session again after every
# compaction, also an automatic one inside a turn. That start must not end the turn; every
# compaction is reported by its own PreCompact and PostCompact.
if [ "$compaction" = "1" ]; then
  exit 0
fi
if [ "$action" = "heartbeat" ]; then
  [ "@PROVIDER@" = "claude" ] && [ -n "$prompt_id" ] || exit 0
  exec "$report_bin" agent wake-heartbeat "$WSX_PANE_ID" --prompt-id "$prompt_id"
fi
set -- agent report "$WSX_PANE_ID" --provider "@PROVIDER@" --state "$state"
[ "@LIFECYCLE@" = "yes" ] && set -- "$@" --lifecycle
[ "$action" = "detached" ] && set -- "$@" --detached
[ "@PROVIDER@" = "claude" ] && set -- "$@" --escape-interrupts --prompt --prompt-receipts
[ -n "$received" ] && [ "$state" = "working" ] && set -- "$@" --received "$received"
# ^ wsxd keeps the turn of a compaction inside it and returns one outside a turn to its earlier state.
[ "$compacting" = "PreCompact" ] && set -- "$@" --compacting
[ "$compacting" = "PostCompact" ] && set -- "$@" --compacted
[ "@PROVIDER@" = "claude" ] && [ "$state" = "working" ] && [ -n "$prompt_id" ] && set -- "$@" --wake-token "$prompt_id"
[ -n "$conversation" ] && set -- "$@" --session-id "$conversation"
[ -n "$conversation" ] && [ -n "$transcript" ] && set -- "$@" --transcript-path "$transcript"
result=$("$report_bin" "$@" 2>&1) && exit 0
# ^ docs/agent-reporting.md "Hook exit": a pane the daemon no longer knows, a pane runtime that
# is stale or gone, or an unreachable daemon or reporter is expected after a restart or upgrade
# and stays quiet.
# Any other rejection must not look like a healthy hook; keep its line bounded, with no
# vendor input or terminal contents.
case "$result" in
  *stale_runtime*|*terminal_unavailable*|*not_found*|*'Connection refused'*|*'No such file or directory'*) exit 0 ;;
esac
printf 'wsx %s %s report failed (report_failed)\n' '@PROVIDER@' "$action" >&2
exit 1
