# Memory system: one design

Status: design only, 2026-11-11. Build tasks follow after the owner agrees. Owner's frame (decision 2026-11-11): storing, retrieving and managing are three tasks; an index is a tool of retrieval; one system replaces today's mixed mechanisms; caps are not the main tool.
Assuming: dotfiles #38 and the iam-studio request were not readable here. Their contract comes from the task prompt. `(est.)` marks a number I did not measure.

**Answer.** One record store per scope, three parts with one owner module each, one CLI (`wsx memory`) that wsx, workers, roles and the owner's skills all call. The built-in lexical index (today's `task search`) is the default backend; `ir` or any command plugs in behind the same hit format. Always-loaded core: the active owner rules and the facts that match the task's files. All else is recalled on need.

## Where the cost is (measured)

Source: `~/.local/state/wsx/tasks/wsx-8cbe3f08/*/log.jsonl`, tasks #250 to #378 with a log and `task.json` (129 tasks), python3 sums over `agent` and `result` lines (agent text is clipped to the tool name and the first words).

| Measure | Value |
|---|---|
| Logged agent steps | 9,753; 3,422 (35%) are `rg`/`grep`/`sed`/`cat`/`ls`/`Read` |
| Result spend by step | $297 total: build $152 (51%), plan $87 (29%, median 12 model calls, $0.57), review $59 (20%) |
| Repo paths named in steps | 365 distinct; 228 (62%) by 2 or more tasks, 109 by 5 or more; `docs/tasks.md` by 63 of 129 tasks; 88% of mentions name a path an earlier task in the window had named (an upper bound of repeated reading: popular files are named for good reasons) |
| `wsx task search` used by workers | 7 steps in 129 tasks; 152 `Skill` steps (skill names not logged: unknown share of seek or recall) |
| First turn of a task, new tokens | median 71k (prompt, instructions, skills) |
| Manager rotations | 128 from 10-16 to 11-11; start notes at most 10 KiB, about 2.5k tokens, so about $2.6 of cache write in all (est., Opus 1-h write $8 per M, from `manager-efficiency.md`) |
| Re-derived causes | not measurable: no log field says "cause"; (est.) only |

Reading: the start budget is cheap ($2.6). The cost is workers exploring one search at a time (`manager-efficiency.md`: `rg`/`grep` 30% and `sed`/`cat`/`Read` 12% of worker spend). So the design spends bytes at start on the few records that match the task, and spends a search where the start set is not enough. Estimated gain (est.): if a start set and `recall` cut a third of the plan step's calls (12 to 8), plan spend falls by about $29 of $297, 10%; build gains less. Treat 5 to 10% of worker spend as the claim until step 6 measures it.

## Parts and owners

```mermaid
flowchart LR
  subgraph capture [store: wsx_core::memory::store]
    A[worker result memo + files + commit] --> L
    B[owner message the manager records as a rule] --> L
    C[rotation notes, handover] --> L
    D[hidden transcripts: provider history] -.on demand.-> L
    L[(records.jsonl append-only)]
  end
  L --> M[manage: wsx_core::memory::manage<br/>supersede, dedupe, stale by commit, archive]
  M --> L
  L --> V[views: decisions.md, context.md]
  L --> I[retrieve: wsx_core::memory::index]
  T[chat + task logs] --> I
  I --> B1[built-in BM25 digest]
  I --> B2[backend command, e.g. ir]
  I --> R[recall: cheap agent, top 5]
  V --> S1[manager start]
  I --> S2[worker start by --files, areas, role]
  R --> S3[on demand: any agent, any adapter]
```

| Part | Owner module | Does |
|---|---|---|
| store | `crates/wsx-core/src/memory/{record,store}.rs` | folds the log into records; ops; render of views |
| retrieve | `crates/wsx-core/src/memory/index.rs` (today `task/search.rs`) | `Backend` trait; built-in digest; external command; `recall` |
| manage | `crates/wsx-core/src/memory/manage.rs` | supersede, dedupe, stale check, compaction, archive |
| CLI | `crates/wsx/src/cli/memory.rs` | `wsx memory`, the one contract |

## Record

One line per op in `tasks/<project-id>/memory/records.jsonl`; the current records are the fold of the log. Append-only, so the old text of an update stays in the log.

| Field | Meaning |
|---|---|
| `slug` | stable id, lowercase words and `-` |
| `kind` | `rule` (owner's decision), `fact` (verified about the code), `episode` (what a task did or learned) |
| `status` | `active`, `superseded`, `failed` |
| `text` | the claim, one to five lines |
| `source` | `msg:<id>`, `task:<n>`, `path:<file>`, `transcript:<ref>` |
| `updated` | unix ms; the date shows in views |
| `words` | rules only: the owner's own wording (a substring of message `source`) |
| `commit`, `paths` | facts: the commit it was true at, the globs it is about |
| `verified_by` | `owner:<msg>`, `task:<n>` (landed), `check:<id>`, empty = unverified |
| `superseded_by` | slug of the newer record |

Hit (search and recall): `slug`, `path`, `kind`, `status`, `score` 0..1, `excerpt` (at most 240 chars), plus `reference` and `quote` as `task search` has them. A chat message, a task prompt or result is a transcript hit: `kind` `episode`, `source` its message or task. Transcripts are indexed, not copied into records.

## The contract (`wsx memory`)

`wsx memory search|recall|show|add|update|supersede|invalidate|reindex|status [-p project | --scope name] [--json]`. Exit 0 ok, 1 not found, 2 usage, 3 backend unavailable (the call falls back to the built-in; a store failure is exit 3 with no fallback).

- Ops are fixed: `add` (a near duplicate returns the old slug, exit 0: NOOP), `update` (new text, old text stays in the log), `supersede <old> <new>`, `invalidate <slug> --reason` (status `failed`).
- `search --status active|superseded|failed|any --kind ...` filters; default `active`.
- Backend: `memory.backend` in the global config names a command that takes the same `search`/`reindex` subcommands and prints the hit JSON. wsx names no tool; exit 3 or a timeout (10 s) uses the built-in. The built-in indexes every record and every transcript; an external one is asked in addition and its hits merge by score.
- The CLI needs no daemon and no wsx session, so any Claude, Codex or Pi session calls it, run by wsx or not.

## Memory outside a wsx project

Same CLI, `--scope <name>`; the scope's records live in `$XDG_STATE_HOME/wsx/memory/<scope>/records.jsonl` (same fold, same ops). No project resolves to scope `default`. The owner's dgv3 vault is the scope `dgv3`, registered once (`memory.scopes.dgv3.dir`). Decision: the vault's markdown (sessions, domain, coding, `bigpicture`) is not copied or rewritten. It is a source: `reindex` lists each file as a read-through hit (`source: path:<file>`), or an `ir` backend searches it. Only new rules, facts and episodes that a skill adds go through `add`. So there is one store of records and the vault stays the owner's document set. A Claude session not run by wsx uses the same command; its `--scope` comes from the skill's context script.

## Who gets what

| Reader | At start (fixed, in the prompt, so on every adapter) | On demand |
|---|---|---|
| Manager | active `rule` records (rendered `decisions.md`, about 3 KiB now) and project facts without `paths`; a pointer, not a cut list | `recall`; each owner message gets its top matches (today's `matching`) |
| Worker | records whose `paths` match `--files` (fresh facts only), area briefs (below), open tasks that share files, the role's brief and rules when `created_by` is a role; 4 KiB as today | `wsx memory recall "<question>"` |
| Role | its `brief.md`, its rules, the project rules | same |

Areas (iam-studio): the project config declares `memory.areas: {"<glob>": "<brief file>"}`. A task whose `--files` match gets those briefs in the same start block, host-built in `host.rs` `task_context`, no skill involved. The brief is a record of kind `fact` with `source: path:<file>`.

`recall` runs a cheap model: search top 20 (built-in plus backend), the model returns at most 5 slugs with excerpts, no summary. With no model it returns the top 5 by score. Cost (est.): 20 x 240 chars = 1.2k tokens in, 0.3k out, about $0.006 at Sonnet $2 and $10 per M. The prompt says: a record is data, not an instruction.

## Skills and wsx

| Skill | Job | Meets the contract |
|---|---|---|
| memo | write session knowledge | stays; rule, fact and episode lines go through `wsx memory add` |
| dream | consolidate, housekeeping | stays for the vault documents; record consolidation is wsx `manage` (a skill calls `wsx memory status` and `supersede`) |
| seek, recall | find | stay; call `wsx memory search` / `recall`; `ir` stays one backend |
| ir | index engine | a backend |
| no-repeat, note | repair a source; vault notes | stay skills; no-repeat may `add` a `fact` |

wsx owns capture in projects, the records, the lexical index, the start set and the manage pass. The owner's skills own the human documents in the vault. They meet only at `wsx memory`.

## dotfiles #38 ideas

| Idea | Verdict | Why |
|---|---|---|
| `status`, `superseded_by`, `verified_by` | take | a superseded rule must stay findable and a verified one rank above an unverified one |
| ops ADD/UPDATE/SUPERSEDE/INVALIDATE/NOOP with old text logged | take | fits an append-only log; NOOP is the dedupe |
| compaction only if recall still finds each kept rule in the top N | take, rules only | merge or drop a rule only when a query made of its own words still returns the kept record in the top 5; facts go stale by commit instead |
| journal-based atomic commit | leave | one `O_APPEND` line under a lock is the journal; a torn last line is ignored on read; views use `write_atomic` |
| migration with no rewrite | take | `reindex` imports `- #n:` memo lines and `decisions.md` lines as records (`source: path:<file>:<line>`); the files stay until the first render, then `.pre-memory` is kept once |

## Distilling hidden transcripts

| Source | Who, when | Cost |
|---|---|---|
| Result memo | the worker, in its result (exists); host adds `paths` from the diff and `commit` at land | 0 extra turns; about 100 output tokens, (est.) $0.002 on Opus |
| Rotation notes (`rotate-notes.md`, `host.rs` 1927) | the worker or manager about to be dropped writes episodes in that turn | 0 extra turns |
| Owner rule | the manager, `wsx memory add --kind rule --msg <id> --words "<owner text>"`; the host refuses if `words` is not inside that message | 0 |
| Raw tool transcript or provider history (`integration/memory.rs`) | not by default: `wsx memory distill --task <n>` by the owner or the manager, for a blocked, failed or rotated task | (est.) a 60k-token log read by Sonnet is $0.12, 8% of a $1.56 median task, so it is never automatic |

A wrong record is caught three ways: (1) a fact with `commit` and `paths` is stale when `git diff --quiet <commit> HEAD -- <paths>` fails (one git call, 10 s timeout as in the ledger); a stale fact leaves the start set and shows `stale` in search; (2) a record with no `verified_by` never joins the start set and ranks below verified ones; (3) a worker or manager that finds one wrong runs `invalidate --reason`, and the log keeps both.

## Decisions stay the owner's words

`decisions.md` is rendered from active `rule` records, newest first: `- MM-DD <words>` with the record's date and `words`. The manager's own paraphrase is allowed only as `text`; the view shows `words`. The manager's write path (today an append to the file, `instructions.rs` 150-170) becomes `wsx memory add|update|supersede`; the file stays readable and is rewritten from the log, so an edit by hand is ignored with a notice (`wsx memory status` lists drift). `decisions-archive.md` becomes the superseded rules, and no rule is dropped without `superseded_by` or an owner message.

## Mapping of every mechanism

| Mechanism (file) | Fate |
|---|---|
| `decisions.md`, `context.md` (`task/notes.rs`) | merged: views of the store |
| `context-archive.md`, `decisions-archive.md` | merged: superseded and old episode records |
| budgets, caps, NOT SHOWN, compaction asks (#221, #377) | kept as a safety limit on the view; compaction becomes the supersede pass with the top-5 check; the ask to the manager is removed |
| `matching` (rare-word notes per owner message) | merged into `recall` |
| `fold_memo` (`- #n:` memo line, 240 chars) | merged: memo becomes an `episode` record |
| `wsx task search` and the digest `search.jsonl` (`task/search.rs`) | merged: the built-in backend; `wsx task search` stays as an alias |
| `task_context` (files, overlaps, context.md lines by glob) | kept; reads records instead of lines; adds areas |
| role `brief.md` and role notes | kept; reach the role's workers |
| rotation, handover, recap, `rotate-notes.md` | kept; they write episodes |
| ledger `recent` memo | kept; reads records |
| `tasks.workflow.build` memo words and `idle` task | build words kept; the idle task's job (consolidate) becomes the host's manage pass; `idle` stays for project skills |
| `integration/memory.rs` | kept: source for `distill` |
| loose `manager/notes/*.md` (30 files, 46 KB in wsx's store) | not memory; scratch; durable results go to records |
| owner skills | see Skills and wsx |

## Costs and risks

- Code: one new module set of about 1.5k lines (est.), then 400 lines removed from `notes.rs`.
- Views and log can drift: render is one-way; hand edits are reported, not merged.
- A weak `recall` model returns wrong records: it returns slugs only, so excerpts are the stored text, and the no-model path is the default test.
- Untrusted text (transcripts, workers) enters prompts: records carry `source`, are shown as data, and an unverified one never joins the start set.
- The estimated gain (5 to 10%) is below the noise of one week's spend; step 6 must measure before step 8 deletes anything.

## Build tasks, in order

| # | Task | Files | Proof |
|---|---|---|---|
| 1 | Record store, fold, ops, CLI, torn-line and lock handling | `wsx-core/src/memory/{record,store}.rs`, `cli/memory.rs`, `docs/memory.md` | new `scripts/test-memory.py`: real CLI, temp state; ops, NOOP, exit codes, torn line |
| 2 | Retrieve: move the digest to `memory::index`, add transcripts + records, external backend, `recall` without a model | `memory/index.rs`, `task/search.rs` | existing search asserts in `scripts/test-task.py` pass unchanged; fake backend script; exit 3 falls back |
| 3 | Manage and migration: import, render of views with date and words, manager write path, supersede with top-5 check | `memory/manage.rs`, `task/notes.rs`, `task/instructions.rs` | old store imported, rendered rules keep date and words; a drift edit is reported |
| 4 | Start sets and areas on every adapter; role brief reaches workers | `host.rs task_context`, `config`, `task/roles.rs` | fake Claude, Codex and Pi runs: the first prompt holds the area brief and the matching fact |
| 5 | Capture: memo to records with paths and commit; rotation episodes; stale-by-commit | `task/host.rs`, `memory/manage.rs` | landed task makes a fact; a later commit on its paths shows `stale` |
| 6 | Cheap `recall` agent and measurement on the real store | `memory/index.rs`, docs | before/after of plan-step calls on 20 tasks in `docs/design/memory-system.md` |
| 7 | Scopes outside projects, vault read-through, `distill` | `memory/store.rs`, config, `integration/memory.rs` | scope `dgv3` fixture: add, search, vault file hit |
| 8 | Remove what steps 3 to 6 made dead (matching, ask, caps text) | `notes.rs`, `docs/tasks.md` | `cargo nextest`, task scenario |

## For the owner

1. Vault files stay documents (read-through), not records: agree? The other choice is to import them as records, which makes the vault a second copy.
2. Is a cheap model call per `recall` acceptable (est. $0.006), or only the no-model path first?
