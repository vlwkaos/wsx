#!/usr/bin/env python3
"""Run one of wsx's `ci:` scenarios on GitHub Actions and wait for its answer.

`python3 -B scripts/ci-scenario.py <name> [--part <part>]` pushes HEAD's tree, with origin/main's
`.github/workflows`, to the branch `wsx-scenario/<name>/<commit>`. ci.yml runs the job for that
name (`task`, `web`, and `weblinux`, the web scenario on Ubuntu); a scenario with parts (`web`: `chat`, `tasks`; `task`: `core`, `worker`, `host`) runs one job for each part at
once, and the run passes only when all pass. `--part` (or `WSX_SCENARIO_PART`, which `wsx task
check-run web --part tasks` sets) runs one part: the branch gets the suffix `--<part>` and the other
parts' jobs skip. The script waits for the run, prints each job's end, the failed jobs' names and the
failed steps' log tail, downloads the run's `scenario-files*` artifacts (files.tar, paths in this
checkout such as `.work/web-captures`) into the same paths, deletes the branch and exits 0 only when the run
passed, 75 (EX_TEMPFAIL) when the run failed for the network or the runner and no scenario step
failed, or the GitHub token is invalid, or its rate limit resets after the deadline (the host then keeps
no result: the next check-run runs it again). A rate limit answer is never a failure: the script waits
for the reset time of `gh api rate_limit` (that endpoint does not count) and goes on. A poll costs one
request (the run), and the jobs are read when the run's status changes, every JOBS_SECONDS and at the
end. A branch a killed run left behind is deleted by a later run once it is STALE_SECONDS
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
# ^ A run older than SLOW_AFTER seconds is polled every SLOW_POLL_SECONDS.
SLOW_AFTER = 10 * 60
SLOW_POLL_SECONDS = 60
# ^ The run's status is no sign that its own jobs started (with a part, the other parts' jobs end at
# once and the run is `in_progress` while the own job still queues): the jobs are read at least this often.
JOBS_SECONDS = 5 * 60
FIND_SECONDS = 180
# ^ A run whose jobs started and have not ended by then hangs (the web scenario's step takes about 26 min
# with its build): it is cancelled. The time the run waited for a runner does not count (QUEUE_SECONDS).
RUN_SECONDS = 60 * 60
# ^ The account runs at most 5 macOS jobs at once: a run whose jobs have not started by then is cancelled.
QUEUE_SECONDS = 60 * 60
LOG_LINES = 150
# ^ The host's `ci:` limit is QUEUE_SECONDS + RUN_SECONDS + 5 min (CI_TIMEOUT in
# crates/wsx/src/task/host.rs); past it, no run still waits on the branch.
STALE_SECONDS = 3 * 3600
# ^ The run's files may land only here: the scenario scripts' output folder.
FILES_UNDER = '.work/'
# ^ Exit status of a failure outside the tree: the host never keeps a result that exits with it.
TEMPFAIL = 75
# ^ The script's whole time: the host's `ci:` limit (CI_TIMEOUT in crates/wsx/src/task/host.rs). A rate
# limit that resets after it is a temporary failure.
DEADLINE = time.monotonic() + QUEUE_SECONDS + RUN_SECONDS + 5 * 60
# ^ What the failed `gh` call says. A 429, or a 403 that names the limit, is a rate limit; a 401 is a bad token.
RATE_LIMITED = re.compile(r'rate limit|abuse detection|HTTP 429', re.IGNORECASE)
BAD_TOKEN = re.compile(r'HTTP 401|Bad credentials|token .{0,40}invalid|gh auth login', re.IGNORECASE)
# ^ A call that fails with one of these asks `gh api rate_limit`, which does not count against the limit.
ASK_LIMIT = re.compile(r'HTTP (401|403|429)|rate limit|abuse detection|Bad credentials|gh auth login', re.IGNORECASE)
SECONDARY_WAIT = 60
CALLS = {'gh': 0, 'rate_limit': 0}


class TempFail(Exception):
    """A failure outside the tree (exit 75): the token or the rate limit, not the scenario."""
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


def gh(*args, check=True, patient=True):
    """Run `gh`. A rate limit answer waits for the reset and runs the call again; an invalid token
    raises TempFail. `patient=False` (the cleanup) never waits."""
    while True:
        CALLS['gh'] += 1
        ran = subprocess.run(['gh', *args], capture_output=True, text=True)
        if ran.returncode == 0:
            return ran
        pause = limit_pause(ran.stderr + ran.stdout) if patient else None
        if pause is None:
            if check:
                raise RuntimeError('gh %s: %s' % (' '.join(args), ran.stderr.strip()))
            return ran
        if time.monotonic() + pause > DEADLINE:
            raise TempFail('GitHub rate limit resets in %d s, after the deadline of this run.' % pause)
        say('%s  GitHub rate limit: waiting %d s' % (time.strftime('%H:%M:%S'), pause))
        time.sleep(pause)


def limit_pause(text):
    """Seconds to wait before the failed call runs again, or None when the failure is not the rate
    limit. The core limit (remaining 0) waits until its reset; a secondary limit (the text says so, or
    a 429) waits Retry-After, else SECONDARY_WAIT. A bad token raises TempFail."""
    if not ASK_LIMIT.search(text):
        return None
    CALLS['rate_limit'] += 1
    ran = subprocess.run(['gh', 'api', 'rate_limit'], capture_output=True, text=True)
    try:
        core = json.loads(ran.stdout)['resources']['core']
    except (ValueError, KeyError, TypeError):
        core = None
    if core and core['remaining'] == 0:
        return max(1, int(core['reset'] - time.time()) + 2)
    if RATE_LIMITED.search(text):
        found = re.search(r'retry[- ]after:?\s*(\d+)', text, re.IGNORECASE)
        return int(found.group(1)) + 1 if found else SECONDARY_WAIT
    if BAD_TOKEN.search(text) or (core is None and 'HTTP 401' in ran.stderr):
        raise TempFail('The GitHub token is invalid (gh auth login): %s' % text.strip().splitlines()[0][:150])
    return None


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


def own_jobs(jobs, part):
    """The run's jobs that matter: with a part, a job named `... (<other part>)` is left out. CI
    starts every part's job on a `--<part>` branch (a job `if` cannot read the matrix) and the
    others end green at once, which would start the run clock and print their ends."""
    if not part:
        return jobs
    return [job for job in jobs if not re.search(r'\((?!%s\)$)[\w-]+\)$' % re.escape(part), job['name'])]


def jobs_started(jobs):
    """True once a job left the queue: it runs or ended (a skipped job never ran)."""
    return any(job['status'] in ('in_progress', 'completed') and job['conclusion'] != 'skipped' for job in jobs)


def overdue(queued, running):
    """Why the run is cancelled, or None: `queued` seconds with no job started, else `running`
    seconds since the first job started (None while it waits)."""
    if running is None:
        return 'no job started in %d min of queue' % (QUEUE_SECONDS // 60) if queued > QUEUE_SECONDS else None
    return 'the jobs did not end in %d min' % (RUN_SECONDS // 60) if running > RUN_SECONDS else None


def wait_run(repo, run_id, part=None):
    """The finished run's status, conclusion and jobs; the queue wait is in the first line said when
    the jobs start. A poll is one request, the run; the jobs are read when its status changes, every
    JOBS_SECONDS and at the end."""
    said = {}
    begun = time.monotonic()
    started = None
    status, read_at, jobs = None, None, []
    while True:
        run = json.loads(gh('api', 'repos/%s/actions/runs/%s' % (repo, run_id)).stdout)
        now = time.monotonic()
        if run['status'] != status or run['status'] == 'completed' or now - read_at >= JOBS_SECONDS:
            jobs = own_jobs(json.loads(gh('api', 'repos/%s/actions/runs/%s/jobs?per_page=100' % (repo, run_id)).stdout)['jobs'], part)
            read_at = now
            for job in jobs:
                state = job['conclusion'] or job['status']
                if said.get(job['name']) != state and state != 'skipped':
                    say('%s  %s: %s' % (time.strftime('%H:%M:%S'), job['name'], state))
                    said[job['name']] = state
            if started is None and jobs_started(jobs):
                started = now
                say('%s  queued %d min before its jobs started' % (time.strftime('%H:%M:%S'), (started - begun) // 60))
        status = run['status']
        if status == 'completed':
            return {'status': status, 'conclusion': run['conclusion'], 'jobs': jobs}
        late = overdue(now - begun, None if started is None else now - started)
        if late:
            raise RuntimeError('the CI run is cancelled: %s' % late)
        time.sleep(SLOW_POLL_SECONDS if now - begun > SLOW_AFTER else POLL_SECONDS)


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
        view = wait_run(repo, run['databaseId'], part)
        ended = True
        take_files(repo, run['databaseId'], root, scratch)
        if view['conclusion'] != 'success':
            failed = gh('run', 'view', str(run['databaseId']), '--repo', repo, '--log-failed', check=False).stdout
            # ^ One tail for each failed job: a job's name holds its part, and a long log of one
            # part never cuts another's failure. The run reports every failed part at once.
            jobs = {}
            for line in failed.splitlines():
                jobs.setdefault(line.split('\t', 1)[0], []).append(line)
            for lines in jobs.values():
                say('\n'.join(lines[-LOG_LINES:]))
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
            gh('run', 'cancel', str(run['databaseId']), '--repo', repo, check=False, patient=False)
        if branch:
            gh('api', '-X', 'DELETE', 'repos/%s/git/refs/heads/%s' % (repo, branch), check=False, patient=False)
        shutil.rmtree(scratch, ignore_errors=True)


def run():
    try:
        code = main()
    except TempFail as why:
        say('%s\na temporary failure: the next check-run runs it again.' % why)
        code = TEMPFAIL
    say('gh calls: %(gh)d (and %(rate_limit)d of `gh api rate_limit`, which is free)' % CALLS)
    return code


if __name__ == '__main__':
    sys.exit(run())
