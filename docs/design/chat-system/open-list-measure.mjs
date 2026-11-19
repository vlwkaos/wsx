// Task 526: rows of the open list that fit the list's own height (the 40% of the chat the client gives it), before and after, per page and width.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node open-list-measure.mjs   (the pages are made by build.py)
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const page = await launch({ captures: here, prefix: "", timeoutSeconds: 240 });
const read = `(() => { const scroll = document.querySelector('.reservoir-scroll'), top = scroll.getBoundingClientRect().top;
  const rows = [...scroll.querySelectorAll('.need')].map((row) => row.getBoundingClientRect());
  const full = rows.filter((box) => box.bottom - top <= scroll.clientHeight + 0.5).length;
  const actions = [...scroll.querySelectorAll('.row-actions')].map((a) => a.getBoundingClientRect());
  const mains = [...scroll.querySelectorAll('.row-main')].map((a) => a.getBoundingClientRect());
  const hits = [...scroll.querySelectorAll('.need-action')].map((b) => b.getBoundingClientRect());
  const own = rows.filter((box, i) => actions[i] && mains[i] && actions[i].top >= mains[i].bottom - 1).length;
  return { rows: rows.length, fullRows: full, listH: Math.round(scroll.clientHeight), allH: Math.round(rows.reduce((sum, box) => sum + box.height, 0)), avgRowH: Math.round(rows.reduce((sum, box) => sum + box.height, 0) / rows.length),
    actionRowsOnOwnLine: own, minHitW: Math.min(...hits.map((b) => Math.round(b.width)), 999), minHitH: Math.min(...hits.map((b) => Math.round(b.height)), 999) }; })()`;
for (const name of ["foryou", "tasks", "routines"]) {
  for (const [width, height] of [[390, 844], [1280, 800]]) {
    for (const which of ["before", "after"]) {
      await page.viewport(width, height, 1);
      await page.send("Page.navigate", { url: pathToFileURL(path.join(here, `x-open-list-${name}-${which}-dark.html`)).href });
      await page.until("document.readyState === 'complete'", "loads", 10);
      await new Promise((resolve) => setTimeout(resolve, 700));
      const result = await page.send("Runtime.evaluate", { expression: read, returnByValue: true });
      console.log(name.padEnd(9), String(width).padEnd(5), which.padEnd(7), JSON.stringify(result.result?.value ?? result));
    }
  }
}
await page.cleanup();
