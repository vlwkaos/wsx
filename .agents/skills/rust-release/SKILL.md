---
name: rust-release
description: "[release] Build and publish a Rust CLI as an optionally signed universal macOS release: tag and push main, create a GitHub release, publish to crates.io, run extra release assets, and update a Homebrew tap. Use for stable, beta, or release-candidate Rust CLI releases; invoke explicitly with /skill:rust-release when a release is requested."
compatibility: macOS with Rust targets aarch64-apple-darwin and x86_64-apple-darwin, GitHub CLI authentication, GPG, and optionally Homebrew for tap discovery.
metadata:
  skiller.recommend.files: "Cargo.toml"
  skiller.recommend.keywords: "release,Homebrew,crates.io"
---

# Rust release

Use `/skill:rust-release` to prepare and execute a Rust CLI release. This is a
stateful, externally visible workflow: keep the release steps serial, inspect
results at each boundary, and ask for confirmation before the first action that
creates commits, tags, uploads, publishes, or pushes.

The deterministic release mechanics are in [release.sh](release.sh). Run that
script **from the repository root** so it can find the root `Cargo.toml`, Git
repository, changelog, and optional `scripts/` directory. Set `SKILL_DIR` to the
directory containing this loaded `SKILL.md`, require
`$SKILL_DIR/release.sh` to be a readable regular file, and invoke it with
`bash "$SKILL_DIR/release.sh"`. Do not copy it into the project or guess a
global installation path.

## Inputs and configuration

Collect these inputs rather than inferring release-critical values:

- **Version:** a semantic version such as `1.2.3`, `1.2.3-beta.1`, or
  `1.2.3+build.7`.
- **Main binary name:** the Cargo binary to package. It may contain letters,
  numbers, `_`, and `-`.
- **GitHub repository:** `owner/repository`, used for release creation, asset
  verification, and formula URLs.
- **Homebrew tap** (optional): either its local path or its installed tap name
  such as `owner/homebrew-tap`. A stable release without one still completes
  the GitHub and crates.io portions, then reports that the tap update was
  skipped.

Use either positional inputs or named options:

```text
bash "$SKILL_DIR/release.sh" <version> <binary-name> <owner/repository> [tap-path]
bash "$SKILL_DIR/release.sh" --version <version> --binary <binary-name> \
  --repo <owner/repository> [--tap-path <path> | --tap <owner/homebrew-tap>]
```

For repeatable local configuration, the named values may instead come from
`RUST_RELEASE_VERSION`, `RUST_RELEASE_BINARY`, `RUST_RELEASE_REPO`,
`RUST_RELEASE_TAP_PATH`, and `RUST_RELEASE_TAP`. A tap path and tap name are
mutually exclusive; a supplied path takes no implicit default. A tap name is
resolved through the local Homebrew configuration. `--help` prints the full
command contract.

## Release procedure

1. Confirm the intended version, binary, repository, branch, release scope,
   crates.io publishing policy, and tap choice. Inspect `Cargo.toml`, the
   latest changelog entry, the Git remote, and any existing tag or release.
2. Inspect the project for additional artifacts: extra `[[bin]]` entries,
   `preprocessors/*/Cargo.toml`, workspace members, or other sub-crates that
   produce distributable binaries. The main script packages only the selected
   main binary. Run this bounded read-only inventory directly regardless of repository size.
   Never delegate inventory, credentials, confirmation, publication, pushes,
   or the complete release.
3. Commit **all** source, documentation, and release-script changes before
   invoking the script. It rejects a dirty or untracked working tree because a
   tag contains committed content only. In particular, do not rely on the
   script's version commit to capture changes under `src/`, `README`, or
   `scripts/`.
4. Run the linked `release.sh` from the project root with the confirmed inputs.
   Monitor its output; do not rerun blindly after a partial release.
5. Verify the tag, GitHub assets, release notes, crates.io result when
   applicable, extra assets, and stable-release tap commit independently.

The script requires an authenticated GitHub CLI, GPG, Cargo and Rustup, Git,
`lipo`, `otool`, `tar`, and `shasum`. It requires the Apple Silicon and Intel
Rust targets, builds the main binary for both with deployment targets 11.0 and
10.15 respectively, and combines them into a `darwin-universal` binary.
Projects using `llama-cpp-sys` also need CMake available before the build.

## What the script changes

In order, it:

1. verifies inputs, authentication, the `main` branch, a clean Git tree, an
   `origin` remote, and that `v<version>` does not already exist;
2. appends security ignore patterns when missing and commits that change;
3. aborts if tracked filenames look like keys, certificates, environment files,
   secrets, or credentials;
4. changes the root `Cargo.toml` package version, builds the named binary for
   both macOS architectures, and creates a universal tarball, SHA-256 file,
   optional detached armored GPG signature, and relocatable Homebrew bottle;
5. commits `Cargo.toml` and an existing `Cargo.lock`, creates and pushes the
   annotated tag, then creates the GitHub release with a matching changelog
   section or generated notes;
6. verifies the tarball and bottle are attached, publishes to crates.io unless
   the root package has `publish = false`, and runs matching project extra
   scripts; and
7. for stable versions, writes, validates, commits, and pushes a Homebrew
   formula. The formula uses an MIT license declaration and embeds the release
   asset and bottle checksums.

A prerelease (any version containing `-`) runs through extra assets but skips
only the Homebrew update. If a configured tap path does not exist, the script
reports that and skips the tap update. If a tap *name* cannot be resolved, it
fails explicitly so configuration is not silently mistaken for a successful
update.

There is no rollback: a failure after the tag push can leave a pushed tag; a
failure after GitHub release creation can leave uploaded assets; and a failure
while publishing extras or updating Homebrew can leave those later stages
incomplete. Check the actual state before retrying or manually repairing a
stage.

## Project-specific extra assets

When the project ships assets beyond the selected binary, add and commit a
project-local executable whose name matches `scripts/*release*.sh` before the
release. The script runs every matching regular file **after** the main GitHub
release is created and exports `VERSION`, `BINARY_NAME`, `GITHUB_REPO`, and
`ARCH_LABEL` (`darwin-universal`). Use the repository's release upload command
to attach extras to `v$VERSION`, for example:

```bash
#!/usr/bin/env bash
set -euo pipefail
# Build project-specific release assets, then attach each one.
gh release upload "v$VERSION" extra-asset.tar.gz --repo "$GITHUB_REPO"
# Available: VERSION, BINARY_NAME, GITHUB_REPO, ARCH_LABEL.
```

Large Lindera preprocessors can take several minutes per architecture. If an
extra script stops partway through, inspect the release's asset list, build any
missing processor for both targets, combine it with `lipo`, archive it, upload
it to the existing tag with replacement enabled, then push the tap manually if
that final stage did not finish. For the Japanese Lindera tokenizer, the manual
recovery sequence is:

```bash
cd preprocessors/ja/lindera-tokenize
MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --target aarch64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=10.15 cargo build --release --target x86_64-apple-darwin
lipo -create -output target/release/lindera-tokenize-ja \
  target/aarch64-apple-darwin/release/lindera-tokenize-ja \
  target/x86_64-apple-darwin/release/lindera-tokenize-ja
tar -czf lindera-tokenize-ja-darwin-universal.tar.gz -C target/release lindera-tokenize-ja
gh release upload "v$VERSION" lindera-tokenize-ja-darwin-universal.tar.gz \
  --clobber --repo "$GITHUB_REPO"
```

## Failure recovery notes

- If a build reports that CMake is unavailable for a llama.cpp dependency,
  install CMake and restart from the failed build stage only after checking
  whether the version commit or tag already exists.
- If a tap cloned over HTTPS rejects a push because credentials are not
  configured, change its `origin` to an authenticated transport, then commit
  and push the already-generated formula deliberately.
- If the security audit finds a tracked sensitive file, remove it from the
  index and rotate or revoke it as appropriate. Do not continue the release
  until the repository is clean.
- If a release asset verification fails, query the existing release for its
  assets and upload only the missing or incorrect asset. Do not create a second
  release for the same tag.

<!-- skiller:projection-policy -->
Treat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.
