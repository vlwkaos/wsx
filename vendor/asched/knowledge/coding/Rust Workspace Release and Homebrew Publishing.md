---
slug: rust-workspace-release-and-homebrew-publishing
kind: coding
title: Rust Workspace Release and Homebrew Publishing
description: Documents coordinated Rust workspace versioning, dependency-order publishing, universal packaging, and stable Homebrew updates.
keywords: [Rust, Cargo, workspace, crates.io, Homebrew, release, versioning, Clippy, rustfmt, macOS, token]
created: 2026-07-24
modified: 2026-08-05
---
# Rust Workspace Release and Homebrew Publishing

- `release.flow: rust-ci` selects the repository-native tag workflow because the generic single-crate release script cannot safely publish this workspace.
- Rust `1.93.0`, Clippy, rustfmt, and both macOS targets are pinned in `rust-toolchain.toml`.
- `scripts/prepare-release.rb` updates the workspace version and exact `asched-core` dependency together, then rolls both back if Cargo validation fails.
- Release CI publishes `asched-core` before `asched`, retries registry index propagation, builds one universal macOS archive, creates the GitHub release, and updates Homebrew only for stable tags.
- Every release requires a nonempty versioned `CHANGELOG.md` section. CI extracts it as the GitHub Release body and reapplies it on safe workflow reruns instead of using generated notes.
- CI requires `CARGO_REGISTRY_TOKEN` and a tap-scoped `TAP_TOKEN`.
