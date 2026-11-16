# Manager context split: what a conversation carries and what could load on demand

Status: audit, 2026-11-16, window 11-09 20:09 to 11-16 20:09 KST, all 14 chats. Owner's question (projk chat): fewer turns, smaller context per turn, less idle work. `(est.)` = estimated. Nothing in this doc is built.

**Answer.** Manager spend was $278.7 in 7 days (1,183 turns, 5,225 model calls). A fixed base of about 30K written tokens opens each fresh conversation and costs 36% of conversation cost. wsx's own text is under a quarter of that base. The skill listing (30 KB) and the owner's CLAUDE.md files and memory index are Claude Code attachments that wsx did not write. Recommended: options 1, 2 and 5 below, about $66 a week (est.), 24% of the spend.

## Method

| Item | Value | Source |
|---|---|---|
| Spend, turns, calls | $278.7, 1,183, 5,225 | `reply.usage` in `~/.local/state/wsx/tasks/chats/*/log.jsonl`, rows of the window |
| First input of a conversation | 139 fresh conversations, 240 manager sessions | Claude transcripts `~/.claude/projects/*/*.jsonl` whose first user text holds `Ledger summary from wsx`; fixtures (`demo`, `bench`) left out |
| Prices ($/M tokens) | read 0.20, 1-hour write 8, output 20 | least squares on 361 single-call replies: 0.2008, 7.9996, 19.4. Calculated $273.0 against logged $278.7 |
| Tokens per byte | 0.44 | fit on 116 first calls: write = 0.462 x prompt B + 0.431 x attachment B + 1,362; median error 665 tokens |
| Part cost | tokens x (8 x (1 + cold rewrites) + 0.2 x (calls - 1 - cold rewrites)) | each conversation's own call count; a cold rewrite is a later call that wrote over half its context |
| $/week | part share of the 139 conversations' cost ($226.6) x $278.7 | the rest of the week's calls belong to older sessions |

The measure scripts were throwaway (`.work/t475`, deleted). The recipe above rebuilds them. `turn.json` is a liveness record (`stall::Turn`), not a token source.

## 1. First input of a fresh conversation

`send_turn` (`crates/wsx/src/task/manager.rs:1605`) sends: `notes_preamble` (decisions and context, 10 KiB cap, `notes.rs:147`), the handover (4 exchanges of 600 chars and routine results, `manager.rs:2156`), the queued messages, then `ledger_summary` (`manager.rs:1510`). The instructions go in `--append-system-prompt` (`instructions.rs`). `--setting-sources project,local` (`claude.rs:447`) still loads the user's CLAUDE.md.

Sent once per fresh conversation: 139 in 7 days (20 a day; 111 idle rotations, 33 instruction changes, first starts). Sent again whole on a cold rewrite: 25 turns, $12.0. A token costs 59% write and 41% read over a conversation (mean write $9.1 and read $6.3 per M). Median first prompt: 11,954 B; first call: 39.5K tokens (11.3K read from the shared cache, 28.5K written).

| Part | Median B (mean) | In n of 139 | Tokens (est.) | Share of cost | Write $/wk | Read $/wk |
|---|---|---|---|---|---|---|
| Skill listing | 30,346 | 139 | 13.4K | 12.6% | 20.7 | 14.4 |
| Tools and Claude Code prompt | n/a | 139 | 11.3K | 4.5% | 0 | 12.6 |
| User CLAUDE.md | 9,629 | 139 | 4.2K | 4.0% | 6.6 | 4.6 |
| Project CLAUDE.md | 6,125 (9,023) | 108 | 2.7K | 2.7% | 4.4 | 3.1 |
| wsx instructions | 5,164 | 139 | 2.3K | 2.1% | 3.5 | 2.5 |
| Auto-memory MEMORY.md | 813 (3,897) | 122 | 0.4K | 1.9% | 3.1 | 2.2 |
| First message: context view | 4,064 (3,771) | 136 | 1.8K | 1.7% | 2.8 | 1.9 |
| First message: decisions view | 3,948 (3,601) | 131 | 1.7K | 1.6% | 2.5 | 1.8 |
| Agent listing | 3,661 | 139 | 1.6K | 1.5% | 2.5 | 1.7 |
| Environment, model, date, deferred tools | 3,278 | 139 | 1.4K | 1.4% | 2.3 | 1.6 |
| Project docs beyond CLAUDE.md | 8,019 | 26 | 3.5K | 0.5% | 0.8 | 0.6 |
| First message: handover | 826 (1,024) | 135 | 0.4K | 0.4% | 0.7 | 0.5 |
| First message: routines, owner, events, matches, ledger | under 1 KB each | 44 to 139 | under 0.1K each | 0.7% | 1.1 | 0.8 |

CLAUDE.md files vary by project: projk 9.9 KB, wsx 19.7 KB (user, project, MEMORY.md), ir 39.4 KB (adds `research/pipeline.md`). The skill listing is 30.2 to 30.5 KB in every session (97 skills in the sample: the owner's skills as `owner:<name>` and the plain ones).

Spend by chat: wsx $142.0 (528 replies), pr-review 40.3 (141), studio-app 32.0 (156), ir 28.0 (173), shopfront 10.4, dotfiles 8.1, projk 6.7, seven others 11.1. Cost split of all replies: read 44%, write 38%, output 18%.

## 2. Per turn

| Measure (918 later turns) | Median | p90 | Max |
|---|---|---|---|
| Whole delivered block | 955 B | 1,794 B | 29,345 B |
| Events text | 233 B | 995 B | 3,814 B |
| Ledger summary | 203 B (18% of prompt bytes) | 310 B | 729 B |
| `since then:` line | 0 | 0 | 167 B |
| Notes matching an owner message | 0 | 985 B | 1,363 B |
| Context growth per turn | 3,901 tokens | 14,033 tokens | n/a |
| Model calls per turn | 3 (mean 4.4) | n/a | n/a |

Growth per turn: the prompt block 9%, the manager's output 31%, tool results and the rest 60%. The block is small. A fresh conversation's ledger summary lists every open task (first turn median 184 B, max 836 B); later turns list only changed tasks.

Tool results by command, 147 sessions (est., written once and read to the end): git $15.6 (464 calls, median 2.4 KB), rg 8.3, `wsx task show` 6.3, sed 5.6, grep 4.6, Read 3.3, `wsx task ledger` 3.2 (151 calls: the summary does not replace it), cat 2.9. Total $61.0.

Conversations are short: median 4 turns, 20 calls, mean 32.6 calls, $1.63. Idle rotations leave at median 84K context (p90 146K); 6 other rotations reach p90 403K.

Turns by kind: owner $117.5 (432 turns, median $0.21), events with a loud reply $97.5 (358, $0.23), chat peers $37.5 (173), events-only quiet $18.7 (173, $0.07, 2.4 calls), peers quiet $1.8 (47). No chat idles with a ping any more: see section 3.

## 3. Idle work

| Item | Count in 7 d | Cost | Note |
|---|---|---|---|
| Pings | 40 | $1.39 | Rows from older hosts; the code went in a09d5fc8 (11-16 15:09). Nothing left to save |
| Idle rotations (over 60 min, over 60K) | 111 | no rotation would cost $90.3 to rewrite; fresh starts cost $27.0 | They save $63.3 a week: keep them |
| Instruction-change rotations | 33 (8, 5, 7, 5, 5, 1, 2 a day) | 33 x 30.4K x $8/M = $8.0 | Cause not traced to the edits |
| Cold turns without rotation | 25 | $12.0 | Contexts under 60K and limit resumes |
| Host restarts | not logged | none in tokens | First-call write: 752 tokens (574 turns after under 5 min), 905 (327 turns, 5 to 60 min). After over 60 min: 40.4K, $0.33 (17 turns) |

## 4. On demand: option per part

The cache rule: a conversation's prefix is tools, then system (our instructions), then messages. A change in the system block or in the first message rewrites all after the tools prefix once (about $0.24 a chat, $3.4 for 14 chats). A running conversation keeps the first block it began with, so a saving starts at its next fresh start.

| # | Option | $/wk (est.) | Quality risk and evidence |
|---|---|---|---|
| 1 | Skill listing off (`--disable-slash-commands`); one names-only line (about 77 names, 1.2 KB) in the instructions; the manager reads `SKILL.md` when it needs a description | 33 | Low. 3 `Skill` calls in 7 days (`be-ambitious` x2, `ko-explain-voice`). 32 of 996 `new`/`add` commands pass `--skills`, so the names stay |
| 2 | Owner CLAUDE.md files, project docs and MEMORY.md off for managers (`claudeMdExcludes`, `autoMemoryEnabled: false` in the `--settings` JSON) | 25 | Medium. The manager follows owner rules from these files now. Instructions repeat "root cause first" and the presentation rule, not the rest. A bench replay must show no loss |
| 3 | Tool results of over 2 KB through a Sonnet subagent | 15 | Medium. Only 8 `Agent` calls in 7 days. The manager would see summaries, not the evidence its Facts rule needs |
| 4 | Decisions and context views read when needed | 9 | High. #392: start-block records gave no gain, and these hold the owner's rules (`--owner-said`). No gain proven either way |
| 5 | Defer the instruction-change rotation to the next idle rotation | 8 | Low. A fix to the instructions reaches a busy chat later, by hours |
| 6 | Hold events-only turns and merge them | 5 | Low. #461 measured $5.09 in a 7-day replay |
| 7 | Agent listing off (`Agent` tool disallowed) | 4 | Low, but excludes option 3 |
| 8 | Handover on demand | 1 | High. #464: a 160K rotation cut open threads in 4 of 14 cases |

## 5. Recommendation

1. Option 1: biggest saving, lowest risk.
2. Option 2: second saving. Ship only after a real `claude -p` first call with each switch (about $0.25 each) shows the text gone, and `bench-manager.py` (about $1 a case pair) shows no change in replies.
3. Option 5: small, low risk, no new mechanism: the digest check in `send_turn` already exists.

Not recommended: option 3 (uncertain saving, loses evidence), 4 and 8 (high risk, past evidence), 6 (measured small), 7 (blocks option 3).

Unproven: that the three switches in options 1 and 2 remove the text. Both exist in the Claude Code 2.1.316 binary (`strings`, `--help`); nothing here ran them.
