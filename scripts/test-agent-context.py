#!/usr/bin/env python3
"""Model-free CLI/native-history and real-PTY draft-stash orchestration journey."""
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
leaf, sequence, idle_chrome = 'answer', 0, True
def report(state):
 with socket.socket(socket.AF_UNIX) as client:
  client.connect(os.environ['WSX_SOCKET']); stream = client.makefile('rb')
  client.sendall(b'{"method":"hello","params":{"protocol":16}}\n'); stream.readline()
  params = {'pane_id':pane,'runtime_generation':generation,'provider':provider,'state':state,
    'session_ref':{'kind':'id','value':'fixture','transcript_path':str(history)},
    'capabilities':{'prompt':True,'lifecycle':True}}
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
  if char == b'\x1e':
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
   draft = ''; paint(); report('done')
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
    work.mkdir(mode=0o700, exist_ok=False)
    daemon, success, started = None, False, time.monotonic()
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
        exchange = json.loads(cli('agent','request',claude,'inspect native decision','--stash-draft','--json').stdout)['exchange']
        wait(lambda: len(traces('claude')) == 1,'delivered prompt')
        received = traces('claude')[0]
        assert received['received'] == '[wsx exchange '+str(exchange['id'])+', round 1, read-only]\ninspect native decision'
        assert received['stash'] == '\nunsent draft'
        wait(lambda: call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'done_observed','done lifecycle')
        cli('session','send-text',claude,'\x13','--no-enter')
        wait(lambda: 'unsent draft' in cli('session','peek',claude,'--trim').stdout,'restore draft before continuation')
        cli('agent','continue',exchange['id'],'follow up','--stash-draft','--json')
        wait(lambda: len(traces('claude')) == 2,'fragmented follow-up delivery')
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
        cli('session','send-text',claude,'\x13\x11','--no-enter')
        wait(lambda: 'unsent draft' in cli('session','peek',claude,'--trim').stdout,'restored draft with unsupported binding')
        failed = json.loads(cli('agent','request',claude,'must not arrive','--stash-draft','--json').stdout)['exchange']
        assert failed['state'] == 'delivery_failed' and failed['evidence'] == 'intent_persisted'
        assert len(traces('claude')) == 3, 'unconfirmed stash delivered a prompt'
        assert call('shutdown')['type'] == 'ack'
        daemon.wait(timeout=5)
        assert not path.exists() and daemon.returncode == 0
        # Context must not bootstrap a daemon or silently fall back to a screen.
        cli('agent','context','--json',ok=False)
        assert not path.exists()
        callback = subprocess.run([str(wsx),'agent','report',str(claude),'--provider','claude','--state','idle'],
            env=dict(env,WSX_PANE_ID=str(claude),WSX_RUNTIME_GENERATION='old'),capture_output=True,text=True,timeout=5)
        assert callback.returncode != 0 and not path.exists(), 'callback bootstrapped an obsolete runtime'
        success = True
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try: daemon.wait(timeout=5)
            except subprocess.TimeoutExpired: daemon.kill(); daemon.wait(timeout=3)
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
        'journey':'one-call context, native-only history, bounds, exact scope, lease/provider refusal, preserved draft, empty-editor continuation, no bootstrap, private cleanup'}))

if __name__ == '__main__': main()
