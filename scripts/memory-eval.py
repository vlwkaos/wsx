#!/usr/bin/env python3
"""Replay evaluation of the memory system: docs/design/memory-eval.md (method, verdict rule).

  pick     choose the tasks from the real task store         -> .work/memory-eval/picks.json
  run      replay the plan step of each pick, one arm        -> .work/memory-eval/runs/<arm>/<task>-<rep>.json
  judge    score every run blind (cause, traps)              -> .work/memory-eval/judged.json
  report   per-task tables, fidelity, noise, verdict inputs  -> stdout (markdown)
  live     before/after comparison from the real logs (confounded: different tasks)
  offline  free score of the start set on every landed task, from the logs and the git history (no model call)
  recall-cheap  (internal) the shim's `wsx memory recall` with a cheap model

Arms: baseline (what a worker had at the task's start), memory (+ start set, `wsx memory` and a store
cut at the task's start), memory-cheap (as memory; `recall` asks a cheap model).
Every run: its own git worktree at the task's base, its own XDG_STATE_HOME with the task store cut at
the task's start; the worktree is the run's project, so a fact is stale against the task's base, not the real
HEAD. The real store, the real memory and the real config are only read.
"""
import argparse
import collections
import fnmatch
import concurrent.futures as cf
import hashlib
import importlib.util
import json
import os
import random
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORK = Path(os.environ.get('MEMORY_EVAL_WORK') or ROOT / '.work' / 'memory-eval')  # ^ the dry run sets its own
PID = 'wsx-8cbe3f08'
REAL = Path(os.environ.get('MEMORY_EVAL_STORE') or Path.home() / '.local/state/wsx/tasks' / PID)
SKILLS = REAL / 'manager' / 'skills-plugin'
FIRST_TASK = 133  # ^ the first task of the store that has a plan step with usage
CAP_USD = 40.0
RUN_SECS = 1200
CHEAP_MODEL = 'haiku'
JUDGE_MODEL = 'sonnet'
# ^ Placeholders for the commands of build steps 4 and 5 (docs/design/memory-system.md). The paid
# memory arm refuses to start unless the wsx given with --wsx answers `--help` to both; rename here
# when the steps land.
CAPTURE_SUB = ['memory', 'capture']
START_SUB = ['memory', 'start']
AREAS = {'crates/wsx-web': 'web', 'crates/wsx-daemon': 'daemon', 'crates/wsx-terminal': 'terminal',
         'crates/wsx-core': 'core', 'crates/wsx/': 'cli_tui'}
QUOTA = {'web': 4, 'daemon': 3, 'terminal': 2, 'core': 4, 'cli_tui': 3, 'scripts_docs': 2}
EXPLORE = {'rg', 'grep', 'sed', 'cat', 'head', 'tail', 'ls', 'find', 'awk', 'wc', 'egrep', 'fgrep'}
SCHEMA = {'type': 'object', 'additionalProperties': False,
          'required': ['status', 'summary', 'decisions', 'evidence', 'human_verify', 'left_open', 'notes', 'memo'],
          'properties': {
              'status': {'type': 'string', 'enum': ['done', 'blocked', 'failed']}, 'summary': {'type': 'string'},
              'decisions': {'type': 'array', 'items': {'type': 'object', 'additionalProperties': False, 'required': ['choice', 'reason'], 'properties': {'choice': {'type': 'string'}, 'reason': {'type': 'string'}}}},
              'evidence': {'type': 'array', 'items': {'type': 'object', 'additionalProperties': False, 'required': ['check', 'proof'], 'properties': {'check': {'type': 'string'}, 'proof': {'type': 'string'}}}},
              'human_verify': {'type': 'array', 'items': {'type': 'object', 'additionalProperties': False, 'required': ['check', 'where', 'capture', 'poster', 'pass_if'], 'properties': {k: {'type': 'string'} for k in ['check', 'where', 'capture', 'poster', 'pass_if']}}},
              'left_open': {'type': 'array', 'items': {'type': 'string'}}, 'notes': {'type': 'string'}, 'memo': {'type': 'string'}}}
PLAN_END = ('Do only this step. When it is done, return status done with what it found or did; the next '
            'step follows. Before the result, wait for or stop each background job you started.')
# ^ crates/wsx/src/task/claude.rs `worker_instructions`, as one worker got it (project checks as of
# today); the replay uses it for both arms, so an old task is replayed with today's instructions.
WORKER = """You are a task worker in a separate working copy on branch {branch}. Nobody reads your terminal: work without asking questions. Build what the prompt asks and commit your changes with git before you finish. Build and test what you change. The owner's skills are yours with the Skill tool as `owner:<name>`: at the start, look at them and follow any whose description fits the work, such as a development workflow, a simplification or a test skill. Follow CLAUDE.md and AGENTS.md. Keep the change lean: reuse what exists, remove what your change makes dead; prefer removing code to adding it. Every model call reads your whole context again, so use few calls: put the searches and reads you already know you need in one message, not one after another, and read line ranges, not whole files. A search across many files goes to a subagent in the foreground (the Agent tool, type Explore, `run_in_background: false`; `model: sonnet`, or `model: haiku` for a plan's code reading), and only its findings, with file:line, come back to you. Never poll: a background job tells you when it ends. Wait for or stop each background job before your result. Prove what you can: each check a command, a test or a capture shows goes to `evidence` with that command or the capture's path, and a number or a cause in the summary names its source. Before you finish a build, put in the result's memo a short, lean memo of durable project knowledge a later task needs, a few lines at most. Leave the memo empty only when you learned none. You run inside wsx's boundary: keep your work in this working copy. When you cannot continue, return status blocked and say what you need. The task runs in steps, one turn each: {steps}. Each turn names its step: do that step only, then return status done as the turn's last call: nothing runs after it. When a step needs a decision that is the owner's, return status blocked with the question; otherwise decide yourself and go on. Write the summary and the report by this rule. The reader sees one screen. Write for that screen: put the answer, the result or the decision first; say each fact once; keep a reply to about one phone screen (25 short lines)."""
MEMORY_BRIEF = ('Project memory: the "Memory start set" above holds records that match your files. For anything else, '
                'run `wsx memory recall "<question>"` (the best records) or `wsx memory search <words>` before you '
                'read many files.')

_lock = threading.Lock()


# ---- the real store --------------------------------------------------------------------------

class Task:
    def __init__(self, n):
        d = REAL / str(n)
        self.n = n
        self.dir = d
        self.rec = json.loads((d / 'task.json').read_text())
        self.raw = (d / 'log.jsonl').read_text().splitlines()
        self.log = [json.loads(x) for x in self.raw]
        self.t0 = next(e['unix_ms'] for e in self.log if e['kind'] == 'created')
        self.landed_ms = next((e['unix_ms'] for e in self.log if e['kind'] == 'landed'), None)
        self.landed_commit = next((e['commit'] for e in self.log if e['kind'] == 'landed'), None)  # ^ what the host's capture stores
        self.closed_ms = next((e['unix_ms'] for e in self.log if e['kind'] == 'closed'), None)
        self.end_ms = self.landed_ms or self.closed_ms
        self.prompt = next((e['text'] for e in self.log if e['kind'] == 'message' and e.get('mode') == 'prompt'), '')
        self.steps = self.rec.get('steps') or []
        res = d / 'result.json'
        self.result = (json.loads(res.read_text()).get('result') or {}) if res.exists() else {}

    @property
    def plan(self):
        return next((e for e in self.log if e['kind'] == 'result' and e.get('step') == 'plan'), None)

    def revision(self):
        revs = [e for e in self.log if e['kind'] == 'revision']
        return revs[-1] if revs else None

    def landed_files(self):
        if not hasattr(self, '_landed'):
            rev = self.revision()
            out = subprocess.run(['git', '-C', str(ROOT), 'diff', '--name-only', rev['base'], rev['commit']],
                                 capture_output=True, text=True)
            self._landed = [x for x in out.stdout.split('\n') if x]
        return self._landed

    def corrections(self):
        """add/steer messages after the plan result: what a send-back said."""
        t = self.plan['unix_ms']
        return [e['text'] for e in self.log if e['kind'] == 'message' and e.get('mode') in ('add', 'steer') and e['unix_ms'] > t]


def real_tasks():
    out = {}
    for d in REAL.iterdir():
        if d.name.isdigit() and (d / 'log.jsonl').exists() and (d / 'task.json').exists():
            out[int(d.name)] = Task(int(d.name))
    return out


def area_of(files):
    """The crate most changed files are in; scripts and docs only when no crate file changed."""
    c = collections.Counter()
    for f in files:
        for prefix, name in AREAS.items():
            if f.startswith(prefix):
                c[name] += 1
                break
    return c.most_common(1)[0][0] if c else 'scripts_docs'


# ---- pick ------------------------------------------------------------------------------------

def cmd_pick(args):
    tasks = real_tasks()
    pool = collections.defaultdict(list)
    for n, t in sorted(tasks.items()):
        if n < FIRST_TASK or t.landed_ms is None or not t.revision() or t.rec.get('adapter') != 'claude':
            continue
        plan = t.plan
        if not plan or not (plan.get('usage') or {}).get('cost_micro_usd') or not t.steps or t.steps[0]['name'] != 'plan':
            continue
        if not (t.result.get('memo') or t.result.get('summary')) or not t.prompt:
            continue
        # ^ Audits have no cause; a plan over $1.50 is an outlier (a blocked or rotated plan) that would eat the $40.
        if (t.rec.get('title') or '').startswith('Audit') or plan['usage']['cost_micro_usd'] > 1_500_000:
            continue
        files = t.landed_files()
        if not files or any('memory' in f for f in files):
            continue
        if re.search(r'docs/memory|wsx memory|memory-system|memory::', t.prompt):
            continue
        pool[area_of(files)].append(n)
    rng = random.Random(385)
    picks = []
    for area, want in QUOTA.items():
        names = pool[area][:]
        rng.shuffle(names)
        picks += [(n, area) for n in names[:want]]
    rest = [(n, a) for a, ns in pool.items() for n in ns if (n, a) not in picks]
    rng.shuffle(rest)
    while len(picks) < 20 and rest:
        picks.append(rest.pop())
    traps = lambda n: len(tasks[n].corrections()) > 0
    # ^ At least 6 picks must have a send-back after the plan, else no trap can be measured.
    swaps = [x for x in rest if traps(x[0])]
    for i, (n, a) in enumerate(picks):
        if sum(traps(p[0]) for p in picks) >= 6:
            break
        if not traps(n):
            same = [x for x in swaps if x[1] == a]
            if same:
                picks[i] = same[0]
                swaps.remove(same[0])
    picks.sort()
    repeat = []
    for area in ('core', 'web', 'cli_tui'):
        repeat += [n for n, a in picks if a == area][:1]
    rows = []
    for n, area in picks:
        t = tasks[n]
        u = t.plan['usage']
        rows.append({'task': n, 'area': area, 'repeat': n in repeat, 'files': len(t.landed_files()),
                     'corrections': len(t.corrections()), 'logged_cost': u['cost_micro_usd'] / 1e6,
                     'logged_calls': u.get('calls'), 'model': t.rec.get('model'), 'title': t.rec.get('title')})
    WORK.mkdir(parents=True, exist_ok=True)
    (WORK / 'picks.json').write_text(json.dumps(rows, indent=1))
    print(f'{"task":>5} {"area":<13}{"files":>6}{"fixes":>6}{"logged $":>9}{"calls":>6}  title')
    for r in rows:
        print(f'{r["task"]:>5} {r["area"]:<13}{r["files"]:>6}{r["corrections"]:>6}{r["logged_cost"]:>9.2f}{str(r["logged_calls"]):>6}  {"*" if r["repeat"] else " "}{r["title"]}')
    print('pool per area:', {a: len(v) for a, v in pool.items()}, '* = run twice in the baseline arm')


# ---- the cut store ---------------------------------------------------------------------------

def day_ms(word, year=2026):
    m = re.fullmatch(r'(?:(\d{4})-)?(\d{2})-(\d{2})', word)
    if not m:
        return None
    y = int(m.group(1) or year)
    return int(time.mktime((y, int(m.group(2)), int(m.group(3)), 0, 0, 0, 0, 0, 0)) * 1000)


def cut_notes(text, n, t0):
    """decisions / decisions-archive lines dated before t0 (or naming only tasks before #n)."""
    out = []
    for line in text.splitlines():
        if not line.startswith('- '):
            out.append(line)
            continue
        first = line[2:].split(' ')[0]
        when = day_ms(first)
        ids = [int(x) for x in re.findall(r'#(\d+)', line)]
        if (when is not None and when < t0) or (when is None and ids and max(ids) < n):
            out.append(line)
    return '\n'.join(out) + '\n'


def cut_context(text, n):
    """The manager's context.md has no history: keep headers and lines that name no task from #n on."""
    out = []
    skip = False
    for line in text.splitlines():
        if line.startswith('#'):
            skip = bool(re.search(r'#(\d+)', line)) and max(int(x) for x in re.findall(r'#(\d+)', line)) >= n
        ids = [int(x) for x in re.findall(r'#(\d+)', line)]
        if skip or (ids and max(ids) >= n):
            continue
        out.append(line)
    return '\n'.join(out) + '\n'


def project_id(name, path):
    """crates/wsx-core/src/task/store.rs `project_id`: the store folder of a project is named by its path."""
    slug = re.sub(r'[^a-z0-9]', '-', name.lower())[:40].strip('-')
    digest = hashlib.sha256(str(path).encode()).hexdigest()[:8]
    return f'{slug}-{digest}' if slug else digest


def build_state(t, tasks, home, wc):
    """XDG_STATE_HOME for one run: the task store as it was at t.t0. The run's project is its own worktree `wc`
    at the task's base, so the stale rule of memory (`git diff <commit> HEAD`) reads the code as the task
    started, never the real repo's HEAD. Returns (host context lines, the project id of the run)."""
    pid = project_id('wsx', wc)
    root = home / 'wsx' / 'tasks' / pid
    for m, o in tasks.items():
        if o.t0 >= t.t0:
            continue
        d = root / str(m)
        d.mkdir(parents=True)
        lines = [r for r, e in zip(o.raw, o.log) if e['unix_ms'] < t.t0]
        (d / 'log.jsonl').write_text('\n'.join(lines) + '\n')
        rec = dict(o.rec, project_id=pid, project_path=str(wc))
        rec['state'] = 'landed' if o.end_ms and o.end_ms < t.t0 else 'working'
        (d / 'task.json').write_text(json.dumps(rec))
        if o.end_ms and o.end_ms < t.t0 and (o.dir / 'result.json').exists():
            shutil.copy(o.dir / 'result.json', d / 'result.json')
    notes = root / 'manager' / 'notes'
    notes.mkdir(parents=True)
    manager = REAL / 'manager'
    lines = [x for x in (manager / 'log.jsonl').read_text().splitlines() if json.loads(x)['unix_ms'] < t.t0]
    (root / 'manager' / 'log.jsonl').write_text('\n'.join(lines) + '\n')
    for name in ('decisions.md', 'decisions-archive.md'):
        if (manager / 'notes' / name).exists():
            (notes / name).write_text(cut_notes((manager / 'notes' / name).read_text(), t.n, t.t0))
    ctx = cut_context((manager / 'notes' / 'context.md').read_text(), t.n)
    (notes / 'context.md').write_text(ctx)
    # ^ The run's own registry of projects: `-p wsx` and the scope `pid` mean the worktree `wc`, never the owner's
    # checkout. A scope that no registry names is an unbound memory store: no `task:<n>` proof, no git root.
    for config in (home / 'config' / 'wsx', home / 'Library' / 'Application Support' / 'wsx'):
        config.mkdir(parents=True)
        (config / 'config-v2.toml').write_text(f'groups = []\n[[projects]]\nname = "wsx"\npath = "{wc}"\ndefault_branch = "main"\n')
    # ^ wsx refuses a state folder that others can read.
    for folder in [home / 'wsx', *(home / 'wsx').rglob('*')]:
        if folder.is_dir():
            folder.chmod(0o700)
    return ctx, pid


def meet(a, b):
    wild = '*?['
    head = lambda g: g[:min([g.find(c) for c in wild if c in g] or [len(g)])].rstrip('/')
    ha, hb = head(a), head(b)
    return ha == hb or ha.startswith(hb + '/') or hb.startswith(ha + '/') or (not hb) or (not ha)


def file_words(files):
    words = []
    for g in files:
        cut = [g.find(c) for c in '*?[{' if c in g]
        lit = g[:min(cut)] if cut else g
        last = next((p for p in reversed(g.split('/')) if not set(p) & set('*?[{')), None)
        for w in (lit.rstrip('/'), last):
            if w and len(w) >= 3 and w not in words:
                words.append(w)
    return words


def host_context(t, tasks, ctx):
    """crates/wsx/src/task/host.rs `task_context`, rebuilt for the time t.t0 (the log does not keep it)."""
    files = t.rec.get('files') or []
    out = []
    if files:
        out.append('Files: ' + ', '.join(files))
    for m, o in sorted(tasks.items()):
        if m == t.n or o.t0 >= t.t0 or (o.end_ms and o.end_ms < t.t0) or o.rec.get('parent') == t.n or t.rec.get('parent') == m:
            continue
        shared = [g for g in files if any(meet(g, h) for h in o.rec.get('files') or [])]
        if shared:
            out.append(f'Open task #{m} "{o.rec.get("title") or ""}" (working) shares: {", ".join(shared[:5])}')
    words = file_words(files)
    room = 4 * 1024
    lines = []
    for line in reversed(ctx.splitlines()):
        if not any(w in line for w in words):
            continue
        if len(line) > room:
            break
        room -= len(line)
        lines.append(line)
    if lines:
        out.append('Project memos (manager context.md) about these files:\n' + '\n'.join(lines))
    return 'Task context from the host:\n' + '\n'.join(out) if out else ''


# ---- one run ---------------------------------------------------------------------------------

def sh(argv, **kw):
    return subprocess.run(argv, capture_output=True, text=True, **kw)


def clean_env(**extra):
    env = {k: v for k, v in os.environ.items() if not k.startswith('WSX_')}
    env.update(extra)
    return env


def wsx_env(home):
    """The environment of a `wsx` call on a run's state. HOME is the state folder too: `dirs` finds the config folder
    (the registry of projects) there on macOS. Only wsx gets it, never the agent."""
    return clean_env(XDG_STATE_HOME=str(home), XDG_CONFIG_HOME=str(home / 'config'), HOME=str(home))


def spent():
    p = WORK / 'spend.jsonl'
    return sum(json.loads(x)['usd'] for x in p.read_text().splitlines()) if p.exists() else 0.0


def spend(kind, ident, usd):
    with _lock:
        with open(WORK / 'spend.jsonl', 'a') as f:
            f.write(json.dumps({'ts': int(time.time()), 'kind': kind, 'id': ident, 'usd': usd}) + '\n')


def leak_check(t, tasks, home, pid):
    """No future in the run's state: every log line, result, memory record and dated note is older than t.t0."""
    root = home / 'wsx' / 'tasks' / pid
    for d in root.iterdir():
        if d.name.isdigit():
            for line in (d / 'log.jsonl').read_text().splitlines():
                assert json.loads(line)['unix_ms'] < t.t0, f'#{d.name} log has a line after the start of #{t.n}'
            if (d / 'result.json').exists():
                o = tasks[int(d.name)]
                assert o.end_ms and o.end_ms < t.t0, f'#{d.name} result is not final at the start of #{t.n}'
    records = home / 'wsx' / 'tasks' / pid / 'memory' / 'records.jsonl'
    if records.exists():
        for line in records.read_text().splitlines():
            m = re.match(r'memo-(\d+)-', json.loads(line).get('slug', ''))
            assert not m or tasks[int(m.group(1))].landed_ms < t.t0, f'record {m.group(0)} lands after the start of #{t.n}'
    for name in ('decisions.md', 'decisions-archive.md'):
        f = root / 'manager' / 'notes' / name
        for line in f.read_text().splitlines() if f.exists() else []:
            when = day_ms(line[2:].split(' ')[0]) if line.startswith('- ') else None
            assert when is None or when < t.t0, f'{name} has a line dated after the start of #{t.n}'


def memory_gate(args):
    """The memory arm, dry run or paid, runs only on the real code of build steps 4 and 5."""
    for sub in (CAPTURE_SUB, START_SUB):
        done = sh([args.wsx, *sub, '--help'], env=clean_env())
        if done.returncode != 0:
            sys.exit(f'refused: {args.wsx} has no `wsx {" ".join(sub)}` (build steps 4 and 5 are not in this binary); '
                     'the paid memory arm does not run on a stub')


def capture_store(t, tasks, home, args, pid):
    """The with-memory store of one run: step 5's capture rule on every task landed before t.t0.
    Returns (records, store bytes). `t` needs t0 only."""
    env = wsx_env(home)
    wsx = [args.wsx]
    records = 0
    for m, o in sorted(tasks.items()):
        if not o.landed_ms or o.landed_ms >= t.t0 or not o.revision():
            continue
        memo = o.result.get('memo') or ''
        if not memo.strip():
            continue
        files = o.landed_files()
        commit = o.landed_commit or o.revision()['commit']
        done = sh([*wsx, *CAPTURE_SUB, '--scope', pid, '--task', str(m), '--memo', memo, '--commit', commit,
                   '--paths', ','.join(files)], env=env)
        records += done.returncode == 0
    # ^ The rules: decisions dated before t0, already cut into the notes of the state.
    store = home / 'wsx' / 'tasks' / pid / 'memory'
    size = sum(f.stat().st_size for f in store.rglob('*') if f.is_file()) if store.exists() else 0
    return records, size


def memory_cut(t, tasks, home, args, pid):
    """capture_store plus the start set of the task's files. Returns (start set text, records, store bytes)."""
    env = wsx_env(home)
    wsx = [args.wsx]
    records, size = capture_store(t, tasks, home, args, pid)
    files = t.rec.get('files') or []
    start = sh([*wsx, *START_SUB, '--scope', pid, '--files', ','.join(files)], env=env).stdout.strip()
    return start, records, size


def base_stale_check(home, args, pid, wc):
    """Dry runs: the stale rule reads the run's code (`wc`, the task's base), not the real repo's HEAD. Finds a
    captured fact whose paths changed after the task started (stale at the real HEAD, not at `wc`) and asserts
    that the start set of its path still holds it. Returns what it found."""
    def changed(repo, r):
        return sh(['git', '-C', str(repo), 'diff', '--quiet', r['commit'], 'HEAD', '--', *r['paths']]).returncode != 0
    log = home / 'wsx' / 'tasks' / pid / 'memory' / 'records.jsonl'
    facts = {}
    for line in log.read_text().splitlines() if log.exists() else []:
        r = json.loads(line)
        if r.get('kind') == 'fact' and r.get('commit') and r.get('paths'):
            facts[r['slug']] = r
    late = [r for r in facts.values() if changed(ROOT, r) and not changed(wc, r)]
    stale_at_base = sum(changed(wc, r) for r in facts.values())
    found = {'facts': len(facts), 'stale_at_real_head': sum(changed(ROOT, r) for r in facts.values()), 'stale_at_base': stale_at_base,
             'changed_after_start': len(late)}
    if late:
        r = late[0]
        env = wsx_env(home)
        start = sh([args.wsx, *START_SUB, '--scope', pid, '--files', r['paths'][0]], env=env).stdout
        assert r['slug'] in start and f"stale (their files changed since they were true): {r['slug']}" not in start, \
            f'{r["slug"]} changed after the start of the task, so it is not stale at the start: {start}'
        found['proof'] = r['slug']
    return found


def shim(run, home, arm, args, pid):
    """`wsx` on the worker's PATH: `task search` always, `memory ...` in the memory arms only."""
    h = f'HOME={shlex.quote(str(home))}'
    bin_dir = run / 'bin'
    bin_dir.mkdir()
    me = Path(__file__).resolve()
    real = shlex.quote(args.wsx)
    recall = (f'exec {shlex.quote(sys.executable)} {shlex.quote(str(me))} recall-cheap --wsx {real} --home {shlex.quote(str(home))} --pid {pid} '
              f'--out {shlex.quote(str(run / "recall-cost.jsonl"))} "$@"') if arm == 'memory-cheap' else \
        f'{h} exec {real} memory recall --scope {pid} "$@"'
    memory = '' if arm == 'baseline' else f'''
  memory)
    case "$2" in
      recall) shift 2; {recall} ;;
      search|status|list) sub=$2; shift 2; {h} exec {real} memory "$sub" --scope {pid} "$@" ;;
    esac ;;'''
    (bin_dir / 'wsx').write_text(f'''#!/bin/sh
unset WSX_TASK_BROKER WSX_AGENT_TREE
export XDG_STATE_HOME={shlex.quote(str(home))} XDG_CONFIG_HOME={shlex.quote(str(home / 'config'))}
case "$1" in
  task) case "$2" in
    search) shift 2; {h} exec {real} task search -p wsx "$@" ;;
  esac ;;{memory}
esac
echo "wsx: only 'wsx task search'{'' if arm == 'baseline' else ' and wsx memory recall|search|status'} run here" >&2
exit 2
''')
    (bin_dir / 'wsx').chmod(0o755)
    return bin_dir


def explore_count(tools):
    n = 0
    for name, inp in tools:
        if name in ('Read', 'Grep', 'Glob'):
            n += 1
        elif name == 'Bash':
            cmd = inp.get('command', '')
            for seg in re.split(r'\|\||&&|[|;\n]', cmd):
                words = seg.split()
                while words and (words[0] in ('cd', 'env', 'time') or '=' in words[0].split('/')[0]):
                    words = words[2:] if words[0] == 'cd' else words[1:]
                if words and words[0].split('/')[-1] in EXPLORE:
                    n += 1
                    break
    return n


def agent_turn(argv, env, cwd, text, secs):
    p = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         text=True, start_new_session=True)
    timer = threading.Timer(secs, lambda: os.killpg(p.pid, 9))
    timer.start()
    events = []
    try:
        p.stdin.write(json.dumps({'type': 'user', 'message': {'role': 'user', 'content': text}}) + '\n')
        p.stdin.flush()
        for line in p.stdout:
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            events.append(ev)
            if ev.get('type') == 'result':
                break
    finally:
        timer.cancel()
        try:
            p.stdin.close()
            p.wait(10)
        except Exception:
            pass
        try:
            os.killpg(p.pid, 9)
        except ProcessLookupError:
            pass
    return events


def analyse(events):
    msgs, subs, tools, plan, said, first_ctx = set(), set(), [], None, [], 0
    result = next((e for e in events if e.get('type') == 'result'), {})
    for e in events:
        if e.get('type') != 'assistant':
            continue
        m = e.get('message') or {}
        (subs if e.get('parent_tool_use_id') else msgs).add(m.get('id') or id(e))
        mu = m.get('usage') or {}
        if not first_ctx and not e.get('parent_tool_use_id'):
            first_ctx = sum(mu.get(k, 0) for k in ('input_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens'))
        for c in m.get('content') or []:
            if c.get('type') == 'text' and c.get('text', '').strip() and not e.get('parent_tool_use_id'):
                said.append(c['text'].strip())
            if c.get('type') == 'tool_use' and not e.get('parent_tool_use_id'):
                tools.append((c['name'], c.get('input') or {}))
                if c['name'] == 'StructuredOutput':
                    plan = c['input']
    if plan is None and isinstance(result.get('structured_output'), dict):
        plan = result['structured_output']
    text = ''
    if plan:
        text = plan.get('summary', '') + '\n' + '\n'.join(f'{d.get("choice")}: {d.get("reason")}' for d in plan.get('decisions') or []) \
            + '\n' + '\n'.join(plan.get('left_open') or [])
    else:
        text = result.get('result') or ''
    u = result.get('usage') or {}
    return {'cost': result.get('total_cost_usd') or 0.0, 'calls': len(msgs), 'sub_calls': len(subs),
            'input': u.get('input_tokens', 0), 'output': u.get('output_tokens', 0),
            'cache_read': u.get('cache_read_input_tokens', 0), 'cache_write': u.get('cache_creation_input_tokens', 0),
            'said': said, 'memory_uses': sum(1 for n, i in tools if n == 'Bash' and 'wsx memory' in i.get('command', '')),
            'ctx_first': first_ctx, 'explore': explore_count(tools), 'tool_counts': dict(collections.Counter(n for n, _ in tools)),
            'plan_text': text.strip(), 'is_error': bool(result.get('is_error')) or not result}


def named_files(text):
    return {m.strip('`.,:;()') for m in re.findall(r'[\w./-]+\.(?:rs|py|js|mjs|md|toml|ya?ml|json|css|html|sh|txt|lock)\b', text)}


def file_scores(text, landed):
    named = named_files(text)
    hit = {f for f in landed if any(f == n or f.endswith('/' + n) or n.endswith('/' + f) for n in named)}
    good = {n for n in named if any(f == n or f.endswith('/' + n) or n.endswith('/' + f) for f in landed)}
    return {'recall': len(hit) / len(landed) if landed else None, 'precision': len(good) / len(named) if named else None,
            'named': len(named)}


def one_run(task, tasks, arm, rep, args):
    out = WORK / 'runs' / arm / f'{task.n}-{rep}.json'
    if out.exists() and not args.force:
        return json.loads(out.read_text())
    if spent() + 1.0 > args.cap:
        print(f'cap {args.cap} reached ({spent():.2f}); skip {arm} #{task.n}')
        return None
    run = Path(tempfile.mkdtemp(prefix=f'{arm}-{task.n}-', dir=WORK / 'tmp'))
    home = run / 'state'
    wc = run / 'wc'
    try:
        sh(['git', '-C', str(ROOT), 'worktree', 'add', '--detach', str(wc), task.rec['base']], check=True)
        wc = wc.resolve()
        ctx, pid = build_state(task, tasks, home, wc)
        block = host_context(task, tasks, ctx if arm == 'baseline' else '')  # ^ the memory arms' host has no context.md lines: the start set replaces them
        info = {'task': task.n, 'arm': arm, 'rep': rep, 'logged_cost': task.plan['usage']['cost_micro_usd'] / 1e6,
                'logged_calls': task.plan['usage'].get('calls'), 'model': task.rec.get('model')}
        brief = ''
        if arm != 'baseline':
            start, records, size = memory_cut(task, tasks, home, args, pid)
            block = (block + '\n' + start).strip()
            leak_check(task, tasks, home, pid)
            if args.dry_run:
                info['base_stale'] = base_stale_check(home, args, pid, wc)
            brief = ' ' + MEMORY_BRIEF
            info.update({'store_records': records, 'store_bytes': size, 'start_bytes': len(start)})
        info['context_bytes'] = len(block)
        steps = ', '.join(s['name'] for s in task.steps)
        step = task.steps[0]
        text = '\n\n'.join(x for x in [task.prompt, block, f'Step 1 of {len(task.steps)}, {step["name"]}: {step["prompt"]}\n{PLAN_END}'] if x)
        if args.dry_run:
            text += '\nMEMORY'  # ^ the fake runs `wsx memory ...` through the shim and traces each answer
        bin_dir = shim(run, home, arm, args, pid)
        env = clean_env(XDG_STATE_HOME=str(home), XDG_CONFIG_HOME=str(home / 'config'),
                        PATH=f'{bin_dir}:{os.environ["PATH"]}', CARGO_INCREMENTAL='0',
                        FAKE_CLAUDE_TRACE=str(run / 'fake.trace'))
        argv = shlex.split(args.agent) + [
            '-p', '--input-format', 'stream-json', '--output-format', 'stream-json', '--verbose',
            '--setting-sources', 'project,local', '--permission-mode', 'bypassPermissions',
            '--model', task.rec.get('model') or 'sonnet',
            '--append-system-prompt', WORKER.format(branch=task.rec['branch'], steps=steps) + brief,
            '--settings', json.dumps({'attribution': {'commit': '', 'pr': ''}, 'includeCoAuthoredBy': False}),
            '--session-id', str(uuid.uuid4()), '--json-schema', json.dumps(SCHEMA), '--max-budget-usd', '3']
        if task.rec.get('effort'):
            argv += ['--effort', task.rec['effort']]
        if SKILLS.exists():
            argv += ['--plugin-dir', str(SKILLS)]
        t_start = time.time()
        events = agent_turn(argv, env, wc, text, RUN_SECS)
        info['wall_s'] = round(time.time() - t_start, 1)
        info.update(analyse(events))
        extra = 0.0
        rc = run / 'recall-cost.jsonl'
        if rc.exists():
            extra = sum(json.loads(x)['usd'] for x in rc.read_text().splitlines())
            info['recall_calls'] = len(rc.read_text().splitlines())
        info['recall_cost'] = extra
        info['cost_total'] = info['cost'] + extra
        info['landed_files'] = task.landed_files()
        info.update(file_scores(info['plan_text'], info['landed_files']))
        info['edited'] = bool(sh(['git', '-C', str(wc), 'status', '--short']).stdout.strip())  # ^ a plan step edits nothing
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(info, indent=1))
        if args.keep:
            shutil.copytree(run, WORK / 'kept' / run.name, ignore=shutil.ignore_patterns('wc'))
        spend('run', f'{arm}-{task.n}-{rep}', info['cost_total'])
        print(f'{arm:<13}#{task.n:<4}r{rep} ${info["cost_total"]:.2f} (logged ${info["logged_cost"]:.2f}) calls {info["calls"]} '
              f'(logged {info["logged_calls"]}) explore {info["explore"]} {info["wall_s"]}s recall {info["recall"]}', flush=True)
        return info
    finally:
        sh(['git', '-C', str(ROOT), 'worktree', 'remove', '--force', str(wc)])
        shutil.rmtree(run, ignore_errors=True)


def cmd_run(args):
    if args.arm != 'baseline':
        memory_gate(args)
    picks = json.loads((WORK / 'picks.json').read_text())
    if args.dry_run:
        picks = picks[:2]
    if args.tasks:
        picks = [p for p in picks if p['task'] in args.tasks]
    if args.arm == 'memory-cheap':
        picks = picks[::2][:10]  # ^ every second pick: 10 of the 20, spread over the areas
    tasks = real_tasks()
    (WORK / 'tmp').mkdir(parents=True, exist_ok=True)
    jobs = [(tasks[p['task']], 0) for p in picks]
    if args.arm == 'baseline':
        jobs += [(tasks[p['task']], 1) for p in picks if p['repeat']]
    print(f'arm {args.arm}: {len(jobs)} runs, spent so far ${spent():.2f}, cap ${args.cap}', flush=True)
    with cf.ThreadPoolExecutor(args.jobs) as pool:
        list(pool.map(lambda j: one_run(j[0], tasks, args.arm, j[1], args), jobs))
    print(f'spent ${spent():.2f}')


def cmd_recall_cheap(args):
    """`wsx memory recall` with a cheap model: the best 20 hits of a search, the model picks up to 5."""
    env = wsx_env(Path(args.home))
    question = ' '.join(args.question)
    hits = sh([args.wsx, 'memory', 'search', question, '--scope', args.pid, '--limit', '20', '--json'], env=env).stdout
    prompt = (f'A coding agent asks its project memory: {question}\n\nSearch hits (JSON):\n{hits[:12000]}\n\n'
              'Return the at most 5 hits that help answer the question, best first, as lines "- <slug>: <why it helps, max 20 words>". '
              'Nothing else. If none helps, return "none".')
    done = sh(['claude', '-p', '--model', CHEAP_MODEL, '--output-format', 'json', '--setting-sources', 'project,local',
               '--tools', '', '--system-prompt', 'You pick records for a coding agent.', prompt], cwd=tempfile.gettempdir())
    try:
        res = json.loads(done.stdout)
        print(res['result'])
        cost = res.get('total_cost_usd') or 0
    except Exception:
        print('none')
        cost = 0
    with open(args.out, 'a') as f:
        f.write(json.dumps({'usd': cost}) + '\n')


# ---- judge -----------------------------------------------------------------------------------

TRAPS_PROMPT = """A coding task and the messages the owner sent AFTER the worker's plan. Decide which messages corrected a mistake or gap in the worker's approach (a wrong file, a wrong cause, a missed constraint, a wrong design). New scope, new requests and plain approvals are not corrections.
Task:
{prompt}

Messages after the plan, numbered:
{messages}

Answer JSON only: {{"traps": [{{"message": <number>, "trap": "<one sentence: the mistake a plan could repeat>"}}]}} (empty list when none)."""
SCORE_PROMPT = """You score one worker's plan for a coding task against what really landed. You do not know how the plan was made.
Task as asked:
{prompt}

What landed (files changed): {files}
What landed (the worker's final summary): {summary}
Memo (the lessons, root cause): {memo}

Known traps (mistakes the owner later corrected): {traps}

The plan to score:
{plan}

Rubric for "cause": 2 = the plan names the same root cause or core mechanism as the landed summary and memo; 1 = right area, but the cause is different, vague or only a symptom; 0 = it does not find the cause or is wrong. A task with no bug (a feature) scores the core idea of the change the same way.
For each trap say whether the plan repeats that mistake (true) or avoids it or does not touch it (false).
Answer JSON only: {{"cause": 0|1|2, "why": "<one sentence>", "traps": [{{"message": <number>, "repeats": true|false}}]}}"""


def ask_judge(prompt, args):
    if args.dry_run:
        return {'traps': [], 'cause': 1, 'score': 1, 'lost': [], 'wrong': [], 'why': 'dry run'}, 0.0
    done = sh(['claude', '-p', '--model', JUDGE_MODEL, '--output-format', 'json', '--setting-sources', 'project,local',
               '--tools', '', '--system-prompt', 'You are a strict, terse evaluator. Answer JSON only.', prompt], cwd=tempfile.gettempdir())
    res = json.loads(done.stdout)
    m = re.search(r'\{.*\}', res['result'], re.S)
    return json.loads(m.group(0)), res.get('total_cost_usd') or 0.0


def cmd_judge(args):
    tasks = real_tasks()
    path = WORK / 'judged.json'
    judged = json.loads(path.read_text()) if path.exists() else {'traps': {}, 'runs': {}}
    (WORK / 'tmp').mkdir(parents=True, exist_ok=True)
    runs = sorted((WORK / 'runs').glob('*/*.json'))
    for f in runs:
        r = json.loads(f.read_text())
        t = tasks[r['task']]
        key = str(r['task'])
        if key not in judged['traps']:
            msgs = t.corrections()
            if msgs:
                ans, usd = ask_judge(TRAPS_PROMPT.format(prompt=t.prompt[:3000], messages='\n'.join(f'{i + 1}. {m[:900]}' for i, m in enumerate(msgs))), args)
                spend('judge', f'traps-{key}', usd)
                judged['traps'][key] = ans.get('traps', [])
            else:
                judged['traps'][key] = []
        rid = f'{f.parent.name}/{f.name}'
        if rid in judged['runs'] and not args.force:
            continue
        traps = judged['traps'][key]
        # ^ Blind: the prompt carries no arm, no cost and no run name.
        ans, usd = ask_judge(SCORE_PROMPT.format(
            prompt=t.prompt[:3000], files=', '.join(r['landed_files'][:30]), summary=(t.result.get('summary') or '')[:1500],
            memo=(t.result.get('memo') or '')[:1500], plan=r['plan_text'][:6000],
            traps=json.dumps(traps) if traps else 'none'), args)
        spend('judge', rid, usd)
        judged['runs'][rid] = {'cause': ans.get('cause'), 'why': ans.get('why'), 'trap_repeats': ans.get('traps', []), 'trap_count': len(traps)}
        print(f'judged {rid}: cause {ans.get("cause")}', flush=True)
        path.write_text(json.dumps(judged, indent=1))
    path.write_text(json.dumps(judged, indent=1))
    print(f'spent ${spent():.2f}')


# ---- report ----------------------------------------------------------------------------------

def load_runs():
    out = collections.defaultdict(dict)
    judged = json.loads((WORK / 'judged.json').read_text()) if (WORK / 'judged.json').exists() else {'runs': {}}
    for f in sorted((WORK / 'runs').glob('*/*.json')):
        r = json.loads(f.read_text())
        j = judged['runs'].get(f'{f.parent.name}/{f.name}', {})
        r['cause'] = j.get('cause')
        r['trap_hits'] = sum(1 for x in j.get('trap_repeats', []) if x.get('repeats')) if j else None
        r['trap_count'] = j.get('trap_count') if j else None
        out[r['arm']][(r['task'], r['rep'])] = r
    return out


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def fmt(x, nd=2):
    return '-' if x is None else (f'{x:.{nd}f}' if isinstance(x, float) else str(x))


def cmd_report(args):
    runs = load_runs()
    picks = {p['task']: p for p in json.loads((WORK / 'picks.json').read_text())}
    print(f'Spend so far: ${spent():.2f} of ${CAP_USD:.0f}\n')
    for arm, rs in runs.items():
        print(f'### Arm `{arm}`, first run of each task\n')
        print('| task | area | $ replay | $ logged | calls | calls logged | explore | wall s | in | out | cache read | cache write | recall | precision | cause | traps |' + (' store recs | store KB | start B |' if arm != 'baseline' else ''))
        print('|' + '---|' * (16 + (3 if arm != 'baseline' else 0)))
        for (n, rep), r in sorted(rs.items()):
            if rep:
                continue
            trap = f'{r["trap_hits"]}/{r["trap_count"]}' if r['trap_count'] is not None else '-'
            print(f'| #{n} | {picks[n]["area"]} | {fmt(r["cost_total"])} | {fmt(r["logged_cost"])} | {r["calls"]} | {fmt(r["logged_calls"])} | {r["explore"]} | {r["wall_s"]:.0f} | {r["input"]} | {r["output"]} | {r["cache_read"]} | {r["cache_write"]} | {fmt(r["recall"])} | {fmt(r["precision"])} | {fmt(r["cause"])} | {trap} |'
                  + (f' {r.get("store_records")} | {r.get("store_bytes", 0) // 1024} | {r.get("start_bytes")} |' if arm != 'baseline' else ''))
        first = [r for (n, rep), r in rs.items() if rep == 0]
        print(f'\nMedian: ${fmt(med([r["cost_total"] for r in first]))} (logged ${fmt(med([r["logged_cost"] for r in first]))}), calls {fmt(med([r["calls"] for r in first]), 1)} '
              f'(logged {fmt(med([r["logged_calls"] for r in first]), 1)}), explore {fmt(med([r["explore"] for r in first]), 1)}, '
              f'recall {fmt(med([r["recall"] for r in first]))}, precision {fmt(med([r["precision"] for r in first]))}, cause mean {fmt(statistics.mean([r["cause"] for r in first if r["cause"] is not None] or [0]))}\n')
    base = runs.get('baseline', {})
    pairs = [(base[(n, 0)], base[(n, 1)]) for (n, rep) in base if rep == 1 and (n, 0) in base]
    if pairs:
        print('### Noise: the same task twice, baseline arm\n\n| task | $ a | $ b | calls a | calls b | recall a | recall b | cause a | cause b |\n|---|---|---|---|---|---|---|---|---|')
        rel = []
        for a, b in pairs:
            print(f'| #{a["task"]} | {fmt(a["cost_total"])} | {fmt(b["cost_total"])} | {a["calls"]} | {b["calls"]} | {fmt(a["recall"])} | {fmt(b["recall"])} | {fmt(a["cause"])} | {fmt(b["cause"])} |')
            rel.append(abs(a['cost_total'] - b['cost_total']) / max((a['cost_total'] + b['cost_total']) / 2, 1e-9))
        spread = statistics.mean(rel)
        print(f'\nRelative cost spread (|a-b| / mean, mean of {len(rel)} pairs): {spread:.2f}; calls spread {statistics.mean([abs(a["calls"] - b["calls"]) / max((a["calls"] + b["calls"]) / 2, 1) for a, b in pairs]):.2f}; '
              f'recall spread {statistics.mean([abs((a["recall"] or 0) - (b["recall"] or 0)) for a, b in pairs]):.2f}; cause spread {statistics.mean([abs((a["cause"] or 0) - (b["cause"] or 0)) for a, b in pairs]):.2f}')
        first = [r for (n, rep), r in base.items() if rep == 0]
        ratio = med([r['cost_total'] for r in first]) / med([r['logged_cost'] for r in first])
        spread = max(spread, 1e-9)
        verdict = 'FAR from' if abs(ratio - 1) > 2 * spread else 'within 2x spread of'
        print(f'\nFidelity: replay median $/logged median = {ratio:.2f}; this is {verdict} the logged median (limit 1 +- {2 * spread:.2f}).')
    mem = runs.get('memory')
    if mem and base:
        print('\n### Memory vs baseline (first run of each task)\n')
        for arm in [a for a in runs if a != 'baseline']:
            both = [(base[(n, 0)], r) for (n, rep), r in runs[arm].items() if rep == 0 and (n, 0) in base]
            print(f'`{arm}` on {len(both)} tasks: cost median {fmt(med([b["cost_total"] for b, _ in both]))} -> {fmt(med([r["cost_total"] for _, r in both]))}; '
                  f'calls {fmt(med([b["calls"] for b, _ in both]), 1)} -> {fmt(med([r["calls"] for _, r in both]), 1)}; '
                  f'recall {fmt(med([b["recall"] for b, _ in both]))} -> {fmt(med([r["recall"] for _, r in both]))}; '
                  f'cause {fmt(statistics.mean([b["cause"] or 0 for b, _ in both]))} -> {fmt(statistics.mean([r["cause"] or 0 for _, r in both]))}')


# ---- live ------------------------------------------------------------------------------------

def cmd_live(args):
    """Landed tasks from the real logs: cost per step, send-backs, failed checks, plan calls."""
    rows = []
    for n, t in sorted(real_tasks().items()):
        if t.landed_ms is None or not t.plan:
            continue
        results = [e for e in t.log if e['kind'] == 'result' and e.get('usage')]
        cost = sum(e['usage'].get('cost_micro_usd', 0) for e in results) / 1e6
        plan = [e for e in results if e.get('step') == 'plan']
        pcost = sum(e['usage'].get('cost_micro_usd', 0) for e in plan) / 1e6
        first_build = next((e['unix_ms'] for e in results if e.get('step') in ('build', 'review')), None)
        sendbacks = sum(1 for e in t.log if e['kind'] == 'message' and e.get('mode') == 'add' and first_build and e['unix_ms'] > first_build)
        failed = sum(1 for e in t.log if e['kind'] == 'checks' and any(r.get('exit') not in (0, None) for r in e.get('results', [])))
        pcalls = (plan[0]['usage'].get('calls') if plan else None)
        rows.append({'task': n, 'landed_ms': t.landed_ms, 'cost': cost, 'plan_cost': pcost, 'sendbacks': sendbacks, 'failed_checks': failed, 'plan_calls': pcalls})
    cutoff = args.cutoff_ms
    if args.cutoff_ms == 0 and args.cutoff_task:
        cutoff = next((r['landed_ms'] for r in rows if r['task'] == args.cutoff_task), None)  # ^ None: not landed yet, all rows are 'before'
    cutoff = cutoff or float('inf')
    print('| group | landed tasks | median $ | mean $ | plan share of $ | median send-backs | mean failed checks | median plan calls |\n|---|---|---|---|---|---|---|---|')
    for name, sel in (('before #380 lands', [r for r in rows if r['landed_ms'] < cutoff]), ('after', [r for r in rows if r['landed_ms'] >= cutoff])):
        if not sel:
            print(f'| {name} | 0 | - | - | - | - | - | - |')
            continue
        share = sum(r['plan_cost'] for r in sel) / sum(r['cost'] for r in sel)
        print(f'| {name} | {len(sel)} | {med([r["cost"] for r in sel]):.2f} | {statistics.mean([r["cost"] for r in sel]):.2f} | {share:.0%} | '
              f'{med([r["sendbacks"] for r in sel])} | {statistics.mean([r["failed_checks"] for r in sel]):.2f} | {fmt(med([r["plan_calls"] for r in sel]), 1)} |')
    print('\nConfounded: the groups are different tasks with different work. Use as support only.')


# ---- manager: does a small rotation limit hold quality, and what does it save? --------------------
# Method and verdict rule: docs/design/memory-eval.md "Manager rotation".

MANAGER_LOG = REAL / 'manager' / 'log.jsonl'
# ^ Opus 5.5, API-equivalent dollars per token (docs/design/manager-efficiency.md): the logged cost of a reply is
# exactly in*4 + out*20 + cache_read*0.2 + cache_write*8 per million.
PRICE = {'in': 4e-6, 'out': 20e-6, 'read': 0.2e-6, 'write': 8e-6}
ARMS = {'cut150': (150_000, True), 'cut60': (60_000, True), 'lean60': (60_000, False)}  # (rotation limit, start notes)
NOTE_CHARS_PER_TOKEN = 3.5
TURN_TEXT_CLIP = 1500


class At:
    """The moment a replayed manager turn starts: what build_state needs of a task."""
    def __init__(self, t0, tasks):
        self.t0 = t0
        self.n = max([n for n, o in tasks.items() if o.t0 < t0] or [0]) + 1
        self.rec = {'project_path': next(iter(tasks.values())).rec['project_path'], 'files': [], 'title': ''}


def manager_turns():
    """Every manager reply of the log with its messages, the commands it ran and whether the conversation was
    rotated just before it (a `rotated` line after the previous reply: tokens 0 is the idle rotation)."""
    log = [json.loads(x) for x in MANAGER_LOG.read_text().splitlines()]
    msgs = {e['id']: e for e in log if e['kind'] == 'message'}
    turns, cmds, rot = [], [], None
    for e in log:
        if e['kind'] == 'agent' and not e.get('said'):
            cmds.append(e['text'])
        elif e['kind'] == 'rotated':
            rot = e['tokens']
        elif e['kind'] == 'reply':
            turns.append({'ms': e['unix_ms'], 'id': e['id'], 'msgs': [msgs[i] for i in e['to'] if i in msgs], 'reply': e['text'],
                          'cmds': cmds, 'usage': e.get('usage'), 'rotated': rot})
            cmds, rot = [], None
    return turns


def week(turns):
    last = turns[-1]['ms']
    return [i for i, t in enumerate(turns) if t['ms'] >= last - 7 * 86400 * 1000 and t['usage']]


def turn_cost(u):
    return u['input_tokens'] * PRICE['in'] + u['output_tokens'] * PRICE['out'] + u['cache_read_tokens'] * PRICE['read'] + u['cache_creation_tokens'] * PRICE['write']


def conv_stats(turns, idx):
    """Growth of the context per turn and the size of a fresh conversation's first reply, from the log."""
    growth, starts = [], []
    for a, b in zip(idx, idx[1:]):
        if b != a + 1:
            continue
        if turns[b]['rotated'] is not None:
            starts.append(turns[b]['usage']['context_tokens'])
        elif 0 < (d := turns[b]['usage']['context_tokens'] - turns[a]['usage']['context_tokens']) < 60_000:
            growth.append(d)
    return statistics.median(growth), statistics.median(starts)


def project(turns, idx, limit, start, recall_uses=0.0):
    """Manager dollars of the week if the conversation rotated at `limit` tokens and a fresh one starts at `start` tokens.
    The reads of a reply scale with the context it ran in; new tokens (cache writes), output and calls stay as logged.
    A rotation costs a cold write of `start` tokens. (est.) Size rotations of the log are replaced, idle ones kept."""
    g = conv_stats(turns, idx)[0]
    total, rotations, ctx, fresh = 0.0, 0, 0, True
    for k, i in enumerate(idx):
        t, u = turns[i], turns[i]['usage']
        idle = t['rotated'] == 0
        real_size_rotation = t['rotated'] is not None and t['rotated'] != 0
        if limit is None:  # ^ the log as it was
            total += turn_cost(u)
            continue
        if fresh or idle or ctx + g >= limit:
            ctx, rotations = start, rotations + 1
            cold = start * PRICE['write']
        else:
            ctx += min(max(u['context_tokens'] - turns[idx[k - 1]]['usage']['context_tokens'], 0), 60_000) if not real_size_rotation else g
            cold = 0.0
        write = u['cache_creation_tokens']
        if real_size_rotation or t['rotated'] == 0:
            write = max(write - max(u['context_tokens'] - g, 0), 0)  # ^ the log's cold start is replaced by the simulated one
        reads = u['cache_read_tokens'] * ctx / max(u['context_tokens'], 1)
        extra = recall_uses * (ctx * PRICE['read'] + 600 * PRICE['write'])
        total += u['input_tokens'] * PRICE['in'] + u['output_tokens'] * PRICE['out'] + reads * PRICE['read'] + write * PRICE['write'] + cold + extra
        fresh = ctx >= limit
    return total, rotations


def cmd_mproject(args):
    turns = manager_turns()
    idx = week(turns)
    g, s_now = conv_stats(turns, idx)
    notes = sum(len(p.read_text()) for p in [REAL / 'manager' / 'notes' / 'decisions.md', REAL / 'manager' / 'notes' / 'context.md'] if p.exists())
    s_lean = s_now - notes / NOTE_CHARS_PER_TOKEN + 1000  # ^ (est.) no notes in the start, a start set of about 1k tokens instead
    logged = sum(turn_cost(turns[i]['usage']) for i in idx)
    real = sum(turns[i]['usage']['cost_micro_usd'] for i in idx) / 1e6
    runs = list(WORK.glob('manager-runs/*/*.json'))
    uses = [json.loads(f.read_text())['memory_uses'] for f in runs] if runs else []
    ru = statistics.mean(uses) if uses else args.recall_uses
    print(f'Week: {len(idx)} replies, ${real:.2f} logged (price model ${logged:.2f}), context median {statistics.median(turns[i]["usage"]["context_tokens"] for i in idx):.0f}, '
          f'growth {g:.0f} tokens per turn, fresh start {s_now:.0f} tokens (lean start (est.) {s_lean:.0f}), recall uses per turn {ru:.2f}'
          f'{" (measured)" if uses else " (assumed, no replay yet)"}\n')
    print('| rotation limit | start tokens | rotations | weekly $ | per reply $ | vs logged |\n|---|---|---|---|---|---|')
    print(f'| as logged (size rotations at {statistics.median([t["rotated"] for t in turns if t["rotated"]]):.0f} median, idle ones) | {s_now:.0f} | {sum(turns[i]["rotated"] is not None for i in idx)} | {logged:.0f} | {logged / len(idx):.3f} | |')
    rows = {}
    for name, limit, start, ru_ in [('400k', 400_000, s_now, 0), ('150k + recall', 150_000, s_now, ru), ('60k + recall', 60_000, s_now, ru),
                                    ('60k + recall, lean start (est.)', 60_000, s_lean, ru), ('100k + recall, lean start (est.)', 100_000, s_lean, ru)]:
        cost, rot = project(turns, idx, limit, start, ru_)
        rows[name] = cost
        print(f'| {name} | {start:.0f} | {rot} | {cost:.0f} | {cost / len(idx):.3f} | {cost / logged - 1:+.0%} |')
    return logged, rows


# ---- manager: pick, run, judge, report --------------------------------------------------------

ASKS = re.compile(r'earlier|before|last time|you said|again|same|that one|those|이전|아까|전에|그때|말한', re.I)


def cmd_mpick(args):
    turns = manager_turns()
    idx = week(turns)
    g, s_now = conv_stats(turns, idx)
    k150, k60 = int((150_000 - s_now) // g), int((60_000 - s_now) // g)
    where = {m['id']: i for i, t in enumerate(turns) for m in t['msgs']}
    where.update({t['id']: i for i, t in enumerate(turns)})  # ^ an owner quote names a message or a reply
    cands = []
    for i in idx:
        t = turns[i]
        owner = [m for m in t['msgs'] if m['from'] == 'owner']
        if not owner or sum(len(m['text']) for m in owner) > 1500 or 'attached image' in ' '.join(m['text'] for m in owner):
            continue
        dist = None
        for m in owner:
            q = where.get(m.get('quote'))
            if q is not None and q < i:
                dist = i - q
            for ref in re.findall(r'#(\d+)', m['text']):
                last = max((j for j in range(i) if f'#{ref}' in turns[j]['reply'] or any(f'#{ref}' in x['text'] for x in turns[j]['msgs'])), default=None)
                if last is not None and (dist is None or i - last > dist):
                    dist = i - last
        if dist and dist > k60 and (ASKS.search(' '.join(m['text'] for m in owner)) or any(m.get('quote') for m in owner) or dist > k150):
            cands.append((i, dist))
    rng = random.Random(385)
    far = [c for c in cands if c[1] > k150]
    near = [c for c in cands if c[1] <= k150]
    rng.shuffle(far), rng.shuffle(near)
    picks = sorted(far[:8] + near[:15 - len(far[:8])])  # ^ every far one (at most 8), the rest near; few messages reach beyond 150k
    rows = [{'turn': i, 'reply': turns[i]['id'], 'distance': d, 'beyond': 'beyond 150k window' if d > k150 else 'beyond 60k window',
             'text': ' / '.join(m['text'][:120] for m in turns[i]['msgs'] if m['from'] == 'owner'), 'logged_cost': turns[i]['usage']['cost_micro_usd'] / 1e6,
             'repeat': j < 3} for j, (i, d) in enumerate(picks)]
    WORK.mkdir(parents=True, exist_ok=True)
    (WORK / 'manager-picks.json').write_text(json.dumps({'k150': k150, 'k60': k60, 'picks': rows}, indent=1))
    print(f'window: 150k = last {k150} turns, 60k = last {k60} turns (growth {g:.0f} tokens per turn, fresh start {s_now:.0f}); {len(cands)} candidates, {len(far)} beyond 150k')
    for r in rows:
        print(f'reply {r["reply"]} dist {r["distance"]:>3} ${r["logged_cost"]:.2f} {r["text"][:100]!r}')


def manager_shim(run, home, args, pid):
    h = f'HOME={shlex.quote(str(home))}'
    """`wsx` for the replayed manager: read verbs of `task` and `wsx memory recall|search|status` run on the cut state;
    the others only record their command (the replay changes nothing)."""
    bin_dir = run / 'bin'
    bin_dir.mkdir()
    real = shlex.quote(args.wsx)
    (bin_dir / 'wsx').write_text(f'''#!/bin/sh
unset WSX_TASK_BROKER WSX_AGENT_TREE
export XDG_STATE_HOME={shlex.quote(str(home))} XDG_CONFIG_HOME={shlex.quote(str(home / 'config'))}
case "$1 $2" in
  "task ledger"|"task show"|"task search"|"task log"|"task status"|"task list") {h} exec {real} "$@" ;;
  "memory recall") shift 2; {h} exec {real} memory recall --scope {pid} "$@" ;;
  "memory search"|"memory status") sub=$2; shift 2; {h} exec {real} memory "$sub" --scope {pid} "$@" ;;
  "task "*) echo "$@" >> {shlex.quote(str(run / 'commands.txt'))}; echo '{{"ok":true,"note":"replay: not run"}}'; exit 0 ;;
esac
echo "wsx: only wsx task and wsx memory recall|search|status run here" >&2
exit 2
''')
    (bin_dir / 'wsx').chmod(0o755)
    return bin_dir


def handover(turns, i, k, notes):
    """The text a fresh manager conversation starts with: the notes, the last k turns, then the new messages."""
    out = []
    if notes:
        out.append('Your notes files (read at the start of this conversation):\n' + notes)
    if k:
        out.append(f'The last {min(k, i)} turns of the earlier conversation, oldest first:')
        for t in turns[max(0, i - k):i]:
            for m in t['msgs']:
                out.append(f'{"Owner" if m["from"] == "owner" else "Events"} (message {m["id"]}): {m["text"][:TURN_TEXT_CLIP]}')
            out += [f'You ran: {c[:200]}' for c in t['cmds'][:6]]
            out.append(f'You: {t["reply"][:TURN_TEXT_CLIP]}')
    out.append('The new messages (answer them):')
    for m in turns[i]['msgs']:
        out.append(f'{"Owner" if m["from"] == "owner" else "Events"} (message {m["id"]}): {m["text"][:3000]}')
    return '\n\n'.join(out)


def manager_run(pick, arm, rep, turns, tasks, args):
    out = WORK / 'manager-runs' / arm / f'{pick["reply"]}-{rep}.json'
    if out.exists() and not args.force:
        return json.loads(out.read_text())
    if spent() + 1.0 > args.cap:
        print(f'cap {args.cap} reached ({spent():.2f}); skip {arm} reply {pick["reply"]}')
        return None
    limit, with_notes = ARMS[arm]
    g_s = conv_stats(turns, week(turns))  # ^ (growth per turn, fresh start) of the log
    i = pick['turn']
    t0 = min(m['unix_ms'] for m in turns[i]['msgs'])
    at = At(t0, tasks)
    run = Path(tempfile.mkdtemp(prefix=f'm-{arm}-{pick["reply"]}-', dir=WORK / 'tmp'))
    home, wc = run / 'state', run / 'wc'
    try:
        base = max((o for o in tasks.values() if o.t0 < t0), key=lambda o: o.t0).rec['base']
        sh(['git', '-C', str(ROOT), 'worktree', 'add', '--detach', str(wc), base], check=True)
        wc = wc.resolve()
        _, pid = build_state(at, tasks, home, wc)
        notes_dir = home / 'wsx' / 'tasks' / pid / 'manager' / 'notes'
        notes = ''
        if with_notes:
            notes = '\n\n'.join(f'{name}:\n{(notes_dir / name).read_text()}' for name in ('decisions.md', 'context.md') if (notes_dir / name).exists())
        s_start = g_s[1] if with_notes else g_s[1] - len(notes_of_log(tasks, at)) / NOTE_CHARS_PER_TOKEN + 1000
        k = max(0, int((limit - s_start) // g_s[0]))
        rec = capture_store(at, tasks, home, args, pid)[0]
        spec = importlib.util.spec_from_file_location('bench_manager', ROOT / 'scripts' / 'bench-manager.py')
        bm = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(bm)
        system = bm.instructions((ROOT / 'crates/wsx/src/task/instructions.rs').read_text(), 'wsx', str(notes_dir), str(home / 'overview.md'))
        system += '\n\nOld conversation is not in your context. Find it with `wsx memory recall "<question>"` or `wsx memory search <words>`: the best records and past messages of this project.'
        text = handover(turns, i, k, notes)
        bin_dir = manager_shim(run, home, args, pid)
        env = clean_env(XDG_STATE_HOME=str(home), XDG_CONFIG_HOME=str(home / 'config'), PATH=f'{bin_dir}:{os.environ["PATH"]}',
                        FAKE_CLAUDE_TRACE=str(run / 'fake.trace'))
        argv = shlex.split(args.agent) + ['-p', '--input-format', 'stream-json', '--output-format', 'stream-json', '--verbose',
                                          '--setting-sources', 'project,local', '--permission-mode', 'bypassPermissions', '--model', 'opus',
                                          '--append-system-prompt', system, '--session-id', str(uuid.uuid4()), '--max-budget-usd', '3']
        started = time.time()
        events = agent_turn(argv, env, wc, text, RUN_SECS)
        info = {'reply': pick['reply'], 'arm': arm, 'rep': rep, 'k_turns': k, 'handover_bytes': len(text), 'store_records': rec,
                'wall_s': round(time.time() - started, 1), 'logged_cost': pick['logged_cost']}
        info.update(analyse(events))
        info['reply_text'] = '\n\n'.join(info['said']) or info['plan_text']
        info['commands'] = (run / 'commands.txt').read_text().splitlines() if (run / 'commands.txt').exists() else []
        info['cost_total'] = info['cost']
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(info, indent=1))
        spend('manager', f'{arm}-{pick["reply"]}-{rep}', info['cost_total'])
        print(f'{arm:<7}reply {pick["reply"]} r{rep} ${info["cost_total"]:.2f} (logged ${pick["logged_cost"]:.2f}) k {k} first ctx {info["ctx_first"]} calls {info["calls"]} memory uses {info["memory_uses"]} {info["wall_s"]}s', flush=True)
        return info
    finally:
        sh(['git', '-C', str(ROOT), 'worktree', 'remove', '--force', str(wc)])
        shutil.rmtree(run, ignore_errors=True)


def notes_of_log(tasks, at):
    # ^ Today's notes files, for the size of a start without them (est.).
    return ''.join(p.read_text() for p in [REAL / 'manager' / 'notes' / 'decisions.md', REAL / 'manager' / 'notes' / 'context.md'] if p.exists())


def cmd_mrun(args):
    memory_gate(args)  # ^ the paid manager arms run on the memory code of steps 4 and 5, like the memory arm
    meta = json.loads((WORK / 'manager-picks.json').read_text())
    picks = meta['picks'][:2] if args.dry_run else meta['picks']
    turns = manager_turns()
    tasks = real_tasks()
    (WORK / 'tmp').mkdir(parents=True, exist_ok=True)
    jobs = [(p, 0) for p in picks] + [(p, 1) for p in picks if p['repeat'] and args.arm == 'cut150']
    print(f'manager arm {args.arm}: {len(jobs)} runs, spent so far ${spent():.2f}, cap ${args.cap}', flush=True)
    with cf.ThreadPoolExecutor(args.jobs) as pool:
        list(pool.map(lambda j: manager_run(j[0], args.arm, j[1], turns, tasks, args), jobs))
    print(f'spent ${spent():.2f}')


MJUDGE_PROMPT = """You compare two answers of a project manager to the same owner messages. REFERENCE is the real answer, CANDIDATE a new one made from less context. Judge only the content, not style.
Owner messages (and events) of the turn:
{messages}

REFERENCE answer:
{ref}
REFERENCE commands run: {ref_cmds}

CANDIDATE answer:
{cand}
CANDIDATE commands run: {cand_cmds}

Score: 2 = the candidate answers the same question with the same facts and decisions; 1 = the same decision or answer, but facts the reference gives are missing or changed; 0 = a different answer, a wrong decision, or it misses what the owner asked.
List "lost": each concrete fact, name, number or decision of the reference that the candidate lacks (short). List "wrong": candidate statements that contradict the reference.
Answer JSON only: {{"score": 0|1|2, "lost": ["..."], "wrong": ["..."], "why": "<one sentence>"}}"""


def cmd_mjudge(args):
    turns = manager_turns()
    by_reply = {t['id']: t for t in turns}
    path = WORK / 'manager-judged.json'
    judged = json.loads(path.read_text()) if path.exists() else {}
    for f in sorted((WORK / 'manager-runs').glob('*/*.json')):
        r = json.loads(f.read_text())
        rid = f'{f.parent.name}/{f.name}'
        if rid in judged and not args.force:
            continue
        t = by_reply[r['reply']]
        ans, usd = ask_judge(MJUDGE_PROMPT.format(
            messages='\n'.join(f'{"Owner" if m["from"] == "owner" else "Events"}: {m["text"][:1500]}' for m in t['msgs']),
            ref=t['reply'][:3000], ref_cmds='; '.join(c[:160] for c in t['cmds'][:12]) or 'none',
            cand=r['reply_text'][:3000], cand_cmds='; '.join(c[:160] for c in r['commands'][:12]) or 'none'), args)
        spend('judge', f'manager-{rid}', usd)
        judged[rid] = {'score': ans.get('score'), 'lost': ans.get('lost', []), 'wrong': ans.get('wrong', []), 'why': ans.get('why')}
        print(f'judged {rid}: {ans.get("score")}', flush=True)
        path.write_text(json.dumps(judged, indent=1))
    print(f'spent ${spent():.2f}')


def cmd_mreport(args):
    meta = json.loads((WORK / 'manager-picks.json').read_text())
    judged = json.loads((WORK / 'manager-judged.json').read_text()) if (WORK / 'manager-judged.json').exists() else {}
    runs = collections.defaultdict(dict)
    for f in sorted((WORK / 'manager-runs').glob('*/*.json')):
        r = json.loads(f.read_text())
        r.update({f'judge_{k}': v for k, v in judged.get(f'{f.parent.name}/{f.name}', {}).items()})
        runs[r['arm']][(r['reply'], r['rep'])] = r
    arms = list(runs)
    print(f'Spend so far: ${spent():.2f} of ${CAP_USD:.0f}. Windows: 150k = last {meta["k150"]} turns, 60k = last {meta["k60"]} turns.\n')
    print('| reply | distance | $ real | ' + ' | '.join(f'$ {a} | calls | memory | k | score {a}' for a in arms) + ' |\n|' + '---|' * (3 + 5 * len(arms)))
    for p in meta['picks']:
        cells = []
        for a in arms:
            r = runs[a].get((p['reply'], 0))
            cells.append(f'{r["cost_total"]:.2f} | {r["calls"]} | {r["memory_uses"]} | {r["k_turns"]} | {r.get("judge_score", "-")}' if r else '- | - | - | - | -')
        print(f'| {p["reply"]} | {p["distance"]} ({p["beyond"].split()[1]}) | {p["logged_cost"]:.2f} | ' + ' | '.join(cells) + ' |')
    print()
    for a in arms:
        rs = [r for (reply, rep), r in runs[a].items() if rep == 0]
        sc = [r['judge_score'] for r in rs if r.get('judge_score') is not None]
        if not sc:
            continue
        print(f'`{a}`: {len(sc)} replies, score mean {statistics.mean(sc):.2f} of 2, same (2) {sum(s == 2 for s in sc)}, different (0) {sum(s == 0 for s in sc)}; '
              f'cost median ${statistics.median(r["cost_total"] for r in rs):.2f} against real ${statistics.median(r["logged_cost"] for r in rs):.2f}; '
              f'memory uses per reply {statistics.mean(r["memory_uses"] for r in rs):.2f}')
    pairs = [(runs['cut150'][(p['reply'], 0)], runs['cut150'][(p['reply'], 1)]) for p in meta['picks'] if 'cut150' in runs and (p['reply'], 1) in runs['cut150'] and (p['reply'], 0) in runs['cut150']]
    pairs = [(a, b) for a, b in pairs if a.get('judge_score') is not None and b.get('judge_score') is not None]
    if pairs:
        print(f'\nRerun spread (cut150, {len(pairs)} replies run twice): score {statistics.mean(abs(a["judge_score"] - b["judge_score"]) for a, b in pairs):.2f}, '
              f'cost {statistics.mean(abs(a["cost_total"] - b["cost_total"]) / max((a["cost_total"] + b["cost_total"]) / 2, 1e-9) for a, b in pairs):.2f}')
    print('\nLost facts (score below 2):')
    for a in arms:
        for (reply, rep), r in sorted(runs[a].items()):
            if rep == 0 and r.get('judge_score') is not None and r['judge_score'] < 2:
                print(f'- {a} reply {reply} (score {r["judge_score"]}): ' + '; '.join(r.get('judge_lost', [])[:4]))
    print()
    cmd_mproject(args)


# ---- offline retrieval: does the start set hold what a plan step read? (docs/design/memory-eval.md "Offline retrieval") ----

READERS = {'rg', 'grep', 'egrep', 'fgrep', 'sed', 'cat', 'head', 'tail', 'awk', 'bat', 'less', 'wc', 'nl'}
HOME_PREFIX = re.compile(r'(?:~|/Users/[^/\s]+)/ws/wsx(?:-t\d+)?/')
TRAP_WORDS = 2  # ^ rare words a send-back shares with a memo; both set on the first two thirds (the doc: "Offline retrieval")
TRAP_RARE = 6  # ^ a word is rare when at most this many memos use it
START_BYTES = 4096  # ^ the budget of the start block (docs/design/memory-system.md)
STOP = set('about after again against because before being between could does doing from have into just more most '
           'must only other should than that their there these they this those through under until what when '
           'where which while with without would your'.split())
_cache = {}


def tree_of(base):
    """(files, directories) of the repo at a commit."""
    if base not in _cache:
        files = set(sh(['git', '-C', str(ROOT), 'ls-tree', '-r', '--name-only', base]).stdout.split('\n')) - {''}
        _cache[base] = (files, {d for f in files for d in [f.rsplit('/', 1)[0]] if '/' in f} |
                        {'/'.join(f.split('/')[:i]) for f in files for i in range(1, f.count('/'))})
    return _cache[base]


def path_tokens(text, tree):
    """repo files and directories a line names (absolute paths of a working copy are cut to repo paths)."""
    files, dirs = tree
    got, seen_dirs = [], []
    for w in re.findall(r'[\w@+./-]+', HOME_PREFIX.sub('', text)):
        w = w.strip('.,:;').removeprefix('./')
        if w in files:
            got.append(w)
        elif w.rstrip('/') in dirs:
            seen_dirs.append(w.rstrip('/'))
    return got, seen_dirs


def read_labels(t, tree):
    """(a) files the plan step read, from the agent lines before the plan result. The log clips text, so this is a
    lower bound: `lost` counts the clipped Bash lines and the calls that carry no arguments."""
    end = t.plan['unix_ms']
    found, dirs = {}, set()
    lost = collections.Counter()
    for i, e in enumerate(t.log):
        if e['kind'] != 'agent' or e['unix_ms'] >= end:
            continue
        text = e['text']
        head = text.split(' ', 1)[0]
        if head == 'Read':
            body = text[5:]
        elif head == 'Bash':
            body = text[5:]
            lost['bash'] += 1
            if text.endswith('…'):  # ^ cut mid-token: drop the last word, it may be half a path
                lost['clipped'] += 1
                body = body.rsplit(None, 1)[0] if ' ' in body else ''
            if not set(re.split(r'[^\w-]+', body)) & READERS:
                continue
        elif head in ('Agent', 'Grep', 'Glob'):
            lost['no_args'] += 1  # ^ subagent and search calls are logged without their arguments
            continue
        else:
            continue
        files, ds = path_tokens(body, tree)
        for f in files:
            found.setdefault(f, []).append(i)
        dirs.update(ds)
    return found, sorted(dirs), dict(lost)


def trap_words(text):
    return {w for w in re.findall(r'[a-z][a-z0-9_]{5,}', text.lower()) if w not in STOP}


def trap_stats(tasks):
    """(words that few memos use, files most tasks change): common words and hot files make every pair match."""
    if 'trap' not in _cache:
        memos = [trap_words(o.result.get('memo') or '') for o in tasks.values()]
        df = collections.Counter(w for m in memos for w in m)
        landed = [o for o in tasks.values() if o.landed_ms and o.revision()]
        files = collections.Counter(f for o in landed for f in o.landed_files())
        _cache['trap'] = ({w for w, c in df.items() if c <= TRAP_RARE}, {f for f, c in files.items() if c > len(landed) * 0.1})
    return _cache['trap']


def trap_pairs(t, tasks, tree, mine):
    """(c) earlier landed tasks whose memo names a file of this task and which a send-back of this task matches.
    A send-back is an `add` after the plan that the host did not write (a land notice names no mistake)."""
    adds = [e['text'] for e in t.log if e['kind'] == 'message' and e.get('mode') == 'add' and e['unix_ms'] > t.plan['unix_ms']
            and not re.match(r'#\d+ landed on main', e['text'])]
    if not adds:
        return []
    rare, hot = trap_stats(tasks)
    prompt = trap_words(t.prompt)
    out = []
    for m, o in tasks.items():
        memo = (o.result.get('memo') or '') if o.landed_ms and o.landed_ms < t.t0 else ''
        named = sorted(set(path_tokens(memo, tree)[0]) & mine - hot)
        if not named:
            continue
        shared = max((trap_words(a) & trap_words(memo) & rare - prompt for a in adds), key=len)
        if len(shared) >= TRAP_WORDS:
            out.append({'task': m, 'files': named, 'words': sorted(shared)[:8]})
    return out


# -- the start set -------------------------------------------------------------------------------

def load_records(home):
    """Active records of the cut store with the files they name: {slug: {slug, task, paths, commit, text, updated}}."""
    f = home / 'wsx' / 'tasks' / PID / 'memory' / 'records.jsonl'
    cur = {}
    for line in f.read_text().splitlines() if f.exists() else []:
        try:
            r = json.loads(line)
        except ValueError:
            continue
        cur[r['slug']] = r
    out = {}
    for slug, r in cur.items():
        if r.get('status', 'active') != 'active':
            continue
        text = r.get('text') or ''
        m = re.search(r'^Files: (.*)$', text, re.M)
        paths = r.get('paths') or ([x.strip() for x in m.group(1).split(',')] if m else [])
        n = re.fullmatch(r't(\d+)', slug) or re.fullmatch(r'task:(\d+)', r.get('source') or '')
        out[slug] = {'slug': slug, 'task': int(n.group(1)) if n else None, 'paths': paths, 'commit': r.get('commit'),
                     'text': text, 'updated': r.get('updated', 0)}
    return out


def changed(commit, base):
    """paths that differ between a record's commit and the base."""
    key = ('changed', commit, base)
    if key not in _cache:
        _cache[key] = set(sh(['git', '-C', str(ROOT), 'diff', '--name-only', commit, base]).stdout.split('\n'))
    return _cache[key]


def fresh_paths(rec, base):
    """the paths of a fact that did not change since its commit (no commit: all of them)."""
    return [p for p in rec['paths'] if not rec['commit'] or p not in changed(rec['commit'], base)]


def stale(rec, base):
    """docs/design/memory-system.md: a fact is stale when `git diff --quiet <commit> <base> -- <paths>` fails, any path counts."""
    return len(fresh_paths(rec, base)) < len(rec['paths'])


def matches(path, globs):
    return any(fnmatch.fnmatchcase(path, g) or path.startswith(g.rstrip('/') + '/') for g in globs)


def pick_budget(ranked):
    """the first records that fit the start block's byte budget (the line of a record: slug and up to 400 chars)."""
    out, used = [], 0
    for r in ranked:
        cost = len(r['slug']) + min(len(r['text']), 400) + 4
        if used + cost > START_BYTES:
            break
        out.append(r)
        used += cost
    return out


def hits_of(wsx, env, sub, opts, query, recs):
    done = sh([*wsx, 'memory', sub, *opts, '--scope', PID, '--json', '--', query], env=env)
    try:
        hits = json.loads(done.stdout)
    except ValueError:
        return []
    return [recs[h['slug']] for h in hits if h['slug'] in recs]


def offline_row(t, tasks, args, real):
    base = t.log[0]['base']  # ^ the plan step worked at the base of `created`; a later rebase moves task.json's base
    tree = tree_of(base)
    a, dirs, lost = read_labels(t, tree)
    b = [f for f in t.landed_files()]
    declared = t.rec.get('files') or []
    tmp = Path(tempfile.mkdtemp(prefix=f'off{t.n}-', dir=WORK / 'offline'))
    wt = None
    try:
        build_state(t, tasks, tmp)
        if real:  # ^ stale and verified-by paths are read at the task's base: a checkout of it is the project path
            wt = tmp / 'wt'
            sh(['git', '-C', str(ROOT), 'worktree', 'add', '--detach', str(wt), base])
            cfg = tmp / 'config' / 'wsx' / 'wsx.config.yml'
            cfg.write_text(cfg.read_text().replace(t.rec['project_path'], str(wt)))
        capture_store(t, tasks, tmp, args)
        recs = load_records(tmp)
        env = clean_env(XDG_STATE_HOME=str(tmp), XDG_CONFIG_HOME=str(tmp / 'config'))
        wsx = [args.wsx]
        keep = [r for r in recs.values() if any(matches(p, declared) or p in a for p in r['paths'])]  # ^ the only ones freshness is read for
        hit_files = lambda rs: [r for r in rs if any(matches(p, declared) for p in r['paths'])]
        cands = hit_files(r for r in keep if not stale(r, base))
        per_path = [{**r, 'paths': fresh_paths(r, base)} for r in keep if fresh_paths(r, base)]
        by_hits = lambda r: (-sum(matches(p, declared) for p in r['paths']), -r['updated'])
        title = t.rec.get('title') or ''
        found = hits_of(wsx, env, 'search', ['--kind', 'fact', '--limit', '50'], title, recs)
        order = {r['slug']: i for i, r in enumerate(found)}
        pc = hit_files(per_path)
        arms = {'paths': pick_budget(sorted(cands, key=by_hits)),
                'paths, per-file fresh': pick_budget(sorted(pc, key=by_hits)),
                'paths, per-file fresh, bm25 order': pick_budget(sorted(pc, key=lambda r: (order.get(r['slug'], 99), by_hits(r)))),
                'bm25 title': found[:5],
                'bm25 title, per-file fresh': [{**r, 'paths': fresh_paths(r, base)} for r in found[:5]],
                'paths, stale ignored': pick_budget(sorted(hit_files(keep), key=by_hits)),
                'recall': hits_of(wsx, env, 'recall', [], title, recs)}
        if real:
            out = sh([*wsx, *START_SUB, '--scope', PID, '--files', ','.join(declared)], env=env).stdout
            arms['start'] = [r for s, r in recs.items() if re.search(r'(?<![\w.-])' + re.escape(s) + r'(?![\w.-])', out)]
        else:
            arms['start'] = arms['paths']
        # misses of (a): why a file read is not in the start set
        have = {p for r in arms['start'] for p in r['paths']}
        causes = {}
        for f in a:
            if f in have:
                continue
            prior = [o for o in tasks.values() if o.landed_ms and o.landed_ms < t.t0 and o.revision() and f in o.landed_files()]
            holders = [r for r in recs.values() if f in r['paths']]
            if not prior:
                causes[f] = 'new'
            elif not holders:
                causes[f] = 'no record'
            elif all(stale(r, base) for r in holders):
                causes[f] = 'stale, file unchanged' if any(f in fresh_paths(r, base) for r in holders) else 'stale, file changed'
            elif not any(r in cands for r in holders):
                causes[f] = 'off files'
            else:
                causes[f] = 'cut'
        traps = trap_pairs(t, tasks, tree, set(b) | {x for x in declared if x in tree[0]})
    finally:
        if wt:
            sh(['git', '-C', str(ROOT), 'worktree', 'remove', '--force', str(wt)])
        shutil.rmtree(tmp, ignore_errors=True)
    pack = lambda rs: [{'slug': r['slug'], 'task': r['task'], 'paths': r['paths']} for r in rs]
    return {'n': t.n, 't0': t.t0, 'title': t.rec.get('title'), 'area': area_of(b), 'a': sorted(a), 'dirs': dirs,
            'lost': lost, 'b': b, 'records': len(recs), 'declared': declared, 'causes': causes,
            'traps': traps,
            'evidence': {f: [(i, t.log[i]['text'][:240]) for i in ix[:2]] for f, ix in a.items()},
            'arms': {k: pack(v) for k, v in arms.items()}}


# -- scoring -------------------------------------------------------------------------------------

def score(row, arm, label):
    s = {p for r in row['arms'][arm] for p in r['paths']}
    L = set(row[label])
    hit = len(s & L)
    return hit / len(L) if L else None, (hit / len(s) if s else 0.0)


def mean(xs):
    xs = [x for x in xs if x is not None]
    return statistics.mean(xs) if xs else float('nan')


def boot(diffs, reps=2000):
    """mean of paired differences with a seeded bootstrap 95% interval."""
    rng = random.Random(388)
    ms = sorted(mean([rng.choice(diffs) for _ in diffs]) for _ in range(reps))
    return mean(diffs), ms[int(reps * 0.025)], ms[int(reps * 0.975)]


def trap_share(rows, arm):
    tot = hit = 0
    for r in rows:
        got = {x['task'] for x in r['arms'][arm]}
        for tp in r['traps']:
            tot += 1
            hit += tp['task'] in got
    return hit, tot


def offline_report(rows, title, mode):
    print(f'\n### {title} ({len(rows)} tasks, start set = {mode})\n')
    arms = ['start', 'paths, per-file fresh', 'paths, per-file fresh, bm25 order', 'bm25 title', 'bm25 title, per-file fresh',
            'paths, stale ignored', 'recall']
    print('| arm | recall (a) | precision (a) | recall (b) | precision (b) | empty | traps in set |')
    print('|---|---|---|---|---|---|---|')
    for arm in arms:
        sa = [score(r, arm, 'a') for r in rows]
        sb = [score(r, arm, 'b') for r in rows]
        h, n = trap_share(rows, arm)
        empty = sum(not r['arms'][arm] for r in rows)
        med = statistics.median([x[0] for x in sa if x[0] is not None] or [float('nan')])
        print(f'| {arm} | {mean([x[0] for x in sa]):.2f} (median {med:.2f}) | {mean([x[1] for x in sa]):.2f} | '
              f'{mean([x[0] for x in sb]):.2f} | {mean([x[1] for x in sb]):.2f} | {empty} | {h}/{n} |')
    print('\nPaired differences of recall (a), per task, with a seeded bootstrap 95% interval (a plan step that read no repo file has no (a)):\n')
    print('| pair | mean | 95% interval | better / tied / worse |\n|---|---|---|---|')
    for x, y in (('start', 'recall'), ('paths, per-file fresh', 'start'), ('paths, stale ignored', 'start'), ('bm25 title', 'start')):
        d = [score(r, x, 'a')[0] - score(r, y, 'a')[0] for r in rows if r['a']]
        m, lo, hi = boot(d)
        print(f'| {x} minus {y} | {m:+.3f} | [{lo:+.3f}, {hi:+.3f}] | {sum(v > 0 for v in d)} / {sum(v == 0 for v in d)} / {sum(v < 0 for v in d)} |')
    print('\n| area | tasks | files read | start recall (a) | start precision (a) | start recall (b) | per-file fresh recall (a) | stale ignored recall (a) | recall-arm recall (a) | traps in start set |')
    print('|---|---|---|---|---|---|---|---|---|---|')
    for area in sorted({r['area'] for r in rows}):
        rs = [r for r in rows if r['area'] == area]
        h, n = trap_share(rs, 'start')
        rec = lambda arm, lab='a': mean([score(r, arm, lab)[0] for r in rs])
        print(f'| {area} | {len(rs)} | {mean([len(r["a"]) for r in rs]):.1f} | {rec("start"):.2f} | '
              f'{mean([score(r, "start", "a")[1] for r in rs]):.2f} | {rec("start", "b"):.2f} | {rec("paths, per-file fresh"):.2f} | '
              f'{rec("paths, stale ignored"):.2f} | {rec("recall"):.2f} | {h}/{n} |')
    causes = collections.Counter(c for r in rows for c in r['causes'].values())
    total = sum(len(r['a']) for r in rows)
    covered = total - sum(causes.values())
    print(f'\nFiles read: {total} (covered by the start set: {covered}). Not covered, by cause:')
    print('\n| cause | files | share of files read |\n|---|---|---|')
    for c in ('new', 'no record', 'stale, file unchanged', 'stale, file changed', 'off files', 'cut'):
        print(f'| {c} | {causes[c]} | {causes[c] / max(total, 1):.2f} |')


def cmd_offline(args):
    global WORK
    WORK = Path(args.work) if args.work else WORK
    (WORK / 'offline').mkdir(parents=True, exist_ok=True)
    rows_file = WORK / 'offline' / 'rows.json'
    tasks = real_tasks()
    real = not args.stub and sh([args.wsx, *START_SUB, '--help'], env=clean_env()).returncode == 0
    args.dry_run = not real  # ^ capture_store's stub, as in the dry run of the paid arms
    mode = 'wsx memory start' if real else 'STUB: declared-file match of fresh records (wsx memory start is not in this binary)'
    if args.rows:
        rows = json.loads(rows_file.read_text())
    else:
        pick = sorted(n for n, o in tasks.items() if n >= FIRST_TASK and o.landed_ms and o.plan and o.revision())
        if args.tasks:
            pick = [n for n in pick if n in args.tasks]
        with cf.ThreadPoolExecutor(args.jobs) as ex:
            rows = list(ex.map(lambda n: offline_row(tasks[n], tasks, args, real), pick))
        rows.sort(key=lambda r: r['t0'])
        rows_file.write_text(json.dumps(rows))
    cut = -(-len(rows) * 2 // 3)
    tune, held = rows[:cut], rows[cut:]
    lost = collections.Counter()
    for r in rows:
        lost.update(r['lost'])
    print(f'Labels: {sum(len(r["a"]) for r in rows)} files read in {len(rows)} plan steps; Bash lines {lost["bash"]}, '
          f'clipped by the log {lost["clipped"]}; Agent/Grep/Glob calls without arguments {lost["no_args"]}.')
    offline_report(tune, f'tuned on: first {cut} by start time', mode)
    if args.final:
        offline_report(held, f'held out: last {len(held)}', mode)
        m = mean([score(r, 'start', 'a')[0] for r in held])
        print(f'\nVerdict (held out, mean recall of the start set against the files read): {m:.2f} against 0.60: '
              f'{"MEETS" if m >= 0.60 else "MISSES"} the target ({mode}).')
    else:
        print('\nHeld-out third not shown: rerun with --final once the rules are fixed.')
    for n in args.show or []:
        r = next(x for x in rows if x['n'] == n)
        print(f'\n#{n} {r["title"]} ({r["area"]})\n  read: {len(r["a"])} files, directories only: {r["dirs"]}\n  landed: {r["b"]}')
        for f, ev in r['evidence'].items():
            print(f'  {f}: ' + ' | '.join(f'log line {i + 1}: {txt}' for i, txt in ev))
        print(f'  traps: {r["traps"]}')


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)
    sub.add_parser('pick').set_defaults(fn=cmd_pick)
    r = sub.add_parser('run')
    r.add_argument('--arm', choices=['baseline', 'memory', 'memory-cheap'], required=True)
    r.add_argument('--tasks', type=int, nargs='*')
    r.add_argument('--agent', default='claude', help='the agent command (a fake in the dry run)')
    r.add_argument('--wsx', default=str(ROOT / 'target/debug/wsx'))
    r.add_argument('--dry-run', action='store_true')
    r.add_argument('--cap', type=float, default=CAP_USD)
    r.add_argument('--jobs', type=int, default=3)
    r.add_argument('--force', action='store_true')
    r.add_argument('--keep', action='store_true', help='keep the run folder (state, shim, trace) for a look')
    r.set_defaults(fn=cmd_run)
    j = sub.add_parser('judge')
    j.add_argument('--dry-run', action='store_true')
    j.add_argument('--force', action='store_true')
    j.set_defaults(fn=cmd_judge)
    sub.add_parser('report').set_defaults(fn=cmd_report)
    lv = sub.add_parser('live')
    lv.add_argument('--cutoff-ms', type=int, default=0)
    lv.add_argument('--cutoff-task', type=int, default=0, help='split at the landing of this task (#384 or #380)')
    lv.set_defaults(fn=cmd_live)
    sub.add_parser('mpick').set_defaults(fn=cmd_mpick)
    mp = sub.add_parser('mproject')
    mp.add_argument('--recall-uses', type=float, default=1.0, help='memory calls per reply when no replay ran')
    mp.set_defaults(fn=cmd_mproject)
    mr = sub.add_parser('mrun')
    mr.add_argument('--arm', choices=list(ARMS), required=True)
    for a, kw in (('--agent', {'default': 'claude'}), ('--wsx', {'default': str(ROOT / 'target/debug/wsx')}), ('--cap', {'type': float, 'default': CAP_USD}),
                  ('--jobs', {'type': int, 'default': 3})):
        mr.add_argument(a, **kw)
    mr.add_argument('--dry-run', action='store_true')
    mr.add_argument('--force', action='store_true')
    mr.set_defaults(fn=cmd_mrun)
    mj = sub.add_parser('mjudge')
    mj.add_argument('--dry-run', action='store_true')
    mj.add_argument('--force', action='store_true')
    mj.set_defaults(fn=cmd_mjudge)
    mrep = sub.add_parser('mreport')
    mrep.add_argument('--recall-uses', type=float, default=1.0)
    mrep.set_defaults(fn=cmd_mreport)
    of = sub.add_parser('offline', help='free retrieval score of the start set on every landed task')
    of.add_argument('--wsx', default=str(ROOT / 'target/debug/wsx'))
    of.add_argument('--work', help='work folder (default .work/memory-eval)')
    of.add_argument('--tasks', type=int, nargs='*')
    of.add_argument('--jobs', type=int, default=4)
    of.add_argument('--stub', action='store_true', help='score the stub even when the binary has `memory start`')
    of.add_argument('--rows', action='store_true', help='score the saved rows again, build nothing')
    of.add_argument('--final', action='store_true', help='also show the held-out third and the verdict')
    of.add_argument('--show', type=int, nargs='*', help='print the labels and the log lines behind them')
    of.set_defaults(fn=cmd_offline)
    rc = sub.add_parser('recall-cheap')
    rc.add_argument('--wsx', required=True)
    rc.add_argument('--home', required=True)
    rc.add_argument('--pid', required=True)
    rc.add_argument('--out', required=True)
    rc.add_argument('question', nargs='+')
    rc.set_defaults(fn=cmd_recall_cheap)
    args = ap.parse_args()
    args.fn(args)


if __name__ == '__main__':
    main()
