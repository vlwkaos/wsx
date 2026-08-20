---
slug: cli-and-tui-client-boundaries
kind: coding
title: CLI and TUI Client Boundaries
description: Separates agent-friendly command operations from a nonblocking observational TUI over the same scheduler client.
keywords: [CLI, TUI, JSON, revision, argv, shell, event, trigger, fire, refresh, worker, rendering, navigation]
created: 2026-07-24
modified: 2026-08-05
---
# CLI and TUI Client Boundaries

- The CLI exposes project registration and filtering, cron or event routine CRUD, provider-neutral event firing, manual run, cancellation, logs, daemon lifecycle, and wsx import. JSON output and expected revisions make mutations suitable for agents.
- Routine add/edit accepts exactly one trigger type. List output exposes trigger and event-kind filters, running state, latest result, and the next cron run where applicable.
- Fire accepts one project, a stable event ID, and inline or file JSON. JSON history exposes typed triggers, run causes, fire outcomes, status, and timestamps.
- Direct commands preserve every argv item. Shell commands are an explicit alternative rather than an implicit reparsing step.
- The minimal TUI observes state through short background refreshes, renders `cron:<expression>` or `event:<kind>`, and executes mutations on longer background workers. Navigation and rendering never block on daemon I/O.
- The isolated CLI scenario covers event add, filtered list, fire, successful completion, and persisted event cause.
- Dirty rendering redraws only when state changes, avoiding host-style flicker.
