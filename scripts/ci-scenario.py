#!/usr/bin/env python3
"""Run one of wsx's `ci:` scenarios on GitHub Actions and wait for its answer.

`python3 -B scripts/ci-scenario.py <name> [--part <part>]` pushes HEAD's tree, with origin/main's
`.github/workflows`, to the branch `wsx-scenario/<name>/<commit>`. ci.yml runs the job for that
name (`task`, `web`, and `weblinux`, the web scenario on Ubuntu); a scenario with parts (the `web` scenario: `chat`, `tasks`) runs one job for each part at
once, and the run passes only when all pass. `--part` (or `WSX_SCENARIO_PART`, which `wsx task
check-run web --part tasks` sets) runs one part: the branch gets the suffix `--<part>` and the other
parts' jobs skip. The script waits for the run, prints each job's end, the failed jobs' names and the
failed steps' log tail, downloads the run's `scenario-files*` artifacts (files.tar, paths in this
checkout such as `.work/web-captures`) into the same paths, deletes the branch and exits 0 only when the run
passed, 75 (EX_TEMPFAIL) when the run failed for the network or the runner and no scenario step
failed (the host then keeps no result: the next check-run runs it again). A branch a killed run left behind is deleted by a later run once it is STALE_SECONDS
old. The task host runs it outside the slot (docs/tasks.md "Scenarios").
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import time

POLL_SECONDS = 30
FIND_SECONDS = 180
# ^ A run that has not ended by then hangs (the web scenario's step takes about 26 min with its build):
# it is cancelled, well before the host's 75 min limit for a `ci:` command.
RUN_SECONDS = 60 * 60
LOG_LINES = 150
# ^ Past the host's 75 min limit for a `ci:` command, no run still waits on the branch.
STALE_SECONDS = 2 * 3600
# ^ The run's files may land only here: the scenario scripts' output folder.
FILES_UNDER = '.work/'
# ^ Exit status of a failure outside the tree: the host never keeps a result that exits with it.
TEMPFAIL = 75
# ^ A scenario's own failure: a log with one is never a temporary failure.
SCENARIO_FAILURE = re.compile(r'\bFAIL\b|AssertionError|Traceback')
# ^ The one list of network and runner failures; add a line of a new one here (docs/tasks.md "Scenarios").
TEMPORARY_FAILURES = re.compile('|'.join([
    r'UnknownHostName', r'Could not resolve host', r'unable to connect to server', r'Connection reset',
    r'Connection timed out', r'TLS handshake', r'The operation was canceled',
    r'The runner has received a shutdown signal', r'lost communication with the server',
    r'HTTP(/[\d.]+)? 5\d\d\b', r'\b5\d\d (Internal Server Error|Bad Gateway|Service Unavailable|Gateway Time-?out)',
]), re.IGNORECASE)


def git(*args, env=None):
    return subprocess.run(['git', *args], capture_output=True, text=True, check=True,
                          env=env).stdout.rstrip()


def gh(*args, check=True):
    ran = subprocess.run(['gh', *args], capture_output=True, text=True)
    if check and ran.returncode != 0:
        raise RuntimeError('gh %s: %s' % (' '.join(args), ran.stderr.strip()))
    return ran


def say(text):
    print(text, flush=True)


def temporary_failure(log):
    """True when the failed run's log shows a network or runner failure and no scenario failure."""
    return bool(TEMPORARY_FAILURES.search(log)) and not SCENARIO_FAILURE.search(log)


def tree_for_ci(main, scratch):
    """HEAD's tree with origin/main's workflows: CI runs main's jobs, and a push that
    changes `.github/workflows` needs a token scope wsx does not have."""
    env = dict(os.environ, GIT_INDEX_FILE=str(scratch / 'index'))
    git('read-tree', 'HEAD', env=env)
    git('rm', '-r', '--cached', '-q', '--ignore-unmatch', '.github/workflows', env=env)
    git('read-tree', '--prefix=.github/workflows/', main + ':.github/workflows', env=env)
    return git('write-tree', env=env)


def find_run(repo, commit):
    deadline = time.monotonic() + FIND_SECONDS
    while time.monotonic() < deadline:
        found = json.loads(gh('run', 'list', '--repo', repo, '--commit', commit, '--workflow', 'ci.yml',
                              '--json', 'databaseId,url', '--limit', '1').stdout or '[]')
        if found:
            return found[0]
        time.sleep(5)
    raise RuntimeError('no CI run started for %s in %d s' % (commit, FIND_SECONDS))


def wait_run(repo, run_id):
    said = {}
    deadline = time.monotonic() + RUN_SECONDS
    while True:
        view = json.loads(gh('run', 'view', str(run_id), '--repo', repo,
                             '--json', 'status,conclusion,jobs').stdout)
        for job in view['jobs']:
            state = job['conclusion'] or job['status']
            if said.get(job['name']) != state and state != 'skipped':
                say('%s  %s: %s' % (time.strftime('%H:%M:%S'), job['name'], state))
                said[job['name']] = state
        if view['status'] == 'completed':
            return view
        if time.monotonic() > deadline:
            raise RuntimeError('the CI run did not end in %d min; it is cancelled' % (RUN_SECONDS // 60))
        time.sleep(POLL_SECONDS)


def take_files(repo, run_id, root, scratch):
    """The run's `scenario-files*` artifacts (one for each part) into this checkout, under
    FILES_UNDER only."""
    folder = scratch / 'files'
    if gh('run', 'download', str(run_id), '--repo', repo, '-p', 'scenario-files*', '-D', str(folder),
          check=False).returncode != 0:
        return
    archives = sorted(folder.rglob('files.tar'))
    opened = [tarfile.open(archive) for archive in archives]
    try:
        members = [(tar, m) for tar in opened for m in tar.getmembers() if m.isfile() or m.isdir()]
        for _, member in members:
            parts = Path(member.name).parts
            if member.name.startswith('/') or '..' in parts or not member.name.startswith(FILES_UNDER):
                raise RuntimeError('scenario-files holds %r, outside %s' % (member.name, FILES_UNDER))
        # ^ A folder the run sends replaces this checkout's copy, as a local run rewrites it; once
        # for all parts, since each part's archive holds the same folder.
        tops = sorted({str(Path(*Path(m.name).parts[:2])) for _, m in members if len(Path(m.name).parts) > 1})
        for top in tops:
            shutil.rmtree(root / top, ignore_errors=True)
        for tar, member in members:
            target = root / member.name
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            with tar.extractfile(member) as source, open(target, 'wb') as out:
                shutil.copyfileobj(source, out)
        if members:
            say('files: %d from the run, in %s' % (sum(m.isfile() for _, m in members), ', '.join(tops)))
    finally:
        for tar in opened:
            tar.close()


def sweep(repo):
    """Delete the scenario branches of killed runs: the name holds when it was pushed."""
    refs = json.loads(gh('api', 'repos/%s/git/matching-refs/heads/wsx-scenario/' % repo, check=False).stdout or '[]')
    for ref in refs if isinstance(refs, list) else []:
        pushed = ref['ref'].rsplit('/', 1)[-1].split('-', 1)[0]
        if pushed.isdigit() and time.time() - int(pushed) > STALE_SECONDS:
            gh('api', '-X', 'DELETE', 'repos/%s/%s' % (repo, ref['ref'].replace('refs/', 'git/refs/', 1)), check=False)


def parts_of(workflow, name):
    """The parts the scenario's job in ci.yml runs for (`part: [a, b]` in its matrix), else none."""
    jobs = re.split(r'(?m)^  [\w-]+:\s*$', workflow)
    job = next((j for j in jobs if "'wsx-scenario/%s/'" % name in j and 'matrix.part' in j), '')
    found = re.search(r'part:\s*\[([^\]]*)\]', job)
    return [p.strip() for p in found.group(1).split(',')] if found else []


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    parser.add_argument('name')
    parser.add_argument('--part', default=os.environ.get('WSX_SCENARIO_PART') or None)
    args = parser.parse_args()
    name, part = args.name, args.part
    if not name.replace('-', '').isalnum() or (part and not part.replace('-', '').isalnum()):
        sys.exit('usage: ci-scenario.py <name> [--part <part>]')
    root = Path(git('rev-parse', '--show-toplevel'))
    os.chdir(root)
    dirty = git('status', '--porcelain')
    if dirty:
        sys.exit('commit first: CI runs HEAD\'s tree, and these are not in it:\n' + dirty)
    repo = gh('repo', 'view', '--json', 'nameWithOwner', '-q', '.nameWithOwner').stdout.strip()
    url = 'https://github.com/%s.git' % repo
    git('fetch', '-q', url, 'main')
    main_commit = git('rev-parse', 'FETCH_HEAD')
    workflow = git('show', main_commit + ':.github/workflows/ci.yml')
    if 'wsx-scenario/%s/' % name not in workflow:
        sys.exit('origin/main\'s ci.yml has no job for wsx-scenario/%s/. The ci.yml on main reaches '
                 'origin only with a release (`wsx task release`, scripts/release.sh pushes main); '
                 'until then run the scenario by hand (docs/tasks.md "Scenarios").' % name)
    if part and part not in parts_of(workflow, name):
        sys.exit('origin/main\'s ci.yml runs no part %r of %s (its parts: %s). A part needs the matrix in '
                 'ci.yml, which reaches origin only with a release.'
                 % (part, name, ', '.join(parts_of(workflow, name)) or 'none'))
    scratch = root / '.work' / ('ci-scenario-%d' % os.getpid())
    scratch.mkdir(parents=True)
    branch, run, ended = None, None, False
    # A SIGTERM (the host's timeout) still runs the cleanup below.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    try:
        head = git('rev-parse', 'HEAD')
        commit = git('commit-tree', tree_for_ci(main_commit, scratch), '-p', head,
                     '-m', 'wsx scenario %s for %s' % (name, head))
        sweep(repo)
        branch = 'wsx-scenario/%s/%d-%s%s' % (name, time.time(), commit[:12], '--' + part if part else '')
        subprocess.run(['git', '-c', 'credential.helper=', '-c', 'credential.helper=!gh auth git-credential',
                        'push', '-q', url, '%s:refs/heads/%s' % (commit, branch)], check=True)
        run = find_run(repo, commit)
        say('%s on CI for %s: %s' % (name, head[:12], run['url']))
        view = wait_run(repo, run['databaseId'])
        ended = True
        take_files(repo, run['databaseId'], root, scratch)
        if view['conclusion'] != 'success':
            failed = gh('run', 'view', str(run['databaseId']), '--repo', repo, '--log-failed', check=False).stdout
            say('\n'.join(failed.splitlines()[-LOG_LINES:]))
            # ^ A matrix job's name holds its part: the failure names it.
            say('failed: %s' % ', '.join(j['name'] for j in view['jobs']
                                         if j['conclusion'] in ('failure', 'timed_out', 'cancelled')))
            if temporary_failure(failed):
                say('%s failed on CI for the network or the runner, not the tree (%s): %s\n'
                    'a temporary failure: the next check-run runs it again.' % (name, view['conclusion'], run['url']))
                return TEMPFAIL
            say('%s failed on CI (%s): %s' % (name, view['conclusion'], run['url']))
            return 1
        say('%s passed on CI: %s' % (name, run['url']))
        return 0
    finally:
        if run and not ended:
            gh('run', 'cancel', str(run['databaseId']), '--repo', repo, check=False)
        if branch:
            gh('api', '-X', 'DELETE', 'repos/%s/git/refs/heads/%s' % (repo, branch), check=False)
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
