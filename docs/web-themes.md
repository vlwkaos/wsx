# Web client themes

The web client's look is a contract with three parts: tokens, component hooks, and theme packages. The core stylesheet (`crates/wsx-web/web/app.css`) holds structure only; every design value is a token. Site file and security rules are in [web-client.md](web-client.md).

```toml
theme = "default"                 # built-in: default (light and dark) or retro (light only)
theme = "/abs/path/to/my-theme"   # a theme directory, or the path of a theme .toml file
```

## Tokens

`crates/wsx-web/web/themes/default/theme.toml` is the complete list. A theme must define every token, directly or through `extends`. A token becomes the CSS variable `--<token>`.

| Group | Tokens | Value |
|---|---|---|
| Color roles | `color-bg` `color-surface` `color-sunken` `color-text` `color-muted` `color-border` `color-accent` `color-accent-text` `color-danger` `color-ok` `color-focus` `color-overlay` | `#rgb`, `#rrggbb` or `#rrggbbaa` |
| Component colors | `color-chrome-bg` `color-chrome-text` `color-bubble-user-bg` `color-bubble-user-text` `color-bubble-agent-bg` `color-bubble-agent-text` `color-terminal-bg` `color-terminal-text` | same |
| Type families | `font-body` `font-heading` `font-mono` | family list: letters, digits, space, `,` `'` `-` `_` |
| Type scale | `font-size-sm` `font-size-md` `font-size-lg` `font-size-xl` `font-size-mono`, `font-size-field` (text fields on a touch device; keep it at 16px or more, or iOS zooms the page on focus) | length |
| Type weight | `font-weight-normal` `font-weight-strong`, `line-height` | 100-900; 1-3 |
| Spacing | `space-1` … `space-5` | length |
| Sizes | `size-control` `size-key` (terminal keys and icon buttons) `size-content` `size-bubble` `size-media` `size-field` `size-log` | length |
| Radius, border | `radius-control` `radius-surface` `border-width` | length |
| Shadow | `shadow-raised` `shadow-sunken` `shadow-overlay` | `none`, or `[inset] x y [blur] [spread] #color`, comma-separated |
| Motion | `motion-duration`, `motion-easing` | `ms` or `s`; `linear` `ease` `ease-in` `ease-out` `ease-in-out` |
| Focus | `focus-width` `focus-offset` (with `color-focus`) | length |
| Opacity | `opacity-disabled` `opacity-quiet` | 0-1 |

A length is `0` or a number with `px`, `rem`, `em`, `ch`, `vh`, `vw` or `%`. Any other value, and any unknown token name, stops the server at start with the token named. `[tokens_dark]` may set only `color-*`, `shadow-*` and `opacity-*` tokens.

Dark scheme rule: a theme has a dark scheme when it has a `[tokens_dark]` table. A theme that changes any `color-*` token in `[tokens]` and has no `[tokens_dark]` is light only; it does not inherit the dark colors of the theme it extends. A light-only theme is served with `color-scheme: light`.

A theme with a dark scheme is shown dark by default. A device switches to light under Settings, "Appearance on this device"; the choice is kept in that browser as `data-scheme="light"` on the page root. The operating system preference is not read. An element with `data-scheme="dark"` uses the dark tokens in either case: the terminal frame does, so there is no light bar around the dark screen.

## Component hooks

Shared parts carry a stable class. Component CSS in a theme should style these classes only, and read tokens.

| Part | Hook classes |
|---|---|
| App chrome and title bar | `.app-chrome` `.app-title` (the name is in a child `span`) `.app-nav` (current link: `[aria-current="page"]`) |
| Panel | `.panel` |
| List and row | `.list` `.list-row` |
| Button | `.button` `.button-primary` `.button-danger` |
| Input | `.input` (text fields, selects, text areas) |
| Chat bubble | `.bubble` `.bubble-user` `.bubble-agent` (`[data-pending]`) |
| Badge and status | `.badge` (`[data-state="idle|working|blocked|done|error|unknown"]`) |
| Dialog | `.dialog` (a native `<dialog>`; backdrop uses `color-overlay`) |
| Toast | `.toast` (`[data-kind="error"]`) |
| Terminal frame | `.terminal-frame` (file text) |
| Live terminal | `.terminal-bar` `.terminal-icon` `.terminal-notice` `.terminal-key` `.terminal-pad` `.terminal-sheet` |

`<body data-theme="<name>">` names the active theme.

## Theme package

A directory with a `theme.toml`, or a single `.toml` file. Unknown fields are refused.

```toml
name = "my-theme"                 # a-z, 0-9, '-'
extends = "retro"                 # a built-in theme; default: "default"
components = "components.css"     # optional component overrides

[tokens]                          # only the tokens that differ from the extended theme
color-accent = "#225577"
font-body = "My Sans, system-ui, sans-serif"

[tokens_dark]                     # optional
color-accent = "#88bbdd"

[[fonts]]                         # optional, at most 8
family = "My Sans"
file = "fonts/my-sans.woff2"      # WOFF2, WOFF, OTF or TTF, by file signature
weight = "400"                    # 100-900, normal or bold
style = "normal"                  # normal or italic

[icons]                           # optional; both are required together
png_192 = "icon-192.png"
png_512 = "icon-512.png"
```

| Part | Limit |
|---|---|
| `theme.toml` | 64 KiB |
| Component CSS | 128 KiB; may not contain `url(`, `@import`, `@font-face`, `image-set(`, `src(`, `expression(`, `javascript:`, `//`, `</` or `\` |
| Font file | 2 MiB each |
| Icon | exact 192 and 512 px PNG, 1 MiB each |

Files are read once at start, relative to the theme directory, without following symlinks. Fonts are declared in `theme.toml` only; the server writes the `@font-face` rules. An `[icon]` table in the site file replaces the theme's icons.

## Serving

The server builds one stylesheet (font faces, tokens, dark tokens, component CSS of the extended theme, then the theme's own) and serves it, the fonts and the icons as `theme/<first 16 hex of SHA-256>.<ext>` with `Cache-Control: public, max-age=31536000, immutable`. The page links the stylesheet; there is no inline style. The CSP stays `default-src 'none'` with `'self'` for styles and fonts, so a theme cannot load anything from another origin. The PWA manifest takes `background_color` from `color-bg`, `theme_color` from `color-chrome-bg`, and its icons from the theme.

## Built-in themes

| Theme | Schemes | Notes |
|---|---|---|
| `default` | dark (default), light | System fonts, flat surfaces. |
| `retro` | light | System 7 look after the remotty project: gray surfaces, black hairlines, beveled controls that invert when pressed, pinstripe title bar, hard offset shadow on dialogs. |

`retro` ships no font. Chicago (ChicagoFLF) has conflicting licence statements (the author's public-domain note against "personal use" listings), and remotty's JetBrains Mono Nerd Font files are 2.5 MB each. The font stacks name Chicago and JetBrains Mono for machines that have them and fall back to Geneva, Verdana and the system monospace font. To ship a font you have the right to distribute, write a theme that extends `retro` and declares it under `[[fonts]]`.

## Verification

`scripts/test-web-client.py` covers theme selection, a custom directory with a font and icons, each refusal above, content-hash URLs, the manifest, the CSP, and that `app.css` holds no color or length literal. With `--browser` it captures every panel, the dialog and the toast for `default` and `retro` at 320 and 1280 px.
