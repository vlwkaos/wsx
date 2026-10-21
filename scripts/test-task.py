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

ROOT = Path(__file__).resolve().parents[1]
# ^ Parts 1-22 build at once (`brief: false`); part 23 turns the brief on. Parts other than 16
# land by hand (`land.auto: false`) so a ready task stays ready.
# ^ `minFreeGb: 0`: the fixture must not wait on the free disk of the machine it runs on.
CONFIG = '''tasks:
  brief: false
  minFreeGb: 0
  land: { auto: false }
  agent:
    model: fake-model
    tools: [WebFetch]
  checks:
    - "test -f a.txt"
    - "test ! -e broken.txt"
'''

FAKE_GH = r'''
import json, os, sys
args = sys.argv[1:]
entry = {'args': args, 'cwd': os.getcwd()}
if args[:2] == ['pr', 'create']:
    entry['body'] = open(args[args.index('--body-file') + 1]).read()
    count = sum(1 for line in open('LOG')) if os.path.exists('LOG') else 0
    with open('LOG', 'a') as out:
        out.write(json.dumps(entry) + '\n')
    print('Creating pull request')
    print('https://example.invalid/pr/%d' % (count + 1))
elif args[:2] == ['pr', 'view']:
    with open('LOG', 'a') as out:
        out.write(json.dumps(entry) + '\n')
    states = json.load(open('STATE')) if os.path.exists('STATE') else {}
    print(json.dumps(states.get(args[2], {'state': 'OPEN', 'mergeCommit': None})))
else:
    sys.exit('fake gh: unsupported ' + ' '.join(args))
'''

LAND_CONFIG = '''tasks:
  brief: false
  minFreeGb: 0
  land: { auto: false }
  agent:
    tools: [WebFetch]
  checks:
    - "test ! -e broken.txt"
'''


def tui_text(wsx, env, cwd, until, seconds=15):
    """The real TUI in a private PTY (120x40): its output as text without escapes, read until
    `until` (a regex) matches or the time is up, then the TUI quits."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
    proc = subprocess.Popen([str(wsx)], cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave,
                            start_new_session=True)
    output, text, deadline = bytearray(), '', time.monotonic() + seconds
    try:
        while time.monotonic() < deadline and proc.poll() is None:
            if select.select([master], [], [], 0.1)[0]:
                output += os.read(master, 65536)
                # ^ Cells of one style are written in one run; escapes split the runs.
                text = re.sub(r'\x1b(\[[0-9;?<>=]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|[()][0-9A-Za-z]|.)', '\n',
                              output.decode('utf-8', 'replace'))
                if re.search(until, text):
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
    return text


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
    owned = {}
    try:
        run(wsx, wsxd, work, owned)
        print(json.dumps({'ok': True, 'seconds': round(time.monotonic() - started, 1)}))
    finally:
        # ^ Stop the isolated wsxd also after a failure: its hosts and fake agents end with it.
        if 'env' in owned:
            subprocess.run([str(wsx), 'daemon', 'stop', '--force'], env=owned['env'],
                           capture_output=True, timeout=20)
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
    (config_dir / 'config-v2.toml').write_text(
        'resume_agents_on_restore = false\n[[projects]]\nname = "demo"\npath = '
        + json.dumps(str(project)) + '\n')
    fake = bin_dir / 'claude'
    fake.write_text('#!/bin/sh\nexec ' + sys.executable + ' ' + str(ROOT / 'scripts/fake-claude-stream.py') + ' "$@"\n')
    fake.chmod(0o755)
    daemon_socket = work / 'd.sock'
    env = {'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(work / 'c'),
           'XDG_CACHE_HOME': str(work / 'k'), 'WSX_DAEMON_BIN': str(wsxd), 'WSX_SOCKET': str(daemon_socket),
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
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(5)
            client.connect(str(daemon_socket))
            stream = client.makefile('rb')
            client.sendall(b'{"method":"hello","params":{"protocol":16}}\n')
            stream.readline()
            client.sendall(json.dumps({'method': method}).encode() + b'\n')
            return json.loads(stream.readline())

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

    def leftovers(marker):
        ps = subprocess.run(['ps', '-axo', 'pid=,command='], capture_output=True, text=True).stdout
        return [line for line in ps.splitlines() if marker in line and 'ps -axo' not in line]

    # Refusals leave nothing: bad config, then bad ids.
    (project / 'wsx.config.yml').write_text('tasks:\n  agent:\n    adapter: nope\n')
    refused = cli('task', 'new', 'WORK a.txt', ok=False)
    assert 'tasks.agent.adapter' in refused.stderr and 'no task was created' in refused.stderr, refused.stderr
    # ^ The sandbox replaces Bash rules; a bare file-tool rule would skip the classifier.
    for tools, word in (('["Bash(make:*)"]', 'the sandbox replaces Bash rules'), ('[Write]', 'file tools are always on')):
        (project / 'wsx.config.yml').write_text('tasks:\n  agent:\n    tools: %s\n' % tools)
        assert word in cli('task', 'new', 'WORK a.txt', ok=False).stderr
    # ^ The manager's extra tools have the same refusals.
    for tools in ('["Bash(git:*)"]', '[Edit]', '[MultiEdit]'):
        (project / 'wsx.config.yml').write_text('tasks:\n  manager:\n    tools: %s\n' % tools)
        assert 'tasks.manager.tools has' in cli('task', 'new', 'WORK a.txt', ok=False).stderr
    # ^ The manager's name is one short line (docs/tasks.md "Config").
    for name, word in (('"At\\nlas"', 'control characters'), ('"%s"' % ('x' * 65), 'at most 64 bytes')):
        (project / 'wsx.config.yml').write_text('tasks:\n  manager:\n    name: %s\n' % name)
        refused = cli('task', 'new', 'WORK a.txt', ok=False).stderr
        assert 'tasks.manager.name' in refused and word in refused, refused
    (project / 'wsx.config.yml').write_text('tasks:\n  agent:\n    briefTools: [Read]\n')
    assert 'briefTools was removed' in cli('task', 'new', 'WORK a.txt', ok=False).stderr
    (project / 'wsx.config.yml').write_text('tasks:\n  sandbox:\n    allowWrite: ["~/"]\n')
    assert 'tasks.sandbox.allowWrite' in cli('task', 'new', 'WORK a.txt', ok=False).stderr
    (project / 'wsx.config.yml').write_text(CONFIG + '  sandbox:\n    allowedDomains: [crates.io]\n    allowWrite: ["~/.cargo/registry"]\n')
    assert 'invalid task number' in cli('task', 'show', '../1', ok=False).stderr
    assert 'a prompt is required' in cli('task', 'new', ok=False).stderr
    assert 'empty' in cli('task', 'new', '-', stdin='\n', ok=False).stderr

    # 1. New: a working turn in its own working copy.
    created = js('task', 'new', 'SLOW WORK a.txt')
    assert created['id'] == 1 and created['state'] == 'working', created
    copy = Path(created['working_copy'])
    assert copy == base / 'project-t1' and created['branch'] == 'wsx/t1'
    assert (copy / 'target/warm').read_text() == 'cached'
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
    # ^ One wsx variable: the host's broker for `wsx task check-run`, its one reachable socket.
    assert launch['wsx_env'] == ['WSX_TASK_BROKER'] and launch['allowed'] == ['Read', 'Glob', 'Grep', 'WebFetch'] and launch['schema']
    worker_sockets = launch['settings']['sandbox']['network']['allowUnixSockets']
    assert len(worker_sockets) == 1 and Path(worker_sockets[0]).name.startswith('w-') and Path(worker_sockets[0]).is_socket()
    assert launch['model'] == 'fake-model' and not launch['resume'] and launch['mode'] == 'auto'
    assert "put in the result's memo a short, lean memo of durable" in launch['instructions'], launch['instructions']
    assert 'memo' in launch['schema_required'], launch['schema_required']
    # One permission model (docs/tasks.md "Permissions"): the sandbox bounds Bash, never a Bash rule.
    sandbox, common = launch['settings']['sandbox'], Path(git('rev-parse', '--path-format=absolute', '--git-common-dir'))
    assert (sandbox['enabled'], sandbox['failIfUnavailable'], sandbox['autoAllowBashIfSandboxed'],
            sandbox['allowUnsandboxedCommands']) == (True, True, True, False), sandbox
    assert sandbox['network']['allowedDomains'] == ['crates.io'] and 'excludedCommands' not in sandbox
    admin = Path(git('rev-parse', '--path-format=absolute', '--git-dir', cwd=copy))
    assert sandbox['filesystem']['allowWrite'] == [str(common / 'objects'), str(admin), str(common / 'refs/heads/wsx'),
                                                    str(common / 'logs/refs/heads/wsx'), '~/.cargo/registry'], sandbox
    assert sandbox['filesystem']['denyWrite'] == [str(common / 'hooks'), str(common / 'config'), str(common / 'refs/heads/main'),
                                                   str(common / 'logs/refs/heads/main')], sandbox
    deny = launch['settings']['permissions']['deny']
    # ^ The main checkout's entries other than .git: a rule on all of it would also stop commits.
    assert 'Edit(/%s)' % (project / 'README') in deny and 'Edit(/%s/**)' % (project / 'target') in deny, deny
    assert not any(rule in ('Edit(/%s/.git/**)' % project, 'Edit(/%s/**)' % project) for rule in deny), deny
    assert 'Edit(/%s/**)' % (state / 'wsx') in deny, deny
    assert 'Edit(/%s/**)' % (home / 'Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'c/wsx') in deny
    assert not any(str(copy) in rule for rule in deny), deny
    assert launch['settings']['attribution'] == {'commit': '', 'pr': ''}

    # 2. Steer during the turn: Claude does not save it, so the host logs it.
    steered = js('task', 'steer', '1', 'use capitals')
    assert steered['ok'] and 'steered' in steered['message'], steered
    ready = wait_state(1, 'ready', 'failed')
    assert ready['state'] == 'ready', ready
    assert any(t.get('steer_in_turn') == 'use capitals' for t in traces())
    assert [e['mode'] for e in log(1) if e['kind'] == 'message'] == ['prompt', 'steer']
    assert [(e['by'], e['actor']) for e in log(1) if e['kind'] == 'brief_skipped'] == [('config', 'owner')]
    assert not launch['briefing'] and launch['disallowed'] == []
    assert ready['result']['checks'] and all(c['exit'] == 0 for c in ready['result']['checks'])
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

    # 3. Add after the turn, blocked on a denied tool, then allow resumes the same conversation.
    added = js('task', 'add', '1', 'NEEDS WebSearch WORK b.txt')
    assert added['ok'] and 'new turn' in added['message'], added
    blocked = wait_state(1, 'blocked')
    assert any(e['kind'] == 'blocked' and e['denials'] == ['Bash(WebSearch)'] for e in log(1)), log(1)
    wait(lambda: (pane_agent(1) or {}).get('state') == 'blocked', 'agent blocked')
    assert blocked['revision'] == 1
    for rule in ('Bash(make:*)', 'Edit'):
        refused = cli('task', 'allow', '1', rule, '--json', ok=False)
        assert 'the sandbox replaces Bash rules' in refused.stdout + refused.stderr or 'file tools' in refused.stdout + refused.stderr, refused
    allowed = js('task', 'allow', '1', 'WebSearch')
    assert allowed['ok'] and 'resumed' in allowed['message'], allowed
    ready = wait_state(1, 'ready', 'failed')
    assert ready['state'] == 'ready' and ready['revision'] == 2, ready
    relaunch = [t for t in traces() if t.get('launch')][-1]
    assert relaunch['resume'] and relaunch['session'] == launch['session']
    assert relaunch['allowed'] == ['Read', 'Glob', 'Grep', 'WebFetch', 'WebSearch'], relaunch['allowed']

    # 3b. Denials the worker worked around do not block: checks run and the denials are evidence.
    js('task', 'add', '1', 'DENIED rm-tmp WORK d.txt')
    ready = wait_state(1, 'ready', 'failed', 'blocked')
    assert ready['state'] == 'ready', ready
    assert ready['result']['denials'] == ['Bash(rm-tmp)'], ready['result']
    report = Path(ready['report']).read_text()
    assert report.index('`Bash(rm-tmp)`') > report.index('## Evidence'), report

    # 3c. A stdout line past the reader's 4 MiB bound is dropped and the turn still ends.
    js('task', 'add', '1', 'HUGE NOOP')
    assert wait_state(1, 'ready', 'failed')['state'] == 'ready'
    assert not any(len(e.get('text', '')) > 1000 for e in log(1) if e['kind'] == 'agent')

    # 4. A failing check makes the task failed with the evidence.
    js('task', 'add', '1', 'WORK broken.txt')
    failed = wait_state(1, 'failed')
    assert [c['exit'] for c in failed['result']['checks']] == [0, 1], failed['result']
    assert '`test ! -e broken.txt`: exit 1' in Path(failed['report']).read_text()
    git('rm', '-q', 'broken.txt', cwd=copy)
    git('commit', '-q', '-m', 'drop broken', cwd=copy)

    # 5. Live daemon handoff while the host runs a turn.
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
    js('task', 'add', '1', 'SLOW NOOP')
    wait_state(1, 'working')
    stopped = js('task', 'stop', '1')
    assert stopped['ok'] and stopped['state'] == 'stopped', stopped
    assert any(t.get('interrupted') for t in traces())
    js('task', 'add', '1', 'NOOP')
    wait_state(1, 'ready', 'failed')
    last = [t for t in traces() if t.get('launch')][-1]
    assert last['resume'] and last['session'] == launch['session']

    # 7. Close refuses unlanded commits and changes nothing.
    refused = cli('task', 'close', '1', ok=False)
    assert 'not on main' in refused.stderr and '--discard' in refused.stderr, refused.stderr
    assert show(1)['host_running'] and copy.exists()

    # 8. A task without commits closes without --discard; nothing is left.
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
    closed = js('task', 'close', '1', '--discard')
    assert closed['discarded'] and closed['state'] == 'closed', closed
    assert not copy.exists() and not git('branch', '--list', 'wsx/t1')
    assert not git('for-each-ref', 'refs/wsx/task/1/')
    assert not leftovers('task host ' + project_id) and not leftovers(str(ROOT / 'scripts/fake-claude-stream.py'))
    sessions = daemon_call('snapshot')['data']['sessions']
    assert not [s for s in sessions if s['label'].startswith('task-')], sessions
    assert 'worktree' not in git('worktree', 'list', '--porcelain').replace('worktree ' + str(project), '')
    assert js('task', 'list')['tasks'] == [] and len(js('task', 'list', '--all')['tasks']) == 2
    assert log(1)[-1]['kind'] == 'closed'

    # 10. A failed start leaves nothing and does not reuse the number.
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
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  brief: false\n  minFreeGb: 0\n  agent:\n    tools: [Read]\n  checks: ["sleep 37"]\n  checkTimeoutSeconds: 1\n')
    timed = js('task', 'new', 'NOOP')
    failed = wait_state(timed['id'], 'failed', 'ready')
    check = failed['result']['checks'][0]
    assert failed['state'] == 'failed' and check['timed_out'] and check['exit'] is None, failed['result']
    assert '`sleep 37`: timed out' in Path(failed['report']).read_text()
    # ^ The worker's instructions name the check too: only the check's own process counts.
    assert not [line for line in leftovers('sleep 37') if line.split(None, 1)[1].startswith(('sleep', '/bin/sh -c sleep'))]
    js('task', 'close', timed['id'])

    # M2: the landing steps use a committed config without the a.txt check.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG)
    (project / 'big.txt').write_text(''.join('line %d\n' % n for n in range(1, 11)))
    git('add', 'big.txt')
    git('commit', '-q', '-am', 'land config')

    # 14. Each revision records its paths; list and show report overlap between open tasks.
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

    # 15. Landing, local-merge. A dirty default-branch checkout refuses and names the file.
    (project / 'README').write_text('dirty\n')
    refused = land(y1, ok=False)
    assert not refused['ok'] and 'README' in refused['message'] and 'land again' in refused['message'], refused
    assert show(y1)['state'] == 'ready'
    git('checkout', '--', 'README')
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

    # 16. Auto land (the default; docs/tasks.md "Landing"): a ready task lands by itself with no
    # owner action, its Human Verify items join the To check list, and a conflict lands after
    # the worker's fix. With main not checked out, the ref moves with the old tip as guard.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG.replace('  land: { auto: false }\n', ''))
    git('commit', '-q', '-am', 'auto land')
    # ^ The task records main as its default branch at `new`; main is left during the turn.
    hv = js('task', 'new', 'SLOW WORK z.txt HV')['id']
    git('switch', '-q', '-c', 'parking')
    landed_hv = wait_state(hv, 'landed', 'ready', 'failed')
    assert landed_hv['state'] == 'landed' and main_has('z.txt'), landed_hv
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
    # Two tasks on one file: whichever lands second conflicts, the worker resolves it, and it
    # lands by itself.
    a1 = js('task', 'new', 'WORK x.txt alpha HV')['id']
    a2 = js('task', 'new', 'WORK x.txt beta HV')['id']
    for n in (a1, a2):
        assert wait_state(n, 'landed', 'failed')['state'] == 'landed', show(n)
    conflicts = [n for n in (a1, a2) if any(e['kind'] == 'conflict' for e in log(n))]
    assert len(conflicts) == 1 and git('show', 'main:x.txt') == 'resolved', conflicts
    assert len(js('task', 'check')['open']) == 3
    # No checks: nothing proves the work, so a ready task waits for a land by hand.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG.replace('  land: { auto: false }\n', '').replace(
        '  checks:\n    - "test ! -e broken.txt"\n', ''))
    git('commit', '-q', '-am', 'no checks')
    unchecked = js('task', 'new', 'WORK n.txt')['id']
    assert wait_state(unchecked, 'ready', 'landed', 'failed')['state'] == 'ready'
    time.sleep(1)
    assert show(unchecked)['state'] == 'ready' and land(unchecked)['state'] == 'landed'
    # Pass one item, then fail one: the fail starts a linked fix task without a brief.
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
    assert [(e['by'], e['actor']) for e in log(fix) if e['kind'] == 'brief_skipped'] == [('flag', 'owner')]
    prompt = next(e['text'] for e in log(fix) if e.get('mode') == 'prompt')
    task_of_c2 = failed_item['task']
    assert 'the text is cut off' in prompt and show(task_of_c2)['report'] in prompt and 'Pass if: it has text' in prompt, prompt
    wait_state(fix, 'ready', 'landed', 'failed')
    assert [i['id'] for i in js('task', 'check')['open']] == ['c3']
    for n in (hv, a1, a2, unchecked):
        wait_closed(n)
    # ^ No checks prove the fix, so it waits ready; it has no commits and closes plainly.
    js('task', 'close', fix)
    (project / 'wsx.config.yml').write_text(LAND_CONFIG)
    git('commit', '-q', '-am', 'land by hand')

    # 17. Deploy and release records. Each runs in a temporary copy of a default-branch commit.
    unset = cli('task', 'deploy', ok=False)
    assert 'tasks.deploy.command is not set' in unset.stderr and 'nothing was run' in unset.stderr
    (project / 'wsx.config.yml').write_text(LAND_CONFIG + '''  deploy:
    command: 'printf "%s %s\\n" "$WSX_DEPLOY_COMMIT" "$(pwd)" >> ../deploy.log'
  release:
    command: 'exit 3'
''')
    git('commit', '-q', '-am', 'deploy config')
    assert js('task', 'deployed')['deploy']['commit'] is None
    # Open items stop only public output: a release, and a deploy marked public.
    refused = cli('task', 'release', '--json', ok=False)
    assert '1 item(s) to check are open' in refused.stderr and 'Nothing was run' in refused.stderr, refused.stderr
    (project / 'wsx.config.yml').write_text((project / 'wsx.config.yml').read_text().replace(
        '  deploy:\n', '  deploy:\n    public: true\n'))
    assert 'to check are open' in cli('task', 'deploy', ok=False).stderr
    git('checkout', '--', 'wsx.config.yml')
    assert not (base / 'deploy.log').exists()
    deployed = js('task', 'deploy')
    assert deployed['message'].endswith('; 1 to check on the deployed site'), deployed
    (base / 'deploy.log').unlink()
    assert js('task', 'check', 'pass', '--all')['open'] == 0
    deployed = js('task', 'deploy')
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
    failed_release = cli('task', 'release', '--json', ok=False)
    assert json.loads(failed_release.stdout)['exit'] == 3 and 'recorded' in failed_release.stderr
    assert js('task', 'deployed')['release'] == {'commit': None, 'failed_since': 1, 'configured': True}
    older = git('rev-parse', 'main~3')
    assert js('task', 'deploy', '--commit', older)['commit'] == older
    assert js('task', 'deployed')['deploy']['behind'] == 3

    # 18. Landing, pr policy: a local bare origin and a fake gh. Never the network.
    remote = work / 'remote.git'
    git('init', '-q', '--bare', str(remote))
    git('remote', 'add', 'origin', str(remote))
    gh_state, gh_log = work / 'gh-state.json', work / 'gh.log'
    (bin_dir / 'gh').write_text('#!/bin/sh\nexec ' + sys.executable + ' ' + str(work / 'gh.py') + ' "$@"\n')
    (bin_dir / 'gh').chmod(0o755)
    (work / 'gh.py').write_text(FAKE_GH.replace('STATE', str(gh_state)).replace('LOG', str(gh_log)))
    (project / 'wsx.config.yml').write_text((project / 'wsx.config.yml').read_text().replace(
        '  land: { auto: false }\n', '  land: { auto: false, policy: pr }\n'))
    git('commit', '-q', '-am', 'pr policy')
    git('push', '-q', 'origin', 'main')

    def gh_calls():
        return [json.loads(line) for line in gh_log.read_text().splitlines()] if gh_log.exists() else []

    p1 = js('task', 'new', 'WORK p.txt')['id']
    assert wait_state(p1, 'ready', 'failed')['state'] == 'ready'
    opened = land(p1)
    assert opened['ok'] and opened['state'] == 'in_review', opened
    url = show(p1)['record']['pr_url']
    create = gh_calls()[0]
    assert create['args'][:2] == ['pr', 'create'] and url == 'https://example.invalid/pr/1'
    assert create['args'][create['args'].index('--head') + 1] == 'wsx/t%d' % p1
    assert create['args'][create['args'].index('--title') + 1] == 'WORK p.txt' and '## Summary' in create['body']
    assert git('rev-parse', 'refs/heads/wsx/t%d' % p1, cwd=remote) == git('rev-parse', 'wsx/t%d' % p1)
    assert 'still open' in land(p1)['message']
    # New work on an open PR: a later land pushes the new revision to it.
    js('task', 'add', p1, 'WORK p2.txt')
    assert wait_state(p1, 'ready', 'failed')['revision'] == 2
    assert 'updated' in land(p1)['message']
    assert git('rev-parse', 'refs/heads/wsx/t%d' % p1, cwd=remote) == git('rev-parse', 'wsx/t%d' % p1)
    assert len([c for c in gh_calls() if c['args'][:2] == ['pr', 'create']]) == 1
    # The PR merges on the remote: land records it; the local default branch stays.
    merger = work / 'merger'
    git('clone', '-q', str(remote), str(merger), cwd=work)
    git('merge', '-q', '--no-ff', '-m', 'merge pr 1', 'origin/wsx/t%d' % p1, cwd=merger)
    git('push', '-q', 'origin', 'main', cwd=merger)
    merged = git('rev-parse', 'HEAD', cwd=merger)
    gh_state.write_text(json.dumps({url: {'state': 'MERGED', 'mergeCommit': {'oid': merged}}}))
    assert land(p1)['state'] == 'landed'
    assert [e['commit'] for e in log(p1) if e['kind'] == 'landed'] == [merged]
    assert git('rev-parse', 'main') != merged
    wait_closed(p1)
    # Deploys follow the fetched remote default branch under pr.
    assert js('task', 'deploy')['commit'] == merged
    assert js('task', 'deployed')['deploy']['behind'] == 0
    # A PR closed without a merge is recorded and the task stays in review.
    p2 = js('task', 'new', 'WORK q2.txt')['id']
    assert wait_state(p2, 'ready', 'failed')['state'] == 'ready'
    url2 = land(p2) and show(p2)['record']['pr_url']
    gh_state.write_text(json.dumps({url2: {'state': 'CLOSED', 'mergeCommit': None}}))
    closed_pr = land(p2)
    assert closed_pr['state'] == 'in_review' and 'closed without a merge' in closed_pr['message']
    assert any(e['kind'] == 'pr_closed' for e in log(p2))
    assert 'not on main' in cli('task', 'close', p2, ok=False).stderr
    js('task', 'close', p2, '--discard')

    # 19. The ledger: open tasks by attention order, recent ones, deploys, the main checkout.
    # ^ The worker doubts a check it could not run; the ledger says what the host's checks did.
    ready_task = js('task', 'new', 'WORK l.txt could not run the browser test')['id']
    blocked_task = js('task', 'new', 'NEEDS WebSearch WORK m.txt')['id']
    wait_state(ready_task, 'ready', 'failed')
    wait_state(blocked_task, 'blocked')
    assert 'only a ready task lands' in land(blocked_task, ok=False)['message']
    raw = cli('task', 'ledger').stdout
    ledger = json.loads(raw)
    assert len(raw.encode()) <= 16 * 1024 and ledger['open_omitted'] == 0
    assert ledger['to_check'] == 0 and ledger['auto_land'] is False and 'verification' not in ledger['open'][0]
    assert [(row['id'], row['state']) for row in ledger['open']] == [(blocked_task, 'blocked'), (ready_task, 'ready')]
    assert ledger['open'][1]['summary'] == 'did: WORK l.txt could not run the browser test' and ledger['open'][1]['human_verify'] == 0
    assert ledger['open'][1]['checks'] == [{'command': 'test ! -e broken.txt', 'outcome': 'passed'}], ledger['open'][1]
    assert 0 < len(ledger['recent']) <= 10 and all(row['state'] in ('landed', 'closed') for row in ledger['recent'])
    assert ledger['deploys']['deploy']['commit'] == merged and ledger['main']['branch'] == 'main'
    assert ledger['main']['changed_files'] >= 1  # scratch.txt is untracked in the main checkout
    for n in (ready_task, blocked_task):
        js('task', 'close', n, '--discard')

    # 20. The manager: started by the first say, answers from the ledger, acts through wsx task.
    (project / 'wsx.config.yml').write_text((project / 'wsx.config.yml').read_text()
                                            + '  manager: { name: "  Atlas ", model: fake-manager, eventModel: fake-events, '
                                              'rotateTokens: 100000, tools: [mcp__docs__search] }\n')
    git('commit', '-q', '-am', 'manager config')
    # ^ The owner's skills folder: the manager gets it as the `owner` plugin.
    owner_skills = home / '.claude/skills'
    (owner_skills / 'scribe').mkdir(parents=True)
    (owner_skills / 'scribe/SKILL.md').write_text('---\nname: scribe\ndescription: fixture\n'
                                                  'allowed-tools: Write, Edit\n---\nWrite a file.\n')

    def chat(after=None):
        return js('task', 'chat', *(['--after', after] if after is not None else []))['entries']

    def reply_to(message_id):
        return wait(lambda: next((e for e in chat() if e['kind'] == 'reply' and message_id in e['to']), None),
                    'reply to %s' % message_id)

    running = js('task', 'new', 'SLOW SLOW SLOW WORK r.txt')['id']
    asked = js('task', 'say', 'STATUS')
    assert asked['ok'] and asked['id'] == 1, asked
    assert ('#%d working' % running) in reply_to(asked['id'])['text']
    launch = next(t for t in traces() if t.get('launch') and t.get('manager'))
    # ^ The message ends with the host's ledger summary; a fresh conversation gets every open task.
    opening = next(t['message'] for t in traces() if t.get('manager') and 'message' in t)
    summary = opening.split('\n\nLedger summary from wsx (`wsx task ledger -p demo` has the details):\n', 1)[1]
    assert summary.startswith('- open: ') and 'working; to check: ' in summary, summary
    assert '#%d working r' % running in summary.split('- open tasks:\n', 1)[1], summary
    assert '`wsx task ledger -p demo` when you need more' in launch['instructions'], launch['instructions']
    page = os.path.realpath(state / 'wsx/tasks' / Path(created['task_dir']).parent.name / 'page.md')
    # ^ It reads as a worker does, uses the owner's skills and subagents (docs/tasks.md "Manager"),
    # plus `tasks.manager.tools`.
    assert launch['allowed'] == ['Read', 'Glob', 'Grep', 'WebFetch', 'WebSearch', 'Skill', 'Agent',
                                 'mcp__docs__search'], launch['allowed']
    plugin = Path(launch['plugin_dir'])
    assert plugin.parent.name == 'manager' and plugin.resolve().is_relative_to(state.resolve()), plugin
    assert json.loads((plugin / '.claude-plugin/plugin.json').read_text())['name'] == 'owner'
    assert os.readlink(plugin / 'skills') == str(owner_skills), os.readlink(plugin / 'skills')
    # ^ File tools are denied by name in the settings, which bind subagents and a skill's
    # `allowed-tools` too; the manager's writes are its Bash alone (docs/tasks.md "Permissions").
    deny = launch['settings']['permissions']['deny']
    assert {'Edit', 'Write', 'NotebookEdit'} <= set(deny) and launch['disallowed'] == [], deny
    assert not any(rule.startswith(('Edit(', 'Write(')) for rule in launch['allowed']), launch['allowed']
    assert launch['model'] == 'fake-manager' and Path(page).read_text().startswith('# demo\n\n## Goal')
    # ^ Its Bash is plain sandboxed Bash that writes only its notes folder and reaches one socket,
    # the manager host's broker, which runs its `wsx task` commands.
    sandbox = launch['settings']['sandbox']
    assert launch['mode'] == 'auto' and 'excludedCommands' not in sandbox, sandbox
    broker = sandbox['network']['allowUnixSockets']
    assert len(broker) == 1 and Path(broker[0]).is_socket() and len(broker[0]) < 104, broker
    # ^ The sandbox matches the real path: the rule names the socket through the real folder.
    assert broker[0] == os.path.realpath(broker[0]) and '/t-real/' in broker[0], broker
    # ^ Its Bash writes the notes folder and one file, the project page (docs/tasks.md "Project page").
    assert len(sandbox['filesystem']['allowWrite']) == 2 and sandbox['filesystem']['allowWrite'][0].endswith('/manager/notes')
    assert sandbox['filesystem']['allowWrite'][1] == page, sandbox['filesystem']
    assert Path(sandbox['filesystem']['allowWrite'][0]).is_dir() and sandbox['filesystem']['denyWrite'] == [str(project)]
    assert 'Edit(/%s/**)' % project in launch['settings']['permissions']['deny'], launch['settings']
    assert launch['actor'] == 'manager' and launch['wsx_env'] == ['WSX_TASK_ACTOR', 'WSX_TASK_BROKER'], launch['wsx_env']
    assert launch['git_optional_locks'] == '0', launch
    # Its rules for briefs: start work with one, skip it only on the owner's word, never start a build alone.
    for rule in ('`--model sonnet` may do', 'critical bug fix or heavy, complex work',
                 'fold those memos into', '/manager/notes/context.md',
                 'starts its build by itself', "`--no-brief` only", 'put them to the owner in plain words',
                 "On 'start #<n>' run `wsx task start <n>`", 'waits for your review', 'when it does what',
                 'never give the owner commands to run', '/manager/notes/decisions.md', 'starts with that file and',
                 '--from-manager --reply', 'use it and do not answer it',
                 'also in a pipe or with a heredoc', 'give each check as passed or failed from them',
                 "trust them over the worker's own words", 'grant it with `wsx task allow <n> --host <host>`',
                 'Never tell the owner to edit settings', 'read-only git in Bash',
                 '`auto_land` is true', 'Never ask the owner to verify a',
                 'never pass or fail items yourself', 'A release refuses while items are open',
                 'the Skill tool as `owner:<name>`', 'look at the skills and subagents you have',
                 'fits lookup, context, memory or research', 'check rather than guess',
                 'use Read, Grep, WebSearch and the Explore subagent', 'the likely next step',
                 'say what you checked in one short line',
                 'docs tools the project settings allow', 'Write and NotebookEdit are denied to them',
                 'Keep answers short', 'update that section in place with Bash',
                 'reply in a few friendly lines', "Never repeat the brief's additions",
                 'only when you need it'):
        assert rule in launch['instructions'], rule
    # ^ No skill is named: the owner's skills are whatever their folder holds.
    assert re.findall(r'owner:[^<\s]', launch['instructions']) == [], launch['instructions']
    assert not any(name in launch['instructions'] for name in ('seek', 'recall', '/memo', 'scribe'))
    assert launch['cwd'] == str(project)
    # ^ The name the owner reads, trimmed; the session keeps its label `task-manager`.
    assert js('task', 'manager')['name'] == 'Atlas'
    # ^ The TUI's manager row reads the name; without one it keeps the provider (docs/tasks.md "Config").
    named = tui_text(wsx, env, project, r'task-manager\s*\n*\s*\(Atlas\)')
    assert re.search(r'task-manager\s*\n*\s*\(Atlas\)', named) and '(manager)' not in named, named[-3000:]
    config_text = (project / 'wsx.config.yml').read_text()
    (project / 'wsx.config.yml').write_text(config_text.replace('name: "  Atlas ", ', ''))
    plain = tui_text(wsx, env, project, r'task-manager\s*\n*\s*\(manager\)')
    (project / 'wsx.config.yml').write_text(config_text)
    assert re.search(r'task-manager\s*\n*\s*\(manager\)', plain) and '(Atlas)' not in plain, plain[-3000:]
    assert js('task', 'manager')['state'] == 'running'
    # A new task and an add to the running one, each by a wsx task command the manager runs.
    started = js('task', 'say', 'RUN wsx task new "WORK s1.txt" -p demo --json')
    assert 'ran: wsx task new' in reply_to(started['id'])['text']
    manager_task = max(row['id'] for row in js('task', 'list')['tasks'])
    assert wait_state(manager_task, 'ready', 'failed')['state'] == 'ready'
    added = js('task', 'say', 'RUN wsx task add %d "WORK s2.txt" -p demo --json' % running)
    assert '-> 0' in reply_to(added['id'])['text']
    # Any command shape reaches the broker: `;`, a heredoc with a multi-line prompt, a pipe.
    script = js('task', 'say', "SCRIPT\nwsx task ledger -p demo --json >/dev/null; wsx task new -p demo --no-brief --json - <<'EOF'\n"
                "WORK h1.txt\nsecond line of the prompt\nEOF\nwsx task list -p demo --json | grep -c '\"id\"' >/dev/null && echo piped")
    answer = reply_to(script['id'])['text']
    assert answer.startswith('script -> 0') and answer.rstrip().endswith('piped'), answer
    heredoc_task = max(row['id'] for row in js('task', 'list')['tasks'])
    first = next(e for e in log(heredoc_task) if e['kind'] == 'message')
    assert first['text'] == 'WORK h1.txt\nsecond line of the prompt', first
    assert [(e['by'], e['actor']) for e in log(heredoc_task) if e['kind'] == 'brief_skipped'] == [('flag', 'manager')]
    assert wait_state(heredoc_task, 'ready', 'failed')['state'] == 'ready'
    # Under the broker no other command runs, neither in the CLI nor at the host.
    refused = js('task', 'say', 'RUN wsx --version\nRUN wsx task host %s 1' % launch['cwd'])
    text = reply_to(refused['id'])['text']
    assert 'ran: wsx --version -> 2 wsx: only `wsx task` commands run here' in text, text
    assert 'ran: wsx task host' in text and "-> 2 wsx: `wsx task host` is not for the manager" in text, text
    with socket.socket(socket.AF_UNIX) as raw:
        raw.connect(broker[0])
        raw.sendall(json.dumps({'argv': ['daemon', 'stop'], 'stdin': None, 'cwd': str(project)}).encode())
        raw.shutdown(socket.SHUT_WR)
        direct = json.loads(raw.makefile().read())
    assert direct['code'] == 2 and 'only `wsx task` commands' in direct['stderr'], direct
    assert wait_state(running, 'ready', 'failed')['state'] == 'ready'
    assert [e['mode'] for e in log(running) if e['kind'] == 'message'] == ['prompt', 'add']
    mlog = Path(created['task_dir']).parent / 'manager' / 'log.jsonl'

    def mlog_entries():
        return [json.loads(line) for line in mlog.read_text().splitlines()]

    # Usage (docs/tasks.md "Manager"): rotation looks at the last main model call's context, not
    # the result's sum over the turn's calls, nor a subagent's; a reply's cost is its own, not the
    # process's running `total_cost_usd`.
    summed = js('task', 'say', 'SUMMED')
    reply_to(summed['id'])
    replies = [e for e in mlog_entries() if e['kind'] == 'reply']
    usage = next(e for e in replies if summed['id'] in e['to'])['usage']
    assert usage['input_tokens'] + usage['cache_read_tokens'] == 200000 and usage['context_tokens'] == 50000, usage
    assert usage['model'] == 'fake-manager', usage
    assert not any(e['kind'] == 'rotated' for e in mlog_entries()), 'four 50k calls do not rotate'
    assert len(replies) > 3 and {e['usage']['cost_micro_usd'] for e in replies} == {1000}, replies
    # Event-only turns run on tasks.manager.eventModel unless a ready task the manager created
    # waits for its review: the manager's own model reviews it.
    review = wait(lambda: next((e for e in mlog_entries() if e['kind'] == 'message' and e['from'] == 'events'
                                and '#%d ready' % manager_task in e['text']), None), 'review event')
    reviewed = wait(lambda: next((t for t in traces() if t.get('manager') and
                                  'Events (message %d)' % review['id'] in str(t.get('message', ''))), None), 'review turn')
    assert reviewed['model'] == 'fake-manager', reviewed
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
    assert bare['plugin_dir'] is None and 'Skill' in bare['allowed'], bare
    assert 'owner:' not in bare['instructions'] and 'check rather than guess' in bare['instructions']
    assert [e['id'] for e in chat(after=big['id']) if e['kind'] == 'message' and e['from'] == 'owner'] == [after_rotation['id']]
    for n in (running, manager_task, heredoc_task):
        js('task', 'close', n, '--discard')

    # 21. Events: task state changes reach the manager, merged while it is busy; an owner
    # message waiting at the same time goes first; a host restart loses no event.
    manager_log = Path(created['task_dir']).parent / 'manager' / 'log.jsonl'

    def manager_entries():
        return [json.loads(line) for line in manager_log.read_text().splitlines()]

    def events_with(*needles):
        return [e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'events'
                and all(needle in e['text'] for needle in needles)]

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
    delivered = [e['ids'] for e in manager_entries() if e['kind'] == 'delivered']
    assert delivered.index([waiting['id']]) < delivered.index([merged[0]['id']]), delivered
    assert 'noted events' in reply_to(merged[0]['id'])['text']
    # ^ Nothing waits for the manager's review: the events turn runs on eventModel in the same
    # process, and the next owner turn switches back (docs/tasks.md "Manager").
    event_turn = next(t for t in traces() if t.get('manager') and 'Events (message %d)' % merged[0]['id'] in str(t.get('message', '')))
    assert event_turn['model'] == 'fake-events', event_turn
    # ^ The owner turn before it already showed both tasks as changed; the events turn has no change.
    status_turn = next(t['message'] for t in traces() if t.get('manager') and 'Owner (message %d)' % waiting['id'] in str(t.get('message', '')))
    assert '- changed since your last turn:\n' in status_turn and '#%d ready r1' % first in status_turn, status_turn
    assert '- open: 2 ready;' in event_turn['message'] and 'changed since' not in event_turn['message'], event_turn
    event_reply = next(e for e in manager_entries() if e['kind'] == 'reply' and merged[0]['id'] in e['to'])
    assert event_reply['usage']['model'] == 'fake-events' and event_reply['usage']['cost_micro_usd'] == 1000, event_reply
    after_events = js('task', 'say', 'STATUS after events')
    reply_to(after_events['id'])
    owner_turn = next(t for t in traces() if t.get('manager') and 'STATUS after events' in str(t.get('message', '')))
    assert owner_turn['model'] == 'fake-manager' and owner_turn['session'] == event_turn['session'], owner_turn
    switches = [t['set_model'] for t in traces() if 'set_model' in t and t['session'] == event_turn['session']]
    assert switches[-2:] == ['fake-events', 'fake-manager'], switches
    # Stop the manager host, finish a task while it is down, start it again.
    manager_socket = js('task', 'manager')['socket']
    assert len(manager_socket) < 104, manager_socket
    cli('session', 'delete', 'task-manager', '-p', 'demo')
    wait(lambda: not Path(manager_socket).exists() or
         subprocess.run(['nc', '-zU', manager_socket]).returncode != 0, 'manager down')
    unseen = js('task', 'new', 'WORK late.txt')['id']
    wait_state(unseen, 'ready', 'failed')
    assert js('task', 'manager')['state'] == 'started'
    late = wait(lambda: events_with('#%d ready r1' % unseen), 'event after restart')
    assert 'noted events' in reply_to(late[0]['id'])['text']
    # ^ The resumed process restored the session's spend: the reply still costs only its own turn.
    resumed_usage = next(e for e in manager_entries() if e['kind'] == 'reply' and late[0]['id'] in e['to'])['usage']
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
    (project / 'wsx.config.yml').write_text((project / 'wsx.config.yml').read_text().replace('model: fake-manager, ', ''))
    git('commit', '-q', '-am', 'default manager model')
    state_file = manager_log.parent / 'state.json'
    older = json.loads(state_file.read_text())
    assert older['agent_session'] and older.pop('instructions'), older
    state_file.write_text(json.dumps(older))
    changed = js('task', 'say', 'STATUS after the change')
    reply_to(changed['id'])
    fresh = [t for t in traces() if t.get('launch') and t.get('manager')][-1]
    assert not fresh['resume'] and fresh['session'] != launches[-1]['session'], fresh
    rotated = [e for e in manager_entries() if e['kind'] == 'rotated']
    assert rotated[-1].get('reason') == "the manager's instructions changed", rotated
    opened = [t['message'] for t in traces() if t.get('manager') and 'message' in t and 'STATUS after the change' in t['message']]
    assert opened and opened[0].startswith('Handover (this is a new conversation') and 'You: noted events' in opened[0], opened
    assert "fold the ledger's memos into your context first" in opened[0], opened
    # ^ The config named no manager model since the restart: the manager defaults to opus.
    assert fresh['model'] == 'opus', fresh
    resumed = js('task', 'say', 'STATUS again')
    reply_to(resumed['id'])
    assert not any(t.get('launch') and t.get('manager') and t['session'] != fresh['session']
                   for t in traces()[traces().index(fresh) + 1:]), 'same launch keeps its conversation'
    opened = [t['message'] for t in traces() if t.get('manager') and 'message' in t and 'STATUS again' in t['message']]
    assert opened and not opened[0].startswith('Handover'), opened
    for n in (first, second, unseen):
        js('task', 'close', n, '--discard')

    # 22. Approvals: what the manager may not do alone becomes a request the owner decides.
    (project / 'wsx.config.yml').write_text((project / 'wsx.config.yml').read_text().replace(
        '  land: { auto: false, policy: pr }\n', '  land: { auto: false }\n'))
    git('commit', '-q', '-am', 'back to local-merge')
    # ^ By default the manager lands alone: only what goes public or is lost asks.
    free = js('task', 'new', 'WORK free.txt')['id']
    assert wait_state(free, 'ready', 'failed')['state'] == 'ready'
    landed_alone = js('task', 'say', 'RUN wsx task land %d -p demo --json' % free)
    assert 'owner was asked' not in reply_to(landed_alone['id'])['text'] and main_has('free.txt')
    wait_closed(free)
    (project / 'wsx.config.yml').write_text((project / 'wsx.config.yml').read_text().replace(
        '  land: { auto: false }\n', '  land: { auto: false, approve: true }\n'))
    git('commit', '-q', '-am', 'land asks')
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
    # An approval for a revision the task has moved past is not run.
    js('task', 'add', keep, 'WORK keep2.txt')
    assert wait_state(keep, 'ready', 'failed')['revision'] == 2
    moved = cli('task', 'approve', 'a3', '--json', ok=False)
    assert 'moved from r1 to r2' in moved.stderr and show(keep)['state'] == 'ready'
    assert 'no pending approval a3' in cli('task', 'approve', 'a3', ok=False).stderr
    assert js('task', 'ledger')['approvals'] == []
    wait_closed(ap)
    js('task', 'close', keep, '--discard')

    # 23. Brief first (docs/tasks.md "Brief"): a read-only turn returns a brief and the task
    # waits; a reply is a talk-through the worker answers; start builds the rewritten prompt
    # in the same conversation with the build's tools; --no-brief builds at once.
    config = (project / 'wsx.config.yml').read_text().replace('  brief: false\n', '')
    (project / 'wsx.config.yml').write_text(config)
    git('commit', '-q', '-am', 'brief on')

    def brief_version(n, version):
        return wait(lambda: (current := show(n))['state'] in ('briefed', 'failed')
                    and (current['brief'] or {}).get('version') == version and current, 't%s brief v%d' % (n, version))

    b = js('task', 'new', 'Fix the fixture EDIT README QUESTION REWRITE WORK b1.txt')['id']
    briefed = brief_version(b, 1)
    assert briefed['state'] == 'briefed' and briefed['record']['phase'] == 'brief', briefed
    brief_launch = [t for t in traces() if t.get('launch')][-1]
    assert brief_launch['briefing'] and not brief_launch['resume'], brief_launch
    assert brief_launch['disallowed'] == ['Edit', 'Write', 'NotebookEdit'], brief_launch
    assert brief_launch['allowed'] == ['Read', 'Glob', 'Grep', 'WebFetch'] and brief_launch['mode'] == 'auto'
    # ^ The OS keeps a brief read-only: Bash writes nothing, not even the working copy.
    files = brief_launch['settings']['sandbox']['filesystem']
    assert files['allowWrite'] == [] and files['denyWrite'][0] == brief_launch['cwd'], files
    # The edit the worker tried in the brief turn was denied and changed nothing.
    copy_b = Path(briefed['record']['working_copy'])
    assert (copy_b / 'README').read_text() == 'fixture\n'
    assert any(t.get('edit_denied') == 'README' for t in traces())
    entry = [e for e in log(b) if e['kind'] == 'brief'][-1]
    assert entry['version'] == 1 and entry['denials'] == ['Edit(README)'] and 'answer' not in entry, entry
    assert entry['usage']['cost_micro_usd'] == 20000, entry
    assert not any(e['kind'] in ('result', 'checks', 'revision') for e in log(b))
    assert briefed['brief']['brief']['rewritten_prompt'] == 'WORK b1.txt'
    brief_md = Path(briefed['brief_text']).read_text()
    assert briefed['brief']['brief']['diagram'].startswith('```mermaid\ngraph LR'), briefed['brief']
    assert brief_md.split('\n## ')[1].startswith('Diagram\n\n```mermaid\ngraph LR'), brief_md
    for section in ('Diagram', 'What a top professional would add', 'Raw material', 'Methods and tools', 'Independent check',
                    'Concepts to know', 'Rewritten prompt', 'Open questions'):
        assert '## ' + section in brief_md, section
    assert brief_md.split('## Rewritten prompt')[1].split('## Open questions')[0].strip() == 'WORK b1.txt', brief_md
    row = next(r for r in js('task', 'ledger')['open'] if r['id'] == b)
    assert row['state'] == 'briefed' and row['brief'] == {'version': 1, 'rewritten_prompt': 'WORK b1.txt'}, row
    # A reply is a talk-through: the worker answers in the log and revises the brief, still read-only.
    js('task', 'add', b, 'Why b1? QUESTION REVISE WORK b2.txt')
    revised = brief_version(b, 2)
    assert revised['state'] == 'briefed' and revised['brief']['brief']['rewritten_prompt'] == 'WORK b2.txt'
    answer = [e for e in log(b) if e['kind'] == 'brief'][-1]
    assert answer['answer'] == 'answered: Why b1? QUESTION REVISE WORK b2.txt', answer
    assert [t for t in traces() if t.get('launch')][-1]['pid'] == brief_launch['pid'], 'the talk-through restarted the agent'
    # Start: the same conversation with the build's tools and result schema, against the revised prompt.
    assert js('task', 'start', b)['message'] == 'started the build of brief v2'
    ready = wait_state(b, 'ready', 'failed')
    assert ready['state'] == 'ready' and ready['record']['phase'] == 'build', ready
    build_launch = [t for t in traces() if t.get('launch')][-1]
    assert build_launch['resume'] and build_launch['session'] == brief_launch['session'], build_launch
    assert not build_launch['briefing'] and build_launch['allowed'] == ['Read', 'Glob', 'Grep', 'WebFetch']
    assert build_launch['disallowed'] == [] and build_launch['settings']['sandbox']['filesystem']['allowWrite'], build_launch
    # ^ Claude reuses a conversation's first system prompt on resume: both launches pass the same text.
    assert build_launch['instructions'] == brief_launch['instructions']
    assert (copy_b / 'b2.txt').exists() and not (copy_b / 'b1.txt').exists()
    assert [(e['version'], e['prompt'], e.get('auto')) for e in log(b) if e['kind'] == 'started'] == [(2, 'WORK b2.txt', None)]
    assert [e['text'] for e in log(b) if e['kind'] == 'message' and e['mode'] == 'prompt'] == \
        ['Fix the fixture EDIT README QUESTION REWRITE WORK b1.txt']
    report = Path(ready['report']).read_text()
    # ^ The report starts at the summary: the brief and the prompts are in the log and brief.md.
    assert [line for line in report.splitlines() if line.startswith('## ')][0] == '## Summary', report
    assert '## Asked' not in report and 'brief v2' in report, report
    assert 'only a briefed task starts' in cli('task', 'start', b, ok=False).stdout
    js('task', 'close', b, '--discard')
    # A brief without open questions builds by itself; an answer that leaves none starts it too.
    auto = js('task', 'new', 'REWRITE WORK auto.txt')['id']
    assert wait_state(auto, 'ready', 'failed')['state'] == 'ready' and show(auto)['record']['phase'] == 'build'
    assert [(e['version'], e['auto']) for e in log(auto) if e['kind'] == 'started'] == [(1, True)]
    asked_back = js('task', 'new', 'QUESTION REWRITE WORK q1.txt')['id']
    assert brief_version(asked_back, 1)['state'] == 'briefed'
    js('task', 'add', asked_back, 'Use q2. REVISE WORK q2.txt')
    assert wait_state(asked_back, 'ready', 'failed')['state'] == 'ready'
    assert (Path(show(asked_back)['record']['working_copy']) / 'q2.txt').exists()
    assert [(e['version'], e['auto']) for e in log(asked_back) if e['kind'] == 'started'] == [(2, True)]
    for n in (auto, asked_back):
        js('task', 'close', n, '--discard')
    # A brief turn without a brief fails; the next reply asks again.
    f = js('task', 'new', 'NOBRIEF')['id']
    assert wait_state(f, 'failed', 'briefed')['state'] == 'failed'
    js('task', 'add', f, 'QUESTION REVISE NOOP')
    assert brief_version(f, 1)['state'] == 'briefed'
    js('task', 'close', f, '--discard')
    # --no-brief builds at once, and the log says who skipped the brief.
    nb = js('task', 'new', 'WORK nb.txt', '--no-brief')['id']
    assert wait_state(nb, 'ready', 'failed')['state'] == 'ready' and show(nb)['brief'] is None
    assert [(e['by'], e['actor']) for e in log(nb) if e['kind'] == 'brief_skipped'] == [('flag', 'owner')]
    js('task', 'close', nb, '--discard')
    # The manager hears of a brief with questions and may start it without asking.
    m = js('task', 'new', 'QUESTION REWRITE WORK m1.txt')['id']
    brief_version(m, 1)
    # ^ The event names the brief's open questions, not its prompt: the sheet holds the brief.
    briefed_event = wait(lambda: events_with('#%d briefed r0' % m, 'brief v1: 1 open question: Which file should change?'), 'briefed event')
    assert 'brief v1: WORK m1.txt' not in briefed_event[0]['text'], briefed_event
    asked = js('task', 'say', 'RUN wsx task start %d -p demo --json' % m)
    text = reply_to(asked['id'])['text']
    assert 'started the build of brief v1' in text and 'owner was asked' not in text, text
    assert js('task', 'ledger')['approvals'] == []
    assert wait_state(m, 'ready', 'failed')['state'] == 'ready'
    # The manager may skip the brief when the owner asks; the log names it.
    js('task', 'say', 'RUN wsx task new "WORK mn.txt" --no-brief -p demo --json')
    # ^ A new number is listed with an error row until its record is written.
    mn = wait(lambda: max((row['id'] for row in js('task', 'list')['tasks'] if 'state' in row), default=0) > m and
              max(row['id'] for row in js('task', 'list')['tasks'] if 'state' in row), 'manager --no-brief task')
    assert wait_state(mn, 'ready', 'failed')['state'] == 'ready'
    assert [(e['by'], e['actor']) for e in log(mn) if e['kind'] == 'brief_skipped'] == [('flag', 'manager')]
    for n in (m, mn):
        js('task', 'close', n, '--discard')

    # 24. Asks and grants (docs/tasks.md "Permissions"): a block names what the sandbox
    # stopped, a grant resumes the task with it, a project grant reaches the next task.
    def launches_in(n):
        copy = os.path.realpath(show(n)['record']['working_copy'])
        return [t for t in traces() if t.get('launch') and os.path.realpath(t['cwd']) == copy]

    def sandbox_of(launch):
        return launch['settings']['sandbox']

    crate = home / '.cargo/registry/cache/x.crate'
    g = js('task', 'new', 'NEEDS_HOST example.org NEEDS_PATH %s RETRY_HOST reached.example' % crate, '--no-brief')['id']
    blocked = wait_state(g, 'blocked')
    asks = [{'kind': 'network', 'host': 'example.org'}, {'kind': 'write', 'path': '~/.cargo'}]
    assert blocked['result']['asks'] == asks, blocked['result']
    assert [e['asks'] for e in log(g) if e['kind'] == 'blocked'] == [asks], log(g)
    assert next(r for r in js('task', 'ledger')['open'] if r['id'] == g)['asks'] == asks
    bad = cli('task', 'allow', g, '--host', 'bad host', '--json', ok=False)
    assert 'is not a host name' in bad.stdout + bad.stderr, bad
    # ^ The block resumes once every ask is granted, so one resume carries both.
    granted = js('task', 'allow', g, '--host', 'example.org')
    assert granted['message'] == 'may now reach example.org; still asks to write ~/.cargo', granted
    assert show(g)['state'] == 'blocked' and len(launches_in(g)) == 1
    # A project-wide grant goes to grants.json (owner-only) and every next agent start.
    granted = js('task', 'allow', g, '--path', '~/.cargo', '--project-wide')
    assert granted['message'] == 'may now write ~/.cargo; resumed the task', granted
    wait(lambda: len(launches_in(g)) == 2, 'relaunch with the grants')
    relaunch = launches_in(g)[-1]
    assert relaunch['resume'] and 'example.org' in sandbox_of(relaunch)['network']['allowedDomains'], relaunch
    assert '~/.cargo' in sandbox_of(relaunch)['filesystem']['allowWrite'], relaunch
    wait_state(g, 'ready', 'failed')
    assert show(g)['record']['grants'] == [{'kind': 'network', 'host': 'example.org'}]
    grants_file = Path(show(g)['log']).parent.parent / 'grants.json'
    assert json.loads(grants_file.read_text()) == [{'kind': 'write', 'path': '~/.cargo'}]
    assert oct(grants_file.stat().st_mode & 0o777) == '0o600'
    h = js('task', 'new', 'NEEDS_PATH %s WORK h.txt' % crate, '--no-brief')['id']
    assert wait_state(h, 'ready', 'failed')['state'] == 'ready'
    assert '~/.cargo' in sandbox_of(launches_in(h)[0])['filesystem']['allowWrite']
    # Revoke: the next turn starts without it.
    assert js('task', 'revoke', g, '--host', 'example.org')['ok']
    js('task', 'add', g, 'NOOP')
    wait(lambda: len(launches_in(g)) == 3, 'relaunch without the host')
    assert 'example.org' not in sandbox_of(launches_in(g)[-1])['network']['allowedDomains']
    assert [e['kind'] for e in log(g) if e['kind'] in ('granted', 'revoked')] == ['granted', 'granted', 'revoked']
    # The manager grants a host for one task alone; a project-wide grant asks the owner.
    wait_state(g, 'ready', 'failed')
    asked = js('task', 'say', 'RUN wsx task allow %d --host crates.io -p demo --json\n'
                              'RUN wsx task allow %d --host npmjs.org --project-wide -p demo --json\n'
                              'RUN wsx task allow %d WebSearch -p demo --json\n'
                              'RUN wsx task revoke %d --path "~/.cargo" --project-wide -p demo --json' % (g, g, g, g))
    text = reply_to(asked['id'])['text']
    assert 'may now reach crates.io from the next turn' in text, text
    assert any(e['kind'] == 'granted' and e['by'] == 'manager' and e['access'] == {'kind': 'network', 'host': 'crates.io'}
               for e in log(g)), log(g)
    requests = re.findall(r'the owner was asked \((a\d+)\)', text)
    assert len(requests) == 2 and "owner's to revoke" in text, text
    pending = {p['id']: p for p in js('task', 'ledger')['approvals']}
    assert pending[requests[0]]['grant'] == {'access': {'kind': 'network', 'host': 'npmjs.org'}, 'project_wide': True}
    assert pending[requests[1]]['grant'] == {'access': {'kind': 'tool', 'rule': 'WebSearch'}, 'project_wide': False}
    assert 'revision' not in pending[requests[0]], pending
    assert js('task', 'approve', requests[0])['ok']
    assert {'kind': 'network', 'host': 'npmjs.org'} in json.loads(grants_file.read_text())
    js('task', 'deny', requests[1])
    for n in (g, h):
        js('task', 'close', n, '--discard')

    # 25. A done result whose checks fail goes back to the worker with the failed command and
    # its output, at most twice, then the task fails as before.
    fixable = 'test -f fixed.txt || { echo "WORK fixed.txt"; exit 1; }'
    (project / 'wsx.config.yml').write_text(CONFIG.replace('    - "test -f a.txt"\n    - "test ! -e broken.txt"\n',
                                                           '    - %s\n' % json.dumps(fixable)))
    git('commit', '-q', '-am', 'fixable check')
    f = js('task', 'new', 'WORK retry1.txt')['id']
    fixed = wait_state(f, 'ready', 'failed')
    assert fixed['state'] == 'ready' and fixed['revision'] == 2, fixed
    retries = [e for e in log(f) if e['kind'] == 'check_retry']
    assert [(e['revision'], e['attempt'], e['commands']) for e in retries] == [(1, 1, [fixable])], retries
    retry_message = next(t['message'] for t in traces() if fixable in t.get('message', ''))
    assert '(automatic retry 1 of 2)' in retry_message and fixable in retry_message and 'WORK fixed.txt' in retry_message
    (project / 'wsx.config.yml').write_text(CONFIG.replace('    - "test -f a.txt"\n    - "test ! -e broken.txt"\n',
                                                           '    - "echo stays broken; exit 3"\n'))
    git('commit', '-q', '-am', 'broken check')
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
    # new file in the same process, which resumes the agent with the new launch.
    (project / 'wsx.config.yml').write_text(CONFIG.split('  checks:')[0])
    git('commit', '-q', '-am', 'no checks')
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

    # 27. The worker runs the checks itself through its host's broker, outside its sandbox and
    # as after its result; any other command is refused (docs/tasks.md "Checks, revisions and report").
    (project / 'wsx.config.yml').write_text(LAND_CONFIG.replace(
        '    - "test ! -e broken.txt"\n', '    - "test ! -e broken.txt"\n    - "echo second check"\n'))
    git('commit', '-q', '-am', 'two checks')
    cr = js('task', 'new', 'CHECKRUN WORK cr.txt')['id']
    assert wait_state(cr, 'ready', 'failed')['state'] == 'ready'
    ran = next(t for t in traces() if 'check_run' in t)
    code, said = ran['check_run']
    assert code == 0 and 'check 1 `test ! -e broken.txt`: exit 0' in said and 'check 2 `echo second check`: exit 0' in said, ran
    assert ran['check_run_other'][0] == 2 and 'check-run' in ran['check_run_other'][1], ran
    assert [len(e['results']) for e in log(cr) if e['kind'] == 'check_run'] == [2]
    worker_launch = launches_in(cr)[0]
    assert '1. `test ! -e broken.txt` 2. `echo second check`' in worker_launch['instructions']
    sockets = worker_launch['settings']['sandbox']['network']['allowUnixSockets']
    assert len(sockets) == 1 and Path(sockets[0]).is_socket() and len(sockets[0]) < 104, sockets
    assert 'WSX_TASK_BROKER' in worker_launch['wsx_env'], worker_launch['wsx_env']
    js('task', 'close', cr, '--discard')
    # Below tasks.minFreeGb a task waits, says why, and goes on by itself without a retry.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG.replace('minFreeGb: 0', 'minFreeGb: 100000'))
    git('commit', '-q', '-am', 'no disk')
    dw = js('task', 'new', 'WORK dw.txt')['id']
    waiting = wait(lambda: (current := show(dw))['state'] == 'blocked' and current['record'].get('waiting') and current,
                   'disk wait')
    assert waiting['record']['waiting'].startswith('Disk full: ') and any(e['kind'] == 'disk_wait' for e in log(dw))
    row = next(r for r in js('task', 'ledger')['open'] if r['id'] == dw)
    assert row['waiting'].startswith('Disk full: '), row
    (project / 'wsx.config.yml').write_text(LAND_CONFIG)
    git('commit', '-q', '-am', 'disk back')
    wait(lambda: show(dw)['state'] == 'ready', 'resumed after disk', 60)
    assert 'waiting' not in show(dw)['record'] and not any(e['kind'] == 'check_retry' for e in log(dw))
    js('task', 'close', dw, '--discard')

    # 28. A land deploys by itself with tasks.deploy.auto; deploys run one at a time and a
    # deploy that waited runs for the newest commit, so quick lands skip the ones between.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG + """  deploy:
    command: 'sleep 8; echo "$WSX_DEPLOY_COMMIT" >> ../auto-deploy.log'
    auto: true
""")
    git('commit', '-q', '-am', 'deploy after land')
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
    tailscale_log = work / 'tailscale.log'
    (bin_dir / 'tailscale').write_text('#!/bin/sh\necho "$@" >> ' + str(tailscale_log) + '\n'
                                       'if [ "$1" = status ]; then echo \'{"Self":{"DNSName":"host.example.ts.net."}}\'; fi\n')
    (bin_dir / 'tailscale').chmod(0o755)
    (project / 'wsx.config.yml').write_text(LAND_CONFIG + """  preview:
    command: 'exec python3 -m http.server "$PORT" --bind 127.0.0.1'
    ports: 9150-9199
""")
    git('commit', '-q', '-am', 'preview')
    pv = js('task', 'new', 'WORK pv.txt')['id']
    assert wait_state(pv, 'ready', 'failed')['state'] == 'ready'

    def answers(port):
        try:
            with urllib.request.urlopen('http://127.0.0.1:%d/pv.txt' % port, timeout=3) as response:
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
    (project / 'wsx.config.yml').write_text(LAND_CONFIG)
    git('commit', '-q', '-am', 'no preview')
    plain = js('task', 'new', 'WORK plain.txt')['id']
    assert wait_state(plain, 'ready', 'failed')['state'] == 'ready'
    assert 'configures no tasks.preview' in cli('task', 'preview', plain, ok=False).stderr
    js('task', 'close', plain, '--discard')

    # 30. By hand (docs/tasks.md "By hand"): the owner takes a running build over in its own
    # working copy with a fork of the worker's conversation; nothing else writes there until
    # handback, whose commits are checked, land by themselves and close the task.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG.replace('  land: { auto: false }\n', ''))
    git('commit', '-q', '-am', 'auto land for hand')
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
    assert taken['ok'] and taken['state'] == 'by_hand' and taken['session'] == 'task-%d-hand' % handed, taken
    assert 'dropped 1 queued' in taken['message'], taken
    context = taken['context']
    assert context['branch'] == 'wsx/t%d' % handed and Path(context['working_copy']) == hand_copy, context
    assert context['head'] == git('rev-parse', 'HEAD', cwd=hand_copy) and context['status'] == 'clean', context
    assert [e['dropped'] for e in log(handed) if e['kind'] == 'hand'] == [1]
    pane = wait(lambda: next((t for t in traces() if t.get('interactive') and t['resume'] == worker_session), None),
                'the hand pane resumes the worker conversation')
    assert pane['argv'] == ['--resume', worker_session, '--fork-session', '--system-prompt-snapshot', 'off'], pane
    assert Path(pane['cwd']).resolve() == hand_copy.resolve(), pane

    def hand_sessions():
        return [x for x in daemon_call('snapshot')['data']['sessions'] if x['label'] == 'task-%d-hand' % handed]

    assert len(hand_sessions()) == 1
    # ^ A second hand is refused and opens no second pane.
    assert 'is by_hand' in cli('task', 'hand', handed, '--json', ok=False).stderr and len(hand_sessions()) == 1
    assert show(handed)['hand']['session'] == 'task-%d-hand' % handed
    # One worktree: the task's own copy on its own branch.
    listed = git('worktree', 'list', '--porcelain').splitlines()
    assert listed.count('branch refs/heads/wsx/t%d' % handed) == 1, listed
    # Nothing starts while by hand: messages, start and land are refused, the old turn's gate
    # opens on no one, and no check runs.
    gate.write_text('open')
    for argv, word in ((('add', handed, 'WORK no.txt'), 'handback'), (('steer', handed, 'x'), 'handback'),
                       (('land', handed), 'handback'), (('start', handed), 'handback')):
        refused = cli('task', *argv, '--json', ok=False)
        assert word in refused.stdout + refused.stderr, refused
    time.sleep(1.5)
    assert show(handed)['state'] == 'by_hand'
    assert not [e for e in log(handed) if e['kind'] in ('checks', 'revision', 'result')], log(handed)
    assert not any(t.get('message') in ('WORK no.txt', 'WORK queued.txt') for t in traces())
    assert not (hand_copy / 'late.txt').exists()
    # Uncommitted tracked work refuses the handback, and the pane stays.
    (hand_copy / 'README').write_text('by hand\n')
    refused = cli('task', 'handback', handed, '--json', ok=False)
    assert 'uncommitted changes: README' in refused.stderr, refused.stderr
    assert show(handed)['state'] == 'by_hand' and len(hand_sessions()) == 1
    (hand_copy / 'hand.txt').write_text('made by hand\n')
    git('add', 'README', 'hand.txt', cwd=hand_copy)
    git('commit', '-q', '-m', 'by hand', cwd=hand_copy)
    back = js('task', 'handback', handed)
    assert back['ok'] and len(back['sessions_closed']) == 1 and '1 commit' in back['message'], back
    assert not hand_sessions()
    assert [e['commits'] for e in log(handed) if e['kind'] == 'handback'] == [1]
    wait_closed(handed)
    assert main_has('hand.txt') and git('show', 'main:README') == 'by hand'
    assert [e['kind'] for e in log(handed) if e['kind'] in ('checks', 'revision')] == ['checks', 'revision']
    # ^ The fork wrote its own transcript; the worker's is as it was.
    assert hashlib.sha256(worker_transcript.read_bytes()).hexdigest() == before
    assert any(path.name.startswith('fork-') for path in (home / '.claude/fake-sessions').iterdir())
    (project / 'wsx.config.yml').write_text(LAND_CONFIG)
    git('commit', '-q', '-am', 'land by hand again')

    # 31. `wsx worktree delete` refuses uncommitted work before it closes anything, ignores
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

    # 32. Managers talk (docs/tasks.md "Managers talk"): a manager reads another project's
    # ledger and messages its manager, which answers once; nothing else crosses projects.
    other = base / 'other'
    other.mkdir(mode=0o700)
    git('init', '-q', '-b', 'main', cwd=other)
    (other / 'README').write_text('other\n')
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
    replies = wait(lambda: [e for e in manager_entries() if e['kind'] == 'message' and e['from'] == 'manager-reply:other'],
                   'reply at demo', 60)
    assert len(replies) == 1 and replies[0]['text'] == 'first answer', replies
    assert any(e['kind'] == 'sent' and e['to'] == 'other' and 'first answer' in e['text'] for e in manager_entries())
    wait(lambda: any(t.get('manager') and str(t.get('message', '')).startswith('Manager other replies (message')
                     for t in traces()), 'the reply reaches the demo agent')
    crossing = js('task', 'say', 'RUN wsx task new "WORK x.txt" -p other --json\nRUN wsx task say -p demo --from-manager hi')
    text = reply_to(crossing['id'])['text']
    assert 'a manager acts only in its own project' in text and "messages another project's manager" in text, text
    assert "is for a project's manager" in cli('task', 'say', '-p', 'other', '--from-manager', 'hello', ok=False).stderr


if __name__ == '__main__':
    main()
