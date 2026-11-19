# Home: labels or groups

Task 541. Mocks of the web Home on task 510's branch (For you row, one state on every chat row). The owner organizes chats alone; no manager or agent sets either.
Both options reuse `homeOrder`; its order holds inside a filter or a group. One accent, no colour per label or group.

Pages are made by `docs/design/home-labels/build.py` and shot by `capture.mjs`. Dark pages show the whole list. Light pages show a state in use: the filter `work` (A), the group Clients folded (B).

| | A. Labels | B. Groups |
|---|---|---|
| Row | Labels after the name, small and muted. A chat has 0 to many. | Row unchanged. The group is its header. |
| Set | ⋯ or long press opens a picker: tick a label, type a new one, tap a ticked one to remove it. | ⋯ or long press opens a menu: pick one group, type a new one, or Ungrouped. |
| List | Chip row under the title: All, then each label; one active. Filtered list keeps `homeOrder`. For you stays. | Header per group (name, count, fold arrow; a dot when a folded group holds unread). Ungrouped last. |
| Picture | `home-a-*`, `picker-a-dark-390.png` | `home-b-*`, `move-b-dark-390.png` |
| wsx-core | A file per chat folder beside `dismissed` (`chats_home::DISMISSED_FILE`): `labels`, one label per line, written with `store::write_atomic`. | Same: `group`, one name. |
| Wire | `chat.labels: []` in the chats row of `chats_home`. The chip row is the union of all rows' labels. | `chat.group: string \| null`. Group order needs a rule (below). |
| Web | `homeOrder` unchanged; filter on its output. | Split its output by `group`; fold state is per device. |
| Cost | A label exists only while a chat has it: no registry, no delete. | Same. An empty group vanishes. |

## Owner questions

1. Several labels on a chat (A) or one place (B)?
2. Group order (B): by name, by the owner's order, or by the group's most urgent chat?
3. Phone: always show ⋯, or only long press?
4. Does Dismiss clear labels or group? Dismiss keeps them as drawn; a re-listed chat returns with them.
5. Does For you follow the filter (A)? Drawn: no.
6. Does the filter or the fold state stay between visits, and on every device?
7. Same in the TUI chat section?
8. Remove a label everywhere: not drawn; a label goes when its last chat drops it. Enough?
9. A colour per label as a variant: not drawn.
