# Orchestration engine: one typed state machine, measured by a suite

Status: proposal 2026-11-27; owner decisions 1 and 2 taken the same day (section 7), step 1 partly built, step 2 started. Made with `/agentic-dev-guide` (its Optimization evaluation). It builds
[orchestration.md](orchestration.md) step 3 ("one phase table; no event turns") and changes the contract in
[tasks.md](../tasks.md) only where section 3 says so.

## 1. Evidence

All paid desk arms of `scripts/eval-desk.py`, 2026-11-25 to 11-26 (`.work/eval/*/results.jsonl`):

| Outcome of a desk arm | Arms | Kind |
|---|---|---|
| waits for the owner, nothing landed | 12 | lifecycle: work stopped with no one to continue it |
| time limit, nothing landed | 4 | lifecycle or speed |
| done, nothing landed | 6 | all under 2 min, 5 in the first run: the lead answered and started no work |
| done, landed, hidden test fails | 2 | quality |
| pass | 2 | |

- Desk: 2 of 26 passed for $54.36. Solo (one `claude -p`): 2 of 3 passed.
- The largest mode is not code quality. A unit of work stops in a state that names no one who will move it on.
- The same concern has had three patches in two days, each on another reader of the state: the wake on `blocked`
  (34bcd6a8), the pass that continues a `paused` worker (7d0aa35d), and the eval check for `check-run.json`
  (0cc307c5). The record has no field for "the worker waits for its own job", so `paused` reads as "waiting for you".

## 2. Goal and decision rule

- Goal: the desk passes the suite at least as often as solo, for no more cost per success, and leaves no orphan.
- Orphan: an open unit with no running turn, no running or queued job, no timed limit and no open owner item.
- Adopt a change when, on the same suite, the paired pass difference against the last accepted engine is 0 or more,
  cost per success is not worse, and the orphan count is 0. An arm that stops early is a failure.

## 3. The engine

```
event ──> next(phase, wait, event) ──> (phase, wait, effects)      pure, wsx_core::task::phase
                                            │
host executes effects: StartTurn{trigger, text}  RunJob{lane}  PostItem{inbox}  Land  Notify
```

**Turn end is typed.** A worker's result is one of:

| Turn end | Then | Replaces |
|---|---|---|
| `done{evidence}` | the host's checks decide ready | `done` |
| `needs_owner{question, basis, options}` | one inbox item; the owning chat may answer first | `blocked`, and `paused` with a question |
| `waits_on` (the host names the job: its one check-run out) | the job's answer is the next turn (a pass with `done` is evidence) | `paused` while its own run is out |
| `failed{reason}` | terminal | `failed` |

The schema refuses a pause with no question and no job. The worker never waits on a long job in its turn: a
`check-run` longer than the agent's command limit returns `waits_on{job}` at once.

**Wait is data.** `wait = {on: owner(item) | job(id) | slot(lane) | limit(account, until) | peer(task) | disk, since}`.
Every screen, the lead and the eval read this field. No reader infers a wait from prose or files.

**One invariant, checked by code each tick: no orphan.** An orphan gets one wake of the chat that owns it (bounded,
as `CHAT_ANSWERS` now), then an owner item. This one rule replaces the wake on `blocked`, the `paused` resume and
the eval reader check. It is OTP supervision: every process has a supervisor with a restart budget.

**Model calls.** Lead: route only (rules, then a small model). Goal chat: plan, write a worker's brief, read proof,
answer `needs_owner` when it can. Worker: build. No turn starts for an event except the orphan wake. Each unit has a
turn bound; a bound reached is `needs_owner`.

| Today | Proposed |
|---|---|
| `TaskState` 7, `WaitOn` 5, result status 4, wake and resume rules spread in `host.rs` (5,250 lines) | phase 5, wait 6, turn end 4, `next()` in core, effects run by the host |
| a stall noticed by a person or an eval | orphan detected by code, with its first divergent event |

## 4. Who judges what

| Judge | Facts |
|---|---|
| Code | hidden tests pass; landed diff; orphan count; phase times; queue time; turn triggers; cost per call; owner items |
| Model grader (validated on 20 hand labels, kappa 0.7 or more) | failure mode of a failed trial (MAST modes plus harness); was an owner question needed; is a brief enough |
| Owner | the inbox and chat screens |

New log entries, so code can judge: `phase{from, to, event}`, `wait{set, cleared}`, `orphan{unit, since, event}`,
and the turn end with its type. The eval reads these, not `task show` text.

## 5. Evaluation plan

1. **Zero-cost gates first.** The fake-agent scenarios, plus one `test-task.py` section that drives the real host
   through every turn end × every wait (job running, queued, ended during the turn, stop, restart) and asserts no
   orphan and the right next owner.
2. **Suite.** 15 cases mined by the rule in orchestration.md "Evaluation": the 3 now plus 12. 5 are a frozen holdout.
3. **Arms and trials.** Solo and desk, 2 trials per case and arm, serial at one load. Train round: 10 cases × 2 arms ×
   2 trials = 40 arms, about $140 [estimate from $3.5 per arm so far]. 20 paired trials detect only a large effect
   (about 30 points); this round gives the direction and the failure Pareto, not a rate.
4. **Labels.** Code labels orphan, limit, refusal and no diff; the model grader labels the rest. Rank by count × cost.
5. **One fix per round** at the owner of the top mode, then the same suite again.
6. **Prompt optimization last.** GEPA `optimize_anything` on the worker instructions (`claude.rs`) and the brief
   template, with the suite as evaluator, after the orphan count is 0; acceptance on the holdout.
7. **Regression.** Cases that pass graduate to a nightly regression run on main.

## 6. Steps

| # | Step | Removes |
|---|---|---|
| 1 | Typed turn end, `waits_on`, the no-orphan invariant and its log entries. Built: `waits_on`, `orphan` for every wait the host ends (`WaitOn::by_host`: check-run, disk, load, limit); open: `phase` and `wait` entries | the `paused` resume and the eval's `check-run.json` check (both removed) |
| 2 | `next(phase, wait, event)` in core; the host only executes effects. Built: `wsx_core::task::phase::answer_to`, where a check-run's answer goes (one rule for the run's end and the result after it); and `phase::settle_to`, where a settled result goes (the owner chose it 2026-11-27 though it adds 52 lines: core 57, host −5; its outcomes have scenarios in test-task.py 32u, `task_lifecycle.rs` and `task_review_items.rs`); open: the waits and the resume. Measured the same day: the moves already go through one table (`TaskState::may_move` in `TaskRecord::move_to`); the 35 calls in `host.rs` (`self.(set_state\|move_to\|wait)(`) are one per event, not copies | about 20 state functions in `host.rs` (orchestration.md step 3); built: the four places that routed an answer |
| 3 | Eval: 15-case suite, serial, code labeller on the new log entries | reading logs by hand |
| 4 | Paid round 1, Pareto, one fix, round 2 | |
| 5 | GEPA on instructions and briefs | |

## 7. Owner decisions

1. Decided 2026-11-27: `paused` leaves the result schema. Built: `waits_on`, wait `check_run`, the orphan entry.
   `needs_owner` keeps its stored name `blocked` until the inbox (orchestration.md step 5) gives it `basis` and
   `options`.
2. Decided 2026-11-27: an orphan wakes the chat that owns it, then stays an owner wait. Built on the existing bound
   (`status::CHAT_ANSWERS`, 2 stops per task, blocked and orphan together).
3. Open: budget for round 1 (about $140) and round 2.
