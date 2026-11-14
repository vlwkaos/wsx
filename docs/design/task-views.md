# Task views: lists and task page

Design only (task 433). No code changed. The owner's three asks: a better place for the task number and the dot; icons or another compact form for dependency, steps and "what is worked on now", also when every task is "running" and one is blocked; a task page that is short and useful. Choices that have a real alternative are in the 3 questions at the end.

Data: the real task store of this project, read on 2026-11-14 (`wsx task ledger` refuses a worker). The To check and Routines words are samples in the shape of real items; the real To check list was empty. "Before" is the current markup (`tasks.js`, `story.js`) rendered statically; "after" is a static mockup. Both use the real `app.css` and the default theme. Mockup CSS: [task-views/rows.css](task-views/rows.css).

## What is wrong now (390 px, "before")

- Every row: the chip (dot and number) takes 64 px before the title, and line 2 is cut ("Paused, the manager continues it · build 3/3 · i…"). The agent and "into main" repeat on every row.
- Running: `#443` waits for `#442` and `#459` waits for CI, yet each has the same pulsing dot as the task that works.
- To check: three icon buttons leave 125 px for the check ("Open a task that w…"). Task page: five blocks come before the one action; the wait is said twice and the stage a third time.

## One row for the four lists

```
[glyph]  Title, wraps or cuts ............ #444      glyph: who has the ball (the tray's own icon)
         now · relation · relation                  one muted line; "now" first, in tone
```

| Part | Rule |
|---|---|
| Glyph | Replaces the dot and takes the tray's icon: run (`play`, accent) when something runs for the task (agent or a CI check-run); wait (`pause`, muted) when nothing runs and it waits on a task, a limit, the manager or disk; you (`unchecked`, attention) in Needs you; check (`pass`, attention) in To check; failed (`fail`, danger). Shape and the word on line 2 carry the state, not colour. |
| Number | After the title, muted, never cut (as the sheet bar has it). In To check it moves to line 2: it is the task the check belongs to. |
| Line 2, "now" | What it does or waits for, as few words: `build 2/3`, `CI check-run 12 min`, `after #442`, `waits for you`, `limit, resumes 14:00`, `held: deploy freeze`. Run in the working tone, wait in muted, you in attention, failed in danger. |
| Line 2, relations | `blocks #444 #447` (open tasks that wait for it), `in #451` (parent, with `3/12` for a parent), `builds on it` (the `after` reason). Dropped from rows: `into main` (the default), the agent (kept only for `fallback`), the status label that repeats the glyph. |
| Chain | A task nests under the task it waits for (`after`), else under its parent. One rail under the parent's glyph, one step in per level, depth 2, deeper rows flatten with `after #n`. A task with 2 blockers nests under the slowest and says `after #443 and #433`. |

### The lists at 390 px

| | before | after |
|---|---|---|
| Running | ![](task-views/before-running-390-dark.png) | ![](task-views/after-running-390-dark.png) |
| To check | ![](task-views/before-check-390-dark.png) | ![](task-views/after-check-390-dark.png) |
| Routines | ![](task-views/before-routines-390-dark.png) | ![](task-views/after-routines-390-dark.png) |

- Running: `#433` runs, `#442` and `#459` run on CI (their agent idles, so line 2 says "CI check-run 12 min"), four tasks wait, each under what it waits for. A glance gives 3 moving, 4 blocked and the blocker of each.
- To check: the check wraps to 2 lines, "passes if" is readable, the buttons have words and their own line, Pass at the right edge under the thumb. A row is 105 px, was 52 px.
- Routines: the clock icon, red when the last run failed; line 2 is the schedule, the last run, the next one.
- 1280 px keeps a 72ch measure and the number beside the title: [Running before](task-views/before-running-1280-dark.png), [after](task-views/after-running-1280-dark.png); To check [before](task-views/before-check-1280-dark.png), [after](task-views/after-check-1280-dark.png).

## Task page (first screen)

| 390 before | 390 after |
|---|---|
| ![](task-views/before-task-390-dark.png) | ![](task-views/after-task-390-dark.png) |

First screen, top to bottom: (1) title and number; (2) now: glyph, what it waits for, how long; (3) chain: the blockers it waits on, itself, what it blocks, each a tappable chip with its glyph, then `also after #433 · into main` when more than the chain; (4) steps on one line; (5) the actions; (6) the goal, 2 lines. For a task with an open To check item the item rows follow here (they stay open: they are the owner's). A parent shows `3/12 landed` and its children as chips with glyphs in place of the chain.

Folded (not removed from the page): *Why it waits* (the status sentence, usage, agent, the check list, the summary and "Decided"), *Messages*, *History*. 1280 px: [before](task-views/before-task-1280-dark.png), [after](task-views/after-task-1280-dark.png), the sheet is 600 px wide on the right.

Removed from the first screen: the state word in the bar (the now line says it), the health line plus its model and age (merged into the now line), the 5-line status sentence (folded), the "Waits on / Lands into / Blocks" grid (the chain strip says it), `Lands into main` (default branch), the worker's "now" tool line, the agent name, the summary's lead (folded under *Why it waits*, with "Decided"), the "Report" and "Usage" blocks (folded). Net: 13 text lines become 6, and the action moves up next to them.

## TUI (same rules, one line per row, 100 columns)

The TUI already has the tray marks `…` `!` `✓` (`workspace_tree::tray_marks`); a row's glyph column reuses them and adds `‖` (waits) and `✗` (failed). Today a row is `title  #444 working  Waiting for #443 to land · builds on it`.

```
Running 7
… Web polish: task sheet and To check           #433  build 1/1 · blocks #444
… Deploy once per quiet window                  #442  CI check-run 12 min · build 2/3 · blocks #443
  └ ‖ One task state transition table           #443  after #442 · blocks #444 #447
    ├ ‖ One view model for web and TUI          #444  after #443 and #433
    └ ‖ Store cleanup, remove stale features    #447  after #443
… Refactor step 3: one turn record, liveness    #459  CI check-run 8 min · build 3/3 · blocks #432
  └ ‖ Web polish: chat and Home lean            #432  after #459 · its proof waits on it
To check 3
✓ Open a task that waits on another task        #454  passes if the sheet names what it waits on
```

Under 64 columns line 2's tail goes first, then the number; the glyph and the title stay. Detail, first screen (`Details` below it, scrolled to):

```
#444 One view model for web and TUI
‖ Waits for #443 to land · 1 h
Chain    … #442 › ‖ #443 › [#444] › ! #439       also after #433 · into main
Steps    plan · build · review                      (current step reversed)
Keys     H take over   a answer   [ ] chain   Enter opens the chain's task
What     Move the status words, counts, needs and rows that tasks.js, store.js …
Details  agent, usage, checks, result, status sentence, history
```

`[` and `]` select the task before and after in the chain (the same keys as the chat's links); `Enter` opens it, `Esc` returns.

## Where it lives

wsx-core owns it, for step 8 of docs/design/root-cause-refactor.md: one row view (`view::row`) per task with `glyph` (`run|wait|you|check|failed`), `now`, `relations`, `under` and `depth`, ordered in chain order. Web and TUI only draw it. The glyph and `now` read `waits_on` (step 5); until it lands they read `status.kind`, `flow.after` and the check-run wait. Asserts to change at build: the row texts in `scripts/web-tasks-check.mjs`, the `Waits on`, `Goes` and `Blocks` rows in `scripts/test-task.py` (TUI detail), docs/web-client-tasks.md "Project screen" and "Task sheet", docs/tasks.md "TUI".

## Questions

1. **The glyph says who has the ball** (run, wait, you, check, failed) and replaces the state dot; the number moves after the title. Alternative: keep the dot and only move the number. Recommended: the glyph. The dot's colour is the only signal today, and a task that waits on CI or on another task looks like one that works.
2. **Running nests a task under the task it waits for** (depth 2, the same in the TUI). Alternative: a flat list with `after #442` on line 2. Recommended: nested. It shows the chain and which blocker holds up the most work; the cost is that the list order is no longer by number, and the tree is computed once in wsx-core.
3. **To check keeps Reply, Fail and Pass on the row**, in their own line with words. Alternative: no buttons on the row; a tap opens the item's sheet. Recommended: keep them on the row. It is the quick path on a phone (#454 chose it); the row grows from 52 to 105 px.

## Proof the build will give

`check-run web task smoke` on the final head; design lint at 390 and 1280 on every changed screen (the mockups pass with no error; two warns kept: the 165 px gap before Pass in To check at 390, which puts Pass under the thumb, and the stub rail of the mockup); captures at 375, 390, 1280 in light and dark.
