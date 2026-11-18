# Chat capability: one table

Owner rule (studio-app web-income, message 49): a chat can do what its project's manager can. Kept per chat: its own log, notes, tasks it owns. Kept for the repo's home chat: it hears what no chat owns (`say` with no `--to`).

## Cause

Every chat already runs with `Role::Manager` (all caps). The drift was outside the cap list: four places decided what a chat may do, and the prompt was written apart from them.

| Refusal (web-income) | Check | First wrong step | Prompt promised |
|---|---|---|---|
| `no message 47 of the owner in this chat` | `memory/manage.rs check_words` | `broker.rs run_memory` ran the command with actor `""` = owner, so `ManagerDir::existing_in(project, None)` opened the **home** chat's log; ids are per chat | "record each rule at once" |
| `--skills: the project lends no skills` | `cmd/new.rs` reads `tasks.skills` | `instructions.rs` offered `new --skills` whenever the agent had a skills folder, whatever `tasks.skills` said | `new --skills` |
| `only a project's manager acts in another project` | `broker.rs reach` gated on `is_home` | a chat is a manager, but only the home chat passed | "consult `@<project>`" |

## The table (`wsx_core::task::policy`)

| Job | Caps | `reach` (run in another project with `-p`) | Lends skills | Prompt line |
|---|---|---|---|---|
| manager and every chat | all | `new show list add steer stop hold land close`; `say --from-chat` to that project's home chat; `ledger` read | when `tasks.skills: owner` | `reach_line`, `lend_line` |
| organizer (stand-in) | read, answer, backlog, note, recall | none | no | none |
| worker | result, recall, remember | none | no | none |

Readers of the table:

| Reader | Reads |
|---|---|
| `broker.rs reach` | `Policy::reaches(sub)`, `Policy::reach_refusal(other)` |
| `instructions.rs` "Other projects" and skills sentence | `reach_line(role)`, `lend_line(owner_skills)` |
| `cmd/new.rs --skills` | `lend_refusal(owner_skills)` (same switch as `lend_line`) |
| `broker.rs run_memory` | the broker's `actor`, so `--source msg:<id>` resolves in the calling chat |

## Deleted

`is_home` in `reach`; `MANAGER_REACH` in `broker.rs`; the two refusal texts of `reach`; the hand-written reach and `new --skills` sentences in `instructions.rs`; the "only a home chat reaches another project" lines of `docs/tasks.md`.

## Proof

`policy.rs` test: each command of `reach` is in `reach_line` and `reach_refusal`, none outside it; `lend_line` is empty exactly when `lend_refusal` is some. `scripts/test-task.py`: a named chat records a rule from its own message (fails on the base with refusal 1), `new --skills` in a project that lends skills, `say -p other --from-chat`; the prompt of a project that lends none has no `--skills`.
