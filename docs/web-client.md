# Hosted web client

`wsx web serve [<site>]` hosts one paired web site: a remote chat with the agents of the granted projects, plus optional files and terminal panels. It is a `wsxd` client like the TUI. It never starts, stops or replaces `wsxd`. The terminal panel needs a `wsxd` that reports the capability `raw_terminal_stream` ([terminal-raw-stream.md](terminal-raw-stream.md)). Against a daemon without it the panel says that the daemon must be upgraded (`daemon_upgrade_required`); chat, files and the inbox work. Deployment commands are in [web-deploy.md](web-deploy.md).

```sh
wsx web vapid                          # print a new Web Push key pair for a site file
wsx web serve   [<site>]               # serve on 127.0.0.1:<port> until SIGINT or SIGTERM
wsx web pair    [<site>]               # print a one-time pairing code (10 minutes, one use)
wsx web devices [<site>] [--revoke <id>] [--json]
```

## Site file

`<site>` (or `--site <site>`) is a site file path, or a site name: the `id` or the file name without `.toml` of a file in the site folder, `${XDG_CONFIG_HOME:-$HOME/.config}/wsx/web/`. A value with a `/` or ending in `.toml` is a path. With no `<site>`, the only `*.toml` file of the site folder is used; with none the command says how to make one, and with two or more it lists them by id and file and asks for one. The site folder is the only place a command looks: a running server is not asked, because its state folder is named by the id being looked for. `wsx web pair` prints the site id, the site file and the URL the code is for.

One site per process. The file must be a regular file owned by the user with mode `600`. Unknown fields are refused.

```toml
id = "owner"                              # a-z, 0-9, '-'; names the state directory and cookie
name = "wsx"                              # shown in the page and the installed app
port = 7431                               # loopback port
public_origin = "https://wsx.tailnet.ts.net"   # exact origin the browser uses
base_path = ""                            # omit for a service or alt-port site; "/app/wsx" for a sub-path
panels = ["chat", "files", "terminal"]
theme = "default"                         # default, or the absolute path of a theme (web-themes.md)

[icon]                                    # optional; replaces the theme's icons
png_192 = "/abs/icon-192.png"
png_512 = "/abs/icon-512.png"

[access]
projects = ["/abs/path/to/project"]       # 1 to 32 absolute paths, or "all" for every project wsx manages
capabilities = ["chat_send", "agent_write", "files_read", "files_write",
                "terminal_view", "terminal_input", "media_upload", "media_download", "push"]
agent_command = []                        # required with session_start, for example ["claude"]

[terminal]                                # optional
ligatures = true                          # false shows operators such as => as separate characters

[media]                                   # optional; these are the defaults
max_file_bytes = 26214400
quota_bytes = 1073741824
ttl_seconds = 604800

[tasks]                                   # optional; when task notifications go out ("Task notifications")
merge_seconds = 3                         # 1-60
digest_quiet_seconds = 180                # 1-3600
digest_max_seconds = 900                  # digest_quiet_seconds-86400

[push]                                    # required with the push capability; from `wsx web vapid`
subject = "mailto:you@example.com"
private_key = "<base64url>"
```

| Capability | Allows | Needs panel |
|---|---|---|
| `chat_send` | send and stop messages | chat |
| `chat_answer` | answer a blocked agent's dialog from the chat with keys; needs `chat_send` | chat |
| `agent_write` | exchanges are created as `writer` instead of `read-only` | chat |
| `session_start` | start a new session that runs `agent_command` | chat |
| `files_read`, `files_write` | browse and read text; replace an existing text file | files |
| `terminal_view` | watch a session's live terminal at the pane's own size | terminal |
| `terminal_input` | type, paste, resize the pane, take control from another wsx window | terminal |
| `media_upload`, `media_download` | attach files to a prompt; receive agent-produced files | any |
| `push` | Web Push for this device | any |
| `task_view` | read a project's tasks, reports and manager chat ("Tasks") | chat |
| `task_act` | talk to the manager and act on tasks; needs `task_view` | chat |

`projects` is the grant. A session, pane, worktree or file of a project outside it answers 404, as if it did not exist; `chat::targets` and `chat::projects` are the only code that maps daemon state to the grant. With `"all"`, a project that wsx starts to manage later is reachable at once. The files API and `POST api/sessions` name the project by its wsx id (`project`), and a new session may name a worktree of that project (`worktree`); the page gets both lists in the `workspace` event.

A second site is the same binary with another file, fewer panels and fewer bits. The server enforces every bit; the page only hides what the session answer does not list. `read-only` and `writer` are the broker's cooperative labels. They do not restrict what the agent process can do.

## Security

- The server listens on `127.0.0.1` only. `Host` must equal the host of `public_origin`. `Origin` must equal `public_origin` when present, and is required on every state-changing request. API and media requests with a cross-site `Sec-Fetch-Site` are refused.
- Pairing: `wsx web pair` stores the SHA-256 of a 10-character code. The device exchanges the code for a random 256-bit credential in an `HttpOnly; SameSite=Strict` cookie with `Path=<base>/` (`Secure` for an https origin). Only the credential's SHA-256 is stored. There is no shared token.
- Limits: 240 requests, 20 messages and 6 session starts per minute per device, four open event streams and four open terminals per device, 16 devices. Pairing attempts are limited to five per minute for the whole site, because the proxy hides client addresses; a tailnet member who keeps guessing can therefore delay a real pairing.
- Origin sharing: a service or alt-port site has its own browser origin. Under a sub-path, every app on the same hostname and port is the same browser origin. Script from a sibling app can call this API with the device's cookie; the `Origin` check and `SameSite` cannot tell them apart. Host a site on its own HTTPS port (alt-port root) when another app on that hostname is not trusted.
- On SIGINT or SIGTERM the server stops taking connections, gives open event streams and terminals two seconds, then ends them and exits.
- A revoke takes effect on the next request. An open event stream or terminal ends within 20 seconds, and the device's push subscription is removed.
- CSP is `default-src 'none'` with `'self'` for scripts, styles, fonts, images, media, connections, the manifest and the worker. There is no inline script or style, and a theme cannot add an origin. The terminal WebSocket is covered by `connect-src 'self'`.
- A WebSocket upgrade is not covered by CORS. The terminal endpoint requires the device cookie and an `Origin` header equal to `public_origin`; a missing `Origin` is refused.
- State lives in `<wsx state dir>/web/<id>/` with owner-only modes: `devices.json`, `push.json`, `media/`.

## Base path

The default is a site at the root of its own hostname (a Tailscale Service) or port, with `base_path` omitted. A sub-path remains supported as a secondary shape. Every browser URL is relative to `<base href="<base>/">`, written at serve time, so one build serves all shapes. The manifest `id`, `scope` and `start_url`, the service worker scope and cache list, and the cookie `Path` use the same base. With a base path, each route answers with and without the prefix, because `tailscale serve --set-path` may strip it.

## Screens

The page is a set of ES modules served as written: Preact 10.29.7 and htm 3.1.1 are vendored in `web/vendor/` (`scripts/vendor-preact.sh`, SHA-512 pinned, MIT) and there is no build step. Routes are in the URL fragment.

| Route | Screen |
|---|---|
| `#/` | With `task_view`: Home, one row per project ("Home" under "Tasks"). Without it: the inbox, as `#/sessions` |
| `#/sessions` | Inbox ("All sessions" with `task_view`): sessions by what they need. *Needs you* (blocked, error, exited agent), *Running* (with the activity line and elapsed time), *Finished* (the newest finish first, unseen ones marked "new"), then the remaining agent sessions by project. A section with no session is not shown. Shells are not listed: the inbox is agent work. Task workers and the manager are not listed either (provider `task` or `manager`, or the label `task-<n>` or `task-manager`); their work shows in the project's chat. |
| `#/projects` | Every granted project with its session count, *Files* and a *+* (its name is "New session in <project>") where granted. The project's name opens it |
| `#/p/<project>` | With `task_view`: the project's manager ("Project screen" under "Tasks"). Without it: the sessions, as `#/p/<project>/sessions` |
| `#/p/<project>/sessions` | One project: a heading per worktree (the main checkout first) with a *+* ("New session in <branch>") for that worktree, and its sessions as in the inbox; a worktree with no session says so |
| `#/p/<project>/t/<task>` | With `task_view`: the project screen with the task sheet open ("Task sheet" under "Tasks") |
| `#/s/<session>/chat`, `#/s/<session>/terminal` | One session. The top row has back, the label with the state in words, and a Chat or Terminal switch with the granted panels. A shell (a session with no agent, open or exited) has no chat: it opens on the terminal, and gains the chat when an agent reports in it |
| `#/p/<project>/files` | The files of one project |
| `#/settings` | Appearance, notifications, this device |

- Back in a top row returns to the screen the reader came from, as the browser's back does: each history entry of the page records its depth and the route before it. A screen opened from outside the page (a link or a notification) goes back to its parent: a session to the inbox, a project to the projects (to Home with `task_view`), files to their project. A task opened from outside puts its project under it in the history, so Back closes the sheet first and then leaves the chat. Switching Chat and Terminal replaces the history entry, so it adds no step to Back.
- A phone shows Inbox, Projects and Settings over a bottom tab bar; with `task_view` it shows Home and Settings, and the inbox is *All sessions* at the bottom of Home and in Settings. A session owns the full height and has no tab bar. At 900 px and wider the inbox, the project list or Home is a 300 px left column and the bar is gone.
- A session row reads by work: the first line is the project, `›` and the worktree (branch), the second the session's label and provider, then a working agent's activity. Each level has one look, as in the terminal title (`docs/terminal-context.md`): the project in medium weight, the worktree in regular weight after a faint `›`, the session smaller and muted. A row under a heading that already names the project (the inbox's project groups) or the worktree (a project's screen) leaves that level out. The state is a word on the right; the mark and the color repeat it. A shell says `shell` where an agent's provider is, and `open` or `exited` as its state.
- Order is stable: a row's place in a section never changes because it was seen. *Finished* is sorted by `state_since_unix_ms`, newest first, then by session id; the other sections keep wsxd's session order. A row moves only when its session changes state.
- The `workspace` event gives each session its `worktree` id, `state_since_unix_ms` and, for a working agent, `activity`. `state_since_unix_ms` is when wsxd saw the agent enter its state (`AgentInfo::state_since_unix_ms`, daemon revision 28), so a restart of the web server does not change it; it is `null` for a session without an agent or with an older wsxd, and the page then shows no time. "new" is per device: the browser keeps the `state_since_unix_ms` it last saw for a session (`localStorage`) and sets it when that session's chat opens.
- When `wsxd` is unavailable the page says so, drops the session list it had, and recovers by itself.
- `index.html` shows "Loading…" in `#app` until the first answer. The page removes it before it starts the app: Preact adopts an existing child only of the same element type, so the placeholder would otherwise stay beside the app. Only the `api/session` request chooses between the app, pairing and the error page.
- Chat: one scroll container between the top row and the composer. New items do not move a reader who is more than 80 px from the end; a "Jump to latest" button appears instead. Tool and thinking items between two messages fold into one line. The working row has the activity, the elapsed time and Stop; the step that runs now is shown there only, and the list above it holds finished steps. While the agent is blocked, a row with "Open terminal" stands in place of the composer. On a wide screen the conversation, the waiting messages and the composer share one centered column of `size-prose`. The end of the conversation keeps four times the gutter (64 px in the default theme) above the composer; the working row ends the log, so the gap follows it, and the jump button sits inside the gap. On a phone (below the 900 px wide layout) the composer is one rounded card inset by the gutter, like the Claude iOS app: a one-line field that grows to six lines, then a short row inside the card with attach at the left and a round Send at the right. The card has one 8 px inset on every side: the field's text, the paperclip, and the circle's right and bottom gaps. The row draws 36 px, and every control's hit area is 44 by 44, reaching past the inset; a tap anywhere else on the card puts the caret in the field. The empty composer is 84 px high at 390 px. Folded step rows, Stop and the other short text actions also get 44 px hit areas without drawing larger. A wide window keeps one line with a text Send. While the composer has the focus, every other form field on the page is out of the tab order, so iOS's previous and next arrows above the keyboard have nowhere to go. The bar itself, with its Done check, belongs to WebKit and stays (https://bugs.webkit.org/show_bug.cgi?id=251787). Enter sends on a hardware keyboard and is a newline on a touch device; nothing is sent during IME composition. A draft, its attachments and a pending message survive a route change; the draft and a message that was not delivered also survive a reload.

## Themes

`theme` selects a built-in theme or a deployer-supplied theme directory: validated tokens, optional component CSS, fonts and icons, served under content-hash URLs. The manifest colors and icons follow the theme. See [web-themes.md](web-themes.md).

## Chat

- Sending uses the exchange broker (`docs/agent-orchestration.md`): one exchange per pane, continued round by round. For an idle or done Claude the draft-stash variant is used, so the owner's unsent input is not merged into the message.
- The page shows the broker's evidence in plain words. `pty_delivery` is "typed into the agent's terminal; the agent has not confirmed it". Only `request_bound` is shown as confirmed.
- The transcript is the provider's own saved conversation (`wsx_core::integration::memory`). Terminal output is never read into chat. A sent message that is not in the saved conversation yet is shown as pending.
- A message wsxd could not deliver says so on the pending message, with wsxd's reason (the exchange's `failure`, for example `editor_stash_unconfirmed`), and **Retry** sends it again. A waiting message whose delivery fails stays in the queue, failed with that reason; it is never dropped.

### Timeline

`GET api/chat/<session>/timeline` answers `{session, status, truncated, items, from, exchange, queue, working}`. With `?from=<item id>&at=<its at_unix_ms>` the items start at that item when it is still in the timeline with the same time, and `from` names it; otherwise `from` is `null` and `items` is the whole timeline. The page asks from its last user message: items before the start of that turn do not change. On a 200 KB conversation this cut each read from about 215 KB to 16 KB.

| Item field | Content |
|---|---|
| `id` | stable between reads: the record id, or the tool call id |
| `kind` | `user`, `assistant`, `tool`, `thinking`, `summary` |
| `at_unix_ms` | the record's own time, when it has one |
| `text`, `attachments` | for `user` and `assistant` items; `text` for a `summary` |
| `tool` | `{name, title, status}` for a `tool` item; `status` is `running`, `ok` or `error` |

- One parser, two projections: `Reader::read` (messages, used by `wsx agent context`) and `Reader::read_timeline` come from the same parsed records. Bounds: the last 1 MiB of the file, 4096 records, then the newest 400 items and 256 KiB.
- `title` is one line from the call's input: its `description`, else `command`, else `file_path` or `path`, else the tool name, at most 120 characters. The input itself, the tool result and the text of a thinking part are never sent. A `thinking` item says only that thinking happened.
- Records a provider writes in the person's name are left out: Claude `isMeta` records, its "[Request interrupted by user" notice, a command's output (`<local-command-stdout>`, `<local-command-stderr>`), and (Claude Code 2.1.300) records with `promptSource: "system"`, such as a `<task-notification>` when a background task ends. Claude's next answer says what the task did, so the page shows no line for it. The person's own prompts are `typed`, `suggestion_accepted`, `queued` or `sdk`; a record without `promptSource` (older Claude) is the person's unless one of the other rules applies. A slash command that Claude runs itself, such as `/compact`, is saved as `<command-name>`, `<command-message>` and `<command-args>` tags; the timeline shows it as typed (`/compact`, or `/name args`). Also seen on 2026-10-15, unchanged: a message from another session is `system` and `isMeta`; `!` shell input and output (`<bash-input>`, `<bash-stdout>`) have no `promptSource` and show as the person's messages.
- A call with no result is `running` only in the last turn (after the newest message a person wrote). An older call without a result is `ok`.
- Records are untrusted: ids and names are clipped and control characters removed.
- Claude 2.1.300 keeps the newest records through a compaction: its `compact_boundary` record names them in `compactMetadata.preservedSegment` (`headUuid` to `tailUuid`, after the summary `anchorUuid`). The timeline shows them right after the summary, as Claude's model sees them. On 2026-10-15 a `/compact` kept the last answer.
- Compaction: the newest summary is one `summary` item where the compaction happened (Pi keeps the earlier records in the same file; Claude starts a new chain, so its summary is first). A summary older than the kept items is the first item. It keeps up to half the byte budget. Claude's fixed words around it ("This session is being continued from a previous conversation … Summary:" and the closing "If you need specific details from before compaction …" instructions, which name a local transcript path) are removed. Pi's summary starts with its content; a Codex `compacted` record seen on 2026-10-15 had no text.
- The page shows a summary as "Summary of the earlier conversation" with a line at its start, open for about eight lines; "Show full summary" opens the rest.
- Formats read (checked against real sessions on 2026-10-12): Claude `tool_use`, `tool_result`, `thinking` parts; Pi and omp `toolCall`, `thinking` parts and `toolResult` messages; Codex `function_call`, `custom_tool_call`, their `_output` items and `reasoning`. A Codex result has no error flag, so a Codex call is never `error`.
- `working` is `null`, or `{since_unix_ms, activity}`. It is set only while wsxd reports the agent as working; the transcript never decides that. `activity` is the title of the running tool, else "Thinking", else "Working". `since_unix_ms` is when wsxd saw the state begin, or `null` with an older wsxd.
- The `history` event is the change signal. While an agent works and a page is open, the server compares transcript stamps about every 0.8 seconds, so a new tool line shows within about a second.
- A blocked agent shows a provider dialog. The chat refuses to send a message into it. With `chat_answer` the page shows the dialog and sends keys ("Answering"); without it the answer is given in the terminal panel or on the host.

### Markdown

An agent's message and the summary of an earlier conversation are rendered as CommonMark with GFM tables, strikethrough, task-list marks and autolinks. The person's own messages and waiting messages stay plain text.

- `web/markdown.js` asks markdown-it only for its token list (`html: false`) and builds the page's nodes from it. No HTML string made from a message reaches the DOM, so no sanitizer library is needed: raw HTML in a message is shown as text, and an attribute or a tag can only come from this file.
- A link is made only for `http`, `https` and `mailto`, and opens in a new tab with `rel="noopener noreferrer"`. Any other scheme and a relative address stay text. Text becomes a link by itself only when it has a scheme, so `main.rs` is not a link.
- A message cannot load a remote resource. An image is shown only when its file name is one of the files the server already granted for that message ("Media"); any other image is its alt text.
- Prose keeps its reading width. A table, a code block and a diagram may be as wide as the conversation minus the gutter (16 px each side in the default theme), centered on the reading column; a narrower one keeps the column's width. The page reads the conversation's width as a container size (`container-type` on `.log-wrap`), so no script measures it. Content wider than that scrolls inside its own block: a code block sideways with a Copy action, a table both ways with its header row in view. Nothing widens the page.
- Code is colored by highlight.js for rust, typescript, javascript, python, bash, json, toml, yaml, diff, html, css, sql and go. The page reads the highlighter's token tree and uses the theme's role colors (accent, attention, muted, danger), so no color is added to the theme contract. Blocks above 40,000 characters stay uncolored.
- A message is parsed once per text. A timeline refresh that brings the same text reuses the nodes. Measured in headless Chrome with the processor slowed four times: a 204 KB message with 145 code blocks takes about 0.2 seconds.
- markdown-it 14.3.2 (125,451 bytes, 44,975 gzip, MIT) and highlight.js 11.11.1 with those 13 grammars (71,996 bytes, 22,705 gzip, BSD-3-Clause) are vendored by `scripts/vendor-markdown.sh` with pinned SHA-512 values. markdown-it is the published browser file, unchanged.

### Diagrams

A fenced block with the language `mermaid` is drawn as a diagram (`web/diagram.js`).

- Mermaid 11.17.2 is vendored by `scripts/vendor-mermaid.sh` (pinned SHA-512, MIT). It is 3,572,508 bytes, stored and served gzip-compressed as `assets/mermaid-11.17.2.js` (975,905 bytes, `Cache-Control: immutable`). A request that does not accept gzip answers 406.
- The page imports it when the first diagram appears. It is not in the service worker's shell list, so installing the page does not download it. The service worker keeps it after the first use, so a diagram also draws offline from then on.
- The CSP is unchanged. Mermaid must draw into the page to measure text, and it writes inline styles, which the CSP refuses. Three things replace that: the vendoring step moves the diagram's CSS from a `<style>` element to a constructed style sheet that the page adopts while drawing; while drawing, a `style` attribute that Mermaid sets goes through the CSSOM; and the vendoring step leaves out Mermaid's last DOMPurify pass over the finished SVG, because that pass parses every style attribute and the CSP reports each one.
- The finished SVG is never inserted as markup. It becomes an image from a `blob:` URL, which `img-src` already allows. An SVG image runs no script, follows no link and makes no request, which is what the left-out pass protected. Mermaid runs with `securityLevel: "strict"` and `htmlLabels: false`; labels are still sanitized by Mermaid.
- Colors and the font are the theme's tokens, read when the diagram is drawn. A diagram is drawn again when the device changes the scheme. Up to 40 drawn diagrams are kept by scheme and source, so a timeline refresh does not draw again.
- The whole diagram shows first: it is scaled down to the width of its box (the conversation minus the gutter) with its shape kept, centered, and never scaled up. Only when that would make its text smaller than 10 px does it stay larger and scroll sideways in its own block, with a fade on the side that holds more. A tap opens it full screen: drag to move, pinch or the two buttons to zoom, Escape or Close to leave. The page itself does not zoom.
- A block that does not parse, or a source above 20,000 characters, shows its source as a code block with the line "Diagram could not be drawn".

### Answering

wsxd knows that an agent is blocked, not what it asks. The question is the dialog the agent drew on its terminal. With `chat_answer`, the page shows that dialog and sends the keys the person chooses. No provider dialog is modelled: this works for any agent that wsxd reports as blocked.

- The timeline's `question` is `null` unless the agent is attached and blocked and the site has `chat_answer`. Else it is `{screen, stamp, options, request}`.
  - `screen`: the pane's screen up to its last written row, as plain text: at most 24 rows and 4 KiB, without control characters. It is read with the daemon's `view` request, like `wsx session peek`. `src/question.rs` is the only code of the web server that reads a screen.
  - `options`: `[{key, label}]`, one generic rule: a row that starts, after a box edge and an optional cursor mark, with `1.` to `9.` and a label. Only the last list that counts up from 1 is taken, so a numbered list in the agent's earlier text is not offered. Empty when no such list is on the screen.
  - `request`: `{name, title}` of the tool call that waits, when the saved conversation ends with one. The title is the same line as a step's. For a tool that asks a question (Claude's `AskUserQuestion`) it is the question text.
  - `stamp`: a hash of `screen`.
- `POST api/chat/<session>/answer` takes `{stamp, key}`. `key` is `1` to `9`, `enter`, `escape`, `up`, `down`, `tab`, or `text` with a `text` field of at most 2000 characters (typed, then Enter, without control characters).
- The server refuses unless the site has `chat_answer` (403), the agent is still blocked (409 `not_blocked`), and the screen read at that moment has the same stamp (409 `question_changed`; the page reads the question again). It takes the pane's terminal lease with a takeover for the one write, so a wsx window or a terminal panel that holds the pane loses it and says that control moved, like Stop. The lease is taken before the stamp is checked, so nobody types between the check and the write. Answers share the message rate limit.
- Named keys go through the daemon's key request, so they are encoded for the pane's keyboard mode. Text goes as one write.
- While a page is open, the server reads the stamp of each blocked session on every poll (about every 0.8 seconds) and raises `history` when it changes, so a dialog answered or moved elsewhere shows on the page.
- The page puts a question sheet where the composer was: the request title, the screen rows in the terminal font (the block scrolls; box drawing never wraps), one button per choice, then Enter, Esc, the arrows, "Type an answer" and "Open terminal". No choice is preselected or filled. After an answer the sheet says "Sent" and takes no further answer until the screen or the state changes.
- Limits: a choice is a key press in the agent's dialog, so it means what that dialog means at that moment; the stamp binds it to the screen that was read. wsxd reports "blocked" for Claude (permission dialogs and `AskUserQuestion`) and for agents whose integration reports it. Codex is not reported as blocked, so its dialogs do not reach the chat.

### Sessions without routine prompts

`agent_command` is the whole command line of a session started from the page; every argument is passed on. The intended shape for Claude is:

```toml
agent_command = ["claude", "--permission-mode", "auto"]
```

In auto mode a second model, the classifier, reviews Claude's actions instead of the person, so routine edits and commands run without a dialog. The prompts that remain, such as a question to the person or an action the classifier does not decide, arrive in the chat through "Answering". The flag and the mode name were checked against `claude --help` of Claude Code 2.1.296. Claude starts in manual mode instead when auto mode is not available to the account.

### Commands

Typing `/` at the start of the composer opens the agent's commands above the field. A note about the list (no match, only built-in commands, wsx too old to run a command) is one muted line at the end of the list's own box. On a phone each command is two lines, its name and source, then its description on one muted line; when less than 560 px is visible (the keyboard is up, or a short screen) the list keeps to 3.5 rows so part of the conversation stays in view. Typing filters it: names that start with the text first, then names that contain it. The arrows move, Tab or Enter picks (on a touch screen a tap), Escape closes it until the text changes. A pick fills the field with `/name ` and sends nothing. While the field starts with a listed `/name`, that name shows as one token in the accent color (an overlay behind the transparent field, so typing, IME, paste and the caret are the field's own). Backspace right after it removes the whole `/name`; the rest stays plain text. The text the agent gets is unchanged.

- `GET api/chat/<session>/commands` answers `{provider, listed, commands, truncated, skipped, runs}`; each command is `{name, description, source}` with `source` `builtin`, `project`, `user` or `plugin`. It needs `chat_send` (403 without it).
- Built-ins, from each provider's own documentation: Claude `compact`, `clear` (Claude Code commands reference; `/new` and `/reset` are aliases of `/clear`); Codex `compact`, `new` (Codex CLI slash commands); Pi `compact`, `new` (Pi 1.1.0 `docs/slash-commands.md`). Other providers have none listed.
- Claude (`listed: true`) adds skills (`skills/<name>/SKILL.md`) and commands (`commands/<name>.md`, one folder level as `folder:name`) from the session worktree's `.claude/`, from `$CLAUDE_CONFIG_DIR` or `~/.claude/`, and from each enabled plugin (`plugins/installed_plugins.json`, `enabledPlugins` in the settings) as `plugin:name`. The project's command wins over a user's of the same name. Other providers answer `listed: false`, and the list says that only built-ins are shown.
- Only the name and one line of description leave the server: frontmatter `name` and `description` (a folded block joined), else the first line of the text, folded to at most 160 characters. Bounds: 200 commands (`truncated`), 512 entries per folder, 64 plugins, the first 8 KiB of a file, 256 KiB of a settings file, names of at most 64 bytes of letters, digits, `-`, `_`, `.` and `:`. A file with unterminated frontmatter, invalid UTF-8 or an invalid name is counted in `skipped`.
- Sending: a message without attachments whose one line starts with a listed `/name` is sent with `command: true`, so wsxd types it as written ([agent-orchestration.md](agent-orchestration.md) "Commands"). Any other text, such as `/usr/bin is the folder` or a command with a second line, keeps the exchange line. Against a daemon without `exchange_command` the server answers 409 `daemon_upgrade_required` (`runs: false`, and the list says so). A command sent while the agent works waits in the queue like any message.

### Queue and Stop

wsxd allows one active exchange per pane. A message sent while the agent works, or while older messages wait, is kept by the web server instead of failing.

- At most 5 messages wait per session. They are saved in `queue.json` in the site state directory (owner-only) and survive a restart of the server. A sixth message answers `queue_full`.
- The event observer delivers the oldest waiting message that did not fail, when the agent is attached and neither working nor blocked and wsxd accepts it.
- "Not yet": `agent_busy`, `exchange_busy`, `editor_unavailable` (Claude draws its editor idle a moment after the turn ends), `daemon_unavailable` and `daemon_replacing`. The message stays and is tried again at least every 800 ms. After 30 s of "not yet" it fails with that reason.
- Any other refusal, for example `write_claim_conflict` while another writer works in the worktree or an attachment that expired, and a delivery that fails (`delivery_failed`), mark the message failed with the reason. A failed message stays in its place with **Retry** (`POST api/chat/<session>/queue/<id>/retry`) and **Remove** (`DELETE api/chat/<session>/queue/<id>`), but it does not hold back the messages after it, and a new message to an idle agent is sent at once. Order rule: waiting messages go oldest first, past any failed one; a retried message goes next in its old place.
- The page shows a failed message above the composer, its reason in words on a line of its own in the danger color.
- `POST api/chat/<session>/send` answers the exchange when it delivered, or `{"queued": {...}}`. The queue is the `queue` field of the timeline, and every change raises the `history` event.
- A queue belongs to one site. Another site's queue for the same session is not visible.
- Stop (`POST api/chat/<session>/cancel`) asks wsxd to interrupt the turn ([agent-orchestration.md](agent-orchestration.md) "Cancel"). Like a send, it takes the pane from a terminal that holds it ([agent-orchestration.md](agent-orchestration.md) "Terminal control"). After a stop, waiting messages are delivered as usual; a cancelled exchange is not continued, the next message starts a new one.

## Build

- The build id is the digest of the page files (the index, the manifest and every module). The page loads its modules from `assets/<build>/`, and they import each other relatively, so a browser or proxy cache never serves an old module beside a new one. Those files are cached for good. Under another build id a module answers 404 `old_build`; the pinned fonts and the diagram library are served under any build. `assets/<name>` still answers for a page that was opened before the build path existed.
- `api/session` sends `build`: `version` (wsx), `commit` (the git commit the binary was built from, or `null` for a build without git), `id` and `started_unix`. Settings shows them under "Version". The commit is `HEAD` when the binary was built; changes not committed then are not in it, but they change the page files' id.
- The `workspace` event carries `build`, and a new event stream gets it first. When it differs from the page's own build, the page shows "A new version of this page is ready" with **Reload**. It never reloads by itself, so nothing typed is lost; a draft is also kept on the device. Reload asks the service worker for the new files, waits at most 3 s, and reloads.

## Live updates

A page opens one `EventSource` (`api/events`) and has no polling timer. Events: `workspace` (projects and session list), `history` (a session's saved conversation changed), `exchange`, `daemon`, `resync`, `revoked`, and with `task_view` `tasks`, `task` and `manager` ("Tasks"). The server follows `wsxd` with its long-poll and states the protocol it reconnects at, so a running web server does not defer a daemon upgrade (`docs/daemon-live-upgrade.md`). While a page is connected it also compares each transcript file's size and modification time about every two seconds.

While an agent writes, `history` arrives about once a second. A chat page reads its timeline once per change, from its last turn ("Timeline"), and runs one read at a time: changes during a read cause one more read after it. Measured by the browser check on a 200 KB conversation at 390 px with the processor slowed four times, during a 21 s turn: 26 `history` and 2 `exchange` events, 29 reads of 16 KB each (215 KB before), 43 ms in the server per read, 28 ms of main-thread work per read.

## Media

- Before an upload the page makes a PNG, JPEG, WebP or HEIC image smaller with a canvas: the long edge is at most 2000 px, as JPEG, or as PNG when that is smaller (screenshots). The original is sent when that fails or is not smaller. Each file shows as "Uploading…" at once, and Send waits until all are in. The draft's text, its uploaded files and a message that was not delivered are kept in this browser's storage, so a reload keeps them.
- Upload: one file per `PUT api/media?name=…` request body. The type comes from the content signature, never from the name: PNG, JPEG, GIF, WebP, PDF, MP4, WebM, MP3, WAV, or valid UTF-8 text up to 1 MiB.
- A sent attachment is added to the prompt as the stored file's absolute path, so the agent can read it.
- Agent-produced files: the server looks for file paths in assistant messages of the saved conversation, opens them inside that session's worktree without following symlinks, and copies allowed types that are newer than the TTL and within the size limit. Hidden paths (`.env`, `.git/`), and names that look like credentials (`*secret*`, `*token*`, `*.pem`, `*.key`, `id_rsa*` and similar) are never copied. For other files this gives a chat-only site the same reach as asking the agent to print the file. Images are shown inline.
- Store: `media/<sha256>.<ext>`, URL `media/<sha256>.<ext>`. Responses are `Cache-Control: private, max-age=<seconds left>, immutable`. An entry is removed when its TTL ends, or oldest first when a new file would exceed the quota. Uploading the same bytes again restarts that entry's TTL. Partly received uploads are not counted against the quota. Single-range requests are supported.

## Tasks

The web side of manager mode ([tasks.md](tasks.md)). The server reads the task store and talks to the task hosts and the manager through `wsx_core::task` (`crates/wsx-web/src/tasks.rs`); the browser only names a project id, a task number and its text. Every route checks the chat panel, the capability and the grant; a project outside the grant, an unknown task and an unknown action answer 404.

| Route | Needs | Answer |
|---|---|---|
| `GET api/tasks` | `task_view` | the overview of every granted project |
| `GET api/tasks/<project>` | `task_view` | overview: `needs`, `working`, `stopped`, `tasks` (chips and the `#` picker, at most 200), `decisions` (last 5), `resolutions`, `last_reply` (the manager's last reply, at most 120 characters, or `null`), `last_unix_ms` (the latest task change or manager entry, or `null`) |
| `GET api/tasks/<project>/t/<n>` | `task_view` | record, `result`, `denials`, `checks`, `report` (`report.md`), `timeline`, `overlaps` with paths, `verification`, pending `approvals`, `actions`, `landed_commit`, `deploy` (`configured`, and whether the last deploy has the landed commit, `deployed`, and its `behind`) |
| `GET api/tasks/<project>/chat?after=<id>` | `task_view` | manager messages, replies, rotations and stops after `after` (at most 200), `running`, `busy`, `activity` |
| `GET api/tasks/<project>/brief` | `task_view` | the project's ledger ([tasks.md](tasks.md) "Ledger"), for the project brief |
| `GET api/tasks/<project>/since?after=<ms>` | `task_view` | task numbers that `started`, `landed`, `blocked`, `conflict` or were `revised` after that time, and the count of `replies` |
| `POST api/tasks/<project>/say` `{text}` | `task_act` | `{ok, id}`; starts the manager when it does not run |
| `POST api/tasks/<project>/t/<n>/<action>` | `task_act` | `add {text}`, `steer {text}`, `allow {tool}`, `verify {result, note}`, `land {}`; the host's `{ok, state, message}` |
| `POST api/tasks/<project>/approvals/<id>/approve` | `task_act` | 202 at once; the action runs as `wsx task ...` without `WSX_TASK_ACTOR`, and its outcome is the decision in the next overview |
| `POST api/tasks/<project>/approvals/<id>/deny` `{note}` | `task_act` | `{ok}` |

- `needs` is in this order: `blocked`, `conflict` and `failed`, `approval`, `verify` (ready with Human Verify items and no pass on the revision), `land` (ready otherwise) and `review` (in review). Within a kind the oldest comes first. A landed task is not a need. A blocked item has the worker's `denials`. An approval item has `label`, the action it asks for in words (`Approve land #3`); the task's pending `approvals` have it too.
- `resolutions` is what was settled, newest first, at most 30, each `{unix_ms, task, text}`: from the logs of the 30 tasks changed most recently within 7 days ("#1 got an answer" for a message after a block, "#4 allowed Bash(make:*)", "#2 passed verification of r1", "#2 failed verification of r1", "#3 landed", "#5 was closed", "#5 was closed and discarded") and from the decisions in `approvals.jsonl` ("Approved land #3", "Denied discard #4").
- `actions` lists the buttons a state has: working `steer add`, blocked `add allow`, ready `verify add` while the revision has Human Verify items and no pass, else `land add`, failed, conflict and stopped `add`, in review `land add`; none without `task_act`. The host still decides: its refusal is a 409 with `refused` and its reason, and a host that does not answer is a 409 `host_not_running`.
- Limits: a message or note is at most 64 KiB, a tool 256 bytes without control characters, a fail needs a note. Actions share the message rate limit. A landing waits for the host for at most 300 s. A second approve of a request whose action still runs is a 409.
- The timeline is the task log with each run of agent progress lines folded into one `steps` entry (count and last line); the last 300 entries.
- The manager's session is created with the server's own daemon client, which never starts wsxd.
- Events: the observer stats each granted project's `task.json`, `log.jsonl`, `result.json` and `report.md` files, `manager/log.jsonl` and `approvals.jsonl` on every poll. A change sends `tasks` (the project's overview, only when it changed), `task {project, task}` for each task whose files changed, and `manager {project}`.

### Home

`crates/wsx-web/web/home.js`, `#/` with `task_view`. One row per granted project: the name, a count of its needs, one status line, "N working" while tasks run, and the time since its last activity: the newer of `last_unix_ms` and the newest `state_since_unix_ms` of its listed agent sessions (task workers and the manager are not listed). The status line is the most urgent need in words ("#1 needs an answer", "Approve land #3", "#2 is ready to verify"); else its agent sessions when one needs input, ended in an error, exited or works ("claude in main exited", "2 working"); else the manager's `last_reply`; else its other agent sessions ("claude in main done 5m ago", "2 idle"); else "Nothing needs you", or "No tasks yet" when the project has neither a task nor an agent session ("Loading…" until its tasks are read). One session is named by provider and branch, more are counted. Projects with needs come first, by their most urgent kind in the `needs` order and then by their oldest need of that kind; the rest follow by their last activity, newest first. A row opens the project screen. *All sessions* under the rows opens `#/sessions`.

### Project screen

`crates/wsx-web/web/tasks.js`. From the top: the top row (back, the project, the manager's state in words: idle, thinking, not started; `⋯` with *Sessions*, which opens `#/p/<project>/sessions`, and *Files* where granted), the project brief, the manager chat with its cards, the tray and the composer.

- Each need is a card in the conversation at the time it arose (its `unix_ms`), with the item's mark, `#n` and the title (an approval reads as the action it asks for, "Approve land #3", "Approve discard #4", "Approve deploy", never as its id), and the kind in words with the worker's summary; the title opens the task sheet. Its buttons are the fixed actions, none filled: *Allow* (a field with the first denied tool, editable) and *Answer* (a field; sent as an add) for a blocked task, *Deny* and *Approve* for an approval, *Reply* (a field; sent as an add, a talk-through turn) and *Start* for a brief ("Brief ready #n" in the tray and on Home, ranked with approvals; its context is the rewritten prompt), *Land* for a ready task, *Check* for one in review, and *Verify*, which opens the task sheet with its Human Verify items. An action's outcome is a notice; a failure stays under its card with the draft kept.
- A settled item is one muted line from `resolutions`, "#4 allowed Bash(make:*)" or "Approved land #3", whose `#n` opens the task. A card settled while the page is open turns into its line in the same place; other lines stand at their own time.
- The tray above the composer lists `needs` in the server's order, one line each with the kind in words and the title: two lines and then "+N", one line with the keyboard up or less than 560 px visible. "+N" unfolds the rest; the tray is at most 40% of the chat and scrolls inside itself. A line scrolls its card into view and marks it for a moment; a need older than the loaded history has no card, so its line opens the item under the tray. With the keyboard up the folded brief is hidden, and the tray is hidden while the `#` or `/` list is open.
- The chat reads `manager` events from its last entry. An owner message is a block on the right, an events message (written by wsx) is muted lines, a reply is markdown. Every `#<n>` in them is a chip with the task's live state and title that opens the task sheet. A sent message shows until the log has it; while the manager thinks, a working row has its last step.
- `#` at the start of a word in the composer opens the task picker in the box of the `/` list: the open tasks in attention order, then the rest, each with its number, title and state; digits or words filter it. Arrows, Tab or Enter (a tap on a touch screen) pick, Escape closes; a pick puts `#n ` in the field and sends nothing. Enter sends on a hardware keyboard; the draft is kept on the device per project.

### Task sheet

`#/p/<project>/t/<task>` opens the task over the chat in a modal `<dialog>` (`TaskSheet` in `tasks.js`): 85% of the visible height from the bottom with a drag handle on a phone, a panel of twice the rail width at the right from 900 px. Back in its top row, the browser's Back, Escape, a tap on the backdrop and a swipe down of more than 80 px on the handle or the top row close it. The composer keeps its draft and gets the focus back. The sheet's content is `crates/wsx-web/web/task.js`. The top row has back ("Back to the chat"), `#n`, the title, the state with the revision (before the build, with the brief version), and *Session*, the worker's terminal, while the worker has a session. Then, in one scroll container:

1. The actions the state allows: *Review* for a task to verify, which scrolls to its Human Verify items; a pending approval of this task with *Deny* and *Approve*; *Start the build* for a brief; for a blocked task the denied tool calls and *Allow a tool* (a field with the first denied call, editable); *Land r<k>* for a ready task, *Check pull request* in review.
- Before the build (`phase: brief`), the brief's sections from `brief.json` come next, in the order of `docs/tasks.md` "Brief", empty ones left out, each concept folded to its term; then the talk-through, each owner reply ("You") and the worker's answer ("Worker, brief v<k>") in order; the message field reads "Reply to the brief". After the start the talk-through stays above the report and the brief is folded as "Brief v<k>" under it.
2. The report (`report.md`) in its sections: Asked, Summary, Decisions, Changes, Evidence, Human Verify, Left open. Under Human Verify a ready task with items has *Fail* and *Pass r<k>*; *Fail* opens a note (required; it goes to the worker), and a fail without one is not sent.
3. Shared files: each other open task that changed a path this one changed, with up to 20 of the paths.
4. The timeline, folded: one line per log entry with its time and what happened in words; agent progress is one line per run of steps.

A message field stays at the bottom while the state takes one: *Send* is an add (the answer to a blocked task), and while the worker runs *Steer now* sends into the turn and *Add* after it. An action's outcome is a notice; a failure is shown above the field and keeps the draft. The sheet reads again on a `task` event for it and when the project's approvals change.

### Project brief and task story

`crates/wsx-web/web/story.js`. Plain words written by wsx from the ledger, the task record, its result and its log; no model call.

- **Project brief**, the top of the project screen, folded to one line: "3 open · 1 needs you · main 2 ahead of deploy" (the open count, the needs-you count, and with a deploy command: never deployed, up to date, or how many commits the default branch is ahead, and "last deploy failed"). A tap unfolds: each open task in one line with its state in words (waits for you, brief ready, ready to verify, ready to land, failed, conflict while landing, running checks, in review, stopped, working), the last 3 landed, the deploy and release with their commit and how far behind, and the main checkout's branch and uncommitted files. The brief reads `brief` on opening and again at most every 2 s while the overview changes.
- **Task story**, the top of the task sheet: "Asked:" and the first line of the prompt; the steps `Asked → Brief → Working → Checks → Ready → Verified → Landed → Deployed` with the current one marked (Brief is left out for a task built without a brief), where it stopped named on that step (blocked, stopped, failed or closed on Brief before the build; blocked, stopped or failed on Working, failed checks on Checks, conflict or in review on Landed), and Verified (no Human Verify items) or Deployed (no deploy command) struck out when they do not apply; and one sentence "What happens next" for the state, such as "The worker waits for you to allow Bash(make:*) or answer.", "Check the 2 items below, then Pass to allow landing." or "Landed on main; the last deploy does not have it yet". A pending approval of the task comes first: "The manager asks: Approve land #3. Approve or Deny it above."
- *Explain* (with `task_act`), in the unfolded brief and in the story, sends a fixed owner message to the manager: "Explain where the project stands in plain words" or "Explain #n in plain words: why, what changed, risks". The task sheet then closes, and the answer comes in the chat. A failure stays beside the button.

### All sessions

With `task_view` the inbox is `#/sessions`, titled *All sessions* with Back to Home, and lists no task workers or manager. The Projects list adds "N need you" after a project's session count.

### Task notifications

A device that picks "A task needs me" in Settings (topic `tasks`, offered with `task_view`) gets two kinds of Web Push, decided by the event observer from the task records and `approvals.jsonl`:

| Change | When | Text |
|---|---|---|
| a task becomes `blocked`, `conflict` or `briefed`, a new approval request | once `merge_seconds` passed since the first of them; later ones in that window go in the same push | `#4 blocked · #6 brief ready · Approve land #5` |
| a task becomes `ready`, `failed` or `landed` | one digest once `digest_quiet_seconds` passed without another, at most `digest_max_seconds` after the first | `2 ready (#5, #6) · 1 landed (#3)` |

- Batches are per project; the title is `<site>: <project>`. A push about one task opens its task sheet, any other its project. A change is counted by its (state, revision); a project first seen after a server start notifies nothing, and working, checking and stopped never notify.
- No push goes to a device while one of its pages shows that project (any `#/p/<project>` route) and is visible. The page opens its event stream as `api/events?page=<id>` with an id it chose, and `POST api/presence {page, project}` tells the server the project it shows (`null` when hidden or elsewhere). The presence of a page ends with its last stream; a page id belongs to the device that opened it (404 for another device's or an unknown page).
- "Since you left": the device records when it left a project's screen (route change or hidden page). On opening it again, or coming back to it, more than a minute later, the screen asks `since` and shows one line, "Since you left: #2 #3 landed · #1 blocked · 2 replies", with *Dismiss*. Nothing runs while the device is away; the line is computed from the logs.
- Limits: a batch waits while `wsxd` is unavailable, because the observer runs with the daemon's long-poll. Push text is written by wsx, never by the manager.

## PWA and push

The manifest has 192 and 512 px PNG icons. The service worker caches the shell and serves it when offline; API and media requests are never cached by it. Push is off until a device picks topics in Settings and accepts the browser prompt: agent needs input (`blocked`), agent finished (`done`), agent error (`error`), and with `task_view` tasks (`tasks`, "Task notifications"). Only a state change notifies. Tapping a notification opens that session's chat (`#/s/<session>/chat`), or for `tasks` the project or task it names. A subscription endpoint must be on one of `push.service_origins` (default: FCM, Mozilla and Apple).

## Terminal

The terminal panel is xterm.js on the pane's raw byte stream. One WebSocket (`api/terminal/<session>/ws`) is bridged to one `wsxd` raw stream (`crates/wsx-web/src/terminal.rs`). Closing the socket ends the stream and releases the pane.

| Frame | Direction | Content |
|---|---|---|
| binary | server to page | PTY bytes; after a `replay` message, the screen to start from |
| binary | page to server | input bytes, at most 64 KiB per frame |
| text | server to page | `replay`, `resized`, `clipboard`, then one of `busy`, `control_lost`, `exited`, `error`, `revoked` before the socket closes |
| text | page to server | `resize` (needs `terminal_input`) |

- **One controller.** A pane has one writable lease, and the newest person's action wins it. Opening the terminal takes the pane from a wsx window; the window says that control moved. A reconnect never takes over, or two clients would take the pane back and forth: when a wsx window or a chat message takes the terminal, the page shows "Control moved to another wsx window" and attaches again only after **Take control**. A device without `terminal_input` does not take over; it shows "Controlled from another wsx window" with **Try again**.
- **Page identity.** A page sends a random `client` value that stays the same while the page is open. The server derives the lease identity from the paired device and that value. A reconnect or a reopened panel therefore replaces the page's own old stream without a takeover. Another tab is another controller.
- **Size.** *Fit screen* (default) resizes the pane to the browser. *Host size* leaves the pane as it is and scales the view. The choice is kept in the browser. A device without `terminal_input` always uses host size and cannot type.
- **Reconnect.** After a lost connection, a daemon handoff or a daemon restart the page reconnects by itself (0.5 to 5 seconds apart) and starts from a new replay. A resize also ends in a new replay, because xterm.js and `wsxd`'s emulator reflow a resized screen differently.
- **Queries.** `wsxd`'s emulator answers terminal queries. The page disables xterm.js's own answers (device attributes, status reports, mode and color queries), so a program gets one reply.
- **Touch devices** (`pointer: coarse`, or a window narrower than 700 px) get the key row `Esc Tab ^C ^V ^A D ^Z ^R ^L ⌫` (a held key repeats every 80 ms) and a second row: Paste at the left, a pad in the middle that sends one arrow key per 18 px of drag, and `^J` (a new line) and Enter at the right edge, under the thumb. `^J` and Enter do not repeat when held. A touch drag on the screen sends wheel reports when the program tracks the mouse, arrow keys on the alternate screen, and scrolls the 10,000-line local scrollback otherwise. Composition events are not touched, so Korean and other IME input works. While the terminal, or any field that takes typing, has the focus, the page is pinned to `visualViewport` (its height and `offsetTop`) and never scrolls. Otherwise it is the full screen (`100dvh`): iOS does not always report that the keyboard closed (a route change, a return from the background, a page from the back-forward cache), and a page left pinned kept half the screen empty. The page measures again on focus changes (also 400 ms after a field loses the focus, when the keyboard has closed), `visibilitychange`, `pageshow` and window `resize`. iOS Safari and an installed PWA keep the layout viewport when the keyboard opens, so this is what puts the key row's bottom edge on the keyboard's top edge. The bottom safe-area inset is dropped while the keyboard is open. The screen runs from edge to edge between the top row and the key row; buttons are `size-key` high with `space-1` gaps.
- **Frame.** The session's top row gains the size toggle as an icon button. The terminal uses the theme's dark tokens in either scheme ([web-themes.md](web-themes.md)).
- **Font and color.** 11 px on touch devices, 14 px otherwise, in the theme's `font-terminal`. The screen takes `color-bg`, `color-text`, `color-accent` (cursor) and the 16 `color-ansi-*` tokens from the dark tokens.

xterm.js 5.5.0 and its fit addon 0.10.0 are vendored in `crates/wsx-web/web/vendor/` and loaded only when a terminal opens. `scripts/vendor-xterm.sh` rebuilds them from the npm tarballs, checks their SHA-512, and applies one patch: the DOM renderer's `<style>` elements and `style` attributes become CSSOM calls, because the CSP has no `'unsafe-inline'`. License: `web/vendor/xterm.LICENSE` (MIT).

### Terminal fonts

The terminal ships its own fonts, so Powerline separators, Nerd Font icons and ligatures do not depend on what the device has installed.

| Font | Version | Files | Bytes | License |
|---|---|---|---|---|
| JetBrains Mono | 2.304 | Regular, Bold, Italic woff2 | 92,164 + 94,588 + 95,864 | SIL OFL 1.1 (`web/vendor/fonts/JetBrainsMono.OFL.txt`) |
| Symbols Nerd Font Mono | 3.4.0 | woff2, and the upstream ttf as a fallback | 1,340,674 (ttf 2,507,556) | MIT for the patcher, per-icon-set licenses (`NerdFonts.LICENSE`) |

- `scripts/vendor-fonts.sh` downloads the pinned releases, checks their SHA-256 and writes `web/vendor/fonts/`. Nerd Fonts publishes no woff2, so the script wraps the upstream ttf in a WOFF2 container with Node's built-in Brotli (tables unchanged). A browser that refuses that file takes the ttf. The symbols font is not subset.
- The files are served as `assets/<name>-<version>.<ext>` with `Cache-Control: immutable`. `terminal-fonts.css` declares them and is loaded only when a terminal opens; the service worker does not precache them. A browser downloads about 1.6 MB once, when the first terminal opens.
- Stack (`font-terminal`): JetBrains Mono, Symbols Nerd Font Mono (limited by `unicode-range` to the private-use areas and five symbols), then the system mono fonts, which keep supplying Korean and other wide characters.
- The page loads the faces before it opens xterm.js, so the cell grid is measured with the real font. When a face arrives later, the page measures again.
- Ligatures: xterm.js's DOM renderer puts each cell in its own element, so a font cannot join characters by itself. The page registers a character joiner for runs of operator characters (`[-=<>!|&:+*/.#?~^%]{2,8}` and `www`). xterm.js then draws such a run as one text element. xterm.js also sets a small letter spacing on its rows to hold the grid, and a browser drops ligatures wherever letter spacing is not zero, so `app.css` names the font features (`font-feature-settings: "calt", "liga"`) to turn them on again. The font's own substitution then applies. Cells, cursor position, selection and copied text are unchanged: `=>` is still two cells and copies as `=>`. `ligatures = false` in the site file's `[terminal]` table skips the joiner and turns the font feature off.
- `@xterm/addon-ligatures` is not used: its 0.10.0 release depends on `font-finder` and `font-ligatures`, which find and parse the font files installed on the machine. A served page cannot do that, and the shipped font is not an installed one.

Limits: while a terminal is open it holds the pane's lease. A chat message takes the pane from it, but `wsx session send-text` to that pane is refused until the terminal closes. xterm.js 5.5 does not implement the kitty keyboard protocol, so keys use legacy encoding. In host size, mouse positions are not corrected for the scale. A replay taken on the alternate screen has no primary screen behind it.

## Files

The files panel covers each granted project's main checkout only, not its other worktrees. A project's root is pinned when it is first opened (at start for listed paths). `.git` is never listed, read or written. File access uses `O_NOFOLLOW` descriptors from the pinned project root; a write replaces an existing text file only when it did not change since it was read.

## Dependencies

`axum` and `tokio` serve HTTP, the event stream and the terminal WebSocket (axum's `ws` feature, `tokio-tungstenite`). `p256`, `hkdf`, `aes-gcm` and `sha2` implement VAPID (RFC 8292) and message encryption (RFC 8291). `ureq` with rustls posts to the push service. No OpenSSL or libcurl is linked.

## Verification

```sh
cargo build --locked -p wsx -p wsx-daemon
python3 -B scripts/test-web-client.py [--part all|chat|tasks] [--browser] [--captures <dir>]
```

`--part tasks` runs only the task journey: its own isolated `wsxd` with `scripts/fake-claude-stream.py` as `claude`, a fixture store (a task blocked on a tool, one ready with Human Verify, one ready to land; in project `plans`, built with a brief, two briefed tasks: one talked through and started by the API, one for the page), three sites (`task_view` and `task_act` on every project, `task_view` on one, neither), every action through the API, the manager's chat, an approval approved and one denied, the capability and grant refusals, and task push to the local receiver: two blocked tasks merged into one push, two ready tasks in one digest, a digest sent at its cap while tasks keep finishing, no push for the project a page of the device shows while one for another project arrives, presence ending with the page's stream, and a brief sent at once. With `--browser` it also runs `scripts/web-tasks-check.mjs` on a second project with the same store, beside the first, `plans`, a project without tasks, and three with agent sessions and no task (two working, one done, one exited): Home in the expected order with its status lines, the brief journey on `plans` (the card with Reply and Start, a reply from the card, the project summary's "brief ready", the sheet with its sections, a folded concept and the talk-through at 390x844 and 390x420, a reply in the sheet, Start until ready, the folded brief and the timeline after it), the bottom bar, *All sessions* without task sessions and its link in Settings, the cards in the chat with their buttons, the tray (two lines and "+4", one line with the keyboard up, a line that scrolls to its card), the brief, `⋯` and Sessions, a reply with chips, the `#` picker (filter, pick, no match, Escape, keyboard up), Answer and Approve on cards (the approval turns into a line in place and leaves the tray), the task sheet (85% high, Escape, Back and a swipe close it with the draft kept and the focus on the composer; the report sections, Fail with and without a note, Pass, the timeline, Land, Deny, Allow), a deep link to a task (Back closes the sheet, then the chat is the first entry), and the project screen and the sheet at 390x844, 390x420, 320x568 and 1280x800, each capture with the slop and mobile audits.

The model-free journey uses an isolated `wsxd`, PTY fixture agents with their own transcripts, six served sites and `scripts/web-push-fixture.mjs`, a separate receiver that verifies the VAPID signature and decrypts the payload. It opens terminal WebSockets like a browser does: refused upgrades, bytes both ways, every key-row byte, resize, control lost and taken, reconnect, and a watch-only site. `--browser` adds `scripts/web-terminal-check.mjs` and `scripts/web-browser-check.mjs` with an installed Chrome and writes captures to `.work/web-captures/` or `--captures`. The terminal check runs at 390x844 with touch and at 1280x800, compares the browser screen with `wsxd`'s screen row by row, and checks the shipped fonts: Powerline and icon glyphs in one cell, joined operator runs (the same line captured with ligatures on and off must differ), Korean and box drawing on the grid, and a full-width line against `stty size`. The page check captures every screen and state at 320x568, 390x844 and 1280x800 with a slop audit beside each. On a phone it also runs the mobile audit of the ui-view-standards skill (`scripts/mobile-audit.js`, or the file `WSX_MOBILE_AUDIT` names) beside each capture: it fails when the page scrolls sideways, a field makes iOS zoom, or another field can take the focus from the composer, and prints the rest (content share, small targets, small text, text outside the gutter). `scripts/web-real-agent-check.py` is the opt-in real turn: one short Claude or Pi turn in an isolated `wsxd`, for the timeline and Stop. The journey also covers the answer API (question with screen, choices and waiting tool; refused without the capability, for a stale stamp, an invalid key, a held terminal and an agent that is not blocked; approve, deny, number and text reach the PTY) with a fixture agent that draws a numbered dialog. The page check renders a markdown answer with a table and code, measures a 204 KB answer with the processor slowed four times, draws two Mermaid diagrams and a broken one in both schemes, opens a diagram full screen and zooms it, shows the question sheet for a permission dialog and a four-option question, and answers one. It fails on any console error, which includes a CSP report, and the journey compares the whole CSP header. `scripts/web-real-agent-check.py --scenario answers` is the opt-in real check of answering: a real Claude permission dialog approved by its number, one denied with Escape, an `AskUserQuestion` answered by number, and a stale stamp refused (three model turns). `--scenario draft` types an unsent line into a real Claude's editor and sends a chat message: it must be delivered, and Claude must put the line back (one model turn). Each helper of a run (wsxd, `wsx web serve`, the Node checks and their Chrome, the push receiver) runs in its own process group, listed in `processes` in the run's folder under `.work/`. The run stops each group with SIGTERM, then SIGKILL, also after an error or a SIGTERM to the run. A run that was killed with SIGKILL leaves its list behind: the next run in the checkout ends those processes first (only ones in a listed group with the listed command, or whose command names that run's folder) and removes the Chrome profiles in that folder. The model-free journey does not prove a real Claude turn, a real push service, a phone, an on-screen keyboard, Safari, or Tailscale.
