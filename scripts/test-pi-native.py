#!/usr/bin/env python3
"""Real broker/PTY -> Pi SDK -> guarded receipts, with an offline deterministic provider.

The Goal Run service uses the actual Pygmalion registration API. Its state is a
controlled fixture, not proof of Task finalization or a real-model orchestration trial.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sdk-root', type=Path, required=True)
    parser.add_argument('--goal-module', type=Path, required=True)
    args = parser.parse_args()
    sdk, goal_module = args.sdk_root.resolve(), args.goal_module.resolve()
    wsx, wsxd = ROOT / 'target/debug/wsx', ROOT / 'target/debug/wsxd'
    node = shutil.which('node')
    assert node and wsx.is_file() and wsxd.is_file() and goal_module.is_file()
    work = ROOT / '.work' / ('pn-' + str(os.getpid()))
    work.mkdir(mode=0o700)
    daemon, success, results = None, False, []
    start = time.monotonic()
    try:
        for name in ('h', 's', 'p'):
            (work / name).mkdir(mode=0o700)
        config = work / ('h/Library/Application Support/wsx' if sys.platform == 'darwin' else 'c/wsx')
        config.mkdir(parents=True)
        (config / 'config-v2.toml').write_text('resume_agents_on_restore = false\n')
        address = work / 's/wsx/wsx.sock'
        env = {'HOME': str(work / 'h'), 'XDG_CONFIG_HOME': str(work / 'c'), 'XDG_STATE_HOME': str(work / 's'),
            'XDG_CACHE_HOME': str(work / 'cache'), 'WSX_SOCKET': str(address), 'WSX_DAEMON_BIN': str(wsxd),
            'WSX_REPORT_BIN': str(wsx), 'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'SHELL': '/bin/sh',
            'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8'}
        subprocess.run(['git', 'init', '-q', '-b', 'main', str(work / 'p')], env=env, check=True, timeout=5)
        def call(method, params=None):
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(5)
                conn.connect(str(address))
                stream = conn.makefile('rb')
                conn.sendall(b'{"method":"hello","params":{"protocol":16}}\n')
                assert json.loads(stream.readline())['type'] == 'hello'
                request = {'method': method}
                if params is not None:
                    request['params'] = params
                conn.sendall((json.dumps(request) + '\n').encode())
                return json.loads(stream.readline())
        def wait(predicate, label, seconds=8):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                value = predicate()
                if value:
                    return value
                time.sleep(0.025)
            raise AssertionError(label + ' timed out')
        def cli(*argv, ok=True):
            reply = subprocess.run([str(wsx), *map(str, argv)], env=env, cwd=work / 'p', capture_output=True, text=True, timeout=12)
            assert (reply.returncode == 0) == ok, reply.stderr or reply.stdout
            return reply
        log = (work / 'daemon.log').open('w')
        daemon = subprocess.Popen([str(wsxd)], env=env, stdout=log, stderr=log)
        wait(address.exists, 'private daemon')
        assert call('synchronize_projects', {'projects': [{'name': 'native-fixture', 'path': str(work / 'p'),
            'worktrees': [{'path': str(work / 'p'), 'branch': 'main'}]}]})['type'] == 'ack'
        worktree = call('snapshot')['data']['worktrees'][0]['id']
        for mode in ('no-goal', 'normal', 'task', 'paused', 'mismatch', 'abort', 'dispose', 'replacement', 'generation'):
            evidence = work / mode
            evidence.mkdir(mode=0o700)
            fixture_mode = 'paused' if mode in ('replacement', 'generation') else mode
            command = [node, str(ROOT / 'scripts/pi-native-fixture.mjs'), str(sdk),
                str(ROOT / 'crates/wsx-core/integrations/pi/wsx-agent-status.ts'), str(goal_module), str(evidence), fixture_mode]
            created = call('session_create', {'worktree_id': worktree, 'label': mode, 'command': command, 'rows': 12, 'cols': 100})
            assert created['type'] == 'created', created
            session_id = created['data']['id']
            pane_id = next(item['focused_pane'] for item in call('snapshot')['data']['sessions'] if item['id'] == session_id)
            target = str(pane_id)
            def state():
                file = evidence / 'sdk-state.json'
                try:
                    return json.loads(file.read_text()) if file.exists() else {}
                except json.JSONDecodeError:
                    return {}
            def exchange():
                return call('agent_exchange_get', {'exchange_id': request['id']})['data']['exchange']
            try:
                try:
                    wait(lambda: state().get('ready'), mode + ' SDK startup', 12)
                except AssertionError:
                    raise AssertionError(cli('session', 'peek', target, '--trim').stdout[-5000:])
                wait(lambda: next((item.get('agent') for item in call('snapshot')['data']['panes']
                    if item['id'] == pane_id), None), mode + ' daemon-observed adapter identity')
                packet = json.loads(cli('agent', 'context', pane_id, '--metadata-only', '--json').stdout)
                capability = packet['candidates'][0]['agent']['capabilities']
                assert capability['prompt'] == (mode != 'no-goal') and capability['exchange_receipts'] == (mode != 'no-goal'), packet
                if mode == 'no-goal':
                    cli('agent', 'request', pane_id, 'must not start without Goal owner', '--json', ok=False)
                    results.append({'case': mode, 'result': 'PASS', 'capability': False})
                    continue
                request = json.loads(cli('agent', 'request', pane_id, 'Native fixture decision with Unicode 한글', '--timeout', '8', '--json').stdout)['exchange']
                if mode == 'mismatch':
                    wait(lambda: state().get('settled') == 1, 'mismatch model settled')
                    time.sleep(0.65)
                    assert exchange().get('native_input_id') is None and exchange()['state'] != 'completed', exchange()
                else:
                    wait(lambda: exchange().get('native_input_id'), mode + ' request-bound acceptance')
                    if mode == 'normal':
                        wait(lambda: exchange()['state'] == 'completed', 'normal native result')
                    elif mode == 'task':
                        wait(lambda: state().get('settled') == 1, 'first Task turn')
                        cli('session', 'send-text', target, 'fixture:steer')
                        wait(lambda: state().get('steeringGoalActive'), 'steering reached the same active Goal')
                        time.sleep(0.55)
                        assert exchange()['state'] != 'completed', 'turn settlement falsely completed Task'
                        cli('agent', 'request', pane_id, 'overlapping intent must be refused', '--json', ok=False)
                        awaited = json.loads(cli('agent', 'wait', request['id'], '--timeout', '8', '--json').stdout)['exchange']
                        assert awaited['state'] == 'completed' and awaited['evidence'] == 'request_bound', awaited
                        observed = state()
                        # ^ Queued steering can add model turns inside one SDK run.
                        assert observed['turns'] >= 3 and observed['completedTurns'] == observed['turns'] and observed['settled'] >= 2, observed
                        assert awaited['updated_unix_ms'] >= observed['lastModelStopMs'], 'native result preceded the last model completion'
                    elif mode == 'paused':
                        wait(lambda: state().get('settled') == 1, 'paused Task turn')
                        time.sleep(0.65)
                        assert exchange()['state'] != 'completed', exchange()
                        cli('session', 'send-text', target, 'fixture:finish')
                        wait(lambda: exchange()['state'] == 'completed', 'explicit Task completion')
                    else:
                        if mode != 'abort':
                            wait(lambda: state().get('settled') == 1, mode + ' turn')
                        control = {'abort': 'abort', 'dispose': 'dispose', 'replacement': 'replacement', 'generation': 'generation'}[mode]
                        cli('session', 'send-text', target, 'fixture:' + control)
                        if mode in ('replacement', 'generation'):
                            cli('session', 'send-text', target, 'fixture:finish')
                        time.sleep(2.4 if mode == 'abort' else 0.85)
                        assert exchange()['state'] != 'completed', mode + ' accepted a stale completion'
                results.append({'case': mode, 'result': 'PASS', 'exchange': exchange(), 'sdk': state()})
            except BaseException:
                frame = cli('session', 'peek', target, '--trim')
                (evidence / 'failed-frame.txt').write_text(frame.stdout + frame.stderr)
                (evidence / 'failed-state.json').write_text(json.dumps(state(), indent=2) + '\n')
                raise
            finally:
                # ^ Cleanup uses the created SessionId, never a provider label or unsupported alias.
                cli('session', 'delete', session_id, '--json')
        snapshot = call('snapshot')['data']
        assert not snapshot['sessions'] and not snapshot['panes'], 'owned SDK sessions survived deletion'
        assert call('shutdown')['type'] == 'ack'
        daemon.wait(timeout=6)
        daemon = None
        log.close()
        assert not address.exists()
        success = True
        receipt = {'result': 'PASS', 'cases': results, 'seconds': round(time.monotonic() - start, 3),
            'scratch': str(work), 'cleanup': 'all owned panes closed; private daemon exited; socket absent',
            'limits': 'Actual Pi SDK/PTY/broker/guarded CLI; deterministic offline provider and controlled Goal state via real registration API. Zero real-model calls. Does not prove actual Task finalization or orchestration adoption.'}
        (work / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
        print(json.dumps(receipt, indent=2))
    finally:
        if daemon is not None:
            if daemon.poll() is None:
                daemon.terminate()
            daemon.wait(timeout=6)
        print(('PASS' if success else 'FAILED (retained)') + ': ' + str(work), file=sys.stderr)

if __name__ == '__main__':
    main()
