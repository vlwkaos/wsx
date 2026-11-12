# A table under a list item

Report (ir, 2026-11-12): "table breaks inside list". A manager's reply has numbered items, and each
has a GFM table on the next lines, not indented (`scripts/fixtures/list-tables.md`). The web showed
the table lines as text of the item, and the TUI chat showed them as `| a | b |` lines.

## What the specs and renderers do

Four shapes: **A** `1. Head` then the table, no blank line, not indented; **B** the same with a
blank line before the table; **C** the table indented to the item's content column (3 for `1. `,
2 for `- `); **D** a paragraph line, then the table.

| Shape | GitHub (`gh api markdown -f mode=gfm`, 2026-11-12) | markdown-it 14 | others |
|---|---|---|---|
| A | text lines | text | text: micromark (lazy line), pulldown-cmark (container must match), cmark-gfm (parent must be the paragraph) |
| B | table after the list; the next `2.` item is `<ol start="2">` | same | same (inferred) |
| C | table inside the item (2 spaces under `1. ` is text) | same | same (inferred) |
| D | table | table | marked, micromark, cmark-gfm: table; pulldown-cmark: table, "not portable" |

- GFM spec: "The table is broken at the first empty line, or beginning of another block-level
  structure". It does not say whether a table may interrupt a paragraph.
- markdown-it's table rule needs the delimiter row at the item's indent
  (`sCount[nextLine] < blkIndent` returns false), so A stays text. Its `alt: ["paragraph"]` is why D works.
- Too wide: glamour wraps cells to the width; termimad shrinks the columns, hard-wraps the cells
  and drops columns below 3 cells each; GitHub's stylesheet scrolls the table sideways. None stacks
  rows as records (no primary source found), which the web does on a phone already.

## Choice

GitHub shows shape A as text, but the manager writes it and means a table. Both chats read A as
a table, and B, C and D as GitHub does. One rule: **a header row and a delimiter row of the same
number of cells start a table whatever line is above, indented to the item or not; it ends at the
first line without `|`**. After a list item the table shows right after the list (as B), the next
item keeps its number.

- Web (`markdown.js`): one block rule before `table` (a paragraph terminator) asks markdown-it's
  own table rule with the indent of the page. Nothing else changes, the vendored file stays as published.
- TUI (`ui/chat_table.rs`): the chat showed no markdown, so the same rule is read from the lines of
  a message (not inside a fenced block) and drawn as aligned columns, the header bold over a rule.
  Too wide: the widest column gives cells down to 6, its words wrap, and rows that wrap get a
  rule between them. If even that does not fit, each row is a record: its first cell in bold, then
  `Header: value` lines.
- On the web a table right after a blockquote line stays text, as before; the TUI reads any indent.

## Sources

- https://github.github.com/gfm/#tables-extension-
- https://spec.commonmark.org/0.31.2/#list-items
- https://github.com/markdown-it/markdown-it/blob/14.1.0/lib/rules_block/table.mjs
- https://github.com/markdown-it/markdown-it/blob/14.1.0/lib/parser_block.mjs
- https://github.com/github/cmark-gfm/blob/master/extensions/table.c
- https://github.com/micromark/micromark-extension-gfm-table/blob/main/dev/lib/syntax.js
- https://github.com/pulldown-cmark/pulldown-cmark/blob/master/pulldown-cmark/specs/table.txt
- https://github.com/markedjs/marked/blob/master/src/rules.ts
- https://github.com/charmbracelet/glamour/blob/master/ansi/table.go
- https://docs.rs/crate/termimad/latest/source/src/fit/tbl_fit.rs
