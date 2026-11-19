# The machine's scheduler

One owner decides when a heavy run starts on this Mac: `wsx_core::task::schedule` (plain rules over plain
facts, unit tested) and `crates/wsx/src/task/slot.rs` (the `flock` files and tickets that hold the runs).
Heavy runs are: a task's checks (lane `checks`, one at a time), a scenario round (`task`, `web`; lane
`scenarios`, several at once), and the deploy's smoke test and the release handoff (`wsx task slot <cmd>`,
which replaced `scripts/check-slot.py`). Owner words, wsx 5441 and 5461: watch the disk, schedule well,
go to the slow external CI only when there is nothing else.

## Flow

```mermaid
flowchart TD
  A[run wants the scenarios lane] --> S[sample: load per core, free RAM, free disk]
  S --> D{decide}
  D -->|first run: load under 2.5/core, or waited 15 min| G[Go: take the slot, run here]
  D -->|beside others: load under 1.5/core, RAM and disk for one more, newest run 90 s old| G
  D -->|none of these| W[Wait: ticket in the queue, reason says why]
  W --> D
  D -->|urgent run, local wait longer than a CI round| C[Ci: reason in status]
  G --> X{scenario.py}
  X -->|tool missing: exit 69| C
  X -->|ran| R[result]
  C --> R
```

States of a waiting run: `queued for the <lane> slot` (record `waiting.on = check_slot`) until it takes the
slot, then `checking`. A run on CI shows `Running its scenario tests on CI` (kind `ci_scenario`) until the run
ends. Both end when the run answers.

## Rules

| Rule | Where |
|---|---|
| Sample: 1-minute load per core (`getloadavg`), free RAM (macOS `host_statistics64`, Linux `/proc/meminfo`), free disk of the working copy's volume | `schedule::sample` |
| Width: at most `MAX_SCENARIO_RUNS` (3); `WSX_SCENARIO_SLOTS` lowers it. A first run needs load under `START_LOAD` 2.5/core, or has waited `LOAD_WAIT` 15 min. Each extra run needs load under `WIDEN_LOAD` 1.5/core, `RUN_RAM_MB` and `RUN_DISK_MB` (3 GiB each) free, and the newest run older than `SETTLE` 90 s (the load average lags) | `schedule::decide` |
| Order: `Land` (a land's checks), `Urgent` (another open task waits for this one through `after`), `Asked` (the owner made the task), `Normal`; then arrival. A ticket's priority is fixed when it joins | `schedule::urgency`, `slot::ticket_name` |
| CI: only an urgent run, and only when its estimated local wait (rounds ahead times the median recorded `ms` of that command, else 15 min) is longer than a CI round (25 min, p50 of 40 runs) | `schedule::decide` |
| CI: any scenario-only run when free disk is under `tasks.minFreeGb` after the cleanup (`build::reclaim`); a run with plain checks is refused as before | `host.rs` `start_check_run` |
| CI: the scenario exits 69 (`cannot run here: ...`: cargo, git, node or Chrome missing) | `scripts/scenario.py`, `host.rs` `spawn_checks` |
| CI run: `WSX_SCENARIO_ON=ci` makes `scenario.py` run `scripts/ci-scenario.py`: push HEAD with main's workflows to `wsx-scenario/<name>/<time>-<commit>[--<part>]`, `ci.yml` runs the parts (`scenario.py --matrix` lists them), wait up to 70 min, print the failed section lines, fetch the captures, delete the branch | `scripts/ci-scenario.py`, `.github/workflows/ci.yml` |

The status and the log say why: `queued for the scenario slot behind <holder>; <note>` while it waits, and
`went to CI: <reason>` for a CI run (`task::status::slot`).

## Not done

- The scheduler does not delay an agent turn under load: the host cannot throttle a turn that runs.
- The constants are first guesses. At 10 cores and 24 GiB with agents building, load sits near 18, so the
  lane stays at width 1 until load drops; `docs/design/check-queue-research.md` rank 7 (N slots) was
  rejected because timed tests fail under load, so the width follows load, not a fixed N.

## Measured

Source: the host's check-run on head 259a89ed (`check-run-8.log`, `check-run-9.log` of task #522), 10 cores,
24 GiB, other tasks building at the same time.

- `task`: 7 parts as 7 processes at once, 653 s for the slowest (`core`); `web`: `chat` 431 s, `tasks` 675 s.
  The `wsxd watch` samples in the web log show load 35 to 60 during the round.
- Free disk of the working copy's volume: 16 GiB at the end (`df`); it was about 11 GiB at the start of the task.
- Section `32v2` (disk low, scenario-only run goes to CI with `went to CI: ... GB free of the 100000 GB`) passed in part `deploy`.
- Not measured: two tasks' rounds at once at width 1 against a wider lane. Load stayed over 1.5 per core on
  this Mac all through the task, so the rule kept the lane at width 1 and a wider lane never ran here.
  The failure counts at width 1 against width 2 or 3 are open.
- The Mac has 511 PTYs (`sysctl kern.tty.ptmx_max`). While other tasks' fixtures held nearly all of them
  (`ls /dev | rg -c '^ttys'` gave 476 to 515), every PTY test of this task failed with `failed to openpty`.
  The scheduler does not count PTYs; a rerun after they are free passes.
