# Web and TUI parity audit

Rule: a web change keeps TUI parity; the TUI gets the feature where it fits a terminal.
Assuming: the range is `85a327fc..HEAD` as given (that commit is v0.41.0, so it holds 0.42.0, 0.43.0 and `[Unreleased]`); #184 is already in it (`afe91a64`); #198 is read from its branch `wsx/t198` (`aee351e3`, not on main yet). Read-only audit, no code run. TUI paths are under `crates/wsx/src/`, web paths under `crates/wsx-web/web/`.

| Feature | Web (file) | TUI today | Fits a terminal? | Proposed TUI form |
|---|---|---|---|---|
| Overview header button (#184); Overview sheet of `overview.md`, capped at 60 lines | `tasks.js` header, `page.js` | present: `o` in the Tasks list and manager chat opens `overview.md` (`app/dispatch.rs:127,161`, `task_list.rs` `open_overview`) | yes | none |
| Routines pill and list in the composer, with run now and on/off (replaced the header button and read-only screen) | `routines.js`, `tasks.js` | present, richer: Routines group per project `ui/workspace_tree.rs:290,518`, detail `app/actions.rs:37`, add/edit/delete `action.rs:17` | yes | none; the header button is n/a (the tree is the entry) |
| Routine post border (#184): the secondary accent on a routine's reply | `bubbles.js:45,59`, `app.css` | present: a reply marked `post` has its bar in the secondary color (`ui/task_chat.rs:349-359`); whether the web rule (`bubbles.js:45`) also matches `routine <name>` events is not re-checked here | yes | none |
| Session news line (#198): `[claude in main](session/12) exited` in the project chat | `bubbles.js` (link to the session chat) | present: the chat parses `[text](session/<id>)` (`task_chat.rs:47-56`); `Enter` on the link enters the session (`app/dispatch.rs:152-157`) | yes | none |
| `files/<path>` links in a routine post (`875ced5a`) | `bubbles.js:180` opens Files | present: same link parser (`task_chat.rs:60`); `Enter` shows the file in place of the chat (`task_chat.rs:888`, docs/tasks.md "TUI") | yes | none |
| Find in every chat | `find.js`, `chat.js` | present for every chat: `/`, `n`/`N`, `Esc` `ui/task_chat.rs:37,91-104`, `task_chat.rs:123` | yes | none. A session chat does not exist in the TUI (the terminal is the session) |
| Usage sheet: cost by day, stacked bar | `usage.js` | present `ui/tasks.rs:216-290` (needs 30 rows) | yes | none |
| Usage: Tokens tab (input, cache read/write, output) and cache hit rate per day | `usage.js` | present: `u` in the Tasks list toggles the day rows between cost and tokens (`app/dispatch.rs:166`, `ui/tasks.rs:50`) | yes | none |
| Task sheet: whole summary and decisions, Less folds | `task.js` | present: summary and memo in detail `ui/tasks.rs:300,371` | yes | none; no fold needed (it scrolls) |
| Agent, model and effort after the model | `tasks.js` bar, `task.js` | present: task Agent row `ui/tasks.rs:325`; manager `task_chat.rs` heading | yes | none |
| Stand-in reply tag "not checked" | `bubbles.js` | present `task_chat.rs:491` | yes | none |
| Reply that asks the owner set off by color | `bubbles.js`, `home.js` | present `ui/task_chat.rs:217-236`; Workspace rows have no mark | yes | `ui/workspace_tree.rs` project row: accent name while the manager asks (as Home); `n` (`event.rs:504`) already jumps by attention |
| Task log shows ping rows ("kept the prompt cache warm") | `task.js:41` | present as a count: the spend line shows " · n pings" (`ui/tasks.rs:560`); the chat hides `Ping` rows | partly | none; no log view needed |
| Fail / Answer put `#n ` in the manager composer | `checks.js:47`, `tasks.js` | present: `f` fails with a note `task_chat.rs:613`, `i` writes | yes | none |
| Project shell (top-row icon) | `tasks.js` | present: `S` (docs/tasks.md "TUI", `event.rs:529`) | yes | none |
| Add and remove a project from Home | `home.js`, `projects.rs` | present: `p` `event.rs:487`, `d` `event.rs:495` to `app/actions.rs:575` | yes | none |
| Shell-first sessions; `+` Agent or Shell; every project in Sessions | `inbox.js`, `shell.rs` | present in kind: `s` makes a plain session `event.rs:489`, tree lists all projects; no `agent_command` (web site setting only) | yes | none |
| `! command` line in a session chat | `chat.js`, `chat.rs` | n/a: the TUI session is the terminal, the user types `!` there | no | n/a |
| Header position Top or Bottom (#184) | `settings.js`, `app.css` | n/a | no | n/a: per-device touch layout; the TUI has one fixed footer |
| Image viewer bar under the image, share icon | `viewer.js` | n/a | no | n/a: touch ergonomics |
| Push, "Manager needs me", device digest to routine | `push.rs` | n/a | no | n/a: browser notifications; the digest is now a routine, which the TUI has |
| Home unread news line and dot | `home.js:76` | present as the accent project name while the manager asks (`ui/workspace_tree.rs:209-213`); no unread state | partly | none; no separate unread dot |
| Settings page, Files browser, theme | `settings.js`, `files.js` | n/a | no | n/a: the TUI has `global_settings.rs` and the preview pane |

## Fix tasks

None open: the four tasks this audit proposed (chat links and routine bar, overview view, usage tokens, Workspace asks mark) are in the code, as the rows above show.
