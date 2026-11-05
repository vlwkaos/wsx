# Manager continuity: resume or rotate

Question (owner, msg 2104): is a resumed manager conversation cheap and more continuous than a fresh one built from notes, handover and ledger? No code changed. Related: `docs/tasks.md` "Manager", "Context rotation"; `docs/design/usage-audit.md`.

Data: `~/.local/state/wsx/tasks/wsx-8cbe3f08/manager/log.jsonl`, 2026-10-20 to 2026-11-04, 1036 replies, 74 rotations, $178.56 manager spend (`python3 -I` sum of `reply.usage.cost_micro_usd`). Every number below comes from:

```sh
M=~/.local/state/wsx/tasks/wsx-8cbe3f08/manager
python3 -I docs/design/manager-continuity-measure.py $M cost   # tables 1, 2
python3 -I docs/design/manager-continuity-measure.py $M more   # tables 2, 3 (cold, per call, owner messages)
python3 -I docs/design/manager-continuity-measure.py $M msgs   # section 2 keyword hits
```

Rotation reasons: 51 size, 21 "instructions changed", 2 agent switch (usage limit; left out). The owner's "74 = 21 + rest" holds. "First turn" = the first `reply` after a `rotated`; "resumed" = every other reply (962). Idle gap = time from the previous reply to this turn's start; over 60 min the 1-hour prompt cache is gone.

## 1. Cost

Median per turn:

| Turn | n | cost $ | cache write | cache read | context (last call) |
|---|---|---|---|---|---|
| first, after instructions changed | 21 | 0.355 | 37.5k | 98.6k | 47.7k |
| first, after size | 51 | 0.338 | 30.2k | 194.9k | n/a (old context 169.8k) |
| resumed, all | 962 | 0.093 | 2.0k | 229.4k | 94.4k |
| resumed, warm, context 100-150k | 119 | 0.090 | n/a | n/a | n/a |
| resumed, cold (idle over 60 min), context over 100k | 24 | 0.858 | 97.3k | n/a | n/a |
| resumed, cold, context 60-100k | 20 | 0.565 | 61.4k | n/a | n/a |

A fresh conversation writes about 30-37k tokens (the instructions, notes, handover, ledger; the manager itself says about 37k in reply 2018), and its first turn reads 98-195k because it makes tool calls (recall, ledger). So the first turn costs 3.6x a warm resumed turn.

What a resume would have cost at the context the old conversation had (median of resumed turns within 25% of that context, same warm or cold state; rotations with at least 3 such peers):

| Reason | rotations | actual first turns | resume instead | difference |
|---|---|---|---|---|
| instructions changed | 18 of 21 | $6.68 | $3.90 | +$2.78 (0.15 per rotation) |
| size | 30 of 51 | $10.49 | $4.95 | +$5.54 (0.18 per rotation) |

Over the whole log the 72 rotations cost about $24.55 in first turns; resuming would save about $8 of it, 4.5% of the $178.56. The size rotations also bound the tail: single turns reached 607k context.

Resume is cheap only while warm. 63 of 961 resumed turns (6.6%) came after more than 60 min idle; they cost a median $0.554 (0.858 above 100k context), more than a fresh start ($0.34-0.44). The manager has no idle rotation (`rotateIdleSeconds` is for workers).

Warm cost grows slowly with context (per model call, turns that log `calls`):

| Context | n | $ per call | $ per turn |
|---|---|---|---|
| under 60k | 16 | 0.022 | 0.082 |
| 60-100k | 89 | 0.030 | 0.081 |
| 100-150k | 119 | 0.037 | 0.090 |
| over 150k | 7 | 0.053 | 0.215 |

A rotation costs about $0.2 more once and saves about $0.01-0.04 per later turn (0.015 per call, 2-3 calls), so it pays back in 5-15 turns. The log shows 13 resumed turns per rotation on average. At 150k the size rotation is about break-even, not a clear gain.

## 2. Continuity

Rotations with an owner message before the second reply: 4 of 21 (instructions), 15 of 51 (size). Most rotations happen during events or idle with no owner waiting. Owner messages in the first 3 after each rotation were read by hand (`msgs` mode plus a listing of the first two per rotation).

| Message | After | What happened |
|---|---|---|
| #1019 "I said to give color to the speaker label not the bubble" | size | Real repeat. The manager answered "You're right" (reply 1020) and started #81; it did not ask what was decided. The earlier instruction was not in the last 4 messages. |
| #935 "did you change them again?" | instructions | Owner repeats a UI complaint. Not read in full: I did not check whether the manager knew the earlier one. |
| #433 "we are going round and round" | instructions | Not a memory gap: the owner reacts to a restart loop. Reply 434 gave the full task table. |
| #1147 "why fail", #1297 "what failed", #1399 "all done?", #1654 "is it relased?", #1783 "new version released? 41?" | instructions or size | Referential questions with no context in the message. Each reply (1149, 1298, 1400, 1655, 1784) answered correctly, with the release history, so notes, handover and ledger carried it. |

Result: 1 clear repeat (#1019) and 1 unchecked (#935) in 72 rotations. 0 cases where the manager asked what was already decided (the `msgs` patterns for "no record", "which task", "remind me" found none). The keyword search is a lower bound: a repeat the owner phrased without those words is not found, and a message whose reply was just wrong needs a read.

## 3. Is the handover enough?

Handover: notes at most 10 KiB, with what is left out named (`notes::preamble`, `crates/wsx/src/task/notes.rs`; docs/tasks.md "Notes budget"), last 4 owner messages and replies at 600 chars, routine results, ledger.

- `notes/decisions.md` is 23.5 KB (132 lines) and is read newest first into the 8 KiB budget. Only the newest 50 lines fit, and `context.md` (8.9 KB) comes after, so it gets what is left, usually little or nothing. This is the only structural gap found: decisions from before the last 50 lines, and the project context, are not in a fresh conversation.
- Last 4 messages at 600 chars hold about 2 owner turns. #1019 refers to an instruction older than that and in no note.
- What worked: the ledger and the `landed`/`closed` event memos (the releases were answered right).
- Not measurable here: reasons the manager made a choice a resumed one would not. The log has no record of them.

## 4. Options

| Option | Cost effect | Continuity effect | Verdict |
|---|---|---|---|
| A. Keep: rotate on size 150k and on instructions change | baseline | 1 repeat in 72 | |
| B. Instructions change: keep the conversation, deliver the new text as a message | saves about $0.15 per change, $2.78 over 18 | Better: nothing lost. Risk: Claude keeps the old system prompt on resume (`docs/tasks.md`), so the new text must be the message and may conflict with the old; a long conversation keeps both | Do it when the context is under 100k and warm |
| C. Raise the size limit to 200k | saves about $0.2 once per rotation avoided; warm turns cost 0.09 to 0.215 above 150k (n=7) | Fewer losses | Not now: no data above 150k; 7 turns suggest per-turn cost rises |
| D. Lower to 100k | about -$0.01-0.02 per turn gain, +$0.2 per rotation, roughly 1.5x as many rotations | More losses | No |
| E. Rotate when idle over 60 min and context over 60k | cold resume 0.565-0.858 against a fresh start 0.34-0.44: saves about $0.2-0.4 on 44 turns, about $12 | A rotation more | Yes |
| F. Richer handover: give `context.md` its own part of the budget, 4 KiB each, and have the manager fold old decisions into `context.md` | adds under 4 KiB (about 1k tokens) per fresh conversation, under $0.01 | Closes the section 3 gap | Yes, cheapest fix |

Recommendation: E and F first (measurable gain, small), then B guarded by a warm, under-100k conversation. Leave the size limit at 150k: cost is flat between 100k and 150k per turn and the data above 150k is too thin.

Limits: one project, one owner, cost per turn includes the number of calls (the first turns call recall and ledger), so "resume instead" is a median of similar turns, not a replay. Continuity counts are lower bounds from keywords and a manual read of the first two owner messages after each rotation.
