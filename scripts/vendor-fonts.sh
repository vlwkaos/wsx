#!/bin/sh
# Rebuilds the terminal fonts in crates/wsx-web/web/vendor/fonts from pinned upstream
# releases (docs/web-client-terminal.md "Terminal fonts"):
#   JetBrains Mono 2.304 (SIL OFL 1.1): regular, bold, italic, as upstream woff2.
#   Symbols Nerd Font Mono 3.4.0 (MIT): upstream ttf, and a woff2 made from it here.
# The woff2 is the same tables in a WOFF2 container, compressed with Node's own
# Brotli and with no table transform, so no font tool is needed. The ttf stays as the
# second source of the @font-face rule for a browser that refuses that file.
set -eu

JETBRAINS_TAG=v2.304
NERD_TAG=v3.4.0
SUMS='a9cb1cd82332b23a47e3a1239d25d13c86d16c4220695e34b243effa999f45f2  JetBrainsMono-Regular.woff2
c503cc5ec5f8b2c7666b7ecda1adf44bd45f2e6579b2eba0fc292150416588a2  JetBrainsMono-Bold.woff2
cb6a1b246318ed3885d7dffa14a2609297fe80e9b8e500bea33b52fa312a36a4  JetBrainsMono-Italic.woff2
7f8c090da3b0eaa7108646bf34cbbb6ed13d5358a72460522108b06c7ecd716a  NerdFontsSymbolsOnly.tar.xz'

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/wsx-web/web/vendor/fonts"
work=$(mktemp -d "${TMPDIR:-/tmp}/vendor-fonts.XXXXXX")
trap 'rm -rf "$work"' EXIT
cd "$work"

for style in Regular Bold Italic; do
    curl -fsSL -o "JetBrainsMono-$style.woff2" \
        "https://raw.githubusercontent.com/JetBrains/JetBrainsMono/$JETBRAINS_TAG/fonts/webfonts/JetBrainsMono-$style.woff2"
done
curl -fsSL -o JetBrainsMono.OFL.txt "https://raw.githubusercontent.com/JetBrains/JetBrainsMono/$JETBRAINS_TAG/OFL.txt"
curl -fsSL -o NerdFontsSymbolsOnly.tar.xz \
    "https://github.com/ryanoasis/nerd-fonts/releases/download/$NERD_TAG/NerdFontsSymbolsOnly.tar.xz"
if ! echo "$SUMS" | shasum -a 256 -c - >/dev/null; then
    echo "vendor-fonts: a download does not match its pinned sha256" >&2
    exit 1
fi
mkdir symbols
tar -xJf NerdFontsSymbolsOnly.tar.xz -C symbols SymbolsNerdFontMono-Regular.ttf LICENSE

node - symbols/SymbolsNerdFontMono-Regular.ttf SymbolsNerdFontMono-Regular.woff2 <<'JS'
// A TrueType file in a WOFF2 container (W3C WOFF2, section 4): the same tables,
// none transformed, in one Brotli stream.
const fs = require("node:fs");
const zlib = require("node:zlib");
const [source, target] = process.argv.slice(2);
const sfnt = fs.readFileSync(source);
const KNOWN = ["cmap", "head", "hhea", "hmtx", "maxp", "name", "OS/2", "post", "cvt ", "fpgm", "glyf", "loca", "prep",
  "CFF ", "VORG", "EBDT", "EBLC", "gasp", "hdmx", "kern", "LTSH", "PCLT", "VDMX", "vhea", "vmtx", "BASE", "GDEF", "GPOS",
  "GSUB", "EBSC", "JSTF", "MATH", "CBDT", "CBLC", "COLR", "CPAL", "SVG ", "sbix", "acnt", "avar", "bdat", "bloc", "bsln",
  "cvar", "fdsc", "feat", "fmtx", "fvar", "gvar", "hsty", "just", "lcar", "mort", "morx", "opbd", "prop", "trak", "Zapf",
  "Silf", "Glat", "Gloc", "Feat", "Sill"];
const count = sfnt.readUInt16BE(4);
const tables = [];
for (let index = 0; index < count; index++) {
  const entry = 12 + index * 16;
  tables.push({ tag: sfnt.toString("latin1", entry, entry + 4), offset: sfnt.readUInt32BE(entry + 8), length: sfnt.readUInt32BE(entry + 12) });
}
const base128 = (value) => {
  const bytes = [value & 0x7f];
  for (value >>>= 7; value; value >>>= 7) bytes.unshift((value & 0x7f) | 0x80);
  return Buffer.from(bytes);
};
const directory = Buffer.concat(tables.map(({ tag, length }) => {
  const known = KNOWN.indexOf(tag);
  // glyf and loca have a transform by default; version 3 says "not transformed".
  const flags = (known < 0 ? 63 : known) | (tag === "glyf" || tag === "loca" ? 0xc0 : 0);
  return Buffer.concat([Buffer.from([flags]), known < 0 ? Buffer.from(tag, "latin1") : Buffer.alloc(0), base128(length)]);
}));
const data = zlib.brotliCompressSync(Buffer.concat(tables.map(({ offset, length }) => sfnt.subarray(offset, offset + length))), {
  params: { [zlib.constants.BROTLI_PARAM_QUALITY]: 11, [zlib.constants.BROTLI_PARAM_LGWIN]: 24,
    [zlib.constants.BROTLI_PARAM_MODE]: zlib.constants.BROTLI_MODE_FONT },
});
const header = Buffer.alloc(48);
header.writeUInt32BE(0x774f4632, 0);
sfnt.copy(header, 4, 0, 4);
header.writeUInt32BE(48 + directory.length + data.length, 8);
header.writeUInt16BE(count, 12);
header.writeUInt32BE(12 + 16 * count + tables.reduce((sum, { length }) => sum + ((length + 3) & ~3), 0), 16);
header.writeUInt32BE(data.length, 20);
fs.writeFileSync(target, Buffer.concat([header, directory, data]));
JS

rm -rf "$out"
mkdir -p "$out"
cp JetBrainsMono-Regular.woff2 JetBrainsMono-Bold.woff2 JetBrainsMono-Italic.woff2 JetBrainsMono.OFL.txt "$out/"
cp SymbolsNerdFontMono-Regular.woff2 symbols/SymbolsNerdFontMono-Regular.ttf "$out/"
cp symbols/LICENSE "$out/NerdFonts.LICENSE"
echo "vendor-fonts: wrote JetBrains Mono $JETBRAINS_TAG and Symbols Nerd Font Mono $NERD_TAG to $out"
ls -l "$out"
