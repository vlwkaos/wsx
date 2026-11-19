// Task 519: where the composer's pills fall onto a line of their own. For each width, the page of x-chat-<before|after>-dark.html and
// big counts ("99+"): prints whether the pill group is on the line of the field, the attach row, or alone, and its height.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node pills-measure.mjs
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const page = await launch({ captures: here, prefix: "", timeoutSeconds: 240 });
const read = `(() => { const box = (s) => document.querySelector(s).getBoundingClientRect();
  const attach = box('.composer-row > .icon-button'), pills = box('.pill'), send = box('.composer-send'), card = box('.composer-row'), field = box('.composer-input');
  const shown = [...document.querySelectorAll('.pill-label')].some((n) => getComputedStyle(n).display !== 'none');
  return { ownLine: Math.abs((pills.top + pills.bottom) / 2 - (attach.top + attach.bottom) / 2) > 8, pillsW: Math.round(pills.width), cardW: Math.round(card.width), h: Math.round(card.height), pillH: Math.round(box('.pill').height), labels: shown,
    clipped: [...document.querySelectorAll('.pill')].some((n) => n.getBoundingClientRect().right > send.left + 1), band: getComputedStyle(document.querySelector('.composer')).backgroundColor }; })()`;
for (const which of ["before", "after"]) {
  for (const count of ["8", "99+"]) {
    for (const width of [320, 375, 390, 430, 600, 900, 1000, 1280]) {
      await page.viewport(width, 800, 1);
      await page.send("Page.navigate", { url: pathToFileURL(path.join(here, `x-chat-${which}-dark.html`)).href });
      await page.until("document.readyState === 'complete'", "loads", 10);
      await page.send("Runtime.evaluate", { expression: `document.querySelectorAll('.pill-count').forEach((n) => { n.textContent = ${JSON.stringify(count)}; })` });
      const result = await page.send("Runtime.evaluate", { expression: read, returnByValue: true });
      console.log(which.padEnd(6), count.padEnd(4), String(width).padEnd(5), JSON.stringify(result.result?.value ?? result));
    }
  }
}
await page.cleanup();
