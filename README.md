# wsx

TUI workspace manager for git worktrees and tmux sessions.

<!-- screenshot -->
![Screen Recording 2026-02-27 at 9 00 58 AM_1](https://github.com/user-attachments/assets/325dfaca-5f18-458b-944f-ce143e32cd51)


## The core idea

Most terminal multiplexer workflows leave you hunting: which session was running that dev server? Which branch had the failing test? Did that long build finish?

`wsx` solves this by keeping a live view of every project → worktree → session in a sidebar, with each session showing its real-time state:

| Icon | Meaning |
|------|---------|
| `◉` green | Actively producing output right now |
| `●` yellow | Needs attention — bell fired, or a non-passive process went quiet |
| `○` gray | Idle |
| `⊘` | Muted — you don't want updates from this one |
| `✎` | Worktree has uncommitted changes |

The yellow `●` is deliberately semi-heuristic: it fires on tmux bell activity *or* when a foreground process that isn't a shell (and isn't a known passive watcher like `node`, `vite`, `tail`, `watch`) goes quiet — meaning it probably finished or is waiting for input. Passive long-runners like dev servers and file watchers never trigger it.

### Iterate pending sessions with `n` / `N`

Press `n` to jump to the next session with a `●` indicator, `N` for previous. This turns "check what needs attention" from a manual scan into a single keypress loop — step through only the sessions that actually want you.

Once you've handled a session, `x` dismisses the indicator (or mutes the session entirely with a second press). Active sessions (`◉`) can't be dismissed — there's nothing to dismiss yet.

## Guide

| Feature | Screenshot |
|---|---|
| **Project config** `.gtrconfig` at the repo root configures worktree setup — run a post-create command, copy env files into new worktrees automatically. Press `e` to view it. | <img width="473" height="245" alt="image" src="https://github.com/user-attachments/assets/41a1ef82-9ebb-49aa-993e-4ae9f1ea0a83" /> |
| **Create a project** Press `p`. Prompt will popup for a project directory. Suggestion per directory is supported. | <img width="457" height="221" alt="image" src="https://github.com/user-attachments/assets/b6c0c7bf-7252-4281-bee4-8dfa4c8d4529" /> |
| **Create a worktree** Select a project and press `w`. Enter a branch name. The worktree appears in the tree immediately. | <img width="459" height="52" alt="image" src="https://github.com/user-attachments/assets/8280c712-29a1-43d6-8504-0c7161ab9b86" /> <img width="264" height="90" alt="image" src="https://github.com/user-attachments/assets/c8183cf6-4de8-414a-88e2-1ceac1722080" /> |
| **Create sessions** Select a worktree and press `s`. Create one session per context — e.g. `shell`, `claude`, `codex`, `build`. If you skip naming, it will automatically use the command you entered. <br> Each is a persistent tmux session; closing your terminal doesn't kill it. You can press `d` for deletion and `r` for renaming. | <img width="270" height="68" alt="image" src="https://github.com/user-attachments/assets/41569337-057f-44b8-bd39-8f1d2ffa6a1f" /> |
| **Iterate pending** Press `n` to jump to the next yellow `●` session. No scanning — just step through whatever needs attention and handle it. `x` dismisses the indicator; press again to mute with `⊘`. <br><br> Similarly you can **Monitor active** by pressing `a` to cycle through green `◉` sessions — ones that are actively producing output right now. Useful for keeping an eye on running builds or agents. | ![Screen Recording 2026-02-27 at 9 35 16 AM](https://github.com/user-attachments/assets/46c6b7be-34b2-4f73-b959-6205d81d1a66) |
| *Send command without entering session*. Sometimes entering the session itself is a bit tedious. You can `S`(Shift+s) to send command directly or `C`(Shift+c) to send Ctrl+C sig. This is useful for closing watcher or runner directly on wsx screen | <img width="464" height="57" alt="image" src="https://github.com/user-attachments/assets/6d466d85-4d92-44c7-abe8-93ec4337f480" /> |
| **Detach to return** Inside a session, press `Ctrl+a d` to detach. wsx resumes and the session keeps running in the background. This is the primary way to move between sessions. | (empty) |

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
| `Enter` | Expand project/worktree · attach session |
| `[` / `]` | Jump to prev / next project |
| `a` | Jump to next active session `◉` |
| `n` / `N` | Jump to next / prev pending session `●` |
| `x` | Dismiss attention · mute session |
| `/` | Incremental search |
| `?` | Full key reference |

Mouse clicks also work: click a tree row to select it, click the preview pane to attach the focused session.

### Workspaces

| Key | Action |
|-----|--------|
| `p` | Add project |
| `w` | New worktree (branch prompt) |
| `s` | New persistent session |
| `m` | Reorder project or session |
| `r` | Set alias |
| `d` | Delete |
| `c` | Clean merged worktrees |
| `e` | View `.gtrconfig` |

### Remote control

wsx can send input to a session without entering it — useful when you want to stay in the overview while interacting with a running process.

| Key | Action |
|-----|--------|
| `S` | Open a prompt and send a command to the selected session |
| `C` | Send `Ctrl+C` to the selected session |

**`C`** is handy for killing a watch-mode process (file watcher, test runner, dev server) the moment you notice it in the sidebar — no need to switch in, interrupt, and switch back.

**`S`** lets you fire a command at a session in the background: start a build, run a migration, trigger a test suite — without losing your place in the tree.

Inside a session, use `Ctrl+a d` to detach and return to wsx.

### tmux status bar

When you attach to a session, wsx sets the tmux `status-right` to `project/worktree` so you always know where you are. If you have a custom `~/.tmux.conf`, the status bar is left untouched — but the values are still available as session options you can reference yourself:

```
# ~/.tmux.conf
set -g status-right "#{@wsx_project}/#{@wsx_alias}"
```

## Config

Global config at `~/.config/wsx/config.toml`. Per-project config via `e` key.

### .gtrconfig

Place a `.gtrconfig` file in the root of a project to control how new worktrees are set up. It uses gitconfig INI format.

```ini
[hooks]
  postCreate = npm install   # run after a new worktree is created

[copy]
  include = .env             # copy these files into new worktrees
  include = .env.local       # multiple values are supported
  exclude = .env.production  # exclude specific patterns
```

| Key | Description |
|-----|-------------|
| `hooks.postCreate` | Shell command run in the new worktree directory after creation |
| `copy.include` | Files to copy from the main worktree into each new worktree |
| `copy.exclude` | Patterns to skip when copying |

This keeps worktree setup reproducible without committing secrets or environment-specific files.

## Inspired by

- [git-worktree-runner](https://github.com/coderabbitai/git-worktree-runner) — automated multi-agent workflows over git worktrees
- [agent-of-empires](https://github.com/njbrake/agent-of-empires) — parallel agent orchestration across worktrees
