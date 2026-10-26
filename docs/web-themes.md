# Web client themes

The web client's look is a contract with three parts: tokens, component hooks, and theme packages. The core stylesheet (`crates/wsx-web/web/app.css`) holds structure only; every design value is a token. Site file and security rules are in [web-client.md](web-client.md).

```toml
theme = "default"                 # the built-in theme (dark, with a light scheme)
theme = "/abs/path/to/my-theme"   # a theme directory, or the path of a theme .toml file
```

## Tokens

`crates/wsx-web/web/themes/default/theme.toml` is the complete list: 64 tokens, 16 of them the terminal palette. A theme must define every token, directly or through `extends`. A token becomes the CSS variable `--<token>`.

| Group | Tokens | Value |
|---|---|---|
| Color roles | `color-bg` `color-surface` `color-raised` `color-hairline` `color-text` `color-muted` `color-faint` `color-accent` `color-accent-text` `color-attention` `color-danger` `color-overlay` `color-peer-1` … `color-peer-4` | `#rgb`, `#rrggbb` or `#rrggbbaa` |
| Terminal palette | `color-ansi-0` … `color-ansi-15` (ANSI 0-7, then bright) | same |
| Type families | `font-body` `font-mono` `font-terminal` | family list: letters, digits, space, `,` `'` `-` `_` |
| Type scale | `font-size-md` `font-size-sm` `font-size-label` `font-size-title`, `font-size-field` (text fields on a touch device; keep it at 16px or more, or iOS zooms the page on focus) | length |
| Type weight, leading, tracking | `font-weight-medium` `font-weight-strong`, `line-height` `line-height-prose`, `tracking-label` `tracking-title` | 100-900; 1-3; length, may be negative |
| Spacing | `space-1` … `space-4` (4 px grid; `space-4` is the screen gutter and row inset) | length |
| Sizes | `size-control` (touch target) `size-row` `size-bar` (top row, tab bar) `size-key` (terminal keys) `size-rail` `size-prose` (longest text line) | length |
| Radius, border | `radius-control` `radius-key` `radius-sheet` `border-width` | length |
| Motion | `motion-duration`, `motion-easing` | `ms` or `s`; `linear` `ease` `ease-in` `ease-out` `ease-in-out` |
| Focus, opacity | `focus-width`, `opacity-disabled` | length; 0-1 |

A length is `0` or a number with `px`, `rem`, `em`, `ch`, `vh`, `vw` or `%`. Any other value, and any unknown token name, stops the server at start with the token named. `[tokens_dark]` may set only `color-*` and `opacity-*` tokens. There are no shadow tokens: the design has no shadows.

Role use: `color-bg` is the page and the terminal. `color-surface` is the composer, sheets and the user's message. `color-raised` is pressed and selected rows, inputs and terminal keys. `color-hairline` is every divider and border. `color-accent` is the one primary action of a screen, the focus ring and the working state. Every working state (a task or a session at work, its mark and its pulse, the manager typing and its dots, "N working") is painted with `--color-working`, set once in `app.css` to `color-accent` and named by the `working` tone; it is not a theme token, so a theme changes it through `color-accent`. Ready, verify and land keep the plain accent. `color-attention` is "needs you". `color-danger` is error, failed and destructive. A state is always a word; the color repeats it. In the manager chat each speaker has a color on its name and its bubble's edge, beside the name that stays: the owner `color-accent`, the manager `color-text`, wsx events `color-muted`, and another project's manager one of `color-peer-1` … `color-peer-4`, chosen by a hash of its project so it keeps its color.

Dark scheme rule: a theme has a dark scheme when it has a `[tokens_dark]` table. A theme that changes any `color-*` token in `[tokens]` and has no `[tokens_dark]` is light only; it does not inherit the dark colors of the theme it extends. A light-only theme is served with `color-scheme: light`.

A theme with a dark scheme is shown dark by default. A device switches to light under Settings, "Appearance on this device"; the choice is kept in that browser as `data-scheme="light"` on the page root. The operating system preference is not read. An element with `data-scheme="dark"` uses the dark tokens in either case: the terminal does, so there is no light bar around the dark screen.

### Contrast of the default theme

WCAG contrast ratio of each text and state color on `color-bg`, `color-surface` and `color-raised`. Every pair is 4.5 or more.

| Token | Dark | bg / surface / raised | Light | bg / surface / raised |
|---|---|---|---|---|
| `color-text` | `#ecedee` | 16.8 / 15.7 / 14.4 | `#16181c` | 16.6 / 17.8 / 15.2 |
| `color-muted` | `#9aa0a8` | 7.5 / 7.0 / 6.4 | `#5a6069` | 5.9 / 6.3 / 5.4 |
| `color-faint` | `#80858e` | 5.3 / 5.0 / 4.6 | `#646a74` | 5.1 / 5.5 / 4.7 |
| `color-accent` | `#6fe3c1` | 12.6 / 11.8 / 10.8 | `#0b6b56` | 6.0 / 6.5 / 5.5 |
| `color-attention` | `#f2b84b` | 11.0 / 10.3 / 9.4 | `#8a5a00` | 5.5 / 5.9 / 5.1 |
| `color-danger` | `#f2777a` | 7.2 / 6.8 / 6.2 | `#b3261e` | 6.1 / 6.5 / 5.6 |
| `color-peer-1` | `#7aa7f7` | 8.2 / 7.6 / 7.0 | `#2a59c4` | 5.9 / 6.3 / 5.4 |
| `color-peer-2` | `#c594f0` | 8.3 / 7.8 / 7.1 | `#7a3fc4` | 5.9 / 6.3 / 5.4 |
| `color-peer-3` | `#f589c4` | 8.7 / 8.1 / 7.4 | `#a8326f` | 5.9 / 6.3 / 5.4 |
| `color-peer-4` | `#a9d16a` | 11.3 / 10.6 / 9.7 | `#4d7300` | 5.2 / 5.6 / 4.8 |
| `color-accent-text` on `color-accent` | `#06231b` | 10.6 | `#ffffff` | 6.5 |

The terminal palette is read on the dark `color-bg` (`#0a0b0d`) in both schemes. Its lowest ratios are `color-ansi-0` (`#747983`, 4.5) and `color-ansi-8` (`#8b9098`, 6.1); every other entry is 7.2 or more.

## Component hooks

Shared parts carry a stable class. Component CSS in a theme should style these classes only, and read tokens.

| Part | Hook classes |
|---|---|
| Layout | `.app` `.rail` (left column at 900 px and wider) `.tabbar` (bottom bar on a phone; current link: `[aria-current="page"]`) |
| Screen | `.title` `.label` (section label) `.rows` `.row` (state mark: `.mark`; `[data-tone="accent|attention|danger"]` repeats a state word in color) |
| Controls | `.button` `.button-primary` `.button-danger` `.link-action` `.icon-button` `.input` |
| Session top row | `.bar` `.switch` (Chat or Terminal) |
| Conversation | `.chat` `.log` `.message` `.steps` `.working` `.composer` `.queue` |
| Files | `.code` `.crumbs` |
| Settings, pairing, dialog | `.options` `.pair-form` `.sheet` (a native `<dialog>`; backdrop uses `color-overlay`) `.notice` |
| Live terminal | `.terminal` `.terminal-notice` `.terminal-key` `.terminal-pad` |

`<body data-theme="<name>">` names the active theme.

## Theme package

A directory with a `theme.toml`, or a single `.toml` file. Unknown fields are refused.

```toml
name = "my-theme"                 # a-z, 0-9, '-'
extends = "default"               # the built-in theme; this is the default
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
| Icon | exact 192 and 512 px PNG, 1 MiB each; full-bleed, with the mark inside the central 80 % circle |

Files are read once at start, relative to the theme directory, without following symlinks. Fonts are declared in `theme.toml` only; the server writes the `@font-face` rules. An `[icon]` table in the site file replaces the theme's icons. The manifest lists the icons as `any maskable`: a launcher may crop them to a circle or another shape, so only the central 80 % is sure to show. The default icon's source is `themes/default/icon.svg`; `scripts/render-web-icon.sh` renders the two PNGs from it.

## Serving

The server builds one stylesheet (font faces, tokens, dark tokens, component CSS of the extended theme, then the theme's own) and serves it, the fonts and the icons as `theme/<first 16 hex of SHA-256>.<ext>` with `Cache-Control: public, max-age=31536000, immutable`. The page links the stylesheet; there is no inline style. The CSP stays `default-src 'none'` with `'self'` for styles and fonts, so a theme cannot load anything from another origin. The PWA manifest takes `background_color` and `theme_color` from `color-bg` of the scheme the page starts in, and its icons from the theme.

## Built-in themes

| Theme | Schemes | Notes |
|---|---|---|
| `default` | dark (default), light | System UI font, hairlines and no shadows, one accent. |

## Verification

`scripts/test-web-client.py` covers theme selection, a custom directory with a font and icons, each refusal above, content-hash URLs, the manifest, the CSP, and that `app.css` holds no color or length literal. With `--browser` it captures every screen at 320, 390 and 1280 px and writes a slop audit beside each capture.
