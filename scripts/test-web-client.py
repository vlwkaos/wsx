#!/usr/bin/env python3
"""Model-free journey for the hosted web client (docs/web-client.md).

Real wsxd, real PTY fixture agents, three `wsx web serve` processes and a local
Web Push receiver. Nothing here touches the user's daemon, Tailscale or network.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]

# A fixture agent: reports as Claude, records what the PTY delivered, and writes
# its own native transcript. The web server may only learn replies from that file.
ACTOR = r'''
import json, os, pathlib, signal, socket, sys, time, tty
signal.signal(signal.SIGHUP, signal.SIG_IGN)
history, trace, project = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])
pane = int(os.environ['WSX_PANE_ID']); generation = os.environ['WSX_RUNTIME_GENERATION']
trace.with_suffix('.pid').write_text(str(os.getpid()))
tty.setraw(0)
draft, pending, leaf, sequence = '', b'', None, 0
def report(state):
 with socket.socket(socket.AF_UNIX) as client:
  client.connect(os.environ['WSX_SOCKET']); stream = client.makefile('rb')
  client.sendall(b'{"method":"hello","params":{"protocol":16}}\n'); stream.readline()
  params = {'pane_id':pane,'runtime_generation':generation,'provider':'claude','state':state,
    'session_ref':{'kind':'id','value':'fixture','transcript_path':str(history)},
    'capabilities':{'prompt':True,'lifecycle':True}}
  client.sendall(json.dumps({'method':'agent_report','params':params}).encode()+b'\n')
  assert json.loads(stream.readline())['type'] == 'ack'
def paint():
 rows = os.get_terminal_size().lines
 sys.stdout.write('\x1b[?2004h\x1b]2;✳ fixture\x07\x1b[2J\x1b['+str(max(1,rows-2))+';1H❯ '+draft+'\x1b[J')
 sys.stdout.flush()
def record(kind, text):
 global leaf, sequence
 sequence += 1; entry = kind+'-'+str(sequence)
 with history.open('a') as out:
  out.write(json.dumps({'type':kind,'uuid':entry,'parentUuid':leaf,'sessionId':'fixture',
    'message':{'role':kind,'content':text}})+'\n')
 leaf = entry
def turn(text):
 with trace.open('a') as out: out.write(json.dumps({'received':text})+'\n')
 record('user', text)
 report('working')
 body = text.split('\n',1)[1] if text.startswith('[wsx exchange ') else text
 if 'make chart' in body:
  (project/'out').mkdir(exist_ok=True)
  (project/'out'/'chart.png').write_bytes(pathlib.Path(sys.argv[4]).read_bytes())
  record('assistant', 'Saved the chart to `out/chart.png`.')
 elif 'leak' in body:
  record('assistant', 'See out/leak.png and ../outside.png and '+sys.argv[5]+' and .env and out/api-token.txt and .git/config.')
 elif 'ask me' in body:
  report('blocked'); return
 elif 'fail' in body:
  report('error'); return
 else:
  record('assistant', 'echo: '+body.splitlines()[0])
 time.sleep(0.2)
 report('done')
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
   text, draft = draft, ''; paint(); turn(text)
  elif char == b'\x03':
   report('idle')
  else:
   draft += char.decode(errors='replace'); paint()
'''


def png(size, shade):
    """A valid, deterministic PNG of roughly size*size*3 raw bytes."""
    raw = b''.join(b'\x00' + bytes((shade + x * 7 + y * 13) % 256 for x in range(size * 3)) for y in range(size))
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw, 0)) + chunk(b'IEND', b''))


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


class Site:
    """HTTP client for one served site. `shape` picks the inbound path form."""

    def __init__(self, port, base):
        self.port, self.base, self.cookie = port, base, None
        self.origin = 'http://127.0.0.1:%d' % port

    def request(self, method, path, body=None, headers=None, shape='prefixed', cookie=True, raw=False):
        prefix = self.base if shape == 'prefixed' else ''
        sent = {'Origin': self.origin} if method not in ('GET', 'HEAD') else {}
        if cookie and self.cookie:
            sent['Cookie'] = self.cookie
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
            sent['Content-Type'] = 'application/json'
        sent.update(headers or {})
        sent = {name: value for name, value in sent.items() if value is not None}
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=10)
        try:
            connection.request(method, prefix + path, body=body, headers=sent)
            response = connection.getresponse()
            data = response.read()
        finally:
            connection.close()
        if raw:
            return response.status, dict((k.lower(), v) for k, v in response.getheaders()), data
        return response.status, (json.loads(data) if data[:1] in (b'{', b'[') else data)


class Events(threading.Thread):
    """The page's one server-sent event stream, read like a browser would."""

    def __init__(self, site, shape='prefixed'):
        super().__init__(daemon=True)
        self.seen, self.ready, self.closed = [], threading.Condition(), threading.Event()
        self.connection = http.client.HTTPConnection('127.0.0.1', site.port, timeout=60)
        self.connection.request('GET', (site.base if shape == 'prefixed' else '') + '/api/events',
                                headers={'Cookie': site.cookie, 'Accept': 'text/event-stream'})
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT / 'target/debug/wsx')
    parser.add_argument('--daemon', type=Path, default=ROOT / 'target/debug/wsxd')
    parser.add_argument('--browser', action='store_true',
                        help='also run scripts/web-browser-check.mjs with an installed Chrome')
    parser.add_argument('--captures', type=Path, default=ROOT / '.work/web-captures')
    args = parser.parse_args()
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    if not wsx.is_file() or not wsxd.is_file() or wsx.parent != wsxd.parent:
        raise RuntimeError('build adjacent wsx/wsxd first')
    node = shutil.which('node')
    assert node, 'Node is required for the independent Web Push receiver'
    # Short path: the Unix socket must fit macOS SUN_LEN.
    work = ROOT / '.work' / ('web-' + str(os.getpid()))
    work.parent.mkdir(exist_ok=True)
    work.mkdir(mode=0o700)
    processes, streams, success, started, path = [], [], False, time.monotonic(), None
    checks = []

    def ok(label):
        checks.append(label)

    try:
        for name in ('home', 'state', 'project', 'project2'):
            (work / name).mkdir(mode=0o700)
        config = (work / 'home/Library/Application Support/wsx' if sys.platform == 'darwin' else work / 'config/wsx')
        config.mkdir(parents=True)
        (config / 'config-v2.toml').write_text('resume_agents_on_restore = false\n')
        path = work / 'state/wsx/wsx.sock'
        env = {'HOME': str(work / 'home'), 'XDG_CONFIG_HOME': str(work / 'config'), 'XDG_STATE_HOME': str(work / 'state'),
               'XDG_CACHE_HOME': str(work / 'cache'), 'WSX_SOCKET': str(path), 'WSX_DAEMON_BIN': str(wsxd),
               'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'SHELL': '/bin/sh', 'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8'}
        for name in ('project', 'project2'):
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
        actor.write_text(ACTOR)

        def call(method, params=None):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(5)
                client.connect(str(path))
                stream = client.makefile('rb')
                client.sendall(b'{"method":"hello","params":{"protocol":16}}\n')
                stream.readline()
                request = {'method': method}
                if params is not None:
                    request['params'] = params
                client.sendall(json.dumps(request).encode() + b'\n')
                return json.loads(stream.readline())

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
            return [json.loads(line)['received'] for line in file.read_text().splitlines()] if file.exists() else []

        def settle(name):
            """Wait until the fixture agent can take the next message."""
            def state():
                snapshot = call('snapshot')['data']
                pane = next(s['focused_pane'] for s in snapshot['sessions'] if s['id'] == sessions[name])
                return next(p for p in snapshot['panes'] if p['id'] == pane)['agent']['state'] in ('idle', 'done')
            wait(state, name + ' agent ready')

        daemon = subprocess.Popen([str(wsxd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        processes.append(daemon)
        wait(path.exists, 'daemon')
        assert call('synchronize_projects', {'projects': [
            {'name': name, 'path': str(work / name), 'worktrees': [{'path': str(work / name), 'branch': 'main'}]}
            for name in ('project', 'project2')]})['type'] == 'ack'
        worktrees = {Path(item['path']).name: item['id'] for item in call('snapshot')['data']['worktrees']}
        sessions = {}
        for name, tree in (('owner', 'project'), ('family', 'project2')):
            created = call('session_create', {'worktree_id': worktrees[tree], 'label': name + ' chat',
                'command': [sys.executable, str(actor), str(work / (name + '.jsonl')), str(work / (name + '.trace')),
                            str(work / tree), str(chart), str(outside)], 'rows': 12, 'cols': 100})
            assert created['type'] == 'created', created
            sessions[name] = created['data']['id']
        wait(lambda: all(pane['agent'] for pane in call('snapshot')['data']['panes']), 'agent reports')

        push = subprocess.Popen([node, str(ROOT / 'scripts/web-push-fixture.mjs'), str(work / 'push.jsonl')],
                                stdout=subprocess.PIPE, text=True)
        processes.append(push)
        receiver = json.loads(push.stdout.readline())
        push_origin = 'http://127.0.0.1:%d' % receiver['port']

        def pushes():
            file = work / 'push.jsonl'
            return [json.loads(line) for line in file.read_text().splitlines()] if file.exists() else []

        vapid = cli('web', 'vapid').stdout
        private_key = next(line.split('"')[1] for line in vapid.splitlines() if line.startswith('private_key'))
        ports = {name: free_port() for name in ('owner', 'family', 'root')}

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
panels = ["chat", "files", "terminal", "activity"]
[theme]
accent = "#225577"
[project]
path = "{project}"
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
        files = {
            'owner': site_file('owner', owner_text),
            # Site B: same binary, fewer panels and capability bits, another project.
            'family': site_file('family', f'''id = "family"
name = "Family"
base_path = "/app/family"
port = {ports['family']}
public_origin = "http://127.0.0.1:{ports['family']}"
panels = ["chat"]
[project]
path = "{(work / 'project2').resolve()}"
capabilities = ["chat_send", "media_upload", "media_download"]
'''),
            # Alt-port root shape, with small media limits for TTL and quota.
            'root': site_file('root', f'''id = "root"
name = "Root"
port = {ports['root']}
public_origin = "http://127.0.0.1:{ports['root']}"
panels = ["chat"]
[project]
path = "{project}"
capabilities = ["chat_send", "session_start", "media_upload", "media_download"]
agent_command = ["/bin/sh"]
[media]
max_file_bytes = 4096
quota_bytes = 9000
ttl_seconds = 4
'''),
        }
        # The site file is a trust boundary: wrong mode and unknown fields are refused.
        refused = cli('web', 'serve', '--site', site_file('open', owner_text, 0o644), expect=1)
        assert 'owner-only' in refused.stderr, refused.stderr
        refused = cli('web', 'serve', '--site', site_file('typo', owner_text + 'bootstrap_token = "x"\n'), expect=1)
        assert 'unknown field' in refused.stderr, refused.stderr
        refused = cli('web', 'serve', '--site', site_file('caps', owner_text.replace('"files", ', '')), expect=1)
        assert 'needs the Files panel' in refused.stderr, refused.stderr
        ok('site file: mode, unknown field and panel/capability mismatch refused')

        sites = {}
        for name in files:
            process = subprocess.Popen([str(wsx), 'web', 'serve', '--site', str(files[name])], env=env,
                                       cwd=project, stdout=subprocess.DEVNULL, stderr=(work / (name + '.log')).open('w'))
            processes.append(process)
            sites[name] = Site(ports[name], {'owner': '/app/owner', 'family': '/app/family', 'root': ''}[name])

        def up(site):
            try:
                return site.request('GET', '/manifest.webmanifest', shape='stripped')[0] == 200
            except OSError:
                return False
        for site in sites.values():
            wait(lambda: up(site), 'web server')
        owner, family, root = sites['owner'], sites['family'], sites['root']

        # --- Sub-path first: both inbound shapes serve the same base-filled documents.
        for shape in ('prefixed', 'stripped'):
            status, headers, html = owner.request('GET', '/', shape=shape, raw=True)
            assert status == 200 and b'<base href="/app/owner/">' in html, html[:200]
            assert b'Owner &lt;wsx&gt;' in html and b'<wsx>' not in html
            assert "script-src 'self'" in headers['content-security-policy'] and 'unsafe' not in headers['content-security-policy']
            assert headers['x-content-type-options'] == 'nosniff' and headers['x-frame-options'] == 'DENY'
            status, manifest = owner.request('GET', '/manifest.webmanifest', shape=shape)
            assert status == 200 and manifest['id'] == manifest['scope'] == manifest['start_url'] == '/app/owner/'
            assert [icon['sizes'] for icon in manifest['icons']] == ['192x192', '512x512']
            status, headers, worker = owner.request('GET', '/sw.js', shape=shape, raw=True)
            assert status == 200 and headers['content-type'].startswith('text/javascript')
            assert b'"assets/main.js"' in worker and b'__WSX_' not in worker
            for asset, kind in (('/assets/main.js', 'text/javascript'), ('/assets/app.css', 'text/css'),
                                ('/theme.css', 'text/css'), ('/icon-192.png', 'image/png'), ('/icon-512.png', 'image/png')):
                status, headers, body = owner.request('GET', asset, shape=shape, raw=True)
                assert status == 200 and headers['content-type'].startswith(kind), asset
            assert body[16:24] == struct.pack('>II', 512, 512)
        status, body = owner.request('GET', '/theme.css')
        assert b'--accent:#225577' in body
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
        ok('HTML, manifest, service worker, assets: prefixed, stripped and root shapes')

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
            assert session['site']['panels'] == ['chat', 'files', 'terminal', 'activity']
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
        assert 'Path=/;' in cookie
        root.cookie = cookie.split(';')[0]
        listed = json.loads(cli('web', 'devices', '--site', files['owner'], '--json').stdout)
        assert [device['name'] for device in listed] == ['owner phone'] and 'secret_sha256' not in listed[0]
        ok('pairing: wrong code, one-time code, hashed store, cookie path per site, forged cookies refused')

        # --- One event stream per page; chat round trip through the exchange broker.
        events = Events(owner)
        streams.append(events)
        first = events.expect('workspace')
        assert [item['id'] for item in first['sessions']] == [sessions['owner']], first
        assert first['sessions'][0]['agent']['provider'] == 'claude'
        chat = '/api/chat/%d' % sessions['owner']
        status, history = owner.request('GET', chat + '/history')
        assert status == 200 and history['messages'] == [] and history['exchange'] is None
        status, exchange = owner.request('POST', chat + '/send', {'text': 'hello from the phone\r\nsecond line'})
        assert status == 200, exchange
        # Truthful evidence: a PTY write is reported as a PTY write, never as acceptance.
        assert exchange['evidence'] == 'pty_delivery' and 'not confirmed' in exchange['delivery'], exchange
        assert exchange['access'] == 'writer'
        received = wait(lambda: traces('owner'), 'PTY delivery')[0]
        assert received == '[wsx exchange %d, round 1, writer]\nhello from the phone\nsecond line' % exchange['id'], received
        events.expect('history', lambda data: data['session_id'] == sessions['owner'])
        events.expect('exchange', lambda data: data['state'] == 'done_observed')

        def messages(site, session):
            return site.request('GET', '/api/chat/%d/history' % session)[1]

        history = wait(lambda: (lambda data: data if len(data['messages']) == 2 else None)(messages(owner, sessions['owner'])), 'history')
        assert [(m['role'], m['text']) for m in history['messages']] == [
            ('user', 'hello from the phone\nsecond line'), ('assistant', 'echo: hello from the phone')]
        assert history['source'] == 'provider_native_persisted_history'
        # A done pane is lifecycle evidence only; it must not be shown as confirmed.
        assert history['exchange']['evidence'] == 'pane_lifecycle' and 'not confirmed' in history['exchange']['delivery']
        # The reply exists only in the native transcript, never on the fixture's screen.
        assert 'echo:' not in cli('session', 'peek', sessions['owner'], '--trim').stdout
        settle('owner')
        status, exchange2 = owner.request('POST', chat + '/send', {'text': 'follow up'}, shape='stripped')
        assert status == 200 and exchange2['id'] == exchange['id'] and exchange2['round'] == 2
        wait(lambda: len(traces('owner')) == 2, 'continued delivery')
        wait(lambda: len(messages(owner, sessions['owner'])['messages']) == 4, 'continued history')
        assert owner.request('POST', chat + '/send', {'text': '   '})[0] == 400
        assert owner.request('POST', chat + '/send', {'text': 'x' * 70000})[0] == 400
        assert owner.request('POST', '/api/chat/999999/send', {'text': 'nope'})[0] == 404
        ok('SSE stream and chat: delivery evidence, native history, continuation, bad input')

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
        assert not any(item['name'] in ('.env', 'api-token.txt', 'config') for item in
                       (work / 'state/wsx/web/owner/media/index.json').exists() and json.loads((work / 'state/wsx/web/owner/media/index.json').read_text()) or [])
        ok('media: upload, content hash URL, immutable cache, range, sniff allowlist, prompt attachment, agent file, no symlink or traversal')

        # --- TTL and quota on the root-shaped site (max 4096 B, quota 9000 B, TTL 4 s).
        blobs = [png(33 + index, index) for index in range(3)]
        assert all(3000 < len(blob) <= 4096 for blob in blobs), [len(blob) for blob in blobs]
        urls = []
        for index, blob in enumerate(blobs):
            status, entry = root.request('PUT', '/api/media?name=%d.png' % index, blob)
            assert status == 200, entry
            urls.append('/' + entry['url'])
            time.sleep(1.05)  # distinct creation seconds, so "oldest" is well defined
        media_dir = work / 'state/wsx/web/root/media'
        assert root.request('GET', urls[0])[0] == 404, 'oldest entry was not evicted at quota'
        assert root.request('GET', urls[1])[1] == blobs[1] and root.request('GET', urls[2])[1] == blobs[2]
        assert sorted(p.suffix for p in media_dir.iterdir()) == ['.json', '.png', '.png']
        max_age = int(root.request('GET', urls[2], raw=True)[1]['cache-control'].split('max-age=')[1].split(',')[0])
        assert 0 < max_age <= 4
        time.sleep(4.2)
        assert root.request('GET', urls[1])[0] == 404 and root.request('GET', urls[2])[0] == 404
        status, entry = root.request('PUT', '/api/media?name=late.png', png(9, 99))
        assert status == 200 and [p.name for p in media_dir.glob('*.png')] == [entry['url'].split('/')[1]]
        ok('media: oldest-first quota eviction, TTL expiry, sweep removes files')

        # --- Files, terminal and activity panels on the owner site.
        status, listing = owner.request('GET', '/api/files?path=')
        # .git is never listed, read or written: its config can run commands on the host.
        assert [(e['name'], e['directory']) for e in listing['entries']] == [('out', True), ('sub', True), ('.env', False), ('notes.txt', False)]
        assert 'escape' not in [e['name'] for e in listing['entries']]  # symlinks are not listed
        assert owner.request('GET', '/api/file?path=sub/deep.txt', shape='stripped')[1]['text'] == 'deep\n'
        for bad, status in (('../outside.png', 400), ('/etc/hosts', 400), ('sub/../../x', 400), ('.git/config', 400),
                            ('escape/.codex', 403), ('out/leak.png', 403), ('missing.txt', 404)):
            assert owner.request('GET', '/api/file?path=' + bad)[0] == status, bad
        assert owner.request('GET', '/api/files?path=escape')[0] == 403
        write = {'path': 'notes.txt', 'original': 'first line\n', 'content': 'edited\n'}
        assert owner.request('PUT', '/api/file', write)[0] == 200 and (project / 'notes.txt').read_text() == 'edited\n'
        assert owner.request('PUT', '/api/file', write)[1]['error']['code'] == 'file_changed'
        assert owner.request('PUT', '/api/file', dict(write, path='out/leak.png'))[0] == 403
        git_config = (project / '.git/config').read_text()
        assert owner.request('PUT', '/api/file', {'path': '.git/config', 'original': git_config, 'content': 'x'})[0] == 400
        assert (project / '.git/config').read_text() == git_config
        assert owner.request('GET', '/api/files?path=.git')[0] == 400
        assert outside.read_bytes() == png(8, 200)
        status, screen = owner.request('GET', '/api/terminal/%d' % sessions['owner'])
        assert status == 200 and '❯' in screen['text'], screen
        settle('owner')
        assert owner.request('POST', '/api/terminal/%d/input' % sessions['owner'], {'text': 'typed in terminal', 'key': 'enter'})[0] == 200
        wait(lambda: traces('owner')[-1] == 'typed in terminal', 'terminal input')
        # Let this turn finish here; its done transition must not reach the push checks below.
        wait(lambda: messages(owner, sessions['owner'])['messages'][-1]['text'] == 'echo: typed in terminal', 'terminal turn')
        settle('owner')
        assert owner.request('POST', '/api/terminal/%d/input' % sessions['owner'], {'text': 'a\x1bb'})[0] == 400
        status, activity = owner.request('GET', '/api/activity')
        assert [s['id'] for s in activity['sessions']] == [sessions['owner']] and activity['exchanges'][0]['id'] == exchange['id']
        # A site without agent_write does not continue the owner's writer exchange.
        status, other = root.request('POST', chat + '/send', {'text': 'from the read-only site'})
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
        ok('files sandbox without .git, terminal snapshot and input, activity, access-bound continuation, stream slots released')

        # --- Site B: same binary, fewer bits. Every missing bit is a server refusal.
        status, session = family.request('GET', '/api/session')
        assert session['site']['panels'] == ['chat'] and 'agent_write' not in session['site']['capabilities']
        family_events = Events(family, shape='stripped')
        streams.append(family_events)
        assert [s['id'] for s in family_events.expect('workspace')['sessions']] == [sessions['family']]
        for method, target, body in (('GET', '/api/files?path=', None), ('GET', '/api/file?path=notes.txt', None),
                                     ('PUT', '/api/file', write), ('GET', '/api/terminal/%d' % sessions['family'], None),
                                     ('POST', '/api/terminal/%d/input' % sessions['family'], {'key': 'enter'}),
                                     ('GET', '/api/activity', None), ('GET', '/api/push', None),
                                     ('POST', '/api/sessions', {'label': 'new'})):
            status, body = family.request(method, target, body)
            assert status == 403 and body['error']['code'] == 'capability_denied', (target, status, body)
        # The other project's session does not exist for this site, in any route.
        for target in ('/api/chat/%d/history', '/api/chat/%d/send', '/api/chat/%d/cancel'):
            method = 'GET' if target.endswith('history') else 'POST'
            assert family.request(method, target % sessions['owner'], {'text': 'x'} if method == 'POST' else None)[0] == 404
        before = len(traces('owner'))
        status, sent = family.request('POST', '/api/chat/%d/send' % sessions['family'], {'text': 'build me a page'})
        assert status == 200 and sent['access'] == 'read_only', sent
        assert wait(lambda: traces('family'), 'family delivery')[0].startswith('[wsx exchange %d, round 1, read-only]\n' % sent['id'])
        assert len(traces('owner')) == before
        wait(lambda: len(messages(family, sessions['family'])['messages']) == 2, 'family history')
        assert family.request('GET', '/' + uploaded['url'])[0] == 404  # media stores are per site
        ok('site B: capability denial for files, terminal, activity, push, session start; other project invisible')

        # --- Optional real browser, on both sites, before any device is revoked.
        if args.browser:
            args.captures.mkdir(parents=True, exist_ok=True)
            for name, site, panels in (('owner', owner, 'Chat,Files,Terminal,Activity,Settings'),
                                       ('family', family, 'Chat,Settings')):
                settle(name)
                code = cli('web', 'pair', '--site', files[name]).stdout.splitlines()[0].rsplit(' ', 1)[1]
                result = subprocess.run([node, str(ROOT / 'scripts/web-browser-check.mjs'), site.origin + site.base + '/',
                                         code, str(sessions[name]), str(args.captures), name, panels],
                                        capture_output=True, text=True, timeout=150)
                assert result.returncode == 0, result.stdout + result.stderr
                ok('real browser, ' + result.stdout.strip().splitlines()[-1])
                wait(lambda: messages(site, sessions[name])['messages'][-1]['text'].startswith('echo: browser check'), 'browser turn')
                settle(name)

        # --- Web Push with explicit device consent, to the local receiver.
        status, info = owner.request('GET', '/api/push')
        assert status == 200 and info['topics'] == [] and len(info['public_key']) == 87
        keys = {'p256dh': receiver['p256dh'], 'auth': receiver['auth']}
        for endpoint in ('https://evil.example/push', 'http://127.0.0.1:1/push', push_origin + '@evil.example/x'):
            refused = owner.request('POST', '/api/push', {'subscription': {'endpoint': endpoint, 'keys': keys}, 'topics': ['done']})
            assert refused[0] == 400, endpoint
        assert owner.request('POST', '/api/push', {'subscription': {'endpoint': push_origin + '/push/1', 'keys': dict(keys, auth='AAAA')}, 'topics': ['done']})[0] == 400
        assert pushes() == []
        subscription = {'endpoint': push_origin + '/push/device-1', 'keys': keys, 'expirationTime': None}
        status, body = owner.request('POST', '/api/push', {'subscription': subscription, 'topics': ['done', 'blocked']})
        assert status == 200 and body['topics'] == ['done', 'blocked'], body
        stored = (work / 'state/wsx/web/owner/push.json')
        assert stored.stat().st_mode & 0o777 == 0o600
        settle('owner')
        owner.request('POST', chat + '/send', {'text': 'notify me'})
        got = wait(lambda: pushes(), 'done push')[0]
        assert got.get('error') is None, got
        assert got['vapid']['ok'] and got['vapid']['alg'] == 'ES256' and got['vapid']['key'] == info['public_key']
        assert got['vapid']['claims']['sub'] == 'mailto:owner@example.invalid' and got['encoding'] == 'aes128gcm'
        assert got['payload'] == {'kind': 'done', 'title': 'Owner <wsx>', 'body': 'owner chat finished',
                                  'session_id': sessions['owner'], 'tag': 'wsx-owner-%d' % sessions['owner']}, got['payload']
        assert got['path'] == '/push/device-1'
        settle('owner')
        owner.request('POST', chat + '/send', {'text': 'ask me something'})
        blocked = wait(lambda: len(pushes()) == 2 and pushes()[1], 'blocked push')
        assert blocked['payload']['kind'] == 'blocked' and blocked['payload']['body'] == 'owner chat needs your input'
        # Blocked: the web chat refuses to type into a provider dialog.
        events.expect('workspace', lambda data: data['sessions'][0]['agent']['state'] == 'blocked')
        refused = owner.request('POST', chat + '/send', {'text': 'yes'})
        assert refused[0] == 409 and refused[1]['error']['code'] == 'agent_blocked'
        assert owner.request('POST', '/api/terminal/%d/input' % sessions['owner'], {'key': 'ctrl_c'})[0] == 200
        events.expect('workspace', lambda data: data['sessions'][0]['agent']['state'] == 'idle')
        # "error" was not consented to: the state change happens, no push is sent.
        settle('owner')
        owner.request('POST', chat + '/send', {'text': 'fail now'})
        events.expect('workspace', lambda data: data['sessions'][0]['agent']['state'] == 'error')
        time.sleep(0.6)
        assert len(pushes()) == 2, pushes()[2:]
        ok('push: endpoint allowlist, VAPID verified and payload decrypted by an independent receiver, consent per topic')

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
        assert len(pushes()) == 2, 'a revoked device was notified'
        assert json.loads(stored.read_text()) == []
        tries = [owner.request('POST', '/api/pair', {'code': 'BBBBB-BBBB%d' % index})[0] for index in range(6)]
        assert tries[-1] == 429 and set(tries[:3]) == {403}, tries
        ok('rate limit per device, revocation closes the stream and stops media, chat and push; pairing guesses limited')

        # --- session_start runs only the site file's command, in the granted project.
        status, body = root.request('POST', '/api/sessions', {'label': 'from\x07 the web'})
        assert status == 200, body
        snapshot = call('snapshot')['data']
        created = next(s for s in snapshot['sessions'] if s['id'] == body['session_id'])
        assert created['label'] == 'from the web' and created['worktree_id'] == worktrees['project']
        ok('session start: site command, granted project, cleaned label')

        # --- The web server never starts wsxd; it reports the gap and recovers.
        assert call('shutdown')['type'] == 'ack'
        daemon.wait(timeout=5)
        family_events.expect('daemon', lambda data: data['available'] is False, timeout=10)
        status, body = family.request('GET', '/api/chat/%d/history' % sessions['family'])
        assert status == 503 and body['error']['code'] == 'daemon_unavailable'
        assert family.request('GET', '/api/session')[0] == 200
        time.sleep(1.5)
        assert not path.exists(), 'wsx web started a daemon'
        ok('daemon stop: reported on the stream, API says unavailable, no daemon started')
        success = True
    finally:
        for stream in streams:
            try:
                stream.connection.close()
            except Exception:
                pass
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
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
                      'checks': checks}, indent=1))


if __name__ == '__main__':
    main()
