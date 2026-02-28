# Changelog

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
