# Structured conversations

WSX protocol 14 adds a wsxd-owned structured conversation API. It is separate from PTY terminal streaming and currently supports Pi RPC only.

## Lifecycle

A client creates one conversation for an existing session using its expected revision. wsxd resolves the worktree, starts `pi --mode rpc` with direct arguments, and owns stdin, stdout, stderr, persistence, and termination. A persisted native session reference is used to restore the RPC process after daemon restart.

Closing the parent session removes its conversation and terminates the complete RPC process group. Daemon shutdown does the same. Input uses a bounded nonblocking queue so a provider that stops reading cannot hold the daemon state lock.

## Operations

- Create a conversation for a session and provider.
- Send `prompt`, `steer`, or `follow_up` input when advertised.
- Abort when advertised.
- Poll typed events after a bounded cursor.
- Respond to typed select, confirm, input, and editor interactions.
- Read validated provider commands after discovery completes.

Attachments and model selection are represented for additive compatibility but return explicit unsupported errors. Clients must follow advertised capabilities.

## Bounds and trust

RPC records, commands, stderr diagnostics, text fields, replay queues, attachments, and event requests are bounded. Malformed or incomplete records fail closed. Provider output is parsed as JSON data and never controls terminal rendering. The RPC executable runs with the user's permissions and is not a sandbox.
