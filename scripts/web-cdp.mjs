// A headless Chrome driven over the Chrome DevTools Protocol with Node's built-in
// WebSocket. Shared by every web-*-check.mjs.
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

// Pairs the page with a site: opens `url`, types `code` into the pairing form and waits for `ready` (a
// selector). `form` runs once the form shows, for a check of the form itself.
export async function pair({ send, evaluate, until }, url, code, { ready = ".app", form } = {}) {
  await send("Page.navigate", { url });
  await until("!!document.querySelector('#pair-code')", "pairing form did not render");
  await form?.();
  await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)}; input.form.requestSubmit(); })()`);
  await until(`!!document.querySelector(${JSON.stringify(ready)})`, `the page did not show ${ready} after pairing`);
}

// The page's fetch, wrapped: it waits when the device's request budget is spent (429,
// `act_budget` in crates/wsx-web/src/http.rs: 10 at once, then one per 3 s) by asking again
// every half second for up to 20 s, so a check never sleeps to let the budget refill; and
// `globalThis.wsxFetching` counts the requests in flight, so a check can wait for them to end.
export async function watchFetch({ send, evaluate }) {
  const source = `(() => { const real = globalThis.fetch;
    globalThis.wsxFetching = 0;
    globalThis.fetch = async (...args) => {
      globalThis.wsxFetching += 1;
      try {
        for (let turn = 0; ; turn++) {
          const response = await real(...args);
          if (response.status !== 429 || turn >= 40 || String(args[0]?.url ?? args[0]).includes('/pair')) return response;
          await new Promise((done) => setTimeout(done, 500));
        }
      } finally { globalThis.wsxFetching -= 1; }
    }; })()`;
  await send("Page.addScriptToEvaluateOnNewDocument", { source });
  await evaluate(source);
}

// Layout rules of every screen: nothing past the right edge, no raw null, one scroll container
// and the page itself never scrolls. `free`: selectors whose nodes may pass the edge or scroll
// sideways; `apart`: selectors whose nodes hold scrollers of their own; `settle`: runs first.
export function soundCheck({ evaluate, fail }, size, { free, apart, settle }) {
  return async (label) => {
    await settle?.(label);
    const seen = await evaluate(`(() => {
      const free = (node) => node.closest(${JSON.stringify(free)});
      const wide = [...document.querySelectorAll('#app *')].filter((node) => !free(node) && node.getBoundingClientRect().right > innerWidth + 0.5);
      const scrollers = [...document.querySelectorAll('#app *')].filter((node) => node.tagName !== 'TEXTAREA' && !free(node) && !node.closest(${JSON.stringify(apart)})
        && /auto|scroll/.test(getComputedStyle(node).overflowY) && node.scrollHeight > node.clientHeight + 1);
      return { wide: wide.slice(0, 3).map((node) => node.className || node.tagName), scrollers: scrollers.map((node) => node.className),
        raw: /\\b(null|undefined|NaN)\\b/.test(document.querySelector('#app').innerText), page: document.scrollingElement.scrollTop + document.scrollingElement.scrollLeft };
    })()`);
    const where = `${label} at ${size()[0]}x${size()[1]}`;
    if (seen.wide.length) await fail(`${where}: elements pass the right edge: ${seen.wide}`);
    if (seen.scrollers.length > 1) await fail(`${where}: more than one scroll container: ${seen.scrollers}`);
    if (seen.raw) await fail(`${where} shows a raw null`);
    if (seen.page) await fail(`${where}: the page itself scrolled`);
  };
}

// One capture with its audits. At most one filled accent control, no sideways scroll, no zooming
// field, and no other field in the way of the composer's iOS arrows. `size()` is the viewport as
// [width, height]; `fileOf(name)` is the capture's name; `before(name)` runs first; `record(file,
// mobile)` gets the mobile audit (null off a phone); `linted` is the set of names that get the design lint.
export function shooter({ evaluate, fail, capture }, { captures, prefix, size, fileOf, linter, linted, auditSource, mobileSource, before, record }) {
  return async (name, primary) => {
    await before?.(name);
    const label = `${name} at ${size()[0]}x${size()[1]}`;
    const file = fileOf(name);
    let mobile = null;
    if (auditSource) {
      await evaluate(`globalThis.SLOP_AUDIT_PRIMARY = ${JSON.stringify(primary ?? null)}`);
      // ^ Behind an open modal dialog (the task sheet beside the chat at 1280 px) the page is
      // inert, so the audit, which does not know modal dialogs, sees only the dialog.
      // The page's CSP refuses a style element; CSSOM properties are allowed.
      const audit = await evaluate(`(() => { const modal = document.querySelector('dialog[open]:modal');
        if (modal) { document.body.style.visibility = 'hidden'; modal.style.visibility = 'visible'; }
        try { return (
${auditSource}
); } finally { if (modal) { document.body.style.visibility = ''; modal.style.visibility = ''; } } })()`);
      fs.writeFileSync(path.join(captures, `${prefix}${file}.slop.json`), JSON.stringify(audit, null, 1));
      if (audit.horizontalOverflow) await fail(`${label} overflows sideways`);
      if (audit.primaryAction.filledInViewport > 1) await fail(`${label} shows ${audit.primaryAction.filledInViewport} filled accent controls: ${JSON.stringify(audit.primaryAction.filled)}`);
      if (primary && !audit.primaryAction.inFirstViewport) await fail(`${label}: the primary action is not in view`);
    }
    if (mobileSource && size()[0] < 600) {
      await evaluate("globalThis.MOBILE_AUDIT_GUTTER = 16");
      mobile = await evaluate(mobileSource);
      fs.writeFileSync(path.join(captures, `${prefix}${file}.mobile.json`), JSON.stringify(mobile, null, 1));
      if (mobile.horizontalOverflow) await fail(`${label}: the page scrolls sideways (mobile audit)`);
      if (mobile.zoomingFields.count) await fail(`${label}: a field makes iOS zoom: ${JSON.stringify(mobile.zoomingFields.items)}`);
      // ^ A field outside an open modal dialog (the task sheet) is inert, so the iOS form bar
      // cannot reach it; the mobile audit does not know about modal dialogs.
      const reachable = await evaluate(`(() => { const modal = document.querySelector('dialog[open]:modal');
        return [...document.querySelectorAll('input:not([type=hidden]), textarea, select')].filter((el) => el !== document.activeElement
          && !el.disabled && el.tabIndex >= 0 && !(modal && !modal.contains(el))).length; })()`);
      if (mobile.focusedField?.startsWith("textarea.composer-field") && mobile.otherFocusableFields?.count && reachable) {
        await fail(`${label}: other fields can take the focus from the composer: ${JSON.stringify(mobile.otherFocusableFields.items)}`);
      }
    }
    record?.(file, mobile);
    if (linted.has(name)) await linter.lint(name);
    await capture(file);
  };
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
