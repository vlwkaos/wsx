"""A wsxd client for the test scripts: one JSON line per message over the Unix socket
(crates/wsx-core/src/runtime/protocol.rs). Import it from a script in this folder."""
import json
from pathlib import Path
import re
import socket
import threading
import weakref

ROOT = Path(__file__).resolve().parents[1]
# ^ Read from the source, so a protocol change needs no script edit.
PROTOCOL = int(re.search(r"PROTOCOL_VERSION: u32 = (\d+);",
                         (ROOT / "crates/wsx-core/src/runtime/protocol.rs").read_text()).group(1))
HELLO = json.dumps({"method": "hello", "params": {"protocol": PROTOCOL}}).encode() + b"\n"
BUFFERS = weakref.WeakKeyDictionary()


def fixture(source):
    """A fixture program's source with `PROTOCOL` set: it runs on its own and cannot import this."""
    return source.replace("@PROTOCOL@", str(PROTOCOL))


def send(client, method, params=None):
    request = {"method": method}
    if params is not None:
        request["params"] = params
    client.sendall(json.dumps(request).encode() + b"\n")


def recv_line(client):
    data = BUFFERS.get(client, b"")
    while b"\n" not in data:
        chunk = client.recv(65536)
        if not chunk:
            raise RuntimeError("wsxd closed response")
        data += chunk
        if len(data) > 32 * 1024 * 1024:
            raise RuntimeError("oversized response")
    line, remainder = data.split(b"\n", 1)
    BUFFERS[client] = remainder
    return json.loads(line)


def connect(path, timeout=5):
    """A connection that said hello. The caller closes it."""
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        client.settimeout(timeout)
        client.connect(str(path))
        client.sendall(HELLO)
        hello = recv_line(client)
        assert hello["type"] == "hello", hello
    except BaseException:
        client.close()
        raise
    return client


def call(path, method, params=None, timeout=5):
    """One request on its own connection, and its answer."""
    with connect(path, timeout) as client:
        send(client, method, params)
        return recv_line(client)


stream_send_lock = threading.Lock()


def send_stream(stream, message):
    with stream_send_lock:
        stream.sendall(json.dumps(message).encode() + b"\n")


def heartbeat_streams(streams, done):
    """Renew each stream's lease every second until `done`; a stream wsxd ended is skipped.

    Close a stream under `stream_send_lock`, so a heartbeat never reaches a reused descriptor."""
    while not done.wait(1):
        for stream in list(streams):
            try:
                send_stream(stream, {"type": "heartbeat"})
            except OSError:
                pass
