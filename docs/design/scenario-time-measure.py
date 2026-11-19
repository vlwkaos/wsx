#!/usr/bin/env python3
"""Where a CI scenario round's time goes (task 517). Reads `gh` only; caches into .work/t517-logs.

  python3 -B docs/design/scenario-time-measure.py fetch [N]   # last N (20) completed task and web runs: jobs + logs
  python3 -B docs/design/scenario-time-measure.py report      # tables
"""
import json, re, statistics, subprocess, sys
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from pathlib import Path

DIR = Path(__file__).resolve().parent / 't517-logs'
REPO = 'vlwkaos/wsx'


def gh(*args):
    return subprocess.run(['gh', *args], capture_output=True, text=True).stdout


def ts(s):
    return datetime.strptime(s[:19], '%Y-%m-%dT%H:%M:%S').timestamp() if s else None


def runs(kind, n):
    out = json.loads(gh('run', 'list', '--workflow', 'ci.yml', '--limit', '300', '--json',
                        'databaseId,conclusion,status,headBranch,createdAt,updatedAt'))
    pick = [r for r in out if r['headBranch'].startswith('wsx-scenario/%s/' % kind) and r['status'] == 'completed']
    return pick[:n]


def fetch(n):
    DIR.mkdir(exist_ok=True)
    todo = []
    for kind in ('task', 'web'):
        for r in runs(kind, n):
            todo.append((kind, r))
    def one(item):
        kind, r = item
        i = r['databaseId']
        meta = DIR / ('%d.meta.json' % i)
        if not meta.exists():
            jobs = json.loads(gh('api', '--paginate', 'repos/%s/actions/runs/%d/jobs' % (REPO, i))
                              .replace('}{', '}\n{').split('\n')[0] or '{}')
            meta.write_text(json.dumps({'kind': kind, 'run': r, 'jobs': jobs.get('jobs', [])}))
        log = DIR / ('%d.log' % i)
        if not log.exists() or log.stat().st_size == 0:
            log.write_text(gh('run', 'view', str(i), '--log'))
    with ThreadPoolExecutor(6) as ex:
        list(ex.map(one, todo))
    print('fetched', len(todo))


LINE = re.compile(r'^(?P<job>[^\t]+)\t[^\t]*\t﻿?(?P<t>\d{4}-\d\d-\d\dT[\d:]+)\.\d+Z (?P<msg>.*)$')


def load():
    data = []
    for meta in sorted(DIR.glob('*.meta.json')):
        m = json.loads(meta.read_text())
        i = m['run']['databaseId']
        log = (DIR / ('%d.log' % i)).read_text(errors='replace')
        data.append((m, log))
    return data


def part_of(job):
    mm = re.search(r'\((\w+)\)', job)
    return mm.group(1) if mm else job


def report():
    data = load()
    jobrows = []
    secs = defaultdict(lambda: defaultdict(list))   # (kind, part) -> section -> [seconds]
    builds = defaultdict(list)
    for m, log in data:
        kind, run = m['kind'], m['run']
        for j in m['jobs']:
            if j['conclusion'] == 'skipped' or not j['started_at'] or not j['completed_at']:
                continue
            part = part_of(j['name'])
            steps = {s['name']: (ts(s['started_at']), ts(s['completed_at'])) for s in j['steps'] if s['started_at'] and s['completed_at']}
            run_step = next((v for k, v in steps.items() if k.startswith('Run the')), None)
            setup = (run_step[0] - ts(j['started_at'])) if run_step else None
            jobrows.append(dict(kind=kind, part=part, id=run['databaseId'], os=('mac' if 'macos' in ','.join(j['labels']) else 'linux'),
                                wait=ts(j['started_at']) - ts(j['created_at']), setup=setup,
                                run=(run_step[1] - run_step[0]) if run_step else None,
                                total=ts(j['completed_at']) - ts(j['created_at']), concl=j['conclusion']))
        # sections
        for line in log.splitlines():
            mm = LINE.match(line)
            if not mm:
                continue
            msg = re.sub(r'\x1b\[[\d;]*m|\^\[\[[\d;]*m', '', mm['msg'])
            part = part_of(mm['job'])
            if msg.startswith('{"ok"') and '"parts"' in msg:
                try:
                    d = json.loads(msg)
                except ValueError:
                    continue
                for name, sec in d['parts'].items():
                    secs[(kind, part)][name].append(sec)
            mt = re.match(r'\s*([\d.]+)s  (.*)', msg)
            if mt:
                secs[(kind, part, 'web', run['databaseId'])][mt[2]] = float(mt[1])
            bm = re.search(r'Finished `dev` profile .* in ([\dms. ]+)', msg)
            if bm:
                t = re.match(r'(?:(\d+)m )?([\d.]+)s', bm[1])
                if t:
                    builds[(kind, part)].append(int(t[1] or 0) * 60 + float(t[2]))
    print('## jobs (seconds): wait = created to started; setup = started to scenario step; run = scenario step')
    g = defaultdict(list)
    for r in jobrows:
        g[(r['kind'], r['part'], r['os'])].append(r)
    for k, rows in sorted(g.items()):
        def med(f):
            v = [r[f] for r in rows if r[f] is not None]
            return '%d/%d' % (statistics.median(v), max(v)) if v else '-'
        print(k, 'n=%d' % len(rows), 'wait p50/max', med('wait'), 'setup', med('setup'), 'run', med('run'), 'total', med('total'),
              'build p50', int(statistics.median(builds[(k[0], k[1])])) if builds[(k[0], k[1])] else '-')
    print('\n## wait per job, by run (kind part os wait)')
    for r in sorted(jobrows, key=lambda r: r['id']):
        print(r['id'], r['kind'], r['part'], r['os'], 'wait', int(r['wait']), 'run', int(r['run'] or 0), r['concl'])
    print('\n## sections (p50 / max / n), task by part')
    allsec = []
    for key, d in secs.items():
        if len(key) == 2 and key[0] == 'task':
            for name, v in d.items():
                allsec.append((statistics.median(v), max(v), len(v), key[1], name))
    # web: per-check increments from cumulative timed lines
    web = defaultdict(list)
    for key, d in secs.items():
        if len(key) == 4:
            prev = 0
            for label, at in d.items():
                web[(key[1], label)].append(at - prev)
                prev = at
    for (part, label), v in web.items():
        allsec.append((statistics.median(v), max(v), len(v), 'web-' + part, label))
    for row in sorted(allsec, reverse=True)[:40]:
        print('%6.1f %6.1f n=%-3d %-10s %s' % row)


if __name__ == '__main__':
    if sys.argv[1] == 'fetch':
        fetch(int(sys.argv[2]) if len(sys.argv) > 2 else 20)
    else:
        report()
