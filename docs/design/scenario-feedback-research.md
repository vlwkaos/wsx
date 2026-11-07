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
