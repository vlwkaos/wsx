# Scenario feedback: where a `ci:` run's time goes, and the ranked fixes

Research of 2026-11-07, task 298. Builds on `check-queue-research.md` (local slot) and `check-scheduling-audit.md` (fixes A parts, B main baseline, C same-tree retry). Code: `scripts/ci-scenario.py`, `.github/workflows/ci.yml`.

## Answer

1. **Keep #293 (parts as parallel jobs) and do three things around it.** First fix the web cache: the `web` job never restores one (30 of 30 runs "No cache found"), so each run builds cold, 156 s against 51 s for `task` (M1).
2. **The `task` scenario is the slowest and fails last**: p50 29 min, 11 of 13 failures after 1300 s (M4). #293 does not touch it (M5).
3. **Linux is untested for `web` and could free the 5-job macOS cap** (docs: Free plan, 5 concurrent macOS jobs). Nothing in `test-web-client.py` needs macOS; only layout asserts may depend on fonts. Run a probe before any claim (M6).
4. Left out: larger runners, self-hosted Mac, local-first mode, test impact, quarantine, binary sharing (see Leave out).

## Measurements

Window: 110 `wsx-scenario/` runs from 2026-11-06 21:24 to 11-07 09:01 UTC, listed by `gh run list --limit 300 --json databaseId,conclusion,createdAt,updatedAt,headBranch`, jobs by `gh run view <id> --json jobs`, logs by `gh run view <id> --log`. Web ids 42296537824-42357901205 (68 runs), task ids 42296489077-42357301629 (42 runs). Steps: last 20 web (42343508545-42357901205), last 20 task (42329059789-42357301629). Logs: last 30 web (42336363800-42357901205) and the same 20 task.

| # | Measure | Number |
|---|---|---|
| M1 | Stage p50, web job (20 runs): start wait / set up / checkout / Rust / Zig / cache restore | 3 / 5 / 3 / 9 / 10 / 1 s; restore is a miss |
| M1 | Same, task job (20 runs) | 3 / 4 / 4 / 9 / 17 / 8 s; restore hits 20 of 20 |
| M1 | `cargo build` in the step (30 web, 20 task logs, "Finished `dev`") | web p50 156 s (103-214), task 51 s (36-69) |
| M1 | Post steps and upload | Zig cache save 13 s p50 (max 34); pack plus upload 4 s |
| M2 | A passing web run's step, p50 of 9 | 1530 s: build 156, chat part 488-512, tasks part 858-898 (6 passing logs: 42342117469, 42346787057, 42348672202, 42349685326, 42350350638, 42352379015) |
| M3 | Web outcome (30 logs) | 21 failed; step p50 753 s (min 56, max 1371) against 1530 s passing; failed runs hold 263 of 491 step-min |
| M3 | Where web fails (traceback, 21) | tasks journey 13, chat journey 7, other 1 (42340355963, 56 s). A chat failure ends the run before the tasks half starts |
| M4 | Task outcome (20 logs) | 13 failed; p50 1663 s failed, 1760 s passing; 11 of 13 failed after 1300 s, 2 early (101, 187 s) |
| M5 | Task parts (run 42357301629, 1712 s) | part 27 341 s, 21a 266 s, 32af 182 s, 32d 93 s: top three are 46%. `test-task.py` has no `--part` (`main()` takes `--wsx`, `--keep`) |
| M6 | Repeated final assert, web failures | 13 of 21 repeat in 2-5 runs: "design lint home" 5, "mobile audit inbox-remove" 4 (same `contentShare 0.904`), "design lint machine" 2, "long chat render CPU 4x" 2. Task: `max() iterable argument is empty` 4 of 13, `'int' object is not callable` 2 |
| M7 | Cache store | `gh api repos/vlwkaos/wsx/actions/cache/usage`: 72 caches, 10.6 GB (the repo limit is 10 GB per the caching docs, not re-fetched); 69 sit on `wsx-scenario/*` refs (128 MB web, 149 MB task each), 3 on main. No other run can read a branch's cache |
| M8 | Load | 2028 job-min in 11.6 h (web 1044, task 984): about 2.9 macOS jobs at once, cap 5. Failed runs: web 724 of 1044 job-min, task 488 of 984 |

Limits: tasks of runs are not recoverable (branches are deleted after a land; only `wsx/t264` 9 and `wsx/t293` 1 of 112 titled commits resolve), so a repeated assert may be one task iterating, a flake or a break on main. The same `contentShare 0.904` in 4 runs points to a fixed break, not a flake; the CPU-4x rows differ (11 ms and 8 ms) and point to a timing flake.

## Research

| Topic | Finding | Source |
|---|---|---|
| Cache scope | A run restores caches of its own branch and the default branch only. A `wsx-scenario/web/<ts>` branch is new each run, so its cache is never read. `rust-cache` skips saving on a failed job by default (post step saved 0 s in failed runs) | [GitHub dependency caching](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching), [rust-cache `save-if`](https://github.com/Swatinem/rust-cache) |
| Runners, price | Public repos: standard runners free, also arm64. Linux 4 vCPU 16 GB; macOS arm64 3 vCPU (M1) 7 GB. Larger runners bill even in public repos; one source lists macOS 5-core arm64 at $0.102/min, sources conflict | [runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners), [pricing](https://docs.github.com/en/billing/reference/actions-runner-pricing), [arm64 GA](https://github.blog/changelog/2025-08-07-arm64-hosted-runners-for-public-repositories-are-now-generally-available/) |
| Concurrency | Free plan: 20 jobs, 5 macOS | [usage limits](https://docs.github.com/en/actions/reference/limits) |
| sccache or rust-cache | No controlled benchmark; anecdotes only (wezterm 56 to 45 min). sccache skips incremental crates (`check-queue-research.md` E8) | [wezterm PR 4468](https://github.com/wez/wezterm/pull/4468) |
| All failures in one run | Matrix `fail-fast: false`; Playwright shards report separately and merge; retries hide a red build unless flaky runs are reported | [matrix strategy](https://runs-on.com/github-actions/the-matrix-strategy/), [Playwright retries](https://testdino.com/blog/playwright-retries) |
| Self-hosted, public repo | GitHub advises against it; fork pull requests are the path to abuse; push-only triggers keep fork code out | [community 26722](https://github.com/orgs/community/discussions/26722), [OWASP](https://cheatsheetseries.owasp.org/cheatsheets/GitHub_Actions_Security_Cheat_Sheet.html) |

What needs macOS: `ci.yml:89` says Seatbelt (task part 23). `test-web-client.py` has `darwin` branches that already handle Linux (lines 939, 1013, 3453); the "LaunchAgent" part (4584) only sets an environment. `ci.yml:30` already builds, tests and runs `runtime-smoke.py` on `ubuntu-24.04`.

## Options

Gain per round, from M1-M8. "Every project": holds without wsx-specific code.

| Option | Time per round | Cost | Risk | Every project |
|---|---|---|---|---|
| O1 Web cache from main, restore-only on scenario branches | web build 156 to about 51 s: -105 s (7% of a pass, 14% of a median failed step); Zig save -13 s; stops 69 dead caches (about 10 GB of the 10.6 GB store) | 0 money; one main-side job keeps the cache | main updates only at a release, so the cache ages; `ci.yml` on main reaches CI only with a release | yes, the rule "a job that runs only on scenario branches never has a cache" |
| O2 #293 parts (given) | pass 1526 s to about 1054 s cold (28 + 156 + 870), about 950 s with O1: -38%. A failed round can last longer (753 s median to up to 950 s) but shows both halves: 7 of 21 failures hid the tasks half | 2 more macOS jobs of 5; per job 28 s setup plus a build | cap queueing at peak (M8: 2.9 average) | yes, parts are named by the project |
| O3 Web on Linux | unknown; probe decides. Frees macOS jobs; free | one probe job | font-dependent layout asserts (`contentShare`, lint) | partly: a project says which OS its scenario needs |
| O4 Task scenario parts | `task` pass 1760 s: 3 shards of about 600 s would give -60% if parts are independent (unproven) | `--only` in `test-task.py`, matrix | shared fixtures between parts | yes |
| O5 Collect all failures in one part | web asserts after the first failure never run; saves rounds, not minutes | `fail()` in `web-tasks-check.mjs` (3704 lines, one stateful page) records and goes on | cascade noise after the first failure | partly |
| O6 Main baseline, same-tree retry (audit B, C) | M6: 13 of 21 web and 6 of 13 task failures repeat an assert | host code | as audit | yes |

## Ranked plan

1. **Cache (O1), one task.** Files: `.github/workflows/ci.yml`, `docs/tasks.md` "Scenarios". Share one rust-cache `shared-key` between `task-scenario` and `web-scenario`; a main job (push to main) builds `cargo build --locked -p wsx -p wsx-daemon` with save; scenario-branch jobs use `save-if: false` and `setup-zig` without cache save. Proof: a `check-run web` log shows "Cache restored", `Finished` under 70 s, and `gh api .../cache/usage` stops growing by 130 MB a run. Takes effect after a release (`ci-scenario.py` runs main's `ci.yml`).
2. **Parts, finish #293 (O2).** Add `fail-fast: false`, so one run reports every part; `ci-scenario.py` prints each failed part and exits non-zero once. Proof: a staged tree failing in chat and in tasks reports both in one run; ids and times are compared to M2.
3. **Web on Linux probe (O3), one task.** Files: `ci.yml` (a job on `ubuntu-24.04` for `wsx-scenario/weblinux/`; `apt-get install poppler-utils ffmpeg`), no change to `web-scenario`. Proof: 3 runs of one tree on both OSes: pass or fail, the failing asserts, step time. Decision rule: move `web` to Linux if all parts pass 3 of 3 with a step 30% shorter; if asserts differ only by font, set per-OS thresholds in the scripts.
4. **Task parts (O4), after 1-3.** Files: `scripts/test-task.py` (`--only 21,21a,27`; print each part's duration), `ci.yml`, `wsx.config.yml`. First step: list which parts share state; shard only independent ones. Proof: shards pass 3 of 3 and the sum of shards equals the whole run's asserts.

After these, audit fixes B and C keep their place; M6 shows they are needed.

## Leave out

- **Larger runners**: paid in a public repo (about $0.10/min macOS, conflicting sources) and the run waits on timers, not only CPU (CPU-4x rows take 27 s).
- **Self-hosted Mac**: GitHub advises against it for public repos; the Mac ran at load 14.9 on 10 cores (`check-queue-research.md`) and local queue p50 was 29 min (audit Q3).
- **Local-first mode**: workers have no Chrome inside the boundary, so a local run takes the one slot again.
- **Shared binaries between part jobs**: saves under 51 s a job and adds an upload and a download.
- **Test impact, quarantine, sccache**: the web journey is one stateful script; a quarantine hides breaks; sccache has no benchmark for this workspace.
- **Poll interval 30 s to 10 s** in `ci-scenario.py`: at most 20 s a round; do it with item 2.

## Round time, 2026-11-18 (task 517)

Data: the last 20 task and 20 web runs on GitHub (`python3 -B docs/design/scenario-time-measure.py fetch|report`; it reads `gh` only). The runs used origin/main's workflows, so web chat ran on macOS too.

### Where a run's time went (CI, seconds, p50)

| Part (all macOS) | Wait for a slot p50 / max | Build | Scenario | Job total p50 |
|---|---|---|---|---|
| task core | 44 / 1729 | 134 | 868 | 1160 |
| task worker | 210 / 1389 | 132 | 971 | 1261 |
| task host | 94 / 2042 | 143 | 1420 | 1682 |
| web chat | 285 / 1207 | 144 | 687 | 855 |
| web tasks | 61 / 1030 | 139 | 1183 | 1364 |

31 of 49 task jobs waited over 60 s for one of the 5 macOS slots. A round took p50 29.5 min (task) and 23 min (web). The build costs about 135 s a job, in every part.

### The 15 slowest sections (p50 s) and what each waits on

| s | Part: section | Waits on |
|---|---|---|
| 623 | web tasks: browser check | real work: about 60 phases in one Chrome page, one after another |
| 262 | core 21a3 | wsx timer: two check-in minutes (70 s), fixed sleeps 31 s, a 25 s fake turn |
| 201 | host 42 | real work |
| 170 | web chat: tour | real work (browser, sizes x schemes) |
| 155 | host m2 | wsx timer: chat host linger, 20 s, five times |
| 141 / 140 / 139 | host / worker / core 20 | `manager_up`, run in all three parts: real start of a manager |
| 116 | host 37c | wsx timer: TURN_QUIET 15 s, three times |
| 116 | web tasks: post levels | real work (LaunchAgent-like environment, key loop) |
| 107 | worker 28 | wsx timer: quietMinutes 30 s, three times |
| 85 | host 32wk | real work and a sweep |
| 67 | worker 27m | sweep interval and reviewMinutes |
| 65 | host 32s2 | fixed sleeps, usage report script |
| 65 | host 37b | wsx timer: SIGN_IN_RETRY 12 s, four times |
| 64 | host 32r | chat host linger and real work |
| 63 | web chat: queue | real work (server restart) |

Timers not yet given a test setting: TURN_QUIET, quietMinutes, SIGN_IN_RETRY, recapMinutes, `manager_up` in worker and host. Cause: each needs a section-by-section run to prove no assert reads the old timing; only the chat host linger and the check-in minute are done.

### What changed

| Cause | Change |
|---|---|
| Waiting for a macOS slot (5 for the whole account) | Task checks run on this Mac only. `task` and `web` are a counted lane (its width follows load, RAM and disk, at most 3; `WSX_SCENARIO_SLOTS` lowers it; docs/design/scheduler.md) and a check-run's scenarios run at once. GitHub runs the task scenario only for pushes to main and tags. |
| Parts one after another | `scripts/scenario.py` starts the parts (task: core, worker, host, memory; web: chat, tasks) as separate processes, each with its own work dir, ports and sockets. |
| Chat host linger 20 s | `WSX_TEST_CHAT_LINGER_SECS` (the scenario sets 5). |
| Check-in minute | scenario setting 35 s to 14 s (it must outlast the linger). |
| One 623 s browser check | per-phase timer (`phase <s> s <name>` on stderr); narrow phases removed (below). |
| Disk: 9 GiB free, each copy 1.5-2.4 GB of `target/` | A shared cargo target stays rejected (cargo reuses another copy's build and runs its code). `build::reclaim` removes `target/` of copies idle over 30 min, longest idle first, when free disk is under 15 GB: before a task waits on `disk`, and in the hourly stale sweep. The `tasks.minFreeGb` refusal stays. |

### Removed sections

| Section | What it checked | Why it goes |
|---|---|---|
| test-task 32t3, 32t6-32t9, 32ob, 32oc, the ci chunk of 32s | the `ci:` lane: retry backoff, queue wait, orphan cancel, main's log | the lane is gone |
| web tasks: the ⋯ menu | menu entries | a label list; each page is checked where it opens |
| web tasks: project overview | markdown and mermaid in the sheet | the browser check covers markdown and diagrams |
| web tasks: Usage sheet filters | one filter set | the Usage tabs phase covers it |
| web tasks: Home marks a project | an accent on a row | one style value |
| web tasks: page never zooms | pinch and tap | one meta setting |
| web tasks: no terminal link in the task sheet | one absent link | one UI string |
| web tasks: narrow and wide, wide chat column | layout at two widths | pixel and column asserts; the browser check runs the sizes |
| web tasks: table under a list item | one rendering | markdown tables are in the browser check |
| browser check: 320 px tour, light loop at other sizes, reduced motion | the same pages at more sizes | the other sizes cover the case |

Seconds saved: the old per-phase times were not recorded (no phase timer existed), so the saving is measured from the first run with the timer, below. Not removed yet: narrow asserts inside kept web phases, test-task 32af wording, 32r TUI rows, 37 credit refusal, 37c S4. Each needs a host run to prove no later phase reads its state.

### What is #515 checking (lib.js plus one test)

`scenarioPaths` in `wsx.config.yml` already picks scenarios by touched paths (task 441): `crates/wsx-web/web/lib.js` is under the `web` paths and under no `task` path, so #515 ran `web` only (2 parts), not the 114 marked sections of `test-task.py` (core 37, worker 37, host 40). Inside `web` the whole journey runs: the chat part and the tasks browser check (about 60 phases). Only the phases that scroll a chat log use the changed `lib.js` code; settings, usage, media, routines, panels and markdown read other files. So the waste is inside `web`. Proposal, not built (needs the manager's ok): each browser phase names its files in a comment, and a run keeps only the phases whose files the diff touches, all phases when `web-cdp.mjs` or the server changes. The phase timer gives the numbers to decide it.

### After, on this Mac (final head 24b8c605, check-run 8 and 9, 2026-11-19)

| Part | Before (CI p50 of 40 runs, parts one after another or waiting for a macOS slot) | After (local, parts at once) |
|---|---|---|
| task core | 10-25 min | 658 s |
| task worker | 17-18 min | 716 s |
| task host | 25-28 min | 1386 s (1094 s in an earlier run) |
| task memory | in core | 18 s |
| task round, push to result | 29.5 min | 1386 s (23 min), the host part |
| web chat | 13 min | 428 s |
| web tasks | 24 min | 840 s |
| web round | 23 min | 841 s (14 min) |

Target of 10 minutes not reached. The floor is the task host part: its sections wait on wsx timers that have no test setting yet (TURN_QUIET 15 s in 37c, SIGN_IN_RETRY 12 s in 37b, quietMinutes in 28, recapMinutes in 35, the manager start of section 20 in every part, about 130 s). The web tasks part is one browser page with about 60 phases in sequence. Each setting needs a host run to prove that no assert reads the old value, so none was added blind.

Load: the machine's load average read 22 to 25 on 10 cores right after these runs while other tasks checked. A sample during a round with several tasks at once is not taken; `WSX_SCENARIO_SLOTS` stays 2 until it is. Disk: `df -h /` read 6.8 GiB free at the end (9.4 GiB at the start of the work, 7.4 GiB in between), so `build::reclaim` is needed.

### Parts, timers and removed sections, 2026-11-19 (task 517, head after 98b5ea62)

Parts. The host part (1386 s) held 32d-32s2, 33-35, 36-39c, 41-43. It is now three processes; the worker part (22-32ak) is two. Times are one local run each, parts at once (check-run 8 on 0312f02c, one run of all 7 parts: 641 s from start to the last part). The longest task part is core (21a3 203 s, mostly check-in minutes). Core (640 s) and web tasks (845 s) are above 10 minutes; every other part is under 10.

| Part | Before | After |
|---|---|---|
| core | 658 s | 640 s |
| worker (22-27d) | 716 s | 476 s |
| deploy (28-32ak) | in worker | 395 s |
| host (32d-32s2) | 1386 s | 574 s |
| agents (36-39c, m2) | in host | 541 s (check-run 8 on 0312f02c; 40 and 39b were fixed after it) |
| chats (33-35, 41-43) | in host | 325 s |
| memory | 18 s | 19 s |
| web chat / tasks | 428 / 840 s | 433 / 845 s |

Section 20 (manager start) costs 120-167 s in every part. It is real work plus fixed negative waits (4, 6 and 3 s), not a timer. Section 42 waited up to 300 s on the limit hold that `m2` leaves; it now runs in `chats`, without `m2`.

Timers. All five already had a setting where the timer is defined. The cost was the value the scenario set, so no new setting was added.

| Timer | Setting | Scenario value |
|---|---|---|
| sign-in retry (`store.rs`) | `WSX_TEST_SIGN_IN_RETRY_SECS` | 12 s, now 8 s |
| hung-turn bound `TURN_QUIET` (`stall.rs`) | `WSX_TEST_TURN_QUIET_FILE` | 15 s, now 10 s |
| check-in minute | `WSX_TEST_CHECKIN_MINUTE_SECS` | 14 s, now 10 s |
| `quietMinutes` | config, fractions allowed | 0.5, kept: section 28 needs three lands inside the window; shorter fails under load 18-25 |
| `recapMinutes` | config, fractions allowed | 0.2, kept for the same reason |

Removed sections. Each names what it no longer proves.

| Removed | Still proved elsewhere | Not proved now |
|---|---|---|
| 37 credit refusal on the Pi fallback (`PIERROR`, back home with a reason) | 37 `away` and S2 use the refusal file; the limit record path is asserted in 37b | the refusal text from a fallback with the own agent still limited ends in a home turn with that reason |
| 37c S4 (hung fallback) | 37c S1-S3 | a hung turn on the fallback while the own agent is limited |
| 32af wording asserts | the refusal itself is asserted | the words of the after-reason sentence |
| web post-levels bad-flag loop | bad `--level` is still refused (`test-task.py`, post-levels section) | key refusals: `a b`, empty, 65 characters, non-ASCII `--key` |

Not removed: the web tasks inner pixel asserts, the web-browser-check items and the 32r TUI rows. The tasks part is one journey with a server restart in the middle: the browser page is 40 phases, 410 s, plus about 430 s of server sections. It is not split, so its floor is about 14 min, above the 10-minute target.

Load and slots. `.work/load-samples.txt` holds 216 samples, 10 s apart, from 03:52 to 04:28 while this task's task and web scenarios ran at once and other tasks checked. Load average (1 minute, 10 cores): min 12.9, median 19.3, mean 21.6, max 47.8. The samples cover one task's two scenarios with the other tasks' load, not two tasks pushing together, so no second task's rounds were measured. The load stays near 2 times the core count with `WSX_SCENARIO_SLOTS` 2, which gave flaky waits only in the 0.5-minute sections above. The default stays 2.

Disk. Per-copy `target/` is 1.4-2.3 GB (7 copies: 12.5 GB). In t517 `debug/deps` is 1.19 GB, of which wsx crates 0.58 GB and third-party 0.61 GB. A shared `CARGO_TARGET_DIR` stays refused: the last build overwrites the binaries, and a check would run another copy's `wsxd` (docs/tasks.md "Reclaim"). `RECLAIM_BELOW_MB` is 15 GB (`crates/wsx-core/src/task/build.rs:73`): a goal, not a cap. It removes the build output of copies idle 30 minutes or more, longest idle first, only until free disk reaches 15 GB (about 4 of 7 copies here). Free disk in the samples was 6-7 GB, never above, and the period was not a busy hour. A cold build takes 49 s.

Cause found late: `replace::host_pid` matched a host by its arguments alone, machine-wide. Each part runs `task chat-host demo`, so a replace or stop in one part could hit another part's host. Full runs failed in section 40, core 21a and chats 33 while the same parts passed alone. It now also compares `TMPDIR` (host and callers share it). Check-run 8 on f48eed69 passed core 636 s, deploy 395 s, host 570 s, agents 498 s, chats 327 s, memory 20 s; worker failed at 27d (a late closed event of an earlier task took the stale line into an earlier turn; the section now waits for no open task).


CI run 44401417727 (host part, head 98055035 of #510): 33r failed with `reply to 253 timeout` and every later section that needs the manager failed too (33c, 33s, 37, 39, 40), so from the `refused` say of 33r on, the home manager answered nothing for the rest of the part (about 7 minutes). A slow turn would have ended inside a section. A CI part is its own job on its own runner, so a wait shared with another part is not the cause there; the part runs alone on that machine. The log has no manager state (the artifacts expired, the scenario printed only the label), so the first wrong state is not named. `reply_to` now puts the manager's turn record (`turn.json`) and its last 10 log entries in the timeout, so the next failure names it. The local cross-part cause (`host_pid`, above) kills a manager the same way, but only when parts share a machine.
