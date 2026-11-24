#!/usr/bin/env python3
"""Evaluation harness: does desk mode keep the quality of one plain Claude Code session, and at what cost?

Question: the project's lead agent starts worker tasks that plan, build and review, then a review job lands
them (docs/design/orchestration.md "Evaluation"). Compared with one `claude -p` session on the same request,
is the result as good (hidden tests), and what do tokens and minutes cost?

Cases come from this repository's history: non-merge commits on --ref (default main) that change both
crates/*/src/** and a top-level Rust integration test crates/<crate>/tests/<name>.rs, at most 400 changed lines.
A case is the parent commit, the request (the commit message) and the hidden tests (the commit's changed
crates/*/tests/**/*.rs files, absent or older in the parent).

Each case runs in two arms, each in its own copy of the parent at .work/eval/<run>/<case>/<arm>:
  solo  one `claude -p "<request>" --output-format json` in an isolated copy (`isolated_copy`), edits and
        Bash allowed, --minutes wall limit.
  desk  an isolated wsx (own HOME, XDG folders, socket and wsxd; built binaries of this checkout) with the
        copy registered as a project. The request goes to the lead with `wsx chat say`. The owner answers a
        question, or a reply that started no task, with one plain go (`--owner-turns`, default 2), as in a real
        session where the lead sizes a request before it works. The run ends when the
        lead answered and no task is open, when only the owner could move the open tasks on (approvals are
        never given: that counts as unfinished), or at the limit. Both arms work in a copy that holds only the
        parent's history (`isolated_copy`): no ref, tag or object of the answer commit, and nothing reaches this repo.
        Project config (wsx.config.yml) is replaced by a small one: auto land, checks = build + the crate's
        visible tests (--desk-check, repeatable; {crate} is replaced).

The judge is the same for both arms and runs outside them: the hidden test files of the commit are copied into
the final tree (desk: main after landing) and `cargo nextest run -p <crate> --test <name> --locked` runs per test
file. A case passes when every file builds, runs at least one test and has no failure.

Output: .work/eval/<run>/results.jsonl, one JSON line per (case, arm): minutes, tokens (solo: claude's JSON usage;
desk: the task and chat logs, summed per scripts/usage-report.py's rules, plus its report text kept in
evidence/), cost, diff size, test counts. The last lines are a table: pass rate, median minutes, median tokens.

  scripts/eval-desk.py --dry-run --cases 1     the fake agent (scripts/fake-claude-stream.py), no model call
  scripts/eval-desk.py --cases 10              the full evaluation (real calls on the owner's subscription)
  scripts/eval-desk.py --case <sha> --arms solo

Needs: built `wsx` and `wsxd` (built here unless --no-build), cargo with nextest, `claude` signed in (not in
--dry-run), and no command sandbox around it (wsxd, PTYs and sockets). Every worktree, clone, isolated wsxd and
target folder is removed at the end, on failure and on Ctrl-C; only processes this run started are stopped.
Stdlib only; Python 3.9 compatible. ref: docs/design/orchestration.md "Evaluation", scripts/dev.sh,
scripts/test-manager-boundary.py, AGENTS.md "Real task runs with a HOME wrapper".
"""
import argparse
import json
import os
import re
import shlex
import shutil
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FAKE = ROOT / 'scripts/fake-claude-stream.py'
USAGE_REPORT = ROOT / 'scripts/usage-report.py'
MAX_LINES = 400
SCAN = 800  # commits looked at for cases
DONE_TASK = ('landed', 'closed')
OWNER_WAIT = ('asks', 'land_stopped', 'owner', 'paused', 'plan')  # waits that only the owner ends
POLL = 5

STARTED = []  # Popen objects of this run
CLEANUPS = []  # removals, newest last; skipped with --keep
STOPS = []  # daemon stops, always run
KEEP = False
DRY_REQUEST = 'RUN wsx task new "WORK eval-dry.txt" --title eval-dry --json'


def log(*parts):
    print('[eval]', *parts, file=sys.stderr, flush=True)


def run(argv, cwd=None, env=None, timeout=120, check=True, input=None):
    done = subprocess.run([str(a) for a in argv], cwd=cwd, env=env, capture_output=True, text=True,
                          timeout=timeout, input=input)
    if check and done.returncode != 0:
        raise RuntimeError('%s failed (%d): %s' % (' '.join(map(str, argv[:4])), done.returncode,
                                                   (done.stderr or done.stdout)[-600:]))
    return done


def git(*argv, cwd=ROOT, check=True):
    return run(['git', *argv], cwd=cwd, check=check)


def clean_env(extra=None):
    """The parent environment without the markers of an outer Claude Code or wsx session."""
    env = {k: v for k, v in os.environ.items()
           if not (k.startswith('CLAUDE_CODE_') or k in ('CLAUDECODE', 'WSX_PANE_ID', 'WSX_SOCKET',
                                                          'WSX_DAEMON_BIN', 'WSX_EXIT_WITH_PID', 'CARGO_TARGET_DIR'))}
    env.update(extra or {})
    return env


# ---- cases -----------------------------------------------------------------------------------------------

def case_of(sha):
    """The case of a commit, or None when it has no hidden test or is too large or a merge."""
    parents = git('rev-list', '--parents', '-n', '1', sha).stdout.split()[1:]
    if len(parents) != 1:
        return None
    status = git('diff-tree', '-r', '--no-commit-id', '--name-status', '--no-renames', sha).stdout.splitlines()
    lines = 0
    for row in git('diff-tree', '-r', '--no-commit-id', '--numstat', '--no-renames', sha).stdout.splitlines():
        added, deleted, _ = row.split('\t', 2)
        lines += int(added) + int(deleted) if added.isdigit() and deleted.isdigit() else 0
    if lines > MAX_LINES:
        return None
    src, tests, targets = False, [], []
    for row in status:
        kind, path = row.split('\t', 1)
        match = re.match(r'crates/([^/]+)/(src|tests)/(.*)$', path)
        if not match or kind == 'D':
            continue
        crate, part, rest = match.groups()
        if part == 'src':
            src = True
        elif path.endswith('.rs'):
            tests.append(path)
            if '/' not in rest:
                targets.append((crate, rest[:-3]))
    if not (src and targets):
        return None
    request = git('log', '-1', '--format=%B', sha).stdout.strip()
    return {'sha': sha, 'short': sha[:8], 'parent': parents[0], 'request': request, 'lines': lines,
            'tests': tests, 'targets': targets}


def pick_cases(args):
    if args.case:
        sha = git('rev-parse', '--verify', args.case + '^{commit}').stdout.strip()
        found = case_of(sha)
        if not found:
            raise SystemExit('eval-desk: %s is not a case (merge, over %d lines, or no src + integration test)'
                             % (args.case, MAX_LINES))
        return [found]
    found = []
    for sha in git('rev-list', '--no-merges', '-n', str(SCAN), args.ref).stdout.split():
        one = case_of(sha)
        if one:
            found.append(one)
        if len(found) == args.cases:
            break
    if not found:
        raise SystemExit('eval-desk: no case in the last %d commits of %s' % (SCAN, args.ref))
    return found


# ---- processes and cleanup ---------------------------------------------------------------------------------

def spawn(argv, **kw):
    proc = subprocess.Popen([str(a) for a in argv], start_new_session=True, **kw)
    STARTED.append(proc)
    return proc


def stop_group(proc):
    """Stop a process this run started and its group; never a pid this run did not start."""
    if proc.poll() is None:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(proc.pid, sig)
            except (ProcessLookupError, PermissionError):
                break
            try:
                proc.wait(timeout=8)
                break
            except subprocess.TimeoutExpired:
                continue
    if proc in STARTED:
        STARTED.remove(proc)


def kill_named(folder):
    """Processes whose command line names this run's folder (the isolated wsxd's host children): ours only."""
    mine = {os.getpid(), os.getppid()}
    found = subprocess.run(['pgrep', '-f', str(folder)], capture_output=True, text=True).stdout.split()
    for pid in (int(p) for p in found if p.isdigit()):
        if pid not in mine and pid != 13470:
            try:
                os.kill(pid, signal.SIGTERM)
            except (ProcessLookupError, PermissionError):
                pass


def cleanup_all():
    for proc in list(STARTED):
        stop_group(proc)
    run_cleanups(0)


def run_cleanups(mark):
    previous = signal.signal(signal.SIGINT, signal.SIG_IGN)  # a second Ctrl-C must not leave half a folder
    try:
        _run_cleanups(mark)
    finally:
        signal.signal(signal.SIGINT, previous)


def _run_cleanups(mark):
    while STOPS:
        try:
            STOPS.pop()()
        except Exception as err:  # keep cleaning
            log('stop failed:', err)
    while len(CLEANUPS) > mark:
        action = CLEANUPS.pop()
        if KEEP:
            continue
        try:
            action()
        except Exception as err:
            log('cleanup failed:', err)


def on_signal(signum, _frame):
    raise KeyboardInterrupt('signal %d' % signum)


# ---- arms --------------------------------------------------------------------------------------------------

def seed_target(arm_dir, args):
    """Clone this checkout's target folder (copy-on-write) so registry dependencies are not rebuilt."""
    target = ROOT / 'target'
    if args.cold or not target.is_dir():
        return
    done = subprocess.run(['cp', '-cR', str(target), str(arm_dir / 'target')], capture_output=True)
    if done.returncode != 0:
        shutil.rmtree(arm_dir / 'target', ignore_errors=True)


def fake_bin(folder):
    """A `claude` on PATH that is the fake agent, as crates/wsx/tests/common/mod.rs Fixture puts it."""
    folder.mkdir(parents=True, exist_ok=True)
    fake = folder / 'claude'
    fake.write_text('#!/bin/sh\nexec python3 %s "$@"\n' % json.dumps(str(FAKE)))
    fake.chmod(0o755)
    return folder


def isolated_copy(case, arm_dir):
    """A repository at the case's parent that holds only its history: one branch `main`, no tags, no
    remote. A worktree or a clone of this repo would show the answer commit (`git log main`, a tag)."""
    git('init', '-q', '-b', 'main', str(arm_dir))
    git('fetch', '-q', '--no-tags', str(ROOT), '%s:refs/heads/start' % case['parent'], cwd=arm_dir)
    git('checkout', '-q', '-B', 'main', 'start', cwd=arm_dir)
    git('branch', '-q', '-D', 'start', cwd=arm_dir)


def solo_copy(case, arm_dir):
    isolated_copy(case, arm_dir)

    def drop():
        shutil.rmtree(arm_dir, ignore_errors=True)
    CLEANUPS.append(drop)
    return drop


def parse_claude_json(text):
    """Claude's `--output-format json` result object: the last JSON object line that has a `type: result`."""
    for line in reversed(text.strip().splitlines()):
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if isinstance(obj, dict) and obj.get('type') == 'result':
            return obj
        if isinstance(obj, list):
            for item in reversed(obj):
                if isinstance(item, dict) and item.get('type') == 'result':
                    return item
    return None


def tokens_of(usage):
    usage = usage or {}
    parts = {'input': usage.get('input_tokens', 0), 'output': usage.get('output_tokens', 0),
             'cache_write': usage.get('cache_creation_input_tokens', 0),
             'cache_read': usage.get('cache_read_input_tokens', 0)}
    parts['total'] = sum(parts.values())
    return parts


def arm_solo(case, arm_dir, args, evidence):
    started = time.monotonic()
    solo_copy(case, arm_dir)
    seed_target(arm_dir, args)
    env = clean_env()
    claude = 'claude'
    if args.dry_run:
        env['PATH'] = str(fake_bin(arm_dir.parent / 'solo-bin')) + ':' + env['PATH']
        env['FAKE_CLAUDE_TRACE'] = str(arm_dir.parent / 'solo-fake.trace')
        env['HOME'] = str(arm_dir.parent / 'solo-home')
        Path(env['HOME']).mkdir(exist_ok=True)
    else:
        claude = shutil.which('claude')
        if not claude:
            raise RuntimeError('claude is not on PATH')
    argv = [claude, '-p', case['request'], '--output-format', 'json', '--permission-mode', 'acceptEdits',
            '--allowedTools', 'Bash,Edit,Write,Read,Glob,Grep,NotebookEdit']
    # ^ acceptEdits alone refuses `cargo` in a headless run; the folder is a throwaway copy.
    if args.model:
        argv += ['--model', args.model]
    proc = spawn(argv, cwd=arm_dir, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                 stderr=subprocess.PIPE, text=True)
    timed_out = False
    try:
        out, err = proc.communicate(timeout=args.minutes * 60)
    except subprocess.TimeoutExpired:
        timed_out = True
        stop_group(proc)
        out, err = '', ''
    minutes = (time.monotonic() - started) / 60
    (evidence / 'solo.out').write_text(out + '\n--- stderr\n' + err)
    result = parse_claude_json(out)
    row = {'minutes': round(minutes, 2), 'timed_out': timed_out,
           'finished': bool(result) and not result.get('is_error') and not timed_out,
           'exit': proc.returncode}
    if result:
        row['tokens'] = tokens_of(result.get('usage'))
        row['cost_usd'] = result.get('total_cost_usd')
        row['turns'] = result.get('num_turns')
        if result.get('is_error'):
            row['error'] = str(result.get('result'))[:300]
    else:
        row['tokens'] = None
        row['error'] = 'no JSON result' + (' (time limit)' if timed_out else ': ' + err[-200:])
    git('add', '-A', cwd=arm_dir, check=False)
    row['diff'] = diff_size(arm_dir, case['parent'])
    return row, arm_dir


def diff_size(tree, parent, tip=None):
    """Changed files and lines from the parent to `tip`, or to the index (solo stages everything first)."""
    files = added = deleted = 0
    span = [parent, tip] if tip else ['--cached', parent]
    out = git('diff', '--numstat', *span, '--', '.', ':!wsx.config.yml', cwd=tree, check=False).stdout
    for row in out.splitlines():
        a, d, _ = row.split('\t', 2)
        files += 1
        added += int(a) if a.isdigit() else 0
        deleted += int(d) if d.isdigit() else 0
    return {'files': files, 'added': added, 'deleted': deleted}


def desk_config(case, args):
    crate = case['targets'][0][0]
    checks = args.desk_check or (['true'] if args.dry_run else ['cargo build -p wsx -p wsx-daemon --locked', 'cargo nextest run -p {crate} --locked'])
    lines = ['tasks:', '  minFreeGb: 0', '  env:', '    CARGO_INCREMENTAL: "0"', '  checks:']
    lines += ['    - %s' % json.dumps(c.replace('{crate}', crate)) for c in checks]
    lines += ['  checkTimeoutSeconds: 1800', '  land: { auto: true }']
    if args.model:
        lines += ['  agent: { model: %s }' % args.model, '  chats: { model: %s }' % args.model,
                  '  lead: { model: same }']
    return '\n'.join(lines) + '\n'


def wsx_json(wsx, env, cwd, *argv, timeout=60):
    done = run([wsx, *argv, '--json'], cwd=cwd, env=env, timeout=timeout)
    return json.loads(done.stdout)


def arm_desk(case, arm_dir, args, evidence):
    started = time.monotonic()
    wsx = Path(args.wsx).resolve()
    wsxd = wsx.with_name('wsxd')
    base = arm_dir.parent
    home, state, cfg, cache, tmp, bindir = (base / n for n in ('dh', 'ds', 'dc', 'dk', 'dt', 'db'))
    CLEANUPS.append(lambda: [shutil.rmtree(p, ignore_errors=True) for p in (home, state, cfg, cache, tmp, bindir)])
    for folder in (home, state, cfg, cache, tmp):
        folder.mkdir(mode=0o700, parents=True, exist_ok=True)
    # ^ An isolated copy, not a worktree: wsx makes branches, workbenches and landings in the project, and
    # none of that may reach this repository.
    isolated_copy(case, arm_dir)
    CLEANUPS.append(lambda: [shutil.rmtree(p, ignore_errors=True) for p in [arm_dir, *base.glob('desk-t*')]])
    # ^ wsx puts each task's workbench beside the project folder: desk-t1, desk-t2.
    seed_target(arm_dir, args)
    ident = ['-c', 'user.name=eval', '-c', 'user.email=eval@example.invalid']
    (arm_dir / 'wsx.config.yml').write_text(desk_config(case, args))
    git(*ident, 'add', '-f', 'wsx.config.yml', cwd=arm_dir)
    git(*ident, 'commit', '-q', '-m', 'eval: project config', cwd=arm_dir)
    (home / '.gitconfig').write_text('[user]\n\tname = eval\n\temail = eval@example.invalid\n')
    real = Path.home()
    config_dir = home / 'Library/Application Support/wsx' if sys.platform == 'darwin' else cfg / 'wsx'
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / 'config-v2.toml').write_text(
        'resume_agents_on_restore = false\n[[projects]]\nname = "eval"\npath = %s\n' % json.dumps(str(arm_dir)))
    if args.dry_run:
        fake_bin(bindir)
        extra_path = str(bindir)
    else:
        # ^ The Claude login needs the real HOME, which would also give every command Claude runs the real
        # HOME: the shell prefix puts the scratch one back (AGENTS.md "Real task runs with a HOME wrapper").
        claude = shutil.which('claude')
        if not claude:
            raise RuntimeError('claude is not on PATH')
        bindir.mkdir(parents=True, exist_ok=True)
        prefix = bindir / 'shell-prefix'
        prefix.write_text('#!/bin/sh\nHOME=%s exec /bin/zsh -c "$1"\n' % shlex.quote(str(home)))
        prefix.chmod(0o755)
        wrapper = bindir / 'claude'
        wrapper.write_text('#!/bin/sh\nHOME=%s CLAUDE_CODE_SHELL_PREFIX=%s exec %s "$@"\n' % (
            shlex.quote(str(real)), shlex.quote(str(prefix)), shlex.quote(str(Path(claude).resolve()))))
        wrapper.chmod(0o755)
        extra_path = str(bindir)
    env = clean_env({
        'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(cfg),
        'XDG_CACHE_HOME': str(cache), 'TMPDIR': str(tmp), 'WSX_SOCKET': str(base / 'dh/d.sock'),
        'WSX_DAEMON_BIN': str(wsxd), 'WSX_EXIT_WITH_PID': str(os.getpid()),
        'RUSTUP_HOME': os.environ.get('RUSTUP_HOME', str(real / '.rustup')),
        'CARGO_HOME': os.environ.get('CARGO_HOME', str(real / '.cargo')),
        'PATH': ':'.join(p for p in (extra_path, str(wsx.parent), os.environ['PATH']) if p),
        'GIT_CEILING_DIRECTORIES': str(base), 'FAKE_CLAUDE_TRACE': str(base / 'fake.trace')})
    if len(env['WSX_SOCKET']) > 90:
        raise RuntimeError('socket path too long: %s' % env['WSX_SOCKET'])

    def stop_daemon():
        run([wsx, 'daemon', 'stop', '--force'], env=env, check=False, timeout=40)
        kill_named(base)
    STOPS.append(stop_daemon)

    run([wsx, 'chat', 'new', 'lead', '-p', 'eval'], cwd=arm_dir, env=env)
    # ^ The project's first chat is its main chat: the lead (docs/tasks.md "Lead and forks").
    request = DRY_REQUEST if args.dry_run else case['request']
    asked = wsx_json(wsx, env, arm_dir, 'chat', 'say', request, '-p', 'eval')
    deadline = time.monotonic() + args.minutes * 60
    outcome, quiet, said, goes = 'limit', 0, asked['id'], 0
    while time.monotonic() < deadline:
        time.sleep(POLL)
        verdict = desk_state(wsx, env, arm_dir, said)
        # ^ The owner's go, as in a real session: the lead sizes a request and asks before it starts
        # work. A question or a reply that started nothing gets one plain yes, at most --owner-turns times.
        if verdict in ('done', 'owner') and goes < args.owner_turns and owner_should_answer(wsx, env, arm_dir):
            said = wsx_json(wsx, env, arm_dir, 'chat', 'say', OWNER_GO, '-p', 'eval')['id']
            goes, quiet = goes + 1, 0
            continue
        if verdict == 'done':
            quiet += 1
            if quiet >= 2:
                outcome = 'done'
                break
        elif verdict == 'owner':
            quiet += 1
            if quiet >= 6:
                outcome = 'waits for the owner'
                break
        else:
            quiet = 0
    minutes = (time.monotonic() - started) / 60
    open_tasks = [t for t in task_items(wsx_json(wsx, env, arm_dir, 'task', 'list'))
                  if t.get('state') not in DONE_TASK]
    row = {'minutes': round(minutes, 2), 'timed_out': outcome == 'limit', 'finished': outcome == 'done',
           'outcome': outcome, 'open_tasks': len(open_tasks), 'owner_turns': goes}
    chat = wsx_json(wsx, env, arm_dir, 'chat', 'read')
    (evidence / 'chat.json').write_text(json.dumps(chat, indent=1))
    replies = [e.get('text', '') for e in chat.get('entries', []) if e.get('kind') == 'reply']
    row['lead_last_reply'] = replies[-1][:400] if replies else None
    row['open_asks'] = len(chat.get('asks') or [])
    row.update(desk_usage(state, evidence))
    if row.get('tasks') == 0:
        row['finished'] = False  # the lead answered and started no task: nothing was built
    # ^ Landed means the default branch moved: judge main as it is, not a task copy.
    git('reset', '-q', '--hard', 'main', cwd=arm_dir, check=False)
    row['diff'] = diff_size(arm_dir, case['parent'], 'main')
    return row, arm_dir


def task_items(listing):
    return listing.get('tasks', []) if isinstance(listing, dict) else listing


def find_key(obj, key):
    if isinstance(obj, dict):
        if key in obj:
            return obj[key]
        for value in obj.values():
            found = find_key(value, key)
            if found is not None:
                return found
    return None


OWNER_GO = 'Yes, go ahead: do it as you proposed, and decide the open points yourself.'


def owner_should_answer(wsx, env, cwd):
    """Whether the owner would answer now: a question of the lead is open, or no task was ever started."""
    try:
        chat = wsx_json(wsx, env, cwd, 'chat', 'read')
        tasks = task_items(wsx_json(wsx, env, cwd, 'task', 'list', '--all'))
    except (RuntimeError, ValueError, subprocess.TimeoutExpired):
        return False
    return bool(chat.get('asks')) or not tasks


def desk_state(wsx, env, cwd, said_id):
    """'done': the lead answered and no task is open; 'owner': only the owner can move things on (an open
    approval or question, or every open task waits on the owner or failed); 'busy' otherwise, also when one
    command fails."""
    try:
        tasks = task_items(wsx_json(wsx, env, cwd, 'task', 'list'))
        chat = wsx_json(wsx, env, cwd, 'chat', 'read')
        open_tasks = [t for t in tasks if t.get('state') not in DONE_TASK]
        stuck = [t for t in open_tasks if t.get('state') == 'failed' or (
            t.get('state') == 'waiting' and (find_key(wsx_json(wsx, env, cwd, 'task', 'show', str(t['id'])),
                                                     'waiting') or {}).get('on') in OWNER_WAIT)]
    except (RuntimeError, ValueError, subprocess.TimeoutExpired):
        return 'busy'
    answered = any(e.get('kind') == 'reply' and said_id in e.get('to', []) for e in chat.get('entries', []))
    if not answered:
        return 'busy'
    if not open_tasks:
        return 'done'
    running = [t for t in open_tasks if t.get('state') in ('working', 'checking')]
    if not running and (chat.get('approvals') or len(stuck) == len(open_tasks)):
        return 'owner'
    return 'busy'


def desk_usage(state, evidence):
    """Tokens and cost of the desk run from `scripts/usage-report.py` (its text is kept in evidence/) and the
    logs it reads: workers' `result` usage and the lead's replies."""
    folders = [p for p in (state / 'wsx/tasks').glob('*') if p.is_dir() and p.name != 'chats']
    if not folders:
        return {'tokens': None, 'cost_usd': None, 'error': 'no task state folder'}
    report = subprocess.run([sys.executable, '-B', str(USAGE_REPORT), *map(str, folders)], capture_output=True,
                            text=True)
    (evidence / 'usage-report.txt').write_text(report.stdout + report.stderr)
    return usage_from_logs(state / 'wsx/tasks', folders)


def usage_from_logs(tasks_dir, folders):
    totals = {'input': 0, 'output': 0, 'cache_write': 0, 'cache_read': 0}
    cost_micro, turns, tasks = 0, 0, 0
    files = []
    for folder in folders:
        files += [p for p in folder.glob('*/log*.jsonl')]
    files += [p for p in (tasks_dir / 'chats').glob('*/log*.jsonl')]
    for path in files:
        for line in path.read_text(errors='replace').splitlines():
            try:
                e = json.loads(line)
            except ValueError:
                continue
            u = e.get('usage') if isinstance(e, dict) and e.get('kind') in ('result', 'reply', 'review') else None
            if not isinstance(u, dict):
                continue
            totals['input'] += u.get('input_tokens', 0)
            totals['output'] += u.get('output_tokens', 0)
            totals['cache_write'] += u.get('cache_creation_tokens', 0)
            totals['cache_read'] += u.get('cache_read_tokens', 0)
            cost_micro += u.get('cost_micro_usd', 0)
            turns += 1
    tasks = sum(1 for f in folders for p in f.glob('*/task.json'))
    totals['total'] = sum(totals.values())
    return {'tokens': totals, 'cost_usd': round(cost_micro / 1e6, 4), 'turns': turns, 'tasks': tasks}


# ---- judge -------------------------------------------------------------------------------------------------

SUMMARY = re.compile(r'(\d+) tests? run: (\d+) passed(?: \([^)]*\))?(?:, (\d+) failed)?')


def judge(case, tree, args):
    """Copy the hidden tests from the commit into the final tree and run each test file, outside the arm."""
    for rel in case['tests']:
        body = git('show', '%s:%s' % (case['sha'], rel)).stdout
        dest = tree / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(body)
    env = clean_env({'FAKE_CLAUDE_TRACE': str(tree.parent / 'judge.trace')})
    files = {}
    for crate, name in case['targets']:
        done = subprocess.run(['cargo', 'nextest', 'run', '-p', crate, '--test', name, '--locked',
                               '--no-fail-fast'], cwd=tree, env=env, capture_output=True, text=True,
                              timeout=args.test_minutes * 60)
        text = done.stdout + done.stderr
        found = SUMMARY.search(text)
        passed, failed = (int(found.group(2)), int(found.group(3) or 0)) if found else (0, 0)
        files['%s/%s' % (crate, name)] = {'passed': passed, 'failed': failed, 'built': bool(found),
                                          'exit': done.returncode,
                                          'tail': '' if done.returncode == 0 else text.strip()[-300:]}
    ok = all(f['built'] and f['passed'] > 0 and f['failed'] == 0 and f['exit'] == 0 for f in files.values())
    return {'files': files, 'passed': ok}


# ---- main --------------------------------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--cases', type=int, default=10, help='the N most recent eligible commits (default 10)')
    ap.add_argument('--case', help='one commit (sha or ref) instead of --cases')
    ap.add_argument('--ref', default='main', help='history to take cases from (default main)')
    ap.add_argument('--arms', default='solo,desk', help='comma list (default solo,desk)')
    ap.add_argument('--minutes', type=float, default=40, help='wall limit of one arm (default 40)')
    ap.add_argument('--owner-turns', type=int, default=2, help='desk: times the owner answers a question or a reply that started nothing with a plain go (default 2)')
    ap.add_argument('--test-minutes', type=float, default=45, help='wall limit of one judged test file')
    ap.add_argument('--model', help='claude model for solo and for every desk agent (default: each one\'s own)')
    ap.add_argument('--desk-check', action='append', help='desk check command, repeatable; {crate} is the case crate')
    ap.add_argument('--wsx', default=str(ROOT / 'target/debug/wsx'))
    ap.add_argument('--no-build', action='store_true', help='skip `cargo build -p wsx -p wsx-daemon`')
    ap.add_argument('--cold', action='store_true', help='do not clone target/ into each arm')
    ap.add_argument('--dry-run', action='store_true', help='fake agent, no model call')
    ap.add_argument('--keep', action='store_true', help='keep the run folder (arms and isolated wsx) for a look')
    args = ap.parse_args()
    global KEEP
    KEEP = args.keep
    arms = args.arms.split(',')
    if not set(arms) <= {'solo', 'desk'}:
        raise SystemExit('eval-desk: --arms takes solo and desk')
    cases = pick_cases(args)
    run_id = time.strftime('%y%m%d-%H%M%S') + ('d' if args.dry_run else '')
    out = ROOT / '.work/eval' / run_id
    out.mkdir(parents=True)
    results = out / 'results.jsonl'
    signal.signal(signal.SIGTERM, on_signal)
    rows = []
    try:
        if not args.no_build:
            run(['cargo', 'build', '-q', '-p', 'wsx', '-p', 'wsx-daemon'], cwd=ROOT, timeout=3600)
        if not Path(args.wsx).is_file() or not Path(args.wsx).with_name('wsxd').is_file():
            raise RuntimeError('build adjacent wsx and wsxd first')
        log('run', run_id, 'cases', ' '.join(c['short'] for c in cases))
        for case in cases:
            for arm in arms:
                folder = out / case['short']
                folder.mkdir(exist_ok=True)
                evidence = out / 'evidence' / case['short'] / arm
                evidence.mkdir(parents=True)
                arm_dir = folder / arm
                row = {'run': run_id, 'case': case['short'], 'arm': arm, 'dry_run': args.dry_run,
                       'request': case['request'].splitlines()[0][:120], 'parent': case['parent'][:8]}
                mark = len(CLEANUPS)
                log('case', case['short'], 'arm', arm)
                try:
                    body, tree = (arm_solo if arm == 'solo' else arm_desk)(case, arm_dir, args, evidence)
                    row.update(body)
                    row['tests'] = judge(case, tree, args)
                    row['passed'] = row['tests']['passed']
                except KeyboardInterrupt:
                    raise
                except Exception as err:
                    row.update({'error': str(err)[:500], 'passed': False})
                    log('arm failed:', err)
                finally:
                    run_cleanups(mark)
                    for proc in list(STARTED):
                        stop_group(proc)
                with results.open('a') as fh:
                    fh.write(json.dumps(row) + '\n')
                rows.append(row)
                log('  ->', 'pass' if row.get('passed') else 'fail', row.get('minutes'), 'min')
    except KeyboardInterrupt:
        log('interrupted; cleaning up')
    finally:
        cleanup_all()
    print(summary(rows))
    print('results:', results)


def median(values):
    values = [v for v in values if v is not None]
    return round(statistics.median(values), 2) if values else '-'


def summary(rows):
    lines = ['| arm | cases | pass | pass rate | finished | median min | median tokens | median cost $ |',
             '|---|---|---|---|---|---|---|---|']
    for arm in ('solo', 'desk'):
        mine = [r for r in rows if r['arm'] == arm]
        if not mine:
            continue
        passed = sum(1 for r in mine if r.get('passed'))
        lines.append('| %s | %d | %d | %.0f%% | %d | %s | %s | %s |' % (
            arm, len(mine), passed, 100.0 * passed / len(mine), sum(1 for r in mine if r.get('finished')),
            median(r.get('minutes') for r in mine),
            median((r.get('tokens') or {}).get('total') for r in mine),
            median(r.get('cost_usd') for r in mine)))
    return '\n'.join(lines)


if __name__ == '__main__':
    main()
