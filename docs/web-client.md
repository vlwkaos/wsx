# Hosted web client

`wsx web serve --site <site.toml>` hosts one paired web site: a remote chat with the agents of one project, plus optional files, terminal and activity panels. It is a `wsxd` client like the TUI. It adds no daemon or protocol surface and never starts, stops or replaces `wsxd`. Deployment commands are in [web-deploy.md](web-deploy.md).

```sh
wsx web vapid                          # print a new Web Push key pair for a site file
wsx web serve   --site <site.toml>     # serve on 127.0.0.1:<port> until SIGINT or SIGTERM
wsx web pair    --site <site.toml>     # print a one-time pairing code (10 minutes, one use)
wsx web devices --site <site.toml> [--revoke <id>] [--json]
```

## Site file

One site per process. The file must be a regular file owned by the user with mode `600`. Unknown fields are refused.

```toml
id = "owner"                              # a-z, 0-9, '-'; names the state directory and cookie
name = "wsx"                              # shown in the page and the installed app
port = 7431                               # loopback port
public_origin = "https://wsx.tailnet.ts.net"   # exact origin the browser uses
base_path = ""                            # omit for a service or alt-port site; "/app/wsx" for a sub-path
panels = ["chat", "files", "terminal", "activity"]
theme = "default"                         # default, retro, or the absolute path of a theme (web-themes.md)

[icon]                                    # optional; replaces the theme's icons
png_192 = "/abs/icon-192.png"
png_512 = "/abs/icon-512.png"

[project]
path = "/abs/path/to/project"             # the one project this site may reach
capabilities = ["chat_send", "agent_write", "files_read", "files_write",
                "terminal_view", "terminal_input", "media_upload", "media_download", "push"]
agent_command = []                        # required with session_start, for example ["claude"]

[media]                                   # optional; these are the defaults
max_file_bytes = 26214400
quota_bytes = 1073741824
ttl_seconds = 604800

[push]                                    # required with the push capability; from `wsx web vapid`
subject = "mailto:you@example.com"
private_key = "<base64url>"
```

| Capability | Allows | Needs panel |
|---|---|---|
| `chat_send` | send and stop messages | chat |
| `agent_write` | exchanges are created as `writer` instead of `read-only` | chat |
| `session_start` | start a new session that runs `agent_command` | chat |
| `files_read`, `files_write` | browse and read text; replace an existing text file | files |
| `terminal_view`, `terminal_input` | read the current screen; type text and six named keys | terminal |
| `media_upload`, `media_download` | attach files to a prompt; receive agent-produced files | any |
| `push` | Web Push for this device | any |

A second site is the same binary with another file, fewer panels and fewer bits. The server enforces every bit; the page only hides what the session answer does not list. `read-only` and `writer` are the broker's cooperative labels. They do not restrict what the agent process can do.

## Security

- The server listens on `127.0.0.1` only. `Host` must equal the host of `public_origin`. `Origin` must equal `public_origin` when present, and is required on every state-changing request. API and media requests with a cross-site `Sec-Fetch-Site` are refused.
- Pairing: `wsx web pair` stores the SHA-256 of a 10-character code. The device exchanges the code for a random 256-bit credential in an `HttpOnly; SameSite=Strict` cookie with `Path=<base>/` (`Secure` for an https origin). Only the credential's SHA-256 is stored. There is no shared token.
- Limits: 240 requests, 20 messages and 6 session starts per minute per device, four open event streams per device, 16 devices. Pairing attempts are limited to five per minute for the whole site, because the proxy hides client addresses; a tailnet member who keeps guessing can therefore delay a real pairing.
- Origin sharing: a service or alt-port site has its own browser origin. Under a sub-path, every app on the same hostname and port is the same browser origin. Script from a sibling app can call this API with the device's cookie; the `Origin` check and `SameSite` cannot tell them apart. Host a site on its own HTTPS port (alt-port root) when another app on that hostname is not trusted.
- A revoke takes effect on the next request. An open event stream ends within 20 seconds, and the device's push subscription is removed.
- CSP is `default-src 'none'` with `'self'` for scripts, styles, fonts, images, media, connections, the manifest and the worker. There is no inline script or style, and a theme cannot add an origin.
- State lives in `<wsx state dir>/web/<id>/` with owner-only modes: `devices.json`, `push.json`, `media/`.

## Base path

The default is a site at the root of its own hostname (a Tailscale Service) or port, with `base_path` omitted. A sub-path remains supported as a secondary shape. Every browser URL is relative to `<base href="<base>/">`, written at serve time, so one build serves all shapes. The manifest `id`, `scope` and `start_url`, the service worker scope and cache list, and the cookie `Path` use the same base. With a base path, each route answers with and without the prefix, because `tailscale serve --set-path` may strip it.

## Themes

`theme` selects a built-in theme or a deployer-supplied theme directory: validated tokens, optional component CSS, fonts and icons, served under content-hash URLs. The manifest colors and icons follow the theme. See [web-themes.md](web-themes.md).

## Chat

- Sending uses the exchange broker (`docs/agent-orchestration.md`): one exchange per pane, continued round by round. For an idle or done Claude the draft-stash variant is used, so the owner's unsent input is not merged into the message.
- The page shows the broker's evidence in plain words. `pty_delivery` is "typed into the agent's terminal; the agent has not confirmed it". Only `request_bound` is shown as confirmed.
- History is the provider's own saved conversation (`wsx_core::integration::memory`): the last 80 messages, at most 256 KiB. Terminal output is never read into chat. A sent message that is not in the saved conversation yet is shown as pending.
- A blocked agent shows a provider dialog. The chat refuses to send; the answer is given in the terminal panel or on the host.

## Live updates

A page opens one `EventSource` (`api/events`) and has no polling timer. Events: `workspace` (session list), `history` (a session's saved conversation changed), `exchange`, `daemon`, `resync`, `revoked`. The server follows `wsxd` with its long-poll. While a page is connected it also compares each transcript file's size and modification time about every two seconds.

## Media

- Upload: one file per `PUT api/media?name=…` request body. The type comes from the content signature, never from the name: PNG, JPEG, GIF, WebP, PDF, MP4, WebM, MP3, WAV, or valid UTF-8 text up to 1 MiB.
- A sent attachment is added to the prompt as the stored file's absolute path, so the agent can read it.
- Agent-produced files: the server looks for file paths in assistant messages of the saved conversation, opens them inside that session's worktree without following symlinks, and copies allowed types that are newer than the TTL and within the size limit. Hidden paths (`.env`, `.git/`), and names that look like credentials (`*secret*`, `*token*`, `*.pem`, `*.key`, `id_rsa*` and similar) are never copied. For other files this gives a chat-only site the same reach as asking the agent to print the file. Images are shown inline.
- Store: `media/<sha256>.<ext>`, URL `media/<sha256>.<ext>`. Responses are `Cache-Control: private, max-age=<seconds left>, immutable`. An entry is removed when its TTL ends, or oldest first when a new file would exceed the quota. Uploading the same bytes again restarts that entry's TTL. Partly received uploads are not counted against the quota. Single-range requests are supported.

## PWA and push

The manifest has 192 and 512 px PNG icons. The service worker caches the shell and serves it when offline; API and media requests are never cached by it. Push is off until a device picks topics in Settings and accepts the browser prompt: agent needs input (`blocked`), agent finished (`done`), agent error (`error`). Only a state change notifies. Tapping a notification opens that session. A subscription endpoint must be on one of `push.service_origins` (default: FCM, Mozilla and Apple).

## Terminal and files

The terminal panel is the pane's current screen as text, refreshed on events and on request. It is not a live terminal emulator. Input uses the same short lease as `wsx session send-text` and never takes over a terminal that another client holds. The files panel covers the project's main checkout only, not its other worktrees. `.git` is never listed, read or written. File access uses `O_NOFOLLOW` descriptors from the pinned project root; a write replaces an existing text file only when it did not change since it was read.

## Dependencies

`axum` and `tokio` serve HTTP and the event stream. `p256`, `hkdf`, `aes-gcm` and `sha2` implement VAPID (RFC 8292) and message encryption (RFC 8291). `ureq` with rustls posts to the push service. No OpenSSL or libcurl is linked.

## Verification

```sh
cargo build --locked -p wsx -p wsx-daemon
python3 -B scripts/test-web-client.py [--browser]
```

The model-free journey uses an isolated `wsxd`, PTY fixture agents with their own transcripts, three served sites and `scripts/web-push-fixture.mjs`, a separate receiver that verifies the VAPID signature and decrypts the payload. `--browser` adds `scripts/web-browser-check.mjs` with an installed Chrome and writes captures to `.work/web-captures/`. It does not prove a real Claude turn, a real push service, a phone, or Tailscale.
