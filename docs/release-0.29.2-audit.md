# 0.29.2 candidate verification

Status: local source gates passed; exact remote CI and Actions publication remain pending.
Scope: non-web UI patch from main `2939bd3`; original dirty 0.30.0 work and live sessions remain untouched.

| Contract | Evidence |
| --- | --- |
| Background-only chips | Brackets replaced with equal-width padding; current stays bright/bold, peers reuse existing quiet panel token |
| Semantic text | Provider identity, Unicode truncation, real user-supplied bracket names and overflow budgets preserved |
| Viewport/input | Actual 12-session/two-worktree journey passed at 120x24 and 56x18 in 8.14 seconds: full traversal/wrap, hidden targets, state updates, unchanged attention/bare input, sidebar and top/bottom dimensions |
| Actual color output | Captured current and peer RGB backgrounds in ANSI title row; no generated brackets; terminal content rectangle unchanged |
| Source gates | Locked workspace check/build, 706/706 Nextest tests (three skipped), doctests, strict Clippy, core package verification, harness/runtime smoke, workflow validator and touched formatting passed |
| Versions | Workspace and all internal pins/lock packages are 0.29.2; every external lock block unchanged |
| Runtime authority | No daemon revision, wire schema, generation, lease, agent lifecycle or navigation mutation |
| Cleanup | Private tmux, wsxd, PTYs and listeners stopped; captures/evidence retained for review |

Parent worktree evidence: `.work/probes/release-r292-20261005/`. Actual captures: `.work/r292/.work/tc-19689/captures/`.

The first collector failed only because two app-level assertions still required literal brackets. The actual render already matched the requested chip design. Corrected formatting checks retain identity, state, order, dimensions and untouched-content oracles; the failed log remains retained.

Simplify: two padding substitutions preserve width arithmetic and existing projections. One peer theme role reuses current context foreground and existing panel background. No new state, routing or dependency.

Backpressure: full tests and real producer-to-consumer TUI journey passed; native color output and cleanup observed. Candidate release includes prior trusted-formula and bounded legacy-fixture CI corrections since the immutable 0.29.1 tag. Changelog covers those changes. Existing vendor-expression scanner findings are unchanged; known lru/paste advisories and unavailable dedicated scanners remain disclosed. No local publication or installation.

## Remaining boundaries

Exact release-commit Linux/macOS CI must pass before the new annotated v0.29.2 tag. Actions owns archive, wsx-core and Homebrew publication; both native pours and independent asset/crate/tap verification remain mandatory. Existing tags are immutable.

This patch does not repair live Codex cached identity/admission, already-loaded integrations, or abrupt production daemon-loss resilience. It does not restart agents or force daemon handoff.

## Human Verify

Optional comfort check: pass if the current chip is easy to distinguish and quiet peer backgrounds are clear at narrow widths. Fail if either is ambiguous; report width/theme. Programmatic render and navigation checks passed; visual comfort is not claimed.
