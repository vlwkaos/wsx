# wsx

Rust TUI workspace manager: git worktrees + tmux sessions via ratatui.

## Session Start

Run `/load` to load project context from `~/knowledge/wsx/`.
Run `/load {task}` when working on an unfamiliar area.

## Quick Reference

```bash
cargo build          # compile
./target/debug/wsx   # run (must be inside tmux)
```

## Key Files

- `src/app.rs` — state machine, event loop, action dispatch
- `src/ops.rs` — workspace business logic (worktree/session ops)
- `src/ui/` — ratatui render code
- `src/tmux/` — tmux shell commands
- `src/git/` — git CLI wrappers
- `src/config/` — global + per-project config (TOML)
- `src/model/workspace.rs` — data model (Project → Worktree → Session)
