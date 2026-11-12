Read-only answer against wsx main (86aeaab8, docs/tasks.md "Landing", "Checks" Scenarios, "Push").

1. What wsx does today
| Step of the owner's flow | wsx today |
|---|---|
| Separate worktree/branch per piece of work | Yes: each task has `<repo>-t<n>` on `wsx/t<n>`; main is untouched until land |
| Iterate until it works | Yes, if it stays one task: `wsx task add <n>` sends it back; each result gets new checks and a revision. #7-#11 were five tasks, so each try landed. That is a manager habit, not a missing feature |
| Land only after review | Yes: a manager-created task never lands by itself; the manager reviews the diff, then `land` |
| Squash at land | No. Land rebases and fast-forwards the worker's commits as they are. No refusal or warning on many or fixup commits |
| Documented worker squash | No wsx rule. A project can add one: `tasks.workflow.build` in ir's wsx.config.yml is added to every worker's instructions (e.g. "before your final result, squash your commits into one per logical change; no fixup commits") |

2. Testing on GitHub CI before land
| Need | Supported way |
|---|---|
| CI on the task's own branch | A `ci:` scenario: `tasks.scenarios: {kaggle: "ci: <script>"}` in ir's config. The worker runs `wsx task check-run kaggle`; the host (outside the boundary, so with network and git credentials) runs the script with no check slot and a 125 min limit, and the result gates the task. wsx's own scripts/ci-scenario.py is the model: it pushes HEAD to a throwaway branch `wsx-scenario/<name>/<time>-<commit>`, waits for the run with `gh api`, prints failed logs and deletes the branch. ir needs its own script (dispatch `gh workflow run eval-kaggle.yml --ref <branch>` instead of a push trigger). Nothing lands or reaches main until it passes |
| Branch that edits .github/workflows | No wsx feature. wsx's own script works around it: it pushes HEAD's tree with origin/main's .github/workflows, so workflow edits themselves are untested until land. Options: keep workflows thin and logic in scripts (what ir #11 did; best), or the owner gives a fine-grained token with Workflows: write for ir only. A host-side ssh push would avoid token scopes, but host scenario commands get no ssh agent (unchecked whether a script could find it) |

3. Does a PR add anything?
| PR gives | Needed for solo ir? |
|---|---|
| CI on the branch before main | No: the `ci:` scenario does it before land |
| A visible review record and discussion for outside users | Only if outside users review or contribute |
| Branch protection / required checks on GitHub | Only if the owner wants GitHub to enforce it; wsx already gates land on checks |
For a solo maintainer, one clean commit per change landed after a passing `ci:` scenario is enough. A PR adds ceremony, not safety.

4. Missing vs habit
| Item | Kind | Smallest change |
|---|---|---|
| One task per piece of work, retries via `add` | Manager habit | Record as an ir rule |
| Test on CI before land | ir config + script | ir task: a `ci:` scenario script for the Kaggle eval |
| Workflow scope | Owner choice / habit | Logic in scripts (done); token scope only if workflow edits must be tested |
| Clean history | Rule now, code later | Now: `tasks.workflow.build` squash rule. Code only if the rule fails: `tasks.land.squash` (land squashes the branch to one commit, titled by the task, before the fast-forward) |
| PR | Not needed | none |
