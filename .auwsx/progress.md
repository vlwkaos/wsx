# Implementation progress

- [x] Inspected accepted plan, manifests, config persistence, CLI dispatch, and TUI model boundaries.
- [x] Added validated routine domain, full numeric cron parser, stable FNV-1a-128 project identity, collision checks, and durable revisioned persistence.
- [x] Added durable claims, singleton daemon IPC, direct-argv process groups, cancellation, restart reconciliation, logs, extraction, retention, and execution locks.
- [x] Added tmux-independent routine/daemon CLI clients with CRUD, run, logs, JSON, and stale revision support.
- [x] Added capability-derived TUI header, searchable routine entries, first/create and edit forms with editable Codex/Claude presets, confirmed delete/cancel, details, and mobile-safe full-screen rendering.
- [x] Corrected daemon supervision: background active-run registry, explicit cancel IPC/CLI, nonblocking run scheduling, overlap rejection, shutdown drain, and confirmed-delete cancellation.
- [x] Added slow-run responsiveness/overlap/concurrent-routine tests plus routine form, empty/nonempty header, search, and narrow mobile rendering coverage.
- [x] Updated English/Korean docs for CLI/TUI/cancellation and persistence/recovery semantics.
- [x] Final verification passed: 213 workspace tests, workspace build, isolated daemon CRUD/run/log smoke, CLI help, diff check, and clippy with only the existing 9 core + 17 wsx warnings.
- [x] Added public `RoutineClient` direct and bounded auto-start request paths with caller-owned daemon commands.
- [x] Moved socket discovery, startup handshake/polling, singleton race convergence, and failed-child cleanup into `wsx-core`.
- [x] Added closed serialized `RoutineErrorKind` values and typed remote-daemon errors without changing protocol-v1 strings.
- [x] Migrated wsx CLI and TUI routine requests to `RoutineClient`; daemon status/stop remain direct and non-starting.
- [x] Added lifecycle, concurrent startup, cleanup, invalid transport, typed error round-trip, and CLI command-wiring tests.
- [x] Verification passed: touched-file rustfmt, workspace build, 262 workspace tests, lifecycle harness, diff check, and unchanged clippy baselines of 9 core + 17 wsx warnings.
