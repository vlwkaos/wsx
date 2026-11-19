// Task 530: measures the dock pages of build.py. One JSON line per page and size; exits 1 when an "after" page fails a check.
//   1. the card (.composer) has a computed background with alpha 1;
//   2. with the For you list open, document.elementFromPoint at the card's centre is inside .composer;
//   3. scrolled to its end, the last message ends above the card;
//   4. no band: the dock and the thread strip have no background and no top border (the strip of task 518 had a surface band on a wide window);
//   5. the whole box is in the window: its bottom is at least 8 px (--space-2) above the window's foot.
// The last page is the 526 mockup with its row menu open (open-list.css lets the list overflow): the baseline that failed (2 is false there).
// The product page is measured by the web check (scripts/web-tasks-check.mjs, "The composer is one card on no band": foot gap, no band, card narrower than the window, at 390 and 1280): a page needs the task fixture and the host's Chrome.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node measure-dock.mjs   (the pages are made by build.py)
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const pages = [];
for (const theme of ["dark", "light"]) {
  for (const name of ["chat-after", "foryou-after", "strip-inside", "strip-above", "strip-list-inside", "strip-list-above"]) pages.push([`x-dock-${name}-${theme}.html`, true]);
}
pages.push(["x-open-list-foryou-menu-after-dark.html", false]);

const page = await launch({ captures: here, prefix: "", timeoutSeconds: 120 });
let failed = 0;
for (const [file, mustPass] of pages) {
  for (const width of [390, 1280]) {
    await page.viewport(width, 844, 1);
    await page.send("Page.navigate", { url: pathToFileURL(path.join(here, file)).href });
    await page.until("document.readyState === 'complete'", `${file} loads`, 10);
    await new Promise((resolve) => setTimeout(resolve, 600));
    const found = JSON.parse(await page.evaluate(`(() => {
      const card = document.querySelector(".composer"), box = card.getBoundingClientRect();
      const color = getComputedStyle(card).backgroundColor, slash = /\\/\\s*([\\d.]+)\\s*\\)/.exec(color), commas = /^rgba\\(.*,\\s*([\\d.]+)\\)$/.exec(color);
      const alpha = slash ? Number(slash[1]) : commas ? Number(commas[1]) : 1;
      const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
      const log = document.querySelector(".chat-body"); log.scrollTop = log.scrollHeight;
      const last = [...log.querySelectorAll(".bubble")].pop().getBoundingClientRect();
      const clear = (n) => !n || (getComputedStyle(n).backgroundColor === "rgba(0, 0, 0, 0)" && getComputedStyle(n).borderTopWidth === "0px");
      const footGap = Math.round(innerHeight - document.querySelector(".composer-row").getBoundingClientRect().bottom);
      return JSON.stringify({ color, alpha, open: !!document.querySelector(".reservoir"), hit: hit.className || hit.tagName, hitInsideComposer: !!hit.closest(".composer"), lastMessageBottom: Math.round(last.bottom), cardTop: Math.round(box.top),
        bandless: clear(document.querySelector(".dock")) && clear(document.querySelector(".threads")), footGap });
    })()`));
    const ok = found.alpha === 1 && found.hitInsideComposer && found.lastMessageBottom <= found.cardTop && found.bandless && found.footGap >= 8;
    console.log(JSON.stringify({ file, width, ...found, ok }));
    if (mustPass && !ok) failed += 1;
  }
}
await page.cleanup();
if (failed) {
  console.error(`FAIL: ${failed} measurements of the dock pages failed`);
  process.exit(1);
}
