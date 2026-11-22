# Work model: long work, parallel features, related projects

Task #544, 2026-11-20. Design only, no product code. Asked in wsx 5772; §7 (knowledge) added for wsx 5791.
Numbers: `python3 -I docs/design/work-model-measure.py`, run 2026-11-20 over 848 tasks of 15 projects (2026-10-16 to 11-20). The store is live, so a rerun moves a count by one or two.

## For the owner

The records support half of the claim. Work is cut where it continues, and long features go wrong. Rejected work is rare. On 5791: the manager's words are 3% of what a worker starts with. The worker reads the rest itself, so its quality rests on what is stored and on what a reset keeps.

| What the records show | Number |
|---|---|
| A task lands 3 min after its last result. Its copy goes in the same minute and it takes no more adds | p50 3 min, 0.0 min |
| Tasks that name a task landed in the 72 h before and change the same files | 267 of 848 (31%); half start within 0.7 h of that land (wsx alone: 1.8 h) |
| Tasks that get an add before their first result | 44%. Of those 969 adds, 258 follow a message of yours and 156 are in no manager turn |
| Worker spend before a task's first result | 71% |
| Tasks with your add after a result, by tasks open in the chat at its start | 8% at 0-1 open, 18% at 5+ |
| Tasks with 4+ revisions | 15% of tasks, 36% of spend; your add after a result in 41%; the manager sends back 79% |
| Work you rejected | To check failed 8 of 216, discarded after a result 7, reverts 6 |
| `hand`, `preview`, `trial` | 0, 0, 1 uses |
| A worker's first model call (§7) | 95.6 KB. The manager's words: 2.5 KB (3%). An index of past tasks: 25.5 KB (27%), which 7 of 20 workers followed |
| Stored knowledge that is past, wsx (§7) | 129 rotation notes of ended tasks are 509 KB of the 976 KB memory store; 142 superseded rules; 36 old manager handovers |
| A reset (`rotated`) (§7) | The fresh conversation gets 8.3 KB of notes the worker wrote. In 88 of 121 rotations an add the worker had already answered is not in it |

"Your add": the task log names no author. It means an add the manager sent in a turn your message started, or an add in no manager turn of the task's chat (your own, another chat's, or a turn the log lost).

| | Today | Proposed |
|---|---|---|
| A line of work | A chain of tasks, each a fresh worker with a new prompt | One task. Its number, its conversation and its notes last as long as the work |
| At land | Copy and branch go at once; the task takes no more adds | The same, but an add in the next 24 h brings the copy back and resumes the same conversation |
| A follow-up | The manager judges: `add` or `new` | wsx refuses the `new` and names the task to add to |
| You run it before it lands | `--hold`, which the manager chooses | The same; unchanged |
| Cleanup | `close` at land; the hourly sweep for other worktrees | Unchanged, so no deletion can be missed |
| Another project asks for work | `say --to`; no record of who asked. Any message went to the asker within 1 h of the land for at most 55 of 110 lands | wsx records who asked and sends them the land |
| The manager | Cuts features into tasks, writes each prompt, relays | Routes, reviews, lands, reports. It cuts only independent work |
| What a worker knows at its start | 92 KB that is the same for every task, 3 KB for this task | The index of past tasks goes; the task's own part stays |
| Past knowledge | Kept: archive files, superseded rules, notes of ended tasks | wsx deletes it at close, at supersede and at land, each with a proof (§7) |
| A reset | The worker's notes carry your answered adds, the rules and the state of the code | wsx sends your words and the rules again; the notes carry only decisions |

Three decisions:

1. **What a line of work is.** Pick: a task (§2). Not a new kind of session and not a chat that edits code.
2. **What an add after a land does.** Pick: it reopens the task for 24 h. Of the follow-ups counted over 14 days, 86% start inside 24 h and 95% inside 72 h. The other ways: keep every copy on disk 24 h after land, or hold every feature until your word.
3. **What is deleted.** Pick: `trial` (1 use), and stored knowledge by the rules of §7: a task's rotation notes at close, a superseded rule at once, a fact when a land removes its paths, the index of past tasks from a worker's start. Keep `hand` and `preview`: your words in 5772 ask for exactly what they do, and the records cannot say why you did not use them.

Nothing is built until the experiment in §6 says so. It runs today with `--hold`, with no new code. §7 names its own test for each change.

## 1. What the records say

| Claim in 5772 | Verdict | Evidence |
|---|---|---|
| A split task needs too much context | Partly | The prompt is small (p50 1.8 KB, p90 3.1 KB) and repeats little (5-word runs already sent to a worker: p50 1%, p90 30%). The context comes later: 44% of tasks get adds before the first result (54% of tasks born in your turn), p50 971 B more |
| Splitting lowers quality | Partly | More tasks open at once, more tasks with your add after a result: 8% / 9% / 18% at 0-1 / 2-4 / 5+ open (wsx alone: 8% / 11% / 18%). But a task that continues a landed one does no worse (8% against 12%), and `--size small` is no cleaner (11% against 11%) |
| A long session must stay alive | Yes, for the long 15% | 4+ revision tasks: p50 511 min to land, $4.49. Half of them (51%) had the conversation replaced by a fresh one (`rotated`); your add after a result in 64% of those and 18% of the others |
| Closing at once hurts | Yes in time, not per unit of work | A follow-up as an add to an open task: p50 17 min, $0.93, 14.6K output tokens. As a new task: 43 min, $2.05, 33.5K output tokens to its first result. So an add is about half the work, and the add row holds only adds that led to a revision. An add that waits when the task lands is dropped (host.rs:4195) |
| Talk between projects is not smooth | Partly | 256 asks between chats that share no project, 207 got a reply, p50 0.4 min, p90 1 min. Tasks another project asked for show no clear difference over all projects (your add 10%, sent back 34%, against 11% / 28%) and do worse in wsx alone (15% / 46%, n 54, against 13% / 35%). The gap is the report: no record says who asked. Any message went to the asker within 1 h of the land for 55 of 110 lands (wsx alone 18 of 53), and that is an upper bound: the message need not be about the task |
| Worktrees are missed | Not for tasks | 837 copies removed at close. Outside tasks: 4 worktrees (ir 2, one uncommitted, 37 days since a commit; dotfiles 1, 21 days; wsx 1) and 17 sessions you opened; 8 `pruned:` notices |

One kept conversation is not cheaper per unit of work: $0.097 per 1K output tokens for your add against $0.063 for a new task (wsx alone: $0.153 against $0.063). It saves the start, not the work.

`--size small` and successors have the same rate of your adds as the rest. So no rule by size.

Revisions, the add rate and tasks open at once are not independent measures: an add that is acted on makes a revision, and a busy chat holds the long tasks.

The records cannot show:
- Your verdict on quality. An add after a result is a correction or a new wish.
- Who wrote an add. The script classes it by the turn of the task's chat around it.
- Context restated in other words, or from Korean to English.
- Why a result missed: thin prompt, wrong decision and weak model look the same.
- Work in sessions you opened by hand, and worktrees deleted before the prune notices.
- Why `hand` and `preview` went unused.

## 2. The model

**A line of work is a task.** The code already gives a task what your long session had: a working copy (`copy::create`, copy.rs:139), one worker process that lives across turns and resumes by `agent_session` (host.rs:1674, 1684), a wsxd session `task-<n>` in that copy (cmd/new.rs:504), and removal at close (cmd/work.rs:497). What it lacks is life after its first land. `TaskState::may_move` says `Landed => to == Closed` (wsx-core/src/task/mod.rs:85).

Why not the others:

| Option | Why not |
|---|---|
| A session with a worktree that wsx tracks | It has no checks, no land and no events. The sweep (stale.rs:153) removes it only when idle 30 days and clean. This is the session mode you left |
| The chat's manager edits the code | A worker needs p50 11 to 12 model minutes to a first result. The chat would not answer in that time, and the manager's boundary denies the default branch |
| A parent with children (`--into`) | Each child is a fresh worker. It fits steps with different files that run at once (23 uses), not one conversation |

```mermaid
stateDiagram-v2
    [*] --> working: wsx task new
    working --> checking: result with a new revision
    working --> waiting: asks, paused, limit, load (WaitOn)
    waiting --> working: add, the wait ends
    checking --> working: checks fail
    checking --> ready: checks pass
    ready --> working: add (the manager sends back, or you)
    ready --> landed: land (held: on your word)
    landed --> closed: wsx task close, at once (host.rs:4291)
    landed --> working: add within 24 h (new: copy::create, resume agent_session)
    closed --> [*]
    note right of landed: no copy, no process, the record stays
```

**Long sessions.** Nothing stays running. The record and the conversation stay; the copy comes back at the same path on the next add, so Claude finds its session by folder and Pi by `--session-dir` under the agent's home (pi.rs:297). You open it from the task sheet (add), from session `task-<n>` in the TUI or web Sessions, or with `wsx task hand`, which opens a fork of the worker's conversation, not the conversation itself (`--fork-session`, cmd/work.rs:378-383). You run it with `wsx task preview`, or `--hold` and a pane in its session. It ends as today: `close` at land. After 24 h a follow-up is a new task. The limit rests on one measure: 86% of follow-ups start inside 24 h. The price of a late turn does not decide it: a turn an add starts after a pause over 24 h costs $0.57 p50 (n 6), after 1-24 h $0.79 (n 48).

A long conversation still loses context at rotation (51% of long tasks). The task's `rotate-notes.md` carries it; the manager host's rotation owns that (docs/tasks.md "Context rotation"). This model makes those notes matter more and changes nothing in them.

**Async tasks.** Splitting fits independent work: 323 tasks (38%) ended with 1 revision, p50 16 min, $1.26. That group has no add after a result by construction (an add that is acted on makes a second revision), so it shows how much such work there is, not that splitting helps. Splitting hurts when the new task needs what a just-landed task knows. wsx can read that from the command: the prompt names a task of the same chat landed in the 24 h before, and `--files` meets that task's own area. That was 206 of the 796 tasks with `--files` (26%). Files alone cannot be the rule: 578 of those 796 (73%) meet such an area.

The rule, in `refuse_overlap` (cmd/new.rs:224), which already refuses a `new` whose files meet an open task: a task landed in the last 24 h that the prompt names counts as open. The refusal text stays "add the work to it with wsx task add <n>". The manager's line "Similar work or the same area goes to the open task" (instructions.rs:76) goes.

**Across projects.** One channel, `say --to` (cmd/chat.rs:42). Two records replace what the managers remember:
- Who asked. A task created in a turn that another chat's message started gets that chat on its record. `chats::owner` (chats.rs:174) then sends the task's events to the asker too, as the muted rows chats already exchange.
- Who waits. `--after` (args.rs:396) names a task of another project. The waiting task starts no turn until it lands, as inside one project today, and `wsx task flow` shows it.

**The manager.** It keeps: routing between chats, review of each result (it sends back 28% of tasks), land, close, To check, the report to the asker. It stops: writing a stand-alone prompt for a follow-up, and judging `add` against `new`. It still starts a new task for independent work, and that prompt still stands alone.

**Adapters.** The model needs one thing from an adapter: resume a conversation by id after the process ended. Claude does (`--resume`, claude.rs:452) and Pi does (pi.rs:297). A defect found on the way: `hand` runs `claude --resume <agent_session> --fork-session` for every task that has an `agent_session`, with no test of the adapter (cmd/work.rs:378-383), so a Pi task would start Claude with a Pi id. A task gets a plain shell only when it has no session. Codex has no task adapter (`Agent`, agent.rs:231), and all 848 tasks ran on Claude. A Codex adapter must meet the same one need. [unverified: that `codex resume <id>` does.]

## 3. How others do it

| Who | Method | What wsx takes |
|---|---|---|
| Trunk-based teams (DORA; Fowler, "Patterns for Managing Source Code Branches") | Three or fewer active branches, merged at least daily; no more than a day's work unmerged | Land each slice, as today. The long thing is the conversation, not the branch. So no "hold every feature" |
| Graphite stacked diffs | Small diffs merge from the bottom; `gt sync` restacks what is above onto trunk | The reopened task starts its next revision from the default branch |
| Devin | A session sleeps after 5 min idle and a message wakes it; a sleeping session costs nothing | A landed task keeps no process; an add wakes it |
| Cursor cloud agents | You take over the agent's desktop to test, then release it; the agent ends with a PR and captures | `hand`, `preview` and To check captures already do this. Keep them |
| Cognition, "Don't build multi-agents" | Share the full trace, not a message; a sub-task copy drops the turns that give it meaning | A follow-up goes to the conversation that did the work |
| Anthropic, multi-agent research system | Parallel agents fit broad, independent work; most coding has fewer parallel parts | Split only independent work |
| Claude Code docs | The main conversation for phases that share context; a fresh one between unrelated tasks | The 24 h limit: old context is a cost |

Sources: dora.dev/capabilities/trunk-based-development, martinfowler.com/articles/branching-patterns.html, graphite.com/docs/merge-stack-prs-github, docs.devin.ai/admin/billing/usage, cursor.com/docs/cloud-agent, cognition.com/blog/dont-build-multi-agents, anthropic.com/engineering/multi-agent-research-system, code.claude.com/docs/en/sub-agents. Read by a research agent, not by me.

## 4. Changes

Lines are rough.

| | What | Where | In / out |
|---|---|---|---|
| Keep | Task, checks, land, close removes the copy | docs/tasks.md; cmd/work.rs:497; copy.rs:316 | 0 |
| Keep | `--hold`, `--after`, `--into`, `--plan`, `--size small` | args.rs:387-432 | 0 |
| Keep | `hand`, `preview` | cmd/work.rs:359; preview.rs:138 | 0 |
| Keep | The stale sweep for worktrees no task owns | stale.rs:153, 370; repo.rs:1015 | 0 |
| Keep | Rotation and its notes | host.rs:2363 | 0 |
| Change | An add within 24 h of the land reopens the task | mod.rs:85 `may_move`; host.rs:1019 `require_open` ("it takes no more messages"); host.rs:4195 (adds dropped at land); copy.rs:139 (`create` refuses a branch that exists; close deletes it, copy.rs:327); host.rs:1684 | +120 / -20 |
| Change | `new` refuses a follow-up of a task landed in the last 24 h | cmd/new.rs:224; `view::files_meet`, wsx-core view.rs:706 | +40 / -5 |
| Change | The record names the chat that asked; its events go there too | mod.rs:334 beside `created_by`; chats.rs:174 | +60 / 0 |
| Change | `--after` takes a task of another project | args.rs:396; cmd/new.rs:334-346 | +50 / -10 |
| Change | Manager text: the same-area line goes | instructions.rs:76-82 | +0 / -3 |
| Change | docs/tasks.md "Status", "By hand", "Hold and trial", "Chats" say the above | docs/tasks.md:59, 355, 399, 613 | +25 / -40 |
| Delete | `trial`: the live-site slot, `deploy --trial`, the web action | trial.rs (81); host.rs:1227-1300; cmd/deploy.rs:541; web tasks.rs, task.js, tasks.js | 0 / -350 |

Order: reopen, then the refusal (it needs reopen to be worth an add), then the two cross-project records, then `trial`. Each is one task with its own scenario in `scripts/test-task.py`.

## 5. Open tasks

| Task | Verdict |
|---|---|
| #510 Home: For you row | Keep. A reopened task is the same row |
| #533 Web task sheet: top part and action row | Change if decision 2 is yes: a task landed in the last 24 h keeps its add field |
| #534 Web chat: one window | Keep |
| #535 Port Preact to SolidJS 2 | Keep. It is the next long feature: run it as the one-task arm of §6 |
| #540 A chat has a purpose | Keep. A purpose belongs to a chat, a line of work is a task; they do not meet |
| #541 Home: chat folders | Keep |
| #542 Shared failure key | Keep |
| #543 Client plugins | Keep. The split arm of §6 |
| Flat-chats steps 1-7 | Keep all. This model adds no chat, thread or derived chat. Step 1 edits instructions.rs first; the same-area line goes after it |

## 6. Experiment

It tests the idea, one conversation for one feature, before any code.

| | One task | Split, as today |
|---|---|---|
| Feature | #535, then the next feature you name | #543, then the one after |
| How | `wsx task new --hold`. Every follow-up is an `add`, yours from the task sheet or the manager's. No second task on those files | The manager cuts and chains as it does now |
| Ends | You say land | Each task lands by itself |

Measures, per feature, from `work-model-measure.py --tasks wsx#535` (the split arm lists all its tasks):

| Measure | Source |
|---|---|
| Your adds after a result | T1 "owner add after", T5 |
| To check failed | T3 |
| Minutes from start to the last land, and dollars | T1 |
| Your verdict, 1 to 5, as your reply on the feature's last To check item | checks.jsonl `replied` |

Decides: build §4 when, on both pairs, your verdict for the one-task feature is the same or higher, your adds after a result are fewer, and its cost is at most 1.5 times the split feature's. Stop and keep today's model when the verdict is lower on both. A mixed result means two more pairs.

Limits: two pairs prove a direction, not a rate. `--hold` tests one conversation per feature; it does not test the reopen after a land. That part is proven by its own scenario when it is built: land, add inside 24 h, the same `agent_session` answers, a second `landed`, then `closed` with no copy left.

## 7. Knowledge: what the work knows

Numbers: `--project wsx`, tables T7 to T9, from the Claude transcripts of the newest 20 tasks (T7) and 100 tasks (T9, "opened"). All are Claude; a Pi worker writes no such transcript.

**Does context through the manager work?** It is a small part. A worker's first model call holds 95.6 KB (34.8K tokens). The task prompt and adds are 2.5 KB of it. Before its first result the worker fetches 74 KB more by itself, and it names 36 files to change 4. So a task is as good as what the worker finds, not as what the manager wrote.

| Part of the first call | Bytes p50 | Share | Used |
|---|---|---|---|
| Claude Code's own prompt, tool list, other lists | 34.8 KB | 36% | Fixed by Claude Code |
| Index of past tasks (`MEMORY.md`, 164 entries) | 25.5 KB | 27% | 7 of 20 workers named a memory file that was not their own; p50 0 files |
| Skill list | 13.2 KB | 14% | 31 listed, 2 called |
| Your CLAUDE.md | 9.8 KB | 10% | Not measurable |
| wsx's worker rules (`worker_instructions`, claude.rs:230) | 7.0 KB | 7% | Not measurable |
| Task prompt and adds | 2.5 KB | 3% | |
| Project CLAUDE.md | 2.0 KB | 2% | AGENTS.md (28.6 KB) is in no first call; 1 of 20 workers named it |
| Step text | 0.5 KB | 0% | |
| Host's task context: files, open tasks, start block (`task_context`, host.rs:2409) | 0.3 KB | 0% | 7 of 20 tasks got a rule. All 20 rules shown were for files the task changed, which is how they are chosen |

The part that differs by task is 3.3 KB of 95.6 KB. The largest part is a list of every past task. Facts (task memos) are in no start block: a paid evaluation found no gain (host.rs:2442). So the 148 facts serve only the manager's `context.md` and search.

**Stale knowledge, wsx.** The mark memory has, `stale` (start.rs:58: the record's paths changed since its commit), is true for 115 of 128 facts. In a repo that changes daily it cannot say what is past. These marks can:

| What is stored | Count | Past by this mark |
|---|---|---|
| Rotation notes kept as memory records (`remember_rotation`, host.rs:2341) | 145, of them 129 of ended tasks | 509 KB of the 976 KB store |
| Superseded rules, and `decisions-archive.md` (23 KB) | 142 | All, by their status |
| Manager handovers | 38 | 36: all but the newest of each chat log |
| Facts with paths | 128 | 4 with every path gone from the tree; 34 whose paths a newer fact all holds |
| A chat's other note files | 61 files, 1245 KB | 15 are named with a task number; all 15 tasks ended |
| `context.md` | 32.6 KB, at its 32 KB ceiling (manage.rs:39) | The oldest memos move to `context-archive.md`; nothing is deleted |
| docs/design | 58 files, 784 KB | 5 cites of a path git deleted, in 4 files. 46 name only ended tasks; 8 are named by no other file; 5 are both and no worker of the newest 100 tasks opened them |
| Worker memory files (the index's targets) | 177 files, 291 KB | 27 opened by a worker of the newest 100 tasks; 66 name only ended tasks and were not opened |

The store has no delete: `records.jsonl` is append-only and `supersede` only sets a status (store.rs:468). A record a later owner rule contradicts cannot be counted unless someone marked it superseded.

The deletion rule. wsx runs it; no manager decides:

| What | Who deletes | When | Proof |
|---|---|---|---|
| A task's rotation notes record | The task host | At `close` | The task is closed. Its landed commit, memo and `rotate-notes.md` in the task folder stay |
| A superseded rule | `Store::supersede` | At once | The newer rule's slug. The 121 without one (21 of 142 name a successor) go in one pass, and `decisions-archive.md` with them |
| A manager handover | The chat host | When it writes the next | A newer handover of the same chat |
| A fact | `fold_memo` (repo.rs:1268) | At a land | `git ls-files` at the landed commit has none of its paths, or the land's own memo holds all of them |
| A chat note file named with a task number | The chat host | When that task closes | The task's state |
| A tracked `.md` that cites a path a task deletes | The task's checks | Before the land: the check fails until the task changes or deletes the doc | The task's diff |
| The index of past tasks in a worker's start | wsx at launch: `autoMemoryEnabled: false`, which managers already have (claude.rs:465) | Every worker start | T7: 27% of the first call |

```mermaid
stateDiagram-v2
    [*] --> active: add, capture (fold_memo), add_episode
    active --> superseded: supersede (today it stays, in decisions-archive.md)
    superseded --> deleted: new, at once
    active --> deleted: new. close (rotation notes), land (a fact: paths gone or held by the land's memo), the next handover
    deleted --> [*]
```

**Rotation.** A conversation is replaced at 200,000 context tokens (`rotate_tokens`, workspace.rs:273; 204 of 281 by size, 61 after an idle hour).

| | Today (`rotation_prompt`, host.rs:4807) | It should carry | From |
|---|---|---|---|
| Your words | The first prompt, the message the worker is on and the adds still queued. An add it already answered reaches it only through its notes: in 88 of 121 rotations the fresh message does not hold such an add (p50 2.7 KB) | The prompt and every add, in order, unchanged | wsx, the task log |
| Rules for its files | Not sent again | The same start block as the first turn (0.3 KB) | wsx, `task_context` |
| The state of the code | "see them with git status, git log and git diff" | The changed files and the last check result | wsx, the task record |
| Decisions and why, what was ruled out, the next action | `rotate-notes.md`, p50 8.3 KB (at most 16 KB, host.rs:149), which the worker writes at 208K tokens and which must also hold all of the above (host.rs:2329) | Only this | The worker |

After a reset the worker fetches 25.6 KB before its next result (a first conversation: 75.3 KB). 60% of the files it names it had named before, and it names again 54% of the changed files it knew.

Cause or not. The records hold the place in the task equal:

| Result number | Your add follows: no rotation yet | After a rotation | Sent back: no rotation yet | After a rotation |
|---|---|---|---|---|
| 1 | 8% (383 results) | 14% (134) | 25% | 33% |
| 2-3 | 3% (388) | 3% (232) | 11% | 17% |
| 4-6 | 1% (110) | 5% (93) | 7% | 16% |
| 7+ | 3% (31, in 12 tasks) | 10% (20, in 8 tasks) | 6% | 35% |

In the 33 long tasks with both, the rotation came before your first add in 21 and after it in 12. So length alone does not explain it. But a task that fills 200K tokens is a harder task, and the records cannot hold that equal.

§6 tells them apart with a fork. At each rotation of the one-task arm, from the same commit, the next add goes to two conversations: the old one, forked (`--fork-session`, as `hand` does), and the fresh one. The fork runs first on a side branch in the same copy, because Claude finds a conversation by its folder. Same task, same moment, same add; only the reset differs. Measure per pair: checks pass at the first try, your blind pick between the two diffs, dollars. At least 5 pairs. Fresh is worse in most pairs: the reset is a cause; build the carry-over above and run the pairs again. No difference: the task's difficulty is the cause and rotation stays as it is.

How each change is verified:

| Change | Test | Decides |
|---|---|---|
| No index of past tasks for workers | Tasks with even numbers run without it for a week; `--tasks` per arm | Keep it off when "sent back" and "owner add after" (T1) are not higher. The first call drops by about 9.3K tokens (T7) |
| Deletion rules | A scenario per row in `scripts/test-task.py` and `scripts/test-memory.py`; the memory eval before and after the first pass | Recall does not drop |
| Rotation carries your words and the rules | The fork pairs, then T9 again after 100 tasks | The "after a rotation" columns come down to the "no rotation yet" ones |

Changes, lines rough:

| What | Where | In / out |
|---|---|---|
| A `delete` in the store, and the archive views go | store.rs:468; manage.rs:151 `render_views`; docs/memory.md "Store behavior" | +70 / -50 |
| Close, the next handover and land delete by the rules above | cmd/work.rs:497; host.rs:2341; repo.rs:1268; manage.rs:491 | +60 / -5 |
| A check for cites of deleted paths | host.rs, beside the task's checks | +40 / 0 |
| Workers start without the index | claude.rs:458-465 | +1 / -1 |
| `rotation_prompt` sends the adds, the start block and the changed files; the notes request asks for less | host.rs:4807, 2363, 2329 | +35 / -8 |

What the records cannot show here: what a worker did with a rule it followed, and whether a file it read helped. "Used" means the worker named the file in a read, an edit or a shell command.
