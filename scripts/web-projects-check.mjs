#!/usr/bin/env node
// Real-browser check of adding and removing a project on Home (docs/web-client.md "Projects"),
// over the Chrome DevTools Protocol. Called by `test-web-client.py --browser`.
//   web-projects-check.mjs <admin-url> <admin-code> <viewer-url> <viewer-code> <capture-dir> <git repo> <project with an open task>
// The admin site reaches every project and types into a terminal; the viewer site has neither.
import fs from "node:fs";
import path from "node:path";
import { designLint, launch, pair, pause } from "./web-cdp.mjs";

const [adminUrl, adminCode, viewerUrl, viewerCode, captures, repo, busy] = process.argv.slice(2);
const cdp = await launch({ captures, prefix: "projects-", timeoutSeconds: 200 });
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = cdp;
const linter = designLint({ evaluate, captures, file: "projects" });
const name = path.basename(repo);
const rows = "[...document.querySelectorAll('.home-rows > .row')]";
const rowOf = (project) => `${rows}.find((row) => row.querySelector('.row-label').firstChild.textContent === ${JSON.stringify(project)})`;

const open = (url, code) => pair(cdp, url, code, { ready: ".home-row" });
// A swipe left: the browser's own scroll, as a finger moves it.
const slide = (project) => evaluate(`(() => { const row = ${rowOf(project)}; row.scrollLeft = row.scrollWidth; })()`);
const submit = (value) => evaluate(`(() => { const field = document.querySelector('dialog.sheet input[name=path]'); field.value = ${JSON.stringify(value)};
  field.form.requestSubmit(); })()`);

await viewport(390, 844, 2);
await open(viewerUrl, viewerCode);
const without = await evaluate(`({ add: !!document.querySelector('[aria-label="Add project"]'), remove: !!document.querySelector('.row-remove'), slide: !!document.querySelector('.row-slide') })`);
if (without.add || without.remove || without.slide) await fail(`a site without the grant shows project buttons: ${JSON.stringify(without)}`);

await open(adminUrl, adminCode);
const all = await evaluate(`({ add: document.querySelectorAll('[aria-label="Add project"]').length, rows: ${rows}.length,
  remove: ${rows}.filter((row) => row.classList.contains('row-slide') && row.querySelector('.row-remove')).length,
  box: document.querySelector('[aria-label="Add project"]').getBoundingClientRect().toJSON() })`);
if (all.add !== 1 || !all.rows || all.remove !== all.rows) await fail(`the managing site shows ${JSON.stringify(all)}`);
if (all.box.width < 44 || all.box.height < 44) await fail(`the + is ${all.box.width}x${all.box.height}, under 44 px`);
await linter.lint("home");
await capture("390x844-home");

// A refusal stays in the sheet; the sheet stays open with the field focused.
await evaluate("document.querySelector('[aria-label=\"Add project\"]').click()");
await until("document.querySelector('dialog.sheet[open] input[name=path]') === document.activeElement", "the add sheet did not take the focus");
await submit(`${repo}-missing`);
await until("/does not exist/.test(document.querySelector('dialog.sheet .error')?.textContent ?? '')", "the sheet shows no refusal for a missing path");
if (!(await evaluate("document.querySelector('dialog.sheet').open"))) await fail("a refusal closed the sheet");
await capture("390x844-add-refused");
await submit(repo);
await until(`!!${rowOf(name)}`, "the added project did not appear on Home", 100);
if (await evaluate("!!document.querySelector('dialog.sheet[open]')")) await fail("the sheet stayed open after the add");

// Remove: slide, then the button, then the sheet that asks. Cancel leaves the row; the folder stays.
const askRemove = async (project) => {
  // ^ A closed sheet leaves the page when its `close` event has been handled, a moment after it stops being open.
  await until("!document.querySelector('dialog.sheet')", "the earlier sheet stayed in the page");
  await slide(project);
  await until(`${rowOf(project)}.hasAttribute('data-open')`, "the row did not open its Remove");
  await evaluate(`${rowOf(project)}.querySelector('.row-remove').click()`);
  await until("!!document.querySelector('dialog.sheet[open] input[name=halt]')", "Remove did not ask");
  if (await evaluate("document.querySelector('dialog.sheet input[name=halt]').checked")) await fail("the halt choice is on by default");
};
await askRemove(name);
await capture("390x844-remove");
await evaluate("[...document.querySelectorAll('dialog.sheet button')].find((button) => button.textContent === 'Cancel').click()");
await until("!document.querySelector('dialog.sheet[open]')", "Cancel left the sheet open");
if (!(await evaluate(`!!${rowOf(name)}`))) await fail("Cancel removed the project");
await askRemove(name);
await evaluate("document.querySelector('dialog.sheet form').requestSubmit()");
await until(`!${rowOf(name)}`, "the removed project stayed on Home", 100);
if (!fs.existsSync(path.join(repo, ".git"))) await fail("Remove deleted the project's folder");

// A project with an open task goes the same way, with its sessions halted when the box is ticked.
await askRemove(busy);
await evaluate("document.querySelector('dialog.sheet input[name=halt]').click()");
if (!(await evaluate("document.querySelector('dialog.sheet input[name=halt]').checked"))) await fail("the halt box does not tick");
await capture("390x844-remove-halt");
await evaluate("document.querySelector('dialog.sheet form').requestSubmit()");
await until(`!${rowOf(busy)}`, "a project with an open task stayed on Home", 100);
if (await evaluate("!!document.querySelector('.notices')?.textContent.includes('open tasks')")) await fail("Remove still refuses a project with open tasks");

await viewport(1280, 800, 1);
await pause(300);
if (!(await evaluate("!!document.querySelector('.rail [aria-label=\"Add project\"]')"))) await fail("the rail shows no + at 1280 px");
await linter.lint("home");
await capture("1280x800-home");
// ^ The refusals are the point: 401 before pairing, 409 for the missing path.
const defects = problems.filter((problem) => !/status of 401/.test(problem) && !/status of 409 .*\/api\/projects$/.test(problem));
if (defects.length) await fail(`page problems: ${defects.join("; ")}`);
await linter.finish(fail);
console.log("projects page: + and Remove only on the managing site, a refusal stays in the sheet, Remove asks first (Cancel keeps the row; halt sessions is a box, off by default) and takes effect, a project with an open task is removed too");
await cleanup();
process.exit(0);
