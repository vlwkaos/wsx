#!/usr/bin/env python3
"""Benchmark one manager conversation against the real claude CLI (it spends real money).

  python3 -B scripts/bench-manager.py --mode before|after|after-nof [--cases synthetic|logged] [--runs 3] [--out DIR]

It plays the manager host of docs/tasks.md "Manager" for a throwaway project: a fixture repo,
a fake `wsx` on PATH that serves a fixture ledger, and fixed turns. `synthetic` is five turns
(a status question, 'build X', the owner's go after the manager sized it, an event-only turn and
an events turn with a review due). `logged` replays batches of the 2026-11-04 manager log
(docs/design/manager-reliability.md), each in a fresh conversation after its handover, and one
sonnet call judges whether each reply answers its owner messages. The instructions come from
crates/wsx/src/task/manager.rs: `before` reads them at BEFORE_COMMIT and its turn text has no
"Answer message <id> first." line, `after` from the working copy; both send the ledger summary,
rotate on the last call's context and use eventModel for event-only turns. `after-nof` is
`after` with every turn on the manager model. It prints one JSON line per turn; the host itself
is tested by scripts/test-task.py.
Bash runs with bypassPermissions here, so no auto-mode classifier call adds to the measured
cost; the real manager runs in auto mode.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BEFORE_COMMIT = '9543a8de'
MAIN_MODEL, EVENT_MODEL = 'opus', 'sonnet'
ROTATE_TOKENS = 400000
TURN_SECONDS = 400
BUDGET_USD = 12.0

FAKE_WSX = r'''#!/usr/bin/env python3
import json, os, sys
state_file = os.environ['BENCH_STATE']
state = json.load(open(state_file))
with open(os.environ['BENCH_CALLS'], 'a') as out:
    out.write(json.dumps(sys.argv[1:]) + '\n')
args = [a for a in sys.argv[1:] if a not in ('--json',)]
if args[:1] != ['task']:
    print('wsx: only `wsx task` commands run here', file=sys.stderr); sys.exit(2)
verb = args[1] if len(args) > 1 else ''
def task(n):
    found = next((t for t in state['tasks'] if t['id'] == n), None)
    if not found:
        print('wsx: no task #%d' % n, file=sys.stderr); sys.exit(1)
    return found
if verb == 'ledger':
    print(json.dumps(state['ledger_extra'] | {
        'open': [t['row'] for t in state['tasks'] if t['open']],
        'recent': [t['row'] for t in state['tasks'] if not t['open']]}, indent=1))
elif verb == 'show':
    t = task(int(next(a for a in args[2:] if a.isdigit())))
    print(json.dumps({'record': t['record'], 'row': t['row']}, indent=1))
elif verb == 'new':
    title = args[args.index('--title') + 1] if '--title' in args else 'new task'
    state['created'] = title
    json.dump(state, open(state_file, 'w'))
    print(json.dumps({'id': 7, 'title': title, 'state': 'working', 'message': 'started task t7'}))
elif verb in ('add', 'steer', 'land', 'allow'):
    state.setdefault('actions', []).append(args[1:])
    json.dump(state, open(state_file, 'w'))
    print(json.dumps({'ok': True, 'message': '%s done' % verb}))
elif verb == 'check':
    print(json.dumps({'items': []}))
elif verb == 'list':
    print(json.dumps({'tasks': [t['row'] for t in state['tasks']]}))
else:
    print(json.dumps({'ok': True}))
'''


def unescape(literal):
    """A Rust string literal's text: line continuations joined, escapes resolved."""
    return re.sub(r'\\\n\s*', '', literal).replace('\\"', '"').replace('\\\\', '\\')


def instructions(source, project, notes, page):
    """The manager's instructions as instructions() in that manager.rs formats them."""
    body = source.split('fn instructions(', 1)[1]
    literal = re.search(r'format!\(\s*"(You are the manager(?:[^"\\]|\\.)*)"', body, re.S).group(1)
    # ^ The default workflow (no review) and the shared presentation rule.
    values = {'project': project, 'notes': notes, 'overview': page, 'skills': '', 'review': '',
              'presentation': (ROOT / 'crates/wsx-core/integrations/common/presentation.md').read_text().rstrip(),
              'context': notes + '/context.md', 'decisions': notes + '/decisions.md'}
    text = re.sub(r'(?<!\{)\{(\w+)\}(?!\})', lambda m: values[m.group(1)], unescape(literal))
    return text.replace('{{', '{').replace('}}', '}')


def git(cwd, *args):
    subprocess.run(['git', '-c', 'user.name=bench', '-c', 'user.email=bench@example.invalid', *args],
                   cwd=cwd, check=True, capture_output=True)
    return subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=cwd, capture_output=True, text=True).stdout.strip()


def fixture(base):
    """A small CLI project, a working copy of #7 with one commit, and the ledger's tasks."""
    project = base / 'project'
    (project / 'src').mkdir(parents=True)
    (project / 'README.md').write_text('# bench\n\nA small CLI that prints greetings.\n')
    (project / 'src/cli.py').write_text('import argparse\n\n\ndef main():\n    parser = argparse.ArgumentParser()\n'
                                        '    parser.add_argument("name")\n    args = parser.parse_args()\n'
                                        '    print("hello " + args.name)\n')
    git(project, 'init', '-q', '-b', 'main')
    git(project, 'add', '.')
    tip = git(project, 'commit', '-q', '-m', 'initial')
    copy = base / 'copies/t7'
    shutil.copytree(project, copy)
    src = (copy / 'src/cli.py').read_text().replace(
        '    parser.add_argument("name")\n',
        '    parser.add_argument("name")\n    parser.add_argument("--version", action="version", version="bench 0.1 (" + COMMIT + ")")\n')
    (copy / 'src/cli.py').write_text('COMMIT = "abc1234"\n' + src)
    git(copy, 'commit', '-qam', 'add --version with the commit')

    def row(n, title, state, revision, **extra):
        return {'id': n, 'title': title, 'state': state, 'revision': revision, **extra}
    tasks = [
        {'id': 1, 'open': False, 'row': row(1, 'Initial CLI skeleton', 'closed', 1,
                                            memo='The CLI is src/cli.py; tests are none yet.')},
        {'id': 2, 'open': False, 'row': row(2, 'Add a config loader', 'closed', 2,
                                            memo='Config is read from bench.yml beside the binary.')},
        {'id': 3, 'open': True, 'row': row(3, 'Add JSON output', 'working', 1, checks=[], summary=None)},
        {'id': 4, 'open': True, 'row': row(4, 'Fetch release notes', 'waiting', 1,
                                           summary='Which release feed should it read?',
                                           waiting={'on': 'owner', 'reason': 'Which release feed should it read?'})},
        {'id': 5, 'open': True, 'row': row(5, 'Fix a typo in the README', 'ready', 1,
                                           checks=[{'command': 'python3 -m py_compile src/cli.py', 'outcome': 'passed'}],
                                           summary='fixed the typo')},
        {'id': 6, 'open': True, 'row': row(6, 'Add colors to the greeting', 'waiting', 1,
                                           summary='Which colors should the greeting use?',
                                           waiting={'on': 'owner', 'reason': 'Which colors should the greeting use?'})},
    ]
    state = {'tasks': tasks, 'ledger_extra': {
        'project': 'bench', 'auto_land': True, 'to_check': 1,
        'main': {'path': str(project), 'branch': 'main', 'changed_files': 0},
        'deploys': {'default_branch': 'refs/heads/main', 'tip': tip,
                    'deploy': {'commit': tip, 'behind': 0, 'failed_since': 0, 'configured': True},
                    'release': {'commit': None, 'failed_since': 0, 'configured': False}},
        'open_omitted': 0, 'approvals': []}}
    return project, copy, tip, state


def turns(copy, tip):
    """The synthetic conversation: (handover, turns); a turn is (blocks of (author, id, text), change
    to the fixture before it, review due). A reply answers the turn's Owner messages."""
    def started(state):
        state['tasks'].append({'id': 7, 'open': True, 'row': {
            'id': 7, 'title': state.get('created') or 'Add a --version flag', 'state': 'working', 'revision': 0},
            'record': {'id': 7, 'created_by': 'manager', 'working_copy': str(copy), 'base': tip}})

    def landed(state):
        five = next(t for t in state['tasks'] if t['id'] == 5)
        five['open'], five['row'] = False, {'id': 5, 'title': 'Fix a typo in the README', 'state': 'landed', 'revision': 1}

    def ready(state):
        seven = next(t for t in state['tasks'] if t['id'] == 7)
        seven['row'].update(state='ready', revision=1, summary='added --version with the commit',
                            checks=[{'command': 'python3 -m py_compile src/cli.py', 'outcome': 'passed'}])
    rows = [
        ('Owner', 'What is the status of the project?', None, False),
        ('Owner', 'Please add a --version flag to the CLI.', None, False),
        ('Owner', 'Yes, go ahead as one task; --version should also print the short git commit.', None, False),
        ('Events', '#5 landed r1: Fix a typo in the README; checks 1 of 1 passed; worker says: fixed the typo',
         lambda state: (landed(state), started(state)), False),
        ('Events', '#7 ready r1: Add a --version flag; checks 1 of 1 passed; worker says: added --version with '
                   'the commit', ready, True),
    ]
    return [([], [([(author, n, text)], change, due)
                  for n, (author, text, change, due) in enumerate(rows, 1)])]


def logged(state):
    """The logged batches (docs/design/manager-reliability.md "A", "C", "E"), clipped as the
    events are, with the ledger they met; the 2312 screenshot is a line of text."""
    def open_task(n, title, state_, **extra):
        return {'id': n, 'open': state_ != 'landed', 'record': {'id': n, 'created_by': 'manager'},
                'row': {'id': n, 'title': title, 'state': state_, 'revision': 1, **extra}}
    state['tasks'] = [
        open_task(184, 'Web: header at the bottom (setting) and routine line', 'ready', summary='header moved'),
        open_task(195, 'Web Home: add and remove projects', 'landed'),
        open_task(197, 'Routines in-house: replace vendored asched', 'ready', summary='asched moved into wsxd'),
        open_task(198, 'Chat line when a project agent session exits', 'ready', summary='chat line added'),
        open_task(206, 'Manager reply keeps every text block of a turn', 'waiting',
                  waiting={'on': 'owner', 'reason': 'the CI task scenario has not run'}),
    ]
    state['ledger_extra']['to_check'] = 0
    first = ('Owner: claude in main exited means what? and should show on the chat log as well I think',
             'You: "claude in main exited" means a Claude session that you opened yourself has ended, in the '
             "project's `main` worktree. It is not a task worker or the manager. I started #198 to also put "
             'these lines into the project chat.')
    review = ('Owner: waiting fir the managers review means what?',
              'You: #184 is landing now. It reruns its checks first. I sent #198 back for one small fix: a code '
              'comment was on the wrong line. #197 is the large one, and a second reviewer is still reading it. '
              "I'll land or return it when that review comes back.")
    gaps = ('Owner: is it batched if so when and how. also this answer hidden bug when was this introduced',
            'You: Are reviews batched? Partly. There is no schedule. Each event that names a ready task starts a '
            'manager turn, and in that turn I review the diff. Events that arrive within 3 seconds of each other '
            "become one message. The hidden-answer bug came on 2026-10-16 with commit `ed76c08c`, the Claude "
            'adapter: it took a reply from the final result, which holds only the last text block.',
            'Owner: why didnt we catch it',
            'You: There were three gaps: 1. The test fake matched the bug: each fake turn sent one text block. '
            '2. Nothing compared the fake with the real Claude. 3. The loss gave no signal. To stop this class '
            'of bug, I would add one check: record a few real Claude turns once, then test the adapter against '
            'those recordings as well as the fake.')
    landed = ('#195 landed r3: Web Home: add and remove projects; checks 3 of 3 passed; worker says: The whole web '
              'check (7) passes, exit 0, on the last commit. That includes my "Projects: add and remove" '
              'scenario and scripts/web-projects-check.mjs. The working…; memo: The web client lists '
              "wsxd's snapshot, not the config.")
    return [
        (list(first), [([('Owner', 2284, 'waiting fir the managers review means what?'),
                         ('Events', 2282, landed), ('Events', 2283, 'deploy of 1e580514cf94 succeeded after 1014 s')],
                        None, False)]),
        (list(review), [([('Owner', 2312, 'here I think my question for waiting for managers review was ignored '
                                          'due new session?\ncan you answer that\nand also improve this\n\n'
                                          '[attached image: the chat, your reply above under my question]')],
                         None, False)]),
        (list(gaps), [([('Owner', 2326, 'yes. but remember wsx is not only for claude.\nalso what I was curious '
                                        'was that when it is waiting for mamagers review it sends you message '
                                        'back? like how do you know or when to review')], None, False),
                      ([('Owner', 2327, 'is it polled etc')], None, False)]),
    ]


def summary(state, baseline):
    """The host's ledger summary, in the format of Manager::ledger_summary."""
    counts = {}
    for t in state['tasks']:
        if t['open']:
            counts[t['row']['state']] = counts.get(t['row']['state'], 0) + 1
    seen = {t['id']: (t['row']['state'], t['row']['revision']) for t in state['tasks']}
    rows = ['#%d %s r%d: %s' % (t['id'], t['row']['state'], t['row']['revision'], t['row']['title'][:60])
            for t in state['tasks']
            if (baseline is None and t['open']) or (baseline is not None and baseline.get(t['id']) != seen[t['id']])]
    out = ('Ledger summary from wsx (`wsx task ledger -p bench` has the details):\n- open: %s; to check: 1; '
           'approvals waiting: 0\n' % ', '.join('%d %s' % (n, s) for s, n in sorted(counts.items())))
    if rows:
        out += '- %s:\n' % ('open tasks' if baseline is None else 'changed since your last turn')
        out += ''.join('  %s\n' % row for row in rows)
    tip = state['ledger_extra']['deploys']['tip']
    return out + '- deploy: %s live, 0 commit(s) behind\n' % tip[:7], seen


class Agent:
    def __init__(self, launch, env, cwd, model):
        self.proc = subprocess.Popen(['claude', '-p', '--input-format', 'stream-json', '--output-format',
                                      'stream-json', '--verbose', '--setting-sources', 'project,local',
                                      '--permission-mode', 'bypassPermissions', '--model', model,
                                      '--append-system-prompt', launch, '--allowedTools', 'Read', 'Glob', 'Grep',
                                      'WebFetch', 'WebSearch', 'Skill', 'Agent'],
                                     cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.model, self.cost_base, self.requests = model, 0.0, 0

    def write(self, value):
        self.proc.stdin.write(json.dumps(value) + '\n')
        self.proc.stdin.flush()

    def set_model(self, model):
        self.requests += 1
        self.write({'type': 'control_request', 'request_id': 'b%d' % self.requests,
                    'request': {'subtype': 'set_model', 'model': model}})
        self.model = model

    def turn(self, text):
        timer = threading.Timer(TURN_SECONDS, self.proc.kill)
        timer.start()
        started = time.monotonic()
        self.write({'type': 'user', 'message': {'role': 'user', 'content': text}})
        tools, context, models, said = {}, None, set(), []
        try:
            for line in self.proc.stdout:
                value = json.loads(line)
                if value.get('type') == 'assistant' and not value.get('parent_tool_use_id'):
                    message = value['message']
                    models.add(message.get('model'))
                    if message.get('usage'):
                        context = context_of(message['usage'])
                    for block in message.get('content', []):
                        if block.get('type') == 'text' and block.get('text', '').strip():
                            said.append(block['text'].strip())
                        if block.get('type') == 'tool_use':
                            kind = tool_kind(block)
                            tools[kind] = tools.get(kind, 0) + 1
                if value.get('type') == 'result':
                    usage = value.get('usage', {})
                    total = value.get('total_cost_usd') or 0.0
                    cost, self.cost_base = total - self.cost_base, total
                    return {'wall_s': round(time.monotonic() - started, 1), 'models': sorted(m for m in models if m),
                            'output': usage.get('output_tokens', 0), 'input': usage.get('input_tokens', 0),
                            'cache_read': usage.get('cache_read_input_tokens', 0),
                            'cache_write': usage.get('cache_creation_input_tokens', 0),
                            'summed_context': context_of(usage),
                            'last_context': context, 'cost_usd': round(cost, 5), 'tools': tools,
                            'reply': '\n\n'.join(said) or value.get('result', '')}
        finally:
            timer.cancel()
        raise RuntimeError('claude ended during a turn')

    def stop(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def context_of(usage):
    return sum(usage.get(k, 0) for k in ('input_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens'))


def tool_kind(block):
    if block['name'] != 'Bash':
        return block['name']
    command = block['input'].get('command', '')
    found = re.search(r'wsx task (\w+)', command)
    if found:
        return 'wsx ' + found.group(1)
    return 'git' if re.match(r'\s*git\b', command) else 'bash'


def run_once(mode, number, out, cases):
    base = Path(tempfile.mkdtemp(prefix='bench-%s-%d-' % (mode, number), dir=os.environ.get('TMPDIR')))
    project, copy, tip, state = fixture(base)
    conversations = turns(copy, tip) if cases == 'synthetic' else logged(state)
    notes = base / 'manager/notes'
    notes.mkdir(parents=True)
    page = base / 'overview.md'
    page.write_text('# bench\n\n## Goal\n\nA small greeting CLI.\n\n## Tasks\n\n#3 to #6 open.\n')
    bindir = base / 'bin'
    bindir.mkdir()
    (bindir / 'wsx').write_text(FAKE_WSX)
    (bindir / 'wsx').chmod(0o755)
    state_file, calls = base / 'state.json', base / 'calls.jsonl'
    state_file.write_text(json.dumps(state))
    env = dict(os.environ, PATH='%s:%s' % (bindir, os.environ['PATH']), BENCH_STATE=str(state_file),
               BENCH_CALLS=str(calls), GIT_OPTIONAL_LOCKS='0')
    source = (subprocess.run(['git', 'show', '%s:crates/wsx/src/task/manager.rs' % BEFORE_COMMIT], cwd=ROOT,
                             capture_output=True, text=True, check=True).stdout if mode == 'before'
              else (ROOT / 'crates/wsx/src/task/manager.rs').read_text())
    launch = instructions(source, 'bench', str(notes), str(page))
    rows, index = [], 0
    for history, conversation in conversations:
        agent, baseline = None, None
        for blocks, change, review_due in conversation:
            index += 1
            state = json.loads(state_file.read_text())
            if change:
                change(state)
                state_file.write_text(json.dumps(state))
            # ^ As next_turn writes it: the owner's messages, the line that puts them first, the events.
            owner = [n for author, n, _ in blocks if author == 'Owner']
            message = ''.join('%s (message %d):\n%s\n\n' % (author, n, text)
                              for author, n, text in blocks if author == 'Owner')
            events = ''.join('Events (message %d):\n%s\n\n' % (n, text) for author, n, text in blocks
                             if author == 'Events')
            if mode != 'before' and owner and events:
                message += 'Answer message%s %s first.\n\n' % ('s' if len(owner) > 1 else '',
                                                               ', '.join(map(str, owner)))
            message = (message + events).rstrip()
            model = EVENT_MODEL if mode != 'after-nof' and not owner and not review_due else MAIN_MODEL
            fresh = agent is None
            if fresh:
                if history:
                    message = ("Handover (this is a new conversation; the last exchanges before it, read the "
                               "ledger for the state):\n%s\n\n%s" % ('\n'.join(history[-4:]), message))
                agent, baseline = Agent(launch, env, project, model), None
            elif agent.model != model:
                agent.set_model(model)
            text_summary, baseline = summary(state, baseline)
            result = agent.turn(message + '\n\n' + text_summary.rstrip())
            earlier = history[-4:]
            history += ['Owner: ' + text for author, _, text in blocks if author == 'Owner']
            history.append('You: ' + ' '.join(result['reply'].split())[:600])
            rotated = (result['last_context'] or result['summed_context']) > ROTATE_TOKENS
            row = {'mode': mode, 'run': number, 'turn': index, 'authors': sorted({a for a, _, _ in blocks}),
                   'owner_ids': owner, 'earlier': earlier, 'message': message, 'review_due': review_due,
                   'fresh': fresh, 'rotated': rotated, **result}
            rows.append(row)
            out.write(json.dumps(row) + '\n')
            out.flush()
            print(json.dumps({k: row[k] for k in ('mode', 'run', 'turn', 'owner_ids', 'models', 'wall_s', 'cost_usd',
                                                  'cache_write', 'cache_read', 'last_context', 'rotated', 'tools')}),
                  flush=True)
            if rotated:
                agent.stop()
                agent = None
        if agent:
            agent.stop()
    shutil.rmtree(base, ignore_errors=True)
    return rows


JUDGE_PROMPT = """You judge a software project manager agent's chat replies to its owner. The input JSON has
`turns`; each holds the exchanges before it (`earlier`), the turn's message (owner messages with
their ids, and events from the tool wsx) and the manager's `reply`. Return one item for every id
of every `answers` list. Decide `answered`: the reply answers every question and ask of that
owner message, and an ask to improve something is met with a concrete step. In `missing`, say in at most 15 words what was left
out; leave it empty when answered. In `kinds`, list each failure that applies, also when
answered: A a question of the owner left unanswered; C a fix narrower than the owner's words (one
agent adapter, one config); E a name the owner has not used left undefined, or an answer that
contradicts an earlier one without saying how both hold. Answer only through the schema."""
JUDGE_SCHEMA = {'type': 'object', 'required': ['answers'], 'properties': {'answers': {'type': 'array', 'items': {
    'type': 'object', 'required': ['id', 'answered', 'missing', 'kinds'],
    'properties': {'id': {'type': 'integer'}, 'answered': {'type': 'boolean'}, 'missing': {'type': 'string'},
                   'kinds': {'type': 'array', 'items': {'enum': ['A', 'C', 'E']}}}}}}}


def judge(rows):
    """One sonnet call, as scripts/retrospect.py judges: each owner id answered or not."""
    data = {'turns': [{'earlier': r['earlier'], 'message': r['message'], 'reply': r['reply'],
                       'answers': r['owner_ids']} for r in rows if r['owner_ids']]}
    run = subprocess.run(['claude', '-p', '--model', 'sonnet', '--effort', 'high', '--output-format', 'json',
                          '--no-session-persistence', '--tools', '', '--system-prompt', JUDGE_PROMPT,
                          '--json-schema', json.dumps(JUDGE_SCHEMA)],
                         input=json.dumps(data), capture_output=True, text=True, cwd=os.environ.get('TMPDIR', '/'),
                         timeout=600)
    answer = json.loads(run.stdout)
    if answer.get('is_error') or not isinstance(answer.get('structured_output'), dict):
        raise RuntimeError('the judge call failed: %s' % str(answer.get('result'))[:200])
    answers = answer['structured_output']['answers']
    missing = {n for r in rows for n in r['owner_ids']} - {a['id'] for a in answers}
    if missing:
        raise RuntimeError('the judge left out %s' % sorted(missing))
    return answers, answer.get('total_cost_usd', 0.0)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mode', required=True, choices=['before', 'after', 'after-nof'])
    parser.add_argument('--cases', default='synthetic', choices=['synthetic', 'logged'])
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--out', default=os.environ.get('TMPDIR', '.'))
    options = parser.parse_args()
    Path(options.out).mkdir(parents=True, exist_ok=True)
    path = Path(options.out) / ('bench-%s-%s.jsonl' % (options.cases, options.mode))
    spent = 0.0
    with path.open('a') as out:
        for number in range(1, options.runs + 1):
            rows = run_once(options.mode, number, out, options.cases)
            spent += sum(row['cost_usd'] for row in rows)
            if options.cases == 'logged':
                answers, cost = judge(rows)
                spent += cost
                for row in answers:
                    print(json.dumps({'mode': options.mode, 'run': number, **row}), flush=True)
                    out.write(json.dumps({'mode': options.mode, 'run': number, 'judged': row}) + '\n')
            print('spent so far: %.3f USD' % spent, flush=True)
            if spent > BUDGET_USD:
                sys.exit('over the budget of %.0f USD' % BUDGET_USD)
    print('rows in', path)


if __name__ == '__main__':
    main()
