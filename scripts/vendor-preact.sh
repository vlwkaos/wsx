#!/bin/sh
# Rebuilds the Preact and htm files in crates/wsx-web/web/vendor from the pinned
# npm tarballs. They are plain ES modules: the page has no build step.
# The only change is the import of "preact" in the hooks module, which becomes the
# relative file the page serves, because a browser cannot resolve a bare name.
set -eu

PREACT_VERSION=10.29.7
PREACT_SHA512='DCHYrK/B10yUD3ZjLfhZ3WIE/9Vf9VFUODcRE2dRomTYDpJk6z6L9wecSfhfE6M9ZTHUdyQkoC46arIDhEV84Q=='
HTM_VERSION=3.1.1
HTM_SHA512='983Vyg8NwUE7JkZ6NmOqpCZ+sh1bKv2iYTlUkzlWmA5JD2acKoxd4KVxbMmxX/85mtfdnDmTFoNKcg5DGAvxNQ=='

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/crates/wsx-web/web/vendor"
work=$(mktemp -d "${TMPDIR:-/tmp}/vendor-preact.XXXXXX")
trap 'rm -rf "$work"' EXIT

fetch() { # name version sha512
    curl -fsS -o "$work/$1.tgz" "https://registry.npmjs.org/$1/-/$1-$2.tgz"
    actual=$(openssl dgst -sha512 -binary "$work/$1.tgz" | base64 | tr -d '\n')
    if [ "$actual" != "$3" ]; then
        echo "vendor-preact: $1@$2 does not match its pinned sha512" >&2
        exit 1
    fi
    mkdir "$work/$1"
    tar -xzf "$work/$1.tgz" -C "$work/$1"
}
fetch preact "$PREACT_VERSION" "$PREACT_SHA512"
fetch htm "$HTM_VERSION" "$HTM_SHA512"

mkdir -p "$out"
python3 - "$work" "$out" <<'PY'
import pathlib, re, sys
work, out = map(pathlib.Path, sys.argv[1:])
def module(path):
    return re.sub(r"\n//# sourceMappingURL=\S+\n?$", "\n", (work / path).read_text())
hooks = module("preact/package/hooks/dist/hooks.module.js")
if hooks.count('from"preact"') != 1:
    sys.exit("vendor-preact: expected exactly one import of preact in the hooks module")
(out / "preact.js").write_text(module("preact/package/dist/preact.module.js"))
(out / "preact-hooks.js").write_text(hooks.replace('from"preact"', 'from"./preact.js"'))
(out / "htm.js").write_text(module("htm/package/dist/htm.module.js"))
(out / "preact.LICENSE").write_text((work / "preact/package/LICENSE").read_text())
(out / "htm.LICENSE").write_text((work / "htm/package/LICENSE").read_text())
PY
echo "vendor-preact: wrote preact $PREACT_VERSION and htm $HTM_VERSION to $out"
