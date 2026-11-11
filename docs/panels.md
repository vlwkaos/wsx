# Panels

A project shows the output of a command, with no model involved: a deploy's state, a queue length, this Mac's own readings. wsxd also has one built-in panel for this machine ("Machine panels").

```yaml
# wsx.config.yml in the main checkout
panels:
  - name: device                           # 1-32 of a-z 0-9 - _, unique, up to 16 panels
    command: [scripts/device-status.sh]    # argv, no shell; a relative program is the checkout's
    refresh: 60                            # seconds while shown; 0 (default) = only on open; else at least 10
    timeout: 30                            # seconds, 1..=300, default 30
    format: text                           # text (default) or json: see "JSON panels"
    scope: project                         # project (default) or machine: see "Machine panels"
```

## Run

- wsxd runs it: one runner and one cache for the web client and the TUI (`crates/wsx-daemon/src/panel.rs`, request `panel`, daemon revision 43). The command runs in the project's main checkout, in its own process group, with stdin closed, `NO_COLOR=1` and `TERM=dumb`, outside any task boundary. A request names only a project wsxd manages.
- It runs only when a client asks: on open, on a manual refresh (`run`), and when a client reads a result older than `refresh` (never for `refresh: 0`). One run at a time per panel; a read during a run gets the last result with `running`. Nothing asks while nothing is shown, so nothing runs.
- The last result stays in wsxd's memory, so a reopen shows it at once with its age (`age_ms`, by wsxd's clock). A wsxd restart clears it.
- The config is read at each request: an edit applies at the next open.

## Result

| Part | Rule |
|---|---|
| Output | stdout as written, ANSI codes and control characters stripped (`\r\n` is a newline), the last 16 KiB; `truncated` says the start was cut |
| Exit 0 | `ok`: stdout only |
| Non-zero exit or a signal | `failed` with the exit code; stdout and then stderr, the last 16 KiB |
| Timeout | the group is killed, `timed_out`, the output so far and `stopped after <n> s` |
| Not started | `not_started` with the reason (missing program) |
| No run yet | `pending` |

A panel that does not validate (a bad name, `refresh` 1..9, a duplicate) fails the whole `wsx.config.yml` like any other known key of the wrong value: the project notice says `panels[<n>].<field>`, and the TUI shows it at start.

## JSON panels

With `format: json` the command prints one object, and the clients draw it as a UI instead of monospace text. `crates/wsx-core/src/panel.rs` (`parse_doc`) reads it once when a run ends with exit 0; the result carries the typed `doc` (`PanelRun::doc`) and `output` keeps the raw text. A failed, timed-out or not-started run stays text.

```json
{"title": "This Mac", "sections": [
  {"title": "Memory", "items": [
    {"kind": "gauge", "label": "Memory", "used": 13.6, "total": 16, "unit": "GB", "level": "warn"},
    {"kind": "stat", "label": "Pressure", "value": "normal", "note": "vm_stat", "level": "ok"}]},
  {"title": "Storage", "items": [
    {"kind": "table", "columns": ["Volume", "Free"], "rows": [["Macintosh HD", "88 GB"]]},
    {"kind": "list", "items": ["Time Machine: 2 days ago"], "level": "bad"}]}]}
```

| Part | Rule |
|---|---|
| Items | `gauge` (`label`, `used` >= 0, `total` > 0, `unit?`, `level?`), `stat` (`label`, `value`, `note?`, `level?`), `table` (`columns`, `rows`), `list` (`items`, `level?`), `cards` (`title?`, `items`): see "Cards" |
| Links | a table cell and a list entry may be `{"text": "...", "url": "..."}` for a string: see "Links" |
| `level` | `ok`, `warn` or `bad`: the theme's state colours, shown with the word, never by colour alone |
| Strings | text or a number; cut at 200 characters with `…` |
| Limits | 12 sections, 50 items per section, a list of 50 entries, a table of 1..=8 columns and 100 rows, 48 cards per `cards` item (rows with another cell count than `columns` skip the table); more is cut and the notice says `cut to the limits: ...` |
| Skipped | an unknown `kind`, a bad `level` or a bad value skips that item, one notice line each (5 at most, then `and n more`); an unknown field is ignored, so a script written for a newer wsx keeps its items |
| Not a document | invalid JSON, a root without `sections`, or no section left: no `doc`; the clients show the text as written with the notice |
| Age | the run's age as for a text panel (wsxd's clock), not a field of the document |

## Links

A table cell and a list entry may be a string or number as above, or an object `{"text": "...", "url": "..."}`. `text` follows the string rule. `url` is one of:

| Kind | Rule |
|---|---|
| Absolute | `https://` (also `http://`), a host, no credentials (`user:pw@`) |
| Project page | a path in the project's main checkout to an `.html` or `.htm` file: no leading `/`, no `..`, no empty part, no dot-folder, no `?`, `#` or `:` |
| Both | at most 2048 bytes, no space, backslash or control character |

Anything else keeps the text and drops the link, with one notice line: `section 1, item 2: link at row 3, column 1 dropped: <reason>` (a list: `link of entry 2 dropped`). An object with no `url` is its text; with no `text` it is a bad value. Rows cut at the limits take their links with them.

- Wire: the typed cell stays the text in `rows`/`items`; the targets are a sparse `links` list on the `table` and `list` item (`{row, col, url}`, a list entry is `{row: index, col: 0}`), left out when empty. A client from before links reads the text and ignores the field, so the daemon revision stays 50 (`output` still carries the raw document).
- The machine card keeps the links of the project panels it merges. A project page link there also names its project (`project`, the project's name), so a client opens it in that project's checkout.
- Web: an `https` link opens in a new tab (`target="_blank" rel="noopener noreferrer"`). A project page link goes to `#/p/<project>/files/<path>`, the files view, which shows the page in the sandboxed frame (`api/file/page`, `view/<token>/`; [web-client.md](web-client.md) "Files"); a site without the `files_read` right or the files panel shows the text alone.
- TUI: no terminal link is made. A link shows as `text ↗ target` in the accent colour with an underline; in a table the target is cut with the cell when the width is short, like any cell.

## Cards

A `cards` item shows what a row cannot: a picture with a few words, as a grid of cards.

```json
{"kind": "cards", "title": "Trending", "items": [
  {"title": "Cat video", "subtitle": "Channel A", "value": "1.2M", "note": "Rising fast", "image": "media/cat.jpg",
   "url": "https://www.youtube.com/watch?v=abc", "level": "ok"}]}
```

| Field | Rule |
|---|---|
| `title` | required, a string or number (200 characters, as every string); a card without one skips the item |
| `subtitle`, `value`, `note` | optional strings |
| `url` | as for a link ("Links"); a bad one is dropped with a notice and the card stays |
| `image` | a path in the project's main checkout: `.jpg`, `.jpeg`, `.png`, `.webp` or `.gif`, no leading `/`, no `..`, no dot-folder, no symbolic link on the way, a file of at most 2 MiB. Missing, too large or refused: the card shows without it, one notice line (`card 3 image dropped: the image file is missing`) |
| `level` | `ok`, `warn` or `bad`, as everywhere: the word beside the value |

- The check happens when the run ends (wsxd, in the project's main checkout); the web reads the file again when it loads the picture. 48 cards at most per item; more is cut (`cut to the limits: cards`).
- Web: a grid that fills the width (two columns on a phone). A picture is on top in a box of one shape (16:9), loaded when it scrolls near (`loading="lazy"`), from `GET api/file/image?project=&path=` (`files_read` and the files panel, the pinned root without a symbolic link, a picture type, 2 MiB; 400 `not_an_image`). A site without `files_read` shows the cards without pictures. The note is clamped to 3 lines. With a `url` the whole card is the link, as in "Links". The machine card keeps the cards; a card with a picture or a project page names its project (`project`).
- TUI: one row per card, `title ↗ subtitle  value  word` (title and subtitle give way to the width), the title underlined with the target at the row's end where it fits whole when there is a `url`, and the note dimmed below, cut to the width. No picture.
- A client from before cards cannot decode a document with one, so the daemon revision is 51 (a wsxd from before it skips a `cards` item with a notice).

## Machine panels

The web's *Machine* tab and the TUI's `M` show the machine, with no project, as one card, *This Mac*: wsxd's built-in panel `this-mac`, then the sections of every `scope: machine` panel of the projects, merged in ("Machine card").

### The built-in panel

wsxd reads it itself (`crates/wsx-daemon/src/machine.rs`): system calls, no script, no disk scan, no project. A `panel` request with no project lists it (`{name: "this-mac", refresh: 60, scope: machine}`) and reads it; a project panel's rules hold: it reads on open, on a manual refresh and after 60 s, one read at a time, the last result in wsxd's memory. The result is a document (`PanelRun::doc`, title "This Mac", "This machine" off macOS) and `output` is that document as JSON. A reading the system does not give is left out, never an error. Daemon revision 46.

| Section | Item | Read with | `warn` / `bad` |
|---|---|---|---|
| Memory (macOS) | *Memory* gauge: app, wired and compressed (as Activity Monitor) of the RAM, GB of 1024³ | `host_statistics64`, `hw.memsize` | 85 % / 95 % |
| | *Pressure* stat: normal, warning, critical | `kern.memorystatus_vm_pressure_level` | warning / critical |
| | *Compressed* stat: its size, and what it holds as the note | `host_statistics64` | none |
| | *Swap* gauge of the swap total, or *none* | `vm.swapusage` | used is 25 % / 50 % of the RAM |
| Load | *Load average* stat: 1, 5 and 15 min, the cores as the note | `getloadavg` | 1-min load per core 1.0 / 2.0 |
| Disks | a gauge per volume, its name, GB of 10⁹ (as Finder) | `getfsstat`, `getattrlist`; elsewhere `statvfs("/")` | 90 % / 95 % used |

The thresholds are the constants at the top of `machine.rs`. The disks are `/` (the boot volume: its numbers are the APFS container's, the Data volume included) and every other local volume that is writable, browsable (`MNT_DONTBROWSE` off), not a snapshot, not under `/System/Volumes/` and at least 1 GB: an external disk shows; a disk image, a simulator runtime or a Time Machine snapshot does not. At most 50.

### Project machine panels

A project's panel about the machine has `scope: machine`. It is still a project's command: wsxd runs it in that project's main checkout with the same refresh, cache, formats and limits, and the project screen and `v` still list it. A scope other than `project` or `machine` fails the config like any bad field (`panels[<n>].scope must be project or machine`). The list answer (`PanelInfo::scope`) carries the scope; daemon revision 45.

### Machine card

`wsx_core::panel::machine_view` builds it for both clients: it reads `this-mac`, then every `scope: machine` panel of the projects it is given, and returns the built-in panel's view with the project sections added to its document. A project panel's own run, refresh, cache, formats and limits do not change; only how it is shown does.

| Project panel | In the card |
|---|---|
| A document | its sections after This Mac's, each titled `project · section` (an untitled one: `project`); the document's own title is not shown |
| Text output (no document) | one section `project · panel` with its lines as a list: 50 lines at most, the last entry `+N more` |
| Failed, timed out, did not start, or unreadable | one section `project` with one line at level `bad`: `device: failed (exit 2)`, `device: timed out`, `device: did not start`, or the first line of the error |
| No result yet | one section `project` with `device: no result yet` |

- `doc.level` (`PanelDoc::level`, set only on the card) is the worst `warn` or `bad` of all items, This Mac's and the projects', failing lines included. Both clients show it as a word beside the title (*This Mac  warn*); none when every level is `ok` or absent.
- `refresh` and `age_ms` of the card are those of the panel due soonest, so a client that reads again then keeps every panel fresh (a read starts a stale panel's run). A read with `run` starts every panel's run.
- A project whose list fails is left out. `scope: project` panels never join the card.

### Clients

- Web: `GET api/machine[?run=1]` gives the card as a panel view (as `GET api/projects/<id>/panels/<name>`), for the granted projects only. The right is the same as for any panel (`terminal_input`, terminal panel; else 403); a project outside the grant is never asked.
- The *Machine* tab (`#/machine`) is in the bottom bar and the rail, and has a switch in Settings > UI, for every device that may read panels. It shows the card with the same panel view: it runs on open, has *Refresh*, and asks again after `refresh`. It starts again when the project list changes.
- TUI: `M` in the workspace opens the card (the footer's last hint, the help line); `j`/`k` scroll, `R` runs it now, `Esc`, `q`, `v` and `M` close.

## Clients

- Web (`crates/wsx-web/src/panels.rs`, `web/panels.js`): `GET api/projects/<id>/panels` (names, `refresh`, config notice) and `GET api/projects/<id>/panels/<name>[?run=1]`. They need the right for running commands, `terminal_input`, and the terminal panel; else 403. The project screen's header shows a Panels icon when the project declares a panel and the device may read it, and `#/p/<id>/panels` opens the page (a sheet over the manager, a page without `task_view`). The page asks with `run=1` on open and on *Refresh*, again each second while a run goes, and after `refresh`; a hidden page asks nothing. The panel's top row is one muted line, *Updated 7 s ago, every 60 s* (or *on open*; a failed run starts with its result, *Failed (exit 2), updated …*), with *Refresh* as a button at the row's end, 44 px high, that says *Running…* while a run goes.
- TUI (`panel_view.rs`, `ui/panels.rs`): `v` on a project opens its panels. `Tab`/`l` and `h` switch, `j`/`k` scroll, `R` runs it now, `Esc`, `q`, `v` and `M` close.
- A `doc` is drawn in both: web cards per section (`role="meter"` bars, a table that scrolls sideways) and TUI boxed sections with `█░` bars; the notice shows above it.

Daemon revision 44 adds `format`, revision 45 `scope`, revision 46 the built-in panel, revision 51 the `cards` item. Proof: `scripts/runtime-smoke.py` ("panels in wsxd", "panels in the TUI", also `scope`, the `links` panel and the `cards` panel, the built-in panel against `sysctl` and `statvfs`, and `M` with and without a project's machine panels), `scripts/test-web-client.py --part tasks` (`api/machine`) and `scripts/web-panels-check.mjs` (links and cards on a site with and without `files_read`; the card with a project's text, failing and document panels, and on a site whose project declares none).
