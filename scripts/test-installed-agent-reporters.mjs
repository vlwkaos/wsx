#!/usr/bin/env node
// Install through the real CLI, then load and exercise the installed JS adapters.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { execFileSync } from "node:child_process";

const wsx = path.resolve(process.argv[2] || "target/debug/wsx");
const work = path.resolve(`.work/installed-reporters-${process.pid}`);
fs.mkdirSync(work, { recursive: true, mode: 0o700 });
let success = false;
try {
  const home = path.join(work, "home");
  for (const directory of [".pi/agent", ".omp/agent", ".config/opencode", ".config/kilo"]) {
    fs.mkdirSync(path.join(home, directory), { recursive: true });
  }
  const log = path.join(work, "reports.jsonl");
  const binary = path.join(work, "current-wsx.mjs");
  fs.writeFileSync(binary, `#!/usr/bin/env node\nimport fs from 'node:fs'; fs.appendFileSync(process.env.WSX_TEST_REPORT_LOG, JSON.stringify(process.argv.slice(2))+'\\n');\n`, { mode: 0o755 });
  fs.symlinkSync(binary, path.join(work, "wsx.reporter"));
  const env = { ...process.env, HOME: home, WSX_SOCKET: path.join(work, "wsx.sock"),
    WSX_AGENT_REPORT_BIN: path.join(work, "removed-0.26.2/wsx"), WSX_PANE_ID: "42",
    WSX_RUNTIME_GENERATION: "fixture-generation", WSX_TEST_REPORT_LOG: log };
  delete env.PI_CODING_AGENT_DIR;
  delete env.PI_CONFIG_DIR;
  for (const provider of ["pi", "omp", "opencode", "kilo"]) {
    execFileSync(wsx, ["agent", "install", provider], { env, timeout: 5000, stdio: "pipe" });
  }
  Object.assign(process.env, env);
  const records = () => fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse) : [];
  const wait = async predicate => {
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline) {
      if (predicate()) return;
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    assert.fail("installed adapter did not publish expected report");
  };
  const load = async relative => import(pathToFileURL(path.join(home, relative)).href);
  for (const [provider, relative] of [["pi", ".pi/agent/extensions/wsx-agent-status.ts"], ["omp", ".omp/agent/extensions/wsx-omp-agent-status.ts"]]) {
    const module = await load(relative);
    const handlers = new Map();
    module.default({ on(name, handler) { handlers.set(name, handler); }, events: { on() {} } });
    const ctx = { hasUI: false, isIdle: () => true, sessionManager: { getSessionFile: () => path.join(work, `${provider}.jsonl`) } };
    await handlers.get("session_start")({}, ctx);
    await wait(() => records().some(args => args.includes(provider) && args.includes("idle")));
    await handlers.get("session_shutdown")({}, ctx);
    assert(records().some(args => args.includes(provider) && args.includes("--detached")));
  }
  for (const [provider, relative] of [["opencode", ".config/opencode/plugins/wsx-agent-status.js"], ["kilo", ".config/kilo/plugin/wsx-agent-status.js"]]) {
    const module = await load(relative);
    const hooks = await module.WsxAgentStatusPlugin();
    await hooks["chat.message"]({ sessionID: `${provider}-fixture` });
    await wait(() => records().some(args => args.includes(provider) && args.includes("working")));
  }
  const tui = await load(".config/opencode/wsx-tui-session.js");
  await tui.default.tui({ route: { current: { name: "session", params: { sessionID: "selected-fixture" } } }, state: { session: { get: () => ({}) } } });
  await wait(() => records().some(args => args.includes("opencode") && args.includes("selected-fixture")));
  success = true;
  console.log("installed Pi/OMP/OpenCode/Kilo adapters and selected-session TUI resolve their shared helper and recover removed reporters PASS");
} finally {
  if (success) fs.rmSync(work, { recursive: true, force: true });
  else console.error(`Installed adapter diagnostics retained at ${work}`);
}
