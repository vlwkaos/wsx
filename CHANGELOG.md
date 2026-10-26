# Changelog

## [Unreleased]

### Fixed

- A task no longer fails with "the agent ended with success and no result" after its worker returned a good result. Claude Code ends a turn of its own when a background job of the worker ends or is killed, also when it resumes the session, and that turn has no result; the host took it as the worker's answer, so #63, #66 and #69 went to failed and the manager had to nudge them. The host now ignores a turn without a model call. A worker turn that really ends without a result is asked once to return it as the turn's last call before the task fails ([docs/tasks.md](docs/tasks.md) "Claude adapter").
- A worker's `wsx task check-run` gets its answer also for a long scenario. Before, the task host re-execed onto a replaced wsx binary while the run waited, which closed the worker's socket unanswered ("the message is empty or larger than 3145728 bytes"), and the scenario ran on with nobody to tell. The host now waits for the run. Each check's whole output goes to `check-<n>.log` or `check-run-<n>.log` in the task folder; the answer keeps its last 8 KiB and names that log when it cut the start, so a check that prints megabytes still answers. Worker instructions now say to start a run in the background and wait for it ([docs/tasks.md](docs/tasks.md) "Checks, revisions and report").
- A markdown table in the web chat and the manager chat takes its full height in the message. Before, a table taller than a few rows scrolled inside its own block under a fixed header row. A table wider than its block still scrolls sideways ([docs/web-client-chat.md](docs/web-client-chat.md) "Markdown").
- The web To check sheet opens in one motion and stays where it lands. It slid up before its list and captures were read, then its top jumped when they arrived, since the sheet is as high as its rows. It now opens once they are in, or after a second ([docs/web-client-tasks.md](docs/web-client-tasks.md) "To check").
- The live site gets each land again within minutes. The deploy (`scripts/deploy-web-live.sh`) ran the task and web end-to-end scripts as its gate, and a stale assert in them failed 19 deploys in a row, so the site stayed 38 commits behind. The gate is now the build and `scripts/runtime-smoke.py`; workers run the end-to-end script of the area they change (`wsx task check-run task`, `web`, `smoke`).
- Task workers and the manager start again. The Seatbelt profile wsx wrote for its boundary had filters without parentheses, which sandbox-exec refuses (exit 65), so every agent ended at once and every deploy since failed at `fake launch timeout`. A host that cannot apply the boundary, also one that runs inside another sandbox, now fails the task at once with sandbox-exec's words in the log, the result and the ledger ([docs/tasks.md](docs/tasks.md) "Permissions").
- The deploy's end-to-end scripts (`scripts/deploy-web-live.sh`) hold the machine's check slot, so they no longer run beside other tasks' checks.
- A land no longer refuses because the main checkout has uncommitted files the land does not touch: they stay as they are and the land goes on. It refuses only for changed files the land changes too, names only those, and says where to commit or stash them; the message starts with words, so a copy no longer drops the path's leading `/`. The ledger (`land_blocked_by`) and the web task sheet, under *Land*, say before the press which files would stop a ready task's land ([docs/tasks.md](docs/tasks.md) "Landing").
- A task host restarted while its task was checking runs the checks again. Before, the task waited for the owner with "the host restarted during a turn".
- A task check that leaves a process behind, such as a test daemon or a push fixture from a killed test run, no longer holds the machine's check slot. Before, the host waited for that process to close the check's output, so one check never ended and every other task stayed in `checking` for hours. Now the check ends 2 seconds after its shell with the output read so far. The same holds for wsx's git and plugin commands.
- The manager chat's `/` skill list opens at the start of any word, as `#` and `@` do, not only in an empty text box. A pick after other words puts `owner:<name> `, so the sentence reads on; a `/` inside a word, as in `src/foo`, opens nothing ([docs/web-client-tasks.md](docs/web-client-tasks.md) "Project screen").
- The manager chat's `/` and `@` lists show why they are empty when their read fails, with a Retry, instead of "Loading…" with no end. Before, a failed read of the owner's skills (the server restarting, a refused or timed-out request) left the list at "Loading…" until the owner cleared the field and typed `/` again.
- Tasks that wait for free disk (`tasks.minFreeGb`) resume one at a time, the oldest wait first, and the next one only after the first one's turn or checks end and there is still space. Before, they all resumed at once when space freed and filled the disk again. A task behind another shows "queued for disk behind <project> t<id>" ([docs/tasks.md](docs/tasks.md) "Checks, revisions and report").
- A request the manager made for the owner's approval no longer goes missing. An old request type in `approvals.jsonl` made wsx count one request too few, so a new request (a9, a discard of #42) got the id of an earlier, decided one and left the pending list with no outcome; the owner was asked again (a10). Ids now follow the highest one in the file. A pending request whose task closed, landed or moved to a newer revision is dropped with the reason, and the manager and the chat say so ("Dropped discard #42: #42 is closed") ([docs/tasks.md](docs/tasks.md) "Approvals").
- A file or a screenshot pasted into the web chat's or the manager chat's text box is attached, as with the attach button. Before, a pasted screenshot on iPhone or desktop was dropped without a word. A pasted image without a useful name gets one with the time, such as `pasted-20261024153012.png`; a text paste is unchanged.
- Opening the terminal of an exited pane no longer fails now and then on macOS with "Invalid argument". The pane sends its last screen and closes the stream, and macOS refuses a socket option on a stream its peer has already closed.
- The web chat and the manager chat no longer jump. A diagram or an image that draws after its message shows keeps a reader at the end, and a reader who scrolled up stays on the same message when rows arrive below, the "Since you left" line opens above, or the keyboard opens. Only *Jump to latest* and the reader's own send go to the end.
- A wsx older than its project's `wsx.config.yml` keeps working. An unknown key is now a warning that names its path and the file, not an error that refused the whole file: when a land added `tasks.workflow.idle` or `tasks.env`, the installed wsx refused the config, so the auto-deploy after that land failed and every later land, ledger and `task new` failed until a deploy by hand. A known key with a wrong type or value still fails ([docs/tasks.md](docs/tasks.md) "Config").
- `wsx routine` commands no longer fail now and then on macOS with "Invalid argument" or "Socket is not connected". The scheduler answers as soon as it reads the request and may close first; macOS then refuses the client's next socket call, though the answer is already there to read.
- A client that connected to wsxd just as it started no longer fails with "wsxd socket is not an owner-only same-user socket". wsxd set the socket's mode after it bound it; the socket now appears only once it is owner-only.
- A web terminal whose lease ran out (its heartbeat came too late on a busy host) reconnects by itself. It said "Control moved to another wsx window" and waited for *Take control*, though no other window had taken it.
- The manager chat shows "New conversation" once. The line has no id, so each read of the chat while the manager worked returned it again, and the page added another copy each time.
- On iOS the terminal's key row no longer blurs the terminal, so the keyboard does not flicker on each `^J`, Enter or arrow.
- In the web chat, the first message of a new conversation no longer shows above its "New conversation" line.
- A task check with more than 8 KiB of output (a test run) runs to its end; the task host keeps the last 8 KiB.
- A web chat message to an idle Claude with a draft is delivered again. Claude Code 2.1.3xx draws up to seven footer rows under its editor, and wsxd looked for the editor only in the last eight rows, so it answered "Claude input editor is not visibly idle" or "fresh empty input was not observed".
- The web chat's "The terminal shows:" rows are the rows above Claude's editor, such as what it is working on, instead of the editor and its footer.
- A Claude question (AskUserQuestion or a permission dialog) in a narrow pane, such as the web terminal at phone size, is seen as blocked, so the web chat shows the question. Claude wraps its hint line there ("… Esc" / "to cancel"), and wsxd matched it only within one row, so the agent stayed working.

### Changed

- The web To check items read in a clear order: the task chip and what the task did as the title, then *Check*, *Look at* and *Passes if* as lines with muted labels. *Try*, *Fail* and *Pass* sit in one place on every item, a row under the words on a phone and a column at the right on a wide screen, instead of moving with the length of the pass line. *Fail*'s note is sent with *Send*; the Cancel button is gone, and × again or Escape closes the note ([docs/web-client-tasks.md](docs/web-client-tasks.md) "To check").
- The manager chat reads as a chat. A manager reply to wsx's task events is a muted line, not a bubble, and `-` shows nothing; a worker's report stays in the task sheet. The manager answers in a few short lines and hands the owner no chores (commands, deletes, settings to edit, tasks to verify). Its instructions are about half as long. The quiet-period recap is off by default (`tasks.manager.recapMinutes`) and at most 8 lines when on ([docs/tasks.md](docs/tasks.md) "Manager", "Recap").
- The idle task that consolidates project knowledge runs as one turn, without the owner's workflow steps.
- The web client's home-screen icon shows split panes, the active one in the theme's green with a prompt, instead of a generic `>_` on blue. The manifest marks the icons `any maskable`, so Android crops them to its own shape without cutting the mark.
- The web chats read better on a phone. The session chat and the manager chat open at the first reply this device has not read, under a "New" line, with "↓ 3 new" to go to the end; with nothing unread they open where you left them. Scrolled up, a new reply does not move the chat and the button counts it.
- Reply to one message in either chat: swipe it right, or long-press it (right-click on a computer) and choose Reply. The quote shows over the text box with ×, the message goes out starting with `> <its first line>`, and a tap on that quote in the chat goes to the quoted message.
- On a phone, a table's columns narrow and their words wrap. A table of more than three columns keeps the first two and shows the others as `Header: value` lines in the second cell; only a word that cannot wrap, such as a long path, still scrolls it sideways.
- On a phone, the task sheet's buttons with an icon (Approve, Deny, Land, Try) show the icon alone, so the sentence beside them keeps the row; the waiting messages' icons sit closer together.
- Task lists (the reservoir, the `#` picker and the task sheet's top row) put the title first and the number after it, small and muted. The `#` picker folds away tasks done or closed more than a day ago until you type a number or a word.
- A task's checks run fewer times. A worker runs only the checks its change touches and never the full set, which wsx runs once after its result; a worker's `wsx task check-run` of a check that already ran on the same clean tree answers from the stored result. A land whose rebase keeps the tree whose checks passed lands without running them again; a land after the default branch moved runs them as before ([docs/tasks.md](docs/tasks.md) "Checks, revisions and report", "Landing").
- A task's checks are the main checkout's `tasks.checks` at the time they run, not the ones the task started with. A task started before the checks changed lands through the current gate.
- The web's task buttons are only *Land*, *Answer*, *Work by hand* and *Try*. *Try live*, *Hold for me* and *Release hold* are gone; the manager still holds a task or starts a trial when it cannot judge a change itself. A working row says when the task waits to run checks or runs them.
- Manager mode wastes fewer turns and tokens:
  - When the owner says "close #42" in the manager chat, the manager runs it with `--owner-said <message>` and wsx does not ask the owner a second time; it checks that the message is the owner's and in the running turn, and records the approval by that message. Project-wide grants and tool rules still need the owner's button. In the web, a request that still waits shows Deny and Approve inside the manager's reply.
  - Under auto land, a task the owner created no longer lands by itself on passing checks: the manager reviews it like its own tasks, then lands it or says what to change. A landing that conflicted still lands by itself after the fix.
  - `wsx task new --plan` makes the first turn write a short plan (goal, files, approach, proof) and stop without editing or checks; the manager reads it and answers with go or what to change. The manager uses it for larger or riskier work.
  - Workers run on Opus: the manager no longer picks `--model sonnet`.
  - The idle task starts only when session notes in `knowledge/sessions/<project>/` are newer than the last one, and says nothing otherwise.
  - No recap is asked for when nothing changed since the last one (no land, close, new wait, approval or To check item). Each recap request names the token cost since the last recap, for the manager and the workers, and the manager names savings ([docs/tasks.md](docs/tasks.md) "Workflow", "Recap", "Landing").
- Task workers and the manager run inside one boundary that wsx owns: a macOS Seatbelt profile over the whole agent process tree, with Claude in its no-prompt mode inside it. Tasks no longer stop to wait on a permission for Chrome, a folder such as `~/.config/ir` or `~/Library`, a test server on 127.0.0.1, a PTY or a deploy tool, and Claude's classifier no longer denies the manager's own `wsx task` commands. The boundary still refuses `~/.ssh`, `~/.aws`, `~/.gnupg`, writes to keychains, wsx's config and state, other tasks' copies and the default branch's ref (lands stay with wsx). When a land refuses on the owner's uncommitted changes in the main checkout, the manager names the files and, on the owner's yes, commits them and lands again. `wsx task allow`, project grants, the `permission` wait, the web's *Allow* buttons, `tasks.sandbox`, `tasks.agent.tools` and `tasks.manager.tools` are gone. An agent start on Linux fails with a clear error until bubblewrap support ([docs/tasks.md](docs/tasks.md) "Permissions").
- The web's To check list leaves more room for the items on a phone. The item's words use the full width from its first line, which the task chip starts, and Try, Fail and Pass end its "Passes if" line (under it when that line is long), so no words wrap in a narrow column beside the buttons; *All passed* moved into the top bar, and the intro sentence is gone.
- The open reservoir in the manager chat no longer spends a row on its chevron alone. The chevron sits at the right end of the list's first row, so the list starts at the top and its first label ("Running") is no longer cut off.
- `tasks.env` sets environment variables for a task's worker and its checks alike. wsx's own config sets `CARGO_INCREMENTAL=0`: incremental rebuilds on macOS left every rebuild's object files in the task copy (one copy grew to 14 GB), and a copy now stays about 2.5 GB with its dependencies still warm. wsx's `minFreeGb` drops to 3, and workers may write Zig's cache, which a cold build of `wsx-terminal` needs ([docs/tasks.md](docs/tasks.md) "Config").
- Web notifications reach you only when you must act. Settings > Notifications has three switches per device: "Manager needs me" (a task waits on you, an approval is asked, To check items are added), "An agent needs me" (a session's agent needs input or stopped with an error) and "Daily digest" at a time you pick (tasks landed, failed or deployed, agents that finished). Each of the first two is one notice that replaces the one before and counts every open item ("2 agents need input: a, b"); a tap opens the item when there is one, else the list. "Agent finished" no longer pushes. A session's `⋯` menu mutes it on that device. Earlier topic choices carry over. The site file's `[tasks]` times are gone: `[notify.manager]` and `[notify.agents]` each have `merge_seconds` (docs/web-client.md "Notifications").
- Web push notifications say what happened. An agent notice names the project, worktree and session, and shows the question it asks, the first line of its reply or the call that failed. A task notice names each task by number and title: what it asks, which check failed, ready for review or landed; a digest lists each one. The installed web app shows a badge with what needs you (blocked sessions and task needs), where the browser supports it ([docs/web-client.md](docs/web-client.md) "PWA and push").
- Manager mode tells pipeline work from work the owner wants to be sure of. A held task (`wsx task new --hold`, `wsx task hold`, *Hold for me* in the web) never lands by itself. `wsx task trial` (*Try live*) rebases its revision, checks it and puts it on the live site before it lands; other deploys wait and hosts keep their binary until the trial ends. *Pass and land* lands it without To check items, and End, a message to the worker or a failed deploy put the default branch back ([docs/tasks.md](docs/tasks.md) "Hold and trial").
- Claude started in a wsx pane uses its full-screen renderer (`CLAUDE_CODE_NO_FLICKER=1`, unless you set the variable), so a phone or window resize no longer leaves copies of a tall reply in the scrollback (anthropics/claude-code#103181). The conversation scrolls inside Claude; `Ctrl+O` then `[` puts it in the scrollback.
- The web chat's **Send now** puts a waiting message into a working Claude's turn: Claude takes it at its next step (wsxd `AgentSteer`, daemon revision 33). A message typed into Claude while it works shows in the chat as the person's message.
- The manager writes the owner a recap after an hour with no message and no turn, once per quiet period and only when something happened (`tasks.manager.recapMinutes`, default 60, 0 off). The web tags it "Recap".
- Web: the terminal rows of a held chat are closed under "What the terminal shows" and run edge to edge when opened; waiting messages, decision rows, session Remove and notice Dismiss use icons, so their text keeps the row.
- Manager mode, lean core. A task is `working`, `waiting`, `checking`, `ready`, `failed`, `landed` or `closed`; `waiting` names what it waits for (`owner`, `permission`, `disk` or `hand`) with a reason, and replaces `starting`, `blocked`, `stopped`, `conflict` and `by_hand` ([docs/tasks.md](docs/tasks.md) "Store").
- The manager talks a request through in the chat and sizes it before it starts any task, writes each task prompt complete on its own, runs tasks at once only when they change different files, reviews what it created for lean code and never sends a task back to rebase, and starts one housekeeping task after a request's tasks land. A long plan goes on the project page with a diagram.
- `tasks.skills: owner` lends workers the owner's skills, and `tasks.workflow` (`build`, `review`, `batch`) holds the project's words for each stage; the defaults name no skill ([docs/tasks.md](docs/tasks.md) "Workflow").
- To check items say what the task did, where to look (a link when it is a URL or page path) and show the worker's capture, kept with the task at the land; Try, Fail and Pass are icon buttons and the list's sheet is only as high as its rows. An item that names a command goes back to the worker.

- With `task_view` the web's tabs are Home, Sessions and Settings: Sessions lists shells with their project, and a project's worktrees are in its chat's menu ([docs/web-client.md](docs/web-client.md) "Screens").
- Chat messages no longer fold: the session chat, the manager's bubbles, a done task's report and the compaction summary always show whole. A left-to-right flowchart too wide to read is drawn top to bottom when that fits.
- The manager and the workers present replies by one rule: the answer first, assumptions on the first line, structure as a diagram or a table, each fact once, about one phone screen ([docs/agent-reporting.md](docs/agent-reporting.md) "Output style").

### Removed

- The worker brief phase (`task start`, `--no-brief`, `tasks.brief`, `brief.json`, the brief on the task sheet), the `pr` landing policy (`tasks.land.policy`, `tasks.land.remote`), `task revoke`, and the log events only those used. Old log lines of removed kinds are skipped.

### Added

- The TUI shows a project's tasks: `t` on a project, worktree or session opens them read-only, from the same ledger as the manager and the web client. Open tasks come first, those waiting on you, failed, ready, checking, then working, with the title, the number, the state as a word and what each waits for; then the recent landed and closed ones. The heading counts To check items and waiting approvals, `Enter` shows a task's checks, waiting reason and summary, and the list follows the tasks as they change. A narrow window keeps the title and the state ([docs/tasks.md](docs/tasks.md) "TUI").
- In the web chat to a Claude session, a draft that is a `!` shell line changes the composer's border colour and its button reads *Run*, so it is clear the line runs in Claude's shell mode.
- `tasks.workflow.steps`: your own workflow as named steps, such as context, plan, build, review and memo. The task host sends each step as its own turn of the worker's conversation, and the web shows the current one under *Running* (`plan (2/5)`). Steps in `~/.config/wsx/wsx.config.yml` apply to every project that has none of its own.
- A session row in the web slides left to show Remove, which closes the session (`DELETE api/sessions/<id>`, with `session_start`); a task's session is refused.
- `wsx agent install claude` writes the Claude output style `wsx` and selects it when no style is chosen.
- Each manager turn's message ends with a ledger summary from the host (open tasks by state, what changed since the last turn, To check, approvals, deploy); the manager reads the full ledger only when it needs more.
- `tasks.manager.eventModel` (default `off`): a turn of only events, with no review due, runs on that model; the host switches the running process with `set_model`. It is off because the switch writes the conversation into the other model's cache and cost more on `scripts/bench-manager.py`, a real-Claude benchmark of five fixed manager turns.
- Manager mode, seventh part: one short path from a request to a running change ([docs/tasks.md](docs/tasks.md)).
  - Approvals only where something goes public or is lost: `tasks.land.approve` and `tasks.deploy.approve` default to `false`, `start` never asks, and a release, `close --discard` and a grant for every task still ask. A web Approve shows "Running…" until its decision settles the row, and a second Approve answers what happened instead of an error.
  - A brief without open questions starts its build by itself (`started{auto: true}`); one with questions waits until the owner answers.
  - A ready task the manager created waits for the manager's review of its diff, then the manager lands it or sends back what is missing. The web shows it as working, not as a need of the owner.
  - After a land the host closes the task (its copy, build and branch go) and, with the new `tasks.deploy.auto`, deploys the default branch. `task deploy --if-new` and a per-project deploy lock make quick lands deploy once, for the newest commit. wsx's own config deploys after each land.
  - `wsx task check-run [<n>...]`: the worker runs the declared checks through its task host's broker, outside its sandbox and as after its result, before it reports done.
  - `tasks.minFreeGb` (default 5): below it a task waits with "Disk full: n GB free" and goes on by itself, also after a check or a worker hit "No space left on device"; no check retry is spent.
  - The manager keeps the owner's decisions in `notes/decisions.md`, and every fresh conversation starts with that file and `notes/context.md` (8 KiB). Workers must leave a memo when they learned a convention, a gotcha or a decision.
  - Managers talk: a manager may read another registered project's ledger and message its manager with `task say --from-manager`, answered once with `--reply`; nothing else crosses projects ("Managers talk").
  - `wsx task preview <n> [--stop]` and `tasks.preview`: a task served on its own tailnet port (`tailscale serve --https=<port>`) for an hour, with *Try* in the web; for projects that configure it (wsx does not).
  - The manager chat in the web has attachments, and shows other managers' messages as "Manager <project>".

- The web chat no longer loses a queued message that the agent drops: a delivered queued message stays above the composer until the agent's conversation shows it, with Copy, Send again and Remove, and every waiting or sent message has Copy. Before, the server removed a queued message as soon as wsxd typed it, so a paste the agent dropped was lost ([docs/web-client-chat.md](docs/web-client-chat.md) "Queue and Stop").
- A one-line `!` message to a Claude session in the web chat runs in Claude's shell mode (`! git status`): it is typed without the exchange line and shows in the chat as written. Before, Claude got it as text under the exchange line. Needs wsxd revision 30 (`exchange_shell`); Pi and Codex sessions keep the exchange line ([docs/web-client-chat.md](docs/web-client-chat.md) "Commands").
- One permission model for every task agent (manager mode, sixth part). Workers, brief turns and the manager run in Claude's auto mode inside Claude's Bash sandbox, set through `--settings`, so a repository's own settings cannot widen it. Any Bash command, compound ones and builds too, runs without a prompt inside the agent's write boundary: a worker writes its working copy and the git paths its commits need, a brief turn writes nothing, and the manager writes only its notes folder while its `wsx task` commands run outside the sandbox. Hosts in the new `tasks.sandbox.allowedDomains` are reachable; for other hosts the auto-mode classifier decides. File tools are kept out of the main checkout, the wsx state and config folders and the other task copies by deny rules, and the classifier reviews them elsewhere. Before, every command that was not on a short allow list was denied, and an allowed compound command never matched its own rule again ([docs/tasks.md](docs/tasks.md) "Permissions").
- Running task and manager hosts follow a deploy of wsx (manager mode, sixth part): before their next agent turn they exec the replaced binary in the same pane and resume the conversation with the new launch, so a fix to the launch reaches tasks that already run. Install by replacing the file; an executable overwritten in place is killed by macOS ([docs/tasks.md](docs/tasks.md) "Host").
- A worker that says done while a check fails gets the failure back (manager mode, sixth part): the host starts a worker turn with each failed command, how it ended and the end of its output, and runs the checks again on the new result, at most twice before the task fails; a message from the owner or the manager allows two more. The log has `check_retry{revision, attempt, commands}`. Before, the task failed at once.
- `wsx task allow <n> --host <host> | --path <path> | "<rule>" [--project-wide]` and `wsx task revoke` (manager mode, sixth part). A blocked task names what the sandbox stopped as typed asks, `{kind: network, host}` or `{kind: write, path}`, in its log, `result.json` and the ledger. A grant resumes it in the same conversation; `--project-wide` keeps the grant in the task store for every task of the project. The manager may grant a host or a path for one task on its own; a project-wide grant or a tool rule asks the owner.
- `wsx task start` and a brief before every build (manager mode, fifth part). A new task first runs one read-only turn and returns a brief: what a top professional would add, the raw material to start from, the methods and tools, what independently shows the result is right, the concepts the owner may be new to, the rewritten prompt and the open questions (`brief.md`). The task waits as `briefed`; a reply is a talk-through the worker answers and may revise the brief from; `task start <n>` builds the rewritten prompt in the same conversation. The manager's `start` asks the owner. `task new --no-brief` and `tasks.brief: false` skip it (logged); `tasks.agent.briefTools` adds the project's own read-only commands. Workers default to the `opus` model; the manager stays on `sonnet`.
- `wsx task new|list|show|steer|add|allow|stop|close` (manager mode, first part). `task new "<prompt>" -p <project> [--from <ref>]` starts a headless Claude worker on branch `wsx/t<n>` in its own working copy, a copy-on-write clone of the main checkout. Nobody types into it: steer it during a turn, add work after it, allow a denied tool, stop it, read its result and report, and close it, each in one call with `--json`. After each result wsx runs the project's `tasks.checks` and records revision `refs/wsx/task/<n>/r<k>`; the task is `ready` only when the worker reports done and every check exits 0 (`tasks.checkTimeoutSeconds` bounds each check). Tool calls Claude denied while the worker carried on are listed as evidence; only the worker's own `blocked` status blocks a task. `close` refuses while the branch has commits that are not on the default branch, unless `--discard`. Settings come from a new `tasks:` section in `wsx.config.yml`. Each host's control socket has a short hashed name in `<TMPDIR>/wsx-<uid>/`, so a long state folder or project name still fits macOS's 104-byte socket path. No wsxd protocol change. See [Tasks](docs/tasks.md).
- `wsx task land|verify|deploy|release|deployed` (manager mode, second part). Each revision records the paths it changed, and `task list --json` names the open tasks that overlap. `task verify <n> pass|fail [--note]` binds a human verdict to the current revision; a fail goes to the worker in the owner's words. `task land <n>` brings a ready task onto the default branch by `tasks.land.policy`: `local-merge` rebases it (a conflict goes back to the worker), reruns the checks, carries a pass only when the default branch changed none of the task's paths, and fast-forwards; `pr` pushes the branch, opens a pull request with `gh` and records the merge. `task deploy` and `task release` run the declared command in a temporary copy of a default-branch commit and record it; `task deployed` says which commit runs and how far it is behind.
- Manager mode on the web (fourth part). Two site capabilities, `task_view` and `task_act`: with them the page opens on Home, one row per project with what needs the owner first (a count, the most urgent need in words or the manager's last reply, how many tasks work, and when it last changed), and the bottom bar is Home and Settings. A project opens on its manager chat: each need (blocked, conflict or failed, approval, ready to verify, ready to land) is a card in the conversation where it arose, with fixed actions that go straight to wsx; a settled card becomes one muted line ("#4 allowed Bash(make:*)", "Approved land #3"); a tray over the composer lists the needs in two lines and "+N", and a line scrolls to its card. The chat has live `#n` chips and a `#` task picker, and `⋯` holds the project's sessions and files. A task opens in a sheet over the chat (85% high, a right panel on a wide screen), closed by Back, Escape or a swipe down with the draft kept; `#/p/<id>/t/<n>` links to it. The sheet shows the report, Pass and Fail (a fail needs a note) at its Human Verify items, Allow, Approve, Deny and Land, shared files and the timeline. The session inbox moves to *All sessions* (`#/sessions`) and no longer lists task workers or the manager. A device may opt in to the `tasks` push topic: blocked, conflict and approval requests after a 3 s merge window, ready, failed and landed as one digest after 3 min without another (at most 15 min), never for a project a page of that device shows; the times are site settings (`[tasks]`). Opening a project after an absence shows what happened "since you left". A one-line project brief at the top of the chat unfolds to the open tasks in words, recent landings, the deploy and how far behind it is, and the main checkout; a task story at the top of the task sheet shows what was asked, `Asked → Working → Checks → Ready → Verified → Landed → Deployed` with the current step, and what happens next, all written by wsx without a model; *Explain* asks the manager for more in the chat. An approval request reads as the action it asks for, "Approve land #3", on Home, the cards, the tray, the task sheet and the push text. The server reads the store and talks to the hosts through `wsx_core::task`; approving an action still runs it as one `wsx task` command without `WSX_TASK_ACTOR`.
- `wsx task manager|say|chat|ledger|approve|deny` (manager mode, third part). `task say -p <project> "<text>"` reaches one manager agent per project, started on demand in a hidden session in the main checkout. It never edits files: it reads `task ledger` (the bounded project state) and acts through `wsx task` commands. Worker state changes reach it as merged events, owner messages go first, and nothing is lost when its host restarts. Its land, deploy, release and `close --discard` become approval requests the owner answers with `task approve` or `task deny` (`tasks.*.approve`, default on). This guards against the manager's mistakes; it is not a security boundary.

### Breaking Changes

- Rust callers that construct `Request::AgentReport` must provide `received` (`None` keeps the old meaning); `AgentCapabilities` has `prompt_receipts` and `AgentExchange` has `command`. Older JSON without them decodes as before. `POST api/chat/<session>/send` answers `{queued, exchange}` instead of the bare exchange.
- The manager lands and deploys without the owner's approval by default, and its `start` no longer asks. Set `tasks.land.approve: true` or `tasks.deploy.approve: true` to keep asking.
- A landed task closes itself: its working copy, build and branch are removed after the land.
- Rust callers: `wsx_core::task::manager::Author` is no longer `Copy` and has the variant `Manager { project, reply }` (it serializes as a string, `manager:<project>`); `ManagerRequest::Say` has `from`; `owner::say` takes `from`; `TaskRecord` has `created_by` and `waiting`; `LogEvent::Started` has `auto`; `TaskConfig` has `min_free_gb`, `deploy_auto` and `preview`. Older JSON without the new fields decodes as before.
- Rust callers that construct `DaemonLifecycle` must provide `last_handoff_error` (`None` when no handoff failed), and a `match` on `Availability` must handle `UpgradeFailed { reason }`. Older JSON without `last_handoff_error` decodes as `None`.
- `tasks.agent.tools` refuses `Bash(...)` rules and bare `Edit`, `Write`, `NotebookEdit` and `MultiEdit`, and `tasks.agent.briefTools` was removed: a project config that has them no longer parses, and the error says why. `wsx task allow` refuses the same rules.
- Rust callers that construct `AgentExchange` must provide `key` and `continue_key`, and `Request` has the new variants `AgentExchangeCreateKeyed` and `AgentExchangeContinueKeyed`. Older JSON without the new fields decodes as before.
- Rust callers that construct `Request::AgentExchangeCreate`, `AgentExchangeCreateStashingDraft`, `AgentExchangeContinue` or `AgentExchangeContinueStashingDraft` must provide `command` (`false` keeps the exchange line) and `takeover` (`false` keeps `terminal_busy`); `AgentExchangeCancel` must provide `takeover` too. `Capabilities` has the new `exchange_command` and `exchange_shell` flags. Older JSON without the new fields decodes as before.
- Rust callers that construct `AgentExchange` must provide `failure` (`None` for a delivered round). Older JSON without it decodes as `None`.
- Rust callers that construct `AgentInfo` must provide `state_since_unix_ms`. Older JSON without it decodes as `None`.
- `wsx_core::integration::memory::Timeline` has no `checkpoint` field: the newest compaction summary is an `ItemKind::Summary` item in `items`, and the web timeline API no longer sends `checkpoint`. Claude's fixed words around a summary are removed from its text, also in `wsx agent context`.

### Added

- Add `wsx agent start <branch> -p <project>`: one call that creates the worktree when it is missing, creates one session that runs the agent, waits until the agent can take a prompt, delivers the request and prints the worktree, session, pane and exchange together. The agent command is the project's `worktree.defaultSession.command`, or `--agent <command>` for one call. A worktree, a session of the same name and an exchange of the same `--key` are used when they exist. A late failure prints what exists and what to run next, and deletes nothing. A failure at the ready step adds a typed `error.reason` and `error.screen`, the end of the pane's screen. The agent's folder trust question stops the start at once with reason `folder_trust_question`; `--accept-folder-trust` answers yes to it, and the agent then saves that trust in its own settings. See `docs/agent-orchestration.md`.
- Add `wsx agent wait --result`: the agent's final message of the exchange's current round, read from the provider's saved history after the delivered request. When that message cannot be named, `result.status` says why; an older message is never returned.
- Add `wsx agent wait --any [<exchange>...] [-p] [-w]`: one blocking call for several workers. It returns the first exchange that is blocked or finished, with the `ready` and `pending` IDs.
- Add `--key <text>` to `agent request`, `agent continue` and `agent start`. The same key for the same pane returns the existing exchange instead of a second delivery. A wsxd before daemon revision 29 refuses the keyed requests; the protocol stays 16.
- Take a prompt from a file or standard input: `agent start`, `agent request`, `agent continue` and `session prompt` accept `--prompt-file <path>` and `-`. Exactly one source is allowed, an empty prompt is refused, and the error names the size limit.

### Fixed

- The web Files screen opens a `.md` or `.markdown` file rendered, as the chat renders an answer, with **Source** in the top bar for the text and the editor; an unsaved edit survives the switch ([docs/web-client.md](docs/web-client.md) "Files").
- The web chat names more of what holds a session. While a `/compact` a person typed runs, the working row reads "Compacting the conversation" (Claude's hook reports a manual `PreCompact`, integration version 24; an automatic compaction stays unreported). A blocked agent whose question the chat cannot read, and a message that has waited more than 10 s, show the last 8 rows of the terminal with Open terminal, on a site that may view the terminal. Before, the page showed only "Working" or "Waiting for your answer." ([docs/web-client-chat.md](docs/web-client-chat.md) "Timeline").
- The web chat outlives the agent. When Claude exits, its session keeps the Chat tab and the conversation, with the line "Claude exited."; a message then types `claude --resume <id>` into the pane's shell and goes to the same conversation once Claude reports ("Resumed Claude in the same conversation."). A conversation started again by hand reads "Same conversation, continued", and a new one "New conversation", with the end of the earlier one kept readable above it ([docs/web-client-chat.md](docs/web-client-chat.md) "Conversations").
- The web terminal no longer flickers while Claude redraws its input box: wsxd holds the raw stream's bytes of an open synchronized update (`CSI ? 2026`) until the update ends or its 1 s bound expires, because xterm.js 5.5 ignores the mode. A "Jump to latest" button shows while the terminal is scrolled back ([docs/terminal-synchronized-output.md](docs/terminal-synchronized-output.md)).
- A message from the web chat is done only when the agent took it. Claude's hook now reports which prompt each turn took (`UserPromptSubmit`, integration version 23), and wsxd (revision 31) accepts the round it names; a round that another prompt passed, or that the agent did not take within 5 s while idle, ends `not_taken`, and the turn of a prompt a person typed is never tied to it. The web server sends such a message again once by itself, with "Sent again: Claude did not take it the first time." in the conversation, and keeps it failed with Copy and Send again after a second miss. Every web message now goes through the session's queue, deliveries run one at a time, and a sent message shows at the end of the conversation until the agent has it. Before, a paste Claude dropped at the end of a turn counted as delivered ([docs/agent-orchestration.md](docs/agent-orchestration.md) "Taken, not typed").
- Manager turns cost what they cost: a turn's `cost_micro_usd` is its own part of Claude's running `total_cost_usd` (the log summed the running total, $41 for $11 of real spend), and `session_cost_micro_usd` keeps the total, also across a resumed process.
- The manager rotates on the context of its last model call, not on the sum over the turn's calls: nearly every turn had started a fresh conversation (19 of 21), at about 16 times the cost of a kept one.
- A task's working copy reuses the main checkout's build. Before, `git reset --hard` on the new worktree's empty index rewrote every tracked file with a new mtime, so cargo rebuilt every workspace crate, and each copy split its cloned `target/` into 4 to 6 GB of its own. Now only files that differ from the task's base commit are rewritten ([docs/tasks.md](docs/tasks.md) "Working copy").
- Hand off a pane whose program exited. Before, `wsx daemon restart` and every automatic upgrade failed while any pane had exited ("wsxd could not hand off and kept running"): the daemon paused that pane's reader, which was gone, and rolled back on "Broken pipe". From daemon revision 32 an exited pane goes to the new daemon as its last screen without a process and stays exited. A daemon before revision 32 still fails this way: restart or close its exited panes before the upgrade ([docs/daemon-live-upgrade.md](docs/daemon-live-upgrade.md) "Exited panes").
- Say why a handoff rolled back. The daemon keeps the failed step and its error as `last_handoff_error` in its lifecycle status; `wsx runtime status` prints it and `wsx daemon restart` names it. A new daemon that exits before it takes the handoff ends the wait at once instead of after 30 s.
- Keep working when an automatic upgrade fails. A newer `wsx` whose upgrade of the running daemon rolled back now works with that daemon when the protocol matches, with the notice "wsxd upgrade failed: <reason>; sessions keep running" in the TUI, the CLI and the web page, and tries that upgrade again only for another daemon or another build. Before, every command of the newer `wsx`, and so the web page, failed with "wsxd is unavailable" ([docs/daemon-live-upgrade.md](docs/daemon-live-upgrade.md) "A failed automatic upgrade").
- Let other changes go on during a slow exchange delivery, and end a delivery that the agent does not read. wsxd held its mutation lock while the PTY took the prompt, so every other change, among them the agents' own reports, waited and failed after the 5-second client limit. Now the lock is released during the write, a live handoff waits for the outcome, and the write ends after 10 seconds with `delivery_failed`. Closing a pane whose program does not read its input no longer blocks wsxd.
- End Claude's wake heartbeat within seconds of its prompt, pane or wsxd. Claude starts `wsx.reporter agent wake-heartbeat` from a detached hook shell and never ends it, and the heartbeat checked again only after 5 minutes, so every prompt left its heartbeat processes alive after the prompt finished, the session was deleted or wsxd stopped. It now renews every 5 seconds and ends at the first refused renewal or when no wsxd answers. `agent wake-heartbeat` and `agent presence-renew` no longer start wsxd.
- Return the exchange of a slow delivery. `wsx agent request` failed with `Resource temporarily unavailable (os error 35)` although the exchange existed, when the agent took more than 5 seconds to read the prompt from its terminal. Until then wsxd also accepted no other connection. The client now waits for the exchange timeout, wsxd writes the prompt without holding its state, and its accept loop no longer waits for a running change. Daemon revision 29.
- Leave no branch after a failed worktree creation. When `git worktree add` failed, the new branch stayed, and the retry of `wsx worktree create <branch> --from <ref>` was refused with "already exists; omit --from". The branch that the failed call made is removed while it still points at its start commit. A branch that existed before is never removed.
- Apply `-p` and `-w` to `wsx agent exchanges` without a session. The scope was ignored, and exchanges of other projects were listed.
- Claude: after a compaction wsx showed the agent idle while it still worked. Claude starts its session again after every compaction, also an automatic one inside a turn, and the hook reported that start as idle. The turn ended early, and the web chat delivered the next message into a busy editor. Claude integration 22 skips that start, and reports `/compact` as working, then done, by its own compaction events. A message sent during `/compact` now waits for it. Run `wsx agent install claude`, then restart Claude when it is idle.
- Web chat after a Claude compaction: the answer Claude kept through the compaction now shows after the summary, `/compact` shows as typed instead of in tags, and its "Compacted" output is left out.
- Web chat: Claude Code 2.1.300 writes a background-task notice (`<task-notification>…`) in the person's name, and the chat showed it as a message the person wrote. The timeline now leaves out a Claude record with `promptSource: "system"`.
- Web chat: a message that failed (for example while Claude's editor was not drawn idle yet, right after a turn) held back every later message, so new messages seemed ignored. `editor_unavailable` and other "not yet" refusals now keep the message waiting for up to 30 s. A failed message no longer holds back later ones; it stays above the composer with its reason, Retry and Remove.
- Web on a phone: from time to time half of the inbox stayed empty. The page kept the height it had while the keyboard was up when iOS did not report that the keyboard closed. It now follows the keyboard only while a field that takes typing has the focus.
- A chat message to a Claude that showed its dimmed next-prompt suggestion, or whose typed text it had just stashed, was not delivered (`delivery_failed`), and the page did not say so. wsxd (daemon revision 28) now takes any fully dimmed editor line as empty. A failed round keeps its reason in the exchange's new `failure`; the page shows it on the sent message with Retry, and a waiting message whose delivery fails stays in the queue with that reason instead of being dropped.
- Web chat: a file showed nothing while it uploaded, so Send could go without it. Each file now shows as "Uploading…" and Send waits. The draft, its files and a message that was not delivered are kept after a reload.
- Web: after a reload of a paired device, the "Loading…" placeholder stayed beside the app and took half of the screen. The page now removes it before the app starts.
- `wsx web serve` did not exit on SIGTERM or SIGINT while a page held an event stream or a terminal. Open streams now get two seconds, then the server exits.
- Web: the working time of every session counted from the start of `wsx web serve`, and a restart of the server made each finished session "new" again. wsxd (daemon revision 28) now records when an agent entered its state, and the web client shows that time.

### Changed

- Web, task surfaces: the strip over the manager's composer is the one decision list. Closed it reads "2 need you · #6 <title> +1 more", naming the newest running task with its pulsing chip. Open, each item that waits for the owner keeps its one action; after a success the row leaves and the focus moves to the next row's action, so the list clears one row after another, and the running tasks follow under *Running*. The deploy, release and main-checkout lines moved to the project page. The task sheet is shorter: one sentence on what happens next with its buttons, then what the owner decides on (a brief's open questions and additions, or Human Verify with Pass and Fail and the summary), the rest under a closed *Details*, then the talk. The progress steps, the timeline (and the task detail's `timeline` field), the *Review* button and Shared files outside a conflict are gone. Task text is read as prose: a paragraph indented by four spaces is no longer a code block, and brief sources are text ([docs/web-client-tasks.md](docs/web-client-tasks.md) "Project screen", "Task sheet").
- Web: every working state has one color. Task and session marks and words, the pulsing dots, the manager's "typing" and its dots, and "N working" on Home use the `working` tone and `--color-working`, defined once in `app.css` as the theme's accent; ready, verify and land keep the plain accent tone. The manager's typing dots were muted and "N working" on Home was uncolored before ([docs/web-themes.md](docs/web-themes.md) "Tokens").
- Task titles are shown without a leading `Owner <words>:` also when the words have brackets (`Owner report (UX failures):` reads `UX failures`), when the prefix stands alone on the first line (the next line is the title), and for a `--title`. `task.json` and the prompt keep what was written ([docs/tasks.md](docs/tasks.md) "Store").
- Give a manager one status call. `wsx status --json` adds `exchange` to each session (its active exchange, or else the last one, with state, evidence and round) and `work` to each worktree (the number of uncommitted paths and of commits that the default branch does not have). Existing keys are unchanged.
- Return every identity from one call. `wsx session create --json` adds `pane_id` and `agent`, and `wsx session list --json` rows add `project`, `branch` and `worktree`.
- Web: "New session" on the inbox, the projects list and each worktree heading is now a + icon with a 44 px hit area. Its name and tooltip say "New session in <project or branch>". The empty inbox keeps the words "Start one".
- `wsx web pair`, `wsx web devices` and `wsx web serve` no longer need `--site`. With no site named they use the only site file in `~/.config/wsx/web/` (or `$XDG_CONFIG_HOME/wsx/web/`); with none they say how to make one, and with several they list them. A site can also be named by its id or file name, for example `wsx web pair wsx`.
- Web: a project in Projects opens its worktrees and their sessions, and each session opens from there. Back returns to the screen a session or a project was opened from, and switching Chat and Terminal no longer adds a step to Back.
- Web inbox: a session row shows its project and worktree first, then the session's label and provider, each with its own look. Finished sessions are sorted newest first, so opening one and coming back no longer moves rows. The end of the chat keeps a clear gap (64 px) above the composer.
- Web chat: typing `/` lists the agent's commands, filtered as you type: built-ins (compact, clear or new, per provider) and, for Claude, the skills and commands of the project, the user and enabled plugins. A picked command is typed into the agent as written. wsxd (daemon revision 27) can deliver a one-line slash command of an exchange without the exchange line; the web server checks the new `exchange_command` capability first.
- Web: a shell session (no agent) opens on the terminal and has no chat tab. The inbox lists agent sessions only; shells stay in Projects under their worktree, marked `shell`. A shell in which an agent starts becomes a chat.
- The newest person wins a session. A web chat message, Stop or answer takes the pane from a wsx window or a web terminal that holds it, instead of failing with `terminal_busy`; opening the web terminal takes it too, and Ctrl+C on a selected session in a wsx window. The displaced window says that control moved and does not take it back by itself. A reconnect never takes over, and CLI exchanges (`wsx agent request`) still never take the pane. Needs wsxd daemon revision 28.
- Web chat while an agent writes: each change read the whole timeline again (about 215 KB on a long conversation). The page now reads from its last turn (`?from=&at=` on the timeline) and one read at a time; each read is about 16 KB.
- Web chat: an image larger than 2000 px is made smaller in the browser before it is uploaded (JPEG, or PNG when smaller).
- Web terminal on a phone: Enter is at the right edge of the second row, with `^J` (a new line) left of it, instead of at the end of the scrolling key row. Neither repeats when held.
- Web: Settings shows the wsx version, the commit it was built from, the page files' build id and when the server started. When the server runs another build, the page offers "A new version of this page is ready" with Reload and never reloads by itself. Modules load from `assets/<build>/`, so no cache serves an old module beside a new one.
- Web chat: a listed command at the start of the composer, such as `/compact`, shows as one token in the accent color; Backspace right after it removes the whole command.
- Web chat on a phone: the composer is one rounded card inset by the gutter, about 84 px when empty: a one-line field, then attach at the left and a round Send at the right inside the card. The field's text, the paperclip and the round Send keep one 8 px inset from the card's border on every side. Every control, folded step row and short text action such as Stop has a 44 px hit area. Section labels and command sources are 12 px instead of 11. While it has the focus no other form field is in the tab order, so iOS's previous and next arrows have nowhere to go (its Done bar stays). The `/` list shows each command's name, then its description on one muted line, and keeps to 3.5 rows when the keyboard is up.
- Web chat: a compacted conversation shows the start of its summary, open, where the compaction happened, with "Show full summary" for the rest. Claude's fixed preamble is not shown.
- Web chat: tables, code blocks and Mermaid diagrams may use the width of the conversation instead of the reading column, so a wide table or diagram is no longer squeezed; prose keeps its reading width.

## [0.31.0] - 2026-10-14

### Breaking Changes

- `wsx daemon stop` refuses while sessions are running. Scripts that stop a daemon with live sessions must pass `--force`.
- Rust callers that construct `TuiClientPresence` must provide `follows`, and callers that construct `Request::PrepareHandoff` must provide `restart`. `Capabilities` has the new `daemon_restart` flag, and `DaemonPhase` and `ReplacementBlocker` have an `Unknown` variant. Older JSON without the new fields decodes as before.
- The web client was not part of a published release. A site made with a development build must follow these changes:
  - The web client's `GET api/terminal/<session>` and `POST api/terminal/<session>/input` routes are removed with the text-snapshot terminal. `terminal_view` now allows watching the live terminal; `terminal_input` allows typing, resizing the pane and taking control.
  - A web site file names its projects under `[access]`: `projects = "all"` or a list of 1 to 32 absolute paths, with `capabilities` and `agent_command`. The old `[project]` table is refused. The files API and `POST api/sessions` now take a `project` id.
  - The built-in `retro` web theme is removed. A site file with `theme = "retro"`, or a custom theme that extends it, is refused; use `default`.
  - The web client's Activity panel is removed; the inbox replaces it. A site file that lists `"activity"` under `panels` is refused.
  - The web theme contract changed with the new page: 60 tokens (`color-raised`, `color-hairline`, `color-faint`, `color-attention`, `color-ansi-0` to `color-ansi-15`, `font-terminal`, `tracking-*`, `size-row`, `size-bar`, `size-rail`, `size-prose` and others are new; the component color, shadow, `color-border`, `color-sunken`, `color-ok` and `color-focus` tokens are gone) and new component hooks. A custom theme must be updated; the server names the first token it refuses. See `docs/web-themes.md`.
  - `GET api/chat/<session>/history` is replaced by `GET api/chat/<session>/timeline`.

### Features

- Add the hosted web client. `wsx web serve --site <site.toml>` hosts one web site for the projects its site file grants, on a loopback port, for use behind `tailscale serve`. A paired phone or browser can chat with the project's agents, attach files to a prompt, download files the agent produced and see images inline. Optional panels show project files and a live terminal.
  - One site file per process sets the public origin, optional base path, name, icon, theme, panels, capability bits, media limits and push keys. The file must be owner-only, and unknown fields are refused. A second site, for example a simpler chat for a family member, is the same binary with fewer panels and capability bits.
  - `wsx web pair` prints a one-time code that pairs one device. The device keeps an `HttpOnly` credential; the server stores only its hash. `wsx web devices` lists and revokes devices. There is no shared token. The server checks the exact `Host` and `Origin`, sends a strict CSP and limits requests per device.
  - Chat sends through the agent exchange broker and shows the agent's own saved conversation, including Claude's. Terminal output is never read into chat. The page states delivery evidence as it is: a PTY delivery is shown as typed into the terminal, not as accepted by the agent.
  - A page receives updates over one server-sent event stream and has no polling timer.
  - Media is stored per site under its SHA-256, with a maximum file size, a total quota with oldest-first eviction, and a time limit. File types are detected from content against an allowlist.
  - The page is an installable PWA with an offline shell. Web Push is off until a device opts in, per topic: agent needs input, agent finished, agent error. Tapping a notification opens that session.
  - Styling is a token and component contract. A theme supplies validated tokens and, optionally, component CSS, fonts and icons; the server serves them under content-hash URLs and the CSP stays strict. `default` (light and dark) is built in, and a deployer can supply a theme directory. The PWA manifest colors and icons follow the theme. See `docs/web-themes.md`.
  - The default deployment is one Tailscale Service hostname per site, served at the root. Every browser URL is relative, so the same build also works at an alt-port root and under a sub-path such as `/app/wsx/`.
  - The wire protocol stays 16. The web server never starts `wsxd`. See `docs/web-client.md` and `docs/web-deploy.md`. Verified with `scripts/test-web-client.py`; a real push service and a phone are not covered by that script. `scripts/web-real-agent-check.py` runs a real Claude turn and is not part of CI.
- Make the web client an inbox of agent sessions. The home screen lists sessions by what they need: needs you, running (with the current activity and elapsed time), finished (new ones first), then the rest by project. A session has one top row with a Chat or Terminal switch. A phone has a bottom tab bar (Inbox, Projects, Settings); a wide window has a left column. The chat shows the user's messages as blocks and the agent's text plain, folds tool steps into one line, shows a working row with Stop, and keeps the reader's place with a "Jump to latest" button. The page is Preact and htm, vendored, with no build step. On a phone the page does not zoom on focus or on a double tap.
- Ship terminal fonts with the web client: JetBrains Mono 2.304 and Symbols Nerd Font Mono 3.4.0. Powerline separators and Nerd Font icons show without an installed font, and operators such as `=>` and `!=` are drawn as ligatures while cells, selection and copied text stay the same. `[terminal] ligatures = false` in the site file turns ligatures off. The terminal's 16 ANSI colors come from the theme.
- Render the agent's messages in the web chat as markdown: headings, lists, task lists, quotes, tables with a fixed header row, links, and code blocks with a Copy action and colors for 13 languages. A message cannot load a remote resource or add live markup; raw HTML stays text. See `docs/web-client.md` "Markdown".
- Draw Mermaid diagrams in the web chat. A `mermaid` code block becomes a diagram in the theme's colors; it is scaled down to the message column, and scrolls sideways only when its text would become too small to read; a tap opens it full screen with zoom. The library (1 MB compressed) is downloaded only when the first diagram appears. The page's content security policy is unchanged. A block that does not parse shows its source. See `docs/web-client.md` "Diagrams".
- Answer a blocked agent from the web chat. With the new `chat_answer` capability the chat shows the dialog the agent drew, with one button per numbered choice and keys for Enter, Esc and the arrows, where the composer was. An answer is bound to the screen it was read from and is refused when the dialog changed. The "needs input" notification opens that sheet. The docs and the example site file start page sessions with `claude --permission-mode auto`, so only the remaining prompts reach the chat. See `docs/web-client.md` "Answering".
- Queue web chat messages while the agent works: up to five per session, delivered in order when the agent is ready, kept across a restart of the web server. A delivery that wsxd refuses stays in the queue with the reason until it is retried or removed.
- Show what the agent is doing in the web chat: the timeline now carries tool calls with their outcome and thinking markers from the agent's own saved conversation, for Claude, Pi, omp and Codex. Tool input, tool output and thinking text are not sent.
- Let one web site reach several projects, or every project wsx manages. Anything outside the grant answers 404.
- Show the web client dark by default when its theme has a dark scheme. A device can choose light under Settings. The operating system preference is no longer read.
- Make the web client's terminal panel a real terminal. It is xterm.js on the pane's own byte stream over a WebSocket, with a key row, paste, an arrow drag pad and touch scrolling on phones, Korean and other IME input, and reconnect after a lost connection. A pane still has one controller: the page shows when a wsx window holds the terminal and takes it only on request. *Fit screen* resizes the pane to the browser; *Host size* scales the view instead. The text-snapshot terminal and its `api/terminal` routes are removed. See `docs/web-client.md`.
- Add a raw terminal stream to wsxd for clients that run their own emulator: a replay of the current screen, then the PTY bytes, with the same single-writer lease as the TUI stream. The wire protocol stays 16: the stream is an added request, announced by the capability `raw_terminal_stream` (daemon revision 23). The TUI's frame stream is unchanged, and an open 0.30 TUI does not defer the daemon upgrade. Against a daemon without the capability the web client's terminal panel says that the daemon must be upgraded; the other panels work. A running `wsx web serve` sends client presence with its poll, so it does not defer the upgrade either. See `docs/terminal-raw-stream.md`.

### Improvements

- Keep wsx windows open through a wsxd upgrade. A window whose installed `wsx` was replaced now re-executes into it in the same terminal, with the same process ID, and returns to the same project, worktree, session and mode, Terminal mode included. It waits while a form, prompt or search is open. When the installed `wsx` cannot start, the window keeps running the old code and says so. A window that follows no longer defers a handoff, even below the target protocol. Daemon revision 25. See `docs/daemon-live-upgrade.md`.
- Upgrade wsxd before a starting wsx opens. A start that finds an older wsxd with live handoff shows an upgrade screen with both versions, the number of running terminals and the step, instead of the workspace on the old daemon. While another window defers the upgrade it says which window to quit, asks again by itself and opens when the upgrade is done; `Enter` opens on the old wsxd and `q` quits. A handoff that rolls back is detected at once instead of after the 35-second wait, and the window opens on the running daemon with a notice. CLI commands are unchanged.
- Add `wsx daemon restart [--json]`. It replaces wsxd with the installed build through a live handoff, also when it is the build that is running. Sessions, shells and agents keep running. A wsxd of 0.30.1 or older cannot restart into its own build: the command says so and stops nothing.
- Make `wsx daemon stop` safe. With running sessions it prints how many would end and that `wsx daemon restart` keeps them; `--force` stops anyway. The help text now says that a stop ends running programs. `--json` reports the result.
- Say what to do when an upgrade waits. The notice names the window to quit, says that sessions keep running, and says that the upgrade then continues by itself. For a wsxd without live handoff it says that the upgrade will restart the sessions.
- Make the Terminal title bar readable at a glance. Project, worktree and session each have one look: the project is a filled chip, a worktree is an accent heading on the bar with no chip and no state icon, and a session is a chip that starts with its state icon. A dim rule takes the cell between two worktree groups, so a second worktree name no longer reads as one more session. Widths, order, truncation and overflow counts are unchanged. The project preview marks a worktree that needs attention with that session's own state icon instead of `●`, and the Terminal footer names the session and group keys only while another session or group exists. See `docs/terminal-context.md`.
- Recover the wsx screen and mouse after the terminal loses its modes. A terminal reset, such as Ghostty's `reset` action, cleared the screen and every mode without telling wsx, and the screen stayed blank until something forced a full draw. After a sleep, mouse capture could be gone, so the wheel scrolled the terminal and a drag selected across the whole window. wsx now writes its modes again and draws a full frame on wake, on focus gained and on resize, and an idle window asks the terminal about once a second and repairs within about a second of a reset. wsx now enables focus reporting and keeps a scroll margin that leaves out the last row; both are restored on quit. A terminal that answers no query is never asked again. See `docs/ui-state-ownership.md`.
- Treat a finished session as checked when you leave it. Entering a `done` session already cleared it to idle. A session that finished while you were inside it stayed `done` after you went back to the workspace, so `n` offered it again. Leaving Terminal mode, or switching to another session with a prefix key, now acknowledges that exact done report. A later working then done shows `done` again.
- State the protocol rule: the protocol number changes only when an older client can no longer talk to the new daemon. Additions use `Capabilities` and the daemon revision. wsxd now answers an unknown request on a mismatched connection with an error instead of closing the connection.

### Bug Fixes

- See a Claude permission dialog in a fresh session, and for a file. wsxd read only the last 12 rows of the screen, so a new Claude in a tall pane was never reported as "needs input", and it knew only "Do you want to proceed?", not "Do you want to create ...?". Daemon revision 26.
- End an exchange when its turn was interrupted. After Escape in the agent's terminal, or a denied dialog, the exchange stayed active until its deadline and every later request to that pane waited. It now becomes `interrupted` when the agent is idle again. See `docs/agent-orchestration.md`.
- Stop an agent turn with the provider's own interrupt. `wsx agent cancel` and the web client's Stop now send Escape to Claude, Pi and omp; Ctrl+C did not stop a working Pi, and in Claude its second press quits. An exchange no longer stays in `cancel_requested` after a working Claude was stopped: wsxd now re-reads Claude's screen after the interrupt and settles the exchange from it. Daemon revision 24. See `docs/agent-orchestration.md`.
- Deliver a request to a freshly started Claude. Its empty editor shows a dimmed suggestion (`Try "..."`), which wsxd refused as a possible draft, so the first message to a new Claude session failed with `editor_unavailable`. The dimmed line now counts as empty.
- Start a new exchange for the first web chat message after a stop. The web client tried to continue the cancelled exchange, which wsxd refuses.

### Packaging and verification

- Add `scripts/test-web-client.py`: one journey against an isolated wsxd and real web servers, with headless Chrome under `--browser`. `scripts/web-real-agent-check.py` checks a real Claude turn, a permission dialog and a question; it is run by hand.
- Vendor the browser files with pinned versions and checksums: `scripts/vendor-preact.sh`, `vendor-xterm.sh`, `vendor-fonts.sh`, `vendor-markdown.sh` and `vendor-mermaid.sh`. `THIRD-PARTY-NOTICES.md` lists them with their license texts.
- Extend `scripts/runtime-smoke.py`, `scripts/test-terminal-context.py` and `scripts/test-ui-state.py` for the raw terminal stream, the upgrade screen, a released-build upgrade, daemon restart, the TUI re-exec and the terminal mode repair.
- Keep routine test socket paths below the macOS path limit in a long worktree path. `AGENTS.md` and `CLAUDE.md` describe the web crate, the sandboxed Zig build and one in-process test flake.

## [0.30.1] - 2026-10-12

### Breaking Changes

- Rust callers that construct `TuiClientPresence` must provide its new `max_protocol` field. Older JSON without the field decodes as a legacy TUI.

### Improvements

- Make the CLI easier for an agent to drive. `wsx worktree create --from <ref>` starts the branch from a chosen commit, tag or branch instead of the default branch. A branch that already exists is checked out instead of failing, and a git failure now shows git's own error instead of only the exit status. `wsx worktree create --json` prints the worktree path, branch, project, session ID and pane ID. `wsx session current [--json]` reports the calling pane from `WSX_PANE_ID` with its session, project, worktree and agent status, without starting wsxd. Commands called with `--json` no longer print the runtime notice on stderr, and other commands print it on one line.
- Upgrade wsxd while wsx windows stay open. A TUI now tells the daemon the newest protocol it can reconnect to, and a live handoff no longer waits for such a TUI to quit. The daemon marks the whole transition, so attached windows wait for the successor without an error notice and without starting a competing daemon. A window in Terminal mode reattaches to the same PTY and keeps its screen; keys typed during the reattach are dropped. Older TUIs, and any TUI older than the target protocol, still defer the handoff until they quit. The running daemon applies its own rule, so this takes effect from the upgrade after this one. Daemon revision 22. See `docs/daemon-live-upgrade.md`.
- Retain successful and failed Claude and Codex inspection records in an owner-only bounded journal. The shared helper reads only validated metadata, refuses symlinks and unsafe files, retains at most 32 records/64 KiB, and never changes the original reporting result. Claude integration is 21; Codex is 15. Source-verified 0.29.3 reporter observations support existing sessions without restarting them.
- Add opt-in Claude and Codex diagnostics with bounded hook, reporter and daemon version observations, report error codes and environment-marker presence. A private hook-directory marker also enables diagnostics for existing sessions that call the refreshed hook. Agent launch version and startup environment remain unknown; installing files alone does not prove that old sessions loaded them.
- Preserve default hook output, report-once behavior, generation fences and transcript settings. These diagnostics do not fix or verify the reported Claude Stop-hook, Codex hook-exit, prompt-submission or transcript-saving failures. See `docs/claude-inspection.md` for installation and existing-session instructions.

### Bug Fixes

- Stop a live daemon handoff from writing the visible screen twice. The keyboard-state transfer also carried the screen text, so the last lines of a shell session appeared doubled after an upgrade.

- Stop a terminal session from staying blank after an interrupted program. A program that enables synchronized output (mode 2026) and is ended by Ctrl+C before it releases the mode no longer hides all later output: the mode now expires after one second, as in Ghostty. Daemon revision 21 records this runtime-owned repair; the fix reaches a session after its daemon upgrades. See `docs/terminal-synchronized-output.md`.
- Keep a resumed agent pane alive when a repeated Ctrl+C arrives after the agent has ended and before its fallback shell starts. The resume supervisor now ignores interactive signals until the shell replaces it.

### Packaging and verification

- Resolve release-workflow validation against the validator's own checkout, so running it from another directory cannot inspect an unrelated workflow.
- Promote the two source-only inspection increments to stable 0.30.1, with matching workspace versions and exact internal pins; registry dependency versions remain unchanged. Installed-hook reporting/journal integration coverage includes both providers, current and released reporters, successful and rejected reports, privacy bounds, malformed records, unsafe paths, generation fencing and private-runtime cleanup.

## [0.30.0] - 2026-10-08

### Breaking Changes

- Rust callers that construct `AgentExchange` must provide its new optional `delivery_sha256` and `native_input_id` fields. `Capabilities` construction also needs `agent_exchange_bound_receipts`. Older JSON without these fields still decodes as no native input binding or support.

### Fixes

- Keep an accepted native request active when the pane reports DoneObserved. Daemon revision 20 preserves its one-active-exchange fence, writer claims, expiry and cold-recovery interruption until a request-bound result or explicit terminal failure. Legacy unbound observations remain compatible.
- Enforce an active exchange's deadline when a native receipt arrives, even without a preceding read probe. Daemon revision 19 records expiry before refusing the late receipt.

### Improvements

- Connect Pi's native input and expanded prompt to guarded receipts when the loaded observer, Pygmalion Goal Run owner and daemon bound-receipt support are available. Hold completion through automatic Task continuations and compatible steering; withhold it after abort, error, Task/session/runtime replacement or disposal. Other installations stay lifecycle-only. Deterministic SDK fixtures are not a real-model orchestration trial.
- Add guarded native receipt submission with exact pane, live runtime, round and delivery digest checks. Bind completion to a prior accepted native input ID; reject overdue DoneObserved completion and legacy receipt bypass after binding. Older daemons refuse the distinct request instead of silently dropping its guards.
- Bind each successful exchange delivery round to the SHA-256 of its exact terminal input without persisting prompt text in receipts. Clear the binding before continuation and on failed delivery; legacy responses remain unbound. Native adapter correlation and completion remain separate requirements.
- Add machine-facing `agent exchange-receipt` submission through the existing generation/round/capability-bound daemon contract. Require the injected generation and an existing daemon; do not bootstrap, retry, infer completion from lifecycle or enable an unconnected provider adapter.
- Add scoped `wsx agent context --metadata-only` discovery with bounded identities and advisory readiness, without reading native transcripts. Default native-history projection remains available.
- Generate portable shell-hook configuration from provider root environment variables and `HOME`, using the same root declaration as installation. Migrate owned legacy bindings from another device while retaining unrelated hooks and compound commands. Preserve quoted paths, stdin and repeated-install behavior. Install integration assets separately on each device; already-running agent environments are unchanged.

### Documentation

- Update both READMEs for metadata-first discovery and conditional native Pi completion. State that arbitrary-provider submission, structured questions and the reported Claude handoff repair remain outside the verified increment.

## [0.29.3] - 2026-10-06

### Fixes

- Preserve final Unix PTY output and clipboard effects before announcing natural pane exit. Let the reader publish exit after draining; the child waiter still reaps and cleans up the process group. Daemon revision 18 records this runtime repair.
- Wait through routine scheduler retirement replies and read-only probe disconnects before starting the adjacent replacement. Observe the existing singleton lock without creating or removing it, refuse replacement when its path disappears, and bound polling and trickled response reads by the startup deadline. Never replay a mutation or shutdown after uncertain delivery. Validation, malformed responses and unsupported protocols still fail closed.
- Show terminal context as project and worktree background groups with `›`, stable session order and a brighter current session. Preserve provider identity, Unicode width, overflow counts and the complete terminal viewport.
- Count Workspace search matches inside folded projects, worktrees and routine headers without unfolding them. Highlight visible text matches, underline folded ancestors and show logical counts. Respect the selected group and clear feedback when search ends.

### Navigation

- Make `Prefix+h/l` the primary previous/next session keys. Keep `j/k` and arrow aliases, bare terminal input and attention navigation. An explicitly configured h/l Workspace escape suffix takes precedence.

### Verification

- Run native routine retirement, held-lock and trickled-response timeouts, missing-lock refusal, genuine-error and no-replay CLI scenarios on both CI platforms. Local macOS scenarios and the real wide/mobile TUI journey passed. Publication requires passing branch CI on Linux and macOS for the exact release commit.

## [0.29.2] - 2026-10-05

### Fixes

- Use padded backgrounds instead of literal brackets around terminal title session chips. Keep the current session brighter and bold, with quiet backgrounds for neighbors. Preserve provider labels, Unicode truncation, overflow counts, navigation and terminal dimensions.

### Release verification

- Pour generated bottles through trusted formula metadata and checksum-verified Homebrew cache bytes, retaining the existing native receipt/test/version/architecture gates without bypassing package-path policy.
- Make the synthetic legacy scheduler retire explicitly after a read-only status probe, with bounded fixture I/O. This removes its connection-reset race during CI without changing runtime behavior or claiming abrupt production shutdown resilience.

### Documentation

- Clarify the background-only chip contract and retain candidate verification, viewport/navigation checks and release-boundary limitations.

### Known limitations

- Existing Codex identity and already-loaded integration recovery still need provider-specific verification or a supported reload. This presentation-only patch does not change daemon revision or force a live daemon restart.

## [0.29.1] - 2026-10-05

### Fixes

- Recover removed versioned agent reporters through a validated current CLI when an older daemon cannot publish its stable entry or that entry points to a removed keg. Retain generation checks, reject unsafe entries, and never retry a delivered or rejected report. Already-loaded older integrations still require their provider's reload or restart mechanism.
- Reserve session-row width for reported provider identity before truncating long names or showing ports.
- Prevent live Working agents and ordinary foreground jobs from causing inactivity collapse or displaying `stale`. Preserve manual folding, stored collapse provenance, and trusted activity-credit rules.
- Compact folded project/worktree activity badges to the state indicator and active-session count, without repeating the word `active`.

### Navigation

- Use one `Prefix+j/k` (or Down/Up) cycle across the current project's sessions in stable Workspace order. Remove duplicate `Prefix+h/l` and Left/Right cycle bindings and release `h/l` as custom Workspace escape suffixes. Attention jumps and bare terminal input are unchanged.
- Replace the spread-out title with a project prefix and highlighted current worktree/session chip in a sliding session preview. Preserve provider identity, cross-worktree attribution, hidden counts and tiny-view position without changing the terminal rectangle.

### Release verification

- Pour each generated Homebrew bottle on its native CI runner before uploading it. Require Homebrew's poured receipt, formula tests, the exact CLI version and both universal companion architectures.

### Known limitations

- The missing identity of one already-running Codex session remains under separate investigation. Current hook discovery reports trusted handlers, but does not expose that session's cached authorization. This release fixes removed-reporter ENOENT recovery, not every cause of missing agent identity. See [Codex reporting follow-up](docs/codex-reporting-followup.md).

### Documentation

- Require exact ownership records, retained results and verified removal when cleaning up task-created orchestration sessions. User-owned sessions are never removed based on `done` state alone.

## [0.29.0] - 2026-10-03

### Breaking Changes

- Replace `WorkspaceCacheSnapshot`/`save_cache` with `WorkspaceCacheChanges`; `AppliedCache` gains saved worktree expansion. Rust callers must update cache APIs and tuple destructuring. `AgentSessionRef` gains optional `transcript_path`; use constructors or initialize that field. Legacy JSON still decodes.
- Reserve `h` and `l` as Terminal prefix suffixes. Custom Workspace escape bindings using those keys must change.

### Features

- Add a colored terminal context bar with the current session, attention-ranked same-project peers, cross-worktree attribution and overflow. Prefix+h/l and Prefix+Left/Right cycle the full ranked ring, including folded worktrees. Existing attention and sibling navigation stay unchanged. Worktree preview ports appear on their owning session rows rather than in terminal titles.
- Add `wsx agent context [target] --json` for exact target discovery, project/worktree metadata, advisory readiness, and bounded provider-native Claude, Pi/OMP and Codex history. Missing, unreadable, unsupported and ambiguous history is explicit; tools and reasoning stay excluded.
- Add opt-in `--stash-draft` to Claude request/continuation. Preserve an idle multiline draft with the documented stash binding and require fresh empty editor evidence before delivery. Intent stays persisted when preparation or PTY delivery fails; protocol 16 is unchanged and separate request variants fail closed on older daemons.
- Show dominant descendant status and an active-session count on folded project/worktree rows, including unfocused panes, without changing runtime authority or stale provenance.

### Bug Fixes

- Publish an upgrade-stable daemon-owned reporter link after ownership commits. Recover removed legacy reporter paths in updated adapters without replaying rejected reports. Accept integration updates and restart agents to load the recovery; live shells and PTYs stay running.
- Release daemon state while waiting for a draft-stash ACK so fragmented PTY frames can finish in both initial and continued exchanges. Keep mutation and pane-operation fences.
- Pin `anyhow` to `1.0.103`, which fixes the `Error::downcast_mut` unsoundness reported by RUSTSEC-2026-0190.
- Observe foreground jobs promptly without inferring provider identity. Reject stale runtime scans, preserve event-reported Working until an interrupt has fresh terminal evidence, remove inherited Claude child-session markers from fresh panes, and report bounded shell-hook failures.
- Keep exact pane lifecycle callbacks and context reads from starting or replacing a missing daemon. Preserve same-session transcript paths and refuse stale-generation updates.
- Persist per-key UI intent under a bounded lock instead of complete window snapshots. Fence stale collapse, merge adaptive activity against durable state, preserve removals and revision acknowledgements, and load asynchronous worktree expansion once. Import the v2 cache into v3 without rewriting predecessors or allowing old writers to overwrite v3.
- Keep terminal modes owned by the TUI thread. Restore them after startup/partial-init failures, owner panics and failed external editors; worker panics return bounded notices instead of changing terminal modes. Retain a private bounded metadata journal, never terminal contents.

### Maintenance

- Prepare the isolated non-web 0.29.0 workspace/pins/lock metadata, update integration version markers, add reporter and fragmented-PTY regressions, and repair isolated fixture setup. Preserve web, bridge and structured-conversation work for 0.30.0.

## [0.28.2] - 2026-09-24

### Bug Fixes

- Complete deferred live daemon upgrades from protocol 11 through 15 without replacing live PTYs. Same-target retries remain idempotent, and a newer release can convert an older pending cold replacement into a live handoff; same-version different builds and newer pending targets remain protected. `wsx runtime status` now shows legacy lifecycle details for incompatible daemons.
- Clear stale agent labels and Working indicators when a resident Pi or OMP adapter stops renewing its pane-bound presence for 30 seconds. Renewals do not write persisted state; expiry retains resume metadata. Hook-only and server-global integrations continue using lifecycle events and `wsx agent detach` rather than an unsafe idle timeout.

### Maintenance

- Prepare exact 0.28.2 workspace version pins and document path-sensitive rebuilds and binary overrides for isolated release checks.

## [0.28.1] - 2026-09-22

### Bug Fixes

- Replace legacy standalone routine schedulers automatically by advancing the routine protocol for the `wsxd` ownership boundary. This closes the `0.26.2` to `0.27.0` migration gap where both processes advertised protocol 3 and the obsolete scheduler was reused indefinitely.
- Keep runtime lifecycle changes readable. TUI notices now queue bounded bursts for a two-second minimum, report successful or deferred daemon upgrades with blocker details, and retain the first disconnect message through a fast reconnect.
- Report daemon state truthfully from `wsx runtime status`: stopped, unavailable, incompatible but running, upgrade available, replacement deferred, or ready, with client and daemon version, protocol, and revision evidence when available.

## [0.28.0] - 2026-09-22

### Features

- Reorder worktrees within a project with the existing `m` then `j/k` Workspace flow. The order persists by stable worktree path across refreshes and restarts, while newly discovered worktrees remain visible.
- Temporarily expand a compact desktop sidebar while Terminal waits for the key after its prefix, without leaving Terminal mode. The behavior defaults on, can be disabled in Global Settings or with `terminal_prefix_shows_sidebar = false`, and preserves the existing persistent Prefix+B sidebar toggle.

### Bug Fixes

- Stop unrelated Pi post-answer maintenance from leaving completed sessions marked Working. Pi integration version 16 binds delayed completion to the exact agent run, lets a real continuation supersede it, and retries transient lifecycle-report failures instead of silently retaining stale state.
- Stop exited or explicitly detached agents from retaining a live provider indication. Runtime exit now detaches preserved resume identity, while `wsx agent detach` provides an exact generation-bound repair when an agent misses its shutdown event but leaves the managed shell running.
- Keep Korean, CJK, emoji, and other width-2 terminal glyphs inside the terminal viewport when a sidebar or resize crops away their spacer cell, instead of allowing the orphaned glyph to wrap into the next screen row.

### Maintenance

- Prepare the 0.28.0 workspace versions and exact internal dependency pins for tag-driven publication.
- Keep vendored scheduler startup-descriptor contract tests with the `asched-core` validator they cover so the excluded scheduler suite compiles and runs in release validation.

## [0.27.0] - 2026-09-21

### Features

- Replace the single automatic-collapse timeout with typed disabled, flat, and adaptive policies. New configurations start with a 24-hour adaptive window, gain one bounded 12-hour credit on the first trusted activity of each UTC day, persist earned windows by project, reset after expiry, and cap at 28 days. Existing numeric values retain fixed-window behavior, with zero mapped to disabled.
- Finish Workspace filtering when Enter confirms the sole search result, while retaining Enter-based cycling when several results match.
- Prompt at TUI startup before updating installed agent integrations whose embedded version is outdated. Missing integrations remain demand-driven, and declined updates prompt again on the next launch.
- Add provider-neutral, runtime-generation-bound agent exchanges with durable delivery receipts, lifecycle-observed state, bounded terminal-frame fallback, multi-round continuation, cancellation requests, and non-overlapping writer claims. Pi, Claude, Codex, OpenCode, Hermes, and future terminal agents share the same wsx contract; optional native adapters may strengthen evidence without changing commands.

### Bug Fixes

- Keep macOS wake mode active through long Claude responses with an asynchronous five-minute heartbeat bound to the exact prompt and runtime generation. Completed, blocked, failed, detached, replaced, and superseded turns revoke old heartbeat authority instead of letting stale helpers prevent sleep.
- Revive an exited session pane instead of leaving a listed but hollow session after Ctrl+C or an agent crash. `wsx session restart` restarts the exact exited pane with its saved command or persisted native agent session, accepts only an exited pane at the caller's expected revision, fences the dead pane's old stream, and leaves the pane unchanged when a start fails. An exited pane also stops reporting a live foreground job or listener, and generation-bound callbacks prevent a replaced or abandoned runtime from publishing change or exit observations for a pane it no longer owns.
- Run routine scheduling through the shipped adjacent `wsxd` binary instead of trying to launch an unshipped standalone `asched` executable during periodic refreshes. A newer client now safely stops an older internal scheduler with that scheduler's protocol before starting the adjacent replacement, while refusing to downgrade a newer scheduler.
- Clear a project's persisted `stale` marker on the first explicit interaction, including expanding the project or entering one of its sessions, instead of requiring a later manual collapse.
- Reconcile known Claude sessions with bounded OSC title and live prompt evidence so immediate Escape or Ctrl+C interruption cannot leave a delayed lifecycle hook active. Preserve event-reported completion and errors, ignore ambiguous terminal text, and keep heuristic working state outside wake-mode authority.
- Start cold recovery only after lifecycle reporting is available, restore ordinary shells first, and load lifecycle-capable resumable agents one at a time so several large Claude histories do not compete for startup memory. Keep pending identity detached until the resumed runtime reports its generation, and continue safely after failure, timeout, deletion, or shutdown.
- Restore live-handoff terminal histories with bounded parallel workers so protocol-15 daemons can transfer a full 43-pane cohort within the legacy readiness window without changing PTYs or process IDs.

### Maintenance

- Prepare the 0.27.0 workspace pins and locked package metadata, align runtime smoke and latency harnesses with protocol 16, keep generated Task Context sessions out of the release tree, and add evaluated operating guidance for provider-neutral agent exchanges.

## [0.26.2] - 2026-09-13

### Bug Fixes

- Bridge live daemon handoff from affected protocol-15 daemons whose cross-version executable check used the source daemon's version, preserving live PTYs while allowing lifecycle reports and runtime refreshes to reach the successor.
- Persist `stale` as the cause of the last automatic project collapse, retain it through restart and expansion, clear it after a manual project collapse, and recover the identifiable `0.26.1` migration cohort.

## [0.26.1] - 2026-09-13

### Bug Fixes

- Preserve each project's expanded or collapsed state across restarts until the configured inactivity period expires, and reset that period when the user interacts with the project.

### Maintenance

- Document the managed-terminal marker required when running isolated outer-TUI smoke probes from a wsx session.
- Set workspace package versions and exact internal dependency pins to `0.26.1`.

## [0.26.0] - 2026-09-12

### Features

- Add standalone session creation and deletion to the CLI, with cwd and sole-target inference, explicit project/worktree scoping, reusable session IDs, and JSON output for low-turn agent control.

### Bug Fixes

- Report the session created with a worktree as `session:` instead of incorrectly labeling its ID as a pane.
- Collapse projects without an available session instead of treating missing activity timestamps as permanently fresh.
- Hide agent identity after a teardown-capable integration exits back to the shell while retaining native conversation metadata for recovery.

- Clear stale Claude working state when Workspace sends Ctrl+C, matching Terminal Escape interruption while preserving newer lifecycle reports.
- Prevent embedded full-screen applications from scrolling the outer wsx display when dense frames reach the physical bottom-right terminal cell.
- Keep drag selection aligned with terminal history by forwarding captured vertical boundary motion and driving Ghostty viewport autoscroll with synchronized selection ticks.

### Maintenance

- Make cold-recovery integration assertions wait for daemon readiness before reading the persisted startup state.
- Add evaluated agent-development guidance for low-turn wsx session control, refresh bundled release and reader-briefing workflows, let release audits continue on proven non-secret scanner matches, and link runtime ownership boundaries to canonical knowledge.

## [0.25.0] - 2026-09-10

### Breaking Changes

- Retire the unused wsxd-owned structured Pi RPC conversation API while preserving terminal-hosted Pi lifecycle reporting and native session identity.

### Improvements

- Replace restart-based wsxd upgrades with transactional Unix live handoff. Running shells, terminal agents, foreground jobs, and listening servers keep their PTYs and process IDs while the successor reconstructs functional terminal state.
- Keep protocol-11 through protocol-14 daemons usable during the one-time transition to handoff-capable wsxd, preserving workspace sessions, activity timestamps, and stale-project folding until safe replacement.
- Keep routine daemon reuse, deferred replacement, handoff, and cold restoration silent in the TUI. Opening wsx continues to show the existing workspace while only incompatible newer-daemon action remains visible.
- Prioritize Blocked sessions when `N` or `Shift+N` navigates attention by default, with a typed Global Settings choice to restore Workspace order. Group navigation remains unchanged.

### Compatibility

- Protocol 15 adds bounded live-handoff registration and capability negotiation. The old daemon retains ownership until the successor imports PTY descriptors, binds the canonical socket, and reports ready; pre-commit failure resumes the old daemon.

### Maintenance

- Exclude repository-local `.work` test scratch at every package depth so CI package verification remains clean after the test suite.
- Wake pausable PTY readers through a control descriptor instead of periodic polling, avoiding platform-specific rewrite latency during live-handoff-capable operation.

## [0.24.0] - 2026-09-09

### Features

- Add a versioned executable worktree-review API and an external read-only Git provider.
- Enter Local file review with Tab without changing tree expansion keys. Inspect unified diffs, navigate hunks, and refresh changed snapshots without replacing the text being read.
- Preserve the existing preview style and metadata where space permits. Review stays outside the live agent terminal.
- Add a bounded wsxd-owned structured conversation API for Pi RPC, including typed events, interactions, command discovery, persistence, and native session restoration.
- Add prefixed Terminal group navigation that selects the first session needing attention, then an idle agent fallback, without leaving the current terminal when no target qualifies.
- Transfer writable terminal control to the latest explicit wsx instance while generation-fencing the displaced stream and returning it to Workspace with a concise notice.

### Compatibility

- Protocol 13 adds worktree review requests, cancellation, and provider availability. Existing event-only and passive-sidecar manifests remain valid, and stale sidecar responses cannot replace a newer view.
- Protocol 14 adds structured conversation requests, events, capabilities, and restart restoration. Attachments and model selection remain explicitly unsupported.

### Maintenance

- Keep runtime smoke and terminal latency clients synchronized with protocol 14.

## [0.23.0] - 2026-09-08

### Features

- Add project-configured worktree branch prefixes and optional default shell sessions with startup commands.
- Extend trusted executable plugins with one versioned, bounded Terminal sidecar contribution while keeping layout, styling, PTY geometry, and input ownership inside wsx.
- Reorder worktree preview details into branch, path, sessions, remote, commits, nested sources, and local changes, with local files using the remaining visible height.

### Compatibility

- Preserve event-only version 1 plugin manifests through additive sidecar defaults, and reject malformed, oversized, timed-out, or stale plugin views without replacing the last valid snapshot.

### Maintenance

- Keep terminal-stream test sockets portable in long linked-worktree paths.

## [0.22.3] - 2026-09-08

### Improvements

- Reuse wsxd across UI-only wsx releases, and defer required daemon replacement while an agent works, a foreground job runs, or a local server listens.
- Show deferred daemon replacement only once at TUI startup, then report completed replacement immediately after reconnect or on the next startup with a warning that terminal sessions restarted from saved commands.
- Clear Claude working state when its session ends, keep rate limits blocked rather than active, and retry one transient release lookup without adding periodic update notifications.
- Keep session mutation revisions separate from focused-pane lifecycle revisions so newly created agent sessions can be reordered immediately.
- Keep release validation and Unix socket tests compatible with macOS system constraints.

## [0.22.2] - 2026-09-07

### Features

- Place the full terminal breadcrumb below terminal content by default in Workspace previews and Terminal mode, with a typed setting to restore top placement.

### Bug Fixes

- Preserve cached project expansion when a legacy daemon cannot provide authoritative activity timestamps, while continuing to auto-collapse projects with known stale activity.
- Tell users to stop a legacy daemon explicitly when its persistent terminal runtimes prevent automatic replacement, and explain that saved sessions restart on the next launch.
- Synchronize clipboard-before-exit runtime smoke after terminal subscription instead of relying on a fixed child-process delay.

## [0.22.1] - 2026-09-07

### Bug Fixes

- Retry project discovery and daemon synchronization once when session creation races a worktree refresh, while preserving the existing refusal for genuinely absent worktrees.
- Set Claude turns stopped by rate limits to blocked and other terminal API failures to error, and invalidate stale working state after Claude's adapter-declared Esc interruption without restoring the unpaired permission-request state.
- Clarify that an exited older TUI's replacement presence expires within three seconds while working agents remain explicit upgrade blockers.
- Harden Homebrew recovery by validating its job graph and trust ordering, isolating source checksums from prior bottles, and publishing bottle tarballs under the canonical filenames declared by Homebrew metadata.

### Maintenance

- Keep daemon replacement tests independent of the current package version and document exact CLI test-filter discovery.

## [0.22.0] - 2026-09-07

### Features

- Add documented one-shot agent templates to routine creation while keeping generated argv visible and editable; Custom starts with an empty command.
- Add a selectable agent-integration manager under Global Settings and offer setup only after explicit agent-template use. Per-agent declines remain suppressed until manual installation.
- Report authoritative Codex lifecycle state through non-destructive hooks for idle, working, blocked, done, and interrupted turns when Codex 0.150.0 or newer is available.

### Bug Fixes

- Keep approved Claude tool execution working instead of leaving it blocked after an interactive permission request; adapter upgrades remove only the stale wsx-owned permission hook.
- Reduce foreground Terminal polling and daemon presentation cadence from 8 ms to 4 ms, and measure added p95 over 100 samples so shared-runner outliers do not decide the existing 16.7 ms performance gate.

### Documentation

- Condense the English and Korean READMEs and present the product tour as parallel screenshot tables.

## [0.21.0] - 2026-09-06

### Breaking Changes

- Remove the Workspace `c` clean command because merged worktrees could still contain uncommitted or untracked files that its forced removal discarded; use confirmed `d` deletion instead. ([`268a741`](https://github.com/vlwkaos/wsx/commit/268a741030b09902bb2881b82dbfcd7513011a59))
- Reserve `a` and `i` as two-chord Terminal command suffixes; configurations using either as the Workspace-focus suffix must choose another key. ([`9fd4d6d`](https://github.com/vlwkaos/wsx/commit/9fd4d6d7a76b34c7c3404f865ddf046e625b63b9))

### Features

- Add prefixed next/previous idle and active session iteration in Terminal mode while preserving unprefixed PTY input. ([`9fd4d6d`](https://github.com/vlwkaos/wsx/commit/9fd4d6d7a76b34c7c3404f865ddf046e625b63b9))

### Bug Fixes

- Reject nested TUI startup inside wsx-managed terminals with an actionable error while keeping explicit CLI subcommands available. ([`990d33d`](https://github.com/vlwkaos/wsx/commit/990d33d95a1a5d3c3fedc2e94212d80d670689e3))
- Limit startup integration prompts to detected agent executables and return explicitly aborted Pi turns to idle instead of marking them done. ([`547ff02`](https://github.com/vlwkaos/wsx/commit/547ff023839bc2636441dfe34fea6b5699d3cb21), [`08a8e79`](https://github.com/vlwkaos/wsx/commit/08a8e7915a3b1d12dc14b47cb9514aa3c9c08e7a))
- Keep Pi sessions working across automatic continuations by reconciling deferred settlement with Pi's authoritative idle state. ([`08a8e79`](https://github.com/vlwkaos/wsx/commit/08a8e7915a3b1d12dc14b47cb9514aa3c9c08e7a))
- Report standard blocking Pi extension dialogs as blocked without requiring extension-specific wsx integration. ([`08a8e79`](https://github.com/vlwkaos/wsx/commit/08a8e7915a3b1d12dc14b47cb9514aa3c9c08e7a))
- Re-prompt for detected agent integrations when their expected adapter versions change, even within the same wsx development version. ([`8d4c2f7`](https://github.com/vlwkaos/wsx/commit/8d4c2f79c0e61f37e806c84639f9b74fc5845947))
- Let protocol-compatible wsx versions share one daemon without downgrades. Version and build-aware TUI presence now blocks migration only while another TUI build or a fresh authoritative `working` agent remains; newer targets supersede stale queued builds, idle PTYs restore from saved recipes, and legacy replacement conflicts keep the workspace snapshot visible with an actionable notice. ([`8d4c2f7`](https://github.com/vlwkaos/wsx/commit/8d4c2f79c0e61f37e806c84639f9b74fc5845947))
- Keep the host-and-user-owned wsxd, terminal buffers, and agent processes alive across SSH disconnects and later same-user logins instead of cold-resuming conversations in replacement PTYs.
- Let an immediately exiting recovered agent wait for wsxd startup publication before it clears its generation and opens the configured fallback shell.
- Persist project, worktree, and routine expansion changes before the next input wait so abrupt TUI loss restores the complete Workspace tree state.

### Documentation

- Add a bilingual current-product tour with six privacy-safe screenshots covering Workspace, session iteration, groups, Terminal panes, routines, and settings. ([`f924105`](https://github.com/vlwkaos/wsx/commit/f924105375e2904fd1976681b314eb1bd81fad25))

### Maintenance

- Add project-local headless terminal, capture, GitHub, and manual release workflow guidance. ([`24db03c`](https://github.com/vlwkaos/wsx/commit/24db03cf0786acf32a11ac55c7d9949f2c656160), [`bc42812`](https://github.com/vlwkaos/wsx/commit/bc4281269624e65e70ad1343b4e7edb6e6325d5f))
- Keep terminal latency smoke isolated from an outer wsx-managed terminal. ([`b9b4e6a`](https://github.com/vlwkaos/wsx/commit/b9b4e6aae81e2625b2692b742d3cb78dd77c7c74))
- Separate branch-only CI from stable tag publication, with pinned Rust, Zig, and Nextest setup, universal adjacent binaries, checksum and bottle verification, OIDC `wsx-core` publishing, and a validated two-binary Homebrew update.

## [0.20.0] - 2026-09-03

### Breaking Changes

- Replace the interim Herdr runtime with wsx-owned `wsxd`, `wsx-terminal`, typed project/session/pane APIs, and pinned `libghostty-vt`. Existing external-runtime sessions are not imported. ([`5a46fe9`](https://github.com/vlwkaos/wsx/commit/5a46fe937ebd8b7a40020c52830f602629ada98b), [`c3f5468`](https://github.com/vlwkaos/wsx/commit/c3f5468043ec10eb09a3e3b28e64cbfff7f9cbfc), [`f13dea4`](https://github.com/vlwkaos/wsx/commit/f13dea4f842539edea3cb1d9809c09a5187e93c6), [`24b5309`](https://github.com/vlwkaos/wsx/commit/24b53095347edc6a74813adeae2069e17f430d56))
- Replace foreground attach and captured previews with a writable semantic terminal viewport and Workspace/Terminal modes. The default configurable focus sequence is `Ctrl+A`, then `W`; double the prefix to send it literally.
- Change `SessionInfo` to wsx `SessionId`, `PaneId`, and `TerminalId` identities with normalized `AgentState`, pane layouts, and subordinate pane projections.
- Replace `wsx herdr status` with side-effect-free `wsx runtime status` and replace `WSX_HERDR_BIN` with `WSX_DAEMON_BIN`.
- Bump the local wsxd protocol to version 11. Version 2 introduced mandatory same-connection handshakes and persistent terminal streams; version 3 added authoritative cell-width occupancy and initial viewport dimensions; version 4 added ephemeral pane listener metadata; version 5 added private restart launch recipes and bounded initial commands; version 6 added persisted project activity and terminal-entry timestamps; version 7 added typed native agent-session references and restoration capability; version 8 added daemon-owned session reordering and ephemeral clipboard-write delivery; version 9 added ephemeral PTY foreground-job metadata; version 10 added controller-local terminal selection ranges and pointer-boundary metadata; version 11 binds agent mutations to the current runtime generation. ([`97a7ed4`](https://github.com/vlwkaos/wsx/commit/97a7ed480ffb55150c4a599c086844724e5ab62f), [`2316d31`](https://github.com/vlwkaos/wsx/commit/2316d31ceca678d6ce9cdc1fcf2b10af2a1e537a), [`d91398d`](https://github.com/vlwkaos/wsx/commit/d91398db3c1d86890e0ea9894ddb0542b739edb6), [`86dccca`](https://github.com/vlwkaos/wsx/commit/86dccca05ac8cd73f81e44126c889f2ad99d4db6))
- Replace tab commands, flags, config, cache, and single-membership behavior with canonical multi-group selection. Stored tab data migrates once; legacy CLI surfaces are removed.
- Reserve suffixes `b`, `j`, `k`, `n`, and `q` in two-chord `terminal_escape_chord` values for Terminal commands. Existing configurations that used one of these suffixes to focus Workspace must choose another suffix; single-chord focus configurations remain unchanged.
- Move 0.20 global configuration and UI cache state to `config-v2.toml` and `workspace-v2.toml`. First launch imports legacy tabs/groups and active-tab cache data once without modifying the old files, preventing wsx 0.17 from erasing 0.20 fields. ([`c390b89`](https://github.com/vlwkaos/wsx/commit/c390b89698874333d469e661a92fd31c1686d093))

### Features

- Add a persistent same-user daemon that owns PTYs, Ghostty terminal state, revisions, bounded snapshots/events, writable leases, persistence, pane mutations, normalized agent reports, and executable plugins.
- Add secure wsx-owned integrations for the 17 agent targets supported by the former Herdr runtime, with `wsx agent install <target>`, a version-scoped TUI startup prompt for detected missing integrations, authoritative lifecycle state for Pi, OMP, Claude, Kimi, OpenCode, Kilo, and MastraCode, honest identity-only reporting elsewhere, stable pane identity from wsxd, parenthesized session identity such as `(pi)`, and typed native session references for cold conversation restoration. ([`3930bad`](https://github.com/vlwkaos/wsx/commit/3930badf62f76ce35cb813dbd9db6820d3e64d36), [`7e40633`](https://github.com/vlwkaos/wsx/commit/7e406335ceb8e1645d9a655588bf3e35f7f4c371), [`4f5d531`](https://github.com/vlwkaos/wsx/commit/4f5d531c36ee5c72722850d7a0e55b88fb650f37))
- Add semantic keyboard and mouse encoding against authoritative terminal modes, styled cell frames, application-requested cursor shape, viewport resize, explicit lease takeover, and pane split/focus/close operations. ([`58e7571`](https://github.com/vlwkaos/wsx/commit/58e7571a8d28dbddc7621e8a3df8d9fbf58888f0))
- Add Ghostty-authoritative terminal text selection with drag copy, word selection on double-click, line selection on triple-click, Shift override for mouse-reporting applications, semantic frame highlights, ordered clipboard effects, and cleanup across viewport, geometry, screen, stream, and exact lease-generation boundaries.
- Add confirmed Workspace `Q` hard quit, `wsx daemon stop`, and explicit crash-budget override `wsx daemon recover`; graceful shutdown recreates saved session commands on the next launch.
- Show sessions directly below worktrees and optional pane rows below multi-pane sessions; use state icons plus authoritative agent names and aggregate automatically detected TCP listeners into sessions and worktrees. Ordinary shells with a distinct PTY foreground job show a static light-green `●`, authoritative working agents pulse light-green `◎ ◉ ● ◉`, idle agents show yellow `○`, blocked agents show red `◐`, and completed agents show green `✓` until explicit interaction acknowledges that report revision, without conflating process activity and agent lifecycle. App notices now share a configurable positive `notification_timeout_seconds` value, including error notices, while runtime-health banners remain state-driven.
- Add a responsive categorized global settings view with typed toggle, single-choice, validated keybinding, number, text, and editable multi-list controls. Configure the terminal prefix through separate modifier and key rows, footer release visibility, a compact or expanded desktop Terminal sidebar, and session port visibility as hidden, non-agentic only (default), or all while keeping branch-detail ports authoritative and visible. ([`5531b58`](https://github.com/vlwkaos/wsx/commit/5531b58b122919f1dbcca946c2007a443c7a006a), [`04eb201`](https://github.com/vlwkaos/wsx/commit/04eb201578dadb994fea26bcae003f39a80d529f))
- Add default-on macOS wake mode with a footer `☕` indicator. Generation-authorized Working reports hold continuously renewed, ten-minute `caffeinate` assertions while adapter heartbeats, a 30-minute freshness lease, fixed command arguments, retry throttling, and child reaping prevent stale or orphaned assertions from running indefinitely.
- Collapse the desktop Terminal sidebar to a two-column mirrored status rail by default, preserving authoritative session-state glyphs, row selection, scrolling, click targets, and full-width mobile Terminal behavior; retain the previous 32-column tree through `terminal_sidebar = "expanded"`, and toggle modes for the current TUI run with `Prefix+B` without rewriting config.
- Switch directly between current-worktree sibling sessions with `Prefix+k`/`Prefix+Up` and `Prefix+j`/`Prefix+Down`, or iterate active-group attention sessions with `Prefix+N` and `Prefix+n`, while preserving the exact resized-baseline gate before target input. Keep lowercase, case-accurate prefix commands visible in the Terminal footer, accent the prefix hint while the sequence is pending, and expose Escape cancellation without forwarding the sequence. ([`fb9a125`](https://github.com/vlwkaos/wsx/commit/fb9a125effbf9f388cdc35dda0e2cb8d53cc82e1))
- Add multi-membership project Groups with one optional Workspace/CLI filter, multi-toggle assignment, a virtual lowercase `ungrouped` view, a canonical header/content/footer layout with one persistent full-width horizontally scrollable chip header and no Workspace spacer, a left-sidebar scrollbar, a stable full focus frame around Workspace navigation, and green push-ready Git status.
- Keep exactly one Workspace group active, restore the last valid selection from an independently written cache, and use the first virtual **ungrouped** anti-group when selection data is absent, malformed, renamed, or deleted. Remove the no-filter **all** control, continue discarding historical selector fields, and prevent unrelated stale-client cache writes from publishing old in-memory group intent. Derive a readable `stale` project state from persisted authoritative agent-working and terminal-entry activity, automatically collapse stale expanded projects after a configurable inactivity period, and treat explicit expansion as a process-local freshness override. ([`8028019`](https://github.com/vlwkaos/wsx/commit/8028019505f7914c1d2d7e3cbb52dbf15b727c3a))
- Add canonical bounded `wsx.config.yml` project settings, edit-time schema templates, atomic `.gtrconfig` migration without deleting the legacy file, Workspace shortcuts for project and global config editing, and default-on `resume_agents_on_restore` policy with a fail-closed opt-out.
- Show Git submodules in a dedicated worktree-preview section with local parent-gitlink, initialization, conflict, modified-content, and untracked-content status. Add explicit validated `git.subtrees` project configuration so subtree changes are separated from ordinary local files without unreliable history inference. ([`0ea2ce8`](https://github.com/vlwkaos/wsx/commit/0ea2ce86acdb499be943c5b0dedaf2ee0ed41ed7))
- Package adjacent universal `wsx` and `wsxd` executables and provide dependency-free `cargo xtask run` and `cargo xtask build` workflows. ([`82e36a9`](https://github.com/vlwkaos/wsx/commit/82e36a9c995579921a799faf4c82459091b17e33), [`2316d31`](https://github.com/vlwkaos/wsx/commit/2316d31ceca678d6ce9cdc1fcf2b10af2a1e537a))
- Add pinned Nextest 0.9.143 and strict Clippy gates on Linux and macOS CI, with doctests and the production-equivalent runtime smoke kept as explicit steps.

### Bug Fixes

- Keep the published `wsx-core` crate self-contained by packaging the canonical agent-integration assets it embeds.
- Make the first Workspace `x` press on an unacknowledged done session acknowledge that exact revision and remove it from attention iteration; later presses retain local mute toggling. ([`b8694cd`](https://github.com/vlwkaos/wsx/commit/b8694cd79e9eeaa4f6568e90ced826bc54817f52))
- Tail-crop observational Workspace terminal previews when authoritative frames are taller than the panel, top-align shorter frames for natural shell placement, and keep Workspace visible after subscription acknowledgment until an exact resized full baseline activates Terminal.
- Persist TUI session reordering through daemon refresh and restart with stable identities, optimistic revisions, same-worktree validation, and transactional state saves.
- Forward bounded standard text clipboard writes from attached terminal applications to the outer terminal, restoring Pygmalion drag-copy without replaying stale writes after reconnect.
- Attribute TCP listeners launched in descendant process groups to their unique wsx PTY, show the actual port numbers that fit without a leading dot, and include the compact project/worktree/session/pane target in terminal-stream errors. ([`3930bad`](https://github.com/vlwkaos/wsx/commit/3930badf62f76ce35cb813dbd9db6820d3e64d36))
- Serialize daemon probe, recovery, replacement, spawn, and readiness with an owner-only coordinator lock while retaining the daemon singleton fence. Automatically recover a genuinely missing daemon with a shared three-start-per-minute crash budget, preserve authenticated same-login daemons that hang or return unsafe data, and keep intentional stops from being undone by background clients. Lifecycle-capable daemons expose binary identity and quiescence across protocol mismatch, defer replacement until zero live runtimes, then stop themselves; compatible pre-lifecycle daemons remain usable until their next natural stop and incompatible pre-lifecycle daemons remain protected. On macOS, authenticate the connected daemon's UID and audit session from its kernel peer token, then automatically restart a daemon inherited from an earlier login so new PTYs regain the current Keychain and trust context. ([`86dccca`](https://github.com/vlwkaos/wsx/commit/86dccca05ac8cd73f81e44126c889f2ad99d4db6))
- Preserve stable session, pane, terminal, and known-agent identity across daemon restart; resume valid unique integration-reported conversations with direct vendor argv for all 17 supported agents, safely fall back for invalid or duplicate references, and retain failed panes as exited. Use persisted agent identity only to plan resume, require a generation-matched adapter report to restore live projection, and rotate that authority before the resume supervisor opens a fallback shell. Delayed reports therefore cannot relabel a replacement runtime; 0.20 clients reject a same-protocol daemon that does not advertise shell fallback without stopping it automatically. ([`7e40633`](https://github.com/vlwkaos/wsx/commit/7e406335ceb8e1645d9a655588bf3e35f7f4c371), [`80c5a7b`](https://github.com/vlwkaos/wsx/commit/80c5a7bac72732b6c873d6bb598a5275f1ea13cf), [`ecd5356`](https://github.com/vlwkaos/wsx/commit/ecd535646d0ab82d0a6f7b07d29b864ea60565b6))
- Harden daemon lifecycle boundaries with SIGHUP/SIGTERM cleanup, secure primary and last-known-good state files, malformed-primary quarantine, file and directory sync, save-before-publish user mutations, truthful retrying runtime observations, deduplicated aggregate-bounded terminal views, one-shot process-group teardown, generation-scoped stream leases and agent mutations, and coordinator-backed snapshot-first terminal reconnection after suspend/resume.
- Keep PTY spawn, terminal I/O, process termination, and plugin callbacks outside daemon-state locks.
- Keep accepted terminal surfaces in one epoch-, pane-, and terminal-keyed TUI projection, independent of workspace metadata revisions, so agent reports cannot blank live terminals while daemon restart and terminal replacement still discard stale content.
- Retain existing asynchronous Git/worktree operations, exact deletion tombstones, routines, group selections, cache state, and responsive TUI behavior during the runtime migration.
- Refresh local Git status for the selected worktree within about one second and sweep remaining worktrees every 15 seconds, without changing the independent remote-fetch interval or failure backoff. ([`aeced30`](https://github.com/vlwkaos/wsx/commit/aeced3096a73b39f894280825da07e32363865eb))
- Replace per-key request/handshake round trips and 500 ms full-frame polling with a persistent duplex terminal stream, bounded asynchronous input, event-driven Ghostty dirty-row updates, and an 8 ms foreground wake and producer cadence. Add a production-equivalent direct-PTY versus wsxd-stream-to-wsx-render latency gate that reports narrow, wide, and erase-rewrite p50/p95 values and enforces a 16.7 ms added-p95 budget. Full runtime smoke requires two independent latency passes so transient shared-runner scheduling does not override deterministic performance evidence.
- Preserve Ghostty wide-cell and spacer occupancy through compact wire cells and ratatui projection so erase and wide-to-narrow redraws invalidate the correct columns.
- Buffer large terminal-frame reads, resize during subscription before the first baseline, and suppress intermediate frames while child synchronized-output mode is active.
- Keep saturated terminal update queues interruptible during shutdown, atomically sample synchronized-output state with frame revisions, preserve up to 64 accepted bounded clipboard writes in FIFO order outside frame cadence and reject overflow without overwriting accepted effects, and recheck stream wake predicates under the daemon mutex so output notifications cannot be stranded until the fallback timeout.
- Make `Ctrl+A`, then `W` the default Terminal-to-Workspace focus sequence; accept `W` while the prefix modifier remains held, reserve prefix then `Q` for TUI-only quit, double the prefix to send literal `Ctrl+A`, and preserve unprefixed `q` and other suffixes as terminal input.
- Keep the bottom status bar to one context-aware line with parenthesized key hints, add consistent `(i/I)idle`, `(a/A)active`, and `(n/N)attention` iteration plus capability-aware routine hints, use semantic mode-badge color families, keep the running version right-aligned, query the bounded GitHub latest-release endpoint at startup, expand it to `v{current} ↑ v{latest}` when a newer release exists, keep quit in the right-side global hints with `(q)uit` in Workspace, format multi-chord actions as `(Ctrl+A W)workspace`, place popup-specific controls on bottom border titles, keep unprefixed Terminal `q` available to applications, remove TUI send-text while preserving CLI automation, restore the one-row terminal breadcrumb, let terminal content fill the remaining right panel, and treat a left-panel click as one Workspace-mode selection action.
- Route bounded chrome backgrounds through semantic theme roles, keep primary surfaces and default terminal backgrounds transparent while preserving explicit ANSI cell backgrounds, and derive sidebar rendering and mouse hit-testing from one geometry contract.
- Size the Workspace scrollbar from rendered rows, scroll the tree by mouse wheel, right-align session listeners, limit idle-session navigation to agent sessions, and keep agent labels neutral while restoring semantic state colors.
- Route terminal wheel input through authoritative Ghostty modes, wake frame streaming immediately after local viewport changes, and remove stale pre-update and duplicate-clear redraws that made scroll bursts lag until later PTY input.
- Render the TUI's config-backed workspace immediately while daemon readiness and one-pass Git discovery continue in the background; remove duplicate startup worktree scans.
- Move compact notifications above the bottom-right status line and suppress obvious Terminal/Workspace mode-change notifications.
- Wait for runtime-smoke daemon socket removal on normal and failure cleanup so repeated tests cannot leave orphan daemons.

### Maintenance

- Import the pinned asched boundary, the reference-only Herdr source snapshot, and the vendored Ghostty and portable-pty runtime sources while keeping Herdr excluded from the workspace and release artifacts. ([`3e1562c`](https://github.com/vlwkaos/wsx/commit/3e1562c31e39c6da579ecb0902e45656aae5cd0d), [`ea65d8c`](https://github.com/vlwkaos/wsx/commit/ea65d8c10679f19cb9edf2855387b93f3512fe6f), [`a55b860`](https://github.com/vlwkaos/wsx/commit/a55b8607f211d5553a2feeb0bc8cdf349e01d69a), [`f0b3298`](https://github.com/vlwkaos/wsx/commit/f0b329850ca1a7ead53ee9ba747694189e3984f4))
- Synchronize English and Korean user documentation plus recurring maintainer invariants with the final runtime, Workspace, and Terminal behavior. ([`afd6b4d`](https://github.com/vlwkaos/wsx/commit/afd6b4dc77da2c9e744c5f9cefc5e48e721574f3))

## [0.17.0] - 2026-08-15

### Breaking Changes

- Move project scheduling to the separately installed asched 0.2 daemon. wsx no longer embeds, starts, stops, or exposes the former routine daemon API; register canonical project paths with `asched project add` before managing their routines. The initial unreleased embedded scheduler and its lifecycle, IPC, persistence, execution, and security hardening were superseded before publication by the shared `asched-core` boundary. ([`3f977a9`](https://github.com/vlwkaos/wsx/commit/3f977a976ef97bf5be0d316b931d7ebd04869783), [`782d863`](https://github.com/vlwkaos/wsx/commit/782d863da95682bf4254bf9a662bf4286f38df71), [`e6a564e`](https://github.com/vlwkaos/wsx/commit/e6a564e3181b52516b3ac030af104c2f0dea64dc), [`b60d107`](https://github.com/vlwkaos/wsx/commit/b60d1071edb6a3a6b9571ba91a5018d82835d4f9))

### Features

- Add project-scoped cron and provider-neutral event routines through asched, including optimistic revisions, enable/disable state, manual run and cancellation, retained logs, event deduplication/no-match results, and background TUI requests. The project-level collection is rendered as `sched`, with routine rows aligned to sessions. New routines use an explicit Codex, Claude, Pi, or Custom runner picker instead of hidden function-key presets, while agents can create the same validated direct-argv routines through the CLI. ([`3cbc525`](https://github.com/vlwkaos/wsx/commit/3cbc52518370727b573a5d6e310440c2d93d39fb), [`b60d107`](https://github.com/vlwkaos/wsx/commit/b60d1071edb6a3a6b9571ba91a5018d82835d4f9))
- Recognize Pi coding-agent processes, including Pi's Node launcher tree, and derive provider-neutral agent activity from bounded semantic pane-tail motion. Settled agents become yellow attention targets after three seconds, runtime and interactive processes remain active, and capture failures fall back to tmux activity. ([`b60d107`](https://github.com/vlwkaos/wsx/commit/b60d1071edb6a3a6b9571ba91a5018d82835d4f9))
- Apply `extended-keys on`, CSI-u extended-key formatting, and `tmux-256color` as wsx tmux server defaults. ([`b60d107`](https://github.com/vlwkaos/wsx/commit/b60d1071edb6a3a6b9571ba91a5018d82835d4f9))

### Bug Fixes

- Keep intentionally unregistered projects out of routine refresh requests so asched's scheduling allowlist does not make the entire TUI report routines unavailable; retain typed conflict, protocol-mismatch, and already-running diagnostics for registered projects. ([`b60d107`](https://github.com/vlwkaos/wsx/commit/b60d1071edb6a3a6b9571ba91a5018d82835d4f9))
- Avoid full-terminal clears when navigating project, worktree, and routine previews, eliminating visible flicker while preserving ghost-cell cleanup around captured session previews. ([`b60d107`](https://github.com/vlwkaos/wsx/commit/b60d1071edb6a3a6b9571ba91a5018d82835d4f9))
- Abort conflicts from the default internal `git pull --rebase` path instead of leaving worktrees mid-operation, and keep deleted worktrees hidden until a live Git refresh confirms removal. ([`74584b0`](https://github.com/vlwkaos/wsx/commit/74584b08a9df0a447b8d9e10fecc0091e16330ea))

### Maintenance

- Record workspace-release, review-tool, warning-baseline, and issue-progress guidance accumulated while the unreleased routine implementation was developed and audited. ([`d243e82`](https://github.com/vlwkaos/wsx/commit/d243e824c5777ecdd3bc8cd6cc07dd550d12ad9a), [`623a8fa`](https://github.com/vlwkaos/wsx/commit/623a8fa6096ebe8e253701c2355db615d165211d), [`82d6ca9`](https://github.com/vlwkaos/wsx/commit/82d6ca939a6092496a404c0b98084eac662eee24), [`4d6313e`](https://github.com/vlwkaos/wsx/commit/4d6313e4e5d190f5015576fa8a80d95fc8b34163))

## [0.16.2] - 2026-06-25

### Bug Fixes

- Adding a project that is already registered is now rejected instead of creating a duplicate tree entry. Re-adding a project after removing it (or re-adding with a trailing slash) previously appended a second item because the workspace was not deduped and trailing-slash paths bypassed the config dedup. Project paths now run through a single normalizer before registration, and duplicates are refused with a status message. ([`15d2427`](https://github.com/vlwkaos/wsx/commit/15d2427))
- Keep git status visible during explicit git operations instead of blanking it on refresh, and pick the newest crash-restore session source so a stale snapshot no longer overrides a more recent cache. ([`15d2427`](https://github.com/vlwkaos/wsx/commit/15d2427))

## [0.16.1] - 2026-06-16

### Bug Fixes

- Recompute tab visibility whenever the workspace tree is rebuilt, so unregistering a project from one tab no longer lets a shifted project from another tab appear in the active tab.

## [0.16.0] - 2026-06-06

### Refactor

- Split the codebase into a Cargo workspace with two crates: `wsx` (TUI/CLI binary) and `wsx-core` (ratatui-free library). External orchestrators can now depend on `wsx-core` directly for worktree, tmux, git, hooks, config, and model primitives. All moves used `git mv` so file history is preserved. The binary's behaviour is unchanged. ([`5898383`](https://github.com/vlwkaos/wsx/commit/5898383))
- `git_fetch` in `wsx-core/src/git/info.rs` promoted from `pub(crate)` to `pub` so the binary can reach it across the new crate boundary.

## [0.15.11] - 2026-05-26

### Features

- Add-project repo cache survives modal opens. The git-repo walker now runs at app start and continues in the background between modal opens, so re-opening "add project" is instant and newly created repos appear automatically without restarting wsx. ([`decdb63`](https://github.com/vlwkaos/wsx/commit/decdb63))
- Add-project scan depth raised from 6 to 8 so deeper layouts like `~/work/<org>/<year>/<project>` are now discovered. Walks still short-circuit at any `.git`, so this only widens trees that have no repos.

### Bug Fixes

- Add-project completion falls back to filesystem path mode when the input contains `/` or starts with `~`, so paths the background walker can't reach (outside `$HOME`, under `node_modules` / `target`, beyond the depth limit) are now reachable by typing them directly. ([`b73a529`](https://github.com/vlwkaos/wsx/commit/b73a529))
- The "scanning..." indicator no longer sticks around once you start typing; combined with the persistent cache it should rarely appear at all now.

## [0.15.10] - 2026-05-25

### Bug Fixes

- Reworks the v0.15.9 session-state model that rendered "everything green" in real usage. The Active green state now requires either recent output (for agents) or a long-running process kind (Runtime / InteractiveApp); a quiet long-running agent renders **yellow** (AgentDone, "see the result") instead of green. ([`32c044e`](https://github.com/vlwkaos/wsx/commit/32c044e))
- Foreground classification now walks the process tree under each pane's PID via one `ps -ax` snapshot per refresh. Previously tmux's `pane_current_command` reported the deepest spawned child, so `claude` / `codex` showed up as a node subprocess named `2.1.x` and fell through to InteractiveApp. The agent is now detected correctly even when nested under shells or subprocesses.
- Shell foreground now distinguishes "prompt just returned" (yellow ●, `ShellPrompt`) from "long idle" (gray ○, `ShellIdle`). Recent activity in a shell signals "do the next thing", not "active app".
- Mute is sticky: pane output no longer auto-unmutes a session (this reverts the 0.15.5 behavior, which fought against the user's intent on noisy sessions). Mute is cleared only when the user interacts with the session in wsx — attach, send command, send Ctrl-C, or rename.

### Refactor

- New `src/proc_tree.rs` module wraps a single `ps` snapshot per refresh and exposes `descendants(pane_pid)` for foreground classification. Single-source-of-truth: derive only walks the tree.
- Added a realistic-workspace fixture test (`session_state::tests::given_realistic_workspace_when_classified_then_each_session_state_matches_spec`) as a permanent regression guard. It pins the per-session outcome for a 10-session fixture covering the full taxonomy; the v0.15.9 "running → Active" rule would have produced Active≈6 instead of 4 and failed this test before release.

## [0.15.9] - 2026-05-23

### Refactor

- Session state collapsed into a single deriver. `SessionInfo` carries only raw inputs (bell, foreground, pane_capture, muted) and every UI/CLI consumer reads the same 3-state projection through `session_state::derive` ([`6a452c4`](https://github.com/vlwkaos/wsx/commit/6a452c445db4dd6c68356c4540c0980038c2555f))
- **Behavior change**: a session running a foreground process (agent, runtime, interactive app) now renders **green** Active, including when quiet. This reverses the 0.15.7 behavior that forced background runtimes (`node`, `bun`, `npm`, ...) to yellow. Under the new 3-state model green means "a process is running"; yellow is reserved for the tmux bell or a detected interactive prompt
- A bare shell is always Idle (gray); recent typing alone no longer flips a shell to Active
- An `[y/n]` confirm or `waiting for user` prompt detected in the captured pane text escalates to NeedsAttention (yellow) regardless of which foreground kind is running. Previously detected but inert

### Bug Fixes

- Project move (`m` + `j`/`k`) now jumps across projects hidden by the tab filter instead of silently swapping with an invisible neighbor ([`74bbef5`](https://github.com/vlwkaos/wsx/commit/74bbef58710b521be78472ab144ae25b3c77705f))
- Multi-window foreground classification is now order-independent. A session with `node` then `claude` across different windows is recognized as Agent, not Runtime
- `wsx status --json` drops the `has_running_app` field (it was a stored mirror of `foreground.is_running()`); use the `foreground` field instead

## [0.15.8] - 2026-05-14

### Bug Fixes

- Mobile mode no longer triggers preview-driven pane capture and forced full redraws while the preview is hidden, which reduces active-session flicker in xtermjs and other portrait SSH terminals
- The preview pane now clears before redraw, which prevents wrapped session output from leaving stale cells behind when line layout changes

## [0.15.7] - 2026-05-05

### Bug Fixes

- Sessions with background runtimes (`node`, `bun`, `deno`, `npm`, `pnpm`, `yarn`, `npx`, `dotenvx`, `watchexec`, `entr`, `reflex`) now show yellow instead of always-green — activity timestamp was being refreshed on every poll for these processes, making them perpetually "active" regardless of actual terminal use

## [0.15.5] - 2026-04-29

### Features

- `--mobile` flag: collapses preview panel for portrait SSH sessions, shows compact key hints ([`52080a5`](https://github.com/vlwkaos/wsx/commit/52080a5e9703427302245551df7aa67f5b5ec968))
- `mobile_detach_key` config option: bind a no-prefix tmux key to detach-client when `--mobile` is active, e.g. `mobile_detach_key = "C-q"` ([`52080a5`](https://github.com/vlwkaos/wsx/commit/52080a5e9703427302245551df7aa67f5b5ec968))

### Bug Fixes

- Muted sessions now auto-unmute when new output streams to the pane — mute is "suppress for now", not permanent ([`482c6d9`](https://github.com/vlwkaos/wsx/commit/482c6d9cf05285c150f135d2afd18e05ee66fb07))

## [0.15.4] - 2026-04-24

### Features

- `focus-events on` applied per session on attach — terminals and editors receive focus in/out signals from tmux ([`700eebd`](https://github.com/vlwkaos/wsx/commit/700eebdab4c12bcf008ba38de5a79206052fcd35))
- `extended-keys on` set globally at startup instead of per-attach — correctly enables kitty keyboard protocol at the server level ([`700eebd`](https://github.com/vlwkaos/wsx/commit/700eebdab4c12bcf008ba38de5a79206052fcd35))

### Bug Fixes

- Project search no longer shows "scanning..." indefinitely — repos now stream to the picker as they are found instead of waiting for the full scan to complete ([`700eebd`](https://github.com/vlwkaos/wsx/commit/700eebdab4c12bcf008ba38de5a79206052fcd35))

## [0.15.3] - 2026-04-21

### Features

- `extended-keys on` applied to every session on attach — enables kitty keyboard protocol passthrough for richer key event support in modern terminals ([`9fa8054`](https://github.com/vlwkaos/wsx/commit/9fa8054e60da3222cf23c254eaeb0221d771c5bb))

## [0.15.2] - 2026-04-21

### Bug Fixes

- Projects deleted from outside wsx no longer persist as stale entries until restart — on the first refresh after deletion the project is shown as `(missing)` in gray; on the second refresh (~6 s later) it is removed from the tree

### UI

- Externally deleted projects display a `(missing)` indicator in the tree so users know why interactions fail during the brief window before removal

## [0.15.1] - 2026-04-17

### Bug Fixes

- Previewing a session running wsx no longer causes an infinite render loop — `pane_current_command` is tracked per session via the activity monitor and capture is suppressed when wsx is the foreground process; sentinel character (⅋ U+214B) in the status bar provides a fallback for renamed binaries

### UI

- Update available badge now reads `↑ update available: v{N}` instead of just `↑ v{N}`

---

## [0.15.0] - 2026-04-17

### Bug Fixes

- Session kill/rename no longer reverts when a stale background refresh arrives mid-operation — pending session ops are filtered/remapped so user intent wins over in-flight tmux snapshots ([`db50e94`](https://github.com/vlwkaos/wsx/commit/db50e94d0dbad0561040b387e920ce1d29acde1a))
- Session rename now persists to cache immediately instead of waiting for the next periodic refresh ([`db50e94`](https://github.com/vlwkaos/wsx/commit/db50e94d0dbad0561040b387e920ce1d29acde1a))
- Idle time counter updates every ~1 second instead of only on state changes ([`db50e94`](https://github.com/vlwkaos/wsx/commit/db50e94d0dbad0561040b387e920ce1d29acde1a))

### Performance

- git fetch deduped across concurrent wsx instances via per-worktree advisory lockfile; skipped fetches report success so backoff stays low ([`db50e94`](https://github.com/vlwkaos/wsx/commit/db50e94d0dbad0561040b387e920ce1d29acde1a))
- slow_timer staggered by PID-derived jitter (0–499ms) so concurrent instances don't hammer tmux/git simultaneously ([`db50e94`](https://github.com/vlwkaos/wsx/commit/db50e94d0dbad0561040b387e920ce1d29acde1a))

### Improvements

- Muted and suppressed session flags stored as tmux user options (`@wsx-muted`, `@wsx-suppressed`) — shared across all concurrent instances without cache coordination; one-time migration from old cache format on startup ([`db50e94`](https://github.com/vlwkaos/wsx/commit/db50e94d0dbad0561040b387e920ce1d29acde1a))

---

## [0.14.7] - 2026-04-10

### Performance

- Eliminate main-thread blocking on worktree delete, clean, session kill, and create ops — synchronous subprocess calls replaced with optimistic UI removal and async background execution via existing `spawn_bg`/`spawn_tmux_refresh` infrastructure ([`52bf27e`](https://github.com/vlwkaos/wsx/commit/52bf27e2cedeaf5a79c6612539e4fe97852572c9))

### Bug Fixes

- Preview ghost cells on tab switch — `{`/`}` navigation did not trigger `force_redraw`, leaving stale session preview fragments when switching tabs ([`cd8051e`](https://github.com/vlwkaos/wsx/commit/cd8051e50cd0c79d6d40d73f819ea29bc0c727a4))

---

## [0.14.6] - 2026-04-06

### Bug Fixes

- Preview ghost cells on navigation — PUA-width-shifted cells from session capture persisted when switching to project/worktree preview; move `force_redraw` into `update_scroll` so every navigation triggers a full terminal clear ([`b7f1500`](https://github.com/vlwkaos/wsx/commit/b7f15009648c54ef0615e2f58b217c71f1f1af29))
- Bell sessions skipped by attention navigation — `n/N` (next/prev pending) missed sessions with `has_activity` (bell) when they were also recently active; bell now triggers attention regardless of active state

---

## [0.14.5] - 2026-04-04

### Features

- `wsx tab` subcommand — `ls`, `create`, `rename`, `own <tab> <project>` for managing tab assignments from the CLI; `own default <project>` unassigns ([`01e4123`](https://github.com/vlwkaos/wsx/commit/01e41232a4f925fa64ecdd8333c30c2270e0a322))
- `wsx status` normal output prefixes each project with `[tab]` when tabs are configured

---

## [0.14.4] - 2026-04-03

### Features

- `--tab <name>` filter for `wsx status`, `wsx worktree list`, and `wsx session list`; `--tab default` matches projects with no tab assigned ([`a89dcba`](https://github.com/vlwkaos/wsx/commit/a89dcbac2fc847523ed358df91a7cbb24b4138dc))

---

## [0.14.3] - 2026-04-01

### Bug Fixes

- Status bar corruption after navigation — `terminal.clear()` resets only the back buffer, leaving stale cell values in the front buffer; ratatui's diff skips cells that match the old content even though the screen was cleared, producing blank gaps in the hint text (e.g. `(e)config` rendered as ` e config`); fix draws an empty frame after `clear()` to flush the front buffer before the real frame ([`07c1aa0`](https://github.com/vlwkaos/wsx/commit/07c1aa0))

---

## [0.14.2] - 2026-04-01

### Features

- `wsx session peek` — new subcommand replacing `capture`; adds `-n <lines>` scrollback depth, `-o <offset>` to skip lines from bottom, and `-a` to strip ANSI/box-drawing for agent/LLM consumption ([`2730058`](https://github.com/vlwkaos/wsx/commit/2730058))
- Session snapshot (`~/.config/wsx/sessions.toml`) — written on every state change and on quit; survives tmux SIGBUS/crash so sessions are restored even when the cache never flushed ([`9d179da`](https://github.com/vlwkaos/wsx/commit/9d179da))

### Bug Fixes

- Terminal resize and sleep/wake layout corruption — `Event::Resize` was silently discarded, so ratatui repainted over a stale buffer after tmux resized the window; now triggers `terminal.clear()` ([`ae235ba`](https://github.com/vlwkaos/wsx/commit/ae235ba))
- Session snapshot stale on normal quit — `flush_cache()` on the quit path never wrote `sessions.toml`; snapshot could lag by up to 3s on clean exit ([`7f7255b`](https://github.com/vlwkaos/wsx/commit/7f7255b))
- Deleted sessions phantom-restored after crash — `do_delete_session` removed the session from the workspace model but never set `cache_dirty`, so the snapshot still listed it and restore recreated it ([`7f7255b`](https://github.com/vlwkaos/wsx/commit/7f7255b))

---

## [0.14.1] - 2026-03-26

### Bug Fixes

- Fix worktree delete flicker — optimistic removal raced with the 3s timer refresh which read stale git worktree list and briefly re-added the deleted entry; guard with `pending_deletions` set filtered from both `apply_tmux_refresh` and `refresh_all` ([`94a78a0`](https://github.com/vlwkaos/wsx/commit/94a78a0))

---

## [0.14.0] - 2026-03-26

### Features

- `i` / `I` keys jump to next/prev idle session (○ gray state), consistent with `a`/`A` for active and `n`/`N` for attention ([`6af341c`](https://github.com/vlwkaos/wsx/commit/6af341c))
- CLI `-f compact` / `--format compact` flag on `status`, `worktree list`, `session list` — labeled fixed-width tabular output with worktree as row unit and sessions inlined as `name[state]`; designed for AI agent stdout consumption with fewer tokens ([`de38f0f`](https://github.com/vlwkaos/wsx/commit/de38f0f))

---

## [0.13.0] - 2026-03-25

### Features

- Add project now uses recursive fuzzy search over git repos instead of directory-by-directory navigation — type a partial name (e.g. `wsx`) to find `~/code/wsx/` directly; scan runs in background with incremental results ([`9c67e15`](https://github.com/vlwkaos/wsx/commit/9c67e15))

---

## [0.12.1] - 2026-03-24

### Features

- `session send-keys` CLI gains `--no-enter` flag — sends keys to a pane without appending Enter, enabling single-key navigation (e.g. selecting numbered Claude suggestions) ([`cfe89c2`](https://github.com/vlwkaos/wsx/commit/cfe89c2))

---

## [0.12.0] - 2026-03-24

### Features

- Restore tmux sessions after server restart — on startup, compares cached tmux server PID to the current one; if they differ (reboot, crash, kill-server), silently recreates all cached sessions in their worktree directories ([`00a531f`](https://github.com/vlwkaos/wsx/commit/00a531f))
- Headless CLI subcommands for agent/scripting use ([`2ff04f5`](https://github.com/vlwkaos/wsx/commit/2ff04f5))

---

## [0.11.3] - 2026-03-20

### Bug Fixes

- Inactive tab labels now use the terminal default foreground instead of `DarkGray` — readable on dark themes ([`2aa4a74`](https://github.com/vlwkaos/wsx/commit/2aa4a74))

---

## [0.11.2] - 2026-03-19

### Bug Fixes

- Worktree no longer flashes on screen for 1+ frames after confirming delete — optimistically remove from model before spawning async cleanup ([`c75ac03`](https://github.com/vlwkaos/wsx/commit/c75ac03))

### Docs

- Add tab grouping to guide and key reference ([`1a83a89`](https://github.com/vlwkaos/wsx/commit/1a83a89))
- Restructure README — install first, collapsible usage/config sections ([`7d97524`](https://github.com/vlwkaos/wsx/commit/7d97524))

---

## [0.11.1] - 2026-03-19

### Other

- Homebrew formula: replace `--version` test with `assert_predicate :executable?` — wsx is a TUI with no CLI flags
- Release binary built with `MACOSX_DEPLOYMENT_TARGET=11.0` (arm64) — sets explicit `minos` instead of inheriting from build SDK

---

## [0.11.0] - 2026-03-19

### Features

- Tab grouping: filter projects into named tabs with `{`/`}` to cycle, `T` to manage (add/rename/delete/reorder), `m`+`h`/`l` to move a project between tabs ([`85f6428`](https://github.com/vlwkaos/wsx/commit/85f6428))
- Compact tab bar in the workspace block title — active tab shown full with highlight, inactive tabs truncated to 2 chars ([`bcb5ad4`](https://github.com/vlwkaos/wsx/commit/bcb5ad4))

---

## [0.10.1] - 2026-03-11

### Bug Fixes

- Replace `Mutex::lock().unwrap()` with `unwrap_or_else(|e| e.into_inner())` in `GitSemaphore` — prevents panic cascade if a git-info thread panics while holding the lock ([`126cfc5`](https://github.com/vlwkaos/wsx/commit/126cfc5))
- Config TOML parse errors no longer exit before TUI starts — falls back to defaults and shows a 10-second warning in the status bar ([`126cfc5`](https://github.com/vlwkaos/wsx/commit/126cfc5))
- Cache save failures are now surfaced as TUI status messages instead of `eprintln` to hidden stderr ([`126cfc5`](https://github.com/vlwkaos/wsx/commit/126cfc5))
- Add file path context to config save IO errors ([`126cfc5`](https://github.com/vlwkaos/wsx/commit/126cfc5))

---

## [0.10.0] - 2026-03-10

### Features

- Check crates.io for a newer version once at startup; highlight the bottom-right version badge in yellow with an up-arrow when an update is available ([`9783f26`](https://github.com/vlwkaos/wsx/commit/9783f26))
- Add `A`/`Shift+Tab` for backward navigation through active sessions and suggestions ([`d3e374b`](https://github.com/vlwkaos/wsx/commit/d3e374b))

### Bug Fixes

- Restore powerline glyphs in preview — stop replacing PUA chars (U+E000–U+F8FF) with spaces; instead force a full terminal clear on every capture content change to prevent bleed ([`6e7514a`](https://github.com/vlwkaos/wsx/commit/6e7514a))

### UI

- Redesign input suggestion list as an inline dropdown instead of a floating overlay ([`48b9094`](https://github.com/vlwkaos/wsx/commit/48b9094))

---

## [0.9.9] - 2026-03-09

### Bug Fixes

- Subprocess process-group isolation — child processes get their own PGID via `process_group(0)`, so `killpg` on timeout reaps git + ssh + credential helpers instead of leaking orphans ([`d1dd486`](https://github.com/vlwkaos/wsx/commit/d1dd486))
- Add `ConnectTimeout=5` to `GIT_SSH_COMMAND` so SSH fails fast for unreachable hosts instead of hanging 60s+ ([`d1dd486`](https://github.com/vlwkaos/wsx/commit/d1dd486))
- Join reader threads after kill in `output_with_timeout` to prevent thread leaks on the timeout path ([`d1dd486`](https://github.com/vlwkaos/wsx/commit/d1dd486))
- Consolidate `git_fetch` onto `output_with_timeout` — removes duplicate timeout loop and blocking `stderr_thread.join()` that hung when SSH survived kill ([`d1dd486`](https://github.com/vlwkaos/wsx/commit/d1dd486))
- Protect `current_branch`, `list_worktrees`, `recent_commits` with `output_with_timeout` — no more bare `.output()` that could block indefinitely ([`d1dd486`](https://github.com/vlwkaos/wsx/commit/d1dd486))

### Refactor

- Move periodic `list_worktrees` and tmux session refresh off the main thread — `spawn_tmux_refresh` now runs all subprocess calls in a background thread and sends results via channel ([`1862be0`](https://github.com/vlwkaos/wsx/commit/1862be0))
- Add `refresh_workspace_with_worktrees` that takes pre-computed worktree entries, keeping synchronous `refresh_workspace` for user-triggered actions only ([`1862be0`](https://github.com/vlwkaos/wsx/commit/1862be0))
- Fetch failure tracking with `FetchFailReason` (auth/timeout/network), per-worktree fail count, and exponential backoff ([`d1dd486`](https://github.com/vlwkaos/wsx/commit/d1dd486))

---

## [0.9.8] - 2026-03-07

### Bug Fixes

- Fix character bleed in preview panel on session navigation — replace PUA chars (U+E000–U+F8FF, powerline/Nerd Font symbols) with space to prevent unicode-width mismatch that caused ratatui diff to leave stale terminal cells ([`1bc83a2`](https://github.com/vlwkaos/wsx/commit/1bc83a2))
- Force full terminal clear inside synchronized update block on nav up/down so any stale content is overwritten atomically without flash ([`1bc83a2`](https://github.com/vlwkaos/wsx/commit/1bc83a2))
- Strip ANSI escapes before empty-line detection in `trim_capture`; pop trailing whitespace-only lines after ANSI parse ([`1bc83a2`](https://github.com/vlwkaos/wsx/commit/1bc83a2))

---

## [0.9.7] - 2026-03-06

### Bug Fixes

- Restore session icon 4-branch priority: active output shows green `◉`, running-but-quiet shows yellow `●` (regression from 0.9.6) ([`ef1c49f`](https://github.com/vlwkaos/wsx/commit/ef1c49f))
- Strip ANSI OSC sequences (hyperlinks, window titles) and bare control chars that caused display artifacts ([`25d97c7`](https://github.com/vlwkaos/wsx/commit/25d97c7))
- attention_candidates now cycles to both bell and running-app sessions; send_command_history deduplicates by moving to end ([`c118ea2`](https://github.com/vlwkaos/wsx/commit/c118ea2))

---

## [0.9.6] - 2026-03-06

### Refactor

- Merge bell alert and running-app-quiet into single "needs attention" state — both show yellow `●` ([`1e7754c`](https://github.com/vlwkaos/wsx/commit/1e7754c))
- Remove worktree-level activity rollup indicator (white `●`) ([`1e7754c`](https://github.com/vlwkaos/wsx/commit/1e7754c))
- Clear stale `has_activity`/`has_running_app` flags when session is absent from tmux ([`1e7754c`](https://github.com/vlwkaos/wsx/commit/1e7754c))
- Include bell alerts in attention candidates and dismiss action ([`1e7754c`](https://github.com/vlwkaos/wsx/commit/1e7754c))

### Docs

- Add Korean README ([`e8f1fc2`](https://github.com/vlwkaos/wsx/commit/e8f1fc2))

---

## [0.9.5] - 2026-03-05

### Bug Fixes

- Skip non-git directories when loading workspace ([`07b6556`](https://github.com/vlwkaos/wsx/commit/07b6556))
- Handle render, tick, and register errors gracefully instead of crashing the TUI ([`88c3ba9`](https://github.com/vlwkaos/wsx/commit/88c3ba9))

### Refactor

- Simplify error handling in event loop — use dispatch-level error catch instead of per-method match ([`f602b75`](https://github.com/vlwkaos/wsx/commit/f602b75))

---

## [0.9.4] - 2026-03-05

### Features

- Status notifications (spinner during processing, checkmark on completion) now overlay the bottom of the workspace tree column instead of the status bar ([`d7a84aa`](https://github.com/vlwkaos/wsx/commit/d7a84aa))

### Refactor

- Unified Tab/Up/Down completion navigation for both path and command history inputs — Tab cycles, Up/Down navigate the list, dropdown scrolls to keep selection visible ([`e48dd4d`](https://github.com/vlwkaos/wsx/commit/e48dd4d))
- Panic hook restores terminal before printing errors so the shell isn't left in raw mode ([`e48dd4d`](https://github.com/vlwkaos/wsx/commit/e48dd4d))

---

## [0.9.3] - 2026-03-05

### Features

- Send-command history persisted across restarts via workspace cache ([`69d403c`](https://github.com/vlwkaos/wsx/commit/69d403c))

### Performance

- Consolidate git-info from 5 subprocesses to 1 per worktree (`git status --porcelain=2 --branch`) with 15s TTL to skip redundant refreshes ([`93ae064`](https://github.com/vlwkaos/wsx/commit/93ae064))
- Cap concurrent git threads to CPU count via counting semaphore ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))
- Skip redraw on `Action::None` and unchanged activity ticks ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))
- Cache parsed ANSI pane previews — re-parse only when capture text changes ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))
- Remove blocking 2s startup wait — render immediately with cached data, fill git info async ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))
- O(1) worktree lookup for async result application via path index ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))
- Pre-indexed session/ordering lookups in `refresh_workspace` ([`93ae064`](https://github.com/vlwkaos/wsx/commit/93ae064))
- Cache writes gated by dirty flag; `sync_all` only on quit ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))

### Refactor

- `clean_merged` uses `HashSet` instead of `Vec::contains` ([`93ae064`](https://github.com/vlwkaos/wsx/commit/93ae064))
- Flatten tree passed into renderer instead of recomputing per frame ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))
- Status hints computed once per frame instead of twice ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))
- Search text cached alongside flat tree, rebuilt only on tree changes ([`ea196d3`](https://github.com/vlwkaos/wsx/commit/ea196d3))

---

## [0.9.2] - 2026-03-04

### Features

- Send Command (`S`) now shows history as a fuzzy-filtered suggestion dropdown while typing, matching the path autocomplete UX — Tab/Up/Down navigate entries ([`6081895`](https://github.com/vlwkaos/wsx/commit/6081895))
- Dropdown opens immediately on `S` showing recent commands (newest first), even before typing ([`6081895`](https://github.com/vlwkaos/wsx/commit/6081895))

---

## [0.9.1] - 2026-03-04

### Features

- Send Command (`S`) history is now persisted across restarts — commands survive wsx exit and are restored from cache on next launch ([`f8fe3f8`](https://github.com/vlwkaos/wsx/commit/f8fe3f8))

---

## [0.9.0] - 2026-03-04

### Features

- Background thread execution for clean, delete/create worktree, and all git ops (pull/push/rebase/merge) — UI stays responsive during long operations ([`d6638e6`](https://github.com/vlwkaos/wsx/commit/d6638e6))
- Braille spinner in the status bar right corner replaces the version badge while a job runs — key hints remain fully visible ([`d6638e6`](https://github.com/vlwkaos/wsx/commit/d6638e6))
- Up/Down arrow navigation in the Send Command (`S`) input recalls previously sent commands, capped at 50 entries ([`c629e8d`](https://github.com/vlwkaos/wsx/commit/c629e8d))

### Bug Fixes

- Batch clean (`c` on project or root) now kills associated tmux sessions for removed worktrees — previously only single-worktree clean did this ([`d6638e6`](https://github.com/vlwkaos/wsx/commit/d6638e6))

---

## [0.8.3] - 2026-03-03

### Features

- Detect when another wsx instance writes the cache — pauses writes and shows a popup; any key reloads expand states and cursor from the updated cache ([`a4c4793`](https://github.com/vlwkaos/wsx/commit/a4c4793))

### Bug Fixes

- Cursor no longer drifts across sessions — position is now persisted as a stable path identity (project/worktree/session path) rather than a raw flat-tree index ([`339713b`](https://github.com/vlwkaos/wsx/commit/339713b))
- Expand state for projects with trailing slashes in config paths (`notes/`, `app/`) now persists correctly — paths are normalized on load ([`339713b`](https://github.com/vlwkaos/wsx/commit/339713b))
- Cache save errors are now visible in the terminal — `flush_cache` runs after `tui::restore` instead of while the alternate screen is active ([`339713b`](https://github.com/vlwkaos/wsx/commit/339713b))

---

## [0.8.2] - 2026-03-01

### Bug Fixes

- Git status indicators (`↑N`/`↓N`/`*`) no longer flicker every 3s — redraws are skipped when git info is unchanged ([`11330c6`](https://github.com/vlwkaos/wsx/commit/11330c6))
- Git indicators stay visible after detaching from a session — previously cleared to blank while the refresh was in flight ([`2644f5a`](https://github.com/vlwkaos/wsx/commit/2644f5a))
- Initial git info load is now async, preventing the event loop from blocking on git CLI calls at first worktree selection ([`11330c6`](https://github.com/vlwkaos/wsx/commit/11330c6))
- Idle timer in session list now updates every 1s instead of every 2s ([`11330c6`](https://github.com/vlwkaos/wsx/commit/11330c6))
- Fix potential panic on non-ASCII branch names in git popup ([`36e96f9`](https://github.com/vlwkaos/wsx/commit/36e96f9))

---

## [0.8.1] - 2026-02-28

### Bug Fixes

- Refresh local diff indicator (`*`) every 3s for the selected worktree — previously cached until a fetch or session attach ([`5ce6565`](https://github.com/vlwkaos/wsx/commit/5ce6565))

### Other

- Add MIT license ([`2bb7529`](https://github.com/vlwkaos/wsx/commit/2bb7529))
- Add `cargo install wsx` to README install instructions ([`bf4d5ce`](https://github.com/vlwkaos/wsx/commit/bf4d5ce))

---

## [0.8.0] - 2026-02-28

### Features

- Add git popup (`g` key on a worktree) with pull, push, pull-rebase, merge-from, and merge-into operations; `p`/`P` run immediately, `r`/`m`/`M` prompt for a branch pre-filled with the project default ([`ab298d1`](https://github.com/vlwkaos/wsx/commit/ab298d1))

---

## [0.7.0] - 2026-02-27

### Features

- Add remote tracking state to worktree display — background `git fetch` per selected worktree (60s interval, 10s timeout), ahead/behind counts updated silently after fetch ([`ac4ce5e`](https://github.com/vlwkaos/wsx/commit/ac4ce5e))
- Show `↑N` / `↓N` / `↓N↑M` git state indicators in tree with colors (cyan/red/magenta); `*` for local changes replaces `✎`; `~` prefix marks the main worktree ([`8537ed8`](https://github.com/vlwkaos/wsx/commit/8537ed8))
- Reorganize worktree preview into Remote / Local Changes / Commits sections with remote branch name and sync status ([`8537ed8`](https://github.com/vlwkaos/wsx/commit/8537ed8))

### Docs

- Document git state icon vocabulary; compact README guide ([`e4ae84d`](https://github.com/vlwkaos/wsx/commit/e4ae84d))

---

## [0.6.3] - 2026-02-27

### Bug Fixes

- Fix asymmetric tree scrolling — up/down now use 1/4 and 3/4 thresholds ([`5040708`](https://github.com/vlwkaos/wsx/commit/5040708))

---

## [0.6.2] - 2026-02-27

### Bug Fixes

- Invalidate worktree git status on session detach so it re-fetches on return ([`f30794c`](https://github.com/vlwkaos/wsx/commit/f30794c))

---

## [0.6.1] - 2026-02-27

### UI

- Align help panel text wraps to description column ([`a64f499`](https://github.com/vlwkaos/wsx/commit/a64f499))
- Align session preview to bottom of panel so latest output is always visible ([`d92d5d7`](https://github.com/vlwkaos/wsx/commit/d92d5d7))

### Docs

- Add remote control, tmux status bar, `.gtrconfig` guide, and inspired-by section ([`39d1ef0`](https://github.com/vlwkaos/wsx/commit/39d1ef0))

---

## [0.6.0] - 2026-02-27

### Features

- Set tmux `status-right` to `project/alias` on session attach; expose `@wsx_project` / `@wsx_alias` session options ([`f7aa7cf`](https://github.com/vlwkaos/wsx/commit/f7aa7cf))
- Add `(a)` to cycle through active (◉) sessions ([`8d0c32f`](https://github.com/vlwkaos/wsx/commit/8d0c32f))
- Keep search active until explicit Esc — no auto-exit on single match ([`8d0c32f`](https://github.com/vlwkaos/wsx/commit/8d0c32f))
- Add `S` to send command to session without entering it ([`8d0c32f`](https://github.com/vlwkaos/wsx/commit/8d0c32f))
- Add `C` to send Ctrl+C to session without entering it ([`8d0c32f`](https://github.com/vlwkaos/wsx/commit/8d0c32f))

### UI

- Show version number in status bar bottom-right ([`c38c8ad`](https://github.com/vlwkaos/wsx/commit/c38c8ad))
- Hide worktree/session counts when expanded ([`f7f1780`](https://github.com/vlwkaos/wsx/commit/f7f1780))
- Show `✎` on worktrees with uncommitted changes ([`f7f1780`](https://github.com/vlwkaos/wsx/commit/f7f1780))
- Rebound project jump from `Ctrl+d/u` to `[` / `]` ([`f7aa7cf`](https://github.com/vlwkaos/wsx/commit/f7aa7cf))
