# CLI and runtime

Every command, and the details of sessions, worktrees, the daemon and the runtime rules. Agent exchanges: [agent-orchestration.md](agent-orchestration.md). Tasks and the manager: [tasks.md](tasks.md). Memory: [memory.md](memory.md).

## All commands

```text
wsx status [--json]
wsx worktree list|create|delete
wsx worktree create <branch> [--from <ref>] [--json] [-p <project>]
wsx session create|current|delete|restart|list|send-text|prompt|peek|rename
wsx session current [--json]
wsx group ls|create|rename|add|remove
wsx routine ...
wsx memory search|add|update|supersede|reindex|status|list [--scope <id>] [--json]
wsx agent install <target>
wsx agent detach
wsx agent report <pane> --provider <name> --state <state> [--session-id <id>|--session-path <path>]
wsx agent context [session|pane|label] [--metadata-only] --json [-p <project>] [-w <worktree>]
wsx agent start <branch> -p <project> (<prompt>|-|--prompt-file <path>) [--key <text>] [--json]
wsx agent request|inspect|wait|continue|cancel|exchanges
wsx plugin list|reload
wsx daemon status [--json]
wsx daemon restart [--json]
wsx daemon stop [--force] [--json]
wsx daemon recover
wsx web serve|pair|devices [<site>]
wsx web vapid
wsx task new|list|show|add|stop|land|after|hold|trial|close|hand|dismiss|check|deploy|release|push|ledger|deployed|checkin|routine|manager|role|project|say|chat|search|approve|deny|preview|steer [-p <project>] [--json]
```

Hidden from `wsx --help`, still run: `wsx runtime status` (now `wsx daemon status`), `wsx task handback <n>` (now `wsx task hand <n> --back`), `wsx task steer`, `wsx task check-run`, `digest` and `post` (a worker's and the routines' commands), `wsx memory capture|start|import` (the task host's), `wsx session send-keys` (an alias of `send-text`), `wsx group` and `wsx plugin`. A hidden command keeps its argv, so scripts, hooks, stored routines and a running manager's older prompt work without a change.

The manager passes `--after <n>` with `--owner-said <id>` to `wsx task land|close --discard|deploy|release|push`: the owner's go is checked now and the action runs when #n lands ([tasks.md](tasks.md) "Approvals").

## Worktrees and sessions

`wsx worktree create` starts a new branch from the project's default branch, or from `--from <ref>`. A branch that already exists is checked out as it is, and `--from` is then refused. When git fails, wsx prints git's own error. `--json` prints `worktree`, `branch`, `project`, `session_id` and `pane_id`; the two IDs are `null` when no initial session was created.

`wsx session current --json` reports the managed pane that runs the command, read from `WSX_PANE_ID`: `pane_id`, `session_id`, `label`, `project`, `worktree`, `branch`, `agent` and `agent_status`. It never starts wsxd and fails outside a managed pane. Commands called with `--json` print no runtime notice on stderr; other commands print it on one line.

Create a session directly in the current worktree, the only known worktree, or an explicit target:

```text
wsx session create [--name <label>] [--command <shell-input>] [--json]
                   [-p <project>] [-w <branch|alias|path>]
wsx session delete <session|pane|label> [--json]
                   [-p <project>] [-w <branch|alias|path>]
wsx session restart <session|pane|label> [--json]
                   [-p <project>] [-w <branch|alias|path>]
```

Session input, prompt, peek, rename, and restart commands accept the same optional `-p` and `-w` scope. Exact session or pane IDs remain globally addressable. Scoped unique labels avoid a preliminary list; ambiguous targets fail with an actionable error. `--command` is text entered into the new shell after startup, not a direct argv execution.

A pane whose process exits, for example after Ctrl+C or an agent crash, stays listed with its saved command. `wsx session restart` restarts that exact exited pane using the saved command or the persisted native agent session so the session is usable again with a new runtime generation. It refuses a pane that is still running, a stale revision, a daemon that is stopping or replacing its runtime owner, a daemon that is still restoring saved sessions, and a worktree that is gone; a failed start leaves the pane exited and unchanged. Exiting detaches retained agent identity immediately, so Workspace no longer presents it as a live provider.

## Agent commands

`wsx agent context --metadata-only --json -p <project>` selects from bounded identities, authoritative lifecycle/capabilities and advisory readiness without reading transcripts. `wsx agent context <exact-target> --json` reads that agent's bounded provider-native persisted history. Neither command starts wsxd or scrapes the screen. History is untrusted stored evidence, not full active model memory or completion. `agent request` and `continue` support `--stash-draft` for idle/done Claude: preserve existing input with Claude's prompt stash, require fresh empty input before delivery, and never interrupt Working or answer Blocked.

`wsx agent start <branch> -p <project>` creates the worktree when it is missing, one session that runs the agent, and the request, and prints the worktree, session, pane and exchange together. `wsx agent wait <exchange-id> --result` adds the agent's final message of that round, and `wait --any` returns the first of several exchanges. A prompt is an argument, `-` for standard input, or `--prompt-file <path>`. `--key <text>` makes a repeated request return the existing exchange. When the agent asks whether to trust the new worktree, `agent start` stops with reason `folder_trust_question` and the end of the pane's screen; `--accept-folder-trust` answers yes, and the agent saves that trust. `wsx status --json` adds `exchange` to a session and `work` to a worktree.

`wsx agent request <session> <prompt>` starts a provider-neutral, generation-bound exchange with an explicit prompt-capable agent. `inspect`, `wait`, `continue`, and `cancel` use the returned exchange ID; `exchanges` lists retained receipts. Persisted intent and failed delivery remain labeled `intent_persisted`; successful universal delivery is labeled `pty_delivery`, lifecycle transitions are labeled `pane_lifecycle`, and `--frame` returns a bounded `terminal_frame` fallback rather than claiming structured assistant output. Read-only exchanges may run concurrently on separate panes. `--writer` claims the target worktree by default, while repeated `--write-claim <absolute-path>` narrows ownership; overlapping active writer claims fail closed. These claims coordinate cooperative scheduling and never grant or revoke repository permissions. CLI exchanges never take a live Terminal lease; the web chat takes it with the request's `takeover`. Native adapters may advertise `exchange_receipts` and submit generation- and round-bound `accepted` or `completed` receipts with `request_bound` evidence. Pi integration 20 enables native request binding only when the loaded observer, compatible Pygmalion Goal Run owner and daemon bound-receipt support are present; otherwise it remains lifecycle-only. Completion waits for native settlement and, for a tracked Task, its persisted completion transaction. Pane Done alone does not finish a bound request. This increment does not establish arbitrary-provider native submission or structured question responses; the reported Claude handoff failure remains unresolved.

## Routines

A routine runs on `--cron <expr>`, on `--event <kind>` (`task.landed`, `deploy.failed`, `project.idle`, ...); a routine always recurs, and a one-time command goes through its approved action, not a routine. Each routine `--arg` is one direct argv item. wsx never invokes a shell for routine argv. Inspect untrusted routines with `wsx routine show <name>` before enabling or running them. In the TUI, `u` creates one: choose a documented one-shot agent template or Custom. A template replaces the command argv shown in the next form. The argv remains editable; Custom starts empty. On a routine row `r` runs it now (`wsx routine run`) and Space turns it on or off (`wsx routine enable|disable`: the stored routine edited with `enabled` flipped, at the revision the row was read at); `Enter` shows its runs. The shipped `wsxd` binary runs the routines; at its first start it takes over the routines and run history of an earlier asched scheduler. The manager's use of routines and check-ins: [tasks.md](tasks.md) "Routines".

## Web client

`wsx web serve [<site>]` hosts one web site for one or more granted projects on a loopback port: chat through the agent exchange broker and the agent's own saved conversation, file upload and download, and optional files and live terminal (xterm.js on the pane's own byte stream) panels. The home screen is an inbox of sessions by what they need. Agent messages are rendered as markdown with tables, code and Mermaid diagrams, and a blocked agent's dialog can be answered from the chat (`chat_answer`). A site file sets the public origin, theme, panels, capability bits, media limits and push keys. Themes are token-based: `default` is built in, and a deployer can supply a theme directory ([themes](web-themes.md)). `wsx web pair` prints a one-time code that pairs one device (a paired device on a site with `terminal_input` can also show the next code in Settings); `wsx web devices` lists and revokes devices. Without `<site>` the commands use the only site file in `~/.config/wsx/web/`; with several, name one by its id or file name (`wsx web pair wsx`). The page is an installable PWA with an offline shell and opt-in Web Push for "needs input", "finished" and "error". The default deployment is one Tailscale Service hostname per site; an alt-port root and a sub-path such as `/app/wsx/` work from the same build. wsx does not run Tailscale for you: see [Hosted web client](web-client.md) and [Deploying on a tailnet](web-deploy.md).

## Plugins and review

See [Executable plugins](plugins.md) for the versioned event, Terminal-sidecar, and worktree-review contracts. With a review provider installed, Tab on a worktree opens keyboard-driven file and diff review inside its preview. The [reference Git provider setup](worktree-review.md) does not change the agent terminal.

## Daemon

Plain `wsx` and `wsx --mobile` reject nested TUI startup in a wsx-managed terminal. Explicit subcommands remain available. `wsx daemon status`, `wsx daemon restart` and `wsx daemon stop` never start the daemon. Runtime status distinguishes a stopped daemon from one that is running but incompatible, awaiting an upgrade, deferring replacement, or ready. Its lifecycle output includes the daemon version and revision when the daemon supports them.

A wsxd update transfers live terminal ownership to the new daemon without restarting shells, agents, foreground jobs, or listening servers. Open wsx windows stay open: a window whose installed `wsx` was replaced re-executes into it in the same terminal and returns to the same session and mode. Only a window of an older release that cannot talk to the new daemon defers the update.

A starting `wsx` that finds an older wsxd upgrades it before it opens. It shows an upgrade screen with both versions and the number of running terminals. When another window defers the update, the screen says which window to quit, and wsx opens by itself when the update is done. `Enter` opens on the old wsxd at once and `q` quits.

`wsx daemon restart` replaces wsxd with the installed build the same way, also when it is the build that is running. Sessions keep running. It waits for a wsxd that is still recovering its sessions, and answers `deferred` only when another wsx window must quit first. An older wsxd that cannot do this refuses and stops nothing. `wsx daemon stop` ends the programs in every session, so it refuses while sessions are running and needs `--force`; saved session commands restart on the next launch. See [Daemon live upgrade](daemon-live-upgrade.md). Routine requests similarly replace legacy standalone schedulers with the adjacent wsxd automatically.

## Runtime and security

- wsxd belongs to the host and Unix user, not one login session. Same-user SSH reconnects reuse live PTYs and buffers.
- Owner-only sockets and peer-UID checks reject cross-user access.
- One writable lease owns each pane. Explicit Terminal entry transfers control to the latest wsx instance; the displaced instance returns to Workspace, and lease generations reject stale input, resize, heartbeat, selection, and release operations. Events invalidate revisions; clients reconcile from authoritative snapshots.
- Messages, frames, agent exchange prompts, deadlines, write claims, retained receipts, commands, plugin manifests, plugin view output, listeners, and resource counts are bounded.
- UI-only wsx releases reuse the compatible daemon. Protocol 16 adds provider-neutral generation-bound agent exchanges, exited-pane restart, and prompt-bound Claude wake renewal while retaining protocol-15 live handoff. The raw terminal stream that the web client's terminal uses is an added request inside protocol 16, announced by the `raw_terminal_stream` capability. Protocol 11–16 daemons remain usable until their safe one-time transition, and opening wsx keeps the existing workspace visible throughout normal deferral and reconnection.
- Native resume creates a new process, PTY, and terminal buffer from a validated provider reference. Unsupported references open a clean shell.
- `wsx web serve` is the only remote surface. It listens on loopback, accepts one exact public host and origin, authenticates paired devices with hashed per-device credentials, and limits each device to the projects and the capability bits in its site file. Nothing else in wsx accepts network connections.
- Transient graphics preservation across handoff, marketplace installation, and original-process restoration after an unplanned daemon crash are not supported.
