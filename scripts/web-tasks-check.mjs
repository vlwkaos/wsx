#!/usr/bin/env node
// Real-browser check of manager mode on the page (docs/web-client.md "Tasks"), driven over the
// Chrome DevTools Protocol. Called by `test-web-client.py --part tasks --browser`.
//   web-tasks-check.mjs <site-url> <code> <project-id> <capture-dir> [slop-audit.js]
// The project has: #1 and #4 blocked on a tool, #2 ready with Human Verify, #3 ready to land,
// and two pending approvals: a1 lands #3, a2 discards #4 (shown as "Approve land #3", "Approve discard #4"). Each capture is saved with the slop audit and, on a phone, the
// mobile audit of /ui-view-standards beside it (WSX_MOBILE_AUDIT names the mobile audit).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { launch } from "./web-cdp.mjs";

const [siteUrl, code, projectArg, captures, auditPath] = process.argv.slice(2);
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
    const scrollers = [...document.querySelectorAll('#app *')].filter((node) => node.tagName !== 'TEXTAREA' && !free(node) && !node.closest('.rail, .needs, .brief, .slash')
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
    if (mobile.focusedField?.startsWith("textarea.composer-field") && mobile.otherFocusableFields?.count) {
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

// The strip as rows: [id, kind words, action labels].
// The task story as words: what was asked, the step marked current (with its stop word),
// each step's state, and what happens next.
const story = () => evaluate(`(() => { const words = (li) => li.querySelector('.story-name').textContent + (li.querySelector('.story-word')?.textContent ?? '');
  const current = document.querySelector('.story-steps [aria-current=step]');
  return { asked: document.querySelector('.story-asked')?.textContent.trim(), current: current && words(current),
    steps: [...document.querySelectorAll('.story-steps li')].map((li) => li.dataset.step).join(), next: document.querySelector('.story-next')?.textContent.trim() }; })()`);
const strip = () => evaluate(`[...document.querySelectorAll('.needs .need')].map((row) => [
  (row.querySelector('.need-id') ?? row.querySelector('.row-label')).textContent, row.querySelector('.row-context').textContent,
  [...row.querySelectorAll('.need-action')].map((node) => node.textContent.trim()).join('+')])`);

await resize([390, 844, 2]);
await pair();
const home = `#/p/${project}`;
await go(home);
await until("document.querySelectorAll('.needs .need').length >= 2 && !!document.querySelector('.composer-field')", "the project screen did not open on the manager");
// "Since you left": this device left the project two hours ago; the line names what happened.
await go("#/");
await evaluate(`localStorage.setItem('wsx-project-left', JSON.stringify({ ${project}: Date.now() - 2 * 3600 * 1000 }))`);
await go(home);
await until("/Since you left: .*#1 #2 #3 #4 started.* repl(y|ies)/.test(document.querySelector('.since')?.textContent ?? '')", "no since-you-left line");
await shot("project-since", ".composer-send");
await evaluate("[...document.querySelectorAll('.since button')].find((node) => node.textContent === 'Dismiss').click()");
await until("!document.querySelector('.since')", "Dismiss did not close the line");
await go("#/");
await go(home);
await pause(500);
if (await count(".since")) await fail("a return within a minute shows a since-you-left line");
await until("document.querySelectorAll('.needs .need').length >= 2", "the project screen did not reopen");

// --- The project screen: strip order, folding, the working line, the top row.
let rows = await strip();
if (rows.map((row) => row[0]).join() !== "#1,#4") await fail(`the strip starts with ${JSON.stringify(rows)}`);
if (!/needs an answer/.test(rows[0][1]) || rows[0][2] !== "Allow+Answer") await fail(`the blocked row reads ${JSON.stringify(rows[0])}`);
if ((await text(".needs-more")).join() !== "Show 4 more") await fail(`the strip does not fold: ${await text(".needs-more")}`);
// --- The project brief: one line, unfolded on a tap.
await until("!!document.querySelector('.brief-text')", "the project shows no brief");
const folded = (await text(".brief-text"))[0];
if (folded !== "4 open · 6 need you · deploy up to date") await fail(`the folded brief reads ${folded}`);
await shot("project", ".composer-send");
await click(".needs-more");
rows = await strip();
if (rows.map((row) => row[0]).join() !== "#1,#4,Approve land #3,Approve discard #4,#2,#3") await fail(`the open strip is ${JSON.stringify(rows)}`);
if (rows[2][2] !== "Deny+Approve" || /\ba1\b/.test(await evaluate("document.querySelector('.needs').textContent"))) await fail(`the approval row reads ${JSON.stringify(rows[2])}`);
if (rows[4][2] !== "Verify" || rows[5][2] !== "Land") await fail(`verify and land rows read ${JSON.stringify(rows.slice(4))}`);
await shot("project-strip-open", ".composer-send");
await click(".brief-line");
const openLines = await text(".brief-list[aria-label='Open tasks'] li");
// ^ The title and the state are two flex items, so their texts meet without a space.
const expected = [/^#1 .*waits for you$/, /^#4 .*waits for you$/, /^#2 WORK a\.txt HVready to verify$/, /^#3 WORK b\.txtready to land$/];
if (openLines.length !== 4 || !expected.every((pattern, index) => pattern.test(openLines[index]))) await fail(`the brief lists ${JSON.stringify(openLines)}`);
const briefBody = (await text(".brief-body"))[0];
if (!/Deploy: [0-9a-f]{8}, up to date with main\./.test(briefBody) || !/Main checkout on main, /.test(briefBody)) await fail(`the brief says ${briefBody}`);
await shot("brief-open", ".composer-send");
await click(".brief-line");
// One tap from the top row to the sessions, and Back to the manager.
if ((await text(".bar .bar-link")).join() !== "Sessions") await fail("the top row has no Sessions link");
await click(".bar .bar-link");
await until(`location.hash === '#/p/${project}/sessions' && !!document.querySelector('.worktree')`, "Sessions did not open the worktree list");
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until(`location.hash === ${JSON.stringify(home)} && !!document.querySelector('.needs')`, "Back did not return to the manager");

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
// A chip opens its task page; Back returns to the manager.
await evaluate("[...document.querySelectorAll('.message:not(.message-user) .task-chip')].find((chip) => chip.textContent.startsWith('#2')).click()");
await until(`location.hash === '#/p/${project}/t/2' && !!document.querySelector('.task-section')`, "a chip did not open its task page");
await evaluate("document.querySelector('.bar a.icon-button').click()");
await until(`location.hash === ${JSON.stringify(home)} && !!document.querySelector('.needs')`, "Back did not return from the task page");
if (await evaluate("document.querySelector('.composer-field').value") !== "") await fail("the sent message stayed in the field");
await shot("project-reply", ".composer-send");
// Explain in the brief: the fixed owner message, visible in the chat.
await until("!!document.querySelector('.brief-line')", "the brief did not come back with the project");
await click(".brief-line");
await click(".brief .story-explain");
await until("[...document.querySelectorAll('.message-user')].some((node) => node.textContent.trim() === 'Explain where the project stands in plain words')", "Explain in the brief sent no message");
await click(".brief-line");

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
// With the keyboard up: the picker keeps to 3.5 rows, part of the chat stays in view.
await keyboard(420);
await pause(300);
const fit = await evaluate(`(() => { const box = (selector) => document.querySelector(selector).getBoundingClientRect();
  return { list: Math.round(box('.slash').height), row: Math.round(box('.slash-option').height), log: Math.round(box('.log-wrap').height),
    composer: Math.round(box('.composer').bottom), visible: Math.round(visualViewport.offsetTop + visualViewport.height) }; })()`);
if (fit.list > fit.row * 3.5 + 2 || Math.abs(fit.composer - fit.visible) > 1) await fail(`the picker does not fit above the keyboard: ${JSON.stringify(fit)}`);
await shot("picker-keyboard", ".composer-send");
await typeInto(".composer-field", "");
await shot("project-keyboard", ".composer-send");
await evaluate("document.activeElement.blur()");
await keyboard(null);
await resize([390, 420, 2]);
await shot("project-short", ".composer-send");

// --- Fixed actions from the strip: Answer keeps its draft on failure, Land and Approve go to wsx.
await resize([390, 844, 2]);
await go("#/");
await go(home);
await until("document.querySelectorAll('.needs .need').length >= 2", "the strip did not come back");
await evaluate("[...document.querySelectorAll('.needs .need')][0].querySelector('.need-action:last-child').click()");
await until("!!document.querySelector('.need-form input')", "Answer did not open its field");
await typeInto(".need-form input", "NOOP use the default");
await shot("strip-answer", ".composer-send");
// With the keyboard up the strip folds to one line, except while its own field is typed in.
await keyboard(420);
await pause(300);
if (!(await evaluate("!!document.querySelector('.need-form input') && document.querySelector('.need-form input').getBoundingClientRect().height > 0"))) await fail("the keyboard hid the answer field");
await shot("strip-answer-keyboard", ".composer-send");
await keyboard(null);
await evaluate("document.querySelector('.need-form').requestSubmit()");
await until("!document.querySelector('.need-form')", "the answer was not sent");
await until("[...document.querySelectorAll('.notice')].some((node) => /#1:/.test(node.textContent))", "the answer's outcome was not shown");
await click(".needs-more");
await evaluate("[...document.querySelectorAll('.needs .need')].find((row) => row.querySelector('.row-label').textContent === 'Approve land #3').querySelectorAll('.need-action')[1].click()");
await until("[...document.querySelectorAll('.notice')].some((node) => /Approved land #3/.test(node.textContent))", "Approve showed no outcome");
await until("![...document.querySelectorAll('.needs .row-label')].some((node) => node.textContent === 'Approve land #3')", "the approval stayed in the strip", 300);
await until("[...document.querySelectorAll('.needs .need')].some((row) => row.querySelector('.need-id')?.textContent === '#3' && /done/.test(row.textContent))", "#3 is not done after the approval", 300);
await shot("strip-after-actions", ".composer-send");

// --- The task page of #2: the report in its sections, Fail needs a note, Pass, then Land.
const page = (n) => `#/p/${project}/t/${n}`;
await go(page(2));
await until("document.querySelectorAll('.task-section').length >= 7", "the task page did not show the report");
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
await until("(document.querySelector('.bar-state')?.textContent ?? '').startsWith('ready · r2')", "#2 did not come back as r2", 300);
await until("[...document.querySelectorAll('.verify button')].some((node) => node.textContent === 'Pass r2')", "no Pass r2 on the task page", 300);
await evaluate("[...document.querySelectorAll('.verify button')].find((node) => node.textContent === 'Pass r2').click()");
await until("!document.querySelector('.verify') && /Verdict on r2: pass/.test(document.querySelector('.task-section[aria-label=\"Human Verify\"]')?.textContent ?? '')", "the pass is not in the report", 300);
await until("[...document.querySelectorAll('.task-actions button')].some((node) => node.textContent === 'Land r2')", "no Land r2 on the task page", 300);
await click(".timeline-head");
const timeline = await text(".timeline-what");
if (!timeline.includes("Verified r1: fail") || !timeline.includes("Human verification (fail)") || !timeline.includes("Revision r2")) await fail(`the timeline reads ${timeline}`);
await shot("task-timeline");
await evaluate("[...document.querySelectorAll('.task-actions button')].find((node) => node.textContent === 'Land r2').click()");
await until("(document.querySelector('.bar-state')?.textContent ?? '').startsWith('landed')", "#2 did not land", 300);
if (await count(".task-message")) await fail("a landed task still offers a message field");
await shot("task-landed");
told = await story();
if (told.current !== "Landed" || told.steps !== "done,done,done,done,done,done,todo"
  || !/^What happens next: Landed on main; the last deploy does not have it yet \(the deploy is \d+ commits? behind\)\.$/.test(told.next)) await fail(`the story of a landed task reads ${JSON.stringify(told)}`);
// Explain in the story: the fixed message about this task, and the chat to read the answer.
await click(".story .story-explain");
await until(`location.hash === ${JSON.stringify(home)} && [...document.querySelectorAll('.message-user')].some((node) => node.querySelector('.task-chip-id')?.textContent === '#2' && /^Explain #2.* in plain words: why, what changed, risks$/.test(node.textContent.trim()))`, "Explain in the story sent no message");
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
await until("(document.querySelector('.bar-state')?.textContent ?? '').startsWith('ready')", "#4 did not resume after Allow", 300);

// "Done, not seen": #2 was opened after it landed, #3 was not.
await go(home);
await until("!!document.querySelector('.needs')", "the project did not reopen");
await click(".needs-more").catch(() => {});
const done = await evaluate("[...document.querySelectorAll('.needs .need')].filter((row) => /done/.test(row.querySelector('.row-context').textContent)).map((row) => row.querySelector('.need-id').textContent)");
if (done.join() !== "#3") await fail(`done, not seen lists ${done}`);

// --- The inbox: each project's items under its name, the same done-not-seen rule.
await go("#/");
await until("!!document.querySelector('section[aria-label=Tasks] .need')", "the inbox lists no task items");
const groups = await evaluate(`Object.fromEntries([...document.querySelectorAll('section[aria-label=Tasks] .group')].map((group) =>
  [group.querySelector('.group-name').textContent, [...group.querySelectorAll('.need-id')].map((node) => node.textContent)]))`);
if (!groups.ui || groups.ui.includes("#2") || !groups.ui.includes("#3")) await fail(`the inbox groups read ${JSON.stringify(groups)}`);
if (!groups.demo?.length) await fail(`the inbox has no group for demo: ${JSON.stringify(groups)}`);
await shot("inbox");
await resize([320, 568, 2]);
await shot("inbox");
await evaluate("[...document.querySelectorAll('section[aria-label=Tasks] .group-name')].find((node) => node.textContent === 'ui').click()");
await until(`location.hash === ${JSON.stringify(home)}`, "a project heading in the inbox did not open the project");
await resize([390, 844, 2]);

// --- Wide and narrow.
for (const next of [[320, 568, 2], [1280, 800, 1]]) {
  await resize(next);
  await shot("project", ".composer-send");
}

const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`console errors: ${defects.join(" | ")}`);
fs.writeFileSync(path.join(captures, "tasks-audits.json"), JSON.stringify(audits, null, 1));
console.log(`tasks page: since-you-left, the project brief (folded line, open tasks, deploy, main checkout, deploy behind after a landing, Explain), the task story (to verify, landed and not deployed, blocked with and without an approval, Explain), project screen with the strip folded and open, Sessions one tap away, a reply with task chips, the # picker (filter, pick, no match, Escape, keyboard up), Answer and Approve from the strip, the task page (report sections, Fail with and without a note, Pass, timeline, Land, Deny, Allow), done-not-seen, the inbox grouped by project; ${audits.length} captures`);
await cleanup();
process.exit(0);
