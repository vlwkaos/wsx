// managed by wsx
// WSX_INTEGRATION_VERSION=20
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { execReporter } from "../common/wsx-reporter.mjs";
import { createHash, randomUUID } from "node:crypto";
import path from "node:path";

const REPORT_TIMEOUT_MS = 1_000;
const PRESENCE_INTERVAL_MS = 10_000;
const presenceId = randomUUID();
const REPORT_RETRY_DELAYS_MS = [100, 500, 2_000] as const;
// ^ A later agent_settled handler may start an automatic continuation. Give its
// agent_start event one turn to invalidate this adapter's stale final report.
const SETTLEMENT_DELAY_MS = 25;
const BLOCKING_UI_METHODS = ["select", "confirm", "input", "custom", "editor"] as const;
const paneId = process.env.WSX_PANE_ID;
const reportBin = process.env.WSX_AGENT_REPORT_BIN || "wsx";
const enabled = typeof paneId === "string" && /^[1-9][0-9]*$/.test(paneId);

type ReportState = "idle" | "working" | "blocked" | "done";
type SessionRef = { id?: string; path?: string };

type PendingReport = {
  state: ReportState;
  sessionRef?: SessionRef;
  attached: boolean;
  retry: number;
  inputBinding: boolean;
};

let sendInFlight = false;
let pending: PendingReport | undefined;
let retryTimer: ReturnType<typeof setTimeout> | undefined;
let drainWaiters: Array<() => void> = [];
let agentActive = false;
let blockedCount = 0;
let lastRunAborted = false;
let currentSessionRef: SessionRef | undefined;
let agentRunGeneration = 0;
let pendingSettlement: ReturnType<typeof setTimeout> | undefined;
let heartbeat: ReturnType<typeof setInterval> | undefined;
let presenceHeartbeat: ReturnType<typeof setInterval> | undefined;
let presenceActive = false;
let presenceInFlight = false;
let inputBindingReady: () => boolean = () => false;

function clearReportRetry(): void {
  if (retryTimer !== undefined) clearTimeout(retryTimer);
  retryTimer = undefined;
}

function report(
  state: ReportState,
  sessionRef = currentSessionRef,
  attached = true,
): void {
  if (!enabled) return;
  clearReportRetry();
  pending = { state, sessionRef, attached, retry: 0, inputBinding: attached && inputBindingReady() };
  drain();
}

function settleDrainWaiters(): void {
  if (sendInFlight || pending || retryTimer !== undefined) return;
  for (const resolve of drainWaiters.splice(0)) resolve();
}

function flushReports(): Promise<void> {
  if (!sendInFlight && !pending && retryTimer === undefined) return Promise.resolve();
  retryTimer?.ref?.();
  return new Promise((resolve) => drainWaiters.push(resolve));
}

function drain(): void {
  if (sendInFlight || retryTimer !== undefined || !pending || !paneId) {
    settleDrainWaiters();
    return;
  }
  const next = pending;
  pending = undefined;
  sendInFlight = true;
  const args = ["agent", "report", paneId, "--provider", "pi", "--state", next.state, "--lifecycle"];
  if (!next.attached) args.push("--detached");
  else args.push("--presence-id", presenceId);
  if (next.inputBinding) args.push("--prompt", "--exchange-receipts");
  if (next.sessionRef?.path) args.push("--session-path", next.sessionRef.path);
  else if (next.sessionRef?.id) args.push("--session-id", next.sessionRef.id);
  execReporter(reportBin, args, { timeout: REPORT_TIMEOUT_MS, windowsHide: true }, (error: Error | null) => {
    sendInFlight = false;
    if (error && !pending && next.retry < REPORT_RETRY_DELAYS_MS.length) {
      const delay = REPORT_RETRY_DELAYS_MS[next.retry];
      pending = { ...next, retry: next.retry + 1 };
      retryTimer = setTimeout(() => {
        retryTimer = undefined;
        drain();
      }, delay);
      if (drainWaiters.length === 0) retryTimer.unref?.();
      return;
    }
    if (error && !pending) {
      console.warn(`wsx: failed to report Pi agent state ${next.state}: ${error.message}`);
    }
    drain();
  });
}

function sessionRef(ctx: unknown): SessionRef | undefined {
  const manager = (ctx as {
    sessionManager?: { getSessionFile?: () => unknown; getSessionId?: () => unknown };
  } | undefined)?.sessionManager;
  try {
    const value = manager?.getSessionFile?.();
    if (typeof value === "string" && path.isAbsolute(value)) return { path: value };
  } catch {}
  try {
    const value = manager?.getSessionId?.();
    if (typeof value === "string" && value) return { id: value };
  } catch {}
  return undefined;
}

function clearPendingSettlement(): void {
  if (pendingSettlement !== undefined) clearTimeout(pendingSettlement);
  pendingSettlement = undefined;
}

// ^ crates/wsx-daemon/src/lib.rs owns expiry; keep renewal independent of
// persisted state reports so idle agents do not write snapshots every ten seconds.
function renewPresence(): void {
  if (!presenceActive || presenceInFlight || !paneId) return;
  presenceInFlight = true;
  execReporter(reportBin, ["agent", "presence-renew", paneId, "--presence-id", presenceId],
    { timeout: REPORT_TIMEOUT_MS, windowsHide: true }, () => {
      presenceInFlight = false;
    });
}

function startHeartbeat(): void {
  if (presenceHeartbeat === undefined) {
    presenceHeartbeat = setInterval(renewPresence, PRESENCE_INTERVAL_MS);
    presenceHeartbeat.unref?.();
  }
  if (heartbeat !== undefined) return;
  heartbeat = setInterval(() => {
    if (agentActive && blockedCount === 0) report("working");
  }, 300_000);
  heartbeat.unref?.();
}

function stopHeartbeat(): void {
  if (heartbeat !== undefined) clearInterval(heartbeat);
  heartbeat = undefined;
  if (presenceHeartbeat !== undefined) clearInterval(presenceHeartbeat);
  presenceHeartbeat = undefined;
  presenceActive = false;
}

type GoalOwner = { get(): unknown };
type GoalObservation = { owner: GoalOwner; active: boolean; key?: string; completed: boolean; aborted: boolean };
type NativeInput = { exchange: string; round: number; digest: string; inputId: string; session: string; generation: string };
type NativeBinding = NativeInput & { deadline: number; taskKey?: string; initialKey?: string; ctx: ExtensionContext; settled: boolean };

// ^ docs/agent-orchestration.md: Goal Run owns Task completion, not title notifications or pane Done.
function observeNativeExchanges(pi: ExtensionAPI, publish: () => void) {
  let disposed = false;
  let supported = false;
  let owner: GoalOwner | undefined;
  let pendingInput: NativeInput | undefined;
  let binding: NativeBinding | undefined;
  let epoch = 0;
  let timer: ReturnType<typeof setInterval> | undefined;
  const generation = process.env.WSX_RUNTIME_GENERATION;
  const goal = (): GoalObservation | undefined => {
    try {
      if (typeof pi.events.emit !== "function") return undefined;
      const request: { service?: GoalOwner } = {};
      pi.events.emit("pygmalion:goal-run-service-request", request);
      if (typeof request.service?.get !== "function") return undefined;
      const value = request.service.get() as {
        active?: unknown; taskId?: unknown; branchLineage?: unknown; criteria?: Array<{ status?: unknown }>;
        pausedReason?: unknown; completionTransactionId?: unknown;
      };
      if (!value || typeof value.active !== "boolean" || !Array.isArray(value.criteria)
          || value.criteria.length > 16 || value.criteria.some((item) => !item || !["pending", "passed", "blocked"].includes(String(item.status)))) return undefined;
      const identity = (text: unknown): text is string => typeof text === "string" && text.length > 0 && text.length <= 128;
      const key = identity(value.taskId) && identity(value.branchLineage)
        ? JSON.stringify([value.taskId, value.branchLineage]) : undefined;
      if (value.active && !key) return undefined;
      return { owner: request.service, active: value.active, key,
        aborted: value.pausedReason === "user-abort",
        completed: !value.active && value.criteria.length > 0
          && value.criteria.every((item) => item.status === "passed") && identity(value.completionTransactionId) };
    } catch { return undefined; }
  };
  const sessionKey = (ctx: ExtensionContext): string => {
    try { return ctx.sessionManager.getSessionId(); } catch { return ""; }
  };
  const ready = () => !disposed && supported && Boolean(generation)
    && process.env.WSX_RUNTIME_GENERATION === generation && goal()?.owner === owner;
  const clear = () => {
    epoch += 1;
    pendingInput = undefined;
    binding = undefined;
    if (timer !== undefined) clearInterval(timer);
    timer = undefined;
  };
  const command = (args: string[]): Promise<any> => new Promise((resolve, reject) => {
    execReporter(reportBin, args, { timeout: REPORT_TIMEOUT_MS, maxBuffer: 64 * 1024, windowsHide: true },
      (error: Error | null, stdout: string) => {
        if (error) { reject(error); return; }
        try { resolve(JSON.parse(stdout)); } catch (error) { reject(error); }
      });
  });
  const receipt = (input: NativeInput, kind: "accepted" | "completed") => command([
    "agent", "exchange-receipt", input.exchange, "--round", String(input.round), "--receipt", kind,
    "--delivery-sha256", input.digest, "--input-id", input.inputId, "--json",
  ]);
  const rememberTask = (current: NativeBinding, observed: GoalObservation): boolean => {
    if (observed.aborted || observed.owner !== owner) return false;
    if (observed.active) {
      current.taskKey ??= observed.key;
      return current.taskKey === observed.key;
    }
    return current.taskKey ? current.taskKey === observed.key : current.initialKey === observed.key;
  };
  const checkCompletion = () => {
    const current = binding;
    if (!current) return;
    const observed = goal();
    if (!ready() || !observed || sessionKey(current.ctx) !== current.session
        || Date.now() >= current.deadline || !rememberTask(current, observed)) { clear(); return; }
    try {
      if (!current.settled || !current.ctx.isIdle() || current.ctx.hasPendingMessages() || blockedCount > 0
          || observed.active || (current.taskKey && !observed.completed)) return;
    } catch { clear(); return; }
    // Clear before dispatch: a rejected/uncertain native receipt is never replayed.
    clear();
    void receipt(current, "completed").catch((error) => {
      console.warn(`wsx: native Pi completion was not recorded: ${error.message}`);
    });
  };
  const unsubscribeGoal = pi.events.on("pygmalion:goal-run-changed", () => {
    if (binding) {
      const observed = goal();
      if (!observed || !rememberTask(binding, observed)) clear();
    }
  });
  return {
    ready,
    async initialize(ctx: ExtensionContext) {
      clear();
      const ticket = epoch;
      const observed = goal();
      supported = false;
      owner = observed?.owner;
      if (!enabled || !generation || !observed) return;
      try {
        await flushReports();
        const packet = await command(["agent", "context", paneId!, "--metadata-only", "--json"]);
        if (disposed || ticket !== epoch || sessionKey(ctx) === "") return;
        supported = packet.exchange_input_binding === true && packet.projection === "metadata_only"
          && Array.isArray(packet.candidates) && packet.candidates.length === 1
          && String(packet.candidates[0].pane_id) === paneId
          && packet.candidates[0].agent?.provider === "pi" && packet.candidates[0].agent?.attached === true
          && packet.candidates[0].agent?.presence_id === presenceId;
        if (supported) publish();
      } catch { supported = false; }
    },
    input(text: string, source: string, ctx: ExtensionContext, hasImages: boolean) {
      pendingInput = undefined;
      if (source === "extension") return;
      const match = /^\[wsx exchange ([1-9][0-9]{0,19}), round ([1-9][0-9]{0,9}), (?:read-only|writer)\]\n/.exec(text);
      if (!match) {
        const observed = goal();
        // Same native Task retains compatible user steering; replacement/fork invalidates it.
        if (!binding || !observed?.active || observed.key !== binding.taskKey || !rememberTask(binding, observed)) clear();
        return;
      }
      clear();
      if (!ready() || hasImages || Buffer.byteLength(text, "utf8") > 64 * 1024 + 256
          || BigInt(match[1]) > 0xffffffffffffffffn || Number(match[2]) > 0xffffffff) return;
      pendingInput = { exchange: match[1], round: Number(match[2]),
        digest: createHash("sha256").update(text, "utf8").digest("hex"), inputId: randomUUID(),
        session: sessionKey(ctx), generation: generation! };
    },
    async beforeStart(prompt: string, ctx: ExtensionContext, hasImages: boolean) {
      const input = pendingInput;
      pendingInput = undefined;
      if (!input) return;
      const observed = goal();
      if (!ready() || !observed || hasImages || input.session !== sessionKey(ctx)
          || createHash("sha256").update(prompt, "utf8").digest("hex") !== input.digest) { clear(); return; }
      const ticket = epoch;
      try {
        await flushReports();
        const reply = await receipt(input, "accepted");
        const exchange = reply.exchange;
        if (ticket !== epoch || !ready() || input.session !== sessionKey(ctx)) return;
        if (!exchange || exchange.native_input_id !== input.inputId || exchange.delivery_sha256 !== input.digest
            || exchange.round !== input.round || String(exchange.pane_id) !== paneId
            || exchange.runtime_generation !== input.generation || !Number.isSafeInteger(exchange.deadline_unix_ms)) throw new Error("invalid native acceptance response");
        binding = { ...input, deadline: exchange.deadline_unix_ms, ctx, settled: false,
          initialKey: observed.key, taskKey: observed.active ? observed.key : undefined };
        timer = setInterval(checkCompletion, 500);
        timer.unref?.();
      } catch (error) {
        clear();
        console.warn(`wsx: native Pi input was not bound: ${error instanceof Error ? error.message : String(error)}`);
      }
    },
    reset: clear,
    start() { if (binding) binding.settled = false; },
    end(successful: boolean) { if (!successful) clear(); },
    settled(ctx: ExtensionContext) {
      if (!binding) return;
      binding.ctx = ctx;
      binding.settled = true;
      // Let all native settlement observers run; Task ownership is checked again on the timer.
    },
    dispose() {
      disposed = true;
      supported = false;
      clear();
      if (typeof unsubscribeGoal === "function") unsubscribeGoal();
    },
  };
}

type AsyncUiMethod = (...args: unknown[]) => Promise<unknown>;

// ^ Pi shares one mutable ExtensionUIContext across extension callbacks. Wrapping
// its blocking methods keeps every extension independent of wsx status details.
export function observeBlockingUi(uiValue: unknown, onChange: (delta: number) => void): () => void {
  if (!uiValue || typeof uiValue !== "object") return () => {};
  const ui = uiValue as Record<string, unknown>;
  const installed = new Map<string, { original: AsyncUiMethod; wrapped: AsyncUiMethod }>();
  let active = true;
  const restore = () => {
    active = false;
    for (const [method, { original, wrapped }] of installed) {
      if (ui[method] !== wrapped) continue;
      try {
        ui[method] = original;
      } catch {}
    }
    installed.clear();
  };
  try {
    for (const method of BLOCKING_UI_METHODS) {
      const original = ui[method];
      if (typeof original !== "function") continue;
      const wrapped: AsyncUiMethod = (...args) => {
        if (!active) return Reflect.apply(original, uiValue, args) as Promise<unknown>;
        let released = false;
        const release = () => {
          if (released || !active) return;
          released = true;
          onChange(-1);
        };
        onChange(1);
        try {
          return Promise.resolve(Reflect.apply(original, uiValue, args)).finally(release);
        } catch (error) {
          release();
          throw error;
        }
      };
      installed.set(method, { original: original as AsyncUiMethod, wrapped });
      ui[method] = wrapped;
    }
  } catch {
    restore();
  }
  return restore;
}

// ^ [[Session Model]] crates/wsx-core/integrations/pi/wsx-agent-status.ts -> crates/wsx-core/src/runtime/domain.rs
// Pi owns lifecycle interpretation; wsx only accepts the normalized report.
export default function wsxAgentStatus(pi: ExtensionAPI): void {
  const publish = () => report(blockedCount > 0 ? "blocked" : agentActive ? "working" : "idle");
  const native = observeNativeExchanges(pi, publish);
  inputBindingReady = native.ready;
  let restoreBlockingUi: (() => void) | undefined;
  const updateBlocked = (delta: number) => {
    blockedCount = Math.max(0, blockedCount + delta);
    publish();
  };

  pi.events.on("herdr:blocked", (data: unknown) => {
    const blocked = data as { active?: boolean } | undefined;
    blockedCount = blocked?.active ? blockedCount + 1 : Math.max(0, blockedCount - 1);
    publish();
  });
  pi.on("session_before_switch", () => { native.reset(); });
  pi.on("session_before_fork", () => { native.reset(); });
  pi.on("session_before_tree", () => { native.reset(); });
  pi.on("input", (event, ctx) => {
    native.input(event.text, event.source, ctx, Boolean(event.images?.length));
    return { action: "continue" };
  });
  pi.on("before_agent_start", async (event, ctx) => {
    await native.beforeStart(event.prompt, ctx, Boolean(event.images?.length));
  });
  pi.on("session_start", async (_event, ctx) => {
    presenceActive = true;
    restoreBlockingUi?.();
    restoreBlockingUi = undefined;
    blockedCount = 0;
    currentSessionRef = sessionRef(ctx);
    agentActive = ctx.isIdle() === false;
    if (ctx.hasUI) restoreBlockingUi = observeBlockingUi(ctx.ui, updateBlocked);
    startHeartbeat();
    publish();
    await native.initialize(ctx);
  });
  pi.on("agent_start", (_event, ctx) => {
    clearPendingSettlement();
    native.start();
    agentRunGeneration += 1;
    currentSessionRef = sessionRef(ctx);
    agentActive = true;
    lastRunAborted = false;
    publish();
  });
  pi.on("agent_end", (event, ctx) => {
    currentSessionRef = sessionRef(ctx);
    const finalAssistant = event.messages.slice().reverse().find((message) => message.role === "assistant");
    lastRunAborted = finalAssistant?.stopReason === "aborted";
    native.end(finalAssistant?.stopReason === "stop");
  });
  pi.on("agent_settled", (_event, ctx) => {
    native.settled(ctx);
    currentSessionRef = sessionRef(ctx);
    const settledSessionRef = currentSessionRef;
    const settledRunGeneration = agentRunGeneration;
    const settledRunAborted = lastRunAborted;
    clearPendingSettlement();
    pendingSettlement = setTimeout(() => {
      pendingSettlement = undefined;
      if (agentRunGeneration !== settledRunGeneration) return;
      agentActive = false;
      blockedCount = 0;
      report(settledRunAborted ? "idle" : "done", settledSessionRef);
    }, SETTLEMENT_DELAY_MS);
    pendingSettlement.unref?.();
  });
  pi.on("session_shutdown", async () => {
    native.dispose();
    restoreBlockingUi?.();
    restoreBlockingUi = undefined;
    blockedCount = 0;
    clearPendingSettlement();
    stopHeartbeat();
    report("idle", currentSessionRef, false);
    await flushReports();
  });
}
