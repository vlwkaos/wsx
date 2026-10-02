#!/usr/bin/env python3
"""Verify contextual chrome through wsxd, real PTYs, wsx, and a private tmux server.

Build adjacent wsx/wsxd first. This scenario does not inspect installed agents,
change user configuration, or use the user's daemon. See docs/terminal-context.md.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wsx", type=Path, default=ROOT / "target/debug/wsx")
    parser.add_argument("--daemon", type=Path, default=ROOT / "target/debug/wsxd")
    parser.add_argument("--keep", action="store_true", help="retain private fixture and captures")
    args = parser.parse_args()
    tmux = shutil.which("tmux")
    if not tmux:
        raise RuntimeError("tmux is required for actual TUI screen verification")
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    if not wsx.is_file() or not wsxd.is_file() or wsx.parent != wsxd.parent:
        raise RuntimeError("build adjacent wsx and wsxd binaries before this scenario")
    work = ROOT / ".work" / ("tc-" + str(os.getpid()))
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    # ^ Register scratch cleanup as soon as ownership is established, before preparation.
    try:
        result = run_scenario(args, tmux, wsx, wsxd, work)
    finally:
        if not args.keep:
            shutil.rmtree(work)
    print(json.dumps(result))


def run_scenario(args, tmux, wsx, wsxd, work):
    home, state, project = work / "home", work / "state", work / "project"
    for path in (home, state, project, work / "captures"):
        path.mkdir(mode=0o700)
    config_dir = (home / "Library/Application Support/wsx" if sys.platform == "darwin"
                  else work / "config/wsx")
    config_dir.mkdir(parents=True)
    config = config_dir / "config-v2.toml"
    config_text = ('resume_agents_on_restore = false\nshow_release_status = false\n'
                   'terminal_prefix_shows_sidebar = false\nterminal_sidebar = "compact"\n'
                   'terminal_title_position = "{position}"\n'
                   '[[projects]]\nname = "demo"\npath = ' + json.dumps(str(project)) + '\n')
    config.write_text(config_text.format(position="bottom"))
    # ^ Use an allowlist: fixture processes must not inherit credentials or pane authority.
    env = {"LANG": os.environ.get("LANG", "en_US.UTF-8")}
    env.update({"HOME": str(home), "XDG_STATE_HOME": str(state),
                "XDG_CONFIG_HOME": str(work / "config"), "XDG_CACHE_HOME": str(work / "cache"),
                "WSX_DAEMON_BIN": str(wsxd), "WSX_SOCKET": str(state / "wsx/wsx.sock"),
                "SHELL": "/bin/sh", "TERM": "xterm-256color",
                "PATH": "/usr/bin:/bin:/usr/sbin:/sbin"})
    subprocess.run(["git", "init", "-q", "-b", "main", str(project)], check=True, env=env)
    daemon_socket, tmux_socket = state / "wsx/wsx.sock", work / "t.sock"
    helper = work / "agent.py"
    helper.write_text(
        "import os,pathlib,socket,subprocess,sys\n"
        "def report(state):\n"
        " subprocess.run([os.environ['WSX_AGENT_REPORT_BIN'],'agent','report',"
        "os.environ['WSX_PANE_ID'],'--provider','codex','--state',state,'--lifecycle'],"
        "check=True,stdout=subprocess.DEVNULL)\n"
        "report(sys.argv[1])\n"
        "if len(sys.argv)>2:\n"
        " listener=socket.socket(); listener.bind(('127.0.0.1',0)); listener.listen()\n"
        " pathlib.Path(sys.argv[2]).write_text(str(listener.getsockname()[1]))\n"
        "print('fixture terminal body',flush=True)\n"
        "for line in sys.stdin:\n"
        " if line.strip(): report(line.strip())\n"
    )

    def call(method, params=None):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(5)
            client.connect(str(daemon_socket))
            stream = client.makefile("rb")
            client.sendall(b'{"method":"hello","params":{"protocol":16}}\n')
            assert json.loads(stream.readline())["type"] == "hello"
            request = {"method": method}
            if params is not None:
                request["params"] = params
            client.sendall(json.dumps(request).encode() + b"\n")
            return json.loads(stream.readline())

    def tm(*argv):
        return subprocess.run([tmux, "-S", str(tmux_socket)] + list(argv), check=True,
                              capture_output=True, text=True, env=env).stdout

    def screen(target="view"):
        return tm("capture-pane", "-p", "-t", target).splitlines()

    def primary_name(target="view", row=-2, left=2):
        words = screen(target)[row][left:].split()
        return words[1] if len(words) > 1 else ""

    def wait(test, label, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if test():
                return
            time.sleep(0.05)
        raise RuntimeError(label + " timeout")

    def capture(name, target="view"):
        rows = screen(target)
        (work / "captures" / (name + ".txt")).write_text("\n".join(rows))
        (work / "captures" / (name + ".ansi")).write_text(tm("capture-pane", "-p", "-e", "-t", target))
        return rows

    def keys(*argv, target="view"):
        tm("send-keys", "-t", target, *argv)

    def pane_dimensions(pane_id):
        reply = call("view", {"pane_ids": [pane_id]})
        assert reply["type"] == "view", reply
        frame = reply["data"]["frames"][0]
        return frame["cols"], frame["rows"]

    daemon = None
    succeeded = False
    started = time.monotonic()
    try:
        daemon = subprocess.Popen([str(wsxd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait(lambda: daemon_socket.exists(), "daemon socket")
        assert call("synchronize_projects", {"projects": [{"name": "demo", "path": str(project),
                    "worktrees": [{"path": str(project), "branch": "main"}]}]})["type"] == "ack"
        worktree_id = call("snapshot")["data"]["worktrees"][0]["id"]
        sessions = {}
        for name, status, port in [("build", "working", True), ("api", "idle", True),
                                   ("finished", "done", False), ("approval", "blocked", False)]:
            command = [sys.executable, str(helper), status]
            if port:
                command.append(str(work / (name + ".port")))
            created = call("session_create", {"worktree_id": worktree_id, "label": name,
                           "command": command, "rows": 21, "cols": 118})
            assert created["type"] == "created", created
            sessions[name] = created["data"]["id"]
        wait(lambda: all(pane["agent"] is not None for pane in call("snapshot")["data"]["panes"]),
             "generation-bound fixture reports")
        wait(lambda: len(call("snapshot")["data"].get("listening_ports", [])) >= 2, "port attribution")
        tm("new-session", "-d", "-s", "view", "-x", "120", "-y", "24", "exec " + shlex.quote(str(wsx)))
        tm("set-option", "-g", "status", "off")
        wait(lambda: "build" in "\n".join(screen()) and "WORKSPACE" in "\n".join(screen()), "Workspace")
        keys("j")
        wait(lambda: "Sessions:" in "\n".join(screen()), "worktree preview")
        rows = capture("preview")
        for name in ("build", "api"):
            port = (work / (name + ".port")).read_text()
            assert any(name in row and ":" + port in row for row in rows), rows
        assert not any("Ports:" in row for row in rows)
        keys("j", "Enter")
        wait(lambda: "TERMINAL" in "\n".join(screen()), "Terminal entry")
        snapshot = call("snapshot")["data"]
        build_pane = next(s["focused_pane"] for s in snapshot["sessions"] if s["id"] == sessions["build"])
        wait(lambda: pane_dimensions(build_pane) == (118, 21), "compact terminal dimensions")
        rows = capture("bottom-compact")
        # ^ The compact rail owns the first two cells even on the title row.
        title = rows[-2][2:]
        assert title.lstrip().startswith("◎ build") or title.lstrip().startswith("◉ build") or title.lstrip().startswith("● build"), title
        assert title.index("approval") < title.index("finished") < title.index("api"), title
        assert not any(":" + (work / (name + ".port")).read_text() in title for name in ("build", "api"))
        assert "48;2;" in (work / "captures/bottom-compact.ansi").read_text(), "RGB chrome not captured"
        for key, name in [("l", "api"), ("h", "build"), ("Right", "api"), ("Left", "build")]:
            keys("C-a", key)
            wait(lambda: primary_name() == name, "project prefix " + key)
        # Existing n keeps its broader attention behavior; the new cycle is separate.
        keys("C-a", "n")
        wait(lambda: primary_name() == "approval", "existing attention command")
        for name in ["api", "build"]:
            keys("C-a", "h")
            wait(lambda: primary_name() == name, "ranked reverse cycle")
        keys("h", "l", "Left", "Right")
        time.sleep(0.2)
        assert primary_name() == "build", "bare keys switched sessions"
        capture("project-prefix-cycle")
        keys("C-a", "b")
        wait(lambda: pane_dimensions(build_pane) == (88, 21), "expanded sidebar dimensions")
        capture("bottom-expanded")
        keys("C-a", "b")
        wait(lambda: pane_dimensions(build_pane) == (118, 21), "restored compact dimensions")
        tm("resize-window", "-t", "view", "-x", "56", "-y", "18")
        wait(lambda: pane_dimensions(build_pane) == (56, 15), "mobile dimensions")
        rows = capture("mobile")
        assert "build" in rows[-2] and "+" in rows[-2], rows[-2]
        assert "◐ approval" in rows[-2], rows[-2]
        # A real generation-authorized report changes the peer order without a UI remount.
        approval = next(s["focused_pane"] for s in snapshot["sessions"] if s["id"] == sessions["approval"])
        assert call("terminal_acquire", {"pane_id": approval, "client_id": 991, "takeover": False})["type"] == "ack"
        assert call("terminal_input", {"pane_id": approval, "client_id": 991, "bytes": list(b"idle\r")})["type"] == "ack"
        assert call("terminal_release", {"pane_id": approval, "client_id": 991})["type"] == "ack"
        wait(lambda: next(pane for pane in call("snapshot")["data"]["panes"] if pane["id"] == approval)["agent"]["state"] == "idle",
             "accepted generation-bound idle report")
        def peers_reflect_idle():
            title = screen()[-2]
            peers = title.partition("demo/main")[2].strip()
            return peers.startswith("✓ finished") and "◐ approval" not in title
        wait(peers_reflect_idle, "updated attention projection")
        capture("mobile-updated")
        keys("C-a", "w")
        wait(lambda: "WORKSPACE" in "\n".join(screen()), "leave Terminal")
        # Keep the first TUI alive so the fixture daemon and live generations remain owned.
        config.write_text(config_text.format(position="top").replace(
            "terminal_prefix_shows_sidebar = false", "terminal_prefix_shows_sidebar = true"))
        tm("new-session", "-d", "-s", "top", "-x", "120", "-y", "24", "exec " + shlex.quote(str(wsx)))
        # ^ Footer mode appears before snapshot hydration; wait for real session rows.
        wait(lambda: "WORKSPACE" in "\n".join(screen("top")) and "build" in "\n".join(screen("top")),
             "top Workspace snapshot")
        capture("top-start", "top")
        # The retained cursor may already be on build; drive the tree from its project row.
        keys("k", "k", target="top")
        keys("j", "j", "Enter", target="top")
        capture("top-after-enter", "top")
        wait(lambda: "TERMINAL" in "\n".join(screen("top")), "top Terminal")
        wait(lambda: primary_name("top", row=1) == "build", "top title")
        capture("top-compact", "top")
        keys("C-a", "l", target="top")
        wait(lambda: primary_name("top", row=1) == "api", "top ranked cycle")
        api_pane = next(s["focused_pane"] for s in snapshot["sessions"] if s["id"] == sessions["api"])
        keys("C-a", target="top")
        wait(lambda: pane_dimensions(api_pane) == (88, 21), "prefix sidebar peek")
        keys("h", target="top")
        wait(lambda: primary_name("top", row=1) == "build" and pane_dimensions(build_pane) == (118, 21),
             "project cycle restores baseline after peek")
        capture("top-prefix-peek-restored", "top")
        succeeded = True
    except Exception:
        if tmux_socket.exists():
            try:
                for target in ("view", "top"):
                    try:
                        rows = capture("failed-" + target, target)
                        print("last isolated " + target + " screen:\n" + "\n".join(rows), file=sys.stderr)
                    except subprocess.CalledProcessError:
                        pass
            except Exception:
                pass
        raise
    finally:
        if tmux_socket.exists():
            subprocess.run([tmux, "-S", str(tmux_socket), "kill-server"], env=env, capture_output=True)
        if daemon_socket.exists():
            try:
                call("shutdown")
            except Exception:
                if daemon is not None:
                    daemon.terminate()
        if daemon is not None:
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=3)
        if succeeded:
            assert daemon.returncode == 0, "isolated wsxd did not stop gracefully"
            assert not daemon_socket.exists(), "isolated daemon socket survived shutdown"
            probe = subprocess.run([tmux, "-S", str(tmux_socket), "has-session"],
                                   env=env, capture_output=True)
            assert probe.returncode != 0, "private tmux server survived cleanup"
            for name in ("build", "api"):
                with socket.socket() as client:
                    client.settimeout(1)
                    assert client.connect_ex(("127.0.0.1", int((work / (name + ".port")).read_text()))) != 0, "fixture listener survived shutdown"
    return {"result": "PASS", "elapsed_seconds": round(time.monotonic()-started, 2),
                      "captures": str(work / "captures") if args.keep else None, "model_calls": 0,
                      "cleanup": "private tmux, wsxd, PTYs and listeners stopped",
                      "journey": "preview ports, local cycle, bare keys, unchanged n, sidebar toggle and peek, mobile, live peer update, top title"}


if __name__ == "__main__":
    main()
