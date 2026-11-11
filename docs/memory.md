# The `memory` contract

One command surface for stored knowledge: `wsx memory <command>` (wsx, projects and `default`) and any other program that follows this text (dotfiles `memory.py`, vault scopes). Each scope has exactly one store and one program writes it. Design and reasons: [design/memory-system.md](design/memory-system.md).

## Scope

`--scope <name>`: lowercase `a-z`, `0-9` and `-`, 1 to 64 characters, not starting with `-`. `wsx memory` defaults it to the project of the current folder (a registered project's task ID, such as `wsx-8cbe3f08`); other programs require it. `search` may repeat `--scope`. Every command takes `--json` (machine output; without it a short text form) and exits with the codes below. `list` takes no `--scope`.

## Record

| Field | Meaning |
|---|---|
| `slug` | id inside the scope: `a-z`, `0-9`, `-`, 1 to 64 characters |
| `kind` | `rule` (owner's decision), `fact` (verified about the code), `episode` (what a task did or learned) |
| `status` | `active` (default), `superseded`, `failed` (with `lesson`) |
| `text`, `title` | the claim, at most `limits.text_bytes` bytes of the store (see "Limits"); optional title, one line |
| `source` | required on every write: `msg:<id>`, `task:<n>`, `path:<file>`, `transcript:<ref>` |
| `updated` | unix ms of the last write |
| `verified_by` | required for `rule` and `fact`, else exit 2. Forms: `owner:<msg id>` (digits, a wsx message), `owner:transcript:<ref>` (the owner's words outside wsx), or a relative path with no `..` that exists under the project root. wsx also takes `task:<n>` (a landed task of the project) and `check:<id>` (a passed check item of the project). `episode` needs none |
| `commit` | facts: the commit it was true at |
| `superseded_by` | slug of the newer record |
| `lesson` | why a `failed` record failed |

A reader ignores fields it does not know, and a program keeps the fields of its own that it does not own. Root of a relative `verified_by` path: the project path for a project scope, else the current folder.

`owner:transcript:<ref>`: `<ref>` is the path of an existing transcript file (absolute, or relative to that root), optionally followed by `#L<line>` (a number from 1). The program checks that the file exists and is a file; it does not read it. A date or a name alone is no proof (`owner:2026-11-11` is exit 2). `owner:<digits>` stays the form for a wsx message.

## Limits

`status` declares what the store keeps in `limits`: `{"text_bytes": N}`, the most bytes a record's `text` may hold (wsx's built-in store: 4096). A write over it is exit 2 and says the limit. A reader or writer takes N from `status` and never assumes 4096; the vault store may declare another.

## Hit

`slug`, `scope`, `path` (the file the record lives in), `kind`, `status`, `score` 0..1 (higher is better), `excerpt` (at most 240 characters).

wsx also searches the transcripts of a project scope (a chat message or reply, a task's prompt, result and story, the manager's decision and memo lines; "Search" in [tasks.md](tasks.md)) and returns them as hits: `slug` is the line's `reference` (`<project>:<id>`, `<project>#<task>`, `<project>:notes/decisions.md`), `kind` one of `message`, `reply`, `task`, `result`, `history`, `decision`, `memo`, `status` `active`, `path` the log or note file it was read from, and the keys of a `wsx task search` hit (`project`, `role`, `id`, `task`, `unix_ms`, `by`, `reference`, `quote`, `outcome`). A reader ignores the keys it does not know and treats a `kind` it does not know as opaque. `--kind` takes any of these kinds.

## Commands

| Command | Arguments | Output |
|---|---|---|
| `search` | `<query> [--limit 10] [--include-inactive] [--kind k]` | `[hit]` by score, newer first on a tie; nothing found: `[]`, exit 0. Active records only unless `--include-inactive` |
| `recall` (wsx) | `<question>` | the best 5 `[hit]`: the top 5 by score of a search's top 20 (no model yet) |
| `add` | `--kind K --slug S --text-file F\|- --source ID [--title T] [--verified-by V] [--commit C]` | the record. A near duplicate returns the old record unchanged (NOOP), exit 0 |
| `update` | `--slug S [--text-file F\|-] --source ID [--verified-by V] [--commit C] [--status failed --lesson L]` | the record. The old text stays in the log. `--status failed` needs `--lesson` and is INVALIDATE |
| `supersede` | `<old> <new> --source ID` | the old record, now `superseded` with `superseded_by` set to `<new>` |
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

- `wsx memory search` runs the built-in, then `<backend> search --scope S --limit N [--include-inactive] [--kind k] --json -- <query>`. The hits of both merge: one hit per `(scope, slug)` with the higher score, best first, at most `--limit`. A hit of the backend with another `kind` than `--kind`, or not `active` without `--include-inactive`, is dropped.
- The built-in always answers. The backend adds hits; when it exits 3 or any other code than 0, does not answer in 10 s, cannot start or prints no list of hits, wsx prints `wsx: memory backend <command> unavailable (<why>); the built-in answered` on stderr and returns the built-in hits (exit 0, clean JSON on stdout).
- `status` runs `<backend> status` for its `search` flags and reports `backend` (the command; `builtin` with none) and those flags. A backend that cannot be reached gives `backend_error` and `network` and `model` false: only the built-in answers.
- `recall` and the broker's `search` use the same merge.
- A backend runs in the process of `wsx memory` (for an agent, its host) with the owner's rights, not inside the agent's boundary. An agent supplies only the query, which goes after `--` in the arguments with no shell. Set a backend that declares `network` only if the owner wants a strict agent's query to reach it.

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
| `search`, `recall`, `status`, `list` | every agent, also an untrusted one; a read may name any scope |
| `add`, `update`, `supersede`, `reindex` | the manager, a named role and a worker, in the agent's own project scope only (`--scope` defaults to it, another one is refused) |
| a `rule` record (add, change, replace) | the manager and a named role: it needs `owner:<id>`, and a worker's call is refused by the store |
| any write | not an untrusted agent (strict profile): read only |

A read may name any scope, but an agent's search finds the transcripts (chats, task stories, notes) of its own project only; of another project's scope it finds the records. (The host sets `WSX_TASK_FROM` to the agent's project; the owner's own call sets none.)

A refusal exits 2 and names the cause. The host sets `WSX_MEMORY_RULES=deny` for an agent without `decide`; the owner's own call sets none.

## Proof

`scripts/test-task.py` sections 23m and 27m run the broker cases (a worker, an untrusted worker, the manager) and the `task:<n>` and `check:<id>` forms of `verified_by` with a landed task and a passed item.

`python3 -B scripts/test-memory.py "<command prefix>"` runs the same scenario against any program, for example `"target/debug/wsx memory"` or `"python3 memory.py"`. It sets `HOME` and `XDG_STATE_HOME` to a temp folder. It checks `limits.text_bytes` (the limit exact, one byte over is exit 2) and the `owner:transcript:` forms for any program. For wsx it also runs the transcripts of a project scope and the backend cases against `scripts/fake-memory-backend.py` (a program that follows `search`, `reindex` and `status` and can exit 3, exit 1, print garbage or hang): merge by score, the kind filter, exit 3, the 10 s stop, `status` flags, `reindex` and `recall`.
