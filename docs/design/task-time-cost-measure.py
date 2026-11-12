#!/usr/bin/env python3
"""Task time and cost: where a task's wall time and money go, from the task host's logs.

ref: docs/design/task-time-cost-audit.md (the numbers), docs/design/check-scheduling-audit.md (the earlier query)

    python3 -I docs/design/task-time-cost-measure.py [--since 2026-11-08] [--gh] [--state ~/.local/state/wsx/tasks/wsx-8cbe3f08]

Reads every task created at or after --since (local midnight): <n>/log.jsonl, <n>/task.json, the manager's
log.jsonl and deploys.jsonl. `--gh` fetches the jobs of each CI run a `ci:` check output names, one
`gh api` call a run, into .work/task-time-cost/gh-jobs.json beside the checkout; later runs read the cache.
Each task's wall time from `created` to `landed` is cut into phases by the log's boundary events; a
`limit_wait` inside any phase moves its wait to `limit`. Manager turns are shared evenly over the tasks the
delivered events name (`#<n>`).
"""
import collections as C, datetime as D, glob, json, os, re, subprocess, sys

ARGS = sys.argv[1:]
def arg(name, default):
    return ARGS[ARGS.index(name) + 1] if name in ARGS else default
STATE = os.path.expanduser(arg('--state', '~/.local/state/wsx/tasks/wsx-8cbe3f08'))
SINCE = int(D.datetime.strptime(arg('--since', '2026-11-08'), '%Y-%m-%d').astimezone().timestamp() * 1000)
CACHE = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..', '.work', 'task-time-cost', 'gh-jobs.json')
REPO = 'vlwkaos/wsx'

def events(path):
    out = []
    for line in open(path, errors='replace'):
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out

def pct(values, q):
    values = sorted(values)
    return values[min(len(values) - 1, int(q * len(values)))] if values else 0

MIN = lambda ms: round(ms / 60000, 1)
USD = lambda micro: round(micro / 1e6, 2)
def stat(name, values, unit=MIN):  # p50 / p90 / sum / n
    print(f'  {name:34} n {len(values):3}  p50 {unit(pct(values, .5)):>7}  p90 {unit(pct(values, .9)):>7}  sum {unit(sum(values)):>8}')

# --- the manager's log: when it landed or sent back each task, and what each turn cost ------------------
MLOG = events(os.path.join(STATE, 'manager', 'log.jsonl'))
msg_by_id = {e['id']: e for e in MLOG if e['kind'] == 'message'}
manager_cmd = C.defaultdict(list)   # task -> [(ts, 'land'|'add')]
for e in MLOG:
    if e['kind'] == 'agent':
        m = re.search(r'task (land|add)\b(?:-batch)?((?: \d+)+)', e['text'])
        if m:
            for n in m.group(2).split():
                manager_cmd[int(n)].append((e['unix_ms'], m.group(1)))
mturns = []  # (delivered_ts, reply_ts, usage, tasks named by the delivered events, kinds)
delivered_at = {}
for e in MLOG:
    if e['kind'] == 'delivered':
        for i in e['ids']:
            delivered_at[i] = e['unix_ms']
    if e['kind'] == 'reply' and e.get('usage') and e['unix_ms'] >= SINCE:
        named, kinds = C.Counter(), set()
        for i in e['to']:
            m = msg_by_id.get(i, {})
            if m.get('from') == 'events':
                for n in re.findall(r'#(\d+) (ready|landed|waiting|blocked)', m['text']):
                    named[int(n[0])] += 1; kinds.add(n[1])
                if 'fail the same way' in m['text']:
                    kinds.add('shared-failure')
            else:
                kinds.add(m.get('from', 'none').split(':')[0])
        mturns.append((min((delivered_at.get(i, e['unix_ms']) for i in e['to']), default=e['unix_ms']), e['unix_ms'], e['usage'], named, kinds))

# --- each task's phases, steps and costs ------------------------------------------------------------
def kind_of(cmd):
    return next((n for k, n in (('ci:', 'ci'), ('web-client', 'web'), ('test-task', 'task'), ('smoke', 'smoke'), ('boundary', 'boundary'), ('memory', 'memory')) if k in cmd), 'plain')

def measure(n, rec, log, cmds):
    """One task's phases, turns and costs from its log."""
    t = dict(id=n, rec=rec, phase=C.Counter(), model_ms=C.Counter(), cost=C.Counter(), tokens=C.Counter(), turns=[], check_runs=[], checks=[],
             sent_back=0, revisions=0, conflicts=0, ci_runs=[], reads=C.Counter(), rotations=0, pings=0, calls=0, created=rec['created_unix_ms'], landed=None, ready=None, mwait=[])
    cursor, pending, limits, ready, since_ready = t['created'], None, [], None, 'none'  # ready: the latest revision's time
    def cut(ts, phase):
        nonlocal cursor
        dt = ts - cursor
        lim = sum(max(0, min(until, ts) - lts) for lts, until in limits if cursor <= lts < ts)
        t['phase'][phase] += dt - lim; t['phase']['limit'] += lim
        cursor = ts
    def landing(ts):  # the manager's land command before ts ends the manager wait; what follows is the land
        nonlocal since_ready
        land_cmd = next((c[0] for c in cmds if c[1] == 'land' and ready and ready <= c[0] <= ts), None)
        if land_cmd and since_ready != 'landing':
            if land_cmd > cursor:
                cut(land_cmd, 'manager-wait')
            t['mwait'].append(land_cmd - ready); since_ready = 'landing'
    CAUSES = (('gh-403', r'HTTP 403'), ('queue-cancel', r'no job started in \d+ min|did not end in \d+ min|\(cancelled\)|: cancelled'), ('uncommitted', r'commit first:'),
              ('push', r'CalledProcessError.*push|no CI run started'), ('assert', r'AssertionError|\bFAIL\b|FAILED'), ('timeout', r'RuntimeError: .*timeout|TimeoutError'), ('other', r''))
    def fail_key(out):  # (cause, key): the last assert or error line (plus the next when bare), digits as N, as task::failure keys a shared failure
        lines = [re.sub(r'^.*?\d{4}-\d\d-\d\dT[\d:.]+Z\s*', '', l) for l in out.split('\n') if not l.startswith('gh calls:')]
        cause = next(c for c, rx in CAUSES if re.search(rx, out, re.S))
        hits = [i for i, l in enumerate(lines) if re.search(r'AssertionError|FAILED|\bFAIL\b|Error:', l)]
        if not hits:
            return cause, 'other: ' + re.sub(r'\d+', 'N', lines[-1].strip() if lines else '')[:70]
        i = hits[-1]; key = lines[i].strip()
        if key.endswith('AssertionError'):  # a bare assert: its source line is the last one before the carets
            src = next((l.strip() for l in reversed(lines[:i]) if l.strip() and not re.fullmatch(r'[\s^~]+', l)), '')
            key = src + ' ' + key
        return cause, re.sub(r'\d+', 'N', key)[:90]
    for e in log:
        k, ts = e['kind'], e['unix_ms']
        if k == 'agent':
            if e['text'].startswith('Read '):
                t['reads'][e['text'].split()[1]] += 1
            continue
        if k == 'limit_wait':
            limits.append((ts, e['until_ms'])); continue
        if k == 'rotated': t['rotations'] += 1; continue
        if k == 'ping':
            t['pings'] += 1; t['cost']['ping'] += e['usage']['cost_micro_usd']; continue
        if k == 'after':
            t['after_because'] = e.get('because')
        if k in ('disk_wait', 'hold', 'stopped', 'conflict', 'after'):
            pending = {'disk_wait': 'disk', 'hold': 'hold', 'stopped': 'owner', 'conflict': 'land', 'after': 'after'}[k]
            if k == 'conflict': t['conflicts'] += 1
            continue
        if k == 'after_ended':
            cut(ts, 'after'); pending = None; continue
        if k == 'result':
            landing(ts); u = e['usage']; step = e['step'] or 'turn'
            if since_ready == 'sent_back': step = 'sent-back'
            elif since_ready == 'landing': step = 'land-conflict'
            phase = 'disk' if pending == 'disk' else 'land' if since_ready == 'landing' else f'step:{step}'
            cut(ts, phase); pending = None
            t['model_ms'][step] += u['duration_ms']; t['cost'][step] += u['cost_micro_usd']
            for key in ('input_tokens', 'output_tokens', 'cache_read_tokens', 'cache_creation_tokens'):
                t['tokens'][key] += u[key]
            t['turns'].append((step, e['trigger'], e['status'], u['model'], u['cost_micro_usd'], u['duration_ms'], u['context_tokens']))
            t['calls'] += u.get('calls', 0)
            if e['status'] in ('blocked', 'failed'): pending = 'owner'
            continue
        if k == 'check_run':
            rs = e['results']; cut(ts, 'check-run' if pending != 'disk' else 'disk'); pending = None
            local = [r for r in rs if kind_of(r['command']) != 'ci' and not r.get('stored')]
            ci = [r for r in rs if kind_of(r['command']) == 'ci']
            t['check_runs'].append(dict(queue=max((r.get('queued_ms', 0) for r in local), default=0), local=sum(r.get('ms', 0) for r in local), ci=max((r['ms'] for r in ci), default=0),
                                        failed=[kind_of(r['command']) for r in rs if r['exit'] != 0], stored=sum(bool(r.get('stored')) for r in rs)))
            for r in ci:
                m = re.search(r'actions/runs/(\d+)', r['output'])
                q = re.search(r'queued (\d+) min before its jobs started', r['output'])
                c = re.search(r' on CI for ([0-9a-f]{12})', r['output'])
                cause, key = (None, None) if r['exit'] == 0 else fail_key(r['output'])
                t['ci_runs'].append(dict(run=int(m.group(1)) if m else None, ms=r['ms'], ok=r['exit'] == 0, kind='task' if 'task' in r['command'] else 'web', queued_min=int(q.group(1)) if q else 0,
                                         commit=c.group(1) if c else None, key=key, cause=cause, at=ts, task=n))
            continue
        if k == 'checks':
            landing(ts); rs = e['results']; phase = 'land' if since_ready == 'landing' else 'checks'
            cut(ts, phase)
            t['checks'].append(dict(queue=rs[0].get('queued_ms', 0) if rs else 0, run=sum(r.get('ms', 0) for r in rs if not r.get('stored')), ok=all(r['exit'] == 0 for r in rs), phase=phase))
            continue
        if k == 'revision':
            t['revisions'] += 1
            ready = ts
            if t['ready'] is None:
                t['ready'] = ts
            if since_ready != 'landing':
                since_ready = 'ready'
            cursor = ts; continue
        if k == 'message' and e['mode'] in ('add', 'steer'):
            if since_ready == 'ready' and any(c[0] <= ts + 2000 and c[1] == 'add' for c in cmds):
                cut(ts, 'manager-wait'); t['sent_back'] += 1; since_ready = 'sent_back'; t['mwait'].append(ts - ready)
            elif pending in ('owner', 'hold'):
                cut(ts, pending); pending = None
            continue
        if k == 'landed':
            landing(ts); cut(ts, 'land' if since_ready == 'landing' else 'manager-wait'); t['landed'] = ts; continue
        if k == 'child_landed':  # a parent waits for its children; each child's land reruns its checks
            cut(ts, 'parent-wait'); continue
    return t

tasks = {}
for path in sorted(glob.glob(os.path.join(STATE, '[0-9]*', 'task.json')), key=lambda p: int(p.split(os.sep)[-2])):
    rec = json.load(open(path))
    if rec.get('created_unix_ms', 0) < SINCE:
        continue
    n = rec['id']
    log = events(os.path.join(STATE, str(n), 'log.jsonl'))
    tasks[n] = measure(n, rec, log, sorted(manager_cmd.get(n, [])))

# --- manager turns shared over tasks ----------------------------------------------------------------
mcost = C.Counter(); mkind = C.Counter()
for d, r, u, named, kinds in mturns:
    share = u['cost_micro_usd'] / (len(named) or 1)
    for n in named:
        if n in tasks:
            tasks[n]['cost']['manager:' + ('review' if 'ready' in kinds else 'other')] += share
    label = 'tasks' if named else ('owner' if 'owner' in kinds else 'other')
    mcost[label] += u['cost_micro_usd']; mkind[label] += 1

# --- GitHub Actions jobs of the CI runs ---------------------------------------------------------------
cache = json.load(open(CACHE)) if os.path.exists(CACHE) else {}
if '--gh' in ARGS:
    want = {str(c['run']) for t in tasks.values() for c in t['ci_runs'] if c['run']} - set(cache)
    for i, run in enumerate(sorted(want)):
        out = subprocess.run(['gh', 'api', f'repos/{REPO}/actions/runs/{run}/jobs?per_page=100'], capture_output=True, text=True)
        if out.returncode == 0:
            cache[run] = [dict(name=j['name'], conclusion=j['conclusion'], created=j['created_at'], started=j['started_at'], done=j['completed_at'])
                          for j in json.loads(out.stdout)['jobs'] if j['conclusion'] != 'skipped']
        print(f'gh {i + 1}/{len(want)} {run} {out.returncode}', file=sys.stderr)
        os.makedirs(os.path.dirname(CACHE), exist_ok=True); json.dump(cache, open(CACHE, 'w'))  # after each call: a stopped fetch keeps what it got
iso = lambda s: D.datetime.strptime(s, '%Y-%m-%dT%H:%M:%SZ').timestamp() * 1000 if s else None
gh_rows = []  # per CI run named by a window task: kind, queue ms, run ms (longest job), ok
for t in tasks.values():
    for c in t['ci_runs']:
        jobs = cache.get(str(c['run']))
        if not jobs:
            continue
        q = max(iso(j['started']) - iso(j['created']) for j in jobs if j['started'] and j['created'])
        run = max(iso(j['done']) - iso(j['started']) for j in jobs if j['done'] and j['started'])
        gh_rows.append(dict(kind=c['kind'], queue=q, run=run, ok=c['ok'], wall=c['ms'], created=min(iso(j['created']) for j in jobs)))

# --- report ----------------------------------------------------------------------------------------
T = list(tasks.values()); L = [t for t in T if t['landed']]
small = [t for t in L if t['rec'].get('small')]; full = [t for t in L if not t['rec'].get('small')]
print(f'tasks {len(T)} created since {arg("--since", "2026-11-08")}; landed {len(L)} (small {len(small)}, full {len(full)}); open {len(T) - len(L)}')
print('\nT1 created to landed (min)')
stat('all', [t['landed'] - t['created'] for t in L]); stat('small', [t['landed'] - t['created'] for t in small]); stat('full', [t['landed'] - t['created'] for t in full])
print('\nT2 phases over landed tasks (min): sum, share, p50/p90 over the tasks that had it')
total = sum(t['landed'] - t['created'] for t in L)
phases = C.Counter()
for t in L:
    phases.update(t['phase'])
for name, ms in phases.most_common():
    have = [t['phase'][name] for t in L if t['phase'][name] > 0]
    print(f'  {name:18} sum {MIN(ms):>8}  {100 * ms / total:4.1f}%  in {len(have):3} tasks  p50 {MIN(pct(have, .5)):>6}  p90 {MIN(pct(have, .9)):>6}')
print('\nT3 worker turns: model time and cost by step (landed tasks)')
by_step = C.defaultdict(lambda: [0, 0, 0, 0])  # turns, ms, cost, tasks
for t in L:
    seen = set()
    for step, trig, status, model, cost, ms, ctx in t['turns']:
        by_step[step][0] += 1; by_step[step][1] += ms; by_step[step][2] += cost
        if step not in seen:
            seen.add(step); by_step[step][3] += 1
for step, (turns, ms, cost, k) in sorted(by_step.items(), key=lambda i: -i[1][2]):
    print(f'  {step:14} turns {turns:4} tasks {k:3}  model-min {MIN(ms):>7}  USD {USD(cost):>7}  USD/task {USD(cost / max(k, 1)):>6}')
trig = C.Counter(); trig_cost = C.Counter()
for t in L:
    for step, tr, status, model, cost, ms, ctx in t['turns']:
        trig[tr] += 1; trig_cost[tr] += cost
print('  turns by trigger:', ', '.join(f'{k} {v} (${USD(c)})' for k, v in trig.most_common() for c in [trig_cost[k]]))
print('\nT4 cost per landed task (USD)')
tc = lambda t: sum(v for k, v in t['cost'].items() if not k.startswith('manager'))
stat('worker, all', [tc(t) for t in L], USD); stat('worker, small', [tc(t) for t in small], USD); stat('worker, full', [tc(t) for t in full], USD)
stat('manager review share', [t['cost']['manager:review'] for t in L], USD); stat('manager other share', [t['cost']['manager:other'] for t in L], USD)
stat('pings', [t['cost']['ping'] for t in L], USD)
tok = C.Counter()
for t in L:
    tok.update(t['tokens'])
cached = tok['cache_read_tokens'] / max(1, tok['cache_read_tokens'] + tok['cache_creation_tokens'] + tok['input_tokens'])
print(f'  tokens M: cache read {tok["cache_read_tokens"] / 1e6:.0f}, cache write {tok["cache_creation_tokens"] / 1e6:.1f}, input {tok["input_tokens"] / 1e6:.3f}, output {tok["output_tokens"] / 1e6:.1f}; cached share {100 * cached:.1f}%')
by_model = C.Counter()
for t in L:
    for step, tr, status, model, cost, ms, ctx in t['turns']:
        by_model[model] += cost
print('  worker USD by model:', {k: USD(v) for k, v in by_model.items()}, ' manager USD:', {k: USD(v) for k, v in mcost.items()}, 'manager turns', dict(mkind))
rr = [(sum(v for v in t['reads'].values()), sum(v - 1 for v in t['reads'].values() if v > 1)) for t in L]
print(f'  Read calls {sum(r[0] for r in rr)}, repeated reads of a path already read in the task {sum(r[1] for r in rr)} ({100 * sum(r[1] for r in rr) / max(1, sum(r[0] for r in rr)):.0f}%); rotations {sum(t["rotations"] for t in L)}, pings {sum(t["pings"] for t in L)}')
stat('model calls a task', [t['calls'] for t in L], int); stat('worker turns a task', [len(t['turns']) for t in L], int)
for name, g in (('small', small), ('full', full)):
    print(f'  {name:5}: sent back {sum(t["sent_back"] > 0 for t in g)}/{len(g)} tasks, check-runs/task p50 {pct([len(t["check_runs"]) for t in g], .5)}, failed check-runs/task p50 {pct([sum(bool(c["failed"]) for c in t["check_runs"]) for t in g], .5)}, plan blocked {sum(any(s == "plan" and st == "blocked" for s, _, st, *_ in t["turns"]) for t in g)}')
print('\nT5 checks and check-runs (landed tasks)')
cr = [c for t in L for c in t['check_runs']]
print(f'  check-runs {len(cr)} in {sum(bool(t["check_runs"]) for t in L)} tasks, p50 {pct([len(t["check_runs"]) for t in L if t["check_runs"]], .5)} max {max((len(t["check_runs"]) for t in L), default=0)} a task; with a failure {sum(bool(c["failed"]) for c in cr)}')
stat('check-run local queue', [c['queue'] for c in cr if c['local']]); stat('check-run local run', [c['local'] for c in cr if c['local']]); stat('check-run CI wall', [c['ci'] for c in cr if c['ci']])
ck = [c for t in L for c in t['checks']]
stat('host checks queue', [c['queue'] for c in ck]); stat('host checks run', [c['run'] for c in ck])
print('  after waits by reason:', dict(C.Counter(t.get('after_because') for t in L if t['phase']['after'] > 0)), 'min', {b: MIN(sum(t['phase']['after'] for t in L if t.get('after_because') == b)) for b in ('needs', 'files', 'check')})
print(f'  host check runs {len(ck)}, failed {sum(not c["ok"] for c in ck)}, in a land {sum(c["phase"] == "land" for c in ck)}; revisions p50 {pct([t["revisions"] for t in L], .5)} max {max(t["revisions"] for t in L)}; sent back {sum(t["sent_back"] for t in L)} in {sum(t["sent_back"] > 0 for t in L)} tasks; conflicts {sum(t["conflicts"] for t in L)}')
ci = [c for t in L for c in t['ci_runs']]
fails = C.Counter((c['kind'], c['ok']) for c in ci)
print(f'  CI runs {len(ci)}: ' + ', '.join(f'{k} {"pass" if ok else "fail"} {v}' for (k, ok), v in sorted(fails.items())) + f'; GH queue >= 5 min (from the output) {sum(c["queued_min"] >= 5 for c in ci)}')
passed = {(c['kind'], c['commit'], c['at']) for c in ci if c['ok'] and c['commit']}
flakes = [c for c in ci if not c['ok'] and c['commit'] and any(k == c['kind'] and cm == c['commit'] and at > c['at'] for k, cm, at in passed)]
keys = C.defaultdict(list)
for c in ci:
    if c['key']:
        keys[c['key']].append(c)
shared = [c for g in keys.values() if len({c['task'] for c in g}) > 1 for c in g]
print(f'  CI wall min: all {MIN(sum(c["ms"] for c in ci))}, failed runs {MIN(sum(c["ms"] for c in ci if not c["ok"]))}, '
      f'flakes (the same commit passed later) {len(flakes)} runs {MIN(sum(c["ms"] for c in flakes))} min, '
      f'failures whose assert also failed in another task {len(shared)} runs {MIN(sum(c["ms"] for c in shared))} min')
by_cause = C.defaultdict(list)
for c in ci:
    if not c['ok']:
        by_cause[c['cause']].append(c)
print('  failed CI runs by cause (runs, tasks, wall min, worker turns they started):')
for cause, g in sorted(by_cause.items(), key=lambda i: -sum(c['ms'] for c in i[1])):
    print(f'    {cause:13} {len(g):3} runs {len({c["task"] for c in g}):2} tasks {MIN(sum(c["ms"] for c in g)):>7} min')
print('  other causes:', [k[:60] for k, g in keys.items() if g[0]['cause'] == 'other'][:6])
keys = {k: g for k, g in keys.items() if g[0]['cause'] in ('assert', 'timeout')}
for key, g in sorted(keys.items(), key=lambda i: -len(i[1]))[:8]:
    print(f'    {len(g):3} runs {len({c["task"] for c in g}):2} tasks {g[0]["kind"]:4} {key[:84]}')
for kind in ('task', 'web'):
    rows = [r for r in gh_rows if r['kind'] == kind]
    if rows:
        stat(f'CI {kind} GH queue', [r['queue'] for r in rows]); stat(f'CI {kind} longest job', [r['run'] for r in rows]); stat(f'CI {kind} wall seen by the worker', [r['wall'] for r in rows])
        stat(f'CI {kind} failed runs wall', [r['wall'] for r in rows if not r['ok']])
if cache:
    jobs = [(iso(j['started']), iso(j['done'])) for runs in cache.values() for j in runs if j['started'] and j['done']]
    busy = lambda ts: sum(1 for a, b in jobs if a <= ts < b)
    for lo, hi in ((0, 2), (3, 4), (5, 99)):
        g = [r for r in gh_rows if lo <= busy(r['created']) <= hi]
        if g:
            print(f'  scenario jobs already running when a run was created {lo}-{hi}: runs {len(g):3}  GH queue p50 {MIN(pct([r["queue"] for r in g], .5)):>5}  p90 {MIN(pct([r["queue"] for r in g], .9)):>5} min')
    print(f'  CI job-minutes of the window tasks: {MIN(sum(iso(j["done"]) - iso(j["started"]) for t in L for c in t["ci_runs"] for j in cache.get(str(c["run"]), []) if j["started"] and j["done"]))}')
print('\nT6 manager review wait after ready (min) and concurrency')
stat('ready to land or add', [w for t in L for w in t['mwait']])
rq = []  # ready event -> delivered
for e in MLOG:
    if e['kind'] == 'message' and e.get('from') == 'events' and e['unix_ms'] >= SINCE and re.match(r'#\d+ ready', e['text']) and e['id'] in delivered_at:
        rq.append(delivered_at[e['id']] - e['unix_ms'])
stat('ready event to its delivery', rq)
stat('manager turn (delivered to reply)', [r - d for d, r, u, named, kinds in mturns if 'ready' in kinds])
def open_at(ts):
    return sum(1 for x in T if x['created'] <= ts and (x['landed'] or 1e18) > ts)
conc = [open_at(t['ready']) for t in L if t['ready']]
print(f'  open tasks when a task got ready: p50 {pct(conc, .5)} p90 {pct(conc, .9)} max {max(conc, default=0)}')
for lo, hi in ((1, 2), (3, 4), (5, 99)):
    g = [t for t in L if t['ready'] and lo <= open_at(t['ready']) <= hi]
    if g:
        print(f'  {lo}-{hi} open: tasks {len(g):3}  created-to-landed p50 {MIN(pct([t["landed"] - t["created"] for t in g], .5)):>6}  manager-wait p50 {MIN(pct([t["phase"]["manager-wait"] for t in g], .5)):>5}  check-run p50 {MIN(pct([t["phase"]["check-run"] for t in g], .5)):>5}  checks p50 {MIN(pct([t["phase"]["checks"] for t in g], .5)):>5}  limit sum {MIN(sum(t["phase"]["limit"] for t in g)):>6}')
dep = [e for e in events(os.path.join(STATE, 'deploys.jsonl')) if e['unix_ms'] >= SINCE and e.get('kind') == 'deploy']
stat('deploys', [e['duration_ms'] for e in dep])
print('\nT7 per task (landed): id size min phases(start/steps/check-run/checks/manager/land/limit) worker$ mgr$ revs sent-back check-runs')
for t in sorted(L, key=lambda t: -(t['landed'] - t['created']))[:int(arg('--top', '12'))]:
    p = t['phase']; steps = sum(v for k, v in p.items() if k.startswith('step:'))
    start = p['after'] + p['disk'] + p['hold'] + p['owner']
    print(f'  #{t["id"]} {"S" if t["rec"].get("small") else "F"} {MIN(t["landed"] - t["created"]):>6} '
          f'{MIN(start):>5}/{MIN(steps):>5}/{MIN(p["check-run"]):>5}/{MIN(p["checks"]):>5}/{MIN(p["manager-wait"]):>5}/{MIN(p["land"]):>5}/{MIN(p["limit"]):>5} '
          f'${USD(tc(t)):>5} ${USD(t["cost"]["manager:review"] + t["cost"]["manager:other"]):>5} r{t["revisions"]} sb{t["sent_back"]} cr{len(t["check_runs"])}')
