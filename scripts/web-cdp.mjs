// A headless Chrome driven over the Chrome DevTools Protocol with Node's built-in
// WebSocket. Shared by web-browser-check.mjs, web-tasks-check.mjs and web-terminal-check.mjs.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// The phone layout audit of the ui-view-standards skill (scripts/mobile-audit.js, or the file
// WSX_MOBILE_AUDIT names), or null when there is none.
export function mobileAudit() {
  const file = process.env.WSX_MOBILE_AUDIT ?? fileURLToPath(new URL("./mobile-audit.js", import.meta.url));
  return { file, source: fs.existsSync(file) ? fs.readFileSync(file, "utf8") : null };
}

// captures: directory for screenshots. prefix: file name prefix. timeoutSeconds: whole-run limit.
export async function launch({ captures, prefix, timeoutSeconds }) {
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

  // In the run's folder when test-web-client.py runs this, so its next run can remove it
  // even when this process was killed.
  const profile = fs.mkdtempSync(path.join(process.env.WSX_TEST_WORK ?? os.tmpdir(), "wsx-web-chrome-"));
  const browser = spawn(chrome, [
    "--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`,
    "--no-first-run", "--no-default-browser-check", "--disable-extensions", "about:blank",
  ], { stdio: ["ignore", "ignore", "pipe"] });
  // Chrome still writes its profile while it exits: wait for it, then remove the profile.
  // Once only, whichever way this process ends.
  let cleaned;
  const cleanup = () => cleaned ??= (async () => {
    if (browser.exitCode === null && browser.signalCode === null) {
      const exited = new Promise((resolve) => browser.once("exit", resolve));
      browser.kill();
      await exited;
    }
    try {
      fs.rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    } catch (error) {
      console.error(`could not remove ${profile}: ${error.code}`);
    }
  })();
  const end = (reason) => async (detail) => {
    console.error(`FAIL: ${reason}${detail instanceof Error ? `: ${detail.stack}` : ""}`);
    await cleanup();
    process.exit(1);
  };
  for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) process.once(signal, end(`stopped by ${signal}`));
  process.once("uncaughtException", end("uncaught exception"));
  process.once("unhandledRejection", end("unhandled rejection"));
  const fail = async (message) => {
    console.error(`FAIL: ${message}`);
    await cleanup();
    process.exit(1);
  };
  setTimeout(() => fail(`timed out after ${timeoutSeconds} s`), timeoutSeconds * 1000).unref();

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
      const details = message.params.exceptionDetails;
      problems.push(`exception: ${details.exception?.description ?? details.text} (${details.url ?? ""}:${details.lineNumber ?? ""})`);
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
  async function until(expression, label, attempts = 100) {
    for (let attempt = 0; attempt < attempts; attempt++) {
      if (await evaluate(expression).catch(() => false)) return;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    await fail(`${label}; page says: ${await evaluate("document.body.innerText.slice(0, 300)").catch(() => "?")}`);
  }
  // scale: device pixel ratio. A phone-sized viewport is emulated as a touch device.
  async function viewport(width, height, scale = 1) {
    const mobile = width < 600;
    await send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: scale, mobile });
    await send("Emulation.setTouchEmulationEnabled", { enabled: mobile, maxTouchPoints: 5 });
  }
  // clip: {x, y, width, height, scale} in CSS pixels, for one enlarged part of the page.
  // Returns the PNG, so two captures can be compared.
  async function capture(name, clip) {
    const { data } = await send("Page.captureScreenshot", { format: "png", ...(clip ? { clip } : {}) });
    const image = Buffer.from(data, "base64");
    fs.writeFileSync(path.join(captures, `${prefix}${name}.png`), image);
    return image;
  }
  const go = (hash) => evaluate(`location.hash = ${JSON.stringify(hash)}`);
  const text = (selector) => evaluate(`[...document.querySelectorAll(${JSON.stringify(selector)})].map((node) => node.textContent.trim().replace(/\\s+/g, ' '))`);
  const count = (selector) => evaluate(`document.querySelectorAll(${JSON.stringify(selector)}).length`);
  // options: more Input.dispatchKeyEvent fields, such as modifiers.
  async function key(name, keyCode, options = {}) {
    await send("Input.dispatchKeyEvent", { type: "keyDown", key: name, code: name, windowsVirtualKeyCode: keyCode, ...options });
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: name, code: name, windowsVirtualKeyCode: keyCode, ...options });
  }
  return {
    send, evaluate, until, viewport, capture, fail, problems, go, text, count, key,
    cleanup: async () => { socket.close(); await cleanup(); },
  };
}
