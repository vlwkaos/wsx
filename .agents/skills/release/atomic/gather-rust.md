# Gather Rust release inputs

Run in the project repository:

```bash
cargo metadata --no-deps --format-version 1
git remote get-url origin
git config --get release.tapPath || true
printf '%s\n' "${HOMEBREW_TAP_PATH:-}"
```

Inspect workspace members, `default-members`, `publish = false`, `[[bin]]`, release profiles, repository scripts, and tag/release workflows. Derive repository slug from the configured remote only after validating its host and URL form.

Ask the user to confirm or provide:

- **Version**: stable or prerelease.
- **Packages**: every workspace crate intended for publication and their dependency order.
- **Binaries**: every shipped executable, including extra bins and helper/preprocessor crates.
- **Repository**: remote and GitHub `owner/repo` when GitHub is used.
- **Targets/assets**: supported target triples, archive naming, checksums, and signing policy. Prefer checked-in release configuration; do not guess.
- **Destinations**: crates.io or another configured registry, GitHub Release, and Homebrew.
- **Tap**: resolve from `HOMEBREW_TAP_PATH`, repository-local release config, or `git config release.tapPath`; if Homebrew was requested and none resolves, stop with an explicit instruction to configure one. Do not search arbitrary home directories.

Validate that a configured tap path exists, is a Git repository, has the expected remote, and is clean before use. Credentials must come from the relevant CLI/configuration, never from arguments or chat.
