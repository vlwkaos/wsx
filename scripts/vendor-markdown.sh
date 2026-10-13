#!/bin/sh
# Rebuilds the markdown parser and the code highlighter in crates/wsx-web/web/vendor
# from the pinned npm tarballs. The page has no build step.
# markdown-it: the published browser file, unchanged. It sets globalThis.markdownit.
# highlight.js: the core and a fixed language list, joined into one ES module.
set -eu

MARKDOWN_IT_VERSION=14.3.2
MARKDOWN_IT_SHA512='sHHjZ5fJKlgrG4qns2YwVcdNep35h5fERrfkD2YNsb9UFk0UIHarbiTaHKVMlPuWAoiilyK8Fv/jAm11slsY7Q=='
HIGHLIGHT_VERSION=11.11.1
HIGHLIGHT_SHA512='VEPdHzwelZ12hEX18BHduqxMZGolcUsrbeokHYxOUIm8X2+M7nx5QPtPeQgRxR9XjhdLv4/7DD5BWOlSrJ3k7Q=='
# docs/web-client.md "Markdown": toml is hljs "ini", html is "xml".
LANGUAGES='rust typescript javascript python bash json ini yaml diff xml css sql go'

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/wsx-web/web/vendor"
work=$(mktemp -d "${TMPDIR:-/tmp}/vendor-markdown.XXXXXX")
trap 'rm -rf "$work"' EXIT

fetch() { # registry path, version, sha512, directory
    curl -fsS -o "$work/$4.tgz" "https://registry.npmjs.org/$1/-/$(basename "$1")-$2.tgz"
    actual=$(openssl dgst -sha512 -binary "$work/$4.tgz" | base64 | tr -d '\n')
    if [ "$actual" != "$3" ]; then
        echo "vendor-markdown: $1@$2 does not match its pinned sha512" >&2
        exit 1
    fi
    mkdir "$work/$4"
    tar -xzf "$work/$4.tgz" -C "$work/$4"
}
fetch markdown-it "$MARKDOWN_IT_VERSION" "$MARKDOWN_IT_SHA512" markdown-it
fetch @highlightjs/cdn-assets "$HIGHLIGHT_VERSION" "$HIGHLIGHT_SHA512" highlight

mkdir -p "$out"
python3 - "$work" "$out" $LANGUAGES <<'PY'
import pathlib, re, sys
work, out = map(pathlib.Path, sys.argv[1:3])
languages = sys.argv[3:]
def once(text, old, new, what):
    # Each replacement must match exactly once, so a new upstream version cannot be
    # patched by accident.
    if text.count(old) != 1:
        sys.exit(f"vendor-markdown: expected exactly one {what}")
    return text.replace(old, new)

markdown = (work / "markdown-it/package/dist/markdown-it.min.js").read_text()
if "globalThis:t||self).markdownit=" not in markdown:
    sys.exit("vendor-markdown: markdown-it no longer sets globalThis.markdownit")
(out / "markdown-it.js").write_text(markdown)
(out / "markdown-it.LICENSE").write_text((work / "markdown-it/package/LICENSE").read_text())

es = work / "highlight/package/es"
core = once((es / "core.min.js").read_text(), "export{te as default};", "const hljs=te;\n", "default export in the highlight.js core")
# The page builds its nodes from the token tree, never from highlight.js HTML.
if "rootNode" not in core:
    sys.exit("vendor-markdown: the highlight.js token tree (rootNode) is gone")
parts = [core]
for name in languages:
    grammar = (es / "languages" / f"{name}.min.js").read_text()
    grammar = once(grammar, "var hljsGrammar=", f'hljs.registerLanguage("{name}",', f"grammar start in {name}")
    grammar = once(grammar, ";export default hljsGrammar;", ");\n", f"grammar export in {name}")
    if re.search(r"""(^|[;\n}])import\s*(\{|\w+\s+from|["'])""", grammar):
        sys.exit(f"vendor-markdown: {name} imports another module")
    parts.append(grammar)
parts.append("export default hljs;\n")
(out / "highlight.js").write_text("".join(parts))
(out / "highlight.LICENSE").write_text((work / "highlight/package/LICENSE").read_text())
PY
echo "vendor-markdown: wrote markdown-it $MARKDOWN_IT_VERSION and highlight.js $HIGHLIGHT_VERSION ($LANGUAGES) to $out"
