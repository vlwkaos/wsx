#!/usr/bin/env node
// Real-browser check of manager mode on the page (docs/web-client.md "Tasks"), driven over the
// Chrome DevTools Protocol. Called by `test-web-client.py --part tasks --browser`.
//   web-tasks-check.mjs <site-url> <code> <project-id> <capture-dir> <home order> [slop-audit.js] [plans-id]
// <home order> is the project names as home must list them, comma-separated. <plans-id> is a
// project whose #1 waits as briefed (rewritten prompt WORK keys.txt), for the brief journey.
// The project has: #1 and #4 blocked on a tool, #2 ready with Human Verify, #3 ready to land,
// and two pending approvals: a1 lands #3, a2 discards #4 (shown as "Approve land #3", "Approve discard #4"). Each capture is saved with the slop audit and, on a phone, the
// mobile audit of /ui-view-standards beside it (WSX_MOBILE_AUDIT names the mobile audit).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { launch } from "./web-cdp.mjs";

const [siteUrl, code, projectArg, captures, homeOrder, auditPath, plansArg] = process.argv.slice(2);
const project = Number(projectArg);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({
  captures, prefix: "tasks-", timeoutSeconds: 240,
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
    const scrollers = [...document.querySelectorAll('#app *')].filter((node) => node.tagName !== 'TEXTAREA' && !free(node) && !node.closest('.rail, .tray, .brief, .slash, .task-sheet')
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

// The task story as words: what was asked, the step marked current (with its stop word),
// each step's state, and what happens next.
const story = () => evaluate(`(() => { const words = (li) => li.querySelector('.story-name').textContent + (li.querySelector('.story-word')?.textContent ?? '');
  const current = document.querySelector('.story-steps [aria-current=step]');
  return { asked: document.querySelector('.story-asked')?.textContent.trim(), current: current && words(current),
    steps: [...document.querySelectorAll('.story-steps li')].map((li) => li.dataset.step).join(), next: document.querySelector('.story-next')?.textContent.trim() }; })()`);
// The cards in the conversation: [label, context, action labels], in the order shown.
const cards = () => evaluate(`[...document.querySelectorAll('.log .need-card')].map((card) => [
  ((card.querySelector('.need-id')?.textContent ?? '') + ' ' + card.querySelector('.row-label').textContent).trim(), card.querySelector('.row-context').textContent,
  [...card.querySelectorAll('.need-action')].map((node) => node.textContent.trim()).join('+')])`);
const trayLines = () => text(".tray-line .tray-words");
// ^ innerText: the button holds "+N" for two lines and for one line, and CSS shows one.
const trayMore = () => evaluate("document.querySelector('.tray-more')?.innerText.trim() ?? ''");
const trayVisible = () => evaluate("[...document.querySelectorAll('.tray-line')].filter((node) => node.getBoundingClientRect().height > 0).length");
const cardOf = (label) => `[...document.querySelectorAll('.log .need-card')].find((card) => ((card.querySelector('.need-id')?.textContent ?? '') + ' ' + card.querySelector('.row-label').textContent).trim().startsWith(${JSON.stringify(label)}))`;
const sheetOpen = () => evaluate("!!document.querySelector('dialog.task-sheet[open]')");
const sheetState = () => evaluate("document.querySelector('.task-sheet .bar-state')?.textContent ?? ''");

await resize([390, 844, 2]);
await pair();
const chat = `#/p/${project}`;

// --- Home: one row per project, needs first, then by activity; the bottom bar is Home and Settings.
await go("#/");
await until("document.querySelectorAll('.home-row').length >= 3", "home does not list the projects");
// ^ The task summaries load after the projects; the order settles when they are in.
await until(`[...document.querySelectorAll('.home-row .row-label')].map((node) => node.textContent).join() === ${JSON.stringify(homeOrder)}`, "home did not settle in the expected order", 30);
const homeRows = await evaluate(`[...document.querySelectorAll('.home-row')].map((row) => ({ name: row.querySelector('.row-label').textContent,
  count: row.querySelector('.count')?.textContent ?? '', status: row.querySelector('.row-context').textContent, state: row.querySelector('.row-state').textContent }))`);
if (homeRows.map((row) => row.name).join() !== homeOrder) await fail(`home lists ${JSON.stringify(homeRows)}, not ${homeOrder}`);
const uiRow = homeRows.find((row) => row.name === "ui");
if (uiRow.count !== "6" || uiRow.status !== "#1 needs an answer" || !/ago|now/.test(uiRow.state)) await fail(`the ui row reads ${JSON.stringify(uiRow)}`);
const calmRow = homeRows.at(-1);
if (calmRow.name !== "calm" || calmRow.count || calmRow.status !== "No tasks yet" || calmRow.state) await fail(`the calm row reads ${JSON.stringify(calmRow)}`);
const plansRow = homeRows.find((row) => row.name === "plans");
// ^ #1 waits as briefed; #2, built by the API checks, is ready to land.
if (plansArg && (plansRow?.status !== "Brief ready #1" || plansRow.count !== "2")) await fail(`the plans row reads ${JSON.stringify(plansRow)}`);
// Projects with agent sessions and no task: the sessions in words, and the time of the newest.
const sessionRows = { busy: "2 working", quiet: "claude in main done just now", gone: "claude in main exited" };
for (const [name, status] of Object.entries(sessionRows)) {
  const row = homeRows.find((item) => item.name === name);
  if (row?.status !== status || row.count || !/ago|now/.test(row.state)) await fail(`the ${name} row reads ${JSON.stringify(row)}, not "${status}" with a time`);
}
if ((await text(".tabbar a")).join() !== "Home,Settings") await fail(`the bottom bar is ${await text(".tabbar a")}`);
await shot("home");
// "All sessions": the session inbox, without task workers and the manager.
await evaluate("[...document.querySelectorAll('.home-more a')].find((node) => node.textContent === 'All sessions').click()");
await until("location.hash === '#/sessions' && /All sessions/.test(document.querySelector('.bar-label')?.textContent ?? '')", "All sessions did not open");
const listedSessions = await text(".rows .row-context");
if (listedSessions.some((line) => /\btask-(\d+|manager)\b|\b(task|manager)$/.test(line))) await fail(`the session list shows task sessions: ${JSON.stringify(listedSessions)}`);
await shot("sessions");
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until("location.hash === '#/' && !!document.querySelector('.home-row')", "Back did not return home");
await go("#/settings");
await until("[...document.querySelectorAll('.settings a')].some((node) => node.textContent === 'All sessions')", "Settings has no All sessions link");
// A row opens the project's chat.
await go("#/");
await until("!!document.querySelector('.home-row')", "home did not come back");
await evaluate(`[...document.querySelectorAll('.home-row')].find((row) => row.querySelector('.row-label').textContent === 'ui').click()`);
await until(`location.hash === ${JSON.stringify(chat)} && !!document.querySelector('.composer-field') && document.querySelectorAll('.log .need-card').length >= 6`, "a home row did not open the chat with its cards");
if (await count(".tabbar")) await fail("the chat shows the bottom bar");

// "Since you left": this device left the project two hours ago; the line names what happened.
await go("#/");
await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 }))`);
await go(chat);
await until("/Since you left: .*#1 #2 #3 #4 started.* repl(y|ies)/.test(document.querySelector('.since')?.textContent ?? '')", "no since-you-left line");
await shot("project-since", ".composer-send");
await evaluate("[...document.querySelectorAll('.since button')].find((node) => node.textContent === 'Dismiss').click()");
await until("!document.querySelector('.since')", "Dismiss did not close the line");
await go("#/");
await go(chat);
await pause(500);
if (await count(".since")) await fail("a return within a minute shows a since-you-left line");
await until("document.querySelectorAll('.log .need-card').length >= 6", "the chat did not reopen");

// --- The chat: no strip; each open item is a card in the conversation, in time order, with
// its buttons; an approval reads as its action; the tray shows two lines and "+N".
if (await count(".needs")) await fail("the chat still has the strip");
let shown = await cards();
if (shown.map((card) => card[0].split(" ").slice(0, 3).join(" ")).join() !== "#1 NEEDS Bash(make:*),#2 WORK a.txt,#3 WORK b.txt,#4 NEEDS Bash(make:*),Approve land #3,Approve discard #4") await fail(`the cards are ${JSON.stringify(shown)}`);
const actionsOf = Object.fromEntries(shown.map((card) => [card[0].split(" ").slice(0, 3).join(" "), card[2]]));
if (actionsOf["#1 NEEDS Bash(make:*)"] !== "Allow+Answer" || actionsOf["Approve land #3"] !== "Deny+Approve" || actionsOf["#2 WORK a.txt"] !== "Verify" || actionsOf["#3 WORK b.txt"] !== "Land") await fail(`the card buttons are ${JSON.stringify(actionsOf)}`);
const named = await evaluate("[...document.querySelectorAll('.log .need-card')].map((card) => card.textContent).find((words) => /\\ba[12]\\b/.test(words))");
if (named) await fail(`a card names an approval by its id: ${named}`);
if ((await trayLines()).join() !== "#1 needs an answer,#4 needs an answer,Approve land #3,Approve discard #4,#2 is ready to verify,#3 is ready to land") await fail(`the tray reads ${await trayLines()}`);
if (await trayVisible() !== 2 || await trayMore() !== "+4") await fail(`the tray shows ${await trayVisible()} lines and ${await trayMore()}`);
// --- The project brief: one line, unfolded on a tap.
await until("!!document.querySelector('.brief-text')", "the project shows no brief");
const folded = (await text(".brief-text"))[0];
if (folded !== "4 open · 6 need you · deploy up to date") await fail(`the folded brief reads ${folded}`);
await shot("project", ".composer-send");
await click(".brief-line");
const openLines = await text(".brief-list[aria-label='Open tasks'] li");
// ^ The title and the state are two flex items, so their texts meet without a space.
const expected = [/^#1 .*waits for you$/, /^#4 .*waits for you$/, /^#2 WORK a\.txt HVready to verify$/, /^#3 WORK b\.txtready to land$/];
if (openLines.length !== 4 || !expected.every((pattern, index) => pattern.test(openLines[index]))) await fail(`the brief lists ${JSON.stringify(openLines)}`);
const briefBody = (await text(".brief-body"))[0];
if (!/Deploy: [0-9a-f]{8}, up to date with main\./.test(briefBody) || !/Main checkout on main, /.test(briefBody)) await fail(`the brief says ${briefBody}`);
await shot("brief-open", ".composer-send");
await click(".brief-line");
// The top row: ⋯ holds Sessions (Files where granted); the worktree list hides task sessions.
await click(".more-button");
if ((await text(".more-menu a")).join() !== "Sessions") await fail(`the ⋯ menu offers ${await text(".more-menu a")}`);
await shot("more-menu", ".composer-send");
await click(".more-menu a");
await until(`location.hash === '#/p/${project}/sessions' && !!document.querySelector('.worktree')`, "Sessions did not open the worktree list");
const worktreeSessions = await text(".worktree .row-session-name");
if (worktreeSessions.some((name) => /^task-/.test(name))) await fail(`the worktree list shows task sessions: ${worktreeSessions}`);
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until(`location.hash === ${JSON.stringify(chat)} && !!document.querySelector('.need-card')`, "Back did not return to the chat");

// --- The manager chat: a reply with task chips.
await typeInto(".composer-field", "STATUS");
await click(".composer-send");
for (let attempt = 0; !(await evaluate("[...document.querySelectorAll('.message:not(.message-user) .task-chip')].length >= 3")); attempt++) {
  if (attempt > 300) {
    const api = await evaluate(`fetch('api/tasks/${project}/chat').then((r) => r.json())`);
    await fail(`the reply has no task chips; the chat API has ${JSON.stringify(api).slice(0, 600)}; the page has ${(await text(".log > *")).join(" | ").slice(0, 400)}`);
  }
  await pause(100);
}
const chips = await text(".message:not(.message-user) .task-chip");
if (!chips.some((chip) => /^#1 blocked NEEDS/.test(chip))) await fail(`the chips read ${JSON.stringify(chips)}`);
if (await evaluate("document.querySelector('.composer-field').value") !== "") await fail("the sent message stayed in the field");
await shot("project-reply", ".composer-send");
// Explain in the brief: the fixed owner message, visible in the chat.
await click(".brief-line");
await click(".brief .story-explain");
await until("[...document.querySelectorAll('.message-user')].some((node) => node.textContent.trim() === 'Explain where the project stands in plain words')", "Explain in the brief sent no message");
await click(".brief-line");

// --- The task sheet: a chip opens it over the chat; Escape, Back and a swipe down close it, the
// composer keeps its draft and gets the focus back.
await typeInto(".composer-field", "a draft that stays");
await evaluate("[...document.querySelectorAll('.message:not(.message-user) .task-chip')].find((chip) => chip.textContent.startsWith('#2')).click()");
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('dialog.task-sheet[open] .story')`, "a chip did not open the task sheet");
const sheetBox = await evaluate("(() => { const r = document.querySelector('dialog.task-sheet').getBoundingClientRect(); return { top: Math.round(r.top), height: Math.round(r.height) }; })()");
if (Math.abs(sheetBox.height - 844 * 0.85) > 2 || Math.abs(sheetBox.top - 844 * 0.15) > 2) await fail(`the sheet is not 85% high: ${JSON.stringify(sheetBox)}`);
await shot("sheet");
await key("Escape", 27);
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Escape did not close the sheet");
const kept = await evaluate("({ value: document.querySelector('.composer-field').value, focused: document.activeElement === document.querySelector('.composer-field') })");
if (kept.value !== "a draft that stays" || !kept.focused) await fail(`after the sheet the composer has ${JSON.stringify(kept)}`);
await evaluate(`${cardOf("#2")}.querySelector('.row-open').click()`);
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "a card title did not open the sheet");
await evaluate("document.querySelector('.task-sheet .bar .icon-button').click()");
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "Back did not close the sheet");
await evaluate(`${cardOf("#2")}.querySelector('.row-open').click()`);
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the sheet did not open again");
const grip = await evaluate("(() => { const r = document.querySelector('.task-sheet-top').getBoundingClientRect(); return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) }; })()");
await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: grip.x, y: grip.y }] });
for (let step = 1; step <= 6; step++) await send("Input.dispatchTouchEvent", { type: "touchMove", touchPoints: [{ x: grip.x, y: grip.y + step * 30 }] });
await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
await until(`location.hash === ${JSON.stringify(chat)} && !document.querySelector('dialog.task-sheet')`, "a swipe down did not close the sheet");
if (await evaluate("document.querySelector('.composer-field').value") !== "a draft that stays") await fail("the swipe lost the draft");
await typeInto(".composer-field", "");

// --- The `#` picker: every task, filtered by number or words, a pick inserts `#n `.
await typeInto(".composer-field", "land #");
await until("document.querySelectorAll('.slash-option').length === 4", "the # picker did not list the tasks");
const listed = await text(".slash-option .slash-name");
if (listed.join() !== "#4,#1,#3,#2") await fail(`the picker lists ${listed}`);
await shot("picker", ".composer-send");
await typeInto(".composer-field", "land #hv");
await until("document.querySelectorAll('.slash-option').length === 1", "the picker does not filter by words");
await key("Enter", 13);
await until("document.querySelector('.composer-field').value === 'land #2 '", "Enter did not insert the task");
if (await count(".slash")) await fail("the picker stayed open after a pick");
await typeInto(".composer-field", "#9");
await until("(document.querySelector('.slash-note')?.textContent ?? '') === 'No task matches that.'", "no note for a picker without matches");
await key("Escape", 27);
await until("!document.querySelector('.slash')", "Escape did not close the picker");
await typeInto(".composer-field", "#");
await until("document.querySelectorAll('.slash-option').length === 4", "the picker did not reopen");
// With the keyboard up: the picker keeps to 3.5 rows, the tray to one line, part of the chat stays in view.
await keyboard(420);
await pause(300);
const fit = await evaluate(`(() => { const box = (selector) => document.querySelector(selector).getBoundingClientRect();
  return { list: Math.round(box('.slash').height), row: Math.round(box('.slash-option').height), log: Math.round(box('.log-wrap').height),
    composer: Math.round(box('.composer').bottom), visible: Math.round(visualViewport.offsetTop + visualViewport.height) }; })()`);
if (fit.list > fit.row * 3.5 + 2 || Math.abs(fit.composer - fit.visible) > 1) await fail(`the picker does not fit above the keyboard: ${JSON.stringify(fit)}`);
await shot("picker-keyboard", ".composer-send");
await typeInto(".composer-field", "");
if (await trayVisible() !== 1 || await trayMore() !== "+5") await fail(`with the keyboard up the tray shows ${await trayVisible()} lines and ${await trayMore()}`);
await shot("project-keyboard", ".composer-send");
await evaluate("document.activeElement.blur()");
await keyboard(null);
await resize([390, 420, 2]);
await shot("project-short", ".composer-send");

// --- Cards act: Answer keeps its draft on failure; Approve turns the card into one muted line in
// place and the tray drops it. A tray line scrolls to its card.
await resize([390, 844, 2]);
await go("#/");
await go(chat);
await until("document.querySelectorAll('.log .need-card').length >= 6", "the cards did not come back");
await click(".tray-more");
await evaluate("[...document.querySelectorAll('.tray-line')].find((node) => node.textContent.startsWith('Approve land #3')).click()");
await until(`${cardOf("Approve land #3")}?.hasAttribute('data-flash') && (() => { const card = ${cardOf("Approve land #3")}.getBoundingClientRect(), log = document.querySelector('.log').getBoundingClientRect(); return card.top >= log.top && card.bottom <= log.bottom; })()`, "a tray line did not bring its card into view");
await shot("tray-open", ".composer-send");
await click(".tray-more");
await evaluate(`${cardOf("#1")}.querySelector('.need-action:last-child').click()`);
await until("!!document.querySelector('.need-card .need-form input')", "Answer did not open its field");
await typeInto(".need-card .need-form input", "NOOP use the default");
await shot("card-answer", ".composer-send");
await evaluate("document.querySelector('.need-card .need-form').requestSubmit()");
await until("!document.querySelector('.need-form')", "the answer was not sent");
await until("[...document.querySelectorAll('.notice')].some((node) => /#1:/.test(node.textContent))", "the answer's outcome was not shown");
const approveAt = await evaluate(`[...document.querySelectorAll('.log > *')].indexOf(${cardOf("Approve land #3")})`);
await evaluate(`${cardOf("Approve land #3")}.querySelectorAll('.need-action')[1].click()`);
await until("[...document.querySelectorAll('.notice')].some((node) => /Approved land #3/.test(node.textContent))", "Approve showed no outcome");
await until(`!${cardOf("Approve land #3")} && [...document.querySelectorAll('.log > *')][${approveAt}]?.matches('.resolved') && /Approved land #3/.test([...document.querySelectorAll('.log > *')][${approveAt}].textContent)`, "the approved card did not turn into a line in place", 300);
await until("![...document.querySelectorAll('.tray-line')].some((node) => node.textContent.startsWith('Approve land #3'))", "the tray kept the approved item", 300);
await until("[...document.querySelectorAll('.resolved')].some((node) => /#3/.test(node.textContent) && /landed/.test(node.textContent))", "#3 landed is not a line after the approval", 300);
await shot("cards-after-actions", ".composer-send");

// --- The task sheet of #2: the report in its sections, Fail needs a note, Pass, then Land.
const page = (n) => `#/p/${project}/t/${n}`;
await go(page(2));
await until("document.querySelectorAll('.task-sheet .task-section').length >= 7", "the task sheet did not show the report");
let told = await story();
if (told.asked !== "Asked: WORK a.txt HV" || told.current !== "Ready" || told.steps !== "done,done,done,done,todo,todo,todo"
  || told.next !== "What happens next: Check the 1 item below, then Pass to allow landing.") await fail(`the story of a task to verify reads ${JSON.stringify(told)}`);
const headings = await text(".task-section > .label");
if (headings.slice(0, 7).join() !== "Asked,Summary,Decisions,Changes,Evidence,Human Verify,Left open") await fail(`the report sections are ${headings}`);
if ((await text(".verify button")).join() !== "Fail,Pass r1") await fail(`the verify buttons are ${await text(".verify button")}`);
if ([...(await text(".task-actions button"))].join() !== "Review") await fail(`a task to verify offers ${await text(".task-actions button")}`);
await shot("task-verify");
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Review').click()");
await pause(300);
if (!(await evaluate("(() => { const r = document.querySelector('.verify').getBoundingClientRect(); return r.top >= 0 && r.bottom <= innerHeight; })()"))) await fail("Review did not bring the verdict into view");
await shot("task-review");
await evaluate("[...document.querySelectorAll('.verify button')].find((node) => node.textContent === 'Fail').click()");
await until("!!document.querySelector('.verify-note textarea')", "Fail did not ask for a note");
await evaluate("document.querySelector('.verify-note').requestSubmit()");
await pause(300);
if (!(await count(".verify-note"))) await fail("a fail without a note was sent");
await typeInto(".verify-note textarea", "HV NOOP the file has no text");
await shot("task-fail-note");
await evaluate("document.querySelector('.verify-note').requestSubmit()");
await until("[...document.querySelectorAll('.notice')].some((node) => /#2:/.test(node.textContent))", "the fail showed no outcome");
// The note went to the worker: a new revision r2 with its own Human Verify item, then Pass.
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '').startsWith('ready · r2')", "#2 did not come back as r2", 300);
await until("[...document.querySelectorAll('.verify button')].some((node) => node.textContent === 'Pass r2')", "no Pass r2 on the task page", 300);
await evaluate("[...document.querySelectorAll('.verify button')].find((node) => node.textContent === 'Pass r2').click()");
await until("!document.querySelector('.verify') && /Verdict on r2: pass/.test(document.querySelector('.task-section[aria-label=\"Human Verify\"]')?.textContent ?? '')", "the pass is not in the report", 300);
await until("[...document.querySelectorAll('.task-actions button')].some((node) => node.textContent === 'Land r2')", "no Land r2 on the task page", 300);
await click(".timeline-head");
const timeline = await text(".timeline-what");
if (!timeline.includes("Verified r1: fail") || !timeline.includes("Human verification (fail)") || !timeline.includes("Revision r2")) await fail(`the timeline reads ${timeline}`);
await shot("task-timeline");
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Land r2').click()");
await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '').startsWith('landed')", "#2 did not land", 300);
if (await count(".task-message")) await fail("a landed task still offers a message field");
await shot("task-landed");
told = await story();
if (told.current !== "Landed" || told.steps !== "done,done,done,done,done,done,todo"
  || !/^What happens next: Landed on main; the last deploy does not have it yet \(the deploy is \d+ commits? behind\)\.$/.test(told.next)) await fail(`the story of a landed task reads ${JSON.stringify(told)}`);
// Explain in the story: the fixed message about this task, and the chat to read the answer.
await click(".story .story-explain");
await until(`location.hash === ${JSON.stringify(chat)} && [...document.querySelectorAll('.message-user')].some((node) => node.querySelector('.task-chip-id')?.textContent === '#2' && /^Explain #2.* in plain words: why, what changed, risks$/.test(node.textContent.trim()))`, "Explain in the story sent no message");
await until("/main \\d+ ahead of deploy/.test(document.querySelector('.brief-text')?.textContent ?? '')", "the brief does not say the deploy is behind");

// --- The task page of #4: Deny the discard, then Allow the denied tool.
await go(page(4));
await until("/The manager asks: Approve discard #4/.test(document.querySelector('.task-actions')?.textContent ?? '')", "#4 shows no pending approval");
told = await story();
if (told.current !== "Working blocked" || told.next !== "What happens next: The manager asks: Approve discard #4. Approve or Deny it above.") await fail(`the story of a blocked task with an approval reads ${JSON.stringify(told)}`);
await shot("task-blocked", ".composer-send");
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Deny').click()");
await until("!/Approve discard #4/.test(document.querySelector('.task-actions')?.textContent ?? '')", "the denied approval stayed", 300);
told = await story();
if (told.next !== "What happens next: The worker waits for you to allow Bash(Bash(make:*)) or answer.") await fail(`the story of a blocked task reads ${JSON.stringify(told)}`);
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Allow a tool').click()");
await until("document.querySelector('.task-allow input')?.value === 'Bash(Bash(make:*))'", "Allow did not offer the denied tool");
await typeInto(".task-allow input", "Bash(make:*)");
await evaluate("document.querySelector('.task-allow').requestSubmit()");
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
if (await evaluate("history.state?.depth ?? 0") !== 0) await fail(`after the sheet the chat is not the first entry: ${await evaluate("JSON.stringify(history.state)")}`);

// --- A brief (docs/tasks.md "Brief"): its card with Reply and Start, a reply from the card, the
// sheet with the sections, folded concepts and the talk-through, a reply there, then Start.
if (plansArg) {
  const plans = Number(plansArg);
  const briefSheet = `#/p/${plans}/t/1`;
  // ^ The actions above spent this device's burst (10, then one per 3 s); this part sends three.
  await pause(9000);
  await go(`#/p/${plans}`);
  await until(`!!(${cardOf("#1")})`, "the brief has no card");
  let [card] = await cards();
  if (card[1] !== "brief ready · WORK keys.txt" || card[2] !== "Reply+Start") await fail(`the brief card reads ${JSON.stringify(card)}`);
  if ((await trayLines())[0] !== "Brief ready #1") await fail(`the tray reads ${await trayLines()}`);
  await shot("brief-card", ".composer-send");
  await evaluate(`[...(${cardOf("#1")}).querySelectorAll('.need-action')].find((node) => node.textContent.trim() === 'Reply').click()`);
  await until(`(${cardOf("#1")}).querySelector('.need-form input')?.placeholder === 'Reply to the brief'`, "Reply did not open a field");
  await typeInto(".need-card .need-form input", "Why a file? REVISE WORK keys2.txt");
  await evaluate("document.querySelector('.need-card .need-form').requestSubmit()");
  await until(`(${cardOf("#1")})?.querySelector('.row-context').textContent === 'brief ready · WORK keys2.txt'`, "the reply did not revise the brief", 60);
  // The project summary names the waiting brief in words.
  await click(".brief-line");
  await until("[...document.querySelectorAll('.brief-list[aria-label=\"Open tasks\"] li')].some((node) => /^#1 .*brief ready$/.test(node.textContent.trim()))", "the project summary does not name the waiting brief");
  await click(".brief-line");
  // The sheet: story at Brief, Start, the sections in order, concepts folded, the talk-through.
  await go(briefSheet);
  await until("!!document.querySelector('dialog.task-sheet[open] .task-talk')", "the brief sheet has no talk-through");
  told = await story();
  if (told.current !== "Brief" || told.steps !== "done,done,todo,todo,todo,skipped,todo,todo"
    || told.next !== "What happens next: The brief is ready. Read it, reply to talk it through, or Start the build.") await fail(`the story of a brief reads ${JSON.stringify(told)}`);
  const titles = await evaluate("[...document.querySelectorAll('.task-sheet .task-section > .label')].map((node) => node.textContent)");
  // ^ The fixture has no open question, so that section is left out.
  if (titles.slice(0, 7).join() !== "What a top professional would add,Raw material,Methods and tools,Independent check,Concepts to know,Rewritten prompt,Talk-through") await fail(`the brief sections are ${titles}`);
  if (await evaluate("document.querySelector('.task-concept').open")) await fail("a concept is not folded");
  await click(".task-concept-term");
  if (!(await evaluate("document.querySelector('.task-concept').open"))) await fail("a concept does not unfold");
  const talk = await text(".task-talk li");
  if (talk.length !== 2 || !talk[0].startsWith("You") || !talk[1].startsWith("Worker, brief v2") || !talk[1].includes("answered: Why a file?")) await fail(`the talk-through reads ${JSON.stringify(talk)}`);
  if ((await text(".task-actions button")).join() !== "Start the build") await fail(`a brief offers ${await text(".task-actions button")}`);
  if (await sheetState() !== "brief ready · brief v2") await fail(`the brief sheet's state reads ${await sheetState()}`);
  if (await evaluate("document.querySelector('.task-message .composer-field').placeholder") !== "Reply to the brief") await fail("the sheet's field is not a reply to the brief");
  await shot("brief-sheet", ".composer-send");
  await resize([390, 420, 2]);
  await shot("brief-sheet", ".composer-send");
  await resize([390, 844, 2]);
  // A reply in the sheet: the answer joins the talk-through.
  await typeInto(".task-message .composer-field", "And the shortcut? REVISE WORK keys3.txt");
  await click(".task-message .composer-send");
  await until("document.querySelectorAll('.task-talk li').length === 4", "the sheet's reply got no answer", 60);
  // ^ The answer is logged just before the task is briefed again and Start comes back.
  await until("[...document.querySelectorAll('.task-actions button')].some((node) => node.textContent === 'Start the build')", "Start did not come back after the answer", 60);
  await click(".task-sheet .task-actions button");
  await pause(500);
  const startError = await text(".task-sheet .error");
  if (startError.length) await fail(`Start failed: ${startError}`);
  await until("(document.querySelector('.task-sheet .bar-state')?.textContent ?? '').startsWith('ready')", "Start did not build the brief", 300);
  told = await story();
  if (told.current !== "Ready" || !told.steps.startsWith("done,done,done,done,done")) await fail(`the story after the build reads ${JSON.stringify(told)}`);
  if ((await text(".task-fold > .timeline-head")).join() !== "Brief v3") await fail(`the built task's brief is ${await text(".task-fold > .timeline-head")}`);
  if (await evaluate("document.querySelector('.task-fold').open")) await fail("the built task's brief is not folded");
  await click(".timeline-head:not(.task-fold > *)");
  const built = await text(".timeline-what");
  if (!built.includes("Brief v3") || !built.includes("Build started from brief v3")) await fail(`the built task's timeline reads ${built}`);
  await shot("brief-built");
  await key("Escape", 27);
  await until("!document.querySelector('dialog.task-sheet')", "Escape did not close the brief sheet");
  await go(chat);
  await until("document.querySelectorAll('.log .need-card').length >= 1", "the ui chat did not come back");
}

// --- Wide and narrow; on a wide screen the sheet is a panel at the right.
for (const next of [[320, 568, 2], [1280, 800, 1]]) {
  await resize(next);
  await shot("project", ".composer-send");
}
await go(page(3));
await until("!!document.querySelector('dialog.task-sheet[open] .story')", "the sheet did not open on a wide screen");
const panel = await evaluate("(() => { const r = document.querySelector('dialog.task-sheet').getBoundingClientRect(); return { left: Math.round(r.left), right: Math.round(r.right), height: Math.round(r.height) }; })()");
if (panel.right !== 1280 || panel.left < 640 || panel.height !== 800) await fail(`on a wide screen the sheet is ${JSON.stringify(panel)}`);
await shot("sheet");
await key("Escape", 27);
await until("!document.querySelector('dialog.task-sheet')", "Escape did not close the wide sheet");

const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);
fs.writeFileSync(path.join(captures, "tasks-audits.json"), JSON.stringify(audits, null, 1));
console.log(`tasks page: since-you-left, the project brief (folded line, open tasks, deploy, main checkout, deploy behind after a landing, Explain), the task story (to verify, landed and not deployed, blocked with and without an approval, Explain), home in order with All sessions and Settings, cards in the chat with their buttons, the tray (two lines, one with the keyboard up, a tap scrolls to the card), the ⋯ menu, a reply with task chips, the # picker (filter, pick, no match, Escape, keyboard up), Answer and Approve on cards (a line in place, the tray updates), the task sheet (85%, Escape, Back, swipe, draft kept, report sections, Fail with and without a note, Pass, timeline, Land, Deny, Allow), the deep link, the wide sheet panel; ${audits.length} captures`);
await cleanup();
process.exit(0);
