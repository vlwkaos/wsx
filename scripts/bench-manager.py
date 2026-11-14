#!/usr/bin/env python3
"""Benchmark one manager conversation against the real claude CLI (it spends real money).

  python3 -B scripts/bench-manager.py --mode before|after|after-nof [--cases synthetic|logged|discussion|stale|idle] [--model sonnet] [--runs 3] [--out DIR]

It plays the manager host of docs/tasks.md "Manager" for a throwaway project: a fixture repo,
a fake `wsx` on PATH that serves a fixture ledger, and fixed turns. `synthetic` is five turns
(a status question, 'build X', the owner's go after the manager sized it, an event-only turn and
an events turn with a review due). `logged` replays batches of the 2026-11-04 manager log
(docs/design/manager-reliability.md), each in a fresh conversation after its handover, and one
sonnet call judges whether each reply answers its owner messages. The instructions come from
crates/wsx/src/task/instructions.rs: `before` reads them at `--before <commit>` (default
BEFORE_COMMIT, where they were in manager.rs) and its turn text has no "Answer message <id>
first." line and no `since then:` line, `after` from the working copy; both send the ledger
summary, rotate on the last call's context and use eventModel for event-only turns. `after-nof` is
`after` with every turn on the manager model. It prints one JSON line per turn; the host itself
is tested by scripts/test-task.py. Each run ends with a `tasks` line: the calls that start, change or end a
task (argv, and stdin for a `-` prompt), the task set to compare between `before` and `after`.
`stale` is the 2026-11-13 events turn whose three limit waits had ended before the manager read
them (reply 4413 told the owner the tasks were paused until 03:20); the ledger shows the tasks
working, and a sonnet judge says whether the reply claims a limit holds now.
`idle` replays real idle returns of the wsx manager log (an owner message after a gap over 60 min that
the host rotated for) three ways: today's fresh start, the fresh start with notes the conversation
wrote at the warm moment, and a cold resume of a copy of the real transcript cut at the gap; read-only
tools, a sonnet judge per return (`--returns`, `--budget`; a return already in the out file is skipped).
`discussion` is two owner turns (review points one by one, then 'agreed on A and C, do them') and checks
the tool calls: no `wsx task new`/`add`/`steer` in the first turn, exactly one new or add in the second.
Bash runs with bypassPermissions here, so no auto-mode classifier call adds to the measured
cost; the real manager runs in auto mode.
"""
import argparse
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from datetime import datetime
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
# ^ A prompt passed as `-` comes on stdin: kept with the call, so the task set holds it.
stdin = sys.stdin.read() if '-' in sys.argv[2:] else None
with open(os.environ['BENCH_CALLS'], 'a') as out:
    out.write(json.dumps({'argv': sys.argv[1:], 'stdin': stdin}) + '\n')
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
    return re.sub(r'\\\n\s*', '', literal).replace('\\"', '"').replace('\\n', '\n').replace('\\\\', '\\')


def source_at(commit):
    """instructions.rs at `commit`, or manager.rs where the instructions were before task #336."""
    for path in ('crates/wsx/src/task/instructions.rs', 'crates/wsx/src/task/manager.rs'):
        run = subprocess.run(['git', 'show', '%s:%s' % (commit, path)], cwd=ROOT, capture_output=True, text=True)
        if run.returncode == 0 and 'fn instructions(' in run.stdout:
            return run.stdout
    raise SystemExit('no instructions() at %s' % commit)


def instructions(source, project, notes, page):
    """The manager's instructions as instructions() in task/instructions.rs formats them."""
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
        ('Events', '#5 landed r1: Fix a typo in the README; checks 1 of 1 passed; worker says (unchecked): fixed the typo',
         lambda state: (landed(state), started(state)), False),
        ('Events', '#7 ready r1: Add a --version flag; checks 1 of 1 passed; worker says (unchecked): added --version with '
                   'the commit', ready, True),
    ]
    return [([], [([(author, n, text)], change, due)
                  for n, (author, text, change, due) in enumerate(rows, 1)])]


def discussion():
    """Review points one by one, then the owner's go (docs/tasks.md "Manager": talk it over or work)."""
    points = ("A: src/cli.py does not check for an empty name. B: the README has no usage section. "
              "C: the CLI has no test.")
    return [([], [([('Owner', 1, "let's go through these three review points one by one: " + points)], None, False),
                  ([('Owner', 2, 'agreed on A and C, do them')], None, False)])]


def check_discussion(rows):
    """The calls of the two turns, by the fake wsx's tool names."""
    edits = ('wsx new', 'wsx add', 'wsx steer')
    first = {k: n for k, n in rows[0]['tools'].items() if k in edits}
    second = {k: n for k, n in rows[1]['tools'].items() if k in edits}
    if first or sum(second.values()) != 1 or not ({'wsx new', 'wsx add'} & set(second)):
        raise AssertionError('discussion calls: turn 1 %s, turn 2 %s' % (first, second))


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
    landed = ('#195 landed r3: Web Home: add and remove projects; checks 3 of 3 passed; worker says (unchecked): The whole web '
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


def stale(state):
    """The 2026-11-13 00:06 events (manager log 4409-4412), read at 00:41 when the limit had ended:
    the ledger shows the four tasks working again."""
    def open_task(n, title):
        return {'id': n, 'open': True, 'record': {'id': n, 'created_by': 'manager'},
                'row': {'id': n, 'title': title, 'state': 'working', 'revision': 0}}
    state['tasks'] = [open_task(422, 'Lean orchestration design on Fable'),
                      open_task(423, 'Usage limit falls to local model'),
                      open_task(426, 'Web GUI polish audit on Fable'),
                      open_task(427, 'Task time and cost audit on Fable')]
    state['ledger_extra']['to_check'] = 0
    limit = ('waits on limit: Waiting for the usage limit for less than a minute: claude usage limit; resumes at '
             '03:20. It goes on by itself then, one task at a time.; worker says (unchecked): ')
    release = ('release of ca9b2f227745 failed (exit 1); its output ends:\n    error: could not compile `wsx-core` '
               '(lib) due to 1 previous error\n    Error: Process completed with exit code 101.')
    return [([], [([('Events', 4409, '#426 waiting r0: Web GUI polish audit on Fable; %s\n#427 waiting r0: Task time and '
                                   'cost audit on Fable; %s' % (limit, limit)),
                    ('Events', 4410, '#423 waiting r0: Usage limit falls to local model; %sAssuming: the endpoint stays '
                                     'down while I build, so the live LFM3 proof may en…' % limit),
                    ('Events', 4411, '#422 waiting r0: Lean orchestration design on Fable; %sPlan for '
                                     'docs/design/orchestration-lean.md (doc only, no code).' % limit),
                    ('Events', 4412, release)], None, False)])]


def since_then(state, blocks):
    """The host's line under a turn's events (Manager::since_then): each task whose last
    `#<n> <state> r<k>` line no longer matches the ledger."""
    heads = {}
    for author, _, text in blocks:
        if author != 'Events':
            continue
        for line in text.splitlines():
            found = re.match(r'#(\d+) (\w+) r(\d+):', line)
            if found and found.group(2) != 'stalled':
                heads[int(found.group(1))] = (found.group(2), int(found.group(3)))
    rows = {t['id']: t['row'] for t in state['tasks']}
    changed = ['#%d %s r%d' % (n, rows[n]['state'], rows[n]['revision']) for n, head in heads.items()
               if n in rows and (rows[n]['state'], rows[n]['revision']) != head]
    return 'since then: %s\n\n' % ', '.join(changed) if changed else ''


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
    def __init__(self, launch, env, cwd, model, real=False, resume=None):
        # ^ `real` runs on the owner's real conversations (`idle`): read-only tools, so it acts on
        # nothing; `resume` forks one of them.
        tools = (['--permission-mode', 'default', '--tools', 'Read,Glob,Grep']
                 + (['--resume', resume, '--fork-session'] if resume else [])
                 if real else ['--permission-mode', 'bypassPermissions', '--allowedTools', 'Read', 'Glob', 'Grep',
                               'WebFetch', 'WebSearch', 'Skill', 'Agent'])
        self.proc = subprocess.Popen(['claude', '-p', '--input-format', 'stream-json', '--output-format',
                                      'stream-json', '--verbose', '--setting-sources', 'project,local',
                                      '--model', model, '--append-system-prompt', launch, *tools],
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
    # ^ `--help` reads a command's options (the instructions point there): no task call.
    if found:
        return 'wsx help' if '--help' in command else 'wsx ' + found.group(1)
    return 'git' if re.match(r'\s*git\b', command) else 'bash'


def run_once(mode, number, out, cases, before):
    base = Path(tempfile.mkdtemp(prefix='bench-%s-%d-' % (mode, number), dir=os.environ.get('TMPDIR')))
    project, copy, tip, state = fixture(base)
    conversations = {'synthetic': lambda: turns(copy, tip), 'logged': lambda: logged(state),
                     'discussion': discussion, 'stale': lambda: stale(state)}[cases]()
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
    source = source_at(before) if mode == 'before' else (ROOT / 'crates/wsx/src/task/instructions.rs').read_text()
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
            if mode != 'before':
                events += since_then(state, blocks)
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
    # ^ The task set: every call that starts, changes or ends a task, to compare `before` and `after`.
    writes = [call for call in map(json.loads, calls.read_text().splitlines()) if call['argv'][:1] == ['task'] and
              '--help' not in call['argv'] and
              call['argv'][1:2] and call['argv'][1] in ('new', 'add', 'steer', 'land', 'after', 'hold', 'close', 'stop')] \
        if calls.exists() else []
    out.write(json.dumps({'mode': mode, 'run': number, 'cases': cases, 'tasks': writes}) + '\n')
    print(json.dumps({'mode': mode, 'run': number, 'tasks': [call['argv'][1:] for call in writes]}), flush=True)
    shutil.rmtree(base, ignore_errors=True)
    if cases == 'discussion':
        check_discussion(rows)
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


STALE_PROMPT = """You judge one chat reply of a software project manager agent to its owner. The input JSON
holds the turn's message (events from the tool wsx and the ledger summary) and the manager's `reply`.
Decide `claims_limit`: the reply tells the owner, as a fact that holds now, that one or more tasks wait
for or are paused by a usage limit, or that they resume at 03:20. A reply that says the waits ended,
or says nothing about a limit, does not claim it. In `tasks`, list the task numbers the reply says
wait now (empty when none). Answer only through the schema."""
STALE_SCHEMA = {'type': 'object', 'required': ['claims_limit', 'tasks'],
                'properties': {'claims_limit': {'type': 'boolean'}, 'tasks': {'type': 'array', 'items': {'type': 'integer'}}}}


def ask_sonnet(system, schema, data):
    run = subprocess.run(['claude', '-p', '--model', 'sonnet', '--effort', 'high', '--output-format', 'json',
                          '--no-session-persistence', '--tools', '', '--system-prompt', system,
                          '--json-schema', json.dumps(schema)],
                         input=json.dumps(data), capture_output=True, text=True, cwd=os.environ.get('TMPDIR', '/'),
                         timeout=600)
    answer = json.loads(run.stdout)
    if answer.get('is_error') or not isinstance(answer.get('structured_output'), dict):
        raise RuntimeError('the judge call failed: %s' % str(answer.get('result'))[:200])
    return answer['structured_output'], answer.get('total_cost_usd', 0.0)


def judge_stale(rows):
    """One sonnet call: whether the reply to the stale events claims the limit holds."""
    row = rows[-1]
    return ask_sonnet(STALE_PROMPT, STALE_SCHEMA, {'message': row['message'], 'reply': row['reply']})


def judge(rows):
    """One sonnet call, as scripts/retrospect.py judges: each owner id answered or not."""
    data = {'turns': [{'earlier': r['earlier'], 'message': r['message'], 'reply': r['reply'],
                       'answers': r['owner_ids']} for r in rows if r['owner_ids']]}
    answer, cost = ask_sonnet(JUDGE_PROMPT, JUDGE_SCHEMA, data)
    answers = answer['answers']
    missing = {n for r in rows for n in r['owner_ids']} - {a['id'] for a in answers}
    if missing:
        raise RuntimeError('the judge left out %s' % sorted(missing))
    return answers, cost


MANAGER_DIR = Path.home() / '.local/state/wsx/tasks/wsx-8cbe3f08/manager'
TRANSCRIPTS = Path.home() / '.claude/projects/-Users-vlwkaos-ws-wsx'
IDLE_MS = 3_600_000
NOTES_HEAD = "The earlier conversation's handoff notes:\n"
# ^ USD per million tokens of claude-opus-5-5: input, cache read, cache write, output, fitted on the
# 170 one-call opus-5-5 turns of the manager log (median error 0.3%). A forked resume reports the
# session's restored running total, not its own cost, so every arm is priced from its tokens.
PRICE = {'input': 4.0, 'cache_read': 0.2, 'cache_write': 8.0, 'output': 20.0}
ASK_TOKENS = 500


def priced(row):
    return round(sum(row[k] * rate for k, rate in PRICE.items()) / 1e6, 4)


def warm_ask(row):
    """The hidden turn as it runs at 55 min: the whole context read from the warm cache, only the
    request written. The bench's fork cannot read the resume's cache, so it pays a cold write."""
    context = row['last_context'] or 0
    return round(((context - ASK_TOKENS) * PRICE['cache_read'] + ASK_TOKENS * PRICE['cache_write']
                  + row['output'] * PRICE['output']) / 1e6, 4)


# ^ The hidden turn's request that `idle` measured (docs/design/manager-continuity.md "Written
# handover"): sent about 55 min after the last answer, while the prompt cache is warm.
HANDOVER_ASK = ("wsx handover: the owner has been away for about an hour and this conversation's prompt "
                "cache ends soon. If the owner comes back later, a fresh conversation continues from your "
                "notes, the notes files and the ledger; if sooner, this conversation goes on. Do no other "
                "work now: run no command and change no file. Answer only with handoff notes for that "
                "conversation, at most 6 KB: the open threads (what the owner was asking or discussing, in "
                "the owner's words where they matter), what you promised and have not done, the decisions "
                "still open with their options, and what the ledger, decisions.md and context.md do not "
                "hold. Leave out what the ledger shows. The owner does not see this answer.")


def stamp_ms(line):
    stamp = line.get('timestamp')
    return int(datetime.fromisoformat(stamp.replace('Z', '+00:00')).timestamp() * 1000) if stamp else None


def transcript(path):
    lines = []
    for raw in path.read_text().splitlines():
        try:
            line = json.loads(raw)
        except ValueError:
            continue
        if isinstance(line, dict):
            lines.append(line)
    return lines


def holding(text):
    """The manager transcripts that hold `text` (a run with no quote or backslash, as JSON keeps it)."""
    probe = max(re.findall(r'[^"\\\n]{1,60}', text) or [''], key=len)
    if len(probe) < 8:
        return []
    found = subprocess.run(['rg', '-l', '-F', probe, str(TRANSCRIPTS)], capture_output=True, text=True).stdout
    return [Path(f) for f in found.split() if not Path(f).name.startswith('agent-')]


def first_text(lines):
    for line in lines:
        if line.get('type') == 'user':
            content = line['message'].get('content')
            if isinstance(content, list):
                content = ''.join(b.get('text', '') for b in content if b.get('type') == 'text')
            if content:
                return content
    return ''


def idle_returns(count):
    """The newest `count` owner returns after an idle gap that the host rotated for: the owner's
    message, the exchanges before the gap, the conversation that went idle (its transcript up to
    the gap) and the fresh conversation's first message (today's handover)."""
    entries = [json.loads(line) for line in (MANAGER_DIR / 'log.jsonl').read_text().splitlines() if line.strip()]
    found, last = [], None
    for k, entry in enumerate(entries):
        if entry['kind'] == 'reply' and entry.get('usage') and not entry.get('post'):
            last = entry
        elif (entry['kind'] == 'message' and entry.get('from') == 'owner' and last
              and entry['unix_ms'] - last['unix_ms'] > IDLE_MS):
            after = next((e for e in entries[k + 1:k + 8] if e['kind'] in ('rotated', 'reply')), {})
            if str(after.get('reason', '')).startswith('idle over 60 min'):
                found.append((k, last))
            last = None
    cases = []
    for k, last in reversed(found):
        gap = entries[k]['unix_ms']
        said = [e for e in entries[:k] if e['kind'] == 'reply' and e.get('text') and e.get('usage')
                and not e.get('post') and not e.get('provisional')]
        # ^ The conversation that went idle: the newest transcript with the last reply before the
        # gap whose last line is before it.
        before = [(max(filter(None, map(stamp_ms, lines)), default=0), path, lines) for path in holding(said[-1]['text'])
                  for lines in [transcript(path)]]
        before = [b for b in before if said[-1]['unix_ms'] - 600_000 <= b[0] < gap]
        fresh = [first_text(lines) for path in holding(entries[k]['text']) for lines in [transcript(path)]
                 if 'Handover (' in first_text(lines) and (min(filter(None, map(stamp_ms, lines)), default=0) >= gap)]
        if not before or not fresh:
            print('idle return %d: no transcripts, left out' % entries[k]['id'], flush=True)
            continue
        talk = [e for e in entries[:k] if e['kind'] == 'message' and e.get('from') == 'owner'
                or e['kind'] == 'reply' and e.get('text') and not e.get('post')][-8:]
        later = entries[k + 1:]
        cases.append({'id': entries[k]['id'], 'gap_min': (gap - last['unix_ms']) // 60000,
                      'message': entries[k]['text'], 'gap_ms': gap,
                      'earlier': ['%s %d: %s' % ('Owner' if e['kind'] == 'message' else 'Manager', e['id'],
                                                 e['text'][:1500]) for e in talk],
                      'real_reply': next((e['text'][:2000] for e in later if e['kind'] == 'reply' and e.get('text')), ''),
                      'followups': [e['text'][:600] for e in later if e['kind'] == 'message' and e.get('from') == 'owner'][:2],
                      'idle': max(before, key=lambda b: b[0])[2], 'first': min(fresh, key=len)})
        if len(cases) == count:
            break
    return list(reversed(cases))


def split_first(first):
    """Today's first message: (notes preamble, handover block, the turn's own text)."""
    at = first.index('Handover (')
    end = first.index('\n\n', at)
    if first[end + 2:].startswith('Routine results ('):
        end = first.index('\n\n', end + 2)
    return first[:at], first[at:end + 2], first[end + 2:].lstrip('\n')


def fork_copy(lines, gap_ms, cwd):
    """The idle conversation up to the gap, as a new session of `cwd` for `--resume`."""
    session = str(uuid.uuid4())
    kept = []
    for line in lines:
        if (stamp_ms(line) or 0) >= gap_ms:
            break
        kept.append(json.dumps({**line, 'sessionId': session}) if 'sessionId' in line else json.dumps(line))
    folder = Path.home() / '.claude/projects' / re.sub(r'[^A-Za-z0-9]', '-', str(cwd.resolve()))
    folder.mkdir(parents=True, exist_ok=True)
    (folder / ('%s.jsonl' % session)).write_text('\n'.join(kept) + '\n')
    return session, folder


IDLE_JUDGE = """You judge replies of a software project manager agent to its owner, who came back after
more than an hour away. `earlier` holds the chat before the gap, `message` the owner's message on
return, `followups` the owner's next messages (they show what the owner meant) and `real_reply` the
reply the owner got then (a reference, not always right). For each of `replies` decide `thread`:
"yes" when it picks up the open thread the owner refers to and acts on or answers it; "partly" when
it finds the thread but misses or mistakes part of it; "no" when it takes the wrong thread, asks what
the owner means though the earlier chat makes it clear, or answers generically. One short question
when the earlier chat truly leaves it open is "yes". Judge continuity with the earlier chat only, not
style or length; the replies could run no command. In `why`, at most 20 words. Answer only through
the schema."""
IDLE_SCHEMA = {'type': 'object', 'required': ['replies'], 'properties': {'replies': {'type': 'array', 'items': {
    'type': 'object', 'required': ['label', 'thread', 'why'],
    'properties': {'label': {'type': 'string'}, 'thread': {'enum': ['yes', 'partly', 'no']},
                   'why': {'type': 'string'}}}}}}


def run_idle(options, out):
    """Each idle return three ways: today's fresh start (`fresh`), the fresh start with the notes
    the hidden turn writes (`notes`; the turn itself, `ask`, forks the idle conversation after the
    cold resume wrote its cache, so it costs as the warm turn at 55 min does) and the cold resume
    (`resume`). Every arm has read-only tools: no command runs."""
    spent = 0.0
    source = (ROOT / 'crates/wsx/src/task/instructions.rs').read_text()
    launch = instructions(source, 'wsx', str(MANAGER_DIR / 'notes'), str(MANAGER_DIR / 'overview.md'))
    env = {k: v for k, v in os.environ.items() if not k.startswith('WSX_')}
    path = Path(out.name)
    done = {json.loads(line).get('id') for line in path.read_text().splitlines() if line.strip()}
    cases = idle_returns(options.returns)
    for case in cases:
        if case['id'] in done:
            print('idle return %d: in %s already' % (case['id'], path), flush=True)
            continue
        base = Path(tempfile.mkdtemp(prefix='bench-idle-', dir=os.environ.get('TMPDIR')))
        session, folder = fork_copy(case['idle'], case['gap_ms'], base)
        try:
            preamble, handover, turn = split_first(case['first'])
            rows = {}
            for arm, text, resume in (('resume', turn, session), ('ask', HANDOVER_ASK, session)):
                agent = Agent(launch, env, base, MAIN_MODEL, real=True, resume=resume)
                rows[arm] = agent.turn(text)
                agent.stop()
            notes = rows['ask']['reply']
            for arm, text in (('fresh', case['first']),
                              ('notes', preamble + NOTES_HEAD + notes + '\n\n' + handover + turn)):
                agent = Agent(launch, env, base, MAIN_MODEL, real=True)
                rows[arm] = agent.turn(text)
                agent.stop()
        finally:
            # ^ The copy, its forks and the scratch folder.
            shutil.rmtree(folder, ignore_errors=True)
            shutil.rmtree(base, ignore_errors=True)
        labels = dict(zip(('fresh', 'notes', 'resume'), random.Random(case['id']).sample('XYZ', 3)))
        verdict, cost = ask_sonnet(IDLE_JUDGE, IDLE_SCHEMA, {
            **{k: case[k] for k in ('earlier', 'message', 'followups', 'real_reply')},
            'replies': sorted(({'label': labels[a], 'reply': rows[a]['reply'][:3000]} for a in labels),
                              key=lambda r: r['label'])})
        by_label = {r['label']: r for r in verdict['replies']}
        spent += cost + sum(priced(r) for r in rows.values())
        row = {'id': case['id'], 'gap_min': case['gap_min'], 'message': case['message'][:200],
               'notes_bytes': len(notes.encode()), 'ask_warm_usd': warm_ask(rows['ask']),
               **{arm: {'usd': priced(rows[arm]), **{k: rows[arm][k] for k in
                                                       ('input', 'cache_write', 'cache_read', 'output', 'last_context')}}
                  for arm in rows},
               **{'%s_thread' % arm: by_label.get(labels[arm], {}).get('thread') for arm in labels},
               **{'%s_why' % arm: by_label.get(labels[arm], {}).get('why') for arm in labels},
               'replies': {arm: rows[arm]['reply'] for arm in rows}}
        out.write(json.dumps(row) + '\n')
        out.flush()
        print(json.dumps({k: v for k, v in row.items() if k != 'replies'}), flush=True)
        print('spent so far: %.3f USD' % spent, flush=True)
        if spent > options.budget:
            sys.exit('over the budget of %.0f USD' % options.budget)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mode', required=True, choices=['before', 'after', 'after-nof'])
    parser.add_argument('--cases', default='synthetic', choices=['synthetic', 'logged', 'discussion', 'stale', 'idle'])
    parser.add_argument('--returns', type=int, default=6, help='idle: how many idle returns')
    parser.add_argument('--budget', type=float, default=BUDGET_USD)
    parser.add_argument('--before', default=BEFORE_COMMIT, help='the commit whose instructions `before` reads')
    parser.add_argument('--model', help='the model of every turn, to bound the cost')
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--out', default=os.environ.get('TMPDIR', '.'))
    options = parser.parse_args()
    if options.model:
        global MAIN_MODEL, EVENT_MODEL
        MAIN_MODEL = EVENT_MODEL = options.model
    Path(options.out).mkdir(parents=True, exist_ok=True)
    path = Path(options.out) / ('bench-%s-%s.jsonl' % (options.cases, options.mode))
    spent = 0.0
    with path.open('a') as out:
        if options.cases == 'idle':
            return run_idle(options, out)
        for number in range(1, options.runs + 1):
            rows = run_once(options.mode, number, out, options.cases, options.before)
            spent += sum(row['cost_usd'] for row in rows)
            if options.cases == 'stale':
                verdict, cost = judge_stale(rows)
                spent += cost
                print(json.dumps({'mode': options.mode, 'run': number, **verdict, 'reply': rows[-1]['reply'][:400]}), flush=True)
                out.write(json.dumps({'mode': options.mode, 'run': number, 'judged': verdict}) + '\n')
            if options.cases == 'logged':
                answers, cost = judge(rows)
                spent += cost
                for row in answers:
                    print(json.dumps({'mode': options.mode, 'run': number, **row}), flush=True)
                    out.write(json.dumps({'mode': options.mode, 'run': number, 'judged': row}) + '\n')
            print('spent so far: %.3f USD' % spent, flush=True)
            if spent > options.budget:
                sys.exit('over the budget of %.0f USD' % options.budget)
    print('rows in', path)


if __name__ == '__main__':
    main()
