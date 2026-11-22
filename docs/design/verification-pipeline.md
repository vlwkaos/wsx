# Verification pipeline: what large-scale CI does, and what wsx should take

Research of 2026-11-03. Question: how to verify agent changes quickly without a weaker gate.

## Proven methods

| Method | Who, evidence | Idea |
|---|---|---|
| Affected-target selection | Google TAP presubmit: runs the targets the change's build dependencies reach, the rest after submit | Run what the diff can break, decided by declared dependencies, not by judgment |
| Predictive test selection | Meta (arXiv 1810.05286): catches over 95% of failing tests and over 99.9% of faulty changes at about half the test cost | Learn from history which tests a change can fail; needs a large history |
| Presubmit and postsubmit tiers | Google TAP: a limited presubmit, then every affected test on submitted ranges, with transition detection to find the breaking commit | Fast gate before landing, the full suite after, on the main branch |
| Flaky handling | Google (2016): 1.5% of test runs flaky, 16% of tests have some flakiness, 84% of pass-to-fail transitions involve a flaky test. Reruns only the failing tests; a flaky test reports a failure after 3 failures in a row; quarantine above a rate | A failure is evidence only after a rerun; flakiness is tracked, not argued |
| Merge queue with prioritization | Uber SubmitQueue (arXiv 2501.03440): 53% less CI use, 37% shorter P95 wait, by predicting build length and conflicts | Check each change against main plus the changes ahead of it; small changes do not wait behind big ones |
| Content-keyed caching | Bazel action cache | A result is reused when its inputs are byte-identical, wherever the commit came from |
| Combined check of parallel work | "Passes alone, fails together" (arXiv 2609.25396); practitioner guides | Changes that pass alone can fail together: the land of a batch checks the stacked tree |

The agent-specific sources (vendor blogs, early arXiv papers) agree on isolation by worktree, an independent reviewer, and checks before merge. None gives a measured standard. The proven parts come from the CI systems above.

## What wsx has now

- Isolation: one worktree per task. Combined check: `land` of several tasks checks the stacked tree once.
- Queue: one check slot per machine; a landing's checks go first.
- Reuse: a result is kept per clean tree id, in the host's memory, one tree at a time.
- Selection: the worker picks scenarios by its own judgment ("run the scenario of each area it changed"). Only GitHub Actions selects by paths (`ci.yml` `task-scenario`).
- Flakiness: none. A failure blocks, and a person decides whether it is related.

## Gaps and changes, by value per effort

| # | Change | Method | Effort | Gain |
|---|---|---|---|---|
| 1 | `tasks.scenarios.<name>.paths`: globs; the host picks the scenarios a diff touches; the worker no longer judges | affected targets | small | no full runs for a one-area change (17 to 10 min today) |
| 2 | Split the web scenario into parts by area (`--part` has chat and tasks; add terminal, push, pages) and list each part as a scenario with its paths | affected targets | medium | finer selection makes 1 pay more |
| 3 | A failed scenario part reruns once by itself; a pass on the rerun lands the change and records the part as flaky (count per part in the ledger) | flaky rerun | small | a load-timed failure (2026-11-03, tasks part, load 13) stops blocking |
| 4 | A failure in a part the diff does not touch is reported as a finding on main, not a block | presubmit scope | small | what was done by hand on 2026-11-03 |
| 5 | A nightly routine runs every scenario on main; a failure starts a fix task with the commit range since the last pass | postsubmit tier | small | the full suite still runs, off the critical path |
| 6 | Kept results keyed by a hash of each scenario's input paths, on disk | content cache | medium | a rebase onto unrelated main work reuses results |

Not worth it at this scale: predictive selection (needs thousands of runs of history) and queue prioritization (2 to 4 agents, one check slot).

## Order

1, 3 and 4 first: config and host logic, no scenario change. Then 5 (a routine). Then 2 and 6.

## Sources

- Predictive Test Selection, Meta: https://arxiv.org/abs/1810.05286
- Google TAP presubmit and postsubmit: https://mpapad.github.io/publications/pdfs/ICSE-SEIP2019.pdf and What Breaks Google: https://research.google/pubs/what-breaks-google/
- Flaky Tests at Google and How We Mitigate Them: https://testing.googleblog.com/2016/05/
- CI at Scale: Lean, Green, and Fast (Uber): https://arxiv.org/abs/2501.03440
- Passes Alone, Fails Together: https://arxiv.org/pdf/2609.25396
