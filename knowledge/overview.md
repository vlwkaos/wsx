---
slug: asched-architecture
kind: overview
title: asched Architecture
description: Architecture and ownership boundaries for the standalone asched scheduler and its host integrations.
keywords: [asched, architecture, asched-core, RoutineClient, ASCHED_ROOT, daemon, scheduler, IPC, TUI, CLI]
created: 2026-07-24
modified: 2026-08-05
---

# asched Architecture

## Ownership

- `crates/asched-core` is the reusable scheduler module. It owns the project registry, cron and event routines, persistence, execution, daemon lifecycle, IPC, and wsx migration without depending on the CLI or TUI.
- `crates/asched` is a thin user surface over `RoutineClient`; hosts use the same typed client, including provider-neutral event fire.

## Process Boundary

- One `ASCHED_ROOT` has one daemon and one durable state tree.
- Embedders such as wsx and auwsx connect to that daemon instead of spawning private schedulers.
- The standalone process boundary prevents scheduler work from invalidating or repainting a host application's navigation loop.
