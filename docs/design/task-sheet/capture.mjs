// Task 533: shoots the before/after pairs of build.py (sheet-*.html, two iframes side by side) to <name>.png, and measures every x-sheet-* page
// at 390 and 1280 px: per page the buttons, the buttons whose text is clipped (scrollWidth > clientWidth) and whether the first content line is
// under the header (its centre is not what document.elementFromPoint returns, or its top is above the header's foot). Writes measure.json,
// prints the counts and exits 1 when an "after" page has a clipped button or a covered line.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node capture.mjs [name prefix]   (the pages are made by build.py)
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const only = process.argv[2] ?? "";
const pairs = fs.readdirSync(here).filter((name) => /^sheet-.*-(390|1280)\.html$/.test(name) && name.startsWith(only)).sort();
const singles = fs.readdirSync(here).filter((name) => /^x-sheet-.*\.html$/.test(name)).sort();
const page = await launch({ captures: here, prefix: "", timeoutSeconds: 300 });
const open = async (file, width, height, scale) => {
  await page.viewport(width, height, scale);
  await page.send("Page.navigate", { url: pathToFileURL(path.join(here, file)).href });
  await page.until("document.readyState === 'complete'", `${file} loads`, 10);
  await new Promise((resolve) => setTimeout(resolve, 400));
};
for (const file of pairs) {
  const width = Number(/-(390|1280)\.html$/.exec(file)[1]);
  await open(file, 2 * width + 50, width === 1280 ? 850 : 920, width === 1280 ? 1 : 2);
  await page.capture(file.replace(/\.html$/, ""));
}
const measure = `(() => {
  const sheet = document.querySelector(".task-sheet"), scroll = sheet.querySelector(".scroll"), bar = sheet.querySelector(".bar");
  const buttons = [...sheet.querySelectorAll("button")];
  const clipped = buttons.filter((button) => button.scrollWidth > button.clientWidth).map((button) => button.textContent.trim());
  const walker = document.createTreeWalker(scroll.querySelector(".story"), NodeFilter.SHOW_TEXT, { acceptNode: (node) => node.textContent.trim() ? 1 : 3 });
  const range = document.createRange();
  range.selectNodeContents(walker.nextNode());
  const line = range.getClientRects()[0], hit = document.elementFromPoint(line.left + line.width / 2, line.top + line.height / 2);
  const under = bar.getBoundingClientRect().bottom;
  return JSON.stringify({ buttons: buttons.length, clipped, covered: !scroll.contains(hit) || line.top < under - 0.5, lineTop: Math.round(line.top), barFoot: Math.round(under), hit: hit?.className });
})()`;
const rows = [];
for (const file of singles) {
  for (const width of [390, 1280]) {
    await open(file, width, 844, 1);
    rows.push({ file, width, ...JSON.parse(await page.evaluate(measure)) });
  }
}
// The phone widths between a small and a large phone (320 to 430 px, every 5 px): where does a button of a task with actions clip?
const sweep = [];
for (const side of ["before", "after"]) {
  for (const width of Array.from({ length: 23 }, (_, index) => 320 + index * 5)) {
    await open(`x-sheet-owner-${side}-dark.html`, width, 844, 1);
    const found = JSON.parse(await page.evaluate(measure));
    sweep.push({ side, width, clipped: found.clipped });
  }
}
await page.cleanup();
fs.writeFileSync(path.join(here, "sweep.json"), JSON.stringify(sweep));
for (const side of ["before", "after"]) {
  const bad = sweep.filter((row) => row.side === side && row.clipped.length);
  console.log(`sweep ${side} (owner state, 3 buttons): ${bad.length} of 23 widths clip a button${bad.length ? `: ${bad.map((row) => `${row.width} ${JSON.stringify(row.clipped)}`).join("; ")}` : ""}`);
  if (side === "after" && bad.length) process.exitCode = 1;
}
fs.writeFileSync(path.join(here, "measure.json"), JSON.stringify(rows, null, 1));
let failed = 0;
for (const side of ["before", "after"]) {
  const set = rows.filter((row) => row.file.includes(`-${side}-`));
  const clipped = set.filter((row) => row.clipped.length), covered = set.filter((row) => row.covered);
  console.log(`${side}: ${set.length} pages, ${set.reduce((sum, row) => sum + row.buttons, 0)} buttons, ${clipped.length} pages with a clipped button, ${covered.length} pages with the first line under the header`);
  for (const row of [...clipped, ...covered]) console.log(`  ${row.file} @${row.width}: clipped=${JSON.stringify(row.clipped)} covered=${row.covered} lineTop=${row.lineTop} barFoot=${row.barFoot}`);
  if (side === "after") failed = clipped.length + covered.length;
}
process.exit(failed ? 1 : 0);
