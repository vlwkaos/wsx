release.flow: rust

## good-to-go

Recurring audit axes (auto-maintained by /good-to-go):
- **Pending ops pattern**: any new user-initiated mutation (session/worktree create/delete/rename) must register a `SessionOp` or `pending_deletions` entry so stale background refreshes don't clobber intent. Check `apply_pending_session_ops` and `filter_pending_deletions` call sites.
- **Cross-instance shared state**: flags that must be visible across concurrent instances (muted, suppressed) must be stored in tmux user options, not the cache. Cache carries only per-instance UI state (cursor, expand, tab, history).
- **Clippy baseline**: 29 pre-existing warnings as of v0.14.7 (all in pre-existing code). Our changes must not add new ones — verify with `cargo clippy -- -D warnings 2>&1 | grep "error:" | wc -l` stays at 29.
