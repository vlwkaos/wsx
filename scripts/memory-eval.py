#!/usr/bin/env python3
"""Replay evaluation of the memory system: docs/design/memory-eval.md (method, verdict rule).

  pick     choose the tasks from the real task store         -> .work/memory-eval/picks.json
  run      replay the plan step of each pick, one arm        -> .work/memory-eval/runs/<arm>/<task>-<rep>.json
  judge    score every run blind (cause, traps)              -> .work/memory-eval/judged.json
  report   per-task tables, fidelity, noise, verdict inputs  -> stdout (markdown)
  live     before/after comparison from the real logs (confounded: different tasks)
  recall-cheap  (internal) the shim's `wsx memory recall` with a cheap model

Arms: baseline (what a worker had at the task's start), memory (+ start set, `wsx memory` and a store
cut at the task's start), memory-cheap (as memory; `recall` asks a cheap model).
Every run: its own git worktree at the task's base, its own XDG_STATE_HOME with the task store cut at
the task's start. The real store, the real memory and the real config are only read.
"""
import argparse
import collections
import concurrent.futures as cf
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
        rev = self.revision()
        out = subprocess.run(['git', '-C', str(ROOT), 'diff', '--name-only', rev['base'], rev['commit']],
                             capture_output=True, text=True)
        return [x for x in out.stdout.split('\n') if x]

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


def build_state(t, tasks, home):
    """XDG_STATE_HOME for one run: the task store as it was at t.t0."""
    root = home / 'wsx' / 'tasks' / PID
    for m, o in tasks.items():
        if o.t0 >= t.t0:
            continue
        d = root / str(m)
        d.mkdir(parents=True)
        lines = [r for r, e in zip(o.raw, o.log) if e['unix_ms'] < t.t0]
        (d / 'log.jsonl').write_text('\n'.join(lines) + '\n')
        rec = dict(o.rec)
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
    # ^ `-p wsx` and the project id come from the config: the run's own, never the owner's.
    (home / 'config' / 'wsx').mkdir(parents=True)
    (home / 'config' / 'wsx' / 'wsx.config.yml').write_text(
        f'projects:\n  - name: wsx\n    path: {t.rec["project_path"]}\n    default_branch: main\n')
    return ctx


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


def spent():
    p = WORK / 'spend.jsonl'
    return sum(json.loads(x)['usd'] for x in p.read_text().splitlines()) if p.exists() else 0.0


def spend(kind, ident, usd):
    with _lock:
        with open(WORK / 'spend.jsonl', 'a') as f:
            f.write(json.dumps({'ts': int(time.time()), 'kind': kind, 'id': ident, 'usd': usd}) + '\n')


def leak_check(t, tasks, home):
    """No future in the run's state: every log line, result, memory record and dated note is older than t.t0."""
    root = home / 'wsx' / 'tasks' / PID
    for d in root.iterdir():
        if d.name.isdigit():
            for line in (d / 'log.jsonl').read_text().splitlines():
                assert json.loads(line)['unix_ms'] < t.t0, f'#{d.name} log has a line after the start of #{t.n}'
            if (d / 'result.json').exists():
                o = tasks[int(d.name)]
                assert o.end_ms and o.end_ms < t.t0, f'#{d.name} result is not final at the start of #{t.n}'
    records = root / 'memory' / 'records.jsonl'
    if records.exists():
        for line in records.read_text().splitlines():
            m = re.match(r't(\d+)$', json.loads(line).get('slug', ''))
            assert not m or tasks[int(m.group(1))].landed_ms < t.t0, f'record {m.group(0)} lands after the start of #{t.n}'
    for name in ('decisions.md', 'decisions-archive.md'):
        f = root / 'manager' / 'notes' / name
        for line in f.read_text().splitlines() if f.exists() else []:
            when = day_ms(line[2:].split(' ')[0]) if line.startswith('- ') else None
            assert when is None or when < t.t0, f'{name} has a line dated after the start of #{t.n}'


def memory_gate(args):
    """The paid memory arm runs only on the real code of build steps 4 and 5."""
    if args.dry_run:
        return
    for sub in (CAPTURE_SUB, START_SUB):
        done = sh([args.wsx, *sub, '--help'], env=clean_env())
        if done.returncode != 0:
            sys.exit(f'refused: {args.wsx} has no `wsx {" ".join(sub)}` (build steps 4 and 5 are not in this binary); '
                     'the paid memory arm does not run on a stub')


def memory_cut(t, tasks, home, args, ctx):
    """The with-memory store of one run: step 5's capture rule on every task landed before t.t0.
    Returns (start set text, store records, store bytes)."""
    env = clean_env(XDG_STATE_HOME=str(home), XDG_CONFIG_HOME=str(home / 'config'))
    wsx = [args.wsx]
    records = 0
    for m, o in sorted(tasks.items()):
        if not o.landed_ms or o.landed_ms >= t.t0 or not o.revision():
            continue
        memo = o.result.get('memo') or ''
        if not memo.strip():
            continue
        files = o.landed_files()
        commit = o.revision()['commit']
        if args.dry_run:  # ^ the stub: dry run only
            text = (f'{memo.strip()}\nFiles: {", ".join(files[:12])}')[:4000]
            done = sh([*wsx, 'memory', 'add', '--scope', PID, '--kind', 'fact', '--slug', f't{m}', '--text-file', '-',
                       '--source', f'task:{m}', '--verified-by', f'task:{m}', '--commit', commit], input=text, env=env)
        else:
            done = sh([*wsx, *CAPTURE_SUB, '--scope', PID, '--task', str(m), '--memo', memo, '--commit', commit,
                       '--files', '\n'.join(files)], env=env)
        records += done.returncode == 0
    # ^ The rules: decisions dated before t0, already cut into the notes of the state.
    store = home / 'wsx' / 'tasks' / PID / 'memory'
    size = sum(f.stat().st_size for f in store.rglob('*') if f.is_file()) if store.exists() else 0
    files = t.rec.get('files') or []
    if args.dry_run:
        words = ' '.join(file_words(files))
        hits = sh([*wsx, 'memory', 'search', words or t.rec.get('title') or 'wsx', '--scope', PID, '--limit', '5', '--json'], env=env)
        start = 'Memory start set:\n' + (hits.stdout.strip()[:3000] if hits.returncode == 0 else '(none)')
    else:
        start = sh([*wsx, *START_SUB, '--scope', PID, '--files', ','.join(files)], env=env).stdout.strip()
    return start, records, size


def shim(run, home, arm, args):
    """`wsx` on the worker's PATH: `task search` always, `memory ...` in the memory arms only."""
    bin_dir = run / 'bin'
    bin_dir.mkdir()
    me = Path(__file__).resolve()
    real = shlex.quote(args.wsx)
    recall = (f'exec {shlex.quote(sys.executable)} {shlex.quote(str(me))} recall-cheap --wsx {real} --home {shlex.quote(str(home))} '
              f'--out {shlex.quote(str(run / "recall-cost.jsonl"))} "$@"') if arm == 'memory-cheap' else \
        f'exec {real} memory recall --scope {PID} "$@"'
    memory = '' if arm == 'baseline' else f'''
  memory)
    case "$2" in
      recall) shift 2; {recall} ;;
      search|status|list) sub=$2; shift 2; exec {real} memory "$sub" --scope {PID} "$@" ;;
    esac ;;'''
    (bin_dir / 'wsx').write_text(f'''#!/bin/sh
unset WSX_TASK_BROKER WSX_AGENT_TREE
export XDG_STATE_HOME={shlex.quote(str(home))} XDG_CONFIG_HOME={shlex.quote(str(home / 'config'))}
case "$1" in
  task) case "$2" in
    search) shift 2; exec {real} task search -p wsx "$@" ;;
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
    msgs, subs, tools, plan = set(), set(), [], None
    result = next((e for e in events if e.get('type') == 'result'), {})
    for e in events:
        if e.get('type') != 'assistant':
            continue
        m = e.get('message') or {}
        (subs if e.get('parent_tool_use_id') else msgs).add(m.get('id') or id(e))
        for c in m.get('content') or []:
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
            'explore': explore_count(tools), 'tool_counts': dict(collections.Counter(n for n, _ in tools)),
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
        ctx = build_state(task, tasks, home)
        sh(['git', '-C', str(ROOT), 'worktree', 'add', '--detach', str(wc), task.rec['base']], check=True)
        block = host_context(task, tasks, ctx)
        info = {'task': task.n, 'arm': arm, 'rep': rep, 'logged_cost': task.plan['usage']['cost_micro_usd'] / 1e6,
                'logged_calls': task.plan['usage'].get('calls'), 'model': task.rec.get('model')}
        brief = ''
        if arm != 'baseline':
            start, records, size = memory_cut(task, tasks, home, args, ctx)
            block = (block + '\n' + start).strip()
            leak_check(task, tasks, home)
            brief = ' ' + MEMORY_BRIEF
            info.update({'store_records': records, 'store_bytes': size, 'start_bytes': len(start)})
        info['context_bytes'] = len(block)
        steps = ', '.join(s['name'] for s in task.steps)
        step = task.steps[0]
        text = '\n\n'.join(x for x in [task.prompt, block, f'Step 1 of {len(task.steps)}, {step["name"]}: {step["prompt"]}\n{PLAN_END}'] if x)
        if args.dry_run:
            text += '\nMEMORY'  # ^ the fake runs `wsx memory ...` through the shim and traces each answer
        bin_dir = shim(run, home, arm, args)
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
    env = clean_env(XDG_STATE_HOME=args.home, XDG_CONFIG_HOME=f'{args.home}/config')
    question = ' '.join(args.question)
    hits = sh([args.wsx, 'memory', 'search', question, '--scope', PID, '--limit', '20', '--json'], env=env).stdout
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
        return {'traps': [], 'cause': 1, 'why': 'dry run'}, 0.0
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
    rc = sub.add_parser('recall-cheap')
    rc.add_argument('--wsx', required=True)
    rc.add_argument('--home', required=True)
    rc.add_argument('--out', required=True)
    rc.add_argument('question', nargs='+')
    rc.set_defaults(fn=cmd_recall_cheap)
    args = ap.parse_args()
    args.fn(args)


if __name__ == '__main__':
    main()
