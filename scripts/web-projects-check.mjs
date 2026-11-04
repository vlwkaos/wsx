#!/usr/bin/env node
// Real-browser check of adding and removing a project on Home (docs/web-client.md "Projects"),
// over the Chrome DevTools Protocol. Called by `test-web-client.py --part tasks --browser`.
//   web-projects-check.mjs <admin-url> <admin-code> <viewer-url> <viewer-code> <capture-dir> <git repo> <project with an open task>
// The admin site reaches every project and types into a terminal; the viewer site has neither.
import fs from "node:fs";
import path from "node:path";
import { launch, pause } from "./web-cdp.mjs";

const [adminUrl, adminCode, viewerUrl, viewerCode, captures, repo, busy] = process.argv.slice(2);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({ captures, prefix: "projects-", timeoutSeconds: 200 });
const name = path.basename(repo);
const rows = "[...document.querySelectorAll('.home-rows > .row')]";
const rowOf = (project) => `${rows}.find((row) => row.querySelector('.row-label').firstChild.textContent === ${JSON.stringify(project)})`;

async function open(url, code) {
  await send("Page.navigate", { url });
  await until("!!document.querySelector('#pair-code')", "pairing form did not render");
  await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)}; input.form.requestSubmit(); })()`);
  await until("!!document.querySelector('.home-row')", "Home did not list projects after pairing");
}
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

// Remove: slide, then the button. The folder stays.
await slide(name);
await until(`${rowOf(name)}.hasAttribute('data-open')`, "the row did not open its Remove");
await capture("390x844-remove");
await evaluate(`${rowOf(name)}.querySelector('.row-remove').click()`);
await until(`!${rowOf(name)}`, "the removed project stayed on Home", 100);
if (!fs.existsSync(path.join(repo, ".git"))) await fail("Remove deleted the project's folder");

// A project with an open task: the refusal is a toast, the row stays.
await slide(busy);
await evaluate(`${rowOf(busy)}.querySelector('.row-remove').click()`);
await until(`document.querySelector('.notices')?.textContent.includes(${JSON.stringify(`${busy} has open tasks`)})`, "no refusal for a project with open tasks");
if (!(await evaluate(`!!${rowOf(busy)}`))) await fail("a refused project left Home");
await capture("390x844-remove-refused");

await viewport(1280, 800, 1);
await pause(300);
if (!(await evaluate("!!document.querySelector('.rail [aria-label=\"Add project\"]')"))) await fail("the rail shows no + at 1280 px");
await capture("1280x800-home");
if (problems.length) await fail(`page problems: ${problems.join("; ")}`);
console.log("projects page: + and Remove only on the managing site, a refusal stays in the sheet, add and remove take effect, an open task refuses a remove");
await cleanup();
process.exit(0);
