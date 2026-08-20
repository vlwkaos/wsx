# asched

release.flow: rust-ci

## Architecture

- `crates/asched-core` is the transplantable scheduler boundary. Keep it free
  of CLI and TUI dependencies.
- `crates/asched` is a thin CLI/TUI client.
- One daemon owns writes beneath one `ASCHED_ROOT`. Hosts connect through
  `RoutineClient`; they do not spawn independent schedulers with the same root.
- Project identity is the canonical working-directory path. The registered
  name is a grouping and selection label.
- Once `projects.toml` exists, it is also the scheduling allowlist. Project
  removal must stop future scheduled runs without deleting routine history.

## Change Gate

- Define failure boundaries before editing, especially daemon lifecycle,
  optimistic revisions, imports, and process cleanup.
- Run `cargo test --workspace`.
- Run `cargo clippy --workspace --all-targets -- -D warnings`.
- TUI changes require render tests and explicit human pass/fail criteria.
- New dependencies must be pinned and security-reviewed before installation.

## Release

- Project override to the generic `rust-ci` flow: CI owns crates.io publishing,
  so skip the generic flow's local `cargo publish` step.
- Run `ruby scripts/prepare-release.rb <version>` so the workspace version,
  exact `asched-core` dependency, and lockfile stay synchronized.
- Commit the version and changelog, then push `main` and tag `v<version>`.
- `.github/workflows/release.yml` publishes `asched-core` before `asched`,
  creates the universal macOS release, and updates `vlwkaos/homebrew-tap`.
- Release CI requires `CARGO_REGISTRY_TOKEN` and `TAP_TOKEN` repository secrets.

- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).

seek.collections.auto: asched rust
seek.collections.extra: architecture   # pass only when the task needs this expertise
