#!/usr/bin/env python3
"""Model-free stand-in for `claude -p --input-format stream-json` used by test-task.py.

Each user message is a small script of words:
  SLOW         take about 2 s and accept steers during the turn
  NEEDS <rule> deny once unless <rule> is in --allowedTools, and report blocked
  DENIED <rule> deny <rule> but carry on and report done
  WORK <file>  write <file> and commit it
  NOOP         change nothing
  HUGE         first print one 5 MiB stdout line
  HV           return one Human Verify item
  LINE <file> <n>  replace line n of <file> with the message and commit
  EDIT <file>  try the Edit tool on <file>: denied unless Edit is allowed and not disallowed
Launched with the brief schema it writes a brief instead and builds nothing:
  REWRITE <words>  the rewritten prompt is the rest of the line
  REVISE <words>   (a reply) the rewritten prompt becomes the rest of the line
  NOBRIEF          end the turn without a brief
The host's landing message ("rebased onto <tip>") makes it rebase and resolve every
conflict with the line "resolved".
Every received message and the launch arguments go to $FAKE_CLAUDE_TRACE.

Started with the manager's instructions it is a scripted manager instead: per turn
  RUN <command>  run that shell command (the wsx task commands it names) and quote its output
  STATUS         answer from `wsx task ledger`
  SLOW           take about 3 s
  BIG            report a 200k-token context, so the host rotates the conversation
"""
import json
import os
import select
import signal
import subprocess
import sys
import time

TRACE = os.environ.get('FAKE_CLAUDE_TRACE') or os.path.join(os.getcwd(), '..', 'fake-claude.trace')


def trace(record):
    with open(TRACE, 'a') as out:
        out.write(json.dumps(record) + '\n')


def emit(value):
    sys.stdout.write(json.dumps(value) + '\n')
    sys.stdout.flush()


def parse_args(argv):
    options, allowed, disallowed, index = {}, [], [], 0
    while index < len(argv):
        arg = argv[index]
        if arg in ('--allowedTools', '--disallowedTools'):
            into = allowed if arg == '--allowedTools' else disallowed
            index += 1
            while index < len(argv) and not argv[index].startswith('--'):
                into.append(argv[index])
                index += 1
            continue
        if arg.startswith('--') and index + 1 < len(argv) and not argv[index + 1].startswith('--'):
            options[arg] = argv[index + 1]
            index += 2
            continue
        options[arg] = True
        index += 1
    return options, allowed, disallowed


def own_repo():
    """A fixture commit must land in this working copy, never in a repository above it."""
    top = subprocess.run(['git', 'rev-parse', '--show-toplevel'], capture_output=True, text=True).stdout.strip()
    assert os.path.realpath(top) == os.path.realpath(os.getcwd()), ('fixture git escaped', os.getcwd(), top)


def rest_of_line(text, word):
    line = next(line for line in text.splitlines() if word in line.split())
    return line.split(word, 1)[1].strip()


def try_edit(words, allowed, disallowed, denials):
    """EDIT <file>: the agent's permission check, as Claude applies the two lists."""
    if 'EDIT' not in words:
        return
    name = words[words.index('EDIT') + 1]
    emit({'type': 'assistant', 'message': {'content': [
        {'type': 'tool_use', 'name': 'Edit', 'input': {'file_path': name}}]}})
    if 'Edit' in disallowed or 'Edit' not in allowed:
        denials.append({'tool_name': 'Edit', 'tool_use_id': 'e', 'tool_input': {'file_path': name}})
        trace({'edit_denied': name})
        return
    with open(name, 'a') as out:
        out.write('edited\n')
    trace({'edited': name})


def brief_turn(text, session, allowed, disallowed, current):
    """One brief turn: a brief whose rewritten prompt comes from REWRITE or REVISE."""
    words = text.split()
    denials = []
    try_edit(words, allowed, disallowed, denials)
    reply = text.startswith('The owner replies to the brief')
    if 'REVISE' in words:
        current = rest_of_line(text, 'REVISE')
    elif 'REWRITE' in words:
        current = rest_of_line(text, 'REWRITE')
    if 'NOBRIEF' in words:
        emit({'type': 'result', 'subtype': 'error_max_turns', 'is_error': True, 'result': '',
              'session_id': session, 'permission_denials': denials})
        return current
    owner = text.split('\n\n')[1] if reply else ''
    emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': 'brief',
          'session_id': session, 'permission_denials': denials,
          'usage': {'input_tokens': 900, 'output_tokens': 300}, 'total_cost_usd': 0.02,
          'structured_output': {
              'additions': [{'what': 'a regression test', 'why': 'it keeps the fix'}],
              'raw_material': [{'source': 'README', 'why': 'the fixture'}],
              'methods': ['git log -p'], 'independent_check': ['the check command exits 0'],
              'concepts': [{'term': 'Working copy', 'explanation': 'A separate checkout. Nothing else changes.'}],
              'rewritten_prompt': current or 'NOOP', 'open_questions': [],
              'answer': ('answered: ' + owner) if reply else ''}})
    return current


def manager_turn(text, project):
    replies = []
    if 'SLOW' in text.split():
        time.sleep(3)
    for line in text.splitlines():
        if line.startswith('RUN '):
            done = subprocess.run(line[4:], shell=True, capture_output=True, text=True)
            replies.append('ran: %s -> %d %s' % (line[4:], done.returncode, (done.stdout + done.stderr).strip()[:300]))
    if 'STATUS' in text.split():
        ledger = json.loads(subprocess.run(['wsx', 'task', 'ledger', '-p', project], capture_output=True,
                                           text=True, check=True).stdout)
        rows = ['#%d %s' % (row['id'], row['state']) for row in ledger['open']]
        replies.append('open: ' + (', '.join(rows) if rows else 'none'))
    if text.startswith('Events'):
        replies.append('noted events')
    return '\n'.join(replies) or 'ok'


def main():
    options, allowed, disallowed = parse_args(sys.argv[1:])
    instructions = options.get('--append-system-prompt', '')
    project = instructions.split('wsx project ')[1].split('.')[0] if 'You are the manager' in instructions else None
    session = options.get('--resume') or options.get('--session-id')
    briefing = 'rewritten_prompt' in str(options.get('--json-schema', ''))
    rewritten = None
    trace({'launch': True, 'pid': os.getpid(), 'session': session, 'resume': '--resume' in options,
           'allowed': allowed, 'disallowed': disallowed, 'briefing': briefing, 'instructions': instructions,
           'schema': '--json-schema' in options, 'model': options.get('--model'),
           'wsx_env': sorted(name for name in os.environ if name.startswith('WSX_')),
           'manager': project is not None, 'actor': os.environ.get('WSX_TASK_ACTOR'),
           'cwd': os.getcwd()})
    signal.signal(signal.SIGINT, lambda *_: (trace({'interrupted': True}), sys.exit(130)))
    emit({'type': 'system', 'subtype': 'init', 'session_id': session})
    stdin = sys.stdin.buffer
    while True:
        line = stdin.readline()
        if not line:
            return
        text = json.loads(line)['message']['content']
        trace({'message': text, 'session': session, 'manager': project is not None})
        if project is not None:
            answer = manager_turn(text, project)
            context = 200000 if 'BIG' in text.split() else 5000
            emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': answer,
                  'session_id': session, 'permission_denials': [],
                  'usage': {'input_tokens': context, 'output_tokens': 10}, 'total_cost_usd': 0.001})
            continue
        if briefing:
            rewritten = brief_turn(text, session, allowed, disallowed, rewritten)
            continue
        words = text.split()
        if 'HUGE' in words:
            emit({'type': 'assistant', 'message': {'content': [{'type': 'text', 'text': 'x' * (5 << 20)}]}})
        steers = []
        if 'SLOW' in words:
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                ready, _, _ = select.select([stdin], [], [], 0.1)
                if ready:
                    extra = stdin.readline()
                    if not extra:
                        return
                    steer = json.loads(extra)['message']['content']
                    steers.append(steer)
                    trace({'steer_in_turn': steer})
                    emit({'type': 'assistant', 'message': {'content': [{'type': 'text', 'text': 'noted: ' + steer}]}})
        denials = []
        if 'NEEDS' in words:
            rule = words[words.index('NEEDS') + 1]
            if rule not in allowed:
                denials.append({'tool_name': 'Bash', 'tool_use_id': 'x', 'tool_input': {'command': rule}})
        if 'rebased' in words and 'onto' in words:
            tip = words[words.index('onto') + 1]
            rebase_env = dict(os.environ, GIT_EDITOR='true', GIT_AUTHOR_NAME='fake', GIT_AUTHOR_EMAIL='fake@example.invalid',
                              GIT_COMMITTER_NAME='fake', GIT_COMMITTER_EMAIL='fake@example.invalid')
            if subprocess.run(['git', 'rebase', '-q', tip], env=rebase_env, capture_output=True).returncode:
                conflicted = subprocess.run(['git', 'diff', '--name-only', '--diff-filter=U'],
                                            capture_output=True, text=True).stdout.split()
                for name in conflicted:
                    with open(name, 'w') as out:
                        out.write('resolved\n')
                    subprocess.run(['git', 'add', name], check=True)
                subprocess.run(['git', 'rebase', '--continue'], env=rebase_env, check=True, capture_output=True)
            trace({'resolved_onto': tip})
        if 'DENIED' in words:
            worked_around = words[words.index('DENIED') + 1]
            denials.append({'tool_name': 'Bash', 'tool_use_id': 'y', 'tool_input': {'command': worked_around}})
        try_edit(words, allowed, disallowed, denials)
        blocked = bool(denials) and 'NEEDS' in words
        if not blocked and 'WORK' in words:
            name = words[words.index('WORK') + 1]
            emit({'type': 'assistant', 'message': {'content': [
                {'type': 'tool_use', 'name': 'Write', 'input': {'file_path': name, 'content': 'x'}}]}})
            own_repo()
            with open(name, 'w') as out:
                out.write('\n'.join([text] + steers) + '\n')
            subprocess.run(['git', 'add', name], check=True)
            subprocess.run(['git', '-c', 'user.name=fake', '-c', 'user.email=fake@example.invalid',
                            'commit', '-q', '-m', 'add ' + name], check=True)
        if not blocked and 'LINE' in words:
            at = words.index('LINE')
            name, number = words[at + 1], int(words[at + 2])
            own_repo()
            with open(name) as source:
                lines = source.read().splitlines()
            lines[number - 1] = text.replace('\n', ' ')
            with open(name, 'w') as out:
                out.write('\n'.join(lines) + '\n')
            subprocess.run(['git', 'commit', '-q', '-am', 'edit ' + name], check=True,
                           env=dict(os.environ, GIT_AUTHOR_NAME='fake', GIT_AUTHOR_EMAIL='fake@example.invalid',
                                    GIT_COMMITTER_NAME='fake', GIT_COMMITTER_EMAIL='fake@example.invalid'))
        status = 'blocked' if blocked else 'done'
        summary = 'did: ' + text + (' | steers: ' + '; '.join(steers) if steers else '')
        emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': summary,
              'session_id': session, 'permission_denials': denials,
              'structured_output': {'status': status, 'summary': summary,
                                    'decisions': [{'choice': 'fake choice', 'reason': 'fixture'}],
                                    'human_verify': [{'check': 'open the file', 'pass_if': 'it has text'}] if 'HV' in words else [],
                                    'left_open': ['nothing real'], 'notes': ''}})


if __name__ == '__main__':
    main()
