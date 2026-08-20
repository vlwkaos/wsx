---
slug: event-triggers
kind: history
branch: asched
completed: 2026-08-05
---
# Event Triggers

## Summary

Added provider-neutral event-triggered routines to the shared asched daemon. Local event sources can now use `RoutineClient::fire` or the CLI with durable deduplication, bounded JSON payload delivery, typed run causes, and cron-compatible migration.

## Key Decisions

- Model triggers as `Trigger::Cron` or `Trigger::Event`, not cron-string markers.
- Keep event sources outside `asched-core`; callers own authentication, normalization, and transport.
- Commit event receipts and initial run records atomically before worker launch for at-most-once admission.
- Report busy routines without queueing or retrying the event.
- Deliver compact JSON only through `ASCHED_EVENT_PAYLOAD`, never argv or durable state.
- Preserve v1 project/runtime/wsx reads without read-time rewrites and split persistence version constants by format.
- Keep one daemon as the sole writer beneath an `ASCHED_ROOT`; wsx builds UI and event-source integration through the typed client.

## Knowledge Created/Updated

- `domain/Routine Scheduling and Run Lifecycle.md` — typed triggers, event admission, receipts, payloads, and run causes.
- `domain/Project Registry and Scheduling Allowlist.md` — allowlist coverage for event fire.
- `coding/Shared Daemon and Versioned IPC.md` — protocol v3 and `RoutineClient::fire`.
- `coding/Durable Scheduler State and Process Cleanup.md` — version splits, prepared runs, and crash reconciliation.
- `coding/Crash-Safe wsx Import.md` — wsx v1 cron compatibility.
- `coding/CLI and TUI Client Boundaries.md` — event CRUD, filtering, fire output, and TUI rendering.
- `overview.md` — host integration through the shared typed client.

## Implementation Notes

Implemented in commit `8ba278c`. Verification covered exact payload delivery, deduplication, concurrent duplicates, busy and disabled routines, crash reconciliation, 64 KiB boundaries, receipt retention, v1 no-rewrite reads, wsx v1 import, IPC serialization, CLI parsing, and a full CLI event scenario. The workspace format, test, Clippy, and diff gates passed before commit.
