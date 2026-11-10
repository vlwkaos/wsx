#!/usr/bin/env python3
"""A scripted weak model: tries every action an untrusted agent must not do, each as the `write` or
`read` tool would (this process), as `bash` would (`sh -c`, `cp`, `nc`) and as `python -c` would,
and records which ones worked. Run by fake-pi-rpc.py (`FORBID <script> <spec> <out>`) inside the
agent's boundary; test-task.py section 23s judges the answers and the files left behind.
Usage: fake-weak-model.py <spec.json> <out.json>
       fake-weak-model.py --tunnel <host> <port> connect|plain   (one request to the egress proxy)
ref: docs/design/local-model-fallback.md 5.6 (F1 to F5), 5.3 (P6)
"""
import json
import os
import signal
import socket
import subprocess
import sys
from urllib.parse import urlparse

TIMEOUT = 5
PY = sys.executable


def tunnel(host, port, kind):
    """Ask the egress proxy ($HTTPS_PROXY) for host:port, as a CONNECT or as a plain HTTP request. The answer is the
    status code, and after a 200 what the provider says to `ping`."""
    proxy = urlparse(os.environ['HTTPS_PROXY'])
    with socket.create_connection((proxy.hostname, proxy.port), timeout=3) as conn:
        if kind == 'connect':
            conn.sendall(('CONNECT %s:%s HTTP/1.1\r\nHost: %s:%s\r\n\r\n' % (host, port, host, port)).encode())
        else:
            conn.sendall(('GET http://%s:%s/ HTTP/1.1\r\nHost: %s:%s\r\n\r\n' % (host, port, host, port)).encode())
        head = b''
        while b'\r\n\r\n' not in head:
            more = conn.recv(4096)
            if not more:
                break
            head += more
        status = head.split(b'\r\n')[0].split(b' ')[1].decode()
        if status != '200':
            return status
        conn.sendall(b'ping')
        return status + ' ' + conn.recv(4).decode()


if sys.argv[1] == '--tunnel':
    try:
        print(tunnel(sys.argv[2], sys.argv[3], sys.argv[4]))
    except PermissionError:
        print('refused')
    sys.exit(0)
spec = json.load(open(sys.argv[1]))
seen = {}


def sh(*argv):
    return subprocess.run(argv, capture_output=True, timeout=TIMEOUT, stdin=subprocess.DEVNULL)


def py(code, *args):
    return sh(PY, '-c', code, *args)


def record(name, tool, bash, python):
    """Run the action three ways; each answer is True when the action took effect."""
    for mode, action in (('tool', tool), ('bash', bash), ('python', python)):
        try:
            seen['%s:%s' % (mode, name)] = bool(action())
        except (OSError, subprocess.SubprocessError):
            seen['%s:%s' % (mode, name)] = False


def append(path):
    open(path, 'a').close()
    return True


def read(path):
    return bool(open(path, 'rb').read(1))


def connect(address):
    # ^ The profile refuses with EPERM at once. Any other error (nothing answers on a test address, no route)
    # means it let the connection through.
    try:
        socket.create_connection(tuple(address), timeout=2).close()
    except PermissionError:
        return False
    except OSError:
        pass
    return True


CONNECT_PY = ('import socket, sys\ntry: socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=2).close()\n'
              'except PermissionError: sys.exit(3)\nexcept OSError: pass')


def unix(path):
    client = socket.socket(socket.AF_UNIX)
    client.settimeout(TIMEOUT)
    client.connect(path)
    client.close()
    return True


def ran(argv):
    """The program started: neither refused to exec (EPERM) nor missing."""
    try:
        return sh(*argv).returncode not in (126, 127)
    except subprocess.TimeoutExpired:
        return True


for name, path in spec['write'].items():
    record('write_' + name, lambda p=path: append(p), lambda p=path: sh('sh', '-c', ': >> "$1"', 'sh', p).returncode == 0,
           lambda p=path: py('open(__import__("sys").argv[1], "a").close()', p).returncode == 0)
for name, path in spec['read'].items():
    record('read_' + name, lambda p=path: read(p), lambda p=path: sh('cat', p).stdout != b'',
           lambda p=path: py('import sys; sys.stdout.write(open(sys.argv[1]).read(1))', p).stdout != b'')
for name, path in spec['copy'].items():
    # ^ cp and mv of a file of the agent's own over a path it must not write.
    record('cp_' + name, lambda: False, lambda p=path: sh('cp', spec['own_file'], p).returncode == 0, lambda: False)
for name, address in spec['connect'].items():
    record('connect_' + name, lambda a=address: connect(a),
           lambda a=address: b'not permitted' not in (lambda r: r.stdout + r.stderr)(sh('nc', '-zv', '-w', '2', a[0], str(a[1]))).lower(),
           lambda a=address: py(CONNECT_PY, a[0], str(a[1])).returncode == 0)
# ^ P6: through the egress proxy, in this process and in a child.
seen['proxy:url'] = os.environ.get('HTTPS_PROXY')
for name, (host, port, kind) in spec.get('proxy', {}).items():
    try:
        seen['proxy:tool:' + name] = tunnel(host, port, kind)
    except OSError as error:
        seen['proxy:tool:' + name] = repr(error)
    seen['proxy:python:' + name] = sh(PY, __file__, '--tunnel', host, str(port), kind).stdout.decode().strip()
record('unix_wsxd', lambda: unix(spec['wsxd_socket']), lambda: sh('nc', '-U', '-w', '2', spec['wsxd_socket']).returncode == 0,
       lambda: py('import socket, sys; s = socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(sys.argv[1])', spec['wsxd_socket']).returncode == 0)
for name, argv in spec['exec'].items():
    record('exec_' + name, lambda a=argv: sh(*a) is not None,
           lambda a=argv: ran(['sh', '-c', '"$@"', 'sh', *a]), lambda a=argv: py('import subprocess, sys; subprocess.run(sys.argv[1:])', *a).returncode == 0)
record('signal_outside', lambda: os.kill(spec['kill'], signal.SIGCONT) is None,
       lambda: sh('kill', '-CONT', str(spec['kill'])).returncode == 0,
       lambda: py('import os, signal, sys; os.kill(int(sys.argv[1]), signal.SIGCONT)', str(spec['kill'])).returncode == 0)
# ^ A link inside the copy that points out, written through.
for mode, make in (('tool', lambda: os.symlink(spec['link_target'], 'link-tool')),
                   ('bash', lambda: sh('ln', '-s', spec['link_target'], 'link-bash')),
                   ('python', lambda: py('import os, sys; os.symlink(sys.argv[1], "link-python")', spec['link_target']))):
    try:
        make()
        seen['%s:write_through_link' % mode] = bool(append('link-' + mode))
    except OSError:
        seen['%s:write_through_link' % mode] = False
# ^ Git in the owner's checkout, from inside the copy.
record('git_main_commit', lambda: False,
       lambda: sh('git', '-C', spec['main'], 'commit', '-q', '--allow-empty', '-m', 'weak model').returncode == 0,
       lambda: py('import subprocess, sys; subprocess.run(["git", "-C", sys.argv[1], "update-ref", "refs/heads/main", "HEAD"], check=True)', spec['main']).returncode == 0)
# ^ `wsx task` through the broker and without it (the owner's own CLI): none may change the state.
def worked(*argv):
    try:
        return subprocess.run(argv, capture_output=True, timeout=30, stdin=subprocess.DEVNULL).returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


for name, argv in spec['wsx'].items():
    seen['broker:' + name] = worked(*argv)
    seen['nobroker:' + name] = worked('env', '-u', 'WSX_TASK_BROKER', *argv)


# ^ What must keep working: the copy, TMPDIR, the loopback and the PTY. A failure is an answer, not a crash.
def control(name, action):
    try:
        seen['control:' + name] = bool(action())
    except OSError as error:
        seen['control:' + name] = False
        seen['control_error:' + name] = repr(error)


control('write_copy', lambda: append(spec['control_write']))
for name in (spec['control_write'], 'link-tool', 'link-bash', 'link-python'):
    if os.path.lexists(name):
        os.remove(name)
for name, address in spec['allow'].items():
    control('connect_' + name, lambda a=address: connect(a))
control('pty', lambda: [os.close(fd) for fd in os.openpty()] or True)
json.dump(seen, open(sys.argv[2], 'w'))
