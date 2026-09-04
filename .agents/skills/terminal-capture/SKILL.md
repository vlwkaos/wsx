---
name: terminal-capture
description: "[presentation] Capture terminal text, screenshots, or recordings without exposing secrets or personal data. Use when saving, sharing, publishing, or attaching terminal output, including TUI screenshots and asciicasts. Keep local inspection separate from external disclosure."
license: MIT
metadata:
  author: vlwkaos
  version: "1.0.0"
---

# Terminal capture

Create the smallest terminal artifact needed for the stated purpose. Treat capture and disclosure as separate actions.

## Classify the request

Before capture, establish:

1. **Artifact:** text excerpt, screenshot, recording, or structured output.
2. **Boundary:** local scratch, durable private file, named recipient, or public destination.
3. **Scope:** exact command, screen region, time window, and required context.

A request to capture locally does not authorize sharing or publication. If the destination or audience is unclear, keep the artifact local and ask before disclosure.

## Human-required decisions

Require a direct human answer before capturing or disclosing any of these:

- credentials, tokens, recovery codes, private keys, or authentication prompts;
- personal identity, private contact details, private messages, or user records;
- private repository names, internal hosts, customer data, or regulated data;
- an artifact destined for a public post, issue, pull request, release, or third party when that destination was not explicit in the user's request.

Recommendations, defaults, inferred preferences, prior auto-policy choices, and tool-selected answers are not consent. Do not turn them into authorization. A human approval for one artifact and destination does not transfer to another.

## Capture workflow

1. **Minimize before capture.** Prefer a bounded command or cropped region. Disable verbose tracing, command echo, environment dumps, shell history, and unrelated panes when practical.
2. **Pause at secret entry.** Never record password, passphrase, MFA, token, keychain, or credential-manager input. Stop recording before the prompt and resume only after the sensitive interaction is complete and the screen is clean.
3. **Capture to owned local scratch.** Use a repository-local `.work/captures/<run>/` directory unless the user names a durable private path. Record which files this run owns.
4. **Inspect privately.** Review visible text, scrollback, frames, cursor-adjacent content, window title, prompt, working path, host and user names, timestamps, notifications, QR codes, and embedded metadata. Do not quote suspected secrets into chat or logs.
5. **Sanitize a copy.** Preserve the private original only while needed. Redact with opaque replacement, crop irrelevant regions, and normalize identifying metadata. Do not blur secrets when pixels or text may remain recoverable.
6. **Verify the final artifact.** Re-open the exact sanitized output. For recordings, inspect the full timeline or extract representative frames plus every interval containing typed input or screen transitions. Search text-based formats for secret-like and personal-data patterns.
7. **Disclose only to the approved boundary.** State whether the artifact is sanitized and name the destination. Ask for direct human approval if the boundary is consequential and was not explicit.
8. **Clean up symmetrically.** Remove current-run originals and scratch after verification or delivery unless the user explicitly asks to retain them. Report every retained path and reason. Never remove another run's files.

## Failure handling

- If content cannot be inspected reliably, do not share it. Report the limitation and offer a narrower recapture.
- If a secret appears, stop disclosure, remove only current-run copies when safe, and tell the user what category and artifact were affected without repeating the value. Recommend rotation when exposure may have crossed the approved boundary.
- If redaction changes the evidence needed for the task, ask whether to produce a narrower private artifact instead.

## Tool-specific capture

Use the owning terminal tool for mechanics. For `ht` sessions, follow `headless-terminal` for session ownership, waits, capture commands, and cleanup. This skill owns privacy, consent, retention, and disclosure decisions.

See `references/checklist.md` before external disclosure or recording an interactive workflow.

<!-- skiller:projection-policy -->
Treat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.
