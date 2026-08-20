---
slug: durable-scheduler-state-and-process-cleanup
kind: coding
title: Durable Scheduler State and Process Cleanup
description: Captures fail-closed persistence and identity-safe process cleanup rules for a durable local scheduler.
keywords: [persistence, TOML, schema, event receipt, prepared run, locking, validation, atomicity, process, PID, reconciliation, shutdown, cleanup]
created: 2026-07-24
modified: 2026-08-05
---
# Durable Scheduler State and Process Cleanup

- Durable TOML and runtime state use locks, validation, and atomic replacement. Corrupt or unknown schemas fail closed.
- Project config, runtime state, and rename transactions have independent version constants. V1 project/runtime reads migrate in memory without rewriting files.
- Event admission commits its receipt and initial run records atomically before worker launch. A crash before spawn reconciles the prepared run to `Interrupted` without replay.
- Worker launch or command failure finalizes the exact prepared run and clears active state without persisting payload data.
- Executed commands own process groups. Cancellation and cleanup verify process identity before signaling so reused PIDs or groups are never killed.
- Startup reconciliation persists the latest run state before pruning stale runtime records and never signals a process whose identity no longer matches.
- Shutdown stops admission first, waits for handlers and workers, then removes the socket and lock symmetrically.
