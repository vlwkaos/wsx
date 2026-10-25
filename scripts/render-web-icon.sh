#!/bin/sh
# Renders the default web theme's app icon from its SVG source to the 192 and 512 px PNGs
# the theme loader requires (docs/web-themes.md "Files"). Needs resvg (made with 0.48.1:
# `brew install resvg` or `cargo install resvg`); the SVG has no text, so no font is read.
set -eu
dir=$(cd "$(dirname "$0")/.." && pwd)/crates/wsx-web/web/themes/default
for size in 192 512; do
  resvg -w "$size" -h "$size" "$dir/icon.svg" "$dir/icon-$size.png"
done
