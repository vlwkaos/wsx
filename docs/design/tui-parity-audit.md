# TUI parity audit: the manager flow

Rule: a web change keeps TUI parity, where the feature fits a terminal. Audit date 2026-11-11, `main` at `a2e3ae38`. Read-only. Complements [tui-parity.md](tui-parity.md) (web changes up to #198), which is still true.
Assuming: #397's prompt was not readable here. Its scope is read from branch `wsx/t397` (role model in the room heading and Roles row: `task_chat.rs`, `task_list.rs`, `ui/task_chat.rs`) and from the memory note (no role rows in the TUI sidebar, on purpose). Those rows are marked "covered by #397".
Paths: TUI under `crates/wsx/src/`, web under `crates/wsx-web/web/`. "Driven" = the real TUI run in a PTY by a scratch script on `scripts/tui_pty.py` (isolated HOME, one git project), 40 rows by 120 columns. Gaps 3, 4 and 5 were driven; every other row rests on the cited code.

## Gaps

| # | Gap | Web / CLI has it | TUI lacks it | Impact | Fix |
|---|---|---|---|---|---|
| 1 | The Workspace tree shows no per-project need: approvals, To check, stuck or failed tasks, "N working". Only the name turns accent while the manager asks. | `home.js:20-27,58-70` (needs, status line, count, order) | `ui/workspace_tree.rs:209-213` (accent only); `app/events.rs:285-300` reads `chat::asking` and nothing else; `n/N` jump sessions (`event.rs:507-509`), not tasks. The owner opens `t` per project to see a need. | high | M |
| 2 | The task detail has no action: Land, Answer, Take over / Hand back, Try, Pass and land, End trial. | `task.js:182-196`; CLI `task land`, `hand`, `handback`, `trial` (`task/args.rs:136,160,168`) | `app/dispatch.rs:163-172` (detail: scroll and close only); footer `ui/mod.rs:749-752` `(j/k)scroll (S)hell Esc`. The owner types "land #n" to the manager, which costs a turn. | high | M |
| 3 | `?` help is cut: its Groups and Global sections (`[`/`]`, `i/I`, `a/A`, `n/N`, `R`, `,`, `q`, `Q`) cannot be reached. The popup is capped at 42 rows and has no scroll. It also lacks `/` search, `\|` and `-` split, and `S`. | README "Keys" lists them | `ui/mod.rs:989` (`clamp(12, 42)`), `994-1070` (entries), `app/dispatch.rs:259-263` (only closes). Driven: at 40 and at 70 rows the text "Global" never shows, and `j`, `G`, PgDn do nothing. `?` in the Tasks view or chat is not a key (`dispatch.rs:121-175`, chat types it). | high | S |
| 4 | Footer and README out of step with the key map (`event.rs:467-550`). The Project row footer lacks `(t)asks`, `(v)` and `(M)`, the entry to the whole manager flow. | README "Keys" has `t`, but not `M`, `[`/`]`, `H`, `o`, `S`, nor the chat keys | `ui/mod.rs:604-616`. Driven: footer on a project row = `(e)dit (w)orktree (u)routine (T)groups (d)unregister (i/I)idle ... (,)config (q)uit`. The Session footer `ui/mod.rs:645` omits the split keys that README lists for sessions. | med | S |
| 5 | Bracketed paste is dropped in every text input: chat line, `p` path prompt, search, routine and settings fields. A long reply or a path cannot be pasted. | `chat.js` `pasteFiles`; docs/web-client-tasks.md names the TUI chat as having no counterpart | `tui.rs:397` turns it on; `event.rs:412` `Event::Paste(_) => Action::None`; `app/run_loop.rs:73-83` maps those modes to Input. Driven: pasting a path into `p` leaves `project:` empty; pasting in the manager chat leaves `> ` empty. | med | S |
| 6 | A routine cannot be run now or turned off and on from the tree. The editor has an `enabled` field, so on/off costs a form. | `routines.js:81` Run now, Turn on/off; CLI `routine run` (`cli/routine.rs:128`) | `ui/mod.rs:660` `Selection::Routine` footer is `(e)dit (d)elete`; no run action in `app/` or `ui/`. | med | S |
| 7 | The chat scrolls with `j/k` and `G` only: no PageUp/PageDown, no jump to the oldest. | docs/web-client-chat.md "Long chats" | `task_chat.rs:864-869` | low | S |
| 8 | Usage: no Projects tab, no Period (5-hour, 7-day) or Who filter. Documented as intended. | `usage.js` tabs and filters | `ui/tasks.rs:42-46` sum line only; docs/tasks.md "TUI" | low | M |
| 9 | Enter on an asked role does nothing. | Roles list | `task_list.rs:238-250` (`open` roles only). Partly covered by #397. | low | S |

## Present, or not a gap

| Feature | TUI form |
|---|---|
| Quote by id, question `r`/`d`, Pass/Skip/Fail with note, approvals `y`/`n` | `task_chat.rs:871-888`, footer `ui/task_chat.rs:285-296` |
| Media list `m`, overview `o`, find `/`, session and file links | `task_chat.rs:856-860`; the list has no pixels and no uploads (web store) |
| Panels `v`, machine panels `M`, links and cards | text form, documented in docs/panels.md "Links", "Cards"; no followable link |
| Routines pill and message jump | tree group plus detail `Enter` (gap 6 is only run and on/off) |
| Role model in heading and row, role rows in the sidebar, role news dot | covered by #397 |
| `wsx memory`, task history search | CLI only; the web has neither (docs/tasks.md:461) |
| A manager in another project: events and memo back | rows come from shared `chat_rows`; not driven, no origin mark on a task row |
| Attachments, push, header position, image viewer | web only, touch or browser features |

## Top 5 fixes

1. **Gap 1, needs in the tree.** The 2 s reader of `events.rs:285` returns the asking set; widen it to approvals, To check and stuck counts from the store, show them after the project name, order `n` jumps by them. M.
2. **Gap 2, task detail actions.** Keys `L` land, `a` answer (opens the chat line with `#n `), `H` hand, with a y/n confirm as approvals have; call the CLI paths the web uses. M.
3. **Gap 3, scrollable help.** Give `Mode::Help` a scroll (`j/k`, PgUp/PgDn), add `/`, `\|`, `-`, `S`; make `?` open a Tasks-view help. S.
4. **Gap 5, paste.** Route `Event::Paste` in Input modes to the active field as typed text (newlines to spaces in one-line fields). S.
5. **Gaps 4 and 6, footer and routine keys.** Add `(t)asks`, `(v)` to the Project footer, `M`, `[`/`]` to README; run now and on/off on a routine row (not `R`: it is Refresh, `event.rs:499`). S.
