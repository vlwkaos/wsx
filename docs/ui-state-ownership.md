# UI state ownership

## Outer terminal

The wsx TUI thread owns raw mode, alternate screen, mouse capture, bracketed paste, cursor state, and synchronized rendering. A scoped guard captures owned handles for the initialized output surface and input terminal, plus the original configured kernel attributes, before the first mode change. It restores them on normal exit, startup error, partial initialization failure, and owner-thread panic, including redirected stderr. Cleanup attempts every restoration step even if an earlier one fails.

A worker panic does not release the outer terminal. Its bounded, control-free failure message returns to the TUI owner for display instead of writing panic output into the live frame. An editor borrows the terminal temporarily. Its baseline kernel attributes return before crossterm re-enters raw mode, even if the editor changes those attributes and fails. Both successful and failed editors return the modes to the same owner. Failure to regain those modes terminates the TUI through normal cleanup.

The daemon still owns child PTYs, Ghostty selection, leases, and frames. This repair adds no mouse recovery shortcut or periodic mode re-enable loop. A natural mouse-loss incident without a worker failure still requires its actual producer to be identified; controlled fault tests do not establish that trigger.

## Recurrence evidence

`ui-terminal-diagnostics-v1.jsonl` in the platform wsx cache directory retains at most 64 KiB of metadata shared by repaired clients. Records contain time, process/thread identity, source file/line, ownership/editor transition, output-device identity, available owned-terminal kernel flags, and OS error number. Panic records retain the actual panic site, not the hook helper; initialization and editor re-entry failures retain their specific phase. App startup/run and final cache-flush errors leave their reporting phase after terminal restoration, without recording their message contents. They exclude screen text, prompts, keystrokes, and panic payloads. The file is owner-only; symlinks, non-regular files, extra hard links, unsafe permissions, and unsafe parent directories are refused. A nonblocking lock serializes append and bounded truncation. Journal errors never change terminal ownership or turn cleanup into a recovery loop.

For a recurrence, retain the journal promptly and note the affected window and time. Compare its ownership and editor transitions with the visible worker notice. This can attribute a wsx-owned teardown or failed re-entry to its source phase. It does not observe an external program's escape sequences, prove the terminal emulator's mouse mode, or name an external producer. Missing records can mean journal I/O failed; absence is not proof that no transition occurred. Controlled faults verify this evidence path, not the spontaneous trigger.

## Durable UI intent

`wsx-core::cache::WorkspaceCacheChanges` applies explicit per-key commands to the latest saved state under one bounded, owner-only file lock. Refresh and quit do not publish a complete window snapshot. Removals are commands too, including cleared stale flags, unmute, dismissed-integration reset, and adaptive-window removal. Acknowledgements never regress a terminal's saved outcome revision.

Automatic collapse and adaptive updates carry the project interaction time they observed. The cache owner rejects them if another client has saved a newer interaction. Adaptive commands contain only the base and activity time, never a caller's derived credits or window. The locked owner earns credit against the latest durable state and rejects collapse while that window remains active. A successful write returns the submitted projects' committed state, including rejected decisions, so the caller adopts the actual window and expansion instead of repeating an old decision. Project interaction, expansion, provenance, and adaptive changes publish in one transaction. Unrelated worktree or terminal changes preserve other keys. Worktree expansion loads once when asynchronous discovery first creates the row.

Commands remain pending if locking, reading, parsing, or publication fails. A failed quit reports unsaved intent with a nonzero exit instead of claiming success. A busy lock has a 100 ms bound, and the TUI retries instead of dropping intent. Quit fsyncs the already-published cache even when there are no new commands. Lock files are stable ownership fences; do not remove them while clients are running.

The canonical cache is `workspace-v3.toml` in the platform wsx cache directory. On first use it imports `workspace-v2.toml`, falling back to `workspace.toml`. Import does not rewrite the predecessor. Later reads use only v3, so older clients with whole-snapshot writers cannot undo repaired-client state. Group selection remains independently persisted. Each write refreshes its affected projects from the committed state. Unrelated windows and rows do not live-sync.

The old core snapshot-write API is replaced by explicit changes. `WorkspaceCacheChanges::save` returns `ProjectCacheState` per submitted project. `AppliedCache` additionally returns saved worktree expansion for deferred discovery. Wire protocol, daemon revision, terminal leases, and configuration schema do not change.

## Folded status

Folded project and worktree rows summarize every descendant pane using normalized lifecycle, acknowledged outcomes, and reported foreground-job metadata. The existing context priority remains blocked/error, done, active, then idle/unknown/muted, with workspace order breaking ties. Active counts count sessions, not panes, and remain visible beside a higher-priority outcome. Exited panes do not count as active. `stale` remains separate inactivity provenance; it does not suppress the status badge or pause background execution.

The projection reads the existing terminal-keyed local mute set, including unfocused panes. A muted pane does not incorrectly dominate attention; raw active counts remain independently visible. The projection performs no I/O or mutations. Folding only changes presentation. Narrow rows reserve space for status before truncating identity; tiny rows retain the dominant symbol and an active marker when space permits. The one-cell compact rail retains the dominant symbol.

## Human Verify

Pass: folded rows remain scannable at your usual sidebar width, and activity is distinguishable from the stale label. Fail: identity or status feels unclear despite the tested narrow-width projection. Visual feel remains a human check.

## Verification

Build fresh adjacent client and daemon binaries, then compile the wsx test executable:

```sh
cargo build -p wsx -p wsx-daemon --locked
cargo test -p wsx --locked --no-run
cargo test -p wsx-terminal --lib --locked --no-run
python3 scripts/test-ui-state.py --test-binary target/debug/deps/wsx-<test-hash> \
  --render-binary target/debug/deps/wsx_terminal-<test-hash>
```

The scenario observes cursor-addressed output through the existing Ghostty engine, not raw substring matching. It retains folded screen/frame evidence beside the receipt. The scenario uses an isolated HOME, configuration, state, project, daemon, and real PTYs. It checks two-window clear/refresh/quit/restart in both exit orders, legitimate later collapse, one-time legacy import, old-writer isolation, real TUI drag through daemon selection and clipboard, worker panic without mode reset, failed editor return, owner panic with shared or redirected stderr, startup failure, partial initialization failure, configured kernel-attribute restoration after a real failed editor, and child/socket cleanup. The Darwin oracle excludes only `PENDIN`, which XNU sets and preserves when returning to canonical input; settings, speeds, and control characters still match their original values. It also verifies private bounded mode records across worker failure, failed editor, normal cleanup, and owner panic. It makes no model calls. A compact JSON receipt remains under `.work`; synthetic runtime fixtures are removed.

Core cache tests exercise concurrent processes, automatic-collapse fencing, removals, revision acknowledgements, unreadable state, bounded lock contention and retry, and unsafe lock-file rejection. Folded-status tests cover hidden panes, exits, mute, acknowledged outcomes, mixed states, and actual tree rendering at tiny/narrow/normal widths. The PTY journey checks an independently advancing heartbeat and unchanged runtime generation beneath project and worktree folds.
