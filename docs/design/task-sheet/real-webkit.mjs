// Task 533: the REAL task sheet in WebKit with the owner's phone emulated, before (--rev) and after (the working copy).
// The phone: the owner's screenshot is 920 x 2000 px, ratio 0.4600 = 402 x 874 css px (iPhone 16/17 Pro, DPR 3; 393 x 852 would be
// 0.4613) in the home-screen app (manifest "display": "standalone", viewport-fit=cover); the status bar ends at about 60 css px
// (--top, 62 here), the home indicator takes 34. --phone WxH sets another size. Playwright's
// WebKit has no safe areas, so the served app.css gets those numbers where it reads env(safe-area-inset-*) (real-lib.mjs).
// Usage: PLAYWRIGHT_BROWSERS_PATH=<dir> node real-webkit.mjs <playwright-core folder> [--rev <commit>] [--shots]
// Prints, per state: the sheet's top and height, the first line's top against the scroll box's top and the header's foot, the
// scroll offset, the buttons and the clipped ones. --shots writes webkit-<before|after>-<state>.png.
import { createRequire } from "node:module";
import { STATES, here, measure, serve } from "./real-lib.mjs";

const args = process.argv.slice(2);
const arg = (name) => { const at = args.indexOf(name); return at < 0 ? null : args[at + 1] ?? true; };
const { webkit, devices } = createRequire(import.meta.url)(args[0]);
const state_names = args.includes("--checks") ? ["checks"] : STATES;
const [phoneWidth, phoneHeight] = (arg("--phone") || "402x874").split("x").map(Number), top = Number(arg("--top") || 62);
const rev = arg("--rev"), shots = args.includes("--shots");
const { origin, close } = await serve({ rev, inset: { top, bottom: 34 } });
const browser = await webkit.launch();
const rows = [], sweep = [];
try {
  const context = await browser.newContext({ ...devices["iPhone 15"], viewport: { width: phoneWidth, height: phoneHeight }, screen: { width: phoneWidth, height: phoneHeight }, colorScheme: "dark" });
  const page = await context.newPage();
  for (const state of state_names) {
    await page.goto(`${origin}/harness.html?state=${state}`);
    await page.waitForSelector(".task-sheet .story, .task-sheet .check-list, [data-error]");
    // ^ --stale: the state main.js followViewport leaves while the composer has the focus (typing): the visual viewport's height
    // and offset in two custom properties, which app.css reads for the sheet (.task-sheet) and the page (body).
    if (args.includes("--stale")) await page.evaluate(([height, offset]) => { document.documentElement.style.setProperty("--viewport-height", `${height}px`); document.documentElement.style.setProperty("--viewport-top", `${offset}px`); }, [phoneHeight - top, top]);
    await page.waitForTimeout(1200);
    const row = JSON.parse(await page.evaluate(measure));
    rows.push({ state, ...row });
    if (shots) await page.screenshot({ path: `${here}/webkit-${phoneWidth}-${rev ? "before" : "after"}${args.includes("--stale") ? "-stale" : ""}-${state}.png` });
  }
  // ^ The wrap threshold of the actions row: every css width from 340 to 430 px, ready and owner states.
  if (args.includes("--sweep")) {
    for (const state of ["ready", "owner"]) {
      for (let width = 340; width <= 430; width += 1) {
        await page.setViewportSize({ width, height: phoneHeight });
        await page.goto(`${origin}/harness.html?state=${state}`);
        await page.waitForSelector(".task-sheet .story, [data-error]");
        await page.waitForTimeout(400);
        const row = JSON.parse(await page.evaluate(measure));
        const beside = await page.evaluate(() => { const box = document.querySelector(".task-next")?.getBoundingClientRect(), text = document.querySelector(".story-act > .story-next")?.getBoundingClientRect(); return Boolean(text && box && text.top < box.bottom && box.top < text.bottom); });
        sweep.push({ state, width, beside, ...row });
      }
    }
    const bad = sweep.filter((row) => row.clipped.length || row.covered);
    console.log(`sweep 340-430: ${sweep.length} pages, sentence beside the buttons on ${sweep.filter((row) => row.beside).map((row) => `${row.state} ${row.width}`).join(", ") || "none"}; clipped or covered: ${bad.map((row) => `${row.state} ${row.width} ${JSON.stringify(row.clipped)}`).join(", ") || "none"}`);
  }
  console.log(`${rev ? `at ${rev}` : "working copy"}: WebKit ${browser.version()}, ${`${phoneWidth}x${phoneHeight} (home-screen app: no Safari bars), top inset ${top}`} DPR ${devices["iPhone 15"].deviceScaleFactor}`);
  for (const row of rows) console.log(`  ${row.state}: sheetTop=${row.sheetTop} sheetHeight=${row.sheetHeight} barFoot=${row.barFoot} scrollTop=${row.scrollTop} lineTop=${row.lineTop} scrolled=${row.scrolled} covered=${row.covered} tools=${JSON.stringify(row.tools)} buttons=${JSON.stringify(row.buttons)} clipped=${JSON.stringify(row.clipped)}`);
} finally {
  await browser.close();
  close();
}
