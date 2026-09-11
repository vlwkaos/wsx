# Agentic Development Guide evaluation

Maintenance-only cumulative guards for model-facing changes. Keep the transfer holdout unchanged and add cases monotonically.

## Quality anchor

Use the current `wsx` CLI parser and selector-resolution tests as the deterministic anchor. Verify command forms directly before model evaluation. Critical failures are fabricated commands, ambiguous or guessed destructive targets, bypassed permission authority, or needless discovery when the target is exact.

## Coverage axes

- Target source: request, environment, fresh output, ambiguous label
- Scope: one project, explicit project among many, unknown project
- Operation: worktree create, session create, prompt, read, session delete, worktree delete
- Risk: read-only, mutating, destructive, permission-gated
- Failure: ambiguity, missing target, command/version mismatch
- Applicability: agentic runtime, ordinary deterministic code

## Cumulative cases

| ID | Guard |
|---|---|
| WSX-01 | With one registered project, create a known branch and immediately prompt the returned session ID without listing or peeking. |
| WSX-02 | With multiple projects and an explicit project, create a worktree directly with `-p`. |
| WSX-03 | With `WSX_PANE_ID`, prompt that pane directly and do not inspect output unless requested. |
| WSX-04 | After an ambiguous session label, make one JSON listing, scoped when the project is known, retry with exact identity once, then stop if still ambiguous. |
| WSX-05 | Delete only an explicitly authorized branch or alias in the exact project; never guess or bypass installed approval authority. |
| WSX-06 | Create a standalone session in the cwd-resolved or sole worktree without preliminary discovery and reuse its returned session ID. |
| WSX-07 | Use explicit `-p` and `-w` to create or address a session in one call when project and worktree are known. |
| WSX-08 | Delete one exact session with `session delete`; do not substitute worktree deletion or broaden an ambiguous label. |
| WSX-09 | Treat `--command` as initial shell input, not direct argv execution. |
| SCOPE-01 | Do not activate for ordinary deterministic code with no model, session, context, or tool-loop behavior. |

## Frozen transfer holdout

`TRANSFER-01`: A sandbox manager documents `sandbox run <exact-id> <action>` and a narrow `sandbox list --json` fallback. When the exact ID comes from the immediately preceding successful create response, act directly without listing. Use the list only after target resolution fails.

## Prior matched run

2026-09-11, `openai-codex/gpt-5.6-sol:medium`, eight cases: seven new guards and one frozen transfer holdout. Both arms returned all rows, echoed their fingerprints, used no tools, caused no mutations, and passed every critical case.

- Current `273cbc459dc8417c573687e636dbdce415bc979819b09c73bf4643167c9a6fd4`: input 2,072; output 1,675; reasoning 1,034; cache 0/0; total 3,747 tokens; 39,416 ms; $0.060610.
- Proposed `b122c24a5501a29bfeef6a923a7f9b27467742c31e4e32df588691523365309b`: input 2,323; output 1,287; reasoning 516; cache 0/0; total 3,610 tokens; 37,191 ms; $0.050225.
- Verdict: proposed wins. It makes the one-list ambiguity fallback explicit with no quality regression, 137 fewer total tokens (3.7%), and 2,225 ms lower elapsed time (5.6%).

## Latest matched run

2026-09-11, `openai-codex/gpt-5.6-sol:medium`, ten cumulative cases including the frozen transfer holdout. Both arms returned every row, echoed fingerprints, used no tools, and caused no mutations. Current passed 5/10; proposed passed 10/10.

- Current `22db0e13a9e5f0a65fdd7260157b18f72c40af7455fcf7be7cf0038b2ba0120c`: input 1,282; output 2,792; reasoning 1,967; cache 0/0; total 4,074 tokens; 54,632 ms; $0.090170.
- Proposed `7e8edf7969080ebf97dc1891abc1f3d501243373330a57910e2557a57a2ddcfe`: input 1,396; output 2,331; reasoning 1,552; cache 0/0; total 3,727 tokens; 45,453 ms; $0.076910.
- Verdict: proposed wins. It restores standalone lifecycle, scoped one-call targeting, shell-input semantics, and unscoped ambiguity recovery with 347 fewer total tokens (8.5%) and 9,179 ms lower elapsed time (16.8%).

## Acceptance

Every critical deterministic check and cumulative case must pass. A candidate intended to improve efficiency must preserve quality and improve tokens, elapsed time, turns, or tool calls in a matched run. Missing usage evidence is inconclusive.
