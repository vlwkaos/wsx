---
slug: asched-standalone-scheduler
kind: history
branch: main
completed: 2026-07-24
---
# asched Standalone Scheduler

## Summary

Extracted routine scheduling into a standalone Rust workspace with a reusable
core, agent-friendly CLI, minimal TUI, shared daemon, and safe wsx import.
Added a tag-driven Cargo, GitHub, universal macOS, and Homebrew release flow.

## Key Decisions

- Keep scheduler ownership in `asched-core`; host applications use
  `RoutineClient` and do not spawn independent daemons for the same root.
- Use canonical working-directory paths as project identity and names only for
  grouping and filtering.
- Keep daemon I/O off TUI navigation and rendering paths.
- Publish workspace crates in dependency order and update Homebrew only from
  stable tags.

## Knowledge Created

- `overview.md`: module and process ownership.
- `domain/Project Registry and Scheduling Allowlist.md`: project identity and
  registry admission rules.
- `domain/Routine Scheduling and Run Lifecycle.md`: scheduling and run rules.
- `coding/Shared Daemon and Versioned IPC.md`: lifecycle and transport boundary.
- `coding/Durable Scheduler State and Process Cleanup.md`: persistence and
  process safety.
- `coding/Crash-Safe wsx Import.md`: transaction and recovery behavior.
- `coding/CLI and TUI Client Boundaries.md`: agent and human client behavior.
- `coding/Rust Workspace Release and Homebrew Publishing.md`: release contract.

## Implementation Notes

The implementation was verified with the full workspace test, Clippy, format,
documentation, universal-binary, package, security, and release-script gates.
wsx and auwsx integration remains a separate compatibility and transplant task.
