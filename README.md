# wsx

TUI workspace manager for git worktrees and tmux sessions.

<!-- screenshot -->

## Overview

`wsx` lets you manage multiple projects, their git worktrees, and tmux sessions from a single terminal interface. Navigate your workspace tree, attach to sessions, and keep context across branches without losing your place.

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

| Key | Action |
|-----|--------|
| `j/k` | Navigate |
| `Enter` | Expand / attach to session |
| `w` | New worktree |
| `s` | New session |
| `d` | Delete |
| `?` | Help |
| `q` | Quit |

Inside a session, use `Ctrl+a d` to detach and return to wsx.

## Config

Global config at `~/.config/wsx/config.toml`. Per-project config via `e` key.
