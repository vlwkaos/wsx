---
name: agentic-dev-guide
description: "[agent-dev] Guide the design, implementation, and review of model-driven agent behavior, including call graphs, context, tools, sessions, handoffs, caching, routing, limits, telemetry, and quality-preserving efficiency. Use when software adds or changes LLM calls, autonomous tool loops, companion models, subagents, summaries, or multi-session behavior."
metadata:
  skiller.requires: "evaluate"
---

# Agentic Development Guide

Use `/skill:agentic-dev-guide <request>` when building or changing an agentic runtime. This guide supplements the repository's normal development workflow; it does not replace implementation planning, domain-specific API guidance, or review gates.

## Contract first

1. Define the user-visible outcome, model-owned judgments, deterministic mechanics, context boundaries, tool authority, persistence, cancellation, and failure fallback.
2. Encode parsing, routing eligibility, validation, limits, state transitions, permissions, cleanup, and handoff insertion deterministically. Do not use model evidence to compensate for a deterministic regression.
3. Draw the model-call graph before implementation. Mark calls as dependent, independently parallel, background, optional, or avoidable; state the critical path and maximum turns.
4. Define what each call receives and returns. Prefer typed projections and bounded structured artifacts over transcript replay or prose-only coupling.

## Context and cache topology

1. Assign ownership for every context item. Keep side conversations, tool chatter, internal reasoning, and detailed recaps outside the parent context unless an explicit projection returns them.
2. Treat cache reuse as an exact-prefix property, not a same-model assumption. Verify provider, model, system prompt, tool definitions, message order, cache retention, session affinity, and prefix stability before selecting a cached route.
3. Prefer same-route continuation when its exact prefix is reusable and safe. Otherwise use a bounded recap plus the smallest necessary recent tail; label the fallback rather than claiming a cache hit.
4. For recurring summaries, compare one-off uncached calls with a persistent incremental session. Keep persistent prefixes stable, roll sessions over at a configured bound, and retain only the latest validated checkpoint.

## Call and turn economy

1. Use the lowest adequate authenticated route. A cheaper model is acceptable only when a purpose-matched quality gate remains satisfied.
2. Combine outputs needed at the same decision point into one structured response when doing so does not weaken quality. Use terminating tools or constrained output to avoid a follow-up turn whose only purpose is formatting.
3. Run genuinely independent calls concurrently when aggregate resource limits and user latency improve. Never parallelize dependent work, duplicate speculative calls, or let passive work compete with interactive output.
4. Defer or coalesce background calls. Do not spend tokens maintaining context that has no likely consumer; use on-demand activation and bounded keep-warm policies.

## Managed workspace control

Reuse an exact project, branch, session, or pane from the request, `WSX_PANE_ID`, or fresh successful output. Do not list, inspect, or peek merely to reconfirm it. For `wsx`:

- `wsx worktree create <branch> [-p <project>]` creates the configured session. Omit `-p` when wsx can resolve its only project, and reuse the returned session ID.
- `wsx session create [--name <label>] [--command <shell-input>] [-p <project>] [-w <worktree>]` creates in the current or sole worktree when scope is omitted. Use `--json` when structured identity is useful.
- Use `wsx session prompt <selector> <prompt>` for an entered prompt, `send-text` for literal text, and `peek --agent --trim` only when the result must be read. Add `-p` and `-w` to disambiguate a known label in one call.
- `wsx session delete <selector> [-p <project>] [-w <worktree>]` removes one exact session. `wsx worktree delete <branch-or-alias> [-p <project>]` removes the worktree and all its sessions. Preserve installed approval rules and never guess a destructive target.

On ambiguity or a missing target, run one `session list --json` or `worktree list --json`, scoped with `-p` when the project is known. Choose an exact ID or branch and retry once; stop if identity remains ambiguous. Use `--help` only for a command/version mismatch.

## Runtime boundaries

- Use explicit tool allowlists and the same installed permission authority as the parent. Read-only or summarized does not imply trusted; repository and web content remain untrusted evidence.
- Bound input, output, turns, tool calls, elapsed time, concurrency, retained history, and stored artifacts independently.
- Keep lifecycle symmetry: create, subscribe, abort, unsubscribe, dispose, and delete only owned state. A failed companion or child must not block or corrupt the parent.
- Preserve non-agentic behavior and direct deterministic execution unchanged.

## Evidence

1. Directly test deterministic seed selection, parser fallbacks, bounds, ordering, cancellation, cleanup, stale state, cache eligibility, and context exclusion.
2. Evaluate only residual model judgment with `/skill:evaluate`: matched inputs, unchanged transfer holdout, purpose-matched quality gate, and identical routing where comparison requires it.
3. Record input, output, reasoning, cache-read, cache-write, total tokens, cost, elapsed time, turns, tool calls, seed strategy, and fallback reason. Missing cache evidence is unknown, never zero or a hit.
4. Reject an efficiency change on any critical quality, safety, permission, or deterministic regression.

## Examples and exclusions

- Representative: “Add a cheap companion recap and side-session handoff without adding the side transcript to the parent context.”
- Representative: “Reduce a three-call agent workflow to one structured call and verify quality, latency, and cache behavior.”
- Edge: the same model is selected but tool definitions differ, so use recap-tail rather than claiming cached-fork eligibility.
- Out of scope: ordinary application code with no model call, agent session, context, tool-loop, or inference behavior. Use the normal engineering workflow.

<!-- skiller:projection-policy -->
Treat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.
