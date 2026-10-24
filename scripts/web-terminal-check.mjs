#!/usr/bin/env node
// Real-browser check of the live terminal (docs/web-client-terminal.md).
// Called by `test-web-client.py --browser` with a shell session whose directory
// holds the fixture programs colors, query, long, alt and keys.
// Usage: node web-terminal-check.mjs <site-url> <pairing-code> <session-id> <capture-dir> <wsxd-socket> <pane-id> <keys-trace>
import fs from "node:fs";
import net from "node:net";
import { launch, pause } from "./web-cdp.mjs";

const [siteUrl, code, sessionId, captures, daemonSocket, paneId, keysTrace] = process.argv.slice(2);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({
  captures, prefix: "terminal-", timeoutSeconds: 170,
});
// wsxd's protocol version, read from its source (crates/wsx-core/src/runtime/protocol.rs).
const PROTOCOL = Number(/PROTOCOL_VERSION: u32 = (\d+);/.exec(fs.readFileSync(new URL("../crates/wsx-core/src/runtime/protocol.rs", import.meta.url), "utf8"))[1]);
const HELLO = `${JSON.stringify({ method: "hello", params: { protocol: PROTOCOL } })}\n`;

// Record the page's WebSockets so a lost connection can be forced from outside.
await send("Page.addScriptToEvaluateOnNewDocument", { source: `(() => {
  const Native = WebSocket;
  window.__sockets = [];
  window.WebSocket = class extends Native { constructor(...args) { super(...args); window.__sockets.push(this); } };
})()` });

const rowsText = "[...document.querySelectorAll('.xterm-rows > div')].map((row) => row.textContent)";
const screenHas = (text) => `${rowsText}.some((row) => row.includes(${JSON.stringify(text)}))`;
const state = (name) => `document.querySelector('.terminal')?.dataset.state === ${JSON.stringify(name)}`;
const box = (selector) => evaluate(`(() => { const r = document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect();
  return { x: r.x, y: r.y, width: r.width, height: r.height }; })()`);
const middle = async (selector) => { const r = await box(selector); return [r.x + r.width / 2, r.y + r.height / 2]; };

async function touch(type, points) {
  await send("Input.dispatchTouchEvent", { type, touchPoints: points.map(([x, y]) => ({ x, y })) });
}
async function tap(selector) {
  // The key row scrolls sideways on a narrow phone: bring the target into view first.
  await evaluate(`document.querySelector(${JSON.stringify(selector)}).scrollIntoView({ inline: "center", block: "nearest" })`);
  const point = await middle(selector);
  await touch("touchStart", [point]);
  await touch("touchEnd", []);
}
// A finger moved from one point to another in small steps.
async function drag(from, to) {
  await touch("touchStart", [from]);
  for (let step = 1; step <= 10; step++) {
    await touch("touchMove", [[from[0] + ((to[0] - from[0]) * step) / 10, from[1] + ((to[1] - from[1]) * step) / 10]]);
  }
  await touch("touchEnd", []);
}
async function key(name, keyCode, text) {
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: name, code: name, windowsVirtualKeyCode: keyCode, text });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: name, code: name, windowsVirtualKeyCode: keyCode });
}
const type = (text) => send("Input.insertText", { text });
const enter = () => key("Enter", 13, "\r");
// One press and release of a key-row button, shorter than its repeat interval.
const press = (label) => evaluate(`(() => {
  const button = [...document.querySelectorAll('.terminal-controls .terminal-key')].find((b) => b.textContent === ${JSON.stringify(label)});
  button.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
  button.dispatchEvent(new PointerEvent('pointerup', { bubbles: true }));
})()`);
async function run(command, marker) {
  await type(command);
  await enter();
  await until(screenHas(marker), `${command} did not print ${marker}`);
}
// One request to wsxd, as any local client makes it.
function daemon(method, params) {
  return new Promise((resolve, reject) => {
    const client = net.connect(daemonSocket);
    let text = "";
    client.on("error", reject);
    client.on("data", (chunk) => {
      text += chunk;
      const lines = text.split("\n");
      if (lines.length > 2) {
        client.destroy();
        resolve(JSON.parse(lines[1]).data);
      }
    });
    client.write(`${HELLO}${JSON.stringify({ method, params })}\n`);
  });
}
// The browser's screen must be the screen wsxd holds, row for row.
async function sameAsDaemon(label) {
  let browser, authority;
  for (let attempt = 0; attempt < 40; attempt++) {
    const frame = (await daemon("view", { pane_ids: [Number(paneId)] })).frames[0];
    authority = [];
    for (let row = 0; row < frame.rows; row++) {
      authority.push(frame.cells.slice(row * frame.cols, (row + 1) * frame.cols).map((cell) => cell[0]).join("").trimEnd());
    }
    browser = (await evaluate(rowsText)).map((row) => row.trimEnd());
    if (JSON.stringify(browser) === JSON.stringify(authority)) return;
    await pause(100);
  }
  const row = authority.findIndex((text, index) => text !== browser[index]);
  await fail(`${label}: the browser screen differs from wsxd at row ${row + 1} of ${authority.length} (browser has ${browser.length}): wsxd ${JSON.stringify(authority.slice(Math.max(0, row - 1), row + 3))}, browser ${JSON.stringify(browser.slice(Math.max(0, row - 1), row + 3))}`);
}

// Wait for the live state; a failure names the state and the notice the reader would see.
async function live(label) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await evaluate(state("live")).catch(() => false)) return;
    await pause(100);
  }
  await fail(`${label}: at ${await evaluate("location.href")}, state ${await evaluate("document.querySelector('.terminal')?.dataset.state")}, notice ${JSON.stringify(
    await evaluate("document.querySelector('.terminal-notice')?.textContent"))}, console ${JSON.stringify(problems)}, page ${JSON.stringify(
    await evaluate("document.querySelector('#app').innerText.slice(0, 300)"))}`);
}
// The on-screen keyboard the way iOS reports it: `height` and `top` of the visual viewport
// change and the layout viewport stays. `null` closes the keyboard.
const keyboard = (height, top = 0) => evaluate(`(() => {
  const viewport = window.visualViewport;
  delete viewport.height;
  delete viewport.offsetTop;
  if (${height} !== null) {
    Object.defineProperty(viewport, "height", { configurable: true, get: () => ${height} });
    Object.defineProperty(viewport, "offsetTop", { configurable: true, get: () => ${top} });
  }
  viewport.dispatchEvent(new Event("resize"));
  viewport.dispatchEvent(new Event("scroll"));
})()`);
// The view fills the visual viewport exactly: top row at its top, key row at its bottom
// (the keyboard's top edge), the screen between them with no gap, and no page scroll.
async function aboveKeyboard(height, top, label) {
  await pause(150);
  const seen = await evaluate(`(() => {
    const rect = (selector) => document.querySelector(selector).getBoundingClientRect();
    const [bar, stage, controls] = ['.bar', '.terminal-stage', '.terminal-controls'].map(rect);
    return { barTop: bar.top, barLeft: bar.left, barWidth: bar.width, gapTop: stage.top - bar.bottom, gapBottom: controls.top - stage.bottom,
      stageLeft: stage.left, stageWidth: stage.width, controlsBottom: controls.bottom, innerWidth, scrollY, scrollX,
      scrollable: document.documentElement.scrollHeight - document.documentElement.clientHeight,
      wide: document.documentElement.scrollWidth - innerWidth }; })()`);
  const wrong = Math.abs(seen.barTop - top) > 1 || Math.abs(seen.controlsBottom - (top + height)) > 1 || Math.abs(seen.gapTop) > 0.5 || Math.abs(seen.gapBottom) > 0.5
    || seen.stageLeft !== 0 || seen.stageWidth !== seen.innerWidth || seen.barLeft !== 0 || seen.scrollY !== 0 || seen.scrollX !== 0 || seen.scrollable > 0 || seen.wide > 0;
  if (wrong) await fail(`${label}: the view does not fill the visual viewport (top ${top}, height ${height}): ${JSON.stringify(seen)}`);
}
// The shipped fonts: Powerline separators and Nerd Font icons from the symbols font,
// ligatures from JetBrains Mono, and a grid that still matches the pane.
async function glyphs(name) {
  await run("printf '\\033[2J\\033[H'; ./glyphs", "glyphs-end");
  const seen = await evaluate(`(() => {
    const rows = [...document.querySelectorAll('.xterm-rows > div')];
    const cell = document.querySelector('.xterm-char-measure-element').getBoundingClientRect().width / 32;
    // Where one character of a row is drawn.
    const box = (row, needle) => {
      const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
      for (let node; (node = walker.nextNode());) {
        const at = node.nodeValue.indexOf(needle);
        if (at < 0) continue;
        const range = document.createRange();
        range.setStart(node, at);
        range.setEnd(node, at + needle.length);
        return range.getBoundingClientRect();
      }
      return null;
    };
    const row = (text) => rows.find((candidate) => candidate.textContent.includes(text));
    const left = rows[0].getBoundingClientRect().left;
    const column = (rect) => rect && (rect.left - left) / cell;
    const joined = [...row('=> !=').querySelectorAll('span')].map((span) => span.textContent).filter((text) => /^[-=<>!|&:+*/.#?~^%w]{2,}$/.test(text));
    return {
      family: getComputedStyle(document.querySelector('.xterm-rows')).fontFamily,
      loaded: [...document.fonts].filter((face) => face.status === 'loaded').map((face) => face.family.replaceAll('"', '') + ' ' + face.weight + ' ' + face.style).sort(),
      powerline: (() => { const rect = box(row('main'), '\\ue0b0'); return rect && [rect.width / cell, rect.height > 0]; })(),
      icon: (() => { const rect = box(row('main'), '\\uf015'); return rect && rect.width / cell; })(),
      joined, arrow: (() => { const rect = box(row('=> !='), '=>'); return rect && rect.width / cell; })(),
      ligatures: getComputedStyle(row('=> !=').querySelector('span')).fontVariantLigatures,
      spacing: [document.querySelector('.xterm-rows').style.letterSpacing, ...[...row('=> !=').querySelectorAll('span')].map((span) => span.style.letterSpacing)],
      line: (() => { const rect = row('=> !=').getBoundingClientRect(); return { x: rect.left, y: rect.top, width: cell * 30, height: rect.height }; })(),
      korean: [column(box(row('\\ud55c\\uae00|'), '|')), column(box(row('abcd|'), '|'))],
      box: [column(box(row('\\u2502ab'), 'ab')), column(box(row('\\u2502ab'), 'cd'))],
      bold: getComputedStyle([...row('bold text').querySelectorAll('span')].find((span) => span.textContent.includes('bold'))).fontWeight,
      italic: getComputedStyle([...row('bold text').querySelectorAll('span')].find((span) => span.textContent.includes('italic'))).fontStyle,
    };
  })()`);
  const near = (value, expected) => typeof value === "number" && Math.abs(value - expected) < 0.06;
  if (!seen.family.startsWith('"JetBrains Mono", "Symbols Nerd Font Mono"')) await fail(`the terminal font stack is ${seen.family}`);
  for (const face of ["JetBrains Mono 400 normal", "JetBrains Mono 700 normal", "JetBrains Mono 400 italic", "Symbols Nerd Font Mono 400 normal"]) {
    if (!seen.loaded.includes(face)) await fail(`the font ${face} is not loaded: ${seen.loaded}`);
  }
  if (!seen.powerline || !near(seen.powerline[0], 1) || !near(seen.icon, 1)) await fail(`a Powerline separator or an icon does not fill one cell: ${JSON.stringify(seen)}`);
  if (!["=>", "!=", "->", "===", "<="].every((run) => seen.joined.includes(run)) || !near(seen.arrow, 2) || seen.ligatures === "none") await fail(`ligature runs are not drawn as one piece of two cells: ${JSON.stringify(seen)}`);
  if (!near(seen.korean[0], 4) || !near(seen.korean[1], 4)) await fail(`wide characters are not aligned to the grid: ${JSON.stringify(seen.korean)}`);
  if (!near(seen.box[0], 1) || !near(seen.box[1], 4)) await fail(`box drawing is not aligned to the grid: ${JSON.stringify(seen.box)}`);
  if (seen.bold !== "700" || seen.italic !== "italic") await fail(`bold or italic is not drawn with its own face: ${JSON.stringify(seen)}`);
  await capture(`${name}-glyphs`);
  // Proof that the font joins the runs: the same line, enlarged, with the font's ligatures
  // on and off. Only the font feature differs, so equal pictures mean nothing was joined.
  const clip = { ...seen.line, scale: 4 };
  const on = await capture(`${name}-ligatures-on`, clip);
  await evaluate("document.querySelector('.terminal-host').dataset.ligatures = 'off'");
  const off = await capture(`${name}-ligatures-off`, clip);
  await evaluate("delete document.querySelector('.terminal-host').dataset.ligatures");
  if (on.equals(off)) await fail(`the font does not join the operator runs: the line looks the same with ligatures off; letter spacing ${JSON.stringify(seen.spacing)}`);
  // The grid still matches the pane: a line of exactly as many characters as stty reports
  // columns fills one row and does not wrap.
  await run("printf '\\033[2J\\033[H'; c=$(stty size | cut -d' ' -f2); printf '%*s' $c '' | tr ' ' '#'; echo; echo cols=$c", "cols=");
  // ^ The typed command has "cols=" too: wait for its output, the ruler and the count.
  await until(`(() => { const rows = [...document.querySelectorAll('.xterm-rows > div')].map((row) => row.textContent.trimEnd());
    return rows.some((row) => /^#+$/.test(row)) && rows.some((row) => /^cols=\\d+/.test(row)); })()`, "the ruler and its column count did not draw");
  const grid = await evaluate(`(() => {
    const rows = [...document.querySelectorAll('.xterm-rows > div')];
    const rulers = rows.filter((row) => /^#+$/.test(row.textContent.trimEnd()));
    const range = document.createRange();
    range.selectNodeContents(rulers[0]);
    const drawn = range.getBoundingClientRect();
    const length = rulers[0].textContent.trimEnd().length;
    return { rulers: rulers.length, length, cols: Number(rows.map((row) => row.textContent).find((row) => /^cols=\\d+/.test(row)).slice(5)),
      cell: drawn.width / length, room: document.querySelector('.terminal-stage').getBoundingClientRect().right - drawn.right }; })()`);
  // One more column would not fit beside the ruler (a desktop also keeps room for the scrollbar).
  if (grid.rulers !== 1 || grid.length !== grid.cols || grid.room < -0.5 || grid.room >= grid.cell + 16) await fail(`the pane's columns do not match the visible grid: ${JSON.stringify(grid)}`);
}
async function open(width, height, scale) {
  await viewport(width, height, scale);
  // A new view: the touch layout is chosen when the view opens.
  await evaluate("location.hash = '#/settings'");
  await until("!document.querySelector('.terminal')", "the terminal view did not close");
  await evaluate(`location.hash = '#/s/${sessionId}/terminal'`);
  await live(`terminal did not go live at ${width} px`);
  await until("!!document.querySelector('.xterm-helper-textarea')", "xterm did not open");
}
const focusTerminal = () => evaluate("document.querySelector('.xterm-helper-textarea').focus()");

// --- Pair, then the phone: 390x844, touch, key row.
await viewport(390, 844, 2);
await send("Page.navigate", { url: siteUrl });
await until("!!document.querySelector('#pair-code')", "pairing form did not render");
await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)}; input.form.requestSubmit(); })()`);
await until("!!document.querySelector('.app')", "the app did not open after pairing");
await open(390, 844, 2);

// The frame: the session's one top row, then the terminal down to the key row, all dark.
const frame = await evaluate(`(() => {
  const color = (selector) => getComputedStyle(document.querySelector(selector)).backgroundColor;
  const keys = document.querySelector('.terminal-keys');
  return { bar: document.querySelector('.bar').getBoundingClientRect().height, barTop: document.querySelector('.bar').getBoundingClientRect().top,
    tabbar: !!document.querySelector('.tabbar'), page: color('body'), surface: color('.terminal'), key: color('.terminal-key'),
    keyHeight: document.querySelector('.terminal-key').getBoundingClientRect().height, pad: document.querySelector('.terminal-pad').getBoundingClientRect().height,
    scrolled: keys.scrollLeft, first: keys.firstElementChild.textContent, more: keys.dataset.more,
    switch: [...document.querySelectorAll('.switch a')].map((a) => a.textContent.trim() + (a.getAttribute('aria-current') ? '*' : '')).join(),
    toggle: document.querySelector('.bar .terminal-size').getAttribute('aria-label'), toggleText: document.querySelector('.terminal-size').textContent };
})()`);
if (frame.bar > 48 || frame.barTop !== 0 || frame.tabbar) await fail(`the session top row is not the only frame above the terminal: ${JSON.stringify(frame)}`);
if (frame.page !== "rgb(10, 11, 13)" || frame.surface !== "rgb(10, 11, 13)" || frame.key !== "rgb(26, 29, 34)") await fail(`the frame is not dark: ${JSON.stringify(frame)}`);
if (frame.keyHeight !== 36 || frame.pad !== 36) await fail(`key row buttons are not 36 px: ${JSON.stringify(frame)}`);
if (frame.scrolled !== 0 || frame.first !== "Esc" || frame.more !== "end") await fail(`the key row does not start at Esc with a fade at its end: ${JSON.stringify(frame)}`);
// ^ This session is a shell: it has no chat, so there is nothing to switch to.
if (frame.switch !== "" || !/^Fit screen/.test(frame.toggle) || frame.toggleText !== "") await fail(`the top row's switch or size toggle is wrong: ${JSON.stringify(frame)}`);

const stage = await box(".terminal-stage");
if (stage.x !== 0 || stage.width !== 390) await fail(`terminal is not edge to edge on the phone: ${JSON.stringify(stage)}`);
if (!(await evaluate("document.documentElement.scrollWidth <= innerWidth"))) await fail("phone terminal overflows sideways");
if ((await evaluate("getComputedStyle(document.querySelector('.xterm-rows')).fontSize")) !== "11px") await fail("phone font is not 11 px");
if ((await evaluate("document.querySelectorAll('.terminal-keys .terminal-key').length")) !== 10) await fail("key row does not have 10 keys");
// Enter at the right edge of the second row, ^J just left of it, on Paste's line.
const lineKeys = await evaluate(`(() => {
  const box = (node) => node.getBoundingClientRect();
  const [newline, enter] = document.querySelectorAll('.terminal-line-keys .terminal-key');
  const paste = [...document.querySelectorAll('.terminal-tools .terminal-key')].find((button) => button.textContent === 'Paste');
  return { labels: [newline.textContent, enter.textContent].join(), right: innerWidth - box(enter).right, gap: box(enter).left - box(newline).right,
    top: box(enter).top, newlineTop: box(newline).top, pasteTop: paste ? box(paste).top : null, pasteLeft: paste ? box(paste).left : null,
    width: box(enter).width, height: box(enter).height, row: [...document.querySelectorAll('.terminal-keys .terminal-key')].map((key) => key.textContent).join() };
})()`);
if (lineKeys.labels !== "^J,Enter" || lineKeys.right > 4.5 || lineKeys.gap !== 4 || lineKeys.top !== lineKeys.newlineTop || lineKeys.height !== 36 || lineKeys.width < 36
  || (lineKeys.pasteTop !== null && (lineKeys.pasteTop !== lineKeys.top || lineKeys.pasteLeft > 4.5)) || /\^J|Enter/.test(lineKeys.row)) await fail(`^J and Enter are not at the right of the second row: ${JSON.stringify(lineKeys)}`);
const attributes = await evaluate(`(() => { const t = document.querySelector('.xterm-helper-textarea');
  return ['autocomplete', 'autocorrect', 'autocapitalize', 'spellcheck'].map((name) => t.getAttribute(name)).join(','); })()`);
if (attributes !== "off,off,off,false") await fail(`textarea attributes are ${attributes}`);
// No zoom: the hidden input field has a 16 px font (iOS zooms below that). touch-action on the
// frame applies to everything inside it, and the screen, key row and pad narrow it further.
const steady = await evaluate(`(() => { const action = (selector) => getComputedStyle(document.querySelector(selector)).touchAction;
  const field = document.querySelector('.xterm-helper-textarea'); field.focus();
  return { font: parseFloat(getComputedStyle(field).fontSize), scale: visualViewport.scale, left: document.scrollingElement.scrollLeft,
    stage: action('.terminal-stage'), keys: action('.terminal-keys'), key: action('.terminal-key'), pad: action('.terminal-pad'), frame: action('.terminal') }; })()`);
if (steady.font < 16 || steady.scale !== 1 || steady.left !== 0 || steady.stage !== "none" || steady.keys !== "pan-x" || steady.pad !== "none" || steady.frame !== "manipulation") await fail(`the terminal can zoom: ${JSON.stringify(steady)}`);

// A tap focuses the terminal; a key-row tap must leave the focus there.
await tap(".terminal-stage");
await until("document.activeElement?.classList.contains('xterm-helper-textarea')", "a tap did not focus the terminal");
// Even a blur that the terminal takes back makes the page jump off the keyboard and back.
await evaluate("window.__blurs = 0; document.activeElement.addEventListener('blur', () => { window.__blurs += 1; })");
await tap(".terminal-line-keys .terminal-key:first-child"); // ^J, an empty line for the shell
await tap(".terminal-keys .terminal-key:last-child"); // ⌫
await type("./colors");
await tap(".terminal-line-keys .terminal-key:last-child"); // Enter
await until(screenHas("truecolor-end"), "the Enter key did not run the command");
if (!(await evaluate("document.activeElement?.classList.contains('xterm-helper-textarea')"))) await fail("a key-row tap took the focus from the terminal");
if (await evaluate("window.__blurs")) await fail(`key-row taps blurred the terminal ${await evaluate("window.__blurs")} times`);
// 256 colours come from the theme stylesheet, truecolour from a cell style: both need the CSP-safe patch.
const colors = await evaluate(`(() => {
  const cells = [...document.querySelectorAll('.xterm-rows span')];
  const background = (cell) => getComputedStyle(cell).backgroundColor;
  return { indexed: cells.some((cell) => cell.classList.contains('xterm-bg-196') && background(cell) === 'rgb(255, 0, 0)'),
           truecolor: cells.some((cell) => background(cell) === 'rgb(18, 52, 86)'),
           screen: getComputedStyle(document.querySelector('.terminal-stage')).backgroundColor }; })()`);
if (!colors.indexed) await fail("256-colour cells are not painted");
if (!colors.truecolor) await fail("truecolour cells are not painted");
if (colors.screen !== "rgb(10, 11, 13)") await fail(`terminal background is ${colors.screen}`);
await capture("phone-colors");

// wsxd's emulator answers a query; xterm.js must not answer it a second time.
await run("./query", "da-replies=");
if (!(await evaluate(screenHas("da-replies=1")))) await fail(`a device-attributes query was not answered exactly once: ${await evaluate(rowsText)}`);

// The pane has the size of this screen.
await run("stty size", "$ stty size");
const rows = await evaluate("document.querySelectorAll('.xterm-rows > div').length");
await until(`${rowsText}.some((row) => new RegExp('^${rows} \\\\d+').test(row))`, `stty size does not report ${rows} rows`);

await glyphs("phone");

// Long output, then touch scroll through the local scrollback.
await run("./long", "long-end");
await sameAsDaemon("after long output");
const center = await middle(".terminal-stage");
await drag(center, [center[0], center[1] + 280]);
await until(`!${screenHas("long-end")} && ${screenHas("line-2")}`, "a touch drag did not scroll the scrollback");
await capture("phone-scrollback");
await drag(center, [center[0], center[1] - 400]);
await until(screenHas("long-end"), "scrolling back down did not reach the end");

// Alternate screen: a touch drag sends arrow keys; with mouse tracking it sends wheel reports.
await type("./alt");
await press("Enter");
await until(screenHas("ALT SCREEN"), "the alternate-screen program did not start");
await drag(center, [center[0], center[1] + 90]);
await until(`${rowsText}.some((row) => /up=[1-9]/.test(row))`, "a touch drag on the alternate screen did not send Up keys");
// Drag pad: one arrow key per 18 px.
const pad = await middle(".terminal-pad");
await evaluate(`(() => { const pad = document.querySelector('.terminal-pad'); const at = (type, x, buttons) =>
  pad.dispatchEvent(new PointerEvent(type, { bubbles: true, cancelable: true, pointerId: 1, clientX: x, clientY: ${pad[1]}, buttons }));
  pad.setPointerCapture = () => {}; at('pointerdown', ${pad[0]}, 1); at('pointermove', ${pad[0]} + 40, 1); at('pointerup', ${pad[0]} + 40, 0); })()`);
await until(screenHas("right=2"), "a 40 px drag on the pad did not send two Right keys");
await type("m");
await until(screenHas("mouse=on"), "mouse tracking did not turn on");
await drag(center, [center[0], center[1] + 90]);
await until(`${rowsText}.some((row) => /wheel-up=[1-9]/.test(row))`, "a touch drag with mouse tracking did not send wheel reports");
await capture("phone-alt-screen");
await type("q");
await until(screenHas("alt-done"), "the alternate-screen program did not exit");

// Korean through the textarea, composed the way an IME does it.
await type("echo ");
for (const [steps, final] of [[["ㅎ", "하", "한"], "한"], [["ㄱ", "그", "글"], "글"]]) {
  for (const step of steps) {
    await send("Input.imeSetComposition", { text: step, selectionStart: step.length, selectionEnd: step.length });
    await pause(40);
  }
  await type(final);
  await pause(40);
}
await type(" 입력");
await press("Enter");
await until(`${rowsText}.some((row) => row.trim() === '한글 입력')`, "Korean text did not round-trip");
await capture("phone-korean");

// Every key of the key row sends its byte once; a held key repeats; Backspace is sent once.
await run("./keys", "keys-ready");
for (const label of ["Esc", "Tab", "^C", "^V", "^A", "D", "^Z", "^R", "^L", "⌫", "^J", "Enter"]) await press(label);
await key("Backspace", 8);
await evaluate(`[...document.querySelectorAll('.terminal-keys .terminal-key')].find((b) => b.textContent === '^A')
  .dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }))`);
await pause(450);
await evaluate(`[...document.querySelectorAll('.terminal-keys .terminal-key')].find((b) => b.textContent === '^A')
  .dispatchEvent(new PointerEvent('pointerup', { bubbles: true }))`);
// A held Enter is one Enter.
const enterKey = "document.querySelector('.terminal-line-keys .terminal-key:last-child')";
await evaluate(`${enterKey}.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }))`);
await pause(450);
await evaluate(`${enterKey}.dispatchEvent(new PointerEvent('pointerup', { bubbles: true }))`);
await type("Q");
await until(screenHas("keys-done"), "the key recorder did not finish");
const typed = fs.readFileSync(keysTrace).toString("latin1");
const expected = "\x1b\t\x03\x16\x01d\x1a\x12\x0c\x7f\n\r\x7f";
if (!typed.startsWith(expected)) await fail(`key row sent ${JSON.stringify(typed)}`);
const repeats = typed.slice(expected.length, -2);
if (!/^\x01{4,8}$/.test(repeats)) await fail(`a key held for 450 ms sent ${JSON.stringify(repeats)}; expected one press and a repeat every 80 ms`);
if (typed.slice(-2, -1) !== "\r" || typed.slice(expected.length).split("\r").length !== 2) await fail(`a held Enter sent ${JSON.stringify(typed.slice(expected.length))}; expected one Enter`);

// The on-screen keyboard as iOS shows it: the visual viewport shrinks, the layout viewport does not.
// The key row's bottom edge is the keyboard's top edge, and the pane is refit to what is left.
await keyboard(508);
await until(`document.querySelectorAll('.xterm-rows > div').length < ${rows}`, "the terminal did not refit above the keyboard");
await aboveKeyboard(508, 0, "keyboard open at 390x844");
const fewer = await evaluate("document.querySelectorAll('.xterm-rows > div').length");
// iOS may also move the visual viewport down the page: the view follows it.
await keyboard(508, 70);
await aboveKeyboard(508, 70, "keyboard open and the viewport moved");
await keyboard(508);
await run("stty size", `${fewer} `);
await sameAsDaemon("after the keyboard resize");
await capture("phone-keyboard-open");
await evaluate("document.documentElement.dataset.scheme = 'light'");
await capture("phone-keyboard-open-light");
await evaluate("delete document.documentElement.dataset.scheme");

// Host size: the pane keeps its size and the screen is scaled to show it.
await evaluate("document.querySelector('.terminal-size').click()");
await until(`${state("live")} && document.querySelector('.terminal-size').dataset.size === 'host'`, "host size did not connect");
await keyboard(null);
await pause(400);
await aboveKeyboard(844, 0, "keyboard closed");
await run("stty size", `${fewer} `);
if ((await evaluate("document.querySelectorAll('.xterm-rows > div').length")) !== fewer) await fail("host size resized the terminal");
await capture("phone-host-size");
await evaluate("document.querySelector('.terminal-size').click()");
await until(`${state("live")} && document.querySelector('.terminal-size').dataset.size === 'fit'`, "fit screen did not reconnect");
await until(`document.querySelectorAll('.xterm-rows > div').length === ${rows}`, "fit screen did not resize the pane");
await sameAsDaemon("after returning to fit screen");

// Scrolled back from the newest output, one tap returns to it (docs/web-client-terminal.md).
await run("i=0; while [ $i -lt 120 ]; do echo scroll-line-$i; i=$((i+1)); done; echo scroll-end", "scroll-end");
const jumpHidden = "document.querySelector('.terminal .jump').hidden";
if (!(await evaluate(jumpHidden))) await fail("Jump to latest shows at the end of the output");
await evaluate("document.querySelector('.terminal .xterm').dispatchEvent(new WheelEvent('wheel', { deltaY: -2000, deltaMode: 0, bubbles: true, cancelable: true }))");
await until(`!${jumpHidden}`, "Jump to latest did not show after a scroll back");
await capture("phone-jump");
await evaluate("document.querySelector('.terminal .jump').click()");
await until(`${jumpHidden} && (() => { const viewport = document.querySelector('.xterm-viewport'); return viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < 2; })()`,
  "Jump to latest did not return to the end");

// A lost connection: the page reconnects by itself and the replay restores the screen where it was.
await run("printf '\\033[2J\\033[5;3Hreplay-marker\\n'", "replay-marker");
await sameAsDaemon("before the connection is lost");
await evaluate("window.__sockets.at(-1).close()");
await until("window.__sockets.length > 1 && window.__sockets.at(-1).readyState === 1", "the page did not reconnect");
await live("the terminal did not go live again");
await sameAsDaemon("after the replay");
if (!(await evaluate(`${rowsText}[4]?.startsWith('  replay-marker')`))) await fail(`the replay did not restore row 5: ${JSON.stringify(await evaluate(rowsText))}`);
await run("echo after-reconnect", "after-reconnect");

// A wsx window takes the terminal: the page shows it and takes control back only on request.
const window_ = net.connect(daemonSocket);
const lines = [];
let buffered = "";
window_.on("data", (chunk) => {
  buffered += chunk;
  const parts = buffered.split("\n");
  buffered = parts.pop();
  lines.push(...parts.map((line) => JSON.parse(line)));
});
window_.write(HELLO);
window_.write(`${JSON.stringify({ method: "terminal_subscribe", params: { pane_id: Number(paneId), client_id: 4242, takeover: true, rows: 24, cols: 80 } })}\n`);
await until(state("elsewhere"), "the page did not notice that a wsx window took the terminal");
if (!(await evaluate("document.querySelector('.terminal-notice').textContent.includes('another wsx window')"))) await fail("no 'controlled elsewhere' bar");
const elsewhere = await evaluate(`(() => { const bar = document.querySelector('.terminal-notice');
  return { height: bar.getBoundingClientRect().height, color: getComputedStyle(bar).backgroundColor }; })()`);
if (elsewhere.height > 44 || elsewhere.color !== "rgb(18, 20, 23)") await fail(`the 'controlled elsewhere' bar is not one dark row: ${JSON.stringify(elsewhere)}`);
await capture("phone-controlled-elsewhere");
await pause(1200);
if (!(await evaluate(state("elsewhere")))) await fail("the page took the terminal back without being asked");
await evaluate("document.querySelector('.terminal-notice button').click()");
await live("Take control did not reconnect");
for (let attempt = 0; attempt < 50 && !lines.some((line) => line.type === "error"); attempt++) await pause(100);
if (lines.find((line) => line.type === "error")?.data.code !== "lease_transferred") await fail("the wsx window was not displaced");
window_.destroy();
await until(`document.querySelectorAll('.xterm-rows > div').length === ${rows}`, "taking control did not restore this screen's size");

// Opening the terminal is the person's own action: it takes the pane from a wsx window.
const holder = net.connect(daemonSocket);
let holderText = "";
holder.on("data", (chunk) => { holderText += chunk; });
holder.write(HELLO);
holder.write(`${JSON.stringify({ method: "terminal_subscribe", params: { pane_id: Number(paneId), client_id: 4243, takeover: true, rows: 24, cols: 80 } })}\n`);
await until(state("elsewhere"), "the page did not notice the second wsx window");
await open(390, 844, 2);
for (let attempt = 0; attempt < 50 && !holderText.includes('"lease_transferred"'); attempt++) await pause(100);
if (!holderText.includes('"lease_transferred"')) await fail("opening the terminal did not take the pane from the wsx window");
holder.destroy();
await until(`document.querySelectorAll('.xterm-rows > div').length === ${rows}`, "opening the terminal did not restore this screen's size");

// Rotated: the same view refits to the landscape viewport.
await send("Emulation.setDeviceMetricsOverride", { width: 844, height: 390, deviceScaleFactor: 2, mobile: true });
await until(`document.querySelectorAll('.xterm-rows > div').length < ${rows}`, "the terminal did not refit after rotating");
await aboveKeyboard(390, 0, "landscape");
await capture("phone-landscape");

// The smallest phone, 320x568: nothing scrolls sideways, and the keyboard leaves a usable screen.
await open(320, 568, 2);
if (!(await evaluate("document.querySelector('.bar').scrollWidth <= innerWidth"))) await fail("the top row overflows sideways at 320 px");
const narrow = await evaluate(`(() => { const box = (selector) => document.querySelector(selector).getBoundingClientRect();
  return { enter: innerWidth - box('.terminal-line-keys .terminal-key:last-child').right, pad: box('.terminal-pad').width,
    padLeft: box('.terminal-pad').left, padRight: box('.terminal-pad').right, keys: box('.terminal-line-keys').left }; })()`);
if (narrow.enter < 0 || narrow.enter > 4.5 || narrow.padRight > narrow.keys || narrow.pad < 100) await fail(`the second row does not fit at 320 px: ${JSON.stringify(narrow)}`);
await aboveKeyboard(568, 0, "320x568");
await capture("phone-320");
// The keyboard opens for a focused terminal: the page is pinned to the visual viewport only then.
await focusTerminal();
await keyboard(315);
await pause(300);
await aboveKeyboard(315, 0, "keyboard open at 320x568");
if ((await evaluate("document.querySelectorAll('.xterm-rows > div').length")) < 8) await fail("the keyboard leaves fewer than 8 terminal rows at 320x568");
await capture("phone-320-keyboard-open");
await keyboard(null);

// --- Desktop: 1280x800, visible tabs, no key row, 14 px.
await open(1280, 800, 1);
if (!(await evaluate("!!document.querySelector('.rail')"))) await fail("the desktop has no rail beside the terminal");
if ((await evaluate("getComputedStyle(document.querySelector('.xterm-rows')).fontSize")) !== "14px") await fail("desktop font is not 14 px");
if (await evaluate("!!document.querySelector('.terminal-keys')")) await fail("the key row shows on a desktop");
await focusTerminal();
await run("./colors", "truecolor-end");
await capture("desktop-colors");
await glyphs("desktop");
await run("./long", "long-end");
const desktop = await middle(".terminal-stage");
await send("Input.dispatchMouseEvent", { type: "mouseWheel", x: desktop[0], y: desktop[1], deltaX: 0, deltaY: -900 });
await until(`!${screenHas("long-end")}`, "the mouse wheel did not scroll the scrollback");
await capture("desktop-scrollback");
await type("./alt");
await enter();
await until(screenHas("ALT SCREEN"), "the alternate-screen program did not start on the desktop");
await key("ArrowUp", 38);
await until(screenHas("up=1"), "the Up key did not reach the program");
await capture("desktop-alt-screen");
await type("q");
await until(screenHas("alt-done"), "the alternate-screen program did not exit on the desktop");
await run("echo 한글 입력 desktop", "한글 입력 desktop");
await sameAsDaemon("on the desktop");
await capture("desktop-korean");

const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);

console.log("terminal: phone 390x844 and desktop 1280x800; edge to edge, 256 and true colour under the strict CSP, one query answer, key row bytes and repeat, drag pad, touch scroll in three modes, Korean composition, keyboard viewport, host size, reconnect with replay, control lost and taken back, opening takes it from a wsx window");
await cleanup();
process.exit(0);
