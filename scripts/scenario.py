#!/usr/bin/env python3
"""Run one end-to-end scenario on this machine: `scenario.py task|web [--part <part>]`.

Build the binaries first (`cargo build -p wsx -p wsx-daemon`). Each part is its own process in its
own process group: every part keeps its own work folder (`.work/<name><pid>`), a TMPDIR for its
sockets and ephemeral ports (127.0.0.1:0), so the parts run at once and share nothing. Every line a
part prints comes out with its name in front. The run ends 1 when a part failed, and then lists
each `FAILED section` and `SKIPPED section` line again. ref: docs/tasks.md "Scenarios"
"""
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / 'scripts'
# ^ A part is a name and its argv. The task scenario's `memory` is the memory contract test, which
# `ci.yml` ran after `core`; it has no part of its own in test-task.py.
SCENARIOS = {
    'task': {
        'core': ['test-task.py', '--part', 'core'],
        'worker': ['test-task.py', '--part', 'worker'],
        'deploy': ['test-task.py', '--part', 'deploy'],
        'host': ['test-task.py', '--part', 'host'],
        'agents': ['test-task.py', '--part', 'agents'],
        'chats': ['test-task.py', '--part', 'chats'],
        'memory': ['test-memory.py', 'target/debug/wsx memory'],
    },
    'web': {
        'chat': ['test-web-client.py', '--browser', '--part', 'chat'],
        'tasks': ['test-web-client.py', '--browser', '--part', 'tasks'],
    },
}
MARKS = ('FAILED section', 'SKIPPED section', 'SECTIONS:')
LOCK = threading.Lock()


def pump(name, process, lines):
    """Copy the part's output, a line at a time, with its name in front."""
    for line in process.stdout:
        text = line.rstrip('\n')
        lines.append(text)
        with LOCK:
            print('[%s] %s' % (name, text), flush=True)


def stop(processes):
    for process in processes:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 10
    for process in processes:
        try:
            process.wait(timeout=max(0.1, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def main():
    args = sys.argv[1:]
    if not args or args[0] not in SCENARIOS or len(args) not in (1, 3) or args[1:2] not in ([], ['--part']):
        sys.exit('usage: scenario.py %s [--part <part>]' % '|'.join(SCENARIOS))
    parts = SCENARIOS[args[0]]
    wanted = args[2] if len(args) == 3 else os.environ.get('WSX_SCENARIO_PART') or None
    if wanted and wanted not in parts:
        sys.exit('no part %r in %s: %s' % (wanted, args[0], ', '.join(parts)))
    chosen = {name: argv for name, argv in parts.items() if wanted in (None, name)}
    env = {key: value for key, value in os.environ.items() if key != 'WSX_SCENARIO_PART'}
    processes, readers, output = {}, [], {}
    started = time.monotonic()
    signal.signal(signal.SIGTERM, lambda *_: (stop(list(processes.values())), os._exit(143)))
    for name, argv in chosen.items():
        command = [sys.executable, '-B', str(SCRIPTS / argv[0]), *argv[1:]]
        process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True, bufsize=1,
                                   start_new_session=True)
        processes[name] = process
        output[name] = []
        reader = threading.Thread(target=pump, args=(name, process, output[name]), daemon=True)
        reader.start()
        readers.append(reader)
    ended = {}
    try:
        while len(ended) < len(processes):
            for name, process in processes.items():
                if name not in ended and process.poll() is not None:
                    ended[name] = time.monotonic() - started
                    with LOCK:
                        print('[%s] ended with exit %d after %d s' % (name, process.returncode, ended[name]), flush=True)
            time.sleep(0.5)
    except KeyboardInterrupt:
        stop(list(processes.values()))
        raise
    for reader in readers:
        reader.join(timeout=5)
    failed = [name for name, process in processes.items() if process.returncode != 0]
    print('== %s scenario: %s, %d s ==' % (
        args[0], ', '.join('%s %s %d s' % (name, 'failed' if name in failed else 'passed', ended[name]) for name in processes),
        time.monotonic() - started), flush=True)
    for name in failed:
        marked = [line for line in output[name] if any(mark in line for mark in MARKS)]
        for line in marked or output[name][-30:]:
            print('[%s] %s' % (name, line))
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
