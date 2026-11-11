# Memory evaluation (step 6)

Status: baseline half measured on 2026-11-11 ("Baseline"). The memory half runs after build steps 4 and 5 (`docs/design/memory-system.md`). Question: does memory make a wsx task better and cheaper? Owner decisions: the vault stays in dotfiles `memory.py`; plain Claude chats get no start-up memory load unless this shows a measured gain; step 8 waits for the verdict.

## Method

`python3 -B scripts/memory-eval.py <pick|run|judge|report|live>`. Every number below comes from it. Runs and spend: `.work/memory-eval/` (`runs/<arm>/<task>-<rep>.json`, `judged.json`, `spend.jsonl`).

| Part | Rule |
|---|---|
| Tasks | 20 landed tasks of the wsx store (`pick`, seed 385): plan step is step 1, a claude worker, a landed diff of crates or scripts, a memo or summary, plan cost at most $1.50, no audit, nothing that touches `memory`. Quotas by the crate most files changed in: web 4, daemon 3, terminal 2, core 4, cli_tui 3, scripts and docs 2, rest by pool; at least 6 with a send-back after the plan. The daemon and terminal pools are small (1 and 0), so other areas fill them |
| One run | the plan step only: a git worktree at the task's base, the task's own model and effort, the task's original prompt, `Task context from the host` rebuilt for its start time, the step text of the task, the worker instructions (today's text, the same for every task and arm), the owner's skills plugin, the result schema. Stopped at the first result |
| Isolation | own `XDG_STATE_HOME` and `XDG_CONFIG_HOME` per run; the real task store, memory and config are only read |
| Baseline arm | what a worker had at the task's start: the host context and `wsx task search` through a shim, over the task store cut at the task's start (every log line after it removed; a result only of tasks landed before; notes lines dated before it). No `wsx memory`. The host context is not logged: it is rebuilt from the task's files, the tasks open at its start that share those files, and the manager's `context.md` lines that name them. `context.md` is rewritten by the manager and has no history, so lines that name a task from this one on are dropped (a proxy) |
| Memory arm | the baseline, plus a start set and a brief, and `wsx memory recall\|search\|status` through the shim on a store cut at the start. Store: step 5's capture rule (a landed task's memo becomes a fact with the paths of its landed diff and its commit) applied to every task landed before the start; rules: the decisions dated before it. The paid arm uses the code of steps 4 and 5 of the `wsx` given with `--wsx`; the script refuses to start without them. A stub of the start set and the capture exists for the dry run only |
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

## What runs next

1. Steps 4 and 5 land in `wsx/t380`: the start set command and the capture command. The constants `CAPTURE_SUB` and `START_SUB` in the script name them; change them to the real names.
2. `python3 -B scripts/memory-eval.py run --arm memory --wsx <t380 binary>`, then `--arm memory-cheap`, then `judge` and `report`; add the table and the verdict to this file.
3. Run `live --cutoff-task 380` again after #380 lands.
4. If the verdict is no gain, a section "Alternative design" follows here: where it failed per task (wrong records retrieved, right records ignored, start set too big, recall not called), the design and its estimate.
