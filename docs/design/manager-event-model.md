# Manager event turns on Sonnet (`tasks.manager.eventModel`)

Task #462, 2026-11-15. Question: should the manager's event-only turns run on Sonnet?
Owner rules that bind: Q2 of [root-cause-refactor.md](root-cause-refactor.md) (yes, after step 6's
turn count), 2026-11-04 "manager opus high", 2026-11-13 "simply changing the model is risky".

## Decision rule (written before the paid runs)

Switch only if all three hold on at least 40 replayed real event-only turns:

1. Quality: the turns where the blind judge marks only Sonnet wrong (minor or major) are at most
   one more than the turns where it marks only Opus wrong (`only_sonnet_wrong <= only_opus_wrong + 1`).
2. No major wrong decision that only Sonnet made (`only_sonnet_major == 0`).
3. Cost: the modeled weekly saving, after the cache writes the model switches add, is at least
   20% of what those turns cost on Opus.

## Method

`python3 -B scripts/bench-manager.py --mode after --cases events [--samples 45] [--days 14] [--budget 60]`

- Replay: both arms resume a fork of the real conversation at effort high with the current
  instructions. `wsx` is a recorder: acting calls answer ok and are kept; reads cannot run, so the
  turn carries the reads the original turn made then. Both arms see the same evidence.
- Judge: Opus sees the events, the reads, what the log shows later, and three answers in random
  order: the original turn, the Opus replay and the Sonnet replay. The original is a free second
  Opus sample, so original against Opus replay is Opus's own noise.
- Cost model (`event_cost`): routes a turn as `turn_model` does (events only and no ready task
  waiting for review, approximated from the events), prices its logged tokens at Sonnet's rates
  times the replays' token ratio, and adds the writes: Claude's prompt cache is per model, so the
  first Sonnet call after Opus writes what Sonnet's cache lacks, and the next Opus turn writes what
  the Sonnet turns added (all of it after an hour).
- Prices (USD per million tokens, claude-api reference): Opus 5.5 input 4, cache read 0.20,
  cache write 8, output 20; Sonnet 5.5 input 2, cache read 0.20, cache write 4, output 10. The cache
  read costs the same on both, and it is half of the event-turn spend.

## Result: no switch

Rule 3 fails before any quality measurement. The cost model shows a loss under every routing,
and the manager stopped the 45-turn paid replay because rule 3 had already failed. `wsx.config.yml` is unchanged.

| Routing (`--cases events --samples 0`) | Window | Turns on Sonnet | Opus $/wk | Sonnet $/wk | Extra writes $/wk (Sonnet + Opus) | Net |
|---|---|---|---|---|---|---|
| today's `turn_model` (no review due) | 14 d | 252 of 1,052 | 23.07 | 15.78 | 16.20 + 10.88 | -86% |
| every event-only turn | 14 d | 1,052 | 102.23 | 76.70 | 19.54 + 40.48 | -34% |
| every event-only turn | 7 d | 528 | 106.15 | 79.66 | 22.87 + 51.78 | -45% |

- Cause: the same tokens cost 25% less on Sonnet (event turns, 14 d: $204 at Opus rates, $153
  at Sonnet rates). Half of that spend is cache reads, which cost the same on both models. Each
  switch writes the conversation into the other model's cache, at $4-8 per million tokens.
- Trial burst (3 wsx turns, ids 3090-3094; 1 failure, 2 ready): Opus, Sonnet and the original
  made the same acts on 2 of 3 turns. On 3094 Sonnet matched the original and the Opus replay
  left out the release of #274. The judge found 0 wrong for Sonnet, 1 minor for the Opus replay
  and 1 minor for the original. Cold fork cost: Opus $0.73, Sonnet $0.35. Warm cost per turn:
  Opus $0.024 to $0.106, Sonnet $0.023 to $0.080. Sonnet's token ratio is 1.10. Real spend:
  **$1.68** (replays priced from tokens, plus 3 Opus judge calls). Three turns cannot show quality.
- Not run: the 45-turn replay and the `--probe` cache check, since the cost side decides.

## Next question: rotation size and idle rule (no paid calls)

`python3 -B scripts/bench-manager.py --mode after --cases rotation [--days 14]` (`rotation_cost`).
Each logged turn splits into its conversation read (calls x context, or one write when cold) and
its own work. A rotation costs the median fresh first turn ($0.44) minus a kept turn's own work,
which leaves $0.38 of start cost, and starts at the fresh median context (53K). Rotations for new
instructions (68 in 14 d) stay where they were. The model is checked against the log: today's
settings model $210.30/wk against $214.35 logged (14 d), and $234.47 against $239.56 (7 d).

| rotateTokens | idle rule: over 60K (today) | idle: any size | no idle rotation | idle kept warm by pings | busy rotations/wk |
|---|---|---|---|---|---|
| 400K | 210.30 | 207.93 | 321.00 | 242.76 | 0 |
| 250K | 205.78 | 203.84 | 277.27 | 216.26 | 3.5 |
| 160K | 198.43 | **195.91** | 247.28 | 198.08 | 13 |
| 120K | 198.17 | 195.53 | 228.92 | 191.08 | 30 |
| 80K | 211.41 | 208.02 | 227.96 | 198.99 | 88 |

USD per week over 14 days. "Busy" counts the size rotations that come less than 10 min after the
last turn, that is, in the middle of work. On 7 days the minimum is also 160K with idle rotation at
any size: $218.85 against $234.47.

- Idle rule: keep rotating at the cache lifetime (60 min). A cold resume costs more than a fresh
  start once the context is over about 50K tokens. Rotating at any size removes 17 cold resumes a
  week (about $2.40). Pings through long idles cost more than they save at 250K and above.
- Minimum: from 120K to 160K. At 80K the extra start costs exceed the smaller reads.

**Recommendation:** `rotateTokens: 160000`, and idle rotation at any size (`IDLE_ROTATE_TOKENS` 60K
to 0, a code constant in `crates/wsx/src/task/manager.rs`). Expected saving: about $14-16 a week
(7%; $14.40 on 14 d, $15.60 on 7 d). The model's error against the log is about 2%, so the saving
is real but small. The setting alone (idle rule unchanged) saves $11.90 a week.

**Quality risk:** a fresh conversation carries the notes (at most 10 KiB, the decisions and the
context) and the last 4 exchanges at 600 characters each, about 15 KB or 4K tokens, plus the
ledger. A conversation of up to 160K tokens holds much more than that. At 160K about 13 rotations
a week land in the middle of work. Each one can drop a review in progress, a promise, or a
thread the owner opened more than 4 exchanges ago. The #456 bench covered only idle returns,
where today's handover tied a written one (manager-continuity.md). Busy rotations were never
measured, so this needs a paid replay before any change. Use the `idle` arm's fresh-against-resume
fork (`fork_copy`) at the turns where 160K would rotate, with a blind judge, at about $0.40 to
$1.20 a turn.
