# Round corners and the composer without a band (task 519, owner message 5409)

Mockups only. `build.py` writes `x-<page>-before|after-<light|dark>.html` and `round-<page>-<scheme>-<390|1280>.html`
(two iframes side by side); `capture.mjs round-` shoots them. Before = the real `app.css`. After = the same plus
`round.css`, which is the build's CSS written as overrides. Pages: `home` (with the tab bar), `chat`, `chat-menu`, `tasks`, `sheet`.
Images: `round-<page>-<light|dark>-<390|1280>.png`.

## Which radius the Home menu uses

Assuming "Home menu" is the phone's bottom tab bar (the only rounded box on Home; its list rows have no corners).

| Part | Today | Token |
| --- | --- | --- |
| `.tabbar-pill` | 32 px | `--radius-sheet` x 2 (a full pill) |
| `.tabbar a` (a tab, 44 px high) | 16 px | `--radius-sheet` |

So 16 px on a 44 px control is the Home look. Radii today: control 10 px (24 uses), key 8 px (11), sheet 16 px (4), plus ad-hoc ones.

## Radius changes (selector, old, new)

| Selector | Old | New |
| --- | --- | --- |
| theme.toml `radius-control` (buttons, inputs, bubbles, cards, menus, code blocks, tables) | 10 px | 16 px |
| theme.toml `radius-key` (images, keys, chips on a line) | 8 px | 12 px |
| theme.toml `radius-sheet` (tab, sheets, phone composer, `.boot-mark`) | 16 px | 16 px, same |
| `.markdown :not(pre) > code`, `.state-chip`, `.task-chip`, `.markdown .task-chip` | `calc(var(--radius-key) / 2)` | `var(--radius-key)` |
| `.usage-legend i`, `.usage-parts dt i` | `calc(var(--space-1) / 2)` | `var(--radius-key)` |
| `.usage-bar i:first-child`, `:last-child`, `:only-child` (3 rules) | `var(--space-1) 0 0 var(--space-1)`, `0 var(--space-1) var(--space-1) 0`, `var(--space-1)` | deleted; `.usage-bar` gets `overflow: hidden; border-radius: var(--radius-key)` |
| `.usage-meter-track` | `var(--space-1)` | `var(--radius-key)` |
| `.app[data-wide] .task-sheet` (right panel) | `0` | `var(--radius-sheet) 0 0 var(--radius-sheet)` |

Not shown in the mockups: the usage bar, the usage legend, the wide task sheet.

Stay as they are:
- Circles and pills: `50%`, `.pill` (`--pill-height`), `.build-pill`, the count badge, the switch, `.sheet-grip`, `.tabbar-pill`.
- `.diagram img, .viewer img` (`0`): a picture shown at full size is not a box.
- `border-radius: inherit` on found rows.

After the change `--radius-control` and `--radius-sheet` are both 16 px. Owner question below.

## Composer, one rule

Home draws its box with no band: the tab bar nav has no background, and the pill has its own surface and hairline.
The phone composer already does the same (`.app:not([data-wide]) .composer`: `background: none`, no `border-top`, a rounded card).
Only the wide window keeps the band (`.composer` base rule: `background: var(--color-surface)` and `border-top`).

The change: drop `:not([data-wide])` from the 11 composer rules (`.composer`, `.composer-row`, `.composer:focus-within .composer-row`, `[data-shell]`,
`.composer-input`, the field and mirror, the icon button and send button, `.send-mark`, `.send-text`), so one card is drawn on every width.
The base rule keeps `.attention` only; the rule `.composer:focus-within { border-top-color }` has nothing left to colour and goes.
The padding takes `--column-inset` instead of `--space-4`, so the card lines up with the message column at 1280 px.

Visible at 1280 px: the card is as wide as the message column, the field on top, attach and the pills under it, and a round arrow replaces the text "Send" button.

## Coordination

#518 (thread strip) and #515 (phone composer touch) edit the same composer rules. The build turn rebases on main after both
and edits `crates/wsx-web/web/app.css` and `themes/default/theme.toml`.
