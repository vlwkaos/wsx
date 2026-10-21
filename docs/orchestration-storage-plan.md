# Orchestration and storage plan

Plan, 2026-10-21. Parallel tasks fill the disk because every task builds its own Rust `target/`. The fix is a bounded pool of reusable working copies and smaller debug builds, plus a manager that admits, merges and cleans up work instead of starting a new copy for every request.

## Goal (owner, 2026-10-21)

Manager and workers exist to lower the owner's context and decision load and to coordinate the work so it is finished correctly sooner. Spreading work over many tasks is not a goal. The manager splits work only when that finishes it sooner, merges overlapping requests, and asks the owner only for the owner's decisions. Measures: time from request to landed, how often the owner was needed, and rework from conflicts or duplicate work. Storage exists so that disk never blocks or slows that work.

## What happened

| Fact | Value |
| --- | --- |
| Disk | 228 GiB; reached 100% full on 2026-10-20 about 20:20 |
| `~/ws` | 88 GB, mostly Rust `target/` in task working copies |
| One task copy | 4.4 to 6.2 GB of `target/` (debug build) |
| Feature worktree | 10 GB |
| `vendor/asched/target` | 1.2 GB in every copy, rebuilt each time (excluded from the workspace) |
| Copies alive at once | 10 (wsx t9 to t18) |
| Free on 2026-10-21 | 5.9 to 9 GiB while tasks #13 to #17 build |

## Options

| Option | What it is | Disk effect here | wsx change | Status |
| --- | --- | --- | --- | --- |
| **B. Smaller debug builds** | Task builds get `CARGO_PROFILE_DEV_DEBUG=line-tables-only` and `CARGO_INCREMENTAL=0` | Guess 30 to 60% per `target/`; no published numbers, to be measured on one task | Build environment of the worker only | Stable cargo settings |
| **C. Slot pool** | 3 or 4 reusable working copies, each with a warm `target/`. A task leases a slot, resets it to its branch (`git clean` that spares `target/`), builds incrementally, releases it | Total no longer grows with requests: about 12 to 18 GB at 3 slots, 6 to 10 GB with B | Copy creation (lease instead of clone); manager queues when no slot is free | Same model as Codex managed worktrees (keeps the newest 15) |
| **A. Shared build output** | One `build.build-dir` (stable since Cargo 1.91) or `CARGO_TARGET_DIR` for all tasks | 80 to 90% saved, but builds take turns on cargo's lock, and branches rebuild each other's crates | Build environment | Fine-grained locking is nightly only |
| **D. sccache 0.18** | Shared compiler cache | Mostly speed, not disk; the cache can fill the disk too | Build environment | Released 2026-09-14 |
| **E. Jujutsu 0.46** | One working-copy commit per agent; `jj undo`; dependent changes rebase by themselves; conflicts stored as data | Almost nothing: source files are small; build output is the problem | A version-control backend in wsx (large) | Released 2026-10-07 |

Coming in Rust: a new layout for intermediate build files is scheduled for Cargo 1.100 (2026-11-12, not confirmed in release notes). A cache shared between workspaces is a 2026 project goal, nightly design only, no stable date.

## Manager policy

The manager owns its working copies and decides when work starts. These rules hold with any option above.

1. **Admission.** Start or lease a copy only above a free-space floor (`tasks.minFreeGb`, plus the size of one more copy). Below it the request waits as "Disk full: n GB free", without spending a retry, and starts when space is back.
2. **Ownership.** The manager keeps a list of its slots and copies, removes leftovers on start, and runs `cargo clean` on a landed copy at once. Finished copies keep a patch and the branch, not the build.
3. **Merging requests.** Before starting, the manager predicts which files and crates a request touches: paths it names, crate names, and `git log` history of files that change together. Requests that overlap run in one task and one slot, one after another, and are marked up front as one landing batch.
4. **Landing.** One at a time per project: rebase, build, test, then land. Independent tasks in separate slots build in parallel.

## Recommendation and order

1. Option B now, and measure one task before and after.
2. Option C with 3 slots, plus admission and ownership in the manager (M6 items 5 and 6).
3. One shared build output for `vendor/asched`, which saves 1.2 GB per copy.
4. Merging overlapping requests, once the pool works.
5. Defer jj, sccache and the cargo cache; look again when Cargo 1.100 ships.

No storage change is built yet. The owner has not chosen; the default above stands unless changed.

## Sources

- [Cargo changelog](https://doc.rust-lang.org/nightly/cargo/CHANGELOG.html)
- [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
- [Rust goal: cross-workspace cache](https://goals.rust-lang.org/2026/cargo-cross-workspace-cache.html)
- [Rust goal: build-dir layout](https://goals.rust-lang.org/2025h2/cargo-build-dir-layout.html)
- [sccache releases](https://github.com/mozilla/sccache/releases)
- [jj releases](https://github.com/jj-vcs/jj/releases)
- [Git worktrees vs Jujutsu for coding agents](https://wavect.io/blog/git-worktrees-vs-jujutsu-ai-coding-agents/)
- [Codex worktrees](https://learn.chatgpt.com/docs/environments/git-worktrees)
- [Claude Code worktrees](https://code.claude.com/docs/en/worktrees)
- [APFS clones](https://eclecticlight.co/2020/04/14/copy-move-and-clone-files-in-apfs-a-primer/)
