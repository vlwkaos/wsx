# Transcript: what reaches the owner

Task 491. Owner ask (studio-app 995, wsx 5065): in a chat, show at a glance whether what they asked for is carried out. Plan approved in wsx 5072. **Built: steps 1 to 3** (rule, muted tokens, peer fold, `add --title`). **Waiting: steps 4 and 5** (topics) for the owner's answer on the tree-context design, which is its own task.

Measure: a stdlib, read-only script that printed totals only (since deleted). It mirrors the old rule (`before`) and the new rule (`after`) over the chat logs of the last 7 days (to 2026-11-17).

## 1. Cause

| Where | At c14c9e34 | Proof |
|---|---|---|
| A reply to an event or a peer becomes a bubble | `ManagerEntry::read` (`manager.rs:469`) mutes a reply only when `quiet_reply` holds: one line, at most 200 characters (`manager.rs:283`). A longer reply to an event is a bubble on purpose (`docs/tasks.md` "Manager"; a routine event for the owner gets a normal reply). `chat_rows` (`chat.rs:950-965`) then draws every non-quiet reply as a bubble, also one to another chat | studio-app replies 984 and 986 answer a peer (`chat:web-income`, `chat-reply:wsx`), 234 and 177 characters: bubbles. wsx 7 d: 417 replies, 43 more quiet, 61 to peers |
| A peer message is a bubble | `chat_rows` draws `Message{from: peer}` (`chat.rs:802`) and `Sent` (`chat.rs:1005`) as a bubble each | wsx 7 d: 64 peer messages + 70 sent = 134 bubbles; studio-app 35 + 35 |
| An empty `-` reply | Not a cause here. `says_nothing` marks it quiet at read (`manager.rs:469`) and the quiet branch dropped it: studio-app 988 and 990 (`quiet: true`, text `-`) never drew. The owner saw them in an older build or as a peer text; `talk()` now drops those too | log rows 988, 990 |
| A task's scope outgrows its title | `HostRequest::Add` (`host.rs:1053`) logs the text and delivers it. No code path writes `record.title` after `task new` | studio-app #90 "Revoice unclaimed teaser": 1 prompt, 6 adds, 1 steer; message 991 (pause and caption rule) is the 5th add (task log, ts 1794872759653). Store, all tasks: studio-app 50 of 110 got an add (16 got 3 or more); wsx 319 of 493 (138 with 3 or more), median 1 KB of added text |
| A request is buried | #88 (stories) landed 11-10 as a brief; #89 to #93 filled the chat; message 992 asked "where did that go". No row ties #88 to the request (697, 701, 703). Topics, step 4 | studio-app log |

## 2. Row rule

One rule, one owner: wsx-core `task::chat::chat_rows` (`reply_row`, `talk`, `push_row`). Web and TUI draw the rows; neither decides.

| Row | Shown as | Why |
|---|---|---|
| Owner message | bubble | the request |
| Reply to an owner or recap message | bubble | the answer |
| Reply that asks, carries an approval, is a post, or has a level | bubble | the owner must act |
| Reply to a peer only, or a short reply to events (`quiet`) | quiet line: tokens, first sentence, **More** (opens the whole text) | wsx's own work |
| Reply to events that is long or for the owner (not `quiet`) | bubble, as before | the manager's own rule at write time (`turn_events`) |
| Peer message or `sent` | one `Talked to <chat>` line per run with that chat, **Show** opens the words | traffic between managers |
| Empty or `-` reply, empty peer or `sent` text | no row | says nothing |
| Event | muted line (tokens when the event names its state), runs fold as before | unchanged |

Counts, last 7 days, bubbles before to after:

| Chat | Owner | Replies before | Replies after (bubble / quiet) | Peer + sent bubbles before | Talked-to lines | Bubbles |
|---|---|---|---|---|---|---|
| wsx | 181 | 417 (+43 quiet) | 383 / 77 | 128 | 96 | 728 to **566** |
| studio-app | 59 | 131 (+10 quiet) | 111 / 30 | 70 | 60 | 260 to **170** |
| all chats | 444 | 954 (+120 quiet) | 834 / 240 | 402 | 316 | 1,824 to **1,302** |

An open ask or an approval keeps its reply a bubble; the script cannot see them, so the bubble counts after are an upper bound. 10 empty replies show nowhere before or after.

Corrections to the approved plan: the plan counted the 10 empty `-` replies as bubbles before; they were not (table above). A first version of the rule muted every reply to events; the task scenario showed that hides a routine event's reply for the owner and the long replies the manager writes on purpose. Only a reply to another chat's words is muted by who it answers; a reply to events keeps the `quiet` flag (counts above).

## 3. Muted lines carry tokens

A muted line is not clipped prose. wsx-core builds `Token{task, state, tone, checks}` from the event's `Short` facts (`Short.state` and `Short.checks`, written by `repo.rs` with the state-change event), never from the manager's words. A quiet reply shows the tokens of the events it answers (one per task, at most 4).

| Token | Web | TUI |
|---|---|---|
| Task `#n` | `TaskChip` link | accent `#n` |
| New state (`landed`, `ready`, `failed`, `waiting`) | status chip, tone from `task::status::tone` | bold word in the tone's color |
| Checks | `2 of 2 checks` chip, danger tone when one failed | same words, red when one failed |
| Topic | step 4 | step 4 |

An event logged before this change has no state: it draws as before.

## 4. Peer messages

Another chat's `say` and the replies to it fold into the chat's `Talked to <chat>` line, in the chat of the project where they arrived, both ways. The opened text names each speaker. A reply to a peer is a quiet line (tokens when it names tasks). An `-` or empty text never draws.

## 5. Title drift

Rule: the title names what the task carries. `wsx task add <n> --title <words>` sets the title in the host in the same step as the add (`checked_title`: 1 to 60 characters, no control characters); a bad title refuses the whole add. The manager instruction line "Similar work goes to the open task with `wsx task add`" gains "(`--title` when it now carries more)", 38 bytes. No add is refused for its words, and no word-overlap gate exists (owner decision, 5072). A new task is the manager's choice, as before.

## 5a. A quote means "this answers that"

Cause (chat ir, owner message 511, replies 504 and 508): 504 was an events-only turn, and `chat::quoted_message` rule 2 quoted the manager's own reply 499 (it named #22 in passing) with no author shown, so it read as an answer to 499. The agent copied that quote into 508 (`> message 499`), and `resolve_quote` took any reply that says something.

Rule, one owner (`chat::quoted_message` chooses, `resolve_quote` checks): a manager's quote names only a message the manager answers, the owner's or another chat's. An events-only turn gets no quote (its muted line names the task). An explicit `> message <id>` of a manager reply or an events message is dropped with the note "no owner or chat message <id> to quote". The owner's Reply on a manager reply still quotes it. The prompt says "of the owner's message you answer"; the Open questions header says "unanswered: do not act on one until the owner answers".

## 6. Topics (steps 4 and 5, not built)

Owner decisions: topics extend the 11-11 decision (pills, quotes, search); no thread feature. A topic is done only on the owner's word or a manager close with a reason, never because its tasks landed. States shown: `asked` (no task), `in progress` (an open task), `done`, `dropped` (closed with a reason).

| Part | Plan |
|---|---|
| Store | a memory record in the chat's scope (kind `fact`, slug `topic-<name>`, `keywords` in the owner's words, `source: msg:<first owner message>`); tasks carry an optional `topic` slug; no new store |
| Who | the manager creates and names it on an owner request that no topic holds; the owner renames or closes it |
| Attach | `task new --topic <slug>`, inherited by `--from`, `--after`, `--into`; an owner message attaches by quote or by the manager's word |
| See | a Topics pill in the composer (like the routine pill), a band on Home; the topic beside the task's title in the lists |
| Context | which context a turn gets for a topic, and what a rotation keeps: the separate tree-context task decides; steps 4 and 5 wait for its answer |

## 7. Build steps and proof

| Step | Files | Proof |
|---|---|---|
| 1 Measure and doc | the measure script (since deleted), this file | the script's totals above |
| 2 Rule, tokens, peer fold | `wsx-core task/chat.rs`, `manager.rs` (`Short.state`, `.checks`), `status.rs` (`tone`), `repo.rs`; web `bubbles.js`, `tasks.js`, `app.css`; TUI `task_chat.rs`, `ui/task_chat.rs` | web scenario (`web-tasks-check.mjs`, `test-web-client.py` chat part): a muted reply to `#93 landed` shows the `#93` link, the `landed` chip in the accent tone and `2 of 2 checks`; `Talked to ui` opens to the words; smoke (`runtime-smoke.py`): TUI rows `#3 landed 4 of 4 checks` and `Talked to elsewhere  Show`, `test-task.py`: `Talked to demo` in a chat's TUI. Captures at 375, 390 and 1280 px, light and dark: web check-run only |
| 3 `add --title` | `args.rs`, `cmd/work.rs`, `host.rs`, `HostRequest::Add.title`, `instructions.rs`, `docs/tasks.md` | `test-task.py` 14b: title set, listed, a 61-character or empty title refused, nothing sent |
| 2b Quote rule | `chat.rs` `quoted_message`, `manager.rs` `resolve_quote`, `Manager::quoted`, `instructions.rs`, open-questions header, `docs/tasks.md` | `test-task.py` quote section: a manager quote of its own reply is dropped, an events turn has no quote, owner quotes resolve |
| 4 Topics | record, `TaskRecord.topic`, `task new --topic`, `Short.topic` token | after the tree-context answer |
| 5 Pill and Home band | web `tasks.js` pills, TUI, Home | web and TUI asserts with captures |

## 8. Questions for the owner

1. The topic's name: the manager writes it from the first message; you rename it. Recommend: yes (it needs no extra step from you).
2. A topic with no task for 14 days: stays `asked`, shown in the band with its age. Recommend: yes (a quiet topic is the #88 case; hiding it is the bug).
3. Wait for the tree-context answer before steps 4 and 5. Recommend: yes (what a topic turn reads decides what a topic record must hold).
