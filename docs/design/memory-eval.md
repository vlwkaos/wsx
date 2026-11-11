# Memory evaluation (step 6)

Status: baseline half measured on 2026-11-11 ("Baseline"); offline retrieval measured the same day with a stub start set ("Offline retrieval"). The memory half runs after build steps 4 and 5 (`docs/design/memory-system.md`). Question: does memory make a wsx task better and cheaper? Owner decisions: the vault stays in dotfiles `memory.py`; plain Claude chats get no start-up memory load unless this shows a measured gain; step 8 waits for the verdict.

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

Question: does the start set hold the files a plan step reads? Target set by the owner: at least 60%. `python3 -B scripts/memory-eval.py offline [--final] [--show N...]` (no model call; about 8 minutes on 6 jobs). It reads the task store, the git history and `wsx memory`, and writes `.work/memory-eval/offline/rows.json` (`--rows` scores it again). Run on a copy of the store (`MEMORY_EVAL_STORE`), 136 landed tasks #133 to #387 that have a plan result. Split by start time: the first 91 tune, the last 45 are held out; the script prints the held-out third only with `--final`, and it was shown once, after the rules below were fixed.

**Start set used here is a stub.** `wsx memory start` and `capture` (#386) are not on this branch. The stub is the design's rule (`memory-system.md`): the capture rule of the dry run (memo plus all landed paths, commit = landed commit) on every task landed before the start, then the records that have a path matching the task's `--files` and are not stale (`git diff --quiet <commit> <base> -- <paths>`, base = the task's own base, any path counts), most matching paths first, 4 KiB. When #386 lands, the script uses `wsx memory start` by itself and reads its slugs; the stale and ranking columns then describe the real code. The area-brief part is empty by construction: wsx declares no `memory.areas`.

**Labels.** (a) files the plan step read: `Read` paths and every repo file a `rg grep sed cat head tail awk wc` Bash line names, in the agent lines before the plan result, kept only if the file exists at the task's base; a directory argument counts apart (not a file). (b) files of the landed diff. (c) trap: an earlier landed task's memo names a file of this task (not a file most tasks change) and a send-back (`add` after the plan, not a land notice) shares at least 2 rare words (used by at most 6 memos) with it. (c) is rare: 3 pairs in the tuned third, 4 in the held-out one; hand check of the 3 (tuned): 2 are real (#182 effort, #265 tab bar), 1 is weak (#197). Read it as a sign, not a rate.

**What the log loses (so (a) is a lower bound).** Of 3827 Bash lines before a plan result, 2481 (65%) end in a clip mark, and their last word is dropped; the 35 `Agent` calls (Explore) are logged without arguments, and `Grep` and `Glob` are not logged at all. 1508 files read in 136 plan steps (11 per task).

### Result (mean over tasks; start = stub)

| arm | recall (a) tuned / held out | precision (a) | recall (b) | tasks with an empty set (held out) | traps in set (held out) |
|---|---|---|---|---|---|
| start (stub) | 0.15 / **0.13** | 0.16 / 0.07 | 0.21 / 0.16 | 17 of 45 | 1/4 |
| paths, each record cut to the paths unchanged since its commit | 0.50 / 0.43 | 0.09 / 0.12 | 0.50 / 0.44 | 7 | 2/4 |
| same, ranked by BM25 of the title | 0.39 / 0.40 | 0.09 / 0.13 | 0.38 / 0.44 | 7 | 3/4 |
| BM25 of the title only (top 5 facts, no paths) | 0.49 / 0.48 | 0.17 / 0.20 | 0.54 / 0.47 | 0 | 3/4 |
| same, cut to unchanged paths | 0.09 / 0.18 | 0.10 / 0.15 | 0.08 / 0.14 | 0 | 3/4 |
| paths, stale ignored | **0.74 / 0.65** | 0.08 / 0.07 | 0.71 / 0.67 | 7 | 2/4 |
| `wsx memory recall "<title>"` (top 5) | 0.03 / 0.08 | 0.07 / 0.11 | 0.04 / 0.09 | 26 | 1/4 |

Paired per task, recall (a), mean and seeded bootstrap 95% interval (tuned / held out): start minus recall +0.118 [+0.068, +0.169] / +0.052 [-0.041, +0.149]; unchanged paths minus start +0.357 [+0.300, +0.421] / +0.302 [+0.200, +0.398]; stale ignored minus start +0.595 [+0.526, +0.660] / +0.521 [+0.398, +0.632].

Per area, recall (a), tuned / held out (tasks: tuned / held out). Terminal: no landed plan task.

| area | tasks | start (stub) | unchanged paths | stale ignored | recall |
|---|---|---|---|---|---|
| cli_tui | 28 / 18 | 0.16 / 0.16 | 0.52 / 0.51 | 0.84 / 0.79 | 0.01 / 0.08 |
| core | 12 / 9 | 0.21 / 0.18 | 0.49 / 0.53 | 0.78 / 0.79 | 0.05 / 0.06 |
| daemon | 4 / 0 | 0.03 / - | 0.46 / - | 0.72 / - | 0.03 / - |
| scripts_docs | 18 / 12 | 0.11 / 0.10 | 0.21 / 0.18 | 0.42 / 0.30 | 0.09 / 0.06 |
| web | 29 / 6 | 0.15 / 0.00 | 0.67 / 0.53 | 0.83 / 0.72 | 0.00 / 0.11 |

Why a file read is not in the start set (share of the files read; tuned 958 files / held out 550):

| cause | tuned | held out |
|---|---|---|
| in the start set | 0.14 | 0.10 |
| stale record, the file itself unchanged since the commit | 0.60 | 0.66 |
| stale record, the file itself changed | 0.16 | 0.17 |
| never in an earlier landed diff | 0.07 | 0.02 |
| no record (the earlier task left no memo) | 0.02 | 0.02 |
| declared `--files` do not reach it | 0.01 | 0.03 |
| cut by the 4 KiB budget | 0.00 | 0.00 |

**Verdict: the stub start set MISSES the 60% target: 0.13 held out (0.15 tuned).** This is the design's stale rule, not `wsx memory start`; read the line again with the real code.

From the data: (1) The cause is staleness, not missing paths, ranking or budget. A record carries every path of the landed diff (median 8, mean 11, and `CHANGELOG.md`, `docs/tasks.md` or `scripts/test-task.py` among them), and the rule drops the whole record when any one path changed; 76% to 83% of the files read are in such a record. 60% to 66% of them are files that did not change themselves. (2) Cutting each record to its unchanged paths lifts recall to 0.50 tuned and 0.43 held out: better, still short. (3) Keeping stale records in the set, marked `stale`, reaches 0.74 tuned and 0.65 held out; that is the change that meets 60%, chosen on the tuned third and confirmed once on the held-out one. It is a pointer, not a claim: the record says which files an earlier task touched, and the worker reads them. Precision stays near 0.1 (about a dozen paths per set). (4) The budget never binds (0.00), and the path-first ranking beats BM25 order among the same records (0.50 against 0.39 tuned, 0.43 against 0.40 held out). (5) The title alone finds 0.48 to 0.49 with no path at all, and `recall` finds 0.03 to 0.08, because its top 5 are all transcript hits (a task's story), which carry no paths; a union of both was not measured. (6) `scripts_docs` stays at 0.30 to 0.42 even then: those plans read many files of code that the docs task does not change (17.8 files read, 2.1 landed per task).

**Hand check (3 tuned tasks, labels against the raw `log.jsonl` lines).** `python3 -B scripts/memory-eval.py offline --rows --show 135 141 170` prints the same.

| task | files read by the script | raw lines (plan result at line 11, 18, 32) |
|---|---|---|
| #135 cli_tui | `host.rs`, `slot.rs`, `test-task.py` | line 5 `sed -n 80,210p crates/wsx/src/task/host.rs`; line 6 `cat crates/wsx/src/task/slot.rs`, `sed -n 2270,2400p scripts/test-task.py`. The `grep ... -r crates/wsx/src scripts` of line 4 names directories only; the glob `crates/wsx/src/task/*.rs` of line 7 is not expanded: both lost |
| #141 web | `http.rs`, `lib.js`, `viewer.js`, `web-tasks-check.mjs` | line 5 `sed -n 1175,1230p crates/wsx-web/src/http.rs` and `cat crates/wsx-web/web/viewer.js`; line 7 `sed -n 1,40p crates/wsx-web/web/lib.js`; lines 6 and 15 `scripts/web-tasks-check.mjs`. Line 5 also has `sed -n 85,110p crates/wsx-web/web/che…` (clipped): `checks.js` is a landed file the label misses |
| #170 scripts_docs | `config/global.rs`, `config/project.rs`, `plugins.rs`, `http.rs`, `docs/plugins.md` | line 5 `cat docs/plugins.md`; line 19 `crates/wsx-daemon/src/plugins.rs`; line 20 `crates/wsx-web/src/http.rs`; line 22 `sed -n 280,300p crates/wsx-core/src/config/global.rs` and `head -15 .../project.rs`. `docs/web-client…` of line 5 is clipped: lost. Knowledge files outside the repo (`dgv3/notes/...`) are not labels |

**Next.** When #386 lands: `python3 -B scripts/memory-eval.py offline --wsx <t380 binary> --final` (the cause table and the verdict then describe `wsx memory start`; capture's own `paths` field is read when it has one, else the `Files:` line). The real branch of the script (a worktree at the task's base as the project path) ran only as a probe on one task.

## What runs next

1. Steps 4 and 5 land in `wsx/t380`: the start set command and the capture command. The constants `CAPTURE_SUB` and `START_SUB` in the script name them; change them to the real names.
2. `python3 -B scripts/memory-eval.py run --arm memory --wsx <t380 binary>`, then `--arm memory-cheap`, then `judge` and `report`; add the table and the verdict to this file.
3. Run `live --cutoff-task 380` again after #380 lands.
4. If the verdict is no gain, a section "Alternative design" follows here: where it failed per task (wrong records retrieved, right records ignored, start set too big, recall not called), the design and its estimate.

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
