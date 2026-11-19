// Task 533: what real.mjs (Chrome) and real-webkit.mjs (WebKit, iPhone emulation) share: a static server for the REAL task sheet
// (crates/wsx-web/web: task.js, story.js, app.css, the default theme's tokens), the shape of `GET api/tasks/<project>/t/<n>` for
// four states, and the measurement. Only that one route's answer is canned (fetch is replaced in the page).
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const here = path.dirname(fileURLToPath(import.meta.url));
export const root = path.resolve(here, "../../..");
export const STATES = ["ready", "working", "owner", "landed"];
const types = { ".js": "text/javascript", ".css": "text/css", ".html": "text/html", ".svg": "image/svg+xml", ".png": "image/png" };

// The folder of the client files: the working copy's, or as they were at `rev` (git archive).
function webFolder(rev) {
  if (!rev) return path.join(root, "crates/wsx-web/web");
  const folder = path.join(root, ".work", `real-${rev}`);
  fs.rmSync(folder, { recursive: true, force: true });
  fs.mkdirSync(folder, { recursive: true });
  execFileSync("sh", ["-c", `git -C ${JSON.stringify(root)} archive ${JSON.stringify(rev)} crates/wsx-web/web | tar -x -C ${JSON.stringify(folder)}`]);
  return path.join(folder, "crates/wsx-web/web");
}

// What the page runs first: the one route answered, the site's rights set, the real TaskPage in a dialog like the app's Sheet,
// inside `.app` (its rules reach `.bar-title`); `wide` is the app's wide layout.
const harness = `
try {
const states = __STATES__;
const query = new URLSearchParams(location.search);
const name = query.get("state");
const answer = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
const real = window.fetch.bind(window);
window.fetch = (url, init) => /api\\/tasks\\/demo\\/checks$/.test(String(url)) ? Promise.resolve(answer({ open: states.checks }))
  : /api\\/tasks\\/demo\\/t\\/\\d+/.test(String(url)) ? Promise.resolve(answer(states[name]))
  : /^https?:\\/\\/[^/]+\\/api\\//.test(String(url)) ? Promise.resolve(answer({ error: { code: "none", message: "none" } }, 404)) : real(url, init);
const { html, render } = await import("./lib.js");
const { update } = await import("./store.js");
const { TaskPage } = await import("./task.js");
const { ChecksPage } = await import("./checks.js");
update({ site: { capabilities: ["task_view", "task_act"], panels: [] }, tasks: { demo: { tasks: [], needs: [], decisions: [] } } });
const Sheet = () => html\`<div class="app" data-wide=\${query.get("wide") ? "" : undefined}><dialog class="task-sheet" ref=\${(node) => node && !node.open && node.showModal()}>
  <div class="task-sheet-top"><span class="sheet-grip" aria-hidden="true"></span></div>
  \${name === "checks" ? html\`<\${ChecksPage} project="demo" onClose=\${() => {}} onReply=\${() => {}} />\`
    : html\`<\${TaskPage} project="demo" task=\${530} onClose=\${() => {}} onMention=\${() => {}} onReply=\${() => {}} />\`}</dialog></div>\`;
render(html\`<\${Sheet} />\`, document.getElementById("app"));
} catch (error) { document.body.setAttribute("data-error", String(error.stack)); }`;

// The screenshot of the owner's phone (task 530, ready): its title, text and report notes.
function stateRows() {
  const now = Date.now();
  const base = {
    id: 530, task: 530, project: "demo", revision: 3, default_branch: "main", conflict: false, overlaps: [], land_blocked_by: [], trial: null, usage: null,
    approvals: [], to_check: [], talk: [{ text: "go on", unix_ms: now - 3_600_000 }], waiting: null, flow: { into: "main", parent: null, after: null, held: null },
    timeline: { entries: [], skipped: 0 }, agent: { adapter: "claude", model: "sonnet", effort: "high", fallback: false }, deploy: { deployed: false },
    report: "## Changes\n\n- the sheet\n\n## Evidence\n\n4 checks passed.\n\n- `cargo test`: exit 0\n\n## Left open\n\n- one\n- two\n- three\n",
    title: "Web: chat box container like the Home box",
  };
  const summary = "Your correction needs no new mockup: the after pictures already have no Land, and the measurement now proves it. I changed no product file, as planned. The band rules stay on main until you pick a variant. Question: strip inside the card (A) or above it (B)? Also: is a card with the box inside what you want?";
  const result = { summary, decisions: [{ choice: "More", reason: "the worker's text is long" }] };
  const status = (kind, label, minutes, facts, next, rest, ask) => ({
    kind, label, since_unix_ms: now - minutes * 60_000, rest, ask,
    sentence: `${label} for ${minutes} min${facts ? `: ${facts}` : ""}. ${next}`,
  });
  const steps = ["plan", "build", "review"];
  // ^ The To check sheet (the owner's second screenshot, task 496's items): six open items.
  const item = (id, task, did, check, where, pass_if) => ({ id, task, did, title: did, check, where, pass_if, state: "open", revision: 1, captures: null });
  const checks = [
    item("a1", 496, "Work is done and committed c0a0d7f0", "Home chat rows: shown name, muted project tag, no line-2 project prefix, a renamed chat and a chat of another project", "Home page of the web client, phone widths 375 and 390, desktop 1280", "Each chat row reads clearly. The project shows as a muted tag and is not cut off. Light and dark both look right."),
    ...[2, 3, 4, 5, 6].map((n) => item(`a${n}`, 490 + n, `Task ${490 + n} is done`, `Check number ${n} of the list`, "The Home page", `Item ${n} reads well`)),
  ];
  return {
    checks,
    ready: { ...base, state: "ready", stage: "ready", actions: ["land", "hand"], result, goal: "Goal.",
      status: status("review", "Ready, waiting for the manager's review", 23, "checks 4 of 4 passed", "The manager reads the diff, then lands it or asks for changes.", "Checks 4 of 4 passed. The manager reads the diff, then lands it or asks for changes.", null),
      progress: { steps, step: 3, last_unix_ms: now - 23 * 60_000, stuck: false } },
    working: { ...base, state: "working", stage: "working", actions: ["hand"], result: null, goal: "Make the sheet read at a glance.",
      status: status("working", "Working on build (2 of 3)", 4, "", "It goes on by itself.", "It goes on by itself.", null),
      progress: { steps, step: 1, last_unix_ms: now - 20_000, stuck: false, now: null } },
    owner: { ...base, state: "waiting", stage: "waiting", actions: ["answer", "hand"], result, goal: "Goal.", waiting: { on: "owner" },
      status: status("owner", "Waiting for your answer", 12, "should the worker's text stay behind More?", "Answer the manager; the worker goes on then.", "Answer the manager; the worker goes on then.", "Should the worker's text stay behind More?"),
      progress: { steps, step: 1, last_unix_ms: now - 12 * 60_000, stuck: false } },
    landed: { ...base, state: "landed", stage: "done", actions: [], result, goal: "Goal.", progress: null,
      status: status("landed", "Landed", 0, "on main; the last deploy does not have it yet", "", "On main; the last deploy does not have it yet.", null) },
  };
}

// `inset`: the safe areas to put where app.css reads env(safe-area-inset-*) ({ top, bottom } in px); an engine without a notch
// reads them as 0, so the served app.css carries the numbers of the phone being emulated.
export async function serve({ rev = null, inset = null } = {}) {
  const web = webFolder(rev);
  const harnessJs = harness.replace("__STATES__", JSON.stringify(stateRows()));
  const server = http.createServer((request, response) => {
    const url = new URL(request.url, "http://x");
    const send = (body, type) => { response.writeHead(200, { "content-type": type }); response.end(body); };
    if (url.pathname === "/harness.html") return send(`<!doctype html><html lang="en"><head><meta charset="utf-8"><base href="/">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no, viewport-fit=cover"><link rel="stylesheet" href="app.css"><link rel="stylesheet" href="/theme.css"></head>
<body><div id="app"></div><script type="module" src="/harness.js"></script></body></html>`, "text/html");
    if (url.pathname === "/harness.js") return send(harnessJs, "text/javascript");
    if (url.pathname === "/theme.css") return send(fs.readFileSync(path.join(root, "docs/design/chat-system/theme.css")), "text/css");
    // ^ The server's asset route has the vendored files beside the client's own (assets.rs).
    let file = path.join(web, path.normalize(url.pathname));
    if (!fs.existsSync(file)) file = path.join(web, "vendor", path.basename(url.pathname));
    if (!file.startsWith(web) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) { response.writeHead(404); return response.end(); }
    let body = fs.readFileSync(file);
    if (inset && url.pathname === "/app.css") {
      body = body.toString().replaceAll("env(safe-area-inset-top)", `${inset.top}px`).replaceAll("env(safe-area-inset-bottom)", `${inset.bottom}px`);
    }
    send(body, types[path.extname(file)] ?? "application/octet-stream");
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { origin: `http://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

// Per page: the sheet's buttons and those whose text is clipped (scrollWidth > clientWidth), and whether the first content line is
// under the header (its centre is not what elementFromPoint returns, or its top is above the header's foot); the first element's top
// against the scroll box's top and the scroll offset, for the layout the owner's phone showed.
export const measure = `(() => {
  const sheet = document.querySelector(".task-sheet"), scroll = sheet.querySelector(".scroll"), bar = sheet.querySelector(".bar");
  const buttons = [...sheet.querySelectorAll(".task-next button, .story-act button, .bar-tools button")];
  const clipped = buttons.filter((button) => button.scrollWidth > button.clientWidth).map((button) => button.textContent.trim());
  const first = scroll.querySelector(".story, .check-list").firstElementChild;
  const range = document.createRange();
  range.selectNodeContents(first);
  const line = range.getClientRects()[0], hit = document.elementFromPoint(line.left + line.width / 2, line.top + line.height / 2);
  const under = bar.getBoundingClientRect().bottom, box = sheet.getBoundingClientRect(), top = scroll.getBoundingClientRect().top;
  return JSON.stringify({ buttons: buttons.map((button) => button.textContent.trim()), clipped, covered: !scroll.contains(hit) || line.top < under - 0.5,
    lineTop: Math.round(line.top), barFoot: Math.round(under), scrollTop: Math.round(top), sheetTop: Math.round(box.top), sheetHeight: Math.round(box.height),
    tools: [...sheet.querySelectorAll(".bar-tools button")].map((button) => { const box = button.getBoundingClientRect(), style = getComputedStyle(button), edge = parseFloat(style.borderTopWidth) || 0;
      return { text: button.textContent.trim(), boxHeight: Math.round(box.height), shownHeight: Math.round(box.height - 2 * edge), shownWidth: Math.round(box.width - 2 * edge), gapBelow: Math.round(under - box.bottom + edge), size: style.fontSize }; }),
    first: first.textContent.trim().slice(0, 60), sideways: scroll.scrollWidth > scroll.clientWidth + 1, scrolled: Math.round(scroll.scrollTop) });
})()`;
