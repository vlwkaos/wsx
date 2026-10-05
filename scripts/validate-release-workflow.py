#!/usr/bin/env python3
"""Validate release recovery ordering without requiring Homebrew."""

from pathlib import Path
from typing import Optional


workflow = Path(".github/workflows/release.yml").read_text()


def section(start: str, end: Optional[str] = None) -> str:
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
pour = section("      - name: Pour and verify generated bottle", "      - name: Upload bottle artifact")
require_before(bottles, "brew bottle --json", "      - name: Pour and verify generated bottle")
require_before(pour, "brew bottle --merge --write --no-commit", "brew --cache --force-bottle")
require_before(pour, "brew --cache --force-bottle", 'cp "${BOTTLES[0]}" "$CACHE"')
require_before(pour, 'cp "${BOTTLES[0]}" "$CACHE"', "brew uninstall --formula")
require_before(pour, "brew uninstall --formula", "brew install --force-bottle vlwkaos/homebrew-tap/wsx")
require_before(pour, "brew install --force-bottle vlwkaos/homebrew-tap/wsx", ".poured_from_bottle == true")
assert 'brew install --force-bottle "${BOTTLES[0]}"' not in pour
assert pour.count('shasum -a 256 -c -') == 2
assert 'test ! -L "$CACHE"' in pour
for bypass in ("HOMEBREW_DEVELOPER=", "HOMEBREW_INTERNAL_ALLOW_PACKAGES_FROM_PATHS="):
    assert bypass not in pour
require_before(pour, ".poured_from_bottle == true", "brew test vlwkaos/homebrew-tap/wsx")
assert '"$PREFIX/bin/wsx" --version' in pour
for binary in ("wsx", "wsxd"):
    assert f'lipo "$PREFIX/bin/{binary}" -verify_arch arm64 x86_64' in pour
assert "needs.build-bottles.result == 'success'" in final
for command in ("brew bottle --merge", "brew style", "brew audit --strict"):
    require_before(final, "brew trust vlwkaos/tap", command)
require_before(final, ".local_filename", 'mv "bottle-assets/$LOCAL_FILENAME" "bottle-assets/$FILENAME"')
assert ".filename" in final
assert "cat release-assets/SHA256SUMS" not in final
assert "(cd release-assets && shasum -a 256 wsx-*-darwin-universal.tar.gz)" in final
assert "(cd bottle-assets && shasum -a 256 *.bottle.tar.gz)" in final

print("release recovery and native bottle-pour workflow: PASS")
