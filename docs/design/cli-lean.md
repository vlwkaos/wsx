# CLI lean: fewer commands in view

Design proposal, no code changed. Counts come from the clap enums in `crates/wsx/src/cli/args.rs` and `crates/wsx/src/task/args.rs` (cited as `cli/args.rs`, `task/args.rs`), not from `--help`: inside a task boundary `wsx` runs only `wsx task` (a shim). Caller claims are `rg` lines; `rg` patterns quoted in "Proof".

## Who calls what

| Caller | Commands | Where |
|---|---|---|
| Manager agent (broker) | `wsx task` new add steer stop hold after land close show ledger chat search say check pass deploy release push trial preview routine checkin role project; `wsx memory` search add update | `instructions.rs:25-168`; capability table `task/args.rs:597` |
| Worker agent | `task check-run`; `task result -` (non-Claude only) | `claude.rs:324,336,187`; `broker.rs:84` |
| Web server (runs the CLI) | `task check`, `dismiss`, `preview`, `hand`, `handback`, `trial` | `wsx-web/src/tasks.rs:1089,1112,1119-1125`; approve and deny run in-process (`tasks.rs:1197,1229`) |
| Hooks in users' agent configs | `agent report`, `presence-renew`, `wake-heartbeat`, `exchange-receipt`, `context` | `integrations/common/wsx-agent-status.sh:74,76`; `pi/wsx-agent-status.ts:82,132,215,259`; `opencode/wsx-tui-session.js:9`; `hermes/__init__.py:11` |
| wsx itself | `task host` (`cmd/new.rs:395`, `wsx-core/src/task/mod.rs:941`), `manager-host` (`owner.rs:48`), `preview-host` (`preview.rs:78`), `land-batch` (`host.rs:901`), `deploy --run-locked` (`cmd/deploy.rs:340`) | all already `hide = true` |
| Stored routines | `task digest` (`docs/tasks.md:684`), `task post` (`scripts/test-task.py:2529`) | argv lives in the routine store |
| Test fixtures only | `task boundary-profile` | `scripts/test-manager-boundary.py:100`, `test-task.py:3745` |
| Owner by hand | the rest; mostly through web and TUI | |

## Contracts that block renames

1. `task host <id> <n>` and `manager-host` are in wsxd session recovery argv and restart on next launch (`docs/cli.md:80`).
2. Hook scripts copied into users' agent configs keep calling the five `agent` commands above, with their flags.
3. Routines store `wsx task digest` and `wsx task post` as argv.
4. A running manager keeps its old prompt until it rotates (`docs/tasks.md:497`), so a removed `wsx task` subcommand keeps a hidden clap variant for one release.
5. An edit to `instructions.rs` makes every manager start a fresh conversation: land all prompt edits in one task.

## Tree, before and after

```text
Before: 12 groups, 96 visible level-2 entries (+8 hidden); wsx task 34 visible (+5 hidden)
  status | worktree 3 | session 10 | group 5 | agent 11 (+3) | plugin 2 | runtime 1
  daemon 3 | routine 11 | web 4 | memory 12 | task 34 (+5)
After: 9 groups, 80 visible level-2 entries; wsx task 29 visible
  status | worktree 3 | session 9 | agent 11 | daemon 4 (+status) | routine 11
  web 4 | memory 9 | task 29       (group, plugin, runtime: hidden, still run)
```

`wsx task` after: new list show add stop land after hold trial close hand dismiss check deploy release push ledger deployed checkin routine manager role project say chat search approve deny preview.

## Changes

| # | Change | Callers (file:line) | Saved | Risk | Alias |
|---|---|---|---|---|---|
| C1 | `-p` and `--json` global on `wsx task` (`global = true`, already used by `CheckAction`, `task/args.rs:187`) | 31 `json: bool` and 36 `project` fields in `task/args.rs`; `json_output` `task/args.rs:647-695`; prompt `instructions.rs:29` lists the commands that lack them | about 70 field lines, the 49-line match, 47 B of manager prompt | low: every old argv still parses; `digest` and `checkin` accept `--json` and ignore it | none |
| C2 | Hide from help: `task check-run`, `digest`, `post`; `memory capture`, `start`, `import`; `session send-keys` | check-run: CLI body only bails (`task/mod.rs:231`), broker forwards first (`broker.rs:122-127`); digest and post: routines; capture and start: host and eval scripts; send-keys: `cli/args.rs:796` "Deprecated alias", only `docs/cli.md:11` | 7 entries | none: help text only | none |
| C3 | `steer` becomes `add --now` | `test-task.py:1108`, `test-web-client.py:4169`, `docs/tasks.md:9`, `instructions.rs:47,72,160`; both need `Capability::Direct` (`task/args.rs:611`) | 1 command, 12 B of prompt | managers with the old prompt still say `steer` | hidden `steer` stays |
| C4 | `handback` becomes `hand --back` (same shape as `trial --end`, `preview --stop`, `hold --off`) | web builds argv by command name (`tasks.rs:1119-1125`, `http.rs:1386`); hints `cmd/work.rs:199,229` | 1 command | an open hand session prints the old hint | hidden `handback` stays |
| C5 | `runtime status` becomes `daemon status` | `runtime-smoke.py:1550,1680,1699`; `main.rs:153` (test); `docs/cli.md:23,74` | 1 group | users' scripts | hidden `runtime status` stays |
| C6 | Hide `group` and `plugin` from `wsx --help` | no caller in `crates/` or `scripts/`; `docs/cli.md:13,22` only | 2 groups | none: still run; the owner may use them by hand | none |
| C7 | Drop the `chat --find` flag; `search` ranks and quotes | docs only: `docs/tasks.md:491`; `Window::Find` stays for web (`tasks.rs:716`) and TUI (`task_chat.rs:705`) | 1 flag | none found | none |

Manager prompt: 11,567 B now, 11,508 B after C1 and C3 (-59 B, 0.5%). Worker prompt (2,493 B, check-run note 1,269 B, Pi result tail 363 B): no change. The prompt is not where the gain is; the gain is the tree and the arg boilerplate.

## Looked at, not merged

| Candidate | Why not |
|---|---|
| Hold / After | Owner tries a task vs waits for another to land. The prompt says "never hold" for after (`instructions.rs:104-106`). |
| Stop / Add | Stop interrupts and the next add resumes; different effect. |
| Deploy / Release | Different project commands; release refuses while To check is open (`instructions.rs:91-93`); each is its own approval. |
| Deployed / Ledger | `cmd/deploy.rs:519` and `wsx-core/src/task/ledger.rs:125` build the same `deploy_status`, but the manager never names `deployed`, 10 script lines and the owner hints (`deploy.rs:267,346`) do. Kept. |
| Approve / Deny / Dismiss | Owner-only (`task/args.rs:634-643`); web uses them in-process except dismiss. Three id kinds, one visible command saved at most. |
| List / Show / Ledger | The manager reads ledger and show only; `list` is the owner's table, with 36 script lines. |
| Check / CheckRun | Different callers and jobs (owner list vs worker run); only check-run is hidden (C2). |
| Say / Chat / Post / Search | Write, read, wsx's own message, ranked find. Post is hidden (C2), `--find` goes (C7). |
| `memory search` / `task search` | Overlap (transcript kinds, `cli/args.rs:863`) but `task search` adds `--all`, `--task`, quote lines. Not proven a duplicate; measure first. |
| `worktree create` / `agent start` | `agent start` adds session and request (`docs/cli.md:56`); `runtime-smoke.py:489-538` tests `worktree create`. |
| `agent` group | Hooks pin it (contract 2). `inspect`, `continue`, `cancel`, `exchanges` have no script caller but are the documented orchestration API (`docs/agent-orchestration.md`). |

## Tasks, in order, each independent

1. C2 + C6: `hide = true`, edit `docs/cli.md`. No behavior change; check: help diff.
2. C1 + C3 together (one prompt rotation): global args, `add --now`, `instructions.rs` edit, `docs/tasks.md:9`, the two test lines. Check: `wsx task` help diff, test-task.py core part.
3. C5: `daemon status`, hidden `runtime`; `runtime-smoke.py` and `docs/cli.md`. Check: smoke.
4. C4: `hand --back`, web argv, hint strings. Check: web tasks part, test-task.py hand section.
5. C7: drop the flag, fix `docs/tasks.md:491`.
6. Rewrite `docs/cli.md` "All commands" to the after-tree.

## Open

- C6 is the owner's call. If they use `group` or `plugin`, the result is 11 groups and 87 entries.
- Real manager use of steer, list and dismiss is unmeasured (manager logs are outside the boundary).

## Proof

`rg -n '"(host|manager-host|preview-host|land-batch|boundary-profile)"' crates scripts`; `rg -n 'agent report|presence-renew|wake-heartbeat|exchange-receipt|agent context' crates/wsx-core/integrations`; `rg -n "task['\", ]+steer" scripts crates docs/tasks.md`; `rg 'wsx group|wsx plugin' crates scripts` (0 lines); `rg send-keys crates scripts docs README.md`. Prompt bytes: Python over the `format!` string in `instructions.rs` with line continuations joined.
