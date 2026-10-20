# Tasks (manager mode M1)

A task is one headless worker on its own branch and working copy. The caller never types into it. Every action is one CLI call; `--json` works on all of them. No wsxd protocol change.

```sh
wsx task new "<prompt>" -p <project> [--from <ref>] [--no-brief] [--title <words>] [--model <name>] --json   # or `-` (stdin) or --prompt-file <path>
wsx task start <n> --json                        # build a briefed task against its rewritten prompt
wsx task list -p <project> [--all] --json
wsx task show <n> -p <project> --json            # record, result.json, report path, host_running, socket
wsx task steer <n> "<text>" --json               # into the running turn
wsx task add <n> "<text>" --json                 # after the turn (queued while one runs)
wsx task allow <n> --host <host> | --path <path> | "<rule>" [--project-wide] --json   # grant; resumes a blocked task
wsx task revoke <n> --host <host> | --path <path> | "<rule>" [--project-wide] --json  # from the next turn
wsx task stop <n> --json                         # interrupt; the next add resumes the conversation
wsx task verify <n> pass|fail [--note <text>] --json   # human verdict on the current revision
wsx task land <n> [--skip-verify] --json         # onto the default branch by tasks.land.policy
wsx task close <n> [--discard] --json
```

`-p` may be left out inside a project folder or when one project is registered. A task number is `3` or `t3`. `new --model <name>` picks the worker's model (one word, at most 256 bytes, not starting with `-`); without it the task uses `tasks.agent.model` (default `opus`). The record keeps it for the brief and the build.

## Store

`$XDG_STATE_HOME/wsx/tasks/<project-id>/<n>/`, each folder 0700 and each file 0600, opened without following links.

| File | Content | Limit |
|---|---|---|
| `task.json` | id, project, base, branch `wsx/t<n>`, working copy, pane, adapter, model, tools, checks, agent session, state, revision | 64 KiB |
| `log.jsonl` | append-only: `created`, `message{prompt\|steer\|add}`, `agent` (short progress), `blocked{denials, asks}`, `granted{access, project, by}`, `revoked`, `result`, `checks`, `check_retry`, `revision`, `stopped`, `closed` | 8 MiB for progress, 16 MiB hard |
| `result.json` | latest worker result and check results, `denials`, and `asks` of a block | 256 KiB |
| `report.md` | report of the latest revision | |
| `brief.json`, `brief.md` | the latest brief (`{version, brief}`) and its text | 256 KiB |

- A host's control socket is not in the store: `sun_path` holds 104 bytes on macOS. The task host serves `<TMPDIR>/wsx-<uid>/t-<hash>.sock` and the manager `m-<hash>.sock`, where `<hash>` is 16 hex of sha256 of the store path the socket stands for (`<n>/host.sock`, `manager/manager.sock`), and `/tmp` replaces a `TMPDIR` longer than 64 bytes. The folder is 0700 and must belong to the user, the socket is 0600, a host removes its socket when it exits and replaces a stale one when it starts. A host and its callers must share `TMPDIR`; `show --json` and `manager --json` name the socket.
- `<project-id>` is `<name-slug>-<first 8 hex of sha256(project path)>`. Numbers come from a locked `next` counter and are never reused, also after a failed start.
- One writer: the CLI writes before the host starts and after it exits; the host writes everything in between.
- States: `starting, working, briefed, blocked, checking, ready, failed, stopped, conflict, in_review, landed, closed`. `landed` and `closed` are not open. `task.json` also has `phase`: `brief` until `start`, then `build` (records from before the brief read as `build`).
- A prompt, steer or add is at most 64 KiB.
- A task's title is `--title` (at most 60 characters, no control characters, kept in `task.json` as `title`), else the first line of its prompt, at most 120 characters. Either one is shown without a leading `Owner <words>:` (up to 80 characters before the colon; a colon inside brackets or without a space after it, as in `http://`, does not end it): `Owner report: tool permission` reads `tool permission`. A lead-in with nothing after it reads as the words in its brackets (`Owner report (UX failures):` reads `UX failures`), else as the prompt's next non-empty line. `Owner: x` has no words and stays. Only the shown title changes: `task.json` and the prompt keep what was written. Every title shown (ledger, events, the web, a pull request) comes from this rule, also for tasks from before `--title`.
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

A host follows a deploy of wsx. It records the identity (device, inode, size, modification time) of the file it was started as (its argv[0], which `task new` and `say` make absolute). Before each agent turn (an add, a resuming grant, `start`, a check retry, delivered queued messages) a task host that finds another file there writes the turn's text to `handoff.txt` (0600), sets `working`, answers its caller and execs that file with the same arguments: the pane and pid stay, and the new host starts the turn with the new launch, resuming the same conversation. The manager host does the same before its next turn; its messages stay logged and undelivered, so the new host queues them again. `wsx task` actions (CLI and web) retry the connect for up to 3 s while the socket is gone. Install by replacing the file (`cargo install`, `install`, `mv`, a package manager): macOS kills a process that execs an executable overwritten in place, and that host then stays down until `wsx session restart task-<n>`. A host started before this release has no such check: restart it once.

## Claude adapter

`claude -p --input-format stream-json --output-format stream-json --verbose --setting-sources project,local --permission-mode auto --model <model> --append-system-prompt <rules> --settings <sandbox and rules> --json-schema <result schema> --allowedTools <tools> [--disallowedTools <tools>]` (see Permissions), with `--session-id <uuid>` first and `--resume <uuid>` after a stop or allow. The environment keeps only `HOME USER LOGNAME PATH SHELL LANG LC_ALL LC_CTYPE TMPDIR CLAUDE_CONFIG_DIR ANTHROPIC_API_KEY`, the proxy variables (`HTTPS_PROXY HTTP_PROXY NO_PROXY` and lowercase), `NODE_EXTRA_CA_CERTS SSL_CERT_FILE CARGO_HOME RUSTUP_HOME`; no `WSX_*`. `GIT_EDITOR=true`, since no terminal can edit a commit or rebase message. A stdout line over 4 MiB is dropped in buffer-sized steps.

- Steer: a stream-json user message during a turn. Claude does not save it in its history, so the host logs it. A steer with no running turn is sent as an add.
- Only the worker's own `status: blocked` makes the task `blocked` (no checks run). Tool calls Claude denied (`permission_denials`) are evidence: they are logged on the `result` event, listed in `result.json` (`denials`, also in `show --json`) and under Evidence in the report, and checks still run.
- Result schema: `{status: done|blocked|failed, summary, decisions[{choice, reason}], human_verify[{check, pass_if}], left_open[], notes, memo}` (`memo`: see Memo; a result without it reads as empty).

## Permissions

Every agent wsx starts (worker, brief turn, manager) runs in Claude's auto mode with Claude's Bash sandbox, set through `--settings`. The sandbox is `enabled`, `failIfUnavailable`, `autoAllowBashIfSandboxed` and `allowUnsandboxedCommands: false`. Set from `--settings`, the last makes the sandbox admin-required: a repository's own `.claude/settings*.json` cannot add hosts, write paths or excluded commands. Claude's sandbox docs: https://code.claude.com/docs/en/sandboxing.

| Role | Bash writes (the OS enforces) | Also |
|---|---|---|
| worker | its working copy; in the main repository's git folder `objects`, `worktrees/<copy>`, `refs/heads/wsx/` and `logs/refs/heads/wsx/` (what a commit, amend and rebase write); `tasks.sandbox.allowWrite` | `denyWrite` the main repository's `hooks`, `config` and the default branch's ref and reflog |
| brief | nothing: `denyWrite` the working copy | `--disallowedTools Edit Write NotebookEdit` |
| manager | `tasks/<project-id>/manager/notes/` and one file, the project page; `denyWrite` the main checkout | `Edit Write NotebookEdit` denied by name; one Unix socket, the manager host's broker (`network.allowUnixSockets`); its `wsx task` commands run there (see below) |

- Bash needs no rules: any command, compound ones too, runs without a prompt inside the boundary. A write outside fails with `Operation not permitted`.
- Network: `tasks.sandbox.allowedDomains` are reachable. Another host fails once with a `<sandbox_violations>` note; the agent may run the command again with the host listed, and the auto-mode classifier decides.
- File tools (Read, Edit, Write) are not in the OS sandbox. Auto mode accepts edits in the working directory, and its classifier reviews the rest. Deny rules (`Edit(//<path>/**)` for a folder, `Edit(//<path>)` for a file; they cover every file tool) keep them out of the main checkout, the wsx state folder (`$XDG_STATE_HOME/wsx`), the wsx config folders (the platform one and `$XDG_CONFIG_HOME/wsx`) and the project's other task copies `<repo>-t<n>` (those that exist at the launch). Claude merges Edit deny rules into the sandbox, where a deny beats `allowWrite`, so a worker's rules name the main checkout's entries other than `.git` (as they are at the launch); a rule on the whole checkout would stop its commits. The manager, which commits nothing, has a rule on the whole main checkout and no state-folder rule.
- The manager's file tools that write are denied by name (`Edit`, `Write`, `NotebookEdit` in the `--settings` deny list), so all its writes are Bash in the sandbox. Settings deny rules bind its subagents too and beat a skill's `allowed-tools`: neither has a Write tool (checked with Claude Code 2.1.303). A deny rule without a path never reaches the sandbox's `denyWrite`, so Bash still writes the notes and the page. The sandbox gives each `allowWrite` entry as a Seatbelt `subpath`, which for a file is that file alone: Bash rewrites the page in place, and a temporary file beside it fails. MCP tools and hooks that the project settings allow run outside the sandbox, as for every role. `python3 -B scripts/test-manager-boundary.py` checks it with the real Claude, outside any command sandbox.
- `--allowedTools` is `Read Glob Grep` plus `tasks.agent.tools`. The manager gets `Read Glob Grep WebFetch WebSearch Skill Agent` and `tasks.manager.tools`. A record from before the sandbox drops its `Bash(...)`, `Edit` and `Write` entries at launch.
- Asks: a tool result with a `<sandbox_violations>` network line, a `<path>: Operation not permitted` line, or a denied Write or Edit outside the copy is a stop. When the worker returns `blocked`, the stops of that turn become typed asks: `{kind: network, host}` for a host no later command reached with `allowed_domains`, and `{kind: write, path}` for a folder (`~/<hidden folder>` in home, such as `~/.cargo`, else the parent folder; never the working copy or the main repository's git folder). They are logged on `blocked`, kept in `result.json` and listed in the ledger.
- Grants: `wsx task allow <n> --host <host> | --path <path> | "<rule>"` adds one to `task.json` (`grants`), logs `granted{access, project, by: owner|manager}` and, once every ask of the block is granted (by the task or the project), resumes a blocked task in the same conversation; before that it answers what the block still asks for. It applies from the next agent start in any case. `--project-wide` keeps it in `tasks/<project-id>/grants.json` (0600, locked writers, never in a checked-in file), which every task of the project reads at its next agent start. `revoke` takes one back from the next agent start. A rule is refused when it is a Bash rule or a bare file-tool rule, with the reason.
- The manager (`WSX_TASK_ACTOR=manager`) grants a host or a path for one task alone, logged with `by: manager` and shown in the web as a settled line. A project-wide grant and a tool rule ask the owner (see Approvals); it cannot revoke a project-wide grant.
- The manager's broker: the manager host serves `<TMPDIR>/wsx-<uid>/b-<hash>.sock` (for `manager/broker.sock`) and gives the agent `WSX_TASK_BROKER=<that socket>`. The rule and the variable name the socket through its real folder: the sandbox matches only real paths, and on macOS `/var` and `/tmp` are links to `/private/...`. Under it the `wsx` CLI runs nothing itself: a `task` command sends its argv, the folder and, when an argument is `-`, its standard input (UTF-8, at most 1 MiB) to the socket, prints the answer's stdout and stderr and exits with its code; any other command is refused (exit 2). The host refuses argv that is not `task <subcommand>` and the hidden `task host` and `task manager-host`, runs its own wsx binary with `WSX_TASK_ACTOR=manager` in that folder (the main checkout when it is not a folder), keeps at most 1 MiB of stdout and of stderr and stops a command after 15 minutes (exit 124). So a heredoc, `;`, `&&` or a pipe works, and the approval rules still see the manager actor.
- What cannot run in the sandbox: a PTY (`openpty` fails) and Chrome (it needs the per-user temp folder and `~/Library`). Tests that need them belong in `tasks.checks`, which the host runs outside the sandbox.

## Brief

With `tasks.brief` (default `true`) a new task first returns a brief and builds nothing. `new --no-brief` skips it, and so does `tasks.brief: false`; both log `brief_skipped{by: flag|config, actor: owner|manager}`. The manager passes `--no-brief` only when the owner's message asks to skip the brief.

1. The first turn asks the worker, as a top professional in the field, for the brief of the owner's prompt. The turn runs read-only (see Permissions): its Bash writes nothing, not even the working copy, and `--disallowedTools Edit Write NotebookEdit`. A denied file edit is logged with the brief.
2. The turn ends with structured output (`--json-schema`, the brief schema): `additions[{what, why}]` (what a top professional would add), `raw_material[{source, why}]` (primary sources by path or link), `methods[]`, `independent_check[]` (what, other than the owner or an agent, shows the result is right), `concepts[{term, explanation}]` (terms the owner is likely new to), `rewritten_prompt`, `open_questions[]` and `answer`. The host writes `brief.json` as the next version and renders `brief.md` with the sections in that order, logs `brief{version, answer, usage, denials}` and sets `briefed`. A turn without a usable brief (no output, or no rewritten prompt) logs a failed `result` and sets `failed`.
3. An `add` before the build is a talk-through: the worker answers the reply in `answer` (logged with the brief, so the conversation reads as reply and answer) and returns the whole brief again, revised where the reply changes it, still read-only. An `allow` before the build adds to the brief tools.
4. `wsx task start <n>` needs `briefed`. It logs `started{version, prompt}` with the rewritten prompt (the first prompt stays in the log), sets `phase: build`, restarts the agent with the build tools and the result schema and resumes the same conversation with the rewritten prompt. Under `WSX_TASK_ACTOR=manager` it records an approval request "Approve start #n" instead. `close` works in every state.

The worker's system prompt is the same in both phases, because Claude records a conversation's first system prompt and reuses it on resume (`--system-prompt-snapshot`); the user messages say which phase runs. `report.md` lists the build prompt under Asked as "Build prompt (brief v<k>)". The host reports `briefed` to wsx lists as Blocked (waits for the owner).

## Checks, revisions and report

After each result that is not blocked the host runs `tasks.checks` with `/bin/sh -c` in the working copy (`tasks.checkTimeoutSeconds`, default 1200, and 8 KiB of output each, no `WSX_*`; a timed-out check's process group is killed), then sets `refs/wsx/task/<n>/r<k>` to the worker's HEAD. The host commits nothing; the worker commits. `ready` needs worker status `done` and every check at exit 0; otherwise `failed`. When the worker said `done` but a check failed, the host first sends the failure back: it logs `check_retry{revision, attempt, commands}` and starts a worker turn with each failed command, how it ended and the last 3 KiB of its output; the checks run again on the result as a new revision. After 2 such turns the task is `failed` as before; a message from the owner or the manager (an add, a grant that resumes, `start`) allows 2 more. A landing's checks are never retried. `report.md` has Asked, Summary, Decisions, Changes (`base..r<k>` commits and diffstat, plus uncommitted files), Evidence, Human Verify and Left open, built from the log, git and `result.json` only.

## Human verification

`wsx task verify <n> pass|fail [--note <text>]` logs `verified{revision, result, note}` for the current revision through the host. The verdict that applies is the latest one for the record's revision, so a new revision has none: it needs a new verdict or a carried pass (see Landing). A fail needs `--note`; the note is logged verbatim as a `verify` message (Asked shows it as "Human verification (fail)") and goes to the worker, as a steer during a turn or as an add after it. `report.md` ticks the Human Verify items on a pass and ends the section with the verdict, its time and note; `show --json` has `verification`.

## Landing (`local-merge`, the default policy)

`wsx task land <n>` asks the host to:

1. refuse unless the task is `ready`, and when the result has Human Verify items and the current revision has no `pass` (`--skip-verify` lands anyway and logs `verify_skipped`);
2. refuse when the worker left uncommitted tracked changes;
3. when the branch does not contain the default branch tip, rebase it there in the working copy:
   - a conflict aborts the rebase, logs `conflict{onto, paths}`, and starts a worker turn that rebases and resolves. The state stays `conflict` until the result; the normal checks then make it `ready`, and the caller lands again;
   - a clean rebase reruns the checks as a new revision and lands when they pass. A `pass` on the old revision is carried (`verified{carried_from}`) only when the default branch changed none of the task's paths since its old base; otherwise the new revision needs a new verdict. A failure logs `land_stopped`;
4. fast-forward the default branch: `git merge --ff-only` in the checkout that has it checked out, refused with the changed files named when that checkout has tracked changes; `git update-ref` with the old tip as guard when no checkout has it;
5. log `landed{commit}` and set `landed`. After a rebase the task goes from `checking` straight to `landed`, or to `ready` with `land_stopped`.

## Landing (`pr`)

With `tasks.land.policy: pr`, the first `land` of a ready task applies the same verify and commit gates, pushes `HEAD` to `wsx/t<n>` on `tasks.land.remote` (default `origin`, `--force-with-lease`), runs `gh pr create --base <default> --head wsx/t<n> --title <first prompt line, 72 chars> --body-file report.md`, records `pr_url` and `pr_revision`, logs `in_review{url, revision}` and sets `in_review`. The branch is not rebased first; the remote shows and merges conflicts. A later `land` runs `gh pr view <url> --json state,mergeCommit`: `MERGED` logs `landed{commit: mergeCommit.oid}` and sets `landed` (the local default branch is not changed); `CLOSED` logs `pr_closed` and the task stays `in_review`; an open PR gets the newer revision pushed when the task is `ready` with one, otherwise the answer says it is still open. Push and `gh` each have 120 s.

## Project page

`tasks/<project-id>/page.md` is the owner's one-page picture of the project: the goal, the tasks and how they relate as a mermaid graph, the decisions and the open questions. The manager host creates it with those sections when it is missing (0600; an existing page stays). The manager edits it with Bash, which may write that one file (its real path in `allowWrite`; see Permissions): its instructions say to update a section in place when it changes (a short script that reads the page, replaces the section and writes the same file back), never to rewrite the whole file or create another page, and to call it "the project page" in a reply. Workers do not write it: the state folder is denied to their file tools and outside their sandbox. The web client shows it ([web-client.md](web-client.md) "Project page").

## Ledger

`wsx task ledger -p <project>` prints JSON of at most 16 KiB, the manager's source of truth:

- `open`: each open task with number, title (80 chars), state, revision, overlaps, the current verdict (without its note), the Human Verify item count, the last result summary (240 chars), `checks` (each check of the latest result as `{command, outcome: passed|failed|timed out}`, the command at most 120 chars), `pr_url`, `phase`, the `asks` of a blocked task and, before the build, `brief{version, rewritten_prompt}` (240 chars), in attention order (blocked, failed, conflict, briefed, ready, in_review, working, stopped);
- `recent`: the last 10 landed or closed tasks, each with its worker's `memo` (600 chars) when it left one;
- `deploys`: the `deployed` status, with `configured` per kind (whether its command is set); under `pr` it uses the last fetched `<remote>/<default>` and does not fetch;
- `main`: the main checkout's path, branch and number of changed files (tracked or new).

When it would pass 16 KiB, the memos are left out first, then the least urgent open tasks, counted in `open_omitted`.

## Manager

```sh
wsx task manager -p <project> --json             # start the manager, or report the running one
wsx task say -p <project> "<text>" --json        # or `-` or --prompt-file; returns {id} at once
wsx task chat -p <project> [--after <id>] --json # messages, replies, rotations
```

One manager per project runs as hidden `wsx task manager-host <project-id>` in a wsxd session `task-manager` in the main checkout (no working copy); `say` starts it when it is not running. Its agent uses `tasks.manager` (`model`, default `opus`; `rotateTokens`, default 150000), project and local settings only (so the MCP tools those settings allow), the read-only tools a worker has (`Read Glob Grep WebFetch WebSearch`), `Skill` and `Agent` (subagents such as Explore) plus `tasks.manager.tools`, plain sandboxed Bash, where read-only git works (`GIT_OPTIONAL_LOCKS=0`, so `git status` does not try to write the index), and the manager boundary: it writes only `manager/notes/` and the project page (see Permissions). The owner's skills reach it as a plugin: before each launch the host writes `manager/skills-plugin/` (`.claude-plugin/plugin.json` named `owner`, `skills` a link to `$CLAUDE_CONFIG_DIR/skills` or `~/.claude/skills`) and passes `--plugin-dir`; the skills are `owner:<name>`. With no such folder there is no plugin and the instructions leave the skills out. User settings stay unloaded: they could add hooks, folders and allow rules. Its instructions: never edit the project's files or run builds; keep notes only in `manager/notes/`; read `wsx task ledger` first; for each owner message answer, add, steer, start, verify or land; keep turns short; name tasks `#<n>`. Asked for work, it runs `task new` (brief on), tells the owner the brief is coming and never starts a build alone; it adds `--no-brief` only when the owner's message asks to skip the brief ("just do it", "no brief"); on "start #n" it runs `task start <n>`, which asks the owner's approval; a reply about a brief goes to the task as an `add`; status answers name the briefs that wait; `wsx task` commands work in any command shape, and no other `wsx` command runs; a blocked task with asks it fixes itself, by granting the host or folder for that task when the task plainly needs it, or by steering or adding, and never tells the owner to edit settings files; a project-wide grant or a tool rule asks the owner; `task new` gets `--title` with a few plain words, never starting "Owner"; a task's checks are stated as passed or failed from the ledger's `checks` (and an event's count), which outrank the worker's own words about what it could not run or verify; the project page is updated in place (see Project page); `task new` gets `--model sonnet` when the owner's message is already a thorough brief and keeps the default (opus) for a critical bug fix or heavy, complex work; memos are folded on rotation (see Memo); it loads context as a worker does (the project, the working copies, the web, read-only git) and never runs a git command that writes; the instructions name no skill (the owner's skills are whatever their folder holds): at the start of a turn it looks at the skills and subagents it has; before it answers about a prior decision, an unknown term, the code or a research question it uses any skill or subagent whose description fits lookup, context, memory or research, and checks rather than guesses; when none fits it uses Read, Grep, WebSearch and the Explore subagent, and docs lookups go to the MCP tools the project settings allow; it thinks about the owner's goal and the likely next step before it answers and says what it checked in one short line; subagents and skills keep its boundary; answers stay short; before it asks the owner to approve a land it reads the task's verify state (the ledger's `human_verify` count and `verification`, the items in `show --json`), and when Human Verify items have no pass and the owner said to land first, it asks for `land <n> --skip-verify` in one step, otherwise it tells the owner the items to verify. A running manager keeps the instructions its conversation started with until it rotates. Its environment is the worker allowlist plus `WSX_TASK_BROKER`, `WSX_TASK_ACTOR=manager` and `GIT_OPTIONAL_LOCKS=0`; the host puts its own `wsx` first on `PATH` and runs the commands with its own environment (see Permissions).

- `manager/log.jsonl` (rolls to `log.1.jsonl` past 8 MiB) has `message{id, from: owner|events, text}`, `delivered{ids}`, `reply{id, to, text, usage}`, `agent` (its commands), `rotated{tokens}` and `stopped`. `manager/state.json` keeps the conversation, the digest of its instructions, the next id and what was already reported.
- One turn at a time. Waiting owner messages go together in one turn, each headed with its id, before waiting events.
- Events: the host checks the task records every second. A change of (state, revision) into `ready`, `briefed`, `blocked`, `failed`, `conflict`, `in_review` or `landed` becomes a line written by wsx, `#<n> <state> r<k>: <title, 80 chars>; checks <passed> of <total> passed; worker says: <summary, 160 chars>` (without the checks part when there are none); for `briefed` it ends `brief v<k>: <rewritten prompt>`. Lines are logged as one `events` message when 3 s have passed since the last one and no turn runs, so events during a turn or close together arrive merged.
- Restart: `state.json` records the reported state of each task only when its message is logged, and `delivered` marks what reached the agent. A restarted host queues logged messages that were never delivered and reports changes it has not reported, so no event is lost. Its first start takes the existing tasks as they are, without events.
- After a reply whose context (input, cache read and cache write tokens) passes `rotateTokens`, the next turn starts a fresh conversation and the log has `rotated`; the ledger carries the state.
- Claude keeps the system prompt a conversation began with, also on `--resume`; settings, tools and model apply at every start. `state.json` keeps a digest of the instructions the conversation began with (`instructions`). A host whose instructions differ, or a state without the digest (from an older host), starts a fresh conversation and logs `rotated{tokens: 0, reason}`.
- The first turn of a fresh conversation after a rotation opens with a handover: the last 4 owner messages and replies, each at most 600 characters.
- A worker conversation is never rotated: it holds the task's work, and its rules that matter at run time (sandbox, tools) come with each start.

The web client shows the manager chat, the tasks, their reports and their fixed actions, and notifies about them: [web-client.md](web-client.md) "Tasks".

## Memo

Durable project knowledge moves from workers to the manager, kept small:

1. The worker's instructions: before a build finishes, put in the result's `memo` a short, lean memo of durable project knowledge a later task needs (a convention, a gotcha, a decision and its reason), a few lines at most; empty when there is none. It is kept in `result.json`.
2. The ledger shows the memo on each `recent` task (see Ledger).
3. After a rotation the fresh conversation's first message always opens with `Handover`, which asks the manager to fold the memos first. The manager's instructions: on a `Handover`, fold the ledger's memos into `manager/notes/context.md` with Bash (lean, duplicates merged, stale lines dropped, at most 60 lines); read that file when a conversation starts.

Memos of tasks that left `recent` (more than 10 closed since the last rotation) are not folded.

## Approvals

`tasks.land.approve`, `tasks.deploy.approve` and `tasks.release.approve` (default `true`) decide whether the manager may land, deploy or release alone. A `wsx task` command run with `WSX_TASK_ACTOR=manager` (the manager host sets it for its agent) that needs approval does not act: it appends `request{id: a<n>, action, task, revision, skip_verify, commit}` to `tasks/<project-id>/approvals.jsonl` and answers that the owner was asked. `close --discard` and `start` by the manager always ask: the build of a brief is the owner's decision. So do its project-wide grants and tool rules (`request{action: grant, grant{access, project_wide}}`, not tied to a revision); see Permissions. The ledger lists pending requests under `approvals`.

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
  agent: { adapter: claude, model: opus, tools: [WebFetch] }   # never Bash rules
  brief: true                                                    # false: build at once
  sandbox: { allowedDomains: [crates.io, "*.npmjs.org"], allowWrite: ["~/.cargo/registry"] }
  checks: ["cargo fmt --check"]
  checkTimeoutSeconds: 1200
  land: { policy: local-merge, remote: origin, approve: true }   # or policy: pr
  deploy: { command: "sh deploy.sh", approve: true }
  release: { command: "sh release.sh", approve: true }
  manager: { adapter: claude, model: opus, rotateTokens: 150000, tools: [] }   # never Bash rules
```

Defaults: adapter `claude`, worker and manager model `opus` (`task new --model` overrides the worker's), no extra tools, `brief: true`, an empty sandbox list, no checks. `tasks.agent.tools` and `tasks.manager.tools` refuse `Bash(...)` (the sandbox replaces it) and bare `Edit`, `Write`, `NotebookEdit`, `MultiEdit` (they would skip the classifier); `briefTools` was removed and is refused with the reason. An `Edit(<path>)` or `Write(<path>)` rule in `tasks.manager.tools` gives the manager no file tool: they are denied by name. `allowedDomains` are host names with an optional `*.`; `allowWrite` paths are absolute or `~/`, below the root or home, without `.` or `..`. Up to 64 tools, 64 checks and 64 entries per sandbox list; `checkTimeoutSeconds` is 1..86400; a tool or model is at most 256 bytes and a check 16 KiB. `task new` refuses a config that does not parse.

## Verify

```sh
cargo build -p wsx -p wsx-daemon && python3 scripts/test-task.py
```

The script runs an isolated wsxd with `scripts/fake-claude-stream.py` as `claude`.
