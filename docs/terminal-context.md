# Terminal session preview

The terminal title is one row of presentation chrome, not a clickable switcher or another input mode. It shows the project once, then a sliding window grouped by worktree.

```text
project  +2  main › ○ build  ◉ audit (pi) │ fix › ◐ tests  +5
```

Each level has one look, and `ui/theme.rs` gives it one role that the row uses nowhere else:

| Level | Role | Look |
|---|---|---|
| Project | `terminal_project` | accent text in a filled chip, once, at the left |
| Worktree | `terminal_worktree` | accent text on the bar itself with a `›`; a heading, never a chip, never a state icon; bold for the current worktree |
| Session | `terminal_session` | a chip that always starts with its state icon; brighter and bold for the current session |

A dim rule (`terminal_group_rule`) takes the one cell between two worktree groups, so a second worktree name does not read as one more session. Sessions of one worktree share one unbroken chip background. Overflow counts stay outside groups. User-supplied punctuation remains unchanged. The Terminal footer names the session and group keys only while another session or group exists.

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

## Claude in a pane

wsxd starts each pane with `CLAUDE_CODE_NO_FLICKER=1` unless the daemon's environment already has that variable; a shell that sets it later wins too. Claude then uses its full-screen renderer ([Claude Code docs](https://code.claude.com/docs/en/fullscreen)), which draws on the alternate screen.

Why: Claude's inline renderer redraws its live area at the new width on every resize, and the rows already in the scrollback cannot be erased, so a reply taller than the screen leaves a full copy in the scrollback each time ([anthropics/claude-code#103181](https://github.com/anthropics/claude-code/issues/103181), seen with Claude Code 2.1.308 on 2026-10-23). A phone at Fit size and a desktop window resize a pane often. Clearing the scrollback on resize was rejected: Claude reprints only part of a long conversation, so the rest would be lost.

What changes: the conversation scrolls inside Claude (wheel, PgUp/PgDn; a drag in the web terminal sends wheel reports), not in the pane's scrollback. `Ctrl+O` then `[` writes it to the scrollback when needed. Select-to-copy inside Claude copies to the host's clipboard. `runtime-smoke.py` checks the variable in a new pane.

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
