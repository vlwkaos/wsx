release.flow: rust

## Release

This is a Cargo **workspace** — the shared `~/.claude/skills/rust-release/release.sh` assumes a single-crate layout, so two manual steps are required every release:

- **Use the manual Rust release flow, not the shared `release.sh`.** The shared script requires a root `[package]` and exits with `Cargo.toml has no root [package] version to update`. Use `scripts/package-companion.sh` only for the universal companion archive; keep versioning, tagging, GitHub publication, crates.io publication, and Homebrew updates in the manual workspace flow.
- **Bump two version locations, not one.** Update root `[workspace.package].version` and the pinned path dependency in `crates/wsx/Cargo.toml` (`wsx-core = { path = "../wsx-core", version = "=X.Y.Z" }`), regenerate `Cargo.lock`, and run the locked workspace gates before tagging.
- **Build and update Homebrew manually.** Run `scripts/package-companion.sh` with Rust 1.96.1, both macOS targets, and Zig 0.15 to produce one universal archive containing adjacent `wsx` and `herdr` executables plus notices. Verify both architectures, the checksum, optional signature, and bottle before creating the GitHub release. Update `Formula/wsx.rb` in `~/ws-ps/homebrew-tap` to install and test both binaries, with the final URL, version, archive SHA, bottle root URL, and bottle SHA; then validate, commit, and push. The tap remote may be ahead, so pull with rebase and retain the new release values on conflict.
- **Publish crates.io in dependency order.** Publish `wsx-core` first, wait until `0.X.Y` resolves from crates.io, then package and publish `wsx`. The crates.io `wsx` package cannot install a binary owned by another Cargo package, so cargo users must install pinned `herdr` 0.8.2 separately. Requires a valid crates.io token (`cargo login`); never tag a replacement version after a partial publish.
- **Herdr provenance.** `vendor/herdr` is the squashed official `v0.8.2` subtree at commit `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c`. Keep it excluded from the root workspace so routine wsx gates do not require Herdr's Rust/Zig toolchain.

## good-to-go

- **auwsx issue inspection**: use `"$AUWSX_BIN" issue get <issue_id>`; `issue show` and `issue --help` are unsupported.
- **auwsx finding inspection/adjudication**: `finding get` is unsupported; inspect with `"$AUWSX_BIN" finding ls "$AUWSX_ISSUE_ID" --open` or use the phase prompt's open-finding records, then call only `finding accept` or `finding reject`.
- **auwsx finding creation**: severities are `blocker|major|minor|nit`; subcommand help is unsupported, so use `"$AUWSX_BIN" --help` for callback syntax.
- **Issue-local durable memory**: the injected control outbox rejects `memory save`; do not retry it as a callback, and report the unavailable durable-memory step in the phase report.
- **Routine daemon smoke tests**: sandboxed `flock` can fail with `EPERM`, and detached children from an escalated command may be reaped before a later command; use one approved foreground harness for lifecycle assertions.
- **macOS isolated wsx smoke tests**: `dirs::config_dir` follows `HOME` rather than XDG variables on macOS. Set a repository-local `HOME` as well as XDG config/state/cache roots before running packaged wsx so tests do not read the user's project registry.
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
- **Pending ops pattern**: stale full Herdr/Git refreshes must not resurrect user intent. Session close uses stable pane IDs in `pending_session_kills`; worktree deletion uses exact paths in `pending_deletions`. Session rename changes only the Herdr label, so it never remaps identity.
- **Herdr is session authority**: wsx owns project/worktree projection and local mute only. Herdr owns pane identity, terminal attachment, output, lifecycle state, persistence, and agent restoration. Never add process-tree, pane-motion, bell, or terminal-text state inference back to wsx.
- **Herdr workspace ownership**: wsx associates a Herdr workspace only when its label has the reserved `wsx:` prefix and a pane cwd exactly matches the Git worktree path. Workspace create, session close, and worktree deletion are serialized across wsx processes, and projection rejects duplicate owned workspaces. Keep these checks when changing creation, refresh, or deletion behavior.
- **Clippy baseline**: 12 pre-existing warnings after the Herdr migration (1 in `wsx-core`, 11 in `wsx`). Changes must not add warnings.
- **Mobile mode interaction boundary**: `app.preview_area` is always `Rect::default()` in mobile mode — mouse click handler uses it to detect preview-area clicks. Any new click target added to the UI must account for the zero-size preview_area in mobile. New UI state that renders differently in mobile (`width < 60`) must update `app.is_mobile` consumers in `dispatch_normal` if action semantics change.
- **Session-state direct projection**: `SessionInfo` stores Herdr's raw `AgentStatus` plus wsx-local `muted`; `session_state::derive` maps those exhaustively to the three app states. UI/CLI must call that projection and never reinterpret Herdr status.
- **Mute is local and sticky**: mute is keyed by stable Herdr pane ID in the wsx cache. Only explicit interaction (`attach`, `send_text`, `send_ctrl_c`, `rename`) clears it via `App::unmute_on_interaction`; background output and navigation do not.

- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).

seek.collections.auto: wsx rust
seek.collections.extra: terminal git

## auwsx Knowledge Collections
<!-- auwsx:knowledge-collections -->
Agents should use these collection hints before answering project/domain questions or changing architecture:
- coding_language: rust
- project_domain: wsx
- knowledge_domains: coding, domain
- update_when: project stack, domain, or knowledge layout changes
