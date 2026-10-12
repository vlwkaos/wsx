# WSX executable plugins

WSX loads owner-controlled JSON manifests from `~/.config/wsx/plugins/`. Existing event-only manifests remain valid. API version 1 may also declare one passive Terminal sidecar and a fenced-block renderer.

```json
{
  "api_version": 1,
  "id": "example",
  "name": "Example",
  "command": ["./example-plugin"],
  "events": ["session.created"],
  "enabled": true,
  "sidecar": {
    "surface": "terminal_right",
    "priority": 10,
    "minimum_columns": 120,
    "preferred_width": 36,
    "refresh_ms": 2000
  }
}
```

Review providers may instead declare `worktree_review` with `api_version: 1`, `priority`, and `comparisons: ["working_against_head"]`. See [worktree review](worktree-review.md) for the request protocol and setup. Review uses bounded JSON on stdin/stdout, not `WSX_PLUGIN_VIEW_JSON`. It never occupies the agent terminal.

Relative executables resolve inside the manifest directory without following symlinks. Manifests, commands, events, dimensions, and refresh rates are bounded. `wsx plugin reload` reloads accepted manifests.

## Sidecar request

When a declared sidecar is eligible, wsxd starts its command without a shell and sets `WSX_PLUGIN_VIEW_JSON`. The JSON object contains:

```json
{
  "api_version": 1,
  "pane_id": 12,
  "worktree_id": 4,
  "worktree_path": "/path/to/worktree",
  "columns": 36,
  "rows": 30,
  "generation": 7
}
```

The command writes one JSON payload to stdout:

```json
{
  "rows": [
    {
      "badge": " M",
      "primary": "main.rs",
      "secondary": "src",
      "value": "+4 -1",
      "tone": "warning"
    }
  ],
  "remaining": 0
}
```

`empty` may replace rows with a short empty-state message. Tones are `normal`, `muted`, `accent`, `success`, `warning`, and `error`.

WSX selects one eligible sidecar by descending priority and stable plugin ID. It owns width, truncation, semantic colors, PTY dimensions, cursor and mouse coordinates, and all input. Plugin output is size-, row-, text-, timeout-, and schema-bounded. Invalid output fails closed and does not replace the last valid view.

## Fenced-block renderers

A plugin may render fenced code blocks of a pane view document ([terminal view](terminal-view.md)). Add `renderer` to the manifest:

```json
"renderer": { "api_version": 1, "languages": ["mermaid"] }
```

wsxd starts the command without a shell for each matching block and writes one JSON request to stdin:

```json
{ "api_version": 1, "language": "mermaid", "source": "flowchart LR\n  A --> B\n", "columns": 116 }
```

The command writes one JSON response to stdout and exits 0:

```json
{ "lines": [[{ "text": "┌───┐", "tone": "muted" }, { "text": " A", "tone": "normal" }]], "note": "optional one-line remark" }
```

- `tone` is one of the sidecar tones; wsx owns the colors. Text must not contain control characters.
- `columns` is the width available in terminal cells. A renderer that cannot fit must fail; wsx cuts a longer line with `…`.
- Limits: 2 seconds per block, 4 seconds and 8 rendered blocks per document, 256 KiB of output, 400 lines, 4096 bytes per line.
- A non-zero exit, a timeout or invalid output leaves the block shown as its source. Rendering never runs in the TUI or under a daemon lock. wsxd keeps the result for one width per page.
- Enabled renderers are tried in plugin ID order; the first that lists the language wins. The pulled sidecar exists only on `terminal_right`; a pane view document is the `terminal_top` surface.

Reference renderer for mermaid (Node 18 or newer, `grok-mermaid` pinned by version and integrity):

```sh
sh plugins/wsx-mermaid/install.sh
wsx plugin reload
```
