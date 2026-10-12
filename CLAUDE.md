# wsx

Project-first terminal workspace manager: `Project → Worktree → Session → Pane → Terminal`.
`wsxd` owns sessions, PTYs and Ghostty terminal state. `wsx` is the ratatui TUI and CLI client.

Release rules, verified gotchas and the recurring audit axes are in @AGENTS.md.

## Quick Reference

```bash
cargo build -p wsx -p wsx-daemon          # adjacent target/debug/wsx and wsxd
cargo nextest run --workspace --locked    # unit and integration tests
cargo test --workspace --locked --doc     # doctests
python3 scripts/runtime-smoke.py          # real PTY and stream scenarios; rebuild both binaries first
python3 -B scripts/test-web-client.py     # hosted web client journey; add --browser for headless Chrome
```

Tests that open a PTY or a Unix socket fail inside a command sandbox. Rerun them outside it.

## Crates

- `crates/wsx-core/` — published library: `cache`, `config`, `git`, `hooks`, `integration`, `model`, `ops`, `runtime` (wire protocol and client).
- `crates/wsx-terminal/` — PTY runtime and embedded `libghostty-vt` (`vendor/libghostty-vt`).
- `crates/wsx-daemon/` — `wsxd`: state, leases, streams, agent lifecycle, live handoff.
- `crates/wsx/` — TUI (`app.rs`, `ui/`) and CLI (`cli.rs`).
- `crates/wsx-web/` — hosted web client behind `wsx web serve`: `src/` server, `web/` browser files embedded as written (`docs/web-client.md`).

## Where to look

- `docs/` — one contract per file, for example `agent-orchestration.md`, `terminal-exit-ordering.md`, `terminal-synchronized-output.md`, `terminal-raw-stream.md`.
- `crates/wsx-core/src/runtime/protocol.rs` — protocol version and `DAEMON_REVISION`.
- `crates/wsx-core/src/integration/` — agent hook installers and status mapping.
- Unknown project term or prior decision: `/seek <topic>` before guessing.
