#!/usr/bin/env node
// Draws the bytes a terminal program wrote as a PNG: xterm.js (the web client's own copy) in
// headless Chrome, so a TUI scenario can leave a capture of its screen.
//   render-ansi.mjs <bytes file> <capture dir> <name> <columns> <rows>
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { launch } from "./web-cdp.mjs";

const [bytesFile, captures, name, columns, rows] = process.argv.slice(2);
const vendor = path.resolve(path.dirname(new URL(import.meta.url).pathname), "../crates/wsx-web/web/vendor");
fs.mkdirSync(captures, { recursive: true });
const page = path.join(captures, `${name}.html`);
fs.writeFileSync(page, `<!doctype html><meta charset="utf-8"><link rel="stylesheet" href="${pathToFileURL(path.join(vendor, "xterm.css"))}">
<body style="margin:0;background:#111"><div id="screen" style="display:inline-block;padding:8px"></div>
<script src="${pathToFileURL(path.join(vendor, "xterm.js"))}"></script>
<script>
  const term = new Terminal({ cols: ${Number(columns)}, rows: ${Number(rows)}, fontSize: 14, fontFamily: "Menlo, monospace", cursorBlink: false });
  term.open(document.getElementById("screen"));
  const bytes = Uint8Array.from(atob("${fs.readFileSync(bytesFile).toString("base64")}"), (c) => c.charCodeAt(0));
  term.write(bytes, () => { window.drawn = true; });
</script>`);
const { send, evaluate, until, capture, cleanup } = await launch({ captures, prefix: "", timeoutSeconds: 60 });
await send("Page.navigate", { url: pathToFileURL(page).href });
await until("window.drawn === true", "the bytes were not drawn");
const box = await evaluate("(() => { const r = document.getElementById('screen').getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height, scale: 1 }; })()");
await capture(name, box);
fs.rmSync(page);
await cleanup();
process.exit(0);
