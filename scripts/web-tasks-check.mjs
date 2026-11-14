#!/usr/bin/env node
// Real-browser check of manager mode on the page (docs/web-client-tasks.md), driven over the
// Chrome DevTools Protocol. Called by `test-web-client.py --part tasks --browser`.
//   web-tasks-check.mjs <site-url> <code> <project-id> <capture-dir> <home order> <slop-audit.js|""> <plans-id> <restart-file> <plans-checkout> <long-name> <relay-file> <wsx-cli.json> <news-file>
// <home order> is the project names as home must list them, comma-separated. <plans-id> is a
// project whose #1 waits for the owner's answer, whose #2 is ready to land and writes <long-name>,
// and whose last deploy failed; <plans-checkout> is its main checkout.
// The project lands by hand and has: #1 and #4 waiting for a host, #2 ready with an item to
// check, #3 ready, #5 closed after a long report the manager heard of, #6 working (its prompt
// opens with "Owner asked:"), #7 and #8 landed with an item each to check, #7 with an image capture and #8 with a video (#7's summary has two
// sentences), and two pending approvals: a1 lands #3, a2 discards #4 (shown as
// "Approve land #3", "Approve discard #4"). Writing <restart-file> asks the test to restart
// the server with another site name, so it serves another build; it writes <restart-file>.done.
// Each capture is saved with the slop audit and, on a phone, the mobile audit of
// /ui-view-standards beside it (scripts/mobile-audit.js, or the file WSX_MOBILE_AUDIT names).
// The project's plain git worktree `old-work`, which no task owns, has a done agent session
// ("old chat"); the project's task copies are on `wsx/t<n>`.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { designLint, jumps, launch, mobileAudit, pair, pause, shooter, soundCheck, watchFetch } from "./web-cdp.mjs";

const [siteUrl, code, projectArg, captures, homeOrder, auditPath, plansArg, restartFile, plansCheckout, longName, relayFile, wsxCli, newsFile] = process.argv.slice(2);
const project = Number(projectArg);
const cdp = await launch({
  captures, prefix: "tasks-", timeoutSeconds: 840,
});
const { send, evaluate, until, viewport, capture, fail, cleanup, problems, go, text, count, key, keyboard, watch, watched, setMotion } = cdp;
await watchFetch(cdp);
const auditSource = auditPath ? fs.readFileSync(auditPath, "utf8") : null;
const { file: mobilePath, source: mobileSource } = mobileAudit();
if (!mobileSource) await fail(`no mobile audit at ${mobilePath}; set WSX_MOBILE_AUDIT`);
// The main screens that get the design lint (scripts/design-lint.js), at 390x844 and 1280x800 only.
const linter = designLint({ evaluate, captures, file: "tasks" });
const LINTED = new Set(["new-chat", "new-chat-refused", "chats-home-approval", "chats-home", "chats-home-wide", "chat-room", "chat-room-wide", "settings-usage", "settings-usage-tokens", "settings-usage-projects", "settings-usage-limit-tones", "home", "sessions", "project", "task-sheet", "task-sheet-take-over-1280", "task-sheet-open", "page", "events-dark", "events-light", "media-sheet", "media-sheet-wide"]);
let size = [390, 844, 2];
const phone = () => size[0] < 600;
const audits = [];

// ^ Across 900 px the app swaps its layout (main.js useWide) and an open task sheet mounts again
// and slides in anew; wait for that state, not a time, so the next audit sees the settled sheet.
async function resize(next) {
  const sheet = await evaluate("!!document.querySelector('dialog.task-sheet[open]')").catch(() => false);
  size = next;
  await viewport(...next);
  await pause(250);
  await until(`(!document.querySelector('.app') || !!document.querySelector('.app[data-wide]') === matchMedia('(min-width: 900px)').matches)
    && (${!sheet} || !!document.querySelector('dialog.task-sheet[open]'))`, `the layout did not follow the resize to ${next[0]}x${next[1]}`);
}
const click = (selector) => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
// Type into the focused-to-be field as an input event does, the caret at the end.
const typeInto = (selector, value) => evaluate(`(() => { const field = document.querySelector(${JSON.stringify(selector)}); field.focus();
  field.value = ${JSON.stringify(value)}; field.setSelectionRange(field.value.length, field.value.length);
  field.dispatchEvent(new Event('input', { bubbles: true })); field.dispatchEvent(new KeyboardEvent('keyup', { bubbles: true })); })()`);
// Paste into a text box: a ClipboardEvent that carries a DataTransfer, as a browser sends it.
// "image" is a PNG named "image.png", as iOS and Chrome name a pasted screenshot, and "image-2"
// another one: 640 x 400 with a border, a grid, a corner block and its name, so a capture of the
// image viewer shows its fit and edges. Any other value is plain text. It returns false when the page cancelled the paste.
const pasteInto = (selector, what) => evaluate(`(async () => {
  const data = new DataTransfer();
  if (${JSON.stringify(what)}.startsWith('image')) {
    const canvas = Object.assign(document.createElement('canvas'), { width: 640, height: 400 });
    const pen = canvas.getContext('2d');
    pen.fillStyle = '#e6e6e6'; pen.fillRect(0, 0, 640, 400);
    pen.fillStyle = '#282c34'; pen.fillRect(8, 8, 624, 384);
    pen.strokeStyle = '#5a5f69';
    for (let at = 40; at < 640; at += 40) { pen.beginPath(); pen.moveTo(at, 8); pen.lineTo(at, 392); pen.moveTo(8, at); pen.lineTo(632, at); pen.stroke(); }
    pen.fillStyle = '#468cdc'; pen.fillRect(8, 8, 112, 72);
    pen.fillStyle = '#fac83c'; pen.font = 'bold 64px sans-serif'; pen.fillText(${JSON.stringify(what)}, 120, 240);
    data.items.add(new File([await new Promise((done) => canvas.toBlob(done, 'image/png'))], 'image.png', { type: 'image/png' }));
  } else data.setData('text/plain', ${JSON.stringify(what)});
  const field = document.querySelector(${JSON.stringify(selector)});
  field.focus();
  return field.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
})()`);

// ^ A sheet that is still sliding in passes the edge.
const sound = soundCheck(cdp, () => size, { free: ".code-block pre, .table-wrap, .diagram, .since p", apart: ".rail, .reservoir, .slash, .task-sheet",
  settle: (label) => until("(document.querySelector('dialog.task-sheet')?.getAnimations().length ?? 0) === 0", `${label}: the sheet kept moving`) });
const shot = shooter(cdp, { captures, prefix: "tasks-", size: () => size, fileOf: (name) => `${size[0]}x${size[1]}-${name}`, linter, linted: LINTED, auditSource, mobileSource, before: sound,
  record: (file, mobile) => audits.push(mobile ? { capture: file, contentShare: mobile.contentShare, smallTargets: mobile.smallTargets.items.map((item) => `${item.label || item.el} ${item.size.join("x")}`),
    tinyText: mobile.tinyText.count, outsideGutter: mobile.outsideGutter.count } : { capture: file }) });

// Every working state on screen has the one working color (--color-working in app.css): marks,
// pulses, typing dots and words. `least` is how many working elements this screen must show.
async function oneWorkingColor(where, least) {
  const seen = await evaluate(`(() => {
    const probe = Object.assign(document.createElement('span'), { style: 'color: var(--color-working)' });
    document.body.append(probe); const working = getComputedStyle(probe).color; probe.remove();
    const paint = (node) => node.matches('.mark, .pulse, .typing-dot') ? getComputedStyle(node).backgroundColor : getComputedStyle(node).color;
    const nodes = [...document.querySelectorAll('[data-tone=working], .pulse, .typing-dot')];
    return { working, count: nodes.length, off: nodes.filter((node) => paint(node) !== working).map((node) => node.className || node.tagName),
      accentRunning: document.querySelectorAll('.mark[data-running]:not([data-tone=working])').length }; })()`);
  if (seen.count < least || seen.off.length || seen.accentRunning) await fail(`${where}: working states do not share one color: ${JSON.stringify(seen)}`);
}

// A manager's bubble uses the column: wider than the old 88% cap, with its box (border, background) kept.
async function managerWidth(where) {
  const seen = await evaluate(`(() => { const log = document.querySelector('.log'), style = getComputedStyle(log);
    const column = log.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
    const box = [...document.querySelectorAll('.log .bubble[data-side=in] .bubble-box')].sort((a, b) => b.getBoundingClientRect().width - a.getBoundingClientRect().width)[0];
    const look = getComputedStyle(box);
    return { column, width: box.getBoundingClientRect().width, border: look.borderTopWidth, edge: look.borderTopColor, fill: look.backgroundColor }; })()`);
  if (!(seen.width > 0.88 * seen.column + 1 && seen.width <= seen.column)) await fail(`${where}: a manager bubble is ${seen.width}px in a ${seen.column}px column`);
  if (parseFloat(seen.border) < 1 || seen.edge === "rgba(0, 0, 0, 0)" || seen.fill === "rgba(0, 0, 0, 0)") await fail(`${where}: the bubble lost its box: ${JSON.stringify(seen)}`);
}

// The top of the task sheet as words: what changed, what to know next, and whether a step path is left.
const story = () => evaluate(`({ summary: document.querySelector('.story-summary .markdown')?.textContent.trim() ?? '',
  next: document.querySelector('.story-next')?.textContent.trim() ?? '', steps: document.querySelectorAll('.story-steps').length })`);
// The muted lines, folded runs opened first (a closed one is opened, an open one stays).
const statusLines = "(document.querySelectorAll('.log .fold-toggle[aria-expanded=\"false\"]').forEach((node) => node.click()), [...document.querySelectorAll('.status-line')])";
// The chat route's rows of a project (wsx-core `chat_rows`): what the page renders.
const chatRows = (of = project) => evaluate(`fetch('api/tasks/${of}/chat').then((r) => r.json()).then((data) => data.rows)`);
// A muted line that names task n as `word` ("#7 done").
const statusSays = (n, word) => `${statusLines}.some((node) => node.textContent.includes('#${n} ${word}'))`;
// The sheet's action buttons, under its top.
const nextButtons = () => text(".task-sheet .task-next button");
const hasNext = (label) => `[...document.querySelectorAll('.task-sheet .task-next button')].some((node) => node.textContent === ${JSON.stringify(label)})`;
const clickNext = (label) => evaluate(`[...document.querySelectorAll('.task-sheet .task-next button')].find((node) => node.textContent === ${JSON.stringify(label)}).click()`);
// Button room (docs/web-client-tasks.md "Task sheet"): each button shows its words on every
// width (× and ✓ mean Fail and Pass in To check) and is as wide as them.
async function buttonRoom(where) {
  const buttons = await evaluate(`[...document.querySelectorAll('.task-sheet .task-next .button')].map((node) => ({
    name: node.getAttribute('aria-label') || node.textContent.trim(), words: node.textContent.trim(), icon: !!node.querySelector('.icon'),
    width: Math.round(node.getBoundingClientRect().width), cut: node.scrollWidth > node.clientWidth + 1 }))`);
  const wrong = buttons.filter((button) => !button.name || button.cut || button.icon || button.words.length < 3);
  if (!buttons.length || wrong.length) await fail(`${where}: the buttons do not show their words: ${JSON.stringify(buttons)}`);
  fs.appendFileSync(path.join(captures, "button-room.txt"), `${where} at ${size[0]} px: ${buttons.map((button) => `${button.words} ${button.width}`).join(", ")}\n`);
}
// Nothing the new sheet replaced: no steps and no Review row.
async function lean(where) {
  if (await count(".task-sheet .story-steps")) await fail(`${where} still has the steps`);
  if ((await nextButtons()).includes("Review")) await fail(`${where} still has a Review button`);
}
// The sheet's open sections, by heading.
const openSections = () => evaluate("[...document.querySelectorAll('.task-sheet .scroll .task-section:not(.task-fold):not(.timeline) > .task-heading')].map((node) => node.textContent.trim())");
// The reservoir's rows: [number, title, context, actions], in the order shown.
// ^ A row's number is its task chip's "#n" (the chip also has the state in hidden words).
const NUMBER = "(row.querySelector('.task-chip')?.textContent.match(/#\\d+/)?.[0] ?? '')";
const rows = () => evaluate(`[...document.querySelectorAll('.reservoir-list .need')].map((row) => [
  ${NUMBER}, row.querySelector('.row-label').textContent, row.querySelector('.row-context')?.textContent ?? '',
  [...row.querySelectorAll('.need-action')].map((node) => node.title).join('+')])`);
const rowOf = (label) => `[...document.querySelectorAll('.reservoir-list .need')].find((row) => (${NUMBER} + ' ' + row.querySelector('.row-label').textContent).trim().startsWith(${JSON.stringify(label)}))`;
// One motion: the sheet's top and left on each animation frame, from before `open` to 700 ms
// after the sheet is first seen open. On each axis it moves one way only, never back, and its
// last frame is where it rests. On a phone it rises from below; on a wide screen it comes in
// from the right.
async function oneMotion(where, open) {
  await evaluate(`(() => { window.sheetFrames = []; window.sheetWatched = false; let seen = null;
    const step = (now) => { const sheet = document.querySelector('dialog.task-sheet[open]');
      if (sheet) { const box = sheet.getBoundingClientRect(); window.sheetFrames.push([box.top, box.left]); seen ??= now; }
      if (seen === null || now - seen < 700) requestAnimationFrame(step); else window.sheetWatched = true; };
    requestAnimationFrame(step); })()`);
  await open();
  await until("window.sheetWatched", `${where}: the sheet did not open`);
  const { frames, end } = await evaluate("({ frames: window.sheetFrames, end: (({ top, left }) => [top, left])(document.querySelector('dialog.task-sheet').getBoundingClientRect()) })");
  const moving = phone() ? 0 : 1;
  for (const axis of [0, 1]) {
    const values = frames.map((frame) => frame[axis]);
    const way = Math.sign(end[axis] - values[0]);
    // ^ A still axis stays at its end; a moving one never steps back by more than half a pixel.
    const back = values.some((value, index) => way ? index > 0 && (value - values[index - 1]) * way < -0.5 : Math.abs(value - end[axis]) > 0.5);
    if (back || Math.abs(values.at(-1) - end[axis]) > 0.5 || (axis === moving) !== (Math.abs(values[0] - end[axis]) > 50)) {
      await fail(`${where}: the sheet does not open in one motion on ${axis ? "left" : "top"}: ${JSON.stringify(values.map(Math.round))}, rests at ${Math.round(end[axis])}`);
    }
  }
}
// ^ A shot of a bubble or a row still fading in shows it dim.
const entered = () => until("document.getAnimations().every((animation) => animation.effect?.getTiming().iterations === Infinity || animation.playState !== 'running')", "the page kept moving");
// The manager's composer holds `value`, focused, the caret at its end and no `#` list open.
const composerHas = (value, what) => until(`(() => { const field = document.querySelector('.composer-field');
  return field.value === ${JSON.stringify(value)} && document.activeElement === field && field.selectionStart === field.value.length
    && field.selectionEnd === field.value.length && !document.querySelector('#slash-list'); })()`, what);
const closeSheet = async (what, hash) => {
  await key("Escape", 27);
  await until(`${hash ? `location.hash === ${JSON.stringify(hash)} && ` : ""}!document.querySelector('dialog.task-sheet')`, `Escape did not close ${what}`);
};
// A tap on an image link opens it full screen over the page (docs/web-client.md "Image viewer"):
// no navigation, the image fits the screen, a swipe (phone) or an arrow key (wide) goes to the
// set's other image and back, and Escape, Back and a tap beside the image close it with the page
// where it was. `links` selects the set's `many` links (one: the viewer shows no count and
// nowhere to go); the open viewer is captured as `name`.
// The download control's URL answers as an attachment named task-<n>-<id>.<ext> with the bytes
// of the inline capture; the capture's own URL stays inline.
async function savedCopy(what, href) {
  const got = await evaluate(`(async () => {
    const kept = await fetch(${JSON.stringify(href)}); const plain = await fetch(${JSON.stringify(href.replace("?download=1", ""))});
    const same = new Uint8Array(await kept.arrayBuffer()).join() === new Uint8Array(await plain.arrayBuffer()).join();
    return { status: kept.status, disposition: kept.headers.get("content-disposition"), inline: plain.headers.get("content-disposition"), same };
  })()`);
  if (got.status !== 200 || !/^attachment; filename="task-\d+-c\d+(-poster)?\.(png|jpeg|webp|mp4)"/.test(got.disposition ?? "") || got.inline !== null || !got.same) await fail(`${what}: the saved copy is wrong: ${JSON.stringify(got)}`);
}
async function viewImages(name, links, many = 2, saves = false) {
  const what = `${name} at ${size[0]} px`;
  const place = "JSON.stringify([location.pathname, location.search, location.hash, [...document.querySelectorAll('.scroll, .log')].map((node) => Math.round(node.scrollTop))])";
  // ^ The set's images are in and the log has followed them to its end before the page's place is read.
  await evaluate(`Promise.all([...document.querySelectorAll(${JSON.stringify(links)})].map((link) => link.querySelector('img').decode().catch(() => {})))`);
  // ^ Other rows still settle (images outside the set, a late row): the log follows its end until its place holds for a moment.
  for (let tries = 0; tries < 20; tries++) {
    const was = await evaluate(place);
    await pause(250);
    if (was === await evaluate(place)) break;
  }
  const sources = await evaluate(`[...document.querySelectorAll(${JSON.stringify(links)})].map((link) => link.href)`);
  if (sources.length !== many) await fail(`${what}: the set has ${sources.length} image links, not ${many}`);
  const shown = "document.querySelector('dialog.viewer[open] .viewer-stage img:not([data-loading])')";
  const press = async ([x, y]) => {
    for (const type of ["mousePressed", "mouseReleased"]) await send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1 });
  };
  // ^ A real click, as a person opens it: Chrome closes a dialog opened without a user action
  // together with the dialog under it on one Escape (the sheet over the To check list).
  // ^ The first image is scrolled into view and the click point and the page's place are read in one step, while nothing
  // (a toast, a menu on its way out, a reply that moves the log) lies over the image's middle; the click follows at once.
  let before;
  const open = async () => {
    const aim = `(() => { const link = document.querySelector(${JSON.stringify(links)}); link.scrollIntoView({ block: 'nearest' }); const box = link.getBoundingClientRect();
      const at = [box.left + box.width / 2, box.top + box.height / 2], hit = document.elementFromPoint(...at);
      return { covered: link.contains(hit) ? '' : hit ? hit.tagName + '.' + hit.className : 'nothing', at, place: ${place} }; })()`;
    await until(`(globalThis.wsxAim = ${aim}).covered === ''`, async () => `${what}: the first image is covered by ${(await evaluate("globalThis.wsxAim")).covered}`);
    const aimed = await evaluate("globalThis.wsxAim");
    before = aimed.place;
    await press(aimed.at.map(Math.round));
    await until(`!!${shown} && history.state?.viewer === true`, `${what}: the image did not open full screen`);
  };
  const closed = async (how) => {
    await until(`!document.querySelector('dialog.viewer') && !history.state?.viewer`, `${what}: ${how} did not close the viewer`);
    const after = await evaluate(place);
    if (after !== before) await fail(`${what}: after ${how} the page is not where it was: ${after}, was ${before}`);
  };
  const state = () => evaluate(`(() => { const stage = document.querySelector('.viewer-stage').getBoundingClientRect(); const box = ${shown}?.getBoundingClientRect();
    return box && { src: ${shown}.src, place: ${place}, count: [...document.querySelectorAll('.viewer-bar .time')].find((node) => / of /.test(node.textContent))?.textContent ?? '', next: !!document.querySelector('.viewer-bar [aria-label="Next image"]'),
      inside: box.left >= stage.left - 1 && box.top >= stage.top - 1 && box.right <= stage.right + 1 && box.bottom <= stage.bottom + 1,
      gaps: [box.left - stage.left, stage.right - box.right, box.top - stage.top, stage.bottom - box.bottom].map(Math.round),
      fills: Math.max(box.width / stage.width, box.height / stage.height) > 0.99,
      beside: box.left - stage.left > 20 ? [stage.left + 10, stage.top + stage.height / 2] : [stage.left + stage.width / 2, stage.bottom - 10] }; })()`);
  const at = async (index) => {
    await until(`${shown}?.src === ${JSON.stringify(sources[index])}`, `${what}: the viewer did not go to image ${index + 1}`);
    const now = await state();
    if (now.count !== (many > 1 ? `${index + 1} of ${many}` : "") || now.next !== (many > 1) || !now.inside || !now.fills) await fail(`${what}: image ${index + 1} does not fit the screen: ${JSON.stringify(now)}`);
    // ^ Centered both ways: the gaps left and right are equal, and so are those above and below.
    const [left, right, top, bottom] = now.gaps;
    if (Math.abs(left - right) > 1 || Math.abs(top - bottom) > 1) await fail(`${what}: image ${index + 1} is not centered: gaps ${JSON.stringify(now.gaps)}`);
    return now;
  };
  const swipe = (from, to) => touch({ x: from, y: Math.round(size[1] / 2) }, to - from);
  await open();
  const first = await at(0);
  if (first.place !== before) await fail(`${what}: opening the image navigated: ${first.place}, was ${before}`);
  await until("document.querySelector('dialog.viewer').getAnimations({ subtree: true }).length === 0", `${what}: the viewer kept moving`);
  await shot(`viewer-${name}`);
  // ^ Touch: the bar is under the stage at the screen's bottom and the link is Share; wide: the bar is on top and it is Download.
  const readBar = () => evaluate(`(() => { const node = document.querySelector('.viewer-bar'); const bar = node.getBoundingClientRect(); const stage = document.querySelector('.viewer-stage').getBoundingClientRect();
    const link = node.querySelector('a[download]'); return { top: Math.round(bar.top), bottom: Math.round(bar.bottom), stageTop: Math.round(stage.top), stageBottom: Math.round(stage.bottom), height: innerHeight, buttonsEnd: Math.round(Math.max(...[...node.children].map((child) => child.getBoundingClientRect().bottom))),
      download: link?.getAttribute('href') ?? null, label: link?.getAttribute('aria-label') ?? null, title: link?.title ?? null, icon: link?.querySelector('path')?.getAttribute('d')?.startsWith('M10 12.5V3.5') ?? false }; })()`);
  const bar = await readBar();
  if (saves !== (bar.download !== null)) await fail(`${what}: the viewer ${saves ? "has no" : "has a"} download control`);
  if (saves) await savedCopy(what, bar.download);
  const placed = phone() ? bar.top >= bar.stageBottom && bar.bottom === bar.height && bar.stageTop === 0 : bar.bottom <= bar.stageTop && bar.top === 0;
  if (!placed || (saves && (bar.label !== (phone() ? "Share" : "Download") || bar.title !== bar.label || !bar.icon))) await fail(`${what}: the viewer bar reads ${JSON.stringify(bar)}`);
  if (phone()) {
    // ^ A notched phone: the stage starts under the top inset and the bar's buttons end above the bottom inset.
    await send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 47, bottom: 34 } });
    await pause(250);
    const notch = { ...await readBar(), inside: (await state()).inside };
    await send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 0, bottom: 0 } });
    if (notch.stageTop !== 47 || notch.buttonsEnd > notch.height - 34 || !notch.inside) await fail(`${what}: the viewer ignores the safe area: ${JSON.stringify(notch)}`);
  }
  if (many > 1) {
    if (phone()) await swipe(size[0] - 60, 60);
    else await key("ArrowRight", 39);
    await at(1);
    if (phone()) await swipe(60, size[0] - 60);
    else await key("ArrowLeft", 37);
    await at(0);
  }
  if (phone() && await count(".viewer-zoom .time")) {
    // ^ The page never zooms, but a pinch in the viewer zooms its image.
    await pinch(await evaluate("(() => { const box = document.querySelector('.viewer-stage').getBoundingClientRect(); return { x: Math.round(box.left + box.width / 2), y: Math.round(box.top + box.height / 2) }; })()"));
    await until("parseInt(document.querySelector('.viewer-zoom .time').textContent) > 100", `${what}: a pinch did not zoom the image`);
    if (await evaluate("visualViewport.scale") !== 1) await fail(`${what}: a pinch in the viewer zoomed the page`);
  }
  await key("Escape", 27);
  await closed("Escape");
  await open();
  await evaluate("history.back()");
  await closed("Back");
  await open();
  const [x, y] = (await state()).beside.map(Math.round);
  if (phone()) await touch({ x, y }, 0);
  else await press([x, y]);
  await closed("a tap beside the image");
}
// The list of one pill: "running", "needs", "check" or "routines" (the pills in the composer row).
const LISTS = { running: "Running", needs: "Needs you", check: "To check", routines: "Routines" };
async function openReservoir(kind = "needs") {
  // ^ A pill shows when its list has something: the task a manager creates comes after a moment.
  await until(`!!document.querySelector('.pill[data-kind=${kind}]')`, `there is no ${LISTS[kind]} pill`, 300);
  if (!(await evaluate(`document.querySelector('.pill[data-kind=${kind}]')?.getAttribute('aria-pressed') === 'true'`))) await click(`.pill[data-kind=${kind}]`);
  await until(`!!document.querySelector('.reservoir-list[aria-label="${LISTS[kind]}"]')`, `the ${LISTS[kind]} list did not open`);
  // ^ The list is open when its reveal and its rows have stopped moving (a dot's pulse never does); at once under reduced motion.
  await until("[...document.querySelector('.reservoir').getAnimations({ subtree: true })].every((animation) => animation.playState !== 'running' || animation.effect.getComputedTiming().iterations === Infinity)", `the ${LISTS[kind]} list kept moving`);
}
// The focus after a row cleared: [number, label] of the row whose action has it, or the open pill.
const focused = () => evaluate(`(() => { const node = document.activeElement; const row = node?.closest('.reservoir .need');
  if (row) return [${NUMBER}, row.querySelector('.row-label').textContent, node.title || node.textContent.trim()];
  if (node?.classList.contains('pill')) return ['pill'];
  return node?.matches('.chat .composer-field') ? ['composer'] : [node?.tagName ?? 'none']; })()`);
// One action in a row, then that row leaves and the focus is on the next row's action (on the
// composer after the last row). The
// row is known by its item (`blocked-1`): the same task may come back as another item (#1
// under Running, then `land-1`). `attempts` are of 100 ms, as in `until`.
async function clearRow(label, action, attempts = 300, toComposer = false) {
  const key = await evaluate(`${rowOf(label)}.dataset.key`);
  if (!key) await fail(`the row ${label} has no item key`);
  // ^ As a pointer or a key press does, the button takes the focus before it acts.
  await evaluate(`(() => { const button = [...${rowOf(label)}.querySelectorAll('.need-action')].find((node) => node.title === ${JSON.stringify(action)});
    button.focus(); button.click(); })()`);
  await until(`!document.querySelector('.reservoir .need[data-key=${JSON.stringify(key)}]')`, `${action} did not clear the row ${label} (${key})`, attempts);
  // ^ After the last row the list closes; no text field takes the focus (a phone's keyboard would open).
  // ^ Pass and Fail in a To check row quote the item in the composer for the owner's note: the focus is there.
  const inList = toComposer ? "document.activeElement === document.querySelector('.composer-field')"
    : "!!document.activeElement?.closest('.reservoir .need .need-action, .reservoir .need .row-open, .pill[aria-pressed=true]') || (!document.querySelector('.reservoir') && !document.activeElement?.matches('textarea, input'))";
  for (let attempt = 0; attempt < 100 && !(await evaluate(inList)); attempt++) await pause(100);
  if (!(await evaluate(inList))) await fail(`after ${action} on ${label} the focus is on ${JSON.stringify(await focused())}, not on the next row`);
  const now = await focused();
  if (await evaluate("document.activeElement?.closest('.need')?.dataset.key ?? ''") === key) await fail(`after ${action} the focus stayed on ${label}`);
  return now;
}
// Each list has a header: its name and one button that closes it (the same as the pill); the rows
// scroll under it. A tap closes with no text field focused (`click()` focuses nothing, as a tap on
// iOS does, so the list's close left the focus on the body), and Escape closes it too.
const textFocus = "!!document.activeElement?.matches('textarea, input')";
async function listHeaders() {
  for (const kind of Object.keys(LISTS)) {
    const label = LISTS[kind];
    await openReservoir(kind);
    const head = await evaluate(`(() => { const box = document.querySelector('.reservoir').getBoundingClientRect(), head = document.querySelector('.reservoir-head'), shut = head.querySelector('button'), scroll = document.querySelector('.reservoir-scroll').getBoundingClientRect();
      const mine = head.getBoundingClientRect(), button = shut.getBoundingClientRect();
      return { title: head.querySelector('.reservoir-title').textContent, name: shut.getAttribute('aria-label'), high: Math.round(mine.height), top: Math.round(mine.top - box.top), under: scroll.top >= mine.bottom - 0.5,
        right: Math.abs(button.right - mine.right) < 16, buttons: head.querySelectorAll('button').length }; })()`);
    if (head.title !== label || head.name !== `Close ${label}` || head.buttons !== 1 || head.top > 1 || !head.under || !head.right || head.high > 40) await fail(`the ${label} header is ${JSON.stringify(head)}`);
    // ^ The header stays while the rows scroll.
    const kept = await evaluate(`(() => { const scroll = document.querySelector('.reservoir-scroll'), head = document.querySelector('.reservoir-head'), before = head.getBoundingClientRect().top; scroll.scrollTop = 9999; return head.getBoundingClientRect().top === before; })()`);
    if (!kept) await fail(`the ${label} header moved with the rows`);
    await shot(`pills-${kind}-header`, ".composer-send");
    // ^ An earlier step may have left the composer focused; a tap on iOS leaves the body focused.
    await evaluate("document.activeElement?.blur()");
    await click(`.pill[data-kind=${kind}]`);
    await until("!document.querySelector('.reservoir')", `a tap on the open ${label} pill did not close its list`);
    if (await evaluate(textFocus)) await fail(`closing ${label} by its pill focused ${await evaluate("document.activeElement.className")}`);
    await openReservoir(kind);
    await evaluate("document.activeElement?.blur()");
    await click(`.reservoir-head button[aria-label="Close ${label}"]`);
    await until("!document.querySelector('.reservoir')", `the close button of ${label} did not close the list`);
    if ((await pressed()) !== "" || await evaluate(textFocus)) await fail(`closing ${label} by its button leaves ${await pressed()} pressed, a text field focused: ${await evaluate(textFocus)}`);
    // ^ Escape from inside the list: the focus goes to the pill that opened it.
    await openReservoir(kind);
    await evaluate(`document.querySelector('.reservoir-head button').focus()`);
    await key("Escape", 27);
    await until("!document.querySelector('.reservoir')", `Escape did not close the ${label} list`);
    if (!(await evaluate(`document.activeElement === document.querySelector('.pill[data-kind=${kind}]')`))) await fail(`after Escape the focus is on ${JSON.stringify(await focused())}, not the ${label} pill`);
    // ^ Escape with the focus on the page (a tap on iOS leaves it there) closes it too.
    await openReservoir(kind);
    await evaluate("document.activeElement?.blur()");
    await key("Escape", 27);
    await until("!document.querySelector('.reservoir')", `Escape on the page did not close the ${label} list`);
    if (await evaluate(textFocus)) await fail(`Escape on the page focused a text field after ${label}`);
  }
}
async function closeReservoir() {
  if (await evaluate("!!document.querySelector('.pill[aria-pressed=true]')")) await click(".pill[aria-pressed=true]");
}
// How far, in px, the bottom of each chip's "#n" text is from the text it sits beside: the
// text node on its line, or the `near` element's text in the same container.
const chipDrift = (chips, near) => evaluate(`(() => {
  const first = (node) => { const walker = document.createTreeWalker(node, NodeFilter.SHOW_TEXT); let text;
    while ((text = walker.nextNode()) && !text.textContent.trim()); return text; };
  const rects = (text) => { const range = document.createRange(); range.selectNodeContents(text); return [...range.getClientRects()]; };
  const beside = (chip) => { for (const step of ['previousSibling', 'nextSibling']) { let node = chip[step];
    while (node && !node.textContent.trim()) node = node[step]; if (node) return node.nodeType === 3 ? node : first(node); } return null; };
  return [...document.querySelectorAll(${JSON.stringify(chips)})].map((chip) => {
    const own = first(chip) && rects(first(chip))[0];
    const other = ${near ? `first(chip.closest(${JSON.stringify(near[0])}).querySelector(${JSON.stringify(near[1])}))` : "beside(chip)"};
    const line = own && other && rects(other).find((rect) => rect.bottom > own.top && rect.top < own.bottom);
    return line ? Math.round(Math.abs(own.bottom - line.bottom) * 10) / 10 : null; }).filter((drift) => drift !== null); })()`);
async function chipsOnLine(where, chips, near) {
  const drifts = await chipDrift(chips, near);
  if (!drifts.length || drifts.some((drift) => drift > 1)) await fail(`${where}: chip text is off the line of its text by ${JSON.stringify(drifts)} px`);
}
// ^ An open task's word is at the top of the sheet; a finished task's stage is in its top row.
const sheetState = () => evaluate("document.querySelector('.task-sheet :is(.progress-health, .bar-state)')?.textContent ?? ''");
const atEnd = "(() => { const log = document.querySelector('.log'); return log.scrollHeight - log.scrollTop - log.clientHeight < 2; })()";
const bubbles = () => evaluate(`[...document.querySelectorAll('.log .bubble')].map((node) => ({ side: node.dataset.side, first: node.hasAttribute('data-first'),
  last: node.hasAttribute('data-last'), author: node.querySelector('.bubble-author')?.textContent ?? '', tag: node.querySelector('.bubble-tag')?.textContent ?? '', text: node.querySelector('.bubble-box').textContent.trim().replace(/\\s+/g, ' ') }))`);

await resize([390, 844, 2]);
await pair(cdp, siteUrl, code);
const chat = `#/p/${project}`;
const page = (n) => `#/p/${project}/t/${n}`;

// The tabs: each one link in the bar, inside the first screen. Home lists the projects.
const TABS = "Home,Sessions,Settings";
// Motion (docs/web-client.md "Motion"): the animated elements carry their animations, and with
// prefers-reduced-motion: reduce none of them move. The page's own store is driven through its
// modules; the frames are captures of the same states.
async function motion() {
  const w = size[0];
  const pageModule = (name) => `import(new URL(${JSON.stringify(name)}, [...document.scripts].find((script) => script.type === 'module').src))`;
  const names = (selector) => evaluate(`[...document.querySelectorAll(${JSON.stringify(selector)})].map((node) => getComputedStyle(node).animationName)`);
  const expectAll = async (label, selector, name) => {
    const seen = await names(selector);
    if (!seen.length || seen.some((item) => item !== name)) await fail(`motion at ${w}px: ${label} (${selector}) animates ${JSON.stringify(seen)}, not ${name}`);
  };
  const run = (reduced) => evaluate(`(async () => {
    const { state, update } = await ${pageModule("store.js")};
    const { toast } = await ${pageModule("ui.js")};
    const { render, html, Loading } = await ${pageModule("lib.js")};
    globalThis.motionKept = { tasks: state.tasks, projects: state.projects };
    const ui = state.projects.find((project) => project.name === 'ui').id;
    const calm = state.projects.find((project) => project.name === 'calm');
    const rows = () => [...document.querySelectorAll('.home-rows > .row:not(.row-chat)')];
    const frames = () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    // The manager of ui runs a turn: typing dots on its row. ^ Only ui's row: another fixture
    // manager may run a real turn at the same time.
    update({ tasks: { ...state.tasks, [ui]: { ...state.tasks[ui], busy: true } } });
    await frames();
    const typing = document.querySelectorAll('.home-rows > .row:not(.row-chat)[data-key="' + ui + '"] .row-typing .typing-dot').length;
    toast('Saved the page');
    await frames();
    const notice = !!document.querySelector('.notice .notice-timer');
    // calm becomes the newest: it moves up; then it goes: its row fades out where it was.
    const before = rows().map((row) => row.dataset.key);
    update({ tasks: { ...state.tasks, [calm.id]: { ...state.tasks[calm.id], last_unix_ms: Date.now() + 1e7 } } });
    await frames();
    const moved = rows().map((row) => row.dataset.key).join() !== before.join();
    const glides = document.getAnimations().filter((animation) => animation.effect?.target?.matches?.('.home-rows > .row:not(.row-chat)')
      && String(animation.effect.getKeyframes()[0].transform ?? '').startsWith('translateY')).length;
    const projects = state.projects;
    update({ projects: projects.filter((project) => project !== calm) });
    await frames();
    const ghosts = document.querySelectorAll('.home-rows > [data-ghost]').length;
    update({ projects });
    const holder = document.body.appendChild(document.createElement('div'));
    render(html\`<\${Loading} />\`, holder);
    return { typing, notice, moved, glides, ghosts, loading: holder.querySelector('.loading')?.getAttribute('role'), reduced: ${reduced} };
  })()`);
  // ^ The fake state goes into the live store: a real update in the same frame (another
  // manager's turn, a project list read) can undo it, so one of 3 tries must show it all.
  const good = (seen) => seen.typing === 3 && seen.notice && seen.moved && seen.glides >= 1 && seen.ghosts === 1 && seen.loading === "status";
  let seen = await run(false);
  // ^ A try that failed left calm the newest: the next one starts from the kept state.
  const restore = () => evaluate(`(async () => (await ${pageModule("store.js")}).update(globalThis.motionKept))()`);
  for (let attempt = 1; attempt < 3 && !good(seen); attempt++) { await restore(); await pause(1000); seen = await run(false); }
  if (!good(seen)) await fail(`motion at ${w}px: ${JSON.stringify(seen)}`);
  // ^ A real overview read between the tries and here can undo the fake busy: set it again, as `run` did.
  const typingOn = () => evaluate(`(async () => {
    const { state, update } = await ${pageModule("store.js")};
    const ui = state.projects.find((project) => project.name === 'ui').id;
    update({ tasks: { ...state.tasks, [ui]: { ...state.tasks[ui], busy: true } } });
    await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
  })()`);
  await typingOn();
  await expectAll("typing dots", ".row-typing .typing-dot", "typing");
  await expectAll("the tray counts", ".home-row .tray-count", "pop");
  await expectAll("the status line", ".home-row .row-context:not([data-loading])", "arrive");
  await expectAll("the notice", ".notice", "slide-down");
  await expectAll("the toast timer", ".notice-timer", "drain");
  await expectAll("the loading placeholder", ".skeleton", "shimmer");
  await typingOn();
  await capture(`${w}-motion-home-typing`);
  await pause(60);
  await capture(`${w}-motion-home-toast`);
  // Reduced motion: the same states, no animation and no script-driven motion.
  await setMotion("reduce");
  const still = await evaluate(`(async () => {
    const { state, update } = await ${pageModule("store.js")};
    const calm = state.projects.find((project) => project.name === 'calm');
    const frames = () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    update({ tasks: { ...state.tasks, [calm.id]: { ...state.tasks[calm.id], last_unix_ms: 1 } } });
    await frames();
    const rows = document.getAnimations().filter((animation) => animation.effect?.target?.matches?.('.home-rows > .row:not(.row-chat)')).length;
    update(globalThis.motionKept);
    return rows;
  })()`);
  if (still) await fail(`motion at ${w}px: ${still} row animations run with reduced motion`);
  for (const selector of [".row-typing .typing-dot", ".home-row .tray-count", ".home-row .row-context", ".notice", ".notice-timer", ".skeleton"]) {
    const seen = await names(selector);
    if (seen.some((item) => item !== "none")) await fail(`motion at ${w}px: ${selector} animates with reduced motion: ${JSON.stringify(seen)}`);
  }
  await capture(`${w}-motion-home-reduced`);
  await setMotion("no-preference");
  await evaluate("document.querySelectorAll('body > div:not(.app):not(#app)').forEach((node) => node.remove())");
  await until("!document.querySelector('.notice')", "the toast did not clear itself", 120);
}
async function tabsInView() {
  const bar = phone() ? ".tabbar a" : ".rail-tabs a";
  if ((await text(bar)).join() !== TABS) await fail(`the tabs at ${size[0]}x${size[1]} are ${await text(bar)}`);
  const out = await evaluate(`[...document.querySelectorAll(${JSON.stringify(bar)})].filter((node) => { const r = node.getBoundingClientRect();
    return !(r.width > 0 && r.height >= 24 && r.top >= 0 && r.bottom <= innerHeight + 0.5 && r.left >= 0 && r.right <= innerWidth + 0.5); }).map((node) => node.textContent)`);
  if (out.length) await fail(`at ${size[0]}x${size[1]} these tabs are not in the first screen: ${out}`);
}
// The plain worktree `old-work` on a project's worktree list: its heading and session, and no
// Adopted list (docs/web-client-tasks.md "Project screen").
async function plainWorktree(where) {
  const section = "[...document.querySelectorAll('.worktree')].find((node) => node.getAttribute('aria-label') === 'old-work')";
  await until(`!!${section}`, `${where} has no old-work worktree`);
  const listed = await evaluate(`${section}.innerText`);
  if (!/old chat/.test(listed)) await fail(`${where}: old-work reads ${JSON.stringify(listed)}`);
  if (await count(".adopted")) await fail(`${where} still has the Adopted list`);
}

// Home's order below the needs (docs/web-client-tasks.md "Home"): a project with an unread message
// sorts above a read one that is newer by activity, and a message arriving while its chat is open
// leaves it read. The page's own store is driven through its modules, as `motion` does.
// The page's repeating timers up to 30 s, by delay, from here on: the ones a component has
// started and not cleared. A timer made before this call is not counted.
const trackTimers = () => evaluate(`(() => {
  if (window.__timers) return;
  const timers = (window.__timers = new Map()), start = window.setInterval.bind(window), stop = window.clearInterval.bind(window);
  window.setInterval = (callback, delay, ...rest) => { const id = start(callback, delay, ...rest); if (delay <= 30000) timers.set(id, delay); return id; };
  window.clearInterval = (id) => { timers.delete(id); return stop(id); };
})()`);
const timers = (delay) => evaluate(`[...window.__timers.values()].filter((delay) => delay === ${delay}).length`);

// Home ticks each second only while a row shows a live age (a working task or session): with
// only ready tasks it has no such timer. ^ Home mounts again so its timer is the tracked one.
async function homeTimers() {
  await trackTimers();
  const pageModule = (name) => `import(new URL(${JSON.stringify(name)}, [...document.scripts].find((script) => script.type === 'module').src))`;
  const mounted = async (stage) => {
    await go(chat);
    await until("!!document.querySelector('.composer-field')", "the chat did not open for the home timers");
    await go("#/");
    await until("!!document.querySelector('.home-row') && !document.querySelector('.composer-field')", "home did not replace the chat for the home timers");
    return evaluate(`(async () => {
      const { state, update } = await ${pageModule("store.js")};
      const kept = { tasks: state.tasks, sessions: state.sessions };
      update(${stage});
      await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
      const ticking = [...window.__timers.values()].filter((delay) => delay === 1000).length;
      update(kept);
      return ticking;
    })()`);
  };
  const working = await mounted("{ tasks: Object.fromEntries(Object.entries(state.tasks).map(([id, tasks]) => [id, { ...tasks, working: 1 }])) }");
  const ready = await mounted("{ sessions: [], tasks: Object.fromEntries(Object.entries(state.tasks).map(([id, tasks]) => [id, { ...tasks, working: 0 }])) }");
  if (working < 1 || ready >= working) await fail(`Home ticks each second ${working} times with working tasks and ${ready} times with only ready ones`);
  await go("#/");
}

async function homeUnread() {
  const pageModule = (name) => `import(new URL(${JSON.stringify(name)}, [...document.scripts].find((script) => script.type === 'module').src))`;
  const result = await evaluate(`(async () => {
    const { state, update } = await ${pageModule("store.js")};
    const frames = () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    const by = (name) => state.projects.find((project) => project.name === name);
    const [calm, gone, quiet] = ['calm', 'gone', 'quiet'].map(by);
    const kept = { tasks: state.tasks, read: localStorage.getItem('wsx-read') };
    const T = Date.now() + 2e7;
    // ^ calm has a message nobody read here; gone and quiet are read, and gone is newer by activity.
    localStorage.setItem('wsx-read', JSON.stringify(Object.fromEntries([calm, gone, quiet].map((project) => ['project-' + project.id, { read: T - 1000, place: null }]))));
    const overview = (project, extra) => ({ ...state.tasks[project.id], ...extra });
    const names = () => [...document.querySelectorAll('.home-rows > .row:not(.row-chat)')].map((row) => row.querySelector('.row-label').textContent).filter((name) => ['calm', 'gone', 'quiet'].includes(name));
    const marked = (project) => document.querySelector('.home-rows > .row:not(.row-chat)[data-key="' + project.id + '"]').hasAttribute('data-news');
    update({ tasks: { ...state.tasks,
      [calm.id]: overview(calm, { news: { unix_ms: T, text: 'new' }, last_unix_ms: T - 9e6 }),
      [gone.id]: overview(gone, { news: null, last_unix_ms: T + 9e6 }),
      [quiet.id]: overview(quiet, { news: null, last_unix_ms: T - 5e6 }) } });
    dispatchEvent(new Event('wsx-read'));
    await frames();
    const unread = { order: names(), calm: marked(calm) };
    // ^ quiet's chat is open: its newer message is read at once and does not move it up.
    location.hash = '#/p/' + quiet.id;
    await frames();
    update({ tasks: { ...state.tasks, [quiet.id]: overview(quiet, { news: { unix_ms: T + 1000, text: 'arrived' } }) } });
    await frames();
    const open = { order: names(), quiet: marked(quiet) };
    update({ tasks: kept.tasks });
    if (kept.read == null) localStorage.removeItem('wsx-read'); else localStorage.setItem('wsx-read', kept.read);
    location.hash = '#/';
    return { unread, open };
  })()`);
  if (result.unread.order.join() !== "calm,gone,quiet" || !result.unread.calm) await fail(`an unread project does not sort first: ${JSON.stringify(result.unread)}`);
  if (result.open.order.join() !== "calm,gone,quiet" || result.open.quiet) await fail(`a message in an open chat marks it unread: ${JSON.stringify(result.open)}`);
}

// --- Home: one row per project, needs first, then by activity; the bottom bar is Home, Sessions and Settings.
await go("#/");
await until("document.querySelectorAll('.home-row').length >= 3", "home does not list the projects");
// ^ The task summaries load after the projects; the order settles when they are in.
// ^ A row's name is its label's first text: an asking row's label adds a hidden ", Asks you".
await until(`[...document.querySelectorAll('.home-row .row-label')].map((node) => node.firstChild.textContent).join() === ${JSON.stringify(homeOrder)}`, "home did not settle in the expected order", 30);
const homeRows = await evaluate(`[...document.querySelectorAll('.home-row')].map((row) => ({ name: row.querySelector('.row-label').firstChild.textContent,
  count: row.querySelector('.tray[data-kind=needs] .tray-count')?.textContent ?? '', trays: [...row.querySelectorAll('.tray')].map((tray) => tray.dataset.kind + ':' + tray.querySelector('.tray-count').textContent + ':' + !!tray.querySelector('svg.icon')).join(),
  status: row.querySelector('.row-context').textContent, state: row.querySelector('.time')?.textContent ?? '' }))`);
const uiRow = homeRows.find((row) => row.name === "ui");
// ^ The tray icons are the pills' (trays.js): the needs tray reads the needs count, and a row with nothing to count shows none.
if (!uiRow.trays.split(",").includes("needs:6:true") || homeRows.find((row) => row.name === "calm").trays) await fail(`the Home trays read ${JSON.stringify(homeRows.map((row) => [row.name, row.trays]))}`);
if (uiRow.count !== "6" || uiRow.status !== "#1 Waiting for your answer" || !/ago|now/.test(uiRow.state)) await fail(`the ui row reads ${JSON.stringify(uiRow)}`);
const calmRow = homeRows.at(-1);
if (calmRow.name !== "calm" || calmRow.count || calmRow.status !== "No tasks yet" || calmRow.state) await fail(`the calm row reads ${JSON.stringify(calmRow)}`);
const plansRow = homeRows.find((row) => row.name === "plans");
// ^ #1 waits for an answer; #2 is ready to land. The needs tray counts them with the project's open questions, as the pill does.
const plansView = await evaluate(`fetch('api/tasks/${plansArg}').then((r) => r.json()).then((view) => ({ needs: view.needs.length, asks: view.asks.length }))`);
if (plansRow?.status !== "#1 Waiting for your answer" || plansView.needs !== 2 || plansRow.count !== String(plansView.needs + plansView.asks)) await fail(`the plans row reads ${JSON.stringify([plansRow, plansView])}`);
// Projects with agent sessions and no task: the sessions in words, and the time of the newest.
// ^ `quiet` went done while the fixture was built, which can take over a minute.
const sessionRows = { busy: /^2 working$/, quiet: /^claude in main done (just now|\d+m ago)$/, gone: /^claude in main exited$/ };
for (const [name, status] of Object.entries(sessionRows)) {
  const row = homeRows.find((item) => item.name === name);
  if (!status.test(row?.status ?? "") || row.count || !/ago|now/.test(row.state)) await fail(`the ${name} row reads ${JSON.stringify(row)}, not "${status}" with a time`);
}
await tabsInView();
await oneWorkingColor("home", 1);
// ^ The server runs this page's build: no new-version pill.
if (await count(".build-pill")) await fail("the new-version pill shows on the same build");
await shot("home");
await motion();
await resize([1280, 800, 1]);
await motion();
await homeUnread();
await resize([390, 844, 2]);
await go("#/");
await homeTimers();
// Sessions, one tap from home: the session inbox, without task workers and the manager, with
// the plain worktree's session.
await click(".tabbar a[href='#/sessions']");
await until("location.hash === '#/sessions' && document.querySelector('h1.title')?.textContent === 'Sessions'", "the Sessions tab did not open the inbox");
const listedSessions = await text(".rows .row-context");
if (listedSessions.some((line) => /\btask-(\d+|manager)\b|\b(task|manager)$/.test(line))) await fail(`the session list shows task sessions: ${JSON.stringify(listedSessions)}`);
if (!listedSessions.includes("old chat · claude")) await fail(`the session list misses the plain worktree's session: ${JSON.stringify(listedSessions)}`);
if ((await text(".tabbar a[aria-current=page]")).join() !== "Sessions") await fail("the Sessions tab is not marked current");
await oneWorkingColor("the session rows", 2);
// ^ A working session's mark pulses (the shared `pulse`, `data-running`); no other mark does,
// and none moves under reduced motion (docs/web-client-tasks.md "Home").
const inboxMarks = () => evaluate(`[...document.querySelectorAll('.rows .row .row-link > .mark')].map((mark) => { const row = mark.closest('.row');
  return { running: mark.hasAttribute('data-running'), working: row.querySelector('.row-state')?.dataset.tone === 'working',
    animated: getComputedStyle(mark).animationName !== 'none' }; })`);
const moving = await inboxMarks();
if (moving.filter((mark) => mark.working).length < 2 || moving.some((mark) => mark.running !== mark.working || mark.animated !== mark.working)) await fail(`only working inbox marks pulse: ${JSON.stringify(moving)}`);
await setMotion("reduce");
const still = await inboxMarks();
await setMotion("no-preference");
if (still.some((mark) => mark.animated) || !still.some((mark) => mark.running)) await fail(`an inbox mark moves under reduced motion: ${JSON.stringify(still)}`);
await shot("sessions");
// A project's worktrees are in its chat's ⋯ menu; the plain worktree is listed like any
// other, with no Adopted list.
await click(".tabbar a[href='#/']");
await until("location.hash === '#/' && !!document.querySelector('.home-row')", "the Home tab did not return home");
await go(chat);
// ^ The manager's log sets --log-scrollbar as the session chat does (useFollowEnd in lib.js): its wide blocks leave a classic scrollbar out.
await until("(() => { const log = document.querySelector('.manager .log'); return !!log && parseFloat(getComputedStyle(log.parentElement).getPropertyValue('--log-scrollbar') || '-1') === log.offsetWidth - log.clientWidth; })()", "the manager's log did not take its scrollbar width for the wide blocks");
await click(".more-button");
await evaluate("[...document.querySelectorAll('.more-menu a')].find((node) => node.textContent === 'Sessions').click()");
await until(`location.hash === '#/p/${project}/sessions' && !!document.querySelector('.worktree')`, "the ⋯ menu did not open the worktrees");
await plainWorktree("the worktree list");
await shot("project-worktrees");
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until(`location.hash === ${JSON.stringify(chat)}`, "Back from the worktrees did not return to the chat");
// A row opens the project's chat.
await go("#/");
await until("!!document.querySelector('.home-row')", "home did not come back");
await evaluate(`[...document.querySelectorAll('.home-row')].find((row) => row.querySelector('.row-label').firstChild.textContent === 'ui').click()`);
await until(`location.hash === ${JSON.stringify(chat)} && !!document.querySelector('.composer-field') && !!document.querySelector('.pill') && !!document.querySelector('.log .bubble')`, "a home row did not open the chat");
if (await count(".tabbar")) await fail("the chat shows the bottom bar");
// No Adopted list on the chat.
if (await count(".adopted")) await fail("the project chat shows the Adopted list");

// "Since you left": this device left the project two hours ago; the line names what happened.
// It sits in one compact line and goes by itself once the newest reply is on screen or the owner
// sends. The log is scrolled to its top first, so no reply is on screen when the line appears.
// ^ The line watches the newest bubble not the owner's (`SinceLine` in tasks.js): it must be out of
// view, twice 300 ms apart, before the device comes back, or the line goes as it appears. The
// chat may still follow its end after a sheet closes or a reply arrives, so the top is set again
// on each try.
const newestOutOfView = `(() => { const log = document.querySelector('.log'); log.scrollTop = 0;
  const last = [...log.querySelectorAll('.bubble[data-side="in"]')].at(-1); if (!last) return false;
  const box = log.getBoundingClientRect(), at = last.getBoundingClientRect(); return at.top >= box.bottom || at.bottom <= box.top; })()`;
const sinceState = `(() => { const log = document.querySelector('.log'), last = [...log.querySelectorAll('.bubble[data-side="in"]')].at(-1), box = log.getBoundingClientRect(), at = last?.getBoundingClientRect();
  return JSON.stringify({ since: document.querySelector('.since')?.textContent ?? null, top: log.scrollTop, height: log.scrollHeight, client: log.clientHeight, newestTop: at && Math.round(at.top - box.top), newestAt: last?.dataset.at ?? null }); })()`;
async function comeBack() {
  await until("!document.querySelector('.since')", "an old since line is still shown");
  // ^ A reply or a closing sheet can make the chat follow its end after the check and drop the
  // line as it appears: the whole come-back is tried again, up to three times.
  const shown = "/Landed .*Ready .*Started .*repl(y|ies)/.test(document.querySelector('.since')?.textContent ?? '')";
  for (let round = 0; round < 3 && !(await evaluate(shown)); round++) {
    let still = false;
    for (let attempt = 0; attempt < 40 && !still; attempt++) {
      still = (await evaluate(newestOutOfView)) && (await pause(300), await evaluate(newestOutOfView));
    }
    if (!still) await fail(`the newest reply does not stay out of view at the top of the chat: ${await evaluate(sinceState)}`);
    await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 })); document.dispatchEvent(new Event('visibilitychange'))`);
    // ^ A task that is ready is listed like the digest lists it (`view::REPORTED_STATES`).
    for (let attempt = 0; attempt < 100 && !(await evaluate(shown)); attempt++) await pause(100);
  }
  if (!(await evaluate(shown))) await fail(`no since-you-left line: ${await evaluate(sinceState)}`);
}
await go("#/");
// ^ The chat notes the time it closes at: set the earlier time after home replaced it.
await until("!!document.querySelector('.home-row') && !document.querySelector('.composer-field')", "home did not replace the chat");
await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 }))`);
await go(chat);
await until("!!document.querySelector('.log .bubble')", "the chat shows no bubble");
await comeBack();
if ((await evaluate("getComputedStyle(document.querySelector('.since')).animationName")) !== "since-in") await fail("the since line does not expand in");
// ^ The line wraps and is never cut (`SinceLine`): at most two lines, no clipped text, the dismiss button beside it.
const sinceFit = `(() => { const line = document.querySelector('.since'), p = line.querySelector('p'), b = line.querySelector('button').getBoundingClientRect(), r = p.getBoundingClientRect();
  const lines = Math.round(p.scrollHeight / parseFloat(getComputedStyle(p).lineHeight));
  return { text: p.textContent, lines, clipped: p.scrollWidth > p.clientWidth || p.scrollHeight > p.clientHeight + 1, inline: b.top < r.bottom && b.bottom > r.top, left: b.left >= r.right - 1 }; })()`;
const sinceBox = await evaluate(sinceFit);
if (sinceBox.lines > 2 || sinceBox.clipped || !sinceBox.inline || !sinceBox.left || /^Since you left/.test(sinceBox.text)) await fail(`the since line is not a short wrapped line: ${JSON.stringify(sinceBox)}`);
await shot("since");
await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
for (let attempt = 0; attempt < 100 && (await count(".since")); attempt++) await pause(100);
if (await count(".since")) {
  await fail(`the since line stayed after the newest reply was on screen: ${await evaluate(`(() => { const log = document.querySelector('.log'), list = [...log.querySelectorAll('.bubble[data-side=in]')], last = list.at(-1), box = log.getBoundingClientRect(), at = last?.getBoundingClientRect();
    return JSON.stringify({ top: log.scrollTop, height: log.scrollHeight, client: log.clientHeight, at: last?.dataset.at ?? null, bubbleTop: at && Math.round(at.top - box.top), bubbleBottom: at && Math.round(at.bottom - box.top), bubbles: list.length, attrs: list.map((n) => n.dataset.at ?? '-').join() }); })()`)}`);
}
await comeBack();
await evaluate("[...document.querySelectorAll('.since button')].find((node) => node.title === 'Dismiss').click()");
await until("!document.querySelector('.since')", "Dismiss did not close the line");
// A long list at 390 px: six landed tasks, ready, blocked, started and replies. Every group word is
// shown whole on at most two lines, and the ids that do not fit become "+N more".
// ^ The answer is rewritten, as a log with that many events would give it (the fixture has two landed).
{
  await evaluate(`(() => { const real = window.fetch; window.__sinceFetch = real; window.fetch = async (url, ...rest) => String(url).includes('/since')
    ? new Response(JSON.stringify({ after: 0, replies: 3, tasks: { landed: [322, 324, 325, 326, 327, 329], ready: [330], blocked: [331, 332], started: [333, 334, 335, 336, 337, 338, 339, 340] } }), { headers: { 'content-type': 'application/json' } })
    : real(url, ...rest); })()`);
  await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 })); document.dispatchEvent(new Event('visibilitychange'))`);
  await until("!!document.querySelector('.since')", "no since line for the long list");
  const long = async () => {
    await until(`(() => { const p = document.querySelector('.since p'); return !!p && p.textContent.includes('Landed') && p.textContent.includes('replies'); })()`, 'no long since line at 390 px');
    await pause(300);
    return evaluate(sinceFit);
  };
  const phone = await long();
  const words = ["Landed", "Ready", "Blocked", "Started", "3 replies"];
  if (phone.lines > 2 || phone.clipped || words.some((word) => !phone.text.includes(word)) || !/\+\d+ more/.test(phone.text)) await fail(`the long since line at 390 px: ${JSON.stringify(phone)}`);
  await shot("since-long");
  await evaluate("window.fetch = window.__sinceFetch");
  await evaluate("document.querySelector('.since button[title=Dismiss]').click()");
  await until("!document.querySelector('.since')", "Dismiss did not close the long line");
}
// ^ The chat opens where it was left: at its end again for what follows.
await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
await pause(300);
await go("#/");
await until("!!document.querySelector('.home-row') && !document.querySelector('.composer-field')", "home did not replace the chat");
await go(chat);
await until("!!document.querySelector('.log .bubble') && !!document.querySelector('.pill')", "the chat did not come back");

// --- Mentions in the composer (docs/web-client-tasks.md "Project screen"): `/` lists the owner's
// skills and inserts a request to use one (the manager takes no slash command), `@` lists the
// other projects; no row of the first read slid in.
if (await count(".log [data-fresh]")) await fail("rows of the first read play the entry");
const optionNamed = (name) => `[...document.querySelectorAll('.slash-option')].find((node) => node.querySelector('.slash-name')?.textContent === ${JSON.stringify(name)})`;
// A failed read shows the server's error and a Retry in the list, not "Loading…".
await evaluate(`(() => { const real = window.fetch; globalThis.realFetch = real;
  window.fetch = (url, init) => String(url).includes('/mentions') ? Promise.resolve(new Response(JSON.stringify({ error: { code: 'unavailable', message: 'The host is busy.' } }), { status: 503 })) : real(url, init); })()`);
await typeInto(".composer-field", "/");
await until("document.querySelector('.slash-failed')?.textContent === 'The host is busy.' && [...document.querySelectorAll('.slash-note button')].some((node) => node.textContent === 'Retry')",
  "a failed / list does not show its error and Retry", 30);
if (await count(".slash-option")) await fail("a failed / list shows options");
await shot("mentions-failed", ".composer-send");
await evaluate("window.fetch = globalThis.realFetch; [...document.querySelectorAll('.slash-note button')].find((node) => node.textContent === 'Retry').click()");
// ^ Retry reads again: `/` alone lists the first 50 of the owner's 73 linked skills.
if ((await evaluate("document.activeElement?.className")) !== "composer-field") await fail("Retry took the focus from the field");
await until("document.querySelectorAll('.slash-option').length === 50", "the / list does not show the first 50 skills", 30);
await shot("mentions-all-skills", ".composer-send");
await typeInto(".composer-field", "/fixture");
await until(`!!${optionNamed("/fixture-skill")}`, "the / list does not show the fixture skill", 30);
const skillOption = await evaluate(`(() => { const node = ${optionNamed("/fixture-skill")}; return { description: node.querySelector('.slash-description').textContent, source: node.querySelector('.slash-source').textContent,
  label: document.querySelector('.slash-list').getAttribute('aria-label') }; })()`);
if (skillOption.description !== "Fixture skill for the manager" || skillOption.source !== "skill" || skillOption.label !== "Skills") await fail(`the skill option reads ${JSON.stringify(skillOption)}`);
await shot("mentions-skills", ".composer-send");
await evaluate(`${optionNamed("/fixture-skill")}.click()`);
if ((await evaluate("document.querySelector('.composer-field').value")) !== "Use owner:fixture-skill: ") await fail(`a picked skill reads ${await evaluate("document.querySelector('.composer-field').value")}`);
// ^ `/` opens the list at the start of any word, and a pick after other words reads as part of the sentence.
await typeInto(".composer-field", "Then /fixture");
await until(`!!${optionNamed("/fixture-skill")}`, "the / list does not open after other words", 30);
await shot("mentions-skills-mid-text", ".composer-send");
await evaluate(`${optionNamed("/fixture-skill")}.click()`);
if ((await evaluate("document.querySelector('.composer-field').value")) !== "Then owner:fixture-skill ") await fail(`a skill picked after other words reads ${await evaluate("document.querySelector('.composer-field').value")}`);
await typeInto(".composer-field", "see src/fix");
await pause(300);
if (await count(".slash")) await fail("a / inside a word opens the skill list");
await typeInto(".composer-field", "Ask @pla");
await until(`!!${optionNamed("@plans")}`, "the @ list does not show the second project", 30);
await shot("mentions-projects", ".composer-send");
await evaluate(`${optionNamed("@plans")}.click()`);
if ((await evaluate("document.querySelector('.composer-field').value")) !== "Ask @plans ") await fail(`a picked project reads ${await evaluate("document.querySelector('.composer-field').value")}`);
await typeInto(".composer-field", "");
await until("!document.querySelector('.slash')", "the list stayed over an empty field");

// --- The chat reads like a messaging app: no cards, no tray, no strip, no Explain; the
// header names the manager only when it matters.
for (const gone of [".need-card", ".tray", ".brief", ".events"]) {
  if (await count(gone)) await fail(`the chat still has ${gone}`);
}
if (await evaluate("/Explain/.test(document.querySelector('#app').innerText)")) await fail("the chat still offers Explain");
if (await evaluate("document.querySelector('.manager .bar-state')?.textContent ?? ''")) await fail(`an idle manager is named in the header: ${await evaluate("document.querySelector('.manager .bar-state').textContent")}`);
// The owner's message on the right, the manager's reply on the left with its author. A worker's
// result is a muted line ("#5 ready"), never a bubble: its report is in the task sheet.
let shown = await bubbles();
const ownerAsk = shown.find((bubble) => bubble.side === "out" && bubble.text.startsWith("RUN wsx task land 3"));
// ^ ui's manager is named "Atlas" (tasks.manager.name); plans' reads "plans", its project.
const managerReply = shown.find((bubble) => bubble.side === "in" && bubble.author === "Atlas");
if (!ownerAsk || ownerAsk.author || !managerReply) await fail(`the bubbles are ${JSON.stringify(shown)}`);
await until(statusSays(5, "ready"), "#5's result left no muted line");
if (await evaluate("/line 30 of the report/.test(document.querySelector('.log').textContent) || [...document.querySelectorAll('.bubble-author')].some((node) => / worker$/.test(node.textContent))")) await fail("a worker's report is in the chat");
await managerWidth("the phone chat");
// ^ The manager's agent in use sits by its name, from the chat's `manager.agent` (docs/web-client-tasks.md "Agent").
const managerAgent = await evaluate(`fetch('api/tasks/${project}/chat').then((r) => r.json()).then((view) => view.manager.agent)`);
if (managerAgent?.adapter !== "claude" || managerAgent.model !== "fake-manager" || managerAgent.fallback !== false) await fail(`the manager's agent field is ${JSON.stringify(managerAgent)}`);
if ((await text(".bar-title .bar-model"))[0] !== `${managerAgent.model} ${managerAgent.effort}`) await fail("the manager chat's top row does not name the model and effort");
await shot("project", ".composer-send");
// The chat opens at its newest message; scrolled up, it shows Jump to latest.
for (let attempt = 0; attempt < 100 && !(await evaluate(atEnd)); attempt++) await pause(100);
if (!(await evaluate(atEnd))) await fail(`the manager chat did not open at its end: ${await evaluate(`JSON.stringify({ top: document.querySelector('.log').scrollTop,
  height: document.querySelector('.log').scrollHeight, client: document.querySelector('.log').clientHeight, read: localStorage.getItem('wsx-read'), visible: document.visibilityState })`)}`);
await evaluate("document.querySelector('.log').scrollTop = 0");
await until("!!document.querySelector('.jump')", "no jump button after scrolling the manager chat up");
await click(".jump");
await until(`!document.querySelector('.jump') && ${atEnd}`, "Jump to latest did not go to the end of the manager chat");
// A landed task is the muted line "#7 done".
await until(`${statusSays(7, "done")} && ${statusSays(8, "done")}`, "the landed tasks have no muted line");
// ^ One line per task change: #5's close is the events line "#5 closed", never also its
// resolution "#5 was closed and discarded".
await until(statusSays(5, "closed"), "#5's close left no muted line");
const closeLines = await evaluate(`${statusLines}.map((node) => node.textContent.trim()).filter((words) => /^#5 (closed|was closed)/.test(words))`);
if (closeLines.length !== 1) await fail(`#5's close shows as ${JSON.stringify(closeLines)}`);
// ^ The manager's reply to a turn of only events (the fake says "noted events") is a muted line
// under the manager's name, never a bubble and never joined to an event.
await until("[...document.querySelectorAll('.reply-line .reply-text')].some((node) => node.textContent.includes('noted events'))", "a reply to events is not a muted line", 60);
if (await evaluate("[...document.querySelectorAll('.bubble, .status-line')].some((node) => node.textContent.includes('noted events'))")) await fail("a reply to events is a bubble or joined to an event");
if (await evaluate("document.querySelector('.reply-line')?.firstElementChild?.textContent !== 'Atlas'")) await fail("the first reply to events is not under the manager's name");
// ^ Each event its own left-aligned line: at most one chip, only at its start; a task number
// inside a sentence, also in a reply to events, is plain text.
const eventLines = await evaluate(`[...document.querySelectorAll('.status-line')].filter((node) => / · /.test(node.textContent) || getComputedStyle(node).textAlign === 'center'
  || node.querySelectorAll('.task-chip').length > (node.firstChild?.matches?.('.task-chip') ? 1 : 0)).map((node) => node.textContent)
  .concat([...document.querySelectorAll('.reply-text .task-chip')].map((node) => 'reply chip ' + node.textContent))`);
if (eventLines.length) await fail(`the event lines read ${JSON.stringify(eventLines)}`);
// ^ A long reply shows its first sentence whole and More; More shows the rest.
const longReply = "[...document.querySelectorAll('.reply-text')].find((node) => node.querySelector('button'))";
const longShown = `${longReply}?.firstChild?.textContent === 'noted events.'`;
// ^ A row after it (an event line) folds it with the run above (bubbles.js `folded`): the folds open.
const lastRows = "[...document.querySelectorAll('.log > *')].slice(-8).map((node) => `${node.className}: ${node.textContent.trim().slice(0, 60)}`)";
let foldedRows = null;
for (let attempt = 0; attempt < 60 && !(await evaluate(longShown)); attempt++) {
  if (attempt === 30 && await count(".log .fold-toggle[aria-expanded=false]")) {
    foldedRows = await evaluate(lastRows);
    await evaluate("document.querySelectorAll('.log .fold-toggle[aria-expanded=false]').forEach((node) => node.click())");
  }
  await pause(100);
}
if (foldedRows) fs.appendFileSync(path.join(captures, "follow-end.txt"), `the long reply to events was in a closed fold: ${JSON.stringify(foldedRows)}\n`);
if (!(await evaluate(longShown))) await fail(`a long reply to events does not show its first sentence and More; the chat's last rows are ${JSON.stringify(await evaluate(lastRows))}`);
// ^ The page in a scheme once its marks have faded to the scheme's colors (app.css transitions).
const inScheme = async (scheme) => { await evaluate(scheme === "light" ? "document.documentElement.dataset.scheme = 'light'" : "delete document.documentElement.dataset.scheme"); await pause(1000); };
for (const scheme of ["dark", "light"]) {
  await inScheme(scheme);
  await evaluate(`${longReply}.scrollIntoView({ block: 'center' })`);
  await shot(`events-${scheme}`);
}
// ^ Back to the dark scheme's colors before a later check reads them.
await inScheme("dark");
await evaluate(`${longReply}.querySelector('button').click()`);
await until("[...document.querySelectorAll('.reply-text')].some((node) => /^noted events\\. #\\d+ landed, so its change is on main and nothing else waits for you\\.$/.test(node.textContent))", "More did not show the whole reply");
await evaluate("[...document.querySelectorAll('.log .bubble, .log .status-line, .log .reply-line')].at(-1).scrollIntoView({ block: 'end' })");
await until(atEnd, "the chat did not go back to its end");

// --- The pills: the composer row holds one pill for each of Running, Needs you and To check,
// between the attach button and Send, each an icon and its count (the label from 900 px, the wide layout) and
// none for a count of 0. A tap opens that pill's list over the composer; one list at a time. No
// deploy facts here. Opening a list keeps the chat at its end.
const pillWords = () => evaluate("[...document.querySelectorAll('.pill')].map((node) => node.getAttribute('aria-label')).join(' | ')");
const pressed = () => evaluate("[...document.querySelectorAll('.pill[aria-pressed=true]')].map((node) => node.dataset.kind).join()");
// ^ Routines is the fourth pill: nightly and idle are on (weekly is off), and its list is read after the chat.
const PILLS = "1 running | 6 need you | 2 to check | 2 routines";
const expectPills = async (who) => {
  for (let attempt = 0; attempt < 100 && (await pillWords()) !== PILLS; attempt++) await pause(100);
  if ((await pillWords()) !== PILLS) await fail(`${who} read ${await pillWords()}`);
};
await expectPills("the pills");
const pillLook = await evaluate(`(() => { const row = document.querySelector('.composer-row').getBoundingClientRect(), send = document.querySelector('.composer-send').getBoundingClientRect();
  const attach = document.querySelector('.composer-row > .icon-button')?.getBoundingClientRect(), mid = (box) => (box.top + box.bottom) / 2;
  return [...document.querySelectorAll('.pill')].map((node) => { const box = node.getBoundingClientRect();
    return { kind: node.dataset.kind, count: node.querySelector('.pill-count').textContent.trim(), icon: !!node.querySelector('svg.icon'), label: getComputedStyle(node.querySelector('.pill-label')).display,
      pressed: node.getAttribute('aria-pressed'), between: box.left >= (attach?.right ?? row.left) - 1 && box.right <= send.left + 1, level: Math.abs(mid(box) - mid(send)) < 3, tall: box.height >= 24 && box.height <= 32, clear: box.top - document.querySelector('.composer-input').getBoundingClientRect().bottom >= 6 }; }); })()`);
if (pillLook.map((pill) => `${pill.kind}:${pill.count}`).join() !== "running:1,needs:6,check:2,routines:2" || pillLook.some((pill) => !pill.icon || pill.label !== "none" || pill.pressed !== "false" || !pill.between || !pill.level || !pill.tall || !pill.clear)) {
  await fail(`the pills are not an icon and a count between the attach button and Send on one row, small and clear of the field, at 390 px: ${JSON.stringify(pillLook)}`);
}
if (await count(".reservoir-list") || await count(".reservoir")) await fail("a list is open before a tap");
const inStore = (body) => evaluate(`(async () => { const { state, update } = await import(new URL('store.js', document.querySelector('script[type=module]').src)); ${body} })()`);
// ^ At rest a pill reads as a pill: a fully rounded shape with a fill and a hairline; pressed, its border is the accent.
const pillShape = await evaluate(`(() => { const node = document.querySelector('.pill'), style = getComputedStyle(node), box = node.getBoundingClientRect();
  return { radius: parseFloat(style.borderTopLeftRadius) >= box.height / 2, fill: style.backgroundColor !== 'rgba(0, 0, 0, 0)', edge: parseFloat(style.borderTopWidth) > 0 }; })()`);
if (!pillShape.radius || !pillShape.fill || !pillShape.edge) await fail(`a pill has no pill shape at rest: ${JSON.stringify(pillShape)}`);
await shot("pills-rest", ".composer-send");
// A device without task_act (a view-only grant: its session lists no such capability) has no
// composer, and still has the pills and their lists, with no action on any row.
const withCapabilities = (list) => inStore(`update({ site: { ...state.site, capabilities: ${JSON.stringify(list)} } })`);
const capabilities = await inStore("return [...state.site.capabilities]");
await withCapabilities(capabilities.filter((name) => name !== "task_act"));
await until("!document.querySelector('.composer-field') && document.querySelectorAll('.pill').length === 4", "a device without task_act lost its pills");
if (!(await evaluate("!!document.querySelector('.pills.pill-bar') && !document.querySelector('.composer-send') && !document.querySelector('.composer-row .pill')"))) await fail("the pills of a device without task_act are not in a bar of their own");
await expectPills("the pills of a device without task_act");
await openReservoir();
if ((await rows()).length !== 6 || (await rows()).some((row) => row[3] !== "")) await fail(`a device without task_act has actions in the Needs you list: ${JSON.stringify(await rows())}`);
await shot("pills-view-only", ".pill-bar");
await openReservoir("running");
if ((await rows()).length !== 1 || (await rows())[0][0] !== "#6") await fail(`a device without task_act sees the Running rows ${JSON.stringify(await rows())}`);
await openReservoir("check");
if ((await rows()).length !== 2 || (await rows()).some((row) => row[3] !== "")) await fail(`a device without task_act sees the To check rows ${JSON.stringify(await rows())}`);
await click(".pill[aria-pressed=true]");
await until("!document.querySelector('.reservoir')", "a tap on the open pill did not close the list of a device without task_act");
await withCapabilities(capabilities);
await until("!!document.querySelector('.composer-row .pill') && !document.querySelector('.pill-bar')", "the pills did not go back into the composer row");

const closedSize = await evaluate("getComputedStyle(document.querySelector('.pill')).fontSize");
await oneWorkingColor("the chat's chips", 0);

// Counts change as events change them (docs/web-client-tasks.md "Motion"): a number ticks, and
// under reduced motion it does not. A pill with a count of 0 is gone. The store is changed in
// the page, as an event would.
const original = await inStore(`return JSON.parse(JSON.stringify(state.tasks[${project}]))`);
const runners = () => inStore(`const ov = state.tasks[${project}], base = ov.tasks.find((task) => task.stage === 'working');
  update({ tasks: { ...state.tasks, [${project}]: { ...ov, tasks: [...ov.tasks, { ...base, id: 91, title: 'second runner' }, { ...base, id: 92, title: 'third runner' }] } } })`);
const restored = () => inStore(`update({ tasks: { ...state.tasks, [${project}]: ${JSON.stringify(original)} } })`);
await runners();
const runningIs = (label, where) => until(`document.querySelector('.pill[data-kind=running]')?.getAttribute('aria-label') === '${label}'`, where);
await runningIs("3 running", "the Running pill did not count three tasks");
if ((await evaluate("getComputedStyle(document.querySelector('.pill-count .tick')).animationName")) !== "tick-in") await fail("a changed count does not tick");
await restored();
await runningIs("1 running", "the Running pill did not go back to one task");
// ^ Nothing waits and nothing is left to check: only Running stays; with nothing, no pill at all.
await inStore(`const ov = state.tasks[${project}]; update({ tasks: { ...state.tasks, [${project}]: { ...ov, needs: [], asks: [], to_check: 0, check_items: [] } } })`);
await until("(document.querySelector('.pills')?.textContent ?? '').includes('1') && !document.querySelector('.pill[data-kind=needs]') && !document.querySelector('.pill[data-kind=check]')", "a pill with a count of 0 is still shown");
if ((await pillWords()) !== "1 running | 2 routines") await fail(`with nothing to answer or check the pills read ${await pillWords()}`);
await inStore(`const ov = state.tasks[${project}]; update({ tasks: { ...state.tasks, [${project}]: { ...ov, tasks: ov.tasks.filter((task) => task.stage !== 'working') } } })`);
await until("document.querySelectorAll('.pill').length === 1 && !!document.querySelector('.pill[data-kind=routines]')", "with the three counts at 0 a pill other than Routines is still shown");
await restored();
await until("document.querySelectorAll('.pill').length === 4", "the pills did not come back");
// Reduced motion: the count does not tick.
await setMotion("reduce");
await go("#/");
await go(chat);
await until("!!document.querySelector('.pill')", "the chat did not come back for reduced motion");
await runners();
await runningIs("3 running", "no count under reduced motion");
const calmNow = await evaluate("({ tick: getComputedStyle(document.querySelector('.pill-count .tick')).animationName })");
if (calmNow.tick !== "none") await fail(`reduced motion still animates the count: ${JSON.stringify(calmNow)}`);
await setMotion("no-preference");
await restored();
await runningIs("1 running", "the Running pill did not go back to one task");

// Open, a list's first row shows whole at the reservoir's top, and the pill is pressed.
async function firstRowInside(where) {
  const seen = await evaluate(`(() => { const body = document.querySelector('.reservoir-scroll'), box = body.getBoundingClientRect(), row = body.querySelector('.need').getBoundingClientRect();
    return { down: Math.round(row.top - box.top), inside: row.top >= box.top && row.bottom <= box.bottom && row.left >= box.left && row.right <= box.right + 0.5 && body.scrollTop === 0,
      box: [box.top, box.bottom, box.left, box.right].map(Math.round), row: [row.top, row.bottom, row.left, row.right].map(Math.round) }; })()`);
  if (!seen.inside || seen.down > 4) await fail(`${where}: the open list's first row is off: ${JSON.stringify(seen)}`);
}
await openReservoir();
await firstRowInside("with items that need the owner");
if ((await pressed()) !== "needs" || (await count(".reservoir-list")) !== 1) await fail(`after a tap on Needs you ${await pressed()} is pressed`);
if (!(await evaluate("document.querySelector('.pill[data-kind=needs]').getAttribute('aria-controls') === 'reservoir-list' && !!document.getElementById('reservoir-list')"))) await fail("the open pill does not name its list");
await shot("pills-needs-open", ".composer-send");
if (!(await evaluate(atEnd))) await fail(`opening a list took the chat off its end: ${JSON.stringify(await evaluate("(() => { const log = document.querySelector('.log'); return { top: log.scrollTop, height: log.scrollHeight, client: log.clientHeight, jump: !!document.querySelector('.jump') }; })()"))}`);
const listedRows = await rows();
const rowWords = listedRows.map((row) => `${row[0]} ${row[3]}`);
// ^ Done tasks (#7, #8) are not in it. A task without --title is named by its worker summary's
// first sentence ("did: WORK b.txt").
const expectedRows = ["#1 Answer", "#4 Answer", " Deny+Approve", " Deny+Approve", "#2 Land", "#3 Land"];
if (rowWords.join("|") !== expectedRows.join("|")) await fail(`the Needs you rows are ${JSON.stringify(listedRows)}`);
if (listedRows[0][2] !== "Waiting for your answer" || listedRows[2][1] !== "Approve land #3" || listedRows[3][1] !== "Approve discard #4" || listedRows[2][2] !== "did: WORK b.txt"
  || listedRows[4][2] !== "Ready, waiting for you to land it") await fail(`the Needs you rows read ${JSON.stringify(listedRows)}`);
// ^ One list at a time: a tap on another pill switches, a tap on the open pill closes.
await click(".pill[data-kind=running]");
await until("!!document.querySelector('.reservoir-list[aria-label=Running]') && !document.querySelector('.reservoir-list[aria-label=\"Needs you\"]')", "a tap on Running did not switch the list");
if ((await pressed()) !== "running" || (await count(".reservoir-list")) !== 1) await fail(`after a tap on Running ${await pressed()} is pressed`);
const runningRows = await rows();
if (runningRows.length !== 1 || runningRows[0][0] !== "#6" || !/^keep a worker busy WAIT /.test(runningRows[0][1])) await fail(`the Running rows are ${JSON.stringify(runningRows)}`);
await click(".pill[data-kind=check]");
await until("!!document.querySelector('.reservoir-list[aria-label=\"To check\"]') && !document.querySelector('.reservoir-list[aria-label=Running]')", "a tap on To check did not switch the list");
const checkRows = await rows();
// ^ One row per open item: its task, its check, what passes it, and Reply, Fail and Pass as the sheet has them.
const openChecks = await evaluate(`fetch('api/tasks/${project}/checks').then((r) => r.json()).then((list) => list.open.map((item) => ['#' + item.task, item.check, 'Passes if ' + item.pass_if, 'Reply+Fail+Pass']))`);
if ((await pressed()) !== "check" || openChecks.length !== 2 || JSON.stringify(checkRows) !== JSON.stringify(openChecks)) await fail(`the To check list is ${JSON.stringify(checkRows)}, not ${JSON.stringify(openChecks)}, with ${await pressed()} pressed`);
// ^ The row's check keeps room: its text ends before its first action at 390 px.
const checkRoom = await evaluate(`[...document.querySelectorAll('.reservoir-list .need')].map((row) => row.querySelector('.row-main').getBoundingClientRect().right <= row.querySelector('.row-actions').getBoundingClientRect().left + 1)`);
if (checkRoom.includes(false)) await fail(`a To check row's text runs under its actions: ${JSON.stringify(checkRoom)}`);
await shot("pills-check-open", ".composer-send");
await click(".pill[data-kind=check]");
await until("!document.querySelector('.reservoir')", "a tap on the open pill did not close its list");
if ((await pressed()) !== "") await fail(`a closed list leaves ${await pressed()} pressed`);
await shot("pills", ".composer-send");
await listHeaders();
await openReservoir();
// ^ Every row of the open list, its labels and its context, is the pills' small size.
const sizes = await evaluate("[...document.querySelectorAll('.reservoir-body :is(.row-label, .row-context)')].map((node) => getComputedStyle(node).fontSize)");
if (!sizes.length || sizes.some((size) => size !== closedSize)) await fail(`the open list's words are not all ${closedSize}: ${JSON.stringify(sizes)}`);
// ^ The three lists use one row type: the same sizes, label and context looks, spacing and marks,
// and a task's chip first (docs/web-client-tasks.md "Project screen").
const listLook = async (kind) => {
  await openReservoir(kind);
  return evaluate(`(() => { const row = document.querySelector('.reservoir-list .need'), part = (selector) => getComputedStyle(row.querySelector(selector)), link = getComputedStyle(row.querySelector('.row-link'));
    return { label: [part('.row-label').fontSize, part('.row-label').fontWeight, part('.row-label').color].join(), context: [part('.row-context').fontSize, part('.row-context').color].join(),
      chip: part('.task-chip').fontSize, row: [link.paddingTop, link.paddingBottom, link.minHeight, link.columnGap, link.fontSize].join(), first: row.querySelector('.row-link').firstElementChild.className, number: ${NUMBER} }; })()`);
};
const looks = { running: await listLook("running"), needs: await listLook("needs"), check: await listLook("check") };
const same = (field) => new Set(Object.values(looks).map((look) => look[field])).size === 1;
if (!["label", "context", "chip", "row"].every(same)) await fail(`the three lists do not share one row type: ${JSON.stringify(looks)}`);
if (looks.running.first !== "task-chip" || looks.running.number !== "#6" || looks.check.first !== "task-chip" || looks.needs.first !== "task-chip") await fail(`a row does not start with its task chip: ${JSON.stringify(looks)}`);
if (looks.running.chip !== closedSize) await fail(`the chip is ${looks.running.chip}, not ${closedSize}`);
await openReservoir();
// ^ The open list reads its tasks ahead: a row's sheet shows its content in the frame it opens,
// with no loading step, on a phone and a wide screen.
for (const [width, height, scale] of [[390, 844, 2], [1280, 800, 1]]) {
  await resize([width, height, scale]);
  await openReservoir();
  // ^ The read ahead of every item's task (`readTaskAhead`) leaves no mark the page shows: wait for the page's requests to end.
  await until("globalThis.wsxFetching === 0", "the read ahead of the tasks did not end");
  const atOnce = await evaluate(`new Promise((done) => { ${rowOf("#2")}.querySelector('.row-open').click();
    requestAnimationFrame(() => requestAnimationFrame(() => done({ open: !!document.querySelector('dialog.task-sheet'),
      loading: !!document.querySelector('dialog.task-sheet .loading'), story: !!document.querySelector('dialog.task-sheet .story') }))); })`);
  if (!atOnce.open || atOnce.loading || !atOnce.story) await fail(`a need's sheet did not show its content at once at ${width} px: ${JSON.stringify(atOnce)}`);
  await shot(`need-open-${width}`, ".composer-send");
  await closeSheet("a need's sheet", chat);
  // ^ #3's land is a pending approval: its sheet offers Approve and Deny, not a Land beside them (`owner_actions`).
  await openReservoir();
  await evaluate(`${rowOf("#3")}.querySelector('.row-open').click()`);
  await until("!!document.querySelector('dialog.task-sheet .task-next button[aria-label=\"Approve land #3\"]')", `#3's sheet shows no Approve at ${width} px`);
  if ((await nextButtons()).join() !== "Deny,Approve,Take over") await fail(`#3 with a pending land approval offers ${await nextButtons()} at ${width} px`);
  if ((await evaluate("fetch('api/tasks/' + location.hash.split('/')[2] + '/t/3').then((r) => r.json()).then((task) => task.actions)")).includes("land")) await fail("the API offers Land beside a pending land approval");
  await closeSheet("#3's sheet", chat);
}
// ^ In the wide layout (900 px) three pills carry their words after the count; with four the composer shows icon and count only, so the field keeps a quarter of the row at least (170 of 680 px at 1280 px).
await resize([1280, 800, 1]);
await until("document.querySelectorAll('.pill').length === 4", "the pills are not in the wide chat");
const wide = await evaluate(`(() => ({ labels: [...document.querySelectorAll('.pill')].map((node) => getComputedStyle(node.querySelector('.pill-label')).display === 'none' ? '' : node.querySelector('.pill-label').textContent.trim()).join('|'),
  names: [...document.querySelectorAll('.pill')].map((node) => node.getAttribute('aria-label')).join('|'),
  gap: document.querySelector('.composer-input').getBoundingClientRect().left - [...document.querySelectorAll('.pill')].at(-1).getBoundingClientRect().right, icon: parseFloat(getComputedStyle(document.querySelector('.pill .icon')).width), field: document.querySelector('.composer-input').getBoundingClientRect().width, row: document.querySelector('.composer-row').getBoundingClientRect().width }))()`);
if (wide.labels !== "|||" || wide.names !== "1 running|6 need you|2 to check|2 routines" || wide.field < wide.row / 4 || wide.gap < 8 || wide.icon > 16) await fail(`the wide pills read ${JSON.stringify(wide)}`);
await shot("pills-wide", ".composer-send");
await resize([390, 844, 2]);
await openReservoir();
// ^ The manager's reply that asked for both approvals holds one tag each, no Approve or Deny: the
// tag opens Needs you at that approval's row, the one place to approve (docs/web-client-tasks.md "Project screen").
const tagsOf = () => evaluate("[...document.querySelectorAll('.bubble[data-side=in] .bubble-need')].map((node) => node.textContent.trim())");
const inBubble = await tagsOf();
if (inBubble.join("|") !== "Needs you: Approve land #3|Needs you: Approve discard #4" || await count(".bubble .bubble-approval, .bubble .need-action")) await fail(`the reply bubble's approvals are ${JSON.stringify(inBubble)}`);
await evaluate("document.querySelector('.bubble .bubble-need').scrollIntoView({ block: 'center' })");
await shot("reply-approvals");
await closeReservoir();
// ^ The mark lasts 2 s, so an observer records it; the row's place is read once the list has settled.
await evaluate(`window.marked = new Set(); new MutationObserver((list) => { for (const change of list) if (change.target.hasAttribute('data-found') && change.target.dataset.key) window.marked.add(change.target.dataset.key); })
  .observe(document.body, { subtree: true, attributes: true, attributeFilter: ['data-found'] })`);
for (const [at, label] of [[1, "Approve discard #4"], [0, "Approve land #3"]]) {
  await evaluate(`document.querySelectorAll('.bubble .bubble-need')[${at}].click()`);
  await until(`!!document.querySelector('.reservoir-list[aria-label="Needs you"]') && ${rowOf(label)} && window.marked.has(${rowOf(label)}.dataset.key)`, `the tag of ${label} did not open its row marked in Needs you`);
  await until(`(() => { const box = document.querySelector('.reservoir-scroll').getBoundingClientRect(), at = ${rowOf(label)}.getBoundingClientRect(); return at.top >= box.top - 1 && at.bottom <= box.bottom + 1; })()`, `the row of ${label} is not in view in Needs you`);
  await shot(`reply-approval-row-${at}`, ".composer-send");
  await closeReservoir();
}
await openReservoir();
await evaluate("(() => { const log = document.querySelector('.log'); log.scrollTop = log.scrollHeight; })()");
await until(atEnd, "the chat did not go back to its end", 100);
// ^ A task row starts with its chip (number and state dot), never cut (docs/web-client-tasks.md "Project screen").
const chipFirstOk = async (where) => {
  const chips = await evaluate(`[...document.querySelectorAll('.reservoir-list .need')].filter((row) => row.querySelector('.task-chip')).map((row) => { const chip = row.querySelector('.row-link').firstElementChild;
    return chip.classList.contains('task-chip') && /^#\\d+/.test(chip.textContent) && chip.scrollWidth <= chip.clientWidth; })`);
  if (!chips.length || chips.includes(false)) await fail(`${where}: a row does not start with its task chip: ${JSON.stringify(chips)}`);
};
await chipFirstOk("Needs you");
// ^ The state the dot shows is in the row's words for a screen reader.
const chipName = await evaluate(`${rowOf("#1")}.querySelector('.task-chip').textContent`);
if (!chipName.endsWith("#1, needs you")) await fail(`a row's words are ${chipName}`);
await openReservoir("running");
await chipFirstOk("Running");
if (!(await evaluate(`${rowOf("#6")}.querySelector('.mark[data-running]')`))) await fail("the running task's dot does not pulse");
const runningName = await evaluate(`${rowOf("#6")}.querySelector('.task-chip').textContent`);
if (!runningName.endsWith("#6, working")) await fail(`a working row's words are ${runningName}`);
// ^ The agent in use now is on the row, from the overview's `agent` field (docs/web-client-tasks.md "Agent").
const agentField = await evaluate(`fetch('api/tasks/${project}').then((r) => r.json()).then((view) => view.tasks.find((task) => task.id === 6).agent)`);
if (agentField?.adapter !== "claude" || !agentField.model || agentField.fallback !== false) await fail(`#6's agent field is ${JSON.stringify(agentField)}`);
const runningContext = await evaluate(`${rowOf("#6")}.querySelector('.row-context').textContent`);
if (!agentField.effort || !runningContext.endsWith(`${agentField.model} ${agentField.effort}`)) await fail(`a working row's context reads ${runningContext}`);
if (await count(".reservoir .facts-line")) await fail("the reservoir still shows the deploy facts");
await shot("reservoir-open", ".composer-send");
await openReservoir();
await resize([390, 420, 2]);
await shot("reservoir-short", ".composer-send");
await resize([320, 568, 2]);
await shot("reservoir-narrow", ".composer-send");
// ^ A short window: the list scrolls inside itself and the composer stays on screen.
for (const [width, height, scale] of [[390, 560, 2], [1280, 520, 1]]) {
  await resize([width, height, scale]);
  await openReservoir();
  const scrolled = await evaluate(`(async () => { const list = document.querySelector('.reservoir-scroll');
    list.scrollTop = list.scrollHeight; await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    return { scrolls: list.scrollHeight > list.clientHeight + 1, moved: list.scrollTop > 0, composer: document.querySelector('.composer').getBoundingClientRect().bottom <= innerHeight + 0.5 }; })()`);
  if (!scrolled.scrolls || !scrolled.moved || !scrolled.composer) await fail(`the list does not scroll inside itself at ${width} px: ${JSON.stringify(scrolled)}`);
  await shot(`reservoir-scrolled-${width}`, ".composer-send");
  await evaluate("document.querySelector('.reservoir-scroll').scrollTop = 0");
}
// ^ The open list ends at its last row: between that row and the composer is only the list's own
// bottom padding, never an empty block.
const noEmptyBlock = async (where) => {
  const seen = await evaluate(`(() => {
    const reservoir = document.querySelector('.reservoir'), last = [...reservoir.querySelectorAll('.need')].at(-1);
    let end = last.getBoundingClientRect().bottom;
    for (let node = last.parentElement; node !== reservoir; node = node.parentElement) if (/auto|scroll/.test(getComputedStyle(node).overflowY)) end = Math.min(end, node.getBoundingClientRect().bottom);
    return { gap: document.querySelector('.composer').getBoundingClientRect().top - end, pad: parseFloat(getComputedStyle(document.querySelector('.reservoir-body')).paddingBottom), last: last.textContent.trim().replace(/\\s+/g, ' ').slice(0, 30) };
  })()`);
  if (seen.gap > seen.pad + 1.5) await fail(`${where}: ${Math.round(seen.gap)} px of empty space under the list's last row (${JSON.stringify(seen)}), only ${seen.pad} px is padding`);
};
// ^ With many running tasks and a To check count, no word of the open list is cut: a row is cut
// only by the list that scrolls it, which sits whole inside the reservoir, its last row whole at its end.
const beforeRunners = await inStore(`return JSON.parse(JSON.stringify(state.tasks[${project}]))`);
await inStore(`const ov = state.tasks[${project}], base = ov.tasks.find((task) => task.stage === 'working');
  update({ tasks: { ...state.tasks, [${project}]: { ...ov, needs: [], to_check: 16, check_items: Array.from({ length: 16 }, (_, n) => ({ id: 'c' + (90 + n), task: 7, check: 'check ' + n + ' with words long enough to be cut at the end of its line', pass_if: 'it passes' })), tasks: [base, { ...base, id: 91, title: 'a second running task' }] } } })`);
await resize([390, 844, 2]);
await openReservoir("running");
await until("document.querySelectorAll('.reservoir-list[aria-label=Running] .need').length === 2", "the Running list does not show two running tasks");
await noEmptyBlock("two running tasks at 390x844");
await shot("reservoir-two-running", ".composer-send");
await openReservoir("check");
await noEmptyBlock("To check at 390x844");
await inStore(`update({ tasks: { ...state.tasks, [${project}]: ${JSON.stringify(beforeRunners)} } })`);
await inStore(`const ov = state.tasks[${project}], base = ov.tasks.find((task) => task.stage === 'working');
  update({ tasks: { ...state.tasks, [${project}]: { ...ov, tasks: [...ov.tasks, ...[1, 2, 3, 4, 5, 6].map((n) => ({ ...base, id: 90 + n, title: 'runner ' + n + ' with a title long enough to be cut at the end' }))] } } })`);
for (const [width, height, scale] of [[390, 844, 2], [390, 560, 2], [900, 700, 1]]) {
  await resize([width, height, scale]);
  await openReservoir("running");
  await until("document.querySelectorAll('.reservoir-list[aria-label=Running] .need').length === 7", `the Running list does not show seven running tasks at ${width} px`);
  const cut = await evaluate(`(() => {
    const box = (node) => node.getBoundingClientRect(), scroller = (node) => /auto|scroll/.test(getComputedStyle(node).overflowY);
    const reservoir = document.querySelector('.reservoir'), found = [];
    for (const word of reservoir.querySelectorAll('.row-label, .row-context, .task-chip')) {
      const name = word.textContent.trim().slice(0, 24);
      if (word.scrollHeight > word.clientHeight + 0.5) found.push(\`\${name}: taller than its box\`);
      let shown = box(word);
      for (let node = word.parentElement; node !== reservoir.parentElement; node = node.parentElement) {
        if (getComputedStyle(node).overflowY === 'visible') continue;
        const clip = box(node);
        if (scroller(node)) { shown = clip; continue; }
        if (shown.top < clip.top - 0.5 || shown.bottom > clip.bottom + 0.5) found.push(\`\${name}: cut by .\${node.classList[0]}\`);
      }
    }
    const list = reservoir.querySelector('.reservoir-scroll');
    list.scrollTop = list.scrollHeight;
    const last = [...list.querySelectorAll('.need')].at(-1), end = box(list);
    // ^ scrollTop and scrollHeight are whole pixels, so one pixel of rounding is allowed.
    if (last && box(last).bottom > end.bottom + 1) found.push(\`its last row ends \${Math.round(box(last).bottom - end.bottom)} px under its end\`);
    list.scrollTop = 0;
    const rect = (selector) => { const node = document.querySelector(selector), area = box(node); return [area.top, area.bottom, node.scrollHeight, node.clientHeight].map(Math.round); };
    return { found, at: Object.fromEntries(['.reservoir', '.reservoir-reveal', '.reservoir-body', '.reservoir-scroll', '.composer'].map((selector) => [selector, rect(selector)])) };
  })()`);
  if (cut.found.length) await fail(`the open list cuts words at ${width}x${height}: ${JSON.stringify(cut)}`);
  await noEmptyBlock(`seven running tasks at ${width}x${height}`);
  await shot(`reservoir-many-running-${width}x${height}`, ".composer-send");
}
await inStore(`update({ tasks: { ...state.tasks, [${project}]: ${JSON.stringify(beforeRunners)} } })`);
// ^ The reply menu is fixed to the viewport: it opens at the press point, kept on screen. An
// ancestor with layout containment (container-type: size on the chat) would make the chat its
// containing block and move it by the chat's offset.
for (const [width, height, scale] of [[390, 844, 2], [1280, 800, 1]]) {
  await resize([width, height, scale]);
  await until("!!document.querySelector('.log > [data-quote]')", `no message to press at ${width} px`);
  const at = { x: Math.round(width / 3), y: Math.round(height / 3) };
  await evaluate(`(document.querySelector('.log > [data-quote] .bubble-body p') ?? document.querySelector('.log > [data-quote]'))
    .dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: ${at.x}, clientY: ${at.y}, button: 2 }))`);
  await until("!!document.querySelector('.reply-menu')", `a press on a manager message opened no reply menu at ${width}x${height}`);
  await pause(100);
  const placed = await evaluate(`(() => { const box = document.querySelector('.reply-menu').getBoundingClientRect(); return { left: box.left, top: box.top, width: box.width, height: box.height }; })()`);
  const want = { left: Math.min(at.x, width - placed.width), top: Math.min(at.y, height - placed.height) };
  if (Math.abs(placed.left - want.left) > 1 || Math.abs(placed.top - want.top) > 1) await fail(`the reply menu is at ${Math.round(placed.left)},${Math.round(placed.top)} not ${want.left},${want.top} at ${width}x${height}`);
  await shot(`reply-menu-manager-${width}`);
  await evaluate("document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))");
  await until("!document.querySelector('.reply-menu')", `Escape did not close the reply menu at ${width} px`);
}
// ^ 1280 px is the wide layout: its page tree is another, so the list comes back closed.
await resize([390, 844, 2]);
await openReservoir();
// Answer puts the task's mention into the composer, the owner tells the manager, and the manager
// relays with `wsx task add` (docs/web-client-tasks.md "Project screen"). A draft stays, a second
// press adds nothing, and #12 is not #1.
const answerKey = await evaluate(`${rowOf("#1")}.dataset.key`);
const pressAnswer = () => evaluate(`${rowOf("#1")}.querySelector('.need-action').click()`);
await typeInto(".composer-field", "about #12 first");
await pressAnswer();
await composerHas("about #12 first #1 ", "Answer did not add #1 after the draft");
await pressAnswer();
await composerHas("about #12 first #1 ", "a second Answer changed the draft");
if (!(await evaluate("Object.values(JSON.parse(localStorage.getItem('wsx-manager-drafts'))).includes('about #12 first #1 ')"))) await fail("the mention is not in the kept draft");
await entered();
await shot("answer-mention", ".composer-send");
// ^ Wide: the composer is another element, with the same draft.
await resize([1280, 800, 1]);
await openReservoir();
await pressAnswer();
await composerHas("about #12 first #1 ", "Answer at 1280 px changed the draft");
await entered();
await shot("answer-mention", ".composer-send");
await resize([390, 844, 2]);
await openReservoir();
await typeInto(".composer-field", "#1 at the start, then words");
await pressAnswer();
await until("document.querySelector('.composer-field').selectionStart === 3 && document.activeElement === document.querySelector('.composer-field')", "Answer did not put the caret after the mention already there");
if (await evaluate("document.querySelector('.composer-field').value") !== "#1 at the start, then words") await fail("Answer changed a draft that names the task");
await typeInto(".composer-field", "");
await pressAnswer();
await composerHas("#1 ", "Answer did not put #1 into the empty composer");
// Clear in place, one row after another: the manager's add resumes #1; Approve land #3 lands
// it; #1, ready, lands. The chat stays at its end, and the settled items become muted lines
// that join when close together.
await typeInto(".composer-field", `#1 go on\nRUN wsx task add 1 "WORK answered.txt" -p ui --json`);
await click(".composer-send");
await until(`!document.querySelector('.reservoir .need[data-key=${JSON.stringify(answerKey)}]')`, "the answer did not clear the row of #1", 300);
// ^ The chat says the answer as a status line.
await until(`${statusLines}.some((node) => /#1 got an answer/.test(node.textContent))`, "the answer in the reservoir left no status line", 60);
// ^ A line keeps its element while newer lines arrive: a row made again would play its entry
// again and take the scroll anchor with it. A message after the line ends its run, so no fold
// takes it in.
await typeInto(".composer-field", "STATUS after the answer");
await click(".composer-send");
const answeredOpen = "[...document.querySelectorAll('.log .bubble[data-side=in]')].at(-1)?.textContent.includes('open:') && !document.querySelector('.log .typing')";
await until(answeredOpen, "no reply after the answer", 300);
// ^ A reply that ends with a question carries "Asks you" in the accent, and the overview's
// `asks` lists it, until an owner message answers it (docs/web-client-tasks.md "Project screen").
const asking = "[...document.querySelectorAll('.log .bubble[data-side=in]')].find((node) => node.textContent.includes('Should I land it now?'))";
const askTags = "[...document.querySelectorAll('.log .bubble[data-asks]')]";
const overviewAsks = () => evaluate(`fetch('api/tasks/${project}').then((r) => r.json()).then((view) => view.asks)`);
await typeInto(".composer-field", "ASK");
await click(".composer-send");
await until(`!!${asking} && !document.querySelector('.log .typing')`, "no reply to the question", 300);
const askBubble = await evaluate(`({ tags: ${askTags}.length, own: ${asking}.dataset.asks, label: ${asking}.getAttribute('aria-label'), icon: !!${asking}.querySelector('.icon'),
  border: getComputedStyle(${asking}.querySelector('.bubble-box')).borderTopColor, accent: getComputedStyle(document.documentElement).getPropertyValue('--color-accent').trim(),
  normal: getComputedStyle(document.querySelector('.log .bubble[data-side=in]:not([data-asks]) .bubble-box')).borderTopColor })`);
const accentColor = await evaluate("(() => { const s = document.createElement('i'); s.style.color = 'var(--color-accent)'; document.body.append(s); const c = getComputedStyle(s).color; s.remove(); return c; })()");
if (askBubble.tags !== 1 || askBubble.own !== "" || !/Asks you/.test(askBubble.label) || askBubble.icon || askBubble.border !== accentColor || askBubble.normal === accentColor) await fail(`a reply that asks reads ${JSON.stringify(askBubble)}`);
if ((await overviewAsks())?.length !== 1) await fail("the overview does not say the manager asks");
const askRow = async () => (await chatRows()).findLast((row) => row.text.includes("Should I land it now?"));
{
  const row = await askRow();
  if (row?.kind !== "message" || row.author !== "manager" || row.asks !== true || row.post) await fail(`the asking reply's row is ${JSON.stringify(row)}`);
}
await entered();
await shot("asks-you", ".composer-send");
// ^ A later report does not drop the mark: plans' manager asks ui's, whose answer is a reply that
// is not quiet and asks nothing. The chat keeps its tag and the Home row, from the real overview,
// shows the same on that row only (docs/web-client-tasks.md "Home").
await evaluate(`fetch('api/tasks/${plansArg}/say', { method: 'POST', headers: { 'content-type': 'application/json' }, body: ${JSON.stringify(JSON.stringify({ text: 'RUN wsx task say -p ui --from-chat "How is ui now?" --json' }))} })`);
await until(`(() => { const bubbles = [...document.querySelectorAll('.log .bubble[data-side=in]')], heard = bubbles.findIndex((node) => node.textContent.includes('How is ui now?'));
  return heard >= 0 && heard < bubbles.length - 1 && !document.querySelector('.log .typing'); })()`, "no report after the question", 300);
if ((await evaluate(`${askTags}.length`)) !== 1) await fail("a later report dropped the chat's Asks you");
if ((await overviewAsks())?.length !== 1) await fail("a later report dropped the overview's asks");
const askingChat = await evaluate("location.hash");
const homeAskRows = "[...document.querySelectorAll('.home-rows > .row:not(.row-chat)')].filter((row) => row.querySelector('.row-label[data-tone=accent]') && /Asks you/.test(row.querySelector('.row-label').textContent) && !row.querySelector('.row-asks, .row-label .icon')).map((row) => row.dataset.key).sort().join()";
// ^ plans may still hold an open question of its log (test-web-client.py): the rows that ask are
// the projects whose overview lists one, ui among them.
const askingNow = async () => (await evaluate(`Promise.all([${project}, ${plansArg}].map((id) => fetch('api/tasks/' + id).then((r) => r.json()).then((view) => view.asks.length ? String(id) : '')))`)).filter(Boolean).sort().join();
const askingKeys = await askingNow();
if (!askingKeys.split(",").includes(String(project))) await fail(`the asking project is not in the overviews' asks: ${askingKeys}`);
await go("#/");
await until(`${homeAskRows} === ${JSON.stringify(askingKeys)}`, "Asks you is not on the rows of the projects whose manager asks", 100);
await entered();
await shot("home-asks");
await go(askingChat);
await until(`!!${asking}`, "the chat did not come back after Home", 100);
await typeInto(".composer-field", "STATUS after the question");
await click(".composer-send");
await until(`${askTags}.length === 0`, "the owner's message did not clear Asks you", 30);
await until(answeredOpen, async () => `no reply after the question: ${await evaluate("JSON.stringify({ typing: !!document.querySelector('.log .typing'), last: [...document.querySelectorAll('.log .bubble')].slice(-4).map((node) => node.dataset.side + ': ' + node.textContent.slice(0, 80)) })")}`, 900);
// ^ The manager answers after the events of the steps before; the label says what the page held when it did not.
if ((await overviewAsks())?.length !== 0) await fail("the overview still says the manager asks");
if ((await askRow())?.asks) await fail("the asking reply's row still asks after the owner wrote");
await entered();
await shot("asks-you-answered", ".composer-send");
await go("#/");
const askingLeft = await askingNow();
// ^ ui's question is answered; plans' older questions (test-web-client.py) are not, so only plans may still ask.
if (askingLeft.split(",").includes(String(project))) await fail(`ui still asks after the owner wrote: ${askingLeft}`);
await until(`!!document.querySelector('.home-rows > .row:not(.row-chat)') && ${homeAskRows} === ${JSON.stringify(askingLeft)}`, "Asks you stayed on the Home row after the owner wrote", 100);
await go(askingChat);
await until(`!!${asking}`, "the chat did not come back after Home", 100);
await openReservoir();
await evaluate(`window.answerLine = ${statusLines}.find((node) => /#1 got an answer/.test(node.textContent))`);
let moved = await clearRow("Approve land #3", "Approve");
if (await evaluate("[...document.querySelectorAll('.notice')].some((node) => /Approved land #3/.test(node.textContent))")) await fail("a notice repeats the status line of Approve");
await until(`!${rowOf("Approve land #3")} && !${rowOf("#3")}`, "the reservoir kept the approved landing", 300);
// ^ The approval and the landing are muted lines.
await until(`${statusLines}.some((node) => /Approved land #3/.test(node.textContent))`, "the approval left no muted line", 300);
await until(statusSays(3, "done"), "the landing of #3 left no muted line", 300);
// ^ The rows carry the short form the host wrote with each event, and its task: the page reads no event text.
{
  const lines = (await chatRows()).filter((row) => row.kind === "line");
  const landed = lines.findLast((row) => row.task === 3);
  const approved = lines.findLast((row) => row.text === "Approved land #3");
  if (landed?.text !== "#3 done" || !approved || approved.task != null) await fail(`the event rows of #3 are ${JSON.stringify({ landed, approved })}`);
}
await until(`(${rowOf("#1")}?.querySelector('.need-action')?.title ?? '') === 'Land'`, "#1 is not ready to land after its answer", 300);
moved = await clearRow("#1", "Land");
// ^ A bubble that arrives with the landing sticks the chat to its end one frame later: on a
// loaded machine that frame comes late, so the check waits for it. A chat the action moved
// stays moved and shows the jump button.
await until(`(${atEnd}) && !document.querySelector('.jump')`, "acting in the reservoir moved the chat", 30);
if (await count(".need-card, .tray")) await fail("an action brought back a card or the tray");
if (!(await evaluate("window.answerLine.isConnected"))) await fail("a status line was made again when newer resolutions arrived");
await shot("reservoir-acted", ".composer-send");
await closeReservoir();

// --- The ⋯ menu: Sessions, Usage, Media and Files (the site reads files); Overview is a header button, Routines a pill (Chats are on Home, checked below); the worktree list hides task sessions.
await click(".more-button");
if ((await text(".more-menu a")).join() !== "Sessions,Usage,Media,Files") await fail(`the ⋯ menu offers ${await text(".more-menu a")}`);
await shot("more-menu", ".composer-send");
await evaluate("[...document.querySelectorAll('.more-menu a')].find((node) => node.textContent === 'Sessions').click()");
await until(`location.hash === '#/p/${project}/sessions' && !!document.querySelector('.worktree')`, "Sessions did not open the worktree list");
const worktreeSessions = await text(".worktree .row-session-name");
if (worktreeSessions.some((name) => /^task-/.test(name))) await fail(`the worktree list shows task sessions: ${worktreeSessions}`);
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until(`location.hash === ${JSON.stringify(chat)} && !!document.querySelector('.pill')`, "Back did not return to the chat");

// --- The project overview: markdown and a mermaid graph in the sheet, with its last change.
await click('.manager a.icon-button[href$="/page"]');
await until(`location.hash === '#/p/${project}/page' && !!document.querySelector('dialog.task-sheet[open] .project-overview h1')`, "Overview did not open the project overview");
await until("!!document.querySelector('.project-overview .diagram img')", "the page's mermaid graph did not draw", 30);
// ^ The button, the sheet's aria-label and its top-bar label all say Overview, and a file under the cap shows no note.
const overviewLabels = await evaluate("({ sheet: document.querySelector('dialog.task-sheet[open]').getAttribute('aria-label'), bar: document.querySelector('dialog.task-sheet[open] .bar-label').textContent, note: !!document.querySelector('.project-overview .over-limit') })");
if (overviewLabels.sheet !== "Overview" || overviewLabels.bar !== "Overview" || overviewLabels.note) await fail(`the overview's labels read ${JSON.stringify(overviewLabels)}`);
if (!/^Updated /.test(await sheetState())) await fail(`the page's top row reads ${await sheetState()}`);
const pageHeads = await text(".project-overview h2");
if (pageHeads.join() !== "Goal,Tasks,Decisions,Open questions") await fail(`the page's sections are ${pageHeads}`);
// ^ The deploy and the main checkout moved here from the reservoir, above the markdown. They
// come from a separate brief request, which can answer after the page has drawn and again as
// tasks change: wait for the words, not only the line.
const factsRead = (facts) => /^Deploy: [0-9a-f]{8}, (up to date with main|\d+ commits? behind main)\./.test(facts) && /Main checkout on main, /.test(facts);
let facts = "";
for (let attempt = 0; attempt < 300 && !factsRead(facts); attempt++) {
  if (attempt) await pause(100);
  facts = (await text(".project-overview .facts-line"))[0] ?? "";
}
if (!factsRead(facts)) await fail(`the page's facts read ${facts}`);
await shot("page");
await closeSheet("the page", chat);

// --- Usage: the ⋯ menu's sheet has the project's spend of one choice of filters, from
// `api/tasks/<project>/usage` (docs/web-client-tasks.md "Usage").
await click(".more-button");
await evaluate("[...document.querySelectorAll('.more-menu a')].find((node) => node.textContent === 'Usage').click()");
await until(`location.hash === '#/p/${project}/usage' && !!document.querySelector('dialog.task-sheet[open] .usage-chart')`, "Usage did not open the usage sheet");
// ^ One row per day, a stacked bar on one scale, a legend, and a tap shows the exact amounts.
// ^ One chart at a time, so each day shows once: Cost first, the Tokens tab switches it.
const usageTabs = await evaluate(`[...document.querySelectorAll('.usage-view [role=tab]')].map((tab) => tab.textContent + ':' + tab.getAttribute('aria-selected'))`);
if (usageTabs.join() !== "Cost:true,Tokens:false" || await evaluate(`document.querySelectorAll('.usage-chart').length`) !== 1) await fail(`the usage tabs are ${usageTabs}`);
// ^ The Cost tab holds the chart and nothing else: no totals table, no limit strip (a project's sheet has none).
if (await evaluate(`document.querySelectorAll('.usage table, .usage-limits, [role=tabpanel] [data-chart=tokens]').length`)) await fail("the Cost tab shows more than the cost chart");
const usageDays = await evaluate(`[...document.querySelectorAll('.usage-chart[data-chart=cost] .usage-days > li')].map((row) => ({
  cost: row.querySelector('.usage-cost').textContent,
  parts: [...row.querySelectorAll('.usage-bar i')].map((part) => part.dataset.part),
  width: row.querySelector('.usage-bar').getBoundingClientRect().width,
}))`);
if (usageDays.length !== 7) await fail(`the usage has ${usageDays.length} day rows`);
const usageToday = usageDays[0];
if (!usageToday.parts.includes("manager") || !(usageToday.width > 0) || !/^\$\d+\.\d\d$/.test(usageToday.cost)) await fail(`today's usage row is ${JSON.stringify(usageToday)}`);
if (usageDays.slice(1).some((day) => day.parts.length)) await fail(`a past day has spend: ${JSON.stringify(usageDays)}`);
const usageLegend = await text("[data-chart=cost] .usage-legend .usage-name");
const usageWeek = await text("[data-chart=cost] .usage-total .usage-cost");
const usageAmounts = await text("[data-chart=cost] .usage-legend .usage-amount");
if (!/^\$\d+\.\d\d$/.test(usageWeek[0]) || usageAmounts.length !== 7 || !usageAmounts.every((cell) => /^\$\d+\.\d\d \d+%$/.test(cell))) await fail(`the usage total is ${usageWeek}, the legend ${usageAmounts}`);
if (usageLegend.join() !== "Build,Plan,Context,Review,Memo,Other work,Manager") await fail(`the usage legend is ${usageLegend}`);
await evaluate(`document.querySelector('[data-chart=cost] .usage-days > li:first-child .usage-day').click()`);
await until(`document.querySelectorAll('[data-chart=cost] .usage-days > li:first-child .usage-parts dd').length === ${usageToday.parts.length}`, "a tap on a day shows its amounts");
// ^ The token chart: the four kinds in the legend, today's bar split into the kinds it spent
// (the fake manager's turns have input, cache read and output), and the cache hit rate per row.
await evaluate("[...document.querySelectorAll('.usage-view [role=tab]')].find((tab) => tab.textContent === 'Tokens').click()");
await until(`!!document.querySelector('[data-chart=tokens]') && document.querySelectorAll('.usage-chart').length === 1`, "the Tokens tab did not switch the chart");
// ^ The Tokens tab adds the totals table below its chart, one column, the filters' slice.
const usageRows = await text(".usage tbody th");
if (usageRows.join() !== "Cost,Input,Cache read,Cache write,Output,Turns") await fail(`the usage rows are ${usageRows}`);
// ^ One column, the filters' slice: 7 days of workers and manager on every agent at first.
const USAGE_TOTALS = `({ head: document.querySelector('.usage thead th')?.textContent,
  cost: document.querySelector('.usage tbody tr:first-child td')?.textContent, cells: document.querySelectorAll('.usage tbody tr:first-child td').length,
  turns: Number(document.querySelector('.usage tbody tr:last-child td')?.textContent), days: document.querySelectorAll('.usage-days > li').length,
  periods: [...document.querySelectorAll('select[name=period] option')].map((option) => option.value),
  agent: document.querySelector('select[name=agent]')?.value })`;
const usageTotals = () => evaluate(USAGE_TOTALS);
const usageAll = await usageTotals();
// ^ The fake worker's results cost nothing; each manager turn costs 0.001 USD.
if (usageAll.head !== "7 days" || usageAll.cells !== 1 || !/^\$\d+\.\d\d$/.test(usageAll.cost) || usageAll.cost === "$0.00" || !(usageAll.turns > 1)) {
  await fail(`the usage totals are ${JSON.stringify(usageAll)}`);
}
const tokenLegend = await text("[data-chart=tokens] .usage-legend .usage-name");
if (tokenLegend.join() !== "Input,Cache read,Cache write,Output") await fail(`the token legend is ${tokenLegend}`);
const tokenAmounts = await text("[data-chart=tokens] .usage-legend .usage-amount");
if (tokenAmounts.length !== 4 || !tokenAmounts.every((cell) => /^\d+(\.\d)?[kM]? \d+%$/.test(cell))) await fail(`the token legend amounts are ${tokenAmounts}`);
const tokenDays = await evaluate(`[...document.querySelectorAll('[data-chart=tokens] .usage-days > li')].map((row) => ({
  total: row.querySelector('.usage-cost').textContent,
  rate: row.querySelector('.usage-rate').textContent,
  kinds: [...row.querySelectorAll('.usage-bar i')].map((kind) => kind.dataset.part),
  narrow: [...row.querySelectorAll('.usage-bar i')].some((kind) => kind.getBoundingClientRect().width < 3),
}))`);
if (tokenDays.length !== 7 || !tokenDays[0].kinds.includes("input") || !tokenDays[0].kinds.includes("cache_read") || !tokenDays[0].kinds.includes("output")
  || tokenDays[0].narrow || !/^\d+%$/.test(tokenDays[0].rate) || !/^\d+(\.\d)?[kM]?$/.test(tokenDays[0].total)) await fail(`today's token row is ${JSON.stringify(tokenDays[0])}`);
if (tokenDays.slice(1).some((day) => day.kinds.length || day.rate !== "-")) await fail(`a past day has tokens: ${JSON.stringify(tokenDays)}`);
const tokenWeek = await text("[data-chart=tokens] .usage-total .usage-rate");
if (!/^\d+%$/.test(tokenWeek[0])) await fail(`the 7-day hit rate is ${tokenWeek}`);
await evaluate(`document.querySelector('[data-chart=tokens] .usage-days > li:first-child .usage-day').click()`);
await until(`document.querySelectorAll('[data-chart=tokens] .usage-days > li:first-child .usage-parts dd').length === ${tokenDays[0].kinds.length}`, "a tap on a token day shows its amounts");
// ^ One set of filters drives the chart and the table: each choice reads again and both follow.
const choose = async (name, value) => {
  const before = JSON.stringify(await usageTotals());
  await evaluate(`(() => { const select = document.querySelector('select[name=${name}]'); select.value = ${JSON.stringify(value)};
    select.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  // ^ The filters are busy until the read of the new choice came.
  await until(`!document.querySelector('.usage-filters[aria-busy=true]') && JSON.stringify(${USAGE_TOTALS}) !== ${JSON.stringify(before)}`, `the usage did not follow ${name} ${value}`);
  return usageTotals();
};
const usageOnlyToday = await choose("period", "today");
if (usageOnlyToday.head !== "Today" || usageOnlyToday.days !== 1 || usageOnlyToday.turns !== usageAll.turns) await fail(`Today's usage is ${JSON.stringify(usageOnlyToday)} after ${JSON.stringify(usageAll)}`);
await choose("period", "week");
// ^ A slice with no spend says so, with its filters kept (the fake worker's results cost nothing).
await choose("who", "workers");
await click("#usage-tab-cost");
await until("!document.querySelector('.usage-chart') && (document.querySelector('#usage-panel .empty')?.textContent ?? '') === 'No spend' && !!document.querySelector('select[name=who]')", "a Cost slice with no spend does not say so");
await click("#usage-tab-tokens");
await until("!!document.querySelector('[data-chart=tokens]')", "the Tokens tab did not come back");
await choose("who", "");
const usageManager = await choose("who", "manager");
if (!(usageManager.turns > 0) || !(usageManager.turns < usageAll.turns)) await fail(`the manager's usage is ${JSON.stringify(usageManager)} of ${JSON.stringify(usageAll)}`);
await choose("who", "");
const agents = await evaluate(`[...document.querySelectorAll('select[name=agent] option')].map((option) => option.value)`);
if (!agents.includes("claude") || !agents.includes("claude/fake-manager")) await fail(`the agent choices are ${agents}`);
const usageAgent = await choose("agent", "claude/fake-manager");
if (!(usageAgent.turns > 0) || usageAgent.turns > usageManager.turns || usageAgent.cost === "$0.00") await fail(`the manager model's usage is ${JSON.stringify(usageAgent)}, the manager's ${JSON.stringify(usageManager)}`);
// ^ The limit window: fake Claude's 5-hour window starts at its newest turn, so it holds fewer
// turns than the 7 days; picked with every agent it counts Claude's.
await choose("agent", "");
if (!usageAll.periods.includes("five_hour") || !usageAll.periods.includes("seven_day")) await fail(`the periods are ${usageAll.periods}`);
const usageWindow = await choose("period", "five_hour");
if (usageWindow.head !== "5-hour window" || usageWindow.agent !== "claude" || !(usageWindow.turns < usageAll.turns) || usageWindow.days !== 1) await fail(`the 5-hour window's usage is ${JSON.stringify(usageWindow)}`);
await shot("usage");
const usageSize = size;
await resize([1280, 800, 1]);
await shot("usage");
await resize(usageSize);
await closeSheet("the usage sheet", chat);

// --- Routines: the fourth pill and its list over the composer (docs/web-client-tasks.md "Routines"). The fixture has
// `nightly` (on, succeeded, printed nothing, so no message), `weekly` (off, found something: it exits 1, a findings code,
// and its end is a message in the chat) and `idle` (on, never run). The pill counts the enabled ones, tints for a failed
// (danger) or found-something (attention) last run, and its rows have the shared list row with Turn on or off and Run now.
{
  const served = (id) => evaluate(`fetch('api/tasks/${id}/routines').then(async (r) => ({ status: r.status, body: await r.json().catch(() => null) }))`);
  const post = (path) => evaluate(`fetch(${JSON.stringify(path)}, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' }).then((r) => r.status)`);
  const unknown = await served(999999);
  if (unknown.status !== 404) await fail(`the routines of a project outside the grant answered ${unknown.status}`);
  const acted = await post("api/tasks/999999/routines/nightly/run");
  if (acted !== 404) await fail(`an action on the routines of a project outside the grant answered ${acted}`);
  const refused = await post(`api/tasks/${project}/routines/nightly/explode`);
  if (refused !== 400) await fail(`an unknown routine action answered ${refused}`);
  // ^ The end of weekly's run comes as an events message once the manager has flushed it: the row has its id.
  let answer;
  for (let attempt = 0; attempt < 600; attempt++) {
    answer = await served(project);
    if (answer.body?.routines?.find((row) => row.name === "weekly")?.last?.message != null) break;
    await pause(100);
  }
  const byName = Object.fromEntries((answer.body?.routines ?? []).map((row) => [row.name, row]));
  if (answer.status !== 200 || !byName.nightly || !byName.weekly || !byName.idle || byName.nightly.enabled !== true || byName.weekly.enabled !== false
    || byName.nightly.trigger !== "0 6 * * *" || byName.weekly.trigger !== "0 6 * * 1" || byName.weekly.last?.exit !== 1
    || byName.nightly.post !== true || byName.weekly.post !== false
    || byName.weekly.last?.status !== "findings" || byName.weekly.findings_exit?.join() !== "1" || byName.nightly.findings_exit
    || byName.nightly.last?.status !== "succeeded" || !(byName.nightly.last.finished_unix_ms > 0) || !(byName.nightly.next_run_unix_ms > Date.now())
    || byName.nightly.command.join(" ") !== "/bin/sh -c exit 0" || !Number.isInteger(byName.weekly.last.message)
    || byName.nightly.last.message !== null || byName.idle.last !== null) await fail(`the routines route answered ${JSON.stringify(answer)}`);
  const message = byName.weekly.last.message;
  await go(chat);
  await until("!!document.querySelector('.pill[data-kind=routines]')", "the project with routines has no Routines pill", 100);
  const rowsOf = () => evaluate(`[...document.querySelectorAll('.reservoir .need')].map((row) => ({ name: row.querySelector('.row-label').textContent,
    context: [...row.querySelectorAll('.row-context')].map((node) => node.textContent), main: row.querySelector('.row-main').tagName, mark: row.querySelector('.mark')?.dataset.tone ?? null,
    actions: [...row.querySelectorAll('.need-action')].map((node) => node.title), icons: [...row.querySelectorAll('.need-action path')].map((node) => node.getAttribute('d')) }))`);
  const pill = () => evaluate(`(() => { const node = document.querySelector('.pill[data-kind=routines]'); return node && { label: node.getAttribute('aria-label'), count: node.querySelector('.pill-count').textContent.trim(), tone: node.dataset.tone ?? null, icon: !!node.querySelector('svg.icon') }; })()`);
  // ^ The list is read as the answer to a fetch of the page: a run's state is set in the answer (the scheduler has no failed run to stage).
  await evaluate(`(() => { window.realFetch ??= window.fetch; window.tweak = null;
    window.fetch = async (input, init) => { const response = await window.realFetch(input, init);
      if (!window.tweak || (init?.method ?? 'GET') !== 'GET' || !/\\/routines$/.test(String(input?.url ?? input))) return response;
      const body = await response.clone().json(); window.tweak(body); return new Response(JSON.stringify(body), { status: response.status, headers: response.headers }); }; })()`);
  const again = async () => { await go("#/"); await go(chat); await until("!!document.querySelector('.pill[data-kind=routines]')", "the Routines pill did not come back"); };
  try {
    let look = await pill();
    if (look.label !== "2 routines" || look.count !== "2" || look.tone || !look.icon) await fail(`the Routines pill with nightly and idle on is ${JSON.stringify(look)}`);
    // ^ Home shows the same icons, counts and tones beside the project's name (docs/web-client-tasks.md "Home").
    const trayOf = (selector, count) => `[...document.querySelectorAll('${selector}')].map((node) => [node.dataset.kind, node.querySelector('${count}').textContent.trim(), node.getAttribute('aria-label'), node.dataset.tone ?? '', node.querySelector('svg.icon path').getAttribute('d')].join('|')).join()`;
    const pills = await evaluate(trayOf(".pill", ".pill-count"));
    await go("#/");
    const homeTrays = `.home-rows > .row[data-key="${project}"] .tray`;
    await until(`document.querySelectorAll('${homeTrays}').length === 4`, "Home shows no four trays on the project's row", 50);
    const trays = await evaluate(trayOf(homeTrays, ".tray-count"));
    if (trays !== pills || pills.split(",").length !== 4) await fail(`Home's trays are ${trays}, the pills ${pills}`);
    const lines = await evaluate(`(() => { const row = document.querySelector('.home-rows > .row[data-key="${project}"] .row-link'), head = row.querySelector('.row-head'), main = row.querySelector('.row-main');
      return { children: [...main.children].map((node) => node.className), time: !!head.querySelector('.time'), height: Math.round(row.getBoundingClientRect().height) }; })()`);
    if (lines.children.join() !== "row-head,row-context" || !lines.time) await fail(`the project's row is not two lines: ${JSON.stringify(lines)}`);
    await again();
    // The list: the shared row; run now and on/off in the row's actions.
    await openReservoir("routines");
    const rows = await rowsOf();
    const by = Object.fromEntries(rows.map((row) => [row.name, row]));
    if (rows.length !== 3 || by.nightly?.context[0] !== "daily 06:00 · On" || !/^succeeded( now|\b.* ago)$/.test(by.nightly.context[1])
      || by.weekly?.context[0] !== "Mondays 06:00, no chat post · Off" || !/^found something \(exit 1\)( now|.* ago)$/.test(by.weekly.context[1])
      || by.idle?.context.join("|") !== "daily 07:00 · On|never run"
      || by.nightly.main !== "SPAN" || by.idle.main !== "SPAN" || by.weekly.main !== "BUTTON"
      || by.nightly.actions.join() !== "Turn off,Run now" || by.weekly.actions.join() !== "Turn on,Run now" || by.idle.actions.join() !== "Turn off,Run now"
      || by.weekly.mark !== null || by.nightly.mark !== "accent" || by.idle.mark !== "accent") await fail(`the routine rows are ${JSON.stringify(rows)}`);
    // ^ Turn off, Turn on and Run now are three different icons (the enable action is no check mark next to a done status).
    if (new Set([by.nightly.icons[0], by.weekly.icons[0], by.weekly.icons[1]]).size !== 3) await fail(`the routine actions share an icon: ${JSON.stringify([by.nightly.icons, by.weekly.icons])}`);
    for (const size of [[375, 812, 2], [1280, 800, 1]]) {
      await resize(size);
      await openReservoir("routines");
      await shot(`routines-list-${size[0]}`, ".composer-send");
    }
    await resize([390, 844, 2]);
    // A tap on the routine with a message takes the chat to it, marked; the routine without one is not a button.
    await openReservoir("routines");
    const shown = `(() => { const node = document.querySelector('[data-id="${message}"]'), log = document.querySelector('.log').getBoundingClientRect(); if (!node) return null;
      const box = node.getBoundingClientRect(); return { found: node.hasAttribute('data-found'), inside: box.top >= log.top - 1 && box.bottom <= log.bottom + 1, text: node.textContent }; })()`;
    for (const size of [[390, 844, 2], [375, 812, 2], [1280, 800, 1]]) {
      await resize(size);
      await openReservoir("routines");
      await evaluate("document.querySelector('.log').scrollTop = 0");
      await click(`.reservoir .need[data-key="routine-weekly"] .row-open`);
      await until(`${shown}?.found === true`, `a tap on a routine with a message did not take the chat to its message at ${size[0]} px`);
      const there = await evaluate(shown);
      if (!there.inside || !/^routine weekly found something \(exit 1\)/.test(there.text) || !(await count(".reservoir"))) await fail(`the message of the routine's last run is ${JSON.stringify(there)}`);
      await shot(`routines-message-${size[0]}`, ".composer-send");
    }
    await resize([390, 844, 2]);
    // A tap on the routine with no message does nothing.
    await pause(2200);
    await openReservoir("routines");
    await click(`.reservoir .need[data-key="routine-nightly"] .row-main`);
    await pause(300);
    if (await count("[data-found]")) await fail("a tap on a routine with no message marked something");
    await shot("routines-no-message", ".composer-send");

    // Turn on, then off: weekly (found something) joins the pill, tinted attention, and the answer is the new list.
    const action = (name, title) => `.reservoir .need[data-key="routine-${name}"] .need-action[title="${title}"]`;
    await click(action("weekly", "Turn on"));
    await until("document.querySelector('.pill[data-kind=routines]')?.getAttribute('aria-label') === '3 routines, 1 found something' && document.querySelector('.pill[data-kind=routines]').dataset.tone === 'attention'", "Turn on did not count weekly in the pill with the attention tone");
    if ((await rowsOf()).find((row) => row.name === "weekly").actions.join() !== "Turn off,Run now") await fail("the row of a routine turned on still offers Turn on");
    await shot("routines-found-pill", ".composer-send");
    await click(action("weekly", "Turn off"));
    await until("document.querySelector('.pill[data-kind=routines]')?.getAttribute('aria-label') === '2 routines'", "Turn off did not take weekly out of the pill");
    // Run now: nightly runs (it exits 0 and prints nothing); its last run reads "succeeded now".
    await click(action("nightly", "Run now"));
    await until("/^succeeded now$/.test([...document.querySelectorAll('.reservoir .need[data-key=\"routine-nightly\"] .row-context')].at(-1)?.textContent ?? '')", "Run now did not show nightly's new run", 300);
    await closeReservoir();

    // A failed last run tints the pill in the danger color. The pill reads the server's `counts`, so the tweak sets them with the row.
    await evaluate("window.tweak = (body) => { const row = body.routines.find((each) => each.name === 'nightly'); row.last = { ...row.last, status: 'failed', exit: 3 }; body.counts = { ...body.counts, failed: 1 }; }");
    await again();
    // ^ The pill first shows the overview's counts; the list's (the tweaked answer) follows.
    await until("document.querySelector('.pill[data-kind=routines]')?.dataset.tone === 'danger'", "the Routines pill did not take the failed tone", 100);
    look = await pill();
    if (look.label !== "2 routines, 1 failed" || look.tone !== "danger") await fail(`the Routines pill with a failed last run is ${JSON.stringify(look)}`);
    // ^ The tone changes the color through a transition ([data-tone] in app.css) when the list's counts follow the overview's: let it end.
    await until("(() => { const node = document.createElement('i'); node.style.color = 'var(--color-danger)'; document.body.append(node); const danger = getComputedStyle(node).color; node.remove(); return getComputedStyle(document.querySelector('.pill[data-kind=routines]')).color === danger; })()", "the failed Routines pill did not reach the danger color", 100);
    const colors = await evaluate(`(() => { const node = document.createElement('i'); node.style.color = 'var(--color-danger)'; document.body.append(node); const danger = getComputedStyle(node).color; node.remove();
      const pill = document.querySelector('.pill[data-kind=routines]'); return { pill: getComputedStyle(pill).color, edge: getComputedStyle(pill).borderTopColor, danger }; })()`);
    if (colors.pill !== colors.danger || colors.edge !== colors.danger) await fail(`the failed Routines pill is not in the danger color: ${JSON.stringify(colors)}`);
    await shot("routines-failed-pill", ".composer-send");
    await openReservoir("routines");
    const failed = (await rowsOf()).find((row) => row.name === "nightly");
    if (failed.mark !== "danger" || !/^failed \(exit 3\)/.test(failed.context[1])) await fail(`the row of a failed last run is ${JSON.stringify(failed)}`);
    await shot("routines-failed-list", ".composer-send");
    await closeReservoir();
    await evaluate("window.tweak = null");

    // A device that cannot act sees the rows with no action.
    await again();
    const capabilitiesNow = await inStore("return [...state.site.capabilities]");
    await withCapabilities(capabilitiesNow.filter((name) => name !== "task_act"));
    await until("!document.querySelector('.composer-field') && !!document.querySelector('.pill-bar .pill[data-kind=routines]')", "a device without task_act lost the Routines pill");
    await openReservoir("routines");
    if ((await rowsOf()).some((row) => row.actions.length)) await fail(`a device without task_act has routine actions: ${JSON.stringify(await rowsOf())}`);
    await click(".pill[aria-pressed=true]");
    await withCapabilities(capabilitiesNow);
    await until("!!document.querySelector('.composer-row .pill[data-kind=routines]')", "the Routines pill did not go back into the composer row");
    // The phone composer keeps its height with four pills, down to 320 px, and the pills stay on one row between attach and Send.
    for (const width of [375, 320]) {
      await resize([width, 740, 2]);
      await until("!!document.querySelector('.composer-row .pill[data-kind=routines]')", `the pills are missing at ${width} px`);
      const fit = await evaluate(`(() => { const row = document.querySelector('.composer-row').getBoundingClientRect(), send = document.querySelector('.composer-send').getBoundingClientRect(), attach = document.querySelector('.composer-row > .icon-button')?.getBoundingClientRect();
        const pills = [...document.querySelectorAll('.pill')].map((node) => node.getBoundingClientRect()); const shut = (box) => box.left >= (attach?.right ?? row.left) - 1 && box.right <= send.left + 1;
        return { count: pills.length, one: pills.every((box) => Math.abs(box.top - pills[0].top) < 2), between: pills.every(shut), composer: document.querySelector('.composer').getBoundingClientRect().height }; })()`);
      if (fit.count !== 4 || !fit.one || !fit.between || fit.composer > 110) await fail(`the four pills at ${width} px are ${JSON.stringify(fit)}`);
      await shot(`pills-four-${width}`, ".composer-send");
    }
    await resize([390, 844, 2]);
  } finally {
    await evaluate("window.tweak = null");
    for (const [name, on] of [["nightly", true], ["idle", true], ["weekly", false]]) {
      const row = (await served(project)).body.routines.find((each) => each.name === name);
      if (row && row.enabled !== on) await post(`api/tasks/${project}/routines/${name}/${on ? "enable" : "disable"}`);
    }
  }
}

// --- The Overview button in the chat header (icon only), and Settings' "Header: top | bottom" (docs/web-client.md "Header position").
{
  try {
    await go(chat);
    await until(`location.hash === '${chat}' && !!document.querySelector('.manager a.icon-button[href$="/page"]')`, "the chat header has no overview item");
    // Overview: an icon-only button too; one tap opens the project overview.
    const overview = await evaluate(`(() => { const node = document.querySelector('.manager a.icon-button[href$="/page"]'); return node && { label: node.getAttribute('aria-label'), title: node.title, icon: !!node.querySelector('svg.icon'), words: node.textContent.trim() }; })()`);
    if (!overview || overview.label !== "Overview" || overview.title !== "Overview" || !overview.icon || overview.words) await fail(`the Overview button is ${JSON.stringify(overview)}`);
    await click('.manager a.icon-button[href$="/page"]');
    await until(`location.hash === '#/p/${project}/page' && !!document.querySelector('dialog.task-sheet[open] .project-overview h1')`, "a tap on the Overview button did not open the overview");
    await closeSheet("the overview sheet", chat);
  } finally {
  }

  // The header at the bottom: the chat's bar sits between the log and the composer, the other
  // screens' bar at the foot of the page, and the choice survives a reload.
  const place = () => evaluate(`(() => { const box = (selector) => document.querySelector(selector)?.getBoundingClientRect();
    const bar = box('.page > .bar, .chat > .bar'), composer = box('.composer'), log = box('.log'), page = box('.page');
    return { side: document.documentElement.dataset.bar ?? 'top', bar: bar && [Math.round(bar.top), Math.round(bar.bottom)], composer: composer && Math.round(composer.top),
      log: log && Math.round(log.bottom), page: page && [Math.round(page.top), Math.round(page.bottom)], menu: document.querySelector('.more-menu') ? 1 : 0 }; })()`);
  await go("#/settings/ui");
  await until("!!document.querySelector('input[name=bar]')", "Settings has no header setting");
  if (await evaluate("document.querySelector('input[name=bar]:checked').value") !== "top") await fail("the header setting does not default to top");
  await click("input[name=bar][value=bottom]");
  await until("document.documentElement.dataset.bar === 'bottom'", "the header setting did not take effect");
  const reload = async (ready) => { await send("Page.reload"); await until(ready, "the page did not come back after a reload"); };
  await reload("document.documentElement.dataset.bar === 'bottom' && !!document.querySelector('input[name=bar]')");
  if (await evaluate("document.querySelector('input[name=bar]:checked').value") !== "bottom") await fail("the header setting did not survive a reload");
  for (const wide of [false, true]) {
    await resize(wide ? [1280, 800, 1] : [390, 844, 2]);
    await go(chat);
    await until(`!!document.querySelector('.manager .chat > .bar') && !!document.querySelector('.composer')`, "the chat has no bottom bar");
    const low = await place();
    if (low.bar[0] < low.log || low.bar[1] > low.composer + 1 || low.composer - low.bar[1] > 2) await fail(`the chat's bar is not just above the composer (${wide ? "wide" : "phone"}): ${JSON.stringify(low)}`);
    await click(".more-button");
    const opens = await evaluate("(() => { const menu = document.querySelector('.more-menu').getBoundingClientRect(), button = document.querySelector('.more-button').getBoundingClientRect(); return menu.bottom <= button.top + 1 && menu.top >= 0; })()");
    if (!opens) await fail("the ⋯ menu does not open upward from the bottom bar");
    await click(".more-button");
    await shot(wide ? "header-bottom-wide" : "header-bottom");
    if (!wide) {
      // ^ The keyboard up (as iOS: only the visual viewport shrinks): the bar rides with the composer above the keyboard's top edge.
      await typeInto(".composer-field", "");
      await keyboard(420);
      await pause(300);
      const up = await evaluate(`(() => { const box = (selector) => document.querySelector(selector).getBoundingClientRect();
        return { bar: Math.round(box('.chat > .bar').bottom), composer: Math.round(box('.composer').top), end: Math.round(box('.composer').bottom),
          visible: Math.round(visualViewport.offsetTop + visualViewport.height), keyboard: document.body.hasAttribute('data-keyboard') }; })()`);
      if (!up.keyboard || up.bar > up.composer + 1 || Math.abs(up.end - up.visible) > 1) await fail(`the bottom bar is not above the composer on the keyboard: ${JSON.stringify(up)}`);
      await shot("header-bottom-keyboard", ".composer-send");
      await evaluate("document.activeElement.blur()");
      await keyboard(null);
      await pause(400);
    }
    await go("#/p/" + project + "/files");
    await until("!!document.querySelector('.page.files > .bar')", "Files did not open");
    const files = await place();
    if (files.bar[1] < files.page[1] - 1) await fail(`the files bar is not at the foot of the page: ${JSON.stringify(files)}`);
    await go(chat);
    await until("!!document.querySelector('.manager .chat > .bar')", "the chat did not return");
    await go(`#/p/${project}/usage`);
    await until("!!document.querySelector('dialog.task-sheet[open] .task-page > .bar')", "Usage did not open");
    const sheet = await evaluate("(() => { const bar = document.querySelector('dialog.task-sheet[open] .task-page > .bar').getBoundingClientRect(), page = document.querySelector('dialog.task-sheet[open] .task-page').getBoundingClientRect(); return bar.bottom >= page.bottom - 1; })()");
    if (!sheet) await fail("a sheet's bar is not at its foot");
    await closeSheet("the usage sheet", chat);
  }
  await resize([390, 844, 2]);
  await go("#/settings/ui");
  await until("!!document.querySelector('input[name=bar]')", "Settings did not return");
  await click("input[name=bar][value=top]");
  await until("!document.documentElement.dataset.bar", "the header setting did not go back to top");
  await go(chat);
  await until("!!document.querySelector('.manager > .bar') && !!document.querySelector('.log .bubble')", "the chat's bar is not back on top");
}

// --- A turn of the manager: two messages group on the right; while it runs a typing bubble
// ends the chat and the header says "typing"; the reply edits the page (with a link to it),
// has short chips (the same task twice as two plain chips) and a table that scrolls inside it.
// ^ #1's landing (Land above) ends when its checks pass. Its event must reach the chat before the
// two sends, else it goes between them and they do not group.
await until(`${statusSays(1, "done")} && !document.querySelector('.log .typing')`, "the landing of #1 left no muted line", 300);
await comeBack();
await typeInto(".composer-field", "SLOW TABLE");
await click(".composer-send");
await until("!document.querySelector('.since')", "the since line stayed after the owner sent a message");
// ^ A send in flight ignores another tap: the field clears when the server has the message.
await until("document.querySelector('.composer-field').value === ''", "the first message was not sent");
await typeInto(".composer-field", "PAGE Goal Ship the chat");
await click(".composer-send");
await until("document.querySelector('.composer-field').value === ''", "the second message was not sent");
await until("!!document.querySelector('.log .typing') && document.querySelector('.manager .bar-state')?.textContent === 'typing'", "no typing bubble while the manager's turn runs", 30);
const headDots = await evaluate("[...document.querySelectorAll('.manager .bar-state .typing-dot')].map((dot) => getComputedStyle(dot).animationName + ' ' + getComputedStyle(dot).animationDelay)");
if (headDots.join() !== "typing 0s,typing 0.32s,typing 0.64s") await fail(`the header's typing dots are ${headDots}`);
const typing = await evaluate("({ author: document.querySelector('.log .typing').closest('.bubble').querySelector('.bubble-author').textContent, said: document.querySelector('.log .typing .visually-hidden').textContent })");
if (typing.author !== "Atlas" || typing.said !== "Atlas is typing") await fail(`the typing bubble reads ${JSON.stringify(typing)}`);
if (await evaluate("document.querySelector('.log .bubble:last-child .typing') === null")) await fail("the typing bubble is not the last message");
await oneWorkingColor("a typing manager", 4);
await shot("typing", ".composer-send");
const sentPair = (await bubbles()).filter((bubble) => bubble.side === "out").slice(-2);
if (sentPair.length !== 2 || sentPair[0].last || sentPair[1].first || !sentPair[1].last) await fail(`two owner messages did not group: ${JSON.stringify(sentPair)}`);
await until("[...document.querySelectorAll('.log .bubble[data-side=in]')].some((node) => !!node.querySelector('.table-wrap'))", "the reply with a table did not arrive", 60);
if ((await evaluate("[...document.querySelectorAll('.log .bubble[data-fresh]')].map((node) => getComputedStyle(node).animationName).join()")) === "") await fail("the reply did not slide in");
if (await evaluate("[...document.querySelectorAll('.log .bubble[data-fresh]')].some((node) => getComputedStyle(node).animationName !== 'slide-up')")) await fail("a new bubble does not play row-in");
const tableReply = "[...document.querySelectorAll('.log .bubble[data-side=in]')].find((node) => !!node.querySelector('.table-wrap'))";
const inside = await evaluate(`(() => { const bubble = ${tableReply}.querySelector('.bubble-box').getBoundingClientRect(), table = ${tableReply}.querySelector('.table-wrap');
  const box = table.getBoundingClientRect(); return { right: box.right <= bubble.right + 0.5, scrolls: table.scrollWidth > table.clientWidth + 1 }; })()`);
// ^ On a phone its long cells wrap, so it fits without scrolling sideways (docs/web-client-chat.md "Markdown").
if (!inside.right || inside.scrolls) await fail(`the table does not fit inside its bubble: ${JSON.stringify(inside)}`);
await chipsOnLine("the chat's bubbles", ".log .bubble .task-chip");
const replyChips = await evaluate(`[...${tableReply}.querySelectorAll('.task-chip')].map((chip) => chip.textContent.trim())`);
if (replyChips.filter((chip) => chip === "#4").length !== 2 || replyChips.some((chip) => !/^#\d+$/.test(chip))) await fail(`the reply's chips read ${JSON.stringify(replyChips)}`);
await until("!document.querySelector('.log .typing')", "the typing bubble stayed after the reply", 30);
if (await evaluate("document.querySelector('.manager .bar-state')?.textContent ?? ''")) await fail("the header still names the manager after its turn");
await shot("project-reply", ".composer-send");
// The second message's reply links "project overview"; the overview has the manager's edit.
await until("document.querySelectorAll('.log .page-link').length > 0", "the reply about the page has no link", 60);
await evaluate("[...document.querySelectorAll('.log .page-link')].at(-1).click()");
await until("/Ship the chat/.test(document.querySelector('.project-overview')?.textContent ?? '')", "the page does not show the manager's edit", 30);
await closeSheet("the page");

// --- A new conversation (BIG passes the manager's rotateTokens) shows once: it has no id, so
// the reads after it return it again until the next message.
const newConversations = "[...document.querySelectorAll('.log .note')].filter((node) => node.textContent.startsWith('New conversation')).length";
const shownBefore = await evaluate(newConversations);
await typeInto(".composer-field", "BIG");
await click(".composer-send");
await until(`${newConversations} > ${shownBefore}`, "the new conversation after BIG does not show", 60);
await typeInto(".composer-field", "STATUS after the rotation");
await click(".composer-send");
await until("[...document.querySelectorAll('.log .bubble[data-side=out]')].some((node) => node.textContent.includes('STATUS after the rotation')) && !document.querySelector('.log .typing')", "the message after the rotation got no answer", 60);
await pause(1500);
if (await evaluate(newConversations) !== shownBefore + 1) await fail(`the new conversation shows ${(await evaluate(newConversations)) - shownBefore} times`);

// --- A turn that ends while the page is offline, or hidden and offline as iOS keeps a PWA in
// the background: the stream misses the reply and the end of the turn. Back online and
// visible, the page reads the chat again, so the typing dots go and the reply shows without
// a reload (docs/web-client.md "Event stream"). ^ Not Page.setWebLifecycleState: a page
// frozen and made active again in headless Chrome no longer renders, so no scroll event fires.
const network = (offline) => send("Network.emulateNetworkConditions", { offline, latency: 0, downloadThroughput: -1, uploadThroughput: -1 });
const visibility = (state) => evaluate(`Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => '${state}' });
  document.dispatchEvent(new Event('visibilitychange'))`);
const replies = "document.querySelectorAll('.log .bubble[data-side=in]').length";
const offlineFrom = problems.length;
for (const away of ["offline", "hidden"]) {
  const before = await evaluate(replies);
  await typeInto(".composer-field", `SLOW STATUS while ${away}`);
  await click(".composer-send");
  await until("!!document.querySelector('.log .typing') && !!document.querySelector('.manager .bar-state .typing-dot')", `no typing dots before the page went ${away}`, 30);
  if (away === "hidden") await visibility("hidden");
  await network(true);
  // ^ The fake manager's SLOW turn takes about 3 s.
  await pause(7000);
  await network(false);
  if (away === "hidden") await visibility("visible");
  await until(`!document.querySelector('.log .typing') && !document.querySelector('.manager .bar-state') && ${replies} > ${before}`,
    `the typing dots stayed or the reply did not show after the page was ${away}`, 50);
}
await evaluate("delete document.visibilityState");
// ^ The network between the page and the server goes silent without an error, as a phone's
// does after a network change or a sleep (`Relay` in test-web-client.py): the stream looks
// open and says nothing. A page back from the background opens its stream again at once; a
// visible page opens it again when its pings stop. A chat read that never answers gives up,
// so the next one runs (docs/web-client.md "Event stream").
const relay = async (word) => {
  fs.writeFileSync(relayFile, word);
  for (let waited = 0; !fs.existsSync(`${relayFile}.done`); waited += 100) {
    if (waited > 10_000) await fail(`the relay did not take ${word}`);
    await pause(100);
  }
  fs.unlinkSync(`${relayFile}.done`);
};
for (const [away, within] of [["silent and back", 8], ["silent", 40], ["a read that never answers", 40]]) {
  const before = await evaluate(replies);
  await typeInto(".composer-field", `SLOW STATUS while ${away}`);
  await click(".composer-send");
  await until("!!document.querySelector('.log .typing')", `no typing dots before ${away}`, 30);
  await relay(away === "a read that never answers" ? "chat" : "events");
  if (away === "a read that never answers") {
    // ^ The next chat read (the event of the reply) hangs; a message during it reads again.
    await pause(5000);
    await typeInto(".composer-field", `STATUS after ${away}`);
    await click(".composer-send");
  }
  if (away === "silent and back") { await visibility("hidden"); await visibility("visible"); }
  const sent = Date.now();
  await until(`!document.querySelector('.log .typing') && ${replies} > ${before}`, `the typing dots stayed or the reply did not show: ${away}`, (within + 10) * 10);
  if ((Date.now() - sent) / 1000 > within) await fail(`the reply took ${Math.round((Date.now() - sent) / 1000)} s to show: ${away}`);
}
await evaluate("delete document.visibilityState");
// ^ The stream opens again (its "Connection lost" notice goes) and reads once more.
await until("!document.querySelector('.notice[data-offline]')", "the stream did not open again after the page was offline", 150);
// ^ The reads the page tried while offline failed, as they should.
problems.splice(offlineFrom, problems.length - offlineFrom, ...problems.slice(offlineFrom).filter((problem) => !/ERR_INTERNET_DISCONNECTED/.test(problem)));

// --- The typing bubble is as wide as its dots, with even padding, and every dot shows whole
// (docs/web-client-tasks.md "Project screen").
await typeInto(".composer-field", "LONG STATUS");
await click(".composer-send");
await until("!!document.querySelector('.log .typing')", "no typing bubble for a long turn", 50);
const typingFit = `(() => { const box = document.querySelector('.log .typing'), outer = box.getBoundingClientRect();
  const dots = [...box.querySelectorAll('.typing-dot')].map((dot) => [dot, dot.getBoundingClientRect()]);
  const style = getComputedStyle(box), inner = { left: outer.left + parseFloat(style.borderLeftWidth), right: outer.right - parseFloat(style.borderRightWidth) };
  const hidden = dots.filter(([dot, at]) => [[at.left + 1, at.top + at.height / 2], [at.right - 1, at.top + at.height / 2], [at.left + at.width / 2, at.top + at.height / 2]]
    .some(([x, y]) => document.elementFromPoint(x, y) !== dot) || at.left < inner.left || at.right > inner.right).length;
  return { dots: dots.length, hidden, left: Math.round(dots[0][1].left - inner.left), right: Math.round(inner.right - dots.at(-1)[1].right) }; })()`;
for (const [width, height, scale] of [[390, 844, 2], [1280, 800, 1]]) {
  await resize([width, height, scale]);
  await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
  await pause(400);
  const fit = await evaluate(typingFit);
  if (fit.dots !== 3 || fit.hidden || Math.abs(fit.left - fit.right) > 1) await fail(`the typing bubble does not fit its dots at ${width} px: ${JSON.stringify(fit)}`);
  await shot(`typing-bubble-${width}`, ".composer-send");
}
await resize([390, 844, 2]);
// The dots loop after a step back in history and after a return from the back-forward cache,
// also when the engine left them standing as WebKit can after an iOS swipe back: each
// animation runs and the dot moves (docs/web-client.md "Motion").
const dotRuns = (selector) => evaluate(`new Promise((done) => { const dot = document.querySelector(${JSON.stringify(selector)});
  if (!dot) { done({ found: false }); return; }
  const states = dot.getAnimations().map((animation) => animation.playState), tops = [];
  const timer = setInterval(() => { tops.push(dot.getBoundingClientRect().top);
    if (tops.length === 15) { clearInterval(timer); done({ found: true, states, moved: Math.max(...tops) - Math.min(...tops) }); } }, 100); })`);
const stand = () => evaluate("document.getAnimations().forEach((animation) => animation.pause())");
const runs = async (where, selector) => {
  const seen = await dotRuns(selector);
  if (!seen.found || !seen.states.length || seen.states.some((state) => state !== "running") || seen.moved < 0.5) await fail(`the typing dots stand still ${where}: ${JSON.stringify(seen)}`);
};
await go(`#/p/${project}/t/2`);
await until("!!document.querySelector('dialog.task-sheet[open]')", "the task sheet did not open over the long turn");
await stand();
await evaluate("history.back()");
await until(`location.hash === '#/p/${project}' && !document.querySelector('dialog.task-sheet')`, "history.back() did not close the sheet");
await runs("after history.back()", ".log .typing .typing-dot");
await stand();
await evaluate("dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }))");
await runs("after a pageshow from the back-forward cache", ".log .typing .typing-dot");
await until("!document.querySelector('.log .typing')", "the long turn did not end", 400);

// --- The task sheet: a chip or a reservoir row opens it over the chat, and the chat does not
// scroll; Escape, Back and a swipe down close it, the composer keeps its draft and gets the
// focus back.
const logTop = () => evaluate("document.querySelector('.log').scrollTop");
await evaluate("document.querySelector('.log').scrollTop = 40");
await pause(200);
const top0 = await logTop();
await openReservoir();
await evaluate(`${rowOf("#2")}.querySelector('.row-open').click()`);
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('dialog.task-sheet[open] .story')`, "a reservoir row did not open the task sheet");
// ^ The sheet's agent: the detail's `agent` field and its words beside the health.
const sheetAgent = await evaluate(`fetch('api/tasks/${project}/t/2').then((r) => r.json()).then((view) => view.agent)`);
if (!sheetAgent?.model || sheetAgent.fallback !== false) await fail(`the task detail's agent is ${JSON.stringify(sheetAgent)}`);
if (!sheetAgent.effort || (await text(".task-sheet .progress-model"))[0] !== `${sheetAgent.model} ${sheetAgent.effort}`) await fail("the open sheet does not show the model and effort");
// ^ The task's usage line (docs/web-client-tasks.md "Task sheet").
const usageLine = (await text(".task-sheet .task-usage"))[0] ?? "";
if (!/^Usage: \$\d+\.\d\d · in \d+k? · cache \S+ read \/ \S+ write · out \S+ · \d+ turns?$/.test(usageLine)) await fail(`the task's usage line reads ${usageLine}`);
const phoneSheet = await evaluate("({ sheet: getComputedStyle(document.querySelector('dialog.task-sheet')).animationName, backdrop: getComputedStyle(document.querySelector('dialog.task-sheet'), '::backdrop').animationName })");
if (phoneSheet.sheet !== "sheet-in" || phoneSheet.backdrop !== "arrive") await fail(`the sheet enters with ${JSON.stringify(phoneSheet)}`);
if (await logTop() !== top0) await fail(`a row moved the chat from ${top0} to ${await logTop()}`);
await closeSheet("the sheet a row opened", chat);
await closeReservoir();
if (await logTop() !== top0) await fail(`closing the sheet moved the chat from ${top0} to ${await logTop()}`);
await evaluate("[...document.querySelectorAll('.log .task-chip')].find((chip) => chip.textContent.trim() === '#2').click()");
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('dialog.task-sheet[open] .story')`, "a chip did not open the task sheet");
if (await logTop() !== top0) await fail(`a chip moved the chat from ${top0} to ${await logTop()}`);
await closeSheet("the sheet a chip opened");
await click(".jump");
await until(atEnd, "Jump to latest did not return to the end");
await typeInto(".composer-field", "a draft that stays");
await evaluate("[...document.querySelectorAll('.log .task-chip')].find((chip) => chip.textContent.trim() === '#2').click()");
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('dialog.task-sheet[open] .story')`, "a chip did not open the task sheet");
await until("document.querySelector('dialog.task-sheet')?.getAnimations().length === 0", "the sheet kept moving");
const sheetBox = await evaluate("(() => { const r = document.querySelector('dialog.task-sheet').getBoundingClientRect(); return { top: Math.round(r.top), height: Math.round(r.height) }; })()");
if (Math.abs(sheetBox.height - 844 * 0.85) > 2 || Math.abs(sheetBox.top - 844 * 0.15) > 2) await fail(`the sheet is not 85% high: ${JSON.stringify(sheetBox)}`);
await closeSheet("the sheet", chat);
const kept = await evaluate("({ value: document.querySelector('.composer-field').value, focused: document.activeElement === document.querySelector('.composer-field') })");
if (kept.value !== "a draft that stays" || !kept.focused) await fail(`after the sheet the composer has ${JSON.stringify(kept)}`);
await go(page(2));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the sheet did not open again");
await evaluate("document.querySelector('.task-sheet .bar .icon-button').click()");
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Back did not close the sheet");
await go(page(2));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the sheet did not open a third time");
await until("document.querySelector('dialog.task-sheet')?.getAnimations().length === 0", "the sheet kept moving");
const grip = await evaluate("(() => { const r = document.querySelector('.task-sheet-top').getBoundingClientRect(); return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) }; })()");
await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: grip.x, y: grip.y }] });
for (let step = 1; step <= 6; step++) await send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: [{ x: grip.x, y: grip.y + step * 30 }] });
await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "a swipe down did not close the sheet");
if (await evaluate("document.querySelector('.composer-field').value") !== "a draft that stays") await fail("the swipe lost the draft");
await typeInto(".composer-field", "");

// --- The `#` picker: every task, filtered by number or words, a pick inserts `#n `.
const taskCount = await evaluate(`fetch('api/tasks/${project}').then((r) => r.json()).then((view) => view.tasks.length)`);
await typeInto(".composer-field", "land #");
await until(`document.querySelectorAll('.slash-option').length === ${taskCount}`, "the # picker did not list the tasks");
// ^ Words of one title: only #6 keeps a worker busy.
await typeInto(".composer-field", "land #busy");
await until("document.querySelectorAll('.slash-option').length === 1", "the picker does not filter by words");
await key("Enter", 13);
await until("document.querySelector('.composer-field').value === 'land #6 '", "Enter did not insert the task");
if (await count(".slash")) await fail("the picker stayed open after a pick");
await typeInto(".composer-field", "#qqq");
await until("(document.querySelector('.slash-note')?.textContent ?? '') === 'No task matches that.'", "no note for a picker without matches");
await key("Escape", 27);
await until("!document.querySelector('.slash')", "Escape did not close the picker");
// A task option reads its title, then its number. Tasks done more than a day ago fold away
// until the text names them; #7 and #8 landed, so the store backdates them in the page.
await typeInto(".composer-field", "#");
await until(`document.querySelectorAll('.slash-option').length === ${taskCount}`, "the picker did not reopen for its order");
const optionOrder = await evaluate("[...document.querySelectorAll('.slash-option')].map((node) => node.querySelector('.slash-name').nextElementSibling?.className + ' ' + node.querySelector('.task-number')?.textContent)");
if (optionOrder.some((line) => !/^task-number #\d+$/.test(line))) await fail(`a task option does not read title, then number: ${JSON.stringify(optionOrder)}`);
const fresh = await inStore(`return JSON.parse(JSON.stringify(state.tasks[${project}]))`);
await inStore(`const ov = state.tasks[${project}];
  update({ tasks: { ...state.tasks, [${project}]: { ...ov, tasks: ov.tasks.map((task) => task.stage === 'done' ? { ...task, updated_unix_ms: Date.now() - 2 * 86400000 } : task) } } })`);
const doneCount = fresh.tasks.filter((task) => task.stage === "done").length;
await typeInto(".composer-field", "#");
await until(`document.querySelectorAll('.slash-option').length === ${taskCount - doneCount}`, "the picker did not fold the old done tasks");
const foldNote = await text(".slash-note");
if (!doneCount || foldNote.join() !== `${doneCount} done more than a day ago: type a number or a word to find them.`) await fail(`the fold note reads ${JSON.stringify(foldNote)} for ${doneCount} done tasks`);
await typeInto(".composer-field", "#7");
await until("[...document.querySelectorAll('.slash-option .task-number')].some((node) => node.textContent === '#7')", "a folded task is not found by its number");
await inStore(`update({ tasks: { ...state.tasks, [${project}]: ${JSON.stringify(fresh)} } })`);
await typeInto(".composer-field", "#");
await until(`document.querySelectorAll('.slash-option').length === ${taskCount}`, "the picker did not reopen");
// With the keyboard up: the picker keeps to 3.5 rows above the composer, the reservoir steps aside.
await keyboard(420);
await pause(300);
const fit = await evaluate(`(() => { const box = (selector) => document.querySelector(selector).getBoundingClientRect();
  return { list: Math.round(box('.slash').height), row: Math.round(box('.slash-option').height),
    composer: Math.round(box('.composer').bottom), visible: Math.round(visualViewport.offsetTop + visualViewport.height),
    reservoir: document.querySelector('.reservoir')?.getBoundingClientRect().height ?? 0 }; })()`);
if (fit.list > fit.row * 3.5 + 2 || Math.abs(fit.composer - fit.visible) > 1 || fit.reservoir) await fail(`the picker does not fit above the keyboard: ${JSON.stringify(fit)}`);
await shot("picker-keyboard", ".composer-send");
await typeInto(".composer-field", "");
await shot("project-keyboard", ".composer-send");
await evaluate("document.activeElement.blur()");
await keyboard(null);
await resize([390, 420, 2]);
await shot("project-short", ".composer-send");
await resize([390, 844, 2]);

// --- Following the end (docs/web-client-chat.md "Following the end"). A reply arrives the way
// another device's message does (not the composer, which goes to the end).
// ^ Each one spends the task-action budget (10 at once, 20 a minute, crates/wsx-web/src/http.rs
// act_budget); the page waits when it is spent (`watchFetch`), so a say or a tap is never refused.
const say = (words, of = project) => evaluate(`fetch('api/tasks/${of}/say', { method: 'POST', headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ text: ${JSON.stringify(words)} }) }).then((response) => response.status)`);
const fromEnd = () => evaluate("(() => { const log = document.querySelector('.log'); return Math.round(log.scrollHeight - log.scrollTop - log.clientHeight); })()");
const drawnDiagrams = "document.querySelectorAll('.log .bubble .diagram img').length";
// The manager answers with a diagram; the bubble shows "Drawing the diagram…" first.
async function diagramReply() {
  const before = await evaluate(drawnDiagrams);
  if (await say("DIAGRAM") !== 200) await fail("the message for a diagram reply was not taken");
  await until(`${drawnDiagrams} > ${before} && !document.querySelector('.log .typing')`, "the reply with a diagram did not arrive", 240);
  await evaluate("Promise.all([...document.querySelectorAll('.log .diagram img')].map((image) => image.decode()))");
  await pause(300);
}
const toLatest = async (where) => {
  await evaluate("document.querySelector('.jump')?.click()");
  await until(atEnd, `${where}: Jump to latest did not return to the end`);
};
// The probe is the first bubble wholly in view; its distance from the screen's top must not change.
const probe = "document.querySelector('[data-follow-probe]').getBoundingClientRect().top";
const markProbe = `(() => { const top = document.querySelector('.log').getBoundingClientRect().top;
  document.querySelector('[data-follow-probe]')?.removeAttribute('data-follow-probe');
  const bubble = [...document.querySelectorAll('.log > .bubble')].find((node) => node.getBoundingClientRect().top > top + 20);
  bubble.setAttribute('data-follow-probe', ''); return ${probe}; })()`;
// ^ The probe's place in the log box, from layout: the page moves on the keyboard, the text must not.
const probeInLog = "(() => { const log = document.querySelector('.log'); return document.querySelector('[data-follow-probe]').offsetTop - log.scrollTop; })()";
// The newest reply with the last named row, its bottom's distance from the log's visible bottom,
// the log's distance from its end and how far the last row reaches below the composer's top, read
// at each frame. A row that arrives under the reply (an event line) moves it as the end is kept:
// its reading names another last row. The typing bubble has no name.
const tail = `(() => { const log = document.querySelector('.log'); const reply = [...log.querySelectorAll(':scope > .bubble[data-in][data-key]')].at(-1);
  const last = [...log.querySelectorAll(':scope > [data-key]')].at(-1);
  return [(reply?.dataset.key ?? '') + ' ' + last.dataset.key, reply ? log.clientHeight - (reply.offsetTop + reply.offsetHeight - log.scrollTop) : null,
    log.scrollHeight - log.scrollTop - log.clientHeight, last.getBoundingClientRect().bottom - document.querySelector('.composer').getBoundingClientRect().top]; })()`;
const scrolledUp = "(() => { const log = document.querySelector('.log'); log.scrollTop = log.scrollHeight - log.clientHeight * 2; log.dispatchEvent(new Event('scroll')); })()";
// ^ As an iPhone 13 does: the keyboard leaves 508 px and the visual viewport moves 336 px down the page.
async function keyboardUpAndDown() {
  await evaluate("document.querySelector('.composer-field').focus({ preventScroll: true })");
  await keyboard(508, 336);
  await pause(400);
  await evaluate("document.activeElement.blur()");
  await keyboard(null);
  // ^ Past the second look main.js takes after the focus left.
  await pause(700);
}
const barAt = async (side) => {
  await go("#/settings/ui");
  await until("!!document.querySelector('input[name=bar]')", "Settings has no header setting");
  await click(`input[name=bar][value=${side}]`);
  await until(`(document.documentElement.dataset.bar ?? 'top') === '${side}'`, `the header did not go to the ${side}`);
  await go(chat);
  await until("!!document.querySelector('.log .bubble') && !!document.querySelector('.composer-field')", "the chat did not come back");
  await pause(300);
};
async function followEnd(where) {
  const moves = [];
  // (a) At the end, a reply whose diagram draws after it shows: still at the end. From the frame
  // it shows, the reply keeps its place over the composer: the typing bubble it replaces does
  // not come back under it, and the end is kept in every frame.
  await toLatest(where);
  await watch(tail);
  await diagramReply();
  const replied = await watched();
  const lateDiagram = await fromEnd();
  if (lateDiagram > 2) await fail(`${where}: a diagram that drew late left the chat ${lateDiagram} px above its end`);
  const replyMoves = jumps(replied.map(([key, gap]) => [key, gap]));
  if (replyMoves.length) moves.push(`at the end, the new reply moved ${JSON.stringify(replyMoves)}`);
  const offEnd = Math.max(...replied.map((reading) => reading[2]));
  if (offEnd > 2) moves.push(`at the end, a frame showed the chat ${Math.round(offEnd)} px above its end`);
  for (let more = 0; more < 4 && await evaluate("(() => { const log = document.querySelector('.log'); return log.scrollHeight < log.clientHeight * 3; })()"); more++) await diagramReply();
  // (b, c) Scrolled up: rows below, a late diagram below, the "Since you left" line above, rows
  // inserted above (as older messages would be) and removed again keep the bubble in view where it is.
  const drift = [];
  for (const anchoring of ["auto", "none"]) {
    // ^ iOS Safari has no CSS scroll anchoring: with it off, Chrome scrolls as Safari does.
    await evaluate(`document.querySelector('.log').style.overflowAnchor = '${anchoring}'`);
    await evaluate(scrolledUp);
    await pause(200);
    const start = await evaluate(markProbe);
    const moved = async (step) => drift.push(`${anchoring} ${step} ${Math.round((await evaluate(probe)) - start)}`);
    // ^ Every frame between the steps, not only after each: a move that is taken back still shows.
    await watch(probe);
    await diagramReply();
    await moved("reply below");
    await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 }))`);
    await evaluate("document.dispatchEvent(new Event('visibilitychange'))");
    await until("!!document.querySelector('.since')", `${where}: no since-you-left line`);
    await pause(300);
    await moved("since line");
    await evaluate("document.querySelector('.since button[title=Dismiss]').click()");
    await pause(300);
    await moved("since dismissed");
    await evaluate(`(() => { const log = document.querySelector('.log');
      for (let n = 0; n < 5; n++) {
        const row = Object.assign(document.createElement('p'), { className: 'note', textContent: 'an older message' });
        row.setAttribute('data-older', '');
        log.prepend(row);
      } })()`);
    await pause(200);
    await moved("rows above");
    await evaluate("document.querySelectorAll('.log > [data-older]').forEach((node) => node.remove())");
    await pause(200);
    await moved("rows above removed");
    // An image that loads in a bubble above the screen (a diagram's image again). ^ Not a row that
    // starts in view: it is the anchor, and what is below it on the screen moves as it grows.
    const above = await evaluate(`(() => { const log = document.querySelector('.log');
      const above = [...log.querySelectorAll(':scope > .bubble')].filter((node) => node.offsetTop + node.offsetHeight <= log.scrollTop).at(-1);
      if (!above) return false;
      const image = document.querySelector('.log .diagram img').cloneNode(); image.setAttribute('data-late', ''); above.append(image); return image.decode().then(() => true); })()`);
    if (!above) await fail(`${where}: scrolled up, no bubble is wholly above the screen`);
    await pause(200);
    await moved("image above");
    await evaluate("document.querySelector('.log [data-late]').remove()");
    await pause(200);
    const frames = jumps(await watched());
    if (frames.length) moves.push(`scrolled up (${anchoring}), the bubble in view moved in frames ${JSON.stringify(frames)}`);
    if (await evaluate(atEnd) || !(await count(".jump"))) drift.push(`${anchoring}: the reader was moved to the end`);
  }
  await evaluate("document.querySelector('.log').style.overflowAnchor = ''");
  // ^ A passing check prints only its last line: the measures are kept beside the captures.
  fs.appendFileSync(path.join(captures, "follow-end.txt"), `following the end, ${where}: late diagram ${lateDiagram} px from the end, ${drift.join(", ")}\n`);
  if (drift.some((line) => !/ -?[0-2]$/.test(line))) await fail(`${where}: the bubble in view moved: ${drift.join(", ")}`);
  // (d) On a phone with the composer focused and the keyboard up: a late diagram keeps the end.
  await toLatest(where);
  if (phone()) {
    await evaluate("document.querySelector('.composer-field').focus()");
    await keyboard(508, 336);
    await pause(300);
    const keyboardUp = await fromEnd();
    await diagramReply();
    const replied = await fromEnd();
    await evaluate("document.activeElement.blur()");
    await keyboard(null);
    if (keyboardUp > 2 || replied > 2) await fail(`${where}: with the keyboard up the chat is ${keyboardUp} px, after a reply ${replied} px above its end`);
    // (e) The keyboard opens and closes, with the header on top and at the bottom: a reader
    // scrolled up keeps the text in the log where it was in every frame and after; at the end
    // the chat stays at its end with its last message above the composer.
    for (const side of ["top", "bottom"]) {
      if (side === "bottom") await barAt("bottom");
      await evaluate(scrolledUp);
      await pause(200);
      await evaluate(markProbe);
      await watch(probeInLog);
      await keyboardUpAndDown();
      const read = await watched();
      const frames = jumps(read);
      if (frames.length || Math.abs(read.at(-1) - read[0]) > 3) moves.push(`header ${side}, scrolled up, the keyboard moved the text in frames ${JSON.stringify(frames)}, from ${Math.round(read[0])} to ${Math.round(read.at(-1))} px`);
      await toLatest(where);
      await watch(tail);
      await keyboardUpAndDown();
      const ends = await watched();
      const off = Math.max(...ends.map((reading) => reading[2])), under = Math.max(...ends.map((reading) => reading[3]));
      if (off > 2 || under > 1) moves.push(`header ${side}, at the end, a frame with the keyboard showed the chat ${Math.round(off)} px above its end, its last message ${Math.round(under)} px under the composer's top`);
    }
    await barAt("top");
    // (f) A find's match: a row with a match that arrives does not move a reader who scrolled away.
    await click('.manager button[aria-label="Find in chat"]');
    await until("document.activeElement === document.querySelector('.find-field')", `${where}: the find bar did not open`);
    await evaluate(`(() => { const field = document.querySelector('.find-field'); field.value = 'DIAGRAM'; field.dispatchEvent(new Event('input', { bubbles: true })); })()`);
    await until("/^[1-9]\\d*\\/\\d+$/.test(document.querySelector('.find-count').textContent)", `${where}: the find did not count its matches`, 200);
    await pause(300);
    await evaluate("(() => { const log = document.querySelector('.log'); log.scrollTop = 0; log.dispatchEvent(new Event('scroll')); })()");
    await pause(200);
    await watch("document.querySelector('.log').scrollTop");
    await evaluate("document.querySelector('.log').append(Object.assign(document.createElement('p'), { className: 'note', textContent: 'DIAGRAM later' }))");
    await pause(500);
    const found = jumps(await watched());
    if (found.length) moves.push(`a find moved the reader to a match that arrived, in frames ${JSON.stringify(found)}`);
    await evaluate("document.querySelector('.log > .note:last-child').remove()");
    await click('.find button[aria-label="Close find"]');
    await until("!document.querySelector('.find')", `${where}: the find bar did not close`);
    await toLatest(where);
  }
  fs.appendFileSync(path.join(captures, "follow-end.txt"), `steady scroll, ${where}: ${moves.join("; ") || "no move"}\n`);
  if (moves.length) await fail(`${where}: the chat moved without the reader: ${moves.join("; ")}`);
}
await followEnd("390 px");

// --- Opening a chat and replying (docs/web-client-chat.md "Opening a chat", "Replies"). This
// device keeps how far it read each chat and where it left it.
const jumpWords = () => evaluate("document.querySelector('.jump')?.textContent.trim() ?? ''");
// ^ The newest reply by its key: a count of replies stays the same once the log shows only its
// newest rows (`useRecent` in chat.js).
const lastReply = "[...document.querySelectorAll('.log > .bubble[data-in]')].at(-1)?.dataset.key";
const fiveColumns = "[...document.querySelectorAll('.log > .bubble[data-in]')].findLast((node) => node.dataset.quote === 'Five columns:')";
// The first named row that starts in view, and its distance from the log's top.
const rowInView = () => evaluate(`(() => { const top = document.querySelector('.log').getBoundingClientRect().top;
  const row = [...document.querySelectorAll('.log > [data-key]')].find((node) => node.getBoundingClientRect().top >= top);
  return row ? { key: row.dataset.key, down: Math.round(row.getBoundingClientRect().top - top) } : null; })()`);
const rowAt = (key) => evaluate(`(() => { const top = document.querySelector('.log').getBoundingClientRect().top;
  const row = [...document.querySelectorAll('.log > [data-key]')].find((node) => node.dataset.key === ${JSON.stringify(key)});
  return row ? Math.round(row.getBoundingClientRect().top - top) : null; })()`);
async function reopen(where) {
  await go("#/");
  await until("!!document.querySelector('.home-row') && !document.querySelector('.composer-field')", `${where}: home did not replace the chat`);
  await go(chat);
  await until("!!document.querySelector('.log > .bubble') && !!document.querySelector('.pill')", `${where}: the chat did not come back`);
  await pause(500);
}
// A touch swipe from `from` by `dx` px to the right, or a press held for `hold` ms.
// A two-finger pinch out around `at`: the fingers move from 20 px to 220 px apart.
async function pinch(at) {
  const points = (gap) => [{ x: at.x - gap / 2, y: at.y, id: 1 }, { x: at.x + gap / 2, y: at.y, id: 2 }];
  await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: points(20) });
  for (let step = 1; step <= 10; step++) await send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: points(20 + step * 20) });
  await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
}
async function touch(from, dx, hold = 0) {
  await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: from.x, y: from.y }] });
  if (hold) await pause(hold);
  for (let step = 1; step <= 5; step++) await send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: [{ x: from.x + (dx * step) / 5, y: from.y }] });
  await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
}
// Where to press a bubble: its first line of text, clear of tables and links.
const pressPoint = (bubble) => evaluate(`(() => { const first = ${bubble}.querySelector('.bubble-body p'); first.scrollIntoView({ block: 'center' }); const rect = first.getBoundingClientRect();
  return { x: Math.round(rect.left + 12), y: Math.round(rect.top + 8) }; })()`);
async function readAndReply(where) {
  await toLatest(where);
  // (a) Scrolled up: a reply below moves nothing, and the button counts it.
  await evaluate("document.querySelector('.log').scrollTop = 0");
  await pause(300);
  const held = await rowInView();
  const before = await evaluate(lastReply);
  if (await say("TABLE5") !== 200) await fail(`${where}: the message for a table reply was not taken`);
  await until(`${lastReply} !== ${JSON.stringify(before)} && !!${fiveColumns} && !document.querySelector('.log .typing')`, `${where}: the table reply did not arrive`, 240);
  await pause(300);
  const drift = (await rowAt(held.key)) - held.down;
  if (Math.abs(drift) > 2) await fail(`${where}: a reply below moved the row in view by ${drift} px`);
  if (await jumpWords() !== "1 new") await fail(`${where}: scrolled up, the button reads "${await jumpWords()}", not "1 new": ${await evaluate(`JSON.stringify((() => { const log = document.querySelector('.log');
    return { top: log.scrollTop, height: log.scrollHeight, client: log.clientHeight, rows: [...log.children].filter((row) => row.dataset.in != null && row.dataset.at).map((row) => [row.dataset.key, row.offsetTop, row.offsetHeight, row.dataset.at]).slice(-4),
      read: localStorage.getItem('wsx-read'), last: [...log.children].slice(-3).map((row) => [row.className, row.dataset.key, row.dataset.at]) }; })())`)}`);
  // (b) Left with that reply unread: the chat opens at a New line over it, at the top of the
  // screen, and the button still counts it.
  await reopen(where);
  const line = await evaluate(`(() => { const line = document.querySelector('.log > .log-new'), log = document.querySelector('.log'); if (!line) return null;
    return { down: Math.round(line.getBoundingClientRect().top - log.getBoundingClientRect().top), padding: parseFloat(getComputedStyle(log).paddingTop),
      next: line.nextElementSibling?.dataset.quote ?? '', words: line.textContent.trim() }; })()`);
  if (!line || Math.abs(line.down - line.padding) > 2 || line.next !== "Five columns:" || line.words !== "New") await fail(`${where}: the chat did not open at the New line: ${JSON.stringify(line)}`);
  if (await jumpWords() !== "1 new") await fail(`${where}: opened at the New line, the button reads "${await jumpWords()}"`);
  await shot("chat-unread", ".composer-send");
  // (c) The table: on a phone two columns, the others as "Header: value" lines in the second,
  // nothing left out and no sideways scroll; on a wide screen five columns.
  const table = await evaluate(`(() => { const wrap = ${fiveColumns}.querySelector('.table-wrap'), cells = [...wrap.querySelectorAll('tbody td')];
    const second = wrap.querySelector('tbody td:nth-child(2)').getBoundingClientRect(), third = wrap.querySelector('tbody td:nth-child(3)').getBoundingClientRect();
    return { fold: wrap.hasAttribute('data-fold'), heads: [...wrap.querySelectorAll('th')].filter((node) => node.getBoundingClientRect().width > 0).map((node) => node.textContent),
      sideways: wrap.scrollWidth > wrap.clientWidth + 1, tall: wrap.scrollHeight - wrap.clientHeight, hidden: cells.filter((cell) => !cell.textContent.trim() || cell.getBoundingClientRect().width === 0).length, cells: cells.length,
      named: wrap.querySelector('tbody td:nth-child(5)').dataset.head, under: Math.abs(third.left - second.left) < 1 && third.top >= second.bottom - 1 }; })()`);
  const tableOk = phone() ? table.fold && table.heads.join() === "Name,State" && table.under && table.named === "Note"
    : table.heads.join() === "Name,State,Owner,Since,Note" && !table.under;
  // ^ Twenty rows take their full height in the message: the table has no scroll of its own.
  if (!tableOk || table.sideways || table.tall > 1 || table.hidden || table.cells !== 100) await fail(`${where}: the five-column table reads ${JSON.stringify(table)}`);
  await capture(`${size[0]}x${size[1]}-table-five`, await evaluate(`(() => { const r = ${fiveColumns}.querySelector('.table-wrap').getBoundingClientRect(); return { x: Math.max(0, r.left - 8), y: Math.max(0, r.top - 8), width: Math.min(innerWidth, r.width + 16), height: Math.min(innerHeight - r.top, r.height + 16), scale: 1 }; })()`));
  await toLatest(where);
  if (await count(".jump")) await fail(`${where}: the button stayed at the end`);
  // (d) Nothing unread: the chat opens where it was left, without a New line.
  await evaluate("(() => { const log = document.querySelector('.log'); log.scrollTop = Math.round((log.scrollHeight - log.clientHeight) / 2); })()");
  await pause(300);
  const left = await rowInView();
  await reopen(where);
  const back = await rowAt(left.key);
  if (back === null || Math.abs(back - left.down) > 2 || await count(".log-new") || await jumpWords() !== "Jump to latest") {
    await fail(`${where}: the chat did not open where it was left: ${JSON.stringify({ left, back, line: await count(".log-new"), jump: await jumpWords() })}`);
  }
  // (e) Reply: a swipe right on a phone, the menu of a right click on a wide screen. × removes
  // the quote; a long press opens the menu too.
  const point = await pressPoint(fiveColumns);
  const quote = "(document.querySelector('.composer .reply-quote-text')?.textContent ?? '').replace('Replying to: ', '')";
  // ^ The menu closes on any scroll (reply.js), also one the page makes while a capture runs: tap
  // Reply when it is still there, else open it again.
  const menuReply = "(() => { const node = [...document.querySelectorAll('.reply-menu button')].find((node) => node.textContent.trim() === 'Reply'); node?.click(); return !!node; })()";
  const rightClickMenu = async (at) => {
    await evaluate(`${fiveColumns}.querySelector('.bubble-body p').dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: ${at.x}, clientY: ${at.y}, button: 2 }))`);
    await until("!!document.querySelector('.reply-menu')", `${where}: a right click opened no menu`);
  };
  const replyFromMenu = async (at) => {
    for (let attempt = 0; attempt < 5; attempt++) {
      await rightClickMenu(at);
      if (await evaluate(menuReply)) return;
    }
    await fail(`${where}: the reply menu closed before Reply was tapped`);
  };
  if (phone()) await touch(point, 90);
  else {
    await rightClickMenu(point);
    await shot("reply-menu");
    if (!(await evaluate(menuReply))) await replyFromMenu(point);
  }
  await until(`${quote} === 'Five columns:'`, `${where}: the reply shows no quote over the composer`);
  if (await evaluate(`${fiveColumns}.style.translate`)) await fail(`${where}: the swiped bubble did not come back`);
  await evaluate("document.querySelector('.composer .reply-quote button').click()");
  await until("!document.querySelector('.composer .reply-quote')", `${where}: × did not remove the quote`);
  // ^ The reply is sent with its quote on every screen.
  if (phone()) {
    await touch(await pressPoint(fiveColumns), 0, 700);
    await until("!!document.querySelector('.reply-menu')", `${where}: a long press opened no menu`);
    if (!(await evaluate(menuReply))) await fail(`${where}: the reply menu closed before Reply was tapped`);
  } else await replyFromMenu(await pressPoint(fiveColumns));
  await until(`${quote} === 'Five columns:'`, `${where}: Reply in the menu shows no quote`);
  if (await evaluate("document.activeElement?.matches('.composer-field')") !== true) await fail(`${where}: the reply did not focus the composer`);
  await shot("reply-quote", ".composer-send");
  await typeInto(".composer-field", "my answer");
  // ^ Only a bubble this send adds counts: the one of an earlier round quotes the same words.
  const sentReplies = "[...document.querySelectorAll('.log > .bubble[data-side=out]:not([data-pending])')].filter((node) => node.querySelector('.quote')?.textContent === 'Five columns:')";
  const sentBefore = JSON.stringify(await evaluate(`${sentReplies}.map((node) => node.dataset.key)`));
  await click(".composer-send");
  const sentReply = `${sentReplies}.findLast((node) => !${sentBefore}.includes(node.dataset.key))`;
  await until(`!!${sentReply} && ${sentReply}.querySelector('.prose').textContent === 'my answer' && !document.querySelector('.composer .reply-quote')`, `${where}: the reply was not sent with its quote`, 60);
  const stored = (await chatRows()).filter((row) => row.kind === "message" && row.author === "owner").at(-1);
  if (stored?.quoted !== "Five columns:" || stored.text !== "my answer") await fail(`${where}: the manager got ${JSON.stringify(stored)}`);
  // (f) A tap on the quote goes to the quoted message.
  await evaluate("document.querySelector('.log').scrollTop = 0");
  await pause(300);
  await evaluate(`${sentReply}.querySelector('.quote').click()`);
  await until(`(() => { const log = document.querySelector('.log').getBoundingClientRect(), box = ${fiveColumns}.getBoundingClientRect();
    return ${fiveColumns}.hasAttribute('data-found') && box.top < log.bottom && box.bottom > log.top; })()`, `${where}: the quote did not go to its message`);
  await toLatest(where);
}
await readAndReply("390 px");

// --- The task sheet of #2: how it does, one sentence of what changed and the one action; the
// report (Changes, Evidence, To check, Left open) in one Report fold, then History. No verdict,
// no Asked, no revision number. Land, then its item is live under To check, outside the folds.
await go(page(2));
await until("document.querySelectorAll('dialog.task-sheet[open] .task-fold').length >= 1", "the task sheet did not show the folded report");
let told = await story();
if (told.steps || told.summary !== "did: WORK a.txt HV" || !/^Ready, waiting for you to land it for [^:]+: checks 1 of 1 passed\. Land it to put it on main\.$/.test(told.next)) await fail(`the top of a ready task reads ${JSON.stringify(told)}`);
if (await sheetState() !== "Ready, waiting for you to land it") await fail(`a ready task's state reads ${await sheetState()}`);
// ^ The top row reads the title first, then the number, small and muted.
const sheetTitle = await evaluate("(() => { const title = document.querySelector('.task-sheet .bar .row-title'); return title && [title.firstElementChild.className, title.querySelector('.task-number').textContent]; })()");
if (!sheetTitle || sheetTitle[0] !== "row-label" || sheetTitle[1] !== "#2") await fail(`the sheet's top row reads ${JSON.stringify(sheetTitle)}`);
// ^ One fold level: the report, then History; the report's parts are headings in it.
const folds = await text(".task-sheet .scroll > details > .fold-head");
if (folds.join("|") !== "Report · 1 check passed · 1 left open|History") await fail(`the folds read ${JSON.stringify(folds)}`);
if (await count(".task-sheet details details")) await fail("a fold holds a fold");
const parts = await text(".task-sheet .task-part > .task-heading");
if (parts.join("|") !== "Changes|Evidence|To check · 1 after it lands|Left open") await fail(`the report's parts read ${JSON.stringify(parts)}`);
const sheetWords = await evaluate("document.querySelector('dialog.task-sheet').innerText");
if (/Asked|Human Verify|Decisions|Review|Pass r\d|Verified/.test(sheetWords)) await fail(`the sheet still shows the verdict or old sections: ${sheetWords}`);
if (await count(".task-sheet .label, .task-sheet .story-explain, .verify, .verify-note")) await fail("the sheet still has labels, Explain or the Verify panel");
if ((await text(".task-next button")).join() !== "Land,Take over,Try") await fail(`a ready task offers ${await text(".task-next button")}`);
const handHint = await evaluate("[...document.querySelectorAll('.task-next button')].find((node) => node.textContent === 'Take over')?.title ?? ''");
if (handHint !== "Stops the worker; you continue in its session" || await count(".task-hint")) await fail(`Take over does not say what it does in its title: ${handHint}`);
await resize([1280, 800, 1]);
await shot("task-sheet-take-over-1280", ".composer-send");
await resize([390, 844, 2]);
if (await evaluate("/\\br\\d+\\b/.test(document.querySelector('.task-sheet .bar').textContent + document.querySelector('.task-next').textContent)")) await fail("the sheet names a revision");
// ^ What shows above the fold of the sheet (text lines of the scroll whose box ends inside it),
// and whether the Flow facts and every action button are there: printed as the overhaul's measure.
const aboveFold = `(() => { const scroll = document.querySelector('dialog.task-sheet[open] .scroll'); const limit = scroll.getBoundingClientRect().bottom + 1;
  const tops = new Set(); const walker = document.createTreeWalker(scroll, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) { if (!node.textContent.trim() || !node.parentElement.checkVisibility()) continue;
    const range = document.createRange(); range.selectNodeContents(node);
    for (const rect of range.getClientRects()) if (rect.height && rect.bottom <= limit) tops.add(Math.round(rect.top / 4)); }
  const buttons = [...document.querySelectorAll('dialog.task-sheet[open] .task-next button')];
  return { lines: tops.size, flow: (document.querySelector('dialog.task-sheet[open] .task-flow')?.getBoundingClientRect().bottom ?? Infinity) <= limit,
    actions: buttons.length > 0 && buttons.every((button) => button.getBoundingClientRect().bottom <= limit) }; })()`;
const glance = await evaluate(aboveFold);
if (!glance.flow || !glance.actions) await fail(`the ready task's Flow facts or actions are below the fold at 390 px: ${JSON.stringify(glance)}`);
// ^ The glance in both schemes at 375, 390 and 1280 px (a resize across 900 px mounts the sheet again).
for (const scheme of ["dark", "light"]) {
  await inScheme(scheme);
  for (const next of [[375, 812, 2], [1280, 800, 1], [390, 844, 2]]) {
    await resize(next);
    await until("!!document.querySelector('dialog.task-sheet[open] .task-flow')", `the sheet lost its Flow facts at ${next[0]} px`);
    await shot(`task-sheet-glance-${scheme}`, ".composer-send");
  }
}
await inScheme("dark");
await shot("task-sheet", ".composer-send");
// ^ The summary shows its first paragraph; More shows the rest with the decisions and goes.
const decided = "/Decided: fake choice \\(fixture\\)\\./.test(document.querySelector('.story').textContent)";
if ((await evaluate(decided)) || (await count(".story .story-more")) !== 1) await fail("the sheet shows the decisions before More, or has no More");
await click(".story .story-more");
await until(`${decided} && !document.querySelector('.story .story-more')`, "More did not show the decisions");
// ^ The sheet as it was before the overhaul: the whole summary and its decisions, no Flow facts.
await evaluate("document.querySelector('dialog.task-sheet[open] .task-flow').style.display = 'none'");
const whole = await evaluate(aboveFold);
await evaluate("document.querySelector('dialog.task-sheet[open] .task-flow').style.display = ''");
console.log(`task sheet above the fold at 390x844: ${glance.lines} lines with the Flow facts and actions; ${whole.lines} lines as before, whole summary and no Flow`);
await shot("task-sheet-summary", ".composer-send");
// Evidence opens to the checks and what the worker showed.
await evaluate("document.querySelector('.task-fold > .fold-head').click()");
await until("/test ! -e broken\\.txt/.test(document.querySelector('.task-fold[open]')?.textContent ?? '') && /the fixture ran/.test(document.querySelector('.task-fold[open]').textContent)", "Evidence did not open to the checks and the worker's evidence");
await shot("task-sheet-open");
await evaluate("document.querySelector('.task-fold > .fold-head').click()");
await clickNext("Land");
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '') === 'done'", "#2 did not land", 300);
told = await story();
// ^ The core's sentence (docs/tasks.md "Status"); only the web knows the deploy fact.
if (told.next !== "Landed: on main; the last deploy does not have it yet.") await fail(`the top of a done task reads ${JSON.stringify(told)}`);
await until("[...document.querySelectorAll('.task-sheet .scroll > .task-section > .task-heading')].some((node) => node.textContent === 'To check · 1 open')", "#2's item is not open under To check, outside the folds", 60);
const offers = await evaluate("[...document.querySelectorAll('.task-sheet .scroll > .task-section .check-item .check-act')].map((node) => node.title).join()");
if (await count(".task-sheet .progress, .task-sheet .progress-now, .task-sheet .progress-steps")) await fail("a finished task still shows the progress view, its steps or a now line");
if (offers !== "Try it on a preview,Reply,Fail,Pass") await fail(`#2's item offers ${offers}`);
await evaluate("document.querySelector('.timeline > .fold-head').click()");
const history = await text(".timeline-what");
if ((await text(".timeline > .fold-head")).join() !== "History" || !history.includes("Landed") || history.some((line) => /Verified|without verification/.test(line))) await fail(`the history reads ${history}`);
await shot("task-landed");

// --- #4: a pending approval in the sentence and its buttons. Deny, then answer through the manager.
// ^ Opened from the chat: its close is history back, and from #2's sheet it would return to #2.
await go(chat);
await go(page(4));
await until("!!document.querySelector('.task-next button[aria-label=\"Approve discard #4\"]')", "#4 shows no pending approval");
// ^ The story says what an action is for; its row holds the buttons alone.
if ((await text(".task-next p")).length) await fail(`an action row repeats the story: ${await text(".task-next p")}`);
told = await story();
if (!/^Waiting for your approval for [^:]+: the manager asks: Approve discard #4\. It goes on when you approve or deny it\.$/.test(told.next) || await sheetState() !== "Waiting for your approval") await fail(`the top of a blocked task with an approval reads ${JSON.stringify(told)}`);
// ^ No Shared files outside a conflict.
if ((await openSections()).includes("Shared files")) await fail("a task that is not in conflict shows Shared files");
await shot("task-blocked", ".composer-send");
await buttonRoom("#4's sheet");
await resize([1280, 800, 1]);
await buttonRoom("#4's sheet");
await shot("task-blocked", ".composer-send");
await resize([390, 844, 2]);
await clickNext("Deny");
await until("!document.querySelector('.task-next button[aria-label=\"Approve discard #4\"]')", "the denied approval stayed", 300);
told = await story();
if (!/^Waiting for your answer for [^:]+: .+ Answer the manager; the worker goes on then\.$/.test(told.next)) await fail(`the story of a blocked task reads ${JSON.stringify(told)}`);
if (await count(".task-sheet textarea, .task-message, .task-steer")) await fail("the sheet still has a message field");
// ^ Opened from another place, with Home under it: its Answer goes to the project's chat, and the
// draft is there.
await closeSheet("#4", chat);
await typeInto(".composer-field", "see the logs");
await go("#/");
await until("!!document.querySelector('.home-rows > .row:not(.row-chat)')", "Home did not open");
await go(page(4));
await until("!!document.querySelector('.task-next button[aria-label^=\"Answer #4\"]')", "#4's sheet has no Answer");
await click(".task-next button[aria-label^=\"Answer #4\"]");
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "the sheet's Answer did not go to the chat");
await composerHas("see the logs #4 ", "the sheet's Answer did not add #4 after the draft");
await shot("answer-from-sheet", ".composer-send");
await typeInto(".composer-field", `see the logs #4\nRUN wsx task add 4 "WORK answered4.txt" -p ui --json`);
await click(".composer-send");
await go(page(4));
await until("/^Ready, waiting for you to land it for [^:]+: .+\\. Land it to put it on main\\.$/.test(document.querySelector('.story-next')?.textContent ?? '')", "#4 did not resume after the manager's add", 300);
await until("document.querySelectorAll('.task-talk .bubble').length === 1", "the manager's add is not in the sheet's messages", 60);
if ((await nextButtons()).includes("Answer")) await fail("a ready task's sheet offers Answer");

// --- Notices lie over the screen, one at a time. An approved action that did not run stays
// until dismissed, also when a later outcome comes; an outcome clears itself.
await go(chat);
await until("!!document.querySelector('.pill')", "the chat did not come back for the notices");
await typeInto(".composer-field", "RUN wsx task land 6 -p ui --json");
await click(".composer-send");
await openReservoir();
await until(`!!${rowOf("Approve land #6")}`, "the manager's request to land #6 has no row", 60);
const barTop = await evaluate("Math.round(document.querySelector('.manager .bar').getBoundingClientRect().top)");
await evaluate(`${rowOf("Approve land #6")}.querySelectorAll('.need-action')[1].click()`);
await until("[...document.querySelectorAll('.notice[data-tone=danger]')].some((node) => /^Land #6 did not run: failed: /.test(node.querySelector('.notice-text').textContent))", "the failed approval left no notice", 60);
await closeReservoir();
const overlay = await evaluate(`({ count: document.querySelectorAll('.notice').length, fixed: getComputedStyle(document.querySelector('.notices')).position,
  top: Math.round(document.querySelector('.manager .bar').getBoundingClientRect().top), dismiss: !!document.querySelector('.notice[data-tone=danger] button') })`);
if (overlay.count !== 1 || overlay.fixed !== "fixed" || overlay.top !== barTop || !overlay.dismiss) await fail(`the notice is not one overlay with Dismiss: ${JSON.stringify(overlay)} (top row was at ${barTop})`);
await shot("notice-failure", ".composer-send");
// A later outcome (Take over) does not hide the failure; it waits past the outcome's time.
await go(page(4));
await until(hasNext("Take over"), "#4 offers no Take over");
await clickNext("Take over");
await until(hasNext("Hand back"), "Take over did not end in Hand back", 60);
await pause(5000);
if (!(await evaluate("/Land #6 did not run/.test(document.querySelector('.notice')?.textContent ?? '')"))) await fail("a later outcome or the time hid the failure");
await closeSheet("#4");
await evaluate("document.querySelector('.notice[data-tone=danger] button').click()");
await until("!document.querySelector('.notice')", "Dismiss did not clear the failure");
// An action the owner approved in the chat leaves no failure notice, even while it runs:
// the manager runs it itself and replies with its result.
await go(chat);
await until("!!document.querySelector('.composer-field')", "the chat did not come back for the owner's word");
await typeInto(".composer-field", "land #6\nRUN wsx task land 6 --owner-said OWNER_SAID -p ui --json");
await click(".composer-send");
await until("/--owner-said \\d+ -p ui --json -> /.test(document.body.textContent)", "the manager did not run the land the owner asked for", 60);
await pause(4000);
if (await evaluate("!!document.querySelector('.notice[data-tone=danger]')")) await fail("an action the owner approved in the chat left a failure notice: " + (await evaluate("document.querySelector('.notice')?.textContent")));
// An outcome alone clears itself.
await go(page(4));
await until(hasNext("Hand back"), "#4 offers no Hand back");
await pause(3000);
await clickNext("Hand back");
await until("[...document.querySelectorAll('.notice')].some((node) => /^#4: /.test(node.textContent))", "Hand back of #4 showed no outcome", 60);
await until("!document.querySelector('.notice')", "the outcome did not clear itself", 80);
await until("/^Ready, waiting for you to land it for [^:]+: .+\\. Land it to put it on main\\.$/.test(document.querySelector('.story-next')?.textContent ?? '')", "#4 did not come back ready after Hand back", 300);
await closeSheet("#4 again");

// --- The manager chat attaches files as a session's chat does, and shows another project's
// manager as its own bubbles (docs/web-client-tasks.md "Project screen").
await go(chat);
await until("!!document.querySelector('.composer .icon-button[aria-label=\"Attach files\"]')", "the manager chat has no attach button");
// ^ The reply to plans is older than the newest 60 rows the chat keeps at its end: the reader
// goes up and shows earlier rows until it is there (docs/web-client-chat.md "Long chats").
const replyToPlans = "[...document.querySelectorAll('.log .bubble-tag')].some((node) => node.textContent === 'Reply to plans')";
for (let n = 0; n < 20 && !(await evaluate(replyToPlans)) && await count(".log-earlier"); n++) {
  await evaluate("(() => { const log = document.querySelector('.log'); log.scrollTop = 0; log.dispatchEvent(new Event('scroll')); })()");
  await pause(200);
  await click(".log-earlier");
  await pause(300);
}
const peers = await bubbles();
if (!peers.some(({ author }) => author === "plans") || !peers.some(({ tag }) => tag === "Reply to plans")) await fail(`the managers' messages read ${JSON.stringify(peers)}`);
if (await count(".jump")) await toLatest("the managers' messages");
// ^ Each speaker with a name has its own color on that name, never on its box: this manager's
// and the other manager's names and wsx's muted lines; the owner's bubble has no name and no color
// (docs/web-themes.md "Tokens").
const speakerColors = () => evaluate(`(() => { const color = (selector) => { const node = document.querySelector(selector);
  return node && getComputedStyle(node).color; };
  return [color('.log .bubble[data-speaker="manager"] .bubble-author'), color('.log .bubble[data-speaker="peer"] .bubble-author'), color('.log .status-line')]; })()`);
// ^ A box whose inline edges differ from its top edge has a side border, and a gradient, a shadow
// or a drawn pseudo-element can tint an edge; the owner's box has no colored border at all.
const sideBorders = () => evaluate(`[...document.querySelectorAll('.log .bubble-box')].filter((node) => { const style = getComputedStyle(node);
  return ['Start', 'End'].some((side) => style['borderInline' + side + 'Width'] !== style.borderTopWidth || style['borderInline' + side + 'Color'] !== style.borderTopColor)
    || style.backgroundImage !== 'none' || style.boxShadow !== 'none' || ['::before', '::after'].some((part) => getComputedStyle(node, part).content !== 'none')
    || (node.closest('.bubble').dataset.side === 'out' && style.borderInlineEndColor !== 'rgba(0, 0, 0, 0)'); }).length`);
if (!(await count('.log .bubble[data-speaker="owner"]')) || await count('.log .bubble[data-speaker="owner"] .bubble-author')) await fail("the chat has no owner bubble, or it shows a name");
for (const scheme of ["dark", "light"]) {
  await inScheme(scheme);
  const colors = await speakerColors();
  if (colors.some((color) => !color) || new Set(colors).size !== colors.length) await fail(`the speakers' colors in the ${scheme} scheme: ${JSON.stringify(colors)}`);
  const sides = await sideBorders();
  if (sides) await fail(`${sides} bubbles have a side border or a tinted edge in the ${scheme} scheme`);
  await evaluate("document.querySelector('.log .bubble[data-speaker=\"peer\"]').scrollIntoView({ block: 'center' })");
  await shot(`speakers-${scheme}`);
  // ^ The owner's bubbles beside the manager's, with no tint on their right edge.
  await evaluate("[...document.querySelectorAll('.log .bubble[data-side=\"out\"]')].at(-1).scrollIntoView({ block: 'center' })");
  await shot(`owner-${scheme}`);
}
await evaluate("delete document.documentElement.dataset.scheme; [...document.querySelectorAll('.log .bubble, .log .status-line')].at(-1).scrollIntoView({ block: 'end' })");
await pause(6000);
await evaluate(`(() => { const input = document.querySelector('.composer input[type=file]'); const files = new DataTransfer();
  files.items.add(new File(['notes for the manager'], 'notes.txt', { type: 'text/plain' })); input.files = files.files;
  input.dispatchEvent(new Event('change', { bubbles: true })); })()`);
await until("document.querySelectorAll('.composer .chips li').length === 1 && !document.querySelector('.composer .chips [data-uploading]')", "the file did not upload in the manager chat", 100);
// A pasted screenshot attaches the same way; a text paste is left to the browser.
if (!(await pasteInto(".composer-field", "pasted words"))) await fail("a text paste was cancelled in the manager chat");
if (await pasteInto(".composer-field", "image")) await fail("a pasted image was left to the manager's text box");
await until("document.querySelectorAll('.composer .chips li').length === 2 && !document.querySelector('.composer .chips [data-uploading]')", "the pasted image did not upload in the manager chat", 100);
// ^ A second screenshot, a second later so its name differs: the message has two images, which
// the image viewer goes between.
await pause(1100);
if (await pasteInto(".composer-field", "image-2")) await fail("a second pasted image was left to the manager's text box");
await until("document.querySelectorAll('.composer .chips li').length === 3 && !document.querySelector('.composer .chips [data-uploading]')", "the second pasted image did not upload in the manager chat", 100);
// ^ An image shows as its thumbnail and another file as its name; each has its remove (×).
await until("[...document.querySelectorAll('.composer .chips img')].filter((image) => image.naturalWidth).length === 2", "the composer's image chips show no thumbnail");
const chips = await evaluate(`[...document.querySelectorAll('.composer .chips li')].map((node) => ({ image: node.querySelector('img')?.naturalWidth ?? 0,
  name: node.querySelector('span')?.textContent ?? '', remove: node.querySelector('button.icon-button:has(.icon)')?.getAttribute('aria-label') ?? '',
  link: !!node.querySelector('.link-action') }))`);
if (chips.length !== 3 || !chips[0].name.startsWith("notes.txt") || chips[0].image || chips[0].remove !== "Remove notes.txt"
  || chips.slice(1).some((chip) => !chip.image || chip.name || !/^Remove pasted-\d{14}/.test(chip.remove)) || chips.some((chip) => chip.link)) await fail(`the composer's file chips read ${JSON.stringify(chips)}`);
await shot("manager-attach", ".composer-send");
await typeInto(".composer-field", "Here are the notes");
await click(".composer-send");
const sentWithFile = "[...document.querySelectorAll('.log .bubble[data-side=out]')].some((node) => /Here are the notes/.test(node.textContent) && [...node.querySelectorAll('.attachments a')].some((link) => link.textContent.startsWith('notes.txt')) && [...node.querySelectorAll('.attachments img')].some((image) => /^pasted-\\d{14}\\.(png|jpg)$/.test(image.alt)))";
await until(`${sentWithFile} || !!document.querySelector('.composer .error')`, "the sent message shows no attachment", 100);
if (!(await evaluate(sentWithFile))) await fail(`the message with a file was not sent: ${await evaluate("document.querySelector('.composer .error')?.textContent ?? ''")}; bubbles ${JSON.stringify(await bubbles())}`);
if (await count(".composer .chips li")) await fail("the sent file stayed in the composer");

const chatImages = ".log .bubble[data-side=out] .attachments a[data-view]";
await viewImages("chat-images", chatImages);

// A long text paste (over 2,000 characters or 30 lines) becomes a pasted-<time>.txt attachment, not
// text in the field; a short one stays the browser's. "Paste as text" puts it back in the field.
const longPaste = Array.from({ length: 60 }, (_, index) => `line ${index + 1} of a long pasted note, with words to fill it`).join("\n");
const shortPaste = "a short paste ".repeat(14);
if (!(await pasteInto(".composer-field", shortPaste))) await fail("a 200 character paste was cancelled in the manager chat");
await pause(300);
if (await count(".composer .chips li")) await fail("a 200 character paste became a file in the manager chat");
if (await pasteInto(".composer-field", longPaste)) await fail("a 3,000 character paste was left to the manager's text box");
await until("document.querySelectorAll('.composer .chips li').length === 1 && !document.querySelector('.composer .chips [data-uploading]')", "the long paste did not upload in the manager chat", 100);
const pastedChip = await evaluate(`({ field: document.querySelector('.composer-field').value, chip: document.querySelector('.composer .chips [data-pasted]')?.textContent ?? '',
  inline: [...document.querySelectorAll('.composer .chips button')].map((node) => node.textContent) })`);
if (pastedChip.field || pastedChip.chip !== "Pasted text · 60 lines · line 1 of a long pasted note, with words to fill it" || !pastedChip.inline.includes("Paste as text")) {
  await fail(`the pasted text chip reads ${JSON.stringify(pastedChip)}`);
}
await shot("manager-pasted-text", ".composer-send");
await click(".composer .chips .link-action");
await until("document.querySelector('.composer-field').value.length > 0 && !document.querySelector('.composer .chips li')", "Paste as text did not put the text back");
if ((await evaluate("document.querySelector('.composer-field').value")) !== longPaste) await fail("Paste as text changed the text");
await typeInto(".composer-field", "");
if (await pasteInto(".composer-field", longPaste)) await fail("the second long paste was left to the manager's text box");
await until("document.querySelectorAll('.composer .chips li').length === 1 && !document.querySelector('.composer .chips [data-uploading]')", "the second long paste did not upload", 100);
await click(".composer-send");
const pastedLink = "[...document.querySelectorAll('.log .bubble[data-side=out] .attachments a')].find((link) => /^pasted-\\d{14}\\.txt /.test(link.textContent))";
await until(`!!(${pastedLink}) || !!document.querySelector('.composer .error')`, "the sent pasted text shows no attachment", 100);
const sentText = await evaluate(`(${pastedLink})?.href ? fetch((${pastedLink}).href).then((response) => response.text()) : ''`);
if (sentText !== longPaste) await fail(`the attached file holds ${JSON.stringify(sentText.slice(0, 60))}, not the pasted text`);
if (await count(".composer .chips li")) await fail("the sent pasted text stayed in the composer");


// Reply on #7's item closes the sheet and quotes the item in the manager's composer
// (`> check <id>`) and leaves it open; the sent words close it as replied, are its note, and the
// manager gets them (docs/tasks.md "To check").
const firstItem = "document.querySelector('.check-list .check-item')";
async function replyQuotesAndCloses(back) {
  const replyButton = "[...document.querySelectorAll('.check-list .check-item')].find((node) => node.querySelector('.task-chip').textContent.trim() === '#7').querySelector('.check-acts .check-act[title=\"Reply\"]')";
  await evaluate(`${replyButton}.click()`);
  await until("!document.querySelector('dialog.task-sheet[open]') && !!document.querySelector('.composer .reply-quote')", "Reply did not close the sheet and quote the item in the manager's composer");
  if (await evaluate("document.activeElement !== document.querySelector('.composer-field')")) await fail("Reply did not focus the composer");
  const quotedState = await evaluate(`fetch('api/tasks/${project}/checks').then((r) => r.json()).then((list) => list.items.find((item) => item.task === 7)?.state)`);
  if (quotedState !== "open") await fail(`Reply left #7's item ${quotedState} before the words were sent`);
  await typeInto(".composer-field", "the text is cut off");
  await click(".composer-send");
  await until(`fetch('api/tasks/${project}/checks').then((r) => r.json()).then((list) => list.items.find((item) => item.task === 7)).then((item) => item?.state === 'replied' && item.note === 'the text is cut off')`, "the reply did not close #7's item as replied with its note", 300);
  await go(back);
  await until("document.querySelectorAll('dialog.task-sheet[open] .check-item').length === 2", "the replied item stayed in the list after the reply");
}

// --- To check: the reservoir's row opens the list; Fail closes the item and quotes it for the
// owner's note; Pass in a To check row changes its bubble in place; Skip closes the last without a verdict.
await go(chat);
await until("!!document.querySelector('.pill')", "the chat did not come back for To check");
await openReservoir("check");
await until(`document.querySelectorAll('.reservoir-list[aria-label="To check"] .need').length === 3`, "the reservoir has no row for each To check item", 60);
// ^ A tap on a row opens the sheet at its item, scrolled to it and marked. The list and #8's
// capture are in the sheet before it moves, so it rises once and stays.
await oneMotion("To check on a phone", async () => {
  await evaluate(`${rowOf("#2")}.querySelector('.row-open').click()`);
  await until(`/^#\\/p\\/${project}\\/checks\\/c\\d+$/.test(location.hash) && document.querySelectorAll('dialog.task-sheet[open] .check-item').length === 3`, "the To check list did not open at the item");
});
// ^ The scroll and the mark come a frame after the sheet shows: wait for them. An item with a
// capture can be taller than the sheet, so it is seen when it covers part of the view.
const seenItem = `(() => { const node = document.querySelector('dialog.task-sheet[open] .check-item[data-check=' + JSON.stringify(location.hash.split('/').at(-1)) + ']'), scroll = document.querySelector('dialog.task-sheet[open] .scroll').getBoundingClientRect(), box = node?.getBoundingClientRect();
  return { task: node?.querySelector('.task-chip')?.textContent.trim(), seen: !!box && box.top < scroll.bottom - 20 && box.bottom > scroll.top + 20 && node.hasAttribute('data-found') }; })()`;
await until(`${seenItem}.seen`, "the sheet did not scroll to the item a To check row opened");
const atItem = await evaluate(seenItem);
if (!/^#2/.test(atItem.task ?? "")) await fail(`a tap on #2's To check row opened the sheet at ${JSON.stringify(atItem)}`);
// ^ The opened To check list uses the reservoir's small size.
const checkSize = await evaluate("getComputedStyle(document.querySelector('dialog.task-sheet[open] .check-item .check-task')).fontSize");
if (checkSize !== closedSize) await fail(`the To check list's words are ${checkSize}, not ${closedSize}`);
await closeSheet("the To check list", chat);
// ^ On a slow network the list and then its captures come late: the sheet still rises once,
// since each capture's box has its size before the image arrives.
await send("Network.setCacheDisabled", { cacheDisabled: true });
await send("Network.emulateNetworkConditions", { offline: false, latency: 1500, downloadThroughput: -1, uploadThroughput: -1 });
await oneMotion("To check on a slow network", () => go(`#/p/${project}/checks`));
await until("[...document.querySelectorAll('.check-list .check-capture img')].every((image) => image.complete && image.naturalWidth)", "the captures did not load on a slow network", 100);
await pause(500);
const slowRest = await evaluate("document.querySelector('dialog.task-sheet').getBoundingClientRect().top");
if (Math.abs(slowRest - (await evaluate("window.sheetFrames.at(-1)[0]"))) > 0.5) await fail(`the To check sheet moved when its captures came: ${Math.round((await evaluate("window.sheetFrames.at(-1)[0]")))} to ${Math.round(slowRest)}`);
await send("Network.emulateNetworkConditions", { offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: -1 });
await send("Network.setCacheDisabled", { cacheDisabled: false });
await closeSheet("the To check list on a slow network", chat);
await resize([1280, 800, 1]);
await oneMotion("To check on a wide screen", () => go(`#/p/${project}/checks`));
// ^ Wide, Try, Fail, Skip and Pass are a column right of each item's words, level with its title line.
const wideRoom = await evaluate(`[...document.querySelectorAll('.check-list .check-item')].map((node) => { const box = (part) => node.querySelector(part).getBoundingClientRect();
  return [Math.round(box('.check-acts').left - box('.check-words').right), Math.round(box('.check-acts').top - box('.check-task').top), Math.round(box('.check-acts').right)]; })`);
if (!wideRoom.every(([gap, top, right]) => gap >= 8 && Math.abs(top) <= 12 && Math.abs(right - wideRoom[0][2]) <= 1)) await fail(`the wide To check buttons are not in one column: ${JSON.stringify(wideRoom)}`);
await shot("to-check", ".composer-send");
await viewImages("to-check", ".check-list .check-item:first-child .check-capture", 1, true);
for (const href of await evaluate("[...document.querySelectorAll('.check-list .check-capture[data-video]')].map((link) => link.dataset.download)")) await savedCopy("the To check video", href);
await closeSheet("the wide To check list", chat);
await resize([390, 844, 2]);
await oneMotion("To check opened again on a phone", () => go(`#/p/${project}/checks`));
const listed = await evaluate("[...document.querySelectorAll('.check-list .check-item')].map((node) => [node.querySelector('.task-chip').textContent.trim(), node.querySelector('.check-what').textContent, node.querySelector('.check-pass').textContent])");
if (JSON.stringify(listed) !== JSON.stringify([["#7", "open the file", "it has text"], ["#8", "open the file", "it has text"], ["#2", "open the file", "it has text"]])) await fail(`the To check list reads ${JSON.stringify(listed)}`);
if (await sheetState() !== "3 open") await fail(`the list's top row reads ${await sheetState()}`);
// ^ Each item says what its task did and where to look; #7's and #8's show their captures. The
// sheet is on the bottom and as high as its rows with no empty band under them, at most 85%: with
// the two captures the rows are higher, so it stops at 85% and its list scrolls.
const context = await evaluate("[...document.querySelectorAll('.check-list .check-item')].map((node) => [(node.querySelector('.check-task')?.textContent ?? '').replace(node.querySelector('.task-chip').textContent, '').trim(), [...node.querySelectorAll('.check-facts > *')].map((line) => line.textContent).join('|')])");
if (!context.every(([did]) => did.startsWith("did: WORK")) || context.slice(0, 2).some(([, facts]) => facts !== "Check|open the file|Look at|/#/x|Passes if|it has text") || context[2][1] !== "Check|open the file|Look at|a real device|Passes if|it has text") await fail(`the items' context reads ${JSON.stringify(context)}`);
await until("[...document.querySelectorAll('.check-list .check-capture img')].map((image) => image.naturalWidth).join() === '480,480'", "#7's and #8's captures did not load");
if (await count(".check-list .check-capture") !== 2) await fail("an item without a capture shows one");
// ^ A capture's link, and so its focus ring, is as wide as its image, not the item's width.
const captureBoxes = await evaluate("[...document.querySelectorAll('.check-list .check-capture')].map((link) => [link.getBoundingClientRect().width, link.querySelector('img').getBoundingClientRect().width].map(Math.round))");
if (captureBoxes.some(([link, image]) => Math.abs(link - image) > 1)) await fail(`a capture's link is not as wide as its image: ${JSON.stringify(captureBoxes)}`);
await until("document.querySelector('dialog.task-sheet')?.getAnimations().length === 0", "the sheet kept moving");
const fitted = await evaluate(`(() => { const sheet = document.querySelector('dialog.task-sheet').getBoundingClientRect(); const last = [...document.querySelectorAll('.check-list .check-item')].pop().getBoundingClientRect(); const list = document.querySelector('dialog.task-sheet .scroll');
  return [Math.round(innerHeight - sheet.bottom), Math.round(sheet.bottom - last.bottom), Math.round(sheet.height / innerHeight * 100), list.scrollHeight > list.clientHeight + 1]; })()`);
const [low, band, high, scrolls] = fitted;
if (Math.abs(low) > 1 || (high < 84 ? scrolls || band < 0 || band > 48 : high > 85 || !scrolls)) await fail(`the To check sheet does not fit its rows: ${JSON.stringify(fitted)}`);
// ^ On a phone the item's words use the list's width: the chip starts the title line, the labels
// are muted, and Try, Fail, Skip and Pass are a row under the words at the same right edge on every
// item, also under #2's long pass line. All passed is in the top bar.
await evaluate(`document.querySelectorAll('.check-list .check-item')[2].querySelector('.check-pass').append(' and the words of the page'.repeat(4))`);
const room = await evaluate(`(() => { const list = document.querySelector('.check-list').getBoundingClientRect(); const box = (node) => node.getBoundingClientRect();
  const items = [...document.querySelectorAll('.check-list .check-item')]; const chip = items[0].querySelector('.task-chip');
  const acts = items.map((node) => box(node.querySelector('.check-acts')));
  const label = items[0].querySelector('.check-facts dt'), value = items[0].querySelector('.check-facts dd');
  return { inline: chip.closest('p') === items[0].querySelector('.check-task'),
    full: items.every((node) => box(node.querySelector('.check-words')).width / list.width > 0.85),
    under: items.every((node, index) => acts[index].top >= box(node.querySelector('.check-words')).bottom),
    edge: acts.every((place) => Math.abs(place.right - acts[0].right) <= 1),
    muted: getComputedStyle(label).color !== getComputedStyle(value).color,
    stacked: box(label).bottom <= box(value).top + 1 && box(value).width > box(items[0].querySelector('.check-words')).width * 0.95,
    all: !!document.querySelector('dialog.task-sheet header.bar .check-all'), intro: !!document.querySelector('.check-intro') }; })()`);
if (!room.inline || !room.full || !room.under || !room.edge || !room.muted || !room.stacked || !room.all || room.intro) await fail(`the To check items are not laid out on a phone: ${JSON.stringify(room)}`);
// ^ #8's capture is a video (docs/web-client-tasks.md "To check"): its poster with a play mark,
// served with byte ranges for iPhone Safari, and the viewer plays it with controls, no autoplay.
const videoLink = ".check-list .check-item:nth-child(2) .check-capture";
const clip = await evaluate(`(async () => { const link = document.querySelector('${videoLink}');
  const part = await fetch(link.href, { headers: { Range: 'bytes=0-99' } });
  return { video: link.hasAttribute('data-video'), poster: link.querySelector('img').src.endsWith('/poster'), play: !!link.querySelector('.check-play svg'),
    status: part.status, type: part.headers.get('content-type'), range: part.headers.get('content-range'), accept: part.headers.get('accept-ranges'),
    length: (await part.arrayBuffer()).byteLength }; })()`);
if (!clip.video || !clip.poster || !clip.play || clip.status !== 206 || clip.type !== "video/mp4" || !/^bytes 0-99\/\d+$/.test(clip.range || "")
  || clip.accept !== "bytes" || clip.length !== 100) await fail(`#8's video capture reads ${JSON.stringify(clip)}`);
await evaluate(`document.querySelector('${videoLink}').scrollIntoView({ block: 'nearest' })`);
const playAt = await evaluate(`(() => { const box = document.querySelector('${videoLink}').getBoundingClientRect(); return [Math.round(box.left + box.width / 2), Math.round(box.top + box.height / 2)]; })()`);
for (const type of ["mousePressed", "mouseReleased"]) await send("Input.dispatchMouseEvent", { type, x: playAt[0], y: playAt[1], button: "left", clickCount: 1 });
const playing = "document.querySelector('dialog.viewer[open] .viewer-stage video')";
await until(`${playing}?.readyState >= 1 && !${playing}.hasAttribute('data-loading')`, "#8's video did not load its metadata in the viewer");
const player = await evaluate(`(() => { const video = ${playing}; const box = video.getBoundingClientRect(); const bar = document.querySelector('.viewer-bar').getBoundingClientRect(); return { clear: box.bottom <= bar.top + 1 || box.top >= bar.bottom - 1, controls: video.controls, inline: video.playsInline, preload: video.getAttribute('preload'), autoplay: video.autoplay,
  paused: video.paused, poster: video.poster.endsWith('/poster'), size: [video.videoWidth, video.videoHeight], zoom: !!document.querySelector('.viewer-zoom button') }; })()`);
if (!player.clear || !player.controls || !player.inline || player.preload !== "metadata" || player.autoplay || !player.paused || !player.poster
  || player.size.join() !== "160,90" || player.zoom) await fail(`#8's video in the viewer reads ${JSON.stringify(player)}`);
await until("document.querySelector('dialog.viewer').getAnimations({ subtree: true }).length === 0", "#8's video viewer kept moving");
await capture("to-check-video");
await key("Escape", 27);
await until("!document.querySelector('dialog.viewer') && !!document.querySelector('dialog.task-sheet[open]')", "Escape did not close the video viewer alone");
await shot("to-check", ".composer-send");
await viewImages("to-check", ".check-list .check-item:first-child .check-capture", 1, true);
// ^ Try starts the preview of the item's task and becomes a link to it (docs/tasks.md "Preview").
await evaluate(`${firstItem}.querySelector('.check-act[title="Try it on a preview"]').click()`);
await until(`!!${firstItem}.querySelector('a.check-act[href^="https://host.example.ts.net:"]')`, "Try did not become a preview link", 300);
await shot("to-check-try", ".composer-send");
await replyQuotesAndCloses(`#/p/${project}/checks`);
await closeSheet("the To check list after a reply", chat);
// ^ Each To check item is also a bubble in the chat at the time it was added: its words, its
// capture (the image viewer opens it) and Try, Reply, Fail and Pass; a decided one shows its result
// (docs/web-client-tasks.md "Project screen").
const checkBubbles = "[...document.querySelectorAll('.log .check-bubble')]";
// ^ The chat renders its newest rows; these items are older, so show the earlier ones. The wide
// layout is another page tree, so after a resize past 900 px they are shown again: the wait taps
// "Show earlier" until the bubble is there, as the log of the other tree comes in late.
const showEarlier = async () => { for (let n = 0; n < 10 && (await count(".log-earlier")); n++) await click(".log-earlier"); };
await showEarlier();
await until(`${checkBubbles}.length >= 3`, "the chat has no To check bubbles", 60);
const bubbleOf = (task) => `${checkBubbles}.find((node) => node.querySelector('.check-task .task-chip')?.textContent.trim() === '#${task}')`;
await until(`(${bubbleOf(8)}?.querySelector('.check-capture img')?.naturalWidth ?? 0) > 0 && /^Replied: the text is cut off$/.test(${bubbleOf(7)}?.querySelector('.check-state')?.textContent ?? '')`, "the To check bubbles did not load their capture and result", 100);
const inChat = await evaluate(`(() => { const open = ${bubbleOf(8)}, failed = ${bubbleOf(7)};
  return { tag: open?.querySelector('.bubble-tag')?.textContent, author: open?.querySelector('.bubble-author')?.textContent, image: open?.querySelector('.check-capture img')?.naturalWidth ?? 0,
    acts: [...(open?.querySelectorAll('.check-acts .check-act') ?? [])].map((node) => node.title), failed: failed?.querySelector('.check-state')?.textContent ?? '',
    failedActs: failed?.querySelectorAll('.check-acts').length ?? -1, unread: open?.hasAttribute('data-in') }; })()`);
if (inChat.tag !== "To check" || inChat.author !== "Atlas" || inChat.image !== 480 || inChat.acts.filter((act) => act !== "Try it on a preview").join() !== "Reply,Fail,Pass"
  || inChat.failed !== "Replied: the text is cut off" || inChat.failedActs !== 0 || !inChat.unread) await fail(`the To check bubbles read ${JSON.stringify(inChat)}`);
await evaluate(`${bubbleOf(8)}.scrollIntoView({ block: 'center' })`);
await pause(300);
await shot("to-check-bubble", ".composer-send");
// ^ Each bubble is its own set; #7's capture is an image (#8's is a video).
const bubbleKey = await evaluate(`${bubbleOf(7)}.dataset.key`);
await viewImages("to-check-bubble", `.log .check-bubble[data-key=${JSON.stringify(bubbleKey)}] .check-capture`, 1, true);
await resize([1280, 800, 1]);
await until(`(() => { if (!${bubbleOf(8)}) document.querySelector('.log-earlier')?.click(); return !!${bubbleOf(8)}; })()`, "the wide chat has no To check bubble");
await evaluate(`${bubbleOf(8)}.scrollIntoView({ block: 'center' })`);
await pause(300);
await shot("to-check-bubble-wide", ".composer-send");
await resize([390, 844, 2]);
await until(`(() => { if (!${bubbleOf(8)}) document.querySelector('.log-earlier')?.click(); return !!${bubbleOf(8)}; })()`, "the chat has no To check bubble after the wide layout");
// ^ A decision changes the bubble in place: it stays at the time the item was added, so it
// neither moves to the end nor counts as new (docs/web-client-tasks.md "Project screen").
const chatPlace = `(() => { const log = document.querySelector('.log'); return { keys: [...log.children].map((node) => node.dataset.key ?? '').join(), at: ${bubbleOf(8)}.dataset.at,
  jump: document.querySelector('.jump')?.textContent ?? '', fresh: ${bubbleOf(8)}.hasAttribute('data-fresh') }; })()`;
const beforePass = await evaluate(chatPlace);
// ^ Pass in the reservoir's To check row: the row leaves, nothing opens, and the bubble settles.
await openReservoir("check");
await clearRow("#8", "Pass");
if (await count(".composer .reply-quote")) await fail("Pass in a To check row quoted the item in the composer");
await closeReservoir();
await until(`(${bubbleOf(8)}?.querySelector('.check-state')?.textContent ?? '') === 'Passed' && !${bubbleOf(8)}.querySelector('.check-acts')`, "Pass in the To check row did not settle the bubble", 60);
await pause(500);
const afterPass = await evaluate(chatPlace);
// ^ Rows that came meanwhile (an event line) go after it; with none, the "N new" count is unchanged.
const samePlace = afterPass.keys.startsWith(beforePass.keys) && afterPass.at === beforePass.at && !afterPass.fresh
  && (afterPass.keys !== beforePass.keys || afterPass.jump === beforePass.jump);
if (!samePlace) await fail(`a passed item's bubble moved or counts as new: ${JSON.stringify({ beforePass, afterPass })}`);
await until("document.querySelector('.pill[data-kind=check]')?.getAttribute('aria-label') === '1 to check'", "Pass in the To check row did not drop the To check count", 60);
await go(`#/p/${project}/checks`);
await until("document.querySelectorAll('dialog.task-sheet[open] .check-item').length === 1", "the item passed in the chat stayed in the list", 60);
// ^ Fail closes an item at once and opens nothing (docs/tasks.md "To check"). Its bubble shows
// "Failed" at once, and the count of the reservoir drops (the list is closed first: Fail is
// pressed on #2's bubble, which has no other way to show a result).
await closeSheet("the To check list", chat);
await showEarlier();
await until(`!!${bubbleOf(2)}?.querySelector('.check-act[title="Fail"]')`, "#2's bubble has no Fail");
if (await evaluate(`${bubbleOf(2)}.querySelector('.check-act[title="Pass"]').getAttribute('aria-label')`) !== "Pass: open the file") await fail("Pass has no clear label");
await evaluate(`${bubbleOf(2)}.querySelector('.check-act[title="Fail"]').click()`);
await until(`(${bubbleOf(2)}?.querySelector('.check-state')?.textContent ?? '') === 'Failed' && !${bubbleOf(2)}.querySelector('.check-acts')`, "Fail in the bubble did not show Failed", 60);
if (await count(".composer .reply-quote")) await fail("Fail on a bubble quoted the item in the composer");
await go(`#/p/${project}/checks`);
await until("(document.querySelector('dialog.task-sheet .empty')?.textContent ?? '') === 'Nothing to check.'", "the failed item stayed in the list", 60);
await shot("to-check-done");
await closeSheet("the To check list", chat);
await until("!document.querySelector('.pill[data-kind=check]')", "the pills still count items to check", 60);
// ^ With #4 and the fix task landed (a pending approval denied) only Running is left: the one
// pill, and its list's first row whole at the reservoir's top.
const onlyRunning = "document.querySelectorAll('.pill').length > 0 && [...document.querySelectorAll('.pill')].every((node) => ['running', 'routines'].includes(node.dataset.kind))";
// ^ A Land that checks again clears the row, then brings it back: two seconds in a row, not a moment.
let runningSince = 0;
for (let attempt = 0; attempt < 600 && runningSince < 20; attempt++) {
  runningSince = (await evaluate(onlyRunning)) ? runningSince + 1 : 0;
  if (attempt % 40 === 0) {
    await evaluate("document.querySelector('.pill[data-kind=needs]:not([aria-pressed=true])')?.click()");
    await pause(300);
    await evaluate("document.querySelector('.reservoir-scroll .need:not([data-leaving]) :is(.need-action[title=Land], .need-action[title=Deny])')?.click()");
  }
  await pause(100);
}
if (!(await evaluate(onlyRunning))) await fail(`Running is not the only task pill: ${await pillWords()}`);
await openReservoir("running");
await firstRowInside("with only running tasks");
await shot("reservoir-running-first", ".composer-send");
await closeReservoir();

// --- Home marks a project with a message its chat has that this device has not read: an accent
// mark and the message's first line; opening the chat clears it (docs/web-client-tasks.md "Home").
await go("#/");
await until("!!document.querySelector('.home-row')", "home did not open for the new message");
const newsRow = `document.querySelector('.home-rows > .row:not(.row-chat)[data-key="${project}"]')`;
if (await evaluate(`!!${newsRow}.querySelector('.news-mark')`)) await fail("a project whose chat was read is marked new");
await evaluate(`fetch('api/tasks/${project}/say', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text: 'STATUS for home' }) })`);
await until(`!!${newsRow}?.querySelector('.news-mark') && /^open: /.test(${newsRow}.querySelector('.row-context[data-news]')?.textContent.trim() ?? '')`, "the new reply did not mark the project's home row", 300);
const marked = await evaluate(`(() => { const mark = getComputedStyle(${newsRow}.querySelector('.news-mark')).backgroundColor;
  const probe = document.createElement('span'); probe.style.color = 'var(--color-accent)'; document.body.append(probe); const accent = getComputedStyle(probe).color; probe.remove();
  return { mark, accent, others: [...document.querySelectorAll('.home-rows > .row:not(.row-chat):not([data-news]) .news-mark')].length }; })()`);
if (marked.mark !== marked.accent || marked.others) await fail(`the new message's mark is not the accent alone: ${JSON.stringify(marked)}`);
await shot("home-news");
await go(chat);
await until("!!document.querySelector('.log .bubble') && !document.querySelector('.jump')", "the chat did not open at its end for the new reply", 100);
await pause(500);
await go("#/");
await until(`!!${newsRow} && !${newsRow}.querySelector('.news-mark') && !${newsRow}.querySelector('.row-context[data-news]')`, "opening the chat did not clear the home row's mark", 50);
await shot("home-read");

// --- A deep link to a task opens the chat with the sheet over it; Back closes the sheet, then
// the chat is the first entry, so the next Back leaves the page.
// ^ Through a blank page: a navigation that changes only the hash stays in this document.
await send("Page.navigate", { url: "about:blank" });
await until("location.href === 'about:blank'", "the blank page did not open");
await send("Page.navigate", { url: `${siteUrl}#/p/${project}/t/3` });
await until("!!document.querySelector('dialog.task-sheet[open] .story') && !!document.querySelector('.composer-field')", "a deep link did not open the chat with the sheet");
if (await evaluate("history.state?.depth") !== 1) await fail(`a deep link sits at depth ${await evaluate("JSON.stringify(history.state)")}`);
// One load reads the tasks once: no second `api/tasks`, and no read of the one project.
await pause(1000);
const taskReads = await evaluate(`performance.getEntriesByType('resource').map((entry) => new URL(entry.name).pathname).filter((path) => /\\/api\\/tasks(\\/\\d+)?$/.test(path))`);
if (taskReads.length !== 1 || !taskReads[0].endsWith("/api/tasks")) await fail(`a load read the tasks as ${JSON.stringify(taskReads)}`);
await evaluate("history.back()");
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Back after a deep link did not close the sheet");
// ^ The chat is the first entry: its Back replaces it with Home, no entry is added, and
// history.back() then leaves the page, as two history.back() calls from the deep link do.
// ^ A history.back() that leaves the page runs after the evaluate returns: a navigation during it
// closes the evaluate's context.
const deepLength = await evaluate("history.length");
await click(".manager > .bar > a.icon-button");
await until("location.hash === '#/' && !!document.querySelector('.home-row')", "Back on the first entry did not go to Home");
const deepHome = await evaluate("({ length: history.length, depth: history.state?.depth })");
if (deepHome.length !== deepLength || deepHome.depth !== 0) await fail(`Back on the first entry pushed: ${JSON.stringify(deepHome)}, was length ${deepLength}`);
await evaluate("setTimeout(() => history.back())");
await until("location.href === 'about:blank'", "history.back() after Back on the first entry did not leave the page");
await send("Page.navigate", { url: `${siteUrl}#/p/${project}/t/3` });
await until("!!document.querySelector('dialog.task-sheet[open] .story') && !!document.querySelector('.composer-field')", "the deep link did not open again");
// ^ A done task's top row holds its title and stage word, not its model too.
if (await count("dialog.task-sheet .bar .bar-model")) await fail("a done task's top row shows its model");
await evaluate("history.back()");
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "history.back() after the deep link did not close the sheet");
await evaluate("setTimeout(() => history.back())");
await until("location.href === 'about:blank'", "a second history.back() after the deep link did not leave the page");
await send("Page.navigate", { url: `${siteUrl}#/` });
await until("!!document.querySelector('.home-row')", "the page did not open again on Home");

// --- Back in a top row or a sheet and the system's swipe back (history.back()) lead to the same
// place with the same scroll (docs/web-client.md "Screens"): each screen opens the normal way
// twice, left once by its Back and once by history.back(). A list's scroll comes back too.
// ^ Short enough that Home's seven rows scroll.
await resize([390, 400, 2]);
const where = () => evaluate("JSON.stringify({ hash: location.hash, length: history.length, depth: history.state?.depth, scroll: [...document.querySelectorAll('#main > .page > .scroll, #main .log')].map((node) => Math.round(node.scrollTop)) })");
const topBack = "#main > .page > .bar > a.icon-button";
const sheetBack = "dialog.task-sheet[open] .bar button[aria-label='Back to the chat']";
async function sameBack(name, open, back) {
  await pause(400);
  const before = await where();
  // ^ The screen left stays as a forward entry, so history.length is compared between the two
  // ways only, and the place (hash, depth, scroll) also with the start.
  const place = (json) => JSON.stringify({ ...JSON.parse(json), length: undefined });
  let byBack;
  for (const how of ["Back", "history.back()"]) {
    await open();
    await pause(400);
    if (how === "Back") await click(back);
    else await evaluate("history.back()");
    await until(`location.hash === ${JSON.stringify(JSON.parse(before).hash)} && !document.querySelector('dialog.task-sheet')`, `${name}: ${how} did not return`);
    await pause(400);
    const after = await where();
    if (place(after) !== place(before)) await fail(`${name}: after ${how} the page is ${after}, was ${before}`);
    if (byBack && after !== byBack) await fail(`${name}: history.back() leads to ${after}, Back to ${byBack}`);
    byBack ??= after;
  }
}
const opens = (selector, ready, what) => async () => {
  await click(selector);
  await until(ready, `${what} did not open`);
};
const fromMenu = (selector, ready, what) => async () => {
  await click(".manager > .bar .more-button");
  await opens(`.more-menu ${selector}`, ready, what)();
};
await evaluate("document.querySelector('#main > .page > .scroll').scrollTop = 120");
if (!(JSON.parse(await where()).scroll[0] > 0)) await fail("Home does not scroll at 390 × 400: the scroll place is not checked");
await sameBack("a project's chat", opens(`a.home-row[href="${chat}"]`, "!!document.querySelector('.composer-field')", "the chat"), topBack);
await go(chat);
await until("!!document.querySelector('.composer-field')", "the chat did not open");
const sheetOpen = (part) => `!!document.querySelector('dialog.task-sheet[open]') && location.hash.startsWith(${JSON.stringify(chat + part)})`;
await sameBack("the overview", opens(`.manager > .bar a[href="${chat}/page"]`, sheetOpen("/page"), "the overview"), sheetBack);
await sameBack("the task sheet", opens(`.log a.task-chip[href^="${chat}/t/"]`, sheetOpen("/t/"), "the task sheet"), sheetBack);
await sameBack("the usage sheet", fromMenu(`a[href="${chat}/usage"]`, sheetOpen("/usage"), "the usage sheet"), sheetBack);
await sameBack("the project's sessions", fromMenu(`a[href="${chat}/sessions"]`, "!!document.querySelector('#main .worktree')", "the project's sessions"), topBack);
await go("#/sessions");
await until("[...document.querySelectorAll('a[href*=\"#/s/\"]')].some((node) => node.textContent.includes('old chat'))", "the inbox does not list old chat");
const oldChat = await evaluate("[...document.querySelectorAll('a[href*=\"#/s/\"]')].find((node) => node.textContent.includes('old chat')).getAttribute('href')");
await sameBack("a session", opens(`a[href="${oldChat}"]`, "!!document.querySelector('.session .bar')", "the session"), topBack);
// ^ Tabs and Settings sections replace the entry: Back leaves the section, not the taps.
await go(chat);
await until("!!document.querySelector('.composer-field')", "the chat did not open");
await go("#/");
await until("!!document.querySelector('.tabbar')", "Home has no tab bar");
const tabbed = await evaluate("({ length: history.length, depth: history.state?.depth })");
for (const href of ["#/sessions", "#/settings", "#/"]) {
  await click(`.tabbar a[href="${href}"]`);
  await until(`location.hash === ${JSON.stringify(href)}`, `the ${href} tab did not open`);
}
await click('.tabbar a[href="#/settings"]');
await until("!!document.querySelector('.subtabs')", "Settings has no sections");
await click('.subtabs a[href="#/settings/ui"]');
await until("location.hash === '#/settings/ui'", "the UI section did not open");
const afterTabs = await evaluate("({ length: history.length, depth: history.state?.depth })");
if (JSON.stringify(afterTabs) !== JSON.stringify(tabbed)) await fail(`tab and section taps added entries: ${JSON.stringify(afterTabs)}, was ${JSON.stringify(tabbed)}`);
await evaluate("history.back()");
await until(`location.hash === ${JSON.stringify(chat)}`, "history.back() after the tabs did not leave the section");

// --- The page never zooms (docs/web-client.md "Zoom"): a two-finger pinch on the chat and a tap
// on the composer leave the visual viewport at scale 1.
await resize([390, 844, 2]);
await until("!!document.querySelector('.composer-field')", "the chat did not come back");
await pinch({ x: 195, y: 400 });
await pause(300);
if (await evaluate("visualViewport.scale") !== 1) await fail(`a pinch zoomed the chat to ${await evaluate("visualViewport.scale")}`);
const field = await evaluate("(() => { const box = document.querySelector('.composer-field').getBoundingClientRect(); return [Math.round(box.left + box.width / 2), Math.round(box.top + box.height / 2)]; })()");
await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: field[0], y: field[1] }] });
await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
await until("document.activeElement === document.querySelector('.composer-field')", "a tap did not focus the composer");
await pause(300);
const focusZoom = await evaluate("({ scale: visualViewport.scale, font: getComputedStyle(document.querySelector('.composer-field')).fontSize })");
if (focusZoom.scale !== 1 || parseFloat(focusZoom.font) < 16) await fail(`the focused composer reads ${JSON.stringify(focusZoom)}`);
await evaluate("document.activeElement.blur()");
// ^ A double tap on the title does not zoom either.
const title = await evaluate("(() => { const box = document.querySelector('.manager > .bar .bar-label').getBoundingClientRect(); return [Math.round(box.left + box.width / 2), Math.round(box.top + box.height / 2)]; })()");
for (let tap = 0; tap < 2; tap++) {
  await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: title[0], y: title[1] }] });
  await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  await pause(80);
}
await pause(400);
const doubleTap = await evaluate("({ scale: visualViewport.scale, hash: location.hash })");
if (doubleTap.scale !== 1 || doubleTap.hash !== chat) await fail(`a double tap on the title zoomed or moved the page: ${JSON.stringify(doubleTap)}`);
// ^ Safari pinches through its gesture events, which Chrome does not send: the page cancels them
// on the document, and the viewport refuses scaling (docs/web-client.md "Zoom").
const safari = await evaluate(`(() => { const sent = ["gesturestart", "gesturechange"].map((name) => { const event = new Event(name, { bubbles: true, cancelable: true });
  document.querySelector('.composer-field').dispatchEvent(event); return event.defaultPrevented; });
  return { sent, viewport: document.querySelector('meta[name=viewport]').content, page: getComputedStyle(document.documentElement).touchAction }; })()`);
if (safari.sent.join() !== "true,true" || !safari.viewport.includes("maximum-scale=1") || !safari.viewport.includes("user-scalable=no") || safari.page !== "pan-x pan-y") {
  await fail(`Safari's zoom is not refused: ${JSON.stringify(safari)}`);
}

// --- A task that waits for the owner's answer: in plans' chat its manager reads "plans" and
// ui's "Atlas". #1's reservoir row with Answer, whose chip opens its sheet: the story asks for
// the answer, the field says so, and the answer is a bubble; then the list cleared one row
// after another: Land #2, Land #1, and then no reservoir with the focus on the composer.
const plans = Number(plansArg);
await go(`#/p/${plans}`);
await until("!!document.querySelector('.pill')", "plans has no reservoir");
await until("document.querySelectorAll('.log .bubble').length >= 2", "plans' chat shows no managers' bubbles", 30);
const plansAuthors = await bubbles();
// ^ plans' manager has no name of its own: its bubbles keep no author line (the top row names plans); ui's "Atlas" is a peer.
if (!plansAuthors.some(({ author, tag }) => author === "" && tag === "To Atlas") || !plansAuthors.some(({ author }) => author === "Atlas") || plansAuthors.some(({ author }) => author === "plans")) await fail(`plans' managers read ${JSON.stringify(plansAuthors)}`);
// ^ An empty `sent` and a `-` reply in plans' older log are no bubbles: no bubble says nothing.
const emptyBubbles = plansAuthors.filter(({ tag, text }) => ["", "-"].includes(text.slice(tag.length).trim()));
if (emptyBubbles.length) await fail(`plans' chat shows bubbles that say nothing: ${JSON.stringify(emptyBubbles)}`);
// ^ plans' older log (test-web-client.py): six events with quiet replies fold into one row, the
// tasks once each with their newest words and the count of rows it hides; the last event and its
// reply stay under it. The owner's message ends the run: the one event after it does not fold.
const foldRow = "[...document.querySelectorAll('.log > .fold')].find((node) => /#89/.test(node.textContent))";
await until(`!!${foldRow}`, "plans' run of events did not fold", 30);
const foldSeen = async () => evaluate(`(() => { const fold = ${foldRow}; const toggle = fold.querySelector('.fold-toggle');
  const after = (node) => node.nextElementSibling?.textContent.trim();
  return { chips: [...toggle.querySelectorAll('.fold-chip')].map((node) => node.textContent),
    count: toggle.querySelector('.fold-count').textContent, open: toggle.getAttribute('aria-expanded'), inside: [...fold.querySelectorAll('.status-line, .reply-text')].map((node) => node.textContent),
    next: [after(fold), after(fold.nextElementSibling)], hidden: [...document.querySelectorAll('.log > .reply-line, .log > .status-line')].some((node) => /Landing #89|^#89 ready/.test(node.textContent)),
    short: [...document.querySelectorAll('.log > .status-line')].some((node) => node.textContent === '#92 ready') && [...document.querySelectorAll('.log > .reply-line')].some((node) => /Landing #92/.test(node.textContent)) }; })()`);
let folding = await foldSeen();
if (JSON.stringify(folding.chips) !== JSON.stringify(["#89 done", "#91 done", "#88 done"]) || folding.count !== "10 updates" || folding.open !== "false"
  || folding.inside.length || folding.hidden || folding.next[0] !== "#88 done" || !/#88 is on main/.test(folding.next[1] ?? "") || !folding.short) await fail(`plans' folded run reads ${JSON.stringify(folding)}`);
// ^ The usage limit's pause and resume are muted wsx lines with the manager's name, between the owner's
// message and the reply it held, never a bubble or under the name as a reply.
const limitSeen = await evaluate(`(() => { const log = [...document.querySelectorAll('.log > *')]; const at = log.findIndex((node) => node.textContent.trim() === 'plans paused: usage limit, resumes at 07:45');
  return { at, resumed: log[at + 1]?.textContent.trim(), tone: at < 0 ? '' : log[at].className, before: log[at - 1]?.textContent.trim().slice(0, 12), after: log[at + 2]?.textContent.trim().slice(0, 12) }; })()`);
if (limitSeen.at < 0 || limitSeen.resumed !== "plans resumed" || limitSeen.tone !== "status-line" || !/Thanks/.test(limitSeen.before) || !/Noted/.test(limitSeen.after)) await fail(`plans' limit rows read ${JSON.stringify(limitSeen)}`);
// ^ A long reply stored as quiet (a table and a list after an event) is a whole bubble.
const longQuiet = await evaluate("({ bubble: [...document.querySelectorAll('.log .bubble')].some((node) => /Release notes are ready/.test(node.textContent) && !!node.querySelector('table')), line: [...document.querySelectorAll('.reply-line')].some((node) => /Release notes are ready/.test(node.textContent)) })");
if (!longQuiet.bubble || longQuiet.line) await fail(`a long reply to events reads ${JSON.stringify(longQuiet)}`);
await evaluate(`${foldRow}.scrollIntoView({ block: 'center' })`);
await shot("fold-closed");
await evaluate(`${foldRow}.querySelector('.fold-toggle').click()`);
await until(`${foldRow}.querySelector('.fold-toggle').getAttribute('aria-expanded') === 'true'`, "a tap did not open the folded run");
folding = await foldSeen();
if (folding.inside.length !== 10 || folding.inside[0] !== "#89 ready" || folding.inside[5] !== "I'm landing #91. More" || folding.count !== "Hide 10 updates") await fail(`plans' open run reads ${JSON.stringify(folding)}`);
await evaluate(`${foldRow}.scrollIntoView({ block: 'start' })`);
await shot("fold-open");
await evaluate(`${foldRow}.querySelector('.fold-toggle').click()`);
await until(`${foldRow}.querySelector('.fold-toggle').getAttribute('aria-expanded') === 'false' && !${foldRow}.querySelector('.status-line')`, "a second tap did not fold the run");
// --- A long chat's render (docs/web-client-chat.md "Long chats"): plans' log holds 3,000 older
// messages with markdown and chips. At 390 px with the CPU slowed 4x, a read that brings no new
// message (the event a change sends) is timed on the main thread; it renders no bubble again. The
// rows and the page's elements are counted with the newest rows shown, while a find shows every
// row, and back at the end after the find closed, where the log keeps its newest rows again.
await send("Performance.enable");
const moduleUrl = (name) => `new URL('${name}', document.querySelector('script[type=module]').src)`;
await evaluate(`(async () => { const { options } = await import(${moduleUrl("preact.js")});
  // ^ Preact's render hook (\`options._render\`, \`__r\` in its build) runs for each component it renders, not for one it skips.
  const before = options.__r; window.wsxRenders = 0;
  options.__r = (vnode) => { if (vnode.type?.name === 'Bubble') window.wsxRenders += 1; before?.(vnode); }; })()`);
const renderCost = async (label) => {
  const busy = async () => (await send("Performance.getMetrics")).metrics.find((item) => item.name === "TaskDuration").value;
  await send("Emulation.setCPUThrottlingRate", { rate: 4 });
  const start = await busy();
  await evaluate(`(async () => { window.wsxRenders = 0; performance.clearResourceTimings();
    const { events } = await import(${moduleUrl("store.js")});
    events.dispatchEvent(new CustomEvent('manager', { detail: {} }));
    for (let n = 0; n < 500 && !performance.getEntriesByType('resource').some((entry) => /\\/chat(\\?|$)/.test(entry.name)); n++) await new Promise((done) => setTimeout(done, 20));
    await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))); })()`);
  const ms = Math.round(((await busy()) - start) * 1000);
  await send("Emulation.setCPUThrottlingRate", { rate: 1 });
  const seen = await evaluate("({ rows: document.querySelector('.log').children.length, named: document.querySelectorAll('.log > [data-key]').length, nodes: document.getElementsByTagName('*').length, renders: window.wsxRenders })");
  const line = { label, ms, ...seen };
  fs.appendFileSync(path.join(captures, "render.txt"), `${JSON.stringify(line)}\n`);
  return line;
};
const costs = [await renderCost("newest rows")];
// --- Find (docs/web-client-chat.md "Find"): the header's icon opens the bar and loads the whole
// chat; "quasar" is only in plans' 1st and 4th older turns, past the newest 400 entries
// (test-web-client.py). It starts at the newer match; Previous goes to the older one, Next back.
const findButton = "document.querySelector('.bar-tools button[aria-label=\"Find in chat\"]')";
if (await evaluate(`!${findButton} || !!${findButton}.textContent.trim()`)) await fail("the header has no icon-only Find button");
if (await evaluate("[...document.querySelectorAll('.log .bubble')].some((node) => /quasar/i.test(node.textContent))")) await fail("the oldest turns show before a find");
await evaluate(`${findButton}.click()`);
await until("document.activeElement === document.querySelector('.find-field')", "the find bar did not open with its field focused");
await typeInto(".find-field", "QUASAR");
const found = () => evaluate(`(() => { const current = CSS.highlights.get('find-current'); const range = current && [...current][0];
  const node = range?.startContainer.parentElement; const log = document.querySelector('.log').getBoundingClientRect(); const box = node?.getBoundingClientRect();
  return { count: document.querySelector('.find-count').textContent, marks: CSS.highlights.get('find')?.size ?? 0, text: range?.toString(),
    bubble: node?.closest('.bubble')?.textContent ?? '', shown: !!box && box.top >= log.top && box.bottom <= log.bottom }; })()`);
await until("document.querySelector('.find-count').textContent === '2/2'", "the find did not count 2/2 over the whole chat", 30);
costs.push(await renderCost("find open"));
let seen = await found();
if (seen.marks !== 2 || seen.text !== "quasar" || !/notes are in docs/.test(seen.bubble) || !seen.shown) await fail(`the newest match reads ${JSON.stringify(seen)}`);
await shot("find");
await evaluate("document.querySelector('.find button[aria-label=\"Previous match\"]').click()");
await until("document.querySelector('.find-count').textContent === '1/2'", "Previous did not move to the older match");
seen = await found();
if (seen.text !== "Quasar" || !/Where did the Quasar notes go/.test(seen.bubble) || !seen.shown) await fail(`the older match reads ${JSON.stringify(seen)}`);
await evaluate("document.querySelector('.find button[aria-label=\"Next match\"]').click()");
await until("document.querySelector('.find-count').textContent === '2/2'", "Next did not move back to the newer match");
await evaluate("document.querySelector('.find-field').focus()");
await key("Escape", 27);
await until("!document.querySelector('.find') && !CSS.highlights.has('find') && !CSS.highlights.has('find-current')", "Esc did not close the find and clear its marks");
await evaluate("document.querySelector('.log').scrollTop = document.querySelector('.log').scrollHeight");
await until("!document.querySelector('.jump')", "the long chat did not reach its end after the find");
await pause(300);
costs.push(await renderCost("at the end after a find"));
console.log(`long chat render at 390 px, CPU 4x: ${costs.map((cost) => `${cost.label} ${cost.ms} ms, ${cost.named} rows, ${cost.nodes} elements, ${cost.renders} bubbles rendered`).join("; ")}`);
const costly = costs.filter((cost) => cost.renders > 0 || (cost.label === "at the end after a find" && cost.named > 60));
if (costly.length) await fail(`a read with no new message rendered bubbles again, or the log kept more than its newest 60 rows: ${JSON.stringify(costs)}`);
// --- Questions (docs/tasks.md "Questions" under "Manager"): plans' log asked four times and the
// owner answered some (test-web-client.py); the plain owner messages since answered more, so plans
// is asked once more here. Each open question waits in the reservoir with Reply and Dismiss; a
// dismiss clears only that one: its row, its bubble's border and the overview's `asks`, which
// Home's accent reads.
// ^ The find above left the whole log among the rows (docs/web-client-chat.md "Find"), the old
// questions with them; the chat read anew shows only its newest part.
await go("#/");
await go(`#/p/${plans}`);
await until("!!document.querySelector('.log .bubble') && !document.querySelector('.find')", "plans' chat did not come back after the find");
const plansAsks = () => evaluate(`fetch('api/tasks/${plansArg}').then((r) => r.json()).then((data) => data.asks)`);
// ^ The fake manager answers ASK with "Should I land it now?" (scripts/fake-claude-stream.py).
await evaluate(`fetch('api/tasks/${plansArg}/say', { method: 'POST', headers: { 'content-type': 'application/json' }, body: ${JSON.stringify(JSON.stringify({ text: "ASK" }))} })`);
await until(`fetch('api/tasks/${plansArg}').then((r) => r.json()).then((data) => data.asks.some((ask) => ask.quote.includes('Should I land it now?')))`, "plans' manager did not ask", 300);
const openAsks = await plansAsks();
const plansQuestion = openAsks.find((ask) => ask.quote.includes("Should I land it now?"));
if (openAsks.some((ask) => ask !== plansQuestion && !["Should the fold keep five rows?", "Can #92 wait for Monday?"].includes(ask.quote))) await fail(`plans' open questions read ${JSON.stringify(openAsks)}`);
const question = plansQuestion.quote;
await openReservoir();
await until(`!!${rowOf(question)}`, "an open question has no reservoir row");
const questionRow = (await rows()).find((row) => row[1] === question);
if (questionRow[2] !== "Asks you" || questionRow[3] !== "Reply+Dismiss") await fail(`the question's row reads ${JSON.stringify(questionRow)}`);
await shot("question-row");
// ^ A tap on a question's label takes the chat to its reply (docs/web-client-tasks.md "Project
// screen"): the bubble sits inside the log's scroll box and shows the accent tint. plans' first
// question is older than the rows the page loads (test-web-client.py), so the tap reads the chat around it.
const oldQuestion = openAsks.find((ask) => ask.quote === "Should the fold keep five rows?");
if (!oldQuestion) await fail(`plans' open questions read ${JSON.stringify(openAsks)}`);
// ^ The node is in the log and its box inside the log's box.
const inLog = (selector) => `(() => { const node = document.querySelector(${JSON.stringify(selector)}); if (!node) return false;
  const log = document.querySelector('.log').getBoundingClientRect(), at = node.getBoundingClientRect(); return at.top >= log.top && at.bottom <= log.bottom; })()`;
const bubbleShown = (id) => `${inLog(`.log [data-key="reply-${id}"]`)} && document.querySelector('.log [data-key="reply-${id}"]').hasAttribute('data-found')`;
if (await evaluate(`!!document.querySelector('.log [data-key="reply-${oldQuestion.id}"]')`)) await fail("the old question's bubble is among the rows before its tap");
for (const ask of [oldQuestion, plansQuestion]) {
  await evaluate("document.querySelector('.log').scrollTop = 0");
  await evaluate(`${rowOf(ask.quote)}.querySelector('button.row-open').click()`);
  await until(bubbleShown(ask.id), `a tap on the question "${ask.quote}" did not bring its bubble into view and mark it`, 50);
  if (!(await rows()).some((row) => row[1] === ask.quote && row[3] === "Reply+Dismiss")) await fail(`the tap on "${ask.quote}" changed its row`);
}
await shot("question-shown");
// ^ A jump to a reply outside the rows opens the chat around it, as a quote's does: Back to newest returns.
if (await evaluate("!!document.querySelector('.note.around')")) {
  await click(".note.around button");
  await until("!document.querySelector('.note.around')", "Back to newest did not return from the question's reply", 60);
}
await evaluate(`${rowOf(question)}.querySelector('.need-action[title="Reply"]').click()`);
await until(`document.querySelector('.reply-quote-text')?.textContent === ${JSON.stringify(`Replying to: ${question}`)}`, "Reply did not quote the question in the composer");
await click('.reply-quote button[aria-label="Remove the quote"]');
await openReservoir();
await evaluate(`${rowOf(question)}.querySelector('.need-action[title="Dismiss"]').click()`);
await until(`!${rowOf(question)}`, "Dismiss did not clear the question's row", 100);
const left = await plansAsks();
if (left.some((ask) => ask.id === plansQuestion.id) || left.length !== openAsks.length - 1) await fail(`after a dismiss plans asks ${JSON.stringify(left)}`);
const chatAsks = await evaluate(`fetch('api/tasks/${plansArg}/chat').then((r) => r.json()).then((data) => data.asks)`);
if (JSON.stringify(chatAsks) !== JSON.stringify(left.map((ask) => ask.id))) await fail(`the chat's asks ${JSON.stringify(chatAsks)} differ from the overview's ${JSON.stringify(left)}`);
await until(`!document.querySelector('.log [data-key="reply-${plansQuestion.id}"][data-asks], .log [data-key="reply-${plansQuestion.id}"] [data-asks]')`, "the dismissed question's bubble keeps its border");
// --- A tap on a quote whose message is not among the rows (docs/web-client-chat.md "Replies"):
// plans' first older turn (message 5001, test-web-client.py) is past the rows the page holds, so
// the manager's quote of it opens the chat around that message, marked, and Back to newest returns.
// (After the questions: this plain owner message would answer the newest open one.)
await evaluate(`fetch('api/tasks/${plansArg}/say', { method: 'POST', headers: { 'content-type': 'application/json' }, body: ${JSON.stringify(JSON.stringify({ text: "QUOTE 5001 about the notes" }))} })`);
const oldQuote = "[...document.querySelectorAll('.log .bubble[data-side=in] button.quote')].at(-1)";
await until(`${oldQuote}?.textContent === 'Where did the Quasar notes go?' && !document.querySelector('.log .typing')`, "the manager's quote of the oldest turn did not show", 120);
if (await evaluate("!!document.querySelector('.log > [data-id=\"5001\"]')")) await fail("message 5001 is among the rows before the tap");
await evaluate(`${oldQuote}.click()`);
await until("!!document.querySelector('.log > [data-id=\"5001\"][data-found]')", "a tap on the quote of a message outside the rows did not open the chat there, marked", 60);
const around = await evaluate(`(() => {
  return { shown: ${inLog('.log > [data-id="5001"]')}, note: document.querySelector('.note.around')?.textContent ?? '', text: document.querySelector('.log > [data-id="5001"]').textContent }; })()`);
if (!around.shown || !/earlier part/.test(around.note) || !/Where did the Quasar notes go/.test(around.text)) await fail(`the chat opened around the quoted message reads ${JSON.stringify(around)}`);
await shot("quote-around");
await click(".note.around button");
await until("!document.querySelector('.note.around') && [...document.querySelectorAll('.log .bubble')].some((node) => node.textContent.includes('about the notes'))", "Back to newest did not return to the newest part", 60);
await openReservoir();
await until(`!!${rowOf("#1")}`, "the waiting task has no reservoir row");
const waitRow = (await rows()).find((row) => row[0] === "#1");
if (waitRow[2] !== "Waiting for your answer" || waitRow[3] !== "Answer") await fail(`the waiting row reads ${JSON.stringify(waitRow)}`);
await evaluate(`${rowOf("#1")}.querySelector('.row-open').click()`);
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the waiting task's sheet did not open");
await lean("the waiting sheet");
told = await story();
if (!/^Waiting for your answer for [^:]+: .+ Answer the manager; the worker goes on then\.$/.test(told.next)) await fail(`the top of a waiting task reads ${JSON.stringify(told)}`);
await shot("waiting-sheet", ".composer-send");
await resize([1280, 800, 1]);
await until("!!document.querySelector('dialog.task-sheet[open] .task-next button[aria-label^=\"Answer #1\"]')", "the waiting sheet has no Answer at 1280 px");
await buttonRoom("the waiting sheet");
await shot("waiting-sheet", ".composer-send");
await resize([390, 844, 2]);
await until("!!document.querySelector('dialog.task-sheet[open] .task-next button[aria-label^=\"Answer #1\"]')", "the waiting sheet has no Answer at 390 px");
await buttonRoom("the waiting sheet");
// ^ The sheet's Answer closes the sheet over the chat and puts the mention into the composer. The
// manager relays it, and the message shows read-only in the sheet's Messages fold.
// ^ All of it inside the tap, read in the same task: a phone raises the keyboard only for a focus
// made there (docs/web-client-tasks.md "Task sheet").
const inTap = await evaluate(`(() => {
  document.querySelector('.task-next button[aria-label^="Answer #1"]').click();
  const field = document.querySelector('.composer-field');
  return { open: document.querySelector('dialog.task-sheet').open, focused: document.activeElement === field, value: field.value, caret: field.selectionStart, end: field.selectionEnd };
})()`);
if (inTap.open || !inTap.focused || inTap.value !== "#1 " || inTap.caret !== 3 || inTap.end !== 3) await fail(`the sheet's Answer did not focus the composer inside the tap: ${JSON.stringify(inTap)}`);
await until(`location.hash === ${JSON.stringify(`#/p/${plans}`)} && !document.querySelector('dialog.task-sheet')`, "the sheet's Answer did not close the sheet");
await composerHas("#1 ", "the sheet's Answer did not put #1 into the composer");
await typeInto(".composer-field", `#1 go on\nRUN wsx task add 1 "WORK keys.txt" -p plans --json`);
await click(".composer-send");
await go(`#/p/${plans}/t/1`);
await until("document.querySelectorAll('.task-talk .bubble').length === 1", "the answer is not a bubble", 60);
if (await evaluate("document.querySelector('.task-talk .bubble').dataset.side") !== "out") await fail("the answer is not the owner's bubble");
await until(`(document.querySelector('.task-sheet .progress-health')?.textContent ?? '') === 'Ready, waiting for you to land it' && ${hasNext("Land")}`, "#1 did not get ready after the answer", 120);
await closeSheet("the waiting sheet", `#/p/${plans}`);
// ^ Closing the sheet gives the composer the focus.
await until("document.activeElement?.classList.contains('composer-field') && !document.activeElement.closest('dialog')", "closing the sheet did not focus the composer");
// --- An uncommitted change in the main checkout to the file #2 writes: Land stays, a line
// under the buttons names it and wraps on a phone; a commit ends the line. The commit is then
// undone, so #2 still lands.
const git = (...args) => execFileSync("git", ["-C", plansCheckout, ...args], { stdio: "pipe" });
fs.writeFileSync(path.join(plansCheckout, longName), "the owner's draft\n");
await go(`#/p/${plans}/t/2`);
await until("(document.querySelector('.task-sheet .task-approval[role=note]')?.textContent ?? '').startsWith('Land stops:')", "no Land stops line for a dirty main checkout", 30);
const stops = await evaluate(`(() => { const line = document.querySelector('.task-sheet .task-approval[role=note]').getBoundingClientRect();
  const buttons = [...document.querySelectorAll('.task-sheet .task-next .button')].map((node) => node.getBoundingClientRect());
  const note = document.querySelector('.task-sheet .task-approval[role=note]'), scroll = document.querySelector('.task-sheet .scroll');
  return { text: note.textContent, top: line.top, right: line.right, width: innerWidth, below: Math.max(...buttons.map((box) => box.bottom)),
    sideways: [document.documentElement, scroll, note].filter((node) => node.scrollWidth > node.clientWidth + 1).map((node) => node.className || node.tagName) }; })()`);
if (!stops.text.includes(longName)) await fail(`the Land stops line reads ${stops.text}`);
if (!(await nextButtons()).includes("Land")) await fail(`with the line the buttons are ${await nextButtons()}`);
if (stops.top < stops.below || stops.right > stops.width || stops.sideways.length) await fail(`the Land stops line is not under the buttons or overflows: ${JSON.stringify(stops)}`);
await shot("land-stops");
git("add", longName);
git("-c", "user.name=owner", "-c", "user.email=owner@example.invalid", "commit", "-q", "-m", "the owner's draft");
await closeSheet("the land-stops sheet", `#/p/${plans}`);
await go(`#/p/${plans}/t/2`);
await until("!!document.querySelector('dialog.task-sheet[open] .story') && !document.querySelector('.task-sheet .task-approval[role=note]')", "the Land stops line stayed after the commit", 30);
git("reset", "-q", "--hard", "HEAD~1");
await closeSheet("the committed sheet", `#/p/${plans}`);

// --- plans' failed deploy: its fold opens to the last lines, monospace, at most half the screen.
await go(`#/p/${plans}/page`);
await until("!!document.querySelector('.project-overview .deploy-output summary')", "plans' page has no fold for the failed deploy", 30);
if (!/^How the deploy of [0-9a-f]{8} ended$/.test((await text(".deploy-output summary"))[0])) await fail(`the fold reads ${await text(".deploy-output summary")}`);
await click(".deploy-output summary");
await until("document.querySelector('.deploy-output').open", "the fold did not open");
await pause(200);
const fold = await evaluate(`(() => { const pre = document.querySelector('.deploy-output pre'), box = pre.getBoundingClientRect();
  return { height: box.height, font: getComputedStyle(pre).fontFamily, end: pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 1,
    last: pre.textContent.trim().split('\\n').pop(), sideways: document.documentElement.scrollWidth > innerWidth }; })()`);
if (fold.last !== "deploy step 60" || !fold.end) await fail(`the fold does not open to the last lines: ${JSON.stringify(fold)}`);
if (!/mono/i.test(fold.font) || fold.height > 844 / 2 || fold.sideways) await fail(`the fold's box is ${JSON.stringify(fold)}`);
await shot("deploy-output");
await closeSheet("plans' page", `#/p/${plans}`);

await openReservoir();
await until(`(${rowOf("#1")}?.querySelector('.need-action')?.title ?? '') === 'Land'`, "#1's row does not offer Land", 60);
// ^ A trial on the live site: its Fail tells the manager, with the mention; the trial runs until
// the manager's add or End (docs/tasks.md "Hold and trial").
const actOnTwo = (name) => evaluate(`fetch('api/tasks/${plans}/t/2/${name}', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' }).then((answer) => answer.status)`);
if (await actOnTwo("trial") !== 200) await fail("the trial of #2 did not start");
await until(`(${rowOf("#2")}?.querySelector('.need-action')?.title ?? '') === 'Fail: tell the manager'`, "the live trial's row has no Fail: tell the manager", 120);
await typeInto(".composer-field", "");
await evaluate(`${rowOf("#2")}.querySelector('.need-action').click()`);
await composerHas("#2 ", "Fail: tell the manager did not put #2 into the composer");
await entered();
if (await evaluate(`(() => { const [fail, pass] = ${rowOf("#2")}.querySelectorAll('.need-action'); return getComputedStyle(fail).color === getComputedStyle(pass).color; })()`)) await fail("Fail: tell the manager looks like Pass and land");
await shot("trial-fail-mention", ".composer-send");
await typeInto(".composer-field", "");
if (await actOnTwo("trial_end") !== 200) await fail("the trial of #2 did not end");
await until(`(${rowOf("#2")}?.querySelector('.need-action')?.title ?? '') === 'Land'`, "#2's row did not go back to Land", 60);
moved = await clearRow("#2", "Land");
moved = await clearRow("#1", "Land");
await until("!document.querySelector('.reservoir')", "the cleared list still shows the reservoir", 60);
await until("!document.activeElement?.matches('textarea, input')", `with the list cleared the focus is on ${JSON.stringify(await focused())}`, 30);
await shot("reservoir-cleared", ".composer-send");

// --- The task sheet has no link to the worker's terminal, though #6's worker has a session;
// the project's ⋯ keeps Sessions.
await go(page(6));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "#6's sheet did not open");
if (await count('.task-sheet a[href*="/terminal"]')) await fail("the task sheet links to the worker's terminal");
await closeSheet("#6's sheet");
await click(".more-button");
if (!(await text(".more-menu a")).includes("Sessions")) await fail(`the ⋯ menu lost Sessions: ${await text(".more-menu a")}`);
await click(".more-button");

// --- Narrow and wide: the tabs in the first screen, the ⋯ menu to the worktrees.
for (const next of [[320, 568, 2], [1280, 800, 1]]) {
  await resize(next);
  await go("#/");
  await until("!!document.querySelector('.home-row')", "home did not open");
  await tabsInView();
  await shot("home");
  // ^ On a wide screen the rail keeps the list it was reached from.
  await go(chat);
  await until("!!document.querySelector('.composer-field')", "the chat did not open");
  await click(".more-button");
  await evaluate("[...document.querySelectorAll('.more-menu a')].find((node) => node.textContent === 'Sessions').click()");
  await until(`location.hash === '#/p/${project}/sessions' && !!document.querySelector('.worktree')`, "the ⋯ menu did not open the worktrees");
  await plainWorktree(`the worktree list at ${size[0]}x${size[1]}`);
  if (!phone() && (await text(".rail-tabs a[aria-current=page]")).join() !== "Home") await fail("the rail does not keep Home for the worktrees");
  await shot("project-worktrees");
  await go(chat);
  await until("!!document.querySelector('.composer-field')", "the chat did not come back");
}
// --- Wide: the chat in its column, the sheet a panel at the right.
await go(chat);
await until("!!document.querySelector('.pill')", "the ui chat did not come back");
await resize([1280, 800, 1]);
await managerWidth("the wide chat");
await shot("project", ".composer-send");
await followEnd("1280 px");
await readAndReply("1280 px");
await viewImages("chat-images", chatImages);
await go(page(3));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the sheet did not open on a wide screen");
if ((await evaluate("getComputedStyle(document.querySelector('dialog.task-sheet')).animationName")) !== "sheet-in-side") await fail("the wide sheet does not come in from the right");
await until("document.querySelector('dialog.task-sheet')?.getAnimations().length === 0", "the sheet kept moving");
const panel = await evaluate("(() => { const r = document.querySelector('dialog.task-sheet').getBoundingClientRect(); return { left: Math.round(r.left), right: Math.round(r.right), height: Math.round(r.height) }; })()");
if (panel.right !== 1280 || panel.left < 640 || panel.height !== 800) await fail(`on a wide screen the sheet is ${JSON.stringify(panel)}`);
await shot("sheet");
await closeSheet("the wide sheet");
await go(`#/p/${project}/page`);
await until("!!document.querySelector('.project-overview .diagram img')", "the wide page did not draw its graph", 30);
await shot("page");
await key("Escape", 27);
await resize([390, 844, 2]);

// --- Settings > Notifications: three switches, each saved at once (docs/web-client.md
// "Notifications"). Only the browser's permission prompt and push service are stubbed: the
// subscription is the local receiver's (WSX_PUSH_FIXTURE), and the server checks and stores it.
const fixture = JSON.parse(process.env.WSX_PUSH_FIXTURE);
await evaluate(`(() => {
  globalThis.PUSH_PERMISSION = "default";
  globalThis.PUSH_ENDPOINT = ${JSON.stringify(fixture.endpoint)};
  Notification.requestPermission = async () => globalThis.PUSH_PERMISSION;
  const subscription = { toJSON: () => ({ endpoint: globalThis.PUSH_ENDPOINT, keys: ${JSON.stringify(fixture.keys)}, expirationTime: null }),
    unsubscribe: async () => { globalThis.PUSH_SUBSCRIBED = false; return true; } };
  PushManager.prototype.getSubscription = async () => (globalThis.PUSH_SUBSCRIBED ? subscription : null);
  PushManager.prototype.subscribe = async () => { globalThis.PUSH_SUBSCRIBED = true; return subscription; };
})()`);
const switches = () => evaluate("Object.fromEntries([...document.querySelectorAll('.options input[role=switch]')].map((node) => [node.name, node.checked]))");
const settled = (what) => until("![...document.querySelectorAll('.options input[role=switch]')].some((node) => node.disabled)", `${what}: the switches stayed disabled`);
const status = () => evaluate("document.querySelector('.option-status')?.textContent ?? ''");
await go("#/settings");
await until("document.querySelectorAll('.options input[role=switch]').length === 2", "Settings does not show the two switches");
await settled("Settings opened");
if (JSON.stringify(await switches()) !== JSON.stringify({ manager: false, agents: false })) await fail(`a new device has switches on: ${JSON.stringify(await switches())}`);
const labels = await text(".option-row .option-text");
if (!labels[0].startsWith("Manager needs me") || !labels[1].startsWith("An agent needs me") ) await fail(`the switch labels are ${labels}`);
// ^ Short hints, no "on this device" in a title, and General holds no appearance or header setting (they are in UI).
const hints = await text(".option-row .option-hint");
if (hints.join("|") !== "Tasks, approvals, things to check|Input needed or an error") await fail(`the notification hints are ${hints}`);
const general = await evaluate("[...document.querySelectorAll('.settings section')].map((section) => section.getAttribute('aria-label')).join()");
if (general.replace(",Version", "") !== "Notifications,This device" || await evaluate("/on this device/i.test(document.querySelector('.settings').textContent)")) await fail(`General is ${general}`);
if (await evaluate("/digest/i.test(document.querySelector('[aria-label=Notifications]').textContent) || !!document.querySelector('input[name=digest], input[name=digest-time]')")) await fail("Settings still offers a digest");
// ^ The label is left and the control right on a wide row; on a narrow one the control may wrap under it.
await shot("settings-notifications-empty");
await evaluate("globalThis.PUSH_PERMISSION = 'denied'");
await click("input[name=manager]");
await until("document.querySelector('.option-status.error')?.textContent.includes('blocks notifications')", "a refused permission shows no error");
await settled("the refused permission");
if ((await switches()).manager) await fail("the manager switch stayed on after the browser refused");
await shot("settings-notifications-denied");
await evaluate("globalThis.PUSH_PERMISSION = 'granted'");
await click("input[name=manager]");
await until("document.querySelector('.option-status')?.textContent === 'Saved.'", "turning on Manager needs me was not saved");
await settled("the manager switch");
const stored = await evaluate("fetch('api/push').then((response) => response.json())");
if (!stored.choices.manager || stored.choices.agents) await fail(`the server stored ${JSON.stringify(stored.choices)}`);
await shot("settings-notifications-saved");
// A failed save: the server refuses an endpoint the site does not list; the error stays.
// ^ The refusal is a 400 the browser logs; only that one is not a page defect.
const refusedFrom = problems.length;
await evaluate("globalThis.PUSH_ENDPOINT = 'https://push.invalid/x'");
await click("input[name=agents]");
await until("document.querySelector('.option-status.error')?.textContent.includes('not allowed')", "a refused save shows no error");
await settled("the refused save");
if ((await switches()).agents) await fail("the agents switch stayed on after the save failed");
await pause(4500);
if (!(await status()).includes("not allowed")) await fail("the save error went away by itself");
await shot("settings-notifications-failed");
problems.splice(refusedFrom, problems.length - refusedFrom, ...problems.slice(refusedFrom).filter((problem) => !/status of 400 .*\/api\/push$/.test(problem)));
await evaluate(`globalThis.PUSH_ENDPOINT = ${JSON.stringify(fixture.endpoint)}`);
await click("input[name=agents]");
await until("document.querySelector('.option-status')?.textContent === 'Saved.'", "turning on An agent needs me was not saved");
await settled("the agents switch");
// Mute a session from its menu; Settings counts it and unmutes it.
await go("#/sessions");
await until("[...document.querySelectorAll('a[href*=\"#/s/\"]')].some((node) => node.textContent.includes('old chat'))", "the inbox does not list old chat");
await evaluate("[...document.querySelectorAll('a[href*=\"#/s/\"]')].find((node) => node.textContent.includes('old chat')).click()");
await until("!!document.querySelector('.session .more-button')", "the session has no ⋯ menu while notifications are on");
await click(".session .more-button");
if ((await text(".more-menu button")).join() !== "Mute notifications") await fail(`the session menu reads ${await text(".more-menu button")}`);
await click(".more-menu button");
await until("(document.querySelector('.notices')?.textContent ?? document.body.textContent).includes('No more notifications about old chat')", "muting said nothing");
await go("#/settings");
await until("[...document.querySelectorAll('.option-row')].some((node) => node.textContent.startsWith('1 session muted'))", "Settings does not count the muted session");
for (const next of [[1280, 800, 1], [390, 844, 2]]) {
  await resize(next);
  await shot("settings-notifications-muted");
}
await evaluate("[...document.querySelectorAll('.option-row button')].find((node) => node.textContent === 'Unmute all').click()");
await until("![...document.querySelectorAll('.option-row')].some((node) => node.textContent.includes('muted'))", "Unmute all left a muted session");
// Every switch off: the device has no subscription, so later pushes do not reach it.
await click("input[name=manager]");
await settled("the manager switch off");
await click("input[name=agents]");
await until("document.querySelector('.option-status')?.textContent === 'Notifications are off for this device.'", "turning every switch off did not say so");

// --- Progress: the top of an open task's sheet at 390x844, for each state, from real tasks of
// `plans` (docs/web-client-tasks.md "Task sheet").
{
  const cli = JSON.parse(fs.readFileSync(wsxCli));
  const wsx = (...args) => JSON.parse(execFileSync(cli.wsx, [...args, "--json", "-p", "plans"], { env: cli.env, cwd: cli.cwd, stdio: "pipe" }));
  const taskNew = (...args) => wsx("task", "new", ...args).id;
  const show = (task) => wsx("task", "show", task);
  const stateIs = async (task, state, within = 60) => {
    for (let waited = 0; show(task).state !== state; waited += 200) {
      if (waited > within * 1000) await fail(`plans #${task} did not become ${state}: ${JSON.stringify(show(task))}`);
      await pause(200);
    }
  };
  const open = async (task) => {
    await go(`#/p/${plansArg}/t/${task}`);
    await until(`location.hash.endsWith('/t/${task}') && !!document.querySelector('dialog.task-sheet[open] .story')`, `plans #${task}'s sheet did not open`);
  };
  const top = () => evaluate(`({ health: document.querySelector('.progress-health')?.textContent ?? '', age: document.querySelector('.progress-age')?.textContent ?? '',
    steps: [...document.querySelectorAll('.progress-steps li')].map((node) => (node.dataset.done != null ? '+' : node.getAttribute('aria-current') ? '>' : '') + node.textContent.trim()).join(' '),
    goal: document.querySelector('.story-goal')?.textContent ?? '', now: document.querySelector('.progress-now')?.textContent ?? '', next: document.querySelector('.story-next')?.textContent ?? '',
    bar: document.querySelector('.task-sheet .bar-state')?.textContent ?? '', sheet: document.querySelector('dialog.task-sheet').innerText })`);
  const shows = async (task, state, want) => {
    await until(`document.querySelector('.progress-health')?.textContent === ${JSON.stringify(want.health)}`, `plans #${task} never read ${want.health}`, 60);
    const seen = await top();
    for (const [name, value] of Object.entries(want)) {
      if (value instanceof RegExp ? !value.test(seen[name]) : seen[name] !== value) await fail(`${state}: ${name} reads ${JSON.stringify(seen[name])}, not ${value}`);
    }
    if (/\bBash\b|cd \/tmp|No report yet/.test(seen.sheet)) await fail(`${state}: the sheet shows a raw line: ${seen.sheet}`);
    if (seen.bar) await fail(`${state}: the stage repeats in the top row: ${seen.bar}`);
    // ^ One glance: the folds are one level of closed heads and the last one is on the first screen.
    // Against the sheet's own bottom, since a sheet opened just now is still rising.
    const room = await evaluate(`(() => { const history = document.querySelector('.timeline'); return { open: history.open, nested: document.querySelectorAll('.task-sheet details details').length, bottom: history.querySelector('.fold-head').getBoundingClientRect().bottom, limit: history.closest('dialog').getBoundingClientRect().bottom }; })()`);
    if (room.open || room.nested || room.bottom > room.limit) await fail(`${state}: History is not folded on the first screen, or a fold holds a fold: ${JSON.stringify(room)}`);
  };
  const config = path.join(plansCheckout, "wsx.config.yml");
  const before = fs.readFileSync(config, "utf8");
  const release = path.join(path.dirname(wsxCli), "release-progress");
  await resize([390, 844, 2]);
  // ^ The config holds in the main checkout only for this block: a check that waits while its
  // working copy has hold-checks.txt, and three workflow steps.
  const steps = `  workflow:\n    steps:\n      - { name: plan, prompt: NOOP }\n      - { name: build, prompt: "WAIT ${release}" }\n      - { name: review, prompt: NOOP }\n`;
  fs.writeFileSync(config, before.replace('"test ! -e broken.txt"', '"while test -e hold-checks.txt; do sleep 0.2; done"').replace("tasks:\n", `tasks:\n${steps}`));

  // Working: step 2 of 3 is current, the age is near now, the goal is the prompt's first sentence
  // (the title is another phrase) and the now line words the newest log line, a Bash call.
  const working = taskNew("--title", "Faster parser", "Goal: NOOP make the parser faster. It must stay correct.");
  await trackTimers();
  await open(working);
  const workingTop = { health: "Working on build (2 of 3)", steps: "+plan >build review", goal: "NOOP make the parser faster.", now: "running ls -la" };
  await until(`document.querySelector('.progress-now')?.textContent === 'running ls -la'`, "the sheet never shows what the worker does now", 60);
  await shows(working, "working", { ...workingTop, age: /^(now|\dm ago)$/ });
  // ^ The age is renewed by a timer of 30 s that runs only while the age shows.
  // ^ An effect's timer: waited for, not read once.
  await until("window.__timers && [...window.__timers.values()].some((delay) => delay === 30000)", "a working task's sheet has no age timer", 20);
  await shot("progress-working", ".composer-send");
  // ^ The word's color moves by a transition: measured once it has settled.
  const signal = () => evaluate(`(async () => { const word = document.querySelector('.progress-health'); await Promise.allSettled(word.getAnimations().map((animation) => animation.finished)); const mark = document.querySelector('.progress-head .mark'), takeOver = [...document.querySelectorAll('.task-sheet button')].find((node) => node.textContent.trim() === 'Take over')?.getBoundingClientRect();
    return { tone: word.dataset.tone, color: getComputedStyle(word).color, pulses: mark.dataset.running != null, takeOver: !!takeOver && takeOver.top >= 0 && takeOver.bottom <= innerHeight }; })()`);
  const workingSignal = await signal();
  if (!workingSignal.pulses || workingSignal.tone === "danger") await fail(`a working task does not read as working: ${JSON.stringify(workingSignal)}`);
  // ^ The tools of both adapters (claude: Edit, Bash…; pi: read, bash, grep, find…) in few words,
  // a tool of neither list by its name, wsx's own lines as they are.
  const lines = ["Edit /a/b/task.js", "read /x/y.rs", "Bash cd /tmp && ls -la", "bash cargo nextest run --workspace", "Grep foo", "find src", "WebFetch https://example.com/x", "mcp__s__lookup q", "Foo", "pi refused x: no reason"];
  const worded = await evaluate(`import(new URL('story.js', [...document.scripts].find((script) => script.type === 'module').src)).then(({ doing }) => ${JSON.stringify(lines)}.map(doing))`);
  const wanted = ["editing task.js", "reading y.rs", "running ls -la", "running cargo nextest run --workspace", "searching code", "listing files", "fetching example.com", "using lookup", "using Foo", "pi refused x: no reason"];
  if (worded.join("|") !== wanted.join("|")) await fail(`the now line words tools as ${JSON.stringify(worded)}`);

  // Stuck: wsx's own stall rule, staged by the log's file time; the sheet reads it, no guess.
  const log = show(working).log;
  if (!log || !fs.existsSync(log)) await fail(`wsx task show names no log: ${log}`);
  const hourAgo = new Date(Date.now() - 3600_000);
  fs.utimesSync(log, hourAgo, hourAgo);
  await go(`#/p/${plansArg}`);
  await open(working);
  await shows(working, "stuck", { ...workingTop, health: "Stuck", age: "1h ago", next: "Stuck: no output for 1 h 0 min. The manager hears of the stall and decides what to do." });
  // ^ A stuck task stands out from a working one: its own color, no pulse, Take over on the screen.
  const stuckSignal = await signal();
  const [red, green] = stuckSignal.color.match(/\d+/g).map(Number);
  if (stuckSignal.tone !== "danger" || red <= green || stuckSignal.pulses || !stuckSignal.takeOver) await fail(`a stuck task does not stand out: ${JSON.stringify({ stuckSignal, workingSignal })}`);
  fs.appendFileSync(path.join(captures, "stuck-signal.txt"), `${JSON.stringify({ working: workingSignal, stuck: stuckSignal })}\n`);
  await shot("progress-stuck", ".composer-send");

  // Ready: every step is done, and neither age nor sentence shows.
  fs.writeFileSync(release, "go");
  await stateIs(working, "ready");
  await shows(working, "ready", { health: "Ready, waiting for you to land it", steps: "+plan +build +review", age: "", now: "" });
  // ^ The sheet clears its timer in an effect after the render: wait for it, do not read once.
  await until("[...window.__timers.values()].filter((delay) => delay === 30000).length === 0", "a ready task's sheet still has an age timer");

  // Running checks, and a second task that waits for the machine's one check slot. Neither has
  // workflow steps, so neither has a step row.
  const holder = taskNew("--no-steps", "WORK hold-checks.txt");
  await stateIs(holder, "checking");
  await open(holder);
  await shows(holder, "running checks", { health: "Running checks", steps: "", now: /^running while test -e hold-checks\.txt/ });
  await shot("progress-checks", ".composer-send");
  const queued = taskNew("--no-steps", "WORK hold-checks.txt");
  await open(queued);
  await shows(queued, "check slot", { health: "Waiting for the check slot", steps: "", now: "",
    next: new RegExp(`^Waiting for the check slot for [^:]+: #${holder} runs its checks now\\. Its checks start when the slot is free\\.$`) });
  await shot("progress-check-slot", ".composer-send");
  for (const task of [holder, queued]) {
    await stateIs(task, "checking");
    fs.rmSync(path.join(show(task).record.working_copy, "hold-checks.txt"));
    await stateIs(task, "ready");
    wsx("task", "close", task, "--discard");
  }

  // A worker's scenario run that waits for another's: the row and the sheet show wsx-core's
  // status words, the same as `wsx task show` (docs/tasks.md "Status").
  const gate = path.join(path.dirname(wsxCli), "scenario-gate");
  fs.writeFileSync(config, fs.readFileSync(config, "utf8").replace("tasks:\n", `tasks:\n  scenarios:\n    slow: "while [ ! -e ${gate} ]; do sleep 0.2; done"\n`));
  const scenario = taskNew("--no-steps", "CHECKRUN_LATE slow WORK scenario1.txt");
  for (let waited = 0; !show(scenario).log || !fs.readFileSync(show(scenario).log, "utf8").includes('"kind":"result"') && waited < 30_000; waited += 200) await pause(200);
  const behind = taskNew("--no-steps", "CHECKRUN_LATE slow WORK scenario2.txt");
  const slotWords = new RegExp(`^Waiting for the scenario test slot for [^:]+: #${scenario} runs its scenario tests now\\. Its scenario tests start when the slot is free\\.$`);
  for (let waited = 0; show(behind).status?.label !== "Waiting for the scenario test slot"; waited += 200) {
    if (waited > 60_000) await fail(`plans #${behind} never waited for the scenario test slot: ${JSON.stringify(show(behind).status)}`);
    await pause(200);
  }
  const shownStatus = show(behind).status;
  if (shownStatus.kind !== "scenario_slot" || !slotWords.test(shownStatus.sentence)) await fail(`wsx task show says ${JSON.stringify(shownStatus)}`);
  await open(behind);
  await shows(behind, "scenario test slot", { health: shownStatus.label, steps: "", now: "", next: slotWords });
  await shot("progress-scenario-slot", ".composer-send");
  await go(`#/p/${plansArg}`);
  await until("!document.querySelector('dialog.task-sheet')", "the scenario wait's sheet did not close");
  await openReservoir("running");
  // ^ The server publishes the overview on its next watch step, up to 2 s after the wait began.
  await until(`${rowOf(`#${behind}`)}?.querySelector('.row-context').textContent.startsWith(${JSON.stringify(shownStatus.label)}) ?? false`,
    `the scenario wait's Running row does not read "${shownStatus.label}"`, 10);
  // A wait for another task (`wsx task after`): the Running row says what it waits after
  // (`flow.phrase`), the sheet's Waits on row and sentence give the reason, and the sheet of the
  // task waited for lists who waits as chips under Blocks (docs/tasks.md "After another task").
  wsx("task", "after", behind, scenario, "WORK after.txt", "--because", "check");
  const reason = `then waits for #${scenario}: its checks fail`;
  if (show(behind).status.wait !== reason || show(scenario).status.waited_by?.[0]?.task !== behind) await fail(`wsx task show says ${JSON.stringify([show(behind).status, show(scenario).status])}`);
  await until(`(${rowOf(`#${behind}`)}?.querySelector('.row-context').textContent ?? '').includes(${JSON.stringify(`after #${scenario}`)})`,
    `the waiting task's Running row does not say after #${scenario}`, 10);
  await shot("running-after-reason", ".composer-send");
  await open(scenario);
  await until(`document.querySelector('.story-next')?.textContent.endsWith('Waited for by #${behind}.')`, `#${scenario}'s sheet does not say who waits for it`, 10);
  const chips = await evaluate("(() => { const term = [...document.querySelectorAll('.task-sheet .task-flow dt')].find((node) => node.textContent === 'Blocks'); return [...(term?.nextElementSibling?.querySelectorAll('.task-chip') ?? [])].map((chip) => chip.textContent.trim()); })()");
  if (chips.join() !== `#${behind}`) await fail(`the sheet of #${scenario} lists ${JSON.stringify(chips)} as waiting for it`);
  await shot("task-waited-for-by", ".composer-send");
  await open(behind);
  await until(`/Then it waits for #${scenario} to land\\. Its checks fail on main until #${scenario} lands\\./.test(document.querySelector('.story-next')?.textContent ?? '')`, `#${behind}'s sheet does not say why it waits`, 10);
  const waitsOn = await evaluate("(() => { const term = [...document.querySelectorAll('.task-sheet .task-flow dt')].find((node) => node.textContent === 'Waits on'); return term?.nextElementSibling?.textContent.replace(/\\s+/g, ' ').trim() ?? ''; })()");
  if (!new RegExp(`^#${scenario}\\b.* to land, its checks fail$`).test(waitsOn)) await fail(`#${behind}'s Waits on row reads ${JSON.stringify(waitsOn)}`);
  await shot("task-waits-on", ".composer-send");
  wsx("task", "after", behind, "--off");
  await go(`#/p/${plansArg}`);
  await until("!document.querySelector('dialog.task-sheet')", "the wait's sheet did not close");
  fs.writeFileSync(gate, "go");
  for (const task of [scenario, behind]) {
    await stateIs(task, "ready", 120);
    wsx("task", "close", task, "--discard");
  }
  fs.writeFileSync(config, before.replace('"test ! -e broken.txt"', '"while test -e hold-checks.txt; do sleep 0.2; done"').replace("tasks:\n", `tasks:\n${steps}`));
  wsx("task", "close", working, "--discard");

  // Waiting for disk: a free space no volume has, and the host resumes by itself.
  fs.writeFileSync(config, before.replace("minFreeGb: 0", "minFreeGb: 100000"));
  const disk = taskNew("NOOP");
  await stateIs(disk, "waiting");
  await open(disk);
  await shows(disk, "disk space", { health: "Waiting for disk space", next: /^Waiting for disk space for [^:]+: [\d.]+ GB free of the 100000 GB a task needs\. It goes on by itself once there is space\.$/ });
  await shot("progress-waiting", ".composer-send");
  fs.writeFileSync(config, before);
  wsx("task", "close", disk, "--discard");

  // Finished: the outcome leads and no progress view shows.
  await open(disk);
  await until("document.querySelector('.task-sheet .bar-state')?.textContent === 'closed'", "a closed task's top row has no stage");
  const closed = await top();
  if (closed.health || await count(".task-sheet .progress") || closed.next !== show(disk).status.sentence) await fail(`a finished task's top reads ${JSON.stringify(closed)}`);
  await shot("progress-finished");
  await go(chat);
}

// --- A new build on the server: a pill until it is tapped; the tap reloads at the same route
// with the composer's draft.
await go(chat);
await until("!!document.querySelector('.composer-field')", "the chat did not come back for the reload");
await typeInto(".composer-field", "a draft across the reload");
const before = await evaluate("new URL(document.querySelector('script[type=module]').src).pathname");
// ^ The server is down for a moment: its failed requests are not page defects.
const quietFrom = problems.length;
fs.writeFileSync(restartFile, "restart");
const restarted = Date.now();
while (!fs.existsSync(`${restartFile}.done`)) {
  if (Date.now() - restarted > 60_000) await fail("the server was not restarted");
  await pause(200);
}
await until("!!document.querySelector('.build-pill')", "no new-version pill after the server's build changed", 60);
if ((await text(".build-pill"))[0] !== "New version · Reload") await fail(`the pill reads ${await text(".build-pill")}`);
await shot("new-version", ".composer-send");
await evaluate("window.__beforeReload = true");
await click(".build-pill");
await until(`!window.__beforeReload && !!document.querySelector('.composer-field') && location.hash === ${JSON.stringify(chat)}`, "the pill did not reload at the same route", 30);
const after = await evaluate("new URL(document.querySelector('script[type=module]').src).pathname");
if (after === before) await fail(`the reload kept the old build ${before}`);
await until("document.querySelector('.composer-field').value === 'a draft across the reload'", "the reload lost the composer's draft");
if (await count(".build-pill")) await fail("the pill stayed after the reload");
problems.splice(quietFrom);

// Chats (docs/web-client-tasks.md "Chats"): Home lists the chat under its project, the chat's
// name over the project's, unread until read; it opens the chat at #/p/<project>/c/<chat>, the
// same bubbles without the manager's reservoir, and its composer talks to the chat.
// ^ The manager says something in the chat: three speakers there, the owner, the manager and the chat.
{
  const state = JSON.parse(fs.readFileSync(wsxCli)).env.XDG_STATE_HOME;
  const found = fs.readdirSync(state, { recursive: true }).find((file) => file.endsWith(path.join("chats", "billing", "log.jsonl")));
  if (!found) await fail(`no log of the billing chat under ${state}`);
  fs.appendFileSync(path.join(state, found), `${JSON.stringify({ unix_ms: Date.now(), kind: "message", id: 8990, from: "chat:manager", text: "manager asks billing for the numbers" })}\n`);
}
const resolved = await evaluate(`fetch('api/tasks/${project}').then((r) => r.json()).then((data) => data.resolutions.length)`);
if (!resolved) await fail("the fixture project has no resolutions, so the chat check proves nothing");
for (const wide of [false, true]) {
  await resize(wide ? [1280, 800, 1] : [390, 844, 2]);
  await go(chat);
  await until("!!document.querySelector('.composer-field')", "the chat did not open for Chats");
  const projectName = (await text(".bar-label"))[0];
  const shownName = await evaluate(`fetch('api/tasks/${project}/chat').then((r) => r.json()).then((data) => data.manager.name)`);
  // ^ Home lists the open chat under its project: its name, the project's name under it, no dot
  // before this device opened its chat; a click opens the chat.
  await go("#/");
  const chatRow = `[...document.querySelectorAll('.home-rows .chat-row')].find((r) => r.querySelector('.row-label').textContent === 'billing')`;
  await until(`!!(${chatRow})`, "Home lists no billing chat under its project");
  const listed = await evaluate(`(() => { const row = ${chatRow}; const before = row.closest('li').previousElementSibling; return { name: row.querySelector('.row-label').textContent, context: row.querySelector('.row-context').textContent.trim(), dots: row.querySelectorAll('.news-mark').length, under: before && !before.classList.contains('row-chat') ? before.querySelector('.row-label').firstChild.textContent : before?.className }; })()`);
  if (listed.name !== "billing" || !listed.context || listed.context === projectName || (!wide && listed.dots) || listed.under !== projectName) await fail(`Home lists the chat as ${JSON.stringify(listed)}, not billing over ${projectName}`);
  if (await count(".more-menu .chat-row")) await fail("the ⋯ menu lists chats");
  await shot(`chats-home${wide ? "-wide" : ""}`);
  await evaluate(`${chatRow}.click()`);
  await until(`location.hash === '#/p/${project}/c/billing' && !!document.querySelector('.log .bubble[data-side=in]')`, "the chat did not open with its reply");
  if (await count(".reservoir")) await fail("another chat shows the manager's reservoir");
  if ((await text(".bar-label"))[0] !== "billing") await fail(`the chat's title is ${await text(".bar-label")}, not its name`);
  // ^ The chat shows its own model, as the manager's chat shows the manager's (billing opened with --agent haiku-chat).
  if (!/^haiku\b/.test((await text(".bar-model"))[0] ?? "")) await fail(`the chat's bar shows the model ${JSON.stringify(await text(".bar-model"))}, not haiku`);
  // ^ The chat holds its own log: none of the project's resolutions ("#5 was closed") is in it.
  if (await count(".log .status-line, .log .fold")) await fail(`a chat shows the project's events: ${await text(".log .status-line, .log .fold")}`);
  // ^ The speakers: the manager's message is under its name with the word manager, the chat's reply under the chat's.
  const speakers = await evaluate(`[...document.querySelectorAll('.log .bubble')].map((node) => node.querySelector('.bubble-author')?.textContent ?? '').filter(Boolean)`);
  if (!speakers.includes(`${shownName} manager`) || !speakers.includes("billing") || speakers.includes(shownName)) await fail(`the chat's speakers are ${speakers}`);
  if (!wide) {
    const answered = await count(".log .bubble[data-side=in]");
    await typeInto(".composer-field", "STATUS");
    await click(".composer-send");
    await until(`document.querySelectorAll('.log .bubble[data-side=in]').length > ${answered}`, "the chat did not answer the page", 60);
  }
  await shot(`chat-room${wide ? "-wide" : ""}`, ".composer-send");
  await evaluate("document.querySelector('.bar a.icon-button').click()");
  // ^ The chat was opened from Home, so Back returns there (the entry before it).
  await until("location.hash === '#/'", "Back from a chat did not return to Home");
}
await go("#/");
await until("!!document.querySelector('.home-rows .chat-row')", "Home lost the chat");
if (await count(".chat-row .news-mark")) await fail("a chat read on this device still shows unread");

// Chat unread is the server's `news` (the newest reply that is not quiet), read against what this
// device read: a ping, a quiet reply and the owner's own message are no news; a reply is.
{
  const state = JSON.parse(fs.readFileSync(wsxCli)).env.XDG_STATE_HOME;
  const found = fs.readdirSync(state, { recursive: true }).find((file) => file.endsWith(path.join("chats", "billing", "log.jsonl")));
  if (!found) await fail(`no log of the billing chat under ${state}`);
  let id = 9000;
  const append = (entry) => fs.appendFileSync(path.join(state, found), `${JSON.stringify({ unix_ms: Date.now(), ...entry })}\n`);
  const marked = async () => {
    // ^ Home is mounted afresh each time: the loop above leaves the page on Home, where go("#/") would change nothing.
    await go(chat);
    await until("!!document.querySelector('.composer-field')", "the manager's chat did not open for the chat dot");
    await go("#/");
    await until("!!document.querySelector('.home-row') && !document.querySelector('.composer-field')", "home did not replace the chat for the chat dot");
    await until("!!document.querySelector('.home-rows .chat-row')", "Home lost the chat for the chat dot");
    return count(".chat-row .news-mark");
  };
  // ^ The overview is the observer's (a poll behind the log), not read with the manager's chat any more:
  // wait until it holds the newest reply, then until the page does.
  const overviewReply = async (reply) => {
    for (let attempt = 0; attempt < 120; attempt++) {
      const last = await evaluate(`fetch('api/tasks/${project}').then((r) => r.json()).then((data) => data.chats.find((c) => c.name === 'billing')?.last_reply ?? null)`);
      if (last === reply) return;
      await pause(500);
    }
    await fail(`the overview never listed the reply ${JSON.stringify(reply)}`);
  };
  await pause(50);
  append({ kind: "ping" });
  append({ kind: "message", id: ++id, from: "owner", text: "later" });
  append({ kind: "reply", id: ++id, to: [], text: "-", quiet: true });
  await overviewReply("-");
  if (await marked()) await fail("a ping, the owner's message or a quiet reply marks the chat unread");
  append({ kind: "reply", id: ++id, to: [], text: "billing has an answer" });
  await overviewReply("billing has an answer");
  await marked();
  await until("!!document.querySelector('.chat-row .news-mark')", "a chat's reply that is news shows no dot", 60);
}

// A chat's Home row counts its own pending approvals, as the first chat's row counts the
// project's (chats::approvals_of; docs/web-client-tasks.md "Home").
{
  const state = JSON.parse(fs.readFileSync(wsxCli)).env.XDG_STATE_HOME;
  const found = fs.readdirSync(state, { recursive: true }).find((file) => file.endsWith(path.join("chats", "billing", "log.jsonl")));
  const approvals = path.join(state, path.dirname(path.dirname(path.dirname(found))), "approvals.jsonl");
  const line = (entry) => fs.appendFileSync(approvals, `${JSON.stringify(entry)}\n`);
  const chatCount = "(document.querySelector('.home-rows .chat-row .tray[data-kind=needs] .tray-count')?.textContent ?? '')";
  const firstCount = `(document.querySelector('.home-rows .row[data-key="${project}"] .tray[data-kind=needs] .tray-count')?.textContent ?? '')`;
  await resize([390, 844, 2]);
  await go("#/");
  await until(`!!document.querySelector('.home-rows .chat-row') && ${chatCount} === ''`, "the chat's row shows a count before it has an approval");
  const before = await evaluate(firstCount);
  line({ kind: "request", id: "a9001", action: "push", chat: "billing", unix_ms: Date.now() });
  await until(`${chatCount} === '1'`, "the chat's row shows no pending approval of its own", 30);
  if ((await evaluate(firstCount)) !== before) await fail("a chat's approval counted on the first chat's row");
  await shot("chats-home-approval");
  line({ kind: "decision", id: "a9001", approved: false, outcome: "dropped; the check is done", unix_ms: Date.now() });
  await until(`${chatCount} === ''`, "the chat's row kept a decided approval", 30);
}

// New chat (docs/web-client-tasks.md "Home"): the + on Home opens a sheet with the projects, then a
// name (focused, with its rule as a hint), an optional goal and an agent whose Default names the
// manager's; a bad or an open name is refused in the sheet before it asks; Create makes the chat.
{
  const sheet = "dialog.sheet[open]";
  const open = async () => {
    await go("#/");
    await until("!!document.querySelector('.home-row')", "Home did not open for New chat");
    await click('[aria-label="New chat"]');
    await until(`!!document.querySelector('${sheet} select[name=project]')`, "the + opened no New chat sheet");
  };
  const choose = (name, value) => evaluate(`(() => { const field = document.querySelector('${sheet} [name=${name}]'); field.value = ${JSON.stringify(value)}; field.dispatchEvent(new Event(field.tagName === 'SELECT' ? 'change' : 'input', { bubbles: true })); })()`);
  const note = () => evaluate(`(() => { const node = document.querySelector('${sheet} .field-note'); return { text: node.textContent, alert: node.getAttribute('role'), invalid: document.querySelector('${sheet} input[name=name]').getAttribute('aria-invalid') }; })()`);
  await open();
  await until(`!!document.querySelector('${sheet} select[name=agent] option[value="haiku-chat"]')`, "the New chat sheet lists no agents", 30);
  const fields = await evaluate(`({ projects: [...document.querySelectorAll('${sheet} select[name=project] option')].map((option) => option.textContent), goal: document.querySelector('${sheet} textarea[name=goal]').disabled, agents: [...document.querySelectorAll('${sheet} select[name=agent] option')].map((option) => option.value),
    standard: document.querySelector('${sheet} select[name=agent] option').textContent, button: document.querySelector('${sheet} button[type=submit]').textContent, plus: document.querySelectorAll('[aria-label="Add project"]').length,
    focus: document.activeElement?.name, placeholder: document.querySelector('${sheet} input[name=name]').placeholder, hint: document.querySelector('${sheet} .field-note').textContent, agentDisabled: document.querySelector('${sheet} select[name=agent]').disabled })`);
  if (fields.goal || fields.agentDisabled || fields.button !== "Create" || fields.focus !== "name" || !fields.placeholder || !fields.hint || !/^Default \(.+\)$/.test(fields.standard) || !fields.agents.includes("haiku-chat") || fields.plus
    || fields.projects.some((name, at) => name === "Add project…" && at !== fields.projects.length - 1)) await fail(`the New chat sheet is ${JSON.stringify(fields)}`);
  await shot("new-chat");
  // ^ No name is refused in the sheet, which stays open and keeps the name field focused.
  await choose("project", String(project));
  await evaluate(`document.querySelector('${sheet} form').requestSubmit()`);
  await until(`/required/.test(document.querySelector('${sheet} .field-note')?.textContent ?? '')`, "an empty name showed no refusal");
  if (!(await evaluate(`document.activeElement?.name === 'name' && location.hash !== '#/p/${project}'`))) await fail("an empty name left the name field or the sheet");
  // ^ A name makes the chat with its goal and agent, and opens it, titled by its name.
  await choose("name", "QA");
  if ((await evaluate(`document.querySelector('${sheet} input[name=name]').value`)) !== "qa") await fail("the name was not lower-cased as typed");
  await choose("goal", "Check the release notes.\nSecond line.");
  await choose("agent", "haiku-chat");
  await evaluate(`document.querySelector('${sheet} form').requestSubmit()`);
  await until(`location.hash === '#/p/${project}/c/qa' && document.querySelector('.bar-label')?.textContent === 'qa'`, "the new chat did not open under its name", 30);
  const made = await evaluate(`fetch('api/tasks/${project}').then((r) => r.json()).then((data) => data.chats.find((chat) => chat.name === 'qa'))`);
  if (!made || made.goal !== "Check the release notes.") await fail(`the new chat is ${JSON.stringify(made)}`);
  // ^ An open name and a name the store would refuse are refused in the sheet, with no request.
  const asked = await evaluate("performance.getEntriesByType('resource').filter((entry) => /\\/chats$/.test(entry.name)).length");
  await open();
  await choose("project", String(project));
  for (const [typed, words] of [["qa", /already open/], ["9lives", /a-z, 0-9 and -/], ["manager", /first chat/]]) {
    await choose("name", typed);
    // ^ The sheet reads the project's open chats again as it opens: wait for that read before the name is judged.
    await until(`${words}.test(document.querySelector('${sheet} .field-note')?.textContent ?? '')`, `"${typed}" is not refused in the sheet: ${JSON.stringify(await note())}`);
    const refused = await note();
    if (!words.test(refused.text) || refused.alert !== "alert" || refused.invalid !== "true") await fail(`"${typed}" is not refused in the sheet: ${JSON.stringify(refused)}`);
    await evaluate(`document.querySelector('${sheet} form').requestSubmit()`);
    await pause(200);
    if (!(await evaluate(`!!document.querySelector('${sheet}')`))) await fail(`"${typed}" closed the sheet`);
  }
  await shot("new-chat-refused");
  if ((await evaluate("performance.getEntriesByType('resource').filter((entry) => /\\/chats$/.test(entry.name)).length")) !== asked) await fail("a refused name was sent to the server");
  await choose("name", "fresh");
  if ((await note()).alert) await fail("a good name is still refused");
  await evaluate(`document.querySelector('${sheet} form').closest('dialog').close()`);
  await until("!document.querySelector('dialog.sheet')", "the New chat sheet stayed open");
}

// --- Session news: the host's post for a session that needs the owner links to that session
// (docs/tasks.md "Session news"). The fixture's host posts it when this file is written.
await go(chat);
await until("!!document.querySelector('.log .bubble')", "the chat did not open for the session news");
fs.writeFileSync(newsFile, "post");
for (let waited = 0; !fs.existsSync(`${newsFile}.done`); waited += 200) {
  if (waited > 120_000) await fail("the session news was not posted");
  await pause(200);
}
const newsSession = fs.readFileSync(`${newsFile}.done`, "utf8");
await until(`!!document.querySelector('.log .bubble a[href="#/s/${newsSession}/chat"]')`, "the session news shows no link to its session", 30);
const newsLine = await evaluate(`document.querySelector('.log .bubble a[href="#/s/${newsSession}/chat"]').closest('.bubble-box').textContent.trim()`);
if (newsLine !== "claude in old-work needs input") await fail(`the session news reads "${newsLine}"`);
await shot("session-news");
await evaluate(`document.querySelector('.log .bubble a[href="#/s/${newsSession}/chat"]').click()`);
await until(`location.hash === '#/s/${newsSession}/chat'`, "the link of the session news did not open its chat");
await go(chat);

// ^ The 401s are expected, and so is the 404 of the Routines block's probe of a project outside the grant.
// --- A message a routine wrote (a digest: wsx posts it with no agent turn) has a border in the
// secondary accent, not the asks-you accent, in both schemes (docs/web-themes.md "color-secondary").
{
  const cli = JSON.parse(fs.readFileSync(wsxCli));
  execFileSync(cli.wsx, ["task", "digest", "-p", "plans", "--since", "0"], { env: cli.env, cwd: cli.cwd, stdio: "pipe" });
  await go(`#/p/${plansArg}`);
  await until("!!document.querySelector('.bubble[data-routine]')", "the digest's bubble has no routine mark", 30);
  const look = () => evaluate(`(() => { const probe = (name) => { const node = Object.assign(document.createElement('i'), {}); node.style.color = 'var(--' + name + ')'; document.body.append(node);
    const color = getComputedStyle(node).color; node.remove(); return color; };
    const box = document.querySelector('.bubble[data-routine] .bubble-box'); return { border: getComputedStyle(box).borderTopColor, secondary: probe('color-secondary'), accent: probe('color-accent'),
      asks: document.querySelector('.bubble[data-routine]').hasAttribute('data-asks') }; })()`);
  for (const scheme of ["dark", "light"]) {
    await inScheme(scheme);
    const seen = await look();
    if (seen.border !== seen.secondary || seen.secondary === seen.accent || seen.asks) await fail(`the routine message's border in the ${scheme} scheme: ${JSON.stringify(seen)}`);
    await shot(`routine-message-${scheme}`);
  }
  await inScheme("dark");
  // ^ Any wsx post has it, not only the digest's: a routine other than the digest, such as the retrospect,
  // posts under its own events text (`wsx task post --title`, docs/tasks.md "Routines").
  const before = await count(".bubble[data-routine]");
  execFileSync(cli.wsx, ["task", "post", "-p", "plans", "--title", "retrospect"], { env: cli.env, cwd: cli.cwd, input: "Since the last post: $0.10\n", stdio: "pipe" });
  await until(`document.querySelectorAll('.bubble[data-routine]').length === ${before + 1}`, "the retrospect's post has no routine mark", 30);
  const retro = await evaluate(`(() => { const last = [...document.querySelectorAll('.bubble[data-routine]')].at(-1); return { text: last.textContent, border: getComputedStyle(last.querySelector('.bubble-box')).borderTopColor }; })()`);
  const digest = await look();
  if (!retro.text.includes("Since the last post") || retro.border !== digest.secondary) await fail(`the retrospect post is ${JSON.stringify(retro)}, not bordered in ${digest.secondary}`);
  // ^ `post` is the host's flag on what `post()` wrote, not a guess from the reply's fields.
  const replies = (await chatRows(plansArg)).filter((row) => row.author === "manager" && row.kind === "message");
  const posted = replies.filter((row) => row.post);
  if (!posted.at(-1)?.text.includes("Since the last post") || posted.length < 2 || posted.length === replies.length) await fail(`plans' post rows are ${JSON.stringify(posted.map((row) => row.text.slice(0, 40)))} of ${replies.length} replies`);
  await shot("routine-message-retrospect");
  // ^ A post with a level (`wsx task post --level warn|bad`) says it in its tag and sets its border
  // in the attention or danger colour, in both schemes (docs/web-client-tasks.md "Task notifications").
  for (const [level, word, role] of [["warn", "Warn", "color-attention"], ["bad", "Bad", "color-danger"]]) {
    execFileSync(cli.wsx, ["task", "post", "-p", "plans", "--title", `guard ${level}`, "--level", level], { env: cli.env, cwd: cli.cwd, input: `memory over the limit (${level})\n`, stdio: "pipe" });
    await until(`!!document.querySelector('.bubble[data-level=${level}]')`, `the ${level} post has no level mark`, 30);
    for (const scheme of ["dark", "light"]) {
      await inScheme(scheme);
      const seen = await evaluate(`(() => { const node = Object.assign(document.createElement('i'), {}); node.style.color = 'var(--${role})'; document.body.append(node);
        const want = getComputedStyle(node).color; node.remove(); const bubble = document.querySelector('.bubble[data-level=${level}]');
        return { want, border: getComputedStyle(bubble.querySelector('.bubble-box')).borderTopColor, tagColor: getComputedStyle(bubble.querySelector('.bubble-tag')).color,
          tag: bubble.querySelector('.bubble-tag').textContent, label: bubble.getAttribute('aria-label'), text: bubble.textContent }; })()`);
      if (seen.border !== seen.want || seen.tagColor !== seen.want || seen.tag !== word || !seen.label.endsWith(`, ${word}`) || !seen.text.includes(`memory over the limit (${level})`)) await fail(`the ${level} post in the ${scheme} scheme: ${JSON.stringify(seen)}`);
    }
    await shot(`post-level-${level}`);
  }
  await inScheme("dark");
}

const defects = problems.filter((problem) => !/status of 401/.test(problem) && !/status of 404.*\/api\/tasks\/999999\/routines/.test(problem) && !/status of 400.*\/routines\/nightly\/explode/.test(problem) && !/status of 403.*\/api\/files\?project=\d+&path=$/.test(problem));
// ^ The header-position block opens Files on a site without the files grant; its bar is what it checks.
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);
fs.writeFileSync(path.join(captures, "tasks-audits.json"), JSON.stringify(audits, null, 1));
// ^ Last: its 35 messages keep the manager busy for a while.
await resize([390, 844, 2]);
await go(chat);
await until("!!document.querySelector('.pill')", "the ui chat did not come back");
// A long chat renders its newest 60 rows; "Show N earlier" adds rows above and keeps the reader
// on the row they read (docs/web-client.md "Screens").
const named = "document.querySelectorAll('.log > [data-key]').length";
for (let n = 0; n < 35; n++) {
  const status = await say(`NOOP ${n}`);
  if (status !== 200) await fail(`message ${n} of the long chat was not taken: ${status}`);
}
await until(`!document.querySelector('.log .typing')`, "the manager did not answer the long chat", 120);
// ^ Read to the end and opened again: nothing unread, so the window is the newest 60 rows.
await toLatest("390 px");
await pause(500);
await reopen("390 px");
for (let n = 0; n < 50 && !(await count(".log-earlier")); n++) await pause(100);
if (!(await count(".log-earlier"))) await fail(`a long chat shows no Show earlier: ${await evaluate(`JSON.stringify({ named: ${named}, rows: document.querySelector('.log').children.length,
  last: [...document.querySelectorAll('.log > [data-key]')].slice(-3).map((node) => node.dataset.key), fresh: !!document.querySelector('.log-new'), note: document.querySelector('.log > .note')?.textContent ?? null })`)}`);
await pause(500);
const rendered = await evaluate(named);
if (rendered > 61) await fail(`a long chat renders ${rendered} rows`);
await evaluate("document.querySelector('.log').scrollTop = 0");
await pause(300);
const reading = await rowInView();
await click(".log-earlier");
await until(`${named} > ${rendered}`, "Show earlier added no rows");
await pause(300);
const shifted = (await rowAt(reading.key)) - reading.down;
if (Math.abs(shifted) > 2) await fail(`Show earlier moved the row in view by ${shifted} px`);
await toLatest("390 px");
// --- The manager quotes an earlier message by id (docs/tasks.md "Manager"): its reply shows the
// quote as one line over its words, and a tap goes to the quoted message.
const sayAgain = async (words) => {
  const status = await say(words);
  if (status !== 200) await fail(`the manager was not told ${JSON.stringify(words)}: ${status}`);
};
await sayAgain("the plan is on the overview");
await until("[...document.querySelectorAll('.log .bubble[data-side=out]')].some((node) => node.textContent.includes('the plan is on the overview'))", "the message to be quoted did not show");
const planId = (await chatRows()).filter((row) => row.kind === "message" && row.author === "owner" && row.text === "the plan is on the overview").at(-1).id;
await sayAgain(`QUOTE ${planId} still waiting on #8`);
await until("[...document.querySelectorAll('.log .bubble[data-side=in] button.quote')].at(-1)?.textContent === 'the plan is on the overview' && !document.querySelector('.log .typing')", "the manager's quote did not show", 120);
const quoted = await evaluate(`(() => { const button = [...document.querySelectorAll('.log .bubble[data-side=in] button.quote')].at(-1);
  // ^ Muted words after an accent edge, the owner's quote look: each colour read back from a probe that names the token.
  const paint = (name) => { const probe = document.createElement('span'); probe.style.color = 'var(--' + name + ')'; document.body.append(probe); const color = getComputedStyle(probe).color; probe.remove(); return color; };
  const style = getComputedStyle(button);
  return { quote: button.textContent, body: button.closest('.bubble-body').textContent.trim(), blockquote: !!button.closest('.bubble-body').querySelector('blockquote'),
    muted: style.color === paint('color-muted'), accentEdge: style.borderLeftColor === paint('color-accent') && parseFloat(style.borderLeftWidth) > 0, oneLine: style.whiteSpace === 'nowrap' && button.offsetHeight < 40 }; })()`);
if (quoted.quote !== "the plan is on the overview" || !quoted.body.endsWith("still waiting on #8") || quoted.blockquote || !quoted.muted || !quoted.accentEdge || !quoted.oneLine) await fail(`the manager's quote shows ${JSON.stringify(quoted)}`);
await shot("manager-quote", ".composer-send");
const jumped = await evaluate(`(() => { [...document.querySelectorAll('.log .bubble[data-side=in] button.quote')].at(-1).click();
  return document.querySelector('.log .bubble[data-found]')?.textContent.trim() ?? null; })()`);
if (!jumped?.includes("the plan is on the overview")) await fail(`a tap on the manager's quote marks ${JSON.stringify(jumped)}`);
// ^ The found bubble has no outline besides its own border: an accent tint over its box that
// fades out (none under reduced motion, which holds the tint), then it is gone.
const foundLook = () => evaluate(`(() => { const row = document.querySelector('.log > [data-found]'), box = row?.querySelector('.bubble-box');
  if (!box) return null; const tint = getComputedStyle(box, '::after'), edge = getComputedStyle(box);
  return { outline: getComputedStyle(row).outlineStyle + '/' + edge.outlineStyle, tint: tint.content !== 'none' && tint.backgroundColor !== 'rgba(0, 0, 0, 0)' && parseFloat(tint.opacity) > 0, motion: tint.animationName }; })()`);
const foundOnce = await foundLook();
if (!foundOnce || foundOnce.outline !== "none/none" || !foundOnce.tint || foundOnce.motion !== "found-out") await fail(`the found bubble reads ${JSON.stringify(foundOnce)}`);
await until("!document.querySelector('.log > [data-found]')", "the found mark did not go after 2 s", 50);
await setMotion("reduce");
await evaluate(`[...document.querySelectorAll('.log .bubble[data-side=in] button.quote')].at(-1).click()`);
await until("!!document.querySelector('.log > [data-found]')", "a tap on the quote did not mark under reduced motion");
const foundCalm = await foundLook();
if (!foundCalm?.tint || foundCalm.motion !== "none" || foundCalm.outline !== "none/none") await fail(`the found bubble under reduced motion reads ${JSON.stringify(foundCalm)}`);
await shot("found-tint", ".composer-send");
await until("!document.querySelector('.log > [data-found]')", "the found mark did not go under reduced motion", 50);
await setMotion("no-preference");
// ^ One cut of a quote, by characters (wsx-core `quote_of`): a line of emoji past 120 characters
// is cut the same in the row, on the page and in the manager's quote, so a tap finds its message.
{
  const wide = `${"🦀".repeat(70)} ${"x".repeat(60)}`;
  const cut = `${[...wide].slice(0, 119).join("").trimEnd()}…`;
  await sayAgain(wide);
  await until(`[...document.querySelectorAll('.log .bubble[data-side=out]')].some((node) => node.textContent.includes(${JSON.stringify("x".repeat(60))}))`, "the emoji message did not show");
  const row = (await chatRows()).findLast((row) => row.kind === "message" && row.author === "owner" && row.text === wide);
  const shown = row && await evaluate(`document.querySelector('.log > .bubble[data-key=${JSON.stringify(row.key)}]')?.dataset.quote ?? null`);
  if (row?.quote !== cut || shown !== cut) await fail(`the emoji message's quote is ${JSON.stringify({ row: row?.quote, shown })}, not ${JSON.stringify(cut)}`);
  await sayAgain(`QUOTE ${row.id} about the crabs`);
  await until(`[...document.querySelectorAll('.log .bubble[data-side=in] button.quote')].at(-1)?.textContent === ${JSON.stringify(cut)} && !document.querySelector('.log .typing')`, "the manager's quote of the emoji message did not show", 120);
  const found = await evaluate(`(() => { [...document.querySelectorAll('.log .bubble[data-side=in] button.quote')].at(-1).click();
    return document.querySelector('.log .bubble[data-found]')?.dataset.key ?? null; })()`);
  if (found !== row.key) await fail(`a tap on the quote of the emoji message marks ${JSON.stringify(found)}, not ${row.key}`);
}
// --- A table under a list item (docs/design/list-tables.md): a manager's real reply has four numbered
// items, each with a table under it that is not indented to the item. Each table shows right after
// its item, with all its rows, and the lists keep their numbers. A plain table, a plain list and a
// list followed by a paragraph read as before.
{
  const reply = (start) => `[...document.querySelectorAll('.log .bubble[data-side=in]')].findLast((node) => node.querySelector('.markdown')?.textContent.trim().startsWith(${JSON.stringify(start)}))`;
  const shape = (start) => evaluate(`(() => { const bubble = ${reply(start)}; if (!bubble) return null; const markdown = bubble.querySelector('.markdown');
    return { kids: [...markdown.children].map((node) => node.tagName.toLowerCase() + (node.classList.contains('table-wrap') ? ':table' : '')),
      starts: [...markdown.querySelectorAll(':scope > ol')].map((list) => list.getAttribute('start') ?? '1'),
      items: [...markdown.querySelectorAll(':scope > ol > li, :scope > ul > li')].map((item) => item.textContent.trim()),
      rows: [...markdown.querySelectorAll('.table-wrap table')].map((table) => table.querySelectorAll('tr').length),
      heads: [...markdown.querySelectorAll('.table-wrap table')].map((table) => [...table.querySelectorAll('th')].map((cell) => cell.textContent.trim()).join('|')),
      raw: markdown.textContent.includes('|---|'), fits: [...markdown.querySelectorAll('.table-wrap')].every((wrap) => wrap.getBoundingClientRect().right <= bubble.querySelector('.bubble-box').getBoundingClientRect().right + 0.5) }; })()`);
  if (await say("LISTTABLE") !== 200) await fail("the message for a list-table reply was not taken");
  await until(`!!${reply("Read-only answer against wsx main")} && !document.querySelector('.log .typing')`, "the reply with tables under list items did not arrive", 120);
  const listed = await shape("Read-only answer against wsx main");
  // ^ Item, table, item, table, item, table, a paragraph, item, table: the lists restart at 2, 3 and 4.
  const kids = ["p", "ol", "div:table", "ol", "div:table", "ol", "div:table", "p", "ol", "div:table"];
  const want = { starts: ["1", "2", "3", "4"], rows: [6, 3, 4, 6], heads: ["Step of the owner's flow|wsx today", "Need|Supported way", "PR gives|Needed for solo ir?", "Item|Kind|Smallest change"] };
  if (!listed || JSON.stringify(listed.kids) !== JSON.stringify(kids) || JSON.stringify(listed.starts) !== JSON.stringify(want.starts)
    || JSON.stringify(listed.rows) !== JSON.stringify(want.rows) || JSON.stringify(listed.heads) !== JSON.stringify(want.heads) || listed.raw || !listed.fits
    || listed.items[0] !== "What wsx does today" || listed.items[3] !== "Missing vs habit") {
    await fail(`the tables under list items read ${JSON.stringify({ listed, kids, want })}`);
  }
  // ^ The reader is above the new reply, so it is brought into view for the capture.
  await evaluate(`${reply("Read-only answer against wsx main")}.scrollIntoView({ block: 'start' })`);
  await pause(300);
  await shot("list-tables", ".composer-send");
  if (await say("LISTPLAIN") !== 200) await fail("the message for a plain-shapes reply was not taken");
  await until(`!!${reply("Plain shapes:")} && !document.querySelector('.log .typing')`, "the reply with plain shapes did not arrive", 120);
  const plain = await shape("Plain shapes:");
  if (JSON.stringify(plain?.kids) !== JSON.stringify(["p", "div:table", "ul", "ol", "p"]) || JSON.stringify(plain.rows) !== "[3]" || JSON.stringify(plain.items) !== JSON.stringify(["first bullet", "second bullet", "only item"])) {
    await fail(`the plain table, list and list with a paragraph read ${JSON.stringify(plain)}`);
  }
  await resize([1280, 800, 1]);
  await evaluate(`${reply("Read-only answer against wsx main")}.scrollIntoView({ block: 'start' })`);
  await pause(300);
  const wide = await shape("Read-only answer against wsx main");
  if (!wide || !wide.fits || wide.rows.join() !== "6,3,4,6") await fail(`the tables under list items at 1280 px read ${JSON.stringify(wide)}`);
  await shot("list-tables-wide", ".composer-send");
  await resize([390, 844, 2]);
}
await toLatest("390 px");
// A ready task the manager created waits for the manager's review: under Running, with those words and a calm dot.
const asked = await say('RUN wsx task new "WORK z.txt" -p ui');
if (asked !== 200) await fail(`the manager was not asked for a task: ${asked}`);
await openReservoir("running");
await until(`!![...document.querySelectorAll('.reservoir-list[aria-label=Running] .need')].find((row) => /waiting for the manager's review/.test(row.textContent))`, "a manager-created ready task is not under Running with the review words", 900);
// ^ The manager turn, the worker's build and its checks on a loaded CI runner took over 30 s once.
const reviewed = await evaluate(`(() => { const row = [...document.querySelectorAll('.reservoir-list[aria-label=Running] .need')].find((row) => /waiting for the manager's review/.test(row.textContent));
  return { running: row.querySelector('.mark').hasAttribute('data-running'), needs: !!document.querySelector('.reservoir-list[aria-label="Needs you"]')?.contains(row) } })()`);
if (reviewed.running || reviewed.needs) await fail(`the review-waiting row is ${JSON.stringify(reviewed)}`);
// ^ The row shows wsx-core's label and the sheet its sentence, as the task's JSON has them (docs/tasks.md "Status").
{
  const id = Number((await evaluate(`[...document.querySelectorAll('.reservoir-list[aria-label=Running] .need')].find((row) => /waiting for the manager's review/.test(row.textContent)).querySelector('.task-chip').textContent.match(/#\\d+/)[0]`)).replace("#", ""));
  const rowWords = (await rows()).find((row) => row[0] === `#${id}`)[2];
  await go(`#/p/${project}/t/${id}`);
  await until("!!document.querySelector('dialog.task-sheet[open] .story-next')", "the review-waiting sheet did not open");
  const sheetWords = await story();
  const json = await evaluate(`fetch('api/tasks/${project}/t/${id}').then((r) => r.json()).then((view) => view.status)`);
  const minutes = (text) => text.replace(/ for [^:]+:/, ":");
  if (json.kind !== "review" || json.label !== "Ready, waiting for the manager's review" || !rowWords.startsWith(json.label) || await sheetState() !== json.label
    || minutes(sheetWords.next) !== minutes(json.sentence)
    || !/^Ready, waiting for the manager's review for [^:]+: (checks \d+ of \d+ passed|no checks prove it)\. The manager reads the diff, then lands it or asks for changes\.$/.test(sheetWords.next)) {
    await fail(`the review-waiting task reads ${JSON.stringify({ rowWords, sheetWords, json })}`);
  }
  await shot("review-sheet", ".composer-send");
  await go(`#/p/${project}`);
  await until("!document.querySelector('dialog.task-sheet')", "the review-waiting sheet did not close");
}
// --- An overview over the cap: the sheet cuts it to 60 lines and 4,000 characters and says so.
// ^ The file is written in `plans`' task folder, where no manager runs: the page only reads it.
{
  const cli = JSON.parse(fs.readFileSync(wsxCli));
  const shown = JSON.parse(execFileSync(cli.wsx, ["task", "show", "1", "--json", "-p", "plans"], { env: cli.env, cwd: cli.cwd, stdio: "pipe" }));
  fs.writeFileSync(path.join(path.dirname(path.dirname(shown.log)), "overview.md"), "# plans\n" + "a line of the overview\n".repeat(69));
}
// ^ Opened from plans' chat: the close is history back, which from ui's chat would return there.
await go(`#/p/${plansArg}`);
await go(`#/p/${plansArg}/page`);
await until("!!document.querySelector('dialog.task-sheet[open] .project-overview .over-limit')", "the over-cap overview shows no note", 60);
const cut = await evaluate("({ lines: (document.querySelector('.project-overview .task-body').textContent.match(/a line of the overview/g) ?? []).length, note: document.querySelector('.over-limit').textContent })");
if (cut.lines > 59 || !/Over the size limit/.test(cut.note)) await fail(`the over-cap overview is not cut: ${JSON.stringify(cut)}`);
await closeSheet("the page", `#/p/${plansArg}`);

// --- A task mention of another project (docs/web-client-tasks.md "Project screen"): ui and plans
// both have a #1. "plans #1" in ui's own reply and a bare #1 in plans' manager's message open plans'
// #1 in plans' chat; a bare #1 in ui's reply opens ui's #1; a sender the site does not grant leaves a
// bare #1 as text.
{
  const cli = JSON.parse(fs.readFileSync(wsxCli));
  const plansMark = "Compare plans #1 with #1 here";
  execFileSync(cli.wsx, ["task", "post", "-p", "ui", "--title", "task mentions"], { env: cli.env, cwd: cli.cwd, input: plansMark, stdio: "pipe" });
  const fromPlans = "Peer note: #1 waits for you";
  // ^ The chip reads "plans #1" there, so the bubble is found by the words after it.
  const peerWords = "waits for you";
  const pageModule = (name) => `import(new URL(${JSON.stringify(name)}, [...document.scripts].find((script) => script.type === 'module').src))`;
  // ^ plans' manager says it to ui's.
  const asked = await say(`RUN wsx task say -p ui --from-chat "${fromPlans}" --json`, plansArg);
  if (asked !== 200) await fail(`plans' manager was not asked to write to ui's: ${asked}`);
  // ^ ui's own chat billing (made by the fixture) writes to ui's first chat: `peer:chat:billing` is no project's.
  const fromBilling = "Chat note: #1 is ours";
  const billingAsked = await evaluate(`fetch('api/tasks/${project}/say', { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text: ${JSON.stringify(`RUN wsx task say -p ui --from-chat "${fromBilling}" --json`)}, chat: 'billing' }) }).then((response) => response.status)`);
  if (billingAsked !== 200) await fail(`ui's chat billing was not asked to write to the first chat: ${billingAsked}`);
  await go(chat);
  const chipsIn = (needle) => evaluate(`(() => { const bubble = [...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(needle)}));
    return bubble && { author: bubble.dataset.speaker, text: bubble.querySelector('.bubble-body').textContent.trim().replace(/\\s+/g, ' '),
      chips: [...bubble.querySelectorAll('a.task-chip')].map((chip) => ({ href: chip.getAttribute('href'), text: chip.textContent.trim(), label: chip.getAttribute('aria-label') })) }; })()`);
  for (const [what, needle] of [["the post of wsx task post", plansMark], ["plans' manager's note", peerWords], ["the billing chat's note", fromBilling]]) {
    await until(`[...document.querySelectorAll('.log .bubble')].some((node) => node.textContent.includes(${JSON.stringify(needle)}))`, `${what} did not reach ui's chat`, 1200);
  }
  const own = await chipsIn(plansMark);
  if (own.chips.length !== 2 || own.chips[0].href !== `#/p/${plansArg}/t/1` || !/^plans #1$/.test(own.chips[0].text)
    || own.chips[1].href !== `#/p/${project}/t/1` || own.chips[1].text !== "#1") await fail(`ui's own reply reads ${JSON.stringify(own)}`);
  const theirs = await chipsIn(peerWords);
  if (theirs.author !== "peer" || theirs.chips.length !== 1 || theirs.chips[0].href !== `#/p/${plansArg}/t/1` || !/^plans #1$/.test(theirs.chips[0].text)) await fail(`plans' message reads ${JSON.stringify(theirs)}`);
  // ^ The billing chat's bare #1 is ui's #1, and its name over the message is plain text, no link.
  const ours = await chipsIn(fromBilling);
  const billingName = await evaluate(`(() => { const bubble = [...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(fromBilling)}));
    return bubble && { link: !!bubble.querySelector('a.bubble-author'), name: bubble.querySelector('.bubble-author')?.textContent.trim() ?? null }; })()`);
  if (ours.author !== "peer" || ours.chips.length !== 1 || ours.chips[0].href !== `#/p/${project}/t/1` || ours.chips[0].text !== "#1" || billingName?.link !== false) {
    await fail(`the billing chat's message reads ${JSON.stringify({ ours, billingName })}`);
  }
  await evaluate(`[...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(plansMark)})).scrollIntoView({ block: 'center' })`);
  // ^ Not `shot`: the long chat holds the manager's `"hold": null` JSON reply, which its raw-null audit refuses.
  await capture(`${size[0]}x${size[1]}-task-mentions`);
  // ^ plans' name over its message is a link to plans' manager chat (docs/web-client-tasks.md "Project screen"): a named,
  // underlined, peer-colored link, and a tap lands on plans' chat (its title and route); Back returns to ui's chat.
  const nameOf = `(() => { const bubble = [...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(peerWords)}));
    const name = bubble?.querySelector('.bubble-author'); return name && { tag: name.tagName, href: name.getAttribute('href'), label: name.getAttribute('aria-label'), text: name.textContent.trim(),
      color: getComputedStyle(name).color, plain: getComputedStyle(bubble.closest('.log').querySelector('.bubble[data-speaker="manager"] .bubble-author') ?? name).color }; })()`;
  const nameLink = await evaluate(nameOf);
  if (nameLink?.tag !== "A" || nameLink.href !== `#/p/${plansArg}` || nameLink.label !== "Open plans manager chat" || nameLink.text !== "plans" || nameLink.color === nameLink.plain) {
    await fail(`plans' name over its message reads ${JSON.stringify(nameLink)}`);
  }
  await evaluate(`[...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(peerWords)})).scrollIntoView({ block: 'center' })`);
  await capture(`${size[0]}x${size[1]}-peer-name`);
  await evaluate(`[...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(peerWords)})).querySelector('a.bubble-author').click()`);
  await until(`location.hash === '#/p/${plansArg}' && document.querySelector('.bar-label')?.textContent === 'plans'`, "plans' name did not open plans' manager chat", 60);
  await evaluate("history.back()");
  await until(`(location.hash === '${chat}' || location.hash === '#/p/${project}') && [...document.querySelectorAll('.log .bubble')].some((node) => node.textContent.includes(${JSON.stringify(plansMark)}))`, "Back did not return to ui's chat from plans'", 100);
  // ^ A tap moves to plans' chat with #1's sheet over it; Back returns to ui's chat.
  await evaluate(`[...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(plansMark)})).querySelector('a.task-chip').click()`);
  await until(`location.hash === '#/p/${plansArg}/t/1' && !!document.querySelector('dialog.task-sheet[open]')`, "plans #1 did not open from ui's chat", 60);
  // ^ The sheet shows "#1" until the task's detail is read; plans' #1 is the one that mentions "jump between projects".
  await until("/jump between projects/.test(document.querySelector('dialog.task-sheet[open]')?.textContent ?? '')", "the sheet is not plans' #1", 100);
  await evaluate("history.back()");
  await until(`location.hash === '${chat}' || location.hash === '#/p/${project}'`, "Back did not return to ui's chat", 60);
  // ^ The own project's bare #1 opens ui's #1.
  await until(`[...document.querySelectorAll('.log .bubble')].some((node) => node.textContent.includes(${JSON.stringify(plansMark)}))`, "ui's chat did not come back", 60);
  await evaluate(`[...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(plansMark)})).querySelectorAll('a.task-chip')[1].click()`);
  await until(`location.hash === '#/p/${project}/t/1' && !!document.querySelector('dialog.task-sheet[open]')`, "ui #1 did not open from a bare #1", 60);
  await closeSheet("ui #1", chat);
  // ^ plans is not granted (taken out of the live store): its manager's bare #1 is text, no link.
  await evaluate(`(async () => { const { state, update } = await ${pageModule("store.js")}; globalThis.mentionKept = state.projects;
    update({ projects: state.projects.filter((other) => other.name !== 'plans') }); })()`);
  await until(`(() => { const bubble = [...document.querySelectorAll('.log .bubble')].find((node) => node.textContent.includes(${JSON.stringify(peerWords)}));
    return bubble && !bubble.querySelector('a.task-chip, a.bubble-author') && bubble.textContent.includes('#1 waits for you'); })()`, "an ungranted sender's bare #1 or name is not plain text", 60);
  await evaluate(`(async () => { const { update } = await ${pageModule("store.js")}; update({ projects: globalThis.mentionKept }); })()`);
}

// --- The bottom bar, Settings > UI and Settings > Usage (docs/web-client.md "Screens", "Settings").
// ^ Equal-width tabs whatever their count, an icon over the label, the current tab on a highlight;
// a tab hidden in Settings > UI leaves the bar, Settings cannot be hidden; Settings > Usage
// shows the sum of every project's usage, which the page reads from the overviews it already has.
{
  await resize([390, 844, 2]);
  await go("#/sessions");
  await until("document.querySelectorAll('.tabbar a').length === 3", "the bottom bar lacks its three tabs");
  const barShape = () => evaluate(`(() => { const tabs = [...document.querySelectorAll('.tabbar a')];
    return { names: tabs.map((tab) => tab.querySelector('.tab-label').textContent), widths: tabs.map((tab) => Math.round(tab.getBoundingClientRect().width * 10) / 10),
      stacked: tabs.every((tab) => tab.querySelector('svg').getBoundingClientRect().bottom <= tab.querySelector('.tab-label').getBoundingClientRect().top + 0.5),
      highlight: tabs.map((tab) => getComputedStyle(tab).backgroundColor !== 'rgba(0, 0, 0, 0)' ? tab.querySelector('.tab-label').textContent : null).filter(Boolean),
      pill: document.querySelector('.tabbar-pill').getBoundingClientRect().width < document.querySelector('.tabbar').getBoundingClientRect().width,
      // ^ The pill is as wide as its tabs (one --size-tab each, the gaps and its padding) and centred (docs/web-client.md "Screens").
      snug: (() => { const box = document.querySelector('.tabbar-pill').getBoundingClientRect(); const sum = tabs.reduce((total, tab) => total + tab.getBoundingClientRect().width, 0);
        return box.width - sum < 24 && Math.abs(tabs[0].getBoundingClientRect().width - parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--size-tab'))) < 0.5
          && Math.abs(box.left - (innerWidth - box.right)) <= 1; })(),
      frame: Math.round(document.querySelector('.tabbar').getBoundingClientRect().bottom) === innerHeight }; })()`);
  const equal = (shape) => shape.widths.length > 0 && shape.widths.every((width) => Math.abs(width - shape.widths[0]) <= 0.5);
  let shape = await barShape();
  if (!equal(shape) || !shape.stacked || shape.highlight.join() !== "Sessions" || !shape.pill || !shape.snug || !shape.frame) await fail(`the bottom bar is ${JSON.stringify(shape)}`);
  await shot("tabbar");
  // ^ The bar floats (docs/web-client.md "Screens"): on a notched phone the nav is transparent and takes no
  // pointer, content scrolled under the gap beside the pill is what a point there hits, and a list scrolled to
  // its end rests above the pill by --tabbar-space (the nav's measured height, safe area included).
  await send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 47, bottom: 34 } });
  const floating = async (hash) => {
    await go(hash);
    await until("document.querySelectorAll('.tabbar a').length === 3", `${hash}: the bar is gone`);
    // A list long enough to scroll: copies of the screen's own first block, removed again after the measure.
    await evaluate(`(() => { const list = document.querySelector('.screen > .page > .scroll'); const first = list.firstElementChild;
      for (let copy = 0; copy < 40; copy += 1) {
        const clone = first.cloneNode(true);
        for (const node of [clone, ...clone.querySelectorAll('[id], [name]')]) { node.removeAttribute('id'); node.removeAttribute('name'); }
        clone.dataset.floatingCopy = '';
        list.append(clone);
      } })()`);
    const measure = (scroll) => evaluate(`(() => { const list = document.querySelector('.screen > .page > .scroll'); list.scrollTop = ${scroll};
      const nav = document.querySelector('.tabbar'); const pill = document.querySelector('.tabbar-pill').getBoundingClientRect();
      const gap = document.elementFromPoint(pill.left / 2, (pill.top + pill.bottom) / 2);
      const last = list.lastElementChild.getBoundingClientRect();
      return { overflow: list.scrollHeight > list.clientHeight, space: getComputedStyle(document.documentElement).getPropertyValue('--tabbar-space'),
        navHeight: nav.offsetHeight, navBackground: getComputedStyle(nav).backgroundColor, pillBottom: Math.round(pill.bottom), height: innerHeight,
        gapHits: gap && gap !== list && list.contains(gap) && !nav.contains(gap), lastBottom: Math.round(last.bottom), pillTop: Math.round(pill.top),
        padding: getComputedStyle(list).paddingBottom }; })()`);
    const middle = await measure(300);
    const end = await measure(1e6);
    // ^ Preact keeps nodes it did not render, so the copies go before the next screen reuses this list.
    await evaluate("document.querySelectorAll('[data-floating-copy]').forEach((node) => node.remove())");
    if (!middle.overflow || middle.navBackground !== "rgba(0, 0, 0, 0)" || !middle.gapHits || middle.space !== `${middle.navHeight}px` || middle.padding !== middle.space
      || middle.pillBottom > middle.height - 34 || end.lastBottom > end.pillTop) await fail(`${hash}: the bar does not float: ${JSON.stringify({ middle, end })}`);
  };
  for (const hash of ["#/sessions", "#/settings", "#/settings/ui"]) await floating(hash);
  // ^ The tab bar's count badge uses --size-badge: 16 px high, a circle for one digit, "99+" wider and uncut.
  const badges = await evaluate(`(() => { const out = {}; const icon = document.querySelector('.tab-icon');
    const read = (label, style, text) => { out[label] = { height: parseFloat(style.height), width: parseFloat(style.width), text }; };
    for (const text of ['2', '99+']) { icon.setAttribute('data-count', text); read('tab ' + text, getComputedStyle(icon, '::after'), text); }
    icon.removeAttribute('data-count');
    out.token = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--size-badge'));
    return out; })()`);
  const roundOne = ["tab 2"].every((key) => badges[key].height === 16 && badges[key].width === badges[key].height);
  const wide = ["tab 99+"].every((key) => badges[key].height === 16 && badges[key].width > 16 && !badges[key].cut);
  if (badges.token !== 16 || !roundOne || !wide) await fail(`the count badge is not the shared 16 px circle: ${JSON.stringify(badges)}`);
  await evaluate("document.querySelector('.tab-icon').setAttribute('data-count', '2')");
  await evaluate("document.querySelector('.screen > .page > .scroll').scrollTop = 300");
  for (const scheme of ["light", "dark"]) { await inScheme(scheme); await shot(`tabbar-floating-${scheme}`); }
  await evaluate("document.querySelector('.tab-icon').removeAttribute('data-count')");
  await send("Emulation.setSafeAreaInsetsOverride", { insets: { top: 0, bottom: 0 } });
  await go("#/settings/ui");
  await until("document.querySelectorAll('.options input[name^=tab-]').length === 3", "Settings > UI does not list the three tabs");
  if ((await text(".subtabs a")).join() !== "General,UI,Usage" || (await text(".subtabs a[aria-current=page]")).join() !== "UI") await fail(`the settings sections are ${await text(".subtabs a")}`);
  const tabSwitches = () => evaluate("Object.fromEntries([...document.querySelectorAll('.options input[name^=tab-]')].map((node) => [node.name, [node.checked, node.disabled]]))");
  if (JSON.stringify(await tabSwitches()) !== JSON.stringify({ "tab-home": [true, false], "tab-inbox": [true, false], "tab-settings": [true, true] })) await fail(`the tab switches are ${JSON.stringify(await tabSwitches())}`);
  const uiSections = await evaluate("[...document.querySelectorAll('.settings section')].map((section) => [section.getAttribute('aria-label'), section.querySelector('h2').textContent].join(':')).join()");
  if (uiSections !== "Appearance:Appearance,Header:Header,Bottom bar:Bottom bar" || (await text(".options .option")).join() !== "Dark,Light,Top,Bottom,Home,Sessions,SettingsAlways shown") await fail(`Settings > UI is ${uiSections}: ${await text(".options .option")}`);
  await shot("settings-ui");
  await click("input[name=tab-inbox]");
  await until("document.querySelectorAll('.tabbar a').length === 2", "hiding Sessions left it in the bar");
  shape = await barShape();
  if (shape.names.join() !== "Home,Settings" || !equal(shape) || !shape.snug) await fail(`the bar with Sessions hidden is ${JSON.stringify(shape)}`);
  await shot("tabbar-two");
  // ^ Settings cannot be hidden: the switch is off limits, and a stored list that names it is not obeyed.
  await evaluate("localStorage.setItem('wsx-tabs-hidden', JSON.stringify(['inbox', 'settings', 'home']))");
  await send("Page.reload");
  await until("!!document.querySelector('.tabbar a')", "the page did not come back after a reload");
  const kept = await text(".tabbar a");
  if (kept.join() !== "Settings") await fail(`the bar with a stored list naming Settings is ${kept}`);
  await evaluate("localStorage.removeItem('wsx-tabs-hidden')");
  await send("Page.reload");
  await until("document.querySelectorAll('.tabbar a').length === 3", "the bar did not return with every tab shown");
  // The header at the bottom (Settings > UI) keeps the bar clear of the tab pill.
  await go("#/settings/ui");
  await click("input[name=bar][value=bottom]");
  await go("#/sessions");
  await until("document.querySelectorAll('.tabbar a').length === 3", "the bar is gone with the header at the bottom");
  await shot("tabbar-header-bottom");
  await go("#/settings/ui");
  await click("input[name=bar][value=top]");

  // Usage: the all-projects total equals the sum of the projects' own usage.
  // ^ The manager may still log a turn between the fetch and the page's own read: a mismatch is read again
  // after a reload, up to five times, and fails when it stays.
  const usd = (micro) => `$${(micro / 1e6).toFixed(2)}`;
  // ^ A tab of Settings > Usage: click it, wait for its panel's first `selector`, read that.
  const openTab = async (name, selector) => {
    await evaluate(`[...document.querySelectorAll('.usage-view [role=tab]')].find((tab) => tab.textContent === ${JSON.stringify(name)}).click()`);
    await until(`!!document.querySelector(${JSON.stringify(selector)}) && document.querySelector('.usage-view [aria-selected=true]').textContent === ${JSON.stringify(name)}`, `the ${name} tab shows no ${selector}`);
    return text(selector);
  };
  let sums, shown;
  for (let attempt = 0; attempt < 5; attempt++) {
    sums = await evaluate(`fetch('api/tasks').then((r) => r.json())
      .then((all) => Promise.all(all.map((view) => fetch('api/tasks/' + view.project + '/usage').then((r) => r.json()))))
      .then((each) => ({ projects: each.filter((usage) => usage.total.cost_micro_usd > 0).length,
        week: each.reduce((sum, usage) => sum + usage.total.cost_micro_usd, 0), turns: each.reduce((sum, usage) => sum + usage.total.turns, 0) }))`);
    if (!(sums.week > 0) || sums.projects < 2) await fail(`the fixture projects have little usage: ${JSON.stringify(sums)}`);
    await go("#/settings");
    await go("#/settings/usage");
    await until("!!document.querySelector('.usage[data-scope=all] [data-chart=cost] .usage-total .usage-cost')", "Settings > Usage shows no total");
    if ((await text(".subtabs a[aria-current=page]")).join() !== "Usage") await fail("Usage is not the current section");
    shown = {
      cost: await text(".usage[data-scope=all] [data-chart=cost] .usage-total .usage-cost"),
      cells: await openTab("Tokens", "table[aria-label='Token use and cost'] tbody tr:first-child td"),
      turns: await text("table[aria-label='Token use and cost'] tbody tr:last-child td"),
    };
    if (shown.cost[0] === usd(sums.week) && shown.cells.join() === usd(sums.week) && Number(shown.turns[0]) === sums.turns) break;
    if (attempt === 4) await fail(`the usage total is ${shown.cost} ${shown.cells} ${shown.turns}, the projects' sum ${JSON.stringify(sums)}`);
    await new Promise((done) => setTimeout(done, 2000));
  }
  const perProject = await evaluate(`fetch('api/tasks/usage').then((r) => r.json()).then((usage) => ({ rows: usage.projects, days: usage.days.length }))`);
  const rowsWeek = perProject.rows.reduce((sum, row) => sum + row.cost_micro_usd, 0);
  const sorted = perProject.rows.every((row, index) => index === 0 || perProject.rows[index - 1].cost_micro_usd >= row.cost_micro_usd);
  if (rowsWeek !== sums.week || perProject.rows.length !== sums.projects || !sorted || perProject.days !== 7) await fail(`the per-project rows are ${JSON.stringify(perProject)} for ${JSON.stringify(sums)}`);
  // ^ Cost, Tokens and Projects are tabs of one panel: each shows its own content and no other's.
  const panels = () => evaluate(`({ tabs: [...document.querySelectorAll('.usage-view [role=tab]')].map((tab) => tab.textContent + ':' + tab.getAttribute('aria-selected')).join(),
    chart: document.querySelector('.usage-chart')?.dataset.chart ?? '', totals: !!document.querySelector('table[aria-label="Token use and cost"]'),
    projects: !!document.querySelector('table[aria-label="Cost per project"]'), panel: document.querySelectorAll('[role=tabpanel]').length })`);
  const same = (got, want) => JSON.stringify(got) === JSON.stringify(want);
  await go("#/settings");
  await go("#/settings/usage");
  await until("!!document.querySelector('.usage[data-scope=all] [data-chart=cost]')", "Settings > Usage shows no cost chart");
  let seen = await panels();
  if (!same(seen, { tabs: "Cost:true,Tokens:false,Projects:false", chart: "cost", totals: false, projects: false, panel: 1 })) await fail(`the Cost tab shows ${JSON.stringify(seen)}`);
  await shot("settings-usage");
  await openTab("Tokens", "[data-chart=tokens]");
  seen = await panels();
  if (!same(seen, { tabs: "Cost:false,Tokens:true,Projects:false", chart: "tokens", totals: true, projects: false, panel: 1 })) await fail(`the Tokens tab shows ${JSON.stringify(seen)}`);
  await shot("settings-usage-tokens");
  const shownRows = await openTab("Projects", "table[aria-label='Cost per project'] tbody tr");
  seen = await panels();
  if (!same(seen, { tabs: "Cost:false,Tokens:false,Projects:true", chart: "", totals: false, projects: true, panel: 1 })) await fail(`the Projects tab shows ${JSON.stringify(seen)}`);
  if (shownRows.length !== sums.projects) await fail(`Settings > Usage lists ${shownRows.length} projects, not ${sums.projects}`);
  await shot("settings-usage-projects");
  // ^ Claude's limit state from fake Claude's rate_limit_event, a meter per window above the filters: its
  // share, reset and tone. On a phone the strip ends inside the first screen with no scroll.
  const meters = () => evaluate(`[...document.querySelectorAll('.usage-limits [role=meter]')].map((meter) => [meter.getAttribute('aria-label'), meter.getAttribute('aria-valuenow'),
    meter.querySelector('.usage-meter-used').textContent.trim(), /^resets /.test(meter.querySelector('.usage-meter-resets').textContent), meter.dataset.tone ?? '',
    Math.round(meter.querySelector('.usage-meter-track i').getBoundingClientRect().width / meter.querySelector('.usage-meter-track').getBoundingClientRect().width * 100)].join('|'))`);
  const limits = await meters();
  if (limits.join() !== "Claude 5 hours|25|25%|true||25,Claude 7 days|47|47%|true||47") await fail(`the limit meters are ${limits}`);
  const order = await evaluate(`(() => { const top = (selector) => document.querySelector(selector).getBoundingClientRect();
    return { strip: top('.usage-limits').bottom, filters: top('.usage-filters').top, tabs: top('.usage-view').top, asOf: !!document.querySelector('.usage-limits .usage-as-of'), table: !!document.querySelector("table[aria-label='Claude limit']"), height: innerHeight }; })()`);
  if (!(order.strip <= order.filters && order.filters < order.tabs) || !order.asOf || order.table) await fail(`the limit strip, filters and tabs are not in order: ${JSON.stringify(order)}`);
  if (phone() && !(order.strip < order.height)) await fail(`the limit strip ends at ${order.strip}, below the first screen of ${order.height}`);
  // ^ A warning and a refused window: a read rewritten in the page (fake Claude only reports allowed) shows the
  // tone and the status word, not color alone.
  await evaluate(`(() => { const real = window.fetch; window.__realFetch = real; window.fetch = async (url, ...rest) => {
    const response = await real(url, ...rest);
    if (!String(url).includes('/api/tasks/usage')) return response;
    const body = await response.json();
    for (const limit of body.limits ?? []) { limit.windows[0].status = 'warning'; limit.windows[1].status = 'rejected'; }
    return new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } }); }; })()`);
  await evaluate(`(() => { const select = document.querySelector('select[name=who]'); select.value = 'workers'; select.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  await until("document.querySelector('.usage-meter[data-tone=danger]')", "a refused window shows no danger tone");
  const toned = await evaluate(`[...document.querySelectorAll('.usage-meter')].map((meter) => meter.dataset.tone + ':' + meter.querySelector('.usage-meter-used').textContent.trim() + ':' + meter.getAttribute('aria-valuetext')).join()`);
  if (toned !== "attention:25% warning:25% warning,danger:47% rejected:47% rejected") await fail(`the toned meters read ${toned}`);
  await shot("settings-usage-limit-tones");
  await evaluate("window.fetch = window.__realFetch");
  await evaluate(`(() => { const select = document.querySelector('select[name=who]'); select.value = ''; select.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  // ^ The same tabs at the wide size, each for the owner's look.
  const usageSize = size;
  await resize([1280, 800, 1]);
  for (const [name, selector, capture] of [["Cost", "[data-chart=cost]", "settings-usage"], ["Tokens", "[data-chart=tokens]", "settings-usage-tokens"], ["Projects", "table[aria-label='Cost per project']", "settings-usage-projects"]]) {
    await openTab(name, selector);
    await shot(capture);
  }
  await resize(usageSize);
  await openTab("Cost", "[data-chart=cost]");
}
// --- Media a message names (docs/web-client-chat.md "Markdown", docs/web-client.md "Media"): `![alt](files/<path>)`
// and a link to a media file of the main checkout, even in a dot-folder, are attachments of that bubble as the
// owner's uploads are (one picture large, several in a grid, a video tile with its length), opening the one
// viewer with swipe between them; the text keeps its words and shows no picture twice. A secret, a path off the
// checkout and a remote address stay alt text; a link to an HTML file opens its sandboxed view.
{
  const state = JSON.parse(fs.readFileSync(wsxCli)).env.XDG_STATE_HOME;
  const logFile = fs.readdirSync(state, { recursive: true }).find((file) => /(^|\/)plans-[0-9a-f]{8}\/manager\/log\.jsonl$/.test(file));
  if (!logFile) await fail(`no manager log of plans under ${state}`);
  const base = ".studio-data/runs/one";
  const folder = path.join(plansCheckout, base);
  fs.mkdirSync(path.join(folder, "thumbs"), { recursive: true });
  // ^ Four different pictures (the server shows a message's files once by content); the files are named
  // for what a studio writes (jpg thumbnails), the server reads their bytes.
  const drawn = [];
  for (const color of ["#fac83c", "#468cdc", "#e0605b", "#5fb878"]) {
    // ^ Full-bleed pictures (a gradient and a disc), 4:3: a gap between a picture and its tile shows as the bubble's colour, not as part of the picture.
    const url = await evaluate(`(() => { const canvas = Object.assign(document.createElement('canvas'), { width: 320, height: 240 }); const pen = canvas.getContext('2d');
      const fill = pen.createLinearGradient(0, 0, 320, 240); fill.addColorStop(0, '${color}'); fill.addColorStop(1, '#282c34'); pen.fillStyle = fill; pen.fillRect(0, 0, 320, 240);
      pen.fillStyle = '#ffffff'; pen.beginPath(); pen.arc(160, 120, 50, 0, 7); pen.fill(); return canvas.toDataURL('image/png'); })()`);
    drawn.push(Buffer.from(url.split(",")[1], "base64"));
  }
  const names = ["thumbs/trend.jpg", "thumbs/b.jpg", "thumbs/c.jpg", "clip.jpg"];
  names.forEach((name, at) => fs.writeFileSync(path.join(folder, name), drawn[at]));
  fs.copyFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures", "check-clip.mp4"), path.join(folder, "clip.mp4"));
  fs.writeFileSync(path.join(folder, "report.html"), "<!doctype html><title>studio report</title><p>report</p>\n");
  fs.writeFileSync(path.join(plansCheckout, ".env"), drawn[0]);
  const reply = (id, text) => fs.appendFileSync(path.join(state, logFile), `${JSON.stringify({ unix_ms: Date.now(), kind: "reply", id, to: [], text, quiet: false })}\n`);
  reply(9_500_000, `Thumbnail of the trend:\n\n![Trend](files/${base}/thumbs/trend.jpg)`);
  reply(9_500_001, `Three more:\n\n![One](files/${base}/thumbs/trend.jpg)\n\n![Two](files/${base}/thumbs/b.jpg)\n\n![Three](files/${base}/thumbs/c.jpg)`);
  reply(9_500_002, [`A clip: ![Clip](files/${base}/clip.mp4)`, "![Secret](files/.env)", "![Parent](files/../trend.jpg)", "![Remote](https://example.com/trend.jpg)",
    `See [the trend link](files/${base}/thumbs/trend.jpg) and [the report](files/${base}/report.html)`].join("\n\n"));
  const keys = {};
  const bubbleOf = (id) => `.log .bubble[data-key=${JSON.stringify(keys[id])}]`;
  const opened = () => evaluate("[...document.querySelectorAll('.viewer-bar .time')].find((node) => / of /.test(node.textContent))?.textContent ?? ''");
  for (const wide of [false, true]) {
    await resize(wide ? [1280, 800, 1] : [390, 844, 2]);
    await go(`#/p/${plansArg}`);
    // ^ What the server grants: the files of each message, once, with the path they were named by.
    const rows = await evaluate(`fetch('api/tasks/${plansArg}/chat').then((r) => r.json()).then((data) => data.rows.filter((row) => row.id >= 9500000 && row.id <= 9500002)
      .map((row) => [row.id, row.key, (row.attachments ?? []).map((file) => [file.src, file.mime, !!file.poster])]))`);
    for (const [id, key] of rows) keys[id] = key;
    const base1 = `${base}/thumbs/trend.jpg`;
    const expected = [[9500000, [[base1, "image/png", false]]], [9500001, [[base1, "image/png", false], [`${base}/thumbs/b.jpg`, "image/png", false], [`${base}/thumbs/c.jpg`, "image/png", false]]],
      [9500002, [[`${base}/clip.mp4`, "video/mp4", true], [base1, "image/png", false]]]];
    if (JSON.stringify(rows.map(([id, , files]) => [id, files])) !== JSON.stringify(expected)) await fail(`the rows grant ${JSON.stringify(rows)}`);
    for (const id of [9500000, 9500001, 9500002]) await until(`!!document.querySelector('${bubbleOf(id)} .attachments')`, `the message ${id} shows no attachments`, 60);
    await until("[...document.querySelectorAll('.log .attachments img')].every((image) => image.naturalWidth > 0)", "an attachment picture did not load");
    // ^ The text keeps its words and shows no picture of its own (nor an empty paragraph).
    const text = (id) => evaluate(`(() => { const bubble = document.querySelector('${bubbleOf(id)}'); const markdown = bubble.querySelector('.markdown');
      return { words: markdown.textContent.trim(), inline: markdown.querySelectorAll('img, video, .check-capture').length, empty: [...markdown.querySelectorAll('p')].filter((p) => !p.textContent.trim()).length,
        tiles: [...bubble.querySelectorAll('.attachments li')].length, grid: bubble.querySelector('.attachments').hasAttribute('data-grid') }; })()`);
    const [one, three, clip] = [await text(9500000), await text(9500001), await text(9500002)];
    if (one.words !== "Thumbnail of the trend:" || three.words !== "Three more:" || [one, three, clip].some((m) => m.inline || m.empty)
      || one.tiles !== 1 || one.grid || three.tiles !== 3 || !three.grid || clip.tiles !== 2 || !clip.grid) await fail(`the media messages at ${size[0]} px read ${JSON.stringify([one, three, clip])}`);
    // ^ One picture is large (not cropped to a square); three are square tiles in two columns.
    const looks = await evaluate(`(() => { const box = (id) => [...document.querySelectorAll('${bubbleOf(9500000)} .attachments img, ${bubbleOf(9500001)} .attachments img')].map((image) => image.getBoundingClientRect());
      const [single, ...tiles] = box(); return { single: [Math.round(single.width), Math.round(single.height)], squares: tiles.every((tile) => Math.abs(tile.width - tile.height) <= 1),
        columns: new Set(tiles.map((tile) => Math.round(tile.left))).size }; })()`);
    if (looks.single[0] < looks.single[1] || looks.single[0] < 90 || !looks.squares || looks.columns !== 2) await fail(`the tiles at ${size[0]} px read ${JSON.stringify(looks)}`);
    // ^ A chat app fills the tile: every picture or poster is its tile's box (within 1 px); in a grid the tile is square and the picture covers it; one picture keeps its own aspect ratio, within the bubble.
    await until(`[...document.querySelectorAll('${bubbleOf(9500000)} .attachments img')].every((image) => image.complete && image.naturalWidth)`, "the single picture did not load");
    const filled = async (id) => evaluate(`[...document.querySelectorAll('${bubbleOf(id)} .attachments li')].map((tile) => { const media = tile.querySelector('img, .check-frame'), a = tile.getBoundingClientRect(), b = media.getBoundingClientRect();
      return { gap: Math.max(Math.abs(a.width - b.width), Math.abs(a.height - b.height)), fit: media.tagName === 'IMG' ? getComputedStyle(media).objectFit : 'cover', ratio: b.width / b.height,
        natural: media.naturalWidth ? media.naturalWidth / media.naturalHeight : 0, inside: b.width <= tile.closest('.bubble').getBoundingClientRect().width }; })`);
    const [alone, ...grids] = [await filled(9500000), await filled(9500001), await filled(9500002)];
    if (alone.length !== 1 || alone[0].gap > 1 || !alone[0].inside || Math.abs(alone[0].ratio - alone[0].natural) > 0.02
      || grids.flat().some((tile) => tile.gap > 1 || tile.fit !== "cover" || Math.abs(tile.ratio - 1) > 0.02)) await fail(`the tiles at ${size[0]} px do not fill: ${JSON.stringify([alone, ...grids])}`);
    await shot(`message-media-1${wide ? "-wide" : ""}`);
    await evaluate(`document.querySelector('${bubbleOf(9500001)}').scrollIntoView({ block: 'center' })`);
    await shot(`message-media-3${wide ? "-wide" : ""}`);
    // ^ A tap on the first picture of three opens the viewer; an arrow goes to the next of that bubble only.
    await click(`${bubbleOf(9500001)} .attachments a[data-view]`);
    await until("!!document.querySelector('dialog.viewer[open] .viewer-stage img:not([data-loading])')", "the picture of the message did not open in the viewer");
    if ((await opened()) !== "1 of 3") await fail(`the viewer of three pictures reads ${await opened()}`);
    await key("ArrowRight", 39);
    await until("[...document.querySelectorAll('.viewer-bar .time')].some((node) => node.textContent === '2 of 3')", async () => `the viewer did not go to the second picture: ${await opened()}`);
    await shot(`message-media-viewer${wide ? "-wide" : ""}`);
    await key("Escape", 27);
    await until("!document.querySelector('dialog.viewer')", "Escape did not close the viewer");
    // ^ The video tile: poster, play mark and its length; Range answers 206; the viewer plays it and swipes to the picture.
    await evaluate(`document.querySelector('${bubbleOf(9500002)}').scrollIntoView({ block: 'center' })`);
    await until(`/^\\d+:\\d\\d$/.test(document.querySelector('${bubbleOf(9500002)} .check-capture[data-video] .check-duration')?.textContent ?? '')`, "the video tile shows no length");
    const tile = await evaluate(`(async () => { const link = document.querySelector('${bubbleOf(9500002)} .check-capture[data-video]'); const got = await fetch(link.href, { headers: { Range: 'bytes=0-99' } });
      return { play: !!link.querySelector('.check-play svg'), poster: /\\/media\\/[0-9a-f]{64}\\.png$/.test(link.dataset.poster ?? ''), status: got.status, type: got.headers.get('content-type'),
        range: got.headers.get('content-range'), length: (await got.arrayBuffer()).byteLength,
        alts: [...document.querySelectorAll('${bubbleOf(9500002)} .markdown .image-alt')].map((node) => node.textContent),
        links: [...document.querySelectorAll('${bubbleOf(9500002)} .markdown a')].map((anchor) => anchor.textContent) }; })()`);
    if (!tile.play || !tile.poster || tile.status !== 206 || tile.type !== "video/mp4" || !/^bytes 0-99\/\d+$/.test(tile.range || "") || tile.length !== 100
      || tile.alts.join() !== "Secret,Parent,Remote" || tile.links.join() !== "the report") await fail(`the video message at ${size[0]} px reads ${JSON.stringify(tile)}`);
    await shot(`message-media-video${wide ? "-wide" : ""}`);
    await click(`${bubbleOf(9500002)} .check-capture[data-video]`);
    const player = "document.querySelector('dialog.viewer[open] .viewer-stage video')";
    await until(`${player}?.readyState >= 1`, "the message's video did not load in the viewer");
    const played = await evaluate(`(async () => { const video = ${player}; video.muted = true; await video.play(); return { poster: /\\/media\\/[0-9a-f]{64}\\.png$/.test(video.poster), playsinline: video.hasAttribute('playsinline'), size: [video.videoWidth, video.videoHeight] }; })()`);
    if (!played.poster || !played.playsinline || played.size.join() !== "160,90" || (await opened()) !== "1 of 2") await fail(`the message's video in the viewer reads ${JSON.stringify(played)} ${await opened()}`);
    await until(`${player}.currentTime > 0`, "the message's video did not play");
    await key("ArrowRight", 39);
    await until("!!document.querySelector('dialog.viewer[open] .viewer-stage img:not([data-loading])') && !document.querySelector('dialog.viewer .viewer-stage video')", "the viewer did not go from the video to the picture");
    await key("Escape", 27);
    await until("!document.querySelector('dialog.viewer')", "Escape did not close the video viewer");
    // ^ A link to an HTML file opens its sandboxed view (docs/web-client.md "Files").
    await click(`${bubbleOf(9500002)} .markdown a[href$="report.html"]`);
    await until(`location.hash === '#/p/${plansArg}/files/${base}/report.html' && /^view\\/[0-9a-f]{64}\\/report\\.html$/.test(document.querySelector('iframe.file-page')?.getAttribute('src') ?? '')`, "the report link did not open the page view");
    if ((await evaluate("document.querySelector('iframe.file-page').getAttribute('sandbox')")) !== "allow-popups allow-popups-to-escape-sandbox") await fail("the report page is not sandboxed");
  }
  // --- The Media sheet (docs/web-client-tasks.md "Media"): the project's captures (a passed image, a skipped
  // video with a poster, a failed item whose file the budget removed), the files the messages above link (a video
  // with no jpg beside it shows its first frame; a file gone from the checkout says so) and the owner's uploads (one
  // the store has, one it no longer has), newest first, with kind and task filters; a tile opens the viewer.
  const tasksDir = path.dirname(path.dirname(path.join(state, logFile)));
  const checksFile = path.join(tasksDir, "checks.jsonl");
  const checksBefore = fs.existsSync(checksFile) ? fs.readFileSync(checksFile) : null;
  const kept = path.join(tasksDir, "1", "captures");
  fs.mkdirSync(kept, { recursive: true });
  fs.writeFileSync(path.join(kept, "r1-1.png"), drawn[1]);
  fs.copyFileSync(path.join(folder, "clip.mp4"), path.join(kept, "r1-2.mp4"));
  fs.writeFileSync(path.join(kept, "r1-2.poster.png"), drawn[2]);
  // ^ A portrait capture (240 x 320) fills its square tile, it does not stretch it to its own height.
  const tall = await evaluate(`(() => { const canvas = Object.assign(document.createElement('canvas'), { width: 240, height: 320 }); const pen = canvas.getContext('2d');
    pen.fillStyle = '#7a3d8c'; pen.fillRect(0, 0, 240, 320); pen.fillStyle = '#ffffff'; pen.fillRect(120, 220, 120, 100); return canvas.toDataURL('image/png'); })()`);
  fs.writeFileSync(path.join(kept, "r1-4.png"), Buffer.from(tall.split(",")[1], "base64"));
  const now = Date.now();
  const item = (id, check, state, capture, poster, at) => JSON.stringify({ id, task: 1, revision: 1, commit: "0".repeat(40), check, pass_if: "it reads well", state, capture, ...(poster && { poster }), unix_ms: at });
  fs.appendFileSync(checksFile, [item("c900", "the chart reads", "passed", "captures/r1-1.png", null, now - 5000), item("c901", "the clip plays", "skipped", "captures/r1-2.mp4", "captures/r1-2.poster.png", now - 4000),
    item("c902", "the old shot", "failed", "captures/r1-3.png", null, now - 3000), item("c903", "the tall shot", "passed", "captures/r1-4.png", null, now - 2000)].join("\n") + "\n");
  fs.copyFileSync(path.join(folder, "clip.mp4"), path.join(folder, "bare.mp4"));
  reply(9_500_003, `A clip with no picture beside it: ![Bare](files/${base}/bare.mp4)`);
  reply(9_500_004, `A shot that is gone: ![Gone](files/${base}/gone.jpg)`);
  const uploaded = await evaluate(`fetch('api/media?name=up.png', { method: 'PUT', body: Uint8Array.from(atob(${JSON.stringify(drawn[3].toString("base64"))}), (c) => c.charCodeAt(0)) }).then((r) => r.json())`);
  const mediaFile = fs.readdirSync(state, { recursive: true }).find((file) => file.endsWith(`/media/${uploaded.hash}.png`));
  if (!mediaFile) await fail(`the upload ${uploaded.hash} is in no media folder under ${state}`);
  const mediaDir = path.join(state, path.dirname(mediaFile));
  const owner = (id, text) => fs.appendFileSync(path.join(state, logFile), `${JSON.stringify({ unix_ms: Date.now(), kind: "message", id, from: "owner", text })}\n`);
  owner(9_500_005, `Look at this\n\n[attached files]\n${mediaDir}/${uploaded.hash}.png (up.png)`);
  owner(9_500_006, `And this\n\n[attached files]\n${mediaDir}/${"a".repeat(64)}.png (old.png)`);
  const tiles = "document.querySelectorAll('dialog.task-sheet[open] .media-tile')";
  const sheetTiles = () => evaluate(`[...${tiles}].map((tile) => { const link = tile.querySelector('a.check-capture'); const media = tile.querySelector('img, video'); const box = tile.getBoundingClientRect();
    return { name: tile.querySelector('.media-name').textContent, facts: tile.querySelector('.media-facts').textContent, task: tile.querySelector('.task-chip')?.textContent ?? '', video: !!link?.hasAttribute('data-video'),
      poster: link?.dataset.poster ?? '', frame: media?.tagName === 'VIDEO', gap: !link, shown: media ? (media.tagName === 'IMG' ? media.naturalWidth > 0 : media.readyState >= 1 && media.videoWidth > 0) : false,
      square: media ? Math.abs(media.getBoundingClientRect().width - media.getBoundingClientRect().height) <= 1 : null, inside: box.width > 60 }; })`);
  for (const wide of [false, true]) {
    await resize(wide ? [1280, 800, 1] : [390, 844, 2]);
    await go(`#/p/${plansArg}/media`);
    await until(`${tiles}.length >= 13`, "the Media sheet did not list the fixtures", 60);
    await until(`[...document.querySelectorAll('.media-tile img')].every((image) => image.complete)`, "a media picture did not load", 60);
    await until(`[...document.querySelectorAll('.media-tile video')].every((video) => video.readyState >= 1)`, "a media video shows no first frame", 60);
    const all = await sheetTiles();
    const named = (name) => all.filter((tile) => tile.name === name);
    const [chart] = named("the chart reads"), [clip] = named("the clip plays"), [old] = named("the old shot"), [bare] = named("bare.mp4"), [gone] = named("gone.jpg"), [up] = named("up.png"), [expired] = named("old.png");
    if (!chart?.shown || !/^Passed/.test(chart.facts) || !/#1$/.test(chart.task) || chart.video) await fail(`the passed capture reads ${JSON.stringify(chart)}`);
    if (!clip?.video || !/\/checks\/c901\/poster$/.test(clip.poster) || !/^Skipped/.test(clip.facts)) await fail(`the skipped video reads ${JSON.stringify(clip)}`);
    if (!old?.gap || !/^Removed · Failed/.test(old.facts)) await fail(`the removed capture reads ${JSON.stringify(old)}`);
    if (!bare?.video || !bare.frame || bare.poster || !bare.shown) await fail(`the video with no poster reads ${JSON.stringify(bare)}`);
    if (!gone?.gap || !/^Removed/.test(gone.facts)) await fail(`the file gone from the checkout reads ${JSON.stringify(gone)}`);
    if (!up?.shown || !/^Upload/.test(up.facts) || !expired?.gap || !/^Expired/.test(expired.facts)) await fail(`the uploads read ${JSON.stringify([up, expired])}`);
    if (all.filter((tile) => tile.name === "trend.jpg").length !== 3 || all.some((tile) => /^\.\.|\.env/.test(tile.name))) await fail(`the linked files read ${JSON.stringify(all.map((tile) => tile.name))}`);
    // ^ Newest first: the two uploads, the gone shot and the bare clip come before the older replies; the pictures fill square tiles.
    const order = all.map((tile) => tile.name);
    if (order.indexOf("old.png") > order.indexOf("up.png") || order.indexOf("up.png") > order.indexOf("gone.jpg") || order.indexOf("gone.jpg") > order.indexOf("bare.mp4") || order.indexOf("bare.mp4") > order.indexOf("the old shot")) await fail(`the Media sheet is not newest first: ${order}`);
    if (all.filter((tile) => !tile.gap).some((tile) => !tile.square)) await fail(`a media tile is not square at ${size[0]} px: ${JSON.stringify(all)}`);
    const columns = await evaluate(`new Set([...${tiles}].map((tile) => Math.round(tile.getBoundingClientRect().left))).size`);
    if (wide ? columns < 3 : columns !== 2) await fail(`the grid has ${columns} columns at ${size[0]} px`);
    await shot(wide ? "media-sheet-wide" : "media-sheet");
    // ^ Filters: Videos keeps only videos, Images none, the task keeps the three captures.
    const chip = (name) => evaluate(`[...document.querySelectorAll('.media-chip')].find((button) => button.textContent === ${JSON.stringify(name)}).click()`);
    await chip("Videos");
    await until(`${tiles}.length > 0 && [...${tiles}].every((tile) => tile.querySelector('[data-video]'))`, "the Videos filter shows a picture");
    await chip("Images");
    await until(`${tiles}.length > 0 && [...${tiles}].every((tile) => !tile.querySelector('[data-video]'))`, "the Images filter shows a video");
    await chip("All");
    await until(`${tiles}.length >= 13`, "All did not bring the list back");
    await evaluate(`(() => { const select = document.querySelector('.media-task select'); select.value = '1'; select.dispatchEvent(new Event('change', { bubbles: true })); })()`);
    await until(`${tiles}.length === 4 && [...${tiles}].every((tile) => /#1$/.test(tile.querySelector('.task-chip')?.textContent ?? ''))`, "the task filter does not keep the task's four captures");
    await evaluate(`(() => { const select = document.querySelector('.media-task select'); select.value = ''; select.dispatchEvent(new Event('change', { bubbles: true })); })()`);
    await until(`${tiles}.length >= 13`, "the task filter did not clear");
    // ^ A video tile opens the viewer and plays; the poster-less clip plays too.
    await click("dialog.task-sheet[open] .media-tile .check-capture[data-video]:not([data-poster])");
    const clipPlayer = "document.querySelector('dialog.viewer[open] .viewer-stage video')";
    await until(`${clipPlayer}?.readyState >= 1`, "the media video did not load in the viewer");
    await shot(wide ? "media-sheet-viewer-wide" : "media-sheet-viewer");
    await key("Escape", 27);
    await until("!document.querySelector('dialog.viewer')", "Escape did not close the viewer of the media video");
  }
  // ^ The fixtures go: the checks list as it was and the task's capture folder.
  if (checksBefore) fs.writeFileSync(checksFile, checksBefore); else fs.rmSync(checksFile);
  fs.rmSync(kept, { recursive: true });
  fs.rmSync(path.join(plansCheckout, ".studio-data"), { recursive: true });
  fs.rmSync(path.join(plansCheckout, ".env"));
  await go(`#/p/${plansArg}`);
}
console.log(`tasks page: home with the Sessions tab in the first screen and no Projects tab, the plain worktree on its project's worktree list and no Adopted list, since-you-left, bubbles (sides, authors, grouping, worker results as muted lines), no cards/tray/strip/Explain, one working color on home, the session rows (only working marks pulse, none under reduced motion), the reservoir and a typing manager, the reservoir (the closed line naming running #6 without \"Owner asked:\", rows with one action, Running, To check, no facts, Answer, Approve and Land cleared in place with the focus on the next row and the chat still), the ⋯ menu, the project overview (facts, markdown, mermaid, its edit by the manager, the reply's link), typing while a turn runs, short chips, a table inside its bubble, the task sheet (a row and a chip open it without a chat scroll, 85%, Escape, Back, swipe, draft kept, three stages, one sentence with More, folds with their notes, no steps, verdict, Review or revision, Land, the item live after landing, History, approval in the sentence, Deny, an answer, Shared files only in a conflict), done lines out of the reservoir, To check (its row, Fail closing the item and its reply kept as the note, Pass in a bubble that stays in place, Skip), the manager chat's attach, plans' run of events folded into one row (its chips, count, last event under it, a tap to open and to fold, an owner message ending the run), the other manager's bubbles and each speaker's color in both schemes, Try on a To check item, the # picker, the deep link (one read of the tasks), a waiting task (reservoir row with Answer, its story and field, the answer as a bubble, Land and Land cleared until the reservoir goes and the focus is on the composer), the wide panel and page, Settings > Notifications (empty, refused permission, saved, failed save kept, a session muted from its menu and unmuted, all off), Chats (a Home row under its project with its unread dot, none in the ⋯ menu, its chat at #/p/<project>/c/<chat> without the reservoir, a message answered, Back), the new-version pill and its reload with the route and the draft; ${audits.length} captures`);
await linter.finish(fail);
await cleanup();
process.exit(0);
