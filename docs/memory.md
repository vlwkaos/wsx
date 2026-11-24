# The `memory` contract

One command surface for stored knowledge: `wsx memory <command>` (wsx, projects and `default`) and any other program that follows this text (dotfiles `memory.py`, vault scopes). Each scope has exactly one store and one program writes it.

## Scope

`--scope <name>`: lowercase `a-z`, `0-9`, `-` and `_`, 1 to 64 characters, not starting with `-` (a vault folder such as `pet_ai_factory` is one scope). `wsx memory` defaults it to the project of the current folder (a registered project's task ID, such as `wsx-8cbe3f08`); other programs require it. `search` may repeat `--scope`. Every command takes `--json` (machine output; without it a short text form) and exits with the codes below. `list` takes no `--scope`.

## Record

| Field | Meaning |
|---|---|
| `slug` | id inside the scope: `a-z`, `0-9`, `-` and `.`, 1 to 64 characters, not starting with `-`; a `.` stands inside only (not first, not last, not twice in a row), as in `release-0.10.0`. A slug is ASCII; a store that names records by a non-ASCII title derives the slug itself |
| `kind` | `rule` (owner's decision), `fact` (verified about the code), `episode` (what a task did or learned). A store maps its own categories into these three and may keep its category in a field of its own |
| `status` | `active` (default), `failed` (with `lesson`). `superseded` is only the answer of `supersede`: the store keeps no superseded record |
| `text`, `title` | the claim, at most `limits.text_bytes` bytes of the store (see "Limits"); optional title, one line |
| `source` | required on every write: `msg:<id>`, `task:<n>`, `path:<file>`, `url:<URL>`, `transcript:<ref>` |
| `updated` | unix ms of the last write; a store that has no write time may give its file's time |
| `verified_by` | required for `rule` and `fact`, else exit 2. Forms: `owner:<msg id>` (digits, a wsx message), `owner:transcript:<ref>` (the owner's words outside wsx), or a relative path with no `..` that exists under the project root. `url:<URL>` (a page that says it: `http` or `https` only, a host name or IPv4 address, no user name, no spaces, at most 2048 bytes; it is checked as text and never fetched). wsx also takes `task:<n>` (a landed task of the project), `task:<project>#<n>` (a landed task of another registered project, which the manager of this one ran there) and `check:<id>` (a passed check item of the project). `episode` needs none |
| `commit` | facts: the commit it was true at |
| `superseded_by` | in the answer of `supersede`: slug of the newer record |
| `lesson` | why a `failed` record failed |
| `paths` | wsx extension: globs or paths (relative to the project, no `..`, not starting with `-` or `:`, at most 500 and 32 KiB together) that the record is about; a rule whose paths meet a task's `--files` is in that task's first prompt (see "Start set"), and a land deletes a fact by them (see "Deletion") |
| `description`, `keywords` | optional. One line (at most 512 bytes) that says what the record is about, and a list of at most 32 words or phrases (64 bytes each). Both are searched with the text and shown in `get` and in a hit. A store may require them; wsx does not |

A reader ignores fields it does not know, and a program keeps the fields of its own that it does not own. wsx's own fields, which another program ignores: `paths`, `words` (a rule's: the owner's words) and `decided` (a rule's: unix ms of the decision, which a view shows as its date). Root of a relative `verified_by` path: the project path for a project scope, else the current folder.

`owner:transcript:<ref>`: `<ref>` is the path of an existing transcript file (absolute, or relative to that root), optionally followed by `#L<line>` (a number from 1). The program checks that the file exists and is a file; it does not read it. A date or a name alone is no proof (`owner:2026-11-11` is exit 2). `owner:<digits>` stays the form for a wsx message.

## Limits

`status` declares what the store keeps in `limits`: `{"text_bytes": N}`, the most bytes a record's `text` may hold (wsx's built-in store: 4096). A write over it is exit 2 and says the limit. A reader or writer takes N from `status` and never assumes 4096; the vault store may declare another.

## Hit

`slug`, `scope`, `path` (the file the record lives in), `kind`, `status`, `score` 0..1 (higher is better), `excerpt` (at most 240 characters), and `description` and `keywords` when the record has them.

wsx also searches the transcripts of a project scope (a chat message or reply, a task's prompt, result and story, the manager's decision and memo lines; "Search" in [tasks.md](tasks.md)) and returns them as hits: `slug` is the line's `reference` (`<project>:<id>`, `<project>#<task>`, `<project>:notes/decisions.md`), `kind` one of `message`, `reply`, `task`, `result`, `history`, `decision`, `memo`, `status` `active`, `path` the log or note file it was read from, and the keys of a `wsx task search` hit (`project`, `chat`, `id`, `task`, `unix_ms`, `by`, `line`, `reference`, `quote`, `outcome`); both commands print the same `Hit`. A reader ignores the keys it does not know and treats a `kind` it does not know as opaque. `--kind` takes any of these kinds.

## Commands

| Command | Arguments | Output |
|---|---|---|
| `search` | `<query> [--limit 10] [--include-inactive] [--kind k]` | `[hit]` by score, newer first on a tie; nothing found: `[]`, exit 0. Active records only unless `--include-inactive` (then also the `failed` ones) |
| `recall` (wsx) | `<question>` | the best 5 `[hit]`: the top 5 by score of a search's top 20 (no model yet) |
| `add` | `--kind K --slug S --text-file F\|- --source ID [--title T] [--verified-by V] [--commit C] [--description D] [--keywords a,b]` (wsx also `--paths a,b`, `--words W`, `--supersedes OLD`) | the record. A near duplicate returns the old record unchanged (NOOP), exit 0. wsx: a `rule` or `fact` also gets `similar`, see "Notes" |
| `update` | `--slug S [--text-file F\|-] --source ID [--verified-by V] [--commit C] [--description D] [--keywords a,b] [--status failed --lesson L]` | the record; `--keywords` replaces the list. The old text stays in the log. `--status failed` needs `--lesson` and is INVALIDATE |
| `supersede` | `<old> <new> --source ID` | the old record as it was deleted, `superseded` with `superseded_by` set to `<new>`. The store deletes `<old>` with every line of it at once |
| `get` | `<slug>` | the record; unknown slug: exit 1 |
| `capture` (wsx) | `--task N --memo TEXT [--commit SHA] [--paths a,b]` | the record, see "Capture" |
| `start` (wsx) | `--files a,b ` | `{"scope","start"}`: the start block a worker with those `--files` gets (the area briefs, then the rules), see "Start set" |
| `reindex` | none | `{"scope","ok"}`. wsx: drops the project's digest of transcripts and builds it again, then asks the backend to reindex (a backend that fails is exit 3) |
| `status` | none | `{"scope","store","backend","search":{"network":bool,"model":bool},"limits":{"text_bytes":N},"counts":{"active","superseded","failed"}}`; `superseded` is 0 (no store keeps one) |
| `list` | none | `[{"scope","store"}]` |

`add` rules:
- A near duplicate is an active record of the same kind with the same text after lowercasing and collapsing white space, or with a word set (split on white space) whose Jaccard index with the new text is 0.9 or more. The old record is returned and nothing is written.
- A slug that exists with different text is exit 2 (use `update`).
- `update` of an unknown slug and `supersede` of an unknown `<old>` or `<new>` is exit 1 (an `<old>` superseded before is unknown). `<new>` must be active, else exit 2.

`search` makes no network or model call unless `status` declares one. A hit needs at least one query word. wsx scores with BM25 (k1 1.2, b 0.75) over the lowercase words of a record's slug, title and text and of the project's transcripts, divided by the most a text could score for those words: more of the words, and rarer ones, score higher; the score is in 0..1 whatever else is stored. A run of Han, Kana or Hangul matches by pairs of neighbouring characters. The score of another program is its own, in 0..1.

## Search backend

The engine ranks a scope. `memory.engine` in the global config picks it: `builtin` (default), the index of `wsx_core::memory::index` (the digest both `wsx task search` and `wsx memory search` read), or `ir`:

```toml
[memory]
engine = "ir"   # needs the ir command (brew install vlwkaos/tap/ir, or cargo install ir-search), 0.19 or newer
```

- With `ir`, wsx writes the scope's records and its project's transcripts as markdown into the store folder's `corpus/` (`records/`, `lines/`; each file named by its content's digest, `kind` and `status` in its frontmatter) and runs `ir` with its own `IR_CONFIG_DIR` (`$XDG_STATE_HOME/wsx/ir`, collection `wsx-<scope>`), BM25 only: no daemon, no model. The owner's ir folder is read for its `ko` preprocessor alone, copied once into wsx's ir config.
- Each search first brings the corpus up to the truths: it writes missing files, deletes the ones no record or transcript names (a superseded or deleted record leaves no file), registers the collection when wsx's ir has none or its index file is gone, and runs `ir update` when anything changed. `reindex` drops the corpus and the collection and builds both again.
- It answers `wsx memory search|recall|status|reindex`, the records an owner message gets ([tasks.md](tasks.md) "Notes budget") and `wsx task search` (the transcript lines only, `-f path~lines/`, `--task` as `meta.task`; each project's collection ranks apart, so `--all` merges 0..1 scores). The `similar` check of `add` keeps the built-in until ir's scores are measured for it (docs/design/memory-layers.md "Search engine: ir").
- An ir that is missing, too old or failing leaves the answer to the built-in, with `wsx: memory engine ir unavailable (<why>); the built-in answered` on stderr; `status` names it as `backend_error`. A query with no words is exit 2 as for the built-in.

The global config can also add one program that follows this contract's `search`, `reindex` and `status` (wsx names no tool):

```toml
[memory]
backend = ["python3", "/path/to/ir-memory.py"]   # a program and its arguments; a string is the program alone
```

- One shape for both programs: a backend is called as `<configured argv> search --scope S --limit N [--include-inactive] [--kind K] --json -- <query>`, and as `<configured argv> reindex --scope S --json` and `<configured argv> status --scope S --json`. A backend may ignore `--limit`, `--include-inactive` and `--kind`: the caller filters its hits again (wsx does). Where its store is belongs to the configured argv (`["prog", "--store", DIR]`), never to a flag of this protocol.
- `wsx memory search` runs the built-in, then `<backend> search --scope S --limit N [--include-inactive] [--kind k] --json -- <query>`. The hits of both merge: one hit per `(scope, slug)` with the higher score, best first, at most `--limit`. A hit of the backend with another `kind` than `--kind`, or not `active` without `--include-inactive`, is dropped.
- The built-in always answers. The backend adds hits; when it exits 3 or any other code than 0, does not answer in 10 s, cannot start or prints no list of hits, wsx prints `wsx: memory backend <command> unavailable (<why>); the built-in answered` on stderr and returns the built-in hits (exit 0, clean JSON on stdout).
- `status` runs `<backend> status` for its `search` flags and reports `backend` (the command; `builtin` with none) and those flags. A backend that cannot be reached gives `backend_error` and `network` and `model` false: only the built-in answers.
- `recall` and the broker's `search` use the same merge.
- A backend runs in the process of `wsx memory` (for an agent, its host) with the owner's rights, not inside the agent's boundary. An agent supplies only the query, which goes after `--` in the arguments with no shell. Set a backend that declares `network` only if the owner wants a strict agent's query to reach it.

## Notes

The manager's notes are views of the project scope's records ([tasks.md](tasks.md) "Notes budget"). The records are the truth; `chats/<name>/notes/` holds two files that wsx writes from them after each write, so the manager prompt reads the same text as before.

| File | Holds |
|---|---|
| `decisions.md` | the active `rule` records, oldest first, one line each: `- <date> <words>`. `words` is the owner's wording, `decided` the date; with no `words` the text shows |
| `context.md` | the active `fact` records whose slug starts `section-` (the project section, in order), then the memos: records whose slug is `memo-<task>-<hash>`, as `- #<task>: <text>`. No size ceiling: the deletion rule keeps it short, and a fresh conversation names the lines it leaves out |

- **Old notes**: notes files from before the memory store are not read any more. A rule or fact write (`add`, `update`, `supersede`) is refused while a view file exists that no render has hashed (no entry in `views.json`), and no view is written; an episode is stored (no view is written either); run `wsx memory import` with wsx v0.43.0 once. The files stay as they are.
- **Write path**, records first: `wsx memory add|update|supersede` store the record and then write the two files, under the store's lock. A memo is stored by the manager host when a task leaves. A view that cannot be written is a notice on stderr; the record stays and the next write renders again.
- **Owner's words**: `add --kind rule --words W` needs `--source msg:<id>` and `--verified-by owner:<id>`, and W must stand in that message, which the owner wrote (white space ignored), else exit 2. Message ids are per chat: the message is read in the calling chat (`WSX_TASK_ACTOR=chat:<name>`), else in the repo's home chat. The record still goes to the project scope, and a chat records a rule only in the scope of a repo it is part of (its `chat.json` `repos`), else exit 2. The manager's own wording may be the `text`.
- **Top 5**: a new `rule` or `fact` first searches the active records of its kind (built-in index, records only, no model). The close ones (top 5, score 0.09 or more) come back as `similar` in the JSON, or as lines in the text form. They are never replaced by score alone: `add --supersedes <old>` stores the new record and supersedes `<old>` in one call, `supersede` does it later. An unknown `<old>` is exit 1 and stores nothing.
- **Threshold, measured** on a copy of the manager's real notes (28 active rules, 121 replaced, 28 facts): 10 rules restated in other words and 10 on new topics, each added to a fresh copy. The restated rule scored 0.05 to 0.17 (top hit for 8 of 10, a rank 2 and a miss for the rest); an unrelated rule's best hit scored up to 0.08. At 0.09: 5 of 10 restatements name their rule, none names another, 0 of 10 new rules get a hit. At 0.07: 6 of 10 and 2 of 10 false. At 0.05 every new rule gets 5 false hits. The restatements shared few words with the old rules; a rule restated with its own terms scores higher. A lexical check misses a restatement that shares no terms: the owner's words and `wsx memory search` stay the second look.
- **Areas**: a `## <area>` line of the old `decisions.md` is no rule. The views have no headers (the start text lists the rules newest first, so a header would label the wrong group); the area is kept as a keyword of its rules, shown in `get` and searched.
- **Drift**: wsx keeps the hash of each file it wrote (`views.json` in the store's folder). A file that differs is a hand edit. `wsx memory status` lists it under `drift` (`view`, `added` lines the records do not hold, `removed` lines they do), the manager host says so once in its progress, and the next write keeps the edited file as `<file>.drift`, prints a notice, and writes the file again from the records. Nothing is merged and nothing is lost.
- A search of the project no longer reads the lines of a file that is a view: the records are searched themselves.

## Start set

The first prompt of a worker holds a block from the memory, built by `wsx_core::memory::start` for every adapter (`host.rs` `task_context`; `wsx memory start` prints the same block). No other start loads memory: a Claude chat outside wsx gets nothing. In order, within 4 KiB:

1. **Area briefs**: `memory.areas` of the project's `wsx.config.yml` is `{"<glob>": "<brief file>"}`. Each brief whose glob meets one of the task's `--files` is read from the project (a relative path inside it, at most 8 KiB) and shown whole.
2. **Rules**: each active `rule` whose `paths` meet one of the task's `--files` (the same match as for areas), newest decision first, as `Rule <slug> (paths …), the owner's words: "…"` and the record text. Rules are the owner's decisions, few, and stay until the owner replaces them; a rule is shown whole or named, never cut. A rule with no `paths` joins no block: it would crowd out the briefs and the rules that fit the task (a store holds about 40, more than the budget), and the manager already reads them all in `decisions.md`. `--paths '**'` makes a rule project-wide: it joins every block, so give it only to a rule every task needs (`add` of a rule with no paths says this on stderr). A failed rule never joins (a superseded one is deleted).

No fact records: the paid evaluation (#392) found that records in the start block gave no gain and likely harm (plan cost +11% median, inside noise; recall -0.05; about 3 extra calls to check records, 92% of them marked stale), and baseline plans already name 0.81 of the landed files by reading. A worker finds records on demand with `wsx memory recall` and `search`.

Nothing is cut silently: a piece that does not fit is named on a last line (`Not shown, over the 4096-byte start budget: <names>.`, a brief by its file, a rule by its slug), and a brief that is cut says how many bytes and where the whole is.

## Capture

When a task leaves, the manager host (`manager.rs` `fold_memo`) stores its result memo, with no model turn:

| Task | Record |
|---|---|
| landed | `fact`, `verified_by` and `source` `task:<n>`, `commit` = the landed commit (the log's `landed`), `paths` = the files of its diff (the task record's `paths`, capped) |
| landed, run by another project's manager | the same fact in the scope of that manager's project: `verified_by` and `source` `task:<project>#<n>`, slug `memo-in-<project>-<n>-<digest>` (no number, so the notes views skip it). Its paths are the other project's, so the deletion rule of this scope leaves it. Its own scope keeps the usual record. |
| closed or failed | `episode`, no `commit`, no `paths` |

The slug is `memo-<n>-<digest>`, so the same memo is one record. A memo is one line of at most 240 characters. `wsx memory capture` does the same for a replay. A task created by a chat keeps its memo in the chat's own notes (not captured). When a worker's conversation ends for its size, its host also stores the handoff notes as an episode `rotation-<n>-<digest>` (cut at the text limit, with the path of the whole), with no turn of its own; it is deleted when the task ends (see "Deletion"). The handover a manager's fresh conversation opens with is not stored: its lines are the chat log's, which search reads.

## Deletion

wsx deletes what is past by rule; no manager decides. One function holds the rule (`wsx_core::memory::manage::sweep`); the repo's service (`wsx task repo-host`) runs it at its start and each time a task of the repo ends (lands or closes). A store with nothing past is read, not written.

| What | Deleted when |
|---|---|
| `supersede`'s `<old>` (also `add --supersedes`) | at once, by `supersede` |
| A task's handoff notes record (`rotation-<n>-…`) | the task has ended. `rotate-notes.md` in the task folder stays |
| A record an older release kept: a `superseded` rule, a manager handover (`handover-…`) | the next sweep |
| A fact of an earlier task of this project (`verified_by` `task:<n>`) with `paths` | a land: every path is gone from the tree of the landed commit (`git ls-tree`; git that cannot answer in 10 s deletes nothing), or the land's own memo fact holds every path |
| Files no code reads: `notes/decisions-archive.md`, `notes/context-archive.md`, `notes/*.pre-memory` and `threads-placed` of a chat of the repo; `batch-check-*.log`, `trial.lock` and `grants.json` of the repo's folder | the next sweep |
| A file in a chat's `notes/` (not a view) whose name holds a task number (2 to 4 digits, a `YYYY-MM-DD` date left out) of a repo of the chat, where every task it names has ended | the next sweep |

`Store::delete` is the one rewrite of the log: it drops every line of the deleted slugs, under the store's lock, by a temp file renamed over `records.jsonl`. The search digest holds no record, so nothing else changes.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok (including `[]` and NOOP) |
| 1 | not found (slug) |
| 2 | usage, an invalid value, or an invalid `verified_by` |
| 3 | store unavailable (lock, I/O), or a `reindex` the backend could not do. A backend that cannot `search` is no exit: the built-in answers |
| 4 | compaction: the recall-query gate refused, the store is untouched, the report names the query |
| 5 | compaction: the store changed meanwhile |

Codes 4 and 5 belong to compaction, which a later step adds. Errors go to stderr as `<program>: <message>`.

## Store behavior wsx has (the contract test checks the first two for any program)

- Two writes at once both land.
- The old text of an `update` stays readable (wsx: in the log, until the record is deleted).
- wsx's store is `tasks/<project-id>/memory/records.jsonl` (scope `default` and unbound names: `$XDG_STATE_HOME/wsx/memory/<name>/records.jsonl`): one JSON object per line, appended and folded into current records; a deletion ("Deletion") rewrites it without the lines of the deleted slugs. A last line cut short by a crash is skipped with a notice on stderr and never fails a command; the next write starts on a new line.

## Agents

A task agent calls `wsx memory` through its host's broker (`WSX_TASK_BROKER`), which runs the command for it. The one rule is `wsx_core::task::policy` ([tasks.md](tasks.md) "Permissions"):

| Command | Who |
|---|---|
| `search`, `recall`, `status`, `list`, `get` | every agent, also an untrusted one; a read may name any scope |
| `add`, `update`, `supersede`, `reindex` | the manager, a named chat and a worker, in the agent's own project scope only (`--scope` defaults to it, another one is refused) |
| a `rule` record (add, change, replace) | the manager and a named chat: it needs `owner:<id>`, and a worker's call is refused by the store |
| any write | not an untrusted agent (strict profile): read only |

The host, not the agent, writes the fact of a task that its manager ran in another project, in that manager's scope ("Capture"); an agent's own writes stay in its own scope. A read may name any scope, but an agent's search finds the transcripts (chats, task stories, notes) of its own project only; of another project's scope it finds the records. (The host sets `WSX_TASK_FROM` to the agent's project; the owner's own call sets none.)

A refusal exits 2 and names the cause. The host sets `WSX_MEMORY_RULES=deny` for an agent without `decide`; the owner's own call sets none.

## Proof

`scripts/test-task.py` sections 23m and 27m run the broker cases (a worker, an untrusted worker, the manager) and the `task:<n>` and `check:<id>` forms of `verified_by` with a landed task and a passed item.

`python3 -B scripts/test-memory.py "<command prefix>"` runs the same scenario against any program, for example `"target/debug/wsx memory"` or `"python3 memory.py"`. It runs every call in a temp home (`HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`) and drops `NOTEVAULT_ROOT` and every `WSX_*` variable of the caller, so a run against `memory.py` or `wsx` never touches the owner's vault or stores. It checks, for any program, `limits.text_bytes` (the limit exact, one byte over is exit 2), the `owner:transcript:` and `url:` forms (a bad URL is exit 2 and nothing is fetched), a scope with `_`, `get`, `description` and `keywords` (stored, shown, searched, replaced by `update`), and a slug with `.`. For wsx it also runs the notes of a project (old notes refused, the views byte for byte, `--words` from the home chat and from a topic chat, `similar` and `--supersedes` deleting the old record, no ceiling or archive, a hand edit as `drift`), the sweep of the repo's service on a real store (the records and files of "Deletion" go, the notes record of a working task stays, a second start writes nothing), the transcripts of a project scope and the backend cases against `scripts/fake-memory-backend.py` (a program that follows `search`, `reindex` and `status` and can exit 3, exit 1, print garbage or hang): merge by score, the kind filter, exit 3, the 10 s stop, `status` flags, `reindex` and `recall`.

`scripts/test-memory.py` also runs, for wsx, the start set on a git project (area brief, the rules that meet the files and not the others, no fact record in the block, no stale mark in `search`, the budget with its `Not shown` line, bad paths, `capture` of an unlanded task). `scripts/test-task.py` checks the first prompt of a Claude and a Pi worker (area brief, no fact record), the fact a landed task leaves (commit, paths), the rotation episode and its deletion once the task ended, and that a handover is no record. The task adapters are `claude` and `pi`; both take the start block from the same first turn.

CI: the `task` scenario's `core` part runs it (`.github/workflows/ci.yml`), so `wsx task check-run task` and every later step prove the contract.
