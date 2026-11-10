#!/usr/bin/env python3
"""Manual proof: the REAL Pi with the REAL lean runner (pygmalion `runner/index.ts`), launched by wsx.

An isolated wsxd runs two untrusted Pi workers inside wsx's Seatbelt boundary. Their provider is a
scripted OpenAI chat-completions server on loopback in this script (no key, no model). Task 1 asks
for a write outside its working copy (the runner must deny it), a write inside (must succeed), a
shell write outside (the profile must refuse it, bash is not path-gated) and reports its result.
Task 2 runs on a policy file whose bytes were changed after wsx wrote it (hash mismatch): the
runner must deny every call. The evidence is the task log (`runner denied ...` progress lines), the
files on disk and the tool results the scripted provider got back. The report goes to
`.work/runner-proof/report.json`.

Needs Pi (`pi`, run with `node`), the pygmalion runner and no command sandbox around this script
(Seatbelt cannot start inside another). Build adjacent wsx and wsxd first.
Usage: runner-proof.py [--pi /opt/homebrew/bin/pi] [--runner <pygmalion>/runner/index.ts] [--keep]
ref: docs/tasks.md "Pi adapter"; pygmalion runner/README.md
"""
import argparse
import collections
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_RUNNER = Path.home() / 'ws/tui_cli/pygmalion/runner/index.ts'
CONFIG = 'tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  checks:\n    - "test ! -e broken.txt"\n'
# ^ Run by the agent in place of `pi`: keeps what wsx wrote (path, hash, file), changes the file of the
# `tamper` model, then starts the real Pi with the same arguments and environment.
WRAPPER = '''#!/bin/sh
model=
prev=
for arg in "$@"; do
  [ "$prev" = --model ] && model=$arg
  prev=$arg
done
tag=$(echo "$model" | tr '/' '_')
mkdir -p "%(proof)s"
cp "$PYGMALION_POLICY" "%(proof)s/policy-$tag.json"
echo "$PYGMALION_POLICY_SHA256" > "%(proof)s/sha-$tag.txt"
stat -f %%Lp "$PYGMALION_POLICY" > "%(proof)s/mode-$tag.txt"
case "$model" in */tamper) printf ' ' >> "$PYGMALION_POLICY" ;; esac
exec "%(pi)s" "$@"
'''


class Provider(http.server.ThreadingHTTPServer):
    """A scripted model: each request of a model id gets the next turn of that model's script, as the
    hostile or confused small model would answer. A turn is {'text'} or {'tools': [(name, arguments)]}."""

    daemon_threads = True

    def __init__(self, scripts):
        super().__init__(('127.0.0.1', 0), Provider.Handler)
        self.scripts, self.next, self.requests = scripts, collections.Counter(), []
        self.lock = threading.Lock()
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def base_url(self):
        return 'http://127.0.0.1:%d/v1' % self.server_address[1]

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get('content-length', 0))) or b'{}')
            model = body.get('model', '')
            with self.server.lock:
                index = self.server.next[model]
                self.server.next[model] += 1
                script = self.server.scripts.get(model, [])
                turn = script[index] if index < len(script) else {'text': 'done'}
                results = [m.get('content') for m in body.get('messages', []) if m.get('role') == 'tool']
                self.server.requests.append({'model': model, 'turn': index, 'tool_results': results})
            self.send_response(200)
            self.send_header('content-type', 'text/event-stream')
            self.end_headers()

            def chunk(delta, finish=None, usage=None):
                data = {'id': 'chatcmpl-%d' % index, 'object': 'chat.completion.chunk', 'created': 0, 'model': model,
                        'choices': [] if usage else [{'index': 0, 'delta': delta, 'finish_reason': finish}]}
                if usage:
                    data['usage'] = usage
                self.wfile.write(('data: %s\n\n' % json.dumps(data)).encode())

            tools = turn.get('tools', [])
            chunk({'role': 'assistant', 'content': ''})
            if turn.get('text'):
                chunk({'content': turn['text']})
            for position, (name, arguments) in enumerate(tools):
                chunk({'tool_calls': [{'index': position, 'id': 'call_%s_%d_%d' % (model, index, position), 'type': 'function',
                                       'function': {'name': name, 'arguments': json.dumps(arguments)}}]})
            chunk({}, 'tool_calls' if tools else 'stop')
            chunk({}, usage={'prompt_tokens': 10, 'completion_tokens': 5, 'total_tokens': 15})
            self.wfile.write(b'data: [DONE]\n\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--pi', type=Path, default=Path(shutil.which('pi') or '/opt/homebrew/bin/pi'))
    parser.add_argument('--runner', type=Path, default=DEFAULT_RUNNER)
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    args = parser.parse_args()
    wsx = args.wsx.resolve()
    wsxd = wsx.with_name('wsxd')
    for needed in (wsx, wsxd, args.pi, args.runner):
        if not needed.exists():
            raise RuntimeError('%s is missing' % needed)
    node = shutil.which('node') or '/opt/homebrew/bin/node'
    if not Path(node).exists():
        raise RuntimeError('node is not on PATH (Pi needs it)')
    work = ROOT / '.work' / ('rp' + str(os.getpid()))
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    owned = {}
    try:
        report = run(wsx, wsxd, args.pi.resolve(), args.runner.resolve(), Path(node).resolve().parent, work, owned)
        out = ROOT / '.work/runner-proof'
        out.mkdir(parents=True, exist_ok=True)
        (out / 'report.json').write_text(json.dumps(report, indent=1))
        print(json.dumps(report, indent=1))
        print(json.dumps({'ok': True, 'report': str(out / 'report.json')}))
    finally:
        if 'env' in owned:
            subprocess.run([str(wsx), 'daemon', 'stop', '--force'], env=owned['env'], capture_output=True, timeout=20)
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)


def run(wsx, wsxd, pi, runner, node_dir, work, owned):
    home, state, project, bin_dir, tmp = (work / name for name in ('h', 's', 'p', 'b', 't'))
    for path in (home, state, project, bin_dir, tmp):
        path.mkdir(mode=0o700)
    proof = tmp / 'proof'
    config_dir = home / 'Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'c/wsx'
    config_dir.mkdir(parents=True)
    outside = project / 'outside-proof.txt'
    outside_bash = project / 'outside-bash.txt'
    scripts = {
        'main': [
            {'tools': [('write', {'path': str(outside), 'content': 'past the runner\n'})]},
            {'tools': [('write', {'path': 'inside.txt', 'content': 'inside the copy\n'})]},
            {'tools': [('bash', {'command': 'echo past the profile > %s; echo exit=$?' % outside_bash})]},
            {'tools': [('bash', {'command': (
                'git add inside.txt && git -c user.name=proof -c user.email=proof@example.invalid commit -qm inside '
                '&& echo \'{"status":"done","summary":"runner proof","memo":""}\' | wsx task result -')})]},
            {'text': 'done'},
        ],
        'tamper': [
            {'tools': [('write', {'path': 'inside.txt', 'content': 'inside the copy\n'})]},
            {'tools': [('bash', {'command': 'echo hello'})]},
            {'text': 'done'},
        ],
    }
    provider = Provider(scripts)
    pi_dir = home / '.pi/agent'
    pi_dir.mkdir(parents=True)
    (pi_dir / 'models.json').write_text(json.dumps({'providers': {'scripted': {
        'baseUrl': provider.base_url, 'api': 'openai-completions', 'apiKey': 'none',
        'models': [{'id': name, 'name': name, 'reasoning': False, 'input': ['text'], 'contextWindow': 32768,
                    'maxTokens': 2048, 'compat': {'supportsStore': False, 'maxTokensField': 'max_tokens'}}
                   for name in scripts]}}}))
    registry = 'resume_agents_on_restore = false\n[[projects]]\nname = "demo"\npath = %s\n' % json.dumps(str(project))
    for name in scripts:
        registry += ('[[agents]]\nname = "proof-%s"\ncommand = ["pi"]\nadapter = "pi"\nmodel = "scripted/%s"\n'
                     'extension = %s\n' % (name, name, json.dumps(str(runner))))
    (config_dir / 'config-v2.toml').write_text(registry)
    (bin_dir / 'pi').write_text(WRAPPER % {'proof': proof, 'pi': pi})
    (bin_dir / 'pi').chmod(0o755)
    env = {'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(work / 'c'),
           'XDG_CACHE_HOME': str(work / 'k'), 'WSX_DAEMON_BIN': str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()),
           'WSX_SOCKET': str(work / 'd.sock'),
           'PATH': ':'.join([str(bin_dir), str(node_dir), '/opt/homebrew/bin', '/usr/bin', '/bin', '/usr/sbin', '/sbin']),
           'SHELL': '/bin/sh', 'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8', 'TMPDIR': str(tmp),
           'GIT_CEILING_DIRECTORIES': str(work)}
    owned['env'] = env

    def git(*argv, cwd=project):
        done = subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', *argv], cwd=cwd, env=env,
                              capture_output=True, text=True, timeout=20)
        assert done.returncode == 0, (argv, done.stderr)
        return done.stdout.strip()

    def js(*argv):
        done = subprocess.run([str(wsx), *map(str, argv), '--json'], cwd=project, env=env, capture_output=True,
                              text=True, timeout=60)
        assert done.returncode == 0, (argv, done.stdout, done.stderr)
        return json.loads(done.stdout)

    def wait(test, label, seconds=120):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = test()
            if value:
                return value
            time.sleep(0.3)
        raise RuntimeError('%s: timeout after %d s' % (label() if callable(label) else label, seconds))

    def log(n):
        path = Path(js('task', 'show', n)['log'])
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def said(n):
        return [entry['text'] for entry in log(n) if entry.get('kind') == 'agent' and not entry.get('said')]

    def new_task(model, prompt):
        (project / 'wsx.config.yml').write_text(CONFIG + '  agent: { adapter: pi, model: scripted/%s }\n' % model)
        git('add', '.')
        git('commit', '-q', '-m', 'configure ' + model)
        return js('task', 'new', prompt)['id']

    def state_of(n):
        return js('task', 'show', n)['state']

    def why(n):
        return {'state': js('task', 'show', n), 'progress': said(n), 'provider': provider.requests}

    git('init', '-q', '-b', 'main')
    (project / '.gitignore').write_text('target/\n')
    (project / 'README').write_text('fixture\n')
    (project / 'wsx.config.yml').write_text(CONFIG)
    git('add', '.')
    git('commit', '-q', '-m', 'start')

    commit = subprocess.run(['git', '-C', str(runner.parent), 'rev-parse', '--short', 'HEAD'], capture_output=True, text=True).stdout.strip()
    report = {'pi': str(pi), 'runner': str(runner), 'runner_commit': commit, 'pi_version': subprocess.run(
        [str(pi), '--version'], env=env, capture_output=True, text=True).stdout.strip()}

    # 1. A worker whose policy file is as wsx wrote it.
    main = new_task('main', 'runner proof: write inside and outside the working copy')
    wait(lambda: state_of(main) in ('ready', 'failed'), lambda: json.dumps(why(main), default=str)[:6000])
    shown = js('task', 'show', main)
    assert shown['state'] == 'ready', json.dumps(why(main), default=str)[:6000]
    copy = Path(shown['record']['working_copy'])
    policy_file = proof / 'policy-scripted_main.json'
    raw = policy_file.read_bytes()
    policy = json.loads(raw)
    logged = said(main)
    denied = [line for line in logged if line.startswith('runner denied ')]
    results = [r for r in provider.requests if r['model'] == 'main']
    report['main'] = {
        'task': main, 'state': shown['state'], 'working_copy': str(copy),
        'policy': policy, 'policy_sha256_matches_env': hashlib.sha256(raw).hexdigest() == (proof / 'sha-scripted_main.txt').read_text().strip(),
        'policy_file_mode': (proof / 'mode-scripted_main.txt').read_text().strip(),
        'log_runner_lines': denied + [line for line in logged if line.startswith('runner policy:')],
        'log_all_progress': logged,
        'tool_results_seen_by_the_provider': [r['tool_results'][-1] if r['tool_results'] else None for r in results],
        'inside_file': (copy / 'inside.txt').read_text() if (copy / 'inside.txt').exists() else None,
        'inside_committed': git('log', '--format=%s', '-3', cwd=copy),
        'outside_write_exists': outside.exists(), 'outside_bash_exists': outside_bash.exists(),
    }
    assert policy['tools'] == ['read', 'bash', 'edit', 'write'] and policy['read'] == ['/'], policy
    assert os.path.realpath(copy) in policy['write'], (copy, policy['write'])
    assert report['main']['policy_sha256_matches_env'] and report['main']['policy_file_mode'] == '600', report['main']
    assert len(denied) == 1 and 'outside-proof.txt' in denied[0] and denied[0].startswith('runner denied write '), logged
    assert not outside.exists() and not outside_bash.exists(), 'a write outside the working copy worked'
    assert report['main']['inside_file'] == 'inside the copy\n' and 'inside' in report['main']['inside_committed'], report['main']
    seen = report['main']['tool_results_seen_by_the_provider']
    assert 'write refused' in str(seen[1]) and 'refused' not in str(seen[2]), seen
    js('task', 'close', main, '--discard')

    # 2. A worker whose policy file changed after wsx wrote it: the runner denies every call.
    tamper = new_task('tamper', 'runner proof: the policy file no longer matches its hash')
    wait(lambda: sum(line.startswith('runner denied ') for line in said(tamper)) >= 2, lambda: json.dumps(why(tamper), default=str)[:6000])
    logged = said(tamper)
    shown = js('task', 'show', tamper)
    copy = Path(shown['record']['working_copy'])
    results = [r for r in provider.requests if r['model'] == 'tamper']
    report['tamper'] = {
        'task': tamper, 'working_copy': str(copy),
        'log_runner_lines': [line for line in logged if line.startswith('runner ')],
        'tool_results_seen_by_the_provider': [r['tool_results'][-1] if r['tool_results'] else None for r in results],
        'inside_file_exists': (copy / 'inside.txt').exists(),
    }
    assert any(line.startswith('runner policy: ') and 'does not match PYGMALION_POLICY_SHA256' in line for line in logged), logged
    assert any(line.startswith('runner denied write ') for line in logged) and any(line.startswith('runner denied bash') for line in logged), logged
    assert not (copy / 'inside.txt').exists(), 'the tampered runner let a write through'
    js('task', 'close', tamper, '--discard')
    return report


if __name__ == '__main__':
    main()
