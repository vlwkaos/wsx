// Deterministic SDK worker for the native exchange consumer journey. No network or model API calls.
import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { StringDecoder } from "node:string_decoder";
import { pathToFileURL } from "node:url";

const [sdkRoot, adapterPath, goalModule, evidenceRoot, mode = "task"] = process.argv.slice(2);
assert(sdkRoot && adapterPath && goalModule && evidenceRoot);
const sdk = await import(pathToFileURL(join(sdkRoot, "dist/index.js")));
const { createAssistantMessageEventStream } = await import(pathToFileURL(
  join(sdkRoot, "node_modules/@earendil-works/pi-ai/dist/utils/event-stream.js")));
const require = createRequire(join(sdkRoot, "package.json"));
const { createJiti } = require("jiti");
const jiti = createJiti(import.meta.url, { moduleCache: false, fsCache: false });
const status = await jiti.import(resolve(adapterPath), { default: true });
const { registerGoalRunService } = await jiti.import(resolve(goalModule));
const state = { turns: 0, completedTurns: 0, settled: 0, inputs: 0, ready: false, mode, sdkVersion: require(join(sdkRoot, "package.json")).version };
const save = () => writeFileSync(join(evidenceRoot, "sdk-state.json"), JSON.stringify(state) + "\n");
let goal = { active: false, taskId: undefined, branchLineage: "native-fixture", taskVersion: 0,
  iteration: 0, continuations: 0, criteria: [] };
let producerApi;
const modelRuntime = await sdk.ModelRuntime.create({
  authPath: join(evidenceRoot, "auth.json"), modelsPath: null,
  modelsStorePath: join(evidenceRoot, "model-cache.json"), refreshOnCreate: false, allowModelNetwork: false,
});
modelRuntime.registerProvider("wsx-native-fixture", {
  api: "wsx-native-fixture", apiKey: "local-fixture", baseUrl: "http://127.0.0.1:1",
  models: [{ id: "fixture", name: "Offline fixture", reasoning: false, input: ["text"],
    contextWindow: 32000, maxTokens: 1000, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
  streamSimple(model, _context, options) {
    const stream = createAssistantMessageEventStream();
    const message = { role: "assistant", api: model.api, provider: model.provider, model: model.id,
      timestamp: Date.now(), content: [], stopReason: "pending", usage: { input: 0, output: 0,
        cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } } };
    void (async () => {
      state.turns += 1;
      save();
      stream.push({ type: "start", partial: message });
      await new Promise((resolve) => setTimeout(resolve, mode === "abort" ? 2000 : 40));
      if (options?.signal?.aborted) {
        message.stopReason = "aborted";
        message.errorMessage = "fixture abort";
        stream.push({ type: "error", reason: "aborted", error: message });
      } else {
        message.content.push({ type: "text", text: "" });
        stream.push({ type: "text_start", contentIndex: 0, partial: message });
        message.content[0].text = "fixture result";
        stream.push({ type: "text_delta", contentIndex: 0, delta: "fixture result", partial: message });
        stream.push({ type: "text_end", contentIndex: 0, content: "fixture result", partial: message });
        message.stopReason = "stop";
        state.completedTurns += 1;
        state.lastModelStopMs = Date.now();
        save();
        stream.push({ type: "done", reason: "stop", message });
      }
      stream.end();
    })().catch((error) => { console.error(error); process.exitCode = 1; stream.end(); });
    return stream;
  },
});
const settingsManager = sdk.SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: false } });
const producer = (pi) => {
  producerApi = pi;
  if (mode !== "no-goal") registerGoalRunService(pi.events, { get: () => structuredClone(goal) });
  pi.on("agent_start", () => {
    if ((mode === "task" || mode === "paused" || mode === "dispose") && state.turns === 0) {
      goal = { ...goal, active: true, taskId: "fixture-task", taskVersion: 1,
        criteria: [{ id: "result", label: "native result", status: "pending" }] };
      pi.events.emit("pygmalion:goal-run-changed", { version: 1, title: "not completion authority" });
    }
  });
  pi.on("agent_settled", () => {
    state.settled += 1;
    save();
    if (mode === "task" && state.settled === 1) {
      setTimeout(() => {
        goal.continuations += 1;
        pi.sendUserMessage("Continue the same native Task", { deliverAs: "followUp", triggerTurn: true });
      }, 800);
    } else if (mode === "task" && state.settled >= 2 && goal.continuations >= 1) {
      goal = { ...goal, active: false, taskVersion: 2, completionTransactionId: "fixture-completion",
        criteria: [{ id: "result", label: "native result", status: "passed" }] };
      pi.events.emit("pygmalion:goal-run-changed", { version: 2, title: "still not authority" });
    } else if (mode === "paused") {
      goal.pausedReason = "blocked";
    }
  });
};
const loader = new sdk.DefaultResourceLoader({ cwd: process.cwd(), agentDir: join(evidenceRoot, "agent"),
  settingsManager, noExtensions: true, noSkills: true, noPromptTemplates: true, noThemes: true, noContextFiles: true,
  extensionFactories: [producer, status], systemPrompt: "Deterministic offline native adapter fixture." });
await loader.reload();
assert.equal(loader.getExtensions().errors.length, 0, JSON.stringify(loader.getExtensions().errors));
const { session } = await sdk.createAgentSession({ cwd: process.cwd(), agentDir: join(evidenceRoot, "agent"),
  modelRuntime, model: modelRuntime.getModel("wsx-native-fixture", "fixture"), thinkingLevel: "off",
  resourceLoader: loader, sessionManager: sdk.SessionManager.create(process.cwd(), join(evidenceRoot, "sessions")),
  settingsManager, noTools: "all" });
await session.bindExtensions({});
state.ready = true;
save();
process.stdout.write("\x1b[?2004hnative fixture ready\r\n");
process.stdin.setRawMode(true);
process.stdin.resume();
const decoder = new StringDecoder("utf8");
let input = "";
process.stdin.on("data", (bytes) => {
  input += decoder.write(bytes);
  for (;;) {
    const begin = input.indexOf("\x1b[200~");
    let text;
    if (begin < 0) {
      // ^ session send-text is literal input; exchange delivery is a bracketed paste.
      const line = /^([^\r\n]*)[\r\n]/.exec(input);
      if (!line) break;
      text = line[1];
      input = input.slice(line[0].length);
      if (!text) continue;
    } else {
      const end = input.indexOf("\x1b[201~", begin + 6);
      if (end < 0) break;
      text = input.slice(begin + 6, end);
      input = input.slice(end + 6).replace(/^[\r\n]+/, "");
    }
    state.inputs += 1;
    save();
    if (text === "fixture:abort") { void session.abort(); continue; }
    if (text === "fixture:dispose") { session.dispose(); goal.active = false; goal.completionTransactionId = "late-disposal"; goal.criteria = [{ status: "passed" }]; continue; }
    if (text === "fixture:generation") { process.env.WSX_RUNTIME_GENERATION = "replaced"; continue; }
    if (text === "fixture:steer") {
      state.steeringGoalActive = goal.active;
      save();
      if (goal.active) void session.prompt("Compatible steering", { streamingBehavior: "steer" });
      continue;
    }
    if (text === "fixture:finish") {
      goal = { ...goal, active: false, pausedReason: undefined, completionTransactionId: "fixture-completion",
        criteria: [{ id: "result", label: "native result", status: "passed" }] };
      producerApi.events.emit("pygmalion:goal-run-changed", { version: 10, title: "not completion authority" });
      continue;
    }
    if (text === "fixture:replacement") {
      goal = { ...goal, taskId: "replacement-task", taskVersion: 9 };
      producerApi.events.emit("pygmalion:goal-run-changed", { version: 9, title: "replacement" });
      continue;
    }
    const prompt = mode === "mismatch" ? text + " changed" : text;
    void session.prompt(prompt).catch((error) => { console.error(error.message); state.error = error.message; save(); });
  }
});
process.on("SIGTERM", () => { session.dispose(); process.exit(0); });
