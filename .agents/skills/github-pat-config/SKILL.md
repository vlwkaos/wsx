---
name: github-pat-config
description: "[github] Configure, rotate, or diagnose GitHub fine-grained personal access tokens (PATs) for repository automation, CI secrets, releases, workflow dispatch, repository access, and Homebrew tap updates. Use to map required operations to least-privilege repository permissions and resolve authorization failures."
user-invocable: false
---

# GitHub Fine-Grained PAT Configuration

Use this skill when creating, rotating, reviewing, or debugging a GitHub fine-grained personal access token (PAT). Invoke it explicitly as `/skill:github-pat-config` when needed.

## Intake and safety

Establish these inputs before recommending a token configuration:

- **Operation(s):** the exact API, Git, CLI, or web action, including whether it reads or writes.
- **Resource owner and repositories:** the personal account or organization and the specific repositories. A fine-grained PAT targets one resource owner.
- **Execution identity and destination:** where the token will be used (for example, local automation or a repository Actions secret). Do not ask the user to disclose the token value.
- **Workflow-file impact:** whether a Git push creates or changes `.github/workflows/` files.
- **Organization controls:** whether the organization requires approval, SSO authorization, has a token policy, or prohibits PATs.

If an operation, repository, resource owner, or error response is unknown, ask for it. Do not recommend broad access as a workaround. Never print, paste, commit, log, or include a PAT in a command line, URL, configuration file, or issue. If a token was exposed, treat it as compromised: revoke it, create a replacement, update its consumers, and review relevant logs and repository history.

## Least-privilege configuration

1. List each required operation separately and map it to the narrowest permission below.
2. Select the correct **resource owner** and choose **Only selected repositories**. Add only the repositories that require the integration. Never choose **All repositories**.
3. Start with no optional repository permissions; add only the mapped access level. Prefer read-only over write.
4. Create the token in GitHub: **Settings → Developer settings → Personal access tokens → Fine-grained tokens → Generate new token**.
5. Use a descriptive, single-purpose token name and a short practical expiration. One year is a maximum recommendation, not a default; calendar renewal before expiry.
6. If an organization owns the target repositories, complete any required organization approval and SSO authorization before testing.
7. Store the value only in the intended secret manager. For GitHub Actions, add it as the target repository's Actions secret: **Settings → Secrets and variables → Actions → New repository secret**. Give consumers the secret name, never the value.
8. Test the smallest real operation against one selected repository. Record the purpose, owner, selected repositories, permissions, expiry, approver if applicable, and rotation owner in an approved inventory without recording the token.

All fine-grained PATs receive **Metadata: Read-only** automatically; it is mandatory and cannot be removed.

## Repository permission reference

The table covers common repository permissions. GitHub can add or rename permissions; when an operation is not represented here, consult the current GitHub documentation for that exact endpoint or action before granting access.

| Permission | Levels | Covers |
|---|---|---|
| Contents | Read-only / Read and write | Files, branches, releases, Git clone and push |
| Actions | Read-only / Read and write | Workflows, workflow dispatch, artifacts, and caches |
| Secrets | Read-only / Read and write | Encrypted repository Actions secrets |
| Workflows | Write-only | Workflow files |
| Pull requests | Read-only / Read and write | Pull requests, reviews, and comments |
| Issues | Read-only / Read and write | Issues, labels, and milestones |
| Environments | Read-only / Read and write | Deployment environments and environment configuration |
| Deployments | Read-only / Read and write | Deployment records |
| Commit statuses | Read-only / Read and write | Status checks |
| Administration | Read-only / Read and write | Repository settings and branch protection |
| Pages | Read-only / Read and write | GitHub Pages configuration |
| Webhooks | Read-only / Read and write | Event subscriptions |

Do not infer that a similarly named permission is sufficient. In particular, **Actions** controls workflow-related operations, while **Workflows: Write-only** is required when a push changes workflow files. Environment secrets and organization-level resources may require a different permission or organization authorization than repository Actions secrets.

## Operation-to-permission matrix

| Requested outcome | Repository access | Minimum permission(s) |
|---|---|---|
| Clone or read repository files | Target repository only | Contents: Read-only |
| Push commits, tags, or ordinary files | Target repository only | Contents: Read and write |
| Push a change to `.github/workflows/` | Target repository only | Contents: Read and write; Workflows: Write-only |
| Create, edit, or delete a release | Target repository only | Contents: Read and write |
| Read workflow runs, artifacts, or caches | Target repository only | Actions: Read-only |
| Trigger a workflow-dispatch event | Target repository only | Actions: Read and write |
| Create or update a repository Actions secret | Target repository only | Secrets: Read and write |
| Read pull requests or reviews | Target repository only | Pull requests: Read-only |
| Create or update pull requests, reviews, or comments | Target repository only | Pull requests: Read and write |
| Read issues, labels, or milestones | Target repository only | Issues: Read-only |
| Create or update issues, labels, or milestones | Target repository only | Issues: Read and write |
| Create or update deployment records | Target repository only | Deployments: Read and write |
| Create or update commit statuses | Target repository only | Commit statuses: Read and write |
| Change repository settings or branch protection | Target repository only | Administration: Read and write |
| Update a Homebrew tap | Tap repository only | Contents: Read and write |

If a request combines rows, grant the union of only those permissions. For example, a release automation that also edits a workflow file needs Contents: Read and write plus Workflows: Write-only; it does not need Administration, Pull requests, or Secrets merely because it runs in CI.

## Choose the right credential

Before issuing a PAT, check whether a narrower or less user-bound mechanism fits:

- Use the repository-provided workflow token for same-repository Actions work when its permissions and event restrictions are sufficient.
- Use OpenID Connect with the target cloud provider for short-lived cloud credentials rather than storing a cloud credential in GitHub.
- Use a GitHub App when automation needs an installation identity, multiple repositories, organization governance, or independent rotation from a human account.
- Use a fine-grained PAT only when it is the appropriate user-associated credential and the resource owner permits it.

Do not switch to a classic PAT, organization-wide access, or an all-repositories selection merely to bypass a permission error. Escalate to the repository or organization administrator when policy or ownership prevents the least-privilege design.

## Diagnose and recover

Use the failing operation, selected resource owner/repository, HTTP status or Git error, and token inventory to isolate the cause. Do not request the token value.

| Symptom | Likely cause | Safe response |
|---|---|---|
| `401 Unauthorized` or authentication failure | Expired, revoked, malformed, or unavailable token | Confirm that the intended secret is available to the job, check expiry and revocation, then rotate if needed. Do not expose the token while testing. |
| `403 Forbidden` | Missing permission, token policy, SSO/organization approval requirement, or workflow/event restriction | Compare the exact operation with the matrix and current endpoint documentation; add only the missing permission after confirming policy and authorization. |
| `404 Not Found` for a known private repository | Repository was not selected, wrong resource owner, or GitHub is masking unauthorized access | Confirm the token's resource owner and selected repository before changing permissions. |
| Git push works except for workflow files | Workflows permission is absent | Add Workflows: Write-only in addition to Contents: Read and write. |
| Secret update fails | Wrong secret scope or missing secret permission | Distinguish repository, environment, and organization secret targets; grant Secrets: Read and write only for a repository Actions secret, then verify the scope-specific requirement. |
| Token is pending or cannot access an organization repository | Organization approval, SSO, or token policy blocks it | Have the organization administrator approve or authorize the token, or use an approved GitHub App or other credential. |
| Automation fails after a token change | Consumer references the wrong secret name, token expired, or selected repositories changed | Verify the secret name and runtime access rules, then compare the replacement token's owner, repository selection, permissions, and expiry with the documented contract. |

After any permission change, rerun only the failed operation. If it still fails, collect the operation, target repository, status/error, token resource owner, selected-repository list, and granted permission names/levels for escalation. Redact token values and authorization headers.

## Rotation and revocation

Create and validate a replacement with the same documented minimum contract before retiring a working token. Update each authorized consumer, verify the intended operation, then revoke the old token promptly. Revoke immediately rather than waiting for a rotation window if compromise, unexpected use, loss of repository ownership, or an unneeded integration is suspected.

## Decision rules

- Never grant organization-level access when repository-level access suffices.
- Never use **All repositories**; use **Only selected repositories**.
- Prefer read-only access when writing is not required.
- Use one token per integration; do not share a token across unrelated workflows.
- Treat GitHub's current permission and endpoint documentation as authoritative when it differs from this guide.
- Audit operation lists directly. Use a read-only `delegate` reviewer only when a consequential least-privilege decision needs a fresh judgment isolated from the parent's proposed permission mapping. Supply only sanitized operations, documented permission facts, and the decision rule. Keep final decisions and all credential handling in the parent.

<!-- skiller:projection-policy -->
Treat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.
