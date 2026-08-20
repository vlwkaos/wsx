---
slug: crash-safe-wsx-import
kind: coding
title: Crash-Safe wsx Import
description: Defines a non-overwriting transactional import that distinguishes rollback from committed-state recovery.
keywords: [migration, import, wsx, cron, Trigger, schema, transaction, rollback, recovery, collision, validation, daemon, lock, enablement]
created: 2026-07-24
modified: 2026-08-05
---
# Crash-Safe wsx Import

- Import is explicit and non-overwriting. Source projects and routines are validated and planned before destination writes begin.
- The destination daemon must be stopped and its lock acquired. Name, path, and routine-file collisions abort without changing destination state.
- A transaction record distinguishes an uncommitted import that must roll back from a committed import that needs only recovery cleanup.
- Imported routines default to disabled unless the caller explicitly preserves enablement.
- Existing wsx v1 cron files remain accepted and deserialize to typed `Trigger::Cron`; destination state uses the current project schema without changing non-overwrite or recovery behavior.
