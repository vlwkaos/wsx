# Web client browser tests

Playwright Test (`@playwright/test` 1.64.1, pinned; the version `drive` uses). Each test owns one
isolated wsx (about 1 s to set up), so a test and its retry start from an empty chat: its own HOME and state, `wsxd`, `wsx web serve` on a loopback port and the fake Claude
(`scripts/fake-claude-stream.py`), as `crates/wsx/tests/common/mod.rs` builds it for the Rust tests.
Its browser context is fresh and paired with that site. Nothing mocks wsx; the fake agent is the
only stand-in, at the model boundary.

```sh
cargo build -p wsx -p wsx-daemon          # the tests run target/debug/wsx and wsxd
cd crates/wsx-web/e2e && pnpm install     # once
pnpm exec playwright test                 # both widths, 3 workers
pnpm exec playwright test -g "sheet"      # one test by name
pnpm exec playwright test --repeat-each 20 --retries 0   # hunt a flake
pnpm exec playwright show-trace ../../../.work/e2e/results/<test>/trace.zip
```

- Projects: `phone` (390x844, touch) and `desktop` (1280x800), the widths the web docs name.
- Results, traces and the HTML report go to `.work/e2e/`. A failed test keeps its trace.
- `fixtures.mjs`: `wsx` (`cli`, `json`, `say`, `pairCode`) and `leadChat` (a paired page at
  demo's lead chat). The fake lead's words (`TABLE5`, `DIAGRAM`, `SLOW`, ...) are listed at the top of
  `scripts/fake-claude-stream.py`.
- Tests drive the page as a person does: replies from the fake lead, a finger on the phone, the wheel
  on the desktop. No DOM is injected and no app code is called from a test; a scroll made from a
  test script (`scrollIntoView`) says why at its line.
- New web checks go here (AGENTS.md "Test placement"); `scripts/web-*-check.mjs` only shrink.
