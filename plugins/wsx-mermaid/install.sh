#!/bin/sh
# Install the wsx-mermaid reference renderer for the current user. See docs/plugins.md.
set -eu
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
plugin_root=${XDG_CONFIG_HOME:-"$HOME/.config"}/wsx/plugins
target=$plugin_root/wsx-mermaid
node=$(command -v node) || { echo "wsx-mermaid: node 18 or newer is required" >&2; exit 1; }

umask 077
mkdir -p "$target"
cp "$source_dir/wsx-mermaid.mjs" "$source_dir/package.json" "$source_dir/package-lock.json" "$target/"
# The lockfile pins grok-mermaid by version and integrity; no install script runs.
(cd "$target" && npm ci --omit=dev --ignore-scripts --no-audit --no-fund)

# wsxd may not have node on its PATH, so the launcher records the absolute interpreter.
# The script path is resolved at run time, relative to the launcher.
cat > "$target/wsx-mermaid" <<LAUNCHER
#!/bin/sh
exec "$node" "\$(dirname -- "\$0")/wsx-mermaid.mjs"
LAUNCHER
chmod 700 "$target/wsx-mermaid"
cat > "$plugin_root/wsx-mermaid.json" <<MANIFEST
{
  "api_version": 1,
  "id": "wsx-mermaid",
  "name": "Mermaid renderer",
  "command": ["./wsx-mermaid/wsx-mermaid"],
  "events": [],
  "enabled": true,
  "renderer": { "api_version": 1, "languages": ["mermaid"] }
}
MANIFEST
chmod 600 "$plugin_root/wsx-mermaid.json"
chmod 700 "$plugin_root" "$target"
echo "installed $plugin_root/wsx-mermaid.json"
echo "run: wsx plugin reload"
