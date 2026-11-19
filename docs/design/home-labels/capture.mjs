// Shoots the pages of build.py: home-* at 390 px (scale 2) and 1280 px (scale 1), light and dark; picker-* and move-* at 390 px dark.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node capture.mjs [name prefix]   (the pages are made by build.py)
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const only = process.argv[2] ?? "";
const pages = fs.readdirSync(here).filter((name) => /^(home|picker|move)-.*-(light|dark)\.html$/.test(name) && name.startsWith(only)).sort();
const page = await launch({ captures: here, prefix: "", timeoutSeconds: 240 });
for (const file of pages) {
  const name = file.replace(/\.html$/, "");
  const sizes = [[390, 844, 2, "390"]];
  if (name.startsWith("home-")) sizes.push([1280, 800, 1, "1280"]);
  for (const [width, height, scale, tag] of sizes) {
    await page.viewport(width, height, scale);
    await page.send("Page.navigate", { url: pathToFileURL(path.join(here, file)).href });
    await page.until("document.readyState === 'complete'", `${file} loads`, 10);
    await new Promise((resolve) => setTimeout(resolve, 300));
    await page.capture(`${name}-${tag}`);
  }
}
await page.cleanup();
