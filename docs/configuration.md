# Configuration

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

## Profiles

A profile owns projects and, through them, their manager, chats and tasks. It chooses the agents' Claude account and is a web site's grant ([web client](web-client.md) "Site file"). Groups stay filters inside the active profile.

```toml
[[profiles]]
name = "work"
claude_config_dir = "~/.claude-work"   # optional; absolute or ~/; absent keeps wsxd's environment

[[projects]]
name = "api"
path = "/abs/api"
profile = "work"                       # absent: the default profile
```

With no `[[profiles]]` there is one implicit `default` profile that holds every project and uses wsxd's environment; wsx rewrites no file. A project names exactly one profile. An unknown profile, a name declared twice or a relative `claude_config_dir` makes the file invalid, and wsx names the error. A `claude_config_dir` that is missing stops each agent start in that profile with that reason; no agent falls back to another account.

The account is the agent's `CLAUDE_CONFIG_DIR`. Task workers, the manager and chats get it in the environment their host builds ([tasks](tasks.md) "Permissions"), and their `owner` skills come from `<root>/skills`. A session or split pane started from the TUI, `wsx session create`, `wsx task hand` or a web site runs its shell or agent as `env CLAUDE_CONFIG_DIR=<root> ...`, so an agent typed in it uses the account and a restore resumes it there. Such an agent reports to wsx through the hooks of its own root: `wsx agent install claude` and the TUI's integration install put them in each profile's `claude_config_dir` as well as in the default root. A project that moves to another profile keeps its open tasks; their next turns use the new root.

The Workspace header shows the active profile (`work ▾`) when the config has `[[profiles]]`. `P` or a click on it switches to the next profile. The choice is kept beside the active group in the cache file `group-selection-v1.toml`.

## Wake mode

With `wake_mode` enabled on macOS, generation-authorized Working reports keep a bounded idle-sleep assertion active. Claude starts an asynchronous heartbeat for each prompt so a long streamed response remains protected beyond the base 30-minute lease. The heartbeat is bound to the exact prompt and runtime generation and renews every 5 seconds; completion, blocking, errors, detachment, runtime replacement, a later prompt, a closed pane or a stopped wsxd ends it within seconds.

## Groups, inactivity and stale

Groups are ordered project filters. The default **ungrouped** anti-group matches projects with no memberships.

Trusted agent work, terminal activity, session entry, and expansion changes update each project's inactivity window. The default adaptive policy starts at 24 hours, adds 12 hours on the first trusted activity of each new UTC day, and stops growing at 28 days. If inactivity exceeds the earned window, the next active period starts again at the configured base. When the timer auto-collapses an open project, wsx marks the project `stale` as the cause of its last collapse. The marker survives restart but clears as soon as you interact with the project. wsx never infers agent identity or state from process trees. For an adapter-identified Claude session only, bounded live terminal evidence may reconcile incomplete lifecycle events.

Set `auto_collapse` to `{ mode = "disabled" }` to turn automatic collapse off, or `{ mode = "flat", hours = 72 }` for a fixed window. Legacy numeric `auto_collapse_after_hours` values remain compatible and migrate to flat mode; zero migrates to disabled.

Project interaction clears the persisted `stale` marker and restarts its inactivity window. Other wsx windows cannot reverse that change by refreshing or quitting. Repaired clients import existing UI state once into `workspace-v3.toml`; older clients keep their separate cache. Open windows do not automatically synchronize their presentation. Folded projects and worktrees retain dominant descendant status and an active-session count; `stale` is separate stored provenance, not a pause command. Active projects hide that marker and do not auto-collapse; manual folds remain unchanged. Session rows reserve reported provider labels before truncating long names. See [UI state ownership](ui-state-ownership.md).

## Restore after a daemon loss

After an unplanned daemon loss, wsx restores ordinary shells first and resumes lifecycle-capable saved agents one at a time after reporting becomes available. This limits startup pressure from large histories and keeps each saved agent identity detached until the resumed runtime confirms it.

## Project configuration

The project-root file is `wsx.config.yml`. The `tasks:` block is in [tasks](tasks.md) "Config".

```yaml
hooks:
  postCreate: cargo build
copy:
  include: [.env.example]
  exclude: [target]
git:
  subtrees: [vendor/portable-pty]
worktree:
  branchPrefix: feature/
  defaultSession:
    enabled: true
    command: cargo watch
```

`branchPrefix` preloads the editable TUI branch prompt; CLI branch arguments remain exact. An explicit `defaultSession` controls both TUI and CLI worktree creation. Omit `command` to open a shell. When the block is absent, existing behavior remains: TUI creates only the worktree and CLI also creates a shell session. The command is entered into the new shell automatically, so review project configuration before creating a worktree from an untrusted repository.

wsx validates files, rejects wrong types and values, oversized worktree defaults, and unsafe subtree paths, and migrates legacy `.gtrconfig` only when no canonical YAML exists. Unknown keys are ignored with a warning that names them, so an older wsx still reads a file that a newer one extended.
