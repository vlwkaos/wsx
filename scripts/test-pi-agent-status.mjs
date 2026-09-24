import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

const workDir = path.resolve(`.work/pi-agent-status-test-${process.pid}`);
const reportBin = path.join(workDir, "fake-wsx.mjs");
const reportLog = path.join(workDir, "reports.jsonl");
const failureCount = path.join(workDir, "failures");
fs.mkdirSync(workDir, { recursive: true });
fs.writeFileSync(
  reportBin,
  `#!/usr/bin/env node
import fs from "node:fs";
const log = process.env.WSX_TEST_REPORT_LOG;
const failureFile = process.env.WSX_TEST_FAILURE_COUNT;
let failures = 0;
try { failures = Number(fs.readFileSync(failureFile, "utf8")); } catch {}
if (failures > 0) {
  fs.writeFileSync(failureFile, String(failures - 1));
  process.exit(1);
}
fs.appendFileSync(log, JSON.stringify(process.argv.slice(2)) + "\\n");
`,
);
fs.chmodSync(reportBin, 0o755);
process.env.WSX_PANE_ID = "982";
process.env.WSX_AGENT_REPORT_BIN = reportBin;
process.env.WSX_TEST_REPORT_LOG = reportLog;
process.env.WSX_TEST_FAILURE_COUNT = failureCount;

const source = new URL("../crates/wsx-core/integrations/pi/wsx-agent-status.ts", import.meta.url);
const { default: wsxAgentStatus, observeBlockingUi } = await import(
  `${source.href}?test=${Date.now()}`
);

const deferred = () => {
  let resolve;
  const promise = new Promise((onResolve) => {
    resolve = onResolve;
  });
  return { promise, resolve };
};

const confirm = deferred();
const select = deferred();
const originals = {
  select: () => select.promise,
  confirm: () => confirm.promise,
  input: async () => { throw new Error("input failed"); },
  custom: () => { throw new Error("custom failed"); },
  editor: async () => "edited",
};
const ui = { ...originals, notify() {} };
const deltas = [];
const restore = observeBlockingUi(ui, (delta) => deltas.push(delta));

const confirming = ui.confirm("Confirm", "Continue?");
const selecting = ui.select("Select", ["one"]);
assert.deepEqual(deltas, [1, 1]);
confirm.resolve(true);
assert.equal(await confirming, true);
assert.deepEqual(deltas, [1, 1, -1]);
select.resolve(undefined);
assert.equal(await selecting, undefined);
assert.deepEqual(deltas, [1, 1, -1, -1]);

await assert.rejects(ui.input("Input"), /input failed/);
assert.deepEqual(deltas.slice(-2), [1, -1]);
assert.throws(() => ui.custom(() => {}), /custom failed/);
assert.deepEqual(deltas.slice(-2), [1, -1]);
assert.equal(await ui.editor("Editor"), "edited");
assert.deepEqual(deltas.slice(-2), [1, -1]);

const pending = deferred();
const pendingOriginal = () => pending.promise;
const pendingUi = { ...originals, confirm: pendingOriginal };
const pendingDeltas = [];
const restorePending = observeBlockingUi(pendingUi, (delta) => pendingDeltas.push(delta));
const pendingConfirmation = pendingUi.confirm("Confirm", "Continue?");
assert.deepEqual(pendingDeltas, [1]);
restorePending();
assert.equal(pendingUi.confirm, pendingOriginal);
pending.resolve(false);
assert.equal(await pendingConfirmation, false);
assert.deepEqual(pendingDeltas, [1]);

restore();
for (const method of Object.keys(originals)) assert.equal(ui[method], originals[method]);
assert.equal(typeof ui.notify, "function");

const handlers = new Map();
const pi = {
  on(name, handler) {
    const registered = handlers.get(name) ?? [];
    registered.push(handler);
    handlers.set(name, registered);
  },
  events: { on() {} },
};
const emit = async (name, ...args) => {
  for (const handler of handlers.get(name) ?? []) await handler(...args);
};
const commands = () => {
  try {
    return fs.readFileSync(reportLog, "utf8").trim().split("\n")
      .filter(Boolean).map((line) => JSON.parse(line));
  } catch {
    return [];
  }
};
const states = () => commands().filter((args) => args[1] === "report")
  .map((args) => args[args.indexOf("--state") + 1]);
const waitFor = async (predicate, message, timeoutMs = 2_000) => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.fail(`${message}: ${JSON.stringify(states())}`);
};

let idle = true;
const ctx = {
  hasUI: false,
  isIdle: () => idle,
  sessionManager: {
    getSessionFile: () => path.join(workDir, "session.jsonl"),
    getSessionId: () => "session-id",
  },
};
const assistantEnd = (stopReason = "stop") => ({
  messages: [{ role: "assistant", stopReason }],
});

wsxAgentStatus(pi);
let shutdownComplete = false;
try {
  await emit("session_start", {}, ctx);
  await waitFor(() => states().at(-1) === "idle", "session start should report idle");
  const initial = commands().at(-1);
  const presenceId = initial[initial.indexOf("--presence-id") + 1];
  assert.match(presenceId, /^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i);
  await waitFor(() => commands().some((args) =>
    args[1] === "presence-renew" && args.at(-1) === presenceId),
  "idle agent must renew presence", 12_000);
  assert.equal(states().at(-1), "idle", "renewal must not publish a state report");

  await emit("agent_start", {}, ctx);
  await waitFor(() => states().at(-1) === "working", "agent start should report working");
  await emit("agent_end", assistantEnd(), ctx);
  idle = false;
  await emit("agent_settled", {}, ctx);
  await waitFor(
    () => states().at(-1) === "done",
    "post-settlement maintenance must not retain working",
  );

  idle = true;
  const continuationStart = states().length;
  await emit("agent_start", {}, ctx);
  await waitFor(
    () => states().slice(continuationStart).includes("working"),
    "continuation setup should report working",
  );
  await emit("agent_end", assistantEnd(), ctx);
  await emit("agent_settled", {}, ctx);
  await new Promise((resolve) => setTimeout(resolve, 5));
  await emit("agent_start", {}, ctx);
  await new Promise((resolve) => setTimeout(resolve, 75));
  const continuationStates = states().slice(continuationStart);
  assert.equal(continuationStates.at(-1), "working");
  assert.equal(continuationStates.includes("done"), false);

  fs.writeFileSync(failureCount, "1");
  const retryStart = states().length;
  await emit("agent_end", assistantEnd(), ctx);
  await emit("agent_settled", {}, ctx);
  await waitFor(
    () => states().slice(retryStart).includes("done"),
    "a transient report failure should retry the latest Done state",
  );

  fs.writeFileSync(failureCount, "1");
  idle = true;
  await emit("session_shutdown", {}, ctx);
  shutdownComplete = true;
  assert.equal(states().at(-1), "idle");
  assert.equal(commands().at(-1).includes("--presence-id"), false,
    "shutdown must not claim live presence");
} finally {
  if (!shutdownComplete) {
    fs.writeFileSync(failureCount, "0");
    idle = true;
    await emit("session_shutdown", {}, ctx);
  }
  fs.rmSync(workDir, { recursive: true, force: true });
}

console.log("Pi agent status lifecycle passed");
