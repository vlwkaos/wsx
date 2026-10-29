# UI state ownership

## Outer terminal

The wsx TUI thread owns raw mode, alternate screen, mouse capture, bracketed paste, cursor state, and synchronized rendering. A scoped guard captures owned handles for the initialized output surface and input terminal, plus the original configured kernel attributes, before the first mode change. It restores them on normal exit, startup error, partial initialization failure, and owner-thread panic, including redirected stderr. Cleanup attempts every restoration step even if an earlier one fails.

A worker panic does not release the outer terminal. Its bounded, control-free failure message returns to the TUI owner for display instead of writing panic output into the live frame. An editor borrows the terminal temporarily. Its baseline kernel attributes return before crossterm re-enters raw mode, even if the editor changes those attributes and fails. Both successful and failed editors return the modes to the same owner. Failure to regain those modes terminates the TUI through normal cleanup.

The daemon still owns child PTYs, Ghostty selection, leases, and frames.

## Mode recovery

The terminal can lose the modes without telling wsx. A terminal reset (for example Ghostty's `reset` action) clears the screen and every mode and sends no event. After a sleep, mouse capture can be gone, so the wheel scrolls the terminal and a drag selects across the whole window; that producer is not identified. ratatui then diffs against its old buffer and writes little or nothing.

`tui::enter_modes` is the one function that writes every output mode wsx owns: alternate screen, mouse capture, bracketed paste, focus reporting and the scroll margin. Each sequence sets a state, so the function is safe to repeat. `tui::draw_sync` calls it and then draws a full frame, inside one synchronized update, for a `ModeCheck` the event loop chooses:

| Trigger | `ModeCheck` | Why |
|---|---|---|
| Resume boundary (suspend detector) | `Reassert` | the repair must not depend on the unknown producer |
| Focus gained (`CSI I`) | `Reassert` | a wake or an unlock normally returns focus |
| Resize | `Reassert` | terminals reset the scroll margin on resize |
| No terminal event for one second, at most once a second | `Probe` | a reset sends no event |

The probe cannot use DECRQM. crossterm 0.28.1 keeps a reply that starts with `CSI ?` in its parse buffer until a `u` or `c` byte arrives (`event/sys/unix/parse.rs`, `parse_csi`), so `CSI ? 1000 ; 2 $ y` would swallow the keys typed after it. A raw read of the terminal beside crossterm can take a key that arrives with the reply and cannot give it back. The only reply crossterm returns to its caller is the cursor position, so the probe reads an owned state through it:

1. `enter_modes` sets the scroll margin to all rows except the last (`CSI 1 ; rows-1 r`). wsx addresses every cell and never scrolls, so the margin changes no frame.
2. The probe moves the cursor to the margin line, sends cursor-down and reads the position with `crossterm::cursor::position()`. With the margin the cursor stays; after a reset it reaches the last row.
3. The frame that follows in the same synchronized update places or hides the cursor again.

Bounds:

- At startup wsx sends one device-attributes query (`CSI c`, a reply crossterm consumes) and waits at most 150 ms for input. A terminal that stays silent is never probed.
- A terminal that answers the first query but not a cursor report costs one wait of crossterm's two seconds, then no more probes.
- A probe that finds the margin gone directly after a repair means the terminal does not keep the margin. wsx repairs twice at most, then stops probing. Two real resets within two seconds end probing for that window in the same way.
- The probe waits for its reply before the frame, so no reply is in flight when wsx quits or starts an editor.
- A probe-detected repair is recorded as `terminal_reset_repaired`. Event-driven reasserts are not recorded.

The probe finds a reset, which returns the margin to the full screen. It does not find a terminal that drops only mouse capture; resume, focus and resize cover that case without a query.

Ghostty's `keybind = ctrl+shift+r=reset` with `chain=text:\r` also sends Enter after the reset. wsx receives it as an ordinary key: Workspace toggles the selected project or worktree or enters the selected session, Terminal mode sends it to the program in the pane, and a form or confirm dialog accepts. wsx does not filter it, because nothing distinguishes it from an Enter the user typed.

## Recurrence evidence

`ui-terminal-diagnostics-v1.jsonl` in the platform wsx cache directory retains at most 64 KiB of metadata shared by repaired clients. Records contain time, process/thread identity, source file/line, ownership/editor transition, output-device identity, available owned-terminal kernel flags, and OS error number. Panic records retain the actual panic site, not the hook helper; initialization and editor re-entry failures retain their specific phase. App startup/run and final cache-flush errors leave their reporting phase after terminal restoration, without recording their message contents. They exclude screen text, prompts, keystrokes, and panic payloads. The file is owner-only; symlinks, non-regular files, extra hard links, unsafe permissions, and unsafe parent directories are refused. A nonblocking lock serializes append and bounded truncation. Journal errors never change terminal ownership or turn cleanup into a recovery loop.

For a recurrence, retain the journal promptly and note the affected window and time. Compare its ownership and editor transitions with the visible worker notice. This can attribute a wsx-owned teardown or failed re-entry to its source phase. It does not observe an external program's escape sequences, prove the terminal emulator's mouse mode, or name an external producer. Missing records can mean journal I/O failed; absence is not proof that no transition occurred. Controlled faults verify this evidence path, not the spontaneous trigger.

## Durable UI intent

`wsx-core::cache::WorkspaceCacheChanges` applies explicit per-key commands to the latest saved state under one bounded, owner-only file lock. Refresh and quit do not publish a complete window snapshot. Removals are commands too, including cleared stale flags, unmute, dismissed-integration reset, and adaptive-window removal. Acknowledgements never regress a terminal's saved outcome revision.

Automatic collapse and adaptive updates carry the project interaction time they observed. The cache owner rejects them if another client has saved a newer interaction. Adaptive commands contain only the base and activity time, never a caller's derived credits or window. The locked owner earns credit against the latest durable state and rejects collapse while that window remains active. A successful write returns the submitted projects' committed state, including rejected decisions, so the caller adopts the actual window and expansion instead of repeating an old decision. Project interaction, expansion, provenance, and adaptive changes publish in one transaction. Unrelated worktree or terminal changes preserve other keys. Worktree expansion loads once when asynchronous discovery first creates the row.

Commands remain pending if locking, reading, parsing, or publication fails. A failed quit reports unsaved intent with a nonzero exit instead of claiming success. A busy lock has a 100 ms bound, and the TUI retries instead of dropping intent. Quit fsyncs the already-published cache even when there are no new commands. Lock files are stable ownership fences; do not remove them while clients are running.

The canonical cache is `workspace-v3.toml` in the platform wsx cache directory. On first use it imports `workspace-v2.toml`, falling back to `workspace.toml`. Import does not rewrite the predecessor. Later reads use only v3, so older clients with whole-snapshot writers cannot undo repaired-client state. Group selection remains independently persisted. Each write refreshes its affected projects from the committed state. Unrelated windows and rows do not live-sync.

The old core snapshot-write API is replaced by explicit changes. `WorkspaceCacheChanges::save(path, sync)` returns `ProjectCacheState` per submitted project. `AppliedCache` additionally returns saved worktree expansion for deferred discovery. Wire protocol, daemon revision, terminal leases, and configuration schema do not change.

## Test isolation

A TUI reads the whole cache only at startup, including the start after it follows an upgraded wsxd, and adopts committed state only for projects it submits. It never polls the file. So another writer changes what a running TUI shows only through the next start, or through the daemon both of them use.

The App owns both: `App::new` resolves `cache::workspace_cache_path()` once into `App.cache_path` and creates `App.runtime_client` once. Every cache read and write and every wsxd call made by the App uses them. The `ops` functions that call wsxd take a `&Client`. `runtime::ensure_available` and `ensure_background_available` refuse a custom client (`Client::new`), so only `Client::local` starts or replaces a daemon. A test App gets a fresh file under `.work/tests/workspace-cache/`, named after its test, and a client for a socket that does not exist.

Before, `persist_state` wrote the platform cache and the App's refresh threads used `Client::local()`. A test run in a wsx pane (same HOME, inherited `WSX_SOCKET`) then wrote its cursor, folds and dismissals into the user's cache, and its `SynchronizeProjects` with the test config removed the user's projects and worktrees that had no sessions. On macOS `dirs::cache_dir` and `dirs::config_dir` follow `HOME`, not `XDG_*`. `scripts/test-ui-state.py` runs the wsx unit tests beside a live TUI and wsxd that share their HOME and socket, and checks that the cache bytes, the daemon's worktrees and the viewed session after a restart do not change.

## Selected row

A runtime refresh and the timed collapse of stale projects keep the cursor on the same row by its identity (`App::cursor_identity` before, `App::restore_cursor` after, in `apply_projected_snapshot` and the slow timer), not by its row number. A snapshot older than one the window already applied, or than a restart it sent, is not applied. Task hosts open and close sessions and worktrees all the time, so a row number above the viewed session moved the view to another session by itself; in Terminal mode the title then named that session while keys still went to the open pane. Only when the selected row is gone does the cursor stay at the row number, clamped to the list. `scripts/runtime-smoke.py` drives a TUI in a PTY: a session above the viewed one closes, a new one opens below it, wsxd hands off, and the cursor in the cache stays on the viewed session.

## Runtime refresh

The TUI event monitor (`EventMonitor` in `wsx-core` runtime/client.rs) asks for a new snapshot only for an event other than `terminal.changed`. The daemon publishes `terminal.changed` for every PTY read, and no snapshot field derives from output: foreground jobs have `pane_activity.changed`, exits have `exited`. A busy pane in any project used to make every TUI fetch a snapshot over a new connection and apply it on the UI thread several times a second. The web server still reads `terminal.changed` for questions and activities. The daemon's accept loop waits in poll(2) on its socket, so a new connection gets its hello at once instead of after a sleep of up to 20 ms. `scripts/runtime-smoke.py` counts the TUI's draws while one pane prints without pause (before: about 10 a second, now none) and times a new connection's hello.

## Folded status

Folded project and worktree rows summarize every descendant pane using normalized lifecycle, acknowledged outcomes, and reported foreground-job metadata. The existing context priority remains blocked/error, done, active, then idle/unknown/muted, with workspace order breaking ties. Active counts count sessions, not panes, and remain visible beside a higher-priority outcome. Folded badges use only the dominant indicator and active count, such as `◐ 1`, without the word `active`; the count keeps its Working color. Exited panes do not count as active. `stale` remains separate stored inactivity provenance, but its visible projection excludes projects with raw active sessions. Those projects also reject automatic inactivity collapse even when their last activity timestamp has expired. Neither guard expands manually folded projects, rewrites stored provenance, or grants activity credits from foreground-job metadata. Inactive projects retain their existing provenance behavior.

The projection reads the existing terminal-keyed local mute set, including unfocused panes. A muted pane does not incorrectly dominate attention; raw active counts remain independently visible. The projection performs no I/O or mutations. Folding only changes presentation. Narrow rows reserve space for status before truncating identity; tiny rows retain the dominant symbol and an active marker when space permits. The one-cell compact rail retains the dominant symbol. Session rows reserve width for the reported provider label before truncating a long name or adding ports. Missing provider identity stays missing; rendering never infers it.

## Human Verify

Pass: folded rows remain scannable, active projects have no visible stale label, and reported providers remain readable when their labels fit. Fail: identity or status feels unclear despite the tested narrow-width projection. Visual feel remains a human check.

## Verification

Build fresh adjacent client and daemon binaries, then compile the wsx test executable:

```sh
cargo build -p wsx -p wsx-daemon --locked
cargo test -p wsx --locked --no-run
cargo test -p wsx-terminal --lib --locked --no-run
python3 scripts/test-ui-state.py --test-binary target/debug/deps/wsx-<test-hash> \
  --render-binary target/debug/deps/wsx_terminal-<test-hash>
```

The scenario observes cursor-addressed output through the existing Ghostty engine, not raw substring matching. It retains folded screen/frame evidence beside the receipt. The scenario uses an isolated HOME, configuration, state, project, daemon, and real PTYs. It checks two-window clear/refresh/quit/restart in both exit orders, legitimate later collapse, one-time legacy import, old-writer isolation, real TUI drag through daemon selection and clipboard, worker panic without mode reset, failed editor return, owner panic with shared or redirected stderr, startup failure, partial initialization failure, configured kernel-attribute restoration after a real failed editor, and child/socket cleanup. The Darwin oracle excludes only `PENDIN`, which XNU sets and preserves when returning to canonical input; settings, speeds, and control characters still match their original values. It also verifies private bounded mode records across worker failure, failed editor, normal cleanup, and owner panic. One view answers device-attribute and cursor queries as a terminal that keeps the scroll margin; clearing that margin stands for a terminal reset, and the scenario requires one full repair, no repeated repair, a reassert on focus gained, and no probe of a view that answers nothing. It makes no model calls. A compact JSON receipt remains under `.work`; synthetic runtime fixtures are removed.

Core cache tests exercise concurrent processes, automatic-collapse fencing, removals, revision acknowledgements, unreadable state, bounded lock contention and retry, and unsafe lock-file rejection. Folded-status tests cover hidden panes, exits, mute, acknowledged outcomes, mixed states, and actual tree rendering at tiny/narrow/normal widths. The PTY journey checks an independently advancing heartbeat and unchanged runtime generation beneath project and worktree folds.
