# Deploying the web client on a tailnet

Each site is one `wsx web serve` process on a loopback port. `wsx` never runs `tailscale` or a process manager for you; run the commands below yourself. The contract is in [web-client.md](web-client.md), themes in [web-themes.md](web-themes.md).

| Shape | Public URL | Site file | Use |
|---|---|---|---|
| **Tailscale Service (default)** | `https://<service>.<tailnet>.ts.net/` | `base_path` omitted | One hostname per site. Each site is its own browser origin. |
| Alt-port root | `https://<machine>.<tailnet>.ts.net:8443/` | `base_path` omitted | The host cannot be a tagged device, or the tailnet policy cannot change. |
| Sub-path (secondary) | `https://<machine>.<tailnet>.ts.net/app/<name>/` | `base_path = "/app/<name>"` | Still works. Every app on that hostname shares one browser origin, so a script in a sibling app can use this site's device cookie. |

## 1. Site file

A ready example for the service `remotty` is [examples/remotty.site.toml](examples/remotty.site.toml).

```sh
mkdir -p ~/.config/wsx/web && chmod 700 ~/.config/wsx/web
cp docs/examples/remotty.site.toml ~/.config/wsx/web/remotty.toml
chmod 600 ~/.config/wsx/web/remotty.toml        # the server refuses any other mode
```

The `public_origin` must be the exact origin the browser uses, and `port` must be free (`lsof -iTCP:4690 -sTCP:LISTEN`). For Web Push, run `wsx web vapid`, paste the printed `[push]` block into your copy, and add `"push"` to `capabilities`. The project must already be a wsx project.

A second site is a second file with its own `id`, `port`, service name and, usually, fewer panels and capability bits:

```toml
id = "family"
name = "Home helper"
port = 4691
public_origin = "https://family.<tailnet>.ts.net"
panels = ["chat"]
theme = "default"

[project]
path = "/Users/<you>/ws/<family-project>"
capabilities = ["chat_send", "agent_write", "media_upload", "media_download"]
```

## 2. Process manager (PM2)

```sh
pm2 start "$(command -v wsx)" --name wsx-web-remotty --interpreter none -- web serve --site ~/.config/wsx/web/remotty.toml
pm2 save
pm2 logs wsx-web-remotty --lines 5      # expect: wsx web: site 'remotty' on http://127.0.0.1:4690/ ...
```

After a wsx upgrade, restart only this process: `pm2 restart wsx-web-remotty`. Never use `pm2 kill`, `pm2 stop all` or `pm2 delete all`. The web server does not start `wsxd`; open `wsx` once on the host, or the page says that wsx is not running.

Loopback check before exposing anything (the `Host` header must be the public host):

```sh
curl -fsS -H 'Host: remotty.<tailnet>.ts.net' http://127.0.0.1:4690/manifest.webmanifest   # "scope":"/"
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:4690/                             # 421: wrong Host
```

## 3. Tailscale Service

Requirements, each one yours to decide and apply:

| Requirement | Where |
|---|---|
| Tailscale 1.86.0 or later on the host | `tailscale version` |
| The host is a tagged device | Tailnet admin. A tagged device loses its user identity and its key expiry; removing all tags needs re-authentication. |
| Service `svc:remotty` defined with `tcp:443` | Admin console, Services |
| The host approved for the service | Admin console, or an `autoApprovers` rule |
| A grant that lets your devices reach `svc:remotty` on 443 | Tailnet policy |

Policy entries (replace the tag and owner with your own):

```jsonc
"tagOwners":     { "tag:server": ["<owner>"] },
"autoApprovers": { "services": { "svc:remotty": ["tag:server"] } },
"grants": [ { "src": ["autogroup:member"], "dst": ["svc:remotty"], "ip": ["443"] } ]
```

Serve configuration is shared by every app on the node. Use service-scoped commands only and never `tailscale serve reset`. Flags differ between releases; confirm them with `tailscale serve --help`.

```sh
tailscale serve status --json                                          # record the state before
tailscale serve --service=svc:remotty --https=443 127.0.0.1:4690
tailscale serve status --json                                          # the service is listed, others unchanged
```

Remove the site (the service definition and grant stay in the tailnet until you remove them):

```sh
tailscale serve drain svc:remotty     # stop new connections, let open ones finish
tailscale serve clear svc:remotty     # remove this service from this host
pm2 delete wsx-web-remotty
```

The service name appears in public Certificate Transparency logs when its certificate is issued; do not put a sensitive word in it.

## 4. Pair a device

```sh
wsx web pair --site ~/.config/wsx/web/remotty.toml      # prints XXXXX-XXXXX, valid 10 minutes, one use
wsx web devices --site ~/.config/wsx/web/remotty.toml
wsx web devices --site ~/.config/wsx/web/remotty.toml --revoke <id>
```

Open the public URL on the device, enter the code, then use the browser's "Add to Home Screen" or "Install". On iPhone, notifications work only from the installed app.

## 5. Check on the tailnet

1. From a second tailnet device, `https://remotty.<tailnet>.ts.net/` shows the pairing form, or the chat on a paired device.
2. If every request answers `421`, the proxy did not pass the public `Host`; compare it with `public_origin`.
3. A device without the grant cannot connect.

## Other shapes

Alt-port root: keep `base_path` omitted, set `public_origin = "https://<machine>.<tailnet>.ts.net:8443"`, then `tailscale serve --bg --https=8443 http://127.0.0.1:4690`. Remove it with `tailscale serve --bg --https=8443 off`.

Sub-path (secondary): set `base_path = "/app/wsx"` and `public_origin = "https://<machine>.<tailnet>.ts.net"`, then `tailscale serve --bg --set-path /app/wsx http://127.0.0.1:4690`. Remove only that path with `tailscale serve --bg --set-path /app/wsx off`. The server answers with and without the prefix, because Serve may strip it. Prefer a service for any site whose sibling apps on the same hostname you do not fully trust.

Moving a site between shapes changes its origin: paired devices, the installed app and push subscriptions do not carry over. Pair again and reinstall.
