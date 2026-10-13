#!/bin/sh
# Rebuilds crates/wsx-web/web/vendor/mermaid.js.gz from the pinned npm tarball.
# The page loads it only when a message holds a diagram (docs/web-client.md "Diagrams").
# The file is stored and served gzip-compressed: 3.6 MB of script are 1 MB that way.
# Four patches, each matched exactly once:
#   1. The bundle is loaded as a module, where its top-level `var` is not a global.
#   2. The diagram's CSS goes into a <metadata> element and to a page hook that adopts it
#      as a constructed style sheet, because the CSP refuses an inline <style>.
#      web/diagram.js turns the element back into <style> in the SVG text.
#   3. Cytoscape's one-rule <style> element for its container is left out for the same reason.
#   4. The last DOMPurify pass over the finished SVG is left out. It parses every style
#      attribute, which the CSP reports, and the page shows the SVG only as an image
#      (no script, no link, no request), never as markup. Labels are still sanitized.
set -eu

MERMAID_VERSION=11.17.2
MERMAID_SHA512='V6K3C8EBdEsPFZXSKMJe6ppQOENxuHARr9GvHX4hh47lAbhMRD9qf4oEK7LoaRQxULMa80/qt5gHO73aCleBBg=='

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/wsx-web/web/vendor"
work=$(mktemp -d "${TMPDIR:-/tmp}/vendor-mermaid.XXXXXX")
trap 'rm -rf "$work"' EXIT

curl -fsS -o "$work/mermaid.tgz" "https://registry.npmjs.org/mermaid/-/mermaid-$MERMAID_VERSION.tgz"
actual=$(openssl dgst -sha512 -binary "$work/mermaid.tgz" | base64 | tr -d '\n')
if [ "$actual" != "$MERMAID_SHA512" ]; then
    echo "vendor-mermaid: mermaid@$MERMAID_VERSION does not match its pinned sha512" >&2
    exit 1
fi
tar -xzf "$work/mermaid.tgz" -C "$work"

mkdir -p "$out"
python3 - "$work" "$out" "$MERMAID_VERSION" "$root" <<'PY'
import gzip, pathlib, sys
work, out, version, root = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3], pathlib.Path(sys.argv[4])
source = (work / "package/dist/mermaid.min.js").read_text()
PATCHES = [
    ('globalThis["mermaid"] = globalThis.__esbuild_esm_mermaid_nm["mermaid"].default;',
     'globalThis["mermaid"] = __esbuild_esm_mermaid_nm["mermaid"].default;'),
    ('k=document.createElement("style");k.innerHTML=S,T.insertBefore(k,w);',
     'k=document.createElementNS("http://www.w3.org/2000/svg","metadata");k.textContent=S,globalThis.__wsxDiagramStyle(S),T.insertBefore(k,w);'),
    ('if(r.className.indexOf(l)<0&&(r.className=(r.className||"")+" "+l),!u){var h=i.createElement("style");'
     'h.id=o,h.textContent="."+l+" { position: relative; }",a.insertBefore(h,a.children[0])}',
     'r.className.indexOf(l)<0&&(r.className=(r.className||"")+" "+l);'),
    ('m||(A=ao.sanitize(A,{ADD_TAGS:ZMt,ADD_ATTR:QMt,HTML_INTEGRATION_POINTS:{foreignobject:!0}}))', 'void 0'),
]
for old, new in PATCHES:
    if source.count(old) != 1:
        sys.exit(f"vendor-mermaid: expected exactly one match for {old[:60]!r}")
    source = source.replace(old, new)
if 'createElement("style")' in source:
    sys.exit("vendor-mermaid: a <style> element is still created")
page = (root / "crates/wsx-web/web/diagram.js").read_text()
if f'"./mermaid-{version}.js"' not in page:
    sys.exit(f"vendor-mermaid: web/diagram.js does not import ./mermaid-{version}.js")
# mtime 0 keeps the file the same for the same input.
(out / "mermaid.js.gz").write_bytes(gzip.compress(source.encode(), 9, mtime=0))
(out / "mermaid.LICENSE").write_text((work / "package/LICENSE").read_text())
print(f"vendor-mermaid: {len(source.encode())} bytes, {(out / 'mermaid.js.gz').stat().st_size} gzip")
PY
echo "vendor-mermaid: wrote mermaid $MERMAID_VERSION to $out"
