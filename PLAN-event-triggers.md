# Event Triggers

## Goal and Boundary
Let any local event source start project routines through asched's existing daemon and Unix-socket client.

```text
event source → RoutineClient → IPC → daemon admission → execution → command
```

- `asched-core` owns trigger validation, matching, deduplication, durable admission, execution, and history.
- Callers own authentication, normalization, and transport.
- One daemon remains the only writer beneath one `ASCHED_ROOT`.
- The core is provider-neutral and adds no dependency.

## Model
```rust
enum Trigger {
    Cron(String),
    Event { kind: String },
}

enum RunCause {
    Manual,
    Cron { scheduled_epoch_minute: i64 },
    Event { kind: String, event_id: String },
}
```

`Routine` carries one `Trigger`. Event kinds are nonempty namespaced strings such as `filesystem.changed`. V1 has no payload filter language; sources may emit a more specific kind or commands may inspect the payload.

## Compatibility
- Split the shared schema constant into project-config, runtime-state, and transaction versions.
- Config v2 stores typed triggers; v1 `cron = "..."` loads as `Trigger::Cron` without a read-time rewrite.
- Runtime v1 records derive `RunCause::Manual` or `RunCause::Cron` from `scheduled_epoch_minute`.
- wsx imports continue accepting their existing cron schema.
- Bump the closed IPC protocol version for the new action and response.

## Fire Contract
```rust
RoutineClient::fire(
    project: &Path,
    kind: &str,
    payload: serde_json::Value,
    event_id: &str,
) -> Result<FireOutcome, RoutineError>

enum FireOutcome {
    Handled { routines: Vec<RoutineFire> },
    Deduplicated,
    NoMatch,
}

enum RoutineFire {
    Started { name: String },
    AlreadyRunning { name: String },
}
```

`Action::Fire` and `Response::Fire` carry this contract over the existing socket. Only enabled routines with an exact event-kind match participate. A busy routine finishes its current run; the new event is not queued. `Action::Run` still manually starts either trigger type.

## Durable Admission
- Deduplicate by `(project, kind, event_id)` and retain the latest 4,096 receipts per project.
- Validate before recording; do not record a no-match event.
- In one runtime transaction, record the receipt and initial `RunRecord`s for every idle match.
- Start workers only after commit.
- A crash after commit but before spawn leaves an interrupted run record and does not replay the event.
- Spawn and command failures use the existing run lifecycle.
- This guarantees at-most-once admission, not successful execution.

Cron, manual, and event starts share run-record preparation and finalization so no path bypasses run history; event admission additionally commits its receipt and initial records atomically.

## Payload
- Serialize compact JSON and reject payloads over 64 KiB before admission.
- Set `ASCHED_EVENT_PAYLOAD` only for event commands.
- Never place payload in argv, routine state, receipts, or run history.

## CLI
```text
asched routine add NAME --cron EXPR ...
asched routine add NAME --event KIND ...
asched routine edit NAME (--cron EXPR | --event KIND) ...
asched routine fire --project NAME --kind KIND --event-id ID (--payload JSON | --payload-file PATH) [--json]
asched routine list [--project NAME] [--filter TEXT] [--trigger cron|event] [--event-kind KIND] [--json]
```

`routine list` reports trigger, enabled/running state, latest result, and next cron run per project. `routine fire` reports each started or busy routine. Existing project selection and JSON output remain.

## Verification
- V1 config/runtime/wsx compatibility and no rewrite on read
- Trigger validation; disabled, busy, no-match, and duplicate outcomes
- Concurrent duplicate requests and atomic multi-routine admission
- Crash after admission is recorded and not replayed
- Payload integrity and pre-admission 64 KiB rejection
- Unchanged manual/cron behavior, IPC mismatch, CLI output, workspace tests, and clippy

## Non-goals
Payload filters, event queues, retries, source adapters, sub-minute cron, and cross-project fire are outside v1.
