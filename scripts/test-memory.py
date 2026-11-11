#!/usr/bin/env python3
"""The `memory` contract (docs/memory.md) against a real program.

python3 -B scripts/test-memory.py "target/debug/wsx memory"
python3 -B scripts/test-memory.py "python3 /path/to/memory.py"

The argument is the command prefix; every call is `<prefix> <command> --scope S --json ...` in a
temp HOME and state folder. The part that reads the store's log (old text kept, torn last line)
runs when `status` names a folder with a records.jsonl, i.e. wsx's store; so does the search
backend part, which runs wsx against scripts/fake-memory-backend.py.
"""
import time
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor

if len(sys.argv) != 2:
    sys.exit(__doc__)
PREFIX = shlex.split(sys.argv[1])
# ^ Every call runs in a work folder, so a program path given relative to this folder is made absolute.
PREFIX = [os.path.abspath(a) if os.path.exists(a) else a for a in PREFIX]
TMP = tempfile.mkdtemp(prefix='wsx-memory-')
WORK = os.path.join(TMP, 'work')
os.makedirs(os.path.join(WORK, 'docs'))
open(os.path.join(WORK, 'docs', 'note.md'), 'w').write('a note\n')
ENV = {k: v for k, v in os.environ.items() if k != 'WSX_TASK_BROKER'}
ENV.update(HOME=TMP, XDG_STATE_HOME=os.path.join(TMP, 'state'), XDG_CONFIG_HOME=os.path.join(TMP, 'config'))
SCOPE = 'contract'


def run(*args, text=None, scope=SCOPE, cwd=WORK, env=None):
    cmd = PREFIX + list(args) + (['--scope', scope] if scope else []) + ['--json']
    done = subprocess.run(cmd, input=text, capture_output=True, text=True, cwd=cwd, env={**ENV, **(env or {})}, timeout=60)
    return done.returncode, done.stdout, done.stderr


def ok(*args, **kw):
    code, out, err = run(*args, **kw)
    check(code == 0, f'{args} exit {code}: {err}')
    return json.loads(out)


def check(condition, message):
    if not condition:
        print(f'FAIL: {message}')
        cleanup()
        sys.exit(1)


def cleanup():
    shutil.rmtree(TMP, ignore_errors=True)


def exits(want, *args, **kw):
    code, out, err = run(*args, **kw)
    check(code == want, f'{args}: want exit {want}, got {code}: {out}{err}')
    return err


def add(slug, text, kind='episode', by=None, source='task:1'):
    extra = ['--verified-by', by] if by else []
    return ok('add', '--kind', kind, '--slug', slug, '--text-file', '-', '--source', source, *extra, text=text)


def counts():
    return ok('status')['counts']


def slugs(query, *flags):
    return [hit['slug'] for hit in ok('search', query, *flags)]


# add: a rule needs verified_by, an episode does not
record = add('indent-rule', 'Indent with tabs in the shell scripts.', kind='rule', by='docs/note.md')
check(record['slug'] == 'indent-rule' and record['status'] == 'active' and record['kind'] == 'rule', record)
check(record['verified_by'] == 'docs/note.md' and record['source'] == 'task:1' and record['updated'] > 0, record)
for bad in (None, '../work/docs/note.md', '/etc/passwd', 'docs/missing.md', 'owner:abc', 'owner:', 'nonsense',
            'owner:2026-11-11', 'owner:transcript', 'owner:transcript:', 'owner:transcript:missing.jsonl', 'owner:transcript:docs',
            'owner:transcript:docs/note.md#L0', 'owner:transcript:docs/note.md#Lx', 'owner:transcript:#L1'):
    extra = ['--verified-by', bad] if bad else []
    exits(2, 'add', '--kind', 'rule', '--slug', 'bad', '--text-file', '-', '--source', 'task:1', *extra, text='Never stored.')
add('owner-rule', 'Ship small changes.', kind='rule', by='owner:12')
exits(2, 'add', '--kind', 'fact', '--slug', 'f', '--text-file', '-', '--source', 'bogus', '--verified-by', 'owner:1', text='x')
exits(2, 'add', '--kind', 'bogus', '--slug', 'f', '--text-file', '-', '--source', 'task:1', text='x')
exits(2, 'add', '--kind', 'episode', '--slug', 'Bad Slug', '--text-file', '-', '--source', 'task:1', text='x')
LIMIT = ok('status')['limits']['text_bytes']
check(isinstance(LIMIT, int) and LIMIT > 0, 'status declares limits.text_bytes')
exits(2, 'add', '--kind', 'episode', '--slug', 'big', '--text-file', '-', '--source', 'task:1', text='x' * (LIMIT + 1))
check(counts()['active'] == 2, counts())

# NOOP: a duplicate returns the old record and writes nothing
again = add('other-slug', 'indent with TABS  in the shell   scripts.', kind='rule', by='owner:13')
check(again['slug'] == 'indent-rule' and counts()['active'] == 2, again)
exits(2, 'add', '--kind', 'episode', '--slug', 'indent-rule', '--text-file', '-', '--source', 'task:1', text='Different words entirely.')

# search: words, score, excerpt, no hit is []
add('long', 'zebra ' + 'y' * 400)
hits = ok('search', 'tabs shell')
check([h['slug'] for h in hits] == ['indent-rule'], hits)
hit = hits[0]
check({'slug', 'path', 'kind', 'status', 'score', 'excerpt'} <= set(hit), hit)
check(0 < hit['score'] <= 1 and hit['status'] == 'active' and hit['kind'] == 'rule', hit)
check(ok('search', 'tabs unknownword')[0]['score'] < hit['score'], 'a missing word lowers the score')
check(ok('search', 'zebra')[0]['excerpt'] == ('zebra ' + 'y' * 400)[:240], 'excerpt is 240 characters')
check(ok('search', 'nothingmatches') == [], 'no hit is []')
check(slugs('tabs', '--kind', 'episode') == [], '--kind filters')
check(len(ok('search', 'zebra tabs ship', '--limit', '1')) == 1, '--limit')

# update: the text changes, the old text is not found any more
updated = ok('update', '--slug', 'indent-rule', '--text-file', '-', '--source', 'task:2', text='Indent with spaces in the shell scripts.', scope=SCOPE)
check(updated['text'].startswith('Indent with spaces') and updated['source'] == 'task:2', updated)
check(slugs('spaces') == ['indent-rule'] and slugs('tabs') == [], 'search reads the new text')
exits(1, 'update', '--slug', 'no-such', '--text-file', '-', '--source', 'task:2', text='x')
exits(2, 'update', '--slug', 'indent-rule', '--source', 'task:2')
exits(2, 'update', '--slug', 'indent-rule', '--status', 'failed', '--source', 'task:2')

# INVALIDATE and supersede: out of search unless --include-inactive
failed = ok('update', '--slug', 'owner-rule', '--status', 'failed', '--lesson', 'It slowed review.', '--source', 'task:3')
check(failed['status'] == 'failed' and failed['lesson'] == 'It slowed review.', failed)
check(slugs('ship') == [] and slugs('ship', '--include-inactive') == ['owner-rule'], 'failed record')
add('newer', 'Spaces everywhere, in the shell scripts too.', kind='episode')
old = ok('supersede', 'indent-rule', 'newer', '--source', 'task:4')
check(old['status'] == 'superseded' and old['superseded_by'] == 'newer', old)
check('indent-rule' not in slugs('indent') and 'indent-rule' in slugs('indent', '--include-inactive'), 'superseded record')
inactive = [h for h in ok('search', 'indent', '--include-inactive') if h['slug'] == 'indent-rule']
check(inactive and inactive[0]['status'] == 'superseded', inactive)
exits(1, 'supersede', 'no-such', 'newer', '--source', 'task:4')
exits(1, 'supersede', 'newer', 'no-such', '--source', 'task:4')
exits(2, 'supersede', 'newer', 'newer', '--source', 'task:4')
exits(2, 'supersede', 'newer', 'indent-rule', '--source', 'task:4')
check(counts() == {'active': 2, 'superseded': 1, 'failed': 1}, counts())

# status, reindex, list
status = ok('status')
check(status['scope'] == SCOPE and status['backend'] and status['search'] == {'network': False, 'model': False}, status)
check(ok('reindex') == {'scope': SCOPE, 'ok': True}, 'reindex')
check(SCOPE in [row['scope'] for row in ok('list', scope=None)], 'list names the scope')

# two writers at once: every record is kept
before = counts()['active']
with ThreadPoolExecutor(8) as pool:
    results = list(pool.map(lambda n: run('add', '--kind', 'episode', '--slug', f'c{n}', '--text-file', '-', '--source', 'task:5',
                                          text=f'word{n}'), range(40)))
check(all(code == 0 for code, _, _ in results), [r for r in results if r[0]])
check(counts()['active'] == before + 40, counts())
check(all(slugs(f'word{n}') == [f'c{n}'] for n in (0, 17, 39)), 'concurrent records are found')

# the text limit is the store's own: exactly the limit is stored, one byte more is exit 2
ok('add', '--kind', 'episode', '--slug', 'edge', '--text-file', '-', '--source', 'task:1', text='y' * LIMIT, scope='extras')
exits(2, 'update', '--slug', 'edge', '--text-file', '-', '--source', 'task:1', text='z' * (LIMIT + 1), scope='extras')
check(ok('status', scope='extras')['limits']['text_bytes'] == LIMIT, 'the limit is the same in every scope of the store')

# verified_by owner:transcript:<file>[#L<n>]: owner words outside wsx, the file must exist; owner:<digits> stays
transcript = os.path.join(TMP, 'owner-session.jsonl')
open(transcript, 'w').write('{"role":"user"}\n{"role":"user"}\n')
for n, by in enumerate(('owner:transcript:%s' % transcript, 'owner:transcript:%s#L2' % transcript, 'owner:transcript:docs/note.md#L1')):
    stored = add('said-%d' % n, 'The owner said so outside wsx, number %d.' % n, kind='rule', by=by, source='transcript:session')
    check(stored['verified_by'] == by, stored)
exits(2, 'add', '--kind', 'rule', '--slug', 'gone', '--text-file', '-', '--source', 'task:1', '--verified-by',
      'owner:transcript:%s' % os.path.join(TMP, 'missing.jsonl'), text='A transcript that is not there.')


def backend_part():
    """wsx with a search backend (docs/memory.md "Search backend"): hits merge by score, exit 3, a hang
    and bad output fall back to the built-in with a notice on stderr and clean JSON on stdout."""
    fake = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'fake-memory-backend.py')
    config = (os.path.join(TMP, 'Library', 'Application Support') if sys.platform == 'darwin' else ENV['XDG_CONFIG_HOME'])
    path = os.path.join(config, 'wsx', 'config-v2.toml')
    with open(path, 'a') as file:
        file.write('\n[memory]\nbackend = ["python3", "%s"]\n' % fake)
    calls = os.path.join(TMP, 'backend-calls.jsonl')
    hits = os.path.join(TMP, 'backend-hits.json')

    def hit(slug, score, **more):
        return {'slug': slug, 'scope': SCOPE, 'path': '/vault/%s.md' % slug, 'kind': 'fact', 'status': 'active', 'score': score,
                'excerpt': 'external %s' % slug, **more}

    def call(*args, mode='ok', **kw):
        env = {'FAKE_MEMORY_MODE': mode, 'FAKE_MEMORY_HITS': hits, 'FAKE_MEMORY_LOG': calls}
        return run(*args, env=env, **kw)

    json.dump([hit('vault-note', 0.99), hit('long', 0.0001, kind='episode'), hit('retired', 0.9, status='superseded'), hit('wrong-kind', 0.8, kind='episode')],
              open(hits, 'w'))
    code, out, err = call('search', 'zebra', '--kind', 'fact', '--limit', '5')
    found = json.loads(out)
    check(code == 0 and 'unavailable' not in err, (code, err))  # ^ The torn line of the part above still earns its own notice.
    # ^ The built-in 'long' is an episode: the kind filter holds for the backend's hits too, and a superseded hit needs --include-inactive.
    check([h['slug'] for h in found] == ['vault-note'], found)
    code, out, err = call('search', 'zebra', '--limit', '5')
    found = json.loads(out)
    check([h['slug'] for h in found] == ['vault-note', 'wrong-kind', 'long'] and found[0]['scope'] == SCOPE, found)
    check(found[2]['score'] > 0.0001 and found[2]['path'].endswith('records.jsonl'), 'a slug in both stores keeps the higher score (the built-in)')
    check(len(json.loads(call('search', 'zebra', '--limit', '1')[1])) == 1, 'the limit holds for the merge')
    check('retired' in [h['slug'] for h in json.loads(call('search', 'zebra', '--include-inactive')[1])], '--include-inactive reaches the backend')
    seen = [json.loads(line) for line in open(calls)]
    first = seen[0]
    check(first[0] == 'search' and first[first.index('--scope') + 1] == SCOPE and '--json' in first and first[-2:] == ['--', 'zebra'], first)
    check(first[first.index('--kind') + 1] == 'fact' and first[first.index('--limit') + 1] == '5', first)

    for mode, why in (('exit3', 'exit 3'), ('exit1', 'exit 1'), ('garbage', 'no list of hits')):
        code, out, err = call('search', 'zebra', mode=mode)
        check(code == 0 and [h['slug'] for h in json.loads(out)] == ['long'], (mode, code, out))
        check('unavailable' in err and why in err and 'built-in answered' in err, (mode, err))
    started = time.time()
    code, out, err = call('search', 'zebra', mode='hang')
    took = time.time() - started
    check(code == 0 and [h['slug'] for h in json.loads(out)] == ['long'] and 'no answer in 10 s' in err, (code, out, err))
    check(9 <= took < 30, 'the backend call stops at 10 s: %.1f s' % took)

    state = json.loads(call('status')[1])
    check(state['backend'].endswith('fake-memory-backend.py') and state['search'] == {'network': True, 'model': False}, state)
    check('backend_error' not in state and state['limits']['text_bytes'] == LIMIT, state)
    down = json.loads(call('status', mode='exit3')[1])
    check(down['search'] == {'network': False, 'model': False} and 'locked' in down['backend_error'], down)
    check(call('reindex')[0] == 0 and json.loads(open(calls).readlines()[-1])[0] == 'reindex', 'reindex reaches the backend')
    check(call('reindex', mode='exit3')[0] == 3, 'a backend that cannot reindex is exit 3')

    # recall: the top 5 by score of the top 20, backend and built-in together
    json.dump([hit('r%d' % n, 0.9 - n / 20) for n in range(8)], open(hits, 'w'))
    code, out, err = call('recall', 'zebra')
    check(code == 0 and [h['slug'] for h in json.loads(out)] == ['r0', 'r1', 'r2', 'r3', 'r4'], (code, out, err))
    check(call('recall', 'zebra', mode='exit3')[0] == 0 and [h['slug'] for h in json.loads(call('recall', 'zebra', mode='exit3')[1])] == ['long'], 'recall falls back')
    exits(2, 'recall', '!!!')
    with open(path) as file:
        text = file.read()
    with open(path, 'w') as file:
        file.write(text[:text.index('\n[memory]')] + '\n')


def project_scope():
    """A registered project: --scope defaults to the project of the current folder."""
    import hashlib
    project = os.path.realpath(os.path.join(TMP, 'proj'))
    os.makedirs(os.path.join(project, 'sub'))
    open(os.path.join(project, 'README.md'), 'w').write('x\n')
    # ^ `dirs` takes macOS's config folder from HOME, not from XDG_CONFIG_HOME.
    config = (os.path.join(TMP, 'Library', 'Application Support') if sys.platform == 'darwin' else ENV['XDG_CONFIG_HOME'])
    os.makedirs(os.path.join(config, 'wsx'))
    open(os.path.join(config, 'wsx', 'config-v2.toml'), 'w').write(f'groups = []\n[[projects]]\nname = "proj"\npath = "{project}"\n')
    pid = 'proj-' + hashlib.sha256(project.encode()).hexdigest()[:8]
    sub = os.path.join(project, 'sub')
    code, out, err = run('add', '--kind', 'fact', '--slug', 'readme', '--text-file', '-', '--source', 'task:1',
                         '--verified-by', 'README.md', text='The project has a readme.', scope=None, cwd=sub)
    check(code == 0, f'add in a project folder: {code} {err}')
    check(ok('status', scope=None, cwd=sub)['scope'] == pid, 'the scope is the project of the current folder')
    check(os.path.exists(os.path.join(ENV['XDG_STATE_HOME'], 'wsx', 'tasks', pid, 'memory', 'records.jsonl')), 'the store is in the task folder')
    check([h['slug'] for h in ok('search', 'readme', scope=pid)] == ['readme'], 'the project ID names the scope')
    # ^ The path is relative to the project, not to the folder the call runs in.
    exits(2, 'add', '--kind', 'fact', '--slug', 'f2', '--text-file', '-', '--source', 'task:1', '--verified-by', 'sub/none.md', text='x', scope=None, cwd=sub)
    exits(2, 'add', '--kind', 'episode', '--slug', 'e', '--text-file', '-', '--source', 'task:1', text='x', scope=None)
    exits(2, 'add', '--kind', 'fact', '--slug', 'f3', '--text-file', '-', '--source', 'task:1', '--verified-by', 'task:9', text='x', scope=pid)
    exits(2, 'add', '--kind', 'fact', '--slug', 'f4', '--text-file', '-', '--source', 'task:1', '--verified-by', 'check:c1', text='x', scope=pid)
    check(pid in [row['scope'] for row in ok('list', scope=None, cwd=sub)], 'list names the project')
    # ^ A project scope also searches the project's transcripts; the manager's notes are the simplest to stage.
    notes = os.path.join(ENV['XDG_STATE_HOME'], 'wsx', 'tasks', pid, 'manager', 'notes')
    os.makedirs(notes)
    open(os.path.join(notes, 'decisions.md'), 'w').write('- 2026-11-01 Always ship small changes with a readme.\n')
    mixed = ok('search', 'readme', scope=pid)
    check([h['slug'] for h in mixed] == ['readme', 'proj:notes/decisions.md'] or {h['slug'] for h in mixed} == {'readme', 'proj:notes/decisions.md'}, mixed)
    note = next(h for h in mixed if h['kind'] == 'decision')
    check(note['status'] == 'active' and note['scope'] == pid and note['path'].endswith('manager/notes/decisions.md') and note['by'] == 'notes', note)
    check(all(0 < h['score'] <= 1 for h in mixed), mixed)
    check([h['slug'] for h in ok('search', 'readme', '--kind', 'decision', scope=pid)] == ['proj:notes/decisions.md'], '--kind decision')
    check([h['slug'] for h in ok('search', 'readme', '--kind', 'fact', scope=pid)] == ['readme'], '--kind fact leaves the transcripts out')
    check(ok('reindex', scope=pid) == {'scope': pid, 'ok': True}, 'reindex of a project scope')
    backend_part()


# the store's log (wsx): the old text stays, a torn last line is skipped
log = os.path.join(status['store'], 'records.jsonl')
if os.path.exists(log):
    check('Indent with tabs' in open(log).read(), 'the old text stays in the log')
    with open(log, 'a') as file:
        file.write('{"op":"add","slug":"torn","kind":"epis')
    code, out, err = run('search', 'zebra')
    check(code == 0 and json.loads(out)[0]['slug'] == 'long' and 'skipped' in err, f'torn line: {code} {out} {err}')
    add('late-note', 'Written after the crash.')
    check(slugs('crash') == ['late-note'] and slugs('torn') == [], 'a write after a torn line is intact')
    exits(2, 'add', '--kind', 'fact', '--slug', 'tk', '--text-file', '-', '--source', 'task:1', '--verified-by', 'task:9', text='x')
    check(ok('status', scope='default')['counts']['active'] == 0, 'scopes are separate')
    project_scope()
else:
    print('skip: the store has no records.jsonl; log checks not run')

print('memory contract ok')
cleanup()
