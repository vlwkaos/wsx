# Refactor audit: wsx-core since v0.41.0

Scope: `git diff v0.41.0..HEAD -- crates/wsx-core scripts/test-task.py` (466 commits, all unreleased; `migrate_page` is the one shipped shim and stays). Plan: one refactor task per finding, then the release (owner, msg 3189). Open task #264 files (tasks.js, app.css, web-tasks-check.mjs pills blocks) are in no finding.

Proof marks: **C** = I read the line on HEAD and ran a grep (callers, copies); **R** = read on HEAD only; **A** = found by a read-only subagent, anchor line re-read by me. No finding was proven by running a test: this audit built and ran nothing. Sizes: S = a change you can state; M = several files, one idea.

## Ranked findings

| # | Sev | Finding | Size |
|---|---|---|---|
| 1 | high | Two rules for "task session" | S |
| 2 | high | `run()` in test-task.py is one 4850-line function | S, then L |
| 3 | high | 296 s of fixed sleeps in test-task.py | M |
| 4 | med | "stopped by request" is a literal in two crates | S, then M |
| 5 | med | `status_of` has dead arms; `After.because` is optional for no case | S+S |
| 6 | med | `localtime_r` written 5 times | M |
| 7 | med | Short commit written 6 times, two lengths | S |
| 8 | med | `chat_rows` is 228 lines; the CLI keeps its own wording | M |
| 9 | med | `ledger::build` is 144 lines and reads state twice | S |
| 10 | med | `panel::run` copies git's bounded runner | M |
| 11 | low | Agent-name rule x3, `validate_tasks` 244 lines | S/M |
| 12 | low | Scenario hygiene: marks, absence waits, gated fixture | S/M |
| 13 | low | `search.rs` owns helpers `history.rs` needs | S |
| 14 | low | `tail_line`, stray doc comment, `task_agent` re-parses the log | S |
| 15 | low | Docs: daemon-discovery.md and CLAUDE.md lack new modules and files | S |

## Detail

1. **Task-session rule drift (C).** `crates/wsx-web/src/chat.rs:1212` and `crates/wsx-web/web/store.js:123` reject `task-<n>-hand`; core's `task/session_news.rs:12` (aee351e3) accepts it and says it is "the same rule" (:11). `needs_owner` (chat.rs:1229) can count a hand session; `http.rs:747` uses core's rule. Fix: chat.rs calls `session_news::is_task_session(label, provider)`; add `-hand` to the store.js regex. Files: chat.rs, store.js. Land first, alone.
2. **One function (C).** `gone` is a helper at `scripts/test-task.py:450` (c5d38a9b) and a task id at :4086 (8a4e7a5f); part 38 had to add `process_gone` (:5097). `ran`, `row`, `answers`, `stamp` are rebound the same way (A). Fix S: rename the rebound locals, add an AST check that no assigned name in `run()` is also a nested def. Fix L: `part_NN(c)` functions with `--only`; part 38 stays last. Do S now, L after 12.
3. **Sleeps (C).** `time.sleep` 75/25/25 at :3017, :3031, :3059 wait out `GRACE` 60 s (`task/stall.rs:18`, b7eaf6f9); 100/25 at :4021, :4029 (b4ee9814); 14 at :3146; 20 at :4881 against `recapMinutes: 0.2`. 43 sleeps in all. Fix: a test-only environment override for GRACE and the recap minutes (small Rust knob), then drop the waits. Files: stall.rs, test-task.py. Parts 27, 32af, 33-35 change together. After 12.
4. **Stop reason (C).** `STOPPED_BY_REQUEST` is `pub(crate)` (`task/status.rs:15`, d3dbec6f); `crates/wsx/src/task/host.rs:1018,1020` repeat the literal. `manager::event_wakes` (`task/manager.rs:244`) parses back text that `crates/wsx/src/task/manager.rs` builds (A). Fix S: make the const `pub`, use it in host.rs. Fix M: carry a `wakes` flag with the event and delete the text sniffing; lands with `owner::say` taking a request value instead of 8 arguments (`owner.rs:73-74`, bf35ba80; 6 callers, A). After 5.
5. **Status (C/R).** `status_of` (`task/status.rs:145`, 228 lines) has two unreachable arms at :252-253: :178 and :181 already catch `CheckSlot` and `CheckRun` in any state (646b6818, 0873aa08). `After.because: Option` (`task/mod.rs:118`, 0d5a8067) never is `None`: `--because` is required (`crates/wsx/src/task/mod.rs:210`). Fix: split `status_of` into per-state functions, drop the arms; make `because` plain, keep `Option` on the clear request only. Check records on disk that lack `because`. After 9; the two share hunks, status first.
6. **localtime_r (C).** Copies: `task/ledger.rs:376` (91e13629), `task/usage.rs:527`, `crates/wsx/src/task/limit.rs:57`, `routine/cron.rs:19`, `crates/wsx/src/ui/preview.rs:709`. Fix: one `local_tm(secs)` in `routine`; move `trigger_text`, `parse_trigger`, `local_minute` (`ledger.rs:306-374`, A) there; `limit::local_time` goes. Files: about 8. After 9.
7. **Short commit (C).** 7 characters: `task/view.rs:88`, `task/history.rs:66`. 12: `task/owner.rs:144`, `task/report.rs:176`, `crates/wsx/src/task/host.rs:3436`, `crates/wsx/src/task/mod.rs:2198`. cdc5120a merged only two. Fix: one `short_commit` in view.rs; choose the length once. It changes shown text: docs/tasks.md says 12; update scenario asserts with it.
8. **chat_rows (C/A).** `task/chat.rs:379-606`, bf35ba80. Callers repeat window, read, asking, rows: `crates/wsx/src/task_chat.rs:666-702`, `crates/wsx-web/src/tasks.rs:708-739`. The CLI printer (`crates/wsx/src/task/mod.rs:2961-2995`) prints its own words for rotate, limit and stop rows. Fix: split into message, event and reply row functions; add `chat::page`. The web caller edits entries between read and rows, so `page` needs a hook. `chat_rows` has no direct test: add one first.
9. **ledger::build (C).** `task/ledger.rs:25-168`; `now` at :37 but `super::now_unix_ms()` again at :134; `result.json` is read here and again inside `status`. Fix: split into row builders and a size fit. Land before 5 and 6.
10. **panel::run (C/A).** `panel.rs:515` (62915a2d) copies `git/mod.rs` bounded output: `killpg` (`panel.rs:565`, `git/mod.rs:278`), `STREAM_GRACE` (`panel.rs:31` 300 ms, `git/mod.rs:252` 2 s), tail readers. Git drops partial output on timeout; panels need it, so add `git::run_bounded` returning both. Also `panel::now_ms` (:469) equals `task::now_unix_ms` (`task/mod.rs:602`); `MAX_PANELS`, `MIN_REFRESH_SECS`, `DEFAULT_TIMEOUT_SECS`, `MAX_TIMEOUT_SECS`, `plain_text` are `pub` with no outside user (grep). wsx-core is published: note the narrowing in the CHANGELOG.
11. **Config (C/R).** Agent-name check `is_ascii_alphanumeric() || '-' | '_' | '.'` at `task/store.rs:379` (b7299489), `config/project.rs:525`, `config/global.rs:345` (48b4a82a): one `is_agent_name`. `validate_tasks` (`project.rs:533-775`) repeats the adapter check and the minutes conversion (A): split per section. The comment above `validate_effort` (:910) belongs on `validate_word` (:920).
12. **Scenario hygiene (C/A).** `mark()` names repeat: `21b` (:1904, :2424), `32h`, `32v`, `32r`, `32s`, so slowest-part output misattributes time. About 33 "sleep, then assert nothing happened" waits pass on a late event. 14 copies of the gated-check fixture (`while [ ! -e`). Fix: assert unique marks; a `stays()` helper that waits for a later positive event first; a table for the 32t family. Before 2 (L) and 3.
13. **Helpers misplaced (C).** `history.rs:6` imports `day_of`, `first_line` from `search.rs:717,273`, which imports history. `FIRST_READ_BYTES` is defined at `chat.rs:19` and `search.rs:25`; `chat.rs:220` `has` is the byte-contains in search.rs (A). Fix: one small module for day and line helpers; read constant and contains next to `read_back_where` in `store.rs`. `rust-version` is not pinned, so `floor_char_boundary` needs the owner's minimum first.
14. **Small leftovers (R).** `task/stall.rs:296,311` `tail_line` re-reads the log tail that `TaskDir::last_entry` (`store.rs:689`) returns typed. The `AFTER_STALL_SECS` doc sits between the `BASH_MAX_SECS` doc and its const (`stall.rs:19-23`, b4ee9814). `view.rs:339` `task_agent` parses the whole log per open task (ledger.rs:73, wsx-web tasks.rs:201,518) to find one switch event; its comment "No effort" (:337) is stale. Fix: a `fallback` flag on the record, or drop the stale words.
15. **Docs (R).** `docs/daemon-discovery.md:8-15` omits `routines/` (`routine/store.rs:23`, afa7a9ad) and says two socket paths share no file, but `ops.rs:701-704` (53d217e9) keeps `shells/` records in the shared state folder. CLAUDE.md lists wsx-core modules without `fs_util`, `panel`, `routine`, `task`. The agent checked paths, flags and constants in panels.md and tasks.md and found no other drift (A).

## Order

```
1                      alone, first
9 -> 5 -> 4 -> 6       ledger.rs and status.rs hunks overlap
7                      with docs/tasks.md and scenario asserts
12 -> 2 -> 3           all in test-task.py; 3 needs the Rust knob
8, 10, 11, 13, 14, 15  independent
```

Not counted: long functions in `runtime/client.rs` and `ops.rs` that did not change since the tag; moving daemon-only routine and panel code out of the published crate (larger, optional).
