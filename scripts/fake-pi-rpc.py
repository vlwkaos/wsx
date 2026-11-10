#!/usr/bin/env python3
"""Model-free stand-in for `pi --mode rpc`, used by test-task.py as a fallback agent.

Each `prompt` command is one run. `PIERROR <file>` ends the run as a provider's refusal, with the
words of <file> as the error; `PICOST <usd>` reports that cost for the run; `PICTX <n>` reports a context of n tokens. A worker (its instructions name a task worker) does
`WORK <file>` (writes and commits it) and sends its result to `wsx task result -`, as the
Pi adapter's instructions ask; a manager answers "pi answers: <first line>". The run then
streams Pi's own events: agent_start, an assistant message_end with usage, agent_settled.
Launch arguments, the Pi folder (`PI_CODING_AGENT_DIR`: its files and `models.json`) and messages
go to $FAKE_CLAUDE_TRACE with `adapter: pi`.
`FORBID <script> <spec> <out>` (a worker) runs scripts/fake-weak-model.py as its own command, as a weak model
would try forbidden actions; a line `RUN <command>` of a message runs that shell command and quotes its
answer. ref: crates/wsx/src/task/pi.rs; Pi docs rpc.md, json.md
"""
import json
import os
import subprocess
import sys

TRACE = os.environ.get('FAKE_CLAUDE_TRACE') or os.path.join(os.getcwd(), '..', 'fake-claude.trace')


def trace(record):
    with open(TRACE, 'a') as out:
        out.write(json.dumps(dict(record, adapter='pi')) + '\n')


def leak(session):
    # ^ LEAK: two tool processes left running, one in the agent's process group and one in a
    # session of its own, as Claude's Bash tool starts its shells. Both keep its environment.
    kids = [subprocess.Popen(['sleep', '600'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL, start_new_session=own) for own in (False, True)]
    trace({'leak': [kid.pid for kid in kids], 'agent': os.getpid(), 'session': session})

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


def argv_shown(argv):
    # ^ The instruction texts are long; their own words are checked as `worker` and `talk_rule`.
    return [('<text>' if before in ('--append-system-prompt', '--system-prompt') and not os.path.isfile(value) else value)
            for before, value in zip([''] + argv, argv)]


def models_json(agent_dir):
    path = os.path.join(agent_dir or '', 'models.json')
    return open(path).read() if os.path.isfile(path) else None


def main():
    argv = sys.argv[1:]
    session = option(argv, '--session-id')
    instructions = ' '.join(value for flag, value in zip(argv, argv[1:]) if flag == '--append-system-prompt')
    agent_dir = os.environ.get('PI_CODING_AGENT_DIR')
    worker = 'You are a task worker' in instructions
    trace({'launch': True, 'session': session, 'model': option(argv, '--model'), 'cwd': os.getcwd(),
           'worker': worker, 'talk_rule': "Judge from the owner's words" in instructions, 'rpc': '--mode' in argv and option(argv, '--mode') == 'rpc',
           'no_tools': '--no-tools' in argv, 'no_session': '--no-session' in argv,
           'thinking': option(argv, '--thinking'), 'argv': argv_shown(argv),
           'agent_dir': agent_dir, 'agent_dir_files': sorted(os.listdir(agent_dir)) if agent_dir else None,
           'models_json': models_json(agent_dir)})
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
        # ^ So are the notes the host attaches, which carry earlier messages with their ids.
        own = text.split('\n\nNotes that match this message', 1)[0].rsplit('Owner (message', 1)[-1]
        own = own.split('Events (message', 1)[0].split()
        # ^ Event lines name tasks by title, and a title may hold a word of this fake: it is no owner's word.
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
        context = int(own[own.index('PICTX') + 1]) if 'PICTX' in own else 100
        if worker:
            if 'LEAK' in words:
                leak(session)
            if 'WORK' in words:
                name = words[words.index('WORK') + 1]
                with open(name, 'w') as out:
                    out.write('written by the fake pi\n')
                for git in (['add', name], ['-c', 'user.name=fake', '-c', 'user.email=fake@example.invalid', 'commit', '-q', '-m', 'add ' + name]):
                    done = subprocess.run(['git'] + git, capture_output=True, text=True)
                    if done.returncode:
                        # ^ The reason goes to the trace: the scenario shows it when the task fails.
                        trace({'session': session, 'git_failed': git[-3:], 'error': done.stderr[-800:]})
                        sys.exit(1)
            if 'FORBID' in words:
                script, spec, out = words[words.index('FORBID') + 1:words.index('FORBID') + 4]
                # ^ A crash of the script is an answer too: the test reads it from the trace.
                done = subprocess.run([sys.executable, script, spec, out], capture_output=True, text=True)
                trace({'session': session, 'forbid_exit': done.returncode, 'forbid_error': done.stderr[-1500:]})
                if done.returncode == 0:
                    with open(out) as answer:
                        trace({'session': session, 'forbid': json.load(answer)})
            result = {'status': 'done', 'summary': 'fake pi finished the turn', 'memo': ''}
            reported = subprocess.run(['wsx', 'task', 'result', '-'], input=json.dumps(result),
                                      capture_output=True, text=True)
            trace({'session': session, 'reported': reported.returncode, 'answer': reported.stdout + reported.stderr})
            answer = 'Done.'
        elif any(line.startswith('RUN ') for line in text.splitlines()):
            # ^ As fake-claude-stream.py: the manager's own commands, here through the broker.
            said = next((line.split('(message ', 1)[1].split(')', 1)[0] for line in text.splitlines()
                         if line.startswith('Owner (message ')), '0')
            answer = '\n'.join(
                'ran: %s -> %d %s' % (line[4:], done.returncode, (done.stdout + done.stderr).strip()[:600])
                for line in text.splitlines() if line.startswith('RUN ')
                for done in [subprocess.run(line.replace('OWNER_SAID', said)[4:], shell=True, capture_output=True, text=True)])
        else:
            answer = 'pi answers: ' + text.strip().splitlines()[0]
        emit({'type': 'tool_execution_start', 'toolCallId': 't1', 'toolName': 'bash', 'args': {'command': 'git status'}})
        emit({'type': 'message_end', 'message': {
            'role': 'assistant', 'model': option(argv, '--model') or 'fake', 'stopReason': 'stop',
            # ^ PITHINK: a long thinking block ahead of the short answer, as Pi's `thinking` content has.
            'content': ([{'type': 'thinking', 'thinking': 'the whole answer, in prose. ' * 8}] if 'PITHINK' in own else [])
            + [{'type': 'text', 'text': answer}],
            'usage': {'input': context, 'output': 10, 'cacheRead': 0, 'cacheWrite': 0, 'totalTokens': context + 10,
                      'cost': {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'total': cost}}}})
        emit({'type': 'agent_end', 'messages': [], 'willRetry': False})
        emit({'type': 'agent_settled', 'aborted': False})


if __name__ == '__main__':
    main()
