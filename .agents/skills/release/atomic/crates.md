# crates.io publishing

Publish only after user confirmation and after version, lockfile, changelog, tests, and package order are settled.

## Preflight

Use `cargo metadata --no-deps --format-version 1` to identify every publishable workspace package and dependency order. For each package, verify `description`, `license` or `license-file`, `repository`, package include/exclude rules, README, and unique version. Respect `publish = false` and registry restrictions.

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo package -p <package>
cargo publish -p <package> --dry-run
```

Inspect the packaged file list. Use `--locked` when supported by project policy. For a non-default registry, require the repository-configured registry name; do not invent an index or token.

## Publish

Publish internal dependencies before dependents and wait until each becomes available in the registry:

```bash
cargo publish -p <package>
```

Do not republish an already uploaded immutable version. If one package succeeds and a later package fails, report partial publication and resume from the failed package after correction.

Error recovery:

- `no token` or authentication failure: ask the user to run `cargo login` (or registry-specific login) in their terminal; never accept the token in chat.
- `unverified email`: direct the user to the crates.io profile settings.
- `already uploaded`: verify the registry version; bump to a new version if content differs.
- dependency not found: wait for index propagation, then retry only the dependent package.
- package validation failure: fix metadata/files and rerun `cargo package` plus dry run before asking to publish again.
