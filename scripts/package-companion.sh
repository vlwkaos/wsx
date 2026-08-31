#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TOOLCHAIN=1.96.1
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
VERSION=$(awk -F '"' '/^version =/ { print $2; exit }' "$ROOT/Cargo.toml")
ARCHIVE=${1:-"$ROOT/wsx-$VERSION-darwin-universal.tar.gz"}
STAGE="$ROOT/target/wsx-$VERSION-darwin-universal"

command -v rustup >/dev/null
command -v lipo >/dev/null
command -v shasum >/dev/null
ZIG_BIN=${ZIG:-}
if [[ -z "$ZIG_BIN" ]]; then
  if command -v zig >/dev/null; then
    ZIG_BIN=$(command -v zig)
  elif command -v brew >/dev/null && brew --prefix zig@0.15 >/dev/null; then
    ZIG_BIN=$(brew --prefix zig@0.15)/bin/zig
  else
    printf '%s\n' 'Zig 0.15 is required; set ZIG or install zig@0.15' >&2
    exit 1
  fi
fi
[[ $("$ZIG_BIN" version) == 0.15.* ]]
rustup run "$TOOLCHAIN" rustc --version >/dev/null

rm -rf "$STAGE"
mkdir -p "$STAGE"

for target in "${TARGETS[@]}"; do
  rustup target list --toolchain "$TOOLCHAIN" --installed | grep -Fx "$target" >/dev/null
  if [[ "$target" == aarch64-apple-darwin ]]; then
    deployment_target=11.0
  else
    deployment_target=10.15
  fi

  MACOSX_DEPLOYMENT_TARGET=$deployment_target \
    ZIG="$ZIG_BIN" \
    cargo +"$TOOLCHAIN" build --release --locked --target "$target" \
    --manifest-path "$ROOT/Cargo.toml" -p wsx -p wsx-daemon
done

lipo -create -output "$STAGE/wsx" \
  "$ROOT/target/aarch64-apple-darwin/release/wsx" \
  "$ROOT/target/x86_64-apple-darwin/release/wsx"
lipo -create -output "$STAGE/wsxd" \
  "$ROOT/target/aarch64-apple-darwin/release/wsxd" \
  "$ROOT/target/x86_64-apple-darwin/release/wsxd"

cp "$ROOT/LICENSE" "$STAGE/LICENSE-wsx"
cp "$ROOT/vendor/libghostty-vt/LICENSE" "$STAGE/LICENSE-libghostty-vt"
cp "$ROOT/vendor/portable-pty/LICENSE.md" "$STAGE/LICENSE-portable-pty.md"
cp "$ROOT/THIRD-PARTY-NOTICES.md" "$STAGE/THIRD-PARTY-NOTICES.md"

tar -czf "$ARCHIVE" -C "$STAGE" .
shasum -a 256 "$ARCHIVE"
