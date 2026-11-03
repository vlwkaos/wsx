# Usage audit: where tokens, time and quality go

Status: audit of project `wsx` (state `~/.local/state/wsx/tasks/wsx-8cbe3f08`), logs 10-16 to 10-28. Numbers come from `scripts/usage-report.py` (stdlib, `/usr/bin/python3 scripts/usage-report.py <state dir> [--since D] [--until D] [--task N]`). Usage fields: docs/tasks.md "Usage".

**Answer.** The daily rise comes from volume, not from more expensive work. Since 10-26 a landed task costs less, waits less and fails fewer checks. More tasks per day, each with 5 steps, multiply the turns. Within that volume, the cost goes to long contexts, to the turns after the last step, and to cache rewrites after idle gaps longer than 1 hour.

## Evidence (report output)

| day | tasks | landed | worker $ | worker turns | turns/task | $/turn | turns > 200k ctx | manager $ |
|---|---|---|---|---|---|---|---|---|
| 10-23 | 7 | 7 | 14.49 | 14 | 2.0 | 1.03 | 0 | 4.64 |
| 10-24 | 7 | 7 | 90.76 | 92 | 13.1 | 0.99 | 37 | 13.13 |
| 10-25 | 17 | 13 | 105.95 | 82 | 4.8 | 1.29 | 27 | 11.81 |
| 10-26 | 24 | 22 | 177.22 | 186 | 7.8 | 0.95 | 40 | 17.60 |
| 10-27 | 30 | 30 | 174.61 | 202 | 6.7 | 0.86 | 38 | 18.34 |
| 10-28 | 14 | 8 | 139.43 | 155 | 11.1 | 0.90 | 52 | 12.86 |

```
window start .. 2026-10-25: tasks 53, landed 36, worker $419.75 in 299 turns, manager $40.99 in 352 turns
per landed task: $8.58, 6.5 turns, 17.9 h wall, 0 min check wait, 0.9 send-backs, 4.2 failed check runs
window 2026-10-26 .. now:   tasks 68, landed 60, worker $379.97 in 494 turns, manager $48.80 in 370 turns
per landed task: $3.49, 6.0 turns, 0.9 h wall, 32 min check wait, 0.6 send-backs, 1.4 failed check runs
```

Window since 10-24 (92 tasks, worker $608.50, manager $73.75):

| worker turns by | turns | cost $ | $/turn | median ctx | cache write share |
|---|---|---|---|---|---|
| context > 200k | 151 | 270.28 | 1.79 | 259k | 21% |
| other | 447 | 273.93 | 0.61 | 110k | 32% |
| after > 1 h idle (all days) | 73 | 171.22 | 2.35 | 208k | 62% |
| after 5-60 min idle (all days) | 178 | 93.48 | 0.53 | 189k | 11% |
| first turn after a session limit | 10 | 31.97 | 3.20 | 231k | 56% |
| step `build` | 108 | 204.68 | 1.90 | 121k | 20% |
| after the last step: `add` (send-back) | 32 | 60.83 | 1.90 | 197k | 42% |
| after the last step: `check_run` answer | 57 | 40.13 | 0.70 | 194k | 22% |
| after the last step: `conflict` | 22 | 35.32 | 1.61 | 285k | 19% |
| after the last step: no event (result retry, limit resume) | 109 | 47.56 | 0.44 | 186k | 37% |
| model opus | 569 | 565.04 | 0.99 | 144k | 28% |
| model sonnet | 29 | 3.15 | 0.11 | 59k | 49% |
| manager turn: events | 350 | 41.10 | 0.12 | 97k | 30% |
| manager turn: owner | 165 | 27.38 | 0.17 | 100k | 35% |

Fields used: `result.usage` and `reply.usage` (`cost_micro_usd`, `session_cost_micro_usd`, `context_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `duration_ms`, `model`), plus `rotated`, `revision`, `message.mode`, `check_queued`, `check_run`, `checks`, `check_retry`, `conflict`, `landed`, the manager's `message.from` and `delivered`, and `task.json` `steps`, `files`, `model`. Lines from before `session_cost_micro_usd` existed (before commit 94f0b26, 10-21) store the session's running total in `cost_micro_usd`. The report turns those totals back into per-turn costs. Cache write share prices writes at 2x and reads at 0.1x base input: the transcripts show only 1-hour cache writes (`ephemeral_1h_input_tokens`). The 5-60 min idle row writes 11% and the over-1-h row 62%, so the cache lives 1 hour.

## What is traceable and what is not

| Traceable now | Gap |
|---|---|
| cost, tokens, context and model of each worker turn and manager turn | step, trigger and `calls` are on `result` since #125; older lines replay `task.json` `steps` |
| rotations, session limits (`<synthetic>` results), idle gaps | lines before 10-24 have no `context_tokens` or `model` |
| send-backs, failed check runs, check-queue wait, wall time to land | subagent and tool-call cost inside a turn: only the turn's sum (`calls` counts main calls since #125; older lines estimate it as cache reads / context) |
| repeated commands, from the first line of `agent` | `agent` keeps one line, so heredoc bodies (about 1,230 `python - <<EOF`) are not visible |
| | quality after landing (To check results, reverts, follow-up fix tasks) is not tied to the task's cost |
| | the owner's own Claude use outside wsx, and the plan's 5-hour quota left |

## Ranked causes and levers

| # | Cause (since 10-24) | Lever | Speed | Cost | Quality |
|---|---|---|---|---|---|
| 1 | Volume: 7 → 30 tasks a day, 5 steps each; $/turn stayed about $1 | manager: one task per request area (`add` over `new`); `--skip context,plan` with `--model sonnet` for exact changes (only 29 sonnet turns) | = | -30% for small tasks | = when the prompt carries the files |
| 2 | Contexts over 200k: 151 turns, $270 (44%), $1.79/turn against $0.61 | `rotateTokens` 350000 → 200000; rotate at a step start (review and memo need the diff, not the history) | = | estimate -20 to -30% on these turns | review sees a fresh context, which can be better |
| 3 | Cold cache after more than 1 hour idle: 73 turns, $171, 62% cache write; session limits add $57 | before a turn after more than 1 h idle with context over 150k, rotate instead of resuming (a fresh 30k start writes less than 250k) | = | estimate -$1 to -2 per cold turn | needs good handoff notes |
| 4 | Turns after the last step: 234 turns, $196; send-backs $61 at $1.90 | the manager's review findings go into the review step (one turn) rather than an `add` after the checks; handle conflicts in the host when only CHANGELOG or docs conflict | faster land | about -$60 since 10-24 (estimate) | = |
| 5 | Retries for a missing result: 109 turns, $48 | count these turns per step in the host and put the result call in each step prompt; find why a step ends without one | = | up to -$48 | = |
| 6 | Manager events turns: 350, $41; 155 quiet | `eventModel: sonnet`, or handle a `landed` or `closed` event with no memo in the host with no turn | = | up to -$41 (estimate) | = |
| 7 | `context` step writes 66% cache, then `/recall` again in build | the host puts the seek result for the task's `--files` in the first prompt; drop the `context` step for kinds that do not need it | -1 min per task | up to -$41 | = |

## Next 3 changes

1. Log `step` and `trigger` on each `result`, and the turn's main-call count, in `crates/wsx/src/task/host.rs`. Then the report stops guessing, and the loop below has exact data.
2. Lower `tasks.agent.rotateTokens` to 200000, and rotate before a turn that follows more than 1 hour idle (`rotation_due`). This targets causes 2 and 3 ($270 and $171).
3. Manager rule: an exact change runs on sonnet with `--skip context,plan`, and review findings go into the review step. Measure with the kind table below.

Changes 1 and 2 shipped in #125 (`rotateIdleSeconds`, default 3600, needs a context of at least half of `rotateTokens`), with the stale docs/tasks.md line removed.

## Self-improvement loop (owner msg 1441)

The loop uses existing pieces only: a routine, the manager's `notes/decisions.md`, the ledger and the memos. It adds no new scheduler or store.

```mermaid
flowchart LR
  R[routine: usage-report.py daily] -->|last 10 lines = Summary| E[manager event]
  E --> M[manager compares with decisions.md baselines]
  M -->|finding| D{who acts}
  D -->|config or prompt| C[project task: wsx.config.yml / workflow words]
  D -->|product| P[wsx task: host code, script, check]
  D -->|manager habit| N[manager notes]
  C & P & N --> B[decisions.md line: change, date, baseline numbers, kind]
  B -->|8 landed tasks of that kind later| R
```

| Part | How | Owner |
|---|---|---|
| Measure each task | cost, turns, wall time to land, check wait, send-backs, failed checks, `limit`, context trail, per kind (area of the first path) | wsx (logs, report) |
| Measure each manager turn | kind (owner, events, recap), cost, context, quiet | wsx |
| Cadence | one routine: `wsx task routine add usage -p wsx --cron "0 6 * * *" --arg /usr/bin/python3 --arg scripts/usage-report.py --arg <state dir> --arg=--days --arg 7 --owner-said <id>`; the recap already reports cost since the last recap | project sets the schedule |
| Daily digest | one routine per project running `wsx task digest` (docs/tasks.md "Routines"). The command posts to the chat itself and prints nothing, so it costs no manager turn: its text is fixed by its inputs (the rule below) | project sets the schedule; wsx writes the text |
| Review | the routine's event wakes the manager; it reads only the Summary and the kind table, never the full output | wsx gives the event; the manager judges |
| Act | config or prompt change (model, steps, `--skip` per kind, `rotateTokens`, workflow words) goes through a project task; deterministic work goes through a wsx task; a manager habit goes into its notes | project for config, wsx for code |
| Prove | each change is one `decisions.md` line: date, kind, baseline medians. After at least 8 landed tasks of that kind, it compares `--until <date>` with `--since <date>` | the manager |
| Keep or roll back | keep it when $/task or turns fall and failed checks and send-backs do not rise; otherwise revert the commit through a task and note why | the manager; a revert is a normal task |

The rule for code or judgment: a step the agents repeat with the same inputs and the same output goes into code. A step whose output depends on reading the owner's words or the diff stays with the agent.

| Repeats today (agent log counts) | Make deterministic as |
|---|---|
| 590 background-job polls (`until grep`, `sleep`), 52 `TaskStop`, 46 `ScheduleWakeup` | `wsx task check-run` blocks and answers itself; the build words forbid polling |
| 171 `cargo build`, 83 `cargo fmt` before checks that the host runs anyway | let the host's checks be the build; agents run only the scenario they changed |
| 185 skill-helper `bash .../memo.sh context` runs, often from the wrong folder | the host puts project context in the first prompt |
| manager: 161 `task show`, 146 `task ledger` (30 piped to python), 132 `git diff` per review | the review event carries diffstat, files outside `--files` and the checks |
| manager: 192 notes reads and edits by python to fold memos | the host appends the memo line; the idle task deduplicates |
| manager: 130 `task land` after a review with no findings | stays with the agent: the judgment is the review itself |

## Findings (2026-11-03)

Assuming: the 10-31 limit is the 7-day one, and plan limits weigh tokens as the API prices them; the shares below use the report's weights (write 2x, read 0.1x base input). The owner's Claude use outside wsx is not in these logs.

**Answer.** The cache already hits: 98% of worker input tokens and 97% of manager input tokens are cache reads. What spends the plan is reads times context times calls, then output; cold writes are a quarter. A limit hit on 10-31 15:40 and held until 11-03 05:00 (manager `limit` entry; over 2 days, so assumed the 7-day one), so 11-01 and 11-02 have no tasks. Per landed task stayed at $4.2-4.6 since 10-21: the levers that shipped cut cold turns and big contexts, but most tasks still run opus with all 5 steps.

### Windows (`/usr/bin/python3 scripts/usage-report.py ~/.local/state/wsx/tasks/wsx-8cbe3f08 …`)

| window | tasks / landed | $/landed | worker $ / turns | manager $ / turns | ctx > 200k $ | idle > 1 h $ | rotations size / idle |
|---|---|---|---|---|---|---|---|
| owner baseline, 7 d to 10-31 | | 4.16 | 767 / 913 | 99 / 686 | 344 | 117 | |
| same, rerun `--since 10-25 --until 10-31` | 128 / 119 | 4.21 | 778 / 927 | 101 / 696 | 347 | 119 | 19 / 4 |
| prior, `--since 10-21 --until 10-27` | 89 / 82 | 4.59 | 672 / 644 | 70 / 512 | 327 | 146 | 2 / 0 |
| last, `--days 7` (10-28..11-03) | 63 / 55 | 4.32 | 278 / 419 | 57 / 371 | 94 | 22 | 17 / 4 |

The rerun differs from the baseline by under 2%: results logged after 10-31 for tasks that started before it.

### Where it goes (last window)

| worker cost by | $ | share | note |
|---|---|---|---|
| cache reads | 309 est. | 62% | 626M tokens, 2,322 main calls |
| cache writes | 123 est. | 25% | 12.9M tokens; first turns 3.8M, rotations 1.1M, idle > 1 h 0.9M |
| output | 67 est. | 13% | |
| step `build` / `plan` / `context` / `memo` / `review` / after | 72 / 58 / 51 / 38 / 26 / 31 | | `context` writes 54%; `memo` counts the check answers after it |
| opus / sonnet | 258 in 306 turns / 16 in 102 | | sonnet $0.16 a turn against $0.84 |
| manager events / owner / other turns | 29 / 23 / 5 | | 152 of 232 event turns quiet |

| cold start (no warm cache) | turns | $ | read share |
|---|---|---|---|
| worker first turn of a task | 62 | 40.20 | 92% |
| worker after rotation | 21 | 14.93 | 94% |
| worker after > 1 h idle, not rotated | 6 | 11.69 | 92% |
| worker after a session limit | 3 | 6.59 | 81% |
| manager after rotation (150k) | 19 | 6.69 | 77% |
| manager fresh conversation (instruction digest, restart) | 3 | 2.81 | 9% |

### Optimizations as designed

| lever | works? | numbers |
|---|---|---|
| `rotateTokens` 200k | partly | 17 size rotations, median 216k, max 418k at rotation. 64 turns still pass 200k ($94): 25 grew past it inside one turn ($61, mostly `build`), 39 started above it as check-run answers or result retries, which never rotate ($33) |
| `rotateIdleSeconds` 3600 | yes | idle > 1 h cost $146 → $22; 4 rotations. Misses: 3 check-run answers after a check wait over 1 h (median wait 60 min) at ≥ 100k, 1 add below 100k |
| `--skip` steps | rarely used | 18 of 63 tasks have fewer than 5 steps, 16 of them sonnet; 40 opus tasks kept all 5 (median $5.73, 7 turns) |
| sonnet for stated changes | yes where used | sonnet 3 steps median $0.54, 4 turns; 5 steps $0.94. 21 of 63 tasks, 6% of worker cost |
| quiet event turns (`event_wakes`) | not live in the window | landed on main 11-03 11:32 (b7299489); window shows 25 quiet `landed` turns ($1.92) and 50 quiet other ($4.13) |
| empty-stdout routines | yes | 0 digest events; 2 `routine usage` events, which print a summary by design |
| result retries | miss | 35 "no event" turns after the last step ($12.67); 33 logs of "ended its own turn with no result" |

### Top 5 changes, by tokens saved

| # | change | evidence | saves (est., per week at last window volume) | cache effect |
|---|---|---|---|---|
| 1 | Opus tasks drop `context` and `memo`: the host puts the seek packet for `--files` in the first prompt, the build result carries the memo | `context` $51 at 54% write, `memo` about $20 of its $38; 40 opus tasks run all 5 steps | $50-60 (18-22% of worker) | removes one cold turn per task; the first turn still writes, but once |
| 2 | Sonnet with `--skip context,plan` as the manager's default when the prompt names files and the change | 40 opus 5-step tasks at median $5.73 against $0.54 | about $5 per moved task; 10 moved = $50 | sonnet has its own cache; no switch inside a task, so no extra writes |
| 3 | Bound growth inside a `build` turn: wide reads go to a sonnet subagent, check output through `tail`, no whole-file reads | 25 turns grew past 200k inside the turn ($61); build 16 calls a turn at 119k | $20-30 | fewer reads per call; hit rate unchanged, read volume down |
| 4 | Ask for the missing result inside the step prompt and find why a turn ends without one | 35 result-retry turns at 174k ($12.67); 33 no-result logs | about $12 | each retry reads the full context at 1-2 calls |
| 5 | Measure `event_wakes` (live 11-03), then give `quiet ready` no turn of its own: batch it with the review | 152 quiet event turns, about $10; 25 quiet `ready` turns $4.26 | $8-12 manager | fewer manager calls; the 150k manager context is read less often |

Rotating check-run answers and retries above 200k would not pay: they run 1-2 calls, and a rotation costs $0.71 a turn plus the handoff. Prove each change with the loop above: one `decisions.md` line, then compare after 8 landed tasks of that kind.

### Decisions (2026-11-03)

- Changes 1 and 4: the owner's steps become `plan, build, review`; `review` asks for the memo in the result. `context` loaded no project knowledge from a worktree (recall `mode=none`), so it goes with no replacement. The 33 "no result" logs were Claude's own turns for background-job notices after a result (25 model turns in 19 tasks; the host's result retry ran once), so each step's text asks to end background jobs first and the host logs a result that comes while one runs. Compare after 8 landed tasks: `$/landed` (4.32) and turns after the last step (35).
