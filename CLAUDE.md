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

## Workspace layout

Cargo workspace with two member crates (since `feat/extract-wsx-core`):

- `crates/wsx-core/` — library (ratatui-free). Exported modules:
  `cache`, `config`, `git`, `hooks`, `model`, `ops`, `proc_tree`, `tmux`.
  Consumable by external orchestrators (notably `auwsx`).
- `crates/wsx/` — TUI/CLI binary. Depends on `wsx-core` via path.

## Key Files

- `crates/wsx/src/main.rs` — entry, tmux check, panic hook, dispatches TUI vs CLI subcommand
- `crates/wsx/src/app.rs` — state machine, event loop, action dispatch
- `crates/wsx/src/ui/` — ratatui render code
- `crates/wsx/src/cli.rs` — non-interactive subcommands (status / worktree / session / tab)
- `crates/wsx-core/src/ops.rs` — workspace business logic (worktree/session ops)
- `crates/wsx-core/src/tmux/` — tmux shell commands
- `crates/wsx-core/src/git/` — git CLI wrappers
- `crates/wsx-core/src/config/` — global + per-project config (TOML)
- `crates/wsx-core/src/model/workspace.rs` — data model (Project → Worktree → Session)
- `crates/wsx-core/src/hooks.rs` — `.gtrconfig` post-create + gitignored copy
- `crates/wsx-core/src/cache.rs` — startup session-snapshot cache
