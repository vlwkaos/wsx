// A headless Chrome driven over the Chrome DevTools Protocol with Node's built-in
// WebSocket. Shared by web-browser-check.mjs, web-tasks-check.mjs and web-terminal-check.mjs.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// The frames of `seen` (numbers, or [name, number] pairs: only readings of one name compare) where
// the reading moved more than `limit` from the frame before: `[frame, from, to]`.
export function jumps(seen, limit = 3) {
  const found = [];
  for (let at = 1; at < seen.length; at++) {
    const [was, now] = [seen[at - 1], seen[at]];
    const pair = Array.isArray(now);
    if (pair && (!Array.isArray(was) || was[0] !== now[0])) continue;
    const [from, to] = pair ? [was[1], now[1]] : [was, now];
    if (from != null && to != null && Math.abs(to - from) > limit) found.push([at, Math.round(from), Math.round(to)]);
  }
  return found;
}

// The phone layout audit of the ui-view-standards skill (scripts/mobile-audit.js, or the file
// WSX_MOBILE_AUDIT names), or null when there is none.
export function mobileAudit() {
  const file = process.env.WSX_MOBILE_AUDIT ?? fileURLToPath(new URL("./mobile-audit.js", import.meta.url));
  return { file, source: fs.existsSync(file) ? fs.readFileSync(file, "utf8") : null };
}

// The design lint of the ui-view-standards skill (scripts/design-lint.js, vendored; its keep list is
// scripts/design-lint-keep.json). `lint(name)` runs it on the page as it is, at 390x844 (touch) and
// 1280x800 only, and records the screen; `finish(fail)` writes `design-lint-<file>.json` into the
// captures folder, prints the warns as a count and the first few, and fails once with every
// un-kept error, so a run shows all of them. Warns never fail.
export function designLint({ evaluate, captures, file }) {
  const here = (name) => fileURLToPath(new URL(name, import.meta.url));
  const source = fs.readFileSync(here("./design-lint.js"), "utf8");
  const keep = JSON.parse(fs.readFileSync(here("./design-lint-keep.json"), "utf8"));
  // ^ The vendored file is the installed one under two header lines; say so when they differ.
  const installed = path.join(os.homedir(), ".agents/skills/ui-view-standards/scripts/design-lint.js");
  if (fs.existsSync(installed)) {
    const body = source.split("\n").filter((line) => !line.startsWith("// vendored:")).join("\n");
    if (body !== fs.readFileSync(installed, "utf8")) console.log(`design lint: scripts/design-lint.js differs from the installed skill copy ${installed}`);
  }
  const screens = [];
  async function lint(name) {
    const [width, height] = await evaluate("[innerWidth, innerHeight]");
    if (!((width === 390 && height === 844) || (width === 1280 && height === 800))) return;
    // ^ A row that arrives or a tone that fades is mid-way in colour and place: lint it once the finite animations and transitions ended.
    for (let waited = 0; waited < 40 && await evaluate("document.getAnimations().some((animation) => animation.playState === 'running' && animation.effect?.getComputedTiming().iterations !== Infinity)"); waited++) await pause(100);
    await evaluate(`globalThis.DESIGN_LINT = ${JSON.stringify({ touch: width < 600, gutter: 16, keep })}`);
    // ^ A closed <details> keeps its content measurable: hide it for the lint, as it is for the reader.
    // ^ A constructed sheet, not a <style> element: the page's CSP blocks inline styles.
    await evaluate("(() => { const sheet = new CSSStyleSheet(); sheet.replaceSync('details:not([open]) > :not(summary) { display: none !important; }'); window.designLintSheet = sheet; document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet]; })()");
    const result = await evaluate(source);
    await evaluate("document.adoptedStyleSheets = document.adoptedStyleSheets.filter((sheet) => sheet !== window.designLintSheet)");
    const scheme = await evaluate("document.documentElement.dataset.scheme ?? 'dark'");
    const screen = { screen: name, size: `${width}x${height}`, scheme, counts: result.counts, findings: result.findings };
    screens.push(screen);
    const { error, warn, kept } = result.counts;
    console.log(`design lint ${name} at ${screen.size} ${scheme}: ${error} errors, ${warn} warns, ${kept} kept`);
    // ^ At once too: a check that stops on something else before finish() still shows what the lint found.
    for (const finding of result.findings.filter((finding) => finding.level === "error" && !finding.kept)) console.log(`  error ${finding.rule} ${finding.element} "${finding.text}" ${JSON.stringify(finding.rect)}: ${finding.fix}`);
  }
  async function finish(fail) {
    fs.writeFileSync(path.join(captures, `design-lint-${file}.json`), JSON.stringify(screens, null, 1));
    const found = (level, kept = false) => screens.flatMap((screen) => screen.findings
      .filter((finding) => finding.level === level && Boolean(finding.kept) === kept).map((finding) => ({ ...finding, where: `${screen.screen} ${screen.size} ${screen.scheme}` })));
    const warns = found("warn");
    console.log(`design lint ${file}: ${screens.length} screens, ${found("error").length} errors, ${warns.length} warns, ${found("error", true).length + found("warn", true).length} kept`);
    for (const warn of warns.slice(0, 5)) console.log(`  warn ${warn.rule} ${warn.element} "${warn.text}" (${warn.where})`);
    // ^ One line per element and rule, with every screen it shows on.
    const errors = new Map();
    for (const error of found("error")) {
      const key = `${error.rule} ${error.element} ${error.text}`;
      errors.set(key, { ...error, wheres: [...(errors.get(key)?.wheres ?? []), error.where] });
    }
    if (errors.size) {
      await fail(`design lint found ${errors.size} errors:\n${[...errors.values()].map((error) => `  ${error.rule} ${error.element} "${error.text}" ${JSON.stringify(error.rect)} on ${error.wheres.join(", ")}\n    fix: ${error.fix}`).join("\n")}`);
    }
  }
  return { lint, finish };
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
  // prefers-reduced-motion: "reduce" or "no-preference". The page starts at no-preference, as
  // this Mac's Chrome reports it: a CI runner's Chrome reports the runner's own setting.
  const setMotion = (value) => send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value }] });
  await setMotion("no-preference");

  async function evaluate(expression) {
    const { result, exceptionDetails } = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (exceptionDetails) throw new Error(exceptionDetails.exception?.description ?? exceptionDetails.text);
    return result.value;
  }
  // Poll the page from here; the page itself has no timers to wait on. label: text, or an async
  // function that reads the page when the wait fails.
  async function until(expression, label, attempts = 100) {
    for (let attempt = 0; attempt < attempts; attempt++) {
      if (await evaluate(expression).catch(() => false)) return;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    await fail(`${typeof label === "function" ? await label() : label}; page says: ${await evaluate("document.body.innerText.slice(0, 300)").catch(() => "?")}`);
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
  // The on-screen keyboard as iOS shows it: the visual viewport shrinks to `height`, the layout
  // viewport does not, and the visual viewport may move `top` down the page. null closes it.
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
  // Reads `expression` in the page once in each frame, as it is painted, until `watched()` returns
  // the readings. The steps between may take any time.
  // ^ Read in a resize observer made after the page's own, so after the page's corrections in that
  // frame: a reading at requestAnimationFrame comes before them and sees a place never painted.
  const watch = (expression) => evaluate(`(() => {
    const seen = window.wsxFrames = [];
    const sentinel = document.body.appendChild(document.createElement("div"));
    sentinel.style.cssText = "position: fixed; top: 0; left: 0; height: 1px; width: 1px; visibility: hidden; pointer-events: none";
    const sizes = new ResizeObserver(() => {
      if (window.wsxFrames !== seen) return;
      seen.push(${expression});
    });
    const next = () => {
      if (window.wsxFrames !== seen) { sizes.disconnect(); sentinel.remove(); return; }
      sentinel.style.width = sentinel.style.width === "1px" ? "2px" : "1px";
      requestAnimationFrame(next);
    };
    sizes.observe(sentinel);
    requestAnimationFrame(next);
  })()`);
  const watched = () => evaluate("(() => { const seen = window.wsxFrames ?? []; window.wsxFrames = null; return seen; })()");
  return {
    send, evaluate, until, viewport, capture, fail, problems, go, text, count, key, keyboard, watch, watched, setMotion,
    cleanup: async () => { socket.close(); await cleanup(); },
  };
}
