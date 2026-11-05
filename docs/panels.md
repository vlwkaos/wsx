# Panels

A project shows the output of a command, with no model involved: this Mac's memory and disk, a deploy's state, a queue length.

```yaml
# wsx.config.yml in the main checkout
panels:
  - name: device                           # 1-32 of a-z 0-9 - _, unique, up to 16 panels
    command: [scripts/device-status.sh]    # argv, no shell; a relative program is the checkout's
    refresh: 60                            # seconds while shown; 0 (default) = only on open; else at least 10
    timeout: 30                            # seconds, 1..=300, default 30
```

## Run

- wsxd runs it: one runner and one cache for the web client and the TUI (`crates/wsx-daemon/src/panel.rs`, request `panel`, daemon revision 42). The command runs in the project's main checkout, in its own process group, with stdin closed, `NO_COLOR=1` and `TERM=dumb`, outside any task boundary. A request names only a project wsxd manages.
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

## Clients

- Web (`crates/wsx-web/src/panels.rs`, `web/panels.js`): `GET api/projects/<id>/panels` (names, `refresh`, config notice) and `GET api/projects/<id>/panels/<name>[?run=1]`. They need the right for running commands, `terminal_input`, and the terminal panel; else 403. The project screen's header shows a Panels icon when the project declares a panel and the device may read it, and `#/p/<id>/panels` opens the page (a sheet over the manager, a page without `task_view`). The page asks with `run=1` on open and on *Refresh*, again each second while a run goes, and after `refresh`; a hidden page asks nothing.
- TUI (`panel_view.rs`, `ui/panels.rs`): `v` on a project opens its panels. `Tab`/`l` and `h` switch, `j`/`k` scroll, `R` runs it now, `Esc` closes.

Proof: `scripts/runtime-smoke.py` ("panels in wsxd", "panels in the TUI"), `scripts/test-web-client.py --part tasks` and `scripts/web-panels-check.mjs`.
