# Security audit

Run before any package publish, remote push, or release creation. Change to the directory containing the parent `SKILL.md`, then run the bundled script; do not resolve it from the project repository:

```sh
./scripts/security-audit.sh <project-root>
```

The script only inspects Git-tracked paths/content and returns:

- `0`: no heuristic findings
- `2`: findings that require review
- another nonzero status: audit could not run; release is blocked

Review every match in context. Common false positives include examples, lockfiles, test fixtures, and variable references; a name alone is not approval. Also inspect package/archive contents (`cargo package`, `npm pack --dry-run`) because ignore rules can differ from Git tracking.

For a real secret or sensitive file: stop, revoke/rotate as needed, remove it from tracking (for example `git rm --cached <file>` when the local file should remain), and consider history cleanup. Re-run the audit after remediation. If the finding is demonstrably non-secret, show the evidence and obtain explicit user acceptance before proceeding; record the accepted path/pattern in the final report. Never print the secret itself beyond the minimum already exposed by a command.
