#!/usr/bin/env node
// Real-browser check of the live terminal (docs/web-client.md "Terminal").
// Called by `test-web-client.py --browser` with a shell session whose directory
// holds the fixture programs colors, query, long, alt and keys.
// Usage: node web-terminal-check.mjs <site-url> <pairing-code> <session-id> <capture-dir> <wsxd-socket> <pane-id> <keys-trace>
import fs from "node:fs";
import net from "node:net";
import { launch } from "./web-cdp.mjs";

const [siteUrl, code, sessionId, captures, daemonSocket, paneId, keysTrace] = process.argv.slice(2);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({
  captures, prefix: "terminal-", timeoutSeconds: 170,
});
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

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
  const button = [...document.querySelectorAll('.terminal-keys .terminal-key')].find((b) => b.textContent === ${JSON.stringify(label)});
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
    client.write(`{"method":"hello","params":{"protocol":17}}\n${JSON.stringify({ method, params })}\n`);
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
    await evaluate("document.querySelector('.terminal-notice')?.textContent"))}, console ${JSON.stringify(problems)}`);
}
async function open(width, height, scale) {
  await viewport(width, height, scale);
  // A new view: the touch layout is chosen when the view opens.
  await evaluate("location.hash = '#/settings'");
  await until("!document.querySelector('.terminal')", "the terminal view did not close");
  await evaluate(`location.hash = '#/terminal/${sessionId}'`);
  await live(`terminal did not go live at ${width} px`);
  await until("!!document.querySelector('.xterm-helper-textarea')", "xterm did not open");
}
const focusTerminal = () => evaluate("document.querySelector('.xterm-helper-textarea').focus()");

// --- Pair, then the phone: 390x844, touch, key row.
await viewport(390, 844, 2);
await send("Page.navigate", { url: siteUrl });
await until("!!document.querySelector('#pair-code')", "pairing form did not render");
await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)}; input.form.requestSubmit(); })()`);
await until("!!document.querySelector('#nav a')", "the app did not open after pairing");
await open(390, 844, 2);

const stage = await box(".terminal-stage");
if (stage.x !== 0 || stage.width !== 390) await fail(`terminal is not edge to edge on the phone: ${JSON.stringify(stage)}`);
if (!(await evaluate("document.documentElement.scrollWidth <= innerWidth"))) await fail("phone terminal overflows sideways");
if ((await evaluate("getComputedStyle(document.querySelector('.xterm-rows')).fontSize")) !== "11px") await fail("phone font is not 11 px");
if ((await evaluate("document.querySelectorAll('.terminal-keys .terminal-key').length")) !== 12) await fail("key row does not have 12 keys");
const attributes = await evaluate(`(() => { const t = document.querySelector('.xterm-helper-textarea');
  return ['autocomplete', 'autocorrect', 'autocapitalize', 'spellcheck'].map((name) => t.getAttribute(name)).join(','); })()`);
if (attributes !== "off,off,off,false") await fail(`textarea attributes are ${attributes}`);

// A tap focuses the terminal; a key-row tap must leave the focus there.
await tap(".terminal-stage");
await until("document.activeElement?.classList.contains('xterm-helper-textarea')", "a tap did not focus the terminal");
await type("./colors");
await tap(".terminal-keys .terminal-key:last-child"); // Enter
await until(screenHas("truecolor-end"), "the Enter key of the key row did not run the command");
if (!(await evaluate("document.activeElement?.classList.contains('xterm-helper-textarea')"))) await fail("a key-row tap took the focus from the terminal");
// 256 colours come from the theme stylesheet, truecolour from a cell style: both need the CSP-safe patch.
const colors = await evaluate(`(() => {
  const cells = [...document.querySelectorAll('.xterm-rows span')];
  const background = (cell) => getComputedStyle(cell).backgroundColor;
  return { indexed: cells.some((cell) => cell.classList.contains('xterm-bg-196') && background(cell) === 'rgb(255, 0, 0)'),
           truecolor: cells.some((cell) => background(cell) === 'rgb(18, 52, 86)'),
           screen: getComputedStyle(document.querySelector('.terminal-stage')).backgroundColor }; })()`);
if (!colors.indexed) await fail("256-colour cells are not painted");
if (!colors.truecolor) await fail("truecolour cells are not painted");
if (colors.screen !== "rgb(17, 17, 16)") await fail(`terminal background is ${colors.screen}`);
await capture("phone-colors");

// wsxd's emulator answers a query; xterm.js must not answer it a second time.
await run("./query", "da-replies=");
if (!(await evaluate(screenHas("da-replies=1")))) await fail(`a device-attributes query was not answered exactly once: ${await evaluate(rowsText)}`);

// The pane has the size of this screen.
await run("stty size", "$ stty size");
const rows = await evaluate("document.querySelectorAll('.xterm-rows > div').length");
await until(`${rowsText}.some((row) => new RegExp('^${rows} \\\\d+').test(row))`, `stty size does not report ${rows} rows`);

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
  for (const step of steps) await send("Input.imeSetComposition", { text: step, selectionStart: step.length, selectionEnd: step.length });
  await type(final);
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
await type("Q");
await until(screenHas("keys-done"), "the key recorder did not finish");
const typed = fs.readFileSync(keysTrace).toString("latin1");
const expected = "\x1b\t\x03\x16\x01d\x1a\x12\x0c\x7f\n\r\x7f";
if (!typed.startsWith(expected)) await fail(`key row sent ${JSON.stringify(typed)}`);
const repeats = typed.slice(expected.length, -1);
if (!/^\x01{4,8}$/.test(repeats)) await fail(`a key held for 450 ms sent ${JSON.stringify(repeats)}; expected one press and a repeat every 80 ms`);

// The on-screen keyboard shrinks the visual viewport: the key row stays at its bottom edge and the pane shrinks.
await viewport(390, 500, 2);
await until(`document.querySelectorAll('.xterm-rows > div').length < ${rows}`, "the terminal did not refit to a smaller viewport");
const controls = await box(".terminal-controls");
if (Math.abs(controls.y + controls.height - 500) > 1) await fail(`key row is not at the bottom of the visible viewport: ${JSON.stringify(controls)}`);
const fewer = await evaluate("document.querySelectorAll('.xterm-rows > div').length");
await run("stty size", `${fewer} `);
await sameAsDaemon("after the keyboard resize");
await capture("phone-keyboard-open");

// Host size: the pane keeps its size and the screen is scaled to show it.
await evaluate("document.querySelector('.terminal-size').click()");
await until(`${state("live")} && document.querySelector('.terminal-size').textContent === 'Host size'`, "host size did not connect");
await viewport(390, 844, 2);
await pause(400);
await run("stty size", `${fewer} `);
if ((await evaluate("document.querySelectorAll('.xterm-rows > div').length")) !== fewer) await fail("host size resized the terminal");
await capture("phone-host-size");
await evaluate("document.querySelector('.terminal-size').click()");
await until(`${state("live")} && document.querySelector('.terminal-size').textContent === 'Fit screen'`, "fit screen did not reconnect");
await until(`document.querySelectorAll('.xterm-rows > div').length === ${rows}`, "fit screen did not resize the pane");
await sameAsDaemon("after returning to fit screen");

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
window_.write('{"method":"hello","params":{"protocol":17}}\n');
window_.write(`${JSON.stringify({ method: "terminal_subscribe", params: { pane_id: Number(paneId), client_id: 4242, takeover: true, rows: 24, cols: 80 } })}\n`);
await until(state("elsewhere"), "the page did not notice that a wsx window took the terminal");
if (!(await evaluate("document.querySelector('.terminal-notice').textContent.includes('another wsx window')"))) await fail("no 'controlled elsewhere' bar");
await capture("phone-controlled-elsewhere");
await pause(1200);
if (!(await evaluate(state("elsewhere")))) await fail("the page took the terminal back without being asked");
await evaluate("document.querySelector('.terminal-notice button').click()");
await live("Take control did not reconnect");
for (let attempt = 0; attempt < 50 && !lines.some((line) => line.type === "error"); attempt++) await pause(100);
if (lines.find((line) => line.type === "error")?.data.code !== "lease_transferred") await fail("the wsx window was not displaced");
window_.destroy();
await until(`document.querySelectorAll('.xterm-rows > div').length === ${rows}`, "taking control did not restore this screen's size");

// --- Desktop: 1280x800, no key row, 14 px.
await open(1280, 800, 1);
if ((await evaluate("getComputedStyle(document.querySelector('.xterm-rows')).fontSize")) !== "14px") await fail("desktop font is not 14 px");
if (await evaluate("!!document.querySelector('.terminal-keys')")) await fail("the key row shows on a desktop");
await focusTerminal();
await run("./colors", "truecolor-end");
await capture("desktop-colors");
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

console.log("terminal: phone 390x844 and desktop 1280x800; edge to edge, 256 and true colour under the strict CSP, one query answer, key row bytes and repeat, drag pad, touch scroll in three modes, Korean composition, keyboard viewport, host size, reconnect with replay, control lost and taken");
await cleanup();
process.exit(0);
