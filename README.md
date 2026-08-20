# asched

`asched` is a machine-local routine scheduler with a small TUI, an
agent-friendly CLI, and a reusable Rust core. A project is only a name plus the
working directory in which its routine commands run.

## Install

On macOS, install the universal Apple Silicon and Intel binary with Homebrew:

```sh
brew install vlwkaos/tap/asched
```

On another Unix system with Rust installed, build from crates.io:

```sh
cargo install asched --locked
```

Prebuilt macOS archives and checksums are also available on the
[GitHub Releases](https://github.com/vlwkaos/asched/releases) page. The daemon
and client require Unix because they use Unix sockets, process groups, and
`poll`.

## Architecture

- `asched-core` owns validation, persistence, cron and event triggers,
  execution, Unix socket IPC, the bounded daemon, and its typed client.
- `asched` owns CLI parsing and terminal rendering. Routine commands
  automatically start one machine-local daemon when needed.
- Hosts such as wsx and auwsx should depend on `asched-core` and connect to the
  same state root. They should not embed a second scheduler.

State lives in the platform config directory (`~/Library/Application
Support/asched` on macOS). Set `ASCHED_ROOT` for an isolated root. Routine data
is keyed by canonical working-directory path. Commands execute directly as
argv, without a shell. Run logs preserve their beginning and final output while
capping each stdout/stderr stream at 8 MiB. Once a registry exists, only
registered projects are scheduled; removal keeps history but prevents later admission.

## CLI

```sh
asched project add wsx ~/src/wsx
asched project list --json

asched routine list
asched routine list --project wsx --project auwsx
asched routine list --filter ws --json
asched routine list --trigger event --event-kind filesystem.changed

asched routine add cleanup --project wsx \
  --cron "0 3 * * *" --arg cargo --arg clean
asched routine add reindex --project wsx \
  --event filesystem.changed --arg cargo --arg check
asched routine fire --project wsx --kind filesystem.changed \
  --event-id delivery-123 --payload '{"path":"src/main.rs"}' --json
asched routine show cleanup --project wsx --json
asched routine disable cleanup --project wsx
asched routine enable cleanup --project wsx
asched routine run cleanup --project wsx
asched routine logs cleanup --project wsx --json
asched routine cancel cleanup --project wsx
asched routine delete cleanup --project wsx

asched daemon status --json
asched daemon stop
```

Mutations accept `--revision` for strict optimistic concurrency. When omitted,
the CLI fetches the current revision immediately before the mutation. JSON
output is available on resource reads and mutations.

Event firing matches enabled routines by exact namespaced kind. Callers provide
a stable event ID for durable deduplication and a JSON payload, exposed to the
command only as `ASCHED_EVENT_PAYLOAD`. Busy routines are reported and are not
queued.

## TUI

Run `asched` without a subcommand.

| Key | Action |
| --- | --- |
| `j`/`k`, arrows | Move |
| `space` | Run or cancel, depending on current capability |
| `e` | Enable or disable |
| `r` | Refresh |
| `q`, `Esc` | Quit |

Project and routine creation, editing, deletion, filtering, and structured
output remain CLI operations. The TUI is intentionally a compact monitoring and
control surface.

## Migrating from wsx

Inspect the import first:

```sh
asched migrate wsx --dry-run
asched migrate wsx
```

The import registers wsx projects and copies routine definitions. It refuses to
overwrite existing asched routine files and requires the asched daemon to be
stopped during the write:

```sh
asched daemon stop
asched migrate wsx
```

Imported routines are disabled by default to prevent duplicate execution while
the wsx routine daemon may still be running. Stop the old daemon, inspect the
imported commands, then enable routines in asched. `--keep-enabled` is
available for a controlled cutover. Historical runtime records and logs remain
in wsx storage.

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

See [RELEASING.md](RELEASING.md) for publishing. The daemon and client are
Unix-only.
