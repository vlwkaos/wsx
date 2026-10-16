#!/usr/bin/env python3
"""Model-free `wsx task` journey on an isolated wsxd with a stream-json fake Claude.

Covers new, steer during a turn, add after it, a blocked result then allow, checks that
pass and fail, report content, a daemon live handoff while a host runs, stop and resume,
close with and without unlanded commits, failure cleanup, and that no process, socket,
worktree or branch is left. Build adjacent wsx and wsxd first. ref: docs/tasks.md
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
CONFIG = '''tasks:
  agent:
    model: fake-model
    tools: [Read, Edit]
  checks:
    - "test -f a.txt"
    - "test ! -e broken.txt"
'''

LAND_CONFIG = '''tasks:
  agent:
    tools: [Read, Edit]
  checks:
    - "test ! -e broken.txt"
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    args = parser.parse_args()
    wsx = args.wsx.resolve()
    wsxd = wsx.with_name('wsxd')
    if not wsx.is_file() or not wsxd.is_file():
        raise RuntimeError('build adjacent wsx and wsxd first')
    # ^ Short root: host.sock must fit in sun_path.
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
    home, state, project, bin_dir = work / 'h', work / 's', work / 'project', work / 'bin'
    for path in (home, state, project, bin_dir):
        path.mkdir(mode=0o700)
    config_dir = (home / 'Library/Application Support/wsx' if sys.platform == 'darwin'
                  else work / 'c/wsx')
    config_dir.mkdir(parents=True)
    (config_dir / 'config-v2.toml').write_text(
        'resume_agents_on_restore = false\n[[projects]]\nname = "demo"\npath = '
        + json.dumps(str(project)) + '\n')
    fake = bin_dir / 'claude'
    fake.write_text('#!/bin/sh\nexec ' + sys.executable + ' ' + str(ROOT / 'scripts/fake-claude-stream.py') + ' "$@"\n')
    fake.chmod(0o755)
    daemon_socket = state / 'wsx/wsx.sock'
    env = {'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(work / 'c'),
           'XDG_CACHE_HOME': str(work / 'k'), 'WSX_DAEMON_BIN': str(wsxd), 'WSX_SOCKET': str(daemon_socket),
           'PATH': str(bin_dir) + ':/usr/bin:/bin:/usr/sbin:/sbin', 'SHELL': '/bin/sh',
           'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8'}
    owned['env'] = env

    def git(*argv, cwd=project, ok=True):
        result = subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', *argv],
                                cwd=cwd, env=env, capture_output=True, text=True, timeout=20)
        if ok:
            assert result.returncode == 0, result.stderr
        return result.stdout.strip()

    git('init', '-q', '-b', 'main')
    (project / 'wsx.config.yml').write_text(CONFIG)
    (project / '.gitignore').write_text('target/\n')
    (project / 'README').write_text('fixture\n')
    git('add', '.')
    git('commit', '-q', '-m', 'start')
    # Ignored build output is kept (warm builds); an untracked file is cleaned.
    (project / 'target').mkdir()
    (project / 'target/warm').write_text('cached')
    (project / 'scratch.txt').write_text('untracked')

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
        path = work / 'fake-claude.trace'
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
    (project / 'wsx.config.yml').write_text(CONFIG)
    assert 'invalid task number' in cli('task', 'show', '../1', ok=False).stderr
    assert 'a prompt is required' in cli('task', 'new', ok=False).stderr
    assert 'empty' in cli('task', 'new', '-', stdin='\n', ok=False).stderr

    # 1. New: a working turn in its own working copy.
    created = js('task', 'new', 'SLOW WORK a.txt')
    assert created['id'] == 1 and created['state'] == 'working', created
    copy = Path(created['working_copy'])
    assert copy == work / 'project-t1' and created['branch'] == 'wsx/t1'
    assert (copy / 'target/warm').read_text() == 'cached'
    assert not (copy / 'scratch.txt').exists()
    assert (copy / '.git').is_file() and git('rev-parse', '--abbrev-ref', 'HEAD', cwd=copy) == 'wsx/t1'
    assert git('rev-parse', '--git-dir', cwd=copy) != git('rev-parse', '--git-dir')
    task_dir = Path(created['task_dir'])
    assert oct(task_dir.stat().st_mode & 0o777) == '0o700'
    assert oct((task_dir / 'host.sock').stat().st_mode & 0o777) == '0o600'
    launch = wait(lambda: next((t for t in traces() if t.get('launch')), None), 'fake launch')
    assert launch['wsx_env'] == [] and launch['allowed'] == ['Read', 'Edit'] and launch['schema']
    assert launch['model'] == 'fake-model' and not launch['resume']

    # 2. Steer during the turn: Claude does not save it, so the host logs it.
    steered = js('task', 'steer', '1', 'use capitals')
    assert steered['ok'] and 'steered' in steered['message'], steered
    ready = wait_state(1, 'ready', 'failed')
    assert ready['state'] == 'ready', ready
    assert any(t.get('steer_in_turn') == 'use capitals' for t in traces())
    assert [e['mode'] for e in log(1) if e['kind'] == 'message'] == ['prompt', 'steer']
    assert ready['result']['checks'] and all(c['exit'] == 0 for c in ready['result']['checks'])
    commit = git('rev-parse', 'refs/wsx/task/1/r1')
    assert commit == git('rev-parse', 'wsx/t1')
    report = Path(ready['report']).read_text()
    for section in ('## Asked', '## Summary', '## Decisions', '## Changes', '## Evidence',
                    '## Human Verify', '## Left open'):
        assert section in report, section
    assert report.index('SLOW WORK a.txt') < report.index('use capitals')
    assert '`test -f a.txt`: exit 0' in report and 'add a.txt' in report
    agent = wait(lambda: (a := pane_agent(1)) and a['state'] == 'done' and a, 'agent done')
    assert agent['provider'] == 'task'

    # 3. Add after the turn, blocked on a denied tool, then allow resumes the same conversation.
    added = js('task', 'add', '1', 'NEEDS Bash(make:*) WORK b.txt')
    assert added['ok'] and 'new turn' in added['message'], added
    blocked = wait_state(1, 'blocked')
    assert any(e['kind'] == 'blocked' and e['denials'] == ['Bash(Bash(make:*))'] for e in log(1)), log(1)
    wait(lambda: (pane_agent(1) or {}).get('state') == 'blocked', 'agent blocked')
    assert blocked['revision'] == 1
    allowed = js('task', 'allow', '1', 'Bash(make:*)')
    assert allowed['ok'] and 'resumed' in allowed['message'], allowed
    ready = wait_state(1, 'ready', 'failed')
    assert ready['state'] == 'ready' and ready['revision'] == 2, ready
    relaunch = [t for t in traces() if t.get('launch')][-1]
    assert relaunch['resume'] and relaunch['session'] == launch['session']
    assert 'Bash(make:*)' in relaunch['allowed']

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
    closed = js('task', 'close', '2')
    assert closed['state'] == 'closed' and closed['working_copy_removed'], closed
    project_id = show(1)['record']['project_id']
    assert not copy2.exists() and not git('branch', '--list', 'wsx/t2')
    assert not (Path(second['task_dir']) / 'host.sock').exists()
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
    (work / 'project-t3').mkdir()
    blocked_path = cli('task', 'new', 'NOOP', ok=False)
    assert 'already exists' in blocked_path.stderr and 'not reused' in blocked_path.stderr, blocked_path.stderr
    assert not (task_dir.parent / '3').exists() and not git('branch', '--list', 'wsx/t3')
    (work / 'project-t3').rmdir()
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
    assert not (task_dir.parent / '5').exists() and not (work / 'project-t5').exists()
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
        # ^ NOOP on main lacks a.txt, so the declared check fails: both still finish r1.
        done = wait_state(n, 'ready', 'failed')
        assert done['revision'] == 1 and done['result']['checks'][0]['exit'] == 1, done
        js('task', 'close', n)

    # 13. A check that runs past tasks.checkTimeoutSeconds is killed and fails the task.
    (project / 'wsx.config.yml').write_text(
        'tasks:\n  agent:\n    tools: [Read]\n  checks: ["sleep 37"]\n  checkTimeoutSeconds: 1\n')
    timed = js('task', 'new', 'NOOP')
    failed = wait_state(timed['id'], 'failed', 'ready')
    check = failed['result']['checks'][0]
    assert failed['state'] == 'failed' and check['timed_out'] and check['exit'] is None, failed['result']
    assert '`sleep 37`: timed out' in Path(failed['report']).read_text()
    assert not leftovers('sleep 37')
    js('task', 'close', timed['id'])

    # M2: the landing steps use a committed config without the a.txt check.
    (project / 'wsx.config.yml').write_text(LAND_CONFIG)
    git('commit', '-q', '-am', 'land config')

    # 14. Each revision records its paths; list and show report overlap between open tasks.
    x1 = js('task', 'new', 'WORK x.txt alpha')['id']
    y1 = js('task', 'new', 'WORK y.txt')['id']
    x2 = js('task', 'new', 'WORK x.txt beta')['id']
    for n in (x1, y1, x2):
        assert wait_state(n, 'ready', 'failed')['state'] == 'ready'
    assert show(x1)['record']['paths'] == ['x.txt'] and show(x1)['record']['paths_total'] == 1
    rows = {row['id']: row for row in js('task', 'list')['tasks']}
    assert rows[x1]['overlaps'] == [{'id': x2, 'count': 1, 'complete': True}], rows[x1]
    assert rows[y1]['overlaps'] == [] and rows[x2]['overlaps'][0]['id'] == x1
    assert show(x2)['overlaps'] == [{'id': x1, 'count': 1, 'complete': True, 'paths': ['x.txt']}]


if __name__ == '__main__':
    main()
