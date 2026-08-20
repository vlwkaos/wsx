# Releasing asched

The tag-driven release workflow publishes both crates, a universal macOS
binary, a GitHub release, and `Formula/asched.rb` in
`vlwkaos/homebrew-tap`.

## One-time repository setup

Add these GitHub Actions repository secrets:

| Secret | Purpose |
| --- | --- |
| `CARGO_REGISTRY_TOKEN` | Publish `asched-core`, then `asched`, to crates.io |
| `TAP_TOKEN` | Push `Formula/asched.rb` to `vlwkaos/homebrew-tap` |

`TAP_TOKEN` needs contents write access to the tap repository. The workflow
uses the built-in GitHub token only for this repository's release.

## Release

```sh
ruby scripts/prepare-release.rb 0.2.0
```

Move the `Unreleased` changelog entries under `## [0.2.0] - YYYY-MM-DD`, then
run the normal test gate and commit. Release CI extracts that exact versioned
section as the GitHub Release body on both initial creation and reruns, so every
release must have a nonempty versioned changelog section:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git add Cargo.toml Cargo.lock crates/asched/Cargo.toml CHANGELOG.md
git commit -m "v0.2.0"
git push origin main
git tag -a v0.2.0 -m v0.2.0
git push origin refs/tags/v0.2.0
```

The workflow verifies that the tag matches both Cargo version locations.
Stable tags update Homebrew; prerelease tags publish crates and GitHub assets
but skip the tap. Re-running a tag skips crates that already exist and replaces
release assets safely.

Before the first `asched-core` publication, local `cargo package -p asched`
cannot resolve its exact registry dependency. Validate `asched-core` locally;
release CI publishes it first and retries `asched` while the index propagates.
