# Terminal session preview

The terminal title is one row of presentation chrome, not a clickable switcher or another input mode. It shows the project once, then a sliding window of session chips. The current chip is highlighted with existing theme roles.

```text
project | +2  ○ build   ◉ main > audit (pi)   ◐ fix > tests  +5
```

Chips use padded background regions, not literal brackets. The current chip is brighter and bold; neighboring chips use the existing quiet panel background. Project context and overflow counts stay outside chip backgrounds.

Current identity includes worktree, session and the selected pane label when applicable. Known provider identity reserves space before extra name detail. Same-worktree peers omit redundant worktree names; other-worktree peers retain attribution. Only authoritative lifecycle or ordinary foreground-job state supplies indicators.

## Shared order and overflow

`session_state::context_sessions` supplies stable Workspace order within the current project, including sessions in collapsed worktrees. Lifecycle changes, mute and Done acknowledgement update indicators but do not reshuffle this order. Other projects are excluded.

The visible window stays contiguous and grows around the current session. Counts at either end report off-screen sessions. At narrow sizes, current identity wins; tiny views replace the window with the current chip and its position, such as `4/12`. Zero-width views paint nothing. Names truncate on complete graphemes using display-cell widths. Ports and key hints do not appear in the title.

## Project-local navigation

In Terminal mode, use the configured prefix followed by `j/k` or Down/Up to visit the next/previous session across the current project. The ring wraps and includes off-screen sessions and collapsed worktrees. Switching reveals the exact typed target through existing expansion persistence and dimension-first stream attachment. With no other session, keep the terminal and report that fact.

The duplicate `Prefix+h/l` and Left/Right session-cycle bindings are removed. Those unassigned prefixed combinations follow normal terminal forwarding; bare keys always reach the terminal application. `h/l` are no longer reserved Workspace escape suffixes. Previously migrated configuration is not guessed or rewritten back.

`Prefix+n/N` attention navigation and its preference remain separate and unchanged. Workspace `j/k`, `h/l`, Enter, group navigation and file/diff review keep their existing behavior.

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

The tmux journey creates owner-only repository-local HOME/config/state, a real Git-owned second worktree and synthetic generation-authorized agents. It observes the active PTY's actor marker independently of title formatting. It verifies normal/mobile captures, every overflow target, cross-worktree navigation, wrap, state changes without reorder, attention jumps, bare input, viewport dimensions, sidebar peek and both title positions. The first TUI stays alive across captures. Only its private tmux server, daemon and actors are stopped. No model calls or installed agents are required.

Default scratch cleanup covers failed preparation and runtime exits; `--keep` retains review captures. Existing fixtures are refused without deletion.

## Human Verify

Pass if current identity is easy to find, the strip reads in switching order, and overflow counts do not resemble dead controls. Fail if highlighting is hard to distinguish, peers look clickable, or the title competes with terminal content. Inspect matching narrow/wide captures; automated geometry checks do not prove visual comfort.
