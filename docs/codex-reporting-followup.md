# Deferred live Codex reporting diagnosis

Status: unresolved and explicitly deferred from the 0.29.1 release gate by user approval. This is separate from the verified removed-reporter ENOENT repair. See [agent reporting](agent-reporting.md) and [candidate audit](release-0.29.1-audit.md).

## Known evidence

- The last protected-session observation found an existing reporter executable and runtime-generation presence, but no attached daemon-owned agent identity.
- Fresh Codex discovery for the target cwd reported eight WSX hooks enabled and trusted. It did not inspect the running session's cached handlers.
- Pinned Codex `rust-v0.160.0` captures the process environment at hook construction and preserves it across hook reconfiguration.
- Discovery excludes enabled hooks whose trust hash is missing or modified. Events use the admitted handler vector until it is rebuilt. Later disk approval does not prove an older session admitted those handlers.
- Trust hashes cover normalized hook configuration, not the referenced script's contents. Script modification time alone is not evidence of revoked authorization.
- Native `hooks/list` accepts cwd paths and runs fresh disk discovery. A new diagnostic app-server cannot expose or reload another server's protected live thread through that method.

## Next evidence and stopping condition

1. Obtain provider-native evidence from the exact protected session: its admitted WSX handlers or a hook execution/failure receipt. Retain event kind and bounded error class, not prompt text, transcripts, credentials or runtime-generation values.
2. If the failure identifies cached configuration or trust, obtain permission for the provider's supported refresh/recovery action in that session. Do not infer that a fresh server or a disk edit refreshed it.
3. Verify a provider-originated, current-generation lifecycle report attaches identity in wsxd. Verify that the original pane, PTY and runtime remain intact when the recovery contract promises that.
4. Close this follow-up only when the exact cause and repair or verified recovery are established. Private synthetic actors and fresh discovery do not meet that condition.

Do not inject identity, bypass trust, send keys or signals, restart the agent/daemon, or disturb user sessions without specific authorization. Repeating fresh discovery adds no evidence about cached admission.

## 0.29.1 rollout boundary

Install the updated integration and load its adapter/helper through the provider's supported idle reload or restart. Installing the CLI alone cannot replace already-loaded adapter code. The removed-binary recovery keeps generation checks, validates executable authority, and never replays a rejected or uncertain report. It does not require the live Codex diagnosis to be resolved.
