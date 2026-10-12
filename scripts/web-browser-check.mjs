#!/usr/bin/env node
// Real-browser check for the hosted web client, driven over the Chrome DevTools
// Protocol with Node's built-in WebSocket. Called by `test-web-client.py --browser`.
// Usage: node web-browser-check.mjs <site-url> <pairing-code> <session-id> <capture-dir> <site-id> <panels> <body-color>
import { launch } from "./web-cdp.mjs";

const [siteUrl, code, sessionId, captures, site, panels, bodyColor] = process.argv.slice(2);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({
  captures, prefix: `${site}-`, timeoutSeconds: 150,
});
// A layout check for the state on screen: no sideways scroll, no leaked "null".
async function sound(label) {
  if (!(await evaluate(noOverflow))) await fail(`${label} overflows horizontally`);
  if (await evaluate("/\\b(null|undefined)\\b/.test(document.body.innerText)")) await fail(`${label} shows a raw null`);
}
const noOverflow = "document.documentElement.scrollWidth <= window.innerWidth";

// 1. Unpaired device at 320 px: the pairing form, no horizontal scroll.
await viewport(320, 640);
await send("Page.navigate", { url: siteUrl });
await until("!!document.querySelector('#pair-code')", "pairing form did not render");
await sound("pairing form at 320 px");
await capture("pair-320");

// 2. Pair by keyboard-equivalent form submit; the browser stores the HttpOnly cookie itself.
await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)};
  input.form.requestSubmit(); })()`);
await until("!!document.querySelector('.composer textarea')", "chat did not open after pairing");
if (await evaluate("document.cookie.includes('wsx_web')")) await fail("credential cookie is readable by scripts");
await until(`[...document.querySelectorAll('select option')].some((o) => o.value === ${JSON.stringify(sessionId)})`,
  "the event stream did not deliver the session list");

// 3. Send a message; the reply must arrive through the event stream, with no reload.
const sentinel = `browser check ${Date.now()}`;
await evaluate(`(() => { const box = document.querySelector('.composer textarea'); box.value = ${JSON.stringify(sentinel)};
  box.dispatchEvent(new Event('input')); document.querySelector('.composer .button-primary').click(); })()`);
await until(`[...document.querySelectorAll('.bubble-agent .text')].some((n) => n.textContent === ${JSON.stringify("echo: " + sentinel)})`,
  "the agent reply did not appear");
await until("!document.querySelector('.bubble[data-pending]')", "the sent message stayed pending");
const statusText = await evaluate("document.querySelector('p[role=status].small').textContent");
if (!/not confirmed/.test(statusText)) await fail(`delivery status overstates evidence: ${statusText}`);
await sound("chat at 320 px");
await capture("chat-320");

// 4. Sub-path surfaces as the browser resolved them.
const facts = await evaluate(`(async () => {
  const registration = await navigator.serviceWorker.ready;
  const requests = performance.getEntriesByType('resource').map((entry) => new URL(entry.name).pathname);
  return {
    scope: registration.scope,
    manifest: document.querySelector('link[rel=manifest]').href,
    requests,
    nav: [...document.querySelectorAll('#nav a')].map((a) => a.textContent),
    focusable: document.querySelector('.composer textarea').getAttribute('aria-label'),
  };
})()`);
const base = new URL(siteUrl).pathname;
if (facts.scope !== siteUrl) await fail(`service worker scope is ${facts.scope}`);
const stray = facts.requests.filter((request) => !request.startsWith(base));
if (stray.length) await fail(`requests left the base path: ${stray.join(", ")}`);
if (!facts.requests.includes(`${base}api/session`)) await fail("no API request under the base path");
if (facts.nav.join(",") !== panels) await fail(`navigation is ${facts.nav}, the site file says ${panels}`);

// 5. The theme the site file selected is the one on screen.
const painted = await evaluate("getComputedStyle(document.body).backgroundColor");
if (painted !== bodyColor) await fail(`body background is ${painted}, the theme says ${bodyColor}`);

// 6. Reduced motion, then every panel at 320 px and 1280 px through its nav link.
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
if ((await evaluate("getComputedStyle(document.querySelector('.log')).scrollBehavior")) !== "auto") await fail("reduced motion is not respected");
await send("Emulation.setEmulatedMedia", { features: [] });
const click = (selector, text) => evaluate(`[...document.querySelectorAll(${JSON.stringify(selector)})].find((n) => n.textContent === ${JSON.stringify(text)}).click()`);
async function both(name, label) {
  for (const [width, height] of [[320, 640], [1280, 800]]) {
    await viewport(width, height);
    await new Promise((resolve) => setTimeout(resolve, 250));
    await sound(`${label} at ${width} px`);
    await capture(`${name}-${width}`);
  }
}
await both("chat", "chat");
for (const panel of panels.split(",").slice(1)) {
  await click("#nav a", panel);
  await until(`document.querySelector('#nav a[aria-current=page]')?.textContent === ${JSON.stringify(panel)} && document.querySelector('#main').children.length > 0`,
    `${panel} did not open`);
  await both(panel.toLowerCase(), panel);
  if (panel === "Files") {
    // Shared toast component: save a file and capture the result notice.
    await until("[...document.querySelectorAll('.list-row .button')].some((n) => n.textContent === 'notes.txt')", "file list did not load");
    await click(".list-row .button", "notes.txt");
    await until("!!document.querySelector('textarea.terminal-frame')", "file did not open");
    await evaluate("document.querySelector('.button-primary').click()");
    await until("/Saved notes.txt/.test(document.querySelector('.toast')?.textContent ?? '')", "save toast did not show");
    await both("files-toast", "file editor with toast");
  }
  if (panel === "Settings") {
    // Shared dialog component: open the unpair confirmation by keyboard, then cancel it.
    await evaluate("document.querySelector('.button-danger').focus()");
    await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
    await until("!!document.querySelector('dialog.dialog[open]')", "dialog did not open");
    if ((await evaluate("document.activeElement.textContent")) !== "Cancel") await fail("dialog focus is not on Cancel");
    await both("dialog", "dialog");
    await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
    await until("!document.querySelector('dialog')", "Escape did not close the dialog");
    if (!(await evaluate("document.activeElement.classList.contains('button-danger')"))) await fail("focus did not return to the opener");
    if (!(await evaluate("fetch('api/session').then((r) => r.ok)"))) await fail("cancel unpaired the device");
  }
}

// 7. Dark preference: a theme with a dark scheme changes, a light-only theme does not.
await viewport(320, 640);
await click("#nav a", "Chat");
await until("!!document.querySelector('.composer textarea')", "chat did not reopen");
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
const scheme = await evaluate("document.querySelector('meta[name=color-scheme]').content");
const dark = await evaluate("getComputedStyle(document.body).backgroundColor");
if ((scheme === "light") !== (dark === bodyColor)) await fail(`color-scheme ${scheme} but dark background is ${dark}`);
await sound("chat in the dark preference");
await capture("chat-320-dark");

// The first, unpaired load answers 401 by design; any other console error is a defect.
const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);

// 8. Offline: the installed shell still loads and says what is wrong.
await send("Network.emulateNetworkConditions", { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0 });
await send("Page.reload");
await until("/Cannot reach the server/.test(document.querySelector('#main')?.textContent ?? '')", "offline shell did not render");
await capture("offline-320");

console.log(`${site}: paired, chat round trip over the event stream, base-path requests only, ${panels} and the dialog and toast at 320 and 1280 px without overflow, theme background ${bodyColor}, reduced motion, dark preference, offline shell`);
await cleanup();
process.exit(0);
