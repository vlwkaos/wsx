# Agent reporter lifetime

wsxd owns the reporting entry point for managed panes. See also
[agent orchestration](agent-orchestration.md) and the runtime ownership rules in `AGENTS.md`.

## Stable entry point

At startup wsxd validates the adjacent `wsx` executable and stages an owner-only-directory
symlink beside its socket. For `wsx.sock`, the stable entry point is `wsx.reporter`.
New panes receive that path as `WSX_AGENT_REPORT_BIN`, never a versioned install path.
The executable must be a regular executable file owned by the current user or root,
with no group or other write permission.

A cold start publishes the link before recovery starts. A handoff successor stages
it before readiness but publishes only after the old daemon commits ownership.
An aborted successor removes only its staging link. Ordinary shutdown keeps the
published entry point; old-daemon cleanup never removes a successor's link.
Publication failure after commit is reported without killing imported PTYs. The successor
retains the staged link and retries once per second while it owns the daemon. Retries
refuse unrelated reserved-path content.

The path is stable across upgrades. Its target remains the current daemon's adjacent
reporter, so cleanup of an older Homebrew Cellar does not break surviving shells.
No executable is copied and no global install path is assumed.

## Existing panes

A live handoff cannot rewrite the environment of an existing shell. Updated adapters
recover an absolute legacy reporter path that no longer exists by using the stable
entry point derived from `WSX_SOCKET`, or the usual XDG/HOME wsx state directory.
The arguments and runtime generation remain unchanged. Reports rejected by the CLI,
including stale-generation reports, never trigger a fallback replay.

Install the updated adapter through Global Settings or accept the integration update
prompt, then restart the agent in its existing pane. This does not restart the shell
or daemon. Already-loaded adapters cannot be repaired by changing their file alone.
The stable entry point requires the repaired daemon; before that upgrade, a removed
reporter path still fails explicitly.

## Verification

- `node scripts/test-agent-reporter.mjs` checks removed-path recovery and no rejection replay.
- `node scripts/test-pi-agent-status.mjs` exercises Pi lifecycle events with a removed legacy path.
- The daemon reporter test checks publication, aborted handoff, old-target removal and unsafe targets.
- Isolated runtime scenarios verify actual reporting and live handoff without replacing pane processes.
