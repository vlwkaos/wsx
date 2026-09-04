---
name: headless-terminal
description: "[agent] Drive interactive terminal programs headlessly with the `ht` CLI. Use for deterministic TUI interaction, rendered-screen inspection, or real keystrokes when plain pipes cannot represent the terminal. For saved or shared terminal artifacts, also follow terminal-capture."
license: MIT
compatibility: Requires a preinstalled ht CLI (v0.3.2+) on macOS (Apple Silicon) or Linux.
metadata:
  skiller.requires: "terminal-capture"
  author: Montana Flynn
  version: "1.0-wsx.1"
  homepage: https://github.com/montanaflynn/headless-terminal
  upstream-ref: v0.3.2
  upstream-commit: 92993b2cc09c3081af384ce5c683b2f448349d37
---

# ht — headless terminal

`ht` is a daemon that owns a pseudo-terminal per session and parses output with the same VT engine Ghostty uses. You can launch a TUI, send keystrokes, snapshot the rendered screen, and block until a screen condition is met.

## Prerequisite

Require `command -v ht` and `ht --version` before use. If either check fails or the version is older than 0.3.2, stop and report it. Do not install or upgrade `ht`; dependency changes require a separate security review and explicit user approval.

## When to reach for this

- Target program draws to the alternate screen / uses (n)curses / reads `$TERM`.
- Needs real keystrokes (arrow keys, `<C-c>`, `<F5>`), not just stdin text.
- You need to inspect the *rendered* screen state, not the raw output byte stream.

If the program works with `echo ... | cmd`, use a plain pipe — don't reach for ht.

## Core workflow

Use a unique current-run session name and record ownership before launch:

```
ht run --name S <cmd...>           # launch, prints session ID
ht send S "<keys>"                 # type into it
ht wait S --text "READY"           # block until a condition
ht view S                          # inspect the rendered screen
ht view S --format png --output FILE
ht stop S && ht remove S           # cleanup
```

Use a repository-local `.work/ht/<run>/` directory for generated scratch and captures unless the user names a durable output path. Never attach to, stop, kill, or remove an `ht` session that this run did not create.

## The one thing agents get wrong

Keystrokes reach the PTY before the program has finished rendering a response. **Do not `view` immediately after `send`** — you'll snapshot a stale screen.

Fold the wait into the send:

```
ht send S --wait-text "Saved" "ihello<Esc>:wq<CR>"
ht send S --wait-idle 200ms --view "q"
```

Picking the right wait flag is the hard part — see `references/waits.md` before writing sends for a new TUI.

## Quick pointers

- **Key notation** (vim-style `<C-c>`, `<CR>`, `<F1>`, …): `references/keys.md`
- **Wait strategies** (idle vs text vs cursor vs change vs duration): `references/waits.md`
- **Recipes** (edit-a-file, REPL, installer, watch-live, extract text): `references/recipes.md`
- **Troubleshooting** (exit codes, timeouts, stuck sessions): `references/troubleshooting.md`

## Output formats

Follow `terminal-capture` for privacy, consent, retention, and disclosure whenever output is saved, attached, sent, or published. This skill owns only `ht` capture mechanics.

- `ht view --format plain` (default) — text grid with a trailing `cursor: R,C`
  line. Use the cursor to disambiguate same-glyph entities (e.g. two `@`s in
  nethack: the cursor sits on you).
- `ht view --format ansi` — preserves colors/styles; good for showing the user.
  Also appends the `cursor:` line.
- `ht view --format html` — embeddable; no cursor line (would break the doc).
- `ht view --format png --output FILE` — rasterized screenshot with window
  chrome. Use for demos, bug reports, or to let yourself *see* a session
  (Claude can read PNGs, can't render SVG).
- `ht view --json` — structured (cursor position, size, text).

## Cleanup

Sessions persist across the daemon's lifetime. Always stop and remove every current-run-owned session, including after timeout, failure, or cancellation, then remove owned scratch that is no longer needed. Use `ht list` to inspect state, but never clean another run's session. Do not use `ht kill` or `ht remove --force` without explicit approval unless the current run created the session and graceful cleanup has failed.

<!-- skiller:projection-policy -->
Treat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.
