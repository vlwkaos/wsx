release.flow: rust

## good-to-go

Recurring audit axes (auto-maintained by /good-to-go):
- **Pending ops pattern**: any new user-initiated mutation (session/worktree create/delete/rename) must register a `SessionOp` or `pending_deletions` entry so stale background refreshes don't clobber intent. Check `apply_pending_session_ops` and `filter_pending_deletions` call sites.
- **Cross-instance shared state**: flags that must be visible across concurrent instances (muted, suppressed) must be stored in tmux user options, not the cache. Cache carries only per-instance UI state (cursor, expand, tab, history).
- **Clippy baseline**: 31 pre-existing warnings as of v0.15.6 (all in pre-existing code). Our changes must not add new ones — verify with `cargo clippy 2>&1 | grep "generated .* warning"` count stays at 31.
- **Muted-session derived fields**: `update_activity` skips muted sessions entirely (`continue`). Any `SessionInfo` field that is derived from monitor data (e.g. `is_running_wsx`) will go stale for muted sessions. These fields must either (a) be covered by a fallback elsewhere, or (b) have the muted branch explicitly clear them.
- **Mobile mode interaction boundary**: `app.preview_area` is always `Rect::default()` in mobile mode — mouse click handler uses it to detect preview-area clicks. Any new click target added to the UI must account for the zero-size preview_area in mobile. New UI state that renders differently in mobile (`width < 60`) must update `app.is_mobile` consumers in `dispatch_normal` if action semantics change.
- **Session-state single deriver**: `SessionInfo` stores only raw inputs (`has_activity`, `foreground`, `pane_capture`, `muted`); session state is derived exclusively via `session_state::derive`. Never add a stored derived bool that mirrors derived state. A new signal = a new raw field + a `SessionHeuristic` branch + an `app_state()` projection arm. UI/CLI must call `session_state::derive(s).app_state()`, never re-implement the logic inline.

- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).
