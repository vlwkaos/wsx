#!/usr/bin/env python3
"""Model-free journey for the hosted web client (docs/web-client.md).

Real wsxd, real PTY fixture agents, three `wsx web serve` processes and a local
Web Push receiver. Nothing here touches the user's daemon, Tailscale or network.
"""
import argparse
import base64
import contextlib
import hashlib
import http.client
import json
import os
import re
from pathlib import Path
import shutil
import signal
import socket
import struct
import urllib.parse
import subprocess
import sys
import threading
import time
import zlib

import wsxd_client

ROOT = Path(__file__).resolve().parents[1]

# A fixture agent: reports as Claude, records what the PTY delivered, and writes
# its own native transcript. The web server may only learn replies from that file.
ACTOR = r'''
import json, os, pathlib, re, select, signal, socket, sys, termios, time, tty
signal.signal(signal.SIGHUP, signal.SIG_IGN)
history, trace, project = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
# Claude unless the test asks for Pi, whose records chain by id and parentId.
provider = os.environ.get('WSX_FIXTURE_PROVIDER', 'claude')
pane = int(os.environ['WSX_PANE_ID']); generation = os.environ['WSX_RUNTIME_GENERATION']
trace.with_suffix('.pid').write_text(str(os.getpid()))
# The conversation the agent reports; a test sets it to start another one.
conversation = os.environ.get('WSX_FIXTURE_CONVERSATION', 'fixture')
cooked = termios.tcgetattr(0)
tty.setraw(0)
# A test that writes an older transcript first names its newest record, so the agent goes on from it.
draft, pending, leaf, sequence, dialog, taken, working = '', b'', os.environ.get('WSX_FIXTURE_LEAF'), 0, None, None, False
def report(state, attached=True, compacting=False):
 # ^ As Claude's hook does, the first Working report of a turn says which prompt it took.
 global taken, working
 with socket.socket(socket.AF_UNIX) as client:
  client.connect(os.environ['WSX_SOCKET']); stream = client.makefile('rb')
  client.sendall(b'{"method":"hello","params":{"protocol":@PROTOCOL@}}\n'); stream.readline()
  params = {'pane_id':pane,'runtime_generation':generation,'provider':provider,'state':state,'attached':attached,
    'session_ref':{'kind':'id','value':conversation,'transcript_path':str(history)},
    'capabilities':{'prompt':True,'lifecycle':True,'prompt_receipts':provider == 'claude'}}
  if state == 'working' and taken:
   params['received'], taken = taken, None
  if compacting: params['compacting'] = True
  client.sendall(json.dumps({'method':'agent_report','params':params}).encode()+b'\n')
  assert json.loads(stream.readline())['type'] == 'ack'
 if working != (state == 'working'):
  working = state == 'working'; paint()
def paint():
 if dialog:
  # A provider dialog owns the screen until it is answered.
  sys.stdout.write('\x1b[?2004h\x1b[2J\x1b[H'+'\r\n'.join(dialog)); sys.stdout.flush(); return
 # Claude Code 2.1.308's layout: the working line, the editor between two borders, the footer.
 lines = ('❯ '+draft).split('\n')
 box = (['✻ Sautéing… (esc to interrupt)', ''] if working else []) + ['─'*40, lines[0]] + ['  '+line for line in lines[1:]] \
   + ['─'*40, '  [S] fixture>main | 3.0%', '  ⏵⏵ auto mode on (shift+tab to cycle)']
 rows = os.get_terminal_size().lines
 sys.stdout.write('\x1b[?2004h\x1b]2;✳ fixture\x07\x1b[2J\x1b['+str(max(1,rows-len(box)+1))+';1H'+'\r\n'.join(box)+'\x1b[J')
 sys.stdout.flush()
def record(kind, content, **extra):
 global leaf, sequence
 sequence += 1; entry = kind+'-'+str(sequence)
 stamp = time.strftime('%Y-%m-%dT%H:%M:%S', time.gmtime()) + '.%03dZ' % (time.time() % 1 * 1000)
 if provider == 'pi':
  line = {'type':'message','id':entry,'parentId':leaf,'timestamp':stamp,'message':{'role':kind,'content':content}}
 else:
  line = {'type':kind,'uuid':entry,'parentUuid':leaf,'sessionId':conversation,'timestamp':stamp,'message':{'role':kind,'content':content}}
 # Both providers name the model on each reply (real transcripts, 2026-10-20).
 if kind == 'assistant': line['message']['model'] = 'gpt-6.1-sol' if provider == 'pi' else 'claude-opus-5-5'
 line.update(extra)
 with history.open('a') as out: out.write(json.dumps(line)+'\n')
 leaf = entry
def compact():
 # A compaction as each provider saves it (real transcripts, 2026-10): Claude starts a new
 # chain at a boundary record and wraps the summary in fixed words; Pi keeps the chain.
 global leaf, sequence
 summary = '## Goal\nFix the login redirect loop on mobile Safari.\n\n' + '\n'.join('- Step %d of the fix is done.' % index for index in range(1, 13))
 if provider == 'pi':
  sequence += 1; entry = 'compaction-'+str(sequence)
  with history.open('a') as out: out.write(json.dumps({'type':'compaction','id':entry,'parentId':leaf,'summary':summary})+'\n')
  leaf = entry
  return
 sequence += 1; boundary = 'boundary-'+str(sequence)
 with history.open('a') as out:
  out.write(json.dumps({'type':'system','subtype':'compact_boundary','uuid':boundary,'parentUuid':None,'logicalParentUuid':leaf,
    'sessionId':'fixture','content':'Conversation compacted'})+'\n')
 leaf = boundary
 record('user', 'This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n'
   + summary + '\n\nIf you need specific details from before compaction (like exact code snippets, error messages, or content you generated), read the full transcript at: /home/fixture/.claude/projects/x.jsonl\n'
   + 'Continue the conversation from where it left off without asking the user any further questions.', isCompactSummary=True, isVisibleInTranscriptOnly=True)
def claude_compact():
 # Claude Code 2.1.300's /compact, as a real transcript holds it (2026-10-15): no prompt and no
 # Stop. The wsx hooks report its PreCompact and PostCompact (trigger "manual") and skip the
 # SessionStart with source "compact". The command goes on the old chain; a boundary starts a
 # new one and names the newest old records it kept, which come after the summary.
 global leaf, sequence
 report('working', compacting=True)
 kept = leaf
 record('user', '/compact')
 sequence += 1; boundary, summary = 'boundary-%d' % sequence, 'summary-%d' % sequence
 with history.open('a') as out:
  out.write(json.dumps({'type':'system','subtype':'compact_boundary','uuid':boundary,'parentUuid':None,'logicalParentUuid':leaf,
    'sessionId':'fixture','content':'Conversation compacted',
    'compactMetadata':{'trigger':'manual','preservedSegment':{'headUuid':kept,'anchorUuid':summary,'tailUuid':kept}}})+'\n')
  out.write(json.dumps({'type':'user','uuid':summary,'parentUuid':boundary,'sessionId':'fixture','isCompactSummary':True,
    'message':{'role':'user','content':'This session is being continued from a previous conversation that ran out of context. '
    'The summary below covers the earlier portion of the conversation.\n\nSummary:\n## Goal\nRun the tests.'}})+'\n')
 leaf = summary
 record('user', '<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>', isMeta=True)
 record('user', '<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args></command-args>')
 record('user', '<local-command-stdout>\x1b[2mCompacted (ctrl+o to see full summary)\x1b[22m</local-command-stdout>')
 # ^ Long enough for the test to read the working row while it compacts.
 time.sleep(1.5)
 report('done')
def steered(text):
 global leaf, sequence, taken
 sequence += 1; entry = 'queued-'+str(sequence)
 stamp = time.strftime('%Y-%m-%dT%H:%M:%S', time.gmtime()) + '.%03dZ' % (time.time() % 1 * 1000)
 with history.open('a') as out:
  out.write(json.dumps({'type':'attachment','uuid':entry,'parentUuid':leaf,'sessionId':conversation,'timestamp':stamp,
    'attachment':{'type':'queued_command','prompt':text,'commandMode':'prompt','origin':{'kind':'human'}}})+'\n')
 leaf = entry
 with trace.open('a') as out: out.write(json.dumps({'steered':text})+'\n')
 taken = {'kind':'other'}; report('working')
class Stopped(Exception): pass
def gate(name):
 # The test opens each gate, so it can read the timeline between two records.
 # Ctrl+C while the turn waits here ends the turn, as an interrupt does in a real agent.
 # Other keys go into the editor, as a person types the next line while Claude works.
 global draft
 while not trace.with_suffix('.'+name).exists():
  if select.select([0],[],[],0.02)[0]:
   data = os.read(0,4096)
   if b'\x03' in data:
    report('idle'); raise Stopped()
   # ^ Claude takes a line entered while it works at its next step and saves it as a
   # queued_command, after UserPromptSubmit (Claude Code 2.1.309, a real capture).
   typed = data.decode(errors='replace').replace('\x1b[200~', '').replace('\x1b[201~', '')
   if '\r' in typed and provider == 'claude':
    draft += typed.split('\r')[0]
    if draft.strip(): steered(draft)
    draft = ''; paint(); continue
   draft += typed; paint()
 trace.with_suffix('.'+name).unlink()
def turn(text):
 global taken
 with trace.open('a') as out: out.write(json.dumps({'received':text})+'\n')
 envelope = re.match(r'\[wsx exchange (\d+), round (\d+), ', text)
 if provider == 'claude':
  taken = {'kind':'round','exchange':int(envelope[1]),'round':int(envelope[2])} if envelope else {'kind':'other'}
 if provider == 'claude' and text == '/compact':
  # ^ Claude's /compact starts no UserPromptSubmit, so no receipt (real Claude, 2026-10-21).
  taken = None; claude_compact(); return
 # A paste the agent drops, as Claude did at the end of a turn (2026-10-21): it works and
 # ends, but its conversation never shows the message. Only the first time.
 if 'drop me once' in text and not trace.with_suffix('.dropped').exists():
  # ^ A dropped paste starts no turn: the hook never runs, so nothing is reported.
  trace.with_suffix('.dropped').touch(); taken = None; return
 if 'drop me always' in text:
  taken = None; return
 # Claude Code 2.1.304's shell mode, as a real transcript holds `! echo pasted-bang-ok` (2026-10-21):
 # the line without its `!`, then the output, then a reply of the model.
 if provider == 'claude' and text.startswith('!'):
  record('user', '<bash-input>'+text[1:]+'</bash-input>')
  record('user', '<bash-stdout>SHELL-OUTPUT</bash-stdout><bash-stderr></bash-stderr>')
  report('working'); time.sleep(0.2)
  record('assistant', 'the shell line ran'); report('done'); return
 # The person's prompt as Claude Code 2.1.300 marks it; other turns keep the older record without the field.
 # Claude Code 2.1.300 can save a pasted prompt inside a pasted_content wrapper (real transcript, 2026-10-20).
 saved = '\n\n<pasted_content id="4666">\n'+text+'\n</pasted_content id="4666">\n' if provider == 'claude' and 'follow up' in text else text
 # ^ Claude runs its prompt hook, which reports the turn and the prompt it took, before it saves
 # the prompt (Claude Code 2.1.310); a slow hook or a slow write leaves a gap between the two.
 report('working')
 if 'slow save' in text: gate('saved')
 record('user', saved, **({'promptSource':'typed'} if 'task notice' in text else {}))
 body = text.split('\n',1)[1] if text.startswith('[wsx exchange ') else text
 if 'make chart' in body:
  (project/'out').mkdir(exist_ok=True)
  (project/'out'/'chart.png').write_bytes(pathlib.Path(sys.argv[4]).read_bytes())
  record('assistant', 'Saved the chart to `out/chart.png`.')
 elif 'leak' in body:
  record('assistant', 'See out/leak.png and ../outside.png and '+sys.argv[5]+' and .env and out/api-token.txt and .git/config.')
 elif 'use tool' in body:
  # A real Claude turn in its saved form: thinking and a tool call, the tool result, the answer.
  record('assistant', [{'type':'thinking','thinking':'private reasoning','signature':'x'},
    {'type':'tool_use','id':'call-'+str(sequence),'name':'Bash','input':{'command':'ls -la\nrm -rf x','description':'List the files\x07'}}])
  call = 'call-'+str(sequence-1)
  gate('result')
  record('user', [{'type':'tool_result','tool_use_id':call,'is_error':'broken' in body,'content':'tool output text'}])
  gate('answer')
  record('assistant', 'Listed the files.')
 elif 'long talk' in body:
  # A long saved conversation: many turns, runs of tool calls between them, a very long word.
  for index in range(1, 25):
   record('user', 'Question %d: what does step %d of the release checklist cover?' % (index, index))
   calls = [{'type':'tool_use','id':'long-%d-%d' % (index, step),'name':('Read','Grep','Bash','Edit')[step % 4],
     'input':{'description':'Check item %d of step %d' % (step + 1, index)}} for step in range(index % 4 + 1)]
   record('assistant', [{'type':'thinking','thinking':'private','signature':'x'}] + calls)
   record('user', [{'type':'tool_result','tool_use_id':call['id'],'is_error':index == 7,'content':'ok'} for call in calls])
   record('assistant', 'Step %d covers the build, the tests and the notes.\nIt has %d checks.\n\n%s' % (index, index % 4 + 1, 'W' * 300 if index == 12 else 'Nothing unusual was found.'))
 elif body.startswith('stream '):
  # A turn as Claude saves it while it works: a record about every quarter second,
  # text blocks with code, and now and then a tool call and its result.
  for index in range(int(body.split()[1])):
   record('assistant', 'Part %d: the parser keeps **every** field.\n\n```python\nprint(%d)\n```' % (index, index))
   if index % 4 == 3:
    record('assistant', [{'type':'tool_use','id':'stream-%d' % index,'name':'Read','input':{'description':'Read part %d' % index}}])
    record('user', [{'type':'tool_result','tool_use_id':'stream-%d' % index,'content':'ok'}])
   time.sleep(0.25)
  record('assistant', 'The stream is done.')
 elif body.startswith('say '):
  # An answer written by the test, for the markdown and diagram checks.
  record('assistant', trace.with_name(body[4:].strip()).read_text())
 elif 'compact now' in body:
  compact()
 elif 'task notice' in body:
  # Claude Code 2.1.300 writes a background-task notice in the person's name and marks it as its own.
  record('user', '<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n<summary>Background command "Run the tests" completed (exit code 0)</summary>\n</task-notification>',
    promptSource='system', origin={'kind':'task-notification'}, turnOrigin='task_notification')
  record('assistant', 'The tests passed.')
 elif 'leave now' in body:
  # As Claude ends: its SessionEnd hook says it is detached, the terminal is restored, it exits.
  record('assistant', 'leaving'); report('unknown', attached=False)
  termios.tcsetattr(0, termios.TCSADRAIN, cooked); os._exit(0)
 elif 'quit now' in body:
  report('working'); os._exit(3)
 elif 'ask me' in body:
  # A dialog as a provider draws it: numbered choices, a cursor mark, answered with keys.
  global dialog
  call = 'ask-'+str(sequence + 1)
  if 'permission' in body:
   record('assistant', [{'type':'tool_use','id':call,'name':'Bash','input':{'command':'rm -rf build','description':'Remove the build folder'}}])
   head = ['╭──────────────────────────────────────────────╮', '│ Bash command                                 │',
     '│   rm -rf build                               │', '│   Remove the build folder                    │',
     '╰──────────────────────────────────────────────╯', ' Do you want to proceed?']
   choices = ['Yes', "Yes, and don't ask again for rm commands in this project", 'No, and tell Claude what to do differently (esc)']
  else:
   question = 'Which option should I take for the login redirect?'
   choices = ['Keep the session cookie', 'Use a bearer token', 'Ask the server on each request', 'Type something else']
   record('assistant', [{'type':'tool_use','id':call,'name':'AskUserQuestion','input':{'questions':[{'question':question,
     'options':[{'label':choice} for choice in choices]}]}}])
   head = [' 1. An earlier list in the answer', ' 2. is not a choice', '', ' ' + question]
  # ^ Claude Code 2.1.309 at 40 columns wraps its hint line (a real capture, 2026-10-23).
  hint = [' Esc to cancel'] if 'permission' in body else ['Enter to select · ↑/↓ to navigate · Esc', 'to cancel']
  cursor = 0
  def draw():
   global dialog
   dialog = head + [(' ❯ ' if index == cursor else '   ') + '%d. %s' % (index + 1, choice) for index, choice in enumerate(choices)] + [''] + hint
   paint()
  # ^ Claude has no hook for a dialog: it reports Working from PreToolUse, and wsxd reads
  # the dialog from the screen (docs/agent-reporting.md).
  draw(); report('working')
  while True:
   data = os.read(0, 4096)
   with trace.with_suffix('.answers').open('a') as out: out.write(json.dumps({'keys': data.hex()})+'\n')
   if data in (b'\x1b[A', b'\x1bOA'): cursor = (cursor - 1) % len(choices); draw()
   elif data in (b'\x1b[B', b'\x1bOB'): cursor = (cursor + 1) % len(choices); draw()
   elif data == b'\x03': dialog = None; paint(); report('idle'); return
   elif data == b'\x1b': answer = 'cancelled'; break
   elif data == b'\r': answer = choices[cursor]; break
   elif data.isdigit() and 1 <= int(data) <= len(choices): answer = choices[int(data) - 1]; break
   elif len(data) > 1 and data.endswith(b'\r'): answer = 'typed: ' + data[:-1].decode(); break
  # The dialog stays while the provider handles the answer.
  if 'slowly' in body: time.sleep(2.5)
  dialog = None
  record('user', [{'type':'tool_result','tool_use_id':call,'is_error':answer == 'cancelled','content':answer}])
  record('assistant', 'You chose: ' + answer)
  paint()
 elif 'slow editor' in body:
  # Claude reports the end of a turn, then draws its idle editor a moment later.
  time.sleep(1)
  record('assistant', 'Done, slowly.')
  sys.stdout.write('\x1b[2J'); sys.stdout.flush()
  report('done'); time.sleep(3); paint(); return
 elif 'fail' in body:
  report('error'); return
 else:
  record('assistant', 'echo: '+body.splitlines()[0])
 time.sleep(0.2)
 report('done')
# Like a real agent TUI, repaint after a resize: a web terminal resizes the pane.
signal.signal(signal.SIGWINCH, lambda *_: paint())
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
  if char == b'\r':
   text, draft = draft, ''; paint()
   try: turn(text)
   except Stopped: pass
  elif char == b'\x03':
   report('idle')
  else:
   draft += char.decode(errors='replace'); paint()
'''

# An answer with every markdown form the page renders, and one with diagrams.
MARKDOWN_SAMPLE = r'''# Release plan

The build is **ready** and the notes are *almost* done. ~~The old plan~~ is gone.
Run `cargo nextest run --workspace --locked` first, then read [the guide](https://example.com/guide)
or https://example.com/changelog. A [script link](javascript:alert(1)) and a [local link](docs/web-client.md) stay text.

## Steps

1. Build both binaries
   - `wsx`
   - `wsxd`
     1. debug
     2. release
2. Run the journey

- [x] Vendor the parser
- [ ] Draw the diagrams

> A quote with `code` in it.
> It has two lines.

### Code

```rust
// The answer is checked against the screen that was read.
fn answer(stamp: &str, key: Key) -> Result<(), Fault> {
    let screen = read_screen()?; // a very long line follows so that the block must scroll sideways inside itself and never widen the page
    if hash(&screen) != stamp { return Err(Fault::new(409, "question_changed", "the question changed")); }
    Ok(())
}
```

```
plain block without a language
```

| Part | Change | Files | Tests | Bytes | Status |
|------|--------|------:|------:|------:|:------:|
| 1 | Markdown in answers | 6 | 14 | 197447 | done |
| 2 | Mermaid diagrams | 4 | 9 | 3572661 | done |
| 3 | Answer a blocked agent from chat | 7 | 21 | 0 | open |

<b>Raw HTML stays text</b> <img src="https://example.com/x.png" onerror="alert(1)">

![A remote picture](https://example.com/remote.png)

---

#### Small heading

WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW
'''

DIAGRAM_SAMPLE = r'''The page talks to wsxd through the web server:

```mermaid
flowchart LR
  A[Browser page] -->|WebSocket| B(wsx web serve)
  B --> C{wsxd}
  C -->|PTY bytes| D[Agent pane]
  C --> E[(State)]
```

An answer is checked against the screen:

```mermaid
sequenceDiagram
  participant P as Page
  participant S as Server
  participant D as wsxd
  P->>S: POST answer {stamp, key}
  S->>D: view, then terminal_input
  D-->>S: ack
  S-->>P: 200 sent
  Note over P,S: A stale stamp answers 409
```

This one is broken:

```mermaid
flowchart LR
  A -->
```
'''

# A terminal fixture: raw mode, records every input byte, answers with its hex
# in angle brackets, and reports its size after each resize.
TERMINAL = r'''
import fcntl, os, pathlib, signal, struct, sys, termios, tty
trace = pathlib.Path(sys.argv[1])
tty.setraw(0)
def winch(*_):
 rows, cols = struct.unpack('hhhh', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\0' * 8))[:2]
 os.write(1, b'\r\n[size %dx%d]\r\n' % (cols, rows))  # its own line: a replay wraps long lines
signal.signal(signal.SIGWINCH, winch)
os.write(1, b'term-ready')
while True:
 data = os.read(0, 65536)
 if not data: break
 with trace.open('ab') as out: out.write(data)
 if data == b'CLIPS':  # OSC 52 (set the clipboard to "copied") and a tick, 20 times
  import threading, time
  def ticks():
   for n in range(20):
    os.write(1, b'\x1b]52;c;Y29waWVk\x07[tick%d]' % n)
    time.sleep(0.25)
  threading.Thread(target=ticks, daemon=True).start()
 os.write(1, b'<' + data.hex().encode() + b'>')
'''

# Programs for the real-browser terminal check, run from a shell in the pane.
TOOLS = {
    'colors': r'''
import sys
out = sys.stdout.write
for row in range(8):
    out(''.join('\x1b[48;5;%dm ' % (row * 32 + cell) for cell in range(32)) + '\x1b[0m\n')
out(''.join('\x1b[48;2;%d;%d;%dm ' % (255 - step * 8, step * 8, 128) for step in range(30)))
out('\x1b[48;2;18;52;86m  \x1b[0m\n')
out('\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \x1b[7minverse\x1b[0m \x1b[38;5;208morange\x1b[0m\n')
out('truecolor-end\n')
''',
    'query': r'''
import os, select, termios, tty
saved = termios.tcgetattr(0)
tty.setraw(0)
os.write(1, b'\x1b[c')
replies = b''
while select.select([0], [], [], 0.7)[0]:
    replies += os.read(0, 1024)
termios.tcsetattr(0, termios.TCSADRAIN, saved)
print('da-replies=%d' % replies.count(b'c'))
''',
    'long': r'''
for index in range(1, 301):
    print('line-%03d' % index)
print('long-end')
''',
    'alt': r'''
import os, re, termios, tty
saved = termios.tcgetattr(0)
tty.setraw(0)
counts = dict.fromkeys(('up', 'down', 'left', 'right', 'wheel-up', 'wheel-down'), 0)
mouse = False
def paint():
    # Two short lines: a phone screen is narrower than one long status line.
    items = ['%s=%d' % item for item in counts.items()]
    lines = [' '.join(items[:4]), ' '.join(items[4:]) + ' mouse=' + ('on' if mouse else 'off')]
    os.write(1, ('\x1b[H\x1b[2J\x1b[7m ALT SCREEN \x1b[0m\r\n\r\n' + '\r\n'.join(lines) + '\r\n\r\nm: mouse tracking   q: quit').encode())
os.write(1, b'\x1b[?1049h')
paint()
while True:
    data = os.read(0, 4096)
    if not data or b'q' in data:
        break
    if b'm' in data and not mouse:
        mouse = True
        os.write(1, b'\x1b[?1000h\x1b[?1006h')
    for name, sequence in (('up', b'\x1b[A'), ('down', b'\x1b[B'), ('right', b'\x1b[C'), ('left', b'\x1b[D'),
                           ('up', b'\x1bOA'), ('down', b'\x1bOB'), ('right', b'\x1bOC'), ('left', b'\x1bOD')):
        counts[name] += data.count(sequence)
    counts['wheel-up'] += len(re.findall(rb'\x1b\[<64;', data))
    counts['wheel-down'] += len(re.findall(rb'\x1b\[<65;', data))
    paint()
os.write(1, b'\x1b[?1000l\x1b[?1006l\x1b[?1049l')
termios.tcsetattr(0, termios.TCSADRAIN, saved)
print('alt-done')
''',
    'glyphs': r'''
import sys
out = sys.stdout.write
# A prompt as Powerline themes draw it: colored segments joined by U+E0B0, thin separators, Nerd Font icons.
out('\x1b[48;5;24;38;5;255m \uf015 home \x1b[48;5;238;38;5;24m\ue0b0\x1b[38;5;255m \ue0a0 main \ue0b1 \uf07c src \x1b[49;38;5;238m\ue0b0\x1b[0m \ue0b2\ue0b3 \ue7a8 \uf121\n')
out('=> != -> === <= >= :: ++ |> www\n')
out('\u250c\u2500\u2500\u252c\u2500\u2500\u2510\n\u2502ab\u2502cd\u2502\n\u2514\u2500\u2500\u2534\u2500\u2500\u2518\n')
out('\x1b[1mbold text\x1b[0m \x1b[3mitalic text\x1b[0m plain text\n')
out('\ud55c\uae00|\nabcd|\n')
out('glyphs-end\n')
''',
    'keys': r'''
import os, pathlib, termios, tty
saved = termios.tcgetattr(0)
tty.setraw(0)
os.write(1, b'keys-ready\r\n')
with (pathlib.Path(__file__).parent / 'keys.trace').open('wb', buffering=0) as trace:
    while True:
        data = os.read(0, 4096)
        trace.write(data)
        if not data or b'Q' in data:
            break
termios.tcsetattr(0, termios.TCSADRAIN, saved)
print('keys-done')
''',
}


def png(size, shade):
    """A valid, deterministic PNG of roughly size*size*3 raw bytes."""
    raw = b''.join(b'\x00' + bytes((shade + x * 7 + y * 13) % 256 for x in range(size * 3)) for y in range(size))
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw, 0)) + chunk(b'IEND', b''))


# ^ A run can be killed with SIGKILL (a tool timeout), and then no cleanup runs. So every
# helper gets its own process group, recorded in the run's work folder, and the next run
# ends whatever a dead run left (docs/web-client.md "Verification").
RECORD = 'processes'
STARTED = []


def start(work, args, **options):
    """Start a helper in its own process group and record it. Chrome started by a Node
    check stays in the check's group."""
    process = subprocess.Popen(args, start_new_session=True, **options)
    STARTED.append(process)
    with (work / RECORD).open('a') as out:
        out.write('%d\t%s\n' % (process.pid, ' '.join(map(str, args))))
    return process


def stop(process, grace=5):
    """SIGTERM to the helper's group, then SIGKILL to what is left."""
    for signal_number, wait_seconds in ((signal.SIGTERM, grace), (signal.SIGKILL, 3)):
        try:
            os.killpg(process.pid, signal_number)
        # ^ macOS answers EPERM for a group whose members have all exited but one is not yet reaped.
        except (ProcessLookupError, PermissionError):
            break
        try:
            process.wait(timeout=wait_seconds)
        except subprocess.TimeoutExpired:
            continue
        # The leader ended; members of its group, such as Chrome, may not have.
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            break


def finish(process, timeout):
    """Wait for a check; one that runs too long is stopped with its whole group."""
    try:
        out, err = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        stop(process, grace=2)
        out, err = process.communicate()
        err = (err or '') + '\ntimed out after %d s' % timeout
    return subprocess.CompletedProcess(process.args, process.returncode, out, err)


def check(work, args, timeout, **options):
    """Run a Node check in its own group. Its Chrome profile goes into the run's folder."""
    options.setdefault('stderr', subprocess.PIPE)
    return finish(start(work, args, stdout=subprocess.PIPE, text=True,
                        env=dict(os.environ, WSX_TEST_WORK=str(work)), **options), timeout)


def sweep():
    """End the processes of earlier runs in this checkout that died before their cleanup,
    and remove their Chrome profiles. A process is ended only when its group is in the
    run's record and its command is the recorded one, or its command names the run's folder."""
    listing = subprocess.run(['ps', '-axo', 'pid=,pgid=,command='], capture_output=True, text=True, timeout=10).stdout
    running = []
    for line in listing.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            running.append((int(parts[0]), int(parts[1]), parts[2]))
    ended = []
    for folder in (ROOT / '.work').glob('*-*'):
        prefix, _, owner = folder.name.partition('-')
        if prefix not in ('web', 'real') or not owner.isdigit() or int(owner) == os.getpid():
            continue
        try:
            os.kill(int(owner), 0)
            continue  # that run is still going
        except ProcessLookupError:
            pass
        except PermissionError:
            continue
        record = folder / RECORD
        groups = {}
        if record.exists():
            for line in record.read_text().splitlines():
                group, _, command = line.partition('\t')
                if group.isdigit():
                    groups[int(group)] = command
        targets = [pid for pid, group, command in running if pid != os.getpid()
                   and (str(folder) in command or (group in groups and command == groups[group]))]
        ended.extend(targets)
        for signal_number in (signal.SIGTERM, signal.SIGKILL):
            for pid in targets:
                try:
                    os.kill(pid, signal_number)
                except (ProcessLookupError, PermissionError):
                    pass
            deadline = time.monotonic() + 3
            while targets and time.monotonic() < deadline:
                targets = [pid for pid in targets if alive(pid)]
                time.sleep(0.05)
        for profile in folder.glob('wsx-web-chrome-*'):
            shutil.rmtree(profile, ignore_errors=True)
        record.unlink(missing_ok=True)
    return ended


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


class Site:
    """HTTP client for one served site. `shape` picks the inbound path form."""

    def __init__(self, port, base, origin=None):
        self.port, self.base, self.cookie = port, base, None
        self.origin = origin or 'http://127.0.0.1:%d' % port
        # A service-style site is reached through a proxy that keeps the public Host.
        self.host = self.origin.split('://')[1] if origin else None

    def request(self, method, path, body=None, headers=None, shape='prefixed', cookie=True, raw=False):
        prefix = self.base if shape == 'prefixed' else ''
        sent = {'Origin': self.origin} if method not in ('GET', 'HEAD') else {}
        if self.host:
            sent['Host'] = self.host
        if cookie and self.cookie:
            sent['Cookie'] = self.cookie
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
            sent['Content-Type'] = 'application/json'
        sent.update(headers or {})
        sent = {name: value for name, value in sent.items() if value is not None}
        # ^ A busy disk holds a wsxd state save, and every request behind it, for over 14 s.
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=30)
        try:
            connection.request(method, prefix + path, body=body, headers=sent)
            response = connection.getresponse()
            data = response.read()
        finally:
            connection.close()
        if raw:
            return response.status, dict((k.lower(), v) for k, v in response.getheaders()), data
        return response.status, (json.loads(data) if data[:1] in (b'{', b'[') else data)


def steady(site, *arguments, **options):
    """A request that waits when the device's request budget is spent (429)."""
    while True:
        answer = site.request(*arguments, **options)
        if answer[0] != 429:
            return answer
        time.sleep(0.5)


class Terminal:
    """One terminal WebSocket, opened and read the way a browser does it."""

    def __init__(self, site, session, query='', origin='site', cookie='site', client='7'):
        self.status, self.error, self.controls, self.output = None, None, [], b''
        self.socket = socket.create_connection(('127.0.0.1', site.port), timeout=10)
        path = '%s/api/terminal/%d/ws?client=%s%s' % (site.base, session, client, query)
        lines = ['GET %s HTTP/1.1' % path, 'Host: %s' % (site.host or '127.0.0.1:%d' % site.port),
                 'Upgrade: websocket', 'Connection: Upgrade', 'Sec-WebSocket-Version: 13',
                 'Sec-WebSocket-Key: ' + base64.b64encode(os.urandom(16)).decode()]
        origin = site.origin if origin == 'site' else origin
        cookie = site.cookie if cookie == 'site' else cookie
        if origin:
            lines.append('Origin: ' + origin)
        if cookie:
            lines.append('Cookie: ' + cookie)
        self.socket.sendall(('\r\n'.join(lines) + '\r\n\r\n').encode())
        self.pending = b''
        while b'\r\n\r\n' not in self.pending:
            self.pending += self.socket.recv(65536)
        head, self.pending = self.pending.split(b'\r\n\r\n', 1)
        self.status = int(head.split(b' ', 2)[1])
        if self.status != 101:
            length = int(re.search(rb'content-length: (\d+)', head, re.I).group(1))
            while len(self.pending) < length:
                self.pending += self.socket.recv(65536)
            self.error = json.loads(self.pending)['error']['code'] if self.pending[:1] == b'{' else None
            self.socket.close()

    def take(self, count):
        while len(self.pending) < count:
            chunk = self.socket.recv(65536)
            if not chunk:
                raise EOFError
            self.pending += chunk
        data, self.pending = self.pending[:count], self.pending[count:]
        return data

    def frame(self):
        """Read one frame into `controls` or `output`. False when the server closed."""
        try:
            first, second = self.take(2)
            length = second & 0x7f
            if length == 126:
                length = struct.unpack('>H', self.take(2))[0]
            elif length == 127:
                length = struct.unpack('>Q', self.take(8))[0]
            payload = self.take(length)
        except (EOFError, ConnectionError):
            return False
        opcode = first & 0x0f
        if opcode == 1:
            self.controls.append(json.loads(payload))
        elif opcode == 2:
            self.output += payload
        elif opcode == 9:
            self.write(10, payload)  # a browser answers each ping
        return opcode != 8

    def until(self, test, label):
        """Read frames until `test()` holds. The stream ending first is a failure."""
        while not test():
            assert self.frame(), '%s: stream ended; controls %r, output %r' % (label, self.controls, self.output[-200:])

    def control(self, kind):
        self.until(lambda: any(item['type'] == kind for item in self.controls), kind)
        return next(item for item in self.controls if item['type'] == kind)

    def send(self, data):
        """bytes: terminal input. dict: a control message."""
        self.write(*((2, data) if isinstance(data, bytes) else (1, json.dumps(data).encode())))

    def write(self, opcode, payload):
        mask = os.urandom(4)
        size = bytes([0x80 | len(payload)]) if len(payload) < 126 else bytes([0x80 | 126]) + struct.pack('>H', len(payload))
        self.socket.sendall(bytes([0x80 | opcode]) + size + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def drop(self):
        """Lose the connection without a close frame, as a network failure does."""
        self.socket.close()


class Events(threading.Thread):
    """The page's one server-sent event stream, read like a browser would."""

    def __init__(self, site, shape='prefixed', page=None):
        super().__init__(daemon=True)
        self.seen, self.ready, self.closed = [], threading.Condition(), threading.Event()
        self.connection = http.client.HTTPConnection('127.0.0.1', site.port, timeout=60)
        self.connection.request('GET', (site.base if shape == 'prefixed' else '') + '/api/events'
                                + ('?page=' + page if page else ''),
                                headers=dict({'Cookie': site.cookie, 'Accept': 'text/event-stream'},
                                             **({'Host': site.host} if site.host else {})))
        self.response = self.connection.getresponse()
        assert self.response.status == 200, self.response.status
        assert self.response.getheader('content-type').startswith('text/event-stream')
        self.start()

    def run(self):
        name, data = None, []
        try:
            for line in self.response:
                line = line.decode().rstrip('\n')
                if line.startswith('event:'):
                    name = line[6:].strip()
                elif line.startswith('data:'):
                    data.append(line[5:].strip())
                elif line == '' and name:
                    with self.ready:
                        self.seen.append((name, json.loads('\n'.join(data) or '{}')))
                        self.ready.notify_all()
                    name, data = None, []
        except Exception:
            pass
        self.closed.set()

    def expect(self, name, test=lambda data: True, timeout=8):
        # Events of different kinds may interleave; each one is matched once.
        deadline = time.monotonic() + timeout
        with self.ready:
            while True:
                for index, (got, data) in enumerate(self.seen):
                    if got == name and test(data):
                        del self.seen[index]
                        return data
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError('no %r event; saw %r' % (name, self.seen[-6:]))
                self.ready.wait(remaining)


class Relay(threading.Thread):
    """The network between the browser and a site: a TCP relay that can go silent without an
    error, as a phone's connection does after a network change or a sleep. A word written to
    `control` acts once, then `<control>.done` says so: `events` silences every connection that
    carries an event stream, `chat` the next one that asks for a manager chat. A silent
    connection takes bytes and passes none, and its close never reaches the other end."""

    def __init__(self, port, target, control):
        super().__init__(daemon=True)
        self.listener = socket.create_server(('127.0.0.1', port))
        self.target, self.control, self.links, self.armed = target, control, [], set()
        self.start()
        threading.Thread(target=self.watch, daemon=True).start()

    def run(self):
        while True:
            client, _ = self.listener.accept()
            try:
                server = socket.create_connection(('127.0.0.1', self.target))
            except OSError:
                client.close()
                continue
            link = {'events': False, 'silent': False}
            self.links.append(link)
            threading.Thread(target=self.pump, args=(client, server, link, True), daemon=True).start()
            threading.Thread(target=self.pump, args=(server, client, link, False), daemon=True).start()

    def pump(self, source, sink, link, upstream):
        try:
            while data := source.recv(65536):
                if upstream and data.startswith(b'GET /api/events'):
                    link['events'] = True
                elif upstream and 'chat' in self.armed and b'/chat' in data.split(b'\r\n', 1)[0]:
                    self.armed.discard('chat')
                    link['silent'] = True
                if not link['silent']:
                    sink.sendall(data)
        except OSError:
            pass
        if not link['silent']:
            for end in (source, sink):
                with contextlib.suppress(OSError):
                    end.shutdown(socket.SHUT_RDWR)

    def watch(self):
        while True:
            if self.control.exists():
                word = self.control.read_text().strip()
                self.control.unlink()
                if word == 'events':
                    for link in self.links:
                        link['silent'] |= link['events']
                else:
                    self.armed.add(word)
                self.control.with_name(self.control.name + '.done').write_text(word)
            time.sleep(0.1)


# ^ The time since the previous passed check, by check: the result names the slowest ones.
TIMES = []
LAST = [time.monotonic()]


def timed(label):
    now = time.monotonic()
    TIMES.append((label[:70], round(now - LAST[0], 1)))
    LAST[0] = now


def slowest(count=8):
    return sorted(TIMES, key=lambda span: -span[1])[:count]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--daemon', type=Path, default=ROOT / 'target/debug/wsxd')
    parser.add_argument('--browser', action='store_true',
                        help='also run scripts/web-browser-check.mjs with an installed Chrome')
    parser.add_argument('--captures', type=Path, default=ROOT / '.work/web-captures')
    parser.add_argument('--part', choices=('all', 'chat', 'tasks'), default='all',
                        help='chat: sessions, files, terminal, push; tasks: tasks and the manager')
    args = parser.parse_args()
    if args.part in ('all', 'chat'):
        web_journey(args)
    if args.part in ('all', 'tasks'):
        task_journey(args)


def web_journey(args):
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    if not wsx.is_file() or not wsxd.is_file() or wsx.parent != wsxd.parent:
        raise RuntimeError('build adjacent wsx/wsxd first')
    node = shutil.which('node')
    assert node, 'Node is required for the independent Web Push receiver'
    left = sweep()
    if left:
        print('ended %d processes of earlier runs that were killed' % len(left), file=sys.stderr)
    # A SIGTERM (a tool or CI timeout) still runs the cleanup below.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    # Short path: the Unix socket must fit macOS SUN_LEN.
    work = ROOT / '.work' / ('web-' + str(os.getpid()))
    work.parent.mkdir(exist_ok=True)
    work.mkdir(mode=0o700)
    processes, streams, success, started, path = [], [], False, time.monotonic(), None
    checks = []

    def ok(label):
        checks.append(label)
        timed(label)

    try:
        for name in ('home', 'state', 'project', 'project2', 'project3', 'design', 'design-login', 'bare', 'bin'):
            (work / name).mkdir(mode=0o700)
        # ^ The pane shells find this run's fake `claude` here, for the resume of an exited agent.
        bin_dir = work / 'bin'
        config = (work / 'home/Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'config/wsx')
        config.mkdir(parents=True)
        # ^ README "Profiles": project2 is in profile work, with its own agent config root.
        (work / 'claude-work').mkdir(mode=0o700)
        profile_root = (work / 'claude-work').resolve()
        (profile_root / 'skills/profile-skill').mkdir(parents=True)
        (profile_root / 'skills/profile-skill/SKILL.md').write_text('---\nname: profile-skill\ndescription: The work skill\n---\nBODY\n')
        registry = config / 'config-v2.toml'
        registry.write_text('resume_agents_on_restore = false\n[[profiles]]\nname = "work"\nclaude_config_dir = %s\n'
                            '[[projects]]\nname = "project2"\npath = %s\nprofile = "work"\n'
                            % (json.dumps(str(profile_root)), json.dumps(str((work / 'project2').resolve()))))
        path = work / 'state/wsx/wsx.sock'
        env = {'HOME': str(work / 'home'), 'XDG_CONFIG_HOME': str(work / 'config'), 'XDG_STATE_HOME': str(work / 'state'),
               'XDG_CACHE_HOME': str(work / 'cache'), 'WSX_SOCKET': str(path), 'WSX_DAEMON_BIN': str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()),
               'PATH': str(work / 'bin') + ':/usr/bin:/bin:/usr/sbin:/sbin', 'SHELL': '/bin/sh', 'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8',
               # ^ A fixture git command must never reach the repository this script runs in.
               'GIT_CEILING_DIRECTORIES': str(work)}
        for name in ('project', 'project2', 'project3', 'design', 'bare'):
            subprocess.run(['git', 'init', '-q', '-b', 'main', str(work / name)], env=env, check=True, timeout=5)
        project = (work / 'project').resolve()
        (project / 'notes.txt').write_text('first line\n')
        (project / 'sub').mkdir()
        (project / 'sub/deep.txt').write_text('deep\n')
        outside = work / 'outside.png'
        outside.write_bytes(png(8, 200))
        (project / 'out').mkdir()
        (project / 'out/leak.png').symlink_to(outside)
        (project / 'escape').symlink_to(work / 'home')
        (project / '.env').write_text('KEY=fixture-secret\n')
        (project / 'out/api-token.txt').write_text('fixture-secret\n')
        chart = work / 'chart-source.png'
        chart.write_bytes(png(12, 40))
        actor = work / 'actor.py'
        actor.write_text(wsxd_client.fixture(ACTOR))

        def call(method, params=None):
            return wsxd_client.call(path, method, params, timeout=30)

        def wait(test, label, timeout=8):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                value = test()
                if value:
                    return value
                time.sleep(0.1)
            raise AssertionError(label + ' timeout')

        def cli(*arguments, expect=0):
            result = subprocess.run([str(wsx), *map(str, arguments)], env=env, cwd=project,
                                    capture_output=True, text=True, timeout=10)
            assert (result.returncode == 0) == (expect == 0), (arguments, result.stdout, result.stderr)
            return result

        def traces(name):
            file = work / (name + '.trace')
            entries = [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []
            return [entry['received'] for entry in entries if 'received' in entry]

        def settle(name):
            """Wait until the fixture agent can take the next message."""
            def state():
                snapshot = call('snapshot')['data']
                pane = next(s['focused_pane'] for s in snapshot['sessions'] if s['id'] == sessions[name])
                # ^ A new agent has no report until its first one: wait for it, do not fail.
                agent = next(p for p in snapshot['panes'] if p['id'] == pane)['agent'] or {}
                return agent.get('state') in ('idle', 'done')
            wait(state, name + ' agent ready')

        def lease_free(session):
            """Wait until no terminal holds the session's pane: a closed socket frees it shortly after."""
            pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == session)
            wait(lambda: call('terminal_acquire', {'pane_id': pane, 'client_id': 6, 'takeover': False})['type'] == 'ack',
                 'terminal lease released')
            assert call('terminal_release', {'pane_id': pane, 'client_id': 6})['type'] == 'ack'

        @contextlib.contextmanager
        def wsx_window(pane):
            """A wsx window in Terminal mode: it holds the pane's lease and reads its frames.
            Yields the messages it got; a displaced window gets an error and its stream ends."""
            with socket.socket(socket.AF_UNIX) as link:
                link.settimeout(10)
                link.connect(str(path))
                lines = link.makefile('rb')
                link.sendall(wsxd_client.HELLO)
                lines.readline()
                link.sendall(json.dumps({'method': 'terminal_subscribe', 'params': {
                    'pane_id': pane, 'client_id': 77, 'takeover': False, 'rows': 12, 'cols': 100}}).encode() + b'\n')
                assert json.loads(lines.readline())['type'] == 'ack'
                seen = []
                # ^ A pane lease ends 3 s after the last input or heartbeat; reading does not renew it.
                done = threading.Event()
                threading.Thread(target=wsxd_client.heartbeat_streams, args=([link], done), daemon=True).start()

                def read():
                    try:
                        for line in lines:
                            seen.append(json.loads(line))
                    except (OSError, ValueError):
                        pass
                threading.Thread(target=read, daemon=True).start()
                try:
                    yield seen
                finally:
                    done.set()

        def told_control_moved(seen):
            moved = wait(lambda: [m for m in seen if m['type'] == 'error'], 'the wsx window is displaced')
            assert moved[0]['data']['code'] == 'lease_transferred', moved

        daemon = start(work, [str(wsxd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        processes.append(daemon)
        wait(path.exists, 'daemon')
        # The browser's project has a second worktree, so its project screen shows two.
        assert call('synchronize_projects', {'projects': [
            {'name': name, 'path': str(work / name), 'worktrees': [{'path': str(work / name), 'branch': 'main'}]
             + ([{'path': str(work / 'design-login'), 'branch': 'fix/login'}] if name == 'design' else [])}
            for name in ('project', 'project2', 'project3', 'design', 'bare')]})['type'] == 'ack'
        worktrees = {Path(item['path']).name: item['id'] for item in call('snapshot')['data']['worktrees']}
        projects = {item['name']: item['id'] for item in call('snapshot')['data']['projects']}
        (work / 'project3/third.txt').write_text('third project\n')
        sessions = {}
        for name, tree in (('owner', 'project'), ('family', 'project2')):
            created = call('session_create', {'worktree_id': worktrees[tree], 'label': name + ' chat',
                'command': [sys.executable, str(actor), str(work / (name + '.jsonl')), str(work / (name + '.trace')),
                            str(work / tree), str(chart), str(outside)], 'rows': 12, 'cols': 100})
            assert created['type'] == 'created', created
            sessions[name] = created['data']['id']
        wait(lambda: all(pane['agent'] for pane in call('snapshot')['data']['panes']), 'agent reports')

        push = start(work, [node, str(ROOT / 'scripts/web-push-fixture.mjs'), str(work / 'push.jsonl')],
                                stdout=subprocess.PIPE, text=True)
        processes.append(push)
        receiver = json.loads(push.stdout.readline())
        push_origin = 'http://127.0.0.1:%d' % receiver['port']

        def pushes():
            file = work / 'push.jsonl'
            return [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []

        vapid = cli('web', 'vapid').stdout
        private_key = next(line.split('"')[1] for line in vapid.splitlines() if line.startswith('private_key'))
        ports = {name: free_port() for name in ('owner', 'family', 'root', 'viewer', 'm2', 'bare', 'work')}
        service_origin = 'https://wsx-fixture.example-tailnet.ts.net'

        def site_file(name, text, mode=0o600):
            file = work / (name + '.toml')
            file.write_text(text)
            file.chmod(mode)
            return file

        owner_text = f'''id = "owner"
name = "Owner <wsx>"
base_path = "/app/owner"
port = {ports['owner']}
public_origin = "http://127.0.0.1:{ports['owner']}"
panels = ["chat", "files", "terminal"]
theme = "default"
[access]
projects = ["{project}", "{(work / 'project3').resolve()}"]
capabilities = ["chat_send", "agent_write", "files_read", "files_write", "terminal_view", "terminal_input", "media_upload", "media_download", "push"]
[media]
max_file_bytes = 65536
quota_bytes = 1048576
ttl_seconds = 600
[push]
subject = "mailto:owner@example.invalid"
private_key = "{private_key}"
service_origins = ["{push_origin}"]
'''
        def theme_dir(name, manifest, extra=None):
            """Write a deployer-supplied theme directory and return its path."""
            directory = work / ('theme-' + name)
            (directory / 'fonts').mkdir(parents=True)
            (directory / 'theme.toml').write_text(manifest)
            for relative, data in (extra or {}).items():
                (directory / relative).write_bytes(data)
            return directory

        fixture_font = b'wOF2' + bytes(range(256)) * 4
        custom_icons = {'small.png': png(192, 9), 'large.png': png(512, 9)}
        custom_manifest = '''name = "family-warm"
extends = "default"
components = "look.css"
[tokens]
color-accent = "#225577"
font-body = "Fixture Sans, system-ui, sans-serif"
radius-control = "0"
[tokens_dark]
color-accent = "#88bbdd"
[[fonts]]
family = "Fixture Sans"
file = "fonts/fixture.woff2"
weight = "700"
[icons]
png_192 = "small.png"
png_512 = "large.png"
'''
        custom_css = b'.button-primary { text-transform: uppercase; }\n'
        custom_theme = theme_dir('family', custom_manifest,
                                 dict(custom_icons, **{'look.css': custom_css, 'fonts/fixture.woff2': fixture_font}))
        # A theme that recolors the light scheme and has no dark table is light only.
        plain_theme = theme_dir('plain', 'name = "plain"\n[tokens]\ncolor-accent = "#225577"\n')
        files = {
            'owner': site_file('owner', owner_text),
            # Site B: same binary, fewer panels and capability bits, another project.
            'family': site_file('family', f'''id = "family"
name = "Family"
base_path = "/app/family"
port = {ports['family']}
public_origin = "http://127.0.0.1:{ports['family']}"
panels = ["chat"]
theme = "{custom_theme}"
[access]
projects = ["{(work / 'project2').resolve()}"]
capabilities = ["chat_send", "media_upload", "media_download"]
'''),
            # The primary shape: a Tailscale Service, one https hostname per site, at
            # the root. Also carries small media limits for TTL and quota.
            'root': site_file('root', f'''id = "root"
name = "Root"
port = {ports['root']}
public_origin = "{service_origin}"
panels = ["chat"]
[access]
projects = ["{project}", "{(work / 'project3').resolve()}"]
capabilities = ["chat_send", "chat_answer", "session_start", "media_upload", "media_download"]
agent_command = ["/bin/sh"]
[media]
max_file_bytes = 4096
quota_bytes = 9000
ttl_seconds = 4
'''),
            # The sites of the real-browser check: every screen and state, and an empty one.
            # No agent_write: wsxd lets only one writer exchange be active in a worktree,
            # and this site shows several agents of one worktree at work.
            'm2': site_file('m2', f'''id = "m2"
name = "wsx"
base_path = "/app/m2"
port = {ports['m2']}
public_origin = "http://127.0.0.1:{ports['m2']}"
panels = ["chat", "files", "terminal"]
[access]
projects = ["{(work / 'design').resolve()}"]
capabilities = ["chat_send", "chat_answer", "session_start", "files_read", "files_write", "terminal_view", "terminal_input", "media_upload", "media_download"]
agent_command = ["/bin/sh"]
'''),
            'bare': site_file('bare', f'''id = "bare"
name = "wsx"
port = {ports['bare']}
public_origin = "http://127.0.0.1:{ports['bare']}"
panels = ["chat"]
[access]
projects = ["{(work / 'bare').resolve()}"]
capabilities = ["chat_send", "session_start"]
agent_command = ["/bin/sh"]
'''),
            # A profile site: the projects of profile work, now and later.
            'work': site_file('work', f'''id = "work"
name = "Work"
port = {ports['work']}
public_origin = "http://127.0.0.1:{ports['work']}"
panels = ["chat"]
[access]
projects = {{ profile = "work" }}
capabilities = ["chat_send", "session_start", "task_view", "task_act"]
agent_command = ["/bin/sh", "-c", {json.dumps('printf %s "$CLAUDE_CONFIG_DIR" > ' + str(work / 'profile-agent.txt') + '; exec sleep 600')}]
'''),
            # Watch-only terminal: terminal_view without terminal_input.
            'viewer': site_file('viewer', f'''id = "viewer"
name = "Viewer"
port = {ports['viewer']}
public_origin = "http://127.0.0.1:{ports['viewer']}"
panels = ["terminal"]
theme = "{plain_theme}"
terminal = {{ ligatures = false }}
[access]
projects = "all"
capabilities = ["terminal_view"]
'''),
        }
        # The site file is a trust boundary: wrong mode and unknown fields are refused.
        refused = cli('web', 'serve', '--site', site_file('open', owner_text, 0o644), expect=1)
        assert 'owner-only' in refused.stderr, refused.stderr
        refused = cli('web', 'serve', '--site', site_file('typo', owner_text + 'bootstrap_token = "x"\n'), expect=1)
        assert 'unknown field' in refused.stderr, refused.stderr
        refused = cli('web', 'serve', '--site', site_file('caps', owner_text.replace('"files", ', '')), expect=1)
        assert 'needs the Files panel' in refused.stderr, refused.stderr
        granted = 'projects = ["%s", "%s"]' % (project, (work / 'project3').resolve())
        assert granted in owner_text
        for label, value, expected in (('old key', None, 'unknown field'), ('a word that is not all', 'projects = "some"', 'must be "all"'),
                                       ('empty list', 'projects = []', '1-32 paths'), ('relative path', 'projects = ["project"]', 'must be absolute'),
                                       ('missing path', 'projects = ["/no/such/project"]', 'is missing'),
                                       ('duplicate', 'projects = ["%s", "%s"]' % (project, project), 'duplicate'),
                                       ('unknown profile', 'projects = { profile = "ghost" }', 'profile ghost is not in the wsx config')):
            text = owner_text.replace('[access]\n' + granted, '[project]\npath = "%s"' % project) if value is None else owner_text.replace(granted, value)
            refused = cli('web', 'serve', '--site', site_file('grant', text), expect=1)
            assert expected in refused.stderr, (label, refused.stderr)
        ok('site file: mode, unknown field, panel/capability mismatch and an invalid project grant refused')

        # Without --site the site folder decides: none, the only one, or a name.
        folder = work / 'config/wsx/web'
        folder.mkdir(parents=True, exist_ok=True)
        refused = cli('web', 'pair', expect=1)
        assert 'no site file in ' + str(folder) in refused.stderr and 'chmod 600' in refused.stderr, refused.stderr
        solo_text = files['bare'].read_text().replace('id = "bare"', 'id = "solo"')
        (folder / 'wsx.toml').write_text(solo_text)
        (folder / 'wsx.toml').chmod(0o600)
        paired = cli('web', 'pair').stdout.splitlines()
        assert paired[0].startswith("pairing code for 'solo': ") and paired[1] == 'site file %s' % (folder / 'wsx.toml'), paired
        assert paired[2].startswith('open http://127.0.0.1:%d/ ' % ports['bare']), paired
        (folder / 'family.toml').write_text(solo_text.replace('id = "solo"', 'id = "duo"'))
        (folder / 'family.toml').chmod(0o600)
        (folder / 'notes.txt').write_text('not a site\n')
        refused = cli('web', 'pair', expect=1)
        assert '2 site files in ' in refused.stderr, refused.stderr
        assert 'duo' in refused.stderr and str(folder / 'family.toml') in refused.stderr and str(folder / 'wsx.toml') in refused.stderr, refused.stderr
        assert cli('web', 'pair', 'duo').stdout.startswith("pairing code for 'duo': ")
        assert cli('web', 'pair', 'wsx').stdout.startswith("pairing code for 'solo': ")
        assert cli('web', 'devices', 'solo', '--json').stdout.strip() == '[]'
        refused = cli('web', 'pair', 'nope', expect=1)
        assert 'no site named "nope"' in refused.stderr and 'solo' in refused.stderr, refused.stderr
        for name in ('wsx.toml', 'family.toml', 'notes.txt'):
            (folder / name).unlink()
        ok('pair without --site: none says how to make one, one is used, two are listed, a site name or file picks one')

        # A theme is deployer data that ends up in a page under a strict CSP, so
        # every part of it is validated before the server starts.
        def refuse_theme(label, spec, expected):
            text = owner_text.replace('theme = "default"', 'theme = "%s"' % spec)
            refused = cli('web', 'serve', '--site', site_file('bad-theme', text), expect=1)
            assert expected in refused.stderr, (label, refused.stderr)

        def broken(label, manifest, expected, extra=None):
            refuse_theme(label, theme_dir(label, manifest, extra), expected)

        refuse_theme('unknown name', 'neon', 'not the built-in theme')
        refuse_theme('relative path', 'themes/mine', 'not the built-in theme')
        refuse_theme('missing path', work / 'no-such-theme', 'does not exist')
        base = 'name = "bad"\n'
        broken('css-injection', base + '[tokens]\ncolor-accent = "red;}body{display:none"\n', 'invalid value')
        broken('remote-font-stack', base + '[tokens]\nfont-body = "url(https://evil.example/a.woff2)"\n', 'invalid value')
        broken('unknown-token', base + '[tokens]\ncolor-brand = "#ffffff"\n', 'not a theme token')
        broken('dark-length', base + '[tokens_dark]\nspace-1 = "9px"\n', 'must be a color')
        broken('removed-token', base + '[tokens]\nshadow-raised = "none"\n', 'not a theme token')
        broken('bad-tracking', base + '[tokens]\ntracking-label = "url(x)"\n', 'invalid value')
        broken('unknown-field', base + 'script = "x.js"\n', 'unknown field')
        broken('extends-unknown', base + 'extends = "neon"\n', 'not the built-in theme')
        broken('bad-name', 'name = "Bad Name"\n', 'theme name')
        for index, css in enumerate((b'.button{background:url(https://evil.example/x.png)}', b'@import "https://evil.example/x.css";',
                                     b'@font-face{font-family:x;src:local(x)}', b'.button{background:image-set("x.png" 1x)}')):
            broken('css-%d' % index, base + 'components = "c.css"\n', 'must not contain', {'c.css': css})
        broken('css-oversized', base + 'components = "c.css"\n', 'larger than', {'c.css': b'.button{color:inherit}\n' * 8000})
        font = '[[fonts]]\nfamily = "F"\nfile = "%s"\n'
        broken('font-escape', base + font % '../outside.woff2', 'inside the theme directory')
        broken('font-magic', base + font % 'fonts/f.woff2', 'not WOFF2', {'fonts/f.woff2': b'<html>' * 20})
        broken('font-oversized', base + font % 'fonts/f.woff2', 'larger than', {'fonts/f.woff2': b'wOF2' + b'\0' * (2 * 1024 * 1024)})
        broken('font-family', base + '[[fonts]]\nfamily = "F\\";}x{"\nfile = "fonts/f.woff2"\n', 'font family', {'fonts/f.woff2': fixture_font})
        linked = theme_dir('font-symlink', base + font % 'fonts/f.woff2')
        (linked / 'fonts/f.woff2').symlink_to(outside)
        refuse_theme('font-symlink', linked, 'inside the theme directory')
        broken('icon-size', base + '[icons]\npng_192 = "a.png"\npng_512 = "a.png"\n', '512x512', {'a.png': png(192, 1)})
        ok('theme: unknown name, token injection, unknown token, URL in CSS, oversized CSS and font, path escape, symlink, wrong icon refused')

        sites = {}
        servers = {}

        def serve(name):
            process = start(work, [str(wsx), 'web', 'serve', '--site', str(files[name])], env=env,
                                       cwd=project, stdout=subprocess.DEVNULL, stderr=(work / (name + '.log')).open('a'))
            processes.append(process)
            servers[name] = process

        for name in files:
            serve(name)
            sites[name] = Site(ports[name], {'owner': '/app/owner', 'family': '/app/family', 'm2': '/app/m2'}.get(name, ''),
                               service_origin if name == 'root' else None)

        def up(site):
            try:
                return site.request('GET', '/manifest.webmanifest', shape='stripped')[0] == 200
            except OSError:
                return False
        for site in sites.values():
            wait(lambda: up(site), 'web server')
        owner, family, root = sites['owner'], sites['family'], sites['root']
        viewer = sites['viewer']

        # --- Sub-path first: both inbound shapes serve the same base-filled documents.
        for shape in ('prefixed', 'stripped'):
            status, headers, html = owner.request('GET', '/', shape=shape, raw=True)
            assert status == 200 and b'<base href="/app/owner/">' in html, html[:200]
            assert b'Owner &lt;wsx&gt;' in html and b'<wsx>' not in html
            # Markdown and diagrams did not loosen the policy: this is the whole header.
            assert headers['content-security-policy'] == (
                "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob:; media-src 'self'; font-src 'self'; "
                "connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'self'; form-action 'none'; frame-ancestors 'none'"), headers
            assert headers['x-content-type-options'] == 'nosniff' and headers['x-frame-options'] == 'DENY'
            status, manifest = owner.request('GET', '/manifest.webmanifest', shape=shape)
            assert status == 200 and manifest['id'] == manifest['scope'] == manifest['start_url'] == '/app/owner/'
            assert [(icon['sizes'], icon['purpose']) for icon in manifest['icons']] == [('192x192', 'any maskable'), ('512x512', 'any maskable')]
            status, headers, worker = owner.request('GET', '/sw.js', shape=shape, raw=True)
            assert status == 200 and headers['content-type'].startswith('text/javascript')
            # Module URLs carry the build: assets/<build>/main.js in the page and the shell list.
            build = re.search(rb'src="assets/([0-9a-f]{12})/main\.js"', html).group(1).decode()
            assert ('"assets/%s/main.js"' % build).encode() in worker and b'__WSX_' not in worker
            assert ('wsx-owner-%s' % build).encode() in worker
            # The diagram library is fetched on first use: not in the shell list, kept afterwards.
            shell, lazy = (json.loads(re.search(rb'const %s = (\[.*?\]);' % name, worker).group(1)) for name in (b'SHELL', b'LAZY'))
            assert lazy == ['assets/%s/mermaid-11.17.2.js' % build] and not set(lazy) & set(shell) and 'assets/%s/markdown-it.js' % build in shell, (shell, lazy)
            # This build's files never change under its path; another build's are refused, pinned files are not.
            status, headers, body = owner.request('GET', '/assets/%s/main.js' % build, shape=shape, raw=True)
            assert status == 200 and 'immutable' in headers['cache-control'] and headers['content-type'].startswith('text/javascript'), headers
            assert owner.request('GET', '/assets/000000000000/main.js', shape=shape)[1]['error']['code'] == 'old_build'
            assert owner.request('GET', '/assets/000000000000/JetBrainsMono-2.304-Regular.woff2', shape=shape, raw=True)[0] == 200
            assert owner.request('GET', '/assets/%s/nothing.js' % build, shape=shape)[0] == 404
            status, headers, body = owner.request('GET', '/' + lazy[0], shape=shape, raw=True, headers={'Accept-Encoding': 'gzip, br'})
            assert status == 200 and headers['content-encoding'] == 'gzip' and 'immutable' in headers['cache-control'], headers
            assert zlib.decompress(body, 31)[:12] == b'"use strict"', body[:20]
            assert owner.request('GET', '/' + lazy[0], shape=shape)[1]['error']['code'] == 'gzip_required'
            for asset, kind in (('/assets/main.js', 'text/javascript'), ('/assets/app.css', 'text/css'),
                                ('/' + re.search(r'href="(theme/[0-9a-f]{16}\.css)"', html.decode()).group(1), 'text/css'),
                                ('/' + manifest['icons'][0]['src'], 'image/png'), ('/' + manifest['icons'][1]['src'], 'image/png')):
                status, headers, body = owner.request('GET', asset, shape=shape, raw=True)
                assert status == 200 and headers['content-type'].startswith(kind), asset
            assert body[16:24] == struct.pack('>II', 512, 512)
        # No file uses an absolute URL: the same bytes must work under any base.
        for file in (ROOT / 'crates/wsx-web/web').glob('*'):
            if file.suffix in ('.js', '.html', '.css'):
                text = file.read_text()
                for bad in ('"/api', "'/api", '`/api', '"/assets', '"/media', "'/media", 'href="/', 'src="/'):
                    assert bad not in text, (file.name, bad)
        status, manifest = root.request('GET', '/manifest.webmanifest')
        assert manifest['scope'] == manifest['start_url'] == '/'
        assert b'<base href="/">' in root.request('GET', '/')[1]
        assert root.request('GET', '/app/owner/')[0] == 404
        # Service-style hostname: only that exact https host is answered.
        assert root.request('GET', '/', headers={'Host': '127.0.0.1:%d' % ports['root']})[0] == 421
        assert root.request('GET', '/', headers={'Host': 'other.example-tailnet.ts.net'})[0] == 421
        assert root.request('POST', '/api/pair', {'code': 'x'}, headers={'Origin': 'http://wsx-fixture.example-tailnet.ts.net'})[0] == 403
        ok('HTML, manifest, service worker, assets: prefixed, stripped and service-hostname root shapes')

        # --- Themes: selection, a deployer directory, content-hash URLs, strict CSP.
        def themed(site):
            status, headers, html = site.request('GET', '/', raw=True)
            html = html.decode()
            csp = headers['content-security-policy']
            assert "default-src 'none'" in csp and "font-src 'self'" in csp and "style-src 'self'" in csp
            assert 'unsafe' not in csp and 'http' not in csp and '*' not in csp, csp
            assert 'style=' not in html and '<style' not in html
            sheet = re.search(r'href="(theme/[0-9a-f]{16}\.css)"', html).group(1)
            status, sheet_headers, css = site.request('GET', '/' + sheet, raw=True)
            assert status == 200 and sheet_headers['cache-control'] == 'public, max-age=31536000, immutable'
            assert sheet == 'theme/%s.css' % hashlib.sha256(css).hexdigest()[:16]
            return html, css.decode(), site.request('GET', '/manifest.webmanifest')[1]

        html, css, manifest = themed(owner)
        assert 'data-theme="default"' in html and 'name="color-scheme" content="light dark"' in html
        # A theme with a dark scheme is dark unless the device chose light.
        dark_rule = ':root:not([data-scheme="light"]),[data-scheme="dark"]{color-scheme:dark;'
        assert '--color-bg:#f7f7f8;' in css and dark_rule in css and '@font-face' not in css
        assert css.split(dark_rule)[1].startswith('--color-accent:#6fe3c1;')
        # The token contract is the default theme's list: 64 tokens, 16 of them the terminal palette.
        light_tokens = re.findall(r'--([a-z0-9-]+):', css.split(dark_rule)[0])
        assert len(light_tokens) == 64 and len([token for token in light_tokens if token.startswith('color-ansi-')]) == 16, len(light_tokens)
        assert 'prefers-color-scheme' not in css
        # The page is dark by default, so the installed app's splash and toolbar are dark too.
        assert (manifest['background_color'], manifest['theme_color']) == ('#0a0b0d', '#0a0b0d')
        assert 'content="#0a0b0d"' in html

        html, css, manifest = themed(sites['viewer'])
        assert 'data-theme="plain"' in html and 'name="color-scheme" content="light"' in html
        assert '--color-accent:#225577;' in css and 'data-scheme' not in css, 'a theme without a dark table is light only'

        html, css, manifest = themed(family)
        assert 'data-theme="family-warm"' in html and 'content="#0a0b0d"' in html
        # Its own tokens over the inherited default ones, light and dark.
        assert '--color-accent:#225577;' in css and '--color-bg:#f7f7f8;' in css and '--radius-control:0;' in css
        assert css.split(dark_rule)[1].count('--color-accent:#88bbdd;') == 1
        assert css.endswith(custom_css.decode())
        font_name = hashlib.sha256(fixture_font).hexdigest()[:16] + '.woff2'
        assert '@font-face{font-family:"Fixture Sans";src:url("%s");font-weight:700;' % font_name in css
        status, headers, body = family.request('GET', '/theme/' + font_name, raw=True)
        assert status == 200 and body == fixture_font and headers['content-type'] == 'font/woff2'
        assert 'immutable' in headers['cache-control']
        assert (manifest['background_color'], manifest['theme_color']) == ('#0a0b0d', '#0a0b0d')
        assert [family.request('GET', '/' + icon['src'])[1] for icon in manifest['icons']] == [custom_icons['small.png'], custom_icons['large.png']]
        assert '"theme/%s"' % font_name in family.request('GET', '/sw.js')[1].decode()  # offline shell has the font
        assert family.request('GET', '/theme/theme.toml')[0] == 404 and family.request('GET', '/theme/..%2Ftheme.toml')[0] == 404
        # Core component CSS holds no design value of its own: only tokens.
        core = re.sub(r'/\*.*?\*/', '', (ROOT / 'crates/wsx-web/web/app.css').read_text(), flags=re.S)
        literals = re.findall(r'#[0-9a-fA-F]{3,8}\b|\b\d*\.?\d+(?:px|rem|em|ms|s|pt)\b|\brgba?\(', core)
        assert literals == [], literals
        for hook in ('app', 'rail', 'tabbar', 'title', 'label', 'rows', 'row', 'button', 'input', 'bar', 'switch',
                     'chat', 'message', 'steps', 'working', 'composer', 'queue', 'code', 'sheet', 'notice',
                     'terminal', 'terminal-key'):
            assert re.search(r'\.%s[\s,.:{\[]' % hook, core), hook
        ok('themes: built-in default, a light-only custom theme, custom directory with font and icons, hash URLs, manifest follows theme, CSP strict, token-only core CSS')

        # --- Request checks.
        assert owner.request('GET', '/api/session')[0] == 401
        assert owner.request('GET', '/api/session', shape='stripped')[1]['error']['code'] == 'unpaired'
        for host in ('evil.example', 'localhost:%d' % ports['owner'], '127.0.0.1'):
            assert owner.request('GET', '/', headers={'Host': host})[0] == 421, host
        assert owner.request('GET', '/', headers={'Origin': 'http://evil.example'})[0] == 403
        assert owner.request('POST', '/api/pair', {'code': 'x'}, headers={'Origin': 'http://127.0.0.1:1'})[0] == 403
        status, body = Site.request(owner, 'POST', '/api/pair', json.dumps({'code': 'x'}).encode(),
                                    headers={'Origin': None, 'Content-Type': 'application/json'})
        assert status == 403 and body['error']['code'] == 'wrong_origin'
        assert owner.request('GET', '/api/session', headers={'Sec-Fetch-Site': 'cross-site'})[0] == 403
        ok('wrong Host, wrong Origin, missing Origin and cross-site fetch refused')

        # --- Pairing: one-time code, hashed credential, scoped cookie.
        assert owner.request('POST', '/api/pair', {'code': 'AAAAA-AAAAA'})[1]['error']['code'] == 'wrong_code'

        def pair(site, name, shape='prefixed'):
            code = cli('web', 'pair', '--site', files[name]).stdout.splitlines()[0].rsplit(' ', 1)[1]
            status, headers, body = site.request('POST', '/api/pair', {'code': code.lower(), 'name': name + ' phone'},
                                                 shape=shape, raw=True)
            assert status == 200, body
            return code, headers['set-cookie']
        code, cookie = pair(owner, 'owner')
        assert 'Path=/app/owner/;' in cookie and 'HttpOnly' in cookie and 'SameSite=Strict' in cookie
        assert 'Secure' not in cookie  # loopback http origin; https origins add Secure
        owner.cookie = cookie.split(';')[0]
        secret = owner.cookie.split('.', 1)[1]
        stored = (work / 'state/wsx/web/owner/devices.json').read_text()
        assert secret not in stored and code.replace('-', '') not in stored
        assert hashlib.sha256(secret.encode()).hexdigest() in stored
        assert (work / 'state/wsx/web/owner/devices.json').stat().st_mode & 0o777 == 0o600
        assert owner.request('POST', '/api/pair', {'code': code})[1]['error']['code'] == 'wrong_code'
        for shape in ('prefixed', 'stripped'):
            status, session = owner.request('GET', '/api/session', shape=shape)
            assert status == 200 and session['device']['name'] == 'owner phone'
            assert session['site']['panels'] == ['chat', 'files', 'terminal'] and session['site']['terminal'] == {'ligatures': True}
        saved = owner.cookie
        for forged in (saved[:-1] + ('0' if saved[-1] != '0' else '1'), saved.split('.')[0] + '.' + '0' * 64, 'wsx_web_owner=x'):
            owner.cookie = forged
            assert owner.request('GET', '/api/session')[0] == 401
        owner.cookie = saved
        # A credential for one site is not a credential for another.
        family.cookie = owner.cookie.replace('wsx_web_owner', 'wsx_web_family')
        assert family.request('GET', '/api/session')[0] == 401
        _, cookie = pair(family, 'family', shape='stripped')
        assert 'Path=/app/family/;' in cookie
        family.cookie = cookie.split(';')[0]
        _, cookie = pair(root, 'root')
        assert 'Path=/;' in cookie and 'Secure' in cookie  # https service origin
        root.cookie = cookie.split(';')[0]
        listed = json.loads(cli('web', 'devices', '--site', files['owner'], '--json').stdout)
        assert [device['name'] for device in listed] == ['owner phone'] and 'secret_sha256' not in listed[0]
        ok('pairing: wrong code, one-time code, hashed store, cookie path per site, forged cookies refused')

        # --- One event stream per page; chat round trip through the exchange broker.
        events = Events(owner)
        streams.append(events)
        first = events.expect('workspace')
        assert [item['id'] for item in first['sessions']] == [sessions['owner']], first
        # The build: this wsx, the commit it was built from, the page files' id and the server's start.
        built = owner.request('GET', '/api/session')[1]['build']
        assert built['id'] == build == first['build'] and built['version'] == cli('--version').stdout.split()[1], (built, first['build'])
        assert (built['commit'] is None or re.fullmatch('[0-9a-f]{7,}', built['commit'])) and built['started_unix'] <= time.time(), built
        # Both granted projects are listed, with their worktrees; the ungranted one is not.
        assert [(p['id'], p['name']) for p in first['projects']] == [(projects['project'], 'project'), (projects['project3'], 'project3')], first
        assert first['projects'][0]['worktrees'] == [{'id': worktrees['project'], 'branch': 'main', 'main': True}]
        assert first['sessions'][0]['project'] == {'id': projects['project'], 'name': 'project'}
        assert first['sessions'][0]['agent']['provider'] == 'claude'
        chat = '/api/chat/%d' % sessions['owner']
        status, history = owner.request('GET', chat + '/timeline')
        assert status == 200 and history['items'] == [] and history['exchange'] is None
        assert history['working'] is None and history['queue'] == [] and history['status'] == 'history_unreadable'
        assert owner.request('GET', chat + '/history')[0] == 404  # replaced by the timeline
        status, sent = owner.request('POST', chat + '/send', {'text': 'hello from the phone\r\nsecond line'})
        assert status == 200 and sent['queued']['delivered_unix_ms'], sent
        exchange = sent['exchange']
        # Truthful evidence: a PTY write is reported as a PTY write, never as acceptance.
        # ^ The answer is read after the delivery: the agent may already have taken the round.
        assert exchange['evidence'] in ('pty_delivery', 'pane_lifecycle') and 'not confirmed' in exchange['delivery'], exchange
        assert exchange['access'] == 'writer'
        received = wait(lambda: traces('owner'), 'PTY delivery')[0]
        assert received == '[wsx exchange %d, round 1, writer]\nhello from the phone\nsecond line' % exchange['id'], received
        events.expect('history', lambda data: data['session_id'] == sessions['owner'])
        events.expect('exchange', lambda data: data['state'] == 'done_observed')

        def messages(site, session):
            """The timeline, with its user and assistant items also listed as `messages`."""
            data = steady(site, 'GET', '/api/chat/%d/timeline' % session)[1]
            data['messages'] = [{'role': item['kind'], 'text': item['text'], 'attachments': item['attachments']}
                                for item in data.get('items', []) if item['kind'] in ('user', 'assistant')]
            return data

        history = wait(lambda: (lambda data: data if len(data['messages']) == 2 else None)(messages(owner, sessions['owner'])), 'history')
        assert [(m['role'], m['text']) for m in history['messages']] == [
            ('user', 'hello from the phone\nsecond line'), ('assistant', 'echo: hello from the phone')]
        assert history['model'] == 'claude-opus-5-5', history.get('model')
        # A done pane is lifecycle evidence only; it must not be shown as confirmed.
        assert history['exchange']['evidence'] == 'pane_lifecycle' and 'not confirmed' in history['exchange']['delivery']
        # The reply exists only in the native transcript, never on the fixture's screen.
        assert 'echo:' not in cli('session', 'peek', sessions['owner'], '--trim').stdout
        settle('owner')
        status, sent = owner.request('POST', chat + '/send', {'text': 'follow up'}, shape='stripped')
        exchange2 = sent['exchange']
        assert status == 200 and exchange2['id'] == exchange['id'] and exchange2['round'] == 2
        wait(lambda: len(traces('owner')) == 2, 'continued delivery')
        continued = wait(lambda: (lambda data: data if len(data['messages']) == 4 else None)(messages(owner, sessions['owner'])), 'continued history')
        # The fixture saved this one as a paste; the page shows the text that was sent, so its pending copy clears.
        assert continued['messages'][2] == {'role': 'user', 'text': 'follow up', 'attachments': []}, continued['messages'][2]
        assert owner.request('POST', chat + '/send', {'text': '   '})[0] == 400
        assert owner.request('POST', chat + '/send', {'text': 'x' * 70000})[0] == 400
        assert owner.request('POST', '/api/chat/999999/send', {'text': 'nope'})[0] == 404
        ok('SSE stream and chat: delivery evidence, native history, continuation, bad input')

        # --- Slash commands: the agent's list from a fake Claude home, and one command
        # typed as written. A session of its own, on the read-only root site.
        claude = work / 'home/.claude'
        for name, text in (('review-diff', '---\nname: review-diff\ndescription: "Review the staged diff"\n---\nBODY-SECRET\n'),
                           ('deploy-notes', '---\ndescription: >\n  Write the deploy\n  notes for a release\n---\nBODY-SECRET\n'),
                           ('zebra', '# Zebra stripes a table\n\nBODY-SECRET\n'),
                           ('broken', '---\nname: broken\ndescription: the frontmatter never closes\nBODY-SECRET\n')):
            (claude / 'skills' / name).mkdir(parents=True)
            (claude / 'skills' / name / 'SKILL.md').write_text(text)
        plugin = work / 'plugin-fixture'
        (plugin / 'skills/hello').mkdir(parents=True)
        (plugin / 'skills/hello/SKILL.md').write_text('---\nname: hello\ndescription: Say hello\n---\nBODY-SECRET\n')
        (claude / 'plugins').mkdir()
        (claude / 'plugins/installed_plugins.json').write_text(json.dumps({'version': 2, 'plugins': {
            'fixture@local': [{'scope': 'user', 'installPath': str(plugin)}], 'off@local': [{'scope': 'user', 'installPath': str(plugin)}]}}))
        (claude / 'settings.json').write_text(json.dumps({'enabledPlugins': {'fixture@local': True, 'off@local': False}}))
        third_root = (work / 'project3').resolve()
        (third_root / '.claude/commands').mkdir(parents=True)
        (third_root / '.claude/commands/ship.md').write_text('---\ndescription: Ship the release\n---\nBODY-SECRET\n')
        # ^ A repository's .claude may link to any file of the user: links there are not read.
        (work / 'private-note.md').write_text('---\ndescription: LEAKED-LINE\n---\n')
        (third_root / '.claude/commands/leak.md').symlink_to(work / 'private-note.md')
        (third_root / '.claude/skills/linked').mkdir(parents=True)
        (third_root / '.claude/skills/linked/SKILL.md').symlink_to(work / 'private-note.md')
        created = call('session_create', {'worktree_id': worktrees['project3'], 'label': 'commands',
            'command': [sys.executable, str(actor), str(work / 'commands.jsonl'), str(work / 'commands.trace'),
                        str(third_root), str(chart), str(outside)], 'rows': 12, 'cols': 100})
        assert created['type'] == 'created', created
        commands_session = sessions['commands'] = created['data']['id']
        commands_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == commands_session)
        wait(lambda: next(p for p in call('snapshot')['data']['panes'] if p['id'] == commands_pane)['agent'], 'commands agent')
        status, listing = steady(root, 'GET', '/api/chat/%d/commands' % commands_session)
        assert status == 200 and listing['provider'] == 'claude' and listing['listed'] and listing['runs'], listing
        names = [(item['name'], item['source']) for item in listing['commands']]
        assert names == [('compact', 'builtin'), ('clear', 'builtin'), ('ship', 'project'), ('deploy-notes', 'user'),
                         ('review-diff', 'user'), ('zebra', 'user'), ('fixture:hello', 'plugin')], names
        described = {item['name']: item['description'] for item in listing['commands']}
        assert described['deploy-notes'] == 'Write the deploy notes for a release' and described['review-diff'] == 'Review the staged diff'
        assert described['zebra'] == 'Zebra stripes a table' and listing['skipped'] == 1 and not listing['truncated'], listing
        assert 'BODY-SECRET' not in json.dumps(listing), 'a file body left the server'
        assert 'LEAKED-LINE' not in json.dumps(listing), 'a linked file left the server'
        assert steady(sites['viewer'], 'GET', '/api/chat/%d/commands' % commands_session)[0] in (401, 403, 404)
        assert steady(root, 'GET', '/api/chat/999999/commands')[0] == 404
        # wsxd types a command as written only when it is one short /name line.
        for prompt in ('/compact\nsecond line', 'compact', '/ compact', '/' + 'x' * 300, '!', '!   ', '!ls\npwd', '!' + 'x' * 300):
            refused = call('agent_exchange_create', {'pane_id': commands_pane, 'prompt': prompt, 'timeout_ms': 60000, 'command': True})
            assert refused['type'] == 'error' and refused['data']['code'] == 'invalid_prompt', (prompt, refused)
        assert traces('commands') == []
        # A listed command is typed as written: no exchange line before it.
        status, sent = root.request('POST', '/api/chat/%d/send' % commands_session, {'text': '/compact'})
        assert status == 200 and sent['exchange']['access'] == 'read_only', sent
        assert wait(lambda: traces('commands'), 'command delivery') == ['/compact']
        settle('commands')
        # Text that only looks like a command keeps the exchange line, and so does a command with a second line.
        for text in ('/usr/bin is the folder', '/compact\nthen this'):
            before = len(traces('commands'))
            status, sent = root.request('POST', '/api/chat/%d/send' % commands_session, {'text': text})
            assert status == 200, sent
            received = wait(lambda: len(traces('commands')) > before and traces('commands')[-1], 'enveloped delivery')
            # Sent at once or queued behind the previous turn: either way it arrives with the exchange line.
            assert received.startswith('[wsx exchange ') and received.endswith(']\n' + text), received
            settle('commands')
        # A `!` line is Claude's shell mode: typed as written, and shown as written, without its output.
        before = len(traces('commands'))
        status, sent = root.request('POST', '/api/chat/%d/send' % commands_session, {'text': '! git status'})
        assert status == 200, sent
        assert wait(lambda: len(traces('commands')) > before and traces('commands')[-1], 'shell line delivery') == '! git status'
        settle('commands')
        shown = wait(lambda: (lambda items: items if items and items[-1].get('text') == 'the shell line ran' else None)(
            steady(root, 'GET', '/api/chat/%d/timeline' % commands_session)[1]['items']), 'shell line timeline')
        assert [item['text'] for item in shown[-2:]] == ['! git status', 'the shell line ran'], shown[-3:]
        assert 'SHELL-OUTPUT' not in json.dumps(shown) and 'bash-input' not in json.dumps(shown), shown[-3:]
        # `!` alone or a shell line with a second line is a message.
        for text in ('!', '!ls\npwd'):
            before = len(traces('commands'))
            status, sent = root.request('POST', '/api/chat/%d/send' % commands_session, {'text': text})
            assert status == 200, sent
            received = wait(lambda: len(traces('commands')) > before and traces('commands')[-1], 'enveloped delivery')
            assert received.startswith('[wsx exchange ') and received.endswith(']\n' + text), received
            settle('commands')
        session = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == commands_session)
        assert call('session_close', {'session_id': commands_session, 'expected_revision': session['revision']})['type'] == 'ack'
        shutil.rmtree(third_root / '.claude')
        ok('commands: built-ins, project, user and plugin skills with one-line descriptions, a malformed file skipped, no file body; /compact and a `!` shell line typed as written, other text keeps the exchange line')

        # --- Compaction: the summary is one item where the conversation was compacted, without Claude's fixed words.
        for provider, expected in (('claude', ['summary', 'user', 'assistant']),
                                   ('pi', ['user', 'assistant', 'user', 'summary', 'user', 'assistant'])):
            name = 'compact-' + provider
            created = call('session_create', {'worktree_id': worktrees['project3'], 'label': name,
                'command': ['/usr/bin/env', 'WSX_FIXTURE_PROVIDER=' + provider, sys.executable, str(actor), str(work / (name + '.jsonl')),
                            str(work / (name + '.trace')), str(third_root), str(chart), str(outside)], 'rows': 12, 'cols': 100})
            assert created['type'] == 'created', created
            sessions[name] = created['data']['id']
            pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == sessions[name])
            wait(lambda: (next(p for p in call('snapshot')['data']['panes'] if p['id'] == pane)['agent'] or {}).get('provider') == provider, name + ' agent')
            for text in ('hello there', 'compact now', 'after the compaction'):
                settle(name)
                before = len(traces(name))
                status, sent = steady(root, 'POST', '/api/chat/%d/send' % sessions[name], {'text': text})
                assert status == 200, sent
                wait(lambda: len(traces(name)) > before, name + ' delivery')
            settle(name)
            status, compacted = steady(root, 'GET', '/api/chat/%d/timeline' % sessions[name])
            assert status == 200 and 'checkpoint' not in compacted, compacted
            assert [item['kind'] for item in compacted['items']] == expected, compacted['items']
            summary = next(item for item in compacted['items'] if item['kind'] == 'summary')
            assert summary['text'].startswith('## Goal\nFix the login redirect loop') and summary['text'].endswith('- Step 12 of the fix is done.'), summary
            assert summary['attachments'] == [] and summary['id'], summary
            assert compacted['items'][-1]['text'] == 'echo: after the compaction', compacted['items'][-1]
            assert compacted['model'] == {'claude': 'claude-opus-5-5', 'pi': 'gpt-6.1-sol'}[provider], compacted.get('model')
            if provider == 'claude':
                # A background-task notice is Claude's own record, not a message the person wrote.
                settle(name)
                before = len(traces(name))
                assert steady(root, 'POST', '/api/chat/%d/send' % sessions[name], {'text': 'task notice'})[0] == 200
                wait(lambda: len(traces(name)) > before, name + ' task notice')
                settle(name)
                # ^ settle can return before the turn reports working: wait for its answer.
                items = wait(lambda: (lambda items: items if items[-1].get('text') == 'The tests passed.' else None)(
                    steady(root, 'GET', '/api/chat/%d/timeline' % sessions[name])[1]['items']), name + ' task notice answer')
                assert [(item['kind'], item.get('text')) for item in items[-2:]] == [('user', 'task notice'), ('assistant', 'The tests passed.')], items
                assert not any('task-notification' in (item.get('text') or '') for item in items), items
                # /compact as Claude 2.1.300 runs it: working, then done, with no prompt. The kept
                # answer follows the new summary; the command shows as typed, its output not at all.
                before = len(traces(name))
                status, sent = steady(root, 'POST', '/api/chat/%d/send' % sessions[name], {'text': '/compact'})
                assert status == 200, sent
                wait(lambda: len(traces(name)) > before, name + ' /compact')
                # The hook reports a manual PreCompact, and the working row says so.
                wait(lambda: (steady(root, 'GET', '/api/chat/%d/timeline' % sessions[name])[1]['working'] or {}).get('activity')
                     == 'Compacting the conversation', name + ' compacting activity')
                wait(lambda: [(item['kind'], item.get('text')) for item in steady(root, 'GET', '/api/chat/%d/timeline' % sessions[name])[1]['items']]
                     == [('summary', '## Goal\nRun the tests.'), ('assistant', 'The tests passed.'), ('user', '/compact')], name + ' timeline after /compact')
                wait(lambda: next(p for p in call('snapshot')['data']['panes'] if p['id'] == pane)['agent']['state'] == 'done', name + ' done after /compact')
                # The next message is delivered and answered.
                before = len(traces(name))
                assert steady(root, 'POST', '/api/chat/%d/send' % sessions[name], {'text': 'after the new compaction'})[0] == 200
                wait(lambda: len(traces(name)) > before, name + ' delivery after /compact')
                settle(name)
                after = steady(root, 'GET', '/api/chat/%d/timeline' % sessions[name])[1]
                assert [(item['kind'], item.get('text')) for item in after['items'][-2:]] == [('user', 'after the new compaction'), ('assistant', 'echo: after the new compaction')], after['items']
                facts_compact = after['exchange']
            session = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == sessions[name])
            assert call('session_close', {'session_id': sessions[name], 'expected_revision': session['revision']})['type'] == 'ack'
        ok('compaction: the summary is one item where it happened (Claude: first, Pi: in place), Claude\'s preamble, closing instructions and transcript path removed; '
           'Claude 2.1.300 /compact: working then done, the kept answer after the summary, the command as typed, the next message delivered (exchange %s)' % facts_compact['state'])

        # --- Older transcripts in the shapes real sessions saved (Claude Code 2.1.302 to 2.1.310, 2026-10).
        def old_record(kind, uuid, parent, content=None, **extra):
            line = {'type': kind, 'uuid': uuid, 'parentUuid': parent, 'sessionId': 'fixture', 'timestamp': '2026-10-20T10:00:00.000Z'}
            if content is not None:
                line['message'] = {'role': kind, 'content': content}
            line.update(extra)
            return json.dumps(line) + '\n'
        # A `deferred_tools_record` attachment whose parent Claude never saved: the conversation
        # before it must still show. A long turn of more than 1 MiB of tool results after the
        # person's message: that message must still show.
        steps = ''.join(old_record('assistant', 'call-%d' % index, 'old-answer' if index == 0 else 'result-%d' % (index - 1),
                                   [{'type': 'tool_use', 'id': 'tool-%d' % index, 'name': 'Bash', 'input': {'description': 'Step %d' % index}}])
                        + old_record('user', 'result-%d' % index, 'call-%d' % index,
                                     [{'type': 'tool_result', 'tool_use_id': 'tool-%d' % index, 'content': 'x' * 150_000}])
                        for index in range(10))
        fixtures = {
            'old-dangling': (old_record('user', 'old-question', None, 'the old question')
                             + old_record('assistant', 'old-answer', 'old-question', [{'type': 'text', 'text': 'the old answer'}])
                             + old_record('attachment', 'deferred', 'never-saved', attachment={'type': 'deferred_tools_record'})
                             + old_record('assistant', 'newest', 'deferred', [{'type': 'text', 'text': 'the newest answer'}]),
                             [('user', 'the old question'), ('assistant', 'the old answer'), ('assistant', 'the newest answer')], True),
            'old-long-turn': (old_record('user', 'old-question', None, 'the long task')
                              + old_record('assistant', 'old-answer', 'old-question', [{'type': 'text', 'text': 'starting'}])
                              + steps + old_record('assistant', 'newest', 'result-9', [{'type': 'text', 'text': 'the task is done'}]),
                              # ^ After the next message the newest MiB holds a person message, but the chain goes
                              # on before it: the read goes back 4 MiB once, so the long turn before it still shows.
                              [('user', 'the long task'), ('assistant', 'starting')], True),
        }
        for name, (content, head, kept) in fixtures.items():
            (work / (name + '.jsonl')).write_text(content)
            created = call('session_create', {'worktree_id': worktrees['project3'], 'label': name,
                'command': ['/usr/bin/env', 'WSX_FIXTURE_LEAF=newest', sys.executable, str(actor), str(work / (name + '.jsonl')),
                            str(work / (name + '.trace')), str(third_root), str(chart), str(outside)], 'rows': 12, 'cols': 100})
            assert created['type'] == 'created', created
            sessions[name] = created['data']['id']
            pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == sessions[name])
            wait(lambda: (next(p for p in call('snapshot')['data']['panes'] if p['id'] == pane)['agent'] or {}).get('session_ref'), name + ' agent')
            status, opened = steady(root, 'GET', '/api/chat/%d/timeline' % sessions[name])
            assert status == 200 and opened['status'] == 'available', opened
            shown = [(item['kind'], item.get('text')) for item in opened['items'] if item['kind'] in ('user', 'assistant')]
            assert shown[:len(head)] == head, (name, shown)
            # A message to the old conversation is delivered, answered and shown after it.
            settle(name)
            before = len(traces(name))
            status, sent = steady(root, 'POST', '/api/chat/%d/send' % sessions[name], {'text': 'continue it'})
            assert status == 200, sent
            wait(lambda: len(traces(name)) > before, name + ' delivery')
            settle(name)
            items = wait(lambda: (lambda items: items if items[-1].get('text') == 'echo: continue it' else None)(
                steady(root, 'GET', '/api/chat/%d/timeline' % sessions[name])[1]['items']), name + ' answer')
            shown = [(item['kind'], item.get('text')) for item in items if item['kind'] in ('user', 'assistant')]
            assert (shown[:len(head)] == head) == kept and shown[-2:] == [('user', 'continue it'), ('assistant', 'echo: continue it')], (name, shown)
            session = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == sessions[name])
            assert call('session_close', {'session_id': sessions[name], 'expected_revision': session['revision']})['type'] == 'ack'
        ok('older transcripts: a record whose parent Claude never saved keeps the conversation before it; '
           'a person message more than 1 MiB before the end still shows; a message to either is answered after it')

        # --- Media: upload, attach to a prompt, agent-produced files, sniffing.
        photo = png(20, 5)
        digest = hashlib.sha256(photo).hexdigest()
        status, uploaded = owner.request('PUT', '/api/media?name=..%2F..%2Fphoto.exe', photo)
        assert status == 200 and uploaded['hash'] == digest and uploaded['url'] == 'media/%s.png' % digest, uploaded
        assert uploaded['name'] == 'photo.exe' and uploaded['mime'] == 'image/png' and uploaded['image']
        again = owner.request('PUT', '/api/media?name=copy.png', photo, shape='stripped')[1]
        assert again['hash'] == digest and again['name'] == 'photo.exe'  # content-addressed
        for shape in ('prefixed', 'stripped'):
            status, headers, body = owner.request('GET', '/' + uploaded['url'], shape=shape, raw=True)
            assert status == 200 and body == photo and headers['content-type'] == 'image/png'
            cache = headers['cache-control']
            assert 'immutable' in cache and 'private' in cache and 0 < int(cache.split('max-age=')[1].split(',')[0]) <= 600
        status, headers, body = owner.request('GET', '/' + uploaded['url'], headers={'Range': 'bytes=8-15'}, raw=True)
        assert status == 206 and body == photo[8:16] and headers['content-range'] == 'bytes 8-15/%d' % len(photo)
        status, headers, _ = owner.request('GET', '/' + uploaded['url'], headers={'Range': 'bytes=999999-'}, raw=True)
        assert status == 416 and headers['content-range'] == 'bytes */%d' % len(photo)
        assert owner.request('GET', '/' + uploaded['url'], headers={'Range': 'bytes=0-1,4-5'})[1] == photo
        owner.cookie, saved = None, owner.cookie
        assert owner.request('GET', '/' + uploaded['url'])[0] == 401
        owner.cookie = saved
        assert owner.request('PUT', '/api/media?name=a.zip', b'PK\x03\x04' + b'\x00' * 64)[0] == 415
        assert owner.request('PUT', '/api/media?name=a.bin', b'\x7fELF\x00\x01\x02\xff\xfe' * 8)[0] == 415
        assert owner.request('PUT', '/api/media?name=big.png', png(200, 1))[0] == 413
        assert owner.request('PUT', '/api/media?name=empty', b'')[0] == 415
        note = owner.request('PUT', '/api/media?name=note.md', '# plan\nstep\n'.encode())[1]
        assert note['mime'].startswith('text/plain') and not note['image']
        status, headers, _ = owner.request('GET', '/' + note['url'], raw=True)
        assert headers['content-disposition'].startswith('attachment') and headers['x-content-type-options'] == 'nosniff'
        settle('owner')
        status, sent = owner.request('POST', chat + '/send', {'text': 'look at this', 'attachments': [digest]})
        assert status == 200, sent
        delivered = wait(lambda: len(traces('owner')) == 3 and traces('owner')[2], 'attachment delivery')
        stored_path = Path(delivered.splitlines()[-1].rsplit(' (', 1)[0])
        assert delivered.endswith('[attached files]\n%s (photo.exe)' % stored_path)
        assert stored_path.read_bytes() == photo and stored_path.name == digest + '.png'
        assert stored_path.parent == (work / 'state/wsx/web/owner/media').resolve()
        history = wait(lambda: (lambda data: data if len(data['messages']) == 6 else None)(messages(owner, sessions['owner'])), 'attachment history')
        assert history['messages'][4]['text'] == 'look at this'
        assert [item['hash'] for item in history['messages'][4]['attachments']] == [digest]
        assert owner.request('POST', chat + '/send', {'text': 'x', 'attachments': ['0' * 64]})[1]['error']['code'] == 'attachment_missing'
        settle('owner')
        owner.request('POST', chat + '/send', {'text': 'please make chart'})
        history = wait(lambda: (lambda data: data if len(data['messages']) == 8 else None)(messages(owner, sessions['owner'])), 'chart history')
        produced = history['messages'][7]['attachments']
        assert len(produced) == 1 and produced[0]['image'] and produced[0]['name'] == 'chart.png', produced
        assert produced[0]['hash'] == hashlib.sha256(chart.read_bytes()).hexdigest()
        assert owner.request('GET', '/' + produced[0]['url'])[1] == chart.read_bytes()
        # Not offered: a symlink that leaves the project, a parent path, an absolute
        # outside path, a hidden file, a credential-like name and anything under .git.
        settle('owner')
        owner.request('POST', chat + '/send', {'text': 'leak'})
        history = wait(lambda: (lambda data: data if len(data['messages']) == 10 else None)(messages(owner, sessions['owner'])), 'leak history')
        assert history['messages'][9]['attachments'] == [], history['messages'][9]
        assert hashlib.sha256(outside.read_bytes()).hexdigest() not in json.dumps(history)

        # --- Timeline: the saved conversation gains a tool call, then its result, then the answer.
        def timeline():
            # These scenarios poll often; when the device's request budget is spent, wait for it.
            status, data = steady(owner, 'GET', chat + '/timeline')
            assert status == 200, data
            return data

        def open_gate(name):
            (work / ('owner.' + name)).write_text('')

        for broken in (False, True):
            settle('owner')
            started = time.time() * 1000
            # The per-device message budget (20 a minute) may be spent by now: wait for it.
            status, sent = wait(lambda: (lambda answer: answer if answer[0] != 429 else None)(
                owner.request('POST', chat + '/send', {'text': 'use tool' + (' broken' if broken else '')})), 'message budget', timeout=20)
            assert status == 200, sent
            running = wait(lambda: (lambda data: data if data['items'] and data['items'][-1]['kind'] == 'tool' else None)(timeline()), 'tool call in the timeline')
            assert [item['kind'] for item in running['items'][-3:]] == ['user', 'thinking', 'tool'], running['items'][-3:]
            step = running['items'][-1]
            # The title is one clean line from the input; the input and the thinking text are never sent.
            assert step['tool'] == {'name': 'Bash', 'title': 'List the files', 'status': 'running'} and step['text'] is None
            assert started - 2000 <= step['at_unix_ms'] <= time.time() * 1000 + 2000, step
            assert 'private reasoning' not in json.dumps(running) and 'rm -rf' not in json.dumps(running)
            assert running['working']['activity'] == 'List the files'
            assert started - 2000 <= running['working']['since_unix_ms'] <= time.time() * 1000, running['working']
            # The change reaches the page within about a second of the write.
            del events.seen[:]
            opened = time.monotonic()
            open_gate('result')
            events.expect('history', lambda data: data['session_id'] == sessions['owner'], timeout=3)
            # ^ An earlier change of the session can signal first: the result must show soon after.
            finished = wait(lambda: (lambda data: data if data['items'][-1]['tool']['status'] != 'running' else None)(timeline()),
                            'tool result in the timeline', timeout=2)
            assert time.monotonic() - opened < 2, time.monotonic() - opened
            assert finished['items'][-1]['tool']['status'] == ('error' if broken else 'ok'), finished['items'][-1]
            assert 'tool output text' not in json.dumps(finished)
            assert finished['working']['activity'] == 'Working'
            # One start time for the whole turn.
            assert finished['working']['since_unix_ms'] == running['working']['since_unix_ms'], (finished['working'], running['working'])
            open_gate('answer')
            answered = wait(lambda: (lambda data: data if data['items'][-1]['kind'] == 'assistant' and data['working'] is None else None)(timeline()), 'answer and done')
            assert answered['items'][-1]['text'] == 'Listed the files.' and answered['items'][-1]['tool'] is None
            assert answered['items'][-2]['id'] == step['id']  # ids are stable between reads
        settle('owner')
        ok('timeline: tool call running, its result ok or failed, the answer; thinking and tool content never sent; working follows wsxd; change signalled within a second')

        # --- Queue: messages sent while the agent works wait in order and survive a server restart.
        def send(text, **extra):
            return steady(owner, 'POST', chat + '/send', dict({'text': text}, **extra))

        def delivered():
            return [text.split(']\n', 1)[-1] for text in traces('owner')]

        assert send('use tool')[0] == 200
        wait(lambda: timeline()['working'], 'working turn')
        # wsxd says when the turn began, not this server.
        began = timeline()['working']['since_unix_ms']
        owner_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == sessions['owner'])
        assert began == next(p for p in call('snapshot')['data']['panes'] if p['id'] == owner_pane)['agent']['state_since_unix_ms']
        del events.seen[:]
        status, first = send('queued one')
        assert status == 200 and first['queued']['text'] == 'queued one' and first['queued']['failed'] is None, first
        events.expect('history', lambda data: data['session_id'] == sessions['owner'])
        assert send('queued two')[1]['queued']['id'] > first['queued']['id']
        extra = [send('extra %d' % index)[1]['queued']['id'] for index in range(3)]
        status, full = send('one too many')
        assert status == 409 and full['error']['code'] == 'queue_full', full
        assert [item['text'] for item in timeline()['queue']] == ['queued one', 'queued two', 'extra 0', 'extra 1', 'extra 2']
        for queued in extra:
            assert steady(owner, 'DELETE', chat + '/queue/%d' % queued)[0] == 200
        assert steady(owner, 'DELETE', chat + '/queue/%d' % extra[0])[0] == 404
        assert steady(owner, 'DELETE', '/api/chat/%d/queue/1' % sessions['family'])[0] == 404  # another project's session
        assert steady(root, 'DELETE', chat + '/queue/%d' % first['queued']['id'])[0] == 404  # another site's queue
        assert delivered()[-1] == 'use tool', 'a queued message was typed while the agent worked'
        # The queue is site state: a restarted server still has it and still delivers it.
        queue_file = work / 'state/wsx/web/owner/queue.json'
        assert queue_file.stat().st_mode & 0o777 == 0o600 and 'queued two' in queue_file.read_text()
        owner_process = processes[[process.args[-1] for process in processes if isinstance(process.args, list)].index(str(files['owner']))]
        # The server ends when its open pages are gone: close this test's event stream first.
        events.connection.sock.shutdown(socket.SHUT_RDWR)
        owner_process.terminate()
        owner_process.wait(timeout=5)
        owner_server = start(work, [str(wsx), 'web', 'serve', '--site', str(files['owner'])], env=env, cwd=project,
                             stdout=subprocess.DEVNULL, stderr=(work / 'owner.log').open('a'))
        processes.append(owner_server)
        wait(lambda: up(owner), 'restarted web server')
        events = Events(owner)
        streams.append(events)
        # A restarted server keeps the turn's start time, in the chat and in the session list.
        assert timeline()['working']['since_unix_ms'] == began
        rows = events.expect('workspace')['sessions']
        assert next(row for row in rows if row['id'] == sessions['owner'])['state_since_unix_ms'] == began, rows
        assert [item['text'] for item in timeline()['queue']] == ['queued one', 'queued two']
        # ^ A message held for more than 10 s brings the end of the screen (docs/web-client-chat.md "Timeline").
        screen = wait(lambda: timeline()['screen'], 'screen of a held message', timeout=15)
        # ^ What is above Claude's editor: its working line, not the editor or the footer.
        assert 0 < len(screen) <= 8 and screen[-1].startswith('✻ Sautéing'), screen
        assert not any(row.startswith(('❯', '─')) or 'auto mode' in row for row in screen), screen
        open_gate('result')
        open_gate('answer')
        wait(lambda: delivered()[-2:] == ['queued one', 'queued two'], 'queued messages delivered in order', timeout=20)
        wait(lambda: messages(owner, sessions['owner'])['messages'][-1]['text'] == 'echo: queued two', 'queued turn')
        assert timeline()['queue'] == [] and not queue_file.exists() or json.loads(queue_file.read_text())['sessions'] == {}
        assert timeline()['screen'] is None
        settle('owner')
        # Send now: a waiting message typed into the working Claude, which takes it at its next
        # step (docs/web-client-chat.md "Queue and Stop"). The conversation shows it, so it leaves.
        assert send('use tool')[0] == 200
        wait(lambda: timeline()['working'], 'working turn for a steer')
        steer = send('steer this turn')[1]['queued']['id']
        assert [item['text'] for item in timeline()['queue']] == ['steer this turn']
        assert steady(owner, 'POST', chat + '/queue/%d/steer' % steer, {})[0] == 200
        wait(lambda: any(item['kind'] == 'user' and item['text'] == 'steer this turn' for item in timeline()['items']), 'steered message in the conversation')
        wait(lambda: timeline()['queue'] == [], 'steered message left the queue')
        assert steady(owner, 'POST', chat + '/queue/%d/steer' % steer, {})[0] == 404
        open_gate('result')
        open_gate('answer')
        wait(lambda: timeline()['working'] is None, 'steered turn ended')
        # Not during a turn: the agent takes a message as usual, so there is nothing to steer.
        status, refused = steady(owner, 'POST', chat + '/queue/999/steer', {})
        assert status == 404, (status, refused)
        settle('owner')
        # A queued message the agent drops stays, with its text, until the conversation shows it;
        # Send again delivers it once more, and then it leaves the queue.
        assert send('use tool')[0] == 200
        wait(lambda: timeline()['working'], 'working turn')
        assert send('drop me once')[1]['exchange'] is None
        open_gate('result')
        open_gate('answer')
        # ^ No receipt names its round: after 5 s wsxd ends it as not taken, and the server sends
        # it once more by itself (docs/agent-orchestration.md "Taken, not typed").
        wait(lambda: timeline()['queue'] == [] and [item['text'] for item in timeline()['items'] if item['kind'] == 'user'][-1] == 'drop me once',
             'sent again and shown', timeout=30)
        assert delivered().count('drop me once') == 2
        # ^ The conversation says it happened, just before the message the second time.
        kinds = [(item['kind'], item['text']) for item in timeline()['items']][-3:]
        assert kinds[0] == ('notice', 'Sent again: Claude did not take it the first time.') and kinds[1] == ('user', 'drop me once'), kinds
        settle('owner')
        # Taken before it is saved: the prompt hook reports the turn before Claude writes the
        # prompt. The sent message stays until the conversation shows it, so it never vanishes.
        assert send('slow save')[0] == 200
        wait(lambda: timeline()['exchange']['state'] == 'working_observed', 'the round taken')
        time.sleep(2)  # ^ more than two rounds of the server's observer
        held = timeline()
        assert [item['text'] for item in held['queue']] == ['slow save'] and held['queue'][0]['delivered_unix_ms'], held['queue']
        assert not any(item['kind'] == 'user' and item['text'] == 'slow save' for item in held['items'])
        open_gate('saved')
        wait(lambda: timeline()['queue'] == [] and [item['text'] for item in timeline()['items'] if item['kind'] == 'user'][-1] == 'slow save',
             'the saved message replaces the sent one')
        settle('owner')
        # A message the agent never takes, while a person types in the terminal: the typed turn is
        # not the message's turn, so its round fails at once; it goes once more, then stays failed.
        always = send('drop me always')[1]['queued']['id']
        wait(lambda: delivered()[-1] == 'drop me always', 'the dropped message delivered')
        assert call('terminal_acquire', {'pane_id': owner_pane, 'client_id': 6, 'takeover': False})['type'] == 'ack'
        assert call('terminal_input', {'pane_id': owner_pane, 'client_id': 6, 'bytes': list(b'typed by a person\r')})['type'] == 'ack'
        assert call('terminal_release', {'pane_id': owner_pane, 'client_id': 6})['type'] == 'ack'
        failed = wait(lambda: next((item for item in timeline()['queue'] if item['id'] == always and item['failed']), None),
                      'not taken twice', timeout=30)
        assert failed['resent'] and failed['failed'] == 'not_taken: Claude did not take this message', failed
        assert delivered().count('drop me always') == 2 and 'typed by a person' in [m['text'] for m in messages(owner, sessions['owner'])['messages']]
        assert steady(owner, 'DELETE', chat + '/queue/%d' % always)[0] == 200
        settle('owner')
        # The chat outlives the agent (docs/web-client-chat.md "Conversations"): Claude exits and its
        # pane is a shell again; a message types `claude --resume` there and goes to the same
        # conversation. A new conversation in the pane keeps the earlier one readable above it.
        (bin_dir / 'claude').write_text('#!/bin/sh\nexec %s %s "%s/resume-${WSX_FIXTURE_CONVERSATION:-fixture}.jsonl" %s %s %s %s\n'
                                        % (sys.executable, actor, work, work / 'resume.trace', project, chart, outside))
        (bin_dir / 'claude').chmod(0o755)
        created = call('session_create', {'worktree_id': worktrees['project'], 'label': 'resume chat',
                                          'command': ['/bin/sh', '-c', 'claude; PS1="$ " exec /bin/sh'], 'rows': 12, 'cols': 100})
        assert created['type'] == 'created', created
        sessions['resume'] = created['data']['id']
        resume_chat = '/api/chat/%d' % sessions['resume']
        resume_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == sessions['resume'])

        def resume_agent():
            return next(p for p in call('snapshot')['data']['panes'] if p['id'] == resume_pane)['agent']

        def resume_timeline():
            return steady(owner, 'GET', resume_chat + '/timeline')[1]

        def said(words):
            return wait(lambda: any(item['kind'] == 'assistant' and item['text'] == words for item in resume_timeline()['items']), words, timeout=30)
        wait(lambda: (resume_agent() or {}).get('attached'), 'resume agent report')
        settle('resume')
        assert steady(owner, 'POST', resume_chat + '/send', {'text': 'before the exit'})[0] == 200
        said('echo: before the exit')
        settle('resume')
        assert steady(owner, 'POST', resume_chat + '/send', {'text': 'leave now'})[0] == 200
        wait(lambda: not resume_agent()['attached'], 'the agent detached')
        exited = wait(lambda: (lambda data: data if any(item['kind'] == 'notice' and item['text'] == 'Claude exited.' for item in data['items']) else None)(resume_timeline()),
                      'the exit line', timeout=20)
        assert exited['session']['conversation'] and 'echo: before the exit' in [item['text'] for item in exited['items']], exited['session']
        status, sent = steady(owner, 'POST', resume_chat + '/send', {'text': 'after the exit'})
        assert status == 200 and sent['exchange'] is None, sent
        said('echo: after the exit')
        lines = [item['text'] for item in resume_timeline()['items'] if item['kind'] == 'notice']
        assert lines[-2:] == ['Claude exited.', 'Resumed Claude in the same conversation.'], lines
        assert 'Same conversation, continued' not in lines, lines
        settle('resume')
        # A new conversation in the pane, started by hand: a line, and the earlier one stays above it.
        assert steady(owner, 'POST', resume_chat + '/send', {'text': 'leave now'})[0] == 200
        wait(lambda: not resume_agent()['attached'], 'the agent detached again')
        assert call('terminal_acquire', {'pane_id': resume_pane, 'client_id': 7, 'takeover': True})['type'] == 'ack'
        assert call('terminal_input', {'pane_id': resume_pane, 'client_id': 7, 'bytes': list(b'WSX_FIXTURE_CONVERSATION=second claude\r')})['type'] == 'ack'
        assert call('terminal_release', {'pane_id': resume_pane, 'client_id': 7})['type'] == 'ack'
        wait(lambda: (resume_agent() or {}).get('attached'), 'the second conversation')
        settle('resume')
        assert steady(owner, 'POST', resume_chat + '/send', {'text': 'in the second'})[0] == 200
        said('echo: in the second')
        # ^ One read: an item that arrives between two reads would move the divider.
        items = resume_timeline()['items']
        shown = [(item['kind'], item['text']) for item in items]
        divider = shown.index(('notice', 'New conversation'))
        assert ('assistant', 'echo: after the exit') in shown[:divider] and ('assistant', 'echo: in the second') in shown[divider:], shown
        assert all(item['id'].startswith('earlier-') for item in items[:divider] if item['kind'] != 'notice'), items[:divider]
        # The same conversation started again by hand reads as continued.
        settle('resume')
        assert steady(owner, 'POST', resume_chat + '/send', {'text': 'leave now'})[0] == 200
        wait(lambda: not resume_agent()['attached'], 'the agent detached a third time')
        assert call('terminal_acquire', {'pane_id': resume_pane, 'client_id': 7, 'takeover': True})['type'] == 'ack'
        assert call('terminal_input', {'pane_id': resume_pane, 'client_id': 7, 'bytes': list(b'WSX_FIXTURE_CONVERSATION=second claude\r')})['type'] == 'ack'
        assert call('terminal_release', {'pane_id': resume_pane, 'client_id': 7})['type'] == 'ack'
        wait(lambda: [item['text'] for item in resume_timeline()['items'] if item['kind'] == 'notice'][-1] == 'Same conversation, continued',
             'the continued line', timeout=20)
        settle('resume')
        # ^ An agent started by hand runs in its shell's job, not the pane's process group: end it.
        assert steady(owner, 'POST', resume_chat + '/send', {'text': 'leave now'})[0] == 200
        wait(lambda: not resume_agent()['attached'], 'the hand-started agent left')
        # The newest person wins: a wsx window holds the pane, a message from the page still
        # goes through, the window is told that control moved, and nobody holds the pane after.
        with wsx_window(owner_pane) as seen:
            assert send('typed over the window')[0] == 200
            told_control_moved(seen)
            wait(lambda: delivered()[-1] == 'typed over the window', 'message typed past the window')
        lease_free(sessions['owner'])
        settle('owner')
        # A delivery the daemon refuses stays in the queue with the daemon's words and stops the queue.
        # wsxd lets one writer exchange be active in a worktree. A read-only turn came last, so the
        # page starts a new writer exchange, and a writer on a second agent there refuses it.
        created = call('session_create', {'worktree_id': worktrees['project'], 'label': 'helper chat',
            'command': [sys.executable, str(actor), str(work / 'helper.jsonl'), str(work / 'helper.trace'),
                        str(work / 'project'), str(chart), str(outside)], 'rows': 12, 'cols': 100})
        assert created['type'] == 'created', created
        sessions['helper'] = created['data']['id']
        helper_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == sessions['helper'])
        wait(lambda: next(p for p in call('snapshot')['data']['panes'] if p['id'] == helper_pane)['agent'], 'helper agent report')
        settle('helper')
        turn = call('agent_exchange_create', {'pane_id': owner_pane, 'prompt': 'use tool', 'timeout_ms': 60000})
        assert turn['type'] == 'agent_exchange', turn
        wait(lambda: timeline()['working'], 'working turn')
        failing = send('held back')[1]['queued']['id']
        send('after the refusal')
        writer = call('agent_exchange_create', {'pane_id': helper_pane, 'prompt': 'use tool', 'timeout_ms': 60000, 'access': 'writer'})
        assert writer['type'] == 'agent_exchange', writer
        open_gate('result')
        open_gate('answer')
        # A failed message does not hold back the next one; here the next meets the same writer.
        failed = wait(lambda: (lambda queue: queue if len(queue) == 2 and all(item['failed'] for item in queue) else None)(timeline()['queue']), 'failed delivery', timeout=10)
        assert failed[0]['id'] == failing and all('write_claim_conflict' in item['failed'] for item in failed), failed
        assert [item['text'] for item in timeline()['queue']] == ['held back', 'after the refusal'] and delivered()[-1] == 'use tool'
        # The other writer finishes; a retry goes through.
        (work / 'helper.result').write_text('')
        (work / 'helper.answer').write_text('')
        wait(lambda: next(item for item in call('agent_exchange_list', {'pane_id': helper_pane})['data']
                          if item['id'] == writer['data']['exchange']['id'])['state'] in ('done_observed', 'completed'), 'helper writer finished', timeout=20)
        helper_id = sessions.pop('helper')
        helper = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == helper_id)
        assert call('session_close', {'session_id': helper['id'], 'expected_revision': helper['revision']})['type'] == 'ack'
        assert steady(owner, 'POST', chat + '/queue/%d/retry' % failing, {})[0] == 200
        wait(lambda: delivered()[-1] == 'held back', 'retried message delivered', timeout=20)
        assert steady(owner, 'POST', chat + '/queue/%d/retry' % failed[1]['id'], {})[0] == 200
        wait(lambda: delivered()[-2:] == ['held back', 'after the refusal'], 'retried queue delivered in order', timeout=20)
        wait(lambda: timeline()['queue'] == [] and timeline()['working'] is None, 'queue drained')
        settle('owner')
        # A delivery that fails is kept with wsxd's reason, never dropped. The person typed a
        # line in the agent's editor while it worked; this fixture, unlike Claude, cannot stash it.
        assert send('use tool')[0] == 200
        wait(lambda: timeline()['working'], 'working turn')
        stuck = send('after the typed line')[1]['queued']['id']

        def type_in_pane(data):
            assert call('terminal_acquire', {'pane_id': owner_pane, 'client_id': 6, 'takeover': False})['type'] == 'ack'
            assert call('terminal_input', {'pane_id': owner_pane, 'client_id': 6, 'bytes': list(data)})['type'] == 'ack'
            assert call('terminal_release', {'pane_id': owner_pane, 'client_id': 6})['type'] == 'ack'
        type_in_pane(b'unsent line')
        open_gate('result')
        open_gate('answer')
        failed = wait(lambda: (lambda queue: queue if queue and queue[0]['failed'] else None)(timeline()['queue']), 'failed delivery kept', timeout=15)
        assert failed[0]['id'] == stuck and failed[0]['failed'].startswith('editor_stash_unconfirmed'), failed
        assert timeline()['exchange']['failure'].startswith('editor_stash_unconfirmed'), timeline()['exchange']
        # The person sends the line from the terminal; the retry then goes through.
        type_in_pane(b'\r')
        wait(lambda: delivered()[-1].startswith('unsent line'), 'the typed line sent from the terminal')
        settle('owner')
        assert steady(owner, 'POST', chat + '/queue/%d/retry' % stuck, {})[0] == 200
        wait(lambda: delivered()[-1] == 'after the typed line', 'retried after the typed line', timeout=20)
        wait(lambda: timeline()['queue'] == [] and timeline()['exchange']['failure'] is None, 'delivered without a failure')
        settle('owner')
        # Claude draws its editor idle a moment after the turn ends. wsxd says "not yet"
        # (editor_unavailable) meanwhile; the waiting message goes once the editor shows.
        assert send('slow editor')[0] == 200
        wait(lambda: timeline()['working'], 'slow editor turn')
        slow = send('after the slow editor')[1]['queued']['id']
        seen = []
        wait(lambda: seen.append([item.get('failed') for item in timeline()['queue']]) or delivered()[-1] == 'after the slow editor',
             'the message after a slow editor', timeout=20)
        assert not any(any(reasons) for reasons in seen), seen
        assert all(item['id'] != slow for item in timeline()['queue'])
        settle('owner')
        # A message that fails does not hold back the next one; it stays with its reason.
        # Its attachment expires while it waits (the root site keeps media for 4 s).
        assert send('use tool')[0] == 200
        wait(lambda: timeline()['working'], 'working turn')

        def via_root(text, **extra):
            return steady(root, 'POST', chat + '/send', dict({'text': text}, **extra))

        def root_queue():
            return steady(root, 'GET', chat + '/timeline')[1]['queue']
        status, note = root.request('PUT', '/api/media?name=note.txt', b'a short note\n')
        assert status == 200, note
        status, waiting = via_root('read the note', attachments=[note['hash']])
        assert status == 200 and 'queued' in waiting, (status, waiting)
        expired = waiting['queued']['id']
        assert 'queued' in via_root('after the note')[1]
        time.sleep(5)
        open_gate('result')
        open_gate('answer')
        wait(lambda: delivered()[-1] == 'after the note', 'the message after a failed one', timeout=20)
        stuck = root_queue()
        assert [item['id'] for item in stuck] == [expired] and stuck[0]['failed'].startswith('an attachment expired'), stuck
        settle('owner')
        # A new message to an idle agent goes past the failed one: at once, or queued until wsxd
        # sees the agent at its wait boundary a moment later.
        status, direct = via_root('past the stuck one')
        assert status == 200 and (direct['exchange'] or (direct['queued'] and not direct['queued']['failed'])), direct
        wait(lambda: delivered()[-1] == 'past the stuck one', 'a new message past the failed one')
        assert [item['id'] for item in root_queue()] == [expired]
        assert steady(root, 'DELETE', chat + '/queue/%d' % expired)[0] == 200
        settle('owner')
        # A page asks from the start of its last turn and gets only what follows it.
        full = timeline()
        turn = [item for item in full['items'] if item['kind'] == 'user'][-1]
        part = steady(owner, 'GET', chat + '/timeline?from=%s&at=%d' % (urllib.parse.quote(turn['id']), turn['at_unix_ms']))[1]
        assert part['from'] == turn['id'] and part['items'] == full['items'][full['items'].index(turn):], part
        assert len(json.dumps(part)) < len(json.dumps(full)) / 2, (len(json.dumps(part)), len(json.dumps(full)))
        # An item the server no longer has, or a different record under that id: the full timeline.
        for query in ('from=gone&at=1', 'from=%s&at=%d' % (urllib.parse.quote(turn['id']), turn['at_unix_ms'] + 1)):
            stale = steady(owner, 'GET', chat + '/timeline?' + query)[1]
            assert stale['from'] is None and stale['items'] == full['items'], query
        ok('queue: waits while the agent works, in order, limit of five, removal, survives a server restart with the turn\'s wsxd start time, sent past a wsx window that is told control moved, failed delivery or a refusal keeps its reason without holding back later messages, an editor drawn idle late is waited for, partial timeline from the last turn')

        # --- Stop: the turn ends, the exchange is cancelled, and a waiting message goes next.
        assert send('use tool')[0] == 200
        wait(lambda: timeline()['working'], 'working turn')
        send('after the stop')
        # A wsx window that holds the pane does not block Stop: Stop takes the pane.
        with wsx_window(owner_pane) as seen:
            status, stopping = steady(owner, 'POST', chat + '/cancel', {})
            assert status == 200 and stopping['state'] == 'cancel_requested', stopping
            told_control_moved(seen)
        # The stopped exchange ends as cancelled. It is not continued: the waiting message starts a new one.
        def stopped_state():
            listed = call('agent_exchange_list', {'pane_id': owner_pane})['data']
            return next(item['state'] for item in listed if item['id'] == stopping['id'])
        wait(lambda: stopped_state() != 'cancel_requested', 'stopped turn')
        assert stopped_state() == 'cancelled', stopped_state()
        wait(lambda: delivered()[-1] == 'after the stop', 'message after the stop')
        assert timeline()['exchange']['id'] != stopping['id']
        wait(lambda: messages(owner, sessions['owner'])['messages'][-1]['text'] == 'echo: after the stop', 'turn after the stop')
        settle('owner')
        ok('stop: taken from a wsx window that holds the pane, the turn ends, the exchange is cancelled and the queue continues')
        assert not any(item['name'] in ('.env', 'api-token.txt', 'config') for item in
                       (work / 'state/wsx/web/owner/media/index.json').exists() and json.loads((work / 'state/wsx/web/owner/media/index.json').read_text()) or [])
        ok('media: upload, content hash URL, immutable cache, range, sniff allowlist, prompt attachment, agent file, no symlink or traversal')

        # --- TTL and quota on the root-shaped site (max 4096 B, quota 9000 B, TTL 4 s).
        blobs = [png(33 + index, index) for index in range(3)]
        assert all(3000 < len(blob) <= 4096 for blob in blobs), [len(blob) for blob in blobs]
        urls = []
        for index, blob in enumerate(blobs):
            status, entry = steady(root, 'PUT', '/api/media?name=%d.png' % index, blob)
            assert status == 200, entry
            urls.append('/' + entry['url'])
            time.sleep(1.05)  # distinct creation seconds, so "oldest" is well defined
        media_dir = work / 'state/wsx/web/root/media'
        assert steady(root, 'GET', urls[0])[0] == 404, 'oldest entry was not evicted at quota'
        assert steady(root, 'GET', urls[1])[1] == blobs[1] and steady(root, 'GET', urls[2])[1] == blobs[2]
        assert sorted(p.suffix for p in media_dir.iterdir()) == ['.json', '.png', '.png']
        max_age = int(steady(root, 'GET', urls[2], raw=True)[1]['cache-control'].split('max-age=')[1].split(',')[0])
        assert 0 < max_age <= 4
        time.sleep(4.2)
        assert steady(root, 'GET', urls[1])[0] == 404 and steady(root, 'GET', urls[2])[0] == 404
        status, entry = steady(root, 'PUT', '/api/media?name=late.png', png(9, 99))
        assert status == 200 and [p.name for p in media_dir.glob('*.png')] == [entry['url'].split('/')[1]]
        ok('media: oldest-first quota eviction, TTL expiry, sweep removes files')

        # --- Files and terminal on the owner site.
        files_of = '/api/files?project=%d&path=' % projects['project']
        file_of = '/api/file?project=%d&path=' % projects['project']
        status, listing = steady(owner, 'GET', files_of)
        # .git is never listed, read or written: its config can run commands on the host.
        assert [(e['name'], e['directory']) for e in listing['entries']] == [('out', True), ('sub', True), ('.env', False), ('notes.txt', False)]
        assert 'escape' not in [e['name'] for e in listing['entries']]  # symlinks are not listed
        assert steady(owner, 'GET', file_of + 'sub/deep.txt', shape='stripped')[1]['text'] == 'deep\n'
        for bad, status in (('../outside.png', 400), ('/etc/hosts', 400), ('sub/../../x', 400), ('.git/config', 400),
                            ('escape/.codex', 403), ('out/leak.png', 403), ('missing.txt', 404)):
            assert steady(owner, 'GET', file_of + bad)[0] == status, bad
        assert steady(owner, 'GET', files_of + 'escape')[0] == 403
        write = {'project': projects['project'], 'path': 'notes.txt', 'original': 'first line\n', 'content': 'edited\n'}
        assert steady(owner, 'PUT', '/api/file', write)[0] == 200 and (project / 'notes.txt').read_text() == 'edited\n'
        assert steady(owner, 'PUT', '/api/file', write)[1]['error']['code'] == 'file_changed'
        assert steady(owner, 'PUT', '/api/file', dict(write, path='out/leak.png'))[0] == 403
        git_config = (project / '.git/config').read_text()
        assert steady(owner, 'PUT', '/api/file', {'project': projects['project'], 'path': '.git/config', 'original': git_config, 'content': 'x'})[0] == 400
        assert (project / '.git/config').read_text() == git_config
        assert steady(owner, 'GET', files_of + '.git')[0] == 400
        assert outside.read_bytes() == png(8, 200)
        # A second granted project has its own root. An ungranted or unknown project does not exist.
        third = '?project=%d&path=' % projects['project3']
        assert [e['name'] for e in steady(owner, 'GET', '/api/files' + third)[1]['entries']] == ['third.txt']
        assert steady(owner, 'GET', '/api/file' + third + 'third.txt')[1]['text'] == 'third project\n'
        assert steady(owner, 'GET', '/api/file' + third + 'notes.txt')[0] == 404  # a file of the first project
        for foreign in (projects['project2'], 987654):
            assert steady(owner, 'GET', '/api/files?project=%d&path=' % foreign)[0] == 404
            assert steady(owner, 'GET', '/api/file?project=%d&path=notes.txt' % foreign)[0] == 404
            assert steady(owner, 'PUT', '/api/file', dict(write, project=foreign))[0] == 404
        assert steady(owner, 'GET', '/api/files?path=')[0] == 400  # the project is required
        # The terminal types into the agent's pane like a keyboard does.
        settle('owner')
        typing = Terminal(owner, sessions['owner'], '&rows=12&cols=100')
        typing.control('replay')
        typing.until(lambda: '❯'.encode() in typing.output, 'agent screen replay')
        typing.send(b'typed in terminal\r')
        wait(lambda: traces('owner')[-1] == 'typed in terminal', 'terminal input')
        typing.drop()
        lease_free(sessions['owner'])
        # Let this turn finish here; its done transition must not reach the push checks below.
        wait(lambda: messages(owner, sessions['owner'])['messages'][-1]['text'] == 'echo: typed in terminal', 'terminal turn')
        settle('owner')
        # The snapshot terminal API is gone.
        assert steady(owner, 'GET', '/api/terminal/%d' % sessions['owner'])[0] == 404
        assert steady(owner, 'POST', '/api/terminal/%d/input' % sessions['owner'], {'key': 'enter'})[0] in (404, 405)
        assert steady(owner, 'GET', '/api/activity')[0] == 404  # the inbox replaced the activity panel
        # A site without agent_write does not continue the owner's writer exchange.
        status, other = steady(root, 'POST', chat + '/send', {'text': 'from the read-only site'})
        other = other['exchange']
        assert status == 200 and other['access'] == 'read_only' and other['id'] != exchange['id'], other
        wait(lambda: traces('owner')[-1] == '[wsx exchange %d, round 1, read-only]\nfrom the read-only site' % other['id'], 'read-only delivery')
        wait(lambda: messages(owner, sessions['owner'])['messages'][-1]['text'] == 'echo: from the read-only site', 'read-only turn')
        settle('owner')
        # Closed pages free their stream slot at once: more than the per-device cap can follow.
        for _ in range(6):
            page = Events(owner)
            page.expect('workspace')
            # Shut the socket down first: close() alone waits for the reader thread's blocked read.
            page.connection.sock.shutdown(socket.SHUT_RDWR)
            assert page.closed.wait(3)
            time.sleep(0.15)
        ok('files sandbox without .git, terminal typing into the agent pane, access-bound continuation, stream slots released')


        # --- Live terminal: one WebSocket per session, bridged to the wsxd raw stream.
        terminal_trace = work / 'terminal.trace'
        (work / 'terminal.py').write_text(TERMINAL)
        created = call('session_create', {'worktree_id': worktrees['project'], 'label': 'terminal',
            'command': [sys.executable, str(work / 'terminal.py'), str(terminal_trace)], 'rows': 12, 'cols': 60})
        assert created['type'] == 'created', created
        term = created['data']['id']
        term_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == term)
        _, viewer_cookie = pair(viewer, 'viewer')
        viewer.cookie = viewer_cookie.split(';')[0]

        def typed():
            return terminal_trace.read_bytes() if terminal_trace.exists() else b''

        # The upgrade is refused before any daemon stream exists.
        for label, refused, status, code in (
                ('no cookie', Terminal(owner, term, cookie=None), 401, 'unpaired'),
                ('forged cookie', Terminal(owner, term, cookie=owner.cookie[:-1] + ('0' if owner.cookie[-1] != '0' else '1')), 401, 'unpaired'),
                ("another site's cookie", Terminal(owner, term, cookie=viewer.cookie), 401, 'unpaired'),
                ('wrong Origin', Terminal(owner, term, origin='http://evil.example'), 403, 'wrong_origin'),
                ('no Origin', Terminal(owner, term, origin=None), 403, 'wrong_origin'),
                ('site without the terminal panel', Terminal(family, sessions['family']), 403, 'capability_denied'),
                ('takeover without terminal_input', Terminal(viewer, term, '&takeover=true'), 403, 'capability_denied'),
                ("another project's session", Terminal(owner, sessions['family']), 404, 'not_found'),
                ('client that is not digits', Terminal(owner, term, client='abc'), 400, 'invalid_client')):
            assert (refused.status, refused.error) == (status, code), (label, refused.status, refused.error)
        assert call('terminal_acquire', {'pane_id': term_pane, 'client_id': 5, 'takeover': False})['type'] == 'ack'
        assert call('terminal_release', {'pane_id': term_pane, 'client_id': 5})['type'] == 'ack'
        ok('terminal upgrade refused: no or forged cookie, wrong or missing Origin, missing panel or capability, foreign session')

        # Replay first, then bytes both ways. Each key of the key row arrives as its byte.
        page = Terminal(owner, term, '&rows=20&cols=70')
        assert page.status == 101
        replay = page.control('replay')
        assert (replay['rows'], replay['cols']) == (20, 70), replay
        page.until(lambda: b'term-ready' in page.output and b'[size 70x20]' in page.output, 'replay and first size')
        key_row = [b'\x1b', b'\t', b'\x03', b'\x16', b'\x01', b'd', b'\x1a', b'\x12', b'\x0c', b'\x7f', b'\n', b'\r']
        for key in key_row:
            page.send(key)
            page.until(lambda: page.output.endswith(b'<' + key.hex().encode() + b'>'), 'echo of %r' % key)
        assert typed() == b''.join(key_row), typed()
        page.send('한글'.encode())
        page.until(lambda: page.output.endswith(b'<' + '한글'.encode().hex().encode() + b'>'), 'UTF-8 echo')
        page.send({'type': 'resize', 'rows': 9, 'cols': 44})
        page.until(lambda: b'[size 44x9]' in page.output, 'resize reaches the PTY')
        ok('terminal: replay then bytes both ways, every key-row byte and UTF-8 reach the PTY, resize reaches the PTY')

        # A watch-only device sees the pane at its own size and cannot type or resize.
        # It attaches only when no controller holds the pane.
        watch = Terminal(viewer, term, '&rows=30&cols=90')
        assert watch.control('busy') and not watch.frame()
        # A wsx window takes the terminal: the page is told, and it does not take it back by itself.
        with socket.socket(socket.AF_UNIX) as window:
            window.settimeout(5)
            window.connect(str(path))
            lines = window.makefile('rb')
            window.sendall(wsxd_client.HELLO)
            lines.readline()
            window.sendall(json.dumps({'method': 'terminal_subscribe', 'params': {
                'pane_id': term_pane, 'client_id': 77, 'takeover': True, 'rows': 12, 'cols': 60}}).encode() + b'\n')
            assert json.loads(lines.readline())['type'] == 'ack'
            # Read frames as a window does; an unread stream would stall the pane.
            window_messages = []

            def read_window():
                try:
                    for line in lines:
                        window_messages.append(json.loads(line))
                except (OSError, ValueError):
                    pass
            threading.Thread(target=read_window, daemon=True).start()
            assert page.control('control_lost') and not page.frame()
            busy = Terminal(owner, term, '&rows=20&cols=70', client='8')
            assert busy.control('busy') and not busy.frame()
            assert typed() == b''.join(key_row) + '한글'.encode()
            taken = Terminal(owner, term, '&rows=20&cols=70&takeover=true', client='8')
            taken.control('replay')
            displaced = wait(lambda: [m for m in window_messages if m['type'] == 'error'], 'the window is displaced')
            assert displaced[0]['data']['code'] == 'lease_transferred', displaced
        taken.send(b'x')
        taken.until(lambda: taken.output.endswith(b'<78>'), 'input after taking control')
        # A lost connection: the same page reconnects without a takeover and gets a new replay.
        taken.drop()
        again = Terminal(owner, term, '&rows=20&cols=70', client='8')
        again.control('replay')
        again.until(lambda: b'<78>' in again.output, 'replay shows the earlier screen')
        again.send(b'y')
        again.until(lambda: again.output.endswith(b'<79>'), 'input after reconnect')
        again.send({'type': 'shutdown'})
        assert again.control('error')['code'] == 'invalid_message' and not again.frame()
        # Closing the socket releases the lease.
        lease_free(term)
        ok('terminal control: lost to a wsx window, busy without takeover, taken back, same-page reconnect with a new replay, lease released on close')

        # Pages that went away without a close, as a phone that sleeps does: they read nothing
        # and answer no ping. Each held the pane's lease, and one whose pane kept writing held a
        # send, and its device slot, until TCP gave up (docs/web-client-terminal.md "Liveness").
        gone = Terminal(owner, term, client='11')
        gone.control('replay')
        started = time.monotonic()
        pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == term)
        wait(lambda: call('terminal_acquire', {'pane_id': pane, 'client_id': 6, 'takeover': False})['type'] == 'ack',
             'a page that went away keeps the lease', timeout=30)
        assert call('terminal_release', {'pane_id': pane, 'client_id': 6})['type'] == 'ack'
        assert time.monotonic() - started > 10, 'a silent page lost the terminal before its liveness bound'
        flooded = []
        for client in range(20, 24):
            page = Terminal(owner, term, '&takeover=true', client=str(client))
            page.control('replay')
            # Echoed twice over until every buffer on the way is full and the bridge's send to
            # this page waits; the page's own send then stops too.
            page.socket.settimeout(1)
            try:
                for _ in range(200):
                    page.send(b'f' * 60000)
            except TimeoutError:
                pass
            flooded.append(page)
        fifth = wait(lambda: (lambda page: page if page.status == 101 else page.socket.close())(
            Terminal(owner, term, '&takeover=true', client='24')), 'a device whose pages went away gets a terminal again', timeout=40)
        fifth.control('replay')
        fifth.drop()
        for page in flooded + [gone]:
            page.drop()
        lease_free(term)
        ok('terminal liveness: a page that answers nothing frees the lease, and pages stuck behind a full socket free their slots')

        size_before = len(typed())
        watch = Terminal(viewer, term, '&rows=30&cols=90')
        replay = watch.control('replay')
        assert (replay['rows'], replay['cols']) == (20, 70), 'a watch-only device must not resize the pane'
        watch.send({'type': 'resize', 'rows': 5, 'cols': 5})
        assert watch.control('error')['code'] == 'invalid_message' and not watch.frame()
        watch = Terminal(viewer, term)
        watch.control('replay')
        watch.send(b'z')
        while watch.frame():  # the socket timeout fails this if the server keeps the stream open
            pass
        time.sleep(0.3)
        assert len(typed()) == size_before, 'a watch-only device typed into the terminal'
        # The program sets the clipboard of a device that may type, never a watching one's.
        page = Terminal(owner, term, client='9')
        page.control('replay')
        page.send(b'CLIPS')
        page.until(lambda: b'[tick' in page.output and any(c['type'] == 'clipboard' and c['text'] == 'copied' for c in page.controls), 'clipboard to a typing device')
        last = max(int(n) for n in re.findall(rb'\[tick(\d+)\]', page.output))
        page.drop()
        lease_free(term)
        watch = Terminal(viewer, term)
        watch.control('replay')
        # ^ A tick after the page left is live output, with its clipboard write before it.
        watch.until(lambda: b'[tick%d]' % (last + 3) in watch.output, 'live ticks reach the watcher')
        assert not any(c['type'] == 'clipboard' for c in watch.controls), watch.controls
        watch.drop()
        lease_free(term)
        # This site is granted every project, so it also reaches the other project's session.
        elsewhere = Terminal(viewer, sessions['family'])
        assert elsewhere.status == 101, (elsewhere.status, elsewhere.error)
        elsewhere.drop()
        lease_free(sessions['family'])
        assert viewer.request('GET', '/api/session')[1]['site']['terminal'] == {'ligatures': False}  # the site file's choice
        # Terminal fonts: versioned names, cached for good, and outside the offline shell.
        status, headers, font = owner.request('GET', '/assets/JetBrainsMono-2.304-Regular.woff2', raw=True)
        assert status == 200 and font[:4] == b'wOF2' and headers['content-type'] == 'font/woff2' and 'immutable' in headers['cache-control']
        status, headers, font = owner.request('GET', '/assets/SymbolsNerdFontMono-3.4.0.woff2', raw=True)
        assert status == 200 and font[:4] == b'wOF2' and len(font) < 1500000
        assert owner.request('GET', '/assets/SymbolsNerdFontMono-3.4.0.ttf', raw=True)[2][:4] == b'\x00\x01\x00\x00'
        assert b'woff2' not in owner.request('GET', '/sw.js')[1] and b'terminal-fonts.css' in owner.request('GET', '/sw.js')[1]
        ok('watch-only terminal: host size, no resize, no input, no clipboard; projects = "all" reaches every project; terminal fonts served')

        # --- A profile site (README "Profiles"): exactly profile work's projects, also one that
        # joins it later without a restart; its agents start under the profile's config root, and
        # the skills it lists come from that root.
        work_site = sites['work']
        work_site.cookie = pair(work_site, 'work')[1].split(';')[0]

        def work_projects():
            listed = Events(work_site)
            try:
                return sorted(p['name'] for p in listed.expect('workspace')['projects'])
            finally:
                listed.connection.close()
        assert work_projects() == ['project2'], work_projects()
        status, mentions = work_site.request('GET', '/api/tasks/%d/mentions' % projects['project2'])
        assert status == 200 and [s['name'] for s in mentions['skills']] == ['profile-skill'], mentions
        assert work_site.request('GET', '/api/tasks/%d/mentions' % projects['project'])[0] == 404
        status, started = work_site.request('POST', '/api/sessions', {'project': projects['project2']})
        assert status == 200, started
        seen = wait(lambda: (work / 'profile-agent.txt').exists() and (work / 'profile-agent.txt').read_text(), 'the profile agent starts')
        assert seen == str(profile_root), seen
        session = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == started['session_id'])
        assert call('session_close', {'session_id': session['id'], 'expected_revision': session['revision']})['type'] == 'ack'
        with registry.open('a') as file:
            file.write('[[projects]]\nname = "project3"\npath = %s\nprofile = "work"\n' % json.dumps(str((work / 'project3').resolve())))
        # ^ A request sees it at once; the page's project list on the next poll (2 s at most).
        assert work_site.request('GET', '/api/tasks/%d/mentions' % projects['project3'])[0] == 200
        wait(lambda: work_projects() == ['project2', 'project3'], 'project3 joins the profile site')
        ok('profile site: exactly its profile\'s projects, a project that joins later, agents and skills under its config root')
        session = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == term)
        assert call('session_close', {'session_id': term, 'expected_revision': session['revision']})['type'] == 'ack'

        # --- Site B: same binary, fewer bits. Every missing bit is a server refusal.
        status, session = family.request('GET', '/api/session')
        assert session['site']['panels'] == ['chat'] and 'agent_write' not in session['site']['capabilities']
        family_events = Events(family, shape='stripped')
        streams.append(family_events)
        assert [s['id'] for s in family_events.expect('workspace')['sessions']] == [sessions['family']]
        for method, target, body in (('GET', '/api/files?project=1&path=', None), ('GET', '/api/file?project=1&path=notes.txt', None),
                                     ('PUT', '/api/file', write),
                                     ('GET', '/api/push', None),
                                     ('POST', '/api/sessions', {'project': projects['project2'], 'label': 'new'}),
                                     ('DELETE', '/api/sessions/%d' % sessions['family'], None)):
            status, body = family.request(method, target, body)
            assert status == 403 and body['error']['code'] == 'capability_denied', (target, status, body)
        # The other project's session does not exist for this site, in any route.
        for target in ('/api/chat/%d/timeline', '/api/chat/%d/send', '/api/chat/%d/cancel'):
            method = 'GET' if target.endswith('timeline') else 'POST'
            assert family.request(method, target % sessions['owner'], {'text': 'x'} if method == 'POST' else None)[0] == 404
        before = len(traces('owner'))
        status, sent = family.request('POST', '/api/chat/%d/send' % sessions['family'], {'text': 'build me a page'})
        sent = sent['exchange']
        assert status == 200 and sent['access'] == 'read_only', sent
        assert wait(lambda: traces('family'), 'family delivery')[0].startswith('[wsx exchange %d, round 1, read-only]\n' % sent['id'])
        assert len(traces('owner')) == before
        wait(lambda: len(messages(family, sessions['family'])['messages']) == 2, 'family history')
        assert family.request('GET', '/' + uploaded['url'])[0] == 404  # media stores are per site
        ok('site B: capability denial for files, terminal, push, session start; other project invisible')

        # --- Answering a blocked agent from the chat: the dialog the agent drew, and keys.
        settle('owner')
        def question(site):
            return steady(site, 'GET', chat + '/timeline')[1]['question']
        def answer(site, stamp, key, **extra):
            return steady(site, 'POST', chat + '/answer', dict({'stamp': stamp, 'key': key}, **extra))
        def answers():
            file = work / 'owner.answers'
            return [bytes.fromhex(json.loads(line)['keys']) for line in file.read_text().splitlines()] if file.exists() else []
        def last_text():
            return [item.get('text') for item in steady(root, 'GET', chat + '/timeline')[1]['items']][-1]
        def ask(text):
            settle('owner')
            assert steady(root, 'POST', chat + '/send', {'text': text})[0] == 200
            return wait(lambda: question(root), 'question for ' + text)
        root_events = Events(root)
        streams.append(root_events)
        asked = ask('ask me for permission')
        assert asked['request'] == {'name': 'Bash', 'title': 'Remove the build folder'}, asked
        assert [option['key'] for option in asked['options']] == ['1', '2', '3'], asked
        assert asked['options'][0]['label'] == 'Yes' and asked['options'][2]['label'] == 'No, and tell Claude what to do differently (esc)', asked
        assert any('rm -rf build' in row for row in asked['screen']) and asked['screen'][0].startswith('╭') and len(asked['screen']) <= 24, asked
        assert re.fullmatch('[0-9a-f]{32}', asked['stamp']) and not re.search(r'[\x00-\x1f\x7f]', ''.join(asked['screen'])), asked
        # A site without chat_answer learns nothing of the screen and cannot answer.
        assert question(owner) is None
        refused = answer(owner, asked['stamp'], '1')
        assert refused[0] == 403 and refused[1]['error']['code'] == 'capability_denied', refused
        assert steady(root, 'POST', '/api/chat/%d/answer' % sessions['family'], {'stamp': asked['stamp'], 'key': '1'})[0] == 404  # another project's session
        for bad in ({'key': '0'}, {'key': 'text'}, {'key': '1', 'text': 'x'}, {'key': 'text', 'text': ' \x07 '}, {'key': 'text', 'text': 'x' * 2001}, {'key': 'F1'}):
            refused = steady(root, 'POST', chat + '/answer', dict({'stamp': asked['stamp']}, **bad))
            assert refused[0] == 400 and refused[1]['error']['code'] == 'invalid_answer', (bad, refused)
        # The answer is bound to the screen it was read from.
        refused = answer(root, '0' * 32, '1')
        assert refused[0] == 409 and refused[1]['error']['code'] == 'question_changed', refused
        owner_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == sessions['owner'])
        assert call('terminal_acquire', {'pane_id': owner_pane, 'client_id': 6, 'takeover': False})['type'] == 'ack'
        assert answers() == []
        # A dialog that changes on the screen is a changed question: the page is told, the old stamp is stale.
        del root_events.seen[:]
        assert call('terminal_input', {'pane_id': owner_pane, 'client_id': 6, 'bytes': list(b'\x1b[B')})['type'] == 'ack'
        assert call('terminal_release', {'pane_id': owner_pane, 'client_id': 6})['type'] == 'ack'
        root_events.expect('history', lambda data: data['session_id'] == sessions['owner'], timeout=5)
        moved = question(root)
        assert moved['stamp'] != asked['stamp'] and any(row.startswith(' ❯ 2.') for row in moved['screen']), moved
        refused = answer(root, asked['stamp'], '1')
        assert refused[0] == 409 and refused[1]['error']['code'] == 'question_changed', refused
        # A key from the page moves the cursor back, past a wsx window that holds the pane;
        # then the dialog is approved by its number.
        with wsx_window(owner_pane) as seen:
            assert answer(root, moved['stamp'], 'up') == (200, {'sent': True})
            told_control_moved(seen)
        back = wait(lambda: (lambda now: now and now['stamp'] == asked['stamp'] and now)(question(root)), 'cursor back on the first choice')
        assert answer(root, back['stamp'], '1') == (200, {'sent': True})
        wait(lambda: last_text() == 'You chose: Yes', 'approved by number')
        assert answers()[0] == b'\x1b[B' and answers()[1] in (b'\x1b[A', b'\x1bOA') and answers()[2:] == [b'1'], answers()
        wait(lambda: question(root) is None, 'the question ends with the blocked state')
        refused = answer(root, back['stamp'], '1')
        assert refused[0] == 409 and refused[1]['error']['code'] == 'not_blocked', refused
        # Denied with Escape.
        asked = ask('ask me for permission')
        assert answer(root, asked['stamp'], 'escape')[0] == 200
        wait(lambda: last_text() == 'You chose: cancelled', 'denied with Escape')
        assert answers()[-1] == b'\x1b', answers()
        # A question tool: its question is the heading, and only the dialog's own list is offered.
        asked = ask('ask me which way')
        assert asked['request'] == {'name': 'AskUserQuestion', 'title': 'Which option should I take for the login redirect?'}, asked
        assert [(option['key'], option['label']) for option in asked['options']] == [
            ('1', 'Keep the session cookie'), ('2', 'Use a bearer token'), ('3', 'Ask the server on each request'), ('4', 'Type something else')], asked
        assert answer(root, asked['stamp'], '3')[0] == 200
        wait(lambda: last_text() == 'You chose: Ask the server on each request', 'answered by number')
        # Typed text and Enter arrive as one write, without control characters.
        asked = ask('ask me which way')
        del events.seen[:]
        assert answer(root, asked['stamp'], 'text', text='neither,\x1b wait')[0] == 200
        wait(lambda: last_text() == 'You chose: typed: neither, wait', 'typed answer')
        assert answers()[-1] == b'neither, wait\r', answers()
        # The owner's server must have seen this turn end before a later step subscribes to
        # push, or the "finished" of this turn is the first notification there.
        events.expect('workspace', lambda data: any(
            item['id'] == sessions['owner'] and item['agent']['state'] == 'done' for item in data['sessions']))
        ok('answer: question with screen, choices and waiting tool; refused without the capability, for a stale stamp, an invalid key and an agent that is not blocked; a held terminal is taken; approve, deny, number and text reach the PTY')

        # --- Optional real browser, on both sites, before any device is revoked.
        if args.browser:
            args.captures.mkdir(parents=True, exist_ok=True)
            # The live terminal, on a shell session of its own.
            tools = work / 'tools'
            tools.mkdir()
            for name, source in TOOLS.items():
                (tools / name).write_text('#!%s\n%s' % (sys.executable, source.lstrip()))
                (tools / name).chmod(0o700)
            created = call('session_create', {'worktree_id': worktrees['project'], 'label': 'shell',
                'command': ['/bin/sh', '-c', "cd '%s' && PS1='$ ' exec /bin/sh" % tools], 'rows': 24, 'cols': 80})
            assert created['type'] == 'created', created
            shell = created['data']['id']
            shell_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == shell)
            code = cli('web', 'pair', '--site', files['owner']).stdout.splitlines()[0].rsplit(' ', 1)[1]
            args.captures.mkdir(parents=True, exist_ok=True)
            result = check(work, [node, str(ROOT / 'scripts/web-terminal-check.mjs'), owner.origin + owner.base + '/',
                                  code, str(shell), str(args.captures), str(path), str(shell_pane), str(tools / 'keys.trace'),
                                  str(owner_server.pid)],
                           timeout=240)
            assert result.returncode == 0, result.stdout + result.stderr
            ok('real browser, ' + result.stdout.strip().splitlines()[-1])
            session = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == shell)
            assert call('session_close', {'session_id': shell, 'expected_revision': session['revision']})['type'] == 'ack'

            # Every screen and state of the page, on a project of its own.
            design = (work / 'design').resolve()
            (design / 'README.md').write_text('# design\n\nA fixture project for the browser check.\n')
            (design / 'src').mkdir()
            (design / 'src/main.rs').write_text('fn main() {\n    println!("hello");\n}\n')
            m2 = sites['m2']
            _, m2_cookie = pair(m2, 'm2')
            m2.cookie = m2_cookie.split(';')[0]
            shown = {}
            for role, label in (('blocked', 'Fix the login redirect loop on mobile Safari when third-party cookies are blocked'),
                                ('permission', 'clean build'),
                                ('error', 'api-tests'), ('exited', 'crashed-import'), ('working', 'release notes'),
                                ('done', 'docs cleanup'), ('long', 'release checklist'), ('idle', 'refactor'),
                                ('markdown', 'release plan'), ('large', 'full audit'), ('diagrams', 'architecture'),
                                ('compacted', 'login fix')):
                created = call('session_create', {'worktree_id': worktrees['design-login' if role == 'idle' else 'design'], 'label': label,
                    'command': [sys.executable, str(actor), str(work / (role + '.jsonl')), str(work / (role + '.trace')),
                                str(design), str(chart), str(outside)], 'rows': 12, 'cols': 100})
                assert created['type'] == 'created', created
                shown[role] = created['data']['id']
            created = call('session_create', {'worktree_id': worktrees['design'], 'label': 'shell', 'command': ['/bin/sh'], 'rows': 12, 'cols': 100})
            shown['shell'] = created['data']['id']

            def state_of(role):
                snapshot = call('snapshot')['data']
                pane = next(s['focused_pane'] for s in snapshot['sessions'] if s['id'] == shown[role])
                pane = next(p for p in snapshot['panes'] if p['id'] == pane)
                return 'exited' if pane['exited'] else pane['agent'] and pane['agent']['state']
            def prompt_shown(role):
                pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == shown[role])
                frames = call('view', {'pane_ids': [pane]})['data']['frames']
                return bool(frames) and '❯' in ''.join(cell[0] for cell in frames[0]['cells'])
            (work / 'markdown.md').write_text(MARKDOWN_SAMPLE)
            (work / 'diagrams.md').write_text(DIAGRAM_SAMPLE)
            (work / 'large.md').write_text(('Paragraph with **bold**, `code` and a [link](https://example.com). ' * 20
                                           + '\n\n- item one\n- item two\n\n```rust\nfn main() { println!("x"); }\n```\n\n') * 145)
            for role in shown:
                if role == 'shell':
                    continue
                # The agent reports idle just before it paints its prompt; wsxd types only into a visible editor.
                wait(lambda: state_of(role) == 'idle' and prompt_shown(role), role + ' agent ready', timeout=20)
            for role, text in (('blocked', 'ask me which option to take, slowly'), ('permission', 'ask me for permission'),
                               ('error', 'fail now'), ('exited', 'quit now'),
                               ('working', 'use tool'), ('done', 'tidy the docs'), ('long', 'long talk'),
                               ('markdown', 'say markdown.md'), ('large', 'say large.md'), ('diagrams', 'say diagrams.md'),
                               ('compacted', 'compact now')):
                status, sent = steady(m2, 'POST', '/api/chat/%d/send' % shown[role], {'text': text})
                assert status == 200, (role, sent)
            for role, expected in (('blocked', 'blocked'), ('permission', 'blocked'), ('error', 'error'), ('exited', 'exited'), ('working', 'working'),
                                   ('done', 'done'), ('long', 'done'), ('markdown', 'done'), ('large', 'done'),
                                   ('diagrams', 'done'), ('compacted', 'done')):
                wait(lambda: state_of(role) == expected, role + ' state', timeout=20)
            # A session continued after its compaction: the summary, then a short exchange.
            assert steady(m2, 'POST', '/api/chat/%d/send' % shown['compacted'], {'text': 'What is left to do?'})[0] == 200
            wait(lambda: steady(m2, 'GET', '/api/chat/%d/timeline' % shown['compacted'])[1]['items'][-1].get('text') == 'echo: What is left to do?', 'compacted turn')
            blocked_items = steady(m2, 'GET', '/api/chat/%d/timeline' % shown['blocked'])[1]
            assert [item.get('text') for item in blocked_items['items']] == ['ask me which option to take, slowly', None], blocked_items
            assert len(blocked_items['question']['options']) == 4, blocked_items
            queued = steady(m2, 'POST', '/api/chat/%d/send' % shown['working'], {'text': 'Also mention the new terminal in the notes'})
            assert 'queued' in queued[1], queued
            # The done agent: a message waits while it works, and a person types a line in its
            # editor meanwhile. This fixture cannot stash the line, so the waiting message and
            # a later one from the page are not delivered, and the page says why.
            done_chat = '/api/chat/%d' % shown['done']
            assert steady(m2, 'POST', done_chat + '/send', {'text': 'use tool'})[0] == 200
            wait(lambda: state_of('done') == 'working', 'done agent working')
            assert 'queued' in steady(m2, 'POST', done_chat + '/send', {'text': 'Please also update the changelog'})[1]
            done_pane = next(s['focused_pane'] for s in call('snapshot')['data']['sessions'] if s['id'] == shown['done'])
            assert call('terminal_acquire', {'pane_id': done_pane, 'client_id': 6, 'takeover': False})['type'] == 'ack'
            assert call('terminal_input', {'pane_id': done_pane, 'client_id': 6, 'bytes': list(b'unsent line')})['type'] == 'ack'
            assert call('terminal_release', {'pane_id': done_pane, 'client_id': 6})['type'] == 'ack'
            for gate in ('result', 'answer'):
                (work / ('done.' + gate)).write_text('')
            wait(lambda: (lambda queue: queue and queue[0]['failed'])(steady(m2, 'GET', done_chat + '/timeline')[1]['queue']),
                 'the waiting message of the done agent failed', timeout=20)
            wait(lambda: state_of('done') == 'done', 'done agent finished again')
            bare = sites['bare']
            _, bare_cookie = pair(bare, 'bare')
            bare.cookie = bare_cookie.split(';')[0]
            bare_events = Events(bare)
            streams.append(bare_events)
            empty = bare_events.expect('workspace')
            assert [p['name'] for p in empty['projects']] == ['bare'] and empty['sessions'] == [], empty
            audit = Path.home() / '.claude/skills/ui-view-standards/scripts/slop-audit.js'
            page_captures = args.captures
            result = check(work, [node, str(ROOT / 'scripts/web-browser-check.mjs'), 'tour', m2.origin + m2.base + '/',
                                  cli('web', 'pair', '--site', files['m2']).stdout.splitlines()[0].rsplit(' ', 1)[1],
                                  bare.origin + bare.base + '/',
                                  cli('web', 'pair', '--site', files['bare']).stdout.splitlines()[0].rsplit(' ', 1)[1],
                                  json.dumps(shown), str(page_captures), str(audit) if audit.exists() else ''],
                           timeout=440)
            assert result.returncode == 0, result.stdout + result.stderr
            # The measurements, for the record (docs/web-client.md "Live updates").
            for line in result.stdout.splitlines():
                if 'processor slowed' in line or line.startswith(('photo of', 'empty phone composer', '/ list', 'mobile audit')):
                    print(line, file=sys.stderr)
            ok('real browser, ' + result.stdout.strip().splitlines()[-1])
            wait(lambda: steady(m2, 'GET', '/api/chat/%d/timeline' % shown['idle'])[1]['items'][-1].get('text') == 'echo: line one', 'browser turn')
            # The command picked in the browser reached the agent as typed.
            assert traces('long')[-1] == '/compact', traces('long')

            # A paired page reloads, with its service worker, before and after a server restart.
            # The page asks for the restart with a file and waits for the answer file.
            signals = work / 'restart'
            signals.mkdir()
            restart = start(work, [node, str(ROOT / 'scripts/web-browser-check.mjs'), 'restart', m2.origin + m2.base + '/',
                                      cli('web', 'pair', '--site', files['m2']).stdout.splitlines()[0].rsplit(' ', 1)[1],
                                      str(page_captures), str(signals), str(audit) if audit.exists() else ''],
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                     env=dict(os.environ, WSX_TEST_WORK=str(work)))
            deadline = time.monotonic() + 120
            while restart.poll() is None and time.monotonic() < deadline:
                if (signals / 'request').exists() and not (signals / 'done').exists():
                    old = servers['m2']
                    old.terminate()
                    started = time.monotonic()
                    try:
                        old.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        old.kill()
                        old.wait(timeout=3)
                        raise AssertionError('wsx web serve did not stop within 10 s of SIGTERM while a page was open')
                    stopped = time.monotonic() - started
                    # Open streams get a two-second grace period, then the server ends.
                    assert stopped < 4, 'wsx web serve took %.1f s to stop' % stopped
                    # A reply the open chat misses while the server is away: the page reads it again.
                    session_id = int((signals / 'request').read_text().split()[1])
                    history = work / (next(role for role, id in shown.items() if id == session_id) + '.jsonl')
                    last = json.loads(history.read_text().splitlines()[-1])
                    with history.open('a') as out:
                        out.write(json.dumps({'type': 'assistant', 'uuid': 'missed-reply', 'parentUuid': last['uuid'],
                            'sessionId': last['sessionId'], 'timestamp': time.strftime('%Y-%m-%dT%H:%M:%S.000Z', time.gmtime()),
                            'message': {'role': 'assistant', 'model': 'claude-opus-5-5',
                                        'content': [{'type': 'text', 'text': 'written while the server was away'}]}}) + '\n')
                    # Another build of the same site: a new name changes the page files.
                    files['m2'].write_text(files['m2'].read_text().replace('name = "wsx"', 'name = "wsx next"', 1))
                    serve('m2')
                    wait(lambda: up(m2), 'm2 server restarted')
                    (signals / 'done').write_text('%.2f\n' % stopped)
                time.sleep(0.1)
            ended = finish(restart, timeout=10)
            output = ended.stdout + (ended.stderr or '')
            assert ended.returncode == 0, output
            ok('real browser, ' + output.strip().splitlines()[-1] + ' (server stopped %s s after SIGTERM)' % (signals / 'done').read_text().strip())

        # --- Web Push with explicit device consent, to the local receiver.
        status, info = owner.request('GET', '/api/push')
        off = {'manager': False, 'agents': False, 'digest': None}
        assert status == 200 and info['choices'] == off and info['muted'] == [] and len(info['public_key']) == 87, info
        keys = {'p256dh': receiver['p256dh'], 'auth': receiver['auth']}
        agents_only = {'manager': False, 'agents': True, 'digest': None}
        for endpoint in ('https://evil.example/push', 'http://127.0.0.1:1/push', push_origin + '@evil.example/x'):
            refused = owner.request('POST', '/api/push', {'subscription': {'endpoint': endpoint, 'keys': keys}, 'choices': agents_only})
            assert refused[0] == 400, endpoint
        assert owner.request('POST', '/api/push', {'subscription': {'endpoint': push_origin + '/push/1', 'keys': dict(keys, auth='AAAA')}, 'choices': agents_only})[0] == 400
        subscription = {'endpoint': push_origin + '/push/device-1', 'keys': keys, 'expirationTime': None}
        # "Manager needs me" needs task_view; a digest time must be a minute of a day.
        assert owner.request('POST', '/api/push', {'subscription': subscription, 'choices': dict(agents_only, manager=True)})[0] == 403
        assert owner.request('POST', '/api/push', {'subscription': subscription, 'choices': dict(agents_only, digest={'minute': 1440, 'utc_offset_minutes': 0})})[0] == 400
        assert pushes() == []
        # The digest far off for now; it is moved to the next minute below.
        later = {'minute': (time.gmtime().tm_hour * 60 + time.gmtime().tm_min + 180) % 1440, 'utc_offset_minutes': 0}
        status, body = owner.request('POST', '/api/push', {'subscription': subscription, 'choices': dict(agents_only, digest=later)})
        assert status == 200 and body['choices'] == dict(agents_only, digest=later), body
        stored = (work / 'state/wsx/web/owner/push.json')
        assert stored.stat().st_mode & 0o777 == 0o600
        settle('owner')
        # An agent that finished never pushes; it waits for the digest.
        owner.request('POST', chat + '/send', {'text': 'notify me'})
        wait(lambda: traces('owner')[-1].splitlines()[-1] == 'notify me', 'finished turn')
        settle('owner')
        time.sleep(4.5)
        assert pushes() == [], pushes()
        # A second agent of the project, for the push that names every session that needs the owner.
        # ^ In another granted project: wsxd runs one writer exchange per worktree.
        created = call('session_create', {'worktree_id': worktrees['project3'], 'label': 'second chat',
            'command': [sys.executable, str(actor), str(work / 'second.jsonl'), str(work / 'second.trace'),
                        str(work / 'project3'), str(chart), str(outside)], 'rows': 12, 'cols': 100})
        assert created['type'] == 'created', created
        sessions['second'] = created['data']['id']
        settle('second')
        # Blocked: the body is the question the agent asks; the badge counts this session.
        owner.request('POST', chat + '/send', {'text': 'ask me something'})
        blocked = wait(lambda: len(pushes()) == 1 and pushes()[0], 'blocked push')
        assert blocked.get('error') is None, blocked
        assert blocked['vapid']['ok'] and blocked['vapid']['alg'] == 'ES256' and blocked['vapid']['key'] == info['public_key']
        assert blocked['vapid']['claims']['sub'] == 'mailto:owner@example.invalid' and blocked['encoding'] == 'aes128gcm'
        assert blocked['path'] == '/push/device-1'
        question = 'Needs your input: Which option should I take for the login redirect?'
        assert blocked['payload'] == {'kind': 'agents', 'title': 'project › main: owner chat', 'body': question,
                                      'session_id': sessions['owner'], 'tag': 'wsx-owner-agents', 'badge': 1}, blocked['payload']
        # The second one blocks: one push in place of the first that names both, and opens the inbox.
        steady(owner, 'POST', '/api/chat/%d/send' % sessions['second'], {'text': 'ask me something'})
        both = wait(lambda: len(pushes()) == 2 and pushes()[1], 'second blocked push')['payload']
        assert both == {'kind': 'agents', 'title': '2 agents need input: owner chat, second chat',
                        'body': 'owner chat: %s\nsecond chat: %s' % (question, question), 'url': '#/',
                        'tag': 'wsx-owner-agents', 'badge': 2}, both
        # Mute the second session on this device: it leaves the push and the digest.
        assert owner.request('POST', '/api/push/mute', {'session': 999999, 'muted': True})[0] == 404
        status, body = owner.request('POST', '/api/push/mute', {'session': sessions['second'], 'muted': True})
        assert status == 200 and body['muted'] == [sessions['second']], body
        assert json.loads(stored.read_text())[0]['muted'] == [sessions['second']]
        # Blocked: the web chat refuses to type into a provider dialog.
        events.expect('workspace', lambda data: data['sessions'][0]['agent']['state'] == 'blocked')
        refused = owner.request('POST', chat + '/send', {'text': 'yes'})
        assert refused[0] == 409 and refused[1]['error']['code'] == 'agent_blocked'
        interrupt = Terminal(owner, sessions['owner'])
        interrupt.control('replay')
        interrupt.send(b'\x03')
        events.expect('workspace', lambda data: data['sessions'][0]['agent']['state'] == 'idle')
        interrupt.drop()
        lease_free(sessions['owner'])
        # A turn a person interrupted ends its exchange: the next message is not held back.
        wait(lambda: steady(owner, 'GET', chat + '/timeline')[1]['exchange']['state'] == 'interrupted', 'interrupted exchange')
        # An error pushes; the muted session, still blocked, is not counted.
        settle('owner')
        owner.request('POST', chat + '/send', {'text': 'fail now'})
        failed = wait(lambda: len(pushes()) == 3 and pushes()[2], 'error push')['payload']
        assert failed['title'] == 'project › main: owner chat' and failed['session_id'] == sessions['owner'] and failed['tag'] == 'wsx-owner-agents', failed
        assert failed['body'] == 'Stopped with an error' or failed['body'].startswith('Stopped with an error: '), failed
        ok('agent push: endpoint allowlist, VAPID verified and payload decrypted by an independent receiver; blocked and error push, finished does not; one push names every session and replaces the one before; a muted session is left out')

        # The digest, moved to the next minute: it keeps the window and names the agent that
        # finished, without the muted one.
        cli('session', 'send-text', sessions['owner'], 'notify me again')
        wait(lambda: traces('owner')[-1].splitlines()[-1] == 'notify me again', 'finished turn after the error')
        settle('owner')
        now = time.gmtime()
        if now.tm_sec > 50:
            time.sleep(61 - now.tm_sec)
            now = time.gmtime()
        soon = {'minute': (now.tm_hour * 60 + now.tm_min + 1) % 1440, 'utc_offset_minutes': 0}
        assert owner.request('POST', '/api/push', {'subscription': subscription, 'choices': dict(agents_only, digest=soon)})[1]['muted'] == [sessions['second']]
        digest = wait(lambda: len(pushes()) == 4 and pushes()[3], 'daily digest', timeout=90)['payload']
        assert digest == {'kind': 'digest', 'title': 'Owner <wsx>: daily digest', 'body': '1 agent finished: owner chat',
                          'url': '#/', 'tag': 'wsx-owner-digest', 'badge': 1}, digest
        # The muted session goes with its session.
        second = next(item for item in call('snapshot')['data']['sessions'] if item['id'] == sessions['second'])
        assert call('session_close', {'session_id': sessions['second'], 'expected_revision': second['revision']})['type'] == 'ack'
        wait(lambda: owner.request('GET', '/api/push')[1]['muted'] == [], 'mute forgotten with its session')
        ok('agent digest: at the minute the device chose, an agent that finished; a muted session is left out and forgotten with its session')

        # --- Per-device rate limit, then revocation.
        codes = [owner.request('GET', '/api/session')[0] for _ in range(140)]
        assert codes[0] == 200 and 429 in codes and codes.index(429) >= 60, codes.index(429) if 429 in codes else codes[-1]
        assert family.request('GET', '/api/session')[0] == 200  # another device keeps its own budget
        time.sleep(1.2)
        device = listed[0]['id']
        assert 'revoked device ' + device in cli('web', 'devices', '--site', files['owner'], '--revoke', device).stdout
        cli('web', 'devices', '--site', files['owner'], '--revoke', device, expect=1)
        assert owner.request('GET', '/api/session')[0] == 401
        events.expect('revoked', timeout=25)
        assert events.closed.wait(5), 'revoked stream stayed open'
        assert owner.request('GET', '/' + uploaded['url'])[0] == 401
        assert owner.request('POST', chat + '/send', {'text': 'after revoke'})[0] == 401
        # A revoked device also loses its notifications.
        cli('session', 'send-text', sessions['owner'], 'after revoke')
        wait(lambda: traces('owner')[-1] == 'after revoke', 'post-revoke turn')
        time.sleep(1.0)
        assert len(pushes()) == 4, 'a revoked device was notified'
        wait(lambda: json.loads(stored.read_text()) == [], 'subscription of the revoked device removed')
        tries = [owner.request('POST', '/api/pair', {'code': 'BBBBB-BBBB%d' % index})[0] for index in range(6)]
        assert tries[-1] == 429 and set(tries[:3]) == {403}, tries
        ok('rate limit per device, revocation closes the stream and stops media, chat and push; pairing guesses limited')

        # --- session_start runs only the site file's command, in the granted project.
        status, body = root.request('POST', '/api/sessions', {'project': projects['project'], 'label': 'from\x07 the web'})
        assert status == 200, body
        snapshot = call('snapshot')['data']
        created = next(s for s in snapshot['sessions'] if s['id'] == body['session_id'])
        assert created['label'] == 'from the web' and created['worktree_id'] == worktrees['project']
        # A chosen worktree must belong to the chosen project, and the project to the grant.
        status, body = root.request('POST', '/api/sessions', {'project': projects['project3'], 'worktree': worktrees['project3']})
        assert status == 200, body
        created = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == body['session_id'])
        assert created['worktree_id'] == worktrees['project3'] and created['label'] == 'chat'
        for refused in ({'project': projects['project2']}, {'project': projects['project'], 'worktree': worktrees['project3']},
                        {'project': projects['project2'], 'worktree': worktrees['project2']}, {'project': 987654}):
            assert root.request('POST', '/api/sessions', refused)[0] == 404, refused
        assert root.request('POST', '/api/sessions', {'label': 'no project'})[0] == 422
        ok('session start: site command, granted project and worktree, cleaned label; ungranted project and foreign worktree refused')

        # --- session_start also removes: wsxd closes the session; a task's own session stays.
        removed = created['id']
        status, body = root.request('DELETE', '/api/sessions/%d' % removed)
        assert status == 200 and body == {'removed': removed}, (status, body)
        assert all(s['id'] != removed for s in call('snapshot')['data']['sessions'])
        assert root.request('DELETE', '/api/sessions/%d' % removed)[0] == 404
        assert root.request('DELETE', '/api/sessions/%d' % sessions['family'])[0] == 404, 'an ungranted project'
        for label in ('task-7', 'task-7-hand'):
            status, body = steady(root, 'POST', '/api/sessions', {'project': projects['project'], 'label': label})
            assert status == 200, body
            worker = body['session_id']
            status, body = root.request('DELETE', '/api/sessions/%d' % worker)
            assert status == 409 and body['error']['code'] == 'task_session', (label, status, body)
            kept = next(s for s in call('snapshot')['data']['sessions'] if s['id'] == worker)
            assert call('session_close', {'session_id': worker, 'expected_revision': kept['revision']})['type'] == 'ack'
        ok('session remove: closed in wsxd, then gone; task-<n> and task-<n>-hand refused; an ungranted one does not exist')

        # --- The web server never starts wsxd; it reports the gap and recovers.
        assert call('shutdown')['type'] == 'ack'
        daemon.wait(timeout=5)
        family_events.expect('daemon', lambda data: data['available'] is False, timeout=10)
        status, body = family.request('GET', '/api/chat/%d/timeline' % sessions['family'])
        assert status == 503 and body['error']['code'] == 'daemon_unavailable'
        assert family.request('GET', '/api/session')[0] == 200
        time.sleep(1.5)
        assert not path.exists(), 'wsx web started a daemon'
        if args.browser:
            result = check(work, [node, str(ROOT / 'scripts/web-browser-check.mjs'), 'down', m2.origin + m2.base + '/',
                                  cli('web', 'pair', '--site', files['m2']).stdout.splitlines()[0].rsplit(' ', 1)[1],
                                  str(page_captures), str(audit) if audit.exists() else ''],
                           timeout=120)
            assert result.returncode == 0, result.stdout + result.stderr
            ok('real browser, ' + result.stdout.strip().splitlines()[-1])
        ok('daemon stop: reported on the stream, API says unavailable, no daemon started')
        success = True
    finally:
        for stream in streams:
            try:
                stream.connection.close()
            except Exception:
                pass
        # Every helper, a check that was still running too.
        for process in reversed(STARTED):
            stop(process)
        if path is not None and path.exists():
            assert path.lstat().st_uid == os.getuid(), 'private socket changed owner'
        for record in work.glob('*.pid'):
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                try:
                    os.kill(int(record.read_text()), 0)
                except ProcessLookupError:
                    break
                time.sleep(0.02)
            else:
                raise RuntimeError('private actor still alive; retained ' + str(work))
        if success:
            shutil.rmtree(work)
        else:
            print('diagnostics retained at ' + str(work), file=sys.stderr)
    print(json.dumps({'result': 'PASS', 'seconds': round(time.monotonic() - started, 2), 'model_calls': 0,
                      'slowest': slowest(), 'checks': checks}, indent=1))


# ^ Ready tasks land by hand here, so the fixture keeps tasks that wait at ready; test-task.py
# covers the default auto land.
# ^ `minFreeGb: 0`: tasks must not wait on this machine's free disk. `approve: true` keeps
# the manager's land an approval request for the journey.
LONG_NAME = 'notes-about-jumping-between-projects-from-the-keyboard-and-back.txt'
TASK_CONFIG = '''tasks:
  minFreeGb: 0
  land: { auto: false, approve: true }
  agent:
    model: fake-model
  checks:
    - "test ! -e broken.txt"
  manager: { model: fake-manager }
  deploy: { command: 'test -z "$WSX_FIXTURE_DEPLOY_FAIL" || { seq -f "deploy step %g" 60; exit 3; }' }
  preview:
    command: 'exec python3 -m http.server "$PORT" --bind 127.0.0.1'
    ports: 9150-9199
'''

# An agent session for Home's session words: report one state as Claude, then stay or end.
SESSION_AGENT = r'''
import json, os, socket, sys, time
with socket.socket(socket.AF_UNIX) as client:
    client.connect(os.environ['WSX_SOCKET']); stream = client.makefile('rb')
    client.sendall(b'{"method":"hello","params":{"protocol":@PROTOCOL@}}\n'); stream.readline()
    params = {'pane_id': int(os.environ['WSX_PANE_ID']), 'runtime_generation': os.environ['WSX_RUNTIME_GENERATION'],
              'provider': 'claude', 'state': sys.argv[1], 'capabilities': {'prompt': False, 'lifecycle': True}}
    client.sendall(json.dumps({'method': 'agent_report', 'params': params}).encode() + b'\n')
    assert json.loads(stream.readline())['type'] == 'ack'
if sys.argv[2] == 'stay':
    time.sleep(3600)
'''


def task_journey(args):
    """Tasks and the manager through the web API on an isolated wsxd with the stream-json
    fake Claude (scripts/fake-claude-stream.py). ref: docs/web-client-tasks.md."""
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    if not wsx.is_file() or not wsxd.is_file() or wsx.parent != wsxd.parent:
        raise RuntimeError('build adjacent wsx/wsxd first')
    node = shutil.which('node')
    assert node, 'Node is required for the independent Web Push receiver'
    sweep()
    work = ROOT / '.work' / ('web-' + str(os.getpid()) + 't')
    work.mkdir(mode=0o700, parents=True)
    started, success, checks, path, env = time.monotonic(), False, [], None, None

    def ok(label):
        checks.append(label)
        timed(label)

    def wait(test, label, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = test()
            if value:
                return value
            time.sleep(0.1)
        raise AssertionError(label + ' timeout')

    try:
        # ^ Its own TMPDIR: the task host sockets' folder goes away with the run.
        # `calm` has no task and no session: home lists it last. With --browser, `busy`, `quiet` and
        # `gone` have agent sessions and no task: home words their rows from the sessions.
        # `plans` has a task that waits for the owner's answer and one ready to land.
        home, state, project, other, calm, plans, bin_dir, tmp = (work / name for name in ('h', 's', 'demo', 'ui', 'calm', 'plans', 'bin', 't'))
        for folder in (home, state, project, other, calm, plans, bin_dir, tmp):
            folder.mkdir(mode=0o700)
        # ^ The owner's skills the manager composer's `/` list shows (docs/web-client-tasks.md), laid
        # out as the owner's: each a link to ../../.agents/skills/<name>, and a folder with no skill.
        owner_skills = ['fixture-skill'] + ['owner-skill-%02d' % number for number in range(72)]
        for name in owner_skills:
            description = 'Fixture skill for the manager' if name == 'fixture-skill' else '[engineering] ' + 'A long line of what the skill does. ' * 6
            (home / '.agents/skills' / name).mkdir(parents=True)
            (home / '.agents/skills' / name / 'SKILL.md').write_text('---\nname: %s\ndescription: %s\n---\nBODY-SECRET\n' % (name, description))
            (home / '.claude/skills').mkdir(parents=True, exist_ok=True)
            (home / '.claude/skills' / name).symlink_to('../../.agents/skills/' + name)
        (home / '.claude/skills/synced').mkdir()
        project, other, calm, plans = project.resolve(), other.resolve(), calm.resolve(), plans.resolve()
        session_projects = [(work / name) for name in (('busy', 'quiet', 'gone') if args.browser else ())]
        for folder in session_projects:
            folder.mkdir(mode=0o700)
        session_projects = [folder.resolve() for folder in session_projects]
        config = (home / 'Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'c/wsx')
        config.mkdir(parents=True)
        (config / 'config-v2.toml').write_text(
            'resume_agents_on_restore = false\n' + ''.join(
                '[[projects]]\nname = "%s"\npath = %s\n' % (folder.name, json.dumps(str(folder)))
                for folder in (project, other, calm, plans, *session_projects)))
        fake = bin_dir / 'claude'
        fake.write_text('#!/bin/sh\nexec ' + sys.executable + ' ' + str(ROOT / 'scripts/fake-claude-stream.py') + ' "$@"\n')
        fake.chmod(0o755)
        # ^ The preview's tailscale: it records its argv and names this machine.
        (bin_dir / 'tailscale').write_text('#!/bin/sh\necho "$@" >> ' + str(work / 'tailscale.log') + '\n'
                                           'if [ "$1" = status ]; then echo \'{"Self":{"DNSName":"host.example.ts.net."}}\'; fi\n')
        (bin_dir / 'tailscale').chmod(0o755)
        path = state / 'wsx/wsx.sock'
        env = {'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(work / 'c'),
               'XDG_CACHE_HOME': str(work / 'k'), 'WSX_DAEMON_BIN': str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()), 'WSX_SOCKET': str(path),
               'PATH': str(bin_dir) + ':/usr/bin:/bin:/usr/sbin:/sbin', 'SHELL': '/bin/sh',
               'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8', 'TMPDIR': str(tmp),
               # ^ A fixture git command must never reach the repository this script runs in.
               'GIT_CEILING_DIRECTORIES': str(work)}
        for folder in (project, other, calm, plans, *session_projects):
            for argv in (['init', '-q', '-b', 'main'], ['add', '.'], ['commit', '-q', '-m', 'start']):
                if argv[0] == 'add':
                    top = subprocess.run(['git', 'rev-parse', '--show-toplevel'], cwd=folder, env=env,
                                         capture_output=True, text=True, timeout=20).stdout.strip()
                    assert os.path.realpath(top) == os.path.realpath(folder), ('fixture git escaped', folder, top)
                    # ^ ui's manager has a name (docs/tasks.md "Config"); the others read as their project.
                    (folder / 'wsx.config.yml').write_text(
                        TASK_CONFIG.replace('{ model: fake-manager }', '{ name: Atlas, model: fake-manager }') if folder == other
                        else TASK_CONFIG)
                    (folder / 'README').write_text('fixture\n')
                subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', *argv],
                               cwd=folder, env=env, check=True, capture_output=True, timeout=20)

        def cli(*argv, expect=0):
            result = subprocess.run([str(wsx), *map(str, argv)], cwd=project, env=env,
                                    capture_output=True, text=True, timeout=60)
            assert (result.returncode == 0) == (expect == 0), (argv, result.stdout, result.stderr)
            return result

        def js(*argv):
            return json.loads(cli(*argv, '--json').stdout)

        def task_state(n):
            return js('task', 'show', n, '-p', 'demo')['state']

        def wait_state(n, *states):
            return wait(lambda: task_state(n) in states and task_state(n), '#%d %s' % (n, '/'.join(states)))

        daemon = start(work, [str(wsxd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait(path.exists, 'daemon')
        # The fixture store: #1 refused a tool and waiting for the owner, #2 ready with a Human Verify item, #3 ready to land.
        assert js('task', 'new', 'NEEDS Bash(curl:*) WORK m.txt', '-p', 'demo')['id'] == 1
        assert js('task', 'new', 'WORK a.txt HV', '-p', 'demo')['id'] == 2
        assert js('task', 'new', 'WORK b.txt', '-p', 'demo')['id'] == 3
        wait_state(1, 'waiting')
        wait_state(2, 'ready')
        wait_state(3, 'ready')
        # plans' chat begins with an older log (`log.1.jsonl`, read before `log.jsonl`): six events,
        # each with the manager's quiet reply, an owner message, one more event and reply,
        # another owner message and an event with a long reply. The page folds the first run (docs/web-client-tasks.md
        # "Project screen"). Delivered turns, so its manager never takes them as new; ids far past
        # the ones it counts from.
        manager_dir = state / 'wsx/tasks' / ('plans-' + hashlib.sha256(str(plans).encode()).hexdigest()[:8]) / 'manager'
        manager_dir.mkdir(mode=0o700, parents=True)
        old_log, first_ms = [], int(time.time() * 1000) - 3_600_000

        def logged(author, text, reply, quiet=True):
            said, at = 9001 + len(old_log), first_ms + len(old_log) * 20_000
            old_log.extend([{'kind': 'message', 'id': said, 'from': author, 'text': text, 'unix_ms': at},
                            {'kind': 'delivered', 'ids': [said], 'unix_ms': at + 1},
                            {'kind': 'reply', 'id': said + 1, 'to': [said], 'text': reply, 'quiet': quiet, 'unix_ms': at + 2000}])
        for line, reply in (('#89 ready r1: Fold the events', 'Landing #89.'), ('#89 landed r1: Fold the events', '#89 is on main.'),
                            ('#91 ready r1: Chat chips', "I'm landing #91. It touches the chat, so I read its captures first."),
                            ('#88 ready r1: Since line', 'Landing #88.'), ('#91 landed r1: Chat chips', '#91 is on main.'),
                            ('#88 landed r1: Since line', '#88 is on main.')):
            logged('events', line, reply)
        logged('owner', 'Keep going.', 'On it.', quiet=False)
        logged('events', '#92 ready r1: Short run', 'Landing #92.')
        logged('owner', 'Thanks.', 'Noted.', quiet=False)
        # ^ The usage limit held that turn: wsx wrote a pause and a resume between the message and the reply.
        held_at = old_log[-3]['unix_ms']
        old_log[-1:-1] = [{'kind': 'limit', 'until': '07:45', 'unix_ms': held_at + 100},
                          {'kind': 'limit', 'unix_ms': held_at + 1500}]
        # ^ Stored quiet before the length rule: a long reply to events reads as a message.
        logged('events', '#93 landed r1: Release notes', 'Release notes are ready:\n\n| Part | State |\n|---|---|\n| wsx | built |\n\n- the notes are written')
        # ^ Older hosts logged a `-` reply to an owner message as a message, and a `sent` without
        # --json with no text: the page shows neither (docs/web-client-tasks.md "Project screen").
        logged('owner', 'Anything new?', '-', quiet=False)
        old_log.append({'kind': 'sent', 'to': 'ui', 'text': '', 'unix_ms': old_log[-1]['unix_ms'] + 1000})
        (manager_dir / 'log.1.jsonl').write_text(''.join(json.dumps(entry) + '\n' for entry in old_log))
        # `plans`: #1 waits for the owner (its worker was refused a tool and asks), #2 is ready.
        assert js('task', 'new', 'Let me jump between projects from the keyboard NEEDS WebSearch NOOP', '-p', 'plans')['id'] == 1
        # ^ A long file name: the sheet's "Land stops" line must wrap it on a phone.
        assert js('task', 'new', 'WORK %s' % LONG_NAME, '-p', 'plans')['id'] == 2
        for n, state_name in ((1, 'waiting'), (2, 'ready')):
            wait(lambda: js('task', 'show', n, '-p', 'plans')['state'] == state_name, 'plans #%d %s' % (n, state_name))
        if args.browser:
            # The same store in the second project (`ui`), for the page: the browser acts on it.
            for prompt in ('NEEDS Bash(curl:*) WORK m.txt', 'WORK a.txt HV', 'WORK b.txt', 'NEEDS Bash(curl:*) WORK n.txt'):
                js('task', 'new', prompt, '-p', 'ui')
            for n, state_name in ((1, 'waiting'), (2, 'ready'), (3, 'ready'), (4, 'waiting')):
                wait(lambda: js('task', 'show', n, '-p', 'ui')['state'] == state_name, 'ui #%d' % n)
            # a1 lands #3, a2 discards #4: both wait for the owner.
            js('task', 'say', 'RUN wsx task land 3 -p ui --json\nRUN wsx task close 4 --discard -p ui --json', '-p', 'ui')
            wait(lambda: len(json.loads(cli('task', 'ledger', '-p', 'ui').stdout)['approvals'] or []) == 2,
                 'approval requests in ui', timeout=30)
            # #5: a long report the manager hears of (one worker bubble, whole), then closed.
            # #6: works until the run ends; its prompt opens as the manager's prompts do.
            long_report = 'WORK c.txt\n' + '\n\n'.join('line %d of the report' % n for n in range(1, 41))
            assert js('task', 'new', long_report, '-p', 'ui')['id'] == 5
            wait(lambda: js('task', 'show', 5, '-p', 'ui')['state'] == 'ready', 'ui #5')
            wait(lambda: any('#5 ready r1' in e.get('text', '') for e in js('task', 'chat', '-p', 'ui')['entries']),
                 'the manager heard of #5', timeout=30)
            js('task', 'close', 5, '--discard', '-p', 'ui')
            js('task', 'new', 'Owner asked: keep a worker busy WAIT ' + str(work / 'release'), '-p', 'ui')
            wait(lambda: js('task', 'show', 6, '-p', 'ui')['state'] == 'working', 'ui #6 working')
            # #7 and #8 landed with an item each to check, #7's with an image capture and #8's
            # with a video and its poster; the manager hears of both ("#7 · done").
            for n, prompt in ((7, 'WORK d.txt HV CAPTURE done. More words follow'), (8, 'WORK e.txt HV VIDEO')):
                assert js('task', 'new', prompt, '-p', 'ui')['id'] == n
                wait(lambda: js('task', 'show', n, '-p', 'ui')['state'] == 'ready', 'ui #%d' % n)
                assert js('task', 'land', n, '-p', 'ui')['state'] == 'landed'
            wait(lambda: sum('#%d landed r1' % n in e.get('text', '') for n in (7, 8) for e in js('task', 'chat', '-p', 'ui')['entries']) == 2,
                 'the manager heard of #7 and #8', timeout=30)
            # The manager of plans asked ui's manager, which answered: both show as bubbles in ui.
            js('task', 'say', 'RUN wsx task say -p ui --from-manager "How is ui?" --json', '-p', 'plans')
            wait(lambda: any(e.get('from') == 'manager:plans' for e in js('task', 'chat', '-p', 'ui')['entries']),
                 'ui heard plans', timeout=30)
            # ^ Without --json: the sender's log still holds the words it sent.
            js('task', 'say', 'RUN wsx task say -p plans --from-manager --reply "ui is fine"', '-p', 'ui')
            wait(lambda: any(e['kind'] == 'sent' for e in js('task', 'chat', '-p', 'ui')['entries']), 'ui answered plans', timeout=30)
            sent = [e for e in js('task', 'chat', '-p', 'ui')['entries'] if e['kind'] == 'sent']
            assert [e['text'] for e in sent] == ['ui is fine'], sent

            # ^ plans' turn held only ui's reply: its short answer is quiet, as one to events
            # (docs/tasks.md "Manager").
            def answered():
                entries = js('task', 'chat', '-p', 'plans')['entries']
                asked = {e['id'] for e in entries if e.get('from') == 'manager-reply:ui'}
                return [e for e in entries if e['kind'] == 'reply' and asked & set(e['to'])]
            wait(answered, 'plans answered the reply of ui', timeout=30)
            assert all(e.get('quiet') for e in answered()), answered()
            # A deploy of the default branch tip: the brief says it is up to date until a task lands.
            # A deploy by hand answers that it started; its detached run records the result.
            assert js('task', 'deploy', '-p', 'ui')['started'] is True
            wait(lambda: js('task', 'deployed', '-p', 'ui')['deploy'].get('behind') == 0, 'the deploy of ui was recorded', timeout=60)
            # plans' deploy fails with 60 lines: its project page folds their end.
            failed = subprocess.run([str(wsx), 'task', 'deploy', '-p', 'plans', '--json'], env=dict(env, WSX_FIXTURE_DEPLOY_FAIL='1'),
                                    cwd=project, capture_output=True, text=True, timeout=60)
            assert json.loads(failed.stdout)['started'] is True, (failed.stdout, failed.stderr)
            wait(lambda: js('task', 'deployed', '-p', 'plans')['deploy'].get('failed_since') == 1,
                 'the failed deploy of plans was recorded', timeout=60)
            # Sessions without tasks, oldest first: an agent that exited (`gone`), one done
            # (`quiet`), two working (`busy`).
            agent_script = work / 'session-agent.py'
            agent_script.write_text(wsxd_client.fixture(SESSION_AGENT))

            def call(method, params):
                return wsxd_client.call(path, method, params, timeout=30)
            snapshot = call('snapshot', None)['data']
            names = {item['id']: Path(item['path']).name for item in snapshot['projects']}
            worktrees = {names[item['project_id']]: item['id'] for item in snapshot['worktrees']}
            for name, state_name, then in (('gone', 'working', 'end'), ('quiet', 'done', 'stay'),
                                           ('busy', 'working', 'stay'), ('busy', 'working', 'stay')):
                created = call('session_create', {'worktree_id': worktrees[name], 'label': name,
                    'command': [sys.executable, str(agent_script), state_name, then], 'rows': 12, 'cols': 80})
                assert created['type'] == 'created', created
                session_id = created['data']['id']

                def reported():
                    current = call('snapshot', None)['data']
                    pane_id = next(s['primary_pane'] for s in current['sessions'] if s['id'] == session_id)
                    pane = next(p for p in current['panes'] if p['id'] == pane_id)
                    return pane.get('agent') and (pane['exited'] or then == 'stay')
                wait(reported, '%s session %s' % (name, then))
                time.sleep(0.05)
            # A plain worktree: `ui` gets a git worktree that no task owns, synced to wsxd
            # as a wsx client does (`wsx status` lists the git worktrees), with an agent session.
            plain_path = work / 'ui-old'
            subprocess.run(['git', 'worktree', 'add', '-q', '-b', 'old-work', str(plain_path)],
                           cwd=other, env=env, check=True, capture_output=True, timeout=20)
            plain_path = plain_path.resolve()
            cli('status', '--json')
            current = call('snapshot', None)['data']
            ui_worktrees = {Path(item['path']).name: item for item in current['worktrees'] if names[item['project_id']] == 'ui'}
            # ^ wsxd lists the task copies and the plain worktree after the sync.
            assert {'ui', 'ui-old', 'ui-t1', 'ui-t2', 'ui-t3', 'ui-t4'} <= set(ui_worktrees), sorted(ui_worktrees)
            assert ui_worktrees['ui-old']['branch'] == 'old-work' and ui_worktrees['ui-t1']['branch'] == 'wsx/t1', ui_worktrees
            created = call('session_create', {'worktree_id': ui_worktrees['ui-old']['id'], 'label': 'old chat',
                'command': [sys.executable, str(agent_script), 'done', 'stay'], 'rows': 12, 'cols': 80})
            assert created['type'] == 'created', created
            plain_session = created['data']['id']

            def plain_reported():
                current = call('snapshot', None)['data']
                pane_id = next(s['primary_pane'] for s in current['sessions'] if s['id'] == plain_session)
                return next(p for p in current['panes'] if p['id'] == pane_id).get('agent')
            wait(plain_reported, 'plain worktree session reported')

            def plain_git():
                listing = subprocess.run(['git', 'worktree', 'list', '--porcelain'], cwd=other, env=env, check=True,
                                         capture_output=True, text=True, timeout=20).stdout
                block = next(part for part in listing.split('\n\n') if part.split('\n', 1)[0].endswith('/ui-old'))
                head = subprocess.run(['git', 'rev-parse', 'refs/heads/old-work'], cwd=other, env=env, check=True,
                                      capture_output=True, text=True, timeout=20).stdout.strip()
                status = subprocess.run(['git', 'status', '--porcelain'], cwd=plain_path, env=env, check=True,
                                        capture_output=True, text=True, timeout=20).stdout
                return block, head, status
            plain_before = plain_git()

        push = start(work, [node, str(ROOT / 'scripts/web-push-fixture.mjs'), str(work / 'push.jsonl')],
                     stdout=subprocess.PIPE, text=True)
        receiver = json.loads(push.stdout.readline())
        push_origin = 'http://127.0.0.1:%d' % receiver['port']
        vapid = cli('web', 'vapid').stdout
        private_key = next(line.split('"')[1] for line in vapid.splitlines() if line.startswith('private_key'))
        ports = {name: free_port() for name in ('owner', 'viewer', 'plain', 'relay')}
        # ^ The browser reaches the owner's site through `Relay`, so its public origin is the relay's.
        public = {name: 'http://127.0.0.1:%d' % ports['relay' if name == 'owner' else name] for name in ('owner', 'viewer', 'plain')}
        grants = {
            'owner': ('"all"', '"task_view", "task_act", "push", "media_upload", "media_download"'),
            'viewer': (json.dumps([str(project)]), '"task_view", "push"'),
            'plain': (json.dumps([str(other)]), '"chat_send", "push"'),
        }
        files, sites, servers = {}, {}, {}
        for name, (projects_value, capabilities) in grants.items():
            files[name] = work / (name + '.toml')
            files[name].write_text(f'''id = "{name}"
name = "{name}"
port = {ports[name]}
public_origin = "{public[name]}"
panels = ["chat"]
[access]
projects = {projects_value}
capabilities = [{capabilities}]
[push]
subject = "mailto:owner@example.invalid"
private_key = "{private_key}"
service_origins = ["{push_origin}"]
[notify.manager]
merge_seconds = 3
''')
            files[name].chmod(0o600)
            servers[name] = start(work, [str(wsx), 'web', 'serve', '--site', str(files[name])], env=env, cwd=project,
                                  stdout=subprocess.DEVNULL, stderr=(work / (name + '.log')).open('a'))
            sites[name] = Site(ports[name], '', origin=public[name])
        for site in sites.values():
            wait(lambda: _up(site), 'web server')
        for name, site in sites.items():
            code = cli('web', 'pair', '--site', files[name]).stdout.splitlines()[0].rsplit(' ', 1)[1]
            status, headers, body = site.request('POST', '/api/pair', {'code': code.lower(), 'name': name}, raw=True)
            assert status == 200, body
            site.cookie = headers['set-cookie'].split(';')[0]
        owner, viewer, plain = sites['owner'], sites['viewer'], sites['plain']
        # ^ The browser check restarts the owner's server under another name (a new build).
        owner_name = 'owner'
        session = owner.request('GET', '/api/session')[1]
        assert 'task_view' in session['site']['capabilities'] and 'task_act' in session['site']['capabilities']
        events = Events(owner)
        workspace = events.expect('workspace')
        ids = {item['name']: item['id'] for item in workspace['projects']}
        demo = ids['demo']

        # --- Read models: the strip order, the working count, the chip list.
        overview = steady(owner, 'GET', '/api/tasks/%d' % demo)[1]
        kinds = [(item['kind'], item.get('task')) for item in overview['needs']]
        assert kinds[0] == ('waiting', 1) and sorted(kinds[1:]) == [('land', 2), ('land', 3)], kinds
        assert overview['needs'][0]['waiting']['on'] == 'owner', overview['needs'][0]
        assert overview['needs'][1]['reason'] == 'ready to land' and overview['working'] == 0 and overview['to_check'] == 0, overview
        # Three states for the owner; the store's own states stay in the record.
        assert {row['id']: row['stage'] for row in overview['tasks']} == {1: 'needs_you', 2: 'needs_you', 3: 'needs_you'}
        # Chips and the `#` picker: attention order, then the newest first.
        assert [row['id'] for row in overview['tasks']] == [1, 3, 2], overview['tasks']
        # ^ A task without --title is named by the first sentence of its worker's summary.
        assert overview['tasks'][2]['title'] == 'did: WORK a.txt HV'
        # ^ No Adopted list reads the task branches any more.
        assert 'task_branches' not in overview, list(overview)
        everything = steady(owner, 'GET', '/api/tasks')[1]
        assert sorted(item['project'] for item in everything) == sorted(ids.values()), everything
        detail = steady(owner, 'GET', '/api/tasks/%d/t/2' % demo)[1]
        assert detail['state'] == 'ready' and detail['stage'] == 'needs_you' and detail['actions'] == ['land', 'add', 'hand'], detail['actions']
        assert detail['land_reason'] == 'ready to land' and detail['to_check'] == [] and detail['fixes'] is None, detail
        assert detail['result']['human_verify'] == [{'check': 'open the file', 'where': 'a real device', 'capture': '', 'poster': '', 'pass_if': 'it has text'}]
        headings = [line for line in detail['report'].splitlines() if line.startswith('## ')]
        assert headings == ['## Summary', '## Changes', '## Evidence', '## To check', '## Left open'], headings
        # ^ Usage: the task's sum is `show --json`'s, and the project's workers today hold every
        # task's turns (the fake worker's results report no tokens; docs/tasks.md "Usage").
        usage = detail['usage']
        assert usage['turns'] >= 1, usage
        assert js('task', 'show', 2, '-p', 'demo')['usage'] == usage
        spent = [steady(owner, 'GET', '/api/tasks/%d/t/%d' % (demo, row['id']))[1]['usage'] for row in overview['tasks']]
        workers = overview['usage']['today']['workers']
        assert workers['turns'] == sum(one['turns'] for one in spent) and workers['cost_micro_usd'] == sum(one['cost_micro_usd'] for one in spent), (workers, spent)
        assert overview['usage']['week']['workers'] == workers, overview['usage']
        ledger_usage = json.loads(cli('task', 'ledger', '-p', 'demo').stdout)['usage']
        assert ledger_usage['today']['workers'] == workers, ledger_usage
        kinds = [entry['kind'] for entry in detail['timeline']['entries']]
        assert kinds[:2] == ['created', 'message'] and 'result' in kinds and 'revision' in kinds, kinds
        assert 'verification' not in detail and detail['overlaps'] == []
        ok('read models: three stages, needs-you order (waiting, then land with its reason), denials, report sections, timeline, actions by state')

        # --- Fixed actions go straight to the task host.
        def act(n, action, body, site=owner, project=demo):
            return steady(site, 'POST', '/api/tasks/%d/t/%d/%s' % (project, n, action), body)

        # ^ No per-task verdict: the route is gone, and so is its body field.
        assert act(2, 'verify', {})[0] == 404
        assert act(2, 'land', {'result': 'pass'})[0] == 422
        assert act(2, 'add', {'text': '  '})[0] == 400
        assert act(2, 'add', {'text': 'x' * (64 * 1024 + 1)})[0] == 413
        assert act(2, 'explode', {})[0] == 404 and act(99, 'add', {'text': 'hi'})[0] == 404
        # ^ A second device of the owner: these steps keep to their own request budget.
        trier = Site(ports['owner'], '', origin=public['owner'])
        code = cli('web', 'pair', '--site', files['owner']).stdout.splitlines()[0].rsplit(' ', 1)[1]
        status, headers, body = trier.request('POST', '/api/pair', {'code': code.lower(), 'name': 'trier'}, raw=True)
        assert status == 200, body
        trier.cookie = headers['set-cookie'].split(';')[0]
        # Hold and trial (docs/tasks.md "Hold and trial"): the owner holds #2, tries it on the
        # live site, ends the trial and releases the hold. Pass needs a running trial.
        status, body = act(2, 'pass', {}, site=trier)
        assert status == 409 and 'no trial of t2 runs' in body['error']['message'], body
        assert act(2, 'hold', {'text': 'two\nlines'}, site=trier)[0] == 400
        assert act(2, 'hold', {}, site=trier)[0] == 200
        held = steady(trier, 'GET', '/api/tasks/%d/t/2' % demo)[1]
        assert held['hold'] == 'the owner holds it' and held['land_reason'] == 'try it first: the owner holds it', held
        assert held['actions'] == ['land', 'add', 'hand'] and held['trial'] is None, held['actions']
        overview = steady(trier, 'GET', '/api/tasks/%d' % demo)[1]
        assert overview['try_live'] is True and overview['trial'] is None, overview
        assert act(2, 'trial', {}, site=trier)[0] == 200
        # ^ The CLI polls: the device's request budget stays for the steps that follow.
        wait(lambda: (js('task', 'deployed', '-p', 'demo')['trial'] or {}).get('stage') == 'live',
             'the trial of #2 goes live')
        assert steady(trier, 'GET', '/api/tasks/%d/t/2' % demo)[1]['trial']['stage'] == 'live'
        assert steady(trier, 'GET', '/api/tasks/%d' % demo)[1]['trial']['task'] == 2
        status, body = act(3, 'trial', {}, site=trier)
        assert status == 409 and 'runs the trial of #2' in body['error']['message'], body
        assert act(2, 'trial_end', {}, site=trier)[0] == 200
        assert steady(trier, 'GET', '/api/tasks/%d' % demo)[1]['trial'] is None
        assert act(2, 'unhold', {}, site=trier)[0] == 200
        assert steady(trier, 'GET', '/api/tasks/%d/t/2' % demo)[1]['hold'] is None
        ok('hold and trial: hold and release, Try live goes live, one trial per project, End frees the site')
        status, body = act(1, 'land', {})
        assert status == 409 and body['error']['code'] == 'refused', body
        status, body = act(2, 'land', {})
        assert status == 200 and body['state'] == 'landed', body
        # The landing adds #2's item to the project's To check list; #2 is done.
        events.expect('task', lambda data: data == {'project': demo, 'task': 2})
        events.expect('tasks', lambda data: data['project'] == demo and data['to_check'] == 1 and
                      all(item.get('task') != 2 for item in data['needs']))
        listed = steady(owner, 'GET', '/api/tasks/%d/checks' % demo)[1]['open']
        assert [(i['id'], i['task'], i['title'], i['check'], i['state']) for i in listed] == [
            ('c1', 2, 'did: WORK a.txt HV', 'open the file', 'open')], listed
        # ^ The title is already the worker's sentence; an item without a capture has no image.
        assert listed[0]['did'] is None and 'capture' not in listed[0] and listed[0]['where'] == 'a real device', listed
        assert steady(owner, 'GET', '/api/tasks/%d/checks/c1/capture' % demo)[0] == 404
        assert steady(owner, 'GET', '/api/tasks/%d/checks/c99/capture' % demo)[0] == 404
        landed = steady(owner, 'GET', '/api/tasks/%d/t/2' % demo)[1]
        assert landed['stage'] == 'done' and [i['id'] for i in landed['to_check']] == ['c1'] and landed['actions'] == [], landed
        status, body = act(1, 'add', {'text': 'NOOP'})
        assert status == 200, body
        wait_state(1, 'ready')
        status, body = act(1, 'add', {'text': 'NOOP and again'})
        assert status == 200, body
        wait(lambda: 'NOOP and again' in (work / 'fake-claude.trace').read_text(), 'add reached the worker')
        wait_state(1, 'ready')
        overview = steady(owner, 'GET', '/api/tasks/%d' % demo)[1]
        kinds = [(item['kind'], item.get('task')) for item in overview['needs']]
        assert kinds == [('land', 3), ('land', 1)], kinds  # oldest first within a kind; landed #2 is no need
        # A steer reaches the running turn.
        assert js('task', 'new', 'SLOW WORK s.txt', '-p', 'demo')['id'] == 4
        wait_state(4, 'working')
        status, body = act(4, 'steer', {'text': 'steer words'})
        assert status == 200, body
        wait(lambda: 'steer words' in (work / 'fake-claude.trace').read_text(), 'steer reached the turn')
        wait_state(4, 'ready')
        # By hand (docs/tasks.md "By hand"): the owner's own pane in the task's copy; the
        # worker gets nothing until Hand back, which checks the work as a new revision.
        # ^ A steer that reaches the worker before its turn reads steers runs as a turn of its
        # own, so the revision before hand is 1 or 2; hand back adds one.
        before_hand = steady(owner, 'GET', '/api/tasks/%d/t/4' % demo)[1]['revision']
        status, body = act(4, 'hand', {})
        assert status == 200 and body['state'] == 'waiting' and body['session'] == 'task-4-hand', body
        handed = steady(owner, 'GET', '/api/tasks/%d/t/4' % demo)[1]
        assert handed['stage'] == 'working' and handed['actions'] == ['handback'], handed['actions']
        assert all(item.get('task') != 4 for item in steady(owner, 'GET', '/api/tasks/%d' % demo)[1]['needs'])
        status, body = act(4, 'add', {'text': 'NOOP'})
        assert status == 409 and 'handback' in body['error']['message'], body
        status, body = act(4, 'handback', {})
        assert status == 200 and len(body['sessions_closed']) == 1, body
        wait_state(4, 'ready')
        assert steady(owner, 'GET', '/api/tasks/%d/t/4' % demo)[1]['revision'] == before_hand + 1
        ok('actions: no verdict route, land refused then landed with its item to check, add resumes a waiting task and reaches the worker, steer reaches the turn, hand and hand back')

        # --- The manager: say starts it, the reply arrives in the chat, events follow.
        status, body = steady(owner, 'POST', '/api/tasks/%d/say' % demo, {'text': 'STATUS'})
        assert status == 200 and body['ok'] and isinstance(body['id'], int), body
        events.expect('manager', lambda data: data == {'project': demo}, timeout=20)
        chat = wait(lambda: (lambda data: data if any(e['kind'] == 'reply' for e in data['entries']) else None)(
            steady(owner, 'GET', '/api/tasks/%d/chat' % demo)[1]), 'manager reply')
        reply = next(e for e in chat['entries'] if e['kind'] == 'reply')
        assert reply['text'].startswith('open: #1 ready') and chat['running'], chat
        assert all(e['kind'] not in ('delivered', 'agent') for e in chat['entries'])
        # ^ The manager's replies count apart from the workers' (docs/tasks.md "Usage").
        manager_usage = steady(owner, 'GET', '/api/tasks/%d' % demo)[1]['usage']['today']['manager']
        assert manager_usage['turns'] >= 1 and manager_usage['cost_micro_usd'] == 1000 * manager_usage['turns'], manager_usage
        later = steady(owner, 'GET', '/api/tasks/%d/chat?after=%d' % (demo, reply['id']))[1]
        assert all(e.get('id', reply['id'] + 1) > reply['id'] for e in later['entries']), later
        # --- A role (docs/tasks.md "Roles"): its chat by ?role=, a say to it starts it, and the
        # manager's chat lists it as a contact.
        notes_brief = work / 'notes-brief.md'
        notes_brief.write_text('Keep the notes.\n')
        js('task', 'role', 'new', 'notes', '--brief', notes_brief, '-p', 'demo')
        status, body = steady(owner, 'POST', '/api/tasks/%d/say' % demo, {'text': 'STATUS', 'role': 'notes'})
        assert status == 200 and body['ok'], body
        events.expect('manager', lambda data: data == {'project': demo, 'role': 'notes'}, timeout=20)
        role_chat = wait(lambda: (lambda data: data if any(e['kind'] == 'reply' for e in data['entries']) else None)(
            steady(owner, 'GET', '/api/tasks/%d/chat?role=notes' % demo)[1]), 'role reply')
        assert role_chat['manager'] == {'name': 'notes'} and role_chat['roles'] is None and role_chat['running'], role_chat
        contacts = steady(owner, 'GET', '/api/tasks/%d/chat' % demo)[1]['roles']
        assert [(c['name'], c['brief']) for c in contacts] == [('notes', 'Keep the notes.')], contacts
        assert contacts[0]['last_reply'].startswith('open: ') and contacts[0]['last_unix_ms'] > 0, contacts
        assert steady(owner, 'GET', '/api/tasks/%d/chat?role=nobody' % demo)[0] == 404
        status, body = steady(owner, 'POST', '/api/tasks/%d/say' % demo, {'text': 'hi', 'role': 'nobody'})
        assert status >= 400 and 'no open role nobody' in json.dumps(body), (status, body)
        ok('roles: a role chat by ?role=, a say to a role starts it, the manager chat lists it as a contact, an unknown role refused')
        # A manager turn alone changes the overview: its `busy` (the home row's typing dots) goes
        # true while the turn runs and false after it (docs/web-client-tasks.md "Home").
        watcher = Events(owner)
        watcher.expect('workspace')
        status, body = steady(owner, 'POST', '/api/tasks/%d/say' % demo, {'text': 'SLOW STATUS'})
        assert status == 200, body
        watcher.expect('tasks', lambda data: data['project'] == demo and data['busy'], timeout=20)
        watcher.expect('tasks', lambda data: data['project'] == demo and not data['busy'], timeout=20)
        # ^ A quiet stream pings every 10 s, so a page can tell a silent dead stream from a quiet one.
        watcher.expect('ping', timeout=15)
        # Streams a phone left behind cannot lock the device out (docs/web-client.md "Limits"):
        # a page's new stream ends its older one, and past four the device's oldest one ends.
        first = Events(owner, page='relay-page-1')
        again = Events(owner, page='relay-page-1')
        assert first.closed.wait(5) and not again.closed.is_set()
        fourth = Events(owner)
        fifth = Events(owner)
        assert events.closed.wait(5) and not any(stream.closed.is_set() for stream in (watcher, again, fourth))
        fifth.expect('workspace')
        events = Events(owner)
        events.expect('workspace')
        ok('manager: a turn alone sends the overview with busy and then without; a quiet stream pings; a page replaces its stream, past four streams the oldest ends')
        # An approval request from the manager, approved on the web; its outcome is the decision.
        status, body = steady(owner, 'POST', '/api/tasks/%d/say' % demo, {'text': 'RUN wsx task land 3 -p demo --json'})
        assert status == 200, body
        request = wait(lambda: next((item for item in steady(owner, 'GET', '/api/tasks/%d' % demo)[1]['needs']
                                     if item['kind'] == 'approval'), None), 'approval request', timeout=30)
        assert request['approval'] == 'a1' and request['action'] == 'land' and request['task'] == 3, request
        assert request['label'] == 'Approve land #3', request
        status, body = steady(owner, 'POST', '/api/tasks/%d/approvals/a1/approve' % demo, {})
        assert status == 202 and body['started'], body
        # ^ A second tap while it runs, or once decided, answers what happened: not an error.
        again = steady(owner, 'POST', '/api/tasks/%d/approvals/a1/approve' % demo, {})
        assert again[0] == 202 and (again[1].get('running') or again[1].get('decided')), again
        decided = events.expect('tasks', lambda data: data['project'] == demo and data['decisions'], timeout=30)
        assert decided['decisions'][0]['approval'] == 'a1' and decided['decisions'][0]['outcome'].startswith('done'), decided
        # ^ When main moved since #3's revision the land rebases, checks again and then lands.
        wait_state(3, 'landed')
        status, body = steady(owner, 'POST', '/api/tasks/%d/say' % demo, {'text': 'RUN wsx task close 1 --discard -p demo --json'})
        wait(lambda: any(item['kind'] == 'approval' and item['approval'] == 'a2'
                         for item in steady(owner, 'GET', '/api/tasks/%d' % demo)[1]['needs']), 'discard request', timeout=30)
        status, body = steady(owner, 'POST', '/api/tasks/%d/approvals/a2/deny' % demo, {'note': 'keep it'})
        assert status == 200, body
        assert steady(owner, 'POST', '/api/tasks/%d/approvals/a2/deny' % demo, {})[0] == 409
        assert steady(owner, 'POST', '/api/tasks/%d/approvals/a2/maybe' % demo, {})[0] == 404
        assert task_state(1) == 'ready'
        # The manager composer's lists: the owner's skills (name and description, never the body)
        # and the other registered projects with their manager's set name.
        mentions = steady(owner, 'GET', '/api/tasks/%d/mentions' % ids['plans'])[1]
        assert [skill['name'] for skill in mentions['skills']] == owner_skills, mentions
        assert mentions['skills'][0]['description'] == 'Fixture skill for the manager', mentions['skills'][0]
        assert 'BODY-SECRET' not in json.dumps(mentions) and mentions['truncated'] is False, mentions
        by_name = {item['name']: item for item in mentions['projects']}
        assert 'plans' not in by_name and by_name['demo']['manager'] is None and 'ui' in by_name, mentions['projects']
        assert not args.browser or by_name['ui']['manager'] == 'Atlas', mentions['projects']
        assert steady(owner, 'GET', '/api/tasks/%d/mentions' % 999999)[0] == 404
        since = steady(owner, 'GET', '/api/tasks/%d/since?after=1' % demo)[1]
        assert since['tasks']['landed'] == [2, 3] and since['tasks']['started'] == [1, 2, 3, 4] and since['replies'] >= 3, since
        # What was settled, newest first, one line each; the home row's last reply and activity.
        overview = steady(owner, 'GET', '/api/tasks/%d' % demo)[1]
        settled = [row['text'] for row in overview['resolutions']]
        for line in ('Denied discard #1', 'Approved land #3', '#1 got an answer'):
            assert line in settled, (line, settled)
        # ^ A landing is the worker's "done" bubble in the chat, never a line.
        assert settled.index('Denied discard #1') < settled.index('Approved land #3') and not any('landed' in line for line in settled), settled
        stamps = [row['unix_ms'] for row in overview['resolutions']]
        assert stamps == sorted(stamps, reverse=True) and overview['resolutions'][0]['task'] == 1, overview['resolutions']
        assert overview['last_reply'] and len(overview['last_reply']) <= 121 and overview['last_unix_ms'] >= overview['news']['unix_ms'], overview
        empty = steady(owner, 'GET', '/api/tasks/%d' % ids['calm'])[1]
        assert empty['needs'] == [] and empty['resolutions'] == [] and empty['last_reply'] is None and empty['last_unix_ms'] is None, empty
        ok('manager: say starts it, reply in the chat, approve runs once and records the outcome, deny with a note, since, resolutions, last reply')
        # The project page: the manager host writes its sections once; a project without a
        # manager has none. A title: `--title` as given, else the first sentence of the
        # worker's summary, else the prompt's first line without a leading "Owner <word>:"
        # (docs/tasks.md "Store"; a task without a result is #6 of the browser fixture).
        page = steady(owner, 'GET', '/api/tasks/%d/page' % demo)[1]
        assert page['text'].startswith('# demo\n\n## Goal') and '```mermaid' in page['text'] and page['unix_ms'] > 0, page
        assert steady(owner, 'GET', '/api/tasks/%d/page' % ids['calm'])[1] == {'text': None, 'unix_ms': None}
        assert steady(viewer, 'GET', '/api/tasks/%d/page' % ids['ui'])[0] == 404
        for title, refusal in (('  ', 'the title is empty'), ('x' * 61, 'longer than 60 characters'), ('a\tb', 'control characters')):
            refused = cli('task', 'new', 'WORK t.txt', '--title', title, '-p', 'demo', expect=1)
            assert refusal in refused.stderr, refused.stderr
        titled = js('task', 'new', 'Owner report: WORK t.txt', '--title', ' Short words ', '-p', 'demo')['id']
        stripped = js('task', 'new', 'Owner report: tool permission for WORK u.txt\nmore', '-p', 'demo')['id']
        # ^ A lead-in with brackets (and a colon in them) names its subject; a bare one gives
        # way to the next line; a `--title` loses it too; `Owner:` alone is not a lead-in.
        bracketed = js('task', 'new', 'Owner report (UX failures: how a task asks):\n1. WORK v.txt', '-p', 'demo')['id']
        bare = js('task', 'new', 'Owner request:\n\nWORK w.txt now', '-p', 'demo')['id']
        retitled = js('task', 'new', 'WORK x.txt', '--title', 'Owner asked: short words', '-p', 'demo')['id']
        untouched = js('task', 'new', "Owner: Alice's WORK y.txt", '-p', 'demo')['id']
        # ^ The web names a task without --title by its summary's first sentence.
        sentence = js('task', 'new', 'Owner report: tool permission. WORK z.txt', '-p', 'demo')['id']
        made = (titled, stripped, bracketed, bare, retitled, untouched, sentence)
        for n in made:
            wait(lambda: task_state(n) == 'ready', 'titled task %d' % n)
            js('task', 'close', n, '--discard', '-p', 'demo')
        expected = {titled: 'Short words', stripped: 'tool permission for WORK u.txt', bracketed: 'UX failures: how a task asks',
                    bare: 'WORK w.txt now', retitled: 'short words', untouched: "Owner: Alice's WORK y.txt"}
        titles = {row['id']: row['title'] for row in steady(owner, 'GET', '/api/tasks/%d' % demo)[1]['tasks']}
        listed = {row['id']: row['title'] for row in js('task', 'list', '-p', 'demo', '--all')['tasks']}
        expected[sentence] = 'tool permission. WORK z.txt'
        assert all(listed[n] == title for n, title in expected.items()), listed
        shown = {titled: 'Short words', retitled: 'short words', stripped: 'did: Owner report: tool permission for WORK u.txt',
                 bracketed: 'did: Owner report (UX failures: how a task asks):', bare: 'did: Owner request:',
                 untouched: "did: Owner: Alice's WORK y.txt", sentence: 'did: Owner report: tool permission.'}
        assert all(titles[n] == title for n, title in shown.items()), titles
        # ^ Only the shown title changes: the record and the prompt keep what was written.
        record = Path(js('task', 'show', retitled, '-p', 'demo')['log']).parent / 'task.json'
        assert json.loads(record.read_text())['title'] == 'Owner asked: short words'
        logged = Path(js('task', 'show', bracketed, '-p', 'demo')['log']).read_text().splitlines()
        assert json.loads(logged[1])['text'].startswith('Owner report (UX failures: how a task asks):'), logged[1]
        ok('project page: written by the manager host, none without a manager, 404 outside the grant; titles: --title, its refusals, "Owner <words>:" left out when shown and kept on disk, on the web the first sentence of the summary')

        # --- To check (docs/tasks.md "To check"): pass one, fail one with a note (a linked fix
        # task starts), pass the rest; the owner's alone.
        made = []
        for prompt in ('WORK h.txt HV', 'WORK i.txt HV'):
            made.append(js('task', 'new', prompt, '-p', 'demo')['id'])
            wait(lambda: task_state(made[-1]) == 'ready', 'demo #%d' % made[-1])
            assert js('task', 'land', made[-1], '-p', 'demo')['state'] == 'landed'
        assert [i['id'] for i in steady(owner, 'GET', '/api/tasks/%d/checks' % demo)[1]['open']] == ['c1', 'c2', 'c3']

        def decide(item, decision, body=None, site=owner):
            return steady(site, 'POST', '/api/tasks/%d/checks/%s/%s' % (demo, item, decision), body or {})
        assert decide('c1', 'pass', site=viewer)[0] == 403
        assert steady(viewer, 'GET', '/api/tasks/%d/checks' % ids['ui'])[0] == 404
        assert decide('c1', 'maybe')[0] == 404 and decide('x1', 'pass')[0] == 404 and decide('all', 'fail')[0] == 404
        status, body = decide('c1', 'fail')
        assert status == 400 and body['error']['code'] == 'note_required', body
        status, body = decide('c9', 'pass')
        assert status == 409 and 'no item c9' in body['error']['message'], body
        status, body = decide('c1', 'pass')
        assert status == 200 and body['open'] == 2 and body['changed'][0]['state'] == 'passed', body
        assert decide('c1', 'pass')[0] == 409
        status, body = decide('c2', 'fail', {'note': 'the text is cut off'})
        assert status == 200, body
        fix = body['changed'][0]['fix_task']
        fixing = steady(owner, 'GET', '/api/tasks/%d/t/%d' % (demo, fix))[1]
        assert fixing['title'] == 'Fix: open the file' and fixing['fixes']['id'] == 'c2' and fixing['fixes']['note'] == 'the text is cut off', fixing
        status, body = decide('all', 'pass')
        assert status == 200 and body['open'] == 0 and [c['id'] for c in body['changed']] == ['c3'], body
        overview = steady(owner, 'GET', '/api/tasks/%d' % demo)[1]
        assert overview['to_check'] == 0 and '#%d failed a check; #%d fixes it' % (made[0], fix) in [
            row['text'] for row in overview['resolutions']], overview['resolutions']
        wait(lambda: task_state(fix) in ('ready', 'failed'), 'the fix task')
        js('task', 'close', fix, '--discard', '-p', 'demo')
        ok('to check: listed with titles, pass, fail needs a note and starts a linked fix task, all passed, refusals (viewer, grant, unknown id or decision)')

        # --- Refusals: the capability and the grant come from the site file only.
        for method, route, body in (('POST', '/api/tasks/%d/say' % demo, {'text': 'hi'}),
                                    ('POST', '/api/tasks/%d/t/1/add' % demo, {'text': 'hi'}),
                                    ('POST', '/api/tasks/%d/approvals/a9/approve' % demo, {})):
            status, refused = steady(viewer, method, route, body)
            assert status == 403 and refused['error']['code'] == 'capability_denied', (route, refused)
        assert steady(viewer, 'GET', '/api/tasks/%d' % demo)[0] == 200
        assert steady(viewer, 'GET', '/api/tasks/%d' % ids['ui'])[0] == 404
        assert steady(viewer, 'GET', '/api/tasks/%d/t/1' % ids['ui'])[0] == 404
        assert steady(viewer, 'GET', '/api/tasks/%d/t/1' % demo)[1]['actions'] == []
        # The brief is the ledger the manager reads; the page writes it in words.
        brief = steady(owner, 'GET', '/api/tasks/%d/brief' % demo)[1]
        assert brief['main']['branch'] == 'main' and isinstance(brief['main']['changed_files'], int), brief['main']
        assert brief['deploys']['deploy']['configured'] and not brief['deploys']['release']['configured'], brief['deploys']
        assert [row['id'] for row in brief['open']] == [row['id'] for row in js('task', 'ledger', '-p', 'demo')['open']]
        assert steady(viewer, 'GET', '/api/tasks/%d/brief' % ids['ui'])[0] == 404
        for route in ('/api/tasks', '/api/tasks/%d' % demo, '/api/tasks/%d/chat' % demo, '/api/tasks/%d/brief' % demo):
            assert steady(plain, 'GET', route)[0] == 403, route
        ok('brief: the ledger with deploys and the main checkout; refusals: task_act and task_view per site, ungranted project 404, actions hidden without task_act')

        # --- A task that waits for the owner's answer: the need, its actions, and an add that
        # resumes it. No brief and no start route are left.
        planned = ids['plans']
        view = steady(owner, 'GET', '/api/tasks/%d' % planned)[1]
        assert [(item['kind'], item['task']) for item in view['needs']] == [('waiting', 1), ('land', 2)], view['needs']
        assert view['needs'][0]['waiting']['on'] == 'owner', view['needs'][0]
        detail = steady(owner, 'GET', '/api/tasks/%d/t/1' % planned)[1]
        assert detail['actions'] == ['add', 'hand'] and detail['talk'] == [] and 'brief' not in detail and 'phase' not in detail, detail
        assert steady(owner, 'POST', '/api/tasks/%d/t/1/start' % planned, {})[0] == 404
        ok('a waiting task: its need and reason, add and hand, no brief or start')
        if args.browser:
            # A worker's report stays in the task sheet: the chat route carries the events lines only.
            entries = steady(owner, 'GET', '/api/tasks/%d/chat' % ids['ui'])[1]['entries']
            assert not any('reports' in e for e in entries), entries
            assert any(e['kind'] == 'message' and e.get('from') == 'events' and '#5 ready' in e['text'] for e in entries), entries
            # The manager's name: ui's set name, plans' default; a peer by its set name.
            ui_chat = steady(owner, 'GET', '/api/tasks/%d/chat' % ids['ui'])[1]
            plans_chat = steady(owner, 'GET', '/api/tasks/%d/chat' % ids['plans'])[1]
            assert ui_chat['manager'] == {'name': 'Atlas'} and ui_chat['peers'] == {'plans': 'plans'}, (ui_chat['manager'], ui_chat['peers'])
            assert plans_chat['manager'] == {'name': 'plans'} and plans_chat['peers'] == {'ui': 'Atlas'}, (plans_chat['manager'], plans_chat['peers'])
            args.captures.mkdir(parents=True, exist_ok=True)
            audit = Path.home() / '.claude/skills/ui-view-standards/scripts/slop-audit.js'
            browser_code = cli('web', 'pair', '--site', files['owner']).stdout.splitlines()[0].rsplit(' ', 1)[1]
            # Home's order by its rule: needs first (most urgent kind, then the oldest need of it),
            # then the last owner-facing activity, newest first (docs/web-client-tasks.md "Home"; no
            # fixture project has a message this device read to, so none is unread).
            urgency = {'waiting': 0, 'failed': 1, 'approval': 2, 'land': 3}
            def home_key(name):
                view = steady(owner, 'GET', '/api/tasks/%d' % ids[name])[1]
                if not view['needs']:
                    return (1, -(view['last_unix_ms'] or 0), 0)
                rank = min(urgency[item['kind']] for item in view['needs'])
                return (0, rank, min(item['unix_ms'] for item in view['needs'] if urgency[item['kind']] == rank))
            home_order = sorted((name for name in ids if name in ('ui', 'demo', 'plans', 'calm')), key=home_key)
            # ^ ui and plans have a waiting task, demo only tasks to land.
            assert sorted(home_order[:2]) == ['plans', 'ui'] and home_order[2:] == ['demo', 'calm'], home_order
            # ^ Projects with sessions and no task follow by their newest session (created in this order).
            home_order[-1:-1] = ['busy', 'quiet', 'gone']
            # A role in ui with a reply, for the page's Roles contact and chat.
            role_brief = work / 'role-brief.md'
            role_brief.write_text('Write the release notes.\n')
            js('task', 'role', 'new', 'billing', '--brief', role_brief, '-p', 'ui')
            assert steady(owner, 'POST', '/api/tasks/%d/say' % ids['ui'], {'text': 'STATUS', 'role': 'billing'})[0] == 200
            wait(lambda: any(e['kind'] == 'reply' for e in steady(owner, 'GET', '/api/tasks/%d/chat?role=billing' % ids['ui'])[1]['entries']),
                 'the ui role reply', 60)
            # The page asks for a server with another build by writing this file: the owner's
            # server restarts under another site name, which is part of the page files' digest.
            restart = work / 'restart-owner'

            def restart_owner():
                while not restart.exists():
                    if servers['owner'].poll() is not None:
                        return
                    time.sleep(0.2)
                stop(servers['owner'])
                files['owner'].write_text(files['owner'].read_text().replace('name = "owner"', 'name = "owner next"'))
                servers['owner'] = start(work, [str(wsx), 'web', 'serve', '--site', str(files['owner'])], env=env, cwd=project,
                                         stdout=subprocess.DEVNULL, stderr=(work / 'owner.log').open('a'))
                wait(lambda: _up(owner), 'restarted web server')
                (work / 'restart-owner.done').write_text('done')
            restarter = threading.Thread(target=restart_owner, daemon=True)
            restarter.start()
            # ^ Settings saves the browser's subscription: the local receiver's endpoint and keys.
            os.environ['WSX_PUSH_FIXTURE'] = json.dumps({'endpoint': push_origin + '/push/browser',
                                                         'keys': {'p256dh': receiver['p256dh'], 'auth': receiver['auth']}})
            Relay(ports['relay'], ports['owner'], work / 'relay')
            result = check(work, [node, str(ROOT / 'scripts/web-tasks-check.mjs'), owner.origin + '/', browser_code,
                                  str(ids['ui']), str(args.captures), ','.join(home_order),
                                  str(audit) if audit.exists() else '', str(ids['plans']), str(restart),
                                  str(plans), LONG_NAME, str(work / 'relay')], timeout=480)
            assert result.returncode == 0, result.stdout + result.stderr
            # ^ Try started #7's preview: its mapping goes with it.
            assert js('task', 'preview', 7, '--stop', '-p', 'ui')['stopped'] is True
            assert 'serve --bg --https=' in (work / 'tailscale.log').read_text() and ' off' in (work / 'tailscale.log').read_text()
            # ^ The server restarted: the event stream starts again for the checks after.
            assert (work / 'restart-owner.done').exists()
            events = Events(owner)
            events.expect('workspace')
            owner_name = 'owner next'
            ok('real browser, ' + result.stdout.strip().splitlines()[-1])
            # ^ The page only read the plain worktree: the same git entry, branch head and files.
            assert plain_git() == plain_before, (plain_git(), plain_before)
            kept = next(item for item in call('snapshot', None)['data']['sessions'] if item['id'] == plain_session)
            assert kept['label'] == 'old chat' and kept['worktree_id'] == ui_worktrees['ui-old']['id'], kept
            ok('plain worktree: its git worktree entry, branch head, status and session are unchanged after the page')
            # A capture the manager's sweep removed (docs/tasks.md "To check"): the task's item says
            # so, and its route answers 404 while the poster is still served.
            captures = Path(js('task', 'show', 8, '-p', 'ui')['log']).parent / 'captures'
            (captures / 'r1-1.mp4').unlink()
            (video_item,) = steady(owner, 'GET', '/api/tasks/%d/t/8' % ids['ui'])[1]['to_check']
            assert video_item.get('capture_removed') is True and video_item['capture'] == 'captures/r1-1.mp4', video_item
            assert steady(owner, 'GET', '/api/tasks/%d/checks/%s/capture' % (ids['ui'], video_item['id']))[0] == 404
            assert steady(owner, 'GET', '/api/tasks/%d/checks/%s/poster' % (ids['ui'], video_item['id']))[0] == 200
            ok('a removed capture: capture_removed on the task\'s item, its route 404, the poster still served')

        # --- "Manager needs me": one push per device naming every open item, replacing the one
        # before; ready, failed and landed wait for the daily digest (docs/web-client.md "Notifications").
        def pushes(kind, path='/push/tasks'):
            file = work / 'push.jsonl'
            records = [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []
            return [record for record in records if record['path'] == path and record['payload'].get('kind') == kind]
        keys = {'p256dh': receiver['p256dh'], 'auth': receiver['auth']}
        subscription = {'endpoint': push_origin + '/push/tasks', 'keys': keys}
        refused = steady(plain, 'POST', '/api/push', {'subscription': dict(subscription, endpoint=push_origin + '/push/plain'), 'choices': {'manager': True}})
        assert refused[0] == 403, refused
        # The digest at a time far off for now; it is moved to the next minute below.
        later = (time.gmtime().tm_hour * 60 + time.gmtime().tm_min + 180) % 1440
        choices = {'manager': True, 'agents': False, 'digest': {'minute': later, 'utc_offset_minutes': 0}}
        status, body = steady(owner, 'POST', '/api/push', {'subscription': subscription, 'choices': choices})
        assert status == 200 and body['choices'] == choices and body['muted'] == [], body
        subscribed_ms = time.time() * 1000
        time.sleep(4)  # items from the journey above went out to nobody
        baselines = {'manager': len(pushes('manager')), 'digest': len(pushes('digest'))}

        def new_pushes(kind, label, timeout=30):
            got = wait(lambda: len(pushes(kind)) > baselines[kind] and pushes(kind)[baselines[kind]:], label, timeout=timeout)
            for record in got:
                assert record.get('error') is None and record['vapid']['ok'], record
            baselines[kind] += len(got)
            return [record['payload'] for record in got]

        def line(n, what, project='demo'):
            row = next(row for row in steady(owner, 'GET', '/api/tasks/%d' % ids[project])[1]['tasks'] if row['id'] == n)
            what = 'needs you: ' + row['waiting']['reason'] if what == 'asks' else what
            title = row['title'] if len(row['title']) <= 60 else row['title'][:60] + '…'  # cut as the push cuts it
            return '#%d %s: %s' % (n, title, what)

        def manager_push(wrote=None):
            """The push the overviews the page shows call for: every waiting task, approval and To check count,
            and `wrote`'s line when that project's manager wrote to the owner on its own."""
            names = {value: key for key, value in ids.items()}
            projects = []
            for overview in sorted(steady(owner, 'GET', '/api/tasks')[1], key=lambda overview: overview['project']):
                project, items = overview['project'], []
                for need in overview['needs']:
                    task_url = '#/p/%d/t/%d' % (project, need['task']) if need.get('task') else '#/p/%d' % project
                    if need['kind'] == 'waiting':
                        items.append((1, line(need['task'], 'asks', names[project]), task_url))
                    elif need['kind'] == 'approval':
                        items.append((1, need['label'], task_url))
                if overview['to_check']:
                    items.append((overview['to_check'], '%d to check' % overview['to_check'], '#/p/%d/checks' % project))
                if names[project] == wrote:
                    items.append((1, overview['manager'] + ' wrote to you', '#/p/%d' % project))
                if items:
                    projects.append((project, names[project], items))
            count = sum(item[0] for _, _, items in projects for item in items)
            every = [item for _, _, items in projects for item in items]
            return {
                'kind': 'manager', 'tag': 'wsx-owner-manager',
                'title': '%d %s: %s' % (count, 'item needs you' if count == 1 else 'items need you', ', '.join(name for _, name, _ in projects)),
                'body': '\n'.join((name + ': ' if len(projects) > 1 else '') + item[1] for _, name, items in projects for item in items),
                'url': every[0][2] if len(every) == 1 else '#/p/%d' % projects[0][0] if len(projects) == 1 else '#/',
                'badge': sum(len(overview['needs']) for overview in steady(owner, 'GET', '/api/tasks')[1]),
            }

        first = js('task', 'new', 'NEEDS Bash(curl:*) NOOP', '-p', 'demo')['id']
        second = js('task', 'new', 'NEEDS Bash(curl:*) NOOP', '-p', 'demo')['id']
        wait_state(first, 'waiting')
        wait_state(second, 'waiting')
        merged = new_pushes('manager', 'waiting push')
        time.sleep(4)
        assert len(merged) == 1 and len(pushes('manager')) == baselines['manager'], pushes('manager')[-2:]
        assert merged[0] == manager_push(), (merged[0], manager_push())
        def says(payload, entry, project='demo'):
            """The line is in the body, after its project's name when several projects have items."""
            lines = payload['body'].split('\n')
            return entry in lines or project + ': ' + entry in lines
        assert says(merged[0], line(first, 'asks')) and says(merged[0], line(second, 'asks')), merged
        # Ready and failed wait for the digest.
        for n in (first, second):
            assert act(n, 'add', {'text': 'NOOP'})[0] == 200
        wait_state(first, 'ready')
        wait_state(second, 'ready')
        broken = js('task', 'new', 'WORK broken.txt', '-p', 'demo')['id']
        wait_state(broken, 'failed')
        # A landing adds a To check item: the push replaces the one before and counts it.
        checked = js('task', 'new', 'WORK j.txt HV', '-p', 'demo')['id']
        wait_state(checked, 'ready')
        assert act(checked, 'land', {})[0] == 200
        added = new_pushes('manager', 'to check push')
        assert len(added) == 1 and added[0] == manager_push(), (added, manager_push())
        assert added[0]['tag'] == merged[0]['tag'] and says(added[0], '%d to check' % steady(owner, 'GET', '/api/tasks/%d' % demo)[1]['to_check']), added
        # A page of this device shows demo: no push for a new demo item, a push for one in ui.
        page = 'page-owner-1'
        assert steady(owner, 'POST', '/api/presence', {'page': page, 'project': demo})[0] == 404  # no stream yet
        assert steady(owner, 'POST', '/api/presence', {'page': 'x', 'project': demo})[0] == 400
        shown = Events(owner, page=page)
        shown.expect('workspace')
        assert steady(owner, 'POST', '/api/presence', {'page': page, 'project': demo})[0] == 200
        assert steady(viewer, 'POST', '/api/presence', {'page': page, 'project': None})[0] == 404
        hidden = js('task', 'new', 'NEEDS Bash(curl:*) NOOP', '-p', 'demo')['id']
        wait_state(hidden, 'waiting')
        time.sleep(5)
        assert len(pushes('manager')) == baselines['manager'], 'a push for the project the page shows'
        other = js('task', 'new', 'NEEDS Bash(curl:*) NOOP', '-p', 'ui')['id']
        wait(lambda: js('task', 'show', other, '-p', 'ui')['state'] == 'waiting', 'ui waiting')
        elsewhere = new_pushes('manager', 'push for the other project')
        assert len(elsewhere) == 1 and elsewhere[0] == manager_push(), (elsewhere, manager_push())
        assert says(elsewhere[0], line(other, 'asks', 'ui'), 'ui') and says(elsewhere[0], line(hidden, 'asks')), elsewhere
        # The page goes away: its presence goes with its stream.
        shown.connection.close()
        time.sleep(1)
        gone = js('task', 'new', 'NEEDS Bash(curl:*) NOOP', '-p', 'demo')['id']
        wait_state(gone, 'waiting')
        assert says(new_pushes('manager', 'push after the page closed')[0], line(gone, 'asks'))
        # An approval request reads as its action, never as its id.
        js('task', 'say', 'RUN wsx task land %d -p demo --json' % first, '-p', 'demo')
        asked = new_pushes('manager', 'approval push')
        assert asked[0] == manager_push() and says(asked[0], 'Approve land #%d' % first), asked
        # A reply the manager starts on its own, a table to a task's event, pushes; its reply to
        # the owner does not, and neither adds to the badge.
        talk = lambda: [e for e in js('task', 'chat', '-p', 'demo')['entries'] if e['kind'] == 'reply']
        replies = len(talk())
        js('task', 'say', 'TABLE', '-p', 'demo')
        wait(lambda: len(talk()) > replies and 'Where things stand' in talk()[-1]['text'], 'the reply to the owner')
        time.sleep(5)
        assert len(pushes('manager')) == baselines['manager'], 'a push for a reply to the owner'
        tabled = js('task', 'new', 'NOOP TABLE', '-p', 'demo')['id']
        wait_state(tabled, 'ready')
        own = new_pushes('manager', 'the push of a reply the manager started')
        assert len(own) == 1 and own[0] == manager_push(wrote='demo'), (own, manager_push(wrote='demo'))
        # ^ manager_push()'s badge counts the overviews' needs only.
        assert says(own[0], 'demo wrote to you'), own
        # ^ A role's reply it started on its own pushes the same way, named by the role and
        # opening its chat (docs/web-client-tasks.md "Roles").
        js('task', 'say', '--role', 'notes', 'RUN wsx task new "NOOP TABLE" -p demo --json', '-p', 'demo')
        roled = new_pushes('manager', 'the push of a reply a role started', timeout=60)
        assert len(roled) == 1 and says(roled[0], 'notes wrote to you'), roled
        ok('manager push: waiting merged in one, a new To check item, an approval, a reply the manager started (not one to the owner); each names every open item and replaces the one before; ready and failed do not push; the shown project skipped for that device; presence ends with the stream; manager needs task_view')

        # The daily digest, moved to the next minute: the window since the switch was turned on
        # stays, so it lists what became ready, failed or landed since, each in its latest state.
        now = time.gmtime()
        if now.tm_sec > 50:
            time.sleep(61 - now.tm_sec)
            now = time.gmtime()
        choices['digest'] = {'minute': (now.tm_hour * 60 + now.tm_min + 1) % 1440, 'utc_offset_minutes': 0}
        assert steady(owner, 'POST', '/api/push', {'subscription': subscription, 'choices': choices})[1]['choices'] == choices
        digest = new_pushes('digest', 'daily digest', timeout=90)
        assert len(digest) == 1 and digest[0]['title'] == owner_name + ': daily digest' and digest[0]['tag'] == 'wsx-owner-digest' and digest[0]['url'] == '#/', digest
        told = digest[0]['body'].split('\n')
        prefix = 'demo: ' if any(entry.startswith('demo: ') for entry in told) else ''
        for expected in (line(first, 'ready for review'), line(second, 'ready for review'),
                         line(broken, 'failed: test ! -e broken.txt failed'), line(checked, 'landed')):
            assert prefix + expected in told, (expected, told)
        # Each task once; nothing from before the switch, nothing that waits.
        before = [row['id'] for row in steady(owner, 'GET', '/api/tasks/%d' % demo)[1]['tasks'] if row['updated_unix_ms'] <= subscribed_ms]
        assert not any(entry.startswith(prefix + '#%d ' % n) for n in before + [hidden, gone] for entry in told), told
        time.sleep(6)
        assert len(pushes('digest')) == baselines['digest'], 'a second digest the same day'
        ok('daily digest: at the minute the device chose, once a day, ready, failed and landed since the last one in their latest state; a new time keeps the window')

        # Earlier topics are read once and map to the switches; the server keeps no topic list.
        viewer_device = steady(viewer, 'GET', '/api/session')[1]['device']['id']
        stop(servers['viewer'])
        stored = state / 'wsx/web/viewer/push.json'
        stored.write_text(json.dumps([{'device_id': viewer_device, 'endpoint': push_origin + '/push/old', 'keys': keys,
                                       'topics': ['blocked', 'done', 'tasks'], 'created_unix': int(time.time())}]))
        servers['viewer'] = start(work, [str(wsx), 'web', 'serve', '--site', str(files['viewer'])], env=env, cwd=project,
                                  stdout=subprocess.DEVNULL, stderr=(work / 'viewer.log').open('a'))
        wait(lambda: _up(viewer), 'restarted viewer server')
        offset = time.localtime().tm_gmtoff // 60
        migrated = steady(viewer, 'GET', '/api/push')[1]
        assert migrated['choices'] == {'manager': True, 'agents': True, 'digest': {'minute': 540, 'utc_offset_minutes': offset}}, migrated
        assert 'topics' not in stored.read_text() and json.loads(stored.read_text())[0]['choices']['manager'] is True
        assert steady(viewer, 'DELETE', '/api/push', {})[1]['choices'] == {'manager': False, 'agents': False, 'digest': None}
        assert json.loads(stored.read_text()) == []
        ok('old topics: blocked, done and tasks read as an agent needs me, manager needs me and a 09:00 digest; stored without topics')
        success = True
    finally:
        for process in reversed(STARTED):
            stop(process)
        if env is not None:
            subprocess.run([str(wsx), 'daemon', 'stop', '--force'], env=env, capture_output=True, timeout=20)
        if success:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print('diagnostics retained at ' + str(work), file=sys.stderr)
    print(json.dumps({'result': 'PASS', 'part': 'tasks', 'seconds': round(time.monotonic() - started, 2),
                      'model_calls': 0, 'slowest': slowest(), 'checks': checks}, indent=1))


def _up(site):
    try:
        return site.request('GET', '/manifest.webmanifest')[0] == 200
    except OSError:
        return False



if __name__ == '__main__':
    main()
