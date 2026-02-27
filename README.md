# wsx

TUI workspace manager for git worktrees and tmux sessions.

<!-- screenshot -->
![Screen Recording 2026-02-27 at 9 00 58 AM_1](https://github.com/user-attachments/assets/325dfaca-5f18-458b-944f-ce143e32cd51)

## The core idea

Keep a live view of every project → worktree → session in a sidebar. Each session shows real-time state so you can see what needs attention without entering it.

**Session icons**

| Icon | Meaning |
|------|---------|
| `◉` green | Actively producing output |
| `●` yellow | Needs attention — bell fired, or a non-passive process went quiet |
| `○` gray | Idle |
| `⊘` | Muted |

The yellow `●` fires on tmux bell activity *or* when a foreground process that isn't a shell or known passive watcher (dev server, file watcher) goes quiet. Press `n` to step through pending sessions, `x` to dismiss or mute.

**Worktree git state**

| Icon | Meaning |
|------|---------|
| `~` prefix | Main (original) worktree |
| `*` yellow | Uncommitted local changes |
| `↑N` cyan | N commits ahead — ready to push |
| `↓N` red | N commits behind — pull before working |
| `↓N↑M` magenta | Diverged |

Remote state is fetched in the background and updates silently. The preview pane shows full detail: remote branch name, sync status, modified files, recent commits.

## Guide

| Feature | Screenshot |
|---|---|
| **Project config** `.gtrconfig` at repo root — post-create hook, auto-copy env files into new worktrees. Press `e` to view. | <img width="473" height="245" alt="image" src="https://github.com/user-attachments/assets/41a1ef82-9ebb-49aa-993e-4ae9f1ea0a83" /> |
| **Add project** Press `p`, enter a path. Tab-completion supported. | <img width="457" height="221" alt="image" src="https://github.com/user-attachments/assets/b6c0c7bf-7252-4281-bee4-8dfa4c8d4529" /> |
| **New worktree** Select a project, press `w`, enter a branch name. | <img width="459" height="52" alt="image" src="https://github.com/user-attachments/assets/8280c712-29a1-43d6-8504-0c7161ab9b86" /> <img width="264" height="90" alt="image" src="https://github.com/user-attachments/assets/c8183cf6-4de8-414a-88e2-1ceac1722080" /> |
| **Sessions** Select a worktree, press `s`. Name by context — `shell`, `claude`, `build`. Sessions are persistent tmux sessions; `d` deletes, `r` renames. | <img width="270" height="68" alt="image" src="https://github.com/user-attachments/assets/41569337-057f-44b8-bd39-8f1d2ffa6a1f" /> |
| **Iterate pending** `n` / `N` to jump between `●` sessions. `x` dismisses; press again to mute `⊘`. `a` cycles active `◉` sessions. | ![Screen Recording 2026-02-27 at 9 35 16 AM](https://github.com/user-attachments/assets/46c6b7be-34b2-4f73-b959-6205d81d1a66) |
| **Remote control** `S` sends a command to the selected session without entering it. `C` sends Ctrl+C — handy for killing a watcher the moment you spot it. | <img width="464" height="57" alt="image" src="https://github.com/user-attachments/assets/6d466d85-4d92-44c7-abe8-93ec4337f480" /> |
| **Detach to return** `Ctrl+a d` inside a session detaches back to wsx. The session keeps running. | |

## Install

```sh
brew tap vlwkaos/tap
brew install wsx
```

Or build from source:

```sh
cargo install --path .
```

> Must be run inside a tmux session.

## Usage

```sh
wsx
```

### Navigation

| Key | Action |
|-----|--------|
| `j/k` `↑/↓` | Move cursor |
| `h/l` `←/→` | Collapse / expand |
| `Enter` | Expand · attach session |
| `[` / `]` | Jump to prev / next project |
| `a` | Next active session `◉` |
| `n` / `N` | Next / prev pending session `●` |
| `x` | Dismiss · mute session |
| `/` | Incremental search |
| `?` | Full key reference |

Mouse clicks work: click a row to select, click the preview to attach.

### Workspaces

| Key | Action |
|-----|--------|
| `p` | Add project |
| `w` | New worktree |
| `s` | New session |
| `m` | Reorder project or session |
| `r` | Set alias |
| `d` | Delete |
| `c` | Clean merged worktrees |
| `e` | View `.gtrconfig` |
| `S` | Send command to session |
| `C` | Send Ctrl+C to session |

### tmux status bar

wsx sets `status-right` to `project/worktree` on attach. With a custom `~/.tmux.conf`:

```
set -g status-right "#{@wsx_project}/#{@wsx_alias}"
```

## Config

Global config: `~/.config/wsx/config.toml`. Per-project config via `e` key.

### .gtrconfig

```ini
[hooks]
  postCreate = npm install

[copy]
  include = .env
  include = .env.local
  exclude = .env.production
```

## Inspired by

- [git-worktree-runner](https://github.com/coderabbitai/git-worktree-runner)
- [agent-of-empires](https://github.com/njbrake/agent-of-empires)
