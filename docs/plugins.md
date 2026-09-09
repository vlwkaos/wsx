# WSX executable plugins

WSX loads owner-controlled JSON manifests from `~/.config/wsx/plugins/`. Existing event-only manifests remain valid. API version 1 may also declare one passive Terminal sidecar.

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
