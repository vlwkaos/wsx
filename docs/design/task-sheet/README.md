# Task sheet (task 533): causes and mockups

Owner message 5524: the sheet of task 530 (ready) is "bad". Rules that stand: no verbose detail page (11-14), smaller buttons that use the space (11-19), all corners round like the Home menu. This turn: read-only, no product change.

## Causes (read from the code)

| # | What the owner sees | Cause |
|---|---|---|
| 1 | Goal line reads "Goal." | `tasks.rs:730-745` `goal()` strips only a `Goal:` lead (`:` required). The prompt began `Goal.`, so the lead stays and `view.rs:279` `first_sentence` ends at the dot: "Goal.". `story.js:163-168` `goalOf` hides the goal only when it prefixes the title, so it shows. A prompt's first sentence is not a goal; the title is already written for the owner. |
| 2 | First status line half hidden under the header | Not a layout fault. In WebKit 26 at the phone's size (below) the old sheet starts its first line 12 px under the header foot and `.scroll` is at scrollTop 0 in all four states (`real-webkit.mjs`). In the screenshot the first line sits about 26 css px higher than that, so the sheet was scrolled by about that much; a scrolled list always cuts its first line under the header. Nothing in `tasks.js:682-718` or `task.js` scrolls `.scroll` on open. |
| 3 | The state is said twice | `story.js:145` shows `status.label` (green). `story.js:230,243` shows `status.sentence` again, and `status.rs:791-805` `words()` builds that sentence as label + " for 23 min" + facts + next. |
| 4 | Land cut to "and" | Not a layout fault: the owner's red stroke crosses the "L" (it also crosses "R" of Ready, "plan", "Lands into", "the"). In WebKit at 402 x 874 the old sheet shows Land whole, 0 clipped buttons at every width 340-430 px. What the screenshot does show is the row of `story.js:243`: the sentence (`app.css:1179`, basis 180 px) beside the actions (`app.css:1030`), which starts at 402 px width (ready) and 419 px (owner) and squeezes the sentence into a 165 px column of six lines. That row is gone in the new sheet. |
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

## The real sheet (real.mjs)

`CHROME_BIN=<chrome --no-sandbox> node real.mjs [--rev <commit>] [--shots]` runs the product's own task.js, story.js, app.css and theme tokens in
headless Chrome, with only the answer of `api/tasks/<project>/t/<n>` canned (four states). It measures clipped buttons, the first line under the
header, sideways scroll and an opened-scrolled sheet at 390 and 1280 px in both schemes, and every phone width 320 to 430 px for two states.
Chrome result at 32709399 (before) and at HEAD (after): 0 clipped buttons, 0 covered first lines, 0 opened scrolled on both. `--shots` writes `real-<state>-<scheme>-<width>.png` (`real-before-...` with `--rev`).

## The owner's phone in WebKit (real-webkit.mjs, pair-webkit.mjs)

The screenshot is 920 x 2000 px: ratio 0.4600 = 402 x 874 css px (iPhone 16/17 Pro, DPR 3; 393 x 852 would be 0.4613), in the home-screen app
(`assets.rs:196` `"display": "standalone"`, `viewport-fit=cover`). `real-webkit.mjs` runs the product files in Playwright's WebKit 26.0 (a scratch copy of
playwright-core 1.55.0 in git-ignored `.work/pw`, not a repo dependency) with that viewport and DPR, `iPhone 15` device flags, and the top inset 62 and
bottom inset 34 put where app.css reads `env(safe-area-inset-*)` (WebKit has none). `--stale` sets `--viewport-height` 812 px and `--viewport-top` 62 px, the
state `main.js:43-52` `followViewport` leaves while the composer has the focus: with it the sheet starts at 184 px, as in the screenshot (without it 131 px).
`--sweep` measures every width 340-430 px. `pair-webkit.mjs` joins the before and after shots into `pair-<state>.png`.

| Old sheet, WebKit 402 x 874 | Result |
|---|---|
| First line against the header foot | 12 px below it in all four states, scrollTop 0 |
| Clipped buttons, 340-430 px, ready and owner | 0 of 182 pages |
| Sentence beside the buttons | ready from 402 px, owner from 419 px (the screenshot's layout) |

After (the new sheet): the same measure gives 0 clipped, 0 covered at 402 x 874 with and without `--stale`, and the first line starts 21 px under the header foot.
Not reproduced: the screenshot's sheet scrolled by 26 px, and its title cut at "Home bo…" (6 px less room in its bar than here).

## The shared sheet header and body (owner message 5624, the To check sheet)

The screenshot is again 402 x 874. Old sheet in WebKit (`real-webkit.mjs --checks --stale --rev 40560ec2`): the header's "All passed" is a full `.button` (`app.css:220`, min-height 44, 15 px text, 16 px padding), 44 px high in a 48 px bar, 3 px left below it, and the first item's chip sits 13 px under the bar's rule. One owner for every sheet: `:is(.bar-tools, .task-next) .button` (the small button: a 36 px pill inside the 44 px hit area the lint asks for, by a transparent border and `background-clip`) and `.task-page .scroll { padding-top }` (the body of the task, To check, For you, overview, usage, panels sheets).

| To check, WebKit 402 x 874 | Before | After |
|---|---|---|
| "All passed", pill height / text / width | 44 px / 15 px / 104 px | 36 px / 13 px / 88 px |
| Gap below the pill in the 48 px bar | 3 px | 7 px |
| First item's text under the bar's rule | about 13 px | about 21 px |

`pair-checks.png` is before | after. The task sheet's actions now use the same small button (36 px pills, hit area 44 px).
