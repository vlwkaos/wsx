"""wsx identity integration for Hermes Agent."""
# WSX_INTEGRATION_VERSION=7
import os, subprocess

def _report(detached=False, **kw):
    pane=os.environ.get("WSX_PANE_ID"); sid=kw.get("session_id")
    if not pane or (not detached and (not isinstance(sid,str) or not sid)): return
    args=[os.environ.get("WSX_AGENT_REPORT_BIN") or "wsx","agent","report",pane,"--provider","hermes","--state","unknown"]
    if isinstance(sid,str) and sid: args.extend(["--session-id",sid])
    if detached: args.append("--detached")
    try: subprocess.run(args,timeout=1,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    except Exception: pass

def _detach(**kw):
    _report(detached=True, **kw)

def register(ctx):
    ctx.register_hook("on_session_start", _report)
    ctx.register_hook("on_session_reset", _report)
    ctx.register_hook("on_session_finalize", _detach)
    ctx.register_hook("pre_llm_call", _report)
