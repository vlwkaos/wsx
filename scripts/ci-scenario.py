#!/usr/bin/env python3
"""Run one `task` or `web` scenario on GitHub Actions: `ci-scenario.py <name> [--part <part>]`.

The task host runs it (through `scenario.py` with WSX_SCENARIO_ON=ci) only when this Mac cannot run
the scenario, ref: docs/design/scheduler.md. It pushes HEAD's tree, with origin/main's
`.github/workflows` (a push that changes them needs a token scope wsx lacks), to the branch
`wsx-scenario/<name>/<time>-<commit>[--<part>]`; ci.yml runs the parts of that name (the part, when
given). The script waits for the run, prints each failed `FAILED section` / `SKIPPED section` line,
puts the run's capture files under `.work/web-captures`, deletes the branch, and exits 0 only when the
run passed.
"""
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
POLL = 15
DEADLINE = 70 * 60
FILES_UNDER = '.work/web-captures'
MARKS = ('FAILED section', 'SKIPPED section', 'SECTIONS:')


def run(*argv, env=None, check=True):
    done = subprocess.run(argv, cwd=ROOT, env=env, capture_output=True, text=True)
    if check and done.returncode:
        raise RuntimeError('%s: %s' % (' '.join(argv[:3]), done.stderr.strip()[:300]))
    return done


def tree_with_main_workflows(scratch):
    env = dict(os.environ, GIT_INDEX_FILE=str(scratch / 'index'))
    run('git', 'read-tree', 'HEAD', env=env)
    run('git', 'rm', '-r', '--cached', '-q', '--ignore-unmatch', '.github/workflows', env=env)
    run('git', 'read-tree', '--prefix=.github/workflows/', 'origin/main:.github/workflows', env=env)
    return run('git', 'write-tree', env=env).stdout.strip()


def push(branch, scratch):
    run('git', 'fetch', '-q', 'origin', 'main')
    head = run('git', 'rev-parse', 'HEAD').stdout.strip()
    commit = run('git', 'commit-tree', tree_with_main_workflows(scratch), '-p', head, '-m', 'scenario ' + branch).stdout.strip()
    run('git', '-c', 'credential.helper=', '-c', 'credential.helper=!gh auth git-credential',
        'push', '-q', run('git', 'remote', 'get-url', 'origin').stdout.strip(), '%s:refs/heads/%s' % (commit, branch))
    return commit


def find_run(branch):
    """The run ci.yml started for the branch; it appears a few seconds after the push."""
    for _ in range(20):
        runs = json.loads(run('gh', 'run', 'list', '--workflow', 'ci.yml', '--branch', branch, '--limit', '1',
                              '--json', 'databaseId,url').stdout or '[]')
        if runs:
            return runs[0]
        time.sleep(POLL)
    raise RuntimeError('no CI run started for %s' % branch)


def wait(run_id):
    end = time.time() + DEADLINE
    while time.time() < end:
        view = json.loads(run('gh', 'run', 'view', str(run_id), '--json', 'status,conclusion').stdout)
        if view['status'] == 'completed':
            return view['conclusion']
        time.sleep(POLL)
    raise RuntimeError('the CI run did not end in %d minutes' % (DEADLINE // 60))


def failed_marks(run_id):
    log = run('gh', 'run', 'view', str(run_id), '--log-failed', check=False).stdout
    for line in log.splitlines():
        if any(mark in line for mark in MARKS):
            print(line.split('\t', 2)[-1], flush=True)


def take_files(run_id, scratch):
    folder = scratch / 'files'
    if run('gh', 'run', 'download', str(run_id), '-p', 'scenario-files*', '-D', str(folder), check=False).returncode:
        return
    for archive in sorted(folder.rglob('files.tar')):
        with tarfile.open(archive) as tar:
            for member in tar.getmembers():
                parts = Path(member.name).parts
                if not (member.isfile() or member.isdir()) or member.name.startswith('/') or '..' in parts \
                        or not member.name.startswith(FILES_UNDER):
                    continue
                tar.extract(member, ROOT)


def main():
    args = sys.argv[1:]
    if not args or args[0] not in ('task', 'web') or len(args) not in (1, 3) or args[1:2] not in ([], ['--part']):
        sys.exit('usage: ci-scenario.py task|web [--part <part>]')
    name, part = args[0], (args[2] if len(args) == 3 else os.environ.get('WSX_SCENARIO_PART') or '')
    head = run('git', 'rev-parse', 'HEAD').stdout.strip()
    branch = 'wsx-scenario/%s/%d-%s%s' % (name, time.time(), head[:12], '--' + part if part else '')
    scratch = Path(tempfile.mkdtemp(prefix='ci-scenario-', dir=ROOT / '.work' if (ROOT / '.work').is_dir() else None))
    pushed = False
    try:
        push(branch, scratch)
        pushed = True
        started = find_run(branch)
        print('%s on CI for %s: %s' % (name, head[:12], started['url']), flush=True)
        conclusion = wait(started['databaseId'])
        take_files(started['databaseId'], scratch)
        if conclusion != 'success':
            failed_marks(started['databaseId'])
            print('CI run %s: %s' % (started['url'], conclusion), flush=True)
            sys.exit(1)
        print('CI run passed: %s' % started['url'], flush=True)
    finally:
        if pushed:
            run('git', '-c', 'credential.helper=', '-c', 'credential.helper=!gh auth git-credential', 'push', '-q',
                run('git', 'remote', 'get-url', 'origin').stdout.strip(), '--delete', branch, check=False)
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == '__main__':
    main()
