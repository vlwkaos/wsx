# Memory evaluation (step 6)

Status: baseline half, offline retrieval and the paid memory half measured on 2026-11-11 ("Baseline", "Offline retrieval", "Paid replay"); verdicts: **plan arm harms** (by the fixed rule, within noise), cheap-recall arm kept by the rule but unproven, manager rotation not worth it. The manager replay stopped at the cap ("Manager rotation"). Alternative design: "Alternative design". Question: does memory make a wsx task better and cheaper? Owner decisions: the vault stays in dotfiles `memory.py`; plain Claude chats get no start-up memory load unless this shows a measured gain; step 8 waits for the verdict.

## Method

`python3 -B scripts/memory-eval.py <pick|run|judge|report|live|offline>`. Every number below comes from it. Runs and spend: `.work/memory-eval/` (`runs/<arm>/<task>-<rep>.json`, `judged.json`, `spend.jsonl`).

| Part | Rule |
|---|---|
| Tasks | 20 landed tasks of the wsx store (`pick`, seed 385): plan step is step 1, a claude worker, a landed diff of crates or scripts, a memo or summary, plan cost at most $1.50, no audit, nothing that touches `memory`. Quotas by the crate most files changed in: web 4, daemon 3, terminal 2, core 4, cli_tui 3, scripts and docs 2, rest by pool; at least 6 with a send-back after the plan. The daemon and terminal pools are small (1 and 0), so other areas fill them |
| One run | the plan step only: a git worktree at the task's base, the task's own model and effort, the task's original prompt, `Task context from the host` rebuilt for its start time, the step text of the task, the worker instructions (today's text, the same for every task and arm), the owner's skills plugin, the result schema. Stopped at the first result |
| Isolation | own `XDG_STATE_HOME` and `XDG_CONFIG_HOME` per run; the real task store, memory and config are only read |
| Baseline arm | what a worker had at the task's start: the host context and `wsx task search` through a shim, over the task store cut at the task's start (every log line after it removed; a result only of tasks landed before; notes lines dated before it). No `wsx memory`. The host context is not logged: it is rebuilt from the task's files, the tasks open at its start that share those files, and the manager's `context.md` lines that name them. `context.md` is rewritten by the manager and has no history, so lines that name a task from this one on are dropped (a proxy) |
| Memory arm | the baseline, plus a start set and a brief, and `wsx memory recall\|search\|status` through the shim on a store cut at the start. Store: step 5's capture rule (a landed task's memo becomes a fact with the paths of its landed diff and its commit) applied to every task landed before the start; rules: the decisions dated before it. The paid arm uses the code of steps 4 and 5 of the `wsx` given with `--wsx`; the script refuses to start without them. The dry run runs the same commands, with a fake agent |
| Cheap-recall arm | 10 of the 20 tasks (every second pick): as the memory arm, but `recall` takes the best 20 search hits and a cheap model (haiku) picks up to 5. Its calls are added to the run's cost. This arm exists in the shim, because `recall` has no model in wsx yet |
| Leaks | `leak_check` stops a memory run if any log line, result, record or dated note is newer than the task's start. Store size per task is in the report |
| Judge | Sonnet 5.5, blind: it sees no arm, cost or run name. It first lists the traps of a task once (messages after the plan that corrected a mistake of the approach; new scope is no trap), then scores each plan: cause 0, 1 or 2 against the landed summary and memo, and whether it repeats each trap |
| Files | recall = landed files the plan names / landed files; precision = file names in the plan that are landed files / file names in the plan (read-only files lower it equally in every arm) |
| Cost | per run from the CLI result (API-equivalent dollars, the task's model); calls = model calls of the main conversation; explore = Read, Grep, Glob and Bash steps that run rg, grep, sed, cat, head, tail, ls, find |
| Noise | 3 tasks (core, web, cli_tui) run twice in the baseline arm |
| Spend | a hard cap of $40 for all arms and the judge (`spend.jsonl`); a run that would pass it is skipped. The cheap-recall arm runs last and gets what is left |

Dry run (before any paid run): two tasks on `scripts/fake-claude-stream.py`, all arms but the cheap one, a stub judge: the cut store held no line newer than the start, `wsx task search` and `wsx memory search` answered through the shim from the cut state, the memory writes were refused, and no worktree was left.

## Baseline (measured, 20 tasks, 3 of them twice)

Spend: runs $12.86 + judge $0.59 = $13.45 of the $40 cap (estimated before: about $15 + $2.5). Areas picked: core 6, cli_tui 6, web 4, scripts and docs 3, daemon 1, terminal 0: the pools for daemon (1) and terminal (0) were too small for the quota. 9 of the 20 tasks have a trap (a correction after the plan).

| task | area | $ replay | $ logged | calls | logged | explore | recall | precision | cause | trap hits |
|---|---|---|---|---|---|---|---|---|---|---|
| #229 | core | 0.62 | 0.51 | 15 | 10 | 15 | 0.75 | 0.88 | 2 | 0/0 |
| #231 | cli_tui | 0.74 | 0.51 | 27 | 20 | 23 | 0.89 | 0.80 | 2 | 0/3 |
| #240 | web | 0.95 | 1.45 | 11 | 14 | 10 | 0.54 | 1.00 | 2 | 0/1 |
| #243 | cli_tui | 0.80 | 1.49 | 7 | 23 | 6 | 0.50 | 0.67 | 2 | 0/1 |
| #252 | daemon | 0.68 | 1.09 | 6 | 8 | 5 | 0.87 | 0.92 | 2 | 0/0 |
| #265 | web | 0.53 | 0.48 | 8 | 9 | 13 | 0.81 | 0.95 | 2 | 0/0 |
| #277 | web | 0.39 | 0.32 | 7 | 7 | 9 | 0.80 | 0.69 | 2 | 0/2 |
| #283 | scripts_docs | 0.26 | 0.28 | 4 | 7 | 4 | 0.50 | 0.80 | 2 | 1/3 |
| #285 | cli_tui | 0.41 | 0.49 | 10 | 12 | 15 | 1.00 | 0.64 | 2 | 0/0 |
| #289 | core | 0.46 | 0.50 | 7 | 11 | 9 | 0.64 | 0.91 | 2 | 0/2 |
| #296 | scripts_docs | 0.81 | 0.94 | 21 | 14 | 21 | 1.00 | 0.03 | 2 | 0/0 |
| #305 | core | 0.52 | 0.54 | 15 | 15 | 19 | 1.00 | 0.58 | 2 | 0/0 |
| #316 | core | 0.40 | 0.57 | 9 | 20 | 9 | 1.00 | 0.70 | 2 | 0/0 |
| #328 | cli_tui | 0.62 | 0.60 | 24 | 22 | 22 | 1.00 | 0.50 | 1 | 1/1 |
| #334 | web | 0.51 | 0.73 | 10 | 13 | 12 | 0.80 | 1.00 | 2 | 0/0 |
| #345 | core | 0.28 | 0.43 | 8 | 14 | 7 | 0.70 | 0.50 | 2 | 0/0 |
| #350 | cli_tui | 0.67 | 0.76 | 14 | 14 | 21 | 0.54 | 0.88 | 1 | 4/4 |
| #357 | core | 0.40 | 0.30 | 10 | 7 | 14 | 0.41 | 0.65 | 2 | 0/2 |
| #363 | scripts_docs | 0.31 | 0.17 | 7 | 4 | 6 | 1.00 | 0.20 | 2 | 0/0 |
| #364 | cli_tui | 0.45 | 0.56 | 7 | 10 | 10 | 0.91 | 0.85 | 2 | 0/0 |

First run of each task. `$ logged` and `logged` calls are the original plan step in the task's log. Cause: 0 to 2. Trap hits: traps the plan repeats / traps the owner corrected later.

| Measure | Replay median | Logged median (same 20 tasks) |
|---|---|---|
| plan step cost | $0.52 | $0.53 |
| model calls | 9.5 | 12.5 |
| explore steps | 11 (Bash calls count one each; a message may hold several) | not logged |
| file recall / precision | 0.80 / 0.75 | |
| cause (mean) | 1.90 (21 of 23 plans score 2) | |
| trap hits | 7 of 23 trap checks, 4 of them in #350 | |

**Fidelity.** Replay median cost over logged median is 0.98, inside the limit of 1 +- 0.23 (twice the cost spread below). Calls are lower (9.5 against 12.5) and wall time is not compared. The replay differs from the original in the rebuilt host context, today's worker instructions and today's skills, so read an arm difference only as the difference between arms of the same replay.

**Noise** (the same task twice, baseline arm; #229, #231, #240):

| Measure | Spread between the two runs |
|---|---|
| cost, \|a - b\| / mean | 0.12 |
| calls, same | 0.11 |
| recall, absolute | 0.08 |
| cause, absolute | 0.00 |

Three pairs are few: treat the spread as a floor. Per task, the difference of two runs has about 15% noise; the median of 20 paired differences about 4%.

**Limits of the measures.** The cause score has a ceiling: 21 of 23 baseline plans score 2, so it can show a loss but hardly a gain. File recall and the trap hits are the two measures that can rise. Traps are few (23 checks on 9 tasks).

## Live comparison (support only, confounded)

`python3 -B scripts/memory-eval.py live --cutoff-task 380`: landed tasks with a plan step, from the logs. #380 has not landed, so only the "before" group exists.

| group | landed tasks | median $ | mean $ | plan share of $ | median send-backs | mean failed checks | median plan calls |
|---|---|---|---|---|---|---|---|
| before #380 lands | 133 | 2.92 | 4.07 | 33% | 1 | 0.06 | 10 |
| after | 0 | | | | | | |

The groups will be different tasks, so a difference is not an effect of memory. Run it again after #380 lands, and read it only beside the replay.

## Verdict rule (fixed on 2026-11-11, before any memory arm runs)

Compare per task, memory arm against the first baseline run of the same task, on the 20 tasks. Cost includes every model call the memory machinery makes in the run (the cheap `recall` model, and any model call of the capture rule per landed task).

| Verdict | Condition |
|---|---|
| **Helps** | A: median paired plan cost falls by at least **X = 15%** (spread 12%, median noise 4%), and no loss: recall median not lower than the baseline's by more than **0.08**, cause mean not lower by more than **0.15**, trap hits not higher by more than 2. Or B: file recall median rises by at least **Y = 0.10**, or trap hits fall by at least **3**, with cost within **Z = +10%** |
| **Harms** | cost rises by more than 10% without B, or recall or cause fall by more than the limits in A |
| **No gain** | neither. Then no tuning to pass: use the per-task data (below) to write the alternative design |

The cheap-recall arm (10 tasks) is judged against the memory arm on the same tasks: it is kept only if its cost with the haiku calls is lower, and quality is within the limits of A.

**Task level.** A plan step is only part of a task. In this store the plan step is 33% of the spend of tasks that have one (the design said 29%); the median landed task costs $2.92. So X = 15% of the plan is about 5% of a task, about $0.15 per landed task at the median ($0.20 at the mean, $4.07), and a plan gain smaller than that is not worth the store. The report states the saving as plan dollars, as percent of task spend (plan share 33%) and as dollars per landed task. Memory may also change build and review cost (fewer wrong turns, fewer send-backs); that is not measured here, and the live comparison is the only support for it.

## Cost of the memory half (estimate)

Baseline: 23 runs $12.86. Memory arm: 20 runs at about +10% for the start set, $12 to $13. Cheap-recall arm: 10 runs plus haiku, $6 to $7. Judge about $1 for 30 runs. Sum with the baseline: about $33 to $35, under the cap of $40 (the script stops at the cap and the cheap arm gets what is left).

## Offline retrieval (free, all landed tasks with a plan step)

Question: does the start set hold the files a plan step reads? Target set by the owner: at least 60% of them. `python3 -B scripts/memory-eval.py offline [--wsx BIN] [--final] [--show N...]` (no model call; about 15 minutes on 6 jobs, `--rows` scores the saved `.work/memory-eval/offline/rows.json` again). It runs on a copy of the task store (`MEMORY_EVAL_STORE`): 137 landed tasks (#133 on) with a plan result. Per task: a git worktree at its base, the task store cut at its start, the capture rule of #386 (`wsx memory capture`) on every task landed before it, and the real `wsx memory start --files <its --files>`. Split by start time: the first 92 tune, the last 45 are held out; the script prints the held-out third only with `--final` (shown once, after the rules below were fixed).

**Labels.** (a) files the plan step read: `Read` paths and every repo file that a `rg grep sed cat head tail awk wc` Bash line names, in the agent lines before the plan result, kept only if the file exists at the task's base; directories count apart. (b) files of the landed diff. (c) trap: an earlier landed task's memo names a file of this task (not one most tasks change) and a send-back (`add` after the plan, not a land notice) shares at least 2 rare words (used by at most 6 memos) with it; rare (3 pairs tuned, 4 held out; a hand check of the 3 tuned: 2 real, 1 weak), read it as a sign, not a rate. **The log loses reads, so (a) is a lower bound**: 2499 of 3846 Bash lines (65%) end in a clip mark and their last word is dropped, the 35 `Agent` calls (Explore) carry no arguments, `Grep` and `Glob` are not logged.

**Measures.** Recall and precision of the start set's record paths against (a) and (b). A caveat on them: a record's line in the block is `- <slug> (<kind>, <date>): <memo>` and shows no path list; the paths count as the set's paths, but the worker only reads the files the memo's words name. That score is the column "from the words of the block": it is 0.01 to 0.23 in every row below. "Records useful" is the share of shown records with a path that the plan read or the diff changed; a record that is not is a wrong fact in the budget.

### The start set today

| | tuned (92) | held out (45) |
|---|---|---|
| recall (a) / precision (a) of `wsx memory start` | 0.07 / 0.09 | 0.06 / 0.06 |
| tasks whose block holds no record | 55 | 31 |
| facts with a commit and paths that are stale at the base of the task that reads them | 0.94 (9105) | 0.93 (8899) |
| files read, by why they are missing: stale record / declared `--files` do not reach it / no record / never in an earlier diff / cut by the budget | 0.52 / 0.23 / 0.11 / 0.07 / 0.03 | 0.40 / 0.43 / 0.07 / 0.02 / 0.05 |

**The start set misses the 60% target by a long way: 0.06 held out.** The cause is the stale rule: a fact is stale when any of its paths changed since its commit, a record holds the paths of the whole landed diff (median 8 files), and `CHANGELOG.md`, `docs/tasks.md` or `scripts/test-task.py` are in nearly every one. Hidden stale facts are only named by slug.

### Stale rule ablation

Rule fixed before the run (2026-11-11): a variant reaches the target if the tuned mean recall (a) is at least 0.60; among those the winner has the most useful records (a tie within 0.03 goes to fewer stale records shown); the held-out third confirms once and is never used to choose. Variants, same 4 KiB budget and order of records: `hidden` today (a port of the scan in the script; it shows the same records as `wsx memory start` in 137 of 137 tasks); `marked` stale facts after the fresh ones with `[stale: files changed since <commit>, N commits]`; `symbols` stale only if the diff since the commit touches a symbol the fact's text names (no symbol: today's rule); `churn K` stale only if the fact's paths changed by at least K lines since the commit; `none` no stale rule.

| variant | recall (a) tuned / held | precision (a) | records useful | records shown | stale shown | recall (a) from the words of the block |
|---|---|---|---|---|---|---|
| `wsx memory start` = hidden | 0.07 / 0.06 | 0.09 / 0.06 | 0.22 / 0.13 | 1.2 / 1.4 | 0 | 0.01 / 0.03 |
| marked | 0.52 / 0.41 | 0.21 / 0.18 | 0.83 / 0.74 | 9.4 / 9.0 | 0.88 / 0.84 | 0.12 / 0.18 |
| symbols | 0.39 / 0.25 | 0.17 / 0.21 | 0.73 / 0.59 | 5.1 / 4.2 | 0 | 0.04 / 0.06 |
| churn 100 | 0.20 / 0.18 | 0.16 / 0.21 | 0.49 / 0.49 | 2.6 / 3.1 | 0 | 0.03 / 0.05 |
| churn 400 | 0.36 / 0.29 | 0.20 / 0.17 | 0.68 / 0.61 | 5.1 / 5.2 | 0 | 0.07 / 0.08 |
| churn 1600 | 0.52 / 0.44 | 0.19 / 0.16 | 0.79 / 0.74 | 9.3 / 8.6 | 0 | 0.12 / 0.15 |
| none | 0.53 / 0.44 | 0.19 / 0.17 | 0.87 / 0.81 | 10.9 / 10.6 | 0 | 0.14 / 0.21 |
| title BM25, top 5 facts (no files) | 0.50 / 0.46 | 0.18 / 0.18 | 0.71 / 0.71 | 5.0 / 5.0 | 0 | 0.06 / 0.09 |
| `wsx memory recall "<title>"` | 0.05 / 0.06 | 0.05 / 0.10 | 0.52 / 0.59 | 0.2 / 0.4 | 0 | 0.01 / 0.01 |
| exploratory: none + title BM25 | 0.62 / 0.63 | 0.15 / 0.16 | 0.81 / 0.77 | 14.9 / 15.0 | 0 | 0.16 / 0.23 |
| exploratory: marked + title BM25 | 0.62 / 0.59 | 0.16 / 0.16 | 0.78 / 0.72 | 13.5 / 13.5 | 0.61 / 0.56 | 0.14 / 0.20 |

Paired per task, recall (a) minus hidden, mean and seeded bootstrap 95% interval (tuned / held out): marked +0.450 [+0.389, +0.516] / +0.353 [+0.267, +0.446]; symbols +0.324 [+0.268, +0.384] / +0.194 [+0.120, +0.272]; churn 1600 +0.453 [+0.391, +0.516] / +0.382 [+0.299, +0.468]; none +0.465 [+0.402, +0.531] / +0.388 [+0.295, +0.480]. Marked, churn 1600 and none are within each other's interval.

Per area, recall (a) tuned (tasks): `wsx memory start` / marked / symbols / none: cli_tui (28) 0.05 / 0.63 / 0.47 / 0.64; core (13) 0.09 / 0.56 / 0.37 / 0.59; web (29) 0.09 / 0.55 / 0.49 / 0.56; daemon (4) 0.00 / 0.21 / 0.17 / 0.25; scripts_docs (18) 0.06 / 0.32 / 0.17 / 0.33. Held out: cli_tui (18) 0.06 / 0.45 / 0.21 / 0.50; core (9) 0.05 / 0.39 / 0.12 / 0.40; web (6) 0.00 / 0.45 / 0.44 / 0.55; scripts_docs (12) 0.08 / 0.34 / 0.31 / 0.34. No terminal task has a plan step.

**Verdict: no stale rule reaches 60%.** The best, `none` and `marked`, give 0.53 and 0.52 tuned and 0.44 and 0.41 held out. The pre-set rule names no winner. From the data:

1. **Hiding is the loss.** Any rule that stops dropping a fact gains +0.45 recall (tuned) on a start of 0.07, and no task gets worse. This is 94% of the facts, so it is the rule that decides the block, not the ranking or the budget (cut 0.03 to 0.05 of the files read).
2. **The finer rules do not pay.** `symbols` loses 0.13 recall against `marked` and has no better precision (0.73 against 0.83 useful); `churn` gives the recall of `marked` only at K = 1600 lines, where it hides almost nothing, and loses 0.16 (K = 400) to 0.32 (K = 100). A fact that names a symbol that the diff did not touch is not the ones the worker needs, and a line count is mostly noise from `CHANGELOG.md` and the docs.
3. **The rule to start from is `marked`.** Keep the predicate (any path changed since the commit), but a stale fact is not dropped: it comes after all fresh ones, with `[stale: files changed since <7 characters of the commit>, <N> commits that touch its paths]`, within the same 4 KiB. It has the same recall as `none` (0.52 against 0.53) and the same useful share within 0.04 (0.83 against 0.87), and the worker learns that the fact may be out of date. Cost: 88% of the records shown are stale, so the first prompt carries many old lines; whether that helps a worker is for the paid replay.
4. **The stale rule alone does not reach 60%.** After it, 0.23 to 0.43 of the files read are not reached by the declared `--files` of the task (the plan reads code that the task does not name), 0.07 to 0.11 have no record, 0.02 to 0.07 were never in an earlier diff. The title alone (BM25 over the facts, no file) finds 0.50 tuned and 0.46 held out; joined with `none` or `marked` it gives 0.62 tuned and 0.63 or 0.59 held out. That join is exploratory (chosen after seeing the tuned run, the byte budget is not applied: 15 records are about 4.5 KiB (est.), it needs about 5 KiB of the budget or shorter lines) and it fills the block with more records that do not match a file (precision 0.15 to 0.16).
5. **The paths must reach the worker.** Every number above counts a record's paths, but the line shows the memo's words only; by those words `none` finds 0.21 held out. A start line that lists the record's paths (a capped `files:` part) is the change this table assumes, and it costs about 40 bytes a path (est.); it was not measured, so the budget effect of it is open.
6. `wsx memory recall` finds 0.05: its top 5 are transcript hits (a task's story), which have no paths.

**The exact rule to change in `wsx_core::memory::start`** (the owner starts it): in `records`, a stale fact is no longer pushed to the `stale` list and dropped; it is kept in a second list, and after the fresh ones it is shown with the mark above while `room` lasts (the check of `MAX_CHECKS` stays). `not_shown` then names only what the budget cut. The predicate `changed` does not change.

**Hand check (3 tuned tasks, labels against the raw `log.jsonl` lines).** `offline --rows --show 135 141 170` prints the same.

| task | files read by the script | raw lines (plan result at line 11, 18, 32) |
|---|---|---|
| #135 cli_tui | `host.rs`, `slot.rs`, `test-task.py` | line 5 `sed -n 80,210p crates/wsx/src/task/host.rs`; line 6 `cat crates/wsx/src/task/slot.rs`, `sed -n 2270,2400p scripts/test-task.py`. The `grep ... -r crates/wsx/src scripts` of line 4 names directories only; the glob `crates/wsx/src/task/*.rs` of line 7 is not expanded: both lost |
| #141 web | `http.rs`, `lib.js`, `viewer.js`, `web-tasks-check.mjs` | line 5 `sed -n 1175,1230p crates/wsx-web/src/http.rs` and `cat crates/wsx-web/web/viewer.js`; line 7 `sed -n 1,40p crates/wsx-web/web/lib.js`; lines 6 and 15 `scripts/web-tasks-check.mjs`. Line 5 also has `sed -n 85,110p crates/wsx-web/web/che…` (clipped): `checks.js` is a landed file the label misses |
| #170 scripts_docs | `config/global.rs`, `config/project.rs`, `plugins.rs`, `http.rs`, `docs/plugins.md` | line 5 `cat docs/plugins.md`; line 19 `crates/wsx-daemon/src/plugins.rs`; line 20 `crates/wsx-web/src/http.rs`; line 22 `sed -n 280,300p crates/wsx-core/src/config/global.rs` and `head -15 .../project.rs`. `docs/web-client…` of line 5 is clipped: lost. Knowledge files outside the repo are not labels |

An earlier run on a stub of the start set (before #386) found the same cause: stale hid 76% to 83% of the files read, and ignoring it gave 0.74 tuned and 0.65 held out; the real capture keeps a 240-character memo and its own list of paths, so the real numbers are lower.

### After the change of the start set (#391, 2026-11-11)

`wsx_core::memory::start` now shows a stale fact in its place with `[stale: files changed since <7 characters>, N commits touch its paths]` (order of variant `none`), adds up to 5 facts that BM25 of the task's title and first prompt line finds (records only, after the path matches, same 4 KiB), and shows up to 3 paths on each record line (`(files: a, b, c)`). `memory-eval.py offline --final` calls the real `wsx memory start --files ... --about "<title> <first prompt line>"` (budget applied) on the same 138 plan steps; the `start` row below is that command's output.

**Plainly: the title join was chosen after the tuned run of #388, and the held-out third has now been seen twice (#388 and this run). The held-out numbers confirm; they were not used to choose anything in this change.**

| `wsx memory start` | tuned (92) | held out (46) |
|---|---|---|
| recall (a) | 0.53 | 0.42 |
| precision (a) | 0.25 | 0.23 |
| records useful | 0.83 | 0.76 |
| stale share of the records shown | 0.92 | 0.94 |
| records shown | 8.2 | 8.0 |
| bytes of a block, mean / median / max | 3708 / 3980 / 4089 | 3605 / 3940 / 4078 |
| of it, `(files: ...)` parts / stale marks (mean) | 478 / 491 | 473 / 489 |
| recall (a) from the words of the block | 0.29 | 0.24 |
| before (#388): recall (a), useful | 0.07, 0.22 | 0.06, 0.13 |

Per area, recall (a), against the 0.60 target (tasks): tuned cli_tui 0.62 (28), core 0.56 (13), web 0.56 (29), scripts_docs 0.40 (18), daemon 0.18 (4); held out cli_tui 0.48 (18), core 0.29 (9), web 0.40 (6), scripts_docs 0.42 (13). Only tuned cli_tui reaches 0.60. (A review step changed the `files:` part to the sorted paths, one pass a merge, and the run was repeated: these are that run's numbers.)

Files read by the plan, by why they are not in the block (tuned / held out): new (never in an earlier diff) 0.07 / 0.02, no record 0.11 / 0.07, declared `--files` and the title do not reach it 0.19 / 0.36, cut by the budget 0.18 / 0.23.

**Verdict: the target is missed, 0.53 tuned and 0.42 held out against 0.60.** From the data:

1. Hiding was the loss and it is fixed: +0.46 tuned and +0.37 held out over the old 0.07 and 0.05 (hidden, same run).
2. The change adds nothing over `marked` of the ablation (0.52 / 0.42, same budget): the block carries fewer records (8.2 and 8.0 against 9.4 and 8.8) because the `(files: ...)` parts and the stale marks take 0.12 of the 4096 bytes each (0.24 together). The words of the task add facts, but after the path matches, so they are the first the budget cuts.
3. The part that misses is the budget and the reach. The exploratory join without a budget scored 0.62 / 0.63 (`none + bm25 title`, 15 records); the real block holds 8, and the files read that the budget cut rose from 0.03 and 0.05 (the old block) to 0.18 and 0.23. The second part is reach: on the held-out third 0.36 of the files read are in no record that the declared `--files` or the words of the title find (the plan reads code the task does not name).
4. The `(files: ...)` part was meant to carry the paths the score assumes. By the words of the block the recall is 0.29 / 0.24 (before: 0.01 / 0.03), so the paths reach the worker, at the price in point 2.

Stopped here, as set: nothing is tuned on the held-out third. What the data point to, for the owner to decide and not tried: a shorter line (a clip under 700 characters, the mark as `[stale <7>, N]`, fewer paths) buys back up to 0.24 of the budget; the joint use with `wsx memory recall` mid-task is what the paid replay measures. The held-out third is spent for any new choice; a new rule needs a new split.

## Paid replay (2026-11-11, `wsx/t380` at fbc31c72)

Spend: $39.07 of the $40 cap (runs: memory $11.95, cheap-recall $5.91, manager replay $6.85; judges $0.91; the baseline $13.45 booked from the table). Estimates were $12 to $13 and $6 to $7 for the first two; the manager replay cost more than planned (see its section) and stopped the work at the cap.

**What differs from the plan, and why** (none changes a rule):

- **The baseline run files were lost.** `.work/` is git-ignored and #385's working copy was deleted. `memory-eval.py import-baseline` writes the baseline table of this file as run records (cost, calls, explore, recall, precision, cause, trap hits) and books $13.45. The paired comparison needs only those. The cheap and memory arms were judged with the trap lists listed again by the judge; the baseline's trap hits come from the old judge call, so a trap difference carries judge variance that cannot be separated (the listed trap counts agree on 8 of the 9 tasks that have traps; #231 has 3 against 2).
- `pick --tasks` rebuilt `picks.json` from the 20 tasks of the table (the store has grown since the seed 385 drew them). Areas, logged $ and calls agree with the table.
- **The start set rule changed after the offline runs.** #391 (stale facts shown with a mark, title-matched facts, files part) was tuned and checked on the offline tuned and held-out thirds; the paid replay did not see those runs. `memory_cut` now passes `--about "<title> <first prompt line>"` as `host.rs` does. The block is the real `wsx memory start` of `wsx/t380`, 3854 bytes on average (4096 budget).
- The brief added to the worker instructions is the one in the script (`MEMORY_BRIEF`), not the text of the product.

### Plan arm: memory against baseline (20 tasks)

| task | $ base | $ memory | change $ | calls b/m | recall b/m | cause b/m | trap hits b/m | recall calls |
|---|---|---|---|---|---|---|---|---|
| #229 | 0.62 | 0.54 | -0.08 | 15/15 | 0.75/0.88 | 2/2 | 0/0 / 0/0 | 1 |
| #231 | 0.74 | 0.62 | -0.12 | 27/18 | 0.89/0.89 | 2/2 | 0/3 / 0/2 | 0 |
| #240 | 0.95 | 1.08 | +0.13 | 11/13 | 0.54/0.50 | 2/2 | 0/1 / 0/1 | 0 |
| #243 | 0.80 | 1.04 | +0.24 | 7/12 | 0.50/0.75 | 2/2 | 0/1 / 1/1 | 4 |
| #252 | 0.68 | 0.76 | +0.08 | 6/7 | 0.87/0.67 | 2/2 | 0/0 / 0/0 | 0 |
| #265 | 0.53 | 0.63 | +0.10 | 8/17 | 0.81/0.77 | 2/2 | 0/0 / 0/0 | 1 |
| #277 | 0.39 | 0.34 | -0.05 | 7/7 | 0.80/0.80 | 2/2 | 0/2 / 2/2 | 0 |
| #283 | 0.26 | 0.29 | +0.03 | 4/8 | 0.50/0.33 | 2/2 | 1/3 / 3/3 | 0 |
| #285 | 0.41 | 0.51 | +0.10 | 10/13 | 1.00/0.86 | 2/2 | 0/0 / 0/0 | 1 |
| #289 | 0.46 | 0.50 | +0.04 | 7/11 | 0.64/0.64 | 2/2 | 0/2 / 1/2 | 0 |
| #296 | 0.81 | 1.12 | +0.31 | 21/29 | 1.00/1.00 | 2/1 | 0/0 / 0/0 | 0 |
| #305 | 0.52 | 0.45 | -0.07 | 15/13 | 1.00/1.00 | 2/2 | 0/0 / 0/0 | 0 |
| #316 | 0.40 | 0.29 | -0.11 | 9/6 | 1.00/1.00 | 2/2 | 0/0 / 0/0 | 0 |
| #328 | 0.62 | 0.68 | +0.06 | 24/22 | 1.00/0.67 | 1/1 | 1/1 / 0/1 | 1 |
| #334 | 0.51 | 0.70 | +0.19 | 10/14 | 0.80/0.52 | 2/2 | 0/0 / 0/0 | 0 |
| #345 | 0.28 | 0.37 | +0.09 | 8/8 | 0.70/0.00 | 2/2 | 0/0 / 0/0 | 2 |
| #350 | 0.67 | 0.68 | +0.01 | 14/11 | 0.54/0.27 | 1/1 | 4/4 / 4/4 | 0 |
| #357 | 0.40 | 0.57 | +0.17 | 10/13 | 0.41/0.52 | 2/2 | 0/2 / 1/2 | 1 |
| #363 | 0.31 | 0.29 | -0.02 | 7/8 | 1.00/1.00 | 2/2 | 0/0 / 0/0 | 0 |
| #364 | 0.45 | 0.47 | +0.02 | 7/10 | 0.91/0.82 | 2/2 | 0/0 / 0/0 | 2 |

Trap hits are "traps repeated / traps the judge listed". The baseline columns are from the table above.

| measure | baseline | memory | change | limit of the rule |
|---|---|---|---|---|
| plan cost, median of the paired change | $0.52 | $0.56 | **+11.2%** | A: at most -15%; B and harms: +10% |
| plan cost, sum of 20 | $10.81 | $11.95 | +$0.057 per task (mean) | |
| file recall, median | 0.81 | 0.76 | -0.05 | A: not below -0.08; B: +0.10 |
| cause, mean | 1.90 | 1.85 | -0.05 | A: not below -0.15 |
| trap hits, total | 6 | 12 | +6 | A: at most +2; B: -3 |
| model calls, median | 9.5 | 12.5 | +3 (12 of 20 tasks more) | not in the rule |
| explore steps, median | 11 | 13.5 | | not in the rule |

**Verdict (rule applied as written): HARMS.** Cost rises by 11.2%, above the 10% limit, without B (recall falls, trap hits rise). A is false twice over (cost, +6 trap hits). **Read it with its noise:** a bootstrap of the 20 paired changes gives a median +11.2% with a 95% interval of -4.8% to +21.4%; 11 tasks cost more than 10% more, 5 cost more than 10% less. The cost alone is inside the noise of the replay; the trap total is the judge-variance case above. The honest reading is *no gain, with a lean to a loss*: no measure moves toward a gain.

At the task level (plan share 33%, median landed task $2.92): **+3.7% of a task, +$0.057 per landed task (mean paired change), +$0.11 at the median landed task**. The threshold for a gain was -$0.15 per landed task.

### Cheap-recall arm (10 tasks) against the memory arm

| task | $ memory | $ cheap (with haiku) | calls m/c | recall m/c | cause m/c | trap hits m/c | recall calls m/c |
|---|---|---|---|---|---|---|---|
| #229 | 0.54 | 0.69 | 15/13 | 0.88/1.00 | 2/2 | 0/0 / 0/0 | 1/0 |
| #240 | 1.08 | 0.88 | 13/9 | 0.50/0.50 | 2/2 | 0/1 / 0/1 | 0/0 |
| #252 | 0.76 | 0.56 | 7/5 | 0.67/0.87 | 2/2 | 0/0 / 0/0 | 0/0 |
| #277 | 0.34 | 0.45 | 7/10 | 0.80/0.70 | 2/2 | 2/2 / 0/2 | 0/0 |
| #285 | 0.51 | 0.34 | 13/9 | 0.86/1.00 | 2/2 | 0/0 / 0/0 | 1/0 |
| #296 | 1.12 | 1.13 | 29/23 | 1.00/1.00 | 1/1 | 0/0 / 0/0 | 0/0 |
| #316 | 0.29 | 0.31 | 6/7 | 1.00/1.00 | 2/2 | 0/0 / 0/0 | 0/0 |
| #334 | 0.70 | 0.50 | 14/10 | 0.52/0.92 | 2/2 | 0/0 / 0/0 | 0/1 |
| #350 | 0.68 | 0.72 | 11/14 | 0.27/0.69 | 1/2 | 4/4 / 2/4 | 0/0 |
| #363 | 0.29 | 0.32 | 8/10 | 1.00/1.00 | 2/2 | 0/0 / 0/0 | 0/0 |


| measure | memory | cheap | change |
|---|---|---|---|
| plan cost with the haiku calls, sum | $6.32 | $5.91 | -$0.041 per task |
| plan cost, median of the paired change | $0.61 | $0.53 | +3.1% |
| recall median / cause mean / trap hits | 0.83 / 1.80 / 6 | 0.96 / 1.90 / 2 | +0.13 / +0.10 / -4 |

**Verdict by the rule (kept if cost with the haiku calls is lower and quality is within A): kept**, by the sum of cost (-6.5%) and quality inside the limits. This is not evidence for the model that picks records: **the cheap `recall` ran once in 10 runs** (haiku cost $0.00), the memory arm's `recall` 8 times in 20. The two arms are the same start set; the difference is the noise of a rerun (cost spread 12%, per-run recall spread 0.08). Treat the cheap arm as not shown to matter.

## Alternative design (from the per-task data)

Rebuilt from the start block each memory run got (`memory-eval.py alt`, no model call). A record is right if its paths hold a landed file.

| task | $ base | $ memory | calls b/m | block B | records shown | right | wrong | landed files in right records | named by the plan | ignored | landed files in no shown record | recall b/m | recall calls |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| #229 | 0.62 | 0.54 | 15/15 | 4007 | 9 | 8 | 1 | 6/8 | 6 | 0 | 2 | 0.75/0.88 | 1 |
| #231 | 0.74 | 0.62 | 27/18 | 3857 | 8 | 8 | 0 | 8/9 | 8 | 0 | 1 | 0.89/0.89 | 0 |
| #240 | 0.95 | 1.08 | 11/13 | 3901 | 8 | 8 | 0 | 13/24 | 6 | 7 | 11 | 0.54/0.50 | 0 |
| #243 | 0.80 | 1.04 | 7/12 | 3818 | 8 | 8 | 0 | 6/8 | 5 | 1 | 2 | 0.50/0.75 | 4 |
| #252 | 0.68 | 0.76 | 6/7 | 3929 | 9 | 4 | 5 | 2/15 | 0 | 2 | 13 | 0.87/0.67 | 0 |
| #265 | 0.53 | 0.63 | 8/17 | 3956 | 8 | 8 | 0 | 11/26 | 8 | 3 | 15 | 0.81/0.77 | 1 |
| #277 | 0.39 | 0.34 | 7/7 | 3911 | 9 | 3 | 6 | 5/10 | 4 | 1 | 5 | 0.80/0.80 | 0 |
| #283 | 0.26 | 0.29 | 4/8 | 4046 | 9 | 9 | 0 | 5/6 | 2 | 3 | 1 | 0.50/0.33 | 0 |
| #285 | 0.41 | 0.51 | 10/13 | 3921 | 9 | 6 | 3 | 5/7 | 5 | 0 | 2 | 1.00/0.86 | 1 |
| #289 | 0.46 | 0.50 | 7/11 | 4013 | 9 | 9 | 0 | 9/11 | 6 | 3 | 2 | 0.64/0.64 | 0 |
| #296 | 0.81 | 1.12 | 21/29 | 2015 | 5 | 0 | 5 | 0/1 | 0 | 0 | 1 | 1.00/1.00 | 0 |
| #305 | 0.52 | 0.45 | 15/13 | 4047 | 9 | 8 | 1 | 4/6 | 4 | 0 | 2 | 1.00/1.00 | 0 |
| #316 | 0.40 | 0.29 | 9/6 | 3988 | 9 | 6 | 3 | 4/4 | 4 | 0 | 0 | 1.00/1.00 | 0 |
| #328 | 0.62 | 0.68 | 24/22 | 4027 | 8 | 1 | 7 | 1/3 | 1 | 0 | 2 | 1.00/0.67 | 1 |
| #334 | 0.51 | 0.70 | 10/14 | 3807 | 8 | 6 | 2 | 14/25 | 8 | 6 | 11 | 0.80/0.52 | 0 |
| #345 | 0.28 | 0.37 | 8/8 | 3846 | 8 | 0 | 8 | 0/10 | 0 | 0 | 10 | 0.70/0.00 | 2 |
| #350 | 0.67 | 0.68 | 14/11 | 4049 | 9 | 8 | 1 | 12/26 | 4 | 8 | 14 | 0.54/0.27 | 0 |
| #357 | 0.40 | 0.57 | 10/13 | 3850 | 8 | 7 | 1 | 11/27 | 6 | 5 | 16 | 0.41/0.52 | 1 |
| #363 | 0.31 | 0.29 | 7/8 | 4059 | 9 | 4 | 5 | 1/1 | 1 | 0 | 0 | 1.00/1.00 | 0 |
| #364 | 0.45 | 0.47 | 7/10 | 4040 | 9 | 9 | 0 | 3/11 | 3 | 0 | 8 | 0.91/0.82 | 2 |

| failure | count | what it cost |
|---|---|---|
| **Records add little over reading.** The baseline plan already names 0.81 of the landed files alone (11 explore steps); the block holds 120 of 238 landed files (50%), the plan named 81 of them | 20 of 20 tasks | recall fell in 8 tasks (by 0.05 or more), rose in 3 |
| **Wrong records** (no landed file in their paths) | 48 of 168 shown (29%); 8 tasks have 3 or more | those 8 tasks: recall -0.17 mean, cost +11% median |
| **Right records ignored** (landed file in a shown right record, plan did not name it) | 39 files in 20 tasks (#240 7, #350 8, #334 6, #357 5) | the plan reads the file anyway: explore 11 to 13.5 steps |
| **Start set too big** | 3854 B mean, about 1k tokens, 8 to 9 records, 92% stale marked | the tokens cost little (cache read); the calls do: +3 median (12 of 20 tasks more), the worker checks stale records |
| **Recall not called, or called late** | called in 8 of 20 runs, 13 calls | runs that called it: +$0.70 total, +21% median cost, +3 calls; runs that did not: +$0.44, +5% |

Where the memory did help: 5 tasks cost more than 10% less (#229 -13%, #231 -16%, #277 -13%, #305 -14%, #316 -26%; -$0.05 to -$0.12; #316 also 20 logged calls to 6). Four of the five blocks held 4 or more landed files in right records with 0 to 3 wrong ones (#277 had 6 wrong). Having right records does not decide it: #240, #243, #265 and #283 had 8 or 9 right records and no wrong one, and cost 12% to 30% more. No task shows the traps falling: baseline repeated 6, memory 12 (see the judge caveat).

**Reading.** On a plan step the work is to find the files and the cause. The worker does that in 11 steps for $0.52 without help, so a record that lists files saves a few reads and costs a few checks. What a worker cannot find by reading code is *why*: what the owner corrected, which approach failed. That is the only thing the data leave for memory to win, and nine tasks in 20 have such a correction.

**Design (est.).** Start from the correction, not from the file list:

1. Capture one short record per send-back (the owner's correction and the paths it concerns), beside the memo. A memo of a clean task adds no record.
2. At the start, show only those records whose paths meet the task's files, at most 3 lines, with no stale mark and no cutoff list; a stale record is shown if the correction names a file still there (`git cat-file -e`) and dropped if not. No title join, no `(files: ...)` parts, about 500 bytes, not 4 KiB.
3. `wsx memory recall` stays a tool the worker calls; no push of a full block.

Estimate (est.): a send-back costs a build turn, about 15% of a task, $0.45 at the median task ($2.92, median 1 send-back). Baseline plans repeat 6 trap checks in 20 tasks, 0.3 per task. If the records stopped all of them: 0.3 x $0.45 = $0.13 per landed task, minus about $0.01 for the short block. A realistic half is **$0.06 per landed task**, below the $0.15 gain threshold of this file and about the size of the measured loss of today's design (+$0.057). Verdict on the design: it cannot reach the threshold on plan steps alone. It is worth building only if the owner wants memory for another reason (decisions that outlive tasks, the manager); the data do not show a cost reason. This estimate is not measured; nothing was tuned on the replay.

## What runs next

1. Done: steps 4 and 5 landed in `wsx/t380`; the paid arms ran on it (above).
2. `live --cutoff-task 380` again after #380 lands.
3. The owner decides step 8 and whether to build the correction records of "Alternative design". A new start-set rule needs a new split and a new replay (the paid replay is spent: $39.07 of $40).

## Manager rotation (memory instead of a long conversation)

Question (owner): if the manager starts a fresh conversation at a much smaller context (150k or 60k instead of 400k) and gets old context back through `wsx memory recall`, does the answer hold, and how much does manager spend fall? The thread feature is not built; memory is the way back.

`python3 -B scripts/memory-eval.py mpick|mproject|mrun|mjudge|mreport`. Same cap of $40 and the same `spend.jsonl` as above.

### Data now (free; the 7 days of `manager/log.jsonl` up to the last reply, 740 replies)

Real spend $155.25; the price model (in $4, out $20, read $0.20, write $8 per million, Opus 5.5) gives $154.30, 0.6% off, so the model reproduces the log. Context median 140k. The log's own size rotations happen at a median of 164k (108k, 150k and 256k are common), so the manager already rotates near 150k and not at 400k. A fresh conversation's first reply has a context of 55k (the median of the 55 rotations; the minimum seen is 45k). An empty start with the manager instructions and no tools measured 16.6k tokens (one call, $0.17), so the rest of the 55k is tools, notes, overview, ledger and the first turn's reads: what fills it is not measured. A turn adds a median 4.4k tokens.

Projection (`mproject`): each logged reply keeps its output, new tokens and calls; its cache reads scale with the simulated context; a rotation costs a cold write of the start. Size rotations of the log are replaced by the simulated limit, idle rotations (18) are kept. One `wsx memory` call per reply is assumed until a replay measures it. (est.)

| rotation limit | start tokens | rotations | weekly $ | per reply $ | vs logged |
|---|---|---|---|---|---|
| as logged | 55k | 33 | 153 | 0.207 | |
| 400k | 55k | 22 | 165 | 0.224 | +8% |
| 150k + recall | 55k | 47 | 151 | 0.205 | -1% |
| 60k + recall | 55k | 358 | 259 | 0.350 | +69% |
| 60k + recall, lean start (est.) | 54k | 326 | 241 | 0.326 | +57% |
| 100k + recall, lean start (est.) | 54k | 88 | 151 | 0.205 | -1% |

Reading: the manager is already near 150k, so 150k saves about 1%. At 60k the limit is 5k above the fresh start, so the conversation rotates about every second reply, and each rotation pays a cold write of 55k tokens ($0.44): spend rises by 69%. The lean start row removes only the notes (about 2k tokens); a lower limit pays only if the start itself falls, and what fills it is the lever. These rows do not need the paid replay; the replay measures only quality and the calls recall adds.

### Replay (paid; runs with the memory half, after steps 4 and 5)

| Part | Rule |
|---|---|
| Messages | 15 owner turns of the last 7 days that quote a reply or name a task last mentioned more than the 60k window ago (`mpick`, seed 385). The 60k window is the last 1 turn and the 150k window the last 21 (growth 4.4k per turn from a start of 55k). Only 2 of the 19 candidates reach beyond the 150k window (22 and 30 turns; one more pick sits at the 21-turn edge); the other 12 reach beyond the 60k window only. A noisy test of 150k, a clear one of 60k |
| Reference (a) | the real reply and its real cost from the log, with the commands it ran. It is not replayed: the real context is not kept, and a replay of it would cost $10 |
| Candidate (b) | a fresh Opus conversation with the manager instructions (`bench-manager.py instructions`), a start of `decisions.md` and `context.md` cut to the message's time, the last K turns as text (messages, commands, replies), then the new messages. `wsx task ledger\|show\|search` and `wsx memory recall\|search\|status` answer from the state cut at that time; other `wsx task` commands are only recorded. K = (limit - start) / growth: `cut150` K=21, `cut60` K=1, `lean60` (no start notes) K=1 |
| Store | the memory arm's store (step 5 capture on tasks landed before the message), the project transcripts cut at the message; the same gate as the memory arm |
| Judge | Sonnet, one candidate against the reference, not told the limit. Score 2 = same question answered with the same facts and decisions, 1 = same decision but facts missing or changed, 0 = different answer or decision; it lists the facts lost and wrong ones |
| Noise | 3 replies run twice in `cut150` |
| Cost per reply | (a) the log; (b) the CLI result of the replay. The text-only context of (b) is smaller than a real one, so (b)'s cost is read as a floor; the weekly projection above (with the recall calls (b) measured) is the spend figure |
| Spend (est.) | 15 + 3 runs of `cut150` about $3.6, 15 of `cut60` about $2.3, judge about $1.4: about $7.3. With the rest the total is about $34 before the cheap-recall arm, which then gets about $6 of the cap. `lean60` ($2.3) runs last and only if the cap is left; raise the cap to run all |

### Verdict rule (fixed before the replay)

Quality holds at an arm if the mean score is at least **1.6** of 2 and at most **2** of the 15 replies score 0. If the rerun spread of the score in `cut150` is above **0.4**, the quality verdict is unreliable and not used.

Spend counts if the projected weekly spend, with the recall calls per reply that the replay measured, falls by at least **15%** (about $23 a week) against the logged week.

| Verdict | Condition |
|---|---|
| Rotate at this limit with recall | quality holds and spend counts |
| Not worth it | spend does not fall by 15%, whatever the quality (the current data say this for 150k and 60k) |
| Quality fails | spend counts but quality does not hold: recall does not bring back what the cut lost; the lost facts list says which |

What would change the answer is a smaller start (the 55k), not the limit. If the replay shows quality holds at `lean60`, the next step is to measure what fills the start and cut it, then run `mproject` again.

### Replay results (2026-11-11, `memory-eval.py mrun|mjudge|mreport`)

**Stopped by the cap.** The estimate was $3.6 for 18 `cut150` runs; a run cost $0.36 to $0.58 (median $0.41), twice the logged $0.22 for the same replies: a fresh conversation pays a cold write of its first context (47k to 54k tokens measured at the first call, 55k assumed), so the text-only context of the replay was not a floor (the table above said so). `cut150` ran 13 of the 15 picks (3895 and 3903 skipped at the cap) and the 3 repeats ($6.85 with the repeats). `cut60` and `lean60` did not run: 15 runs would cost about $6 and $2.24 was left.

| reply | distance | $ real | $ cut150 | calls | recall calls | score |
|---|---|---|---|---|---|---|
| 2668 | 3 | 0.09 | 0.36 | 3 | 0 | 1 |
| 3286 | 3 | 0.77 | 0.46 | 7 | 3 | 1 |
| 3592 | 21 | 0.30 | 0.44 | 6 | 0 | 1 |
| 3653 | 3 | 0.09 | 0.41 | 3 | 1 | 1 |
| 3664 | 3 | 0.36 | 0.42 | 4 | 0 | 0 |
| 3665 | 2 | 0.06 | 0.39 | 2 | 0 | 1 |
| 3678 | 3 | 0.22 | 0.46 | 4 | 3 | 1 |
| 3719 | 2 | 0.93 | 0.39 | 3 | 0 | 0 |
| 3720 | 3 | 0.15 | 0.40 | 3 | 0 | 1 |
| 3722 | 2 | 0.31 | 0.38 | 3 | 0 | 0 |
| 3821 | 30 | 0.43 | 0.58 | 13 | 2 | 0 |
| 3822 | 22 | 0.09 | 0.40 | 5 | 1 | 1 |
| 3891 | 2 | 0.16 | 0.49 | 5 | 0 | 1 |
| 3895, 3903 | 3, 2 | 0.22, 0.12 | not run | | | |

| measure | `cut150` | rule |
|---|---|---|
| score mean (13 replies) | **0.69** of 2 (no reply scores 2; 9 score 1, 4 score 0) | at least 1.6 |
| replies scoring 0 | **4** of 13 (3664, 3719, 3722, 3821) | at most 2 of 15 |
| rerun spread of the score (3 replies, twice) | 0.00 (cost 0.09) | quality verdict unreliable above 0.4: reliable, but 3 pairs |
| recall calls per reply (16 runs) | 0.62 | |
| projected weekly spend with that (`mproject`) | 150k -6%; 60k +64%; 60k lean (est.) +51%; 100k lean (est.) -6% | at least -15% |

| arm | spend counts | quality holds | verdict |
|---|---|---|---|
| `cut150` (150k + recall) | no (-6%) | no (0.69, 4 zeros) | **Not worth it**, and quality fails too |
| `cut60` (60k + recall) | no (+64%) | not run | **Not worth it** whatever the quality; not replayed |
| `lean60` | no (+51%, est.) | not run | not worth it on spend; not replayed |

The lost facts (reply 3664 onward in the `mreport` output) are what the cut conversation held and `wsx memory` did not bring back: the numbers of a search done earlier in the same hour, the task a worker was just started on, the reason for a decision in the last turns. 5 of the 13 replies called `recall` or `search` (0.77 calls per reply on the 13); the records are tasks' memos, which hold no manager conversation. The spend figure does not depend on the replay: the start of a fresh conversation (55k) is what costs. Next step if the owner wants it: measure what fills the 55k and cut that, then run `mproject` again.
