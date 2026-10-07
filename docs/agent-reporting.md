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

The entry path is stable across upgrades; its target is the daemon's adjacent
reporter at publication time. Package cleanup can remove that target before handoff,
or while a compatible older daemon remains in use. Updated adapters recover this
transition without copying an executable or assuming one global install prefix.

## Multi-device hook configuration

Shared provider configuration stores environment references, not the installing machine's absolute hook path. For example, Claude hooks resolve `CLAUDE_CONFIG_DIR` at execution time, or use `$HOME/.claude` when it is unset or empty. Codex uses `CODEX_HOME`; other supported shell-hook providers use their existing root variables or `HOME`. Installation and hook generation share one root declaration, including `~/` expansion and provider subdirectories.

Install the integration on each device. A copied configuration does not copy the executable assets. Quoted expansion treats spaces, quotes and shell-looking directory names as path data; it does not use `eval`. Missing roots or assets fail rather than selecting a development checkout. Each hook keeps its provider input on stdin and its existing report-generation fences.

Refresh migrates only managed WSX command bindings, including bindings copied from another device. It preserves unrelated hooks, their matchers and unrelated configuration fields. Repeated refresh does not add duplicate commands. This does not modify the environment of an already-running agent or tmux server. The private installed-config journey verifies macOS shell execution. Candidate Linux execution and Windows shell execution are not established by that receipt.

## Existing panes

A live handoff cannot rewrite the environment of an existing shell. Updated adapters
recover an absolute legacy reporter path that no longer exists by using the stable
entry point derived from `WSX_SOCKET`, or the usual XDG/HOME wsx state directory.
If the owner-controlled entry is absent or points to a removed keg, recovery checks
at most 32 absolute inherited PATH directories for `wsx`. It executes a canonical
regular executable owned by the caller or root, with no group/other write permission,
in a similarly protected canonical parent. The socket directory must remain private
and caller-owned. An invalid reserved entry is a refusal, not permission to bypass it.
The arguments and runtime generation remain unchanged. Reports rejected by the CLI,
including stale-generation reports, never trigger a fallback replay.

Install the updated adapter through Global Settings or accept the integration update
prompt, then use the provider's supported reload or restart when the agent is idle.
Already-loaded adapter/helper code cannot be repaired by changing its file alone.
Do not restart the shell or daemon to refresh an adapter. Current-CLI recovery does
not require daemon handoff to finish; missing or unsafe candidates still fail explicitly.

## Verification

- `node scripts/test-agent-reporter.mjs` checks removed-path recovery and no rejection replay.
- `node scripts/test-pi-agent-status.mjs` exercises Pi lifecycle events with a removed legacy path.
- The daemon reporter test checks publication, aborted handoff, old-target removal and unsafe targets.
- Isolated runtime scenarios verify actual reporting and live handoff without replacing pane processes.
