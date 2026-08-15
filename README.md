# wsx

**ENG** | [한국어](README.ko.md)

TUI workspace manager for git worktrees and tmux sessions.

<!-- screenshot -->
![Screen Recording 2026-03-06 at 12 02 09 AM_1](https://github.com/user-attachments/assets/8427aa7d-bfa2-4349-847e-9f374c44e7f0)


## The core idea

Keep a live view of every project → worktree → tmux session in a sidebar.
Each session shows real-time state so you can see what needs attention without entering it.
Simply pressing `n` to iterate sessions where attention is required.

```
▼ project
  ▾ main * ↑2
      ◉ wsx_cc_main
  ▸ feature-auth ↓1
      ○ wsx_cc_auth
```

```mermaid
flowchart LR
  P[Project] --> W1[Worktree main]
  P --> W2[Worktree feature-auth]
  W1 --> S1[Session: nvim]
  W1 --> S2[Session: dev]
  W2 --> S3[Session: dev]
```

## Install

**macOS (Homebrew)**
```sh
brew tap vlwkaos/tap
brew install wsx
```

**macOS / Linux (cargo)**
```sh
cargo install wsx
```

**Build from source**
```sh
cargo install --path .
```

> Must be run inside a tmux session.

## Guide

| Feature | Screenshot |
|---|---|
| **Project config** `.gtrconfig` at repo root — post-create hook, auto-copy env files into new worktrees. Press `e` to view. | <img width="473" height="245" alt="image" src="https://github.com/user-attachments/assets/41a1ef82-9ebb-49aa-993e-4ae9f1ea0a83" /> |
| **Add project** Press `p`, enter a path. Tab-completion supported. | <img width="457" height="221" alt="image" src="https://github.com/user-attachments/assets/b6c0c7bf-7252-4281-bee4-8dfa4c8d4529" /> |
| **New worktree** Select a project, press `w`, enter a branch name. | <img width="459" height="52" alt="image" src="https://github.com/user-attachments/assets/8280c712-29a1-43d6-8504-0c7161ab9b86" /> <img width="264" height="90" alt="image" src="https://github.com/user-attachments/assets/c8183cf6-4de8-414a-88e2-1ceac1722080" /> |
| **Sessions** Select a worktree, press `s`. Name by context — `shell`, `claude`, `build`. Sessions are persistent tmux sessions; `d` deletes, `r` renames. | <img width="270" height="68" alt="image" src="https://github.com/user-attachments/assets/41569337-057f-44b8-bd39-8f1d2ffa6a1f" /> |
| **Iterate pending** `n` / `N` to jump between `●` sessions. `x` dismisses; press again to mute `⊘`. `a` cycles active `◉` sessions. | ![Screen Recording 2026-02-27 at 9 35 16 AM](https://github.com/user-attachments/assets/46c6b7be-34b2-4f73-b959-6205d81d1a66) |
| **Remote control** `S` sends a command to the selected session without entering it. `C` sends Ctrl+C — handy for killing a watcher the moment you spot it. | <img width="464" height="57" alt="image" src="https://github.com/user-attachments/assets/6d466d85-4d92-44c7-abe8-93ec4337f480" /> |
| **Tabs** Press `T` to open the tab manager — create named tabs and assign projects to them. `{` / `}` cycles tabs. Active tab shown full, others abbreviated in the title bar: `[default\|pe\|wo]` | |

> [!IMPORTANT]
> **Returning to wsx from inside a session:** press `Ctrl+a d` to detach. The session keeps running.
>
> If your tmux prefix is not `Ctrl+a`, add this to `~/.tmux.conf`:
> ```
> set -g prefix C-a
> ```

### .gtrconfig

Place `.gtrconfig` at the root of any project repo to automate worktree setup.

> [!TIP]
> New worktrees automatically run `postCreate` and receive copies of listed env files — no manual setup per branch.

```ini
[hooks]
  postCreate = npm install

[copy]
  include = .env
  include = .env.local
  exclude = .env.production
```

Press `e` on any project or worktree to view its config.

## Usage

```sh
wsx
```

<details>
<summary>Navigation</summary>

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

</details>

<details>
<summary>Workspaces</summary>

| Key | Action |
|-----|--------|
| `p` | Add project |
| `w` | New worktree |
| `s` | New session |
| `u` | New routine for the selected project; `F1`/`F2` apply editable Codex/Claude presets |
| `m` | Reorder project or session |
| `r` | Set alias |
| `d` | Delete; a running routine is cancelled before deletion |
| `g` | Git popup (pull / push / rebase / merge) |
| `c` | Clean merged worktrees |
| `e` | Edit the selected routine, otherwise view `.gtrignore` |
| `S` | Send command to session |
| `C` | Send Ctrl+C to session |
| `T` | Tab manager (add / rename / delete / reorder) |
| `{` / `}` | Switch to prev / next tab |
| `m` + `h`/`l` | Move project to adjacent tab (in Move mode) |

</details>

<details>
<summary>tmux status bar</summary>

wsx sets `status-right` to `project/worktree` on attach. With a custom `~/.tmux.conf`:

```
set -g status-right "#{@wsx_project}/#{@wsx_alias}"
```

Agent status is provider-neutral and requires no integration plugin. wsx classifies the foreground process first, then compares a bounded pane tail when an agent emits terminal activity. Visible motion stays active; repeated identical redraws settle to attention after three seconds. Runtime and watch processes remain active while running, and capture failures fall back to tmux activity.

</details>

## Mobile / SSH

```sh
wsx --mobile
```

Collapses the preview panel and shows compact key hints — designed for portrait SSH sessions on a phone or narrow terminal. Use the `mobile_detach_key` config option to bind a no-prefix tmux key for easy detach:

```toml
# ~/.config/wsx/config.toml
mobile_detach_key = "C-q"
```

## CLI

### Machine-local routines

`wsx routine` is a project-scoped client for [asched](https://github.com/vlwkaos/asched). Each wsx project node shows routines registered for that same canonical project path. Install and start asched separately; wsx never embeds or starts a scheduler.

```sh
wsx routine add nightly --cron "0 2 * * *" --arg codex --arg exec --arg=--json --arg '{prompt}' --prompt "Run maintenance" -p wsx
wsx routine list -p wsx
wsx routine show nightly -p wsx
wsx routine disable nightly -p wsx
wsx routine enable nightly -p wsx
wsx routine run nightly -p wsx
wsx routine cancel nightly -p wsx
wsx routine logs nightly -p wsx
wsx routine fire --kind filesystem.changed --event-id delivery-123 --payload '{"path":"src/main.rs"}' -p wsx
wsx routine delete nightly -p wsx
```

wsx and asched resolve the same platform-default state directory, overridden by `ASCHED_ROOT`. Register projects with `asched project add`; that registry remains the scheduling allowlist. The single asched daemon owns routine writes, scheduling, execution, and event deduplication. wsx sends optimistic revisions for mutations and reports conflicts, protocol mismatch, deduplicated/no-match events, and already-running routines.

In the TUI, press `u` on a project or any of its entries to create the first routine, then expand its project-level `sched` section for later entries. `e` edits, and confirmed `d` deletes or cancels then deletes. The form keeps command argv as a JSON array so arguments never pass through a shell. The preview shows configuration, next/last run, log paths, currently allowed actions, and final agent output. On mobile, Enter opens the routine detail full-screen.

Routine persistence, retained history/logs, cron and event semantics, execution cleanup, and daemon lifecycle belong to asched. TUI daemon I/O runs on background workers so unavailable or slow daemon responses do not block navigation. The preview derives enabled/running capabilities, next cron run, latest result, and recent history directly from `asched_core::routine::ipc::RoutineView`.

```sh
# Worktrees
wsx worktree create <branch> [-p <project>]
wsx worktree delete <branch> [-p <project>]
wsx worktree list  [-p <project>] [--json]

# Sessions
wsx session send-keys <session> <keys>
wsx session peek <session> [-n <lines>] [-o <offset>] [-a]
wsx session rename <old> <new>
wsx session list   [-p <project>] [--json]

# Status
wsx status [--json]
```

`peek` captures pane output. `-n` sets how many scrollback lines to include (default: visible viewport). `-o` skips lines from the bottom to scroll further back. `-a` strips ANSI/decorations for agent/LLM consumption.

## Config

<details>
<summary>Global config</summary>

Global config: `~/.config/wsx/config.toml`. Per-project config via `e` key.

</details>

## Inspired by

- [git-worktree-runner](https://github.com/coderabbitai/git-worktree-runner)
- [agent-of-empires](https://github.com/njbrake/agent-of-empires)
