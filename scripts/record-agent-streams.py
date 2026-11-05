#!/usr/bin/env python3
"""Record the real streams of the headless agent CLIs the task adapters read, scrubbed, as test
fixtures: crates/wsx/tests/fixtures/agent-streams/<adapter>/<scenario>.jsonl and a README.md.

  python3 -B scripts/record-agent-streams.py [claude|pi] [scenario ...]

Each scenario is one short turn on the smallest model, run in an empty temp folder with the
adapter's own stream flags (crates/wsx/src/task/claude.rs claude_args, pi.rs pi_args). It costs
cents. Ids, paths, the user name and the account's rate-limit numbers are replaced; field names,
types and order stay. Review `git diff` after a run, then fix the asserts in
crates/wsx/src/task/stream_tests.rs. ref: docs/tasks.md "Agent stream recordings"
"""
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / 'crates/wsx/tests/fixtures/agent-streams'
CLAUDE_MODEL = 'haiku'
PI_MODEL = 'openai-codex/gpt-5.6-luna'

# name -> (what it covers, prompt, extra arguments, extra environment)
CLAUDE = {
    'plain': ('a plain text turn', 'Reply with exactly: Hello from the fixture.', [], {}),
    'text-tool-text': (
        'text, a Bash tool use, then more text (the result holds only the last block)',
        'Say the word one. Then run the shell command `echo done` with the Bash tool. '
        'Then say the word two. Say nothing else.', [], {}),
    'quiet': ('a quiet turn: the whole reply is -', 'Reply with only a single hyphen character: -', [], {}),
    'subagent': (
        'a subagent: its lines carry parent_tool_use_id',
        'Start one subagent with the Task tool whose prompt is: reply with the word ok. '
        'When it returns, say finished.', [], {}),
    'structured': ('a worker turn: --json-schema gives the result a structured_output',
                   'Return status done.',
                   ['--json-schema', json.dumps({'type': 'object', 'properties': {'status': {'type': 'string'}},
                                                  'required': ['status']})], {}),
    'error': ('a turn that ends in an error result (--max-turns 1 with a tool request)',
              'Run the shell command `echo hi` with the Bash tool.', ['--max-turns', '1'], {}),
}
PI = {
    'plain': ('a plain text turn', 'Reply with exactly: Hello from the fixture.', [], {}),
    'text-tool-text': (
        'text, a bash tool call, then more text in one run',
        'Say the word one. Then run the shell command `echo done` with the bash tool. '
        'Then say the word two. Say nothing else.', [], {}),
    'quiet': ('a quiet turn: the whole reply is -', 'Reply with only a single hyphen character: -', [], {}),
    # ^ The Anthropic account had no credit when this was recorded: a refusal of the kind
    # pi.rs `refusal` reads. On a refresh any provider error does.
    'error': ('a provider refusal (an Anthropic account without credit)', 'Reply with the word hi.',
              ['--model', 'anthropic/claude-haiku-5-5'], {}),
}
# Pi has no subagent of its own; an extension would add one.

LONG_LISTS = {'tools', 'slash_commands', 'terminal_slash_commands', 'skills', 'agents'}
ID = re.compile(r'^(?:(msg|req|toolu|srvtoolu|call)_[A-Za-z0-9]+|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$')


class Scrub:
    def __init__(self, folder):
        home = str(Path.home())
        self.texts = [(str(folder.resolve()), '/work'), (str(folder), '/work'), (str(folder.resolve()).removeprefix('/private'), '/work'),
                      (home, '/home/user'), (os.environ.get('USER', '\0'), 'user')]
        self.ids = {}

    def string(self, text):
        match = ID.match(text)
        if match:
            prefix = match.group(1) or 'uuid'
            known = self.ids.setdefault(text, len(self.ids) + 1)
            return ('00000000-0000-4000-8000-%012d' % known) if prefix == 'uuid' else '%s_%d' % (prefix, known)
        for real, fake in self.texts:
            text = text.replace(real, fake)
        text = re.sub(r'-private-var-folders-[^/"]*-wsx-record-[^/"]*', '-work', text)
        # ^ Claude's per-user task dir holds the uid and the temp folder's name.
        text = re.sub(r'/private/tmp/claude-\d+/[^/]+/[^/]+/tasks/', '/tmp/claude/tasks/', text)
        return re.sub(r'/tmp/cc-socks/\d+\.sock', '/tmp/cc-socks/1.sock', text)

    def value(self, value, key=''):
        if isinstance(value, dict):
            # ^ Pi lists its system prompt's sections: keep the names, not the prompt.
            if key == 'sections':
                return {name: '...' for name in value}
            return {name: self.value(item, name) for name, item in value.items()}
        if isinstance(value, list):
            items = value[:2] if key in LONG_LISTS else value
            return [self.value(item, key) for item in items]
        if isinstance(value, str):
            if key == 'signature':
                return '...'
            return self.string(value)
        if isinstance(value, float) and key in ('utilization',):
            return 0.1
        if isinstance(value, int) and key in ('resetsAt', 'timestamp') and value > 10**9:
            return 1793794200 * (1000 if value > 10**11 else 1)
        return value


def scrub_all(lines, folder):
    scrub = Scrub(folder)
    out = []
    for line in lines:
        value = scrub.value(json.loads(line))
        out.append(json.dumps(value, ensure_ascii=False, separators=(',', ':')))
    text = '\n'.join(out) + '\n'
    for secret in (str(Path.home()), os.environ.get('USER', '\0')):
        assert secret not in text, 'unscrubbed %r' % secret
    return text


def run(argv, first, env, folder, until):
    """Start the CLI, send the first command, collect stdout lines up to the one `until` accepts."""
    signal.alarm(240)
    child = subprocess.Popen(argv, cwd=folder, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True)
    child.stdin.write(json.dumps(first) + '\n')
    child.stdin.flush()
    lines = []
    for line in child.stdout:
        lines.append(line.rstrip('\n'))
        if until(json.loads(line)):
            break
    child.stdin.close()
    try:
        child.wait(timeout=20)
    except subprocess.TimeoutExpired:
        child.kill()
    signal.alarm(0)
    return lines


def environment(extra):
    keep = ('HOME', 'USER', 'LOGNAME', 'PATH', 'SHELL', 'LANG', 'TMPDIR', 'PI_CODING_AGENT_DIR', 'CLAUDE_CONFIG_DIR')
    return {**{name: os.environ[name] for name in keep if name in os.environ}, **extra}


def record_claude(prompt, extra, env, folder):
    argv = ['claude', '-p', '--input-format', 'stream-json', '--output-format', 'stream-json', '--verbose',
            '--setting-sources', 'project,local', '--permission-mode', 'bypassPermissions',
            '--model', CLAUDE_MODEL, '--session-id', str(uuid.uuid4()), *extra]
    first = {'type': 'user', 'message': {'role': 'user', 'content': prompt}}
    return run(argv, first, environment(env), folder, lambda value: value.get('type') == 'result')


def record_pi(prompt, extra, env, folder):
    argv = ['pi', '--mode', 'rpc', '--model', PI_MODEL, '--no-extensions', '--no-mcp', '--no-prompt-templates',
            '--no-themes', '--no-context-files', '--no-skills', '--no-approve', '--no-session',
            '--thinking', 'off', *extra]  # ^ a later --model wins
    first = {'type': 'prompt', 'id': 'wsx-1', 'message': prompt}
    return run(argv, first, environment(env), folder, lambda value: value.get('type') == 'agent_settled')


def version(command):
    return subprocess.run([command, '--version'], capture_output=True, text=True).stdout.strip().splitlines()[0]


def main():
    wanted = sys.argv[1:]
    adapters = {'claude': (CLAUDE, record_claude), 'pi': (PI, record_pi)}
    chosen = [name for name in adapters if name in wanted] or list(adapters)
    scenarios = [name for name in wanted if name not in adapters]
    for adapter in chosen:
        table, record = adapters[adapter]
        folder = OUT / adapter
        folder.mkdir(parents=True, exist_ok=True)
        for name, (_, prompt, extra, env) in table.items():
            if scenarios and name not in scenarios:
                continue
            work = Path(tempfile.mkdtemp(prefix='wsx-record-'))
            try:
                lines = record(prompt, extra, env, work)
                (folder / (name + '.jsonl')).write_text(scrub_all(lines, work))
                print(adapter, name, len(lines), 'lines')
            finally:
                shutil.rmtree(work, ignore_errors=True)
    readme()


def readme():
    rows = ['# Agent stream recordings', '',
            'Real output of the headless agent CLIs that the task adapters read, one turn per file.',
            'Made by `python3 -B scripts/record-agent-streams.py` (docs/tasks.md "Agent stream recordings"); '
            'ids, paths and rate-limit numbers are scrubbed, field names, types and order are not.',
            'Used by `crates/wsx/src/task/stream_tests.rs` (the adapters\' parse path) and '
            '`crates/wsx/tests/agent_stream_fakes.rs` (the fakes\' shapes).', '']
    for adapter, table, command in (('claude', CLAUDE, 'claude'), ('pi', PI, 'pi')):
        stream = ('`claude -p --input-format stream-json --output-format stream-json --verbose --model %s`' % CLAUDE_MODEL
                  if adapter == 'claude' else '`pi --mode rpc --model %s --thinking off --no-session`' % PI_MODEL)
        rows += ['## %s' % adapter, '', 'Recorded with %s %s: %s' % (command, version(command), stream), '']
        rows += ['- `%s.jsonl`: %s' % (name, what) for name, (what, *_rest) in table.items()]
        if adapter == 'pi':
            rows.append('- No subagent recording: Pi has no subagent of its own.')
        rows.append('')
    (OUT / 'README.md').write_text('\n'.join(rows))


if __name__ == '__main__':
    main()
