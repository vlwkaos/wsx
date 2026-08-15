release.flow: rust

## Release

This is a Cargo **workspace** — the shared `~/.claude/skills/rust-release/release.sh` assumes a single-crate layout, so two manual steps are required every release:

- **Use the manual Rust release flow, not the shared `release.sh`.** The shared script now requires a root `[package]` and exits with `Cargo.toml has no root [package] version to update` on this workspace before building any assets. Build/tag/publish through the release skill's manual workspace path until a workspace-aware project script exists.
- **Bump two version locations, not one.** Update root `[workspace.package].version` and the pinned path dependency in `crates/wsx/Cargo.toml` (`wsx-core = { path = "../wsx-core", version = "=X.Y.Z" }`), regenerate `Cargo.lock`, and run the locked workspace gates before tagging.
- **Build and update Homebrew manually.** Produce and verify the universal archive, checksum, optional signature, and bottle before creating the GitHub release. Update `Formula/wsx.rb` in `~/ws-ps/homebrew-tap` with the final URL, version, archive SHA, bottle root URL, and bottle SHA, then validate, commit, and push. The tap remote may be ahead, so pull with rebase and retain the new release values on conflict.
- **Publish crates.io in dependency order.** Publish `wsx-core` first, wait until `0.X.Y` resolves from crates.io, then package and publish `wsx`. Requires a valid crates.io token (`cargo login`); never tag a replacement version after a partial publish.

## good-to-go

- **auwsx issue inspection**: use `"$AUWSX_BIN" issue get <issue_id>`; `issue show` and `issue --help` are unsupported.
- **auwsx finding inspection/adjudication**: `finding get` is unsupported; inspect with `"$AUWSX_BIN" finding ls "$AUWSX_ISSUE_ID" --open` or use the phase prompt's open-finding records, then call only `finding accept` or `finding reject`.
- **auwsx finding creation**: severities are `blocker|major|minor|nit`; subcommand help is unsupported, so use `"$AUWSX_BIN" --help` for callback syntax.
- **Issue-local durable memory**: the injected control outbox rejects `memory save`; do not retry it as a callback, and report the unavailable durable-memory step in the phase report.
- **Routine daemon smoke tests**: sandboxed `flock` can fail with `EPERM`, and detached children from an escalated command may be reaped before a later command; use one approved foreground harness for lifecycle assertions.
- **Filtered Cargo tests**: `cargo test` accepts one positional `TESTNAME` filter; run distinct filters as separate commands.
- **Formatting baseline**: whole-workspace `cargo fmt --all -- --check` currently reports pre-existing drift in `crates/wsx-core/src/config/global.rs`; format touched Rust files directly until that baseline is repaired.
- **Touched-file formatting**: `cargo fmt -- <paths>` still formats the workspace; use `rustfmt --edition 2021 <paths>` to format only selected Rust files.
- **Sibling-repo formatting sandbox**: when the active workspace is `auwsx`, formatting `wsx` source can require escalation even for `rustfmt --edition 2021 <paths>`.
- **Sibling-repo Cargo sandbox**: from an `auwsx` session, `wsx` check/Clippy can require escalation to acquire `target/debug/.cargo-lock`; rerun the same Cargo command with permission.
- **Rust newline literal search**: use `rg -n -F "split(b'\\n')" <path>` so the shell does not turn the pattern into an actual newline.
- **Linked-worktree Git writes**: sandboxed `git add`/`git commit` can fail creating the shared worktree `index.lock`; rerun the Git mutation with approved escalation.
- **Moving a linked worktree**: pass both paths, e.g. `git worktree move <old-path> <new-path>`.
- **Read-only merge audit**: sandboxed `git merge-tree --write-tree` can fail while creating temporary Git objects; use `git merge-tree <merge-base> <main> <branch>` when only conflict inspection is needed.
- **Core test layout**: this repo has no `crates/wsx-core/tests`; search core unit tests under `crates/wsx-core/src`.
- **Exact core unit-test filter**: include the module path, e.g. `cargo test -p wsx-core git::ops::tests::<name> -- --exact --test-threads=1`; a bare function name with `--exact` selects zero tests.
- **Routine daemon shutdown**: use `wsx routine daemon stop`; there is no `shutdown` subcommand.

Recurring audit axes (auto-maintained by /good-to-go):
- **Pending ops pattern**: any new user-initiated mutation (session/worktree create/delete/rename) must register a `SessionOp` or `pending_deletions` entry so stale background refreshes don't clobber intent. Check `apply_pending_session_ops` and `filter_pending_deletions` call sites.
- **Cross-instance shared state**: flags that must be visible across concurrent instances (muted, suppressed) must be stored in tmux user options, not the cache. Cache carries only per-instance UI state (cursor, expand, tab, history).
- **Clippy baseline**: 26 pre-existing warnings as of v0.16.2 (9 in `wsx-core`, 17 in `wsx`). Our changes must not add new ones — verify `cargo clippy` keeps those package counts unchanged.
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
