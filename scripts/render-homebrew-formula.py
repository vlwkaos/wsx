#!/usr/bin/env python3
"""Render the wsx Homebrew formula used by release automation."""

import argparse
from pathlib import Path
import re

VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--archive-sha", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    if not VERSION.fullmatch(args.version):
        parser.error("--version must be stable SemVer without a v prefix")
    if not SHA256.fullmatch(args.archive_sha):
        parser.error("--archive-sha must be a lowercase SHA-256 digest")
    if not args.output.parent.is_dir():
        parser.error("--output parent must already exist")
    if args.output.is_symlink() or (args.output.exists() and not args.output.is_file()):
        parser.error("--output must be a regular file or a new path")

    archive = f"wsx-{args.version}-darwin-universal.tar.gz"
    # ^ https://docs.brew.sh/Bottles: brew bottle later adds native bottle metadata.
    formula = f'''class Wsx < Formula
  desc "Project-first terminal workspace manager for Git worktrees"
  homepage "https://github.com/vlwkaos/wsx"
  url "https://github.com/vlwkaos/wsx/releases/download/v{args.version}/{archive}"
  sha256 "{args.archive_sha}"
  license "MIT"

  def install
    bin.install "wsx", "wsxd"
  end

  test do
    assert_predicate bin/"wsx", :executable?
    assert_predicate bin/"wsxd", :executable?
  end
end
'''
    args.output.write_text(formula)


if __name__ == "__main__":
    main()
