import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

const workDir = path.resolve(`.work/omp-agent-status-test-${process.pid}`);
const reportBin = path.join(workDir, "fake-wsx.mjs");
const reportLog = path.join(workDir, "reports.jsonl");
fs.mkdirSync(workDir, { recursive: true });
fs.writeFileSync(reportBin, `#!/usr/bin/env node
import fs from "node:fs";
fs.appendFileSync(process.env.WSX_TEST_REPORT_LOG, JSON.stringify(process.argv.slice(2)) + "\\n");
`);
fs.chmodSync(reportBin, 0o755);
process.env.WSX_PANE_ID = "982";
process.env.WSX_AGENT_REPORT_BIN = reportBin;
process.env.WSX_TEST_REPORT_LOG = reportLog;
const source = new URL("../crates/wsx-core/integrations/omp/wsx-agent-status.ts", import.meta.url);
const { default: wsxOmpAgentStatus } = await import(`${source.href}?test=${Date.now()}`);
const handlers = new Map();
const pi = { on(name, handler) { handlers.set(name, handler); } };
const emit = async (name, ctx) => handlers.get(name)?.({}, ctx);
const commands = () => {
  try {
    return fs.readFileSync(reportLog, "utf8").trim().split("\n")
      .filter(Boolean).map((line) => JSON.parse(line));
  } catch {
    return [];
  }
};
const waitFor = async (predicate, label, timeoutMs = 2_000) => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail(`${label}: ${JSON.stringify(commands())}`);
};
const ctx = { sessionManager: { getSessionId: () => "session-id" } };
wsxOmpAgentStatus(pi);
let shutdownComplete = false;
try {
  await emit("session_start", ctx);
  await waitFor(() => commands().some((args) => args.includes("--state") && args.includes("idle")),
    "idle report");
  const initial = commands().find((args) => args[1] === "report");
  const presenceId = initial[initial.indexOf("--presence-id") + 1];
  assert.match(presenceId, /^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i);
  await waitFor(() => commands().some((args) =>
    args[1] === "presence-renew" && args.at(-1) === presenceId),
  "idle renewal", 12_000);
  assert.equal(commands().filter((args) => args[1] === "report").length, 1,
    "renewal must not publish a state report");
  await emit("session_shutdown", ctx);
  shutdownComplete = true;
  await waitFor(() => commands().some((args) => args.includes("--detached")), "shutdown detach");
  const detached = commands().find((args) => args.includes("--detached"));
  assert.equal(detached.includes("--presence-id"), false);
} finally {
  if (!shutdownComplete) await emit("session_shutdown", ctx);
  fs.rmSync(workDir, { recursive: true, force: true });
}
console.log("OMP agent presence lifecycle passed");
