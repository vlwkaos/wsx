#!/usr/bin/env python3
"""Polling of scripts/ci-scenario.py against a fake `gh` on PATH (no network, no token).

`python3 -B scripts/test-ci-scenario.py` runs `wait_run` and `gh` through the real code for four
answers of GitHub: a rate limit that resets soon (the script waits and passes), one that resets after
the deadline (TempFail, exit 75), an invalid token (TempFail, one line) and a plain run (the request
count). The fake counts every call in $FAKE_GH_DIR/calls.
"""

import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import time

FAKE = r'''#!/usr/bin/env python3
import json, os, sys, time
d = os.environ['FAKE_GH_DIR']
mode = os.environ['FAKE_GH_MODE']
args = sys.argv[1:]
def bump(name):
    p = os.path.join(d, name)
    n = int(open(p).read()) + 1 if os.path.exists(p) else 1
    open(p, 'w').write(str(n))
    return n
with open(os.path.join(d, 'calls'), 'a') as log:
    log.write(' '.join(args) + '\n')
if args[:2] == ['api', 'rate_limit']:
    reset = float(open(os.path.join(d, 'reset')).read())
    if mode == 'token':
        sys.stderr.write('gh: Bad credentials (HTTP 401)\n'); sys.exit(1)
    left = 0 if time.time() < reset else 5000
    print(json.dumps({'resources': {'core': {'limit': 5000, 'remaining': left, 'reset': int(reset)}}}))
    sys.exit(0)
if mode == 'token':
    sys.stderr.write('gh: Bad credentials (HTTP 401)\n'); sys.exit(1)
n = bump('requests')
if args[1].endswith('/jobs?per_page=100'):
    last = int(open(os.path.join(d, 'runs')).read()) >= 5
    print(json.dumps({'jobs': [
        {'name': 'web-scenario (chat)', 'status': 'completed' if last else 'in_progress', 'conclusion': 'success' if last else None},
        {'name': 'web-scenario (tasks)', 'status': 'completed', 'conclusion': 'skipped'}]}))
    sys.exit(0)
runs = bump('runs')
if mode in ('limit', 'late') and runs == 3 and not os.path.exists(os.path.join(d, 'limited')):
    open(os.path.join(d, 'limited'), 'w').write('1')
    open(os.path.join(d, 'reset'), 'w').write(str(time.time() + (2 if mode == 'limit' else 3600)))
    open(os.path.join(d, 'runs'), 'w').write(str(runs - 1))
    sys.stderr.write('gh: API rate limit exceeded for user ID 1. (HTTP 403)\n'); sys.exit(1)
done = runs >= 5
print(json.dumps({'status': 'completed' if done else ('queued' if runs == 1 else 'in_progress'),
                  'conclusion': 'success' if done else None}))
'''


def check(ok, text):
    print(('ok   ' if ok else 'FAIL ') + text, flush=True)
    if not ok:
        sys.exit(1)


def load():
    spec = importlib.util.spec_from_file_location('ci_scenario', Path(__file__).with_name('ci-scenario.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.POLL_SECONDS = module.SLOW_POLL_SECONDS = 0.1
    return module


def scenario(mode, folder):
    """Run wait_run once in a fresh module against the fake; (result or TempFail, seconds, calls)."""
    work = Path(tempfile.mkdtemp(dir=folder))
    (work / 'bin').mkdir()
    fake = work / 'bin' / 'gh'
    fake.write_text(FAKE)
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
    (work / 'reset').write_text('0')
    os.environ.update(FAKE_GH_DIR=str(work), FAKE_GH_MODE=mode, PATH=str(work / 'bin') + os.pathsep + os.environ['PATH'])
    module = load()
    if mode == 'late':
        module.DEADLINE = time.monotonic() + 30
    begun = time.monotonic()
    try:
        out = module.wait_run('o/r', 1, 'chat')
    except module.TempFail as why:
        out = why
    return out, time.monotonic() - begun, (work / 'calls').read_text().splitlines() if (work / 'calls').exists() else [], module


def main():
    with tempfile.TemporaryDirectory() as folder:
        out, seconds, calls, module = scenario('plain', folder)
        check(out['conclusion'] == 'success', 'a plain run passes')
        polls = [c for c in calls if c.endswith('/runs/1')]
        jobs = [c for c in calls if c.endswith('/jobs?per_page=100')]
        check(len(polls) == 5 and len(jobs) == 3, 'five polls of the run, the jobs read at the first poll, the status change and the end (%d, %d)' % (len(polls), len(jobs)))
        check(not any('run view' in c for c in calls), 'no `gh run view`')

        out, seconds, calls, module = scenario('limit', folder)
        check(not isinstance(out, Exception) and out['conclusion'] == 'success', 'a rate limit answer is waited out and the run passes')
        check(seconds >= 2, 'the wait lasted until the reset (%.1f s)' % seconds)
        check(sum(c == 'api rate_limit' for c in calls) == 1, 'the reset time came from one `gh api rate_limit`')
        print('     requests: %d (polls and jobs; the failed poll counted once more); gh calls %s' % (len(calls) - 1, module.CALLS))

        out, seconds, calls, module = scenario('late', folder)
        check(isinstance(out, module.TempFail) and 'after the deadline' in str(out), 'a reset after the deadline is a TempFail (%s)' % out)
        check(seconds < 5, 'it did not wait')

        out, seconds, calls, module = scenario('token', folder)
        check(isinstance(out, module.TempFail) and 'token is invalid' in str(out) and '\n' not in str(out), 'an invalid token is a one-line TempFail (%s)' % out)


if __name__ == '__main__':
    main()
