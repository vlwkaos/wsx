#!/usr/bin/env python3
"""What fills each model call of wsx agents, measured from Claude Code transcripts.

Reads `~/.claude/projects/<slug>/*.jsonl` of a project's workers (`<slug>-t<n>`) and of its
manager (the `sdk-cli` sessions in `<slug>`), and prints totals only: the parts of a fresh
start, the growth of a conversation by source, the growth per host turn, cold cache writes,
the cost of a task by part, and a replay of each candidate bound. Method and findings:
docs/design/context-budget.md.

    python3 -B scripts/context-measure.py [--project <main checkout>]... [--last 80]
        [--task <n>]... [--manager-log <manager/log.jsonl>] [--projects-dir ~/.claude/projects]
"""

import argparse
import collections
import importlib.util
import json
import re
import statistics
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
# ^ One price table: memory-eval.py's, Opus 5.5 API dollars, which reproduces the manager log's
# logged reply cost. Every model is priced with it here (Fable's and Sonnet's rates differ).
_spec = importlib.util.spec_from_file_location('memory_eval', HERE / 'memory-eval.py')
_memory_eval = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_memory_eval)
PRICE = _memory_eval.PRICE

CHARS_PER_TOKEN = 4  # ^ Only to share a measured delta between the inputs that made it.
COLD_WRITE = 20_000  # A write of at least half the context, and this large, after the first call: the cache was gone.
MAX_DELTA = 60_000  # A larger delta is a resume after a zero-usage (limit or error) line.
CHECK_RE = re.compile(r'check-run|cargo (test|nextest|clippy|build|fmt)|python3 .*test-|scripts/test-|runtime-smoke')
READ_RE = re.compile(r'^(cat|sed|head|tail|rg|grep|wc|ls|awk|diff|git (show|diff|log|blame))\b')
HOST_KINDS = (
    ('check answer', re.compile(r'^(check-run|Check-run|The check)')),
    ('handoff', re.compile(r'^You continue a task whose earlier conversation')),
    ('step', re.compile(r'^Step \d of \d|^Do only this step', re.M)),
    ('manager start', re.compile(r"^The owner's decisions")),
)


def slug(path):
    return re.sub(r'[/.]', '-', str(path))


def host_kind(text, first):
    for name, rx in HOST_KINDS:
        if rx.search(text[:400]):
            return name
    return 'task prompt' if first else 'message'


def tool_part(name, tool_input):
    if name == 'Bash':
        command = str((tool_input or {}).get('command', '')).strip()
        if CHECK_RE.search(command):
            return 'check output'
        if READ_RE.search(command) or ' | head' in command:
            return 'file read (bash)'
        return 'command output'
    if name == 'Read':
        ranged = bool((tool_input or {}).get('offset') or (tool_input or {}).get('limit'))
        return 'file read (range)' if ranged else 'file read (whole)'
    if name in ('Edit', 'Write', 'MultiEdit'):
        return 'edit result'
    if name == 'Skill':
        return 'skill load'
    if name == 'Agent':
        return 'subagent report'
    return f'tool {name}'


def load_session(path):
    """One transcript: its calls in order, each with what arrived before it."""
    calls, pending, seen, tools = [], [], set(), {}
    shape, attachments, files, entry, before_first_host_text = None, collections.Counter(), {}, None, True
    with open(path, encoding='utf-8') as handle:
        for line in handle:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = row.get('type')
            if kind == 'api-request-shape' and shape is None:
                s = row.get('shape') or {}
                shape = {'system': [len(b.get('text', '')) for b in s.get('system', [])],
                         'tools': sorted(((len(json.dumps(t)), t.get('name')) for t in s.get('tools', [])), reverse=True)}
            if row.get('isSidechain'):
                continue
            if kind == 'user' and entry is None:
                entry = row.get('entrypoint')
            if kind == 'attachment':
                rendered = row.get('rendered')
                if not rendered:
                    continue
                text = json.dumps(rendered)
                name = (row.get('attachment') or {}).get('type', '?')
                if not calls:
                    attachments[name] += len(text)
                    if name == 'instructions':
                        heads = [(m.start(), m.group(1).strip()) for m in re.finditer(r'Contents of ([^\n(]+)', text)]
                        for i, (at, file) in enumerate(heads):
                            files[file] = (heads[i + 1][0] if i + 1 < len(heads) else len(text)) - at
                pending.append(('reminders' if calls else 'start attachments', len(text)))
            elif kind == 'user':
                content = row['message'].get('content')
                blocks = [{'type': 'text', 'text': content}] if isinstance(content, str) else content
                for block in blocks:
                    if block.get('type') == 'tool_result':
                        body = block.get('content')
                        body = body if isinstance(body, str) else json.dumps(body)
                        use = tools.get(block.get('tool_use_id'), ('?', {}))
                        pending.append((tool_part(*use), len(body)))
                    elif block.get('type') == 'text':
                        pending.append(('host: ' + host_kind(block['text'], before_first_host_text), len(block['text'])))
                        before_first_host_text = False
            elif kind == 'assistant':
                message = row['message']
                usage = message.get('usage') or {}
                for block in message.get('content', []):
                    if block.get('type') == 'tool_use':
                        tools[block['id']] = (block['name'], block.get('input') or {})
                ctx = usage.get('input_tokens', 0) + usage.get('cache_read_input_tokens', 0) + usage.get('cache_creation_input_tokens', 0)
                if not ctx or message.get('id') in seen:
                    continue  # ^ A `<synthetic>` line (limit, error) has no usage: skip it.
                seen.add(message.get('id'))
                calls.append({
                    'ctx': ctx, 'read': usage.get('cache_read_input_tokens', 0), 'write': usage.get('cache_creation_input_tokens', 0),
                    'out': usage.get('output_tokens', 0), 'thinking': (usage.get('output_tokens_details') or {}).get('thinking_tokens', 0),
                    'model': (message.get('model') or '')[7:], 'ts': row.get('timestamp', ''), 'inputs': pending,
                    'turn': any(k.startswith('host:') for k, _ in pending),
                })
                pending = []
    return {'path': path, 'calls': calls, 'shape': shape, 'attachments': attachments, 'files': files, 'entry': entry}


def attribute(session):
    """Tokens of growth by source: each call's delta less the prior output, shared by chars."""
    by = collections.Counter()
    calls = session['calls']
    for prev, call in zip(calls, calls[1:]):
        by['own output'] += prev['out']
        by['  of which thinking'] += prev['thinking']
        delta = call['ctx'] - prev['ctx'] - prev['out']
        if delta < 0 or delta > MAX_DELTA:
            continue
        chars = sum(n for _, n in call['inputs']) or 1
        for part, n in call['inputs']:
            by[part] += int(delta * n / chars)
    return by


def turns(session):
    """(calls, growth) of each host turn."""
    out, start, count = [], session['calls'][0]['ctx'], 0
    for call in session['calls'][1:]:
        if call['turn']:
            out.append((count, call['ctx'] - start))
            start, count = call['ctx'], 0
        count += 1
    out.append((count, session['calls'][-1]['ctx'] - start))
    return out


def cold_writes(session):
    """Calls after the first whose cache write says the cache was gone."""
    return [c for c in session['calls'][1:] if c['write'] > COLD_WRITE and c['write'] * 2 >= c['ctx']]


def cost(session):
    """Dollars of a session by part: reads and writes follow the context, output is its own."""
    by, parts = collections.Counter(), attribute(session)
    read = sum(c['read'] for c in session['calls'])
    write = sum(c['write'] for c in session['calls'])
    grown = sum(v for k, v in parts.items() if not k.startswith('  '))
    start = session['calls'][0]['ctx']
    total_ctx = sum(c['ctx'] for c in session['calls']) or 1
    by['start (prefix + first message)'] = start * len(session['calls']) / total_ctx
    for part, tokens in parts.items():
        if not part.startswith('  '):
            by[part] = (1 - by['start (prefix + first message)']) * tokens / (grown or 1)
    dollars = read * PRICE['read'] + write * PRICE['write'] + sum(c['out'] for c in session['calls']) * PRICE['out']
    return {part: share * dollars for part, share in by.items()}, dollars


# Each bound as its rule: `prefix` tokens off every call, `cap` tokens per tool result, `thinking` dropped
# inside a turn, a fresh conversation past `rotate` tokens (`at_turn`: checked only when a host message
# arrives, as the host does today; 0 = at every host message).
SCENARIOS = {
    'actual': {},
    'prefix -10k': {'prefix': 10_000},
    'tool cap 2k': {'cap': 2_000},
    'no thinking': {'thinking': True},
    'rotate 100k at turn': {'rotate': 100_000, 'at_turn': True},
    'rotate 150k live': {'rotate': 150_000},
    'fresh per turn': {'rotate': 0, 'at_turn': True},
    'live 150k + prefix + cap': {'rotate': 150_000, 'prefix': 10_000, 'cap': 2_000},
}


def simulate(sessions, start, handover=6_000):
    table = {name: [0, 0] for name in SCENARIOS}
    for session in sessions:
        calls = session['calls']
        for name, rule in SCENARIOS.items():
            read = write = cut = thinking_cut = base = 0
            for i, call in enumerate(calls):
                if call['turn']:
                    thinking_cut = 0  # ^ The API drops earlier turns' thinking by itself.
                if 'cap' in rule:
                    cut += sum(max(0, n // CHARS_PER_TOKEN - rule['cap']) for part, n in call['inputs'] if not part.startswith('host'))
                ctx = call['ctx'] - cut - thinking_cut - rule.get('prefix', 0)
                due = 'rotate' in rule and i > 0 and ctx - base >= rule['rotate'] and (call['turn'] or not rule.get('at_turn'))
                if due:
                    base = ctx - start - handover
                    write += start + handover
                ctx = max(ctx - base, 0)
                if rule.get('thinking'):
                    thinking_cut += call['thinking']
                write += min(call['write'], ctx)
                read += ctx
            table[name][0] += read
            table[name][1] += write
    return table


def quantiles(values):
    values = sorted(values)
    return (statistics.median(values), values[int(len(values) * 0.9)], values[-1]) if values else (0, 0, 0)


def manager_log_stats(path):
    ctx, calls, fresh, rotations, prev = [], [], [], 0, None
    with open(path, encoding='utf-8') as handle:
        for line in handle:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = row.get('kind')
            if kind == 'rotated':
                rotations += 1
                prev = 'rotated'
            elif kind == 'reply':
                usage = row.get('usage') or {}
                if usage.get('context_tokens'):
                    ctx.append(usage['context_tokens'])
                    calls.append(usage.get('calls', 0))
                    if prev == 'rotated':
                        fresh.append(usage['context_tokens'])
                prev = 'reply'
    return ctx, calls, fresh, rotations


def sessions_of(folder, want_manager):
    out = []
    if not folder.is_dir():
        return out
    for file in sorted(folder.glob('*.jsonl'), key=lambda f: -f.stat().st_mtime):
        session = load_session(file)
        if len(session['calls']) < 3:
            continue
        if want_manager:
            first = next((k for c in session['calls'] for k, _ in c['inputs'] if k.startswith('host:')), '')
            if session['entry'] == 'cli' or first != 'host: manager start':
                continue
        out.append(session)
    return out


def print_start(label, session):
    first = session['calls'][0]
    print(f'{label}: first call {first["ctx"]} tokens (cache read {first["read"]}, written {first["write"]}, model {first["model"]})')
    shape = session['shape']
    if shape:
        tools = shape['tools']
        print(f'  system prompt {sum(shape["system"])} chars in {len(shape["system"])} blocks; {len(tools)} tool schemas {sum(n for n, _ in tools)} chars, largest {tools[:5]}')
    for name, chars in session['attachments'].most_common(6):
        print(f'  attachment {name}: {chars} chars')
    for file, chars in session['files'].items():
        print(f'    {file}: {chars} chars')
    host = [(k, n) for k, n in first['inputs'] if k.startswith('host:')]
    print(f'  first message: {host}')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--project', action='append', type=Path, help='main checkout of a project (default: this repository)')
    parser.add_argument('--projects-dir', type=Path, default=Path.home() / '.claude' / 'projects')
    parser.add_argument('--last', type=int, default=80, help='newest tasks per project to read (manager sessions: all)')
    parser.add_argument('--task', action='append', type=int, default=[], help='print each session of this task')
    parser.add_argument('--manager-log', type=Path, help='manager/log.jsonl of the project state folder')
    args = parser.parse_args()
    if not args.project:
        common = subprocess.run(['git', 'rev-parse', '--git-common-dir'], capture_output=True, text=True, check=True).stdout.strip()
        args.project = [Path(common).resolve().parent]

    workers, managers = [], []
    for project in args.project:
        base = slug(project.resolve())
        folders = [f for f in args.projects_dir.iterdir() if re.fullmatch(re.escape(base) + r'-t\d+', f.name)]
        folders.sort(key=lambda f: -int(f.name.rsplit('t', 1)[1]))
        for folder in folders[:args.last]:
            for session in sessions_of(folder, False):
                session['task'] = int(folder.name.rsplit('t', 1)[1])
                workers.append(session)
        managers.extend(sessions_of(args.projects_dir / base, True))
    if not workers:
        sys.exit('no worker transcripts found')
    pi = Path.home() / '.pi' / 'agent' / 'sessions' / 'wsx'
    print(f'workers: {len(workers)} sessions of {len({s["task"] for s in workers})} tasks; managers: {len(managers)} sessions; '
          f'Pi sessions under {pi}: {len(list(pi.glob("*.jsonl"))) if pi.is_dir() else "none"}')

    print('\n## Fresh start')
    print_start('worker (newest)', workers[0])
    if managers:
        print_start('manager (newest)', managers[0])
    firsts = [s['calls'][0]['ctx'] for s in workers]
    print(f'worker first call: median {statistics.median(firsts):.0f}, p10 {sorted(firsts)[len(firsts) // 10]}, p90 {quantiles(firsts)[1]}')
    if managers:
        print(f'manager first call: median {statistics.median([s["calls"][0]["ctx"] for s in managers]):.0f}')

    for label, group in (('worker', workers), ('manager', managers)):
        if not group:
            continue
        print(f'\n## {label} growth by source ({len(group)} sessions)')
        by = collections.Counter()
        for session in group:
            by.update(attribute(session))
        total = sum(v for k, v in by.items() if not k.startswith('  ')) or 1
        for part, tokens in by.most_common(14):
            print(f'  {part:28s} {tokens:>11,} {100 * tokens / total:4.0f}%')
        peaks = [max(c['ctx'] for c in s['calls']) for s in group]
        growth = [(max(c['ctx'] for c in s['calls']) - s['calls'][0]['ctx']) / (len(s['calls']) - 1) for s in group]
        print(f'  peak context median {quantiles(peaks)[0]:.0f}, p90 {quantiles(peaks)[1]}, max {quantiles(peaks)[2]}; '
              f'over 200k: {sum(p > 200_000 for p in peaks)}, over 100k: {sum(p > 100_000 for p in peaks)}; growth per call median {quantiles(growth)[0]:.0f}')
        all_turns = [t for s in group for t in turns(s)]
        calls_q, growth_q = quantiles([t[0] for t in all_turns]), quantiles([t[1] for t in all_turns])
        print(f'  host turns {len(all_turns)}: calls per turn median {calls_q[0]:.0f}, p90 {calls_q[1]}; growth per turn median {growth_q[0]:.0f}, p90 {growth_q[1]}, max {growth_q[2]}')
        cold = [c['write'] for s in group for c in cold_writes(s)]
        writes = sum(c['write'] for s in group for c in s['calls'])
        reads = sum(c['read'] for s in group for c in s['calls'])
        print(f'  cache: reads {reads:,}, writes {writes:,}; cold rewrites {len(cold)} = {sum(cold):,} tokens ({100 * sum(cold) / (writes or 1):.0f}% of writes), median {quantiles(cold)[0]:.0f}')

    if args.manager_log:
        ctx, calls, fresh, rotations = manager_log_stats(args.manager_log)
        print(f'\n## manager log: {len(ctx)} replies, {rotations} rotations; context median {quantiles(ctx)[0]:.0f}, p90 {quantiles(ctx)[1]}, max {quantiles(ctx)[2]}; '
              f'calls per reply median {quantiles(calls)[0]:.0f}, p90 {quantiles(calls)[1]}, max {quantiles(calls)[2]}; fresh first reply median {quantiles(fresh)[0]:.0f} (n {len(fresh)})')

    print('\n## Cost per task by part (dollars at memory-eval.py PRICE, every model priced as Opus 5.5)')
    per_task = collections.defaultdict(collections.Counter)
    totals = {}
    for session in workers:
        by, dollars = cost(session)
        per_task[session['task']].update(by)
        totals[session['task']] = totals.get(session['task'], 0) + dollars
    sum_by = collections.Counter()
    for by in per_task.values():
        sum_by.update(by)
    grand = sum(totals.values()) or 1
    print(f'  tasks {len(per_task)}: median ${statistics.median(totals.values()):.2f}, sum ${grand:.2f}')
    for part, dollars in sum_by.most_common(10):
        print(f'  {part:28s} ${dollars:8.2f} {100 * dollars / grand:4.0f}%')

    for label, group, start in (('worker', workers, 36_000), ('manager', managers, 47_000)):
        if not group:
            continue
        table = simulate(group, start)
        actual = table['actual'][0] or 1
        print(f'\n## {label} replay of each bound (cache-read tokens, what every call re-reads; start {start} + handover 6000)')
        print(f'  {"bound":28s} {"read M":>8s} {"write M":>8s}  reads')
        for name, (read, write) in table.items():
            print(f'  {name:28s} {read / 1e6:8.1f} {write / 1e6:8.1f} {100 * read / actual:4.0f}%')

    for task in args.task:
        sessions = [s for s in workers if s['task'] == task]
        print(f'\n## task {task}: {len(sessions) or "no"} sessions')
        for session in sessions:
            calls = session['calls']
            cold = [f'{c["ts"][11:16]} wrote {c["write"]}' for c in cold_writes(session)]
            print(f'  {Path(session["path"]).name[:8]}: {len(calls)} calls, {calls[0]["ctx"]} to {max(c["ctx"] for c in calls)} tokens, '
                  f'own output {sum(c["out"] for c in calls)} (thinking {sum(c["thinking"] for c in calls)}), cold writes {cold or "none"}')
            for part, tokens in attribute(session).most_common(6):
                print(f'    {part:28s} {tokens:>9,}')


if __name__ == '__main__':
    main()
