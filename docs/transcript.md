# Transcript: what reaches the owner

Contract for the chat's rows (task 491, owner wsx 5072). Topics (its old steps 4 and 5) were dropped: a goal is its own chat now ([orchestration.md](design/orchestration.md)).

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
