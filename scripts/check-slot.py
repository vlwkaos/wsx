#!/usr/bin/env python3
"""Run a command in the host-wide check slot that task checks take, so it never runs beside
another task's checks: an flock on tasks/check-slot/slot.lock (crates/wsx/src/task/slot.rs).
Usage: python3 -B scripts/check-slot.py <command> [<arg>...]"""
import fcntl
import os
import subprocess
import sys
import tempfile

# ^ runtime::state_dir (docs/daemon-discovery.md): an empty or relative value is ignored.
state, home = os.environ.get("XDG_STATE_HOME", ""), os.environ.get("HOME", "")
state = state if os.path.isabs(state) else os.path.join(home, ".local/state") if os.path.isabs(home) else tempfile.gettempdir()
folder = os.path.join(state, "wsx", "tasks", "check-slot")
os.makedirs(folder, mode=0o700, exist_ok=True)
slot = open(os.path.join(folder, "slot.lock"), "a")
try:
    fcntl.flock(slot, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    print("waiting for the check slot", flush=True)
    fcntl.flock(slot, fcntl.LOCK_EX)
sys.exit(subprocess.call(sys.argv[1:]))
