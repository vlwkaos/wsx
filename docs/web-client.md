# Hosted web client

`wsx web serve [<site>]` hosts one paired web site: a remote chat with the agents of the granted projects, plus optional files and terminal panels. It is a `wsxd` client like the TUI. It never starts, stops or replaces `wsxd`. The terminal panel needs a `wsxd` that reports the capability `raw_terminal_stream` ([terminal-raw-stream.md](terminal-raw-stream.md)). Against a daemon without it the panel says that the daemon must be upgraded (`daemon_upgrade_required`); chat, files and the inbox work. Deployment commands are in [web-deploy.md](web-deploy.md).

The panels have their own contracts: the chat in [web-client-chat.md](web-client-chat.md), manager mode in [web-client-tasks.md](web-client-tasks.md), the terminal in [web-client-terminal.md](web-client-terminal.md). This file holds the site, its security, the screens and what they share.

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

[notify.manager]                          # optional; "Manager needs me" ("Notifications")
merge_seconds = 3                         # 1-60: new items this close go in one push
[notify.agents]                           # optional; "An agent needs me"
merge_seconds = 3                         # 1-60

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
| `task_view` | read a project's tasks, reports and manager chat ([web-client-tasks.md](web-client-tasks.md)) | chat |
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

A chat log (a session's and the manager's) renders its newest 60 rows, or from its first unread row when that is earlier; *Show 60 earlier* above them adds as many more, and the reader stays on the row they read. Rows that arrive while the log is shown add to its end: its top row stays, so no row goes from above the reader. The page is a set of ES modules served as written: Preact 10.29.7 and htm 3.1.1 are vendored in `web/vendor/` (`scripts/vendor-preact.sh`, SHA-512 pinned, MIT) and there is no build step. Routes are in the URL fragment.

| Route | Screen |
|---|---|
| `#/` | With `task_view`: Home, one row per project ([web-client-tasks.md](web-client-tasks.md) "Home"). Without it: the inbox, as `#/sessions` |
| `#/sessions` | Inbox ("Sessions" with `task_view`): sessions by what they need. *Needs you* (blocked, error, exited agent), *Running* (with the activity line and elapsed time), *Finished* (the newest finish first, unseen ones marked "new"), then the remaining agent sessions and the shells by project. A section with no session is not shown. Task workers and the manager are not listed either (provider `task` or `manager`, or the label `task-<n>` or `task-manager`); their work shows in the project's chat. |
| `#/projects` | Without `task_view` a tab; with it reached only by a link (the inbox's "Start one in a project"). Every granted project with its session count, *Files* and a *+* (its name is "New session in <project>") where granted. The project's name opens it: its sessions, with `task_view` too (`#/p/<project>/sessions`, not the manager) |
| `#/p/<project>` | With `task_view`: the project's manager ([web-client-tasks.md](web-client-tasks.md) "Project screen"). Without it: the sessions, as `#/p/<project>/sessions` |
| `#/p/<project>/sessions` | One project: a heading per worktree (the main checkout first) with a *+* ("New session in <branch>") for that worktree, and its sessions as in the inbox; a worktree with no session says so |
| `#/p/<project>/t/<task>` | With `task_view`: the project screen with the task sheet open ([web-client-tasks.md](web-client-tasks.md) "Task sheet") |
| `#/s/<session>/chat`, `#/s/<session>/terminal` | One session. The top row has back, the label with the state in words, and a Chat or Terminal switch with the granted panels. A shell (a session with no agent, open or exited) has no chat: it opens on the terminal, and gains the chat when an agent reports in it |
| `#/p/<project>/files` | The files of one project |
| `#/settings` | Appearance, notifications, this device |

- Back in a top row returns to the screen the reader came from, as the browser's back does: each history entry of the page records its depth and the route before it. A screen opened from outside the page (a link or a notification) goes back to its parent: a session to the inbox, a project to the projects (to Home with `task_view`), files to their project. A task opened from outside puts its project under it in the history, so Back closes the sheet first and then leaves the chat. Switching Chat and Terminal replaces the history entry, so it adds no step to Back.
- A phone shows Inbox, Projects and Settings over a bottom tab bar; with `task_view` it shows Home, Sessions (the inbox, shells too) and Settings: Home lists every project, and a project's worktrees are in its chat's `⋯` menu (*Sessions*). A session owns the full height and has no tab bar. At 900 px and wider the inbox, the project list or Home is a 300 px left column with the same tabs at its top, and the bar is gone. With `task_view` the column is Home on Home, a project's chat and a project's worktrees, and the inbox on Sessions; a session, the files and Settings keep the column they were opened from.
- A session row reads by work: the first line is the project, `›` and the worktree (branch), the second the session's label and provider, then a working agent's activity. Each level has one look, as in the terminal title (`docs/terminal-context.md`): the project in medium weight, the worktree in regular weight after a faint `›`, the session smaller and muted. A row under a heading that already names the project (the inbox's project groups) or the worktree (a project's screen) leaves that level out. The state is a word on the right; the mark and the color repeat it, and a working session's mark pulses (still under reduced motion). A shell says `shell` where an agent's provider is, and `open` or `exited` as its state.
- **Removing a session.** Where `session_start` is granted, a session row slides left to show *Remove* ("Remove session <label>"). The row is a horizontal scroll container with snapping (`.row-slide`), so the browser moves it: the list still scrolls up and down, and Tab reaches *Remove*, which slides it into view. The slide is the confirmation; a tap on *Remove* sends `DELETE api/sessions/<session>`, which closes the session as `wsx session delete` does (wsxd `session_close` at the snapshot's revision). The row goes when the workspace event says so; a failure stays as a toast. A task's worker or manager session answers 409 `task_session`: it closes with its task.
- **Whole messages.** A message, a manager bubble and a compaction summary always show whole; nothing folds. Agents are asked to fit one screen (agent-reporting.md "Output style").
- Order is stable: a row's place in a section never changes because it was seen. *Finished* is sorted by `state_since_unix_ms`, newest first, then by session id; the other sections keep wsxd's session order. A row moves only when its session changes state.
- The `workspace` event gives each session its `worktree` id, `state_since_unix_ms` and, for a working agent, `activity`. `state_since_unix_ms` is when wsxd saw the agent enter its state (`AgentInfo::state_since_unix_ms`, daemon revision 28), so a restart of the web server does not change it; it is `null` for a session without an agent or with an older wsxd, and the page then shows no time. "new" is per device: the browser keeps the `state_since_unix_ms` it last saw for a session (`localStorage`) and sets it when that session's chat opens.
- When `wsxd` is unavailable the page says so, drops the session list it had, and recovers by itself.
- `index.html` shows "Loading…" in `#app` until the first answer. The page removes it before it starts the app: Preact adopts an existing child only of the same element type, so the placeholder would otherwise stay beside the app. Only the `api/session` request chooses between the app, pairing and the error page.
- Chat: one scroll container between the top row and the composer. New items do not move a reader who is more than 80 px from the end; a "Jump to latest" button appears instead ([web-client-chat.md](web-client-chat.md) "Following the end"). Tool and thinking items between two messages fold into one line. The working row has the activity, the elapsed time and Stop; the step that runs now is shown there only, and the list above it holds finished steps. While the agent is blocked, a row with "Open terminal" stands in place of the composer. On a wide screen the conversation, the waiting messages and the composer share one centered column of `size-prose`. The end of the conversation keeps four times the gutter (64 px in the default theme) above the composer; the working row ends the log, so the gap follows it, and the jump button sits inside the gap. On a phone (below the 900 px wide layout) the composer is one rounded card inset by the gutter, like the Claude iOS app: a one-line field that grows to six lines, then a short row inside the card with attach at the left and a round Send at the right. A file pasted into the field, such as a screenshot, attaches as attach does (`pasteFiles` in `web/chat.js`, shared with the manager chat); a pasted image named `image.png` or with no name becomes `pasted-<time>.<type>`. Above the field each attached image shows as a small thumbnail and any other file as its name and size, each with a × (`Remove <name>`); `FileChips` in `web/chat.js`. A paste without files is the browser's own text paste. The card has one 8 px inset on every side: the field's text, the paperclip, and the circle's right and bottom gaps. The row draws 36 px, and every control's hit area is 44 by 44, reaching past the inset; a tap anywhere else on the card puts the caret in the field. The empty composer is 84 px high at 390 px. Folded step rows, Stop and the other short text actions also get 44 px hit areas without drawing larger. A wide window keeps one line with a text Send. While the composer has the focus, every other form field on the page is out of the tab order, so iOS's previous and next arrows above the keyboard have nowhere to go. The bar itself, with its Done check, belongs to WebKit and stays (https://bugs.webkit.org/show_bug.cgi?id=251787). Enter sends on a hardware keyboard and is a newline on a touch device; nothing is sent during IME composition. A draft and its attachments survive a route change and a reload; waiting, sent and failed messages are kept by the server ([web-client-chat.md](web-client-chat.md) "Queue and Stop").

## Themes

`theme` selects a built-in theme or a deployer-supplied theme directory: validated tokens, optional component CSS, fonts and icons, served under content-hash URLs. The manifest colors and icons follow the theme. See [web-themes.md](web-themes.md).

## Build

- The build id is the digest of the page files (the index, the manifest and every module). The page loads its modules from `assets/<build>/`, and they import each other relatively, so a browser or proxy cache never serves an old module beside a new one. Those files are cached for good. Under another build id a module answers 404 `old_build`; the pinned fonts and the diagram library are served under any build. `assets/<name>` still answers for a page that was opened before the build path existed.
- `api/session` sends `upgrade_failed`: why the upgrade of the running `wsxd` into this build rolled back, while that daemon still runs, or `null`. The server reads it when it connects to `wsxd` and every 30 s; a change comes as a `daemon` event with `available` and `upgrade_failed`.
- `api/session` sends `build`: `version` (wsx), `commit` (the git commit the binary was built from, or `null` for a build without git), `id` and `started_unix`. Settings shows them under "Version". The commit is `HEAD` when the binary was built; changes not committed then are not in it, but they change the page files' id.
- The `workspace` event carries `build`, and a new event stream gets it first. When it differs from the page's own build, a small "New version · Reload" pill shows over the top of the screen until it is tapped; nothing shows while the build is the same, and Settings keeps its Reload beside the version only then. The page never reloads by itself. A tap asks the service worker for the new files, waits at most 3 s, and reloads at the same route; the manager and task drafts are kept on the device.

## Motion

Motion shows that something arrived, changed or left; nothing moves for its own sake. It reads `--motion-duration` and `--motion-easing` ([web-themes.md](web-themes.md)). Every rule sits in `app.css` inside `@media (prefers-reduced-motion: no-preference)`, so with `reduce` the page is the static one; script-driven motion (`motion()` in `lib.js`) is off there too.

| State | Motion |
|---|---|
| Loading | One placeholder (`Loading` in `lib.js`, two shimmering bars; the words "Loading…" are for a screen reader) replaces the static line on every screen that waits for data. The content that arrives fades in. |
| Home row | Typing dots (the manager's turn runs: `busy` of `GET api/tasks/<project>`) before the status line; a pulse dot while the project has a working task or session; the status line fades in when its text changes. The `.count` badge pops in and again when the number changes; "N working" fades in. The list reorders with FLIP (`useFlip`): rows glide, new rows fade in, a row that goes fades out where it was. |
| Chat | The working bar slides in; a sent message eases from the muted pending color; Jump to latest fades in and out; "Uploading…" pulses. |
| Notices | A notice slides and fades in and out. A toast shows a thin bar that drains over its 4 s; the offline notice pulses slowly. |
| State words | A working session's word in the top row has a pulse dot; a tone change fades in color. |
| Image viewer | The viewer and each image it goes to fade in; a swiped image springs back. |

`scripts/web-tasks-check.mjs` (`motion()`) checks the animation names on the home row, count, notice, timer and placeholder, the FLIP glide and ghost, and that none of them runs under `reduce`; it captures phone and desktop frames.

## Image viewer

`viewer.js`. A tap on an image opens it full screen over the page, without a navigation: a To check capture, an image attached to a message or shown in one, and a drawn diagram. The image links stay real links (`a[data-view]`), so a link opened with a modifier key opens as a link does.

- The image fits the screen on a dark backdrop (a diagram keeps the page's scheme, which it is drawn in). The `<img>` is the original URL, so a long press reaches it.
- Zoom: pinch, a double tap on the image (2.5×, at that point, and back), ctrl+wheel, or − and + in the top bar. A drag moves a zoomed image. The page itself never zooms.
- The set is the other `data-view` links of the closest `data-gallery`: the captures of one To check item, the images of one message. A swipe at 1× or ArrowLeft and ArrowRight go to the previous and next; the top bar shows "1 of 2" with Previous and Next. A set of one image has no count and nowhere to go.
- × (Close), Escape, a tap beside the image at 1× and Back close it. The viewer pushes a history entry with the same URL when it opens and goes Back when it closes, so the phone's back gesture closes it and the page and its scroll stay where they were. The focus goes back to the link.
- Left out: download and share buttons.

`scripts/web-tasks-check.mjs` (`viewImages`) opens a To check capture (a set of one) and a chat message's images at 390 × 844 and 1280 × 800, and checks the fit, a swipe or the arrow keys, the three ways to close and that the page did not move.

## Notices

`Notices` in `ui.js`. One notice at a time lies over the screen under the top row and takes no layout, so showing one moves nothing. "Connection lost. Reconnecting…" stays until the stream is back. An outcome (a saved file, an add, a steer, a start, a landing that rebased) clears itself after 4 s. A failure the owner acts on stays, with *Dismiss*, and a later outcome does not hide it; today that is an approved action that did not run ("Land #6 did not run: failed: …"), noticed when a new decision arrives, never for decisions from before the page loaded; and a failed upgrade of the running `wsxd` into this build ("wsxd upgrade failed: <reason>; sessions keep running", `docs/daemon-live-upgrade.md` "A failed automatic upgrade"), from `api/session`'s `upgrade_failed` and the `daemon` event, said once per reason. An action whose outcome the chat already shows as a status line (approve, deny, allow, a landing) gives no notice. Errors of a form stay beside the form.

## Live updates

A page opens one `EventSource` (`api/events`) and has no polling timer. Events: `workspace` (projects and session list), `history` (a session's saved conversation changed), `exchange`, `daemon`, `resync`, `revoked`, and with `task_view` `tasks`, `task` and `manager` ([web-client-tasks.md](web-client-tasks.md)). The server follows `wsxd` with its long-poll and states the protocol it reconnects at, so a running web server does not defer a daemon upgrade (`docs/daemon-live-upgrade.md`). While a page is connected it also compares each transcript file's size and modification time about every two seconds.

### Event stream

Events that fire while the stream is down, or while iOS keeps the page in the background, are lost. So each time the stream opens, and each time the page comes back (`visibilitychange` to visible, `online`, `pageshow` from the back-forward cache), the page reads again what the events would have changed: the task overviews, the open session chat and the open manager chat, with its typing state (`busy`). A stream that gave up opens again then. A visible page with a live stream reads nothing more; no timer polls.

While an agent writes, `history` arrives about once a second. A chat page reads its timeline once per change, from its last turn ([web-client-chat.md](web-client-chat.md) "Timeline"), and runs one read at a time: changes during a read cause one more read after it. Measured by the browser check on a 200 KB conversation at 390 px with the processor slowed four times, during a 21 s turn: 26 `history` and 2 `exchange` events, 29 reads of 16 KB each (215 KB before), 43 ms in the server per read, 28 ms of main-thread work per read.

## Media

- Before an upload the page makes a PNG, JPEG, WebP or HEIC image smaller with a canvas: the long edge is at most 2000 px, as JPEG, or as PNG when that is smaller (screenshots). The original is sent when that fails or is not smaller. Each file shows as "Uploading…" at once, and Send waits until all are in. The draft's text, its uploaded files and a message that was not delivered are kept in this browser's storage, so a reload keeps them.
- Upload: one file per `PUT api/media?name=…` request body. The type comes from the content signature, never from the name: PNG, JPEG, GIF, WebP, PDF, MP4, WebM, MP3, WAV, or valid UTF-8 text up to 1 MiB.
- A sent attachment is added to the prompt as the stored file's absolute path, so the agent can read it.
- Agent-produced files: the server looks for file paths in assistant messages of the saved conversation, opens them inside that session's worktree without following symlinks, and copies allowed types that are newer than the TTL and within the size limit. Hidden paths (`.env`, `.git/`), and names that look like credentials (`*secret*`, `*token*`, `*.pem`, `*.key`, `id_rsa*` and similar) are never copied. For other files this gives a chat-only site the same reach as asking the agent to print the file. Images are shown inline.
- Store: `media/<sha256>.<ext>`, URL `media/<sha256>.<ext>`. Responses are `Cache-Control: private, max-age=<seconds left>, immutable`. An entry is removed when its TTL ends, or oldest first when a new file would exceed the quota. Uploading the same bytes again restarts that entry's TTL. Partly received uploads are not counted against the quota. Single-range requests are supported.

## PWA and push

The manifest has 192 and 512 px PNG icons. The service worker caches the shell and serves it when offline; API and media requests are never cached by it. A subscription endpoint must be on one of `push.service_origins` (default: FCM, Mozilla and Apple).

- App badge: every push carries `badge`, the number of items that need the owner: blocked agent sessions the lists show (not task or manager sessions) plus each granted project's task `needs` (the Home count). The service worker sets it on push; the page sets it whenever its data changes, so it drops once an opened item is answered, and clears it at 0. It uses the Badging API (`navigator.setAppBadge`) where it exists: an installed PWA on iOS and iPadOS 16.4+ with notification permission, and Chromium on desktop. Android Chrome shows its own notification dot instead; elsewhere nothing happens.

## Notifications

A push reaches the owner only when the owner must act; the rest waits for one digest a day. Settings has three switches per device, each saved at once. Turning one on asks the browser for permission; a refusal or a failed save keeps the switches as stored and shows the error until the next save. With all three off the device has no subscription. Push text is written by wsx, never by the manager.

| Switch | Pushes when | Text | Tap opens |
|---|---|---|---|
| Manager needs me (`task_view`) | a task starts to wait on the owner (not on `hand`), an approval request is new, To check items are added | `<n> items need you: <projects>`; one line per open item: `#4 Fix the login: needs you: <its wait reason>`, `Approve land #5`, `2 to check` (with the project before each line when several have items) | the item when only one is open (its task sheet, or the To check list), else the project, else Home |
| An agent needs me | a listed session's agent needs input or stopped with an error | one session: `<project> › <branch>: <label>` over `Needs your input: <the question or the call it waits on>` or `Stopped with an error: <the call that failed, else the last reply>`; more: `3 agents need input: a, b, c; 1 agent stopped with an error: d`, one line per session | the session when only one, else the inbox |
| Daily digest | once a day at the device's chosen time (default 09:00), when something happened | `<site>: daily digest`; each task that became ready, failed (with the first check that did not pass) or landed since the last digest in its latest state, `deployed <commit>` and `released <commit>`, and `2 agents finished: a, b` | Home, or the inbox without `task_view` |

- Each of the first two is one push per device that replaces the earlier one (tag `wsx-<site>-manager`, `wsx-<site>-agents`) and counts every item still open, so a phone shows one notice per switch. It goes out once `notify.<switch>.merge_seconds` passed since the first new item; later ones in that window join it, and an item answered within the window is no news. Only a change notifies: the first sight of a session or project, as at a server start, does not. An agent that finished never pushes: it goes in the digest.
- A line is at most 160 characters with the title cut at 60; a body stops at 1500 bytes and ends `and <n> more`.
- No "Manager needs me" push goes to a device while one of its visible pages shows the project of every new item ([web-client-tasks.md](web-client-tasks.md) "Task notifications").
- Mute: a session's `⋯` menu has *Mute notifications* (*Unmute notifications*) while the device has a switch on. A muted session is left out of that device's agent pushes and its digest. Settings says "2 sessions muted" with *Unmute all*. A mute ends with its session.
- The digest time is a minute of the device's day in the UTC offset it had when the time was saved; save it again after a clock change. A digest that falls while the server is stopped goes when it starts again that day. A digest with nothing to say is not sent. Off means no digest; the "Since you left" line of a project stays.
- Stored per device in `push.json`: the subscription, `choices` (`manager`, `agents`, `digest {minute, utc_offset_minutes}`), `muted` and the last digest. The topics of earlier versions are read once at start: `blocked` and `error` turn on "An agent needs me", `tasks` "Manager needs me" and the digest, `done` the digest (09:00 in the server's offset).
- `GET api/push` answers `{public_key, choices, muted}`; `POST api/push {subscription, choices: {manager, agents, digest}}` saves (403 for `manager` without `task_view`, 400 for an endpoint off `service_origins` or bad keys); `DELETE api/push` removes the subscription; `POST api/push/mute {session, muted}` (404 for a session outside the grant, 409 `notifications_off` without a subscription).

## Files

The files panel covers each granted project's main checkout only, not its other worktrees. A project's root is pinned when it is first opened (at start for listed paths). `.git` is never listed, read or written. File access uses `O_NOFOLLOW` descriptors from the pinned project root; a write replaces an existing text file only when it did not change since it was read.

A `.md` or `.markdown` file opens rendered, as the chat renders an answer ([web-client-chat.md](web-client-chat.md) "Markdown"), with **Source** to show the text and, with `files_write`, the editor. The editor stays loaded while the rendered view shows, so an unsaved edit is kept; the rendered view shows the text as last read or saved.

## Dependencies

`axum` and `tokio` serve HTTP, the event stream and the terminal WebSocket (axum's `ws` feature, `tokio-tungstenite`). `p256`, `hkdf`, `aes-gcm` and `sha2` implement VAPID (RFC 8292) and message encryption (RFC 8291). `ureq` with rustls posts to the push service. No OpenSSL or libcurl is linked.

## Verification

```sh
cargo build --locked -p wsx -p wsx-daemon
python3 -B scripts/test-web-client.py [--part all|chat|tasks] [--browser] [--captures <dir>]
```

Each part runs against its own isolated `wsxd`, real PTYs and served sites, with fixture agents (`scripts/fake-claude-stream.py` as `claude`) and no model calls:

- **chat:** pairing and devices, every API route with its capability and refusals, the queue, timeline and conversations, Web Push through `scripts/web-push-fixture.mjs` (a receiver that checks the VAPID signature and decrypts), and terminal WebSockets the way a browser opens them (keys, resize, control lost and taken, reconnect, watch-only).
- **tasks:** a fixture store with tasks in every stage, three sites with and without `task_view` and `task_act`, every task action through the API, To check, the manager chat, approvals and task pushes.
- **`--browser`:** adds `scripts/web-terminal-check.mjs`, `scripts/web-browser-check.mjs` and `scripts/web-tasks-check.mjs` in an installed Chrome at phone and desktop sizes, with captures in `.work/web-captures/` or `--captures`.

The exact steps are the scripts; each prints the scenarios it passed.
