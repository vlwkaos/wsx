#!/usr/bin/env python3
"""Model-free `wsx task` journey on an isolated wsxd with a stream-json fake Claude.

Covers new, steer during a turn, add after it, a blocked result then allow, checks that
pass and fail, report content, a daemon live handoff while a host runs, stop and resume,
close with and without unlanded commits, failure cleanup, and that no process, socket,
worktree or branch is left. Build adjacent wsx and wsxd first. ref: docs/tasks.md
"""
import argparse
import fcntl
import hashlib
import urllib.request
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import socket
import struct
import subprocess
import sys
import termios
import time
import unicodedata

import wsxd_client

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

LAND_CONFIG = '''tasks:
  minFreeGb: 0
  land: { auto: false }
  checks:
    - "test ! -e broken.txt"
'''


def tui_run(wsx, env, cwd, done, cols=120, seconds=15):
    """The real TUI in a private PTY (`cols`x40). `done(output, write, idle)` sees all output so
    far after each read, and with `idle` true after 0.1 s without output, and may write keys; the
    TUI quits when it returns true or the time is up."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, cols, 0, 0))
    proc = subprocess.Popen([str(wsx)], cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave,
                            start_new_session=True)
    output, deadline = bytearray(), time.monotonic() + seconds
    try:
        while time.monotonic() < deadline and proc.poll() is None:
            idle = not select.select([master], [], [], 0.1)[0]
            if not idle:
                output += os.read(master, 65536)
            if done(output, lambda keys: os.write(master, keys), idle):
                break
    finally:
        if proc.poll() is None:
            os.write(master, b'\x01q')
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=3)
        os.close(master)
        os.close(slave)


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
json.dump(seen, open(sys.argv[2], "w"))
'''
# ^ True: the boundary allows it; False: it refuses it.
PROBE_EXPECTED = {'read_ssh': False, 'write_ssh': False, 'write_state': False, 'write_existing_copy': False,
                  'write_later_copy': False, 'write_main_ref': False, 'write_main_lock': False,
                  'write_main_reflog': False, 'write_hooks': False, 'write_config': False, 'write_routines': False,
                  'write_own_copy': True, 'write_task_folder': True, 'write_tmpdir': True, 'write_cargo': True,
                  'write_library': True, 'move_main': False, 'listen': True, 'pty': True}


def stop_scheduler(home):
    """Shut down the routine scheduler that wsxd hosts for an isolated home, when one runs."""
    request = {'protocol': 4, 'project': '', 'action': {'action': 'shutdown'}}
    try:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(str(home / 'Library/Application Support/asched/daemon-v1.sock'))
            connection.sendall((json.dumps(request) + '\n').encode())
            connection.shutdown(socket.SHUT_WR)
            connection.recv(4096)
    except OSError:
        pass


def tui_text(wsx, env, cwd, until, seconds=15):
    """The TUI's output as text without escapes, read until `until` (a regex) matches."""
    text = ''

    def done(output, _write, _idle):
        nonlocal text
        # ^ Cells of one style are written in one run; escapes split the runs.
        text = re.sub(r'\x1b(\[[0-9;?<>=]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|[()][0-9A-Za-z]|.)', '\n',
                      output.decode('utf-8', 'replace'))
        return re.search(until, text)
    tui_run(wsx, env, cwd, done, seconds=seconds)
    return text


# ^ Each numbered part marks its start; the result names the slowest parts, so a slow run
# says where its time went.
PARTS = []


def mark(name):
    PARTS.append((name, time.monotonic()))


def slowest(count=8):
    ends = [at for _, at in PARTS[1:]] + [time.monotonic()]
    spans = [(name, round(end - at, 1)) for (name, at), end in zip(PARTS, ends)]
    return sorted(spans, key=lambda span: -span[1])[:count]


def screen(output, cols, rows=40, styled=False):
    """The screen `output` leaves: the cursor moves, clears and prints the TUI writes, without
    styles. ratatui writes only the cells that changed, so the raw text alone can split words.
    With `styled`, a list of rows of `(char, bold, background)` cells instead."""
    grid = [[' '] * cols for _ in range(rows)]
    styles = [[(False, False)] * cols for _ in range(rows)]
    bold = background = False
    row = col = 0
    tokens = re.finditer(r'\x1b\[([0-9;?<>=]*)([ -/]*[@-~])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b.|(.)',
                         output.decode('utf-8', 'replace'), re.S)
    for token in tokens:
        params, final, char = token.group(1), token.group(2), token.group(3)
        if final == 'm':
            # ^ SGR: 1/22 bold, 4x/10x/48 a background, 49 none, 0 resets; 38/48 carry a colour.
            codes = [int(n) if n.isdigit() else 0 for n in params.split(';')] if params else [0]
            index = 0
            while index < len(codes):
                code = codes[index]
                if code == 0:
                    bold = background = False
                elif code == 1:
                    bold = True
                elif code == 22:
                    bold = False
                elif 40 <= code <= 47 or 100 <= code <= 107:
                    background = True
                elif code == 49:
                    background = False
                if code in (38, 48):
                    background = background or code == 48
                    index += 2 if index + 1 < len(codes) and codes[index + 1] == 5 else 4
                index += 1
        elif final == 'H':
            numbers = [int(n) if n else 1 for n in params.split(';')] if params else [1, 1]
            row, col = numbers[0] - 1, (numbers[1] if len(numbers) > 1 else 1) - 1
        elif final == 'J' and params == '2':
            grid = [[' '] * cols for _ in range(rows)]
        elif final == 'K':
            grid[row][col:] = [' '] * (cols - col)
        elif char == '\r':
            col = 0
        elif char == '\n':
            row = min(row + 1, rows - 1)
        elif char is not None and char.isprintable() and 0 <= row < rows and col < cols:
            grid[row][col] = char
            styles[row][col] = (bold, background)
            col += 2 if unicodedata.east_asian_width(char) in 'WF' else 1
    if styled:
        return [[(c, *s) for c, s in zip(line, style)] for line, style in zip(grid, styles)]
    return '\n'.join(''.join(line).rstrip() for line in grid)


def tui_screens(wsx, env, cwd, steps, cols=120, raw=None):
    """For each `(regex, keys)` step: wait until the drawn screen matches, keep it, send the
    keys. Returns the kept screens. A screen counts once output pauses: a frame can arrive in
    several reads. `raw`, a list, gets the output so far at each kept screen."""
    screens, shown = [], ''

    def done(output, write, idle):
        nonlocal shown
        if not idle:
            return False
        shown = screen(output, cols)
        if re.search(steps[len(screens)][0], shown):
            screens.append(shown)
            if raw is not None:
                raw.append(bytes(output))
            if len(screens) == len(steps):
                return True
            keys = steps[len(screens) - 1][1]
            write(keys() if callable(keys) else keys)
        return False
    tui_run(wsx, env, cwd, done, cols=cols, seconds=40)
    assert len(screens) == len(steps), 'TUI step %d %r not shown:\n%s' % (
        len(screens), steps[len(screens)][0], shown)
    return screens


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
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
        run(wsx, wsxd, work, owned)
        print(json.dumps({'ok': True, 'seconds': round(time.monotonic() - started, 1), 'slowest': slowest()}))
    finally:
        # ^ Stop the isolated wsxd also after a failure: its hosts and fake agents end with it.
        if 'env' in owned:
            subprocess.run([str(wsx), 'daemon', 'stop', '--force'], env=owned['env'],
                           capture_output=True, timeout=20)
            stop_scheduler(Path(owned['env']['HOME']))
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)


def run(wsx, wsxd, work, owned):
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
    # ^ README "Profiles": demo is in profile work, whose agents use its own config root.
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
    fake.write_text('#!/bin/sh\nexec ' + sys.executable + ' ' + str(fake_script) + ' "$@"\n')
    fake.chmod(0o755)
    daemon_socket = work / 'd.sock'
    env = {'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(work / 'c'),
           'XDG_CACHE_HOME': str(work / 'k'), 'WSX_DAEMON_BIN': str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()), 'WSX_SOCKET': str(daemon_socket),
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

    def cli(*argv, ok=True, stdin=None):
        result = subprocess.run([str(wsx), *map(str, argv)], cwd=project, env=env, input=stdin,
                                capture_output=True, text=True, timeout=60)
        if ok:
            assert result.returncode == 0, (argv, result.stdout, result.stderr)
        else:
            assert result.returncode != 0, (argv, result.stdout)
        return result

    def js(*argv):
        return json.loads(cli(*argv, '--json').stdout)

    def wait(test, label, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = test()
            if value:
                return value
            time.sleep(0.1)
        raise RuntimeError(label + ' timeout')

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
        path = base / 'fake-claude.trace'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def log(n):
        path = Path(show(n)['log'])
        return [json.loads(line) for line in path.read_text().splitlines()]

    def kinds(n, *names):
        return [e['kind'] for e in log(n) if e['kind'] in names]

    def leftovers(marker):
        ps = subprocess.run(['ps', '-axo', 'pid=,command='], capture_output=True, text=True).stdout
        return [line for line in ps.splitlines() if marker in line and 'ps -axo' not in line]

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
    # ^ README "Profiles": a profile error is named, and no task starts under another account.
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
    assert launch['wsx_env'] == ['WSX_TASK_BROKER'] and launch['schema']
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
    owner_config = work / 'c' / 'wsx' / 'wsx.config.yml'
    owner_config.parent.mkdir(parents=True, exist_ok=True)
    owner_config.write_text('tasks:\n  workflow:\n    steps:\n'
                            '      - name: plan\n        prompt: PLAN only\n'
                            '      - name: build\n        prompt: WORK s.txt\n')
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  checks: ["test -f s.txt"]\n')
    stepped = js('task', 'new', 'NOOP')['id']
    done = wait_state(stepped, 'ready', 'failed')
    assert done['state'] == 'ready' and done['record']['step'] == 1, done
    assert [step['name'] for step in done['record']['steps']] == ['plan', 'build'], done['record']
    turns = [entry for entry in log(stepped) if entry['kind'] == 'result']
    assert len(turns) == 2 and 'Step 1 of 2, plan: PLAN only' in turns[0]['summary'], turns
    assert 'Step 2 of 2, build: WORK s.txt' in turns[1]['summary'], turns
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
    rotated = js('task', 'new', 'NOOP')['id']
    done = wait_state(rotated, 'ready', 'failed')
    assert done['state'] == 'ready', done
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
    js('task', 'close', rotated, '--discard')
    # A bad rotateTokens refuses the task.
    (project / 'wsx.config.yml').write_text('tasks:\n  minFreeGb: 0\n  agent: { rotateTokens: 5 }\n')
    assert 'tasks.agent.rotateTokens must be' in cli('task', 'new', 'NOOP', ok=False).stderr
    # A project's own steps win; a bad step refuses the task before anything starts.
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  minFreeGb: 0\n  workflow:\n    steps:\n'
        '      - {name: a, prompt: x}\n      - {name: a, prompt: y}\n')
    assert 'named twice' in cli('task', 'new', 'NOOP', ok=False).stderr
    owner_config.unlink()
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
    flags = lambda name: {k: strays[name][k] for k in ('branch', 'merged_into_main', 'dirty', 'missing', 'sessions')}
    assert flags('stray-merged') == {'branch': 'stray-merged', 'merged_into_main': True, 'dirty': False,
                                     'missing': False, 'sessions': 0}, strays
    assert flags('stray-dirty') == {'branch': 'stray-dirty', 'merged_into_main': False, 'dirty': True,
                                    'missing': False, 'sessions': 0}, strays
    assert flags('stray-gone') == {'branch': 'stray-gone', 'merged_into_main': True, 'dirty': False,
                                   'missing': True, 'sessions': 0}, strays
    assert ledger['stray_worktrees_omitted'] == 0
    for name in ('stray-merged', 'stray-dirty'):
        git('worktree', 'remove', '--force', str(base / name))
    git('worktree', 'prune')
    git('branch', '-D', 'stray-merged', 'stray-dirty', 'stray-gone')
    assert js('task', 'ledger')['stray_worktrees'] == []
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
        (r'Waits', b'\x1b'),
        (r'\(enter\)detail', b'j\r'),
        (r'Checks', b'\x1b'),
        (r'\(enter\)detail', open_gate),
        (r'#%d +ready' % d1, b'\x1b'),
        (r'WORKSPACE', b'')])
    listed, asked, _, checked = shots[1:5]
    order = [listed.index(row(listed, n)) for n in (asking, failing, x1, d1, memo_task)]
    assert order == sorted(order) and order[-1] > listed.index('Recent'), listed
    assert re.search(r'NEEDS WebSearch NOOP +#%d +waiting +owner: ' % asking, row(listed, asking)), listed
    assert re.search(r'#%d +failed' % failing, row(listed, failing)), listed
    assert re.search(r'#%d +working' % d1, row(listed, d1)) and 'closed' in row(listed, memo_task), listed
    assert 'NEEDS WebSearch NOOP  #%d' % asking in asked and re.search(r'State +waiting', asked), asked
    assert re.search(r'Waits +owner: ' + re.escape(reason[:30]), asked), asked
    assert re.search(r'Checks +0 passed, 1 failed', checked) and re.search(r'failed +test ! -e broken.txt', checked), checked
    assert 'Summary' in checked and 'TASK ' in checked, checked
    assert 'TASKS' not in shots[-1], shots[-1]
    narrow = tui_screens(wsx, env, project, [(r'WORKSPACE', b't'), (r'Recent', b'')], cols=40)[1]
    for title, word in (('NEEDS WebSearch', 'waiting'), ('WORK broken.txt', 'failed'), ('WORK memo.txt', 'closed')):
        assert re.search(r'^ %s.* %s$' % (re.escape(title), word), narrow, re.M), narrow
    assert not re.search(r'#\d', narrow.split('Open', 1)[1]), narrow
    for n in (asking, failing):
        js('task', 'close', n, '--discard')
    for n in (d1, d2, d3):
        js('task', 'close', n, '--discard')

    # 14e. `S` in the Tasks view opens a shell: in the task's working copy on its row, in the
    # main checkout from the chat. A second `S` enters the same session, and the worker keeps
    # its turn: no hand, no stop (docs/tasks.md "TUI").
    mark('14e')
    shell_gate = work / 'shell-gate'
    sh = js('task', 'new', 'WAIT %s' % shell_gate)['id']
    wait_state(sh, 'working')
    copy_name = Path(show(sh)['record']['working_copy']).name
    where = b'printf "at:%s\\n" "${PWD##*/}"\r'

    def sessions_at(path):
        data = daemon_call('snapshot')['data']
        ids = {w['id'] for w in data['worktrees'] if os.path.realpath(w['path']) == os.path.realpath(path)}
        return [s['label'] for s in data['sessions'] if s['worktree_id'] in ids]
    prompt = r'(?m)\$ *$'
    tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'#%d +working' % sh, b'S'),
        (prompt, where),
        (r'at:%s' % re.escape(copy_name), b'\x01w'),
        (r'WORKSPACE', b'tm'),
        (r'MANAGER', b'S'),
        (prompt, where),
        (r'at:%s' % re.escape(project.name), b'')])
    copy_path = show(sh)['record']['working_copy']
    task_shells = [label for label in sessions_at(copy_path) if label != 'task-%d' % sh]
    main_shells = [label for label in sessions_at(project) if not label.startswith('task-')]
    assert len(task_shells) == 1 and len(main_shells) == 1, (task_shells, main_shells)
    again = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'#%d +working' % sh, b'S'),
        (r'at:%s' % re.escape(copy_name), b'\x01w'),
        (r'WORKSPACE', b'tm'),
        (r'MANAGER', b'S'),
        (r'at:%s' % re.escape(project.name), b'')])
    assert 'TASKS' not in again[2] and 'MANAGER' not in again[-1], again
    assert [label for label in sessions_at(copy_path) if label != 'task-%d' % sh] == task_shells
    assert [label for label in sessions_at(project) if not label.startswith('task-')] == main_shells
    assert show(sh)['state'] == 'working' and not kinds(sh, 'hand', 'stopped'), log(sh)
    shell_gate.write_text('open')
    wait_state(sh, 'ready', 'failed')
    js('task', 'close', sh, '--discard')
    cli('session', 'delete', main_shells[0], '-p', 'demo')

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

    def reviewed(n):
        """Wait for a ready task that stays ready, and land it as the manager's review would."""
        assert wait_state(n, 'ready', 'landed', 'failed')['state'] == 'ready', show(n)
        return land(n)

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
    manager_env = dict(env, WSX_TASK_ACTOR='manager')
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

    unset = cli('task', 'deploy', ok=False)
    assert 'tasks.deploy.command is not set' in unset.stderr and 'nothing was run' in unset.stderr
    configure(LAND_CONFIG + '''  deploy:
    command: 'printf "%s %s\\n" "$WSX_DEPLOY_COMMIT" "$(pwd)" >> ../deploy.log'
  release:
    command: 'seq 1 60; echo release broke here >&2; exit 3'
''', 'deploy config')
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
    assert failed['output'].splitlines() == [str(n) for n in range(22, 61)] + ['release broke here'], failed
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
    assert ledger['open'][1]['checks'] == [{'command': 'test ! -e broken.txt', 'outcome': 'passed'}], ledger['open'][1]
    assert 0 < len(ledger['recent']) <= 10 and all(row['state'] in ('landed', 'closed') for row in ledger['recent'])
    assert ledger['deploys']['deploy']['commit'] == older and ledger['main']['branch'] == 'main'
    assert ledger['main']['changed_files'] >= 1  # scratch.txt is untracked in the main checkout
    for n in (ready_task, blocked_task):
        js('task', 'close', n, '--discard')

    # 20. The manager: started by the first say, answers from the ledger, acts through wsx task.
    mark('20')
    configure((project / 'wsx.config.yml').read_text()
              + '  manager: { name: "  Atlas ", model: fake-manager, eventModel: fake-events, '
                'rotateTokens: 100000 }\n'
              + '  skills: owner\n  workflow:\n    build: "Run the fixture rule."\n'
                '    review: "Check the fixture rule."\n    batch: "Fold the fixture tasks."\n', 'manager config')
    # ^ The owner's skills folder: the manager gets it as the `owner` plugin. It is the one of
    # demo's profile (README "Profiles"), not ~/.claude.
    owner_skills = profile_root / 'skills'
    (owner_skills / 'scribe').mkdir(parents=True)
    (owner_skills / 'scribe/SKILL.md').write_text('---\nname: scribe\ndescription: fixture\n'
                                                  'allowed-tools: Write, Edit\n---\nWrite a file.\n')

    def chat(after=None):
        return js('task', 'chat', *(['--after', after] if after is not None else []))['entries']

    def reply_to(message_id):
        return wait(lambda: next((e for e in chat() if e['kind'] == 'reply' and message_id in e['to']), None),
                    'reply to %s' % message_id)

    def turn_with(text):
        """The first manager turn whose message holds `text`."""
        return next((t for t in traces() if t.get('manager') and text in str(t.get('message', ''))), None)

    running = js('task', 'new', 'SLOW SLOW SLOW WORK r.txt')['id']
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
    assert '`wsx task ledger -p demo` for more' in launch['instructions'], launch['instructions']
    page = os.path.realpath(state / 'wsx/tasks' / Path(created['task_dir']).parent.name / 'page.md')
    # ^ It uses the owner's skills (docs/tasks.md "Manager"); no tool list: nothing prompts.
    assert not {'--allowedTools', '--disallowedTools'} & set(launch['argv']), launch['argv']
    plugin = Path(launch['plugin_dir'])
    assert plugin.parent.name == 'manager' and plugin.resolve().is_relative_to(state.resolve()), plugin
    assert json.loads((plugin / '.claude-plugin/plugin.json').read_text())['name'] == 'owner'
    assert os.readlink(plugin / 'skills') == str(owner_skills), os.readlink(plugin / 'skills')
    # ^ No prompts inside wsx's boundary (docs/tasks.md "Permissions").
    assert launch['mode'] == 'bypassPermissions' and 'permissions' not in launch['settings'], launch['settings']
    assert launch['model'] == 'fake-manager' and Path(page).read_text().startswith('# demo\n\n## Goal')
    assert launch['actor'] == 'manager' and launch['wsx_env'] == ['WSX_TASK_ACTOR', 'WSX_TASK_BROKER'], launch['wsx_env']
    assert launch['git_optional_locks'] == '0', launch
    # ^ README "Profiles": the manager and every worker so far run under demo's profile root.
    demo_launches = [t for t in traces() if t.get('launch')]
    assert demo_launches and all(t['claude_config_dir'] == str(profile_root) for t in demo_launches), \
        {t['claude_config_dir'] for t in demo_launches}
    # Its rules: chat, not reports; quiet replies to events and no chores for the owner; start
    # at once; an open task before a new one; a complete prompt; a lean review without rebase
    # send-backs; one housekeeping task after a request lands; the owner's word; memory.
    for rule in ('Talk like a person in a chat, not a report', 'The reader sees one screen',
                 'A turn of only Events is your work', 'or with exactly `-` when nothing',
                 'Never hand the owner chores', 'Start a request for work at once', 'with a mermaid diagram',
                 'stands alone, since a worker cannot ask', 'in every task new', 'With `--plan` the worker',
                 'Before `wsx task new`, compare the request', 'Never ask the owner to wait',
                 'for lean code', 'never send a task back to rebase', 'trust them over the worker',
                 'Check the fixture rule.', 'start one housekeeping task', 'Fold the fixture tasks.',
                 'tell that worker to prove it', 'never pass, fail or skip items yourself',
                 'A release refuses while items are open, unless the owner', '`wsx task trial <n>`',
                 '`--owner-said <id>`', 'never name a message that did not ask',
                 'call a change live only when', 'On `hand`, the owner works in its copy',
                 'Nothing asks you for permission', 'read-only git in Bash', '`wsx task new --into <n>`',
                 'only after the owner says yes, commit', 'also in a pipe or with a heredoc',
                 'the Skill tool as `owner:<name>`', 'check rather than guess',
                 '/manager/notes/decisions.md', '/manager/notes/context.md', "worker's 'memo:'",
                 "'Handover'", 'Update a section in place',
                 '--from-manager --reply', 'use it and do not answer it', "'@<project>'"):
        assert rule in launch['instructions'], rule
    # ^ Only `role new` takes a brief (docs/tasks.md "Roles").
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
    assert 'ran: wsx --version -> 2 wsx: only `wsx task` commands run here' in text, text
    assert 'ran: wsx task host' in text and "-> 2 wsx: `wsx task host` is not for the manager" in text, text
    with socket.socket(socket.AF_UNIX) as raw:
        raw.connect(launch['broker'])
        raw.sendall(json.dumps({'argv': ['daemon', 'stop'], 'stdin': None, 'cwd': str(project)}).encode())
        raw.shutdown(socket.SHUT_WR)
        direct = json.loads(raw.makefile().read())
    assert direct['code'] == 2 and 'only `wsx task` commands' in direct['stderr'], direct
    assert wait_state(running, 'ready', 'failed')['state'] == 'ready'
    assert [e['mode'] for e in log(running) if e['kind'] == 'message'] == ['prompt', 'add']
    manager_log = Path(created['task_dir']).parent / 'manager' / 'log.jsonl'

    def manager_entries():
        return [json.loads(line) for line in manager_log.read_text().splitlines()]

    # Usage (docs/tasks.md "Manager"): rotation looks at the last main model call's context, not
    # the result's sum over the turn's calls, nor a subagent's; a reply's cost is its own, not the
    # process's running `total_cost_usd`.
    summed = js('task', 'say', 'SUMMED')
    usage = reply_to(summed['id'])['usage']
    replies = [e for e in manager_entries() if e['kind'] == 'reply']
    assert usage['input_tokens'] + usage['cache_read_tokens'] == 200000 and usage['context_tokens'] == 50000, usage
    assert usage['model'] == 'fake-manager', usage
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
    reply_to(big['id'])
    assert wait(lambda: any(e['kind'] == 'rotated' for e in chat()), 'rotated')
    after_rotation = js('task', 'say', 'STATUS')
    reply_to(after_rotation['id'])
    manager_launches = [t for t in traces() if t.get('launch') and t.get('manager')]
    assert not manager_launches[-1]['resume'] and manager_launches[-1]['session'] != launch['session']
    bare = manager_launches[-1]
    assert bare['plugin_dir'] is None, bare
    assert 'owner:' not in bare['instructions'] and 'check rather than guess' in bare['instructions']
    assert [e['id'] for e in chat(after=big['id']) if e['kind'] == 'message' and e['from'] == 'owner'] == [after_rotation['id']]
    for n in (running, manager_task, heredoc_task):
        js('task', 'close', n, '--discard')

    # 21. Events: task state changes reach the manager, merged while it is busy; an owner
    mark('21')
    # message waiting at the same time goes first; a host restart loses no event.
    def events_with(*needles):
        return [e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                and all(needle in e['text'] for needle in needles)]

    # ^ A failed deploy or release is an event with the end of its output.
    js('task', 'check', 'pass', '--all')
    ran('release')
    wait(lambda: events_with('release of %s failed (exit 3); its output ends:\n    52\n' % git('rev-parse', 'main')[:12],
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
    wait(lambda: events_with('routine nightly failed (exit 3); its output ends:\n    boom'), 'the routine event')
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
    def manager_idle():
        entries = manager_entries()
        delivered = {n for e in entries if e['kind'] == 'delivered' for n in e['ids']}
        turns = [e['kind'] for e in entries if e['kind'] in ('delivered', 'reply')]
        return all(e['id'] in delivered for e in entries if e['kind'] == 'message') and turns[-1:] != ['delivered']
    wait(manager_idle, 'the manager idle before SLOW', 60)
    time.sleep(3.5)
    wait(manager_idle, 'the manager idle before SLOW', 60)
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
    assert '#%d ready r1: NOOP; checks 1 of 1 passed; worker says: did: NOOP' % first in merged[0]['text'], merged
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
    # A decided item's capture files go 7 days after the decision, when the manager starts and
    # hourly; its row stays. Staged: the video item skipped 8 days ago, #recent's item open for 8
    # days; the shot item was passed just now (docs/tasks.md "To check").
    checks_file = Path(show(video)['report']).parent.parent / 'checks.jsonl'
    current = {}
    for line in checks_file.read_text().splitlines():
        current[json.loads(line)['id']] = json.loads(line)
    video_item, recent_item = (next(r for r in current.values() if r['task'] == n and r.get('capture')) for n in (video, recent))
    eight_days_ago = int(time.time() * 1000) - 8 * 24 * 3600 * 1000

    def append_rows(*rows):
        with open(checks_file, 'a') as out:
            fcntl.flock(out, fcntl.LOCK_EX)
            out.write(''.join(json.dumps(row) + '\n' for row in rows))

    append_rows(dict(video_item, unix_ms=eight_days_ago), dict(recent_item, state='open', unix_ms=eight_days_ago))
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
    wait(lambda: not any(captures_of[video].iterdir()), 'the decided video capture removed', 30)
    assert [p.name for p in captures_of[recent].iterdir()] == ['r1-1.png'] and [p.name for p in captures_of[shot].iterdir()] == ['r1-1.png']
    after_sweep = [json.loads(line) for line in checks_file.read_text().splitlines()]
    assert [r for r in after_sweep if r['id'] == video_item['id']][-1]['capture'] == 'captures/r1-1.mp4', after_sweep
    append_rows(dict(recent_item, unix_ms=int(time.time() * 1000)))
    late = wait(lambda: events_with('#%d ready r1' % unseen), 'event after restart')
    late_reply = reply_to(late[0]['id'])
    assert 'noted events' in late_reply['text']
    # ^ The resumed process restored the session's spend: the reply still costs only its own turn.
    resumed_usage = late_reply['usage']
    assert resumed_usage['cost_micro_usd'] == 1000 and resumed_usage['session_cost_micro_usd'] > 5000, resumed_usage
    # The same instructions resume the conversation; other ones start a new conversation that
    # opens with the last exchanges (Claude keeps a conversation's first system prompt). A
    # state written by a host from before the instructions digest stands for changed ones.
    launches = [t for t in traces() if t.get('launch') and t.get('manager')]
    assert launches[-1]['resume'], launches[-1]
    manager_socket = js('task', 'manager')['socket']
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    wait(lambda: not Path(manager_socket).exists() or
         subprocess.run(['nc', '-zU', manager_socket]).returncode != 0, 'manager down')
    configure((project / 'wsx.config.yml').read_text().replace('model: fake-manager, ', ''), 'default manager model')
    state_file = manager_log.parent / 'state.json'
    older = json.loads(state_file.read_text())
    assert older['agent_session'] and older.pop('instructions'), older
    state_file.write_text(json.dumps(older))
    # A fresh conversation opens with the notes; a linked notes file is not read.
    notes = manager_log.parent / 'notes'
    notes.mkdir(exist_ok=True)
    (notes / 'decisions.md').write_text('- 2026-10-22 keep the store in sqlite\n')
    outside = manager_log.parent.parent / 'outside-context.md'
    outside.write_text('SECRET-OUTSIDE\n')
    (notes / 'context.md').unlink(missing_ok=True)
    (notes / 'context.md').symlink_to(outside)
    changed = js('task', 'say', 'STATUS after the change')
    reply_to(changed['id'])
    fresh = [t for t in traces() if t.get('launch') and t.get('manager')][-1]
    assert not fresh['resume'] and fresh['session'] != launches[-1]['session'], fresh
    rotated = [e for e in manager_entries() if e['kind'] == 'rotated']
    assert rotated[-1].get('reason') == "the manager's instructions changed", rotated
    opened = turn_with('STATUS after the change')['message']
    # ^ The notes come first, then the handover.
    assert opened.startswith("The owner's decisions") and '\n\nHandover (this is a new conversation' in opened and 'You: noted events' in opened, opened
    assert "fold the ledger's memos into your context first" in opened, opened
    # ^ The three failures before the rotation are one line: the newest and how many.
    routines = opened.split('\nRoutine results (')[1].split('\n\n')[0].split('\n')[1:]
    assert routines == ['routine nightly failed (exit 3); 3 failures, never succeeded'], opened
    assert 'keep the store in sqlite' in opened and 'SECRET-OUTSIDE' not in opened, opened
    # ^ Only a short reply to events is quiet: one with a table is a message (docs/tasks.md "Manager").
    tabled = js('task', 'new', 'NOOP TABLE')['id']
    wait_state(tabled, 'ready')
    table_reply = reply_to(wait(lambda: events_with('#%d ready r1' % tabled), 'the event of a task that asks for a table')[0]['id'])
    raw_reply = next(e for e in manager_entries() if e['kind'] == 'reply' and e['id'] == table_reply['id'])
    assert 'Where things stand' in raw_reply['text'] and 'quiet' not in raw_reply and 'quiet' not in table_reply, raw_reply
    js('task', 'close', tabled)
    (notes / 'context.md').unlink()
    # ^ The config named no manager model since the restart: the manager defaults to opus.
    assert fresh['model'] == 'opus', fresh
    # ^ Claude's own turn for a notification is no reply (docs/tasks.md "Claude adapter").
    resumed = js('task', 'say', 'NOTIFY STATUS again')
    assert reply_to(resumed['id'])['text'], chat()[-3:]
    assert not any(e['kind'] == 'reply' and not e['text'] for e in chat())
    assert not any(t.get('launch') and t.get('manager') and t['session'] != fresh['session']
                   for t in traces()[traces().index(fresh) + 1:]), 'same launch keeps its conversation'
    assert not turn_with('STATUS again')['message'].startswith('Handover')
    # ^ A usage limit is no reply: the message waits and is answered after the reset.
    limited = js('task', 'say', 'LIMIT 1 STATUS limited')
    held = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'limit' and e.get('until')), None), 'the manager limit wait')
    assert re.fullmatch(r'\d\d:\d\d', held['until']), held
    answer = reply_to(limited['id'])
    assert answer['text'].startswith('open: ') and answer['unix_ms'] >= held['unix_ms'] + 1000, (held, answer)
    assert not any(e['kind'] == 'reply' and 'session limit' in e['text'] for e in chat())
    # ^ wsx says the pause and the resume in the chat, in that order, before the answer.
    shown = [(e['kind'], bool(e.get('until'))) for e in chat() if e['kind'] in ('limit', 'reply') and e['unix_ms'] >= held['unix_ms']]
    assert shown[:3] == [('limit', True), ('limit', False), ('reply', False)], shown
    assert turn_with('The session limit has reset'), 'the manager continues after its limit'
    # ^ A turn of only events that a limit held stays quiet after the reset: "-" shows nothing.
    # One with an owner message is a message (docs/web-client-tasks.md).
    for seconds, owner_says in ((1, None), (3, 'STATUS while held')):
        held_count = len([e for e in manager_entries() if e['kind'] == 'limit' and e.get('until')])
        title = js('task', 'new', 'NOOP MLIMIT %d DASH' % seconds)['id']
        event = wait(lambda: events_with('#%d ready r1' % title), 'the event the limit holds')[0]
        if owner_says:
            wait(lambda: len([e for e in manager_entries() if e['kind'] == 'limit' and e.get('until')]) > held_count, 'the second limit wait')
            owner_id = js('task', 'say', owner_says)['id']
        dash = reply_to(event['id'])
        assert dash['text'] == '-' and (owner_says is None) == (dash.get('quiet') is True), dash
        assert owner_says is None or owner_id in dash['to'], dash
        js('task', 'close', title, '--discard')
    for n in (first, second, unseen):
        js('task', 'close', n, '--discard')
    # 21b. Memo (docs/tasks.md "Memo"): each task that leaves brings its worker's memo to the
    mark('21b')
    # manager in its event, also past the ledger's 10 recent tasks.
    memo_tasks = [js('task', 'new', 'WORK m%d.txt MEMO lesson %d kept' % (i, i))['id'] for i in range(11)]
    for n in memo_tasks:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
        js('task', 'close', n, '--discard')
    for i, n in enumerate(memo_tasks):
        wait(lambda: events_with('#%d closed r1' % n, 'memo: lesson %d kept' % i), 'memo of t%d in its event' % n)
    assert len(js('task', 'ledger')['recent']) == 10

    # 22. Approvals: what the manager may not do alone becomes a request the owner decides.
    mark('22')
    # ^ By default the manager lands alone: only what goes public or is lost asks.
    free = js('task', 'new', 'WORK free.txt MEMO landing brings it')['id']
    assert wait_state(free, 'ready', 'failed')['state'] == 'ready'
    landed_alone = js('task', 'say', 'RUN wsx task land %d -p demo --json' % free)
    assert 'owner was asked' not in reply_to(landed_alone['id'])['text'] and main_has('free.txt')
    wait_closed(free)
    wait(lambda: events_with('#%d landed' % free, 'memo: landing brings it'), 'the land event carries the memo')
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
    js('task', 'deny', 'a2', '--note', 'keep it for now')
    wait(lambda: events_with('approval a2 (discard #%d) denied; keep it for now' % keep), 'deny event')
    assert show(keep)['state'] == 'ready' and Path(show(keep)['record']['working_copy']).exists()
    # The manager cannot decide its own request.
    asked = js('task', 'say', 'RUN wsx task land %d -p demo --json\nRUN wsx task approve a3 -p demo --json' % keep)
    text = reply_to(asked['id'])['text']
    assert 'the owner was asked (a3)' in text and 'is for the owner' in text, text
    # An approval for a revision the task has moved past is dropped, and the manager hears why
    # in the chat (docs/tasks.md "Approvals").
    js('task', 'add', keep, 'WORK keep2.txt')
    assert wait_state(keep, 'ready', 'failed')['revision'] == 2
    wait(lambda: events_with('approval a3 (land #%d) dropped; #%d moved from r1 to r2' % (keep, keep)), 'the drop event')
    assert 'no pending approval a3' in cli('task', 'approve', 'a3', ok=False).stderr and show(keep)['state'] == 'ready'
    assert js('task', 'ledger')['approvals'] == []
    wait_closed(ap)
    js('task', 'close', keep, '--discard')

    # 22b. The TUI's manager chat (`t`, then `m`; docs/tasks.md "TUI"): the conversation, the
    # open To check item and the waiting approval; a message goes to the manager's log, `p`
    # passes the item and `n` then `y` denies the request; 80 columns still show the message.
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

    def owner_said(text):
        lines = [json.loads(line) for line in manager_log.read_text().splitlines() if line.strip()]
        return any(e.get('kind') == 'message' and e.get('from') == 'owner' and e.get('text') == text
                   for e in lines)

    shots = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'),
        (r'\(m\)anager', b'm'),
        (r'(?s)Manager demo +1 to check  1 approval\n.*▸ %s' % item['id'], b'i'),
        (r'Enter: send', b'hello from the tui'),
        (r'> hello from the tui', b'\r'),
        # ^ "Sent" comes before the next read draws the message: wait for both.
        (r'(?ms)^ Sent$.*^   hello from the tui$', b'\x1b'),
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
    assert re.search(r'(?m)^ Manager +\d\d:\d\d$', opened) and 'MANAGER' in opened, opened
    assert owner_said('hello from the tui'), manager_log.read_text()[-2000:]
    assert item['id'] not in [i['id'] for i in js('task', 'check')['open']]
    rows = [json.loads(line) for line in checks_file.read_text().splitlines() if line.strip()]
    assert [r['state'] for r in rows if r['id'] == item['id']][-1] == 'passed', rows
    assert js('task', 'ledger')['approvals'] == [] and show(asks)['state'] == 'ready'
    narrow = tui_screens(wsx, env, project, [
        (r'WORKSPACE', b't'), (r'\(m\)anager', b'm'), (r'(?m)^ You +\d\d:\d\d$', b'\x1b'),
        (r'\(m\)anager', b'\x1b'), (r'WORKSPACE', b'')], cols=80)[2]
    assert re.search(r'(?m)^ You +\d\d:\d\d$', narrow) and 'hello from the tui' in narrow, narrow
    js('task', 'close', asks, '--discard')

    # 23. wsx's boundary (docs/tasks.md "Permissions"): one Seatbelt profile per role over the
    mark('23')
    # whole agent process tree, so this fake worker, which is not Claude, is inside it too.
    def launches_in(n):
        copy = os.path.realpath(show(n)['record']['working_copy'])
        return [t for t in traces() if t.get('launch') and os.path.realpath(t['cwd']) == copy]

    secret = home / '.ssh/id_fixture'
    secret.parent.mkdir(mode=0o700, exist_ok=True)
    secret.write_text('secret\n')
    store_dir, common = Path(created['task_dir']).parent, project / '.git'
    probe_script = work / 'probe.py'
    probe_script.write_text(PROBE_SCRIPT)

    def probe_files(n):
        """The probe's spec and answer for task n, which is created next."""
        spec = {'read': {'read_ssh': str(secret)},
                'write': {'write_ssh': str(home / '.ssh/probe'), 'write_state': str(store_dir / 'probe.txt'),
                          'write_existing_copy': str(base / 'project-t998/probe.txt'),
                          'write_later_copy': str(base / 'project-t999/probe.txt'),
                          'write_main_ref': str(common / 'refs/heads/main'),
                          'write_main_lock': str(common / 'refs/heads/main.lock'),
                          'write_main_reflog': str(common / 'logs/refs/heads/main'),
                          'write_hooks': str(common / 'hooks/pre-commit'), 'write_config': str(common / 'config'),
                          'write_routines': str(home / 'Library/Application Support/asched/projects/probe.toml'),
                          'write_own_copy': str(base / ('project-t%d' % n) / 'probe.txt'),
                          'write_task_folder': str(store_dir / str(n) / 'probe.txt'),
                          'write_tmpdir': str(tmp / 'probe.txt'), 'write_cargo': str(home / '.cargo/registry/probe.txt'),
                          'write_library': str(home / 'Library/Caches/wsx-probe/probe.txt')}}
        spec_file, answer = work / ('probe-%d.json' % n), work / ('probe-%d-answer.json' % n)
        spec_file.write_text(json.dumps(spec))
        return spec_file, answer

    def next_task():
        return max(row['id'] for row in js('task', 'list', '--all')['tasks']) + 1

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
    # The manager's `wsx task` commands run through its broker with no denial.
    held = js('task', 'new', 'WAIT %s NOOP' % (base / 'never'))['id']
    wait(lambda: show(held)['state'] == 'working' and launches_in(held), 'held worker')
    stopped = js('task', 'say', 'RUN wsx task stop %d -p demo --json' % held)
    assert 'ran: wsx task stop %d -p demo --json -> 0' % held in reply_to(stopped['id'])['text']
    assert wait_state(held, 'waiting')['record']['waiting']['on'] == 'owner'
    # A land refused on the owner's uncommitted change on main that the task's change overlaps:
    # on the owner's yes the manager commits it in the main checkout, which its boundary allows,
    # and the land passes.
    shared = project / 'shared.txt'
    shared.write_text('one\ntwo\nthree\nfour\nfive\n')
    git('add', 'shared.txt')
    git('commit', '-q', '-m', 'shared file')
    shared.write_text('one\ntwo\nthree\nfour\nowner edit\n')
    dirty = js('task', 'new', 'LINE shared.txt 1')['id']
    assert wait_state(dirty, 'ready', 'failed')['state'] == 'ready'
    refused = land(dirty, ok=False)
    assert 'shared.txt' in refused['message'] and 'land again' in refused['message'], refused
    commit = js('task', 'say', 'RUN git -c user.name=o -c user.email=o@example.invalid commit -q -m '
                               '"owner edit: shared.txt" -- shared.txt')
    assert '-> 0' in reply_to(commit['id'])['text']
    assert git('log', '-1', '--format=%s', 'main') == 'owner edit: shared.txt'
    assert land(dirty)['ok']
    wait_closed(dirty)
    assert git('show', 'main:shared.txt').splitlines()[4] == 'owner edit'
    for n in (probed, held):
        js('task', 'close', n, '--discard')

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
    js('task', 'close', r, '--discard')

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
    assert ran['check_run_other'][0] == 2 and 'check-run' in ran['check_run_other'][1], ran
    assert [len(e['results']) for e in log(cr) if e['kind'] == 'check_run'] == [2]
    worker_launch = launches_in(cr)[0]
    assert '1. `test ! -e broken.txt` 2. `test "$TASK_PROBE" = "a b"`' in worker_launch['instructions']
    assert worker_launch['task_probe'] == 'a b' and launch['task_probe'] is None, worker_launch
    assert all(c['exit'] == 0 for c in show(cr)['result']['checks']), show(cr)['result']
    assert 'WSX_TASK_BROKER' in worker_launch['wsx_env'], worker_launch['wsx_env']
    js('task', 'close', cr, '--discard')
    # Below tasks.minFreeGb a task waits, says why, and goes on by itself without a retry.
    configure(LAND_CONFIG.replace('minFreeGb: 0', 'minFreeGb: 100000'), 'no disk')
    dw = js('task', 'new', 'WORK dw.txt')['id']
    waiting = wait(lambda: (current := show(dw))['state'] == 'waiting' and current['record'].get('waiting') and current,
                   'disk wait')
    assert waiting['record']['waiting']['on'] == 'disk' and waiting['record']['waiting']['reason'].startswith('Disk full: ')
    assert any(e['kind'] == 'disk_wait' for e in log(dw))
    row = next(r for r in js('task', 'ledger')['open'] if r['id'] == dw)
    assert row['waiting']['on'] == 'disk', row
    configure(LAND_CONFIG, 'disk back')
    wait(lambda: show(dw)['state'] == 'ready', 'resumed after disk', 60)
    assert 'waiting' not in show(dw)['record'] and not any(e['kind'] == 'check_retry' for e in log(dw))
    js('task', 'close', dw, '--discard')
    # A usage limit is no failure: the task waits on `limit` and its session continues after the
    # reset, oldest wait first, one task at a time (docs/tasks.md "Claude adapter").
    first = js('task', 'new', 'LIMIT 4 WORK la.txt')['id']
    limited = lambda n: (current := show(n))['state'] == 'waiting' and current['record'].get('waiting') and current
    waiting = wait(lambda: limited(first), 'limit wait')['record']['waiting']
    assert waiting['on'] == 'limit' and waiting['reason'].startswith('session limit; resumes at '), waiting
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
    resumed = [sessions.get(t['session']) for t in traces()
               if not t.get('manager') and str(t.get('message', '')).startswith('The session limit has reset')]
    assert resumed == [first, second], resumed
    for n in (first, second):
        js('task', 'close', n, '--discard')
    # 27b. A limit wait ends with the resume whatever comes meanwhile, also for the task the
    mark('27b')
    # limit slot passes to: a background check-run ends during the wait and wsx is replaced
    # under both hosts. Nothing calls a host after the reset (`show` would wake it): only files
    # are read.
    limit_gate = base / 'limit-gate'
    configure(LAND_CONFIG + '  scenarios:\n    slow: while [ ! -e %s ]; do sleep 0.2; done\n' % limit_gate,
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
                       if not t.get('manager') and str(t.get('message', '')).startswith('The session limit has reset')]
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

    # 28. A land deploys by itself with tasks.deploy.auto; deploys run one at a time and a
    mark('28')
    # deploy that waited runs for the newest commit, so quick lands skip the ones between.
    configure(LAND_CONFIG + """  deploy:
    command: 'sleep 8; echo "$WSX_DEPLOY_COMMIT" >> ../auto-deploy.log'
    auto: true
""", 'deploy after land')
    quick = [js('task', 'new', 'WORK d%d.txt' % i)['id'] for i in range(3)]
    for n in quick:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
    tips = []
    for n in quick:
        land(n)
        tips.append(wait(lambda: show(n)['state'] == 'landed' and git('rev-parse', 'main'), 'quick land'))
    deploy_log = base / 'auto-deploy.log'
    wait(lambda: deploy_log.exists() and deploy_log.read_text().split()[-1:] == [tips[-1]], 'deploy of the newest land', 60)
    # ^ The deploy that waited behind the newest one skips it at once.
    time.sleep(3)
    deployed_commits = deploy_log.read_text().split()
    assert deployed_commits[0] == tips[0] and deployed_commits[-1] == tips[-1] and tips[1] not in deployed_commits, (tips, deployed_commits)
    assert js('task', 'deployed')['deploy']['commit'] == tips[-1]
    for n in quick:
        wait_closed(n)
    follow_up = state / 'wsx/tasks' / Path(created['task_dir']).parent.name / 'follow-up.log'
    assert 'already deployed' in follow_up.read_text(), follow_up.read_text()[-2000:]

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
    assert 'only a task worked by hand' in cli('task', 'handback', handed, '--json', ok=False).stderr
    queued = js('task', 'add', handed, 'WORK queued.txt')
    assert 'queued' in queued['message'], queued
    for command in ('hand', 'handback'):
        refused = subprocess.run([str(wsx), 'task', command, str(handed)], cwd=project, env=manager_env,
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
    for argv, word in ((('add', handed, 'WORK no.txt'), 'handback'), (('steer', handed, 'x'), 'handback'),
                       (('land', handed), 'handback')):
        refused = cli('task', *argv, '--json', ok=False)
        assert word in refused.stdout + refused.stderr, refused
    time.sleep(1.5)
    assert show(handed)['record']['waiting']['on'] == 'hand'
    assert not kinds(handed, 'checks', 'revision', 'result'), log(handed)
    assert not any(t.get('message') in ('WORK no.txt', 'WORK queued.txt') for t in traces())
    assert not (hand_copy / 'late.txt').exists()
    # Uncommitted tracked work refuses the handback, and the pane stays.
    (hand_copy / 'README').write_text('by hand\n')
    refused = cli('task', 'handback', handed, '--json', ok=False)
    assert 'uncommitted changes: README' in refused.stderr, refused.stderr
    assert show(handed)['record']['waiting']['on'] == 'hand' and len(hand_sessions()) == 1
    (hand_copy / 'hand.txt').write_text('made by hand\n')
    git('add', 'README', 'hand.txt', cwd=hand_copy)
    git('commit', '-q', '-m', 'by hand', cwd=hand_copy)
    back = js('task', 'handback', handed)
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
    spans, release = base / 'slot-spans.log', base / 'slot-release'
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
        'mark("end")\n' % (str(spans), str(release)))

    def slot_config(seconds):
        configure(LAND_CONFIG.replace(
            '"test ! -e broken.txt"', '"%s %s %d"' % (sys.executable, slot_check, seconds)), 'slot checks %d' % seconds)

    def slot_marks():
        marks = {}
        for line in spans.read_text().splitlines() if spans.exists() else []:
            copy_name, what, at = line.split()
            marks.setdefault(copy_name, {})[what] = float(at)
        return marks

    def copy_name(n):
        return Path(show(n)['record']['working_copy']).name

    def queued_row(pair):
        rows = [r for r in js('task', 'ledger')['open'] if r['id'] in pair]
        return next((r for r in rows if r['state'] == 'checking' and (r['waiting'] or {}).get('on') == 'check_slot'), None)

    slot_config(3)
    pair = [js('task', 'new', 'WORK slot%d.txt' % i)['id'] for i in (1, 2)]
    queued = wait(lambda: queued_row(pair), 'a queued check run', 30)
    assert queued['waiting']['reason'].startswith('queued for the check slot behind demo t'), queued
    for n in pair:
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
        assert 'waiting' not in show(n)['record']
    assert [e['reason'] for e in log(queued['id']) if e['kind'] == 'check_queued'], log(queued['id'])
    first, second = sorted((slot_marks()[copy_name(n)] for n in pair), key=lambda span: span['start'])
    assert first['end'] <= second['start'], (first, second)
    for n in pair:
        js('task', 'close', n, '--discard')
    spans.unlink()
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
    # A worker's check-run waits for the slot with no client timeout; a client that gave up and
    # asks again joins the run it left, so the checks run once, never beside each other.
    spans.unlink()
    release.unlink()
    holder = js('task', 'new', 'WORK slot5.txt')['id']
    wait(lambda: copy_name(holder) in slot_marks(), 'the holder runs its checks', 30)
    again = js('task', 'new', 'CHECKRUN_AGAIN WORK slot6.txt')['id']
    wait(lambda: [e for e in log(again) if e['kind'] == 'check_queued'], 'the check-run queued', 30)
    # ^ The first client is killed 1 s after it started, while its run waits.
    time.sleep(2)
    release.touch()
    assert wait_state(again, 'ready', 'failed')['state'] == 'ready'
    code, said = next(t for t in traces() if 'check_run_again' in t)['check_run_again']
    assert code == 0 and ': exit 0' in said, said
    assert kinds(again, 'check_run') == ['check_run'], log(again)
    # ^ A run that ended before the result starts no turn of its own.
    assert kinds(again, 'result') == ['result'], log(again)
    # ^ One start for the check-run and one for the checks after the result.
    starts = [line for line in spans.read_text().splitlines() if line.startswith(copy_name(again) + ' start')]
    assert len(starts) == 2, spans.read_text()
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
    (project / 'main-moved.txt').write_text('main moved\n')
    git('add', 'main-moved.txt')
    git('commit', '-q', '-m', 'main moves')
    assert 'checks run on the new revision' in land(moved)['message']
    assert wait_state(moved, 'landed', 'failed')['state'] == 'landed' and len(kinds(moved, 'checks')) == ran + 1, log(moved)
    wait_closed(moved)
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
    # 32t. A result that comes before its own check-run's answer waits for it: the worker gets
    # the answer in one turn, and only its next result runs the checks.
    mark('32t')
    late_gate = base / 'late-gate'
    configure(LAND_CONFIG + '  scenarios:\n    slow: while [ ! -e %s ]; do sleep 0.2; done\n' % late_gate, 'a slow scenario')
    late = js('task', 'new', 'CHECKRUN_LATE slow WORK late.txt')['id']
    wait(lambda: kinds(late, 'result'), 'the result before the answer', 30)
    time.sleep(1)
    assert show(late)['state'] == 'working' and not kinds(late, 'checks'), log(late)
    late_gate.touch()
    assert wait_state(late, 'ready', 'failed')['state'] == 'ready', log(late)
    assert kinds(late, 'result', 'check_run', 'checks') == ['result', 'check_run', 'result', 'checks'], log(late)
    answers = [t['message'] for t in traces() if 'Your check-run ended after your result' in t.get('message', '')]
    assert len(answers) == 1 and 'check 2 `while [ ! -e' in answers[0] and ': exit 0' in answers[0], answers
    js('task', 'close', late, '--discard')
    # 32u. A result that comes while the checks of an earlier one run (Claude's own turn for a
    # background notice) is answered by those checks: one result, one run, one revision.
    mark('32u')
    gate_open = base / 'gate-open'
    configure(LAND_CONFIG.replace('"test ! -e broken.txt"', '"while [ ! -e %s ]; do sleep 0.2; done"' % gate_open), 'a slow gate')
    twice = js('task', 'new', 'REPEAT WORK twice.txt')['id']
    wait(lambda: kinds(twice, 'result') == ['result'] * 2, 'the repeated result', 30)
    assert show(twice)['state'] == 'checking', show(twice)
    gate_open.touch()
    assert wait_state(twice, 'ready', 'failed')['state'] == 'ready', log(twice)
    time.sleep(1)
    assert kinds(twice, 'checks', 'revision') == ['checks', 'revision'], log(twice)
    js('task', 'close', twice, '--discard')
    configure(LAND_CONFIG, 'no scenario')
    # 32d. The disk queue (docs/tasks.md "Checks, revisions and report"): tasks that wait for
    mark('32d')
    # disk resume one at a time, oldest first, so they do not fill the freed space at once.
    release.unlink()
    slot_config(120)
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
    release.touch()
    assert wait_state(older, 'ready', 'failed')['state'] == 'ready'
    assert wait(lambda: (current := show(newer))['state'] in ('ready', 'failed') and current,
                'the newer disk wait resumed', 60)['state'] == 'ready'
    for n in (older, newer):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'disk as before')
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
    configure(LAND_CONFIG, 'release as before')
    # 32g. A plan first (docs/tasks.md "Workflow"): the first turn of a `--plan` task is told to
    # plan and stop; its result runs no check and waits on plan for the manager, and an add
    # continues it as an ordinary task.
    planned = js('task', 'new', '--plan', 'NOOP plan the change')['id']
    waiting = wait_state(planned, 'waiting', 'ready', 'failed')
    assert waiting['record']['waiting'] == {'on': 'plan', 'reason': 'its plan waits for the manager'}, waiting['record']
    assert 'plan' not in waiting['record'], waiting['record']
    assert not kinds(planned, 'checks', 'revision'), log(planned)
    assert 'First write a short plan and stop' in next(e for e in log(planned) if e['kind'] == 'result')['summary']
    wait(lambda: events_with('#%d waiting r0' % planned, 'waits on plan'), 'the plan event')
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
        js('task', 'close', auto, '--discard')
        fake.write_text('#!/bin/sh\nexec ' + sys.executable + ' ' + str(fake_script) + ' "$@"\n')
        configure(LAND_CONFIG, 'fake claude')

    # 32h. A dirty main checkout (docs/tasks.md "Landing"): a changed file the land does not
    # touch stays dirty and the land goes on; one the land changes too refuses it, naming only
    # that file, and the ledger named it before the press. The message keeps the path's '/'.
    mark('32h')
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
    configure(LAND_CONFIG.replace('"test ! -e broken.txt"', '"while [ ! -e %s ]; do sleep 0.2; done"' % gate),
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
    # The second child rebases onto the moved parent, is checked, lands, and the parent checks again.
    assert 'checks run on the new revision' in land(second)['message']
    assert wait_state(second, 'landed', 'ready', 'failed')['state'] == 'landed', log(second)
    wait(lambda: (r := show(parent))['state'] == 'ready' and r['record']['revision'] == 3, 'the parent r3', 30)
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

    # 32r. Roles (docs/tasks.md "Roles"): a role is the manager host with a name. wsx, not the
    mark('32r')
    # prompt, guards their number: the manager asks the owner once, at most tasks.roles.max are
    # open, a denied name waits 7 days, and an idle role closes. A role hears only its own tasks.
    configure(LAND_CONFIG + '  roles: { max: 2 }\n', 'two roles')
    roles_dir = Path(created['task_dir']).parent / 'roles'
    brief = base / 'role-brief.md'
    brief.write_text('Drive the billing rewrite.\n')

    def roles():
        return {r['name']: r for r in js('task', 'role', 'list')['roles']}

    def role_entries(name):
        path = roles_dir / name / 'log.jsonl'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def role_reply(name, message_id):
        return wait(lambda: next((e for e in role_entries(name) if e['kind'] == 'reply' and message_id in e['to']), None),
                    'reply of role %s to %s' % (name, message_id), 60)

    assert 'lowercase letter' in cli('task', 'role', 'new', 'Bad_Name', '--brief', brief, ok=False).stderr
    assert 'not a role to create' in cli('task', 'role', 'new', 'manager', '--brief', brief, ok=False).stderr
    # The owner opens a role at once.
    opened = js('task', 'role', 'new', 'billing', '--brief', brief)
    assert opened['state'] == 'open' and (roles_dir / 'billing/brief.md').read_text() == brief.read_text(), opened
    assert 'already open' in cli('task', 'role', 'new', 'billing', '--brief', brief, ok=False).stderr
    # The manager's role new is a request: one approval, which counts against the cap.
    asked = js('task', 'say', 'RUN wsx task role new qa --brief %s -p demo --json' % brief)
    text = reply_to(asked['id'])['text']
    assert 'role new qa needs the owner' in text and 'the owner was asked (a' in text, text
    pending = [a for a in js('task', 'ledger')['approvals'] if a['action'] == 'role']
    assert [(a['role'], a['brief']) for a in pending] == [('qa', brief.read_text())], pending
    qa_request = pending[0]['id']
    assert roles()['qa']['state'] == 'asked' and not (roles_dir / 'qa').exists(), roles()
    capped = cli('task', 'role', 'new', 'docs', '--brief', brief, ok=False).stderr
    assert 'has 2 of tasks.roles.max 2 roles (billing, qa (%s, asked))' % qa_request in capped, capped
    # The owner denies it: that name refuses for 7 days, also for the owner.
    js('task', 'deny', qa_request, '--note', 'not now')
    wait(lambda: events_with('approval %s (role qa) denied; not now' % qa_request), 'the role deny event')
    denied = cli('task', 'role', 'new', 'qa', '--brief', brief, ok=False).stderr
    assert 'the owner denied role qa; it cannot be asked for again for 168 more hour(s)' in denied, denied
    # The owner's word in the chat opens one at once, recorded as approved by that message.
    said = js('task', 'say', 'open a docs role\nRUN wsx task role new docs --brief %s --owner-said OWNER_SAID -p demo --json'
              % brief)
    text = reply_to(said['id'])['text']
    assert 'role docs is open' in text and 'owner was asked' not in text, text
    assert {name: r['state'] for name, r in roles().items()} == {'billing': 'open', 'docs': 'open'}, roles()
    assert 'tasks.roles.max 2' in cli('task', 'role', 'new', 'extra', '--brief', brief, ok=False).stderr
    # A role is the manager host with its brief, its own folder, session and actor; it starts
    # on say. What only the owner or the manager does is refused to it.
    manager_task = js('task', 'new', 'WORK owner-role.txt')['id']
    talk = js('task', 'say', '--role', 'billing',
              'RUN wsx task new "WORK role.txt" -p demo --json\n'
              'RUN wsx task role new more --brief %s -p demo --json\n'
              'RUN wsx task land %d -p demo --json\n'
              'RUN wsx task deploy -p demo --json\n'
              'RUN wsx task routine list -p other --json' % (brief, manager_task))
    text = role_reply('billing', talk['id'])['text']
    assert 'role new is not for a role' in text and 'acts only on the tasks it created; #%d is the owner' % manager_task in text, text
    assert 'deploy is not for a role' in text and 'a manager acts only in its own project' in text, text
    role_task = max(t['id'] for t in js('task', 'list')['tasks'])
    assert show(role_task)['record']['created_by'] == 'role:billing', show(role_task)['record']
    role_launch = next(t for t in traces() if t.get('launch') and t.get('actor') == 'role:billing')
    assert role_launch['instructions'].startswith('You are billing, a role in the project demo'), role_launch
    assert 'Drive the billing rewrite.' in role_launch['instructions'] and '/roles/billing/notes' in role_launch['instructions']
    assert role_launch['wsx_env'] == ['WSX_TASK_ACTOR', 'WSX_TASK_BROKER'], role_launch
    assert roles()['billing']['running'], roles()
    # Only the creating role hears its task's events; the manager hears its own.
    assert wait_state(role_task, 'ready', 'failed')['state'] == 'ready'
    assert wait_state(manager_task, 'ready', 'failed')['state'] == 'ready'
    wait(lambda: any(e['kind'] == 'message' and e['from'] == 'events' and '#%d ready' % role_task in e['text']
                     for e in role_entries('billing')), 'the role hears its task', 60)
    wait(lambda: events_with('#%d ready' % manager_task), 'the manager hears its task')
    time.sleep(4)
    assert not events_with('#%d ' % role_task), [e['text'] for e in manager_entries() if e['kind'] == 'message']
    assert not any('#%d ' % manager_task in e.get('text', '') for e in role_entries('billing') if e['kind'] == 'message')
    # A role lands only a child into a parent; its own task with no parent is the manager's to land.
    landed = role_reply('billing', js('task', 'say', '--role', 'billing',
                                      'RUN wsx task land %d -p demo --json' % role_task)['id'])['text']
    assert 'a role lands only a child into its parent task' in landed, landed
    # Roles talk inside the project as managers do: one answer to each message.
    asked = js('task', 'say', '--role', 'billing', "SCRIPT\nwsx task say -p demo --from-role --json - <<'EOF'\n"
               "RUN wsx task say -p demo --role billing --from-role --reply 'role answer'\n"
               "RUN wsx task say -p demo --role billing --from-role --reply 'second answer'\nEOF")
    assert role_reply('billing', asked['id'])['text'].startswith('script -> 0')
    received = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'role:billing'), None),
                    'the role message at the manager', 60)
    answered = wait(lambda: next((e for e in manager_entries() if e['kind'] == 'reply' and received['id'] in e['to']), None),
                    'the manager answers the role', 60)
    assert "--reply 'role answer' -> 0" in answered['text'] and 'no message from billing waits' in answered['text'], answered
    wait(lambda: [e for e in role_entries('billing') if e['kind'] == 'message' and e['from'] == 'role-reply:manager'
                  and e['text'] == 'role answer'], 'the answer at the role', 60)
    assert 'not itself' in role_reply('billing', js('task', 'say', '--role', 'billing',
                                                    'RUN wsx task say -p demo --role billing --from-role hi')['id'])['text']
    assert 'is for the manager or a role' in cli('task', 'say', '--from-role', 'hello', ok=False).stderr
    # An idle role's host leaves (no agent, no pane); the next say starts it again.
    wait(lambda: not roles()['billing']['running'], 'the idle role host leaves', 60)
    back = js('task', 'say', '--role', 'billing', 'STATUS')
    assert role_reply('billing', back['id']) and roles()['billing']['running']
    assert [e['id'] for e in js('task', 'chat', '--role', 'billing')['entries'] if e['kind'] == 'message'][-1] == back['id']
    # The TUI's Tasks view lists the open roles under Open; Enter opens a role's chat as `m`
    # opens the manager's (docs/tasks.md "TUI"), without the manager's items.
    below = len(js('task', 'list')['tasks'])
    for cols in (120, 80):
        shots = tui_screens(wsx, env, project, [
            (r'WORKSPACE', b't'),
            (r'(?ms)^ Roles$.*^ billing +open +Drive the billing', b'j' * below),
            (r'\(enter\)chat', b'\r'),
            (r'(?ms)^ billing +\d\d:\d\d$.*ROLE', b'\x1b'),
            (r'\(m\)anager', b'\x1b'),
            (r'WORKSPACE', b'')], cols=cols)
        talk = shots[3]
        assert 'to check' not in talk and re.search(r'(?m)^ billing +\d\d:\d\d$', talk), talk
        assert 'role answer' in talk or 'STATUS' in talk, talk
    # role close keeps the notes; the role's tasks go back to the manager. A role with no turn for
    # tasks.roles.closeDays is closed by wsx, and the manager hears it.
    (roles_dir / 'billing/notes/context.md').write_text('billing notes\n')
    assert js('task', 'role', 'close', 'billing')['state'] == 'closed'
    assert (roles_dir / 'billing/notes/context.md').read_text() == 'billing notes\n'
    wait(lambda: not roles()['billing']['running'], 'the closed role host leaves', 30)
    assert 'no open role billing' in cli('task', 'say', '--role', 'billing', 'hi', ok=False).stderr
    js('task', 'add', role_task, 'WORK role2.txt')
    wait(lambda: events_with('#%d ready r2' % role_task), 'a closed role\'s task reaches the manager', 60)
    configure(LAND_CONFIG + '  roles: { max: 2, closeDays: 0.0001 }\n', 'idle roles close')
    wait(lambda: events_with('role docs closed by wsx: no turn for tasks.roles.closeDays; its notes are kept'),
         'the idle role closes', 60)
    assert roles()['docs']['state'] == 'closed', roles()
    for n in (manager_task, role_task):
        js('task', 'close', n, '--discard')
    configure(LAND_CONFIG, 'no roles')

    # 32v. A turn that ends while a background job runs is no missing result (docs/tasks.md
    # "Claude adapter"): the host waits for Claude's own turn after the job's notice.
    mark('32v')
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
    mark('32r')
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
    assert results()[0]['usage']['calls'] == 1, results()[0]
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

    # 33. Managers talk (docs/tasks.md "Managers talk"): a manager reads another project's
    mark('33')
    # ledger and messages its manager, which answers once; nothing else crosses projects.
    other = base / 'other'
    other.mkdir(mode=0o700)
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
    asked = js('task', 'say', "SCRIPT\nwsx task ledger -p other --json >/dev/null && wsx task say -p other --from-manager --json - <<'EOF'\n"
               "RUN wsx task say -p demo --from-manager --reply 'first answer'\n"
               "RUN wsx task say -p demo --from-manager --reply 'second answer'\nEOF")
    assert reply_to(asked['id'])['text'].startswith('script -> 0'), reply_to(asked['id'])

    def other_entries():
        return [json.loads(line) for line in other_log.read_text().splitlines()] if other_log.exists() else []

    received = wait(lambda: next((e for e in other_entries() if e['kind'] == 'message' and e['from'] == 'manager:demo'), None),
                    'message at the other manager', 60)
    assert 'first answer' in received['text']
    answered = wait(lambda: next((e for e in other_entries() if e['kind'] == 'reply' and received['id'] in e['to']), None),
                    'the other manager answers', 60)
    assert "--reply 'first answer' -> 0" in answered['text'] and 'waits for an answer' in answered['text'], answered
    # ^ README "Profiles": other is in the default profile, so its manager keeps the host's account.
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
    cells = screen(header[0], 120, styled=True)[0]
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
    crossing = js('task', 'say', 'RUN wsx task new "WORK x.txt" -p other --json\nRUN wsx task say -p demo --from-manager hi')
    text = reply_to(crossing['id'])['text']
    assert 'a manager acts only in its own project' in text and "messages another project's manager" in text, text
    assert "is for a project's manager" in cli('task', 'say', '-p', 'other', '--from-manager', 'hello', ok=False).stderr

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
    # 34. Recap (docs/tasks.md "Recap"): once the other project is quiet for its 12 s after a
    mark('34')
    # land, wsx asks its manager for one recap with the token cost since the last one, and the
    # manager answers it in the chat.
    def recaps():
        return [e for e in other_entries() if e['kind'] == 'message' and e['from'] == 'recap']

    recap = wait(lambda: next(iter(recaps()), None), 'the recap request after a quiet period', 120)
    assert 'at most 8 lines' in recap['text'] and 'Start no' in recap['text'], recap
    assert 'Token cost since the last recap: ' in recap['text'] and 'workers ' in recap['text'], recap
    assert 'in 1 turn(s) (#%d ' % other_land in recap['text'], recap
    wait(lambda: next((e for e in other_entries() if e['kind'] == 'reply' and recap['id'] in e['to']), None),
         'the manager answers the recap', 60)
    # 35. Idle task (docs/tasks.md "Workflow"): a quiet period with no change asks for no recap
    mark('35')
    # (an owner message is no change), and with no session notes newer than the last idle task
    # it starts no idle task and says nothing. A new session note starts one task, as the
    # manager; its close is a change, so one more recap comes, and the older note starts no
    # second idle task.
    assert [t['id'] for t in other_tasks()] == [other_land], other_tasks()
    idle_text = 'WORK idle.txt MEMO knowledge folded'
    (other / 'wsx.config.yml').write_text(other_config + '  workflow:\n    idle: "%s"\n' % idle_text)
    git('commit', '-q', '-am', 'idle task', cwd=other)

    def idle_tasks():
        return [t for t in other_tasks() if t.get('title') == 'Consolidate project knowledge']

    nudge = js('task', 'say', '-p', 'other', 'STATUS')
    wait(lambda: next((e for e in other_entries() if e['kind'] == 'reply' and nudge['id'] in e['to']), None),
         'the other manager answers', 60)
    time.sleep(20)
    assert len(recaps()) == 1 and not idle_tasks(), (recaps(), other_tasks())
    # ^ The owner has workflow steps; the idle task still runs as one turn.
    owner_config.write_text('tasks:\n  workflow:\n    steps:\n      - name: plan\n        prompt: PLAN only\n')
    notes = other / 'knowledge/sessions/other'
    notes.mkdir(parents=True)
    (notes / 'session-1.md').write_text('a lesson\n')
    idle = wait(idle_tasks, 'the idle task after a new session note', 150)
    assert len(idle) == 1 and len(other_tasks()) == 2 and len(recaps()) == 1, (other_tasks(), recaps())
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
    assert len(idle_tasks()) == 1 and len(other_tasks()) == 2, other_tasks()

if __name__ == '__main__':
    main()
