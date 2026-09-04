---
name: release
description: "[release] Plan and execute production releases for Rust, Node.js, GitHub, and tag-driven GitOps projects, with complete English or Korean changelogs, security checks, package publishing, GitHub Releases, binaries, Homebrew, and CI/CD verification. Use for release preparation, version/tag publishing, or diagnosing a release pipeline."
metadata:
  skiller.requires: "dream,gh-pr,github-pat-config,ko-reader-brief,rust-release"
compatibility: Requires Git and the project ecosystem tools; GitHub operations require the gh CLI. Publishing requires existing registry and repository credentials.
---

# Release

Run from the repository being released. Treat publish, tag, force-push, and deployment operations as irreversible: show the exact action and obtain confirmation immediately before the first irreversible operation. Never print credentials.

Explicit invocation accepts `/skill:release`, `/skill:release <version>`, `/skill:release <version> <flow>`, or named values such as `version=1.2.3 flow=node-ci`. Versions may be stable or prerelease (for example `1.2.3-beta.1`). A named flow overrides detection after confirmation. Reject conflicting or malformed inputs rather than guessing.

## Dependency routing

- `$knowledge:dream` | required | Load before release work when project sessions need consolidation.
- `$github:gh-pr` | optional | Load when the approved release flow must prepare or create a pull request.
- `$github:github-pat-config` | optional | Load when GitHub authentication or repository permissions block the selected flow.
- `$writing:ko-reader-brief` | optional | Load before writing a Korean changelog or release note when the repository has no house-style document of its own.
- `$release:rust-release` | optional | Load when the project matches its specialized signed universal macOS Rust CLI workflow.

A dependency is reachable regardless of its configured mode but remains unloaded until its condition applies. Follow the dependency's own confirmation and safety gates; routing never authorizes a push, publication, credential change, or release by itself.

## 0. Consolidate project knowledge when applicable

If `knowledge/` exists and `knowledge/sessions/*/session-*.md` contains unconsolidated sessions, invoke `/skill:dream` before release work. If that skill is unavailable, report the condition and ask whether to continue; do not silently discard or alter project knowledge.

## 1. Select and persist the flow

First inspect `release.flow:` in `AGENTS.md`. An explicit user override wins. Otherwise, if no saved flow exists, capture the repository root, change to the directory containing this `SKILL.md`, and run the bundled detector with that root. Never look for the detector in the project repository:

```sh
./scripts/detect-flow.sh <project-root>
```

The detector returns one of:

- `rust-ci`: Cargo project whose tag workflow builds/releases or publishes crates
- `rust`: other Cargo project
- `gh-ko-gitops` / `gh-en-gitops`: tag-driven container publish/deploy, selected by changelog language
- `node-ci`: npm/pnpm publish workflow
- `node`: versioned Node package without publish CI
- `gh-ko` / `gh-en`: GitHub-only release, selected by changelog language

Confirm the flow and explain the decisive signals. If detection reports an error or ambiguity, inspect the workflows and ask the user. Persist the confirmed value as `release.flow: <flow>` under an existing `## Release` section in `AGENTS.md`, or append a new section. Do not create duplicate keys. A stale saved flow must be corrected when repository automation no longer matches it.

Ask for a version if none was supplied. Confirm whether the repository requires a `v` tag prefix, the release branch (do not assume `main`), and all requested destinations. Check that the version does not already exist in manifests, tags, package registries, or GitHub Releases unless the user is explicitly performing approved recovery.

## 2. Universal gates

Before any publish or remote push:

1. Identify the repository root, release branch, remote, tag convention, package manager/lockfile, and relevant workflows.
2. Require a clean working tree before edits. Preserve unrelated user changes; never reset, clean, or stash them without consent.
3. Fetch remote and tags. Ensure the local release branch is not behind or diverged. Run project tests/builds required by its documented release process. Inspect pinned CI setup actions and requested tool versions; a local tool's availability does not prove that the pinned CI installer supports it.
4. From the directory containing this `SKILL.md`, run `./scripts/security-audit.sh <project-root>` and follow [the security policy](atomic/security-audit.md). Never resolve this script from the project repository. Any finding blocks release until resolved or explicitly accepted by the user.
5. Determine credentials by testing the relevant CLI (`gh auth status`, `cargo login`/publish dry run where supported, `npm whoami` or `pnpm whoami`). Never request a token in chat.
6. Present a release plan, including commits, tag, registries, workflows, environments, and prerelease effects. Confirm before publishing or pushing.

Use repository scripts and documented workflows when present; inspect them before execution. Do not invent missing signing, target, tap, registry, or deployment configuration.

## Changelog coverage invariant

Every changelog flow must cover the complete release range:

1. Resolve the previous **published release** from changelog and release metadata, then verify its tag is reachable from `HEAD`. Do not use an unrelated, draft, or unpublished tag merely because it is nearest.
2. Inventory every commit (including merges) and changed path in `<previous-tag>..HEAD`; the previous tag is excluded and `HEAD` included. Never sample a fixed commit count.
3. Map every commit and path to at least one bullet. Related commits may share a bullet, but preserve their full scope; group mechanical work explicitly.
4. Reconcile the draft against both inventories before finalizing.
5. With no previous release, state that this is the first release and cover complete history through `HEAD`.

Use [English changelog](atomic/changelog-en.md) or [Korean changelog](atomic/changelog-ko.md). Changelog analysis is agent judgment; shell commands only collect evidence.

Map release coverage directly regardless of range size. Use a read-only `delegate` reviewer only when a high-impact release needs a fresh coverage judgment isolated from the parent's changelog work. Supply the frozen commit/path map, changelog, coverage invariant, and required unmapped findings; prohibit writes, publishing, and further delegation. Verify every finding in the parent.

## 3. Execute the selected flow

### `rust`

Manual Rust release: gather [Rust inputs](atomic/gather-rust.md), run the [security audit](atomic/security-audit.md), write the English changelog, then follow [manual Rust release](atomic/rust-manual.md). This includes version/lockfile update, tests, package ordering, optional crates.io publishing, GitHub assets, and optional Homebrew update.

For extra `[[bin]]` targets or preprocessor/helper crates, inventory every shipped executable and use the repository's release script if present. Otherwise require explicit target/archive configuration and build all named assets; never silently release only the main binary.

Prereleases skip Homebrew by default. Ask separately about crates.io and Homebrew.

### `rust-ci`

CI builds the GitHub release/assets and optionally crates/Homebrew on a release tag.

1. Audit and inspect the complete workflow, including trigger, expected tag, package publish, artifact targets, GitHub Release, tap checkout/push, secrets, and prerelease behavior.
2. If CI clones a tap with `gh` then uses raw `git push` under `GH_TOKEN`, require and verify `gh auth setup-git` in the workflow. A successful clone does not authenticate a later push.
3. Write the English changelog. Update the intended Cargo package/workspace version and lockfile; prefer a repository tool or `cargo set-version` when configured, otherwise make a targeted manifest edit. Run `cargo check --locked` plus repository tests.
4. Commit the manifest, lockfile, and changelog. After confirmation, push the release branch alone. If required CI runs on branch pushes, wait for every exact-commit run and require success before creating or pushing the annotated release tag. Push only the explicit tag ref.
5. Publish locally to crates.io only if CI does not and the user confirms; follow [crates.io publish](atomic/crates.md).
6. Watch the exact tag-triggered run with `gh run list`/`gh run watch`. Verify the GitHub Release, every expected asset, checksums/signatures, and tap update—not merely a green job.

A prerelease may still trigger Homebrew in CI. Confirm that consequence before tagging. If a tap credential or required secret is absent, stop before tag creation.

### `node`

1. Confirm npm versus pnpm from lockfiles and `packageManager`; if a publish workflow exists, switch to `node-ci`.
2. Validate package visibility, workspace package(s), registry, provenance/signing policy, and whether scoped packages require `--access public`.
3. Bump all intended package versions with the selected package manager so lockfiles stay consistent. Write the English changelog.
4. Run repository tests/build and `npm pack --dry-run` (or the package-manager equivalent); inspect included files and secrets.
5. Confirm, then publish with the required tag/access/provenance options. For prereleases, require an explicit non-`latest` dist-tag unless policy says otherwise.
6. Commit version, lockfile, and changelog if not already committed, then use [GitHub Release](atomic/github-release.md).

### `node-ci`

1. Follow [Node workflow detection](atomic/detect-node-workflow.md). Extract branch/tag/manual trigger, registry, package manager, secret names, provenance, and whether CI creates a GitHub Release.
2. Validate the correct release branch and clean tree. Bump every intended `package.json` (and `Cargo.toml` only for an actual hybrid package), update lockfiles, and write the English changelog.
3. Run tests/build/pack dry run. Commit and push the release branch after confirmation. If required CI runs on branch pushes, wait for every exact-commit run and require success before continuing.
4. If and only if the workflow is tag-triggered, create an annotated tag and push its explicit ref. Do not push all tags.
5. If CI does not create the GitHub Release, follow [GitHub Release](atomic/github-release.md) in “existing release commit” mode.
6. Watch the exact workflow run and verify the published registry version/dist-tag and release URL. Report branch, commit, tag, package URL, run URL, and release URL.

### `gh-ko` and `gh-en`

Require the configured release branch, clean tree, `gh`, and authenticated remote. Write the [Korean](atomic/changelog-ko.md) or [English](atomic/changelog-en.md) changelog, then follow [GitHub Release](atomic/github-release.md).

### `gh-ko-gitops` and `gh-en-gitops`

Use the corresponding changelog language, then:

1. Inspect tag filters and image publish/deploy steps. Determine whether branch push deploys dev and which overlays tag push updates. For a first app release, inspect the configured recipes/config repository and compare service/ingress shape with a known-good app.
2. Confirm version/tag, target overlays, branch-push effects, image registry, and whether a tag already exists. Never overwrite a tag without explicit approval.
3. Follow [GitHub Release](atomic/github-release.md), pushing only `refs/tags/<tag>`. Never use `git push --tags`.
4. Verify both branch and tag runs; image tags/digests; recipes commit (`newTag` and digest); Argo sync and pod readiness for every target; Service endpoints; ingress events; and external `/health` routing.
5. A `503 no healthy upstream` with Ready pods may indicate Service/Ingress shape (including platform-specific `ClusterIP` versus `NodePort`). A timeout with healthy pods/endpoints points toward L4, DNS, ingress propagation, or platform routing until disproved.

Rebuilding an existing release tag is exceptional. After the fix is committed and only with explicit approval:

```bash
git push origin <release-branch>
git tag -f "<tag>"
git push --force origin "refs/tags/<tag>"
```

Explain that consumers may have cached the old tag/artifacts. Prefer a new patch version.

## Failure and recovery

Stop on a failed gate, test, build, publish, push, workflow, or verification. Report what succeeded and what may be externally visible. Do not continue to later destinations, retry a publish blindly, delete/recreate a release, move a tag, or force-push without a new user decision. If publication succeeded but later steps failed, preserve the immutable version and resume only the failed downstream step. A release with failed or unresolved required CI is partial even when every requested destination is externally visible; do not mark its goal complete until CI succeeds or the user explicitly removes that gate. End with version, commit, tag, destinations, CI/release URLs, deployment health, and unresolved failures.

<!-- skiller:projection-policy -->
Treat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.
