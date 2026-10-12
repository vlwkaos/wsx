// A headless Chrome driven over the Chrome DevTools Protocol with Node's built-in
// WebSocket. Shared by web-browser-check.mjs and web-terminal-check.mjs.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

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
  // scale: device pixel ratio. A phone-sized viewport is emulated as a touch device.
  async function viewport(width, height, scale = 1) {
    const mobile = width < 600;
    await send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: scale, mobile });
    await send("Emulation.setTouchEmulationEnabled", { enabled: mobile, maxTouchPoints: 5 });
  }
  async function capture(name) {
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(path.join(captures, `${prefix}${name}.png`), Buffer.from(data, "base64"));
  }
  return {
    send, evaluate, until, viewport, capture, fail, problems,
    cleanup: async () => { socket.close(); await cleanup(); },
  };
}
