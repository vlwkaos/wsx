# Refactor audit: task #264 (question jump, composer pills)

Status: read-only audit, 2026-11-07, HEAD `5b15f5de`. Scope: `git diff 521e4401..5b15f5de` (`0a2fbdfb` and 8 `web check:` commits; 11 files, +388 -310). Same method and format as [refactor-audit-cli.md](refactor-audit-cli.md). Paths are relative to the repo root. Nothing was built or run.

**Answer.** No dead code from the old reservoir head. Five small findings: two comments and a doc claim that still say 600 px (the code says 900), one lookup written twice in `tasks.js`, and three test repeats in `scripts/web-tasks-check.mjs`. Each is under 15 lines to fix.

Proof marks. **R** = I read the lines on HEAD. **G** = I confirmed it with `rg` or a script.

## Findings, ranked

| # | Sev | Where | Problem | Fix | Size | Proof |
|---|---|---|---|---|---|---|
| F1 | low | `crates/wsx-web/web/app.css:800`; `scripts/web-tasks-check.mjs:837` | Both say the pill label shows "from 600 px". The wide layout starts at 900 px (`web/main.js:21` `min-width: 900px`; `.app[data-wide]`, `app.css:815`). `docs/web-client-tasks.md:49`, `CHANGELOG.md:21` and the check at `:968` say 900. | Write 900 in the two comments. | 2 lines | R, G (`rg '600 px'`: only these two) |
| F2 | low | `web/tasks.js:899-904` (layout effect after a quote jump) and `:948-953` `goToReply` (0a2fbdfb) | Both do `logNode.current?.querySelector(':scope > [data-id="…"]')`, then `reveal(node); markFound(node)`. | One `showRow(id)` that returns whether the row was there; the effect calls it, `goToReply` is `showRow(id) \|\| jump.current(id)`. | -5 lines | R |
| F3 | low | `scripts/web-tasks-check.mjs:887-892` and `:905-912` | The count test runs twice, once with motion and once reduced: add two runners, `until "3 running"`, check the tick, `restored()`, `until "1 running"`. The two `until` strings repeat 2 and 3 times. `pillWords() === "1 running \| 6 need you \| 2 to check"` is asserted at `:842` and `:865`. | `const runningIs = (label) => until(…)`; `const expectPills = (where)`. Keep the two motion halves; share the waits. | -8 lines | R, G (`rg "'1 running'"`) |
| F4 | low | `scripts/web-tasks-check.mjs:1057` (0a2fbdfb); the same in `:922`, `:960`, `:1026`, `:1053`, `:1066` | `pause(400)` after `openReservoir` waits for the list's open animation. Only `:1057` is new in #264; the other five moved with the block. The sheet check at `:79` already waits on `getAnimations().length === 0`. | `openReservoir` ends with `until(".reservoir-reveal" has no running animations)` (also true at once under reduced motion); delete the six pauses. `:960` (1500 ms) waits for `readTaskAhead`; wait for the sheet's detail instead. | -6 lines | R, G (`git blame` of each line) |
| F5 | low | `scripts/web-tasks-check.mjs:2848-2858` (`bubbleShown`) and `:2909-2912` (`around`) | Two copies of "the node's box is inside the log's box and has `data-found`", keyed by `data-key="reply-<id>"` and `data-id`. | `const markedInLog = (selector)` used by both. | -6 lines | R |

## Not found (checked)

| Suspect | Result | Proof |
|---|---|---|
| `.reservoir-head`, ticker, rotation CSS | None left. The only `rotate` rules are arrow icons (`app.css:436,664,947`). | G (`rg 'reservoir-head\|ticker\|rotat' app.css`) |
| `calm` unused | Used at `tasks.js:35,58` (`useLeaving`, `useFlag`). | G |
| `TICK_MS` | Does not exist in `crates/wsx-web/web`. | G |
| Unused import in `tasks.js` | All 50 imported names are used at least once. | G (script) |
| Unused CSS in `app.css:700-830` | Every class is in some `web/*.js`. Every `@keyframes` is referenced (`tick-in`, `row-out`, `reveal` too). | G (script) |
| `goToReply` duplicates the quote jump | Only through F2. `Quote.go` (`reply.js:46-61`) must find a row by DOM order when it has no id; its id branch only dispatches `quote-jump`. Merging them changes scroll style (`reveal` against smooth `scrollIntoView`). | R |
| Docs for the TUI Enter on a question | Match: `docs/tasks.md:369` names it, `hints` shows "Enter: show". | R |

## Notes

- `tasks.js:22` `calm` (a9bcbca6, not #264) is the inverse of the check in `lib.js:380` `motion()`. One shared `reducedMotion()` in `lib.js` would serve both. Small; outside this scope.
- `docs/tasks.md:372` says "`i` or `Enter` writes on the last line" and does not say that `Enter` on a question item or a link does something else. `:369` and `:371` do. One clause.
- Tests of #264 are fine in shape: the Questions block reads the fixture's ids, not text order, and `until` has no fixed delay. The `test-web-client.py` fixture (230 `sent` entries) is the reason a tap reads "around"; the comment says so.

## Order

F1 and F2 touch `app.css`, `tasks.js` and one script line: one commit. F3, F4 and F5 edit only `web-tasks-check.mjs`: one commit, after F1 (same file). The check needs `wsx task check-run web` once at the end.
