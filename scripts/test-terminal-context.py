#!/usr/bin/env python3
"""Verify contextual chrome through wsxd, real PTYs, wsx, and a private tmux server.

Build adjacent wsx/wsxd first. This scenario does not inspect installed agents,
change user configuration, or use the user's daemon. See docs/terminal-context.md.
"""
import argparse
import json
import os
import re
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import threading
import time

import wsxd_client

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wsx", type=Path, default=ROOT / "target/debug/wsx")
    parser.add_argument("--daemon", type=Path, default=ROOT / "target/debug/wsxd")
    parser.add_argument("--keep", action="store_true", help="retain private fixture and captures")
    # docs/daemon-live-upgrade.md: optional real binaries of another build or release.
    parser.add_argument("--next-wsx", type=Path, help="wsx of a build with a higher daemon revision")
    parser.add_argument("--next-daemon", type=Path, help="wsxd adjacent to --next-wsx")
    parser.add_argument("--released-wsx", type=Path, help="wsx of the previous release")
    parser.add_argument("--released-daemon", type=Path, help="wsxd of the previous release")
    args = parser.parse_args()
    if bool(args.next_wsx) != bool(args.next_daemon) or bool(args.released_wsx) != bool(args.released_daemon):
        raise RuntimeError("give wsx and wsxd of another build together")
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
        released = None
        if args.released_wsx:
            released = run_released_upgrade(tmux, args.released_wsx.resolve(),
                                            args.released_daemon.resolve(), wsx, wsxd, work / "r",
                                            args.next_daemon and args.next_daemon.resolve())
        result = run_scenario(args, tmux, wsx, wsxd, work)
        result["released_upgrade"] = released
    finally:
        if not args.keep:
            shutil.rmtree(work)
    print(json.dumps(result))


CONFIG_TEXT = ('resume_agents_on_restore = false\nshow_release_status = false\n'
               'terminal_prefix_shows_sidebar = false\nterminal_sidebar = "compact"\n'
               'terminal_title_position = "{position}"\n'
               '[[projects]]\nname = "demo"\npath = {project}\n')


class Fixture:
    """One private home, state directory, Git project, wsxd socket and tmux server."""

    def __init__(self, work, tmux, daemon_bin):
        self.work, self.tmux = work, tmux
        home, state, self.project = work / "home", work / "state", work / "project"
        for path in (home, state, self.project, work / "captures"):
            path.mkdir(mode=0o700, parents=True)
        config_dir = (home / "Library/Application Support/wsx" if sys.platform == "darwin"
                      else work / "config/wsx")
        config_dir.mkdir(parents=True)
        self.config = config_dir / "config-v2.toml"
        self.configure(position="bottom")
        self.daemon_socket, self.tmux_socket = state / "wsx/wsx.sock", work / "t.sock"
        # ^ Use an allowlist: fixture processes must not inherit credentials or pane authority.
        self.env = {"LANG": os.environ.get("LANG", "en_US.UTF-8")}
        self.env.update({"HOME": str(home), "XDG_STATE_HOME": str(state),
                         "XDG_CONFIG_HOME": str(work / "config"), "XDG_CACHE_HOME": str(work / "cache"),
                         "WSX_DAEMON_BIN": str(daemon_bin), "WSX_SOCKET": str(self.daemon_socket),
                         "SHELL": "/bin/sh", "TERM": "xterm-256color",
                         "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
                         # ^ A fixture git command must never reach the repository this runs in.
                         "GIT_CEILING_DIRECTORIES": str(work)})
        subprocess.run(["git", "init", "-q", "-b", "main", str(self.project)], check=True, env=self.env)

    def configure(self, position, replace=None):
        text = CONFIG_TEXT.format(position=position, project=json.dumps(str(self.project)))
        self.config.write_text(text.replace(*replace) if replace else text)

    def call(self, method, params=None):
        return wsxd_client.call(self.daemon_socket, method, params)

    def epoch(self):
        try:
            return self.call("lifecycle_status")["data"]["epoch"]
        except (OSError, ValueError, AssertionError):
            return None

    def cli(self, binary, *argv, daemon_bin=None):
        env = dict(self.env, WSX_DAEMON_BIN=str(daemon_bin)) if daemon_bin else self.env
        return subprocess.run([str(binary)] + list(argv), cwd=self.project, env=env,
                              capture_output=True, text=True, timeout=60)

    def tm(self, *argv):
        return subprocess.run([self.tmux, "-S", str(self.tmux_socket)] + list(argv), check=True,
                              capture_output=True, text=True, env=self.env).stdout

    def screen(self, target="view"):
        return self.tm("capture-pane", "-p", "-t", target).splitlines()

    def capture(self, name, target="view"):
        rows = self.screen(target)
        (self.work / "captures" / (name + ".txt")).write_text("\n".join(rows))
        (self.work / "captures" / (name + ".ansi")).write_text(
            self.tm("capture-pane", "-p", "-e", "-t", target))
        return rows

    def keys(self, *argv, target="view"):
        self.tm("send-keys", "-t", target, *argv)

    def tui_args(self, target="view"):
        # The pane runs `exec wsx`, so the pane process is the TUI.
        pid = self.tm("display-message", "-p", "-t", target, "#{pane_pid}").strip()
        return pid, subprocess.run(["ps", "-o", "args=", "-p", pid], capture_output=True, text=True).stdout

    def stop_tmux(self):
        if self.tmux_socket.exists():
            subprocess.run([self.tmux, "-S", str(self.tmux_socket), "kill-server"],
                           env=self.env, capture_output=True)


def wait(test, label, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if test():
            return
        time.sleep(0.05)
    raise RuntimeError(label + " timeout")


def relink(link, target):
    # Replace an installed name in one step, as a package manager does.
    staged = link.with_name(link.name + ".new")
    staged.symlink_to(target)
    os.replace(staged, link)


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def legacy_window(fx):
    """Renew the presence of a wsx window from before the presence contract: it defers a handoff."""
    stop = threading.Event()

    def renew():
        while not stop.is_set():
            try:
                fx.call("poll", {"after_revision": 2**62, "timeout_ms": 0})
            except (OSError, ValueError, AssertionError):
                pass
            stop.wait(0.5)
    thread = threading.Thread(target=renew, daemon=True)
    thread.start()

    def quit_window():
        stop.set()
        thread.join()
    return quit_window


def run_released_upgrade(tmux, released_wsx, released_daemon, wsx, wsxd, work, unfit_daemon=None):
    """docs/daemon-live-upgrade.md: the previous release keeps its sessions and its open TUI."""
    fx = Fixture(work, tmux, released_daemon)
    daemon = None
    try:
        daemon = subprocess.Popen([str(released_daemon)], env=fx.env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait(fx.daemon_socket.exists, "released daemon socket")
        assert fx.call("synchronize_projects", {"projects": [{"name": "demo", "path": str(fx.project),
                       "worktrees": [{"path": str(fx.project), "branch": "main"}]}]})["type"] == "ack"
        worktree_id = fx.call("snapshot")["data"]["worktrees"][0]["id"]
        counter_pid, sleep_pid, count = work / "counter.pid", work / "sleep.pid", work / "count"
        commands = {
            "counter": "echo $$ > {pid}; i=0; while :; do i=$((i+1)); echo count=$i; echo $i > {count}; sleep 0.2; done"
                       .format(pid=shlex.quote(str(counter_pid)), count=shlex.quote(str(count))),
            "sleeper": "echo $$ > {pid}; exec sleep 100000".format(pid=shlex.quote(str(sleep_pid))),
        }
        for label, command in commands.items():
            created = fx.call("session_create", {"worktree_id": worktree_id, "label": label,
                              "command": ["/bin/sh", "-c", command], "rows": 21, "cols": 118})
            assert created["type"] == "created", created
        wait(lambda: counter_pid.exists() and sleep_pid.exists() and count.exists(), "released sessions")
        pids = [int(counter_pid.read_text()), int(sleep_pid.read_text())]
        fx.tm("new-session", "-d", "-s", "view", "-x", "120", "-y", "24", "exec " + shlex.quote(str(released_wsx)))
        fx.tm("set-option", "-g", "status", "off")
        wait(lambda: "counter" in "\n".join(fx.screen()) and "WORKSPACE" in "\n".join(fx.screen()),
             "released Workspace")
        fx.keys("j")
        fx.keys("j", "Enter")
        wait(lambda: "TERMINAL" in "\n".join(fx.screen()) and "count=" in "\n".join(fx.screen()),
             "released Terminal")
        before = fx.call("lifecycle_status")["data"]

        # This wsx cannot restart the released daemon into the released build: a release before
        # revision 25 refuses it, a later one rolls the handoff back. Either way it says so and
        # stops nothing.
        refused = fx.cli(wsx, "daemon", "restart", daemon_bin=released_daemon)
        assert refused.returncode != 0, refused
        assert ("cannot restart into the same build" in refused.stderr and "nothing was stopped" in refused.stderr
                or "could not hand off" in refused.stderr and "no session was stopped" in refused.stderr), refused.stderr
        guarded = fx.cli(wsx, "daemon", "stop", daemon_bin=released_daemon)
        assert guarded.returncode != 0 and "2 running session(s)" in guarded.stderr, guarded
        assert "wsx daemon restart" in guarded.stderr, guarded.stderr
        assert fx.epoch() == before["epoch"] and all(alive(pid) for pid in pids)

        if not unfit_daemon:
            # ^ A wsxd that cannot take the handoff: it exits at once.
            unfit_daemon = work / "unfit" / "wsxd"
            unfit_daemon.parent.mkdir()
            unfit_daemon.write_text("#!/bin/sh\nexit 3\n")
            unfit_daemon.chmod(0o755)
        # A starting TUI asks for a handoff into a wsxd that refuses the transfer. The running
        # daemon rolls back; the TUI must say so and open on the running daemon
        # (docs/daemon-live-upgrade.md "A failed automatic upgrade").
        fx.tm("new-session", "-d", "-s", "unfit", "-x", "100", "-y", "24",
              "exec env WSX_DAEMON_BIN=" + shlex.quote(str(unfit_daemon)) + " " + shlex.quote(str(wsx)))
        wait(lambda: "wsxd upgrade failed" in "\n".join(fx.screen("unfit")), "rolled-back upgrade notice", timeout=45)
        wait(lambda: "WORKSPACE" in "\n".join(fx.screen("unfit")) and "counter" in "\n".join(fx.screen("unfit")),
             "Workspace on the running daemon after a rollback")
        fx.capture("rolled-back-upgrade", "unfit")
        assert fx.epoch() == before["epoch"] and all(alive(pid) for pid in pids), "a rollback lost the daemon"
        fx.keys("q", target="unfit")
        # The same protocol: a CLI command of the newer wsx works with the running daemon and
        # says why the upgrade failed. It does not try that upgrade again for this daemon.
        started = time.monotonic()
        listed = fx.cli(wsx, "session", "list", daemon_bin=unfit_daemon)
        assert listed.returncode == 0 and listed.stdout.count("demo/main") == 2, listed
        assert "wsxd upgrade failed" in listed.stderr and "sessions keep running" in listed.stderr, listed.stderr
        assert time.monotonic() - started < 10, "the failed upgrade was tried again"
        assert fx.epoch() == before["epoch"], "a failed upgrade replaced the daemon"

        # The new build runs once. The daemon is replaced live under the open released TUI.
        upgraded = fx.cli(wsx, "session", "list", daemon_bin=wsxd)
        assert upgraded.returncode == 0, upgraded
        assert "Runtime upgraded" in upgraded.stderr, upgraded.stderr
        after = fx.call("lifecycle_status")["data"]
        assert after["epoch"] != before["epoch"], (before, after)
        assert after["daemon_revision"] > before["daemon_revision"], (before, after)
        assert all(alive(pid) for pid in pids), "a session process ended in the upgrade"
        assert pids == [int(counter_pid.read_text()), int(sleep_pid.read_text())]
        resumed_at = int(count.read_text())
        wait(lambda: int(count.read_text() or 0) > resumed_at + 3, "counter after the upgrade")

        def shown():
            values = re.findall(r"count=(\d+)", "\n".join(fx.screen()))
            return max(int(value) for value in values) if values else 0
        wait(lambda: shown() > resumed_at + 3, "released TUI shows the counter after the upgrade")
        rows = fx.capture("released-after-upgrade")
        assert rows[-1].startswith(" TERMINAL"), rows[-1]
        assert not any(word in row.lower() for row in rows
                       for word in ("unavailable", "disconnected", "failed")), rows
        # A released client decodes the new daemon's answers, which carry a capability it does not know.
        listed = fx.cli(released_wsx, "session", "list", "--json", daemon_bin=released_daemon)
        assert listed.returncode == 0 and "counter" in listed.stdout, listed
        assert fx.call("snapshot")["data"]["capabilities"]["daemon_restart"] is True
        assert fx.epoch() == after["epoch"], "a released client replaced the newer daemon"

        fx.stop_tmux()
        forced = fx.cli(wsx, "daemon", "stop", "--force", daemon_bin=wsxd)
        assert forced.returncode == 0 and "stopped" in forced.stdout, forced
        wait(lambda: not fx.daemon_socket.exists() and not any(alive(pid) for pid in pids),
             "forced stop ends the sessions")
        return {"result": "PASS", "source_revision": before["daemon_revision"],
                "target_revision": after["daemon_revision"], "session_pids_kept": pids}
    except Exception:
        try:
            print("last released screen:\n" + "\n".join(fx.screen()), file=sys.stderr)
        except Exception:
            pass
        raise
    finally:
        fx.stop_tmux()
        if fx.daemon_socket.exists():
            try:
                fx.call("shutdown")
            except Exception:
                pass
        if daemon is not None:
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=3)


def starting_tui_waits_for_the_upgrade(fx, next_wsx, replaced_daemon):
    """docs/daemon-live-upgrade.md: upgrade screen, its two keys, and the unattended continuation."""
    launch = "exec " + shlex.quote(str(next_wsx))

    def text():
        return "\n".join(fx.screen("up"))

    def start(width, height):
        fx.tm("new-session", "-d", "-s", "up", "-x", str(width), "-y", str(height), launch)
        wait(lambda: "Waiting" in text() and "Quit the other wsx window" in text(), "upgrade screen")
        rows = fx.capture("upgrade-waiting-{}x{}".format(width, height), "up")
        assert all(len(row) <= width for row in rows), rows
        shown = text()
        # The workspace must not open on the old daemon.
        assert "WORKSPACE" not in shown and "build" not in shown, shown
        assert "wsx upgrade" in shown and "→" in shown, shown
        assert "12 running terminals keep running" in shown, shown
        assert "continues by itself" in shown, shown
        assert "Enter" in shown and "open on wsxd" in shown and "quit" in shown, shown
        return rows

    def gone():
        return subprocess.run([fx.tmux, "-S", str(fx.tmux_socket), "has-session", "-t", "up"],
                              env=fx.env, capture_output=True).returncode != 0

    quit_legacy_window = legacy_window(fx)
    try:
        # Quit key: no dead end, and the daemon is untouched.
        tiny = start(40, 12)
        fx.keys("q", target="up")
        wait(gone, "quit from the upgrade screen")
        assert fx.epoch() == replaced_daemon.epoch, "quitting the upgrade screen changed wsxd"

        # Open on the old version anyway: the normal TUI on the running daemon, with the notice.
        normal = start(80, 24)
        fx.keys("Enter", target="up")
        wait(lambda: "WORKSPACE" in text() and "build" in text(), "Workspace on the old daemon")
        wait(lambda: "is waiting" in text(), "waiting notice in the opened TUI")
        assert fx.epoch() == replaced_daemon.epoch, "opening on the old daemon changed wsxd"
        fx.keys("q", target="up")
        wait(gone, "quit the TUI on the old daemon")

        # The blocker goes away. The user presses nothing.
        start(80, 24)
    finally:
        quit_legacy_window()
    wait(lambda: fx.epoch() not in (None, replaced_daemon.epoch), "unattended upgrade", timeout=30)
    replaced_daemon.epoch = fx.epoch()
    wait(lambda: "WORKSPACE" in text() and "build" in text(), "Workspace on the new daemon")
    wait(lambda: "Runtime upgraded" in text(), "upgrade notice in the opened TUI")
    fx.capture("upgrade-opened", "up")
    fx.keys("q", target="up")
    wait(gone, "quit the upgraded TUI")
    return {"80x24": normal, "40x12": tiny}


def run_scenario(args, tmux, wsx, wsxd, work):
    # ^ docs/daemon-live-upgrade.md: wsx and wsxd are installed names that a later step relinks.
    installed = work / "bin"
    installed.mkdir(mode=0o700)
    installed_wsx, installed_daemon = installed / "wsx", installed / "wsxd"
    installed_wsx.symlink_to(wsx)
    installed_daemon.symlink_to(wsxd)
    fx = Fixture(work, tmux, installed_daemon)
    project, config, env = fx.project, fx.config, fx.env
    config_text = CONFIG_TEXT.replace("{project}", json.dumps(str(project)))
    fix_project = work / 'fix'
    # A real Git-owned second worktree needs no fixture commit or identity override.
    subprocess.run(['git','worktree','add','--orphan','-b','fix',str(fix_project)],
                   cwd=project,env=env,check=True,capture_output=True)
    daemon_socket, tmux_socket = fx.daemon_socket, fx.tmux_socket
    helper = work / "agent.py"
    helper.write_text(
        "import os,pathlib,socket,subprocess,sys\n"
        "def report(state):\n"
        " subprocess.run([os.environ['WSX_AGENT_REPORT_BIN'],'agent','report',"
        "os.environ['WSX_PANE_ID'],'--provider','codex','--state',state,'--lifecycle'],"
        "check=True,stdout=subprocess.DEVNULL)\n"
        "report(sys.argv[1])\n"
        "if len(sys.argv)>3:\n"
        " listener=socket.socket(); listener.bind(('127.0.0.1',0)); listener.listen()\n"
        " pathlib.Path(sys.argv[3]).write_text(str(listener.getsockname()[1]))\n"
        "print('CURRENT_SESSION:'+sys.argv[2],flush=True)\n"
        "for line in sys.stdin:\n"
        " if line.strip(): report(line.strip())\n"
    )

    call, tm, screen, capture, keys = fx.call, fx.tm, fx.screen, fx.capture, fx.keys

    def primary_name(target="view", row=-2, left=2):
        # Independent switching oracle: the active PTY's actor marker, not title placement.
        match = re.search(r'CURRENT_SESSION:([a-z0-9_-]+)', '\n'.join(screen(target)))
        return match.group(1) if match else ""

    def pane_dimensions(pane_id):
        reply = call("view", {"pane_ids": [pane_id]})
        assert reply["type"] == "view", reply
        frame = reply["data"]["frames"][0]
        return frame["cols"], frame["rows"]

    daemon = None
    succeeded = False
    started = time.monotonic()
    try:
        daemon = subprocess.Popen([str(installed_daemon)], env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait(lambda: daemon_socket.exists(), "daemon socket")
        assert call("synchronize_projects", {"projects": [{"name": "demo", "path": str(project),
                    "worktrees": [{"path": str(project), "branch": "main"},
                                  {"path": str(fix_project), "branch": "fix"}]}]})["type"] == "ack"
        worktrees = call('snapshot')['data']['worktrees']
        worktree_id = next(w['id'] for w in worktrees if w['path'] == str(project))
        fix_id = next(w['id'] for w in worktrees if w['path'] == str(fix_project))
        sessions = {}
        for name, status, port in [("build", "working", True), ("api", "idle", True),
                                   ("finished", "done", False), ("approval", "blocked", False)]:
            command = [sys.executable, str(helper), status, name]
            if port:
                command.append(str(work / (name + ".port")))
            created = call("session_create", {"worktree_id": worktree_id, "label": name,
                           "command": command, "rows": 21, "cols": 118})
            assert created["type"] == "created", created
            sessions[name] = created["data"]["id"]
        for index in range(8):
            name = 'fix-' + str(index)
            created = call('session_create',{'worktree_id':fix_id,'label':name,
                'command':[sys.executable,str(helper),'idle',name],'rows':21,'cols':118})
            assert created['type'] == 'created', created
            sessions[name] = created['data']['id']
        wait(lambda: all(pane["agent"] is not None for pane in call("snapshot")["data"]["panes"]),
             "generation-bound fixture reports")
        wait(lambda: len(call("snapshot")["data"].get("listening_ports", [])) >= 2, "port attribution")
        tm("new-session", "-d", "-s", "view", "-x", "120", "-y", "24", "exec " + shlex.quote(str(installed_wsx)))
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
        assert title.startswith(' demo ') and 'main ›' in title and 'build (codex)' in title, title
        assert ' | ' not in title and ' > ' not in title, title
        assert title.count('main ›') == 1, title
        assert 'h/l' in rows[-1], rows[-1]
        assert '[' not in title and ']' not in title, title
        assert title.index('build') < title.index('api') < title.index('finished') < title.index('approval'), title
        assert not any(":" + (work / (name + ".port")).read_text() in title for name in ("build", "api"))
        title_ansi = (work / "captures/bottom-compact.ansi").read_text().splitlines()[-2]
        assert "48;2;36;43;55" in title_ansi, "current chip background not captured"
        assert "48;2;14;16;20" in title_ansi, "peer chip background not captured"
        for key, name in [("l", "api"), ("h", "build"), ("Right", "api"), ("Left", "build"),
                          ("j", "api"), ("k", "build"), ("Down", "api"), ("Up", "build")]:
            keys("C-a", key)
            wait(lambda: primary_name() == name, "project prefix " + key)
        # ^ docs/daemon-live-upgrade.md: wsxd is replaced while this TUI stays in Terminal mode.
        # The same PTY must remain on screen and accept input after every step.
        before_upgrade = capture("before-live-upgrade")
        assert not any("z" in row for row in before_upgrade), before_upgrade
        tui_pid = fx.tui_args()[0]

        def typed_input_reaches_the_shell(label):
            # The first accepted key proves the stream reattached; earlier keys are dropped, not rerouted.
            started_typing, typed = time.monotonic(), [0]

            def echoed():
                keys("-l", "z")
                typed[0] += 1
                return any("z" in row for row in screen())
            wait(echoed, "terminal input after " + label)
            seconds = round(time.monotonic() - started_typing, 2)
            keys(*["BSpace"] * typed[0])
            wait(lambda: not any("z" in row for row in screen()), "input erased after " + label)
            rows = capture("after-" + label.replace(" ", "-"))
            # The rail and title pulse; the PTY row and the mode line must be unchanged.
            assert rows[1][2:] == before_upgrade[1][2:], ("the terminal changed across " + label, rows[1])
            assert rows[-1].startswith(" TERMINAL"), rows[-1]
            assert not any(word in row.lower() for row in rows for word in ("unavailable", "disconnected")), rows
            return seconds

        def replaced_daemon(result, label):
            assert result.returncode == 0, (label, result)
            assert fx.epoch() not in (None, replaced_daemon.epoch), label + " did not replace wsxd"
            replaced_daemon.epoch = fx.epoch()
        replaced_daemon.epoch = fx.epoch()

        # A stop would end the 12 running sessions, so it needs --force.
        guarded = fx.cli(installed_wsx, "daemon", "stop")
        assert guarded.returncode != 0 and "12 running session(s)" in guarded.stderr, guarded
        assert "wsx daemon restart" in guarded.stderr and "--force" in guarded.stderr, guarded.stderr
        assert fx.epoch() == replaced_daemon.epoch, "a refused stop changed the daemon"

        # A restart into the same build keeps the sessions and the attached TUI process.
        restarted = fx.cli(installed_wsx, "daemon", "restart")
        replaced_daemon(restarted, "same-build restart")
        assert "12 running terminal(s) kept" in restarted.stdout, restarted.stdout
        reattach_seconds = typed_input_reaches_the_shell("same-build restart")
        assert "--followed-from" not in fx.tui_args()[1], "a same-build restart must not re-execute the TUI"

        # A new install replaces both names. Its wsx cannot start yet: the TUI must stay usable.
        next_wsx, next_daemon = work / "next" / "wsx", work / "next" / "wsxd"
        next_wsx.parent.mkdir(mode=0o700)
        shutil.copy(args.next_wsx or wsx, next_wsx)
        shutil.copy(args.next_daemon or wsxd, next_daemon)
        unusable = next_wsx.with_name("wsx-unusable")
        shutil.copy(next_wsx, unusable)
        unusable.chmod(0o600)
        relink(installed_daemon, next_daemon)
        relink(installed_wsx, unusable)
        if args.next_wsx:
            # A starting TUI of the higher revision must upgrade wsxd before it opens.
            upgrade_screens = starting_tui_waits_for_the_upgrade(fx, next_wsx, replaced_daemon)
        else:
            # A copy of this build is a different install of the same revision: no upgrade is
            # pending, so a restart replaces the daemon.
            upgrade_screens = None
            replaced_daemon(fx.cli(next_wsx, "daemon", "restart"), "upgrade into the new install")
        wait(lambda: any("Could not switch to the installed wsx" in row for row in screen()),
             "notice of the failed re-exec", timeout=20)
        capture("failed-re-exec")
        typed_input_reaches_the_shell("failed re-exec")
        assert "--followed-from" not in fx.tui_args()[1], "the TUI left its old code after a failed exec"

        # The install is repaired. The next daemon change makes the TUI re-execute into it.
        relink(installed_wsx, next_wsx)
        replaced_daemon(fx.cli(installed_wsx, "daemon", "restart"), "restart with the repaired install")
        wait(lambda: "--followed-from" in fx.tui_args()[1], "TUI re-executed into the installed wsx", timeout=20)
        followed_pid, followed_args = fx.tui_args()
        assert followed_pid == tui_pid, "the TUI must replace its own process, not start another"
        assert "--follow-pane " + str(build_pane) in followed_args, followed_args
        wait(lambda: "TERMINAL" in "\n".join(screen()) and primary_name() == "build",
             "same session and mode after the re-exec")
        follow_seconds = typed_input_reaches_the_shell("re-exec")
        rows = capture("re-exec-notice")
        assert any("became wsx" in row for row in rows) and any("Sessions kept running" in row for row in rows), rows
        # The notice covers the end of the title row until it expires.
        wait(lambda: not any("Sessions kept running" in row for row in screen()), "re-exec notice expiry", timeout=30)
        # A second install arrives while a search field has unsent input. The TUI must wait,
        # then re-execute from Workspace mode and keep the cursor on the same session.
        keys("C-a", "w")
        wait(lambda: "WORKSPACE" in "\n".join(screen()), "Workspace before the second install")
        keys("/", "bui")
        wait(lambda: "/bui_" in screen()[-1], "open search field")
        later_wsx, later_daemon = work / "later" / "wsx", work / "later" / "wsxd"
        later_wsx.parent.mkdir(mode=0o700)
        shutil.copy(next_wsx, later_wsx)
        shutil.copy(next_daemon, later_daemon)
        relink(installed_daemon, later_daemon)
        relink(installed_wsx, later_wsx)
        replaced_daemon(fx.cli(installed_wsx, "daemon", "restart"), "restart into the second install")
        time.sleep(3)
        assert "--follow-pane" in fx.tui_args()[1], "the TUI re-executed while a search field was open"
        assert "/bui_" in screen()[-1], screen()[-1]
        keys("Escape")
        wait(lambda: "--followed-from" in fx.tui_args()[1] and "--follow-pane" not in fx.tui_args()[1],
             "re-exec after the search field closed", timeout=20)
        assert fx.tui_args()[0] == tui_pid
        wait(lambda: "WORKSPACE" in "\n".join(screen()) and "build" in "\n".join(screen()),
             "Workspace after the second re-exec")
        wait(lambda: not any("Sessions kept running" in row for row in screen()), "second notice expiry", timeout=30)
        keys("Enter")
        wait(lambda: "TERMINAL" in "\n".join(screen()) and primary_name() == "build",
             "the cursor stayed on the same session across the re-exec")
        # Existing n keeps its broader attention behavior; the new cycle is separate.
        keys("C-a", "n")
        wait(lambda: primary_name() == "approval", "existing attention command")
        for name in ['finished','api','build']:
            keys('C-a','k')
            wait(lambda: primary_name() == name, 'stable reverse cycle')
        # Visit every hidden target across both worktrees, then wrap to build.
        for name in ['api','finished','approval'] + ['fix-'+str(i) for i in range(8)] + ['build']:
            keys('C-a','j')
            wait(lambda: primary_name() == name, 'project traversal '+name)
            if name == 'fix-3':
                overflow = capture('cross-worktree-overflow')[-2]
                assert 'fix ›' in overflow and 'fix-3 (codex)' in overflow and re.search(r'\+\d', overflow), overflow
                assert overflow.index('fix ›') < overflow.index('fix-3 (codex)'), overflow
                long_name = '审计👩‍💻-' * 4 + 'e\u0301'
                renamed = subprocess.run([str(wsx),'session','rename',str(sessions[name]),long_name],
                    cwd=fix_project,env=env,capture_output=True,text=True,timeout=60)
                assert renamed.returncode == 0, renamed.stderr[-4096:]
                wait(lambda: '审计' in screen()[-2] and '(codex)' in screen()[-2], 'Unicode title update')
                capture('long-unicode-current')
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
        assert 'main ›' in rows[-2] and 'build (codex)' in rows[-2], rows[-2]
        # A live generation-authorized report changes state without reshuffling the cycle.
        approval = next(s["focused_pane"] for s in snapshot["sessions"] if s["id"] == sessions["approval"])
        assert call("terminal_acquire", {"pane_id": approval, "client_id": 991, "takeover": False})["type"] == "ack"
        assert call("terminal_input", {"pane_id": approval, "client_id": 991, "bytes": list(b"idle\r")})["type"] == "ack"
        assert call("terminal_release", {"pane_id": approval, "client_id": 991})["type"] == "ack"
        wait(lambda: next(pane for pane in call("snapshot")["data"]["panes"] if pane["id"] == approval)["agent"]["state"] == "idle",
             "accepted generation-bound idle report")
        def peers_reflect_idle():
            title = screen()[-2]
            return 'main ›' in title and '○ approval (codex)' in title and '◐' not in title
        assert primary_name() == 'build'
        for name in ['api','finished','approval']:
            keys('C-a','j')
            wait(lambda: primary_name() == name, 'unchanged order after state update')
        wait(peers_reflect_idle, "updated current state")
        capture("mobile-updated")
        for name in ['finished','api','build']:
            keys('C-a','k')
            wait(lambda: primary_name() == name, 'mobile reverse cycle')
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
        keys("C-a", "j", target="top")
        wait(lambda: primary_name("top", row=1) == "api", "top project cycle")
        api_pane = next(s["focused_pane"] for s in snapshot["sessions"] if s["id"] == sessions["api"])
        keys("C-a", target="top")
        wait(lambda: pane_dimensions(api_pane) == (88, 21), "prefix sidebar peek")
        keys("k", target="top")
        wait(lambda: primary_name("top", row=1) == "build" and pane_dimensions(build_pane) == (118, 21),
             "project cycle restores baseline after peek")
        capture("top-prefix-peek-restored", "top")
        # Search hidden logical entries without unfolding the project. One fix session was renamed.
        keys('C-a', 'w', target='top')
        wait(lambda: 'WORKSPACE' in '\n'.join(screen('top')), 'Workspace for search')
        keys('k', 'k', 'h', target='top')
        capture('search-folded-before', 'top')
        keys('/', 'fix', target='top')
        wait(lambda: '/fix_ (8)' in screen('top')[-1], 'folded logical search count')
        capture('search-folded-ancestor', 'top')
        keys('BSpace', 'BSpace', 'BSpace', target='top')
        wait(lambda: '/_ (0)' in screen('top')[-1], 'cleared search count')
        capture('search-cleared', 'top')
        keys('Escape', target='top')
        wait(lambda: 'WORKSPACE' in screen('top')[-1], 'search exit')
        capture('search-folded-after', 'top')
        keys('/', '__absent__', target='top')
        wait(lambda: '/__absent___ (0)' in screen('top')[-1], 'absent search result')
        capture('search-empty', 'top')
        keys('Escape', target='top')
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
                # The live-upgrade successor is not the process started above.
                deadline = time.monotonic() + 5
                while daemon_socket.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
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
                      "restart_reattach_seconds": reattach_seconds,
                      "re_exec_ready_seconds": follow_seconds,
                      "next_build": "given" if args.next_wsx else "copy of this build",
                      "upgrade_screens": upgrade_screens,
                      "cleanup": "private tmux, wsxd, PTYs and listeners stopped",
                      "journey": "preview ports, h/l and j/k cycle, bare keys, guarded stop, same-build restart, failed and successful TUI re-exec under an attached terminal, unchanged n, sidebar toggle and peek, mobile, live peer update, hierarchy, folded search counts and clearing, top title"}


if __name__ == "__main__":
    main()
