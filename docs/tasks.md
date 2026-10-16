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
- States: `starting, working, blocked, checking, ready, failed, stopped, closed`.
- A prompt, steer or add is at most 64 KiB.

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

`claude -p --input-format stream-json --output-format stream-json --verbose --setting-sources project,local --permission-mode default --model <model> --json-schema <result schema> --append-system-prompt <worker rules> --allowedTools <tools>`, with `--session-id <uuid>` first and `--resume <uuid>` after a stop or allow. The environment keeps only `HOME USER LOGNAME PATH SHELL LANG LC_ALL LC_CTYPE TMPDIR CLAUDE_CONFIG_DIR ANTHROPIC_API_KEY`; no `WSX_*`.

- Steer: a stream-json user message during a turn. Claude does not save it in its history, so the host logs it. A steer with no running turn is sent as an add.
- A non-empty `permission_denials`, or a result `status: blocked`, makes the task `blocked` without checks.
- Result schema: `{status: done|blocked|failed, summary, decisions[{choice, reason}], human_verify[{check, pass_if}], left_open[], notes}`.

## Checks, revisions and report

After each result that is not blocked the host runs `tasks.checks` with `/bin/sh -c` in the working copy (20 min and 8 KiB of output each, no `WSX_*`), then sets `refs/wsx/task/<n>/r<k>` to the worker's HEAD. The host commits nothing; the worker commits. `ready` needs worker status `done` and every check at exit 0; otherwise `failed`. `report.md` has Asked, Summary, Decisions, Changes (`base..r<k>` commits and diffstat, plus uncommitted files), Evidence, Human Verify and Left open, built from the log, git and `result.json` only.

## Close

`close` refuses while `wsx/t<n>` has commits that are not on the default branch, unless `--discard`. It asks the host to close, waits for it to exit, removes the sessions of the working copy, runs `cargo clean` when the copy has `Cargo.toml`, and removes the worktree, folder and branch. `--discard` also deletes `refs/wsx/task/<n>/*`. The task folder stays as history (`list --all`).

## Config

```yaml
tasks:
  agent: { adapter: claude, model: sonnet, tools: [Read, Edit, "Bash(cargo test:*)"] }
  checks: ["cargo fmt --check"]
```

Defaults: adapter `claude`, model `sonnet`, tools `Read Glob Grep Edit Write` plus `Bash(git add|commit|status|diff|log:*)`, no checks. Up to 64 tools and 64 checks; a tool or model is at most 256 bytes and a check 16 KiB. `task new` refuses a config that does not parse.

## Verify

```sh
cargo build -p wsx -p wsx-daemon && python3 scripts/test-task.py
```

The script runs an isolated wsxd with `scripts/fake-claude-stream.py` as `claude`.
