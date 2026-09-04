# Detect Node publish workflow

## Find candidates

```bash
grep -RIlE 'npm (publish|publish\s)|pnpm publish|yarn npm publish' .github/workflows 2>/dev/null
```

If none exists, report that there is no detected publish workflow and use manual publish only after checking repository documentation. Multiple matches are not interchangeable: inspect all and identify the one that publishes the intended package.

## Inspect, do not execute

Read each complete YAML file and any local actions/scripts it calls. YAML `on` may parse unexpectedly in generic YAML tools, so verify trigger text directly. Extract:

| Item | Evidence |
|---|---|
| Trigger | branch push, tag pattern, release event, dispatch, or reusable call |
| Constraints | paths, branches, tag prefix, environment approvals, concurrency |
| Package manager | lockfile, Corepack/setup action, publish command |
| Registry/access | setup action registry URL, scope, dist-tag, public/private |
| Authentication | secret/environment variable names only; never values |
| Build gates | install mode, tests, build, pack, provenance/signing |
| Version source | manifest, tag, changeset, release-please, or script |
| GitHub Release | `gh release`, release action, or called workflow |
| Runner/artifacts | OS, architecture, uploaded assets, retention |

Do not classify a workflow as branch- or tag-triggered from a publish command alone. Follow reusable workflows and called scripts.

## Decide local work

- Branch trigger: commit and push only the required branch; do not add a tag unless another verified step requires it.
- Tag trigger: push the release commit first, then an explicit annotated tag ref matching the filter.
- Release-event trigger: determine whether creating the GitHub Release must precede package publish and avoid a circular plan.
- Manual trigger: report required inputs and obtain confirmation before dispatch.
- If CI creates the GitHub Release, skip manual creation.
- If it does not, follow [GitHub Release](github-release.md) after the release commit/tag is available.

If triggers or ownership remain ambiguous, stop and ask rather than causing duplicate publication.
