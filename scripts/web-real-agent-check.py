#!/usr/bin/env python3
"""Real agent turns through the hosted web client (docs/web-client.md "Verification").

Opt-in: it starts the installed Claude in an isolated wsxd and makes short model turns
on the person's own account. It checks what the fixture agents cannot.
  --scenario turn (default): the timeline against the real saved format, and Stop
    against the real program. One model turn.
  --scenario answers: a blocked agent answered from the chat. A permission dialog is
    approved by its number and another is denied with Escape, a question tool is
    answered by number, and a stale stamp is refused. Three model turns.
The agent runs with the person's HOME (its login and its wsx hooks); wsxd, the web
server and the project are private to this run. The live daemon is never contacted.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('journey', ROOT / 'scripts/test-web-client.py')
journey = importlib.util.module_from_spec(spec)
spec.loader.exec_module(journey)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--daemon', type=Path, default=ROOT / 'target/debug/wsxd')
    parser.add_argument('--agent', nargs='+', default=['claude', '--model', 'haiku'],
                        help='the agent command line: claude or pi, with its wsx integration installed')
    parser.add_argument('--scenario', choices=['turn', 'answers'], default='turn')
    args = parser.parse_args()
    if args.scenario == 'answers' and Path(args.agent[0]).name == 'claude' and '--permission-mode' not in args.agent:
        # The scenario needs the dialogs: no mode that answers them by itself.
        args.agent += ['--permission-mode', 'manual']
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    agent = shutil.which(args.agent[0])
    assert agent, args.agent[0] + ' is not installed'
    work = ROOT / '.work' / ('real-' + str(os.getpid()))
    work.parent.mkdir(exist_ok=True)
    work.mkdir(mode=0o700)
    for name in ('home', 'state', 'project'):
        (work / name).mkdir(mode=0o700)
    config = (work / 'home/Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'config/wsx')
    config.mkdir(parents=True)
    (config / 'config-v2.toml').write_text('resume_agents_on_restore = false\n')
    path = work / 'state/wsx/wsx.sock'
    env = {'HOME': str(work / 'home'), 'XDG_CONFIG_HOME': str(work / 'config'), 'XDG_STATE_HOME': str(work / 'state'),
           'XDG_CACHE_HOME': str(work / 'cache'), 'WSX_SOCKET': str(path), 'WSX_DAEMON_BIN': str(wsxd),
           'PATH': os.environ['PATH'], 'SHELL': '/bin/sh', 'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8'}
    project = (work / 'project').resolve()
    subprocess.run(['git', 'init', '-q', '-b', 'main', str(project)], env=env, check=True, timeout=5)
    (project / 'note.txt').write_text('The word is heron.\n')
    processes, success, facts = [], False, {}

    def call(method, params=None):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(5)
            client.connect(str(path))
            stream = client.makefile('rb')
            client.sendall(b'{"method":"hello","params":{"protocol":16}}\n')
            stream.readline()
            client.sendall(json.dumps({'method': method, **({'params': params} if params is not None else {})}).encode() + b'\n')
            return json.loads(stream.readline())

    def wait(test, label, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = test()
            if value:
                return value
            time.sleep(0.2)
        raise AssertionError(label + ' timeout')

    def screen(pane):
        frames = call('view', {'pane_ids': [pane]})['data']['frames']
        return ''.join(cell[0] for cell in frames[0]['cells']) if frames else ''

    def type_keys(pane, data):
        assert call('terminal_acquire', {'pane_id': pane, 'client_id': 9, 'takeover': False})['type'] == 'ack'
        call('terminal_input', {'pane_id': pane, 'client_id': 9, 'bytes': list(data)})
        call('terminal_release', {'pane_id': pane, 'client_id': 9})

    try:
        daemon = subprocess.Popen([str(wsxd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        processes.append(daemon)
        wait(path.exists, 'daemon')
        assert call('synchronize_projects', {'projects': [
            {'name': 'real', 'path': str(project), 'worktrees': [{'path': str(project), 'branch': 'main'}]}]})['type'] == 'ack'
        snapshot = call('snapshot')['data']
        created = call('session_create', {'worktree_id': snapshot['worktrees'][0]['id'], 'label': 'real agent',
            'command': ['/usr/bin/env', 'HOME=' + os.environ['HOME'], agent, *args.agent[1:]], 'rows': 40, 'cols': 120})
        assert created['type'] == 'created', created
        session = created['data']['id']
        pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == session)

        def agent_state():
            info = next(p for p in call('snapshot')['data']['panes'] if p['id'] == pane)['agent']
            return info and info['attached'] and info['state']

        # A new folder asks for trust once; choose "Yes, I trust this folder" like a person would.
        # The dialog may not take keys at once, so act on what the screen shows.
        def ready():
            text = screen(pane)
            if '❯ Yes, I trust this folder' in text:
                type_keys(pane, b'\r')
                time.sleep(1)
            elif 'Yes, I trust this folder' in text:
                type_keys(pane, b'\x1b[B')
                time.sleep(0.5)
            return agent_state() in ('idle', 'done')
        wait(ready, 'the agent to report idle (is the wsx Claude integration installed?)', timeout=60)

        # A fresh Claude shows a dimmed suggestion in its empty editor; wsxd must take it as empty.
        claude = Path(agent).name == 'claude'
        deadline = time.monotonic() + 8
        while claude and '❯ Try "' not in screen(pane) and time.monotonic() < deadline:
            time.sleep(0.2)
        facts['fresh_editor_suggestion'] = '❯ Try "' in screen(pane)
        port = journey.free_port()
        site_file = work / 'site.toml'
        site_file.write_text(f'''id = "real"
name = "Real"
port = {port}
public_origin = "http://127.0.0.1:{port}"
panels = ["chat", "terminal"]
[access]
projects = ["{project}"]
capabilities = ["chat_send", "chat_answer", "agent_write", "terminal_view", "terminal_input"]
''')
        site_file.chmod(0o600)
        processes.append(subprocess.Popen([str(wsx), 'web', 'serve', '--site', str(site_file)], env=env, cwd=project,
                                          stdout=subprocess.DEVNULL, stderr=(work / 'web.log').open('w')))
        site = journey.Site(port, '')

        def up():
            try:
                return site.request('GET', '/manifest.webmanifest')[0] == 200
            except OSError:
                return False
        wait(up, 'web server')
        code = subprocess.run([str(wsx), 'web', 'pair', '--site', str(site_file)], env=env, capture_output=True, text=True,
                              timeout=10).stdout.splitlines()[0].rsplit(' ', 1)[1]
        status, headers, _ = site.request('POST', '/api/pair', {'code': code, 'name': 'check'}, raw=True)
        assert status == 200
        site.cookie = headers['set-cookie'].split(';')[0]
        chat = '/api/chat/%d' % session

        def timeline():
            return site.request('GET', chat + '/timeline')[1]

        if args.scenario == 'answers':
            def question():
                return timeline().get('question')

            def answer(stamp, key):
                return site.request('POST', chat + '/answer', {'stamp': stamp, 'key': key})

            def ask(text, tool):
                wait(lambda: agent_state() in ('idle', 'done'), 'the agent to be ready', timeout=60)
                status, sent = site.request('POST', chat + '/send', {'text': text})
                assert status == 200, sent
                asked = wait(lambda: (lambda now: now if now and now['options'] else None)(question()),
                             'a dialog with numbered choices for ' + tool, timeout=120)
                assert asked['request'] and asked['request']['name'] == tool, asked['request']
                return asked

            def settled():
                return wait(lambda: agent_state() in ('idle', 'done') and timeline(), 'the turn to end', timeout=120)

            # 1. A permission dialog, approved by its number. A stale stamp is refused first.
            asked = ask('Create the file approved.txt with the Write tool. Its content is the one word: yes. Do nothing else.', 'Write')
            facts['permission'] = {'request': asked['request'], 'options': asked['options'], 'rows': len(asked['screen'])}
            assert asked['options'][0]['key'] == '1' and asked['options'][0]['label'].lower().startswith('yes'), asked['options']
            stale = answer('0' * 32, '1')
            assert stale[0] == 409 and stale[1]['error']['code'] == 'question_changed', stale
            time.sleep(1)
            assert not (project / 'approved.txt').exists(), 'a refused answer reached the agent'
            assert answer(asked['stamp'], '1')[0] == 200
            wait(lambda: (project / 'approved.txt').exists(), 'the approved file to be written', timeout=60)
            settled()
            facts['approved'] = True
            # 2. Another permission dialog, denied with Escape.
            asked = ask('Create the file denied.txt with the Write tool. Its content is the one word: no. Do nothing else.', 'Write')
            assert answer(asked['stamp'], 'escape')[0] == 200
            wait(lambda: agent_state() != 'blocked', 'the dialog to close after Escape', timeout=30)
            time.sleep(2)
            assert not (project / 'denied.txt').exists(), 'a denied file was written'
            facts['denied'] = agent_state()
            # 3. A question tool, answered by the number of one choice.
            asked = ask('Use the AskUserQuestion tool to ask me one question: "Which color do you prefer?" with exactly the options '
                        'Red, Green and Blue. After I answer, reply with only the word I chose.', 'AskUserQuestion')
            facts['question'] = {'request': asked['request'], 'options': asked['options']}
            assert 'color' in asked['request']['title'].lower(), asked['request']
            green = next(option for option in asked['options'] if option['label'].lower().startswith('green'))
            assert answer(asked['stamp'], green['key'])[0] == 200
            # Some versions show a review step with its own numbered list before they go on.
            deadline = time.monotonic() + 60
            while agent_state() == 'blocked' and time.monotonic() < deadline:
                now = question()
                if now and now['stamp'] != asked['stamp'] and any('submit' in option['label'].lower() for option in now['options']):
                    submit = next(option for option in now['options'] if 'submit' in option['label'].lower())
                    answer(now['stamp'], submit['key'])
                    facts['question']['review_step'] = True
                    asked = now
                time.sleep(0.5)
            final = settled()
            replies = [item['text'] for item in final['items'] if item['kind'] == 'assistant' and item.get('text')]
            facts['reply'] = replies[-1][:80]
            assert 'green' in replies[-1].lower(), replies[-1]
            assert final['question'] is None
            success = True
            print(json.dumps({'result': 'PASS', 'scenario': 'answers', 'model_calls': 3, **facts}, indent=1))
            return

        # One turn: a tool call, then output long enough to stop in the middle.
        status, sent = site.request('POST', chat + '/send', {
            'text': 'Read the file note.txt with the Read tool. Then count from 1 to 400, one number per line, and nothing else.'})
        assert status == 200, sent
        done_tool = wait(lambda: (lambda data: data if any(
            item['kind'] == 'tool' and item['tool']['status'] == 'ok' for item in data.get('items', [])) else None)(timeline()),
            'a finished tool call in the timeline', timeout=90)
        tool = next(item for item in done_tool['items'] if item['kind'] == 'tool')
        facts['tool'] = tool['tool']
        assert tool['tool']['name'].lower() == 'read' and 'note.txt' in tool['tool']['title'], tool
        assert 'heron' not in json.dumps([item for item in done_tool['items'] if item['kind'] == 'tool']), 'tool output was sent'
        assert done_tool['working'], 'the agent is not reported as working during its turn'
        facts['kinds'] = sorted({item['kind'] for item in done_tool['items']})
        facts['working'] = done_tool['working']['activity']
        assert done_tool['items'][0]['kind'] == 'user' and done_tool['items'][0]['text'].startswith('Read the file'), done_tool['items'][0]

        # Stop: the turn ends, the agent is ready again, and the exchange does not stay in cancel_requested.
        time.sleep(1.5)
        status, cancelled = site.request('POST', chat + '/cancel', {})
        assert status == 200, cancelled
        stopped = time.monotonic()
        wait(lambda: agent_state() != 'working', 'the agent to stop working after Stop', timeout=20)
        facts['stop_seconds'] = round(time.monotonic() - stopped, 1)
        final = wait(lambda: (lambda data: data if data['exchange']['state'] != 'cancel_requested' else None)(timeline()),
                     'the exchange to leave cancel_requested', timeout=10)
        facts['exchange'] = final['exchange']['state']
        facts['agent'] = agent_state()
        assert final['working'] is None and final['exchange']['state'] == 'cancelled', final['exchange']
        assert not any('interrupted by user' in (item['text'] or '') for item in final['items']), final['items'][-2:]
        numbers = [item['text'] for item in final['items'] if item['kind'] == 'assistant' and item['text']]
        assert not any('400' in text.split() for text in numbers), 'the turn ran to its end; Stop did nothing'
        facts['last_items'] = [(item['kind'], (item['text'] or item['tool'] and item['tool']['title'] or '')[:60]) for item in final['items'][-4:]]
        # The agent takes the next message: it is ready, not stuck in a dialog.
        assert 'again to exit' not in screen(pane), 'Stop left the agent one key away from quitting'
        success = True
    finally:
        if 'pane' in locals() and not success:
            print('agent pane: ' + json.dumps({key: value for key, value in next(p for p in call('snapshot')['data']['panes'] if p['id'] == pane).items() if key in ('title', 'agent', 'exited')}, ensure_ascii=False), file=sys.stderr)
            try:
                print('timeline: ' + json.dumps({key: timeline().get(key) for key in ('exchange', 'queue', 'working', 'question')})[:1500], file=sys.stderr)
            except Exception as error:
                print('timeline unavailable: %r' % error, file=sys.stderr)
            print('agent screen:\n' + '\n'.join(line.rstrip() for line in [screen(pane)[i:i + 120] for i in range(0, 4800, 120)] if line.strip()), file=sys.stderr)
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
        if success:
            shutil.rmtree(work)
        else:
            print('diagnostics retained at ' + str(work), file=sys.stderr)
    print(json.dumps({'result': 'PASS', 'model_calls': 1, **facts}, indent=1))


if __name__ == '__main__':
    main()
