# Terminal view

An agent publishes a small markdown document for its own pane. wsx shows it over the top of that terminal when the user asks, so a diagram or table is visible without leaving the session. Status: prototype.

```text
agent ── wsx view set ──▶ wsxd (document per pane) ── snapshot metadata ──▶ wsx TUI
                                   ▲                                          │
                                   └──────── pane_view_get (body) ◀───────────┘
```

## Publish

```sh
wsx view set [--title T] [--page NAME] [--file PATH]   # markdown from stdin or --file
wsx view clear [--page NAME]                           # one page, or every page
```

- Pane and runtime generation come from `WSX_PANE_ID` and `WSX_RUNTIME_GENERATION`, as for `wsx agent report`. A stale generation gets `stale_runtime`. The command never starts wsxd.
- Bounds: 65536 bytes per page, 4 pages per pane (the oldest is dropped), a 120-byte title, a page name of 1..32 characters from `A-Z a-z 0-9 - _ .`. Control characters other than newline and tab are refused. A refused publish changes nothing.
- The default page is `main`. Publishing a page again replaces it and makes it the newest.

## Ownership

| Owner | Holds |
|---|---|
| wsxd | Documents per pane, in memory only. They survive a live handoff (`docs/daemon-live-upgrade.md`) and are lost on a cold start. Closing the pane or restarting an exited pane removes them. |
| Snapshot | `pane_views`: pane, surface, and per page the name, title, revision and line count. No body. |
| `pane_view_get` | Body for one page at one width, plus renderer-plugin output. |
| wsx TUI | Markdown rendering, layout, scroll position, read state and all input. |

Wire: protocol 17, daemon revision 23. New fields decode with defaults, so a new client reads an older daemon. `PluginSurface::TerminalTop` is the second surface beside `terminal_right`: `terminal_right` is pulled rows from a plugin, `terminal_top` is a pushed document.

## Display

| Key or input | Effect |
|---|---|
| `Prefix v` | Closed → band → reader → band |
| `Prefix V` | Close the band or reader |
| Wheel over the band | Scroll the band; the terminal does not receive it |
| Other keys with the band open | Go to the terminal as usual |
| Reader: `j/k` `↓/↑`, `space/b` `PgDn/PgUp`, `g/G`, `[` `]`, `q` or `Esc` | Line, page, ends, previous/next page, back to the band |

- The band covers the top rows of the terminal viewport. Its height is the content plus one status row, at most one third of the terminal. It is not drawn when the terminal has fewer than 6 rows.
- The band is an overlay. The PTY size, the terminal frame, and the cursor and mouse coordinates below the band do not change. Covered rows are hidden, and a cursor on a covered row is hidden.
- The reader covers the whole terminal viewport and takes keys and paste. The terminal keeps running underneath and receives none of them.
- The title bar shows `▤ view` when the pane has a document, and `▤ view •` until the user has opened the newest revision.
- The agent cannot open the band. `view_auto_open = true` (Global Settings, Terminal, "View opens itself") opens the band, never the reader, when the pane shown in Terminal mode publishes. It needs a prefix chord with a Workspace key, because that chord is the way to close the band.
- A configured Workspace key of `v` keeps its meaning; `Prefix v` then does not open the view.
- The TUI fetches every page of the shown pane when its metadata arrives. Opening the band and turning a page then need no request. A new revision or width keeps the previous rendering on screen until the new one is ready.

## Markdown

Headings, paragraphs, ordered and unordered lists, task items, block quotes, rules, `**bold**`, `*emphasis*`, `~~strike~~`, `` `code` ``, links (label only), fenced code and pipe tables with alignment. Table cells wrap inside their column. Lines that contain box-drawing characters or start with four spaces are kept as written. Width uses terminal cells, so CJK text stays aligned. Lines that cannot wrap are cut with `…`. HTML and images are not rendered.

A fenced block whose language has a renderer plugin is replaced by the plugin output (`docs/plugins.md`, "Fenced-block renderers"). Without the plugin, or when it fails, the block shows as its source. `plugins/wsx-mermaid` is the reference renderer for `mermaid`.

## Verification

```sh
cargo build --locked -p wsx -p wsx-daemon
python3 scripts/runtime-smoke.py            # CLI, wire, bounds, stale generation, renderer bounds, handoff
python3 -B scripts/test-terminal-view.py --keep   # band, wheel, reader, chip, close, auto-open
python3 -B scripts/test-terminal-view.py --plugins ~/.config/wsx/plugins   # with wsx-mermaid installed
```

Both scripts use their own HOME, state and wsxd. `--keep` retains text and ANSI captures under `.work/tv-<pid>/captures`.

## Human Verify

Pass if the band opens with no visible delay, typing under it feels unchanged, the status row makes closing obvious, and the reader reads comfortably at your usual width. Fail if the band flickers when the agent republishes, the unread chip is easy to miss or distracting, or a diagram is hard to read against the band background.
