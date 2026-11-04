#!/usr/bin/env python3
"""Model-free stand-in for `pi --mode rpc`, used by test-task.py as a fallback agent.

Each `prompt` command is one run. `PIERROR <file>` ends the run as a provider's refusal, with the
words of <file> as the error; `PICOST <usd>` reports that cost for the run. A worker (its instructions name a task worker) does
`WORK <file>` (writes and commits it) and sends its result to `wsx task result -`, as the
Pi adapter's instructions ask; a manager answers "pi answers: <first line>". The run then
streams Pi's own events: agent_start, an assistant message_end with usage, agent_settled.
Launch arguments and messages go to $FAKE_CLAUDE_TRACE with `adapter: pi`.
ref: crates/wsx/src/task/pi.rs; Pi docs rpc.md, json.md
"""
import json
import os
import subprocess
import sys

TRACE = os.environ.get('FAKE_CLAUDE_TRACE') or os.path.join(os.getcwd(), '..', 'fake-claude.trace')


def trace(record):
    with open(TRACE, 'a') as out:
        out.write(json.dumps(dict(record, adapter='pi')) + '\n')


# ^ The fields the real Pi writes on an assistant message, as recorded in
# crates/wsx/tests/fixtures/agent-streams/pi (docs/tasks.md "Agent stream recordings"); the
# scenario's own values stay. crates/wsx/tests/agent_stream_fakes.rs fails when this drifts.
USAGE = {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'reasoning': 0, 'totalTokens': 0,
         'cost': {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'total': 0}}
MESSAGE = {'api': 'fake-responses', 'provider': 'fake', 'timestamp': 0, 'responseId': 'resp_fake',
           'rawStopReason': 'completed', 'durationMs': 0, 'thinkingLevel': 'off'}


def shaped(value):
    if value.get('type') == 'message_end' and value['message'].get('role') == 'assistant':
        message = {**MESSAGE, **value['message']}
        message['usage'] = {**USAGE, **message['usage']}
        message['content'] = [{'textSignature': '{}', **block} if block['type'] == 'text' else block
                              for block in message['content']]
        if message['stopReason'] == 'error':
            # ^ A refused call has no response: the real message lacks these.
            for name in ('responseId', 'rawStopReason'):
                del message[name]
            del message['usage']['reasoning']
        value = {**value, 'message': message}
    return value


def emit(value):
    sys.stdout.write(json.dumps(shaped(value)) + '\n')
    sys.stdout.flush()


def option(argv, name):
    return argv[argv.index(name) + 1] if name in argv else None


def main():
    argv = sys.argv[1:]
    session = option(argv, '--session-id')
    instructions = ' '.join(value for flag, value in zip(argv, argv[1:]) if flag == '--append-system-prompt')
    worker = 'You are a task worker' in instructions
    trace({'launch': True, 'session': session, 'model': option(argv, '--model'), 'cwd': os.getcwd(),
           'worker': worker, 'rpc': '--mode' in argv and option(argv, '--mode') == 'rpc',
           'no_tools': '--no-tools' in argv, 'no_session': '--no-session' in argv,
           'thinking': option(argv, '--thinking')})
    for line in sys.stdin:
        command = json.loads(line)
        if command.get('type') != 'prompt':
            emit({'id': command.get('id'), 'type': 'response', 'command': command.get('type'), 'success': True})
            continue
        text = command['message']
        trace({'session': session, 'message': text})
        emit({'id': command.get('id'), 'type': 'response', 'command': 'prompt', 'success': True,
              'data': {'disposition': 'started'}})
        emit({'type': 'agent_start'})
        words = text.split()
        # ^ A handover quotes earlier messages: only this message's own words steer the fake.
        own = text.rsplit('Owner (message', 1)[-1].split()
        if 'PIERROR' in own:
            with open(own[own.index('PIERROR') + 1]) as refusal:
                error = refusal.read().strip()
            emit({'type': 'message_end', 'message': {
                'role': 'assistant', 'model': option(argv, '--model') or 'fake', 'stopReason': 'error',
                'errorMessage': error, 'content': [],
                'usage': {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'totalTokens': 0,
                          'cost': {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'total': 0}}}})
            emit({'type': 'agent_end', 'messages': [], 'willRetry': False})
            emit({'type': 'agent_settled', 'aborted': False})
            continue
        cost = float(own[own.index('PICOST') + 1]) if 'PICOST' in own else 0.001
        if worker:
            if 'WORK' in words:
                name = words[words.index('WORK') + 1]
                with open(name, 'w') as out:
                    out.write('written by the fake pi\n')
                subprocess.run(['git', 'add', name], check=True)
                subprocess.run(['git', '-c', 'user.name=fake', '-c', 'user.email=fake@example.invalid',
                                'commit', '-q', '-m', 'add ' + name], check=True)
            result = {'status': 'done', 'summary': 'fake pi finished the turn', 'memo': ''}
            reported = subprocess.run(['wsx', 'task', 'result', '-'], input=json.dumps(result),
                                      capture_output=True, text=True)
            trace({'session': session, 'reported': reported.returncode, 'answer': reported.stdout + reported.stderr})
            answer = 'Done.'
        else:
            answer = 'pi answers: ' + text.strip().splitlines()[0]
        emit({'type': 'tool_execution_start', 'toolCallId': 't1', 'toolName': 'bash', 'args': {'command': 'git status'}})
        emit({'type': 'message_end', 'message': {
            'role': 'assistant', 'model': option(argv, '--model') or 'fake', 'stopReason': 'stop',
            'content': [{'type': 'text', 'text': answer}],
            'usage': {'input': 100, 'output': 10, 'cacheRead': 0, 'cacheWrite': 0, 'totalTokens': 110,
                      'cost': {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'total': cost}}}})
        emit({'type': 'agent_end', 'messages': [], 'willRetry': False})
        emit({'type': 'agent_settled', 'aborted': False})


if __name__ == '__main__':
    main()
