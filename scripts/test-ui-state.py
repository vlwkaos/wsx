#!/usr/bin/env python3
"""Verify UI intent persistence and outer-terminal ownership with real private PTYs.

Build adjacent wsx/wsxd and the wsx test executable first. This scenario never
reads the user's config, uses the user's daemon, or invokes a model.
"""
import argparse
import base64
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import socket
import struct
import subprocess
import sys
import termios
import time

ROOT = Path(__file__).resolve().parents[1]
ENABLE = b'\x1b[?1006h'
DISABLE = b'\x1b[?1006l'


class View:
    def __init__(self, command, env, rows=24, cols=100, failed_stdout=False, stderr_file=None):
        self.master, self.slave = pty.openpty()
        self.output = bytearray()
        self.rows, self.cols = rows, cols
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        self.original_attributes = termios.tcgetattr(self.slave)
        broken = None
        if failed_stdout:
            # ^ Stdout suppresses EBADF; a readerless pipe fails with EPIPE after raw mode starts.
            read_end, broken = os.pipe()
            os.close(read_end)
        try:
            self.proc = subprocess.Popen(command, env=env, stdin=self.slave,
                                         stdout=broken if broken is not None else self.slave,
                                         stderr=self.slave if stderr_file is None else stderr_file,
                                         start_new_session=True)
        except BaseException:
            os.close(self.master)
            os.close(self.slave)
            raise
        finally:
            if broken is not None:
                os.close(broken)

    def drain(self, seconds=0.05):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], min(0.02, max(0, deadline-time.monotonic())))[0]:
                try:
                    data = os.read(self.master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not data:
                    break
                self.output.extend(data)

    def send(self, data):
        os.write(self.master, data)

    def raw(self):
        return not bool(termios.tcgetattr(self.slave)[3] & termios.ICANON)

    def close(self, quit_key=b'\x01q'):
        forced = False
        if self.proc.poll() is None:
            self.send(quit_key)
            self.drain(0.2)
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                forced = True
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=3)
        self.drain()
        restored = termios.tcgetattr(self.slave)
        expected = list(self.original_attributes)
        if sys.platform == 'darwin':
            # ^ XNU tty.c ttioctl_locked adds/preserves PENDIN on ICANON return; it is driver state, not configuration.
            # https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c
            expected[3] &= ~termios.PENDIN
            restored[3] &= ~termios.PENDIN
        self.attribute_difference = [(index, before, after) for index, (before, after)
                                     in enumerate(zip(expected, restored)) if before != after]
        clean_modes = not self.attribute_difference
        os.close(self.master)
        os.close(self.slave)
        return not forced and clean_modes


def wait(predicate, label, views=(), seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        for view in views:
            view.drain()
        if predicate():
            return
        time.sleep(0.02)
    raise RuntimeError(label + ' timeout')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--wsx', type=Path, default=ROOT/'target/debug/wsx')
    parser.add_argument('--daemon', type=Path, default=ROOT/'target/debug/wsxd')
    parser.add_argument('--test-binary', type=Path, required=True)
    parser.add_argument('--render-binary', type=Path, required=True)
    args = parser.parse_args()
    wsx, wsxd, test_binary = args.wsx.resolve(), args.daemon.resolve(), args.test_binary.resolve()
    render_binary = args.render_binary.resolve()
    if wsx.parent != wsxd.parent or not all(path.is_file() for path in (wsx, wsxd, test_binary, render_binary)):
        raise RuntimeError('fresh adjacent binaries and the compiled wsx test executable are required')
    work = ROOT/'.work'/('us-'+str(os.getpid()))
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    receipt = ROOT/'.work'/('ui-state-'+str(os.getpid())+'.json')
    report = {'version': subprocess.check_output([str(wsx), '--version'], text=True).strip(),
              'model_calls': 0, 'scenario': 'private PTYs and two client windows'}
    views, completed = [], []
    daemon = None
    started = time.monotonic()
    env = None
    try:
        home, state, project = work/'h', work/'s', work/'p'
        for path in (home, state, project):
            path.mkdir(mode=0o700)
        config_dir = home/'Library/Application Support/wsx' if sys.platform == 'darwin' else work/'c/wsx'
        cache_dir = home/'Library/Caches/wsx' if sys.platform == 'darwin' else work/'cache/wsx'
        config_dir.mkdir(parents=True)
        cache_dir.mkdir(parents=True)
        config_file, cache_file = config_dir/'config-v2.toml', cache_dir/'workspace-v3.toml'
        key = json.dumps(str(project))
        config = ('resume_agents_on_restore = false\nshow_release_status = false\nwake_mode = false\n'
                  'terminal_title_position = "bottom"\n[[projects]]\nname = "probe"\npath = '+key+
                  '\n[auto_collapse]\nmode = "flat"\nhours = 24\n')
        config_file.write_text(config)
        env = {'HOME': str(home), 'XDG_STATE_HOME': str(state), 'XDG_CONFIG_HOME': str(work/'c'),
               'XDG_CACHE_HOME': str(work/'cache'), 'WSX_SOCKET': str(state/'wsx/wsx.sock'),
               'WSX_DAEMON_BIN': str(wsxd), 'SHELL': '/bin/sh', 'TERM': 'xterm-256color',
               'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'LANG': 'en_US.UTF-8', 'TMPDIR': str(work)}
        subprocess.run(['git', 'init', '-q', '-b', 'main', str(project)], env=env, check=True)

        def call(method, params=None):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(5)
                client.connect(env['WSX_SOCKET'])
                reader = client.makefile('rb')
                client.sendall(b'{"method":"hello","params":{"protocol":16}}\n')
                assert json.loads(reader.readline())['type'] == 'hello'
                request = {'method': method}
                if params is not None:
                    request['params'] = params
                client.sendall(json.dumps(request).encode()+b'\n')
                return json.loads(reader.readline())

        def view(command=None, marker=b'WORKSPACE', **kwargs):
            item = View(command or [str(wsx), '--mobile'], env, **kwargs)
            views.append(item)
            wait(lambda: marker in item.output, 'startup '+marker.decode(), [item])
            return item

        def verify_render(item, name, expected):
            item.drain()
            replay = work/(name+'.ansi')
            replay.write_bytes(item.output)
            probe_env = dict(env, WSX_RENDER_REPLAY=str(replay), WSX_RENDER_EXPECTED=expected,
                             WSX_RENDER_ROWS=str(item.rows), WSX_RENDER_COLS=str(item.cols))
            subprocess.run([str(render_binary),'tests::rendered_output_probe',
                            '--ignored','--exact','--nocapture'], env=probe_env, check=True, timeout=6)
            for suffix in ('screen.txt','frame.txt'):
                shutil.copyfile(replay.with_suffix('.'+suffix),
                                ROOT/'.work'/('ui-state-'+str(os.getpid())+'-'+name+'.'+suffix))
            return replay.with_suffix('.screen.txt').read_text()

        def verify_activity(item, name):
            # ^ Wait for runtime projection in decoded cells; raw PTY writes interleave escapes.
            deadline = time.monotonic() + 8
            while True:
                screen = verify_render(item, name, 'WORKSPACE')
                if re.search(r'[◎◉●] 1(?:\s|$)', screen):
                    assert not re.search(r'\bactive\b', screen), 'redundant activity text remains'
                    return screen
                if time.monotonic() >= deadline:
                    raise RuntimeError('compact activity missing after runtime projection: ' + screen)
                item.drain(.1)

        def close(item, key=b'\x01q', expected_clean=True):
            clean = item.close(key)
            views.remove(item)
            completed.append(item.proc)
            if expected_clean:
                assert clean, 'child exit did not restore terminal modes: '+repr(item.attribute_difference)

        def stale():
            return bool(re.search(r'^stale_collapsed_projects = \[[^\n]+\]', cache_file.read_text(), re.M))

        def expanded():
            return key+' = true' in cache_file.read_text().split('[project_expanded]', 1)[1].split('[', 1)[0]

        def seed():
            old = int(time.time()*1000) - 10*24*3600000
            cache_file.write_text('written_at_unix_ms = '+str(old)+'\nstale_collapsed_projects = ['+key+']\n'
                '[project_expanded]\n'+key+' = false\n[project_touched_unix_ms]\n'+key+' = '+str(old)+'\n')

        daemon = subprocess.Popen([str(wsxd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait(lambda: Path(env['WSX_SOCKET']).exists(), 'private daemon')
        for first_close in ('untouched', 'interacted'):
            seed()
            a, b = view(), view()
            a.send(b'\r')
            wait(lambda: not stale() and expanded(), 'A clear persisted', [a, b])
            b.send(b'R')
            b.drain(0.3)
            assert not stale() and expanded(), 'unrelated refresh overwrote newer intent'
            for item in ((b, a) if first_close == 'untouched' else (a, b)):
                close(item)
                assert not stale() and expanded(), 'quit overwrote newer intent'
            c = view()
            c.drain(0.2)
            assert b'stale' not in c.output and not stale() and expanded()
            # A later legitimate manual collapse still persists without becoming stale.
            c.send(b'h')
            wait(lambda: not expanded() and not stale(), 'manual collapse persisted', [c])
            close(c)
            c = view()
            assert not stale() and not expanded()
            close(c)
        report['multi_window_clear_refresh_quit_restart'] = 'passed in both exit orders'
        # Old binaries keep writing v2, but cannot touch the new intent owner after import.
        cache_file.unlink()
        legacy_file = cache_dir/'workspace-v2.toml'
        legacy_file.write_text('stale_collapsed_projects = ['+key+']\n[project_expanded]\n'+key+' = false\n')
        a = view()
        assert stale() and not expanded()
        a.send(b'\r')
        wait(lambda: not stale() and expanded(), 'imported flag cleared', [a])
        close(a)
        legacy_file.write_text('stale_collapsed_projects = ['+key+']\n[project_expanded]\n'+key+' = false\n')
        a = view()
        assert not stale() and expanded()
        close(a)
        report['one_time_import_and_old_writer_isolation'] = 'passed'

        # A failed quit must report unsaved intent, not claim successful persistence.
        saved_cache = cache_file.read_text()
        a = view()
        cache_file.write_text('broken = [')
        a.send(b'h')
        wait(lambda: b'Cache save failed' in a.output, 'pending intent save error', [a])
        a.send(b'q')
        wait(lambda: a.proc.poll() is not None, 'unsaved intent quit failure', [a])
        a.drain()
        assert a.proc.returncode != 0 and not a.raw() and DISABLE in a.output
        assert b'could not persist UI intent' in a.output
        assert cache_file.read_text() == 'broken = ['
        close(a)
        cache_file.write_text(saved_cache)
        report['failed_cache_save_quit_is_explicit_and_preserves_file'] = 'passed'

        # A real pane and the real TUI input/stream/selection/clipboard consumer chain.
        helper = work/'body.py'
        heartbeat = work/'heartbeat'
        generation_file = work/'runtime-generation'
        helper.write_text(
            "import os,pathlib,sys,threading,time\n"
            f"heartbeat=pathlib.Path({str(heartbeat)!r})\n"
            f"pathlib.Path({str(generation_file)!r}).write_text(os.environ['WSX_RUNTIME_GENERATION'])\n"
            "def tick():\n"
            " n=0\n"
            " while True:\n"
            "  n+=1; stage=heartbeat.with_suffix('.stage'); stage.write_text(str(n)); stage.replace(heartbeat); time.sleep(.05)\n"
            "threading.Thread(target=tick,daemon=True).start()\n"
            "print('SELECT_THIS_TEXT',flush=True)\nsys.stdin.read()\n")
        assert call('synchronize_projects', {'projects': [{'name': 'probe', 'path': str(project),
            'worktrees': [{'path': str(project), 'branch': 'main'}]}]})['type'] == 'ack'
        worktree_id = call('snapshot')['data']['worktrees'][0]['id']
        created = call('session_create', {'worktree_id': worktree_id, 'label': 'selection',
            'command': [sys.executable, str(helper)], 'rows': 21, 'cols': 100})
        assert created['type'] == 'created', created
        a = view()
        if not expanded():
            a.send(b'\r')
        wait(lambda: expanded(), 'expanded selection project', [a])
        wait(lambda: b'selection' in a.output, 'session row hydration', [a])
        a.send(b'jj\r')
        wait(lambda: b'SELECT_THIS_TEXT' in a.output, 'terminal baseline', [a])
        # ^ One-shot View omits controller-local selection; assert the stream's completed clipboard effect.
        a.send(b'\x1b[<0;1;2M\x1b[<32;8;2M\x1b[<0;8;2m')
        wait(lambda: b'\x1b]52;c;' in a.output, 'drag clipboard effect', [a])
        copied = base64.b64decode(re.findall(rb'\x1b\]52;c;([^\x07]*)\x07', a.output)[-1])
        assert copied == b'SELECT_T', copied
        close(a)
        report['tui_drag_to_daemon_selection_and_clipboard'] = 'passed'

        # Actual execution continues under project/worktree folds, not just a live PID flag.
        wait(lambda: heartbeat.exists() and generation_file.exists(), 'runtime heartbeat')
        pane = call('snapshot')['data']['panes'][0]
        generation = generation_file.read_text()
        assert call('agent_report', {'pane_id':pane['id'], 'runtime_generation':generation,
            'provider':'pi', 'state':'working', 'session_ref':{'kind':'id','value':'ui-state-fixture'},
            'capabilities':{'prompt':True,'lifecycle':True}})['type'] == 'ack'
        seed()
        a = view()
        active_screen = verify_activity(a, 'active-with-stale-provenance')
        assert not re.search(r'probe[^\n]*stale', active_screen), 'active project retained visible stale label'
        assert stale(), 'read-only activity projection rewrote stored provenance'
        narrow = view(cols=56)
        verify_activity(narrow, 'compact-activity-56')
        close(narrow)
        assert stale(), 'narrow read-only projection rewrote stored provenance'
        a.send(b'kkkh')
        wait(lambda: not expanded(), 'folded project', [a])
        try:
            verify_activity(a, 'folded-project')
        except RuntimeError:
            diagnostic = {'snapshot':call('snapshot')['data'], 'cache':cache_file.read_text(),
                          'output_tail':bytes(a.output[-12000:]).decode('utf-8','replace')}
            (ROOT/'.work'/('folded-status-failure-'+str(os.getpid())+'.json')).write_text(json.dumps(diagnostic,indent=2))
            raise
        before = int(heartbeat.read_text())
        wait(lambda: int(heartbeat.read_text()) >= before+5, 'work continues beneath folded project', [a])
        a.send(b'ljh')
        wait(lambda: expanded(), 'expanded parent with folded worktree', [a])
        a.drain(.2)
        verify_activity(a, 'folded-worktree')
        before = int(heartbeat.read_text())
        wait(lambda: int(heartbeat.read_text()) >= before+5, 'work continues beneath folded worktree', [a])
        after = call('snapshot')['data']['panes'][0]
        assert not after['exited'] and generation_file.read_text() == generation
        assert after['agent']['state'] == 'working'
        a.send(b'l')
        # ^ Keep the fixture within the daemon's 128-byte label limit but wider than the 100-column view.
        long_name = ('provider-label-long-name-' * 5)[:120]
        # ^ CLI scope inference must use the private project, not the harness repository.
        renamed = subprocess.run([str(wsx),'session','rename',str(created['data']['id']),long_name],
                       cwd=project,env=env,capture_output=True,text=True,timeout=5)
        assert renamed.returncode == 0, (renamed.returncode, renamed.stderr[-4096:])
        wait(lambda: b'provider-label' in a.output, 'long session-name update', [a])
        label_screen = verify_render(a, 'long-session-provider-label', '(pi)')
        assert any('provider-label' in line and '(pi)' in line for line in label_screen.splitlines())
        close(a)
        report['active_stale_exclusion_and_long_provider_identity'] = 'passed through real TUI frames with saved provenance unchanged'
        report['folded_status_and_background_execution'] = 'passed with unchanged runtime generation and advancing heartbeat'

        command = [str(test_binary), 'tui::tests::terminal_owner_probe', '--ignored', '--exact', '--nocapture']
        a = view(command, b'PROBE_READY')
        assert a.raw() and ENABLE in a.output
        mark = len(a.output)
        a.send(b'p')
        wait(lambda: b'PROBE_WORKER_REPORTED' in a.output[mark:], 'worker diagnostic', [a])
        assert a.raw() and DISABLE not in a.output[mark:] and ENABLE not in a.output[mark:]
        a.send(b'\x1b[<0;2;2M\x1b[<32;8;2M\x1b[<0;8;2m')
        wait(lambda: b'PROBE_MOUSE:Up(Left)' in a.output, 'mouse release after worker panic', [a])
        assert b'PROBE_MOUSE:Drag(Left)' in a.output and b'PROBE_MOUSE:Down(Left)' in a.output
        a.send(b'e')
        wait(lambda: b'PROBE_EDITOR_RETURNED' in a.output, 'failed editor returned', [a])
        assert a.raw() and DISABLE in a.output[mark:] and ENABLE in a.output[mark:]
        close(a, b'q')
        report['worker_failure_mouse_and_failed_editor'] = 'passed without re-enable workaround'
        a = view(command, b'PROBE_READY')
        a.send(b'x')
        wait(lambda: a.proc.poll() is not None, 'owner panic termination', [a])
        assert a.proc.returncode != 0 and DISABLE in a.output and not a.raw()
        close(a)
        with (work/'owner-stderr.log').open('wb') as redirected:
            a = view(command, b'PROBE_READY', stderr_file=redirected)
            a.send(b'x')
            wait(lambda: a.proc.poll() is not None, 'owner panic with redirected stderr', [a])
            assert a.proc.returncode != 0 and DISABLE in a.output and not a.raw()
            close(a)
        assert b'controlled owner failure' in (work/'owner-stderr.log').read_bytes()
        report['owner_panic_cleanup'] = 'passed with shared and redirected stderr'

        # Read errors after mode initialization and output errors during partial initialization.
        config_file.unlink()
        config_file.mkdir()
        a = View([str(wsx)], env)
        views.append(a)
        wait(lambda: a.proc.poll() is not None, 'App startup failure', [a])
        assert a.proc.returncode != 0 and ENABLE in a.output and DISABLE in a.output and not a.raw()
        close(a)
        config_file.rmdir()
        config_file.write_text(config)
        a = View([str(wsx)], env, failed_stdout=True)
        views.append(a)
        wait(lambda: a.proc.poll() is not None, 'partial terminal initialization failure', [a])
        assert a.proc.returncode != 0 and not a.raw()
        close(a)
        report['startup_and_partial_initialization_cleanup'] = 'passed'
        journal = cache_dir/'ui-terminal-diagnostics-v1.jsonl'
        assert journal.stat().st_mode & 0o777 == 0o600
        assert journal.stat().st_size <= 64*1024
        records = [json.loads(line) for line in journal.read_text().splitlines()]
        events = {record['event'] for record in records}
        assert {'owner_acquired','initialized','worker_panic_preserved_modes',
                'editor_suspended','editor_returned_with_error','owner_released',
                'owner_panic_released'} <= events, events
        allowed = {'time_unix_ms','pid','thread','event','output_identity','input_flags','error_os','source'}
        assert all(set(record) == allowed for record in records), records
        assert any(record['input_flags'] is not None for record in records)
        source_lines = (ROOT/'crates/wsx/src/tui.rs').read_text().splitlines()
        worker_sites = {index+1 for index, line in enumerate(source_lines)
                        if 'panic!("controlled worker failure")' in line or 'panic!("{payload}")' in line}
        owner_sites = {index+1 for index, line in enumerate(source_lines)
                       if 'panic!("controlled owner failure")' in line}
        for record in records:
            if record['event'] == 'worker_panic_preserved_modes':
                assert record['source']['file'].endswith('crates/wsx/src/tui.rs')
                assert record['source']['line'] in worker_sites, record
            if record['event'] == 'owner_panic_released':
                assert record['source']['line'] in owner_sites, record
        assert {'initial_output_modes_failed','app_start_failed','cache_flush_failed'} <= events, events
        retained = ROOT/'.work'/('ui-state-'+str(os.getpid())+'-mode-diagnostics.jsonl')
        retained.write_text(journal.read_text())
        report['bounded_ownership_diagnostics'] = 'passed with private metadata only; not an external-trigger diagnosis'
    finally:
        for item in views:
            item.close()
            completed.append(item.proc)
        if daemon and daemon.poll() is None:
            subprocess.run([str(wsx), 'daemon', 'stop'], env=env, capture_output=True, timeout=8)
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.terminate()
                daemon.wait(timeout=5)
        report['cleanup'] = {'tuis_reaped': all(proc.poll() is not None for proc in completed),
            'daemon_reaped': daemon is None or daemon.poll() is not None,
            'socket_removed': env is None or not Path(env['WSX_SOCKET']).exists()}
        report['elapsed_seconds'] = round(time.monotonic()-started, 3)
        receipt.write_text(json.dumps(report, indent=2)+'\n')
        # Remove only this run's exclusive synthetic HOME/state/project fixture.
        shutil.rmtree(work)
    assert all(report['cleanup'].values()), report
    print(json.dumps(report, indent=2))
    print('Receipt: '+str(receipt.relative_to(ROOT)))


if __name__ == '__main__':
    main()
