#!/usr/bin/env python3
import atexit
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time

import wsxd_client
from tui_pty import TuiPty
from wsxd_client import PROTOCOL, heartbeat_streams, recv_line, send_stream, stream_send_lock

ROOT = Path(__file__).resolve().parents[1]
# ^ Plugin manifest commands must be absolute even when the scratch root is relative.
WORK = Path(os.environ.get("WSX_SMOKE_WORK", ROOT / ".work" / "runtime-smoke")).resolve()
if WORK.exists():
    shutil.rmtree(WORK)
HOME = WORK / "home"
STATE = WORK / "state"
PROJECT = WORK / "project"
for path in (HOME, STATE, PROJECT):
    path.mkdir(parents=True, exist_ok=True)
# ^ The stream assertions require one output line per input line, including OSC bytes.
TERMINAL_HELPER = WORK / "terminal-helper.sh"
TERMINAL_HELPER.write_text(
    "#!/bin/sh\n"
    "printf 'wsx-smoke:%s\\n' \"$$\"\n"
    "printf 'claude-renderer:%s\\n' \"${CLAUDE_CODE_NO_FLICKER-unset}\"\n"
    "while IFS= read -r line; do\n"
    "  case $line in\n"
    "    __WSX_REPORT__:*)\n"
    "      conversation=${line#__WSX_REPORT__:}\n"
    "      \"$WSX_AGENT_REPORT_BIN\" agent report \"$WSX_PANE_ID\" --provider codex "
    "--state working --conversation-id \"$conversation\" --prompt --resume --lifecycle\n"
    "      ;;\n"
    "    __WSX_DETACH__:*)\n"
    "      conversation=${line#__WSX_DETACH__:}\n"
    "      \"$WSX_AGENT_REPORT_BIN\" agent report \"$WSX_PANE_ID\" --provider codex "
    "--state unknown --conversation-id \"$conversation\" --detached\n"
    "      ;;\n"
    "    __WSX_PID__:*)\n"
    "      marker=${line#__WSX_PID__:}\n"
    "      printf 'wsx-helper:%s:%s\\n' \"$$\" \"$marker\"\n"
    "      ;;\n"
    "    *) printf '%s\\n' \"$line\" ;;\n"
    "  esac\n"
    "done\n"
)
TERMINAL_HELPER.chmod(0o700)
CONFIG_DIR = (
    HOME / "Library" / "Application Support" / "wsx"
    if sys.platform == "darwin"
    else WORK / "config" / "wsx"
)
CONFIG_DIR.mkdir(parents=True, mode=0o700)
(CONFIG_DIR / "config-v2.toml").write_text(
    "resume_agents_on_restore = false\n"
    "[[projects]]\n"
    "name = \"smoke\"\n"
    f"path = {json.dumps(str(PROJECT))}\n"
)
PLUGIN_DIR = WORK / "config" / "wsx" / "plugins"
PLUGIN_DIR.mkdir(parents=True, mode=0o700)
PLUGIN_MARKER = WORK / "plugin-events.jsonl"
PLUGIN = PLUGIN_DIR / "recorder.py"
PLUGIN.write_text(
    "#!/usr/bin/env python3\n"
    "import os, pathlib\n"
    f"pathlib.Path({str(PLUGIN_MARKER)!r}).open('a').write(os.environ['WSX_EVENT_JSON'] + '\\n')\n"
)
PLUGIN.chmod(0o700)
(PLUGIN_DIR / "recorder.json").write_text(json.dumps({
    "api_version": 1,
    "id": "smoke-recorder",
    "name": "Smoke recorder",
    "command": [str(PLUGIN)],
    "events": ["session.created"],
    "enabled": True,
}))
(PLUGIN_DIR / "recorder.json").chmod(0o600)
env = os.environ.copy()
# ^ wsxd chooses Claude's renderer for its panes only when the owner has not.
env.pop("CLAUDE_CODE_NO_FLICKER", None)
# ^ A wsx pane exports its daemon's socket; the smoke daemon is found from XDG_STATE_HOME.
env.pop("WSX_SOCKET", None)
env.update({
    "HOME": str(HOME),
    # ^ docs/daemon-discovery.md "Lifetime": its daemons exit with this script.
    "WSX_EXIT_WITH_PID": str(os.getpid()),
    "XDG_STATE_HOME": str(STATE),
    "XDG_CONFIG_HOME": str(WORK / "config"),
    "XDG_CACHE_HOME": str(WORK / "cache"),
    "SHELL": "/bin/sh",
    # A fresh pane must not inherit a Claude nesting marker from wsxd's parent.
    "CLAUDE_CODE_CHILD_SESSION": "parent-only-marker",
    # ^ A fixture git command must never reach the repository this script runs in.
    "GIT_CEILING_DIRECTORIES": str(WORK),
})
subprocess.run(["git", "init", "-q", "-b", "main", str(PROJECT)], check=True, env=env)


def git(*args):
    return subprocess.run(
        ["git", "-C", str(PROJECT), "-c", "user.name=smoke", "-c", "user.email=smoke@example.invalid", *args],
        check=True, env=env, capture_output=True, text=True,
    ).stdout.strip()


assert os.path.realpath(git("rev-parse", "--show-toplevel")) == os.path.realpath(PROJECT), "fixture git escaped"
# ^ `git worktree add` needs a commit; the second commit separates `--from` from HEAD.
git("commit", "-q", "--allow-empty", "-m", "base")
BASE_COMMIT = git("rev-parse", "HEAD")
git("commit", "-q", "--allow-empty", "-m", "head")
SOCKET = STATE / "wsx" / "wsx.sock"
DAEMON = Path(os.environ.get("WSX_SMOKE_DAEMON", ROOT / "target" / "debug" / "wsxd"))
WSX = Path(os.environ.get("WSX_SMOKE_WSX", ROOT / "target" / "debug" / "wsx"))


def raw_call(payload):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(str(SOCKET))
        client.sendall(payload)
        return recv_line(client)


def call(method, params=None):
    return wsxd_client.call(SOCKET, method, params)


def wsx(*args, extra_env=None, pane_id=None, timeout=None):
    # ^ The caller's own WSX_PANE_ID must never leak into an isolated CLI call.
    cli_env = {key: value for key, value in env.items() if key != "WSX_PANE_ID"}
    if pane_id is not None:
        cli_env["WSX_PANE_ID"] = str(pane_id)
    return subprocess.run(
        [str(WSX), *args], cwd=PROJECT, env=cli_env | (extra_env or {}),
        check=False, capture_output=True, text=True, timeout=timeout,
    )


def wait_for(test, label, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if test():
            return
        time.sleep(0.1)
    raise AssertionError(label() if callable(label) else label)


def send_shell_command(pane_id, command, client_id):
    assert call("terminal_acquire", {
        "pane_id": pane_id,
        "client_id": client_id,
        "takeover": False,
    })["type"] == "ack"
    response = call("terminal_input", {
        "pane_id": pane_id,
        "client_id": client_id,
        "bytes": list(command.encode() + b"\r"),
    })
    assert response["type"] == "ack", response
    assert call("terminal_release", {
        "pane_id": pane_id,
        "client_id": client_id,
    })["type"] == "ack"


def wait_ready(process):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"wsxd exited early: {process.returncode}")
        if SOCKET.exists():
            try:
                response = call("snapshot")
                if response.get("type") == "snapshot":
                    return
            except OSError:
                pass
        time.sleep(0.05)
    raise RuntimeError("wsxd readiness timeout")


def wait_singleton_released():
    lock = STATE / "wsx" / "state.lock"
    lock.parent.mkdir(parents=True, exist_ok=True)
    os.chmod(lock.parent, 0o700)
    descriptor = os.open(lock, os.O_CREAT | os.O_RDWR, 0o600)
    os.chmod(lock, 0o600)
    deadline = time.monotonic() + 5
    try:
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(descriptor, fcntl.LOCK_UN)
                return
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise RuntimeError("wsxd singleton lock did not become available")
                time.sleep(0.05)
    finally:
        os.close(descriptor)


def start(descriptor_limit=None):
    wait_singleton_released()
    limit = None
    if descriptor_limit is not None:
        hard = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
        limit = lambda: resource.setrlimit(resource.RLIMIT_NOFILE, (descriptor_limit, hard))
    process = subprocess.Popen([str(DAEMON)], env=env, preexec_fn=limit)
    wait_ready(process)
    return process


def wait_socket_stopped():
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and SOCKET.exists():
        time.sleep(0.05)
    assert not SOCKET.exists(), "wsxd did not remove its smoke socket"


def cleanup_smoke_daemon():
    if not SOCKET.exists():
        return
    try:
        response = call("shutdown")
        if response.get("type") == "ack":
            wait_socket_stopped()
    except (OSError, RuntimeError, AssertionError):
        pass


atexit.register(cleanup_smoke_daemon)


def shutdown(process):
    response = call("shutdown")
    assert response["type"] == "ack", response
    process.wait(timeout=5)
    wait_socket_stopped()


def shutdown_from_incompatible_client(process):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(str(SOCKET))
        client.sendall(json.dumps({"method": "hello", "params": {"protocol": 999}}).encode() + b"\n")
        hello = recv_line(client)
        assert hello["type"] == "hello" and hello["data"]["protocol"] == PROTOCOL, hello
        client.sendall(json.dumps({"method": "shutdown"}).encode() + b"\n")
        response = recv_line(client)
        assert response["type"] == "ack", response
    process.wait(timeout=5)
    wait_socket_stopped()

# Concurrent agent reports. Owner report on 0.31: the TUI kept reconnecting, then every
# session ended at once while agents in several projects and worktrees reported together.
# ^ wsxd started from a macOS GUI terminal inherits launchd's soft descriptor limit, 256.
burst_projects = []
for name in ("smoke", "burst-b", "burst-c"):
    root = PROJECT if name == "smoke" else WORK / name
    if root != PROJECT:
        subprocess.run(["git", "init", "-q", "-b", "main", str(root)], check=True, env=env)
        subprocess.run(["git", "-C", str(root), "-c", "user.name=smoke", "-c",
                        "user.email=smoke@example.invalid", "commit", "-q", "--allow-empty",
                        "-m", "base"], check=True, env=env)
    worktrees = [{"path": str(root), "branch": "main"}]
    for branch in (f"burst-{name}-1", f"burst-{name}-2"):
        path = WORK / f"{name}-{branch}"
        subprocess.run(["git", "-C", str(root), "worktree", "add", "-q", "-b", branch, str(path)],
                       check=True, env=env)
        worktrees.append({"path": str(path), "branch": branch})
    burst_projects.append({"path": str(root), "name": name, "worktrees": worktrees})
burst_process = start(descriptor_limit=256)
assert call("synchronize_projects", {"projects": burst_projects})["type"] == "ack"
burst_identity = call("lifecycle_status")["data"]
burst_panes = []
burst_idle_panes = []
# ^ Each worktree has one agent pane and three idle sessions; every pane holds about five
# wsxd descriptors.
for index, worktree in enumerate(call("snapshot")["data"]["worktrees"]):
    for slot in range(4):
        created = call("session_create", {
            "worktree_id": worktree["id"], "label": f"burst-{index}-{slot}", "command": [],
            "rows": 12, "cols": 80,
        })
        assert created["type"] == "created", created
        session = next(item for item in call("snapshot")["data"]["sessions"]
                       if item["id"] == created["data"]["id"])
        pane = (session["focused_pane"], Path(worktree["path"]))
        (burst_idle_panes if slot else burst_panes).append(pane)
assert len(burst_panes) == 9 and len(call("snapshot")["data"]["projects"]) == 3, burst_panes
burst_terminals = {pane["id"]: pane["terminal_id"] for pane in call("snapshot")["data"]["panes"]}
# ^ A TUI holds one terminal stream per visible pane, reads it, types through it and renews
# its lease with a heartbeat each second. A stream that ends is a reconnect.
burst_streams = {}
burst_stream_ends = []
burst_done = threading.Event()
def drain_burst_stream(pane_id, stream):
    last = b""
    try:
        while chunk := stream.recv(65536):
            last = (last + chunk)[-300:]
        burst_stream_ends.append((pane_id, last.decode(errors="replace")))
    except OSError as error:
        burst_stream_ends.append((pane_id, repr(error)))


for client_id, (pane_id, _) in enumerate(burst_panes, start=600):
    stream = wsxd_client.connect(SOCKET, 5)
    wsxd_client.send(stream, "terminal_subscribe", {
        "pane_id": pane_id, "client_id": client_id, "takeover": False, "rows": 12, "cols": 80,
    })
    assert recv_line(stream)["type"] == "ack"
    stream.settimeout(None)
    threading.Thread(target=drain_burst_stream, args=(pane_id, stream), daemon=True).start()
    burst_streams[pane_id] = stream
threading.Thread(
    target=heartbeat_streams, args=(burst_streams.values(), burst_done), daemon=True
).start()
BURST_ROUNDS = 3
for round_index in range(BURST_ROUNDS):
    markers = []
    for index, (pane_id, path) in enumerate(burst_panes):
        provider = "claude" if index % 2 else "codex"
        marker = WORK / f"burst-{pane_id}-{round_index}.done"
        markers.append(marker)
        # ^ A per-directory profile (direnv) moves HOME and XDG_STATE_HOME inside the pane.
        profile = f"export HOME={path}/.home XDG_STATE_HOME={path}/.state; " if index % 3 == 0 else ""
        assert not burst_stream_ends, (
            f"terminal streams ended before burst {round_index}: {burst_stream_ends}, "
            f"wsxd exit {burst_process.poll()}"
        )
        send_stream(burst_streams[pane_id], {"type": "input", "data": list((
            f"cd {path} && {profile}"
            "i=0; while [ $i -lt 8 ]; do for s in working blocked idle done; do "
            f"\"$WSX_AGENT_REPORT_BIN\" agent report \"$WSX_PANE_ID\" --provider {provider} "
            f"--state $s --lifecycle >/dev/null 2>>{marker}.err & done; i=$((i+1)); done; "
            f"wait; touch {marker}\r"
        ).encode())})
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and not all(marker.exists() for marker in markers):
        assert burst_process.poll() is None, f"wsxd exited during report burst {round_index}"
        time.sleep(0.1)
    assert all(marker.exists() for marker in markers), f"report burst {round_index} did not finish"
assert burst_process.poll() is None, "wsxd exited under concurrent agent reports"
burst_after = call("lifecycle_status")["data"]
assert burst_after["phase"] == "ready", burst_after
assert (burst_after["epoch"], burst_after["started_unix_ms"]) == (
    burst_identity["epoch"], burst_identity["started_unix_ms"]
), "wsxd restarted or handed off under concurrent agent reports"
assert not burst_stream_ends, f"terminal streams ended under concurrent agent reports: {burst_stream_ends}"
burst_done.set()
with stream_send_lock:
    for stream in burst_streams.values():
        stream.shutdown(socket.SHUT_RDWR)
        stream.close()
burst_snapshot = call("snapshot")["data"]
for pane_id, _ in burst_panes + burst_idle_panes:
    pane = next(item for item in burst_snapshot["panes"] if item["id"] == pane_id)
    assert pane["exited"] is False and pane["terminal_id"] == burst_terminals[pane_id], pane
shutdown(burst_process)
# ^ The steps below start from one project with one worktree.
for branch in ("burst-smoke-1", "burst-smoke-2"):
    git("worktree", "remove", "--force", str(WORK / f"smoke-{branch}"))
    git("branch", "-D", branch)
shutil.rmtree(STATE / "wsx")
print("concurrent agent reports: PASS", flush=True)

process = start()
assert SOCKET.stat().st_mode & 0o777 == 0o600
plugins = call("plugin_list")
assert plugins["type"] == "plugins" and plugins["data"][0]["id"] == "smoke-recorder", plugins
assert call("plugin_reload")["type"] == "plugins"
second = subprocess.Popen([str(DAEMON)], env=env)
assert second.wait(timeout=5) != 0
malformed = raw_call(b"{}\n")
assert malformed["type"] == "error" and malformed["data"]["code"] == "invalid_json", malformed
unhandshaken = raw_call(json.dumps({"method": "snapshot"}).encode() + b"\n")
assert unhandshaken["type"] == "error" and unhandshaken["data"]["code"] == "handshake_required", unhandshaken
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as incompatible:
    incompatible.settimeout(5)
    incompatible.connect(str(SOCKET))
    incompatible.sendall(json.dumps({"method": "hello", "params": {"protocol": 999}}).encode() + b"\n")
    advertised = recv_line(incompatible)
    assert advertised["type"] == "hello" and advertised["data"]["protocol"] == PROTOCOL, advertised
    incompatible.sendall(json.dumps({"method": "snapshot"}).encode() + b"\n")
    rejected = recv_line(incompatible)
    assert rejected["type"] == "error" and rejected["data"]["code"] == "protocol_mismatch", rejected
# docs/daemon-live-upgrade.md: a request from a later release is answered, not dropped,
# and an unknown field in a known request is ignored.
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as later_release:
    later_release.settimeout(5)
    later_release.connect(str(SOCKET))
    later_release.sendall(json.dumps({"method": "hello", "params": {"protocol": 999}}).encode() + b"\n")
    assert recv_line(later_release)["type"] == "hello"
    later_release.sendall(json.dumps({"method": "request_of_a_later_release", "params": {"value": 1}}).encode() + b"\n")
    unknown = recv_line(later_release)
    assert unknown["type"] == "error" and unknown["data"]["code"] == "invalid_json", unknown
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as same_protocol:
    same_protocol.settimeout(5)
    same_protocol.connect(str(SOCKET))
    same_protocol.sendall(json.dumps({"method": "hello", "params": {"protocol": PROTOCOL, "field_of_a_later_release": True}}).encode() + b"\n")
    assert recv_line(same_protocol)["type"] == "hello"
    same_protocol.sendall(json.dumps({"method": "request_of_a_later_release"}).encode() + b"\n")
    unknown = recv_line(same_protocol)
    assert unknown["type"] == "error" and unknown["data"]["code"] == "invalid_json", unknown
    same_protocol.sendall(json.dumps({"method": "lifecycle_status"}).encode() + b"\n")
    assert recv_line(same_protocol)["type"] == "lifecycle", "the connection must survive an unknown request"
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as lifecycle_client:
    lifecycle_client.settimeout(5)
    lifecycle_client.connect(str(SOCKET))
    lifecycle_client.sendall(json.dumps({"method": "hello", "params": {"protocol": 999}}).encode() + b"\n")
    assert recv_line(lifecycle_client)["type"] == "hello"
    lifecycle_client.sendall(json.dumps({"method": "lifecycle_status"}).encode() + b"\n")
    lifecycle = recv_line(lifecycle_client)
    assert lifecycle["type"] == "lifecycle", lifecycle
    assert lifecycle["data"]["phase"] == "ready", lifecycle
    assert lifecycle["data"]["binary_id"], lifecycle
mismatch = call("hello", {"protocol": 999})
assert mismatch["type"] == "error" and mismatch["data"]["code"] == "protocol_mismatch", mismatch
response = call("synchronize_projects", {"projects": [{
    "path": str(PROJECT),
    "name": "smoke",
    "worktrees": [{"path": str(PROJECT), "branch": "main"}],
}]})
assert response["type"] == "ack", response
duplicate = call("synchronize_projects", {"projects": [
    {"path": str(PROJECT), "name": "one", "worktrees": []},
    {"path": str(PROJECT), "name": "two", "worktrees": []},
]})
assert duplicate["type"] == "error", duplicate
snapshot = call("snapshot")["data"]
project_id = snapshot["projects"][0]["id"]
worktree_id = snapshot["worktrees"][0]["id"]
cli_created = subprocess.run(
    [
        str(WSX),
        "session",
        "create",
        "--name",
        "cli-smoke",
        "--command",
        "printf cli-created",
        "--json",
    ],
    cwd=PROJECT,
    env=env,
    check=True,
    capture_output=True,
    text=True,
)
cli_session = json.loads(cli_created.stdout)
assert cli_session["label"] == "cli-smoke", cli_session
assert cli_session["project"] == "smoke", cli_session
assert cli_session["branch"] == "main", cli_session
assert cli_session["worktree"] == str(PROJECT), cli_session
cli_session_id = cli_session["session_id"]
cli_snapshot = call("snapshot")["data"]
assert any(item["id"] == cli_session_id for item in cli_snapshot["sessions"]), cli_snapshot
cli_deleted = subprocess.run(
    [str(WSX), "session", "delete", str(cli_session_id), "--json"],
    cwd=PROJECT,
    env=env,
    check=True,
    capture_output=True,
    text=True,
)
assert json.loads(cli_deleted.stdout) == {
    "deleted": True,
    "label": "cli-smoke",
    "session_id": cli_session_id,
}, cli_deleted.stdout
assert not any(
    item["id"] == cli_session_id for item in call("snapshot")["data"]["sessions"]
)
# Agent-facing worktree commands: start point, existing branch, JSON identity, git errors.
created = wsx("worktree", "create", "feat/json", "--json")
assert created.returncode == 0 and created.stderr == "", created
created_worktree = json.loads(created.stdout)
feat_path = WORK / "project-feat-json"
assert created_worktree["worktree"] == str(feat_path), created_worktree
assert created_worktree["branch"] == "feat/json", created_worktree
assert git("-C", str(feat_path), "rev-parse", "HEAD") == git("rev-parse", "HEAD")
created_session = next(
    item for item in call("snapshot")["data"]["sessions"]
    if item["id"] == created_worktree["session_id"]
)
assert created_session["focused_pane"] == created_worktree["pane_id"], created_worktree
current = wsx("session", "current", "--json", pane_id=created_worktree["pane_id"])
assert current.returncode == 0 and current.stderr == "", current
assert json.loads(current.stdout) == {
    "pane_id": created_worktree["pane_id"],
    "session_id": created_worktree["session_id"],
    "label": created_session["label"],
    "project": "smoke",
    "worktree": str(feat_path),
    "branch": "feat/json",
    "agent": None,
    "agent_status": None,
}, current.stdout
outside = wsx("session", "current", "--json")
assert outside.returncode != 0 and outside.stdout == "", outside
assert "not inside a wsx-managed pane" in outside.stderr, outside.stderr
absent = wsx("session", "current", "--json", pane_id=999999)
assert absent.returncode != 0 and "is not present" in absent.stderr, absent
from_base = wsx("worktree", "create", "feat/from", "--from", BASE_COMMIT, "--json")
assert from_base.returncode == 0, from_base
from_path = Path(json.loads(from_base.stdout)["worktree"])
assert git("-C", str(from_path), "rev-parse", "HEAD") == BASE_COMMIT
git("branch", "existing", BASE_COMMIT)
existing = wsx("worktree", "create", "existing")
assert existing.returncode == 0, existing
existing_path = WORK / "project-existing"
assert f"worktree: {existing_path}" in existing.stdout, existing.stdout
assert git("-C", str(existing_path), "rev-parse", "--abbrev-ref", "HEAD") == "existing"
assert git("-C", str(existing_path), "rev-parse", "HEAD") == BASE_COMMIT
ignored_start = wsx("worktree", "create", "existing", "--from", "main")
assert ignored_start.returncode != 0 and "already exists" in ignored_start.stderr, ignored_start
checked_out = wsx("worktree", "create", "existing")
assert checked_out.returncode != 0 and "fatal:" in checked_out.stderr, checked_out
bad_start = wsx("worktree", "create", "feat/bad", "--from", "no-such-ref")
assert bad_start.returncode != 0, bad_start
assert "fatal:" in bad_start.stderr and "no-such-ref" in bad_start.stderr, bad_start.stderr
assert not (WORK / "project-feat-bad").exists()
for branch in ("feat/json", "feat/from", "existing"):
    deleted = wsx("worktree", "delete", branch)
    assert deleted.returncode == 0, deleted
assert wsx("status", "--json").returncode == 0
cli_snapshot = call("snapshot")["data"]
assert len(cli_snapshot["worktrees"]) == 1, cli_snapshot
assert not any(
    item["id"] == created_worktree["session_id"] for item in cli_snapshot["sessions"]
)
quick = call("session_create", {
    "worktree_id": worktree_id,
    "label": "quick-exit",
    "command": ["/usr/bin/true"],
    "rows": 2,
    "cols": 8,
})
assert quick["type"] == "created", quick
quick_session_id = quick["data"]["id"]
deadline = time.monotonic() + 3
quick_pane = None
while time.monotonic() < deadline:
    quick_snapshot = call("snapshot")["data"]
    quick_session = next(item for item in quick_snapshot["sessions"] if item["id"] == quick_session_id)
    quick_pane = next(item for item in quick_snapshot["panes"] if item["id"] == quick_session["focused_pane"])
    if quick_pane["exited"]:
        break
    time.sleep(0.05)
assert quick_pane is not None and quick_pane["exited"], quick_pane
quick_session = next(item for item in call("snapshot")["data"]["sessions"] if item["id"] == quick_session_id)
assert call("session_close", {"session_id": quick_session_id, "expected_revision": quick_session["revision"]})["type"] == "ack"
activity_session = call("session_create", {
    "worktree_id": worktree_id,
    "label": "foreground-job-smoke",
    "command": [],
    # The isolated HOME has no user-installed xterm-ghostty terminfo for ncurses watch.
    "initial_input": "TERM=xterm-256color watch -n 1 date" if shutil.which("watch") else "sleep 30",
    "rows": 12,
    "cols": 80,
})
assert activity_session["type"] == "created", activity_session
activity_session_id = activity_session["data"]["id"]
activity_snapshot = None
activity_record = None
activity_pane_id = None
deadline = time.monotonic() + 6
while time.monotonic() < deadline:
    activity_snapshot = call("snapshot")["data"]
    activity_record = next(
        item for item in activity_snapshot["sessions"] if item["id"] == activity_session_id
    )
    activity_pane_id = activity_record["focused_pane"]
    if any(
        item["pane_id"] == activity_pane_id and item["foreground_job"]
        for item in activity_snapshot.get("pane_activity", [])
    ):
        break
    time.sleep(0.1)
if activity_snapshot is None or not any(
    item["pane_id"] == activity_pane_id and item["foreground_job"]
    for item in activity_snapshot.get("pane_activity", [])
):
    activity_frame = call("view", {"pane_ids": [activity_pane_id]})
    activity_text = "".join(cell[0] for cell in activity_frame["data"]["frames"][0]["cells"])
    raise AssertionError(f"foreground job was not observed: {activity_text!r}")
activity_pane = next(item for item in activity_snapshot["panes"] if item["id"] == activity_pane_id)
assert activity_pane["agent"] is None, "foreground work must not imply agent identity"
activity_project = next(item for item in activity_snapshot["projects"] if item["id"] == project_id)
assert activity_project["last_agent_active_unix_ms"] is None, activity_project
assert time.monotonic() < deadline - 5, "foreground scan must not wait for the port poll"
# Interrupt the foreground job without closing the shell.
assert call("terminal_acquire", {
    "pane_id": activity_pane_id, "client_id": 81, "takeover": False,
})["type"] == "ack"
assert call("terminal_input", {
    "pane_id": activity_pane_id, "client_id": 81, "bytes": [3],
})["type"] == "ack"
assert call("terminal_release", {
    "pane_id": activity_pane_id, "client_id": 81,
})["type"] == "ack"
interrupted = time.monotonic()
while time.monotonic() - interrupted < 1:
    activity_snapshot = call("snapshot")["data"]
    if not any(item["pane_id"] == activity_pane_id and item["foreground_job"]
               for item in activity_snapshot.get("pane_activity", [])):
        break
    time.sleep(0.03)
assert not any(item["pane_id"] == activity_pane_id and item["foreground_job"]
               for item in activity_snapshot.get("pane_activity", [])), activity_snapshot
# ^ The idle clock of the stale rule (docs/tasks.md "Stale worktrees"): every pane with a runtime
# has one, it stays put while the shell is silent and moves on input and on output.
def last_io(pane_id):
    rows = [item for item in call("snapshot")["data"]["pane_activity"] if item["pane_id"] == pane_id]
    assert len(rows) == 1 and rows[0]["last_io_unix_ms"] > 0, rows
    return rows[0]["last_io_unix_ms"]


time.sleep(1.0)
quiet_clock = last_io(activity_pane_id)
time.sleep(1.2)
assert last_io(activity_pane_id) == quiet_clock, "a silent shell moved its idle clock"
send_shell_command(activity_pane_id, "echo clock-moves", 83)
wait_for(lambda: last_io(activity_pane_id) > quiet_clock, "input and output move the idle clock")
# This child is a separate top-level shell, not a Claude subprocess.
send_shell_command(activity_pane_id, "printf 'marker:%s\\n' \"${CLAUDE_CODE_CHILD_SESSION-unset}\"", 82)
marker_deadline = time.monotonic() + 2
while time.monotonic() < marker_deadline:
    frame = call("view", {"pane_ids": [activity_pane_id]})
    assert frame["type"] == "view", frame
    text = "".join(cell[0] for cell in frame["data"]["frames"][0]["cells"])
    if "marker:unset" in text:
        break
    time.sleep(0.03)
assert "marker:unset" in text, text
# Missing runtime authority is rejected; it cannot attach a guessed provider.
rejected = call("agent_report", {
    "pane_id": activity_pane_id, "provider": "codex", "state": "working",
    "capabilities": {"lifecycle": True},
})
assert rejected["type"] == "error" and rejected["data"]["code"] == "stale_runtime", rejected
send_shell_command(activity_pane_id,
    '\"$WSX_AGENT_REPORT_BIN\" agent report \"$WSX_PANE_ID\" --provider codex '
    '--state working --lifecycle && sleep 1 && '
    '\"$WSX_AGENT_REPORT_BIN\" agent report \"$WSX_PANE_ID\" --provider codex '
    '--state done --lifecycle', 83)
working_seen = False
report_deadline = time.monotonic() + 4
while time.monotonic() < report_deadline:
    activity_snapshot = call("snapshot")["data"]
    activity_pane = next(item for item in activity_snapshot["panes"] if item["id"] == activity_pane_id)
    agent = activity_pane["agent"]
    if agent is not None and agent["state"] == "working":
        working_seen = True
    if working_seen and agent is not None and agent["state"] == "done":
        break
    time.sleep(0.03)
assert working_seen and agent is not None and agent["state"] == "done", activity_snapshot
current_agent = wsx("session", "current", "--json", pane_id=activity_pane_id)
assert current_agent.returncode == 0, current_agent
current_agent = json.loads(current_agent.stdout)
assert current_agent["agent"] == "codex" and current_agent["agent_status"] == "done", current_agent
send_shell_command(activity_pane_id, "exit", 84)
exit_deadline = time.monotonic() + 3
while time.monotonic() < exit_deadline:
    activity_snapshot = call("snapshot")["data"]
    activity_pane = next(item for item in activity_snapshot["panes"] if item["id"] == activity_pane_id)
    if activity_pane["exited"]:
        break
    time.sleep(0.03)
assert activity_pane["exited"] and activity_pane["agent"]["state"] == "unknown", activity_pane
activity_record = next(item for item in activity_snapshot["sessions"] if item["id"] == activity_session_id)
assert call("session_close", {
    "session_id": activity_session_id,
    "expected_revision": activity_record["revision"],
})["type"] == "ack"
port_file = WORK / "listener-port"
listener_script = WORK / "descendant-listener.py"
listener_script.write_text(
    "import os, pathlib, socket, time\n"
    "os.setpgrp()\n"
    "listener = socket.socket()\n"
    "listener.bind(('127.0.0.1', 0))\n"
    "listener.listen()\n"
    f"pathlib.Path({str(port_file)!r}).write_text(str(listener.getsockname()[1]))\n"
    "time.sleep(30)\n"
)
listener_session = call("session_create", {
    "worktree_id": worktree_id,
    "label": "listener-smoke",
    "command": [
        "/bin/sh",
        "-c",
        "\"$1\" \"$2\" & wait",
        "wsx-listener",
        sys.executable,
        str(listener_script),
    ],
    "rows": 2,
    "cols": 8,
})
assert listener_session["type"] == "created", listener_session
listener_session_id = listener_session["data"]["id"]
# ^ wsxd scans listeners with lsof every 2 s and drops a scan that raced a session change;
# on a loaded machine one scan can take seconds, so allow several scans.
deadline = time.monotonic() + 20
listener_port = None
listener_snapshot = None
listener_record = None
listener_pane_id = None
listener_attributed_record = None
while time.monotonic() < deadline:
    if port_file.exists():
        listener_port = int(port_file.read_text())
        listener_snapshot = call("snapshot")["data"]
        listener_record = next(
            item for item in listener_snapshot["sessions"] if item["id"] == listener_session_id
        )
        listener_pane_id = listener_record["focused_pane"]
        listener_attributed_record = next(
            (
                item for item in listener_snapshot.get("listening_ports", [])
                if item["pane_id"] == listener_pane_id and listener_port in item["tcp"]
            ),
            None,
        )
        if listener_attributed_record is not None:
            break
    time.sleep(0.1)
assert listener_port is not None, "listener process did not publish its port"
assert listener_attributed_record is not None, listener_snapshot
assert call("session_close", {
    "session_id": listener_session_id,
    "expected_revision": listener_record["revision"],
})["type"] == "ack"
cleanup_snapshot = None
deadline = time.monotonic() + 6
while time.monotonic() < deadline:
    cleanup_snapshot = call("snapshot")["data"]
    if not any(
        item["pane_id"] == listener_pane_id
        for item in cleanup_snapshot.get("listening_ports", [])
    ):
        break
    time.sleep(0.1)
assert cleanup_snapshot is not None and not any(
    item["pane_id"] == listener_pane_id
    for item in cleanup_snapshot.get("listening_ports", [])
), cleanup_snapshot
created = call("session_create", {
    "worktree_id": worktree_id,
    "label": "smoke-session",
    "command": [str(TERMINAL_HELPER)],
    "rows": 12,
    "cols": 40,
})
assert created["type"] == "created", created
session_id = created["data"]["id"]
deadline = time.monotonic() + 3
while time.monotonic() < deadline and not PLUGIN_MARKER.exists():
    time.sleep(0.05)
assert PLUGIN_MARKER.exists() and "session.created" in PLUGIN_MARKER.read_text()
snapshot = call("snapshot")["data"]
project = next(item for item in snapshot["projects"] if item["id"] == project_id)
assert isinstance(project["last_terminal_active_unix_ms"], int), project
assert isinstance(project["last_agent_active_unix_ms"], int), project
session = next(item for item in snapshot["sessions"] if item["id"] == session_id)
pane_id = session["focused_pane"]


def pane_text(pane):
    frames = call("view", {"pane_ids": [pane]})["data"]["frames"]
    return "".join(cell[0] for cell in frames[0]["cells"]) if frames else ""


# ^ session_create returns before its helper runs: the helper starts 0.1 s later, and seconds
# later on a loaded machine (measured under `yes` load: 2 s). The 3 s report deadline below
# counts from a helper that reads its input.
wait_for(lambda: "claude-renderer:" in pane_text(pane_id), "terminal helper did not start", 30)
reorder_peer = call("session_create", {
    "worktree_id": worktree_id,
    "label": "reorder-peer",
    "command": ["/bin/sh", "-lc", "cat"],
    "rows": 12,
    "cols": 40,
})
assert reorder_peer["type"] == "created", reorder_peer
reorder_peer_id = reorder_peer["data"]["id"]
reordered = call("session_reorder", {
    "session_id": session_id,
    "target_session_id": reorder_peer_id,
    "placement": "after",
    "expected_revision": session["revision"],
})
assert reordered["type"] == "ack", reordered
snapshot = call("snapshot")["data"]
worktree_session_ids = [
    item["id"] for item in snapshot["sessions"] if item["worktree_id"] == worktree_id
]
assert worktree_session_ids[-2:] == [reorder_peer_id, session_id], worktree_session_ids
peer = next(item for item in snapshot["sessions"] if item["id"] == reorder_peer_id)
assert call("session_close", {
    "session_id": reorder_peer_id,
    "expected_revision": peer["revision"],
})["type"] == "ack"
reported = call("agent_report", {
    "pane_id": pane_id,
    "provider": "codex",
    "state": "working",
    "conversation_id": "smoke-conversation",
    "capabilities": {"prompt": True, "resume": True, "lifecycle": True},
})
assert reported["type"] == "error" and reported["data"]["code"] == "stale_runtime", reported
send_shell_command(
    pane_id,
    "__WSX_REPORT__:smoke-conversation",
    9,
)
deadline = time.monotonic() + 3
reported_pane = None
reported_snapshot = None
while time.monotonic() < deadline:
    reported_snapshot = call("snapshot")["data"]
    reported_pane = next(item for item in reported_snapshot["panes"] if item["id"] == pane_id)
    if reported_pane["agent"] is not None:
        break
    time.sleep(0.05)
assert reported_pane is not None and reported_pane["agent"] is not None, reported_snapshot
assert reported_pane["agent"]["provider"] == "codex"
assert reported_pane["agent"]["state"] == "working"
assert reported_pane["agent"]["attached"] is True
send_shell_command(pane_id, "__WSX_DETACH__:smoke-conversation", 9)
deadline = time.monotonic() + 3
while time.monotonic() < deadline:
    reported_snapshot = call("snapshot")["data"]
    reported_pane = next(item for item in reported_snapshot["panes"] if item["id"] == pane_id)
    if reported_pane["agent"] is not None and reported_pane["agent"]["attached"] is False:
        break
    time.sleep(0.05)
assert reported_pane["agent"]["attached"] is False, reported_snapshot
assert reported_pane["agent"]["state"] == "unknown", reported_pane
assert reported_pane["agent"]["session_ref"]["value"] == "smoke-conversation", reported_pane
send_shell_command(pane_id, "__WSX_REPORT__:smoke-conversation", 9)
deadline = time.monotonic() + 3
while time.monotonic() < deadline:
    reported_snapshot = call("snapshot")["data"]
    reported_pane = next(item for item in reported_snapshot["panes"] if item["id"] == pane_id)
    if reported_pane["agent"] is not None and reported_pane["agent"]["attached"] is True:
        break
    time.sleep(0.05)
assert reported_pane["agent"]["attached"] is True, reported_snapshot
project = next(item for item in reported_snapshot["projects"] if item["id"] == project_id)
assert isinstance(project["last_agent_active_unix_ms"], int), project
assert isinstance(project["last_terminal_active_unix_ms"], int), project
session = next(item for item in call("snapshot")["data"]["sessions"] if item["id"] == session_id)
conflict = call("session_rename", {
    "session_id": session_id,
    "label": "stale",
    "expected_revision": session["revision"] - 1,
})
assert conflict["type"] == "error" and conflict["data"]["code"] == "revision_conflict", conflict

assert call("terminal_acquire", {"pane_id": pane_id, "client_id": 10, "takeover": False})["type"] == "ack"
busy = call("terminal_acquire", {"pane_id": pane_id, "client_id": 11, "takeover": False})
assert busy["type"] == "error" and busy["data"]["code"] == "terminal_busy", busy
assert call("terminal_acquire", {"pane_id": pane_id, "client_id": 11, "takeover": True})["type"] == "ack"
stale = call("terminal_input", {"pane_id": pane_id, "client_id": 10, "bytes": [3]})
assert stale["type"] == "error" and stale["data"]["code"] == "lease_required", stale
assert call("terminal_release", {"pane_id": pane_id, "client_id": 11})["type"] == "ack"
assert call("terminal_acquire", {"pane_id": pane_id, "client_id": 20, "takeover": False})["type"] == "ack"
time.sleep(2.0)
assert call("terminal_heartbeat", {"pane_id": pane_id, "client_id": 20})["type"] == "ack"
time.sleep(2.0)
still_busy = call("terminal_acquire", {"pane_id": pane_id, "client_id": 21, "takeover": False})
assert still_busy["type"] == "error" and still_busy["data"]["code"] == "terminal_busy", still_busy
assert call("terminal_release", {"pane_id": pane_id, "client_id": 20})["type"] == "ack"
assert call("terminal_acquire", {"pane_id": pane_id, "client_id": 20, "takeover": False})["type"] == "ack"
time.sleep(3.2)
assert call("terminal_acquire", {"pane_id": pane_id, "client_id": 21, "takeover": False})["type"] == "ack"
expired = call("terminal_input", {"pane_id": pane_id, "client_id": 20, "bytes": [3]})
assert expired["type"] == "error" and expired["data"]["code"] == "lease_required", expired
assert call("terminal_release", {"pane_id": pane_id, "client_id": 21})["type"] == "ack"
project = next(item for item in call("snapshot")["data"]["projects"] if item["id"] == project_id)
assert isinstance(project["last_terminal_active_unix_ms"], int), project

with wsxd_client.connect(SOCKET, 2) as terminal:
    terminal.sendall(json.dumps({
        "method": "terminal_subscribe",
        "params": {
            "pane_id": pane_id,
            "client_id": 30,
            "takeover": False,
            "rows": 12,
            "cols": 40,
        },
    }).encode() + b"\n")
    assert recv_line(terminal)["type"] == "ack"
    project = next(item for item in call("snapshot")["data"]["projects"] if item["id"] == project_id)
    assert isinstance(project["last_terminal_active_unix_ms"], int), project
    assert isinstance(project["last_agent_active_unix_ms"], int), project
    initial = recv_line(terminal)
    assert initial["type"] == "update" and initial["data"]["kind"] == "full", initial
    frame = initial["data"]["data"]
    started = time.monotonic()
    terminal.sendall(json.dumps({"type": "input", "data": list(b"stream-input\\n")}).encode() + b"\n")
    deadline = started + 1
    stream_text = ""
    while time.monotonic() < deadline:
        update = recv_line(terminal)
        assert update["type"] == "update", update
        payload = update["data"]
        if payload["kind"] == "full":
            frame = payload["data"]
        else:
            patch = payload["data"]
            assert patch["base_revision"] == frame["revision"], patch
            for changed in patch["changed_rows"]:
                cell_start = changed["row"] * frame["cols"]
                frame["cells"][cell_start:cell_start + frame["cols"]] = changed["cells"]
            frame["revision"] = patch["revision"]
            frame["cursor"] = patch["cursor"]
        stream_text = "".join(cell[0] for cell in frame["cells"])
        if "stream-input" in stream_text:
            break
    assert "stream-input" in stream_text, stream_text
    assert time.monotonic() - started < 1
    terminal.sendall(json.dumps({
        "type": "input",
        "data": list(b"\x1b]52;c;Zmlyc3Q=\x07\x1b]52;c;c2Vjb25k\x07\n"),
    }).encode() + b"\n")
    deadline = time.monotonic() + 2
    clipboard_writes = []
    while time.monotonic() < deadline and len(clipboard_writes) < 2:
        message = recv_line(terminal)
        if message["type"] == "clipboard_write":
            clipboard_writes.append(bytes(message["data"]))
            continue
        assert message["type"] == "update", message
    assert clipboard_writes == [b"first", b"second"], clipboard_writes
    terminal.sendall(json.dumps({"type": "detach"}).encode() + b"\n")

exit_effect = call("session_create", {
    "worktree_id": worktree_id,
    "label": "exit-effect",
    "command": ["/bin/sh", "-c", "read trigger; printf '\\033]52;c;ZXhpdA==\\007'"],
    "rows": 2,
    "cols": 12,
})
assert exit_effect["type"] == "created", exit_effect
exit_session_id = exit_effect["data"]["id"]
exit_snapshot = call("snapshot")["data"]
exit_session = next(item for item in exit_snapshot["sessions"] if item["id"] == exit_session_id)
exit_pane_id = exit_session["focused_pane"]
with wsxd_client.connect(SOCKET, 2) as terminal:
    terminal.sendall(json.dumps({
        "method": "terminal_subscribe",
        "params": {
            "pane_id": exit_pane_id,
            "client_id": 31,
            "takeover": False,
            "rows": 2,
            "cols": 12,
        },
    }).encode() + b"\n")
    assert recv_line(terminal)["type"] == "ack"
    terminal.sendall(json.dumps({"type": "input", "data": list(b"emit\n")}).encode() + b"\n")
    ordered_effects = []
    while "exited" not in ordered_effects:
        message = recv_line(terminal)
        if message["type"] == "clipboard_write":
            assert bytes(message["data"]) == b"exit", message
            ordered_effects.append("clipboard_write")
        elif message["type"] == "exited":
            ordered_effects.append("exited")
        else:
            assert message["type"] == "update", message
    assert ordered_effects == ["clipboard_write", "exited"], ordered_effects
exit_session = next(
    item for item in call("snapshot")["data"]["sessions"] if item["id"] == exit_session_id
)
assert call("session_close", {
    "session_id": exit_session_id,
    "expected_revision": exit_session["revision"],
})["type"] == "ack"

# ^ docs/terminal-synchronized-output.md: a program killed mid-frame never resets mode 2026.
stuck_sync = call("session_create", {
    "worktree_id": worktree_id,
    "label": "stuck-sync",
    "command": [
        "/bin/sh",
        "-c",
        "read trigger; printf '\\033[?2026hsync-released'; read hold",
    ],
    "rows": 2,
    "cols": 16,
})
assert stuck_sync["type"] == "created", stuck_sync
stuck_session_id = stuck_sync["data"]["id"]
stuck_session = next(
    item for item in call("snapshot")["data"]["sessions"] if item["id"] == stuck_session_id
)
with wsxd_client.connect(SOCKET, 3) as terminal:
    terminal.sendall(json.dumps({
        "method": "terminal_subscribe",
        "params": {
            "pane_id": stuck_session["focused_pane"],
            "client_id": 32,
            "takeover": False,
            "rows": 2,
            "cols": 16,
        },
    }).encode() + b"\n")
    assert recv_line(terminal)["type"] == "ack"
    initial = recv_line(terminal)
    assert initial["type"] == "update" and initial["data"]["kind"] == "full", initial
    frame = initial["data"]["data"]
    terminal.sendall(json.dumps({"type": "input", "data": list(b"go\n")}).encode() + b"\n")
    deadline = time.monotonic() + 3
    stuck_text = ""
    try:
        while "sync-released" not in stuck_text and time.monotonic() < deadline:
            update = recv_line(terminal)
            assert update["type"] == "update", update
            payload = update["data"]
            if payload["kind"] == "full":
                frame = payload["data"]
            else:
                patch = payload["data"]
                for changed in patch["changed_rows"]:
                    cell_start = changed["row"] * frame["cols"]
                    frame["cells"][cell_start:cell_start + frame["cols"]] = changed["cells"]
                frame["revision"] = patch["revision"]
            stuck_text = "".join(cell[0] for cell in frame["cells"])
    except socket.timeout:
        pass
    assert "sync-released" in stuck_text, (
        "an unreleased synchronized-output mode must not hide later output", stuck_text
    )
    terminal.sendall(json.dumps({"type": "detach"}).encode() + b"\n")
stuck_session = next(
    item for item in call("snapshot")["data"]["sessions"] if item["id"] == stuck_session_id
)
assert call("session_close", {
    "session_id": stuck_session_id,
    "expected_revision": stuck_session["revision"],
})["type"] == "ack"

# A stall stays in its own pane. Owner reports: a pane froze until wsxd restarted, and TUIs
# reconnected for no visible reason. A program that stops reading its input held wsxd's
# input write, and with it the pane's frames, its lease and a session close; a client that
# stops reading held the pane operation lock in a frame write, so a takeover waited 5 s.


def stall_session(label, command):
    created = call("session_create", {
        "worktree_id": worktree_id, "label": label, "command": ["/bin/sh", "-c", command],
        "rows": 6, "cols": 60,
    })
    assert created["type"] == "created", created
    return next(item for item in call("snapshot")["data"]["sessions"]
                if item["id"] == created["data"]["id"])


def frame_subscribe(pane_id, client_id, takeover=False, heartbeat=False):
    stream = wsxd_client.connect(SOCKET, 10)
    wsxd_client.send(stream, "terminal_subscribe", {
        "pane_id": pane_id, "client_id": client_id, "takeover": takeover, "rows": 6, "cols": 60,
        "heartbeat": heartbeat,
    })
    acked = recv_line(stream)
    assert acked["type"] == "ack", acked
    return stream


def close_stall(session):
    """Close the session at its current revision; the seconds the close took."""
    current = next(item for item in call("snapshot")["data"]["sessions"] if item["id"] == session["id"])
    started = time.monotonic()
    closed = call("session_close", {"session_id": session["id"], "expected_revision": current["revision"]})
    assert closed["type"] == "ack", closed
    return time.monotonic() - started


def slowest_call(seconds):
    """The longest of unrelated snapshot calls made one after another for `seconds`."""
    slowest, deadline = 0.0, time.monotonic() + seconds
    while time.monotonic() < deadline:
        started = time.monotonic()
        call("snapshot")
        slowest = max(slowest, time.monotonic() - started)
        time.sleep(0.05)
    return slowest


stall_streams = []
stall_done = threading.Event()
threading.Thread(target=heartbeat_streams, args=(stall_streams, stall_done), daemon=True).start()
# The program prints a line every 0.2 s and never reads its input.
deaf = stall_session("deaf", "while :; do echo tick-$(date +%s%N); sleep 0.2; done")
deaf_stream = frame_subscribe(deaf["focused_pane"], 40)
stall_streams.append(deaf_stream)
assert recv_line(deaf_stream)["type"] == "update"
for _ in range(16):
    send_stream(deaf_stream, {"type": "input", "data": [ord("x")] * 4096})
deaf_frames, deaf_errors, deadline = 0, [], time.monotonic() + 6
deaf_stream.settimeout(2)
while time.monotonic() < deadline:
    try:
        message = recv_line(deaf_stream)
    except socket.timeout:
        raise AssertionError("a pane whose program stops reading its input stopped sending frames")
    if message["type"] == "update":
        deaf_frames += 1
    else:
        deaf_errors.append(message)
        break
assert not deaf_errors and deaf_frames >= 10, (deaf_frames, deaf_errors)
closing = close_stall(deaf)
assert closing < 3, ("closing a pane that does not read its input", closing)
with stream_send_lock:
    stall_streams.remove(deaf_stream)
    deaf_stream.close()

# A wsx window that stops reading while its pane floods, beside a window on another pane.
flood = stall_session("flood", "while :; do seq 1 2000; done")
calm = stall_session("calm", "read hold")
calm_stream = frame_subscribe(calm["focused_pane"], 43)
stall_streams.append(calm_stream)
stalled = frame_subscribe(flood["focused_pane"], 41)
stall_streams.append(stalled)
time.sleep(1)
started = time.monotonic()
taker = frame_subscribe(flood["focused_pane"], 42, takeover=True)
took = time.monotonic() - started
# ^ A window that stops reading could hold wsxd for 5 s (its stream's write limit), which this
# catches. CI usually reads 0.01 s here; one run read 1.55 s, cause not known, so the time is printed.
print(f"takeover beside a window that stopped reading: {took:.2f} s", flush=True)
assert took < 3, ("a takeover waited for a window that stopped reading", took)
slowest = slowest_call(6)
assert slowest < 1, ("wsxd stalled behind a window that stopped reading", slowest)
calm_stream.settimeout(0)
calm_seen = b""
try:
    while chunk := calm_stream.recv(65536):
        calm_seen += chunk
except BlockingIOError:
    pass
assert b'"error"' not in calm_seen, ("the window on another pane lost its stream", calm_seen[-300:])
stall_done.set()
for stream in (taker, stalled, calm_stream):
    stream.close()
for session in (flood, calm):
    close_stall(session)

# ^ docs/terminal-raw-stream.md: PTY bytes for a client that runs its own emulator.
RAW_FIXTURE = WORK / "raw-fixture.py"
RAW_FIXTURE.write_text(
    "import fcntl, os, signal, struct, termios, tty\n"
    "tty.setraw(0)\n"
    "def winch(*_):\n"
    "    rows, cols = struct.unpack('hhhh', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\\0' * 8))[:2]\n"
    "    os.write(1, b'\\r\\n[size %dx%d]\\r\\n' % (cols, rows))\n"
    "signal.signal(signal.SIGWINCH, winch)\n"
    "os.write(1, b'raw-ready')\n"
    "while True:\n"
    "    data = os.read(0, 65536)\n"
    "    if not data or data == b'\\x04':\n"
    "        break\n"
    "    if data.startswith(b'flood:'):\n"
    "        for index in range(int(data[6:]) // 9):\n"
    "            os.write(1, b'%07d\\r\\n' % index)\n"
    "        os.write(1, b'[flood-end]')\n"
    "    else:\n"
    "        os.write(1, data)\n"
    "os.write(1, b'[bye]')\n"
)


def raw_subscribe(target, client_id, takeover, rows, cols):
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(10)
    client.connect(str(SOCKET))
    client.sendall(json.dumps({"method": "hello", "params": {"protocol": PROTOCOL}}).encode() + b"\n")
    hello = recv_line(client)
    assert hello["data"]["capabilities"]["raw_terminal_stream"] is True, hello
    client.sendall(json.dumps({
        "method": "terminal_raw_subscribe",
        "params": {
            "pane_id": target, "client_id": client_id, "takeover": takeover,
            "rows": rows, "cols": cols,
        },
    }).encode() + b"\n")
    response = recv_line(client)
    if response["type"] == "ack":
        raw_streams.append(client)
    return client, response


def raw_send(client, kind, data=None):
    send_stream(client, {"type": kind} if data is None else {"type": kind, "data": data})


def raw_close(client):
    with stream_send_lock:
        raw_streams.remove(client)
        client.close()


# ^ docs/terminal-raw-stream.md: a raw client renews its lease with a heartbeat each second,
# as the web client does, so a pause in reading never costs it the lease.
raw_streams = []
raw_done = threading.Event()
threading.Thread(target=heartbeat_streams, args=(raw_streams, raw_done), daemon=True).start()


def raw_output_until(client, marker):
    """Output bytes after the newest replay, read until they contain `marker`."""
    output = b""
    replays = []
    while marker not in output:
        message = recv_line(client)
        if message["type"] == "replay":
            replays.append(message["data"])
            output = bytes(message["data"]["ansi"]) if marker in bytes(message["data"]["ansi"]) else b""
        elif message["type"] == "output":
            output += bytes(message["data"])
        else:
            assert message["type"] in ("resized", "clipboard_write"), message
    return output, replays


raw_created = call("session_create", {
    "worktree_id": worktree_id,
    "label": "raw-stream",
    "command": [sys.executable, str(RAW_FIXTURE)],
    "rows": 12,
    "cols": 40,
})
assert raw_created["type"] == "created", raw_created
raw_session_id = raw_created["data"]["id"]
raw_pane_id = next(
    item for item in call("snapshot")["data"]["sessions"] if item["id"] == raw_session_id
)["focused_pane"]
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    frames = call("view", {"pane_ids": [raw_pane_id]})["data"]["frames"]
    if frames and "raw-ready" in "".join(cell[0] for cell in frames[0]["cells"]):
        break
    time.sleep(0.05)
refused, response = raw_subscribe(raw_pane_id, 60, False, 0, 5)
assert response["type"] == "error", response
refused.close()
first, response = raw_subscribe(raw_pane_id, 60, False, 10, 40)
assert response["type"] == "ack", response
replay = recv_line(first)
assert replay["type"] == "replay", replay
assert (replay["data"]["rows"], replay["data"]["cols"]) == (10, 40), replay
assert b"raw-ready" in bytes(replay["data"]["ansi"]), replay
# Replay, then exactly the bytes the PTY wrote after it: no gap and no repeat.
exact = b"exact-\x01\x02-\xe2\x9c\x93-bytes"
raw_send(first, "input", list(exact))
output, replays = raw_output_until(first, b"-bytes")
assert output.endswith(exact) and not replays, (output, replays)
assert output[:-len(exact)] in (b"", b"\r\n[size 40x10]\r\n"), output
# ^ docs/terminal-synchronized-output.md: the bytes of an open synchronized update wait for
# its end, then arrive in one message; the bytes before it go at once.
raw_send(first, "input", list(b"before-sync\x1b[?2026hhalf-drawn"))
output, replays = raw_output_until(first, b"before-sync")
assert b"half-drawn" not in output and not replays, output
first.settimeout(0.4)
try:
    early = recv_line(first)
except (socket.timeout, TimeoutError):
    early = None
assert early is None, ("an open update was sent", early)
first.settimeout(10)
raw_send(first, "input", list(b"-rest\x1b[?2026lafter-sync"))
whole = recv_line(first)
assert whole["type"] == "output", whole
whole = bytes(whole["data"])
assert whole.startswith(b"\x1b[?2026hhalf-drawn") and b"\x1b[?2026l" in whole, whole
if b"after-sync" not in whole:
    raw_output_until(first, b"after-sync")
# An update that never ends is sent once its bound (1 s) expires.
started = time.monotonic()
raw_send(first, "input", list(b"\x1b[?2026hnever-ends"))
raw_output_until(first, b"never-ends")
assert 0.8 <= time.monotonic() - started < 3, time.monotonic() - started
# A resize reaches the PTY. The stream reports it and restarts from a replay,
# because a client emulator reflows a resized screen differently.
raw_send(first, "resize", {"rows": 7, "cols": 33})
resized = recv_line(first)
assert resized == {"type": "resized", "data": {"rows": 7, "cols": 33}}, resized
replay = recv_line(first)
assert replay["type"] == "replay" and (replay["data"]["rows"], replay["data"]["cols"]) == (7, 33), replay
output = bytes(replay["data"]["ansi"])
while b"[size 33x7]" not in output:
    message = recv_line(first)
    assert message["type"] == "output", message
    output += bytes(message["data"])
# One writable controller: a second subscriber is refused, a takeover displaces.
busy, response = raw_subscribe(raw_pane_id, 61, False, 7, 33)
assert response["type"] == "error" and response["data"]["code"] == "terminal_busy", response
busy.close()
second, response = raw_subscribe(raw_pane_id, 61, True, 0, 0)
assert response["type"] == "ack", response
replay = recv_line(second)
assert replay["type"] == "replay", replay
assert (replay["data"]["rows"], replay["data"]["cols"]) == (7, 33), "0x0 keeps the pane size"
displaced = recv_line(first)
while displaced["type"] != "error":
    displaced = recv_line(first)
assert displaced["data"]["code"] == "lease_transferred", displaced
raw_close(first)
# A subscriber that stops reading while the pane writes more than the tap holds
# is resynchronized with a new replay. It pauses past the lease's 3 s and keeps the
# lease by its heartbeat alone.
raw_send(second, "input", list(b"flood:4194304"))
time.sleep(3.5)
output, replays = raw_output_until(second, b"[flood-end]")
assert replays, "a subscriber behind the tap must get a new replay"
raw_send(second, "input", list(b"after-flood"))
output, replays = raw_output_until(second, b"after-flood")
assert output.endswith(b"after-flood") and not replays, (output[-64:], replays)
raw_send(second, "resync")
output, replays = raw_output_until(second, b"after-flood")
assert len(replays) == 1 and b"after-flood" in bytes(replays[0]["ansi"]), replays
# The last PTY bytes arrive before the exit notice.
raw_send(second, "input", [4])
output = b""
while True:
    message = recv_line(second)
    if message["type"] == "exited":
        break
    assert message["type"] == "output", message
    output += bytes(message["data"])
assert output.endswith(b"[bye]"), output
raw_close(second)


def close_session(session_id):
    item = next(item for item in call("snapshot")["data"]["sessions"] if item["id"] == session_id)
    assert call("session_close", {"session_id": session_id,
                                  "expected_revision": item["revision"]})["type"] == "ack"


def pane_session(command, rows=24, cols=80):
    created = call("session_create", {"worktree_id": worktree_id, "label": "scrollback-check",
                                      "command": command, "rows": rows, "cols": cols})
    assert created["type"] == "created", created
    session_id = created["data"]["id"]
    return session_id, next(item for item in call("snapshot")["data"]["sessions"]
                            if item["id"] == session_id)["focused_pane"]


# ^ Ghostty reads max_scrollback as bytes, not lines (crates/wsx-terminal/src/lib.rs
# MAX_SCROLLBACK_BYTES): with 10,000 passed as rows a pane kept about 1,000 of them. 20,000
# numbered lines must leave the newest 10,000 in the raw replay's scrollback. Screen rows follow
# a cursor move (`H19979`), so a number is bounded by non-digits, not by \b.
scrollback_session, scrollback_pane = pane_session(
    ["/bin/sh", "-lc", "seq 1 20000; echo scrollback-done; sleep 60"], rows=40, cols=80)
wait_for(lambda: "scrollback-done" in pane_text(scrollback_pane), "20,000 lines were not written", 30)
scrollback_client, response = raw_subscribe(scrollback_pane, 70, False, 40, 80)
assert response["type"] == "ack", response
replay = recv_line(scrollback_client)
assert replay["type"] == "replay" and not replay["data"]["truncated"], replay["type"]
kept = {int(n) for n in re.findall(rb"(?<!\d)\d{5}(?!\d)", bytes(replay["data"]["ansi"]))}
missing = [n for n in range(10_001, 20_001) if n not in kept]
assert not missing, f"scrollback lost {len(missing)} of the newest 10,000 lines, first {missing[:3]}"
raw_close(scrollback_client)
close_session(scrollback_session)


# ^ docs/design/tui-health-audit.md #9 and #15. The TUI preview polls one pane twice a second: its
# `frames` reply carries the screen and the epoch, not the snapshot of every session. A frame stream
# that asked for it hears a heartbeat while its pane is quiet; one that did not hears none.
preview_sessions = [pane_session(["/bin/sh", "-c", "sleep 120"]) for _ in range(50)]
probe_pane = preview_sessions[0][1]


def reply_size(method):
    with wsxd_client.connect(SOCKET) as client:
        wsxd_client.send(client, method, {"pane_ids": [probe_pane]})
        data = b""
        while not data.endswith(b"\n"):
            data += client.recv(65536)
    return len(data), json.loads(data)


view_bytes, view_reply = reply_size("view")
frames_bytes, frames_reply = reply_size("frames")
assert view_reply["type"] == "view" and frames_reply["type"] == "frames", (view_reply["type"], frames_reply["type"])
assert frames_reply["data"]["epoch"] == view_reply["data"]["snapshot"]["epoch"], frames_reply["data"]["epoch"]
assert [frame["pane_id"] for frame in frames_reply["data"]["frames"]] == [probe_pane], frames_reply
assert "snapshot" not in frames_reply["data"], list(frames_reply["data"])
assert frames_bytes < view_bytes, (frames_bytes, view_bytes)
print(f"preview poll with {len(preview_sessions)} sessions: view {view_bytes} bytes, frames {frames_bytes} bytes", flush=True)

quiet_with, quiet_without = preview_sessions[1][1], preview_sessions[2][1]
hb_asking = frame_subscribe(quiet_with, 80, heartbeat=True)
hb_silent = frame_subscribe(quiet_without, 81)
hb_heard = {"asking": [], "silent": []}
for hb_stream in (hb_asking, hb_silent):
    hb_stream.settimeout(0.5)
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    for hb_name, hb_stream in (("asking", hb_asking), ("silent", hb_silent)):
        send_stream(hb_stream, {"type": "heartbeat"})
        try:
            hb_heard[hb_name].append(recv_line(hb_stream)["type"])
        except socket.timeout:
            pass
assert "heartbeat" in hb_heard["asking"], hb_heard
assert "heartbeat" not in hb_heard["silent"], hb_heard
hb_asking.close()
hb_silent.close()
for preview_session_id, _ in preview_sessions:
    close_session(preview_session_id)
print("frames reply and stream heartbeat: PASS", flush=True)


raw_done.set()
raw_session = next(
    item for item in call("snapshot")["data"]["sessions"] if item["id"] == raw_session_id
)
assert call("session_close", {
    "session_id": raw_session_id,
    "expected_revision": raw_session["revision"],
})["type"] == "ack"

deadline = time.monotonic() + 3
text = ""
while time.monotonic() < deadline:
    response = call("view", {"pane_ids": [pane_id]})
    assert response["type"] == "view", response
    frames = response["data"]["frames"]
    if frames:
        text = "".join(cell[0] for cell in frames[0]["cells"])
        if "wsx-smoke" in text and "claude-renderer:" in text:
            break
    time.sleep(0.05)
assert "wsx-smoke" in text, text
# docs/terminal-context.md "Claude in a pane": Claude in a pane uses its full-screen renderer.
assert "claude-renderer:1" in text, text

snapshot = call("snapshot")["data"]
session = next(item for item in snapshot["sessions"] if item["id"] == session_id)
split = call("pane_split", {
    "session_id": session_id,
    "target": pane_id,
    "axis": "vertical",
    "label": "split",
    "command": ["/bin/sh", "-lc", "printf 'split-ok\\n'; sleep 2"],
    "rows": 12,
    "cols": 20,
    "expected_revision": session["revision"],
})
assert split["type"] == "created", split
split_id = split["data"]["id"]
snapshot = call("snapshot")["data"]
split_session = next(item for item in snapshot["sessions"] if item["id"] == session_id)
assert len(split_session["panes"]) == 2
focused = call("pane_focus", {"session_id": session_id, "pane_id": split_id})
assert focused["type"] == "ack", focused
focused_session = next(item for item in call("snapshot")["data"]["sessions"] if item["id"] == session_id)
assert focused_session["focused_pane"] == split_id
assert focused_session["revision"] == focused["data"]["revision"]
snapshot = call("snapshot")["data"]
split_pane = next(item for item in snapshot["panes"] if item["id"] == split_id)
assert call("pane_close", {"pane_id": split_id, "expected_revision": split_pane["revision"]})["type"] == "ack"
session = next(item for item in call("snapshot")["data"]["sessions"] if item["id"] == session_id)
assert call("session_close", {"session_id": session_id, "expected_revision": session["revision"]})["type"] == "ack"
recovery = call("session_create", {
    "worktree_id": worktree_id,
    "label": "recoverable",
    "command": [],
    "initial_input": "printf 'recovered-init\\n'",
    "rows": 12,
    "cols": 40,
})
assert recovery["type"] == "created", recovery
recovery_session_id = recovery["data"]["id"]
snapshot = call("snapshot")["data"]
recovery_session = next(item for item in snapshot["sessions"] if item["id"] == recovery_session_id)
recovery_pane_id = recovery_session["focused_pane"]
recovery_pane = next(item for item in snapshot["panes"] if item["id"] == recovery_pane_id)
recovery_terminal_id = recovery_pane["terminal_id"]
missing_generation = call("agent_report", {
    "pane_id": recovery_pane_id,
    "provider": "codex",
    "state": "working",
    "conversation_id": "recoverable-conversation",
    "capabilities": {"prompt": True, "resume": True, "lifecycle": True},
})
assert missing_generation["type"] == "error"
assert missing_generation["data"]["code"] == "stale_runtime"
send_shell_command(
    recovery_pane_id,
    '"$WSX_AGENT_REPORT_BIN" agent report "$WSX_PANE_ID" --provider codex '
    '--state working --conversation-id recoverable-conversation --prompt --resume --lifecycle',
    40,
)
deadline = time.monotonic() + 3
while time.monotonic() < deadline:
    recovery_snapshot = call("snapshot")["data"]
    recovery_pane = next(
        item for item in recovery_snapshot["panes"] if item["id"] == recovery_pane_id
    )
    if recovery_pane["agent"] is not None:
        break
    time.sleep(0.05)
assert recovery_pane["agent"] is not None, recovery_snapshot

# docs/daemon-discovery.md: a shell profile that changes XDG_STATE_HOME and HOME per
# directory (direnv) still reaches the daemon that owns its pane and starts no second one.
PROFILE = WORK / "profile-b"
profile_status = WORK / "profile-status.json"
send_shell_command(
    recovery_pane_id,
    f'XDG_STATE_HOME={PROFILE}/state HOME={PROFILE}/home "$WSX_AGENT_REPORT_BIN" '
    f'runtime status --json > {profile_status}.tmp 2>&1; mv {profile_status}.tmp {profile_status}',
    41,
)
wait_for(profile_status.exists, "the pane's wsx did not answer runtime status")
profile_text = profile_status.read_text()
assert not (PROFILE / "state" / "wsx" / "wsx.sock").exists(), ("second wsxd", profile_text)
assert json.loads(profile_text)["socket"] == str(SOCKET), profile_text


def second_root(extra):
    # ^ A second root is its own daemon with its own state, beside the first, never against it.
    run = lambda *args: wsx(*args, extra_env={"WSX_DAEMON_BIN": str(DAEMON)} | extra, timeout=30)
    started = run("status", "--json")
    assert started.returncode == 0, ("second root start", extra, started.stdout, started.stderr)
    status = run("runtime", "status", "--json")
    stop = run("daemon", "stop", "--force")
    assert status.returncode == 0, ("second root", extra, status.stdout, status.stderr)
    assert stop.returncode == 0, ("second root stop", extra, stop.stdout, stop.stderr)
    return Path(json.loads(status.stdout)["socket"])


# Another socket on the same XDG state root: before, its wsxd found the first one's lock.
other_socket = WORK / "other" / "wsx.sock"
assert second_root({"WSX_SOCKET": str(other_socket)}) == other_socket
assert (other_socket.parent / "state.json").is_file()
# An empty XDG_STATE_HOME is ignored; it never makes the socket relative to the directory.
assert second_root({"XDG_STATE_HOME": ""}) == HOME / ".local" / "state" / "wsx" / "wsx.sock"
assert call("snapshot")["type"] == "snapshot"
# A client that finds no daemon names the error kind and the socket it tried.
missing_socket = WORK / "missing" / "wsx.sock"
missed = wsx("agent", "report", str(recovery_pane_id), "--provider", "codex", "--state", "idle",
             extra_env={"WSX_SOCKET": str(missing_socket)}, pane_id=recovery_pane_id)
assert missed.returncode != 0, missed
assert f"NotFound at {missing_socket}" in missed.stderr, missed.stderr
project_before_restart = call("snapshot")["data"]["projects"][0]
activity_before_restart = project_before_restart["last_agent_active_unix_ms"]
terminal_activity_before_restart = project_before_restart["last_terminal_active_unix_ms"]
assert isinstance(activity_before_restart, int) and activity_before_restart > 0
assert isinstance(terminal_activity_before_restart, int) and terminal_activity_before_restart > 0
failed_recovery = call("session_create", {
    "worktree_id": worktree_id,
    "label": "failed-recovery",
    "command": ["/definitely/missing-wsx-command"],
    "rows": 12,
    "cols": 40,
})
assert failed_recovery["type"] == "error", failed_recovery
snapshot = call("snapshot")["data"]
terminal_activity_before_restart = snapshot["projects"][0]["last_terminal_active_unix_ms"]
failed_session = next(item for item in snapshot["sessions"] if item["label"] == "failed-recovery")
failed_session_id = failed_session["id"]
failed_pane_id = failed_session["focused_pane"]
replacement = call(
    "prepare_replacement", {"target_binary_id": "0.22.0:1:2:3:ffffffffffffffff"}
)
assert replacement["type"] == "replacement", replacement
assert replacement["data"]["disposition"] == "deferred", replacement
assert replacement["data"]["live_runtimes"] >= 1, replacement
assert call("snapshot")["type"] == "snapshot"
# ^ docs/daemon-state-store.md: a reply comes after the save, so a SIGKILL right after it
# keeps the change.
killed_session = next(item for item in call("snapshot")["data"]["sessions"] if item["id"] == recovery_session_id)
renamed = call("session_rename", {
    "session_id": recovery_session_id,
    "label": "saved-before-kill",
    "expected_revision": killed_session["revision"],
})
assert renamed["type"] == "ack", renamed
process.kill()
process.wait(timeout=5)
# ^ As start() does: the killed wsxd's lock can outlive it for a moment.
wait_singleton_released()
bootstrap_env = env | {"WSX_DAEMON_BIN": str(DAEMON)}
# ^ Preserve captured bootstrap diagnostics instead of an opaque CalledProcessError.
recovered = subprocess.run(
    [str(WSX), "status", "--json"],
    env=bootstrap_env,
    check=False,
    capture_output=True,
    text=True,
)
assert recovered.returncode == 0, (
    "CLI cold recovery failed",
    recovered.returncode,
    recovered.stdout[-8192:],
    recovered.stderr[-8192:],
)
assert recovered.stdout, recovered
snapshot = call("snapshot")["data"]
assert next(item for item in snapshot["sessions"] if item["id"] == recovery_session_id)["label"] == "saved-before-kill", snapshot

# docs/daemon-live-upgrade.md: a stop ends running programs, so live sessions need --force.
live_before_stop = call("lifecycle_status")["data"]
assert live_before_stop["live_runtimes"] > 0, live_before_stop
guarded = subprocess.run(
    [str(WSX), "daemon", "stop", "--json"], env=env, check=False, capture_output=True, text=True,
)
assert guarded.returncode != 0, guarded
guarded_report = json.loads(guarded.stdout)
assert guarded_report["stopped"] is False and guarded_report["live_sessions"] > 0, guarded_report
assert "wsx daemon restart" in guarded.stderr and "--force" in guarded.stderr, guarded.stderr
assert call("lifecycle_status")["data"]["epoch"] == live_before_stop["epoch"], "a refused stop changed wsxd"
stopped = subprocess.run(
    [str(WSX), "daemon", "stop", "--force"],
    env=env,
    check=True,
    capture_output=True,
    text=True,
)
assert "saved session commands restart on next launch" in stopped.stdout, stopped.stdout
assert "programs in its sessions ended" in stopped.stdout, stopped.stdout
wait_socket_stopped()

process = start()
snapshot = call("snapshot")["data"]
assert len(snapshot["projects"]) == 1, snapshot
assert snapshot["projects"][0]["last_agent_active_unix_ms"] == activity_before_restart, snapshot
assert snapshot["projects"][0]["last_terminal_active_unix_ms"] == terminal_activity_before_restart, snapshot
assert len(snapshot["worktrees"]) == 1, snapshot
recovered_session = next(item for item in snapshot["sessions"] if item["id"] == recovery_session_id)
assert recovered_session["focused_pane"] == recovery_pane_id, recovered_session
recovered_pane = next(item for item in snapshot["panes"] if item["id"] == recovery_pane_id)
assert recovered_pane["terminal_id"] == recovery_terminal_id, recovered_pane
assert recovered_pane["exited"] is False, recovered_pane
assert recovered_pane["agent"] is None, recovered_pane
failed_session = next(item for item in snapshot["sessions"] if item["id"] == failed_session_id)
assert failed_session["focused_pane"] == failed_pane_id, failed_session
failed_pane = next(item for item in snapshot["panes"] if item["id"] == failed_pane_id)
assert failed_pane["exited"] is True, failed_pane
assert call("view", {"pane_ids": [failed_pane_id]})["data"]["frames"] == []
deadline = time.monotonic() + 3
recovered_text = ""
while time.monotonic() < deadline:
    response = call("view", {"pane_ids": [recovery_pane_id]})
    frames = response["data"]["frames"]
    if frames:
        recovered_text = "".join(cell[0] for cell in frames[0]["cells"])
        if "recovered-init" in recovered_text:
            break
    time.sleep(0.05)
assert "recovered-init" in recovered_text, recovered_text
shutdown_from_incompatible_client(process)

before = subprocess.run(
    [str(WSX), "runtime", "status", "--json"],
    env=bootstrap_env,
    check=True,
    capture_output=True,
    text=True,
)
assert json.loads(before.stdout)["running"] is False
assert not SOCKET.exists()
subprocess.run(
    [str(WSX), "status", "--json"],
    env=bootstrap_env,
    check=True,
    capture_output=True,
    text=True,
)
deadline = time.monotonic() + 5
while time.monotonic() < deadline and not SOCKET.exists():
    time.sleep(0.05)
after = subprocess.run(
    [str(WSX), "runtime", "status", "--json"],
    env=bootstrap_env,
    check=True,
    capture_output=True,
    text=True,
)
assert json.loads(after.stdout)["running"] is True
assert call("shutdown")["type"] == "ack"
wait_socket_stopped()

signal_process = start()
signal_process.terminate()
signal_process.wait(timeout=5)
wait_socket_stopped()
assert SOCKET.with_suffix(".lifecycle").read_text().strip() == "intentional"
hup_process = start()
hup_created = call("session_create", {
    "worktree_id": worktree_id,
    "label": "sighup-survives-login",
    "command": [str(TERMINAL_HELPER)],
    "rows": 12,
    "cols": 40,
})
assert hup_created["type"] == "created", hup_created
hup_session_id = hup_created["data"]["id"]
hup_snapshot = call("snapshot")["data"]
hup_session = next(item for item in hup_snapshot["sessions"] if item["id"] == hup_session_id)
hup_pane_id = hup_session["focused_pane"]
hup_pane = next(item for item in hup_snapshot["panes"] if item["id"] == hup_pane_id)
hup_terminal_id = hup_pane["terminal_id"]
hup_lifecycle = call("lifecycle_status")
assert hup_lifecycle["type"] == "lifecycle", hup_lifecycle
hup_lifecycle_identity = {
    "epoch": hup_lifecycle["data"]["epoch"],
    "binary_id": hup_lifecycle["data"]["binary_id"],
    "started_unix_ms": hup_lifecycle["data"]["started_unix_ms"],
}

deadline = time.monotonic() + 3
hup_child_pid = None
while time.monotonic() < deadline:
    frames = call("view", {"pane_ids": [hup_pane_id]})["data"]["frames"]
    if frames:
        hup_text = "".join(cell[0] for cell in frames[0]["cells"])
        match = re.search(r"wsx-smoke:(\d+)", hup_text)
        if match:
            hup_child_pid = int(match.group(1))
            break
    time.sleep(0.05)
assert hup_child_pid is not None, "terminal helper did not publish its process identity"
os.kill(hup_child_pid, 0)

os.kill(hup_process.pid, signal.SIGHUP)
deadline = time.monotonic() + 5
hup_reconnected_snapshot = None
last_reconnect_error = None
while time.monotonic() < deadline:
    if hup_process.poll() is not None:
        raise RuntimeError(f"wsxd exited after SIGHUP: {hup_process.returncode}")
    try:
        hup_reconnected_snapshot = call("snapshot")["data"]
        break
    except (OSError, RuntimeError) as error:
        last_reconnect_error = error
        time.sleep(0.05)
assert hup_reconnected_snapshot is not None, last_reconnect_error
assert SOCKET.exists()
hup_lifecycle_after = call("lifecycle_status")
assert hup_lifecycle_after["type"] == "lifecycle", hup_lifecycle_after
assert {
    "epoch": hup_lifecycle_after["data"]["epoch"],
    "binary_id": hup_lifecycle_after["data"]["binary_id"],
    "started_unix_ms": hup_lifecycle_after["data"]["started_unix_ms"],
} == hup_lifecycle_identity
hup_pane_after = next(
    item for item in hup_reconnected_snapshot["panes"] if item["id"] == hup_pane_id
)
assert hup_pane_after["terminal_id"] == hup_terminal_id, hup_pane_after
assert hup_pane_after["exited"] is False, hup_pane_after
os.kill(hup_child_pid, 0)
hup_marker = "reconnected"
send_shell_command(hup_pane_id, f"__WSX_PID__:{hup_marker}", 50)
deadline = time.monotonic() + 3
hup_text = ""
while time.monotonic() < deadline:
    frames = call("view", {"pane_ids": [hup_pane_id]})["data"]["frames"]
    if frames:
        hup_text = "".join(cell[0] for cell in frames[0]["cells"])
        if f"wsx-helper:{hup_child_pid}:{hup_marker}" in hup_text:
            break
    time.sleep(0.05)
assert f"wsx-smoke:{hup_child_pid}" in hup_text, hup_text
assert f"wsx-helper:{hup_child_pid}:{hup_marker}" in hup_text, hup_text
hup_session = next(
    item for item in call("snapshot")["data"]["sessions"] if item["id"] == hup_session_id
)
assert call("session_close", {
    "session_id": hup_session_id,
    "expected_revision": hup_session["revision"],
})["type"] == "ack"
shutdown(hup_process)

contenders = [
    subprocess.Popen(
        [str(WSX), "status", "--json"],
        env=bootstrap_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    for _ in range(6)
]
for contender in contenders:
    stdout, stderr = contender.communicate(timeout=10)
    assert contender.returncode == 0, (stdout, stderr)
assert call("lifecycle_status")["data"]["active_clients"] >= 1
assert call("shutdown")["type"] == "ack"
wait_socket_stopped()

state_file = STATE / "wsx" / "state.json"
backup_file = STATE / "wsx" / "state.json.backup"
assert state_file.is_file() and backup_file.is_file()
state_file.write_text("{invalid")
state_file.chmod(0o600)
recovered_from_backup = start()
assert list(state_file.parent.glob("state.json.corrupt.*"))
assert call("lifecycle_status")["data"]["recovered_from_backup"] is True
# Availability notices stay off stderr for JSON callers and take one line otherwise.
# ^ A same-revision daemon is always current, so the upgrade-deferred notice is not reachable here.
notice_json = wsx("status", "--json")
assert notice_json.returncode == 0 and notice_json.stderr == "", notice_json.stderr
json.loads(notice_json.stdout)
notice_text = wsx("status")
assert notice_text.returncode == 0, notice_text
assert notice_text.stderr == (
    "wsx: wsxd recovered a corrupt primary state from its last-known-good backup\n"
), notice_text.stderr
shutdown(recovered_from_backup)

# docs/ui-state-ownership.md: the TUI's cursor stays on the session the owner looks at
# while sessions above it close, new ones open and wsxd hands off. The cursor the TUI keeps
# in its cache is the oracle.
cursor_daemon = start()
assert call("synchronize_projects", {"projects": [{
    "path": str(PROJECT), "name": "smoke", "worktrees": [{"path": str(PROJECT), "branch": "main"}],
}]})["type"] == "ack"
CURSOR_CACHE = (HOME / "Library" / "Caches" if sys.platform == "darwin" else WORK / "cache") / "wsx" / "workspace-v3.toml"


def cursor_terminal():
    match = re.search(r'\[cursor_identity\.Session\][^\[]*?terminal_id = "(\d+)"',
                      CURSOR_CACHE.read_text() if CURSOR_CACHE.exists() else "")
    return match and int(match.group(1))


def cursor_session(label, command=("/bin/sh",)):
    created = call("session_create", {"worktree_id": call("snapshot")["data"]["worktrees"][0]["id"],
                                      "label": label, "command": list(command), "rows": 24, "cols": 80})
    assert created["type"] == "created", created
    return next(pane["terminal_id"] for pane in call("snapshot")["data"]["panes"]
                if pane["session_id"] == created["data"]["id"]), created["data"]["id"]

for leftover in call("snapshot")["data"]["sessions"]:
    assert call("session_close", {"session_id": leftover["id"],
                                  "expected_revision": leftover["revision"]})["type"] == "ack"
cursor_sessions = {label: cursor_session(label) for label in ("keep-a", "keep-b", "keep-c")}


def settled_cursor():
    # ^ The TUI applies a snapshot within about a second of the event and writes a changed
    # cursor to its cache then.
    time.sleep(2.5)
    return cursor_terminal()


def smoke_tui(wsx=WSX, tui_env=None):
    # ^ The WSX_PANE_ID of this run would make the TUI a pane of a session.
    return TuiPty([str(wsx)], PROJECT, tui_env or {k: v for k, v in env.items() if k != "WSX_PANE_ID"})


tui = smoke_tui()


def in_terminal_mode():
    # ^ The mode line drawn last names the mode.
    return tui.raw.rfind(b"TERMINAL") > tui.raw.rfind(b"WORKSPACE")


try:
    # The last row is keep-c, the newest session.
    wait_for(lambda: b"keep-c" in tui.raw, "the TUI did not show keep-c")
    tui.send(b"j" * 12)
    wait_for(lambda: cursor_terminal() == cursor_sessions["keep-c"][0],
             "the cursor did not reach keep-c")
    viewed = cursor_sessions["keep-c"][0]

    # docs/ui-state-ownership.md "Runtime refresh": a new connection is accepted at once,
    # and a pane that prints without pause does not make the TUI fetch a snapshot and draw
    # for each read. Each draw opens synchronized output (`CSI ? 2026 h`).
    connect_ms = []
    for _ in range(21):
        started = time.monotonic()
        wsxd_client.connect(SOCKET).close()
        connect_ms.append((time.monotonic() - started) * 1000)
    connect_ms = sorted(connect_ms)[10]
    printer = cursor_session("printer", ("/bin/sh", "-c", "while :; do date; sleep 0.02; done"))
    time.sleep(2)
    drawn_from = len(tui.raw)
    time.sleep(5)
    draws = tui.raw[drawn_from:].count(b"\x1b[?2026h")
    print(f"tui refresh: connect median {connect_ms:.1f} ms, {draws} draws in 5 s of output", flush=True)
    assert connect_ms < 5, f"a new connection waited {connect_ms:.1f} ms for its hello"
    assert draws <= 5, f"the TUI drew {draws} times in 5 s while only a pane printed"
    printer_session = next(item for item in call("snapshot")["data"]["sessions"]
                           if item["id"] == printer[1])
    assert call("session_close", {"session_id": printer_session["id"],
                                  "expected_revision": printer_session["revision"]})["type"] == "ack"

    def viewed_shell_answers(marker, value):
        # ^ The shell's answer, not the typed line, on the TUI's screen: the screen is still
        # the viewed pane, and keys still reach it. Keys typed while the stream reattaches
        # are dropped, so the line is typed again until it is answered.
        time.sleep(2.5)
        answer = f"{marker}-{40 + value}".encode()
        deadline = time.monotonic() + 20
        while answer not in tui.raw:
            assert time.monotonic() < deadline, f"the TUI does not show the viewed shell's answer {marker}"
            # ^ Keys typed outside Terminal mode are commands ('e' opens an editor).
            if in_terminal_mode():
                tui.send(f"echo {marker}-$((40+{value}))\r".encode())
            time.sleep(1)
        assert settled_cursor() == viewed, (marker, CURSOR_CACHE.read_text())

    # A session above the viewed one closes and a new one opens below it: the row number of
    # the viewed session changes, the session does not.
    closing = next(item for item in call("snapshot")["data"]["sessions"]
                   if item["id"] == cursor_sessions["keep-a"][1])
    assert call("session_close", {"session_id": closing["id"],
                                  "expected_revision": closing["revision"]})["type"] == "ack"
    cursor_session("keep-d")
    assert settled_cursor() == viewed, ("the cursor left the viewed session", cursor_sessions, CURSOR_CACHE.read_text())

    # docs/design/tui-health-audit.md finding 7: wsxd stops (SIGSTOP) and does not answer. Enter
    # opens the session on a worker, so keys still move the selection, an open the selection left
    # is dropped when wsxd answers late, and an open that stays wanted reports its failure.
    def tui_sees(text, since):
        return text in tui.raw[since:]

    os.kill(cursor_daemon.pid, signal.SIGSTOP)
    try:
        tui.send(b"\r")
        time.sleep(0.5)  # the open job and its draw
        mark = len(tui.raw)
        sent = time.monotonic()
        tui.send(b"k")  # the highlight moves to keep-b: its cells are drawn again
        while not tui_sees(b"keep-b", mark):
            assert time.monotonic() - sent < 5, "a key did not move the selection while wsxd was silent"
            time.sleep(0.005)
        key_ms = (time.monotonic() - sent) * 1000
        print(f"tui wedged wsxd: the selection moved {key_ms:.0f} ms after the key (was a freeze of up to 5 s)", flush=True)
        assert key_ms < 200, f"the selection moved {key_ms:.0f} ms after the key while wsxd was silent"
        time.sleep(1)
    finally:
        os.kill(cursor_daemon.pid, signal.SIGCONT)
    time.sleep(1)
    assert not in_terminal_mode(), "an open the selection had left entered Terminal mode"
    tui.send(b"j")
    time.sleep(0.5)
    os.kill(cursor_daemon.pid, signal.SIGSTOP)
    try:
        failing = time.monotonic()
        tui.send(b"\r")
        tui.wait_text(r"Terminal stream failed", "the open did not report that wsxd did not answer", 30)
        print(f"tui wedged wsxd: the open reported its failure after {time.monotonic() - failing:.1f} s", flush=True)
    finally:
        os.kill(cursor_daemon.pid, signal.SIGCONT)
    time.sleep(1)
    assert not in_terminal_mode(), "a failed open entered Terminal mode"
    tui.send(b"\r")
    wait_for(in_terminal_mode, "the TUI did not open keep-c")
    viewed_shell_answers("opened", 1)
    # wsxd stops for longer than a lease lasts (3 s), as when it is busy: the lease runs out and
    # the TUI reattaches once by itself instead of leaving Terminal mode.
    os.kill(cursor_daemon.pid, signal.SIGSTOP)
    time.sleep(4)
    os.kill(cursor_daemon.pid, signal.SIGCONT)
    viewed_shell_answers("stalled", 4)
    assert in_terminal_mode(), "a lease that ran out ended Terminal mode"
    # A live handoff into the same build reconnects the TUI; the view stays.
    epoch = call("lifecycle_status")["data"]["epoch"]
    restarted = wsx("daemon", "restart")
    assert restarted.returncode == 0, restarted
    wait_for(lambda: call("lifecycle_status")["data"]["epoch"] != epoch, "wsxd did not hand off")
    cursor_session("keep-e")
    viewed_shell_answers("handed-off", 2)

    # docs/terminal-exit-ordering.md: the viewed program exits. The TUI says so and how to go
    # on, and Enter starts the session's command again in the same pane.
    def viewed_exited():
        return next(pane for pane in call("snapshot")["data"]["panes"]
                    if pane["terminal_id"] == viewed)["exited"]
    # ^ As in viewed_shell_answers: a key typed while the stream reattaches is dropped. A pane
    # that already exited gets no second line: its Enter would start the program again.
    exit_deadline = time.monotonic() + 20
    while b"Enter starts it again" not in tui.raw:
        assert time.monotonic() < exit_deadline, (
            "the TUI did not say the program exited", viewed_exited(), bytes(tui.raw[-600:]))
        if in_terminal_mode() and not viewed_exited():
            tui.send(b"exit\r")
        time.sleep(1)
    assert viewed_exited()
    tui.send(b"\r")
    viewed_shell_answers("restarted", 3)
    assert not viewed_exited()

    # docs/design/tui-health-audit.md #5: a window drag sends many resize events; wsxd gets the
    # last size, not one resize per event. The raw fixture prints a line at each SIGWINCH it gets.
    viewed_pane = next(pane["id"] for pane in call("snapshot")["data"]["panes"]
                       if pane["terminal_id"] == viewed)
    tui.send(f"python3 {RAW_FIXTURE}; stty sane\r")
    wait_for(lambda: "raw-ready" in pane_text(viewed_pane), "the resize fixture did not start", 30)
    time.sleep(1)
    sizes_before = pane_text(viewed_pane).count("[size ")
    for step in range(20):
        tui.resize(30 + step, 100 + step)
    # ^ A loaded host reads the burst late: wait for the first resize, then let the rest settle.
    wait_for(lambda: pane_text(viewed_pane).count("[size ") > sizes_before,
             lambda: ("no resize reached the pane", pane_text(viewed_pane)[-300:], bytes(tui.raw[-400:])), 30)
    time.sleep(3)
    resizes = pane_text(viewed_pane).count("[size ") - sizes_before
    print(f"tui resize: a burst of 20 window sizes gave wsxd {resizes} resize(s)", flush=True)
    assert 1 <= resizes <= 3, f"20 window sizes in a burst reached the pane as {resizes} resizes"
    tui.resize(40, 120)
    time.sleep(1)
    tui.send(b"\x04")
    wait_for(lambda: "[bye]" in pane_text(viewed_pane), "the resize fixture did not end", 30)
    viewed_shell_answers("resized", 5)

    # audit #6 and #8: after `wsx daemon stop` the TUI's 30 s routine refresh and its event
    # monitor leave wsxd stopped, and the outage shows no restart failure.
    mark = len(tui.raw)
    stopped_by_owner = wsx("daemon", "stop", "--force")
    assert stopped_by_owner.returncode == 0, stopped_by_owner
    wait_socket_stopped()
    time.sleep(35)
    assert not SOCKET.exists(), "the TUI started wsxd again after `wsx daemon stop`"
    assert b"restart failed" not in tui.raw[mark:], bytes(tui.raw[mark:])[-600:]
    print("tui stop: wsxd was still stopped 35 s after `wsx daemon stop` with the TUI running", flush=True)
    cursor_daemon = start()
finally:
    tui.close()

# `d` on a project asks; `y` only unregisters it (its sessions keep running), `H` also halts all
# its sessions, the hidden `task-manager` one included (docs/web-client.md "Projects").
removal = {name: WORK / name for name in ("halt-me", "keep-me")}
for name, folder in removal.items():
    folder.mkdir()
    subprocess.run(["git", "init", "-q", "-b", "main", str(folder)], check=True, env=env)
with (CONFIG_DIR / "config-v2.toml").open("a") as config_file:
    for name, folder in removal.items():
        config_file.write(f"[[projects]]\nname = \"{name}\"\npath = {json.dumps(str(folder))}\n")
remove_tui = smoke_tui()


def project_sessions(name):
    snapshot = call("snapshot")["data"]
    project = next((item for item in snapshot["projects"] if item["name"] == name), None)
    trees = {item["id"] for item in snapshot["worktrees"] if project and item["project_id"] == project["id"]}
    return [item["label"] for item in snapshot["sessions"] if item["worktree_id"] in trees]


def select_in_tui(name):
    # ^ `[` goes up to the first project, then each `]` down one; `d` asks about the project under
    # the cursor, which names it, and `q` closes the question. Keys reach the TUI in order, so
    # no sleep sits between them: each wait is for what the screen shows, and for long. (An Esc
    # followed at once by `]` would read as Alt-`]`.)
    asked = re.compile(r"Unregister project '([^']+)'")
    remove_tui.send(b"[" * 6)
    for _ in range(8):
        remove_tui.send(b"d")
        wait_for(lambda: asked.search(remove_tui.text()), "the TUI asked no project question", seconds=60)
        shown = asked.search(remove_tui.text()).group(1)
        remove_tui.send(b"q")
        wait_for(lambda: "Unregister project" not in remove_tui.text(), "the question stayed after q", seconds=60)
        if shown == name:
            return
        remove_tui.send(b"]")
    raise AssertionError(f"the cursor never reached {name}:\n{remove_tui.text()}")


def remove_from_tui(name, key):
    select_in_tui(name)
    remove_tui.send(b"d")
    wait_for(lambda: f"Unregister project '{name}'" in remove_tui.text(), "the TUI asked no project question", seconds=60)
    assert "halt all its sessions" in remove_tui.text(), remove_tui.text()
    remove_tui.send(key)


def config_names():
    return re.findall(r'name = "([^"]+)"', (CONFIG_DIR / "config-v2.toml").read_text())


try:
    wait_for(lambda: {"halt-me", "keep-me"} <= {item["name"] for item in call("snapshot")["data"]["projects"]},
             "the TUI did not give wsxd the added projects")
    for name in removal:
        tree = next(item["id"] for item in call("snapshot")["data"]["worktrees"] if item["path"] == str(removal[name].resolve()))
        for label in ("shell", "task-manager"):
            created = call("session_create", {"worktree_id": tree, "label": label, "command": ["/bin/sh"], "rows": 24, "cols": 80})
            assert created["type"] == "created", created
        assert sorted(project_sessions(name)) == ["shell", "task-manager"], project_sessions(name)
    wait_for(lambda: "halt-me" in remove_tui.text(), "the TUI did not list halt-me", seconds=60)
    # The TUI's Tasks view (`t`) sums the usage of every project (docs/tasks.md "TUI"): two
    # projects with a manager reply each, 0.03 and 0.012 USD today, show as $0.04 in one line.
    # ^ The store id is `<name>-<first 8 hex of sha256(path)>` (wsx-core task/store.rs `project_id`).
    # Each reply names its adapter and model, so `a` counts one agent (docs/tasks.md "Usage").
    for name, micro, adapter, model in (("halt-me", 30000, "claude", "m1"), ("keep-me", 12000, "pi", "local")):
        manager = STATE / "wsx" / "tasks" / f"{name}-{hashlib.sha256(str(removal[name]).encode()).hexdigest()[:8]}" / "manager"
        manager.mkdir(parents=True, mode=0o700)
        (manager / "log.jsonl").write_text(json.dumps(
            {"unix_ms": int(time.time() * 1000), "kind": "reply", "id": 1, "to": [], "text": "ok",
             "usage": {"cost_micro_usd": micro, "turns": 1, "adapter": adapter, "model": model}}) + "\n")
    # ^ Claude's newest limit state, as `claude.rs` keeps a rate_limit_event (`store::note_rate_limit`).
    agents = STATE / "wsx" / "tasks" / "agents"
    agents.mkdir(parents=True, exist_ok=True, mode=0o700)
    now = int(time.time())
    (agents / "claude.rate.json").write_text(json.dumps({"unix_ms": now * 1000, "info": {
        "status": "allowed", "resetsAt": now + 3600, "rateLimitType": "five_hour", "unifiedWindows": {
            "five_hour": {"utilization": 0.25, "resetsAt": now + 3600, "status": "allowed"},
            "seven_day": {"utilization": 0.47, "resetsAt": now + 3 * 86400, "status": "allowed"}}}}))
    remove_tui.send(b"[t")
    wait_for(lambda: "All projects" in remove_tui.text(), "the Tasks view showed no all-projects usage", seconds=60)
    all_projects = next(row.strip() for row in remove_tui.text().splitlines() if "All projects" in row)
    assert re.search(r"All projects Today \$0\.04 \(manager \$0\.04\) · 7 days \$0\.04 \(manager \$0\.04\)", all_projects), all_projects
    limit = next((row.strip() for row in remove_tui.text().splitlines() if "Claude limit" in row), "")
    assert re.search(r"Claude limit 5 hours 25%, resets \d\d:\d\d · 7 days 47%", limit), remove_tui.text()
    # ^ `a` counts the first agent of the 7 days, `claude`: only halt-me's 0.03 USD.
    remove_tui.send(b"a")
    wait_for(lambda: "All projects Today $0.03" in remove_tui.text(), "a did not count one agent", seconds=60)
    remove_tui.send(b"q")
    wait_for(lambda: "All projects" not in remove_tui.text(), "q did not close the Tasks view", seconds=60)
    # The TUI's manager chat renders wsx-core `chat_rows`, the rows the web gets (docs/tasks.md
    # "TUI"): keep-me's manager is named Atlas, and its log holds each kind of row.
    (removal["keep-me"] / "wsx.config.yml").write_text("tasks:\n  manager:\n    name: Atlas\n")
    at = int(time.time() * 1000) - 600_000
    chat_log = [
        {"kind": "message", "id": 10, "from": "owner", "text": "Where is #3?"},
        # ^ A turn took it (reply 16 answers it); only message 18 below is still waiting.
        {"kind": "delivered", "ids": [10]},
        {"kind": "message", "id": 11, "from": "events", "text": "#3 landed r2: Chat rows; worker says: done",
         "short": [{"text": "#3 done", "task": 3}]},
        {"kind": "reply", "id": 12, "to": [11], "text": "#3 is on main.", "quiet": True},
        {"kind": "message", "id": 13, "from": "recap", "text": "RECAP REQUEST"},
        {"kind": "reply", "id": 14, "to": [13], "text": "Quiet day: one landing."},
        {"kind": "message", "id": 15, "from": "manager:elsewhere", "text": "Hello from elsewhere."},
        # ^ Thirty peer lines after Hello, so the question is more than a screen below the chat's top.
        *[{"kind": "message", "id": 30 + n, "from": "manager:elsewhere", "text": f"Filler line {n}."} for n in range(30)],
        {"kind": "ping", "usage": {"cost_micro_usd": 0, "turns": 1}},
        {"kind": "rotated", "tokens": 152000, "reason": "the launch changed"},
        {"kind": "reply", "id": 16, "to": [10], "text": "Should I land #4?", "asks": True},
        {"kind": "reply", "id": 17, "to": [10], "text": "Shall I deploy it too?", "asks": True},
        {"kind": "message", "id": 18, "from": "owner", "text": "Not yet."},
        # ^ Two stops of one reason, nothing between: one row with the count. The stopped manager
        # holds, so the owner message no turn took is a waiting line after it (`waiting_ids`).
        {"kind": "stopped", "reason": "agent exited"},
        {"kind": "stopped", "reason": "agent exited"},
    ]
    keep_log = STATE / "wsx" / "tasks" / f"keep-me-{hashlib.sha256(str(removal['keep-me']).encode()).hexdigest()[:8]}" / "manager" / "log.jsonl"
    with keep_log.open("a") as log_file:
        for n, entry in enumerate(chat_log):
            log_file.write(json.dumps({"unix_ms": at + n * 1000, **entry}) + "\n")
    # A task that waits for another (`wsx task after`): the Tasks view's row adds the reason after
    # the label, and the detail of the task waited for lists who waits (docs/tasks.md "TUI").
    keep_store = keep_log.parent.parent
    now_ms = int(time.time() * 1000)
    for number, state, after in ((1, "working", None), (2, "ready", {"task": 1, "text": "WORK x.txt", "since_unix_ms": now_ms, "because": "check"})):
        record = {"id": number, "project": "keep-me", "project_id": keep_store.name, "project_path": str(removal["keep-me"]),
                  "default_branch": "main", "base": "0" * 40, "branch": f"wsx/t{number}", "working_copy": str(removal["keep-me"]),
                  "pane_id": None, "adapter": "claude", "model": "m1", "checks": [], "agent_session": None, "state": state,
                  "revision": 1, "created_unix_ms": now_ms, "updated_unix_ms": now_ms, "state_unix_ms": now_ms}
        if after:
            record["after"] = after
        (keep_store / str(number)).mkdir(mode=0o700)
        (keep_store / str(number) / "task.json").write_text(json.dumps(record))
        (keep_store / str(number) / "log.jsonl").write_text("")
    select_in_tui("keep-me")
    remove_tui.send(b"t")
    wait_for(lambda: "All projects" in remove_tui.text(), "t opened no Tasks view for keep-me", seconds=60)
    wait_for(lambda: re.search(r"#2\s+ready\s+Waiting for #1 to land · its checks fail", remove_tui.text()),
             lambda: f"the Tasks view row did not give the reason of the wait:\n{remove_tui.text()}", seconds=60)
    remove_tui.send(b"\r")
    # ^ A narrow detail wraps the sentence: the words are read with their line breaks as one space.
    wait_for(lambda: "Its checks fail" in " ".join(remove_tui.text().split()),
             lambda: f"the detail did not give the reason:\n{remove_tui.text()}", seconds=60)
    remove_tui.send(b"\x1b")
    wait_for(lambda: "Its checks fail" not in " ".join(remove_tui.text().split()), "Esc did not close the detail", seconds=60)
    remove_tui.send(b"j\r")
    wait_for(lambda: re.search(r"Waited by\s+#2", remove_tui.text()),
             lambda: f"the detail of #1 did not list who waits for it:\n{remove_tui.text()}", seconds=60)
    remove_tui.send(b"\x1b")
    wait_for(lambda: "Waited by" not in remove_tui.text(), "Esc did not close the detail of #1", seconds=60)
    shutil.rmtree(keep_store / "1")
    shutil.rmtree(keep_store / "2")
    remove_tui.send(b"m")
    wait_for(lambda: "Atlas stopped 2 times: agent exited" in remove_tui.text(), "the TUI chat did not show keep-me's log", seconds=60)
    # ^ Each row in order, by its kind and shown name: an owner message, a short event line, a
    # quiet reply under the manager's name, the recap answer tagged, a peer, a rotation with its
    # reason, an asking reply and a stop, both by the shown name. The recap request and the ping
    # show nothing.
    def rows_in_order(expected):
        chat_screen = remove_tui.text()
        shown = [row.strip() for row in chat_screen.splitlines() if row.strip()]
        found, at_row = [], 0
        for pattern in expected:
            at_row = next((n for n in range(at_row, len(shown)) if re.search(pattern, shown[n].lstrip("│ "))), None)
            assert at_row is not None, f"the TUI chat has no row {pattern} after {found}:\n{chat_screen}"
            found.append(pattern)
            at_row += 1
        assert "RECAP REQUEST" not in chat_screen and "Manager" not in chat_screen and "r2:" not in chat_screen, chat_screen
    # ^ The open question's row under the chat leaves no room for the first rows; they are
    # read after the scroll to the top below.
    rows_in_order([r"^New conversation: the launch changed; the ledger carries the state\.$",
                   r"^Atlas\s+\d\d:\d\d", r"^Should I land #4\?", r"^Shall I deploy it too\?",
                   r"^Atlas stopped 2 times: agent exited$", r"^You\s+waiting\s+Not yet\.$"])
    # ^ Two questions, then an owner message with no quote: it answers the newer one only; the
    # older one waits under the chat, counted in the heading. `r` starts a reply that quotes
    # it, Esc drops that, and `d` dismisses it (docs/tasks.md "Questions" under "Manager").
    chat_screen = remove_tui.text()
    assert "1 question  0 to check  0 approvals" in chat_screen, chat_screen
    assert re.search(r"▸ asks +Should I land #4\?", chat_screen) and not re.search(r"asks +Shall I deploy", chat_screen), chat_screen
    assert "(r)eply" in chat_screen and "(d)ismiss" in chat_screen, chat_screen
    # ^ Enter on the question item scrolls the chat to its reply (docs/tasks.md "TUI"): from the
    # top of the chat, where the item's own line is the only place the question shows.
    # ^ The 200 keys scroll one line each: "Where is #3?" shows one line before the owner header
    # above it, so the top is reached when that header row is on the screen too.
    def chat_top_shown():
        shown = [row.strip().lstrip("│ ") for row in remove_tui.text().splitlines() if row.strip()]
        at_row = next((n for n, row in enumerate(shown) if row.startswith("Where is #3?")), None)
        return at_row is not None and at_row > 0 and re.search(r"^You\s+\d\d:\d\d", shown[at_row - 1]) is not None
    def question_in_chat():
        return any(re.search(r"^Should I land #4\?", row.strip().lstrip("│ ")) for row in remove_tui.text().splitlines())
    remove_tui.send(b"k" * 200)
    wait_for(chat_top_shown, lambda: f"k did not scroll the TUI chat to its top:\n{remove_tui.text()}", seconds=60)
    assert not question_in_chat(), remove_tui.text()
    remove_tui.send(b"\r")
    wait_for(question_in_chat, "Enter on the question item did not show its reply in the chat", seconds=60)
    remove_tui.send(b"r")
    wait_for(lambda: "Reply to “Should I land #4?”:" in remove_tui.text(), "r did not start a reply to the question", seconds=60)
    remove_tui.send(b"\x1b")
    wait_for(lambda: "Reply to" not in remove_tui.text(), "Esc did not drop the reply", seconds=60)
    remove_tui.send(b"d")
    wait_for(lambda: "1 question" not in remove_tui.text() and "▸ asks" not in remove_tui.text(),
             "d did not dismiss the open question", seconds=60)
    assert [json.loads(line)["id"] for line in keep_log.read_text().splitlines()
            if '"kind":"answered"' in line] == [16], keep_log.read_text()[-400:]
    # ^ The chat opens at its end; on a short screen the first rows are above it, and k scrolls
    # up one line at a time to the top.
    remove_tui.send(b"k" * 200)
    wait_for(chat_top_shown, lambda: f"k did not scroll the TUI chat to its top:\n{remove_tui.text()}", seconds=60)
    rows_in_order([r"^You\s+\d\d:\d\d", r"^Where is #3\?", r"^#3 done$", r"^Atlas$", r"^#3 is on main\.$",
                   r"^Atlas\s+Recap\s+\d\d:\d\d", r"^Quiet day: one landing\.", r"^elsewhere\s+\d\d:\d\d",
                   r"^Hello from elsewhere\."])
    remove_tui.send(b"\x1b")
    wait_for(lambda: "press i to write a message" not in remove_tui.text(), "Esc did not close the manager chat", seconds=60)
    remove_tui.send(b"q")
    wait_for(lambda: "All projects" not in remove_tui.text(), "q did not close keep-me's Tasks view", seconds=60)
    # docs/design/tui-health-audit.md #1, #2, #10, #14: the chat of a manager log of about 7.5
    # MiB (the log rolls over at 8 MiB) reads only the end of it, and a 20 KB line in the
    # composer costs the same as a short one. The find in the chat is in scripts/test-task.py.
    MiB = 1024 * 1024
    halt_log = STATE / "wsx" / "tasks" / f"halt-me-{hashlib.sha256(str(removal['halt-me']).encode()).hexdigest()[:8]}" / "manager" / "log.jsonl"
    filler = "lorem ipsum dolor sit amet " * 11
    filler_count = 0
    base = int(time.time() * 1000) - 7_200_000
    with halt_log.open("a") as log_file:
        while log_file.tell() < 7.5 * MiB:
            text = f"old-{filler_count:05d} " + filler
            log_file.write(json.dumps({"unix_ms": base + filler_count, "kind": "message", "id": 100 + filler_count,
                                       "from": "owner", "text": text}) + "\n")
            filler_count += 1
        log_file.write(json.dumps({"unix_ms": base + filler_count, "kind": "message", "id": 100 + filler_count,
                                   "from": "owner", "text": "LAST-ENTRY-0"}) + "\n")
    assert 7 * MiB < halt_log.stat().st_size < 8 * MiB - 64 * 1024, halt_log.stat().st_size
    select_in_tui("halt-me")
    remove_tui.send(b"t")
    wait_for(lambda: "All projects" in remove_tui.text(), "t opened no Tasks view for halt-me", seconds=120)
    remove_tui.send(b"m")
    wait_for(lambda: "LAST-ENTRY-0" in remove_tui.text(), "the TUI chat did not show the end of the 7.5 MiB log", seconds=120)
    # ^ A new entry shows within the poll interval (1 s) and one read of the end: a read of the
    # whole log, about 20,000 entries here, takes far longer in this build.
    REFRESH_SECONDS = 4
    started = time.monotonic()
    with halt_log.open("a") as log_file:
        log_file.write(json.dumps({"unix_ms": base + filler_count + 1, "kind": "message", "id": 100 + filler_count + 1,
                                   "from": "owner", "text": "LAST-ENTRY-1"}) + "\n")
    wait_for(lambda: "LAST-ENTRY-1" in remove_tui.text(), "the chat did not show the new entry", seconds=60)
    refresh = time.monotonic() - started
    print(f"smoke: the chat of a {halt_log.stat().st_size / MiB:.1f} MiB log refreshed in {refresh:.2f} s", flush=True)
    assert refresh < REFRESH_SECONDS, f"the chat refresh took {refresh:.2f} s"
    # ^ A 20 KB line typed (a terminal without bracketed paste sends a paste as keys), in ten
    # parts of 2,000 characters, each ending in a mark that shows when the TUI has taken the
    # part. Each key costs one redraw (the first part sets the cost of that), so the last part
    # must take about as long as the first; a cost that grows with the line (the old loop, which
    # cut the line one character at a time) makes it take many times as long.
    remove_tui.send(b"i")
    wait_for(lambda: "Enter: send" in remove_tui.text(), "i did not open the composer", seconds=60)
    part_seconds = []
    for part in range(10):
        mark = f"<<{part}>>"
        started = time.monotonic()
        remove_tui.send(("paste0123 " * 199 + "paste012 ").encode())
        remove_tui.send(mark.encode())
        # ^ A wait that fails says whether the TUI is alive and what it shows: the CI log of a
        # piped run carries no timing, and a hang and a slow redraw look alike without them.
        wait_for(lambda: mark in remove_tui.text(),
                 lambda: f"the composer did not show the end of part {part} of a 20 KB paste "
                         f"(TUI exit status {remove_tui.proc.poll()}, {time.monotonic() - started:.0f} s):\n{remove_tui.text()}",
                 seconds=180)
        part_seconds.append(time.monotonic() - started)
        print(f"smoke: part {part} of the 20 KB line shown after {part_seconds[-1]:.1f} s", flush=True)
    print("smoke: 20 KB typed in the composer, seconds per 2,000 characters: " + " ".join(f"{sec:.1f}" for sec in part_seconds), flush=True)
    assert part_seconds[-1] < 3 * part_seconds[0] + 2, part_seconds
    # ^ Two Escs in one write read as Alt-Esc: the first leaves the composer, the second the chat.
    remove_tui.send(b"\x1b")
    wait_for(lambda: "Enter: send" not in remove_tui.text(), "Esc did not leave the composer", seconds=60)
    remove_tui.send(b"\x1b")
    wait_for(lambda: "All projects" in remove_tui.text(), "Esc did not close the manager chat", seconds=60)
    remove_tui.send(b"q")
    wait_for(lambda: "All projects" not in remove_tui.text(), "q did not close halt-me's Tasks view", seconds=60)
    remove_from_tui("halt-me", b"H")
    wait_for(lambda: "halt-me" not in config_names(), "H did not unregister halt-me")
    wait_for(lambda: project_sessions("halt-me") == [], "H did not halt the sessions of halt-me")
    assert sorted(project_sessions("keep-me")) == ["shell", "task-manager"], "H halted another project's sessions"
    remove_from_tui("keep-me", b"y")
    wait_for(lambda: "keep-me" not in config_names(), "y did not unregister keep-me")
    time.sleep(2)
    assert sorted(project_sessions("keep-me")) == ["shell", "task-manager"], "y halted the sessions of keep-me"
    assert (removal["halt-me"] / ".git").is_dir() and (removal["keep-me"] / ".git").is_dir()
finally:
    remove_tui.close()
shutdown(cursor_daemon)

# docs/panels.md: a project's panels run in wsxd; the clients only ask. A real project config
# with panel scripts, the daemon's one runner and cache, then the TUI over a PTY.
panel_daemon = start()
BAD = WORK / "bad-panel"
subprocess.run(["git", "init", "-q", "-b", "main", str(BAD)], check=True, env=env)
(BAD / "wsx.config.yml").write_text("panels:\n  - name: Bad Name\n    command: [true]\n")
BADSCOPE = WORK / "bad-scope"
subprocess.run(["git", "init", "-q", "-b", "main", str(BADSCOPE)], check=True, env=env)
(BADSCOPE / "wsx.config.yml").write_text("panels:\n  - name: mac\n    command: [true]\n    scope: world\n")
PANEL_SCRIPTS = PROJECT / "scripts"
PANEL_SCRIPTS.mkdir()
# ^ The `format: json` panels print these files (docs/panels.md "JSON panels"): every kind, an
# unknown field (ignored), a bad level, an unknown kind and a gauge with total 0 (each skipped
# with a notice); `limits` is over every limit.
PANEL_DOC = {
    "title": "This Mac",
    "future": 1,
    "sections": [
        {"title": "Memory", "items": [
            {"kind": "gauge", "label": "Memory", "used": 13.6, "total": 16, "unit": "GB", "level": "warn", "future": True},
            {"kind": "gauge", "label": "Swap", "used": 0, "total": 4, "unit": "GB", "level": "ok"},
            {"kind": "stat", "label": "Pressure", "value": "normal", "note": "vm_stat", "level": "ok"},
            {"kind": "stat", "label": "Disk", "value": "full", "level": "red"},
        ]},
        {"title": "Storage", "items": [
            {"kind": "table", "columns": ["Volume", "Used", "Free"], "rows": [["Macintosh HD", "412 GB", "88 GB"], ["Data", 12, 3]]},
            {"kind": "list", "items": ["Time Machine", "Spotlight idle"], "level": "bad"},
            {"kind": "chart", "label": "x"},
            {"kind": "gauge", "label": "Zero", "used": 1, "total": 0},
        ]},
    ],
}
PANEL_LIMITS = {"sections": [{"title": "s0", "items": [
    {"kind": "table", "columns": ["n"], "rows": [[r] for r in range(101)]},
    {"kind": "stat", "label": "x" * 300, "value": "v"}] + [{"kind": "stat", "label": "a", "value": "v"}] * 50}
] + [{"title": f"s{n}", "items": [{"kind": "list", "items": ["a"]}]} for n in range(1, 13)]}
for name, body in {"json.json": PANEL_DOC, "limits.json": PANEL_LIMITS}.items():
    (PANEL_SCRIPTS / name).write_text(json.dumps(body))
for script, body in {
    "device-status.sh": "echo x >> ok.log\nprintf '\\033[32mdevice-ok\\033[0m\\r\\n'\npwd -P\n",
    "fail.sh": "echo boom\necho oops >&2\nexit 3\n",
    "big.sh": "i=0\nwhile [ $i -lt 3000 ]; do echo \"line $i padding padding padding\"; i=$((i+1)); done\n",
    "slow.sh": "echo $$ > slow.pid\necho started\nsleep 30\n",
    "tick.sh": "echo x >> tick.log\nsleep 1\necho tick\n",
    "json.sh": "cat scripts/json.json\n",
    "badjson.sh": "echo '{oops'\n",
    "limits.sh": "cat scripts/limits.json\n",
    "machine.sh": "echo machine-ok\n",
}.items():
    (PANEL_SCRIPTS / script).write_text("#!/bin/sh\n" + body)
    (PANEL_SCRIPTS / script).chmod(0o755)
PANEL_CONFIG = (
    "panels:\n"
    "  - name: ok\n    command: [scripts/device-status.sh]\n"
    "  - name: fail\n    command: [scripts/fail.sh]\n"
    "  - name: json\n    command: [scripts/json.sh]\n    format: json\n"
    "  - name: badjson\n    command: [scripts/badjson.sh]\n    format: json\n"
    "  - name: big\n    command: [scripts/big.sh]\n"
    "  - name: slow\n    command: [scripts/slow.sh]\n    timeout: 2\n"
    "  - name: tick\n    command: [scripts/tick.sh]\n    refresh: 10\n"
    "  - name: limits\n    command: [scripts/limits.sh]\n    format: json\n"
    "  - name: machine\n    command: [scripts/machine.sh]\n    scope: machine\n"
)
(PROJECT / "wsx.config.yml").write_text(PANEL_CONFIG)
assert call("synchronize_projects", {"projects": [
    {"path": str(PROJECT), "name": "smoke", "worktrees": [{"path": str(PROJECT), "branch": "main"}]},
    {"path": str(BAD), "name": "bad", "worktrees": [{"path": str(BAD), "branch": "main"}]},
    {"path": str(BADSCOPE), "name": "badscope", "worktrees": [{"path": str(BADSCOPE), "branch": "main"}]},
]})["type"] == "ack"


def panel_call(project, action):
    answer = call("panel", {"project": str(project), "action": action})
    assert answer["type"] == "panel", answer
    return answer["data"]


def panel_get(name, run=False):
    answer = panel_call(PROJECT, {"action": "get", "name": name, "run": run})
    assert answer["kind"] == "panel", answer
    return answer["data"]


def panel_finished(name, run=True):
    panel_get(name, run)
    wait_for(lambda: panel_get(name)["run"]["status"] != "pending" and not panel_get(name)["running"],
             f"the panel {name} did not finish", 15)
    return panel_get(name)


def log_lines(name):
    path = PROJECT / name
    return len(path.read_text().splitlines()) if path.exists() else 0


listed = panel_call(PROJECT, {"action": "list"})
assert [item["name"] for item in listed["data"]["panels"]] == ["ok", "fail", "json", "badjson", "big", "slow", "tick", "limits", "machine"], listed
# ^ The scope is in the list: only `machine` is machine-scoped (docs/panels.md "Machine panels").
assert {item["name"]: item["scope"] for item in listed["data"]["panels"]}["machine"] == "machine", listed
assert [item["name"] for item in listed["data"]["panels"] if item["scope"] == "machine"] == ["machine"], listed
# wsxd's built-in machine panel: no project and no command, this machine's own numbers, with the
# same cache rules as a project's panel (docs/panels.md "Machine panels").
def builtin_get(name, run=False):
    answer = call("panel", {"project": None, "action": {"action": "get", "name": name, "run": run}})
    assert answer["type"] == "panel", answer
    return answer["data"]


assert call("panel", {"project": None, "action": {"action": "list"}})["data"] == {"kind": "panels", "data": {
    "panels": [{"name": "this-mac", "refresh": 60, "scope": "machine"}], "notice": None}}
assert builtin_get("nope") == {"kind": "error", "data": 'wsxd has no panel "nope"'}
builtin_get("this-mac", run=True)
wait_for(lambda: builtin_get("this-mac")["data"]["run"]["status"] == "ok", "the built-in panel did not finish", 15)
mac = builtin_get("this-mac")["data"]
mac_doc = mac["run"]["doc"]
mac_items = {section["title"]: {item["label"]: item for item in section["items"]} for section in mac_doc["sections"]}
assert mac["refresh"] == 60 and json.loads(mac["run"]["output"]) == mac_doc, mac
load = mac_items["Load"]["Load average"]
assert len(load["value"].split(", ")) == 3 and load["note"] == f"1, 5, 15 min, {os.cpu_count()} cores", load
# ^ The boot volume, against the file system's own numbers.
root = os.statvfs("/")
boot = mac_doc["sections"][-1]["items"][0]
assert mac_doc["sections"][-1]["title"] == "Disks" and abs(boot["total"] - root.f_blocks * root.f_frsize / 1e9) < 0.2, (boot, root)
assert 0 < boot["used"] <= boot["total"] and boot["unit"] == "GB" and boot["level"] in ("ok", "warn", "bad"), boot
if sys.platform == "darwin":
    assert mac_doc["title"] == "This Mac" and list(mac_items) == ["Memory", "Load", "Disks"], mac_doc
    memsize = int(subprocess.run(["sysctl", "-n", "hw.memsize"], check=True, capture_output=True, text=True).stdout)
    memory = mac_items["Memory"]["Memory"]
    assert memory["total"] == round(memsize / 2**30, 1) and 0 < memory["used"] <= memory["total"], (memory, memsize)
    pressure = mac_items["Memory"]["Pressure"]
    assert {"normal": "ok", "warning": "warn", "critical": "bad"}[pressure["value"]] == pressure["level"], pressure
    assert mac_items["Memory"]["Compressed"]["value"].endswith(" GB") and "Swap" in mac_items["Memory"], mac_items
else:
    assert mac_doc["title"] == "This machine" and list(mac_items) == ["Load", "Disks"], mac_doc
# ^ A read within `refresh` is the cached result, not a new run.
again = builtin_get("this-mac")["data"]
assert again["run"]["finished_unix_ms"] == mac["run"]["finished_unix_ms"] and not again["running"], again
# Exit 0: the output as written, ANSI and CR stripped, in the main checkout, found by its relative path.
shown = panel_finished("ok")
assert shown["run"]["status"] == "ok" and shown["run"]["exit"] == 0, shown
assert shown["run"]["output"] == f"device-ok\n{os.path.realpath(PROJECT)}\n", shown["run"]["output"]
# A non-zero exit: the output and what the command wrote to stderr, with the exit code.
failed = panel_finished("fail")
assert failed["run"]["status"] == "failed" and failed["run"]["exit"] == 3, failed
assert "boom" in failed["run"]["output"] and "oops" in failed["run"]["output"], failed
# `format: json`: the typed document, an unknown field ignored, what cannot be drawn skipped with a notice.
doc_run = panel_finished("json")["run"]
doc = doc_run["doc"]
assert doc_run["status"] == "ok" and doc["title"] == "This Mac" and [s["title"] for s in doc["sections"]] == ["Memory", "Storage"], doc_run
assert [[i["kind"] for i in s["items"]] for s in doc["sections"]] == [["gauge", "gauge", "stat"], ["table", "list"]], doc
assert doc["sections"][0]["items"][0] == {"kind": "gauge", "label": "Memory", "used": 13.6, "total": 16.0, "unit": "GB", "level": "warn"}, doc
assert doc["sections"][1]["items"][0]["rows"] == [["Macintosh HD", "412 GB", "88 GB"], ["Data", "12", "3"]], doc
notes = doc_run["notice"].splitlines()
assert len(notes) == 3 and notes[0].startswith("section 1, item 4:") and "`red`" in notes[0], notes
assert notes[1].startswith("section 2, item 3:") and "`chart`" in notes[1] and notes[2].startswith("section 2, item 4:"), notes
assert json.loads(doc_run["output"])["title"] == "This Mac", "the raw text stays in output"
# Not valid JSON: no document, the text as written and a notice, so a broken script still shows something.
bad_run = panel_finished("badjson")["run"]
assert bad_run["status"] == "ok" and bad_run["doc"] is None and bad_run["output"] == "{oops\n", bad_run
assert bad_run["notice"].startswith("not a panel document:"), bad_run
# Over every limit: 12 sections, 50 items, 100 rows, strings of 200 characters.
limits_run = panel_finished("limits")["run"]
limits = limits_run["doc"]
assert len(limits["sections"]) == 12 and len(limits["sections"][0]["items"]) == 50, limits_run["notice"]
assert len(limits["sections"][0]["items"][0]["rows"]) == 100, "rows are cut at 100"
label = limits["sections"][0]["items"][1]["label"]
assert len(label) == 200 and label.endswith("…"), label
assert limits_run["notice"] == "cut to the limits: sections, items, table rows", limits_run["notice"]
# Over 16 KiB: the last 16 KiB stay.
big = panel_finished("big")["run"]
assert len(big["output"].encode()) <= 16 * 1024 and big["truncated"] and "line 2999 " in big["output"], big["truncated"]
# A timeout (the panel's `timeout: 2`) stops the command and its children.
began = time.monotonic()
slow = panel_finished("slow")["run"]
assert slow["status"] == "timed_out" and slow["output"].endswith("stopped after 2 s"), slow
assert time.monotonic() - began < 10, "the timeout did not stop the command"
slow_pid = int((PROJECT / "slow.pid").read_text())
try:
    os.kill(slow_pid, 0)
    raise AssertionError("the timed out command still runs")
except ProcessLookupError:
    pass
# One run at a time: five opens while one runs make one run.
for _ in range(5):
    panel_get("tick", True)
panel_finished("tick", False)
assert log_lines("tick.log") == 1, f"five opens made {log_lines('tick.log')} runs"
# A reopen shows the last output at once, with its age, while the new run goes.
again = panel_get("ok", True)
assert again["run"]["status"] == "ok" and again["running"] and again["age_ms"] >= 0, again
panel_finished("ok", False)
assert log_lines("ok.log") == 2
# Nothing asks, so nothing runs: `tick` (refresh 10) stays at one run, and `ok` (refresh 0)
# does not run again when asked without `run`.
time.sleep(11)
assert log_lines("tick.log") == 1, "a panel ran while no client showed it"
assert not panel_get("ok")["running"] and log_lines("ok.log") == 2, "refresh 0 ran again"
# A client that shows it asks again after `refresh`: the cache comes at once, a run starts.
due = panel_get("tick")
assert due["running"] and due["run"]["status"] == "ok", due
panel_finished("tick", False)
assert log_lines("tick.log") == 2
assert not panel_get("tick")["running"], "a fresh result started a run"
# A project wsxd does not manage runs nothing; a bad panel in a config shows as its notice.
assert call("panel", {"project": str(WORK), "action": {"action": "list"}})["data"]["kind"] == "error"
bad = panel_call(BAD, {"action": "list"})["data"]
assert bad["panels"] == [] and "panels[0].name" in bad["notice"], bad
bad_scope = panel_call(BADSCOPE, {"action": "list"})["data"]
assert bad_scope["panels"] == [] and "panels[0].scope" in bad_scope["notice"], bad_scope
print("panels in wsxd: PASS", flush=True)


def tui_over_pty(registry):
    # ^ The TUI reads this registry, and its screen is the bytes it wrote.
    (CONFIG_DIR / "config-v2.toml").write_text(
        "resume_agents_on_restore = false\n" + "".join(
            f'[[projects]]\nname = "{name}"\npath = {json.dumps(str(path))}\n' for name, path in registry))
    return smoke_tui()


# The TUI shows a bad panel where it shows every config problem: the project notice.
bad_tui = tui_over_pty([("bad", BAD)])
try:
    # ^ The toast is 48 columns by 3 lines: it shows the start; wsxd's list above proves the cause text.
    wait_for(lambda: b"Could not parse" in bad_tui.raw, "the TUI did not show the bad panel's notice", 20)
finally:
    bad_tui.close()

# `v` on the project opens its panels; Tab shows the next; Esc goes back.
panel_tui = tui_over_pty([("smoke", PROJECT)])
try:
    wait_for(lambda: b"WORKSPACE" in panel_tui.raw, "the TUI did not start", 20)
    panel_tui.send(b"v")
    # ^ ratatui writes only changed cells: a plain space over a blank cell is skipped, so each
    # asserted string is one word or one run in a single style.
    try:
        wait_for(lambda: b"smoke panels" in panel_tui.raw and b"device-ok" in panel_tui.raw,
                 "the TUI did not show the panel's output", 20)
    except AssertionError:
        print("screen:", panel_tui.text())
        raise
    wait_for(lambda: b"updated" in panel_tui.raw and b"ago" in panel_tui.raw and b"open" in panel_tui.raw,
             "the TUI did not show the state with its age")
    wait_for(lambda: b"scroll" in panel_tui.raw and b"next" in panel_tui.raw and b"close" in panel_tui.raw,
             "the TUI did not show the panels' key hints")
    assert b"\x1b[32mdevice" not in panel_tui.raw.split(b"smoke panels", 1)[1], "ANSI reached the screen"
    # ^ The capture of this screen for the review: its bytes drawn by the web client's xterm.js.
    CAPTURES = ROOT / ".work" / "web-captures"
    (WORK / "panels-tui.bytes").write_bytes(bytes(panel_tui.raw))
    if shutil.which("node"):
        drawn = subprocess.run(["node", str(ROOT / "scripts/render-ansi.mjs"), str(WORK / "panels-tui.bytes"), str(CAPTURES),
                                "panels-tui", "120", "40"], capture_output=True, text=True, timeout=90)
        print("panels TUI capture:", "written" if drawn.returncode == 0 else "not drawn: " + drawn.stderr.strip()[-300:], flush=True)
    panel_tui.send(b"\t")
    wait_for(lambda: b"failed" in panel_tui.raw and b"(exit" in panel_tui.raw and b"boom" in panel_tui.raw,
             "the TUI did not show the failing panel with its exit code", 20)
    # A `format: json` panel is drawn: boxed sections, a bar with its percent and the level word.
    panel_tui.send(b"\t")
    wait_for(lambda: all(word in panel_tui.raw for word in (b"Memory", b"85%", b"warn", b"Macintosh", "┌".encode(), "█".encode())),
             "the TUI did not draw the json panel", 20)
    wait_for(lambda: b"chart" in panel_tui.raw, "the TUI did not show the skipped item's notice")
    (WORK / "panels-tui-json.bytes").write_bytes(bytes(panel_tui.raw))
    if shutil.which("node"):
        drawn = subprocess.run(["node", str(ROOT / "scripts/render-ansi.mjs"), str(WORK / "panels-tui-json.bytes"), str(CAPTURES),
                                "panels-tui-json", "120", "40"], capture_output=True, text=True, timeout=90)
        print("panels TUI json capture:", "written" if drawn.returncode == 0 else "not drawn: " + drawn.stderr.strip()[-300:], flush=True)
    # Not valid JSON: the text as written and the notice.
    panel_tui.send(b"\t")
    wait_for(lambda: b"oops" in panel_tui.raw and b"document:" in panel_tui.raw,
             "the TUI did not show the broken json panel's text and notice", 20)
    panel_tui.send(b"\x1b")
    wait_for(lambda: panel_tui.raw.rfind(b"WORKSPACE") > panel_tui.raw.rfind(b"PANELS"), "Esc did not leave the panels")
    # `M` with no project declaring a machine panel: wsxd's built-in panel alone, drawn as a document.
    (PROJECT / "wsx.config.yml").write_text(PANEL_CONFIG.replace("  - name: machine\n    command: [scripts/machine.sh]\n    scope: machine\n", ""))
    mark = len(panel_tui.raw)
    panel_tui.send(b"M")
    wait_for(lambda: all(word in panel_tui.raw[mark:] for word in (b"Machine", b"this-mac", b"Load", b"Disks", "█".encode())),
             "M did not draw the built-in machine panel", 20)
    assert b"smoke \xc2\xb7" not in panel_tui.raw[mark:], "M showed a project with no machine panel"
    panel_tui.send(b"\x1b")
    wait_for(lambda: panel_tui.raw.rfind(b"WORKSPACE") > panel_tui.raw.rfind(b"PANELS"), "Esc did not leave the machine panels")
    # With machine panels (docs/panels.md "Machine panels"): one card, This Mac first, then the project's
    # sections headed `smoke · ...`: a document, a text panel and a failing one as one line; no project
    # panel of scope `project` is in it. The card's state is the worst level of all items.
    (PANEL_SCRIPTS / "machine.json").write_text(json.dumps({"title": "Pizza", "sections": [
        {"title": "Swap holders", "items": [{"kind": "list", "items": ["Slack 1.2 GB"], "level": "warn"}]}]}))
    (PANEL_SCRIPTS / "machine-doc.sh").write_text("#!/bin/sh\ncat scripts/machine.json\n")
    (PANEL_SCRIPTS / "machine-doc.sh").chmod(0o755)
    (PROJECT / "wsx.config.yml").write_text(PANEL_CONFIG
        + "  - name: machine-doc\n    command: [scripts/machine-doc.sh]\n    format: json\n    scope: machine\n"
        + "  - name: machine-down\n    command: [scripts/fail.sh]\n    scope: machine\n")
    mark = len(panel_tui.raw)
    panel_tui.send(b"M")
    wait_for(lambda: b"this-mac" in panel_tui.raw[mark:] and b"Load" in panel_tui.raw[mark:], "M did not draw the card", 20)
    # ^ The TUI writes only the cells that changed, so a line is not whole in the bytes: the text is read from the screen they make.
    wait_for(lambda: all(line in panel_tui.text() for line in (
        "smoke · Swap holders", "Slack 1.2 GB", "smoke · machine", "machine-ok")),
        lambda: "the machine card did not show the project's sections:\n" + panel_tui.text(), 20)
    card = panel_tui.text()
    assert re.search(r"This (Mac|machine) +bad", card), "the card title shows no state\n" + card
    # ^ The card is taller than the PTY: scroll to the failing panel's line.
    panel_tui.send(b"j" * 20)
    wait_for(lambda: "machine-down: failed (exit 3)" in panel_tui.text(),
             lambda: "the machine card did not show the failing panel's line:\n" + panel_tui.text(), 20)
    for text in (card, panel_tui.text()):
        assert "device-ok" not in text, "a project panel showed in the machine view"
        assert "Pizza" not in text, "the project panel's own title showed in the card"
    panel_tui.send(b"\x1b")
    wait_for(lambda: panel_tui.raw.rfind(b"WORKSPACE") > panel_tui.raw.rfind(b"PANELS"), "Esc did not leave the machine panels")
finally:
    panel_tui.close()
(CONFIG_DIR / "config-v2.toml").write_text(
    "resume_agents_on_restore = false\n"
    "[[projects]]\n"
    "name = \"smoke\"\n"
    f"path = {json.dumps(str(PROJECT))}\n"
)
shutil.rmtree(PANEL_SCRIPTS)
for leftover in ("wsx.config.yml", "ok.log", "tick.log", "slow.pid"):
    (PROJECT / leftover).unlink(missing_ok=True)
shutdown(panel_daemon)
print("panels in the TUI: PASS", flush=True)

# docs/daemon-live-upgrade.md: an install replaces wsx and wsxd under a running daemon with
# live sessions. The source is a released build when WSX_SMOKE_RELEASED_DIR names a folder
# with its wsx and wsxd (for example v0.31.0), else a copy of this build.
RELEASED = Path(os.environ["WSX_SMOKE_RELEASED_DIR"]) if os.environ.get("WSX_SMOKE_RELEASED_DIR") else None
INSTALL = WORK / "install"
# ^ The installed wsx finds the wsxd beside it, as on a real machine.
install_env = {key: value for key, value in env.items()
               if key not in ("WSX_PANE_ID", "WSX_DAEMON_BIN", "WSX_SOCKET")}
LIFECYCLE = SOCKET.with_suffix(".lifecycle")


def install(version, wsx_binary, wsxd_binary=None, wsxd_script=None):
    """Install as a package manager does: a new keg, both names relinked, the old keg gone."""
    keg = INSTALL / "Cellar" / "wsx" / version / "bin"
    keg.mkdir(parents=True)
    shutil.copy2(wsx_binary, keg / "wsx")
    if wsxd_script:
        # A wsxd that wraps this build's: `{real}` names the copy beside it.
        shutil.copy2(DAEMON, keg / "wsxd.real")
        (keg / "wsxd").write_text(wsxd_script.format(real=keg / "wsxd.real"))
        (keg / "wsxd").chmod(0o755)
    else:
        shutil.copy2(wsxd_binary, keg / "wsxd")
    (INSTALL / "bin").mkdir(exist_ok=True)
    for name in ("wsx", "wsxd"):
        staged = INSTALL / "bin" / (name + ".new")
        staged.symlink_to(Path("..") / "Cellar" / "wsx" / version / "bin" / name)
        os.replace(staged, INSTALL / "bin" / name)
    for kept in (INSTALL / "Cellar" / "wsx").iterdir():
        if kept.name != version:
            shutil.rmtree(kept)


def installed(*args, timeout=120):
    return subprocess.run([str(INSTALL / "bin" / "wsx"), *args], cwd=PROJECT, env=install_env,
                          capture_output=True, text=True, timeout=timeout)


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def lifecycle():
    return call("lifecycle_status")["data"]


def upgrade_session(label):
    """A shell that records its PID and writes each line it reads, so input proves it works."""
    pid_file, line_file = WORK / f"{label}.pid", WORK / f"{label}.line"
    command = (f"echo $$ > {pid_file}; "
               f"while IFS= read -r line; do printf '%s\\n' \"$line\" > {line_file}; done")
    created = call("session_create", {"worktree_id": call("snapshot")["data"]["worktrees"][0]["id"],
                                      "label": label, "command": ["/bin/sh", "-c", command],
                                      "rows": 24, "cols": 80})
    assert created["type"] == "created", created
    wait_for(lambda: pid_file.exists() and pid_file.read_text().strip(), f"{label} did not start")
    pane = next(pane["id"] for pane in call("snapshot")["data"]["panes"]
                if pane["session_id"] == created["data"]["id"])
    return int(pid_file.read_text()), pane, line_file


def sessions_answer(sessions, word):
    for pid, pane, line_file in sessions:
        assert alive(pid), f"session {pid} ended"
        send_shell_command(pane, word, 9300 + pane)
        wait_for(lambda: line_file.exists() and line_file.read_text().strip() == word,
                 f"session {pid} did not take {word}")


install("1-source", RELEASED / "wsx" if RELEASED else WSX, RELEASED / "wsxd" if RELEASED else DAEMON)
wait_singleton_released()
assert installed("session", "list").returncode == 0, "the installed wsx did not start its wsxd"
assert call("synchronize_projects", {"projects": [{
    "path": str(PROJECT), "name": "smoke", "worktrees": [{"path": str(PROJECT), "branch": "main"}],
}]})["type"] == "ack"
upgrade_sessions = [upgrade_session(label) for label in ("upgrade-a", "upgrade-b")]
source = lifecycle()

# 1. The install replaces the binaries and removes the running daemon's keg. A release of an
# older revision upgrades by the first command; a copy of this build by a restart.
install("2-upgrade", WSX, DAEMON)
upgraded = installed("session", "list") if RELEASED else installed("daemon", "restart")
assert upgraded.returncode == 0, upgraded
if RELEASED:
    assert "Runtime upgraded" in upgraded.stderr, upgraded.stderr
successor = lifecycle()
assert successor["epoch"] != source["epoch"] and successor["phase"] == "ready", (source, successor)
assert successor["daemon_revision"] >= source["daemon_revision"], (source, successor)
sessions_answer(upgrade_sessions, "after-upgrade")

# 2. A successor that ends after the commit, before it reports `owned`: the source sees it
# exit, takes its sessions back and keeps serving (docs/daemon-live-upgrade.md
# "A successor that dies").
install("3-dies", WSX, wsxd_script='#!/bin/sh\nWSX_HANDOFF_FAULT=after_commit exec "{real}" "$@"\n')
died = installed("daemon", "restart")
assert died.returncode != 0, died
assert "kept running" in died.stderr and "no session was stopped" in died.stderr, died.stderr
assert "exited before it took the sessions" in died.stderr, died.stderr
kept = lifecycle()
assert kept["epoch"] == successor["epoch"] and kept["phase"] == "ready", kept
assert kept["last_handoff_error"]["step"] == "owned", kept
sessions_answer(upgrade_sessions, "after-dead-successor")
assert installed("session", "list").returncode == 0

# 3. A successor that cannot bind the socket, and a rollback that cannot bind it either: the
# rollback still ends the handoff, and the daemon binds the socket again when it can
# (docs/daemon-live-upgrade.md "A stuck handoff").
install("4-blocked", WSX, DAEMON)
blocker = SOCKET.with_name(SOCKET.name + "~")
blocker.mkdir()
(blocker / "keep").write_text("")
blocked = subprocess.Popen([str(INSTALL / "bin" / "wsx"), "daemon", "restart"], cwd=PROJECT,
                           env=install_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
wait_for(lambda: not SOCKET.exists(), "the source did not give its socket up", seconds=20)
time.sleep(2)
assert not SOCKET.exists(), "a socket was bound through the blocked path"
shutil.rmtree(blocker)
wait_for(lambda: SOCKET.exists(), "the source did not bind its socket again", seconds=10)
_, blocked_err = blocked.communicate(timeout=60)
assert blocked.returncode != 0 and "no session was stopped" in blocked_err, blocked_err
rebound = lifecycle()
assert rebound["epoch"] == successor["epoch"] and rebound["phase"] == "ready", rebound
assert "bind the socket again" in rebound["last_handoff_error"]["error"], rebound
sessions_answer(upgrade_sessions, "after-rebind")
stopped = installed("daemon", "stop", "--force")
assert stopped.returncode == 0, stopped
wait_socket_stopped()

# 4. A daemon before revision 35 that never left its handoff answers only Hello and
# LifecycleStatus. `wsx daemon stop --force` ends its process, and the next command starts a
# new wsxd; a plain stop, and a stop during a fresh handoff, leave it running
# (docs/daemon-live-upgrade.md "A stuck handoff"). The stand-in also ignores SIGTERM.
STUCK_WSXD = r'''
import json, os, signal, socket, sys, threading, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
started = int(time.time() * 1000)
path = sys.argv[1]
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(path)
os.chmod(path, 0o600)
listener.listen(16)
lifecycle = {"protocol": 16, "epoch": 31, "binary_id": "0.31.0:1:2:3:4", "version": "0.31.0",
             "daemon_revision": 26, "started_unix_ms": started, "phase": "ready", "live_runtimes": 2,
             "active_clients": 1, "recovered_from_backup": False}
def serve(connection):
    with connection, connection.makefile("rwb") as stream:
        for line in stream:
            method = json.loads(line).get("method")
            if method == "hello":
                answer = {"type": "hello", "data": {"protocol": 16, "epoch": 31}}
            elif method == "lifecycle_status":
                answer = {"type": "lifecycle", "data": lifecycle}
            else:
                answer = {"type": "error", "data": {"code": "daemon_replacing",
                                                    "message": "wsxd live handoff is in progress"}}
            stream.write(json.dumps(answer).encode() + b"\n")
            stream.flush()
while True:
    threading.Thread(target=serve, args=(listener.accept()[0],), daemon=True).start()
'''


def write_marker(reason):
    LIFECYCLE.unlink(missing_ok=True)
    with os.fdopen(os.open(LIFECYCLE, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as marker:
        marker.write(reason + "\n")


stuck_script = WORK / "stuck-wsxd.py"
stuck_script.write_text(STUCK_WSXD)
SOCKET.unlink(missing_ok=True)
write_marker(f"handoff:{int(time.time() * 1000) - 14 * 3600 * 1000}")
stand_in = subprocess.Popen([sys.executable, "-I", str(stuck_script), str(SOCKET)])
try:
    wait_for(lambda: SOCKET.exists(), "the stuck stand-in did not bind its socket")
    plain = installed("daemon", "stop")
    assert plain.returncode != 0 and stand_in.poll() is None, plain
    write_marker(f"handoff:{int(time.time() * 1000)}")
    fresh = installed("daemon", "stop", "--force")
    assert fresh.returncode != 0 and stand_in.poll() is None, fresh
    write_marker(f"handoff:{int(time.time() * 1000) - 14 * 3600 * 1000}")
    forced = installed("daemon", "stop", "--force")
    assert forced.returncode == 0, forced
    stand_in.wait(timeout=10)
    assert stand_in.returncode == -signal.SIGKILL, stand_in.returncode
    assert not SOCKET.exists(), "the stop left the stuck daemon's socket"
    assert LIFECYCLE.read_text().strip() == "intentional", LIFECYCLE.read_text()
finally:
    if stand_in.poll() is None:
        stand_in.kill()
    stand_in.wait(timeout=5)
assert installed("session", "list").returncode == 0, "no fresh wsxd started"
assert lifecycle()["version"] != "0.31.0", lifecycle()
stopped = installed("daemon", "stop", "--force")
assert stopped.returncode == 0, stopped
wait_socket_stopped()

# 5. A released daemon whose rollback fails stays in its handoff for ever: it cannot be
# repaired, so a client of this build must say so and name the next step instead of
# waiting (docs/daemon-live-upgrade.md "A stuck handoff").
if RELEASED:
    wait_singleton_released()
    stuck_daemon = subprocess.Popen([str(RELEASED / "wsxd")], env=install_env)
    try:
        wait_ready(stuck_daemon)
        released_epoch = lifecycle()["epoch"]
        install("5-unfit", WSX, wsxd_script="#!/bin/sh\nexit 3\n")
        attempt = subprocess.Popen([str(INSTALL / "bin" / "wsx"), "session", "list"], cwd=PROJECT,
                                   env=install_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wait_for(lambda: LIFECYCLE.read_text().startswith("handoff:"), "the handoff did not start")
        os.chmod(SOCKET.parent, 0o500)
        try:
            attempt.communicate(timeout=90)
        finally:
            os.chmod(SOCKET.parent, 0o700)
        assert call("snapshot")["data"]["code"] == "daemon_replacing", "the released daemon recovered"
        started = time.monotonic()
        refused = installed("session", "list")
        assert refused.returncode != 0 and time.monotonic() - started < 15, refused
        assert "did not finish" in refused.stderr and "wsx daemon stop --force" in refused.stderr, refused.stderr
        stuck_tui = smoke_tui(INSTALL / "bin" / "wsx", install_env)
        stuck_screen = stuck_tui.raw
        try:
            wait_for(lambda: all(word in re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", stuck_screen)
                                 for word in (b"finish", b"--force")),
                     "the TUI did not say the handoff did not finish", seconds=60)
        finally:
            stuck_tui.close()
        # The next step the notice names ends the released daemon, and a new one starts.
        install("6-fresh", WSX, DAEMON)
        forced = installed("daemon", "stop", "--force")
        assert forced.returncode == 0, forced
        stuck_daemon.wait(timeout=10)
        assert installed("session", "list").returncode == 0, "no fresh wsxd started"
        assert lifecycle()["epoch"] != released_epoch, lifecycle()
        assert installed("daemon", "stop", "--force").returncode == 0
        wait_socket_stopped()
    finally:
        stuck_daemon.kill()
        stuck_daemon.wait(timeout=5)
        for leftover in (SOCKET, LIFECYCLE):
            leftover.unlink(missing_ok=True)

latency_passes = 0
latency_failures = 0
for attempt in range(1, 4):
    print(f"terminal latency gate: attempt {attempt}/3", flush=True)
    result = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "terminal-latency.py")],
        cwd=ROOT,
        env=env | {"WSX_LATENCY_WORK": str(WORK / "terminal-latency")},
        check=False,
    )
    if result.returncode == 0:
        latency_passes += 1
        if latency_passes == 2:
            break
    else:
        latency_failures += 1
        if latency_failures == 2:
            raise RuntimeError("terminal latency gate failed two independent attempts")
# docs/daemon-discovery.md "Lifetime": a main and a routine wsxd exit when the
# WSX_EXIT_WITH_PID process ends or the directory they serve is deleted, so a killed or
# finished test leaves none behind.
LIFETIME = WORK / "lifetime"
shutil.rmtree(LIFETIME, ignore_errors=True)


def lifetime_daemons(root, owner=None):
    root.mkdir(mode=0o700, parents=True)
    extra = {"WSX_SOCKET": str(root / "wsx.sock")}
    extra |= {"WSX_EXIT_WITH_PID": str(owner)} if owner else {}
    base = {key: value for key, value in env.items() if key != "WSX_EXIT_WITH_PID"}
    daemons = [subprocess.Popen([str(DAEMON), *args], env=base | extra, stdin=subprocess.DEVNULL)
               for args in ([],)]
    wait_for(lambda: (root / "wsx.sock").exists(), "lifetime daemons did not start")
    return daemons


def assert_lifetime_ended(daemons, label):
    wait_for(lambda: all(daemon.poll() is not None for daemon in daemons), label)


owner = subprocess.Popen(["sleep", "600"])
with_owner = lifetime_daemons(LIFETIME / "owner", owner.pid)
time.sleep(2)
assert all(daemon.poll() is None for daemon in with_owner), "a lifetime daemon exited early"
owner.kill()
owner.wait()
assert_lifetime_ended(with_owner, "wsxd outlived its WSX_EXIT_WITH_PID process")
assert not (LIFETIME / "owner" / "wsx.sock").exists(), "wsxd kept its socket after its owner"
deleted = lifetime_daemons(LIFETIME / "deleted")
# ^ The daemons still write in it (a marker's temporary file): one pass can fail or leave a file.
wait_for(lambda: shutil.rmtree(LIFETIME / "deleted", ignore_errors=True) or not (LIFETIME / "deleted").exists(),
         "the lifetime directory stayed")
assert_lifetime_ended(deleted, "wsxd outlived its deleted directory")
invalid = subprocess.run([str(DAEMON)], env=env | {"WSX_EXIT_WITH_PID": "1",
                         "WSX_SOCKET": str(LIFETIME / "owner" / "invalid.sock")},
                         capture_output=True, text=True, timeout=10)
assert invalid.returncode != 0 and "WSX_EXIT_WITH_PID" in invalid.stderr, invalid
shutil.rmtree(LIFETIME)
print("daemon lifetime: PASS", flush=True)
print("wsxd runtime smoke: PASS")
