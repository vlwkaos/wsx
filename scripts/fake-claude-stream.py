#!/usr/bin/env python3
"""Model-free stand-in for `claude -p --input-format stream-json` used by test-task.py.

Each user message is a small script of words:
  SLOW         take about 2 s and accept steers during the turn
  NEEDS <rule> deny <rule> and report blocked
  DENIED <rule> deny <rule> but carry on and report done
  WORK <file>  write <file> and commit it
  NOOP         change nothing
  HUGE         first print one 5 MiB stdout line
  HV           return one Human Verify item
  LINE <file> <n>  replace line n of <file> with the message and commit
  MEMO <words> return the rest of the line as the result's memo
  CAPTURE      (with HV) the item has a page path and a small PNG capture
  CMDHV        return a Human Verify item that names a command
  WAIT <file>  keep working until <file> exists, at most 10 minutes
  CHECKRUN_AGAIN  start `wsx task check-run`, kill it after 1 s, run it again and trace that answer
  CHECKRUN_TWICE  run `wsx task check-run` twice on the same tree and trace both answers
  PROBE <script> <spec> <out>  run the test's boundary probe script (python3 <script> <spec> <out>)
               and trace what it wrote to <out>
The host's landing message ("rebased onto <tip>") makes it rebase and resolve every
conflict with the line "resolved".
Every received message and the launch arguments go to $FAKE_CLAUDE_TRACE, and each
message to the session's transcript $HOME/.claude/fake-sessions/<session>.jsonl.
Started without -p it is an interactive pane (`wsx task hand`): it traces its arguments,
writes one line to the transcript it resumes, a new one with --fork-session, and waits.

Started with the manager's instructions it is a scripted manager instead: per turn
  RUN <command>  run that shell command (the wsx task commands it names) and quote its output;
                 OWNER_SAID in it becomes the id of the turn's first owner message
  SCRIPT         (a line) run the rest of the message as one shell script, heredocs too
  TABLE          answer with a wide markdown table that names #4 twice and #2
  TABLE5         answer "Five columns:" with a table of five columns and 30 lines after it
  DIAGRAM        answer with a tall mermaid diagram, drawn after the bubble shows
  PAGE <section> <words>  replace that section of the project page (its instructions name it)
  STATUS         answer from `wsx task ledger`
  SLOW           take about 3 s
  BIG            a last model call of 200k tokens, so the host rotates the conversation
  SUMMED         four model calls of 50k each: the result's usage sums 200k, the context is 50k
Each manager turn's message ends with the host's ledger summary, cut off before the words
are read. Its cost grows by 0.001 USD a turn (`total_cost_usd` is the session's, restored on
`--resume`), and a `set_model` control request changes the model of the next turns.
"""
import json
import os
import select
import signal
import struct
import subprocess
import sys
import time
import zlib

TRACE = os.environ.get('FAKE_CLAUDE_TRACE') or os.path.join(os.getcwd(), '..', 'fake-claude.trace')


def trace(record):
    with open(TRACE, 'a') as out:
        out.write(json.dumps(record) + '\n')


def emit(value):
    sys.stdout.write(json.dumps(value) + '\n')
    sys.stdout.flush()


def parse_args(argv):
    options, index = {}, 0
    while index < len(argv):
        arg = argv[index]
        if arg.startswith('--') and index + 1 < len(argv) and not argv[index + 1].startswith('--'):
            options[arg] = argv[index + 1]
            index += 2
            continue
        options[arg] = True
        index += 1
    return options


def tool_use(use_id, command, **extra):
    emit({'type': 'assistant', 'message': {'content': [
        {'type': 'tool_use', 'id': use_id, 'name': 'Bash', 'input': dict(command=command, **extra)}]}})


def tool_result(use_id, text, failed):
    emit({'type': 'user', 'message': {'role': 'user', 'content': [
        {'type': 'tool_result', 'tool_use_id': use_id, 'is_error': failed, 'content': text}]}})


def probe(script, spec, out):
    """The test's boundary probe, run as this worker's own command inside wsx's boundary."""
    subprocess.run([sys.executable, script, spec, out], check=True)
    with open(out) as answer:
        trace({'probe': json.load(answer)})


def png_chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))


# ^ A real 1x1 PNG, so a browser draws the capture.
PIXEL = (b'\x89PNG\r\n\x1a\n' + png_chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 0, 0, 0, 0))
         + png_chunk(b'IDAT', zlib.compress(b'\x00\x80')) + png_chunk(b'IEND', b''))


def own_repo():
    """A fixture commit must land in this working copy, never in a repository above it."""
    top = subprocess.run(['git', 'rev-parse', '--show-toplevel'], capture_output=True, text=True).stdout.strip()
    assert os.path.realpath(top) == os.path.realpath(os.getcwd()), ('fixture git escaped', os.getcwd(), top)


def rest_of_line(text, word):
    line = next(line for line in text.splitlines() if word in line.split())
    return line.split(word, 1)[1].strip()


def edit_page(page, section, words):
    with open(page) as source:
        text = source.read()
    head = '## %s\n\n' % section
    start = text.index(head) + len(head)
    end = text.find('\n\n', start)
    with open(page, 'w') as out:
        out.write(text[:start] + words + text[end:])


def manager_turn(text, project, page):
    replies = []
    if 'SLOW' in text.split():
        time.sleep(3)
    if '\nSCRIPT\n' in '\n' + text:
        done = subprocess.run(('\n' + text).split('\nSCRIPT\n', 1)[1], shell=True, capture_output=True, text=True)
        return 'script -> %d %s' % (done.returncode, (done.stdout + done.stderr).strip()[:600])
    said = next((line.split('(message ', 1)[1].split(')', 1)[0] for line in text.splitlines()
                 if line.startswith('Owner (message ')), '0')
    for line in text.splitlines():
        if line.startswith('RUN '):
            line = line.replace('OWNER_SAID', said)
            done = subprocess.run(line[4:], shell=True, capture_output=True, text=True)
            replies.append('ran: %s -> %d %s' % (line[4:], done.returncode, (done.stdout + done.stderr).strip()[:300]))
    for line in text.splitlines():
        if line.startswith('PAGE '):
            _, section, words = line.split(' ', 2)
            edit_page(page, section, words)
            replies.append('I updated the %s of the project page.' % section)
    if 'TABLE' in text.split():
        cell = 'a long cell that keeps going ' * 4
        replies.append('Where things stand:\n\n| Task | State | Note |\n|---|---|---|\n'
                       '| #4 | blocked | %s |\n| #4 | asks | %s |\n| #2 | ready | %s |' % (cell, cell, cell))
    if 'TABLE5' in text.split():
        rows = [('alpha', 'blocked', 'owner', 'Monday', 'waits for the network grant'),
                ('beta', 'ready', 'manager', 'Tuesday', 'lands after the checks pass'),
                ('gamma', 'working', 'worker', 'Wednesday', 'writes the migration notes'),
                ('delta', 'done', 'owner', 'Thursday', 'shipped in the last release')]
        table = '\n'.join('| %s |' % ' | '.join(row) for row in rows)
        notes = '\n\n'.join('Note %d about the table.' % n for n in range(1, 31))
        replies.append('Five columns:\n\n| Name | State | Owner | Since | Note |\n|---|---|---|---|---|\n%s\n\n%s' % (table, notes))
    if 'DIAGRAM' in text.split():
        steps = ' --> '.join('S%d[step %d]' % (n, n) for n in range(8))
        replies.append('The plan:\n\n```mermaid\nflowchart TD\n  %s\n```' % steps)
    if 'STATUS' in text.split():
        ledger = json.loads(subprocess.run(['wsx', 'task', 'ledger', '-p', project], capture_output=True,
                                           text=True, check=True).stdout)
        rows = ['#%d %s' % (row['id'], row['state']) for row in ledger['open']]
        replies.append('open: ' + (', '.join(rows) if rows else 'none'))
    if text.startswith('Events'):
        replies.append('noted events')
    return '\n'.join(replies) or 'ok'


def transcript(session, text):
    folder = os.path.join(os.environ.get('HOME', '.'), '.claude', 'fake-sessions')
    os.makedirs(folder, exist_ok=True)
    with open(os.path.join(folder, '%s.jsonl' % session), 'a') as out:
        out.write(json.dumps({'message': text}) + '\n')


def interactive(options):
    resumed = options.get('--resume')
    session = 'fork-%d' % os.getpid() if '--fork-session' in options else resumed
    trace({'interactive': True, 'argv': sys.argv[1:], 'resume': resumed, 'session': session, 'cwd': os.getcwd()})
    if session:
        transcript(session, 'the owner opened the conversation')
    signal.signal(signal.SIGHUP, lambda *_: sys.exit(0))
    while True:
        time.sleep(60)


def main():
    options = parse_args(sys.argv[1:])
    if '-p' not in options:
        return interactive(options)
    instructions = options.get('--append-system-prompt', '')
    project = instructions.split('wsx project ')[1].split('.')[0] if 'You are the manager' in instructions else None
    session = options.get('--resume') or options.get('--session-id')
    trace({'launch': True, 'pid': os.getpid(), 'session': session, 'resume': '--resume' in options,
           'argv': sys.argv[1:], 'instructions': instructions,
           'schema': '--json-schema' in options, 'model': options.get('--model'),
           'schema_required': json.loads(options['--json-schema']).get('required') if '--json-schema' in options else None,
           'mode': options.get('--permission-mode'), 'settings': json.loads(options.get('--settings', '{}')),
           'wsx_env': sorted(name for name in os.environ if name.startswith('WSX_')),
           'git_optional_locks': os.environ.get('GIT_OPTIONAL_LOCKS'),
           'task_probe': os.environ.get('TASK_PROBE'),
           'manager': project is not None, 'actor': os.environ.get('WSX_TASK_ACTOR'),
           'plugin_dir': options.get('--plugin-dir'),
           'cwd': os.getcwd()})
    signal.signal(signal.SIGINT, lambda *_: (trace({'interrupted': True}), sys.exit(130)))
    emit({'type': 'system', 'subtype': 'init', 'session_id': session})
    stdin = sys.stdin.buffer
    # ^ A resumed session restores its earlier spend into total_cost_usd, as Claude Code does.
    spent_file = TRACE + '.spent'
    spending = lambda: json.load(open(spent_file)) if os.path.exists(spent_file) else {}
    model, spent = options.get('--model'), spending().get(session, 0.0) if '--resume' in options else 0.0
    while True:
        line = stdin.readline()
        if not line:
            return
        request = json.loads(line)
        if request.get('type') == 'control_request':
            model = request['request']['model']
            trace({'set_model': model, 'session': session})
            emit({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': request['request_id']}})
            continue
        text = request['message']['content']
        trace({'message': text, 'session': session, 'manager': project is not None, 'model': model})
        transcript(session, text)
        if project is not None:
            # ^ The host's ledger summary ends the message; its titles are not this turn's words.
            text = text.split('\n\nLedger summary from wsx', 1)[0]
            # ^ A handover quotes earlier messages; only the messages after it are this turn's.
            answer = manager_turn(text.split('\n\n', 1)[1] if text.startswith('Handover') else text, project,
                                  instructions.split('The project page ', 1)[1].split(' ', 1)[0])
            # ^ A line of its own: a handover quotes earlier messages as `Owner: BIG`.
            calls = [200000] if 'BIG' in text.splitlines() else [50000] * 4 if 'SUMMED' in text.splitlines() else [5000]
            for tokens in calls:
                emit({'type': 'assistant', 'parent_tool_use_id': None, 'message': {
                    'model': model, 'content': [],
                    'usage': {'input_tokens': 10, 'cache_read_input_tokens': tokens - 10, 'output_tokens': 5}}})
            # ^ A subagent's call is another context: it never counts as the conversation's.
            emit({'type': 'assistant', 'parent_tool_use_id': 'sub', 'message': {
                'model': model, 'content': [], 'usage': {'input_tokens': 900000}}})
            spent += 0.001
            saved = spending() | {session: spent}
            with open(spent_file, 'w') as out:
                json.dump(saved, out)
            emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': answer,
                  'session_id': session, 'permission_denials': [],
                  'usage': {'input_tokens': 10 * len(calls), 'cache_read_input_tokens': sum(calls) - 10 * len(calls),
                            'output_tokens': 5 * len(calls)},
                  'total_cost_usd': round(spent, 6)})
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
        if 'WAIT' in words:
            release = words[words.index('WAIT') + 1]
            deadline = time.monotonic() + 600
            while not os.path.exists(release) and time.monotonic() < deadline:
                time.sleep(0.2)
        denials = []
        if 'NEEDS' in words:
            rule = words[words.index('NEEDS') + 1]
            denials.append({'tool_name': 'Bash', 'tool_use_id': 'x', 'tool_input': {'command': rule}})
        if 'PROBE' in words:
            probe(*words[words.index('PROBE') + 1:words.index('PROBE') + 4])
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
        if 'CHECKRUN' in words:
            # ^ Through the task host's broker (WSX_TASK_BROKER); any other command is refused.
            ran = subprocess.run(['wsx', 'task', 'check-run'], capture_output=True, text=True)
            other = subprocess.run(['wsx', 'task', 'list'], capture_output=True, text=True)
            trace({'check_run': [ran.returncode, ran.stdout + ran.stderr], 'check_run_other': [other.returncode, other.stderr]})
        if 'CHECKRUN_AGAIN' in words:
            # ^ A client that gives up (a Bash timeout) and asks again joins the run it left.
            first = subprocess.Popen(['wsx', 'task', 'check-run'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(1)
            first.kill()
            first.wait()
            again = subprocess.run(['wsx', 'task', 'check-run'], capture_output=True, text=True)
            trace({'check_run_again': [again.returncode, again.stdout + again.stderr]})
        if 'CHECKRUN_TWICE' in words:
            answers = [subprocess.run(['wsx', 'task', 'check-run'], capture_output=True, text=True) for _ in range(2)]
            trace({'check_run_twice': [[ran.returncode, ran.stdout + ran.stderr] for ran in answers]})
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
        # ^ CAPTURE leaves a small PNG for the item; CMDHV names a command, which wsx sends back.
        verify = []
        if 'HV' in words:
            verify.append({'check': 'open the file', 'where': '/#/x' if 'CAPTURE' in words else '',
                           'capture': 'shot.png' if 'CAPTURE' in words else '', 'pass_if': 'it has text'})
            if 'CAPTURE' in words:
                with open('shot.png', 'wb') as out:
                    out.write(PIXEL)
        if 'CMDHV' in words:
            verify.append({'check': 'run `wsx task list`', 'where': '', 'capture': '', 'pass_if': 'it lists the task'})
        summary = 'did: ' + text + (' | steers: ' + '; '.join(steers) if steers else '')
        emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': summary,
              'session_id': session, 'permission_denials': denials,
              'structured_output': {'status': status, 'summary': summary,
                                    'decisions': [{'choice': 'fake choice', 'reason': 'fixture'}],
                                    'evidence': [{'check': 'the fixture ran', 'proof': 'fake-claude-stream.py'}] if status == 'done' else [],
                                    'human_verify': verify,
                                    'left_open': ['nothing real'], 'notes': '',
                                    'memo': rest_of_line(text, 'MEMO') if 'MEMO' in words else ''}})


if __name__ == '__main__':
    main()
