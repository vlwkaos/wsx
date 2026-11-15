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
    def __init__(self, launch, env, cwd, model, real=False, resume=None, bash=False):
        # ^ `real` runs on the owner's real conversations (`idle`, `events`): read-only tools, so it
        # acts on nothing; `resume` forks one of them. `bash` (`events`) adds Bash, whose `wsx` is
        # the replay's recorder, at the manager's effort.
        tools = ((['--permission-mode', 'bypassPermissions', '--tools', 'Bash,Read,Glob,Grep', '--effort', 'high']
                  if bash else ['--permission-mode', 'default', '--tools', 'Read,Glob,Grep'])
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
        tools, context, first, models, said = {}, None, None, set(), []
        try:
            for line in self.proc.stdout:
                value = json.loads(line)
                if value.get('type') == 'assistant' and not value.get('parent_tool_use_id'):
                    message = value['message']
                    models.add(message.get('model'))
                    if message.get('usage'):
                        context = context_of(message['usage'])
                        first = first or context
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
                            'first_context': first, 'last_context': context, 'cost_usd': round(cost, 5), 'tools': tools,
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


def ask_sonnet(system, schema, data, model='sonnet'):
    run = subprocess.run(['claude', '-p', '--model', model, '--effort', 'high', '--output-format', 'json',
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


def priced(row, rates=PRICE):
    return round(sum(row[k] * rate for k, rate in rates.items()) / 1e6, 4)


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


def holding(text, root=TRANSCRIPTS):
    """The manager transcripts under `root` that hold `text` (a run with no quote or backslash, as
    JSON keeps it)."""
    probe = max(re.findall(r'[^"\\\n]{1,60}', text) or [''], key=len)
    if len(probe) < 8:
        return []
    found = subprocess.run(['rg', '-l', '-F', probe, str(root)], capture_output=True, text=True).stdout
    return [Path(f) for f in found.split() if not Path(f).name.startswith('agent-')]


def prompt_text(line):
    """A user line's own text (a prompt, not a tool result), else ''."""
    if line.get('type') != 'user':
        return ''
    content = line['message'].get('content')
    if isinstance(content, list):
        content = ''.join(b.get('text', '') for b in content if b.get('type') == 'text')
    return content or ''


def first_text(lines):
    return next((text for text in map(prompt_text, lines) if text), '')


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


# ^ USD per million tokens of claude-sonnet-5-5 (the claude-api reference, prices of 2026-10-06):
# a cache read costs what Opus 5.5's does; a cache write is twice the input price, as PRICE's is.
SONNET_PRICE = {'input': 2.0, 'cache_read': 0.2, 'cache_write': 4.0, 'output': 10.0}
STATE = Path.home() / '.local/state/wsx/tasks'
CLAUDE_PROJECTS = Path.home() / '.claude/projects'
CACHE_MS = 3_600_000
DAY_MS = 86_400_000
# ^ `wsx task` verbs that only read (crates/wsx/src/task/args.rs); every other verb acts.
READ_VERBS = 'list show why review ledger deployed digest search chat help manager'
KINDS = ('failure', 'question', 'ready', 'landed', 'deploy', 'other')
EVENT_WSX = r'''#!/usr/bin/env python3
import json, os, sys
# ^ wsx bench replay: every call is recorded; a read cannot show the state of the replayed turn.
stdin = sys.stdin.read() if '-' in sys.argv[2:] else None
with open(os.environ['BENCH_CALLS'], 'a') as out:
    out.write(json.dumps({'argv': sys.argv[1:], 'stdin': stdin}) + '\n')
verb = sys.argv[2] if sys.argv[1:2] == ['task'] and len(sys.argv) > 2 else ''
if verb in os.environ['BENCH_READS'].split() or '--help' in sys.argv:
    print('wsx bench replay: reads cannot run here; the turn lists the reads made then')
else:
    print(json.dumps({'ok': True, 'message': 'wsx bench replay: recorded, not run'}, indent=1))
'''
REPLAY_NOTE = ("\n\n(wsx bench replay, not from the host: this turn runs again on a copy of the conversation. "
               "`wsx task` commands that act are recorded, not run, and answer ok. `wsx task` reads cannot "
               "run, and git and files show today's checkout, not the state of then. The reads this turn "
               "made then, with their output:\n%s\nDecide and answer as you would have then.)")
EVENT_JUDGE = """You judge turns of a software project manager agent. The manager runs coding tasks
for its owner: it reviews a ready task's result and diff, then lands it (`land`), sends it back
with a change (`add`, `steer`), closes it, starts a task (`new`), asks the owner, or does nothing
when the events need nothing. `events` is what woke the turn, `reads` what the turn could read
then, `later` what the log shows afterwards (the owner's next messages and the tasks' later events:
hindsight, not always caused by the turn). Each of `answers` is one run of the same turn: the
`wsx task` commands that act (`acts`) and the reply to the owner. For each answer give `decision`
(a few words); `same_as`, the labels of the other answers that decided the same in substance (the
same tasks landed, sent back, closed, started or asked about; wording and extra reads do not
matter); `wrong`: "none", "minor" (a needless or missing small step, or a reply that misleads a
little) or "major" (lands, closes or approves what should not; leaves a failure, a worker's
question or an owner need unhandled; sends work the wrong way; or claims what is false), with
`why` in at most 25 words; and `gaps`, the problems in a task's result this answer named, in a
few words each. Do not prefer an answer for its length or style. Answer only through the schema."""
EVENT_SCHEMA = {'type': 'object', 'required': ['answers'], 'properties': {'answers': {'type': 'array', 'items': {
    'type': 'object', 'required': ['label', 'decision', 'same_as', 'wrong', 'why', 'gaps'],
    'properties': {'label': {'type': 'string'}, 'decision': {'type': 'string'},
                   'same_as': {'type': 'array', 'items': {'type': 'string'}},
                   'wrong': {'enum': ['none', 'minor', 'major']}, 'why': {'type': 'string'},
                   'gaps': {'type': 'array', 'items': {'type': 'string'}}}}}}}
CONTEXT_ASK = ("wsx bench: run nothing and answer in one line: quote the first eight words of the "
               "owner's last message in this conversation.")


def rates(models):
    return SONNET_PRICE if any('sonnet' in m for m in models) else PRICE


def event_kind(text):
    """The kind of an events message: the most urgent of its event lines (docs/tasks.md "Manager")."""
    found = {'other'}
    for line in text.splitlines():
        if re.match(r'#\d+ failed|deploy of \S+ failed|checks? fail', line):
            found.add('failure')
        elif re.match(r'#\d+ (waiting|paused)', line) and re.search(r'waits on (plan|owner|manager)|\?$', line):
            found.add('question')
        elif re.match(r'#\d+ ready', line):
            found.add('ready')
        elif re.match(r'#\d+ (landed|closed)', line):
            found.add('landed')
        elif line.startswith('deploy of '):
            found.add('deploy')
    return min(found, key=KINDS.index)


def manager_turns(days):
    """Each project's manager turns with usage in the last `days`, in log order. `review_due`
    approximates turn_model's rule (crates/wsx/src/task/manager.rs): some task's newest event, under
    a day old, is `ready`."""
    since = time.time() * 1000 - days * DAY_MS
    projects = {}
    for log in sorted(STATE.glob('*/manager/log.jsonl')):
        entries = [json.loads(line) for line in log.read_text().splitlines() if line.strip()]
        messages = {e['id']: e for e in entries if e['kind'] == 'message' and 'id' in e}
        newest, session, turns = {}, 0, []
        for e in entries:
            if e['kind'] == 'rotated':
                session += 1
            if e['kind'] == 'message' and e.get('from') == 'events':
                for line in e['text'].splitlines():
                    head = re.match(r'#(\d+) (\w+)', line)
                    if head:
                        newest[int(head.group(1))] = (head.group(2), e['unix_ms'])
            if e['kind'] != 'reply' or not e.get('usage') or e['unix_ms'] < since:
                continue
            ids = e.get('to') or []
            events = [messages[i] for i in ids if messages.get(i, {}).get('from') == 'events']
            turns.append({'project': log.parent.parent.name, 'id': e['id'], 'ms': e['unix_ms'], 'session': session,
                          'usage': e['usage'], 'ids': ids, 'events_only': bool(ids) and len(events) == len(ids),
                          'kind': event_kind('\n'.join(m['text'] for m in events)), 'reply': e.get('text', ''),
                          'review_due': any(word == 'ready' and e['unix_ms'] - ms < DAY_MS
                                            for word, ms in newest.values())})
        projects[log.parent.parent.name] = turns
    return projects


def token_cost(usage, rates_):
    return (usage['input_tokens'] * rates_['input'] + usage['cache_read_tokens'] * rates_['cache_read']
            + usage['cache_creation_tokens'] * rates_['cache_write'] + usage['output_tokens'] * rates_['output']) / 1e6


def event_cost(projects, days, ratio=1.0):
    """What `eventModel: sonnet` would have cost on the logged turns, per 7 days. A routed turn costs
    its own tokens at Sonnet's rates, times `ratio` (Sonnet's tokens over Opus's on the replays).
    Claude caches per model: the first Sonnet call after Opus turns writes what Sonnet's cache lacks
    (all of it after a rotation or an hour), and the next Opus turn writes what Sonnet's turns added.
    That holds only where the logged turn read a warm cache; a cold one paid its write already."""
    out = dict(turns=0, events_only=0, routed=0, switches=0, opus_usd=0.0, sonnet_usd=0.0, sonnet_write_usd=0.0,
               opus_write_usd=0.0)
    gap_s = SONNET_PRICE['cache_write'] - SONNET_PRICE['cache_read']
    gap_o = PRICE['cache_write'] - PRICE['cache_read']
    for turns in projects.values():
        cached, before = {}, None
        for t in turns:
            out['turns'] += 1
            out['events_only'] += t['events_only']
            warm = bool(before) and before['session'] == t['session'] and t['ms'] - before['ms'] < CACHE_MS
            prefix = before['usage'].get('context_tokens', 0) if warm else 0
            model = 'sonnet' if t['events_only'] and not t['review_due'] else 'opus'
            last = cached.get(model)
            # ^ Before its first turn in the window, Opus holds what the log shows it read.
            held = (prefix if last is None and model == 'opus' else
                    last[1] if last and last[0] == t['session'] and t['ms'] - last[2] < CACHE_MS else 0)
            missing = max(0, prefix - held)
            if model == 'sonnet':
                out['routed'] += 1
                out['switches'] += held == 0 and warm
                out['opus_usd'] += token_cost(t['usage'], PRICE)
                out['sonnet_usd'] += token_cost(t['usage'], SONNET_PRICE) * ratio
                out['sonnet_write_usd'] += missing * gap_s / 1e6
            else:
                out['opus_write_usd'] += missing * gap_o / 1e6
            cached[model] = (t['session'], t['usage'].get('context_tokens', 0), t['ms'])
            before = t
    week = 7 / days
    out = {k: round(v * week, 2) if k.endswith('usd') else v for k, v in out.items()}
    out['saving_usd'] = round(out['opus_usd'] - out['sonnet_usd'] - out['sonnet_write_usd'] - out['opus_write_usd'], 2)
    out['saving_pct'] = round(100 * out['saving_usd'] / out['opus_usd'], 1) if out['opus_usd'] else 0.0
    return out


def event_bursts(projects, rng):
    """Up to 3 event-only turns in a row (one conversation, each within an hour of the last), in an
    order that takes the first turn's kinds in turn and, within a kind, the least used project."""
    bursts = {kind: [] for kind in KINDS}
    for turns in projects.values():
        k = 0
        while k < len(turns):
            run = [turns[k]]
            while turns[k]['events_only'] and len(run) < 3 and k + len(run) < len(turns):
                follow = turns[k + len(run)]
                if not follow['events_only'] or follow['session'] != run[-1]['session'] or follow['ms'] - run[-1]['ms'] > CACHE_MS:
                    break
                run.append(follow)
            if turns[k]['events_only']:
                bursts[run[0]['kind']].append(run)
            k += len(run)
    for runs in bursts.values():
        rng.shuffle(runs)
    used = {}
    while any(bursts.values()):
        for kind in KINDS:
            if bursts[kind]:
                run = min(bursts[kind], key=lambda r: used.get(r[0]['project'], 0))
                bursts[kind].remove(run)
                used[run[0]['project']] = used.get(run[0]['project'], 0) + 1
                yield run


def acts(command):
    """The acting `wsx task` calls of one shell command, as 'verb #n'."""
    found = []
    for verb, rest in re.findall(r'wsx task ([a-z][\w-]*)([^|;&\n]*)', command):
        if verb not in READ_VERBS.split() and '--help' not in rest:
            number = re.search(r'(?<![\w-])#?(\d+)\b', rest)
            found.append(verb + (' #' + number.group(1) if number else ''))
    return found


def original_turn(turn):
    """The turn in the manager's transcript: (its lines before the turn, the prompt, the reads it
    made, what it acted, its reply). The prompt names the first events message."""
    probe = 'Events (message %d)' % turn['ids'][0]
    paths = [p for p in holding(probe, CLAUDE_PROJECTS) if 'subagents' not in p.parts]
    # ^ A later conversation's handover can quote the prompt: the transcript with the reply first.
    head = max(re.findall(r'[^"\\\n]{1,60}', turn['reply']) or [''], key=len)
    paths.sort(key=lambda p: head not in p.read_text())
    for path in paths:
        lines = transcript(path)
        start = next((k for k, line in enumerate(lines) if probe in prompt_text(line)), None)
        if start is None:
            continue
        end = next((k for k in range(start + 1, len(lines)) if prompt_text(lines[k])), len(lines))
        uses, results, said = {}, {}, []
        for line in lines[start + 1:end]:
            content = line.get('message', {}).get('content')
            for block in content if isinstance(content, list) else []:
                if block.get('type') == 'tool_use':
                    uses[block['id']] = block
                elif block.get('type') == 'tool_result':
                    result = block.get('content')
                    results[block['tool_use_id']] = (result if isinstance(result, str) else
                                                     ''.join(b.get('text', '') for b in result or [] if isinstance(b, dict)))
                elif block.get('type') == 'text' and line.get('type') == 'assistant':
                    said.append(block['text'].strip())
        reads, done = [], []
        for key, block in uses.items():
            command = block['input'].get('command') if block['name'] == 'Bash' else None
            done += acts(command or '')
            if not acts(command or ''):
                shown = command or '%s %s' % (block['name'], json.dumps(block['input'])[:300])
                reads.append('$ %s\n%s' % (shown, results.get(key, '')[:2000]))
        return {'path': path, 'before': lines[:start], 'prompt': prompt_text(lines[start]),
                'reads': '\n\n'.join(reads)[:12000] or '(none)', 'acts': sorted(done), 'reply': '\n\n'.join(said)}
    return None


def later(turn, projects_log):
    """Hindsight for the judge: the owner's next messages and the turn's tasks' later events, 12 h."""
    entries, tasks = projects_log, set(re.findall(r'#(\d+)', turn['prompt']))
    after = [e for e in entries if turn['ms'] < e['unix_ms'] < turn['ms'] + 12 * CACHE_MS and e['kind'] == 'message']
    owner = ['Owner: ' + e['text'][:300] for e in after if e.get('from') == 'owner'][:3]
    events = [line[:200] for e in after if e.get('from') == 'events' for line in e['text'].splitlines()
              if re.match(r'#(\d+) ', line) and re.match(r'#(\d+) ', line).group(1) in tasks][:8]
    return owner + events


def replay_arm(model, launch, env, base, session, burst, results):
    """One arm of a burst: the forked conversation on `model`, its turns in order."""
    calls = base / ('calls-%s.jsonl' % model)
    agent = Agent(launch, {**env, 'BENCH_CALLS': str(calls)}, base, model, real=True, resume=session, bash=True)
    try:
        for turn in burst:
            calls.write_text('')
            row = agent.turn(turn['prompt'] + REPLAY_NOTE % turn['reads'])
            row['acts'] = sorted(acts(' ; '.join(' '.join(['wsx'] + json.loads(line)['argv'])
                                                for line in calls.read_text().splitlines() if line.strip())))
            results.setdefault(model, []).append(row)
    except Exception as error:
        results.setdefault('errors', []).append('%s: %s' % (model, error))
    finally:
        agent.stop()


def warm(row, position):
    """A replay row priced as the live turn would be: the fork's first call writes the whole
    conversation once, which the live manager reads from its warm cache."""
    moved = min(row['cache_write'], row['first_context'] or 0) if position == 0 else 0
    return priced({**row, 'cache_write': row['cache_write'] - moved, 'cache_read': row['cache_read'] + moved},
                  rates(row['models']))


def run_events(options, out):
    """`events`: the cost model on the logged turns, then real event-only bursts replayed on Opus and
    Sonnet from a fork of the manager's own transcript, judged blind by Opus (docs/design/manager-event-model.md)."""
    projects = manager_turns(options.days)
    print(json.dumps({'cost_model': event_cost(projects, options.days)}), flush=True)
    path = Path(out.name)
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    done = {r['id'] for r in rows if 'id' in r}
    spent, turns_done = 0.0, len(rows)
    logs = {name: [json.loads(line) for line in (STATE / name / 'manager/log.jsonl').read_text().splitlines() if line.strip()]
            for name in projects}
    source = (ROOT / 'crates/wsx/src/task/instructions.rs').read_text()
    env = {k: v for k, v in os.environ.items() if not k.startswith('WSX_')}
    rng = random.Random(462)
    for burst in event_bursts(projects, rng):
        if turns_done >= options.samples:
            break
        if burst[0]['id'] in done:
            continue
        found = []
        for turn in burst:
            original = original_turn(turn)
            if not original or found and original['path'] != found[0]['path']:
                break
            found.append({**turn, **original})
        if not found:
            print('turn %s %d: no transcript, left out' % (burst[0]['project'], burst[0]['id']), flush=True)
            continue
        name = found[0]['project'].rsplit('-', 1)[0]
        notes = STATE / found[0]['project'] / 'manager'
        launch = instructions(source, name, str(notes / 'notes'), str(notes / 'overview.md'))
        base = Path(tempfile.mkdtemp(prefix='bench-events-', dir=os.environ.get('TMPDIR')))
        (base / 'bin').mkdir()
        (base / 'bin/wsx').write_text(EVENT_WSX)
        (base / 'bin/wsx').chmod(0o755)
        arm_env = {**env, 'PATH': '%s:%s' % (base / 'bin', env['PATH']), 'BENCH_READS': READ_VERBS}
        cut = stamp_ms(next(line for line in transcript(found[0]['path']) if found[0]['prompt'] == prompt_text(line)))
        session, folder = fork_copy(found[0]['before'], cut, base)
        results = {}
        try:
            arms = [threading.Thread(target=replay_arm, args=(model, launch, arm_env, base, session, found, results))
                    for model in (MAIN_MODEL, EVENT_MODEL)]
            for arm in arms:
                arm.start()
            for arm in arms:
                arm.join()
        finally:
            shutil.rmtree(folder, ignore_errors=True)
            shutil.rmtree(base, ignore_errors=True)
        spent += sum(priced(r, rates(r['models'])) for model in (MAIN_MODEL, EVENT_MODEL) for r in results.get(model, []))
        for position, turn in enumerate(found):
            arms = {'original': {'acts': turn['acts'], 'reply': turn['reply']}}
            for model in (MAIN_MODEL, EVENT_MODEL):
                if position < len(results.get(model, [])):
                    arms[model] = results[model][position]
            if len(arms) < 3:
                print('turn %d: %s' % (turn['id'], results.get('errors')), flush=True)
                break
            labels = dict(zip(arms, random.Random(turn['id']).sample('XYZ', 3)))
            verdict, cost = ask_sonnet(EVENT_JUDGE, EVENT_SCHEMA, {
                'project': name, 'events': turn['prompt'][:6000], 'reads': turn['reads'][:8000],
                'later': later(turn, logs[turn['project']]),
                'answers': sorted(({'label': labels[a], 'acts': arms[a]['acts'], 'reply': arms[a]['reply'][:3000]}
                                   for a in arms), key=lambda r: r['label'])}, model=MAIN_MODEL)
            spent += cost
            by_label = {r['label']: r for r in verdict['answers']}
            name_of = {label: arm for arm, label in labels.items()}
            row = {'id': turn['id'], 'project': turn['project'], 'kind': turn['kind'], 'position': position,
                   'review_due': turn['review_due'], 'logged_usd': round(token_cost(turn['usage'], PRICE), 4),
                   'judge_usd': round(cost, 4)}
            for arm in arms:
                judged = by_label.get(labels[arm], {})
                row[arm] = {'acts': arms[arm]['acts'], 'wrong': judged.get('wrong'), 'why': judged.get('why'),
                            'decision': judged.get('decision'), 'gaps': judged.get('gaps'),
                            'same_as': sorted(name_of.get(l, l) for l in judged.get('same_as', []))}
                if arm != 'original':
                    row[arm].update({k: arms[arm][k] for k in ('input', 'cache_read', 'cache_write', 'output',
                                                               'first_context', 'last_context', 'models', 'wall_s')},
                                    usd=priced(arms[arm], rates(arms[arm]['models'])), warm_usd=warm(arms[arm], position),
                                    reply=arms[arm]['reply'])
            out.write(json.dumps(row) + '\n')
            out.flush()
            turns_done += 1
            print(json.dumps({k: (v if not isinstance(v, dict) else {x: v[x] for x in ('acts', 'wrong', 'why')})
                              for k, v in row.items()}), flush=True)
        print('spent so far: %.3f USD, %d turns' % (spent, turns_done), flush=True)
        if spent > options.budget:
            print('over the budget of %.0f USD' % options.budget, flush=True)
            break
    summary = event_summary([json.loads(line) for line in path.read_text().splitlines() if line.strip()])
    print(json.dumps({'summary': summary}, indent=1), flush=True)
    print(json.dumps({'cost_model': event_cost(projects, options.days, summary.get('token_ratio', 1.0))}), flush=True)


def event_summary(rows):
    """The bench table: wrong decisions per arm, the pairs where only one model was wrong, agreement,
    and cost per turn."""
    rows = [r for r in rows if 'id' in r]
    arms = (MAIN_MODEL, EVENT_MODEL)
    out = {'turns': len(rows), 'projects': len({r['project'] for r in rows}),
           'kinds': {k: sum(r['kind'] == k for r in rows) for k in KINDS}}
    for arm in ('original',) + arms:
        out[arm] = {'wrong_minor': sum(r[arm]['wrong'] == 'minor' for r in rows),
                    'wrong_major': sum(r[arm]['wrong'] == 'major' for r in rows)}
    bad = lambda r, arm: r[arm]['wrong'] in ('minor', 'major')
    out['only_sonnet_wrong'] = sum(bad(r, EVENT_MODEL) and not bad(r, MAIN_MODEL) for r in rows)
    out['only_opus_wrong'] = sum(bad(r, MAIN_MODEL) and not bad(r, EVENT_MODEL) for r in rows)
    out['only_sonnet_major'] = sum(r[EVENT_MODEL]['wrong'] == 'major' and r[MAIN_MODEL]['wrong'] != 'major' for r in rows)
    for a, b in (('original', MAIN_MODEL), ('original', EVENT_MODEL), (MAIN_MODEL, EVENT_MODEL)):
        out['same_acts %s=%s' % (a, b)] = sum(r[a]['acts'] == r[b]['acts'] for r in rows)
        out['judged_same %s=%s' % (a, b)] = sum(b in r[a]['same_as'] for r in rows)
    for arm in arms:
        n = len(rows) or 1
        out[arm].update({k: round(sum(r[arm][k] for r in rows) / n, 4 if 'usd' in k else 0)
                         for k in ('usd', 'warm_usd', 'input', 'cache_read', 'cache_write', 'output', 'wall_s')})
    out['logged_usd'] = round(sum(r['logged_usd'] for r in rows) / (len(rows) or 1), 4)
    # ^ Sonnet's tokens over Opus's, both at Sonnet's rates, warm: the cost model's ratio.
    at = lambda arm: sum(warm({**r[arm], 'models': [EVENT_MODEL]}, r['position']) for r in rows)
    out['token_ratio'] = round(at(EVENT_MODEL) / at(MAIN_MODEL), 3) if rows and at(MAIN_MODEL) else 1.0
    out['spent_usd'] = round(sum(r[a]['usd'] for r in rows for a in arms) + sum(r['judge_usd'] for r in rows), 2)
    return out


def run_probe(options):
    """One real event turn across a model change in one process: Opus, set_model sonnet, the next
    event turn, then the owner's last message asked on Sonnet and again on Opus. It shows whether
    the conversation keeps its context and what each switch writes to the cache."""
    projects = manager_turns(options.days)
    env = {k: v for k, v in os.environ.items() if not k.startswith('WSX_')}
    for burst in event_bursts(projects, random.Random(463)):
        if len(burst) < 2 or burst[0]['project'] != 'wsx-8cbe3f08':
            continue
        found = [o for o in map(original_turn, burst[:2]) if o]
        if len(found) < 2 or found[0]['path'] != found[1]['path']:
            continue
        logged = [json.loads(line) for line in (STATE / burst[0]['project'] / 'manager/log.jsonl').read_text().splitlines()
                  if line.strip()]
        owner = [e['text'] for e in logged if e['kind'] == 'message' and e.get('from') == 'owner' and e['unix_ms'] < burst[0]['ms']]
        base = Path(tempfile.mkdtemp(prefix='bench-probe-', dir=os.environ.get('TMPDIR')))
        (base / 'bin').mkdir()
        (base / 'bin/wsx').write_text(EVENT_WSX)
        (base / 'bin/wsx').chmod(0o755)
        arm_env = {**env, 'PATH': '%s:%s' % (base / 'bin', env['PATH']), 'BENCH_READS': READ_VERBS,
                   'BENCH_CALLS': str(base / 'calls.jsonl')}
        source = (ROOT / 'crates/wsx/src/task/instructions.rs').read_text()
        notes = STATE / burst[0]['project'] / 'manager'
        launch = instructions(source, 'wsx', str(notes / 'notes'), str(notes / 'overview.md'))
        cut = stamp_ms(next(line for line in transcript(found[0]['path']) if found[0]['prompt'] == prompt_text(line)))
        session, folder = fork_copy(found[0]['before'], cut, base)
        agent = Agent(launch, arm_env, base, MAIN_MODEL, real=True, resume=session, bash=True)
        steps = []
        try:
            for model, text in ((MAIN_MODEL, found[0]['prompt'] + REPLAY_NOTE % found[0]['reads']),
                                (EVENT_MODEL, found[1]['prompt'] + REPLAY_NOTE % found[1]['reads']),
                                (EVENT_MODEL, CONTEXT_ASK), (MAIN_MODEL, CONTEXT_ASK)):
                if agent.model != model:
                    agent.set_model(model)
                row = agent.turn(text)
                steps.append({'asked': model, **{k: row[k] for k in ('models', 'input', 'cache_read', 'cache_write',
                                                                     'output', 'first_context', 'last_context')},
                              'usd': priced(row, rates(row['models'])), 'reply': row['reply'][:300]})
                print(json.dumps(steps[-1]), flush=True)
        finally:
            agent.stop()
            shutil.rmtree(folder, ignore_errors=True)
            shutil.rmtree(base, ignore_errors=True)
        words = set(re.findall(r'\w+', (owner[-1] if owner else '').lower())[:8])
        print(json.dumps({'probe': burst[0]['id'], 'owner_last': (owner[-1] if owner else '')[:120],
                          'recalled': [len(words & set(re.findall(r'\w+', s['reply'].lower()))) for s in steps[2:]],
                          'spent_usd': round(sum(s['usd'] for s in steps), 3)}), flush=True)
        return
    sys.exit('no wsx burst of two turns with a transcript')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mode', required=True, choices=['before', 'after', 'after-nof'])
    parser.add_argument('--cases', default='synthetic', choices=['synthetic', 'logged', 'discussion', 'stale', 'idle', 'events'])
    parser.add_argument('--returns', type=int, default=6, help='idle: how many idle returns')
    parser.add_argument('--samples', type=int, default=45, help='events: how many turns to replay (0: the cost model only)')
    parser.add_argument('--days', type=int, default=14, help='events: the log window')
    parser.add_argument('--probe', action='store_true', help='events: one turn across a model change, nothing else')
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
        if options.cases == 'events':
            return run_probe(options) if options.probe else run_events(options, out)
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
