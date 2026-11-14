#!/usr/bin/env python3
"""Model-free `wsx task` journey on an isolated wsxd with a stream-json fake Claude.

Covers new, steer during a turn, add after it, a blocked result then allow, checks that
pass and fail, report content, a daemon live handoff while a host runs, stop and resume,
close with and without unlanded commits, failure cleanup, and that no process, socket,
worktree or branch is left. Build adjacent wsx and wsxd first. `--part core|worker|host` (or
WSX_SCENARIO_PART) runs one part; none runs all. ref: docs/tasks.md
"""
import argparse
import atexit
import datetime
import fcntl
import hashlib
import http.server
import urllib.request
import json
import os
import pwd
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import threading
import time

import wsxd_client
from tui_pty import Screen, TuiPty

ROOT = Path(__file__).resolve().parents[1]
# ^ Parts other than 16 land by hand (`land.auto: false`) so a ready task stays ready.
# ^ `minFreeGb: 0`: the fixture must not wait on the free disk of the machine it runs on.
CONFIG = '''tasks:
  minFreeGb: 0
  land: { auto: false }
  agent:
    model: fake-model
  checks:
    - "test -f a.txt"
    - "test ! -e broken.txt"
'''

# A process that holds a slot of tasks/<queue>/ (crates/wsx/src/task/slot.rs) under a label.
HOLD_SLOT = """import fcntl, os, sys, time
slot = open(os.path.join(sys.argv[1], 'slot.lock'), 'a')
fcntl.flock(slot, fcntl.LOCK_EX)
slot.truncate(0)
slot.write(sys.argv[2])
slot.flush()
time.sleep(600)
"""
# What a host that started before the CLI's newest request does with it: it cannot read the
# request and closes the connection without an answer. Status it answers. Each other request
# is a line `<pid> <action>` in the log; the CLI sends it again for 3 s, so there are many.
OLD_HOST = """import json, os, socket, sys
path, log, pidfile = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    os.unlink(path)
except FileNotFoundError:
    pass
server = socket.socket(socket.AF_UNIX)
server.bind(path)
os.chmod(path, 0o600)
server.listen(8)
with open(pidfile, 'w') as out:
    out.write(str(os.getpid()))
while True:
    client, _ = server.accept()
    data = b''
    while True:
        chunk = client.recv(65536)
        if not chunk:
            break
        data += chunk
    action = json.loads(data)['action']
    if action != 'status':
        with open(log, 'a') as out:
            out.write('%d %s\\n' % (os.getpid(), action))
    if action == 'status':
        client.sendall(b'{"ok":true,"state":"ready","message":""}\\n')
    client.close()
"""
LAND_CONFIG = '''tasks:
  minFreeGb: 0
  land: { auto: false }
  checks:
    - "test ! -e broken.txt"
'''
# ^ Section 17's config, which the sections after it start from.
DEPLOY_CONFIG = LAND_CONFIG + '''  deploy:
    command: 'printf "%s %s\\n" "$WSX_DEPLOY_COMMIT" "$(pwd)" >> ../deploy.log; printf "%s" "$SSH_AUTH_SOCK" > ../deploy-agent.log'
  release:
    command: 'seq 1 60; echo "release broke here: Permission denied (publickey)." >&2; exit 3'
'''


def unquoted(entry):
    """A reply without the quote wsx adds for the owner message it answers (`quote` is one of `to`,
    docs/tasks.md "A quote"): its words as the manager wrote them, for the checks that compare them.
    The quote itself has its own block in part_core."""
    if entry['kind'] != 'reply' or entry.get('quote') not in entry['to']:
        return entry
    kept = {key: value for key, value in entry.items() if key != 'quote'}
    return {**kept, 'text': entry['text'].partition('\n\n')[2]}


# ^ What a worker tries inside wsx's boundary (docs/tasks.md "Permissions"), run as the worker's
# own command by the fake worker (part 23) and by the real Claude (part 32h). A write to a file
# that exists only opens it for appending, so nothing changes when it is allowed.
PROBE_SCRIPT = '''import json, os, socket, subprocess, sys
spec = json.load(open(sys.argv[1]))
def tried(action):
    try:
        action()
        return True
    except OSError:
        return False
def write(path):
    if os.path.isfile(path):
        os.close(os.open(path, os.O_WRONLY | os.O_APPEND))
    else:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        open(path, "w").close()
        os.remove(path)
def listen():
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    client = socket.create_connection(server.getsockname())
    server.accept()[0].close()
    client.close()
    server.close()
def pty():
    for fd in os.openpty():
        os.close(fd)
seen = {name: tried(lambda path=path: open(path).read()) for name, path in spec["read"].items()}
seen.update({name: tried(lambda path=path: write(path)) for name, path in spec["write"].items()})
seen["move_main"] = subprocess.run(["git", "update-ref", "refs/heads/main", "HEAD"], capture_output=True).returncode == 0
seen.update(listen=tried(listen), pty=tried(pty))
# ^ A signal reaches only this agent's own sandbox: its child yes, a process outside it no.
child = subprocess.Popen(["sleep", "60"])
seen["signal_own"] = tried(lambda: os.kill(child.pid, 15))
child.wait()
seen["signal_outside"] = tried(lambda: os.kill(spec["signal_outside"], 9))
json.dump(seen, open(sys.argv[2], "w"))
'''
# ^ True: the boundary allows it; False: it refuses it.
PROBE_EXPECTED = {'read_ssh': False, 'write_ssh': False, 'write_state': False, 'write_existing_copy': False,
                  'write_later_copy': False, 'write_main_ref': False, 'write_main_lock': False,
                  'write_main_reflog': False, 'write_hooks': False, 'write_config': False, 'write_routines': False,
                  'write_own_copy': True, 'write_task_folder': True, 'write_tmpdir': True, 'write_cargo': True,
                  'write_library': True, 'move_main': False, 'listen': True, 'pty': True,
                  'signal_own': True, 'signal_outside': False}


# ^ Ctrl-A q: quits the TUI.
QUIT = b'\x01q'


def tui_text(wsx, env, cwd, until, seconds=15):
    """The TUI's screen as text, read until `until` (a regex) matches."""
    tui = TuiPty([str(wsx)], cwd, env)
    try:
        return tui.wait_text(until, 'the TUI did not show %r' % until, seconds)
    finally:
        tui.close(QUIT)


# ^ The scenario runs as these parts (`--part`, or WSX_SCENARIO_PART); none runs all, in order. A part
# run alone starts from setup() and makes the project state the sections before it left (fresh_project,
# store_up, manager_up). docs/tasks.md "Scenarios"
PART_NAMES = ('core', 'worker', 'host')


# ^ Each numbered section marks its start; the result names the slowest sections, so a slow run
# says where its time went.
PARTS = []


def mark(name):
    assert all(name != seen for seen, _ in PARTS), 'part name %s is used twice' % name
    PARTS.append((name, time.monotonic()))


def spans():
    ends = [at for _, at in PARTS[1:]] + [time.monotonic()]
    return [(name, round(end - at, 1)) for (name, at), end in zip(PARTS, ends)]


def slowest(count=8):
    return sorted(spans(), key=lambda span: -span[1])[:count]


def gate_cmd(path):
    """A check or scenario command that runs until `path` exists."""
    return 'while [ ! -e %s ]; do sleep 0.2; done' % path


# ^ The manager host's stall grace (60 s) and sweep (10 s): the scenario sets the grace to
# STALL_GRACE through wsxd's environment, so a stall is reported on the second sweep.
STALL_GRACE = 4
# ^ How long a failed sign-in on record keeps the account skipped (store.rs `sign_in_retry_ms`).
SIGN_IN_RETRY = 12
SWEEP = 10


def tui_screens(wsx, env, cwd, steps, cols=120, raw=None):
    """For each `(regex, keys)` step: wait until the drawn screen matches, keep it, send the
    keys. Returns the kept screens. A screen counts once output pauses: a frame can arrive in
    several reads. `raw`, a list, gets the output so far at each kept screen."""
    tui = TuiPty([str(wsx)], cwd, env, cols=cols)
    screens = []
    try:
        for index, (pattern, keys) in enumerate(steps):
            screens.append(tui.wait_text(pattern, 'TUI step %d %r not shown' % (index, pattern), 40, idle=0.1))
            if raw is not None:
                raw.append(bytes(tui.raw))
            if index < len(steps) - 1:
                tui.send(keys() if callable(keys) else keys)
    finally:
        tui.close(QUIT)
    return screens


def forget_notes(notes):
    """Make the manager's notes folder old notes again (docs/memory.md "Notes"): drop the copies, the copies of
    hand edits, the hashes of the views and the records the notes made, so a fixture written by hand is
    imported at the next write. Records of other kinds stay."""
    for found in list(notes.glob('*.pre-memory')) + list(notes.glob('*.drift')):
        found.unlink()
    memory = notes.parent.parent / 'memory'
    (memory / 'views.json').unlink(missing_ok=True)
    log = memory / 'records.jsonl'
    if log.exists():
        kept = []
        for line in log.read_text().splitlines():
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if record.get('kind') != 'rule' and not record.get('slug', '').startswith(('section-', 'memo-', 'start-', 'rotation-', 'handover-')):
                kept.append(line + '\n')
        log.write_text(''.join(kept))


def memory_records(raw):
    """The records of a `wsx memory search` answer: a project scope also finds the project's transcripts."""
    return [h for h in json.loads(raw) if h['kind'] in ('rule', 'fact', 'episode')]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    parser.add_argument('--part', choices=PART_NAMES, default=os.environ.get('WSX_SCENARIO_PART') or None,
                        help='run one part; no part runs all')
    args = parser.parse_args()
    wsx = args.wsx.resolve()
    wsxd = wsx.with_name('wsxd')
    if not wsx.is_file() or not wsxd.is_file():
        raise RuntimeError('build adjacent wsx and wsxd first')
    work = ROOT / '.work' / ('tk' + str(os.getpid()))
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    started = time.monotonic()
    PARTS.append(('setup', started))
    owned = {}
    try:
        run(wsx, wsxd, work, owned, args.part)
        print(json.dumps({'ok': True, 'seconds': round(time.monotonic() - started, 1), 'slowest': slowest(), 'part': args.part, 'parts': dict(spans())}))
    finally:
        # ^ Stop the isolated wsxd also after a failure: its hosts and fake agents end with it.
        if 'env' in owned:
            subprocess.run([str(wsx), 'daemon', 'stop', '--force'], env=owned['env'],
                           capture_output=True, timeout=20)
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)


# The local floor's endpoint (docs/local-model.md): /health and /v1/models of a llama-server, up
# or down as FLOOR['up'] says; `asked` counts the answers it gave.
FLOOR = {'up': False, 'asked': 0, 'key': 'floor-key-1234', 'model': 'lfm3-1.7b'}


class FloorEndpoint(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        FLOOR['asked'] += 1
        body, status = b'{}', 200
        if not FLOOR['up']:
            body, status = b'{"error":"loading"}', 503
        elif self.path == '/v1/models':
            if self.headers.get('Authorization') != 'Bearer ' + FLOOR['key']:
                body, status = b'{"error":"key"}', 401
            else:
                body = json.dumps({'data': [{'id': FLOOR['model']}]}).encode()
        elif self.path != '/health':
            body, status = b'{}', 404
        self.send_response(status)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def start_floor(work):
    """Serve FloorEndpoint on a free loopback port; its URL and key file go to the hosts' env."""
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), FloorEndpoint)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    key_file = work / 'local-key'
    key_file.write_text(FLOOR['key'] + '\n')
    return {'LOCAL_MODEL_URL': 'http://127.0.0.1:%d' % server.server_address[1],
            'LOCAL_MODEL_KEY_FILE': str(key_file), 'WSX_TEST_FLOOR_CACHE_SECS': '1'}


def run(wsx, wsxd, work, owned, part):
    # ^ Long state and project paths: a socket in the store would pass sun_path's 104 bytes,
    # so the hosts' sockets must live elsewhere (docs/tasks.md "Store").
    base = work / ('long-project-folder-' + 'p' * 40)
    home, state, project, bin_dir = work / 'h', work / ('state-' + 's' * 40), base / 'project', work / 'bin'
    # ^ Its own TMPDIR: the host sockets' folder goes away with the run. It is reached through
    # a link, as macOS `/var/folders/...` is a link to `/private/var/...`.
    tmp = work / 't'
    base.mkdir(mode=0o700)
    for path in (home, state, project, bin_dir, work / 't-real'):
        path.mkdir(mode=0o700)
    tmp.symlink_to(work / 't-real')
    config_dir = (home / 'Library/Application Support/wsx' if sys.platform == 'darwin'
                  else work / 'c/wsx')
    config_dir.mkdir(parents=True)
    # ^ docs/configuration.md "Profiles": demo is in profile work, whose agents use its own config root.
    profile_root = home / 'claude-work'
    profile_root.mkdir()
    registry_text = ('resume_agents_on_restore = false\n[[profiles]]\nname = "work"\n'
                     'claude_config_dir = "~/claude-work"\n[[projects]]\nname = "demo"\npath = '
                     + json.dumps(str(project)) + '\nprofile = "work"\n')
    (config_dir / 'config-v2.toml').write_text(registry_text)
    fake = bin_dir / 'claude'
    # ^ A copy per run: the leftover checks then see this run's fake workers, not another run's.
    fake_script = bin_dir / 'fake-claude-stream.py'
    shutil.copy(ROOT / 'scripts/fake-claude-stream.py', fake_script)
    shutil.copytree(ROOT / 'scripts/fixtures', bin_dir / 'fixtures')
    # ^ The traces go to TMPDIR, where an untrusted agent may write (section 23s).
    trace_env = 'export FAKE_CLAUDE_TRACE=' + str(tmp / 'fake-claude.trace') + '\n'
    fake.write_text('#!/bin/sh\n' + trace_env + 'exec ' + sys.executable + ' ' + str(fake_script) + ' "$@"\n')
    fake.chmod(0o755)
    # ^ The fallback agent of part 36 (docs/tasks.md "Agents").
    shutil.copy(ROOT / 'scripts/fake-pi-rpc.py', bin_dir / 'fake-pi-rpc.py')
    (bin_dir / 'pi').write_text('#!/bin/sh\n' + trace_env + 'exec ' + sys.executable + ' ' + str(bin_dir / 'fake-pi-rpc.py') + ' "$@"\n')
    (bin_dir / 'pi').chmod(0o755)
    daemon_socket = work / 'd.sock'
    env = {'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(work / 'c'),
           'XDG_CACHE_HOME': str(work / 'k'), 'WSX_DAEMON_BIN': str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()), 'WSX_SOCKET': str(daemon_socket),
           'WSX_TEST_STALL_GRACE_SECS': str(STALL_GRACE), 'WSX_TEST_ALLOW_FILE_CLONE': '1', 'WSX_TEST_PRUNE_SWEEP_SECS': '3',
           'WSX_TEST_SIGN_IN_RETRY_SECS': str(SIGN_IN_RETRY), 'WSX_TEST_CI_BACKOFF_SECS': '1', **start_floor(work),
           # ^ The limit probe's period (30 s) would put probe traces into every long limit; the
           # limit scenario of part 11 starts a probe by clearing the stamp.
           'WSX_TEST_LIMIT_PROBE_SECS': '600',
           # ^ While this file holds a number, a turn quiet that many seconds ends (docs/tasks.md
           # "Agents"): the hung-turn sections write 40, longer than the fakes' LONG (25 s); the
           # other sections keep the real 10 minutes, since a WAIT gate holds a turn quiet longer.
           'WSX_TEST_TURN_QUIET_FILE': str(work / 'turn-quiet'),
           'PATH': str(bin_dir) + ':/usr/bin:/bin:/usr/sbin:/sbin', 'SHELL': '/bin/sh',
           'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8', 'TMPDIR': str(tmp),
           # ^ A fixture git command must never reach the repository this script runs in.
           'GIT_CEILING_DIRECTORIES': str(work)}
    owned['env'] = env

    def git(*argv, cwd=project, ok=True):
        result = subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', *argv],
                                cwd=cwd, env=env, capture_output=True, text=True, timeout=20)
        if ok:
            assert result.returncode == 0, result.stderr
        return result.stdout.strip()

    def configure(text, message):
        (project / 'wsx.config.yml').write_text(text)
        git('commit', '-q', '-am', message)

    git('init', '-q', '-b', 'main')
    assert os.path.realpath(git('rev-parse', '--show-toplevel')) == os.path.realpath(project), 'fixture git escaped'
    (project / 'wsx.config.yml').write_text(CONFIG)
    (project / '.gitignore').write_text('target/\n')
    (project / 'README').write_text('fixture\n')
    git('add', '.')
    git('commit', '-q', '-m', 'start')
    # Ignored build output is kept (warm builds); an untracked file is cleaned.
    (project / 'target').mkdir()
    (project / 'target/warm').write_text('cached')
    # ^ docs/tasks.md "Build output": incremental/ is not cloned; stale units and objects are
    # pruned after checks. bar is a dependency with two rustc sessions, foo a unit rebuilt below.
    deps = project / 'target/debug/deps'
    deps.mkdir(parents=True)
    (project / 'target/debug/incremental/foo-1/s-1').mkdir(parents=True)
    (project / 'target/debug/incremental/foo-1/s-1/a.o').write_text('o')
    for name, when in [('libbar-00000000000000aa.rlib', 100), ('bar-00000000000000aa.c1.old.rcgu.o', 0),
                       ('bar-00000000000000aa.c1.new.rcgu.o', 100), ('foo-00000000000000bb', 0),
                       ('foo-00000000000000bb.d', 0)]:
        (deps / name).write_text(name)
        os.utime(deps / name, (1_600_000_000 + when,) * 2)
    (project / 'scratch.txt').write_text('untracked')
    # ^ An unchanged tracked file keeps its clone's mtime, so cargo reuses the warm build.
    os.utime(project / 'README', (1_600_000_000, 1_600_000_000))

    # ^ Once a second project is registered, a task command must name its project.
    named_project = []

    def cli(*argv, ok=True, stdin=None):
        if named_project and argv[0] == 'task' and '-p' not in argv:
            argv = (*argv, '-p', named_project[0])
        result = subprocess.run([str(wsx), *map(str, argv)], cwd=project, env=env, input=stdin,
                                capture_output=True, text=True, timeout=60)
        if ok:
            assert result.returncode == 0, (argv, result.stdout, result.stderr)
        else:
            assert result.returncode != 0, (argv, result.stdout)
        return result

    def js(*argv):
        return json.loads(cli(*argv, '--json').stdout)

    def cli_env(extra, *argv):
        return subprocess.run([str(wsx), *map(str, argv)], cwd=project, env={**env, **extra}, capture_output=True,
                              text=True, timeout=60, check=True)

    def wait(test, label, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = test()
            if value:
                return value
            time.sleep(0.1)
        raise RuntimeError((label() if callable(label) else label) + ' timeout')

    def stays(positive, absent, label, seconds=60):
        """Something did not happen: first wait for `positive`, an event that comes after the
        moment it could have happened, so a late event is no pass; then `absent` must hold."""
        wait(positive, label, seconds)
        assert absent(), label

    def sweeps(count):
        """Time for `count` sweeps of the manager host's stall check after the grace."""
        time.sleep(STALL_GRACE + count * SWEEP + 2)

    def show(n):
        return js('task', 'show', n)

    def wait_state(n, *states):
        return wait(lambda: (current := show(n))['state'] in states and current, 't%s %s' % (n, states))

    def daemon_call(method):
        return wsxd_client.call(daemon_socket, method)

    def pane_agent(n):
        record = show(n)['record']
        panes = daemon_call('snapshot')['data']['panes']
        pane = next((p for p in panes if p['id'] == record['pane_id']), None)
        return pane and pane.get('agent')

    def traces():
        path = tmp / 'fake-claude.trace'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def log(n):
        path = Path(show(n)['log'])
        return [json.loads(line) for line in path.read_text().splitlines()]

    def kinds(n, *names):
        return [e['kind'] for e in log(n) if e['kind'] in names]

    def leftovers(marker):
        ps = subprocess.run(['ps', '-axo', 'pid=,command='], capture_output=True, text=True).stdout
        return [line for line in ps.splitlines() if marker in line and 'ps -axo' not in line]

    def leaks(seen):
        # ^ The fake's LEAK records after trace line `seen`: its tool pids and its own.
        return [t for t in traces()[seen:] if 'leak' in t]

    def gone(*pids):
        # ^ No such process: an ended child that its parent has not reaped is still found.
        def missing(pid):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return True
            return False
        return all(missing(pid) for pid in pids)

    def tree_gone(record, label):
        wait(lambda: gone(record['agent'], *record['leak']), label, 15)

    def zombies(n):
        host = int(leftovers('task host %s %d' % (show(n)['record']['project_id'], n))[0].split()[0])
        ps = subprocess.run(['ps', '-axo', 'ppid=,stat='], capture_output=True, text=True).stdout
        return [line for line in ps.splitlines() if line.split()[0] == str(host) and 'Z' in line.split()[1]]

    owner_config = work / 'c' / 'wsx' / 'wsx.config.yml'

    owner_config.parent.mkdir(parents=True, exist_ok=True)

    def sessions_at(path, key='label'):
        data = daemon_call('snapshot')['data']
        ids = {w['id'] for w in data['worktrees'] if os.path.realpath(w['path']) == os.path.realpath(path)}
        return [s[key] for s in data['sessions'] if s['worktree_id'] in ids]

    def land(n, *flags, ok=True):
        return json.loads(cli('task', 'land', n, *flags, '--json', ok=ok).stdout)

    def main_has(path):
        return subprocess.run(['git', 'cat-file', '-e', 'main:' + path], cwd=project, env=env).returncode == 0

    def wait_closed(n):
        # ^ A landed task closes itself and stays `landed`: its copy, build and branch go; the
        # task folder and the log, which ends `closed`, stay.
        copy = Path(show(n)['record']['working_copy'])
        wait(lambda: show(n)['state'] == 'landed' and not copy.exists() and log(n)[-1]['kind'] == 'closed'
             and not git('branch', '--list', 'wsx/t%d' % n), 't%s closed after landing' % n, 60)

    def reviewed(n):
        """Wait for a ready task that stays ready, and land it as the manager's review would."""
        assert wait_state(n, 'ready', 'landed', 'failed')['state'] == 'ready', show(n)
        return land(n)

    manager_env = dict(env, WSX_TASK_ACTOR='manager')

    def next_task():
        return max(row['id'] for row in js('task', 'list', '--all')['tasks']) + 1

    def manager_new(prompt, *flags):
        made = subprocess.run([str(wsx), 'task', 'new', *flags, prompt, '--json'], cwd=project, env=manager_env,
                              capture_output=True, text=True, timeout=60)
        assert made.returncode == 0, made.stderr
        return json.loads(made.stdout)['id']

    slot_spans, release = base / 'slot-spans.log', base / 'slot-release'

    slot_check = base / 'slot-check.py'

    slot_check.write_text(
        'import os, sys, time\n'
        'def mark(what):\n'
        '    with open(%r, "a") as out:\n'
        '        out.write("%%s %%s %%.3f\\n" %% (os.path.basename(os.getcwd()), what, time.time()))\n'
        'mark("start")\n'
        'deadline = time.time() + float(sys.argv[1])\n'
        'while time.time() < deadline and not os.path.exists(%r):\n'
        '    time.sleep(0.05)\n'
        'mark("end")\n' % (str(slot_spans), str(release)))

    def slot_config(seconds):
        configure(LAND_CONFIG.replace(
            '"test ! -e broken.txt"', '"%s %s %d"' % (sys.executable, slot_check, seconds)), 'slot checks %d' % seconds)

    def slot_marks():
        marks = {}
        for line in slot_spans.read_text().splitlines() if slot_spans.exists() else []:
            copy_name, what, at = line.split()
            marks.setdefault(copy_name, {})[what] = float(at)
        return marks

    def copy_name(n):
        return Path(show(n)['record']['working_copy']).name

    # Refusals leave nothing: bad config, then bad ids.
    (project / 'wsx.config.yml').write_text('tasks:\n  agent:\n    adapter: nope\n')
    refused = cli('task', 'new', 'WORK a.txt', ok=False)
    assert 'tasks.agent.adapter' in refused.stderr and 'no task was created' in refused.stderr, refused.stderr
    # ^ The manager's name is one short line (docs/tasks.md "Config").
    for name, word in (('"At\\nlas"', 'control characters'), ('"%s"' % ('x' * 65), 'at most 64 bytes')):
        (project / 'wsx.config.yml').write_text('tasks:\n  manager:\n    name: %s\n' % name)
        refused = cli('task', 'new', 'WORK a.txt', ok=False).stderr
        assert 'tasks.manager.name' in refused and word in refused, refused
    # ^ A known key with a wrong type still fails; tasks.skills and tasks.workflow are checked.
    for bad, word in (('  minFreeGb: lots\n', 'invalid type: string "lots"'), ('  land: { auto: maybe }\n', 'invalid type'),
                      ('  skills: everyone\n', 'tasks.skills must be owner or none'),
                      ('  workflow: { build: "a\\u0007b" }\n', 'tasks.workflow.build must be'),
                      ('  workflow: { idle: "a\\u0007b" }\n', 'tasks.workflow.idle must be'),
                      ('  env: { WSX_SOCKET: x }\n', 'tasks.env: wsx sets WSX_SOCKET itself'),
                      ('  env: { "A-B": x }\n', 'is not a variable name')):
        (project / 'wsx.config.yml').write_text('tasks:\n' + bad)
        refused = cli('task', 'new', 'WORK a.txt', ok=False).stderr
        assert word in refused, refused
    (project / 'wsx.config.yml').write_text(CONFIG)
    # ^ docs/configuration.md "Profiles": a profile error is named, and no task starts under another account.
    for old, bad, word in (('profile = "work"', 'profile = "ghost"', 'names profile ghost, which no [[profiles]] declares'),
                           ('[[projects]]', '[[profiles]]\nname = "work"\n[[projects]]', 'profile work is declared twice'),
                           ('"~/claude-work"', '"claude-work"', 'claude_config_dir must be absolute or start with ~/'),
                           ('"~/claude-work"', '"~/claude-gone"', 'claude_config_dir %s is missing' % (home / 'claude-gone'))):
        (config_dir / 'config-v2.toml').write_text(registry_text.replace(old, bad, 1))
        refused = cli('task', 'new', 'WORK a.txt', ok=False).stderr
        assert word in refused, (word, refused)
    assert not (state / 'wsx/tasks').exists() or not list((state / 'wsx/tasks').glob('*/*/task.json'))
    (config_dir / 'config-v2.toml').write_text(registry_text)
    assert 'invalid task number' in cli('task', 'show', '../1', ok=False).stderr
    assert 'a prompt is required' in cli('task', 'new', ok=False).stderr
    assert 'empty' in cli('task', 'new', '-', stdin='\n', ok=False).stderr

    # ^ The parts read what setup and the shared starts (store_up, manager_up, probe_up) made as module
    # globals. A name a part assigns is its own for the whole part: it must not read it before.
    globals().update(locals())
    # ^ A part run after another goes on from its state; one run alone makes its own (fresh_project).
    created = None
    for name, body in (('core', part_core), ('worker', part_worker), ('host', part_host)):
        if part in (None, name):
            created = body(created)


def login_agent_socket():
    """The login session's ssh agent socket, as wsx finds it: launchd's on macOS when it answers."""
    if sys.platform != 'darwin':
        return None
    printed = subprocess.run(['launchctl', 'print', 'gui/%d/com.openssh.ssh-agent' % os.getuid()],
                             capture_output=True, text=True).stdout
    found = re.search(r'SSH_AUTH_SOCK => (\S+)', printed)
    if not found:
        return None
    with socket.socket(socket.AF_UNIX) as probe:
        try:
            probe.connect(found.group(1))
        except OSError:
            return None
    return found.group(1)


def store_up(created):
    """The deploy and release records of the project store (section 17)."""
    project_store = state / 'wsx/tasks' / Path(created['task_dir']).parent.name

    def deploy_records():
        path = project_store / 'deploys.jsonl'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def deploy_lock_free():
        with open(project_store / 'deploy.lock', 'a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except BlockingIOError:
                return False

    def ran(*argv, seconds=20):
        # ^ A deploy or release by hand answers once it started; its detached run records it.
        before = len(deploy_records())
        started = js('task', *argv)
        assert started['started'] and ' started; ' in started['message'], started
        wait(lambda: len(deploy_records()) > before and deploy_lock_free(), 'the detached ' + argv[0], seconds)
        assert started['commit'] == deploy_records()[-1]['commit'], started
        return dict(deploy_records()[-1], message=started['message'])

    globals().update(locals())


def default_manager_model():
    """The manager config with no model (it defaults to opus): what section 21 leaves for the parts after it."""
    configure((project / 'wsx.config.yml').read_text().replace('model: fake-manager, ', ''), 'default manager model')


def manager_up(created):
    """Section 20: the manager starts and answers. A part run alone starts it again."""
    # 20. The manager: started by the first say, answers from the ledger, acts through wsx task.
    mark('20')
    configure((project / 'wsx.config.yml').read_text()
              + '  manager: { name: "  Atlas ", model: fake-manager, eventModel: fake-events, '
                'rotateTokens: 100000 }\n'
              + '  skills: owner\n  workflow:\n    build: "Run the fixture rule."\n'
                '    review: "Check the fixture rule."\n', 'manager config')
    # ^ The owner's skills folder: the manager gets it as the `owner` plugin. It is the one of
    # demo's profile (docs/configuration.md "Profiles"), not ~/.claude.
    owner_skills = profile_root / 'skills'
    (owner_skills / 'scribe').mkdir(parents=True)
    (owner_skills / 'scribe/SKILL.md').write_text('---\nname: scribe\ndescription: fixture\n'
                                                  'allowed-tools: Write, Edit\n---\nWrite a file.\n')

    (owner_skills / 'other').mkdir()
    (owner_skills / 'other/SKILL.md').write_text('---\nname: other\ndescription: fixture\n---\nx\n')

    def chat(after=None, raw=False):
        found = js('task', 'chat', *(['--after', after] if after is not None else []))['entries']
        return found if raw else [unquoted(e) for e in found]

    def reply_to(message_id, seconds=20):
        return wait(lambda: next((e for e in chat() if e['kind'] == 'reply' and message_id in e['to']), None),
                    'reply to %s' % message_id, seconds)

    def raw_reply(message_id, seconds=20):
        """The stored reply to `message_id` with the quote wsx added (`reply_to` leaves it out)."""
        reply_to(message_id, seconds)
        return next(e for e in chat(raw=True) if e['kind'] == 'reply' and message_id in e['to'])

    def turn_with(text):
        """The first manager turn whose message holds `text`."""
        return next((t for t in traces() if t.get('manager') and text in str(t.get('message', ''))), None)

    running = js('task', 'new', 'SLOW SLOW SLOW WORK r.txt')['id']
    # ^ A project with the old `page.md` and no overview gets it moved once, when the manager starts.
    old_page = state / 'wsx/tasks' / Path(created['task_dir']).parent.name / 'page.md'
    old_page.write_text('# demo\n\n## Goal\n\nmigrated\n')
    asked = js('task', 'say', 'STATUS')
    assert asked['ok'] and asked['id'] == 1, asked
    asked_reply = reply_to(asked['id'])
    assert ('#%d working' % running) in asked_reply['text'] and 'quiet' not in asked_reply, asked_reply
    launch = next(t for t in traces() if t.get('launch') and t.get('manager'))
    # ^ The message ends with the host's ledger summary; a fresh conversation gets every open task.
    opening = next(t['message'] for t in traces() if t.get('manager') and 'message' in t)
    summary = opening.split('\n\nLedger summary from wsx (`wsx task ledger -p demo` has the details):\n', 1)[1]
    assert summary.startswith('- open: ') and 'working; to check: ' in summary, summary
    assert '#%d working r' % running in summary.split('- open tasks:\n', 1)[1], summary
    assert '`wsx task ledger -p demo` are the truth' in launch['instructions'], launch['instructions']
    # ^ The manager never commits in the main checkout (docs/design/root-cause-refactor.md step 1).
    assert 'git commit' not in launch['instructions'] and 'only after the owner says yes' not in launch['instructions']
    page = os.path.realpath(state / 'wsx/tasks' / Path(created['task_dir']).parent.name / 'overview.md')
    assert not old_page.exists() and 'migrated' in Path(page).read_text(), old_page
    # ^ It uses the owner's skills (docs/tasks.md "Manager"); no tool list: nothing prompts.
    assert not {'--allowedTools', '--disallowedTools'} & set(launch['argv']), launch['argv']
    plugin = Path(launch['plugin_dir'])
    assert plugin.parent.name == 'manager' and plugin.resolve().is_relative_to(state.resolve()), plugin
    assert json.loads((plugin / '.claude-plugin/plugin.json').read_text())['name'] == 'owner'
    assert os.readlink(plugin / 'skills') == str(owner_skills), os.readlink(plugin / 'skills')
    # ^ No prompts inside wsx's boundary (docs/tasks.md "Permissions").
    assert launch['mode'] == 'bypassPermissions' and 'permissions' not in launch['settings'], launch['settings']
    assert launch['model'] == 'fake-manager' and Path(page).read_text().startswith('# demo\n\n## Goal')
    # ^ A manager turn that leaves the overview over the cap (60 lines, 4,000 characters) gets one
    # follow-up turn that names the sizes; the second over-cap result gets no third
    # (docs/tasks.md "Project overview").
    long_said = js('task', 'say', 'OVERLONG 70')
    wait(lambda: turn_with('The project overview is 70 lines / ') and reply_to(long_said['id']), 'the overview follow-up')
    time.sleep(4)
    follow = [t for t in traces() if t.get('manager') and 'The project overview is ' in str(t.get('message', ''))]
    assert len(follow) == 1 and 'the cap is 60 / 4,000' in follow[0]['message'], follow
    assert len(Path(page).read_text().splitlines()) == 70
    Path(page).write_text('# demo\n\n## Goal\n\nshort\n')
    # ^ An expired sign-in (docs/tasks.md "Manager"): one `stopped` that says to sign in, no new
    # conversation, a new agent process for each try, and an owner message tries at once.
    signed_in = work / 'signed-in'
    manager_log = Path(page).parent / 'manager/log.jsonl'
    entries = lambda: [json.loads(line) for line in manager_log.read_text().splitlines()]
    launches = lambda: [t for t in traces() if t.get('launch') and t.get('manager')]
    tried = lambda n: [t for t in traces() if t.get('manager') and 'AUTHFAIL' in str(t.get('message', ''))][n - 1:]
    first_launch = len(launches())
    failed_at = lambda: next(e['unix_ms'] for e in entries() if e['kind'] == 'stopped' and 'Sign in again' in e['reason'])
    failing = js('task', 'say', 'AUTHFAIL %s' % signed_in)
    wait(lambda: any(e['kind'] == 'stopped' and 'Sign in again' in e.get('reason', '') for e in entries()),
         'the sign-in stopped entry')
    again = js('task', 'say', 'AUTHFAIL %s again' % signed_in)
    wait(lambda: tried(2) and len(launches()) >= first_launch + 1, 'the owner message retries in a new process', 20)
    # ^ The hold logs no pause, so the chat has no "resumed" row for it, and an owner message that
    # no turn has taken yet is queued (`waiting_ids`): a turn that fails 4 s after it starts keeps
    # the next message waiting, in the JSON, in the text and in the log.
    slow = js('task', 'say', 'AUTHFAIL %s AUTHSLOW' % signed_in)
    wait(lambda: turn_with('AUTHSLOW'), 'the slow sign-in turn')
    waits = js('task', 'say', 'AUTHFAIL %s waits' % signed_in)
    held = js('task', 'chat')
    assert held['waiting'] == [waits['id']], (held['waiting'], waits, slow)
    text = cli('task', 'chat').stdout
    assert re.search(r'\[%d\] You \(waiting\): AUTHFAIL' % waits['id'], text) and text.count('(waiting)') == 1, text
    assert [e['kind'] for e in entries() if e['kind'] == 'limit' and e['unix_ms'] >= failed_at()] == [], entries()[-12:]
    assert len([e for e in entries() if e['kind'] == 'stopped' and 'Sign in again' in e['reason']]) == 1, entries()[-12:]
    assert 'resumed' not in text.split('Sign in again')[-1], text
    # ^ The slow turn fails and sets the next try ahead: a message sent after that tries at once.
    assert waits['busy'], waits
    time.sleep(6)
    signed_in.touch()
    after_sign_in = js('task', 'say', 'STATUS')
    replied = reply_to(after_sign_in['id'])
    assert {failing['id'], again['id'], slow['id'], waits['id']} <= set(replied['to']), replied
    # ^ A reply to a turn with several owner messages quotes the newest one (docs/tasks.md "A quote").
    several = raw_reply(after_sign_in['id'])
    assert several['quote'] == after_sign_in['id'] and several['text'].startswith('> STATUS\n\n'), several
    assert js('task', 'chat')['waiting'] == [], 'a message the turn took is a bubble'
    failed = [e for e in entries() if 'OAuth session expired' in str(e)]
    assert [e['kind'] for e in failed] == ['agent', 'stopped', 'reply'], failed
    # ^ The hold for the sign-in reaches the owner once, at once: a post at level bad with the
    # reason, the retry and the open tasks (docs/tasks.md "Manager").
    alert = failed[2]
    assert alert['post'] and alert['level'] == 'bad' and 'It tries again at' in alert['text'] \
        and re.search(r'(\d+ open tasks?: .+|No task is open)\.$', alert['text']), alert
    assert not [e for e in entries() if e['kind'] == 'rotated' and e['unix_ms'] >= failed[0]['unix_ms']], entries()[-20:]
    retries = launches()[first_launch:]
    assert len({t['pid'] for t in retries}) == len(retries) >= 2 and all(t['resume'] for t in retries), retries
    # ^ A usage limit the provider lifts before its reset, as when the owner uses a reset
    # (docs/tasks.md "Agents"): the account's record, confirmed by a probe, decides every wait,
    # not the reset time a host kept. The held manager and two tasks go on with no message.
    agents_dir = state / 'wsx/tasks/agents'
    limit_record, probe_stamp = agents_dir / 'claude.json', agents_dir / 'claude.probe.json'
    reset_mark, warn_mark = tmp / 'fake-claude.trace.reset', tmp / 'fake-claude.trace.warn'
    probes = lambda: [t for t in traces() if t.get('limit_probe')]
    # ^ No turn in flight: a call that goes through reports `allowed`, which ends the limit.
    wait(lambda: entries()[-1]['kind'] in ('reply', 'delivered') and time.time() * 1000 - entries()[-1]['unix_ms'] > 5000,
         'a quiet manager', 60)
    # ^ A warning is an answer that takes calls: no wait and no record, also with the 7-day window near its end.
    warn_mark.touch()
    warned = js('task', 'new', 'WORK lw.txt')['id']
    wait(lambda: kinds(warned, 'result'), 'a turn that reports allowed_warning', 40)
    assert not kinds(warned, 'limit_wait') and not limit_record.exists(), log(warned)
    js('task', 'close', warned, '--discard')
    warn_mark.unlink()
    agents_dir.mkdir(parents=True, exist_ok=True)
    now_ms = int(time.time() * 1000)
    limit_record.write_text(json.dumps({'since_ms': now_ms, 'until_ms': now_ms + 3 * 3600 * 1000,
                                        'window': 'five_hour', 'model': 'haiku'}))
    held = js('task', 'say', 'STATUS during a long limit')
    wait(lambda: probes(), lambda: 'the limit probe (state %s, agents %s, chat %s)' % (
        (manager_log.parent / 'state.json').read_text()[:600], sorted(p.name for p in agents_dir.iterdir()),
        [(e['kind'], str(e.get('text', e.get('reason', '')))[:80]) for e in entries()[-6:]]))
    assert probes()[0]['argv'][probes()[0]['argv'].index('--model') + 1] == 'haiku', probes()
    time.sleep(3)
    assert limit_record.exists() and len(probes()) == 1, probes()
    assert not [e for e in chat() if e['kind'] == 'reply' and held['id'] in e['to']], chat()[-3:]
    reset_mark.touch()
    probe_stamp.write_text(json.dumps({'unix_ms': 0}))
    replied = reply_to(held['id'], 40)
    assert not limit_record.exists() and len(probes()) == 2, (replied, probes())
    reset_mark.unlink()
    # ^ Two tasks meet a 5-hour limit (a rejected event with its window): the status names the
    # window and the reset, not an elapsed time, and the record keeps both.
    waited = [js('task', 'new', 'LIMIT 3600 WORK lr%d.txt' % i)['id'] for i in (1, 2)]
    for n in waited:
        wait(lambda: kinds(n, 'limit_wait'), 'the task waits on a 3 hour limit')
    held_status = wait(lambda: (current := show(waited[0])) and current['status']['kind'] == 'limit' and current,
                       'the limit status sentence')['status']
    assert re.fullmatch(r'Waiting for the usage limit: Claude 5-hour limit, resets \d\d:\d\d\. It goes on by itself when the account '
                        r'answers again, one task at a time\.', held_status['sentence']), held_status
    record = json.loads(limit_record.read_text())
    assert record['until_ms'] > (time.time() + 3000) * 1000 and record['window'] == 'five_hour' and record['model'], record
    # ^ The account answers again: the probe clears the record and both go on, with no message.
    reset_mark.touch()
    probe_stamp.write_text(json.dumps({'unix_ms': 0}))
    for n in waited:
        wait(lambda: kinds(n, 'result'), 'the task goes on once the limit lifts', 60)
        assert kinds(n, 'limit_wait', 'result') == ['limit_wait', 'result'], log(n)
        assert not [e for e in log(n) if e['kind'] == 'message' and e.get('mode') != 'prompt'], log(n)
        js('task', 'close', n, '--discard')
    assert not limit_record.exists()
    reset_mark.unlink()
    # ^ A stamp a day ahead: no later section sees a probe clear its own short limit.
    probe_stamp.write_text(json.dumps({'unix_ms': int((time.time() + 86400) * 1000)}))
    assert launch['actor'] == 'manager' and launch['wsx_env'] == ['WSX_AGENT_TREE', 'WSX_TASK_ACTOR', 'WSX_TASK_BROKER'], launch['wsx_env']
    assert launch['git_optional_locks'] == '0', launch
    # ^ docs/configuration.md "Profiles": the manager and every Claude worker so far run under demo's profile
    # root. Part 13t's Pi worker reads no CLAUDE_CONFIG_DIR.
    demo_launches = [t for t in traces() if t.get('launch') and t.get('adapter') != 'pi']
    assert demo_launches and all(t['claude_config_dir'] == str(profile_root) for t in demo_launches), \
        {t['claude_config_dir'] for t in demo_launches}
    # Its rules (crates/wsx/src/task/instructions.rs): root cause first; facts, not guesses; chat,
    # not reports; talk or work; a complete prompt passed on stdin; a lean review; the owner's word;
    # memory. What commands do or refuse and their options are in `--help`, not here.
    for rule in ('`wsx task <command> --help` lists the rest', 'with a quoted heredoc or `--prompt-file`',
                 "--files <glob> - <<'EOF'\n<prompt>\nEOF\n", 'never in double quotes',
                 'Root cause first. A finding is its cause, the first state that goes wrong (file:line and log proof)',
                 'plus the design step that removes it; never a task that fixes a symptom',
                 'A second fix on one concern stops: name the shared owner of that concern',
                 'Say as fact only what wsx reported or you checked', '`since then:` line',
                 "state and `checks` from the ledger", 'live only when a deploy event names the task', 'or call it unchecked',
                 'Talk like a person in a chat, not a report', 'The reader sees one screen',
                 'answer each Owner message first, every question in it', 'Define each name the owner has not used',
                 'say how both hold', 'A turn of only Events is your work', 'or with exactly `-` when nothing',
                 "never to a line 'for the owner'", 'Never hand the owner chores or ask them to wait',
                 'open with `> message <id>`', "Judge from the owner's words whether the owner wants to talk it over",
                 'no task, `add` or `steer` until the owner agrees, then one `new` or `add` with all', 'Work starts at once', 'as a mermaid diagram',
                 'Similar work or the same area goes to the open task', 'Every new task names its files',
                 'stands alone, since a worker cannot ask', 'every project and agent adapter', 'runs with `--size small`',
                 'one read-only audit task per area', '`--into <n>`', 'for lean code', "its prompt and each later add",
                 'naming each ask it missed', 'Check the fixture rule.', 'tell that worker to prove it',
                 "only when the owner's message of this turn says", '`--owner-said <id>` (on `land`, `close --discard`, `deploy`, `release`, `push`)', 'never name a message that did not ask',
                 'name the files and ask the owner; on yes, `wsx task commit <files>`', 'Read `wsx task review <n>`',
                 'Start a stall from `wsx task why <n>`.',
                 'Continue a `paused` task.', 'Never touch a task on `hand`', '`wsx task checkin`',
                 'read-only git', 'the Skill tool as `owner:<name>`', 'check rather than guess', '`wsx memory add --kind rule`',
                 '/manager/notes/decisions.md', '/manager/notes/context.md', 'update a section in place',
                 '--from-chat --reply', 'and do not answer it', "'@<project>'"):
        assert rule in launch['instructions'], rule
    # ^ No instruction names a brief or a goal file: only the owner's `chat new` takes one (docs/tasks.md "Chats").
    assert 'brief' not in launch['instructions'].lower().replace('--brief', ''), launch['instructions']
    # ^ No skill is named: the owner's skills are whatever their folder holds.
    assert re.findall(r'owner:[^<\s]', launch['instructions']) == [], launch['instructions']
    assert not any(name in launch['instructions'] for name in ('seek', 'recall', '/memo', 'scribe'))
    assert launch['cwd'] == str(project)
    # ^ The name the owner reads, trimmed; the session keeps its label `task-manager`.
    assert js('task', 'manager')['name'] == 'Atlas'
    # ^ The TUI's manager row reads the name; without one the project's (docs/tasks.md "Config").
    named = tui_text(wsx, env, project, r'task-manager\s*\n*\s*\(Atlas\)')
    assert re.search(r'task-manager\s*\n*\s*\(Atlas\)', named) and '(manager)' not in named, named[-3000:]
    config_text = (project / 'wsx.config.yml').read_text()
    (project / 'wsx.config.yml').write_text(config_text.replace('name: "  Atlas ", ', ''))
    plain = tui_text(wsx, env, project, r'task-manager\s*\n*\s*\(demo\)')
    plain_name = js('task', 'manager')['name']
    (project / 'wsx.config.yml').write_text(config_text)
    assert re.search(r'task-manager\s*\n*\s*\(demo\)', plain) and '(Atlas)' not in plain, plain[-3000:]
    assert plain_name == 'demo', plain_name
    assert js('task', 'manager')['state'] == 'running'
    # A new task and an add to the running one, each by a wsx task command the manager runs.
    started = js('task', 'say', 'RUN wsx task new "WORK s1.txt" -p demo --json')
    assert 'ran: wsx task new' in reply_to(started['id'])['text']
    manager_task = max(row['id'] for row in js('task', 'list')['tasks'])
    assert wait_state(manager_task, 'ready', 'failed')['state'] == 'ready'
    # ^ tasks.skills: owner lends the worker the owner's skills; tasks.workflow.build is its rule.
    skilled = [t for t in traces() if t.get('launch') and not t.get('manager')][-1]
    assert skilled['plugin_dir'].endswith('/%d/skills-plugin' % manager_task), skilled
    assert 'Run the fixture rule.' in skilled['instructions'] and 'as `owner:<name>`' in skilled['instructions']
    # ^ No tasks.skillsOnly: the worker gets every skill. With it: only the named ones, and a
    # name the owner lacks is skipped (docs/tasks.md "Workflow").
    assert os.readlink(Path(skilled['plugin_dir']) / 'skills') == str(owner_skills), skilled
    unrestricted = (project / 'wsx.config.yml').read_text()
    configure(unrestricted + '  skillsOnly: [scribe, ghost]\n', 'skills only')
    only_task = js('task', 'new', 'WORK only.txt')['id']
    assert wait_state(only_task, 'ready', 'failed')['state'] == 'ready'
    only = [t for t in traces() if t.get('launch') and not t.get('manager')][-1]
    only_dir = Path(only['plugin_dir']) / 'skills'
    assert only_dir.is_dir() and not only_dir.is_symlink() and sorted(os.listdir(only_dir)) == ['scribe'], os.listdir(only_dir)
    assert os.readlink(only_dir / 'scribe') == str(owner_skills / 'scribe')
    # ^ `task new --skills` replaces tasks.skillsOnly for one task; `owner:<name>` names it too.
    picked = js('task', 'new', 'WORK picked.txt', '--skills', 'owner:other')['id']
    assert wait_state(picked, 'ready', 'failed')['state'] == 'ready'
    assert show(picked)['record']['skills'] == ['other'], show(picked)['record']
    picked_dir = Path([t for t in traces() if t.get('launch') and not t.get('manager')][-1]['plugin_dir']) / 'skills'
    assert sorted(os.listdir(picked_dir)) == ['other'], os.listdir(picked_dir)
    js('task', 'close', picked, '--discard')
    assert 'is not a skill name' in cli('task', 'new', 'WORK bad.txt', '--skills', 'owner:../x', ok=False).stderr
    configure(unrestricted, 'skills all')
    added = js('task', 'say', 'RUN wsx task add %d "WORK s2.txt" -p demo --json' % running)
    assert '-> 0' in reply_to(added['id'])['text']
    # Any command shape reaches the broker: `;`, a heredoc with a multi-line prompt, a pipe.
    script = js('task', 'say', "SCRIPT\nwsx task ledger -p demo --json >/dev/null; wsx task new -p demo --json - <<'EOF'\n"
                "WORK h1.txt\nsecond line of the prompt\nEOF\nwsx task list -p demo --json | grep -c '\"id\"' >/dev/null && echo piped")
    answer = reply_to(script['id'])['text']
    assert answer.startswith('script -> 0') and answer.rstrip().endswith('piped'), answer
    heredoc_task = max(row['id'] for row in js('task', 'list')['tasks'])
    first = next(e for e in log(heredoc_task) if e['kind'] == 'message')
    assert first['text'] == 'WORK h1.txt\nsecond line of the prompt', first
    assert wait_state(heredoc_task, 'ready', 'failed')['state'] == 'ready'
    # Under the broker no other command runs, neither in the CLI nor at the host.
    refused = js('task', 'say', 'RUN wsx --version\nRUN wsx task host %s 1' % launch['cwd'])
    text = reply_to(refused['id'])['text']
    assert 'ran: wsx --version -> 2 wsx: only `wsx task` and `wsx memory` commands run here' in text, text
    assert 'ran: wsx task host' in text and "-> 2 wsx: `wsx task host` is not for the manager" in text, text
    # The manager's `wsx memory` runs at its host too (docs/memory.md "Agents"): a rule with the owner's word is its own.
    (work / 'manager-rule.txt').write_text('The manager writes the rules.\n')
    text = reply_to(js('task', 'say', 'RUN wsx memory add --kind rule --slug manager-rule --text-file %s --source msg:1 '
                       '--verified-by owner:1 --json' % (work / 'manager-rule.txt'))['id'])['text']
    assert '-> 0 {"slug":"manager-rule","kind":"rule"' in text, text
    assert [h['slug'] for h in memory_records(cli('memory', 'search', 'rules', '--json').stdout)] == ['manager-rule']
    with socket.socket(socket.AF_UNIX) as raw:
        raw.connect(launch['broker'])
        raw.sendall(json.dumps({'argv': ['daemon', 'stop'], 'stdin': None, 'cwd': str(project)}).encode())
        try:
            raw.shutdown(socket.SHUT_WR)
        except OSError:
            pass  # ^ The broker refuses at the first line and may close before this half-close (macOS: ENOTCONN).
        direct = json.loads(raw.makefile().read())
    assert direct['code'] == 2 and 'only `wsx task` commands' in direct['stderr'], direct
    assert wait_state(running, 'ready', 'failed')['state'] == 'ready'
    assert [e['mode'] for e in log(running) if e['kind'] == 'message'] == ['prompt', 'add']
    manager_log = Path(created['task_dir']).parent / 'manager' / 'log.jsonl'

    def manager_entries(raw=False):
        found = [json.loads(line) for line in manager_log.read_text().splitlines()]
        return found if raw else [unquoted(e) for e in found]

    def manager_idle():
        entries = manager_entries()
        delivered = {n for e in entries if e['kind'] == 'delivered' for n in e['ids']}
        turns = [e['kind'] for e in entries if e['kind'] in ('delivered', 'reply')]
        # ^ A quiet event waits in the queue for a turn that runs anyway (Short::any_wakes).
        waking = [e for e in entries if e['kind'] == 'message'
                  and (e['from'] != 'events' or not e.get('short') or any(s.get('wakes', True) for s in e['short']))]
        return all(e['id'] in delivered for e in waking) and turns[-1:] != ['delivered']

    def manager_settled(why):
        # ^ An event line is logged as a message 3 s after the last one: idle, then idle again after that.
        wait(manager_idle, why, 60)
        time.sleep(3.5)
        wait(manager_idle, why, 60)

    # Usage (docs/tasks.md "Manager"): rotation looks at the last main model call's context, not
    # the result's sum over the turn's calls, nor a subagent's; a reply's cost is its own, not the
    # process's running `total_cost_usd`.
    summed = js('task', 'say', 'SUMMED')
    usage = reply_to(summed['id'])['usage']
    # ^ A post (the sign-in alert above) has no usage: only the agent's replies count.
    replies = [e for e in manager_entries() if e['kind'] == 'reply' and not e.get('post')]
    assert usage['input_tokens'] + usage['cache_read_tokens'] == 200000 and usage['context_tokens'] == 50000, usage
    assert usage['model'] == 'fake-manager' and usage['adapter'] == 'claude', usage
    # Quotes (docs/tasks.md "Manager"): a reply that opens with `> message <id>` is stored with
    # that message's first line in the owner's quote format; a bad or unknown id drops the line and
    # the reply stands. The next turn names the id of the manager's last reply.
    promise = reply_to(js('task', 'say', 'STATUS promise')['id'])
    own = reply_to(js('task', 'say', 'QUOTE %d still waiting on #8' % promise['id'])['id'])
    assert own['text'] == '> %s\n\nstill waiting on #8' % promise['text'].splitlines()[0], own
    # ^ The entry keeps the quoted message's id, for a tap that opens the chat there at any age.
    assert own['quote'] == promise['id'], own
    assert 'Your last reply: %d.' % promise['id'] in turn_with('QUOTE %d still' % promise['id'])['message']
    # ^ wsx quotes the owner message a reply answers when the reply opens with no quote of its own
    # and will not sit under it (docs/tasks.md "A quote", `chat::quoted_message`): the turn
    # held several owner messages (the sign-in turn above: the newest), or a chat item was logged
    # after the message. One a manager wrote wins.
    mine = raw_reply(js('task', 'say', 'QUOTE %d keeps mine' % promise['id'])['id'])
    assert mine['quote'] == promise['id'] and mine['text'].startswith('> %s\n\nkeeps mine' % promise['text'].splitlines()[0]), mine
    # ^ A task event logged during its turn would sit between: the events of earlier tasks land first.
    manager_settled('the events before one owner message')
    one = js('task', 'say', 'STATUS one owner message')
    own_words = raw_reply(one['id'])
    assert own_words['to'] == [one['id']] and 'quote' not in own_words, own_words
    # ^ A reply under its one message is no quote; one with a chat item between is. A message that
    # comes in during a long turn waits for the next turn and sits between the first and its reply.
    apart = js('task', 'say', 'ASK LONG apart')
    wait(lambda: any(e['kind'] == 'delivered' and apart['id'] in e['ids'] for e in manager_entries()), 'apart delivered')
    second = js('task', 'say', 'STATUS second')
    far = raw_reply(apart['id'], 60)
    assert second['id'] < far['id'] and far['to'] == [apart['id']] and far['quote'] == apart['id'], (second, far)
    assert far['text'].startswith('> ASK LONG apart\n\n') and far.get('asks') is True, far
    assert far['id'] in js('task', 'chat')['asks'], js('task', 'chat')['asks']
    js('task', 'dismiss', str(far['id']))
    reply_to(second['id'], 60)
    # ^ A follow-up to the manager's own earlier reply: a reply to an events turn that reports task
    # #n quotes the newest earlier reply that names #n (a promise) when other chat items lie
    # between, and none when the report sits right under it (`chat::quoted_message`). The task
    # waits for a release file, so the promise comes first.
    def event_of(task):
        return next((e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                     and '#%d ready r1' % task in e['text']), None)

    def follow_up(label, between_words):
        release = work / ('release-' + label)
        task = js('task', 'new', 'NOOP ASK WAIT %s' % release)['id']
        pledge = reply_to(js('task', 'say', 'BLOCKS I will land #%d once CI passes.' % task)['id'])
        assert '#%d once CI passes' % task in pledge['text'], pledge
        if between_words:
            reply_to(js('task', 'say', between_words)['id'])
        release.write_text('go')
        event = wait(lambda: event_of(task), 'the ready event of the promised task', 60)
        report = raw_reply(event['id'])
        assert report.get('asks') is True, report
        items = [e for e in manager_entries(raw=True)
                 if pledge['id'] < (e.get('id') or 0) < event['id'] and e['kind'] in ('message', 'reply')]
        js('task', 'dismiss', str(report['id']))
        js('task', 'close', task, '--discard')
        return pledge, report, items

    pledge, report, items = follow_up('direct', None)
    assert not items and 'quote' not in report, (items, report)
    pledge, report, items = follow_up('apart', 'QUOTE OWNER_SAID between')
    assert items and report['quote'] == pledge['id'], (pledge, report)
    assert report['text'].startswith('> I will land #') and report['text'].endswith('Should I land it now?'), report
    heard = js('task', 'say', 'QUOTE OWNER_SAID noted')
    assert raw_reply(heard['id'])['text'] == '> QUOTE OWNER_SAID noted\n\nnoted'
    # ^ A quote of a quote is the words of that reply, not its quote line.
    again = reply_to(js('task', 'say', 'QUOTE %d and again' % own['id'])['id'])
    assert again['text'] == '> still waiting on #8\n\nand again', again
    for bad in ('abc', '0', '99999999', '99999999999999999999', '-3'):
        dropped = reply_to(js('task', 'say', 'QUOTE %s words stay' % bad)['id'])
        assert dropped['text'] == 'words stay' and 'quote' not in dropped, (bad, dropped)
    # ^ The TUI shows the quote as one line; `[` selects it and Enter goes to the quoted message,
    # the owner's first one, far above.
    first = reply_to(js('task', 'say', 'QUOTE 1 jump')['id'])
    assert first['text'] == '> STATUS\n\njump' and first['quote'] == 1, first
    jumped = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'),
        (r'(?m)^   \u258e STATUS$', b'['),
        (r'(?m)^ MANAGER +Enter: open', b'\r'),
        (r'(?m)^ You +\d\d:\d\d\n   STATUS$', b'\x1b'),
        (r'(?m)^ MANAGER +\(i\)message', b'\x1b'), (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')])
    assert 'jump' in jumped[2] and 'jump' not in jumped[4], jumped[4]
    # ^ After the jump: the owner's Reply (web *Reply*, TUI `r`) sends `> message <id>` too: stored in the same
    # format with the id, and the agent reads the line.
    said_back = js('task', 'say', '> message 1\n\nthe owner answers')
    stored = next(e for e in manager_entries() if e['kind'] == 'message' and e['id'] == said_back['id'])
    assert stored['text'] == '> STATUS\n\nthe owner answers' and stored['quote'] == 1, stored
    reply_to(said_back['id'])
    assert not any(e['kind'] == 'rotated' for e in manager_entries()), 'four 50k calls do not rotate'
    assert len(replies) > 3 and {e['usage']['cost_micro_usd'] for e in replies} == {1000}, replies
    # Event-only turns run on tasks.manager.eventModel unless a ready task the manager created
    # waits for its review: the manager's own model reviews it.
    review = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                                and '#%d ready' % manager_task in e['text']), None), 'review event')
    review_turn = wait(lambda: turn_with('Events (message %d)' % review['id']), 'review turn')
    assert review_turn['model'] == 'fake-manager', review_turn
    # Rotation: past rotateTokens the next turn starts a fresh conversation.
    # ^ Without a skills folder the next launch has no plugin, keeps the Skill tool and
    # the generic guidance.
    shutil.rmtree(owner_skills)
    big = js('task', 'say', 'BIG')
    big_reply = reply_to(big['id'])
    assert wait(lambda: any(e['kind'] == 'rotated' for e in chat()), 'rotated')
    after_rotation = js('task', 'say', 'STATUS')
    reply_to(after_rotation['id'])
    # ^ A rotation after a reply took no waiting message: its divider keeps its place, under that
    # reply and above the next message (`chat::chat_window`, docs/tasks.md "Manager").
    shown = [(e['kind'], e.get('id')) for e in js('task', 'chat')['entries']]
    divider = max(at for at, (kind, _) in enumerate(shown) if kind == 'rotated')
    assert shown.index(('reply', big_reply['id'])) < divider < shown.index(('message', after_rotation['id'])), shown[-8:]
    manager_launches = [t for t in traces() if t.get('launch') and t.get('manager')]
    assert not manager_launches[-1]['resume'] and manager_launches[-1]['session'] != launch['session']
    bare = manager_launches[-1]
    assert bare['plugin_dir'] is None, bare
    assert 'the Skill tool as `owner:<name>`' not in bare['instructions'] and 'check rather than guess' in bare['instructions']
    assert [e['id'] for e in chat(after=big['id']) if e['kind'] == 'message' and e['from'] == 'owner'] == [after_rotation['id']]
    for n in (running, manager_task, heredoc_task, only_task):
        js('task', 'close', n, '--discard')

    def events_with(*needles):
        return [e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                and all(needle in e['text'] for needle in needles)]

    def stalls(n):
        return [line for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                for line in e['text'].split('\n') if line.startswith('#%d stalled' % n)]

    # 20s. Stale worktrees (docs/tasks.md "Stale worktrees"): the manager host's sweep, every 3 s
    # here, closes idle sessions and removes an idle worktree that is clean and merged; a dirty
    # one is only flagged, once; a worktree whose session still prints and the main checkout's
    # session stay; the news start no turn. Idle is 3 s and removal 6 s (the config says days).
    mark('20s')
    manager_config = (project / 'wsx.config.yml').read_text()
    configure(manager_config + '  prune: { idleDays: 0.00003, removeDays: 0.00007 }\n', 'a quick prune')
    for name in ('stale-clean', 'stale-dirty', 'stale-busy'):
        git('worktree', 'add', '-q', '-b', name, str(base / name))
    (base / 'stale-dirty/edited.txt').write_text('mine\n')
    keep = js('session', 'create', '--name', 'keep-main', '-p', 'demo')['session_id']
    for name in ('stale-clean', 'stale-dirty'):
        js('session', 'create', '--name', name, '--worktree', name, '-p', 'demo')
    # ^ A background job prints to the terminal: the session is not idle while it does.
    js('session', 'create', '--name', 'stale-busy', '--worktree', 'stale-busy', '-p', 'demo',
       '--command', '(while true; do echo tick; sleep 1; done) &')
    sessions_in = lambda name: [s for s in daemon_call('snapshot')['data']['sessions'] if s['label'] == name]
    # ^ The host logs event lines together, 3 s after the last: a message holds one or more lines.
    pruned_events = lambda: [e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                             and any(line.startswith('pruned: ') for line in e['text'].split('\n'))]
    wait(lambda: not (base / 'stale-clean').exists(), 'the idle clean merged worktree is removed', 60)
    removed = wait(lambda: [e for e in pruned_events() if 'removed worktree stale-clean' in e['text']], 'the removal is posted', 30)
    assert len(removed) == 1 and not any(short['wakes'] for short in removed[0]['short']), pruned_events()
    assert not sessions_in('stale-clean'), 'the removed worktree keeps no session'
    wait(lambda: not sessions_in('stale-dirty'), 'the idle session of the dirty worktree is closed', 30)
    assert (base / 'stale-dirty/edited.txt').read_text() == 'mine\n', 'a dirty worktree is not touched'
    time.sleep(9)
    flagged = [e for e in pruned_events() if 'worktree stale-dirty is idle and kept' in e['text']]
    assert len(flagged) == 1 and '1 uncommitted file (edited.txt)' in flagged[0]['text'], pruned_events()
    assert not any(short['wakes'] for short in flagged[0]['short'])
    assert (base / 'stale-busy').is_dir() and sessions_in('stale-busy'), 'a session that prints is not idle'
    assert [s for s in daemon_call('snapshot')['data']['sessions'] if s['id'] == keep], 'the main checkout keeps its session'
    ids = {e['id'] for e in pruned_events()}
    assert not [e for e in manager_entries() if e['kind'] == 'reply' and ids & set(e['to'])], 'the news started a turn'
    # ^ The ledger lists what is left (docs/tasks.md "Ledger").
    strays = {Path(row['path']).name for row in js('task', 'ledger')['stray_worktrees']}
    assert {'stale-dirty', 'stale-busy'} <= strays and 'stale-clean' not in strays, strays
    configure(manager_config, 'restore the config')
    for name in ('stale-dirty', 'stale-busy'):
        for session in sessions_in(name):
            cli('session', 'delete', name, '-p', 'demo')
        git('worktree', 'remove', '--force', str(base / name))
    cli('session', 'delete', 'keep-main', '-p', 'demo')
    git('branch', '-D', 'stale-clean', 'stale-dirty', 'stale-busy', ok=False)
    # 20k. A process that left the agent's group and cleared its environment (a daemon the worker
    # started) is found by its executable's path under the working copy: `close` stops it; the
    # manager host's sweep stops one whose copy is gone, and one left in a closed task's folder;
    # a process outside every copy stays (docs/tasks.md "Close", "Stale worktrees").
    mark('20k')
    configure('tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  agent:\n    model: fake-model\n', 'tasks without checks')
    escaped = lambda seen: [t for t in traces()[seen:] if 'escape' in t]
    seen = len(traces())
    closing = js('task', 'new', 'NOOP ESCAPE')['id']
    wait_state(closing, 'ready', 'failed')
    first = wait(lambda: escaped(seen), 't%s escaped' % closing)[0]
    # ^ The same helper, outside every copy, is the bystander the stops must leave.
    helper = work / 'escape-helper'
    shutil.copy2(first['bin'], helper)
    bystander = subprocess.Popen([str(helper)], env={}, start_new_session=True)
    copy_a = Path(show(closing)['record']['working_copy'])
    assert not gone(first['escape']) and (copy_a / 'escape-bin').is_file()
    closed = js('task', 'close', closing, '--discard')
    # ^ The host marks the task closed before the close command removes the copy, and the sweep
    # (every 3 s here) may stop the helper in that gap: then it posts the stop, else `close` names it.
    by_close = [one['pid'] for one in closed['processes_stopped']]
    assert by_close in ([first['escape']], []), closed
    if by_close:
        assert os.path.realpath(closed['processes_stopped'][0]['path']) == os.path.realpath(copy_a / 'escape-bin'), closed
        assert any(e['kind'] == 'agent' and 'close stopped 1 process' in e['text'] for e in log(closing)), log(closing)
    else:
        wait(lambda: [e for e in pruned_events() if 'task #%s' % closing in e['text'] and 'stopped 1 process that outlived' in e['text']],
             'the sweep posted the stop', 30)
    wait(lambda: gone(first['escape']), 'the escaped helper is stopped', 10)
    assert not copy_a.exists() and bystander.poll() is None, 'close left the copy or stopped the bystander'
    # ^ The sweep: a task whose copy was removed outside close.
    seen = len(traces())
    orphan = js('task', 'new', 'NOOP ESCAPE')['id']
    wait_state(orphan, 'ready', 'failed')
    second = wait(lambda: escaped(seen), 't%s escaped' % orphan)[0]
    copy_b = Path(show(orphan)['record']['working_copy'])
    js('task', 'stop', orphan)
    shutil.rmtree(copy_b)
    git('worktree', 'prune')
    wait(lambda: gone(second['escape']), 'the sweep stopped the helper of the removed copy', 40)
    stopped = wait(lambda: [e for e in pruned_events() if 'task #%s' % orphan in e['text'] and 'stopped 1 process that outlived' in e['text']],
                   'the stop is posted', 30)
    # ^ The message may also carry the stop's own event lines, which wake: only the pruned line is quiet.
    quiet = [short for short in stopped[0]['short'] if short['text'].startswith('pruned: stopped')]
    assert len(stopped) == 1 and quiet and not any(short.get('wakes', True) for short in quiet), pruned_events()
    # ^ A process left in the folder of a closed task (the situation of a copy that outlived its close).
    copy_a.mkdir()
    shutil.copy2(helper, copy_a / 'left')
    left = subprocess.Popen([str(copy_a / 'left')], env={}, start_new_session=True)
    wait(lambda: left.poll() is not None, 'the sweep stopped the helper in the closed task folder', 40)
    left.wait()
    assert bystander.poll() is None, 'a process outside every copy was stopped'
    # ^ Why only the path finds them: a wsxd started by a process that carries the agent's mark keeps it,
    # so a mark cleared by the caller (and the group left by setsid) is the whole escape.
    # ^ A daemon's state file and lock lie beside its socket: its own folder keeps it from the scenario's.
    probe = {'WSX_SOCKET': str(work / 'd2/d.sock'), 'XDG_STATE_HOME': str(work / 'state2'), 'WSX_AGENT_TREE': 'probe-20k'}
    (work / 'state2').mkdir(exist_ok=True)
    (work / 'd2').mkdir(mode=0o700, exist_ok=True)
    started = subprocess.run([str(wsx), 'session', 'list', '--json'], cwd=project, env={**env, **probe},
                             capture_output=True, text=True, timeout=60)
    assert started.returncode == 0, (started.stdout, started.stderr)
    def environment(pid):
        # ^ macOS: the arguments block of KERN_PROCARGS2 (argv, then the environment); Linux: /proc.
        if sys.platform != 'darwin':
            try:
                return Path('/proc/%d/environ' % pid).read_bytes()
            except OSError:
                return b''
        import ctypes
        buffer, size = ctypes.create_string_buffer(1 << 20), ctypes.c_size_t(1 << 20)
        if ctypes.CDLL(None).sysctl((ctypes.c_int * 3)(1, 49, pid), 3, buffer, ctypes.byref(size), None, 0):
            return b''
        return buffer.raw[:size.value]
    listed = subprocess.run(['ps', '-axo', 'pid=,command='], capture_output=True, text=True).stdout
    daemons = [int(line.split()[0]) for line in listed.splitlines() if 'wsxd' in line and 'ps -axo' not in line]
    mine = [pid for pid in daemons if probe['WSX_SOCKET'].encode() in environment(pid)]
    assert len(mine) == 1 and b'WSX_AGENT_TREE=probe-20k' in environment(mine[0]), (daemons, mine)
    os.kill(mine[0], 15)
    bystander.kill()
    bystander.wait()
    git('branch', '-D', 'wsx/t%s' % orphan, ok=False)
    js('task', 'close', orphan, '--discard')
    shutil.rmtree(copy_a, ignore_errors=True)
    configure(manager_config, 'restore the config again')
    globals().update(locals())


def probe_up(created):
    """The boundary probe of section 23: its files, script and the process its signal must not reach."""
    secret = home / '.ssh/id_fixture'

    secret.parent.mkdir(mode=0o700, exist_ok=True)

    secret.write_text('secret\n')

    store_dir, common = Path(created['task_dir']).parent, project / '.git'

    probe_script = work / 'probe.py'

    probe_script.write_text(PROBE_SCRIPT)

    # ^ A process outside the boundary that the probe's signal must not reach.
    signal_target = subprocess.Popen(['sleep', '600'])

    atexit.register(signal_target.kill)

    def probe_files(n):
        """The probe's spec and answer for task n, which is created next."""
        spec = {'read': {'read_ssh': str(secret)}, 'signal_outside': signal_target.pid,
                'write': {'write_ssh': str(home / '.ssh/probe'), 'write_state': str(store_dir / 'probe.txt'),
                          'write_existing_copy': str(base / 'project-t998/probe.txt'),
                          'write_later_copy': str(base / 'project-t999/probe.txt'),
                          'write_main_ref': str(common / 'refs/heads/main'),
                          'write_main_lock': str(common / 'refs/heads/main.lock'),
                          'write_main_reflog': str(common / 'logs/refs/heads/main'),
                          'write_hooks': str(common / 'hooks/pre-commit'), 'write_config': str(common / 'config'),
                          'write_routines': str(work / 'routines/projects/probe.json'),
                          'write_own_copy': str(base / ('project-t%d' % n) / 'probe.txt'),
                          'write_task_folder': str(store_dir / str(n) / 'probe.txt'),
                          'write_tmpdir': str(tmp / 'probe.txt'), 'write_cargo': str(home / '.cargo/registry/probe.txt'),
                          'write_library': str(home / 'Library/Caches/wsx-probe/probe.txt')}}
        spec_file, answer = work / ('probe-%d.json' % n), work / ('probe-%d-answer.json' % n)
        spec_file.write_text(json.dumps(spec))
        return spec_file, answer

    globals().update(locals())


def fresh_project():
    """The project as section 17 leaves it, for a part that starts after it: the deploy config and a
    first task, whose folder names the project store."""
    configure(DEPLOY_CONFIG, 'deploy config')
    created = js('task', 'new', 'WORK a.txt')  # ^ the task's check wants a.txt
    wait_state(created['id'], 'ready', 'failed')
    js('task', 'close', created['id'], '--discard')
    return created


def part_core(created=None):
    """Sections 0 to 21c: the task journey, landing, checks, and the manager from its start to its notes."""
    # 0. The TUI's Tasks view (`t`) of a project without tasks (docs/tasks.md "TUI").
    mark('0')
    empty, = tui_screens(wsx, env, project, [(r'WORKSPACE', b't'), (r'No tasks', b'')])[1:]
    assert re.search(r'Tasks demo +0 to check  0 approvals', empty) and 'TASKS' in empty, empty

    # 1. New: a working turn in its own working copy.
    mark('1')
    created = js('task', 'new', 'SLOW WORK a.txt')
    assert created['id'] == 1 and created['state'] == 'working', created
    copy = Path(created['working_copy'])
    assert copy == base / 'project-t1' and created['branch'] == 'wsx/t1'
    assert (copy / 'target/warm').read_text() == 'cached'
    assert not (copy / 'target/debug/incremental').exists() and (deps / 'foo-00000000000000bb').exists()
    copy_deps = copy / 'target/debug/deps'
    assert sorted(p.name for p in copy_deps.iterdir()) == sorted(p.name for p in deps.iterdir())
    # ^ The copy's own build of foo: other settings, another hash.
    (copy_deps / 'foo-00000000000000cc').write_text('rebuilt')
    assert not (copy / 'scratch.txt').exists()
    assert (copy / 'README').stat().st_mtime == 1_600_000_000, (copy / 'README').stat().st_mtime
    assert (copy / '.git').is_file() and git('rev-parse', '--abbrev-ref', 'HEAD', cwd=copy) == 'wsx/t1'
    assert git('rev-parse', '--git-dir', cwd=copy) != git('rev-parse', '--git-dir')
    task_dir = Path(created['task_dir'])
    assert oct(task_dir.stat().st_mode & 0o777) == '0o700'
    host_socket = Path(show(1)['socket'])
    assert len(str(task_dir / 'manager/manager.sock')) > 104 and len(str(host_socket)) < 104, host_socket
    assert oct(host_socket.stat().st_mode & 0o777) == '0o600'
    assert host_socket.parent == tmp / ('wsx-%d' % os.getuid()), host_socket
    assert oct(host_socket.parent.stat().st_mode & 0o777) == '0o700'
    launch = wait(lambda: next((t for t in traces() if t.get('launch')), None), 'fake launch')
    # ^ One wsx variable: the host's broker for `wsx task check-run`.
    assert launch['wsx_env'] == ['WSX_AGENT_TREE', 'WSX_TASK_BROKER'] and launch['schema']
    assert launch['model'] == 'fake-model' and not launch['resume'] and launch['mode'] == 'bypassPermissions'
    assert not {'--allowedTools', '--disallowedTools'} & set(launch['argv']), launch['argv']
    assert "put in the result's memo a short, lean memo of durable" in launch['instructions'], launch['instructions']
    assert 'memo' in launch['schema_required'], launch['schema_required']
    # One permission model (docs/tasks.md "Permissions"): wsx's boundary, no agent settings for it.
    assert launch['settings'] == {'attribution': {'commit': '', 'pr': ''}, 'includeCoAuthoredBy': False}, launch

    # 2. Steer during the turn: Claude does not save it, so the host logs it.
    mark('2')
    steered = js('task', 'steer', '1', 'use capitals')
    assert steered['ok'] and 'steered' in steered['message'], steered
    ready = wait_state(1, 'ready', 'failed')
    assert ready['state'] == 'ready', ready
    assert any(t.get('steer_in_turn') == 'use capitals' for t in traces())
    assert [e['mode'] for e in log(1) if e['kind'] == 'message'] == ['prompt', 'steer']
    assert ready['result']['checks'] and all(c['exit'] == 0 for c in ready['result']['checks'])
    assert sorted(p.name for p in copy_deps.iterdir()) == [
        'bar-00000000000000aa.c1.new.rcgu.o', 'foo-00000000000000cc', 'libbar-00000000000000aa.rlib']
    assert (deps / 'foo-00000000000000bb').exists(), 'main is never pruned'
    commit = git('rev-parse', 'refs/wsx/task/1/r1')
    assert commit == git('rev-parse', 'wsx/t1')
    report = Path(ready['report']).read_text()
    sections = [line for line in report.splitlines() if line.startswith('## ')]
    assert sections == ['## Summary', '## Changes', '## Evidence', '## To check', '## Left open'], sections
    assert 'Decided: fake choice (fixture).' in report and '2 checks passed' in report, report
    assert '`test -f a.txt`: exit 0' in report and 'add a.txt' in report
    assert '- the fixture ran: `fake-claude-stream.py`' in report and 'Nothing needs a person.' in report
    agent = wait(lambda: (a := pane_agent(1)) and a['state'] == 'done' and a, 'agent done')
    assert agent['provider'] == 'task'

    # 3. Add after the turn, blocked on a denied tool, then an answer resumes the same conversation.
    mark('3')
    added = js('task', 'add', '1', 'NEEDS WebSearch WORK b.txt')
    assert added['ok'] and 'new turn' in added['message'], added
    blocked = wait_state(1, 'waiting')
    assert blocked['record']['waiting']['on'] == 'owner', blocked['record']
    assert any(e['kind'] == 'blocked' and e['denials'] == ['Bash(WebSearch)'] for e in log(1)), log(1)
    wait(lambda: (pane_agent(1) or {}).get('state') == 'blocked', 'agent blocked')
    assert blocked['revision'] == 1
    assert js('task', 'add', '1', 'WORK b.txt')['ok']
    ready = wait_state(1, 'ready', 'failed')
    assert ready['state'] == 'ready' and ready['revision'] == 2, ready
    assert {t['session'] for t in traces() if 'message' in t and not t.get('manager')} == {launch['session']}

    # 3b. Denials the worker worked around do not block: checks run and the denials are evidence.
    mark('3b')
    js('task', 'add', '1', 'DENIED rm-tmp WORK d.txt')
    ready = wait_state(1, 'ready', 'failed', 'waiting')
    assert ready['state'] == 'ready', ready
    assert ready['result']['denials'] == ['Bash(rm-tmp)'], ready['result']
    report = Path(ready['report']).read_text()
    assert report.index('`Bash(rm-tmp)`') > report.index('## Evidence'), report

    # 3c. A stdout line past the reader's 4 MiB bound is dropped and the turn still ends.
    mark('3c')
    js('task', 'add', '1', 'HUGE NOOP')
    assert wait_state(1, 'ready', 'failed')['state'] == 'ready'
    assert not any(len(e.get('text', '')) > 1000 for e in log(1) if e['kind'] == 'agent')

    # 4. A failing check makes the task failed with the evidence.
    mark('4')
    js('task', 'add', '1', 'WORK broken.txt')
    failed = wait_state(1, 'failed')
    assert [c['exit'] for c in failed['result']['checks']] == [0, 1], failed['result']
    assert '`test ! -e broken.txt`: exit 1' in Path(failed['report']).read_text()
    git('rm', '-q', 'broken.txt', cwd=copy)
    git('commit', '-q', '-m', 'drop broken', cwd=copy)

    # 5. Live daemon handoff while the host runs a turn.
    mark('5')
    js('task', 'add', '1', 'SLOW WORK c.txt')
    wait_state(1, 'working')
    before = daemon_call('lifecycle_status')['data']['epoch']
    hosts_before = leftovers('task host ' + show(1)['record']['project_id'])
    restarted = json.loads(cli('daemon', 'restart', '--json').stdout)
    assert restarted['state'] == 'restarted', restarted
    wait(lambda: daemon_call('lifecycle_status')['data']['epoch'] != before, 'handoff')
    ready = wait_state(1, 'ready', 'failed')
    assert ready['state'] == 'ready' and ready['host_running'], ready
    assert leftovers('task host ' + ready['record']['project_id']) == hosts_before
    wait(lambda: (pane_agent(1) or {}).get('state') == 'done', 'agent report after handoff')

    # 6. Stop interrupts; the next add resumes the same conversation.
    mark('6')
    js('task', 'add', '1', 'SLOW NOOP')
    wait_state(1, 'working')
    stopped = js('task', 'stop', '1')
    assert stopped['ok'] and stopped['state'] == 'waiting', stopped
    assert show(1)['record']['waiting'] == {'on': 'owner', 'reason': 'stopped by request'}
    assert any(t.get('interrupted') for t in traces())
    js('task', 'add', '1', 'NOOP')
    wait_state(1, 'ready', 'failed')
    last = [t for t in traces() if t.get('launch')][-1]
    assert last['resume'] and last['session'] == launch['session']

    # 7. Close refuses unlanded commits and changes nothing.
    mark('7')
    refused = cli('task', 'close', '1', ok=False)
    assert 'not on main' in refused.stderr and '--discard' in refused.stderr, refused.stderr
    assert show(1)['host_running'] and copy.exists()

    # 8. A task without commits closes without --discard; nothing is left.
    mark('8')
    second = js('task', 'new', 'NOOP')
    assert second['id'] == 2
    wait_state(2, 'ready', 'failed')
    copy2 = Path(second['working_copy'])
    socket2 = Path(show(2)['socket'])
    closed = js('task', 'close', '2')
    assert closed['state'] == 'closed' and closed['working_copy_removed'], closed
    project_id = show(1)['record']['project_id']
    assert not copy2.exists() and not git('branch', '--list', 'wsx/t2')
    assert not socket2.exists()
    assert not leftovers('task host ' + project_id + ' 2')

    # 9. Close with --discard removes everything of task 1, including its revision refs.
    mark('9')
    closed = js('task', 'close', '1', '--discard')
    assert closed['discarded'] and closed['state'] == 'closed', closed
    assert not copy.exists() and not git('branch', '--list', 'wsx/t1')
    assert not git('for-each-ref', 'refs/wsx/task/1/')
    assert not leftovers('task host ' + project_id) and not leftovers(str(fake_script))
    sessions = daemon_call('snapshot')['data']['sessions']
    assert not [s for s in sessions if s['label'].startswith('task-')], sessions
    assert 'worktree' not in git('worktree', 'list', '--porcelain').replace('worktree ' + str(project), '')
    assert js('task', 'list')['tasks'] == [] and len(js('task', 'list', '--all')['tasks']) == 2
    assert log(1)[-1]['kind'] == 'closed'

    # 10. A failed start leaves nothing and does not reuse the number.
    mark('10')
    (base / 'project-t3').mkdir()
    blocked_path = cli('task', 'new', 'NOOP', ok=False)
    assert 'already exists' in blocked_path.stderr and 'not reused' in blocked_path.stderr, blocked_path.stderr
    assert not (task_dir.parent / '3').exists() and not git('branch', '--list', 'wsx/t3')
    (base / 'project-t3').rmdir()
    fourth = js('task', 'new', 'NOOP')
    assert fourth['id'] == 4, fourth
    wait_state(4, 'ready', 'failed')
    js('task', 'close', '4')

    # 11. --from starts at another commit and records it; a bad ref leaves nothing.
    mark('11')
    first = git('rev-parse', 'main')
    (project / 'later.txt').write_text('later')
    git('add', 'later.txt')
    git('commit', '-q', '-m', 'later')
    assert 'not a commit' in cli('task', 'new', 'NOOP', '--from', 'no-such-ref', ok=False).stderr
    assert not (task_dir.parent / '5').exists() and not (base / 'project-t5').exists()
    sixth = js('task', 'new', 'NOOP', '--from', first)
    assert sixth['id'] == 6 and sixth['base'] == first, sixth
    assert not (Path(sixth['working_copy']) / 'later.txt').exists()
    wait_state(6, 'ready', 'failed')
    js('task', 'close', '6')

    # 12. Two concurrent starts get distinct numbers and both work.
    mark('12')
    starts = [subprocess.Popen([str(wsx), 'task', 'new', 'NOOP', '--json'], cwd=project, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for _ in range(2)]
    outputs = [start.communicate(timeout=60) for start in starts]
    assert all(start.returncode == 0 for start in starts), outputs
    ids = sorted(json.loads(out)['id'] for out, _ in outputs)
    assert ids == [7, 8], ids
    for n in ids:
        # ^ NOOP on main lacks a.txt, so the declared check fails: both go back to the worker
        # twice (docs/tasks.md "Checks"), then fail at r3.
        done = wait_state(n, 'failed')
        assert done['revision'] == 3 and done['result']['checks'][0]['exit'] == 1, done
        js('task', 'close', n)

    # 13. A check that runs past tasks.checkTimeoutSeconds is killed and fails the task.
    mark('13')
    # ^ `land: { auto: false }`: the uncommitted config no longer stops an auto land, and
    # these tasks must stay ready (docs/tasks.md "Landing").
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  checks: ["sleep 37"]\n  checkTimeoutSeconds: 1\n')
    timed = js('task', 'new', 'NOOP')
    failed = wait_state(timed['id'], 'failed', 'ready')
    check = failed['result']['checks'][0]
    assert failed['state'] == 'failed' and check['timed_out'] and check['exit'] is None, failed['result']
    assert '`sleep 37`: timed out' in Path(failed['report']).read_text()
    # ^ The worker's instructions name the check too: only the check's own process counts.
    assert not [line for line in leftovers('sleep 37') if line.split(None, 1)[1].startswith(('sleep', '/bin/sh -c sleep'))]
    js('task', 'close', timed['id'])
    # A check that prints far past the kept output runs to its end: its exit decides, and the
    # output it keeps is the end, where a test run puts its summary.
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n'
        '  checks: ["seq 1 40000; seq 1 40000 >&2; echo tail end"]\n')
    loud = js('task', 'new', 'NOOP')['id']
    check = wait_state(loud, 'ready', 'failed')['result']['checks'][0]
    assert check['exit'] == 0 and 'tail end' in check['output'] and len(check['output']) < 20000, check
    js('task', 'close', loud, '--discard')
    # A check that leaves a process holding its output ends with its shell: the leftover
    # cannot keep the check, and the machine's check slot, waiting for that output to close.
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n'
        '  checks: ["(sleep 47 &); echo left running"]\n')
    left = js('task', 'new', 'NOOP')['id']
    check = wait_state(left, 'ready', 'failed')['result']['checks'][0]
    assert check['exit'] == 0 and 'left running' in check['output'], check
    for line in leftovers('sleep 47'):
        os.kill(int(line.split(None, 1)[0]), 9)
    js('task', 'close', left, '--discard')

    # 13b. Workflow steps: the owner's own steps apply to a project that names none; the host
    mark('13b')
    # sends each step as its own turn and runs the checks after the last.
    owner_config.write_text('tasks:\n  workflow:\n    steps:\n'
                            '      - name: plan\n        prompt: PLAN only\n'
                            '      - name: build\n        prompt: WORK s.txt\n')
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  checks: ["test -f s.txt"]\n')
    stepped = js('task', 'new', 'BGRESULT NOOP')['id']
    done = wait_state(stepped, 'ready', 'failed')
    assert done['state'] == 'ready' and done['record']['step'] == 1, done
    assert [step['name'] for step in done['record']['steps']] == ['plan', 'build'], done['record']
    turns = [entry for entry in log(stepped) if entry['kind'] == 'result']
    assert len(turns) == 2 and 'Step 1 of 2, plan: PLAN only' in turns[0]['summary'], turns
    assert 'Step 2 of 2, build: WORK s.txt' in turns[1]['summary'], turns
    # ^ Each step asks to end its background jobs first; the last asks for the memo too, so
    # no step of its own writes it.
    assert all('wait for or stop each background job' in t['summary'] for t in turns), turns
    assert 'return your final result, with its memo' in turns[1]['summary'], turns
    # ^ A result while a job still runs is logged once: its notice wakes another turn.
    early = [e for e in log(stepped) if e['kind'] == 'agent' and 'the result came while 1 background job(s) run' in e['text']]
    assert len(early) == 1, log(stepped)
    js('task', 'close', stepped, '--discard')
    # `--skip` leaves a step out for a prompt that carries its plan; the model goes with it.
    skipped = js('task', 'new', '--skip', 'plan', '--model', 'sonnet', 'NOOP')['id']
    done = wait_state(skipped, 'ready', 'failed')
    assert done['state'] == 'ready' and done['record']['model'] == 'sonnet', done
    assert any(t.get('model') == 'sonnet' for t in traces()), 'the worker did not start on sonnet'
    assert [step['name'] for step in done['record']['steps']] == ['build'], done['record']
    turns = [entry for entry in log(skipped) if entry['kind'] == 'result']
    assert len(turns) == 1 and 'Step 1 of 1, build: WORK s.txt' in turns[0]['summary'], turns
    js('task', 'close', skipped, '--discard')
    assert 'has no such step (steps: plan, build)' in cli('task', 'new', '--skip', 'review', 'NOOP', ok=False).stderr
    assert '--skip leaves no step' in cli('task', 'new', '--skip', 'plan', '--skip', 'build', 'NOOP', ok=False).stderr
    # A `context` step folds into the first other step and a `memo` step into the one before
    # it: no turn of their own. The first turn opens with the host's task context: the files,
    # the open tasks that share them and the manager's memo lines that name them.
    owner_config.write_text('tasks:\n  workflow:\n    steps:\n'
                            '      - name: context\n        prompt: CTX recall\n'
                            '      - name: plan\n        prompt: PLAN only\n'
                            '      - name: build\n        prompt: WORK s.txt\n'
                            '      - name: memo\n        prompt: MEMO words\n')
    sharing = js('task', 'new', '--files', 's.txt', '--title', 'Sharer', 'WAIT %s' % (work / 'release-13b'))
    sharer = sharing['id']
    # ^ The host's start set (docs/memory.md "Start set"): the area brief whose glob meets s.txt reaches the
    # prompt; an area brief of another glob and every fact record (here about s.txt too) do not.
    area_text = (project / 'wsx.config.yml').read_text()
    (project / 'briefs').mkdir(exist_ok=True)
    (project / 'briefs' / 's.md').write_text('Area brief: s.txt keeps the flag; do not rename it.\n')
    (project / 'wsx.config.yml').write_text(area_text + 'memory:\n  areas:\n    "s.*": briefs/s.md\n    "zz/**": briefs/zz.md\n')
    head = git('rev-parse', 'HEAD')
    for slug, path, text in (('start-s', 's.txt', 'The fixture flag lives in s.txt.'), ('start-other', 'other.rs', 'other.rs is unrelated.')):
        cli('memory', 'add', '--kind', 'fact', '--slug', slug, '--text-file', '-', '--source', 'task:1', '--verified-by', 'wsx.config.yml',
            '--commit', head, '--paths', path, stdin=text)
    folded = js('task', 'new', '--files', 's.txt', 'NOOP')['id']
    done = wait_state(folded, 'ready', 'failed')
    assert done['state'] == 'ready', done
    assert [step['name'] for step in done['record']['steps']] == ['plan', 'build'], done['record']
    assert done['record']['effort'] == 'high', done
    assert {r['id']: r for r in js('task', 'ledger')['open']}[folded]['agent']['effort'] == 'high'
    assert [t for t in traces() if t.get('launch')][-1]['effort'] == 'high', traces()[-1]
    turns = [entry for entry in log(folded) if entry['kind'] == 'result']
    assert len(turns) == 2, turns
    first = turns[0]['summary']
    assert 'Task context from the host:\nFiles: s.txt' in first, first
    assert 'Open task #%s "Sharer" (working) shares: s.txt' % sharer in first, first
    assert 'start-s' not in first and 'The fixture flag lives in s.txt.' not in first, first
    assert 'start-other' not in first and 'zz.md' not in first, first
    assert 'Area brief briefs/s.md (files s.*):\nArea brief: s.txt keeps the flag; do not rename it.' in first, first
    assert 'data from earlier work' not in first, first
    assert 'PLAN only\n\nIf the task context above is not enough: CTX recall' in first, first
    assert 'WORK s.txt\n\nFor the memo: MEMO words' in turns[1]['summary'], turns[1]
    js('task', 'close', folded, '--discard')
    js('task', 'close', sharer, '--discard')
    # `--size small` runs only the build step, with the folded words, on the small model;
    # `--model` still wins, and `--skip` does not go with it.
    small = js('task', 'new', '--size', 'small', 'NOOP')['id']
    done = wait_state(small, 'ready', 'failed')
    assert done['state'] == 'ready' and done['record']['model'] == 'sonnet', done
    assert [step['name'] for step in done['record']['steps']] == ['build'], done['record']
    # ^ The effort goes with the size: on the record, in the launch argv and in the view.
    assert done['record']['effort'] == 'high', done
    assert {r['id']: r for r in js('task', 'ledger')['open']}[small]['agent']['effort'] == 'high'
    assert [t for t in traces() if t.get('launch')][-1]['effort'] == 'high', traces()[-1]
    turns = [entry for entry in log(small) if entry['kind'] == 'result']
    assert len(turns) == 1 and 'If the task context above is not enough: CTX recall' in turns[0]['summary'], turns
    assert 'For the memo: MEMO words' in turns[0]['summary'], turns
    js('task', 'close', small, '--discard')
    chosen = js('task', 'new', '--size', 'small', '--model', 'haiku', 'NOOP')['id']
    assert show(chosen)['record']['model'] == 'haiku', show(chosen)
    js('task', 'close', chosen, '--discard')
    assert 'cannot be used with' in cli('task', 'new', '--size', 'small', '--skip', 'plan', 'NOOP', ok=False).stderr
    (project / 'wsx.config.yml').write_text(area_text)
    shutil.rmtree(project / 'briefs')
    forget_notes(Path(sharing['task_dir']).parent / 'manager' / 'notes')
    # 13r. Context rotation: a step that ends past tasks.agent.rotateTokens writes handoff notes
    # in its conversation; the next step runs in a fresh one that opens with the task's prompt,
    # those notes and the step.
    mark('13r')
    owner_config.write_text('tasks:\n  workflow:\n    steps:\n'
                            '      - name: plan\n        prompt: BIG PLAN\n'
                            '      - name: build\n        prompt: WORK r.txt\n')
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  checks: ["test -f r.txt"]\n'
        '  agent: { rotateTokens: 300000 }\n')
    seen = len(traces())
    rotated = js('task', 'new', 'NOOP LEAK')['id']
    done = wait_state(rotated, 'ready', 'failed')
    assert done['state'] == 'ready', done
    # ^ The rotation stopped the first conversation's agent with its tool processes, and its
    # host reaped it (docs/tasks.md "Agents", process tree).
    first = leaks(seen)
    assert len({t['agent'] for t in first}) == 2, first
    tree_gone(first[0], 'rotated agent tree')
    assert not zombies(rotated), zombies(rotated)
    assert [e['tokens'] for e in log(rotated) if e['kind'] == 'rotated'] == [400000], log(rotated)
    sent = [t for t in traces() if 'message' in t and t.get('manager') is False]
    asked = [t for t in sent if "past this task's limit" in t['message']]
    fresh = [t for t in sent if 'handoff notes:\nNOTES from the fixture' in t['message']]
    assert len(asked) == 1 and len(fresh) == 1, [t['message'][:80] for t in sent[-4:]]
    assert asked[0]['session'] != fresh[0]['session'], (asked, fresh)
    opening = fresh[0]['message']
    assert 'The task as it was asked:\nNOOP' in opening and 'Step 2 of 2, build: WORK r.txt' in opening, opening
    # ^ The handoff turn is not a result: one per step.
    assert len([e for e in log(rotated) if e['kind'] == 'result']) == 2, log(rotated)
    # ^ The notes are also an episode of the project's memory, written by the host (docs/memory.md "Capture").
    episodes = [h for h in memory_records(cli('memory', 'search', 'NOTES fixture', '--json').stdout) if h['slug'].startswith('rotation-%s-' % rotated)]
    assert len(episodes) == 1 and episodes[0]['kind'] == 'episode' and 'NOTES from the fixture' in episodes[0]['excerpt'], episodes
    js('task', 'close', rotated, '--discard')
    for record in leaks(seen):
        tree_gone(record, 'closed agent tree')
    # A bad rotateTokens refuses the task.
    (project / 'wsx.config.yml').write_text('tasks:\n  minFreeGb: 0\n  agent: { rotateTokens: 5 }\n')
    assert 'tasks.agent.rotateTokens must be' in cli('task', 'new', 'NOOP', ok=False).stderr
    # A project's own steps win; a bad step refuses the task before anything starts.
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  workflow:\n    steps:\n'
        '      - {name: a, prompt: x}\n      - {name: a, prompt: y}\n')
    assert 'named twice' in cli('task', 'new', 'NOOP', ok=False).stderr
    owner_config.unlink()
    # 13t. A stopped agent takes its whole tree: a tool process in its process group and one in
    # a session of its own, as Claude's Bash tool starts its shells. Stop, close, a Pi stop and
    # an agent that ends by itself leave none, and the host reaps the agent (docs/tasks.md
    # "Agents", process tree).
    mark('13t')
    (project / 'wsx.config.yml').write_text('tasks:\n  minFreeGb: 0\n  land: { auto: false }\n')
    release = work / 'release-13t'
    seen = len(traces())
    held = js('task', 'new', 'LEAK WAIT %s' % release)['id']
    wait(lambda: leaks(seen), 't%s leaked' % held)
    stopped = js('task', 'stop', held)
    assert stopped['ok'] and stopped['state'] == 'waiting', stopped
    tree_gone(leaks(seen)[0], 'stopped agent tree')
    assert not zombies(held), zombies(held)
    js('task', 'close', held, '--discard')
    seen = len(traces())
    ended = js('task', 'new', 'LEAKEXIT')['id']
    failed = wait_state(ended, 'failed')
    assert any(e['kind'] == 'stopped' and 'ended during a turn' in e['reason'] for e in log(ended)), log(ended)
    tree_gone(leaks(seen)[0], 'self-ended agent tree')
    assert failed['host_running'] and not zombies(ended), zombies(ended)
    js('task', 'close', ended, '--discard')
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  agent: { adapter: pi, model: fake/pi-model }\n')
    seen = len(traces())
    piped = js('task', 'new', 'NOOP LEAK')['id']
    wait_state(piped, 'ready', 'failed')
    pi_leak = leaks(seen)
    assert [t.get('adapter') for t in pi_leak] == ['pi'], pi_leak
    js('task', 'close', piped, '--discard')
    tree_gone(pi_leak[0], 'closed pi agent tree')
    release.unlink(missing_ok=True)

    # 13c. The checks are the main checkout's as they are when a result comes: a task started
    # under a failing check passes once the project's checks change.
    mark('13c')
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  checks: ["false"]\n')
    release = work / 'release-13c'
    followed = js('task', 'new', 'WAIT %s' % release)['id']
    wait(lambda: show(followed)['record']['agent_session'], 't%s started' % followed)
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  checks: ["true"]\n')
    release.write_text('go')
    done = wait_state(followed, 'ready', 'failed')
    assert done['state'] == 'ready' and done['record']['checks'] == ['true'], done
    assert [check['command'] for check in done['result']['checks']] == ['true'], done['result']
    js('task', 'close', followed, '--discard')

    # M2: the landing steps use a committed config without the a.txt check.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG)
    (project / 'big.txt').write_text(''.join('line %d\n' % n for n in range(1, 11)))
    git('add', 'big.txt')
    git('commit', '-q', '-am', 'land config')

    # 14. Each revision records its paths; list and show report overlap between open tasks.
    mark('14')
    x1 = js('task', 'new', 'WORK x.txt alpha')['id']
    y1 = js('task', 'new', 'WORK y.txt')['id']
    x2 = js('task', 'new', 'WORK x.txt beta')['id']
    for n in (x1, y1, x2):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
    # ^ LAND_CONFIG names no model: workers default to opus.
    assert show(x1)['record']['model'] == 'opus'
    # `new --model` picks the worker's model; a bad one creates no task (docs/tasks.md).
    for bad in ('two words', '-x', ''):
        refused = cli('task', 'new', 'WORK no.txt', '--model=' + bad, ok=False)
        assert 'model' in refused.stderr, refused.stderr
    assert max(row['id'] for row in js('task', 'list', '--all')['tasks']) == x2
    memo_task = js('task', 'new', 'WORK memo.txt MEMO fixtures stay small', '--model', 'sonnet')['id']
    assert wait_state(memo_task, 'ready', 'failed')['state'] == 'ready'
    assert show(memo_task)['record']['model'] == 'sonnet'
    assert [t for t in traces() if t.get('launch')][-1]['model'] == 'sonnet'
    # The memo stays in result.json and reaches the ledger's recent tasks once the task is closed.
    assert show(memo_task)['result']['result']['memo'] == 'fixtures stay small'
    js('task', 'close', memo_task, '--discard')
    recent = {row['id']: row for row in js('task', 'ledger')['recent']}
    assert recent[memo_task]['memo'] == 'fixtures stay small', recent
    assert show(x1)['record']['paths'] == ['x.txt'] and show(x1)['record']['paths_total'] == 1
    rows = {row['id']: row for row in js('task', 'list')['tasks']}
    assert rows[x1]['overlaps'] == [{'id': x2, 'count': 1, 'complete': True}], rows[x1]
    assert rows[y1]['overlaps'] == [] and rows[x2]['overlaps'][0]['id'] == x1
    assert show(x2)['overlaps'] == [{'id': x1, 'count': 1, 'complete': True, 'paths': ['x.txt']}]

    # 14b. Declared `--files` globs overlap before any revision; disjoint ones do not; bad ones
    mark('14b')
    # create no task (docs/tasks.md "Store").
    gate = work / 'files-gate'
    d1 = js('task', 'new', 'WAIT %s' % gate, '--files', 'src/*.rs', '--files', 'docs/a.md')['id']
    d2 = js('task', 'new', 'WAIT %s' % gate, '--files', 'src/**/*.rs')['id']
    d3 = js('task', 'new', 'WAIT %s' % gate, '--files', 'web/*.js')['id']
    assert show(d1)['record']['files'] == ['src/*.rs', 'docs/a.md'] and show(d1)['record']['revision'] == 0
    rows = {row['id']: row for row in js('task', 'list')['tasks']}
    assert rows[d1]['overlaps'] == [{'id': d2, 'count': 1, 'complete': True, 'declared': True}], rows[d1]
    assert rows[d3]['overlaps'] == [] and rows[x1]['overlaps'] == [{'id': x2, 'count': 1, 'complete': True}]
    assert show(d2)['overlaps'] == [{'id': d1, 'count': 1, 'complete': True, 'declared': True,
                                     'paths': ['src/**/*.rs']}]
    # `add --files` extends; a declared glob meets another task's changed path.
    js('task', 'add', d3, '--files', '*.txt', 'also the text files')
    assert show(d3)['record']['files'] == ['web/*.js', '*.txt']
    ledger_rows = {row['id']: row for row in js('task', 'ledger')['open']}
    assert ledger_rows[d3]['overlaps'] == [{'id': n, 'count': 1, 'complete': True, 'declared': True}
                                           for n in (x1, y1, x2)], ledger_rows[d3]
    assert {'id': d3, 'count': 1, 'complete': True, 'declared': True} in ledger_rows[x1]['overlaps']
    # The ledger names the agent in use now (docs/tasks.md "Agents"); no effort.
    assert ledger_rows[d3]['agent'] == {'adapter': 'claude', 'model': ledger_rows[d3]['agent']['model'], 'effort': 'high', 'fallback': False}, ledger_rows[d3]
    last = max(row['id'] for row in js('task', 'list', '--all')['tasks'])
    for bad in ('/abs/*.rs', '../x', 'a/../b', 'x' * 257, '[', ''):
        refused = cli('task', 'new', 'WORK no.txt', '--files', bad, ok=False)
        assert '--files' in refused.stderr, (bad, refused.stderr)
    many = [arg for n in range(65) for arg in ('--files', 'f%d' % n)]
    assert 'at most 64' in cli('task', 'new', 'WORK no.txt', *many, ok=False).stderr
    assert '--files' in cli('task', 'add', d1, '--files', '../up', 'x', ok=False).stderr
    assert max(row['id'] for row in js('task', 'list', '--all')['tasks']) == last

    # 14c. The ledger lists the project's other worktrees with their flags, never a task's copy.
    mark('14c')
    git('worktree', 'add', '-q', '-b', 'stray-merged', str(base / 'stray-merged'))
    git('worktree', 'add', '-q', '-b', 'stray-dirty', str(base / 'stray-dirty'))
    (base / 'stray-dirty/new.txt').write_text('ahead\n')
    git('add', 'new.txt', cwd=base / 'stray-dirty')
    git('commit', '-q', '-m', 'ahead', cwd=base / 'stray-dirty')
    (base / 'stray-dirty/README').write_text('edited\n')
    git('worktree', 'add', '-q', '-b', 'stray-gone', str(base / 'stray-gone'))
    shutil.rmtree(base / 'stray-gone')
    ledger = js('task', 'ledger')
    strays = {Path(row['path']).name: row for row in ledger['stray_worktrees']}
    assert set(strays) == {'stray-merged', 'stray-dirty', 'stray-gone'}, ledger['stray_worktrees']
    # ^ The sweep's rule (stale::loss): uncommitted files and commits neither merged nor pushed.
    flags = lambda name: {k: strays[name][k] for k in ('branch', 'dirty', 'unmerged', 'missing', 'sessions')}
    assert flags('stray-merged') == {'branch': 'stray-merged', 'dirty': 0, 'unmerged': 0,
                                     'missing': False, 'sessions': 0}, strays
    assert flags('stray-dirty') == {'branch': 'stray-dirty', 'dirty': 1, 'unmerged': 1,
                                    'missing': False, 'sessions': 0}, strays
    assert flags('stray-gone') == {'branch': 'stray-gone', 'dirty': 0, 'unmerged': 0,
                                   'missing': True, 'sessions': 0}, strays
    assert ledger['stray_worktrees_omitted'] == 0
    for name in ('stray-merged', 'stray-dirty'):
        git('worktree', 'remove', '--force', str(base / name))
    git('worktree', 'prune')
    git('branch', '-D', 'stray-merged', 'stray-dirty', 'stray-gone')
    assert js('task', 'ledger')['stray_worktrees'] == []
    # 14c1. A ledger build parses each task log once: the agent's fallback flag comes with the
    # usage, not from a second read (docs/tasks.md "Agents"). It was two reads per open task.
    mark('14c1')
    traced = cli_env({'WSX_TRACE_READ_LOG': '1'}, 'task', 'ledger', '--json').stderr.splitlines()
    reads = [line.split(' ', 2)[2] for line in traced if line.startswith('wsx-trace read_log ')]
    task_logs = [path for path in reads if path.endswith('/log.jsonl') and '/manager/' not in path]
    assert task_logs and len(task_logs) == len(set(task_logs)), task_logs
    print('14c1 task log reads per ledger build: %d for %d logs' % (len(task_logs), len(set(task_logs))), file=sys.stderr)
    # 14c2. A git that hangs: the ledger answers after the 10 s wait with a note, its rows kept
    # and the main checkout's changed files left out (docs/tasks.md "Ledger").
    mark('14c2')
    real_git = shutil.which('git', path=env['PATH'])
    hang = tmp / 'hang-git'
    hang.mkdir()
    (hang / 'git').write_text('#!/bin/sh\ncase " $* " in *" status "*) exec sleep 60;; esac\nexec ' + real_git + ' "$@"\n')
    (hang / 'git').chmod(0o755)
    started = time.monotonic()
    hung = json.loads(cli_env({'PATH': str(hang) + ':' + env['PATH']}, 'task', 'ledger', '--json').stdout)
    took = time.monotonic() - started
    assert 9 < took < 40, took
    assert len(hung['notes']) == 1 and 'git did not answer in 10 s' in hung['notes'][0], hung.get('notes')
    assert hung['main']['branch'] == 'main' and hung['main']['changed_files'] is None, hung['main']
    assert [row['id'] for row in hung['open']] == [row['id'] for row in js('task', 'ledger')['open']]
    assert 'notes' not in js('task', 'ledger')
    # 14d. The TUI's Tasks view: open tasks in attention order with their state word and what
    # they wait for, then the recent ones; Enter shows one read-only; the list follows the
    # ledger as it changes; a narrow width keeps title and state (docs/tasks.md "TUI").
    mark('14d')
    asking = js('task', 'new', 'NEEDS WebSearch NOOP')['id']
    failing = js('task', 'new', 'WORK broken.txt')['id']
    reason = wait_state(asking, 'waiting')['record']['waiting']['reason']
    wait_state(failing, 'failed')

    def row(shown, n):
        return next(line for line in shown.splitlines() if re.search(r'#%d\b' % n, line))

    def open_gate():
        gate.write_text('open')
        return b''
    shots = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'(?s)Open.*#%d .*Recent' % d3, b'\r'),
        (r'Next +Waiting for your answer', b'\x1b'),
        (r'\(enter\)detail', b'j\r'),
        (r'Checks', b'\x1b'),
        (r'\(enter\)detail', open_gate),
        (r'#%d +ready' % d1, b'\x1b'),
        (r'WORKSPACE', b'')])
    listed, asked, _, checked = shots[1:5]
    order = [listed.index(row(listed, n)) for n in (asking, failing, x1, d1, memo_task)]
    assert order == sorted(order) and order[-1] > listed.index('Recent'), listed
    # ^ The row's label and the detail's sentence are wsx-core's status words, as `show` has them.
    assert re.search(r'NEEDS WebSearch NOOP +#%d +waiting +Waiting for your answer' % asking, row(listed, asking)), listed
    assert re.search(r'#%d +failed' % failing, row(listed, failing)), listed
    assert re.search(r'#%d +working' % d1, row(listed, d1)) and 'closed' in row(listed, memo_task), listed
    assert 'NEEDS WebSearch NOOP  #%d' % asking in asked and re.search(r'State +waiting', asked), asked
    asked_status = show(asking)['status']
    assert asked_status['label'] == 'Waiting for your answer' and reason in asked_status['sentence'], asked_status
    assert asked_status['sentence'].startswith('Waiting for your answer for '), asked_status
    assert re.search(r'Next +Waiting for your answer for ', asked), asked
    assert next(r for r in js('task', 'ledger')['open'] if r['id'] == asking)['status']['label'] == asked_status['label']
    assert re.search(r'Checks +0 passed, 1 failed', checked) and re.search(r'failed +test ! -e broken.txt', checked), checked
    # ^ The glance rows first, the per-check lines under Details (docs/tasks.md "TUI").
    assert re.search(r'Result +did: The checks failed', checked) and checked.index('Details') < re.search(r'(?m)^ +failed +test ! -e', checked).start(), checked
    assert 'ping' not in checked, checked
    assert 'TASKS' not in shots[-1], shots[-1]
    narrow = tui_screens(wsx, env, project, [(r'WORKSPACE', b't'), (r'Recent', b'')], cols=40)[1]
    for title, word in (('NEEDS WebSearch', 'waiting'), ('WORK broken.txt', 'failed'), ('WORK memo.txt', 'closed')):
        assert re.search(r'^ %s.* %s$' % (re.escape(title), word), narrow, re.M), narrow
    assert not re.search(r'#\d', narrow.split('Open', 1)[1]), narrow
    for n in (asking, failing):
        js('task', 'close', n, '--discard')
    for n in (d1, d2, d3):
        js('task', 'close', n, '--discard')

    # 14e. `S` in the Tasks view opens a shell: in the task's working copy on its row, the
    # project's shell in the main checkout from the chat. The workspace key goes back to the
    # Tasks view. A second `S` enters the same session, and the worker keeps its turn: no
    # hand, no stop. After `exit`, `S` makes a new project shell (docs/tasks.md "TUI").
    mark('14e')
    shell_gate = work / 'shell-gate'
    sh = js('task', 'new', 'WAIT %s' % shell_gate)['id']
    wait_state(sh, 'working')
    copy_name = Path(show(sh)['record']['working_copy']).name
    where = b'printf "at:%s\\n" "${PWD##*/}"\r'

    prompt = r'(?m)\$ *$'
    # ^ The raw frames of the list and the chat with the `S` hint, replayed into the Human
    # Verify captures; `.work/` is git-ignored.
    captures = Path(__file__).resolve().parents[1] / '.work' / 'captures'
    captures.mkdir(parents=True, exist_ok=True)
    for cols in (80, 120):
        frames = []
        hinted = tui_screens(wsx, env, project, [
            (r'WORKSPACE', b't'),
            (r'#%d +working' % sh, b'm'),
            (r'MANAGER', b'')], cols=cols, raw=frames)
        (captures / ('tasks-%d.ans' % cols)).write_bytes(frames[1])
        (captures / ('manager-%d.ans' % cols)).write_bytes(frames[2])
        # ^ The footer keeps `(S)hell` and `Esc: close` at both widths.
        assert re.search(r'(?m)^ TASKS +\(j/k\)move +\(enter\)detail +\(m\)anager +\(S\)hell +Esc: close ', hinted[1]), hinted[1]
        # ^ The footer line by its content: `(u)sage` follows `(a)gent`, fits at 120 columns and
        # goes before `(o)verview` at 80 (ui/mod.rs `status_bar`).
        footer = re.search(r'(?m)^ TASKS .*$', hinted[1]).group(0)
        assert bool(re.search(r'\(a\)gent +\(u\)sage', footer)) == (cols == 120), footer
        assert re.search(r'(?m)^ MANAGER +\(i\)message +\(/\)find +\(S\)hell +Esc: back +\(j/k\)scroll ', hinted[2]), hinted[2]
    # ^ Open tasks of earlier steps sort above a working one (task_list.rs `rows`): move to
    # its row, and its detail shows it is the one.
    rank = {'waiting': 0, 'failed': 1, 'ready': 2, 'checking': 3}
    ordered = sorted(js('task', 'ledger')['open'], key=lambda task: rank.get(task['state'], 4))
    to_row = b'j' * [task['id'] for task in ordered].index(sh) + b'\r'
    shown_task = r'WAIT .*#%d\b' % sh
    tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'#%d +working' % sh, to_row),
        (shown_task, b'S'),
        (prompt, where),
        (r'at:%s' % re.escape(copy_name), b'\x01w'),
        (shown_task, b'\x1b'),
        (r'(?m)^ TASKS ', b'm'),
        (r'MANAGER', b'S'),
        (prompt, where),
        (r'at:%s' % re.escape(project.name), b'')])
    copy_path = show(sh)['record']['working_copy']
    task_shells = [label for label in sessions_at(copy_path) if label != 'task-%d' % sh]
    main_shells = [label for label in sessions_at(project) if not label.startswith('task-')]
    assert len(task_shells) == 1 and main_shells == ['shell'], (task_shells, main_shells)
    main_ids = [i for i, label in zip(sessions_at(project, 'id'), sessions_at(project)) if not label.startswith('task-')]
    again = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'#%d +working' % sh, to_row),
        (shown_task, b'S'),
        (r'at:%s' % re.escape(copy_name), b'\x01w'),
        (shown_task, b'\x1b'),
        (r'(?m)^ TASKS ', b'm'),
        (r'MANAGER', b'S'),
        (r'at:%s' % re.escape(project.name), b'')])
    assert 'TASK ' not in again[3] and 'MANAGER' not in again[-1], again
    assert [label for label in sessions_at(copy_path) if label != 'task-%d' % sh] == task_shells
    assert [label for label in sessions_at(project) if not label.startswith('task-')] == main_shells
    # ^ The same project shell, its earlier output on the screen; the workspace key and its
    # exit go back to the chat, and the next `S` makes a new one.
    exited = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b'tm'),
        (r'MANAGER', b'S'),
        (r'at:%s' % re.escape(project.name), b'\x01w'),
        (r'MANAGER', b'S'),
        (r'at:%s' % re.escape(project.name), b'exit\r'),
        (r'MANAGER', b'S'),
        (prompt, b'')])
    assert '(S)hell' in exited[3] and 'at:' not in exited[-1], exited
    renewed = [i for i, label in zip(sessions_at(project, 'id'), sessions_at(project)) if not label.startswith('task-')]
    assert len(main_ids) == 1 and len(renewed) == 1 and renewed != main_ids, (main_ids, renewed)
    main_shells = ['shell']
    assert show(sh)['state'] == 'working' and not kinds(sh, 'hand', 'stopped'), log(sh)
    shell_gate.write_text('open')
    wait_state(sh, 'ready', 'failed')
    js('task', 'close', sh, '--discard')
    cli('session', 'delete', main_shells[0], '-p', 'demo')

    # 15. Landing, local-merge (a dirty default-branch checkout is part 32h).
    mark('15')
    # Uncommitted work in the task's own copy refuses too: wsx commits nothing for the worker.
    y1_copy = Path(show(y1)['record']['working_copy'])
    (y1_copy / 'README').write_text('uncommitted\n')
    refused = land(y1, ok=False)
    assert 'uncommitted changes: README' in refused['message'], refused
    git('checkout', '--', 'README', cwd=y1_copy)
    # y1 is on the tip: fast-forward. x1 then rebases onto the moved main without conflict.
    landed = land(y1)
    assert landed['ok'] and landed['state'] == 'landed', landed
    assert git('rev-parse', 'main') == git('rev-parse', 'wsx/t%d' % y1) and (project / 'y.txt').exists()
    assert land(x1)['ok']
    landed_x1 = wait_state(x1, 'landed', 'failed')
    assert landed_x1['state'] == 'landed' and main_has('x.txt') and main_has('y.txt'), landed_x1
    assert landed_x1['record']['paths'] == ['x.txt'] and landed_x1['record']['revision'] == 2
    # x2 changed the same file: the rebase conflicts, the worker resolves it, then it lands.
    conflict = land(x2)
    assert conflict['ok'] and 'conflicted in x.txt' in conflict['message'], conflict
    assert any(e['kind'] == 'conflict' and e['paths'] == ['x.txt'] for e in log(x2))
    assert wait_state(x2, 'ready', 'failed')['state'] == 'ready'
    assert any(t.get('resolved_onto') for t in traces())
    assert land(x2)['state'] == 'landed'
    assert git('show', 'main:x.txt') == 'resolved'
    for n in (x1, y1, x2):
        wait_closed(n)
    # 15a. The detail's actions (docs/tasks.md "TUI"; after 15, since a land moves main under y1, x1 and x2): the footer names a key only while its action
    # applies (`view::owner_actions`, the web's rule too). `L` and `H` ask first; `y` lands the
    # ready task through `wsx task land`; `a` on a task that waits for the owner opens the
    # manager's message line with `#<n> `.
    mark('15a')
    lt = js('task', 'new', 'WORK tuilanded.txt')['id']
    assert wait_state(lt, 'ready', 'failed')['state'] == 'ready', show(lt)
    ask = js('task', 'new', 'NEEDS WebSearch NOOP')['id']
    wait_state(ask, 'waiting')
    ordered = sorted(js('task', 'ledger')['open'], key=lambda task: rank.get(task['state'], 4))
    rows_to = lambda n: b'j' * [task['id'] for task in ordered].index(n) + b'\r'
    ready_footer = r'(?m)^ TASK +\(j/k\)scroll +\(S\)hell +\(L\)and +\(H\)and +Esc: back'
    seen = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'#%d +ready' % lt, rows_to(lt)),
        (ready_footer, b'H'),
        (r'Take over #%d\? \(y\)es  \(n\)o' % lt, b'n'),
        (ready_footer, b'L'),
        (r'Land #%d\? \(y\)es  \(n\)o' % lt, b'y'),
        (r'State +landed', b'')])
    assert '(a)nswer' not in seen[2] and 'Take over' not in seen[4], seen
    assert show(lt)['state'] == 'landed' and main_has('tuilanded.txt'), show(lt)
    seen = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'#%d +waiting' % ask, rows_to(ask)),
        (r'(?m)^ TASK +\(j/k\)scroll +\(S\)hell +\(a\)nswer +\(H\)and +Esc: back', b'L'),
        (r'(?m)^ TASK .*\(a\)nswer', b'a'),
        (r'(?s)> #%d\b.*MANAGER' % ask, b'')])
    assert 'Land #' not in seen[2], seen[2]
    js('task', 'close', ask, '--discard')

    # Two lands rebase onto one tip; the check slot runs p's checks first, so q's checks end
    # after p landed and main moved: q rebases and checks again by itself and lands.
    configure(LAND_CONFIG.replace('  checks:\n', '  checks:\n    - "sleep 1"\n'), 'slow checks')
    p, q = (js('task', 'new', 'WORK %s.txt' % name)['id'] for name in ('p', 'q'))
    for n in (p, q):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', show(n)
    (project / 'moved.txt').write_text('moved\n')
    git('add', 'moved.txt')
    git('commit', '-q', '-m', 'main moves')
    for n in (p, q):
        assert 'checks run on the new revision' in land(n)['message'], show(n)
    for n in (p, q):
        assert wait_state(n, 'landed', 'ready', 'failed')['state'] == 'landed', log(n)
    assert not [e for e in log(q) if e['kind'] == 'land_stopped'], log(q)
    assert [e['kind'] for e in log(q)].count('checks') == 3 and main_has('p.txt') and main_has('q.txt'), log(q)
    for n in (p, q):
        wait_closed(n)

    # 16. Auto land (the default; docs/tasks.md "Landing"): a ready task the owner created waits
    mark('16')
    # for the manager's review instead of landing by itself, the review's land needs no other
    # step, its Human Verify items join the To check list, and a landing that conflicts lands by
    # itself after the worker's fix. With main not checked out, the ref moves with the old tip
    # as guard.
    configure(LAND_CONFIG.replace('  land: { auto: false }\n', ''), 'auto land')

    # ^ The task records main as its default branch at `new`; main is left during the turn.
    hv = js('task', 'new', 'SLOW WORK z.txt HV')['id']
    git('switch', '-q', '-c', 'parking')
    assert wait_state(hv, 'ready', 'landed', 'failed')['state'] == 'ready'
    time.sleep(1.5)
    assert show(hv)['state'] == 'ready' and not main_has('z.txt'), 'an owner task landed without a review'
    assert reviewed(hv)['state'] == 'landed' and main_has('z.txt')
    assert git('rev-parse', '--abbrev-ref', 'HEAD') == 'parking'
    assert [e['kind'] for e in log(hv)].count('landed') == 1
    item = js('task', 'check')['open']
    assert [(i['id'], i['task'], i['check'], i['pass_if'], i['state']) for i in item] == [
        ('c1', hv, 'open the file', 'it has text', 'open')], item
    assert item[0]['commit'] == git('rev-parse', 'main') and item[0]['revision'] == 1
    assert show(hv)['to_check'][0]['id'] == 'c1'
    report = Path(show(hv)['report']).read_text()
    assert '## To check\n\n- [ ] open the file (pass if: it has text)' in report and 'State `landed`' in report, report
    git('switch', '-q', 'main')
    # Two tasks on one file: the second land conflicts, the worker resolves it, and it lands by
    # itself without a second review.
    a1 = js('task', 'new', 'WORK x.txt alpha HV')['id']
    a2 = js('task', 'new', 'WORK x.txt beta HV')['id']
    for n in (a1, a2):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', show(n)
    assert land(a1)['state'] == 'landed'
    assert 'lands when its checks pass' in land(a2)['message']
    assert wait_state(a2, 'landed', 'failed')['state'] == 'landed', show(a2)
    conflicts = [n for n in (a1, a2) if any(e['kind'] == 'conflict' for e in log(n))]
    assert conflicts == [a2] and git('show', 'main:x.txt') == 'resolved', conflicts
    assert len(js('task', 'check')['open']) == 3
    # An item keeps where to look and its capture, copied into the task folder before the copy
    # goes. An item that names a command and one about a page without a capture go back to the
    # worker once, in one message; its next result has none.
    shot = js('task', 'new', 'WORK cap.txt HV CAPTURE')['id']
    command = js('task', 'new', 'WORK cmd.txt CMDHV BAREHV')['id']
    assert reviewed(shot)['state'] == 'landed', show(shot)
    # ^ command branched before shot landed: its land rebases, reruns the checks and lands when
    # they pass (docs/tasks.md "Landing" 3).
    assert 'checks run on the new revision' in reviewed(command)['message'], show(command)
    assert wait_state(command, 'landed', 'ready', 'failed')['state'] == 'landed', show(command)
    for n in (shot, command):
        wait_closed(n)
    assert [e['kind'] for e in log(command)].count('result') == 2, log(command)
    sent = [t['message'] for t in traces() if 'yours to prove' in str(t.get('message'))]
    assert len(sent) == 1 and 'wsx task list' in sent[0] and 'the page reads well' in sent[0], sent
    kept = [i for i in js('task', 'check')['open'] if i['task'] in (shot, command)]
    assert [(i['task'], i['where'], i['capture']) for i in kept] == [(shot, '/#/x', 'captures/r1-1.png')], kept
    stored = Path(show(shot)['report']).parent / 'captures' / 'r1-1.png'
    assert stored.read_bytes().startswith(b'\x89PNG'), stored
    assert stored.parent.stat().st_mode & 0o777 == 0o700, oct(stored.parent.stat().st_mode)
    js('task', 'check', 'pass', kept[0]['id'])
    # A video capture (MP4 by its `ftyp` box) and its poster are kept as r<k>-<n>.mp4 and
    # r<k>-<n>.poster.png; a video over 25 MiB and a text file named .mp4 are dropped, their
    # items kept without a capture (docs/tasks.md "To check").
    video = js('task', 'new', 'WORK v.txt HV VIDEO BIGVIDEO NOTVIDEO')['id']
    assert reviewed(video)['state'] == 'landed', show(video)
    wait_closed(video)
    folder = Path(show(video)['report']).parent
    clip = (ROOT / 'scripts/fixtures/check-clip.mp4').read_bytes()
    assert (folder / 'captures/r1-1.mp4').read_bytes() == clip, folder
    assert (folder / 'captures/r1-1.poster.png').read_bytes().startswith(b'\x89PNG'), folder
    assert sorted(p.name for p in (folder / 'captures').iterdir()) == ['r1-1.mp4', 'r1-1.poster.png'], folder
    rows = [json.loads(line) for line in (folder.parent / 'checks.jsonl').read_text().splitlines()]
    rows = [{k: row.get(k) for k in ('check', 'capture', 'poster')} for row in rows if row['task'] == video]
    assert rows == [{'check': 'open the file', 'capture': 'captures/r1-1.mp4', 'poster': 'captures/r1-1.poster.png'},
                    {'check': 'the long video plays', 'capture': None, 'poster': None},
                    {'check': 'the text video plays', 'capture': None, 'poster': None}], rows
    # ^ A third item with a capture for the capture sweep (part 20).
    recent = js('task', 'new', 'WORK r.txt HV CAPTURE')['id']
    assert reviewed(recent)['state'] == 'landed', show(recent)
    wait_closed(recent)
    # ^ Skipped, so the capture sweep (part 20) frees a skipped item's files like a passed one's.
    js('task', 'check', 'skip', *[i['id'] for i in js('task', 'check')['open'] if i['task'] in (video, recent)])
    # No checks: nothing proves the work, so a ready task waits for a land by hand.
    configure(LAND_CONFIG.replace('  land: { auto: false }\n', '').replace(
        '  checks:\n    - "test ! -e broken.txt"\n', ''), 'no checks')
    unchecked = js('task', 'new', 'WORK n.txt')['id']
    assert wait_state(unchecked, 'ready', 'landed', 'failed')['state'] == 'ready'
    time.sleep(1)
    assert show(unchecked)['state'] == 'ready' and land(unchecked)['state'] == 'landed'
    # Pass one item, then fail one: the fail starts a linked fix task.
    passed = js('task', 'check', 'pass', 'c1')
    assert [c['state'] for c in passed['changed']] == ['passed'] and passed['open'] == 2, passed
    assert 'already passed' in cli('task', 'check', 'pass', 'c1', '--json', ok=False).stderr
    assert 'no item c9' in cli('task', 'check', 'pass', 'c9', ok=False).stderr
    refused = subprocess.run([str(wsx), 'task', 'check', 'pass', '--all'], cwd=project, env=manager_env,
                             capture_output=True, text=True)
    assert refused.returncode != 0 and 'for the owner' in refused.stderr, refused.stderr
    assert 'needs --note' in cli('task', 'check', 'fail', 'c2', '--note', ' ', ok=False).stderr
    failed = js('task', 'check', 'fail', 'c2', '--note', 'the text is cut off')
    (failed_item,) = failed['changed']
    fix = failed_item['fix_task']
    assert failed_item['state'] == 'failed' and failed_item['note'] == 'the text is cut off' and fix, failed
    fixing = show(fix)
    assert fixing['record']['fixes'] == 'c2' and fixing['title'] == 'Fix: open the file', fixing
    prompt = next(e['text'] for e in log(fix) if e.get('mode') == 'prompt')
    task_of_c2 = failed_item['task']
    assert 'the text is cut off' in prompt and show(task_of_c2)['report'] in prompt and 'Pass if: it has text' in prompt, prompt
    wait_state(fix, 'ready', 'landed', 'failed')
    assert [i['id'] for i in js('task', 'check')['open']] == ['c3']
    for n in (hv, a1, a2, unchecked):
        wait_closed(n)
    # ^ No checks prove the fix, so it waits ready; it has no commits and closes plainly.
    js('task', 'close', fix)
    configure(LAND_CONFIG, 'land by hand')

    # 17. Deploy and release records. Each runs in a temporary copy of a default-branch commit.
    mark('17')
    store_up(created)

    unset = cli('task', 'deploy', ok=False)
    assert 'tasks.deploy.command is not set' in unset.stderr and 'nothing was run' in unset.stderr
    configure(DEPLOY_CONFIG, 'deploy config')
    assert js('task', 'deployed')['deploy']['commit'] is None
    # Open items stop only public output: a release, and a deploy marked public.
    refused = cli('task', 'release', '--json', ok=False)
    assert '1 item(s) to check are open' in refused.stderr and 'Nothing was run' in refused.stderr, refused.stderr
    (project / 'wsx.config.yml').write_text((project / 'wsx.config.yml').read_text().replace(
        '  deploy:\n', '  deploy:\n    public: true\n'))
    assert 'to check are open' in cli('task', 'deploy', ok=False).stderr
    git('checkout', '--', 'wsx.config.yml')
    assert not (base / 'deploy.log').exists()
    deployed = ran('deploy')
    assert deployed['message'].endswith('; 1 to check on the deployed site'), deployed
    (base / 'deploy.log').unlink()
    # A skip closes the item without a verdict: it is recorded as skipped, starts no task, refuses
    # the manager, and the release below runs (docs/tasks.md "To check").
    tasks_before = len(js('task', 'list', '--all')['tasks'])
    refused = subprocess.run([str(wsx), 'task', 'check', 'skip', '--all'], cwd=project, env=manager_env,
                             capture_output=True, text=True)
    assert refused.returncode != 0 and 'check skip is for the owner' in refused.stderr, refused.stderr
    # ^ A dismiss of a question is the owner's too (docs/tasks.md "Questions" under "Manager").
    refused = subprocess.run([str(wsx), 'task', 'dismiss', '1'], cwd=project, env=manager_env,
                             capture_output=True, text=True)
    assert refused.returncode != 0 and 'dismiss is for the owner' in refused.stderr, refused.stderr
    skipped = js('task', 'check', 'skip', '--all')
    assert [c['state'] for c in skipped['changed']] == ['skipped'] and skipped['open'] == 0, skipped
    assert skipped['message'] == '1 skipped. 0 to check', skipped
    (skipped_item,) = skipped['changed']
    assert skipped_item.get('fix_task') is None and skipped_item.get('note') is None, skipped_item
    assert 'already skipped' in cli('task', 'check', 'pass', skipped_item['id'], ok=False).stderr
    assert next(i for i in show(skipped_item['task'])['to_check'] if i['id'] == skipped_item['id'])['state'] == 'skipped'
    assert len(js('task', 'list', '--all')['tasks']) == tasks_before
    # ^ A release whose process was killed left its copy; the next deploy removes it. A pid
    # above macOS's 99998 is never alive.
    orphan = base / 'project-release-999999'
    git('worktree', 'add', '-q', '--detach', str(orphan))
    # ^ A worktree on a branch with the same name shape is someone's work: it stays.
    owned = base / 'project-deploy-999998'
    git('worktree', 'add', '-q', '-b', 'owned-999998', str(owned))
    deployed = ran('deploy')
    assert not orphan.exists() and str(orphan) not in git('worktree', 'list')
    assert owned.is_dir() and str(owned) in git('worktree', 'list')
    git('worktree', 'remove', '--force', str(owned))
    git('branch', '-D', 'owned-999998')
    tip = git('rev-parse', 'main')
    assert deployed['commit'] == tip and deployed['exit'] == 0, deployed
    commit_line, where = (base / 'deploy.log').read_text().split()
    assert commit_line == tip and Path(where) != project and not Path(where).exists()
    assert git('worktree', 'list', '--porcelain').count('worktree ') == 1
    # ^ The fixture's wsxd has no SSH_AUTH_SOCK, as one first started from an ssh login has: the
    # command gets the login session's agent socket (macOS), else none (Linux CI has no login agent).
    assert 'SSH_AUTH_SOCK' not in env
    login_sock = login_agent_socket()
    assert (base / 'deploy-agent.log').read_text() == (login_sock or ''), (base / 'deploy-agent.log').read_text()
    assert js('task', 'deployed')['deploy']['behind'] == 0
    later = js('task', 'new', 'WORK w.txt')['id']
    assert wait_state(later, 'ready', 'failed')['state'] == 'ready'
    assert land(later)['state'] == 'landed'
    wait_closed(later)
    status = js('task', 'deployed')
    assert status['deploy']['commit'] == tip and status['deploy']['behind'] == 1, status
    assert ran('release')['exit'] == 3
    release_log = (project_store / 'follow-up.log').read_text()
    assert 'release broke here' in release_log and 'the failure is recorded' in release_log, release_log[-2000:]
    # ^ A failed attempt keeps the end of its output: the ledger's deploys show its last 40 lines.
    release_status = js('task', 'deployed')['release']
    failed = release_status.pop('failed')
    assert release_status == {'commit': None, 'failed_since': 1, 'configured': True}, release_status
    assert failed['exit'] == 3 and failed['commit'] == git('rev-parse', 'main'), failed
    # ^ The output says which agent ssh asked, or why there was none, as a refused push does.
    *printed, agent_words = failed['output'].splitlines()
    assert printed == [str(n) for n in range(23, 61)] + ['release broke here: Permission denied (publickey).'], failed
    if login_sock:
        assert agent_words.startswith("ssh offered the keys of the login session's ssh agent at %s " % login_sock), agent_words
    else:
        assert agent_words.startswith('ssh had no agent: SSH_AUTH_SOCK is not set'), agent_words
    assert js('task', 'ledger')['deploys']['release']['failed'] == failed
    older = git('rev-parse', 'main~3')
    assert ran('deploy', '--commit', older)['commit'] == older
    assert js('task', 'deployed')['deploy']['behind'] == 3

    # 18. The ledger: open tasks by attention order, recent ones, deploys, the main checkout.
    mark('18')
    # ^ The worker doubts a check it could not run; the ledger says what the host's checks did.
    ready_task = js('task', 'new', 'WORK l.txt could not run the browser test')['id']
    blocked_task = js('task', 'new', 'NEEDS WebSearch WORK m.txt')['id']
    wait_state(ready_task, 'ready', 'failed')
    wait_state(blocked_task, 'waiting')
    assert 'only a ready task lands' in land(blocked_task, ok=False)['message']
    raw = cli('task', 'ledger').stdout
    ledger = json.loads(raw)
    assert len(raw.encode()) <= 16 * 1024 and ledger['open_omitted'] == 0
    assert ledger['to_check'] == 0 and ledger['auto_land'] is False and 'verification' not in ledger['open'][0]
    assert [(row['id'], row['state']) for row in ledger['open']] == [(blocked_task, 'waiting'), (ready_task, 'ready')]
    assert ledger['open'][0]['waiting']['on'] == 'owner', ledger['open'][0]
    assert ledger['open'][1]['summary'] == 'did: WORK l.txt could not run the browser test' and ledger['open'][1]['human_verify'] == 0
    checked = ledger['open'][1]['checks']
    assert [{k: c[k] for k in ('command', 'outcome')} for c in checked] == [{'command': 'test ! -e broken.txt', 'outcome': 'passed'}], checked
    assert all(isinstance(c['ms'], int) and isinstance(c['queued_ms'], int) for c in checked), checked
    assert ledger['history'] == 'wsx task search --task <n> | <words>'  # the ledger keeps no history
    assert 0 < len(ledger['recent']) <= 10 and all(row['state'] in ('landed', 'closed') for row in ledger['recent'])
    assert ledger['deploys']['deploy']['commit'] == older and ledger['main']['branch'] == 'main'
    assert ledger['main']['changed_files'] >= 1  # scratch.txt is untracked in the main checkout
    for n in (ready_task, blocked_task):
        js('task', 'close', n, '--discard')

    manager_up(created)

    # 21. Events: task state changes reach the manager, merged while it is busy; an owner
    mark('21')
    # message waiting at the same time goes first; a host restart loses no event.

    # ^ A failed deploy or release is an event with the end of its output.
    js('task', 'check', 'pass', '--all')
    ran('release')
    wait(lambda: events_with('release of %s failed (exit 3); its output ends:\n    53\n' % git('rev-parse', 'main')[:12],
                             '\n    60\n    release broke here'), 'the failed release event')
    # ^ A release longer than a call through the manager's broker: the call answers that it
    # started while the command runs, a second one is refused, and the result is recorded and
    # becomes an event.
    release_config = (project / 'wsx.config.yml').read_text()
    configure(LAND_CONFIG + "  release:\n    command: 'sleep 6; echo released >> ../long-release.log'\n    approve: false\n", 'long release')
    long_tip = git('rev-parse', 'main')
    records_before = len(deploy_records())
    started = js('task', 'say', 'RUN wsx task release -p demo --json')
    text = reply_to(started['id'])['text']
    assert 'ran: wsx task release -p demo --json -> 0' in text and 'release of %s started; ' % long_tip[:12] in text, text
    assert not (base / 'long-release.log').exists() and len(deploy_records()) == records_before
    busy = cli('task', 'release', ok=False).stderr
    assert 'a deploy or release of demo runs' in busy and 'Nothing was run' in busy, busy
    # ^ The detached run's flag skips the checks only with the held lock's descriptor.
    forged = cli('task', 'release', '--run-locked', '0', ok=False).stderr
    assert 'descriptor 0 is not' in forged, forged
    wait(lambda: (base / 'long-release.log').exists(), 'the long release', 30)
    wait(lambda: len(deploy_records()) == records_before + 1, 'the long release record')
    long_release = deploy_records()[-1]
    assert long_release['kind'] == 'release' and long_release['exit'] == 0 and long_release['commit'] == long_tip, long_release
    assert long_release['duration_ms'] >= 6000 and (base / 'long-release.log').read_text() == 'released\n'
    assert js('task', 'ledger')['deploys']['release']['commit'] == long_tip
    wait(lambda: events_with('release of %s succeeded after 6 s' % long_tip[:12]), 'the release event')
    wait(deploy_lock_free, 'the deploy lock')
    # ^ A successful deploy is an event too, so a manager that waits for one goes on.
    configure(LAND_CONFIG + "  deploy:\n    command: 'true'\n", 'quick deploy')
    ran('deploy')
    quick_deploy = deploy_records()[-1]
    assert quick_deploy['kind'] == 'deploy' and quick_deploy['exit'] == 0, quick_deploy
    wait(lambda: events_with('deploy of %s succeeded after' % quick_deploy['commit'][:12]), 'the deploy event')
    configure(release_config, 'release as before')
    # ^ Routines (docs/tasks.md "Routines"): the manager's `wsx task routine` writes one only
    # with the owner's words; a run that ends is an event and the ledger's row, and the TUI
    # shows it while the scheduler has no project registry.
    assert js('task', 'ledger')['routines'] == []
    add = ('wsx task routine add nightly --cron "0 3 * * *" --arg sh --arg=-c --arg "echo boom; exit 3"'
           ' -p demo --json')
    text = reply_to(js('task', 'say', 'RUN ' + add)['id'])['text']
    assert '-> 1 ' in text and 'nothing was run' in text, text
    assert js('task', 'routine', 'list')['routines'] == []
    text = reply_to(js('task', 'say', 'add the nightly routine\nRUN %s --owner-said OWNER_SAID' % add)['id'])['text']
    assert '-> 0 ' in text, text
    text = reply_to(js('task', 'say', 'RUN wsx task routine run nightly -p demo --json')['id'])['text']
    assert '-> 0 ' in text, text
    # ^ A failure of a routine that posts is an event for the owner: one line, the output in it.
    wait(lambda: events_with('routine nightly failed (exit 3); for the owner; output ends: boom'), 'the routine event')
    # ^ Two more failures: the handover after the rotation below folds all three in one line.
    for count in (2, 3):
        text = reply_to(js('task', 'say', 'RUN wsx task routine run nightly -p demo --json')['id'])['text']
        assert '-> 0 ' in text, text
        wait(lambda: len(events_with('routine nightly failed (exit 3)')) == count, 'routine event %d' % count)
    row = js('task', 'ledger')['routines'][0]
    assert (row['name'], row['trigger'], row['enabled'], row['running']) == ('nightly', '0 3 * * *', True, False), row
    assert row['next_run_unix_ms'] > time.time() * 1000, row
    assert (row['last']['status'], row['last']['exit']) == ('failed', 3) and row['last']['duration_ms'] >= 0, row
    assert re.search(r'sched \[1\]', tui_text(wsx, env, project, r'sched \[1\]'))
    # ^ The close events of the tasks above may still run a turn: SLOW must start its own.
    manager_settled('the manager idle before SLOW')
    busy = js('task', 'say', 'SLOW')
    first = js('task', 'new', 'NOOP')['id']
    second = js('task', 'new', 'NOOP')['id']
    for n in (first, second):
        wait_state(n, 'ready', 'failed')
    waiting = js('task', 'say', 'STATUS')
    assert waiting['busy'] and 'queued' in waiting['message'], waiting
    merged = wait(lambda: events_with('#%d ready r1' % first, '#%d ready r1' % second), 'merged events')
    assert len(merged) == 1, merged
    # ^ An event names the host's checks before the worker's own words.
    assert '#%d ready r1: NOOP; checks 1 of 1 passed; worker says (unchecked): did: NOOP' % first in merged[0]['text'], merged
    # ^ A turn takes every waiting message of one author: an earlier events message may share it.
    delivered = wait(lambda: (lambda ids: ids if any(merged[0]['id'] in batch for batch in ids) else None)(
        [e['ids'] for e in manager_entries() if e['kind'] == 'delivered']), 'merged events delivered')
    batch = lambda message_id: next(n for n, ids in enumerate(delivered) if message_id in ids)
    assert delivered[batch(waiting['id'])] == [waiting['id']] and batch(waiting['id']) < batch(merged[0]['id']), (
        delivered, waiting['id'], merged[0]['id'],
        [(e.get('id'), e['kind'], e.get('from'), e.get('text', '')[:60]) for e in manager_entries() if e['kind'] in ('message', 'reply')][-12:])
    event_reply = reply_to(merged[0]['id'])
    # ^ A reply to a turn of only events is quiet: the chat shows it as a muted line.
    assert 'noted events' in event_reply['text'] and event_reply.get('quiet') is True, event_reply
    # ^ Nothing waits for the manager's review: the events turn runs on eventModel in the same
    # process, and the next owner turn switches back (docs/tasks.md "Manager").
    event_turn = turn_with('Events (message %d)' % merged[0]['id'])
    assert event_turn['model'] == 'fake-events', event_turn
    # ^ The owner turn before it already showed both tasks as changed; the events turn has no change.
    status_turn = turn_with('Owner (message %d)' % waiting['id'])['message']
    assert '- changed since your last turn:\n' in status_turn and '#%d ready r1' % first in status_turn, status_turn
    assert '- open: 2 ready;' in event_turn['message'] and 'changed since' not in event_turn['message'], event_turn
    assert event_reply['usage']['model'] == 'fake-events' and event_reply['usage']['cost_micro_usd'] == 1000, event_reply
    after_events = js('task', 'say', 'STATUS after events')
    reply_to(after_events['id'])
    owner_turn = turn_with('STATUS after events')
    assert owner_turn['model'] == 'fake-manager' and owner_turn['session'] == event_turn['session'], owner_turn
    switches = [t['set_model'] for t in traces() if 'set_model' in t and t['session'] == event_turn['session']]
    assert switches[-2:] == ['fake-events', 'fake-manager'], switches
    # A decided item's capture files stay however old it is; only the project's byte budget
    # (`tasks.captureBudgetMb`) removes them, skipped and failed items before passed ones, oldest
    # first, never an open item's (docs/tasks.md "To check"). Staged: the video item skipped 8
    # days ago, the shot item passed 9 days ago, #recent's item open for 8 days.
    checks_file = Path(show(video)['report']).parent.parent / 'checks.jsonl'
    current = {}
    for line in checks_file.read_text().splitlines():
        current[json.loads(line)['id']] = json.loads(line)
    video_item, recent_item, shot_item = (next(r for r in current.values() if r['task'] == n and r.get('capture')) for n in (video, recent, shot))
    assert shot_item['state'] == 'passed' and video_item['state'] == 'skipped', (shot_item, video_item)
    day_ms = 24 * 3600 * 1000
    eight_days_ago = int(time.time() * 1000) - 8 * day_ms

    def append_rows(*rows):
        with open(checks_file, 'a') as out:
            fcntl.flock(out, fcntl.LOCK_EX)
            out.write(''.join(json.dumps(row) + '\n' for row in rows))

    append_rows(dict(video_item, unix_ms=eight_days_ago), dict(recent_item, state='open', unix_ms=eight_days_ago),
                dict(shot_item, unix_ms=eight_days_ago - day_ms))
    captures_of = {n: Path(show(n)['report']).parent / 'captures' for n in (video, recent, shot)}
    assert sorted(p.name for p in captures_of[video].iterdir()) == ['r1-1.mp4', 'r1-1.poster.png']
    # Stop the manager host, finish a task while it is down, start it again.
    manager_socket = js('task', 'manager')['socket']
    assert len(manager_socket) < 104, manager_socket
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    wait(lambda: not Path(manager_socket).exists() or
         subprocess.run(['nc', '-zU', manager_socket]).returncode != 0, 'manager down')
    unseen = js('task', 'new', 'WORK late.txt')['id']
    wait_state(unseen, 'ready', 'failed')
    assert js('task', 'manager')['state'] == 'started'
    append_rows(dict(recent_item, unix_ms=int(time.time() * 1000)))
    late = wait(lambda: events_with('#%d ready r1' % unseen), 'event after restart')
    # ^ The restart swept (the default budget is 1 GiB): nothing decided 8 or 9 days ago went.
    assert sorted(p.name for p in captures_of[video].iterdir()) == ['r1-1.mp4', 'r1-1.poster.png']
    assert [p.name for p in captures_of[recent].iterdir()] == ['r1-1.png'] and [p.name for p in captures_of[shot].iterdir()] == ['r1-1.png']
    after_sweep = [json.loads(line) for line in checks_file.read_text().splitlines()]
    assert [r for r in after_sweep if r['id'] == video_item['id']][-1]['capture'] == 'captures/r1-1.mp4', after_sweep
    late_reply = reply_to(late[0]['id'])
    assert 'noted events' in late_reply['text']
    # ^ The resumed process restored the session's spend: the reply still costs only its own turn.
    resumed_usage = late_reply['usage']
    assert resumed_usage['cost_micro_usd'] == 1000 and resumed_usage['session_cost_micro_usd'] > 5000, resumed_usage
    # Over the budget the sweep removes the oldest decided captures: padded to 400 KiB each,
    # the three take 1.2 MB of the 1 MB budget. The skipped video (8 days old) goes before the
    # passed shot (9 days old, older), which stays with the open item's capture, as the files
    # left fit the budget.
    for n, name in ((video, 'r1-1.mp4'), (recent, 'r1-1.png'), (shot, 'r1-1.png')):
        with open(captures_of[n] / name, 'ab') as pad:
            pad.write(b'\0' * (400 * 1024))
    plain_config = (project / 'wsx.config.yml').read_text()
    configure(plain_config.replace('  minFreeGb: 0\n', '  minFreeGb: 0\n  captureBudgetMb: 1\n'), 'capture budget')
    manager_socket = js('task', 'manager')['socket']
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    wait(lambda: not Path(manager_socket).exists() or
         subprocess.run(['nc', '-zU', manager_socket]).returncode != 0, 'manager down')
    reply_to(js('task', 'say', 'STATUS budget')['id'])
    wait(lambda: not any(captures_of[video].iterdir()), 'the skipped video capture removed over the budget', 30)
    assert [p.name for p in captures_of[recent].iterdir()] == ['r1-1.png'] and [p.name for p in captures_of[shot].iterdir()] == ['r1-1.png']
    assert (captures_of[recent] / 'r1-1.png').stat().st_size > 400 * 1024
    configure(plain_config, 'capture budget done')
    # The same instructions resume the conversation; other ones start a new conversation that
    # opens with the last exchanges (Claude keeps a conversation's first system prompt). A
    # state written by a host from before the instructions digest stands for changed ones.
    launches = [t for t in traces() if t.get('launch') and t.get('manager')]
    assert launches[-1]['resume'], launches[-1]
    manager_socket = js('task', 'manager')['socket']
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    wait(lambda: not Path(manager_socket).exists() or
         subprocess.run(['nc', '-zU', manager_socket]).returncode != 0, 'manager down')
    default_manager_model()
    state_file = manager_log.parent / 'state.json'
    older = json.loads(state_file.read_text())
    assert older['agent_session'] and older.pop('instructions'), older
    state_file.write_text(json.dumps(older))
    # A fresh conversation opens with the notes; a linked notes file is not read.
    notes = manager_log.parent / 'notes'
    notes.mkdir(exist_ok=True)

    def put(name, text=None):
        # ^ decisions.md and context.md are views of the records: a fixture is written as old notes (forget_notes).
        forget_notes(notes)
        if text is None:
            (notes / name).unlink(missing_ok=True)
        else:
            (notes / name).write_text(text)

    put('decisions.md', '- 2026-10-22 keep the store in sqlite\n')
    outside = manager_log.parent.parent / 'outside-context.md'
    outside.write_text('SECRET-OUTSIDE\n')
    put('context.md')
    (notes / 'context.md').symlink_to(outside)
    changed = js('task', 'say', 'STATUS after the change')
    reply_to(changed['id'])
    fresh = [t for t in traces() if t.get('launch') and t.get('manager')][-1]
    assert not fresh['resume'] and fresh['session'] != launches[-1]['session'], fresh
    rotated = [e for e in manager_entries() if e['kind'] == 'rotated']
    assert rotated[-1].get('reason') == "the manager's instructions changed", rotated
    # ^ The host rotated as it took the waiting message: the log keeps the message first, and the
    # chat shows the divider above it, in the JSON entries and in the rows (`chat::chat_window`).
    raw = manager_entries()
    at_rotation = max(at for at, e in enumerate(raw) if e['kind'] == 'rotated')
    assert next(at for at, e in enumerate(raw) if e.get('id') == changed['id']) < at_rotation, raw[-8:]
    shown = [(e['kind'], e.get('id')) for e in js('task', 'chat')['entries']]
    divider = max(at for at, (kind, _) in enumerate(shown) if kind == 'rotated')
    assert divider < shown.index(('message', changed['id'])), shown[-8:]
    rows = cli('task', 'chat').stdout
    assert rows.rindex("(New conversation: the manager's instructions changed") < rows.index('[%d] You: STATUS after the change' % changed['id']), rows[-800:]
    opened = turn_with('STATUS after the change')['message']
    # ^ The notes come first, then the handover.
    assert opened.startswith("The owner's decisions") and '\n\nHandover (this is a new conversation' in opened and re.search(r'You \(reply \d+\): noted events', opened) and 'Owner (message ' in opened, opened
    # ^ The three failures before the rotation are one line: the newest and how many.
    routines = opened.split('\nRoutine results (')[1].split('\n\n')[0].split('\n')[1:]
    assert routines == ['routine nightly failed (exit 3); 3 failures, never succeeded'], opened
    assert 'keep the store in sqlite' in opened and 'SECRET-OUTSIDE' not in opened, opened
    # ^ The handover is also an episode of the project's memory, written in that turn (docs/memory.md "Capture").
    handovers = [h for h in memory_records(cli('memory', 'search', 'Handover new conversation exchanges', '--json').stdout) if h['slug'].startswith('handover-')]
    assert handovers and handovers[0]['kind'] == 'episode' and 'Handover (this is a new conversation' in handovers[0]['excerpt'], handovers

    # Continuity (docs/tasks.md "Manager"): notes share their budget, idle large conversations
    # rotate, and a warm small one takes changed instructions as a message.
    def stop_manager():
        sock = js('task', 'manager')['socket']
        cli('session', 'delete', 'task-manager', '-p', 'demo')
        wait(lambda: not Path(sock).exists() or subprocess.run(['nc', '-zU', sock]).returncode != 0, 'manager down')

    def edit_state(**fields):
        stop_manager()
        data = json.loads(state_file.read_text())
        data.update(fields)
        state_file.write_text(json.dumps(data))

    # ^ A decisions.md of 12 KiB takes the start budget except what a short context.md uses.
    put('context.md')
    put('context.md', 'PROJECT-SECTION line\n' + ''.join('- #%d: memo %d\n' % (n, n) for n in range(1, 31)))
    put('decisions.md', ''.join('- decision %03d %s\n' % (n, 'x' * 50) for n in range(200)))
    edit_state(instructions=None)
    notes_message = js('task', 'say', 'NOTES BUDGET')
    reply_to(notes_message['id'])
    opened = turn_with('NOTES BUDGET')['message']
    head = opened.split('\n\nHandover')[0]
    assert 'PROJECT-SECTION line' in head and '- #30: memo 30' in head and '- decision 199' in head, head
    assert '- decision 000' not in head and len(head.encode()) < 10 * 1024 + 600, len(head)
    # ^ The decisions it left out are named, with their lines and the file's absolute path.
    assert re.search(r'NOT SHOWN: decisions\.md lines 1-\d+ of 200 .*Read /\S+/notes/decisions\.md', head), head
    assert 'NOT SHOWN: context.md' not in head, head
    # 21b. Restart: a message delivered to a turn that got no reply goes to the next turn once,
    # one with a reply does not (docs/tasks.md "Manager"). The first HANG turn never answers; the
    # host and its agent are killed after `delivered` is logged.
    mark('21b')
    wait(manager_idle, 'the manager idle before HANG', 60)
    answered = js('task', 'say', 'STATUS before the kill')
    reply_to(answered['id'])
    hung = js('task', 'say', 'HANG once')
    wait(lambda: any(e['kind'] == 'delivered' and hung['id'] in e['ids'] for e in manager_entries()), 'HANG delivered')
    assert not [e for e in manager_entries() if e['kind'] == 'reply' and hung['id'] in e['to']]
    host_id = show(unseen)['record']['project_id']
    manager_host = next(int(line.split()[0]) for line in leftovers('task manager-host %s' % host_id)
                        if line.endswith('task manager-host %s' % host_id))
    os.kill(manager_host, 9)
    try:  # ^ The host's death may already have stopped the agent tree.
        os.kill([t for t in traces() if t.get('launch') and t.get('manager')][-1]['pid'], 9)
    except ProcessLookupError:
        pass
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    assert js('task', 'manager')['state'] == 'started'
    reply_to(hung['id'])
    turns_of = lambda text: [t for t in traces() if t.get('manager') and text in str(t.get('message', ''))]
    assert len(turns_of('HANG once')) == 2 and len(turns_of('STATUS before the kill')) == 1, (
        turns_of('HANG once'), turns_of('STATUS before the kill'))
    # ^ A restart after the reply delivers neither again.
    stop_manager()
    reply_to(js('task', 'say', 'STATUS after the second start')['id'])
    assert len(turns_of('HANG once')) == 2 and len(turns_of('STATUS before the kill')) == 1
    assert len([e for e in manager_entries() if e['kind'] == 'reply' and hung['id'] in e['to']]) == 1

    # 21a. Notes budget (docs/tasks.md "Notes budget"): a context.md over what a fresh conversation
    # shows is never cut silently. The fixture is pizzahouse's: 60 lines, about 15 KB, and the
    # fact the manager got wrong sits among the oldest memos.
    mark('21a')

    def pizza(first_line):
        def word(n):
            return 'qz' + ''.join(chr(97 + n // 26 ** k % 26) for k in range(4))
        fact = '- #7: restarted Claude.app with open -g -a Claude from the Background session on 2026-10-31'
        memos = [fact if n == 7 else '- #%d: %s notes %s %s' % (n, word(n), 'fixture detail ' * 15, word(n + 100))
                 for n in range(1, 58)]
        return first_line + '\n\nPizzahouse sells pizza.\n' + '\n'.join(memos) + '\n'

    put('decisions.md', '- 2026-10-22 keep the store in sqlite\n')
    put('context.md')
    reply_to(js('task', 'say', 'ZEBRAFISH quarterly')['id'])
    edit_state(instructions=None)
    put('context.md', pizza('# pizzahouse'))
    fixture = (notes / 'context.md').read_text().splitlines()
    assert len(fixture) == 60 and 14000 < len('\n'.join(fixture)) < 17000, len(fixture)
    reply_to(js('task', 'say', 'NOTES PIZZA')['id'])
    first_turn = turn_with('NOTES PIZZA')['message']
    reply_to(js('task', 'say', 'STATUS ordinary')['id'])
    ordinary_turn = turn_with('STATUS ordinary')['message']
    print('MEASURE first turn %d bytes, ordinary turn %d bytes (~%d and ~%d tokens)' % (
        len(first_turn.encode()), len(ordinary_turn.encode()), len(first_turn.encode()) // 4, len(ordinary_turn.encode()) // 4))
    notice = re.search(r'NOT SHOWN: context\.md lines ([0-9, -]+) of 60 .*Read (/\S+/notes/context\.md) for them', first_turn)
    assert notice, first_turn
    # ^ Every line of the file is in the first message or inside a named range.
    hidden = set()
    for part in notice.group(1).split(', '):
        low, _, high = part.partition('-')
        hidden |= set(range(int(low), int(high or low) + 1))
    head_text = first_turn.split('Your project context (context.md):')[1].split('NOT SHOWN')[0]
    for number, line in enumerate(fixture, 1):
        assert line.strip() == '' or line in head_text or number in hidden, (number, line)
        assert not (line.startswith('- #') and line in head_text and number in hidden), number
    assert fixture[9] not in head_text and 10 in hidden and len(hidden) > 10, (sorted(hidden), first_turn)
    # ^ The notice names the file, whose path is absolute and is the one the manager reads.
    assert notice.group(2) == str((notes / 'context.md').resolve()) or notice.group(2).endswith('/manager/notes/context.md'), notice.group(2)
    # ^ The first message holds at most the 10 KiB budget of the notes.
    assert len(head_text.encode()) < 10 * 1024 + 400, len(head_text)
    # ^ A fresh conversation, an owner message whose key word matches a line it did not show:
    # that line is attached, and nothing else of the notes. A message with no such word gets none.
    edit_state(instructions=None)
    put('context.md', pizza('# stubborn'))
    attached = js('task', 'say', 'Did you restart Claude.app yesterday?')
    reply_to(attached['id'])
    matched = turn_with('Did you restart Claude.app yesterday?')['message']
    block = matched.split('Notes that match this message')[1].split('\n\n')[0]
    assert 'context.md:10: - #7: restarted Claude.app with open -g -a Claude' in block, matched
    assert block.count('\n- ') <= 5 and len(block.split('):\n', 1)[1].encode()) <= 1024, block
    assert '/notes/context.md' in block.split('\n')[0] and 'qz' not in block, block
    reply_to(js('task', 'say', 'GLOBETROTTER plain')['id'])
    assert 'Notes that match' not in turn_with('GLOBETROTTER plain')['message']
    shown_word = fixture[-1].split()[2]
    reply_to(js('task', 'say', 'what is %s' % shown_word)['id'])
    assert 'Notes that match' not in turn_with('what is %s' % shown_word)['message']
    # ^ Memo lines: one line of at most 240 characters, no 60-line cap, and past 32 KiB the
    # oldest go to context-archive.md, which the fresh conversation is told about.
    put('context.md', '# stubborn\n' + ''.join('- #%d: short memo %d\n' % (n, n) for n in range(100, 175)))
    long_memo = 'x' * 400
    long_task = js('task', 'new', 'WORK long.txt MEMO ' + long_memo)['id']
    assert wait_state(long_task, 'ready', 'failed')['state'] == 'ready'
    js('task', 'close', long_task, '--discard')
    folded = wait(lambda: [l for l in (notes / 'context.md').read_text().splitlines() if l.startswith('- #%d: ' % long_task)], 'the long memo')[0]
    lines_now = (notes / 'context.md').read_text().splitlines()
    # ^ `view::clip` counts the ellipsis: 239 characters and `…` make the 240.
    assert folded == '- #%d: %s…' % (long_task, 'x' * 239) and len(lines_now) == 77, (folded, len(lines_now))
    assert not (notes / 'context-archive.md').exists()
    put('context.md', '# stubborn\n' + ''.join('- #%d: %s %d\n' % (n, 'y' * 230, n) for n in range(1, 140)))
    big_task = js('task', 'new', 'WORK big.txt MEMO the newest memo')['id']
    assert wait_state(big_task, 'ready', 'failed')['state'] == 'ready'
    js('task', 'close', big_task, '--discard')
    # ^ The archive is written first and context.md after it: wait for the file read next.
    archive = notes / 'context-archive.md'
    wait(lambda: '- #%d: the newest memo' % big_task in (notes / 'context.md').read_text(), 'the newest memo in context.md')
    kept = (notes / 'context.md').read_text()
    moved = archive.read_text()
    assert len(kept.encode()) <= 32 * 1024 and '- #1: ' not in kept and moved.startswith('- #1: ') and '- #%d: the newest memo' % big_task in kept, (len(kept), moved[:40])
    assert len(kept.splitlines()) + len(moved.splitlines()) == 141, (len(kept.splitlines()), len(moved.splitlines()))
    edit_state(instructions=None)
    reply_to(js('task', 'say', 'STATUS after the archive')['id'])
    opened_archive = turn_with('STATUS after the archive')['message']
    assert re.search(r'in (/\S+/notes/context-archive\.md)\.', opened_archive) and 'NOT SHOWN: context.md' in opened_archive, opened_archive
    put('context-archive.md')
    put('context.md', 'PROJECT-SECTION line\n' + ''.join('- #%d: memo %d\n' % (n, n) for n in range(1, 31)))
    # ^ Idle past 60 min with a context over 60k: a fresh conversation, with the reason logged.
    edit_state(last_reply_ms=int((time.time() - 7200) * 1000), context_tokens=70000)
    idle_message = js('task', 'say', 'IDLE LARGE')
    reply_to(idle_message['id'])
    rotated = [e for e in manager_entries() if e['kind'] == 'rotated']
    assert rotated[-1].get('reason') == 'idle over 60 min with 70k context', rotated
    # ^ The launch just before the turn that holds the message: a later launch is not the rotation's.
    idle_turn = turn_with('IDLE LARGE')
    idle_launch = [t for t in traces()[:traces().index(idle_turn)] if t.get('launch') and t.get('manager')][-1]
    assert not idle_launch['resume'] and 'Handover' in idle_turn['message'], (idle_launch, idle_turn)
    # ^ An exchange of the conversation before the rotation, older than the handover's, matches too.
    reply_to(js('task', 'say', 'ZEBRAFISH revisit')['id'])
    earlier = turn_with('ZEBRAFISH revisit')['message']
    assert re.search(r'earlier conversation: Owner \(message \d+\): ZEBRAFISH quarterly', earlier), earlier
    # ^ Warm and small, with other instructions: the same conversation, the new ones as a message.
    rotations = len([e for e in manager_entries() if e['kind'] == 'rotated'])
    edit_state(instructions='stale-digest')
    warm_message = js('task', 'say', 'WARM CHANGE')
    reply_to(warm_message['id'])
    warm_launch = [t for t in traces() if t.get('launch') and t.get('manager')][-1]
    assert warm_launch['resume'], warm_launch
    assert len([e for e in manager_entries() if e['kind'] == 'rotated']) == rotations
    assert turn_with('WARM CHANGE')['message'].startswith('Your instructions changed'), turn_with('WARM CHANGE')
    # ^ An idle rotation whose fresh turn meets a usage limit: that conversation has no context yet,
    # so the turn after the reset continues it, where the held message is. Before, the rotation kept
    # the old size and the idle rule rotated again at each try (5 times in a row on 2026-11-10),
    # into a conversation that never saw the message.
    edit_state(last_reply_ms=int((time.time() - 7200) * 1000), context_tokens=70000)
    rotations = len([e for e in manager_entries() if e['kind'] == 'rotated'])
    held_back = js('task', 'say', 'LIMIT 1 BACK INTO A LIMIT')
    reply_to(held_back['id'], 60)
    assert len([e for e in manager_entries() if e['kind'] == 'rotated']) == rotations + 1, manager_entries()[-8:]
    limited_turn = turn_with('BACK INTO A LIMIT')
    after_reset = [t for t in traces()[traces().index(limited_turn) + 1:]
                   if t.get('manager') and str(t.get('message', '')).startswith('The usage limit has reset')]
    assert after_reset and after_reset[0]['session'] == limited_turn['session'], (limited_turn, after_reset)
    put('decisions.md', '- 2026-10-22 keep the store in sqlite\n')
    # 21a3. decisions.md over what a fresh conversation shows (docs/tasks.md "Notes budget"): the rules
    # that fit the larger part of the start budget show, what is left out is named with the archive, and
    # an owner message whose rare word is only in an archived rule gets that line. wsx asks the manager
    # to rewrite nothing: the rules are records, and decisions-archive.md holds the replaced ones.
    mark('21a3')

    def rules(count, first=0):
        # ^ One day per rule, oldest first: the views order the rules by their date.
        day = lambda n: datetime.date(2026, 10, 1) + datetime.timedelta(days=n)
        return ['- %s rule %03d: %s' % (day(n), n, 'owner words ' * 6) for n in range(first, first + count)]

    old_rules = rules(90)
    old_rules[3] = '- 2026-10-04 rule 003: never render the 3d scene with tessellation'
    put('context.md', 'PROJECT-SECTION line\n- #1: memo 1\n')
    put('decisions-archive.md', '\n'.join(old_rules) + '\n')
    put('decisions.md', '\n'.join(rules(50, 200)) + '\n')
    # ^ Between 4 and 6 KiB beside a long context.md: every rule is in the first message.
    put('context.md', pizza('# stubborn'))
    edit_state(instructions=None)
    reply_to(js('task', 'say', 'RULES mid')['id'])
    mid = turn_with('RULES mid')['message']
    assert 'rule 200' in mid and 'rule 249' in mid and 'NOT SHOWN: decisions.md' not in mid, mid
    # ^ Still over after the rewrite: the newest rules fill 6 KiB, context.md keeps at least 3.5 KiB,
    # and what is left out is named with the archive.
    put('decisions.md', '\n'.join(rules(110, 300)) + '\n')
    edit_state(instructions=None)
    reply_to(js('task', 'say', 'RULES big')['id'])
    big = turn_with('RULES big')['message'].split('\n\nHandover')[0]
    shown = big.split('Your project context (context.md):')[0]
    assert 'rule 409' in shown and 'rule 300' not in shown and 5800 < len(shown.encode()) < 7000, (len(shown), shown[:300])
    assert re.search(r'NOT SHOWN: decisions\.md lines 1-\d+ of 110 .*Read /\S+/notes/decisions\.md', shown), shown
    assert re.search(r'Older lines that left decisions\.md for its size are in /\S+/notes/decisions-archive\.md\.', shown), shown
    context_part = big.split('Your project context (context.md):')[1]
    assert 'Pizzahouse sells pizza.' in context_part and len(context_part.split('NOT SHOWN')[0].encode()) > 3500, context_part[:200]
    # ^ An owner message whose rare word is only in an archived rule gets that line.
    reply_to(js('task', 'say', 'Did we settle tessellation?')['id'])
    block = turn_with('Did we settle tessellation?')['message'].split('Notes that match this message')[1].split('\n\n')[0]
    assert 'decisions-archive.md:4: - 2026-10-04 rule 003: never render the 3d scene with tessellation' in block, block
    # ^ `wsx task search` finds the rule of the archive.
    kept = js('task', 'search', '-p', 'demo', 'tessellation')['hits']
    assert [h['kind'] for h in kept if h['by'] == 'notes'] == ['decision'], kept
    put('decisions.md', '- 2026-10-22 keep the store in sqlite\n')
    put('decisions-archive.md')
    put('context.md', 'PROJECT-SECTION line\n' + ''.join('- #%d: memo %d\n' % (n, n) for n in range(1, 31)))
    # ^ The size limit by adapter (docs/tasks.md "Manager"): with no tasks.manager.rotateTokens a
    # manager on Claude keeps its conversation to 400000, one on Pi to 150000; a set value wins.
    # The config is read at the host's start, so each case stops the manager first.
    def rotated_entries():
        return [e for e in manager_entries() if e['kind'] == 'rotated']

    def rotates_at(manager_line, adapter, below, above, tokens_of_above):
        stop_manager()
        configure(limit_config.replace(manager_line_now, manager_line), 'manager limit ' + adapter)
        # ^ A first turn takes whatever a changed adapter or instructions rotate.
        reply_to(js('task', 'say', 'STATUS start on ' + adapter)['id'])
        before = len(rotated_entries())
        # ^ A second turn after the first shows the conversation went on: a rotation is logged
        # right after the reply that crosses the limit.
        for context in below:
            reply_to(js('task', 'say', '%s %d' % (ctx_word[adapter], context))['id'])
            reply_to(js('task', 'say', 'STATUS after %d' % context)['id'])
            assert len(rotated_entries()) == before, (adapter, context, rotated_entries()[before:])
        reply_to(js('task', 'say', '%s %d' % (ctx_word[adapter], above))['id'])
        wait(lambda: len(rotated_entries()) == before + 1, '%s rotated at %d' % (adapter, above))
        assert rotated_entries()[-1]['tokens'] == tokens_of_above, rotated_entries()[-1]

    ctx_word = {'claude': 'CTX', 'pi': 'PICTX'}
    limit_config = (project / 'wsx.config.yml').read_text()
    manager_line_now = next(line for line in limit_config.splitlines() if line.startswith('  manager:'))
    assert 'rotateTokens: 100000' in manager_line_now, manager_line_now
    plain = manager_line_now.replace(', rotateTokens: 100000', '')
    # ^ Claude, not set: 200000 is past 150000 and under 400000, so no rotation; 450000 rotates.
    rotates_at(plain, 'claude', [200000], 450000, 450000)
    # ^ Claude, set to 100000: it rotates at 200000.
    rotates_at(manager_line_now, 'claude', [], 200000, 200000)
    # ^ Pi, not set: 120000 stays, 200000 rotates (the 150000 limit).
    pi_plain = plain.replace('{ ', '{ adapter: pi, model: fake/pi-model, ', 1).replace('eventModel: fake-events', 'eventModel: off')
    rotates_at(pi_plain, 'pi', [120000], 200000, 200000)
    # ^ Pi, set to 250000: 200000 stays.
    pi_set = pi_plain.replace(' }', ', rotateTokens: 250000 }', 1)
    rotates_at(pi_set, 'pi', [200000], 300000, 300000)
    # ^ Back on Claude with the fixture's config for the parts below.
    stop_manager()
    configure(limit_config, 'manager limit restored')
    reply_to(js('task', 'say', 'STATUS back on claude')['id'])
    # ^ Dynamic routines (docs/tasks.md "Routines"), after the handover above so it lists only
    # nightly: an event routine runs on the task change wsx fires, and `wsx task post` puts its
    # output in the chat with no manager turn.
    def posted(title):
        # ^ A post logs its message, then `delivered`, then the reply: match the reply by `to`.
        entries = manager_entries()
        ids = {e['id'] for e in entries if e['kind'] == 'message' and e['from'] == 'events' and e['text'] == title}
        return [e for e in entries if e['kind'] == 'reply' and 'usage' not in e and ids & set(e['to'])]
    for early in ('0m', '8d'):
        text = reply_to(js('task', 'say', 'RUN wsx task checkin -p demo --in %s --why "too soon"' % early)['id'])['text']
        assert '-> 1 ' in text and 'between 1 minute and 7 days' in text, text
    # ^ A check-in is a timer in the manager host's state, not a routine (docs/tasks.md "Check-in"):
    # a second one replaces the first, nothing is listed in Routines, it fires as an event with its
    # why, and a host restart keeps one that comes due while the host is down.
    pending = lambda: json.loads(state_file.read_text()).get('checkin')
    text = reply_to(js('task', 'say', 'RUN wsx task checkin -p demo --in 2h --why "first look"')['id'])['text']
    assert '-> 0 check-in at ' in text, text
    saved = wait(pending, 'the first check-in is saved')
    assert saved['why'] == 'first look' and saved['at_ms'] > time.time() * 1000 + 3600 * 1000, saved
    text = reply_to(js('task', 'say', 'RUN wsx task checkin -p demo --in 1m --why "look at the nightly failures"')['id'])['text']
    assert '-> 0 check-in at ' in text, text
    replaced = pending()
    assert replaced['why'] == 'look at the nightly failures' and replaced['at_ms'] < time.time() * 1000 + 70 * 1000, replaced
    # ^ Only `nightly` is listed: a check-in is no routine.
    listed = lambda: [r['name'] for r in js('task', 'ledger')['routines']] + [r['routine']['name'] for r in js('task', 'routine', 'list')['routines']]
    assert listed() == ['nightly', 'nightly'], listed()
    wait(lambda: events_with('check-in: look at the nightly failures'), 'the check-in event', 150)
    assert not events_with('first look') and pending() is None, (events_with('first look'), pending())
    # ^ Due while the host is down: it fires late, once, when the host starts.
    reply_to(js('task', 'say', 'RUN wsx task checkin -p demo --in 1m --why "came due while down"')['id'])
    stop_manager()
    assert pending()['why'] == 'came due while down'
    wait(lambda: pending()['at_ms'] < time.time() * 1000, 'the check-in is past due', 90)
    assert not events_with('came due while down')
    reply_to(js('task', 'say', 'STATUS after the check-in came due')['id'])
    wait(lambda: events_with('check-in: came due while down'), 'the late check-in event', 30)
    assert pending() is None and listed() == ['nightly', 'nightly'], (pending(), listed())
    # ^ Routines recur: no `--at`, and nothing else names a one-shot.
    refused = cli('task', 'routine', 'add', 'once', '-p', 'demo', '--at', '23:59', '--arg=true', ok=False)
    assert 'unexpected argument' in refused.stderr, refused.stderr
    refused = cli('task', 'checkin', '-p', 'demo', '--in', '1m', '--at', '23:59', '--why', 'x', ok=False)
    assert 'cannot be used with' in refused.stderr, refused.stderr
    js('task', 'routine', 'add', 'closed', '-p', 'demo', '--event', 'task.closed', '--arg=sh', '--arg=-c',
       '--arg=printf "%%s" "$WSX_EVENT_PAYLOAD" | %s task post -p demo --title closed' % wsx)
    # ^ Only a short reply to events is quiet: one with a table is a message (docs/tasks.md "Manager").
    tabled = js('task', 'new', 'NOOP TABLE')['id']
    wait_state(tabled, 'ready')
    table_reply = reply_to(wait(lambda: events_with('#%d ready r1' % tabled), 'the event of a task that asks for a table')[0]['id'])
    stored_reply = next(e for e in manager_entries() if e['kind'] == 'reply' and e['id'] == table_reply['id'])
    assert 'Where things stand' in stored_reply['text'] and 'quiet' not in stored_reply and 'quiet' not in table_reply, stored_reply
    js('task', 'close', tabled)
    closed = wait(lambda: posted('closed'), 'the closed event routine post', 60)[0]
    assert json.loads(closed['text'])['task'] == tabled, closed
    assert not events_with('routine closed'), 'a routine that prints nothing costs no turn'
    # ^ The retrospect is one such routine: it posts counts of the chat above (the three
    # `routine run nightly` asks) and prints nothing.
    state_dir = manager_log.parent.parent
    js('task', 'routine', 'add', 'retrospect', '-p', 'demo', '--cron', '0 6 * * *',
       *('--arg=%s' % word for word in (sys.executable, '-B', ROOT / 'scripts' / 'retrospect.py', state_dir,
                                        '-p', 'demo', '--no-model', '--wsx', wsx)))
    js('task', 'routine', 'run', 'retrospect', '-p', 'demo')
    report = wait(lambda: posted('retrospect'), 'the retrospect post', 60)[0]['text']
    assert report.startswith('Since ') and 'You asked 3 times: "RUN wsx task routine run nightly -p demo --json"' in report, report
    quoting = re.search(r'\nQuotes: (\d+) of (\d+) replies', report)
    assert quoting and 0 < int(quoting[1]) < int(quoting[2]), report
    assert 'not judged (--no-model)' in report and Path(report.split('Detail: ')[1].strip()).is_file(), report
    assert not events_with('routine retrospect'), 'the retrospect prints nothing'
    # ^ A run's end is an event for the owner by default, and the manager answers it with a
    # message: the result, the end of its output and a link to each file of the main checkout it
    # names. `--no-post` keeps it an event for the manager's own work; a success that prints
    # nothing gives nothing.
    board = project / 'routine-out' / 'board.html'
    board.parent.mkdir(exist_ok=True)
    board.write_text('<p>board</p>\n')
    js('task', 'routine', 'add', 'board', '-p', 'demo', '--cron', '0 5 * * *', '--arg=sh', '--arg=-c',
       '--arg=echo "board at routine-out/board.html and missing/none.html"; echo \'{"ok": true}\'')
    js('task', 'routine', 'add', 'hushed', '-p', 'demo', '--cron', '0 5 * * *', '--no-post', '--arg=echo', '--arg=hushed words')
    js('task', 'routine', 'add', 'silent', '-p', 'demo', '--cron', '0 5 * * *', '--arg=true')
    rows = {r['name']: r for r in js('task', 'ledger')['routines']}
    assert rows['board']['post'] is True and rows['hushed']['post'] is False, rows
    js('task', 'routine', 'run', 'board', '-p', 'demo')
    event_message = wait(lambda: events_with('routine board succeeded after ', '; for the owner; output ends: '),
                         lambda: 'the board routine event (routine: %s; log end: %s)' % (
                             next(r for r in js('task', 'ledger')['routines'] if r['name'] == 'board'),
                             [(e['kind'], str(e.get('text', ''))[:160]) for e in manager_entries()[-6:]]), 60)[0]
    event = next(line for line in event_message['text'].split('\n') if line.startswith('routine board'))
    assert '{"ok": true}' in event and '\n' not in event, event
    assert '; files: [board.html](files/routine-out/board.html)' in event and 'none.html]' not in event, event
    message = reply_to(event_message['id'])
    assert 'usage' in message and not message.get('quiet') and len(message['text']) < 200, message
    assert message['text'].endswith('[board.html](files/routine-out/board.html)'), message
    for name in ('hushed', 'silent'):
        js('task', 'routine', 'run', name, '-p', 'demo')
    hushed = wait(lambda: events_with('routine hushed succeeded after', 'hushed words'), 'the --no-post event', 60)[0]
    assert 'for the owner' not in hushed['text'], hushed
    assert reply_to(hushed['id']).get('quiet'), 'a --no-post event is the manager\'s own work'
    wait(lambda: all(r['last'] and not r['running'] for r in js('task', 'ledger')['routines']
                     if r['name'] in ('board', 'hushed', 'silent')), 'the three routines ran', 60)
    time.sleep(3)
    assert len(events_with('routine board')) == 1 and not posted('routine board'), 'one event, no raw post'
    assert not posted('routine hushed') and not posted('routine silent') and not events_with('routine silent'), \
        'a --no-post routine and an empty success give no post'
    # ^ --findings-exit 1,2 (docs/tasks.md "Routines"): exit 1 is `findings`, exit 3 stays `failed`,
    # exit 0 `succeeded`. Each is an event for the owner, not a post; a findings run with no stdout
    # is no empty success: it gives the event too.
    names = ('found', 'broke', 'clean', 'scan')
    for name, code in (('found', 1), ('broke', 3), ('clean', 0), ('scan', 2)):
        output = '' if name == 'scan' else 'echo %s output; ' % name
        js('task', 'routine', 'add', name, '-p', 'demo', '--cron', '0 5 * * *', '--findings-exit', '1,2',
           '--arg=sh', '--arg=-c', '--arg=%sexit %d' % (output, code))
    rows = {r['name']: r for r in js('task', 'ledger')['routines']}
    assert rows['found']['findings_exit'] == [1, 2] and 'findings_exit' not in rows['board'], rows
    for name in names:
        js('task', 'routine', 'run', name, '-p', 'demo')
    wait(lambda: all(r['last'] and not r['running'] for r in js('task', 'ledger')['routines']
                     if r['name'] in names), 'the exit routines ran', 60)
    last = {r['name']: r['last'] for r in js('task', 'ledger')['routines'] if r['name'] in names}
    assert [(n, last[n]['status'], last[n]['exit']) for n in names] == [
        ('found', 'findings', 1), ('broke', 'failed', 3), ('clean', 'succeeded', 0), ('scan', 'findings', 2)], last
    wait(lambda: events_with('routine found found something (exit 1); for the owner; output ends: found output'),
         'the findings event', 60)
    wait(lambda: events_with('routine broke failed (exit 3); for the owner; output ends: broke output'),
         'the failed event', 60)
    scan = wait(lambda: events_with('routine scan found something (exit 2)'), 'the findings event without output', 60)[0]
    assert next(l for l in scan['text'].split('\n') if l.startswith('routine scan')) \
        == 'routine scan found something (exit 2); for the owner', scan
    assert not events_with('routine found failed') and not events_with('routine clean found something')
    time.sleep(3)
    assert not any(posted('routine ' + n) for n in names), 'no raw post of a run'
    for bad in ('0', '256', '1,x'):
        cli('task', 'routine', 'add', 'bad', '-p', 'demo', '--cron', '0 5 * * *', '--findings-exit', bad,
            '--arg=true', ok=False)
    # ^ The TUI chat shows a link's words and opens it (docs/tasks.md "TUI"): `S` makes sure the
    # project shell lives; a post links to it as the session news does. `[` selects the newest
    # link, Enter enters that session and the workspace key comes back to the chat on that link; the next `[`
    # reaches the board's file (an older quote link is not one `[` further), which Enter shows in place of the chat and Esc closes. A routine's post
    # (any `wsx task post`, here the retrospect's) has its bar in the secondary colour.
    tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'), (r'MANAGER', b'S'), (r'(?m)\$ *$', b'')])
    shell_id = next(i for i, label in zip(sessions_at(project, 'id'), sessions_at(project)) if label == 'shell')
    subprocess.run([str(wsx), 'task', 'post', '-p', 'demo', '--title', 'session shell'], env=env, cwd=project,
                   input='[shell in main](session/%d) exited\n' % shell_id, text=True, check=True)
    frames = []
    linked = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'\(m\)anager', b'm'),
        (r'\(\[/\]\)link', b'['),
        (r'(?m)^ MANAGER +Enter: open +\(\[/\]\)link +Esc: clear +\(i\)message', b'\r'),
        (r'(?m)\$ *$', b'\x01w'),
        (r'(?m)^ MANAGER ', b'['),
        (r'(?m)^ MANAGER +Enter: open', b'\r'),
        (r'(?s)files/routine-out/board.html +\d+/\d+\n.*<p>board</p>', b'\x1b'),
        (r'(?m)^ MANAGER +Enter: open', b'\x1b'),
        (r'(?m)^ MANAGER +\(i\)message', b'\x1b'),
        (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')], raw=frames)
    # ^ The chat shows the words, not `[words](session/N)`.
    assert 'shell in main exited' in linked[2] and '(session/' not in linked[2], linked[2]
    assert re.search(r'(?m)^ MANAGER ', linked[5]) and 'shell in main exited' in linked[5], linked[5]
    assert re.search(r'(?m)^ \u2502 shell in main exited', linked[2]), linked[2]
    assert re.search(r'38;2;184;140;240[;0-9]*m\u2502', frames[2].decode('utf-8', 'replace')), 'a wsx post has no secondary bar'
    # ^ A post with a level (`wsx task post --level warn|bad`, docs/tasks.md "Routines") shows the word in its
    # heading and the bar in the warning or error colour; `--key` drops a repeat with exit 0.
    for level, text in (('warn', 'rss over 8 GB'), ('bad', 'rss over 12 GB')):
        subprocess.run([str(wsx), 'task', 'post', '-p', 'demo', '--title', 'guard ' + level, '--level', level,
                        '--key', 'guard-' + level], env=env, cwd=project, input=text + '\n', text=True, check=True)
    repeat = subprocess.run([str(wsx), 'task', 'post', '-p', 'demo', '--title', 'guard warn', '--level', 'warn',
                             '--key', 'guard-warn'], env=env, cwd=project, input='again\n', text=True,
                            capture_output=True, check=True)
    assert repeat.stdout.strip() == 'deduped: guard-warn', repeat.stdout
    bad_level = subprocess.run([str(wsx), 'task', 'post', '-p', 'demo', '--level', 'loud'], env=env, cwd=project,
                               input='x\n', text=True, capture_output=True)
    assert bad_level.returncode == 2, bad_level
    leveled = []
    shown = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'),
        (r'(?s)rss over 8 GB.*rss over 12 GB', b'\x1b'), (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')], raw=leveled)
    assert re.search(r'(?m)\d\d:\d\d +warn *$', shown[2]) and re.search(r'(?m)\d\d:\d\d +bad *$', shown[2]), shown[2]
    assert 'again' not in shown[2], shown[2]
    colored = leveled[2].decode('utf-8', 'replace')
    for rgb, word in (('224;151;91', 'warn'), ('234;105;126', 'bad')):
        assert re.search(r'38;2;%s[;0-9]*m(?:\x1b\[[;0-9]*m)*  %s' % (rgb, word), colored), 'the %s word has no colour' % word
    # ^ An image `![alt](files/<path>)` is a link line with its path: the TUI draws no picture (docs/tasks.md "TUI").
    subprocess.run([str(wsx), 'task', 'post', '-p', 'demo', '--title', 'studio image'], env=env, cwd=project,
                   input='Thumbnail: ![trend](files/.studio-data/trend.jpg)\n', text=True, check=True)
    pictured = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'),
        (r'Thumbnail: files/\.studio-data/trend\.jpg', b'\x1b'), (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')])
    assert '![' not in pictured[2] and '(files/' not in pictured[2], pictured[2]
    # ^ `m` in the chat lists the project's media (docs/tasks.md "TUI", "Media"): the To check captures
    # and the pictures and videos the posts link, newest first. Enter shows an item's details and where
    # its file is, `v` steps through all, images and videos, `t` through the tasks. A file gone from the
    # checkout (and a capture the budget removed) says "removed".
    trend = project / '.studio-data' / 'trend.jpg'
    trend.parent.mkdir(exist_ok=True)
    trend.write_bytes(b'\x89PNG\r\n\x1a\n' + b'\0' * 32)
    subprocess.run([str(wsx), 'task', 'post', '-p', 'demo', '--title', 'studio missing'], env=env, cwd=project,
                   input='Missing: ![gone](files/.studio-data/gone.jpg)\n', text=True, check=True)
    listed = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'), (r'\(m\)edia', b'm'),
        (r'Media +all +all tasks +1/\d+', b'\r'),
        (r'(?s)files/\.studio-data/gone\.jpg +\d+/\d+.*removed: the file is gone', b'\x1b'),
        (r'Media +all +all tasks', b'j\r'),
        (r'(?s)files/\.studio-data/trend\.jpg +\d+/\d+.*image, linked by manager.*file:.*trend\.jpg', b'\x1b'),
        (r'Media +all +all tasks', b'v'), (r'Media +images', b'v'), (r'Media +videos', b'v'),
        (r'Media +all +all tasks', b't'), (r'Media +all +#\d+', b'\x1b'),
        (r'\(i\)message', b'\x1b'), (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')])
    # ^ `(i)message` is in the chat's bar only: the media list's bar also starts with MANAGER, and a second Esc sent
    # before the chat is drawn joins the first.
    trend.unlink()
    (project / '.studio-data').rmdir()
    everything, only_images, only_videos, one_task = listed[3], listed[8], listed[9], listed[11]
    assert re.search(r'img +- +removed +\.studio-data/gone\.jpg', everything) and re.search(r'img +- +manager +\.studio-data/trend\.jpg', everything), everything
    assert re.search(r'img +#%d +passed' % shot, everything) and re.search(r'vid +#%d +removed' % video, everything), everything
    assert 'img ' in only_images and 'vid ' not in only_images, only_images
    assert 'vid ' in only_videos and 'img ' not in only_videos, only_videos
    tasks_shown = set(re.findall(r'(?m)(?:img|vid) +(\S+)', one_task)) - {'-'}
    assert len(tasks_shown) == 1 and re.search(r'(?m)(?:img|vid) +#\d+ ', one_task), one_task
    # ^ The terminal output sets a colour only when it changes, so a bar after the heading in the same colour has
    # no escape of its own: the colour of a bar is the last one set before it.
    bars, last = set(), None
    for token in re.finditer(r'38;2;(\d+;\d+;\d+)|\u2502', colored):
        if token.group(1):
            last = token.group(1)
        elif last:
            bars.add(last)
    for rgb, word in (('224;151;91', 'warn'), ('234;105;126', 'bad')):
        assert rgb in bars, 'the %s post has no coloured bar: %s' % (word, bars)
    board.unlink()
    board.parent.rmdir()
    for name in ('closed', 'retrospect', 'board', 'hushed', 'silent', 'found', 'broke', 'clean'):
        js('task', 'routine', 'delete', name, '-p', 'demo')
    # ^ Session news (docs/tasks.md "Session news"): a project agent session that enters exited,
    # error or blocked is one post with no turn, naming it; a session of a task is no news. A pane
    # reports from inside itself, so the fixture types the report command into a session's shell.
    def agent_session(name):
        made = js('session', 'create', '--name', name, '-p', 'demo')
        return made['session_id'], made['pane_id']

    def pane_of(pane):
        return next(p for p in daemon_call('snapshot')['data']['panes'] if p['id'] == pane)

    def agent_reports(name, pane, provider, state):
        cli('session', 'send-text', name, '"$WSX_AGENT_REPORT_BIN" agent report "$WSX_PANE_ID" --provider %s '
            '--state %s --lifecycle' % (provider, state), '-p', 'demo')
        wait(lambda: (pane_of(pane).get('agent') or {}).get('state') == state, '%s reports %s' % (name, state))

    def news_of(session):
        """The posts that name the session: replies with no usage, each after an events message."""
        titles = {e['id'] for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                  and e['text'].startswith('session ')}
        return [e['text'] for e in manager_entries() if e['kind'] == 'reply' and 'usage' not in e
                and titles & set(e['to']) and '(session/%d)' % session in e['text']]

    def posts_of(session, count, label):
        wait(lambda: len(news_of(session)) >= count, label, 30)
        time.sleep(5)
        assert len(news_of(session)) == count, (label, news_of(session))
        return news_of(session)

    exited, gone_pane = agent_session('gone')
    agent_reports('gone', gone_pane, 'claude', 'working')
    time.sleep(5)
    assert not news_of(exited), 'a working agent is no news'
    cli('session', 'send-text', 'gone', 'exit', '-p', 'demo')
    wait(lambda: pane_of(gone_pane)['exited'], 'the agent shell exits')
    assert posts_of(exited, 1, 'the exit post') == ['[claude in main](session/%d) exited' % exited]
    assert [e['text'] for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
            and e['text'].startswith('session ')][-1] == 'session claude in main'
    # ^ blocked is one post, a second report of it none; leaving and entering again is a second.
    stuck, stuck_pane = agent_session('stuck')
    agent_reports('stuck', stuck_pane, 'claude', 'blocked')
    assert posts_of(stuck, 1, 'the blocked post') == ['[claude in main](session/%d) needs input' % stuck]
    agent_reports('stuck', stuck_pane, 'claude', 'working')
    agent_reports('stuck', stuck_pane, 'claude', 'blocked')
    assert posts_of(stuck, 2, 'the second blocked post')[1:] == ['[claude in main](session/%d) needs input' % stuck]
    agent_reports('stuck', stuck_pane, 'claude', 'error')
    assert posts_of(stuck, 3, 'the error post')[2:] == ['[claude in main](session/%d) ended in an error' % stuck]
    # ^ A session of a task is no news, by its label or by its provider.
    ours = {}
    for name, provider in (('task-99', 'claude'), ('worker', 'task')):
        ours[name], pane = agent_session(name)
        agent_reports(name, pane, provider, 'blocked')
        agent_reports(name, pane, provider, 'error')
        cli('session', 'send-text', name, 'exit', '-p', 'demo')
        wait(lambda: pane_of(pane)['exited'], '%s exits' % name)
    time.sleep(6)
    assert not any(news_of(session) for session in ours.values()), manager_entries()[-4:]
    # ^ A turn that runs holds the post: it comes after the turn's reply, never inside it.
    turn = js('task', 'say', 'LONG STATUS held')['id']
    held, held_pane = agent_session('held')
    agent_reports('held', held_pane, 'claude', 'error')
    time.sleep(5)
    assert not news_of(held) and not [e for e in manager_entries() if e['kind'] == 'reply' and turn in e['to']], \
        'the turn still runs and the post waits'
    answer = wait(lambda: next((i for i, e in enumerate(manager_entries()) if e['kind'] == 'reply' and turn in e['to']), None),
                  'the long turn ends', 60)
    wait(lambda: news_of(held), 'the held post', 30)
    assert answer < next(i for i, e in enumerate(manager_entries()) if e['kind'] == 'reply'
                         and '(session/%d)' % held in e.get('text', '')), 'the post waits for the turn'
    for name in ('gone', 'stuck', 'task-99', 'worker', 'held'):
        cli('session', 'delete', name, '-p', 'demo')
    # ^ A reply that ends with a question asks the owner and is never quiet, also to a turn of
    # only events; a reply that ends with a table, or a report, does not ask (docs/tasks.md "Manager").
    asking = js('task', 'new', 'NOOP ASK')['id']
    wait_state(asking, 'ready')
    asked = reply_to(wait(lambda: events_with('#%d ready r1' % asking), 'the event of a task that asks')[0]['id'])
    assert asked['text'] == 'Should I land it now?' and asked.get('asks') is True and 'quiet' not in asked, asked
    assert 'quote' not in next(e for e in chat(raw=True) if e.get('id') == asked['id']), 'a turn of only events quotes no owner message'
    # ^ An open question shows in the next turn's input ("Open questions", docs/tasks.md "Questions"):
    # a message that quotes another message answers none. A dismiss, or an owner message that
    # quotes it, ends it; with none open the section is absent.
    line = '- %d: Should I land it now?' % asked['id']
    kept = js('task', 'say', '> message 1\n\nkeeping that open')
    reply_to(kept['id'])
    assert line in turn_with('keeping that open')['message'].split('Open questions', 1)[1], turn_with('keeping that open')
    js('task', 'dismiss', str(asked['id']))
    reply_to(js('task', 'say', 'STATUS nothing open')['id'])
    assert 'Open questions' not in turn_with('STATUS nothing open')['message'], turn_with('STATUS nothing open')
    once_more = js('task', 'say', 'ASK once more')
    again = reply_to(once_more['id'])
    assert again.get('asks') is True, again
    assert again['id'] in js('task', 'chat')['asks'], js('task', 'chat')['asks']
    reply_to(js('task', 'say', '> message %d\n\nyes, land it' % again['id'])['id'])
    answered_turn = turn_with('yes, land it')['message']
    assert 'Open questions' not in answered_turn and '- %d:' % again['id'] not in answered_turn, answered_turn
    js('task', 'close', asking)
    question_table = reply_to(js('task', 'say', 'ASK TABLE')['id'])
    assert question_table['text'].startswith('Should I land it now?\nWhere things stand') and 'asks' not in question_table, question_table
    assert 'asks' not in stored_reply and 'asks' not in event_reply, (stored_reply, event_reply)
    # ^ A question in a last table row with no closing pipe does not ask either.
    question_row = reply_to(js('task', 'say', 'ASKROW')['id'])
    assert question_row['text'].endswith('| #2 | land it?') and 'asks' not in question_row, question_row
    put('context.md')
    # ^ The config named no manager model since the restart: the manager defaults to opus.
    assert fresh['model'] == 'opus', fresh
    # ^ Claude's own turn for a notification is no reply (docs/tasks.md "Claude adapter").
    # ^ The continuity part above restarted the manager since `fresh`: compare with its newest launch.
    running = [t for t in traces() if t.get('launch') and t.get('manager')][-1]
    resumed = js('task', 'say', 'NOTIFY STATUS again')
    assert reply_to(resumed['id'])['text'], chat()[-3:]
    assert not any(e['kind'] == 'reply' and not e['text'] for e in chat())
    assert not any(t.get('launch') and t.get('manager') and t['session'] != running['session']
                   for t in traces()[traces().index(running) + 1:]), 'same launch keeps its conversation'
    assert not turn_with('STATUS again')['message'].startswith('Handover')
    # ^ A usage limit is no reply: the message waits and is answered after the reset.
    before_limit = int(time.time() * 1000)
    limited = js('task', 'say', 'LIMIT 1 STATUS limited')
    held = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'limit' and e.get('until') and e['unix_ms'] >= before_limit),
                             None), 'the manager limit wait')
    assert re.fullmatch(r'\d\d:\d\d \(no stand-in answers; .*\)', held['until']), held
    answer = reply_to(limited['id'])
    assert answer['text'].startswith('open: ') and answer['unix_ms'] >= held['unix_ms'] + 1000, (held, answer)
    assert not any(e['kind'] == 'reply' and 'session limit' in e['text'] for e in chat())
    # ^ wsx says the pause and the resume in the chat, in that order, before the answer.
    shown = [(e['kind'], bool(e.get('until'))) for e in chat() if e['kind'] in ('limit', 'reply') and e['unix_ms'] >= held['unix_ms']]
    assert shown[:3] == [('limit', True), ('limit', False), ('reply', False)], shown
    assert turn_with('The usage limit has reset'), 'the manager continues after its limit'
    # ^ A turn that a limit held answers "-" after the reset, with or without an owner message:
    # a reply that says nothing is quiet whatever its turn (docs/tasks.md).
    for seconds, owner_says in ((1, None), (3, 'STATUS while held')):
        held_count = len([e for e in manager_entries() if e['kind'] == 'limit' and e.get('until')])
        title = js('task', 'new', 'NOOP MLIMIT %d DASH' % seconds)['id']
        event = wait(lambda: events_with('#%d ready r1' % title), 'the event the limit holds')[0]
        if owner_says:
            wait(lambda: len([e for e in manager_entries() if e['kind'] == 'limit' and e.get('until')]) > held_count, 'the second limit wait')
            owner_id = js('task', 'say', owner_says)['id']
        dash = reply_to(event['id'])
        assert dash['text'] == '-' and dash.get('quiet') is True, dash
        assert owner_says is None or owner_id in dash['to'], dash
        js('task', 'close', title, '--discard')
    # ^ The reply is every text block of the turn, in order, joined with a blank line: Claude's
    # `result` holds only the last (docs/tasks.md "Manager"). A `-` block next to words is dropped,
    # and a turn whose only block is `-` is quiet.
    spoken = js('task', 'say', 'BLOCKS First, the answer.|TOOL|Then, the closing note.')
    spoken_reply = reply_to(spoken['id'])
    assert spoken_reply['text'] == 'First, the answer.\n\nThen, the closing note.', spoken_reply
    mixed = js('task', 'say', 'BLOCKS -|TOOL|Only this stays.')
    assert reply_to(mixed['id'])['text'] == 'Only this stays.', chat()[-2:]
    silent = js('task', 'say', 'BLOCKS -|TOOL|-')
    silent_reply = reply_to(silent['id'])
    assert silent_reply['text'].strip() in ('', '-'), silent_reply
    # ^ A turn whose thinking holds far more than its words gets one follow-up that asks for the
    # answer as reply text (docs/tasks.md "Reply text"); a `-` answer to it stays quiet and starts
    # no third turn; short thinking asks nothing.
    def thinking_asks(after):
        return [e for e in chat() if e['kind'] == 'message' and e['from'] == 'recap'
                and e['id'] > after and 'only in your thinking' in e['text']]
    draft = 'The rewritten Q1 draft, written as thinking only: ' + 'a full answer in prose. ' * 6
    thought = js('task', 'say', 'BLOCKS THINK %s|TOOL|Saved as a rule.' % draft)
    assert reply_to(thought['id'])['text'] == 'Saved as a rule.'
    ask = wait(lambda: thinking_asks(thought['id']), 'the follow-up for the answer in thinking')[0]
    assert 'or reply - if it held nothing for the owner' in ask['text'], ask
    assert reply_to(ask['id'])['text'] == draft, chat()[-3:]
    nothing = js('task', 'say', 'BLOCKS THINK nothing for the owner, only notes about events and tasks that I read: %s|TOOL|-' % ('x ' * 40))
    nothing_ask = wait(lambda: thinking_asks(nothing['id']), 'the follow-up after a quiet turn')[0]
    dash = reply_to(nothing_ask['id'])
    assert dash['text'].strip() in ('', '-') and dash.get('quiet') is True, dash
    short = js('task', 'say', 'BLOCKS THINK a short thought|TOOL|Hi there.')
    assert reply_to(short['id'])['text'] == 'Hi there.'
    time.sleep(4)
    assert len(thinking_asks(thought['id'])) == 2 and not thinking_asks(short['id']), thinking_asks(0)
    for n in (first, second, unseen):
        js('task', 'close', n, '--discard')
    # 21b. Memo (docs/tasks.md "Memo"): each task that leaves brings its worker's memo to the
    mark('21c')
    # manager in its event, also past the ledger's 10 recent tasks.
    memo_tasks = [js('task', 'new', 'WORK m%d.txt MEMO lesson %d kept' % (i, i))['id'] for i in range(11)]
    for n in memo_tasks:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
        js('task', 'close', n, '--discard')
    for i, n in enumerate(memo_tasks):
        wait(lambda: events_with('#%d closed r1' % n, 'memo: lesson %d kept' % i), 'memo of t%d in its event' % n)
    assert len(js('task', 'ledger')['recent']) == 10
    # ^ wsx writes the memos into the manager's notes, once each, and no turn runs for a close: the
    # events go along with the next turn (docs/tasks.md "Memo", "Manager").
    context = (notes / 'context.md').read_text()
    for i, n in enumerate(memo_tasks):
        assert context.count('lesson %d kept' % i) == 1 and '- #%d: ' % n in context, context
    # ^ One close alone, after the turns above have ended, is the case that costs no turn. A stop
    # by request costs none either: its wait on owner is a chat line that joins the next turn,
    # while a ready event still starts one (docs/tasks.md "Manager", `state_wakes`).
    lone = js('task', 'new', 'WORK lone.txt')['id']
    halted = js('task', 'new', 'WAIT %s NOOP' % (base / 'never'))['id']
    assert wait_state(lone, 'ready', 'failed')['state'] == 'ready'
    reply_to(wait(lambda: events_with('#%d ready r1' % lone), 'the ready event')[0]['id'])
    wait(lambda: show(halted)['state'] == 'working', 'the halted worker')
    time.sleep(5)
    assert js('task', 'stop', halted)['ok']
    stop_event = wait(lambda: events_with('#%d waiting r' % halted, 'waits on owner: Stopped', 'stopped by request'),
                      'the stop event')[0]
    # ^ A stop someone asked for starts no manager turn: the line says `wakes: False`.
    assert stop_event['short'] == [{'text': '#%d waiting' % halted, 'task': halted, 'wakes': False}], stop_event
    js('task', 'close', lone, '--discard')
    last_close = wait(lambda: events_with('#%d closed r1' % lone), 'the lone close event')[0]
    time.sleep(5)
    for quiet in (stop_event, last_close):
        assert not any(e['kind'] == 'reply' and quiet['id'] in e['to'] for e in chat()), chat()[-3:]
    along = js('task', 'say', 'STATUS after the closes')
    assert {last_close['id'], stop_event['id']} <= set(reply_to(along['id'])['to']), chat()[-3:]
    along_turn = turn_with('STATUS after the closes')['message']
    # ^ The owner's message comes first and the turn says so (docs/tasks.md "Manager").
    assert 'closed r1' in along_turn and 'Answer message %d first.' % along['id'] in along_turn, along_turn
    assert 'stopped by request' in along_turn, along_turn
    js('task', 'close', halted, '--discard')

    return created


def part_worker(created=None):
    """Sections 22 to 32u: the push, the boundary, the worker's checks and the limit waits."""
    if created is None:
        created = fresh_project()
        store_up(created)
        manager_up(created)
        # ^ Section 22b reads the default model's heading; core's section 21 left that config.
        default_manager_model()

    # 22. Approvals: what the manager may not do alone becomes a request the owner decides.
    mark('22')
    # ^ By default the manager lands alone: only what goes public or is lost asks.
    free = js('task', 'new', 'WORK free.txt MEMO landing brings it')['id']
    assert wait_state(free, 'ready', 'failed')['state'] == 'ready'
    landed_alone = js('task', 'say', 'RUN wsx task land %d -p demo --json' % free)
    assert 'owner was asked' not in reply_to(landed_alone['id'])['text'] and main_has('free.txt')
    wait_closed(free)
    wait(lambda: events_with('#%d landed' % free, 'memo: landing brings it'), 'the land event carries the memo')
    # Capture (docs/memory.md "Capture"): the landed memo is a fact verified by the task, with the landed
    # commit and the files of its diff, in the same host step; no turn of the model.
    landed_tip = git('rev-parse', 'main')
    fact = wait(lambda: next((h for h in memory_records(cli('memory', 'search', 'landing brings it', '--json').stdout) if h['kind'] == 'fact'), None),
                'the memo of the landed task as a fact')
    record = json.loads(cli('memory', 'get', fact['slug'], '--json').stdout)
    assert record['verified_by'] == 'task:%d' % free and record['source'] == 'task:%d' % free, record
    assert record['paths'] == ['free.txt'] and record['commit'] == landed_tip and not fact.get('stale'), (record, landed_tip)
    # A worker on that file starts without the fact (no fact record joins a prompt: docs/memory.md "Start set");
    # once main changes the file, search marks the fact stale.
    def first_prompt(files, *title):
        n = js('task', 'new', '--size', 'small', '--files', files, *title, 'NOOP')['id']
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
        text = next(e for e in log(n) if e['kind'] == 'result')['summary']
        js('task', 'close', n, '--discard')
        return text
    assert fact['slug'] not in first_prompt('free.txt', '--title', 'landing brings it')
    (project / 'free.txt').write_text('moved on\n')
    git('commit', '-q', '-am', 'free.txt moves')
    stale = next(h for h in memory_records(cli('memory', 'search', 'landing brings it', '--json').stdout) if h['slug'] == fact['slug'])
    assert stale.get('stale') is True, stale
    after = first_prompt('free.txt', '--title', 'landing brings it')
    assert fact['slug'] not in after and 'stale' not in after and 'Records about' not in after, after
    # Backfill (docs/memory.md "Capture"): a memo imported before paths existed has no commit or paths; the
    # import gives it those of the landed task, once, and a rerun changes nothing.
    store = Path(json.loads(cli('memory', 'status', '--json').stdout)['store'])
    rows = [json.loads(line) for line in (store / 'records.jsonl').read_text().splitlines()]
    for row in rows:
        if row['slug'] == fact['slug']:
            row.pop('commit', None)
            row.pop('paths', None)
    (store / 'records.jsonl').write_text(''.join(json.dumps(row) + '\n' for row in rows))
    (store / 'paths-backfilled').unlink(missing_ok=True)
    assert 'paths' not in json.loads(cli('memory', 'get', fact['slug'], '--json').stdout)
    cli('memory', 'import')
    record = json.loads(cli('memory', 'get', fact['slug'], '--json').stdout)
    assert record['paths'] == ['free.txt'] and record['commit'] == landed_tip, record
    size = (store / 'records.jsonl').stat().st_size
    cli('memory', 'import')
    assert (store / 'records.jsonl').stat().st_size == size, 'the rerun changed the store'
    configure((project / 'wsx.config.yml').read_text().replace(
        '  land: { auto: false }\n', '  land: { auto: false, approve: true }\n'), 'land asks')
    ap = js('task', 'new', 'WORK ap.txt')['id']
    assert wait_state(ap, 'ready', 'failed')['state'] == 'ready'
    asked = js('task', 'say', 'RUN wsx task land %d -p demo --json' % ap)
    assert 'the owner was asked (a1)' in reply_to(asked['id'])['text']
    assert show(ap)['state'] == 'ready'
    pending = js('task', 'ledger')['approvals']
    assert [(p['id'], p['action'], p['task'], p['revision']) for p in pending] == [('a1', 'land', ap, 1)], pending
    approved = js('task', 'approve', 'a1')
    assert approved['ok'] and approved['result']['state'] == 'landed', approved
    assert show(ap)['state'] == 'landed'
    heard = wait(lambda: events_with('approval a1 (land #%d) approved; done' % ap), 'approval event')
    assert 'noted events' in reply_to(heard[0]['id'])['text']
    # close --discard by the manager always asks; the owner denies it with a note.
    keep = js('task', 'new', 'WORK keep.txt')['id']
    assert wait_state(keep, 'ready', 'failed')['state'] == 'ready'
    asked = js('task', 'say', 'RUN wsx task close %d --discard -p demo --json' % keep)
    assert 'the owner was asked (a2)' in reply_to(asked['id'])['text']
    # The owner sees the newest request of a task only: a second discard replaces the first, which
    # then answers that it was replaced and runs nothing (docs/tasks.md "Approvals").
    asked = js('task', 'say', 'RUN wsx task close %d --discard -p demo --json' % keep)
    assert 'the owner was asked (a3)' in reply_to(asked['id'])['text']
    assert [p['id'] for p in js('task', 'ledger')['approvals']] == ['a3']
    assert 'approval a2 was decided: dropped; replaced by a3; nothing ran' in cli('task', 'approve', 'a2', ok=False).stderr
    wait(lambda: events_with('approval a2 (discard #%d) dropped; replaced by a3' % keep), 'replace event')
    js('task', 'deny', 'a3', '--note', 'keep it for now')
    wait(lambda: events_with('approval a3 (discard #%d) denied; keep it for now' % keep), 'deny event')
    assert show(keep)['state'] == 'ready' and Path(show(keep)['record']['working_copy']).exists()
    # The manager cannot decide its own request.
    asked = js('task', 'say', 'RUN wsx task land %d -p demo --json\nRUN wsx task approve a4 -p demo --json' % keep)
    text = reply_to(asked['id'])['text']
    assert 'the owner was asked (a4)' in text and 'is for the owner' in text, text
    # An approval for a revision the task has moved past is dropped, and the manager hears why
    # in the chat (docs/tasks.md "Approvals").
    js('task', 'add', keep, 'WORK keep2.txt')
    assert wait_state(keep, 'ready', 'failed')['revision'] == 2
    wait(lambda: events_with('approval a4 (land #%d) dropped; #%d moved from r1 to r2' % (keep, keep)), 'the drop event')
    assert 'approval a4 was decided: dropped; #%d moved from r1 to r2' % keep in cli('task', 'approve', 'a4', ok=False).stderr and show(keep)['state'] == 'ready'
    assert js('task', 'ledger')['approvals'] == []
    wait_closed(ap)
    js('task', 'close', keep, '--discard')

    # 22a. Push (docs/tasks.md "Push"): the manager's push asks; a denied one leaves origin, an
    # approved one runs in the host and moves it. A rewrite (a squash) leases the origin tip it
    # saw and names the open tasks on the old main. A worker's push is refused (part 27's
    # CHECKRUN trace).
    mark('22a')

    origin = work / 'origin.git'
    git('init', '-q', '--bare', '--template=', str(origin))
    git('remote', 'add', 'origin', str(origin))
    tip = git('rev-parse', 'main')
    git('push', '-q', 'origin', 'main~1:refs/heads/main')
    before = git('rev-parse', 'main~1')

    def origin_tip():
        return git('--git-dir', str(origin), 'rev-parse', 'main')

    def ask_push(argv):
        text = reply_to(js('task', 'say', 'RUN wsx task push %s-p demo --json' % argv)['id'])['text']
        return re.search(r'push needs the owner.s approval; the owner was asked \((a\d+)\)', text).group(1), text

    denied, _ = ask_push('')
    assert [(p['id'], p['action'], p['commit']) for p in js('task', 'ledger')['approvals']] == [(denied, 'push', tip)]
    # ^ The host posts the ask line itself (docs/tasks.md "Approvals"): the reply needs no words for it.
    # It records the request's id (`approval`), so the web pushes the request alone.
    asked = wait(lambda: events_with('asked the owner: Approve push', '(%s)' % denied), 'the ask line')
    assert any(e['kind'] == 'reply' and asked[0]['id'] in e['to'] and e['text'].startswith('Asked you: Approve push')
               and e['text'].endswith('(%s).' % denied)
               and e.get('approval') == denied for e in chat()), asked
    js('task', 'deny', denied)
    wait(lambda: events_with('approval %s (push) denied' % denied), 'the push deny event')
    assert origin_tip() == before
    # ^ The request says what it brings: the task landed since origin's tip (`ap`) and the one
    # commit; a newer request for the same commit replaces it (docs/tasks.md "Push").
    stale, _ = ask_push('')
    approved, _ = ask_push('')
    waiting = js('task', 'ledger')['approvals']
    assert [p['id'] for p in waiting] == [approved], waiting
    assert (waiting[0]['tasks'], waiting[0]['commits']) == ([ap], 1), waiting
    assert waiting[0]['label'] == 'Approve push #%d (1 commit)' % ap, waiting
    assert 'replaced by %s; nothing ran' % approved in cli('task', 'approve', stale, ok=False).stderr
    wait(lambda: events_with('approval %s (push) dropped; replaced by %s' % (stale, approved)), 'the push replace event')
    assert origin_tip() == before
    done = js('task', 'approve', approved)
    assert done['ok'] and done['result']['pushed'] and done['result']['forced'] is False, done
    assert origin_tip() == tip
    wait(lambda: events_with('approval %s (push) approved; done: pushed main to origin: %s' % (approved, tip[:12])),
         'the push approve event')
    # ^ A rewrite with no --force refuses before it asks; nothing waits on the owner.
    on_old = js('task', 'new', 'WORK on-old.txt')['id']
    assert wait_state(on_old, 'ready', 'failed')['state'] == 'ready'
    squash = git('commit-tree', 'main^{tree}', '-m', 'squash')
    text = reply_to(js('task', 'say', 'RUN wsx task push --commit %s -p demo --json' % squash)['id'])['text']
    assert 'a rewrite needs --force' in text and 'owner was asked' not in text, text
    forced, _ = ask_push('--commit %s --force ' % squash)
    pending = js('task', 'ledger')['approvals']
    assert [(p['id'], p['lease']) for p in pending] == [(forced, tip)], pending
    done = js('task', 'approve', forced)
    message = done['result']['message']
    assert done['ok'] and 'forced over %s' % tip[:12] in message, done
    assert 'open tasks on the old main: #%d' % on_old in message, message
    assert origin_tip() == squash and git('rev-parse', 'main') == squash
    # ^ The owner pushes the old main back by hand: no request, the same rewrite.
    back = js('task', 'push', '--commit', tip, '--force')
    assert back['pushed'] and origin_tip() == tip and git('rev-parse', 'main') == tip, back
    assert js('task', 'ledger')['approvals'] == []
    js('task', 'close', on_old, '--discard')
    git('remote', 'remove', 'origin')
    # ^ An ssh origin. The fixture's wsxd, and so its manager host, has no SSH_AUTH_SOCK, as a
    # wsxd first started from an ssh login has: ssh reaches the login session's agent. ssh reads
    # the passwd home's ~/.ssh and that agent is the runner's own, so this runs only on CI.
    if os.environ.get('GITHUB_ACTIONS') == 'true':
        assert 'SSH_AUTH_SOCK' not in env
        keys = work / 'ssh'
        keys.mkdir(mode=0o700)
        for name in ('host', 'user'):
            subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-C', name, '-f', str(keys / name)],
                           check=True)
        authorized = keys / 'authorized_keys'
        authorized.write_text((keys / 'host.pub').read_text())
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            port = probe.getsockname()[1]
        (keys / 'sshd_config').write_text(
            'Port %d\nListenAddress 127.0.0.1\nHostKey %s\nAuthorizedKeysFile %s\nPidFile %s\nUsePAM no\n'
            'StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n'
            % (port, keys / 'host', authorized, keys / 'sshd.pid'))
        sshd_log = (keys / 'sshd.log').open('w')
        sshd = subprocess.Popen(['/usr/sbin/sshd', '-D', '-e', '-f', str(keys / 'sshd_config')],
                                stdout=sshd_log, stderr=subprocess.STDOUT)
        atexit.register(sshd.kill)

        def listening():
            try:
                socket.create_connection(('127.0.0.1', port), timeout=1).close()
                return True
            except OSError:
                return False
        wait(listening, 'sshd on port %d' % port)
        user = pwd.getpwuid(os.getuid())
        known = Path(user.pw_dir) / '.ssh/known_hosts'
        known.parent.mkdir(mode=0o700, exist_ok=True)
        with known.open('a') as lines:
            lines.write('[127.0.0.1]:%d %s' % (port, (keys / 'host.pub').read_text()))
        printed = subprocess.run(['launchctl', 'print', 'gui/%d/com.openssh.ssh-agent' % os.getuid()],
                                 capture_output=True, text=True).stdout
        login = re.search(r'SSH_AUTH_SOCK => (\S+)', printed)
        assert login, 'the runner has no login-session ssh agent: %s' % printed[-500:]
        agent_env = dict(os.environ, SSH_AUTH_SOCK=login.group(1))
        ssh_origin = work / 'origin-ssh.git'
        git('init', '-q', '--bare', '--template=', str(ssh_origin))
        git('push', '-q', str(ssh_origin), 'main~1:refs/heads/main')
        git('remote', 'add', 'origin', 'ssh://%s@127.0.0.1:%d%s' % (user.pw_name, port, ssh_origin))
        try:
            # ^ The server takes no key of that agent: nothing is asked, the answer names the agent.
            text = reply_to(js('task', 'say', 'RUN wsx task push -p demo --json')['id'])['text']
            assert 'Permission denied (publickey)' in text, (text, (keys / 'sshd.log').read_text()[-2000:])
            assert "the login session's ssh agent at %s and the server took none" % login.group(1) in text, text
            assert 'owner was asked' not in text and js('task', 'ledger')['approvals'] == [], text
            # ^ With the key in that agent, ls-remote gets past auth and the approved push moves origin.
            subprocess.run(['ssh-add', '-q', '-t', '600', str(keys / 'user')], env=agent_env, check=True)
            authorized.write_text((keys / 'user.pub').read_text())
            approved, _ = ask_push('')
            done = js('task', 'approve', approved)
            assert done['ok'] and done['result']['pushed'], done
            assert git('--git-dir', str(ssh_origin), 'rev-parse', 'main') == git('rev-parse', 'main')
        finally:
            subprocess.run(['ssh-add', '-d', str(keys / 'user')], env=agent_env, capture_output=True)
            sshd.kill()
            sshd.wait()
            sshd_log.close()
        git('remote', 'remove', 'origin')

    # 22a2. Project add (docs/tasks.md "Approvals"): the manager's request names the path and the URL;
    # the owner's yes clones a bare repository (file:// is allowed by the scenario's env only) and
    # registers it as the web's Add project does. Each refusal comes before anyone is asked.
    mark('22a2')
    seed, bare, grown = work / 'grown-seed', work / 'grown-origin.git', home / 'grown'
    git('init', '-q', '--bare', '--template=', str(bare))
    seed.mkdir()
    git('init', '-q', '-b', 'main', cwd=seed)
    (seed / 'grown.txt').write_text('grown\n')
    git('add', 'grown.txt', cwd=seed)
    git('commit', '-q', '-m', 'grown', cwd=seed)
    git('push', '-q', str(bare), 'main', cwd=seed)
    clone_url = 'file://' + str(bare)

    def ask_add(*argv):
        return reply_to(js('task', 'say', 'RUN wsx task project add %s -p demo --json' % ' '.join(map(str, argv)))['id'])['text']

    (home / 'plain').mkdir()
    for argv, why in (((work / 'outside', '--clone', clone_url), 'is outside'),
                      (('../up', '--clone', clone_url), 'is not an absolute path'),
                      ((home / 'plain',), 'exists and is not a git repository'),
                      ((home / 'nothing',), 'does not exist; give --clone'),
                      ((home / 'ftp', '--clone', 'ftp://example.invalid/r.git'), 'is not ssh or https'),
                      ((home / 'http', '--clone', 'http://example.invalid/r.git'), 'is not ssh or https'),
                      ((home / 'dash', '--clone=--upload-pack=x'), 'starts with')):
        text = ask_add(*argv)
        assert why in text and 'owner was asked' not in text, (argv, text)
    assert js('task', 'ledger')['approvals'] == [] and not grown.exists()
    # ^ A second request for the path replaces the first; the owner sees one card.
    stale = re.search(r'\((a\d+)\)', ask_add(grown, '--clone', clone_url)).group(1)
    approved = re.search(r'\((a\d+)\)', ask_add(grown, '--clone', clone_url)).group(1)
    waiting = js('task', 'ledger')['approvals']
    assert [(p['id'], p['action'], p['path'], p['url'], p['label']) for p in waiting] == \
        [(approved, 'project', str(grown), clone_url, 'Approve project add %s' % grown)], waiting
    wait(lambda: events_with('approval %s (project %s) dropped; replaced by %s' % (stale, grown, approved)), 'the project replace event')
    assert 'replaced by %s; nothing ran' % approved in cli('task', 'approve', stale, ok=False).stderr
    assert not grown.exists(), 'nothing is cloned before the owner says yes'
    done = js('task', 'approve', approved)
    assert done['ok'] and done['result']['project'] == 'grown' and done['result']['path'] == str(grown), done
    assert (grown / 'grown.txt').read_text() == 'grown\n' and (grown / '.git').exists()
    wait(lambda: events_with('approval %s (project %s) approved; done: project grown added at %s' % (approved, grown, grown)),
         'the project added event')
    # ^ The new project works: its ledger answers and the manager of `grown` takes a message.
    assert js('task', 'ledger', '-p', 'grown')['project'] == 'grown'
    assert js('task', 'say', '-p', 'grown', 'STATUS')['id']
    # ^ Registered paths and names refuse, naming the project; the owner's own run needs no card.
    text = ask_add(grown)
    assert 'project grown is already registered' in text and 'owner was asked' not in text, text
    owned = js('task', 'project', 'add', home / 'owned', '--clone', clone_url)
    assert owned['project'] == 'owned' and (home / 'owned' / 'grown.txt').exists() and js('task', 'ledger')['approvals'] == [], owned
    # ^ Without the scenario's env the file URL is refused: only ssh and https clone.
    nofile = {key: value for key, value in env.items() if key != 'WSX_TEST_ALLOW_FILE_CLONE'}
    refused = subprocess.run([str(wsx), 'task', 'project', 'add', str(home / 'nofile'), '--clone', clone_url],
                             cwd=project, env=nofile, capture_output=True, text=True, timeout=60)
    assert refused.returncode != 0 and 'is not ssh or https' in refused.stderr and not (home / 'nofile').exists(), refused
    # ^ The sections below run with one project: unregister `grown` and `owned` in the config.
    registry = (home / 'Library' / 'Application Support' if sys.platform == 'darwin' else work / 'c') / 'wsx' / 'config-v2.toml'
    # ^ `dirs` takes macOS's config folder from HOME, not from XDG_CONFIG_HOME.
    blocks = registry.read_text().split('[[projects]]')
    assert sum('name = "grown"' in b or 'name = "owned"' in b for b in blocks) == 2, blocks
    registry.write_text('[[projects]]'.join(b for b in blocks if 'name = "grown"' not in b and 'name = "owned"' not in b))
    js('task', 'ledger')

    # 22b. The TUI's manager chat (`t`, then `m`; docs/tasks.md "TUI"): the conversation, the
    # open To check item and the waiting approval; a message goes to the manager's log, `r` opens
    # a reply to the item, `p` passes it and `n` then `y` denies the request; 80 columns still show
    # the message.
    mark('22b')
    seen_item = js('task', 'new', 'WORK tui.txt HV')['id']
    assert wait_state(seen_item, 'ready', 'failed')['state'] == 'ready'
    assert js('task', 'land', seen_item)['state'] == 'landed'
    item = next(i for i in js('task', 'check')['open'] if i['task'] == seen_item)
    asks = js('task', 'new', 'WORK asks.txt')['id']
    assert wait_state(asks, 'ready', 'failed')['state'] == 'ready'
    asked = js('task', 'say', 'RUN wsx task land %d -p demo --json' % asks)
    request = re.search(r'the owner was asked \((a\d+)\)', reply_to(asked['id'])['text']).group(1)
    # ^ Nothing else waits: the chat's selection starts at this item and Tab reaches the request.
    assert [i['id'] for i in js('task', 'check')['open']] == [item['id']]
    assert [p['id'] for p in js('task', 'ledger')['approvals']] == [request]
    checks_file = Path(show(seen_item)['report']).parent.parent / 'checks.jsonl'

    # The Workspace row marks what waits for the owner after the project's name: `!N` tasks and
    # approvals (the ready task and its request, so 2 or more), `✓N` To check items (one), and `N`
    # from the worktree row reaches the project row (docs/tasks.md "TUI"). Not `n`: this
    # project has agent sessions below its worktrees, which `n` would reach first.
    tui_screens(wsx, env, project, [
        (r'(?s)[▼▶] demo !(?:[2-9]|\d\d) ✓%d\b.*~ main' % len(js('task', 'check')['open']), b'j'),
        (r'WORKSPACE +\(s\)ession', b'N'),
        (r'WORKSPACE +\(t\)asks +\(e\)dit +\(w\)orktree', b'')])

    def owner_said(text):
        lines = [json.loads(line) for line in manager_log.read_text().splitlines() if line.strip()]
        return any(e.get('kind') == 'message' and e.get('from') == 'owner' and e.get('text') == text
                   for e in lines)

    shots = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'\(m\)anager', b'm'),
        # ^ The heading names the manager as the chat rows do: its shown name, `Atlas` here.
        (r'(?s)%s demo · opus high +1 to check  1 approval\n.*▸ %s' % (js('task', 'manager')['name'], item['id']), b'i'),
        (r'Enter: send', b'hello from the tui'),
        (r'> hello from the tui', b'\r'),
        # ^ "Sent" comes before the next read draws the message: wait for both.
        (r'(?ms)^ Sent$.*^   hello from the tui$', b'\x1b'),
        # ^ Pass, Fail and Reply on the item; `r` opens the message line as its reply, Esc drops it.
        (r'\(p\)ass +\(f\)ail +\(r\)eply', b'r'),
        (r'Reply to “', b'\x1b'),
        (r'\(p\)ass', b'p'),
        (r'%s passed' % item['id'], b'\t'),
        (r'\(n\)deny', b'n'),
        (r'Deny land #%d\? y/n' % asks, b'y'),
        (r'%s denied' % request, b'\x1b'),
        (r'\(m\)anager', b'\x1b'),
        (r'WORKSPACE', b'')])
    opened, sent = shots[2], shots[5]
    assert re.search(r'(?m)^ +▸ %s +#%d +open the file' % (item['id'], seen_item), opened), opened
    assert re.search(r'pass if: it has text', opened) and re.search(r'%s +Approve land #%d' % (request, asks), opened), opened
    assert re.search(r'(?m)^ You +\d\d:\d\d$', sent) and 'hello from the tui' in sent, sent
    # ^ The chat names the manager as the web does: its shown name (docs/tasks.md "Config").
    assert re.search(r'(?m)^ %s +\d\d:\d\d$' % re.escape(js('task', 'manager')['name']), opened) and 'MANAGER' in opened, opened
    assert owner_said('hello from the tui'), manager_log.read_text()[-2000:]
    assert item['id'] not in [i['id'] for i in js('task', 'check')['open']]
    rows = [json.loads(line) for line in checks_file.read_text().splitlines() if line.strip()]
    assert [r['state'] for r in rows if r['id'] == item['id']][-1] == 'passed', rows
    assert js('task', 'ledger')['approvals'] == [] and show(asks)['state'] == 'ready'
    narrow = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'), (r'(?m)^ You +\d\d:\d\d$', b'\x1b'),
        (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')], cols=80)[2]
    assert re.search(r'(?m)^ You +\d\d:\d\d$', narrow) and 'hello from the tui' in narrow, narrow
    # `/` finds in the chat (docs/tasks.md "TUI"): "quasar" is only in the 1st and 4th of 210 older
    # turns, past the newest 200 entries, so the find reads the whole log. It starts at the newer
    # match; `n` goes to the older one, `N` back, `Esc` clears. The older log goes after.
    old_log = manager_log.with_name('log.1.jsonl')
    assert not old_log.exists()
    long_ago = int(time.time() * 1000) - 86_400_000
    old_log.write_text(''.join(json.dumps(entry) + '\n' for n in range(210) for entry in (
        {'kind': 'message', 'id': 90001 + 2 * n, 'from': 'owner', 'unix_ms': long_ago + n * 20_000,
         'text': 'Where did the Quasar notes go?' if n == 0 else 'Older question %d.' % n},
        {'kind': 'delivered', 'ids': [90001 + 2 * n], 'unix_ms': long_ago + n * 20_000 + 1},
        {'kind': 'reply', 'id': 90002 + 2 * n, 'to': [90001 + 2 * n], 'unix_ms': long_ago + n * 20_000 + 2000,
         'text': 'The quasar notes are in docs.' if n == 3 else 'Older answer %d.' % n})))
    found = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'), (r'\(/\)find', b'/'),
        (r'Enter: keep', b'QUASAR'),
        (r'(?m)^ /QUASAR 2/2', b'\r'),
        (r'n: older', b'n'),
        (r'(?m)^ /QUASAR 1/2', b'N'),
        (r'(?m)^ /QUASAR 2/2', b'\x1b'),
        (r'\(i\)message', b'\x1b'), (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')])
    old_log.unlink()
    assert 'The quasar notes are in docs.' in found[4], found[4]
    assert 'Where did the Quasar notes go?' in found[6], found[6]
    assert '/QUASAR' not in found[8] and 'press i to write' in found[8], found[8]
    # ^ Esc leaves the whole-log find and reads the newest entries again: the owner's last message
    # shows, and the two entries only the older log holds do not.
    assert 'hello from the tui' in found[8] and 'quasar' not in found[8].lower(), found[8]
    # 22c. A reply to a To check item (docs/tasks.md "To check"): "x" or "ㅌ" skips an open item with
    # no manager turn and a quiet line; other words close an open item as replied, are its note and go
    # to the manager quoted with the item's check, marked `To check <id> replied:`. Pass and Fail
    # (`check pass`, `check fail` with no note) close the item and open nothing; words after them are
    # its note and reach the manager once, a failed item's marked `To check <id> failed:`. A closed
    # item says so to an "x".
    mark('22c')
    status_reply = reply_to(js('task', 'say', 'STATUS')['id'])
    replied = []
    for n in range(5):
        replied.append(js('task', 'new', 'WORK x%d.txt HV' % n)['id'])
        assert wait_state(replied[-1], 'ready', 'failed')['state'] == 'ready'
        landed = js('task', 'land', replied[-1])
        assert landed['state'] == 'landed', landed
    by_task = {i['task']: i for i in js('task', 'check')['open']}
    first, second, third, fourth, fifth = (by_task[n] for n in replied)
    reply_to(js('task', 'say', 'STATUS')['id'])  # ^ The landings' events are read by now.
    # ^ Only the turns that take an owner message: an event turn (a landed task's ready line) can come
    # at any time in this section and is not what it counts.
    manager_turns = lambda: len([t for t in traces() if t.get('manager') and 'Owner (message' in str(t.get('message', ''))])
    open_ids = lambda: [i['id'] for i in js('task', 'check')['open'] if i['task'] in replied]
    row_of = lambda item: {r['id']: r for r in (json.loads(line) for line in checks_file.read_text().splitlines() if line.strip())}[item['id']]
    # ^ The logged message names the item it answers (`check`, next to `quote`), the item names the
    # message that closed it (`owner_said`) or whose words are its note (`note_said`), and the search
    # finds the reply by the item's id.
    logged_message = lambda message_id: next(
        e for e in (json.loads(line) for line in manager_log.read_text().splitlines())
        if e.get('kind') == 'message' and e.get('id') == message_id)
    found_by_item = lambda item, message_id: wait(
        lambda: any(h['reference'].endswith(':%d' % message_id) for h in js('task', 'search', '-p', 'demo', item['id'])['hits']),
        'the search finds message %d by %s' % (message_id, item['id']), 30)
    turns = manager_turns()
    for item, words in ((first, ' X '), (second, 'ㅌ')):
        said = js('task', 'say', '> check %s\n\n%s' % (item['id'], words))
        assert logged_message(said['id'])['check'] == item['id'] and row_of(item)['owner_said'] == said['id'], (logged_message(said['id']), row_of(item))
        found_by_item(item, said['id'])
        answered = reply_to(said['id'])
        assert answered['post'] and answered['quiet'] and answered['text'] == 'Skipped: ' + item['check'], answered
        assert 'usage' not in answered, answered
        logged = next(e for e in chat() if e['kind'] == 'message' and e['id'] == said['id'])
        assert logged['text'] == '> %s\n\n%s' % (item['check'], words.strip()), logged
    assert [row_of(i)['state'] for i in (first, second)] == ['skipped', 'skipped'] and row_of(first).get('note') is None
    assert open_ids() == [third['id'], fourth['id'], fifth['id']]
    assert manager_turns() == turns, 'an x reply started a manager turn'
    again = js('task', 'say', '> check %s\n\nx' % first['id'])
    assert reply_to(again['id'])['text'] == 'Already skipped: ' + first['check'] and manager_turns() == turns
    # ^ Pass (the button) then a reply: the item is passed with no note until the words come; they
    # are its note and go to the manager once.
    js('task', 'check', 'pass', third['id'])
    assert row_of(third)['state'] == 'passed' and row_of(third).get('note') is None
    thanks = js('task', 'say', '> check %s\n\nlooks right, thanks' % third['id'])
    reply_to(thanks['id'])
    assert manager_turns() == turns + 1
    assert third['check'] in str(turn_with('looks right, thanks')), traces()[-1]
    assert logged_message(thanks['id'])['check'] == third['id'] and row_of(third)['note_said'] == thanks['id'], (logged_message(thanks['id']), row_of(third))
    found_by_item(third, thanks['id'])
    assert row_of(third)['state'] == 'passed' and row_of(third)['note'] == 'looks right, thanks', row_of(third)
    # ^ Fail (the button) closes the item with no fix task; its reply is the note, marked for the manager.
    js('task', 'check', 'fail', fourth['id'])
    assert row_of(fourth)['state'] == 'failed' and row_of(fourth).get('note') is None and row_of(fourth).get('fix_task') is None
    assert open_ids() == [fifth['id']]
    wrong = js('task', 'say', '> check %s\n\nthe text is cut off' % fourth['id'])
    reply_to(wrong['id'])
    assert manager_turns() == turns + 2
    assert 'To check %s failed: the text is cut off' % fourth['id'] in str(turn_with('the text is cut off')), traces()[-1]
    assert row_of(fourth)['state'] == 'failed' and row_of(fourth)['note'] == 'the text is cut off' and row_of(fourth).get('fix_task') is None
    assert logged_message(wrong['id'])['check'] == fourth['id'] and row_of(fourth)['note_said'] == wrong['id'], (logged_message(wrong['id']), row_of(fourth))
    found_by_item(fourth, wrong['id'])
    # ^ A second reply goes to the manager and leaves the first note.
    more = js('task', 'say', '> check %s\n\nalso on a phone' % fourth['id'])
    reply_to(more['id'])
    assert manager_turns() == turns + 3 and row_of(fourth)['note'] == 'the text is cut off'
    assert logged_message(more['id'])['check'] == fourth['id'] and row_of(fourth)['note_said'] == wrong['id']
    # ^ Words to an open item are the third verdict: it closes as replied with them as its note, and
    # the manager reads them marked as a fail's are. An x that quotes a message is the manager's.
    wordy = js('task', 'say', '> check %s\n\nx marks the spot' % fifth['id'])
    reply_to(wordy['id'])
    assert manager_turns() == turns + 4
    assert 'To check %s replied: x marks the spot' % fifth['id'] in str(turn_with('x marks the spot')), traces()[-1]
    assert fifth['check'] in str(turn_with('x marks the spot')), traces()[-1]
    assert row_of(fifth)['state'] == 'replied' and row_of(fifth)['note'] == 'x marks the spot', row_of(fifth)
    assert row_of(fifth)['note_said'] == wordy['id'] and 'owner_said' not in row_of(fifth), row_of(fifth)
    assert logged_message(wordy['id'])['check'] == fifth['id'] and open_ids() == []
    assert next(i for i in show(fifth['task'])['to_check'] if i['id'] == fifth['id'])['state'] == 'replied'
    assert reply_to(js('task', 'say', '> check %s\n\nx' % fifth['id'])['id'])['text'] == 'Already replied: ' + fifth['check']
    quoting = js('task', 'say', '> message %d\n\nx' % status_reply['id'])
    reply_to(quoting['id'])
    assert manager_turns() == turns + 5
    # ^ An item no one knows: the quote line goes and the words stay.
    stray = js('task', 'say', '> check c9999\n\nx')
    reply_to(stray['id'])
    assert 'check' not in logged_message(stray['id'])
    assert manager_turns() == turns + 6

    # A quote reaches a message of any age (docs/tasks.md "Log window"): the log rolls to
    # log.1, log.2 and so on, and every file stays. `agent` lines (not in the chat) fill the
    # current log just past the roll size; the host rolls it at its next write. A rotation entry
    # sits in the first roll and 250 chat entries (past the 200 a chat shows) in the second.
    now_ms = int(time.time() * 1000) - 5000

    def fill(entries=()):
        pad = json.dumps({'kind': 'agent', 'text': 'x' * 60000, 'unix_ms': now_ms}) + '\n'
        head = ''.join(json.dumps(e) + '\n' for e in entries)
        lines = (8 * 1024 * 1024 + 4096 - manager_log.stat().st_size - len(head)) // len(pad) + 1
        with manager_log.open('a') as out:
            out.write(head + pad * lines)
    fill([{'kind': 'rotated', 'tokens': 400000, 'unix_ms': now_ms}])
    reply_to(js('task', 'say', 'roll one')['id'])
    assert manager_log.with_name('log.1.jsonl').stat().st_size > 8 * 1024 * 1024 and not manager_log.with_name('log.2.jsonl').exists()
    fill([{'kind': 'limit', 'until': '07:45' if n % 2 else None, 'unix_ms': now_ms + n} for n in range(250)])
    reply_to(js('task', 'say', 'roll two')['id'])
    assert manager_log.with_name('log.1.jsonl').stat().st_size > 8 * 1024 * 1024 and manager_log.with_name('log.2.jsonl').exists()
    shown = js('task', 'chat')
    assert shown['skipped'] > 0 and all(e.get('id') != 1 for e in shown['entries']), 'message 1 is outside the chat window'
    around = js('task', 'chat', '--around', '1')
    assert around['skipped'] == 0 and len(around['entries']) == 200 and around['entries'][0]['id'] == 1, around['entries'][:2]
    old = reply_to(js('task', 'say', 'QUOTE 1 across the rolls')['id'])
    assert old['text'] == '> STATUS\n\nacross the rolls' and old['quote'] == 1, old
    # ^ In the TUI, Enter on that quote reads the part around message 1, marks it and says how to
    # go back; Esc reads the newest part again.
    rolled = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'),
        (r'(?m)^   \u258e STATUS$', b'['),
        (r'(?m)^ MANAGER +Enter: open', b'\r'),
        (r'(?m)^ MANAGER +Esc: newest', b''),
        (r'(?m)^ You +\d\d:\d\d\n   STATUS$', b'\x1b'),
        (r'across the rolls', b'\x1b'), (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')])
    assert 'across the rolls' not in rolled[5], rolled[5]
    for rolled_log in ('log.1.jsonl', 'log.2.jsonl'):
        manager_log.with_name(rolled_log).unlink()
    # The Workspace row of a project shows its name in the accent while the manager's last reply
    # asks the owner, and stops when the owner writes after it (docs/tasks.md "TUI"). One TUI run
    # sees both changes live: "ASK" makes the fake manager reply with a question.
    reply_to(js('task', 'say', 'ASK')['id'])
    accent, plain = (102, 153, 255), (224, 229, 237)
    seen_names = []

    def name_color(tui):
        """The foreground of `demo` in the project row, once the Workspace is drawn."""
        for line in tui.cells(colors=True):
            text = ''.join(c for c, _ in line)
            hit = re.search(r'[▼▶] demo', text)
            if hit:
                return line[hit.end() - 4][1]

    tui = TuiPty([str(wsx)], project, env)
    try:
        def row_colors(_text):
            color = name_color(tui)
            if color == accent and not seen_names:
                seen_names.append(color)
                reply_to(js('task', 'say', 'hello again')['id'])
            elif color == plain and seen_names:
                seen_names.append(color)
            return len(seen_names) == 2
        tui.wait_text(row_colors, lambda: 'the project row did not show ask, then answer: %s' % seen_names, 40, idle=0.1)
    finally:
        tui.close(QUIT)
    assert seen_names == [accent, plain], seen_names
    # `o` opens the project's overview.md read-only, from the list and from the chat: the text as
    # written, the mermaid graph as a code block; `j` and PageDown scroll, `Esc` returns to what
    # was under it (docs/tasks.md "TUI"). The file is longer than the window.
    Path(page).write_text('# demo\n\n## Goal\n\nShip the overview view.\n\n## Tasks\n\n```mermaid\n'
                          'graph LR\n  one[Write it] --> two[Show it]\n```\n\n## Open questions\n\n'
                          + ''.join('Open question %d.\n' % n for n in range(1, 61)))
    seen = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'(?m)^ TASKS +\(j/k\)move.*Esc: close +(?:\(\?\)help +)?(?:\(a\)gent +)?(?:\(u\)sage +)?\(o\)verview', b'o'),
        (r'(?m)^ OVERVIEW +\(j/k\)scroll +Esc: back', b'jjjjj'),
        (r'(?m)^ Overview demo +6-\d+/\d+$', b'\x1b[6~\x1b[6~\x1b[6~'),
        (r'Open question 60\.', b'\x1b'),
        (r'(?m)^ TASKS +\(j/k\)move', b'm'),
        (r'(?m)^ MANAGER +.*\(j/k\)scroll +\(o\)verview', b'o'),
        (r'(?m)^ OVERVIEW +\(j/k\)scroll', b'\x1b'),
        (r'(?m)^ MANAGER +.*\(j/k\)scroll +\(o\)verview', b'\x1b'),
        (r'(?m)^ TASKS +\(j/k\)move', b'\x1b'),
        (r'WORKSPACE', b'')])
    top, scrolled, bottom = seen[2], seen[3], seen[4]
    assert re.search(r'(?m)^ Overview demo +1-\d+/\d+$', top), top
    assert all(word in top for word in ('# demo', '## Goal', '```mermaid', 'graph LR', 'one[Write it] --> two[Show it]')), top
    assert 'Ship the overview view.' in top and 'Open question 1.' in top, top
    assert '# demo' not in scrolled and 'Ship the overview view.' not in scrolled and '## Tasks' in scrolled, scrolled
    assert '## Open questions' in bottom or 'Open question 60.' in bottom, bottom
    assert 'Overview' not in seen[8] and 'MANAGER' in seen[8], seen[8]
    Path(page).write_text('# demo\n\n## Goal\n\nshort\n')
    js('task', 'close', asks, '--discard')

    # 23. wsx's boundary (docs/tasks.md "Permissions"): one Seatbelt profile per role over the
    mark('23')
    # whole agent process tree, so this fake worker, which is not Claude, is inside it too.
    def launches_in(n):
        copy = os.path.realpath(show(n)['record']['working_copy'])
        return [t for t in traces() if t.get('launch') and os.path.realpath(t['cwd']) == copy]

    probe_up(created)
    # ^ A copy that exists at the launch and one made after it are both another task's.
    (base / 'project-t998').mkdir()
    main_tip, config_before = git('rev-parse', 'main'), (common / 'config').read_text()
    spec_file, answer = probe_files(next_task())
    probed = js('task', 'new', 'PROBE %s %s %s WORK p.txt' % (probe_script, spec_file, answer))['id']
    assert spec_file.name == 'probe-%d.json' % probed
    assert wait_state(probed, 'ready', 'failed')['state'] == 'ready'
    seen = next(t['probe'] for t in traces() if 'probe' in t)
    assert seen == PROBE_EXPECTED, {k: v for k, v in seen.items() if PROBE_EXPECTED.get(k) != v}
    assert git('rev-parse', 'main') == main_tip and (common / 'config').read_text() == config_before
    assert not (common / 'hooks/pre-commit').exists() and not (common / 'refs/heads/main.lock').exists()
    assert not (base / 'project-t999').exists() and not any((base / 'project-t998').iterdir())
    (base / 'project-t998').rmdir()
    assert signal_target.poll() is None, 'the worker signalled a process outside its sandbox'
    # The manager's `wsx task` commands run through its broker with no denial.
    held = js('task', 'new', 'WAIT %s NOOP' % (base / 'never'))['id']
    wait(lambda: show(held)['state'] == 'working' and launches_in(held), 'held worker')
    stopped = js('task', 'say', 'RUN wsx task stop %d -p demo --json' % held)
    assert 'ran: wsx task stop %d -p demo --json -> 0' % held in reply_to(stopped['id'])['text']
    assert wait_state(held, 'waiting')['record']['waiting']['on'] == 'owner'
    # The manager's boundary denies the main checkout, its git folder and a push (docs/tasks.md
    # "Permissions"): a write, its own commit and a push from inside it fail and change nothing.
    shared = project / 'shared.txt'
    shared.write_text('one\ntwo\nthree\nfour\nfive\n')
    git('add', 'shared.txt')
    git('commit', '-q', '-m', 'shared file')
    shared.write_text('one\ntwo\nthree\nfour\nowner edit\n')
    main_tip = git('rev-parse', 'main')
    denied = reply_to(js('task', 'say', '\n'.join((
        'RUN echo x > manager-wrote.txt',
        'RUN git -c user.name=o -c user.email=o@example.invalid commit -q -m "owner edit: shared.txt" -- shared.txt',
        'RUN git push %s HEAD:refs/heads/manager-pushed' % origin)))['id'])['text']
    assert denied.count('-> 0') == 0 and "'remote-wsx-task-push' is not a git command" in denied, denied
    assert not (project / 'manager-wrote.txt').exists() and git('rev-parse', 'main') == main_tip
    assert git('--git-dir', str(origin), 'for-each-ref', 'refs/heads/manager-pushed') == ''
    assert git('status', '--porcelain', 'shared.txt') == 'M shared.txt'
    # A land refused on the owner's uncommitted change on main that the task's change overlaps: on
    # the owner's yes `wsx task commit`, which runs in the host, commits it once the light checks
    # pass on that commit (docs/tasks.md "Manager"), and the land passes. It refuses without the
    # owner's message of this turn, and when a light check fails on the commit.
    dirty = js('task', 'new', 'LINE shared.txt 1')['id']
    assert wait_state(dirty, 'ready', 'failed')['state'] == 'ready'
    refused = land(dirty, ok=False)
    assert 'shared.txt' in refused['message'] and 'land again' in refused['message'], refused
    config_text, cargo_fails = (project / 'wsx.config.yml').read_text(), work / 'cargo-fails'
    assert '  checks:\n' in config_text, config_text
    (project / 'wsx.config.yml').write_text(config_text.replace('  checks:\n', '  checks:\n    - "cargo fmt --check"\n', 1))
    (bin_dir / 'cargo').write_text('#!/bin/sh\necho "cargo $* in $PWD" >> %s\nif [ -e %s ]; then echo "Diff in shared.txt"; exit 1; fi\n'
                                   % (work / 'cargo.log', cargo_fails))
    (bin_dir / 'cargo').chmod(0o755)
    cargo_fails.write_text('')
    commit_line = 'RUN wsx task commit shared.txt -m "owner edit: shared.txt" -p demo --json'
    stale = js('task', 'say', 'RUN wsx task commit shared.txt -m "owner edit: shared.txt" --owner-said 1 -p demo --json')
    assert '--owner-said 1: ' in reply_to(stale['id'])['text'], reply_to(stale['id'])['text']
    unsaid = reply_to(js('task', 'say', commit_line)['id'])['text']
    assert '-> 0' not in unsaid and 'pass --owner-said <id>; nothing was committed' in unsaid, unsaid
    failing = reply_to(js('task', 'say', commit_line + ' --owner-said OWNER_SAID')['id'])['text']
    assert '`cargo fmt --check` failed on the commit (exit 1); nothing was committed' in failing, failing
    assert 'Diff in shared.txt' in failing and git('rev-parse', 'main') == main_tip
    assert not list(base.glob('project-commit-*')), 'the scratch copy is left'
    cargo_fails.unlink()
    passed = reply_to(js('task', 'say', commit_line + ' --owner-said OWNER_SAID')['id'])['text']
    assert '-> 0' in passed and '"checks": [\n    "cargo fmt --check"\n  ]' in passed, passed
    assert git('log', '-1', '--format=%s', 'main') == 'owner edit: shared.txt' and git('rev-parse', 'main~1') == main_tip
    # ^ Only the named file: the changed config stays uncommitted, and the index matches main again.
    assert git('status', '--porcelain', 'shared.txt', 'wsx.config.yml') == 'M wsx.config.yml'
    assert git('diff', '--cached', '--name-only') == ''
    ran = (work / 'cargo.log').read_text().splitlines()
    assert len(ran) == 2 and all(line.startswith('cargo fmt --check in ') and '/project-commit-' in line for line in ran), ran
    assert not list(base.glob('project-commit-*'))
    (project / 'wsx.config.yml').write_text(config_text)
    (bin_dir / 'cargo').unlink()
    assert land(dirty)['ok']
    wait_closed(dirty)
    assert git('show', 'main:shared.txt').splitlines()[4] == 'owner edit'
    for n in (probed, held):
        js('task', 'close', n, '--discard')

    # 23g. The manager's new task names no files an open task names or changes (docs/tasks.md
    # "Store"): it adds to that task or starts after it. `why` and `review` read a task for the
    # manager, through its broker as for the owner (docs/tasks.md "Manager").
    mark('23g')
    ov_gate = work / 'release-23g'
    ov_first = js('task', 'new', '--files', 'ov*.txt', '--title', 'Overlap first', 'WAIT %s WORK ov-a.txt' % ov_gate)['id']
    ov_new = 'RUN wsx task new --files "ov*.txt" -p demo --json%s NOOP'
    refused = reply_to(js('task', 'say', ov_new % '')['id'])['text']
    assert '-> 0' not in refused and ('--files ov*.txt meets open task #%d "Overlap first": add the work to it with '
                                      'wsx task add %d, or start after it with --after %d --because files; no task was '
                                      'created' % (ov_first, ov_first, ov_first)) in refused, refused
    ov_after = next_task()
    assert '-> 0' in reply_to(js('task', 'say', ov_new % (' --after %d --because files' % ov_first))['id'])['text']
    why = js('task', 'why', ov_after)
    assert why['after']['task'] == ov_first and why['after']['because'] == 'files' and why['history']['entries'], why
    assert why['last_message']['mode'] == 'prompt' and why['last_message']['text'] == 'NOOP', why
    said_why = reply_to(js('task', 'say', 'RUN wsx task why %d -p demo' % ov_after)['id'])['text']
    assert '-> 0' in said_why and 'after:   #%d lands (files)' % ov_first in said_why, said_why
    ov_gate.write_text('')
    assert wait_state(ov_first, 'ready', 'failed')['state'] == 'ready'
    review = js('task', 'review', ov_first)
    assert [m['mode'] for m in review['asked']] == ['prompt'] and 'WORK ov-a.txt' in review['asked'][0]['text'], review
    assert review['base'] == git('merge-base', 'main', review['head']) and len(review['commits']) == 1, review
    assert 'ov-a.txt' in review['stat'] and '+++ b/ov-a.txt' in review['diff'], review
    said_review = reply_to(js('task', 'say', 'RUN wsx task review %d -p demo' % ov_first)['id'])['text']
    assert '-> 0' in said_review and '== prompt ' in said_review, said_review
    # ^ A task no worker edits blocks only through the paths its commits changed: #ov_after waits
    # for #ov_first with no commits, #ov_quiet is stopped with none, #ov_first is stopped after
    # its commit of ov-a.txt.
    ov_quiet = js('task', 'new', '--files', 'ovq*.txt', 'WAIT %s WORK ovq.txt' % (work / 'release-23g-q'))['id']
    js('task', 'add', ov_first, 'WAIT %s' % (work / 'release-23g-2'))
    for n in (ov_quiet, ov_first):
        wait(lambda n=n: show(n)['state'] == 'working', 'the 23g worker of #%d' % n)
        assert js('task', 'stop', n)['state'] == 'waiting'
    ov_free = next_task()
    passed = reply_to(js('task', 'say', ov_new.replace('"ov*.txt"', '"ovq*.txt" --files ov-b.txt') % '')['id'])['text']
    assert '-> 0' in passed, passed
    refused = reply_to(js('task', 'say', ov_new.replace('"ov*.txt"', 'ov-a.txt') % '')['id'])['text']
    assert '-> 0' not in refused and '--files ov-a.txt meets open task #%d "Overlap first"' % ov_first in refused, refused
    for n in (ov_first, ov_after, ov_quiet, ov_free):
        js('task', 'close', n, '--discard')

    # 23s. An untrusted agent (Pi with any model) runs under the strict profile and the broker's
    # class table (docs/tasks.md "Permissions", docs/design/local-model-fallback.md 5.6). A scripted
    # weak model (scripts/fake-weak-model.py) tries each forbidden action as the write tool, as bash
    # and as `python -c`; the OS profile and the broker refuse, whatever the model does.
    mark('23s')
    config_before, registry = (project / 'wsx.config.yml').read_text(), config_dir / 'config-v2.toml'
    registry_before = registry.read_text()
    with open(registry, 'a') as out:
        # ^ `may` cuts the role's group down (crates/wsx-core/src/task/policy.rs): none passes all of it.
        for name, may in (('m1', None), ('m2', '["read", "notify", "backlog", "direct"]'), ('m3', '["answer"]'),
                          ('a1', '["answer", "note"]')):
            out.write('[[agents]]\nname = "strict-%s"\ncommand = ["pi"]\nadapter = "pi"\nmodel = "fake/strict-%s"\n%s'
                      % (name, name, 'may = %s\n' % may if may else ''))
    task_dir = Path(created['task_dir']).parent
    fixtures = {home / '.zshrc': 'zshrc\n', home / '.aws/credentials': 'aws-secret\n', home / '.config/gh/hosts.yml': 'gh-secret\n',
                home / '.pi/agent/auth.json': 'pi-secret\n'}
    for path, text in fixtures.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    for folder in (base / 'project-t998', common / 'hooks', home / '.cargo/registry', home / 'Library/Caches/wsx-probe', home / '.ssh'):
        folder.mkdir(parents=True, exist_ok=True)
    guarded = [common / 'hooks/pre-commit', project / 'pwned.txt', base / 'project-t998/probe.txt', task_dir / 'probe.txt',
               home / '.ssh/probe', home / '.cargo/registry/probe.txt', home / 'Library/Caches/wsx-probe/probe.txt']
    config_text = (common / 'config').read_text()
    # ^ A server on loopback is the control that works. The test addresses (RFC 5737) answer nobody: the
    # profile refuses a connection to them at once (EPERM), a profile that lets it through times out.
    server = socket.socket()
    server.bind(('127.0.0.1', 0))
    server.listen(64)
    atexit.register(server.close)
    port = server.getsockname()[1]
    remote_ip, other_ip, remote_port = '192.0.2.10', '192.0.2.11', 8443
    connect, allow = {'remote_host': [remote_ip, remote_port]}, {'loopback': ['127.0.0.1', port]}
    spec = {'write': {'zshrc': str(home / '.zshrc'), 'main_file': str(project / 'pwned.txt'), 'other_copy': str(base / 'project-t998/probe.txt'),
                      'state': str(task_dir / 'probe.txt'), 'main_ref': str(common / 'refs/heads/main'), 'hooks': str(common / 'hooks/pre-commit'),
                      'git_config': str(common / 'config'), 'ssh': str(home / '.ssh/probe'), 'home_cargo': str(home / '.cargo/registry/probe.txt'),
                      'home_library': str(home / 'Library/Caches/wsx-probe/probe.txt')},
            'read': {'ssh': str(secret), 'aws': str(home / '.aws/credentials'), 'gh': str(home / '.config/gh/hosts.yml'),
                     'pi_auth': str(home / '.pi/agent/auth.json'), 'wsx_config': str(registry)},
            'copy': {'zshrc': str(home / '.zshrc'), 'hooks': str(common / 'hooks/pre-commit')},
            'own_file': str(ROOT / 'scripts/fake-weak-model.py'), 'connect': connect, 'allow': allow, 'wsxd_socket': str(daemon_socket),
            'exec': {'sudo': ['/usr/bin/sudo', '-n', 'true'], 'su': ['/usr/bin/su', '-m', 'nobody', '-c', 'true'],
                     'osascript': ['/usr/bin/osascript', '-e', '1'], 'launchctl': ['/bin/launchctl', 'list'],
                     'open': ['/usr/bin/open', '-a', 'NoSuchApplication']},
            'kill': signal_target.pid, 'link_target': str(home / '.zshrc'), 'main': str(project),
            'wsx': {'approve': ['wsx', 'task', 'approve', 'a1', '-p', 'demo'], 'land': ['wsx', 'task', 'land', '1', '-p', 'demo'],
                    'new': ['wsx', 'task', 'new', 'weak model task', '-p', 'demo'], 'deploy': ['wsx', 'task', 'deploy', '-p', 'demo']},
            'control_write': 'weak-control.txt'}
    (work / 'weak-spec.json').write_text(json.dumps(spec))
    configure(LAND_CONFIG + '  agent: { adapter: pi, model: fake/pi-model }\n', 'untrusted worker')
    main_tip = git('rev-parse', 'main')  # ^ `configure` commits on main.
    tasks_before = [row['id'] for row in js('task', 'list', '--all')['tasks']]
    weak = js('task', 'new', 'FORBID %s %s %s WORK weak.txt' % (ROOT / 'scripts/fake-weak-model.py', work / 'weak-spec.json', work / 't' / 'weak-out.json'))['id']
    assert wait_state(weak, 'ready', 'failed')['state'] == 'ready', (show(weak), [t for t in traces() if t.get('adapter') == 'pi'][-8:])
    crash = [t for t in traces() if t.get('forbid_exit')]
    assert not crash, crash
    tried = next(t['forbid'] for t in traces() if 'forbid' in t)
    allowed = {name: value for name, value in tried.items() if name.startswith('control:')}
    worked = sorted(name for name, value in tried.items() if value and not name.startswith('control:'))
    # F1 to F5: every forbidden action failed, in all three ways; the controls worked.
    assert allowed == {'control:write_copy': True, 'control:connect_loopback': True, 'control:pty': True}, allowed
    assert not worked, worked
    assert sum(1 for name in tried if name.startswith('tool:')) == sum(1 for name in tried if name.startswith('python:')) > 15, tried
    assert 'tool:connect_remote_host' in tried, tried
    # ^ Nothing changed: files, refs, the state and the daemon.
    assert all(path.read_text() == text for path, text in fixtures.items())
    assert not any(path.exists() for path in guarded), [str(path) for path in guarded if path.exists()]
    assert (common / 'config').read_text() == config_text and git('rev-parse', 'main') == main_tip
    assert sorted(row['id'] for row in js('task', 'list', '--all')['tasks']) == sorted(tasks_before + [weak]), 'a task was created'
    assert signal_target.poll() is None
    assert js('task', 'ledger')['approvals'] == []
    assert show(weak)['record']['adapter'] == 'pi' and any(t.get('launch') and t.get('worker') and t.get('model') == 'fake/pi-model' for t in traces())
    js('task', 'close', weak, '--discard')
    # The start set reaches a Pi worker through the same first turn (docs/memory.md "Start set"): the area brief of
    # the config; the fact whose paths meet the task's files stays out.
    pi_config = (project / 'wsx.config.yml').read_text()
    (project / 'briefs').mkdir()
    (project / 'briefs' / 'pi.md').write_text('Pi area brief: the tool gate lives here.\n')
    (project / 'wsx.config.yml').write_text(pi_config + 'memory:\n  areas:\n    "pi-area/**": briefs/pi.md\n')
    cli('memory', 'add', '--kind', 'fact', '--slug', 'start-pi', '--text-file', '-', '--source', 'task:1', '--verified-by', 'wsx.config.yml',
        '--commit', git('rev-parse', 'HEAD'), '--paths', 'pi-area/x.rs', stdin='The Pi gate keeps its list in pi-area/x.rs.')
    seen_messages = len(traces())
    pi_start = js('task', 'new', '--files', 'pi-area/**', 'NOOP')['id']
    assert wait_state(pi_start, 'ready', 'failed')['state'] == 'ready' and show(pi_start)['record']['adapter'] == 'pi'
    pi_prompt = next(t['message'] for t in traces()[seen_messages:] if 'Task context from the host:' in t.get('message', ''))
    assert 'Area brief briefs/pi.md (files pi-area/**):\nPi area brief: the tool gate lives here.' in pi_prompt, pi_prompt
    assert 'start-pi' not in pi_prompt and 'not an instruction' not in pi_prompt, pi_prompt
    js('task', 'close', pi_start, '--discard')
    (project / 'wsx.config.yml').write_text(pi_config)
    shutil.rmtree(project / 'briefs')
    js('task', 'close', weak, '--discard')
    # P6: a Pi agent on a remote provider reaches it through the egress proxy (crates/wsx/src/task/egress.rs) and
    # nothing else. Seatbelt names no remote host, so the profile refuses every address but the loopback, and the
    # proxy tunnels to the provider's host and port alone. The stand-in provider listens on this machine's own
    # address, which is not loopback (a test address would answer nobody).
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.connect((remote_ip, 9))  # ^ Sends nothing: it only picks the interface.
    lan = udp.getsockname()[0]
    udp.close()
    assert not lan.startswith('127.'), 'this machine has no address but the loopback to stand in for a remote provider'
    reached = []

    def stand_in(listener, name):
        while True:
            try:
                conn, _ = listener.accept()
            except OSError:
                return
            reached.append(name)
            with conn:
                conn.settimeout(2)
                try:
                    if conn.recv(4) == b'ping':
                        conn.sendall(b'pong')
                except OSError:
                    pass

    listeners = {}
    for name in ('provider', 'other'):
        listeners[name] = socket.socket()
        listeners[name].bind((lan, 0))
        listeners[name].listen(8)
        atexit.register(listeners[name].close)
        threading.Thread(target=stand_in, args=(listeners[name], name), daemon=True).start()
    provider_port, other_port = (listeners[name].getsockname()[1] for name in ('provider', 'other'))

    def proxy_closed(url):
        proxy_port = int(url.rsplit(':', 1)[1])

        def closed():
            try:
                socket.create_connection(('127.0.0.1', proxy_port), timeout=1).close()
            except OSError:
                return True
            return False
        wait(closed, 'the egress proxy of %s closed' % url)

    assert tried['proxy:url'] is None, tried  # ^ A provider on this machine needs no proxy.
    # 23r. The lean runner (docs/tasks.md "Pi adapter"): an entry with `extension` gets a policy file from the same
    # paths as the profile, and the runner's deny line reaches the task's log. The fake pi checks the variables, the
    # file's hash and shape, and stands in for the runner and the write tool (scripts/fake-pi-rpc.py RUNNERWRITE).
    runner_ext = work / 'runner-ext.js'
    runner_ext.write_text('// the lean runner stands here\n')
    with open(registry, 'a') as out:
        out.write('[[agents]]\nname = "strict-runner"\ncommand = ["pi"]\nadapter = "pi"\nmodel = "fake/runner-model"\n'
                  'extension = %s\n' % json.dumps(str(runner_ext)))
    configure(LAND_CONFIG + '  agent: { adapter: pi, model: fake/runner-model }\n', 'untrusted worker with a runner')
    outside = project / 'runner-outside.txt'
    gated = js('task', 'new', 'RUNNERWRITE %s WORK runner.txt' % outside)['id']
    assert wait_state(gated, 'ready', 'failed')['state'] == 'ready', (show(gated), [t for t in traces() if t.get('adapter') == 'pi'][-8:])
    runner_launch = next(t for t in traces() if t.get('launch') and t.get('model') == 'fake/runner-model' and t.get('worker'))
    runner = runner_launch['runner']
    assert runner_launch['extension'] == str(runner_ext) and runner['ok'] and runner['sha_matches'] and runner['sha_lower_hex'], runner
    assert runner['mode'] == '0o600' and runner['folder'] == runner_launch['agent_dir'] and 'policy.json' in runner_launch['agent_dir_files'], runner_launch
    rules = runner['policy']
    copy_dir = os.path.realpath(show(gated)['record']['working_copy'])
    inside = lambda path: any(os.path.realpath(path) == w or os.path.realpath(path).startswith(w + '/') for w in rules['write'])
    assert rules['tools'] == ['read', 'bash', 'edit', 'write'] and rules['read'] == ['/'], rules
    assert copy_dir in rules['write'] and inside(copy_dir + '/a.txt') and not inside(outside) and not inside(home / '.zshrc'), rules['write']
    assert os.path.realpath(home / '.ssh') + '/**' in rules['secrets'] and os.path.realpath(home / '.config/gh') + '/**' in rules['secrets'], rules['secrets']
    # ^ The runner and the profile refuse the same write; the control in the working copy passes both.
    tried = next(t['runner_write'] for t in traces() if 'runner_write' in t)
    assert tried['target'] == {'runner_denied': True, 'written': False, 'error': 'PermissionError'}, tried
    assert tried['control'] == {'runner_denied': False, 'written': True}, tried
    assert not outside.exists()
    denied = wait(lambda: [e for e in log(gated) if e['kind'] == 'agent' and e['text'].startswith('runner denied write ')], 'the runner deny line is logged')
    assert len(denied) == 1 and not denied[0].get('said') and 'runner-outside.txt' in denied[0]['text'] and 'write refused' in denied[0]['text'], denied
    assert not any('plain stderr line' in e.get('text', '') for e in log(gated)), 'a line that is no runner line is not progress'
    js('task', 'close', gated, '--discard')
    wait(lambda: not Path(runner['path']).exists(), 'the policy file goes with the launch folder')
    # 23m. An untrusted agent's memory is read only (docs/memory.md "Agents"): its search works, every write is refused
    # through its host's broker with the reason, and the store keeps the owner's rule as it was.
    cli('memory', 'add', '--kind', 'rule', '--slug', 'seed-rule', '--text-file', '-', '--source', 'msg:1', '--verified-by', 'owner:1',
        stdin='Seed rule for the memory probes.')
    pi_memory = js('task', 'new', 'MEMORY WORK pimemory.txt')['id']
    assert wait_state(pi_memory, 'ready', 'failed')['state'] == 'ready', show(pi_memory)
    answers = next(t['memory'] for t in traces() if 'memory' in t and 'session' in t)
    assert answers['search'][0] == 0 and [h['slug'] for h in memory_records(answers['search'][1])] == ['seed-rule'], answers
    for name in ('add_episode', 'add_rule', 'update_rule', 'supersede_rule'):
        assert answers[name][0] == 2 and 'reads memory and writes none' in answers[name][1], (name, answers)
    assert memory_records(cli('memory', 'search', 'worker', '--json').stdout) == [], 'the untrusted agent wrote nothing'
    js('task', 'close', pi_memory, '--discard')
    models = home / '.pi/agent/models.json'
    models.write_text(json.dumps({'providers': {'remote': {'baseUrl': 'http://%s:%d/v1' % (lan, provider_port), 'api': 'openai-completions',
                                                           'apiKey': 'none', 'models': [{'id': 'pi-model'}]}}}))
    # ^ (a) another host on the provider's port, (b) a direct connection past the proxy, (c) other targets through it.
    # Seatbelt counts this machine's own addresses as `localhost`: a direct connection to the stand-in succeeds, so (b)
    # and (a) are judged on non-local test addresses with the provider's port, the hole the old port rule left open.
    spec['connect'] = {'other_host_direct': [other_ip, provider_port], 'remote_direct': [remote_ip, provider_port]}
    spec['allow'] = allow
    spec['proxy'] = {'provider': [lan, provider_port, 'connect'], 'other_host': [other_ip, provider_port, 'connect'],
                     'other_port': [lan, other_port, 'connect'], 'loopback': ['127.0.0.1', port, 'connect'], 'plain_http': [lan, provider_port, 'plain']}
    (work / 'weak-spec.json').write_text(json.dumps(spec))
    configure(LAND_CONFIG + '  agent: { adapter: pi, model: remote/pi-model }\n', 'untrusted worker on a remote provider')
    remote = js('task', 'new', 'FORBID %s %s %s' % (ROOT / 'scripts/fake-weak-model.py', work / 'weak-spec.json', work / 't' / 'weak-out2.json'))['id']
    assert wait_state(remote, 'ready', 'failed')['state'] == 'ready', (show(remote), [t for t in traces() if t.get('adapter') == 'pi'][-8:])
    seen = [t['forbid'] for t in traces() if 'forbid' in t][-1]
    assert seen['control:connect_loopback'], seen
    assert not any(seen[f'{mode}:connect_{name}'] for mode in ('tool', 'bash', 'python') for name in spec['connect']), seen
    assert not sorted(name for name, value in seen.items() if value and not name.startswith(('control', 'proxy'))), seen
    refused = ('403', '403')
    assert {name: (seen['proxy:tool:' + name], seen['proxy:python:' + name]) for name in spec['proxy']} == {
        'provider': ('200 pong', '200 pong'), 'other_host': refused, 'other_port': refused, 'loopback': refused, 'plain_http': refused}, seen
    assert seen['proxy:url'].startswith('http://127.0.0.1:') and 'other' not in reached, (seen, reached)
    js('task', 'close', remote, '--discard')
    proxy_closed(seen['proxy:url'])  # ^ The proxy ends with its agent.
    for path in list(fixtures) + guarded:
        path.unlink(missing_ok=True)
    (base / 'project-t998').rmdir()
    # F8: the trusted manager profile has none of the strict lines, and each of its lines is in the strict one.
    trusted = cli('task', 'boundary-profile', '-p', 'demo').stdout
    strict = cli('task', 'boundary-profile', '-p', 'demo', '--untrusted').stdout
    assert '(deny file-write* )' not in trusted and 'network' not in trusted, trusted
    push_rule = '(deny process-exec (regex #"(/git-(remote-[a-z0-9]+|receive-pack)|^/usr/bin/ssh)$"))'
    assert [l for l in trusted.splitlines() if 'process-exec' in l] == [push_rule] and push_rule in strict.splitlines(), trusted
    # ^ A manager, trusted or not, runs no push program; the strict exec denials stay strict-only.
    assert set(l for l in trusted.splitlines() if 'deny file-' in l or 'allow file-' in l) <= set(strict.splitlines()), strict
    for rule in ('(deny file-write* )', '(deny network* )', '(deny network-outbound (remote unix-socket (path-literal "%s")))' % os.path.realpath(daemon_socket)):
        assert rule in strict.splitlines(), (rule, strict)
    assert '(deny process-exec (literal "/usr/bin/sudo"))' in strict and '(subpath "%s")' % os.path.realpath(home / '.config/gh') in strict
    # F5 (broker), F6, F7, F9: an untrusted manager. With no `may` it has the manager's group, and its outward
    # commands ask the owner; `may` cuts the group down: read, notify, backlog and direct (m2), only answer (m3).
    # A named chat (the area group) on an agent that passed answer and note may not create a task.
    def stop_manager_now():
        sock = js('task', 'manager')['socket']
        cli('session', 'delete', 'task-manager', '-p', 'demo')
        wait(lambda: not Path(sock).exists() or subprocess.run(['nc', '-zU', sock]).returncode != 0, 'manager down')

    def manager_as(name, *lines):
        stop_manager_now()
        configure(LAND_CONFIG.replace('{ auto: false }', '{ auto: false, approve: false }') + '  manager: { adapter: pi, model: fake/strict-%s }\n' % name,
                  'untrusted manager ' + name)
        return reply_to(js('task', 'say', '\n'.join('RUN ' + line for line in lines))['id'])['text']

    # P6 (manager): an untrusted manager on the remote provider is up, reaches it through the proxy and nothing else.
    probe = work / 'connect-probe.py'
    probe.write_text('import socket, sys\ntry:\n    socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=2).close()\n'
                     'except PermissionError:\n    print("refused")\n    sys.exit(0)\nexcept OSError:\n    pass\nprint("passed")\n')
    tunnel = lambda host, number, kind='connect': 'python3 %s --tunnel %s %d %s' % (ROOT / 'scripts/fake-weak-model.py', host, number, kind)
    stop_manager_now()
    configure(LAND_CONFIG + '  manager: { adapter: pi, model: remote/pi-model }\n', 'untrusted manager on a remote provider')
    lines = ['echo $HTTPS_PROXY', tunnel(lan, provider_port), tunnel(other_ip, provider_port), 'python3 %s %s %d' % (probe, other_ip, provider_port),
             tunnel(lan, other_port), tunnel('127.0.0.1', port), tunnel(lan, provider_port, 'plain')]
    said = reply_to(js('task', 'say', '\n'.join('RUN ' + line for line in lines))['id'])['text']
    runs = ['ran: ' + part for part in said.split('ran: ')[1:]]
    # ^ The first line of each answer: the macOS python3 shim may add a stderr line about its cache.
    answers = [run.split(' -> 0 ', 1)[1].splitlines()[0].strip() for run in runs if ' -> 0 ' in run]
    assert len(runs) == len(lines) and answers[0].startswith('http://127.0.0.1:') and answers[1:] == ['200 pong', '403', 'refused', '403', '403', '403'], said
    assert 'other' not in reached, reached
    assert any(t.get('launch') and t.get('model') == 'remote/pi-model' and not t.get('worker') for t in traces()), 'no manager launch on the remote provider'
    stop_manager_now()
    proxy_closed(answers[0])
    models.unlink()
    ready = js('task', 'new', 'WORK landme.txt')['id']
    assert wait_state(ready, 'ready', 'failed')['state'] == 'ready'
    said = manager_as('m1', 'wsx task land %d -p demo --json' % ready, 'wsx task land %d -p demo --owner-said OWNER_SAID --json' % ready,
                      'wsx task close %d -p demo --discard --json' % ready, 'wsx task approve a1 -p demo', 'wsx task frobnicate',
                      'wsx task host demo 1', 'wsx task list -p demo --json',
                      'wsx task check pass c1 --owner-said OWNER_SAID -p demo', 'wsx task routine delete nightly --owner-said OWNER_SAID -p demo')
    runs = ['ran: ' + part for part in said.split('ran: ')[1:]]  # ^ A command's JSON answer spans lines.
    assert len(runs) == 9, said
    # ^ The owner's message is no word of a model that may be any model: a pass or a routine (it runs outside the boundary) needs the owner.
    assert '-> 1' in runs[7] and 'is for the owner' in runs[7], runs[7]
    assert '-> 1' in runs[8] and 'the owner adds, edits and deletes routines' in runs[8], runs[8]
    assert 'wsx task land %d -p demo --json -> 0' % ready in runs[0] and 'needs the owner' in runs[0], runs[0]
    assert 'owner-said' in runs[1] and '-> 0' in runs[1] and 'needs the owner' in runs[1], runs[1]
    assert '--discard --json -> 0' in runs[2] and 'needs the owner' in runs[2], runs[2]
    # ^ The broker passes the owner's commands and an unknown subcommand on: the CLI refuses them as before.
    assert '-> 1' in runs[3] and 'is for the owner' in runs[3], runs[3]
    assert '-> 2' in runs[4] and 'unrecognized subcommand' in runs[4], runs[4]
    assert '-> 2' in runs[5] and 'is not for the manager' in runs[5], runs[5]
    assert 'wsx task list -p demo --json -> 0' in runs[6], runs[6]
    asked = js('task', 'ledger')['approvals']
    assert {p['action'] for p in asked} == {'discard', 'land'}, asked
    assert show(ready)['state'] == 'ready' and git('log', '-1', '--format=%s', 'main') == 'untrusted manager m1' and main_has('landme.txt') is False
    for request in asked:
        cli('task', 'deny', request['id'], '-p', 'demo')
    said = manager_as('m2', 'wsx task new tier-two-task -p demo --json', 'wsx task land %d -p demo --json' % ready, 'wsx task list -p demo --json',
                      'wsx task stop %d -p demo --json' % ready)
    runs = ['ran: ' + part for part in said.split('ran: ')[1:]]
    assert all('-> 2' in line and 'is beyond what this agent may do' in line and 'it passed: read, notify, backlog, direct' in line
               for line in runs[:2]), runs
    assert '-> 0' in runs[2] and 'is beyond' not in runs[3], runs
    assert js('task', 'ledger')['approvals'] == [] and show(ready)['state'] in ('ready', 'waiting')
    said = manager_as('m3', 'wsx task list -p demo --json', 'wsx task ledger -p demo')
    runs = ['ran: ' + part for part in said.split('ran: ')[1:]]
    assert len(runs) == 2 and all('-> 2' in line and 'it passed: answer' in line for line in runs), said
    # ^ A named chat runs as the area group; its agent passed only answer and note, so even `new` and `list` are refused,
    # and the chat's own limit (no deploy) still reads as before.
    goal = work / 'strict-goal.md'
    goal.write_text('Keep the strict area.\n')
    cli('task', 'chat', 'new', 'strictarea', '--goal', str(goal), '-p', 'demo', '--json')
    stop_manager_now()
    configure(LAND_CONFIG + '  manager: { adapter: pi, model: fake/strict-a1 }\n', 'untrusted area')
    ids_before = sorted(row['id'] for row in js('task', 'list', '--all')['tasks'])
    asked = js('task', 'say', '--to', 'strictarea', 'RUN wsx task new area-task -p demo --json\nRUN wsx task list -p demo --json')
    said = wait(lambda: next((unquoted(e) for e in js('task', 'chat', '--chat', 'strictarea')['entries']
                              if e['kind'] == 'reply' and asked['id'] in e['to']), None), 'the chat answers', 60)['text']
    runs = ['ran: ' + part for part in said.split('ran: ')[1:]]
    assert len(runs) == 2 and all('-> 2' in line and 'is beyond what this agent may do' in line and 'it passed: answer, note' in line
                                  for line in runs), said
    assert sorted(row['id'] for row in js('task', 'list', '--all')['tasks']) == ids_before, 'the chat created a task'
    cli('task', 'chat', 'close', 'strictarea', '-p', 'demo', '--json')
    stop_manager_now()
    registry.write_text(registry_before)
    configure(config_before, 'strict restored')
    js('task', 'close', ready, '--discard')

    # 24b. A land that meets index.lock in the main checkout: a fresh lock refuses naming it, an
    # old one that a live process holds open refuses with its pid, an old one with no holder is
    # cleared (log `lock_cleared`) and the land goes on. Only index.lock is ever removed.
    mark('24b')
    locked = js('task', 'new', 'WORK locked.txt')['id']
    assert wait_state(locked, 'ready', 'failed')['state'] == 'ready'
    lock, other_lock = common / 'index.lock', common / 'shallow.lock'
    named = os.path.realpath(common) + '/index.lock'  # ^ git names the real path
    lock.write_text('')
    other_lock.write_text('')
    refused = land(locked, ok=False)
    assert named in refused['message'] and 'younger than 300 s' in refused['message'], refused
    assert lock.exists() and not kinds(locked, 'lock_cleared'), log(locked)
    old = time.time() - 600
    os.utime(lock, (old, old))
    with open(lock, 'ab') as held_open:
        holder = subprocess.Popen(['sleep', '60'], stdout=held_open)
    try:
        refused = land(locked, ok=False)
        assert named in refused['message'] and 'held open by pid %d' % holder.pid in refused['message'], refused
        assert lock.exists() and not kinds(locked, 'lock_cleared'), log(locked)
    finally:
        holder.kill()
        holder.wait()
    assert land(locked)['ok']
    wait_closed(locked)
    cleared = [e for e in log(locked) if e['kind'] == 'lock_cleared']
    assert len(cleared) == 1 and cleared[0]['path'].endswith('/.git/index.lock') and cleared[0]['age_s'] >= 600, cleared
    assert not lock.exists() and other_lock.exists() and main_has('locked.txt')
    other_lock.unlink()

    # 25. A done result whose checks fail goes back to the worker with the failed command and
    mark('25')
    # its output, at most twice, then the task fails as before.
    fixable = 'test -f fixed.txt || { echo "WORK fixed.txt"; exit 1; }'
    configure(CONFIG.replace('    - "test -f a.txt"\n    - "test ! -e broken.txt"\n',
              '    - %s\n' % json.dumps(fixable)), 'fixable check')
    f = js('task', 'new', 'WORK retry1.txt')['id']
    fixed = wait_state(f, 'ready', 'failed')
    assert fixed['state'] == 'ready' and fixed['revision'] == 2, fixed
    retries = [e for e in log(f) if e['kind'] == 'check_retry']
    assert [(e['revision'], e['attempt'], e['commands']) for e in retries] == [(1, 1, [fixable])], retries
    retry_message = next(t['message'] for t in traces() if fixable in t.get('message', ''))
    assert '(automatic retry 1 of 2)' in retry_message and fixable in retry_message and 'WORK fixed.txt' in retry_message
    configure(CONFIG.replace('    - "test -f a.txt"\n    - "test ! -e broken.txt"\n',
              '    - "echo stays broken; exit 3"\n'), 'broken check')
    b = js('task', 'new', 'WORK retry2.txt')['id']
    broken = wait_state(b, 'failed', 'ready')
    assert broken['state'] == 'failed' and broken['revision'] == 3, broken
    assert [e['attempt'] for e in log(b) if e['kind'] == 'check_retry'] == [1, 2]
    # ^ A message from a person allows two more.
    js('task', 'add', b, 'NOOP')
    wait(lambda: len([e for e in log(b) if e['kind'] == 'check_retry']) == 4 and show(b)['state'] == 'failed', 'two more retries')
    for n in (f, b):
        js('task', 'close', n, '--discard')

    # 26. A host follows a replaced wsx binary (docs/tasks.md "Host"): the next turn execs the
    mark('26')
    # new file in the same process, which resumes the agent with the new launch.
    configure(CONFIG.split('  checks:')[0], 'no checks')
    moved = work / 'moved'
    moved.mkdir(mode=0o700)
    shutil.copy2(wsx, moved / 'wsx')
    started = subprocess.run([str(moved / 'wsx'), 'task', 'new', 'WORK follow1.txt', '--json'], cwd=project, env=env,
                             capture_output=True, text=True, timeout=60)
    r = json.loads(started.stdout)['id']
    assert wait_state(r, 'ready', 'failed')['state'] == 'ready'

    def host_process():
        rows = subprocess.run(['ps', '-axwwo', 'pid=,command='], capture_output=True, text=True).stdout.splitlines()
        return next(row.split()[0] for row in rows if ' task host ' in row and row.rstrip().endswith(' %d' % r))

    before = host_process()
    launches = len(launches_in(r))
    # ^ A deploy replaces the file (a new inode), as install and mv do: macOS kills a process
    # that execs an executable overwritten in place.
    shutil.copy2(wsx, moved / 'wsx.new')
    os.replace(moved / 'wsx.new', moved / 'wsx')
    added = js('task', 'add', r, 'WORK follow2.txt')
    assert added['ok'], added
    wait(lambda: len(launches_in(r)) > launches, 'the agent restarts with the new binary')
    assert wait_state(r, 'ready', 'failed')['state'] == 'ready'
    after = host_process()
    # ^ exec keeps the pid; the agent, which a live host would reuse, started again and resumed.
    relaunch = launches_in(r)[launches:]
    assert after == before and len(relaunch) == 1 and relaunch[0]['resume'], (before, after, relaunch)
    assert not (Path(show(r)['log']).parent / 'handoff.txt').exists()
    # ^ The turn the old host handed over logs its real cause, and that it came through the exec.
    carried = [e for e in log(r) if e['kind'] == 'result'][-1]
    assert carried['trigger'] == 'message' and carried.get('reexec') is True, carried
    js('task', 'close', r, '--discard')
    # A ready task has no agent turn to follow a deploy at, so its land is the next start: the
    # host execs the replaced file before it takes the land, and the caller's retry gets the
    # new host's answer. The replacement is a script that marks its start, then runs wsx.
    l = json.loads(subprocess.run([str(moved / 'wsx'), 'task', 'new', 'WORK follow3.txt', '--json'], cwd=project,
                                  env=env, capture_output=True, text=True, timeout=60).stdout)['id']
    assert wait_state(l, 'ready', 'failed')['state'] == 'ready'
    marker = moved / 'started'
    (moved / 'wsx.new').write_text('#!/bin/sh\necho started >> %s\nexec %s "$@"\n' % (marker, wsx))
    os.chmod(moved / 'wsx.new', 0o755)
    os.replace(moved / 'wsx.new', moved / 'wsx')
    landed = js('task', 'land', l)
    assert landed['state'] == 'landed', landed
    assert marker.read_text().splitlines() == ['started'], 'the host execs once, before the land'
    assert (project / 'follow3.txt').exists()

    # 27. The worker runs the checks itself through its host's broker, in its host's queue and
    mark('27')
    # as after its result; any other command is refused (docs/tasks.md "Checks, revisions and report").
    # `tasks.env` reaches the worker and every check run, so both build with the same settings.
    configure(LAND_CONFIG.replace(
        '    - "test ! -e broken.txt"\n', '    - "test ! -e broken.txt"\n    - "test \\"$TASK_PROBE\\" = \\"a b\\""\n'
    ) + '  env: { TASK_PROBE: "a b" }\n', 'two checks')
    cr = js('task', 'new', 'CHECKRUN WORK cr.txt')['id']
    assert wait_state(cr, 'ready', 'failed')['state'] == 'ready'
    ran = next(t for t in traces() if 'check_run' in t)
    code, said = ran['check_run']
    assert code == 0 and 'check 1 `test ! -e broken.txt`: exit 0' in said and 'check 2 `test "$TASK_PROBE" = "a b"`: exit 0' in said, ran
    assert ran['check_run_other'][0] == 2 and 'push is not for the worker role' in ran['check_run_other'][1], ran
    assert [len(e['results']) for e in log(cr) if e['kind'] == 'check_run'] == [2]
    worker_launch = launches_in(cr)[0]
    assert '1. `test ! -e broken.txt` 2. `test "$TASK_PROBE" = "a b"`' in worker_launch['instructions']
    assert worker_launch['task_probe'] == 'a b' and launch['task_probe'] is None, worker_launch
    assert all(c['exit'] == 0 for c in show(cr)['result']['checks']), show(cr)['result']
    assert 'WSX_TASK_BROKER' in worker_launch['wsx_env'], worker_launch['wsx_env']
    js('task', 'close', cr, '--discard')
    # 27m. Memory through the worker's broker (docs/memory.md "Agents"): search and an episode work; a rule, a change
    # of the owner's rule and a scope of another project are refused with the reason.
    mark('27m')
    mem = js('task', 'new', 'MEMORY WORK mem.txt')['id']
    assert wait_state(mem, 'ready', 'failed')['state'] == 'ready', show(mem)
    answers = next(t['memory'] for t in traces() if 'memory' in t and 'session' not in t)
    assert answers['search'][0] == 0 and [h['slug'] for h in memory_records(answers['search'][1])] == ['seed-rule'], answers
    assert answers['add_episode'][0] == 0 and json.loads(answers['add_episode'][1])['slug'] == 'worker-note', answers
    for name in ('add_rule', 'update_rule', 'supersede_rule'):
        assert answers[name][0] == 2 and 'a rule needs the owner' in answers[name][1], (name, answers)
    assert answers['other_scope'][0] == 2 and "only in its own project's scope" in answers['other_scope'][1], answers
    stored = {h['slug']: h for h in memory_records(cli('memory', 'search', 'rule worker', '--json').stdout)}
    assert {'seed-rule', 'worker-note'} <= set(stored) and 'worker-rule' not in stored, stored
    assert stored['seed-rule']['excerpt'] == 'Seed rule for the memory probes.', stored
    # `verified_by` task:<n> needs a landed task and check:<id> a passed item (docs/memory.md "Record").
    refused = subprocess.run([str(wsx), 'memory', 'add', '--kind', 'fact', '--slug', 'early', '--text-file', '-', '--source', 'task:%s' % mem,
                              '--verified-by', 'task:%s' % mem], cwd=project, env=env, input='Too early.', capture_output=True, text=True)
    assert refused.returncode == 2 and 'has not landed' in refused.stderr, refused
    assert js('task', 'land', mem)['state'] == 'landed'
    cli('memory', 'add', '--kind', 'fact', '--slug', 'landed-fact', '--text-file', '-', '--source', 'task:%s' % mem,
        '--verified-by', 'task:%s' % mem, stdin='A landed task verifies this.')
    item_task = js('task', 'new', 'WORK item27.txt HV')['id']
    assert wait_state(item_task, 'ready', 'failed')['state'] == 'ready'
    assert js('task', 'land', item_task)['state'] == 'landed'
    item = next(i for i in js('task', 'check')['open'] if i['task'] == item_task)['id']
    unpassed = subprocess.run([str(wsx), 'memory', 'add', '--kind', 'fact', '--slug', 'unpassed', '--text-file', '-', '--source', 'task:%s' % mem,
                               '--verified-by', 'check:' + item], cwd=project, env=env, input='Not yet.', capture_output=True, text=True)
    assert unpassed.returncode == 2 and 'no passed check item' in unpassed.stderr, unpassed
    js('task', 'check', 'pass', item)
    cli('memory', 'add', '--kind', 'fact', '--slug', 'passed-fact', '--text-file', '-', '--source', 'task:%s' % mem,
        '--verified-by', 'check:' + item, stdin='A passed item verifies this.')
    # Below tasks.minFreeGb a task waits, says why, and goes on by itself without a retry.
    configure(LAND_CONFIG.replace('minFreeGb: 0', 'minFreeGb: 100000'), 'no disk')
    dw = js('task', 'new', 'WORK dw.txt')['id']
    waiting = wait(lambda: (current := show(dw))['state'] == 'waiting' and current['record'].get('waiting') and current,
                   'disk wait')
    assert waiting['record']['waiting']['on'] == 'disk' and ' GB free of the 100000 GB a task needs' in waiting['record']['waiting']['reason']
    assert waiting['status']['label'] == 'Waiting for disk space', waiting['status']
    assert waiting['status']['sentence'].endswith('It goes on by itself once there is space.'), waiting['status']
    assert any(e['kind'] == 'disk_wait' for e in log(dw))
    row = next(r for r in js('task', 'ledger')['open'] if r['id'] == dw)
    assert row['waiting']['on'] == 'disk', row
    # Stalls (docs/tasks.md "Manager"): a wait whose cause is real is none, past the grace.
    js('task', 'manager')

    sweeps(2)
    assert not stalls(dw), stalls(dw)
    # ^ Space is back, but a process holds the disk slot under the label of a closed task: one
    # event names the wait, its age and the evidence, and a later sweep does not repeat it.
    disk_slot = state / 'wsx' / 'tasks' / 'disk-slot'
    holder = subprocess.Popen([sys.executable, '-c', HOLD_SLOT, str(disk_slot), 'demo t%d' % cr])
    try:
        wait(lambda: (disk_slot / 'slot.lock').read_text() == 'demo t%d' % cr, 'the fixture holds the disk slot')
        configure(LAND_CONFIG, 'disk back')
        wait(lambda: 'queued for disk behind demo t%d' % cr in (show(dw)['record'].get('waiting') or {}).get('reason', ''),
             'queued behind the fixture', 60)
        stalled = wait(lambda: stalls(dw), 'the stall event', 120)
        assert len(stalled) == 1 and 'waiting on disk for ' in stalled[0], stalled
        assert 'free ' in stalled[0] and 'tasks.minFreeGb is 0; queued behind demo t%d, which is closed' % cr in stalled[0], stalled
        sweeps(1)
        assert stalls(dw) == stalled, stalls(dw)
    finally:
        holder.kill()
        holder.wait()
    wait(lambda: show(dw)['state'] == 'ready', 'resumed after disk', 60)
    assert stalls(dw) == stalled, stalls(dw)
    assert 'waiting' not in show(dw)['record'] and not any(e['kind'] == 'check_retry' for e in log(dw))
    js('task', 'close', dw, '--discard')
    # A task the manager must act on and does not is a stall once `tasks.manager.reviewMinutes`
    # passed since it entered the state; a land, or an add and a close, before that end the
    # wait (docs/tasks.md "Manager", Stalls). The fake manager reviews nothing by itself.
    configure(LAND_CONFIG.replace('tasks:\n', 'tasks:\n  manager: { reviewMinutes: 0.2 }\n'), 'review wait')

    idle, landed_first, added = (manager_new('WORK rw%d.txt' % i) for i in range(3))
    for n in (landed_first, added):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', show(n)
    assert land(landed_first)['state'] == 'landed'
    js('task', 'add', added, 'WORK rw-more.txt')
    js('task', 'close', added, '--discard')
    waited = wait(lambda: stalls(idle), 'the manager-wait stall event', 150)
    assert len(waited) == 1 and re.search(r'^#%d stalled r1: .*; ready for .*; the manager has not reviewed it$' % idle, waited[0]), waited
    sweeps(1)
    assert stalls(idle) == waited and not stalls(landed_first) and not stalls(added), (stalls(idle), stalls(landed_first), stalls(added))
    js('task', 'close', idle, '--discard')
    configure(LAND_CONFIG, 'review wait over')
    # A silent worker is one stall per silence: its turn record (`turn.json`, docs/tasks.md
    # "Agents") is past its deadline and still open, so its host did not end it. What the manager
    # or the host writes (an add, a land notice) moves no deadline; a line of the worker's does
    # (docs/tasks.md "Manager", Stalls). The record and the turn are aged by 3 h by hand, since
    # the host's own bound is `tasks.checkTimeoutSeconds` plus 10 minutes.
    mark('27q')
    quiet_gate = base / 'quiet-gate'
    quiet = js('task', 'new', 'WAIT %s' % quiet_gate, '--files', 'q.txt')['id']
    wait(lambda: show(quiet)['record']['agent_session'], 'the silent worker runs')
    quiet_dir = Path(show(quiet)['log']).parent
    aged_ms = 3 * 3600 * 1000
    turn_path = quiet_dir / 'turn.json'
    wait(turn_path.exists, 'the silent worker\'s turn record')
    turn = json.loads(turn_path.read_text())
    assert turn['agent'].startswith('claude/') and 'ended' not in turn and turn['deadline_ms'] == turn['output_ms'] + turn['quiet_ms'], turn

    def write_turn(output_ms):
        tmp = quiet_dir / 'turn.json.tmp'
        tmp.write_text(json.dumps({**turn, 'started_ms': turn['started_ms'] - aged_ms, 'output_ms': output_ms,
                                   'deadline_ms': output_ms + turn['quiet_ms']}))
        os.replace(tmp, turn_path)

    write_turn(turn['output_ms'] - aged_ms)
    record_path = quiet_dir / 'task.json'
    quiet_started = show(quiet)['record']['state_unix_ms']

    def age_record():
        # ^ The host may save its record again (an add queues a message): age it again, and
        # tell the caller whether it had to, since a record with a new state start is no silence.
        record = json.loads(record_path.read_text())
        if record['state_unix_ms'] < quiet_started - aged_ms // 2:
            return False
        for key in ('state_unix_ms', 'created_unix_ms'):
            record[key] -= aged_ms
        tmp = quiet_dir / 'task.json.tmp'
        tmp.write_text(json.dumps(record))
        os.replace(tmp, record_path)
        return True

    assert age_record()
    silent = wait(lambda: stalls(quiet), 'the silence stall event', 150)
    assert len(silent) == 1 and re.search(r'working for 3 h .*; its turn on claude/\S+ is past its quiet bound of .*; its last output 3 h', silent[0]), silent
    # ^ `wsx task why` reads the same record.
    why = cli('task', 'why', str(quiet)).stdout
    assert re.search(r'^turn:    on claude/\S+ since .*, last output 3 h .*ago, ended by its host at .* with no output$', why, re.M), why
    # ^ An add and a land notice (a message the host writes) restart no clock.
    assert js('task', 'add', quiet, 'WORK q-more.txt')['ok']
    q_lander = js('task', 'new', 'WORK q.txt')['id']
    assert wait_state(q_lander, 'ready', 'failed')['state'] == 'ready', log(q_lander)
    assert land(q_lander)['state'] == 'landed'
    wait(lambda: any('landed on main' in e.get('text', '') for e in log(quiet) if e['kind'] == 'message'), 'the land notice')
    age_record()
    sweeps(2)
    assert stalls(quiet) == silent, stalls(quiet)
    assert not age_record(), 'the record kept its aged state start through the sweeps'
    # ^ A line of the worker's is output: its silence after it is a new stall.
    line_ms = int(time.time() * 1000) - aged_ms + 1000
    with open(quiet_dir / 'log.jsonl', 'a') as out:
        out.write(json.dumps({'unix_ms': line_ms, 'kind': 'agent', 'text': 'cargo build'}) + '\n')
    write_turn(line_ms)
    again = wait(lambda: len(stalls(quiet)) == 2 and stalls(quiet), 'the stall after new output', 150)
    assert again[:1] == silent and 'cargo build' in again[1], again
    sweeps(2)
    assert stalls(quiet) == again, stalls(quiet)
    quiet_gate.write_text('go')
    js('task', 'close', quiet, '--discard')
    wait_closed(q_lander)
    # A usage limit is no failure: the task waits on `limit` and its session continues after the
    # reset, oldest wait first, one task at a time (docs/tasks.md "Claude adapter").
    traced_before_limits = len(traces())
    first = js('task', 'new', 'LIMIT 4 WORK la.txt')['id']
    limited = lambda n: (current := show(n))['state'] == 'waiting' and current['record'].get('waiting') and current
    waiting = wait(lambda: limited(first), 'limit wait')['record']['waiting']
    assert waiting['on'] == 'limit' and re.fullmatch(r'Claude 5-hour limit, resets \d\d:\d\d', waiting['reason']), waiting
    assert waiting['until_ms'] > time.time() * 1000, waiting
    row = next(r for r in js('task', 'ledger')['open'] if r['id'] == first)
    assert row['waiting']['on'] == 'limit', row
    # ^ Due first, but behind the older wait in the limit queue.
    second = js('task', 'new', 'LIMIT 1 WORK lb.txt')['id']
    wait(lambda: (w := limited(second)) and 'queued for limit behind' in w['record']['waiting']['reason'],
         'the second limit wait queued behind the first', 30)
    for n in (first, second):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', show(n)
        assert kinds(n, 'limit_wait', 'result', 'check_retry') == ['limit_wait', 'result'], log(n)
        assert "You've hit" not in next(e for e in log(n) if e['kind'] == 'result')['summary']
    sessions = {show(n)['record']['agent_session']: n for n in (first, second)}
    resumed = [sessions.get(t['session']) for t in traces()[traced_before_limits:]
               if not t.get('manager') and str(t.get('message', '')).startswith('The usage limit has reset')]
    assert resumed == [first, second], resumed
    for n in (first, second):
        js('task', 'close', n, '--discard')
    # 27b. A limit wait ends with the resume whatever comes meanwhile, also for the task the
    mark('27b')
    # limit slot passes to: a background check-run ends during the wait and wsx is replaced
    # under both hosts. Nothing calls a host after the reset (`show` would wake it): only files
    # are read.
    limit_gate = base / 'limit-gate'
    configure(LAND_CONFIG + '  scenarios:\n    slow: %s\n' % gate_cmd(limit_gate),
              'a slow scenario')
    swapped = work / 'swapped'
    swapped.mkdir(mode=0o700)
    shutil.copy2(wsx, swapped / 'wsx')

    def swapped_new(prompt):
        started = subprocess.run([str(swapped / 'wsx'), 'task', 'new', prompt, '--json'],
                                 cwd=project, env=env, capture_output=True, text=True, timeout=60)
        return json.loads(started.stdout)['id']
    first = swapped_new('LIMIT 10 CHECKRUN_LATE slow WORK le.txt')
    session = wait(lambda: limited(first), 'the limit wait during a check-run')['record']['agent_session']
    second = swapped_new('LIMIT 1 WORK lf.txt')
    wait(lambda: (w := limited(second)) and 'queued for limit behind' in w['record']['waiting']['reason'],
         'the second limit wait queued behind the first', 30)
    first_log = Path(show(first)['log'])
    on_file = lambda: [json.loads(line) for line in first_log.read_text().splitlines()]
    record = lambda: json.loads((first_log.parent / 'task.json').read_text())
    limit_gate.touch()
    wait(lambda: any(e['kind'] == 'check_run' for e in on_file()), 'the check-run ends during the wait', 30)
    assert record()['waiting']['on'] == 'limit', record()
    shutil.copy2(wsx, swapped / 'wsx.new')
    os.replace(swapped / 'wsx.new', swapped / 'wsx')
    assert record()['waiting']['on'] == 'limit', 'a limit wait that ended before its reset'
    resumes = lambda: [t['session'] for t in traces()
                       if not t.get('manager') and str(t.get('message', '')).startswith('The usage limit has reset')]
    seen = len(resumes())
    wait(lambda: len(resumes()[seen:]) == 2, 'both resumes after the reset with a replaced wsx', 60)
    assert resumes()[seen:][0] == session, resumes()[seen:]
    for n in (first, second):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    # ^ A stop, then two adds while the host restarts with a replaced wsx: the add it had not
    # answered is sent again to the new host, not lost behind an empty answer.
    shutil.copy2(wsx, swapped / 'wsx.new')
    os.replace(swapped / 'wsx.new', swapped / 'wsx')
    assert js('task', 'stop', first)['ok']
    adds = [subprocess.Popen([str(wsx), 'task', 'add', str(first), 'WORK lg%d.txt' % i, '--json'], cwd=project, env=env,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for i in range(2)]
    answers = [add.communicate(timeout=60) for add in adds]
    assert all(add.returncode == 0 and json.loads(out)['ok'] for add, (out, _) in zip(adds, answers)), answers
    wait(lambda: all(any('WORK lg%d.txt' % i in str(t.get('message', '')) for t in traces()) for i in range(2)),
         'both adds reach the agent', 30)
    logged = [e['text'] for e in log(first) if e['kind'] == 'message']
    assert all(any('WORK lg%d.txt' % i in text for text in logged) for i in range(2)), logged
    for n in (first, second):
        js('task', 'close', n, '--discard')

    # 27c. A stop outlasts a limit wait (docs/tasks.md "Host", Stop): the reset starts no turn, the task
    # waits on the owner, and an add resumes it.
    mark('27c')
    held = js('task', 'new', 'LIMIT 4 WORK lh.txt')['id']
    wait(lambda: limited(held), 'the limit wait to stop', 30)
    assert js('task', 'stop', held)['state'] == 'waiting'
    assert js('task', 'stop', held)['message'].startswith('already stopped')
    # ^ A task that is not stopped hits the same limit after it: its resume after the reset is the
    # event that comes after the moment the stopped task could have resumed.
    held_session = show(held)['record']['agent_session']
    sentinel = js('task', 'new', 'LIMIT 4 WORK lh0.txt')['id']
    sentinel_session = wait(lambda: limited(sentinel), 'the limit wait of the task that is not stopped', 30)['record']['agent_session']
    stays(lambda: sentinel_session in resumes(), lambda: held_session not in resumes(),
          'the stopped task resumed after the reset', 60)
    stopped_at = show(held)['record']
    assert show(held)['state'] == 'waiting' and stopped_at['waiting'] == {'on': 'owner', 'reason': 'stopped by request'}, stopped_at
    assert kinds(held, 'result', 'stopped') == ['stopped'], log(held)
    assert wait_state(sentinel, 'ready', 'failed')['state'] == 'ready', log(sentinel)
    js('task', 'close', sentinel, '--discard')
    js('task', 'add', held, 'WORK lh2.txt')
    assert wait_state(held, 'ready', 'failed')['state'] == 'ready', log(held)
    js('task', 'close', held, '--discard')

    # 27d. A turn reads its events as wsx wrote them; a line under them says what holds now for
    # each task that moved on since (docs/tasks.md "Manager", Events): the limit wait of a
    # task that works again by the time the owner's turn takes the event along.
    mark('27d')
    # ^ The events of 27c may still be queued: a waking line merged with the limit wait's would
    # take it into a turn before the owner's.
    manager_settled('the manager idle before the stale wait')
    stale_release = base / 'stale-release'
    stale = js('task', 'new', 'LIMIT 4 WAIT %s WORK ls.txt' % stale_release)['id']
    wait(lambda: limited(stale), 'the task waits on the limit', 30)
    stale_event = wait(lambda: events_with('#%d waiting r0' % stale, 'waits on limit'), 'the limit wait event', 30)[0]
    wait(lambda: show(stale)['state'] == 'working', 'the task works again', 30)
    asked = js('task', 'say', 'STATUS after the stale wait')
    reply_to(asked['id'])
    stale_turn = turn_with('STATUS after the stale wait')['message']
    assert 'Events (message %d)' % stale_event['id'] in stale_turn and '#%d waiting r0' % stale in stale_turn, stale_turn
    assert '\n\nsince then: #%d working r0\n\n' % stale in stale_turn, stale_turn
    assert stale_turn.index('since then:') > stale_turn.index('#%d waiting r0' % stale), stale_turn
    stale_release.write_text('')
    assert wait_state(stale, 'ready', 'failed')['state'] == 'ready', log(stale)
    # ^ The task's last event line is current: no line says what it already says.
    stale_ready = wait(lambda: turn_with('#%d ready r1' % stale), 'the ready turn', 30)['message']
    assert 'since then:' not in stale_ready, stale_ready
    js('task', 'close', stale, '--discard')

    # 28. A land deploys by itself with tasks.deploy.auto, once per quiet window: lands within
    mark('28')
    # tasks.deploy.quietMinutes of each other deploy the newest commit once; a deploy by hand
    # runs at once (docs/tasks.md "Deploy and release").
    configure(LAND_CONFIG + """  deploy:
    command: 'echo "$WSX_DEPLOY_COMMIT" >> ../auto-deploy.log'
    auto: true
    quietMinutes: 0.5
""", 'deploy after land')
    # ^ A deploy is due while the default branch is ahead of the last deploy: the part starts level
    # with it, so a manager host that starts in the window starts no waiter of its own.
    level = len(deploy_records())
    js('task', 'deploy')
    wait(lambda: len(deploy_records()) > level and deploy_lock_free(), 'the deploy by hand that levels the part', 30)
    (base / 'auto-deploy.log').unlink()
    quick = [js('task', 'new', 'WORK d%d.txt' % i)['id'] for i in range(4)]
    for n in quick:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
    records_before = len(deploy_records())
    tips = []
    for n in quick[:3]:
        land(n)
        tips.append(wait(lambda: show(n)['state'] == 'landed' and git('rev-parse', 'main'), 'quick land'))
    deploy_log = base / 'auto-deploy.log'
    time.sleep(2)
    assert not deploy_log.exists() and len(deploy_records()) == records_before, 'a deploy ran inside the quiet window'
    # ^ The waiters of the first two lands find the branch moved and leave the deploy to the last.
    wait(lambda: deploy_log.exists() and deploy_lock_free(), 'one deploy of the newest land', 90)
    time.sleep(1)
    assert deploy_log.read_text().split() == [tips[-1]], (tips, deploy_log.read_text())
    assert [r['commit'] for r in deploy_records()[records_before:]] == [tips[-1]], deploy_records()[records_before:]
    assert js('task', 'deployed')['deploy']['commit'] == tips[-1]
    # ^ A deploy's event names the tasks it put live: those landed since the previous deploy
    # (docs/tasks.md "Deploy and release"), so no one reads `deploys` to say what is live.
    # The first deploy of the part names every earlier land too: those before it deployed none.
    newest_deploy = wait(lambda: events_with('deploy of %s succeeded after' % tips[-1][:12]), 'the deploy event', 30)[0]['text']
    assert re.search(r'deploy of %s succeeded after \d+ s: (#\d+, )*#%d, #%d, #%d live(\n|$)' % (tips[-1][:12], quick[0], quick[1], quick[2]), newest_deploy), newest_deploy
    # ^ A land after the window deploys again: 4 lands, 2 deploys. A deploy by hand in the next
    # window runs at once, and the land's waiter finds its commit deployed.
    land(quick[3])
    last = wait(lambda: show(quick[3])['state'] == 'landed' and git('rev-parse', 'main'), 'the later land')
    # ^ `ran` is a local of this part below: the by-hand run is spelled out.
    by_hand = len(deploy_records())
    assert js('task', 'deploy')['commit'] == last
    wait(lambda: len(deploy_records()) > by_hand and deploy_lock_free(), 'the deploy by hand', 30)
    assert [r['commit'] for r in deploy_records()[records_before:]] == [tips[-1], last]
    for n in quick:
        wait_closed(n)
    follow_up = state / 'wsx/tasks' / Path(created['task_dir']).parent.name / 'follow-up.log'
    wait(lambda: 'already deployed' in follow_up.read_text() and deploy_lock_free(), 'the waiter that found the land deployed', 60)
    assert len(deploy_records()[records_before:]) == 2, deploy_records()[records_before:]
    # ^ A waiter killed in its window (a reboot does the same) leaves the deploy due: a manager host
    # that starts starts the waiter again, and the deploy runs once.
    lost = js('task', 'new', 'WORK d4.txt')['id']
    assert wait_state(lost, 'ready', 'failed')['state'] == 'ready'
    land(lost)
    lost_tip = wait(lambda: show(lost)['state'] == 'landed' and git('rev-parse', 'main'), 'the land whose waiter dies')
    waiters = lambda: leftovers('task deploy -p demo --if-new --quiet')
    for line in wait(waiters, 'the waiter of the land'):
        os.kill(int(line.split()[0]), 9)
    wait(lambda: not waiters(), 'the waiter killed')
    records_before = len(deploy_records())
    stop_manager_now()
    assert js('task', 'manager')['state'] == 'started'
    wait(waiters, 'the waiter the starting manager host started')
    wait(lambda: len(deploy_records()) > records_before and deploy_lock_free(), 'the deploy after the restart', 90)
    time.sleep(2)
    assert [r['commit'] for r in deploy_records()[records_before:]] == [lost_tip], deploy_records()[records_before:]
    wait_closed(lost)

    # 29. A preview on request (docs/tasks.md "Preview"): a fake tailscale records its argv.
    mark('29')
    tailscale_log = work / 'tailscale.log'
    (bin_dir / 'tailscale').write_text('#!/bin/sh\necho "$@" >> ' + str(tailscale_log) + '\n'
                                       'if [ "$1" = status ]; then echo \'{"Self":{"DNSName":"host.example.ts.net."}}\'; fi\n')
    (bin_dir / 'tailscale').chmod(0o755)
    preview_config = LAND_CONFIG + """  preview:
    command: 'exec python3 -m http.server "$PORT" --bind 127.0.0.1'
    ports: 9150-9199
"""
    configure(preview_config, 'preview')
    pv = js('task', 'new', 'WORK pv.txt')['id']
    assert wait_state(pv, 'ready', 'failed')['state'] == 'ready'

    def answers(port, name='pv.txt'):
        try:
            with urllib.request.urlopen('http://127.0.0.1:%d/%s' % (port, name), timeout=3) as response:
                return response.status == 200
        except OSError:
            return False

    running_preview = js('task', 'preview', pv)
    port = running_preview['port']
    assert running_preview['url'] == 'https://host.example.ts.net:%d/' % port and 9150 <= port <= 9199, running_preview
    assert answers(port) and ('serve --bg --https=%d http://127.0.0.1:%d' % (port, port)) in tailscale_log.read_text()
    assert js('task', 'preview', pv)['port'] == port and any(e['kind'] == 'preview' for e in log(pv))
    assert js('task', 'preview', pv, '--stop')['stopped'] is True
    assert not answers(port) and ('serve --bg --https=%d off' % port) in tailscale_log.read_text()
    assert 'reset' not in tailscale_log.read_text()
    # Closing the task stops a running preview.
    again = js('task', 'preview', pv)['port']
    js('task', 'close', pv, '--discard')
    assert not answers(again) and tailscale_log.read_text().count('--https=%d off' % again) >= 1
    # `local: true`, and a machine without tailscale: the url is local and tailscale never runs.
    calls = tailscale_log.read_text()
    configure(preview_config + "    local: true\n", 'local preview')
    lp = js('task', 'new', 'WORK lp.txt')['id']
    assert wait_state(lp, 'ready', 'failed')['state'] == 'ready'
    local = js('task', 'preview', lp)
    assert local['url'] == 'http://127.0.0.1:%d/' % local['port'] and answers(local['port'], 'lp.txt'), local
    assert js('task', 'preview', lp, '--stop')['stopped'] is True and not answers(local['port'], 'lp.txt')
    (bin_dir / 'tailscale').unlink()
    configure(preview_config, 'preview without tailscale')
    absent = js('task', 'preview', lp)
    assert absent['url'] == 'http://127.0.0.1:%d/' % absent['port'] and answers(absent['port'], 'lp.txt'), absent
    js('task', 'close', lp, '--discard')
    assert not answers(absent['port'], 'lp.txt') and tailscale_log.read_text() == calls
    configure(LAND_CONFIG, 'no preview')
    plain = js('task', 'new', 'WORK plain.txt')['id']
    assert wait_state(plain, 'ready', 'failed')['state'] == 'ready'
    assert 'configures no tasks.preview' in cli('task', 'preview', plain, ok=False).stderr
    js('task', 'close', plain, '--discard')

    # 30. By hand (docs/tasks.md "By hand"): the owner takes a running build over in its own
    mark('30')
    # working copy with a fork of the worker's conversation; nothing else writes there until
    # handback, whose commits are checked and, once reviewed, land and close the task.
    configure(LAND_CONFIG.replace('  land: { auto: false }\n', ''), 'auto land for hand')
    gate = work / 'hand-gate'
    handed = js('task', 'new', 'WAIT %s WORK late.txt' % gate)['id']
    hand_copy = Path(show(handed)['record']['working_copy'])
    worker_session = wait(lambda: show(handed)['record']['agent_session'], 'worker session')
    wait(lambda: any(t.get('message', '').startswith('WAIT') for t in traces()), 'the worker turn runs')
    assert 'only a task worked by hand' in cli('task', 'hand', handed, '--back', '--json', ok=False).stderr
    queued = js('task', 'add', handed, 'WORK queued.txt')
    assert 'queued' in queued['message'], queued
    for argv in (('hand',), ('hand', '--back'), ('handback',)):
        refused = subprocess.run([str(wsx), 'task', argv[0], str(handed), *argv[1:]], cwd=project, env=manager_env,
                                 capture_output=True, text=True)
        assert refused.returncode != 0 and 'for the owner' in refused.stderr, refused.stderr
    worker_transcript = home / '.claude/fake-sessions' / ('%s.jsonl' % worker_session)
    before = hashlib.sha256(worker_transcript.read_bytes()).hexdigest()
    taken = js('task', 'hand', handed)
    assert taken['ok'] and taken['state'] == 'waiting' and taken['session'] == 'task-%d-hand' % handed, taken
    assert 'dropped 1 queued' in taken['message'], taken
    context = taken['context']
    assert context['branch'] == 'wsx/t%d' % handed and Path(context['working_copy']) == hand_copy, context
    assert context['head'] == git('rev-parse', 'HEAD', cwd=hand_copy) and context['status'] == 'clean', context
    assert [e['dropped'] for e in log(handed) if e['kind'] == 'hand'] == [1]
    pane = wait(lambda: next((t for t in traces() if t.get('interactive') and t['resume'] == worker_session), None),
                'the hand pane resumes the worker conversation')
    assert pane['argv'] == ['--resume', worker_session, '--fork-session', '--system-prompt-snapshot', 'off'], pane
    assert Path(pane['cwd']).resolve() == hand_copy.resolve(), pane
    # ^ The owner's pane resumes the worker's conversation under the same account.
    assert pane['claude_config_dir'] == str(profile_root), pane

    def hand_sessions():
        return [x for x in daemon_call('snapshot')['data']['sessions'] if x['label'] == 'task-%d-hand' % handed]

    assert len(hand_sessions()) == 1
    # ^ A second hand is refused and opens no second pane.
    assert 'works on it by hand' in cli('task', 'hand', handed, '--json', ok=False).stderr and len(hand_sessions()) == 1
    assert show(handed)['hand']['session'] == 'task-%d-hand' % handed
    # One worktree: the task's own copy on its own branch.
    listed = git('worktree', 'list', '--porcelain').splitlines()
    assert listed.count('branch refs/heads/wsx/t%d' % handed) == 1, listed
    # Nothing starts while by hand: messages and land are refused, the old turn's gate
    # opens on no one, and no check runs.
    gate.write_text('open')
    for argv, word in ((('add', handed, 'WORK no.txt'), '--back'), (('steer', handed, 'x'), '--back'),
                       (('land', handed), '--back')):
        refused = cli('task', *argv, '--json', ok=False)
        assert word in refused.stdout + refused.stderr, refused
    time.sleep(1.5)
    assert show(handed)['record']['waiting']['on'] == 'hand'
    assert not kinds(handed, 'checks', 'revision', 'result'), log(handed)
    assert not any(t.get('message') in ('WORK no.txt', 'WORK queued.txt') for t in traces())
    assert not (hand_copy / 'late.txt').exists()
    # Uncommitted tracked work refuses the handback, and the pane stays.
    (hand_copy / 'README').write_text('by hand\n')
    refused = cli('task', 'hand', handed, '--back', '--json', ok=False)
    assert 'uncommitted changes: README' in refused.stderr, refused.stderr
    assert show(handed)['record']['waiting']['on'] == 'hand' and len(hand_sessions()) == 1
    (hand_copy / 'hand.txt').write_text('made by hand\n')
    git('add', 'README', 'hand.txt', cwd=hand_copy)
    git('commit', '-q', '-m', 'by hand', cwd=hand_copy)
    back = js('task', 'handback', handed)  # the hidden old name of `hand --back`
    assert back['ok'] and len(back['sessions_closed']) == 1 and '1 commit' in back['message'], back
    assert not hand_sessions()
    assert [e['commits'] for e in log(handed) if e['kind'] == 'handback'] == [1]
    assert reviewed(handed)['state'] == 'landed'
    wait_closed(handed)
    assert main_has('hand.txt') and git('show', 'main:README') == 'by hand'
    assert kinds(handed, 'checks', 'revision') == ['checks', 'revision']
    # ^ The fork wrote its own transcript; the worker's is as it was.
    assert hashlib.sha256(worker_transcript.read_bytes()).hexdigest() == before
    assert any(path.name.startswith('fork-') for path in (home / '.claude/fake-sessions').iterdir())
    configure(LAND_CONFIG, 'land by hand again')

    # 31. `wsx worktree delete` refuses uncommitted work before it closes anything, ignores
    mark('31')
    # build output, and keeps a branch with unmerged commits.
    cli('worktree', 'create', 'feat/keep')
    keep = Path(next(line.split(' ', 1)[1] for line in git('worktree', 'list', '--porcelain').splitlines()
                     if line.startswith('worktree ') and line.endswith('-feat-keep')))
    (keep / 'target').mkdir()
    (keep / 'target/build').write_text('ignored output')

    def keep_sessions():
        snapshot = daemon_call('snapshot')['data']
        ids = [w['id'] for w in snapshot['worktrees'] if Path(w['path']) == keep]
        return [x for x in snapshot['sessions'] if x['worktree_id'] in ids]

    wait(lambda: keep_sessions(), 'the worktree session')
    (keep / 'README').write_text('changed\n')
    refused = cli('worktree', 'delete', 'feat/keep', ok=False)
    assert 'uncommitted changes: README' in refused.stderr and 'Nothing was removed' in refused.stderr, refused.stderr
    assert keep.exists() and keep_sessions()
    git('checkout', '--', 'README', cwd=keep)
    (keep / 'notes.txt').write_text('draft\n')
    assert 'untracked files: notes.txt' in cli('worktree', 'delete', 'feat/keep', ok=False).stderr
    assert keep.exists() and keep_sessions()
    git('add', 'notes.txt', cwd=keep)
    git('commit', '-q', '-m', 'unmerged', cwd=keep)
    deleted = cli('worktree', 'delete', 'feat/keep')
    assert 'kept branch feat/keep' in deleted.stdout, deleted.stdout
    assert not keep.exists() and not keep_sessions() and git('branch', '--list', 'feat/keep')
    git('branch', '-D', 'feat/keep')

    # 32. Hold and trial (docs/tasks.md "Hold and trial"): a held task waits for the owner while
    mark('32')
    # pipeline work lands; its trial puts the rebased revision on the live site, holds the
    # default branch's deploys, and ends on Pass (it lands), End, or a message to the worker.
    configure(LAND_CONFIG.replace('  land: { auto: false }\n', '') + """  deploy:
    command: 'echo "$WSX_DEPLOY_COMMIT" >> ../trial-deploy.log'
    auto: true
    quietMinutes: 0
  release:
    command: 'echo released >> ../trial-release.log'
""", 'hold and trial')
    live_log = base / 'trial-deploy.log'

    def live():
        return live_log.read_text().split()[-1] if live_log.exists() else None

    def trial():
        return js('task', 'deployed')['trial']

    held = js('task', 'new', '--hold', 'try the held page', 'WORK held.txt HV')
    assert held['hold'] == 'try the held page', held
    held = held['id']
    assert wait_state(held, 'ready', 'failed')['state'] == 'ready'
    time.sleep(1.5)
    assert show(held)['state'] == 'ready' and not main_has('held.txt'), 'a held task landed by itself'
    assert [e['reason'] for e in log(held) if e['kind'] == 'hold'] == ['try the held page']
    assert next(r for r in js('task', 'ledger')['open'] if r['id'] == held)['hold'] == 'try the held page'
    first = js('task', 'new', 'WORK pipe1.txt')['id']
    reviewed(first)
    wait_closed(first)
    wait(lambda: live() == git('rev-parse', 'main'), 'the pipeline land deploys', 60)
    # The trial rebases onto the newest main and checks it before it goes live.
    started = js('task', 'trial', held)
    assert started['ok'] and 'rebased onto main' in started['message'], started
    wait(lambda: (trial() or {}).get('stage') == 'live', 'the trial goes live', 60)
    tried = trial()
    assert tried['task'] == held and live() == tried['commit'], (tried, live())
    assert subprocess.run(['git', 'merge-base', '--is-ancestor', 'main', tried['commit']], cwd=project, env=env).returncode == 0
    assert [e['commit'] for e in log(held) if e['kind'] == 'trial'] == [tried['commit']]
    assert 'already runs' in js('task', 'trial', held)['message']
    # Pipeline work lands meanwhile; its deploy waits, and a deploy by hand is refused.
    second = js('task', 'new', 'WORK pipe2.txt')['id']
    reviewed(second)
    wait_closed(second)
    time.sleep(2)
    assert live() == tried['commit'] and main_has('pipe2.txt'), live()
    assert 'runs the trial of #%d' % held in follow_up.read_text()
    assert 'end it first' in cli('task', 'deploy', ok=False).stderr
    # ^ A release also waits for open To check items, which it checks first.
    released = cli('task', 'release', ok=False).stderr
    assert 'end it first' in released or 'to check are open' in released, released
    assert not (base / 'trial-release.log').exists()
    other_held = js('task', 'new', '--hold', 'second', 'WORK held2.txt')['id']
    assert wait_state(other_held, 'ready', 'failed')['state'] == 'ready'
    assert 'runs the trial of #%d' % held in cli('task', 'trial', other_held, ok=False).stderr
    # Only the owner passes a trial; Pass lands it without To check items and deploys main.
    refused = subprocess.run([str(wsx), 'task', 'land', str(held), '--tried'], cwd=project, env=manager_env,
                             capture_output=True, text=True)
    assert refused.returncode != 0 and 'only the owner' in refused.stderr, refused.stderr
    assert 'no trial of t%d runs' % other_held in cli('task', 'land', other_held, '--tried', ok=False).stdout
    land(held, '--tried')
    wait_closed(held)
    assert main_has('held.txt') and trial() is None
    assert not [item for item in js('task', 'check')['open'] if item['task'] == held]
    wait(lambda: live() == git('rev-parse', 'main'), 'main deploys after the pass', 60)
    assert [e['reason'] for e in log(held) if e['kind'] == 'trial_ended'] == ['the owner passed it and it landed']
    # End frees the live site; a message to the worker ends the trial too.
    for ending in ('end', 'add'):
        js('task', 'trial', other_held)
        wait(lambda: (trial() or {}).get('stage') == 'live', 'the second trial goes live', 60)
        assert live() != git('rev-parse', 'main')
        if ending == 'end':
            assert js('task', 'trial', other_held, '--end')['ended'] is True
        else:
            js('task', 'add', other_held, 'WORK more.txt')
        assert trial() is None
        wait(lambda: live() == git('rev-parse', 'main'), 'main deploys after the trial ends', 60)
        if ending == 'add':
            assert wait_state(other_held, 'ready', 'failed')['state'] == 'ready'
    assert [e['reason'] for e in log(other_held) if e['kind'] == 'trial_ended'] == [
        'ended without a decision', 'a message went to the worker']
    js('task', 'hold', other_held, '--off')
    assert 'hold' not in show(other_held)['record']
    js('task', 'close', other_held, '--discard')
    configure(LAND_CONFIG, 'no trial')

    # 32b. A wsx older than its config (docs/tasks.md "Config"): unknown keys at any level are
    mark('32b')
    # ignored with one warning that names them, so the ledger, a new task and its land work.
    configure(LAND_CONFIG + '  futureSetting: { on: true }\nfutureTop: 1\n', 'a newer config')
    warned = cli('task', 'ledger', '--json')
    assert 'unknown keys ignored: tasks.futureSetting, futureTop' in warned.stderr and 'wsx.config.yml' in warned.stderr, warned.stderr
    # ^ The TUI shows it as the project notice, not on stderr over its screen.
    notice = tui_text(wsx, env, project, r'futureTop')
    assert 'unknown keys ignored' in notice and 'tasks.futureSetting' in notice, notice[-3000:]
    # ^ A background config read's stderr warning goes to ui-stderr.log, not over the TUI's cells.
    stderr_log = (home / 'Library/Caches' if sys.platform == 'darwin' else work / 'k') / 'wsx/ui-stderr.log'
    wait(lambda: stderr_log.exists() and 'futureTop' in stderr_log.read_text(), 'the TUI keeps its stderr in ui-stderr.log', 10)
    assert warned.stderr.count('warning:') == 1, warned.stderr
    newer = cli('task', 'new', 'WORK newer.txt', '--json')
    assert 'tasks.futureSetting' in newer.stderr, newer.stderr
    newer = json.loads(newer.stdout)['id']
    assert wait_state(newer, 'ready', 'failed')['state'] == 'ready'
    assert land(newer)['ok'] and main_has('newer.txt')
    wait_closed(newer)
    configure(LAND_CONFIG, 'config as before')
    # 32c. The check slot (docs/tasks.md "Checks, revisions and report"): one check run at a
    mark('32c')
    # time on the machine. The run that waits shows queued; a killed holder frees the slot.

    def queued_row(pair):
        rows = [r for r in js('task', 'ledger')['open'] if r['id'] in pair]
        return next((r for r in rows if r['state'] == 'checking' and (r['waiting'] or {}).get('on') == 'check_slot'), None)

    slot_config(3)
    pair = [js('task', 'new', 'WORK slot%d.txt' % i)['id'] for i in (1, 2)]
    queued = wait(lambda: queued_row(pair), 'a queued check run', 30)
    assert queued['waiting']['reason'].startswith('queued for the check slot behind demo t'), queued
    assert queued['status']['label'] == 'Waiting for the check slot', queued['status']
    assert re.match(r'Waiting for the check slot for [^:]+: #\d+ runs its checks now(, \d+ more tasks? ahead)?\. Its checks start when the slot is free\.$', queued['status']['sentence']), queued['status']
    for n in pair:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
        assert 'waiting' not in show(n)['record']
    assert [e['reason'] for e in log(queued['id']) if e['kind'] == 'check_queued'], log(queued['id'])
    first, second = sorted((slot_marks()[copy_name(n)] for n in pair), key=lambda span: span['start'])
    assert first['end'] <= second['start'], (first, second)
    for n in pair:
        js('task', 'close', n, '--discard')
    slot_spans.unlink()
    slot_config(60)
    pair = [js('task', 'new', 'WORK slot%d.txt' % i)['id'] for i in (3, 4)]
    holder_copy = wait(lambda: next(iter(slot_marks()), None), 'the first check run starts', 30)
    holder = next(n for n in pair if copy_name(n) == holder_copy)
    waiter = next(n for n in pair if n != holder)
    assert wait(lambda: queued_row(pair), 'the second run queued', 30)['id'] == waiter
    project_id = show(holder)['record']['project_id']
    host_pid = next(int(line.split()[0]) for line in leftovers('task host %s %d' % (project_id, holder))
                    if line.endswith('task host %s %d' % (project_id, holder)))
    os.kill(host_pid, 9)
    killed = time.time()
    started = wait(lambda: slot_marks().get(copy_name(waiter), {}).get('start'), 'the next run after the holder died', 15)
    assert started - killed < 5 and 'end' not in slot_marks()[holder_copy], slot_marks()
    release.touch()
    assert wait_state(waiter, 'ready', 'failed')['state'] == 'ready'
    # ^ The killed holder, restarted, runs its checks again instead of waiting for the owner.
    assert show(holder)['state'] == 'checking'
    cli('session', 'restart', 'task-%d' % holder)
    again = wait_state(holder, 'ready', 'failed', 'waiting')
    assert again['state'] == 'ready', again
    for n in pair:
        js('task', 'close', n, '--discard')
    # A worker's check-run that must wait for the slot answers `queued` at once, also to a
    # client that asks again, so no turn waits or polls (docs/tasks.md "Check-run"): the step
    # ends with its result, the answer comes as one `check_run` turn of that step, and the next
    # step follows. The checks run once, never beside each other.
    slot_spans.unlink()
    release.unlink()
    configure(LAND_CONFIG.replace('"test ! -e broken.txt"', '"%s %s 60"' % (sys.executable, slot_check))
              + '  workflow:\n    steps:\n      - {name: build, prompt: build it}\n'
              '      - {name: review, prompt: look}\n', 'slot checks in steps')
    holder = js('task', 'new', 'WORK slot5.txt')['id']
    wait(lambda: copy_name(holder) in slot_marks(), 'the holder runs its checks', 30)
    again = js('task', 'new', 'CHECKRUN_AGAIN WORK slot6.txt')['id']
    wait(lambda: [e for e in log(again) if e['kind'] == 'check_queued'], 'the check-run queued', 30)
    code, said = wait(lambda: next((t for t in traces() if 'check_run_again' in t), None),
                      'the queued answer', 30)['check_run_again']
    assert code == 0 and "queued for the machine's check slot" in said, said
    # ^ The step's result comes while the run still waits: it waits too, the step stays.
    wait(lambda: kinds(again, 'result'), 'the step result before the answer', 30)
    assert show(again)['record']['step'] == 0 and not kinds(again, 'check_run'), log(again)
    release.touch()
    assert wait_state(again, 'ready', 'failed')['state'] == 'ready', log(again)
    assert kinds(again, 'result', 'check_run', 'checks') == ['result', 'check_run', 'result', 'result', 'checks'], log(again)
    turns = [(e['step'], e['trigger']) for e in log(again) if e['kind'] == 'result']
    assert turns == [('build', 'step'), ('build', 'check_run'), ('review', 'step')], turns
    answers = [t['message'] for t in traces() if t.get('message', '').startswith('Your check-run ended. Its answer')]
    assert len(answers) == 1 and ': exit 0' in answers[0], answers
    # ^ One start for the check-run and one for the checks after the result.
    starts = [line for line in slot_spans.read_text().splitlines() if line.startswith(copy_name(again) + ' start')]
    assert len(starts) == 2, slot_spans.read_text()
    for n in (holder, again):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no slot checks')

    # 32b. Checks run once per tree (docs/tasks.md "Checks, revisions and report", "Landing"): a
    # repeat check-run on the same clean tree runs nothing, a land with an unmoved main runs no
    # checks, and a land after main moved runs them on the rebased revision.
    once = js('task', 'new', 'CHECKRUN_TWICE WORK once.txt')['id']
    assert wait_state(once, 'ready', 'failed')['state'] == 'ready'
    first, second = next(t for t in traces() if 'check_run_twice' in t)['check_run_twice']
    assert first[0] == 0 and 'stored' not in first[1], first
    assert second[0] == 0 and 'check 1 `test ! -e broken.txt`: exit 0 (stored: it ran on this tree before)' in second[1], second
    assert kinds(once, 'check_run') == ['check_run'], log(once)
    ran = len(kinds(once, 'checks'))
    assert land(once)['state'] == 'landed' and len(kinds(once, 'checks')) == ran, log(once)
    wait_closed(once)
    moved = js('task', 'new', 'WORK moved.txt')['id']
    assert wait_state(moved, 'ready', 'failed')['state'] == 'ready'
    ran = len(kinds(moved, 'checks'))
    # ^ A host that started again at ready has no worker result in memory: the checks of the
    # land's rebase read the stored one, so the passed checks keep `done` and the task lands.
    moved_id = show(moved)['record']['project_id']
    os.kill(next(int(line.split()[0]) for line in leftovers('task host %s %d' % (moved_id, moved))
                 if line.endswith('task host %s %d' % (moved_id, moved))), 9)

    def restart_moved():
        try:
            return cli('session', 'restart', 'task-%d' % moved)
        except AssertionError:
            return None
    wait(restart_moved, 'the ready host restarts')
    (project / 'main-moved.txt').write_text('main moved\n')
    git('add', 'main-moved.txt')
    git('commit', '-q', '-m', 'main moves')
    assert 'checks run on the new revision' in land(moved)['message']
    assert wait_state(moved, 'landed', 'failed')['state'] == 'landed' and len(kinds(moved, 'checks')) == ran + 1, log(moved)
    assert not kinds(moved, 'land_stopped') and json.loads((Path(show(moved)['report']).parent / 'result.json').read_text())['result']['status'] == 'done', log(moved)
    wait_closed(moved)
    # 32b1. A check that exits 75 (EX_TEMPFAIL, a failure outside the tree such as the CI runner's
    # network) is never kept (docs/tasks.md "Check-run"): the next check-run on the same clean tree
    # runs it again, while a check that exits 1 is answered from its kept result.
    mark('32b1')
    temp_runs, hard_runs = base / 'tempfail.log', base / 'hardfail.log'
    configure(LAND_CONFIG + '  scenarios:\n    tempfail: "echo run >> %s; exit 75"\n    hardfail: "echo run >> %s; exit 1"\n'
              % (temp_runs, hard_runs), 'tempfail scenarios')
    flaky = js('task', 'new', 'CHECKRUN_REPEAT tempfail')['id']
    hard = js('task', 'new', 'CHECKRUN_REPEAT hardfail')['id']
    for n in (flaky, hard):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    first, second = wait(lambda: next((t for t in traces() if 'check_run_repeat_tempfail' in t), None),
                         'the tempfail answers', 60)['check_run_repeat_tempfail']
    assert first[0] == 1 and second[0] == 1 and 'stored' not in first[1] + second[1], (first, second)
    assert 'exit 75 (a temporary failure, not kept: the next check-run runs it again)' in second[1], second
    assert temp_runs.read_text().count('run') == 2, temp_runs.read_text()
    first, second = wait(lambda: next((t for t in traces() if 'check_run_repeat_hardfail' in t), None),
                         'the hardfail answers', 60)['check_run_repeat_hardfail']
    assert first[0] == 1 and 'exit 1' in first[1] and 'stored' not in first[1], first
    assert second[0] == 1 and '(stored: it ran on this tree before)' in second[1], second
    assert hard_runs.read_text().count('run') == 1, hard_runs.read_text()
    for n in (flaky, hard):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no tempfail scenarios')
    # 32w. Each check's result has its run time and its run's wait for the check slot (ms), in
    # show and the ledger; the checks after a result answer from a check-run on the same clean
    # tree and do not run again (docs/tasks.md "Checks, revisions and report").
    mark('32w')
    configure(LAND_CONFIG.replace('  checks:\n', '  checks:\n    - "sleep 1"\n'), 'timed checks')
    timed = js('task', 'new', 'WORK timed.txt CHECKRUN_AFTER')['id']
    assert wait_state(timed, 'ready', 'failed')['state'] == 'ready', log(timed)
    code, said = next(t for t in traces() if 'check_run_after' in t)['check_run_after']
    assert code == 0 and 'stored' not in said, said
    ran = [e for e in log(timed) if e['kind'] == 'check_run'][-1]['results']
    assert ran[0]['ms'] >= 1000 and all(isinstance(c['queued_ms'], int) for c in ran), ran
    gate = [e for e in log(timed) if e['kind'] == 'checks'][-1]['results']
    assert [c.get('stored') for c in gate] == [True, True] and gate[0]['ms'] == ran[0]['ms'], gate
    assert [c['ms'] for c in show(timed)['result']['checks']] == [c['ms'] for c in gate]
    row = next(t for t in js('task', 'ledger')['open'] if t['id'] == timed)
    assert row['checks'][0]['ms'] >= 1000 and 'queued_ms' in row['checks'][0], row
    js('task', 'close', timed, '--discard')
    configure(LAND_CONFIG, 'untimed checks')
    # 32x. A land tells each open task that shares its files to rebase: the message names the
    # landed commit and the files and joins a turn that starts anyway; it never starts one: a
    # worker that ends its turn gets no turn for it, and a task with no turn to come gets
    # nothing, as its own land rebases it (docs/tasks.md "Landing").
    mark('32x')
    notice_gate = base / 'notice-gate'
    carry_gate = base / 'carry-gate'
    busy = js('task', 'new', 'WAIT %s' % notice_gate, '--files', 'shared.txt')['id']
    carry = js('task', 'new', 'WAIT %s' % carry_gate, '--files', 'shared.txt')['id']
    idle = js('task', 'new', 'WORK idle.txt', '--files', 'shared.txt')['id']
    lander = js('task', 'new', 'WORK shared.txt')['id']
    for n in (idle, lander):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    for n in (busy, carry):
        wait(lambda: show(n)['record']['agent_session'], 't%s works' % n)
    assert land(lander)['state'] == 'landed'
    tip = git('rev-parse', 'main')
    heard = lambda n: [e['text'] for e in log(n) if e['kind'] == 'message' and 'landed on main' in e['text']]
    wait(lambda: heard(busy), 't%s hears the land' % busy)
    assert tip in heard(busy)[0] and 'shared.txt' in heard(busy)[0] and 'Run no `wsx task check-run`' in heard(busy)[0], heard(busy)
    assert not heard(idle) and kinds(idle, 'result') == ['result'], log(idle)
    wait(lambda: heard(carry), 't%s hears the land' % carry)
    # ^ An add queued in the turn starts the next one at its end: the notice rides in it.
    assert js('task', 'add', carry, 'WORK carried.txt')['ok']
    carry_gate.write_text('go')
    wait(lambda: any(tip in t.get('message', '') and 'WORK carried.txt' in t.get('message', '') for t in traces()),
         'the notice joins the add turn', 30)
    assert wait_state(carry, 'ready', 'failed')['state'] == 'ready', log(carry)
    # ^ A worker whose turn ends gets no turn for the notice: one result, the land rebases it.
    notice_gate.write_text('go')
    assert wait_state(busy, 'ready', 'failed')['state'] == 'ready', log(busy)
    time.sleep(1)
    assert kinds(busy, 'result') == ['result'], log(busy)
    assert sum(tip in t.get('message', '') for t in traces()) == 1, [t.get('message') for t in traces()]
    for n in (busy, carry, idle):
        js('task', 'close', n, '--discard')
    wait_closed(lander)
    # 32y. A batch land: ready tasks stack on main in order, the checks run once on the
    # combined tree and each lands without checks of its own. A batch whose checks fail splits
    # until the failing task lands alone through its own checks (docs/tasks.md "Batch land").
    mark('32y')
    batch = [js('task', 'new', 'WORK %s.txt' % name)['id'] for name in ('b1', 'b2', 'b3')]
    for n in batch:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    ran = {n: len(kinds(n, 'checks')) for n in batch}
    ended = follow_up.read_text().count('batch land ended')
    started = js('task', 'land', *map(str, batch))
    assert started['ok'] and 'batch land of #%d, #%d, #%d started' % tuple(batch) in started['message'], started
    for n in batch:
        wait(lambda: show(n)['state'] == 'landed', 't%s lands in the batch' % n, 60)
        assert len(kinds(n, 'checks')) == ran[n], log(n)
    assert all(main_has('%s.txt' % name) for name in ('b1', 'b2', 'b3'))
    wait(lambda: follow_up.read_text().count('batch land ended') == ended + 1, 'the batch ends', 60)
    assert 'and 1 batch check run(s)' in follow_up.read_text().rsplit('batch land ended', 1)[-1], follow_up.read_text()[-2000:]
    assert not any(Path(p).name.startswith(project.name + '-batch-') for p in base.iterdir())
    for n in batch:
        wait_closed(n)
    refused = cli('task', 'land', batch[0], batch[1], ok=False)
    assert 'is landed: a batch lands only ready tasks' in refused.stderr, refused.stderr
    configure(LAND_CONFIG.replace('  checks:\n', '  checks:\n    - "! test -e s1.txt -a -e s2.txt"\n'), 'no s1 with s2')
    split = [js('task', 'new', 'WORK %s.txt' % name)['id'] for name in ('g1', 's1', 's2')]
    for n in split:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    assert js('task', 'land', *map(str, split))['ok']
    g1, s1, s2 = split
    for n in (g1, s1):
        wait(lambda: show(n)['state'] == 'landed', 't%s lands' % n, 60)
    wait(lambda: show(s2)['state'] == 'failed', 't%s fails alone' % s2, 60)
    assert any(e['kind'] == 'land_stopped' for e in log(s2)) and not main_has('s2.txt'), log(s2)
    wait(lambda: follow_up.read_text().count('batch land ended') == ended + 2, 'the split batch ends', 60)
    tail = follow_up.read_text().rsplit('the checks failed on the batch', 1)
    assert len(tail) == 2 and 'and 2 batch check run(s)' in tail[-1], follow_up.read_text()[-3000:]
    for n in (g1, s1):
        wait_closed(n)
    js('task', 'close', s2, '--discard')
    configure(LAND_CONFIG, 'no batch check')
    # 32s. A scenario runs through check-run by name or by number after the checks, never in a
    # bare check-run or after a result; the worker's instructions list it.
    mark('32s')
    scenario_log = base / 'scenario.log'
    configure(LAND_CONFIG + '  scenarios:\n    probe: echo scenario-ran >> %s\n' % scenario_log, 'a scenario')
    scenario = js('task', 'new', 'CHECKRUN_WITH probe WORK scenario.txt')['id']
    assert wait_state(scenario, 'ready', 'failed')['state'] == 'ready'
    named, bare = next(t for t in traces() if 'check_run_with' in t)['check_run_with']
    assert named[0] == 0 and 'check 2 `echo scenario-ran' in named[1] and 'check 1' not in named[1], named
    assert bare[0] == 0 and 'check 1 `test ! -e broken.txt`' in bare[1] and 'scenario' not in bare[1], bare
    assert scenario_log.read_text() == 'scenario-ran\n'
    worker_launch = [t for t in traces() if t.get('launch') and not t.get('manager')][-1]
    assert '2. `probe` (`echo scenario-ran' in worker_launch['instructions'], worker_launch['instructions']
    js('task', 'close', scenario, '--discard')
    # ^ A `ci:` scenario (docs/tasks.md "Scenarios") runs its command without the check slot and
    # answers `queued` at once: two tasks run theirs at the same time while a third holds the
    # scenarios slot, and each gets its answer as a turn after its result.
    ci_lane, ci_gate, ci_started = base / 'ci-lane-gate', base / 'ci-gate', base / 'ci-started.log'
    configure(LAND_CONFIG + '  scenarios:\n    slow: %s\n'
              '    remote: "ci: echo started >> %s; %s; echo remote-ran"\n'
              % (gate_cmd(ci_lane), ci_started, gate_cmd(ci_gate)), 'a ci scenario')
    holds = js('task', 'new', 'CHECKRUN_LATE slow WORK cilane.txt')['id']
    wait(lambda: kinds(holds, 'result'), 'the local scenario run that holds the slot', 30)
    remotes = [js('task', 'new', 'CHECKRUN_WITH remote WORK remote%d.txt' % n)['id'] for n in (1, 2)]
    queued = wait(lambda: len(answers := [t['check_run_with'][0] for t in traces() if 'check_run_with' in t
                                          and t['check_run_with'][0][1].startswith('queued on CI')]) == 2 and answers,
                  'both ci check-runs answer queued', 60)
    assert all(code == 0 and 'its answer comes after your result' in said for code, said in queued), queued
    wait(lambda: ci_started.exists() and ci_started.read_text().count('started') == 2, 'both ci scenarios run at once', 30)
    assert not ci_lane.exists() and not ci_gate.exists()
    for n in remotes:
        assert not kinds(n, 'check_queued'), log(n)
    ci_gate.touch()
    for n in remotes:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
        ran = next(e for e in log(n) if e['kind'] == 'check_run')['results']
        assert len(ran) == 1 and ran[0]['exit'] == 0 and ran[0]['queued_ms'] == 0 and 'remote-ran' in ran[0]['output'], ran
        assert ran[0]['command'].startswith('ci: '), ran
    assert len([t for t in traces() if 'Your check-run ended. Its answer' in t.get('message', '') and str(ci_gate) in t['message']]) == 2
    ci_lane.touch()
    assert wait_state(holds, 'ready', 'failed')['state'] == 'ready', log(holds)
    for n in (holds, *remotes):
        js('task', 'close', n, '--discard')
    # 32t. A result that comes before its own check-run's answer waits for it: the worker gets
    # the answer in one turn, and only its next result runs the checks.
    mark('32t')
    late_gate = base / 'late-gate'
    configure(LAND_CONFIG + '  scenarios:\n    slow: %s\n' % gate_cmd(late_gate), 'a slow scenario')
    late = js('task', 'new', 'CHECKRUN_LATE slow WORK late.txt')['id']
    wait(lambda: kinds(late, 'result'), 'the result before the answer', 30)
    time.sleep(1)
    assert show(late)['state'] == 'working' and not kinds(late, 'checks'), log(late)
    late_gate.touch()
    assert wait_state(late, 'ready', 'failed')['state'] == 'ready', log(late)
    assert kinds(late, 'result', 'check_run', 'checks') == ['result', 'check_run', 'result', 'checks'], log(late)
    # ^ 32c's answer turn has the same words: only this task's late gate counts.
    answers = [t['message'] for t in traces() if 'Your check-run ended. Its answer' in t.get('message', '') and str(late_gate) in t['message']]
    assert len(answers) == 1 and 'check 2 `while [ ! -e' in answers[0] and ': exit 0' in answers[0], answers
    js('task', 'close', late, '--discard')
    # 32t1. A stop outlasts a check-run (docs/tasks.md "Host", Stop): its late answer is logged and
    # kept, starts no turn, and joins the turn the next add starts.
    mark('32t1')
    stop_gate = base / 'stop-gate'
    configure(LAND_CONFIG + '  scenarios:\n    slow: %s\n' % gate_cmd(stop_gate), 'a stop scenario')
    parked = js('task', 'new', 'CHECKRUN_LATE slow WORK parked.txt')['id']
    wait(lambda: kinds(parked, 'result'), 'the result before the answer', 30)
    assert js('task', 'stop', parked)['state'] == 'waiting'
    stop_gate.touch()
    wait(lambda: kinds(parked, 'check_run'), 'the check-run ends while stopped', 30)
    time.sleep(3)
    assert show(parked)['state'] == 'waiting' and show(parked)['record']['waiting']['on'] == 'owner', log(parked)
    assert kinds(parked, 'result', 'check_run', 'checks') == ['result', 'check_run'], log(parked)
    answers = lambda: [t['message'] for t in traces() if 'Your check-run ended. Its answer' in t.get('message', '') and str(stop_gate) in t['message']]
    assert not answers(), answers()
    js('task', 'add', parked, 'WORK parked2.txt')
    assert wait_state(parked, 'ready', 'failed')['state'] == 'ready', log(parked)
    assert len(answers()) == 1 and 'WORK parked2.txt' in answers()[0], answers()
    js('task', 'close', parked, '--discard')
    # 32t2. A check-run that ended after the worker's last result and passed on a clean tree
    # starts no turn: the host records it as evidence in the result. A failure starts a turn.
    mark('32t2')
    pass_gate, fail_gate = base / 'pass-gate', base / 'fail-gate'
    configure(LAND_CONFIG + '  scenarios:\n    slow: %s\n'
              '    slowfail: %s; exit 1\n' % (gate_cmd(pass_gate), gate_cmd(fail_gate)), 'late scenarios')
    passed = js('task', 'new', 'CHECKRUN_LATER slow WORK later.txt')['id']
    failed = js('task', 'new', 'CHECKRUN_LATER slowfail WORK laterfail.txt')['id']
    for n in (passed, failed):
        wait(lambda: kinds(n, 'result'), 'the result before the answer', 30)
    time.sleep(1)
    assert not kinds(passed, 'check_run') and not kinds(failed, 'check_run'), (log(passed), log(failed))
    pass_gate.touch()
    fail_gate.touch()
    for n in (passed, failed):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    assert kinds(passed, 'result', 'check_run', 'checks') == ['result', 'check_run', 'checks'], log(passed)
    assert [e['trigger'] for e in log(passed) if e['kind'] == 'result'] == ['step'], log(passed)
    proof = [e for e in show(passed)['result']['result']['evidence'] if 'check-run' in e['check']]
    assert len(proof) == 1 and ': exit 0' in proof[0]['proof'] and str(pass_gate) in proof[0]['proof'], show(passed)['result']
    assert not [t for t in traces() if 'Your check-run ended' in t.get('message', '') and str(pass_gate) in t['message']]
    # ^ The manager hears the pass in the ready event: the worker's own words (written while the
    # run was pending) come after it. The task whose run failed has no such line.
    heard = wait(lambda: events_with('#%d ready r1' % passed, 'check-run slow passed (recorded by the host); worker says'),
                 'the ready event with the recorded check-run', 30)
    assert len(heard) == 1 and len(events_with('#%d ready' % passed)) == 1, manager_entries()[-6:]
    # ^ Events that come together are one message: look at the failed task's own line.
    own = wait(lambda: [line for e in events_with('#%d ready r1' % failed) for line in e['text'].split('\n')
                        if line.startswith('#%d ready r1' % failed)], 'the ready event of the failed run', 30)
    assert 'recorded by the host' not in own[0], own
    assert [e['trigger'] for e in log(failed) if e['kind'] == 'result'] == ['step', 'check_run'], log(failed)
    sent = [t for t in traces() if 'Your check-run ended' in t.get('message', '') and str(fail_gate) in t['message']]
    assert len(sent) == 1 and ': exit 1' in sent[0]['message'], sent
    for n in (passed, failed):
        js('task', 'close', n, '--discard')
    # 32t3. A `ci:` check-run that fails after the result (docs/tasks.md "Check-run"): the
    # `check_run` turn's result goes on to the checks. When that turn starts a new `ci:` run,
    # its result waits for it, and the record shows the wait on `check_run` with the run's time
    # limit, so the stall check does not call it silent; the pass then goes on to the checks.
    mark('32t3')
    ci_mark, ci_pass, ci_go = base / 'ci-again-mark', base / 'ci-again-gate', base / 'ci-fail-gate'
    # ^ The first runs fail only after both results: a run that ends first takes another path.
    hold = gate_cmd(ci_go)
    configure(LAND_CONFIG + '  scenarios:\n    cifail: "ci: %s; exit 1"\n'
              '    ciagain: "ci: if [ -e %s ]; then %s; echo ci-passed; '
              'else %s; touch %s; echo WORK ciagain2.txt CHECKRUN_LATER ciagain ; exit 1; fi"\n'
              % (hold, ci_mark, gate_cmd(ci_pass), hold, ci_mark), 'ci scenarios that fail')
    plain = js('task', 'new', 'CHECKRUN_LATER cifail WORK cifail.txt')['id']
    again = js('task', 'new', 'CHECKRUN_LATER ciagain WORK ciagain.txt')['id']
    wait(lambda: kinds(plain, 'result') and kinds(again, 'result'), 'both first results', 30)
    ci_go.touch()
    assert wait_state(plain, 'ready', 'failed')['state'] == 'ready', log(plain)
    assert kinds(plain, 'result', 'check_run', 'checks') == ['result', 'check_run', 'result', 'checks'], log(plain)
    assert [e['trigger'] for e in log(plain) if e['kind'] == 'result'] == ['step', 'check_run'], log(plain)
    held = wait(lambda: len(kinds(again, 'result')) == 2 and (current := show(again))['record'].get('waiting') and current,
                'the second result waits for its new ci check-run', 30)
    waiting, now_ms = held['record']['waiting'], time.time() * 1000
    assert held['state'] == 'working' and waiting['on'] == 'check_run', held['record']
    assert 'check-run ciagain on CI' in waiting['reason'], waiting
    assert now_ms + 120 * 60_000 < waiting['until_ms'] <= now_ms + 125 * 60_000, waiting
    assert held['status']['kind'] == 'check_run' and held['status']['label'] == 'Waiting for its check-run', held['status']
    assert kinds(again, 'result', 'check_run', 'checks') == ['result', 'check_run', 'result'], log(again)
    ci_pass.touch()
    done = wait_state(again, 'ready', 'failed')
    assert done['state'] == 'ready' and not done['record'].get('waiting'), log(again)
    assert kinds(again, 'result', 'check_run', 'checks') == ['result', 'check_run', 'result', 'check_run', 'checks'], log(again)
    assert [e['trigger'] for e in log(again) if e['kind'] == 'result'] == ['step', 'check_run'], log(again)
    proof = [e for e in done['result']['result']['evidence'] if 'check-run ciagain passed' in e['check']]
    assert len(proof) == 1 and 'ci-passed' in proof[0]['proof'], done['result']
    for n in (plain, again):
        js('task', 'close', n, '--discard')
    # 32t4. A check-run joins a pending run only on the same commit (docs/tasks.md "Check-run"):
    # one on a newer commit answers `queued` and starts when the pending run ends, the held
    # result waits for it, and each answer names its own commit, as the run's own output does.
    mark('32t4')
    two_gate = base / 'two-commits-gate'
    # ^ The run reads HEAD when it starts, as ci-scenario.py does: the gate opens after commit B.
    configure(LAND_CONFIG + '  scenarios:\n    oncommit: "ci: at=$(git rev-parse --short=12 HEAD); '
              '%s; echo ran-on-$at; exit 1"\n' % gate_cmd(two_gate), 'a ci scenario per commit')
    two = js('task', 'new', 'CHECKRUN_LATER oncommit WORK first.txt')['id']
    wait(lambda: kinds(two, 'result'), 'the result before the first answer', 30)
    js('task', 'add', two, 'WORK second.txt CHECKRUN_LATER oncommit')
    held = wait(lambda: len(kinds(two, 'result')) == 2 and (current := show(two))['record'].get('waiting') and current,
                'the result after the second commit waits', 30)
    assert held['record']['waiting']['on'] == 'check_run' and not kinds(two, 'check_run'), log(two)
    copy = held['record']['working_copy']
    first, second = (git('rev-parse', '--short=12', ref, cwd=copy) for ref in ('HEAD~1', 'HEAD'))
    two_gate.touch()
    assert wait_state(two, 'ready', 'failed')['state'] == 'ready', log(two)
    assert kinds(two, 'result', 'check_run', 'checks') == ['result', 'result', 'check_run', 'check_run', 'result', 'checks'], log(two)
    assert [e['trigger'] for e in log(two) if e['kind'] == 'result'] == ['step', 'message', 'check_run'], log(two)
    sent = [t['message'] for t in traces() if 'Your check-run ended' in t.get('message', '') and 'ran-on-' in t['message']]
    assert len(sent) == 1, sent
    newer, older = sent[0].split('Your check-run ended. Its answer, on commit ')[1:]
    assert newer.startswith(second + ':') and 'ran-on-' + second in newer and first not in newer, sent[0]
    assert older.startswith(first + ':') and 'ran-on-' + first in older and second not in older, sent[0]
    js('task', 'close', two, '--discard')
    # 32t5. A scenario's `--part` (docs/tasks.md "Scenarios"): the command gets WSX_SCENARIO_PART, a
    # repeat of the same part joins the run, the full run and a bad part or a plain check are
    # refused, and the part's pass is no full pass: no evidence. A full run records its own.
    mark('32t5')
    configure(LAND_CONFIG + '  scenarios:\n    parted: "ci: echo part=[$WSX_SCENARIO_PART]"\n', 'a scenario with parts')
    parted = js('task', 'new', 'CHECKRUN_PART parted tasks WORK part.txt')['id']
    done = wait_state(parted, 'ready', 'failed')
    assert done['state'] == 'ready', log(parted)
    first, again, full_run, bad, plain_check = next(t for t in traces() if 'check_run_part' in t)['check_run_part']
    assert first[0] == again[0] == 0 and first[1].startswith('queued on CI') and again[1].startswith('queued on CI'), (first, again)
    assert full_run[0] != 0 and '--part tasks still runs' in full_run[1], full_run
    assert bad[0] != 0 and '--part takes one name' in bad[1], bad
    assert plain_check[0] != 0 and '--part needs exactly one scenario' in plain_check[1], plain_check
    ran = next(e for e in log(parted) if e['kind'] == 'check_run')['results']
    assert len(ran) == 1 and ran[0]['command'].startswith("ci: export WSX_SCENARIO_PART='tasks'; ") and 'part=[tasks]' in ran[0]['output'], ran
    turn = [t for t in traces() if 'Your check-run ended' in t.get('message', '') and "export WSX_SCENARIO_PART='tasks'" in t['message']]
    assert len(turn) == 1 and 'not a full pass' in turn[0]['message'], turn
    assert not [e for e in done['result']['result']['evidence'] if 'recorded by the host' in e['check']], done['result']
    full = js('task', 'new', 'CHECKRUN_LATER parted WORK full.txt')['id']
    done = wait_state(full, 'ready', 'failed')
    proof = [e for e in done['result']['result']['evidence'] if 'check-run parted passed' in e['check']]
    assert done['state'] == 'ready' and len(proof) == 1 and 'WSX_SCENARIO_PART]' in proof[0]['proof'] and 'export' not in proof[0]['proof'], done['result']
    for n in (parted, full):
        js('task', 'close', n, '--discard')
    # 32t6. A `ci:` run that ends outside the tree (exit 75: the rate limit, a queue cancel) runs
    # again on the same tree, up to 3 runs (docs/tasks.md "Scenarios"): one `check_run` entry, a
    # `ci_retry` entry for each retry, and the worker gets no turn until a run answers.
    mark('32t6')
    outside = {'ci403': 'echo "gh api: HTTP 403: API rate limit exceeded"; echo "retry-after: 1"',
               'cicancel': 'echo "RuntimeError: the CI run is cancelled: no job started in 60 min of queue"',
               'cinever': 'echo "gh api: HTTP 403: API rate limit exceeded"'}
    counts = {name: base / (name + '.count') for name in outside}
    # ^ ci403 and cicancel answer on the second run; cinever never does.
    configure(LAND_CONFIG + '  scenarios:\n' + ''.join(
        "    %s: 'ci: echo x >> %s; if [ $(wc -l < %s) -lt %d ]; then %s; exit 75; fi; echo ci-passed'\n"
        % (name, counts[name], counts[name], 99 if name == 'cinever' else 2, said) for name, said in outside.items()), 'ci runs outside the tree')
    retried = {name: js('task', 'new', 'CHECKRUN_LATER %s WORK %s.txt' % (name, name))['id'] for name in outside}
    for name in ('ci403', 'cicancel'):
        n = retried[name]
        done = wait_state(n, 'ready', 'failed')
        assert done['state'] == 'ready' and counts[name].read_text().count('x') == 2, (name, log(n))
        assert len(kinds(n, 'check_run')) == 1 and len(kinds(n, 'ci_retry')) == 1, log(n)
        retry = [e for e in log(n) if e['kind'] == 'ci_retry'][0]
        assert retry['attempt'] == 1 and retry['wait_secs'] >= 1 and retry['command'].startswith('ci: ') and retry['reason'], retry
        assert [e['trigger'] for e in log(n) if e['kind'] == 'result'] == ['step'], log(n)
        assert [e for e in done['result']['result']['evidence'] if 'check-run %s passed' % name in e['check']], done['result']
    assert 'rate limit' in [e for e in log(retried['ci403']) if e['kind'] == 'ci_retry'][0]['reason']
    # ^ Three runs in all; the worker then gets the temporary failure as today.
    n = retried['cinever']
    assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    assert counts['cinever'].read_text().count('x') == 3 and len(kinds(n, 'ci_retry')) == 2, log(n)
    assert [e['trigger'] for e in log(n) if e['kind'] == 'result'] == ['step', 'check_run'], log(n)
    assert any('exit 75 (a temporary failure, not kept' in t.get('message', '') for t in traces()), 'the temporary failure turn'
    for n in retried.values():
        js('task', 'close', n, '--discard')
    # 32t7. A `ci:` assert that main's latest CI run fails with too (the script's `== main's latest
    # CI run` block) starts no turn: the host records "failed on main too" as evidence, which the
    # manager's ready event names. An assert that only other tasks fail with answers a short
    # "known failure" turn without a rerun request; an own assert goes to the worker as before.
    mark('32t7')
    configure(LAND_CONFIG + '  scenarios:\n'
              '    cimain: \'ci: echo "FAIL: ci main assert 12"; echo "== main\'\'s latest CI run https://x (abc123) =="; echo "FAIL: ci main assert 34"; exit 1\'\n'
              '    ciknown: \'ci: echo "FAIL: ci known assert"; exit 1\'\n'
              '    ciown: \'ci: echo "FAIL: ci own assert"; exit 1\'\n', 'ci assert scenarios')
    main_task = js('task', 'new', 'CHECKRUN_LATER cimain WORK cimain.txt')['id']
    done = wait_state(main_task, 'ready', 'failed')
    proof = [e for e in done['result']['result']['evidence'] if 'check-run cimain failed on main too' in e['check']]
    assert done['state'] == 'ready' and len(proof) == 1 and 'FAIL: ci main assert 12' in proof[0]['check'] and 'not yours' in proof[0]['proof'], done['result']
    assert proof[0]['check'].endswith('(recorded by the host)') and [e['trigger'] for e in log(main_task) if e['kind'] == 'result'] == ['step'], log(main_task)
    wait(lambda: events_with('check-run cimain failed on main too'), 'the ready event names the known failure', 30)
    known_a = js('task', 'new', 'CHECKRUN_LATER ciknown WORK ciknown1.txt')['id']
    assert wait_state(known_a, 'ready', 'failed')['state'] == 'ready', log(known_a)
    assert [e['trigger'] for e in log(known_a) if e['kind'] == 'result'] == ['step', 'check_run'], log(known_a)
    known_b = js('task', 'new', 'CHECKRUN_LATER ciknown WORK ciknown2.txt')['id']
    done = wait_state(known_b, 'ready', 'failed')
    assert done['state'] == 'ready' and not [e for e in done['result']['result']['evidence'] if 'recorded by the host' in e['check']], done['result']
    sent = {t['message'] for t in traces() if 'Your check-run ended' in t.get('message', '') and 'ci known assert' in t['message']}
    first_turn, = [m for m in sent if 'Put it in your result' in m]
    second_turn, = [m for m in sent if 'known failure: do not run it again' in m]
    assert 'fails in #%s too' % known_a in second_turn and 'not yours' in second_turn and 'Put it in' not in second_turn, second_turn
    own = js('task', 'new', 'CHECKRUN_LATER ciown WORK ciown.txt')['id']
    assert wait_state(own, 'ready', 'failed')['state'] == 'ready', log(own)
    assert [e['trigger'] for e in log(own) if e['kind'] == 'result'] == ['step', 'check_run'], log(own)
    assert [m for m in (t.get('message', '') for t in traces()) if 'ci own assert' in m and 'Put it in your result' in m], 'the own assert turn'
    for n in (main_task, known_a, known_b, own):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no ci assert scenarios')
    # 32v. Two lanes (docs/tasks.md "Check slot"): a scenario check-run holds the scenarios lane, a
    # task's result checks still run at once in the checks lane, and a second scenario run waits
    # with the scenarios reason.
    mark('32v')
    lane_gate = base / 'lane-gate'
    configure(LAND_CONFIG + '  scenarios:\n    slow: %s\n' % gate_cmd(lane_gate), 'a slow scenario')
    holds = js('task', 'new', 'CHECKRUN_LATE slow WORK lane1.txt')['id']
    wait(lambda: kinds(holds, 'result'), 'the first scenario run', 30)
    waits = js('task', 'new', 'CHECKRUN_LATE slow WORK lane2.txt')['id']
    reason = wait(lambda: (show(waits)['record'].get('waiting') or {}).get('reason', '').startswith('queued') and show(waits)['record']['waiting']['reason'], 'the second scenario run queued', 30)
    assert reason.startswith('queued for the scenario test slot behind demo t%s' % holds), reason
    # ^ `show` and its text say the scenario wait in wsx-core's words (docs/tasks.md "Status").
    lane_status = show(waits)['status']
    assert lane_status['label'] == 'Waiting for the scenario test slot', lane_status
    assert re.match(r'Waiting for the scenario test slot for [^:]+: #%s runs its scenario tests now(, \d+ more tasks? ahead)?\. Its scenario tests start when the slot is free\.$' % holds, lane_status['sentence']), lane_status
    assert 'status:       Waiting for the scenario test slot for ' in cli('task', 'show', waits).stdout
    plain = js('task', 'new', 'WORK lane3.txt')['id']
    assert wait_state(plain, 'ready', 'failed')['state'] == 'ready', log(plain)
    checked = next(e for e in log(plain) if e['kind'] == 'checks')
    assert all(c['queued_ms'] < 1500 for c in checked['results']), checked
    assert not lane_gate.exists()
    lane_gate.touch()
    for n in (holds, waits):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    for n in (holds, waits, plain):
        js('task', 'close', n, '--discard')
    # 32u. A result that comes while the checks of an earlier one run (Claude's own turn for a
    # background notice) is answered by those checks: one result, one run, one revision.
    mark('32u')
    gate_open = base / 'gate-open'
    configure(LAND_CONFIG.replace('"test ! -e broken.txt"', '"%s"' % gate_cmd(gate_open)), 'a slow gate')
    twice = js('task', 'new', 'REPEAT WORK twice.txt')['id']
    wait(lambda: kinds(twice, 'result') == ['result'] * 2, 'the repeated result', 30)
    assert show(twice)['state'] == 'checking', show(twice)
    gate_open.touch()
    assert wait_state(twice, 'ready', 'failed')['state'] == 'ready', log(twice)
    time.sleep(1)
    assert kinds(twice, 'checks', 'revision') == ['checks', 'revision'], log(twice)
    js('task', 'close', twice, '--discard')
    # 32ak. The simplify check (docs/tasks.md "Workflow"): a full task whose worker was lent the
    # simplify skill and never loaded it is sent back once, and the ready event names it when it
    # still did not; one that loads it goes ready at once; a small task is not checked.
    mark('32ak')
    simplify_skill = profile_root / 'skills/simplify'
    simplify_skill.mkdir(parents=True)
    (simplify_skill / 'SKILL.md').write_text('---\nname: simplify\ndescription: fixture\n---\nx\n')
    configure(LAND_CONFIG + '  skills: owner\n  workflow:\n    steps:\n      - {name: build, prompt: build it}\n',
              'simplify check')
    never = js('task', 'new', 'WORK never.txt')['id']
    ran = js('task', 'new', 'WORK ran.txt SIMPLIFY')['id']
    late = js('task', 'new', 'WORK late.txt SIMPLIFYLATE')['id']
    small = js('task', 'new', '--size', 'small', 'WORK small.txt')['id']
    for n in (never, ran, late, small):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    asked = lambda n: [e for e in log(n) if e['kind'] == 'result' and e.get('trigger') == 'simplify']
    assert (len(asked(never)), len(asked(late))) == (1, 1) and not asked(ran) and not asked(small), \
        [log(n) for n in (never, ran, late, small)]
    sent = [t for t in traces() if 'The simplify skill did not run' in str(t.get('message'))]
    assert len(sent) == 2 and all('owner:simplify' in t['message'] for t in sent), sent
    ready_line = lambda n: wait(lambda: [line for e in events_with('#%d ready r1' % n) for line in e['text'].split('\n')
                                         if line.startswith('#%d ready r1' % n)], 'the ready event of #%d' % n, 30)[0]
    assert '; simplify did not run' in ready_line(never), manager_entries()[-6:]
    assert 'simplify did not run' not in ' '.join(ready_line(n) for n in (ran, late, small))
    assert 'simplify did not run' in show(never)['status']['sentence'], show(never)['status']
    assert 'simplify did not run' not in show(late)['status']['sentence'], show(late)['status']
    for n in (never, ran, late, small):
        js('task', 'close', n, '--discard')
    shutil.rmtree(profile_root / 'skills')
    configure(LAND_CONFIG, 'no scenario')
    return created


def part_host(created=None):
    """Sections 32d to 39: the disk queue, stalls, chats, other projects, the fallback agent and lost hosts."""
    if created is None:
        created = fresh_project()
        store_up(created)
        manager_up(created)

    # 32d. The disk queue (docs/tasks.md "Checks, revisions and report"): tasks that wait for
    mark('32d')
    # disk resume one at a time, oldest first, so they do not fill the freed space at once, but
    # the next one resumes about a minute later, while the first one's checks still run.
    release.unlink(missing_ok=True)
    slot_config(240)
    roomy = (project / 'wsx.config.yml').read_text()
    configure(roomy.replace('minFreeGb: 0', 'minFreeGb: 100000'), 'no disk for two')
    older, newer = [js('task', 'new', 'WORK disk%d.txt' % i)['id'] for i in (1, 2)]
    for n in (older, newer):
        wait(lambda: (show(n)['record'].get('waiting') or {}).get('on') == 'disk', 't%s disk wait' % n)
    configure(roomy, 'disk for one at a time')
    # ^ The older one's checks run until `release`; the newer one waits for its turn.
    wait(lambda: show(older)['state'] in ('working', 'checking'), 'the older disk wait resumed', 45)
    queued = wait(lambda: (w := show(newer)['record'].get('waiting') or {}).get('reason', '').startswith('queued') and w,
                  'the newer disk wait queued', 45)
    assert queued == {'on': 'disk', 'reason': 'queued for disk behind demo t%s' % older}, queued
    assert show(newer)['state'] == 'waiting' and show(older)['state'] in ('working', 'checking')
    # ^ An add while it waits before its first turn joins that turn: it neither skips the queue
    # nor replaces the held prompt.
    added = js('task', 'add', str(newer), 'ADDED-WHILE-DISK')
    assert added['ok'] and 'joined the work that waits for disk' in added['message'], added
    assert show(newer)['record'].get('waiting') == queued and not [t for t in traces() if 'ADDED-WHILE-DISK' in t.get('message', '')]
    # ^ The settle time (60 s) and one disk poll (30 s), never the older one's whole turn.
    wait(lambda: show(newer)['state'] in ('working', 'checking'), 'the newer disk wait resumed', 150)
    assert show(older)['state'] == 'checking', show(older)
    release.touch()
    for n in (older, newer):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    joined = [t['message'] for t in traces() if 'ADDED-WHILE-DISK' in t.get('message', '')]
    assert len(joined) == 1 and 'WORK disk2.txt' in joined[0], joined
    for n in (older, newer):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'disk as before')
    # 32af. A task waits for another to land (docs/tasks.md "After another task"): the status
    # says so on a ready task and a working one, the stall check skips it, the landing ends the
    # wait with the text as an add, a close ends it with one event, and a wait past 24 h is one
    # stall.
    mark('32af')
    configure(LAND_CONFIG.replace('tasks:\n', 'tasks:\n  manager: { reviewMinutes: 0.2 }\n'), 'after wait')
    after_gate = base / 'after-gate'

    def manager_after(*argv, because='files', ok=True):
        kind = ['--because', because] if because else []
        done = subprocess.run([str(wsx), 'task', 'after', *map(str, argv), *kind, '--json'], cwd=project, env=manager_env,
                              capture_output=True, text=True, timeout=60)
        assert (done.returncode == 0) == ok, done.stderr
        return done

    parked, unparked = (manager_new('NOOP af-plan', '--plan') for _ in range(2))
    for n in (parked, unparked):
        assert wait_state(n, 'waiting', 'ready', 'failed')['record']['waiting']['on'] == 'plan', show(n)
    b, a, w, c, d, e, f, g = (manager_new(prompt) for prompt in (
        'WORK af-b.txt', 'WORK af-a.txt', 'WAIT %s WORK af-w.txt' % after_gate, 'WORK af-c.txt',
        'WORK af-d.txt', 'WORK af-e.txt', 'WORK af-f.txt', 'WORK af-g.txt'))
    for n in (b, a, c, d, e, f, g):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    assert show(w)['state'] == 'working'
    # ^ A wait needs another open task, not the task itself.
    assert 'this task' in manager_after(a, a, 'WORK x.txt', ok=False).stderr
    # ^ The reason is required and one of three; a missing or unknown one is refused with the list.
    for bad in (None, 'because'):
        refused = manager_after(a, b, 'WORK x.txt', because=bad, ok=False).stderr
        assert 'needs' in refused and 'files' in refused and 'check' in refused, refused
    assert 'after' not in show(a)['record']
    manager_after(a, b, 'WORK af-a2.txt', because='files')
    manager_after(w, b, 'WORK af-w2.txt', because='needs')
    manager_after(c, d, 'WORK af-c2.txt', because='check')
    manager_after(e, f, 'WORK af-e2.txt', because='check')
    manager_after(parked, g, 'WORK af-plan2.txt', because='needs')
    status = show(a)['status']
    assert status['kind'] == 'after' and status['label'] == 'Waiting for #%d to land' % b, status
    # ^ The sentence gives the reason, not the manager's instruction to the worker.
    assert '#%d is ready' % b in status['sentence'] and 'WORK af-a2.txt' not in status['sentence'], status
    assert 'It changes the same files as #%d, so it lands after it.' % b in status['sentence'], status
    assert status['wait'] == 'same files', status
    assert show(a)['record']['after']['because'] == 'files', show(a)['record']
    assert "Its checks fail on main until #%d lands." % d in show(c)['status']['sentence'], show(c)['status']
    row = next(r for r in js('task', 'ledger')['open'] if r['id'] == a)
    assert row['after']['task'] == b and row['after']['because'] == 'files' and row['status']['kind'] == 'after', row
    working = show(w)['status']
    assert working['kind'] == 'working' and working['sentence'].endswith(
        "Then it waits for #%d to land. It builds on #%d's change." % (b, b)), working
    assert working['wait'] == "then waits for #%d: builds on it" % b, working
    # ^ The task waited for says who waits: the kinds follow the numbers when they differ.
    waited = show(b)['status']
    assert waited['sentence'].endswith('Waited for by #%d (same files), #%d (builds on it).' % (a, w)), waited
    assert waited['waited_by'] == [{'task': a, 'because': 'files'}, {'task': w, 'because': 'needs'}], waited
    assert next(r for r in js('task', 'ledger')['open'] if r['id'] == b)['status']['waited_by'] == waited['waited_by']
    single = show(d)['status']
    assert single['sentence'].endswith(' Waited for by #%d.' % c) and single['waited_by'] == [{'task': c, 'because': 'check'}], single
    assert show(f)['status']['waited_by'] == [{'task': e, 'because': 'check'}], show(f)['status']
    assert [e['kind'] for e in log(a) if e['kind'] == 'after'] == ['after']
    # ^ Backdated: the file is read by the stall check, and the idle host saves nothing meanwhile.
    record_file = Path(show(e)['log']).parent / 'task.json'
    record = json.loads(record_file.read_text())
    record['after']['since_unix_ms'] -= 25 * 3600 * 1000
    record_file.write_text(json.dumps(record))
    # ^ Past the review limit (0.2 min) and the grace: a task that waits for a landing is no
    # "ready, not reviewed", and one that waits past 24 h is the only stall reported.
    # ^ a and c are ready past the review limit for two sweeps after the grace, and the sweep that
    # reports the stalls of unparked and e looked at them too.
    aged = lambda: all(time.time() * 1000 - show(n)['record']['state_unix_ms'] > (12 + STALL_GRACE + 2 * SWEEP + 2) * 1000
                       for n in (a, c))
    stays(lambda: aged() and stalls(unparked) and stalls(e), lambda: not stalls(a) and not stalls(c),
          'the plan and the long wait stall, a and c do not', 150)
    # ^ The `after` is the manager's review of a plan too; a plan that is not parked still stalls.
    assert not stalls(parked), stalls(parked)
    plan_stall = stalls(unparked)
    assert len(plan_stall) == 1 and re.search(r'^#%d stalled r\d+: .*; waiting on plan for .*; the manager has not reviewed it$' % unparked, plan_stall[0]), plan_stall
    long = stalls(e)
    assert len(long) == 1 and re.search(r'^#%d stalled r1: .*; ready for 1 d [01] h; waiting for #%d to land$' % (e, f), long[0]), long
    sweeps(1)
    assert stalls(e) == long, stalls(e)
    # ^ The other task is closed, not landed: one event, no text added, the task is ready as before.
    js('task', 'close', d, '--discard')
    over = wait(lambda: events_with('#%d wait over: #%d was closed without landing; nothing was added to #%d' % (c, d, c)),
                'the wait-over event', 40)
    time.sleep(12)
    assert len(events_with('#%d wait over' % c)) == len(over) == 1, events_with('#%d wait over' % c)
    assert 'after' not in show(c)['record'] and show(c)['state'] == 'ready', show(c)['record']
    assert [(x['kind'], x.get('landed')) for x in log(c) if x['kind'] in ('after_ended', 'message')] == [('message', None), ('after_ended', False)], log(c)
    # ^ The landing ends the others: the text is a message of the ready task, which runs a turn,
    # and of the working one, which queues it behind its turn.
    assert land(b)['state'] == 'landed'
    wait(lambda: kinds(a, 'after_ended') and kinds(w, 'after_ended'), 'both waits end', 40)
    assert [(x['kind'], x.get('landed')) for x in log(a) if x['kind'] == 'after_ended'] == [('after_ended', True)]
    assert wait_state(a, 'ready', 'failed')['state'] == 'ready' and 'after' not in show(a)['record'], log(a)
    adds = [x['text'] for x in log(a) if x['kind'] == 'message' and x['mode'] == 'add']
    assert adds == ['WORK af-a2.txt'] and len(kinds(a, 'result')) == 2, log(a)
    assert [x['trigger'] for x in log(a) if x['kind'] == 'result'] == ['step', 'message'], log(a)
    assert show(w)['state'] == 'working' and len(kinds(w, 'result')) == 0, log(w)
    after_gate.touch()
    wait(lambda: len(kinds(w, 'result')) == 2, 'the working task runs the add after its turn', 60)
    assert wait_state(w, 'ready', 'failed')['state'] == 'ready' and 'after' not in show(w)['record'], log(w)
    for n in (a, w, c, e, f, g, parked, unparked):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'after wait over')
    # 32ag. A landed task stays landed: an add that waited for the worker while the checks of its
    # land ran is not delivered, the manager is told, and the close that follows leaves `landed`.
    # Before, the queued add started a turn on the landed task, and the close made it `closed`.
    mark('32ag')
    land_gate = base / 'land-gate'
    land_gate.touch()
    configure(LAND_CONFIG.replace('  checks:\n', '  checks:\n    - "%s"\n' % gate_cmd(land_gate)), 'land gate')
    gated = js('task', 'new', 'WORK ag-q.txt')['id']
    assert wait_state(gated, 'ready', 'failed')['state'] == 'ready', log(gated)
    (project / 'ag-moved.txt').write_text('moved\n')
    git('add', 'ag-moved.txt')
    git('commit', '-q', '-m', 'main moves for the gated land')
    land_gate.unlink()
    assert 'checks run on the new revision' in land(gated)['message'], show(gated)
    wait_state(gated, 'checking')
    results_before = len(kinds(gated, 'result'))
    queued = js('task', 'add', gated, 'ADD-DURING-LAND\nand a second line')
    assert queued['ok'] and 'queued until the current turn ends' in queued['message'], queued
    land_gate.touch()
    assert wait_state(gated, 'landed', 'ready', 'failed')['state'] == 'landed', log(gated)
    wait_closed(gated)
    assert show(gated)['state'] == 'landed' and show(gated)['status']['kind'] == 'landed', show(gated)
    assert main_has('ag-q.txt') and len(kinds(gated, 'result')) == results_before, log(gated)
    assert not [t for t in traces() if 'ADD-DURING-LAND' in t.get('message', '') and not t.get('manager')], 'the add reached the worker'
    told = wait(lambda: events_with('#%d landed at' % gated, 'not delivered because the task landed', 'ADD-DURING-LAND'),
                'the dropped add event', 40)
    assert len(told) == 1 and told[0]['short'][0].get('wakes', True), told
    configure(LAND_CONFIG, 'land gate over')
    # 32ah. A worker that is idle or blocked while its task waits for another to land asks nobody:
    # the status is the wait, the manager's event wakes no turn, the agent is idle, and the
    # overview lists no need. The same worker with no wait is "Waiting for your answer".
    mark('32ah')
    idle_gate = base / 'idle-gate'
    wanted = manager_new('WORK ah-w.txt')
    assert wait_state(wanted, 'ready', 'failed')['state'] == 'ready', log(wanted)
    idle, asking = (manager_new('WAIT %s NEEDS WebSearch WORK ah-%s.txt' % (idle_gate, name)) for name in ('i', 'a'))
    manager_after(idle, wanted, 'WORK ah-i2.txt', because='needs')
    idle_gate.touch()
    for n in (idle, asking):
        wait_state(n, 'waiting')
    status = show(idle)['status']
    assert status['kind'] == 'after' and status['label'] == 'Waiting for #%d to land' % wanted, status
    assert 'Waiting for your answer' not in status['sentence'] and status['wait'] == 'builds on it', status
    assert show(asking)['status']['kind'] == 'owner', show(asking)['status']
    wait(lambda: (pane_agent(idle) or {}).get('state') == 'idle', 'the idle worker reports idle')
    wait(lambda: (pane_agent(asking) or {}).get('state') == 'blocked', 'the asking worker reports blocked')
    quiet = wait(lambda: events_with('#%d waiting r' % idle), 'the idle worker event')[0]
    assert [x for x in quiet['short'] if x.get('task') == idle] == [{'text': '#%d waiting' % idle, 'task': idle, 'wakes': False}], quiet
    line = [x for x in quiet['text'].split('\n') if x.startswith('#%d waiting' % idle)][0]
    assert 'waits on owner' not in line and 'waits for #%d to land' % wanted in line, line
    loud = wait(lambda: events_with('#%d waiting r' % asking, 'waits on owner: '), 'the asking worker event')[0]
    assert [x for x in loud['short'] if x.get('task') == asking] == [{'text': '#%d waiting' % asking, 'task': asking}], loud
    # ^ The landing of the task it waits for sends its text, as before.
    assert land(wanted)['state'] == 'landed'
    wait(lambda: kinds(idle, 'after_ended'), 'the idle wait ends', 40)
    assert [x['text'] for x in log(idle) if x['kind'] == 'message' and x['mode'] == 'add'] == ['WORK ah-i2.txt'], log(idle)
    for n in (idle, asking):
        js('task', 'close', n, '--discard')
    wait_closed(wanted)
    # 32ai. `new --after`: the task has no turn until the other one lands, then starts on the
    # default branch with it. `--off` and a close of the other task start it too.
    mark('32ai')
    refused = cli('task', 'new', '--after', str(wanted), 'WORK ai-x.txt', ok=False).stderr
    assert '--because' in refused, refused
    base_a, base_b, base_c = (manager_new('WORK ai-%s.txt' % name) for name in 'abc')
    for n in (base_a, base_b, base_c):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    seen = len(traces())
    follower, freed, orphan = (
        manager_new('WORK ai-f%d.txt' % i, '--after', str(other), '--because', 'needs')
        for i, other in enumerate((base_a, base_b, base_c)))
    for n in (follower, freed, orphan):
        waiting = wait_state(n, 'waiting')
        assert waiting['status']['kind'] == 'after' and waiting['record']['after']['task'] in (base_a, base_b, base_c), waiting
        assert waiting['record']['after']['because'] == 'needs' and not waiting['record'].get('agent_session'), waiting
        assert [x['kind'] for x in log(n)].count('after') == 1, log(n)
    time.sleep(3)
    # ^ A worker's turn: the manager's event lines name the waiting tasks' titles too.
    started = [t for t in traces()[seen:] if not t.get('manager') and 'ai-f' in t.get('message', '')]
    assert not started, ('a waiting task started', started)
    assert js('task', 'after', freed, '--off')['ok']
    assert wait_state(freed, 'ready', 'failed')['state'] == 'ready', log(freed)
    assert land(base_a)['state'] == 'landed'
    js('task', 'close', base_c, '--discard')
    for n in (follower, orphan):
        wait(lambda: kinds(n, 'after_ended'), 'the wait of t%s ends' % n, 40)
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
    tip = git('rev-parse', 'main')
    assert main_has('ai-a.txt'), 'the other task landed'
    for n in (follower, orphan):
        assert subprocess.run(['git', 'merge-base', '--is-ancestor', tip, 'wsx/t%d' % n], cwd=project, env=env).returncode == 0, n
    over = wait(lambda: events_with('#%d wait over: #%d was closed without landing; #%d started without it' % (orphan, base_c, orphan)),
                'the wait-over event', 40)
    assert len(over) == 1, over
    for n in (follower, freed, orphan, base_b):
        js('task', 'close', n, '--discard')
    wait_closed(base_a)
    # 32aj. A worker whose turn ends on a status (`status: paused`, the fake's PAUSE) asks nobody:
    # the task reads paused in the event, the ledger count, `show`, `list` and the ledger row, the
    # agent is idle, and the manager continues it with an add. A worker that asks a real question
    # (`blocked`) still reads "Waiting for your answer" (docs/tasks.md "Status").
    mark('32aj')
    pausing, questioning = manager_new('PAUSE WORK aj-p.txt'), manager_new('NEEDS WebSearch WORK aj-q.txt')
    for n in (pausing, questioning):
        wait_state(n, 'waiting')
    status = show(pausing)['status']
    assert status['kind'] == 'paused' and status['label'] == 'Paused, the manager continues it', status
    assert 'Waiting for your answer' not in status['sentence'] and 'nothing is asked of you' in status['sentence'], status
    assert show(pausing)['record']['waiting']['on'] == 'paused' and show(questioning)['status']['kind'] == 'owner', show(pausing)
    assert re.search(r'^t%d paused ' % pausing, cli('task', 'show', str(pausing)).stdout), cli('task', 'show', str(pausing)).stdout
    assert re.search(r'^t%d +paused ' % pausing, cli('task', 'list').stdout, re.M), cli('task', 'list').stdout
    assert re.search(r'^t%d +waiting ' % questioning, cli('task', 'list').stdout, re.M), cli('task', 'list').stdout
    row_of = {r['id']: r for r in js('task', 'ledger')['open']}
    assert row_of[pausing]['status']['kind'] == 'paused' and row_of[pausing]['waiting']['on'] == 'paused', row_of[pausing]
    wait(lambda: (pane_agent(pausing) or {}).get('state') == 'idle', 'the paused worker reports idle')
    wait(lambda: (pane_agent(questioning) or {}).get('state') == 'blocked', 'the asking worker reports blocked')
    quiet = wait(lambda: events_with('#%d paused r' % pausing, 'waits on paused: Paused, the manager continues it'), 'the paused event')[0]
    assert 'waits on owner' not in [x for x in quiet['text'].split('\n') if x.startswith('#%d paused' % pausing)][0], quiet
    assert [x for x in quiet['short'] if x.get('task') == pausing] == [{'text': '#%d paused' % pausing, 'task': pausing}], quiet
    loud = wait(lambda: events_with('#%d waiting r' % questioning, 'waits on owner: Waiting for your answer'), 'the question event')[0]
    assert [x for x in loud['short'] if x.get('task') == questioning] == [{'text': '#%d waiting' % questioning, 'task': questioning}], loud
    turn = wait(lambda: turn_with('#%d paused r' % pausing), 'the manager turn that read the paused event')
    assert re.search(r'- open: [^\n]*\b1 paused', turn['message']), turn['message']
    assert js('task', 'add', pausing, 'WORK aj-p2.txt')['ok']
    assert wait_state(pausing, 'ready', 'failed')['state'] == 'ready', log(pausing)
    for n in (pausing, questioning):
        js('task', 'close', n, '--discard')
    for n in (pausing, questioning):
        wait_state(n, 'closed')
    # 32e. Approval from the owner's words (docs/tasks.md "Approvals"): in a turn that began with
    # an owner message, the manager names that message and the action runs without a new request;
    # approvals.jsonl records it as approved by that message, and no event repeats it. A message
    # outside the running turn approves nothing.
    approvals_file = Path(created['task_dir']).parent / 'approvals.jsonl'

    def approval_lines():
        return [json.loads(line) for line in approvals_file.read_text().splitlines()]

    # ^ An older wsx wrote an action this one cannot read (approval a9 of the owner's log): the
    # next id still comes after it, so an old decision never settles a new request.
    with open(approvals_file, 'a') as old:
        old.write('{"kind":"request","id":"a90","action":"start","task":1,"unix_ms":1}\n'
                  '{"kind":"decision","id":"a90","approved":true,"outcome":"done","unix_ms":2}\n')
    said_task = js('task', 'new', 'WORK said.txt')['id']
    assert wait_state(said_task, 'ready', 'failed')['state'] == 'ready'
    stale = js('task', 'say', 'RUN wsx task close %d --discard --owner-said 1 -p demo --json' % said_task)
    text = reply_to(stale['id'])['text']
    assert 'message 1 is not in the running turn; nothing was run' in text, text
    assert show(said_task)['state'] == 'ready'
    said = js('task', 'say', 'close #%d\nRUN wsx task close %d --discard --owner-said OWNER_SAID -p demo --json'
              % (said_task, said_task))
    text = reply_to(said['id'])['text']
    assert 'owner was asked' not in text and '--discard --owner-said %d -p demo --json -> 0' % said['id'] in text, text
    assert show(said_task)['state'] == 'closed'
    request, decision = approval_lines()[-2:]
    assert (request['kind'], request['id'], request['action'], request['task']) == ('request', 'a91', 'discard', said_task), request
    assert (decision['kind'], decision['id'], decision['approved'], decision['owner_said']) == ('decision', 'a91', True, said['id']), decision
    assert js('task', 'ledger')['approvals'] == []
    # 32f. A pending request whose task leaves is dropped with the reason, and the manager and
    # the owner read it in the chat.
    gone = js('task', 'new', 'WORK gone.txt')['id']
    assert wait_state(gone, 'ready', 'failed')['state'] == 'ready'
    asked = js('task', 'say', 'RUN wsx task close %d --discard -p demo --json' % gone)
    assert 'the owner was asked (a92)' in reply_to(asked['id'])['text']
    assert [p['id'] for p in js('task', 'ledger')['approvals']] == ['a92']
    js('task', 'close', gone, '--discard')
    wait(lambda: events_with('approval a92 (discard #%d) dropped; #%d is closed' % (gone, gone)), 'the dropped request event')
    assert js('task', 'ledger')['approvals'] == [] and approval_lines()[-1]['outcome'] == 'dropped; #%d is closed' % gone
    assert not events_with('approval a91'), 'an approval by the owner message became an event'
    # ^ A release refuses while items are open (docs/tasks.md "Deploy and release"), but one the
    # owner's message asked for runs, and its answer lists the items still to check on the release.
    item_task = js('task', 'new', 'WORK said-release.txt HV')['id']
    assert wait_state(item_task, 'ready', 'failed')['state'] == 'ready'
    assert land(item_task)['state'] == 'landed'
    wait_closed(item_task)
    open_ids = [i['id'] for i in js('task', 'check')['open']]
    assert open_ids, js('task', 'check')
    configure(LAND_CONFIG + "  release:\n    command: 'echo released >> ../said-release.log'\n    approve: true\n", 'release config')
    refused = cli('task', 'release', ok=False).stderr
    assert 'item(s) to check are open' in refused and 'Nothing was run' in refused, refused
    asked = js('task', 'say', 'release\nRUN wsx task release --owner-said OWNER_SAID -p demo')
    text = reply_to(asked['id'])['text']
    assert ' started; ' in text and 'still to check on the release: %s' % ', '.join(open_ids) in text, text
    wait(lambda: (base / 'said-release.log').exists() and deploy_lock_free(), 'the release the owner asked for')
    assert (base / 'said-release.log').read_text() == 'released\n'
    assert [i['id'] for i in js('task', 'check')['open']] == open_ids
    # ^ The manager passes an item only on the owner's message of its turn (docs/tasks.md "To check"):
    # named ids, no --all, no skip or fail; the row keeps the message id.
    item = open_ids[0]
    def manager_run(words, *args):
        return reply_to(js('task', 'say', words + '\nRUN wsx task check ' + args[0] + ' -p demo --json')['id'])['text']
    text = manager_run('looks good', 'pass %s' % item)
    assert '-> 1 ' in text and 'is for the owner' in text, text
    text = manager_run('looks good', 'pass %s --owner-said 1' % item)
    assert 'message 1 is not in the running turn; nothing was run' in text, text
    assert "unexpected argument '--owner-said'" in manager_run('looks good', 'skip %s --owner-said OWNER_SAID' % item)
    for args in ('pass --all --owner-said OWNER_SAID', 'skip %s' % item, 'skip --all', 'fail %s --note no' % item):
        text = manager_run('looks good', args)
        assert '-> 1 ' in text and 'is for the owner' in text, (args, text)
    assert [i['id'] for i in js('task', 'check')['open']] == open_ids
    said = js('task', 'say', 'the %s looks good\nRUN wsx task check pass %s --owner-said OWNER_SAID -p demo --json' % (item, item))
    text = reply_to(said['id'])['text']
    assert '--owner-said %d -p demo --json -> 0' % said['id'] in text, text
    rows = [json.loads(line) for line in (project_store / 'checks.jsonl').read_text().splitlines()]
    assert [(r['state'], r.get('owner_said')) for r in rows if r['id'] == item][-1] == ('passed', said['id']), rows[-3:]
    assert [i['id'] for i in js('task', 'check')['open']] == open_ids[1:]
    # 32ha. The owner's go outlives its turn when the action can only run after a task lands
    # (docs/tasks.md "Approvals", "After a landing"): `--owner-said <id> --after <n>` checks the
    # message now and records the request waiting; the manager host runs it once #n has landed.
    release_log = base / 'said-release.log'

    def ready_task(work):
        n = js('task', 'new', work)['id']
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
        return n

    def manager_says(words, command):
        said = js('task', 'say', '%s\nRUN wsx task %s OWNER_SAID -p demo --json' % (words, command))
        return said['id'], reply_to(said['id'])['text']

    def decision_of(request_id):
        return wait(lambda: next((l for l in approval_lines() if l['kind'] == 'decision' and l['id'] == request_id), None),
                    'the decision of %s' % request_id, 40)

    aft1 = ready_task('WORK after-first.txt HV')
    aft_ran = release_log.read_text()
    lines_before = len(approval_lines())
    # ^ A message outside the turn, a task that does not exist, one that is the action's own,
    # and --after with no message refuse with nothing recorded.
    text = reply_to(js('task', 'say', 'RUN wsx task release --owner-said 1 --after %d -p demo --json' % aft1)['id'])['text']
    assert 'message 1 is not in the running turn' in text and 'nothing was' in text, text
    _, text = manager_says('release when it lands', 'release --after 99999 --owner-said')
    assert '--after 99999: no such task; nothing was recorded' in text, text
    _, text = manager_says('land it after itself', 'land %d --after %d --owner-said' % (aft1, aft1))
    assert 'cannot wait for itself; nothing was recorded' in text, text
    text = reply_to(js('task', 'say', 'RUN wsx task release --after %d -p demo --json' % aft1)['id'])['text']
    assert 'required arguments' in text and '--owner-said' in text, text
    assert len(approval_lines()) == lines_before and js('task', 'ledger')['approvals'] == []
    # ^ The release waits: nothing runs, no card, the ledger says what waits.
    aft_said, text = manager_says('release once #%d has landed' % aft1, 'release --after %d --owner-said' % aft1)
    assert 'runs when #%d lands' % aft1 in text and 'owner was asked' not in text, text
    aft_req = approval_lines()[-1]
    assert (aft_req['kind'], aft_req['action'], aft_req['after'], aft_req['said']) == ('request', 'release', aft1, aft_said), aft_req
    assert len(approval_lines()) == lines_before + 1 and release_log.read_text() == aft_ran
    aft_rows = js('task', 'ledger')['approvals']
    assert [(r['id'], r['after'], r['label']) for r in aft_rows] == [(aft_req['id'], aft1, 'release after #%d, approved by the owner' % aft1)], aft_rows
    aft_refused = cli('task', 'approve', aft_req['id'], ok=False).stderr
    assert 'runs when #%d lands' % aft1 in aft_refused, aft_refused
    assert not events_with('approval %s' % aft_req['id'])
    # ^ A later turn: #n lands, the release runs once with the owner's message on its decision,
    # and the open To check items stay on it.
    assert land(aft1)['state'] == 'landed'
    aft_dec = decision_of(aft_req['id'])
    assert aft_dec['approved'] and aft_dec['owner_said'] == aft_said and aft_dec['outcome'].startswith('done'), aft_dec
    assert 'still to check on the release' in aft_dec['outcome'], aft_dec
    wait(lambda: release_log.read_text() == aft_ran + 'released\n' and deploy_lock_free(), 'the release after the land')
    wait(lambda: events_with('approval %s (release) approved; done' % aft_req['id']), 'the result event')
    wait_closed(aft1)
    time.sleep(3)
    assert release_log.read_text() == aft_ran + 'released\n' and js('task', 'ledger')['approvals'] == []
    # ^ A land waits as well: the same run as `approve`.
    aft_head, aft_tail = ready_task('WORK after-head.txt'), ready_task('WORK after-tail.txt')
    aft_said, text = manager_says('land #%d after #%d' % (aft_tail, aft_head), 'land %d --after %d --owner-said' % (aft_tail, aft_head))
    aft_req = approval_lines()[-1]
    assert (aft_req['action'], aft_req['task'], aft_req['after']) == ('land', aft_tail, aft_head) and show(aft_tail)['state'] == 'ready', aft_req
    assert land(aft_head)['state'] == 'landed'
    aft_dec = decision_of(aft_req['id'])
    assert aft_dec['approved'] and aft_dec['owner_said'] == aft_said and aft_dec['outcome'].startswith('done'), aft_dec
    wait(lambda: show(aft_tail)['state'] == 'landed', 'the land that waited')
    wait_closed(aft_head)
    wait_closed(aft_tail)
    # ^ #n closes without landing: dropped, and the manager hears why. A newer request that
    # covers it replaces it first.
    aft_gone, aft_newer = ready_task('WORK after-gone.txt'), ready_task('WORK after-newer.txt')
    _, text = manager_says('release when #%d lands' % aft_gone, 'release --after %d --owner-said' % aft_gone)
    aft_old = approval_lines()[-1]
    _, text = manager_says('rather when #%d lands' % aft_newer, 'release --after %d --owner-said' % aft_newer)
    # ^ The replaced request's decision comes after the new request: take the last request.
    aft_cur = [l for l in approval_lines() if l['kind'] == 'request'][-1]
    wait(lambda: events_with('approval %s (release) dropped; replaced by %s' % (aft_old['id'], aft_cur['id'])), 'the replaced event')
    assert [r['id'] for r in js('task', 'ledger')['approvals']] == [aft_cur['id']]
    js('task', 'close', aft_newer, '--discard')
    wait(lambda: events_with('approval %s (release) dropped; #%d is closed, so release waits for nothing' % (aft_cur['id'], aft_newer)), 'the dropped event')
    assert decision_of(aft_cur['id'])['approved'] is False and js('task', 'ledger')['approvals'] == []
    js('task', 'close', aft_gone, '--discard')
    assert release_log.read_text() == aft_ran + 'released\n'
    # ^ The approved release runs at the land. The project's own deploy waits for the quiet
    # window, then runs for the same commit after it (docs/tasks.md "Deploy and release"). A
    # deploy by hand, no approval, still refuses while the deploy runs.
    order_log = base / 'order.log'
    configure(LAND_CONFIG + """  deploy:
    command: 'echo start >> ../order.log; sleep 6; echo deploy >> ../order.log'
    auto: true
    quietMinutes: 0.2
  release:
    command: 'echo release >> ../order.log'
    approve: true
""", 'deploy and release order')
    ord_task = ready_task('WORK order.txt')
    _, text = manager_says('release once #%d has landed' % ord_task, 'release --after %d --owner-said' % ord_task)
    ord_req = approval_lines()[-1]
    assert (ord_req['action'], ord_req['after']) == ('release', ord_task), ord_req
    records_before = len(deploy_records())
    assert land(ord_task)['state'] == 'landed'
    wait(lambda: order_log.exists() and order_log.read_text() == 'release\nstart\n', 'the release at the land, then the deploy', 60)
    refused = cli('task', 'deploy', ok=False).stderr
    assert 'a deploy or release of demo runs' in refused and 'Nothing was run' in refused, refused
    ord_dec = decision_of(ord_req['id'])
    assert ord_dec['approved'] and ord_dec['outcome'].startswith('done: release of '), ord_dec
    wait(lambda: order_log.read_text() == 'release\nstart\ndeploy\n' and deploy_lock_free(), 'the deploy after the quiet window', 30)
    assert [r['kind'] for r in deploy_records()[records_before:]] == ['release', 'deploy'], deploy_records()[records_before:]
    assert all(r['exit'] == 0 for r in deploy_records()[records_before:]), deploy_records()[records_before:]
    wait(lambda: events_with('approval %s (release) approved; done' % ord_req['id']), 'the release event')
    wait_closed(ord_task)
    configure(LAND_CONFIG, 'release as before')
    # 32g. A plan first (docs/tasks.md "Workflow"): the first turn of a `--plan` task is told to
    # plan and stop; its result runs no check and waits on plan for the manager, and an add
    # continues it as an ordinary task.
    planned = js('task', 'new', '--plan', 'NOOP plan the change')['id']
    waiting = wait_state(planned, 'waiting', 'ready', 'failed')
    assert waiting['record']['waiting'] == {'on': 'plan', 'reason': 'the worker wrote its plan'}, waiting['record']
    assert waiting['status']['label'] == "Plan ready, waiting for the manager's review", waiting['status']
    assert 'plan' not in waiting['record'], waiting['record']
    assert not kinds(planned, 'checks', 'revision'), log(planned)
    assert 'First write a short plan and stop' in next(e for e in log(planned) if e['kind'] == 'result')['summary']
    wait(lambda: events_with('#%d waiting r0' % planned, "waits on plan: Plan ready, waiting for the manager's review for "), 'the plan event')
    js('task', 'add', planned, 'go WORK planned.txt')
    assert wait_state(planned, 'ready', 'failed')['state'] == 'ready'
    assert kinds(planned, 'checks', 'revision') == ['checks', 'revision']
    js('task', 'close', planned, '--discard')
    # 32h. The real Claude in wsx's boundary (docs/tasks.md "Permissions"): with no prompt it
    # runs part 23's probe and gets the same answers. Skipped, and said so, when no claude is on PATH.
    mark('32h')
    real = shutil.which('claude')
    if real is None:
        print('32h skipped: no real claude on PATH', file=sys.stderr)
    else:
        probe_up(created)
        # ^ `tasks.env` reaches the worker's launch: the wrapper runs the real claude, signed in
        # through the real home, for this task alone.
        # ^ The real claude signs in through the real home, not demo's fixture profile root.
        (bin_dir / 'claude').write_text('#!/bin/sh\nif [ -n "$REAL_CLAUDE" ]; then HOME=%s exec env -u CLAUDE_CONFIG_DIR %s "$@"; fi\n%s'
                                        % (json.dumps(str(Path.home())), json.dumps(os.path.realpath(real)),
                                           fake.read_text().split('\n', 1)[1]))
        configure(LAND_CONFIG + '  env: { REAL_CLAUDE: "1" }\n', 'real claude')
        (base / 'project-t998').mkdir()
        tip = git('rev-parse', 'main')
        spec_file, answer = probe_files(next_task())
        command = 'python3 %s %s %s' % (probe_script, spec_file, answer)
        auto = js('task', 'new', '--model', 'sonnet', '--title', 'Boundary probe',
                  'This is a wsx test of the worker boundary. Run this one Bash command exactly as written: '
                  '`%s`. It records what the boundary allows in a file outside the working copy. Then change '
                  'no file, commit nothing and return status done with the command and its exit code as '
                  'evidence.' % command)['id']
        assert spec_file.name == 'probe-%d.json' % auto
        ended = wait(lambda: (current := show(auto))['state'] in ('ready', 'failed', 'waiting') and current,
                     'real claude turn', 600)
        assert ended['state'] == 'ready', ended
        seen = json.loads(answer.read_text())
        assert seen == PROBE_EXPECTED, {k: v for k, v in seen.items() if PROBE_EXPECTED.get(k) != v}
        # ^ The turn itself reached Anthropic's API from inside the boundary: the network is open.
        assert git('rev-parse', 'main') == tip and not any(e['kind'] == 'blocked' for e in log(auto))
        (base / 'project-t998').rmdir()
        assert signal_target.poll() is None, 'the real claude signalled a process outside its sandbox'
        js('task', 'close', auto, '--discard')
        fake.write_text('#!/bin/sh\nexport FAKE_CLAUDE_TRACE=' + str(tmp / 'fake-claude.trace') + '\nexec ' + sys.executable + ' ' + str(fake_script) + ' "$@"\n')
        configure(LAND_CONFIG, 'fake claude')

    # 32h. A dirty main checkout (docs/tasks.md "Landing"): a changed file the land does not
    # touch stays dirty and the land goes on; one the land changes too refuses it, naming only
    # that file, and the ledger named it before the press. The message keeps the path's '/'.
    mark('32h2')
    (project / '.gitignore').write_text('target/\nunrelated/\n')
    free = js('task', 'new', 'WORK free.txt')['id']
    wait_state(free, 'ready', 'failed')
    assert next(row for row in json.loads(cli('task', 'ledger').stdout)['open'] if row['id'] == free)['land_blocked_by'] == []
    assert land(free)['state'] == 'landed' and main_has('free.txt')
    assert git('status', '--porcelain', '--', '.gitignore') == 'M .gitignore'
    wait_closed(free)
    (project / 'README').write_text('dirty\n')
    overlap = js('task', 'new', 'WORK README')['id']
    wait_state(overlap, 'ready', 'failed')
    row = next(row for row in json.loads(cli('task', 'ledger').stdout)['open'] if row['id'] == overlap)
    assert row['land_blocked_by'] == ['README'], row
    refused = land(overlap, ok=False)['message']
    assert 'uncommitted changes in README that this land changes too' in refused, refused
    assert '.gitignore' not in refused and 'scratch.txt' not in refused, refused
    named = re.search(r'stash them in (\S+), then land again', refused)
    assert named and named.group(1).startswith('/') and os.path.realpath(named.group(1)) == os.path.realpath(project), refused
    assert show(overlap)['state'] == 'ready' and (project / 'README').read_text() == 'dirty\n'
    js('task', 'close', overlap, '--discard')
    git('checkout', '--', 'README', '.gitignore')

    # 32o. A check-run of a check that prints more than the broker's 3 MiB answer (docs/tasks.md
    mark('32o')
    # "Checks, revisions and report"): the worker gets its exit code and the end of its output,
    # whose first line names the log in the task folder that holds all of it.
    configure(LAND_CONFIG + '  scenarios:\n    loud: yes check-line | head -c 4000000; echo loud-end; exit 3\n', 'a loud scenario')
    loud = js('task', 'new', 'CHECKRUN_WITH loud WORK loud.txt')['id']
    assert wait_state(loud, 'ready', 'failed')['state'] == 'ready'
    code, said = [t for t in traces() if 'check_run_with' in t][-1]['check_run_with'][0]
    assert code == 1 and 'check 2 `yes check-line' in said and ': exit 3' in said, said
    assert said.rstrip().endswith('check-line\nchecloud-end') and len(said) < 16 * 1024, said[-300:]
    cut = re.search(r'\[the first (\d+) bytes are cut; the whole output is in (\S+)\]', said)
    whole = Path(show(loud)['log']).parent / 'check-run-2.log'
    assert cut and os.path.samefile(cut.group(2), whole) and whole.stat().st_size == 4000000 + len('loud-end\n'), said[:300]
    js('task', 'close', loud, '--discard')
    configure(LAND_CONFIG, 'no loud scenario')

    # 32oa. The same failure in two tasks (docs/tasks.md "Checks, revisions and report", "Shared
    # failure"): the second task's check-run answer says it is likely on main, the manager gets
    # one event naming both, a third task adds none, and a different line posts nothing.
    mark('32oa')
    configure(LAND_CONFIG + '  scenarios:\n    shared: \'echo "FAIL: after Back the page is $$ items"; exit 1\'\n'
              '    other: \'echo "FAIL: another thing broke"; exit 1\'\n', 'shared failure scenarios')

    def shared_run(scenario, name):
        n = js('task', 'new', 'CHECKRUN_WITH %s WORK %s.txt' % (scenario, name))['id']
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', log(n)
        code, said = [t for t in traces() if 'check_run_with' in t][-1]['check_run_with'][0]
        assert code == 1 and ': exit 1' in said, said
        return n, said

    def shared_events():
        return events_with('checks fail the same way')

    first, said = shared_run('shared', 'sf1')
    assert 'likely on main' not in said and not shared_events(), (said, shared_events())
    second, said = shared_run('shared', 'sf2')
    assert 'fails in #%d too: `FAIL: after Back' % first in said and 'not yours.' in said, said
    wait(lambda: shared_events(), 'the shared failure event', 30)
    event = shared_events()[0]['text']
    assert 'in #%d, #%d, likely on main' % (first, second) in event and re.search(r'ends with `FAIL: after Back the page is [1-9]\d+ items`', event), event
    third, said = shared_run('shared', 'sf3')
    assert 'fails in #%d, #%d too' % (first, second) in said, said
    time.sleep(2)
    assert len(shared_events()) == 1, shared_events()
    differ, said = shared_run('other', 'sf4')
    time.sleep(2)
    assert 'likely on main' not in said and len(shared_events()) == 1, (said, shared_events())
    for n in (first, second, third, differ):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no shared failure scenarios')

    # 32ob. Output shaped like a ci-scenario.py log (`<job>\t<step>\t<time> <text>`, GitHub's
    # generic `##[error]Process completed` last): two different asserts share only that tail, so
    # no event; the same assert with other times is one.
    mark('32ob')
    # ^ One command for all three tasks (the key holds the command); a counter file picks the assert
    # (the second run differs: the worker writes its WORK file after its check-run), the date gives
    # each run other times.
    stamp = r'web scenario\tRun the web scenario\t$(date -u +%Y-%m-%dT%H:%M:%S).123456Z'
    count = work / 'cilog.count'
    configure(LAND_CONFIG + '  scenarios:\n    cilog: \'echo x >> %s; n=$(wc -l < %s); if [ $n -eq 2 ]; then m="ValueError: diagram has no node"; '
              'else m="NameError: name qrows is not defined at line 4011"; fi; '
              'printf "%s %%s\\n%s ##[error]Process completed with exit code 1.\\n" "$m"; exit 1\'\n' % (count, count, stamp, stamp),
              'ci log scenario')
    cia, said = shared_run('cilog', 'ci1')
    cib, said = shared_run('cilog', 'ci2')
    time.sleep(2)
    assert 'likely on main' not in said and not events_with('ends with `NameError'), (said, events_with('ends with `NameError'))
    cic, said = shared_run('cilog', 'ci3')
    wait(lambda: events_with('ends with `NameError'), 'the shared event of one assert in two ci logs', 30)
    event = events_with('ends with `NameError')[0]['text']
    assert 'in #%d, #%d, likely on main' % (cia, cic) in event and 'ends with `NameError: name qrows is not defined at line 4011`' in event, event
    assert 'diagram' not in event and 'Process completed' not in event, event
    assert 'fails in #%d too' % cia in said, said
    for n in (cia, cib, cic):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no ci log scenarios')

    # 32oc. Two false readings of a ci-scenario log: the env echo `CACHE_ON_FAILURE: false` that every
    mark('32oc')
    # log holds is no FAIL line (two runs with other RuntimeError lines share nothing), and the event
    # shows the line as printed ('60 min', not the digit-collapsed key text).
    ci_tail = ('web scenario\\tSet up job\\t2026-11-10T01:02:03.123456Z   CACHE_ON_FAILURE: false\\n'
               'web scenario\\tRun the web scenario\\t2026-11-10T01:09:09.123456Z %s\\n'
               'web scenario\\tRun the web scenario\\t2026-11-10T01:09:10.123456Z ##[error]Process completed with exit code 1.\\n')
    ci_sh = work / 'cirun.sh'
    ci_count = work / 'cirun.count'
    ci_sh.write_text('echo x >> %s; n=$(wc -l < %s)\n'
                     'if [ "$n" -eq 1 ]; then m="RuntimeError: the close of the first worker reached the manager timeout"\n'
                     'elif [ "$n" -eq 2 ]; then m="RuntimeError: the idle clean merged worktree is removed timeout"\n'
                     'else m="RuntimeError: no job started in 60 min of queue"; fi\n'
                     'printf "%s" "$m"; exit 1\n' % (ci_count, ci_count, ci_tail))
    configure(LAND_CONFIG + '  scenarios:\n    cirun: \'sh %s\'\n' % ci_sh, 'ci run scenario')
    before = len(shared_events())
    # ^ The events of 32oa and 32ob stay in the manager's log: count from here.
    cra, said = shared_run('cirun', 'cr1')
    crb, said = shared_run('cirun', 'cr2')
    time.sleep(2)
    assert 'likely on main' not in said and not events_with('RuntimeError') and len(shared_events()) == before, (said, shared_events())
    crc, said = shared_run('cirun', 'cr3')
    time.sleep(2)
    assert 'likely on main' not in said and len(shared_events()) == before, (said, shared_events())
    crd, said = shared_run('cirun', 'cr4')
    wait(lambda: events_with('no job started'), 'the shared event of the queue line', 30)
    event = events_with('no job started')[0]['text']
    assert 'in #%d, #%d, likely on main' % (crc, crd) in event and 'ends with `RuntimeError: no job started in 60 min of queue`' in event, event
    assert 'in 0 min' not in event, event
    for n in (cra, crb, crc, crd):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no ci run scenarios')

    # 32od. Scenarios by changed paths (docs/tasks.md "Scenarios"): a done result runs, unasked,
    # each scenario whose `scenarioPaths` name a path the task changed, and records its pass; a change that
    # only `scenarioFree` names runs none; a path no glob names runs every scenario with paths, and
    # their failure is the task's. A scenario the worker names whose paths the change does not touch
    # fails as a finding: host evidence, one manager event, no fix turn. One without paths runs only
    # by name.
    mark('32od')
    path_log, far_gate = base / 'path-scenarios.log', base / 'far-gate'
    configure(LAND_CONFIG + '  scenarioFree: [path-free.txt]\n  scenarioPaths: {near: [near.txt], far: [far.txt]}\n'
              '  scenarios:\n    near: echo near >> %s\n'
              '    far: \'%s; echo far >> %s; echo "FAIL: far assert"; exit 1\'\n'
              '    named: echo named >> %s\n' % (path_log, gate_cmd(far_gate), path_log, path_log), 'path scenarios')

    def path_runs():
        return path_log.read_text().split() if path_log.exists() else []

    def triggers(n):
        return [e['trigger'] for e in log(n) if e['kind'] == 'result']

    near = js('task', 'new', 'WORK near.txt')['id']
    done = wait_state(near, 'ready', 'failed')
    assert done['state'] == 'ready' and path_runs() == ['near'] and triggers(near) == ['step'], (path_runs(), log(near))
    assert [e for e in done['result']['result']['evidence'] if e['check'] == 'wsx task check-run near passed (recorded by the host)'], done['result']
    free = js('task', 'new', 'WORK path-free.txt')['id']
    assert wait_state(free, 'ready', 'failed')['state'] == 'ready' and not kinds(free, 'check_run'), log(free)
    assert path_runs() == ['near'], path_runs()
    # ^ The worker names `far` after its commit; its answer holds the result, then `near` runs.
    # The gate opens once the result is in, so the answer cannot reach the worker's own call.
    finding = js('task', 'new', 'CHECKRUN_LATER far WORK near.txt')['id']
    wait(lambda: kinds(finding, 'result'), 'the result before the far answer', 30)
    far_gate.touch()
    done = wait_state(finding, 'ready', 'failed')
    assert done['state'] == 'ready' and triggers(finding) == ['step'] and path_runs()[1:] == ['far', 'near'], (path_runs(), log(finding))
    proof = [e for e in done['result']['result']['evidence'] if 'check-run far failed outside the task\'s changed paths' in e['check']]
    assert len(proof) == 1 and 'FAIL: far assert' in proof[0]['check'] and 'not yours' in proof[0]['proof'], done['result']
    event = wait(lambda: events_with('#%d outside its changed paths' % finding), 'the finding event', 30)
    assert len(event) == 1 and 'ends with `FAIL: far assert`' in event[0]['text'], event
    unnamed = js('task', 'new', 'WORK misc.txt')['id']
    assert wait_state(unnamed, 'ready', 'failed')['state'] == 'ready', log(unnamed)
    assert sorted(path_runs()[3:]) == ['far', 'near'] and triggers(unnamed) == ['step', 'check_run'], (path_runs(), log(unnamed))
    # ^ #finding's log has the same assert, so the turn calls it known; it is still no finding.
    assert not [e for e in show(unnamed)['result']['result']['evidence'] if 'recorded by the host' in e['check']], show(unnamed)['result']
    assert 'named' not in path_runs(), path_runs()
    for n in (near, free, finding, unnamed):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no path scenarios')

    # 32p. A turn with no result (docs/tasks.md "Claude adapter"): Claude's own 0-token turn for
    mark('32p')
    # a background-task notification is ignored; a model turn without a result is asked once.
    def result_retries(n):
        session = show(n)['record']['agent_session']
        return [t for t in traces() if t.get('session') == session
                and t.get('message', '').startswith('Your turn ended without a result.')]
    once = js('task', 'new', 'NORESULT 1 WORK once.txt')['id']
    assert wait_state(once, 'ready', 'failed')['state'] == 'ready', show(once)['result']
    assert len(result_retries(once)) == 1
    assert [(e['status'], e.get('trigger')) for e in log(once) if e['kind'] == 'result'] == [('done', 'result_retry')]
    assert any(e.get('text') == 'claude ended its own turn with no result: ignored' for e in log(once))
    twice = js('task', 'new', 'NORESULT 2 WORK twice.txt')['id']
    assert wait_state(twice, 'ready', 'failed')['state'] == 'failed'
    assert len(result_retries(twice)) == 1
    assert show(twice)['result']['result']['summary'] == 'the agent ended with success and no result'
    js('task', 'close', once, '--discard')
    js('task', 'close', twice, '--discard')

    # 32q. Parent tasks (docs/tasks.md "Parent tasks"): children land into a held parent, which
    mark('32q')
    # checks each child's commits and lands on main once, with the children's To check items.
    gate = base / 'parent-gate'
    gate.touch()
    # ^ The second check fails in the parent's copy only, when 32q puts a broken.txt there.
    configure(LAND_CONFIG.replace('"test ! -e broken.txt"', '"%s"\n    - "test ! -e broken.txt"' % gate_cmd(gate)),
              'a parent gate')
    parent = js('task', 'new', 'HV WORK parent.txt')['id']
    assert wait_state(parent, 'ready', 'failed')['state'] == 'ready', show(parent)
    assert 'cannot be used with' in cli('task', 'new', '--into', parent, '--from', 'main', 'WORK x.txt', ok=False).stderr
    first = js('task', 'new', '--into', parent, '--files', '*.txt', 'HV CAPTURE WORK child1.txt')
    assert first['base'] == git('rev-parse', 'wsx/t%d' % parent), first
    first = first['id']
    second = js('task', 'new', '--into', parent, '--files', '*.txt', 'HV WORK child2.txt')['id']
    assert show(parent)['record']['hold'] == 'parent of #%d' % first, show(parent)['record']
    record = show(first)['record']
    assert record['parent'] == parent and record['default_branch'] == 'wsx/t%d' % parent, record
    for n in (first, second):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready', show(n)
    assert 'a child takes no children' in cli('task', 'new', '--into', first, 'WORK x.txt', ok=False).stderr
    # ^ The children's '*.txt' meets the parent's parent.txt: only the sibling is an overlap.
    rows = {r['id']: r for r in js('task', 'ledger')['open']}
    assert rows[parent]['children'] == 2 and rows[first]['parent'] == parent, rows
    # ^ Where each goes (`flow`, docs/tasks.md "Where a task goes"): the parent counts its children
    # by state, each child lands into the parent.
    assert rows[parent]['flow']['phrase'].endswith('children: 2 ready'), rows[parent]['flow']
    assert rows[parent]['flow']['children'] == [{'id': first, 'state': 'ready'}, {'id': second, 'state': 'ready'}], rows[parent]['flow']
    assert rows[first]['flow']['phrase'] == 'into #%d' % parent and rows[first]['flow']['into'] == '#%d' % parent, rows[first]['flow']
    assert [o['id'] for o in rows[first]['overlaps']] == [second] and rows[parent]['overlaps'] == [], rows
    # The first child is on the parent's tip: its land fast-forwards the parent's branch, not
    # main, and the parent checks the new commit while its gate is closed.
    gate.unlink()
    assert land(first)['state'] == 'landed'
    git('cat-file', '-e', 'wsx/t%d:child1.txt' % parent)
    assert not main_has('child1.txt')
    wait(lambda: show(parent)['state'] == 'checking', 'the parent checks the landed child', 30)
    assert [(e['child'], e['commits']) for e in log(parent) if e['kind'] == 'child_landed'] == [(first, 1)], log(parent)
    busy = land(second, ok=False)
    assert 'parent #%d is busy (checking)' % parent in busy['message'] and show(second)['state'] == 'ready', busy
    for flags in ((), ('--discard',)):
        refused = cli('task', 'close', parent, *flags, ok=False).stderr
        assert 'task t%d has open child tasks #%d' % (parent, second) in refused, refused
    gate.touch()
    assert wait_state(parent, 'ready', 'failed')['state'] == 'ready' and show(parent)['record']['revision'] == 2
    assert 'has open child tasks #%d' % second in land(parent, ok=False)['message']
    # ^ The held parent that is ready again because a child landed with its checks passed wakes
    # no manager turn (`state_wakes`); its first ready, from its own worker, did.
    def ready_short(n, revision):
        event = wait(lambda: events_with('#%d ready r%d' % (n, revision)), 'the ready event of r%d' % revision)[0]
        # ^ A message of several events holds one short per line (r1 and r2 of the parent can share
        # one): this task's only, oldest first.
        return [x for x in event['short'] if x.get('task') == n]
    assert ready_short(parent, 1)[0].get('wakes', True), 'the held parent\'s own first ready wakes'
    assert ready_short(parent, 2)[-1] == {'text': '#%d ready' % parent, 'task': parent, 'wakes': False}, ready_short(parent, 2)
    # The second child rebases onto the moved parent, is checked, lands, and the parent checks again;
    # with a broken.txt in the parent's copy its checks fail, and that wakes.
    broken = Path(show(parent)['record']['working_copy']) / 'broken.txt'
    broken.touch()
    assert 'checks run on the new revision' in land(second)['message']
    assert wait_state(second, 'landed', 'ready', 'failed')['state'] == 'landed', log(second)
    assert wait_state(parent, 'failed')['state'] == 'failed' and show(parent)['record']['revision'] == 3
    failed_event = wait(lambda: events_with('#%d failed r3' % parent), 'the failed event of r3')[0]
    assert [x.get('wakes', True) for x in failed_event['short'] if x.get('task') == parent][-1], failed_event
    broken.unlink()
    assert js('task', 'add', parent, 'HV WORK again.txt')['ok']
    wait(lambda: (r := show(parent))['state'] == 'ready' and r['record']['revision'] == 4, 'the parent r4', 60)
    assert ready_short(parent, 4)[-1].get('wakes', True), ready_short(parent, 4)
    for n in (first, second):
        wait_closed(n)
    ours = lambda: [i for i in js('task', 'check')['open'] if i['task'] in (parent, first, second)]
    assert ours() == [], ours()
    assert land(parent)['state'] == 'landed'
    assert all(main_has(name) for name in ('parent.txt', 'child1.txt', 'child2.txt'))
    items = ours()
    assert sorted(i['task'] for i in items) == sorted([parent, first, second]), items
    shot = next(i for i in items if i['task'] == first)
    # ^ Kept at the child's land: its working copy is gone by now.
    assert shot['capture'] == 'captures/r1-1.png', shot
    assert (Path(show(first)['log']).parent / 'captures/r1-1.png').read_bytes().startswith(b'\x89PNG')
    wait_closed(parent)
    js('task', 'check', 'skip', *[i['id'] for i in items])
    configure(LAND_CONFIG, 'no parent gate')

    # 32r. Chats (docs/tasks.md "Chats"): a chat is the manager host with a name, a goal, its own
    # log and notes. Only the owner opens, lists and closes them. A chat hears only its own tasks.
    mark('32r')
    chats_dir = Path(created['task_dir']).parent / 'chats'
    goal = base / 'chat-goal.md'
    goal.write_text('Drive the billing rewrite.\n')
    registry = config_dir / 'config-v2.toml'
    registry_before = registry.read_text()
    with open(registry, 'a') as out:
        out.write('[[agents]]\nname = "haiku-chat"\ncommand = ["claude"]\nadapter = "claude"\nmodel = "haiku"\n')

    def chats():
        return {c['name']: c for c in js('task', 'chat', 'list')['chats']}

    def chat_entries(name):
        path = chats_dir / name / 'log.jsonl'
        return [unquoted(json.loads(line)) for line in path.read_text().splitlines()] if path.exists() else []

    def chat_reply(name, message_id):
        return wait(lambda: next((e for e in chat_entries(name) if e['kind'] == 'reply' and message_id in e['to']), None),
                    'reply of chat %s to %s' % (name, message_id), 60)

    assert 'lowercase letter' in cli('task', 'chat', 'new', 'Bad_Name', ok=False).stderr
    assert 'first chat' in cli('task', 'chat', 'new', 'manager', ok=False).stderr
    # ^ A chat's agent is an [[agents]] entry, checked before the chat is opened.
    assert 'no [[agents]] entry' in cli('task', 'chat', 'new', 'odd', '--agent', 'nope', ok=False).stderr
    assert not (chats_dir / 'odd').exists()
    opened = js('task', 'chat', 'new', 'billing', '--goal', goal, '--agent', 'haiku-chat')
    assert opened['state'] == 'open' and (chats_dir / 'billing/goal.md').read_text() == goal.read_text(), opened
    assert json.loads((chats_dir / 'billing/chat.json').read_text()) == {'agent': 'haiku-chat'}
    assert 'already open' in cli('task', 'chat', 'new', 'billing', '--goal', goal, ok=False).stderr
    # A new chat holds none of the project's earlier events, though the project has closed tasks.
    assert len(js('task', 'list', '--all')['tasks']) > len(js('task', 'list')['tasks'])
    assert not chat_entries('billing') and js('task', 'chat', '--chat', 'billing')['entries'] == []
    # The manager does not open a chat: it is the owner's.
    asked = js('task', 'say', 'RUN wsx task chat new qa --goal %s -p demo --json' % goal)
    text = reply_to(asked['id'])['text']
    assert 'chat new is for the owner' in text and 'qa' not in chats(), text
    # A chat is the manager host with its goal, its own folder, session and actor; it starts
    # on say. It runs with the manager's policy: only the owner's `chat new` and another project's
    # work (the first chat's) are refused to it.
    manager_task = js('task', 'new', 'WORK owner-chat.txt')['id']
    talk = js('task', 'say', '--to', 'billing',
              'RUN wsx task new "WORK chat.txt" -p demo --json\n'
              'RUN wsx task chat new more --goal %s -p demo --json\n'
              'RUN wsx task routine list -p other --json' % goal)
    text = chat_reply('billing', talk['id'])['text']
    assert 'chat new is for the owner' in text and "only a project's manager acts in another project" in text, text
    chat_task = max(t['id'] for t in js('task', 'list')['tasks'])
    assert show(chat_task)['record']['created_by'] == 'chat:billing', show(chat_task)['record']
    chat_launch = next(t for t in traces() if t.get('launch') and t.get('actor') == 'chat:billing')
    assert chat_launch['instructions'].startswith('You are billing, a chat of the project demo'), chat_launch
    assert 'Drive the billing rewrite.' in chat_launch['instructions'] and '/chats/billing/notes' in chat_launch['instructions']
    assert chat_launch['model'] == 'haiku', chat_launch
    assert chat_launch['wsx_env'] == ['WSX_AGENT_TREE', 'WSX_TASK_ACTOR', 'WSX_TASK_BROKER'], chat_launch
    assert chats()['billing']['running'], chats()
    # ^ chat list and the ledger show the chat's own agent: the entry and its adapter and model.
    assert (chats()['billing']['agent'], chats()['billing']['model'], chats()['billing']['adapter']) == ('haiku-chat', 'haiku', 'claude'), chats()
    assert next(c for c in js('task', 'ledger')['chats'] if c['name'] == 'billing')['model'] == 'haiku'
    assert 'billing open haiku-chat' in cli('task', 'chat', 'list').stdout
    # Only the creating chat hears its task's events; the manager hears its own.
    assert wait_state(chat_task, 'ready', 'failed')['state'] == 'ready'
    assert wait_state(manager_task, 'ready', 'failed')['state'] == 'ready'
    wait(lambda: any(e['kind'] == 'message' and e['from'] == 'events' and '#%d ready' % chat_task in e['text']
                     for e in chat_entries('billing')), 'the chat hears its task', 60)
    wait(lambda: events_with('#%d ready' % manager_task), 'the manager hears its task')
    time.sleep(4)
    assert not events_with('#%d ' % chat_task), [e['text'] for e in manager_entries() if e['kind'] == 'message']
    assert not any('#%d ' % manager_task in e.get('text', '') for e in chat_entries('billing') if e['kind'] == 'message')
    # An approval a chat's agent asks belongs to that chat (docs/tasks.md "Chats"): only its view
    # lists it, and its host hears the decision; the first chat does neither.
    asked = chat_reply('billing', js('task', 'say', '--to', 'billing',
                                     'RUN wsx task close %d --discard -p demo --json' % chat_task)['id'])['text']
    assert 'needs the owner' in asked, asked
    request = next(p['id'] for p in js('task', 'ledger')['approvals'] if p['action'] == 'discard' and p.get('task') == chat_task)
    assert js('task', 'chat', '--chat', 'billing')['approvals'] == [request] and js('task', 'chat')['approvals'] == []
    cli('task', 'deny', request, '-p', 'demo')
    wait(lambda: any(e['kind'] == 'message' and e['from'] == 'events' and 'approval %s' % request in e['text']
                     for e in chat_entries('billing')), 'the chat hears the decision', 60)
    time.sleep(4)
    assert not events_with('approval %s' % request), [e['text'] for e in manager_entries() if e['kind'] == 'message']
    assert js('task', 'chat', '--chat', 'billing')['approvals'] == []
    # Chats talk inside the project as managers do: one answer to each message.
    asked = js('task', 'say', '--to', 'billing', "SCRIPT\nwsx task say -p demo --from-chat --json - <<'EOF'\n"
               "RUN wsx task say -p demo --to billing --from-chat --reply 'chat answer'\n"
               "RUN wsx task say -p demo --to billing --from-chat --reply 'second answer'\nEOF")
    assert chat_reply('billing', asked['id'])['text'].startswith('script -> 0')
    received = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'chat:billing'), None),
                    'the chat message at the manager', 60)
    answered = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'reply' and received['id'] in e['to']), None),
                    'the manager answers the chat', 60)
    assert "--reply 'chat answer' -> 0" in answered['text'] and 'no message from billing waits' in answered['text'], answered
    wait(lambda: [e for e in chat_entries('billing') if e['kind'] == 'message' and e['from'] == 'chat-reply:manager'
                  and e['text'] == 'chat answer'], 'the answer at the chat', 60)
    assert 'not itself' in chat_reply('billing', js('task', 'say', '--to', 'billing',
                                                    'RUN wsx task say -p demo --to billing --from-chat hi')['id'])['text']
    assert '--from-chat is for' in cli('task', 'say', '--from-chat', 'hello', ok=False).stderr
    # An idle chat's host leaves (no agent, no pane); the next say starts it again.
    wait(lambda: not chats()['billing']['running'], 'the idle chat host leaves', 60)
    back = js('task', 'say', '--to', 'billing', 'STATUS')
    assert chat_reply('billing', back['id']) and chats()['billing']['running']
    # ^ The CLI prints the rows the other chats show: the speakers' names, no Debug text.
    printed = cli('task', 'chat', '--chat', 'billing').stdout
    assert re.search(r'(?m)^\[%d\] You: STATUS$' % back['id'], printed) and 'Owner' not in printed, printed
    assert [e['id'] for e in js('task', 'chat', '--chat', 'billing')['entries'] if e['kind'] == 'message'][-1] == back['id']
    # The TUI's Tasks view lists the open chats under Open; Enter opens a chat as `m`
    # opens the manager's (docs/tasks.md "TUI"), without the manager's items.
    below = len(js('task', 'list')['tasks'])
    # ^ The manager says something in the chat: a third speaker next to the owner and the chat.
    with (chats_dir / 'billing/log.jsonl').open('a') as chat_log:
        chat_log.write(json.dumps({'unix_ms': int(time.time() * 1000), 'kind': 'message', 'id': 9000001,
                                   'from': 'chat:manager', 'text': 'MANAGER PING'}) + '\n')
    for cols in (120, 80):
        shots = tui_screens(wsx, env, project, [
            (r'WORKSPACE', b't'),
            (r'(?ms)^ Chats$.*^ billing +(?:[…!✓]\d+ +)*open +haiku · Drive the billing', b'j' * below),
            (r'\(enter\)chat', b'\r'),
            (r'(?ms)^ billing +\d\d:\d\d$.*CHAT', b'\x1b'),
            (r'\(m\)anager', b'\x1b'),
            (r'WORKSPACE', b'')], cols=cols)
        talk = shots[3]
        assert 'to check' not in talk and re.search(r'(?m)^ billing +\d\d:\d\d$', talk), talk
        # ^ The title is the chat's name and its model; the manager's message is under "<name> manager", not the bare project name.
        assert 'billing · haiku' in talk and 'demo · billing' not in talk and 'MANAGER PING' in talk, talk
        assert re.search(r'(?m)^ demo manager +\d\d:\d\d$', talk) and not re.search(r'(?m)^ demo +\d\d:\d\d$', talk), talk
    # chat close keeps the notes; the chat's tasks go back to the manager.
    (chats_dir / 'billing/notes/context.md').write_text('billing notes\n')
    assert js('task', 'chat', 'close', 'billing')['state'] == 'closed'
    assert (chats_dir / 'billing/notes/context.md').read_text() == 'billing notes\n'
    wait(lambda: not chats()['billing']['running'], 'the closed chat host leaves', 30)
    assert 'no open chat billing' in cli('task', 'say', '--to', 'billing', 'hi', ok=False).stderr
    js('task', 'add', chat_task, 'WORK chat2.txt')
    wait(lambda: events_with('#%d ready r2' % chat_task), 'a closed chat\'s task reaches the manager', 60)
    # ^ The manager cannot close a chat either; the owner's close is final until `chat new` again.
    assert 'chat close is for the owner' in reply_to(js('task', 'say', 'RUN wsx task chat close billing -p demo --json')['id'])['text']
    assert js('task', 'chat', 'new', 'keep', '--goal', goal)['state'] == 'open'
    js('task', 'chat', 'close', 'keep')
    for n in (manager_task, chat_task):
        js('task', 'close', n, '--discard')

    # 32rm. A store from before chats moves at its first open (docs/tasks.md "Chats"): `roles/research`
    # with its brief, role.json, a log and notes becomes `chats/research`; the values `role:research`
    # in a task record and the logs, and the approval lines of the removed `role` action, are rewritten
    # or dropped; the argv wsxd stored for the role's session, `manager-host <id> --role research`, starts the chat.
    mark('32rm')
    mig_store = Path(created['task_dir']).parent
    # ^ The store is opened (and moved) by the first `wsx task` that runs after the folder is there: the task is
    # made first, the old-shaped files last.
    old_task = js('task', 'new', 'WORK migrated.txt')['id']
    assert wait_state(old_task, 'ready', 'failed')['state'] == 'ready'
    old_role = mig_store / 'roles/research'
    (old_role / 'notes').mkdir(parents=True)
    (old_role / 'brief.md').write_text('Research the market.\n')
    (old_role / 'role.json').write_text(json.dumps({'by': 'owner', 'adapter': 'claude', 'model': 'haiku'}))
    (old_role / 'notes/context.md').write_text('research notes\n')
    now_ms = int(time.time() * 1000)
    (old_role / 'log.jsonl').write_text(''.join(json.dumps(line) + '\n' for line in (
        {'unix_ms': now_ms, 'kind': 'message', 'id': 1, 'from': 'owner', 'text': 'find the numbers'},
        {'unix_ms': now_ms, 'kind': 'message', 'id': 2, 'from': 'role:manager', 'text': 'manager asks research'},
        {'unix_ms': now_ms, 'kind': 'reply', 'id': 3, 'to': [1], 'text': 'the numbers are in'})))
    with open(mig_store / 'approvals.jsonl', 'a') as out:
        out.write(json.dumps({'kind': 'request', 'id': 'a-old-role', 'action': 'role', 'role': 'qa', 'brief': 'x', 'unix_ms': now_ms}) + '\n')
    (mig_store / 'search.jsonl').write_text(json.dumps({'kind': 'message', 'role': 'research', 'id': 1, 'at': 1, 'text': 'old'}) + '\n')
    record_path = mig_store / str(old_task) / 'task.json'
    record = json.loads(record_path.read_text())
    record['created_by'] = 'role:research'
    record_path.write_text(json.dumps(record))
    with (mig_store / 'manager/log.jsonl').open('a') as mig_log:
        mig_log.write(json.dumps({'unix_ms': now_ms, 'kind': 'message', 'id': 9000002, 'from': 'role:research', 'text': 'RESEARCH SAYS'}) + '\n')
    migrated = chats()
    assert 'research' in migrated and migrated['research']['state'] == 'open', migrated
    assert not (mig_store / 'roles').exists()
    new_chat = mig_store / 'chats/research'
    assert (new_chat / 'goal.md').read_text() == 'Research the market.\n' and not (new_chat / 'brief.md').exists()
    assert (new_chat / 'notes/context.md').read_text() == 'research notes\n' and not (new_chat / 'role.json').exists()
    # ^ role.json's adapter and model map to the [[agents]] entry with the same two (haiku-chat above).
    assert json.loads((new_chat / 'chat.json').read_text()) == {'agent': 'haiku-chat'}, (new_chat / 'chat.json').read_text()
    assert [(e['kind'], e.get('from')) for e in chat_entries('research')][:2] == [('message', 'owner'), ('message', 'chat:manager')]
    assert json.loads(record_path.read_text())['created_by'] == 'chat:research'
    assert show(old_task)['record']['created_by'] == 'chat:research'
    assert any(e['kind'] == 'message' and e['from'] == 'chat:research' for e in manager_entries())
    assert '"action":"role"' not in (mig_store / 'approvals.jsonl').read_text().replace(' ', '')
    assert all(a['action'] != 'role' for a in js('task', 'ledger')['approvals'])
    assert not (mig_store / 'search.jsonl').exists(), 'the search digest with a role field is dropped (rebuilt on the next search)'
    # ^ The migration is idempotent: a second run changes nothing.
    again = (new_chat / 'log.jsonl').read_bytes()
    chats()
    assert (new_chat / 'log.jsonl').read_bytes() == again
    host = subprocess.Popen([str(wsx), 'task', 'manager-host', mig_store.name, '--role', 'research'], cwd=project, env=env,
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait(lambda: chats()['research']['running'], 'the stored argv starts the migrated chat', 30)
        said = js('task', 'say', '--to', 'research', 'STATUS')
        assert chat_reply('research', said['id'])
    finally:
        host.terminate()
        host.wait(timeout=30)
    js('task', 'chat', 'close', 'research')
    js('task', 'close', old_task, '--discard')
    registry.write_text(registry_before)

    # 32v. A turn that ends while a background job runs is no missing result (docs/tasks.md
    # "Claude adapter"): the host waits for Claude's own turn after the job's notice.
    mark('32v2')
    waiting = 'the turn ended while 1 background job(s) run: waiting for their notice'
    bg = js('task', 'new', 'BACKGROUND 2 WORK background.txt')['id']
    assert wait_state(bg, 'ready', 'failed')['state'] == 'ready', show(bg)['result']
    assert result_retries(bg) == []
    assert [e['status'] for e in log(bg) if e['kind'] == 'result'] == ['done']
    assert [e.get('text') for e in log(bg)].count(waiting) == 1
    # ^ A job that never reports waits at most tasks.checkTimeoutSeconds, then the result is
    # asked for once.
    configure(LAND_CONFIG + '  checkTimeoutSeconds: 2\n', 'short background wait')
    stuck = js('task', 'new', 'BACKGROUND never WORK stuck.txt')['id']
    assert wait_state(stuck, 'ready', 'failed')['state'] == 'ready', show(stuck)['result']
    assert len(result_retries(stuck)) == 1
    texts = [e.get('text') for e in log(stuck)]
    assert waiting in texts and 'no notice from 1 background job(s) in 2 s: the turn ended without a result' in texts, texts
    assert texts.index(waiting) < texts.index('no notice from 1 background job(s) in 2 s: the turn ended without a result')
    configure(LAND_CONFIG, 'background wait back to default')
    js('task', 'close', bg, '--discard')
    js('task', 'close', stuck, '--discard')

    # 32r. Usage fields and idle rotation (docs/tasks.md "Usage", "Context rotation"): each result
    mark('32r2')
    # names its step, what started its turn and its main-conversation calls; a turn after the agent
    # was idle past tasks.agent.rotateIdleSeconds rotates a context of at least half of rotateTokens.
    configure(LAND_CONFIG + '  agent: { rotateTokens: 600000, rotateIdleSeconds: 2 }\n'
              '  workflow:\n    steps:\n      - name: plan\n        prompt: BIG PLAN\n'
              '      - name: build\n        prompt: WORK idle.txt\n', 'idle rotation')
    idler = js('task', 'new', 'NOOP')['id']
    assert wait_state(idler, 'ready', 'failed')['state'] == 'ready', show(idler)['result']
    results = lambda: [e for e in log(idler) if e['kind'] == 'result']
    # ^ 400k is under 600k and the build step came at once: no rotation yet.
    assert [(e['step'], e['trigger']) for e in results()] == [('plan', 'step'), ('build', 'step')], results()
    assert results()[0]['usage']['calls'] == 1 and results()[0]['usage']['adapter'] == 'claude', results()[0]
    assert not [e for e in log(idler) if e['kind'] == 'rotated']
    time.sleep(3)
    js('task', 'add', idler, 'NOOP')
    wait(lambda: len(results()) == 3, 't%s third result' % idler)
    assert wait_state(idler, 'ready', 'failed')['state'] == 'ready', show(idler)['result']
    rotated = [e for e in log(idler) if e['kind'] == 'rotated']
    assert len(rotated) == 1 and rotated[0]['tokens'] == 400000, rotated
    assert rotated[0]['reason'].startswith('idle '), rotated
    assert (results()[2]['step'], results()[2]['trigger']) == ('build', 'rotation'), results()[2]
    sent = [t for t in traces() if 'message' in t and t.get('manager') is False]
    handoff_asked = [t for t in sent if 'was idle past this task' in t['message']]
    fresh = [t for t in sent if 'handoff notes:\nNOTES from the fixture' in t['message'] and t['message'].endswith('NOOP')]
    assert len(handoff_asked) == 1 and fresh and handoff_asked[0]['session'] != fresh[-1]['session'], [t['message'][:80] for t in sent[-3:]]
    # ^ The usage report reads the logged fields, not its replay of the steps.
    report = subprocess.run([sys.executable, '-B', str(ROOT / 'scripts' / 'usage-report.py'),
                             str(Path(show(idler)['log']).parent.parent), '--task', str(idler)],
                            capture_output=True, text=True, check=True).stdout
    assert re.search(r'worker rotations: \d+ by size, 1 after idle', report), report[-400:]
    # ^ The manager's quotes are counted per manager (replies that open with a quote / replies).
    assert re.search(r'\| manager +\| +\d+ +\| +[1-9]\d* +\| +\d+\.\d%', report), report
    assert re.search(r'\| build +\| step +\| done', report) and re.search(r'\| build +\| rotation +\| done', report), report
    # ^ Idle with a context under half of rotateTokens (400k of 2M): the same conversation resumes.
    js('task', 'add', idler, 'BIG')
    wait(lambda: len(results()) == 4, 't%s fourth result' % idler)
    assert wait_state(idler, 'ready', 'failed')['state'] == 'ready', show(idler)['result']
    configure(LAND_CONFIG + '  agent: { rotateTokens: 2000000, rotateIdleSeconds: 2 }\n', 'idle under half')
    time.sleep(3)
    js('task', 'add', idler, 'NOOP')
    wait(lambda: len(results()) == 5, 't%s fifth result' % idler)
    assert results()[4]['trigger'] == 'message', results()[4]
    assert len([e for e in log(idler) if e['kind'] == 'rotated']) == 1, log(idler)
    js('task', 'close', idler, '--discard')
    configure(LAND_CONFIG, 'no idle rotation')

    # 32s. Keep-warm pings (docs/tasks.md "Context rotation", "Manager"): a worker that waits on
    mark('32s2')
    # its checks gets a no-op turn tasks.agent.keepWarmSeconds after its last answer, at most 3 in
    # a row; it logs `ping`, is no result and starts no step. None while the account's usage limit
    # holds. The manager pings while a task checks, and a ping is never a reply.
    configure(LAND_CONFIG.replace('test ! -e broken.txt', 'sleep 11') + '  agent: { keepWarmSeconds: 2 }\n',
              'keep warm')
    pings = lambda n: [e for e in log(n) if e['kind'] == 'ping']
    keep_alive = lambda: [t for t in traces() if str(t.get('message', '')).startswith('wsx keep-alive')]
    replies_before = len([e for e in manager_entries() if e['kind'] == 'reply'])
    js('task', 'say', 'STATUS before the pings')
    wait(lambda: len([e for e in manager_entries() if e['kind'] == 'reply']) > replies_before, 'manager reply')
    # ^ A title without LIMIT: the fake manager would meet a limit of its own on the event line.
    held = js('task', 'new', '--title', 'Held by a limit', 'LIMIT 25 NOOP')['id']
    wait(lambda: show(held)['record'].get('waiting', {}).get('on') == 'limit', 'the limit wait')
    # ^ The account is limited while this task checks: no ping, though it waits on the checks. A
    # call that reports `allowed` would end the limit (docs/tasks.md "Agents"), so this task's
    # turn reports no rate state.
    norate = tmp / 'fake-claude.trace.norate'
    norate.touch()
    cold = js('task', 'new', 'NOOP')['id']
    assert wait_state(cold, 'ready', 'failed')['state'] == 'ready', show(cold)['result']
    norate.unlink()
    assert not pings(cold), log(cold)
    done = wait(lambda: (current := show(held))['state'] in ('ready', 'failed') and current, 'held ready', 90)
    assert done['state'] == 'ready', done['result']
    # ^ After the reset its checks wait 11 s: pings at about 2, 4 and 6 s, then the cap holds.
    assert kinds(held, 'limit_wait', 'result', 'ping') == ['limit_wait', 'result', 'ping', 'ping', 'ping'], log(held)
    assert all('usage' in e for e in pings(held)), pings(held)
    results = [e for e in log(held) if e['kind'] == 'result']
    assert len(results) == 1 and results[0]['trigger'] == 'limit', results
    session = show(held)['record']['agent_session']
    assert len([t for t in keep_alive() if t['session'] == session]) == 3, keep_alive()
    # ^ The account is limited while this task checks: no ping, though it waits on the checks. A
    # Claude call that goes through lifts a limit on record (docs/tasks.md "Agents"), so the limit
    # is written after the task's step, as an owner's does, and covers its 11 s of checks.
    cold = js('task', 'new', 'NOOP')['id']
    wait_state(cold, 'checking')
    limit_file = state / 'wsx/tasks/agents/claude.json'
    limit_file.parent.mkdir(parents=True, exist_ok=True)
    limit_file.write_text(json.dumps({'until_ms': int((time.time() + 14) * 1000)}))
    assert wait_state(cold, 'ready', 'failed')['state'] == 'ready', show(cold)['result']
    assert not pings(cold), log(cold)
    limit_file.unlink(missing_ok=True)
    report = subprocess.run([sys.executable, '-B', str(ROOT / 'scripts' / 'usage-report.py'),
                             str(Path(show(held)['log']).parent.parent), '--task', str(held)],
                            capture_output=True, text=True, check=True).stdout
    assert len(re.findall(r'\| ping +\| ping ', report)) == 3, report[-600:]
    # ^ The manager turns by trigger: a ready event woke turns, a stop by request (21b) none.
    triggers = report.split('Manager turns by trigger', 1)[1].split('\n## ', 1)[0]
    assert re.search(r'\| ready +\| [1-9]', triggers) and '| stopped ' not in triggers, triggers
    entries = manager_entries()
    mpings = [e for e in entries if e['kind'] == 'ping']
    assert mpings and all(e['usage']['output_tokens'] > 0 for e in mpings), entries[-12:]
    assert [t for t in keep_alive() if t.get('manager')], keep_alive()
    # ^ At most 3 pings between two real turns; a ping never logs a reply or a delivery.
    run = 0
    for e in entries:
        run = run + 1 if e['kind'] == 'ping' else 0 if e['kind'] == 'delivered' else run
        assert run <= 3, entries[-20:]
    assert all(e['to'] for e in entries if e['kind'] == 'reply'), entries[-12:]
    # ^ The TUI (docs/tasks.md "TUI"): `u` flips the day rows from cost to tokens with each day's
    # cache hit rate, from the ledger's `usage.days[].usage`; the task's Usage line counts its pings.
    ledger = js('task', 'ledger')
    short = lambda n: str(n) if n < 1000 else '%dk' % (n // 1000) if n < 10 ** 6 else '%.1fM' % (n / 1e6)

    def today(ledger):
        """The first day's expected cost row end, token sum and cache hit rate."""
        day = ledger['usage']['days'][0]
        spent = day['usage']
        kinds = [spent[k] for k in ('input_tokens', 'cache_read_tokens', 'cache_creation_tokens', 'output_tokens')]
        assert spent['cache_read_tokens'] > 0 and spent['pings'] >= 3, spent
        rate = '%d%%' % int(100 * spent['cache_read_tokens'] / sum(kinds[:3]) + 0.5)
        return day, '$%.2f' % (sum(day['parts'].values()) / 1e6), [short(sum(kinds)), rate]
    day = today(ledger)[0]
    stamp = time.localtime(day['day_ms'] / 1000)
    label = r'^ %s +%d ' % (time.strftime('%a', stamp), stamp.tm_mday)
    rank = {'waiting': 0, 'failed': 1, 'ready': 2, 'checking': 3}
    ordered = sorted(ledger['open'], key=lambda task: rank.get(task['state'], 4))
    to_held = b'j' * [task['id'] for task in ordered].index(held) + b'\r'
    pinged = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'(?s)■ build.*\(u\)sage', b'u'),
        (r'■ cache read', to_held),
        (r'Usage ', b'')])
    costs, tokens, detail = pinged[1:]
    # ^ A manager ping can land between the reads: the TUI shows the ledger as it was before or
    # after the TUI ran, so the row must equal one of the two.
    before, after = today(ledger), today(js('task', 'ledger'))
    cost_day = next(line for line in costs.splitlines() if re.search(label, line))
    assert re.search(r'■ build.*■ manager', costs) and cost_day.rstrip().endswith((before[1], after[1])), costs
    token_day = next(line for line in tokens.splitlines() if re.search(label, line))
    assert re.search(r'■ input +■ cache read +■ cache write +■ output +hit rate', tokens) and '■ build' not in tokens, tokens
    # ^ Several pings can land while the TUI reads its screens, so the row may be a state between the two.
    unit = lambda text: float(text.rstrip('kM')) * (1e6 if text.endswith('M') else 1e3 if text.endswith('k') else 1)
    total, rate = token_day.split()[-2:]
    assert unit(before[2][0]) <= unit(total) <= unit(after[2][0]) and int(before[2][1][:-1]) <= int(rate[:-1]) <= int(after[2][1][:-1]) and '█' in token_day, (token_day, before, after)
    assert re.search(r'Usage +.* · 3 pings$', detail, re.M), detail
    for n in (held, cold):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no keep-warm pings')

    # 33. Managers talk (docs/tasks.md "Managers talk"): a manager reads another project's
    mark('33')
    # ledger and messages its manager, which answers once; nothing else crosses projects.
    # ^ Its folder is a link to the real one, as a project kept behind a symlink: wsxd stores the
    # real path, so every compare with the registry's path must treat both as one folder.
    other = base / 'other'
    (base / 'other-real').mkdir(mode=0o700)
    other.symlink_to(base / 'other-real')
    git('init', '-q', '-b', 'main', cwd=other)
    (other / 'README').write_text('other\n')
    # ^ A recap one quiet minute after its last message (docs/tasks.md "Recap").
    other_config = 'tasks:\n  minFreeGb: 0\n  manager:\n    recapMinutes: 0.2\n'
    (other / 'wsx.config.yml').write_text(other_config)
    git('add', '.', cwd=other)
    git('commit', '-q', '-m', 'start', cwd=other)
    with open(config_dir / 'config-v2.toml', 'a') as registry:
        registry.write('[[projects]]\nname = "other"\npath = ' + json.dumps(str(other)) + '\n')
    other_id = 'other-' + hashlib.sha256(str(other).encode()).hexdigest()[:8]
    other_log = state / 'wsx/tasks' / other_id / 'manager' / 'log.jsonl'
    asked = js('task', 'say', "SCRIPT\nwsx task ledger -p other --json >/dev/null && wsx task say -p other --from-chat --json - <<'EOF'\n"
               "RUN wsx task say -p demo --from-chat --reply 'first answer'\n"
               "RUN wsx task say -p demo --from-chat --reply 'second answer'\nEOF")
    assert reply_to(asked['id'])['text'].startswith('script -> 0'), reply_to(asked['id'])

    def other_entries():
        return [unquoted(json.loads(line)) for line in other_log.read_text().splitlines()] if other_log.exists() else []

    received = wait(lambda: next((e for e in other_entries() if e['kind'] == 'message' and e['from'] == 'manager:demo'), None),
                    'message at the other manager', 60)
    assert 'first answer' in received['text']
    answered = wait(lambda: next((e for e in other_entries() if e['kind'] == 'reply' and received['id'] in e['to']), None),
                    'the other manager answers', 60)
    assert "--reply 'first answer' -> 0" in answered['text'] and 'waits for an answer' in answered['text'], answered
    # ^ docs/configuration.md "Profiles": other is in the default profile, so its manager keeps the host's account.
    other_launch = next(t for t in traces() if t.get('launch') and Path(t['cwd']).resolve() == other.resolve())
    assert other_launch['claude_config_dir'] is None, other_launch
    # The TUI heads the Workspace with the active profile; P switches it, and the choice is kept
    # beside the active group (group-selection-v1.toml) for the next start.
    selection = (home / 'Library/Caches' if sys.platform == 'darwin' else work / 'k') / 'wsx/group-selection-v1.toml'
    header = []
    tui_screens(wsx, env, project, [(r'(?s) work ▾ (?!.*\bother\b).*\bdemo\b', b'P'), (r'(?s) default ▾ (?!.*\bdemo\b).*\bother\b', b'')],
                raw=header)
    assert 'profile = "default"' in selection.read_text(), selection.read_text()
    # ^ The title leads row 1: bold, on the terminal's own background, then a gap, then the
    # group chips on their chip background.
    painted = Screen(40, 120)
    painted.feed(header[0])
    cells = painted.cells()[0]
    text = ''.join(c for c, _, _ in cells)
    assert text.startswith(' work ▾  ungrouped'), text
    assert all(bold and not background for _, bold, background in cells[1:7]), cells[:10]
    assert not cells[7][2], cells[6:10]
    assert all(background for _, _, background in cells[8:19]), cells[7:20]
    # A click on the title switches the profile, as P does (the \u25be says it opens a choice).
    tui_screens(wsx, env, project, [(r'(?s) default ▾ (?!.*\bdemo\b).*\bother\b', b'\x1b[<0;3;1M\x1b[<0;3;1m'),
                                    (r'(?s) work ▾ (?!.*\bother\b).*\bdemo\b', b'')])
    assert 'profile = "work"' in selection.read_text(), selection.read_text()
    replies = wait(lambda: [e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'manager-reply:other'],
                   'reply at demo', 60)
    assert len(replies) == 1 and replies[0]['text'] == 'first answer', replies
    assert any(e['kind'] == 'sent' and e['to'] == 'other' and 'first answer' in e['text'] for e in manager_entries())
    wait(lambda: turn_with('Manager other replies (message'), 'the reply reaches the demo agent')
    crossing = js('task', 'say', 'RUN wsx task deploy -p other --json\nRUN wsx task say -p demo --from-chat hi')
    text = reply_to(crossing['id'])['text']
    assert 'a manager acts in another project only with' in text and "messages another chat, not itself" in text, text
    assert "is for a chat's agent" in cli('task', 'say', '-p', 'other', '--from-chat', 'hello', ok=False).stderr

    def other_tasks():
        return js('task', 'list', '--all', '-p', 'other')['tasks']

    def other_state(n, *states):
        return wait(lambda: (current := js('task', 'show', n, '-p', 'other'))['state'] in states and current,
                    'other t%s %s' % (n, states), 60)

    # ^ A task lands in the other project while its manager runs: the land event carries the memo.
    other_land = js('task', 'new', '-p', 'other', 'WORK o.txt MEMO other lesson')['id']
    if other_state(other_land, 'ready', 'landed', 'failed')['state'] == 'ready':
        js('task', 'land', other_land, '-p', 'other')
    wait(lambda: any(e['kind'] == 'message' and e['from'] == 'events' and '#%d landed' % other_land in e['text']
                     and 'memo: other lesson' in e['text'] for e in other_entries()), 'the other land event', 60)
    # 33r. A manager works in another project (docs/tasks.md "Managers talk"): demo's manager creates
    mark('33r')
    # a task in `other`. It runs and lands in other's repo and store, its events reach demo's chat as
    # `other #<n>` and not other's manager, its memo is a fact in demo's memory whose paths are
    # checked in other, a chat of demo and the approved actions are refused.
    def other_ids():
        return {t['id'] for t in other_tasks()}

    def b_show(n):
        return js('task', 'show', n, '-p', 'other')

    def event_lines():
        return [e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events']

    seen_ids = other_ids()
    said = js('task', 'say', 'RUN wsx task new "WORK b.txt MEMO reach lesson" -p other --json')
    assert '"id"' in reply_to(said['id'], 60)['text']
    reached = wait(lambda: other_ids() - seen_ids, 'the task in other', 60)
    n = reached.pop()
    record = b_show(n)['record']
    assert record['created_by'] == 'manager@demo' and record['project'] == 'other' and record['branch'] == 'wsx/t%d' % n, record
    assert record['project_id'] == other_id, record
    assert other_state(n, 'ready', 'failed')['state'] == 'ready', b_show(n)
    # ^ The creator reviews it: it waits ready, and the event is demo's.
    wait(lambda: next((e for e in event_lines() if 'other #%d ready' % n in e['text']), None), 'the ready event in demo', 60)
    landed = js('task', 'say', 'RUN wsx task land %d -p other --json' % n)
    reply_to(landed['id'], 60)
    other_state(n, 'landed')
    git('cat-file', '-e', 'main:b.txt', cwd=other)
    assert not main_has('b.txt'), 'landed in demo'
    event = wait(lambda: next((e for e in event_lines() if 'other #%d landed' % n in e['text'] and 'memo: reach lesson' in e['text']), None),
                 'the land event in demo', 60)
    assert any(line['text'] == 'other #%d done' % n and 'task' not in line for line in event['short']), event
    # ^ other's manager never hears of it: its host ran (`new` starts it) and heard its own land event.
    assert not [e for e in other_entries() if e['kind'] == 'message' and e['from'] == 'events'
                and '#%d ' % n in e['text'] and 'reach lesson' in e['text']], other_entries()
    # The TUI draws the same words as the chat entry's short form.
    tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'\(m\)anager', b'm'),
        (r'other #%d done' % n, b'\x1b'),
        (r'\(m\)anager', b'\x1b'),
        (r'WORKSPACE', b'')])
    # Memory: the memo is a fact in demo's scope, verified by other's task, its paths checked in other.
    hit = wait(lambda: next((h for h in memory_records(cli('memory', 'search', 'reach lesson', '--json').stdout)
                             if h['kind'] == 'fact'), None), 'the memo as a fact in demo')
    fact = json.loads(cli('memory', 'get', hit['slug'], '--json').stdout)
    other_tip = git('rev-parse', 'main', cwd=other)
    assert fact['verified_by'] == 'task:other#%d' % n and fact['source'] == 'task:other#%d' % n, fact
    assert fact['paths'] == ['b.txt'] and fact['commit'] == other_tip, (fact, other_tip)
    assert [h for h in json.loads(cli('memory', 'search', 'reach lesson', '--scope', other_id, '--json').stdout) if h['kind'] == 'fact']

    def stale():
        found = next(h for h in memory_records(cli('memory', 'search', 'reach lesson', '--json').stdout) if h['slug'] == fact['slug'])
        return bool(found.get('stale'))
    # ^ A change in demo to a file of the same name moves nothing; one in other marks the fact stale.
    assert not stale()
    (project / 'b.txt').write_text('a\n')
    git('add', 'b.txt')
    git('commit', '-q', '-m', 'a b.txt')
    assert not stale()
    (other / 'b.txt').write_text('changed\n')
    git('add', 'b.txt', cwd=other)
    git('commit', '-q', '-m', 'change b.txt', cwd=other)
    assert stale()
    # Refusals: a chat of demo, then the manager's discard, deploy and an unregistered project.
    js('task', 'chat', 'new', 'reachr', '--goal', goal)
    before = other_ids()
    talk = js('task', 'say', '--to', 'reachr',
              'RUN wsx task new "WORK z.txt" -p other --json\nRUN wsx task say -p other --from-chat hi')
    text = chat_reply('reachr', talk['id'])['text']
    assert text.count("only a project's manager acts in another project") == 2, text
    refused = js('task', 'say', 'RUN wsx task close %d --discard -p other --json\nRUN wsx task deploy -p other --json\n'
                 'RUN wsx task new "WORK z.txt" -p ghost --json' % n)
    text = reply_to(refused['id'], 60)['text']
    assert text.count('a manager acts in another project only with') == 2 and "project 'ghost' not found" in text, text
    assert other_ids() == before
    # A task of demo's own by its manager is as before: `manager`, not `manager@`.
    mine = js('task', 'say', 'RUN wsx task new "WORK own.txt" -p demo --json')
    reply_to(mine['id'], 60)
    own_task = max(t['id'] for t in js('task', 'list', '--all')['tasks'])
    assert show(own_task)['record']['created_by'] == 'manager', show(own_task)['record']
    js('task', 'close', own_task, '--discard')
    js('task', 'chat', 'close', 'reachr')
    reach_task = n
    # ^ Demo's manager left a landed task in other: the recap and the task counts below include it.
    # 33s. Search (docs/tasks.md "Search"): a manager finds an old decision across projects, ranked,
    mark('33s')
    # and quotes it by id; the digest keeps what was said when the log goes; a hit quotes by id
    # only in the project the search ran in.
    # ^ The rolls were deleted above, and with them the `rotated` entry that fill() wrote (it sits
    # in the first roll, docs/tasks.md "Manager"): a rotation ends the conversation the earlier
    # messages belong to, so the current log gets one more, as the host writes it.
    with manager_log.open('a') as out:
        out.write(json.dumps({'kind': 'rotated', 'tokens': 400000, 'unix_ms': int(time.time() * 1000)}) + '\n')
    entries = manager_entries()
    last_rotation = max(i for i, e in enumerate(entries) if e['kind'] == 'rotated')

    def words_of(text):
        return set(re.findall(r'[a-z]{6,}', text.lower()))

    spoken = [e for e in entries if e['kind'] in ('message', 'reply')]
    old, rare = next((e, sorted(words_of(e['text']) - set().union(*(words_of(o['text']) for o in spoken if o is not e)))[0])
                     for e in entries[:last_rotation]
                     if e['kind'] == 'message' and e['from'] == 'owner'
                     and words_of(e['text']) - set().union(*(words_of(o['text']) for o in spoken if o is not e)))
    found = js('task', 'search', '-p', 'demo', rare)
    mine = next((h for h in found['hits'] if h['reference'] == 'demo:%d' % old['id']), None)
    assert mine and mine['quote'] == '> message %d' % old['id'] and mine['kind'] == 'message', found['hits']
    assert found['projects'] == 1 and found['problems'] == [], found
    saved = js('task', 'say', '-p', 'other', 'We keep the marmalade quokka retries at seven.')
    registry_path = config_dir / 'config-v2.toml'
    registry_before = registry_path.read_text()
    third = base / 'third'
    third.mkdir(mode=0o700)
    registry_path.write_text(registry_before + '[[projects]]\nname = "third"\npath = ' + json.dumps(str(third)) + '\n')
    third_id = 'third-' + hashlib.sha256(str(third).encode()).hexdigest()[:8]
    third_log = state / 'wsx/tasks' / third_id / 'manager' / 'log.jsonl'
    third_log.parent.mkdir(parents=True, mode=0o700)

    def third_says(*lines):
        third_log.write_text(''.join(json.dumps(line, separators=(',', ':')) + '\n' for line in lines))

    third_says({'unix_ms': 1_700_000_000_000, 'kind': 'message', 'id': 1, 'from': 'owner',
                'text': '> an older line\n\nThe zebra ledger rule is decided: marmalade stays out of it.'},
               {'unix_ms': 1_700_000_001_000, 'kind': 'reply', 'id': 2, 'to': [1], 'text': 'Noted: the zebra ledger rule.'},
               {'unix_ms': 1_700_000_002_000, 'kind': 'reply', 'id': 3, 'to': [1], 'text': 'zebra quiet', 'quiet': True},
               {'unix_ms': 1_700_000_003_000, 'kind': 'message', 'id': 4, 'from': 'events', 'text': 'zebra event'},
               {'unix_ms': 1_700_000_004_000, 'kind': 'agent', 'text': 'zebra tool step'})
    found = js('task', 'search', '-p', 'demo', '--all', 'zebra', 'marmalade')
    refs = [(h['reference'], h.get('quote')) for h in found['hits']]
    assert refs[0][0] == 'third:1' and ('third:2', None) in refs and ('other:%d' % saved['id'], None) in refs, refs
    assert not any(h['reference'] in ('third:3', 'third:4') for h in found['hits']), refs
    assert found['hits'][0]['line'] == 'The zebra ledger rule is decided: marmalade stays out of it.', found['hits'][0]
    assert found['projects'] == 3 and found['docs'] > 3, found
    assert 'an older line' not in json.dumps(js('task', 'search', '-p', 'demo', '--all', 'older'))
    assert 'third:1' in [h['reference'] for h in js('task', 'search', '-p', 'demo', '--all', 'ZEBRA')['hits']]
    # ^ The log was replaced (a roll that deleted the old file): the digest still holds the old
    # words, and a rebuild from the logs that exist drops them (docs/tasks.md "Search"). Pairs of
    # characters are the words of Korean: a particle after the word still matches.
    third_says({'unix_ms': 1_700_000_005_000, 'kind': 'message', 'id': 5, 'from': 'owner', 'text': '배포 결정은 월요일로 합니다'})
    assert js('task', 'search', '-p', 'demo', '--all', '결정을')['hits'][0]['reference'] == 'third:5'
    assert 'third:1' in [h['reference'] for h in js('task', 'search', '-p', 'demo', '--all', 'zebra')['hits']]
    assert 'third:1' not in [h['reference'] for h in js('task', 'search', '-p', 'demo', '--all', '--reindex', 'zebra')['hits']]
    assert js('task', 'search', '-p', 'demo', '--all', '결정을')['hits'][0]['reference'] == 'third:5'
    # ^ A manager runs it in its boundary, and quotes the old message by id from the hit.
    asked = js('task', 'say', 'RUN wsx task search -p demo --all --limit 3 %s' % rare)
    searched = reply_to(asked['id'])['text']
    assert 'demo:%d  > message %d' % (old['id'], old['id']) in searched, searched
    quoted = js('task', 'say', 'QUOTE %d Yes, that one.' % old['id'])
    answer = reply_to(quoted['id'])['text']
    assert answer.startswith('> ' + old['text'].splitlines()[0][:40]) and answer.endswith('Yes, that one.'), answer
    # ^ A task's story (docs/tasks.md "Search"): the log of a landed and of a closed task, written
    # as the host writes it. A word of a later add finds the task with its outcome, and
    # `--task` lists its start, add, result, checks and landing in order.
    t0 = 1_700_100_000_000

    def third_task(n, task_state, revision, events):
        folder = third_log.parent.parent / str(n)
        folder.mkdir(mode=0o700)
        (folder / 'task.json').write_text(json.dumps({
            'id': n, 'project': 'third', 'project_id': third_id, 'project_path': str(third), 'default_branch': 'main',
            'base': 'abc', 'branch': 'wsx/t%d' % n, 'working_copy': str(third), 'pane_id': None, 'adapter': 'claude',
            'model': 'm', 'checks': [], 'agent_session': None, 'state': task_state, 'revision': revision,
            'created_unix_ms': t0, 'updated_unix_ms': t0}))
        (folder / 'log.jsonl').write_text(''.join(json.dumps(e, separators=(',', ':')) + '\n' for e in events))
        return folder / 'log.jsonl'

    def stamp(ms):
        return time.strftime('%Y-%m-%d %H:%M', time.gmtime(ms // 1000))

    landed_log = third_task(1, 'landed', 2, [
        {'unix_ms': t0, 'kind': 'created', 'base': 'abc', 'branch': 'wsx/t1'},
        {'unix_ms': t0 + 1000, 'kind': 'message', 'mode': 'prompt', 'text': 'Fix the quolloquy parser'},
        *({'unix_ms': t0 + 2000 + i, 'kind': 'agent', 'text': 'tool step %d' % i} for i in range(3)),
        {'unix_ms': t0 + 3000, 'kind': 'result', 'status': 'done', 'summary': 'First pass: the parser is fixed.'},
        {'unix_ms': t0 + 4000, 'kind': 'message', 'mode': 'add', 'text': 'Also keep the zymurgy case small'},
        {'unix_ms': t0 + 5000, 'kind': 'checks', 'results': [{'command': 'cargo test', 'exit': 1, 'timed_out': False, 'output': ''}]},
        {'unix_ms': t0 + 6000, 'kind': 'revision', 'revision': 2, 'commit': 'cd67238e1f90aaaa'},
        {'unix_ms': t0 + 7000, 'kind': 'landed', 'commit': 'cd67238e1f90aaaa'}])
    third_task(2, 'closed', 1, [
        {'unix_ms': t0, 'kind': 'created', 'base': 'abc', 'branch': 'wsx/t2'},
        {'unix_ms': t0 + 1000, 'kind': 'message', 'mode': 'prompt', 'text': 'Try the gizmo idea'},
        {'unix_ms': t0 + 2000, 'kind': 'message', 'mode': 'steer', 'text': 'Drop the gizmo, it is a dead end'},
        {'unix_ms': t0 + 3000, 'kind': 'closed', 'discarded': True}])
    day = time.strftime('%m-%d', time.gmtime((t0 + 7000) // 1000))
    hit = next(h for h in js('task', 'search', '-p', 'demo', '--all', 'zymurgy')['hits'] if h['reference'] == 'third#1')
    assert hit['kind'] == 'history' and hit['by'] == 'add' and hit['outcome'] == 'landed %s cd67238e1f90' % day, hit
    hit = next(h for h in js('task', 'search', '-p', 'demo', '--all', 'dead', 'end')['hits'] if h['reference'] == 'third#2')
    assert hit['by'] == 'steer' and hit['outcome'] == 'closed: discarded', hit
    story = js('task', 'search', '-p', 'third', '--task', '1')
    assert [e['label'] for e in story['entries']] == ['start', 'prompt', 'steps', 'result', 'add', 'checks', 'revision', 'landed'], story
    assert [e['text'] for e in story['entries']][2:] == ['3 steps, last: tool step 2', 'First pass: the parser is fixed.',
                                                         'Also keep the zymurgy case small', '1 failed: cargo test', 'r2 cd67238e1f90', 'cd67238e1f90'], story
    assert story['outcome'] == 'landed %s cd67238e1f90' % day and story['title'] == 'Fix the quolloquy parser' and story['skipped'] == 0, story
    assert story['entries'][4]['unix_ms'] == t0 + 4000
    printed = cli('task', 'search', '-p', 'third', '--task', '1').stdout.splitlines()
    assert printed[0] == 't1 [landed %s cd67238e1f90]  Fix the quolloquy parser' % day, printed
    assert printed[5] == '%s add      Also keep the zymurgy case small' % stamp(t0 + 4000), printed
    assert js('task', 'search', '-p', 'third', '--task', '1', '--limit', '2')['skipped'] == 6
    # ^ Words with --task keep only that task's hits; a line the log gains later is found by the next
    # search without a rebuild, and the lines read before are not found twice.
    assert [h['reference'] for h in js('task', 'search', '-p', 'third', '--task', '1', 'gizmo', 'zymurgy')['hits']] == ['third#1']
    with landed_log.open('a') as handle:
        handle.write(json.dumps({'unix_ms': t0 + 9000, 'kind': 'message', 'mode': 'add', 'text': 'A tumbleweed note'}, separators=(',', ':')) + '\n')
    assert [h['reference'] for h in js('task', 'search', '-p', 'third', '--task', '1', 'tumbleweed')['hits']] == ['third#1']
    assert len(js('task', 'search', '-p', 'third', '--task', '1', 'zymurgy')['hits']) == 1
    assert js('task', 'search', '-p', 'third', '--task', '1', '--reindex', 'tumbleweed')['docs'] == 8
    # ^ A search of nothing, and of a project with no history, says so.
    assert 'needs some words' in cli('task', 'search', '-p', 'demo', '!!', ok=False).stderr
    registry_path.write_text(registry_before)
    shutil.rmtree(third_log.parent.parent)
    # 34. Recap (docs/tasks.md "Recap"): once the other project is quiet for its 12 s after a
    mark('34')
    # land, wsx asks its manager for one recap with the token cost since the last one, and the
    # manager answers it in the chat.
    def recaps():
        return [e for e in other_entries() if e['kind'] == 'message' and e['from'] == 'recap']

    recap = wait(lambda: next(iter(recaps()), None), 'the recap request after a quiet period', 120)
    assert 'at most 8 lines' in recap['text'] and 'Start no' in recap['text'], recap
    assert 'Token cost since the last recap: ' in recap['text'] and 'workers ' in recap['text'], recap
    assert 'in 2 turn(s) (' in recap['text'] and '#%d ' % other_land in recap['text'] and '#%d ' % reach_task in recap['text'], recap
    wait(lambda: next((e for e in other_entries() if e['kind'] == 'reply' and recap['id'] in e['to']), None),
         'the manager answers the recap', 60)
    # 35. Idle task (docs/tasks.md "Workflow"): a quiet period with no change asks for no recap
    mark('35')
    # (an owner message is no change), and with no session notes newer than the last idle task
    # it starts no idle task and says nothing. A new session note starts one task, as the
    # manager; its close is a change, so one more recap comes, and the older note starts no
    # second idle task.
    assert sorted(t['id'] for t in other_tasks()) == sorted([other_land, reach_task]), other_tasks()
    idle_text = 'WORK idle.txt MEMO knowledge folded'
    (other / 'wsx.config.yml').write_text(other_config + '  workflow:\n    idle: "%s"\n' % idle_text)
    git('commit', '-q', '-am', 'idle task', cwd=other)

    def idle_tasks():
        return [t for t in other_tasks() if t.get('title') == 'Consolidate project knowledge']

    # ^ Each quiet period's look fires project.idle: a line in `looks` is a look that happened, so
    # the absence of a recap and of an idle task is read after the look that could have made them.
    looks = base / 'other-looks.log'
    js('task', 'routine', 'add', 'lookout', '-p', 'other', '--event', 'project.idle', '--no-post',
       '--arg=sh', '--arg=-c', '--arg=echo look >> %s' % looks)
    looks_count = lambda: len(looks.read_text().split()) if looks.exists() else 0
    nudge = js('task', 'say', '-p', 'other', 'STATUS')
    wait(lambda: next((e for e in other_entries() if e['kind'] == 'reply' and nudge['id'] in e['to']), None),
         'the other manager answers', 60)
    looked = looks_count()
    stays(lambda: looks_count() > looked, lambda: len(recaps()) == 1 and not idle_tasks(),
          'a look of the quiet period', 60)
    js('task', 'routine', 'delete', 'lookout', '-p', 'other')
    # ^ The owner has workflow steps; the idle task still runs as one turn.
    owner_config.write_text('tasks:\n  workflow:\n    steps:\n      - name: plan\n        prompt: PLAN only\n')
    notes = other / 'knowledge/sessions/other'
    notes.mkdir(parents=True)
    (notes / 'session-1.md').write_text('a lesson\n')
    idle = wait(idle_tasks, 'the idle task after a new session note', 150)
    assert len(idle) == 1 and len(other_tasks()) == 3 and len(recaps()) == 1, (other_tasks(), recaps())
    idle_shown = other_state(idle[0]['id'], 'ready', 'failed')
    # ^ The manager's own task: it waits for the manager's review, never lands by itself.
    assert idle_shown['state'] == 'ready' and idle_shown['record']['created_by'] == 'manager', idle_shown
    prompt = json.loads(Path(idle_shown['log']).read_text().splitlines()[1])
    assert prompt['kind'] == 'message' and prompt['text'] == idle_text, prompt
    # ^ One turn: the idle task runs no workflow steps (`task new --no-steps`).
    assert not idle_shown['record'].get('steps'), idle_shown['record']
    owner_config.unlink()
    js('task', 'close', idle[0]['id'], '--discard', '-p', 'other')
    wait(lambda: len(recaps()) == 2, 'the recap after the idle task closed', 150)
    time.sleep(3)
    assert len(idle_tasks()) == 1 and len(other_tasks()) == 3, other_tasks()
    # 36. Fallback agent (docs/tasks.md "Agents"): a usage limit that lifts after
    mark('36')
    named_project.append('demo')
    # ^ These tasks write other files; CONFIG's check wants a.txt, which main need not hold.
    FALLBACK_CONFIG = CONFIG.replace('    - "test -f a.txt"\n', '')
    # maxWaitMinutes moves the worker to the first fallback of the registry, a Pi RPC agent,
    # in a new conversation with the unfinished turn; it reports through `wsx task result -`
    # and the task stays there. The limit is on record for the machine, so the next task
    # starts on the fallback. A short limit is still waited out (part 27).
    # ^ The Pi adapter (docs/tasks.md "Pi adapter"): `extension` becomes `-e`, and the launch's own
    # PI_CODING_AGENT_DIR holds only the owner's models.json, not their settings.
    lean = work / 'lean-runner.js'
    lean.write_text('// the lean runner stands here\n')
    owner_pi = home / '.pi/agent'
    owner_pi.mkdir(parents=True)
    (owner_pi / 'models.json').write_text('{"providers": {"fake": {}}}\n')
    (owner_pi / 'settings.json').write_text('{"packages": ["npm:owner-extension"]}\n')
    with open(config_dir / 'config-v2.toml', 'a') as registry:
        registry.write('[[agents]]\nname = "fake-pi"\ncommand = ["pi"]\nadapter = "pi"\nmodel = "fake/pi-model"\n'
                       'extension = ' + json.dumps(str(lean)) + '\n')
        # ^ An entry whose `may` holds no job's capability comes first in every list below: it is no
        # full fallback and no stand-in, so a worker, the manager and a stand-in skip it. Were the
        # gate open, the task would end on its model, fake/backlog.
        registry.write('[[agents]]\nname = "fake-backlog"\ncommand = ["pi"]\nadapter = "pi"\n'
                       'model = "fake/backlog"\nmay = ["note", "handoff"]\n')
    WORKER_FALLBACK = FALLBACK_CONFIG.replace('    model: fake-model\n', '    model: fake-model\n    fallback: [fake-backlog, fake-pi]\n    maxWaitMinutes: 1\n')
    configure(WORKER_FALLBACK, 'fallback agent')
    # ^ Part 20 ran a Pi manager: only the Pi turns from here on are the worker's.
    traced_before = len(traces())
    switched = js('task', 'new', 'LIMIT 600 WORK fa.txt')['id']
    shown = wait_state(switched, 'ready', 'failed')
    assert shown['state'] == 'ready', shown
    assert shown['record']['adapter'] == 'pi' and shown['record']['model'] == 'fake/pi-model', shown['record']
    assert shown['record']['fallback'] is True, shown['record']
    # ^ The ledger row shows the fallback, from the record's flag.
    assert {r['id']: r for r in js('task', 'ledger')['open']}[switched]['agent']['fallback'] is True
    switch = next(e for e in log(switched) if e['kind'] == 'agent_switch')
    assert switch['to'] == 'fake-pi (pi/fake/pi-model)' and 'claude usage limit until' in switch['reason'], switch
    assert kinds(switched, 'agent_switch', 'rotated', 'result', 'limit_wait') == ['agent_switch', 'rotated', 'result'], log(switched)
    assert next(e for e in log(switched) if e['kind'] == 'result')['summary'] == 'fake pi finished the turn'
    pi_turns = [t for t in traces()[traced_before:] if t.get('adapter') == 'pi' and 'message' in t]
    assert len(pi_turns) == 1 and 'LIMIT 600 WORK fa.txt' in pi_turns[0]['message'], pi_turns
    assert 'could write no handoff notes' in pi_turns[0]['message'], pi_turns[0]
    assert any(t.get('adapter') == 'pi' and t.get('reported') == 0 for t in traces()), 'the result was not kept'
    # ^ The task's effort goes on the fallback's launch as Pi's --thinking.
    assert any(t.get('launch') and t.get('thinking') == 'high' for t in traces() if t.get('worker')), 'no --thinking on the pi launch'
    pi_launch = next(t for t in traces()[traced_before:] if t.get('adapter') == 'pi' and t.get('launch') and t.get('worker'))
    argv = pi_launch['argv']
    assert argv[argv.index('-e') + 1] == str(lean) and '--no-extensions' in argv, argv
    assert argv[argv.index('--session-dir') + 1] == str(owner_pi / 'sessions/wsx'), argv
    assert pi_launch['agent_dir'] != str(owner_pi) and pi_launch['agent_dir_files'] == ['models.json', 'policy.json'], pi_launch
    assert pi_launch['models_json'] == (owner_pi / 'models.json').read_text(), pi_launch
    limit_file = state / 'wsx/tasks/agents/claude.json'
    assert json.loads(limit_file.read_text())['until_ms'] > time.time() * 1000 + 500_000, limit_file.read_text()
    on_pi = js('task', 'new', 'WORK fb.txt')['id']
    assert wait_state(on_pi, 'ready', 'failed')['record']['adapter'] == 'pi', show(on_pi)
    assert not any(e['kind'] == 'agent_switch' for e in log(on_pi)), log(on_pi)
    # ^ A host restarted in a wait that is now past maxWaitMinutes, here since the fallback came
    # after the limit, moves to the fallback at once.
    configure(FALLBACK_CONFIG, 'no fallback yet')
    restarted = js('task', 'new', 'LIMIT 600 WORK fc.txt')['id']
    wait(lambda: show(restarted)['state'] == 'waiting', 'the worker waits on its own agent')
    assert show(restarted)['record']['adapter'] == 'claude', show(restarted)['record']
    configure(WORKER_FALLBACK, 'fallback after the limit')
    project_id = show(restarted)['record']['project_id']
    os.kill(next(int(line.split()[0]) for line in leftovers('task host %s %d' % (project_id, restarted))
                 if line.endswith('task host %s %d' % (project_id, restarted))), 9)
    # ^ The pane counts as exited once the daemon sees the host gone.
    def restart_host():
        try:
            return cli('session', 'restart', 'task-%d' % restarted)
        except AssertionError:
            return None
    wait(restart_host, 'the host restarts')
    assert wait_state(restarted, 'ready', 'failed')['record']['adapter'] == 'pi', show(restarted)
    assert any(e['kind'] == 'agent_switch' for e in log(restarted)), log(restarted)
    for n in (switched, on_pi, restarted):
        js('task', 'close', n, '--discard')
    limit_file.unlink()
    configure(CONFIG, 'no fallback')

    # 37. The manager's agent choice (docs/tasks.md "Agents"): a stand-in during a short limit, a
    # fallback's credit running out, the return to the own agent at the next turn and a budget.
    mark('37')
    agents = state / 'wsx/tasks/agents'
    limit_for = lambda seconds: limit_file.write_text(json.dumps({'until_ms': int((time.time() + seconds) * 1000)}))
    norate.touch()
    # ^ The records these sections write by hand must outlive the fake claude turns that end
    # meanwhile (the manager's events turns): a result that reports `allowed` would end them.
    with open(config_dir / 'config-v2.toml', 'a') as registry:
        registry.write('[[agents]]\nname = "fake-local"\ncommand = ["pi"]\nadapter = "pi"\n'
                       'model = "local/stand-in"\nmay = ["answer"]\n')
        registry.write('[[agents]]\nname = "fake-paid"\ncommand = ["pi"]\nadapter = "pi"\n'
                       'model = "paid/model"\nbudget_usd_per_day = 1\n')
        # ^ Another model on the paid account sets no budget: the account's budget still holds.
        registry.write('[[agents]]\nname = "fake-paid-other"\ncommand = ["pi"]\nadapter = "pi"\n'
                       'model = "paid/other"\n')
        # ^ An entry with the organizer's group is a stand-in too, and no full fallback.
        registry.write('[[agents]]\nname = "fake-organizer"\ncommand = ["pi"]\nadapter = "pi"\n'
                       'model = "local/organizer"\nmay = ["read", "answer", "backlog", "note"]\n')
        registry.write('[[agents]]\nname = "fake-pi2"\ncommand = ["pi"]\nadapter = "pi"\n'
                       'model = "other/pi-model"\n')
    with_fallback = lambda names: FALLBACK_CONFIG.replace(
        '  checks:\n', '  manager: { fallback: [%s] }\n  checks:\n' % names).replace(
        '    model: fake-model\n', '    model: fake-model\n    maxWaitMinutes: 1\n')
    # ^ A short limit is waited out, and the owner still gets words from the stand-in. The message
    # stays due: the own agent answers it after the limit and hears of the stand-in's reply.
    configure(with_fallback('fake-backlog, fake-local, fake-pi'), 'stand-in')
    # ^ The earlier fa.txt task's ready event waits for the manager: the fake takes its title
    # (`LIMIT 600 ...`) for no trigger (fake-claude-stream.py own_words), so the limit stays 15 s.
    limit_for(15)
    asked = js('task', 'say', 'STATUS during the limit')
    stood_in = wait(lambda: next((e for e in chat() if e['kind'] == 'reply' and e.get('provisional') == 'fake-local'
                                  and asked['id'] in e['to']), None), 'the stand-in reply')
    assert stood_in['text'].startswith('pi answers: '), stood_in
    call = next(t for t in reversed(traces()) if t.get('adapter') == 'pi' and t.get('launch'))
    assert call['no_tools'] and call['no_session'] and call['session'] is None and call['model'] == 'local/stand-in', call
    assert not any(t.get('adapter') == 'pi' and t.get('session') and 'STATUS during' in str(t.get('message')) for t in traces())
    answer = wait(lambda: next((e for e in chat() if e['kind'] == 'reply' and not e.get('provisional')
                                and asked['id'] in e['to']), None), 'the own agent answers after the limit', 60)
    assert answer['text'].startswith('open: ') and answer['unix_ms'] > stood_in['unix_ms'], (
        answer, json.loads((manager_log.parent / 'state.json').read_text()),
        [(e['kind'], e.get('id'), e.get('to'), e.get('text', '')[:60], e.get('provisional')) for e in manager_entries()[-12:]],
        [(t.get('adapter'), t.get('model'), t.get('session'), t.get('resume')) for t in traces() if t.get('launch') and t.get('manager')][-4:])
    told = turn_with('a stand-in model with no tools answered these')
    assert told and 'fake-local to message %d: pi answers' % asked['id'] in told['message'], told
    assert not told.get('adapter')
    # ^ Past maxWaitMinutes the manager moves to the first fallback that is no stand-in. When
    # that one's credit is out, the refusal is no chat reply. The turn after the own agent's
    # limit ends goes back to it, in a fresh conversation that holds the message.
    refusal = work / 'credit-refusal.txt'
    refusal.write_text('400 {"type":"error","error":{"type":"invalid_request_error","message":'
                       '"Your credit balance is too low to access the Anthropic API."}}\n')
    manager_state = lambda: json.loads((manager_log.parent / 'state.json').read_text())
    # A manager already held at a long limit moves when a fallback comes.
    configure(with_fallback('fake-backlog, fake-organizer'), 'no fallback for the manager yet')
    limit_for(600)
    held = js('task', 'say', 'STATUS held')
    wait(lambda: any(e['kind'] == 'reply' and e.get('provisional') and held['id'] in e['to'] for e in chat()),
         lambda: 'the stand-in answers the held message %r %r' % (
             manager_state(), [(e['kind'], e.get('reason'), e.get('text', '')[:80]) for e in manager_entries()[-10:]]))
    configure(with_fallback('fake-backlog, fake-organizer, fake-pi'), 'manager fallback')
    on_fallback = js('task', 'say', 'STATUS on the fallback')
    assert reply_to(on_fallback['id'])['text'].startswith('pi answers: '), chat()[-3:]
    assert any(e['kind'] == 'reply' and not e.get('provisional') and held['id'] in e['to']
               and e['text'].startswith('pi answers: ') for e in chat()), chat()[-4:]
    assert manager_state()['agent']['name'] == 'fake-pi', manager_state()
    # ^ The Pi adapter counts `thinking` content too: the same follow-up asks for the answer once.
    pi_thought = js('task', 'say', 'PITHINK on the fallback')
    assert reply_to(pi_thought['id'])['text'].startswith('pi answers: ')
    pi_ask = wait(lambda: next((e for e in chat() if e['kind'] == 'message' and e['from'] == 'recap' and e['id'] > pi_thought['id']
                                and 'only in your thinking' in e['text']), None), 'the Pi follow-up')
    assert reply_to(pi_ask['id'])['text'].startswith('pi answers: '), chat()[-3:]
    # ^ The Pi manager hears the same instructions, the owner's-words rule included (pi.rs `--append-system-prompt`).
    assert any(t.get('launch') and t.get('talk_rule') and not t.get('worker') and not t['no_tools'] for t in traces() if t.get('adapter') == 'pi'), 'no talk rule on the pi manager launch'
    # ^ The size limit follows the adapter that runs now (docs/tasks.md "Manager"): unset, the
    # Pi fallback rotates at 150000, where the own Claude agent keeps its conversation to 400000.
    big_fallback = js('task', 'say', 'PICTX 200000 STATUS big on the fallback')
    reply_to(big_fallback['id'])
    wait(lambda: any(e['kind'] == 'rotated' and e['tokens'] == 200000 for e in manager_entries()), 'the fallback rotated at 200000')
    no_credit = js('task', 'say', 'PIERROR %s STATUS without credit' % refusal)
    pi_limit = agents / 'pi-fake.json'
    wait(lambda: pi_limit.exists(), 'the fallback counted as limited')
    assert json.loads(pi_limit.read_text())['until_ms'] > (time.time() + 5 * 3600) * 1000, pi_limit.read_text()
    assert not any(e['kind'] == 'reply' and 'credit balance' in e['text'] for e in chat()), chat()[-3:]
    limit_file.unlink()
    back_home = reply_to(no_credit['id'])
    assert back_home['text'].startswith('open: '), back_home
    assert 'agent' not in manager_state(), manager_state()
    # ^ The fallback's refusal with the own agent still limited: back home for the wait, saying why.
    assert any(e['kind'] == 'rotated' and e.get('reason', '').startswith('back on claude: pi was refused: 400') for e in chat()), chat()[-5:]
    home_turn = turn_with('PIERROR %s' % refusal)
    assert home_turn and not home_turn.get('adapter') and 'STATUS without credit' in home_turn['message'], home_turn
    pi_limit.unlink()
    # ^ A daily budget: past it the account counts as limited until midnight and is left alone.
    configure(with_fallback('fake-paid-other'), 'budget')
    limit_for(600)
    paid = js('task', 'say', 'PICOST 2 STATUS paid')
    assert reply_to(paid['id'])['text'].startswith('pi answers: '), chat()[-3:]
    wait(lambda: (agents / 'pi-paid.json').exists(), 'the budget was spent')
    assert json.loads((agents / 'pi-paid.spend.json').read_text())['micro_usd'] == 2_000_000
    assert json.loads((agents / 'pi-paid.json').read_text())['until_ms'] > time.time() * 1000
    after_budget = js('task', 'say', 'STATUS after the budget')
    time.sleep(3)
    assert not any(e['kind'] == 'reply' and after_budget['id'] in e['to'] for e in chat()), chat()[-3:]
    limit_file.unlink()
    assert reply_to(after_budget['id'])['text'].startswith('open: '), chat()[-3:]
    # ^ A worker has no fallback by default and waits; on a fallback whose credit is out, it goes
    # home as soon as its own agent is free.
    configure(FALLBACK_CONFIG.replace('    model: fake-model\n', '    model: fake-model\n    fallback: [fake-pi]\n    maxWaitMinutes: 1\n'),
              'worker fallback')
    limit_for(600)
    # ^ The wait is at home, on the own account's record; the fake's LIMIT is the refusal the real
    # Claude would give the turn that comes home while the record holds.
    away = js('task', 'new', 'LIMIT 600 PIERROR %s WORK home.txt' % refusal)['id']
    wait(lambda: show(away)['state'] == 'waiting' and show(away)['record']['adapter'] == 'claude', 'the worker waits at home', 40)
    limit_file.unlink()
    assert wait_state(away, 'ready', 'failed')['state'] == 'ready', show(away)
    assert show(away)['record']['adapter'] == 'claude', show(away)['record']
    away_switch = next(e for e in log(away) if e['kind'] == 'agent_switch')
    assert away_switch['to'].startswith('claude ') and away_switch['reason'].startswith('back on claude: was refused: 400'), away_switch
    js('task', 'close', away, '--discard')
    # ^ A rate limit has no reset time and is backed off. Backoffs in a row on the account add up
    # from the first: past maxWaitMinutes the worker moves on to the next fallback.
    configure(FALLBACK_CONFIG.replace('    model: fake-model\n', '    model: fake-model\n    fallback: [fake-pi, fake-pi2]\n    maxWaitMinutes: 60\n'),
              'long rate limit')
    limit_for(7200)
    rate = work / 'rate-refusal.txt'
    rate.write_text('429 Too Many Requests: rate limit exceeded\n')
    since = int((time.time() - 7200) * 1000)
    pi_limit.write_text(json.dumps({'since_ms': since, 'until_ms': int(time.time() * 1000) - 1000}))
    moved = js('task', 'new', 'PIERROR %s WORK rate.txt' % rate)['id']
    switch = wait(lambda: next((e for e in log(moved) if e['kind'] == 'agent_switch'), None), 'the long rate limit moves the worker')
    assert switch['to'].startswith('fake-pi2 '), log(moved)
    assert json.loads(pi_limit.read_text())['since_ms'] == since, pi_limit.read_text()
    js('task', 'close', moved, '--discard')
    limit_file.unlink()
    for name in ('pi-fake.json', 'pi-paid.json', 'pi-other.json'):
        (agents / name).unlink(missing_ok=True)
    configure(CONFIG, 'no fallback')

    # 37b. A failed sign-in moves the manager and a worker to the first usable fallback at once, and
    # both go home on the first success once the record on the account ends (docs/tasks.md "Agents").
    # fake-claude2 is on account `claude`, which the failed sign-in rules out; fake-pi is not.
    mark('37b')
    with open(config_dir / 'config-v2.toml', 'a') as registry:
        registry.write('[[agents]]\nname = "fake-claude2"\ncommand = ["claude"]\nadapter = "claude"\nmodel = "other"\n')
    si_both = with_fallback('fake-claude2, fake-pi').replace(
        '    maxWaitMinutes: 1\n', '    maxWaitMinutes: 1\n    fallback: [fake-claude2, fake-pi]\n')
    configure(si_both, 'sign-in fallback')
    si_auth_record = agents / 'claude.auth.json'
    si_record_ended = lambda: json.loads(si_auth_record.read_text())['until_ms'] < time.time() * 1000
    assert reply_to(js('task', 'say', 'STATUS before the sign-in')['id'])['text'].startswith('open: '), chat()[-3:]
    assert 'agent' not in manager_state(), manager_state()
    si_signed_in = work / 'signed-in-fallback'
    si_signed_in.unlink(missing_ok=True)
    def si_reply(message):
        # ^ A reply that does not come says what the manager and the account records hold.
        try:
            return reply_to(message['id'])
        except RuntimeError as error:
            tail = [{k: (str(v)[:160]) for k, v in e.items() if k in ('kind', 'reason', 'text', 'to', 'tokens')}
                    for e in manager_entries()[-14:]]
            records = {f.name: f.read_text()[:200] for f in agents.iterdir() if f.suffix == '.json'}
            raise RuntimeError('%s; log %s; state %s; records %s; env %s' % (
                error, tail, {k: manager_state().get(k) for k in ('agent', 'limit')}, records,
                os.environ.get('WSX_TEST_SIGN_IN_RETRY_SECS'))) from None
    si_away = js('task', 'say', 'AUTHFAIL %s STATUS away' % si_signed_in)
    assert si_reply(si_away)['text'].startswith('pi answers: '), chat()[-3:]
    assert manager_state()['agent']['name'] == 'fake-pi', manager_state()
    si_moves = lambda: [e['reason'] for e in manager_entries() if e['kind'] == 'rotated' and 'sign-in expired' in e.get('reason', '')]
    assert si_moves() == ['claude moved to fake-pi: sign-in expired'], manager_entries()[-8:]
    assert not any(e['kind'] == 'stopped' and 'Sign in again' in e.get('reason', '') for e in manager_entries()[-8:]), manager_entries()[-8:]
    assert si_auth_record.exists(), list(agents.iterdir())
    # ^ A sign-in still missing when the record ends: the try on the own agent fails and moves it back.
    wait(si_record_ended, 'the sign-in record ends')
    si_again = js('task', 'say', 'AUTHFAIL %s STATUS again' % si_signed_in)
    assert si_reply(si_again)['text'].startswith('pi answers: '), chat()[-3:]
    assert len(si_moves()) == 2 and manager_state()['agent']['name'] == 'fake-pi', si_moves()
    # ^ The worker: the first fallback on the failed account is skipped, the task continues on fake-pi
    # in a new conversation, and the next task starts there with no call to Claude.
    # ^ A task born while the record holds starts on the fallback with no switch (si_skipped below);
    # this one must try Claude first, so the manager's record ends before it is created.
    wait(si_record_ended, 'the sign-in record ends before the worker')
    si_worker_in = work / 'worker-signed-in'
    si_worker_in.unlink(missing_ok=True)
    si_traced_before = len(traces())
    si_sw = js('task', 'new', 'WAUTHFAIL %s WORK sa.txt' % si_worker_in)['id']
    si_shown = wait_state(si_sw, 'ready', 'failed')
    assert si_shown['state'] == 'ready' and si_shown['record']['adapter'] == 'pi', si_shown
    assert si_shown['record']['fallback'] is True and si_shown['record']['home']['adapter'] == 'claude', si_shown['record']
    si_switch = next(e for e in log(si_sw) if e['kind'] == 'agent_switch')
    assert si_switch['to'] == 'fake-pi (pi/fake/pi-model)' and si_switch['reason'] == 'sign-in expired', si_switch
    assert kinds(si_sw, 'agent_switch', 'rotated', 'result') == ['agent_switch', 'rotated', 'result'], log(si_sw)
    assert not any(e['kind'] == 'result' and e['status'] == 'failed' for e in log(si_sw)), log(si_sw)
    si_pi_turns = [t for t in traces()[si_traced_before:] if t.get('adapter') == 'pi' and 'message' in t and 'WAUTHFAIL' in t['message']]
    assert len(si_pi_turns) == 1 and 'could write no handoff notes' in si_pi_turns[0]['message'], si_pi_turns
    si_skipped = js('task', 'new', 'WORK sb.txt')['id']
    assert wait_state(si_skipped, 'ready', 'failed')['record']['adapter'] == 'pi', show(si_skipped)
    assert show(si_skipped)['record']['fallback'] is True and show(si_skipped)['record']['home'] == {'adapter': 'claude', 'model': 'fake-model'}, show(si_skipped)['record']
    assert not any(e['kind'] == 'agent_switch' for e in log(si_skipped)), log(si_skipped)
    # ^ Signed in again: both go home once the record ends, on the first turn they have.
    si_signed_in.touch()
    si_worker_in.touch()
    wait(si_record_ended, 'the sign-in record ends again')
    si_home = js('task', 'say', 'AUTHFAIL %s STATUS home' % si_signed_in)
    assert si_reply(si_home)['text'].startswith('open: '), chat()[-3:]
    assert 'agent' not in manager_state(), manager_state()
    assert any(e['kind'] == 'rotated' and e.get('reason') == 'back on claude' for e in manager_entries()), manager_entries()[-6:]
    assert not si_auth_record.exists(), 'the first success clears the record'
    js('task', 'add', si_sw, 'WORK sc.txt')
    wait(lambda: show(si_sw)['record']['adapter'] == 'claude', 'the worker goes home')
    assert wait_state(si_sw, 'ready', 'failed')['state'] == 'ready', show(si_sw)
    assert not show(si_sw)['record'].get('fallback') and show(si_sw)['record']['home'] == {'adapter': 'claude', 'model': 'fake-model'}, show(si_sw)['record']
    si_back = [e for e in log(si_sw) if e['kind'] == 'agent_switch'][-1]
    assert si_back['to'].startswith('claude ') and si_back['reason'] == 'back on claude', si_back
    # ^ A task born during the record is on the fallback like a switched one and goes home the same way.
    js('task', 'add', si_skipped, 'WORK se.txt')
    wait(lambda: show(si_skipped)['record']['adapter'] == 'claude', 'the task born on the fallback goes home')
    assert wait_state(si_skipped, 'ready', 'failed')['state'] == 'ready', show(si_skipped)
    assert not show(si_skipped)['record'].get('fallback'), show(si_skipped)['record']
    assert [e['reason'] for e in log(si_skipped) if e['kind'] == 'agent_switch'] == ['back on claude'], log(si_skipped)
    for n in (si_sw, si_skipped):
        js('task', 'close', n, '--discard')
    # ^ With no usable fallback a worker does not end failed: it waits like a limit, on the sign-in
    # record's retry time, says so in its waiting reason, and resumes by itself once a try succeeds.
    configure(FALLBACK_CONFIG, 'sign-in without a fallback')
    si_alone_in = work / 'alone-signed-in'
    si_alone_in.unlink(missing_ok=True)
    si_alone = js('task', 'new', 'WAUTHFAIL %s WORK sd.txt' % si_alone_in)['id']
    waiting_reason = lambda: ((show(si_alone)['record'].get('waiting') or {}).get('reason') or '')
    wait(lambda: re.fullmatch(r'sign-in expired; tries again at \d\d:\d\d', waiting_reason()), 'the worker waits for its sign-in')
    assert show(si_alone)['state'] == 'waiting' and show(si_alone)['record']['adapter'] == 'claude', show(si_alone)
    assert show(si_alone)['record']['waiting']['on'] == 'limit', show(si_alone)['record']['waiting']
    assert not any(e['kind'] == 'result' for e in log(si_alone)), log(si_alone)
    si_alone_in.touch()
    assert wait_state(si_alone, 'ready', 'failed')['state'] == 'ready', show(si_alone)
    assert next(e for e in log(si_alone) if e['kind'] == 'result')['status'] == 'done', log(si_alone)
    assert 'limit_wait' in [e['kind'] for e in log(si_alone)], log(si_alone)
    assert not (agents / 'claude.auth.json').exists(), 'the try that went through clears the record'
    js('task', 'close', si_alone, '--discard')
    configure(CONFIG, 'no fallback')

    # 37c. The limit lifecycle's return (docs/tasks.md "Agents"): `agent::next` is the one rule of
    # where a host goes, asked before every turn and at every look of a wait, and home is the
    # task's own agent on its record. S1: a fallback that refuses the prompt (Pi without an API
    # key) makes one launch; the worker and the manager go home for the wait on their own account
    # and resume there when its record goes. S2: a working fallback goes home at the next turn
    # after its handoff notes: a task born on the fallback starts a fresh own conversation with the
    # task's prompt, one switched mid-task resumes the conversation it kept. S3: a turn with no
    # output past the bound (WSX_TEST_TURN_QUIET_FILE) ends by its own host: the worker's failed result. S4: a
    # hung fallback counts as a refusal with no reset: home for the wait. S5: a 403 block page is
    # no sign-in failure. S6: home keeps the task's own model, never the project's.
    mark('37c')
    no_key = work / 'pi-no-key.txt'
    no_key.write_text('No API key found for anthropic.\n\nUse /login to log into a provider via OAuth or API key.\n')
    configure(with_fallback('fake-pi').replace('    maxWaitMinutes: 1\n', '    maxWaitMinutes: 1\n    fallback: [fake-pi]\n'),
              'lifecycle fallbacks')
    lc_home = lambda n: show(n)['record'].get('home')
    lc_switches = lambda n: [(e['to'].split(' ')[0], e['reason']) for e in log(n) if e['kind'] == 'agent_switch']
    lc_pi_launches = lambda since: [t for t in traces()[since:] if t.get('adapter') == 'pi' and t.get('launch') and t.get('worker')]
    lc_claude_launches = lambda since: [t for t in traces()[since:] if t.get('launch') and not t.get('manager') and t.get('adapter') != 'pi']
    lc_claude_turn = lambda since, words: next(t for t in traces()[since:] if 'message' in t and t.get('adapter') != 'pi'
                                               and not t.get('manager') and words in t['message'])
    lc_rotated = lambda words: next((e for e in manager_entries() if e['kind'] == 'rotated' and e.get('reason', '').startswith(words)), None)
    lc_turn = lambda folder: json.loads((Path(folder) / 'turn.json').read_text())
    # ^ The alerts of failed turns posted after message `since` (docs/tasks.md "Manager").
    lc_alerts = lambda since, entries=None: [e for e in (entries or manager_entries()) if e['kind'] == 'reply' and e.get('post')
                                             and e.get('level') == 'bad' and e['id'] > since]
    for name in ('pi-fake.json', 'claude.auth.json'):
        (agents / name).unlink(missing_ok=True)
    # S1, the worker: born on the fallback while the limit holds; the refused prompt sends it home for
    # the wait, where the own agent's refusal (the fake's LIMIT) is waited out at home.
    limit_for(600)
    s1_before = len(traces())
    s1 = js('task', 'new', 'LIMIT 600 PIREFUSE %s WORK s1.txt' % no_key)['id']
    wait(lambda: show(s1)['state'] == 'waiting' and show(s1)['record']['adapter'] == 'claude', 'the worker waits at home', 40)
    assert lc_home(s1) == {'adapter': 'claude', 'model': 'fake-model'} and not show(s1)['record'].get('fallback'), show(s1)['record']
    assert (agents / 'pi-fake.json').exists(), 'the refused prompt counts as the fallback account limit'
    assert len(lc_switches(s1)) == 1 and lc_switches(s1)[0][0] == 'claude' \
        and lc_switches(s1)[0][1].startswith('back on claude: was refused: No API key found'), lc_switches(s1)
    assert show(s1)['record']['waiting']['on'] == 'limit', show(s1)['record']['waiting']
    assert len(lc_pi_launches(s1_before)) == 1, lc_pi_launches(s1_before)
    # S1, the manager: one Pi launch, back home for the wait, the reply once the record goes.
    (agents / 'pi-fake.json').unlink()
    s1_say = js('task', 'say', 'PIREFUSE %s STATUS without a key' % no_key)
    wait(lambda: lc_rotated('back on claude: pi was refused: No API key found'), 'the manager back home for the wait', 40)
    assert 'agent' not in manager_state() and manager_state()['limit']['account'] is True, manager_state()
    # ^ The refusal ended the turn's record with Pi's words, at once; a move to the own agent is
    # no failure the owner must hear of (docs/tasks.md "Agents", "Manager").
    s1_turn = lc_turn(manager_log.parent)
    assert s1_turn['agent'].startswith('pi/') and s1_turn['ended']['reason'].startswith('No API key found'), s1_turn
    assert not lc_alerts(s1_say['id']), lc_alerts(s1_say['id'])
    assert not any(e['kind'] == 'reply' and s1_say['id'] in e['to'] for e in chat()), chat()[-3:]
    limit_file.unlink()
    assert reply_to(s1_say['id'], 40)['text'].startswith('open: '), chat()[-3:]
    assert wait(lambda: show(s1)['state'] in ('ready', 'failed') and show(s1), 'the worker resumes at home', 40)['state'] == 'ready', show(s1)
    assert show(s1)['record']['adapter'] == 'claude' and (Path(show(s1)['record']['working_copy']) / 's1.txt').exists(), show(s1)
    assert kinds(s1, 'agent_switch', 'rotated', 'limit_wait', 'result') == ['agent_switch', 'rotated', 'limit_wait', 'result'], log(s1)
    # S2: born on the fallback, works there, goes home at the next turn after its handoff notes, in a
    # fresh own conversation that carries the task's prompt.
    (agents / 'pi-fake.json').unlink()
    limit_for(600)
    s2 = js('task', 'new', 'born on the fallback')['id']
    # ^ No WORK in the prompt: the home turn carries the prompt before the added text, and the
    # fake acts on the first WORK word only.
    assert wait_state(s2, 'ready', 'failed')['record']['adapter'] == 'pi', show(s2)
    limit_file.unlink()
    s2_before = len(traces())
    js('task', 'add', s2, 'WORK s2b.txt')
    assert wait_state(s2, 'ready', 'failed')['state'] == 'ready', show(s2)
    assert show(s2)['record']['adapter'] == 'claude' and not show(s2)['record'].get('fallback'), show(s2)['record']
    s2_handoff = [t for t in traces()[s2_before:] if t.get('adapter') == 'pi' and 'message' in t]
    assert len(s2_handoff) == 1 and 'This conversation ends after this turn and its own conversation continues' in s2_handoff[0]['message'], s2_handoff
    assert len(lc_claude_launches(s2_before)) == 1 and not lc_claude_launches(s2_before)[0]['resume'], lc_claude_launches(s2_before)
    s2_turn = lc_claude_turn(s2_before, 'WORK s2b.txt')['message']
    assert 'The task as it was asked' in s2_turn and 'handoff notes' in s2_turn, s2_turn[:400]
    assert lc_switches(s2) == [('claude', 'back on claude')], lc_switches(s2)
    assert [e.get('reason') for e in log(s2) if e['kind'] == 'rotated'] == ['back on claude'], log(s2)
    assert (Path(show(s2)['record']['working_copy']) / 's2b.txt').exists()
    # S2, a kept conversation: switched mid-task by a long limit, the task keeps its own conversation
    # and the turn it could not finish on the record, and resumes that conversation on return.
    s2r = js('task', 'new', 'kept conversation on its own agent')['id']
    # ^ No WORK in the prompt either: the fake pi commits the first WORK word's file and exits
    # when a second write of it leaves nothing to commit.
    assert wait_state(s2r, 'ready', 'failed')['record']['adapter'] == 'claude', show(s2r)
    s2r_session = show(s2r)['record']['agent_session']
    assert s2r_session, show(s2r)['record']
    js('task', 'add', s2r, 'LIMIT 600 WORK s2d.txt')
    wait(lambda: show(s2r)['record']['adapter'] == 'pi', 'the long limit moves the worker', 40)
    assert wait_state(s2r, 'ready', 'failed')['record']['adapter'] == 'pi', show(s2r)
    assert lc_home(s2r)['session'] == s2r_session and lc_home(s2r)['turn'].startswith('The usage limit has reset'), lc_home(s2r)
    limit_file.unlink()
    s2r_before = len(traces())
    js('task', 'add', s2r, 'WORK s2e.txt')
    assert wait_state(s2r, 'ready', 'failed')['state'] == 'ready' and show(s2r)['record']['adapter'] == 'claude', show(s2r)
    assert show(s2r)['record']['agent_session'] == s2r_session and lc_home(s2r) == {'adapter': 'claude', 'model': 'fake-model'}, show(s2r)['record']
    assert len(lc_claude_launches(s2r_before)) == 1 and lc_claude_launches(s2r_before)[0]['resume'] \
        and lc_claude_launches(s2r_before)[0]['session'] == s2r_session, lc_claude_launches(s2r_before)
    s2r_turn = lc_claude_turn(s2r_before, 'WORK s2e.txt')['message']
    assert s2r_turn.startswith('The usage limit has reset') and 'While your agent was unavailable, pi/fake/pi-model worked on the task' in s2r_turn, s2r_turn[:400]
    assert 'the turn it could not finish' not in s2r_turn, 'the fallback finished its turn: nothing of it is unfinished'
    assert lc_switches(s2r)[-1] == ('claude', 'back on claude') and 'claude usage limit until' in lc_switches(s2r)[0][1], lc_switches(s2r)
    assert not any(e['kind'] == 'rotated' and e.get('reason') == 'back on claude' for e in log(s2r)), 'the own conversation is resumed, not rotated'
    assert (Path(show(s2r)['record']['working_copy']) / 's2e.txt').exists()
    # S2, refused after it answered: moved again by a long limit, the fallback answers one turn and
    # is refused on the next while the own account still holds; the task goes home for the wait and
    # resumes its own conversation with the turn the fallback could not finish.
    js('task', 'add', s2r, 'LIMIT 600 WORK s2f.txt')
    wait(lambda: show(s2r)['record']['adapter'] == 'pi', 'the long limit moves the worker again', 40)
    assert wait_state(s2r, 'ready', 'failed')['state'] == 'ready' and (Path(show(s2r)['record']['working_copy']) / 's2f.txt').exists(), (show(s2r)['record'], log(s2r)[-6:])
    js('task', 'add', s2r, 'PIERROR %s WORK s2g.txt' % refusal)
    wait(lambda: show(s2r)['state'] == 'waiting' and show(s2r)['record']['adapter'] == 'claude', 'the refused fallback sends the worker home for the wait', 40)
    assert lc_switches(s2r)[-1][1].startswith('back on claude: was refused: 400') and show(s2r)['record']['agent_session'] == s2r_session, (lc_switches(s2r), show(s2r)['record'])
    s2x_before = len(traces())
    limit_file.unlink()
    assert wait(lambda: show(s2r)['state'] in ('ready', 'failed') and show(s2r), 'the worker resumes at home', 40)['state'] == 'ready', show(s2r)
    assert (Path(show(s2r)['record']['working_copy']) / 's2g.txt').exists() and show(s2r)['record']['agent_session'] == s2r_session, show(s2r)['record']
    s2x_turn = lc_claude_turn(s2x_before, 'WORK s2g.txt')['message']
    assert s2x_turn.startswith('The usage limit has reset') and 'pi/fake/pi-model was refused: 400' in s2x_turn \
        and 'the turn it could not finish:\nPIERROR' in s2x_turn, s2x_turn[:600]
    assert len(lc_claude_launches(s2x_before)) == 1 and lc_claude_launches(s2x_before)[0]['resume'], lc_claude_launches(s2x_before)
    for n in (s1, s2, s2r):
        js('task', 'close', n, '--discard')
    # S3, the worker: a turn with no output past the bound ends as the turn's failed result.
    turn_quiet = work / 'turn-quiet'
    turn_quiet.write_text('40')
    s3 = js('task', 'new', 'WHANG %s WORK s3.txt' % (work / 's3-release'))['id']
    wait(lambda: show(s3)['state'] in ('ready', 'failed'), 'the hung turn ends', 70)
    s3_result = next(e for e in log(s3) if e['kind'] == 'result')
    assert show(s3)['state'] == 'failed' and s3_result['status'] == 'failed' and s3_result['summary'].startswith('no output for'), (show(s3)['state'], s3_result)
    assert not any(e['kind'] in ('agent_switch', 'limit_wait') for e in log(s3)) and show(s3)['record']['adapter'] == 'claude', log(s3)
    # ^ The record the host ended the turn by: its bound, its deadline and the reason.
    s3_turn = lc_turn(Path(show(s3)['log']).parent)
    assert s3_turn['agent'].startswith('claude/') and s3_turn['quiet_ms'] == 40000 and s3_turn['deadline_ms'] == s3_turn['output_ms'] + 40000, s3_turn
    assert s3_turn['ended']['reason'].startswith('no output for') and s3_turn['ended']['unix_ms'] >= s3_turn['deadline_ms'], s3_turn
    js('task', 'close', s3, '--discard')
    # S4, the worker: a hung fallback counts as its refusal with no reset: its account backs off and
    # the task goes home for the wait.
    (agents / 'pi-fake.json').unlink()
    # ^ The S2 refusal backed the fallback's account off: a held fallback is no move.
    limit_for(600)
    s4 = js('task', 'new', 'LIMIT 600 PIHANG %s WORK s4.txt' % (work / 's4-release'))['id']
    wait(lambda: show(s4)['state'] == 'waiting' and show(s4)['record']['adapter'] == 'claude', 'the hung fallback sends the worker home for the wait', 80)
    assert lc_switches(s4)[0][1].startswith('back on claude: was refused: no output for'), lc_switches(s4)
    assert json.loads((agents / 'pi-fake.json').read_text())['until_ms'] < (time.time() + 3600) * 1000, 'the hung fallback backs off'
    limit_file.unlink()
    assert wait(lambda: show(s4)['state'] in ('ready', 'failed') and show(s4), 'the worker resumes at home', 40)['state'] == 'ready', show(s4)
    assert show(s4)['record']['adapter'] == 'claude' and (Path(show(s4)['record']['working_copy']) / 's4.txt').exists(), show(s4)
    js('task', 'close', s4, '--discard')
    # S4, the manager: the same, back home for the wait, the reply once the record goes.
    (agents / 'pi-fake.json').unlink()
    limit_for(600)
    s4_say = js('task', 'say', 'PIHANG %s STATUS on a hung fallback' % (work / 's4-release'))
    lc_tail = lambda: '%r %r' % (manager_state(), [(e['kind'], e.get('reason'), e.get('text', '')[:80]) for e in manager_entries()[-10:]])
    wait(lambda: lc_rotated('back on claude: pi was refused: no output for'), lambda: 'the manager back home from the hung fallback ' + lc_tail(), 80)
    assert 'agent' not in manager_state() and manager_state()['limit']['account'] is True, manager_state()
    limit_file.unlink()
    assert reply_to(s4_say['id'], 40)['text'].startswith('open: '), chat()[-3:]
    (agents / 'pi-fake.json').unlink()
    turn_quiet.unlink()
    # S5: a 403 block page ("Failed to authenticate. API Error: 403 Attention Required! | Cloudflare")
    # is no expired sign-in: no record, no move, the turn's error result.
    s5 = js('task', 'new', 'WCFAUTH %s WORK s5.txt' % (work / 's5-release'))['id']
    assert wait_state(s5, 'ready', 'failed')['state'] == 'failed', show(s5)
    assert 'Attention Required' in next(e for e in log(s5) if e['kind'] == 'result')['summary'], log(s5)
    assert 'Attention Required' in lc_turn(Path(show(s5)['log']).parent)['ended']['reason'], lc_turn(Path(show(s5)['log']).parent)
    assert not (agents / 'claude.auth.json').exists() and not any(e['kind'] == 'agent_switch' for e in log(s5)), log(s5)
    js('task', 'close', s5, '--discard')
    # S6: home is the task's own model as it started, never the project's default.
    limit_for(600)
    s6 = js('task', 'new', '--model', 'sonnet', 'WORK s6a.txt')['id']
    assert wait_state(s6, 'ready', 'failed')['record']['adapter'] == 'pi', show(s6)
    assert lc_home(s6) == {'adapter': 'claude', 'model': 'sonnet'}, show(s6)['record']
    limit_file.unlink()
    s6_before = len(traces())
    js('task', 'add', s6, 'WORK s6b.txt')
    assert wait_state(s6, 'ready', 'failed')['state'] == 'ready', show(s6)
    assert (show(s6)['record']['adapter'], show(s6)['record']['model']) == ('claude', 'sonnet'), show(s6)['record']
    assert len(lc_claude_launches(s6_before)) == 1 and lc_claude_launches(s6_before)[0]['model'] == 'sonnet', lc_claude_launches(s6_before)
    js('task', 'close', s6, '--discard')
    # S7: the reset of a limit is the rejected window's. Claude Code names the representative
    # claim at the top level (here the 7-day window, 4 days ahead) while five_hour is the window
    # that rejected: the task waits out the 45 s at home instead of moving to the fallback.
    s7 = js('task', 'new', 'LIMIT 45 REPRESENTATIVE WORK s7.txt')['id']
    wait(lambda: show(s7)['state'] == 'waiting' and show(s7)['record']['waiting']['on'] == 'limit', 'the worker waits out the five-hour reset', 40)
    s7_record = json.loads(limit_file.read_text())
    assert s7_record['window'] == 'five_hour' and s7_record['until_ms'] < (time.time() + 60) * 1000, s7_record
    assert not any(e['kind'] == 'agent_switch' for e in log(s7)) and show(s7)['record']['adapter'] == 'claude', log(s7)
    limit_file.unlink()
    assert wait_state(s7, 'ready', 'failed')['state'] == 'ready' and (Path(show(s7)['record']['working_copy']) / 's7.txt').exists(), show(s7)
    js('task', 'close', s7, '--discard')
    # S8: a manager an older host pinned to a fallback (state.json `agent`, and a `limit` with
    # `switched` whose time is long past and no `account`), as the ir project's was. (1) With the
    # own record gone it goes home at the next host start and answers the waiting message with no
    # Pi launch. (2) With the limit on record and the fallback refusing at once, it comes home for
    # the wait after one Pi launch and answers once the record goes.
    def pin_manager():
        sock = js('task', 'manager')['socket']
        cli('session', 'delete', 'task-manager', '-p', 'demo')
        wait(lambda: not Path(sock).exists() or subprocess.run(['nc', '-zU', sock]).returncode != 0, 'manager down')
        data = manager_state()
        data['agent'] = {'name': 'fake-pi', 'adapter': 'pi', 'model': 'fake/pi-model'}
        data['limit'] = {'until_ms': int(time.time() * 1000) - 3600_000, 'attempt': 0, 'ids': [], 'events': True, 'switched': True}
        (manager_log.parent / 'state.json').write_text(json.dumps(data))
        return len(traces()), int(time.time() * 1000)
    lc_pi_manager = lambda since: [t for t in traces()[since:] if t.get('adapter') == 'pi' and t.get('launch') and not t.get('worker')]
    lc_rotated_since = lambda at, words: any(e['kind'] == 'rotated' and e.get('reason', '').startswith(words) and e['unix_ms'] >= at for e in manager_entries())
    (agents / 'pi-fake.json').unlink(missing_ok=True)
    limit_file.unlink(missing_ok=True)
    s8_before, s8_at = pin_manager()
    pinned = js('task', 'say', 'STATUS pinned by an older host')
    assert reply_to(pinned['id'], 40)['text'].startswith('open: '), chat()[-3:]
    assert 'agent' not in manager_state() and lc_rotated_since(s8_at, 'back on claude'), (manager_state(), manager_entries()[-6:])
    assert not lc_pi_manager(s8_before), lc_pi_manager(s8_before)
    s8_before, s8_at = pin_manager()
    limit_for(600)
    pinned_refused = js('task', 'say', 'PIREFUSE %s STATUS pinned, the fallback refuses' % no_key)
    wait(lambda: lc_rotated_since(s8_at, 'back on claude: pi was refused: No API key found'), 'the pinned manager back home for the wait', 40)
    assert 'agent' not in manager_state() and manager_state()['limit']['account'] is True, manager_state()
    assert len(lc_pi_manager(s8_before)) == 1, lc_pi_manager(s8_before)
    assert not any(e['kind'] == 'reply' and pinned_refused['id'] in e['to'] for e in chat()), chat()[-3:]
    limit_file.unlink()
    assert reply_to(pinned_refused['id'], 40)['text'].startswith('open: '), chat()[-3:]
    (agents / 'pi-fake.json').unlink(missing_ok=True)
    configure(CONFIG, 'no fallback')
    norate.unlink()
    # 37f. The local floor is a stand-in with no entry or list (docs/tasks.md "Agents", docs/local-model.md):
    # a limit of an hour pauses the manager's conversation, the floor answers the owner in its own
    # lane when its endpoint answers, and the same conversation resumes with what was said.
    mark('37f')
    FLOOR['up'] = False
    start_floor_url = env['LOCAL_MODEL_URL']
    # ^ A record that ends past a probe's period (600 s) is probed at once, and the fake's probe
    # reports `allowed` and ends it: these records last 600 s, as the other sections' do.
    limit_for(600)
    session_before = manager_state()['agent_session']
    rotations = lambda: len([e for e in manager_entries() if e['kind'] == 'rotated'])
    rotated_before = rotations()
    floor_calls = lambda: [t for t in traces() if t.get('adapter') == 'pi' and t.get('launch') and t.get('model') == 'llama-wsx/lfm3-1.7b']
    # (b) The endpoint down: the manager waits for Claude and the chat says why.
    said_at = int(time.time() * 1000)
    f_first = js('task', 'say', 'We decided: the theme is blue. STATUS first')
    f_held = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'limit' and e.get('until')
                                and e['unix_ms'] >= said_at), None), 'the hold with the floor down')
    assert 'does not answer at ' + start_floor_url in f_held['until'], f_held
    time.sleep(3)
    assert not any(e['kind'] == 'reply' and f_first['id'] in e['to'] for e in chat()), chat()[-3:]
    assert not floor_calls()
    # (a) The endpoint comes up: the floor answers the held message in its own lane (one call, no
    # tools, no stored conversation), marked as the stand-in's, and says when Claude returns.
    FLOOR['up'] = True
    f_answer = wait(lambda: next((e for e in chat() if e['kind'] == 'reply' and e.get('provisional') == 'local-model'
                                  and f_first['id'] in e['to']), None), 'the floor stands in', 40)
    assert re.search(r'pi answers: claude is limited until \d\d:\d\d', f_answer['text']), f_answer
    f_call = floor_calls()[-1]
    assert f_call['no_tools'] and f_call['no_session'] and f_call['session'] is None, f_call
    provider = json.loads(f_call['models_json'])['providers']['llama-wsx']
    assert provider['baseUrl'] == start_floor_url + '/v1' and provider['apiKey'] == FLOOR['key'], provider
    assert provider['models'][0]['id'] == 'lfm3-1.7b', provider
    # ^ A second message and its decision: the call builds its context from the chat, not from Claude's conversation.
    f_second = js('task', 'say', 'We decided: ship on Friday. STATUS second')
    f_answer2 = wait(lambda: next((e for e in chat() if e['kind'] == 'reply' and e.get('provisional') == 'local-model'
                                   and f_second['id'] in e['to']), None), 'the floor stands in again', 40)
    f_prompt = next(t for t in traces() if t.get('adapter') == 'pi' and 'STATUS second' in str(t.get('message')))
    assert 'We decided: the theme is blue' in f_prompt['message'] and 'We decided: ship on Friday' in f_prompt['message'], f_prompt
    assert manager_state().get('agent') is None and manager_state()['agent_session'] == session_before, manager_state()
    assert not any(e['kind'] == 'reply' and e.get('provisional') is None and f_second['id'] in e['to'] for e in chat()), chat()[-3:]
    # (d) The limit ends: the same Claude conversation resumes and its turn holds the owner's messages and the floor's replies.
    limit_file.unlink()
    f_back = js('task', 'say', 'STATUS back')
    assert reply_to(f_back['id'])['text'].startswith('open: '), chat()[-3:]
    assert rotations() == rotated_before and manager_state()['agent_session'] == session_before, (rotated_before, manager_state())
    f_home = turn_with('STATUS back')
    assert f_home['session'] == session_before and f_home['message'] and 'We decided: the theme is blue' in f_home['message'], f_home
    for held_word in ('We decided: ship on Friday', 'a stand-in model with no tools answered these',
                      'local-model to message %d: pi answers' % f_first['id'], 'local-model to message %d: pi answers' % f_second['id']):
        assert held_word in f_home['message'], (held_word, f_home['message'])
    assert not f_home['message'].startswith('Handover'), f_home['message'][:200]
    # (c) A worker on an hour's limit still waits for Claude: the floor is no worker's fallback.
    limit_for(600)
    f_worker = js('task', 'new', 'WORK floor.txt')['id']
    wait(lambda: show(f_worker)['state'] == 'waiting' and show(f_worker)['record']['waiting']['on'] == 'limit', 'the worker waits')
    time.sleep(3)
    assert show(f_worker)['record']['adapter'] == 'claude' and not any(e['kind'] == 'agent_switch' for e in log(f_worker)), log(f_worker)
    limit_file.unlink()
    assert wait_state(f_worker, 'ready', 'failed')['state'] == 'ready', show(f_worker)
    js('task', 'close', f_worker, '--discard')
    FLOOR['up'] = False
    configure(CONFIG, 'no fallback')

    # 38. A host that started before this wsx cannot read a request the newer CLI added
    # (docs/tasks.md "Host"). The stand-in is what an older host does with a request it cannot
    # read: it closes the connection unanswered. A task at rest gets its host session replaced
    # by one on the current wsx and the request again; a task in a turn gets a plain answer.
    mark('38')
    stale = work / 'stale'

    def process_gone(pid):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        return False

    stale.mkdir(mode=0o700)
    fake_host = stale / 'old-host.py'
    fake_host.write_text(OLD_HOST)
    old_log = stale / 'old.log'
    old, other = (js('task', 'new', 'WORK a.txt')['id'] for _ in range(2))
    for n in (old, other):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
    task_folder = Path(show(old)['log']).parent
    host_socket = Path(show(old)['socket'])
    working_copy = show(old)['record']['working_copy']
    old_pid = stale / 'old.pid'

    def old_host():
        # ^ The session of the task's host runs the stand-in instead, as after a deploy.
        cli('session', 'delete', 'task-%d' % old, '-w', working_copy)
        old_pid.unlink(missing_ok=True)
        cli('session', 'create', '--name', 'task-%d' % old, '-w', working_copy,
            '--command', 'exec %s %s %s %s %s' % (sys.executable, fake_host, host_socket, old_log, old_pid))
        wait(lambda: old_pid.exists() and old_pid.read_text() and host_socket.exists(), 'the older host')
        assert show(old)['host_running']
        return int(old_pid.read_text())

    def old_requests():
        return [line.split()[1] for line in old_log.read_text().splitlines()] if old_log.exists() else []

    stand_in = old_host()
    # ^ The request of the incident: `after` is newer than the host, and the task is ready.
    parked = js('task', 'after', old, other, 'WORK a.txt', '--because', 'files')
    assert parked['ok'] and parked['state'] == 'ready', parked
    assert set(old_requests()) == {'after'}, old_requests()
    wait(lambda: process_gone(stand_in), 'the older host ended')
    assert show(old)['host_running']
    assert show(old)['record']['after']['task'] == other and show(old)['state'] == 'ready', show(old)
    # ^ The new host is this wsx: it answers a later request at once, as before.
    seen = len(old_requests())
    assert js('task', 'after', old, '--off')['ok'] and 'after' not in show(old)['record']
    assert len(old_requests()) == seen
    # A second request type.
    stand_in = old_host()
    assert js('task', 'hold', old, 'try it first')['ok'], show(old)
    assert set(old_requests()) == {'after', 'hold'} and show(old)['record']['hold'], (old_requests(), show(old)['record'])
    wait(lambda: process_gone(stand_in), 'the older host ended')
    assert show(old)['host_running']
    # In a turn the host is not replaced: that would stop the turn. The answer says when to repeat.
    stand_in = old_host()
    record_path = task_folder / 'task.json'
    record = json.loads(record_path.read_text())
    record['state'] = 'working'
    record_path.write_text(json.dumps(record))
    seen = len(old_requests())
    refused = cli('task', 'after', old, other, 'WORK a.txt', '--because', 'files', ok=False).stderr
    assert 'started before this wsx' in refused and 'repeat it when the task is ready' in refused, refused
    assert 'wsx session' not in refused, refused
    assert set(old_requests()[seen:]) == {'after'} and not process_gone(stand_in) and show(old)['host_running']
    for n in (old, other):
        js('task', 'close', n, '--discard')
    assert process_gone(stand_in) and not show(old)['host_running']

    # 39. A host that never started or died while the task is working (docs/tasks.md "A host
    # that is gone"): its socket is gone and its pane has exited or the session is lost. An add
    # or a hold restarts the host in the working copy and sends the request; the turn runs.
    mark('39')
    lost = js('task', 'new', 'WORK a.txt')['id']  # ^ the task's check wants a.txt
    assert wait_state(lost, 'ready', 'failed')['state'] == 'ready'
    lost_id = show(lost)['record']['project_id']
    lost_socket = Path(show(lost)['socket'])
    lost_record = Path(show(lost)['log']).parent / 'task.json'

    def host_pids():
        return [int(line.split()[0]) for line in leftovers('task host %s %d' % (lost_id, lost))
                if line.endswith('task host %s %d' % (lost_id, lost))]

    def kill_host():
        for pid in host_pids():
            os.kill(pid, 9)
        wait(lambda: not host_pids(), 'the host ended')
        assert wait(lambda: not show(lost)['host_running'], 'the host is not running')

    # A killed host leaves its pane exited and a task `working` with nobody to run its turn.
    kill_host()
    record = json.loads(lost_record.read_text())
    record['state'] = 'working'
    lost_record.write_text(json.dumps(record))
    added = js('task', 'add', lost, 'WORK h.txt')
    assert added['ok'], added
    assert show(lost)['host_running']
    assert wait_state(lost, 'ready', 'failed')['state'] == 'ready', show(lost)
    assert (Path(show(lost)['record']['working_copy']) / 'h.txt').exists()
    # The host session is lost altogether (no session, an old socket file left behind): a hold
    # creates the session again.
    kill_host()
    cli('session', 'delete', 'task-%d' % lost, '-w', show(lost)['record']['working_copy'])
    assert js('task', 'hold', lost, 'back again')['ok'] and show(lost)['host_running']
    assert show(lost)['record']['hold'], show(lost)
    # A closed task is not revived: the answer is the plain error.
    js('task', 'close', lost, '--discard')
    assert not show(lost)['host_running']
    refused = cli('task', 'hold', lost, 'x', ok=False).stderr
    assert 'no task host answers' in refused or 'closed' in refused, refused
    assert not lost_socket.exists() and not host_pids()

    # 40. A host hung in a turn under an older wsx (docs/tasks.md "A host hung in a turn"): it answers
    # its socket, but its code never ends the turn, so a deploy cannot reach it. A caller on this wsx
    # (a deploy, a task request, a say) closes it once the turn is quiet past the bound (the caller's
    # own WSX_TEST_TURN_QUIET_FILE; the hosts keep the real bound), stops its agent's tree, logs one
    # `stopped` entry and starts a host on this wsx: the task resumes its conversation, the manager
    # answers the queued message. A stale host whose turn still writes output is left alone.
    mark('40')
    configure(DEPLOY_CONFIG.replace('  checks:\n', '  agent:\n    model: fake-model\n  checks:\n'), 'deploys for hung hosts')
    caller = {'WSX_TEST_TURN_QUIET_FILE': str(work / 'caller-quiet')}
    (work / 'caller-quiet').write_text('3')
    older = work / 'older'
    older.mkdir(mode=0o700)
    shutil.copy2(wsx, older / 'wsx')

    def on(binary, *argv, extra=None):
        if named_project:
            argv = (*argv, '-p', named_project[0])
        done =subprocess.run([str(binary), *map(str, argv), '--json'], cwd=project, env={**env, **(extra or {})},
                              capture_output=True, text=True, timeout=120)
        assert done.returncode == 0, (argv, done.stdout, done.stderr)
        return json.loads(done.stdout)

    def install():
        # ^ A deploy renames a new file over the path: every host started from it is now older.
        shutil.copy2(wsx, older / 'wsx.new')
        os.replace(older / 'wsx.new', older / 'wsx')

    def host_of(args):
        return [int(line.split()[0]) for line in leftovers(args) if line.rstrip().endswith(args)]

    def agent_tree(pid):
        ps = subprocess.run(['ps', '-axwwE', '-o', 'pid=,command='], capture_output=True, text=True).stdout
        return [line for line in ps.splitlines() if 'WSX_AGENT_TREE=%d-' % pid in line]

    replaced = lambda entries: [e for e in entries if e['kind'] == 'stopped' and e.get('reason', '').startswith('the host ran an older wsx')]
    hanging = lambda path: any(t.get('hang') == str(path) for t in traces())
    resumed = lambda since: [t for t in traces()[since:] if str(t.get('message', '')).startswith('the host ran an older wsx')]
    # A deploy: the worker's turn hangs (WHANG) in a host on the older file; the deploy replaces it.
    w_release = work / 'hung-release'
    hung = on(older / 'wsx', 'task', 'new', 'WHANG %s WORK a.txt' % w_release)['id']
    project_id = show(hung)['record']['project_id']
    wait(lambda: hanging(w_release), 'the worker turn hangs', 40)
    old_host = host_of('task host %s %d' % (project_id, hung))
    assert len(old_host) == 1 and agent_tree(old_host[0]), (old_host, leftovers('task host'))
    install()
    time.sleep(4)
    seen = len(traces())
    before = len(deploy_records())
    assert on(wsx, 'task', 'deploy', extra=caller)['started']
    wait(lambda: len(deploy_records()) > before and deploy_lock_free(), 'the deploy and its sweep', 60)
    assert len(replaced(log(hung))) == 1 and 'had no agent output for' in replaced(log(hung))[0]['reason'], log(hung)[-4:]
    assert process_gone(old_host[0]) and not agent_tree(old_host[0]), agent_tree(old_host[0])
    wait(lambda: resumed(seen), 'the new host resumes the turn', 40)
    assert host_of('task host %s %d' % (project_id, hung)) not in ([], old_host)
    launched = [t for t in traces()[seen:] if t.get('launch')]
    assert launched and launched[0]['resume'] and launched[0]['session'] == show(hung)['record']['agent_session'], launched
    w_release.touch()
    wait_state(hung, 'ready', 'failed')
    js('task', 'close', hung, '--discard')
    def legacy_host(name, aged_ms=0):
        # ^ A host from before the status answer named its binary (the stand-in of 38), in front
        # of a task marked `working`; `aged_ms` moves its log and record back by that much.
        n = js('task', 'new', 'WORK a.txt')['id']
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
        copy, socket = show(n)['record']['working_copy'], Path(show(n)['socket'])
        pid_file = stale / (name + '.pid')
        cli('session', 'delete', 'task-%d' % n, '-w', copy)
        cli('session', 'create', '--name', 'task-%d' % n, '-w', copy,
            '--command', 'exec %s %s %s %s %s' % (sys.executable, fake_host, socket, stale / (name + '.log'), pid_file))
        wait(lambda: pid_file.exists() and pid_file.read_text() and socket.exists(), 'the %s host' % name)
        folder = Path(show(n)['log']).parent
        lines = [{**e, 'unix_ms': e['unix_ms'] - aged_ms} for e in log(n)]
        (folder / 'log.jsonl.tmp').write_text(''.join(json.dumps(line) + '\n' for line in lines))
        os.replace(folder / 'log.jsonl.tmp', folder / 'log.jsonl')
        record = json.loads((folder / 'task.json').read_text())
        record['state'] = 'working'
        for key in ('state_unix_ms', 'created_unix_ms'):
            record[key] -= aged_ms
        (folder / 'task.json').write_text(json.dumps(record))
        return n, int(pid_file.read_text())

    # A task request: the legacy host counts as older; its task is in a turn with no output past
    # the bound, so an add replaces it.
    legacy, legacy_pid = legacy_host('legacy')
    time.sleep(4)
    seen = len(traces())
    assert on(wsx, 'task', 'add', legacy, 'WORK c.txt', extra=caller)['ok']
    assert process_gone(legacy_pid) and len(replaced(log(legacy))) == 1, log(legacy)[-4:]
    assert not (stale / 'legacy.log').exists(), 'the add went to the new host'
    wait(lambda: resumed(seen), 'the new host resumes the legacy turn', 40)
    wait_state(legacy, 'ready', 'failed')
    js('task', 'close', legacy, '--discard')
    # A manager host that starts on a new wsx sweeps once: a deploy that ran on the older wsx has
    # no sweep of its own. Its own env keeps the real bound, so the turn is 3 h quiet.
    unswept, unswept_pid = legacy_host('unswept', aged_ms=3 * 3600 * 1000)
    manager_socket = js('task', 'manager')['socket']
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    wait(lambda: not Path(manager_socket).exists() or subprocess.run(['nc', '-zU', manager_socket]).returncode != 0, 'manager down')
    seen = len(traces())
    assert js('task', 'manager')['state'] == 'started'
    wait(lambda: replaced(log(unswept)), 'the starting manager replaces the hung host', 60)
    assert process_gone(unswept_pid) and len(replaced(log(unswept))) == 1, log(unswept)[-4:]
    assert re.search(r'turn, started 3 h.* ago, had no agent output for 3 h', replaced(log(unswept))[0]['reason']), replaced(log(unswept))
    wait(lambda: resumed(seen), 'the new host resumes the unswept turn', 40)
    assert wait_state(unswept, 'ready', 'failed')['state'] == 'ready', log(unswept)[-6:]
    js('task', 'close', unswept, '--discard')
    # A say: the manager's turn hangs (MHANG) in a host on the older file; the next say replaces it
    # and the new host answers the queued message.
    socket_path = js('task', 'manager')['socket']
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    wait(lambda: not Path(socket_path).exists() or subprocess.run(['nc', '-zU', socket_path]).returncode != 0, 'manager down')
    m_release = work / 'mhang-release'
    on(older / 'wsx', 'task', 'say', 'MHANG %s STATUS hung under an older wsx' % m_release)
    wait(lambda: hanging(m_release), 'the manager turn hangs', 40)
    old_manager = host_of('task manager-host %s' % project_id)
    assert len(old_manager) == 1 and agent_tree(old_manager[0]), old_manager
    stopped_before = len(replaced(manager_entries()))
    install()
    time.sleep(4)
    queued = on(wsx, 'task', 'say', 'STATUS after the hang', extra=caller)
    assert queued['ok'], queued
    assert process_gone(old_manager[0]) and not agent_tree(old_manager[0]), agent_tree(old_manager[0])
    assert len(replaced(manager_entries())) == stopped_before + 1, manager_entries()[-4:]
    # ^ The new host queues the hung turn's message again; released, it no longer hangs.
    m_release.touch()
    assert reply_to(queued['id'], 60)['text'].startswith('open: '), chat()[-3:]
    # A stale host whose turn writes output keeps its turn: it follows at its next one.
    drip = work / 'drip-release'
    busy = on(older / 'wsx', 'task', 'new', 'DRIP %s WORK a.txt' % drip)['id']
    wait(lambda: any(e['kind'] == 'agent' and 'still working' in e.get('text', '') for e in log(busy)), 'the dripping turn', 40)
    busy_host = host_of('task host %s %d' % (project_id, busy))
    install()
    time.sleep(5)
    assert on(wsx, 'task', 'add', busy, 'WORK d.txt', extra=caller)['ok']
    assert host_of('task host %s %d' % (project_id, busy)) == busy_host and not replaced(log(busy)), log(busy)[-4:]
    drip.touch()
    assert wait_state(busy, 'ready', 'failed')['state'] == 'ready', log(busy)[-6:]
    js('task', 'close', busy, '--discard')
    configure(CONFIG, 'no deploys')

    # 39b. S3, the manager (docs/tasks.md "Agents"): a turn with no output past the bound ends as the
    # agent's error, which holds and retries with the backoff, and reaches the owner at once as one
    # alert with the open-task count ("Manager"). Last, since the hold lasts 5 minutes.
    mark('39b')
    turn_quiet.write_text('40')
    open_tasks = len(js('task', 'list')['tasks'])
    s3_say = js('task', 'say', 'MHANG %s STATUS hung' % (work / 's3m-release'))
    s3_stop = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'stopped' and e.get('reason', '').startswith('no output for')), None),
                   lambda: 'the hung manager turn ends ' + lc_tail(), 70)
    assert s3_stop and manager_state()['limit']['attempt'] == 1 and 'agent' not in manager_state(), manager_state()
    assert not any(e['kind'] == 'reply' and s3_say['id'] in e['to'] for e in chat()), chat()[-3:]
    assert any(e['kind'] == 'limit' and e.get('until') for e in manager_entries()[-6:]), manager_entries()[-6:]
    s3m_turn = lc_turn(manager_log.parent)
    assert s3m_turn['ended']['reason'].startswith('no output for') and s3m_turn['quiet_ms'] == 40000, s3m_turn
    open_words = {0: 'No task is open.', 1: '1 open task: '}.get(open_tasks, '%d open tasks: ' % open_tasks)
    s3_alerts = lc_alerts(s3_say['id'])
    assert len(s3_alerts) == 1 and s3_alerts[0]['text'].startswith('no output for') and 'It tries again at' in s3_alerts[0]['text'] \
        and open_words in s3_alerts[0]['text'], s3_alerts
    turn_quiet.unlink()

    # 39c. A chat's turn that hits a 403 block page (docs/tasks.md "Manager", "Chats"): the chat
    # host holds as the manager does and alerts the owner in its own chat, with the project's open
    # tasks. The chat holds 5 minutes too, so it is closed after.
    mark('39c')
    alert_goal = work / 'alert-goal.md'
    alert_goal.write_text('Watch the alerts.\n')
    cli('task', 'chat', 'new', 'alerts', '--goal', str(alert_goal), '-p', 'demo', '--json')
    alerts_log = manager_log.parent.parent / 'chats' / 'alerts' / 'log.jsonl'
    alerts_entries = lambda: [json.loads(line) for line in alerts_log.read_text().splitlines()] if alerts_log.exists() else []
    manager_alerts = len(lc_alerts(0))
    blocked_say = js('task', 'say', '--to', 'alerts', 'CFAUTH %s STATUS blocked' % (work / 'cf-release'))
    chat_alerts = wait(lambda: lc_alerts(blocked_say['id'], alerts_entries()), lambda: 'the chat alert %r' % alerts_entries()[-6:], 40)
    assert len(chat_alerts) == 1 and chat_alerts[0]['text'].startswith('Failed to authenticate. API Error: 403 Attention Required') \
        and 'It tries again at' in chat_alerts[0]['text'] and re.search(r'(\d+ open tasks?: .+|No task is open)\.$', chat_alerts[0]['text']), chat_alerts
    assert 'Attention Required' in lc_turn(alerts_log.parent)['ended']['reason'], lc_turn(alerts_log.parent)
    assert len(lc_alerts(0)) == manager_alerts, 'the chat alerts in its own chat, not the manager\'s'
    cli('task', 'chat', 'close', 'alerts', '-p', 'demo', '--json')
    return created


if __name__ == '__main__':
    main()
