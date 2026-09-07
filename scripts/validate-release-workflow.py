#!/usr/bin/env python3
"""Validate release recovery ordering without requiring Homebrew."""

from pathlib import Path


workflow = Path(".github/workflows/release.yml").read_text()


def section(start: str, end: str | None = None) -> str:
    begin = workflow.index(start)
    finish = workflow.index(end, begin) if end else len(workflow)
    return workflow[begin:finish]


def require_before(text: str, first: str, second: str) -> None:
    assert first in text, f"missing {first!r}"
    assert second in text, f"missing {second!r}"
    assert text.index(first) < text.index(second), f"{first!r} must precede {second!r}"


recovery = section("  recovery-assets:", "  build-macos:")
bottles = section("  build-bottles:", "  homebrew:")
prepare = section("      - name: Prepare formula", "      - name: Build and test bottle")
final = section("  homebrew:")

assert '"${ARCHIVE}.sha256"' in recovery
assert "sha256sum --check SHA256SUMS" not in recovery
assert "needs: [publish-core, recovery-assets]" in bottles
assert "needs.recovery-assets.result == 'success'" in bottles
require_before(prepare, "brew trust vlwkaos/tap", "render-homebrew-formula.py")
require_before(bottles, "brew trust vlwkaos/tap", "brew install --build-bottle")
assert "needs.build-bottles.result == 'success'" in final
for command in ("brew bottle --merge", "brew style", "brew audit --strict"):
    require_before(final, "brew trust vlwkaos/tap", command)
require_before(final, ".local_filename", 'mv "bottle-assets/$LOCAL_FILENAME" "bottle-assets/$FILENAME"')
assert ".filename" in final
assert "cat release-assets/SHA256SUMS" not in final
assert "(cd release-assets && shasum -a 256 wsx-*-darwin-universal.tar.gz)" in final
assert "(cd bottle-assets && shasum -a 256 *.bottle.tar.gz)" in final

print("release recovery workflow: PASS")
