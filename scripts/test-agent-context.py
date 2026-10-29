#!/usr/bin/env python3
"""Model-free CLI/native-history and real-PTY draft-stash orchestration journey."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

import wsxd_client

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
# ^ Claude Code 2.1.3xx draws the editor between two borders, with up to seven footer rows
# under it (status line, mode, update notice, branch, background agents).
tall_footer = False
FOOTER = ['  [S] wsx>main(+0,-0) | 23.0%', '  Opus 5.5 medium | 5h 3%', '  auto mode on · 1 shell',
          '               ✔ Update installed · Restart to update', '', '  main', '  general-purpose  Checking · 6m 47s']
pause_completion, pending_completion, receipt_capable = False, False, False
leaf, sequence, idle_chrome = 'answer', 0, True
def report(state):
 with socket.socket(socket.AF_UNIX) as client:
  client.connect(os.environ['WSX_SOCKET']); stream = client.makefile('rb')
  client.sendall(b'{"method":"hello","params":{"protocol":@PROTOCOL@}}\n'); stream.readline()
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
 if tall_footer:
  lines = (marker+draft).split('\n')
  box = ['─'*40, lines[0]] + ['  '+line for line in lines[1:]] + ['─'*40] + FOOTER
  sys.stdout.write('\x1b[?2004h\x1b]2;'+title+'\x07\x1b[2J\x1b['+str(max(1,rows-len(box)))+';1H'+'\r\n'.join(box)+'\x1b[J')
 else:
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
  elif char == b'\x06':
   tall_footer = not tall_footer; paint()
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

# A worker for the orchestration journey: a prompt-capable agent that writes each
# submitted prompt and its reply to a provider-native history file. It is named after
# the worktree it runs in. `no-reply` in a prompt leaves the reply out, `hold` keeps the
# turn at work until a `<name>.release` file exists, and a `<name>.stall` file holds its
# first read for that many seconds.
WORKER = r'''
import json, os, pathlib, signal, socket, sys, time, tty
signal.signal(signal.SIGHUP, signal.SIG_IGN)
root = pathlib.Path(sys.argv[1]); name = pathlib.Path.cwd().name
history, trace, stall = root / (name+'.jsonl'), root / (name+'.trace'), root / (name+'.stall')
pane = int(os.environ['WSX_PANE_ID']); generation = os.environ['WSX_RUNTIME_GENERATION']
(root / (name+'.pid')).write_text(str(os.getpid()))
def report(state):
 with socket.socket(socket.AF_UNIX) as client:
  client.connect(os.environ['WSX_SOCKET']); stream = client.makefile('rb')
  client.sendall(b'{"method":"hello","params":{"protocol":@PROTOCOL@}}\n'); stream.readline()
  params = {'pane_id':pane,'runtime_generation':generation,'provider':'claude','state':state,
    'session_ref':{'kind':'id','value':'fixture','transcript_path':str(history)},
    'capabilities':{'prompt':True,'lifecycle':True}}
  client.sendall(json.dumps({'method':'agent_report','params':params}).encode()+b'\n')
  assert json.loads(stream.readline())['type'] == 'ack'
def record(role, entry, parent, text):
 with history.open('a') as out:
  out.write(json.dumps({'type':role,'uuid':entry,'parentUuid':parent,'sessionId':'fixture',
    'message':{'content':text}})+'\n')
tty.setraw(0)
sys.stdout.write('\x1b[?2004h'); sys.stdout.flush()
if (root / (name+'.ask-trust')).exists():
 # Claude's folder trust question: no hook reports until it is answered, "No" is first.
 choice = 0
 def ask():
  sys.stdout.write('\x1b[2J\x1b[H Quick safety check: Is this a project you created or one you trust?\r\n'
   +(' ❯ ' if choice == 0 else '   ')+'No, exit\r\n'+(' ❯ ' if choice == 1 else '   ')+'Yes, I trust this folder\r\n'
   +' Enter to confirm\r\n'); sys.stdout.flush()
 ask()
 while True:
  key = os.read(0,16)
  if key == b'\x1b[B': choice = 1; ask()
  elif key == b'\r':
   if choice == 0: sys.exit(0)
   (root / (name+'.trusted')).write_text('yes'); sys.stdout.write('\x1b[2J\x1b[H'); break
record('assistant', 'before', None, 'an older message that is not a result')
draft, pending, leaf, turn = '', b'', 'before', 0
report('idle')
if stall.exists(): time.sleep(float(stall.read_text())); stall.unlink()
while True:
 pending += os.read(0,65536)
 while pending:
  if pending.startswith(b'\x1b[200~'):
   end = pending.find(b'\x1b[201~')
   if end < 0: break
   draft += pending[6:end].decode(); pending = pending[end+6:]; continue
  if pending.startswith(b'\x1b') and len(pending) < 6: break
  char, pending = pending[:1], pending[1:]
  if char != b'\r': draft += char.decode(errors='replace'); continue
  with trace.open('a') as out: out.write(json.dumps({'received':draft})+'\n')
  turn += 1; report('working')
  record('user', 'u'+str(turn), leaf, draft); leaf = 'u'+str(turn)
  while 'hold' in draft and not (root / (name+'.release')).exists(): time.sleep(0.02)
  if 'no-reply' not in draft:
   record('assistant', 'a'+str(turn), leaf, 'result of '+name+': '+draft.splitlines()[-1]); leaf = 'a'+str(turn)
  draft = ''; report('done')
'''


rpc = wsxd_client.call


def until(test, label, seconds=5):
    deadline = time.monotonic()+seconds
    while time.monotonic() < deadline:
        if test(): return
        time.sleep(0.02)
    raise RuntimeError(label+' timeout')


def orchestration(wsx, wsxd, released):
    '''A manager starts workers in new worktrees, waits and reads results: docs/agent-orchestration.md.'''
    work = ROOT / '.work' / ('ao-' + str(os.getpid()))
    work.parent.mkdir(exist_ok=True)
    work.mkdir(mode=0o700, exist_ok=False)
    daemons, success, started = [], False, time.monotonic()
    repos = work / 'repos'
    try:
        for name in ('home','state','repos','run'):
            (work / name).mkdir(mode=0o700)
        config = (work / 'home/Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'config/wsx')
        config.mkdir(parents=True)
        (config / 'config-v2.toml').write_text('resume_agents_on_restore = false\n' + ''.join(
            '[[projects]]\nname = "%s"\npath = %s\n' % (name, json.dumps(str(repos / name))) for name in ('fixture','other')))
        path = work / 'state/wsx/wsx.sock'
        env = {'HOME':str(work / 'home'),'XDG_CONFIG_HOME':str(work / 'config'),'XDG_STATE_HOME':str(work / 'state'),
               'XDG_CACHE_HOME':str(work / 'cache'),'WSX_SOCKET':str(path),'WSX_DAEMON_BIN':str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()),
               'PATH':'/usr/bin:/bin:/usr/sbin:/sbin','SHELL':'/bin/sh','TERM':'xterm-256color','LANG':'en_US.UTF-8',
               'GIT_AUTHOR_NAME':'fixture','GIT_AUTHOR_EMAIL':'fixture@example.invalid',
               'GIT_COMMITTER_NAME':'fixture','GIT_COMMITTER_EMAIL':'fixture@example.invalid',
               'GIT_CEILING_DIRECTORIES':str(work)}
        def git(repo, *args, ok=True):
            result = subprocess.run(['git','-C',str(repo),*args],env=env,capture_output=True,text=True,timeout=10)
            assert not ok or result.returncode == 0, result.stderr
            return result
        for name in ('fixture','other'):
            subprocess.run(['git','init','-q','-b','main',str(repos / name)],env=env,check=True,timeout=5)
            top = git(repos / name,'rev-parse','--show-toplevel').stdout.strip()
            assert os.path.realpath(top) == os.path.realpath(repos / name), ('fixture git escaped', top)
            git(repos / name,'commit','-q','--allow-empty','-m','start')
        worker = work / 'worker.py'; worker.write_text(wsxd_client.fixture(WORKER))
        agent = '%s %s %s' % (sys.executable, worker, work / 'run')
        def cli(*args, ok=True, stdin=None, seconds=20, cwd=work, binary=wsx, env=env):
            result = subprocess.run([str(binary),*map(str,args)],env=env,cwd=cwd,input=stdin,capture_output=True,text=True,timeout=seconds)
            assert ok is None or (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
            return result
        def received(name):
            file = work / 'run' / (name+'.trace')
            return [json.loads(line)['received'] for line in file.read_text().splitlines()] if file.exists() else []
        def exchange(exchange_id):
            return rpc(path,'agent_exchange_get',{'exchange_id':exchange_id})['data']['exchange']
        def at_boundary(exchange_id):
            return exchange(exchange_id)['state'] == 'done_observed'
        daemons.append(subprocess.Popen([str(wsxd)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL))
        until(path.exists,'daemon')

        # A failed `git worktree add` leaves no branch, so the same command works again.
        fixture = repos / 'fixture'
        git(fixture,'branch','kept','main')
        repos.chmod(0o500)
        try:
            refused = cli('worktree','create','w/retry','--from','main','--json','-p','fixture',ok=False)
            assert 'git worktree add' in refused.stderr, refused.stderr
            assert git(fixture,'show-ref','--verify','--quiet','refs/heads/w/retry',ok=False).returncode != 0, 'failed creation left its branch'
            cli('worktree','create','kept','--json','-p','fixture',ok=False)
            git(fixture,'show-ref','--verify','--quiet','refs/heads/kept')
        finally:
            repos.chmod(0o700)
        retried = json.loads(cli('worktree','create','w/retry','--from','main','--json','-p','fixture').stdout)
        assert retried['branch'] == 'w/retry' and Path(retried['worktree']).is_dir() and retried['pane_id']

        # One call returns every identity of a new session, and a list row says where it is.
        scope = ('-p','fixture','-w','w/retry')
        def ready(pane_id):
            pane = next(pane for pane in rpc(path,'snapshot')['data']['panes'] if pane['id'] == pane_id)
            return pane['agent'] and pane['agent']['state'] in ('idle','done')
        session = json.loads(cli('session','create','--name','worker','--command',agent,'--json',*scope).stdout)
        assert session['pane_id'] and session['branch'] == 'w/retry' and 'agent' in session
        until(lambda: ready(session['pane_id']),'worker agent')
        row = next(row for row in json.loads(cli('session','list','--json','-p','fixture').stdout)
                   if row['session_id'] == session['session_id'])
        assert (row['project'],row['branch'],row['worktree']) == ('fixture','w/retry',retried['worktree'])

        # A prompt comes from an argument, a file or standard input: exactly one, never empty.
        retry = 'fixture-w-retry'
        prompt_file = work / 'prompt.txt'; prompt_file.write_text('from a file\nsecond line\n')
        first = json.loads(cli('agent','request','worker','--prompt-file',prompt_file,'--json',*scope).stdout)['exchange']
        until(lambda: len(received(retry)) == 1,'file prompt')
        assert received(retry)[0] == '[wsx exchange %d, round 1, read-only]\nfrom a file\nsecond line' % first['id']
        until(lambda: at_boundary(first['id']),'file prompt done')
        cli('agent','continue',first['id'],'-','--json',stdin='from standard input\n')
        until(lambda: len(received(retry)) == 2,'standard input prompt')
        assert received(retry)[1].endswith(', round 2, read-only]\nfrom standard input')
        until(lambda: at_boundary(first['id']),'standard input prompt done')
        large = work / 'large.txt'; large.write_text('x' * 65537)
        for refused, reason in [(('agent','request','worker','-'),'empty'),
                (('agent','request','worker'),'prompt is required'),
                (('agent','request','worker','two','--prompt-file',prompt_file),'one prompt source'),
                (('agent','request','worker','--prompt-file',large),'65536 bytes'),
                (('agent','continue',first['id'],'--prompt-file',large),'65536 bytes'),
                (('agent','continue',first['id'],'--prompt-file',work / 'absent.txt'),'cannot open'),
                (('session','prompt','worker','--prompt-file',large),'65535 bytes')]:
            assert reason in cli(*refused,*(scope if refused[1] != 'continue' else ()),ok=False,stdin='').stderr, refused
        assert len(received(retry)) == 2 and exchange(first['id'])['round'] == 2, 'a refused prompt reached the agent'
        cli('session','prompt','worker','--prompt-file',prompt_file,*scope)
        until(lambda: len(received(retry)) == 3,'session prompt from a file')
        assert received(retry)[2] == 'from a file\nsecond line'

        # A delivery that takes longer than the ordinary reply limit still returns its exchange.
        slow = json.loads(cli('worktree','create','w/slow','--from','main','--json','-p','fixture').stdout)
        (work / 'run/fixture-w-slow.stall').write_text('7')
        scope = ('-p','fixture','-w','w/slow')
        session = json.loads(cli('session','create','--name','worker','--command',agent,'--json',*scope).stdout)
        until(lambda: ready(session['pane_id']),'slow worker agent')
        spare = json.loads(cli('session','create','--name','spare','--json',*scope).stdout)['session_id']
        prompt_file.write_text('slow delivery\n' + 'y' * 16000)
        keyed = ('agent','request','worker','--prompt-file',prompt_file,'--key','slow-1','--json',*scope)
        revision = rpc(path,'snapshot')['data']['revision']
        first_call = subprocess.Popen([str(wsx),*map(str,keyed)],env=env,cwd=work,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        # The worker holds its first read for 7 seconds. wsxd still answers a read meanwhile.
        # A repeat with the same key, sent while the first delivery is still in the PTY,
        # must return that exchange and deliver nothing.
        until(lambda: rpc(path,'snapshot')['data']['revision'] > revision,'intent of the slow delivery')
        began = time.monotonic()
        repeat = json.loads(cli(*keyed,seconds=40).stdout)['exchange']
        # Other changes go on while the PTY takes the prompt; the client limit is 5 seconds.
        cli('session','rename',spare,'spare-renamed')
        cli('session','delete',spare,'--json')
        assert time.monotonic() - began < 3 and first_call.poll() is None, 'a slow delivery held other changes'
        stdout, stderr = first_call.communicate(timeout=40)
        assert first_call.returncode == 0, stderr
        slow_exchange = json.loads(stdout)['exchange']
        assert time.monotonic() - began > 3, 'the worker did not hold the delivery'
        assert slow_exchange['id'] == repeat['id'] and slow_exchange['key'] == 'slow-1'
        assert slow_exchange['state'] == 'delivered' and slow_exchange['evidence'] == 'pty_delivery'
        slow_name = 'fixture-w-slow'
        until(lambda: at_boundary(slow_exchange['id']),'slow delivery done',10)
        assert json.loads(cli(*keyed).stdout)['exchange']['id'] == slow_exchange['id']
        assert [text.splitlines()[1] for text in received(slow_name)] == ['slow delivery'], 'a keyed repeat delivered again'
        follow_up = ('agent','continue',slow_exchange['id'],'keyed follow up','--key','slow-2','--json')
        rounds = [json.loads(cli(*follow_up).stdout)['exchange'] for _ in range(2)]
        assert [item['round'] for item in rounds] == [2,2] and rounds[1]['continue_key'] == 'slow-2'
        until(lambda: at_boundary(slow_exchange['id']),'keyed follow up done')
        assert len(received(slow_name)) == 2 and received(slow_name)[1].endswith('\nkeyed follow up')
        assert 'invalid_key' in cli('agent','request','worker','refused','--key','has space','--json',*scope,ok=False).stderr
        # An agent that does not read its input ends the delivery at the 10-second write limit.
        cli('worktree','create','w/stuck','--from','main','--json','-p','fixture')
        (work / 'run/fixture-w-stuck.stall').write_text('60')
        stuck = ('-p','fixture','-w','w/stuck')
        stuck_session = json.loads(cli('session','create','--name','worker','--command',agent,'--json',*stuck).stdout)
        until(lambda: ready(stuck_session['pane_id']),'stuck worker agent')
        prompt_file.write_text('never read\n' + 'z' * 60000)
        began = time.monotonic()
        failed = json.loads(cli('agent','request','worker','--prompt-file',prompt_file,'--timeout','120','--json',*stuck,seconds=40).stdout)['exchange']
        assert 9 < time.monotonic() - began < 15 and failed['state'] == 'delivery_failed', failed
        assert failed.get('delivery_sha256') is None and failed['evidence'] == 'intent_persisted'
        # Closing that pane returns too: the PTY writer's end-of-file byte no longer blocks.
        cli('session','delete',stuck_session['session_id'],'--json')
        assert len(received(slow_name)) == 2

        # The result of a round is the agent's last message after that round was delivered.
        answer = json.loads(cli('agent','wait',slow_exchange['id'],'--result','--json').stdout)
        assert answer['exchange']['state'] == 'done_observed' and answer['exchange']['round'] == 2
        assert answer['result']['status'] == 'available' and answer['result']['round'] == 2
        assert answer['result']['text'] == 'result of fixture-w-slow: keyed follow up' and not answer['result']['truncated']
        clipped = json.loads(cli('agent','wait',slow_exchange['id'],'--result','--result-bytes','6','--json').stdout)['result']
        assert clipped['text'] == 'result' and clipped['truncated']
        # A round with no message after its delivery has no result. The message of the
        # round before it is in the same history and must not be returned.
        cli('agent','continue',slow_exchange['id'],'no-reply for this round','--json')
        until(lambda: len(received(slow_name)) == 3 and at_boundary(slow_exchange['id']),'round without a reply')
        refused = json.loads(cli('agent','wait',slow_exchange['id'],'--result','--json').stdout)
        assert refused['exchange']['round'] == 3 and refused['result']['status'] == 'no_message_after_delivery'
        assert refused['result']['text'] is None and 'keyed follow up' not in json.dumps(refused['result'])
        assert 'result unavailable: no_message_after_delivery' in cli('agent','wait',slow_exchange['id'],'--result').stdout
        assert 'result' not in json.loads(cli('agent','wait',slow_exchange['id'],'--json').stdout)

        # A manager starts two workers in two new worktrees with one shell line, waits for the
        # first that finishes and reads each result. It calls no lookup command: every ID comes
        # from the output of the command before it.
        (work / 'one.txt').write_text('task one\n'); (work / 'two.txt').write_text('task two\n')
        manager = '''
id() { /usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["exchange"]["id"])' "$1"; }
"$WSX" agent start w/a -p fixture --from main --agent "$AGENT" --name worker --prompt-file one.txt --key a1 --json > a.json &&
"$WSX" agent start w/b -p fixture --from main --agent "$AGENT" --name worker --prompt-file two.txt --key b1 --writer --json > b.json &&
A=$(id a.json) && B=$(id b.json) &&
"$WSX" agent wait --any $A $B --result --timeout 30 --json > first.json &&
for X in $A $B; do "$WSX" agent wait $X --result --timeout 30 --json > result-$X.json || exit 1; done
'''
        assert not any(word in manager for word in ('list','status','context','exchanges','inspect','peek'))
        line = subprocess.run(['/bin/sh','-c',manager],env=dict(env,WSX=str(wsx),AGENT=agent),cwd=work,capture_output=True,text=True,timeout=120)
        assert line.returncode == 0, line.stderr
        started_a, started_b = (json.loads((work / name).read_text()) for name in ('a.json','b.json'))
        for start, branch, name, task in [(started_a,'w/a','fixture-w-a','task one'),(started_b,'w/b','fixture-w-b','task two')]:
            assert start['project'] == 'fixture' and start['branch'] == branch and start['agent'] == 'claude'
            assert Path(start['worktree']).name == name and Path(start['worktree']).is_dir()
            assert start['worktree_created'] and start['session_created'] and start['session_label'] == 'worker'
            assert start['session_id'] and start['pane_id'] == start['exchange']['pane_id'] and 'error' not in start
            answer = json.loads((work / ('result-%d.json' % start['exchange']['id'])).read_text())
            assert answer['result']['status'] == 'available' and answer['result']['text'] == 'result of %s: %s' % (name, task)
            assert [text.splitlines()[-1] for text in received(name)] == [task]
            # The agent session is the only session of the new worktree.
            rows = [row for row in json.loads(cli('session','list','--json','-p','fixture').stdout) if row['branch'] == branch]
            assert [row['session_id'] for row in rows] == [start['session_id']], rows
        assert started_b['exchange']['access'] == 'writer' and started_a['exchange']['key'] == 'a1'
        first = json.loads((work / 'first.json').read_text())
        both = {started_a['exchange']['id'], started_b['exchange']['id']}
        assert first['exchange']['id'] in first['ready'] and set(first['ready']) | set(first['pending']) == both
        assert first['result']['status'] == 'available'
        # The same command with the same key uses what exists and delivers nothing.
        again = json.loads(cli('agent','start','w/a','-p','fixture','--from','main','--agent',agent,'--name','worker',
            '--prompt-file',work / 'one.txt','--key','a1','--json').stdout)
        assert again['exchange']['id'] == started_a['exchange']['id'] and again['session_id'] == started_a['session_id']
        assert not again['worktree_created'] and not again['session_created'] and len(received('fixture-w-a')) == 1

        # A late failure reports what exists and deletes nothing.
        late = cli('agent','start','w/late','-p','fixture','--agent','cat','late task','--ready-timeout','2','--json',ok=False)
        report = json.loads(late.stdout)
        assert report['error']['step'] == 'ready' and 'agent_not_ready' in report['error']['message']
        assert report['worktree_created'] and report['session_created'] and report['pane_id'] and report['exchange'] is None
        assert 'session peek %d' % report['session_id'] in report['next'] and Path(report['worktree']).is_dir()
        assert "stopped at step 'ready'" in late.stderr
        assert report['error']['reason'] == 'no_agent_report' and 'cat' in report['error']['screen']
        # The folder trust question stops the start at once and is answered only on request.
        (work / 'run/fixture-w-trust.ask-trust').write_text('')
        asked_at = time.monotonic()
        asked = json.loads(cli('agent','start','w/trust','-p','fixture','--agent',agent,'trust task','--key','t1',
            '--ready-timeout','60','--json',ok=False).stdout)
        assert time.monotonic() - asked_at < 15 and asked['error']['reason'] == 'folder_trust_question', asked
        assert 'Yes, I trust this folder' in asked['error']['screen'] and '--accept-folder-trust' in asked['next']
        assert not asked['folder_trust_accepted'] and not (work / 'run/fixture-w-trust.trusted').exists()
        trusted = json.loads(cli('agent','start','w/trust','-p','fixture','--agent',agent,'trust task','--key','t1',
            '--accept-folder-trust','--json').stdout)
        assert trusted['folder_trust_accepted'] and trusted['session_id'] == asked['session_id'], trusted
        assert (work / 'run/fixture-w-trust.trusted').exists() and trusted['exchange']['key'] == 't1'
        until(lambda: [text.splitlines()[-1] for text in received('fixture-w-trust')] == ['trust task'],'trusted worker received its task')
        # Without a command there is nothing to start, and nothing is made.
        assert 'no agent command' in cli('agent','start','w/none','-p','fixture','no command','--json',ok=False).stderr
        assert git(fixture,'show-ref','--verify','--quiet','refs/heads/w/none',ok=False).returncode != 0

        # The project's own default session command starts the agent when --agent is absent.
        (repos / 'other/wsx.config.yml').write_text('worktree:\n  defaultSession:\n    enabled: true\n    command: %s\n' % json.dumps(agent))
        other = json.loads(cli('agent','start','main','-p','other','--name','worker','hold the result','--json').stdout)
        assert not other['worktree_created'] and other['session_created'] and other['worktree'] == str(repos / 'other')
        # -p and -w restrict the exchange list.
        listed = lambda *scope: {item['id'] for item in json.loads(cli('agent','exchanges','--json',*scope).stdout)}
        assert listed('-p','other') == {other['exchange']['id']}
        assert listed('-p','fixture','-w','w/a') == {started_a['exchange']['id']}
        assert other['exchange']['id'] not in listed('-p','fixture') and other['exchange']['id'] in listed()
        cli('agent','exchanges','-p','absent','--json',ok=False)
        # One status call gives a manager the agent state, the exchange and the work of a worktree.
        (Path(started_a['worktree']) / 'draft.txt').write_text('not committed\n')
        git(started_b['worktree'],'commit','-q','--allow-empty','-m','work of b')
        status = json.loads(cli('status','--json').stdout)
        tree = lambda project, branch: next(worktree for item in status if item['name'] == project
                                            for worktree in item['worktrees'] if worktree['branch'] == branch)
        assert tree('fixture','w/a')['work'] == {'uncommitted':1,'unmerged_commits':0}
        assert tree('fixture','w/b')['work'] == {'uncommitted':0,'unmerged_commits':1}
        done = tree('fixture','w/a')['sessions'][0]
        assert done['agent_status'] == 'done' and done['exchange']['id'] == started_a['exchange']['id']
        assert done['exchange']['state'] == 'done_observed' and done['exchange']['key'] == 'a1'
        held = tree('other','main')['sessions'][0]
        assert held['agent_status'] == 'working' and held['exchange']['id'] == other['exchange']['id']
        assert held['exchange']['state'] == 'working_observed' and held['exchange']['round'] == 1
        shell = next(session for session in tree('fixture','w/retry')['sessions'] if session['session_id'] == retried['session_id'])
        assert shell['exchange'] is None
        # `wait --any` with a scope watches the exchanges that are active in it.
        assert 'wait_timeout' in cli('agent','wait','--any','-p','other','--timeout','1','--json',ok=False).stderr
        waiting = subprocess.Popen([str(wsx),'agent','wait','--any','-p','other','--result','--timeout','30','--json'],
            env=env,cwd=work,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        # The scope form watches what is active when it starts, so let it start before the release.
        time.sleep(1); assert waiting.poll() is None, 'the scoped wait returned before a boundary'
        (work / 'run/other.release').touch()
        stdout, stderr = waiting.communicate(timeout=40)
        assert waiting.returncode == 0, stderr
        scoped = json.loads(stdout)
        assert scoped['exchange']['id'] == other['exchange']['id'] and scoped['pending'] == []
        assert scoped['result']['text'] == 'result of other: hold the result'
        assert 'no_active_exchange' in cli('agent','wait','--any','-p','other','--json',ok=False).stderr
        assert 'one exchange' in cli('agent','wait',started_a['exchange']['id'],started_b['exchange']['id'],'--json',ok=False).stderr

        # An older wsxd (0.30.1, or a revision 28 build) refuses the keyed requests and
        # delivers nothing for them.
        # A new wsx hands an older wsxd over to its own build first. It keeps the older one
        # only while an open TUI of that release defers the handoff, as the polls here do.
        if (released / 'wsx').is_file() and (released / 'wsxd').is_file():
            (work / 'old').mkdir(mode=0o700); (work / 'old-state').mkdir(mode=0o700)
            for name in ('wsx','wsxd'): shutil.copy2(released / name, work / 'old' / name)
            old_path = work / 'old-state/wsx/wsx.sock'
            old_env = dict(env,XDG_STATE_HOME=str(work / 'old-state'),WSX_SOCKET=str(old_path),WSX_DAEMON_BIN=str(work / 'old/wsxd'))
            daemons.append(subprocess.Popen([str(work / 'old/wsxd')],env=old_env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL))
            until(old_path.exists,'released daemon')
            old_epoch = rpc(old_path,'snapshot')['data']['epoch']
            legacy = repos / 'legacy'
            subprocess.run(['git','init','-q','-b','main',str(legacy)],env=env,check=True,timeout=5)
            assert rpc(old_path,'synchronize_projects',{'projects':[{'name':'legacy','path':str(legacy),
                'worktrees':[{'path':str(legacy),'branch':'main'}]}]})['type'] == 'ack'
            created = rpc(old_path,'session_create',{'worktree_id':rpc(old_path,'snapshot')['data']['worktrees'][0]['id'],
                'label':'worker','command':[sys.executable,str(worker),str(work / 'run')],'rows':12,'cols':100})
            old_pane = next(session['focused_pane'] for session in rpc(old_path,'snapshot')['data']['sessions'] if session['id'] == created['data']['id'])
            until(lambda: next(pane for pane in rpc(old_path,'snapshot')['data']['panes'] if pane['id'] == old_pane)['agent'],'worker on the released daemon')
            def old_cli(*args, ok=True):
                assert rpc(old_path,'poll',{'after_revision':0,'timeout_ms':0})['type'] == 'events'
                return cli(*args,ok=ok,env=old_env)
            refused = old_cli('agent','request',old_pane,'keyed on an old daemon','--key','old-1','--json',ok=False)
            assert 'unsupported_request' in refused.stderr and 'wsx daemon restart' in refused.stderr, refused.stderr
            assert not received('legacy') and rpc(old_path,'agent_exchange_list',{})['data'] == []
            plain = json.loads(old_cli('agent','request',old_pane,'plain on an old daemon','--json').stdout)['exchange']
            until(lambda: len(received('legacy')) == 1 and rpc(old_path,'agent_exchange_get',{'exchange_id':plain['id']})['data']['exchange']['state'] == 'done_observed','plain request on the released daemon')
            refused = old_cli('agent','continue',plain['id'],'keyed follow up','--key','old-2','--json',ok=False)
            assert 'unsupported_request' in refused.stderr, refused.stderr
            assert len(received('legacy')) == 1 and rpc(old_path,'agent_exchange_get',{'exchange_id':plain['id']})['data']['exchange']['round'] == 1
            assert rpc(old_path,'snapshot')['data']['epoch'] == old_epoch, 'the older wsxd was replaced'
            skipped = []
        else:
            skipped = ['older binaries not found at %s: the old-daemon refusal was not checked' % released]
        success = True
    finally:
        repos.chmod(0o700)
        for daemon in daemons:
            if daemon.poll() is None:
                daemon.terminate()
                try: daemon.wait(timeout=5)
                except subprocess.TimeoutExpired: daemon.kill(); daemon.wait(timeout=3)
        # A worker ignores the hangup of its closed PTY so that it can finish a write.
        # Its PID file names a process that only this journey started.
        for record in (work / 'run').glob('*.pid'):
            try: os.kill(int(record.read_text()),signal.SIGKILL)
            except ProcessLookupError: pass
        if success: shutil.rmtree(work)
        else: print('diagnostics retained at '+str(work),file=sys.stderr)
    print(json.dumps({'result':'PASS','seconds':round(time.monotonic()-started,2),'model_calls':0,'skipped':skipped,
        'journey':'worktree retry without a leftover branch, session identities, prompt from file and standard input, slow delivery, once-per-key request and continuation, result of a round and its refusal, other changes during a slow delivery, delivery write limit, one-line manager with agent start and wait --any, late failure report, folder trust question, scoped exchanges, manager status, released-daemon refusal'}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--daemon', type=Path, default=ROOT / 'target/debug/wsxd')
    parser.add_argument('--only', choices=('context','orchestration'),
                        help='run one journey instead of both')
    parser.add_argument('--released', type=Path, default=Path('/opt/homebrew/Cellar/wsx/0.30.1/bin'),
                        help='directory of an older wsx and wsxd (0.30.1, or a revision 28 build) for the old-daemon refusal')
    args = parser.parse_args()
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    if not wsx.is_file() or not wsxd.is_file() or wsx.parent != wsxd.parent:
        raise RuntimeError('build adjacent wsx/wsxd first')
    if args.only != 'context':
        orchestration(wsx, wsxd, args.released)
    if args.only == 'orchestration':
        return
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
               'XDG_CACHE_HOME':str(work / 'cache'),'WSX_SOCKET':str(path),'WSX_DAEMON_BIN':str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()),
               'PATH':'/usr/bin:/bin:/usr/sbin:/sbin','SHELL':'/bin/sh','TERM':'xterm-256color','LANG':'en_US.UTF-8',
               'GIT_CEILING_DIRECTORIES':str(work)}
        subprocess.run(['git','init','-q','-b','main',str(work / 'project')],env=env,check=True,timeout=5)
        actor = work / 'actor.py'; actor.write_text(wsxd_client.fixture(ACTOR))
        def call(method, params=None): return rpc(path, method, params)
        wait = until
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
        # ^ docs/agent-reporting.md "Hook exit": a stale generation is expected and stays quiet.
        assert rejected.returncode == 0 and rejected.stderr == '', (rejected.returncode, rejected.stderr)
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
        # Refresh hooks after the actor opened. Observe the real reporter/daemon
        # without changing its process, generation or terminal draft.
        actor_pid = int((work / 'claude.pid').read_text())
        # An older install: guarded bindings beside the managed ones, an unrelated hook and the
        # retired 0.30.1 inspection files. Install keeps one managed binding per event.
        (work / 'home/.claude/hooks').mkdir(mode=0o700,parents=True)
        guarded = lambda action: ('if [ -x "$HOME/.claude/hooks/wsx-agent-status.sh" ]; then '
                                  f'"$HOME/.claude/hooks/wsx-agent-status.sh" {action}; fi')
        (work / 'home/.claude/settings.json').write_text(json.dumps({'hooks':{
            'PreToolUse':[{'matcher':'*','hooks':[{'type':'command','command':guarded('working')}]},
                          {'matcher':'Bash','hooks':[{'type':'command','command':'true owner-hook'}]}],
            'UserPromptSubmit':[{'matcher':'*','hooks':[{'type':'command','command':guarded('heartbeat'),'async':True}]}],
            'StopFailure':[{'matcher':'rate_limit','hooks':[{'type':'command','command':guarded('blocked')}]}]}}))
        retired = [work / 'home/.claude/hooks' / name
                   for name in ('wsx-agent-inspect.py','wsx-inspect-enabled','wsx-inspect.jsonl')]
        for file in retired: file.touch(mode=0o600)
        subprocess.run([str(wsx),'agent','install','claude'],env=env,
                       check=True,capture_output=True,text=True,timeout=5)
        claude_config = json.loads((work / 'home/.claude/settings.json').read_text())
        installed = [(event,hook['command']) for event,entries in claude_config['hooks'].items()
                     for entry in entries for hook in entry['hooks']]
        assert not any('if [ -x' in command for _,command in installed), installed
        assert len(installed) == len(set(installed)) and ('PreToolUse','true owner-hook') in installed
        assert not any(file.exists() for file in retired), 'install kept retired inspection files'
        claude_commands = {state:claude_config['hooks'][event][0]['hooks'][0]['command']
                           for state,event in [('idle','SessionStart'),('done','Stop')]}
        private = 'private-inspection-sentinel'
        claude_env = dict(env,WSX_PANE_ID=str(claude),WSX_RUNTIME_GENERATION=generation,
                          WSX_AGENT_REPORT_BIN=str(wsx),CLAUDE_CODE_CHILD_SESSION=private,
                          CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=private)
        def claude_hook(state, extra=None, missing_generation=False, fields=None, command=None):
            selected = dict(claude_env,**(extra or {}))
            if missing_generation: selected.pop('WSX_RUNTIME_GENERATION')
            return subprocess.run(['/bin/sh','-c',command or claude_commands[state]],
                input=json.dumps({'session_id':'fixture','transcript_path':str(work / 'claude.jsonl'),
                                  'prompt':private*8192,'credential':private,**(fields or {})}),
                env=selected,capture_output=True,text=True,timeout=5)
        def claude_agent():
            return next(pane for pane in call('snapshot')['data']['panes'] if pane['id'] == claude)['agent']
        quiet = claude_hook('done')
        assert quiet.returncode == 0 and quiet.stderr == '' and claude_agent()['state'] == 'done'
        # Claude starts its session again after every compaction (SessionStart, source "compact"),
        # also an automatic one inside a turn (real transcripts and hooks, Claude Code 2.1.300); that
        # start is not reported. Every compaction works from its PreCompact to its PostCompact: one
        # inside a turn leaves the turn working, one outside a turn (the person's /compact, or Claude
        # Code 2.1.310's idle compaction, 37 to 55 s in real transcripts) returns to the state before
        # it. Every installed command of an event runs, as Claude runs them.
        def hook_states(steps):
            seen = []
            for event, fields in steps:
                for entry in claude_config['hooks'][event]:
                    # Claude does not wait for an async hook (the heartbeat).
                    for hook in (hook for hook in entry['hooks'] if not hook.get('async')):
                        result = claude_hook(None, command=hook['command'], fields={'hook_event_name':event, **fields})
                        assert result.returncode == 0 and result.stderr == '', (event, result.stderr)
                seen.append(claude_agent()['state'])
            return seen
        assert hook_states([('UserPromptSubmit', {'prompt_id':'turn-1'}),
                            ('PreCompact', {'trigger':'auto'}), ('SessionStart', {'source':'compact'}), ('PostCompact', {'trigger':'auto'}),
                            ('Stop', {}),
                            ('PreCompact', {'trigger':'manual'}), ('SessionStart', {'source':'compact'}), ('PostCompact', {'trigger':'manual'}),
                            ('PreCompact', {'trigger':'auto'}), ('SessionStart', {'source':'compact'}), ('PostCompact', {'trigger':'auto'}),
                            ('SessionStart', {'source':'startup'})]) == ['working', 'working', 'working', 'working', 'done',
                                                                         'working', 'working', 'done', 'working', 'working', 'done', 'idle']
        # An idle compaction holds a delivery as a turn does, and the agent is idle again after it.
        assert hook_states([('PreCompact', {'trigger':'auto'})]) == ['working'] and claude_agent()['compacting']
        assert 'agent_busy' in cli('agent','request',claude,'denied','--stash-draft','--json',ok=False).stderr
        assert hook_states([('SessionStart', {'source':'compact'}), ('PostCompact', {'trigger':'auto'})]) == ['working', 'idle']
        assert not claude_agent().get('compacting')
        # ^ docs/agent-reporting.md "Hook exit": expected rejections stay quiet and change nothing.
        assert claude_hook('idle').returncode == 0 and claude_agent()['state'] == 'idle'
        before_hook_rejection = claude_agent()
        for extra, missing_generation in [({'WSX_RUNTIME_GENERATION':'stale-'+private}, False),
                                          (None, True),
                                          ({'WSX_SOCKET':str(work / 'gone.sock')}, False)]:
            result = claude_hook('done', extra, missing_generation=missing_generation)
            assert result.returncode == 0 and result.stderr == '', (extra, result.stderr)
            assert claude_agent() == before_hook_rejection
        assert not (work / 'gone.sock').exists(), 'a quiet report started wsxd'
        # Restore this private actor's deliberate prompt capability for the
        # existing unsupported-provider draft-policy journey below.
        assert call('agent_report',{'pane_id':codex,'runtime_generation':codex_generation,
            'provider':'codex','state':'idle','session_ref':{'kind':'id','value':'fixture'},
            'capabilities':{'prompt':True,'lifecycle':True}})['type'] == 'ack'
        assert int((work / 'claude.pid').read_text()) == actor_pid
        assert (work / 'claude.generation').read_text() == generation
        os.kill(actor_pid,0)
        assert 'unsent draft' in cli('session','peek',claude,'--trim').stdout and not traces('claude')
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
        assert 'exchange_busy' in cli('agent','request',claude,'must wait for native completion','--json',ok=False).stderr
        assert 'wait_timeout' in cli('agent','wait',exchange['id'],'--timeout','1','--json',ok=False).stderr
        completed = json.loads(receipt('completed',bound=True).stdout)['exchange']
        assert completed['state'] == 'completed' and completed['evidence'] == 'request_bound'
        assert json.loads(receipt('completed',bound=True).stdout)['exchange']['revision'] == completed['revision']
        cli('session','send-text',claude,'\x13','--no-enter')
        wait(lambda: 'unsent draft' in cli('session','peek',claude,'--trim').stdout,'restore draft before continuation')
        # Claude's own layout: the two-line draft sits above seven footer rows.
        cli('session','send-text',claude,'\x06','--no-enter')
        wait(lambda: 'general-purpose' in cli('session','peek',claude,'--trim').stdout,'tall footer drawn')
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
        cli('session','send-text',claude,'\x06','--no-enter')
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
        assert call('agent_exchange_get',{'exchange_id':exchange['id']})['data']['exchange']['state'] == 'expired'
        cli('session','send-text',claude,'\x13\x11','--no-enter')
        wait(lambda: 'unsent draft' in cli('session','peek',claude,'--trim').stdout,'restored draft with unsupported binding')
        failed = json.loads(cli('agent','continue',exchange['id'],'must not arrive','--stash-draft','--json').stdout)['exchange']
        assert failed['round'] == 2, 'failed continuation did not invalidate the previous round'
        assert failed['state'] == 'delivery_failed' and failed['evidence'] == 'intent_persisted'
        assert failed.get('delivery_sha256') is None, 'failed delivery retained input-binding authority'
        assert failed.get('native_input_id') is None, 'failed continuation retained native acceptance'
        assert len(traces('claude')) == 5, 'unconfirmed stash delivered a prompt'
        # A wake heartbeat ends within seconds of its prompt, pane or wsxd. Claude starts it from
        # a detached hook shell and never ends it: docs/agent-reporting.md "Wake heartbeat".
        beat_command = next(hook['command'] for group in claude_config['hooks']['UserPromptSubmit']
                            for hook in group['hooks'] if hook['command'].endswith(' heartbeat'))
        def heartbeat(prompt, pane=claude, pane_generation=generation):
            working = dict(report,pane_id=pane,runtime_generation=pane_generation,state='working',wake_token=prompt)
            assert call('agent_report',working)['type'] == 'ack'
            shell = subprocess.Popen(['/bin/sh','-c',beat_command],stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,start_new_session=True,
                env=dict(claude_env,WSX_PANE_ID=str(pane),WSX_RUNTIME_GENERATION=pane_generation))
            shell.stdin.write(json.dumps({'prompt_id':prompt}).encode()); shell.stdin.close()
            time.sleep(1)
            assert shell.poll() is None, 'heartbeat did not start'
            return shell
        def ended(shell, label):
            wait(lambda: shell.poll() is not None, label, 12)
        done_beat = heartbeat('prompt-done')
        assert call('agent_report',dict(report,state='done'))['type'] == 'ack'
        ended(done_beat,'heartbeat of a finished prompt')
        replaced, last_beat = heartbeat('prompt-replaced'), heartbeat('prompt-last')
        ended(replaced,'heartbeat of a replaced prompt')
        assert last_beat.poll() is None, 'the current heartbeat ended with the replaced one'
        codex_generation = (work / 'codex.generation').read_text()
        pane_beat = heartbeat('prompt-pane', codex, codex_generation)
        codex_session = next(pane for pane in call('snapshot')['data']['panes'] if pane['id'] == codex)['session_id']
        cli('session','delete',codex_session,'--json')
        ended(pane_beat,'heartbeat of a deleted pane')
        assert last_beat.poll() is None
        assert call('shutdown')['type'] == 'ack'
        daemon.wait(timeout=5)
        assert not path.exists() and daemon.returncode == 0
        ended(last_beat,'heartbeat after wsxd stopped')
        assert not path.exists(), 'a heartbeat started wsxd again'
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
        'journey':'installed shell/JS reporting, current/stale generation, unchanged actor, metadata/native history, scoped bounds, lease/provider refusal, preserved draft/continuation, capability/generation/round-fenced request-bound receipts, lifecycle distinct from completion, duplicate receipt idempotence, wake heartbeat ends with its prompt, pane and daemon, no bootstrap, private cleanup'}))

if __name__ == '__main__': main()
