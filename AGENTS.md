release.flow: rust

## Release

This is a Cargo **workspace** — the shared `~/.claude/skills/rust-release/release.sh` assumes a single-crate layout, so two manual steps are required every release:

- **Bump two version locations, not one.** `release.sh`'s `sed` only rewrites the root `[workspace.package].version`. Also bump the pinned path dep in `crates/wsx/Cargo.toml` (`wsx-core = { path = "../wsx-core", version = "=X.Y.Z" }`), or the in-script `cargo build` fails on the version mismatch. Bump both and run `cargo build` before invoking `release.sh`.
- **Homebrew step dies silently on the workspace root.** `release.sh` runs `grep '^description' Cargo.toml` near the end; the workspace root has no `[package]`/`description`, so under `set -e` grep exit-1 kills the script *after* the GitHub release/tag/push but *before* the tap update. Recover the tap manually using the artifacts the script already produced: tarball SHA is in `wsx-<ver>-darwin-universal.tar.gz.sha256`, bottle SHA is printed as `Bottle: ... (<sha>)`. Update `Formula/wsx.rb` in `~/ws-ps/homebrew-tap` (url, version, sha256, bottle root_url + bottle sha256), then commit + push. The tap remote may be ahead — `git pull --rebase` and keep the new version on conflict.
- **crates.io is a manual step.** `release.sh` skips publish because the workspace root has no `[package]`. Publish in dependency order after the GitHub release: `cargo publish -p wsx-core` then `cargo publish -p wsx`. Requires a valid crates.io token (`cargo login`); a 403 means the token is stale — GitHub release + brew are already live, so this can be finished later without re-tagging.

## good-to-go

- **auwsx issue inspection**: use `"$AUWSX_BIN" issue get <issue_id>`; `issue show` and `issue --help` are unsupported.
- **auwsx finding inspection/adjudication**: `finding get` is unsupported; inspect with `"$AUWSX_BIN" finding ls "$AUWSX_ISSUE_ID" --open` or use the phase prompt's open-finding records, then call only `finding accept` or `finding reject`.
- **Routine daemon smoke tests**: sandboxed `flock` can fail with `EPERM`, and detached children from an escalated command may be reaped before a later command; use one approved foreground harness for lifecycle assertions.
- **Filtered Cargo tests**: `cargo test` accepts one positional `TESTNAME` filter; run distinct filters as separate commands.
- **Rust newline literal search**: use `rg -n -F "split(b'\\n')" <path>` so the shell does not turn the pattern into an actual newline.
- **Linked-worktree Git writes**: sandboxed `git add`/`git commit` can fail creating the shared worktree `index.lock`; rerun the Git mutation with approved escalation.

Recurring audit axes (auto-maintained by /good-to-go):
- **Pending ops pattern**: any new user-initiated mutation (session/worktree create/delete/rename) must register a `SessionOp` or `pending_deletions` entry so stale background refreshes don't clobber intent. Check `apply_pending_session_ops` and `filter_pending_deletions` call sites.
- **Cross-instance shared state**: flags that must be visible across concurrent instances (muted, suppressed) must be stored in tmux user options, not the cache. Cache carries only per-instance UI state (cursor, expand, tab, history).
- **Clippy baseline**: 31 pre-existing warnings as of v0.15.6 (all in pre-existing code). Our changes must not add new ones — verify with `cargo clippy 2>&1 | grep "generated .* warning"` count stays at 31.
- **Muted-session derived fields**: `update_activity` skips muted sessions entirely (`continue`). Any `SessionInfo` field that is derived from monitor data (e.g. `is_running_wsx`) will go stale for muted sessions. These fields must either (a) be covered by a fallback elsewhere, or (b) have the muted branch explicitly clear them.
- **Mobile mode interaction boundary**: `app.preview_area` is always `Rect::default()` in mobile mode — mouse click handler uses it to detect preview-area clicks. Any new click target added to the UI must account for the zero-size preview_area in mobile. New UI state that renders differently in mobile (`width < 60`) must update `app.is_mobile` consumers in `dispatch_normal` if action semantics change.
- **Session-state single deriver**: `SessionInfo` stores only raw inputs (`has_activity`, `foreground`, `pane_capture`, `muted`); session state is derived exclusively via `session_state::derive`. Never add a stored derived bool that mirrors derived state. A new signal = a new raw field + a `SessionHeuristic` branch + an `app_state()` projection arm. UI/CLI must call `session_state::derive(s).app_state()`, never re-implement the logic inline.
- **Mute is sticky — interaction-only unmute**: background pane output never auto-unmutes a session (as of 0.15.10). Only an explicit user interaction in wsx (`attach`, `send_keys`, `send_ctrl_c`, `rename`) clears mute, via `App::unmute_on_interaction`. Any new session-targeting action MUST call that helper or muted users get stuck with no signal. Cursor navigation must NOT call it — peeking is not interaction.
- **Foreground classification via process tree**: tmux's `pane_current_command` reports the deepest spawned child (e.g. a node subprocess named `2.1.x` masks the real `claude` agent). `tmux::monitor::session_activity` walks `pane_pid`'s descendants via one `ps -ax` snapshot per refresh and picks the highest-rank kind. Any new foreground class added to `ForegroundKind` needs a matching `classify_foreground` arm AND a `foreground_rank` entry, else multi-window aggregation silently mis-orders it.
- **Realistic-workspace regression test**: `session_state::tests::given_realistic_workspace_when_classified_then_each_session_state_matches_spec` pins the per-session outcomes for a 10-session fixture representing typical usage. This test exists because v0.15.9 shipped with all unit tests passing while real-world usage rendered "everything green" — the fixture is the floor for any future derive change. Do not loosen it without updating the fixture.

- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).

## auwsx Knowledge Collections
<!-- auwsx:knowledge-collections -->
Agents should use these collection hints before answering project/domain questions or changing architecture:
- coding_language: rust
- project_domain: wsx
- knowledge_domains: coding, domain
- update_when: project stack, domain, or knowledge layout changes
