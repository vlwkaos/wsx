# Changelog

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

- Add project now uses recursive fuzzy search over git repos instead of directory-by-directory navigation — type a partial name (e.g. `wsx`) to find `~/ws-ps/wsx/` directly; scan runs in background with incremental results ([`9c67e15`](https://github.com/vlwkaos/wsx/commit/9c67e15))

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
- Expand state for projects with trailing slashes in config paths (`dgv3/`, `eyetrackpad/`) now persists correctly — paths are normalized on load ([`339713b`](https://github.com/vlwkaos/wsx/commit/339713b))
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
