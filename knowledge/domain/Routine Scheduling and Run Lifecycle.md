---
slug: routine-scheduling-and-run-lifecycle
kind: domain
title: Routine Scheduling and Run Lifecycle
description: Routine definition, admission, execution-claim, lifecycle, logging, and retention rules in asched.
keywords: [Routine, Trigger, RunRecord, RunCause, cron, event, event receipt, enabled, manual run, minute claim, overlap policy, retention, logs, admission]
created: 2026-07-24
modified: 2026-08-05
---

# Routine Scheduling and Run Lifecycle

## Definition and Availability

- Each routine has one typed trigger: a cron expression or an exact namespaced event kind. Event kinds contain at least two nonempty dot-separated segments and no whitespace or control characters.
- Routine definitions retain direct argv, prompt, and enablement. Disabled routines remain manually runnable but have no scheduled next run and do not match events.

## Admission and Execution

- Persisted minute claims prevent duplicate cron execution and execution caused by clock rollback.
- Event admission atomically persists a `(kind, event ID)` receipt and initial run records before worker launch. The latest 4,096 receipts per project provide at-most-once admission, not successful execution.
- Event payloads are compact JSON bounded to 64 KiB and are delivered only through `ASCHED_EVENT_PAYLOAD`; payloads never enter routine state, receipts, argv, or run history.
- Busy event routines finish their current run, report `AlreadyRunning`, and do not queue or retry the event.
- `RunCause` records manual, cron, or event admission while preserving legacy cron history during runtime-state migration.

## Output and Retention

- Logs and extracted results are bounded.
- Retention deletes only validated persisted output paths.
