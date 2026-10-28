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
  NORESULT <n> end this turn and the next n-1 without a result, each after a 0-token result as
               Claude's own turn for a background-task notification ends
  CAPTURE      (with HV) the item has a page path and a small PNG capture
  CMDHV        return a Human Verify item that names a command
  BAREHV       return a Human Verify item about a page without a capture
  WAIT <file>  keep working until <file> exists, at most 10 minutes
  CHECKRUN_AGAIN  start `wsx task check-run`, kill it after 1 s, run it again and trace that answer
  CHECKRUN_TWICE  run `wsx task check-run` twice on the same tree and trace both answers
  CHECKRUN_WITH <a,b>  run `wsx task check-run a b`, then a bare `wsx task check-run`, and trace both
  CHECKRUN_LATE <a>  start `wsx task check-run a` in the background and finish before its answer
  REPEAT       return the result again 1 s later, as Claude's own turn for a background notice
  LIMIT <s>    (once per process) end the turn as Claude's usage limit does, resetting in <s> s;
               the next message continues this one (also for the manager)
  PROBE <script> <spec> <out>  run the test's boundary probe script (python3 <script> <spec> <out>)
               and trace what it wrote to <out>
  BIG          a last model call of 400k tokens, past the worker's rotateTokens
The host's request for handoff notes ("Write <path> for that fresh conversation") makes it
write "NOTES from the fixture" to <path>.
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
  TABLE5         answer "Five columns:" with a table of five columns and 20 rows, and 30 lines after it
  DIAGRAM        answer with a tall mermaid diagram, drawn after the bubble shows
  PAGE <section> <words>  replace that section of the project page (its instructions name it)
  STATUS         answer from `wsx task ledger`
  NOTIFY         first end a 0-token turn with no result, as Claude's own turn for a notification
  SLOW           take about 3 s
  LONG           take about 25 s
  BIG            a last model call of 200k tokens, so the host rotates the conversation
  SUMMED         four model calls of 50k each: the result's usage sums 200k, the context is 50k
Each manager turn's message ends with the host's ledger summary, cut off before the words
are read. Its cost grows by 0.001 USD a turn (`total_cost_usd` is the session's, restored on
`--resume`), and a `set_model` control request changes the model of the next turns.
"""
import json
import os
import re
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


# 3x5 glyphs for a capture's label, a row per string, '#' an on cell.
GLYPHS = {'C': ['###', '#..', '#..', '#..', '###'], 'A': ['###', '#.#', '###', '#.#', '#.#'],
          'P': ['###', '#.#', '###', '#..', '#..'], 'T': ['###', '.#.', '.#.', '.#.', '.#.'],
          'U': ['#.#', '#.#', '#.#', '#.#', '###'], 'R': ['###', '#.#', '##.', '#.#', '#.#'],
          'E': ['###', '#..', '###', '#..', '###'], 'D': ['##.', '#.#', '#.#', '#.#', '##.'],
          ' ': ['...'] * 5}


def capture_png(label):
    """A visible, non-square capture: a light border, a grid, a blue corner block and `label`
    in large letters, so a viewer's fit, centering and edges show in a screenshot."""
    width, height, scale, x0, y0 = 480, 300, 10, 60, 120
    on = {(index * 4 + col, row) for index, char in enumerate(label)
          for row, line in enumerate(GLYPHS.get(char, GLYPHS[' '])) for col, cell in enumerate(line) if cell == '#'}
    rows = []
    for y in range(height):
        row = bytearray(b'\x00')
        for x in range(width):
            if x < 6 or y < 6 or x >= width - 6 or y >= height - 6:
                pixel = (230, 230, 230)
            elif x < 90 and y < 60:
                pixel = (70, 140, 220)
            elif x >= x0 and y >= y0 and ((x - x0) // scale, (y - y0) // scale) in on:
                pixel = (250, 200, 60)
            elif x % 30 == 0 or y % 30 == 0:
                pixel = (90, 95, 105)
            else:
                pixel = (40, 44, 52)
            row += bytes(pixel)
        rows.append(bytes(row))
    return (b'\x89PNG\r\n\x1a\n' + png_chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
            + png_chunk(b'IDAT', zlib.compress(b''.join(rows))) + png_chunk(b'IEND', b''))


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
    if 'LONG' in text.split():
        time.sleep(25)
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
        table = '\n'.join('| %s |' % ' | '.join(row) for row in rows * 5)
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
        # ^ A land gets a reply of two sentences, with a task number inside one: the web shows its
        # first sentence and More (docs/web-client-tasks.md "Project screen").
        landed = re.search(r'#(\d+) landed\b', text)
        replies.append('noted events. #%s landed, so its change is on main and nothing else waits for you.' % landed[1]
                       if landed else 'noted events')
    return '\n'.join(replies) or 'ok'


def transcript(session, text):
    folder = os.path.join(os.environ.get('HOME', '.'), '.claude', 'fake-sessions')
    os.makedirs(folder, exist_ok=True)
    with open(os.path.join(folder, '%s.jsonl' % session), 'a') as out:
        out.write(json.dumps({'message': text}) + '\n')


def interactive(options):
    resumed = options.get('--resume')
    session = 'fork-%d' % os.getpid() if '--fork-session' in options else resumed
    trace({'interactive': True, 'argv': sys.argv[1:], 'resume': resumed, 'session': session, 'cwd': os.getcwd(),
           'claude_config_dir': os.environ.get('CLAUDE_CONFIG_DIR')})
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
           'broker': os.environ.get('WSX_TASK_BROKER'),
           'task_probe': os.environ.get('TASK_PROBE'),
           'manager': project is not None, 'actor': os.environ.get('WSX_TASK_ACTOR'),
           'plugin_dir': options.get('--plugin-dir'),
           'claude_config_dir': os.environ.get('CLAUDE_CONFIG_DIR'),
           'cwd': os.getcwd()})
    signal.signal(signal.SIGINT, lambda *_: (trace({'interrupted': True}), sys.exit(130)))
    emit({'type': 'system', 'subtype': 'init', 'session_id': session})
    stdin = sys.stdin.buffer
    # ^ A resumed session restores its earlier spend into total_cost_usd, as Claude Code does.
    spent_file = TRACE + '.spent'
    spending = lambda: json.load(open(spent_file)) if os.path.exists(spent_file) else {}
    model, spent = options.get('--model'), spending().get(session, 0.0) if '--resume' in options else 0.0
    no_result = 0
    limited, limit_hit = None, False
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
        if limited is not None:
            text, limited = limited + '\n\n' + text, None
        elif 'LIMIT' in text.split() and not limit_hit:
            limit_hit, limited = True, text
            usage_limit(session, int(text.split()[text.split().index('LIMIT') + 1]))
            continue
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
            if 'NOTIFY' in text.split():
                emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': '',
                      'session_id': session, 'permission_denials': [],
                      'usage': {'input_tokens': 0, 'output_tokens': 0}})
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
        if no_result:
            no_result = end_without_result(session, no_result)
            continue
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
        if 'CHECKRUN_WITH' in words:
            named = words[words.index('CHECKRUN_WITH') + 1].split(',')
            answers = [subprocess.run(['wsx', 'task', 'check-run', *argv], capture_output=True, text=True)
                       for argv in (named, [])]
            trace({'check_run_with': [[ran.returncode, ran.stdout + ran.stderr] for ran in answers]})
        if 'CHECKRUN_LATE' in words:
            # ^ A worker that does not wait for its background check-run: the host sends the answer.
            subprocess.Popen(['wsx', 'task', 'check-run', words[words.index('CHECKRUN_LATE') + 1]],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(1)
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
        # ^ CAPTURE leaves a small PNG for the item; CMDHV names a command and BAREHV is a page
        # without a capture: wsx sends both back.
        verify = []
        # ^ VIDEO leaves the small committed MP4 and a PNG poster; BIGVIDEO and NOTVIDEO add items with a
        # video over 25 MiB and a text file named .mp4: wsx drops both captures.
        if 'HV' in words:
            shown = 'CAPTURE' in words or 'VIDEO' in words
            verify.append({'check': 'open the file', 'where': '/#/x' if shown else 'a real device',
                           'capture': 'clip.mp4' if 'VIDEO' in words else 'shot.png' if 'CAPTURE' in words else '',
                           'poster': 'poster.png' if 'VIDEO' in words else '', 'pass_if': 'it has text'})
            if 'CAPTURE' in words:
                # ^ Labeled by the first worked file's stem, so two captures differ: "CAPTURE D".
                stem = words[words.index('WORK') + 1][:1].upper() if 'WORK' in words else ''
                with open('shot.png', 'wb') as out:
                    out.write(capture_png('CAPTURE ' + stem))
            if 'VIDEO' in words:
                with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'fixtures', 'check-clip.mp4'), 'rb') as clip:
                    with open('clip.mp4', 'wb') as out:
                        out.write(clip.read())
                with open('poster.png', 'wb') as out:
                    out.write(capture_png('CAPTURE'))
        if 'BIGVIDEO' in words:
            with open('huge.mp4', 'wb') as out:
                out.write(b'\0\0\0\x20ftypisom')
                out.truncate(26 * 1024 * 1024)
            verify.append({'check': 'the long video plays', 'where': '/#/x', 'capture': 'huge.mp4', 'poster': '', 'pass_if': 'it plays'})
        if 'NOTVIDEO' in words:
            with open('notes.mp4', 'w') as out:
                out.write('not a video\n')
            verify.append({'check': 'the text video plays', 'where': '/#/x', 'capture': 'notes.mp4', 'poster': '', 'pass_if': 'it plays'})
        if 'CMDHV' in words:
            verify.append({'check': 'run `wsx task list`', 'where': '', 'capture': '', 'pass_if': 'it lists the task'})
        if 'BAREHV' in words:
            verify.append({'check': 'the page reads well', 'where': '/#/x', 'capture': '', 'pass_if': 'it reads well'})
        if 'BACKGROUND' in words:
            # ^ BACKGROUND <s>: the turn ends while a job runs, which ends after s seconds and
            # gets Claude's own turn with the result. BACKGROUND never: it never ends.
            after = words[words.index('BACKGROUND') + 1]
            background_turn(session, model)
            if after == 'never':
                continue
            time.sleep(int(after))
            background_notice(session)
        if 'NORESULT' in words:
            no_result = end_without_result(session, int(words[words.index('NORESULT') + 1]))
            continue
        handoff = re.search(r'Write (\S+) for that fresh conversation', text)
        if handoff:
            with open(handoff.group(1), 'w') as out:
                out.write('NOTES from the fixture\n')
        if 'BIG' in words:
            emit({'type': 'assistant', 'parent_tool_use_id': None, 'message': {
                'model': model, 'content': [],
                'usage': {'input_tokens': 10, 'cache_read_input_tokens': 399990, 'output_tokens': 5}}})
        summary = 'did: ' + text + (' | steers: ' + '; '.join(steers) if steers else '')
        for repeat in range(2 if 'REPEAT' in words else 1):
            time.sleep(repeat)
            emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': summary,
                  'session_id': session, 'permission_denials': denials,
                  'structured_output': {'status': status, 'summary': summary,
                                        'decisions': [{'choice': 'fake choice', 'reason': 'fixture'}],
                                        'evidence': [{'check': 'the fixture ran', 'proof': 'fake-claude-stream.py'}] if status == 'done' else [],
                                        'human_verify': verify,
                                        'left_open': ['nothing real'], 'notes': '',
                                        'memo': rest_of_line(text, 'MEMO') if 'MEMO' in words else ''}})


def usage_limit(session, seconds):
    """Claude's usage limit: a rejected rate_limit_event, the synthetic API error, its result."""
    resets = int(time.time()) + seconds
    info = {'status': 'rejected', 'resetsAt': resets, 'rateLimitType': 'five_hour', 'isUsingOverage': False}
    words = "You've hit your session limit \u00b7 resets 12:10am (Asia/Seoul)"
    emit({'type': 'rate_limit_event', 'rate_limit_info': info, 'session_id': session})
    emit({'type': 'assistant', 'parent_tool_use_id': None, 'session_id': session, 'error': 'rate_limit',
          'api_error': 'usage_limit_reached', 'api_error_status': 429, 'api_error_params': {'rate_limit_info': info},
          'message': {'model': '<synthetic>', 'content': [{'type': 'text', 'text': words}],
                      'usage': {'input_tokens': 0, 'output_tokens': 0}}})
    emit({'type': 'result', 'subtype': 'success', 'is_error': True, 'result': words, 'session_id': session,
          'api_error_status': 429, 'api_error': 'usage_limit_reached', 'permission_denials': [],
          'usage': {'input_tokens': 0, 'output_tokens': 0}})


def background_turn(session, model):
    """A turn that starts a background subagent and ends without a result, as Claude 2.1.310 streams it."""
    job = {'task_id': 'a57e6485d4d828dfd', 'task_type': 'local_agent', 'description': 'review the diff'}
    emit({'type': 'assistant', 'parent_tool_use_id': None, 'session_id': session, 'message': {
        'model': model, 'content': [{'type': 'tool_use', 'id': 'toolu_bg', 'name': 'Agent', 'input': {'prompt': 'review'}}],
        'usage': {'input_tokens': 10, 'output_tokens': 5}}})
    emit({'type': 'system', 'subtype': 'background_tasks_changed', 'session_id': session,
          'tasks': [job, {'task_id': 'own', 'task_type': 'local_bash', 'description': 'watcher', 'ambient': True}]})
    emit({'type': 'system', 'subtype': 'task_started', 'session_id': session, 'task_id': job['task_id'],
          'tool_use_id': 'toolu_bg', 'task_type': 'local_agent', 'is_backgrounded': True, 'description': job['description']})
    emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': 'The review runs in the background.',
          'session_id': session, 'permission_denials': [], 'usage': {'input_tokens': 10, 'output_tokens': 5}})


def background_notice(session):
    """The job ends: its notice, the live set with only Claude's own watcher, the init of Claude's own turn."""
    emit({'type': 'system', 'subtype': 'task_notification', 'session_id': session, 'task_id': 'a57e6485d4d828dfd',
          'tool_use_id': 'toolu_bg', 'status': 'completed', 'output_file': '', 'summary': 'review done'})
    emit({'type': 'system', 'subtype': 'background_tasks_changed', 'session_id': session,
          'tasks': [{'task_id': 'own', 'task_type': 'local_bash', 'description': 'watcher', 'ambient': True}]})
    emit({'type': 'system', 'subtype': 'init', 'session_id': session})


def end_without_result(session, left):
    """A notification turn with no model call, then a model turn with no result."""
    for usage in ({'input_tokens': 0, 'output_tokens': 0}, {'input_tokens': 10, 'output_tokens': 5}):
        emit({'type': 'result', 'subtype': 'success', 'is_error': False, 'result': '',
              'session_id': session, 'permission_denials': [], 'usage': usage})
    return left - 1


if __name__ == '__main__':
    main()
