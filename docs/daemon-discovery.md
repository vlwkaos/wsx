# Daemon discovery

How `wsx` (TUI and CLI), the agent hook reporter and `wsxd` find each other.
Code: `crates/wsx-core/src/runtime/protocol.rs` (`state_dir`, `default_socket_path`). The same rule is copied where Rust cannot run: the agent hook reporters (`integrations/common/wsx-reporter.py`, `.mjs`) and the check slot in `scripts/check-slot.py`. Change them together; a reporter change bumps every integration version.

## The socket is the daemon

| Path | Rule |
|---|---|
| State folder | `$XDG_STATE_HOME/wsx`, else `$HOME/.local/state/wsx`, else the temp dir. An empty or relative value is ignored (XDG base directory spec), so the folder never depends on the current directory. The task store lives here. |
| Socket | `$WSX_SOCKET` when set and not empty, else `<state folder>/wsx.sock`. |
| Daemon state, singleton lock, routine store | `state.json`, `state.lock`, `routines/` beside the socket (`routine/store.rs:23`). |
| Bootstrap lock, lifecycle marker, reporter | `<socket>.bootstrap.lock`, `<socket>.lifecycle`, `<socket>.reporter`. |

One socket path is one daemon with one state. Two socket paths are two daemons with their own `state.json`, `state.lock`, `routines/` and `<socket>.*` files, so they never fight over a lock. They do share the state folder: the task store and the project shell records in `shells/` (`ops.rs:701-704`) live there, not beside the socket. Before, the state followed `XDG_STATE_HOME` while the socket followed `WSX_SOCKET`: a second socket on the same state root started a wsxd that found the first one's lock and exited, and its client waited for a socket that never came.

## Panes keep their daemon

wsxd gives each pane `WSX_SOCKET=<its socket>` and `WSX_AGENT_REPORT_BIN=<socket>.reporter`. A shell profile that changes `XDG_STATE_HOME` or `HOME` per directory (direnv, a work and a personal profile) does not move the `wsx` CLI, a nested TUI or a hook report in that pane to another daemon. To start a separate daemon from a pane, set `WSX_SOCKET` yourself.

A script that copies its caller's environment and wants its own daemon removes `WSX_SOCKET` (as `scripts/runtime-smoke.py` does) or sets it.

## Errors name the socket

A failed connect reads `<ErrorKind> at <socket>: <error>`, for example `NotFound at /Users/me/.local/state/wsx/wsx.sock: No such file or directory (os error 2)`. The TUI's sticky "Runtime disconnected; retrying" notice shows that line, also before the first snapshot. The notice names one reason per outage; a failed restart joins it as `; restart failed: ...`. A path that is not the one the daemon uses means the client's environment differs from the daemon's.

Verified by `scripts/runtime-smoke.py` (the "daemon-discovery" steps).

## Long paths

A Unix socket path holds 103 bytes on macOS (107 on Linux). The daemon socket and its state files stay together, so wsx does not move the socket to another folder; a path over the limit fails at once with `socket path <path> is <n> bytes and a Unix socket takes 103: set WSX_SOCKET to a shorter path` (`runtime::check_socket_path`, checked by `wsxd` and by every client) in place of std's "path must be shorter than SUN_LEN". The handoff socket `h-<hex>.sock` is 24 bytes longer than the folder it sits in, so when it does not fit beside the daemon's socket it is bound in the task store's socket folder (`<TMPDIR>/wsx-<uid>`, [tasks.md](tasks.md) "Store"); the running daemon names it and hands the path to the new one. Task host sockets already live there. Verified by `live_handoff.rs`.

## Lifetime

A wsxd stops by itself when what it serves goes away. Its routine scheduler stops with it (docs/tasks.md "Routines").

- The socket's directory is gone or replaced. A deleted `.work` folder or task copy then leaves no daemon behind.
- The process named by `WSX_EXIT_WITH_PID` has ended. Tests, fixtures and scripts set it to their own pid, so their daemons exit when they end, also after a panic or a SIGKILL. A value that is not a pid above 1 makes wsxd refuse to start.

Each daemon checks once a second, then stops as it does on SIGTERM. A user daemon has no `WSX_EXIT_WITH_PID` and keeps its state directory, so it runs as before.
