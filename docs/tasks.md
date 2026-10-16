# Tasks (manager mode M1)

A task is one headless worker on its own branch and working copy. The caller never types into it. Every action is one CLI call; `--json` works on all of them. No wsxd protocol change.

```sh
wsx task new "<prompt>" -p <project> [--from <ref>] --json   # or `-` (stdin) or --prompt-file <path>
wsx task list -p <project> [--all] --json
wsx task show <n> -p <project> --json            # record, result.json, report path, host_running
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
| `host.sock` | owner-only control socket while the host runs | |

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
3. serves `host.sock`: one JSON request per connection (`status`, `steer`, `add`, `allow`, `stop`, `close`), one JSON answer `{ok, state, message}`;
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

## Deploy and release

```sh
wsx task deploy -p <project> [--commit <ref>] --json    # tasks.deploy.command
wsx task release -p <project> [--commit <ref>] --json   # tasks.release.command
wsx task deployed -p <project> --json
```

`deploy` and `release` make a temporary detached working copy of one commit (default: the default branch tip) beside the main checkout, run the command there with `/bin/sh -c` and `WSX_DEPLOY_COMMIT`, record `{kind, commit, exit, unix_ms, duration_ms}` in `tasks/<project-id>/deploys.jsonl`, then `cargo clean` and remove the copy. They never run in a task's working copy. With `--json` the command's output goes to stderr. Under the `pr` policy the local default branch can lag, so `deploy`, `release` and `deployed` first fetch it from `tasks.land.remote` and use `<remote>/<default>` as the default commit and as the base of `behind`; `local-merge` stays local. `deployed` gives, per kind, the last successful commit, how many default-branch commits it lacks (`behind`) and how many attempts failed since (`failed_since`).

## Close

`close` refuses while `wsx/t<n>` has commits that are not on the default branch, unless `--discard` or the task is `landed`. It asks the host to close, waits for it to exit, removes the sessions of the working copy, runs `cargo clean` when the copy has `Cargo.toml`, and removes the worktree, folder and branch. `--discard` also deletes `refs/wsx/task/<n>/*`. The task folder stays as history (`list --all`).

## Config

```yaml
tasks:
  agent: { adapter: claude, model: sonnet, tools: [Read, Edit, "Bash(cargo test:*)"] }
  checks: ["cargo fmt --check"]
  checkTimeoutSeconds: 1200
  land: { policy: local-merge, remote: origin }   # or policy: pr
  deploy: { command: "sh deploy.sh" }
  release: { command: "sh release.sh" }
```

Defaults: adapter `claude`, model `sonnet`, tools `Read Glob Grep Edit Write` plus `Bash(git add|commit|status|diff|log:*)`, no checks. Up to 64 tools and 64 checks; `checkTimeoutSeconds` is 1..86400; a tool or model is at most 256 bytes and a check 16 KiB. `task new` refuses a config that does not parse.

## Verify

```sh
cargo build -p wsx -p wsx-daemon && python3 scripts/test-task.py
```

The script runs an isolated wsxd with `scripts/fake-claude-stream.py` as `claude`.
