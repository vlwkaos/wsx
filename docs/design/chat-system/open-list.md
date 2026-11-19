# The open For you, Tasks and Routines lists: buttons beside the text (task 526, owner messages 5412, 5474)

Mockups only; nothing in `crates/` changed. The closed pills, Home and the corners are as #519 left them.
`build.py` (section "Task 526") writes `x-open-list-<page>-before|after-<light|dark>.html` and `open-list-<page>-<scheme>-<390|1280>.html`
(two iframes side by side); `CHROME_BIN=<chrome --no-sandbox> node capture.mjs open-list` shoots them to `open-list-<page>-<scheme>-<390|1280>.png`;
`node open-list-measure.mjs` prints the numbers below. Before = `open-before.css`, the real `app.css` of commit `b94b748c`. After = the same
plus `open-list.css` (the build's CSS written as overrides). Pages: `foryou`, `foryou-menu` (the more menu open), `tasks`, `routines`.

## Cause

The row's actions are in a line of their own by design: `.reservoir .row-link` is one grid column, so `.row-actions` wraps under the text, and
`.reservoir .need-action.button` is 44 px high with icon and word. Each item with an action costs a second line of 44 px buttons.
The Tasks list has no action in any row (`TaskRow` and `WaitingRow` pass none), so it has no button line: it is the same before and after.

## After

| Part | Before | After |
| --- | --- | --- |
| Row | text, then the actions in a line of their own | text (takes the width left, wraps) and the actions on one line |
| Action, 390 px | icon and word, 44 px high, raised chip | icon only, chip drawn 32 px, box 44 x 44 (hit area); word is `title` and `aria-label` |
| Action, 1280 px | same | icon and word, chip 32 px high, box 44 px high |
| 3 or more actions (To check: Reply, Fail, Pass) | three buttons in the line | the last (Pass) stays; the rest are in a `⋯` menu (Reply, Fail), same at every width |
| Routines (Turn off or on, Run now) | two icon buttons in a line of their own | the same two icon buttons beside the text |

Hit areas: the 44 px box is the button; the drawn chip is a `::before` inside it (`inset` 6 px), so two neighbours never overlap. Measured: every action box is 44 x 44 or wider.
The actions' negative block margin (`-space-1`) keeps a one-line row at 44 px.

## Rows per screen (the list's own height: 40% of the chat, 273 px at 390 x 844, 256 px at 1280 x 800)

`fullRows` = rows that fit the list whole; `row` = mean row height; actions on a line of their own = rows where the buttons are under the text.

| List | Width | Rows fit before | Rows fit after | Row before | Row after | Rows with actions on their own line, before / after |
| --- | --- | --- | --- | --- | --- | --- |
| For you (7 rows) | 390 | 2 | 4 | 112 px | 64 px | 7 / 0 |
| For you | 1280 | 2 | 4 | 109 px | 53 px | 7 / 0 |
| Routines (6 rows) | 390 | 2 | 3 | 127 px | 74 px | 6 / 0 |
| Routines | 1280 | 2 | 3 | 127 px | 71 px | 6 / 0 |
| Tasks (6 rows) | 390, 1280 | 3 | 3 | 53 px | 53 px | 0 / 0 |

Total height of the 7 For you rows: 783 px before, 445 px after (390 px). Routines: 764 px before, 447 px after.

## Decisions for the owner

1. Icon only on a phone. Reply, Dismiss (x), Deny (x, red), Approve and Pass (check, green) read from the icon and its colour; the word is the accessible name and the title. Alternative: keep a short word on the main action only (more width, fewer rows saved).
2. Three actions: the main one plus `⋯`, at every width. Alternative: all three inline from 600 px, which keeps Fail one tap away on a desktop.
3. The `⋯` menu must not be cut by the list's scroll area. The mock (`foryou-menu`) lets it overflow; the build makes it a popover (top layer) set outside `.chat`, whose size container holds fixed boxes (the reply menu does the same, `app.css` near `.manager .chat`).
4. Tasks list: nothing to change today. If the owner meant the To check and question items inside it, they are in For you.

## Build turn (after the owner picks, after #518 lands app.css)

`tasks.js` `rowButton` puts the word in `<span class="need-word">`; a row with 3 or more actions renders the last one plus the `⋯` menu (`MoreMenu` in `ui.js` as base). `app.css`: replace `.reservoir .row-link` (grid), `.reservoir .row-actions` and `.reservoir .need-action.button` by the rules of `open-list.css` (fold the high-specificity doubling into the old rule). Proof: design lint and `check-run web` on the final head.
