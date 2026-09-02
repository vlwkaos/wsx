release.flow: rust

## Release

This is a Cargo **workspace** — the shared `~/.claude/skills/rust-release/release.sh` assumes a single-crate layout, so two manual steps are required every release:

- **Use the manual Rust release flow, not the shared `release.sh`.** The shared script requires a root `[package]` and exits with `Cargo.toml has no root [package] version to update`. Use `scripts/package-companion.sh` only for the universal companion archive; keep versioning, tagging, GitHub publication, crates.io publication, and Homebrew updates in the manual workspace flow.
- **Bump two version locations, not one.** Update root `[workspace.package].version` and the pinned path dependency in `crates/wsx/Cargo.toml` (`wsx-core = { path = "../wsx-core", version = "=X.Y.Z" }`), regenerate `Cargo.lock`, and run the locked workspace gates before tagging.
- **Build and update Homebrew manually.** Run `scripts/package-companion.sh` with Rust 1.96.1, both macOS targets, and Zig 0.15 to produce one universal archive containing adjacent `wsx` and `wsxd` executables plus notices. Verify both architectures, the checksum, optional signature, and bottle before creating the GitHub release. Update `Formula/wsx.rb` in `~/ws-ps/homebrew-tap` to install and test both binaries, with the final URL, version, archive SHA, bottle root URL, and bottle SHA; then validate, commit, and push. The tap remote may be ahead, so pull with rebase and retain the new release values on conflict.
- **Binary releases are archive/Homebrew only.** `wsx-terminal`, `wsx-daemon`, and `wsx` are not published because the pinned Ghostty source is workspace-vendored and the runtime requires adjacent `wsx`/`wsxd` binaries. Publish `wsx-core` only if its package gate passes; never advertise `cargo install wsx` for 0.18.
- **Herdr provenance.** `vendor/herdr` is the squashed official `v0.8.2` subtree at commit `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c`. It is reference-only: keep it excluded from the workspace, runtime, archives, and install instructions.

## good-to-go

- **auwsx issue inspection**: use `"$AUWSX_BIN" issue get <issue_id>`; `issue show` and `issue --help` are unsupported.
- **auwsx finding inspection/adjudication**: `finding get` is unsupported; inspect with `"$AUWSX_BIN" finding ls "$AUWSX_ISSUE_ID" --open` or use the phase prompt's open-finding records, then call only `finding accept` or `finding reject`.
- **auwsx finding creation**: severities are `blocker|major|minor|nit`; subcommand help is unsupported, so use `"$AUWSX_BIN" --help` for callback syntax.
- **Issue-local durable memory**: the injected control outbox rejects `memory save`; do not retry it as a callback, and report the unavailable durable-memory step in the phase report.
- **Routine daemon smoke tests**: sandboxed `flock` can fail with `EPERM`, and detached children from an escalated command may be reaped before a later command; use one approved foreground harness for lifecycle assertions.
- **macOS isolated wsx smoke tests**: `dirs::config_dir` follows `HOME` rather than XDG variables on macOS. Set a repository-local `HOME` as well as XDG config/state/cache roots before running packaged wsx so tests do not read the user's project registry.
- **Test runner**: use `cargo nextest run --workspace --locked` for unit/integration tests and `cargo test --workspace --locked --doc` for doctests, which Nextest does not support.
- **Runtime-smoke binary freshness**: `scripts/runtime-smoke.py` executes `target/debug/wsx` and `target/debug/wsxd`; rebuild both after source changes before treating a missing wire field as a protocol failure.
- **Staged-tree Cargo isolation**: validate an exported Git index with an export-local `CARGO_TARGET_DIR`; a shared target can retain binaries built from concurrent worktree sources.
- **Post-audit Cargo freshness**: if diagnostics cite `.work/index-check-*` or newly added tests are absent from `cargo test -- --list`, refresh touched source mtimes and clean only affected packages before retrying; a newer audit mirror can otherwise leave stale shared artifacts.
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
- **Exact Rust unit-test filter**: include the module path, e.g. `cargo test -p wsx-terminal tests::<name> -- --exact` or `cargo test -p wsx-core git::ops::tests::<name> -- --exact`; a bare function name with `--exact` selects zero tests.
- **Daemon shutdown**: use `wsx daemon stop`; it gracefully closes wsxd and live PTYs so saved session commands recreate on the next launch. There is no `routine daemon` command.

Recurring audit axes (auto-maintained by /good-to-go):
- **Pending ops pattern**: stale wsxd/Git refreshes must not resurrect user intent. Session close uses stable `SessionId` tombstones in `pending_session_kills`; worktree deletion uses exact paths in `pending_deletions`. Labels never remap typed identity.
- **wsxd is runtime authority**: wsxd owns sessions, panes, PTYs, Ghostty state, terminal text selection, frames, leases, revisions, and mutations. Clients own presentation projection, Workspace row selection, rendering, and local mute. Terminal text selection is controller-local Ghostty state scoped to the exact active lease generation. Never add process-tree, pane-motion, bell, or terminal-text agent-state inference.
- **Terminal stream performance**: Terminal mode uses one same-connection handshake and persistent duplex stream. Input stays off the TUI thread; subscribe applies initial dimensions before ACK. Protocol-4 snapshots add ephemeral pane listener metadata; terminal updates retain protocol-3 compact Ghostty wide/spacer occupancy, suppress child synchronized-output intermediates, and send one full baseline followed by exact-base dirty-row patches. Baseline mismatch requests full resync. Never restore byte-at-a-time frame reads, per-key request sockets, or foreground frame polling.
- **Protocol evolution**: Additive wire fields must deserialize with defaults so a new client can read an older daemon's Hello before deciding to upgrade it. Compatibility tests must feed raw legacy JSON that omits the new field; serializing fixtures with current structs cannot prove backward decoding. Protocol 9 adds ephemeral PTY foreground-job metadata. Protocol 10 adds controller-local terminal selection ranges and pointer-boundary metadata.
- **Project/worktree ownership**: Git remains authoritative for registered worktree paths; wsxd assigns stable typed project/worktree IDs during synchronization. Session/pane mutations use expected revisions, and deletion closes associated sessions before removing a worktree.
- **Clippy baseline**: the wsx-owned runtime targets a zero-warning workspace gate. Do not add warnings.
- **Mobile mode interaction boundary**: `app.preview_area` is `Rect::default()` in mobile Workspace mode and the full main viewport in mobile Terminal mode. `app.terminal_area` excludes the one-row breadcrumb. New click targets or responsive state (`width < 60`) must preserve those separate panel/content rectangles and update `app.is_mobile` consumers when semantics change.
- **Workspace view contract**: One persistent full-width, whole-chip horizontally scrollable group header remains visible above both Workspace and Terminal content. Workspace content begins directly below it; Terminal places its breadcrumb directly below it. Projects may belong to multiple persisted groups. Workspace always applies exactly one group filter; the first virtual **ungrouped** anti-group matches no memberships and is the default when independently persisted selection data is missing or invalid. Restore the last valid group, never render an **all** no-filter control, and never let unrelated workspace-cache writes publish stale in-memory group intent. Navigation and terminal input do not count. Virtual groups are never assignable. The footer is always one context-prioritized row with parenthesized key hints. Terminal frames, PTY dimensions, cursor placement, and mouse coordinates use the complete remaining content rectangle without wsx padding. A header or left-panel click leaves Terminal and applies the Workspace action in the same interaction; wheel scrolling over the header does not enter the terminal stream. TUI send-text is intentionally absent while CLI send commands remain available.
- **Session-state direct projection**: `SessionInfo` stores normalized `AgentState`, ephemeral pane foreground-job metadata, wsx-local `muted`, and revision-bound local outcome acknowledgement; `session_state::derive` maps agent lifecycle exhaustively and gives non-agentic foreground jobs a separate Running app state. Session rows use a light-green `◎ ◉ ● ◉` working pulse, yellow `○` idle, red `◐` blocked, green `✓` unacknowledged done, static light-green `●` for ordinary shell foreground jobs, and only adapter-reported agent names. Explicit terminal interaction acknowledges an exact done revision and projects it as idle; a newer done report needs attention again. Process metadata never changes agent lifecycle; provider-specific interpretation stays in adapters.
- **Listener metadata**: wsxd may map bounded `lsof` TCP listener results to pane process groups, but never infer agent state from listeners or process trees. Scans run outside daemon locks, preserve prior data on scanner failure, and remain ephemeral across restart.
- **Mute is local and sticky**: mute is keyed by stable wsx `TerminalId` in the cache. Only explicit interaction (`Terminal` entry, send text, interrupt, rename) clears it via `App::unmute_on_interaction`; background output and navigation do not.

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
