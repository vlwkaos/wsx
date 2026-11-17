# Thread context: what the manager gets when the owner writes in a thread (#494)

Status: design and measurement, 2026-11-17. No product code. Numbers: `python3 -B scripts/thread-context-measure.py port|recall|rotation|cost` (no model call; real logs of this machine, 14 days). Owner words: studio-app msg 995, wsx 5065, 5075, 5076, 5082. Terms (5082): a **topic** is a **thread**, one branch of the one chat; the chat is the trunk. Decided: threads extend the 11-11 decision (one chat per manager, no separate thread window); a thread is a memory record in the chat's scope plus `TaskRecord.topic`; it ends on the owner's word or when the manager closes it with a reason. #491 builds stage 1 and is not touched.

## Recommendation and questions

**Option (a), with a tag written at write time, not found by search.** One conversation. A turn in a thread gets that thread's block (about 700 to 1,500 tokens). A trunk turn gets a one-line index of open threads. Cost: +$9 to +$17 a week at 800 to 1,500 tokens on every owner turn (3.6% to 6.8% of $250). Option (b) models at +$20 to +$98 a week. Option (c) has no evidence. The owner's condition for (a), that search rebuilds a thread, **fails as it stands**: seed search finds 0.00 to 0.60 of a thread. A `topic` field on rows and records, read by key, finds 0.67 to 1.00. That is the one change to the memory system (stage 2 below).

| # | Question for the owner |
|---|---|
| 1 | Block budget 1,500 tokens, open owner asks never cut, other rows newest first, the cut named with its `--topic` query. Agreed? |
| 2 | One message can touch several threads (4806, "yes to all", touched 5). Allow the manager's reply to carry several topics, and leave the owner message in the trunk? |
| 3 | Approve stage 2: a `topic` field on digest rows and records and `wsx memory search --topic`, `wsx task search --topic` (core change, one retrieval path, no new store)? |
| 4 | Approve the rotation replay (section 3, about $5 to $11) after topics have been in use for two weeks? Until then rotation stays at 400K. |

## 1. Options and cost

| | (a) one conversation, thread block per turn | (b) one conversation per thread | (c) mix |
|---|---|---|---|
| First input | unchanged prefix (cache read) + block 0.7K to 1.5K written | 14.3K per thread start (docs/design/manager-context-split.md, median first input) + block | no case found |
| Cache | prefix stays exact: the block sits in the turn text after it; hit rate unchanged (2% of owner turns cold today) | each thread has its own 1 h cache: 38% to 58% of owner turns cold with threads spread at random, 27% if the owner stays on a thread 80% of turns | |
| Cost a week | +$9.0 (800 tokens), +$17.0 (1,500), +$33.9 (3,000) on $250.12 logged | owner turns $181 (3 threads), $209 (5), $234 (8) against $136 today; $157 for 5 threads at 80% stay | |
| Cross-thread facts | in the same conversation | only through memory (`wsx memory`): the same rows, plus a search per turn | |

Method (script `cost`): 1,162 owner turns in 14 days, `bench-manager.py` `manager_turns` and its prices (write $8, read $0.2 per M). (a): the block is written once at the turn and read by each later call of the session (cold gaps pay the write again); it is an upper bound, since a trunk turn gets the index only. (b): same formula both sides (own work + reading the conversation); threads assigned to owner turns at random, 20 seeds; a thread idle over 60 min with over 60K context starts fresh at $0.43 (today's `IDLE_ROTATE_TOKENS` rule); median growth 3,947 tokens a turn. Limits: thread labels do not exist in the log, so assignment is modeled, not measured; no paid replay was run. A block of 1,500 tokens is added to a median growth of 3,947 tokens a turn, so 400K rotations come sooner (a rotation costs about $0.38); that is not in the figures.

**(c)** A mix would promote a long thread to its own conversation. The evidence does not ask for it: the cost of (b) comes from cold turns, and a promoted thread is just as cold. Owner-opened topic chats (`wsx task chat new`) stay what they are.

## 2. Does the memory system rebuild a thread? (owner's condition for (a))

Threads, each at the owner turn named: studio-app #88 (stories, msg 992, buried by #89 to #93), #90 (Korean voice and pacing, msg 991), and two wsx threads of the last 7 days: topics (msg 5091) and the 160K rotation (msg 4806). The hand lists (10, 15, 17, 11 items) are my reading of the chat logs, task logs and records: owner and manager rows by id, tasks, records; only items older than the turn. Recall = list items in the block / list. Precision = list items / block items; it is a floor, since the lists are not exhaustive.

Retrieval is the real BM25 of `memory::index` (the port returns the same first 10 refs as `wsx memory search` on 6 of 6 queries; order differs among equal scores). Reason for a port: a worker's `wsx memory search` reads transcripts only of the project of its folder (wsx), not studio-app's. Records come from the real `wsx memory search --scope <project-id>`. Block = 5 records + 15 rows. Block tokens: ASCII chars / 4 + other chars / 1.7 (an estimate, not a tokenizer).

| Arm | #88 | #90 | topics | 160K | Block tokens |
|---|---|---|---|---|---|
| A0 seed = the owner's message | 0.30 / 0.16 | 0.20 / 0.15 | 0.12 / 0.10 | 0.00 / 0.00 | 880 to 910 |
| A1 seed = thread title + message | 0.60 / 0.30 | 0.20 / 0.15 | 0.47 / 0.40 | 0.18 / 0.10 | 870 to 960 |
| B A1 + joins from the hits (replies to a hit, `#n`, records that cite) | 0.60 / 0.24 | 0.20 / 0.14 | 0.47 / 0.31 | 0.18 / 0.06 | 920 to 1,500 |
| C1 tag on the first row, links the log has | 0.10 / 0.50 | 0.13 / 0.67 | 0.18 / 1.00 | 0.18 / 1.00 | 70 to 130 |
| C2 tag on each owner row, same links | 0.70 / 0.88 | 0.53 / 0.57 | 0.82 / 0.74 | 0.18 / 1.00 | 90 to 620 |
| D C2 + `TaskRecord.topic` | **0.80** / 0.44 | **0.67** / 0.24 | **1.00** / 0.77 | **0.91** / 0.37 | 700 to 1,480 |

Cells: recall / precision. Links the log has: a reply and the messages it answers (`to`), a quoting message and the reply it quotes, an events message and the tasks it names, a reply and a task created within 3 min, a record's `msg:` or `task:` source, two steps from the tagged rows. Earlier eval for comparison: start-set recall about 0.5 (docs/design/memory-eval.md).

**First step that loses the facts: the ranking of the seed, not the index.** In arm A1 every missed row is in the digest and shares a word with the seed, but ranks 29 to 6,900 of the project's rows (examples: 5072 at 6,418, 5076 at 6,911, 687 at 1,222). The owner's turn is short or has no thread word ("yes to all", "where did it go"). Two rows share no token with the seed at all (studio-app 701, 703: "한글에만 있던 썰" against the title). The joins in B start from the wrong hits and add nothing. Korean text is not the cause: CJK pairs are indexed (`each_token`), and the port found Korean hits.

**Smallest fix in the memory system (stage 2):** a `topic` slug on digest rows (message, reply, task, result, history) and on records, set when the row is written, and `--topic <slug>` on `memory search` and `task search` that selects by key before ranking. The thread's block is then a read, not a search. No second store: the digest and the record store stay the only ones.

What D still loses, and the rule that would fix it:

| Lost in D | Why | Fix |
|---|---|---|
| Imported records of studio-app (3 items: `rule-169b…`, `section-6f19…`, `section-56bf…`) | imported from notes: source is a path, no message id | the manager sets `--topic` when it writes or supersedes a record; the import stays untagged |
| #90 rows 708, 964, 973 (a peer message and replies naming #87, #90, #478) | no link in the log; they name two tasks | a reply carries its topics; a peer message names the topic with the task |
| `manager-idle-written-handover` (160K) | owner rule record with source `msg:4645`, in no tagged row | tagged at write, as above |
| Quiet replies (4803) | not indexed by design | the task's `landed` history row carries the fact |

## 3. Rotation (#464)

#464 lost 4 of 14 replays at 160K: pending acts, a hold, review findings, open owner questions (docs/design/manager-event-model.md). Script `rotation`: for the four counted cases (4422, 788, 435, 315), rows that hold the lost fact were in the log before the turn: 2, 2, 29 and 33 rows, newest 23, 0, 0 and 5 min before. The handover (4 exchanges at 600 characters) did not carry them. The terms are a rough test, not proof that the fact is stated.

**Per-thread blocks fix the half that was written** (open owner questions, review findings, task states). They do not fix a hold or a restart the manager only intended: that is why the block has a `next:` line the manager writes in its reply and the rule that a pending act is set before the reply (788 lost exactly this). Handover = the trunk index + the blocks of threads touched in the last 24 h, at most 3 x 1,500 tokens. That is +4.5K tokens written per rotation, $0.036; at 13 rotations a week $0.47, against the $14 to $16 a week the 160K setting saves.

**Replay that would prove it** (not run: it needs tags that the log does not have yet): the same 14 cases and rule as #464 (judge blind, at most 1 counted of 12). Arm C = a fresh start whose handover is the index + the blocks built by the arm D rule from the log cut at the case time, with the tags of the live threads of stage 2. Run the 4 counted cases first, then the rest. Cost: B arm $0.16 to $0.43 a case plus the judge; about $5 for the 14 B arms, up to $11 with the kept control (#464 paid $11.13). Prerequisite: two weeks of topics in use, so the tag coverage (the 0.67 of D) is measured on live rows, not assumed.

## 4. Where an owner message lands

| Order | Owner action | Thread |
|---|---|---|
| 1 | reply or quote of a row | the topic of that row (a reply row has the topics of the messages it answers) |
| 2 | writes in a thread view (the composer names the thread) | that thread, set by the client |
| 3 | answers a To check or task item | the task's `TaskRecord.topic` |
| 4 | none of these | trunk |

wsx picks by this order with no model call. A trunk message may touch several threads; it stays trunk, and the manager's reply carries the topics (one or several), and the host tags the message from them. The owner can move a row into a thread from its menu (stage 1 UI). A quote of an older row follows the row, not the time. What the manager sees:

| Turn | Header | Body |
|---|---|---|
| thread | `Thread <slug> "<title>", open since <date>, <n> open asks` | open owner asks by id (never cut), tasks and states, decisions (records), last rows, then `<k> older rows not shown: wsx task search --topic <slug>` |
| trunk | `Open threads:` | one line per thread: slug, title, open asks, age of the last row; at most 12, about 300 tokens (about $2 a week) |

## 5. Owner rules

| Rule | Status |
|---|---|
| No silent cut (11-05) | the cut is named in the block; open asks are exempt from the cut; open asks over the budget raise it, and the header says so |
| Manager instructions grow by near zero | one sentence (what the `Thread:` header and `next:` line are): about 200 bytes on 7,547 |
| Cost per option | section 1 |
| One name per idea (5082) | thread everywhere; the field is `topic` only in the code |

## Staging

1. #491 (bubble rule, muted lines, peer fold, `add --title`).
2. `topic` on message, reply, task and record, `--topic` on both searches, and the block builder (one function in wsx-core that the host and the web read).
3. Two weeks in use, then the rotation replay (question 4).
