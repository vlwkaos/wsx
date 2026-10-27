# Daemon discovery

How `wsx` (TUI and CLI), the agent hook reporter and `wsxd` find each other.
Code: `crates/wsx-core/src/runtime/protocol.rs` (`state_dir`, `default_socket_path`).

## The socket is the daemon

| Path | Rule |
|---|---|
| State folder | `$XDG_STATE_HOME/wsx`, else `$HOME/.local/state/wsx`, else the temp dir. An empty or relative value is ignored (XDG base directory spec), so the folder never depends on the current directory. The task store lives here. |
| Socket | `$WSX_SOCKET` when set and not empty, else `<state folder>/wsx.sock`. |
| Daemon state, singleton lock | `state.json`, `state.lock` beside the socket. |
| Bootstrap lock, lifecycle marker, reporter | `<socket>.bootstrap.lock`, `<socket>.lifecycle`, `<socket>.reporter`. |

One socket path is one daemon with one state. Two socket paths are two daemons that share no file, so they never fight over a lock. Before, the state followed `XDG_STATE_HOME` while the socket followed `WSX_SOCKET`: a second socket on the same state root started a wsxd that found the first one's lock and exited, and its client waited for a socket that never came.

## Panes keep their daemon

wsxd gives each pane `WSX_SOCKET=<its socket>` and `WSX_AGENT_REPORT_BIN=<socket>.reporter`. A shell profile that changes `XDG_STATE_HOME` or `HOME` per directory (direnv, a work and a personal profile) does not move the `wsx` CLI, a nested TUI or a hook report in that pane to another daemon. To start a separate daemon from a pane, set `WSX_SOCKET` yourself.

A script that copies its caller's environment and wants its own daemon removes `WSX_SOCKET` (as `scripts/runtime-smoke.py` does) or sets it.

## Errors name the socket

A failed connect reads `<ErrorKind> at <socket>: <error>`, for example `NotFound at /Users/me/.local/state/wsx/wsx.sock: No such file or directory (os error 2)`. The TUI's sticky "Runtime disconnected; retrying" notice shows that line, also before the first snapshot. A path that is not the one the daemon uses means the client's environment differs from the daemon's.

Verified by `scripts/runtime-smoke.py` (the "daemon-discovery" steps).
