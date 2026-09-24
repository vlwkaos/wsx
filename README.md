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

Press `u` to create a routine. Choose a documented one-shot agent template or Custom. A template replaces the command argv shown in the next form. The argv remains editable; Custom starts empty. The shipped `wsxd` binary owns routine scheduling, so release installations do not require a separate `asched` executable.

At startup, wsx offers to update installed integrations whose embedded version is outdated. Declining defers that update until the next launch. Missing integrations remain demand-driven: wsx offers setup only after you explicitly choose an installed agent that needs it, and declining suppresses that agent until you install it from **Global Settings → Runtime → Agent integrations**. PATH detection and residual config files alone never trigger a missing-integration prompt.

Install directly when needed:

```bash
wsx agent install pi
wsx agent install claude
```

Installers preserve unrelated hooks and honor standard config-directory overrides. Restart the affected agent after installation. Codex authoritative lifecycle reporting requires Codex 0.150.0 or newer. Pi reports standard blocking dialogs as blocked without extension-specific wiring. Claude status combines lifecycle hooks with bounded OSC and live prompt evidence so interrupted turns can settle without treating arbitrary terminal text as agent identity. When Pi, OMP, Claude, Codex, Copilot, Devin, Droid, Kimi, Hermes, Qoder, Qwen, Cursor, MastraCode, or Grok exits back to the shell, wsx hides its live agent label but keeps bounded native resume metadata. OpenCode, Kilo, and Antigravity do not currently expose a trustworthy CLI-exit hook, so their identity may remain visible after exit.

## Navigation

| Context | Keys |
|---|---|
| Workspace | `j/k` move, `h/l` collapse/expand, `Enter` select, `m` reorder, `i/I` idle, `a/A` active, `n/N` attention |
| Project | `p` add project, `w` add worktree, `u` add routine, `e` config, `g` assign group |
| Worktree | `s` add session, `r` alias, `m` reorder, `d` delete |
| Session or pane | `Enter` Terminal, `x` acknowledge or mute, `C` interrupt |
| Pane | `|` split right, `-` split down, `d` close |
| Groups | `T` manage, `{`/`}` switch, `g` assign |
| Global | `/` search, `,` settings, `R` refresh, `?` help, `q` quit TUI, `Q` stop wsxd and quit |

Terminal mode uses the configured prefix, `Ctrl+A` by default. Follow it with `j/k` for adjacent sessions, `{`/`}` for the previous or next group, `i/I` for idle, `a/A` for active, `n/N` for attention, `B` to toggle the desktop sidebar, `W` for Workspace, or `Q` to quit only the TUI. While wsx waits for the next key after the prefix, it can temporarily show the expanded sidebar without leaving Terminal mode. Attention navigation defaults to Blocked sessions before other attention states and can restore Workspace order in Global Settings. Group navigation keeps Workspace order, selecting the first session needing attention and then the first idle agent session; if neither exists, the current terminal stays active. `Ctrl+A Ctrl+A` sends a literal prefix.

Groups are ordered project filters. The default **ungrouped** anti-group matches projects with no memberships. Trusted agent work, terminal activity, session entry, and expansion changes update each project's inactivity window. The default adaptive policy starts at 24 hours, adds 12 hours on the first trusted activity of each new UTC day, and stops growing at 28 days. If inactivity exceeds the earned window, the next active period starts again at the configured base. When the timer auto-collapses an open project, wsx marks the project `stale` as the cause of its last collapse. The marker survives restart but clears as soon as you interact with the project. wsx never infers agent identity or state from process trees. For an adapter-identified Claude session only, bounded live terminal evidence may reconcile incomplete lifecycle events.

## Configuration

Open typed Global Settings with `,`. The platform configuration file is `~/.config/wsx/config-v2.toml` on Linux and the equivalent application-support path on macOS.

```toml
terminal_escape_chord = "ctrl+a w"
terminal_prefix_shows_sidebar = true
resume_agents_on_restore = true
wake_mode = true
auto_collapse = { mode = "adaptive", base_hours = 24 }
notification_timeout_seconds = 4
show_release_status = true
terminal_sidebar = "compact"
terminal_title_position = "bottom"
port_visibility = "non_agentic"
attention_priority = "blocked_first"
```

With `wake_mode` enabled on macOS, generation-authorized Working reports keep a bounded idle-sleep assertion active. Claude starts an asynchronous five-minute heartbeat for each prompt so a long streamed response remains protected beyond the base 30-minute lease. The heartbeat is bound to the exact prompt and runtime generation; completion, blocking, errors, detachment, runtime replacement, or a later prompt revokes the old heartbeat.

Set `auto_collapse` to `{ mode = "disabled" }` to turn automatic collapse off, or `{ mode = "flat", hours = 72 }` for a fixed window. Legacy numeric `auto_collapse_after_hours` values remain compatible and migrate to flat mode; zero migrates to disabled.

After an unplanned daemon loss, wsx restores ordinary shells first and resumes lifecycle-capable saved agents one at a time after reporting becomes available. This limits startup pressure from large histories and keeps each saved agent identity detached until the resumed runtime confirms it.

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
wsx session create|delete|restart|list|send-keys|send-text|prompt|peek|rename
wsx group ls|create|rename|add|remove
wsx routine ...
wsx agent install <target>
wsx agent detach
wsx agent report <pane> --provider <name> --state <state> [--session-id <id>|--session-path <path>]
wsx agent request|inspect|wait|continue|cancel|exchanges
wsx plugin list|reload
wsx runtime status [--json]
wsx daemon stop|recover
```

Create a session directly in the current worktree, the only known worktree, or an explicit target:

```text
wsx session create [--name <label>] [--command <shell-input>] [--json]
                   [-p <project>] [-w <branch|alias|path>]
wsx session delete <session|pane|label> [--json]
                   [-p <project>] [-w <branch|alias|path>]
wsx session restart <session|pane|label> [--json]
                   [-p <project>] [-w <branch|alias|path>]
```

Session input, prompt, peek, rename, and restart commands accept the same optional `-p` and `-w` scope. Exact session or pane IDs remain globally addressable. Scoped unique labels avoid a preliminary list; ambiguous targets fail with an actionable error. `--command` is text entered into the new shell after startup, not a direct argv execution.

A pane whose process exits, for example after Ctrl+C or an agent crash, stays listed with its saved command. `wsx session restart` restarts that exact exited pane using the saved command or the persisted native agent session so the session is usable again with a new runtime generation. It refuses a pane that is still running, a stale revision, a daemon that is stopping or replacing its runtime owner, a daemon that is still restoring saved sessions, and a worktree that is gone; a failed start leaves the pane exited and unchanged. Exiting detaches retained agent identity immediately, so Workspace no longer presents it as a live provider.

Updated Pi and OMP integrations renew pane-bound presence while their processes run, including while idle. If renewals stop for 30 seconds, wsxd hides the agent label and Working indicator while retaining resumable identity. A stalled adapter or failed local reports can also cause expiry; a later lifecycle report can reattach it. Install the updated integration and restart that agent to enable this behavior. A pane inherited through a daemon handoff can retain `WSX_AGENT_REPORT_BIN` pointing at the old CLI; before starting the updated agent there, set it to the installed matching `wsx` binary or use a newly created pane. Other integrations still rely on their exit events. If any agent misses its shutdown event while leaving the managed shell running, run `wsx agent detach` inside that pane. The command preserves resumable identity and refuses execution outside the target pane rather than guessing from process or terminal heuristics.

`wsx agent request <session> <prompt>` starts a provider-neutral, generation-bound exchange with an explicit prompt-capable agent. `inspect`, `wait`, `continue`, and `cancel` use the returned exchange ID; `exchanges` lists retained receipts. Persisted intent and failed delivery remain labeled `intent_persisted`; successful universal delivery is labeled `pty_delivery`, lifecycle transitions are labeled `pane_lifecycle`, and `--frame` returns a bounded `terminal_frame` fallback rather than claiming structured assistant output. Read-only exchanges may run concurrently on separate panes. `--writer` claims the target worktree by default, while repeated `--write-claim <absolute-path>` narrows ownership; overlapping active writer claims fail closed. These claims coordinate cooperative scheduling and never grant or revoke repository permissions. Exchanges never steal a live Terminal lease. Native adapters may advertise `exchange_receipts` and submit generation- and round-bound `accepted` or `completed` receipts with `request_bound` evidence. Pygmalion may provide this optimization for Pi later, but is not required by the contract.

Each routine `--arg` is one direct argv item. wsx never invokes a shell for routine argv. Inspect untrusted routines with `wsx routine show <name>` before enabling or running them.

See [Executable plugins](docs/plugins.md) for the versioned event, Terminal-sidecar, and worktree-review contracts. With a review provider installed, Tab on a worktree opens keyboard-driven file and diff review inside its preview. The [reference Git provider setup](docs/worktree-review.md) does not change the agent terminal.

Plain `wsx` and `wsx --mobile` reject nested TUI startup in a wsx-managed terminal. Explicit subcommands remain available. `wsx runtime status` and `wsx daemon stop` never start the daemon. Runtime status distinguishes a stopped daemon from one that is running but incompatible, awaiting an upgrade, deferring replacement, or ready. Its lifecycle output includes the daemon version and revision when the daemon supports them.

Handoff-capable wsxd updates wait until other wsx TUI generations detach, then transfer live terminal ownership to the new daemon without restarting shells, agents, foreground jobs, or listening servers. The TUI reports a successful upgrade or the blockers that deferred it. Routine requests similarly replace legacy standalone schedulers with the adjacent wsxd automatically.

## Runtime and security

- wsxd belongs to the host and Unix user, not one login session. Same-user SSH reconnects reuse live PTYs and buffers.
- Owner-only sockets and peer-UID checks reject cross-user access.
- One writable lease owns each pane. Explicit Terminal entry transfers control to the latest wsx instance; the displaced instance returns to Workspace, and lease generations reject stale input, resize, heartbeat, selection, and release operations. Events invalidate revisions; clients reconcile from authoritative snapshots.
- Messages, frames, agent exchange prompts, deadlines, write claims, retained receipts, commands, plugin manifests, plugin view output, listeners, and resource counts are bounded.
- UI-only wsx releases reuse the compatible daemon. Protocol 16 adds provider-neutral generation-bound agent exchanges, exited-pane restart, and prompt-bound Claude wake renewal while retaining protocol-15 live handoff. Protocol 11–15 daemons remain usable until their safe one-time transition, and opening wsx keeps the existing workspace visible throughout normal deferral and reconnection.
- Native resume creates a new process, PTY, and terminal buffer from a validated provider reference. Unsupported references open a clean shell.
- Remote access, transient graphics preservation across handoff, marketplace installation, and original-process restoration after an unplanned daemon crash are not supported.

## Development

```bash
cargo check --workspace --all-targets --locked
cargo nextest run --workspace --locked
cargo test --workspace --locked --doc
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/runtime-smoke.py
```

See `THIRD-PARTY-NOTICES.md` for vendored terminal dependencies.
