#!/usr/bin/env python3
"""Work model: does cutting work into tasks cost quality? Totals from the task and chat records.

ref: docs/design/work-model.md (the numbers and what they decide)

    python3 -I docs/design/work-model-measure.py [--since 2026-11-06] [--project wsx] [--tasks wsx#540,wsx#541] [--no-git]
        [--sample 20] [--state ~/.local/state/wsx] [--claude ~/.claude/projects]

--tasks keeps only the named tasks: the two arms of the experiment in work-model.md run it once each.

Read-only. Prints totals only, never a message's text. Reads tasks/<project-id>/<n>/{task.json,log.jsonl},
tasks/<project-id>/{checks.jsonl,home.json}, tasks/chats/<name>/{log.jsonl,chat.json} and the daemon's
state.json; `git log` and `git status` in each project and worktree unless --no-git.

How it reads the records:
- A task log's `message` has no author. An add is classed by the turn of the task's own chat (the project's
  home chat when the task names none) that contains its time (a chat log's `delivered` to `reply`), and the
  turn by what started it: `owner`, another chat (`peer`), or `events`. The manager wrote every such add.
  `outside` is an add in no turn of that chat: the owner's own add, another chat's add, or a turn the log
  does not hold.
- First result = the task's first `revision`. An add after it from an owner turn or outside is `owner add`
  (a correction or a follow-up: the records cannot tell which); from an event turn it is `sent back`.
- A successor of landed task A is a task of the same project created within 72 h of A's land that names
  `#A` in its prompt and shares a changed path or a `--files` glob with it. The 14-day line drops the 72 h.
- Two chats are of different projects when their `chat.json` `repos` share none. The chat lines count every
  project, also under --project. A `sent` reply names no message: it answers the newest ask still open.
- T7 and T9 read the Claude transcripts of the newest tasks (--sample for T7, five times that for T9 and for
  "opened by a worker"): the folder under --claude named after the task's working copy, one file a conversation.
  A file is named when a Read, an edit or a shell command of the worker holds its path; that is all "used" means.
- T8 reads each project's memory/records.jsonl (the last line of a slug is the record). `stale` is the code's own
  rule (memory/start.rs `stale_of`): the record has a commit and paths, and the paths changed since. A cited
  path is "deleted" when `git log --diff-filter=D` holds it and no file ends with it now.
- Repeated text = 5-word runs of a prompt that the same chat already wrote to a worker, or that the owner
  wrote in that chat, in the 14 days before. A lower bound: the owner writes Korean, prompts are English.
"""
import bisect, collections as C, datetime as D, fnmatch, glob, itertools, json, os, re, statistics, subprocess, sys

ARGS = sys.argv[1:]
def arg(name, default):
    return ARGS[ARGS.index(name) + 1] if name in ARGS else default
STATE = os.path.expanduser(arg('--state', '~/.local/state/wsx'))
TASKS = os.path.join(STATE, 'tasks')
SINCE = int(D.datetime.strptime(arg('--since', '2000-01-01'), '%Y-%m-%d').astimezone().timestamp() * 1000)
ONLY = arg('--project', None)
NAMED = set(filter(None, arg('--tasks', '').split(',')))
GIT = '--no-git' not in ARGS
CLAUDE, SAMPLE = os.path.expanduser(arg('--claude', '~/.claude/projects')), int(arg('--sample', '20'))
H, DAY, INF = 3600_000, 86_400_000, 1 << 62
OWNER = ('owner', 'outside')  # ^ the add classes counted as the owner's

def events(path):  # ^ a torn or foreign line is skipped, as in task-time-cost-measure.py
    out = []
    for name in dict.fromkeys((path.replace('log.jsonl', 'log.1.jsonl'), path)):  # ^ the rolled log first
        if os.path.exists(name):
            for line in open(name, errors='replace'):
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass
    return out
def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else float('nan')
def q(xs, f=.9):
    xs = sorted(x for x in xs if x is not None)
    return xs[min(len(xs) - 1, int(len(xs) * f))] if xs else float('nan')
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
by_chat = C.defaultdict(list)
for turn in sorted(turns):
    by_chat[turn[2]].append(turn)
STARTS = {chat: [t[0] for t in ts] for chat, ts in by_chat.items()}
def turn_at(ms, chat):  # ^ a chat's turns are serial: only the last one started by then can hold ms
    i = bisect.bisect_right(STARTS.get(chat, []), ms + 2000)
    return by_chat[chat][i - 1] if i and ms <= by_chat[chat][i - 1][1] + 5000 else None

# --- tasks
tasks, by_key, chat_repos, home_chat = [], {}, {}, {}
for path in glob.glob(os.path.join(TASKS, 'chats', '*', 'chat.json')) + glob.glob(os.path.join(TASKS, '*', 'home.json')):
    try:
        rec, name = json.load(open(path)), path.split(os.sep)[-2]
        if path.endswith('chat.json'): chat_repos[name] = set(rec.get('repos') or [])
        else: home_chat[name] = rec['chat']
    except (ValueError, KeyError):
        pass
foreign = lambda chat, pid: chat in chat_repos and pid not in chat_repos[chat]  # ^ a chat that does not hold the project
apart = lambda a, b: not chat_repos.get(a, set()) & chat_repos.get(b, set())    # ^ two chats that share no project
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
             holds=[], rot=[], n=C.Counter(), cost=0)
    talks = t['chat'] or home_chat.get(t['pid'])
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
                turn = turn_at(ms, talks)
                t['adds'].append((ms, turn[3] if turn else 'outside', e.get('text', ''), (ms - turn[0]) / 60000 if turn else None))
        elif k == 'landed': t['landed'] = ms
        elif k == 'closed': t['closed'], t['discarded'] = ms, bool(e.get('discarded'))
        elif k == 'hold': t['holds'].append(ms)
        else:
            t['n'][k] += 1
            if k == 'rotated': t['rot'].append((ms, e.get('tokens') or 0, e.get('reason') or 'size'))  # ^ read below: rotated, conflict, hand, trial, preview, blocked
    born = turn_at(t['created'], talks)
    t['born'], t['asker'] = (born[3], born[4]) if born else ('outside', None)
    t['first'] = t['revs'][0] if t['revs'] else None
    t['pre'] = [a for a in t['adds'] if not t['first'] or a[0] < t['first']]
    t['post'] = [a for a in t['adds'] if t['first'] and a[0] >= t['first']]
    t['owner_add'] = any(a[1] in OWNER for a in t['post'])
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
    b['pred'] = None
    named = {int(n) for n in re.findall(r'#(\d{1,4})\b', b['prompt'])}
    since_land = lambda a: b['created'] - a['landed']
    far = [a for a in landed if a['pid'] == b['pid'] and a['id'] != b['id'] and 0 <= since_land(a) <= 14 * DAY]
    near = [a for a in far if since_land(a) <= 72 * H]
    day = [a for a in near if since_land(a) <= 24 * H and a['chat'] == b['chat']]
    hit = lambda paths: any(fnmatch.fnmatch(p, g) for g in b['files'] for p in paths)
    b['loose'] = {k for k, v in [('names', any(a['id'] in named for a in near)), ('path', any(meets(a, b) for a in near)), ('files', bool(b['files'])),
                                 ('any path, 24 h', any(hit(a['paths']) for a in day)), ('own area, 24 h', any(hit(inside(a)) for a in day)),
                                 ('own area and named, 24 h', any(hit(inside(a)) and a['id'] in named for a in day))] if v}
    wide = [a for a in far if a['id'] in named and meets(a, b)]
    b['wide'] = since_land(wide[-1]) / H if wide else None
    strict = [a for a in wide if a in near]
    if strict:
        b['pred'] = strict[-1]
        strict[-1].setdefault('succ', []).append(b)
    b['open_with'] = sum(1 for a in tasks if a is not b and a['pid'] == b['pid'] and a['chat'] == b['chat'] and a['created'] < b['created']
                         and (a['closed'] or a['landed'] or INF) > b['created'])
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
        return [name, 0] + ['-'] * (len(HEAD) - 2)
    done = [t for t in ts if t['first']]
    return [name, n, pct(sum(1 for t in ts if t['landed']), n), med([len(t['revs']) for t in ts]), q([len(t['revs']) for t in ts]),
            f"{sum(len(t['pre']) for t in ts) / n:.2f}", pct(sum(t['owner_add'] for t in done), len(done)),
            pct(sum(1 for t in done if t['sent_back']), len(done)), f"{med([t['life'] for t in ts]):.0f}", f"{q([t['life'] for t in ts]):.0f}",
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
      f" owner's message to the add it caused, min p50 {med([a[3] for t in tasks for a in t['adds'] if a[1] == 'owner']):.1f} p90 {q([a[3] for t in tasks for a in t['adds'] if a[1] == 'owner']):.1f}")
print(f"open now: {dict(C.Counter(t['project'] for t in tasks if t['state'] not in ('landed', 'closed')))}")
print(f"send-back rounds per task with a first result: mean {sum(t['sent_back'] for t in done) / max(len(done), 1):.2f}, tasks with 2+: {sum(1 for t in done if t['sent_back'] >= 2)}")

size = lambda t: len(t['prompt'].encode())
table('T2 prompt size and repeated text', ['tasks', 'n', 'prompt B p50', 'p90', 'adds before 1st result B p50 (when any)', 'tasks with such adds',
                                           'repeats own earlier text p50', 'p90', "repeats owner's text p50", 'p90'],
      [[name, len(ts), med([size(t) for t in ts]), q([size(t) for t in ts]), med([sum(len(a[2].encode()) for a in t['pre']) for t in ts if t['pre']]),
        pct(sum(1 for t in ts if t['pre']), len(ts)), pct(med([t.get('again') for t in ts]), 1), pct(q([t.get('again') for t in ts]), 1),
        pct(med([t.get('owner_said') for t in ts]), 1), pct(q([t.get('owner_said') for t in ts]), 1)]
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
        out = subprocess.run(['git', '-C', main, 'log', '--oneline', '-i', '--grep=^revert', f'--since={SINCE // 1000}'], capture_output=True, text=True)
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
loose = lambda k: sum(k in t['loose'] for t in tasks)
of_files = lambda k: f"{loose(k)} ({pct(loose(k), loose('files'))})"
print(f"new tasks with --files {loose('files')}; their --files meet a task of the same chat landed in the 24 h before:"
      f" any changed path {of_files('any path, 24 h')}, a changed path inside that task's own --files {of_files('own area, 24 h')},"
      f" the same and the prompt names it {of_files('own area and named, 24 h')}")
gap = [(t['created'] - t['pred']['landed']) / H for t in tasks if t['pred']]
wide = [t['wide'] for t in tasks if t['wide'] is not None]
print(f"hours from a land to its successor's start: p50 {med(gap):.1f} p75 {q(gap, .75):.1f} p90 {q(gap):.1f}; within 24 h {pct(sum(g <= 24 for g in gap), len(gap))}")
print(f"the same rule with 14 days in place of 72 h: {len(wide)} tasks, hours p50 {med(wide):.1f} p90 {q(wide):.1f};"
      f" within 24 h {pct(sum(g <= 24 for g in wide), len(wide))}, within 72 h {pct(sum(g <= 72 for g in wide), len(wide))}")
print(f"commits whose subject starts with 'revert': {reverts if GIT else 'not read'}")

kinds = [('small', lambda t: t['small']), ('full', lambda t: not t['small']), ('1 revision', lambda t: len(t['revs']) == 1),
         ('2-3 revisions', lambda t: 2 <= len(t['revs']) <= 3), ('4+ revisions', lambda t: len(t['revs']) >= 4),
         ('born in an owner turn', lambda t: t['born'] == 'owner'), ('born in an event turn', lambda t: t['born'] == 'events'),
         ("born in another chat's turn", lambda t: t['born'] == 'peer'), ('successor of a landed task', lambda t: t['pred']),
         ('no predecessor', lambda t: not t['pred']), ('0-1 open in its chat at start', lambda t: t['open_with'] <= 1),
         ('2-4 open', lambda t: 2 <= t['open_with'] <= 4), ('5+ open', lambda t: t['open_with'] >= 5), ('child (--into)', lambda t: t['parent'])]
table('T4 task kinds', HEAD, [row(name, [t for t in tasks if f(t)]) for name, f in kinds])
table('T4b by adapter', HEAD, [row(a or 'none', [t for t in tasks if t['adapter'] == a]) for a, _ in C.Counter(t['adapter'] for t in tasks).most_common()])
cross = [t for t in tasks if t['born'] == 'peer' and foreign(t['asker'], t['pid'])]
other = [t for t in tasks if foreign(t['chat'], t['pid'])]
since = lambda rows: [r for r in rows if r[0] >= SINCE]
pairs = [s for s in since(sent) if apart(s[1], s[2])]
talk, waits = C.defaultdict(list), []
for ms, chat, who, is_reply in [r for r in since(peer_in) if not r[3]] + [s for s in sent if s[3]]:  # ^ asks received, replies sent back
    if apart(chat, who):
        talk[chat, who].append((ms, is_reply))
for rows in talk.values():  # ^ a `sent` reply names no message: it answers the newest ask still open
    asks = []
    for ms, is_reply in sorted(rows):
        if not is_reply: asks.append(ms)
        elif asks: waits.append((ms - asks.pop()) / 60000)
    waits += [None] * len(asks)
told = [any(s[1] == t['chat'] and s[2] == t['asker'] and 0 <= s[0] - t['landed'] <= H for s in sent) for t in cross if t['landed']]
table('T4c across projects', HEAD, [row("born in another project's manager's turn", cross), row("created by another project's chat", other)])
print(f"messages between chats of different projects: {len(pairs)} sent, {sum(s[3] for s in pairs)} of them replies;"
      f" asks received {len(waits)}, answered with a reply {sum(1 for w in waits if w is not None)}, min to the reply p50 {med(waits):.1f} p90 {q(waits):.0f};"
      f" landed tasks another project's manager asked for {len(told)}, any message went to the asker within 1 h of the land for {sum(told)} (an upper bound: the message need not be about the task)")

kept, fresh, any_first = C.defaultdict(list), [], []
for t in tasks:
    marks = t['revs'] + [INF, INF]
    for i, lo in enumerate(t['revs']):
        asks = [a for a in t['post'] if lo <= a[0] < marks[i + 1]]
        if asks and marks[i + 1] < INF:
            nxt = [a for a in t['post'] if marks[i + 1] <= a[0] < marks[i + 2]]
            kind = 'owner' if asks[0][1] in OWNER else 'manager' if asks[0][1] == 'events' else 'peer'
            kept[kind].append(((marks[i + 1] - asks[0][0]) / 60000, *span(t, asks[0][0], marks[i + 1]), any(a[1] in OWNER for a in nxt)))
    if t['first']:
        one = ((t['first'] - t['created']) / 60000, *span(t, t['created'], t['first']), any(a[1] in OWNER and a[0] < marks[1] for a in t['post']))
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
      [[g, len(v), f'{med([x[0] for x in v]):.2f}', f'{q([x[0] for x in v]):.2f}', f'{med([x[1] for x in v]):.0f}'] for g, v in gaps.items()])
rot = [t for t in tasks if t['n']['rotated']]
peak = [max((u.get('context_tokens') or 0 for *_, u in t['results']), default=0) for t in tasks if t['results']]
print(f"worker conversations replaced by a fresh one (`rotated`): {sum(t['n']['rotated'] for t in tasks)} times in {len(rot)} tasks ({pct(len(rot), len(tasks))});"
      f" among 4+ revision tasks {pct(sum(1 for t in rot if len(t['revs']) >= 4), sum(1 for t in tasks if len(t['revs']) >= 4))};"
      f" peak context tokens p50 {med(peak):.0f} p90 {q(peak):.0f}")
long = [t for t in tasks if len(t['revs']) >= 4]
print(f"4+ revision tasks with an owner add after a result: {pct(sum(t['owner_add'] for t in long if t['n']['rotated']), sum(1 for t in long if t['n']['rotated']))} of the rotated,"
      f" {pct(sum(t['owner_add'] for t in long if not t['n']['rotated']), sum(1 for t in long if not t['n']['rotated']))} of the others")
wait = lambda ts: med([(t['landed'] - max(r[0] for r in t['results'] if r[0] <= t['landed'])) / 60000 for t in ts if t['landed'] and any(r[0] <= t['landed'] for r in t['results'])])
print(f"last worker result to land, min p50: held {wait([t for t in tasks if t['holds']]):.0f} (n {sum(1 for t in tasks if t['holds'] and t['landed'])}),"
      f" not held {wait([t for t in tasks if not t['holds']]):.0f};"
      f" land to copy removed, min p50 {med([(t['closed'] - t['landed']) / 60000 for t in tasks if t['landed'] and t['closed']]):.1f}")
had = lambda k: sum(1 for t in tasks if t['n'][k])
print(f"tasks with hand {had('hand')}, with preview {had('preview')}, with trial {had('trial')}, with a conflict {had('conflict')}, where the worker asked (`blocked`) {had('blocked')}")

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
    rows, now = C.defaultdict(C.Counter), D.datetime.now().timestamp()
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

# --- knowledge: what a worker's context holds, what of the stored knowledge is past, what a rotation keeps
PATHY = re.compile(r'[\w.@/-]*[\w-]\.(?:rs|md|py|mjs|js|toml|sh|css|html|yml)\b')
nb = lambda x: len((x if isinstance(x, str) else json.dumps(x, ensure_ascii=False)).encode())
def read(path):
    try:
        return open(path, errors='replace').read()
    except OSError:
        return ''
def conversations(t):  # ^ the worker's Claude transcripts, oldest first; `api-request-blob` lines repeat the messages
    out, root = [], (t['path'] or '-') + '/'
    for name in glob.glob(os.path.join(CLAUDE, re.sub(r'[^A-Za-z0-9]', '-', root[:-1]), '*.jsonl')):
        c = dict(at=None, parts=C.Counter(), text='', files=set(), entries=0, listed=0, used=set(), got=C.Counter(), first_got=None, named=set(), ctx=None)
        tool = {}
        for line in open(name, errors='replace'):
            if '"api-request-blob"' in line:
                continue
            try:
                e = json.loads(line)
            except ValueError:
                continue
            k, body = e.get('type'), (e.get('message') or {}).get('content')
            if k == 'api-request-shape' and not c['parts']['tool list']:
                text = ''.join(b.get('text', '') for b in e['shape'].get('system') or [])
                cut = max(text.find('You are a task worker'), 0) or len(text)
                c['parts'].update({"Claude Code's own prompt": nb(text[:cut]), "wsx's worker rules": nb(text[cut:]), 'tool list': nb(e['shape'].get('tools') or [])})
            elif k == 'attachment' and c['ctx'] is None:
                a = e['attachment']
                for f in a.get('files') or [] if a.get('type') == 'instructions' else []:
                    path, text = f.get('path', ''), f.get('content') or ''
                    c['files'].add(os.path.basename(path))
                    c['parts']['MEMORY.md index' if path.endswith('MEMORY.md') else 'project CLAUDE.md' if path.startswith(root) else "owner's CLAUDE.md"] += nb(text)
                    if path.endswith('MEMORY.md'):
                        c['entries'] = len(re.findall(r'^- \[', text, re.M))
                if a.get('type') != 'instructions':
                    c['parts']['skill list' if a.get('type') == 'skill_listing' else 'other lists (agents, tools, environment)'] += nb(e.get('rendered') or '')
                    c['listed'] = a.get('skillCount') or c['listed']
            elif k == 'user':
                text = body if isinstance(body, str) else ''.join(b.get('text', '') for b in body or [] if b.get('type') == 'text')
                if text and c['at'] is None:
                    c['at'], c['text'] = int(D.datetime.fromisoformat(e['timestamp'].replace('Z', '+00:00')).timestamp() * 1000), text
                for b in body or [] if isinstance(body, list) else []:
                    if b.get('type') == 'tool_result':  # ^ text only: a capture read as an image is not counted
                        got = b.get('content') or ''
                        c['got'][tool.get(b.get('tool_use_id'), 'other')] += nb(got if isinstance(got, str) else ''.join(x.get('text', '') for x in got))
            elif k == 'assistant':
                if c['ctx'] is None:
                    u = (e.get('message') or {}).get('usage') or {}
                    c['ctx'] = sum(u.get(x) or 0 for x in ('input_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens'))
                for b in body or []:
                    if b.get('type') == 'tool_use':
                        tool[b.get('id')], given = b.get('name'), b.get('input') or {}
                        if b.get('name') == 'StructuredOutput' and c['first_got'] is None:
                            c['first_got'] = sum(c['got'].values())
                        if b.get('name') == 'Skill':
                            c['used'].add(given.get('skill'))
                        c['named'] |= {f[len(root):] if f.startswith(root) else re.sub(r'^\./', '', f) for f in [given.get('file_path') or ''] + PATHY.findall(given.get('command') or '') if f}
        if c['at'] is not None:
            out.append(c)
    return sorted(out, key=lambda c: c['at'])
talks = {}
def talk(t):
    if (t['pid'], t['id']) not in talks:
        talks[t['pid'], t['id']] = conversations(t)
    return talks[t['pid'], t['id']]
wide = list(itertools.islice((t for t in sorted(tasks, key=lambda t: -t['created']) if t['first'] and talk(t)), 5 * SAMPLE))
sample = wide[:SAMPLE]
def split(text):  # ^ host.rs `first_turn`: the prompt, "Task context from the host:", the step text
    a, b = text.find('Task context from the host:'), text.find('\n\nStep ')
    b = b if b >= 0 else len(text)
    return text[:a if 0 <= a < b else b], text[a:b] if 0 <= a < b else '', text[b:]
rows, shown = C.defaultdict(list), [0, 0, 0]
for t in sample:
    first = talk(t)[0]
    prompt, context, step = split(first['text'])
    for part, n in {**first['parts'], 'task prompt and adds': nb(prompt), "host's task context (start block)": nb(context), 'step text': nb(step)}.items():
        rows[part].append(n)
    for globs in re.findall(r'^(?:Rule|Area brief) \S+ \((?:paths|files) ([^)]*)\)', context, re.M):
        shown[0] += 1
        shown[1] += any(fnmatch.fnmatch(f, g.strip()) for g in globs.split(',') for f in t['paths'])
    shown[2] += bool(re.search(r'^(?:Rule|Area brief) ', context, re.M))
print(f'\n## T7 what a worker\'s context holds at its first model call (the newest {len(sample)} tasks with a transcript)')
if sample:
    total = sum(med(v) for v in rows.values())
    table('bytes by part', ['part', 'tasks with it', 'bytes p50', 'p90', 'of the sum of p50'],
          [[part, sum(1 for x in v if x), med(v), q(v), pct(med(v), total)] for part, v in sorted(rows.items(), key=lambda r: -med(r[1]))])
    firsts_c = [talk(t)[0] for t in sample]
    mem = lambda t: {f for c in talk(t) for f in c['named'] if '/memory/' in f and not re.search(rf"/(MEMORY|task-{t['id']}-[^/]*)\.md$", f)}
    by_task = lambda f: [f(t, set().union(*(c['named'] for c in talk(t)))) for t in sample]
    ctx = med([c['ctx'] for c in firsts_c])
    print(f"sum of p50 {total:.0f} bytes; context tokens at the first call p50 {ctx:.0f}; at that rate ({total / ctx:.2f} bytes a token) the MEMORY.md index is about {med(rows['MEMORY.md index']) * ctx / total:.0f} tokens")
    print(f"tool results the worker fetched before its first result, bytes p50 {med([c['first_got'] for c in firsts_c]):.0f} p90 {q([c['first_got'] for c in firsts_c]):.0f};"
          f" in the whole first conversation p50 {med([sum(c['got'].values()) for c in firsts_c]):.0f}, by tool (sum): {dict(sum((c['got'] for c in firsts_c), C.Counter()).most_common(5))}")
    print(f"MEMORY.md index: entries p50 {med([c['entries'] for c in firsts_c]):.0f}; tasks whose worker named a memory file not its own {sum(1 for t in sample if mem(t))} of {len(sample)},"
          f" files named p50 {med([len(mem(t)) for t in sample]):.0f}; first calls with AGENTS.md {sum('AGENTS.md' in c['files'] for c in firsts_c)} ({nb(read(os.path.join(sample[0]['main'] or '', 'AGENTS.md')))} bytes in {sample[0]['project']}); tasks whose worker named it {sum(by_task(lambda t, n: 'AGENTS.md' in n))}")
    print(f"skills: listed p50 {med([c['listed'] for c in firsts_c]):.0f}, called in the task p50 {med([len(set().union(*(c['used'] for c in talk(t)))) for t in sample]):.0f}")
    print(f"start block: tasks with a rule or an area brief {shown[2]} of {len(sample)}; rules and briefs shown {shown[0]}, of them for files the task changed {shown[1]}")
    print(f"files named by the worker in the task: p50 {med(by_task(lambda t, n: len(n))):.0f}; changed by the task p50 {med(by_task(lambda t, n: len(n & t['paths']))):.0f};"
          f" docs/*.md named p50 {med(by_task(lambda t, n: sum(f.startswith('docs/') for f in n))):.0f}, of them changed p50 {med(by_task(lambda t, n: sum(f.startswith('docs/') for f in n & t['paths']))):.0f}")

print('\n## T8 stored knowledge that is past')
def git(main, *a):
    return subprocess.run(['git', '-C', main, *a], capture_output=True, text=True)
mains, trees = {t['pid']: t['main'] for t in tasks if t['main'] and os.path.isdir(t['main'])}, {}
def tree(main):  # ^ files now, and the '/'-tails of files now and of files git deleted
    if main not in trees:
        now = set(git(main, 'ls-files').stdout.splitlines())
        gone = set(git(main, 'log', '--no-renames', '--diff-filter=D', '--name-only', '--format=').stdout.splitlines()) - now
        tails = lambda ps: {'/'.join(f.split('/')[i:]) for f in ps for i in range(f.count('/'))} | set(ps)
        trees[main] = (now, tails(now), tails(gone), set(git(main, 'log', '--since=14.days', '--name-only', '--format=').stdout.splitlines()))
    return trees[main]
def cited(text, tr):  # ^ paths with a folder that a text names: (all that resolve, those git deleted)
    found = {f.lstrip('./') for f in PATHY.findall(text) if '/' in f.strip('./')}
    dead = sum(f not in tr[1] and f in tr[2] for f in found)
    return sum(f in tr[1] for f in found) + dead, dead
if not GIT:
    print('not read: --no-git')
else:
    out = []
    store = lambda pid: os.path.join(TASKS, pid, 'memory', 'records.jsonl')
    for pid, main in sorted(mains.items(), key=lambda m: -len(read(store(m[0]))))[:5]:
        log, tr = events(store(pid)), tree(main)
        live = {e['slug']: e for e in log if 'slug' in e}
        n, facts = C.Counter(), [r for r in live.values() if r.get('kind') == 'fact' and r.get('status') == 'active' and r.get('paths')]
        n['covered'] = sum(1 for r in facts if any(o['updated'] > r['updated'] and set(r['paths']) <= set(o['paths']) for o in facts))
        hand = [r.get('source') for r in live.values() if r['slug'].startswith('handover-')]
        n['handovers'], n['handovers but the newest'] = len(hand), len(hand) - len(set(hand))
        for r in live.values():
            kind, paths = r.get('kind'), r.get('paths') or []
            n[kind if r.get('status') == 'active' else f"{kind}, {r.get('status')}"] += 1
            if r.get('status') != 'active':
                n['not active, bytes'] += nb(r.get('text') or '')
                n['successor'] += kind == 'rule' and bool(r.get('superseded_by'))
                continue
            n['paths'] += bool(paths)
            n['paths gone'] += bool(paths) and not any(fnmatch.fnmatch(f, g) or f.startswith(g.rstrip('/*') + '/') for g in paths for f in tr[0])
            if r.get('commit') and paths:
                n['judged'] += 1
                n['stale'] += git(main, 'diff', '--quiet', r['commit'], 'HEAD', '--', *paths).returncode != 0
            elif kind != 'rule':
                n['never judged'] += 1
            task = re.match(r'rotation-(\d+)', r['slug'])
            if task:
                ended = by_key.get((pid, int(task.group(1))), {}).get('state') in ('landed', 'closed')
                n['rotation notes'] += 1
                n['rotation notes of ended tasks'] += ended
                n['rotation notes of ended tasks, bytes'] += ended * nb(r.get('text') or '')
        out.append([by_key[next(k for k in by_key if k[0] == pid)]['project'], len(live), len(log), n['rule'], n['rule, superseded'], n['successor'], n['fact'], n['episode'], n['not active, bytes'] // 1024,
                    n['judged'], n['stale'], n['never judged'], n['paths'], n['paths gone'], nb(read(store(pid))) // 1024, n['rotation notes'], n['rotation notes of ended tasks'], n['rotation notes of ended tasks, bytes'] // 1024, n['handovers'], n['handovers but the newest'], len(facts), n['covered']])
    table('memory records by project', ['project', 'records', 'log lines', 'rules', 'rules superseded', 'of them with superseded_by', 'facts', 'episodes', 'not active KB', 'with commit and paths', 'of them stale',
                                         'facts and episodes never judged', 'active with paths', 'every path gone', 'store KB', 'rotation notes', 'of ended tasks', 'KB', 'manager handovers', 'all but the newest of a log', 'facts with paths', 'a newer fact holds all its paths'], out)
    opened = C.defaultdict(set)
    for t in wide:
        opened[t['main']] |= {f for c in talk(t) for f in c['named'] if f"/memory/task-{t['id']}-" not in f}  # ^ not the memo it wrote itself
    out = []
    for chat in sorted(chat_repos, key=lambda c: -len(read(os.path.join(TASKS, 'chats', c, 'notes', 'context.md'))))[:5]:
        main, notes = mains.get(next(iter(chat_repos[chat]), None)), os.path.join(TASKS, 'chats', chat, 'notes')
        views = {f: read(os.path.join(notes, f)) for f in ('decisions.md', 'context.md', 'decisions-archive.md', 'context-archive.md')}
        rest = [f for f in glob.glob(os.path.join(notes, '*')) if os.path.isfile(f) and os.path.basename(f) not in views]
        old = [f for f in rest if os.path.getmtime(f) < D.datetime.now().timestamp() - 14 * 86400]
        states = [{by_key.get((p, int(x)), {}).get('state') for p in chat_repos[chat] for x in re.findall(r'\d{2,4}', os.path.basename(f))} - {None} for f in rest]
        out.append([chat, *(nb(v) for v in views.values()), *(cited(views['decisions.md'] + views['context.md'], tree(main)) if main else ('-', '-')),
                    len(rest), sum(map(os.path.getsize, rest)) // 1024, len(old), sum(1 for x in states if x), sum(1 for x in states if x and x <= {'landed', 'closed'})])
    table("manager notes by chat (the views of the records, and the chat's other note files)",
          ['chat', 'decisions.md B', 'context.md B', 'decisions-archive.md B', 'context-archive.md B', 'paths cited in the first two', 'git deleted', 'other files', 'KB', 'not written for 14 d', 'named with a task number', 'of an ended task'], out)
    out = []
    for main in [m for m, _ in C.Counter(t['main'] for t in tasks if t['main'] in trees or t['main'] in mains.values()).most_common(3)]:
        tr = tree(main)
        folders = [('docs/', [f for f in tr[0] if re.match(r'docs/[^/]+\.md$', f)]), ('docs/design/', [f for f in tr[0] if re.match(r'docs/design/[^/]+\.md$', f)]),
                   ('worker memory files', glob.glob(os.path.join(CLAUDE, re.sub(r'[^A-Za-z0-9]', '-', main), 'memory', '*.md')))]
        long_ago, pid = D.datetime.now().timestamp() - 14 * 86400, next(k for k, v in mains.items() if v == main)
        other = {f: read(os.path.join(main, f)) for f in tr[0] if re.search(r'\.(md|rs|py|mjs)$', f) and not f.startswith('vendor/')}
        for name, files in folders:
            texts = [read(os.path.join(main, f)) for f in files]
            marks = [cited(text, tr) for text in texts]
            named = [{by_key.get((pid, int(x)), {}).get('state') for x in re.findall(r'#(\d{1,4})\b', text)} for text in texts]
            done = [bool(states) and states <= {'landed', 'closed'} for states in named]
            alone = [not os.path.isabs(f) and not any(os.path.basename(f) in text for g, text in other.items() if g != f) for f in files]
            quiet = sum(1 for f in files if (os.path.getmtime(f) < long_ago if os.path.isabs(f) else f not in tr[3]))
            used = sum(1 for f in files if f in opened[main])
            out.append([f'{os.path.basename(main)} {name}', len(files), sum(map(nb, texts)) // 1024, sum(m[0] for m in marks), sum(m[1] for m in marks), sum(1 for m in marks if m[1]),
                        quiet, sum(done), sum(alone) if name != 'worker memory files' else '-', used,
                        sum(1 for f, d, a in zip(files, done, alone) if d and (a or os.path.isabs(f)) and f not in opened[main])])
    table(f'documents (opened: by a worker of the newest {len(wide)} tasks with a transcript)',
          ['files', 'n', 'KB', 'paths cited', 'git deleted', 'files citing a deleted path', 'not changed for 14 d', 'every task it names ended', 'named by no other .md .rs .py .mjs', 'opened', 'ended, named by none and not opened'], out)

print(f'\n## T9 rotation: what the fresh conversation gets (the newest {len(wide)} tasks with a transcript)')
turns_r, firsts_w = [], [talk(t)[0] for t in wide]
for t in wide:
    for i, c in enumerate(talk(t)):
        if i and 'You continue a task' in c['text'][:200]:
            mark, last = "The earlier conversation's handoff notes:", 'What follows came after:' in c['text']
            notes = c['text'].split(mark, 1)[1] if mark in c['text'] else ''
            given = [a[2].strip() for a in t['adds'] if a[0] < c['at'] and a[2].strip()]
            if not last:  # ^ before 564c9c77 the step line, the waiting message and the pending adds came after the notes
                ends = [notes.find(x) for x in ['\n\nYou are in step '] + [a[:80] for a in given]]
                notes = notes[:min([x for x in ends if x >= 0] + [len(notes)])]
            lost = [a for a in given if a[:80] not in c['text']]
            before = set().union(*(x['named'] for x in talk(t)[:i]))
            turns_r.append(dict(prompt=nb(c['text']), notes=nb(notes), last=last, adds=len(given), lost=sum(map(nb, lost)), ctx=c['ctx'],
                                got=c['first_got'] if c['first_got'] is not None else sum(c['got'].values()),
                                named=len(c['named']), again=len(c['named'] & before) / len(c['named']) if c['named'] else None,
                                kept=len(before & t['paths'] & c['named']) / len(before & t['paths']) if before & t['paths'] else None))
col = lambda k: [r[k] for r in turns_r]
if turns_r:
    print(f"rotations read {len(turns_r)} in {sum(1 for t in wide if len(talk(t)) > 1)} tasks (`rotated` events in these tasks: {sum(len(t['rot']) for t in wide)})")
    print(f"the first message of the fresh conversation: bytes p50 {med(col('prompt')):.0f} p90 {q(col('prompt')):.0f}; its handoff notes p50 {med(col('notes')):.0f} p90 {q(col('notes')):.0f},"
          f" none in {sum(1 for r in turns_r if not r['notes'])}; where the notes end the message ({sum(col('last'))} rotations since 564c9c77) p50 {med([r['notes'] for r in turns_r if r['last']]):.0f}")
    print(f"adds the task got before the rotation: in {sum(1 for r in turns_r if r['adds'])} rotations; with an add the fresh message does not hold: {sum(1 for r in turns_r if r['lost'])},"
          f" bytes of those adds p50 {med([r['lost'] for r in turns_r if r['lost']]):.0f}")
    print(f"context tokens at its first call p50 {med(col('ctx')):.0f} (a task's first conversation: {med([c['ctx'] for c in firsts_w]):.0f});"
          f" tool results fetched before its first result, bytes p50 {med(col('got')):.0f} (first conversation: {med([c['first_got'] for c in firsts_w]):.0f})")
    print(f"files it names: p50 {med(col('named')):.0f}, of them named before the rotation p50 {pct(med(col('again')), 1)};"
          f" of the task's changed files the earlier conversations named, it names again p50 {pct(med(col('kept')), 1)}")
allrot = [r for t in tasks for r in t['rot']]
print(f"all tasks: `rotated` {len(allrot)}, by reason {dict(C.Counter('idle' if 'idle' in r[2] else r[2] if r[2] == 'size' else 'other' for r in allrot))}, context tokens at the rotation p50 {med([r[1] for r in allrot]):.0f}")
rate, order = C.defaultdict(lambda: [0, 0, 0, set()]), C.Counter()
for t in tasks:
    marks = t['revs'] + [INF]
    for i, lo in enumerate(t['revs']):
        adds = [a for a in t['post'] if lo <= a[0] < marks[i + 1]]
        cell = rate['1' if i == 0 else '2-3' if i < 3 else '4-6' if i < 6 else '7+', 'after a rotation' if any(r[0] < lo for r in t['rot']) else 'no rotation yet']
        cell[0] += 1
        cell[1] += any(a[1] in OWNER for a in adds)
        cell[2] += any(a[1] == 'events' for a in adds)
        cell[3].add((t['pid'], t['id']))
    own = [a[0] for a in t['post'] if a[1] in OWNER]
    if own and t['rot'] and len(t['revs']) >= 4:
        order['the owner add came first' if min(own) < t['rot'][0][0] else 'the rotation came first'] += 1
table('a result, by its place in the task and whether the conversation was replaced before it',
      ['result number', 'conversation', 'results', 'in tasks', 'owner add before the next result', 'sent back'],
      [[k[0], k[1], v[0], len(v[3]), pct(v[1], v[0]), pct(v[2], v[0])] for k, v in sorted(rate.items())])
print(f"4+ revision tasks with both a rotation and an owner add after a result: {dict(order)}")

print("""
## T10 what these records cannot show
- The owner's verdict on quality. An owner add after a result is a correction or a new wish; a To check state is the nearest record of a verdict.
- Who wrote an add. The class comes from the turn of the task's chat around it; the owner's own add, another chat's add and a lost turn all read `outside`.
- Context restated in other words or in another language. The repeated-text share counts exact 5-word runs only.
- Why a result missed: thin prompt, a wrong decision or a weak model look the same here.
- Work in sessions the owner opened by hand: no record but the sessions that exist now; no history of deleted worktrees before the prune notices.
- Which task another project's manager asked for: only the turn that created it links them.
- Size of the change behind a result: closed tasks' branches are gone, so cost per output token stands in for it.
- What a worker did with a rule: CLAUDE.md, the worker rules and a start-block rule leave no trace when followed. A file is "used" when the worker named it.
- A record that a later owner rule contradicts, unless someone marked it superseded.
- Whether a rotation caused an owner add. T9 holds the place in the task equal; it does not hold the task's difficulty equal.
- Pi workers: they write no Claude transcript, so T7 and T9 are Claude only.""")
