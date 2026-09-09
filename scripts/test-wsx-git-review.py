#!/usr/bin/env python3
"""Contract tests for the external Git review provider."""
import importlib.util
import pathlib
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("git_review", ROOT / "scripts/wsx-git-review.py")
PROVIDER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROVIDER)


class GitReviewTests(unittest.TestCase):
    def setUp(self):
        scratch = ROOT / ".work"
        scratch.mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="review-test-", dir=str(scratch))
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        (self.root / "file.txt").write_text("before\n")
        self.git("add", "file.txt")
        self.git("commit", "-qm", "fixture")

    def git(self, *args):
        subprocess.run(["git", "-C", str(self.root), *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def request(self, **extra):
        return dict(api_version=1, worktree_path=str(self.root), comparison="working_against_head",
                    limits=dict(files=1000, hunks=256, lines=10000), **extra)

    def test_changed_file_diff_and_stale_snapshot(self):
        (self.root / "file.txt").write_text("after\n")
        kind, listing = PROVIDER.review(self.request(operation="list_files"))
        self.assertEqual(kind, "files")
        self.assertEqual(listing["files"][0]["file_id"], "file.txt")
        request = self.request(operation="file_diff", snapshot=listing["snapshot"], file_id="file.txt")
        kind, diff = PROVIDER.review(request)
        self.assertEqual(kind, "diff")
        self.assertIn({"kind": "addition", "text": "after"}, diff["hunks"][0]["lines"])
        (self.root / "file.txt").write_text("newer\n")
        with self.assertRaises(PROVIDER.ReviewError) as error:
            PROVIDER.review(request)
        self.assertEqual(error.exception.code, "stale_snapshot")

    def test_untracked_missing_final_newline(self):
        (self.root / "new.txt").write_text("new")
        _, listing = PROVIDER.review(self.request(operation="list_files"))
        _, diff = PROVIDER.review(self.request(operation="file_diff", snapshot=listing["snapshot"], file_id="new.txt"))
        self.assertEqual(diff["hunks"][0]["lines"][-1], {"kind": "no_newline"})

    def test_symlink_does_not_read_target(self):
        (self.root / "link.txt").symlink_to("file.txt")
        _, listing = PROVIDER.review(self.request(operation="list_files"))
        self.assertEqual(listing["files"][0]["content_kind"], "unreadable")

    def test_oversized_file_is_explicit_and_changes_invalidate_snapshot(self):
        path = self.root / "large.txt"
        path.write_bytes(b"a" * (PROVIDER.LIMIT + 1))
        _, first = PROVIDER.review(self.request(operation="list_files"))
        self.assertEqual(first["files"][0]["content_kind"], "oversized")
        path.write_bytes(b"b" * (PROVIDER.LIMIT + 2))
        _, second = PROVIDER.review(self.request(operation="list_files"))
        self.assertNotEqual(first["snapshot"], second["snapshot"])


if __name__ == "__main__":
    unittest.main()
