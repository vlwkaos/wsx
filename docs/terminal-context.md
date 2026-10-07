# Terminal session preview

The terminal title is one row of presentation chrome, not a clickable switcher or another input mode. It shows the project once, then a sliding window grouped by worktree.

```text
project  +2  main › ○ build  ◉ audit (pi)   fix › ◐ tests  +5
```

The project and worktree groups use padded background regions, not literal bars, brackets or borders. Each worktree name appears once before its visible sessions, with a typographic `›`. The current session is brighter and bold. Sessions in the same worktree share their parent's background; overflow counts stay outside groups. User-supplied punctuation remains unchanged.

Current identity includes session and the selected pane label when applicable. Known provider identity reserves space before extra name detail. At tiny widths, current identity and position outrank parent context. Only authoritative lifecycle or ordinary foreground-job state supplies indicators.

## Shared order and overflow

`session_state::context_sessions` supplies stable Workspace order within the current project, including sessions in collapsed worktrees. Lifecycle changes, mute and Done acknowledgement update indicators but do not reshuffle this order. Other projects are excluded.

The visible window stays contiguous and grows around the current session. Counts at either end report off-screen sessions. At narrow sizes, current identity wins; tiny views replace the window with the current chip and its position, such as `4/12`. Zero-width views paint nothing. Names truncate on complete graphemes using display-cell widths. Ports and key hints do not appear in the title.

## Project-local navigation

In Terminal mode, use the configured prefix followed by `h/l` for previous/next session across the current project. Left/Right, `k/j` and Up/Down remain aliases. The ring wraps and includes off-screen sessions and collapsed worktrees. Switching reveals the exact typed target through existing expansion persistence and dimension-first stream attachment. With no other session, keep the terminal and report that fact.

Bare keys always reach the terminal application. An explicitly configured `h` or `l` escape suffix wins over its navigation alias; hints advertise the remaining working keys. Existing configuration is not guessed or rewritten.

`Prefix+n/N` attention navigation and its preference remain separate and unchanged. Workspace `j/k`, `h/l`, Enter, group navigation and file/diff review keep their existing behavior.

## Workspace search

`/` searches logical entries in the selected group, including folded worktrees, sessions, panes and routines. Search never unfolds a node or changes activity provenance. Hidden matches map to the highest visible ancestor, with a count on that row. The footer reports the total logical match count separately from visible navigation anchors.

Matching visible text is highlighted on complete terminal cells. Matching ancestors are underlined even when the matching child is hidden. Unicode case conversion and wide-cell occupancy preserve the label and its geometry. Query edits update counts; clearing or leaving search removes feedback. At tiny widths, the footer count takes priority over query detail.

## Ports and viewport ownership

Worktree previews show each session's sorted, deduplicated pane listener ports on its session row. Narrow rows compact the port list with overflow. The preview detail always shows ports independently of the Workspace session-row visibility setting.

`TerminalLayout` still owns title/content rectangles. Top/bottom title placement, compact/expanded sidebars, mobile layout, cursor projection and mouse coordinates retain the existing contract. The title background never paints terminal content. Session and pane previews share this projection. No daemon revision, wire field or lease behavior changes.

## Verification

```sh
cargo build --locked -p wsx -p wsx-daemon
cargo nextest run -p wsx --locked
python3 -B scripts/test-terminal-context-harness.py
python3 -B scripts/test-terminal-context.py --keep
```

The tmux journey creates owner-only repository-local HOME/config/state, a real Git-owned second worktree and synthetic generation-authorized agents. It observes the active PTY's actor marker independently of title formatting. It verifies normal/mobile captures, every overflow target, cross-worktree navigation, h/l and j/k aliases, wrap, state changes without reorder, attention jumps, bare input, viewport dimensions, sidebar peek, both title positions, and folded search counts through query clearing and exit. The first TUI stays alive across captures. Only its private tmux server, daemon and actors are stopped. No model calls or installed agents are required.

Default scratch cleanup covers failed preparation and runtime exits; `--keep` retains review captures. Existing fixtures are refused without deletion.

## Human Verify

Pass if current identity is easy to find, the strip reads in switching order, and overflow counts do not resemble dead controls. Fail if highlighting is hard to distinguish, peers look clickable, or the title competes with terminal content. Inspect matching narrow/wide captures; automated geometry checks do not prove visual comfort.
