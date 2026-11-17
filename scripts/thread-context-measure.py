#!/usr/bin/env python3
"""Measures docs/design/thread-context.md. No model call; prints totals, never logs.

  python3 -B scripts/thread-context-measure.py port|recall|rotation|cost [--days 14]

port      checks the BM25 port below against `wsx memory search` on the wsx scope (the real code)
recall    per thread and arm: recall and precision of the rebuilt thread block against a hand list
rotation  the four facts #464's fresh start lost: are they in the log, and does the seed find them
cost      option (a) and (b) in USD a week from the real manager turn log (bench-manager's loader)

Why a port: `wsx memory search` reads the transcripts only of the project of the current folder,
and a worker's folder is wsx. The port is crates/wsx-core/src/memory/index.rs (`each_token`,
`rank`) over the project's own digest `tasks/<project-id>/search.jsonl`.
"""
import argparse
import importlib.util
import json
import math
import random
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
STATE = Path.home() / '.local/state/wsx/tasks'
K1, B = 1.2, 0.75


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def jsonl(path):
    out = []
    for line in open(path, encoding='utf-8'):
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def is_cjk(c):
    o = ord(c)
    return (0x1100 <= o <= 0x11FF or 0x3040 <= o <= 0x30FF or 0x3130 <= o <= 0x318F or 0x3400 <= o <= 0x4DBF
            or 0x4E00 <= o <= 0x9FFF or 0xAC00 <= o <= 0xD7AF or 0xF900 <= o <= 0xFAFF)


def tokens(text):
    """index.rs `each_token`: words of letters and digits; a CJK run gives its overlapping pairs
    (a run of one character gives that character)."""
    out, word, run = [], '', ''
    for c in text.lower() + '\0':
        if is_cjk(c):
            if word:
                out.append(word)
                word = ''
            if run:
                out.append(run[-1] + c)
            run += c
        else:
            if len(run) == 1:
                out.append(run)
            run = ''
            if c.isalnum():
                word += c
            elif word:
                out.append(word)
                word = ''
    return out


def rank(texts, words):
    """index.rs `rank`: BM25 (k1 1.2, b 0.75); returns (score, position) for texts with a term."""
    terms = list(dict.fromkeys(tokens(words)))
    want = set(terms)
    seen, df, total = [], dict.fromkeys(terms, 0), 0
    for at, text in enumerate(texts):
        toks = tokens(text)
        total += len(toks)
        counts = {}
        for t in toks:
            if t in want:
                counts[t] = counts.get(t, 0) + 1
        for t in counts:
            df[t] += 1
        if counts:
            seen.append((at, len(toks), counts))
    n = len(texts)
    avg = max(total / max(n, 1), 1.0)
    idf = lambda d: math.log(1 + (n - d + 0.5) / (d + 0.5))
    return [(sum(idf(df[t]) * tf * (K1 + 1) / (tf + K1 * (1 - B + B * ln / avg)) for t, tf in c.items()), at)
            for at, ln, c in seen]


class Project:
    def __init__(self, name, pid):
        self.name, self.pid = name, pid
        self.dir = STATE / pid
        self.docs = [d for d in jsonl(self.dir / 'search.jsonl') if d['kind'] != 'mark']
        self.log = jsonl(STATE / 'chats' / name / 'log.jsonl')
        self.rows = {r['id']: r for r in self.log if r['kind'] in ('message', 'reply')}
        self.born, self.recs = {}, {}
        for r in jsonl(self.dir / 'memory/records.jsonl'):
            self.born.setdefault(r['slug'], r.get('updated', 0))
            self.recs.setdefault(r['slug'], {}).update({k: v for k, v in r.items() if k != 'op'})

    def transcripts(self, words, cut):
        """The digest docs older than `cut`, ranked, equal scores newer first."""
        docs = [d for d in self.docs if d['at'] < cut]
        scored = rank([d['text'] for d in docs], words)
        scored.sort(key=lambda s: (-s[0], -docs[s[1]]['at']))
        return [docs[at] for _, at in scored]

    def records(self, words, cut, limit=10):
        """The real `wsx memory search` over the scope's records (a record older than `cut`)."""
        hits = {}
        for kind in ('rule', 'fact', 'episode'):
            out = subprocess.run(['wsx', 'memory', 'search', '--scope', self.pid, '--kind', kind, '--json',
                                  '--limit', str(limit), words[:900]], capture_output=True, text=True).stdout
            for h in json.loads(out or '[]'):
                hits[h['slug'].split('/')[-1]] = h
        live = [h for s, h in hits.items() if self.born.get(s, 0) < cut]
        return sorted(live, key=lambda h: -h['score'])


def key(project, doc):
    """What a digest doc is a record of: a chat row of the project's own chat, or a task."""
    if doc['kind'] in ('message', 'reply') and doc.get('chat') == project.name:
        return f"msg:{doc['id']}"
    if doc.get('task'):
        return f"task:{doc['task']}"
    return None


def est_tokens(text):
    ascii_chars = sum(1 for c in text if ord(c) < 128)
    return round(ascii_chars / 4 + (len(text) - ascii_chars) / 1.7)


def line(doc_or_record):
    text = doc_or_record.get('text') or doc_or_record.get('excerpt') or ''
    return ' '.join(text.split())[:160]


def block(project, seed, cut, join):
    """The thread block of a turn: 5 records, 15 transcript rows (arm A), plus the rows the
    deterministic joins add (arm B): the replies and messages that quote a hit, the tasks the
    thread text names (`#n`, at most 4) and the records that cite a hit's id or task."""
    items = {}  # key -> line, in order
    for h in project.records(seed, cut)[:5]:
        items['rec:' + h['slug'].split('/')[-1]] = line(h)
    ranked = project.transcripts(seed, cut)
    for d in ranked:
        k = key(project, d)
        if k and k not in items and sum(not x.startswith('rec:') for x in items) < 15:
            items[k] = line(d)
    if join:
        ids = {int(k[4:]) for k in items if k.startswith('msg:')}
        for r in project.log:
            if r['kind'] == 'reply' and r['unix_ms'] < cut and ids & set(r.get('to') or []):
                items.setdefault(f"msg:{r['id']}", line(r))
        named = {}
        for text in [seed] + [i for k, i in items.items() if k.startswith('msg:')]:
            for n in re.findall(r'#(\d+)\b', text):
                named[n] = named.get(n, 0) + 1
        tasks = [n for n, _ in sorted(named.items(), key=lambda x: -x[1])[:4]]
        for n in tasks:
            title = next((line(d) for d in project.docs if d['kind'] == 'task' and str(d.get('task')) == n), '')
            items.setdefault(f'task:{n}', title)
        cites = [f'#{n} ' for n in tasks] + [f'msg:{i}' for i in ids] + [f'owner:{i}' for i in ids]
        for slug, rec in project.recs.items():
            blob = (rec.get('text') or '') + ' ' + str(rec.get('source')) + ' ' + str(rec.get('verified_by'))
            if project.born.get(slug, 0) < cut and rec.get('status', 'active') == 'active' and any(c in blob + ' ' for c in cites):
                items.setdefault('rec:' + slug, line(rec))
    return items



def closure(project, anchors, cut, depth=2, cap=60):
    """Arm C: the rows a thread tag at its first row reaches through links the log already has.
    Edges: a reply and the messages it answers (`to`); a quoting message ("> ...") and the reply
    whose text it quotes; an events message and the tasks it names; a reply and the task created
    within 3 min of it; a record and the message ids or tasks it cites. Breadth first from the
    anchor, at most `depth` steps and `cap` nodes."""
    edges = {}
    def link(a, b):
        edges.setdefault(a, set()).add(b)
        edges.setdefault(b, set()).add(a)
    rows = [r for r in project.log if r['kind'] in ('message', 'reply') and r['unix_ms'] < cut]
    for r in rows:
        for i in r.get('to') or []:
            link(f"msg:{r['id']}", f'msg:{i}')
        text = r.get('text') or ''
        if r['kind'] == 'message' and r.get('from') == 'events':
            for n in set(re.findall(r'(?m)^#(\d+) \w+', text)):
                link(f"msg:{r['id']}", f'task:{n}')
        if r['kind'] == 'message' and text.startswith('> '):
            head = text[2:].splitlines()[0][:50]
            for q in rows:
                if q['kind'] == 'reply' and head and head in (q.get('text') or '')[:400] and q['id'] < r['id']:
                    link(f"msg:{r['id']}", f"msg:{q['id']}")
    for tdir in project.dir.iterdir():
        if tdir.name.isdigit() and (tdir / 'task.json').exists():
            made = json.load(open(tdir / 'task.json')).get('created_unix_ms', 0)
            if made < cut:
                for r in rows:
                    if r['kind'] == 'reply' and 0 <= r['unix_ms'] - made < 180_000:
                        link(f"msg:{r['id']}", f'task:{tdir.name}')
    for slug, rec in project.recs.items():
        if project.born.get(slug, 0) >= cut or rec.get('status', 'active') != 'active':
            continue
        blob = (rec.get('text') or '') + ' ' + str(rec.get('source')) + ' ' + str(rec.get('verified_by'))
        for i in re.findall(r'(?:msg:|owner:)(\d+)', blob):
            link('rec:' + slug, f'msg:{i}')
        for n in re.findall(r'task:(\d+)', str(rec.get('source'))):
            link('rec:' + slug, f'task:{n}')
    seen, frontier = {a: 0 for a in anchors}, list(anchors)
    while frontier and len(seen) < cap:
        nxt = []
        for node in frontier:
            for other in sorted(edges.get(node, ())):
                if other not in seen and seen[node] < depth and len(seen) < cap:
                    seen[other] = seen[node] + 1
                    nxt.append(other)
        frontier = nxt
    return seen

# Hand lists: read from the chat logs, task logs and records by the author of this script; every
# key is checked to exist. `cut`: the owner turn's time (the block holds only what is older).
THREADS = {
    '#88 stories (studio-app, msg 992)': dict(
        project=('studio-app', 'studio-app-a97d776d'), turn=992, title='Collect popular Korean 2010s stories',
        truth=['msg:697', 'msg:701', 'msg:703', 'msg:702', 'msg:704', 'msg:724', 'task:88', 'task:89',
               'rec:rule-169b89a4e712c33f', 'rec:section-6f19c2c529123f16']),
    '#90 voice (studio-app, msg 991)': dict(
        project=('studio-app', 'studio-app-a97d776d'), turn=991, title='Korean narration voice and pacing',
        truth=['msg:687', 'msg:689', 'msg:691', 'msg:692', 'msg:695', 'msg:746', 'msg:708', 'msg:964', 'msg:973',
               'task:87', 'task:90', 'task:91', 'task:93', 'rec:memo-108-29390d45c477dda9',
               'rec:section-56bfd4ecc061ec75']),
    'topics (wsx, msg 5091)': dict(
        project=('wsx', 'wsx-8cbe3f08'), turn=5091, title='Thread design: context in a tree transcript',
        truth=['msg:5065', 'msg:5072', 'msg:5075', 'msg:5076', 'msg:5080', 'msg:5082', 'msg:5066', 'msg:5068',
               'msg:5074', 'msg:5077', 'msg:5078', 'msg:5081', 'msg:5083', 'task:491', 'task:494',
               'rec:transcript-muted-lines-carry-status', 'rec:tree-context-from-memory',
               'rec:one-term-shared-picture']),
    '160K rotation (wsx, msg 4806)': dict(
        project=('wsx', 'wsx-8cbe3f08'), turn=4806, title='Manager rotation at 160K',
        truth=['msg:4639', 'msg:4641', 'msg:4646', 'msg:4650', 'msg:4799', 'msg:4801', 'msg:4803', 'task:456',
               'task:462', 'rec:manager-idle-written-handover', 'rec:memo-456-396dec46c8958e95']),
}
ARMS = {  # seed of the retrieval, whether the joins run
    'A0 message only': (lambda t, m: m, False),
    'A1 title + message': (lambda t, m: f'{t} {m}', False),
    'B  A1 + joins': (lambda t, m: f'{t} {m}', True),
}


def run_recall(_):
    cache = {}
    for name, spec in THREADS.items():
        project = cache.setdefault(spec['project'], Project(*spec['project']))
        turn = project.rows[spec['turn']]
        cut = turn['unix_ms']
        def born(k):  # when a truth item came to exist
            if k.startswith('msg:'):
                return project.rows[int(k[4:])]['unix_ms']
            if k.startswith('task:'):
                return json.load(open(project.dir / k[5:] / 'task.json'))['created_unix_ms']
            return project.born[k[4:]]
        late = [k for k in spec['truth'] if born(k) >= cut]
        spec['truth'] = [k for k in spec['truth'] if k not in late]
        have = {f'msg:{i}' for i in project.rows} | {f'task:{d["task"]}' for d in project.docs if d.get('task')}
        have |= {'rec:' + s for s in project.born}
        missing = [k for k in spec['truth'] if k not in have and not k.startswith('msg:')]
        later = [k for k in spec['truth'] if k.startswith('msg:') and project.rows.get(int(k[4:]), {}).get('unix_ms', 0) >= cut]
        indexed = {key(project, d) for d in project.docs if d['at'] < cut}
        print(f'\n## {name}: truth {len(spec["truth"])} (dropped, newer than the turn: {late}), missing keys {missing}, newer than the turn {later}, '
              f'not in the digest before the turn {[k for k in spec["truth"] if k.startswith("msg:") and k not in indexed]}')
        for arm, (seed_of, join) in ARMS.items():
            items = block(project, seed_of(spec['title'], turn['text']), cut, join)
            hit = [k for k in spec['truth'] if k in items]
            size = sum(est_tokens(v) + 4 for v in items.values())
            print(f'  {arm:20} recall {len(hit)}/{len(spec["truth"])} = {len(hit)/len(spec["truth"]):.2f}  '
                  f'precision {len(hit)}/{len(items)} = {len(hit)/max(len(items),1):.2f}  block {len(items)} items ~{size} tokens')
            if join:
                print('  lost:', [k for k in spec['truth'] if k not in items])
        first = min((k for k in spec['truth'] if k.startswith('msg:')), key=lambda k: project.rows[int(k[4:])]['unix_ms'])
        mine = [k for k in spec['truth'] if k.startswith('msg:') and project.rows[int(k[4:])].get('from') == 'owner']
        tasks = [k for k in spec['truth'] if k.startswith('task:')]
        for label, anchors in (('C1 tag on the first row', [first]), ('C2 tag on each owner row', mine or [first]),
                               ('D  C2 + TaskRecord.topic', (mine or [first]) + tasks)):
            reach = closure(project, anchors, cut)
            hit = [k for k in spec['truth'] if k in reach]
            size = sum(est_tokens(line(project.rows[int(k[4:])]) if k.startswith('msg:') else k) + 4 for k in reach)
            print(f'  {label:25} recall {len(hit)}/{len(spec["truth"])} = {len(hit)/len(spec["truth"]):.2f}  '
                  f'precision {len(hit)}/{len(reach)} = {len(hit)/max(len(reach),1):.2f}  block {len(reach)} items ~{size} tokens'
                  + (f'  lost {[k for k in spec["truth"] if k not in reach]}' if label.startswith('D') else ''))
        reach = closure(project, mine or [first], cut)
        # where each lost item stops: the rank of its best doc among all transcripts for arm A1
        seed = f"{spec['title']} {turn['text']}"
        ranked = project.transcripts(seed, cut)
        where = {}
        for at, d in enumerate(ranked):
            where.setdefault(key(project, d), at + 1)
        step = {}
        a1 = block(project, seed, cut, False)
        for k in spec['truth']:
            if k in a1:
                continue
            if k.startswith('rec:'):
                step[k] = 'record: not in the records top 5' if k[4:] in project.born and project.born[k[4:]] < cut else 'record: newer than the turn'
            elif k not in indexed:
                step[k] = 'digest: no row before the turn'
            elif k not in where:
                step[k] = 'query: shares no token with the seed'
            else:
                step[k] = f'cut: ranked {where[k]}'
        print('  first loss, arm A1:', step)


def run_port(_):
    subprocess.run(['wsx', 'memory', 'search', '--limit', '1', 'warm'], capture_output=True)  # catches the digest up
    project = Project('wsx', 'wsx-8cbe3f08')
    queries = ['rotation 160K open threads', 'tree transcript topics thread', 'scenario flakes web part', 'check-run queue deadline',
               'manager handover notes', '한국어 목소리']
    ok = 0
    for q in queries:
        out = subprocess.run(['wsx', 'memory', 'search', '--json', '--limit', '40', q], capture_output=True, text=True).stdout
        real = [h['slug'] for h in json.loads(out or '[]') if h['kind'] in ('message', 'reply', 'task', 'result', 'history')]
        mine = []
        for d in project.transcripts(q, 1 << 60):
            ref = f"wsx:{d['id']}" if d['kind'] in ('message', 'reply') and d.get('chat') == 'wsx' else f"wsx#{d.get('task')}"
            mine.append(ref)
        n = min(len(real), 10)
        a, b = list(dict.fromkeys(real))[:n], list(dict.fromkeys(mine))[:n]
        same = set(a) == set(b)
        ok += same
        print(f'{q!r}: real {len(real)} transcript hits; first {n}: same refs {same}, same order {a == b}')
    print(f'the port finds the same first-10 refs on {ok} of {len(queries)} queries (order differs only among equal scores)')


def run_rotation(_):
    """#464's four counted cases (docs/design/manager-event-model.md): was the lost fact written in a
    chat row, task row or record before the turn (a block can only hold what was written)?"""
    has = lambda *w: (lambda t: all(x in t for x in w))
    anyof = lambda f, *g: (lambda t: f(t) and any(x in t for x in g))
    cases = [('wsx', 'wsx-8cbe3f08', 4422, '#422 #423 #426 stalled, restart them', has('#422', '#423', '#426')),
             ('studio-app', 'studio-app-a97d776d', 788, '#90 waits for #91 (a hold)', anyof(has('#90', '#91'), 'hold', ' after', 'wait')),
             ('pr-review', 'pr-review-d10dc66b', 435, 'criteria 3 and 4 compared', has('③', '④')),
             ('pr-review', 'pr-review-d10dc66b', 315, 'open scoring question (a/b, absolute or relative, Pingo)', has('pingo'))]
    for name, pid, rid, fact, test in cases:
        project = Project(name, pid)
        reply = project.rows.get(rid)
        if not reply:
            print(f'case {rid}: reply not in the log')
            continue
        cut = min(project.rows[i]['unix_ms'] for i in reply.get('to') or [rid] if i in project.rows)
        rows = [d for d in project.docs if d['at'] < cut and d['kind'] in ('message', 'reply', 'task', 'result', 'history', 'decision', 'memo')
                and test(d['text'].lower())]
        recs = [s for s, r in project.recs.items() if project.born.get(s, 0) < cut and test((r.get('text') or '').lower())]
        by = {}
        for d in rows:
            by[d['kind']] = by.get(d['kind'], 0) + 1
        newest = max((d['at'] for d in rows), default=0)
        print(f'case {rid} ({name}): "{fact}": {len(rows)} rows hold it before the turn {by}, {len(recs)} records; '
              f'newest {round((cut - newest) / 60000) if rows else "-"} min before the turn')


def run_cost(options):
    bm = load('bench_manager', 'bench-manager.py')
    projects = bm.manager_turns(options.days)
    r, w, cache_ms = bm.PRICE['cache_read'], bm.PRICE['cache_write'], bm.CACHE_MS
    week = 7 / options.days
    turns = [t for ts in projects.values() for t in ts]
    logged = sum(t['usage']['cost_micro_usd'] for t in turns) / 1e6 * week
    owner = [(p, i) for p, ts in projects.items() for i, t in enumerate(ts) if not t['events_only']]
    print(f'{options.days} d: {len(turns)} manager turns, {len(owner)} with an owner or chat message, logged ${logged:.2f} a week')
    # (a) a block of B tokens joins the context at every owner turn: written once, then read by every later call of the session
    for B in (800, 1500, 3000):
        extra = 0.0
        for p, i in owner:
            ts = projects[p]
            extra += B * w / 1e6 + B * r * (ts[i]['usage']['calls'] - 1) / 1e6
            prev = ts[i]
            for t in ts[i + 1:]:
                if t['session'] != ts[i]['session']:
                    break
                cold = t['ms'] - prev['ms'] >= cache_ms
                extra += (B * (w - r) / 1e6 if cold else 0) + B * r * t['usage']['calls'] / 1e6
                prev = t
        print(f'(a) block {B} tokens on every owner turn: +${extra*week:.2f} a week ({extra*week/logged*100:.1f}% of the logged spend)')
    # (b) one conversation per thread. Same formula for both sides, as bench-manager's rotation_cost:
    # a turn is its own work plus reading its conversation (calls x context, or one write when cold).
    kept = []
    rows = []
    for p, ts in projects.items():
        before = None
        for t in ts:
            u = t['usage']
            start = 0 if t['fresh'] or not before else before['usage'].get('context_tokens', 0)
            cold = before is None or t['ms'] - before['ms'] >= cache_ms
            calls = max(1, u.get('calls', 1))
            prefix = (start * w + (calls - 1) * start * r if cold else calls * start * r) / 1e6
            own = u['cost_micro_usd'] / 1e6 - prefix
            rows.append((p, t, own, calls, cold))
            if not t['fresh']:
                kept.append((own, u.get('context_tokens', 0) - start))
            before = t
    fresh = sorted(t['usage']['cost_micro_usd'] for t in turns if t['fresh'])
    start_usd = fresh[len(fresh) // 2] / 1e6
    start_ctx = sorted(t['usage'].get('context_tokens', 0) for t in turns if t['fresh'])[len(fresh) // 2]
    growth = sorted(g for _, g in kept)[len(kept) // 2]
    own_owner = [(p, t, max(0.0, own), calls) for p, t, own, calls, _ in rows if not t['events_only']]
    base = sum(o + (calls * t['usage'].get('context_tokens', 0) * r) / 1e6 for _, t, o, calls in own_owner)  # warm reads at the logged context
    cold_today = sum(c for _, t, _, _, c in rows if not t['events_only'] and not t['fresh'])
    print(f'a fresh first turn costs ${start_usd:.2f} (median of {len(fresh)}) and ends at {start_ctx} tokens; a kept turn grows the context {growth} tokens (median)')
    print(f'today, one conversation per project: {cold_today} of {len(own_owner)} owner turns start cold (gap over 60 min or fresh)')
    rng = random.Random(7)
    for n, stay in ((3, 0.0), (5, 0.0), (8, 0.0), (5, 0.8)):
        usd = cold_n = 0.0
        for _ in range(20):
            for p, ts in projects.items():
                conv, th = {}, 0
                for t in ts:
                    if t['events_only']:
                        continue
                    own = next(o for pp, tt, o, _ in own_owner if tt is t)
                    calls = max(1, t['usage'].get('calls', 1))
                    th = th if rng.random() < stay else rng.randrange(n)
                    ctx, last = conv.get(th, (0, None))
                    if last is None or ctx >= 400_000:
                        usd += start_usd
                        ctx = start_ctx
                    else:
                        cold = t['ms'] - last >= cache_ms
                        cold_n += cold
                        if cold and ctx > 60_000:  # today's idle rule (IDLE_ROTATE_TOKENS): a fresh start, not a cold resume
                            usd += start_usd
                            ctx = start_ctx
                        else:
                            usd += own + ((ctx * w + (calls - 1) * ctx * r) if cold else calls * ctx * r) / 1e6
                    conv[th] = (ctx + growth, t['ms'])
        usd, cold_n = usd / 20 * week, cold_n / 20 * week
        print(f'(b) {n} threads a project, stay on the last thread {stay:.0%}: owner turns cost ${usd:.2f} a week against ${base * week:.2f} today (modeled the same way); '
              f'{cold_n:.0f} cold turns a week = {cold_n / (len(own_owner) * week) * 100:.0f}% of owner turns (today {cold_today / len(own_owner) * 100:.0f}%)')


if __name__ == '__main__':
    ap = argparse.ArgumentParser()
    ap.add_argument('part', choices=['port', 'recall', 'rotation', 'cost'])
    ap.add_argument('--days', type=int, default=14)
    o = ap.parse_args()
    {'port': run_port, 'recall': run_recall, 'rotation': run_rotation, 'cost': run_cost}[o.part](o)
