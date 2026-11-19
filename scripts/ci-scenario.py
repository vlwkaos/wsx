#!/usr/bin/env python3
"""Old name of scripts/scenario.py, kept while a running task host's project config still names it
(`ci: python3 -B scripts/ci-scenario.py task|web`). Builds, then runs the scenario's parts at once
on this machine. Delete it when no config names it. Usage: ci-scenario.py <task|web> [--part <part>] [--local]"""
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
args = [a for a in sys.argv[1:] if a != '--local']
build = subprocess.run(['cargo', 'build', '-p', 'wsx', '-p', 'wsx-daemon'], cwd=ROOT)
if build.returncode:
    sys.exit(build.returncode)
os.execv(sys.executable, [sys.executable, '-B', str(ROOT / 'scripts' / 'scenario.py'), *args])
