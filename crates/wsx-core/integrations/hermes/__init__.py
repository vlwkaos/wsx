"""wsx identity integration for Hermes Agent."""
# WSX_INTEGRATION_VERSION=9
import os, runpy, subprocess

# The installer places the shared resolver beside this plugin.
_resolve_reporter = runpy.run_path(os.path.join(os.path.dirname(__file__), "../common/wsx-reporter.py"))["resolve_reporter"]

def _report(detached=False, **kw):
    pane=os.environ.get("WSX_PANE_ID"); sid=kw.get("session_id")
    if not pane or (not detached and (not isinstance(sid,str) or not sid)): return
    args=[os.environ.get("WSX_AGENT_REPORT_BIN") or "wsx","agent","report",pane,"--provider","hermes","--state","unknown"]
    if isinstance(sid,str) and sid: args.extend(["--session-id",sid])
    if detached: args.append("--detached")
    try: subprocess.run(args,timeout=1,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    except FileNotFoundError:
        # ^ docs/agent-reporting.md: no replay after a delivered or rejected report.
        fallback = _resolve_reporter(args[0])
        if not fallback: return
        try:
            args[0]=fallback
            subprocess.run(args,timeout=1,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        except Exception: pass
    except Exception: pass

def _detach(**kw):
    _report(detached=True, **kw)

def register(ctx):
    ctx.register_hook("on_session_start", _report)
    ctx.register_hook("on_session_reset", _report)
    ctx.register_hook("on_session_finalize", _detach)
    ctx.register_hook("pre_llm_call", _report)
