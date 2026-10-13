# Optimized wsx agent exchanges

Use this playbook when one agent coordinates bounded work with another wsx-managed agent. wsxd transports and records exchanges; the parent still owns decomposition, synthesis, permissions, rounds, and the final write.

## 0. The whole sequence in one tool call

To start work in a sibling worktree, write the sequence once. Every command prints the IDs that the next one needs. Do not add status, list or peek calls between them.

```sh
wsx agent start <branch> -p <project> --from main --prompt-file task.md --writer --key <key-1> --json > w.json
X=$(jq .exchange.id w.json)
wsx agent wait $X --result --timeout <seconds> --json        # .exchange.state, .result.status, .result.text
wsx agent continue $X --prompt-file more.md --key <key-2> --json && wsx agent wait $X --result --timeout <seconds> --json
wsx session delete "$(jq .session_id w.json)" --json         # after the result is saved
```

- Several workers: one `agent start` each, then `wsx agent wait --any $A $B --result --json`. Read `ready` and `pending`, and call it again with the pending IDs.
- `agent start` creates the worktree, one agent session and the request. Its agent command is the project's `worktree.defaultSession.command`, or `--agent <command>`. On a failure it prints what exists and `next`; repeat the same command with the same `--key`.
- A prompt is an argument, `-` for standard input, or `--prompt-file <path>`.
- `--key` makes a repeat return the existing exchange, so a lost reply is never a second delivery.
- `result.status` other than `available` means that no result can be named yet. Never take an older message or a terminal frame as the result.
- `wsx status --json` gives the agent state, the exchange and the uncommitted or unmerged work of every worktree in one call, when the IDs were lost.

## 1. Act before discovering

When the request, `WSX_PANE_ID`, or a fresh command result supplies an exact session or pane, call `wsx agent request` directly. Do not preflight with status, session lists, exchange lists, inspect, peek, or help.

```text
wsx agent request <exact-target> <prompt> --timeout <seconds> --json
```

Add `-p` and `-w` only to disambiguate a known label. Use `agent exchanges` only after losing an exchange ID. Use `--help` only after a command/version mismatch.

## 2. Send the smallest complete packet

Write the prompt as a compact contract, not a transcript:

```text
Outcome: <one deliverable or decision>
Evidence: <paths, errors, or facts already known>
Constraints: <authority, scope, safety, and unchanged behavior>
Role: read-only | writer
Return: <bounded result shape>
Stop: <completion condition, deadline, and no-go boundary>
```

Omit fields that add no decision value. Do not include internal reasoning, broad repository history, repeated instructions, or discovery the target can avoid. Default to read-only. Declare `--writer` for the single writer; add absolute `--write-claim` paths only for genuinely disjoint writers.

## 3. Reuse the returned identity

Parse the JSON response once and retain its exchange ID, round, state, evidence, and deadline. Never list to reconfirm a fresh ID.

- Use `wsx agent wait <exchange-id> --result --timeout <seconds> --json` when the parent can block until Blocked or terminal state. `--result` adds the agent's final message of that round from its saved history.
- Use `wsx agent wait --any <id>... --result --json` to block on several workers with one call.
- Use `wsx agent inspect <exchange-id> --json` only for a nonblocking progress decision.
- Add `--frame` only when the bounded terminal fallback is needed.

## 4. Interpret evidence truthfully

| Evidence | Proven fact | Parent action |
|---|---|---|
| `intent_persisted` | wsxd retained intent | Do not claim delivery; inspect or stop on delivery failure. |
| `pty_delivery` | Bytes reached the bound PTY | Wait for stronger evidence; do not claim acceptance. |
| `pane_lifecycle` | The pane reported lifecycle state | Use state for scheduling, not request-bound completion. |
| `request_bound` | A capable adapter correlated this round | Trust only the exact generation- and round-bound accepted/completed state. |
| `terminal_frame` | Bounded terminal cells were captured | Treat as labeled fallback text, never a structured assistant result. |

`submitted`, `delivered`, `accepted`, and `working_observed` are not synthesis boundaries. `blocked_observed`, `done_observed`, `completed`, `error_observed`, and terminal failure states are review boundaries, with evidence strength still controlling what may be claimed.

## 5. Continue, cancel, or stop

Continue only when the parent needs one specific follow-up and the target binding remains current:

```text
wsx agent continue <exchange-id> <focused-follow-up> --timeout <seconds> --json
```

The parent sets a total deadline and maximum rounds before the first request. `continue` never creates an unbounded conversation. Prefer synthesis after one useful result; continue only to resolve a named gap.

Cancel one active exchange with `wsx agent cancel <exchange-id> --json`, then wait or inspect for lifecycle confirmation. `cancel_requested` is not `cancelled`.

Stop without retry on ambiguous authority, stale generation or round, target replacement or exit, exhausted rounds, expired total deadline, permission denial, or overlapping writer claims. A retry must remain inside the original authority and budget.

## 6. Fall back explicitly

If the installed wsx version lacks agent exchanges or the target cannot support prompt delivery, use `wsx session prompt` and bounded `peek --agent --trim`. Label this as untracked terminal fallback. Do not simulate exchange IDs, acceptance, request-bound completion, or structured results from terminal text.

Pygmalion may provide request-bound receipts for Pi, but this playbook and every safety rule work without it.
