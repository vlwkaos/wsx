# pr-review manager: root-cause review (2026-11-15)

Read-only review of the pr-review project's manager, asked by the owner on 2026-11-15.
Source: `~/.local/state/wsx/tasks/pr-review-d10dc66b/manager/` (`log.jsonl`, `notes/`, `../memory/records.jsonl`).
Times are local (KST). Ids are `log.jsonl` message and reply ids.

## Answer

Three causes explain 16 of the 17 owner corrections in the last 7 days. Two are in wsx, one is in pr-review's notes.

| # | Cause | Corrections it explains | Owner of the fix |
|---|---|---|---|
| 1 | The manager does the evaluation itself in chat turns: it reads code, scores and writes the commentary, and nothing reviews its own output | 11: 234, 242, 254, 300, 308, 438, 442, 444, 470, 474, 482 | wsx `instructions.rs:51`, and pr-review `decisions.md` line 8 |
| 2 | Decisions lose their meaning: the rule view shows the owner's excerpt, not the rule, and one decision was never recorded | 3: 427, 476, 444 (shared with 1) | wsx `memory/manage.rs:127` `rule_line` |
| 3 | One Sonnet build-only task (#6) carries every team's scoring, and its results come back unchecked | 4: 295, 427 (shared with 2), 454, 456 | pr-review `decisions.md` line 6 |

Ruled out: stand-in or fallback agents (236 of 236 turns ran on `claude-opus-5-5`), a rotation that dropped a recorded rule (no rule was hidden before 11-15 08:31, see §3), and the instruction text as the cause of the Korean wording (`presentation.md` has no plain-word rule).

## 1. Measurements

The log has no records between 11-08 and 11-11 23:00, so the 7-day window runs from 11-11 23:00 to 11-15 09:42.

| Window | Owner msgs | Replies with usage | Cost | Manager reads of team code or ledgers | `task add`/`new` | Rule writes |
|---|---|---|---|---|---|---|
| 24 h | 34 | 47 | $15.34 | 109 | 9 | 23 |
| 7 d | 105 | 137 | $38.63 | 359 | 33 | 42 |

- From 11-14 21:23 to 11-15 08:31 the manager did not rotate. Context grew from 70k to 281k tokens over 33 turns (`context_tokens` in `usage`).
- Over 7 days, 17 rotations, all at idle or on an instruction change. Each one keeps 4 exchanges (`manager.rs:123 HANDOVER_ENTRIES`).
- 12 task events in 7 days carry `worker says (unchecked)`.
- The manager wrote its own work files into `notes/`: `eval_review_checklist.md` (445), `eval_review_438.md` (447) and `final_commentary.md` (481, 483).
- Task #6 ("채움 evaluation: fill open items") ran all five teams on `model: sonnet` for 30 revisions, with 18 rotations and 24 limit waits (`6/task.json`, `6/log.jsonl`).

## 2. Owner corrections, classified

| Msg | Excerpt | Class | First wrong state (log ids) |
|---|---|---|---|
| 222 | "workers do not need to land anything" | setup | 214, 217: the manager landed #7 and #9, which were assessment ledgers. The Review paragraph says "Then land it" |
| 234 | "너의 의견에 맥락을 더" | shallow | 231: the manager gave an opinion with no evidence |
| 242 | "놓친것이 맞는지 한번만 더 검토" | wrong fact | 243: the Gitory per-task model claim was wrong. The manager had stated it in a turn without checking |
| 254 | "리드미를 물어본건 스펙축소…" | shallow | 251 answered a different question |
| 295 | "시연영상… 점수가 낮은 이유는?" | shallow | 297: the scores used only documents; the demo was not counted |
| 300, 304 | "사실이야 다시한번 확인", "진짜없다고?" | wrong fact | 303: the manager's own wording "수동뿐" from its code read was too mild |
| 308 | "코드가 더 우선순위를 가져" | wrong fact | 309: a team document's number was stated as fact without a check |
| 427 | "채움을 왜 점수를 높게" | forgotten decision | 429: ② got 8 with no use evidence. Rule 316 (mark it 확인불가) showed in the preamble only as its first line "문서랑 싱크 안맞는거…" |
| 438 | "4점이면 매뉴얼 배포인데 맞아?" | wrong fact | 424: the manager designed the deduction method itself, starting at the maximum and skipping the band |
| 442, 444 | "다시 전체적으로", "대충 하지말고 … 컨텍스트 다" | shallow, repeated ask | 443 deferred the ask ($0.05). In 445 the manager had to gather 15 rules from three stores: memory records, `notes/`, and Claude's `project-eval-*.md` |
| 454 | "단위테스트는 숫자만 부풀리고" | shallow | 455: ⑧ gave every team 9 because each one *had* the test types. #6 scored it and the manager relayed it |
| 456 | "너무 literally하면 안됨" | shallow | the same literal band reading as 454 |
| 470 | "내가 말한 그대로 복붙아니야?" | copy of owner words | 431: the manager drafted all five 총평 in its turn from a context full of the owner's notes (348, 350, 427) |
| 474 | "손으로 돌린다는게 말이된다생각해?" | shallow | 473: replied "Rewritten in ko-explain-voice" with 0 tool calls. The whole log has 1 `Skill` line, and the skill was available in `skills-plugin/` |
| 476 | "처음에 얘기했던 부분 다시 나오는거같네" | forgotten decision, repeated | 373: answered 372 (tech spec is the latest document) with a `task add` only, no `wsx memory add`. Six rotations followed before 476 (11-14 08:49 to 21:23) |
| 482 | "어떻게 개발했는지에 대한 내용이 더 많잖아" | shallow | 481: the manager wrote the commentary in its turn and applied the scoring rule "AI-driven development matters most" to it |

By class: shallow 9, wrong fact 4, forgotten decision 2, copy 1, setup 1. Two are repeated asks (444 repeats 442, 476 repeats 372).

## 3. Was the decision there when the turn ran?

I rebuilt the decisions preamble at each rotation from `records.jsonl`, with `notes.rs` `split`/`fit` and `manage.rs` `rule_line`. The budget is 7,079 bytes (10 KiB minus `context.md`'s 3.2 KiB).

| Rotation | Rules | Shown | Eval rules | Eval rules shown only as the owner's excerpt |
|---|---|---|---|---|
| 11-12 02:10 | 31 | 31 | 2 | 2 |
| 11-13 05:00 | 43 | 43 | 14 | 14 |
| 11-14 21:23 (start of turns 427 to 479) | 46 | 46 | 17 | 17 |
| 11-15 08:31 (start of 481 to 485) | 55 | 26 | 26 | 16 (the 29 R15 rules are hidden) |

- Every rule was in the preamble, but as the owner's words: "- 2026-11-12 yes", "- 2026-11-12 감점", "- 2026-11-12 문서랑 싱크 안맞는거 그 자체로 감점이있고". The record's `text` holds the real rule (for example the 확인불가 mark), but the view never shows it. `manage.rs:127` uses `words` whenever `words` is set.
- 372 was never a record, so no view could show it.
- The R15 eliot_voice rule (line 10) was shown at 21:23. 474 is not a hidden-rule case. The manager claimed a voice rewrite it did not load.

## 4. Causes and owning fixes

**Cause 1. The evaluation work happens in the manager's chat turns.** `instructions.rs:51`: "To talk, read the code yourself and answer the points in turn; no task … until the owner agrees". pr-review `decisions.md` line 8 (2026-11-05) says the same. For the evaluation, almost every owner message is "talk", so 359 code and ledger reads, the scoring method (424) and every commentary draft (431, 472, 481, 483) ran in turns of 150k to 281k context. A worker's result gets a manager review. The manager's own output gets none, and a draft written from the chat context repeats the owner's words (470).
- Fix (wsx): `instructions.rs`, "Talk or work" paragraph. Talk answers from the ledger and the task results. A question that needs a code survey, a score, or a text deliverable becomes a read-only `--hold` task, with the owner's rules in the prompt, and the manager reviews it like any other task. This replaces "read the code yourself".
- Coverage: none. Root-cause refactor step 1 (`root-cause-refactor.md` §2) kept the sentence. `orchestration-model.md` §3 leaves chat turns as they are.
- Fix (pr-review): remove `decisions.md` line 8. It was an R15 review rule, and it repeats the wsx text.

**Cause 2. A rule's meaning is not where the next turn reads it.** `rule_line` (`crates/wsx-core/src/memory/manage.rs:127-133`) prints `words` over `text`. 17 of the 28 evaluation rule records have `words`, so the preamble after each rotation lists excerpts without their rule. Printing `text` instead does not fit: the 27 active rules take 10,566 bytes as text, against the 7,079-byte budget (7,214 bytes as words).
- Fix (wsx): the view prints a rule's `title` (one line, the meaning), and `wsx memory add --kind rule` requires `--title`. `words` stays in the record as the owner's proof (`verified_by`). This replaces the `words`-or-`text` choice and keeps the budget.
- Coverage: none. `orchestration-model.md` §6 keeps the notes views unchanged. The 372 miss is model judgment. The instruction to record each owner decision already exists (`instructions.rs`, Memory paragraph), so it needs no new text. Cause 1's fix carries the rules in the task prompt.

**Cause 3. The scoring task is set up as a text task.** pr-review `decisions.md` line 6 (2026-11-05): "Text/doc tasks run --size small (Sonnet, build step only)". This rule was made for review prose, but #6 inherited it for judgment work: band placement for five teams, 30 revisions, no plan or review step. 12 of its results arrived "(unchecked)", and the manager relayed scores such as ⑧ 9 for every team (455).
- Fix (pr-review notes): scoring and re-checks are judgment tasks. Run them at full size, one task per team or per criterion, and keep the size-small rule for prose only.
- Coverage: not a wsx change.

**Smaller causes**
- 222: the Review paragraph assumes code to land. pr-review already has rule `assessment-tasks-not-landed`. No wsx change, since `--hold` exists.
- 441 (not an owner correction): the manager copied the 438 attachment into its own `$TMPDIR`. #6 got "Band text:" with nothing after it. Cause 1's fix passes attachments by their media path in the task prompt.

## 5. Questions for the owner

1. Should a question that needs a code survey, a score or a text deliverable go to a read-only task even while you are talking, so the manager answers from its results? **Recommend yes.** It covers 11 of the 17 corrections, and it moves the $15/day of in-turn reading into reviewed tasks.
2. Should every rule record need a one-line `--title`, with the decisions view printing it instead of your words? **Recommend yes.** The full rule text does not fit the 7 KiB start budget, and your words alone ("yes", "감점") carry no rule.
3. Should pr-review's 29 R15 decisions (2026-11-05 and 11-06, a finished review) move to the archive, together with lines 6 and 8? **Recommend yes.** Lines 6 and 8 cause 3 and part of cause 1. The other 27 are already hidden after 11-15 08:31 and only take budget.

## Method

Read-only Python over the files above. The scripts are in the task's `.work/t466/` (git-ignored).
- Counts: the `kind` of each `log.jsonl` record. `usage.cost_micro_usd` and `usage.model` on replies. An `agent` line is a tool call when it starts with `Bash`, `Read`, `Write` or `Skill` (`claude.rs:813 progress_text`).
- Preamble rebuild: the latest record per slug with `updated <= t`, status `active`, rendered as `rule_line`, newest first, `take_while` within `10240 - min(4096, size(context.md))`.
