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

Step 0 is not a child: #438 lands, then #450 lands as is (CI passed). Every later step builds on both, and step 3 owns their rules from then on. All other open work is inside a step (§3). Web and TUI work comes after the core state it reads (step 5), and the view model comes after the polish it moves.

| Step | Title | `--files` | Removes | Proof (check-run) | Model, cost* |
|---|---|---|---|---|---|
| 1 | Manager: root cause first, lean prompt | `crates/wsx/src/task/instructions.rs`, `crates/wsx/src/task/args.rs`, `crates/wsx/src/task/cmd/**`, `scripts/test-task.py`, `scripts/bench-manager.py`, `docs/tasks.md` | class 1 (3.1 KB) and class 4 (3.1 KB) text; the main-checkout commit rule `instructions.rs:135-137` | `bench-manager.py` replay gives the same task set; prompt ≤ 5 KB; `task` | opus, $8-12 |
| 2 | Gates replace manager rules | `crates/wsx/src/task/boundary.rs`, `crates/wsx/src/task/cmd/**`, `crates/wsx/src/task/args.rs`, `crates/wsx-core/src/task/{owner,copy}.rs`, `scripts/test-task.py`, `scripts/test-manager-boundary.py` | manager writes to the main checkout and push; hand reads of `log.jsonl` | `task`, `boundary` | opus, $10-15 |
| 3 | One turn record: liveness, quiet bound, alert | `crates/wsx/src/task/{host,manager,agent}.rs`, `crates/wsx-core/src/task/{stall,mod,status}.rs`, `scripts/test-task.py`, `scripts/fake-*.py`, `docs/tasks.md` | stall from mtime; per-adapter hang fixes; separate quiet rules in the task host and the chat host | `task` (host part) | opus, $12-18 |
| 4 | Limit floor on `agent::next` | `crates/wsx/src/task/{manager,host,agent}.rs`, `scripts/test-task.py`, `scripts/fake-*.py`, `docs/local-model.md` | #423's own move rule, which duplicates `agent::next` | `task` | sonnet, $4-6 |
| 5 | One transition table with `waits_on` | `crates/wsx/src/task/host.rs`, `crates/wsx-core/src/task/{mod,status,stall,needs}.rs`, `scripts/test-task.py`, `docs/tasks.md` | 15 `set_state` and 4 raw `record.waiting` writes; `manager_wait`'s guess | `task`, `web --part tasks` | opus, $10-15 |
| 6 | Manager turns by need, live context size | `crates/wsx/src/task/{manager,host,claude}.rs`, `crates/wsx-core/src/task/manager.rs`, `scripts/fake-claude-stream.py`, `scripts/test-task.py` | event-only turns; the size check at turn start only; unbounded tool output | `task` | sonnet, $5-8 |
| 7 | Web and TUI polish on the new state | `crates/wsx-web/**`, `crates/wsx/src/ui/**`, `crates/wsx-core/src/task/{view,needs,chats}.rs`, `scripts/test-web-client.py`, `scripts/*-check.mjs`, `scripts/test-task.py`, `scripts/runtime-smoke.py` | 4 paused branches; #432's own approval filter (now read from `waits_on`) | `web`, `task`, `smoke` | sonnet, $8-12 |
| 8 | One view model for web and TUI | `crates/wsx-core/src/task/{view,status,needs}.rs`, `crates/wsx-web/**`, `crates/wsx/src/ui/**`, `scripts/**` | duplicate status, count and row logic in `tasks.js`, `store.js` and `ui/` | `web`, `task`, `smoke` | opus, $12-18 |
| 9 | Scenarios by changed paths, one rerun, finding on main | `crates/wsx/src/task/{host,claude}.rs`, `crates/wsx-core/src/config/project.rs`, `scripts/test-task.py`, `docs/tasks.md` | the worker's "run only touched checks" sentence | `task` | sonnet, $5-8 |
| 10 | Deploy once per quiet window | `crates/wsx/src/task/host.rs`, `crates/wsx-core/src/task/store.rs`, `scripts/test-task.py` | one deploy per land | `task` (part 32) | sonnet, $3-5 |
| 11 | `say --model` with its composer pill | `crates/wsx-core/src/task/manager.rs`, `crates/wsx/src/task/{manager,args}.rs`, `crates/wsx-web/**`, `crates/wsx/src/ui/**`, `scripts/test-task.py` | nothing; one field on `Say` | `task`, `web --part tasks` | sonnet, $4-6 |
| 12 | Store cleanup, broker in process | `crates/wsx-core/src/task/**`, `crates/wsx/src/task/**`, `scripts/**`, `CHANGELOG.md` | trial, batch, 3 extra `seen` copies, one overview cache, loose notes, 1 Hz reads | `task`, `web`, `smoke` | sonnet, $10-15 |

*Cost: an estimate scaled from #427's average of $2.34 worker cost a task. Opus and design steps take more turns. The total is about $95-140.

**Order:** step 1 runs first, alone (it changes the manager that runs the rest). Steps 2 to 6 share `host.rs` and `manager.rs`, so they run one after another. Steps 9 and 10 can start in parallel with step 7: under step 2's overlap gate their files differ from the web files. Step 8 waits for step 7, and step 11 waits for step 8.

### Prompt outlines (paste after `wsx task new --into 451 --title <title> --files <globs> -`)

1. Cherry-pick #431's `4909432e` and `1a670f25`. Then rewrite the manager's text (`instructions.rs`) per `orchestration-lean.md` App. A: delete class 1, move class 4 to `wsx task --help`, keep class 3. Add one rule: a finding is "cause (the first state that goes wrong, file:line, log proof) plus the design step that removes it", never a symptom task. A second fix on one concern stops and names the shared owner. Examples show prompts through `-` and `<<'EOF'`. The `manager_up` phrase asserts in `test-task.py` follow the new sentences. Prove with a `bench-manager.py` replay and `task`.
2. Add gates (lean §7, class 2). In `boundary.rs`, the manager profile denies writes to the main checkout and denies push. `wsx task commit <files> --owner-said` runs `fmt` and `build` before it commits. `new` refuses an overlap (§4). Add `wsx task why <n>` (state, `waits_on`, last turn, log tail) and `task review <n>`. The host posts the ask line. Prove each refusal in `test-task.py` and `test-manager-boundary.py`.
3. Add one turn record per agent turn (worker and chat host): start, agent, last output, deadline. Silence past the bound ends the turn with its reason (#438's quiet bound). Stall is one per silence (#450's rule). A failed or silent manager turn is posted to the owner at `--level alert`, with the open-task count. Prove with fake agents that refuse, go silent or hit API 403.
4. Use #423's six commits as the reference and re-apply their rule on `agent::next` (`crates/wsx/src/task/agent.rs`): a limit pauses the Claude conversation and never rotates it; the local floor is a stand-in that answers from a fresh call; the owner's `tasks.manager.fallback` list stays. Run scenario 37f (a to d) for the first time.
5. One table `(state, event) -> (state, waits_on)` in `host.rs`. `waits_on` is owner, manager, task, host, limit or disk. `land_stopped` waits on the owner. `stall`, `status` and `needs` read `waits_on` and stop guessing. Prove that `task` passes, plus a new section for a land refused on main changes.
6. Events that need no decision become ledger rows, not manager turns. One wake rule owns which ones need a turn (`state_wakes`, wsx-core `task/manager.rs:308`). Apply context-budget bounds 1 to 3: live size with `rotation_due` after each line, `skillsOnly` for the manager, `--disallowedTools`, and the Bash and Read caps. Prove the turn count on a replayed event burst, and rotation inside a turn with `fake-claude-stream.py`.
7. Cherry-pick, in order, #420's 12 commits, then #432's `196e82ac`, #433's `43347a3a` and #434's `8c57a1aa`. Redo #432 row 3 (`needs::approved_tasks`) as a read of step 5's `waits_on`. Show "waits on you" from `waits_on` on the task sheet and Home. Follow `ui-view-standards`. Fix the failing web and task asserts. Prove with `web`, `task` and `smoke`, plus captures at 375, 390 and 1280 px.
8. Move status words, counts, needs and rows that `tasks.js`, `store.js` and `ui/` each compute into the wsx-core view model (`orchestration-lean.md` §6). Behavior stays the same; step 7's asserts stay green.
9. to 12. Write each prompt from `orchestration-lean.md` §9 items 1-3, §8 and §11 rows 7 and 10, and from `verification-pipeline.md` items 1, 3 and 4, with the proofs named there.

## 3. Open tasks: where each one goes

| Task | Commits on its branch (`git log main..wsx/t<n>`) | State of the work (its `log.jsonl`) | Step | Carried | Left or redone |
|---|---|---|---|---|---|
| #438, #450 | proven | CI passed | 0: land on their own | n/a | nothing; steps 3 and 4 own their rules from then on |
| #431 manager instructions | 2: `4909432e`, `1a670f25` | CI failed in `manager_up` on a phrase assert; `1a670f25` fixed it; rerun unproven | 1 | both, by cherry-pick | the phrase asserts move with step 1's rewrite |
| #423 local model fallback | 6: `e4be2752`, `7fc6a482`, `fac0dd2e`, `4bc59bf9`, `dd6ccfb8`, `bb980da4` | `task` CI failed; 37f rewritten, never run | 4 | rule and docs, as the reference | redone on `agent::next` |
| #420 Home pills, New chat | 12: `a55ba258`, `023ec18d`, `78f9d3ae`, `d81909f0`, `5c98cdd5`, `c98e9dd4`, `8206652d`, `866aaf86`, `5dac5f87`, `6795deff`, `c92fb170`, `ba4d8d7d` | review done; `web`, `task` and smoke passed on its base | 7 | all 12 | rerun the three checks on the parent |
| #432 web polish A | 1: `196e82ac` (rows 1, 3, 12, 14, 15, 19, 21, 23) | `web` and `task` failed (failure not read here) | 7 | the commit | row 3 redone on `waits_on`; fix the failures |
| #433 task sheet, To check | 1: `43347a3a` (8 rows) | `web` and `task` failed | 7 | the commit | fix the failures; row 4's lint `overlap` cause is unconfirmed |
| #434 sessions, settings, machine | 1: `8c57a1aa` (rows 7, 8, 13, 16, 18, 20, 22) | `web` failed; no row proven | 7 | the commit | fix and prove each row |
| #439 to #444, #446, #447 (the orchestration-lean steps, not #445) | none | waiting | #440 → 2, #443 → 5, #444 → 8, #441 → 9, #442 → 10, #439 → 11, #446 and #447 → 12 | none | all; Q1 and Q4 set step 12's scope |
| #445 prompt by capability | none | waiting | close | none | obsolete: step 1 writes one judgment prompt for every level, and step 2 makes the rest gates. The level B reply schema waits on Q2. |

When a step starts, the manager closes the tasks it absorbs (`wsx task close <n>`). The branch stays until the parent lands, so the cherry-picks keep their source.

## 4. Decision for problem 7: refuse at `new`, no host queue

- **Measured:** 26 `wsx task after` calls in the whole manager log, 4 of them `--because files`; `new --after` 4 times in 344 `new` calls. The open `files` links are in §1, row 7.
- **A host queue** would automate the same guess. On crate-wide globs every task overlaps, so it would serialize everything. `after` already starts the waiting task when the other task lands.
- **Choice:** `new` refuses when its `--files` overlap an open task, and prints the task and its files. A glob wider than one folder below `crates/<crate>/` is refused as too broad. The manager then chooses one of three: `add` to the open task, `--into` a parent, or `--after` with the overlap named. Ordered plans become children of one parent, so they run in parallel when their files differ. Built in step 2.

## 5. Owner questions that still block a step

| Q | Blocks | Question | Recommendation |
|---|---|---|---|
| 1 | step 12 | Remove trial and batch? (0 uses since 10-29; `--into` stays, this plan uses it) | Yes |
| 2 | step 6 | Sonnet for manager event turns, once the wake rule leaves only events that need a decision? | Yes, after step 6's turn count |
| 3 | step 2 | The manager loses all writes to the main checkout; your draft lands through `wsx task commit --owner-said` (runs `fmt` and `build` first). Agreed? | Yes |
| 4 | step 12 | Delete the 29 loose manager notes files and the scratch files? | Yes |
| 5 | step 4 | Keep the explicit `tasks.manager.fallback` list (#423's assumption)? | Yes |
