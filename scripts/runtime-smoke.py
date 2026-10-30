#!/usr/bin/env python3
import atexit
import fcntl
import json
import os
from pathlib import Path
import re
import resource
import shutil
import signal
import socket
import struct
import subprocess
import sys
import termios
import threading
import time

import wsxd_client
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
    raise AssertionError(label)


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


def frame_subscribe(pane_id, client_id, takeover=False):
    stream = wsxd_client.connect(SOCKET, 10)
    wsxd_client.send(stream, "terminal_subscribe", {
        "pane_id": pane_id, "client_id": client_id, "takeover": takeover, "rows": 6, "cols": 60,
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
# ^ A window that stops reading could hold wsxd for 5 s (its stream's write limit). A loaded CI
# runner took 1.55 s without one, so the limit separates the two and not the runner's speed.
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
assert next(item for item in snapshot["sessions"] if item["id"] == recovery_session_id)

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
tui_master, tui_slave = os.openpty()
fcntl.ioctl(tui_slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
tui = subprocess.Popen([str(WSX)], cwd=PROJECT, env={k: v for k, v in env.items() if k != "WSX_PANE_ID"},
                       stdin=tui_slave, stdout=tui_slave, stderr=tui_slave, close_fds=True)
os.close(tui_slave)
tui_screen = bytearray()


def read_tui():
    # ^ Read the TUI's output all the time: a full PTY blocks its draw, and then its keys.
    try:
        while chunk := os.read(tui_master, 65536):
            tui_screen.extend(chunk)
    except OSError:
        pass


threading.Thread(target=read_tui, daemon=True).start()


def in_terminal_mode():
    # ^ The mode line drawn last names the mode.
    return tui_screen.rfind(b"TERMINAL") > tui_screen.rfind(b"WORKSPACE")


try:
    # The last row is keep-c, the newest session.
    wait_for(lambda: b"keep-c" in tui_screen, "the TUI did not show keep-c")
    os.write(tui_master, b"j" * 12)
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
    drawn_from = len(tui_screen)
    time.sleep(5)
    draws = tui_screen[drawn_from:].count(b"\x1b[?2026h")
    print(f"tui refresh: connect median {connect_ms:.1f} ms, {draws} draws in 5 s of output")
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
        while answer not in tui_screen:
            assert time.monotonic() < deadline, f"the TUI does not show the viewed shell's answer {marker}"
            # ^ Keys typed outside Terminal mode are commands ('e' opens an editor).
            if in_terminal_mode():
                os.write(tui_master, f"echo {marker}-$((40+{value}))\r".encode())
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
    os.write(tui_master, b"\r")
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
    # ^ As in viewed_shell_answers: a key typed while the stream reattaches is dropped.
    exit_deadline = time.monotonic() + 20
    while b"Enter starts it again" not in tui_screen:
        assert time.monotonic() < exit_deadline, (
            "the TUI did not say the program exited", viewed_exited(), bytes(tui_screen[-600:]))
        if in_terminal_mode():
            os.write(tui_master, b"exit\r")
        time.sleep(3)
    assert viewed_exited()
    os.write(tui_master, b"\r")
    viewed_shell_answers("restarted", 3)
    assert not viewed_exited()
finally:
    tui.terminate()
    tui.wait(timeout=5)
    os.close(tui_master)
shutdown(cursor_daemon)

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
        stuck_master, stuck_slave = os.openpty()
        fcntl.ioctl(stuck_slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        stuck_tui = subprocess.Popen([str(INSTALL / "bin" / "wsx")], cwd=PROJECT, env=install_env,
                                     stdin=stuck_slave, stdout=stuck_slave, stderr=stuck_slave, close_fds=True)
        os.close(stuck_slave)
        stuck_screen = bytearray()

        def read_stuck_tui():
            try:
                while chunk := os.read(stuck_master, 65536):
                    stuck_screen.extend(chunk)
            except OSError:
                pass
        threading.Thread(target=read_stuck_tui, daemon=True).start()
        try:
            wait_for(lambda: all(word in re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", stuck_screen)
                                 for word in (b"finish", b"--force")),
                     "the TUI did not say the handoff did not finish", seconds=60)
        finally:
            stuck_tui.terminate()
            stuck_tui.wait(timeout=5)
            os.close(stuck_master)
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
    extra = {"WSX_SOCKET": str(root / "wsx.sock"), "ASCHED_ROOT": str(root / "asched")}
    extra |= {"WSX_EXIT_WITH_PID": str(owner)} if owner else {}
    base = {key: value for key, value in env.items() if key != "WSX_EXIT_WITH_PID"}
    daemons = [subprocess.Popen([str(DAEMON), *args], env=base | extra, stdin=subprocess.DEVNULL)
               for args in ([], ["__wsx_routine_daemon"])]
    wait_for(lambda: (root / "wsx.sock").exists() and (root / "asched" / "daemon-v1.sock").exists(),
             "lifetime daemons did not start")
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
assert not (LIFETIME / "owner" / "asched" / "daemon-v1.sock").exists(), "routine socket kept"
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
