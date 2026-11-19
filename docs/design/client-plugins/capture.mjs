// Shoots the pages of build.py: 390 px at scale 2, 1280 px at scale 1.
// Usage: CHROME_BIN=<chrome with --no-sandbox> node capture.mjs [name prefix]   (the pages are made by build.py)
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { launch } from "../../../scripts/web-cdp.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const only = process.argv[2] ?? "";
const pages = fs.readdirSync(here).filter((name) => /-(light|dark)-(390|1280)\.html$/.test(name) && name.startsWith(only)).sort();
const page = await launch({ captures: here, prefix: "", timeoutSeconds: 240 });
for (const file of pages) {
  const name = file.replace(/\.html$/, "");
  const wide = name.endsWith("-1280");
  await page.viewport(wide ? 1280 : 390, wide ? 800 : 844, wide ? 1 : 2);
  await page.send("Page.navigate", { url: pathToFileURL(path.join(here, file)).href });
  await page.until("document.readyState === 'complete'", `${file} loads`, 10);
  await new Promise((resolve) => setTimeout(resolve, 400));
  await page.capture(name);
}
await page.cleanup();
