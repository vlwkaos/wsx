# Lean refactor: delete what the current model does not use

Audit at HEAD `914ca28f` (task 561). Lines are the ranges named in the rows, read from the source; no `git diff --stat` yet. "Proof" is a command or an owner message. `hand` and `preview` stay.

## The model

| Concept | One owner |
|---|---|
| Chat: a manager with a name and goal, flat, one main chat per project (`home.json`, `chats::set_main`) | `wsx_core::task::chats`, `needs::for_you` |
| Task: one line of work; a land ends it | task host (`crates/wsx/src/task/host.rs`), `wsx_core::task::{store,ledger,view}` |
| Agent: the process that does a turn | `agent::next`, `claude.rs`, `AgentProcess` |
| Stored knowledge: rules and facts; wsx deletes what is past | `wsx_core::memory` (one `Store`, one sweep, step 3) |
| Wire: one protocol, one handoff path | `runtime::protocol`, `wsx-daemon` |

Already gone (checked): threads, derived chats and the per-repo home chat. `rg 'ChatFrom|thread.rs|is_home|home chat' crates` = 0. `main_chat` stays: owner task 554.

## 1. What goes

| What | Why it is not live (proof) | Files, ~lines out | Proves nothing live broke |
|---|---|---|---|
| `trial`: `trial.rs`, `task trial`, `deploy --trial`, `land --tried`, host `start_trial/end_trial`, `Trial` types and store, TUI keys, web `trial_end`/`pass` | Owner: work-model §4, root-cause §5 Q1 yes. 0 uses since 10-29 (root-cause §5). `git log -S'mod trial'` = only the adding commit | wsx 476, core 130, web server 70, tests 134, docs 40 = ~850 | `task` (section 32), `web`, cargo checks |
| `batch`: `batch.rs`, hidden `land-batch`, `HostRequest::LandBatch`, `batch_tree`, `batch-check-*.log` | Same decision. `rg -n 'land-batch\|LandBatch\|batch_tree' crates` = 12 lines; only `host.rs:1109` invokes it. `--into` stays | wsx 319, core 17, tests 41 (32y), docs 22 = ~400 | `task` |
| Rotation notes, handovers, superseded rules as stored records (`remember_rotation`, `remember_handover`, `add_episode`, `Status::Superseded`, `decisions-archive.md`, `context-archive.md`, the `stale` mark) | Owner: work-model §7 picked. 160 rotation records, 153 of ended tasks (`records.jsonl`, 1117 KB, rotation = 680 KB). `start::stale` has 1 caller; true for 115 of 128 facts | core ~195, tests ~45 | `memory`, `task` (21a3, 37c), store copy (step 3) |
| Past-task index in a worker's first prompt | 27% of the first call, 7 of 20 workers used it (work-model §7). `claude.rs:465` sets `autoMemoryEnabled: false` for managers only | 1 line changed | `task` 37c, first-call size |
| Daemon wire: `Request::TerminalPaste/Mouse/Resize/Heartbeat`, 6 `Capabilities` flags, `legacy_tui_until`, protocol-15 bridge, `PrepareReplacement` path, `cfg(windows)` terminal code | `rg -nw 'Request::(TerminalPaste\|TerminalMouse\|TerminalResize\|TerminalHeartbeat)' crates` = 4 lines (definition, arms), 0 senders in v0.40.0 to v0.43.0. Capability flags: 0 reads. Poll always sends presence since v0.22.3. `live_handoff: true` in every wsxd since revision 25. CI and release build only Linux and macOS. Floor: v0.40.0 (revision 34) | core+daemon+terminal ~307, tests ~60 | `smoke` (`runtime-smoke.py:927` sends `terminal_heartbeat`; switch to a stream message), cargo checks |
| Hidden CLI: `session send-keys`, `runtime status` | `rg -n 'send-keys\|SendKeys' crates scripts` = 2 definition sites; `runtime status` only in `main.rs:155` test | ~36 | `cli/tests.rs`, cargo checks |
| Dead core code: `ChatCounts::waits_words`, notes-file reader `notes()` with `Kind::Decision/Memo` | `rg -nw waits_words crates scripts` = 1. `notes()` reads only files that are not record views (`index.rs:607-653`) | 19 + 65 | `memory`, `task` |
| Unread web server fields and routes: action `hold`/`unhold`, `/assets/{name}`, legacy push `topics`, 10 detail keys, exchange fields, timeline `session`, queue `resent`, push `kind`, `site.id`, session `pane_id` | `rg -c` in `web/*.js` and scripts = 0 for each (list in step 6). Needs wsx-ui to confirm the port reads none | ~80 | `web` |
| Release-gated shims: `legacy_record`, `old_host_args`/`manager-host`, `RepoState.checkin`, asched import, serde aliases, `restore_question` | Each comment says "one release". In v0.43.0: `legacy_record` (c1edcad0), `old_host_args` (a09d5fc8, not in a tag), asched import (afa7a9ad). Gate: remove after the 0.44 tag | ~505 incl. tests `task_legacy_creator.rs`, `task_cut_reason.rs`, `routine_wsxd_lifecycle.rs` | `task`, `smoke` |
| 61 design files (813 KB, 7,870 lines) and 4 mockup folders (16.5 MB), plus `task-sheet/` (5.6 MB) | Each is built, measured or an audit whose findings are in git. Kept: `work-model.md` + measure, `web-solid.md`, `client-plugins*`, `transcript.md`, `memory-layers.md`, `scheduler.md`, `scale-out.md` | 7,870 lines md and py | `rg` for each deleted name outside `docs/design` = 0 after the refs are fixed (step 7) |
| `scripts/memory-eval.py` (2081), `scripts/context-measure.py` (391) | Only their own deleted designs and one comment in `start.rs` cite them; 0 in CI | 2,472 | `rg -n 'memory-eval\|context-measure' .github crates scripts docs` = 0 |
| Cuts from `orchestration-lean.md` still unbuilt: `decide --msg`, `@project`, reply schema, `say --model`, `live` flag, `for_policy` | `rg` finds no symbol; no task since 11-10 | 0 code, doc goes | none |

## 2. What changes, not deleted

| Change | Why | Files | Proof |
|---|---|---|---|
| One `Store::delete` rewrite and one sweep that holds every §7 row (task close, supersede, next handover, fact at land via `fold_memo`) | The store is append-only (`store.rs:468`); the only new primitive. `remember_*` and archive code go (table 1) | `memory/{store,manage,record,start,backend}.rs`, `host.rs`, `manager.rs` | `memory`; run on a copy of the store, counts below |
| `steer` becomes `add --now` | cli-lean C3 stands; one verb | `args.rs:75`, `cmd/work.rs:661`, `mod.rs:76` | `task` |
| Duplicates fold into the owner: `clip` into `manage::clip_to`, `is_zero` (2 copies), `print_json` (2 copies), the overview held twice (`App.overviews`, `Watch.overviews`), 3 `seen` copies | Same body, other name | core `integration/memory.rs:1075`, `index.rs:107`, `manager.rs:646`; wsx `task/mod.rs:247`; web `tasks.rs:1476`, `lib.rs:68` | cargo checks; `web` |
| The docs state the model once: `docs/tasks.md` (drop trial and batch, 22+11 lines), `docs/cli.md`, `docs/memory.md`, `docs/web-client-tasks.md` (6 lines), `README.md:74` | A contract says what exists | those files | `rg -i 'trial\|batch land' docs README.md` = 0 |

## 3. Stored knowledge outside git (this machine, project `wsx`)

Counts read now from `tasks/wsx-8cbe3f08/memory/records.jsonl` and the folders. wsx deletes; nobody does it by hand.

| Kind | Count, size | Deleted by |
|---|---|---|
| Rotation notes records (`rotation-*`) | 160 (153 of ended tasks), 680 KB | The sweep at task `close`; the first run purges the 153. `rotate-notes.md` in the task folder stays |
| Handover records | 47, 87 KB | The chat host when it writes the next (`manager.rs:1730`); first run keeps the newest per chat |
| Superseded rules | 143 (121 imported, 22 by `supersede`), 96 KB | `Store::supersede` deletes at once; first run purges all. `decisions-archive.md`, 3 `*.pre-memory` files go with it |
| Loose chat notes (`chats/wsx/notes`) | 72 files, 1.6 MB; 65 are not `context*`/`decisions*` | The chat host, when the task number in a name closes; the rest in the first run (root-cause Q4 yes) |
| Loose chat files: `t371.json`, `t376.diff`, `decisions-before-trim-*`, `skill-backup-*`, `threads-placed` (9 chats) | `threads-placed` read by 0 code (`rg` = 0) | First run |
| Project folder leftovers: `batch-check-1..4.log`, `trial.lock`, `grants.json` (0 code refs) | 4 logs, 2 files | First run |
| Past-task index (Claude auto memory `MEMORY.md`, 183 lines, 27 KB) | Loaded in every worker's first call | Not deleted: workers start with `autoMemoryEnabled: false` |
| Facts | 144 + 31 imported/backfilled | `fold_memo` at a land, by paths gone or held by the land's memo |

Not touched, reported: `chats/pr-review/scratch` (184 MB) and `chats/ir/scratch` (80 MB), not written by wsx code (`rg scratch crates/*/src` = 0 outside batch); 561 task folders (559 ended, 186 MB; captures 19 MB, `search.jsonl` 9.5 MB) have no delete rule and the ledger reads them. Both are the owner's call.

## 4. Steps in landing order

| # | Cut | Files | Order | Proof |
|---|---|---|---|---|
| 1 | Remove trial (rows 1 of table 1, web server part) | `host.rs`, `cmd/{deploy,land,work}.rs`, `args.rs`, `task/mod.rs`, core `task/{mod,store,ledger,view,replace}.rs`, web `tasks.rs`, `http.rs`, `task_list.rs`, `ui/{help,mod}.rs`, `test-task.py`, `test-web-client.py`, `web-tasks-check.mjs`, `docs/tasks.md` | first | `task`, `web`, fmt, clippy, nextest |
| 2 | Remove batch | `batch.rs`, `cmd/land.rs`, `host.rs`, `args.rs`, core `task/mod.rs`, `http.rs`, `test-task.py` 32y, `docs/tasks.md` | after 1 (same files) | `task` |
| 3 | One store delete, one sweep, no archives, no stale mark, workers off auto memory | `memory/*`, `host.rs`, `manager.rs`, `claude.rs`, `test-memory.py`, `test-task.py` 21a3/37c, `docs/memory.md` | after 2 (`host.rs`) | `memory`, `task`; run the sweep on a copy of the store: records 503 active before, no rotation of an ended task, no superseded, handovers = chats |
| 4 | Daemon and terminal wire cut | core `runtime/{protocol,domain,client}.rs`, `wsx-daemon/*`, `wsx-terminal/ghostty/mod.rs`, `runtime-smoke.py`, `test-terminal-context.py` | parallel to 1 to 3 (no shared file); before 8 | `smoke`, cargo checks; bump `DAEMON_REVISION` only if a worker finds a behavior change |
| 5 | Small cuts and folds: dead code, hidden CLI, `steer`, `notes()`, duplicates | `needs.rs`, `index.rs`, `integration/memory.rs`, `cli/args.rs`, `cli.rs`, `main.rs`, `task/mod.rs`, `args.rs`, `cmd/work.rs` | after 3 (`index.rs`) and 2 (`args.rs`) | cargo checks, `task`, `memory` |
| 6 | Unread web fields (hand the `web/*.js` side to wsx-ui first: `task.js`, `tasks.js` trial UI; `assets.rs` list changes with the Solid port) | `tasks.rs`, `chat.rs`, `http.rs`, `push.rs`, `events.rs`, `test-web-client.py` | after 1, 2 and wsx-ui's answer | `web` |
| 7 | Delete design files and the 2 eval scripts; fix refs first: `policy.rs`, `manage.rs`, `start.rs`, `runtime-smoke.py`, `app.rs:742`, `tests.rs`, `client_tests.rs`, `memory.md:106`, `web-client-tasks.md:48,78`; re-grep `design/task-sheet` before deleting it | `docs/design/*`, `scripts/{memory-eval,context-measure}.py` | after 3 (`start.rs`, `manage.rs`) | `rg` of every deleted name = 0; fmt, build |
| 8 | Release-gated shims | `store.rs`, `replace.rs`, `manager.rs`, `repo.rs`, `routine.rs`, `Cargo.toml`, 3 tests | after the 0.44 tag | `task`, `smoke` |
| 9 | Delete this file, `root-cause-refactor.md`, `cli-lean.md` | `docs/design/` | last | none |

## 5. Lines out and in (estimate)

| | Out | In |
|---|---|---|
| Code and tests, steps 1 to 6 | ~2,160 | ~170 (`Store::delete` and sweep ~150, `add --now` ~10) |
| Step 8 (gated) | ~505 | 0 |
| Docs and scripts, step 7 | ~10,300 lines, 22 MB of mockups | 0 |

## Earlier audits

| Doc | Standing | Landed | Dropped |
|---|---|---|---|
| `deep-refactor.md` | step 2 (`send-keys`, `Runtime`, `Ping`, `Party`; `Handback` is `hand`, kept), step 4 (`drift`, `views.json`, `check_views`, notes preamble; not in this plan, measure first) | steps 1, 3, 5 | steps 6 to 9 (flat-chats) |
| `refactor-audit-core.md` | #4 `owner::say` takes 9 args (a signature change) | 14 of 15 | 0 |
| `refactor-audit-daemon.md` | #4 scheduler reshape; #5 asched import (step 8) | 8 of 11 | 0 |
| `refactor-audit-web.md` | #4 long functions; #14 ten fixed sleeps (~55 s) | 14 of 16 | 0 |
| `refactor-audit-cli.md`, `refactor-audit-264.md` | none | 9 of 9, 5 of 5 | 0 |
| `cli-lean.md` | C3 `steer` (step 5), C6 hide `group`/`plugin` (2 attributes; `plugin` belongs to client-plugins) | C1, C2, C4, C5, C7 | 0 |
| `orchestration-lean.md` | tasks 5, 7, 9, 10 (unbuilt; dropped here) | 1, 3, 4, 6, 8, most of 2 | `--into` removal (owner kept it), rerun with per-part paths |
| `root-cause-refactor.md` | steps 11, 12; §5 Q1 = steps 1 and 2, Q4 = step 3 | 10 of 12; Q3, Q5 | Q2 Sonnet event turns (task 462 measured: off) |

## Checked and left alone

`main_chat`, `open_main` (live, 15+ callers); `refuse_old_layout` (guards a store that can exist); `refuse_unimported` (data guard, until the owner's store is converted: [unverified]); `chat_rows_ask.rs` `OLD_ASK_LINE` (shipped in v0.43.0); frame-stream `heartbeat: false` (v0.41/0.42 clients); `test-sections.py` (tests live `sections.py`, unwired: the manager's call); `chats_home_folder.rs` (web Home folders, not a home chat). `rg` over `pub` names: 1 of 1,131 in core and 1 of 1,230 in wsx has no caller (`waits_words`). Compiler dead-code lint cannot see unused `pub` items of a library crate: not run.
