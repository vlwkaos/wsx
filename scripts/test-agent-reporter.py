#!/usr/bin/env python3
"""Real shell/Hermes removed-reporter recovery and refusal, with private cleanup."""
import importlib.util
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
    binary.write_text('#!/bin/sh\nprintf "agent\\n" >> "$WSX_TEST_REPORT_LOG"\nexit "${WSX_TEST_REPORT_EXIT:-0}"\n')
    binary.chmod(0o755)
    stable = work / 'wsx.reporter'
    stable.symlink_to(binary)
    missing = work / 'removed-0.26.2/wsx'
    env = dict(os.environ, WSX_PANE_ID='42', WSX_RUNTIME_GENERATION='fixture-generation',
               WSX_SOCKET=str(work / 'wsx.sock'), WSX_AGENT_REPORT_BIN=str(missing),
               WSX_TEST_REPORT_LOG=str(log))
    source = (ROOT / 'crates/wsx-core/integrations/common/wsx-agent-status.sh').read_text()
    shell = work / 'hook.sh'
    shell.write_text(source.replace('@PROVIDER@','claude').replace('@LIFECYCLE@','yes').replace('@VERSION@','18'))
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
    success = True
    print('shell/Hermes reporter: removed path recovery, no rejection replay, unsafe-path refusal, bounded absence PASS')
finally:
    if success:
        shutil.rmtree(work)
    else:
        print('Reporter hook diagnostics retained at ' + str(work))
