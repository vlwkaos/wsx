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
entry point derived from `WSX_SOCKET`, which wsxd gives each pane, or the usual
XDG/HOME wsx state directory ([daemon-discovery.md](daemon-discovery.md)).
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

## Hook exit

The shell hook (`wsx-agent-status.sh`, Claude integration 27, Codex integration 17) exits 0 without output when the daemon cannot take a report for an expected reason: the pane has no `WSX_PANE_ID`, wsxd answers `stale_runtime` (a stale or missing runtime generation), `terminal_unavailable` (the pane runtime is gone) or `not_found` (the pane is gone), or the socket or the reporter is gone. These follow a daemon restart, an upgrade or a closed pane, and a hook error on every tool call would only add noise. Any other rejection exits 1 with one bounded line, `wsx <provider> <action> report failed (report_failed)`, and no vendor input. No report is replayed.

An installed integration is current when its version is newer than this build's, or the same and its primary file is exactly what this build writes. So a shared asset changed without a version bump, such as this shell hook of every shell provider, still shows as outdated. `wsx agent install <provider>`, and the TUI's update prompt for an outdated integration, rewrite the hook files to the current build. They also rewrite an older guarded binding, `if [ -x "X/wsx-agent-status.sh" ]; then "X/wsx-agent-status.sh" ACTION; fi`, to the managed command, so each event keeps one wsx binding and other hooks stay as they are. The 0.30.1 opt-in inspection (`wsx-agent-inspect.py`, `WSX_INSPECT`, the `wsx-inspect-enabled` marker) is retired: install deletes its files beside the hook.

## Output style

Every wsx agent presents a reply by one rule, `crates/wsx-core/integrations/common/presentation.md` (`integration::PRESENTATION`): the answer first, an unverified assumption on the first line, structure as a mermaid diagram or a table, each fact once, about one phone screen. The manager's and the workers' instructions include it (docs/tasks.md "Workflow").

`wsx agent install claude` also writes it as the Claude Code output style `wsx` (`output-styles/wsx.md` in the Claude config root, with `keep-coding-instructions: true`, https://code.claude.com/docs/en/output-styles). It sets `outputStyle` to `wsx` in `settings.json` only when that file names no style: a style the person chose stays, and a project's `.claude/settings.local.json` still wins. A session reads the style when it starts. `/output-style` switches it off for a session.

## Claude compaction

Claude Code compacts its conversation in three ways: the person's `/compact`, an automatic compaction at the context limit, which can come inside a turn, and (Claude Code 2.1.310) an idle compaction between turns. The idle one runs about 54 minutes after a turn ends, at 90% of the one-hour prompt cache, for a conversation of at least 100k tokens; Claude shows "Compacted while idle, before the prompt cache expired" and `"idleCompaction": false` turns it off. It is a turn of Claude's own: it took 37 to 55 s in real transcripts (2026-10), and it fires no `UserPromptSubmit` and no `Stop`. Each compaction fires `PreCompact` and `PostCompact` with its `trigger`, and `SessionStart` with `source: "compact"` after it. Claude integration 27 (`wsx-agent-status.sh`) reports:

| Hook input (Claude Code 2.1.300 to 2.1.310) | Report |
|---|---|
| `PreCompact`, any trigger | `working --compacting` |
| `PostCompact`, any trigger | `--compacted` |
| `SessionStart`, `source: "compact"` | none: the turn, or the done state, goes on |
| `SessionStart` from `startup`, `resume`, `clear` | `idle`, as before |

wsxd (daemon revision 37) keeps the turn: a compaction that begins while the agent works leaves it working when it ends. One that begins outside a turn (idle, done, or a `/compact`) makes the agent work until it ends, then returns it to the state before it. So a message from the chat waits in the queue while Claude compacts, and is delivered after it. The agent's `compacting` is set from the start to the end, and the chat shows "Compacting the conversation".

- Before, only `/compact` was reported. An idle compaction left wsxd showing done while Claude worked: the chat's message waited for an editor that did not look idle, failed after 30 s with `editor_unavailable`, or was typed into the busy Claude (seen by the owner on 2026-10-15 and 2026-10-27).
- A compaction outside a turn whose `PostCompact` never comes (the compaction failed) ends after 10 minutes. Any other report of the agent ends it at once.
- `/compact` has no `UserPromptSubmit` and no `Stop` (seen with Claude Code 2.1.300). Its `PostCompact` ends it, so its exchange ends `done_observed`.
- The script decides, not the hook matchers, so an older copy of the managed hook command that still calls the same script is also fixed. `PostCompact` must exist: a Claude without it stays `working` after a compaction until the next report.
- The reporter is the `wsx` beside wsxd, so it sends `--compacted` only to a wsxd that knows it. An older wsxd reads the end of a compaction as `done`.
- Other providers are unchanged.
- An installed integration below 27 shows as outdated. Run `wsx agent install claude`, then restart Claude when it is idle so that it loads the new hook configuration.

## Pi native exchange observer

Pi integration 20 and later keeps lifecycle-only behavior without Pygmalion's Goal Run owner or daemon bound-receipt support. With both present, the loaded observer advertises prompt/receipt support, binds exact delivered native input before a run, and holds completion through Task continuations. See [native request-bound receipts](agent-orchestration.md#native-request-bound-receipts). Update assets and load the new extension through the provider's normal lifecycle; never rewrite or restart an unrelated live pane. Updating files alone does not alter loaded code.

## Wake heartbeat

Claude's `UserPromptSubmit` hook starts `wsx.reporter agent wake-heartbeat <pane> --prompt-id <id>` as an asynchronous hook. Claude runs it in a detached `/bin/sh -c` and does not end it, so its parent process tells nothing about the pane.

- The heartbeat renews the wake lease of its prompt every 5 seconds. A renewal is an in-memory lease update in wsxd, so it is also the check that the prompt, the pane and wsxd still exist.
- It ends at the first refused renewal: the prompt ended or was replaced, the pane or its runtime generation is gone, or no Claude agent is at work.
- It ends when no wsxd answers on the socket, after the client's wait for a live handoff. Other failures, such as a timeout, end it after 5 minutes without an answer.
- `agent wake-heartbeat` and `agent presence-renew` never start wsxd, as `agent report` does not.

## Verification

- The reporter scenario: `python3 -B scripts/test-agent-reporter.py` runs the shell and Hermes reporters' removed-path recovery and refusal and the Claude hook's prompt receipts, and `node scripts/test-agent-reporter.mjs` checks the JS adapters' removed-path recovery and no rejection replay.
- `node scripts/test-pi-agent-status.mjs` exercises Pi lifecycle events with a removed legacy path.
- The daemon reporter test checks publication, aborted handoff, old-target removal and unsafe targets.
- Isolated runtime scenarios verify actual reporting and live handoff without replacing pane processes.
- `scripts/test-agent-context.py` runs the installed Claude hooks for a compaction inside a turn, a `/compact`, an idle compaction (a stashed delivery is refused while it runs) and a session start against a real wsxd.
- `scripts/web-real-agent-check.py --scenario compact` runs a real Claude with this build's hooks: `/compact`, and a message sent while it compacts.
- `python3 -B scripts/test-agent-context.py --only context` starts the installed heartbeat hook in a detached shell and checks that it ends after a finished prompt, a replaced prompt, a deleted pane and a stopped wsxd, and that it starts no wsxd.
