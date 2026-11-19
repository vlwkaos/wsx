// Shoots the pages of build.py: 390 px light and dark at scale 2, 1280 px dark at scale 1.
// The composer-* pages (task 512) are shot at 1280 and 1440 px, light and dark, at scale 1.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node capture.mjs [name prefix]   (the pages are made by build.py)
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const only = process.argv[2] ?? "";
// round-* (task 519) and open-list-* (task 526) are before/after pairs of iframes onto the x-* pages; the x-* pages are not shot alone.
const pages = fs.readdirSync(here).filter((name) => (/-(light|dark)\.html$/.test(name) || /^(round|open-list)-.*-(320|390|1280)\.html$/.test(name)) && !name.startsWith("x-") && name.startsWith(only)).sort();
const page = await launch({ captures: here, prefix: "", timeoutSeconds: 240 });
for (const file of pages) {
  const name = file.replace(/\.html$/, "");
  const pair = /^(round|open-list)-.*-(320|390|1280)$/.exec(name);
  if (pair) {
    const wide = pair[2] === "1280";
    const small = pair[2] === "320";
    await page.viewport(wide ? 2610 : small ? 690 : 830, wide ? 850 : small ? 820 : 920, wide ? 1 : 2);
    await page.send("Page.navigate", { url: pathToFileURL(path.join(here, file)).href });
    await page.until("document.readyState === 'complete'", `${file} loads`, 10);
    await new Promise((resolve) => setTimeout(resolve, 600));
    await page.capture(name);
    continue;
  }
  const sizes = name.startsWith("composer-") ? [[1280, 800, 1, "1280"], [1440, 900, 1, "1440"]] : [[390, 844, 2, "390"]];
  if (!name.startsWith("composer-") && name.endsWith("-dark")) sizes.push([1280, 800, 1, "1280"]);
  for (const [width, height, scale, tag] of sizes) {
    await page.viewport(width, height, scale);
    await page.send("Page.navigate", { url: pathToFileURL(path.join(here, file)).href });
    await page.until("document.readyState === 'complete'", `${file} loads`, 10);
    await new Promise((resolve) => setTimeout(resolve, 300));
    await page.capture(`${name}-${tag}`);
  }
}
await page.cleanup();
