# Lean wsx: one task model, tests that run by default, nothing unused

Owner, 2026-11-22: before more manager-mode work, make the product lean and coherent. Evidence came from three read-only
measurements of the code, the logs of the last 30 days and the test runs of this day.

## Why

- A task's state is spread over `record.state`, `record.waiting`, `record.after`, `record.hold`, host flags (`stopped`,
  `turn`, `gate`, `after_rebase`, `check_run`, queues) and log events read back as state. `host.rs` is 4,934 lines and had
  84 commits in two weeks, 42 of them on waits, parking, stops, holds and check-runs. The parked-build path broke three
  times on 2026-11-22.
- 13 contradictions found by reading (`host.rs` line numbers as of 2026-11-22; each gets a scenario before its fix):
  a stop is lost across a host restart (C1); a first prompt can be sent twice after a stop (C2); a stopped, parked task
  never resumes from an add (C3); three predicates read "waits after #m" differently (C4); a restart in Checking drops a
  land without a word (C9); a disk or load wait has no exit after a restart (C8); a manual land ignores `hold` and
  `after` (C10); stale `CheckQueued` events carry no epoch (C11); and five smaller ones (C5-C7, C12, C13).
- Tests: 28,000 lines of Python scenarios and browser checks against 6,700 lines of Rust integration tests. A task
  scenario part takes 7 to 10 min; section 20 (manager start) costs about 112 s in each of three parts. 23 commits since
  2026-10-15 fixed flakes, mostly browser timing.
- The owner uses the TUI and the web; the CLI is run by agents (78,048 agent commands, 4 by hand in 30 days).

## Target

| Measure | Now | Target |
|---|---|---|
| Places that change a task's state | about 20 functions over 12 fields | one `next(phase, event)` in `wsx_core::task::phase` |
| Contradictions C1-C13 | 13 | 0, each with a scenario |
| Default gate | nextest plus scenario parts, 10 min a part | nextest only, under 5 min, as capable as the scenarios |
| Browser checks | in every `web` run | opt-in, one journey per area |
| Code with no use and a replacement | about 800 lines | 0 |

## Steps (each lands green; the default gate is nextest)

1. **Phase model.** `wsx_core::task::phase`: `enum Phase { Parked{stage}, Working{turn}, Checking{queued, land},
   Throttled{on, then}, Plan, Blocked{question}, Paused{why}, Stopped{parked}, Hand, Ready{asks, land_stopped}, Failed{why},
   Landed, Closed }`, persisted in `task.json`; `next(&Phase, &Ctx, Event) -> (Phase, Vec<Effect>)`, pure; `Phase` derives
   the old `TaskState`, `WaitOn` and status kind, so the web, TUI and CLI wire stays. `hold`, `after`, `check_run` and the
   home or away agent stay orthogonal fields. A reader maps old records once. Proof: a table test drives every
   transition of the table below from real stored records through the real host (no mocks), plus one scenario per Cn.
2. **Host on Phase.** `host.rs` runs the effects; `awaits_start`, `build_step`, `build_parked`, `start_parked_build`,
   `start_after_wait`, `resume_state`, `deliver_or_queue`, `stop`, `hand`, `after`, `after_end`, `wait`, `set_state`,
   `move_to`, `state_words`, `require_open` and `TaskState::may_move` go (about 640 lines out, 150 in).
3. **Test harness.** One `Fixture` in `crates/wsx/tests/common` (the three copies in `task_deploy.rs`, `task_sync.rs`
   and `routine_wsxd_lifecycle.rs` fold in), a manager-up helper with no fixed sleeps, a `Site` helper that starts
   `wsx web serve` and speaks REST, SSE and WS, a time-scale knob for limit and hold waits, and the Pi fixture. Then the
   no-screen sections move from `test-task.py` (about 90 %) and `test-web-client.py` (70 to 80 %) into nextest files.
4. **E2E tiers.** `scripts/scenario.py` runs no browser by default. Critical e2e, on changes to its area and before a
   release: live daemon handoff, the Seatbelt boundary, land and deploy, one web journey, one TUI smoke. The rest of the
   Python scenarios and browser checks run by hand.
5. **Deletions with evidence** (0 uses in 30 days or a proven replacement): `task preview` and its host and
   `tasks.preview`; `.gtrconfig` and the old `config.toml` imports; `memory recall|update|reindex`; `task widths`; the
   `handback` alias; `manager-host` and `run_old` once no saved session names them; the old web `#/p/...` routes and
   the second asset route. Merges: `wsx routine` and `wsx task routine` into one; the three ways to add a project into
   `ops::add_project`. To ask the owner first (no evidence either way): dev analysis scripts, the `wsx agent
   request|start|...` CLI, the TUI routine editor and integration manager.

## Transitions (the table the tests drive)

| Phase | Event | Next |
|---|---|---|
| Parked | AfterEnd | Working (build or first turn, adds read once from the log) |
| Parked | Add | Parked (the add waits in the log) |
| Parked, any open but Hand | Stop | Stopped{parked} |
| Stopped{parked} | Add | Parked when `after` is set, else Working |
| Stopped | AfterEnd, Restart | Stopped |
| Working | result: step done / last step / plan / blocked / paused | Working or Parked{Planned} / Checking / Plan / Blocked / Paused |
| Working | limit, disk, load | Throttled{then: the turn} |
| Throttled | WaitEnded / Restart | `then` / Throttled (timer re-armed) |
| Checking | pass / fail with retries / fail / Restart | Ready or Landed / Working / Failed / Checking with the same land |
| Ready | Land | Landed, Checking{land}, Working{conflict} or Ready{land_stopped}; the one hold and after guard |
| Ready, Blocked, Paused, Plan, Failed | Add / Hand | Working / Hand |
| Hand | Handback | Checking |
| any open | Close | Closed |

Manager mode (`manager-mode.md`) resumes after step 2: approved runs, the boundary and chat joins are on main already.
