#!/usr/bin/env python3
import atexit
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
import weakref

ROOT = Path(__file__).resolve().parents[1]
PROTOCOL = 11
WORK = Path(os.environ.get("WSX_SMOKE_WORK", ROOT / ".work" / "runtime-smoke"))
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
    "while IFS= read -r line; do\n"
    "  case $line in\n"
    "    __WSX_REPORT__:*)\n"
    "      conversation=${line#__WSX_REPORT__:}\n"
    "      \"$WSX_AGENT_REPORT_BIN\" agent report \"$WSX_PANE_ID\" --provider codex "
    "--state working --conversation-id \"$conversation\" --prompt --resume --lifecycle\n"
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
(CONFIG_DIR / "config-v2.toml").write_text("resume_agents_on_restore = false\n")
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
env.update({
    "HOME": str(HOME),
    "XDG_STATE_HOME": str(STATE),
    "XDG_CONFIG_HOME": str(WORK / "config"),
    "XDG_CACHE_HOME": str(WORK / "cache"),
    "SHELL": "/bin/sh",
})
subprocess.run(["git", "init", "-q", "-b", "main", str(PROJECT)], check=True, env=env)
SOCKET = STATE / "wsx" / "wsx.sock"
DAEMON = Path(os.environ.get("WSX_SMOKE_DAEMON", ROOT / "target" / "debug" / "wsxd"))
WSX = Path(os.environ.get("WSX_SMOKE_WSX", ROOT / "target" / "debug" / "wsx"))


RECV_BUFFERS = weakref.WeakKeyDictionary()


def recv_line(client):
    data = RECV_BUFFERS.get(client, b"")
    while b"\n" not in data:
        chunk = client.recv(65536)
        if not chunk:
            raise RuntimeError("wsxd closed response")
        data += chunk
        if len(data) > 32 * 1024 * 1024:
            raise RuntimeError("oversized response")
    line, remainder = data.split(b"\n", 1)
    RECV_BUFFERS[client] = remainder
    return json.loads(line)


def raw_call(payload):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(str(SOCKET))
        client.sendall(payload)
        return recv_line(client)


def call(method, params=None):
    request = {"method": method}
    if params is not None:
        request["params"] = params
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(str(SOCKET))
        client.sendall(json.dumps({"method": "hello", "params": {"protocol": PROTOCOL}}).encode() + b"\n")
        hello = recv_line(client)
        assert hello["type"] == "hello", hello
        client.sendall(json.dumps(request).encode() + b"\n")
        return recv_line(client)


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


def start():
    wait_singleton_released()
    process = subprocess.Popen([str(DAEMON)], env=env)
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
    "initial_input": "sleep 30",
    "rows": 4,
    "cols": 20,
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
assert activity_snapshot is not None and any(
    item["pane_id"] == activity_pane_id and item["foreground_job"]
    for item in activity_snapshot.get("pane_activity", [])
), activity_snapshot
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
deadline = time.monotonic() + 6
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
assert project["last_agent_active_unix_ms"] is None, project
session = next(item for item in snapshot["sessions"] if item["id"] == session_id)
pane_id = session["focused_pane"]
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

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as terminal:
    terminal.settimeout(2)
    terminal.connect(str(SOCKET))
    terminal.sendall(json.dumps({"method": "hello", "params": {"protocol": PROTOCOL}}).encode() + b"\n")
    assert recv_line(terminal)["type"] == "hello"
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
    "command": ["/bin/sh", "-c", "sleep 0.2; printf '\\033]52;c;ZXhpdA==\\007'"],
    "rows": 2,
    "cols": 12,
})
assert exit_effect["type"] == "created", exit_effect
exit_session_id = exit_effect["data"]["id"]
exit_snapshot = call("snapshot")["data"]
exit_session = next(item for item in exit_snapshot["sessions"] if item["id"] == exit_session_id)
exit_pane_id = exit_session["focused_pane"]
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as terminal:
    terminal.settimeout(2)
    terminal.connect(str(SOCKET))
    terminal.sendall(json.dumps({"method": "hello", "params": {"protocol": PROTOCOL}}).encode() + b"\n")
    assert recv_line(terminal)["type"] == "hello"
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

deadline = time.monotonic() + 3
text = ""
while time.monotonic() < deadline:
    response = call("view", {"pane_ids": [pane_id]})
    assert response["type"] == "view", response
    frames = response["data"]["frames"]
    if frames:
        text = "".join(cell[0] for cell in frames[0]["cells"])
        if "wsx-smoke" in text:
            break
    time.sleep(0.05)
assert "wsx-smoke" in text, text

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
process.kill()
process.wait(timeout=5)
bootstrap_env = env | {"WSX_DAEMON_BIN": str(DAEMON)}
recovered = subprocess.run(
    [str(WSX), "status", "--json"],
    env=bootstrap_env,
    check=True,
    capture_output=True,
    text=True,
)
assert recovered.stdout, recovered
snapshot = call("snapshot")["data"]
assert next(item for item in snapshot["sessions"] if item["id"] == recovery_session_id)

stopped = subprocess.run(
    [str(WSX), "daemon", "stop"],
    env=env,
    check=True,
    capture_output=True,
    text=True,
)
assert "saved session commands restart on next launch" in stopped.stdout, stopped.stdout
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
shutdown(recovered_from_backup)

subprocess.run(
    [sys.executable, str(ROOT / "scripts" / "terminal-latency.py")],
    cwd=ROOT,
    env=env | {"WSX_LATENCY_WORK": str(WORK / "terminal-latency")},
    check=True,
)
print("wsxd runtime smoke: PASS")
