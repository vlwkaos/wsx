# Agent orchestration

Use the local CLI on the machine that owns the WSX sessions. These commands do not provide a remote transport. An SSH caller runs them on the target host. Agent identity comes from generation-authorized adapter reports, never process names, ports, or terminal content.

## A manager's sequence

A manager on the main checkout starts workers in sibling worktrees, waits, reads results, continues and closes. Each command prints every ID that the next command needs, so no lookup command is necessary. The block can be written before any of it ran, as one tool call.

```sh
# Start: one call for each worker. The same --key can be repeated safely.
wsx agent start fix/a -p <project> --from main --prompt-file a.md --writer --key a-1 --json > a.json
wsx agent start fix/b -p <project> --from main --prompt-file b.md --writer --key b-1 --json > b.json
A=$(jq .exchange.id a.json); B=$(jq .exchange.id b.json)
# Wait for the first worker that is blocked or finished, and read its result.
wsx agent wait --any $A $B --result --timeout 1800 --json   # .exchange .result .ready .pending
# Continue one worker, then wait for it and read the new result.
wsx agent continue $A --prompt-file more.md --key a-2 --json && wsx agent wait $A --result --timeout 1800 --json
# Close a worker after its result is saved.
wsx session delete "$(jq .session_id a.json)" --json
```

### Start a worker in one call

`wsx agent start <branch> -p <project> [--from <ref>] [--agent <command>] [--name <session>] (<prompt> | - | --prompt-file <path>) [--writer] [--write-claim <path>] [--key <text>] [--timeout <s>] [--ready-timeout <s>] [--accept-folder-trust] [--json]`

| Step | What it does | When it already exists |
|---|---|---|
| Worktree | Creates the branch and the worktree. A failed `git worktree add` leaves no branch. No default session is made. | The worktree of that branch is used. |
| Session | Creates one session that runs the agent command. | A session of the same name in that worktree is used. |
| Ready | Waits until the pane's agent is attached, can take a prompt and reports `idle` or `done`. Default 60 seconds. | Same. |
| Request | Delivers the prompt as `agent request` does. | With `--key`, the exchange of that key is returned. |

- The agent command is the project's `worktree.defaultSession.command`. `--agent <command>` gives it for one call. There is no other setting. Without a command, nothing is made.
- The session name is `--name`, or the first word of the command.
- The output has `project`, `branch`, `worktree`, `worktree_created`, `session_id`, `session_label`, `session_created`, `pane_id`, `agent`, `folder_trust_accepted` and `exchange`.
- A failure after the worktree step prints the same object with `error` (`step` and `message`) and `next`, and exits with 1. Nothing is deleted. The same command continues from what exists.
- A failure at the ready step adds `error.reason` and `error.screen`, the last 20 non-empty lines of the pane (at most 2048 bytes). Reasons: `folder_trust_question`, `no_agent_report`, `agent_not_idle`, `agent_cannot_prompt`, `session_exited`.

#### Folder trust question

Claude and Codex ask whether to trust a folder they have not seen, so every new worktree asks. No agent hook runs before the answer, so the agent cannot report.

1. Without `--accept-folder-trust`, the ready step stops as soon as the question is on the screen, with reason `folder_trust_question`. Nothing is answered.
2. Review the worktree. Then repeat the same command with `--accept-folder-trust`.
3. With the flag, wsx moves the highlighted choice to the trust choice and presses Enter, once per call. Claude highlights "No, exit" first. When no trust choice is found, the step stops with the same reason.

Security effect of `--accept-folder-trust`: the answer is the agent's own trust decision, and the agent saves it in its own settings (Claude for the folder, Codex for the repository root). The agent can then read, edit and run files in that folder, and the folder's own agent settings, hooks and MCP servers can run code without a model request. Use it only for worktrees made from branches you trust. wsx writes no agent settings itself.

### Prompt sources

`agent start`, `agent request`, `agent continue` and `session prompt` take the prompt as an argument, as `-` for standard input, or as `--prompt-file <path>`. Exactly one source is allowed. One line end at the end of a file or pipe is removed. An empty prompt is refused. The limit is 65536 bytes (65535 for `session prompt`), and the error names it.

### Idempotent delivery

wsxd answers a request after the PTY took the whole prompt. An agent that reads slowly holds that answer back. Before daemon revision 29 the client stopped reading after 5 seconds and printed `Resource temporarily unavailable (os error 35)`, although the exchange existed. Meanwhile wsxd accepted no other connection.

- The client now waits for the answer to a create or continue request for the exchange `--timeout`.
- wsxd writes the prompt without holding its state or its mutation lock, so reads, other changes and the agents' own reports go on during the delivery. A live handoff waits until the outcome is recorded. A delivery waits for its pane without holding the mutation lock, and a keyed repeat is answered at once with the submitted round.
- The PTY write ends after 10 seconds, or at the exchange deadline when that is sooner. An agent that does not read its input then gets `delivery_failed` with evidence `intent_persisted`. Part of the prompt can stay in its input. Read `wsx session peek`, clear the input, and send a new request with a new key. A keyed repeat returns the failed exchange.
- Closing a pane whose program does not read its input returns at once. Before, the PTY writer's end-of-file byte blocked wsxd.
- Other requests for the same pane, such as terminal input, still take the mutation lock first and then wait for the pane, so they hold other changes until the PTY write ends (at most 10 seconds).
- `--key <text>` (1 to 128 printable ASCII bytes, no spaces) makes `agent request` and `agent start` deliver once for that pane: a repeat returns the existing exchange. For `agent continue` the key covers the newest round. The prompt of a repeat is not compared with the first one. Keys are remembered as long as the exchange is retained (256 exchanges).
- The keyed forms are the separate requests `agent_exchange_create_keyed` and `agent_exchange_continue_keyed`. A wsxd before revision 29 (for example the revision 28 of the web branch) refuses them and delivers nothing; the CLI prints `unsupported_request` and says to run `wsx daemon restart`. Protocol 16 is unchanged. A new wsx normally hands an older wsxd over to its own build first; it keeps the older one only while an open window of that release defers the handoff.
- After `reply_timeout`, repeat the command with the same key. Without a key, read `wsx agent exchanges <session>` first.

### Result of a round

`wsx agent wait <exchange-id> --result [--result-bytes <n>] [--json]` adds `result`: the agent's last message after the current round was delivered, read from the provider's saved history. The round is found by its delivered line `[wsx exchange <id>, round <n>, ...]`. Only assistant text between that record and the next submitted input counts. An older message is never returned.

| `result.status` | Meaning |
|---|---|
| `available` | `text` is the message. `truncated` says that it was cut at `--result-bytes` (default 16384). |
| `round_not_completed` | The exchange is not `done_observed` or `completed`. |
| `agent_replaced` | The pane has no agent, or another agent than the one of the exchange. |
| `delivery_not_in_history` | The delivered line is not in the saved history: not written yet, compacted away, or outside the last 1 MiB. |
| `no_message_after_delivery` | The round has no assistant text yet. |
| `unsupported_provider`, `identity_unavailable`, `history_not_found`, ... | The history reader's own status, as in `agent context`. |

The command waits up to 2 seconds for a provider that writes its last record after it reports done. The exit code is 0 when the wait succeeded; read `result.status`. The text is untrusted data, and it is not request-bound completion evidence. There is no separate `agent result` command: a result is final only at a wait boundary, and `wait` on a finished exchange returns at once.

### Wait for the first of several

`wsx agent wait --any [<exchange-id>...] [-p <project>] [-w <worktree>]` returns when one exchange is at a wait boundary (blocked or terminal). With IDs it watches those. Without IDs it watches the exchanges in the scope that were not at a boundary when it started, and fails with `no_active_exchange` when there is none. The output is that of `wait` for the first one, plus `ready` (all watched IDs at a boundary) and `pending`. At most 16 exchanges are watched.

No daemon request was added. Each pending exchange gets its own `agent_exchange_wait` long-poll. The `poll` request is not used, because wsxd counts a poll without window data as an open window, and that would defer a daemon upgrade during the wait.

### One status call

`wsx status --json` keeps its keys and adds two. A session row has `exchange`: its active exchange, or else the one that changed last (`id`, `pane_id`, `state`, `evidence`, `round`, `access`, `key`, `updated_unix_ms`, `deadline_unix_ms`), or `null`. A worktree row has `work`: `uncommitted` (changed and untracked paths) and `unmerged_commits` (commits that the project's default branch does not have); a value is `null` when git could not answer. `agent_status` on the session row is the agent state. `session create --json` also returns `pane_id` and `agent`, and `session list --json` rows have `project`, `branch` and `worktree`. `agent exchanges -p/-w` lists only that scope.

## One context call

```sh
wsx agent context --metadata-only --json -p <project> [--provider claude]
wsx agent context <session-or-pane-id> --json
```

The packet combines exact target IDs, project/worktree paths, pane revision, attached provider, lifecycle state, capabilities, advisory request eligibility, and recent provider-native persisted history. It uses one existing-daemon snapshot and no model calls. Neither this read nor an exact injected-pane lifecycle report starts or replaces wsxd. A missing daemon returns an error.

Use `--metadata-only` first when choosing a target. It returns the same bounded identities, scope, state, capabilities and advisory readiness from the daemon snapshot, with `projection: "metadata_only"` and `memory: null`. It does not construct a history reader or inventory provider stores. Select an exact returned target before reading history. This filter grants no authority to send work, mutate or close that target; approved project scope still applies.

Without this flag, existing history behavior is unchanged. Defaults: eight candidates, four recent messages, the latest available persisted compaction checkpoint, and 4 KiB of text per candidate. A checkpoint receives at most half that same budget; it does not duplicate a tail message. `--limit` permits 1–32 candidates; `--messages` permits 1–32 messages; `--bytes` permits 1–16 KiB per candidate. Aggregate projected text, including checkpoints, is limited to 64 KiB. Discovery reports omitted candidates. Readiness is advisory: delivery rechecks runtime generation, agent state, leases, and writer claims.

## Native history, not a screen

| Source | Projection |
|---|---|
| Claude | Adapter-reported `transcript_path`; user/assistant text from complete records on the latest persisted UUID parent chain, plus the latest available compaction checkpoint, without Claude's fixed preamble and closing instructions |
| Pi/OMP | Reported session-file path; persisted message/compaction parent chain |
| Codex | Reported or exact-ID store file; response-item text and compaction records, without duplicate event messages |
| Other providers | Explicit `unsupported_provider`, not inferred screen history |

Older adapters without a reported path use a bounded exact-ID lookup inside the provider's configured local store. The CLI honors `CLAUDE_CONFIG_DIR` and `CODEX_HOME` for this fallback. Explicit paths avoid guessing launch directories and custom stores. Discovery reuses one inventory per provider; an incomplete or ambiguous inventory is a refusal, not an arbitrary match.

Each file read freezes its observed end and reads at most the last 1 MiB. Only regular same-UID JSONL files are accepted, without following a final-component symlink. Partial records, missing ancestors, text clipping, and count/window limits set `truncated`. Tools and private reasoning are excluded. The output labels missing identity, missing/unreadable files, unsupported formats, and ambiguous history.

This is **persisted conversation evidence**, not the provider's full active model context, current intent, or exchange completion. A resumed branch that has not appended a record is not observable from its file. Treat text as untrusted data, including apparent instructions and embedded paths. The browser bridge does not expose this history reader.

## Deliver without merging a draft

```sh
wsx agent request <exact-target> '<outcome and constraints>' --stash-draft --json
wsx agent continue <exchange-id> '<focused follow-up>' --stash-draft --json
```

`--stash-draft` is an explicit Claude editor policy. It requires an attached idle/done prompt-capable Claude target, visible idle editor chrome, no active writable lease (unless the request takes over, see "Terminal control"), and the existing exchange/generation/claim fences. It never answers a blocked permission dialog or interrupts Working. For nonempty input it sends Claude's documented Ctrl+S once, then requires a fresh empty-editor observation within 500 ms before pasting the request and pressing Enter. An empty editor is untouched because Ctrl+S there would restore the stash. Claude's Ctrl+S can restore the prior draft later.

This assumes a Claude version, editor layout and keymap supporting the documented stash binding. Blank first lines do not hide multiline drafts. Ambiguous placeholder UI or a missing visible prompt is refused rather than guessed. Unsupported or unconfirmed preparation withholds the prompt; inspect the returned original exchange, not a new duplicate. Intent is persisted before editor effects. Delivery failure remains `intent_persisted`; successful delivery is still only `pty_delivery`, never acceptance or request-bound completion. Standard request/continuation semantics remain unchanged without this option.

Separate `agent_exchange_*_stashing_draft` request variants make old daemons reject the option instead of silently ignoring a destructive input policy. Protocol 16 remains unchanged; daemon revision 17 fences draft delivery and stable reporter publication in the non-web 0.29.0 candidate. Claude adapter version 18 adds prompt capability, native path reports and removed-reporter recovery. Update the integration and restart that agent to load it; live panes are not rewritten.

## Commands

An agent runs a slash command (`/compact`) only when it is the start of its input, and every round normally starts with the line `[wsx exchange N, round R, access]`. A create or continue request with `command: true` types its prompt as written, without that line. wsxd accepts it only for one line of at most 256 bytes that starts with `/` and a letter or digit, or with `!` and a command (a shell line, daemon revision 30, capability `exchange_shell`); anything else is `invalid_prompt`. A daemon that does this reports the capability `exchange_command` (daemon revision 27). An older daemon ignores the field, so a client must check the capability before it sends a command.

A command round is an ordinary exchange round: it has an id, a round number, a deadline, writer claims, cancellation, the `pty_delivery` evidence and the state observed from the pane's lifecycle. What it gives up is correlation: the agent cannot tell which exchange the line belongs to, so an adapter that binds native turns by the envelope (Pi) does not bind it, and the round never reaches `request_bound`. `/clear` and `/new` start a new provider conversation; the session follows the transcript the agent's hooks report next. The CLI does not send commands; the web chat does (`docs/web-client-chat.md` "Commands").

## Native request-bound receipts

Adapters can submit the existing daemon receipt through a machine-facing command:

```sh
wsx agent exchange-receipt <exchange-id> --round <round> --receipt accepted --json
wsx agent exchange-receipt <exchange-id> --round <round> --receipt completed --json
```

The command requires the injected `WSX_RUNTIME_GENERATION` and uses an existing daemon only. wsxd validates the exact generation, round, current state and attached agent's advertised `exchange_receipts` capability. A duplicate receipt is idempotent. Daemon revision 19 expires a still-active overdue exchange at receipt handling, without requiring an earlier read probe. Invalid kinds or zero rounds fail parsing; missing generation fails before IPC. No receipt is retried.

A successfully delivered round exposes optional `delivery_sha256`: SHA-256 of the exact UTF-8 terminal input, including its exchange envelope. Intent alone and failed delivery expose no digest. Continuation clears the previous binding before attempting delivery and publishes a new digest only on success. Legacy responses that omit this field decode as no binding. Receipts never store the prompt text.

Only an adapter that compares actual native input with this digest and rechecks the exact pane, runtime generation, agent identity, round, deadline and live delivery state may bind the native turn. The digest is correlation data, not an authorization token or completion evidence by itself. A missing or mismatched digest cannot enable the adapter. Existing non-Pi receipt submission stays compatible.

Native observers submit `--delivery-sha256 <digest> --input-id <native-id>` together on `agent exchange-receipt`. This uses the distinct `agent_exchange_bound_receipt` request, so an older daemon refuses instead of ignoring the binding. The CLI also requires the injected pane. wsxd checks that pane, live runtime generation, current round and digest; input IDs must contain 1–128 printable ASCII bytes. Bound Accepted persists the input ID. Bound Completed requires that same prior acceptance and rejects overdue requests even if the pane is DoneObserved. Subsequent unbound receipts cannot bypass an established native binding. Duplicate matching receipts keep their revision; continuation clears both digest and native input ID.

A bound request remains active at DoneObserved until its native Completed, cancellation, failure or deadline. It keeps the pane's one-active-exchange fence and writer claims; `agent wait` keeps waiting, and the request cannot be continued or pruned as completed. A new native run can still update its lifecycle observation. Cold recovery interrupts it. Unbound legacy DoneObserved remains a terminal boundary.

Pi integration 20 and later advertises prompt and receipt support only when its native observer loads, a validated Pygmalion Goal Run service is present, and the existing daemon explicitly supports bound receipts. Otherwise it remains lifecycle-only. It binds actual interactive/RPC input and rechecks the raw expanded `before_agent_start` prompt against the delivered digest. Images, changed prompts and forged envelopes cannot obtain acceptance.

Native completion waits for Pi settlement with no queued work. If a Task starts, its exact Task ID and branch lineage must remain the same; its owner must become inactive with all criteria passed and a completion transaction. Automatic extension continuations and compatible steering remain in that Task. Abort, error, truncated output, replacement Task, fork/tree/session switch, runtime-generation change and disposal withhold completion. The observer never infers completion from a title-change notification or pane Done. This still trusts the installed native observer and Goal owner, not the envelope.

Only an adapter that correlates a native turn with this exact exchange and round may send these receipts. A prompt envelope, PTY delivery, pane lifecycle or discovered metadata does not establish that correlation. `accepted` never proves completion; `completed` must come from the correlated native completion. This command alone does not enable Pi/Pygmalion receipt support or prove real-model orchestration.

## Task-owned session cleanup

Record the exact session, pane and exchange IDs when creating orchestration work. Reusing a user's existing agent does not transfer session ownership to the caller. Save the result and verification evidence before cleanup. Close a task-created session with `wsx session delete <exact-session-id> --json` only after its work has settled and no continuation remains; verify that its session and panes are absent from `wsx session list --json` afterward. Cancellation requests and pane `done` state alone do not prove completion or ownership. Retain blocked or failed sessions only for a named investigation, and report their IDs and pending cleanup. Never close the caller, a user's agent, or an unrelated shell by label or provider state.

## Verification

```sh
cargo build --locked -p wsx -p wsx-daemon
python3 -B scripts/test-agent-context.py --wsx <target>/debug/wsx --daemon <target>/debug/wsxd
cargo test --locked -p wsx-core --test agent_memory_contract
```

The same script runs a second journey on its own daemon (`--only orchestration` runs it alone). Its workers are fake agents that write their own history files. It checks: a failed `git worktree add` leaves no branch and the retry succeeds; prompts from a file and standard input with their refusals; a delivery that the worker holds for 7 seconds returns its exchange while a keyed repeat delivers nothing more and another session is renamed and deleted within 3 seconds; a worker that does not read its input gets `delivery_failed` after the 10-second write limit and its session closes; a result and its refusal when the round has no message; one shell line that starts two workers in two new worktrees, waits with `--any` and reads each result without a lookup command; the report of a late failure with its screen; the folder trust question that stops the start at once and is answered only with `--accept-folder-trust`; scoped lists; the status keys; and that an older wsxd (released 0.30.1 or 0.31.0, or a revision 28 build) refuses the keyed requests and is not replaced (`--released <dir>`, skipped with a note when the binaries are absent).

The model-free journey uses an isolated daemon, real PTYs, independent provider-native files and a documented-keyboard actor. It checks native-only context absent from the screen, bounds, exact scope, refusal, the actual submitted request and preserved multiline stash, empty-editor continuation, an unconfirmed stash binding with no delivered prompt, capability/generation/round-fenced native receipts, lifecycle separate from completion, duplicate receipt idempotence, shutdown, and no bootstrap. It does not prove an arbitrary remote Claude version or customized keymap. Verify that host's installed version and binding before relying on the option there.

`scripts/test-pi-native.py --sdk-root <installed-Pi-package> --goal-module <Pygmalion>/extensions/shared/goal-run.ts` exercises real PTY delivery through Pi's SDK, native events and guarded CLI. Its offline provider and controlled Goal state verify correlation, Task continuation/steering, pause, abort, disposal and replacement fences. They do not prove real-model behavior or actual Task finalization. These need the isolated orchestration trial.

The scenario also splits stash acknowledgements across real PTY reads for both initial and continued requests. wsxd releases daemon state and the mutation lock while waiting, retains the pane-operation fence and blocks a live handoff, then adopts current state before recording delivery. Pre-repair binaries fail the independent received-input assertion; the repaired path preserves the draft and observes completion. Current release-candidate verification is recorded separately from earlier whole-workspace receipts.

These source changes require a new WSX installation and updated integrations before use. Read-only skill evaluations are not proof of actual agent acceptance or completion.

## Cancel

`wsx agent cancel` and the web client's Stop write the provider's own interrupt to the pane and mark the exchange `cancel_requested`. Daemon revision 24.

| Provider | Key | Checked against the real program (2026-10-12) |
|---|---|---|
| Claude (`escape_interrupts`) | Escape | The turn ends with "Interrupted" and the editor is ready. Ctrl+C would ask to quit on its second press. |
| Pi, omp | Escape | The turn ends. Ctrl+C does not stop a working Pi. |
| others | Ctrl+C | not checked |

- The exchange becomes `cancelled` when the agent is seen to stop working. Claude has no interrupt hook, so the cancel asks for a new reading of Claude's screen; a state read from the screen settles the exchange like a reported state does. Before revision 24 that reading was never requested and its result never reached the exchange, so the exchange stayed in `cancel_requested`.
- Without `takeover`, a cancel is refused with `terminal_busy` while a writable terminal lease exists on the pane ("Terminal control").
- A cancelled exchange cannot be continued. The next request starts a new exchange.

## Taken, not typed

A round is delivered when wsxd typed it into the agent's terminal; that is not yet proof the agent took it. Claude drops a paste that lands the moment its turn ends, and the next turn a person types would then pass as the round's. An agent whose hook reports prompt receipts (capability `prompt_receipts`; the Claude integration, version 23) says which prompt each turn took:

- On `UserPromptSubmit` the hook reads the prompt's exchange line `[wsx exchange <id>, round <n>, ` (also inside Claude's `<pasted_content>` wrapper) and reports Working with `received: {kind: round, exchange, round}`, or `{kind: other}` for any other prompt (`wsx agent report --received <id>:<n>|other --prompt-receipts`).
- wsxd: the round the receipt names becomes `accepted`. A round still `submitted` or `delivered` when another prompt starts ends `delivery_failed` with failure `not_taken: the agent took another prompt first`; a command round, which has no exchange line (`command` on the exchange), takes the prompt as its own. A `delivered` round that is not taken within 5 s while the agent is not working (and has not been for 5 s) ends `not_taken: the agent did not take it within 5 s`; the command rounds are left out.
- Until a round is taken, the agent's lifecycle reports do not move it: Working, Blocked and Done belong to another prompt's turn. A detached agent still interrupts it. A command round follows the lifecycle as before: Claude's `/compact` starts no `UserPromptSubmit` (checked with Claude Code on 2026-10-21, where a chat round's prompt reached the hook as `[wsx exchange 7, round 1, writer]\n…`).
- An agent without the capability keeps the lifecycle evidence of the sections above.

The web client sends a message whose round was not taken again once ([web-client-chat.md](web-client-chat.md) "Queue and Stop").

## Delivery failure

A round that is not delivered ends in `delivery_failed` with `failure` set to `code: message`, for example `editor_stash_unconfirmed: prompt was not sent because fresh empty input was not observed` or `terminal_write_failed: …`. The next round clears it. Before daemon revision 28 the reason was not kept.

`scripts/web-real-agent-check.py --scenario draft` checks a real Claude: a typed line in the editor is stashed (Claude 2.1.300 then shows `❯ Try "…"` dimmed, which counts as emptied, "Claude's empty editor"), the message is delivered, and Claude puts the line back after the turn.

## Terminal control

An exchange types into the pane, so a live writable terminal lease (a wsx window or a web terminal) blocks it with `terminal_busy`. The create, continue and cancel requests take an optional `takeover` (default `false`). With `takeover`, wsxd takes the pane from that controller after every other check passes, the way `terminal_subscribe` with a takeover does. The displaced stream ends with `lease_transferred`, so a wsx window says "Terminal control moved to another wsx instance" and returns to Workspace, and a web terminal shows "Control moved to another wsx window." Neither takes the pane back by itself.

- Only a person's own action sets `takeover`: the web chat's send, continue and Stop. The newest person wins the pane; nobody is told it is occupied.
- `wsx agent request`, `continue`, `cancel`, `wsx session prompt` and `send-text` never take over. They serve automation, and an orchestrating agent must not take the pane from a person who types in it.
- A daemon before revision 28 ignores `takeover` and still answers `terminal_busy`.

## Interrupted turn

An agent that was seen at work or blocked on an exchange and then reports idle was stopped by a person: Escape in its terminal, or a dialog denied with Escape. No "done" follows such a turn. From daemon revision 26 the exchange then becomes `interrupted`, which is terminal, and the next request starts a new exchange. Before, the exchange stayed active until its deadline and held back every later request to that pane. An idle report before the agent was seen at work changes nothing.

## Reading Claude's screen

wsxd reads the end of Claude's screen to see a dialog or a ready editor, because Claude has no hook for either. From revision 26:

- The rows are the last 12 up to the last written row. Before, they were the last 12 rows of the screen, so a fresh Claude in a tall pane, whose output ends near the top, was never seen as blocked.
- A permission dialog is a row with "Do you want to", numbered Yes or No choices and "Esc to cancel". This covers "Do you want to proceed?" for a command and "Do you want to create notes.txt?" for a file. Before, only the first wording was known.

Both were found with a real Claude 2.1.296 on 2026-10-13 (`scripts/web-real-agent-check.py --scenario answers`).

## Steer

`AgentSteer {pane_id, text, takeover}` (revision 33, capability `agent_steer`) types a message into a working Claude. Claude takes a line entered while it works at its next step, fires `UserPromptSubmit` for it at once, and saves it as an `attachment` of type `queued_command` with the words in `prompt` (Claude Code 2.1.309, checked on 2026-10-23 with a turn that ran `sleep 8`). It is not an exchange: the running turn keeps its round, and the reply answers both.

wsxd refuses with `steer_unsupported` for another provider, `agent_not_working` when the agent is not at work, `exchange_busy` while a delivered round waits to be taken (its receipt would read this prompt as its own refusal), and `editor_unavailable` when Claude's input line is not empty, because typed text would go with the message. The timeline shows a `queued_command` from a person (`origin.kind: "human"`) as the person's message.

## Claude's empty editor

Draft stashing needs an editor that is visibly idle. A fresh Claude shows a suggestion in its empty editor, such as `❯ Try "fix typecheck errors"`. Claude draws it dimmed; text a person typed is not dimmed. After a turn, Claude 2.1.300 may instead suggest a next prompt in plain words, such as `❯ commit the scout skill in dotfiles`, also dimmed. The editor line counts as empty when every character of that line is dimmed in the terminal frame, whatever its words. wsxd then sends no Ctrl+S, because Ctrl+S on an empty editor restores an older stash. After a stash, an editor that shows a dimmed suggestion again counts as emptied. A line that starts with `Try "` and is not dimmed is still refused as ambiguous.

## Resumed agents

After an unplanned daemon loss, wsxd resumes a saved agent under `wsxd __wsx_resume_supervisor <agent argv>` (`run_resume_supervisor` in `crates/wsx-daemon/src/recovery.rs`). When the agent quits, the supervisor opens `$SHELL` in the same pane, so the pane stays a terminal:

1. It writes a reset of the modes an agent can leave on: the alternate screen, kitty keyboard flags (`CSI = 0 ; 1 u`), modifyOtherKeys, mouse and focus reports, bracketed paste, application cursor keys and keypad, a hidden cursor. Without it, the pane's emulator kept encoding typed keys as `CSI u` codes, which the shell printed as text, and the shell drew on a screen the agent had cleared.
2. It clears the agent's record (`AgentClear` with its runtime generation). When that fails (a stale generation, a full disk), it says so in the pane and still opens the shell; before revision 34 it exited, and the pane ended as an empty, dead screen.

### Profile and directory

A resume runs where the agent ran and with the profile it ran with, without a shell profile or a PATH wrapper:

1. Each adapter report runs `wsx agent report` in the agent's own environment. With a session reference, the CLI adds the agent's launch context (`AgentLaunch` on `AgentSessionRef`). The context holds its cwd and the agent's config-root variables that are set: `CLAUDE_CONFIG_DIR` for Claude, `CODEX_HOME` for Codex, and the other variables in `crates/wsx-core/src/integration/root_location.rs` (`integration::root_variables`).
2. wsxd accepts only that provider's variables, with an absolute cwd and no control characters. The first report of a session reference fixes the context, because a later hook can run after the agent changed directory.
3. On a cold start or `wsx session restart`, the resume starts in the recorded cwd. When that directory is gone, it starts in the worktree. The supervisor gets `--set NAME=VALUE` for each recorded variable and `--unset NAME` for each other variable of the provider, before `--` and the agent argv. So a session that ran without `CLAUDE_CONFIG_DIR` resumes without it, even when wsxd's own environment has one. The supervisor sets these on the agent only, and the shell after it keeps the pane's environment.
4. A record from before revision 36, or from an older reporter, has no context. That resume inherits wsxd's environment and starts in the worktree, as before.

Before revision 36, the resume ran `claude --resume <id>` from wsxd's PATH with wsxd's environment, in the worktree root. A per-directory wrapper on PATH (for example one that sets `CLAUDE_CONFIG_DIR` for a work tree) worked only when wsxd's PATH had it first. Otherwise a work session came back on the personal profile. `crates/wsx-daemon/tests/cold_agent_restore.rs` `cold_recovery_resumes_claude_with_its_own_config_root_and_cwd` reports through the real `wsx` with and without a work profile, kills wsxd and restarts it with a stray `CLAUDE_CONFIG_DIR`. It then checks the resumed processes' variable and cwd.

A supervisor runs the `wsxd` that started it, so a pane resumed by an older wsxd keeps the older behavior until it is restarted. `crates/wsx-daemon/tests/live_handoff.rs` `a_resumed_agent_that_quits_leaves_a_usable_shell` quits an agent in its alternate screen with all kitty flags and mouse reports on and a stale generation, then types a line as key events and reads the shell's answer.
