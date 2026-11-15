# Owner items that go stale (design)

Status: design only, 2026-11-15, task #467. No code. It answers the owner's request: everything that wsx shows as the owner's to act on stays relevant. An item that a later decision or change makes moot leaves the view with a one-line reason, and nobody clears it by hand.

Assuming: code lines are at `bf5f93fe`. The numbers come from `.work/owner-items-measure.py`, a read-only stdlib script on `~/.local/state/wsx/tasks` (all 15 project stores, read 2026-11-15). The script stays git-ignored in task #467's working copy because this task's output is this file only. Its rules are in §2.

## 0. The answer

**Only open To check items go stale.** The other owner items close by rule today, or they have no stored state. Two small changes cover To check:

1. **Age (wsx rule).** An item open for 14 days is retired, with the reason "open 14 days".
2. **Supersession (manager judgment).** The manager retires a moot item with `wsx task check skip <id> --reason "<why>"`.

In both cases, one event line says why and gives the owner **Reopen**. Approvals stay the owner's.

Live case: c120 (#433, landed) asks the owner to confirm "a short tap opens it; ... a swipe right quotes". In messages 4806-4808 the owner decided that a swipe opens the menu. Reply 4809 put that change into #463. With this design, the manager retires c120 in reply 4809 with the reason "owner chose swipe opens the menu (4807); #463 builds it". No rule could have seen that: #463 has not landed.

## 1. Inventory

| Kind | Owning code | Opens | Closes today | Who closes |
|---|---|---|---|---|
| To check | `task/checks.rs` (`collect` :150, `close` :195) | a task lands with Human Verify items (`host.rs:3968`) | pass, fail, reply, skip (`x` reply or the Skip button) | the owner. The manager may only pass, with `--owner-said` (`cmd/check.rs:36-47`). Skip and fail are owner-only (:55, :60). wsx never closes one |
| Needs you | `status::waits_on` (`status.rs:79`), `needs::stage` | the task state | the task state moves | whoever acts on the task. No item is stored |
| Questions (asks) | `chat::open_asks` (`chat.rs:327`) | a manager reply that ends in "?" | an owner message, `wsx task dismiss`, 7 days of age (`ASKS_WITHIN_MS`, :23) | the owner. `dismiss` is owner-only (`cmd/chat.rs:276`) |
| Approvals | `owner.rs`, `ApprovalLine` | a manager request in `approvals.jsonl` | approve, deny, or a drop by wsx: task closed, revision moved, `after` task gone, replaced (`owner::stale_reason` :247, `manager.rs:849-898`) | the owner. wsx drops stale requests |
| Alerts, session news | `Level`, `session_news.rs` | `wsx task post --level`, a session that exits | never: they are chat lines | nobody |
| Failed routine | Routines tray tone (`routines.js:53-57`) | a failed run | not an item: a tone | nobody |

## 2. Measured

| Kind | Closed how (all stores) | Time open | Stale today |
|---|---|---|---|
| To check, 184 items | 124 passed, 7 failed, 2 replied, 25 skipped, **26 open** | median: passed 0.2 d, skipped 1.7 d. Max: passed **7.2 d**, skipped 11.1 d | **26 open**, all from landed tasks: 14 over 14 d (projy 9, shopfront 5; median 17.5 d), 12 younger (studio-app 9, projk 1, projp 1, wsx c120) |
| Questions, 362 asked | 355 answered (median 0.2 h), 3 aged out, **0 dismissed** | | 4 open now |
| Approvals, 195 decided | 193 approved (163 of them by an owner message), **2 dropped** by wsx, 0 denied | median under 1 min | 0 pending |
| Needs you | derived from the task state | | 3 tasks waiting, median under 1 d |

What the To check numbers say:

- **All 25 skips were the owner's Skip button or CLI** (no `owner_said`, so not an `x` reply; the manager cannot skip). This is the work the owner wants to stop.
- **No passed item was older than 7.2 days.** An age limit of 14 days would have retired 0 items that the owner later passed.
- **Two rules never fire.** "Task discarded" and "newer revision of the same task" hit 0 items: items come only from a landed revision.
- **"Same files" does not separate moot items from good ones.** A later landing on the item's task files came before 105 of 124 passed items and 20 of 25 skipped items. As a flag it would fire on 363 of 673 landings and name 1379 items, 3.8 per landing.
- **"A rule record was added while the item was open"** is true for 17 of 25 skipped items and 42 of 124 passed items. The time open explains it. Nothing links a rule record to an item.

Limits: the questions count uses the `open_asks` rule by quote id or newest. It does not match an old-style quoted line. "Same files" compares the `paths` of the two tasks.

## 3. How an item goes stale

| Cause | Example | Detected by rule? |
|---|---|---|
| An owner decision supersedes it | c120 and messages 4806-4808 | No. The decision is chat words, and memory `--supersedes` links rules, not items. Manager judgment |
| A later landed change replaces its behavior | a later task on `reply.js` lands | Not usefully: the same-files signal fires for 85% of the items the owner passed. Manager judgment, in the landing turn it already has |
| Its task is closed or discarded | | Not possible: items exist only for landed tasks. Approvals already have this rule |
| Age | projy c1-c9, 17-20 d | Yes. `checks.rs`, in the shape of `ASKS_WITHIN_MS` |

## 4. Options

| | A. Rules only in wsx | B. The manager retires, with a reason | C. wsx flags on landing, the manager decides |
|---|---|---|---|
| Catches c120 | no (0 days old) | yes, in reply 4809 | no, not until #463 lands |
| Catches the 14 old items | yes (age) | only if the manager notices | yes, with an age rule |
| Wrong retirements | none: no passed item was over 7.2 d | possible. Reopen undoes one | possible, and noisy: 3.8 items for each flag |
| Manager context per turn | 0 | +~20 tokens of instructions (cached), +~4 tokens per open item on the ledger line | B, plus a flag line on 54% of landings (~40 tokens per item named) |
| Added manager turns | 0 | 0: it acts in the owner-message and landing turns it already has | 0 added, but more tool calls to judge each flag |
| Code | an age sweep | `skip --reason` for the manager, `reopen`, the event line | B plus a path matcher |

**Recommended: A (age only) plus B.** C's flag costs context in most landing turns, misses the decision-time case, and adds a matcher whose signal measured as noise.

## 5. The design

```mermaid
stateDiagram-v2
  [*] --> Open: task lands (checks::collect)
  Open --> Passed: owner Pass, or manager --owner-said
  Open --> Failed: owner Fail (fix task)
  Open --> Replied: owner words
  Open --> Skipped: owner Skip or x
  Open --> Retired: manager skip --reason
  Open --> Retired: wsx, open 14 days
  Retired --> Open: owner Reopen
  Passed --> [*]
  Failed --> [*]
  Replied --> [*]
  Skipped --> [*]
  Retired --> [*]
```

| Part | Owner | Change |
|---|---|---|
| Retired item | `checks.rs` | A `skipped` row whose `note` is the reason. There is no new state: today skip rows keep `note` empty. `reopen` appends an `open` row, and the fold by id (:93) makes it current |
| Age rule | `checks.rs`, run by the host sweep that drops stale approvals (`manager.rs:849`). In M2 it moves to the repo service | open longer than 14 days: retire with "open 14 days" |
| Manager retire | `cmd/check.rs` | `check skip <ids> --reason` is allowed for the manager. A reason is required, and `--all` is refused |
| Event line | the chat that made the item's task (orchestration model §3.2) | "c120 retired: <reason>" with **Reopen** (web) and a key (TUI). Reopen is owner-only |
| Manager knows the ids | `ledger_summary` (`manager.rs:2884`) | "to check: 1 (c120 #433)" instead of only the count. The texts come from `wsx task check` on demand |
| Instructions | `instructions.rs:68` | Change the existing sentence to: "Pass a To check item only when the owner's message of this turn says it looks good; skip one with `--reason` when a later decision or change makes it moot." Net +~85 bytes |

It **replaces** the owner's hand skip of moot items (25 taps so far, 26 items waiting). It adds no list. Option B of `task-views.md` reads only open items for its For you band, so retired items leave For you with no view change. A retired item no longer blocks a release (`cmd/deploy.rs:303`).

**Unchanged**: Needs you (it follows the task state), questions (0 stale: 3 aged out of 362, 0 dismissed), and alerts. Approvals already drop by rule, and **the manager can never retire** land, deploy, release, push, discard, project add, a check Fail, or a Reopen.

Proof at build: a `test-task.py` section with a fixture store. It shows an item aged 15 days that is retired with its line, a manager `skip --reason` with its event line, a manager `skip` with no reason that is refused, and a Reopen that reopens the item. It also shows that `approve` stays owner-only. The web check asserts the Reopen button. `docs/tasks.md` "To check" changes with the build.

## 6. Questions for the owner

| # | Question | Recommendation |
|---|---|---|
| 1 | After how many days does an unchecked item retire by itself? | 14 days. No passed item was older than 7.2 days, and questions use 7 |
| 2 | May the manager retire first and let you Reopen, or must it ask first? | Retire first. Asking first adds the tap you want gone. The reason line and Reopen keep it visible |
| 3 | Should a retired item still hold a release? | No. It is closed. Reopen it if it matters for the release |
| 4 | Should the manager also close its own stale questions (`dismiss --reason`)? | Not now: 0 of 362 needed it |
| 5 | On release, retire the 14 items over 14 days (projy 9, shopfront 5) at once? | Yes, with one line in each project's chat |
