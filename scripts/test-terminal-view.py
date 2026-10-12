#!/usr/bin/env python3
"""Verify the pane view band and reader through wsxd, a real PTY, wsx and a private tmux.

Build adjacent wsx/wsxd first. The scenario uses its own HOME, config, state and daemon.
It never touches the user's daemon or configuration. See docs/terminal-view.md.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
PROTOCOL = 17
COLS, ROWS = 120, 30
DOCUMENT = """# Handoff plan

The successor takes **every** live PTY. See `docs/daemon-live-upgrade.md`.

| step | owner | 상태 |
| :--- | :---: | ---: |
| pause readers | wsxd | done |
| transfer lock | 후속 데몬 | 진행 중 |
| publish socket | successor | waiting |

- first item with a long tail that must wrap inside the band without touching the terminal rows below it
- second item

```text
┌────────┐   ┌────────┐
│ source │──▶│  next  │
└────────┘   └────────┘
```

""" + "\n".join("Filler paragraph %02d.\n" % index for index in range(1, 21))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wsx", type=Path, default=ROOT / "target/debug/wsx")
    parser.add_argument("--daemon", type=Path, default=ROOT / "target/debug/wsxd")
    parser.add_argument("--keep", action="store_true", help="retain private fixture and captures")
    parser.add_argument("--plugins", type=Path, help="installed wsx plugin directory to copy into the "
                        "fixture, for example one that holds the wsx-mermaid reference renderer")
    args = parser.parse_args()
    tmux = shutil.which("tmux")
    if not tmux:
        raise RuntimeError("tmux is required for actual TUI screen verification")
    wsx, wsxd = args.wsx.resolve(), args.daemon.resolve()
    if not wsx.is_file() or not wsxd.is_file() or wsx.parent != wsxd.parent:
        raise RuntimeError("build adjacent wsx and wsxd binaries before this scenario")
    work = ROOT / ".work" / ("tv-" + str(os.getpid()))
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    try:
        result = run_scenario(args, tmux, wsx, wsxd, work)
    finally:
        if not args.keep:
            shutil.rmtree(work)
    print(json.dumps(result))


def run_scenario(args, tmux, wsx, wsxd, work):
    home, state, project = work / "home", work / "state", work / "project"
    for path in (home, state, project, work / "captures"):
        path.mkdir(mode=0o700)
    config_dir = (home / "Library/Application Support/wsx" if sys.platform == "darwin"
                  else work / "config/wsx")
    config_dir.mkdir(parents=True)
    config = config_dir / "config-v2.toml"
    config_text = ('resume_agents_on_restore = false\nshow_release_status = false\n'
                   'terminal_prefix_shows_sidebar = false\nterminal_sidebar = "compact"\n'
                   'view_auto_open = {auto}\n'
                   '[[projects]]\nname = "demo"\npath = ' + json.dumps(str(project)) + '\n')
    config.write_text(config_text.format(auto="false"))
    # ^ Use an allowlist: fixture processes must not inherit credentials or pane authority.
    env = {"LANG": os.environ.get("LANG", "en_US.UTF-8")}
    env.update({"HOME": str(home), "XDG_STATE_HOME": str(state),
                "XDG_CONFIG_HOME": str(work / "config"), "XDG_CACHE_HOME": str(work / "cache"),
                "WSX_DAEMON_BIN": str(wsxd), "WSX_SOCKET": str(state / "wsx/wsx.sock"),
                "SHELL": "/bin/sh", "TERM": "xterm-256color",
                "PATH": "/usr/bin:/bin:/usr/sbin:/sbin"})
    subprocess.run(["git", "init", "-q", "-b", "main", str(project)], check=True, env=env)
    if args.plugins:
        shutil.copytree(args.plugins, work / "config/wsx/plugins", symlinks=True)
        os.chmod(work / "config/wsx", 0o700)
    daemon_socket, tmux_socket = state / "wsx/wsx.sock", work / "t.sock"
    # The actor records its pane authority, fills scrollback and echoes every input line.
    actor = work / "actor.py"
    actor.write_text(
        "import json,os,sys\n"
        "open(sys.argv[1],'w').write(json.dumps({key: os.environ[key] for key in "
        "('WSX_PANE_ID','WSX_RUNTIME_GENERATION')}))\n"
        "for index in range(1, 61): print('LINE %03d of the agent terminal' % index)\n"
        "sys.stdout.flush()\n"
        "for line in sys.stdin: print('ECHO:' + line.strip(), flush=True)\n"
    )

    def call(method, params=None):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(5)
            client.connect(str(daemon_socket))
            stream = client.makefile("rb")
            client.sendall(json.dumps({"method": "hello", "params": {"protocol": PROTOCOL}}).encode() + b"\n")
            assert json.loads(stream.readline())["type"] == "hello"
            request = {"method": method}
            if params is not None:
                request["params"] = params
            client.sendall(json.dumps(request).encode() + b"\n")
            return json.loads(stream.readline())

    def tm(*argv):
        return subprocess.run([tmux, "-S", str(tmux_socket)] + list(argv), check=True,
                              capture_output=True, text=True, env=env).stdout

    def screen(target="view"):
        rows = tm("capture-pane", "-p", "-t", target).splitlines()
        return rows + [""] * (ROWS - len(rows))

    def wait(test, label, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if test():
                return
            time.sleep(0.05)
        raise RuntimeError(label + " timeout")

    def capture(name, target="view"):
        rows = screen(target)
        (work / "captures" / (name + ".txt")).write_text("\n".join(rows))
        (work / "captures" / (name + ".ansi")).write_text(tm("capture-pane", "-p", "-e", "-t", target))
        return rows

    def keys(*argv, target="view"):
        tm("send-keys", "-t", target, *argv)

    def wheel(direction, column, row, target="view"):
        # SGR mouse report, as the outer terminal sends it. Coordinates are one-based.
        report = "\x1b[<%d;%d;%dM" % (64 if direction == "up" else 65, column + 1, row + 1)
        tm("send-keys", "-t", target, "-H", *["%02x" % byte for byte in report.encode()])

    def pane_dimensions(pane_id):
        reply = call("view", {"pane_ids": [pane_id]})
        assert reply["type"] == "view", reply
        frame = reply["data"]["frames"][0]
        return frame["cols"], frame["rows"]

    def publish(authority, arguments, document):
        return subprocess.run([str(wsx), "view", "set"] + arguments, input=document, text=True,
                              capture_output=True, env=env | authority, cwd=project)

    def status_row(rows):
        """Index and text of the band status row, or None when the band is closed."""
        for index, row in enumerate(rows[:-2]):
            if "▤" in row and ")close" in row:
                return index, row
        return None

    def position(row):
        match = re.search(r"(\d+)/(\d+)", row)
        assert match, row
        return int(match.group(1)), int(match.group(2))

    daemon = None
    succeeded = False
    started = time.monotonic()
    try:
        daemon = subprocess.Popen([str(wsxd)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait(lambda: daemon_socket.exists(), "daemon socket")
        assert call("synchronize_projects", {"projects": [{"name": "demo", "path": str(project),
                    "worktrees": [{"path": str(project), "branch": "main"}]}]})["type"] == "ack"
        worktree_id = call("snapshot")["data"]["worktrees"][0]["id"]
        authorities = {}
        for name in ("agent", "other"):
            created = call("session_create", {"worktree_id": worktree_id, "label": name,
                           "command": [sys.executable, str(actor), str(work / (name + ".json"))],
                           "rows": 27, "cols": 118})
            assert created["type"] == "created", created
            wait(lambda: (work / (name + ".json")).exists(), name + " actor")
            wait(lambda: (work / (name + ".json")).read_text().endswith("}"), name + " authority")
            authorities[name] = json.loads((work / (name + ".json")).read_text())
        agent_pane = int(authorities["agent"]["WSX_PANE_ID"])
        tm("new-session", "-d", "-s", "view", "-x", str(COLS), "-y", str(ROWS), "exec " + shlex.quote(str(wsx)))
        tm("set-option", "-g", "status", "off")
        wait(lambda: "agent" in "\n".join(screen()) and "WORKSPACE" in "\n".join(screen()), "Workspace")
        keys("j", "j", "Enter")
        wait(lambda: "TERMINAL" in "\n".join(screen()), "Terminal entry")
        wait(lambda: any("LINE 060" in row for row in screen()), "agent terminal content")
        dimensions = pane_dimensions(agent_pane)
        before = capture("01-terminal-without-view")
        title_index = len(before) - 2
        assert "▤" not in "\n".join(before), "chip shown without a view"

        # No view: the key reports that and changes nothing on screen.
        keys("C-a", "v")
        wait(lambda: "no view" in "\n".join(screen()), "no-view notice")
        # The transient notice covers one cell range of the last terminal row, as every notice does.
        assert screen()[1:title_index - 1] == before[1:title_index - 1], "no-view key changed the terminal"
        wait(lambda: screen()[1:title_index] == before[1:title_index], "notice expiry", timeout=20)

        # Publishing shows the chip only. The band stays closed: the agent cannot take the screen.
        published = publish(authorities["agent"], ["--title", "Handoff plan"], DOCUMENT)
        assert published.returncode == 0, published
        wait(lambda: "▤ view •" in screen()[title_index], "unread chip")
        chip_only = capture("02-chip-unread")
        assert status_row(chip_only) is None, "publishing opened the band without a key"
        assert chip_only[1:title_index] == before[1:title_index], "publishing changed terminal rows"

        # Prefix+v opens the band over the top rows; rows below it and the PTY stay as they were.
        keys("C-a", "v")
        wait(lambda: status_row(screen()) is not None, "band")
        wait(lambda: position(status_row(screen())[1])[1] > 20, "band document")
        band = capture("03-band")
        band_end, band_status = status_row(band)
        band_rows = band_end  # rows 1..band_end are the band; row 0 is the group header
        terminal_rows = title_index - 1
        assert band_rows <= terminal_rows // 3, (band_rows, terminal_rows)
        assert "Handoff plan" in band_status and "v)read" in band_status, band_status
        assert band[band_end + 1:title_index] == before[band_end + 1:title_index], "rows below the band changed"
        assert pane_dimensions(agent_pane) == dimensions, "opening the band resized the PTY"
        assert "▤ view" in band[title_index] and "•" not in band[title_index], band[title_index]
        shown, total = position(band_status)
        assert shown == band_rows - 1 and total > shown, band_status
        table = [row for row in band[1:band_end] if "│" in row and ("step" in row or "owner" in row)]
        assert table and "상태" in table[0], band[1:band_end]

        # Keys still reach the terminal while the band is open.
        keys("-l", "hello")
        keys("Enter")
        wait(lambda: any("ECHO:hello" in row for row in screen()[band_end + 1:title_index]), "key under band")
        typed = capture("04-band-typing")
        assert status_row(typed) == (band_end, band_status), "typing moved the band"

        # The wheel over the band scrolls the band only; the same wheel below it scrolls the terminal.
        below = typed[band_end + 1:title_index]
        wheel("down", 30, 3)
        wait(lambda: position(status_row(screen())[1])[0] > shown, "band wheel")
        scrolled = capture("05-band-scrolled")
        assert scrolled[band_end + 1:title_index] == below, "band wheel reached the terminal"
        assert scrolled[1:band_end] != band[1:band_end], "band content did not scroll"
        wheel("up", 30, title_index - 3)
        wait(lambda: screen()[band_end + 1:title_index] != below, "terminal wheel below the band")
        assert position(status_row(screen())[1]) == position(status_row(scrolled)[1]), "terminal wheel moved the band"
        wheel("down", 30, title_index - 3)
        wait(lambda: screen()[band_end + 1:title_index] == below, "terminal wheel back to the bottom")
        assert pane_dimensions(agent_pane) == dimensions

        # Prefix+v again opens the reader. Its keys move the document and never reach the terminal.
        second = publish(authorities["agent"], ["--page", "notes", "--title", "Notes"],
                         "Second page.\n\n```mermaid\nflowchart LR\n  A[Agent] --> B[wsxd]\n```\n")
        assert second.returncode == 0, second
        wait(lambda: "Notes" in (status_row(screen()) or (0, ""))[1], "newest page in band")
        keys("C-a", "v")
        wait(lambda: "(q)back" in screen()[title_index - 1], "reader")
        wait(lambda: any("Second page." in row for row in screen()), "reader document")
        reader = capture("06-reader")
        assert "Notes" in reader[1] and "page 1/2" in reader[1], reader[1]
        assert "([ ])page" in reader[title_index - 1], reader[title_index - 1]
        assert not any("LINE 0" in row for row in reader[1:title_index]), "reader left terminal rows visible"
        if args.plugins:
            # A renderer plugin replaces the mermaid source with box art.
            assert any("│ Agent ├" in row for row in reader) and "flowchart" not in "\n".join(reader), reader
        else:
            assert any("│ flowchart LR" in row for row in reader), "mermaid block is not shown as its source"
        keys("]")
        wait(lambda: "Handoff plan" in screen()[1] and "page 2/2" in screen()[1], "next page")
        wait(lambda: re.search(r"\d+/\d+", screen()[title_index - 1]), "page document")
        first_position = position(screen()[title_index - 1])
        keys("j")
        wait(lambda: position(screen()[title_index - 1])[0] == first_position[0] + 1, "reader j")
        keys("G")
        wait(lambda: position(screen()[title_index - 1])[0] == first_position[1], "reader G")
        capture("07-reader-end")
        keys("g")
        wait(lambda: position(screen()[title_index - 1]) == first_position, "reader g")
        keys("Space")
        wait(lambda: position(screen()[title_index - 1])[0] > first_position[0] + 1, "reader space")
        keys("k")
        keys("[")
        wait(lambda: "Notes" in screen()[1], "previous page")
        assert pane_dimensions(agent_pane) == dimensions, "reader resized the PTY"
        keys("q")
        wait(lambda: status_row(screen()) is not None, "reader q returns to the band")
        after_reader = capture("08-band-after-reader")
        echoes = [row for row in after_reader if "ECHO:" in row]
        assert [row.split("│")[-1].strip() for row in echoes] == ["ECHO:hello"], echoes

        # Prefix+V closes the band; the terminal is whole again and still the same size.
        keys("C-a", "V")
        wait(lambda: status_row(screen()) is None, "band close")
        closed = capture("09-closed")
        assert sum("LINE 0" in row for row in closed[1:title_index]) >= terminal_rows - 4, closed
        assert "▤ view" in closed[title_index], closed[title_index]
        assert pane_dimensions(agent_pane) == dimensions

        # Another session has no view: no chip there, and the key reports it.
        keys("C-a", "l")
        wait(lambda: "▤" not in screen()[title_index], "session without view")
        keys("C-a", "h")
        wait(lambda: "▤ view" in screen()[title_index], "session with view")

        # Clearing removes the chip.
        cleared = subprocess.run([str(wsx), "view", "clear"], capture_output=True, text=True,
                                 env=env | authorities["agent"], cwd=project)
        assert cleared.returncode == 0, cleared
        wait(lambda: "▤" not in screen()[title_index], "chip after clear")
        capture("10-cleared")

        # Opt-in auto-open: a second TUI with the option on opens the band, never the reader.
        config.write_text(config_text.format(auto="true"))
        tm("new-session", "-d", "-s", "auto", "-x", str(COLS), "-y", str(ROWS), "exec " + shlex.quote(str(wsx)))
        wait(lambda: "WORKSPACE" in "\n".join(screen("auto")), "second Workspace")
        # The first TUI's terminal entry left the project collapsed in the shared cache.
        keys("l", target="auto")
        wait(lambda: "agent" in "\n".join(screen("auto")), "second TUI project expansion")
        keys("j", "j", "Enter", target="auto")
        wait(lambda: "TERMINAL" in "\n".join(screen("auto")), "second Terminal entry")
        if not any("ECHO:hello" in row for row in screen("auto")):
            # Selection after expansion is not the contract here; reach the agent session.
            keys("C-a", "l", target="auto")
        wait(lambda: any("ECHO:hello" in row for row in screen("auto")), "agent session in second TUI")
        assert status_row(screen("auto")) is None
        again = publish(authorities["agent"], ["--title", "Auto"], "Opened by the option.\n")
        assert again.returncode == 0, again
        wait(lambda: status_row(screen("auto")) is not None, "auto-open band")
        auto = capture("11-auto-open", "auto")
        assert "(q)back" not in "\n".join(auto), "auto-open entered the reader"
        keys("-l", "auto-typed", target="auto")
        keys("Enter", target="auto")
        wait(lambda: any("ECHO:auto-typed" in row for row in screen("auto")), "keys after auto-open")
        assert pane_dimensions(agent_pane) == dimensions
        succeeded = True
    except Exception:
        if tmux_socket.exists():
            for target in ("view", "auto"):
                try:
                    rows = capture("failed-" + target, target)
                    print("last isolated " + target + " screen:\n" + "\n".join(rows), file=sys.stderr)
                except Exception:
                    pass
        raise
    finally:
        if tmux_socket.exists():
            subprocess.run([tmux, "-S", str(tmux_socket), "kill-server"], env=env, capture_output=True)
        if daemon_socket.exists():
            try:
                call("shutdown")
            except Exception:
                if daemon is not None:
                    daemon.terminate()
        if daemon is not None:
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=3)
        if succeeded:
            assert daemon.returncode == 0, "isolated wsxd did not stop gracefully"
            assert not daemon_socket.exists(), "isolated daemon socket survived shutdown"
    return {"result": "PASS", "elapsed_seconds": round(time.monotonic() - started, 2),
            "captures": str(work / "captures") if args.keep else None, "model_calls": 0,
            "pty": {"cols": dimensions[0], "rows": dimensions[1]},
            "cleanup": "private tmux, wsxd and PTYs stopped",
            "journey": "no view, unread chip, band, keys under band, wheel in and below band, "
                       "reader keys and pages, return, close, clear, opt-in auto-open"}


if __name__ == "__main__":
    main()
