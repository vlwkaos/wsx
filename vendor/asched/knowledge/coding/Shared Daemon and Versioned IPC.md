---
slug: shared-daemon-and-versioned-ipc
kind: coding
title: Shared Daemon and Versioned IPC
description: Defines safe daemon startup, bounded Unix-socket transport, and typed lifecycle boundaries for a shared scheduler.
keywords: [daemon, IPC, socket, framing, protocol, RoutineClient, fire, event, autostart, handshake, timeout, concurrency, shutdown]
created: 2026-07-24
modified: 2026-08-05
---
# Shared Daemon and Versioned IPC

- Clients auto-start the daemon only for ordinary data requests. Explicit start, stop, and status operations never recursively auto-start it.
- Concurrent auto-starts converge through the daemon lock. Startup readiness uses a dedicated descriptor and bounded timeout; losing or failed children are killed and reaped.
- Unix-socket frames and handler concurrency are bounded. Clients verify the protocol version and daemon identity before trusting a response.
- CRUD, run, logs, cancellation, event fire, and shutdown share one typed request boundary with revision conflicts and a closed set of error categories.
- Protocol version 3 adds `Action::Fire` and `Response::Fire`. `RoutineClient::fire` is the supported provider-neutral host boundary, returning `Handled`, `Deduplicated`, or `NoMatch` plus per-routine `Started` or `AlreadyRunning` results.
- Hosts such as wsx connect to the same daemon and state root instead of embedding another scheduler.
