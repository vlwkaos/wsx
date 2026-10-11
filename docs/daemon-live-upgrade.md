# Daemon live upgrade

A newer `wsx` replaces the running `wsxd` through a live handoff. The successor receives the PTY descriptors, the singleton lock and bounded terminal state, so shells and agents keep running. This file states what an open TUI does during that handoff.

## Who defers a handoff

Each TUI poll carries `TuiClientPresence.max_protocol`, the newest protocol that TUI can reconnect to. `handoff_blockers` in `crates/wsx-daemon/src/lib.rs` defers a handoff only for:

- a TUI that sent no presence or `max_protocol` 0 (a release before this contract), and
- a TUI whose `max_protocol` is lower than the target protocol.

Such a TUI cannot talk to the successor, so it must quit first. A TUI at or above the target protocol stays attached.

The running daemon applies its own rule. A daemon older than revision 22 still waits for every TUI to quit, so the first upgrade to this release needs that once.

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

- `crates/wsx-daemon/tests/live_handoff.rs` keeps a real `EventMonitor` and an open terminal stream attached. The handoff must report no blocker, the stream must end with `daemon_handoff`, and the monitor must reach the successor without a `Disconnected` signal.
- `scripts/test-terminal-context.py` replaces the daemon while a real TUI is in Terminal mode. The PTY row must be unchanged, the mode line must still be Terminal, no failure notice may appear, and typed input must reach the same shell.

## Not covered yet

A TUI older than the target protocol still defers the handoff. Replacing that TUI process in place is the remaining step, and it needs a real protocol change to test against.
