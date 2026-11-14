# Root-cause refactor of wsx orchestration

Owner, 2026-11-14: stop patching; do one refactor along the big plan, and make the manager look for the root cause first. This doc merges `orchestration-lean.md` §11 and `context-budget.md` (ranked bounds) into one order. #451 is the parent task. Each step is a child task (`wsx task new --into 451`), and the refactor lands on main once (docs/tasks.md "Parent tasks").

Evidence: the task store `~/.local/state/wsx/tasks/wsx-8cbe3f08`, 24 h to 2026-11-14 (manager `log.jsonl`, 14,898 rows; task logs and `task.json`), plus this checkout at main `ecb9523f`.

## 1. Four causes behind the eight problems

| # | Problem | First step where the state goes wrong (proof) | Cause |
|---|---|---|---|
| 1 | Hung turns | `420/log.jsonl:295`: Pi refused the prompt ("No API key"). The next row is the manager's `after`, 7.7 h later. The host has no bound on a turn that shows no output; `pi.rs:378` (`ecb9523f`) fixed one adapter only. The ir manager was dead from 2026-11-13 01:13 (`438/log.jsonl:169`). | A |
| 2 | Same stall reported again and again | `stall.rs:277-297 progress_ms` is the newest mtime of `log.jsonl`, so manager and host writes restart the clock. The manager log has 30 stall lines in 24 h: #420, #423 and #431 have 5 each, #433 and #434 have 4 each. | A |
| 8 | The owner learns of failures first | A turn that ends on an error makes no event. The owner's main session stopped on API 403 at 19:17, and the manager answered only when the owner asked, 3.2 h later (reply 4516). No count of open tasks reaches the owner. | A |
| 3 | Ready, "the manager has not reviewed it", while the land waits on the owner | `host.rs:3271-3282 finish_landing` logs `LandStopped` and sets `Ready` with no `waiting`, and `stall.rs:72-114 manager_wait` counts that Ready as the manager's. #438: `land_stopped` at 1794607107 (main's uncommitted `pi.rs`), then a stall at 1794608978 saying "the manager has not reviewed it". | B |
| 4 | Writes to the main checkout bypass checks | `instructions.rs:135-137` tells the manager to commit in the main checkout, and `boundary.rs:95-101` allows it. That commit (`ecb9523f`) fails `cargo fmt --all --check` at `pi.rs:378` today. | C |
| 5 | Fragile manager tool calls | Stdin and `--prompt-file` already exist (`cli/args.rs:721`). The prompt teaches double-quoted text (`instructions.rs:107`), so backticks run in the shell. Bash lines are clipped in the log (0 of 344 `new` lines show the loss). 40 of 313 manager calls in 24 h read store files by hand: no command answers "why is #n where it is". | C |
| 7 | Long `after` chains, 18 open tasks | Of 16 `after` links open now, 8 are `files`, 7 `needs` and 1 `check`. All 8 `files` links are the #439-#447 chain, made by hand from one ordered plan. Their `--files` globs are crate-wide (`crates/**`), so every pair "overlaps". Open now: 15 waiting, 2 working, 1 failed. | C |
| 6 | Turns that change nothing; 85-130k context | 31 of 49 delivered batches in 24 h were events only, and 30 of them got a manager turn. `host.rs:1977 rotation_due` runs only at a turn's start (`context-budget.md`). | D |

- **A. A turn has no owner record.** No record holds "this agent turn is alive, since when, until when". Liveness is guessed from file mtimes and from each adapter's own end-of-turn. A dead or silent turn makes no event, so it does not reach the manager or the owner.
- **B. State does not say who it waits on.** `Ready`, `land_stopped` and `after` hold no "waits on owner, manager, task or host" field. Stall, status and the owner's view guess it, and they guess wrong.
- **C. Decisions that code can make or check are left to the manager's prompt.** Examples: commit on main, how to quote a prompt, judging overlap, diagnosing from raw logs. A strong model gets them right mostly; nothing refuses a wrong one.
- **D. Every event costs a manager turn, and context is checked late.** The event stream goes to the model, not to the ledger. Size is checked only between turns.

## 2. Ordered steps (each one is a child of #451)

Step 0 is not a child: #438 lands, then #450 lands as is (CI passed). Step 3 builds on both and owns the liveness rule from then on.

| Step | Title | `--files` | Removes | Proof (check-run) | Model, cost* |
|---|---|---|---|---|---|
| 1 | Manager: root cause first, lean prompt | `crates/wsx/src/task/instructions.rs`, `crates/wsx/src/task/args.rs`, `crates/wsx/src/task/cmd/**`, `scripts/test-task.py`, `scripts/bench-manager.py`, `docs/tasks.md` | class 1 (3.1 KB) and class 4 (3.1 KB) text; the main-checkout commit rule `instructions.rs:135-137` | `bench-manager.py` replay gives the same task set; prompt ≤ 5 KB; `task` | opus, $8-12 |
| 2 | Gates replace manager rules | `crates/wsx/src/task/boundary.rs`, `crates/wsx/src/task/cmd/**`, `crates/wsx/src/task/args.rs`, `crates/wsx-core/src/task/{owner,copy}.rs`, `scripts/test-task.py`, `scripts/test-manager-boundary.py` | manager write access to the main checkout and push; the hand reads of `log.jsonl` | `task`, `boundary` | opus, $10-15 |
| 3 | One turn record: liveness, quiet bound, alert | `crates/wsx/src/task/{host,manager,agent}.rs`, `crates/wsx-core/src/task/{stall,mod,status}.rs`, `scripts/test-task.py`, `scripts/fake-*.py`, `docs/tasks.md` | stall from mtime; per-adapter hang fixes; the host's and the chat host's separate quiet rules | `task` (host part) | opus, $12-18 |
| 4 | One transition table with `waits_on` | `crates/wsx/src/task/host.rs`, `crates/wsx-core/src/task/{mod,status,stall,needs}.rs`, `scripts/test-task.py`, `docs/tasks.md` | 15 `set_state` and 4 raw `record.waiting` writes; `manager_wait`'s guess | `task`, `web --part tasks` | opus, $10-15 |
| 5 | Manager turns by need, live context size | `crates/wsx/src/task/{manager,host,claude}.rs`, `crates/wsx-core/src/task/manager.rs`, `scripts/fake-claude-stream.py`, `scripts/test-task.py` | event-only turns; the turn-start-only size check; unbounded tool output | `task` | sonnet, $5-8 |
| 6 | Web polish lands (paused work) | `crates/wsx-web/**`, `crates/wsx/src/ui/**`, `scripts/test-web-client.py`, `scripts/*-check.mjs` | 4 paused branches | `web`, `task` | sonnet, $5-8 |
| 7 | Limit floor on `agent::next` | `crates/wsx/src/task/{manager,host,agent}.rs`, `scripts/test-task.py`, `docs/local-model.md` | #423's own move rule, duplicated by #438's `agent::next` | `task` | sonnet, $4-6 |
| 8 | Scenarios by changed paths, one rerun, finding on main | `crates/wsx/src/task/{host,claude}.rs`, `crates/wsx-core/src/config/project.rs`, `scripts/test-task.py`, `docs/tasks.md` | the worker's "run only touched checks" sentence | `task` | sonnet, $5-8 |
| 9 | Deploy once per quiet window | `crates/wsx/src/task/host.rs`, `crates/wsx-core/src/task/store.rs`, `scripts/test-task.py` | one deploy per land | `task` (part 32) | sonnet, $3-5 |
| 10 | `say --model` | `crates/wsx-core/src/task/manager.rs`, `crates/wsx/src/task/{manager,args}.rs`, `crates/wsx-web/**`, `scripts/test-task.py` | no field removed | `task`, `web --part tasks` | sonnet, $4-6 |
| 11 | Store cleanup, then one view model, then broker in process | `crates/wsx-core/src/task/**`, `crates/wsx/src/task/**`, `crates/wsx-web/**`, `scripts/**` | trial, batch, 3 extra `seen` copies, one overview cache, loose notes, 1 Hz reads | `task`, `web`, `smoke` | opus for the view model, sonnet for the rest, $20-30 |

*Cost: an estimate scaled from #427's average of $2.34 worker cost a task. Opus and design steps take more turns. The total is about $90-140.

**Parallel:** steps 1, 6 and 7 do not share files, so they run at once. Steps 2 to 5 follow step 1 (the instruction text). Steps 8 to 11 follow step 4 (the state table).

### Prompt outlines (paste after `wsx task new --into 451 --title <title> --files <globs> -`)

1. Rewrite the manager's text (`instructions.rs`) per `orchestration-lean.md` App. A. Delete class 1. Move class 4 to `wsx task --help`. Keep class 3. Add one rule: a finding is "cause (first divergent state, file:line, log proof) plus the design step that removes it". Never a symptom task. A second fix on one concern stops and names the shared owner. Examples show prompts through `-` and `<<'EOF'`. Take #431's `4909432e` and `1a670f25`. Prove with a `bench-manager.py` replay and the `test-task.py` prompt asserts.
2. Add gates (lean §7, class 2). In `boundary.rs`, the manager profile denies writes to the main checkout and denies push. `wsx task commit <files> --owner-said` runs `fmt` and `build` before it commits. `new` refuses an overlap (see §4 of this doc). Add `wsx task why <n>`: state, `waits_on`, last turn, log tail. Add `task review <n>`. The host posts the ask line. Prove each refusal in `test-task.py` and `test-manager-boundary.py`.
3. Add one turn record per agent turn (worker and chat host): start, agent, last output, deadline. Silence past the bound ends the turn with its reason (#438's quiet bound). Stall is one per silence (#450's rule). A failed or silent turn of the manager is posted to the owner at `--level alert`, together with the open-task count. Prove with fake agents that refuse, go silent or hit API 403.
4. One table `(state, event) -> (state, waits_on)` in `host.rs`. `waits_on` is owner, manager, task, host, limit or disk. `land_stopped` waits on the owner. `stall` and `status` read `waits_on` and stop guessing. Prove that the `task` scenario passes unchanged, with a new section for a land refused on main changes.
5. Events that need no decision become ledger rows, not manager turns. One wake rule owns which ones need a turn (`state_wakes`, wsx-core `task/manager.rs:308`). Apply context-budget bounds 1 to 3: live size with `rotation_due` after each line, `skillsOnly` for the manager, `--disallowedTools`, and the Bash/Read caps. Prove the turn count on a replayed event burst, and rotation inside a turn with `fake-claude-stream.py`.
6. Cherry-pick the polish commits (table in §3), rebase, and pass `web`. Follow `ui-view-standards`. Then the original tasks close.
7. Re-apply #423's rule on #438's `agent::next` (`crates/wsx/src/task/agent.rs`): a limit pauses the Claude conversation and never rotates it; the local floor is a stand-in that answers from a fresh call. Use #423's commits as a reference, not as a cherry-pick.
8. to 11. Write each prompt from `orchestration-lean.md` §9 items 1-3, §8, §11 rows 7, 8 and 10, and `verification-pipeline.md` items 1, 3 and 4, with the proofs named there.

## 3. Open tasks: where each one goes

| Task | Commits on its branch (`git log main..wsx/t<n>`) | Goes to | Carried | Left or redone |
|---|---|---|---|---|
| #420 | 12: `a55ba258` … `ba4d8d7d` | step 6, then close | all 12 by cherry-pick | rebase; web pass |
| #423 | 6: `e4be2752`, `7fc6a482`, `fac0dd2e`, `4bc59bf9`, `dd6ccfb8`, `bb980da4` | step 7, then close | the rule and test sections 37 (as reference) | redone on `agent::next` |
| #431 | 2: `4909432e`, `1a670f25` | step 1, then close | both commits | the rest of its review is step 1's rewrite |
| #432 | 1: `196e82ac` | step 6, then close | cherry-pick | rebase |
| #433 | 1: `43347a3a` | step 6, then close | cherry-pick | rebase |
| #434 | 1: `8c57a1aa` | step 6, then close | cherry-pick | rebase |
| #439 | none | step 10, close | none | all |
| #440 | none | step 2, close | none | all |
| #441 | none | step 8, close | none | all |
| #442 | none | step 9, close | none | all |
| #443 | none | step 4, close | none | all |
| #444 | none | step 11, close | none | all |
| #445 | none | close | none | step 1 writes one judgment prompt for every level. The level B reply schema waits on Q2. |
| #446 | none | step 11, close | none | all |
| #447 | none | step 11, close | none | all; Q1 and Q4 decide its scope |
| #438, #450 | proven | step 0, land as is | n/a | step 3 owns their rules from then on |

## 4. Decision for problem 7: refuse at `new`, no host queue

- **Measured:** 26 `wsx task after` calls in the whole manager log, 4 of them `--because files`. `new --after` was used 4 times in 344 `new` calls. All 8 open `files` links come from one ordered plan, on `crates/**` globs.
- **A host queue** would automate the same guess. On crate-wide globs every task overlaps, so it would serialize everything. `after` already starts the waiting task when the other task lands.
- **Choice:** `new` refuses when its `--files` overlap an open task, and prints the task and its files. A glob wider than one folder below `crates/<crate>/` is refused as too broad. The manager then chooses one of three: `add` to the open task, `--into` a parent, or `--after` with the overlap named. Ordered plans become children of one parent, so they run in parallel when their files differ. Built in step 2.

## 5. Owner questions that still block a step

| Q | Blocks | Question | Recommendation |
|---|---|---|---|
| 1 | step 11 | Remove trial and batch? (0 uses since 10-29; `--into` stays, this plan uses it) | Yes |
| 2 | step 5, #445 | Sonnet for manager event turns, once the wake list leaves only events that need a decision? | Yes, after step 5's turn count |
| 3 | step 2 | The manager loses all writes to the main checkout; your draft lands through `wsx task commit --owner-said` (runs `fmt` and `build` first). Agreed? | Yes |
| 4 | step 11 | Delete the 29 loose manager notes files and the scratch files? | Yes |
| 5 | step 9 | Is a 10 min quiet window right for deploys? | 10 min |
