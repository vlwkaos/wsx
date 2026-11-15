#!/usr/bin/env python3
"""Web scenario failures: one row for each failed `web` check run, grouped by the assert that failed.

ref: docs/design/scenario-flakes.md (the numbers and the cause)

    python3 -I docs/design/scenario-flakes-measure.py [--until 2026-11-15T10:00] [--days 21] [--gh] [--load] [--rows] [--state ~/.local/state/wsx/tasks/wsx-8cbe3f08]
        [--yield] [--commits] [--asserts]   (docs/design/verification-bottleneck.md)

Reads the `check_run` and `checks` entries of every task log whose command runs `ci-scenario.py web`.
A run's CI id is the last `actions/runs/<id>` its output names. `--gh` fetches that run once
(`gh api`) into .work/scenario-flakes/runs.json: the scenario commit's message names the task's HEAD
("wsx scenario web for <sha>"). The HEAD's diff from its fork point with main says whether the
task wrote or changed the failing assert. A same-tree rerun is a later web run of the same part on a HEAD with the same git
tree. `--load` fetches the job logs of passing runs to read their `wsxd watch` load samples
(test-web-client.py prints them at exit) into .work/scenario-flakes/loads.json. Read-only: it changes no
task, branch or run.
"""
import collections as C, glob, json, os, re, statistics, subprocess, sys, time

ARGS = sys.argv[1:]
def arg(name, default):
    at = ARGS.index(name) + 1 if name in ARGS else len(ARGS)
    return ARGS[at] if at < len(ARGS) and not ARGS[at].startswith('--') else default
STATE = os.path.expanduser(arg('--state', '~/.local/state/wsx/tasks/wsx-8cbe3f08'))
UNTIL = time.mktime(time.strptime(arg('--until', time.strftime('%Y-%m-%dT%H:%M')), '%Y-%m-%dT%H:%M')) * 1000  # ^ local time
SINCE = UNTIL - float(arg('--days', '21')) * 86400000
ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))
CACHE = os.path.join(ROOT, '.work', 'scenario-flakes')
REPO = 'vlwkaos/wsx'
COMMAND = re.compile(r'ci-scenario\.py web(\s|$)')


# The groups, read from the check code (docs/design/scenario-flakes.md "Shared groups and the first wrong state"): pattern -> (group, kind).
#   timing:  the check judges a value read at a moment (after a fixed pause, a fixed frame window, or a wait
#            on another part of the state), not a wait on the value it judges
#   ranout:  the check waits on the value it judges and its bound passed (the state never came)
#   main:    a break on main every task based on it inherits (deterministic on one tree)
#   product: a product bug the check found (or, with a hypothesis in the doc, a likely one)
#   input:   the browser did not get the synthesized input
GROUPS = [
    (r'the chat moved without the reader', 'followEnd jump', 'product'),
    (r'does not open in one motion', 'To check sheet motion', 'product'),  # ^ frames step back: 844,127,563
    (r'motion at \d+px|typing dots', 'typing dots motion', 'timing'),
    (r'since.you.left line|since line', 'since line', 'timing'),
    (r'Jump to latest did not show', 'Jump to latest', 'timing'),
    (r'a slide did not move the row', 'Inbox slide', 'input'),  # ^ synthesized gesture; touch events since #291
    (r'the working chat is wrong', 'working chat state', 'timing'),
    (r'wide blocks do not use', 'wide blocks', 'timing'),
    (r'elapsed time does not count|no age timer', 'age timer', 'timing'),
    (r'key row does not start at Esc', 'key row', 'timing'),
    (r"assert self\.frame\(\)", 'terminal stream frame', 'timing'),
    (r'card pictures did not load', 'panel card pictures', 'ranout'),
    (r'no reply after the question', 'no reply after question', 'ranout'),
    (r'manager_push\(\)', 'push badge', 'main'),
    (r'full_reads\(\) == reads', 'unchanged stamp reads', 'main'),
    (r"held_pdf", 'held_pdf NameError', 'main'),
    (r'assert literals == \[\]', 'CSS literals', 'main'),
    (r'design lint found', 'design lint', 'lint'),
]

GENERIC = {'design lint', 'uncaught exception', 'console errors'}

def events(path):
    for line in open(path, errors='replace'):
        if 'ci-scenario.py web' in line:
            try:
                yield json.loads(line)
            except ValueError:
                pass

def git(*cmd):
    out = subprocess.run(['git', '-C', ROOT, *cmd], capture_output=True, text=True)
    return out.stdout.strip() if out.returncode == 0 else None

def load_cache(name):
    path = os.path.join(CACHE, name)
    return json.load(open(path)) if os.path.exists(path) else {}

def save_cache(name, data):
    os.makedirs(CACHE, exist_ok=True)
    tmp = os.path.join(CACHE, name + '.tmp')
    json.dump(data, open(tmp, 'w'))
    os.replace(tmp, os.path.join(CACHE, name))  # ^ After every call: a session end kills a long fetch.

def gh(*cmd):
    out = subprocess.run(['gh', *cmd], capture_output=True, text=True)
    return out.stdout if out.returncode == 0 else None

def tail(out):
    """The cause line of a run with no FAIL line: the last error-looking line before the closing lines."""
    lines = [re.sub(r'^.*?\t\S+Z ', '', l) for l in out.split("== main's latest CI run")[0].splitlines()]
    hits = [l for l in lines if re.search(r'Error|error:|assert|cancelled|timed out|Traceback', l) and 'gh calls' not in l]
    return (hits or lines or [''])[-1].strip()

def loads_of(text):
    """The load figures of the `wsxd watch` line (the 5 worst-stall samples of a run)."""
    m = re.search(r'wsxd watch \(.*?\): \[(.*?)\] of', text or '')
    return [float(x) for x in re.findall(r", ([\d.]+), '\d\d:\d\d:\d\d'\)", m.group(1))] if m else []

def fragment(text):
    """The first static words of a FAIL message or an assert line, as the check's source writes them."""
    text = re.sub(r'^(AssertionError|NameError|TypeError): ', '', text)
    text = re.sub(r'^(\d+ ?px|[\w-]+ at \d+x\d+|To check on [\w ]+|#\d+\'s sheet|motion at \d+px): ', '', text)
    return re.split(r'[:;{\[(]| \d', text)[0].strip()[:40] if not text.startswith('assert ') else text[:60]

def message(out):
    """(what failed, file or function): the last FAIL line, else the failing assert of test-web-client.py, else None."""
    out = out.split("== main's latest CI run")[0]
    fails = re.findall(r'FAIL: (.*)$', out, re.M)
    if fails:
        return fails[-1].strip(), None
    asserts = re.findall(r'test-web-client\.py", line \d+, in (\w+)\n(?:.*\t\S+Z )?\s*(assert .*)', out)
    if asserts:
        return asserts[-1][1].strip(), 'test-web-client.py ' + asserts[-1][0]
    errors = re.findall(r'^(?:.*\t\S+Z )?((?:AssertionError|NameError|TypeError): .*)$', out, re.M)
    return (errors[-1].strip(), 'test-web-client.py') if errors else (None, None)

SOURCES = {f: open(os.path.join(ROOT, f), errors='replace').read().splitlines()
           for f in [os.path.relpath(g, ROOT) for g in glob.glob(os.path.join(ROOT, 'scripts', 'web-*.mjs'))] + ['scripts/test-web-client.py']}
def where(text):
    """file:line of the assert in this checkout, by its first words (None when it is gone or reworded)."""
    words = fragment(text)
    for size in range(len(words), 11, -1):
        hits = ['%s:%d' % (f, n + 1) for f, lines in SOURCES.items() for n, line in enumerate(lines) if words[:size] in line]
        if hits:
            return hits[0]
    return None

# 1. The web check runs.
runs = []
for path in sorted(glob.glob(os.path.join(STATE, '*', 'log.jsonl'))):
    task = os.path.basename(os.path.dirname(path))
    for e in events(path):
        if e.get('kind') not in ('check_run', 'checks') or not SINCE <= e.get('unix_ms', 0) < UNTIL:
            continue
        for r in e.get('results', []):
            if not COMMAND.search(r.get('command', '')):
                continue
            out = r.get('output', '')
            part = re.search(r"WSX_SCENARIO_PART='(\w+)'|--part (\w+)", r['command'])
            ids = re.findall(r'actions/runs/(\d+)', out.split("== main's latest CI run")[0])
            text, fn = message(out)
            runs.append(dict(task=task, ms=e['unix_ms'], exit=r.get('exit'), part=(part and (part.group(1) or part.group(2))) or 'all',
                             run=ids[-1] if ids else None, text=text, fn=fn, out=out, dur=r.get('ms') or 0, log=path))
failed = [r for r in runs if r['exit'] not in (0, None)]

# 2. Each CI run's scenario commit names the task's HEAD; its diff from the fork point with main.
meta = load_cache('runs.json')
if '--gh' in ARGS:
    for r in runs:
        if r['run'] and r['run'] not in meta:
            got = gh('api', 'repos/%s/actions/runs/%s' % (REPO, r['run']), '--jq',
                     '{message: .head_commit.message}')
            if got:
                meta[r['run']] = json.loads(got)
                save_cache('runs.json', meta)
for r in runs:
    m = re.search(r'for ([0-9a-f]{40})', (meta.get(r['run'] or '') or {}).get('message', ''))
    r['head'] = m and m.group(1)
    r['tree'] = r['head'] and git('rev-parse', r['head'] + '^{tree}')
    r['base'] = r['tree'] and git('merge-base', r['head'], 'main')  # ^ local main: origin/main moves only with a release
    r['diff'] = r['base'] and (git('diff', '-U0', r['base'], r['head'], '--', 'scripts', 'crates/wsx-web/web') or '')

# 3. Each failure's class.
#   infra:  no assert failed (a GitHub or runner failure, a queue or run deadline, a cancel)
#   own:    the task's diff adds or changes a line with the failing assert's words
#   generic: a message that names no assert (design lint, an uncaught exception, console errors)
#   shared: the same assert failed in 2 or more tasks that did not change it
#   once:   one task, which did not change the assert
for r in failed:
    r['group'] = next((name for pattern, name, _ in GROUPS if r['text'] and re.search(pattern, r['text'])), None) or (r['text'] and fragment(r['text']))
    r['at'] = r['fn'] or (r['text'] and where(r['text']))
    r['own'] = None if r['text'] is None or r['diff'] is None else any(
        l[:1] in '+-' and not l.startswith(('+++', '---')) and fragment(r['text']) in l for l in r['diff'].splitlines())
tasks_of = C.defaultdict(set)
for r in failed:
    if r['text'] and not r['own']:
        tasks_of[r['group']].add(r['task'])
for r in failed:
    r['class'] = ('infra' if r['text'] is None else 'own' if r['own'] else 'generic' if r['group'] in GENERIC
                  else 'shared' if len(tasks_of[r['group']]) > 1 else 'once')
    same = [o for o in runs if o is not r and o['tree'] and o['tree'] == r['tree'] and o['part'] in (r['part'], 'all') and o['ms'] > r['ms']]
    r['same_pass'] = any(o['exit'] == 0 for o in same) if same else None  # a same-tree rerun passed: timing, not the tree
    r['same_fail'] = any(o['exit'] not in (0, None) and o['text'] == r['text'] for o in same) if same else None  # it failed again: the tree

def ratio(values):
    return '%d/%d' % (sum(1 for v in values if v), sum(1 for v in values if v is not None))

day = [r for r in runs if r['ms'] > UNTIL - 86400000]
print('web check runs since %s: %d, failed %d; CI run named %d, task HEAD known %d. Last 24 h: %d runs, %d failed'
      % (time.strftime('%Y-%m-%d', time.localtime(SINCE / 1000)), len(runs), len(failed), sum(1 for r in runs if r['run']),
         sum(1 for r in runs if r['head']), len(day), sum(1 for r in day if r['exit'] not in (0, None))))
print('\nclass   fails  24 h  tasks  same tree passed  same tree failed again')
for name in ('own', 'shared', 'once', 'generic', 'infra'):
    rows = [r for r in failed if r['class'] == name]
    print('%-7s %5d  %4d  %5d  %16s  %22s' % (name, len(rows), sum(1 for r in rows if r in day), len({r['task'] for r in rows}),
                                               ratio([r['same_pass'] for r in rows]), ratio([r['same_fail'] for r in rows])))
infra = C.Counter(re.sub(r'\d+', 'N', tail(r['out']))[:70] for r in failed if r['class'] == 'infra')
print('\ninfra causes:', '; '.join('%d %s' % (n, k) for k, n in infra.most_common(8)))

print('\nshared groups (failed in 2+ tasks that did not change the assert):')
print('%-34s %-7s %5s %5s %6s %6s  %-11s  %-11s  %s' % ('group', 'kind', 'fails', 'tasks', 'pass=', 'fail=', 'first', 'last', 'assert'))
kinds = {name: kind for _, name, kind in GROUPS}
shared = C.defaultdict(list)
for r in failed:
    if r['class'] == 'shared':
        shared[r['group']].append(r)
for group, rows in sorted(shared.items(), key=lambda kv: -len(kv[1])):
    stamp = lambda ms: time.strftime('%m-%d %H:%M', time.localtime(ms / 1000))
    print('%-34s %-7s %5d %5d %6s %6s  %-11s  %-11s  %s' % (group[:34], kinds.get(group, '?'), len(rows), len({r['task'] for r in rows}),
                                                         ratio([r['same_pass'] for r in rows]), ratio([r['same_fail'] for r in rows]),
                                                         stamp(min(r['ms'] for r in rows)), stamp(max(r['ms'] for r in rows)), rows[0]['at']))
OPEN = UNTIL - 3 * 86400000
print('\nshared by kind   fails  tasks  last 3 days')
for kind in ('timing', 'main', 'product', 'ranout', 'input', '?'):
    rows = [r for r in failed if r['class'] == 'shared' and kinds.get(r['group'], '?') == kind]
    print('%-15s %6d %6d %12d' % (kind, len(rows), len({r['task'] for r in rows}), sum(1 for r in rows if r['ms'] > OPEN)))
timing = [r for r in failed if r['class'] == 'shared' and kinds.get(r['group']) == 'timing']

# 4. Runner load: failing runs (their own output) against passing runs (their job logs, --load).
loads = load_cache('loads.json')
if '--load' in ARGS:
    for r in runs:
        if r['exit'] == 0 and r['run'] and r['run'] not in loads:
            jobs = gh('api', 'repos/%s/actions/runs/%s/jobs' % (REPO, r['run']), '--jq', '[.jobs[] | select(.name | startswith("web scenario")) | .id]')
            seen = []
            for job in json.loads(jobs or '[]'):
                log = gh('run', 'view', '--repo', REPO, '--job', str(job), '--log') or ''
                watch = [line for line in log.splitlines() if 'wsxd watch' in line]
                seen += loads_of(watch[-1]) if watch else []  # ^ the last line, as a failed run's kept output has it
            loads[r['run']] = seen
            save_cache('loads.json', loads)
def load_line(name, values):
    if values:
        print('load at the 5 worst wsxd stalls, %s: n %d, median %.1f, p10 %.1f, p90 %.1f'
              % (name, len(values), statistics.median(values), sorted(values)[len(values) // 10], sorted(values)[len(values) * 9 // 10]))
print()
load_line('shared timing failures', [statistics.median(v) for v in (loads_of(r['out']) for r in timing) if v])
load_line('other assert failures', [statistics.median(v) for v in (loads_of(r['out']) for r in failed if r['text'] and r not in timing) if v])
load_line('passing runs', [statistics.median(v) for v in loads.values() if v])

if '--rows' in ARGS:
    print()
    for r in sorted(failed, key=lambda r: r['ms']):
        if arg('--rows', '') in '%s %s' % (r['class'], r['group']):
            print('%s #%-4s %-5s %-6s head %-8s pass= %-5s fail= %-5s %-28s %s' % (
                time.strftime('%m-%d %H:%M', time.localtime(r['ms'] / 1000)), r['task'], r['part'], r['class'], (r['head'] or '')[:8],
                r['same_pass'], r['same_fail'], (r['at'] or '')[:28], (r['text'] or tail(r['out']))[:110]))

# 5. Yield (--yield): what the task's next attempt changed after each failure, read from the git trees.
#    A failure's fix is the set of files whose patch (against each attempt's own fork point, so a rebase
#    compares equal) differs between the failing HEAD and the task's next web run on another tree.
#    product: only crates/ changed (the scenario found a product bug, an upper bound: other work may ride along)
#    script:  only scripts/ changed (the check lagged an intended change, or a timing read was fixed)
#    both, neither (a rebase or docs only), none (no later web run on another tree: landed, closed or rerun only)
#    The worker turn that read the failure is the task's first `check_run` turn after it.
ASSERT_LINE = re.compile(r'fail\(|^\s*assert |until\(')
KINDS = [('motion', r'animation|transition|motion|frame|getAnimations|moved without|one motion|slide|followEnd|jump'),
         ('layout', r'\bpx\b|\d+ ?px|getBoundingClientRect|\brect\b|offset(Top|Height|Width|Left)|client(Height|Width)|scroll(Top|Height)'
                    r'|\bwidth\b|\bheight\b|\btop\b|\bbottom\b|\bleft\b|\bright\b|overlap|\bgap\b|align|\bwraps?\b|\bfits?\b|clip|contentShare'),
         ('style', r'getComputedStyle|color|font|rgb|var\(--|className|classList|cursor|opacity|literals|design lint'),
         ('words', r"textContent|innerText|=== '|== '|!== '|\.includes\(|\.startsWith\(|\btext\b")]
def kind_of(text):
    """motion, layout, style or words: the first whose words the assert (and the 2 lines before it) uses; else behavior."""
    return next((k for k, rx in KINDS if re.search(rx, text or '')), 'behavior')
def window(at):
    if not at or ':' not in at:
        return ''
    f, n = at.rsplit(':', 1)
    return '\n'.join(SOURCES.get(f, [])[max(0, int(n) - 3):int(n)]) if n.isdigit() else ''
patch_cache = {}
def patches(head):
    """file -> the added and removed lines of the task's change at head (against its fork point with main)."""
    if head not in patch_cache:
        base = git('merge-base', head, 'main')
        files = {}
        for block in re.split(r'^diff --git ', (base and git('diff', '-U0', base, head)) or '', flags=re.M)[1:]:
            lines = block.splitlines()
            files[lines[0].split(' b/')[-1]] = '\n'.join(l for l in lines[1:] if l[:1] in '+-' and not l.startswith(('+++', '---')))
        patch_cache[head] = files
    return patch_cache[head]
results_of = {}
def results(task, path):
    if task not in results_of:
        rows = []
        for line in open(path, errors='replace'):
            if '"kind":"result"' in line:
                try:
                    rows.append(json.loads(line))
                except ValueError:
                    pass
        results_of[task] = rows
    return results_of[task]
if '--yield' in ARGS or '--asserts' in ARGS:
    used = set()
    for r in sorted(failed, key=lambda r: r['ms']):
        turn = next((e for e in results(r['task'], r['log']) if e['unix_ms'] > r['ms'] and e.get('trigger') == 'check_run' and id(e) not in used), None)
        if turn:
            used.add(id(turn))
        r['turn_usd'] = turn['usage']['cost_micro_usd'] / 1e6 if turn else 0
        r['said'] = (turn or {}).get('summary', '')[:150].replace('\n', ' ')
        nxt = next((o for o in sorted(runs, key=lambda o: o['ms']) if o['task'] == r['task'] and o['ms'] > r['ms'] and o['head'] and o['tree'] != r['tree']), None)
        if not r['head'] or not nxt or git('cat-file', '-e', r['head']) is None or git('cat-file', '-e', nxt['head']) is None:  # ^ '' on success
            r['fix'], r['then'] = 'none', None
            continue
        a, b = patches(r['head']), patches(nxt['head'])
        changed = [f for f in set(a) | set(b) if a.get(f) != b.get(f)]
        s, p = any(f.startswith('scripts/') for f in changed), any(f.startswith('crates/') for f in changed)
        r['fix'] = 'both' if s and p else 'script' if s else 'product' if p else 'neither'
        r['then'] = 'pass' if nxt['exit'] == 0 else 'same' if nxt['text'] == r['text'] else 'other'
        r['kind'] = kind_of((r['text'] or '') + '\n' + window(r['at']))
    print('\nyield: the fix of each failed web run (the next attempt on another tree), CI wall min and the worker turn that read it')
    print('%-8s %-8s %5s %7s %8s   then pass/same/other' % ('class', 'fix', 'runs', 'CI min', 'turn USD'))
    for cls in ('own', 'once', 'shared', 'generic', 'infra'):
        for fix in ('product', 'script', 'both', 'neither', 'none'):
            rows = [r for r in failed if r['class'] == cls and r['fix'] == fix]
            if rows:
                then = C.Counter(r['then'] for r in rows)
                print('%-8s %-8s %5d %7.0f %8.2f   %d/%d/%d' % (cls, fix, len(rows), sum(r['dur'] for r in rows) / 60000, sum(r['turn_usd'] for r in rows),
                                                          then['pass'], then['same'], then['other']))
    total = lambda rows: '%d runs, %.0f CI min, $%.2f in %d turns' % (len(rows), sum(r['dur'] for r in rows) / 60000, sum(r['turn_usd'] for r in rows), sum(1 for r in rows if r['turn_usd']))
    print('all failed:', total(failed))
    print('fixed in product only:', total([r for r in failed if r['fix'] == 'product']))
    print('fixed in the scripts only:', total([r for r in failed if r['fix'] == 'script']))
    print('\nkind of the failing assert x fix   product  script  both  neither  none')
    for k in [k for k, _ in KINDS] + ['behavior']:
        c = C.Counter(r['fix'] for r in failed if r['class'] != 'infra' and r.get('kind', kind_of((r['text'] or '') + '\n' + window(r['at']))) == k)
        print('%-34s %7d %7d %5d %8d %5d' % (k, c['product'], c['script'], c['both'], c['neither'], c['none']))
    if '--rows' in ARGS and arg('--rows', '') in 'yield':
        for r in sorted(failed, key=lambda r: r['ms']):
            if r['fix'] in ('product', 'both'):
                print('%s #%-4s %-6s %-7s %-6s %-8s %-60s | %s' % (time.strftime('%m-%d %H:%M', time.localtime(r['ms'] / 1000)), r['task'], r['class'], r['fix'],
                                                                 r['then'], r.get('kind'), (r['text'] or '')[:60], r['said']))

# 6. Commits (--commits): main's commits in the window that touch the web scripts, and the ones that touch
#    only them (plus docs and CHANGELOG), each with the task that landed it (its log's last revision base
#    to its landed commit, matched by author date and subject, which a land's rebase keeps).
#    timing:   the subject says wait, until, load or another timing word
#    coverage: it adds at least 2 more assert lines (fail(, assert, until() than it removes
#    adapt:    its task changed crates/ too, or the subject says since, now, not, instead, renamed or moved
#    other:    none of these
TIMING = re.compile(r'\b(wait|waits|waited|until|load|lease|settle|stall|timing|flak\w*|race|retry|deadline|timeout|pause|in one step|at once|stays?|later|polls?|longer)\b', re.I)
ADAPT = re.compile(r'\b(since|now|not|instead|renam\w*|moved|no longer|follows?|expects?)\b', re.I)
if '--commits' in ARGS:
    span = ['--since', time.strftime('%Y-%m-%dT%H:%M', time.localtime(SINCE / 1000)), '--until', time.strftime('%Y-%m-%dT%H:%M', time.localtime(UNTIL / 1000)), 'main']
    WEB = ['scripts/web-*.mjs', 'scripts/test-web-client.py']
    only = re.compile(r'scripts/web-[^/]*\.mjs$|scripts/test-web-client\.py$|docs/|CHANGELOG\.md$')
    task_of, product_of = {}, {}
    for path in glob.glob(os.path.join(STATE, '[0-9]*', 'log.jsonl')):
        task, base, landed = os.path.basename(os.path.dirname(path)), None, None
        for line in open(path, errors='replace'):
            if '"kind":"revision"' in line or '"kind":"landed"' in line:
                e = json.loads(line)
                base, landed = (e.get('base'), landed) if e['kind'] == 'revision' else (base, e.get('commit'))
        if base and landed and git('cat-file', '-e', landed) is not None:
            for row in (git('log', '--format=%ad%x00%s', base + '..' + landed) or '').splitlines():
                task_of[row] = task
            product_of[task] = any(f.startswith('crates/') for f in (git('diff', '--name-only', base, landed) or '').split())
    all_n = len((git('log', '--format=%H', *span) or '').split())
    web = (git('log', '--format=%H', *span, '--', *WEB) or '').split()
    rows = []
    for h in web:
        files = (git('show', '--format=', '--name-only', h) or '').split()
        if not all(only.match(f) for f in files):
            continue
        subject = git('log', '-1', '--format=%s', h); key = git('log', '-1', '--format=%ad%x00%s', h)
        diff = (git('show', '-U0', '--format=', h, '--', 'scripts') or '').splitlines()
        added = sum(1 for l in diff if l.startswith('+') and not l.startswith('+++') and ASSERT_LINE.search(l[1:]))
        removed = sum(1 for l in diff if l.startswith('-') and not l.startswith('---') and ASSERT_LINE.search(l[1:]))
        task = task_of.get(key)
        cls = ('timing' if TIMING.search(subject) else 'coverage' if added - removed >= 2
               else 'adapt' if (task and product_of.get(task)) or ADAPT.search(subject) else 'other')
        rows.append((cls, task, h, subject, added, removed, sum(1 for l in diff if l[:1] in '+-')))
    print('\ncommits on main in the window %d; touching the web scripts %d; only them (plus docs, CHANGELOG) %d; of those with a task %d'
          % (all_n, len(web), len(rows), sum(1 for r in rows if r[1])))
    for cls, n in C.Counter(r[0] for r in rows).most_common():
        g = [r for r in rows if r[0] == cls]
        print('  %-9s %4d commits, %5d lines changed, in %d tasks' % (cls, n, sum(r[6] for r in g), len({r[1] for r in g if r[1]})))
    script_tasks = sorted({r[1] for r in rows if r[1] and not product_of.get(r[1])}, key=int)
    print('  tasks that landed script-only web commits and no crates/ change: %d' % len(script_tasks))
    if '--rows' in ARGS and arg('--rows', '') in 'commits':
        for r in rows:
            print('  %-8s #%-4s %s +%d/-%d asserts  %s' % (r[0], r[1] or '?', r[2][:8], r[4], r[5], r[3][:110]))

# 7. Asserts (--asserts): every fail( of scripts/web-*.mjs and assert of test-web-client.py, by the kind of value it judges.
if '--asserts' in ARGS:
    print('\nasserts by kind (the line and the 2 before it)')
    count = C.Counter()
    for f, lines in SOURCES.items():
        for n, line in enumerate(lines):
            if re.search(r'\bfail\(' if f.endswith('.mjs') else r'^\s*assert ', line) and 'function fail' not in line:
                count[kind_of('\n'.join(lines[max(0, n - 2):n + 1]))] += 1
    print('  ' + ', '.join('%s %d' % kv for kv in count.most_common()) + '; all %d' % sum(count.values()))
