# Web and TUI parity audit

Rule: a web change keeps TUI parity; the TUI gets the feature where it fits a terminal.
Assuming: the range is `85a327fc..HEAD` as given (that commit is v0.41.0, so it holds 0.42.0, 0.43.0 and `[Unreleased]`); #184 is already in it (`afe91a64`); #198 is read from its branch `wsx/t198` (`aee351e3`, not on main yet). Read-only audit, no code run. TUI paths are under `crates/wsx/src/`, web paths under `crates/wsx-web/web/`.

| Feature | Web (file) | TUI today | Fits a terminal? | Proposed TUI form |
|---|---|---|---|---|
| Overview header button (#184); Overview sheet of `overview.md`, capped at 60 lines | `tasks.js` header, `page.js` | missing: `overview` is only a store path (`task/boundary.rs:29`), no view | yes | `o` in the Tasks list and manager chat opens a scroll view of `overview.md` (text, mermaid as a code block) |
| Routines header button (#184) and the read-only Routines screen | `routines.js`, `tasks.js` | present, richer: Routines group per project `ui/workspace_tree.rs:290,518`, detail `app/actions.rs:37`, add/edit/delete `action.rs:17` | yes | none; the header button is n/a (the tree is the entry) |
| Routine post border (#184): the secondary accent on a routine's reply | `bubbles.js:45,59`, `app.css` | missing: a routine reply is a plain manager row `task_chat.rs:481-492`, `ui/task_chat.rs:226-238` | yes | `Row::Said` gets a `routine` flag; `ui/task_chat.rs` draws its bar `│` in the secondary color, as the asks bar `:231` does. Note: the web rule matches only the events text "daily digest" (`bubbles.js:45`), but docs/tasks.md "Routines" posts `routine <name>`; fix both together |
| Session news line (#198): `[claude in main](session/12) exited` in the project chat | `bubbles.js` (link to the session chat) | missing: the TUI shows the text as written (docs/tasks.md "Session news"); the chat has no link rows | yes | the chat parses `[text](session/<id>)`, shows `text`, and a key (`Enter` on the selected row, or `g`) enters that session's terminal as `S` does `app/actions.rs` |
| `files/<path>` links in a routine post (`875ced5a`) | `bubbles.js:180` opens Files | missing: link text shows raw | yes | same link parser; `Enter` on a `files/` link opens the file in the preview pane or `$EDITOR` |
| Find in every chat | `find.js`, `chat.js` | present for manager and role chat: `/`, `n`/`N`, `Esc` `ui/task_chat.rs:37,91-104`, `task_chat.rs:123` | yes | none. A session chat does not exist in the TUI (the terminal is the session) |
| Usage sheet: cost by day, stacked bar | `usage.js` | present `ui/tasks.rs:216-290` (needs 30 rows) | yes | none |
| Usage: Tokens tab (input, cache read/write, output) and cache hit rate per day | `usage.js` | missing: only cost parts `ui/tasks.rs:218,229`; a task's detail shows tokens, no day view | yes | `u` in the Tasks list toggles the day rows between cost and tokens, same ledger fields |
| Task sheet: whole summary and decisions, Less folds | `task.js` | present: summary and memo in detail `ui/tasks.rs:300,371` | yes | none; no fold needed (it scrolls) |
| Agent, model and effort after the model | `tasks.js` bar, `task.js` | present: task Agent row `ui/tasks.rs:325`; manager `task_chat.rs` heading | yes | none |
| Stand-in reply tag "not checked" | `bubbles.js` | present `task_chat.rs:491` | yes | none |
| Reply that asks the owner set off by color | `bubbles.js`, `home.js` | present `ui/task_chat.rs:217-236`; Workspace rows have no mark | yes | `ui/workspace_tree.rs` project row: accent name while the manager asks (as Home); `n` (`event.rs:504`) already jumps by attention |
| Task log shows ping rows ("kept the prompt cache warm") | `task.js:41` | missing: the chat hides `Ping` `task_chat.rs:512`; detail has no log `ui/tasks.rs:300-371` | partly | one muted "pings: n" count in the task detail Usage line; no log view needed |
| Fail / Answer put `#n ` in the manager composer | `checks.js:47`, `tasks.js` | present: `f` fails with a note `task_chat.rs:613`, `i` writes | yes | none |
| Project shell (top-row icon) | `tasks.js` | present: `S` (docs/tasks.md "TUI", `event.rs:529`) | yes | none |
| Add and remove a project from Home | `home.js`, `projects.rs` | present: `p` `event.rs:487`, `d` `event.rs:495` to `app/actions.rs:575` | yes | none |
| Shell-first sessions; `+` Agent or Shell; every project in Sessions | `inbox.js`, `shell.rs` | present in kind: `s` makes a plain session `event.rs:489`, tree lists all projects; no `agent_command` (web site setting only) | yes | none |
| `! command` line in a session chat | `chat.js`, `chat.rs` | n/a: the TUI session is the terminal, the user types `!` there | no | n/a |
| Header position Top or Bottom (#184) | `settings.js`, `app.css` | n/a | no | n/a: per-device touch layout; the TUI has one fixed footer |
| Image viewer bar under the image, share icon | `viewer.js` | n/a | no | n/a: touch ergonomics |
| Push, "Manager needs me", device digest to routine | `push.rs` | n/a | no | n/a: browser notifications; the digest is now a routine, which the TUI has |
| Home unread news line and dot | `home.js:76` | missing: no unread state in the Workspace tree | partly | covered by the accent mark above; no separate unread dot |
| Settings page, Files browser, theme | `settings.js`, `files.js` | n/a | no | n/a: the TUI has `global_settings.rs` and the preview pane |

## Fix tasks

1. **Chat links and routine bar.** Parse `[text](session/<id>)` and `[text](files/<path>)` in the TUI chat; `Enter` opens the session or file; routine replies get a secondary-color bar; align the web's routine rule with "routine <name>" events.
   Files: `crates/wsx/src/task_chat.rs`, `crates/wsx/src/ui/task_chat.rs`, `crates/wsx/src/app/actions.rs`, `crates/wsx-web/web/bubbles.js`, `scripts/web-tasks-check.mjs`, `docs/tasks.md`.
2. **Overview view.** `o` opens `overview.md` in a scroll view from the Tasks list and chat; no edit.
   Files: `crates/wsx/src/task_list.rs`, `crates/wsx/src/task_chat.rs`, `crates/wsx/src/ui/tasks.rs`, `crates/wsx/src/ui/task_chat.rs`, `docs/tasks.md` "TUI".
3. **Usage tokens.** `u` toggles day rows between cost and tokens with the cache hit rate; show the ping count in the task detail.
   Files: `crates/wsx/src/ui/tasks.rs`, `crates/wsx/src/task_list.rs`, `docs/tasks.md` "TUI".
4. **Workspace asks mark.** The project row shows the accent name while the manager asks.
   Files: `crates/wsx/src/ui/workspace_tree.rs`, `docs/tasks.md` "TUI".

Tasks 1 and 4 touch `docs/tasks.md`; #197 (Routines in-house) shares this file, so keep edits to the TUI section.
