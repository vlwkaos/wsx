# Deploying the web client on a tailnet

Two example sites from one installed `wsx`: `owner` (all panels) and `family` (chat only). Both use the sub-path shape on `:443`, so they share one hostname and certificate. The contract is in [web-client.md](web-client.md). `wsx` never runs `tailscale` or a process manager for you; run the commands below yourself.

| Site | Site file | Loopback port | Public URL |
|---|---|---|---|
| owner | `~/.config/wsx/web/owner.toml` | 7431 | `https://<host>.<tailnet>.ts.net/app/wsx/` |
| family | `~/.config/wsx/web/family.toml` | 7432 | `https://<host>.<tailnet>.ts.net/app/family/` |

Replace `<host>.<tailnet>.ts.net` with the name from `tailscale status`. Check that both ports are free before you start (`lsof -iTCP:7431 -sTCP:LISTEN`).

## 1. Site files

```sh
mkdir -p ~/.config/wsx/web && chmod 700 ~/.config/wsx/web
wsx web vapid        # run once per site; paste the [push] block into that site file
```

`~/.config/wsx/web/owner.toml`:

```toml
id = "owner"
name = "wsx"
base_path = "/app/wsx"
port = 7431
public_origin = "https://<host>.<tailnet>.ts.net"
panels = ["chat", "files", "terminal", "activity"]

[project]
path = "/Users/<you>/ws/<project>"
capabilities = ["chat_send", "agent_write", "session_start", "files_read", "files_write",
                "terminal_view", "terminal_input", "media_upload", "media_download", "push"]
agent_command = ["claude"]

[push]
subject = "mailto:<you>@example.com"
private_key = "<from wsx web vapid>"
```

`~/.config/wsx/web/family.toml`:

```toml
id = "family"
name = "Home helper"
base_path = "/app/family"
port = 7432
public_origin = "https://<host>.<tailnet>.ts.net"
panels = ["chat"]

[project]
path = "/Users/<you>/ws/<family-project>"
capabilities = ["chat_send", "agent_write", "media_upload", "media_download", "push"]

[push]
subject = "mailto:<you>@example.com"
private_key = "<from a second wsx web vapid>"
```

```sh
chmod 600 ~/.config/wsx/web/*.toml       # the server refuses any other mode
```

The project must already be a wsx project with at least one agent session, unless the site has `session_start`.

## 2. Process manager (PM2)

```sh
pm2 start "$(command -v wsx)" --name wsx-web-owner  --interpreter none -- web serve --site ~/.config/wsx/web/owner.toml
pm2 start "$(command -v wsx)" --name wsx-web-family --interpreter none -- web serve --site ~/.config/wsx/web/family.toml
pm2 save
pm2 logs wsx-web-owner --lines 5         # expect: wsx web: site 'owner' on http://127.0.0.1:7431/app/wsx/ ...
```

After a wsx upgrade, restart only these two: `pm2 restart wsx-web-owner wsx-web-family`. Never use `pm2 kill`, `pm2 stop all` or `pm2 delete all`. The web server does not start `wsxd`; open `wsx` once on the host, or the page says that wsx is not running.

Loopback check before exposing anything (the `Host` header must be the public host):

```sh
curl -fsS -H 'Host: <host>.<tailnet>.ts.net' http://127.0.0.1:7431/app/wsx/manifest.webmanifest   # scope "/app/wsx/"
curl -fsS -H 'Host: <host>.<tailnet>.ts.net' http://127.0.0.1:7431/manifest.webmanifest           # same answer, stripped prefix
curl -s  -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7431/app/wsx/                          # 421: wrong Host
```

## 3. Tailscale Serve

Serve configuration is shared by every app on the node. Change one path at a time and never use `--reset`. Flags differ between Tailscale releases; confirm them with `tailscale serve --help`.

```sh
tailscale serve status                                            # record the state before
tailscale serve --bg --set-path /app/wsx    http://127.0.0.1:7431
tailscale serve --bg --set-path /app/family http://127.0.0.1:7432
tailscale serve status                                            # both paths listed, others unchanged
```

Remove one site (leaves every other mapping):

```sh
tailscale serve --bg --set-path /app/family off
pm2 delete wsx-web-family
```

Every app under one hostname and port shares a browser origin, so a script in a sibling app can use this site's device cookie. If another app on `:443` is not trusted, use the alt-port shape for the site. Alt-port root instead of a sub-path: set `base_path = ""` and `public_origin = "https://<host>.<tailnet>.ts.net:8443"` in the site file, then `tailscale serve --bg --https=8443 http://127.0.0.1:7432` (remove with `tailscale serve --bg --https=8443 off`).

## 4. Pair a device

```sh
wsx web pair --site ~/.config/wsx/web/family.toml     # prints XXXXX-XXXXX, valid 10 minutes, one use
wsx web devices --site ~/.config/wsx/web/family.toml
wsx web devices --site ~/.config/wsx/web/family.toml --revoke <id>
```

Open the public URL on the device, enter the code, then use the browser's "Add to Home Screen" or "Install". On iPhone, notifications work only from the installed app.

## 5. Check on the tailnet

1. `https://<host>.<tailnet>.ts.net/app/wsx/` shows the pairing form, or the chat on a paired device.
2. In browser developer tools, every request path starts with `/app/wsx/`.
3. If every request answers `421`, the proxy did not pass the public `Host`; compare it with `public_origin`.
