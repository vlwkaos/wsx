#!/usr/bin/env python3
"""Real shell/Hermes removed-reporter recovery and refusal, the Claude hook's prompt receipts,
with private cleanup."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
work = ROOT / '.work' / ('reporter-hooks-' + str(os.getpid()))
work.mkdir(parents=True, mode=0o700, exist_ok=False)
success = False
try:
    log = work / 'calls'
    binary = work / 'current-wsx'
    binary.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$WSX_TEST_REPORT_LOG"\nexit "${WSX_TEST_REPORT_EXIT:-0}"\n')
    binary.chmod(0o755)
    stable = work / 'wsx.reporter'
    stable.symlink_to(binary)
    missing = work / 'removed-0.26.2/wsx'
    env = dict(os.environ, WSX_PANE_ID='42', WSX_RUNTIME_GENERATION='fixture-generation',
               WSX_SOCKET=str(work / 'wsx.sock'), WSX_AGENT_REPORT_BIN=str(missing),
               WSX_TEST_REPORT_LOG=str(log), PATH='/usr/bin:/bin', PYTHONDONTWRITEBYTECODE='1')
    source = (ROOT / 'crates/wsx-core/integrations/common/wsx-agent-status.sh').read_text()
    shell = work / 'hook.sh'
    shell.write_text(source.replace('@PROVIDER@','claude').replace('@LIFECYCLE@','yes').replace('@VERSION@','19'))
    shutil.copyfile(ROOT / 'crates/wsx-core/integrations/common/wsx-reporter.py', work / 'wsx-reporter.py')
    module_path = ROOT / 'crates/wsx-core/integrations/hermes/__init__.py'
    def invoke(actor, extra=None):
        selected = dict(env, **(extra or {}))
        if actor == 'shell':
            return subprocess.run(['/bin/sh',str(shell),'idle'],input='{}',text=True,
                                  env=selected,capture_output=True,timeout=3)
        # Run the actual provider hook in a separate process with its effective environment.
        runner = work / 'hermes-probe.py'
        runner.write_text('import importlib.util\nspec=importlib.util.spec_from_file_location("wsx_hook",' + repr(str(module_path)) + ')\nmodule=importlib.util.module_from_spec(spec)\nspec.loader.exec_module(module)\nmodule._report(session_id="fixture")\n')
        return subprocess.run(['/usr/bin/env','python3',str(runner)],env=selected,capture_output=True,text=True,timeout=3)
    def calls():
        return log.read_text().splitlines() if log.exists() else []
    for actor in ('shell','hermes'):
        before = len(calls())
        recovered = invoke(actor)
        assert recovered.returncode == 0, recovered.stderr
        assert len(calls()) == before + 1, actor + ' failed to recover removed reporter'
        rejected = invoke(actor, {'WSX_AGENT_REPORT_BIN':str(binary), 'WSX_TEST_REPORT_EXIT':'1'})
        assert len(calls()) == before + 2, actor + ' replayed rejected delivery'
        if actor == 'shell':
            assert rejected.returncode == 1 and 'report_failed' in rejected.stderr
        work.chmod(0o755)
        invoke(actor)
        assert len(calls()) == before + 2, actor + ' executed a fallback in an unsafe directory'
        work.chmod(0o700)
        binary.chmod(0o777)
        invoke(actor)
        assert len(calls()) == before + 2, actor + ' executed a writable fallback'
        binary.chmod(0o755)
    stable.unlink()
    for actor in ('shell','hermes'):
        before = len(calls())
        invoke(actor)
        assert len(calls()) == before, actor + ' invented a reporter when unavailable'
    installed = work / 'installed'
    installed.mkdir(mode=0o755)
    (installed / 'wsx').symlink_to(binary)
    selected_path = str(installed) + ':/usr/bin:/bin'
    for actor in ('shell', 'hermes'):
        before = len(calls())
        assert invoke(actor, {'PATH': selected_path}).returncode == 0
        assert len(calls()) == before + 1, actor + ' did not recover before daemon handoff'
        assert invoke(actor, {'PATH': selected_path, 'WSX_AGENT_REPORT_BIN': str(stable)}).returncode == 0
        assert len(calls()) == before + 2, actor + ' did not recover a removed stable entry'
        stable.symlink_to(work / 'removed-keg/wsx')
        invoke(actor, {'PATH': selected_path})
        assert len(calls()) == before + 3, actor + ' did not recover a dangling stable entry'
        stable.unlink()
        stable.write_text('invalid entry')
        invoke(actor, {'PATH': selected_path})
        assert len(calls()) == before + 3, actor + ' bypassed an invalid stable entry'
        stable.unlink()
        binary.chmod(0o777)
        invoke(actor, {'PATH': selected_path})
        assert len(calls()) == before + 3, actor + ' executed a writable installed reporter'
        binary.chmod(0o755)
    # docs/daemon-discovery.md: an empty WSX_SOCKET and a relative XDG_STATE_HOME are ignored,
    # as runtime::state_dir ignores them, so recovery finds the HOME state folder's entry.
    home_state = work / 'home' / '.local' / 'state' / 'wsx'
    home_state.mkdir(parents=True)
    home_state.chmod(0o700)
    (home_state / 'wsx.reporter').symlink_to(binary)
    for actor in ('shell', 'hermes'):
        before = len(calls())
        invoke(actor, {'WSX_SOCKET': '', 'XDG_STATE_HOME': 'relative-state', 'HOME': str(work / 'home')})
        assert len(calls()) == before + 1, actor + ' did not ignore a relative XDG_STATE_HOME'
    # The Claude hook names the prompt a turn took: a wsx round by its exchange line, also
    # inside Claude's paste wrapper, else another prompt (docs/agent-orchestration.md "Taken, not typed").
    def prompt_report(prompt, event='UserPromptSubmit', **fields):
        before = len(calls())
        hook_input = json.dumps({'session_id': 'fixture', 'hook_event_name': event, 'prompt': prompt, **fields})
        done = subprocess.run(['/bin/sh', str(shell), 'working'], input=hook_input, text=True,
                              env=dict(env, WSX_AGENT_REPORT_BIN=str(binary)), capture_output=True, timeout=5)
        assert done.returncode == 0 and len(calls()) == before + 1, done.stderr
        return calls()[-1]
    for prompt, expected in (('[wsx exchange 7, round 2, writer]\nhello', '--received 7:2'),
                             ('<pasted_content id="4666">\n[wsx exchange 9, round 1, read-only]\nhi\n</pasted_content id="4666">', '--received 9:1'),
                             ('typed by a person', '--received other')):
        line = prompt_report(prompt)
        assert expected in line and '--prompt-receipts' in line, (prompt, line)
    assert '--received' not in prompt_report('[wsx exchange 7, round 2, writer]\nhello', event='PostToolUse')
    # A /compact a person types is reported; an automatic one is not, so an idle agent never looks busy.
    assert '--compacting' in prompt_report('', event='PreCompact', trigger='manual')
    before = len(calls())
    subprocess.run(['/bin/sh', str(shell), 'working'], text=True, capture_output=True, timeout=5, check=True,
                   input=json.dumps({'session_id': 'fixture', 'hook_event_name': 'PreCompact', 'trigger': 'auto'}),
                   env=dict(env, WSX_AGENT_REPORT_BIN=str(binary)))
    assert len(calls()) == before, 'an automatic compaction was reported'
    success = True
    print('shell/Hermes reporter: pre-handoff and dangling-entry recovery, no rejection replay, unsafe-path refusal, bounded absence; '
          'Claude prompt receipts by round, in a paste wrapper and for another prompt; manual compaction only PASS')
finally:
    if success:
        shutil.rmtree(work)
    else:
        print('Reporter hook diagnostics retained at ' + str(work))
