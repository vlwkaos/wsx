# The `memory` contract

One command surface for stored knowledge: `wsx memory <command>` (wsx, projects and `default`) and any other program that follows this text (dotfiles `memory.py`, vault scopes). Each scope has exactly one store and one program writes it. Design and reasons: [design/memory-system.md](design/memory-system.md).

## Scope

`--scope <name>`: lowercase `a-z`, `0-9`, `-` and `_`, 1 to 64 characters, not starting with `-` (a vault folder such as `pet_ai_factory` is one scope). `wsx memory` defaults it to the project of the current folder (a registered project's task ID, such as `wsx-8cbe3f08`); other programs require it. `search` may repeat `--scope`. Every command takes `--json` (machine output; without it a short text form) and exits with the codes below. `list` takes no `--scope`.

## Record

| Field | Meaning |
|---|---|
| `slug` | id inside the scope: `a-z`, `0-9`, `-` and `.`, 1 to 64 characters, not starting with `-`; a `.` stands inside only (not first, not last, not twice in a row), as in `release-0.10.0`. A slug is ASCII; a store that names records by a non-ASCII title derives the slug itself |
| `kind` | `rule` (owner's decision), `fact` (verified about the code), `episode` (what a task did or learned). A store maps its own categories into these three and may keep its category in a field of its own |
| `status` | `active` (default), `superseded`, `failed` (with `lesson`) |
| `text`, `title` | the claim, at most `limits.text_bytes` bytes of the store (see "Limits"); optional title, one line |
| `source` | required on every write: `msg:<id>`, `task:<n>`, `path:<file>`, `url:<URL>`, `transcript:<ref>` |
| `updated` | unix ms of the last write; a store that has no write time may give its file's time |
| `verified_by` | required for `rule` and `fact`, else exit 2. Forms: `owner:<msg id>` (digits, a wsx message), `owner:transcript:<ref>` (the owner's words outside wsx), or a relative path with no `..` that exists under the project root. `url:<URL>` (a page that says it: `http` or `https` only, a host name or IPv4 address, no user name, no spaces, at most 2048 bytes; it is checked as text and never fetched). wsx also takes `task:<n>` (a landed task of the project) and `check:<id>` (a passed check item of the project). `episode` needs none |
| `commit` | facts: the commit it was true at |
| `superseded_by` | slug of the newer record |
| `lesson` | why a `failed` record failed |
| `paths` | wsx extension: globs or paths (relative to the project, no `..`, not starting with `-` or `:`, at most 500 and 32 KiB together) that the record is about. See "Start set" |
| `description`, `keywords` | optional. One line (at most 512 bytes) that says what the record is about, and a list of at most 32 words or phrases (64 bytes each). Both are searched with the text and shown in `get` and in a hit. A store may require them; wsx does not |

A reader ignores fields it does not know, and a program keeps the fields of its own that it does not own. wsx's own fields, which another program ignores: `paths`, `words` (a rule's: the owner's words) and `decided` (a rule's: unix ms of the decision, which a view shows as its date). Root of a relative `verified_by` path: the project path for a project scope, else the current folder.

`owner:transcript:<ref>`: `<ref>` is the path of an existing transcript file (absolute, or relative to that root), optionally followed by `#L<line>` (a number from 1). The program checks that the file exists and is a file; it does not read it. A date or a name alone is no proof (`owner:2026-11-11` is exit 2). `owner:<digits>` stays the form for a wsx message.

## Limits

`status` declares what the store keeps in `limits`: `{"text_bytes": N}`, the most bytes a record's `text` may hold (wsx's built-in store: 4096). A write over it is exit 2 and says the limit. A reader or writer takes N from `status` and never assumes 4096; the vault store may declare another.

## Hit

`slug`, `scope`, `path` (the file the record lives in), `kind`, `status`, `score` 0..1 (higher is better), `excerpt` (at most 240 characters), and `description` and `keywords` when the record has them.

wsx also searches the transcripts of a project scope (a chat message or reply, a task's prompt, result and story, the manager's decision and memo lines; "Search" in [tasks.md](tasks.md)) and returns them as hits: `slug` is the line's `reference` (`<project>:<id>`, `<project>#<task>`, `<project>:notes/decisions.md`), `kind` one of `message`, `reply`, `task`, `result`, `history`, `decision`, `memo`, `status` `active`, `path` the log or note file it was read from, and the keys of a `wsx task search` hit (`project`, `role`, `id`, `task`, `unix_ms`, `by`, `reference`, `quote`, `outcome`). A reader ignores the keys it does not know and treats a `kind` it does not know as opaque. `--kind` takes any of these kinds.

## Commands

| Command | Arguments | Output |
|---|---|---|
| `search` | `<query> [--limit 10] [--include-inactive] [--kind k]` | `[hit]` by score, newer first on a tie; nothing found: `[]`, exit 0. Active records only unless `--include-inactive` |
| `recall` (wsx) | `<question>` | the best 5 `[hit]`: the top 5 by score of a search's top 20 (no model yet) |
| `add` | `--kind K --slug S --text-file F\|- --source ID [--title T] [--verified-by V] [--commit C] [--description D] [--keywords a,b]` (wsx also `--paths a,b`, `--words W`, `--supersedes OLD`) | the record. A near duplicate returns the old record unchanged (NOOP), exit 0. wsx: a `rule` or `fact` also gets `similar`, see "Notes" |
| `update` | `--slug S [--text-file F\|-] --source ID [--verified-by V] [--commit C] [--description D] [--keywords a,b] [--status failed --lesson L]` | the record; `--keywords` replaces the list. The old text stays in the log. `--status failed` needs `--lesson` and is INVALIDATE |
| `supersede` | `<old> <new> --source ID` | the old record, now `superseded` with `superseded_by` set to `<new>` |
| `get` | `<slug>` | the record; unknown slug: exit 1 |
| `capture` (wsx) | `--task N --memo TEXT [--commit SHA] [--paths a,b]` | the record, see "Capture" |
| `start` (wsx) | `--files a,b [--role NAME] [--about WORDS]` | `{"scope","start"}`: the start block a worker with those `--files` (and the title and first prompt line as `--about`) gets, see "Start set" |
| `import` (wsx) | none | `{"scope","imported":{"rules","archived","facts","episodes"}}`; see "Notes" |
| `reindex` | none | `{"scope","ok"}`. wsx: drops the project's digest of transcripts and builds it again, then asks the backend to reindex (a backend that fails is exit 3) |
| `status` | none | `{"scope","store","backend","search":{"network":bool,"model":bool},"limits":{"text_bytes":N},"counts":{"active","superseded","failed"}}` |
| `list` | none | `[{"scope","store"}]` |

`add` rules:
- A near duplicate is an active record of the same kind with the same text after lowercasing and collapsing white space, or with a word set (split on white space) whose Jaccard index with the new text is 0.9 or more. The old record is returned and nothing is written.
- A slug that exists with different text is exit 2 (use `update`).
- `update` of an unknown slug and `supersede` of an unknown `<old>` or `<new>` is exit 1. `<new>` must be active, else exit 2.

`search` makes no network or model call unless `status` declares one. A hit needs at least one query word. wsx scores with BM25 (k1 1.2, b 0.75) over the lowercase words of a record's slug, title and text and of the project's transcripts, divided by the most a text could score for those words: more of the words, and rarer ones, score higher; the score is in 0..1 whatever else is stored. A run of Han, Kana or Hangul matches by pairs of neighbouring characters. The score of another program is its own, in 0..1.

## Search backend

The built-in index (`wsx_core::memory::index`, the digest of `wsx task search`) is the one backend wsx has. The global config can add one program that follows this contract's `search`, `reindex` and `status` (wsx names no tool):

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

The manager's notes are views of the project scope's records ([tasks.md](tasks.md) "Notes budget"). The records are the truth; `manager/notes/` holds four files that wsx writes from them after each write, so the manager prompt reads the same text as before.

| File | Holds |
|---|---|
| `decisions.md` | the active `rule` records, oldest first, one line each: `- <date> <words>`. `words` is the owner's wording, `decided` the date; with no `words` the text shows |
| `decisions-archive.md` | the `superseded` rules, same lines |
| `context.md` | the active `fact` records whose slug starts `section-` (the project section, in order), then the memos: records whose slug is `memo-<task>-<hash>`, as `- #<task>: <text>`, up to 32 KiB |
| `context-archive.md` | the older memos past that ceiling |

- **Import**, once: the first write of a writer that may give rules (or `wsx memory import`) copies each old file to `<file>.pre-memory` and reads only the copies, one record per line. The slug is a hash of the line, so a rerun adds nothing and an archive line that repeats an active rule stays one record. A dated rule line keeps its words and its date byte for byte; a line with no date is shown with none. A `- #<n>:` memo is a `fact` verified by `task:<n>` when that task landed, else an `episode`. Other `context.md` lines become `section-` facts. A line that is no bullet of `decisions.md` is not imported (the copy keeps it). A writer that may not give rules (a worker) imports nothing and renders nothing while old notes wait.
- **Write path**, records first: `wsx memory add|update|supersede` store the record and then write the four files, under the store's lock. A memo is stored by the manager host when a task leaves. A view that cannot be written is a notice on stderr; the record stays and the next write renders again.
- **Owner's words**: `add --kind rule --words W` needs `--source msg:<id>` and `--verified-by owner:<id>`, and W must stand in that message, which the owner wrote (white space ignored), else exit 2. The manager's own wording may be the `text`.
- **Top 5**: a new `rule` or `fact` first searches the active records of its kind (built-in index, records only, no model). The close ones (top 5, score 0.09 or more) come back as `similar` in the JSON, or as lines in the text form. They are never replaced by score alone: `add --supersedes <old>` stores the new record and supersedes `<old>` in one call, `supersede` does it later. An unknown `<old>` is exit 1 and stores nothing.
- **Threshold, measured** on a copy of the manager's real notes (28 active rules, 121 replaced, 28 facts): 10 rules restated in other words and 10 on new topics, each added to a fresh copy. The restated rule scored 0.05 to 0.17 (top hit for 8 of 10, a rank 2 and a miss for the rest); an unrelated rule's best hit scored up to 0.08. At 0.09: 5 of 10 restatements name their rule, none names another, 0 of 10 new rules get a hit. At 0.07: 6 of 10 and 2 of 10 false. At 0.05 every new rule gets 5 false hits. The restatements shared few words with the old rules; a rule restated with its own terms scores higher. A lexical check misses a restatement that shares no terms: the owner's words and `wsx memory search` stay the second look.
- **Areas**: a `## <area>` line of the old `decisions.md` is no rule. The views have no headers (the start text lists the rules newest first, so a header would label the wrong group); the area is kept as a keyword of its rules, shown in `get` and searched.
- **Drift**: wsx keeps the hash of each file it wrote (`views.json` in the store's folder). A file that differs is a hand edit. `wsx memory status` lists it under `drift` (`view`, `added` lines the records do not hold, `removed` lines they do), the manager host says so once in its progress, and the next write keeps the edited file as `<file>.drift`, prints a notice, and writes the file again from the records. Nothing is merged and nothing is lost.
- A search of the project no longer reads the lines of a file that is a view: the records are searched themselves.

## Start set

The first prompt of a worker holds a block from the memory, built by `wsx_core::memory::start` for every adapter (`host.rs` `task_context`; `wsx memory start` prints the same block). No other start loads memory: a Claude chat outside wsx gets nothing. In order, within 4 KiB:

1. **Area briefs**: `memory.areas` of the project's `wsx.config.yml` is `{"<glob>": "<brief file>"}`. Each brief whose glob meets one of the task's `--files` is read from the project (a relative path inside it, at most 8 KiB) and shown whole.
2. **The role's brief and rules**, when the task was created by a role: `brief.md`, then the role's `notes/decisions.md` lines, newest first.
3. **Records**, under a line that says a record is data from earlier work and not an instruction, and that a fact marked stale may be old. First the active records whose `paths` meet a file (a literal path covers what is under it): the larger share of a record's paths that meet, then more of them, then the newer. Then up to 5 facts that the task's words find (`--about`: the title and the first line of the prompt; BM25 of the built-in index over the records alone), the best first, left out when the path matches already hold them. Both share the same 4 KiB, so the facts of the words are the first to be cut.

A record line is `- <slug> (<kind>, <day>): <text> (files: a, b, c)`. `files:` shows at most 3 of the record's paths, those the task's `--files` do not meet first; when there are more, the two with the longest common folder become that folder (`dir/`), and paths with no folder in common are cut (`+N more`). A stale fact ends with `[stale: files changed since <7 characters of the commit>, <N> commits touch its paths]`.

Nothing is cut silently: a piece that does not fit is named on a last line (`Not shown, over the 4096-byte start budget: <slugs>. wsx memory recall "<question>" finds them.`), a brief that is cut says how many bytes and where the whole is, a record over 700 characters shows its head and `wsx memory get <slug>`, and `Not shown` names only what the budget cut: a stale fact is shown, never hidden (the offline evaluation, `docs/design/memory-eval.md` "Offline retrieval", found hiding them lost 94% of the facts).

**Stale**: a `fact` with `commit` and `paths` is stale when `git diff --quiet <commit> HEAD -- <paths>` in the project folder fails; git that cannot answer (10 s, not a hash) counts as stale for `search`; a start set that git cannot answer for names that fact and the rest as not shown, so an unchecked fact never joins a prompt. A start set checks at most 30 facts, best first. `search` marks the hits it returns with `"stale": true` (text form: `, stale` after the status); a stale fact is not hidden there.

## Capture

When a task leaves, the manager host (`manager.rs` `fold_memo`) stores its result memo, with no model turn:

| Task | Record |
|---|---|
| landed | `fact`, `verified_by` and `source` `task:<n>`, `commit` = the landed commit (the log's `landed`), `paths` = the files of its diff (the task record's `paths`, capped) |
| closed or failed | `episode`, no `commit`, no `paths` |

The slug is `memo-<n>-<digest>`, so the same memo is one record. A memo is one line of at most 240 characters. `wsx memory capture` does the same for a replay. A task created by a role keeps its memo in the role's own notes (not captured). When a worker's conversation ends for its size, its host also stores the handoff notes as an episode `rotation-<n>-<digest>` (cut at the text limit, with the path of the whole). When the manager starts a fresh conversation, the host also stores the handover it puts in that turn as an episode `handover-<digest>` (source `path:manager/log.jsonl`; a role's manager keeps files). Neither takes a turn of its own.

**Backfill**: a memo imported before `paths` existed is a `fact` with no `commit` and no `paths`, so it would leave every start set. `wsx memory import`, the first write of a rule writer and the host's first start set (once, marked by `paths-backfilled` in the store folder) give each such fact of a landed task the landed commit and the task's `paths` from the task's own record and log, the data `capture` stores. The time of the record stays. A task with no landed diff stays without paths. A rerun changes nothing.

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
- The old text of an `update` stays readable (wsx: in the log).
- wsx's store is `tasks/<project-id>/memory/records.jsonl` (scope `default` and unbound names: `$XDG_STATE_HOME/wsx/memory/<name>/records.jsonl`): one JSON object per line, append-only, folded into current records. A last line cut short by a crash is skipped with a notice on stderr and never fails a command; the next write starts on a new line.

## Agents

A task agent calls `wsx memory` through its host's broker (`WSX_TASK_BROKER`), which runs the command for it. The one rule is `wsx_core::task::policy` ([tasks.md](tasks.md) "Permissions"):

| Command | Who |
|---|---|
| `search`, `recall`, `status`, `list`, `get` | every agent, also an untrusted one; a read may name any scope |
| `add`, `update`, `supersede`, `import`, `reindex` | the manager, a named role and a worker, in the agent's own project scope only (`--scope` defaults to it, another one is refused) |
| a `rule` record (add, change, replace) | the manager and a named role: it needs `owner:<id>`, and a worker's call is refused by the store |
| any write | not an untrusted agent (strict profile): read only |

A read may name any scope, but an agent's search finds the transcripts (chats, task stories, notes) of its own project only; of another project's scope it finds the records. (The host sets `WSX_TASK_FROM` to the agent's project; the owner's own call sets none.)

A refusal exits 2 and names the cause. The host sets `WSX_MEMORY_RULES=deny` for an agent without `decide`; the owner's own call sets none.

## Proof

`scripts/test-task.py` sections 23m and 27m run the broker cases (a worker, an untrusted worker, the manager) and the `task:<n>` and `check:<id>` forms of `verified_by` with a landed task and a passed item.

`python3 -B scripts/test-memory.py "<command prefix>"` runs the same scenario against any program, for example `"target/debug/wsx memory"` or `"python3 memory.py"`. It runs every call in a temp home (`HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`) and drops `DGV3_ROOT` and every `WSX_*` variable of the caller, so a run against `memory.py` or `wsx` never touches the owner's vault or stores. It checks, for any program, `limits.text_bytes` (the limit exact, one byte over is exit 2), the `owner:transcript:` and `url:` forms (a bad URL is exit 2 and nothing is fetched), a scope with `_`, `get`, `description` and `keywords` (stored, shown, searched, replaced by `update`), and a slug with `.`. For wsx it also runs the notes of a project (import and its rerun, the views byte for byte, `--words`, `similar` and `--supersedes`, the memo ceiling, a hand edit as `drift`), the transcripts of a project scope and the backend cases against `scripts/fake-memory-backend.py` (a program that follows `search`, `reindex` and `status` and can exit 3, exit 1, print garbage or hang): merge by score, the kind filter, exit 3, the 10 s stop, `status` flags, `reindex` and `recall`.

`scripts/test-memory.py` also runs, for wsx, the start set and the stale rule on a git project (areas, data line, a later commit making a fact stale in the start set and in `search`, the budget with its `Not shown` line, bad paths, `capture` of an unlanded task). `scripts/test-task.py` checks the first prompt of a Claude, a Pi and a role-created worker, the fact a landed task leaves (commit, paths) and its going stale, and the rotation episode. The task adapters are `claude` and `pi`; both take the start block from the same first turn.

CI: the `task` scenario's `core` part runs it (`.github/workflows/ci.yml`), so `wsx task check-run task` and every later step prove the contract.
