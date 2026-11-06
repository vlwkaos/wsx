#!/usr/bin/env node
// Real-browser check of a project's panels (docs/panels.md), over the Chrome DevTools Protocol.
// Called by `test-web-client.py --browser`.
//   web-panels-check.mjs <admin-url> <admin-code> <owner-url> <owner-code> <capture-dir> <project id>
// The admin site types into a terminal (so runs commands); the owner site has no terminal_input.
// The project declares `device` (refresh 10, prints "device run <n>"), `broken` (exit 2), `gauges`
// (format json: every kind, a bad level and an unknown kind) and `badjson` (not a document);
// `device` and `gauges` are `scope: machine` (the Machine tab, docs/panels.md "Machine panels").
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

// ... and no Machine tab.
await send("Page.navigate", { url: `${ownerUrl}#/` });
await until("document.querySelectorAll('.tabbar a').length > 0", "the owner site's bottom bar did not render");
await new Promise((resolve) => setTimeout(resolve, 1500));
const labels = () => evaluate("[...document.querySelectorAll('.tabbar a')].map((tab) => tab.querySelector('.tab-label').textContent).join()");
if ((await labels()) !== "Home,Sessions,Settings") await fail(`a site without terminal_input has the bar ${await labels()}`);

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
// The Machine tab: the machine panels of every granted project, no project opened.
await send("Page.navigate", { url: `${adminUrl}#/` });
await until("[...document.querySelectorAll('.tabbar a')].some((tab) => tab.textContent === 'Machine')", "the bottom bar has no Machine tab");
if ((await labels()) !== "Home,Sessions,Machine,Settings") await fail(`the bar with a machine panel is ${await labels()}`);
await evaluate("[...document.querySelectorAll('.tabbar a')].find((tab) => tab.textContent === 'Machine').click()");
await until("document.querySelectorAll('.machine-panel .panel').length === 2 && document.querySelectorAll('.machine-panel .panel-card').length === 2", "the Machine screen shows no panel and no cards");
const machine = await evaluate(`({ names: [...document.querySelectorAll('.machine-name')].map((name) => name.firstChild.textContent + '/' + name.querySelector('small').textContent).join(),
  current: document.querySelector('.tabbar a[aria-current=page]')?.textContent, output: /device run \\d+/.test(document.querySelector('.panel-output')?.textContent ?? ''),
  meter: !!document.querySelector('[role=meter]'), wide: document.documentElement.scrollWidth > innerWidth, hash: location.hash })`);
if (machine.names !== "device/calm,gauges/calm" || machine.current !== "Machine" || !machine.output || !machine.meter || machine.wide || machine.hash !== "#/machine") await fail(`the Machine screen: ${JSON.stringify(machine)}`);
await capture("390x844-machine");
// Four tabs: the pill is four tabs wide, not the screen, and centred; each tab keeps the token's width and a touch target.
const bar = await evaluate(`(() => { const box = document.querySelector('.tabbar-pill').getBoundingClientRect(); const tabs = [...document.querySelectorAll('.tabbar a')].map((tab) => tab.getBoundingClientRect());
  return { pill: box.width, screen: innerWidth, tabs: tabs.map((tab) => Math.round(tab.width * 10) / 10), height: Math.min(...tabs.map((tab) => tab.height)), left: box.left, right: innerWidth - box.right,
    token: parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--size-tab')), control: parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--size-control')) }; })()`);
if (bar.tabs.length !== 4 || bar.tabs.some((width) => Math.abs(width - bar.token) > 0.5) || bar.pill >= bar.screen - 40 || bar.pill - bar.tabs.reduce((sum, width) => sum + width, 0) > 24
  || Math.abs(bar.left - bar.right) > 1 || bar.height < bar.control || bar.token < bar.control) await fail(`the four-tab pill is ${JSON.stringify(bar)}`);
// At most four tabs show, Settings among them, whatever the stored list says; the real function with a fifth tab.
const capped = await evaluate(`(async () => { const store = await import(document.querySelector('script[type=module]').src.replace(/main\\.js$/, 'store.js'));
  const list = ['home', 'inbox', 'machine', 'extra', 'settings'].map((name) => [name, '#/' + name, name, 'home', 0]);
  const was = store.state.hiddenTabs; store.state.hiddenTabs = [];
  const none = store.shownTabs(list).map(([name]) => name).join(); store.state.hiddenTabs = ['inbox'];
  const one = store.shownTabs(list).map(([name]) => name).join(); store.state.hiddenTabs = was;
  return { none, one, max: store.MAX_TABS }; })()`);
if (capped.max !== 4 || capped.none !== "home,inbox,machine,settings" || capped.one !== "home,machine,extra,settings") await fail(`the tab cap gives ${JSON.stringify(capped)}`);
// Settings > UI hides the tab; the screen stays reachable by its link.
await send("Page.navigate", { url: `${adminUrl}#/settings/ui` });
await until("!!document.querySelector('input[name=tab-machine]')", "Settings > UI has no Machine switch");
await evaluate("document.querySelector('input[name=tab-machine]').click()");
await until("[...document.querySelectorAll('.tabbar a')].map((tab) => tab.textContent).join() === 'Home,Sessions,Settings'", "hiding Machine left it in the bar");
await capture("390x844-machine-hidden");
await evaluate("document.querySelector('input[name=tab-machine]').click()");
await until("[...document.querySelectorAll('.tabbar a')].some((tab) => tab.textContent === 'Machine')", "the Machine tab did not return");
// The project screen still lists its machine panels with the others.
await send("Page.navigate", { url: `${adminUrl}#/p/${project}/panels` });
await until("document.querySelectorAll('.panel-tabs [role=tab]').length === 4", "the project's panels page lost a panel");
await tab("gauges");
await until("document.querySelectorAll('.panel-doc .panel-card').length === 2", "the json panel did not come back on the project's page");
await viewport(1280, 800, 1);
await capture("1280x800-panel-json");
await tab("broken");
await until("/failed \\(exit 2\\)/.test(document.querySelector('.panel-state[data-failed]')?.textContent ?? '')", "the failing panel did not come back");
await capture("1280x800-panel-failed");
const defects = problems.filter((problem) => !/status of 401/.test(problem));
if (defects.length) await fail(`page problems: ${defects.join("; ")}`);
console.log(`panels page: the icon and the Machine tab only with terminal_input, the Machine screen lists both machine panels, Settings > UI hides the tab, output as written (first run ${first}), refreshes by itself, a failing command shows its exit code, a json panel draws cards, bars and level words`);
await cleanup();
process.exit(0);
