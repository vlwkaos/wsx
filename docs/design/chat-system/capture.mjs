// Shoots the pages of build.py: 390 px light and dark at scale 2, 1280 px dark at scale 1.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node capture.mjs   (the pages are made by build.py)
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const pages = fs.readdirSync(here).filter((name) => /-(light|dark)\.html$/.test(name)).sort();
const page = await launch({ captures: here, prefix: "", timeoutSeconds: 240 });
for (const file of pages) {
  const name = file.replace(/\.html$/, "");
  const sizes = [[390, 844, 2, "390"]];
  if (name.endsWith("-dark")) sizes.push([1280, 800, 1, "1280"]);
  for (const [width, height, scale, tag] of sizes) {
    await page.viewport(width, height, scale);
    await page.send("Page.navigate", { url: pathToFileURL(path.join(here, file)).href });
    await page.until("document.readyState === 'complete'", `${file} loads`, 10);
    await new Promise((resolve) => setTimeout(resolve, 300));
    await page.capture(`${name}-${tag}`);
  }
}
await page.cleanup();
