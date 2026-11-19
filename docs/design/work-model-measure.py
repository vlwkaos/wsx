#!/usr/bin/env python3
"""Work model: does cutting work into tasks cost quality? Totals from the task and chat records.

ref: docs/design/work-model.md (the numbers and what they decide)

    python3 -I docs/design/work-model-measure.py [--since 2026-11-06] [--project wsx] [--tasks wsx#540,wsx#541] [--no-git] [--state ~/.local/state/wsx]

--tasks keeps only the named tasks: the two arms of the experiment in work-model.md run it once each.

Read-only. Prints totals only, never a message's text. Reads tasks/<project-id>/<n>/{task.json,log.jsonl},
tasks/<project-id>/checks.jsonl, tasks/chats/<name>/log.jsonl and the daemon's state.json; `git worktree list`
and `git log` in each project unless --no-git.

How it reads the records:
- A task log's `message` has no author. An add is classed by the manager turn that contains its time (a chat
  log's `delivered` to `reply`), and the turn by what started it: `owner`, another chat (`peer`), or `events`.
  `outside` is an add in no manager turn: the owner's own add, or a turn the log does not hold.
- First result = the task's first `revision`. An add after it from an owner turn or outside is `owner add`
  (a correction or a follow-up: the records cannot tell which); from an event turn it is `sent back`.
- A successor of landed task A is a task of the same project created within 72 h of A's land that names
  `#A` in its prompt and shares a changed path or a `--files` glob with it.
- Repeated text = 5-word runs of a prompt that the same chat already wrote to a worker, or that the owner
  wrote in that chat, in the 14 days before. A lower bound: the owner writes Korean, prompts are English.
"""
import bisect, collections as C, datetime as D, fnmatch, glob, json, os, re, statistics, subprocess, sys

ARGS = sys.argv[1:]
def arg(name, default):
    return ARGS[ARGS.index(name) + 1] if name in ARGS else default
STATE = os.path.expanduser(arg('--state', '~/.local/state/wsx'))
TASKS = os.path.join(STATE, 'tasks')
SINCE = int(D.datetime.strptime(arg('--since', '2000-01-01'), '%Y-%m-%d').astimezone().timestamp() * 1000)
ONLY = arg('--project', None)
NAMED = set(filter(None, arg('--tasks', '').split(',')))
GIT = '--no-git' not in ARGS
H, DAY = 3600_000, 86_400_000

def events(path):  # ^ a torn or foreign line is skipped, as in task-time-cost-measure.py
    out = []
    for name in (path.replace('log.jsonl', 'log.1.jsonl'), path):
        if name != path and name == path or not os.path.exists(name):
            continue
        for line in open(name, errors='replace'):
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out
def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else float('nan')
def p90(xs):
    xs = sorted(x for x in xs if x is not None)
    return xs[int(len(xs) * .9)] if xs else float('nan')
def pct(a, b):
    return f'{100 * a / b:.0f}%' if b else '-'
def runs(text, n=5):
    w = re.findall(r'\S+', text.lower())
    return {' '.join(w[i:i + n]) for i in range(len(w) - n + 1)}
def table(title, head, rows):
    print(f'\n## {title}')
    cells = [head] + [[f'{c:.1f}' if isinstance(c, float) else str(c) for c in r] for r in rows]
    wide = [max(len(r[i]) for r in cells) for i in range(len(head))]
    for r in cells:
        print('  '.join(c.ljust(wide[i]) if i == 0 else c.rjust(wide[i]) for i, c in enumerate(r)))

# --- chats: turns, owner text, messages between chats
turns, owner_text, sent, peer_in, prunes = [], C.defaultdict(list), [], [], 0
def peer(who):  # ^ `manager:<name>` is the wire name before chats, `chat:<name>` since
    m = re.match(r'(?:manager|chat)(-reply)?:(.+)', who)
    return (m.group(2), bool(m.group(1))) if m else None
for path in glob.glob(os.path.join(TASKS, 'chats', '*', 'log.jsonl')):
    chat, sender, start = path.split(os.sep)[-2], {}, None
    for e in events(path):
        k = e.get('kind')
        if k == 'message':
            who = str(e.get('from'))
            sender[e.get('id')] = who
            if who == 'owner':
                owner_text[chat].append((e['unix_ms'], e.get('text', '')))
            elif peer(who):
                peer_in.append((e['unix_ms'], chat, *peer(who)))
            elif who == 'events' and e.get('text', '').startswith('pruned:'):
                prunes += 1
        elif k == 'delivered':
            who = [sender.get(i, '') for i in e.get('ids', [])]
            start = (e['unix_ms'], 'owner' if 'owner' in who else 'peer' if any(peer(w) for w in who) else 'events',
                     next((peer(w)[0] for w in who if peer(w)), None))
        elif k == 'reply' and start:
            turns.append((start[0], e['unix_ms'], chat, start[1], start[2])); start = None
        elif k == 'sent':
            sent.append((e['unix_ms'], chat, str(e.get('to', '')).split(':')[-1], bool(e.get('reply'))))
turns.sort()
STARTS = [t[0] for t in turns]
def turn_at(ms, chat):
    hit = None
    for t in turns[max(0, bisect.bisect_right(STARTS, ms + 2000) - 60):bisect.bisect_right(STARTS, ms + 2000)]:
        if t[0] - 2000 <= ms <= t[1] + 5000:
            if t[2] == chat:
                return t
            hit = hit or t
    return hit

# --- tasks
tasks, by_key, chat_home = [], {}, {}
for path in glob.glob(os.path.join(TASKS, '*', 'home.json')):
    try:
        chat_home[json.load(open(path))['chat']] = path.split(os.sep)[-2]
    except (ValueError, KeyError):
        pass
for path in sorted(glob.glob(os.path.join(TASKS, '*', '[0-9]*', 'task.json'))):
    try:
        rec = json.load(open(path))
    except ValueError:
        continue
    if ONLY and rec.get('project') != ONLY:
        continue
    by = str(rec.get('created_by') or '')
    t = dict(project=rec['project'], pid=rec['project_id'], id=rec['id'], chat=by[5:] if by.startswith('chat:') else None,
             by=by or 'none', small=bool(rec.get('small')), parent=rec.get('parent'), state=rec['state'], path=rec.get('working_copy'),
             main=rec.get('project_path'), created=rec['created_unix_ms'], paths=set(rec.get('paths') or []), files=rec.get('files') or [],
             adapter=rec.get('adapter'), revs=[], adds=[], results=[], landed=None, closed=None, discarded=False, prompt='',
             rotated=0, conflicts=0, holds=[], hand=0, trial=0, asks=0, cost=0)
    for e in events(os.path.join(os.path.dirname(path), 'log.jsonl')):
        k, ms = e.get('kind'), e.get('unix_ms', 0)
        if k == 'revision': t['revs'].append(ms)
        elif k == 'result':
            u = e.get('usage') or {}
            t['cost'] += u.get('cost_micro_usd', 0)
            t['results'].append((ms, e.get('trigger'), e.get('step'), e.get('status'), u))
        elif k == 'ping': t['cost'] += (e.get('usage') or {}).get('cost_micro_usd', 0)
        elif k == 'message':
            if e.get('mode') == 'prompt': t['prompt'] = e.get('text', '')
            else:
                turn = turn_at(ms, t['chat'])
                t['adds'].append((ms, turn[3] if turn else 'outside', e.get('text', ''), (ms - turn[0]) / 60000 if turn else None))
        elif k == 'landed': t['landed'] = ms
        elif k == 'closed': t['closed'], t['discarded'] = ms, bool(e.get('discarded'))
        elif k == 'rotated': t['rotated'] += 1
        elif k == 'conflict': t['conflicts'] += 1
        elif k == 'hold': t['holds'].append(ms)
        elif k in ('hand', 'handback'): t['hand'] += 1
        elif k == 'trial': t['trial'] += 1
        elif k == 'blocked': t['asks'] += 1
    born = turn_at(t['created'], t['chat'])
    t['born'], t['asker'] = (born[3], born[4]) if born else ('outside', None)
    t['first'] = t['revs'][0] if t['revs'] else None
    t['pre'] = [a for a in t['adds'] if not t['first'] or a[0] < t['first']]
    t['post'] = [a for a in t['adds'] if t['first'] and a[0] >= t['first']]
    t['owner_add'] = any(a[1] in ('owner', 'outside') for a in t['post'])
    t['sent_back'] = sum(a[1] == 'events' for a in t['post'])
    t['peer_add'] = any(a[1] == 'peer' for a in t['post'])
    t['life'] = (t['landed'] - t['created']) / 60000 if t['landed'] else None
    by_key[(t['pid'], t['id'])] = t
    tasks.append(t)

# ^ successors, by the strict rule and by each half of it
def meets(a, b):
    return bool(a['paths'] & b['paths']) or any(fnmatch.fnmatch(p, g) for g in b['files'] for p in a['paths']) \
        or any(fnmatch.fnmatch(p, g) for g in a['files'] for p in b['paths'])
landed = sorted((t for t in tasks if t['landed']), key=lambda t: t['landed'])
inside = lambda a: {p for p in a['paths'] if any(fnmatch.fnmatch(p, g) for g in a['files'])}  # ^ changed paths in its own --files
for b in tasks:
    b['pred'], b['open_with'], b['loose'] = None, 0, set()
    named = {int(n) for n in re.findall(r'#(\d{1,4})\b', b['prompt'])}
    near = [a for a in landed if a['pid'] == b['pid'] and a['id'] != b['id'] and 0 <= b['created'] - a['landed'] <= 72 * H]
    day = [a for a in near if b['created'] - a['landed'] <= 24 * H and a['chat'] == b['chat']]
    hit = lambda paths: any(fnmatch.fnmatch(p, g) for g in b['files'] for p in paths)
    b['loose'] = {k for k, v in [('names', any(a['id'] in named for a in near)), ('path', any(meets(a, b) for a in near)), ('files', bool(b['files'])),
                                 ('any path, 24 h', any(hit(a['paths']) for a in day)), ('own area, 24 h', any(hit(inside(a)) for a in day)),
                                 ('own area and named, 24 h', any(hit(inside(a)) and a['id'] in named for a in day))] if v}
    strict = [a for a in near if a['id'] in named and meets(a, b)]
    if strict:
        b['pred'] = strict[-1]
        strict[-1].setdefault('succ', []).append(b)
    b['open_with'] = sum(1 for a in tasks if a is not b and a['pid'] == b['pid'] and a['chat'] == b['chat'] and a['created'] < b['created']
                         and (a['closed'] or a['landed'] or 1 << 62) > b['created'])
tasks = [t for t in tasks if t['created'] >= SINCE and (not NAMED or f"{t['project']}#{t['id']}" in NAMED)]
if not tasks:
    sys.exit('no task matches')

# ^ text a prompt repeats: the same chat's earlier words to workers, and the owner's words in that chat
feed = []
for t in tasks:
    feed.append((t['created'], 0, t['chat'], 'prompt', t, t['prompt']))
    feed += [(a[0], 1, t['chat'], 'add', t, a[2]) for a in t['adds']]
feed += [(ms, 2, chat, 'owner', None, text) for chat, rows in owner_text.items() for ms, text in rows]
seen = {'worker': C.defaultdict(dict), 'owner': C.defaultdict(dict)}
for ms, _, chat, kind, t, text in sorted(feed, key=lambda f: f[:2]):
    r = runs(text)
    if kind == 'prompt' and r:
        t['again'] = sum(1 for x in r if ms - seen['worker'][chat].get(x, -1e18) <= 14 * DAY) / len(r)
        t['owner_said'] = sum(1 for x in r if ms - seen['owner'][chat].get(x, -1e18) <= 14 * DAY) / len(r)
    for x in r:
        seen['owner' if kind == 'owner' else 'worker'][chat][x] = ms

def span(t, lo, hi):  # ^ the worker turns between two times: dollars, model minutes, output tokens
    us = [u for ms, *_, u in t['results'] if lo < ms <= hi + 60000]
    return (sum(u.get('cost_micro_usd', 0) for u in us) / 1e6, sum(u.get('duration_ms', 0) for u in us) / 60000,
            sum(u.get('output_tokens', 0) for u in us))
def row(name, ts):
    n = len(ts)
    if not n:
        return [name, 0] + ['-'] * 10
    done = [t for t in ts if t['first']]
    return [name, n, pct(sum(1 for t in ts if t['landed']), n), med([len(t['revs']) for t in ts]), p90([len(t['revs']) for t in ts]),
            f"{sum(len(t['pre']) for t in ts) / n:.2f}", pct(sum(t['owner_add'] for t in done), len(done)),
            pct(sum(1 for t in done if t['sent_back']), len(done)), f"{med([t['life'] for t in ts]):.0f}", f"{p90([t['life'] for t in ts]):.0f}",
            f"{med([t['cost'] / 1e6 for t in ts]):.2f}", f"{sum(t['cost'] for t in ts) / 1e6:.0f}"]
HEAD = ['tasks', 'n', 'landed', 'revs p50', 'p90', 'adds before 1st result', 'owner add after', 'sent back', 'min to land p50', 'p90', '$ p50', '$ sum']

print(f"tasks {len(tasks)} in {len({t['pid'] for t in tasks})} projects, created {D.datetime.fromtimestamp(min(t['created'] for t in tasks) / 1000):%Y-%m-%d}"
      f" to {D.datetime.fromtimestamp(max(t['created'] for t in tasks) / 1000):%Y-%m-%d}; manager turns {len(turns)} ({dict(C.Counter(t[3] for t in turns))})")
top = [p for p, _ in C.Counter(t['project'] for t in tasks).most_common(5)]
table('T1 per task, by project', HEAD, [row('all', tasks)] + [row(p, [t for t in tasks if t['project'] == p]) for p in top])
done = [t for t in tasks if t['first']]
print(f"adds: {sum(len(t['adds']) for t in tasks)} in all, {sum(len(t['pre']) for t in tasks)} before the first result;"
      f" after it by turn: {dict(C.Counter(a[1] for t in tasks for a in t['post']))}")
print(f"adds before the first result by turn: {dict(C.Counter(a[1] for t in tasks for a in t['pre']))};"
      f" owner's message to the add it caused, min p50 {med([a[3] for t in tasks for a in t['adds'] if a[1] == 'owner']):.1f} p90 {p90([a[3] for t in tasks for a in t['adds'] if a[1] == 'owner']):.1f}")
print(f"open now: {dict(C.Counter(t['project'] for t in tasks if t['state'] not in ('landed', 'closed')))}")
print(f"send-back rounds per task with a first result: mean {sum(t['sent_back'] for t in done) / max(len(done), 1):.2f}, tasks with 2+: {sum(1 for t in done if t['sent_back'] >= 2)}")

size = lambda t: len(t['prompt'].encode())
table('T2 prompt size and repeated text', ['tasks', 'n', 'prompt B p50', 'p90', 'adds before 1st result B p50 (when any)', 'tasks with such adds',
                                           'repeats own earlier text p50', 'p90', "repeats owner's text p50", 'p90'],
      [[name, len(ts), med([size(t) for t in ts]), p90([size(t) for t in ts]), med([sum(len(a[2].encode()) for a in t['pre']) for t in ts if t['pre']]),
        pct(sum(1 for t in ts if t['pre']), len(ts)), pct(med([t.get('again') for t in ts]), 1), pct(p90([t.get('again') for t in ts]), 1),
        pct(med([t.get('owner_said') for t in ts]), 1), pct(p90([t.get('owner_said') for t in ts]), 1)]
       for name, ts in [('all', tasks), ('born in an owner turn', [t for t in tasks if t['born'] == 'owner']),
                        ('born in an event turn', [t for t in tasks if t['born'] == 'events']), ('successor', [t for t in tasks if t['pred']])]])
firsts = [t['results'][0][4] for t in tasks if t['results']]
print(f"first worker result: context tokens p50 {med([u.get('context_tokens') for u in firsts]):.0f}, $ p50 {med([u.get('cost_micro_usd', 0) / 1e6 for u in firsts]):.2f};"
      f" all first results are {pct(sum(u.get('cost_micro_usd', 0) for u in firsts), sum(t['cost'] for t in tasks))} of worker spend")
to_first = [span(t, t['created'], t['first']) for t in done]
print(f"up to the first result: {pct(sum(s[0] for s in to_first) * 1e6, sum(t['cost'] for t in tasks))} of worker spend, $ p50 {med([s[0] for s in to_first]):.2f}")

checks = C.Counter()
for path in glob.glob(os.path.join(TASKS, '*', 'checks.jsonl')):
    last = {}
    for e in events(path):
        if 'id' in e and (path.split(os.sep)[-2], e.get('task')) in by_key and by_key[(path.split(os.sep)[-2], e.get('task'))]['created'] >= SINCE:
            last[e['id']] = e.get('state', 'open')
    checks.update(last.values())
reverts = 0
if GIT:
    for main in {t['main'] for t in tasks if t['main'] and os.path.isdir(t['main'])}:
        out = subprocess.run(['git', '-C', main, 'log', '--oneline', '-i', '--grep=^revert', f'--since={max(SINCE, 1) // 1000}'], capture_output=True, text=True)
        reverts += len(out.stdout.splitlines())
print('\n## T3 the first result missed, or the work was redone')
print(f"tasks with a first result {len(done)}: owner add after it {sum(t['owner_add'] for t in done)} ({pct(sum(t['owner_add'] for t in done), len(done))}),"
      f" sent back by the manager {sum(1 for t in done if t['sent_back'])} ({pct(sum(1 for t in done if t['sent_back']), len(done))}),"
      f" add from another chat's turn {sum(t['peer_add'] for t in done)}")
print(f"closed with --discard: {sum(t['discarded'] for t in tasks)} ({sum(1 for t in tasks if t['discarded'] and t['first'])} after a first result)")
print(f"To check items by last state: {dict(checks)}")
print(f"landed tasks {sum(1 for t in tasks if t['landed'])}: with a successor in 72 h {sum(1 for t in tasks if t.get('succ'))};"
      f" successors {sum(1 for t in tasks if t['pred'])} (born in: {dict(C.Counter(t['born'] for t in tasks if t['pred']))});"
      f" looser: names a task landed in 72 h {sum('names' in t['loose'] for t in tasks)}, shares a path with one {sum('path' in t['loose'] for t in tasks)}")
print(f"new tasks with --files {sum('files' in t['loose'] for t in tasks)}; their --files meet a task of the same chat landed in the 24 h before:"
      f" any changed path {sum('any path, 24 h' in t['loose'] for t in tasks)}, a changed path inside that task's own --files {sum('own area, 24 h' in t['loose'] for t in tasks)},"
      f" the same and the prompt names it {sum('own area and named, 24 h' in t['loose'] for t in tasks)}")
gap = [(t['created'] - t['pred']['landed']) / H for t in tasks if t['pred']]
print(f"hours from a land to its successor's start: p50 {med(gap):.1f} p75 {(sorted(gap) or [float('nan')])[int(len(gap) * .75) if gap else 0]:.1f} p90 {p90(gap):.1f}; within 24 h {pct(sum(g <= 24 for g in gap), len(gap))}")
print(f"commits whose subject starts with 'revert': {reverts if GIT else 'not read'}")

kinds = [('small', lambda t: t['small']), ('full', lambda t: not t['small']), ('1 revision', lambda t: len(t['revs']) == 1),
         ('2-3 revisions', lambda t: 2 <= len(t['revs']) <= 3), ('4+ revisions', lambda t: len(t['revs']) >= 4),
         ('born in an owner turn', lambda t: t['born'] == 'owner'), ('born in an event turn', lambda t: t['born'] == 'events'),
         ("born in another chat's turn", lambda t: t['born'] == 'peer'), ('successor of a landed task', lambda t: t['pred']),
         ('no predecessor', lambda t: not t['pred']), ('0-1 open in its chat at start', lambda t: t['open_with'] <= 1),
         ('2-4 open', lambda t: 2 <= t['open_with'] <= 4), ('5+ open', lambda t: t['open_with'] >= 5), ('child (--into)', lambda t: t['parent'])]
table('T4 task kinds', HEAD, [row(name, [t for t in tasks if f(t)]) for name, f in kinds])
table('T4b by adapter', HEAD, [row(a or 'none', [t for t in tasks if t['adapter'] == a]) for a, _ in C.Counter(t['adapter'] for t in tasks).most_common()])
cross = [t for t in tasks if t['born'] == 'peer' and t['asker'] and chat_home.get(t['asker']) not in (None, t['pid'])]
other = [t for t in tasks if t['chat'] and chat_home.get(t['chat']) not in (None, t['pid'])]
since = lambda rows: [r for r in rows if r[0] >= SINCE]
pairs = [s for s in since(sent) if chat_home.get(s[1]) != chat_home.get(s[2])]
waits = []
for ms, chat, who, is_reply in since(peer_in):
    if not is_reply:
        back = [s[0] for s in sent if s[1] == chat and s[2] == who and s[3] and s[0] > ms]
        waits.append((min(back) - ms) / 60000 if back else None)
told = [any(s[1] == t['chat'] and s[2] == t['asker'] and 0 <= s[0] - t['landed'] <= H for s in sent) for t in cross if t['landed']]
table('T4c across projects', HEAD, [row("born in another project's manager's turn", cross), row("created by another project's chat", other)])
print(f"messages between chats of different projects: {len(pairs)} sent, {sum(s[3] for s in pairs)} of them replies;"
      f" asks received {len(waits)}, answered with a reply {sum(1 for w in waits if w is not None)}, min to the reply p50 {med(waits):.1f} p90 {p90(waits):.0f};"
      f" landed tasks another project's manager asked for {len(told)}, a message went back to the asker within 1 h of the land for {sum(told)}")

kept, fresh, any_first = C.defaultdict(list), [], []
for t in tasks:
    marks = t['revs'] + [1 << 62]
    for i, lo in enumerate(t['revs']):
        asks = [a for a in t['post'] if lo <= a[0] < marks[i + 1]]
        if asks and marks[i + 1] < 1 << 62:
            nxt = [a for a in t['post'] if marks[i + 1] <= a[0] < (marks + [1 << 62])[i + 2]]
            kind = 'owner' if asks[0][1] in ('owner', 'outside') else 'manager' if asks[0][1] == 'events' else 'peer'
            kept[kind].append(((marks[i + 1] - asks[0][0]) / 60000, *span(t, asks[0][0], marks[i + 1]), any(a[1] in ('owner', 'outside') for a in nxt)))
    if t['first']:
        one = ((t['first'] - t['created']) / 60000, *span(t, t['created'], t['first']),
               any(a[1] in ('owner', 'outside') and a[0] < (t['revs'] + [1 << 62])[1] for a in t['post']))
        (fresh if t['pred'] else any_first).append(one)
def arm(name, xs):
    return [name, len(xs), f'{med([x[0] for x in xs]):.0f}', f'{med([x[2] for x in xs]):.1f}', f'{med([x[1] for x in xs]):.2f}', f'{med([x[3] for x in xs]):.0f}',
            f'{1000 * sum(x[1] for x in xs) / max(sum(x[3] for x in xs), 1):.3f}', pct(sum(x[4] for x in xs), len(xs))]
table('T5 one more result: the same worker conversation or a fresh worker',
      ['how the next result was made', 'n', 'min p50', 'model min p50', '$ p50', 'output tokens p50', '$ per 1K output', 'owner add right after'],
      [arm("add to an open task, owner's", kept['owner']), arm("add to an open task, manager's send-back", kept['manager']),
       arm('new task, successor: start to first result', fresh), arm('new task, other: start to first result', any_first)])
gaps = C.defaultdict(list)
for t in tasks:
    for (prev, *_), (ms, trig, _, _, u) in zip(t['results'], t['results'][1:]):
        if trig == 'message':
            g = (ms - u.get('duration_ms', 0) - prev) / 60000
            gaps['under 5 min' if g < 5 else '5-60 min' if g < 60 else '1-24 h' if g < 1440 else 'over 24 h'].append((u.get('cost_micro_usd', 0) / 1e6, u.get('context_tokens')))
table('T5b a turn an add started, by the pause before it', ['pause', 'turns', '$ p50', '$ p90', 'context tokens p50'],
      [[g, len(v), f'{med([x[0] for x in v]):.2f}', f'{p90([x[0] for x in v]):.2f}', f'{med([x[1] for x in v]):.0f}'] for g, v in gaps.items()])
rot = [t for t in tasks if t['rotated']]
peak = [max((u.get('context_tokens') or 0 for *_, u in t['results']), default=0) for t in tasks if t['results']]
print(f"worker conversations replaced by a fresh one (`rotated`): {sum(t['rotated'] for t in tasks)} times in {len(rot)} tasks ({pct(len(rot), len(tasks))});"
      f" among 4+ revision tasks {pct(sum(1 for t in rot if len(t['revs']) >= 4), sum(1 for t in tasks if len(t['revs']) >= 4))};"
      f" peak context tokens p50 {med(peak):.0f} p90 {p90(peak):.0f}")
long = [t for t in tasks if len(t['revs']) >= 4]
print(f"4+ revision tasks with an owner add after a result: {pct(sum(t['owner_add'] for t in long if t['rotated']), sum(1 for t in long if t['rotated']))} of the rotated,"
      f" {pct(sum(t['owner_add'] for t in long if not t['rotated']), sum(1 for t in long if not t['rotated']))} of the others")
wait = lambda ts: med([(t['landed'] - max(r[0] for r in t['results'] if r[0] <= t['landed'])) / 60000 for t in ts if t['landed'] and any(r[0] <= t['landed'] for r in t['results'])])
print(f"last worker result to land, min p50: held {wait([t for t in tasks if t['holds']]):.0f} (n {sum(1 for t in tasks if t['holds'] and t['landed'])}),"
      f" not held {wait([t for t in tasks if not t['holds']]):.0f};"
      f" land to copy removed, min p50 {med([(t['closed'] - t['landed']) / 60000 for t in tasks if t['landed'] and t['closed']]):.1f}")
print(f"tasks with hand {sum(1 for t in tasks if t['hand'])}, with trial {sum(1 for t in tasks if t['trial'])}, with a conflict {sum(1 for t in tasks if t['conflicts'])},"
      f" where the worker asked (`blocked`) {sum(1 for t in tasks if t['asks'])}")

print('\n## T6 worktrees and sessions now')
try:
    daemon = json.load(open(os.path.join(STATE, 'state.json')))
except (OSError, ValueError) as error:
    daemon = None
    print(f'state.json not read: {error}')
if daemon:
    name = {p['id']: p for p in daemon['projects']}
    tree = {w['id']: w for w in daemon['worktrees']}
    open_copy = {t['path'] for t in by_key.values() if t['state'] not in ('landed', 'closed')}
    rows, now = C.defaultdict(lambda: C.Counter()), D.datetime.now().timestamp()
    ages = C.defaultdict(list)
    for w in daemon['worktrees']:
        p = name[w['project_id']]
        kind = 'main' if w['path'] == p['path'] else 'open task' if w['path'] in open_copy else 'other'
        rows[p['name']][kind] += 1
        if GIT and kind == 'other' and os.path.isdir(w['path']):
            out = subprocess.run(['git', '-C', w['path'], 'log', '-1', '--format=%ct'], capture_output=True, text=True).stdout.strip()
            dirty = subprocess.run(['git', '-C', w['path'], 'status', '--porcelain'], capture_output=True, text=True).stdout.strip()
            if out:
                ages[p['name']].append((now - int(out)) / 86400)
            rows[p['name']]['other, uncommitted'] += bool(dirty)
    for s in daemon['sessions']:
        label = s.get('label') or ''
        kind = 'task session' if re.match(r'task-\d+$', label) else 'hand session' if re.match(r'task-\d+-hand$', label) else \
            'host session' if label.startswith('task-') else "owner's session"
        rows[name[tree[s['worktree_id']]['project_id']]['name']][kind] += 1
    cols = ['main', 'open task', 'other', 'other, uncommitted', 'task session', 'hand session', 'host session', "owner's session"]
    table('by project', ['project'] + cols + ['other: days since last commit p50', 'max'],
          [[p] + [c[k] for k in cols] + [f'{med(ages[p]):.0f}' if ages[p] else '-', f'{max(ages[p]):.0f}' if ages[p] else '-']
           for p, c in sorted(rows.items(), key=lambda r: -sum(r[1].values())) if not ONLY or p == ONLY])
    print(f"in all: worktrees {len(daemon['worktrees'])}, sessions {len(daemon['sessions'])}")
print(f"task copies removed at close: {sum(1 for t in tasks if t['closed'])}; `pruned:` notices in chat logs: {prunes}")

print("""
## T7 what these records cannot show
- The owner's verdict on quality. An owner add after a result is a correction or a new wish; a To check state is the nearest record of a verdict.
- Who wrote an add. The class comes from the manager turn around it; an owner's own add and a lost turn both read `outside`.
- Context restated in other words or in another language. The repeated-text share counts exact 5-word runs only.
- Why a result missed: thin prompt, a wrong decision or a weak model look the same here.
- Work in sessions the owner opened by hand: no record but the sessions that exist now; no history of deleted worktrees before the prune notices.
- Which task another project's manager asked for: only the turn that created it links them.
- Size of the change behind a result: closed tasks' branches are gone, so cost per output token stands in for it.""")
