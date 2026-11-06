#!/usr/bin/env python3
"""Run one of wsx's `ci:` scenarios on GitHub Actions and wait for its answer.

`python3 -B scripts/ci-scenario.py <name>` pushes HEAD's tree, with origin/main's
`.github/workflows`, to the branch `wsx-scenario/<name>/<commit>`. ci.yml runs the job for that
name. The script waits for the run, prints each job's end and the failed steps' log tail,
downloads the run's `scenario-files` artifact (files.tar, paths in this checkout such as
`.work/web-captures`) into the same paths, deletes the branch and exits 0 only when the run
passed. A branch a killed run left behind is deleted by a later run once it is STALE_SECONDS
old. The task host runs it outside the slot (docs/tasks.md "Scenarios").
"""

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tarfile
import time

POLL_SECONDS = 30
FIND_SECONDS = 180
# ^ A run that has not ended by then hangs (the web scenario takes about 10 min with its build):
# it is cancelled, well before the host's 75 min limit for a `ci:` command.
RUN_SECONDS = 60 * 60
LOG_LINES = 150
# ^ Past the host's 75 min limit for a `ci:` command, no run still waits on the branch.
STALE_SECONDS = 2 * 3600
# ^ The run's files may land only here: the scenario scripts' output folder.
FILES_UNDER = '.work/'


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
    """The run's `scenario-files` artifact into this checkout, under FILES_UNDER only."""
    folder = scratch / 'files'
    if gh('run', 'download', str(run_id), '--repo', repo, '-n', 'scenario-files', '-D', str(folder),
          check=False).returncode != 0:
        return
    archive = folder / 'files.tar'
    if not archive.is_file():
        return
    with tarfile.open(archive) as tar:
        members = [m for m in tar.getmembers() if m.isfile() or m.isdir()]
        for member in members:
            parts = Path(member.name).parts
            if member.name.startswith('/') or '..' in parts or not member.name.startswith(FILES_UNDER):
                raise RuntimeError('scenario-files holds %r, outside %s' % (member.name, FILES_UNDER))
        # ^ A folder the run sends replaces this checkout's copy, as a local run rewrites it.
        tops = sorted({str(Path(*Path(m.name).parts[:2])) for m in members if len(Path(m.name).parts) > 1})
        for top in tops:
            shutil.rmtree(root / top, ignore_errors=True)
        for member in members:
            target = root / member.name
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            with tar.extractfile(member) as source, open(target, 'wb') as out:
                shutil.copyfileobj(source, out)
        say('files: %d from the run, in %s' % (sum(m.isfile() for m in members), ', '.join(tops)))


def sweep(repo):
    """Delete the scenario branches of killed runs: the name holds when it was pushed."""
    refs = json.loads(gh('api', 'repos/%s/git/matching-refs/heads/wsx-scenario/' % repo, check=False).stdout or '[]')
    for ref in refs if isinstance(refs, list) else []:
        pushed = ref['ref'].rsplit('/', 1)[-1].split('-', 1)[0]
        if pushed.isdigit() and time.time() - int(pushed) > STALE_SECONDS:
            gh('api', '-X', 'DELETE', 'repos/%s/%s' % (repo, ref['ref'].replace('refs/', 'git/refs/', 1)), check=False)


def main():
    if len(sys.argv) != 2 or not sys.argv[1].replace('-', '').isalnum():
        sys.exit('usage: ci-scenario.py <name>')
    name = sys.argv[1]
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
        branch = 'wsx-scenario/%s/%d-%s' % (name, time.time(), commit[:12])
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
