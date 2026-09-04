# Changelog

## [Unreleased]

### Breaking Changes

- Reserve `a` and `i` as two-chord Terminal command suffixes; configurations using either as the Workspace-focus suffix must choose another key.

### Features

- Add prefixed next/previous idle and active session iteration in Terminal mode while preserving unprefixed PTY input.

### Bug Fixes

- Reject nested TUI startup inside wsx-managed terminals with an actionable error while keeping explicit CLI subcommands available.
- Limit startup integration prompts to detected agent executables and return explicitly aborted Pi turns to idle instead of marking them done.
- Keep Pi sessions working across automatic continuations by reconciling deferred settlement with Pi's authoritative idle state.

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
- Replace per-key request/handshake round trips and 500 ms full-frame polling with a persistent duplex terminal stream, bounded asynchronous input, event-driven Ghostty dirty-row updates, and an 8 ms foreground wake and producer cadence. Add a production-equivalent direct-PTY versus wsxd-stream-to-wsx-render latency gate that reports narrow, wide, and erase-rewrite p50/p95 values and enforces a 16.7 ms added-p95 budget.
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
