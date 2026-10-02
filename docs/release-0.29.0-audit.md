# 0.29.0 candidate audit

Preparation snapshot: audited with dependency warnings before commit/publication.
Final release review reuses those functional gates; a later knowledge-anchor comment
passed the owning daemon all-target check. Publication has separate confirmation gates.
Candidate: `.work/r29`, based on published `v0.28.2`. Audit date: 2026-10-03.
Main remains 0.30.0. This report does not approve a tag or publication.

## Scope

- Include non-web changes from `7bc5ac3`, `1230878`, and scoped working-tree files.
- Include reporter lifetime repair, native agent context/history, draft preservation,
  cache intent ownership, terminal-mode cleanup, context navigation and folded status.
- Exclude `abe5197`, web/bridge/structured conversations, and private skill-catalog changes.
- Four workspace packages and internal pins are 0.29.0; protocol stays 16, daemon revision is 17.
- Main's versions, web work, installed binaries and live user sessions are preserved.

## Verification

| Boundary | Result |
|---|---|
| Locked workspace all-target check | Pass |
| Candidate Nextest | 702 passed, 3 ignored probes exercised separately where applicable |
| Doctests and strict all-target Clippy | Pass |
| Stable reporter and installed Pi/OMP/OpenCode/Kilo/TUI adapters | Pass |
| Live handoff and stale-generation rejection | Pass; shell PID, PTY I/O and identity retained |
| Fragmented PTY/native-history/draft journey | Pass, 4.72 seconds, zero model calls |
| Runtime smoke and latency | Pass; added p95 below 7.9 ms against a 16.7 ms budget |
| Actual private PTY UI ownership and two-window state | Pass, 14.727 seconds, zero model calls |
| Release recovery workflow validator | Pass |
| wsx-core package verification | Pass, dry-run only, no upload |
| Retained main 0.30.0 regressions | 737 Nextest tests, check, Clippy and isolated Pi RPC/handoff passed |

Test-owned TUIs and daemon were reaped; the UI probe socket was removed.
The UI diagnostics scenario verifies cleanup, not the natural external mouse-loss trigger.

## Dependency disposition

The OSV query covered 137 exact root-lock registry versions and verified a vulnerable
`anyhow 1.0.100` control. The candidate now pins patched `anyhow 1.0.103`.
The scan is **not clean**: existing Ratatui 0.29.0 dependencies remain flagged.

| Advisory | Trigger and current consumer evidence | Disposition |
|---|---|---|
| RUSTSEC-2026-0002 (also GHSA-rhfx-m35p-ff5j) | `lru::IterMut`; Ratatui's private layout cache does not use mutable iteration | Retained warning; no triggering current path identified |
| RUSTSEC-2026-0253 | `LruCache::pop` with a panicking key destructor and continued use after unwinding; current cache uses `get_or_insert` and `resize`, not `pop`, with concrete `(Rect, Layout)` keys | Retained warning; no triggering current path identified |
| RUSTSEC-2024-0436 | `paste 1.0.15` is unmaintained; Ratatui uses it to generate styling methods at build time | Maintenance follow-up, not a reported runtime exploit |

Primary advisories: https://github.com/rustsec/advisory-db/tree/main/crates/lru
and https://github.com/rustsec/advisory-db/tree/main/crates/paste.
Evidence: Ratatui 0.29.0 `src/layout/layout.rs` (cache type, `init_cache`,
`split_with_spacers`), `src/style/stylize.rs`, and lru 0.12.5 `resize`/`pop_lru`.
Cache keys contain ordinary value types and `Vec<Constraint>`, not custom key destructors.
This is scoped reachability evidence, not proof that the affected library is safe generally.

Owner: wsx maintainers. Follow-up: migrate and pin the UI dependency stack separately;
reassess these findings if cache operations, key types or dependency versions change.
Publication approval must include these warnings. No UI-library upgrade was performed.

## Coverage and remaining release actions

- Dedicated cargo-audit, Gitleaks and TruffleHog were unavailable; current OSV and scoped
  credential-pattern checks were used. Ignore-rule warnings remain for `secrets/` and `.secrets`.
- Credential-like vendor workflow assignments are not treated as leaked token values.
- The direct audit covered ownership, documentation variants, breaking API coverage,
  public consumers, subprocess failure behavior and portable adapter invocation.
- No configured audit-axes file was provided; defaults and project ownership rules were used.
- Retain this candidate and `.work/probes/reporter-r29` receipts for the release handoff.
- Scoped knowledge consolidation added reporter ownership and release-history notes.
  Pre-existing staged/unstaged vault edits and all nine source sessions were preserved;
  broader Dream cleanup remains partial and is not claimed complete.
- Before publication: review and commit the isolated candidate, pass Ubuntu/macOS branch CI
  on that exact SHA, verify credentials, then obtain explicit annotated-tag confirmation.
- GitHub Actions alone publishes wsx-core, adjacent universal wsx/wsxd archives, native
  Homebrew bottles and the tap. Those artifacts and installed versions remain unverified here.
- Existing panes require the repaired daemon and updated, restarted agents; shells need not restart.
