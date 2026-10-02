# Terminal session context

The colored terminal title is one row of presentation chrome, not a clickable session switcher. The current session and its authoritative state stay on the left. Project/worktree context is secondary. Same-project peers appear beside it, including sessions in collapsed worktrees.

## Shared order

`session_state::context_sessions` derives the order from current normalized session state:

1. Blocked and Error.
2. Unacknowledged Done.
3. Working agents and ordinary Running jobs.
4. Idle, Unknown, muted, and acknowledged Done.

Workspace order breaks ties within a tier. Rendering omits the current `SessionId`; a peer in another worktree includes its worktree label. Names use complete graphemes and display-cell widths. Peers that do not fit become an explicit `+N` count. Ports and key controls do not appear in the title.

## Project-local navigation

In Terminal mode, use the configured prefix followed by `h/l` or Left/Right to visit the previous/next session in the shared ranked project ring. The ring includes the current session, overflow peers, and collapsed worktrees. It wraps and recomputes from live state, not a cached navigation queue. The command reveals a collapsed target through existing expansion persistence and uses the normal dimension-first stream attach gate. With no other session, it keeps the current terminal and reports that fact.

The title always keeps its current identity on the left and displays the remaining peers attention-first. Navigation follows the complete ranked ring, not repeatedly the first displayed peer. For example, a Working current session followed by an Idle peer advances to Idle, then wraps to Blocked. Entering Done acknowledges its exact revision, so its normalized rank changes to Idle.

Existing `Prefix+n/N` attention navigation and its preference remain unchanged. `Prefix+j/k` still visits worktree siblings. Bare `h/l` and arrows reach the terminal application. Workspace `h/l`, Enter, group navigation, and file/diff review retain their existing behavior. `h/l` are now reserved prefix command suffixes, like `j/k`. If a custom Workspace escape uses either suffix, choose another suffix.

## Ports and viewport ownership

Worktree previews show each session's sorted, deduplicated pane listener ports on its session row. Narrow rows compact the port list with overflow instead of recreating a detached worktree-wide Ports row. This detail view always shows ports independently of the Workspace session-row visibility setting.

`TerminalLayout` remains the sole title/viewport geometry owner. Top/bottom placement, compact/expanded sidebars, mobile layout, cursor projection, and mouse coordinates retain the existing contract. The title's background never paints the terminal content. No daemon revision, wire field, or lease behavior changes.

## Verification

Build adjacent binaries before installation or real-TUI tests:

```sh
cargo build --locked -p wsx -p wsx-daemon
cargo nextest run -p wsx --locked
python3 -B scripts/test-terminal-context-harness.py
python3 scripts/test-terminal-context.py
```

The tmux scenario creates an owner-only repository-local fixture with isolated HOME/config/state and synthetic generation-authorized sessions. It checks actual rendered rows, kernel-observed listener ownership, stream dimensions, prefix navigation, live peer updates, and both title positions. It keeps the first TUI alive across captures and shuts down only its private tmux server and daemon. `--wsx` and `--daemon` accept adjacent isolated builds. Default scratch cleanup covers failed preparation as well as the runtime scenario; `--keep` explicitly retains diagnostics. A pre-existing fixture is refused without deleting its data. No model calls or installed agents are required.

## Human Verify

Pass if the current session is immediately identifiable, peer state colors remain legible, and the bounded background does not distract from terminal content at narrow and wide sizes. Fail if labels or colors obscure current identity, peers suggest a false state, or chrome spills into terminal content.
