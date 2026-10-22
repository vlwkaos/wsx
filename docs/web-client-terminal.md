# Web client terminal

The terminal panel is xterm.js on the pane's raw byte stream. One WebSocket (`api/terminal/<session>/ws`) is bridged to one `wsxd` raw stream (`crates/wsx-web/src/terminal.rs`). Closing the socket ends the stream and releases the pane.

| Frame | Direction | Content |
|---|---|---|
| binary | server to page | PTY bytes; after a `replay` message, the screen to start from |
| binary | page to server | input bytes, at most 64 KiB per frame |
| text | server to page | `replay`, `resized`, `clipboard` (only to a device with `terminal_input`: the program sets that device's clipboard), then one of `busy`, `control_lost`, `exited`, `error`, `revoked` before the socket closes |
| text | page to server | `resize` (needs `terminal_input`) |

- **One controller.** A pane has one writable lease, and the newest person's action wins it. Opening the terminal takes the pane from a wsx window; the window says that control moved. A reconnect never takes over, or two clients would take the pane back and forth: when a wsx window or a chat message takes the terminal, the page shows "Control moved to another wsx window" and attaches again only after **Take control**. A device without `terminal_input` does not take over; it shows "Controlled from another wsx window" with **Try again**.
- **Page identity.** A page sends a random `client` value that stays the same while the page is open. The server derives the lease identity from the paired device and that value. A reconnect or a reopened panel therefore replaces the page's own old stream without a takeover. Another tab is another controller.
- **Size.** *Fit screen* (default) resizes the pane to the browser. *Host size* leaves the pane as it is and scales the view. The choice is kept in the browser. A device without `terminal_input` always uses host size and cannot type.
- **Reconnect.** After a lost connection, a daemon handoff or a daemon restart the page reconnects by itself (0.5 to 5 seconds apart) and starts from a new replay. A resize also ends in a new replay, because xterm.js and `wsxd`'s emulator reflow a resized screen differently.
- **Queries.** `wsxd`'s emulator answers terminal queries. The page disables xterm.js's own answers (device attributes, status reports, mode and color queries), so a program gets one reply.
- While the screen is scrolled back from the newest output, a "Jump to latest" button at the bottom of the screen returns to it; it hides again at the end.
- **Touch devices** (`pointer: coarse`, or a window narrower than 700 px) get the key row `Esc Tab ^C ^V ^A D ^Z ^R ^L ⌫` (a held key repeats every 80 ms) and a second row: Paste at the left, a pad in the middle that sends one arrow key per 18 px of drag, and `^J` (a new line) and Enter at the right edge, under the thumb. `^J` and Enter do not repeat when held. A touch drag on the screen sends wheel reports when the program tracks the mouse, arrow keys on the alternate screen, and scrolls the 10,000-line local scrollback otherwise. Composition events are not touched, so Korean and other IME input works. While the terminal, or any field that takes typing, has the focus, the page is pinned to `visualViewport` (its height and `offsetTop`) and never scrolls. Otherwise it is the full screen (`100dvh`): iOS does not always report that the keyboard closed (a route change, a return from the background, a page from the back-forward cache), and a page left pinned kept half the screen empty. The page measures again on focus changes (also 400 ms after a field loses the focus, when the keyboard has closed), `visibilitychange`, `pageshow` and window `resize`. iOS Safari and an installed PWA keep the layout viewport when the keyboard opens, so this is what puts the key row's bottom edge on the keyboard's top edge. The bottom safe-area inset is dropped while the keyboard is open. The screen runs from edge to edge between the top row and the key row; buttons are `size-key` high with `space-1` gaps.
- **Frame.** The session's top row gains the size toggle as an icon button. The terminal uses the theme's dark tokens in either scheme ([web-themes.md](web-themes.md)).
- **Font and color.** 11 px on touch devices, 14 px otherwise, in the theme's `font-terminal`. The screen takes `color-bg`, `color-text`, `color-accent` (cursor) and the 16 `color-ansi-*` tokens from the dark tokens.

xterm.js 5.5.0 and its fit addon 0.10.0 are vendored in `crates/wsx-web/web/vendor/` and loaded only when a terminal opens. `scripts/vendor-xterm.sh` rebuilds them from the npm tarballs, checks their SHA-512, and applies one patch: the DOM renderer's `<style>` elements and `style` attributes become CSSOM calls, because the CSP has no `'unsafe-inline'`. License: `web/vendor/xterm.LICENSE` (MIT).

# Terminal fonts

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
