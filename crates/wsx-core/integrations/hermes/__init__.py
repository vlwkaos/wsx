"""wsx identity integration for Hermes Agent."""
# WSX_INTEGRATION_VERSION=8
import os, stat, subprocess

def _report(detached=False, **kw):
    pane=os.environ.get("WSX_PANE_ID"); sid=kw.get("session_id")
    if not pane or (not detached and (not isinstance(sid,str) or not sid)): return
    args=[os.environ.get("WSX_AGENT_REPORT_BIN") or "wsx","agent","report",pane,"--provider","hermes","--state","unknown"]
    if isinstance(sid,str) and sid: args.extend(["--session-id",sid])
    if detached: args.append("--detached")
    try: subprocess.run(args,timeout=1,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    except FileNotFoundError:
        # ^ docs/agent-reporting.md: no replay after a delivered or rejected report.
        if not os.path.isabs(args[0]): return
        root=os.environ.get("XDG_STATE_HOME") or os.path.join(os.environ.get("HOME", ""), ".local/state")
        socket=os.environ.get("WSX_SOCKET") or os.path.join(root, "wsx/wsx.sock")
        stable=os.path.splitext(socket)[0]+".reporter"
        if not os.path.isabs(stable) or stable == args[0]: return
        try:
            directory=os.lstat(os.path.dirname(stable)); entry=os.lstat(stable); target=os.stat(stable); uid=os.geteuid()
            if not stat.S_ISDIR(directory.st_mode) or directory.st_uid!=uid or directory.st_mode&0o077: return
            if not stat.S_ISLNK(entry.st_mode) or entry.st_uid!=uid: return
            if not stat.S_ISREG(target.st_mode) or target.st_uid not in (0,uid) or target.st_mode&0o022 or not target.st_mode&0o111: return
            args[0]=stable
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
