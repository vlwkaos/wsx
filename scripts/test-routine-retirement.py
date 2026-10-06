"""Drive the rebuilt public routine CLI against bounded private retirement peers.

Use real singleton flock ownership and adjacent wsxd. Unread socket close may
produce EOF on macOS and reset on Linux; this does not claim Linux evidence.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    modes = ['unavailable', 'unread-close', 'held-timeout', 'drip-timeout', 'missing-lock', 'genuine-error', 'mutation-disconnect']
    parser.add_argument('--case', action='append', choices=modes)
    args = parser.parse_args()
    modes = args.case or modes
    wsx = args.wsx.resolve()
    wsxd = wsx.with_name('wsxd')
    assert wsx.is_file() and wsxd.is_file(), 'build adjacent companions first'
    ROOT.joinpath('.work').mkdir(exist_ok=True)
    scratch = ROOT / '.work' / ('rq-' + str(os.getpid()))
    scratch.mkdir(mode=0o700)
    results = []
    try:
        for mode in modes:
            results.append(run_case(wsx, wsxd, scratch, mode))
    finally:
        receipt = {'result': 'PASS' if len(results) == len(modes) else 'FAILED',
                   'cases': results, 'scratch': str(scratch), 'platform': os.uname().sysname,
                   'wsx_sha256': hashlib.sha256(wsx.read_bytes()).hexdigest(),
                   'wsxd_sha256': hashlib.sha256(wsxd.read_bytes()).hexdigest(),
                   'limits': 'Controlled private peer, real public CLI and singleton lock. Linux close/reset evidence requires Linux CI.',
                   'cleanup': 'Each completed case requires joined peers, absent sockets and exited replacement PIDs. Incomplete cases require inspection; scratch is retained.'}
        (scratch / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps(receipt, indent=2), flush=True)


def run_case(wsx, wsxd, scratch, mode):
    root = scratch / mode
    scheduler, home, project = root / 'a', root / 'h', root / 'p'
    for path in [scheduler, home, project]:
        path.mkdir(parents=True, mode=0o700)
    endpoint = scheduler / 'daemon-v1.sock'
    assert len(os.fsencode(endpoint)) < 100
    listener = socket.socket(socket.AF_UNIX)
    listener.settimeout(5)
    listener.bind(str(endpoint))
    endpoint.chmod(0o600)
    listener.listen()
    lock = None
    if mode != 'mutation-disconnect':
        lock = (scheduler / 'daemon-v1.lock').open('w+b')
        os.fchmod(lock.fileno(), 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    actions, faults = [], []
    released = threading.Event()

    def receive(connection):
        connection.settimeout(5)
        with connection.makefile('rb') as stream:
            body = stream.readline(4097)
        assert body.endswith(b'\n') and len(body) <= 4096
        request = json.loads(body)
        actions.append({'action': request['action']['action'], 'protocol': request['protocol']})
        return request

    def reply(connection, value):
        connection.sendall((json.dumps(value) + '\n').encode())

    def serve():
        try:
            with listener.accept()[0] as connection:
                receive(connection)
                assert actions[-1] == {'action': 'status', 'protocol': 4}
                if mode == 'mutation-disconnect':
                    reply(connection, {'result': 'daemon', 'protocol': 4, 'pid': os.getpid()})
                else:
                    reply(connection, {'result': 'error', 'kind': 'protocol_mismatch', 'message': 'legacy protocol'})
            if mode == 'mutation-disconnect':
                with listener.accept()[0] as connection:
                    receive(connection)
                    assert actions[-1]['action'] == 'fire'
                listener.settimeout(0.4)
                try:
                    with listener.accept()[0] as connection:
                        receive(connection)
                    raise AssertionError('uncertain mutation was replayed')
                except socket.timeout:
                    return
            with listener.accept()[0] as connection:
                receive(connection)
                assert actions[-1] == {'action': 'status', 'protocol': 3}
                reply(connection, {'result': 'daemon', 'protocol': 3, 'pid': os.getpid()})
            with listener.accept()[0] as connection:
                receive(connection)
                assert actions[-1] == {'action': 'shutdown', 'protocol': 3}
                reply(connection, {'result': 'ok', 'revision': None})
            with listener.accept()[0] as connection:
                if mode != 'unread-close':
                    receive(connection)
                    assert actions[-1] == {'action': 'status', 'protocol': 4}
                    if mode == 'drip-timeout':
                        # Send often enough to reset a per-read timeout while retaining ownership.
                        for byte in b'{"result':
                            try:
                                connection.sendall(bytes([byte]))
                            except (BrokenPipeError, ConnectionResetError):
                                pass  # A correctly bounded client closes before this peer finishes.
                            time.sleep(0.6)
                    else:
                        kind = 'validation' if mode == 'genuine-error' else 'unavailable'
                        reply(connection, {'result': 'error', 'kind': kind, 'message': 'private retirement observation'})
            listener.close()
            endpoint.unlink()
            if mode == 'missing-lock':
                # Path disappearance does not release flock on the original inode.
                (scheduler / 'daemon-v1.lock').unlink()
                time.sleep(4)
            if mode == 'held-timeout':
                time.sleep(4)
            elif mode in ['unavailable', 'unread-close']:
                time.sleep(0.2)
        except BaseException as error:
            faults.append(repr(error))
        finally:
            listener.close()
            if endpoint.exists():
                endpoint.unlink()
            if lock is not None:
                fcntl.flock(lock, fcntl.LOCK_UN)
                lock.close()
            released.set()

    worker = threading.Thread(target=serve, daemon=True)
    worker.start()
    env = {'LANG': 'en_US.UTF-8', 'PATH': '/usr/bin:/bin:/usr/sbin:/sbin',
           'ASCHED_ROOT': str(scheduler), 'HOME': str(home), 'WSX_DAEMON_BIN': str(wsxd),
           'XDG_CONFIG_HOME': str(home / 'c'), 'XDG_DATA_HOME': str(home / 'd'),
           'XDG_STATE_HOME': str(home / 's'), 'XDG_CACHE_HOME': str(home / 'k')}
    started = time.monotonic()
    process = None
    pid = None

    def direct(action):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(2)
            connection.connect(str(endpoint))
            request = {'protocol': 4, 'project': str(project), 'action': {'action': action}}
            connection.sendall((json.dumps(request) + '\n').encode())
            connection.shutdown(socket.SHUT_WR)
            with connection.makefile('rb') as stream:
                return json.loads(stream.readline(4097))

    try:
        process = subprocess.run([str(wsx), 'routine', 'fire', '--project', str(project),
            '--kind', 'retirement.probe', '--event-id', 'verification-' + mode],
            cwd=project, env=env, capture_output=True, text=True, timeout=8)
        elapsed = time.monotonic() - started
        if mode in ['held-timeout', 'drip-timeout', 'missing-lock']:
            assert not released.is_set(), 'fixture did not retain ownership through timeout'
        worker.join(6)
        assert not worker.is_alive() and not faults, faults
        if mode in ['unavailable', 'unread-close']:
            assert process.returncode == 0, process.stderr
            status = direct('status')
            assert status['result'] == 'daemon' and status['protocol'] == 4
            pid = status['pid']
        else:
            assert process.returncode != 0, 'expected fail-closed result'
            assert not endpoint.exists(), 'a refused request started a replacement'
            if mode in ['held-timeout', 'drip-timeout']:
                # ^ routine/client.rs reports its readiness budget, not a literal transport timeout.
                assert 'did not become ready within 3000 ms' in process.stderr and elapsed < 4, (elapsed, process.stderr)
            if mode == 'missing-lock':
                assert 'singleton lock disappeared during retirement' in process.stderr, process.stderr
            if mode == 'mutation-disconnect':
                assert [row['action'] for row in actions] == ['status', 'fire'], actions
                assert not (scheduler / 'daemon-v1.lock').exists(), 'mutation disconnect started a daemon'
        return {'mode': mode, 'result': 'PASS', 'exit': process.returncode,
                'elapsed_seconds': round(elapsed, 3), 'actions': actions,
                'stdout': process.stdout[-4096:], 'stderr': process.stderr[-4096:],
                'replacement_pid': pid, 'peer_joined': True}
    finally:
        (root / 'observed.json').write_text(json.dumps({
            'mode': mode, 'actions': actions, 'peer_faults': faults,
            'exit': process.returncode if process is not None else None,
            'stdout': process.stdout[-4096:] if process is not None else '',
            'stderr': process.stderr[-4096:] if process is not None else '',
        }, indent=2) + '\n')
        worker.join(6)
        if endpoint.exists() and not worker.is_alive():
            status = direct('status')
            pid = status.get('pid')
            assert direct('shutdown')['result'] == 'ok'
        deadline = time.monotonic() + 5
        while endpoint.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert not worker.is_alive(), 'retain scratch: private peer did not settle'
        assert not endpoint.exists(), 'retain scratch: replacement socket survived shutdown'
        if pid is not None:
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                raise AssertionError('retain scratch: owned replacement PID survived shutdown')


if __name__ == '__main__':
    main()
