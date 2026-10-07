#!/usr/bin/env python3
"""Model-free CLI/native-history and real-PTY draft-stash orchestration journey."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
ACTOR = r'''
import json, os, pathlib, signal, socket, sys, time, tty
signal.signal(signal.SIGHUP, signal.SIG_IGN)
provider, history, trace = sys.argv[1], pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
pane = int(os.environ['WSX_PANE_ID']); generation = os.environ['WSX_RUNTIME_GENERATION']
trace.with_suffix('.pid').write_text(str(os.getpid()))
trace.with_suffix('.generation').write_text(generation)
tty.setraw(0)
draft, stash, pending = '\nunsent draft', '', b''
broken_stash_binding = False
pause_completion, pending_completion, receipt_capable = False, False, False
leaf, sequence, idle_chrome = 'answer', 0, True
def report(state):
 with socket.socket(socket.AF_UNIX) as client:
  client.connect(os.environ['WSX_SOCKET']); stream = client.makefile('rb')
  client.sendall(b'{"method":"hello","params":{"protocol":16}}\n'); stream.readline()
  params = {'pane_id':pane,'runtime_generation':generation,'provider':provider,'state':state,
    'session_ref':{'kind':'id','value':'fixture','transcript_path':str(history)},
    'capabilities':{'prompt':True,'lifecycle':True,'exchange_receipts':receipt_capable}}
  client.sendall(json.dumps({'method':'agent_report','params':params}).encode()+b'\n')
  assert json.loads(stream.readline())['type'] == 'ack'
def paint(fragmented=False):
 if fragmented:
  # Real terminal frames may arrive in separate PTY reads. The owner must let
  # its reader finish the frame while it waits for the empty-editor ACK.
  sys.stdout.write('\x1b[2J'); sys.stdout.flush(); time.sleep(0.05)
 rows = os.get_terminal_size().lines
 title = '✳ fixture' if idle_chrome else 'fixture'
 marker = '❯ ' if idle_chrome else 'editor hidden '
 sys.stdout.write('\x1b[?2004h\x1b]2;'+title+'\x07\x1b[2J\x1b['+str(max(1,rows-2))+';1H'+marker+draft+'\x1b[J')
 sys.stdout.flush()
report('idle'); paint()
while True:
 pending += os.read(0,4096)
 while pending:
  if pending.startswith(b'\x1b[200~'):
   end = pending.find(b'\x1b[201~')
   if end < 0: break
   draft += pending[6:end].decode(); pending = pending[end+6:]; paint(); continue
  if pending.startswith(b'\x1b') and len(pending) < 6: break
  char, pending = pending[:1], pending[1:]
  if char == b'\x12':
   pause_completion = not pause_completion
   if pending_completion and not pause_completion:
    pending_completion = False; report('done')
  elif char == b'\x14':
   receipt_capable = True; report('working' if pending_completion else 'idle')
  elif char == b'\x1e':
   idle_chrome = not idle_chrome; paint()
  elif char == b'\x11':
   broken_stash_binding = not broken_stash_binding
  elif char == b'\x13':
   # Mirrors the documented Claude prompt stash, including empty-input restore.
   if broken_stash_binding: continue
   if draft: stash, draft = draft, ''
   else: draft, stash = stash, ''
   paint(fragmented=True)
  elif char == b'\x03':
   raise RuntimeError('orchestration must not interrupt or discard this draft')
  elif char == b'\r':
   with trace.open('a') as out: out.write(json.dumps({'received':draft,'stash':stash})+'\n')
   if provider == 'claude':
    sequence += 1
    entry = 'submitted-' + str(sequence)
    with history.open('a') as out:
     out.write(json.dumps({'type':'user','uuid':entry,'parentUuid':leaf,'sessionId':'fixture',
       'message':{'content':draft}})+'\n')
    leaf = entry
   draft = ''; paint()
   if pause_completion:
    pending_completion = True; report('working')
   else: report('done')
  else:
   draft += char.decode(errors='replace'); paint()
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--daemon', type=Path, default=ROOT / 'target/debug/wsxd')
    args = parser.parse_args()
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    if not wsx.is_file() or not wsxd.is_file() or wsx.parent != wsxd.parent:
        raise RuntimeError('build adjacent wsx/wsxd first')
    work = ROOT / '.work' / ('ac-' + str(os.getpid()))
    work.parent.mkdir(exist_ok=True)
    work.mkdir(mode=0o700, exist_ok=False)
    daemon, success, started, path = None, False, time.monotonic(), None
    try:
        for name in ('home','state','project'):
            (work / name).mkdir(mode=0o700)
        config = (work / 'home/Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'config/wsx')
        config.mkdir(parents=True)
        (config / 'config-v2.toml').write_text('resume_agents_on_restore = false\n')
        path = work / 'state/wsx/wsx.sock'
        env = {'HOME':str(work / 'home'),'XDG_CONFIG_HOME':str(work / 'config'),'XDG_STATE_HOME':str(work / 'state'),
               'XDG_CACHE_HOME':str(work / 'cache'),'WSX_SOCKET':str(path),'WSX_DAEMON_BIN':str(wsxd),
               'PATH':'/usr/bin:/bin:/usr/sbin:/sbin','SHELL':'/bin/sh','TERM':'xterm-256color','LANG':'en_US.UTF-8'}
        subprocess.run(['git','init','-q','-b','main',str(work / 'project')],env=env,check=True,timeout=5)
        actor = work / 'actor.py'; actor.write_text(ACTOR)
        def call(method, params=None):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5); client.connect(str(path)); stream = client.makefile('rb')
                client.sendall(b'{"method":"hello","params":{"protocol":16}}\n'); stream.readline()
                request = {'method':method}
                if params is not None: request['params'] = params
                client.sendall(json.dumps(request).encode()+b'\n')
                return json.loads(stream.readline())
        def wait(test, label):
            deadline = time.monotonic()+5
            while time.monotonic() < deadline:
                if test(): return
                time.sleep(0.02)
            raise RuntimeError(label+' timeout')
        def cli(*args, ok=True):
            result = subprocess.run([str(wsx),*map(str,args)],env=env,cwd=work / 'project',capture_output=True,text=True,timeout=8)
            if ok: assert result.returncode == 0, result.stderr
            else: assert result.returncode != 0, result.stdout
            return result
        def traces(provider):
            file = work / (provider+'.trace')
            return [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []
        daemon = subprocess.Popen([str(wsxd)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        wait(path.exists,'daemon')
        assert call('synchronize_projects',{'projects':[{'name':'fixture','path':str(work / 'project'),
            'worktrees':[{'path':str(work / 'project'),'branch':'main'}]}]})['type'] == 'ack'
        worktree = call('snapshot')['data']['worktrees'][0]['id']
        panes = {}
        for provider in ('claude','codex'):
            history = work / (provider+'.jsonl')
            if provider == 'claude':
                entries = [{'type':'user','uuid':'question','parentUuid':None,'sessionId':'fixture','message':{'content':'native-only decision request'}},
                    {'type':'assistant','uuid':'answer','parentUuid':'question','sessionId':'fixture','message':{'content':'native-only decision: keep port 4321'}}]
            else:
                entries = [{'type':'response_item','payload':{'type':'message','role':'assistant','content':[{'type':'output_text','text':'independent codex history'}]}}]
            history.write_text(''.join(json.dumps(entry)+'\n' for entry in entries))
            created = call('session_create',{'worktree_id':worktree,'label':provider,
                'command':[sys.executable,str(actor),provider,str(history),str(work / (provider+'.trace'))],'rows':12,'cols':100})
            assert created['type'] == 'created', created
            panes[provider] = next(session['focused_pane'] for session in call('snapshot')['data']['sessions'] if session['id'] == created['data']['id'])
        wait(lambda: all(pane['agent'] for pane in call('snapshot')['data']['panes']),'agent reports')
        # Native-only text never appears on the fake terminal. One CLI packet proves the source.
        packet = json.loads(cli('agent','context','--json').stdout)
        assert len(packet['candidates']) == 2 and packet['matched'] == 2 and not packet['truncated']
        # Discover scoped identities before requesting any native conversation text.
        metadata = json.loads(cli('agent','context','--metadata-only','-p','fixture','--provider','claude','--json').stdout)
        assert metadata['projection'] == 'metadata_only' and metadata['matched'] == 1
        assert metadata['candidates'][0]['pane_id'] == panes['claude']
        assert all(item['memory'] is None for item in metadata['candidates'])
        assert 'native-only decision' not in json.dumps(metadata)
        selected = json.loads(cli('agent','context',panes['codex'],'--metadata-only','--json').stdout)
        assert len(selected['candidates']) == 1 and selected['candidates'][0]['pane_id'] == panes['codex']
        capped = json.loads(cli('agent','context','--metadata-only','--limit','1','--json').stdout)
        assert capped['truncated'] and capped['matched'] == 2 and len(capped['candidates']) == 1
        cli('agent','context','--metadata-only','-p','wrong','--json',ok=False)
        candidate = next(item for item in packet['candidates'] if item['pane_id'] == panes['claude'])
        assert candidate['eligible_state'] and candidate['project']['name'] == 'fixture'
        assert [message['text'] for message in candidate['memory']['messages']] == ['native-only decision request','native-only decision: keep port 4321']
        assert 'native-only' not in cli('session','peek',panes['claude'],'--trim').stdout
        bounded = json.loads(cli('agent','context','--limit','1','--bytes','7','--json').stdout)
        assert bounded['truncated'] and len(bounded['candidates']) == 1
        assert sum(len(message['text'].encode()) for message in bounded['candidates'][0]['memory']['messages']) <= 7
        exact = json.loads(cli('agent','context',panes['claude'],'--json').stdout)
        assert len(exact['candidates']) == 1
        assert 'ambiguous' not in cli('agent','context',panes['claude'],'-p','wrong','--json',ok=False).stderr
        # Exercise installed reporters against the real private generation owner.
        # Removing only this fixture's entry represents the pre-handoff/removed-keg boundary.
        codex = panes['codex']
        codex_generation = (work / 'codex.generation').read_text()
        installed = work / 'bin'; installed.mkdir(mode=0o755)
        (installed / 'wsx').symlink_to(wsx)
        version_stub = installed / 'codex'
        version_stub.write_text('#!/bin/sh\nprintf "codex-cli 0.160.0\\n"\n')
        version_stub.chmod(0o755)
        (work / 'home/.codex').mkdir(mode=0o700)
        report_env = dict(env, PATH=str(installed)+':'+env['PATH'],
            WSX_PANE_ID=str(codex), WSX_RUNTIME_GENERATION=codex_generation,
            WSX_AGENT_REPORT_BIN=str(work / 'removed-keg/wsx'))
        subprocess.run([str(wsx),'agent','install','codex'],env=report_env,
            capture_output=True,text=True,check=True,timeout=5)
        path.with_suffix('.reporter').unlink()
        hook = work / 'home/.codex/wsx-agent-status.sh'
        # Consume the installed configuration, not a direct script invocation.
        # Copy this fixture's assets/config to a second device root. Literal paths
        # would still reach device A, so remove A's entry to make the oracle discriminate.
        second_home = work / "device-b space ' home"
        second_home.mkdir(mode=0o700)
        shutil.copytree(work / 'home/.codex', second_home / '.codex')
        hook.unlink()
        hook_config = json.loads((second_home / '.codex/hooks.json').read_text())
        hook_commands = {state: hook_config['hooks'][event][0]['hooks'][0]['command']
                         for state, event in [('working','PreToolUse'),('blocked','PermissionRequest')]}
        assert all('CODEX_HOME' in command and 'HOME' in command
                   for command in hook_commands.values())
        assert all(str(work) not in command for command in hook_commands.values())
        def hook_report(state, generation):
            return subprocess.run(['/bin/sh','-c',hook_commands[state]],
                input=json.dumps({'session_id':'fixture'}),
                env=dict(report_env,HOME=str(second_home),WSX_RUNTIME_GENERATION=generation),
                capture_output=True,text=True,timeout=5)
        assert hook_report('working',codex_generation).returncode == 0
        def codex_agent():
            return next(pane for pane in call('snapshot')['data']['panes'] if pane['id'] == codex)['agent']
        assert codex_agent()['state'] == 'working'
        before_rejected = codex_agent()
        rejected = hook_report('blocked','stale-generation')
        assert rejected.returncode != 0 and 'stale_runtime' in rejected.stderr, (rejected.returncode, rejected.stderr)
        assert codex_agent() == before_rejected, 'stale-generation recovery changed the authoritative agent'
        node = shutil.which('node')
        assert node, 'Node is required for shared JavaScript reporter verification'
        runner = work / 'reporter.mjs'
        runner.write_text('import {execReporter} from '+json.dumps((ROOT / 'crates/wsx-core/integrations/common/wsx-reporter.mjs').as_uri())+';\n'
            'execReporter(process.env.WSX_AGENT_REPORT_BIN,["agent","report",process.env.WSX_PANE_ID,"--provider","codex","--state","idle","--session-id","fixture"],{env:process.env,timeout:3000},error=>{process.exitCode=error?1:0});\n')
        subprocess.run([node,str(runner)],env=report_env,check=True,capture_output=True,text=True,timeout=5)
        assert codex_agent()['state'] == 'idle'
        assert not path.with_suffix('.reporter').exists(), 'scenario did not preserve the missing-entry boundary'
        assert (work / 'codex.generation').read_text() == codex_generation
        os.kill(int((work / 'codex.pid').read_text()),0)
        # Installed Codex reports lifecycle only. Restore the actor's deliberate
        # prompt capability for the unsupported draft-policy scenario below.
        assert call('agent_report',{'pane_id':codex,'runtime_generation':codex_generation,
            'provider':'codex','state':'idle','session_ref':{'kind':'id','value':'fixture'},
            'capabilities':{'prompt':True,'lifecycle':True}})['type'] == 'ack'
        claude = panes['claude']
        # A lease or blocked agent must preserve both the draft and persisted intent.
        assert call('terminal_acquire',{'pane_id':claude,'client_id':987,'takeover':False})['type'] == 'ack'
        before = call('agent_exchange_list', {})['data']
        assert 'terminal_busy' in cli('agent','request',claude,'denied','--stash-draft','--json',ok=False).stderr
        assert call('agent_exchange_list', {})['data'] == before and not traces('claude')
        assert call('terminal_release',{'pane_id':claude,'client_id':987})['type'] == 'ack'
        generation = (work / 'claude.generation').read_text()
        for state in ('working','blocked','unknown','error'):
            if state == 'unknown':
                cli('session','send-text',claude,'\x1e','--no-enter')
                wait(lambda: 'editor hidden' in cli('session','peek',claude,'--trim').stdout,'non-idle chrome')
            report = {'pane_id':claude,'runtime_generation':generation,'provider':'claude','state':state,
                'session_ref':{'kind':'id','value':'fixture'},'capabilities':{'prompt':True,'lifecycle':True}}
            assert call('agent_report',report)['type'] == 'ack'
            assert 'agent_busy' in cli('agent','request',claude,'denied','--stash-draft','--json',ok=False).stderr
            assert call('agent_exchange_list', {})['data'] == before and not traces('claude')
        cli('session','send-text',claude,'\x1e','--no-enter')
        wait(lambda: '❯' in cli('session','peek',claude,'--trim').stdout,'restored idle chrome')
        report['state'] = 'idle'
        assert call('agent_report',report)['type'] == 'ack'
        # Path metadata survives a same-session report; stale reports cannot replace it.
        old = next(pane for pane in call('snapshot')['data']['panes'] if pane['id'] == claude)['agent']['session_ref']
        assert old['transcript_path'] == str(work / 'claude.jsonl')
        stale = dict(report,runtime_generation='stale',session_ref={'kind':'id','value':'other','transcript_path':str(work / 'codex.jsonl')})
        assert call('agent_report',stale)['data']['code'] == 'stale_runtime'
        assert next(pane for pane in call('snapshot')['data']['panes'] if pane['id'] == claude)['agent']['session_ref'] == old
        assert 'draft_policy_unsupported' in cli('agent','request',panes['codex'],'denied','--stash-draft','--json',ok=False).stderr
        # Actual PTY producer records what was submitted and what remains in the stash.
        cli('session','send-text',claude,'\x12','--no-enter')
        exchange = json.loads(cli('agent','request',claude,'inspect native decision','--stash-draft','--json').stdout)['exchange']
        wait(lambda: len(traces('claude')) == 1,'delivered prompt')
        received = traces('claude')[0]
        assert received['received'] == '[wsx exchange '+str(exchange['id'])+', round 1, read-only]\ninspect native decision'
        assert received['stash'] == '\nunsent draft'
        # The independent PTY actor records native input; do not hash a returned prompt.
        initial_digest = hashlib.sha256(received['received'].encode()).hexdigest()
        assert exchange['delivery_sha256'] == initial_digest
        assert exchange['delivery_sha256'] != hashlib.sha256(b'forged envelope body').hexdigest()
        wait(lambda: call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'working_observed','native turn pending')
        def receipt(kind, round=1, generation=generation, ok=True, bound=False,
                    digest=None, input_id='fixture-input-1', pane=claude):
            args = [str(wsx),'agent','exchange-receipt',str(exchange['id']),
                '--round',str(round),'--receipt',kind,'--json']
            if bound:
                args += ['--delivery-sha256',digest or exchange['delivery_sha256'],
                         '--input-id',input_id]
            result = subprocess.run(args,
                env=dict(env,WSX_RUNTIME_GENERATION=generation,WSX_PANE_ID=str(pane)),
                capture_output=True,text=True,timeout=5)
            assert (result.returncode == 0) == ok, (result.stdout,result.stderr)
            return result
        assert 'unsupported_exchange_receipt' in receipt('accepted',ok=False).stderr
        cli('session','send-text',claude,'\x14','--no-enter')
        wait(lambda: next(pane for pane in call('snapshot')['data']['panes'] if pane['id'] == claude)['agent']['capabilities']['exchange_receipts'],'native receipt capability')
        before_receipt = call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']
        assert 'managed wsx runtime generation' in receipt('accepted',generation='',ok=False).stderr
        assert 'stale_runtime' in receipt('accepted',generation='stale',ok=False).stderr
        assert 'stale_exchange_round' in receipt('accepted',round=2,ok=False).stderr
        receipt('done',ok=False)
        receipt('accepted',round=0,ok=False)
        assert call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange'] == before_receipt
        assert 'invalid_delivery_binding' in receipt('accepted',bound=True,digest='0'*64,ok=False).stderr
        assert 'invalid_delivery_binding' in receipt('accepted',bound=True,pane=panes['codex'],ok=False).stderr
        assert 'native_input_unaccepted' in receipt('completed',bound=True,ok=False).stderr
        assert call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange'] == before_receipt
        accepted = json.loads(receipt('accepted',bound=True).stdout)['exchange']
        assert accepted['native_input_id'] == 'fixture-input-1'
        assert accepted['evidence'] == 'request_bound' and accepted['state'] != 'completed'
        assert json.loads(receipt('accepted',bound=True).stdout)['exchange']['revision'] == accepted['revision']
        assert 'invalid_delivery_binding' in receipt('completed',bound=True,input_id='other-input',ok=False).stderr
        assert 'bound_receipt_required' in receipt('completed',ok=False).stderr
        cli('session','send-text',claude,'\x12','--no-enter')
        wait(lambda: call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'done_observed','done lifecycle')
        completed = json.loads(receipt('completed',bound=True).stdout)['exchange']
        assert completed['state'] == 'completed' and completed['evidence'] == 'request_bound'
        assert json.loads(receipt('completed',bound=True).stdout)['exchange']['revision'] == completed['revision']
        cli('session','send-text',claude,'\x13','--no-enter')
        wait(lambda: 'unsent draft' in cli('session','peek',claude,'--trim').stdout,'restore draft before continuation')
        continued = json.loads(cli('agent','continue',exchange['id'],'follow up','--stash-draft','--json').stdout)['exchange']
        wait(lambda: len(traces('claude')) == 2,'fragmented follow-up delivery')
        assert continued['round'] == 2 and continued.get('native_input_id') is None
        assert 'stale_exchange_round' in receipt('completed',bound=True,ok=False).stderr
        assert continued['delivery_sha256'] == hashlib.sha256(traces('claude')[1]['received'].encode()).hexdigest()
        assert continued['delivery_sha256'] != initial_digest
        assert traces('claude')[1]['received'].endswith('\nfollow up')
        assert traces('claude')[1]['stash'] == '\nunsent draft'
        wait(lambda: call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'done_observed','follow-up done')
        cli('agent','continue',exchange['id'],'empty editor follow up','--stash-draft','--json')
        wait(lambda: len(traces('claude')) == 3,'empty-editor continuation')
        assert traces('claude')[2]['received'].endswith('\nempty editor follow up')
        assert traces('claude')[2]['stash'] == '\nunsent draft', 'empty editor restored or lost the prior stash'
        context = json.loads(cli('agent','context',claude,'--json').stdout)['candidates'][0]
        assert context['memory']['messages'][-1]['text'].endswith('\nempty editor follow up')
        wait(lambda: call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'done_observed','empty-editor done')
        # Receipts must enforce their deadline without a preceding get/list probe.
        cli('session','send-text',claude,'\x12','--no-enter')
        exchange = json.loads(cli('agent','request',claude,'late receipt must not complete',
            '--timeout','1','--json').stdout)['exchange']
        wait(lambda: len(traces('claude')) == 4,'late native turn pending')
        time.sleep(max(0,exchange['deadline_unix_ms']/1000-time.time())+0.1)
        assert 'invalid_exchange_state' in receipt('completed',ok=False).stderr
        assert call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'expired'
        cli('session','send-text',claude,'\x12','--no-enter')
        wait(lambda: next(pane for pane in call('snapshot')['data']['panes'] if pane['id'] == claude)['agent']['state'] == 'done','late actor settled')
        # A DoneObserved pane is not native completion and must not bypass the bound deadline.
        cli('session','send-text',claude,'\x12','--no-enter')
        exchange = json.loads(cli('agent','request',claude,'late DoneObserved native completion',
            '--timeout','1','--json').stdout)['exchange']
        wait(lambda: len(traces('claude')) == 5,'second late native input')
        receipt('accepted',bound=True)
        cli('session','send-text',claude,'\x12','--no-enter')
        wait(lambda: call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'done_observed','late done observation')
        time.sleep(max(0,exchange['deadline_unix_ms']/1000-time.time())+0.1)
        assert 'receipt_deadline_exceeded' in receipt('completed',bound=True,ok=False).stderr
        assert call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'done_observed'
        cli('session','send-text',claude,'\x13\x11','--no-enter')
        wait(lambda: 'unsent draft' in cli('session','peek',claude,'--trim').stdout,'restored draft with unsupported binding')
        failed = json.loads(cli('agent','continue',exchange['id'],'must not arrive','--stash-draft','--json').stdout)['exchange']
        assert failed['round'] == 2, 'failed continuation did not invalidate the previous round'
        assert failed['state'] == 'delivery_failed' and failed['evidence'] == 'intent_persisted'
        assert failed.get('delivery_sha256') is None, 'failed delivery retained input-binding authority'
        assert failed.get('native_input_id') is None, 'failed continuation retained native acceptance'
        assert len(traces('claude')) == 5, 'unconfirmed stash delivered a prompt'
        assert call('shutdown')['type'] == 'ack'
        daemon.wait(timeout=5)
        assert not path.exists() and daemon.returncode == 0
        # Context must not bootstrap a daemon or silently fall back to a screen.
        cli('agent','context','--json',ok=False)
        assert not path.exists()
        receipt('completed',round=3,ok=False)
        assert not path.exists(), 'receipt bootstrapped a missing daemon'
        callback = subprocess.run([str(wsx),'agent','report',str(claude),'--provider','claude','--state','idle'],
            env=dict(env,WSX_PANE_ID=str(claude),WSX_RUNTIME_GENERATION='old'),capture_output=True,text=True,timeout=5)
        assert callback.returncode != 0 and not path.exists(), 'callback bootstrapped an obsolete runtime'
        success = True
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try: daemon.wait(timeout=5)
            except subprocess.TimeoutExpired: daemon.kill(); daemon.wait(timeout=3)
        # A failed no-bootstrap assertion may have spawned a replacement outside
        # the original Popen handle. Stop only this private fixture's socket.
        if path is not None and path.exists():
            assert path.lstat().st_uid == os.getuid(), 'private socket changed owner'
            assert call('shutdown')['type'] == 'ack'
            wait(lambda: not path.exists(), 'private replacement cleanup')
        for record in work.glob('*.pid'):
            deadline = time.monotonic()+3
            while time.monotonic() < deadline:
                try: os.kill(int(record.read_text()),0)
                except ProcessLookupError: break
                time.sleep(0.02)
            else: raise RuntimeError('private actor still alive; retained '+str(work))
        if success: shutil.rmtree(work)
        else: print('diagnostics retained at '+str(work),file=sys.stderr)
    print(json.dumps({'result':'PASS','seconds':round(time.monotonic()-started,2),'model_calls':0,
        'journey':'installed shell/JS reporting, current/stale generation, unchanged actor, metadata/native history, scoped bounds, lease/provider refusal, preserved draft/continuation, capability/generation/round-fenced request-bound receipts, lifecycle distinct from completion, duplicate receipt idempotence, no bootstrap, private cleanup'}))

if __name__ == '__main__': main()
