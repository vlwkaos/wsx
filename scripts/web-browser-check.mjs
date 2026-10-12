#!/usr/bin/env node
// Real-browser check of the page: every screen and agent state at three sizes, driven
// over the Chrome DevTools Protocol. Called by `test-web-client.py --browser`.
//   tour <site-url> <code> <empty-site-url> <code> <sessions-json> <capture-dir> [slop-audit.js]
//   down <site-url> <code> <capture-dir> [slop-audit.js]     (wsxd is stopped)
// Each capture is saved with the slop audit of /ui-view-standards beside it.
import fs from "node:fs";
import path from "node:path";
import { launch } from "./web-cdp.mjs";

const [mode, siteUrl, code, ...rest] = process.argv.slice(2);
const [emptyUrl, emptyCode, sessionsJson, captures, auditPath] = mode === "tour" ? rest : [null, null, "{}", ...rest];
const ids = JSON.parse(sessionsJson);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({
  captures, prefix: "", timeoutSeconds: mode === "tour" ? 260 : 100,
});
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const auditSource = auditPath ? fs.readFileSync(auditPath, "utf8") : null;
const SIZES = [[320, 568, 2], [390, 844, 2], [1280, 800, 1]];
let size = SIZES[0];
const phone = () => size[0] < 600;

// One capture with its slop audit. A screen may not scroll sideways and may show at
// most one filled accent control.
async function shot(name, primary) {
  const label = `${name} at ${size[0]} px`;
  if (auditSource) {
    await evaluate(`globalThis.SLOP_AUDIT_PRIMARY = ${JSON.stringify(primary ?? null)}`);
    const audit = await evaluate(auditSource);
    fs.writeFileSync(path.join(captures, `${size[0]}-${name}.slop.json`), JSON.stringify(audit, null, 1));
    if (audit.horizontalOverflow) await fail(`${label} overflows sideways`);
    if (audit.primaryAction.filledInViewport > 1) await fail(`${label} shows ${audit.primaryAction.filledInViewport} filled accent controls: ${JSON.stringify(audit.primaryAction.filled)}`);
    if (primary && !audit.primaryAction.inFirstViewport) await fail(`${label}: the primary action is not in view`);
  }
  await capture(`${size[0]}-${name}`);
}
// Layout rules of every screen: nothing past the right edge, no raw null, one scroll container.
async function sound(label) {
  const seen = await evaluate(`(() => {
    const free = (node) => node.closest('.crumbs, .terminal-keys, .code, .xterm');
    const wide = [...document.querySelectorAll('#app *')].filter((node) => !free(node) && node.getBoundingClientRect().right > innerWidth + 0.5);
    const scrollers = [...document.querySelectorAll('#app *')].filter((node) => node.tagName !== 'TEXTAREA' && !free(node)
      && /auto|scroll/.test(getComputedStyle(node).overflowY) && node.scrollHeight > node.clientHeight + 1);
    return { wide: wide.slice(0, 3).map((node) => node.className || node.tagName), scrollers: scrollers.map((node) => node.className),
      raw: /\\b(null|undefined|NaN)\\b/.test(document.querySelector('#app').innerText), page: document.scrollingElement.scrollTop + document.scrollingElement.scrollLeft };
  })()`);
  const where = `${label} at ${size[0]} px`;
  if (seen.wide.length) await fail(`${where}: elements pass the right edge: ${seen.wide}`);
  if (seen.scrollers.length > 1) await fail(`${where}: more than one scroll container: ${seen.scrollers}`);
  if (seen.raw) await fail(`${where} shows a raw null`);
  if (seen.page) await fail(`${where}: the page itself scrolled`);
}
// A focused text field on a phone must not make the browser zoom or pan:
// iOS zooms when the field's font is below 16 px.
async function steadyOnFocus(selector, label) {
  const seen = await evaluate(`(() => { const field = document.querySelector(${JSON.stringify(selector)}); field.focus();
    return { focused: document.activeElement === field, font: parseFloat(getComputedStyle(field).fontSize),
      scale: visualViewport.scale, left: document.scrollingElement.scrollLeft, top: document.scrollingElement.scrollTop }; })()`);
  if (!seen.focused || (phone() && seen.font < 16) || seen.scale !== 1 || seen.left !== 0 || seen.top !== 0) {
    await fail(`${label} zooms or scrolls the page on focus at ${size[0]} px: ${JSON.stringify(seen)}`);
  }
}
const go = (hash) => evaluate(`location.hash = ${JSON.stringify(hash)}`);
const text = (selector) => evaluate(`[...document.querySelectorAll(${JSON.stringify(selector)})].map((node) => node.textContent.trim().replace(/\\s+/g, ' '))`);
const count = (selector) => evaluate(`document.querySelectorAll(${JSON.stringify(selector)}).length`);
async function key(name, keyCode, options = {}) {
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: name, code: name, windowsVirtualKeyCode: keyCode, ...options });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: name, code: name, windowsVirtualKeyCode: keyCode, ...options });
}
async function pair(url, pairingCode) {
  await send("Page.navigate", { url });
  await until("!!document.querySelector('#pair-code')", "pairing form did not render");
  await sound("pairing form");
  await steadyOnFocus("#pair-code", "the pairing code field");
  await steadyOnFocus("#pair-name", "the device name field");
  await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(pairingCode)}; input.form.requestSubmit(); })()`);
  await until("!!document.querySelector('.app')", "the app did not open after pairing");
  if (await evaluate("document.cookie.includes('wsx_web')")) await fail("credential cookie is readable by scripts");
}
async function resize(next) {
  size = next;
  await viewport(...next);
  await pause(250);
}

if (mode === "down") {
  await resize(SIZES[1]);
  await pair(siteUrl, code);
  for (const next of SIZES) {
    await resize(next);
    await until("document.querySelector('#app').innerText.includes('wsx is not running on the host')", "the page does not say that wsx is stopped");
    await sound("daemon unavailable");
    await shot("daemon-down");
  }
  console.log("page with wsxd stopped: says so at 320, 390 and 1280 px");
  await cleanup();
  process.exit(0);
}

async function inbox() {
  await go("#/");
  await until("document.querySelectorAll('.row').length >= 8", "the inbox did not list the sessions");
  const labels = await text(".label");
  if (labels.join() !== "Needs you,Running,Finished,Sessions") await fail(`inbox sections are ${labels}`);
  const sections = await evaluate(`Object.fromEntries([...document.querySelectorAll('section')].map((section) => [section.getAttribute('aria-label'),
    [...section.querySelectorAll('.row')].map((row) => ({ label: row.querySelector('.row-label').textContent, context: row.querySelector('.row-context').textContent,
      state: row.querySelector('.row-state').textContent.trim().replace(/\\s+/g, ' '), activity: row.querySelector('.row-activity')?.textContent ?? null,
      clipped: row.querySelector('.row-label').scrollWidth > row.querySelector('.row-label').clientWidth })) ]))`);
  const states = (name) => sections[name].map((row) => row.state.replace(/ ?\d.*$/, "")).sort().join();
  if (states("Needs you") !== "error,exited,needs input") await fail(`Needs you shows ${JSON.stringify(sections["Needs you"])}`);
  const running = sections.Running[0];
  if (sections.Running.length !== 1 || running.activity !== "List the files" || !/^working ?\d+[smh]/.test(running.state)) await fail(`Running shows ${JSON.stringify(sections.Running)}`);
  if (running.context !== "design · main · claude") await fail(`the context line is ${running.context}`);
  if (!sections.Finished.some((row) => /^done new$/.test(row.state))) await fail(`Finished marks nothing as new: ${JSON.stringify(sections.Finished)}`);
  if (states("Sessions") !== "idle,shell") await fail(`Sessions shows ${JSON.stringify(sections.Sessions)}`);
  if (!sections["Needs you"].some((row) => row.clipped)) await fail("the long session label is not truncated with an ellipsis");
  if (!phone() && Math.round(await evaluate("document.querySelector('.rail').getBoundingClientRect().width")) !== 300) await fail("the rail is not 300 px wide");
  if ((await text(".group-name")).join() !== "design" || !(await count(".group-head .link-action"))) await fail("the Sessions group lacks its project or its New session action");
  // The elapsed time of a working session counts on the page.
  const before = await text("section[aria-label=Running] .time");
  await pause(1200);
  if ((await text("section[aria-label=Running] .time")).join() === before.join()) await fail("the elapsed time does not count");
  if (phone()) {
    if ((await text(".tabbar a")).join() !== "Inbox,Projects,Settings" || (await text(".tabbar a[aria-current=page]")).join() !== "Inbox") await fail("the tab bar is wrong");
    const bar = await evaluate("(() => { const r = document.querySelector('.tabbar').getBoundingClientRect(); return [r.bottom, innerHeight, r.height]; })()");
    if (Math.abs(bar[0] - bar[1]) > 1 || bar[2] < 44) await fail(`the tab bar is not at the bottom edge: ${bar}`);
  } else if ((await count(".tabbar")) || !(await count(".rail")) || (await text(".placeholder")).join() !== "Choose a session.") {
    await fail("the wide layout lacks its rail or shows a tab bar");
  }
  await sound("inbox");
  await shot("inbox");
}

async function openSession(role, tab = "chat") {
  await go(`#/s/${ids[role]}/${tab}`);
  await until("!!document.querySelector('.session .bar-label')", `the ${role} session did not open`);
  if (phone() && (await count(".tabbar"))) await fail("the tab bar shows inside a session");
  const bar = await evaluate("document.querySelector('.bar').getBoundingClientRect().height");
  if (bar > 48) await fail(`the session top row is ${bar} px high`);
}

async function chats() {
  // Working: a running tool, the elapsed time, Stop, and a message that waits.
  await openSession("working");
  await until("!!document.querySelector('.working') && !!document.querySelector('.queued')", "the working row or the queue did not show");
  const working = await evaluate(`({ state: document.querySelector('.bar-state').textContent, activity: document.querySelector('.working-activity').textContent,
    live: [...document.querySelectorAll('.steps[data-live] .step')].map((step) => step.textContent.trim().replace(/\\s+/g, ' ')),
    send: document.querySelector('.composer .button-primary').textContent, stop: [...document.querySelectorAll('.working button')].map((b) => b.textContent).join(),
    queued: document.querySelector('.queued-text').textContent, switch: [...document.querySelectorAll('.switch a')].map((a) => a.textContent.trim() + (a.getAttribute('aria-current') ? '*' : '')).join(),
    pulse: getComputedStyle(document.querySelector('.pulse')).animationName })`);
  if (working.state !== "working" || working.activity !== "List the files" || working.send !== "Queue" || working.stop !== "Stop"
    || working.queued !== "Also mention the new terminal in the notes" || working.switch !== "Chat*,Terminal"
    || !working.live.some((line) => /^List the files ?running$/.test(line)) || !working.live.includes("Thinking") || working.pulse !== "pulse") {
    await fail(`the working chat is wrong: ${JSON.stringify(working)}`);
  }
  await sound("working chat");
  await shot("chat-working", ".composer .button-primary");

  // A long conversation: opens at its end; scrolled up, a jump button shows.
  await openSession("long");
  await until("document.querySelectorAll('.message').length > 40", "the long conversation did not load");
  const long = await evaluate(`(() => { const log = document.querySelector('.log');
    return { end: log.scrollHeight - log.scrollTop - log.clientHeight, wide: log.scrollWidth - log.clientWidth,
      steps: [...document.querySelectorAll('details.steps > summary')].slice(-4).map((node) => node.textContent.trim()),
      user: Math.round(document.querySelector('.message-user').getBoundingClientRect().width / log.clientWidth * 100),
      jump: !!document.querySelector('.jump'), labels: document.querySelector('.log').innerText.includes('Agent') }; })()`);
  if (long.end > 2 || long.wide > 0 || long.jump || long.labels) await fail(`the long chat does not open at its end, or a 300-character word widens it: ${JSON.stringify(long)}`);
  if (!long.steps.every((line) => /^\d+ steps?: [A-Z]/.test(line)) || long.user > 86) await fail(`steps or the user message are wrong: ${JSON.stringify(long)}`);
  await evaluate("(() => { const log = document.querySelector('.log'); log.scrollTop = log.scrollHeight / 2; log.dispatchEvent(new Event('scroll')); })()");
  await until("!!document.querySelector('.jump')", "no jump button after scrolling up");
  await sound("long chat scrolled up");
  await shot("chat-long-scrolled");
  await evaluate("document.querySelector('.jump').click()");
  await until("!document.querySelector('.jump') && (() => { const log = document.querySelector('.log'); return log.scrollHeight - log.scrollTop - log.clientHeight < 2; })()", "Jump to latest did not go to the end");
  // A disclosure lists each step with its state word; a failed call says so.
  await evaluate("[...document.querySelectorAll('details.steps')].forEach((node) => { node.open = true; })");
  if (!(await count(".step-state[data-tone=danger]")) || !(await count(".step-title"))) await fail("an opened steps line does not list its steps, or a failed call is not marked");

  // Blocked: a persistent row, a way to the terminal, and no sending.
  await openSession("blocked");
  await until("!!document.querySelector('.attention')", "the blocked row did not show");
  const blocked = await evaluate(`({ row: document.querySelector('.attention').textContent.trim().replace(/\\s+/g, ' '), link: document.querySelector('.attention a').getAttribute('href'),
    disabled: document.querySelector('.composer-field').disabled && document.querySelector('.composer .button-primary').disabled,
    label: (() => { const node = document.querySelector('.bar-label'); return node.scrollWidth > node.clientWidth; })(), state: document.querySelector('.bar-state').textContent })`);
  if (!blocked.row.startsWith("The agent is waiting for your answer.") || !blocked.link.endsWith("/terminal") || !blocked.disabled || (phone() && !blocked.label) || blocked.state !== "needs input") {
    await fail(`the blocked chat is wrong: ${JSON.stringify(blocked)}`);
  }
  await sound("blocked chat");
  await shot("chat-blocked");

  // Empty: one sentence, and the composer ready.
  await openSession("idle");
  await until("!!document.querySelector('.composer-field')", "the idle chat did not show");
  if (size !== SIZES[2] || !(await count(".message"))) {
    await until("!!document.querySelector('.log .empty')", "the empty chat does not say so");
    if (!(await text(".log .empty")).join().startsWith("No messages yet")) await fail("the empty chat does not say so");
  }
  if ((await text(".composer .button-primary")).join() !== "Send") await fail("the send button of an idle agent does not read Send");
  await sound("empty chat");
  await shot("chat-empty", ".composer .button-primary");
  await steadyOnFocus(".composer-field", "the chat field");
  await send("Input.insertText", { text: "line one" });
  const value = () => evaluate("document.querySelector('.composer-field').value");
  if (phone()) {
    // A touch keyboard's Enter is a newline; nothing is sent.
    await key("Enter", 13, { text: "\r" });
    if ((await value()) !== "line one\n" || (await count(".message"))) await fail(`Enter on a touch device did not add a newline: ${JSON.stringify(await value())}`);
    await evaluate("(() => { const field = document.querySelector('.composer-field'); field.value = ''; field.dispatchEvent(new Event('input', { bubbles: true })); })()");
  } else {
    // A hardware keyboard: Shift+Enter is a newline, a key of an IME composition never sends, Enter sends.
    await key("Enter", 13, { text: "\r", modifiers: 8 });
    if ((await value()) !== "line one\n") await fail(`Shift+Enter did not add a newline: ${JSON.stringify(await value())}`);
    await key("Backspace", 8);
    await evaluate("document.querySelector('.composer-field').dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', isComposing: true, bubbles: true, cancelable: true }))");
    await pause(200);
    if ((await value()) !== "line one" || (await count(".message"))) await fail("Enter during an IME composition sent the message");
    await key("Enter", 13, { text: "\r" });
    await until("[...document.querySelectorAll('.message:not(.message-user) .prose')].some((node) => node.textContent === 'echo: line one')", "Enter did not send, or the reply did not arrive");
    if ((await value()) !== "") await fail("the field kept its text after sending");
  }
}

async function others() {
  await go("#/projects");
  await until("!!document.querySelector('.row-actions')", "the projects screen did not open");
  const project = await evaluate(`(() => { const row = document.querySelector('.row-actions').closest('.row');
    return { row: row.querySelector('.row-label').textContent, count: row.querySelector('.row-context').textContent, actions: [...row.querySelectorAll('.row-actions > *')].map((n) => n.textContent).join() }; })()`);
  if (project.row !== "design" || !/^\d+ sessions$/.test(project.count) || project.actions !== "Files,New session") await fail(`the projects screen is wrong: ${JSON.stringify(project)}`);
  await sound("projects");
  await shot("projects");
  await evaluate("document.querySelector('.row-actions a').click()");
  await until("[...document.querySelectorAll('.files .row-label')].some((node) => node.textContent === 'README.md')", "the files screen did not list the project");
  await evaluate("[...document.querySelectorAll('.files .row-link')].find((node) => node.textContent.trim() === 'README.md').click()");
  await until("!!document.querySelector('textarea.code')", "the file did not open");
  await steadyOnFocus("textarea.code", "the file editor");
  await sound("files");
  await shot("files", ".files .button-primary");
  await go("#/settings");
  await until("!!document.querySelector('input[name=scheme]')", "settings did not open");
  await sound("settings");
  await shot("settings");
}

// --- The tour.
await resize(SIZES[0]);
await pair(siteUrl, code);
const background = await evaluate("getComputedStyle(document.body).backgroundColor");
if (background !== "rgb(10, 11, 13)") await fail(`the page is not dark by default: ${background}`);
for (const next of SIZES) {
  await resize(next);
  await inbox();
  await chats();
  await others();
}

// Once, at 390 px: the device's light choice, the dialog, reduced motion, the installed shell.
await resize(SIZES[1]);
await evaluate("document.querySelector('input[name=scheme][value=light]').click()");
if ((await evaluate("getComputedStyle(document.body).backgroundColor")) !== "rgb(247, 247, 248)" || (await evaluate("document.documentElement.dataset.scheme")) !== "light") await fail("choosing light did not change the scheme");
await sound("settings in the light scheme");
await shot("settings-light");
await go("#/");
await until("document.querySelectorAll('.row').length >= 8", "the inbox did not open in the light scheme");
await shot("inbox-light");
await go("#/settings");
await until("!!document.querySelector('input[name=scheme]')", "settings did not reopen");
await evaluate("document.querySelector('input[name=scheme][value=dark]').click()");
if ((await evaluate("getComputedStyle(document.body).backgroundColor")) !== background) await fail("choosing dark did not restore the dark scheme");
// The unpair confirmation: opened by keyboard, focus on Cancel, Escape closes it, focus returns.
await evaluate("document.querySelector('.button-danger').focus()");
await key("Enter", 13, { text: "\r" });
await until("!!document.querySelector('dialog.sheet[open]')", "the dialog did not open");
if ((await evaluate("document.activeElement.textContent")) !== "Cancel") await fail("dialog focus is not on Cancel");
await shot("dialog");
await key("Escape", 27);
await until("!document.querySelector('dialog')", "Escape did not close the dialog");
if (!(await evaluate("document.activeElement.classList.contains('button-danger')"))) await fail("focus did not return to the opener");
if (!(await evaluate("fetch('api/session').then((r) => r.ok)"))) await fail("cancel unpaired the device");

await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
await openSession("working");
await until("!!document.querySelector('.pulse')", "the working row did not show");
if ((await evaluate("getComputedStyle(document.querySelector('.pulse')).animationName")) !== "none") await fail("reduced motion is not respected");
await send("Emulation.setEmulatedMedia", { features: [] });

// Sub-path surfaces as the browser resolved them.
const facts = await evaluate(`(async () => {
  const registration = await navigator.serviceWorker.ready;
  return { scope: registration.scope, requests: performance.getEntriesByType('resource').map((entry) => new URL(entry.name).pathname) };
})()`);
const base = new URL(siteUrl).pathname;
if (facts.scope !== siteUrl) await fail(`service worker scope is ${facts.scope}`);
const stray = facts.requests.filter((request) => !request.startsWith(base));
if (stray.length) await fail(`requests left the base path: ${stray.join(", ")}`);
if (!facts.requests.includes(`${base}api/session`)) await fail("no API request under the base path");

// The first, unpaired load answers 401 by design; any other console error is a defect.
const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);

// A site with no session: one sentence and the create action.
await pair(emptyUrl, emptyCode);
for (const next of SIZES) {
  await resize(next);
  await until("(document.querySelector('.empty')?.textContent ?? '').includes('No session')", "the empty inbox did not show");
  const empty = (await text(".empty")).join();
  if (empty !== "No session is open. Start one") await fail(`the empty inbox says ${JSON.stringify(empty)}`);
  await sound("empty inbox");
  await shot("inbox-empty");
}

// Offline, last: the installed shell of the first site still loads and says what is wrong.
await resize(SIZES[1]);
await send("Page.navigate", { url: siteUrl });
await until("!!document.querySelector('.app')", "the first site did not reopen");
await evaluate("navigator.serviceWorker.ready.then(() => true)");
await send("Network.emulateNetworkConditions", { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0 });
await send("Page.reload");
await until("/Cannot reach the server/.test(document.querySelector('#app')?.textContent ?? '')", "offline shell did not render");
await shot("offline");

console.log("page at 320x568, 390x844 and 1280x800: inbox with a session in each state, working chat with a running tool and a waiting message, long chat with jump button, blocked, empty chat and its Enter rules, projects, files, settings, light scheme, dialog, reduced motion, base-path requests only, offline shell, empty inbox; no sideways overflow, one scroll container, at most one filled accent control");
await cleanup();
process.exit(0);
