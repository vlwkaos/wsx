#!/usr/bin/env node
// Real-browser check of adding a project from Home's New chat and removing one on the Projects page (docs/web-client.md "Projects"),
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
// ^ Home lists a project by its main chat's row; the Projects page (`#/projects`, one step from Sessions) lists the projects and removes them.
const rows = "[...document.querySelectorAll('.home-rows > .row:has(.home-row)')]";
const homeRow = (project) => `${rows}.find((row) => row.querySelector('.row-label').firstChild.textContent === ${JSON.stringify(project)})`;
const listed = "[...document.querySelectorAll('.page .rows > .row')]";
const rowOf = (project) => `${listed}.find((row) => row.querySelector('.row-label').firstChild.textContent === ${JSON.stringify(project)})`;

const open = (url, code) => pair(cdp, url, code, { ready: ".home-row" });
// A swipe left: the browser's own scroll, as a finger moves it.
const slide = (project) => evaluate(`(() => { const row = ${rowOf(project)}; row.scrollLeft = row.scrollWidth; })()`);
// ^ Typed as a person types: the sheet reads the folder for the text it is given.
const type = (value) => evaluate(`(() => { const field = document.querySelector('dialog.add-project input[name=path]'); field.value = ${JSON.stringify(value)};
  field.dispatchEvent(new Event('input', { bubbles: true })); })()`);
const addButton = "document.querySelector('dialog.add-project button[type=submit]')";

await viewport(390, 844, 2);
await open(viewerUrl, viewerCode);
// ^ Add project is the last choice of the New chat sheet's project list, where the site manages projects.
const choices = () => evaluate(`[...document.querySelectorAll('dialog.sheet[open] select[name=project] option')].map((option) => option.textContent)`);
const without = await evaluate(`({ remove: !!document.querySelector('[aria-label^="Remove project"]'), slide: !!document.querySelector('.row-slide .home-row [aria-label^="Remove"]'), separate: !!document.querySelector('[aria-label="Add project"]') })`);
if (without.remove || without.slide || without.separate) await fail(`a site without the grant shows project buttons: ${JSON.stringify(without)}`);
if (await evaluate("!!document.querySelector('[aria-label=\"New chat\"]')")) {
  await evaluate("document.querySelector('[aria-label=\"New chat\"]').click()");
  await until("!!document.querySelector('dialog.sheet[open] select[name=project]')", "the New chat sheet did not open");
  if ((await choices()).includes("Add project…")) await fail("a site without the grant offers Add project…");
  await evaluate("document.querySelector('dialog.sheet').close()");
  await until("!document.querySelector('dialog.sheet')", "the New chat sheet stayed open");
}

await open(adminUrl, adminCode);
const all = await evaluate(`({ add: document.querySelectorAll('[aria-label="New chat"]').length, separate: document.querySelectorAll('[aria-label="Add project"]').length, rows: ${rows}.length,
  remove: ${rows}.filter((row) => row.classList.contains('row-slide') && row.querySelector('[aria-label^="Dismiss chat"]')).length,
  removeProject: !!document.querySelector('[aria-label^="Remove project"]'),
  box: document.querySelector('[aria-label="New chat"]').getBoundingClientRect().toJSON() })`);
if (all.add !== 1 || all.separate || !all.rows || all.remove !== all.rows || all.removeProject) await fail(`the managing site shows ${JSON.stringify(all)}`);
if (all.box.width < 44 || all.box.height < 44) await fail(`the + is ${all.box.width}x${all.box.height}, under 44 px`);
await linter.lint("home");
await capture("390x844-home");

// A folder that cannot be added keeps Add off and says why in the sheet; the sheet stays open.
await evaluate("document.querySelector('[aria-label=\"New chat\"]').click()");
await until("!!document.querySelector('dialog.sheet[open] select[name=project]')", "the + opened no New chat sheet");
{
  const list = await choices();
  if (list.at(-1) !== "Add project…" || list.filter((name) => name === "Add project…").length !== 1) await fail(`the project list is ${JSON.stringify(list)}, not ending with Add project…`);
}
await capture("390x844-new-chat");
await evaluate(`(() => { const field = document.querySelector('dialog.sheet[open] select[name=project]'); field.value = 'add'; field.dispatchEvent(new Event('change', { bubbles: true })); })()`);
await until("document.querySelector('dialog.add-project[open] input[name=path]') === document.activeElement", "the add sheet did not take the focus");
await type(`${repo}-missing`);
await until("/Nothing matches/.test(document.querySelector('dialog.add-project .note')?.textContent ?? '')", "the sheet does not say a missing path matches nothing", 20);
if (!(await evaluate(`${addButton}.disabled`))) await fail("Add is on for a missing path");
await capture("390x844-add-refused");
await type(repo);
await until(`${addButton}.disabled === false && document.querySelector('dialog.add-project .folder-card')?.textContent.includes(${JSON.stringify(name)})`, "the sheet does not offer the repository", 20);
await evaluate(`${addButton}.click()`);
// ^ From New chat, the sheet goes back to New chat with the new project chosen; it is closed here.
await until(`!document.querySelector('dialog.add-project') && document.querySelector('dialog.sheet[open] select[name=project] option:checked')?.textContent === ${JSON.stringify(name)}`, "New chat did not come back with the added project", 20);
await evaluate("document.querySelector('dialog.sheet[open]').close()");
// ^ An added project has no chat, so Home has no row for it until a chat is made (docs/tasks.md "Chats").
if (await evaluate(`!!${homeRow(name)}`)) await fail("Home lists a project that has no chat");

// The Projects page: the site that manages projects has + and a slide to Remove there.
await evaluate("location.hash = '#/projects'");
await until(`!!${rowOf(name)}`, "the Projects page does not list the added project");
if (!(await evaluate("!!document.querySelector('.page [aria-label=\"Add project\"]')"))) await fail("the Projects page has no + to add a project");
await linter.lint("projects");
await capture("390x844-projects");

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
await until(`!${rowOf(name)}`, "the removed project stayed on the Projects page", 100);
await evaluate("location.hash = '#/'");
await until(`!!document.querySelector('.home-rows') && !${homeRow(name)}`, "the removed project stayed on Home", 100);
await evaluate("location.hash = '#/projects'");
await until(`!!${rowOf(busy)}`, "the Projects page does not list the project with an open task");
if (!fs.existsSync(path.join(repo, ".git"))) await fail("Remove deleted the project's folder");

// A project with an open task goes the same way, with its sessions halted when the box is ticked.
await askRemove(busy);
await evaluate("document.querySelector('dialog.sheet input[name=halt]').click()");
if (!(await evaluate("document.querySelector('dialog.sheet input[name=halt]').checked"))) await fail("the halt box does not tick");
await capture("390x844-remove-halt");
await evaluate("document.querySelector('dialog.sheet form').requestSubmit()");
await until(`!${rowOf(busy)}`, "a project with an open task stayed on the Projects page", 100);
if (await evaluate("!!document.querySelector('.notices')?.textContent.includes('open tasks')")) await fail("Remove still refuses a project with open tasks");

await evaluate("location.hash = '#/'");
await viewport(1280, 800, 1);
await pause(300);
if (!(await evaluate("!!document.querySelector('.rail [aria-label=\"New chat\"]')"))) await fail("the rail shows no + at 1280 px");
await linter.lint("home");
await capture("1280x800-home");
// ^ The refusal is the point: 401 before pairing.
const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`page problems: ${defects.join("; ")}`);
await linter.finish(fail);
console.log("projects page: the + opens New chat and its project list ends with Add project… only on the managing site, a folder that cannot be added keeps Add off and New chat comes back with the added one, Remove asks first (Cancel keeps the row; halt sessions is a box, off by default) and takes effect, a project with an open task is removed too");
await cleanup();
process.exit(0);
