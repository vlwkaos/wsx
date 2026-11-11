#!/usr/bin/env python3
"""The `memory` contract (docs/memory.md) against a real program.

python3 -B scripts/test-memory.py "target/debug/wsx memory"
python3 -B scripts/test-memory.py "python3 /path/to/memory.py"

The argument is the command prefix; every call is `<prefix> <command> --scope S --json ...` in a
temp HOME and state folder. The part that reads the store's log (old text kept, torn last line)
runs when `status` names a folder with a records.jsonl, i.e. wsx's store.
"""
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


def run(*args, text=None, scope=SCOPE, cwd=WORK):
    cmd = PREFIX + list(args) + (['--scope', scope] if scope else []) + ['--json']
    done = subprocess.run(cmd, input=text, capture_output=True, text=True, cwd=cwd, env=ENV, timeout=60)
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
for bad in (None, '../work/docs/note.md', '/etc/passwd', 'docs/missing.md', 'owner:abc', 'owner:', 'nonsense'):
    extra = ['--verified-by', bad] if bad else []
    exits(2, 'add', '--kind', 'rule', '--slug', 'bad', '--text-file', '-', '--source', 'task:1', *extra, text='Never stored.')
add('owner-rule', 'Ship small changes.', kind='rule', by='owner:12')
exits(2, 'add', '--kind', 'fact', '--slug', 'f', '--text-file', '-', '--source', 'bogus', '--verified-by', 'owner:1', text='x')
exits(2, 'add', '--kind', 'bogus', '--slug', 'f', '--text-file', '-', '--source', 'task:1', text='x')
exits(2, 'add', '--kind', 'episode', '--slug', 'Bad Slug', '--text-file', '-', '--source', 'task:1', text='x')
exits(2, 'add', '--kind', 'episode', '--slug', 'big', '--text-file', '-', '--source', 'task:1', text='x' * 5000)
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
