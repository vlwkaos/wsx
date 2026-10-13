# Daemon live upgrade

A newer `wsx` replaces the running `wsxd` through a live handoff. The successor receives the PTY descriptors, the singleton lock and bounded terminal state, so shells and agents keep running. This file states what an open TUI does during that handoff.

## Protocol rule

`PROTOCOL_VERSION` changes only when an older client can no longer talk to the new daemon. Everything else is additive and leaves the number alone:

- A new request, field or event is announced by a `Capabilities` flag or by the daemon revision. A client checks that before it relies on the addition.
- A new field decodes with a default. A client ignores a field it does not know.
- A request the daemon does not know is answered with an error (`invalid_json`) on a matching and on a mismatched connection. The connection is not dropped.
- `DaemonPhase` and `ReplacementBlocker` decode an unknown value as `Unknown`, so a client can read the lifecycle of a newer daemon.

A real protocol change makes every open TUI of the old number leave or re-execute, so it needs a reason that the list above cannot serve.

## Who defers a handoff

Each TUI poll carries `TuiClientPresence.max_protocol`, the newest protocol that TUI can reconnect to, and `follows`, which says that the TUI re-executes into the installed wsx (see below). `handoff_blockers` in `crates/wsx-daemon/src/lib.rs` defers a handoff only for:

- a TUI that sent no presence or `max_protocol` 0 (a release before this contract), and
- a TUI whose `max_protocol` is lower than the target protocol and that does not follow.

Such a TUI cannot talk to the successor, so it must quit first. A TUI at or above the target protocol stays attached. A TUI that follows replaces itself.

The daemon reads every `poll` this way, so a long-lived client that is not a TUI must send presence too. `wsx web serve` sends it with its own protocol and `follows` false (`crates/wsx-web/src/events.rs`): it cannot re-execute itself, and it reconnects by itself at the same protocol, so it does not defer a handoff.

The running daemon applies its own rule. A daemon older than revision 22 still waits for every TUI to quit. A daemon older than revision 25 does not know `follows` (revisions 23 and 24 are the web line before the merge).

A deferred handoff stays pending in the daemon. When the last blocking TUI quits, its presence expires after three seconds and the daemon starts the handoff by itself. The notice in the waiting TUI and the output of `wsx daemon restart` say which window to quit and that sessions keep running.

## A starting TUI waits for the upgrade

A starting TUI asks once, before its first frame, whether a live upgrade is pending (`pending_upgrade` in `crates/wsx-core/src/runtime/client.rs`). A start that needs no upgrade opens as before. A daemon without live handoff also opens as before, because its upgrade restarts sessions.

When an upgrade is pending, the TUI shows the upgrade screen (`crates/wsx/src/ui/upgrade.rs`) and not the workspace: the old and new version and daemon revision, the number of running terminals, and the step.

| Step | What happens | Keys |
| --- | --- | --- |
| Upgrading wsxd | A worker runs the live handoff. The TUI then opens on the new daemon with the upgrade notice. | `q` quit |
| Waiting | A window blocks the handoff. The screen says which window to quit. The TUI asks again every second and opens when the daemon has upgraded. | `Enter` open on the old daemon now, `q` quit |

A handoff that fails rolls back in the source daemon. The client reads the rollback from the lifecycle (ready phase, no target, old revision) and stops waiting. The TUI then opens on the running daemon with a notice. It reads that daemon without requesting the handoff again.

CLI commands show no screen. They keep the one-line notice.

## A TUI follows the daemon

A TUI records the file it was started from. When the daemon connection changes, and every 30 seconds, `crates/wsx/src/follow.rs` checks two facts:

1. The file at that path is now another file: a package manager replaced the install.
2. The running daemon is the `wsxd` beside that file. The packages ship both binaries together, so the installed `wsx` is the daemon's own release.

When both hold, the TUI saves its cursor, restores the shell's terminal modes and replaces its own process with the installed `wsx` (`exec`). The process ID and the terminal stay the same. The new process restores the selected row from the cache. `--followed-from` and `--follow-pane` carry what the cache lacks: the old version for the notice, and the pane that was open in Terminal mode.

The TUI waits while a form, a prompt, a search, a pending operation or a half-typed prefix is open, and follows when that closes.

When the installed `wsx` cannot start, the TUI enters its terminal modes again, keeps running the old code and shows a notice. It does not try the same file again.

The start path is the path the shell resolved, for example the Homebrew link in `bin/`. A TUI started through a path that the upgrade deletes cannot follow. It keeps running, which is safe while the protocol number is unchanged.

## Restart and stop

`wsx daemon restart` requests a live handoff into the installed `wsxd`, also when it is the build that is running. `Request::PrepareHandoff.restart` permits the equal build; a request without it still needs a newer target. The command never falls back to a stop:

| Running daemon | Result |
| --- | --- |
| Advertises `Capabilities::daemon_restart` (revision 25 and later) | Handoff. PTYs, shells and agents keep running. |
| Older, installed `wsxd` is the same build | Refused with one sentence. Nothing is stopped. |
| A TUI blocks the handoff | Pending. It continues when that TUI quits. |

`wsx daemon stop` ends every program in every session. It refuses while a session has a running terminal, prints the number, and names `wsx daemon restart`. `--force` stops anyway. A daemon that cannot report its sessions also needs `--force`.

## Transition marker

When the handoff starts, the source daemon writes `handoff:<unix_ms>` to the lifecycle marker beside the socket, before it tells any client. The successor writes `ready` when it publishes the socket. A rollback also writes `ready`.

`wait_for_handoff` in `crates/wsx-core/src/runtime/client.rs` reads that marker. While it is fresh (35 seconds), a client waits. It does not report a disconnect and does not start another daemon. A stale marker means the source died inside the handoff, and normal recovery applies.

## Single requests

`Client::call` repeats a request once on the successor when a handoff was in progress and the request was certainly not applied: the daemon refused it (`daemon_replacing`, `daemon_unavailable`) or the socket was absent. A connection lost in the middle of a request repeats only for reads (`Snapshot`, `View`, `LifecycleStatus`), because a mutation may already have been applied. A single request waits at most five seconds for the handoff.

The TUI event monitor does not use the repeating call. Its reconnect is the signal that tells the TUI to take a new snapshot.

## Terminal streams

A stream attached during the handoff receives the error `daemon_handoff`. The TUI keeps Terminal mode, waits for the marker, takes a new snapshot, and reattaches only if the same `PaneId` and `TerminalId` are live. It then starts from a new full frame. Keys typed in that interval are dropped, as they are during terminal entry.

A stream that ends without notice (`stream_disconnected`) reattaches the same way once. A second loss within five seconds is reported as a failure.

## Restored screen

The keyboard-state dump from libghostty also contains the visible screen. `TerminalRuntime::from_handoff_fd` applies it first, erases the text, and then writes the bounded history, so the screen is not doubled.

## Verification

- `crates/wsx-daemon/tests/live_handoff.rs` keeps a real `EventMonitor` and an open terminal stream attached. The handoff must report no blocker, the stream must end with `daemon_handoff`, and the monitor must reach the successor without a `Disconnected` signal. A second scenario restarts into the same build: a TUI below the target protocol defers it only when it does not follow, and the restart continues by itself when that TUI's presence expires.
- `scripts/test-terminal-context.py` keeps a real TUI in Terminal mode through a refused stop, a same-build restart, a new install whose `wsx` cannot start, and the re-exec into the repaired install. After each step the PTY row must be unchanged, the mode line must still be Terminal, and typed input must reach the same shell. The re-exec must keep the process ID.

```bash
# A build with a higher daemon revision upgrades by running once; without it the script copies this build.
python3 scripts/test-terminal-context.py --next-wsx <dir>/wsx --next-daemon <dir>/wsxd
# The previous release: its daemon and its open TUI keep two sessions through the upgrade.
python3 scripts/test-terminal-context.py --released-wsx <path>/wsx --released-daemon <path>/wsxd
```

- With `--next-wsx`, the same script starts a TUI of the higher revision while an older window blocks the handoff. The upgrade screen must appear at 80x24 and 40x12 without the workspace, `q` must quit, `Enter` must open the old daemon, and the TUI must open on the new daemon by itself when the blocking window quits. With `--released-wsx` and `--next-daemon` it also starts a TUI whose handoff target refuses the transfer: the daemon rolls back and the TUI opens on it with a notice.
- `scripts/runtime-smoke.py` sends a request of a later release on a matching and on a mismatched connection, and stops a daemon with live sessions only with `--force`.

## Not covered yet

- A handoff across two protocol numbers with a following TUI has no real second protocol to run against. The daemon rule is tested with a presence below the target protocol; the re-exec is tested with the same protocol.
- The "Upgrading wsxd" step lasts only as long as the handoff, so no capture of it exists.
- A TUI of a release before `follows` cannot re-execute. It stays attached while the protocol number is unchanged.
