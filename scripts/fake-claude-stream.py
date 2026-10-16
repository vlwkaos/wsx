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
The host's landing message ("rebased onto <tip>") makes it rebase and resolve every
conflict with the line "resolved".
Every received message and the launch arguments go to $FAKE_CLAUDE_TRACE.
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
    options, allowed, index = {}, [], 0
    while index < len(argv):
        arg = argv[index]
        if arg == '--allowedTools':
            index += 1
            while index < len(argv) and not argv[index].startswith('--'):
                allowed.append(argv[index])
                index += 1
            continue
        if arg.startswith('--') and index + 1 < len(argv) and not argv[index + 1].startswith('--'):
            options[arg] = argv[index + 1]
            index += 2
            continue
        options[arg] = True
        index += 1
    return options, allowed


def main():
    options, allowed = parse_args(sys.argv[1:])
    session = options.get('--resume') or options.get('--session-id')
    trace({'launch': True, 'pid': os.getpid(), 'session': session, 'resume': '--resume' in options,
           'allowed': allowed, 'schema': '--json-schema' in options, 'model': options.get('--model'),
           'wsx_env': sorted(name for name in os.environ if name.startswith('WSX_')),
           'cwd': os.getcwd()})
    signal.signal(signal.SIGINT, lambda *_: (trace({'interrupted': True}), sys.exit(130)))
    emit({'type': 'system', 'subtype': 'init', 'session_id': session})
    stdin = sys.stdin.buffer
    while True:
        line = stdin.readline()
        if not line:
            return
        text = json.loads(line)['message']['content']
        trace({'message': text, 'session': session})
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
        blocked = bool(denials) and 'NEEDS' in words
        if not blocked and 'WORK' in words:
            name = words[words.index('WORK') + 1]
            emit({'type': 'assistant', 'message': {'content': [
                {'type': 'tool_use', 'name': 'Write', 'input': {'file_path': name, 'content': 'x'}}]}})
            with open(name, 'w') as out:
                out.write('\n'.join([text] + steers) + '\n')
            subprocess.run(['git', 'add', name], check=True)
            subprocess.run(['git', '-c', 'user.name=fake', '-c', 'user.email=fake@example.invalid',
                            'commit', '-q', '-m', 'add ' + name], check=True)
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
