#!/usr/bin/env python3
"""Guard the terminal-context operator's preparation-failure cleanup boundary.

Git is the only injected external failure. Real fixture I/O and the public main
entry run normally; no daemon or terminal is started. The full TUI scenario
covers the successful runtime lifecycle separately.
"""
import importlib.util
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("terminal_context", ROOT / "scripts/test-terminal-context.py")
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)


class PreparationCleanup(unittest.TestCase):
    def invoke(self, root, keep=False):
        binaries = root / "bin"
        binaries.mkdir()
        wsx, wsxd = binaries / "wsx", binaries / "wsxd"
        wsx.write_text("fixture executable is not run\n")
        wsxd.write_text("fixture executable is not run\n")
        argv = ["test-terminal-context.py", "--wsx", str(wsx), "--daemon", str(wsxd)]
        if keep:
            argv.append("--keep")

        def git_failure(argv, **kwargs):
            self.assertEqual(argv[:3], ["git", "init", "-q"])
            raise subprocess.CalledProcessError(1, argv)

        with mock.patch.object(harness, "ROOT", root), mock.patch.object(sys, "argv", argv), \
                mock.patch.object(harness.shutil, "which", return_value="/fixture/tmux"), \
                mock.patch.object(harness.subprocess, "run", side_effect=git_failure), \
                mock.patch.object(harness.subprocess, "Popen", side_effect=AssertionError("runtime must not start")):
            harness.main()

    def test_failed_preparation_cleans_owned_scratch_unless_keep_is_explicit(self):
        ROOT.joinpath(".work").mkdir(exist_ok=True)
        for keep in (False, True):
            with self.subTest(keep=keep), tempfile.TemporaryDirectory(prefix="tc-prep-", dir=ROOT / ".work") as directory:
                root = Path(directory)
                owned = root / ".work" / ("tc-" + str(os.getpid()))
                with self.assertRaises(subprocess.CalledProcessError):
                    self.invoke(root, keep)
                self.assertEqual(owned.exists(), keep, "failed Git init leaked default scratch or erased requested diagnostics")
                if keep:
                    self.assertTrue((owned / "project").is_dir())
                    self.assertEqual(owned.stat().st_mode & 0o777, 0o700)

    def test_existing_fixture_is_refused_without_deleting_another_runs_data(self):
        ROOT.joinpath(".work").mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="tc-prep-", dir=ROOT / ".work") as directory:
            root = Path(directory)
            existing = root / ".work" / ("tc-" + str(os.getpid()))
            existing.mkdir(parents=True)
            marker = existing / "prior-run.txt"
            marker.write_text("preserve\n")
            with self.assertRaises(FileExistsError):
                self.invoke(root)
            self.assertEqual(marker.read_text(), "preserve\n")


if __name__ == "__main__":
    unittest.main()
