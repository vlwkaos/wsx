# Seams: where an agent's context, memory or cache is cut

Status: map of 2026-11-24, read from code (file:line as of that day); fixes not built unless marked. Owner ask: find
where memory, model context and the prompt cache cut off before more orchestration work. Agents: L = lead,
G = goal chat, W = worker. Plan of record: [orchestration.md](orchestration.md).

## Context (a conversation ends or is rewritten)

| Seam | Agents | Trigger | Carries over | Lost |
|---|---|---|---|---|
| Size rotation | W | `host.rs` `rotation_due`: context >= `rotateTokens` (200k), also inside a turn on Claude | `rotate-notes.md` (asked 12 KB, read 16 KiB), task prompt, step, waiting turn | the conversation; the start block (rules, area briefs) is not given again |
| Size rotation | L, G | after a reply: context > 120k (L) or 400k (G) | notes (24 KiB), last 8 exchanges x 600 chars, the owner's answers (30, 8 KiB, built 2026-11-24), routines | everything else; **a chat's turn is never checked inside the turn** (`manager.rs:1969`) |
| Idle rotation | all | idle > 60 min and context > 150k (chat) or > rotateTokens/2 (worker) | as above | as above; the lead (limit 120k) never idles out |
| Instructions changed | L, G | digest differs | warm: a message, same conversation; cold: rotation | cold: the conversation |
| Errors in a row | L, G | 3 (`ERROR_ROTATE_ATTEMPTS`) | as size rotation | the conversation |
| **Host restart in a turn** | W | deploy, live upgrade, crash (`host.rs:1019`) | the session id | **the turn: the task waits on the owner** |
| **Claude auto-compaction** | all | inside Claude Code, invisible to the headless hosts (no `compact_boundary` read; the daemon sees it only for terminal sessions, `agent_report.rs:243`) | Claude's own summary | detail; nothing logs it |
| Fork | L -> G | `chat new --brief --owner-said` | one owner message, goal = first line | the lead's conversation |
| New task | G -> W | `task new` | brief (free text, 64 KiB), files, overlaps, start block (4 KiB) | the chat's conversation, the owner's answers, decisions |

## Memory (a fact or decision is dropped)

| Seam | Passed | Dropped |
|---|---|---|
| Owner states a rule in chat | only if the agent runs `wsx memory add --kind rule` | **silently, once past 8 exchanges** (an answer to an ask survives since 2026-11-24) |
| Notes in a fresh conversation | decisions.md first (20 KiB), context.md; cut named | the rule's `text` and `paths` (the view shows `words`) |
| Per-message records | top 5 `rule`/`fact` by BM25 on the raw message, 160 chars | silently: no cut mark; no external backend |
| Worker rotation notes -> task end | episode record while open | deleted at task end; only the memo (240 chars) stays, if non-empty |
| Worker result -> chat | summary 160 chars, memo 600 then 240 | the worker's decisions, notes, left-open items |
| **Goal chat closes** | goal, open tasks, "last decisions" | **always "none"**: views are written only to the main chat (`store.rs:544`), `hand_back` reads the chat's own (`chats.rs:723`) |
| External backend | `wsx memory search/recall` only | never sees writes; never reaches a prompt |
| Owner's skills (/memo) | the worker may write them | wsx never reads them back |

## Cache (the prompt cache goes cold)

| Seam | Agents | Note |
|---|---|---|
| Idle over 1 h | all | workers ping 3 times at 55 min while `checking` only; **chats have no ping** (tasks.md still says the manager pings) |
| `eventModel` turn | L, G | a cache is per model [unverified here]: the event turn and the turn after it read cold |
| Limit wait over 1 h | all | resumed conversation rewritten once |
| Goal set on the main chat | L <-> G | resumed with `--resume`: the system prompt stays the first one; model and limit change [goal text in the prompt: unverified] |

Measured today: cache read and write tokens per turn; `usage-report.py` groups only by idle gap. Not measured:
compaction, cache misses by seam, rules lost.

## Fix order (by evidence that it breaks long work)

1. Owner words survive: the handover carries the owner's own messages of the chat (newest first, a byte budget), not only answers to asks.
2. Measure before more fixes: log Claude's `compact_boundary` and cache write per seam (rotation, model change, restart, wait) in `usage-report.py`.
3. A restarted host resumes the turn it lost (same session, "the host restarted; continue") instead of waiting on the owner.
4. A worker's first prompt carries the owner's decisions and answers that the brief names, or the brief format requires them (orchestration.md step 4).
5. Bugs: goal-chat hand-back decisions; chat keep-warm doc; lead idle threshold.
6. `eventModel` removed (owner's rule: no weaker model in a running conversation).
