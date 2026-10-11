#!/usr/bin/env node
// Real-browser check for the hosted web client, driven over the Chrome DevTools
// Protocol with Node's built-in WebSocket. Called by `test-web-client.py --browser`.
// Usage: node web-browser-check.mjs <site-url> <pairing-code> <session-id> <capture-dir> <site-id> <panels>
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const [siteUrl, code, sessionId, captures, site, panels] = process.argv.slice(2);
const chrome = [
  process.env.CHROME_BIN,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
].find((candidate) => candidate && fs.existsSync(candidate));
if (!chrome) {
  console.error("no Chrome found; set CHROME_BIN");
  process.exit(2);
}

const profile = fs.mkdtempSync(path.join(os.tmpdir(), "wsx-web-chrome-"));
const browser = spawn(chrome, [
  "--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`,
  "--no-first-run", "--no-default-browser-check", "--disable-extensions", "about:blank",
], { stdio: ["ignore", "ignore", "pipe"] });
// Chrome still writes its profile while it exits: wait for it, then remove the profile.
const cleanup = async () => {
  const exited = new Promise((resolve) => browser.once("exit", resolve));
  browser.kill();
  await exited;
  try {
    fs.rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch (error) {
    console.error(`could not remove ${profile}: ${error.code}`);
  }
};
const fail = async (message) => {
  console.error(`FAIL: ${message}`);
  await cleanup();
  process.exit(1);
};
setTimeout(() => fail("timed out after 90 s"), 90_000).unref();

const endpoint = await new Promise((resolve) => {
  let text = "";
  browser.stderr.on("data", (chunk) => {
    text += chunk;
    const match = /DevTools listening on (ws:\/\/\S+)/.exec(text);
    if (match) resolve(match[1]);
  });
});
const socket = new WebSocket(endpoint);
await new Promise((resolve) => socket.addEventListener("open", resolve));
let nextId = 0;
const waiting = new Map();
const problems = [];
socket.addEventListener("message", (event) => {
  const message = JSON.parse(event.data);
  if (message.id) {
    const done = waiting.get(message.id);
    waiting.delete(message.id);
    if (message.error) done.reject(new Error(JSON.stringify(message.error)));
    else done.resolve(message.result);
  } else if (message.method === "Runtime.exceptionThrown") {
    problems.push(`exception: ${message.params.exceptionDetails.text}`);
  } else if (message.method === "Log.entryAdded" && message.params.entry.level === "error") {
    problems.push(`log: ${message.params.entry.text} ${message.params.entry.url ?? ""}`);
  }
});
let session;
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++nextId;
    waiting.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params, sessionId: session }));
  });

const { targetId } = await send("Target.createTarget", { url: "about:blank" });
session = (await send("Target.attachToTarget", { targetId, flatten: true })).sessionId;
for (const domain of ["Page", "Runtime", "Log", "Network"]) await send(`${domain}.enable`);

async function evaluate(expression) {
  const { result, exceptionDetails } = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (exceptionDetails) throw new Error(exceptionDetails.exception?.description ?? exceptionDetails.text);
  return result.value;
}
// Poll the page from here; the page itself has no timers to wait on.
async function until(expression, label) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await evaluate(expression).catch(() => false)) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  await fail(`${label}; page says: ${await evaluate("document.body.innerText.slice(0, 300)").catch(() => "?")}`);
}
async function viewport(width, height) {
  await send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 600 });
}
async function capture(name) {
  const { data } = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(path.join(captures, `${site}-${name}.png`), Buffer.from(data, "base64"));
}
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
  box.dispatchEvent(new Event('input')); document.querySelector('.composer button.primary').click(); })()`);
await until(`[...document.querySelectorAll('.message[data-role="assistant"] .text')].some((n) => n.textContent === ${JSON.stringify("echo: " + sentinel)})`,
  "the agent reply did not appear");
await until("!document.querySelector('.message[data-pending]')", "the sent message stayed pending");
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

// 5. Desktop width, then reduced motion.
await viewport(1280, 800);
await sound("chat at 1280 px");
await capture("chat-1280");
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
if ((await evaluate("getComputedStyle(document.querySelector('.log')).scrollBehavior")) !== "auto") await fail("reduced motion is not respected");
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
await viewport(320, 640);
// Every other panel of this site, dark theme, 320 px, opened through its nav link.
for (const panel of panels.split(",").slice(1)) {
  await evaluate(`[...document.querySelectorAll('#nav a')].find((a) => a.textContent === ${JSON.stringify(panel)}).click()`);
  await until(`document.querySelector('#nav a[aria-current=page]')?.textContent === ${JSON.stringify(panel)} && document.querySelector('#main').children.length > 0`,
    `${panel} did not open`);
  await new Promise((resolve) => setTimeout(resolve, 400));
  await sound(`${panel} at 320 px`);
  await capture(`${panel.toLowerCase()}-320-dark`);
}

// The first, unpaired load answers 401 by design; any other console error is a defect.
const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);

// 6. Offline: the installed shell still loads and says what is wrong.
await send("Network.emulateNetworkConditions", { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0 });
await send("Page.reload");
await until("/Cannot reach the server/.test(document.querySelector('#main')?.textContent ?? '')", "offline shell did not render");
await capture("offline-320");

console.log(`${site}: paired, chat round trip over the event stream, base-path requests only, ${panels} at 320 px and chat at 1280 px without overflow, reduced motion, offline shell`);
socket.close();
await cleanup();
process.exit(0);
