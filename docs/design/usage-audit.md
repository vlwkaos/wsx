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
  R[routine: retrospect.py daily] -->|chat post, no turn| E[owner reads it]
  E -->|asks| M[manager compares with decisions.md baselines]
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
| Cadence | one routine, the retrospect (docs/design/dynamic-routines-retrospect.md): `wsx task routine add usage -p wsx --cron "0 6 * * *" --arg /usr/bin/python3 --arg scripts/retrospect.py --arg <state dir> --arg=-p --arg wsx --owner-said <id>`; it posts to the chat and only reports (the owner rejected daily audit tasks) | project sets the schedule |
| Daily digest | one routine per project running `wsx task digest` (docs/tasks.md "Routines"). The command posts to the chat itself and prints nothing, so it costs no manager turn: its text is fixed by its inputs (the rule below) | project sets the schedule; wsx writes the text |
| Review | the owner reads the retrospect's post; `usage-report.py` stays the tool for a deeper audit by hand | the owner |
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

## Audit 2026-11-04

Replaces "Findings (2026-11-03)" (#169). Window: rolling 7 days, plus the tasks since #169 landed. Transcript evidence is in each task's `log.jsonl` and `~/.claude/projects/-Users-vlwkaos-ws-wsx-t<n>/`.

**Answer.** Per landed task fell from $4.32 to $2.38 over 7 days and to $1.73 since 11-03. Most of the fall comes from the mix of tasks, not from the two changes: the expensive 10-28 tasks left the window, and 6 of the 10 tasks landed since 11-03 ran on sonnet. The two changes have not run yet: #176 still runs a `context` step, and only #174 ran build only. Since 11-03 the cost goes to turns started by owner messages, restarts and check-run answers, to waiting for checks inside a turn, and to failed check runs that each wait 20-90 min in the queue. (Corrected 11-04, #182: an earlier version put 39% on the `context` step; see the step row below.)

### Evidence

```
$ /usr/bin/python3 scripts/usage-report.py ~/.local/state/wsx/tasks/wsx-8cbe3f08 --days 7
window 2026-10-29 .. now: tasks 50, landed 46, worker $183.32 in 330 turns, manager $46.71 in 269 turns
per landed task: $2.38, 5.5 turns, 2.1 h wall, 50 min check wait, 0.3 send-backs, 1.6 failed check runs
turns over 200k context: $49.22; turns after > 1 h idle (cold cache): $4.82
$ ... --since 2026-11-03
window 2026-11-03 .. now: tasks 12, landed 10, worker $19.60 in 79 turns, manager $8.23 in 34 turns
per landed task: $1.73, 6.5 turns, 3.2 h wall, 181 min check wait, 0.2 send-backs, 1.6 failed check runs
turns over 200k context: $0.00; turns after > 1 h idle (cold cache): $0.99
```

| since 11-03 | $ | turns | where it went (log.jsonl `result`, `check_run`) |
|---|---|---|---|
| step `context` (all tasks) | 3.06 of 19.60 | 5 | only the turns the step started (trigger `step`): $0.47-0.79 each, mostly the first-turn cache write any first step pays. The other $4.62 in 9 turns are `message`, `restart`, `check_run` and `rotation` turns that came while the record still named `context` (#167 $2.98, #173 $0.95, #170 $0.68); `usage-report.py` now lists them by trigger. In #173, recall returned `mode=none` and 4 unrelated vault hits |
| #167 opus, 5 steps | 4.49 | 14 | $3.71 while the step was `context`, of which $0.73 is its own turn: 2 owner messages grew it to 141k, then 2 `restart` turns after a background `check-run` was killed at 30 min; 4 failed runs (test-task.py timeouts, 35-78 min queue each); 363 min check wait |
| #175 sonnet, build+review | 2.22 | 3 | one build turn of 323 min and 87 calls: it polled with `until [ -s …web.out ]` (47 of 113 agent lines; about 43% of its cache reads, est.); 4 failed `web` runs, the first one a stale "Page" assert |
| #172 sonnet, 3 steps | 1.92 | 16 | 7 `restart` + 3 `rotation` turns: a `web` check-run killed at the background limit, then idle > 1 h; 3 failed runs (stale menu assert, own probe 404) |
| #173 opus, 5 steps | 2.91 | 10 | $1.74 while the step was `context`, of which $0.79 is its own turn (recall found nothing); 1 rotation, 1 conflict |
| #174 sonnet, build only | 0.33 | 4 | the only build-only task: $0.33 / 4 turns against the $0.54 / 4 turns baseline |

### Did the changes since #169 work?

| change | applied? | verdict |
|---|---|---|
| #173: drop `context` and `memo` from the owner's `steps:` | no: the `wsx.config.yml` edit was left for the owner; #176 still runs context, plan, build, memo | cannot judge; 0 tasks |
| #173: steps end background jobs before the result | yes, in `host.rs` `step_text` | 1 logged `result_retry` turn in 7 days ($0.08), against 35 "no event" turns after the last step in #169's window; keep it |
| #169: sonnet build only for stated changes | 1 of 6 sonnet tasks (#174) | too few (1 of 8 needed) |

### Current practice (web, read 2026-11-04)

| finding | source |
|---|---|
| Headless `-p` turns get a 1 h cache only on a subscription; with an API key they get 5 min (`CLAUDE_CODE_PROMPT_CACHE_TTL`). Each model has its own cache, so a model switch inside a session re-reads uncached | [Claude Code: prompt caching](https://code.claude.com/docs/en/prompt-caching.md) |
| 1 h write 2x, 5 min write 1.25x, read 0.1x base input; on the 5.5 models a read costs 0.05x | [API: prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching) |
| Clearing stale tool results is the "lightest touch" compaction; subagents return 1-2k token summaries; notes outside the window survive a reset | [Effective context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents) (2025-09-29) |
| Context editing cut tokens 84% on a 100-turn eval | [Managing context](https://www.anthropic.com/news/context-management) (2025-10) |
| Sonnet for most coding, Opus for architecture; filter test output with a hook; specific prompts avoid broad scans | [Manage costs](https://code.claude.com/docs/en/costs) |
| Agents use about 4x the tokens of chat, multi-agent about 15x: use subagents only for parallel, high-value work | [Multi-agent research system](https://www.anthropic.com/engineering/built-multi-agent-research-system) (2025-06-13) |

Report gap: `usage-report.py` weighs reads at 0.1x. On the 5.5 models a read costs 0.05x, so the report overstates the read share and understates the write share. The dollar totals come from the logs and are correct.

### Proposals, ranked

| # | change | cause and evidence | expected effect | measure after 8 landed tasks |
|---|---|---|---|---|
| 1 | Owner applies the #173 `steps:` edit: remove `context`; the last step's result carries the memo | the `context` step's own turns are 16% of worker cost since 11-03 ($3.06 of $19.60, 5 turns) and load nothing from a worktree | small: about one first turn's recall work per task, since the first real step then pays the first-turn cache write; one turn less; speed about -1 min; quality = (recall gave nothing) | per-step table: `context` turns = 0; opus 5-step median $5.73 → 4-step |
| 2 | Waiting on a check costs no turn: step text says to start `wsx task check-run` in the background with the longest timeout and never poll; or the host answers the check as a `check_run` turn and the step ends after queueing | queue wait is 20-90 min, longer than the 30 min background default: #167 got 2 restarts, #172 got 7 restarts + 3 rotations, #175 polled for 323 min (est. $0.85); `restart` turns cost $14.67 in 47 turns over 7 days | cost about -$0.3 to -0.9 per task with a check-run; fewer restarts and rotations; quality = | `restart` trigger turns per task (47 in 7 days), polling lines in "Repeated agent commands", $ of turns over 60 min |
| 3 | Before `check-run`, the build step searches the scenario scripts (`scripts/test-*.py`) for every UI string or label the diff renames, and fixes stale asserts | 11 failed runs in #167, #172, #175; at least 3 were stale asserts in the `web` scenario; each failure costs a 20-90 min queue plus a repair turn; 181 min check wait per task | failed check runs 1.6 → under 1 per task; wall time -40 to -80 min per failure avoided; quality + (fewer test-only fixes after the run) | failed check runs and check wait per landed task in the Summary |

Prove each with the loop above: one `decisions.md` line, then compare `--until` with `--since` the change date after 8 landed tasks of that kind.

### Decisions in force (2026-11-03)

- The owner's steps become `plan, build, review`, and the `review` result carries the memo. The `context` step goes with no replacement. This waits on the owner (proposal 1). Baseline: $4.32 per landed task, 35 turns after the last step.
- A stated change runs on sonnet as build only. Opus runs only when the cause is unknown or the change needs design. Baseline: opus 5-step median $5.73 / 7 turns, sonnet $0.54 / 4 turns.
- 2026-11-04 (#182): the lean flow is wsx's own behavior for every project and manager, not an edit to one config. A `context` step folds into the first other step after the host's task context block; a `memo` step folds into the step before it; `task new --size small` runs the build only on `tasks.agent.smallModel` (sonnet). Measure after 8 landed tasks against $2.38 per landed task, 5.5 turns (`usage-report.py --since` the land date): `context` turns = 0, small tasks near $0.33-0.54. The saving from folding `context` is small, about one first turn's recall work: its own turns cost $0.47-0.79, mostly the first-turn cache write, which the first real step now pays.
