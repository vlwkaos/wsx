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
| `text`, `title` | the claim, at most 4096 bytes; optional title, one line |
| `source` | required on every write: `msg:<id>`, `task:<n>`, `path:<file>`, `transcript:<ref>` |
| `updated` | unix ms of the last write |
| `verified_by` | required for `rule` and `fact`, else exit 2. Forms: `owner:<msg id>` (digits), or a relative path with no `..` that exists under the project root. wsx also takes `task:<n>` (a landed task of the project) and `check:<id>` (a passed check item of the project). `episode` needs none |
| `commit` | facts: the commit it was true at |
| `superseded_by` | slug of the newer record |
| `lesson` | why a `failed` record failed |

A reader ignores fields it does not know, and a program keeps the fields of its own that it does not own. Root of a relative `verified_by` path: the project path for a project scope, else the current folder.

## Hit

`slug`, `scope`, `path` (the file the record lives in), `kind`, `status`, `score` 0..1 (higher is better), `excerpt` (at most 240 characters). wsx adds more keys for chat and task hits; a reader ignores them.

## Commands

| Command | Arguments | Output |
|---|---|---|
| `search` | `<query> [--limit 10] [--include-inactive] [--kind k]` | `[hit]` by score, newer first on a tie; nothing found: `[]`, exit 0. Active records only unless `--include-inactive` |
| `add` | `--kind K --slug S --text-file F\|- --source ID [--title T] [--verified-by V] [--commit C]` | the record. A near duplicate returns the old record unchanged (NOOP), exit 0 |
| `update` | `--slug S [--text-file F\|-] --source ID [--verified-by V] [--commit C] [--status failed --lesson L]` | the record. The old text stays in the log. `--status failed` needs `--lesson` and is INVALIDATE |
| `supersede` | `<old> <new> --source ID` | the old record, now `superseded` with `superseded_by` set to `<new>` |
| `reindex` | none | `{"scope","ok"}` |
| `status` | none | `{"scope","store","backend","search":{"network":bool,"model":bool},"counts":{"active","superseded","failed"}}` |
| `list` | none | `[{"scope","store"}]` |

`add` rules:
- A near duplicate is an active record of the same kind with the same text after lowercasing and collapsing white space, or with a word set (split on white space) whose Jaccard index with the new text is 0.9 or more. The old record is returned and nothing is written.
- A slug that exists with different text is exit 2 (use `update`).
- `update` of an unknown slug and `supersede` of an unknown `<old>` or `<new>` is exit 1. `<new>` must be active, else exit 2.

`search` makes no network or model call unless `status` declares one. The score of the built-in match is the share of query words (lowercased, split on non-alphanumerics) found in the slug, title and text; a hit needs at least one.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok (including `[]` and NOOP) |
| 1 | not found (slug) |
| 2 | usage, an invalid value, or an invalid `verified_by` |
| 3 | store or backend unavailable (lock, I/O) |
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
| `search`, `status`, `list` | every agent, also an untrusted one; a read may name any scope |
| `add`, `update`, `supersede`, `reindex` | the manager, a named role and a worker, in the agent's own project scope only (`--scope` defaults to it, another one is refused) |
| a `rule` record (add, change, replace) | the manager and a named role: it needs `owner:<id>`, and a worker's call is refused by the store |
| any write | not an untrusted agent (strict profile): read only |

A refusal exits 2 and names the cause. The host sets `WSX_MEMORY_RULES=deny` for an agent without `decide`; the owner's own call sets none.

## Proof

`scripts/test-task.py` sections 23m and 27m run the broker cases (a worker, an untrusted worker, the manager) and the `task:<n>` and `check:<id>` forms of `verified_by` with a landed task and a passed item.

`python3 -B scripts/test-memory.py "<command prefix>"` runs the same scenario against any program, for example `"target/debug/wsx memory"` or `"python3 memory.py"`. It sets `HOME` and `XDG_STATE_HOME` to a temp folder.
