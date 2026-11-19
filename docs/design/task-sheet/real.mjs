// Task 533: the REAL task sheet (crates/wsx-web/web: task.js, story.js, app.css, the default theme's tokens) in headless Chrome,
// fed with the shape of `GET api/tasks/<project>/t/<n>` for four states (ready, working, waiting on the owner, landed).
// Only the answer of that one route is canned (fetch is replaced in the page); every line of the sheet is the product's.
// Measures per state, width (390, 1280) and scheme: buttons, buttons whose text is clipped (scrollWidth > clientWidth), and whether the
// first content line is under the header (its centre is not what elementFromPoint returns, or its top is above the header's foot).
// Usage: CHROME_BIN=<chrome with --no-sandbox> node real.mjs [--rev <commit>] [--shots]
//   --rev   serve crates/wsx-web/web as it was at <commit> (git archive), to measure the sheet before the change
//   --shots write real-<state>-<scheme>-<width>.png here
// Exits 1 when a clipped button or a covered first line is found.
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../../..");
const arg = (name) => { const at = process.argv.indexOf(name); return at < 0 ? null : process.argv[at + 1] ?? true; };
const rev = arg("--rev"), shots = process.argv.includes("--shots");

let web = path.join(root, "crates/wsx-web/web");
if (rev) {
  const folder = path.join(root, ".work", `real-${rev}`);
  fs.rmSync(folder, { recursive: true, force: true });
  fs.mkdirSync(folder, { recursive: true });
  execFileSync("sh", ["-c", `git -C ${JSON.stringify(root)} archive ${JSON.stringify(rev)} crates/wsx-web/web | tar -x -C ${JSON.stringify(folder)}`]);
  web = path.join(folder, "crates/wsx-web/web");
}
const types = { ".js": "text/javascript", ".css": "text/css", ".html": "text/html", ".svg": "image/svg+xml", ".png": "image/png" };

// What the page runs first: the one route answered, the site's rights set, the real TaskPage in a dialog like the app's Sheet.
const harness = `
try {
const states = ${JSON.stringify(stateRows())};
const name = new URLSearchParams(location.search).get("state");
const answer = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const real = window.fetch.bind(window);
window.fetch = (url, init) => /api\\/tasks\\/demo\\/t\\/\\d+/.test(String(url)) ? Promise.resolve(answer(states[name]))
  : /^https?:\\/\\/[^/]+\\/api\\//.test(String(url)) ? Promise.resolve(answer({ error: { code: "none", message: "none" } }, 404)) : real(url, init);
const { html, render } = await import("./lib.js");
const { update } = await import("./store.js");
const { TaskPage } = await import("./task.js");
update({ site: { capabilities: ["task_view", "task_act"], panels: [] }, tasks: { demo: { tasks: [], needs: [], decisions: [] } } });
const Sheet = () => html\`<dialog class="task-sheet" ref=\${(node) => node && !node.open && node.showModal()}>
  <div class="task-sheet-top"><span class="sheet-grip" aria-hidden="true"></span></div>
  <\${TaskPage} project="demo" task=\${530} onClose=\${() => {}} onMention=\${() => {}} onReply=\${() => {}} /></dialog>\`;
render(html\`<\${Sheet} />\`, document.getElementById("app"));
} catch (error) { document.body.setAttribute("data-error", String(error.stack)); }`;

function stateRows() {
  const now = Date.now();
  const base = {
    id: 530, task: 530, project: "demo", revision: 3, default_branch: "main", conflict: false, overlaps: [], land_blocked_by: [], trial: null, usage: null,
    approvals: [], to_check: [], talk: [], waiting: null, flow: { into: "main", parent: null, after: null, held: null }, timeline: { entries: [], skipped: 0 },
    agent: { adapter: "claude", model: "claude-opus-5-5", effort: "high", fallback: false }, deploy: { deployed: false }, report: "## Changes\n\n- the sheet\n\n## Evidence\n\n- checks 4 of 4 passed\n",
    title: "Task sheet: the title is the only headline",
  };
  const summary = "Goal. The sheet says the state once.\n\nI changed story.js and task.js; the question for you: should the worker's text stay behind More? It also touches app.css and the status words, which I measured at 390 and 1280 px in both schemes.\n\nThe checks pass.";
  const result = { summary, decisions: [{ choice: "More", reason: "the worker's text is long" }] };
  const status = (kind, label, minutes, facts, next, rest, ask) => ({
    kind, label, since_unix_ms: now - minutes * 60_000, rest, ask,
    sentence: `${label} for ${minutes} min${facts ? `: ${facts}` : ""}. ${next}`,
  });
  return {
    ready: { ...base, state: "ready", stage: "ready", actions: ["land", "hand", "try"], result, goal: "Goal.",
      status: status("review", "Ready, waiting for the manager's review", 23, "checks 4 of 4 passed", "The manager reads the diff, then lands it or asks for changes.", "Checks 4 of 4 passed. The manager reads the diff, then lands it or asks for changes.", null),
      progress: { steps: ["plan", "build", "review"], step: 2, last_unix_ms: now - 23 * 60_000, stuck: false } },
    working: { ...base, state: "working", stage: "working", actions: ["hand"], result: null, goal: "Make the sheet read at a glance.",
      status: status("working", "Working on build (2 of 3)", 4, "", "It goes on by itself.", "It goes on by itself.", null),
      progress: { steps: ["plan", "build", "review"], step: 1, last_unix_ms: now - 20_000, stuck: false, now: null } },
    owner: { ...base, state: "waiting", stage: "waiting", actions: ["answer", "hand"], result, goal: "Goal.", waiting: { on: "owner" },
      status: status("owner", "Waiting for your answer", 12, "should the worker's text stay behind More?", "Answer the manager; the worker goes on then.", "Answer the manager; the worker goes on then.", "Should the worker's text stay behind More?"),
      progress: { steps: ["plan", "build", "review"], step: 1, last_unix_ms: now - 12 * 60_000, stuck: false } },
    landed: { ...base, state: "landed", stage: "done", actions: [], result, goal: "Goal.", progress: null,
      status: status("landed", "Landed", 0, "on main; the last deploy does not have it yet", "", "On main; the last deploy does not have it yet.", null) },
  };
}

const server = http.createServer((request, response) => {
  const url = new URL(request.url, "http://x");
  const send = (body, type) => { response.writeHead(200, { "content-type": type }); response.end(body); };
  if (url.pathname === "/harness.html") return send(`<!doctype html><html lang="en"><head><meta charset="utf-8"><base href="/">
<meta name="viewport" content="width=device-width, initial-scale=1"><link rel="stylesheet" href="app.css"><link rel="stylesheet" href="/theme.css"></head>
<body><div id="app"></div><script type="module" src="/harness.js"></script></body></html>`, "text/html");
  if (url.pathname === "/harness.js") return send(harness, "text/javascript");
  if (url.pathname === "/theme.css") return send(fs.readFileSync(path.join(root, "docs/design/chat-system/theme.css")), "text/css");
  // ^ The server's asset route has the vendored files beside the client's own (assets.rs).
  let file = path.join(web, path.normalize(url.pathname));
  if (!fs.existsSync(file)) file = path.join(web, "vendor", path.basename(url.pathname));
  if (!file.startsWith(web) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) { response.writeHead(404); return response.end(); }
  send(fs.readFileSync(file), types[path.extname(file)] ?? "application/octet-stream");
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}`;

const measure = `(() => {
  const sheet = document.querySelector(".task-sheet"), scroll = sheet.querySelector(".scroll"), bar = sheet.querySelector(".bar");
  const buttons = [...sheet.querySelectorAll(".task-next button")];
  const clipped = buttons.filter((button) => button.scrollWidth > button.clientWidth).map((button) => button.textContent.trim());
  const first = scroll.querySelector(".story").firstElementChild;
  const range = document.createRange();
  range.selectNodeContents(first);
  const line = range.getClientRects()[0], hit = document.elementFromPoint(line.left + line.width / 2, line.top + line.height / 2);
  const under = bar.getBoundingClientRect().bottom;
  return JSON.stringify({ buttons: buttons.map((button) => button.textContent.trim()), clipped, covered: !scroll.contains(hit) || line.top < under - 0.5,
    lineTop: Math.round(line.top), barFoot: Math.round(under), first: first.textContent.trim().slice(0, 60), sideways: scroll.scrollWidth > scroll.clientWidth + 1, scrolled: Math.round(scroll.scrollTop) });
})()`;

const page = await launch({ captures: here, prefix: "", timeoutSeconds: 300 });
const rows = [], sweep = [];
try {
  for (const state of ["ready", "working", "owner", "landed"]) {
    for (const scheme of ["dark", "light"]) {
      for (const width of [390, 1280]) {
        await page.viewport(width, 844, width === 1280 ? 1 : 2);
        await page.send("Page.navigate", { url: `${origin}/harness.html?state=${state}` });
        await page.until("document.readyState === 'complete' && !!document.querySelector('.task-sheet .story, [data-error]')", `${state} sheet renders`, 15);
        const failure = await page.evaluate("document.body.dataset.error ?? ''");
        if (failure) throw new Error(failure);
        if (scheme === "light") await page.evaluate("document.documentElement.dataset.scheme = 'light'");
        await new Promise((resolve) => setTimeout(resolve, 1200));
        rows.push({ state, scheme, width, ...JSON.parse(await page.evaluate(measure)) });
        if (shots) await page.capture(`real-${state}-${scheme}-${width}`);
      }
    }
  }
  // ^ Every phone width from 320 to 430 px, every 5 px, for the two states with the most buttons.
  for (const state of ["ready", "owner"]) {
    for (let width = 320; width <= 430; width += 5) {
      await page.viewport(width, 844, 2);
      await page.send("Page.navigate", { url: `${origin}/harness.html?state=${state}` });
      await page.until("!!document.querySelector('.task-sheet .story')", `${state} sheet renders at ${width}`, 15);
      await new Promise((resolve) => setTimeout(resolve, 600));
      sweep.push({ state, width, ...JSON.parse(await page.evaluate(measure)) });
    }
  }
} finally {
  await page.cleanup();
  server.close();
}
const clipped = rows.filter((row) => row.clipped.length), covered = rows.filter((row) => row.covered), sideways = rows.filter((row) => row.sideways), scrolled = rows.filter((row) => row.scrolled), narrow = sweep.filter((row) => row.clipped.length || row.covered);
console.log(`${rev ? `at ${rev}` : "working copy"}: ${rows.length} pages, ${rows.reduce((sum, row) => sum + row.buttons.length, 0)} buttons, ${clipped.length} pages with a clipped button, ${covered.length} with the first line under the header, ${sideways.length} with sideways scroll, ${scrolled.length} opened scrolled; sweep 320-430 px: ${sweep.length} pages, ${narrow.length} with a clipped button or a covered line${narrow.length ? ` (${narrow.map((row) => `${row.state} ${row.width}`).join(", ")})` : ""}`);
for (const row of rows) console.log(`  ${row.state} ${row.scheme} ${row.width}: buttons=${JSON.stringify(row.buttons)} clipped=${JSON.stringify(row.clipped)} covered=${row.covered} first=${JSON.stringify(row.first)}`);
process.exit(clipped.length + covered.length + sideways.length + scrolled.length + narrow.length ? 1 : 0);
