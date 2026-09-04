# Manual Rust release

Prefer a checked-in release script or configuration after reviewing it. If none exists, use this explicit sequence; stop when required targets or asset policy are unknown.

## Version and validation

1. Update only intended workspace package versions. Use configured tooling (`cargo set-version` when available) or targeted manifest edits; do not replace every `version =` line because dependency versions may be present.
2. Update internal dependency constraints and regenerate `Cargo.lock` with Cargo.
3. Run repository-required checks, at minimum formatting, tests, and release builds appropriate to the workspace:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
cargo build --workspace --release --locked
```

Run Clippy when project policy requires it. Inventory and build every shipped `[[bin]]`, including helper/preprocessor crates.

## Package registries

Follow [crates.io publishing](crates.md). Publishing is optional and requires separate confirmation. In a workspace, publish dependencies before dependents and verify registry availability between packages.

## GitHub assets

Use repository-configured target triples and builders (for example native Cargo, `cross`, or CI). Never claim cross-platform support from a host-only build. For each target:

1. Build release binaries reproducibly with the locked dependency graph.
2. Package executable(s), license, and README using the repository's naming convention.
3. Generate configured checksums and signatures. A checksum manifest must name the downloadable asset by basename, never a build directory or temporary path. From the artifact directory, run `shasum -a 256 -c <asset>.sha256` so the published manifest works after download.
4. Inspect archives and run smoke/version checks where executable on the host.

Commit manifest, lockfile, and changelog; push the release branch and explicit annotated tag as described in [GitHub Release](github-release.md). Create the release and upload all validated assets. If CI is actually responsible for assets, use the `rust-ci` flow instead of duplicating it locally.

## Homebrew

Skip prereleases unless policy and user explicitly require them. Resolve the tap only through `HOMEBREW_TAP_PATH`, repository release config, or `git config release.tapPath`. Require a clean tap checkout with verified remote.

Update the correct formula/cask with the final release URL and SHA-256 for each supported asset, preserving existing platform/architecture structure. Do not add an explicit `version` when Homebrew correctly derives it from the URL.

Before tap publication, run Ruby syntax and `brew style <formula-path>`. Modern Homebrew accepts strict audit and install by formula name, not an arbitrary path. If the authoring checkout is the registered tap, run `HOMEBREW_NO_INSTALL_FROM_API=1 brew audit --strict <owner/tap/formula>` before pushing. If it is a separate checkout, do not mistake an audit of the stale registered formula for candidate validation: publish the reviewed tap commit after confirmation, fast-forward the registered tap, then run strict audit, bottle install/upgrade, formula test, version smoke, and shipped-executable checks by fully qualified name. A post-push failure is a downstream partial release; fix it with a new tap commit rather than rewriting published history.

## Ordering and partial failure

Prefer validation → release commit/tag → immutable registry publish → GitHub assets → Homebrew, unless checked-in automation defines another safe order. Once any immutable publication succeeds, never reuse that version for changed content. Report partial success and resume downstream steps only.
