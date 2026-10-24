#!/usr/bin/env node
// Real-browser check of manager mode on the page (docs/web-client-tasks.md), driven over the
// Chrome DevTools Protocol. Called by `test-web-client.py --part tasks --browser`.
//   web-tasks-check.mjs <site-url> <code> <project-id> <capture-dir> <home order> <slop-audit.js|""> <plans-id> <restart-file>
// <home order> is the project names as home must list them, comma-separated. <plans-id> is a
// project whose #1 waits for the owner's answer and whose #2 is ready to land.
// The project lands by hand and has: #1 and #4 waiting for a host, #2 ready with an item to
// check, #3 ready, #5 closed after a long report the manager heard of, #6 working (its prompt
// opens with "Owner asked:"), #7 and #8 landed with an item each to check (#7's summary has two
// sentences), and two pending approvals: a1 lands #3, a2 discards #4 (shown as
// "Approve land #3", "Approve discard #4"). Writing <restart-file> asks the test to restart
// the server with another site name, so it serves another build; it writes <restart-file>.done.
// Each capture is saved with the slop audit and, on a phone, the mobile audit of
// /ui-view-standards beside it (scripts/mobile-audit.js, or the file WSX_MOBILE_AUDIT names).
// The project's plain git worktree `old-work`, which no task owns, has a done agent session
// ("old chat"); the project's task copies are on `wsx/t<n>`.
import fs from "node:fs";
import path from "node:path";
import { launch, mobileAudit, pause } from "./web-cdp.mjs";

const [siteUrl, code, projectArg, captures, homeOrder, auditPath, plansArg, restartFile] = process.argv.slice(2);
const project = Number(projectArg);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems, go, text, count, key } = await launch({
  captures, prefix: "tasks-", timeoutSeconds: 300,
});
const auditSource = auditPath ? fs.readFileSync(auditPath, "utf8") : null;
const { file: mobilePath, source: mobileSource } = mobileAudit();
if (!mobileSource) await fail(`no mobile audit at ${mobilePath}; set WSX_MOBILE_AUDIT`);
let size = [390, 844, 2];
const phone = () => size[0] < 600;
const audits = [];

async function resize(next) {
  size = next;
  await viewport(...next);
  await pause(250);
}
const click = (selector) => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
// Type into the focused-to-be field as an input event does, the caret at the end.
const typeInto = (selector, value) => evaluate(`(() => { const field = document.querySelector(${JSON.stringify(selector)}); field.focus();
  field.value = ${JSON.stringify(value)}; field.setSelectionRange(field.value.length, field.value.length);
  field.dispatchEvent(new Event('input', { bubbles: true })); field.dispatchEvent(new KeyboardEvent('keyup', { bubbles: true })); })()`);
// Paste into a text box: a ClipboardEvent that carries a DataTransfer, as a browser sends it.
// "image" is a small PNG named "image.png", as iOS and Chrome name a pasted screenshot; any
// other value is plain text. It returns false when the page cancelled the paste.
const pasteInto = (selector, what) => evaluate(`(async () => {
  const data = new DataTransfer();
  if (${JSON.stringify(what)} === 'image') {
    const canvas = Object.assign(document.createElement('canvas'), { width: 8, height: 8 });
    canvas.getContext('2d').fillRect(0, 0, 4, 4);
    data.items.add(new File([await new Promise((done) => canvas.toBlob(done, 'image/png'))], 'image.png', { type: 'image/png' }));
  } else data.setData('text/plain', ${JSON.stringify(what)});
  const field = document.querySelector(${JSON.stringify(selector)});
  field.focus();
  return field.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
})()`);
// The keyboard up: the visual viewport shrinks, the layout viewport does not (iOS).
const keyboard = (height) => evaluate(`(() => {
  const viewport = window.visualViewport;
  delete viewport.height;
  if (${height} !== null) Object.defineProperty(viewport, "height", { configurable: true, get: () => ${height} });
  viewport.dispatchEvent(new Event("resize"));
})()`);

// Layout rules of every screen: nothing past the right edge, no raw null, one scroll container
// besides the bounded strip and the suggestion list, and the page itself never scrolls.
async function sound(label) {
  // ^ A sheet that is still sliding in passes the edge.
  await until("(document.querySelector('dialog.task-sheet')?.getAnimations().length ?? 0) === 0", `${label}: the sheet kept moving`);
  const seen = await evaluate(`(() => {
    const free = (node) => node.closest('.code-block pre, .table-wrap, .diagram, .since p');
    const wide = [...document.querySelectorAll('#app *')].filter((node) => !free(node) && node.getBoundingClientRect().right > innerWidth + 0.5);
    const scrollers = [...document.querySelectorAll('#app *')].filter((node) => node.tagName !== 'TEXTAREA' && !free(node) && !node.closest('.rail, .reservoir, .slash, .task-sheet')
      && /auto|scroll/.test(getComputedStyle(node).overflowY) && node.scrollHeight > node.clientHeight + 1);
    return { wide: wide.slice(0, 3).map((node) => node.className || node.tagName), scrollers: scrollers.map((node) => node.className),
      raw: /\\b(null|undefined|NaN)\\b/.test(document.querySelector('#app').innerText), page: document.scrollingElement.scrollTop + document.scrollingElement.scrollLeft };
  })()`);
  const where = `${label} at ${size[0]}x${size[1]}`;
  if (seen.wide.length) await fail(`${where}: elements pass the right edge: ${seen.wide}`);
  if (seen.scrollers.length > 1) await fail(`${where}: more than one scroll container: ${seen.scrollers}`);
  if (seen.raw) await fail(`${where} shows a raw null`);
  if (seen.page) await fail(`${where}: the page itself scrolled`);
}

// One capture with its audits. At most one filled accent control, no sideways scroll, no
// zooming field, and no other field in the way of the composer's iOS arrows.
async function shot(name, primary) {
  await sound(name);
  const label = `${name} at ${size[0]}x${size[1]}`;
  const file = `${size[0]}x${size[1]}-${name}`;
  const row = { capture: file };
  if (auditSource) {
    await evaluate(`globalThis.SLOP_AUDIT_PRIMARY = ${JSON.stringify(primary ?? null)}`);
    const audit = await evaluate(auditSource);
    fs.writeFileSync(path.join(captures, `tasks-${file}.slop.json`), JSON.stringify(audit, null, 1));
    if (audit.horizontalOverflow) await fail(`${label} overflows sideways`);
    if (audit.primaryAction.filledInViewport > 1) await fail(`${label} shows ${audit.primaryAction.filledInViewport} filled accent controls: ${JSON.stringify(audit.primaryAction.filled)}`);
    if (primary && !audit.primaryAction.inFirstViewport) await fail(`${label}: the primary action is not in view`);
  }
  if (mobileSource && phone()) {
    await evaluate("globalThis.MOBILE_AUDIT_GUTTER = 16");
    const mobile = await evaluate(mobileSource);
    fs.writeFileSync(path.join(captures, `tasks-${file}.mobile.json`), JSON.stringify(mobile, null, 1));
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
    Object.assign(row, { contentShare: mobile.contentShare, smallTargets: mobile.smallTargets.items.map((item) => `${item.label || item.el} ${item.size.join("x")}`),
      tinyText: mobile.tinyText.count, outsideGutter: mobile.outsideGutter.count });
  }
  audits.push(row);
  await capture(file);
}

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

async function pair() {
  await send("Page.navigate", { url: siteUrl });
  await until("!!document.querySelector('#pair-code')", "pairing form did not render");
  await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)}; input.form.requestSubmit(); })()`);
  await until("!!document.querySelector('.app')", "the app did not open after pairing");
}

// The top of the task sheet as words: what changed, what to know next, and whether a step path is left.
const story = () => evaluate(`({ summary: document.querySelector('.story-summary')?.textContent.trim() ?? '',
  next: document.querySelector('.story-next')?.textContent.trim() ?? '', steps: document.querySelectorAll('.story-steps').length })`);
// A task's "done" bubble, by the header of the worker's report.
const workerBubble = (n) => `[...document.querySelectorAll('.log .bubble')].find((node) => node.querySelector('.bubble-head')?.textContent.trim().replace(/\\s+/g, ' ') === '#${n} · done')`;
// The sheet's action buttons, under its top.
const nextButtons = () => text(".task-sheet .task-next button");
const clickNext = (label) => evaluate(`[...document.querySelectorAll('.task-sheet .task-next button')].find((node) => node.textContent === ${JSON.stringify(label)}).click()`);
// Nothing the new sheet replaced: no steps and no Review row.
async function lean(where) {
  if (await count(".task-sheet .story-steps")) await fail(`${where} still has the steps`);
  if ((await nextButtons()).includes("Review")) await fail(`${where} still has a Review button`);
}
// The sheet's open sections, by heading.
const openSections = () => evaluate("[...document.querySelectorAll('.task-sheet .scroll > .task-section:not(.task-fold):not(.timeline) > .task-heading')].map((node) => node.textContent.trim())");
// The reservoir's rows: [chip, title, context, actions], in the order shown.
const rows = () => evaluate(`[...document.querySelectorAll('.reservoir-list .need')].map((row) => [
  row.querySelector('.task-chip')?.textContent.trim() ?? '', row.querySelector('.row-label').textContent, row.querySelector('.row-context')?.textContent ?? '',
  [...row.querySelectorAll('.need-action')].map((node) => node.title).join('+')])`);
const rowOf = (label) => `[...document.querySelectorAll('.reservoir-list .need')].find((row) => ((row.querySelector('.task-chip')?.textContent.trim() ?? '') + ' ' + row.querySelector('.row-label').textContent).trim().startsWith(${JSON.stringify(label)}))`;
// The closed line as read: "6 need you · #6 <title> +1 more".
const reservoirWords = () => evaluate("document.querySelector('.reservoir-head')?.textContent.trim().replace(/\\s+/g, ' ') ?? ''");
async function openReservoir() {
  if (!(await evaluate("!!document.querySelector('.reservoir[data-open]')"))) await click(".reservoir-head");
  await until("!!document.querySelector('.reservoir-list')", "the reservoir did not open");
}
// The focus after a row cleared: [chip, label] of the row whose action has it, or the head.
const focused = () => evaluate(`(() => { const node = document.activeElement; const row = node?.closest('.reservoir .need');
  if (row) return [row.querySelector('.task-chip')?.textContent.trim() ?? '', row.querySelector('.row-label').textContent, node.title || node.textContent.trim()];
  if (node?.classList.contains('reservoir-head')) return ['head'];
  return node?.matches('.chat .composer-field') ? ['composer'] : [node?.tagName ?? 'none']; })()`);
// One action in a row, then that row leaves and the focus is on the next row's action (on the
// composer after the last row). The
// row is known by its item (`blocked-1`): the same task may come back as another item (#1
// under Running, then `land-1`). `attempts` are of 100 ms, as in `until`.
async function clearRow(label, action, attempts = 300) {
  const key = await evaluate(`${rowOf(label)}.dataset.key`);
  if (!key) await fail(`the row ${label} has no item key`);
  // ^ As a pointer or a key press does, the button takes the focus before it acts.
  await evaluate(`(() => { const button = [...${rowOf(label)}.querySelectorAll('.need-action')].find((node) => node.title === ${JSON.stringify(action)});
    button.focus(); button.click(); })()`);
  await until(`!document.querySelector('.reservoir .need[data-key=${JSON.stringify(key)}]')`, `${action} did not clear the row ${label} (${key})`, attempts);
  // ^ After the last row the reservoir goes and the composer has the focus.
  const inList = "!!document.activeElement?.closest('.reservoir .need .need-action, .reservoir .need .row-open, .reservoir-head') || (!document.querySelector('.reservoir') && !!document.activeElement?.matches('.chat .composer-field'))";
  for (let attempt = 0; attempt < 100 && !(await evaluate(inList)); attempt++) await pause(100);
  if (!(await evaluate(inList))) await fail(`after ${action} on ${label} the focus is on ${JSON.stringify(await focused())}, not on the next row`);
  const now = await focused();
  if (await evaluate("document.activeElement?.closest('.need')?.dataset.key ?? ''") === key) await fail(`after ${action} the focus stayed on ${label}`);
  return now;
}
async function closeReservoir() {
  if (await evaluate("!!document.querySelector('.reservoir[data-open]')")) await click(".reservoir-head");
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
const sheetState = () => evaluate("document.querySelector('.task-sheet .bar-state')?.textContent ?? ''");
const atEnd = "(() => { const log = document.querySelector('.log'); return log.scrollHeight - log.scrollTop - log.clientHeight < 2; })()";
const bubbles = () => evaluate(`[...document.querySelectorAll('.log .bubble')].map((node) => ({ side: node.dataset.side, first: node.hasAttribute('data-first'),
  last: node.hasAttribute('data-last'), author: node.querySelector('.bubble-author')?.textContent ?? '', tag: node.querySelector('.bubble-tag')?.textContent ?? '', text: node.querySelector('.bubble-box').textContent.trim().replace(/\\s+/g, ' ') }))`);

await resize([390, 844, 2]);
await pair();
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
    const rows = () => [...document.querySelectorAll('.home-rows > .row')];
    const frames = () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    // The manager of ui runs a turn: typing dots on its row.
    update({ tasks: { ...state.tasks, [ui]: { ...state.tasks[ui], busy: true } } });
    await frames();
    const typing = document.querySelectorAll('.home-row .row-typing .typing-dot').length;
    toast('Saved the page');
    await frames();
    const notice = !!document.querySelector('.notice .notice-timer');
    // calm becomes the newest: it moves up; then it goes: its row fades out where it was.
    const before = rows().map((row) => row.dataset.key);
    update({ tasks: { ...state.tasks, [calm.id]: { ...state.tasks[calm.id], last_unix_ms: Date.now() + 1e7 } } });
    await frames();
    const moved = rows().map((row) => row.dataset.key).join() !== before.join();
    const glides = document.getAnimations().filter((animation) => animation.effect?.target?.matches?.('.home-rows > .row')
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
  const seen = await run(false);
  if (seen.typing !== 3 || !seen.notice || !seen.moved || seen.glides < 1 || seen.ghosts !== 1 || seen.loading !== "status") await fail(`motion at ${w}px: ${JSON.stringify(seen)}`);
  await expectAll("typing dots", ".row-typing .typing-dot", "pulse");
  await expectAll("a working row", ".home-row .pulse", "pulse");
  await expectAll("the count badge", ".home-row .count", "pop");
  await expectAll("the status line", ".home-row .row-context:not([data-loading])", "arrive");
  await expectAll("the notice", ".notice", "slide-down");
  await expectAll("the toast timer", ".notice-timer", "drain");
  await expectAll("the loading placeholder", ".skeleton", "shimmer");
  await capture(`${w}-motion-home-typing`);
  await pause(60);
  await capture(`${w}-motion-home-toast`);
  // Reduced motion: the same states, no animation and no script-driven motion.
  await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
  const still = await evaluate(`(async () => {
    const { state, update } = await ${pageModule("store.js")};
    const calm = state.projects.find((project) => project.name === 'calm');
    const frames = () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    update({ tasks: { ...state.tasks, [calm.id]: { ...state.tasks[calm.id], last_unix_ms: 1 } } });
    await frames();
    const rows = document.getAnimations().filter((animation) => animation.effect?.target?.matches?.('.home-rows > .row')).length;
    update(globalThis.motionKept);
    return rows;
  })()`);
  if (still) await fail(`motion at ${w}px: ${still} row animations run with reduced motion`);
  for (const selector of [".row-typing .typing-dot", ".home-row .pulse", ".home-row .count", ".home-row .row-context", ".notice", ".notice-timer", ".skeleton"]) {
    const seen = await names(selector);
    if (seen.some((item) => item !== "none")) await fail(`motion at ${w}px: ${selector} animates with reduced motion: ${JSON.stringify(seen)}`);
  }
  await capture(`${w}-motion-home-reduced`);
  await send("Emulation.setEmulatedMedia", { features: [] });
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

// --- Home: one row per project, needs first, then by activity; the bottom bar is Home, Sessions and Settings.
await go("#/");
await until("document.querySelectorAll('.home-row').length >= 3", "home does not list the projects");
// ^ The task summaries load after the projects; the order settles when they are in.
await until(`[...document.querySelectorAll('.home-row .row-label')].map((node) => node.textContent).join() === ${JSON.stringify(homeOrder)}`, "home did not settle in the expected order", 30);
const homeRows = await evaluate(`[...document.querySelectorAll('.home-row')].map((row) => ({ name: row.querySelector('.row-label').textContent,
  count: row.querySelector('.count')?.textContent ?? '', status: row.querySelector('.row-context').textContent, state: row.querySelector('.row-state').textContent }))`);
const uiRow = homeRows.find((row) => row.name === "ui");
if (uiRow.count !== "6" || uiRow.status !== "#1 needs to reach crates.io" || !/ago|now/.test(uiRow.state)) await fail(`the ui row reads ${JSON.stringify(uiRow)}`);
const calmRow = homeRows.at(-1);
if (calmRow.name !== "calm" || calmRow.count || calmRow.status !== "No tasks yet" || calmRow.state) await fail(`the calm row reads ${JSON.stringify(calmRow)}`);
const plansRow = homeRows.find((row) => row.name === "plans");
// ^ #1 waits for an answer; #2 is ready to land.
if (plansRow?.status !== "#1 needs an answer" || plansRow.count !== "2") await fail(`the plans row reads ${JSON.stringify(plansRow)}`);
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
await resize([390, 844, 2]);
await go("#/");
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
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
const still = await inboxMarks();
await send("Emulation.setEmulatedMedia", { features: [] });
if (still.some((mark) => mark.animated) || !still.some((mark) => mark.running)) await fail(`an inbox mark moves under reduced motion: ${JSON.stringify(still)}`);
await shot("sessions");
// A project's worktrees are in its chat's ⋯ menu; the plain worktree is listed like any
// other, with no Adopted list.
await click(".tabbar a[href='#/']");
await until("location.hash === '#/' && !!document.querySelector('.home-row')", "the Home tab did not return home");
await go(chat);
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
await evaluate(`[...document.querySelectorAll('.home-row')].find((row) => row.querySelector('.row-label').textContent === 'ui').click()`);
await until(`location.hash === ${JSON.stringify(chat)} && !!document.querySelector('.composer-field') && !!document.querySelector('.reservoir-head') && !!document.querySelector('.log .bubble')`, "a home row did not open the chat");
if (await count(".tabbar")) await fail("the chat shows the bottom bar");
// No Adopted list on the chat.
if (await count(".adopted")) await fail("the project chat shows the Adopted list");

// "Since you left": this device left the project two hours ago; the line names what happened.
// It sits in one compact line and goes by itself once the newest reply is on screen or the owner
// sends. The log is scrolled to its top first, so no reply is on screen when the line appears.
const scrollLogToTop = () => evaluate("document.querySelector('.log').scrollTop = 0");
async function comeBack() {
  await until("!document.querySelector('.since')", "an old since line is still shown");
  await scrollLogToTop();
  await pause(300);
  await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 })); document.dispatchEvent(new Event('visibilitychange'))`);
  await until("/Since you left: .*#1 #2 #3 #4.* started.* repl(y|ies)/.test(document.querySelector('.since')?.textContent ?? '')", "no since-you-left line");
}
await go("#/");
// ^ The chat notes the time it closes at: set the earlier time after home replaced it.
await until("!!document.querySelector('.home-row') && !document.querySelector('.composer-field')", "home did not replace the chat");
await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 }))`);
await go(chat);
await until("!!document.querySelector('.log .bubble')", "the chat shows no bubble");
await comeBack();
if ((await evaluate("getComputedStyle(document.querySelector('.since')).animationName")) !== "since-in") await fail("the since line does not expand in");
const sinceBox = await evaluate("(() => { const line = document.querySelector('.since'); const p = line.querySelector('p').getBoundingClientRect(), b = line.querySelector('button').getBoundingClientRect();"
  + " return { height: line.getBoundingClientRect().height, inline: b.top < p.bottom && b.bottom > p.top }; })()");
if (sinceBox.height > 40 || !sinceBox.inline) await fail(`the since line is not one compact line: ${JSON.stringify(sinceBox)}`);
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
// ^ The chat opens at its end again for what follows.
await go("#/");
await go(chat);
await until("!!document.querySelector('.log .bubble') && !!document.querySelector('.reservoir-head')", "the chat did not come back");

// --- Mentions in the composer (docs/web-client-tasks.md "Project screen"): `/` lists the owner's
// skills and inserts a request to use one (the manager takes no slash command), `@` lists the
// other projects; no row of the first read slid in.
if (await count(".log [data-fresh]")) await fail("rows of the first read play the entry");
const optionNamed = (name) => `[...document.querySelectorAll('.slash-option')].find((node) => node.querySelector('.slash-name')?.textContent === ${JSON.stringify(name)})`;
await typeInto(".composer-field", "/fixture");
await until(`!!${optionNamed("/fixture-skill")}`, "the / list does not show the fixture skill", 30);
const skillOption = await evaluate(`(() => { const node = ${optionNamed("/fixture-skill")}; return { description: node.querySelector('.slash-description').textContent, source: node.querySelector('.slash-source').textContent,
  label: document.querySelector('.slash-list').getAttribute('aria-label') }; })()`);
if (skillOption.description !== "Fixture skill for the manager" || skillOption.source !== "skill" || skillOption.label !== "Skills") await fail(`the skill option reads ${JSON.stringify(skillOption)}`);
await shot("mentions-skills", ".composer-send");
await evaluate(`${optionNamed("/fixture-skill")}.click()`);
if ((await evaluate("document.querySelector('.composer-field').value")) !== "Use owner:fixture-skill: ") await fail(`a picked skill reads ${await evaluate("document.querySelector('.composer-field').value")}`);
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
// The owner's message on the right, the manager's reply on the left with its author, and #5's
// long report as one bubble by "#5 worker" with its header, shown whole.
let shown = await bubbles();
const ownerAsk = shown.find((bubble) => bubble.side === "out" && bubble.text.startsWith("RUN wsx task land 3"));
// ^ ui's manager is named "Atlas" (tasks.manager.name); plans' reads "Manager".
const managerReply = shown.find((bubble) => bubble.side === "in" && bubble.author === "Atlas");
if (!ownerAsk || ownerAsk.author || !managerReply) await fail(`the bubbles are ${JSON.stringify(shown)}`);
const report = `[...document.querySelectorAll('.log .bubble')].find((node) => node.querySelector('.bubble-author')?.textContent === '#5 worker')`;
await until(`!!${report}`, "the long report has no worker bubble");
const head = await evaluate(`${report}.querySelector('.bubble-head').textContent.trim().replace(/\\s+/g, ' ')`);
if (head !== "#5 · ready") await fail(`the report's header reads ${head}`);
if (!(await evaluate(`/line 30 of the report/.test(${report}.textContent) && !${report}.querySelector('button.link-action')`))) await fail("the long report is not shown whole");
await managerWidth("the phone chat");
await shot("project", ".composer-send");
// The chat opens at its newest message; scrolled up, it shows Jump to latest.
await until(atEnd, "the manager chat did not open at its end");
await evaluate("document.querySelector('.log').scrollTop = 0");
await until("!!document.querySelector('.jump')", "no jump button after scrolling the manager chat up");
await click(".jump");
await until(`!document.querySelector('.jump') && ${atEnd}`, "Jump to latest did not go to the end of the manager chat");
// A landed task is one worker bubble "#7 · done" with its whole summary. No muted line
// repeats the landing.
await until(`!!${workerBubble(7)} && !!${workerBubble(8)}`, "the landed tasks have no worker bubbles");
const done = await evaluate(`({ head: ${workerBubble(7)}.querySelector('.bubble-head').textContent.trim().replace(/\\s+/g, ' '),
  body: ${workerBubble(7)}.querySelector('.bubble-body').textContent.trim(), more: !!${workerBubble(7)}.querySelector('button.link-action') })`);
if (done.head !== "#7 · done" || !done.body.startsWith("did: WORK d.txt HV done.") || !/More words follow/.test(done.body) || done.more) await fail(`the done bubble reads ${JSON.stringify(done)}`);
if (await evaluate("[...document.querySelectorAll('.status-line')].some((node) => /#[78] (landed|done)/.test(node.textContent))")) await fail("a muted line repeats a landing");
// ^ The owner's write grant for the manager is a settled line (docs/tasks.md "Permissions").
await until("[...document.querySelectorAll('.status-line')].some((node) => /Allowed by the owner: the manager writes \\S+\\/granted until /.test(node.textContent))", "the manager's grant left no muted line", 30);

// --- The reservoir: one line over the composer that names the running task (its pulsing chip
// and its title without "Owner asked:") and the items to check; a tap opens the decision list,
// one row per item with its one action, then Running and To check. No deploy facts here.
// Opening it keeps the chat at its end.
const closedLine = await reservoirWords();
if (!/^6 need you · #6 keep a worker busy WAIT \S+ · 2 to check$/.test(closedLine) || /Owner|more/.test(closedLine)) await fail(`the reservoir reads ${closedLine}`);
const line = await evaluate(`(() => { const head = document.querySelector('.reservoir-head'), title = head.querySelector('.reservoir-title');
  return { height: head.getBoundingClientRect().height, control: parseFloat(getComputedStyle(head).minHeight), pulse: !!head.querySelector('.task-chip .mark[data-running]'),
    cut: title.scrollWidth > title.clientWidth, right: head.getBoundingClientRect().right <= innerWidth + 0.5 }; })()`);
if (line.height > line.control + 1 || !line.pulse || !line.right) await fail(`the closed line is not one line with a pulsing chip: ${JSON.stringify(line)}`);
if (await count(".reservoir-list")) await fail("the reservoir is open before a tap");
await chipsOnLine("the closed reservoir line", ".reservoir-head .task-chip", [".reservoir-head", ".reservoir-title"]);
const closedSize = await evaluate("getComputedStyle(document.querySelector('.reservoir-title')).fontSize");
await oneWorkingColor("the closed reservoir and the chat's chips", 1);

// Motion of the closed line (docs/web-client-tasks.md "Motion"): with three running tasks the
// running slot shows each in turn, waits while it has the focus, and under reduced motion keeps
// the newest and counts the rest. The store is changed in the page, as an event would.
const inStore = (body) => evaluate(`(async () => { const { state, update } = await import(new URL('store.js', document.querySelector('script[type=module]').src)); ${body} })()`);
const original = await inStore(`return JSON.parse(JSON.stringify(state.tasks[${project}]))`);
const runners = () => inStore(`const ov = state.tasks[${project}], base = ov.tasks.find((task) => task.stage === 'working');
  update({ tasks: { ...state.tasks, [${project}]: { ...ov, tasks: [...ov.tasks, { ...base, id: 91, title: 'second runner' }, { ...base, id: 92, title: 'third runner' }] } } })`);
const restored = () => inStore(`update({ tasks: { ...state.tasks, [${project}]: ${JSON.stringify(original)} } })`);
const slot = () => evaluate("document.querySelector('.reservoir-head .ticker-item .task-chip')?.textContent.trim() ?? ''");
const slotMotion = () => evaluate("getComputedStyle(document.querySelector('.reservoir-head .ticker-item')).animationName");
await runners();
await until("!!document.querySelector('.reservoir-head .ticker-item')", "the closed line has no running slot");
const turned = new Set();
for (let at = 0; at < 80 && turned.size < 3; at++) {
  turned.add(await slot());
  await pause(500);
}
if ([...turned].sort().join() !== "#6,#91,#92") await fail(`the running slot showed ${[...turned]}, not each of the three tasks`);
if ((await slotMotion()) !== "ticker-in") await fail(`the running slot enters with ${await slotMotion()}`);
if (!/^\d\/3$/.test(await evaluate("document.querySelector('.reservoir-more')?.textContent ?? ''"))) await fail("the closed line has no place count");
await capture(`${size[0]}x${size[1]}-ticker`);
await evaluate("document.querySelector('.reservoir-head').focus()");
const held = await slot();
await pause(5000);
if ((await slot()) !== held) await fail("the running slot turned while it had the focus");
await evaluate("document.querySelector('.reservoir-head').blur()");
if (!(await evaluate("!!document.querySelector('.reservoir-words .tick')"))) await fail("the counts do not tick");
// Reduced motion: no animation, the newest task and the count of the others.
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
await go("#/");
await go(chat);
await until("!!document.querySelector('.reservoir-head')", "the chat did not come back for reduced motion");
await restored();
await runners();
await until("!!document.querySelector('.reservoir-head .ticker-item')", "no running slot under reduced motion");
const calmSlot = await slot();
await pause(4500);
const calmNow = await evaluate(`({ slot: document.querySelector('.reservoir-head .ticker-item .task-chip').textContent.trim(), more: document.querySelector('.reservoir-more').textContent,
  item: getComputedStyle(document.querySelector('.reservoir-head .ticker-item')).animationName, tick: getComputedStyle(document.querySelector('.reservoir-words .tick')).animationName,
  mark: getComputedStyle(document.querySelector('.reservoir-head .mark')).transitionDuration, pulse: getComputedStyle(document.querySelector('.reservoir-head .mark[data-running]')).animationName })`);
if (calmSlot !== "#92" || calmNow.slot !== "#92" || calmNow.more !== "+2 more") await fail(`under reduced motion the slot reads ${JSON.stringify({ calmSlot, ...calmNow })}`);
if (calmNow.item !== "none" || calmNow.tick !== "none" || calmNow.mark !== "0s" || calmNow.pulse !== "none") await fail(`reduced motion still animates: ${JSON.stringify(calmNow)}`);
await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "no-preference" }] });
await restored();
await until("!document.querySelector('.reservoir-more')", "the closed line kept the extra tasks");

// Open, the summary line is gone and the chevron has not moved in its row.
const chevron = () => evaluate(`(() => { const head = document.querySelector('.reservoir-head'), box = head.getBoundingClientRect(), icon = head.querySelector('.icon').getBoundingClientRect();
  return { right: Math.round(box.right - (icon.left + icon.width / 2)), down: Math.round(icon.top + icon.height / 2 - box.top), height: Math.round(box.height) }; })()`);
const chevronClosed = await chevron();
await openReservoir();
await pause(400);
const chevronOpen = await chevron();
if (JSON.stringify(chevronClosed) !== JSON.stringify(chevronOpen)) await fail(`the chevron moved: closed ${JSON.stringify(chevronClosed)}, open ${JSON.stringify(chevronOpen)}`);
if (await count(".reservoir-words, .reservoir-more, .reservoir-checks")) await fail("the open reservoir still shows the closed summary");
if (!(await evaluate("document.querySelector('.reservoir-head').getAttribute('aria-label') === 'Hide the list'"))) await fail("the open head has no name");
await shot("reservoir-open-head", ".composer-send");
if (!(await evaluate(atEnd))) await fail(`opening the reservoir took the chat off its end: ${JSON.stringify(await evaluate("(() => { const log = document.querySelector('.log'); return { top: log.scrollTop, height: log.scrollHeight, client: log.clientHeight, jump: !!document.querySelector('.jump') }; })()"))}`);
const listedRows = await rows();
const rowWords = listedRows.map((row) => `${row[0]} ${row[3]}`);
// ^ Done tasks (#7, #8) are not in it; the To check list is one row at its end. A task
// without --title is named by its worker summary's first sentence ("did: WORK b.txt").
const expectedRows = ["#1 Allow", "#4 Allow", " Deny+Approve", " Deny+Approve", "#2 Land", "#3 Land", "#6 ", " "];
if (rowWords.join("|") !== expectedRows.join("|")) await fail(`the reservoir rows are ${JSON.stringify(listedRows)}`);
if (listedRows[0][2] !== "needs to reach crates.io" || listedRows[2][1] !== "Approve land #3" || listedRows[3][1] !== "Approve discard #4" || listedRows[2][2] !== "did: WORK b.txt"
  || listedRows[4][2] !== "ready to land" || listedRows[7][1] !== "2 to check") await fail(`the reservoir rows read ${JSON.stringify(listedRows)}`);
if (!/^keep a worker busy WAIT /.test(listedRows[6][1])) await fail(`the running task's title is ${listedRows[6][1]}`);
if ((await text(".reservoir-label")).join() !== "Running" || !(await evaluate(`!!document.querySelector('.reservoir-list[aria-label=Running]').contains(${rowOf("#6")})`))) await fail("#6 is not under Running");
if (!(await evaluate(`${rowOf("#6")}.querySelector('.task-chip .mark[data-running]')`))) await fail("the running task's chip does not pulse");
// ^ Open, the running row reads like the closed line: the same small size, rows close together.
const runningRow = await evaluate(`(() => { const row = ${rowOf("#6")}, link = row.querySelector('.row-link'), style = getComputedStyle(link);
  return { label: getComputedStyle(row.querySelector('.row-label')).fontSize, chip: getComputedStyle(row.querySelector('.task-chip')).fontSize,
    gap: parseFloat(style.paddingTop) + parseFloat(style.paddingBottom), minHeight: style.minHeight }; })()`);
if (runningRow.label !== closedSize || runningRow.chip !== closedSize || runningRow.gap > 8 || !["0px", "auto"].includes(runningRow.minHeight)) {
  await fail(`the open running row does not match the closed line (${closedSize}): ${JSON.stringify(runningRow)}`);
}
await chipsOnLine("a running row", ".reservoir-running .task-chip", [".row-link", ".row-label"]);
const chipName = await evaluate(`${rowOf("#1")}.querySelector('.task-chip').getAttribute('aria-label')`);
if (chipName !== "#1, needs you") await fail(`a chip's accessible name is ${chipName}`);
const runningName = await evaluate(`${rowOf("#6")}.querySelector('.task-chip').getAttribute('aria-label')`);
if (runningName !== "#6, working") await fail(`a working chip's accessible name is ${runningName}`);
if (await count(".reservoir .facts-line")) await fail("the reservoir still shows the deploy facts");
await shot("reservoir-open", ".composer-send");
await resize([390, 420, 2]);
await shot("reservoir-short", ".composer-send");
await resize([320, 568, 2]);
await shot("reservoir-narrow", ".composer-send");
await resize([390, 844, 2]);
// Clear in place, one row after another: Allow #1 grants its host and resumes it, the focus
// moves to #4's Allow; Approve land #3 lands it; #1, ready, lands. The chat stays at its end,
// and the settled items become muted lines that join when close together.
let moved = await clearRow("#1", "Allow");
if (`${moved[0]} ${moved[2]}` !== "#4 Allow") await fail(`after Allow #1 the focus is on ${JSON.stringify(moved)}`);
// ^ The chat says what Allow did as a status line; no notice repeats it.
await until("[...document.querySelectorAll('.status-line')].some((node) => /#1 may reach crates\\.io \\(owner\\)/.test(node.textContent))", "Allow in the reservoir left no status line", 60);
if (await evaluate("[...document.querySelectorAll('.notice')].some((node) => /#1:/.test(node.textContent))")) await fail("a notice repeats the status line of Allow");
moved = await clearRow("Approve land #3", "Approve");
if (await evaluate("[...document.querySelectorAll('.notice')].some((node) => /Approved land #3/.test(node.textContent))")) await fail("a notice repeats the status line of Approve");
await until(`!${rowOf("Approve land #3")} && !${rowOf("#3")}`, "the reservoir kept the approved landing", 300);
// ^ The approval is a muted line; the landing is #3's "done" bubble, not a second line.
await until("[...document.querySelectorAll('.status-line')].some((node) => /Approved land #3/.test(node.textContent))", "the approval left no muted line", 300);
await until(`!!${workerBubble(3)}`, "the landing of #3 has no done bubble", 300);
if (await evaluate("[...document.querySelectorAll('.status-line')].some((node) => /#3 landed/.test(node.textContent))")) await fail("a muted line repeats the landing of #3");
await until(`(${rowOf("#1")}?.querySelector('.need-action')?.title ?? '') === 'Land'`, "#1 is not ready to land after Allow", 300);
moved = await clearRow("#1", "Land");
if (!(await evaluate(atEnd)) || await count(".jump")) await fail("acting in the reservoir moved the chat");
if (await count(".need-card, .tray")) await fail("an action brought back a card or the tray");
await shot("reservoir-acted", ".composer-send");
await closeReservoir();

// --- The ⋯ menu: Page and Sessions; the worktree list hides task sessions.
await click(".more-button");
if ((await text(".more-menu a")).join() !== "Page,Sessions") await fail(`the ⋯ menu offers ${await text(".more-menu a")}`);
await shot("more-menu", ".composer-send");
await evaluate("[...document.querySelectorAll('.more-menu a')].find((node) => node.textContent === 'Sessions').click()");
await until(`location.hash === '#/p/${project}/sessions' && !!document.querySelector('.worktree')`, "Sessions did not open the worktree list");
const worktreeSessions = await text(".worktree .row-session-name");
if (worktreeSessions.some((name) => /^task-/.test(name))) await fail(`the worktree list shows task sessions: ${worktreeSessions}`);
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until(`location.hash === ${JSON.stringify(chat)} && !!document.querySelector('.reservoir-head')`, "Back did not return to the chat");

// --- The project page: markdown and a mermaid graph in the sheet, with its last change.
await click(".more-button");
await evaluate("[...document.querySelectorAll('.more-menu a')].find((node) => node.textContent === 'Page').click()");
await until(`location.hash === '#/p/${project}/page' && !!document.querySelector('dialog.task-sheet[open] .project-page h1')`, "Page did not open the project page");
await until("!!document.querySelector('.project-page .diagram img')", "the page's mermaid graph did not draw", 30);
if (!/^Updated /.test(await sheetState())) await fail(`the page's top row reads ${await sheetState()}`);
const pageHeads = await text(".project-page h2");
if (pageHeads.join() !== "Goal,Tasks,Decisions,Open questions") await fail(`the page's sections are ${pageHeads}`);
// ^ The deploy and the main checkout moved here from the reservoir, above the markdown. They
// come from a separate brief request, which can answer after the page has drawn and again as
// tasks change: wait for the words, not only the line.
const factsRead = (facts) => /^Deploy: [0-9a-f]{8}, (up to date with main|\d+ commits? behind main)\./.test(facts) && /Main checkout on main, /.test(facts);
let facts = "";
for (let attempt = 0; attempt < 300 && !factsRead(facts); attempt++) {
  if (attempt) await pause(100);
  facts = (await text(".project-page .facts-line"))[0] ?? "";
}
if (!factsRead(facts)) await fail(`the page's facts read ${facts}`);
await shot("page");
await key("Escape", 27);
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Escape did not close the page");

// --- A turn of the manager: two messages group on the right; while it runs a typing bubble
// ends the chat and the header says "typing"; the reply edits the page (with a link to it),
// has short chips (the same task twice as two plain chips) and a table that scrolls inside it.
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
if (headDots.join() !== "pulse 0s,pulse 0.32s,pulse 0.64s") await fail(`the header's typing dots are ${headDots}`);
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
  const box = table.getBoundingClientRect(); return { right: box.right <= bubble.right + 0.5, scrolls: table.scrollWidth > table.clientWidth }; })()`);
if (!inside.right || !inside.scrolls) await fail(`the table does not stay inside its bubble: ${JSON.stringify(inside)}`);
await chipsOnLine("the chat's bubbles", ".log .bubble .task-chip");
const replyChips = await evaluate(`[...${tableReply}.querySelectorAll('.task-chip')].map((chip) => chip.textContent.trim())`);
if (replyChips.filter((chip) => chip === "#4").length !== 2 || replyChips.some((chip) => !/^#\d+$/.test(chip))) await fail(`the reply's chips read ${JSON.stringify(replyChips)}`);
await until("!document.querySelector('.log .typing')", "the typing bubble stayed after the reply", 30);
if (await evaluate("document.querySelector('.manager .bar-state')?.textContent ?? ''")) await fail("the header still names the manager after its turn");
await shot("project-reply", ".composer-send");
// The second message's reply links "project page"; the page has the manager's edit.
await until("document.querySelectorAll('.log .page-link').length > 0", "the reply about the page has no link", 60);
await evaluate("[...document.querySelectorAll('.log .page-link')].at(-1).click()");
await until("/Ship the chat/.test(document.querySelector('.project-page')?.textContent ?? '')", "the page does not show the manager's edit", 30);
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close the page");

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
const phoneSheet = await evaluate("({ sheet: getComputedStyle(document.querySelector('dialog.task-sheet')).animationName, backdrop: getComputedStyle(document.querySelector('dialog.task-sheet'), '::backdrop').animationName })");
if (phoneSheet.sheet !== "sheet-in" || phoneSheet.backdrop !== "arrive") await fail(`the sheet enters with ${JSON.stringify(phoneSheet)}`);
if (await logTop() !== top0) await fail(`a row moved the chat from ${top0} to ${await logTop()}`);
await key("Escape", 27);
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Escape did not close the sheet a row opened");
await closeReservoir();
if (await logTop() !== top0) await fail(`closing the sheet moved the chat from ${top0} to ${await logTop()}`);
await evaluate("[...document.querySelectorAll('.log .task-chip')].find((chip) => chip.textContent.trim() === '#2').click()");
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('dialog.task-sheet[open] .story')`, "a chip did not open the task sheet");
if (await logTop() !== top0) await fail(`a chip moved the chat from ${top0} to ${await logTop()}`);
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close the sheet a chip opened");
await click(".jump");
await until(atEnd, "Jump to latest did not return to the end");
await typeInto(".composer-field", "a draft that stays");
await evaluate("[...document.querySelectorAll('.log .task-chip')].find((chip) => chip.textContent.trim() === '#2').click()");
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('dialog.task-sheet[open] .story')`, "a chip did not open the task sheet");
await until("document.querySelector('dialog.task-sheet')?.getAnimations().length === 0", "the sheet kept moving");
const sheetBox = await evaluate("(() => { const r = document.querySelector('dialog.task-sheet').getBoundingClientRect(); return { top: Math.round(r.top), height: Math.round(r.height) }; })()");
if (Math.abs(sheetBox.height - 844 * 0.85) > 2 || Math.abs(sheetBox.top - 844 * 0.15) > 2) await fail(`the sheet is not 85% high: ${JSON.stringify(sheetBox)}`);
await key("Escape", 27);
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Escape did not close the sheet");
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
// act_budget); followEnd gives back what it spent, so the actions after it are not refused.
let said = 0;
const say = (words) => (said += 1, evaluate(`fetch('api/tasks/${project}/say', { method: 'POST', headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ text: ${JSON.stringify(words)} }) }).then((response) => response.status)`));
const fromEnd = () => evaluate("(() => { const log = document.querySelector('.log'); return Math.round(log.scrollHeight - log.scrollTop - log.clientHeight); })()");
const drawnDiagrams = "document.querySelectorAll('.log .bubble .diagram img').length";
// The manager answers with a diagram; the bubble shows "Drawing the diagram…" first.
async function diagramReply() {
  const before = await evaluate(drawnDiagrams);
  if (await say("DIAGRAM") !== 200) await fail("the message for a diagram reply was not taken");
  await until(`${drawnDiagrams} > ${before} && !document.querySelector('.log .typing')`, "the reply with a diagram did not arrive", 60);
  await evaluate("Promise.all([...document.querySelectorAll('.log .diagram img')].map((image) => image.decode()))");
  await pause(300);
}
const toLatest = async (where) => {
  await evaluate("document.querySelector('.jump')?.click()");
  await until(atEnd, `${where}: Jump to latest did not return to the end`);
};
// The probe is the first bubble wholly in view; its distance from the screen's top must not change.
const probe = "document.querySelector('[data-follow-probe]').getBoundingClientRect().top";
async function followEnd(where) {
  said = 0;
  // (a) At the end, a reply whose diagram draws after it shows: still at the end.
  await toLatest(where);
  await diagramReply();
  const lateDiagram = await fromEnd();
  if (lateDiagram > 2) await fail(`${where}: a diagram that drew late left the chat ${lateDiagram} px above its end`);
  for (let more = 0; more < 4 && await evaluate("(() => { const log = document.querySelector('.log'); return log.scrollHeight < log.clientHeight * 3; })()"); more++) await diagramReply();
  // (b, c) Scrolled up: rows below, a late diagram below, the "Since you left" line above, rows
  // inserted above (as older messages would be) and removed again keep the bubble in view where it is.
  const drift = [];
  for (const anchoring of ["auto", "none"]) {
    // ^ iOS Safari has no CSS scroll anchoring: with it off, Chrome scrolls as Safari does.
    await evaluate(`document.querySelector('.log').style.overflowAnchor = '${anchoring}'`);
    await evaluate("(() => { const log = document.querySelector('.log'); log.scrollTop = log.scrollHeight - log.clientHeight * 2; })()");
    await pause(200);
    const start = await evaluate(`(() => { const top = document.querySelector('.log').getBoundingClientRect().top;
      document.querySelector('[data-follow-probe]')?.removeAttribute('data-follow-probe');
      const bubble = [...document.querySelectorAll('.log > .bubble')].find((node) => node.getBoundingClientRect().top > top + 20);
      bubble.setAttribute('data-follow-probe', ''); return ${probe}; })()`);
    const moved = async (step) => drift.push(`${anchoring} ${step} ${Math.round((await evaluate(probe)) - start)}`);
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
    await keyboard(420);
    await pause(300);
    const keyboardUp = await fromEnd();
    await diagramReply();
    const replied = await fromEnd();
    await evaluate("document.activeElement.blur()");
    await keyboard(null);
    if (keyboardUp > 2 || replied > 2) await fail(`${where}: with the keyboard up the chat is ${keyboardUp} px, after a reply ${replied} px above its end`);
  }
  await pause(said * 3000);
}
await followEnd("390 px");

// --- The task sheet of #2: the stage, one sentence of what changed and the one action; the
// report folded (Changes, Evidence, To check, Left open) and History folded. No verdict, no
// Asked, no revision number. Land, then its item is live under To check.
await go(page(2));
await until("document.querySelectorAll('.task-sheet .task-fold').length >= 4", "the task sheet did not show the folded report");
let told = await story();
if (told.steps || told.summary !== "did: WORK a.txt HV" || told.next !== "Land it to put it on main.") await fail(`the top of a ready task reads ${JSON.stringify(told)}`);
if (await sheetState() !== "needs you") await fail(`a ready task's state reads ${await sheetState()}`);
const folds = await text(".task-sheet .task-fold > .fold-head");
if (folds.join("|") !== "Changes|Evidence · 1 check passed|To check · 1 after it lands|Left open · 1") await fail(`the folds read ${JSON.stringify(folds)}`);
const sheetWords = await evaluate("document.querySelector('dialog.task-sheet').innerText");
if (/Asked|Human Verify|Decisions|Review|Pass r\d|Verified/.test(sheetWords)) await fail(`the sheet still shows the verdict or old sections: ${sheetWords}`);
if (await count(".task-sheet .label, .task-sheet .story-explain, .verify, .verify-note")) await fail("the sheet still has labels, Explain or the Verify panel");
if ((await text(".task-next button")).join() !== "Land,Work by hand,Try,Hold for me") await fail(`a ready task offers ${await text(".task-next button")}`);
if (await evaluate("/\\br\\d+\\b/.test(document.querySelector('.task-sheet .bar').textContent + document.querySelector('.task-next').textContent)")) await fail("the sheet names a revision");
await shot("task-sheet", ".composer-send");
// Evidence opens to the checks and what the worker showed; More shows the decisions.
await evaluate("[...document.querySelectorAll('.task-fold > .fold-head')].find((node) => node.textContent.startsWith('Evidence')).click()");
await until("/test ! -e broken\\.txt/.test(document.querySelector('.task-fold[open]')?.textContent ?? '') && /the fixture ran/.test(document.querySelector('.task-fold[open]').textContent)", "Evidence did not open to the checks and the worker's evidence");
await evaluate("document.querySelector('.story .link-action').click()");
await until("/Decided: fake choice \\(fixture\\)\\./.test(document.querySelector('.story').textContent)", "More did not show the decisions");
await shot("task-sheet-open");
await evaluate("document.querySelector('.story .link-action').click()");
await evaluate("[...document.querySelectorAll('.task-next button')].find((node) => node.textContent === 'Land').click()");
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '') === 'done'", "#2 did not land", 300);
told = await story();
if (told.next !== "On main; the last deploy does not have it yet.") await fail(`the top of a done task reads ${JSON.stringify(told)}`);
await until("[...document.querySelectorAll('.task-fold > .fold-head')].some((node) => node.textContent === 'To check · 1 open')", "#2's item is not open under To check", 60);
await evaluate("[...document.querySelectorAll('.task-fold > .fold-head')].find((node) => node.textContent.startsWith('To check')).click()");
const offers = await evaluate("[...document.querySelectorAll('.task-fold[open] .check-item .check-act')].map((node) => node.title).join()");
if (offers !== "Try it on a preview,Fail,Pass") await fail(`#2's item offers ${offers}`);
await evaluate("document.querySelector('.timeline > .fold-head').click()");
const history = await text(".timeline-what");
if ((await text(".timeline > .fold-head")).join() !== "History" || !history.includes("Landed") || history.some((line) => /Verified|without verification/.test(line))) await fail(`the history reads ${history}`);
await shot("task-landed");

// --- #4: a pending approval in the sentence and its buttons. Deny, then Allow for project.
await go(page(4));
await until("!!document.querySelector('.task-next button[aria-label=\"Approve discard #4\"]')", "#4 shows no pending approval");
// ^ The story says what an action is for; its row holds the buttons alone.
if ((await text(".task-next p")).length) await fail(`an action row repeats the story: ${await text(".task-next p")}`);
told = await story();
if (told.next !== "The manager asks: Approve discard #4." || await sheetState() !== "needs you") await fail(`the top of a blocked task with an approval reads ${JSON.stringify(told)}`);
// ^ No Shared files outside a conflict.
if ((await openSections()).includes("Shared files")) await fail("a task that is not in conflict shows Shared files");
await shot("task-blocked", ".composer-send");
await evaluate("[...document.querySelectorAll('.task-next button')].find((node) => node.textContent === 'Deny').click()");
await until("!document.querySelector('.task-next button[aria-label=\"Approve discard #4\"]')", "the denied approval stayed", 300);
told = await story();
if (told.next !== "The worker waits for you to let it reach crates.io, or to answer.") await fail(`the story of a blocked task reads ${JSON.stringify(told)}`);
await evaluate("[...document.querySelectorAll('.task-next button')].find((node) => node.textContent === 'Allow for project').click()");
await until("document.querySelector('.story-next')?.textContent === 'Land it to put it on main.'", "#4 did not resume after Allow", 300);

// --- Notices lie over the screen, one at a time. An approved action that did not run stays
// until dismissed, also when a later outcome comes; an outcome clears itself.
await go(chat);
await until("!!document.querySelector('.reservoir-head')", "the chat did not come back for the notices");
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
// A later outcome (a message to a task) does not hide the failure; it waits past the outcome's time.
await go(page(4));
await until("!!document.querySelector('.task-message .composer-field')", "#4 offers no message field");
await typeInto(".task-message .composer-field", "NOOP and a note");
await click(".task-message .composer-send");
await pause(5000);
if (!(await evaluate("/Land #6 did not run/.test(document.querySelector('.notice')?.textContent ?? '')"))) await fail("a later outcome or the time hid the failure");
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close #4");
await evaluate("document.querySelector('.notice[data-tone=danger] button').click()");
await until("!document.querySelector('.notice')", "Dismiss did not clear the failure");
// An outcome alone clears itself.
await go(page(4));
await until("!!document.querySelector('.task-message .composer-field')", "#4 offers no message field again");
await pause(3000);
await typeInto(".task-message .composer-field", "NOOP once more");
await click(".task-message .composer-send");
await until("[...document.querySelectorAll('.notice')].some((node) => /^#4: /.test(node.textContent))", "a message to #4 showed no outcome");
await until("!document.querySelector('.notice')", "the outcome did not clear itself", 80);
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close #4 again");

// --- The manager chat attaches files as a session's chat does, and shows another project's
// manager as its own bubbles (docs/web-client-tasks.md "Project screen").
await go(chat);
await until("!!document.querySelector('.composer .icon-button[aria-label=\"Attach files\"]')", "the manager chat has no attach button");
const peers = await bubbles();
if (!peers.some(({ author }) => author === "Manager plans") || !peers.some(({ tag }) => tag === "Reply to Manager plans")) await fail(`the managers' messages read ${JSON.stringify(peers)}`);
await pause(6000);
await evaluate(`(() => { const input = document.querySelector('.composer input[type=file]'); const files = new DataTransfer();
  files.items.add(new File(['notes for the manager'], 'notes.txt', { type: 'text/plain' })); input.files = files.files;
  input.dispatchEvent(new Event('change', { bubbles: true })); })()`);
await until("document.querySelectorAll('.composer .chips li').length === 1 && !document.querySelector('.composer .chips [data-uploading]')", "the file did not upload in the manager chat", 100);
// A pasted screenshot attaches the same way; a text paste is left to the browser.
if (!(await pasteInto(".composer-field", "pasted words"))) await fail("a text paste was cancelled in the manager chat");
if (await pasteInto(".composer-field", "image")) await fail("a pasted image was left to the manager's text box");
await until("document.querySelectorAll('.composer .chips li').length === 2 && !document.querySelector('.composer .chips [data-uploading]')", "the pasted image did not upload in the manager chat", 100);
await shot("manager-attach", ".composer-send");
await typeInto(".composer-field", "Here are the notes");
await click(".composer-send");
const sentWithFile = "[...document.querySelectorAll('.log .bubble[data-side=out]')].some((node) => /Here are the notes/.test(node.textContent) && [...node.querySelectorAll('.attachments a')].some((link) => link.textContent.startsWith('notes.txt')) && [...node.querySelectorAll('.attachments img')].some((image) => /^pasted-\\d{14}\\.(png|jpg)$/.test(image.alt)))";
await until(`${sentWithFile} || !!document.querySelector('.composer .error')`, "the sent message shows no attachment", 100);
if (!(await evaluate(sentWithFile))) await fail(`the message with a file was not sent: ${await evaluate("document.querySelector('.composer .error')?.textContent ?? ''")}; bubbles ${JSON.stringify(await bubbles())}`);
if (await count(".composer .chips li")) await fail("the sent file stayed in the composer");

// --- To check: the reservoir's row opens the list; Fail needs a note and starts a task that
// fixes it; Pass; All passed empties the list.
// ^ The actions above spent this device's burst (10, then one per 3 s); this part sends four.
await pause(12000);
await go(chat);
await until("!!document.querySelector('.reservoir-head')", "the chat did not come back for To check");
await openReservoir();
await until(`!!${rowOf("3 to check")}`, "the reservoir has no To check row", 60);
await evaluate(`${rowOf("3 to check")}.querySelector('.row-open').click()`);
await until(`location.hash === '#/p/${project}/checks' && document.querySelectorAll('dialog.task-sheet[open] .check-item').length === 3`, "the To check list did not open");
const listed = await evaluate("[...document.querySelectorAll('.check-item')].map((node) => [node.querySelector('.task-chip').textContent.trim(), node.querySelector('.check-what').textContent, node.querySelector('.check-pass').textContent])");
if (JSON.stringify(listed) !== JSON.stringify([["#7", "open the file", "Passes if it has text"], ["#8", "open the file", "Passes if it has text"], ["#2", "open the file", "Passes if it has text"]])) await fail(`the To check list reads ${JSON.stringify(listed)}`);
if (await sheetState() !== "3 open") await fail(`the list's top row reads ${await sheetState()}`);
// ^ Each item says what its task did; #8's says where to look and shows its capture. Three short
// items fit: the sheet is as high as its rows, on the bottom, with no empty band under them.
const context = await evaluate("[...document.querySelectorAll('.check-item')].map((node) => [node.querySelector('.check-task')?.textContent ?? '', [...node.querySelectorAll('.check-pass')].map((line) => line.textContent).join('|')])");
if (!context.every(([did]) => did.startsWith("did: WORK")) || context[1][1] !== "Passes if it has text|Look at /#/x") await fail(`the items' context reads ${JSON.stringify(context)}`);
await until("document.querySelector('.check-capture img')?.naturalWidth === 1", "#8's capture did not load");
if (await count(".check-capture") !== 1) await fail("an item without a capture shows one");
await until("document.querySelector('dialog.task-sheet')?.getAnimations().length === 0", "the sheet kept moving");
const fitted = await evaluate("(() => { const sheet = document.querySelector('dialog.task-sheet').getBoundingClientRect(); const last = [...document.querySelectorAll('.check-item')].pop().getBoundingClientRect(); return [Math.round(innerHeight - sheet.bottom), Math.round(sheet.bottom - last.bottom), Math.round(sheet.height / innerHeight * 100)]; })()");
if (Math.abs(fitted[0]) > 1 || fitted[1] > 48 || fitted[2] >= 85) await fail(`the To check sheet does not fit its rows: ${JSON.stringify(fitted)}`);
// ^ On a phone the item words use the list's width: Try, Fail and Pass share the chip's row, All
// passed is in the top bar and no intro sentence takes a row.
const room = await evaluate(`(() => { const item = document.querySelector('.check-item'); const chip = item.querySelector('.task-chip').getBoundingClientRect();
  const pass = item.querySelector('.check-act[title="Pass"]').getBoundingClientRect(); const list = document.querySelector('.check-list').getBoundingClientRect();
  return { row: pass.top >= chip.top - 12 && pass.top <= chip.bottom, share: Math.round(item.querySelector('.check-what').getBoundingClientRect().width / list.width * 100),
    all: !!document.querySelector('dialog.task-sheet header.bar .check-all'), intro: !!document.querySelector('.check-intro') }; })()`);
if (!room.row || room.share < 85 || !room.all || room.intro) await fail(`the To check items waste room on a phone: ${JSON.stringify(room)}`);
await shot("to-check", ".composer-send");
const firstItem = "document.querySelector('.check-item')";
// ^ Try starts the preview of the item's task and becomes a link to it (docs/tasks.md "Preview").
await evaluate(`${firstItem}.querySelector('.check-act[title="Try it on a preview"]').click()`);
await until(`!!${firstItem}.querySelector('a.check-act[href^="https://host.example.ts.net:"]')`, "Try did not become a preview link", 300);
await shot("to-check-try", ".composer-send");
await evaluate(`${firstItem}.querySelector('.check-act[title="Fail"]').click()`);
await until(`!!${firstItem}.querySelector('.need-form input')`, "Fail did not ask for a note");
await evaluate(`${firstItem}.querySelector('.need-form').requestSubmit()`);
await pause(300);
if (await count(".check-item") !== 3) await fail("a fail without a note was sent");
await typeInto(".check-item .need-form input", "the text is cut off");
await shot("to-check-fail", ".composer-send");
await evaluate(`${firstItem}.querySelector('.need-form').requestSubmit()`);
await until("document.querySelectorAll('.check-item').length === 2", "the failed item stayed in the list", 60);
await evaluate(`${firstItem}.querySelector('.check-act[title="Pass"]').click()`);
await until("document.querySelectorAll('.check-item').length === 1", "the passed item stayed in the list", 60);
await evaluate("document.querySelector('.check-all').click()");
await until("(document.querySelector('dialog.task-sheet .empty')?.textContent ?? '') === 'Nothing to check.'", "All passed did not empty the list", 60);
await shot("to-check-done");
await key("Escape", 27);
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Escape did not close the To check list");
await until("[...document.querySelectorAll('.status-line')].some((node) => /#7 failed a check; #\\d+ fixes it/.test(node.textContent))", "the chat does not say which task fixes the failed check", 60);
await until("!/to check/.test(document.querySelector('.reservoir-words')?.textContent ?? '')", "the reservoir still counts items to check", 60);

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

// --- A task that waits for the owner's answer: in plans' chat its manager reads "Manager" and
// ui's "Atlas". #1's reservoir row with Answer, whose chip opens its sheet: the story asks for
// the answer, the field says so, and the answer is a bubble; then the list cleared one row
// after another: Land #2, Land #1, and then no reservoir with the focus on the composer.
const plans = Number(plansArg);
// ^ The actions above spent this device's burst (10, then one per 3 s); this part sends three.
await pause(12000);
await go(`#/p/${plans}`);
await until("!!document.querySelector('.reservoir-head')", "plans has no reservoir");
await until("document.querySelectorAll('.log .bubble').length >= 2", "plans' chat shows no managers' bubbles", 30);
const plansAuthors = await bubbles();
if (!plansAuthors.some(({ author, tag }) => author === "Manager" && tag === "To Atlas") || !plansAuthors.some(({ author }) => author === "Atlas")) await fail(`plans' managers read ${JSON.stringify(plansAuthors)}`);
await openReservoir();
await until(`!!${rowOf("#1")}`, "the waiting task has no reservoir row");
const waitRow = (await rows()).find((row) => row[0] === "#1");
if (waitRow[2] !== "needs an answer" || waitRow[3] !== "Answer") await fail(`the waiting row reads ${JSON.stringify(waitRow)}`);
await evaluate(`${rowOf("#1")}.querySelector('.task-chip').click()`);
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the waiting task's sheet did not open");
await lean("the waiting sheet");
told = await story();
if (told.next !== "The worker waits for your answer.") await fail(`the top of a waiting task reads ${JSON.stringify(told)}`);
if (await evaluate("document.querySelector('.task-message .composer-field').placeholder") !== "Answer the worker") await fail("the sheet's field does not ask for the answer");
await shot("waiting-sheet", ".composer-send");
await typeInto(".task-message .composer-field", "WORK keys.txt");
await click(".task-message .composer-send");
await until("document.querySelectorAll('.task-talk .bubble').length === 1", "the answer is not a bubble", 60);
if (await evaluate("document.querySelector('.task-talk .bubble').dataset.side") !== "out") await fail("the answer is not the owner's bubble");
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '') === 'needs you' && [...document.querySelectorAll('.task-sheet .task-next button')].some((node) => node.textContent === 'Land')", "#1 did not get ready after the answer", 120);
await key("Escape", 27);
await until(`location.hash === '#/p/${plans}' && !document.querySelector('dialog.task-sheet')`, "Escape did not close the waiting sheet");
// ^ Closing the sheet gives the composer the focus after the next paint: wait for it, or it
// takes the focus back from the row button pressed below.
await until("document.activeElement?.classList.contains('composer-field') && !document.activeElement.closest('dialog')", "closing the sheet did not focus the composer");
await openReservoir();
await until(`(${rowOf("#1")}?.querySelector('.need-action')?.title ?? '') === 'Land'`, "#1's row does not offer Land", 60);
moved = await clearRow("#2", "Land");
moved = await clearRow("#1", "Land");
await until("!document.querySelector('.reservoir')", "the cleared list still shows the reservoir", 60);
await until("document.activeElement?.matches('.chat .composer-field')", `with the list cleared the focus is on ${JSON.stringify(await focused())}`, 30);
await shot("reservoir-cleared", ".composer-send");

// --- The task sheet has no link to the worker's terminal, though #6's worker has a session;
// the project's ⋯ keeps Sessions.
await go(page(6));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "#6's sheet did not open");
if (await count('.task-sheet a[href*="/terminal"]')) await fail("the task sheet links to the worker's terminal");
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close #6's sheet");
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
await until("!!document.querySelector('.reservoir-head')", "the ui chat did not come back");
await resize([1280, 800, 1]);
await managerWidth("the wide chat");
await shot("project", ".composer-send");
await followEnd("1280 px");
await go(page(3));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the sheet did not open on a wide screen");
if ((await evaluate("getComputedStyle(document.querySelector('dialog.task-sheet')).animationName")) !== "sheet-in-side") await fail("the wide sheet does not come in from the right");
await until("document.querySelector('dialog.task-sheet')?.getAnimations().length === 0", "the sheet kept moving");
const panel = await evaluate("(() => { const r = document.querySelector('dialog.task-sheet').getBoundingClientRect(); return { left: Math.round(r.left), right: Math.round(r.right), height: Math.round(r.height) }; })()");
if (panel.right !== 1280 || panel.left < 640 || panel.height !== 800) await fail(`on a wide screen the sheet is ${JSON.stringify(panel)}`);
await shot("sheet");
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close the wide sheet");
await go(`#/p/${project}/page`);
await until("!!document.querySelector('.project-page .diagram img')", "the wide page did not draw its graph", 30);
await shot("page");
await key("Escape", 27);
await resize([390, 844, 2]);

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

const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);
fs.writeFileSync(path.join(captures, "tasks-audits.json"), JSON.stringify(audits, null, 1));
console.log(`tasks page: home with the Sessions tab in the first screen and no Projects tab, the plain worktree on its project's worktree list and no Adopted list, since-you-left, bubbles (sides, authors, a whole worker report, grouping), no cards/tray/strip/Explain, one working color on home, the session rows (only working marks pulse, none under reduced motion), the reservoir and a typing manager, the reservoir (the closed line naming running #6 without \"Owner asked:\", rows with one action, Running, To check, no facts, Allow, Approve and Land cleared in place with the focus on the next row and the chat still), the ⋯ menu, the project page (facts, markdown, mermaid, its edit by the manager, the reply's link), typing while a turn runs, short chips, a table inside its bubble, the task sheet (a row and a chip open it without a chat scroll, 85%, Escape, Back, swipe, draft kept, three stages, one sentence with More, folds with their notes, no steps, verdict, Review or revision, Land, the item live after landing, History, approval in the sentence, Deny, Allow for project, Shared files only in a conflict), done bubbles out of the reservoir, To check (its row, Fail with and without a note, Pass, All passed, the fix task's line), the manager chat's attach and the other manager's bubbles, Try on a To check item, the # picker, the deep link (one read of the tasks), a waiting task (reservoir row with Answer, its story and field, the answer as a bubble, Land and Land cleared until the reservoir goes and the focus is on the composer), the wide panel and page, the new-version pill and its reload with the route and the draft; ${audits.length} captures`);
await cleanup();
process.exit(0);
