# wsx

TUI workspace manager for git worktrees and tmux sessions.

<!-- screenshot -->

## The core idea

Most terminal multiplexer workflows leave you hunting: which session was running that dev server? Which branch had the failing test? Did that long build finish?

`wsx` solves this by keeping a live view of every project → worktree → session in a sidebar, with each session showing its real-time state:

| Icon | Meaning |
|------|---------|
| `◉` green | Actively producing output right now |
| `●` yellow | Needs attention — bell fired, or a non-passive process went quiet |
| `○` gray | Idle |
| `⊘` | Muted — you don't want updates from this one |

The yellow `●` is deliberately semi-heuristic: it fires on tmux bell activity *or* when a foreground process that isn't a shell (and isn't a known passive watcher like `node`, `vite`, `tail`, `watch`) goes quiet — meaning it probably finished or is waiting for input. Passive long-runners like dev servers and file watchers never trigger it.

### Iterate pending sessions with `n` / `N`

Press `n` to jump to the next session with a `●` indicator, `N` for previous. This turns "check what needs attention" from a manual scan into a single keypress loop — step through only the sessions that actually want you.

Once you've handled a session, `x` dismisses the indicator (or mutes the session entirely with a second press). Active sessions (`◉`) can't be dismissed — there's nothing to dismiss yet.

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
| `Ctrl+d/u` | Jump to next/prev project |
| `n` / `N` | Jump to next / prev pending session `●` |
| `x` | Dismiss attention · mute session |
| `/` | Incremental search |
| `?` | Full key reference |
| `q` | Quit |

Mouse clicks also work: click a tree row to select it, click the preview pane to attach the focused session.

### Workspaces

| Key | Action |
|-----|--------|
| `p` | Add project |
| `w` | New worktree (branch prompt) |
| `s` | New persistent session |
| `o` | Open ephemeral session (exits on detach) |
| `m` | Reorder project or session |
| `r` | Set alias |
| `d` | Delete |
| `c` | Clean merged worktrees |
| `e` | View `.gtrconfig` |

Inside a session, use `Ctrl+a d` to detach and return to wsx.

## Config

Global config at `~/.config/wsx/config.toml`. Per-project config via `e` key.
