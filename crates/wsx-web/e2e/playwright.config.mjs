// The web client's browser tests (README.md). Each test owns one isolated wsx: its wsxd,
// `wsx web serve` and the fake agent (fixtures.mjs), and a fresh, newly paired page.
import { defineConfig, devices } from "@playwright/test";
import path from "node:path";

const work = path.resolve(import.meta.dirname, "../../../.work/e2e");

export default defineConfig({
  testDir: "tests",
  outputDir: path.join(work, "results"),
  // ^ A wsx per test: wsxd, the web server and the fake agents' turns are not light.
  workers: process.env.CI ? 2 : 3,
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  // ^ One retry, so a flaky test shows as flaky in the report instead of passing silently.
  retries: 1,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [["list"], ["html", { outputFolder: path.join(work, "report"), open: "never" }]],
  use: {
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  // ^ The two widths the web docs name (docs/web-client.md): a phone with touch and a desktop.
  projects: [
    { name: "phone", use: { ...devices["Pixel 7"], viewport: { width: 390, height: 844 } } },
    { name: "desktop", use: { ...devices["Desktop Chrome"], viewport: { width: 1280, height: 800 } } },
  ],
});
