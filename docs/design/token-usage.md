# Token usage: where Claude spend goes and what cuts it

Measured on 2026-11-04. The window is the tasks created from 2026-10-28 (85 Claude tasks, 6,869 model calls, $391), plus the manager.
Prices are API-equivalent: Opus 5.5 at $4 input, $20 output, $0.20 cache read and $8 1h write. Sonnet 5.5 at $2, $10, $0.10 and $4 (corrected 2026-11-04 from $0.20: cache reads cost 0.05x input on both models; the tables below keep the first run's numbers).
Assuming: Haiku 5.5 at $1, $5, $0.10 and $2 (not verified).
The baseline already includes #182 (context and memo steps folded, `--size small`) and #183 (a queued check-run answers at once).

## Result

- Workers already hit the cache for 98% of input tokens. Hit rate is not the problem: the cost is calls × context, plus the writes of new tokens.
- No single cache trick gives a large factor. #182 removes about 22%. The three changes below remove about 16% more, so the total is about 1.5x.
- A factor of about 2x also needs fewer Opus calls (option 5), and that choice is the owner's.
- The owner's example, a shared byte-identical prefix for parallel workers, is already in effect for 11.8k tokens. It can grow by at most about 2.5k tokens: worth about $3.5 in the window.

## Measurements

Every number comes from `docs/design/token-usage-measure.py` (run with `/usr/bin/python3 -I`) unless the row says otherwise.
It reads `~/.local/state/wsx/tasks/wsx-8cbe3f08/<n>/{task.json,log*.jsonl}` and the transcripts at `~/.claude/projects/<working copy path with / as ->/<agent_session>.jsonl`.

| Fact | Number | Source (script part or query) |
|---|---|---|
| Cost split | reads $166 (42%), writes $151 (39%), output about $80 (20%) | part 1: 828.5M read, 19.8M write, 4.3M out |
| Context per call | p50 112k, p90 211k, mean 123k | part 1 |
| Prefix at the first call | 33.3k: 11.8k read from cache (shared by every worker), 21.5k written per task | part 1, first-call p50, n=116, 20 cold |
| What fills the per-task prefix | skill list 8.9k, CLAUDE.md and AGENTS.md 2.9k, agent list 1.0k, appended worker prompt about 2.5k, prompt 0.5k | t192 transcript `attachment` records, chars/4 |
| Prefix share of all reads | 33k × 6,869 calls = 229M, 27% of reads | derived |
| Tool results carried | 210M token reads ($42, 25% of reads): rg/sed/cat/git reads $22.2, other Bash $15.3, Read $2.4, cargo $1.0, check-run $0.9 | part 1, "carried" = size × later calls |
| Cache writes by cause | incremental $88.6 (59%), session start $34.6 (23%), rewrite after a limit $15.5 (12 calls, all after `<synthetic>`), rewrite after >1h $12.4 (14) | part 2 |
| Gaps between calls | 6,088 under 5 min, 574 at 5-60 min (no rewrite: the TTL is 1h), 26 over 1h | part 4 |
| Context at step start | plan 67k, build 72k, review 93k, memo 97k | part 3 |
| Turns per task | p50 5, mean 6.4. Triggers: step 204, untagged (older) 188, restart 68 ($18.5), check_run 34, message 29 ($37.4), rotation 27 ($15.3), limit 8, conflict 6 | part 1 counts, part 4 cost by trigger |
| Step cost (tagged turns) | context $56.0, build $52.3, memo $29.5, review $23.8, plan $19.3 | part 1 |
| Skills used by workers | 770 invocations: simplify 130, recall 126, develop 122, memo 121, backpressure 108, ui-view-standards 47, all others 16. 76 skills are listed | `rg '"name":"Skill"'` over worker transcripts |
| Subagents | 6 Agent calls in 6,869 main calls; their 33 calls cost $1.39 | part 1 (`<session>/subagents/*.jsonl`) |
| Models | Opus 5,236 calls, Sonnet 1,479, Haiku 12 | part 2 |
| Big tool results | every result over 8k tokens is a `Read`, mostly images. Bash output is already middle-truncated | part 4 (72 results, all `Read`) |
| Manager | 423 turns, $69.8, context p50 96k. Rewrites after >1h: $13.6 (20) | manager `log*.jsonl` usage; part 2 logic on the 21 sessions in `~/.claude/projects/<main checkout path with / as ->` whose first 20 kB hold "manager" and "wsx task" |

## Research (searched 2026-11-04)

- A prompt cache entry is keyed by the exact prefix bytes and the model. A 1h entry costs 2x input to write, a 5m entry 1.25x. A read costs 0.1x of 4.x prices, but 0.05x of 5.5 Opus prices. Changing the model inside one session rewrites the whole context.
- Context editing (`clear_tool_uses_20250919`, beta `context-management-2025-06-27`) clears old tool results on the API side. I found no Claude Code CLI switch for it.
- Claude Code documents `BASH_MAX_OUTPUT_LENGTH` and `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` (compaction at about 95% by default). Third-party sources name the TTL settings differently, so those names are not verified.
- Sources: [context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing), [Claude Code env vars](https://code.claude.com/docs/en/env-vars), [claude-code #32671](https://github.com/anthropics/claude-code/issues/32671), [Portkey prompt caching](https://portkey.ai/docs/integrations/llms/anthropic/prompt-caching).

## Options, ranked by saving per build cost

Savings are for the 10-28..11-04 window ($391 workers plus $70 manager). Build cost: S is under 50 lines, M is a host change with a test.

| # | Option | Saving | Build | Quality risk | Measure after |
|---|---|---|---|---|---|
| 1 | Skill allowlist: `skills_plugin` (`claude.rs:73`) links only the skills a project names into `skills-plugin/skills/` (about 8 skills, not 76) | skill list 8.9k → about 1.5k: $10 reads + $11 start writes + $2 manager = **$23 (5%)** | S | low: 98% of invocations use 6 skills; a missing skill is visible in the result | part 1 first-call write p50 falls from 21.5k to about 14k |
| 2 | Keep the cache warm across a long wait: the host sends a no-op turn at 55 min while a worker waits on a check slot or the manager has a wake due within hours | rewrites after >1h: $12.4 workers + $13.6 manager, against about $0.03 per ping = **$24 (5%)**. It can also replace the idle rotation (`tasks.agent.rotateIdleSeconds`, docs/tasks.md "Context rotation") for a wait the host knows: 27 rotation turns cost $15.3. A limit resume ($15.5) cannot ping | M | none: one short line in context | part 2 "rewrite gap>1h" in $ near 0; ping turns counted |
| 3 | Read through an Explore subagent on Haiku in the plan step: the step text requires it. The current "Explore, sonnet" line is ignored (6 calls) | read results write $16 + carried $25. If half move and come back as 3k findings: **about $15 (4%)** | S (step text) | medium: the build may re-read what the summary missed | Agent calls per task; part 1 "Bash:read" carried $ per task; re-reads of the same file |
| 4 | Fresh session per step with a handoff packet | build starts at 72k, review at 93k, prefix 33k: about $16 of reads, minus about $29 of new starts (about $19 after option 1) = **about $0 net** | M | medium: the plan's reasoning is lost | part 3 start context; only worth it to run review on Sonnet |
| 5 | Sonnet by default for builds that are not large (#182 `--size small` extended) | Sonnet halves writes and output (59% of cost) and its reads cost half (corrected price); Opus is 76% of calls: **up to $95 (24%)** | S | high, and the owner decides: no clean quality comparison exists | landed and reverted rate and $/landed by size, 8 tasks each |
| 6 | Earlier compaction (`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`) or tool-result clearing | upper bound is the carried $42; with rewrite and re-read costs, **about $10-15** | S (env) | medium-high: lost file content leads to re-reads | p90 context and calls after compaction |
| 7 | Longer shared prefix: move the branch name and per-task notes (`claude.rs:174`) behind the fixed text | at most about 2.5k more shared (the CLAUDE.md header carries the working-copy path): **$3.5 (1%)** | S | none | part 1 first-call read p50 rises above 11.8k |
| 8 | Lower `BASH_MAX_OUTPUT_LENGTH` | no Bash result is over 8k tokens: **about $0** | S | low | - |
| 9 | 5-minute cache TTL | saves $33 of incremental writes, but 574 calls after 5-60 min gaps would rewrite about 120k each: **costs about $300** | - | - | rejected |

## Recommended first changes

1. **Option 1, skill allowlist.** It is cheap, has the surest saving and the lowest risk, and it also shrinks every rotation and the manager.
2. **Option 2, warm cache across long waits.** It is a pure cost change with no effect on quality. Combine it with the check-slot wait, which already knows when a worker idles.
3. **Option 3, Haiku Explore reads in the plan step.** It only changes the prompt. Judge it on the next 8 landed tasks against the 11-04 baseline ($2.38 per landed task, 5.5 turns) before you keep it.

Option 5 has the largest potential and needs an owner decision on quality. Options 4, 6 and 7 are not worth their build cost yet. Options 8 and 9 are rejected by the data.

## Rotation or compaction (measured 2026-11-04, part 5)

Decision: **keep the cold rotation at its defaults** (`tasks.agent.rotateTokens` 200000, `tasks.manager.rotateTokens` 150000). A higher threshold costs more, and compaction in place saves too little to pay for the loss of control. The `wsx.config.yml` comment that said "a context past 200k costs about 3x a turn" was stale, and it is fixed.

| Fact | Number | Source |
|---|---|---|
| Long-context premium on Opus 5.5 and Sonnet 5.5 | none: the 1M window costs the same per token (only Haiku 5.5 charges more, over 100k) | [pricing](https://platform.claude.com/docs/en/about-claude/pricing#long-context-pricing), [context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows) |
| Claude Code's own compaction | at about 967k on 1M models. `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (or `autoCompactWindow`) sets the window; `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` can only lower the percent. It never fires before wsx's rotation | [model config](https://code.claude.com/docs/en/model-config.md#set-the-auto-compact-window), [env vars](https://code.claude.com/docs/en/env-vars) |
| Worker rotations in the window | 31: 19 by size, 12 idle | part 5 |
| Cost of one rotation | handoff turn $0.77 + the new conversation's first turn $0.27 (its prefix write alone $0.19) = **$0.96 to $1.04** (handoff by size $0.54, idle $1.13: an idle handoff resumes the cold conversation and pays the rewrite) | part 5 |
| Reads saved after it | **$1.11** per rotation (size $1.51, idle $0.48); 23 later calls p50; the new conversation's first turn ends at 37k p50 | part 5, assumes the same growth without the rotation |
| Context growth per call | workers 1,745 tokens p50 (n=99), manager 1,440 (n=20) | part 5 |
| Compaction estimate | one call reads the context and writes a summary: about $0.32 at 200k-400k. Not verified: the summary size (10k assumed), whether `-p` compacts by itself, and whether the prefix stays cached | part 5 estimate, docs above |

Extra read cost per call over a fresh start, rotating every k calls: F/k + $0.20/M × g × k / 2, with F the cost of one rotation and g the growth per call (workers, F $0.4 to $1.0):

| Threshold | 150k | 200k | 300k | 400k |
|---|---|---|---|---|
| Worker, $ a call | 0.018-0.027 | 0.021-0.027 | 0.029-0.033 | 0.038-0.041 |
| Manager, $ a call | 0.016-0.024 | 0.020-0.025 | 0.028-0.032 | 0.038-0.040 |

- Rotating at 400k (compacted or not) costs about 1.5-1.9x the extra reads of 200k: every call between 200k and 400k reads up to 200k more for $0.04 each.
- Compaction at 200k would save at most about $0.4 per size rotation (F $0.72 against about $0.32): about $8 a week. Against that, it fires inside a turn, drops wsx's handoff notes (what is done, checks, decisions), and its cost in `-p` is not verified. Not worth building now; a trial would set `CLAUDE_CODE_AUTO_COMPACT_WINDOW=200000` for 8 tasks and compare part 5's numbers.
- Idle rotations save less than they cost ($1.31 against $0.48), because the handoff turn pays the cold rewrite anyway. The keep-warm pings (option 2, docs/tasks.md "Context rotation") keep a known wait warm, so these rotations become rare.

## Limits

- 188 older turns carry no step or trigger. Their cost ($157) is in the totals but not in the step rows.
- Tool-result tokens are chars/4 of the text. Images are left out of the "carried" numbers.
- Claude Code does not write the system prompt into transcripts, so the 21.5k prefix split is an estimate from the attachments.
