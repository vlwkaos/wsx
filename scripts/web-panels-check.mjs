#!/usr/bin/env node
// Real-browser check of a project's panels (docs/panels.md), over the Chrome DevTools Protocol.
// Called by `test-web-client.py --browser`.
//   web-panels-check.mjs <admin-url> <admin-code> <owner-url> <owner-code> <capture-dir> <project id>
// The admin site types into a terminal (so runs commands); the owner site has no terminal_input.
// The project declares `device` (refresh 10, prints "device run <n>"), `broken` (exit 2), `gauges`
// (format json: every kind, a bad level and an unknown kind) and `badjson` (not a document).
import { launch } from "./web-cdp.mjs";

const [adminUrl, adminCode, ownerUrl, ownerCode, captures, project] = process.argv.slice(2);
const { send, evaluate, until, viewport, capture, fail, cleanup, problems } = await launch({ captures, prefix: "panels-", timeoutSeconds: 120 });

async function open(url, code) {
  await send("Page.navigate", { url });
  await until("!!document.querySelector('#pair-code')", "pairing form did not render");
  await evaluate(`(() => { const input = document.querySelector('#pair-code'); input.value = ${JSON.stringify(code)}; input.form.requestSubmit(); })()`);
  await until("!!document.querySelector('.home-row')", "Home did not list projects after pairing");
}
const run = () => evaluate("/device run (\\d+)/.exec(document.querySelector('.panel-output')?.textContent ?? '')?.[1] ?? null");

await viewport(390, 844, 2);
// A site without the right to run commands has no Panels icon.
await open(ownerUrl, ownerCode);
await send("Page.navigate", { url: `${ownerUrl}#/p/${project}` });
await until("!!document.querySelector('.bar-tools')", "the project screen did not render on the owner site");
await new Promise((resolve) => setTimeout(resolve, 1500));
if (await evaluate("!!document.querySelector('[aria-label=\"Panels\"]')")) await fail("a site without terminal_input shows the Panels icon");

await open(adminUrl, adminCode);
await send("Page.navigate", { url: `${adminUrl}#/p/${project}` });
await until("!!document.querySelector('a[aria-label=\"Panels\"]')", "the project screen shows no Panels icon");
await capture("390x844-project-header");
await evaluate("document.querySelector('a[aria-label=\"Panels\"]').click()");
await until("!!document.querySelector('dialog[open] .panel-output') && /device run \\d+/.test(document.querySelector('.panel-output').textContent)", "the panel did not show the command's output");
const first = Number(await run());
const state = await evaluate("document.querySelector('.panel-state').textContent");
if (!/ok, updated/.test(state) || !/every 10 s/.test(state)) await fail(`the state line reads ${JSON.stringify(state)}`);
await capture("390x844-panel");
// The refresh: the page asks again after 10 s and the counter moves.
await until(`Number(${JSON.stringify(await run())}) < Number(/device run (\\d+)/.exec(document.querySelector('.panel-output')?.textContent ?? '')?.[1])`, "the panel did not refresh by itself", 40);
// A failing command: the exit code and its output.
await evaluate("[...document.querySelectorAll('.panel-tabs [role=tab]')].find((tab) => tab.textContent === 'broken').click()");
await until("/failed \\(exit 2\\)/.test(document.querySelector('.panel-state[data-failed]')?.textContent ?? '') && /broken/.test(document.querySelector('.panel-output')?.textContent ?? '')", "the failing panel shows no exit code and output");
await capture("390x844-panel-failed");
// A json panel: a card per section, a bar per gauge, a level as colour plus its word.
const tab = (name) => evaluate(`[...document.querySelectorAll('.panel-tabs [role=tab]')].find((tab) => tab.textContent === ${JSON.stringify(name)}).click()`);
await tab("gauges");
await until("document.querySelectorAll('.panel-doc .panel-card').length === 2", "the json panel shows no card per section");
const doc = await evaluate(`(() => {
  const meters = [...document.querySelectorAll('[role=meter]')].map((meter) => [meter.getAttribute('aria-label'), meter.getAttribute('aria-valuenow'), meter.getAttribute('aria-valuemax'), meter.getAttribute('aria-valuetext'), Math.round(meter.firstElementChild.getBoundingClientRect().width / meter.getBoundingClientRect().width * 100)]);
  const levels = [...document.querySelectorAll('.panel-level')].map((level) => level.textContent + ':' + level.dataset.level + ':' + getComputedStyle(level).color);
  return { title: document.querySelector('.panel-title')?.textContent, meters, levels, cells: [...document.querySelectorAll('.panel-table td')].map((cell) => cell.textContent),
    list: [...document.querySelectorAll('.panel-list li')].map((item) => item.textContent), notice: document.querySelector('.panel-notice')?.textContent ?? '', raw: !!document.querySelector('.panel-output'),
    wide: document.documentElement.scrollWidth > innerWidth };
})()`);
const want = (what, ok) => ok || fail(`the json panel: ${what}: ${JSON.stringify(doc)}`);
await want("title", doc.title === "This Mac");
await want("memory gauge", JSON.stringify(doc.meters[0]) === JSON.stringify(["Memory", "13.6", "16", "13.6 / 16 GB · 85%", 85]));
await want("swap gauge", doc.meters[1][1] === "0" && doc.meters[1][4] === 0);
await want("level words", doc.levels.map((level) => level.split(":").slice(0, 2).join(":")).join(",") === "warn:warn,ok:ok,ok:ok,bad:bad");
await want("level colours differ", new Set(doc.levels.map((level) => level.split(":")[2])).size === 3);
await want("table", doc.cells.join("|") === "Macintosh HD|412 GB|88 GB|Data|12|3" && doc.list.length === 2);
await want("the notice for the skipped items", /`red`/.test(doc.notice) && /`chart`/.test(doc.notice) && !doc.raw);
await want("no sideways page scroll", !doc.wide);
await capture("390x844-panel-json");
// Not a document: the text as written and the notice.
await tab("badjson");
await until("/oops/.test(document.querySelector('.panel-output')?.textContent ?? '') && /not a panel document/.test(document.querySelector('.panel-notice')?.textContent ?? '') && !document.querySelector('.panel-doc')", "the broken json panel shows no text and notice");
await capture("390x844-panel-badjson");
await tab("gauges");
await until("document.querySelectorAll('.panel-doc .panel-card').length === 2", "the json panel did not come back");
await viewport(1280, 800, 1);
await capture("1280x800-panel-json");
await tab("broken");
await until("/failed \\(exit 2\\)/.test(document.querySelector('.panel-state[data-failed]')?.textContent ?? '')", "the failing panel did not come back");
await capture("1280x800-panel-failed");
const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`page problems: ${defects.join("; ")}`);
console.log(`panels page: the icon only with terminal_input, output as written (first run ${first}), refreshes by itself, a failing command shows its exit code, a json panel draws cards, bars and level words`);
await cleanup();
process.exit(0);
