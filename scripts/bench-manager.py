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
`events` (docs/design/manager-event-model.md) prints what `eventModel: sonnet` would cost on the logged
turns, cache switches counted, then replays real event-only turns on Opus and Sonnet from a fork of the
manager's transcript (`--samples`, `--budget`; `--probe`: one turn across a model change) for a blind
Opus judge. `switches` (docs/design/manager-context-split.md): one real call with the skill listing and one without, the
first call's tokens each; `switches --replay N` (paid) replays N real owner and event turns in fresh
conversations with and without the CLAUDE.md and auto memory switch, judged blind. `rotation` models the manager's weekly spend over rotation sizes and idle rules; no model call.
`rotation --replay N` (paid) judges N real turns where a manager at ROTATE_TO would rotate in the middle of
work: the real turn against the same turn in a fresh conversation that opens as a rotation's does.
Bash runs with bypassPermissions here, so no auto-mode classifier call adds to the measured
cost; the real manager runs in auto mode.
"""
import argparse
import importlib.util
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

_spec = importlib.util.spec_from_file_location('usage_report', Path(__file__).resolve().parent / 'usage-report.py')
usage = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(usage)  # ^ the repo's home chat lookup: home.json, then tasks/chats/<name>/ (docs/tasks.md "Chats")
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


# ^ The manager's lend_line(true) and reach_line() of wsx-core task/policy.rs (docs/design/chat-capability.md).
LEND = '`new --skills` lends a worker the ones its task needs. '
REACH = ("`-p <project>` runs new, show, list, add, steer, stop, hold, land, close in another project, not its deploys, "
         "releases or pushes. Consult its manager ('@<project>') with `wsx task say -p <project> --from-chat`.")


def skills_sentence(source, folder, names):
    """The `Skills::Folder` sentence of instructions() in task/instructions.rs for a manager that
    reads SKILL.md by path (docs/design/manager-context-split.md option 1)."""
    literal = re.search(r'Skills::Folder\(folder, names\) => format!\(\s*"((?:[^"\\]|\\.)*)"', source, re.S).group(1)
    return unescape(literal).replace('{lend}', LEND).replace('{}', str(folder), 1).replace('{}', ', '.join(names), 1)


def instructions(source, project, notes, page, skills=''):
    """The manager's instructions as instructions() in task/instructions.rs formats them."""
    body = source.split('fn instructions(', 1)[1]
    literal = re.search(r'format!\(\s*"(You are the manager(?:[^"\\]|\\.)*)"', body, re.S).group(1)
    # ^ The default workflow (no review) and the shared presentation rule.
    values = {'project': project, 'notes': notes, 'overview': page, 'skills': skills, 'review': '', 'reach': REACH,
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
    def __init__(self, launch, env, cwd, model, real=False, resume=None, bash=False, extra=()):
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
                                      '--model', model, '--append-system-prompt', launch, *tools, *extra],
                                     cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.model, self.cost_base, self.requests, self.init = model, 0.0, 0, {}

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
        tools, context, first, first_write, models, said = {}, None, None, 0, set(), []
        try:
            for line in self.proc.stdout:
                value = json.loads(line)
                if value.get('type') == 'system' and value.get('subtype') == 'init':
                    self.init = value
                if value.get('type') == 'assistant' and not value.get('parent_tool_use_id'):
                    message = value['message']
                    models.add(message.get('model'))
                    if message.get('usage'):
                        context = context_of(message['usage'])
                        if not first:
                            first, first_write = context, message['usage'].get('cache_creation_input_tokens', 0)
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
                            'first_context': first, 'first_write': first_write, 'last_context': context, 'cost_usd': round(cost, 5), 'tools': tools,
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
    entries = [json.loads(line) for line in (home_chat('wsx-8cbe3f08') / 'log.jsonl').read_text().splitlines() if line.strip()]
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
    launch = instructions(source, 'wsx', str(home_chat('wsx-8cbe3f08') / 'notes'), str(STATE / 'wsx-8cbe3f08' / 'overview.md'))
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


def home_chat(project):
    """The home chat's folder of the store `project`: its log, notes and skills plugin."""
    return Path(usage.home_folder(str(STATE / project)))
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
# ^ `rotation`: a read the conversation made before the turn answers with its output then.
served = json.load(open(os.environ['BENCH_SERVED'])) if os.environ.get('BENCH_SERVED') else {}
number = next((a.lstrip('#') for a in sys.argv[3:] if a.lstrip('#').isdigit()), '')
key = ('%s %s' % (verb, number)).strip()
if key in served and '--help' not in sys.argv:
    print(served[key])
elif verb in os.environ['BENCH_READS'].split() or '--help' in sys.argv:
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
    for repo in sorted(p.name for p in STATE.iterdir() if p.name != 'chats' and (home_chat(p.name) / 'log.jsonl').exists()):
        entries = jsonl(home_chat(repo) / 'log.jsonl')
        messages = {e['id']: e for e in entries if e['kind'] == 'message' and 'id' in e}
        newest, session, turns, fresh = {}, 0, [], 'start'
        for e in entries:
            if e['kind'] == 'rotated':
                session += 1
                fresh = e.get('reason') or 'other'
            if e['kind'] == 'message' and e.get('from') == 'events':
                for line in e['text'].splitlines():
                    head = re.match(r'#(\d+) (\w+)', line)
                    if head:
                        newest[int(head.group(1))] = (head.group(2), e['unix_ms'])
            if e['kind'] != 'reply' or not e.get('usage') or e['unix_ms'] < since:
                continue
            ids = e.get('to') or []
            events = [messages[i] for i in ids if messages.get(i, {}).get('from') == 'events']
            turns.append({'project': repo, 'id': e['id'], 'ms': e['unix_ms'], 'session': session,
                          'usage': e['usage'], 'ids': ids, 'events_only': bool(ids) and len(events) == len(ids),
                          'kind': event_kind('\n'.join(m['text'] for m in events)), 'reply': e.get('text', ''),
                          'fresh': fresh,
                          'review_due': any(word == 'ready' and e['unix_ms'] - ms < DAY_MS
                                            for word, ms in newest.values())})
            fresh = None
        projects[repo] = turns
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
    made, what it acted, its reply). The prompt names its first message under its heading."""
    probe = '(message %d):' % turn['ids'][0]
    paths = [p for p in holding(probe, CLAUDE_PROJECTS) if 'subagents' not in p.parts]
    # ^ A later conversation's handover can quote the prompt: the transcript with the reply first.
    head = max(re.findall(r'[^"\\\n]{1,60}', turn['reply']) or [''], key=len)
    paths.sort(key=lambda p: head not in p.read_text())
    for path in paths:
        lines = transcript(path)
        # ^ A later handover quotes a message on one line: the prompt has it under its heading.
        start = next((k for k, line in enumerate(lines) if probe + '\n' in prompt_text(line)), None)
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


def recorded_acts(calls):
    """The acting `wsx task` commands the recorder wrote to `calls`, sorted."""
    return sorted(acts(' ; '.join(' '.join(['wsx'] + json.loads(line)['argv'])
                                  for line in calls.read_text().splitlines() if line.strip())))


def replay_arm(model, launch, env, base, session, burst, results):
    """One arm of a burst: the forked conversation on `model`, its turns in order."""
    calls = base / ('calls-%s.jsonl' % model)
    agent = Agent(launch, {**env, 'BENCH_CALLS': str(calls)}, base, model, real=True, resume=session, bash=True)
    try:
        for turn in burst:
            calls.write_text('')
            row = agent.turn(turn['prompt'] + REPLAY_NOTE % turn['reads'])
            row['acts'] = recorded_acts(calls)
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


def jsonl(path):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]


def replay_start(first, base, source=None):
    """What a replay of `first` (a turn with its transcript) starts from: the manager's instructions
    for its project (from `source`, else the working copy), an environment with the `wsx` recorder
    on PATH, and a fork of the transcript cut before the turn's prompt."""
    notes = home_chat(first['project'])
    launch = instructions(source or (ROOT / 'crates/wsx/src/task/instructions.rs').read_text(),
                          first['project'].rsplit('-', 1)[0], str(notes / 'notes'), str(STATE / first['project'] / 'overview.md'))
    (base / 'bin').mkdir()
    (base / 'bin/wsx').write_text(EVENT_WSX)
    (base / 'bin/wsx').chmod(0o755)
    env = {k: v for k, v in os.environ.items() if not k.startswith('WSX_')}
    env.update(PATH='%s:%s' % (base / 'bin', env['PATH']), BENCH_READS=READ_VERBS)
    cut = stamp_ms(next(line for line in transcript(first['path']) if first['prompt'] == prompt_text(line)))
    session, folder = fork_copy(first['before'], cut, base)
    return launch, env, session, folder


def run_events(options, out):
    """`events`: the cost model on the logged turns, then real event-only bursts replayed on Opus and
    Sonnet from a fork of the manager's own transcript, judged blind by Opus (docs/design/manager-event-model.md)."""
    projects = manager_turns(options.days)
    print(json.dumps({'cost_model': event_cost(projects, options.days)}), flush=True)
    path = Path(out.name)
    rows = jsonl(path)
    done = {r['id'] for r in rows if 'id' in r}
    spent, turns_done = 0.0, len(rows)
    logs = {name: jsonl(home_chat(name) / 'log.jsonl') for name in projects}
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
        base = Path(tempfile.mkdtemp(prefix='bench-events-', dir=os.environ.get('TMPDIR')))
        launch, arm_env, session, folder = replay_start(found[0], base)
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
    summary = event_summary(jsonl(path))
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
    for burst in event_bursts(projects, random.Random(463)):
        if len(burst) < 2 or burst[0]['project'] != 'wsx-8cbe3f08':
            continue
        found = [o for o in map(original_turn, burst[:2]) if o]
        if len(found) < 2 or found[0]['path'] != found[1]['path']:
            continue
        logged = jsonl(home_chat(burst[0]['project']) / 'log.jsonl')
        owner = [e['text'] for e in logged if e['kind'] == 'message' and e.get('from') == 'owner' and e['unix_ms'] < burst[0]['ms']]
        base = Path(tempfile.mkdtemp(prefix='bench-probe-', dir=os.environ.get('TMPDIR')))
        launch, arm_env, session, folder = replay_start({**burst[0], **found[0]}, base)
        arm_env['BENCH_CALLS'] = str(base / 'calls.jsonl')
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



IDLE_MIN = 60_000


def rotation_cost(projects, days, rotate, idle_min=IDLE_MIN, ping=False):
    """The manager's spend per 7 days, had it rotated at `rotate` tokens and, after an hour idle
    (Claude's cache lifetime), at over `idle_min` tokens (None: never; a cold resume writes it all).
    `ping` keeps an idle conversation warm with a ping each 55 min instead. Each logged turn splits
    into reading its conversation (calls x context, or one write when cold) and its own work, the
    rest; a fresh start costs the logged fresh turns' median over a kept turn's own work and leaves
    their median context. Rotations for other reasons (new instructions, a move between agents) stay
    where they were. Pings are left out (1.09 USD over 14 days)."""
    r, w = PRICE['cache_read'], PRICE['cache_write']
    rows = []
    for turns in projects.values():
        before = None
        for t in turns:
            u = t['usage']
            gap = t['ms'] - before['ms'] if before else None
            start = 0 if t['fresh'] or not before else before['usage'].get('context_tokens', 0)
            calls = max(1, u.get('calls', 1))
            cold = gap is None or gap >= CACHE_MS
            prefix = (start * w + (calls - 1) * start * r if cold else calls * start * r) / 1e6
            rows.append((t, gap, calls, u['cost_micro_usd'] / 1e6 - prefix, u.get('context_tokens', 0) - start))
            before = t
    kept = sorted(own for t, _, _, own, _ in rows if not t['fresh'])
    fresh_cost = sorted(u_cost for u_cost in (t['usage']['cost_micro_usd'] / 1e6 for t, *_ in rows if t['fresh']))
    fresh_ctx = sorted(t['usage'].get('context_tokens', 0) for t, *_ in rows if t['fresh'])
    growth = sorted(g for t, _, _, _, g in rows if not t['fresh'])
    base = fresh_cost[len(fresh_cost) // 2] - kept[len(kept) // 2]
    out = dict(rotate_k=rotate // 1000, idle_min_k=None if idle_min is None else idle_min // 1000, ping=ping,
               usd=0.0, rotations=0, size_rotations=0, idle_rotations=0, busy_rotations=0, cold_resumes=0)
    context, project = 0, None
    for t, gap, calls, own, grow in rows:
        if t['project'] != project:
            context, project = 0, t['project']
        forced = t['fresh'] and not t['fresh'].startswith('idle over')
        idle = gap is not None and gap >= CACHE_MS
        why = ('start' if not context else 'other' if forced else 'size' if context >= rotate else
               'idle' if idle and not ping and idle_min is not None and context > idle_min else None)
        work = own - base if t['fresh'] else own
        if why:
            out['rotations'] += why != 'start'
            out['size_rotations'] += why == 'size'
            out['idle_rotations'] += why == 'idle'
            out['busy_rotations'] += why == 'size' and gap is not None and gap < 600_000
            cost = t['usage']['cost_micro_usd'] / 1e6 if t['fresh'] else base + max(0.0, work)
            context = t['usage'].get('context_tokens', 0) if t['fresh'] else fresh_ctx[len(fresh_ctx) // 2]
        else:
            if idle and ping:
                cost = (gap // 3_300_000) * context * r / 1e6 + calls * context * r / 1e6
            elif idle:
                out['cold_resumes'] += 1
                cost = (context * w + (calls - 1) * context * r) / 1e6
            else:
                cost = calls * context * r / 1e6
            cost += max(0.0, work)
            context += grow if not t['fresh'] else growth[len(growth) // 2]
        out['usd'] += cost
    week = 7 / days
    for k in ('usd', 'rotations', 'size_rotations', 'idle_rotations', 'busy_rotations', 'cold_resumes'):
        out[k] = round(out[k] * week, 1 if k != 'usd' else 2)
    out['fresh_base_usd'] = round(base, 3)
    return out


def run_rotation(options):
    """`rotation`: the manager's modeled spend per week across rotation sizes and idle rules, from the
    logged turns (no model call); the first row is today's settings, against the logged spend."""
    projects = manager_turns(options.days)
    logged = sum(t['usage']['cost_micro_usd'] for turns in projects.values() for t in turns) / 1e6 * 7 / options.days
    print(json.dumps({'logged_usd_per_week': round(logged, 2), 'turns': sum(map(len, projects.values()))}))
    for idle_min, ping in ((IDLE_MIN, False), (0, False), (None, False), (IDLE_MIN, True)):
        for rotate in (400_000, 250_000, 160_000, 120_000, 80_000):
            print(json.dumps(rotation_cost(projects, options.days, rotate, idle_min, ping)), flush=True)


# ^ The quality side of the rotation question (docs/design/manager-event-model.md "Rotation
# replay"): at each busy crossing of ROTATE_TO, arm A is the real turn of the kept conversation
# and arm B the same turn in a fresh conversation that opens as a rotation's does.
ROTATE_TO = 160_000
BUSY_MS = 600_000
HANDOVER_HEAD = ('Handover (this is a new conversation; the last exchanges before it, read the '
                 'ledger for the state):\n')
ROUTINES_HEAD = ("\nRoutine results (each routine's newest with its failures since its last success, "
                 "and its runs since your last reply):\n")
# ^ wsx-core's `clip` keeps chars - 1 and an ellipsis since bf35ba80 (2026-11-05): from
# 2026-11-07 on, every real fresh start's exchanges must match the port byte for byte. The routine
# block cannot: the routine store drops one-shot `checkin` runs and keeps MAX_RUNS per routine.
VALID_FROM_MS = 1793977200000
ROUTINES = Path.home() / '.local/state/wsx/routines/projects'
ROTATION_JUDGE = """You judge turns of a software project manager agent. The manager runs coding tasks
for its owner: it reviews a ready task's result and diff, then lands it (`land`), sends it back
(`add`, `steer`), closes it, starts a task (`new`), asks the owner, answers the owner, or does
nothing. `earlier` is the chat before the turn, `prompt` what the turn got, `reads` what it could read
then, `later` what the log shows afterwards (hindsight, not always caused by the turn). Each of
`answers` is one run of the same turn by agents that knew different amounts of the earlier work:
the `wsx task` commands that act (`acts`) and the reply. For each answer give `decision` (a few
words); `same_as`, the labels of the other answers that decided the same in substance (the same tasks
landed, sent back, closed, started or asked about, and the same verdict on the owner's question;
wording and extra reads do not matter); `wrong`: "none", "minor" (a needless or missing small step,
or a reply that misleads a little) or "major" (lands, closes or approves what should not; leaves a
failure, a worker's question or an owner need unhandled; sends work the wrong way; or claims what is
false), with `why` in at most 25 words; and `lost_facts`: each fact that another answer used for its
decision or reply and this answer lacks, gets wrong or asks the owner for (a promise, an owner
instruction, a review finding, a task's state), a few words each; leave out facts no decision needs.
Do not prefer an answer for its length or style. Answer only through the schema."""
ROTATION_SCHEMA = {'type': 'object', 'required': ['answers'], 'properties': {'answers': {'type': 'array', 'items': {
    'type': 'object', 'required': ['label', 'decision', 'same_as', 'wrong', 'why', 'lost_facts'],
    'properties': {'label': {'type': 'string'}, 'decision': {'type': 'string'},
                   'same_as': {'type': 'array', 'items': {'type': 'string'}},
                   'wrong': {'enum': ['none', 'minor', 'major']}, 'why': {'type': 'string'},
                   'lost_facts': {'type': 'array', 'items': {'type': 'string'}}}}}}}


def clip(text, chars):
    """wsx-core task::view::clip: the first line, at most `chars` characters with the ellipsis."""
    line = (text.splitlines() or [''])[0]
    return line if len(line) <= chars else line[:chars - 1].rstrip() + '…'


def recent_exchanges(log, limit=4):
    """manager.rs `recent_exchanges`: the last `limit` owner messages and replies, one line each."""
    lines = []
    for e in reversed(log):
        said = clip(' '.join(e.get('text', '').split()), 600)
        if e['kind'] == 'message' and e.get('from') == 'owner':
            lines.append('Owner (message %d): %s' % (e['id'], said))
        elif e['kind'] == 'reply':
            who = 'Stand-in %s' % e['provisional'] if e.get('provisional') else 'Posted' if e.get('post') else 'You'
            lines.append('%s (reply %d): %s' % (who, e['id'], said))
        if len(lines) == limit:
            break
    return lines[::-1]


def result_line(run):
    """wsx-core RunRecord::result_line."""
    if run['status'] == 'succeeded':
        return 'routine %s succeeded after %d s' % (run['routine'], run['finished_epoch'] - run['started_epoch'])
    words = {'findings': 'found something', 'spawn_failed': 'could not start', 'cancelled': 'was cancelled',
             'interrupted': 'was interrupted'}.get(run['status'], 'failed')
    code = run.get('exit_code')
    return 'routine %s %s (%s)' % (run['routine'], words, 'no exit code' if code is None else 'exit %d' % code)


def handover_routines(runs, replied):
    """manager.rs `handover_routines`."""
    runs = sorted(runs, key=lambda r: r['finished_epoch'])
    names = []
    for run in reversed(runs):
        if run['routine'] not in names:
            names.append(run['routine'])
    lines = []
    for name in reversed(names):
        mine = [r for r in runs if r['routine'] == name]
        ok = [k for k, r in enumerate(mine) if r['status'] in ('succeeded', 'findings')]
        failed = len(mine) - (ok[-1] + 1 if ok else 0)
        plural = '' if failed == 1 else 's'
        if failed == 0:
            lines.append(result_line(mine[-1]))
        elif ok:
            stamp = datetime.fromtimestamp(mine[ok[-1]]['finished_epoch']).strftime('%Y-%m-%d %H:%M')
            lines.append('%s; %d failure%s since its last success at %s' % (result_line(mine[-1]), failed, plural, stamp))
        else:
            lines.append('%s; %d failure%s, never succeeded' % (result_line(mine[-1]), failed, plural))
        after = [r for r in mine if r['finished_epoch'] > replied]
        if len(after) > 1:
            lines.append('routine %s ran %d times since your last reply, %d of them without success'
                         % (name, len(after), sum(r['status'] not in ('succeeded', 'findings') for r in after)))
    return lines


def routine_runs(cwd):
    """The finished runs of the project whose main checkout is `cwd`, from the routine store."""
    for path in ROUTINES.glob('*.json'):
        stored = json.loads(path.read_text())
        if stored.get('project_path') == cwd:
            return [r for runs in stored.get('runs', {}).values() for r in runs if r.get('finished_epoch')]
    return []


def handover(entries, runs, ms):
    """manager.rs `handover` as it ran at `ms`, from the log and the routine runs up to then:
    (the exchanges part, the rest)."""
    log = [e for e in entries if e['unix_ms'] <= ms]
    replied = next((e['unix_ms'] // 1000 for e in reversed(log) if e['kind'] == 'reply' and e.get('usage')), 0)
    routines = handover_routines([r for r in runs if r['finished_epoch'] <= ms // 1000], replied)
    return (HANDOVER_HEAD + '\n'.join(recent_exchanges(log)) + '\n',
            (ROUTINES_HEAD + '\n'.join(routines) + '\n' if routines else '') + '\n')


def fresh_starts(folder, entries, runs):
    """The real fresh starts of the project's manager in `folder`, as (ms, first message, matched):
    from VALID_FROM_MS on, `matched` says whether `handover` rebuilds its exchanges byte for byte.
    A chat's conversation in the same folder quotes another log: its first exchange is not here."""
    said = {e['id']: e for e in entries if e['kind'] in ('message', 'reply') and 'id' in e}
    found = []
    for path in folder.glob('*.jsonl'):
        first = next((line for line in transcript(path) if prompt_text(line)), None)
        text = prompt_text(first) if first else ''
        ms = stamp_ms(first) if first else 0
        if HANDOVER_HEAD not in text:
            continue
        head = re.match(r'.* \((?:message|reply) (\d+)\): (.*)', text.split(HANDOVER_HEAD, 1)[1].split('\n', 1)[0])
        own = head and said.get(int(head.group(1)))
        if not own or not clip(' '.join(own.get('text', '').split()), 600).startswith(head.group(2)[:40]):
            continue
        found.append((ms, text, handover(entries, runs, ms)[0] + '\n' in text if ms >= VALID_FROM_MS else None))
    return sorted(found)


def busy_crossings(projects):
    """Per conversation, its first turn whose last reply was past ROTATE_TO and under BUSY_MS ago:
    where a manager at ROTATE_TO rotates in the middle of work. Ordered so projects and kinds
    (an owner message or events) take turns."""
    found = []
    for turns in projects.values():
        seen = set()
        for before, t in zip(turns, turns[1:]):
            if (not t['fresh'] and before['session'] == t['session'] and t['session'] not in seen
                    and before['usage'].get('context_tokens', 0) >= ROTATE_TO and t['ms'] - before['ms'] < BUSY_MS):
                seen.add(t['session'])
                found.append({**t, 'context': before['usage']['context_tokens']})
    return interleaved(found, 464)


def interleaved(found, seed):
    """`found` shuffled, then ordered so projects and kinds (an owner message or events) take turns."""
    random.Random(seed).shuffle(found)
    used, ordered = {}, []
    while found:
        kind = not ordered[-1]['events_only'] if ordered else False
        pick = min(found, key=lambda t: (used.get(t['project'], 0), t['events_only'] != kind))
        found.remove(pick)
        used[pick['project']] = used.get(pick['project'], 0) + 1
        ordered.append(pick)
    return ordered


def served_reads(before, ms):
    """The newest output of each `wsx task ledger` and `wsx task show <n>` the conversation read
    before the turn, keyed as the recorder looks them up, with its age."""
    uses, served = {}, {}
    for line in before:
        content = line.get('message', {}).get('content')
        for block in content if isinstance(content, list) else []:
            if block.get('type') == 'tool_use' and block.get('name') == 'Bash':
                uses[block['id']] = block['input'].get('command', '')
            elif block.get('type') == 'tool_result' and block.get('tool_use_id') in uses:
                read = re.search(r'wsx task (ledger|show)\b([^|;&\n]*)', uses[block['tool_use_id']])
                if not read:
                    continue
                number = re.search(r'(?<![\w-])#?(\d+)\b', read.group(2)) if read.group(1) == 'show' else None
                result = block.get('content')
                result = result if isinstance(result, str) else ''.join(
                    b.get('text', '') for b in result or [] if isinstance(b, dict))
                served[('%s %s' % (read.group(1), number.group(1) if number else '')).strip()] = (
                    '(read %d min before this turn)\n%s' % ((ms - (stamp_ms(line) or ms)) // 60000, result[:12000]))
    return served


def fresh_first(case, original, entries):
    """B's first message as `send_turn` builds it for a rotation: the notes preamble, the handover,
    the turn's text. The preamble is the newest matched real fresh start's before the turn, with the
    rules the owner gave after it from the memory records; what it still lacks is counted."""
    cwd = next((line['cwd'] for line in original['before'] if line.get('cwd')), '')
    runs = routine_runs(cwd)
    starts = fresh_starts(original['path'].parent, entries, runs)
    checked = {'starts': sum(s[2] is not None for s in starts), 'matched': sum(bool(s[2]) for s in starts)}
    earlier = [s for s in starts if s[0] < case['ms']]
    if not earlier:
        return None, checked
    start_ms, text, _ = earlier[-1]
    preamble = text[:text.index(HANDOVER_HEAD)]
    records = STATE / case['project'] / 'memory/records.jsonl'
    rules = [r for r in (jsonl(records) if records.exists() else [])
             if r.get('op') == 'add' and r.get('kind') == 'rule' and start_ms < r.get('updated', 0) <= case['ms']]
    title = "The owner's decisions (decisions.md, newest first):\n"
    if rules and title in preamble:
        lines = ''.join('- %s %s\n' % (datetime.fromtimestamp(r.get('decided', r['updated']) / 1000).strftime('%Y-%m-%d'),
                                        r.get('words') or r['text']) for r in reversed(rules))
        preamble = preamble.replace(title, title + lines, 1)
    cut = stamp_ms(next(line for line in transcript(original['path']) if original['prompt'] == prompt_text(line)))
    first = preamble + ''.join(handover(entries, runs, cut)) + original['prompt']
    return first, {**checked, 'preamble_age_min': (case['ms'] - start_ms) // 60000,
                   'rules_added': len(rules), 'first_bytes': len(first.encode())}


def rotation_judge(case, original, entries, arms):
    """One blind Opus call over the arms; returns the verdict per arm and its cost."""
    labels = dict(zip(arms, random.Random(case['id']).sample('XYZ', len(arms))))
    talk = [e for e in entries if e['unix_ms'] < case['ms'] and (e['kind'] == 'message' and e.get('from') == 'owner'
                                                                 or e['kind'] == 'reply' and e.get('text'))][-8:]
    verdict, cost = ask_sonnet(ROTATION_JUDGE, ROTATION_SCHEMA, {
        'project': case['project'].rsplit('-', 1)[0],
        'earlier': ['%s %d: %s' % ('Owner' if e['kind'] == 'message' else 'Manager', e['id'], e['text'][:1500]) for e in talk],
        'prompt': original['prompt'][:6000], 'reads': original['reads'][:8000],
        'later': later({**case, 'prompt': original['prompt']}, entries),
        'answers': sorted(({'label': labels[a], 'acts': arms[a]['acts'], 'reply': arms[a]['reply'][:3000]} for a in arms),
                          key=lambda r: r['label'])}, model=MAIN_MODEL)
    by_label = {r['label']: r for r in verdict['answers']}
    name_of = {label: arm for arm, label in labels.items()}
    return {arm: {**{k: by_label.get(labels[arm], {}).get(k) for k in ('decision', 'wrong', 'why', 'lost_facts')},
                  'same_as': sorted(name_of.get(l, l) for l in by_label.get(labels[arm], {}).get('same_as', []))}
            for arm in arms}, cost


def fails(judged, against):
    """The case's rule (docs/design/manager-event-model.md "Rotation replay"): `fresh` decided
    otherwise than `against` and wrongly, or lost a fact."""
    fresh = judged['fresh']
    return bool(fresh['lost_facts']) or (against not in fresh['same_as'] and fresh['wrong'] != 'none')


def replay_turn(launch, env, base, text, resume=None):
    calls = base / 'calls.jsonl'
    calls.write_text('')
    agent = Agent(launch, {**env, 'BENCH_CALLS': str(calls)}, base, MAIN_MODEL, real=True, resume=resume, bash=True)
    try:
        row = agent.turn(text)
    finally:
        agent.stop()
    row['acts'] = recorded_acts(calls)
    return row


def run_rotation_replay(options, out):
    """`rotation --replay N`: N busy crossings judged, fresh against the real turn; a failing case
    runs the kept conversation again in the same replay (`kept`) and counts only if it fails there too."""
    projects = manager_turns(options.days)
    path = Path(out.name)
    rows = [r for r in jsonl(path) if 'id' in r]
    done = {(r['project'], r['id']) for r in rows}
    spent = sum(r['usd'] for r in rows)
    logs = {name: jsonl(home_chat(name) / 'log.jsonl') for name in projects}
    for case in busy_crossings(projects):
        if len(rows) >= options.replay or spent > options.budget:
            break
        if (case['project'], case['id']) in done:
            continue
        original = original_turn(case)
        if not original:
            print('turn %s %d: no transcript, left out' % (case['project'], case['id']), flush=True)
            continue
        entries = logs[case['project']]
        first, built = fresh_first(case, original, entries)
        print(json.dumps({'case': case['id'], 'project': case['project'], 'context': case['context'], **built}), flush=True)
        if not first or built['matched'] < built['starts']:
            print('turn %d: no fresh start the port rebuilds, left out' % case['id'], flush=True)
            continue
        commit = subprocess.run(['git', 'rev-list', '-1', '--before=@%d' % (case['ms'] // 1000), 'HEAD'], cwd=ROOT,
                                capture_output=True, text=True).stdout.strip()
        base = Path(tempfile.mkdtemp(prefix='bench-rotation-', dir=os.environ.get('TMPDIR')))
        launch, env, session, folder = replay_start({**case, **original}, base, source_at(commit))
        (base / 'served.json').write_text(json.dumps(served_reads(original['before'], case['ms'])))
        env['BENCH_SERVED'] = str(base / 'served.json')
        note = REPLAY_NOTE % original['reads']
        arms = {'original': {'acts': original['acts'], 'reply': original['reply']}}
        usd = 0.0
        try:
            arms['fresh'] = replay_turn(launch, env, base, first + note)
            usd += priced(arms['fresh'])
            judged, cost = rotation_judge(case, original, entries, arms)
            usd += cost
            counted = fails(judged, 'original')
            if counted and spent + usd < options.budget:
                arms['kept'] = replay_turn(launch, env, base, original['prompt'] + note, resume=session)
                usd += priced(arms['kept'])
                judged, cost = rotation_judge(case, original, entries, arms)
                usd += cost
                counted = fails(judged, 'kept')
        finally:
            shutil.rmtree(folder, ignore_errors=True)
            shutil.rmtree(base, ignore_errors=True)
        spent += usd
        row = {'id': case['id'], 'project': case['project'], 'context': case['context'], 'events_only': case['events_only'],
               'commit': commit[:12], **built, 'usd': round(usd, 4), 'counted': counted,
               'wrong_land': counted and any(a.split()[0] in ('land', 'close', 'approve') and a not in arms['original']['acts']
                                             for a in arms['fresh']['acts']),
               **{arm: {**judged[arm], 'acts': arms[arm]['acts'], 'reply': arms[arm]['reply'],
                        **({'usd': priced(arms[arm])} if arm != 'original' else {})} for arm in arms}}
        out.write(json.dumps(row) + '\n')
        out.flush()
        rows.append(row)
        print(json.dumps({k: (v if not isinstance(v, dict) else {x: v[x] for x in ('acts', 'wrong', 'why', 'lost_facts')})
                          for k, v in row.items()}, ensure_ascii=False), flush=True)
        print('spent so far: %.3f USD, %d cases' % (spent, len(rows)), flush=True)
    counted = [r for r in rows if r['counted']]
    print(json.dumps({'summary': {'cases': len(rows), 'counted': len(counted), 'allowed': len(rows) // 12,
                                  'wrong_land': sum(r['wrong_land'] for r in rows), 'spent_usd': round(spent, 2),
                                  'switch': len(rows) >= 12 and len(counted) <= len(rows) // 12
                                  and not any(r['wrong_land'] for r in rows)}}), flush=True)


SWITCH_SETTINGS = {
    # ^ The same keys as the manager's `--settings` in claude.rs `claude_args`.
    'memory': {'claudeMdExcludes': ['**/CLAUDE.md', '**/CLAUDE.local.md', '**/AGENTS.md'], 'autoMemoryEnabled': False}}
# ^ The tools no wsx agent calls, as claude.rs NEVER_TOOLS: without it the flag grows the Workflow tool by 17 KB.
NEVER = ['--disallowedTools', 'Workflow,ScheduleWakeup,ReportFindings']
PROOF_PROMPT = ("Which owner skill is named `simplify`? Use whatever way your instructions give to open it "
                "and tell me its description line, in one sentence.")


def owner_skills():
    """The owner's skills folder and the names in it: where the manager's plugin links."""
    folder = Path(os.environ.get('CLAUDE_CONFIG_DIR') or Path.home() / '.claude') / 'skills'
    return folder, sorted(p.name for p in folder.iterdir() if (p / 'SKILL.md').is_file())


def switch_arm(arm, base, source, project='bench'):
    """(launch text, extra argv) of a manager arm: `today` has the skill listing and the Skill tool
    (a plugin of the owner's skills), `skills` the flag and the names line, `memory` the flag, the
    names line and the settings that leave out CLAUDE.md files and the auto memory."""
    folder, names = owner_skills()
    notes = base / 'notes'
    notes.mkdir(exist_ok=True)
    page = base / 'overview.md'
    page.write_text('# bench\n\n## Goal\n\nA small greeting CLI.\n')
    if arm == 'today':
        plugin = base / 'skills-plugin'
        (plugin / '.claude-plugin').mkdir(parents=True)
        (plugin / '.claude-plugin/plugin.json').write_text(json.dumps({'name': 'owner', 'description': 'bench'}))
        (plugin / 'skills').symlink_to(folder)
        text = instructions(source, project, str(notes), str(page),
                            "The owner's skills are yours with the Skill tool as `owner:<name>`; `new --skills` lends a worker the ones its task needs. ")
        return text, NEVER + ['--plugin-dir', str(plugin)]
    text = instructions(source, project, str(notes), str(page), skills_sentence(source, folder, names))
    return text, NEVER + ['--disable-slash-commands'] + (['--settings', json.dumps(SWITCH_SETTINGS['memory'])] if arm == 'memory' else [])


def run_switch_proof(options):
    """`switches`: one real manager call per arm (today, option 1, option 2): the first call's tokens, the
    skills the session offers, and how each arm opens a skill. Run with TMPDIR under the home folder: a
    manager's CLAUDE.md files load because its folder lies under it (`~/.claude/CLAUDE.md` is an ancestor's)."""
    source = (ROOT / 'crates/wsx/src/task/instructions.rs').read_text()
    rows = []
    for arm in ('today', 'skills', 'memory'):
        base = Path(tempfile.mkdtemp(prefix='bench-switch-', dir=os.environ.get('TMPDIR')))
        try:
            text, extra = switch_arm(arm, base, source)
            agent = Agent(text, dict(os.environ), base, MAIN_MODEL, extra=extra)
            try:
                row = agent.turn(PROOF_PROMPT)
            finally:
                agent.stop()
            init = agent.init
            row.update(arm=arm, skills_in_init=len(init.get('skills', [])), skill_tool='Skill' in init.get('tools', []),
                       sample_names=[n for n in init.get('skills', []) if 'simplify' in n][:3])
            rows.append(row)
            print(json.dumps({k: row[k] for k in ('arm', 'first_context', 'first_write', 'cache_read', 'cache_write', 'skills_in_init',
                                                  'skill_tool', 'sample_names', 'tools', 'cost_usd', 'reply')}), flush=True)
        finally:
            shutil.rmtree(base, ignore_errors=True)
    today, skills, memory = rows
    print(json.dumps({'first_context': {r['arm']: r['first_context'] for r in rows},
                      'skills_saved': today['first_context'] - skills['first_context'],
                      'memory_saved': skills['first_context'] - memory['first_context']}), flush=True)


def switch_cases(projects):
    """The turns of a running conversation, owner messages and events alternating."""
    found = []
    for turns in projects.values():
        for before, t in zip(turns, turns[1:]):
            if not t['fresh'] and before['session'] == t['session']:
                found.append({**t, 'context': before['usage'].get('context_tokens', 0)})
    return interleaved(found, 487)


def bad(judged):
    return bool(judged['lost_facts']) or judged['wrong'] != 'none'


def run_switch_replay(options, out):
    """`switches --replay N` (paid): N real owner and event turns, each as the first message of a fresh
    conversation with today's switches (`skills`) and with the memory switch, judged blind by Opus
    with the real turn as the third answer. The memory switch ships when it is bad in no more cases than `skills`."""
    projects = manager_turns(options.days)
    path = Path(out.name)
    rows = [r for r in jsonl(path) if 'id' in r]
    done = {(r['project'], r['id']) for r in rows}
    spent = sum(r['usd'] for r in rows)
    logs = {name: jsonl(home_chat(name) / 'log.jsonl') for name in projects}
    source = (ROOT / 'crates/wsx/src/task/instructions.rs').read_text()
    for case in switch_cases(projects):
        if len(rows) >= options.replay or spent > options.budget:
            break
        if (case['project'], case['id']) in done:
            continue
        original = original_turn(case)
        if not original:
            continue
        entries = logs[case['project']]
        first, built = fresh_first(case, original, entries)
        if not first or built['matched'] < built['starts']:
            print('turn %d: no fresh start the port rebuilds, left out' % case['id'], flush=True)
            continue
        base = Path(tempfile.mkdtemp(prefix='bench-switch-', dir=os.environ.get('TMPDIR')))
        launch, env, session, folder = replay_start({**case, **original}, base, source)
        (base / 'served.json').write_text(json.dumps(served_reads(original['before'], case['ms'])))
        env['BENCH_SERVED'] = str(base / 'served.json')
        # ^ The manager runs in the project's checkout, which holds its CLAUDE.md files: the arms need them.
        project = Path(next((line['cwd'] for line in original['before'] if line.get('cwd')), ''))
        for name in ('CLAUDE.md', 'AGENTS.md'):
            if (project / name).is_file():
                shutil.copy(project / name, base / name)
        note = REPLAY_NOTE % original['reads']
        arms = {'original': {'acts': original['acts'], 'reply': original['reply']}}
        usd = 0.0
        try:
            for arm in ('skills', 'memory'):
                (base / ('arm-' + arm)).mkdir()
                text, extra = switch_arm(arm, base / ('arm-' + arm), source, case['project'].rsplit('-', 1)[0])
                calls = base / 'calls.jsonl'
                calls.write_text('')
                agent = Agent(text, {**env, 'BENCH_CALLS': str(calls)}, base, MAIN_MODEL, real=True, bash=True, extra=extra)
                try:
                    row = agent.turn(first + note)
                finally:
                    agent.stop()
                row['acts'] = recorded_acts(calls)
                arms[arm] = row
                usd += priced(row)
            judged, cost = rotation_judge(case, original, entries, arms)
            usd += cost
        finally:
            shutil.rmtree(folder, ignore_errors=True)
            shutil.rmtree(base, ignore_errors=True)
        spent += usd
        row = {'id': case['id'], 'project': case['project'], 'events_only': case['events_only'], **built, 'usd': round(usd, 4),
               **{arm: {**judged[arm], 'acts': arms[arm]['acts'], 'reply': arms[arm]['reply'],
                        **({'first_context': arms[arm]['first_context'], 'usd': priced(arms[arm])} if arm != 'original' else {})}
                  for arm in arms}}
        out.write(json.dumps(row) + '\n')
        out.flush()
        rows.append(row)
        print(json.dumps({k: (v if not isinstance(v, dict) else {x: v.get(x) for x in ('acts', 'wrong', 'why', 'lost_facts', 'first_context')})
                          for k, v in row.items()}, ensure_ascii=False), flush=True)
        print('spent so far: %.3f USD, %d cases' % (spent, len(rows)), flush=True)
    worse = [r for r in rows if bad(r['memory']) and not bad(r['skills'])]
    better = [r for r in rows if bad(r['skills']) and not bad(r['memory'])]
    land = lambda r: any(a.split()[0] in ('land', 'close', 'approve') and a not in r['original']['acts'] for a in r['memory']['acts'])
    print(json.dumps({'summary': {
        'cases': len(rows), 'bad_skills': sum(bad(r['skills']) for r in rows), 'bad_memory': sum(bad(r['memory']) for r in rows),
        'memory_worse': len(worse), 'memory_better': len(better), 'wrong_land': sum(map(land, rows)),
        'first_context_skills': sorted(r['skills']['first_context'] for r in rows)[len(rows) // 2] if rows else None,
        'first_context_memory': sorted(r['memory']['first_context'] for r in rows)[len(rows) // 2] if rows else None,
        'spent_usd': round(spent, 2),
        'ship': len(rows) >= 10 and len(worse) <= len(better) and not any(map(land, rows))}}), flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mode', required=True, choices=['before', 'after', 'after-nof'])
    parser.add_argument('--cases', default='synthetic', choices=['synthetic', 'logged', 'discussion', 'stale', 'idle', 'events', 'rotation', 'switches'])
    parser.add_argument('--returns', type=int, default=6, help='idle: how many idle returns')
    parser.add_argument('--samples', type=int, default=45, help='events: how many turns to replay (0: the cost model only)')
    parser.add_argument('--days', type=int, default=14, help='events: the log window')
    parser.add_argument('--probe', action='store_true', help='events: one turn across a model change, nothing else')
    parser.add_argument('--replay', type=int, default=0, help='rotation: how many busy rotations to replay (paid)')
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
        if options.cases == 'switches':
            return run_switch_replay(options, out) if options.replay else run_switch_proof(options)
        if options.cases == 'rotation':
            return run_rotation_replay(options, out) if options.replay else run_rotation(options)
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
