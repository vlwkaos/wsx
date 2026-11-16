# A finished check-run that nobody hears about

Read of 2026-11-16 for the failsafe map. Source: code at `789eecab`. No live run: the task store of #472 is outside this boundary, so which of the two paths below it took is not verified. The failsafe map itself is not in this working copy; the rank below is against `root-cause-refactor.md` causes A and B.

## Where a host keeps the run

`Host.check_run: Option<CheckRun>` (`crates/wsx/src/task/host.rs:318`), set in `start_check_run` (`:3059`). It holds the call's reply channels, `queued`, `result_waits`, `deadline_ms` and an early `answer`. The `ci:` command runs in a thread (`spawn_checks`, `:3199`, `run_ci`, `:4523`) that sends `Event::CheckRunDone` to the host loop. Nothing of this is on disk except the log file of the check's output and, once the held result shows it, `waiting: check_run` in `task.json` (`show_held_result`, `:3379`).

## Why the answer is lost

| Path | What the code does | What the worker sees |
|---|---|---|
| A. `wsx task stop`, run ends later | `stop` sets `stopped`, drops `result_waits`, keeps the run (`:1297`). `check_run_done` takes the `Some(_) if self.stopped` branch (`:733`): the answer goes into `self.notices`, a `check_run` entry is logged, no turn starts. Documented and tested (`docs/tasks.md` Stop, `test-task.py` 32t1). The wait is `owner` "stopped by request", which `state_wakes` skips (`crates/wsx-core/src/task/manager.rs:323`) and `stall.rs` has no cause for. | Nothing, until the next `add`. No event tells the manager the run ended. |
| B. The host process restarts while the run is out | `check_run`, `notices`, `stored_checks` and the `ci:` thread live in memory. `resume_state` (`:845`) has no arm for a pending run: a task in `working` (also with `waiting: check_run`) becomes `owner` "the host restarted during a turn" (`:865`). | Nothing. A later `add` makes the worker run `check-run` again: `stored_checks` is gone and `ci-scenario.py` pushes a new branch and commit (`scripts/ci-scenario.py:352`), so CI runs again (20 min or more). A restart after path A also loses the held `notices`. |

The `checks` after a result survive a restart (`:857`, they run again). The worker's own check-run has no such arm.

## Rank

| Rank | Failure | Cost | Same family |
|---|---|---|---|
| 1 | A stopped or restarted task with a finished or lost check-run: no event, no turn | idle for as long as nobody looks (1 h 12 min in #472), and a full CI run again on path B | cause A (no event from a silent state) and cause B (the wait says `owner`, the task owes the worker an answer) |

Place it next to the dead-turn stalls: same effect (a task idle with no wake), different cause (a recorded wait with no producer).

## Smallest fixes (not built)

1. Path A: when `check_run_done` ends while `stopped`, write one log entry the wake rule reads (the `check_run` entry is already there) and let `state_wakes` count a `check_run` entry newer than the stop as a wake for a `STOPPED_BY_REQUEST` wait. The manager then decides to `add`.
2. Path B: write `check-run.json` (label, indexes, commit, `ci`) when the run starts, remove it when the answer is sent. `resume_state` that finds it starts the same run again, as it does for `Checking` (`:857`), so the answer reaches the worker as a turn.
3. Proof for both: a 32t1 variant that kills the host (as `test-task.py` does near line 7384) before the gate opens.

## What #472 (M2) changes

#472 had not landed on main when this was written (main `d0aefcad`), so the rows were re-read on branch `wsx/t472` (`git diff main...wsx/t472`). Re-read them against main once it lands.

| Row above | M2 | Effect |
|---|---|---|
| Where a host keeps the run, path A, path B (`host.rs`, `stall.rs`, `status.rs`) | not touched | unchanged |
| Duties: who reads `state_wakes` (path A, fix 1) | the store watch, stalls, approvals and session news move from the home chat's host to the repo service (`crates/wsx/src/task/repo.rs:1202`, `:1353`); `seen` and `stalls` move to `RepoState` (`repo.json`). `state_wakes` and its `STOPPED_BY_REQUEST` skip stay in `wsx-core/src/task/manager.rs:317-323` | fix 1 is the same one-rule change in core. The service is always on, so the wake starts the home chat's host if it has left |
| Host start (path B) | the task host's `resume_state` is unchanged. Chat hosts and the repo service start by `say` or an event, and take no check-run | path B is unchanged. A repo service restart does not help: it reads task states, and the lost run leaves none |
| Chat hosts | only talk, with no check-run | none |
