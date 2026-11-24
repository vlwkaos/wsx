# Orchestration: chats, the lead and workbenches

Status: decided 2026-11-23 (owner), partly built (section 7). This file holds the decisions and the open plan. What is
built is the contract in [tasks.md](../tasks.md); a built part leaves this file. Memory: [memory-layers.md](memory-layers.md).
Earlier text (evidence tables, approaches A and C, the walkthrough, `lean.md`, `verification-pipeline.md`, `scale-out.md`)
is in git history: `git show 9d983952:docs/design/<file>`.

## 1. Why (measured 2026-10-28 to 11-20)

- The gate was the bottleneck, not the hierarchy: check waits were 49.8% of a task's p50 89 min; worker turns 9%.
- 227 of 318 `web` runs failed, 108 fixed only in `scripts/`: a pass meant "the script matches", not decided behavior.
- Injected start-block facts recalled 0.07 (target 0.60); 94% were hidden as stale. Hiding was the loss.
- A follow-up as an add took 17 min and $0.93, as a new task 43 min and $2.05: continue a line of work; fork only a new goal.
- Event-only turns were 49% of manager spend. Concepts rose from about 6 to 20 while owner messages stayed about 30 a day.
- Outside: "most coding" is a poor fit for multi-agent work (Anthropic); one linear agent by default (Cognition); every
  shipped tool hands over one artifact per unit of work.

## 2. Goal and judge

- Goal: the owner ships across many projects with less attention than one focused session; the machine never fills up.
- Targets: a small change lands in p50 15 min or less (65.9 min measured); no LLM turn started only by an event; disk
  bounded by copy-on-write and a workbench cap; 5 concepts or fewer; 10 or fewer decision items a day.
- Judge: a logged day's owner messages replayed through the router and engine with the fake agents
  (`scripts/fake-claude-stream.py`, `fake-pi-rpc.py`): routing against hand labels, LLM turns, peak disk, time to proof.

## 3. Names

| Name | Meaning | Code |
|---|---|---|
| chat | one goal, one agent conversation, as capable as a single session | `wsx_core::task::chats` |
| lead | the project's main chat with no goal: takes project messages, forks every goal | `chats::runs_on` |
| workbench | a chat's or task's working copy: a worktree cloned copy-on-write with its `target/` | `copy::create` |
| desk | the project's code side: memory scope, mailbox, policy, events, workbench cap | grows from `wsx task repo-host` |
| slot | the machine's check lane, only that | `crates/wsx/src/task/slot.rs` |
| proof card, inbox | one artifact per unit of work; the one place for what needs the owner | not built |

## 4. The design

```
owner message -> router: rules (/new, @chat, reply-quote), then a small model (stay | chat | new)
  same goal   -> current chat
  new goal    -> fork: a new chat; an aside's brief is the message, a part's brief is written by the current chat
  other project or mail -> that project's lead -> fork
chat -> workers (subagents and tasks) -> workbench -> proof card -> inbox: Decide | Review | FYI
```

- A chat is synchronous while the owner is in it and background when the owner leaves; an add within 24 h reopens it.
- The lead never watches events; code handles them. It takes a turn only for the owner or another project's lead.
- A role is a desk profile (path globs, skills, model, memory area); a chat can open at a role.
- A multi-day goal keeps `plan.md` in the desk record and forks a child chat per part.
- Session mode stays one key away; "take over" attaches to a workbench's session.

## 5. Owner's decisions (2026-11-23)

1. **Lead: yes.** One always-open lead per project; it forks every goal.
2. **Router: automatic with undo.** Rules first, then Haiku (or the local model while Claude is limited) reading only
   the message and the open chats' goal lines. A moved message dims in place under "⑂ moved to *name* · undo · open";
   focus stays; a long press moves any message by hand.
3. **Landing:** `local` or `pr` per project.
4. **Ask items, closed by code only.** Every suggestion, question or approval is one inbox item with a basis (subject,
   commit, files); prose cannot ask. Code closes it when a newer item of the same chat and subject replaces it, when a
   land changes its basis files, when its chat ends, or after 3 days; at most 20 open per project; closed items stay
   30 days. A stricter contract (asserts only added) lands as FYI; a changed assert is Decide. **Autonomy is off by
   default**; on, a higher-tier reviewer may approve a changed assert, and the owner gets it as Review with a 24 h revert.
5. **Names:** workbench, desk; slot keeps its one meaning.
6. **Preview: designed again** in step 5; the old `task preview` is removed.
7. **Command surface: the minimum.** Each step removes the commands its replacement makes unused.

Owner, 2026-11-24: the lead runs on its own lighter model and context limit (`tasks.lead.model`,
`leadRotateTokens`); "manager" becomes "lead" in every name (config `tasks.chats.*`, `tasks.lead.*`; stored values stay).
Owner, 2026-11-24: the CLI splits `wsx task` by its three parts: `wsx chat` (say, read, new, close, rename, main, search,
approve, deny, dismiss, checkin, lead, ledger), `wsx task` (the task's own life: new, list, show, why, review, add, stop,
after, hold, hand, move, land, close) and `wsx project` (the desk's commands: add, push, sync, commit, deploy, release,
deployed, check, run, routine; "desk" stays the internal name, the screens and the CLI say project). Project routines
sit at `wsx project routine` until decision 7 merges them with the machine's `wsx routine`. Reading a chat is
`wsx chat read`. An old form is refused with its new name.

## 6. UX rules

| # | Rule | Measure |
|---|---|---|
| 1 | Every ask is an inbox item, never only a chat bubble | items answered in the inbox vs. in chat |
| 2 | Attention budget: only Decide pushes, batched per 10 min; Review is a badge; FYI never interrupts | 10 or fewer Decide a day |
| 3 | Sync or async is where the owner is: the chat on screen streams; on return, "since you left" in one line | no toggle exists |
| 4 | A fork never takes focus: a chip names it and undoes it | undo and move rate under 10% |
| 5 | Status is a word from code, the same in web, TUI and CLI | "status?" messages near 0 |
| 6 | Proof before prose: a Review item opens on its proof card; Land is the primary action | time from open to land |
| 7 | Options are choices: captures side by side, or a trade-off table; a pick is a message | picks made without typing |
| 8 | A preview is a link with a state (live, starting, stopped) while the workbench runs | opens on the first tap |
| 9 | Coming back: Decide, then Review, then one FYI line per changed chat | taps to clear after 8 h |
| 10 | A fan-out is one row: one origin, N child chats, counts per state | one row per origin |

Push timing (from the 2026-10 attention notes): by urgency, never on a fixed schedule; Decide at once within its
batch, everything else in the digest.

## 7. Steps

| # | Step | State | Open |
|---|---|---|---|
| 1 | Gate: contract files, scenarios by path, triage | partly: `tasks.contracts`, `scenarioPaths`, and a failure two tasks share told as "not yours" (`task::failure`, tasks.md "Shared failure") built | a nightly full run on main that starts a fix task, then a single task's red judged against main's last result; a failed part reruns once and is counted flaky; later: web parts by area, results keyed by input hash |
| 2 | Workbenches, warm by copy-on-write | built (7bf75497; measured 5 builds 2.8 GB against 11 GB) | the cap per machine and the idle drop of `target/` (step 3) |
| 3 | Chat engine on one phase table; desk as code; no event turns | partly: task events and shared failures start no turn (99a45b5d) | `next(phase, event)` in `wsx_core::task::phase` replacing about 20 state functions; approvals, deploys, routine ends and recaps still start lead turns; one test `Fixture` and the no-screen scenario sections moved into nextest; browser checks opt-in |
| 4 | Router and fork with brief | partly: rules, optional model, fork with the owner's message as brief, undo (0d2f90c8, e2a5d669, 66b38ffc) | the model move's undo; a part's brief written by the current chat; replay accuracy 90% on hand labels |
| 5 | Proof card, preview on a leased port, inbox | not built; mockups first | the inbox projection (`inbox`, `chat_rows`) in wsx-core, checked against a replayed day before any screen |
| 6 | Mail between desks | built (66b38ffc) | |
| — | Agents (section 8) | partly: no switching | re-dispatch to a peer as a Decide item (needs the inbox); tiers; circuit breaker; provenance |
| — | Deletions (decision 7) | partly | `manager-host` and `run_old`, `.gtrconfig` and old `config.toml` imports, old web `#/p/...` routes; merge `wsx routine` into `wsx task routine`; three ways to add a project into `ops::add_project` |

## 8. Agents, fallbacks and limits

Owner, 2026-11-23: a fallback model must not take over a running conversation; a weaker model can ruin the work. It may
run as its own role with its own permissions, for basic operations and lookups.

Built: `[[agents]]` with `may`; `Policy::of(trust, role, agent)`; trusted and untrusted Seatbelt profiles; a limit
record per account; the stand-in (`organizer` role); the local floor (`floor.rs`); no switching (2026-11-24: a limit
or a failed sign-in is waited out on the own agent, a sign-in wait pushes; `Next::Home` stays only for records an
earlier wsx left away).

| Part | Rule |
|---|---|
| Catalog | each entry: client, model, account, trust, cost and tier |
| Tier | Read (search, status, summaries), Operate (wsx commands that change no code), Build (edit in a workbench, make a proof card), Lead (open chats, land under policy); passed by an eval set with an outside judge, rerun when the model id changes |
| Permission | a chat may do its role's needs, cut to its agent's tier, inside its trust profile |
| No switching | a chat keeps one agent for life; on a limit it waits and resumes the same conversation |
| Stand-in | the cheapest Read or Operate agent answers lookups in a side lane, marked "stand-in, not checked"; on resume the chat gets them as unchecked notes |
| Re-dispatch | a long wait becomes a Decide item: "continue on *peer* as a new chat?"; the new chat gets a brief built by code: goal, plan, the workbench diff as a work-in-progress commit, the last proof, the owner's messages |
| Lower tier | its proof card lands only after a review by a higher tier or the owner |
| Circuit breaker | 3 undone or rejected results in a day drop one tier until it passes its eval again |
| Provenance | every message and proof card shows its agent and tier |

## Sources

Anthropic multi-agent research system; Cognition "Don't build multi-agents"; MAST (arXiv 2503.13657); Claude Code agent
teams; Devin knowledge by trigger; OTP supervision restart budget. Links in git history.
