# Tasks (manager mode M1)

A task is one headless worker on its own branch and working copy. The caller never types into it. Every action is one CLI call; `--json` works on all of them. No wsxd protocol change.

```sh
wsx task new "<prompt>" -p <project> [--from <ref>] --json   # or `-` (stdin) or --prompt-file <path>
wsx task list -p <project> [--all] --json
wsx task show <n> -p <project> --json            # record, result.json, report path, host_running, socket
wsx task steer <n> "<text>" --json               # into the running turn
wsx task add <n> "<text>" --json                 # after the turn (queued while one runs)
wsx task allow <n> "Bash(make:*)" --json         # restart with the tool and resume a blocked task
wsx task stop <n> --json                         # interrupt; the next add resumes the conversation
wsx task verify <n> pass|fail [--note <text>] --json   # human verdict on the current revision
wsx task land <n> [--skip-verify] --json         # onto the default branch by tasks.land.policy
wsx task close <n> [--discard] --json
```

`-p` may be left out inside a project folder or when one project is registered. A task number is `3` or `t3`.

## Store

`$XDG_STATE_HOME/wsx/tasks/<project-id>/<n>/`, each folder 0700 and each file 0600, opened without following links.

| File | Content | Limit |
|---|---|---|
| `task.json` | id, project, base, branch `wsx/t<n>`, working copy, pane, adapter, model, tools, checks, agent session, state, revision | 64 KiB |
| `log.jsonl` | append-only: `created`, `message{prompt\|steer\|add}`, `agent` (short progress), `blocked`, `allowed`, `result`, `checks`, `revision`, `stopped`, `closed` | 8 MiB for progress, 16 MiB hard |
| `result.json` | latest worker result and check results | 256 KiB |
| `report.md` | report of the latest revision | |

- A host's control socket is not in the store: `sun_path` holds 104 bytes on macOS. The task host serves `<TMPDIR>/wsx-<uid>/t-<hash>.sock` and the manager `m-<hash>.sock`, where `<hash>` is 16 hex of sha256 of the store path the socket stands for (`<n>/host.sock`, `manager/manager.sock`), and `/tmp` replaces a `TMPDIR` longer than 64 bytes. The folder is 0700 and must belong to the user, the socket is 0600, a host removes its socket when it exits and replaces a stale one when it starts. A host and its callers must share `TMPDIR`; `show --json` and `manager --json` name the socket.
- `<project-id>` is `<name-slug>-<first 8 hex of sha256(project path)>`. Numbers come from a locked `next` counter and are never reused, also after a failed start.
- One writer: the CLI writes before the host starts and after it exits; the host writes everything in between.
- States: `starting, working, blocked, checking, ready, failed, stopped, conflict, in_review, landed, closed`. `landed` and `closed` are not open.
- A prompt, steer or add is at most 64 KiB.
- Each revision records `base` (the merge base with the default branch, so it follows a rebase) and the paths changed in `base..r<k>`: at most 500 paths and 32 KiB in `paths`, the full count in `paths_total`.
- `list --json` gives each open task `overlaps: [{id, count, complete}]`, the other open tasks that changed a path it changed; `show --json` adds up to 20 shared `paths` per task. `complete` is false when a side kept fewer paths than it changed.

## Working copy

`<parent of main checkout>/<repo>-t<n>`: `git worktree add --no-checkout --detach` at the commit of `--from <ref>` (default: the default branch; the main checkout can be stale, so name a fresher ref when needed; `task.json` records the resolved commit as `base`), an APFS clone (`cp -c -R`, plain copy when that fails) of every top-level entry except `.git`, then `git reset --hard`, `git clean -fd` (ignored build output stays warm) and `git switch -c wsx/t<n>`. Ignored files of the main checkout, such as `.env` or local credentials, are copied into the working copy too, and the worker can read them. Worktrees nested in the main checkout are removed from the copy, so no `.git` file points at another checkout's admin folder. Projects with `.gitmodules` are refused. A failure removes the folder, worktree, branch and task folder.

## Host

`wsx task new` starts hidden `wsx task host <project-id> <n>` as the argv of a wsxd session named `task-<n>` in the working copy. The host:

1. starts the adapter and sends the prompt;
2. prints one progress line per agent step to its terminal (watch it in wsx like any session);
3. serves its control socket (see Store): one JSON request per connection (`status`, `steer`, `add`, `allow`, `stop`, `close`), one JSON answer `{ok, state, message}`;
4. reports its pane through `AgentReport` with provider `task`: working/checking as Working, blocked as Blocked, ready as Done, failed as Error, stopped as Idle;
5. keeps running through `wsx daemon restart` (live handoff), because it is a pane process.

A host restarted by a cold daemon start marks a running turn `stopped`; the next add resumes.

## Claude adapter

`claude -p --input-format stream-json --output-format stream-json --verbose --setting-sources project,local --permission-mode default --model <model> --json-schema <result schema> --append-system-prompt <worker rules> --allowedTools <tools>`, with `--session-id <uuid>` first and `--resume <uuid>` after a stop or allow. The environment keeps only `HOME USER LOGNAME PATH SHELL LANG LC_ALL LC_CTYPE TMPDIR CLAUDE_CONFIG_DIR ANTHROPIC_API_KEY`, the proxy variables (`HTTPS_PROXY HTTP_PROXY NO_PROXY` and lowercase), `NODE_EXTRA_CA_CERTS SSL_CERT_FILE CARGO_HOME RUSTUP_HOME`; no `WSX_*`. `GIT_EDITOR=true`, since no terminal can edit a commit or rebase message. A stdout line over 4 MiB is dropped in buffer-sized steps.

- Steer: a stream-json user message during a turn. Claude does not save it in its history, so the host logs it. A steer with no running turn is sent as an add.
- Only the worker's own `status: blocked` makes the task `blocked` (no checks run). Tool calls Claude denied (`permission_denials`) are evidence: they are logged on the `result` event, listed in `result.json` (`denials`, also in `show --json`) and under Evidence in the report, and checks still run.
- Result schema: `{status: done|blocked|failed, summary, decisions[{choice, reason}], human_verify[{check, pass_if}], left_open[], notes}`.

## Checks, revisions and report

After each result that is not blocked the host runs `tasks.checks` with `/bin/sh -c` in the working copy (`tasks.checkTimeoutSeconds`, default 1200, and 8 KiB of output each, no `WSX_*`; a timed-out check's process group is killed), then sets `refs/wsx/task/<n>/r<k>` to the worker's HEAD. The host commits nothing; the worker commits. `ready` needs worker status `done` and every check at exit 0; otherwise `failed`. `report.md` has Asked, Summary, Decisions, Changes (`base..r<k>` commits and diffstat, plus uncommitted files), Evidence, Human Verify and Left open, built from the log, git and `result.json` only.

## Human verification

`wsx task verify <n> pass|fail [--note <text>]` logs `verified{revision, result, note}` for the current revision through the host. The verdict that applies is the latest one for the record's revision, so a new revision has none: it needs a new verdict or a carried pass (see Landing). A fail needs `--note`; the note is logged verbatim as a `verify` message (Asked shows it as "Human verification (fail)") and goes to the worker, as a steer during a turn or as an add after it. `report.md` ticks the Human Verify items on a pass and ends the section with the verdict, its time and note; `show --json` has `verification`.

## Landing (`local-merge`, the default policy)

`wsx task land <n>` asks the host to:

1. refuse unless the task is `ready`, and when the result has Human Verify items and the current revision has no `pass` (`--skip-verify` lands anyway and logs `verify_skipped`);
2. refuse when the worker left uncommitted tracked changes;
3. when the branch does not contain the default branch tip, rebase it there in the working copy:
   - a conflict aborts the rebase, logs `conflict{onto, paths}`, allows `Bash(git rebase:*)` for this task (logged) and starts a worker turn that rebases and resolves. The state stays `conflict` until the result; the normal checks then make it `ready`, and the caller lands again;
   - a clean rebase reruns the checks as a new revision and lands when they pass. A `pass` on the old revision is carried (`verified{carried_from}`) only when the default branch changed none of the task's paths since its old base; otherwise the new revision needs a new verdict. A failure logs `land_stopped`;
4. fast-forward the default branch: `git merge --ff-only` in the checkout that has it checked out, refused with the changed files named when that checkout has tracked changes; `git update-ref` with the old tip as guard when no checkout has it;
5. log `landed{commit}` and set `landed`. After a rebase the task goes from `checking` straight to `landed`, or to `ready` with `land_stopped`.

## Landing (`pr`)

With `tasks.land.policy: pr`, the first `land` of a ready task applies the same verify and commit gates, pushes `HEAD` to `wsx/t<n>` on `tasks.land.remote` (default `origin`, `--force-with-lease`), runs `gh pr create --base <default> --head wsx/t<n> --title <first prompt line, 72 chars> --body-file report.md`, records `pr_url` and `pr_revision`, logs `in_review{url, revision}` and sets `in_review`. The branch is not rebased first; the remote shows and merges conflicts. A later `land` runs `gh pr view <url> --json state,mergeCommit`: `MERGED` logs `landed{commit: mergeCommit.oid}` and sets `landed` (the local default branch is not changed); `CLOSED` logs `pr_closed` and the task stays `in_review`; an open PR gets the newer revision pushed when the task is `ready` with one, otherwise the answer says it is still open. Push and `gh` each have 120 s.

## Ledger

`wsx task ledger -p <project>` prints JSON of at most 16 KiB, the manager's source of truth:

- `open`: each open task with number, title (80 chars), state, revision, overlaps, the current verdict (without its note), the Human Verify item count, the last result summary (240 chars) and `pr_url`, in attention order (blocked, failed, conflict, ready, in_review, working, stopped);
- `recent`: the last 10 landed or closed tasks;
- `deploys`: the `deployed` status, with `configured` per kind (whether its command is set); under `pr` it uses the last fetched `<remote>/<default>` and does not fetch;
- `main`: the main checkout's path, branch and number of changed files (tracked or new).

When it would pass 16 KiB, the least urgent open tasks are left out and counted in `open_omitted`.

## Manager

```sh
wsx task manager -p <project> --json             # start the manager, or report the running one
wsx task say -p <project> "<text>" --json        # or `-` or --prompt-file; returns {id} at once
wsx task chat -p <project> [--after <id>] --json # messages, replies, rotations
```

One manager per project runs as hidden `wsx task manager-host <project-id>` in a wsxd session `task-manager` in the main checkout (no working copy); `say` starts it when it is not running. Its agent uses `tasks.manager` (`model`, default `sonnet`; `rotateTokens`, default 150000), project and local settings only, and the tools `Read Grep Glob Bash(wsx task:*)`. Its instructions: never edit files or run builds; read `wsx task ledger` first; for each owner message answer, add, steer, start, verify or land; keep turns short; name tasks `#<n>`. Its environment is the worker allowlist plus `XDG_STATE_HOME XDG_CONFIG_HOME XDG_CACHE_HOME WSX_SOCKET WSX_DAEMON_BIN` (so its `wsx task` commands reach this project) and `WSX_TASK_ACTOR=manager`; the host puts its own `wsx` first on `PATH`.

- `manager/log.jsonl` (rolls to `log.1.jsonl` past 8 MiB) has `message{id, from: owner|events, text}`, `delivered{ids}`, `reply{id, to, text, usage}`, `agent` (its commands), `rotated{tokens}` and `stopped`. `manager/state.json` keeps the conversation, the next id and what was already reported.
- One turn at a time. Waiting owner messages go together in one turn, each headed with its id, before waiting events.
- Events: the host checks the task records every second. A change of (state, revision) into `ready`, `blocked`, `failed`, `conflict`, `in_review` or `landed` becomes a line written by wsx, `#<n> <state> r<k>: <title, 80 chars>; <summary, 160 chars>`. Lines are logged as one `events` message when 3 s have passed since the last one and no turn runs, so events during a turn or close together arrive merged.
- Restart: `state.json` records the reported state of each task only when its message is logged, and `delivered` marks what reached the agent. A restarted host queues logged messages that were never delivered and reports changes it has not reported, so no event is lost. Its first start takes the existing tasks as they are, without events.
- After a reply whose context (input, cache read and cache write tokens) passes `rotateTokens`, the next turn starts a fresh conversation and the log has `rotated`; the ledger carries the state.

The web client shows the manager chat, the tasks, their reports and their fixed actions, and notifies about them: [web-client.md](web-client.md) "Tasks".

## Approvals

`tasks.land.approve`, `tasks.deploy.approve` and `tasks.release.approve` (default `true`) decide whether the manager may land, deploy or release alone. A `wsx task` command run with `WSX_TASK_ACTOR=manager` (the manager host sets it for its agent) that needs approval does not act: it appends `request{id: a<n>, action, task, revision, skip_verify, commit}` to `tasks/<project-id>/approvals.jsonl` and answers that the owner was asked. `close --discard` by the manager always asks. The ledger lists pending requests under `approvals`.

```sh
wsx task approve a3 -p <project> --json            # run the action as the owner would
wsx task deny a3 [--note <text>] -p <project> --json
```

Both refuse when `WSX_TASK_ACTOR=manager`. `approve` refuses a land or discard for a task that moved past the requested revision (recorded as not run; ask again). Otherwise it runs the action as its own `wsx task ... --json` command without the actor mark and records `decision{id, approved, note, outcome}`. Decisions reach the manager as events (`approval a3 (land #5) approved; done: ...`). Writers lock the file; requests are numbered under that lock.

This is a guard against the manager's mistakes, not a security boundary: the manager's tools allow every `wsx task` command, and anything running as the owner can leave out `WSX_TASK_ACTOR`. An OS sandbox per agent comes later.

## Deploy and release

```sh
wsx task deploy -p <project> [--commit <ref>] --json    # tasks.deploy.command
wsx task release -p <project> [--commit <ref>] --json   # tasks.release.command
wsx task deployed -p <project> --json
```

`deploy` and `release` make a temporary detached working copy of one commit (default: the default branch tip) beside the main checkout, run the command there with `/bin/sh -c` and `WSX_DEPLOY_COMMIT`, record `{kind, commit, exit, unix_ms, duration_ms}` in `tasks/<project-id>/deploys.jsonl`, then `cargo clean` and remove the copy. They never run in a task's working copy. With `--json` the command's output goes to stderr. Under the `pr` policy the local default branch can lag, so `deploy`, `release` and `deployed` first fetch it from `tasks.land.remote` and use `<remote>/<default>` as the default commit and as the base of `behind`; `local-merge` stays local. `deployed` gives, per kind, whether its command is set (`configured`), the last successful commit, how many default-branch commits it lacks (`behind`) and how many attempts failed since (`failed_since`).

## Close

`close` refuses while `wsx/t<n>` has commits that are not on the default branch, unless `--discard` or the task is `landed`. It asks the host to close, waits for it to exit, removes the sessions of the working copy, runs `cargo clean` when the copy has `Cargo.toml`, and removes the worktree, folder and branch. `--discard` also deletes `refs/wsx/task/<n>/*`. The task folder stays as history (`list --all`).

## Config

```yaml
tasks:
  agent: { adapter: claude, model: opus, tools: [Read, Edit, "Bash(cargo test:*)"] }
  checks: ["cargo fmt --check"]
  checkTimeoutSeconds: 1200
  land: { policy: local-merge, remote: origin, approve: true }   # or policy: pr
  deploy: { command: "sh deploy.sh", approve: true }
  release: { command: "sh release.sh", approve: true }
  manager: { adapter: claude, model: sonnet, rotateTokens: 150000 }
```

Defaults: adapter `claude`, worker model `opus` (the manager keeps `sonnet`), tools `Read Glob Grep Edit Write` plus `Bash(git add|commit|status|diff|log:*)`, no checks. Up to 64 tools and 64 checks; `checkTimeoutSeconds` is 1..86400; a tool or model is at most 256 bytes and a check 16 KiB. `task new` refuses a config that does not parse.

## Verify

```sh
cargo build -p wsx -p wsx-daemon && python3 scripts/test-task.py
```

The script runs an isolated wsxd with `scripts/fake-claude-stream.py` as `claude`.
