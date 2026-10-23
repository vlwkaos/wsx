# Tasks (manager mode)

A task is one headless worker on its own branch and working copy. The caller never types into it. Every action is one CLI call; `--json` works on all of them. No wsxd protocol change.

```sh
wsx task new "<prompt>" -p <project> [--from <ref>] [--title <words>] [--model <name>] [--hold <why>] --json   # or `-` (stdin) or --prompt-file <path>
wsx task list -p <project> [--all] --json
wsx task show <n> -p <project> --json            # record, result.json, report path, host_running, socket
wsx task steer <n> "<text>" --json               # into the running turn
wsx task add <n> "<text>" --json                 # after the turn (queued while one runs)
wsx task allow <n> --host <host> | --path <path> | "<rule>" [--project-wide] --json   # grant; resumes a waiting task
wsx task stop <n> --json                         # interrupt; the next add resumes the conversation
wsx task land <n> [--tried] --json               # rebase, check, fast-forward (a ready task lands by itself by default)
wsx task hold <n> "<why>" | --off --json         # the owner tries it before it lands; see Hold and trial
wsx task trial <n> [--end] --json                # its revision on the live site before it lands
wsx task hand <n> --json                         # owner only: work on the task by hand; see By hand
wsx task handback <n> --json                     # owner only: its commits are checked as the next revision
wsx task preview <n> [--stop] --json             # a link to try the task (tasks.preview); see Preview
wsx task check-run [<n>...]                      # from a worker: run the checks as after its result
wsx task check -p <project> [--json]             # the open "To check" items
wsx task check pass <id>... | --all --json       # owner only
wsx task check fail <id> --note <text> --json    # owner only; starts a task that fixes it
wsx task close <n> [--discard] --json
```

`-p` may be left out inside a project folder or when one project is registered. A task number is `3` or `t3`. `new --model <name>` picks the worker's model (one word, at most 256 bytes, not starting with `-`); without it the task uses `tasks.agent.model` (default `opus`). The record keeps it.

## Store

`$XDG_STATE_HOME/wsx/tasks/<project-id>/<n>/`, each folder 0700 and each file 0600, opened without following links.

| File | Content | Limit |
|---|---|---|
| `task.json` | id, project, base, branch `wsx/t<n>`, working copy, pane, adapter, model, tools, checks, agent session, state, revision, `created_by` (`owner` or `manager`), `hold` (why the owner tries it first), `waiting` (`{on, reason}` while the state is `waiting`) | 64 KiB |
| `log.jsonl` | append-only: `created`, `message{prompt\|steer\|add}`, `agent` (short progress), `blocked{denials, asks}`, `granted{access, project, by}`, `result`, `checks`, `check_retry`, `check_run`, `disk_wait{free_mb, min_gb}`, `revision`, `preview{url, port}`, `hand{by, dropped}`, `handback{by, commits}`, `hold{reason, by}`, `trial{revision, commit}`, `trial_ended{reason}`, `stopped`, `closed` | 8 MiB for progress, 16 MiB hard |
| `result.json` | latest worker result and check results, `denials`, and `asks` of a block | 256 KiB |
| `report.md` | report of the latest revision | |
| `captures/` | the To check captures kept at the land (see To check) | 2 MiB each |

- A host's control socket is not in the store: `sun_path` holds 104 bytes on macOS. The task host serves `<TMPDIR>/wsx-<uid>/t-<hash>.sock` and its worker's broker `w-<hash>.sock`, the manager `m-<hash>.sock` and its broker `b-<hash>.sock`, where `<hash>` is 16 hex of sha256 of the store path the socket stands for (`<n>/host.sock`, `<n>/broker.sock`, `manager/manager.sock`, `manager/broker.sock`), and `/tmp` replaces a `TMPDIR` longer than 64 bytes. The folder is 0700 and must belong to the user, the socket is 0600, a host removes its socket when it exits and replaces a stale one when it starts. A host and its callers must share `TMPDIR`; `show --json` and `manager --json` name the socket.
- `<project-id>` is `<name-slug>-<first 8 hex of sha256(project path)>`. Numbers come from a locked `next` counter and are never reused, also after a failed start.
- One writer: the CLI writes before the host starts and after it exits; the host writes everything in between.
- States: `working` (an agent turn runs; also the turn that resolves a landing conflict, which the web reads from the log), `waiting`, `checking`, `ready`, `failed`, `landed`, `closed`. `landed` and `closed` are not open. A `waiting` task names what it waits for in `waiting.on`, with a `reason` in plain words:

  | `on` | Why | Ends when |
  |---|---|---|
  | `owner` | the worker returned `blocked` without asks, was stopped, or its host restarted during a turn | an `add` |
  | `permission` | the worker returned `blocked` with asks (see Permissions) | every ask is granted, or an `add` |
  | `disk` | less than `tasks.minFreeGb` is free | the host finds space (every 30 s) |
  | `hand` | the owner works in the copy (see By hand) | `handback` |
- A prompt, steer or add is at most 64 KiB.
- A task's title is `--title` (at most 60 characters, no control characters, kept in `task.json` as `title`), else the first line of its prompt, at most 120 characters. Either one is shown without a leading `Owner <words>:` (up to 80 characters before the colon; a colon inside brackets or without a space after it, as in `http://`, does not end it): `Owner report: tool permission` reads `tool permission`. A lead-in with nothing after it reads as the words in its brackets (`Owner report (UX failures):` reads `UX failures`), else as the prompt's next non-empty line. `Owner: x` has no words and stays. Only the shown title changes: `task.json` and the prompt keep what was written. Every title shown (ledger, events, the web) comes from this rule, also for tasks from before `--title`. The web shows a task without `--title` by the first sentence of its latest worker summary while it has one (`view::readable_title`).
- Each revision records `base` (the merge base with the default branch, so it follows a rebase) and the paths changed in `base..r<k>`: at most 500 paths and 32 KiB in `paths`, the full count in `paths_total`.
- `list --json` gives each open task `overlaps: [{id, count, complete}]`, the other open tasks that changed a path it changed; `show --json` adds up to 20 shared `paths` per task. `complete` is false when a side kept fewer paths than it changed.

## Working copy

`<parent of main checkout>/<repo>-t<n>`: `git worktree add --no-checkout --detach` at the commit of `--from <ref>` (default: the default branch; the main checkout can be stale, so name a fresher ref when needed; `task.json` records the resolved commit as `base`), an APFS clone (`cp -c -R`, plain copy when that fails) of every top-level entry except `.git`, then `git reset` and `git reset --hard`, `git clean -fd` (ignored build output stays warm; filling the index first means only files that differ from the commit are rewritten, so the others keep their clone's mtime and cargo reuses the cloned build) and `git switch -c wsx/t<n>`. Ignored files of the main checkout, such as `.env` or local credentials, are copied into the working copy too, and the worker can read them. Worktrees nested in the main checkout are removed from the copy, so no `.git` file points at another checkout's admin folder. Projects with `.gitmodules` are refused. A failure removes the folder, worktree, branch and task folder.

## Host

`wsx task new` starts hidden `wsx task host <project-id> <n>` as the argv of a wsxd session named `task-<n>` in the working copy. The host:

1. starts the adapter and sends the prompt;
2. prints one progress line per agent step to its terminal (watch it in wsx like any session);
3. serves its control socket (see Store): one JSON request per connection (`status`, `steer`, `add`, `allow`, `stop`, `land`, `hold`, `trial`, `hand`, `handback`, `close`), one JSON answer `{ok, state, message}`;
4. reports its pane through `AgentReport` with provider `task`: working and checking as Working, waiting on the owner or a permission as Blocked, ready as Done, failed as Error, waiting on disk or hands as Idle;
5. keeps running through `wsx daemon restart` (live handoff), because it is a pane process.

A host restarted by a cold daemon start logs `stopped` for a running turn and waits on the owner; the next add resumes. A new task's host starts the first turn with the prompt.

A host follows a deploy of wsx. It records the identity (device, inode, size, modification time) of the file it was started as (its argv[0], which `task new` and `say` make absolute). Before each agent turn (an add, a resuming grant, a check retry, delivered queued messages) a task host that finds another file there writes the turn's text to `handoff.txt` (0600), sets `working`, answers its caller and execs that file with the same arguments: the pane and pid stay, and the new host starts the turn with the new launch, resuming the same conversation. While a trial of any project holds a live site (a `trial.json` in the store), no host follows: the new file may be the trial's build, and the hosts follow the build that replaces it when the trial ends. The manager host does the same before its next turn; its messages stay logged and undelivered, so the new host queues them again. `wsx task` actions (CLI and web) retry the connect for up to 3 s while the socket is gone. Install by replacing the file (`cargo install`, `install`, `mv`, a package manager): macOS kills a process that execs an executable overwritten in place, and that host then stays down until `wsx session restart task-<n>`. A host started before this release has no such check: restart it once.

## Claude adapter

`claude -p --input-format stream-json --output-format stream-json --verbose --setting-sources project,local --permission-mode auto --model <model> --append-system-prompt <rules> --settings <sandbox and rules> --json-schema <result schema> --allowedTools <tools> [--plugin-dir <owner skills>]` (see Permissions and Workflow), with `--session-id <uuid>` first and `--resume <uuid>` after a stop or allow. The environment keeps only `HOME USER LOGNAME PATH SHELL LANG LC_ALL LC_CTYPE TMPDIR CLAUDE_CONFIG_DIR ANTHROPIC_API_KEY`, the proxy variables (`HTTPS_PROXY HTTP_PROXY NO_PROXY` and lowercase), `NODE_EXTRA_CA_CERTS SSL_CERT_FILE CARGO_HOME RUSTUP_HOME`; no `WSX_*`. `GIT_EDITOR=true`, since no terminal can edit a commit or rebase message. A stdout line over 4 MiB is dropped in buffer-sized steps.

- Steer: a stream-json user message during a turn. Claude does not save it in its history, so the host logs it. A steer with no running turn is sent as an add.
- Usage (`usage` on `result` and the manager's `reply`): the token counts are the result's, summed over the turn's model calls; `context_tokens` and `model` are the last main-conversation call's (`assistant` lines without `parent_tool_use_id`); `session_cost_micro_usd` is Claude's `total_cost_usd`, the session's running total (a resumed process restores the earlier spend), and `cost_micro_usd` is this turn's part of it, the difference from the session's previous turn (the whole total when it is lower, as after a crash; after `handback` the first turn also counts what the owner's own `claude --resume` of the session spent while worked by hand). Ref: [cost tracking](https://code.claude.com/docs/en/agent-sdk/cost-tracking).
- Only the worker's own `status: blocked` makes the task wait (no checks run): on `permission` when the turn has asks, else on `owner` with the summary's first line as the reason. Tool calls Claude denied (`permission_denials`) are evidence: they are logged on the `result` event, listed in `result.json` (`denials`, also in `show --json`) and under Evidence in the report, and checks still run.
- Result schema: `{status: done|blocked|failed, summary, decisions[{choice, reason}], evidence[{check, proof}], human_verify[{check, where, capture, pass_if}], left_open[], notes, memo}` (`memo`: see Memo; a result without it reads as empty).

## Permissions

Every agent wsx starts (worker, manager) runs in Claude's auto mode with Claude's Bash sandbox, set through `--settings`. The sandbox is `enabled`, `failIfUnavailable`, `autoAllowBashIfSandboxed` and `allowUnsandboxedCommands: false`. Set from `--settings`, the last makes the sandbox admin-required: a repository's own `.claude/settings*.json` cannot add hosts, write paths or excluded commands. Claude's sandbox docs: https://code.claude.com/docs/en/sandboxing.

| Role | Bash writes (the OS enforces) | Also |
|---|---|---|
| worker | its working copy; in the main repository's git folder `objects`, `worktrees/<copy>`, `refs/heads/wsx/` and `logs/refs/heads/wsx/` (what a commit, amend and rebase write); `tasks.sandbox.allowWrite` | `denyWrite` the main repository's `hooks`, `config` and the default branch's ref and reflog; one Unix socket, its task host's broker, for `wsx task check-run` |
| manager | `tasks/<project-id>/manager/notes/` and one file, the project page; `denyWrite` the main checkout | `Edit Write NotebookEdit` denied by name; one Unix socket, the manager host's broker (`network.allowUnixSockets`); its `wsx task` commands run there (see below) |

- Bash needs no rules: any command, compound ones too, runs without a prompt inside the boundary. A write outside fails with `Operation not permitted`.
- Network: `tasks.sandbox.allowedDomains` are reachable. Another host fails once with a `<sandbox_violations>` note; the agent may run the command again with the host listed, and the auto-mode classifier decides.
- File tools (Read, Edit, Write) are not in the OS sandbox. Auto mode accepts edits in the working directory, and its classifier reviews the rest. Deny rules (`Edit(//<path>/**)` for a folder, `Edit(//<path>)` for a file; they cover every file tool) keep them out of the main checkout, the wsx state folder (`$XDG_STATE_HOME/wsx`), the wsx config folders (the platform one and `$XDG_CONFIG_HOME/wsx`) and the project's other task copies `<repo>-t<n>` (those that exist at the launch). Claude merges Edit deny rules into the sandbox, where a deny beats `allowWrite`, so a worker's rules name the main checkout's entries other than `.git` (as they are at the launch); a rule on the whole checkout would stop its commits. The manager, which commits nothing, has a rule on the whole main checkout and no state-folder rule.
- The manager's file tools that write are denied by name (`Edit`, `Write`, `NotebookEdit` in the `--settings` deny list), so all its writes are Bash in the sandbox. Settings deny rules bind its subagents too and beat a skill's `allowed-tools`: neither has a Write tool (checked with Claude Code 2.1.303). A deny rule without a path never reaches the sandbox's `denyWrite`, so Bash still writes the notes and the page. The sandbox gives each `allowWrite` entry as a Seatbelt `subpath`, which for a file is that file alone: Bash rewrites the page in place, and a temporary file beside it fails. MCP tools and hooks that the project settings allow run outside the sandbox, as for every role. `python3 -B scripts/test-manager-boundary.py` checks it with the real Claude, outside any command sandbox.
- `--allowedTools` is `Read Glob Grep` plus `tasks.agent.tools`, and `Skill` with `tasks.skills: owner`. The manager gets `Read Glob Grep WebFetch WebSearch Skill Agent` and `tasks.manager.tools`. A record from before the sandbox drops its `Bash(...)`, `Edit` and `Write` entries at launch.
- Asks: a tool result with a `<sandbox_violations>` network line, a `<path>: Operation not permitted` line, or a denied Write or Edit outside the copy is a stop. When the worker returns `blocked`, the stops of that turn become typed asks: `{kind: network, host}` for a host no later command reached with `allowed_domains`, and `{kind: write, path}` for a folder (`~/<hidden folder>` in home, such as `~/.cargo`, else the parent folder; never the working copy or the main repository's git folder). They are logged on `blocked`, kept in `result.json` and listed in the ledger.
- Grants: `wsx task allow <n> --host <host> | --path <path> | "<rule>"` adds one to `task.json` (`grants`), logs `granted{access, project, by: owner|manager}` and, once every ask of the block is granted (by the task or the project), resumes a task that waits on `permission` or `owner` in the same conversation; before that it answers what the block still asks for. It applies from the next agent start in any case. `--project-wide` keeps it in `tasks/<project-id>/grants.json` (0600, locked writers, never in a checked-in file), which every task of the project reads at its next agent start. A grant is not taken back: a project-wide one is removed from `grants.json` by hand. A rule is refused when it is a Bash rule or a bare file-tool rule, with the reason.
- The manager (`WSX_TASK_ACTOR=manager`) grants a host or a path for one task alone, logged with `by: manager` and shown in the web as a settled line. A project-wide grant and a tool rule ask the owner (see Approvals).
- The manager's broker: the manager host serves `<TMPDIR>/wsx-<uid>/b-<hash>.sock` (for `manager/broker.sock`) and gives the agent `WSX_TASK_BROKER=<that socket>`. The rule and the variable name the socket through its real folder: the sandbox matches only real paths, and on macOS `/var` and `/tmp` are links to `/private/...`. Under it the `wsx` CLI runs nothing itself: a `task` command sends its argv, the folder and, when an argument is `-`, its standard input (UTF-8, at most 1 MiB) to the socket, prints the answer's stdout and stderr and exits with its code; any other command is refused (exit 2). The host refuses argv that is not `task <subcommand>` and the hidden `task host` and `task manager-host`, runs its own wsx binary with `WSX_TASK_ACTOR=manager` in that folder (the main checkout when it is not a folder), keeps at most 1 MiB of stdout and of stderr and stops a command after 15 minutes (exit 124). So a heredoc, `;`, `&&` or a pipe works, and the approval rules still see the manager actor.
- What cannot run in the sandbox: a PTY (`openpty` fails) and Chrome (it needs the per-user temp folder and `~/Library`). Tests that need them belong in `tasks.checks`, which the host runs outside the sandbox.

## Workflow

The stages are fixed: the manager talks the request through with the owner and sizes it, workers build, the host checks and lands, the manager reviews what it created, and after a request's tasks land one housekeeping task consolidates their code. The words each stage gets are the project's:

```yaml
tasks:
  skills: owner          # or none (default): lend workers the owner's skills, as the manager has them
  workflow:
    build: "..."         # added to every worker's instructions
    review: "..."        # added to the manager's review of a task it created
    batch: "..."         # the housekeeping task's prompt
    idle: "..."          # the idle task's prompt; empty (default) is off
```

- Defaults name no skill. `build`: keep the change lean, reuse what exists, remove what the change makes dead, and simplify the diff before finishing. `review`: nothing beyond the fixed review (the owner's words, no copy of an existing helper, no dead code, nothing unasked). `batch`: consolidate the code the tasks changed, merge duplicates, remove dead code and empty layers, keep behavior and public interfaces, lower the net line count; the checks prove the behavior is unchanged.
- Each text is at most 16 KiB, newlines allowed, no other control characters; it is trimmed.
- With `skills: owner` the task host writes the skills plugin into the task folder (`<n>/skills-plugin/`, as the manager's) and passes `--plugin-dir`; the worker gets `Skill` and is told to look at the skills and follow any whose description fits the work. With no skills folder there is no plugin.
- The housekeeping task is an ordinary task the manager starts after the last task of a request lands, when no open task changes the same files. It lands like any other.
- The idle task: when `idle` is set, the manager host itself (not the agent, so it is deterministic) starts one ordinary task at the moment it asks for a recap (see Recap), titled "Consolidate project knowledge", with `idle` as its prompt and `created_by: manager`, so it waits for the manager's review and lands through the normal checks. It starts only when a task other than the last idle task landed since that task started, and that task is no longer open; `state.json` keeps both (`idle_task`, `landed_since_idle`). A quiet project gets one, not one per period. With `recapMinutes: 0` there is no idle task. A failed start is tried again at the next recap.

## Checks, revisions and report

After each result that is not blocked the host runs `tasks.checks` with `/bin/sh -c` in the working copy (`tasks.checkTimeoutSeconds`, default 1200, and the last 8 KiB of each output stream kept while the check runs to its end, no `WSX_*`; a timed-out check's process group is killed), then sets `refs/wsx/task/<n>/r<k>` to the worker's HEAD. The host commits nothing; the worker commits. `ready` needs worker status `done` and every check at exit 0; otherwise `failed`. When the worker said `done` but a check failed, the host first sends the failure back: it logs `check_retry{revision, attempt, commands}` and starts a worker turn with each failed command, how it ended and the last 3 KiB of its output; the checks run again on the result as a new revision. After 2 such turns the task is `failed` as before; a message from the owner or the manager (an add, a grant that resumes) allows 2 more. A landing's checks are never retried. **Check-run.** In the build the worker runs `wsx task check-run` (all checks) or `wsx task check-run 2 3` (by number, as its instructions list them) before it reports done. Under `WSX_TASK_BROKER` the CLI sends the argv to the task host's broker, which runs those checks outside the sandbox exactly as after a result, answers each command and how it ended (the output of a failed one) with exit 0 when all passed, and logs `check_run{results}`; no revision is recorded. Any other argv is refused, and so is a run while the owner works by hand.

**Free disk.** Before an agent turn and before checks the host needs `tasks.minFreeGb` (default 5) free on the working copy's volume. Below it the task waits on `disk` with the reason "Disk full: <n> GB free of the <m> GB a task needs" and logs `disk_wait`; it looks again every 30 s and runs the turn or the checks once there is space, without spending a check retry. Messages that arrive meanwhile join the waiting turn. A check whose output says `No space left on device` waits the same way and runs again, and so does a worker that returns `blocked` while the disk is low: it is told there is space again and continues.

The worker proves what it can: its result has `evidence` (`{check, proof}`, the command or capture path) for each check a command, a test or a capture shows, and `human_verify` only for what needs a person (look, feel, a real device, real data). `report.md` has Summary (with one "Decided:" line when the worker made decisions), Changes (`base..r<k>` commits and diffstat, plus uncommitted files), Evidence ("4 checks passed", each check, then the worker's evidence), To check (the `human_verify` items) and Left open, built from the log, git and `result.json` only.

## To check

There is no verdict per task: a task the checks prove lands, and what needs a person waits in one list per project, `tasks/<project-id>/checks.jsonl`. When a task lands, its host adds the `human_verify` items of the landed revision as `{id: c<n>, task, revision, commit, check, where, capture, pass_if, state: open, note, fix_task, unix_ms}`, once per task and revision, before it sets `landed` (items of a revision whose landing conflicted are kept when the resolving turn leaves them out). The file only grows, under a lock: a later line with the same id is the item's current row.

- A worker's item is `{check, where, capture, pass_if}`: what a person looks at or feels in plain words, where (a URL, a page path or the screen), the path of an image of it in the working copy (or empty) and when it passes. At the land the host copies each capture, a PNG, JPEG or WebP image of at most 2 MiB inside the working copy, to `captures/r<revision>-<n>.<ext>` in the task folder, before the copy goes; another file is dropped with a line in the pane.
- An item whose check or `pass_if` holds a code span names a command, which is the worker's to run. A `done` result with one goes back to the worker before the checks, within the check retries: it runs the command, puts what it shows in evidence and keeps only what a person checks.

- `wsx task check` lists the open items; `show --json` has the task's items as `to_check`; the ledger counts them.
- `check pass <id>...` or `--all` marks open items `passed`; an unknown or settled id refuses the whole call.
- `check fail <id> --note <text>` starts a task titled "Fix: <check>" whose prompt holds the check, `pass_if`, the note and the original `report.md` path; the new record has `fixes: c<n>`, and the item gets `state: failed`, the note and `fix_task`.
- Both refuse with `WSX_TASK_ACTOR=manager`: checking is the owner's.
- A log line that does not parse (a last line cut by a crash) is skipped when a log is read.

## By hand

The owner can take a build over and work on it directly, with the worker's context.

1. `wsx task hand <n>` (the web's *Work by hand*) refuses with `WSX_TASK_ACTOR=manager`, and while the task is `checking`, `landed`, `closed` or already worked by hand. The host stops a running turn, drops queued messages, logs `hand{by: owner, dropped}` and waits on `hand`.
2. No second worktree or branch: the owner works in the task's own working copy on `wsx/t<n>`, with its warm build. `hand` opens a wsxd session `task-<n>-hand` there (one only; a second `hand` names the open one) running `claude --resume <agent_session> --fork-session --system-prompt-snapshot off`, or the login shell when the task has no conversation yet.
   - `--fork-session` copies the conversation under a new id, so the worker's own transcript stays as it was; the next `add` resumes it, and it sees the owner's commits in git.
   - Claude records a conversation's system prompt on its first request and reuses it after `--resume` until the conversation is compacted (https://code.claude.com/docs/en/cli-reference, "System prompt flags in resumed conversations"). `--system-prompt-snapshot off` rebuilds it, so the owner gets Claude's own prompt with their user settings and CLAUDE.md, not the headless worker rules. The worker's `--settings` (its sandbox), `--permission-mode` and tool rules are launch flags and do not carry over.
   - Nothing is written into the working copy. `hand --json` gives `session` and `context`: the working copy, branch, HEAD, a short `git status` and the paths of `report.md` and `result.json` (its `memo`); `show --json` gives the same under `hand` while the task waits on `hand`.
3. While it waits on `hand` nothing else writes the copy or its conversation: `add`, `steer` and `land` are refused with a message that names `handback`; a grant is recorded for the next turn and resumes nothing; every path that starts a turn (a check retry, queued messages, a disk-wait resume) starts none; no checks run, a worker's `check-run` is refused, and nothing lands by itself. The manager reads it from the ledger and leaves the task alone. `close` keeps its rules.
4. `wsx task handback <n>` (the web's *Hand back*) refuses while tracked changes are uncommitted, and the session stays. Otherwise the host logs `handback{by: owner, commits}` (the commits after the last revision), writes a `done` result that keeps the worker's Human Verify items and memo, and runs `tasks.checks` on the next revision: the task becomes `ready` or `failed` as after a worker result, and auto land and the close after a land apply. A failed check is not sent back to the worker; the owner adds a message or hands again. The CLI then closes `task-<n>-hand`; the forked transcript stays in Claude's store.

## Landing

`wsx task land <n>` asks the host to:

1. refuse unless the task is `ready` (a task worked by hand names `handback`);
2. refuse when the worker left uncommitted tracked changes;
3. when the branch does not contain the default branch tip, rebase it there in the working copy:
   - a conflict aborts the rebase, logs `conflict{onto, paths}`, and starts a worker turn that rebases and resolves (the task is `working`); the normal checks then make it `ready`, and it lands again (by itself under auto land);
   - a clean rebase reruns the checks as a new revision and lands when they pass. A failure logs `land_stopped`;
4. fast-forward the default branch: `git merge --ff-only` in the checkout that has it checked out, refused with the changed files named when that checkout has tracked changes; `git update-ref` with the old tip as guard when no checkout has it;
5. add the To check items, log `landed{commit}` and set `landed`. After a rebase the task goes from `checking` straight to `landed`, or to `ready` with `land_stopped`.

**Auto land** (`tasks.land.auto`, default `true`): when the checks of a result pass, at least one check is declared and the task is not held (see Hold and trial), the host lands at once, without an owner or a manager and without approval; the task goes from `checking` to `landed` (or to `working` on a conflict) without a stop at `ready`. A landing that stops (a dirty main checkout, uncommitted work) logs `land_stopped` and leaves the task `ready`. With no checks nothing proves the work, so the task waits `ready` for a land by hand; so does every ready task with `auto: false`, and an old `ready` task when its host restarts.

**Manager review.** A task the manager created (`created_by: manager`) never lands by itself. It waits `ready`, with or without checks, and its event asks the manager to review: the manager reads `git -C <working copy> diff <base>..HEAD` against the owner's words and for lean code (and `tasks.workflow.review`), then runs `wsx task land <n>` or `wsx task add <n> "<what is missing>"`. The manager never sends a task back to rebase: the land rebases and checks again. The web shows such a task as working ("the manager reviews it"), not as a need of the owner.

**After a land** the host starts two detached `wsx task` commands, each in a session of its own with its output in `tasks/<project-id>/follow-up.log` (emptied past 4 MiB): `deploy --if-new` when `tasks.deploy.auto` is on (see Deploy and release), then `close <n>`, which removes the working copy, its build and the branch. The task stays `landed` (its log ends with `closed`), and the task folder and `refs/wsx/task/<n>/r<k>` stay.

## Hold and trial

Some work the owner wants to try before it lands: a larger piece handed over, a long branch, a change that is costly to undo. The manager tells this apart from pipeline work by judgment and holds such a task (`new --hold`, `hold <n> "<why>"`); the owner holds or releases one in the web.

- **Hold.** `hold` (one line, at most 240 characters) is on the record and in the ledger. A held task never lands by itself, also when the owner created it, and the manager lands it only on the owner's word. A held task the manager created is the owner's need, not the manager's review. `hold <n> --off` releases it; a ready task then waits for a land by hand.
- **Trial.** `trial <n>` puts a ready revision on the live site (`tasks.deploy.command`) before it lands. It refuses without a deploy command, with `tasks.deploy.public: true`, and for a manager when `tasks.deploy.approve` is on. The project has one slot, `tasks/<project-id>/trial.json` `{task, revision, commit, by, unix_ms}`, taken under `trial.lock`; a second task's trial is refused until the first ends.
  1. The host rebases the branch onto the default branch tip when it lacks it, and the checks run on the new revision (a conflict starts the worker turn that resolves it and ends the trial). The trial goes live with the revision the owner would land: the host names its commit in the slot, logs `trial{revision, commit}` and starts `wsx task deploy --trial <n>`, which records `trial: <n>` in `deploys.jsonl`.
  2. While the slot is taken, the default branch does not deploy: the `deploy --if-new` after a land answers that it waits, and `deploy` and `release` by hand refuse. Lands go on. Hosts do not follow a replaced wsx binary (see Host), because it may be the trial's build.
  3. It ends when the owner passes it (`land <n> --tried`, owner only, the web's *Pass and land*): the task lands without adding its Human Verify items to To check. It also ends on `trial <n> --end`, on an `add` (the web's *Fail* sends the owner's note this way), on `hand`, on `close`, on failed checks after the rebase and when its deploy fails. The task logs `trial_ended{reason}` and `deploy --if-new` deploys the default branch tip again.
- `deployed` and the ledger's `deploys` carry `trial`: `null`, or the slot with `stage` `preparing` (rebase and checks), `deploying` or `live` (its deploy succeeded).
- A live site cannot always roll back: a stored format, a migration or a newer daemon may stay. Such a change goes to a preview instead (see Preview). For wsx itself, `scripts/deploy-web-live.sh` replaces the binaries and restarts the web server only, so a trial runs its web and CLI against the running daemon. A `wsx daemon restart` during a trial would start its daemon, and the default branch's build cannot take that daemon back: wsxd refuses a handoff to an older build ("handoff target is not newer", checked from the dev build to 0.31.0). The terminals stay, and the newer daemon runs until a cold restart.

## Project page

`tasks/<project-id>/page.md` is the owner's one-page picture of the project: the goal, the tasks and how they relate as a mermaid graph, the plans, the decisions and the open questions. A plan longer than a few lines goes here as its own section with a mermaid diagram, and the chat reply links it. The manager host creates it with those sections when it is missing (0600; an existing page stays). The manager edits it with Bash, which may write that one file (its real path in `allowWrite`; see Permissions): its instructions say to update a section in place when it changes (a short script that reads the page, replaces the section and writes the same file back), never to rewrite the whole file or create another page, and to call it "the project page" in a reply. Workers do not write it: the state folder is denied to their file tools and outside their sandbox. The web client shows it ([web-client-tasks.md](web-client-tasks.md) "Project page").

## Ledger

`wsx task ledger -p <project>` prints JSON of at most 16 KiB, the manager's source of truth:

- `auto_land` and `to_check` (the count of open items);
- `open`: each open task with number, title (80 chars), state, revision, overlaps, the Human Verify item count of its latest result, the last result summary (240 chars), `checks` (each check of the latest result as `{command, outcome: passed|failed|timed out}`, the command at most 120 chars), `waiting` (`{on, reason}`) and the `asks` of a task waiting on `permission`, in attention order (waiting, failed, ready, working and checking);
- `recent`: the last 10 landed or closed tasks, each with its worker's `memo` (600 chars) when it left one (the manager already got it in the task's event, see Memo);
- `deploys`: the `deployed` status, with `configured` per kind (whether its command is set);
- `main`: the main checkout's path, branch and number of changed files (tracked or new).

When it would pass 16 KiB, the memos are left out first, then the least urgent open tasks, counted in `open_omitted`.

## Manager

```sh
wsx task manager -p <project> --json             # start the manager, or report the running one
wsx task say -p <project> "<text>" --json        # or `-` or --prompt-file; returns {id} at once
wsx task chat -p <project> [--after <id>] --json # messages, replies, rotations
```

One manager per project runs as hidden `wsx task manager-host <project-id>` in a wsxd session `task-manager` in the main checkout (no working copy); `say` starts it when it is not running. Its agent uses `tasks.manager` (`model`, default `opus`; `eventModel`, default `off`; `rotateTokens`, default 150000), project and local settings only (so the MCP tools those settings allow), the read-only tools a worker has (`Read Glob Grep WebFetch WebSearch`), `Skill` and `Agent` (subagents such as Explore) plus `tasks.manager.tools`, plain sandboxed Bash, where read-only git works (`GIT_OPTIONAL_LOCKS=0`, so `git status` does not try to write the index), and the manager boundary: it writes only `manager/notes/` and the project page (see Permissions). The owner's skills reach it as a plugin: before each launch the host writes `manager/skills-plugin/` (`.claude-plugin/plugin.json` named `owner`, `skills` a link to `$CLAUDE_CONFIG_DIR/skills` or `~/.claude/skills`) and passes `--plugin-dir`; the skills are `owner:<name>`. With no such folder there is no plugin and the instructions leave the skills out. User settings stay unloaded: they could add hooks, folders and allow rules. Its instructions (`manager.rs` `instructions`): the owner talks to it as to a colleague; for a request for work it first talks it through in the chat (the goal, the size: how many tasks, which run at once because they change different files and which wait, the choices with its recommendation, what it leaves out) and asks whether to go, and starts tasks only after the owner agrees or says to just do it; a longer plan goes on the project page with a mermaid diagram; `task new` gets `--title` (a few plain words, never starting "Owner") and a prompt complete on its own (goal, decisions, scope, expected files, how to prove it), and `--model sonnet` only for an already thorough prompt; overlapping work is one task or waits; it reviews what it created for the owner's words and lean code and never sends a task back to rebase; after a request's last task lands and no open task changes the same files it starts one housekeeping task with `tasks.workflow.batch`; it reports checks from the ledger, never asks the owner to verify a task, never passes or fails To check items, and tells a worker to prove what a command could; it handles each `waiting.on` (grants for one task, an `add` or a question to the owner, nothing for disk or hands) and never tells the owner to edit settings files; it keeps notes and decisions, folds each memo when its task lands or closes and again on rotation, reads other projects' ledgers and messages their managers, loads context read-only, uses the skills and subagents whose descriptions fit lookup or research, and keeps answers short with no commands for the owner. A running manager keeps the instructions its conversation started with until it rotates; a wsx with changed instructions starts each manager's next turn as a fresh conversation (below), which opens with its notes and decisions. Its environment is the worker allowlist plus `WSX_TASK_BROKER`, `WSX_TASK_ACTOR=manager` and `GIT_OPTIONAL_LOCKS=0`; the host puts its own `wsx` first on `PATH` and runs the commands with its own environment (see Permissions).

- `manager/log.jsonl` (rolls to `log.1.jsonl` past 8 MiB) has `message{id, from: owner|events, text}`, `delivered{ids}`, `reply{id, to, text, usage}`, `agent` (its commands), `rotated{tokens}` and `stopped`. `manager/state.json` keeps the conversation, the digest of its instructions, the next id and what was already reported.
- One turn at a time. Waiting owner messages go together in one turn, each headed with its id, before waiting events.
- Events: the host checks the task records every second. A change of (state, revision) into `ready`, `failed`, `landed`, `closed` or `waiting` (not on `hand`) becomes a line written by wsx, `#<n> <state> r<k>: <title, 80 chars>; [waits on <on>: <reason>; ]checks <passed> of <total> passed; worker says: <summary, 160 chars>` (without the checks part when there are none); `landed` and `closed` add "; memo: <memo, one line, 600 chars>" when the worker left one, and `landed` adds "; <n> to check" when the landing added items. Closing a landed task keeps `landed`, so it sends no second event. Lines are logged as one `events` message when 3 s have passed since the last one and no turn runs, so events during a turn or close together arrive merged.
- Restart: `state.json` records the reported state of each task only when its message is logged, and `delivered` marks what reached the agent. A restarted host queues logged messages that were never delivered and reports changes it has not reported, so no event is lost. Its first start takes the existing tasks as they are, without events.
- Messages from another project's manager are headed `Manager <project>` (or `Manager <project> replies`) and go after owner messages and before events.
- Every turn's message ends with a ledger summary the host writes: `Ledger summary from wsx (...)`, the open tasks by state, the To check and waiting approval counts, the tasks whose (state, revision) changed since the last turn (every open task in a fresh conversation), at most 12 lines of `#<n> <state> r<k>: <title, 60 chars>`, and the last deploy when one is configured. `state.json` keeps what the last summary showed (`turn_seen`). The manager runs `wsx task ledger` only when it needs more, so a turn does not start with a 16 KiB tool result that every later call of the turn reads again.
- Model per turn: a turn of only events runs on `eventModel` while no ready task the manager created waits for its review; any other turn runs on `model`. The host switches the running process with the stream-json control request `set_model` (the SDK's `setModel`), so the conversation stays. Claude's prompt cache is per model, so a switch writes the conversation into the other model's cache: on scripts/bench-manager.py a Sonnet event turn cost a median 0.063 USD against 0.023 USD on Opus, hence `off`.
- After a reply whose context passes `rotateTokens`, the next turn starts a fresh conversation and the log has `rotated`; the ledger carries the state. The context is the last model call's (`usage.context_tokens`), not the result's sum over the turn's calls: a turn of 5 tool calls at 30k counts 30k, not 150k. Rotating on the sum made nearly every turn a fresh conversation (19 of 21 turns on the owner's log), at a median 0.32 USD a turn against 0.02 USD in a kept conversation.
- Claude keeps the system prompt a conversation began with, also on `--resume`; settings, tools and model apply at every start. `state.json` keeps a digest of the instructions the conversation began with (`instructions`). A host whose instructions differ, or a state without the digest (from an older host), starts a fresh conversation and logs `rotated{tokens: 0, reason}`.
- The first turn of every fresh conversation opens with the manager's notes: `notes/decisions.md` newest line first, then `notes/context.md`, at most 8 KiB together. After a rotation a handover follows: the last 4 owner messages and replies, each at most 600 characters.
- A worker conversation is never rotated: it holds the task's work, and its rules that matter at run time (sandbox, tools) come with each start.

The web client shows the manager chat, the tasks, their reports and their fixed actions, and notifies about them: [web-client-tasks.md](web-client-tasks.md).

## Managers talk

A manager may reach the manager of another registered project, and nothing else of it:

- `wsx task ledger -p <other>` reads its state.
- `wsx task say -p <other> --from-manager "<text>"` sends it a message. The manager host's broker sets `WSX_TASK_FROM` to its own project, and the message is logged in the other manager's log as `message{from: "manager:<project>"}`. The sender's log gets `sent{to, text}`. The web shows both as bubbles: "Manager <project>" in the receiving chat, "To Manager <other>" in the sending one; a project with `tasks.manager.name` shows that name instead ("Atlas", "To Atlas"), read only from projects the site grants.
- `--reply` answers such a message (`from: "manager-reply:<project>"`). Each delivered message from a manager allows one reply to it, and a reply allows none, so no two managers answer each other in a loop.
- Every other command with `-p <other>`, and `--from-manager` to its own project, is refused by the broker. A manager creates and changes tasks only in its own project.

## Recap

After `tasks.manager.recapMinutes` (default 60, `0` is off, at most 1440) with no message and no turn, and only when a message other than a recap request came since the last recap, the manager host logs a message from `recap` that asks the manager for a recap. The request is fixed text written by wsx: a recap for someone back after two weeks with a faint memory of the project, the goal first, then what landed and what it changed, what waits for the owner and why, what runs, and the open decisions as a table, read from the ledger and the notes, with no task started. At the same moment the host may start the idle task (see Workflow). The manager answers in an ordinary turn. The web does not show the request and tags the answer "Recap". A quiet project gets one recap, not one per period.

## Memo

Durable project knowledge moves from workers to the manager, kept small:

1. The worker's instructions: before a build finishes, put in the result's `memo` a short, lean memo of durable project knowledge a later task needs (a convention, a gotcha, a decision and its reason), a few lines at most; empty when there is none. It is kept in `result.json`.
2. The event of a task that lands or closes carries its memo (see Manager), so every memo reaches the manager when its task leaves, however many tasks leave before a rotation. The manager's instructions: in that turn, fold each memo into `manager/notes/context.md` with Bash (lean, duplicates merged, stale lines dropped, at most 60 lines); read that file when a conversation starts.
3. The ledger shows the memo on each `recent` task (see Ledger). After a rotation the fresh conversation's first message always opens with `Handover`, which asks the manager to fold the ledger's memos first, as a catch-up for one a turn missed.

A project whose skills keep knowledge in files opts in through its own words: `tasks.workflow.build` can tell workers to save it with a skill, and `tasks.workflow.idle` to consolidate it (see Workflow). wsx names no skill.

## Preview

For a project with `tasks.preview`, a person can try a task on a link. wsx configures none: a wsx change lands and deploys to the live app.

1. `wsx task preview <n>` (the manager on "try #n", the web's *Try* on a To check row and in the task sheet) answers the running preview, or starts a detached `wsx task preview-host`.
2. The preview host takes the first port of `tasks.preview.ports` that nothing listens on and runs `tasks.preview.command` with `$PORT` in its own process group: in the task's working copy, or for a landed task in a temporary copy `<repo>-preview-<n>` of its landed commit. Output goes to `preview.log` in the task folder.
3. Once `GET <ready>` answers 200 (within 180 s), it runs `tailscale serve --bg --https=<port> http://127.0.0.1:<port>` (tailnet only, port-scoped, never `reset`), writes `preview.json` `{url, port, pid, until_unix_ms}` with `url` `https://<this machine's tailnet name>:<port>/`, and logs `preview{url, port}`.
4. It stops after 60 minutes, on `preview <n> --stop` (`preview.stop`), when the command exits and when the task closes. Then it turns the Serve mapping off, ends the process group (TERM, then KILL after 5 s) and removes the temporary copy after `cargo clean`. A start that fails leaves `preview.error`, which `preview` answers.

## Approvals

Approvals are only for what goes public or is lost. `tasks.land.approve` and `tasks.deploy.approve` (default `false`) and `tasks.release.approve` (default `true`) decide whether the manager may land, deploy or release alone. A `wsx task` command run with `WSX_TASK_ACTOR=manager` (the manager host sets it for its agent) that needs approval does not act: it appends `request{id: a<n>, action, task, revision, commit}` to `tasks/<project-id>/approvals.jsonl` and answers that the owner was asked. `close --discard` by the manager always asks, and so do its project-wide grants and tool rules (`request{action: grant, grant{access, project_wide}}`, not tied to a revision); see Permissions. The ledger lists pending requests under `approvals`.

```sh
wsx task approve a3 -p <project> --json            # run the action as the owner would
wsx task deny a3 [--note <text>] -p <project> --json
```

Both refuse when `WSX_TASK_ACTOR=manager`, and so do `hand` and `handback`: the manager may tell the owner to run them. In the web a second Approve while the action runs, or after it was decided, answers what happened instead of an error. `approve` refuses a land or discard for a task that moved past the requested revision (recorded as not run; ask again). Otherwise it runs the action as its own `wsx task ... --json` command without the actor mark and records `decision{id, approved, note, outcome}`. Decisions reach the manager as events (`approval a3 (land #5) approved; done: ...`). Writers lock the file; requests are numbered under that lock.

This is a guard against the manager's mistakes, not a security boundary: the manager's tools allow every `wsx task` command, and anything running as the owner can leave out `WSX_TASK_ACTOR`. An OS sandbox per agent comes later.

## Deploy and release

```sh
wsx task deploy -p <project> [--commit <ref> | --if-new] --json    # tasks.deploy.command
wsx task release -p <project> [--commit <ref>] --json   # tasks.release.command
wsx task deployed -p <project> --json
```

Deploys and releases of a project run one at a time under `tasks/<project-id>/deploy.lock`. `--if-new` waits for that lock and then does nothing when the last successful deploy is already the default branch tip, so the deploys that quick lands start run once for the newest commit. With `tasks.deploy.auto: true` every land starts one (see Landing); `tasks.release` has no `auto`.

`release` refuses while To check items are open, and so does `deploy` with `tasks.deploy.public: true`, before anything runs or is asked; a deploy that is not public runs and its answer ends "<n> to check on the deployed site". `deploy` and `release` make a temporary detached working copy of one commit (default: the default branch tip) beside the main checkout, run the command there with `/bin/sh -c` and `WSX_DEPLOY_COMMIT`, record `{kind, commit, exit, unix_ms, duration_ms}` in `tasks/<project-id>/deploys.jsonl`, then `cargo clean` and remove the copy. They never run in a task's working copy. With `--json` the command's output goes to stderr. `deployed` gives, per kind, whether its command is set (`configured`), the last successful commit, how many default-branch commits it lacks (`behind`) and how many attempts failed since (`failed_since`).

## Close

`close` refuses while `wsx/t<n>` has commits that are not on the default branch, unless `--discard` or the task is `landed`; a task worked by hand closes by the same rules, its hand session with the other sessions. A landed task closes itself (see Landing) and keeps the state `landed` unless `--discard`. It stops the task's preview, asks the host to close, waits for it to exit, removes the sessions of the working copy, runs `cargo clean` when the copy has `Cargo.toml`, and removes the worktree, folder and branch. `--discard` also deletes `refs/wsx/task/<n>/*`. The task folder stays as history (`list --all`).

## Config

```yaml
tasks:
  agent: { adapter: claude, model: opus, tools: [WebFetch] }   # never Bash rules
  skills: none                                                   # owner: lend workers the owner's skills (see Workflow)
  workflow: { build: "...", review: "...", batch: "...", idle: "" }   # see Workflow
  sandbox: { allowedDomains: [crates.io, "*.npmjs.org"], allowWrite: ["~/.cargo/registry"] }
  checks: ["cargo fmt --check"]
  checkTimeoutSeconds: 1200
  minFreeGb: 5
  land: { approve: false, auto: true }
  deploy: { command: "sh deploy.sh", approve: false, public: false, auto: true }
  release: { command: "sh release.sh", approve: true }
  preview: { command: "npm run serve -- --port $PORT", ready: /health, ports: 9100-9199 }
  manager: { name: Atlas, adapter: claude, model: opus, eventModel: off, rotateTokens: 150000, recapMinutes: 60, tools: [] }   # never Bash rules; eventModel: a model or off
```

`tasks.manager.name` is the name the owner reads for the manager: the web chat's author and typing bubble ("Atlas is typing"), another project's chat, the TUI row (`task-manager (Atlas)`) and `wsx task manager --json` (`name`). It is trimmed, one line without control characters, at most 64 bytes; empty is no name, and without one the manager reads "Manager" (the TUI keeps `(manager)`). Inside, the manager stays `manager`: the session label `task-manager`, the provider, `WSX_TASK_ACTOR` and `from: manager:<project>`; the name is not in its instructions. The name is the manager's address for a later talk between managers by name.

Defaults: adapter `claude`, worker and manager model `opus` (`task new --model` overrides the worker's), no extra tools, no owner skills, the default workflow words, an empty sandbox list, no checks, `minFreeGb: 5` (0..=100000), `deploy.auto: false`, no preview (`ready` defaults to `/`, `ports` to `9100-9199`, above 1023). `tasks.agent.tools` and `tasks.manager.tools` refuse `Bash(...)` (the sandbox replaces it) and bare `Edit`, `Write`, `NotebookEdit`, `MultiEdit` (they would skip the classifier). An `Edit(<path>)` or `Write(<path>)` rule in `tasks.manager.tools` gives the manager no file tool: they are denied by name. `allowedDomains` are host names with an optional `*.`; `allowWrite` paths are absolute or `~/`, below the root or home, without `.` or `..`. Up to 64 tools, 64 checks and 64 entries per sandbox list; `checkTimeoutSeconds` is 1..86400; a tool or model is at most 256 bytes and a check 16 KiB. `task new` refuses a config that does not parse.

## Verify

```sh
cargo build -p wsx -p wsx-daemon && python3 scripts/test-task.py
```

The script runs an isolated wsxd with `scripts/fake-claude-stream.py` as `claude`.
