// Task 533: the REAL task sheet (crates/wsx-web/web: task.js, story.js, app.css, the default theme's tokens) in headless Chrome,
// fed with the shape of `GET api/tasks/<project>/t/<n>` for four states (ready, working, waiting on the owner, landed).
// Only the answer of that one route is canned (fetch is replaced in the page); every line of the sheet is the product's.
// Measures per state, width (390, 1280) and scheme: buttons, buttons whose text is clipped (scrollWidth > clientWidth), and whether the
// first content line is under the header (its centre is not what elementFromPoint returns, or its top is above the header's foot).
// Usage: CHROME_BIN=<chrome with --no-sandbox> node real.mjs [--rev <commit>] [--shots]
//   --rev   serve crates/wsx-web/web as it was at <commit> (git archive), to measure the sheet before the change
//   --shots write real-<state>-<scheme>-<width>.png here (real-before-... with --rev)
// Exits 1 when a clipped button or a covered first line is found.
import { launch } from "../../../scripts/web-cdp.mjs";
import { STATES, here, measure, serve } from "./real-lib.mjs";

const arg = (name) => { const at = process.argv.indexOf(name); return at < 0 ? null : process.argv[at + 1] ?? true; };
const rev = arg("--rev"), shots = process.argv.includes("--shots");
const { origin, close } = await serve({ rev });

const page = await launch({ captures: here, prefix: "", timeoutSeconds: 300 });
const rows = [], sweep = [];
try {
  for (const state of STATES) {
    for (const scheme of ["dark", "light"]) {
      for (const width of [390, 1280]) {
        await page.viewport(width, 844, width === 1280 ? 1 : 2);
        await page.send("Page.navigate", { url: `${origin}/harness.html?state=${state}${width === 1280 ? "&wide=1" : ""}` });
        await page.until("document.readyState === 'complete' && !!document.querySelector('.task-sheet .story, [data-error]')", `${state} sheet renders`, 15);
        const failure = await page.evaluate("document.body.dataset.error ?? ''");
        if (failure) throw new Error(failure);
        if (scheme === "light") await page.evaluate("document.documentElement.dataset.scheme = 'light'");
        await new Promise((resolve) => setTimeout(resolve, 1200));
        rows.push({ state, scheme, width, ...JSON.parse(await page.evaluate(measure)) });
        if (shots) await page.capture(`real-${rev ? "before-" : ""}${state}-${scheme}-${width}`);
      }
    }
  }
  // ^ Every phone width from 320 to 430 px, every 5 px, for the two states with the most buttons.
  for (const state of ["ready", "owner"]) {
    for (let width = 320; width <= 430; width += 5) {
      await page.viewport(width, 844, 2);
      await page.send("Page.navigate", { url: `${origin}/harness.html?state=${state}` });
      await page.until("!!document.querySelector('.task-sheet .story')", `${state} sheet renders at ${width}`, 15);
      await new Promise((resolve) => setTimeout(resolve, 600));
      sweep.push({ state, width, ...JSON.parse(await page.evaluate(measure)) });
    }
  }
} finally {
  await page.cleanup();
  close();
}
const clipped = rows.filter((row) => row.clipped.length), covered = rows.filter((row) => row.covered), sideways = rows.filter((row) => row.sideways), scrolled = rows.filter((row) => row.scrolled), narrow = sweep.filter((row) => row.clipped.length || row.covered);
console.log(`${rev ? `at ${rev}` : "working copy"}: ${rows.length} pages, ${rows.reduce((sum, row) => sum + row.buttons.length, 0)} buttons, ${clipped.length} pages with a clipped button, ${covered.length} with the first line under the header, ${sideways.length} with sideways scroll, ${scrolled.length} opened scrolled; sweep 320-430 px: ${sweep.length} pages, ${narrow.length} with a clipped button or a covered line${narrow.length ? ` (${narrow.map((row) => `${row.state} ${row.width}`).join(", ")})` : ""}`);
for (const row of rows) console.log(`  ${row.state} ${row.scheme} ${row.width}: buttons=${JSON.stringify(row.buttons)} clipped=${JSON.stringify(row.clipped)} covered=${row.covered} first=${JSON.stringify(row.first)}`);
process.exit(clipped.length + covered.length + sideways.length + scrolled.length + narrow.length ? 1 : 0);
