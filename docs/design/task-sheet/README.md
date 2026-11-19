# Task sheet (task 533): causes and mockups

Owner message 5524: the sheet of task 530 (ready) is "bad". Rules that stand: no verbose detail page (11-14), smaller buttons that use the space (11-19), all corners round like the Home menu. This turn: read-only, no product change.

## Causes (read from the code)

| # | What the owner sees | Cause |
|---|---|---|
| 1 | Goal line reads "Goal." | `tasks.rs:730-745` `goal()` strips only a `Goal:` lead (`:` required). The prompt began `Goal.`, so the lead stays and `view.rs:279` `first_sentence` ends at the dot: "Goal.". `story.js:163-168` `goalOf` hides the goal only when it prefixes the title, so it shows. A prompt's first sentence is not a goal; the title is already written for the owner. |
| 2 | First status line half hidden under the header | Not reproduced. In the mock (real app.css, scrollTop 0) the first line starts 13 px under the header foot on every page (`measure.json`, `covered` false on all 32 pages). The screenshot's line is about 26 px higher, so the sheet was scrolled by about that much. No code scrolls `.scroll` on open (`tasks.js:682-718`, `task.js` have no scroll call). Unproven: a small scroll gesture on the phone. |
| 3 | The state is said twice | `story.js:145` shows `status.label` (green). `story.js:230,243` shows `status.sentence` again, and `status.rs:791-805` `words()` builds that sentence as label + " for 23 min" + facts + next. |
| 4 | Land cut to "and" | `story.js:243` puts the sentence (`app.css:1179`, basis 180 px) and the actions (`app.css:1030`: `.task-next` flex 0 1 auto, min-width 0, `overflow-wrap: anywhere`) on one row. The buttons inherit `anywhere`, so a button's min-content is one letter and it can shrink under its word. Not reproduced in headless Chrome: 0 clipped buttons at 23 widths 320-430 px; at 410 px and wider the buttons sit beside the text unclipped. The phone's font metrics put the row just at the limit. |
| 5 | The worker's text fills the middle | `story.js:223-241` shows the summary's first paragraph whenever `summaryIsCurrent` (ready). A worker puts its question to the owner in that paragraph, so More only hides later paragraphs. |

## After (sheet.css, tokens only)

- The bar is the title and its number. No state word in the bar (the landed sheet's "done" goes).
- One state line: dot, `status.label`, age. The goal line goes.
- A task that asks the owner shows the question as one line (`waiting.reason`), not the worker's text.
- Working only: workflow steps and the `now` line.
- Actions on their own line: `--size-key` high, round (`--size-key / 2`), `flex: 0 0 auto`, no wrap of a word, wrap between buttons. The one decision (Land, Answer) is `button-primary`.
- One fold, More: model, where it lands, the rest of the status sentence, the worker's whole summary. Report, Messages, History stay as they are.

## Measures (`node capture.mjs`, `measure.json`, `sweep.json`)

| | pages (4 states x dark/light x 390/1280) | buttons | pages with a clipped button | pages with the first line under the header |
|---|---|---|---|---|
| before | 16 | 52 | 0 | 0 |
| after | 16 | 40 | 0 | 0 |

Sweep, owner state (3 buttons), 23 widths 320-430 px: before 0 clipped, after 0 clipped. The before numbers do not reproduce causes 2 and 4 (see above); the after numbers show the layout holds.

## Step 2 needs

- `goal()` and `goalOf` removed; `detail.goal` removed if nothing else reads it.
- The wire gives the status's parts (label, age, facts, next) so the client does not cut the sentence. `status::words` (status.rs:783) owns them.
- `Progress` and `TaskStory` become the state line, the ask line, the actions and More; `app.css` takes `sheet.css`.
- Scenario asserts that read "Goal", `.story-next` or `.task-next` in `scripts/test-web-client.py` and `scripts/web-*-check.mjs` change with it.

## Files

`build.py` makes the pages from `../chat-system/build.py`'s helpers (run from its source, not copied). `before.css` is app.css at fe0a7c25. `capture.mjs` shoots `sheet-<state>-<dark|light>-<390|1280>.png` (before left, after right) and measures. `CHROME_BIN=<chrome --no-sandbox> node capture.mjs`.
