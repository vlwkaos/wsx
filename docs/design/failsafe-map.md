# Failsafe map: what fails, who notices, who recovers

Read-only audit for task #477. The owner asked (projk chat, 2026-11-16) that each part of wsx fail safe and recover by itself.
Code: `main` at ebab168e (after #472). Evidence: the real store `~/.local/state/wsx/tasks`, read 2026-11-16.
Assuming: #474 (owner reminder), #479 and #486 (check-run answer fix) are not on `main`: their branches carry no commit past 18ff36b1 except #479 (open_asks, `task.json` broken list). Their code was not read. Nothing here was run: "test" names the scenario whose text asserts the case.
Proof rule: file:line, a store row, or the word *hypothesis*. Lines are from a code read at ebab168e.

## Verdict

Most parts recover from a crash. Six gaps remain. Ranks 1, 3 and 6 stop without a word. Ranks 2, 4 and 5 lose data:

| Rank | Gap | Harm | Proof |
|---|---|---|---|
| 1 | An owner wait has no timer, no reminder and no named listener | Silent stop, up to a day | projk #5 waited 21 h on a dyld error (projk chat row 2026-11-16 08:51). 13 `check_run` rows came after `stopped by request` with no message between, in 8 tasks. 10 of 48 stops waited over 60 min for the next row (a stop may be on purpose, so this is a ceiling) |
| 2 | A busy manager rotation drops open threads | Lost decisions | 4 of 14 replayed rotations lost them (`manager-event-model.md:170-182`). 339 `rotated` rows in 17 chats, 151 of them idle rotations. No fix on main |
| 3 | The repo service has no watchdog | Silent stop of every event | A dead or hung service is replaced only on demand or after a deploy (`owner.rs:77-79`, `:104-112`). No kill test |
| 4 | A write error is one stdout line | Lost record, log or answer | `host.rs:4229-4234`, `manager.rs:2203`, `repo.rs:414` print and go on. No alert, no ENOSPC test |
| 5 | A corrupt small file is skipped or defaulted, or stops a host | Wrong agent or home chat; chat host or repo service down | rows 13-15 below |
| 6 | A tool or permission lost after an upgrade shows only as a worker's own error | Hours until the owner reads it | rows 29-31 |

## Rows

Short names: `S` = `crates/wsx-core/src/task/store.rs`, `H` = `crates/wsx/src/task/host.rs`, `R` = `crates/wsx/src/task/repo.rs`, `M` = `crates/wsx/src/task/manager.rs`, `C` = `crates/wsx-core/src/runtime/client.rs`, `T` = `scripts/test-task.py` (section = `mark('<id>')` line), `RS` = `scripts/runtime-smoke.py`.

### 1. wsxd crash, restart, deploy handoff

| # | Part | Notice | Recover | Test | Gap |
|---|---|---|---|---|---|
| 1 | wsxd killed | Client connect error NotFound, Refused, Reset, Eof (`C:2474-2480`); TUI notice "Runtime disconnected; retrying" (`ui/notice.rs:41`) | `ensure_background_available` (`C:2675`), bootstrap flock (`C:1821-1862`), cold recovery relaunches each pane from its recipe, resumes an agent with an id (`recovery.rs:5-111`, `:148-157`) | RS:1610-1688 (kill, relabel survives, pane not exited); `an_attached_tui_hears_of_a_daemon_that_died` (`wsx-daemon/tests/live_handoff.rs`) | PTY children and scrollback are lost (hypothesis: no signal handling found). Start budget is 3 in 60 s, then `crash_loop` (`C:1897-1900`) and a manual `wsx daemon recover` |
| 2 | `state.json` torn | `load_one` fails to parse (`state_store.rs:389-400`) | Valid `.backup` loads, the bad primary moves to `state.json.corrupt.<ms>`, TUI prints a notice (`state_store.rs:56-80`, `app.rs:1072`) | RS:1833-1850; `invalid_primary_falls_back_to_valid_backup_and_is_quarantined` (`state_store.rs:485`) | Primary and backup both bad: `Err`, wsxd does not start (`state_store.rs:60-66`). No test. Auto-started wsxd has stderr on `/dev/null` (`C:1702`), so the client says only "did not become ready" after 5 s (`C:1757-1765`) |
| 3 | Deploy handoff fails | Each step rolls back and records `last_handoff_error` (`live_handoff.rs:62-64`, `:256-260`) | `rollback_handoff` rebinds the socket and the source keeps serving (`live_handoff.rs:236-273`) | RS handoff sections 2-5 (fault `WSX_HANDOFF_FAULT=after_commit`, `server.rs:154-156`); `a_failed_handoff_keeps_its_reason` (`live_handoff.rs:1059`) | A successor that is alive but silent past 30 s counts as success (`handoff.rs:343-345`). `let _ = fs::remove_file(socket)` ignored (`live_handoff.rs:182`) |
| 4 | Daemon state save fails (disk) | One stderr line, then `durability_degraded` on requests (`state_store.rs:337-343`, `dispatch.rs:16-19`) | Saver keeps the bytes and retries every 1 s (`state_store.rs:173`, `:344-355`) | `a_failed_save_answers_durability_degraded_and_the_saver_retries` (`tests.rs:4334`; forced with a directory, not ENOSPC) | The line is invisible (stderr null). Retry has no limit and no alert. A crash before space returns loses the unsaved change (hypothesis) |

### 2. Hosts, repo service, silent turns

| # | Part | Notice | Recover | Test | Gap |
|---|---|---|---|---|---|
| 5 | Hung or silent turn, task host | `Turn::left` (`H:581`, `:604`) against `turn.json` deadline `(checkTimeoutSeconds+600) s` (`stall.rs:239`) | `turn_hung` stops the agent, task `failed`, manager wakes (`H:2696`, `:1894-1905`, `manager.rs:302` in core) | T `S3` (:7796-7808), `27q` (:4521) | None found |
| 6 | Hung or silent turn, chat host | 1 s tick (`M:413`) | `end_hung_turn` holds, retries, alerts `bad` on attempt 1 (`M:1225`, `:1913`) | T `39b` (:8236), `39c` (:8259) | `turn.json` of a chat is read by nobody else: the stall sweep covers task folders only (`stall.rs:46-50`) |
| 7 | Older host that hangs | `replace::sweep` (`replace.rs:200`): output older than the bound, answers Status, older binary | Close session, `stop_tree`, log `stopped`, start a new host (`replace.rs:118-140`) | T `40` (:8101) | Runs only after a deploy or a repo-service start (`cmd/deploy.rs:501`, `R:201-205`). A hung host of the current binary is never touched. Posts nothing (`replace.rs:5`) |
| 8 | Task host killed | Repo sweep says `host_gone` (`stall.rs:147-158`); any caller (`call_task_host`, core `mod.rs:914-980`) | The caller restarts the host; `resume_state` (`H:878-941`): Working becomes an owner wait "restarted during a turn", Checking reruns, limit wait re-reads the record | T `39` (:8055), `32c`, `36` | Nothing restarts it unprompted (`docs/tasks.md:146`). Lost: pending adds, `notices`, the check-run (own doc: `check-run-lost-answer.md`). A wedged host that still holds its socket is never replaced (hypothesis) |
| 9 | Disk wait across a host restart | Stall rule: free space is back but no resume (`stall.rs:169-175`) | none | none | `resume_state` sends `Waiting`+`Disk` to the `state =>` arm (`H:936`), so `disk_wait` stays `None` and the poll never arms (`H:558-561`). The stall is only a `wakes=false` note (`stall_wakes`, core `manager.rs:335`). Code read, not run |
| 10 | Chat host killed | Next `say`, event or sweep needs it | `restore` requeues messages never delivered or delivered without a reply (`M:565-607`); `ensure_manager` (`owner.rs:29-70`) | T `21b` (:2569, kill -9) | No timer restarts it. A named chat has no kill test (`replace.rs:237`) |
| 11 | Repo service killed or hung | Next `ensure_repo`; `Status` times out at 15 s (`owner.rs:17`) | Restart on demand (`ensure_repo`, `cmd/new.rs:481`, `R:1519`); `repo.json` restores `seen`, stalls, check-ins (`R:426-446`) | T `21a` (:2816, check-in kept) | No heartbeat or watchdog. Hung: replaced only by a sweep after a deploy (`owner.rs:77-79`, `:108`). Lost on restart: outbox, `stall_suspects` (grace restarts). One thread runs probes and git in `tick` (`R:300-311`, hypothesis for a hang). No kill -9 or SIGSTOP test |

### 3. A lost or corrupt file

All task-store records use `store::write_atomic` (temp, fsync, rename; `S:318`, `fs_util.rs:80`), so none is torn by a crash. Reading a bad file differs:

| # | File | On a parse error | On a write error | Test | Gap |
|---|---|---|---|---|---|
| 12 | `task.json` | `read_record` fails (`S:655`). A host at start posts one alert, deduped 10 min (`H:354`, `:385`). #478 reads an older `manager` creator (`S:667`) | `H:4229` prints, keeps the memory copy | `a_host_that_cannot_load_its_record_posts_one_alert` (`wsx/tests/task_launch.rs`) | Alert only at host start. Readers elsewhere skip it (`view.rs:467`, `stall.rs:381`). #479 lists it as broken: not on main |
| 13 | chat `state.json` | `Manager::run` returns the error (`M:243`): the chat host does not start, no alert | `M:2203` prints | none | Chat dead until the owner looks (hypothesis on the owner path) |
| 14 | `repo.json` | `restore` returns the error (`R:427`): the repo service ends. The fallback to a chat `state.json` is for a missing file only (`R:431`) | `R:414` prints | none | Check-ins, recap and stalls stop with no alert (hypothesis) |
| 15 | `home.json`, `chat.json` | `main_chat` gives `None`, `chat_file` gives the default (`chats.rs:261-266`) | Propagated | `T 33m` (:7045, valid shape) | Silent: with no main chat events are dropped (the tasks stay in the ledger); a chat loses its agent and repos (hypothesis) |
| 16 | Task and chat `log.jsonl` | `read_log` skips bad lines (`S:712-730`) | `append` fails, host prints (`S:707`, `H:4303`, `M:2209`) | none | `append` does not end a torn tail, so the next event glues onto it and both are lost (`S:689-710`). Memory heals this (`memory/store.rs:312-318`) |
| 17 | `approvals.jsonl` | `read_approvals` skips bad lines (`S:1571`) | `append_approval` returns `Err` (`S:1534-1566`) | T `32e` (:5926, valid path) | Same glue as row 16; the id scan reads parseable lines only (`S:1553`), so an id can repeat (hypothesis) |
| 18 | `turn.json` | `Turn::read` gives "no turn" (`stall.rs:323`) | `eprintln` (`stall.rs:333`) | T `27m` (:4516, hand-written valid file) | A bad file hides a silent worker from the sweep (hypothesis) |
| 19 | Memory records | `fold` skips, `load` prints a count (`memory/store.rs:273`) | `sync_data?` fails the add (`memory/store.rs:328`); a partial line is healed on the next append | `test-memory.py:484-493` | ENOSPC not simulated |
| 20 | `notes/` views | `read_view` `.ok()`; edited view becomes `.drift` (`manage.rs:297`) | prints, record stays (`manage.rs:626`) | `test-memory.py:363-369` | None: views are derived |

### 4. Handover that drops facts

| # | Part | Notice | Recover | Test | Gap |
|---|---|---|---|---|---|
| 21 | Manager rotation (idle rule 60k tokens, `M:1617`) | Nobody: `Rotated` row and a divider (`M:2123-2136`) | A fresh conversation gets the notes preamble, 10 KiB (`notes.rs:147`), the last 4 exchanges at 600 chars (`M:93`, `:98`), routine results, ledger summary, open owner asks, the handover as a memory episode (`M:2183`) | T `21a` (:2599), `13r` (:1658, worker) | #464 replay: 4 of 14 lost pending acts, review findings, open owner decisions (`manager-event-model.md:170-182`). Written handover (#456) tied on idle only and is not shipped. No test for open threads |
| 22 | Limit hold across a rotation | `context_tokens = 0` on rotate (`M:2132`) | Fixed: the retry no longer rotates again | T `21a` (:2599; `manager-continuity.md:116`) | None |

### 5. A wait nobody sees

| # | Wait | Who is told | Timer | Test | Gap |
|---|---|---|---|---|---|
| 23 | Owner wait (blocked result, restart, stop) | Owner sees Needs you (`needs.rs:41`); manager skips it (`stall.rs:70`) | none | none | Rank 1. #474 not on main |
| 24 | `stopped by request` and its check-run answer | Manager is not woken (`manager.rs:323` in core); the answer sits in memory (`H:769`) | none | T `32t1` (:5336) | Own doc. #486 builds the fix, not on main |
| 25 | `after` chain | Closed parent: one event (`R:1386-1410`); 24 h: one stall (`stall.rs:27`, `:55`) | one report at 24 h | T `32af` (:5654), `32ai` (:5815) | A healthy parent that never lands is reported once |
| 26 | `ci:` check that never answers | `timed out` result (`H:4450`); retry event up to 3 (`H:72`, `:4556`); stall past the limit (`stall.rs:178`) | `CI_TIMEOUT` 125 min (`H:223`) | T `32t6` (:5477), `test-ci-scenario.py:103` | A hung `gh` call is unbounded (hypothesis) |
| 27 | Limit wait | Reason line with reset time (`limit.rs:39`); owner Needs you; manager not woken (core `manager.rs:322`) | `resume_at` 5 min to 1 h, probe every 30 s (`limit.rs:6-24`) | T `27b` (:4608), `27c` (:4662), `36`, `37` | A probe that cannot spawn returns without a word (`claude.rs:917`). 38 OAuth stop rows over 21.6 h in the wsx chat (2026-11-09 to 11-10); whether the owner saw each is not in the log |

### 6. Disk, tools, permissions

| # | Part | Notice | Recover | Test | Gap |
|---|---|---|---|---|---|
| 28 | Disk low before a turn | `low_disk` against `tasks.minFreeGb` (`H:2768`, `:2431`); a check that says out of disk (`H:3467`) | Queue, poll 30 s, one task at a time (`H:2445-2470`) | T `27` disk part (:4443-4477) | Writes after the gate ignore ENOSPC: rank 4. Real case: wsx chat could not start its manager, "No space left on device", 2026-10-20 20:23 KST |
| 29 | `claude` or `pi` missing | "cannot start {program}; is it on PATH?" (`agent.rs:724`); task `Failed` (`H:1504-1518`) | Manager retries every 30 s (`M:947-960`) | none | A worker needs a manual add. No preflight, no `doctor` (no match in the crates) |
| 30 | Binary dyld failure after upgrade (projk #5: Homebrew moved ggml, `llama-server` failed) | The process exits at once; wsx never starts `llama-server` and only probes its URL (`floor.rs:68-160`) | none | none | The worker blocked and waited 21 h on the owner (row 23). A replaced wsx binary re-execs (`H:814`); a failed exec is an owner wait (`H:510`) |
| 31 | Missing macOS permission (#476: agents get EPERM on `/Volumes/EXTDRIVE`) | None: wsx has no TCC code (`docs/design/tcc-volumes.md`, "Cause") | Owner grants the responsible app and restarts wsxd | none | Cause is by elimination, not proven. Smallest fix: a probe in `run_daemon` (not built) |

## Past failures in the real store

| Source | Count or row | Reads as |
|---|---|---|
| Task `log.jsonl`, every task folder of every project | 66 `stopped`: 52 by request, 7 "agent process ended", 7 "older wsx" (turns 39 min to 1 d 2 h old) | The older-host sweep found hours-old hung turns (row 7). The next row followed in 0.0-0.1 min: the sweep restarts the host at once |
| Same | 13 `check_run` rows after `stopped by request` with no `message` between: tasks 124, 125, 234, 235, 236, 239, 242, 482 | The lost answer (row 24) happened 13 times, not once |
| Same | 0 unparsable lines | A torn tail has not happened yet (rows 16-17 stay hypothesis) |
| Chat logs | 339 `rotated`: 151 idle, 114 "instructions changed" (research chat), 74 other | Rotation is routine, so row 21 repeats |
| Chat `wsx`, 2026-10-20 | `stopped`: "cannot start the manager agent: cannot write ...skills-plugin/.claude-plugin: No space left on device" | Row 28: disk full seen once |
| Chat `projk`, 2026-11-15 11:19 and 2026-11-16 08:51 | Homebrew replaced `ggml`; `llama-server` failed in dyld; "projk #5 has waited on the owner for 21 h" | Rows 23 and 30 |

Commands: `python3` over `~/.local/state/wsx/tasks/*/[0-9]*/log.jsonl` and `chats/*/log*.jsonl`, grouping by `kind` and `reason`.

## Design steps (at most 5)

| # | Step | Owner module | Proof it needs |
|---|---|---|---|
| 1 | One `listener(WaitOn)` match: every wait names who hears it (manager, owner reminder, timer). An exhaustive match makes a new wait without a listener a compile error. #474 and #486 become two arms | `wsx-core/src/task/stall.rs`, `manager.rs` (`state_wakes`) | A `T` section that parks a task on each `WaitOn` and asserts an event or a reminder within the bound; a row of the real store with a wait older than the bound before the fix |
| 2 | Open threads survive a busy rotation: measure the written handover on busy rotations before shipping it | `crates/wsx/src/task/manager.rs` (`rotate`), `notes.rs` | `bench-manager.py --replay` on the 14 cases: 0 lost threads (it was 4) |
| 3 | A repo-service watchdog: a chat host with a live turn, and `task new`, replace a service whose `Status` fails twice | `wsx-core/src/task/owner.rs` (`ensure_repo`), `manager.rs` tick | A `T` section that kills -9 and one that SIGSTOPs the repo host; events flow again within 30 s |
| 4 | One reader and writer rule for small files: a parse error moves the file to `<name>.corrupt.<ms>`, loads the default and posts the `alert_unreadable` alert; `append` ends a torn tail as memory does; a write error posts one alert | `wsx-core/src/task/store.rs`, `chats.rs`; hosts in `hosting.rs` | A `T` section per file: corrupt it, assert one alert and a running host; truncate a log and an approvals file, append, read both lines; a fault hook like `WSX_HANDOFF_FAULT` for ENOSPC |
| 5 | A preflight at wsxd start and at each agent start failure: resolve `claude`, `pi`, `git`, run `--version` for a dyld error, probe `/Volumes` for TCC (the #476 fix); show it on the `this-mac` panel and in the home chat | `wsx-daemon/src/server.rs`, `machine.rs`; `crates/wsx/src/task/agent.rs` | A fake `claude` that prints a dyld error: the alert names the library. A TCC denial needs the owner's Mac (`human_verify`) |

Order by harm: 1, 2, 4, 3, 5. Step 1 first because it ends rank 1 and pins rank 6's silence to a listener.

## Not proven

- Rows 9, 13, 14, 15, 17, 18 name a result from reading code, with no run.
- #474, #479 and #486 are described from the branch list and the owner's note; their code was not read.
- Whether a stop is on purpose is not in the log, so the 10 long stops and the 13 lost answers are counts of rows, not counts of harm.
