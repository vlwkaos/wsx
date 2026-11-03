# Check queue: measured wait, evidence, recommendation

Research of 2026-11-04. Question: why does a 1.4 min check wait up to 75 min, and what fix has measured support.
Contract: `docs/tasks.md` "Check slot". Code: `crates/wsx/src/task/slot.rs`.

## Local measurement (proven)

Source: every `checks` and `check_run` event with `queued_ms` in `~/.local/state/wsx/tasks/*/*/log.jsonl`.
`queued_ms` exists since 10-29, so the window is 10-29 20:37 to 11-04 08:32 (153 runs, all wsx).
Command: `python3 -I .work/queue.py` (the script is not committed; `.work/` is git-ignored).

| Run class | n | queued p50 | queued p90 | queued max | ran p50 | ran p90 | failed |
|---|---|---|---|---|---|---|---|
| plain (fmt, build, nextest) | 57 | 2.1 m | 45.7 m | 75.5 m | 1.4 m | 1.8 m | 2 |
| plain, checks after a result | 46 | 2.2 m | 35.6 m | 75.5 m | 1.4 m | 1.8 m | 1 |
| scenario, all | 96 | 1.8 m | 49.7 m | 87.2 m | 16.1 m | 18.0 m | 46 |
| scenario `web` | 78 | 0 s | 42.4 m | 87.2 m | 16.3 m | 18.0 m | 37 |
| scenario `task` | 11 | 41.5 m | 54.9 m | 77.6 m | 16.2 m | 17.8 m | 8 |
| scenario `smoke` | 7 | 10.7 m | 13.9 m | 14.4 m | 3.5 m | 4.3 m | 1 |

What the slot was busy with during each wait (summed over waiters, 1 s grid, 2209.5 waiter-minutes):

| Holder | Waiter-minutes | Share |
|---|---|---|
| scenario `web` | 1383.9 | 63% |
| scenario `task` | 556.2 | 25% |
| deploy (its gate runs `runtime-smoke.py` in the slot) | 173.5 | 8% |
| plain checks | 74.3 | 3% |
| none found | 7.3 | 0% |

Findings:
1. 88% of all queue time is spent behind scenarios. Only 3% is spent behind plain checks.
2. A plain run's p90 wait (45.7 m) is 25 times its p90 run (1.8 m). #177's 46 min wait matches this.
3. The `task` scenario ran 11 times on this machine in the window and failed 8 times, although `docs/tasks.md` moves it to GitHub Actions.
4. 37 of 78 `web` runs failed. Only 5 of 38 failure outputs contain "timeout" or "timed out" (`python3 -I .work/webfail.py`), so most failures are not wait timeouts. Each failed run still costs a full 16 min of slot time.
5. Load at `uptime` during this research: 14.90 / 12.91 / 11.71 on 10 cores (`sysctl -n hw.ncpu` = 10, 24 GiB). Agents' own cargo builds run outside the slot.

## External evidence

Status column: "draft" means the number comes from the earlier draft (`/Users/vlwkaos/ws/wsx/docs/design/verification-pipeline.md`, not in this repo). "Not re-fetched" means the source agent was stopped before it confirmed the page, so the number needs a check before anyone quotes it.

| # | Claim | Source | Measured | Number | Status | Transfer to 1 machine, 2-5 agents |
|---|---|---|---|---|---|---|
| E1 | Resource limits cause flakiness | Parry, Silva et al., "The Effects of Computational Resources on Flaky Tests", TSE 2024, https://arxiv.org/abs/2310.12132 | 52 projects, tests rerun with limited CPU and memory | about 46% of flaky tests are resource-affected | not re-fetched | Strong: a timed scenario beside parallel cargo and Chrome is this setup |
| E2 | Async wait is the top flaky cause | Luo et al., "An Empirical Analysis of Flaky Tests", FSE 2014, https://doi.org/10.1145/2635868.2635920 | 201 fixed flaky tests in Apache projects | async wait 45%, concurrency 20% | not re-fetched | Medium: the web scenario waits on browser and PTY events |
| E3 | UI tests are mostly async flaky | Romano et al., "An Empirical Analysis of UI-based Flaky Tests", ICSE 2021, https://arxiv.org/abs/2103.02669 | 235 flaky UI tests | async wait about 45% | not re-fetched | Medium: headless Chrome scenario |
| E4 | Flakiness rate at scale | Google Testing Blog, "Flaky Tests at Google and How We Mitigate Them", 2016, https://testing.googleblog.com/2016/05/flaky-tests-at-google-and-how-we.html | all TAP test runs | 1.5% of runs flaky, 16% of tests have some flakiness | draft; industry, method in post | Weak transfer: a rerun policy, not a load cause |
| E5 | Selection keeps detection | Machalica et al., "Predictive Test Selection", ICSE-SEIP 2019, https://arxiv.org/abs/1810.05286 | Meta change history | over 99.9% of faulty changes caught at about a third to half of test cost | draft | Weak: it needs a large run history. Path-based selection is the small form |
| E6 | Presubmit plus postsubmit | Memon et al., "Taming Google-Scale Continuous Testing", ICSE-SEIP 2017, https://research.google/pubs/taming-google-scale-continuous-testing/ | TAP history | few tests ever fail; a full suite after submit with culprit finding keeps main healthy | not re-fetched | Medium: a nightly run of all scenarios on main is the 1-machine form |
| E7 | Queue prioritization | Uber, "CI at Scale: Lean, Green, and Fast", https://arxiv.org/abs/2501.03440 | SubmitQueue at Uber | 53% less CI use, 37% shorter P95 wait | draft | Weak: thousands of changes a day. The idea "short work does not wait behind long work" transfers |
| E8 | sccache skips incremental Rust | sccache README, https://github.com/mozilla/sccache#rust | documented limitation, no benchmark | crates built incrementally (the workspace's own crates in debug) are not cached | not re-fetched; vendor doc | Low gain: the workspace crates are what changes per task |

No source found in this run gives a measured number for a shared `CARGO_TARGET_DIR` across concurrent worktrees. Cargo takes a lock on the build directory, so concurrent builds into one target directory wait for each other ("Blocking waiting for file lock"). That is a documented behaviour, not a measured gain.

## Ranked recommendation

| Rank | Change | Backed by | Expected gain | Effort | Risk |
|---|---|---|---|---|---|
| 1 | Plain checks get their own queue; scenarios keep the one slot | Local: 88% of wait is behind scenarios, 3% behind plain | Plain wait falls from p90 45.7 m toward the plain plus deploy share (about 11% of today's wait) | small (a second `Ticket` queue name in `slot.rs`) | Medium: nextest beside Chrome adds load; E1 says load makes timed tests fail. Limit: at most 1 plain run beside 1 scenario |
| 2 | Stop local runs of the `task` scenario: refuse it in `check-run`, or warn | Local: 11 runs, 8 failed, 556 waiter-min (25%) | Up to 25% of the queue time | tiny | Low: GitHub Actions already runs it on task paths |
| 3 | Select scenarios from the diff's paths (`tasks.scenarios.<name>.paths`) | E5, E6 for the method; local: `web` 78 runs at 16 m each | Removes runs that touch no web path; the size is unknown until paths are set | small | Low: a missed path is caught by rank 4 |
| 4 | Nightly run of all scenarios on main, failures start a fix task | E6 | Lets rank 3 be strict; no wait on the critical path | small (a routine) | Low |
| 5 | Split `web` into parts, each a scenario with its own paths | Local: `web` holds 63% of wait time | Multiplies rank 3 | medium | Medium: parts share setup; the total time can grow |
| 6 | Rerun a failed scenario part once, then record it as flaky | E1, E2, E4 | Fewer false blocks; local data shows only 5 of 38 failures mention timeouts, so the gain is small now | small | Medium: a rerun hides real races |
| 7 | N general slots (N > 1) | E1 against it | Short waits for all | tiny | High: 11-03 failures at load 13 happened with 1 slot already |
| 8 | sccache or a shared target dir | E8; no measurement found | Unknown, likely small for workspace crates | medium | Medium: lock waits, disk |

Next step: confirm E1, E2, E3 and E6 numbers on their pages before the owner decides on rank 1.
