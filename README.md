# wsx

[![CI](https://github.com/vlwkaos/wsx/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/vlwkaos/wsx/actions/workflows/ci.yml)

Project-first terminal workspace manager for Git worktrees.

wsx presents **Project → Worktree → Session → Pane** in a keyboard-first TUI. The adjacent `wsxd` daemon owns PTYs and terminal state, so sessions continue while clients and SSH connections disconnect.

![wsx Workspace with a running agent and terminal preview](docs/screenshots/01-workspace-overview.png)

## Features

- Git project and worktree discovery, groups, status, aliases, creation, and confirmed deletion
- Persistent sessions with optional horizontal and vertical pane splits
- Writable Ghostty-based terminal viewport with keyboard, mouse, selection, clipboard, and cursor support
- Provider-neutral agent states, native conversation resume, and project routines
- Typed, versioned, bounded same-user local protocol
- Trusted executable plugins with bounded passive Terminal sidecars
- macOS and Linux support

## Product tour

| Workflow | Guide |
|---|---|
| **Iterate attention**<br>`a/A` visits active sessions. `n/N` visits sessions that need attention. | ![Blocked Codex session selected by attention iteration](docs/screenshots/02-attention-iteration.png) |
| **Group work**<br>Persistent groups filter projects. Inactive projects remain visible as stale. | ![A group filtered to a stale project](docs/screenshots/03-groups-and-stale.png) |
| **Use split terminals**<br>Terminal mode keeps pane state, foreground jobs, and detected ports visible. | ![Terminal mode with a split server session](docs/screenshots/04-terminal-and-panes.png) |
| **Schedule routines**<br>Select an agent template, then edit its visible argv, schedule, and prompt. | ![Pi routine editor](docs/screenshots/05-routine-editor.png) |
| **Configure wsx**<br>Typed settings cover workspace, view, terminal, runtime, and agent integrations. | ![Global settings](docs/screenshots/06-global-settings.png) |

## Install

Release archives and the Homebrew formula install adjacent `wsx` and `wsxd` executables.

Build requirements are Rust 1.96.1 and Zig 0.15.2. Development tests use `cargo-nextest` 0.9.143.

```bash
cargo install cargo-nextest --version 0.9.143 --locked
git clone https://github.com/vlwkaos/wsx.git
cd wsx
cargo +1.96.1 build --workspace --locked
cargo xtask run
```

Create a host-native bundle in `target/wsx-dev/`:

```bash
cargo xtask build
```

## Agent integrations and routines

Press `u` to create a routine. Choose a documented one-shot agent template or Custom. A template replaces the command argv shown in the next form. The argv remains editable; Custom starts empty.

wsx offers integration setup only after you explicitly choose an agent that is installed and needs setup. Declining suppresses that agent permanently until you install it from **Global Settings → Runtime → Agent integrations**. PATH detection and residual config files never trigger a prompt.

Install directly when needed:

```bash
wsx agent install pi
wsx agent install claude
```

Installers preserve unrelated hooks and honor standard config-directory overrides. Restart the affected agent after installation. Codex authoritative lifecycle reporting requires Codex 0.150.0 or newer. Pi reports standard blocking dialogs as blocked without extension-specific wiring.

## Navigation

| Context | Keys |
|---|---|
| Workspace | `j/k` move, `h/l` collapse/expand, `Enter` select, `m` reorder, `i/I` idle, `a/A` active, `n/N` attention |
| Project | `p` add project, `w` add worktree, `u` add routine, `e` config, `g` assign group |
| Worktree | `s` add session, `r` alias, `d` delete |
| Session or pane | `Enter` Terminal, `x` acknowledge or mute, `C` interrupt |
| Pane | `|` split right, `-` split down, `d` close |
| Groups | `T` manage, `{`/`}` switch, `g` assign |
| Global | `/` search, `,` settings, `R` refresh, `?` help, `q` quit TUI, `Q` stop wsxd and quit |

Terminal mode uses the configured prefix, `Ctrl+A` by default. Follow it with `j/k` for adjacent sessions, `i/I` for idle, `a/A` for active, `n/N` for attention, `B` to toggle the desktop sidebar, `W` for Workspace, or `Q` to quit only the TUI. `Ctrl+A Ctrl+A` sends a literal prefix.

Groups are ordered project filters. The default **ungrouped** anti-group matches projects with no memberships. A project becomes stale when neither trusted agent work nor terminal entry occurs within the configured window. wsx never infers agent state from terminal output or process trees.

## Configuration

Open typed Global Settings with `,`. The platform configuration file is `~/.config/wsx/config-v2.toml` on Linux and the equivalent application-support path on macOS.

```toml
terminal_escape_chord = "ctrl+a w"
resume_agents_on_restore = true
wake_mode = true
auto_collapse_after_hours = 24
notification_timeout_seconds = 4
show_release_status = true
terminal_sidebar = "compact"
terminal_title_position = "bottom"
port_visibility = "non_agentic"
```

The project-root file is `wsx.config.yml`:

```yaml
hooks:
  postCreate: cargo build
copy:
  include: [.env.example]
  exclude: [target]
git:
  subtrees: [vendor/asched, vendor/herdr]
worktree:
  branchPrefix: feature/
  defaultSession:
    enabled: true
    command: cargo watch
```

`branchPrefix` preloads the editable TUI branch prompt; CLI branch arguments remain exact. An explicit `defaultSession` controls both TUI and CLI worktree creation. Omit `command` to open a shell. When the block is absent, existing behavior remains: TUI creates only the worktree and CLI also creates a shell session. The command is entered into the new shell automatically, so review project configuration before creating a worktree from an untrusted repository.

wsx validates files, rejects unknown fields, bounded worktree defaults, and unsafe subtree paths, and migrates legacy `.gtrconfig` only when no canonical YAML exists.

## CLI

```text
wsx status [--json]
wsx worktree list|create|delete
wsx session list|send-keys|send-text|prompt|peek|rename
wsx group ls|create|rename|add|remove
wsx routine ...
wsx agent install <target>
wsx agent report <pane> --provider <name> --state <state> [--session-id <id>|--session-path <path>]
wsx plugin list|reload
wsx runtime status [--json]
wsx daemon stop|recover
```

Each routine `--arg` is one direct argv item. wsx never invokes a shell. Inspect untrusted routines with `wsx routine show <name>` before enabling or running them.

See [Executable plugins](docs/plugins.md) for the versioned event, Terminal-sidecar, and worktree-review contracts. With a review provider installed, Tab on a worktree opens keyboard-driven file and diff review inside its preview. The [reference Git provider setup](docs/worktree-review.md) does not change the agent terminal.

Plain `wsx` and `wsx --mobile` reject nested TUI startup in a wsx-managed terminal. Explicit subcommands remain available. `wsx runtime status` and `wsx daemon stop` never start the daemon.

## Runtime and security

- wsxd belongs to the host and Unix user, not one login session. Same-user SSH reconnects reuse live PTYs and buffers.
- Owner-only sockets and peer-UID checks reject cross-user access.
- One writable lease owns each pane. Events invalidate revisions; clients reconcile from authoritative snapshots.
- Messages, frames, commands, plugin manifests, plugin view output, listeners, and resource counts are bounded.
- UI-only wsx releases reuse the compatible daemon. Required daemon replacement waits for other daemon revisions, fresh authoritative `working` reports, foreground jobs, and listening servers to clear. wsx reports once when saved terminal commands restart.
- Native resume creates a new process, PTY, and terminal buffer from a validated provider reference. Unsupported references open a clean shell.
- Remote access, live cross-version process handoff, graphics transport, marketplace installation, and original-process restoration are not supported.

## Development

```bash
cargo check --workspace --all-targets --locked
cargo nextest run --workspace --locked
cargo test --workspace --locked --doc
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/runtime-smoke.py
```

See `THIRD-PARTY-NOTICES.md` for vendored terminal dependencies.
