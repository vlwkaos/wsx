#!/usr/bin/env node
// Real-browser check of manager mode on the page (docs/web-client.md "Tasks"), driven over the
// Chrome DevTools Protocol. Called by `test-web-client.py --part tasks --browser`.
//   web-tasks-check.mjs <site-url> <code> <project-id> <capture-dir> <home order> <slop-audit.js|""> <plans-id> <restart-file>
// <home order> is the project names as home must list them, comma-separated. <plans-id> is a
// project whose #1 waits as briefed (rewritten prompt WORK keys.txt), for the brief journey.
// The project has: #1 and #4 blocked on a host, #2 ready with Human Verify, #3 ready to land,
// #5 closed after a long report the manager heard of, #6 working (its prompt opens with
// "Owner asked:"), and two pending approvals: a1 lands #3, a2 discards #4 (shown as
// "Approve land #3", "Approve discard #4"). Writing <restart-file> asks the test to restart
// the server with another site name, so it serves another build; it writes <restart-file>.done.
// Each capture is saved with the slop audit and, on a phone, the mobile audit of
// /ui-view-standards beside it (WSX_MOBILE_AUDIT names the mobile audit).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { launch } from "./web-cdp.mjs";

const [siteUrl, code, projectArg, captures, homeOrder, auditPath, plansArg, restartFile] = process.argv.slice(2);
const project = Number(projectArg);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({
  captures, prefix: "tasks-", timeoutSeconds: 300,
});
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const auditSource = auditPath ? fs.readFileSync(auditPath, "utf8") : null;
const mobilePath = process.env.WSX_MOBILE_AUDIT ?? path.join(os.homedir(), ".claude/skills/ui-view-standards/scripts/mobile-audit.js");
const mobileSource = fs.existsSync(mobilePath) ? fs.readFileSync(mobilePath, "utf8") : null;
if (!mobileSource) await fail(`no mobile audit at ${mobilePath}; set WSX_MOBILE_AUDIT`);
let size = [390, 844, 2];
const phone = () => size[0] < 600;
const audits = [];

async function resize(next) {
  size = next;
  await viewport(...next);
  await pause(250);
}
const go = (hash) => evaluate(`location.hash = ${JSON.stringify(hash)}`);
const text = (selector) => evaluate(`[...document.querySelectorAll(${JSON.stringify(selector)})].map((node) => node.textContent.trim().replace(/\\s+/g, ' '))`);
const count = (selector) => evaluate(`document.querySelectorAll(${JSON.stringify(selector)}).length`);
const click = (selector) => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
// Type into the focused-to-be field as an input event does, the caret at the end.
const typeInto = (selector, value) => evaluate(`(() => { const field = document.querySelector(${JSON.stringify(selector)}); field.focus();
  field.value = ${JSON.stringify(value)}; field.setSelectionRange(field.value.length, field.value.length);
  field.dispatchEvent(new Event('input', { bubbles: true })); field.dispatchEvent(new KeyboardEvent('keyup', { bubbles: true })); })()`);
async function key(name, keyCode) {
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: name, code: name, windowsVirtualKeyCode: keyCode });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: name, code: name, windowsVirtualKeyCode: keyCode });
}
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
  const seen = await evaluate(`(() => {
    const free = (node) => node.closest('.code-block pre, .table-wrap, .diagram, .timeline-output');
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

async function pair() {
  await send("Page.navigate", { url: siteUrl });
  await until("!!document.querySelector('#pair-code')", "pairing form did not render");
  await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)}; input.form.requestSubmit(); })()`);
  await until("!!document.querySelector('.app')", "the app did not open after pairing");
}

// The task story as words: the steps shown (with a stop word), the current one, and the sentence.
const story = () => evaluate(`(() => { const words = (li) => li.querySelector('.story-name').textContent + (li.querySelector('.story-word') ? ' ' + li.querySelector('.story-word').textContent : '');
  const current = document.querySelector('.story-steps [aria-current=step]');
  return { shown: [...document.querySelectorAll('.story-steps li')].map(words).join(' → '), current: current && words(current),
    next: document.querySelector('.story-next')?.textContent.trim(), asked: !!document.querySelector('.story-asked') }; })()`);
// The reservoir's rows: [chip, title, context, actions], in the order shown.
const rows = () => evaluate(`[...document.querySelectorAll('.reservoir-list .need')].map((row) => [
  row.querySelector('.task-chip')?.textContent.trim() ?? '', row.querySelector('.row-label').textContent, row.querySelector('.row-context')?.textContent ?? '',
  [...row.querySelectorAll('.need-action')].map((node) => node.textContent.trim()).join('+')])`);
const rowOf = (label) => `[...document.querySelectorAll('.reservoir-list .need')].find((row) => ((row.querySelector('.task-chip')?.textContent.trim() ?? '') + ' ' + row.querySelector('.row-label').textContent).trim().startsWith(${JSON.stringify(label)}))`;
const reservoirWords = () => evaluate("document.querySelector('.reservoir-words')?.textContent ?? ''");
async function openReservoir() {
  if (!(await evaluate("!!document.querySelector('.reservoir[data-open]')"))) await click(".reservoir-head");
  await until("!!document.querySelector('.reservoir-list, .facts-line')", "the reservoir did not open");
}
async function closeReservoir() {
  if (await evaluate("!!document.querySelector('.reservoir[data-open]')")) await click(".reservoir-head");
}
const sheetState = () => evaluate("document.querySelector('.task-sheet .bar-state')?.textContent ?? ''");
const atEnd = "(() => { const log = document.querySelector('.log'); return log.scrollHeight - log.scrollTop - log.clientHeight < 2; })()";
const bubbles = () => evaluate(`[...document.querySelectorAll('.log .bubble')].map((node) => ({ side: node.dataset.side, first: node.hasAttribute('data-first'),
  last: node.hasAttribute('data-last'), author: node.querySelector('.bubble-author')?.textContent ?? '', text: node.querySelector('.bubble-box').textContent.trim().replace(/\\s+/g, ' ') }))`);

await resize([390, 844, 2]);
await pair();
const chat = `#/p/${project}`;
const page = (n) => `#/p/${project}/t/${n}`;

// --- Home: one row per project, needs first, then by activity; the bottom bar is Home and Settings.
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
// ^ #1 waits as briefed; #2, built by the API checks, is ready to land.
if (plansRow?.status !== "Brief ready #1" || plansRow.count !== "2") await fail(`the plans row reads ${JSON.stringify(plansRow)}`);
// Projects with agent sessions and no task: the sessions in words, and the time of the newest.
const sessionRows = { busy: "2 working", quiet: "claude in main done just now", gone: "claude in main exited" };
for (const [name, status] of Object.entries(sessionRows)) {
  const row = homeRows.find((item) => item.name === name);
  if (row?.status !== status || row.count || !/ago|now/.test(row.state)) await fail(`the ${name} row reads ${JSON.stringify(row)}, not "${status}" with a time`);
}
if ((await text(".tabbar a")).join() !== "Home,Settings") await fail(`the bottom bar is ${await text(".tabbar a")}`);
// ^ The server runs this page's build: no new-version pill.
if (await count(".build-pill")) await fail("the new-version pill shows on the same build");
await shot("home");
// "All sessions": the session inbox, without task workers and the manager.
await evaluate("[...document.querySelectorAll('.home-more a')].find((node) => node.textContent === 'All sessions').click()");
await until("location.hash === '#/sessions' && /All sessions/.test(document.querySelector('.bar-label')?.textContent ?? '')", "All sessions did not open");
const listedSessions = await text(".rows .row-context");
if (listedSessions.some((line) => /\btask-(\d+|manager)\b|\b(task|manager)$/.test(line))) await fail(`the session list shows task sessions: ${JSON.stringify(listedSessions)}`);
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until("location.hash === '#/' && !!document.querySelector('.home-row')", "Back did not return home");
await go("#/settings");
await until("[...document.querySelectorAll('.settings a')].some((node) => node.textContent === 'All sessions')", "Settings has no All sessions link");
// A row opens the project's chat.
await go("#/");
await until("!!document.querySelector('.home-row')", "home did not come back");
await evaluate(`[...document.querySelectorAll('.home-row')].find((row) => row.querySelector('.row-label').textContent === 'ui').click()`);
await until(`location.hash === ${JSON.stringify(chat)} && !!document.querySelector('.composer-field') && !!document.querySelector('.reservoir-head') && !!document.querySelector('.log .bubble')`, "a home row did not open the chat");
if (await count(".tabbar")) await fail("the chat shows the bottom bar");

// "Since you left": this device left the project two hours ago; the line names what happened.
await go("#/");
await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 }))`);
await go(chat);
await until("/Since you left: .*#1 #2 #3 #4.* started.* repl(y|ies)/.test(document.querySelector('.since')?.textContent ?? '')", "no since-you-left line");
await evaluate("[...document.querySelectorAll('.since button')].find((node) => node.textContent === 'Dismiss').click()");
await until("!document.querySelector('.since')", "Dismiss did not close the line");

// --- The chat reads like a messaging app: no cards, no tray, no strip, no Explain; the
// header names the manager only when it matters.
for (const gone of [".need-card", ".tray", ".brief", ".story-explain", ".events"]) {
  if (await count(gone)) await fail(`the chat still has ${gone}`);
}
if (await evaluate("/Explain/.test(document.querySelector('#app').innerText)")) await fail("the chat still offers Explain");
if (await evaluate("document.querySelector('.manager .bar-state')?.textContent ?? ''")) await fail(`an idle manager is named in the header: ${await evaluate("document.querySelector('.manager .bar-state').textContent")}`);
// The owner's message on the right, the manager's reply on the left with its author, and #5's
// long report as one bubble by "#5 worker" with its header, folded until "Show all".
let shown = await bubbles();
const ownerAsk = shown.find((bubble) => bubble.side === "out" && bubble.text.startsWith("RUN wsx task land 3"));
const managerReply = shown.find((bubble) => bubble.side === "in" && bubble.author === "Manager");
if (!ownerAsk || ownerAsk.author || !managerReply) await fail(`the bubbles are ${JSON.stringify(shown)}`);
const report = `[...document.querySelectorAll('.log .bubble')].find((node) => node.querySelector('.bubble-author')?.textContent === '#5 worker')`;
await until(`!!${report}`, "the long report has no worker bubble");
const head = await evaluate(`${report}.querySelector('.bubble-head').textContent.trim().replace(/\\s+/g, ' ')`);
if (head !== "#5 · done") await fail(`the report's header reads ${head}`);
if (!(await evaluate(`${report}.querySelector('.bubble-body').hasAttribute('data-clamped')`))) await fail("the long report is not folded");
await shot("project", ".composer-send");
await evaluate(`${report}.querySelector('.bubble-more').click()`);
await until(`${report}.querySelector('.bubble-body').hasAttribute('data-open') && /line 30 of the report/.test(${report}.textContent)`, "Show all did not open the report");
await evaluate(`${report}.querySelector('.bubble-more').click()`);
// The chat opens at its newest message; scrolled up, it shows Jump to latest.
await until(atEnd, "the manager chat did not open at its end");
await evaluate("document.querySelector('.log').scrollTop = 0");
await until("!!document.querySelector('.jump')", "no jump button after scrolling the manager chat up");
await click(".jump");
await until(`!document.querySelector('.jump') && ${atEnd}`, "Jump to latest did not go to the end of the manager chat");

// --- The reservoir: one line over the composer; a tap opens one row per item with its one
// action; the running task's chip pulses; its title has no "Owner asked:"; the deploy and the
// main checkout under the list. Opening it keeps the chat at its end.
if (await reservoirWords() !== "6 need you · 1 working") await fail(`the reservoir reads ${await reservoirWords()}`);
if (await count(".reservoir-list")) await fail("the reservoir is open before a tap");
await openReservoir();
await pause(300);
if (!(await evaluate(atEnd))) await fail(`opening the reservoir took the chat off its end: ${JSON.stringify(await evaluate("(() => { const log = document.querySelector('.log'); return { top: log.scrollTop, height: log.scrollHeight, client: log.clientHeight, jump: !!document.querySelector('.jump') }; })()"))}`);
const listedRows = await rows();
const rowWords = listedRows.map((row) => `${row[0]} ${row[3]}`);
const expectedRows = ["#1 Allow", "#4 Allow", " Deny+Approve", " Deny+Approve", "#2 Verify", "#3 Land", "#6 "];
if (rowWords.join("|") !== expectedRows.join("|")) await fail(`the reservoir rows are ${JSON.stringify(listedRows)}`);
if (listedRows[0][2] !== "needs to reach crates.io" || listedRows[2][1] !== "Approve land #3" || listedRows[3][1] !== "Approve discard #4") await fail(`the reservoir rows read ${JSON.stringify(listedRows)}`);
if (!/^keep a worker busy WAIT /.test(listedRows[6][1])) await fail(`the running task's title is ${listedRows[6][1]}`);
if (!(await evaluate(`${rowOf("#6")}.querySelector('.task-chip .mark[data-running]')`))) await fail("the running task's chip does not pulse");
const chipName = await evaluate(`${rowOf("#1")}.querySelector('.task-chip').getAttribute('aria-label')`);
if (chipName !== "#1, blocked") await fail(`a chip's accessible name is ${chipName}`);
const facts = (await text(".facts-line"))[0] ?? "";
if (!/Deploy: [0-9a-f]{8}, up to date with main\./.test(facts) || !/Main checkout on main, /.test(facts)) await fail(`the reservoir's facts read ${facts}`);
await shot("reservoir-open", ".composer-send");
await resize([390, 420, 2]);
await shot("reservoir-short", ".composer-send");
await resize([390, 844, 2]);
// Act in place: Allow #1 grants its host and resumes it; Approve land #3 lands it. The chat
// stays at its end, and the settled items become muted lines that join when close together.
await evaluate(`${rowOf("#1")}.querySelector('.need-action').click()`);
await until("[...document.querySelectorAll('.notice')].some((node) => /#1:/.test(node.textContent))", "Allow in the reservoir showed no outcome");
// ^ #1 works for a moment, then is ready to land: either way it no longer waits for the host.
await until(`(${rowOf("#1")}?.querySelector('.row-context')?.textContent ?? '') !== 'needs to reach crates.io'`, "#1 still waits for its host after Allow", 60);
await evaluate(`${rowOf("Approve land #3")}.querySelectorAll('.need-action')[1].click()`);
await until("[...document.querySelectorAll('.notice')].some((node) => /Approved land #3/.test(node.textContent))", "Approve showed no outcome");
await until(`!${rowOf("Approve land #3")} && !${rowOf("#3")}`, "the reservoir kept the approved landing", 300);
await until("[...document.querySelectorAll('.status-line')].some((node) => /Approved land #3 · .*#3 landed|#3 landed · .*Approved land #3/.test(node.textContent))", "the approval and the landing are not one line", 300);
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
await shot("page");
await key("Escape", 27);
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Escape did not close the page");

// --- A turn of the manager: two messages group on the right; while it runs a typing bubble
// ends the chat and the header says "typing"; the reply edits the page (with a link to it),
// has short chips (the same task twice as two plain chips) and a table that scrolls inside it.
await typeInto(".composer-field", "SLOW TABLE");
await click(".composer-send");
// ^ A send in flight ignores another tap: the field clears when the server has the message.
await until("document.querySelector('.composer-field').value === ''", "the first message was not sent");
await typeInto(".composer-field", "PAGE Goal Ship the chat");
await click(".composer-send");
await until("document.querySelector('.composer-field').value === ''", "the second message was not sent");
await until("!!document.querySelector('.log .typing') && document.querySelector('.manager .bar-state')?.textContent === 'typing'", "no typing bubble while the manager's turn runs", 30);
const typing = await evaluate("({ author: document.querySelector('.log .typing').closest('.bubble').querySelector('.bubble-author').textContent, said: document.querySelector('.log .typing .visually-hidden').textContent })");
if (typing.author !== "Manager" || typing.said !== "Manager is typing") await fail(`the typing bubble reads ${JSON.stringify(typing)}`);
if (await evaluate("document.querySelector('.log .bubble:last-child .typing') === null")) await fail("the typing bubble is not the last message");
await shot("typing", ".composer-send");
const sentPair = (await bubbles()).filter((bubble) => bubble.side === "out").slice(-2);
if (sentPair.length !== 2 || sentPair[0].last || sentPair[1].first || !sentPair[1].last) await fail(`two owner messages did not group: ${JSON.stringify(sentPair)}`);
await until("[...document.querySelectorAll('.log .bubble[data-side=in]')].some((node) => !!node.querySelector('.table-wrap'))", "the reply with a table did not arrive", 60);
const tableReply = "[...document.querySelectorAll('.log .bubble[data-side=in]')].find((node) => !!node.querySelector('.table-wrap'))";
const inside = await evaluate(`(() => { const bubble = ${tableReply}.querySelector('.bubble-box').getBoundingClientRect(), table = ${tableReply}.querySelector('.table-wrap');
  const box = table.getBoundingClientRect(); return { right: box.right <= bubble.right + 0.5, scrolls: table.scrollWidth > table.clientWidth }; })()`);
if (!inside.right || !inside.scrolls) await fail(`the table does not stay inside its bubble: ${JSON.stringify(inside)}`);
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

// --- The task sheet: a chip opens it over the chat; Escape, Back and a swipe down close it, the
// composer keeps its draft and gets the focus back.
await typeInto(".composer-field", "a draft that stays");
await evaluate("[...document.querySelectorAll('.log .task-chip')].find((chip) => chip.textContent.trim() === '#2').click()");
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('dialog.task-sheet[open] .story')`, "a chip did not open the task sheet");
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
await typeInto(".composer-field", "land #hv");
await until("document.querySelectorAll('.slash-option').length === 1", "the picker does not filter by words");
await key("Enter", 13);
await until("document.querySelector('.composer-field').value === 'land #2 '", "Enter did not insert the task");
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

// --- The task sheet of #2: two steps of progress, one sentence, the report under plain
// headings, Fail needs a note, Pass, then Land. No Explain, no "Asked:".
await go(page(2));
await until("document.querySelectorAll('.task-sheet .task-section').length >= 7", "the task sheet did not show the report");
let told = await story();
if (told.asked || told.shown !== "Checks → Ready" || told.current !== "Ready" || told.next !== "Check the 1 item below, then Pass to allow landing.") await fail(`the story of a task to verify reads ${JSON.stringify(told)}`);
const headings = await text(".task-section > .task-heading");
if (headings.slice(0, 7).join() !== "Asked,Summary,Decisions,Changes,Evidence,Human Verify,Left open") await fail(`the report sections are ${headings}`);
if (await count(".task-sheet .label, .task-sheet .story-explain")) await fail("the sheet still has uppercase labels or Explain");
if ((await text(".verify button")).join() !== "Fail,Pass r1") await fail(`the verify buttons are ${await text(".verify button")}`);
await shot("task-verify");
await evaluate("[...document.querySelectorAll('.verify button')].find((node) => node.textContent === 'Fail').click()");
await until("!!document.querySelector('.verify-note textarea')", "Fail did not ask for a note");
await evaluate("document.querySelector('.verify-note').requestSubmit()");
await pause(300);
if (!(await count(".verify-note"))) await fail("a fail without a note was sent");
await typeInto(".verify-note textarea", "HV NOOP the file has no text");
await evaluate("document.querySelector('.verify-note').requestSubmit()");
await until("[...document.querySelectorAll('.notice')].some((node) => /#2:/.test(node.textContent))", "the fail showed no outcome");
// The note went to the worker: a new revision r2 with its own Human Verify item, then Pass.
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '').startsWith('ready · r2')", "#2 did not come back as r2", 300);
await until("[...document.querySelectorAll('.verify button')].some((node) => node.textContent === 'Pass r2')", "no Pass r2 on the task page", 300);
await evaluate("[...document.querySelectorAll('.verify button')].find((node) => node.textContent === 'Pass r2').click()");
await until("[...document.querySelectorAll('.task-actions button')].some((node) => node.textContent === 'Land r2')", "no Land r2 on the task page", 300);
await click(".timeline-head");
const timeline = await text(".timeline-what");
if (!timeline.includes("Verified r1: fail") || !timeline.includes("Human verification (fail)") || !timeline.includes("Revision r2")) await fail(`the timeline reads ${timeline}`);
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Land r2').click()");
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '').startsWith('landed')", "#2 did not land", 300);
told = await story();
if (told.shown !== "Verified → Landed" || !/^Landed on main; the last deploy does not have it yet \(the deploy is \d+ commits? behind\)\.$/.test(told.next)) await fail(`the story of a landed task reads ${JSON.stringify(told)}`);
await shot("task-landed");

// --- #4: a pending approval first; a stop reads as the step and its word. Deny, then Allow
// for project.
await go(page(4));
await until("!!document.querySelector('.task-actions button[aria-label=\"Approve discard #4\"]')", "#4 shows no pending approval");
// ^ The story says what an action is for; its row holds the buttons alone.
if ((await text(".task-actions p")).length) await fail(`an action row repeats the story: ${await text(".task-actions p")}`);
told = await story();
if (told.shown !== "Asked → Working blocked" || told.next !== "The manager asks: Approve discard #4. Approve or Deny it above.") await fail(`the story of a blocked task with an approval reads ${JSON.stringify(told)}`);
await shot("task-blocked", ".composer-send");
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Deny').click()");
await until("!document.querySelector('.task-actions button[aria-label=\"Approve discard #4\"]')", "the denied approval stayed", 300);
told = await story();
if (told.next !== "The worker waits for you to let it reach crates.io, or to answer.") await fail(`the story of a blocked task reads ${JSON.stringify(told)}`);
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Allow for project').click()");
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '').startsWith('ready')", "#4 did not resume after Allow", 300);

// --- A deep link to a task opens the chat with the sheet over it; Back closes the sheet, then
// the chat is the first entry, so the next Back leaves the page.
// ^ Through a blank page: a navigation that changes only the hash stays in this document.
await send("Page.navigate", { url: "about:blank" });
await until("location.href === 'about:blank'", "the blank page did not open");
await send("Page.navigate", { url: `${siteUrl}#/p/${project}/t/3` });
await until("!!document.querySelector('dialog.task-sheet[open] .story') && !!document.querySelector('.composer-field')", "a deep link did not open the chat with the sheet");
if (await evaluate("history.state?.depth") !== 1) await fail(`a deep link sits at depth ${await evaluate("JSON.stringify(history.state)")}`);
await evaluate("history.back()");
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Back after a deep link did not close the sheet");

// --- A brief (docs/tasks.md "Brief"): its reservoir row with Start, the sheet with the
// sections and folded concepts, a reply there answered as "Brief v2", then Start.
const plans = Number(plansArg);
const briefSheet = `#/p/${plans}/t/1`;
// ^ The actions above spent this device's burst (10, then one per 3 s); this part sends two.
await pause(9000);
await go(`#/p/${plans}`);
await until("!!document.querySelector('.reservoir-head')", "plans has no reservoir");
await openReservoir();
await until(`!!${rowOf("#1")}`, "the brief has no reservoir row");
const briefRow = (await rows()).find((row) => row[0] === "#1");
if (briefRow[2] !== "brief ready" || briefRow[3] !== "Start") await fail(`the brief's row reads ${JSON.stringify(briefRow)}`);
await shot("brief-row", ".composer-send");
await closeReservoir();
await go(briefSheet);
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the brief sheet did not open");
told = await story();
if (told.shown !== "Asked → Brief" || told.next !== "The brief is ready. Read it, reply to talk it through, or Start the build.") await fail(`the story of a brief reads ${JSON.stringify(told)}`);
const titles = await evaluate("[...document.querySelectorAll('.task-sheet .task-section > .task-heading')].map((node) => node.textContent)");
// ^ The fixture has no open question, so that section is left out.
if (titles.slice(0, 6).join() !== "What a top professional would add,Raw material,Methods and tools,Independent check,Concepts to know,Rewritten prompt") await fail(`the brief sections are ${titles}`);
if (await evaluate("document.querySelector('.task-concept').open")) await fail("a concept is not folded");
if ((await text(".task-actions button")).join() !== "Start the build") await fail(`a brief offers ${await text(".task-actions button")}`);
if (await evaluate("document.querySelector('.task-message .composer-field').placeholder") !== "Reply to the brief") await fail("the sheet's field is not a reply to the brief");
await shot("brief-sheet", ".composer-send");
await typeInto(".task-message .composer-field", "Why a file? REVISE WORK keys2.txt");
await click(".task-message .composer-send");
await until("document.querySelectorAll('.task-talk .bubble').length === 2", "the sheet's reply got no answer", 60);
const talk = await evaluate("[...document.querySelectorAll('.task-talk .bubble')].map((node) => ({ side: node.dataset.side, author: node.querySelector('.bubble-author')?.textContent ?? '', tag: node.querySelector('.bubble-tag')?.textContent ?? '', text: node.querySelector('.bubble-box').textContent }))");
if (talk[0].side !== "out" || talk[1].author !== "#1 worker" || talk[1].tag !== "Brief v2" || !talk[1].text.includes("answered: Why a file?")) await fail(`the talk reads ${JSON.stringify(talk)}`);
if (await count(".task-sheet .label")) await fail("the brief sheet still has a Talk-through label");
await until("[...document.querySelectorAll('.task-actions button')].some((node) => node.textContent === 'Start the build')", "Start did not come back after the answer", 60);
await resize([390, 420, 2]);
await shot("brief-sheet", ".composer-send");
await resize([390, 844, 2]);
await click(".task-sheet .task-actions button");
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '').startsWith('ready')", "Start did not build the brief", 300);
if ((await text(".task-fold > .timeline-head")).join() !== "Brief v2") await fail(`the built task's brief is ${await text(".task-fold > .timeline-head")}`);
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close the brief sheet");

// --- Wide: the chat in its column, the sheet a panel at the right.
await go(chat);
await until("!!document.querySelector('.reservoir-head')", "the ui chat did not come back");
await resize([1280, 800, 1]);
await shot("project", ".composer-send");
await go(page(3));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the sheet did not open on a wide screen");
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
console.log(`tasks page: home and All sessions, since-you-left, bubbles (sides, authors, a folded worker report, grouping), no cards/tray/strip/Explain, the reservoir (count, open, rows with one action, a pulsing working chip, a stripped title, facts, Allow and Approve in place without moving the chat, joined status lines), the ⋯ menu, the project page (markdown, mermaid, its edit by the manager, the reply's link), typing while a turn runs, short chips, a table inside its bubble, the task sheet (85%, Escape, Back, swipe, draft kept, two steps, plain headings, Fail with and without a note, Pass, Land, approval first, Deny, Allow for project), the # picker, the deep link, the brief (reservoir row, sections, talk as bubbles with Brief v2, Start), the wide panel and page, the new-version pill and its reload with the route and the draft; ${audits.length} captures`);
await cleanup();
process.exit(0);
