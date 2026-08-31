# wsx

Project-first terminal workspace manager for Git worktrees.

wsx presents **Project → Worktree → Session → Pane** in a keyboard-first TUI. Sessions stay visible as task contexts. Multi-pane sessions expose optional pane rows beneath them. The adjacent `wsxd` daemon owns PTYs and pinned `libghostty-vt` state, so terminals continue running while clients disconnect.

## Features

- Git project and worktree discovery, creation, deletion, cleanup, status, aliases, and project groups
- Persistent sessions and optional horizontal or vertical pane splits
- Writable styled terminal viewport with application cursor shape, resize, keyboard, and mouse support
- Workspace mode for navigation and Terminal mode for direct PTY input
- One explicit writable lease per pane with explicit takeover
- Typed, versioned, bounded same-user local protocol and authoritative snapshots
- Provider-neutral agent states and capabilities
- Trusted executable plugins with bounded versioned manifests
- Project routines through the existing asched boundary
- macOS and Linux runtime support

## Install

Release archives contain adjacent `wsx` and `wsxd` executables. The Homebrew formula must be updated to the 0.18 archive before publication.

### Build from source

Requirements: Rust 1.96.1 and Zig 0.15. Install `cargo-nextest` 0.9.143 to run the development test suite.

```bash
cargo install cargo-nextest --version 0.9.143 --locked
```

```bash
git clone https://github.com/vlwkaos/wsx.git
cd wsx
cargo +1.96.1 build --workspace --locked
cargo xtask run
```

Create a host-native release bundle:

```bash
cargo xtask build  # target/wsx-dev/{wsx,wsxd}
```

On startup, the TUI detects installed agent CLIs with missing or outdated wsx integrations and offers to install them. Declining suppresses the prompt until the next wsx version. Install one integration directly with:

```bash
wsx agent install pi
wsx agent install claude
```

Restart the affected agent after installation. Installers honor each agent's standard configuration-directory override and preserve unrelated configuration.

## Navigation

| Context | Keys |
|---|---|
| Workspace | `j/k` move, `h/l` collapse/expand, `Enter` select |
| Project | `p` add project, `w` add worktree, `u` add routine, `g` assign group |
| Worktree | `s` add session, `r` alias, `d` delete |
| Session or pane | `Enter` Terminal mode, `C` interrupt |
| Pane | `|` split right, `-` split down, `d` close |
| Groups | `T` open groups, `{`/`}` switch, `g` assign selected project |
| Global | `/` search, `R` refresh, `?` help, `q` quit TUI, `Q` confirm hard quit and stop wsxd |

Groups are ordered, persistent project filters. A project can belong to multiple groups, while Workspace applies at most one group filter; selecting none shows every project. The virtual **ungrouped** group matches projects with no memberships. **◷ recent** matches projects touched in the last 24 hours by an authoritative agent `working` report, session creation, or entering a terminal session. In Recent, `d` on a project removes it from Recent until the next qualifying touch without unregistering it.

Group chips occupy one persistent full-width header row in both Workspace and Terminal modes. Workspace content begins immediately below it; in Terminal mode, the existing breadcrumb occupies the next content row. When chips exceed the row, clickable `‹`/`›` controls and the mouse wheel scroll by whole chip; no `+N` overflow is used. Project assignment mode still toggles multiple memberships, and the left sidebar adds a right-edge scrollbar when its rows overflow.

Use `wsx group ls|create|rename|add|remove` to manage groups. Status, worktree-list, and session-list accept one `--group <name>`. Legacy tab configuration and temporary multi-selection cache data migrate once and are rewritten; tab commands and flags are removed. Recent still accepts trusted `wsx agent report` input, but wsx never infers vendors or semantic activity from processes or terminal output.

Session rows use icons for state and show the adapter-reported agent name in parentheses without redundant state words: `○` idle, `◐` working, `×` blocked, `✓` done, `!` error, `·` unknown, and `⊘` muted. Ordinary shells omit the agent label. The terminal header shows `project › worktree › session`, the same state icon, the agent when known, and detected TCP listeners. Worktree previews aggregate ports from their sessions. Port detection is best effort and requires `lsof` on macOS or Linux.

Terminal mode forwards ordinary keyboard and mouse input over a persistent stream. The terminal fills the right panel below its one-row breadcrumb without wsx padding. Click the left panel to return to Workspace mode and select that row. Press `Ctrl+A`, then `W` to focus Workspace; `W` also works while Control remains held. `Ctrl+A Ctrl+A` sends a literal `Ctrl+A`; any other suffix forwards both keys to the terminal. Default terminal backgrounds remain transparent; applications such as Vim retain explicit ANSI cell backgrounds and control the visible block, underline, or bar cursor through Ghostty cursor state.

Configure the escape sequence in `~/.config/wsx/config.toml`:

```toml
terminal_escape_chord = "ctrl+a w"
```

The prefix must include a modifier. A single modified chord remains supported. Names include `ctrl`, `alt`, `shift`, `super`, `space`, `tab`, `esc`, and single characters.

## Project configuration

The canonical project-root configuration is `wsx.config.yml`:

```yaml
hooks:
  postCreate: cargo build
copy:
  include:
    - .env.example
  exclude:
    - target
```

If a project has `.gtrconfig` but no `wsx.config.yml`, wsx reads the legacy values and atomically creates an equivalent YAML file. It leaves `.gtrconfig` in place so you can review and remove it later. An existing `wsx.config.yml` always wins. Files larger than 64 KiB, malformed values, and unknown fields are rejected without falling back to legacy behavior.

## CLI

```bash
wsx status [--json]
wsx worktree list|create|delete
wsx session list
wsx session send-keys <session-or-pane> <keys>
wsx session send-text <session-or-pane> <text>
wsx session prompt <session-or-pane> <prompt>
wsx session peek <session-or-pane> [-n VISIBLE_LINES] [--trim]
wsx session rename <session-id> <label>
wsx agent install <target>
wsx agent report <pane> --provider <name> --state <state> [capabilities]
wsx plugin list [--json]
wsx plugin reload
wsx runtime status [--json]
wsx daemon stop
wsx routine ...
```

`wsx runtime status` and `wsx daemon stop` never start the daemon. `Q` in Workspace asks for confirmation, gracefully stops wsxd and all live PTYs, then exits the TUI. Normal wsx startup reuses a compatible running daemon. If the local protocol changed, wsx requests graceful shutdown, waits for cleanup, and starts the adjacent or `PATH`-resolved `wsxd`. Cross-version process handoff is not supported, so the old PTYs end; the new daemon preserves each wsx session and pane identity and recreates its saved launch command instead. `WSX_DAEMON_BIN` and `WSX_SOCKET` are trusted same-user overrides.

## Plugins and agents

Place owner-controlled JSON manifests in `~/.config/wsx/plugins/`. A manifest declares API version `1`, a stable ID, executable argv, subscribed event names, and whether it is enabled. Relative executables resolve inside the plugin directory. wsxd rejects symlinks, group/world-writable files, wrong owners, oversized manifests, invalid tokens, and non-executable commands. Plugin calls have bounded payloads and timeouts.

Agent integrations report normalized `unknown`, `idle`, `working`, `blocked`, `done`, or `error` state plus declared capabilities. Supported install targets are `pi`, `omp`, `claude`, `codex`, `copilot`, `devin`, `droid`, `kimi`, `opencode`, `kilo`, `hermes`, `qodercli`, `qwen`, `cursor`, `mastracode`, `antigravity-cli`, and `grok`. Pi, OMP, Kimi, OpenCode, Kilo, and MastraCode expose authoritative lifecycle state. Other supported hooks expose authoritative agent/session identity with unknown state. Provider-specific metadata and conversation handling remain adapter-owned. wsx does not infer agent state from terminal motion or process trees.

## Runtime and security

- The socket and state directory are owner-only. Do not expose them to untrusted processes running as the same user.
- Each terminal pane has one writable client lease. A second client must request takeover explicitly.
- Events invalidate revisions; clients reconcile from authoritative snapshots.
- Slow clients do not block PTY parsing. Messages, frames, commands, plugins, and resource counts are bounded.
- Terminal frames preserve Ghostty wide/spacer occupancy, defer intermediate synchronized-output frames after the first baseline, and use the subscribed viewport for that baseline. Workspace metadata refreshes never own or clear the accepted terminal surface.
- wsxd persists project, worktree, session, pane, terminal, and known-agent identity. After daemon restart it recreates each pane from an owner-only saved launch recipe; this is a fresh process and terminal buffer, not restoration of the original process or agent conversation.
- Remote access, live daemon handoff, graphics transport, marketplace installation, and guaranteed process or conversation restoration are not yet supported.

## Development

```bash
cargo check --workspace --all-targets --locked
cargo nextest run --workspace --locked
cargo test --workspace --locked --doc
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/runtime-smoke.py
```

Nextest runs each test in its own process. Keep the separate `cargo test --doc` step because Nextest does not run doctests.

See `THIRD-PARTY-NOTICES.md` for vendored terminal dependencies.
