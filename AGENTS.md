release.flow: rust

## good-to-go

Recurring audit axes (auto-maintained by /good-to-go):
- **Pending ops pattern**: any new user-initiated mutation (session/worktree create/delete/rename) must register a `SessionOp` or `pending_deletions` entry so stale background refreshes don't clobber intent. Check `apply_pending_session_ops` and `filter_pending_deletions` call sites.
- **Cross-instance shared state**: flags that must be visible across concurrent instances (muted, suppressed) must be stored in tmux user options, not the cache. Cache carries only per-instance UI state (cursor, expand, tab, history).
- **Clippy baseline**: 28 pre-existing warnings as of v0.15.0 (all in pre-existing code). Our changes must not add new ones — verify with `cargo clippy 2>&1 | grep "generated .* warning"` count stays at 28.
- **Muted-session derived fields**: `update_activity` skips muted sessions entirely (`continue`). Any `SessionInfo` field that is derived from monitor data (e.g. `is_running_wsx`) will go stale for muted sessions. These fields must either (a) be covered by a fallback elsewhere, or (b) have the muted branch explicitly clear them.
