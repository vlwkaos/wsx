// Task 533: before | after of the real sheet in WebKit (real-webkit.mjs --shots, with and without --rev, both --stale) side by side,
// one picture per state: pair-<state>.png. Usage: PLAYWRIGHT_BROWSERS_PATH=<dir> node pair-webkit.mjs <playwright-core folder>
import fs from "node:fs";
import { createRequire } from "node:module";
import { STATES, here } from "./real-lib.mjs";

const { webkit } = createRequire(import.meta.url)(process.argv[2]);
const browser = await webkit.launch();
const page = await browser.newPage({ viewport: { width: 1000, height: 1000 }, deviceScaleFactor: 1 });
for (const state of STATES) {
  await page.setContent(`<body style="margin:0;background:#222;color:#fff;font:600 28px system-ui"><div style="display:flex;gap:20px;padding:16px">
    ${["before", "after"].map((side) => `<figure style="margin:0"><figcaption style="padding-bottom:8px">${side === "before" ? "Before" : "After"}: ${state}</figcaption>
    <img src="data:image/png;base64,${fs.readFileSync(`${here}/webkit-402-${side}-stale-${state}.png`).toString("base64")}" style="width:402px;display:block"></figure>`).join("")}</div></body>`);
  await page.waitForFunction(() => [...document.images].every((image) => image.complete && image.naturalWidth > 0));
  await page.screenshot({ path: `${here}/pair-${state}.png`, fullPage: true });
}
await browser.close();
