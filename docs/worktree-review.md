# Worktree review contract

Core types, daemon invocation, an external Git provider, and keyboard-driven review are implemented. The existing executable API is documented in [plugins.md](plugins.md).

## Enable the reference provider

The provider requires Python 3 and Git. It is not installed into user configuration automatically. Copy `scripts/wsx-git-review.py` and `examples/plugins/git-review.json` into `${XDG_CONFIG_HOME:-$HOME/.config}/wsx/plugins/`, set the directory and script modes to 700 and the manifest mode to 600, and run `wsx plugin reload` using matching rebuilt wsx/wsxd binaries. The script must remain adjacent to its manifest. No Git mutations are performed.

Select a worktree and press Tab. Use j/k to select files, Enter to focus the diff, [/] for hunks, PageUp/PageDown to scroll, and Esc to return. Press r to refresh. If no provider is installed, the view reports that rather than silently using an internal provider.

## Verification boundary

Automated checks cover the real Git provider through the daemon's executable transport and response validator, and host focus/render behavior. They do not constitute visual verification inside the user's running terminal. Existing live sessions and installed binaries are not modified by a source build.


## User experience

The ordinary worktree preview keeps its colors, spacing, section order, and Local file list. Enter and h/l retain tree expansion behavior. Tab enters Local only when a review provider is available. No panel appears inside the agent terminal.

| Focus | Input | Result |
|---|---|---|
| Worktree tree row | Tab | Enter Local and restore file selection |
| Local | j/k | Select file and request its diff |
| Local | Enter | Focus the displayed diff |
| Diff | j/k, PageUp/PageDown | Scroll |
| Diff | [ / ] | Previous/next hunk |
| Diff | Esc | Return to Local |
| Local | Esc | Restore ordinary preview and tree focus |
| Review | r | Refresh the comparison explicitly |

WSX owns footer hints and focus. Terminal input is unchanged. Files use stable provider file IDs rather than list indices. Selection is scoped to worktree and provider. Removed files select the nearest remaining row without opening another worktree.

While reviewing, a short file list sits above a flexible unified diff. Branch and Path remain visible. Other metadata collapses only when needed in review mode and returns on exit. Tiny views use file and diff pages instead of stacking unusable rectangles. Existing semantic theme roles style headers, additions, and deletions; literal + and - remain visible.

A displayed diff never changes beneath the reader. A newer observation marks the view as updated; explicit refresh installs a new snapshot and preserves the selected file where possible.

## Public executable contribution

Retain version 1 event and sidecar manifests unchanged. Add an optional independently versioned `worktree_review` declaration containing `api_version: 1`, integer `priority`, and supported comparisons. The initial provider supports `working_against_head`. Do not advertise staged or revision-range comparisons until implemented.

The host selects one enabled compatible provider by descending priority and ascending plugin ID. Duplicate IDs are rejected at discovery rather than resolved differently by list and execution paths. No built-in provider bypasses discovery, validation, or invocation limits.

Review invocations use direct argv, no shell, one JSON request on stdin, and one JSON response on stdout. Stderr is diagnostic only and bounded. This transport is separate from the existing sidecar environment protocol. Every request includes API version, request ID, operation, daemon-resolved worktree context, comparison, and host limits. Providers do not choose workspace paths through client input.

| Operation | Request | Successful response |
|---|---|---|
| `list_files` | Worktree context, comparison | Snapshot token, comparison label, file summaries, truncation metadata |
| `file_diff` | Same context, snapshot token, file ID | Same token and file ID, structured hunks or explicit non-text result |

File summaries contain opaque file ID, old/new relative paths, typed status, optional addition/deletion counts, and content kind. Unknown counts are not zero. Renames retain both paths. Untracked, binary, submodule, unreadable, and oversized content have explicit representations.

A diff contains hunks with old/new start and count plus typed lines: context, addition, deletion, and no-newline metadata. WSX derives line numbers and styling. Providers cannot return ANSI, widgets, keybindings, arbitrary actions, or executable links. Text is treated as untrusted data, never terminal instructions.

Responses echo request ID and carry either typed data or an error code: `unsupported`, `stale_snapshot`, `unavailable`, `invalid_request`, or `limit_exceeded`. Host transport errors separately distinguish timeout, cancellation, abnormal exit, and malformed response. Human-readable messages do not control routing.

## Consistency and limits

Snapshot tokens are opaque and scoped to provider, worktree, and comparison. A provider must not return current content under an old token. It either serves captured content or validates the comparison before and after reading and rejects detectable drift. Git working-tree observation is not an atomic filesystem snapshot; document this limit rather than claiming transactional consistency.

The initial Git provider fingerprints HEAD, index, and relevant file observations. It validates selected-file bytes and comparison inputs around diff production. A change invalidates the token and returns `stale_snapshot`; the host retains the previously valid display and offers refresh. Agent/session causality is never inferred.

Host hard ceilings: 3-second execution deadline, 1 MiB stdout, 64 KiB stderr, 1,000 file summaries, 256 hunks, and 10,000 diff lines. Each request may lower these limits. Oversized content returns explicit truncation or a non-text result; it is never reported as a complete diff. Path and text bounds are validated before publication.

The host bounds request concurrency and queues. Changing worktree/provider cancels obsolete work. Cancellation kills and reaps the invocation process group, including descendants. Results must match request generation, daemon epoch, worktree, provider, file ID, and snapshot token before installation.

Executable plugins are trusted programs running with the user's permissions. Output validation is not a sandbox. Only owner-controlled manifests and executables are accepted. No installation, dependency download, Git mutation, or user daemon restart is part of review.

## Verification coverage

Tests cover typed and legacy manifests, request and response validation, bounded transport, timeout and process-group cancellation, stale tokens, real Git file and diff output, symlink refusal, preview preservation, Terminal Tab passthrough, focus unwinding, tiny geometry, and stable displayed diffs during updates. The executable example is validated through the same public daemon transport used by third-party providers.
