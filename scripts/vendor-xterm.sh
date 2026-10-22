#!/bin/sh
# Rebuilds crates/wsx-web/web/vendor from the pinned npm tarballs.
# docs/web-client-terminal.md explains why xterm.js is patched:
# the site CSP has no 'unsafe-inline' for styles, and the xterm.js DOM renderer
# writes <style> elements and style attributes. The patch moves both to CSSOM
# calls, which the CSP allows. Nothing else in the file changes.
set -eu

XTERM_VERSION=5.5.0
XTERM_SHA512='hqJHYaQb5OptNunnyAnkHyM8aCjZ1MEIDTQu1iIbbTD/xops91NB5yq1ZK/dC2JDbVWtF23zUtl9JE2NqwT87A=='
FIT_VERSION=0.10.0
FIT_SHA512='UFYkDm4HUahf2lnEyHvio51TNGiLK66mqP2JoATy7hRZeXaGMRDr00JiSF7m63vR5WKATF605yEggJKsw0JpMQ=='

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/wsx-web/web/vendor"
work=$(mktemp -d "${TMPDIR:-/tmp}/vendor-xterm.XXXXXX")
trap 'rm -rf "$work"' EXIT

fetch() { # name version sha512 directory
    curl -fsS -o "$work/$4.tgz" "https://registry.npmjs.org/@xterm/$1/-/$1-$2.tgz"
    actual=$(openssl dgst -sha512 -binary "$work/$4.tgz" | base64 | tr -d '\n')
    if [ "$actual" != "$3" ]; then
        echo "vendor-xterm: @xterm/$1@$2 does not match its pinned sha512" >&2
        exit 1
    fi
    mkdir "$work/$4"
    tar -xzf "$work/$4.tgz" -C "$work/$4"
}
fetch xterm "$XTERM_VERSION" "$XTERM_SHA512" xterm
fetch addon-fit "$FIT_VERSION" "$FIT_SHA512" fit

mkdir -p "$out"
python3 - "$work" "$out" <<'PY'
import pathlib, sys
work, out = map(pathlib.Path, sys.argv[1:])
source = (work / "xterm/package/lib/xterm.js").read_text()
# Each replacement must match exactly once, so a new upstream version cannot be
# patched by accident.
SHEET = ('const __wsxSheet=d=>{const s=new CSSStyleSheet;d.adoptedStyleSheets=[...d.adoptedStyleSheets,s];'
         'return{set textContent(t){s.replaceSync(t)},remove(){d.adoptedStyleSheets=d.adoptedStyleSheets.filter(x=>x!==s)}}};')
PATCHES = [
    ('(this._dimensionsStyleElement=this._document.createElement("style"),this._screenElement.appendChild(this._dimensionsStyleElement))',
     '(this._dimensionsStyleElement=__wsxSheet(this._document))'),
    ('(this._themeStyleElement=this._document.createElement("style"),this._screenElement.appendChild(this._themeStyleElement))',
     '(this._themeStyleElement=__wsxSheet(this._document))'),
    ('_addStyle(e,t){e.setAttribute("style",`${e.getAttribute("style")||""}${t};`)}',
     '_addStyle(e,t){e.style.cssText+=`${t};`}'),
    ('\n//# sourceMappingURL=xterm.js.map', '\n'),
]
for old, new in PATCHES:
    if source.count(old) != 1:
        sys.exit(f"vendor-xterm: expected exactly one match for {old[:60]!r}")
    source = source.replace(old, new)
if 'createElement("style")' in source or 'setAttribute("style"' in source:
    sys.exit("vendor-xterm: an inline style write is left")
(out / "xterm.js").write_text(SHEET + "\n" + source)
fit = (work / "fit/package/lib/addon-fit.js").read_text()
(out / "addon-fit.js").write_text(fit.replace("\n//# sourceMappingURL=addon-fit.js.map", "\n"))
(out / "xterm.css").write_text((work / "xterm/package/css/xterm.css").read_text())
(out / "xterm.LICENSE").write_text((work / "xterm/package/LICENSE").read_text())
PY
echo "vendor-xterm: wrote @xterm/xterm $XTERM_VERSION and @xterm/addon-fit $FIT_VERSION to $out"
