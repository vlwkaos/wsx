"""wsx identity integration for Hermes Agent."""
# WSX_INTEGRATION_VERSION=5
import os, subprocess

def _report(**kw):
    pane=os.environ.get("WSX_PANE_ID"); sid=kw.get("session_id")
    if not pane or not isinstance(sid,str) or not sid: return
    try: subprocess.run([os.environ.get("WSX_AGENT_REPORT_BIN") or "wsx","agent","report",pane,"--provider","hermes","--state","unknown","--conversation-id",sid],timeout=1,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    except Exception: pass

def register(ctx):
    ctx.register_hook("on_session_start", _report)
    ctx.register_hook("on_session_reset", _report)
    ctx.register_hook("pre_llm_call", _report)
