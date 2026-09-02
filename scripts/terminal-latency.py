#!/usr/bin/env python3
"""Gate direct PTY visibility against wsxd-stream-to-wsx-render visibility."""

import json
import math
import os
from pathlib import Path
import pty
import select
import shutil
import socket
import struct
import subprocess
import sys
import termios
import time
import fcntl
import weakref

ROOT = Path(__file__).resolve().parents[1]
WORK = ROOT / ".work" / "terminal-latency"
DAEMON = Path(os.environ.get("WSX_LATENCY_DAEMON", ROOT / "target" / "debug" / "wsxd"))
WSX = Path(os.environ.get("WSX_LATENCY_WSX", ROOT / "target" / "debug" / "wsx"))
PROJECT = WORK / "project"
HOME = WORK / "home"
STATE = WORK / "state"
SOCKET = STATE / "wsx" / "wsx.sock"
PROTOCOL = 10
WARMUPS = 4
SAMPLES = 20
BUDGET_MS = 16.7
BUFFERS = weakref.WeakKeyDictionary()


def recv_line(client):
    data = BUFFERS.get(client, b"")
    while b"\n" not in data:
        chunk = client.recv(65536)
        if not chunk:
            raise RuntimeError("wsxd closed response")
        data += chunk
    line, remainder = data.split(b"\n", 1)
    BUFFERS[client] = remainder
    return json.loads(line)


def call(method, params=None):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(str(SOCKET))
        client.sendall(json.dumps({"method": "hello", "params": {"protocol": PROTOCOL}}).encode() + b"\n")
        assert recv_line(client)["type"] == "hello"
        request = {"method": method}
        if params is not None:
            request["params"] = params
        client.sendall(json.dumps(request).encode() + b"\n")
        return recv_line(client)


def set_size(fd, rows=24, cols=80):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def drain(fd):
    while select.select([fd], [], [], 0)[0]:
        try:
            if not os.read(fd, 65536):
                break
        except OSError:
            break


def wait_marker(fd, marker, timeout):
    deadline = time.monotonic() + timeout
    data = b""
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], deadline - time.monotonic())
        if not ready:
            break
        chunk = os.read(fd, 65536)
        if not chunk:
            break
        data += chunk
        if marker in data:
            return
    raise RuntimeError(f"render marker not observed: {marker!r}; tail={data[-500:]!r}")


def percentile(values, probability):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * probability) - 1)]


def measure(fd, glyphs, rewrite=False):
    values = []
    for glyph in glyphs:
        if rewrite:
            os.write(fd, b"\x7f")
            time.sleep(0.02)
            drain(fd)
        marker = glyph.encode()
        started = time.perf_counter_ns()
        os.write(fd, marker)
        wait_marker(fd, marker, 1)
        values.append((time.perf_counter_ns() - started) / 1_000_000)
    return values[WARMUPS:]


def spawn_pty(argv, env):
    master, slave = pty.openpty()
    set_size(slave)
    process = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True)
    os.close(slave)
    return process, master


if WORK.exists():
    shutil.rmtree(WORK)
for path in (PROJECT, HOME, STATE):
    path.mkdir(parents=True, exist_ok=True)
subprocess.run(["git", "init", "-q", "-b", "main", str(PROJECT)], check=True)
config_dir = HOME / "Library" / "Application Support" / "wsx" if sys.platform == "darwin" else WORK / "config" / "wsx"
config_dir.mkdir(parents=True, exist_ok=True)
config = config_dir / "config-v2.toml"
config.write_text(
    "resume_agents_on_restore = false\n"
    "[[projects]]\n"
    "name = \"latency\"\n"
    f"path = {json.dumps(str(PROJECT))}\n"
)
config.chmod(0o600)
env = os.environ.copy()
env.update({
    "HOME": str(HOME),
    "XDG_STATE_HOME": str(STATE),
    "XDG_CONFIG_HOME": str(WORK / "config"),
    "XDG_CACHE_HOME": str(WORK / "cache"),
    "WSX_SOCKET": str(SOCKET),
    "WSX_DAEMON_BIN": str(DAEMON),
    "SHELL": "/bin/sh",
    "TERM": "xterm-256color",
    "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
})
daemon = None
ui = None
direct = None
ui_fd = None
direct_fd = None
try:
    daemon = subprocess.Popen(
        [str(DAEMON)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if SOCKET.exists():
            try:
                if call("snapshot")["type"] == "snapshot":
                    break
            except OSError:
                pass
        time.sleep(0.02)
    else:
        raise RuntimeError("wsxd readiness timeout")

    assert call("synchronize_projects", {"projects": [{
        "path": str(PROJECT),
        "name": "latency",
        "worktrees": [{"path": str(PROJECT), "branch": "main"}],
    }]})["type"] == "ack"
    snapshot = call("snapshot")["data"]
    worktree_id = snapshot["worktrees"][0]["id"]
    created = call("session_create", {
        "worktree_id": worktree_id,
        "label": "latency-session",
        "command": ["/bin/cat"],
        "rows": 22,
        "cols": 80,
    })
    assert created["type"] == "created", created

    direct, direct_fd = spawn_pty(["/bin/cat"], env)
    ui, ui_fd = spawn_pty([str(WSX), "--mobile"], env)
    wait_marker(ui_fd, b"latency-session", 8)
    os.write(ui_fd, b"jj\r")
    wait_marker(ui_fd, b"TERMINAL", 5)
    time.sleep(0.1)
    drain(ui_fd)
    drain(direct_fd)

    sample_count = WARMUPS + SAMPLES
    classes = {
        "narrow": [chr(0x3b1 + i) for i in range(sample_count)],
        "wide": [chr(0x4e00 + i) for i in range(sample_count)],
        "rewrite": [chr(0x4e40 + i) for i in range(sample_count)],
    }
    report = {}
    max_added_p95 = 0.0
    for name, glyphs in classes.items():
        rewrite = name == "rewrite"
        direct_ms = measure(direct_fd, glyphs, rewrite=rewrite)
        full_ms = measure(ui_fd, glyphs, rewrite=rewrite)
        direct_p50 = percentile(direct_ms, 0.50)
        direct_p95 = percentile(direct_ms, 0.95)
        full_p50 = percentile(full_ms, 0.50)
        full_p95 = percentile(full_ms, 0.95)
        added_p50 = max(0.0, full_p50 - direct_p50)
        added_p95 = max(0.0, full_p95 - direct_p95)
        max_added_p95 = max(max_added_p95, added_p95)
        report[name] = {
            "samples": SAMPLES,
            "direct_p50_ms": round(direct_p50, 3),
            "direct_p95_ms": round(direct_p95, 3),
            "full_p50_ms": round(full_p50, 3),
            "full_p95_ms": round(full_p95, 3),
            "added_p50_ms": round(added_p50, 3),
            "added_p95_ms": round(added_p95, 3),
        }
    report["path"] = "direct PTY vs wsxd stream + wsx apply/render + outer PTY"
    report["budget_ms"] = BUDGET_MS
    report["max_added_p95_ms"] = round(max_added_p95, 3)
    print(json.dumps(report, sort_keys=True))
    if max_added_p95 >= BUDGET_MS:
        raise RuntimeError(f"terminal added p95 {max_added_p95:.3f} ms exceeds {BUDGET_MS:.1f} ms")
finally:
    for process, fd in ((ui, ui_fd), (direct, direct_fd)):
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=2)
    if SOCKET.exists():
        try:
            call("shutdown")
        except Exception:
            if daemon is not None:
                daemon.terminate()
    if daemon is not None and daemon.poll() is None:
        try:
            daemon.wait(timeout=3)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=2)
    shutil.rmtree(WORK, ignore_errors=True)
