# Changelog

## Unreleased

## [0.2.0] - 2026-08-05

### Features

- Add provider-neutral event-triggered routines with durable event-ID
  deduplication, bounded JSON payload delivery, run-cause history, CLI and typed
  client fire APIs, cron-compatible state migration, and end-to-end coverage
  ([`8ba278c`](https://github.com/vlwkaos/asched/commit/8ba278c98038832761c2295b3d75bd1204d1793a)).

### Compatibility

- Existing v1 cron configuration, runtime history, and wsx imports remain
  readable without read-time rewrites.
- IPC advances to protocol version 3; all clients sharing an `ASCHED_ROOT` must
  upgrade together. Rust consumers must replace direct `Routine.cron` access
  with `Routine.trigger`
  ([`8ba278c`](https://github.com/vlwkaos/asched/commit/8ba278c98038832761c2295b3d75bd1204d1793a)).

### Bug Fixes

- Authenticate raw Git pushes before updating the Homebrew tap and cover the
  release-script contract
  ([`b92cd3b`](https://github.com/vlwkaos/asched/commit/b92cd3b17f864f5dbae801f76059596f92701411)).
- Publish the exact versioned changelog section as GitHub Release notes on both
  initial creation and workflow reruns
  ([`724f141`](https://github.com/vlwkaos/asched/commit/724f1419594d8873c0c55b8bbb9d2df1b541a22a)).

### Docs

- Document Homebrew, Cargo, and direct macOS archive installation
  ([`79a1cff`](https://github.com/vlwkaos/asched/commit/79a1cffd360132b4c9a8567d763a796d3133aa18)).
- Record and consolidate event-trigger architecture, lifecycle, migration, IPC,
  CLI, TUI, and wsx integration knowledge
  ([`76c04d2`](https://github.com/vlwkaos/asched/commit/76c04d299ba0b10ab8335d0001ba7a83afbb3ca1),
  [`f586914`](https://github.com/vlwkaos/asched/commit/f586914e165fa267a734189ffc5d44c72ff4a74e)).

### Other

- Synchronize workspace and package versions, the exact `asched-core`
  dependency, and the lockfile for 0.2.0
  ([`49360c6`](https://github.com/vlwkaos/asched/commit/49360c6560e630e769890108ac839c5119ad89a1)).
- Configure project knowledge collections and automatic release-skill discovery
  ([`25979ad`](https://github.com/vlwkaos/asched/commit/25979ad9ddfa9d5441b5c5f15c0aa7616e2bae3f)).

### Install

```sh
brew upgrade asched
```

or:

```sh
cargo install asched --version 0.2.0 --locked
```

## [0.1.0] - 2026-07-24

### Features

- Add generic project registration backed by canonical working directories,
  with registered projects acting as the scheduling allowlist
  ([`e12e654`](https://github.com/vlwkaos/asched/commit/e12e65446d17636ded6b5df9ecc32f8575872509)).
- Add routine CRUD, filtering, JSON output, run control, daemon lifecycle
  commands, and a minimal project/routine TUI
  ([`e12e654`](https://github.com/vlwkaos/asched/commit/e12e65446d17636ded6b5df9ecc32f8575872509)).
- Extract the scheduler, persistence, execution, IPC, and typed client into
  the reusable `asched-core` crate
  ([`e12e654`](https://github.com/vlwkaos/asched/commit/e12e65446d17636ded6b5df9ecc32f8575872509)).
- Add an explicit, non-overwriting wsx import with imported routines disabled
  by default
  ([`e12e654`](https://github.com/vlwkaos/asched/commit/e12e65446d17636ded6b5df9ecc32f8575872509)).

### Docs

- Document the CLI and TUI boundaries, crash-safe imports, durable scheduler
  state, shared daemon protocol, registry rules, run lifecycle, and release
  architecture
  ([`56652cc`](https://github.com/vlwkaos/asched/commit/56652cc93271373ad2db8adb9bb455c19623b12d),
  [`f2d5b8c`](https://github.com/vlwkaos/asched/commit/f2d5b8cfea6d9fbc9ec95504dedc6dc25cacd380)).

### Other

- Establish the MIT-licensed Rust workspace with pinned dependencies,
  repository configuration, and contract, render, and end-to-end scenario
  tests
  ([`e12e654`](https://github.com/vlwkaos/asched/commit/e12e65446d17636ded6b5df9ecc32f8575872509)).
- Add tag-driven crates.io publishing, universal macOS binaries, GitHub
  releases, and Homebrew tap updates with release-script coverage
  ([`79f108b`](https://github.com/vlwkaos/asched/commit/79f108be96a783b92f7da3f8c1a2e8b805f541b0)).
