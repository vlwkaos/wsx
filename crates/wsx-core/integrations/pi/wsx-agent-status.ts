// managed by wsx
// WSX_INTEGRATION_VERSION=14
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { execFile } from "node:child_process";
import path from "node:path";

const REPORT_TIMEOUT_MS = 1_000;
// ^ A later agent_settled handler may start an automatic continuation. Give its
// agent_start event one turn to cancel this adapter's stale final report.
const SETTLEMENT_DELAY_MS = 25;
const BLOCKING_UI_METHODS = ["select", "confirm", "input", "custom", "editor"] as const;
const paneId = process.env.WSX_PANE_ID;
const reportBin = process.env.WSX_AGENT_REPORT_BIN || "wsx";
const enabled = typeof paneId === "string" && /^[1-9][0-9]*$/.test(paneId);

type ReportState = "idle" | "working" | "blocked" | "done";
type SessionRef = { id?: string; path?: string };

let sendInFlight = false;
let pending: { state: ReportState; sessionRef?: SessionRef } | undefined;
let agentActive = false;
let blockedCount = 0;
let lastRunAborted = false;
let currentSessionRef: SessionRef | undefined;
let pendingSettlement: ReturnType<typeof setTimeout> | undefined;
let heartbeat: ReturnType<typeof setInterval> | undefined;

function report(state: ReportState, sessionRef = currentSessionRef): void {
  if (!enabled) return;
  pending = { state, sessionRef };
  drain();
}

function drain(): void {
  if (sendInFlight || !pending || !paneId) return;
  const next = pending;
  pending = undefined;
  sendInFlight = true;
  const args = ["agent", "report", paneId, "--provider", "pi", "--state", next.state, "--lifecycle"];
  if (next.sessionRef?.path) args.push("--session-path", next.sessionRef.path);
  else if (next.sessionRef?.id) args.push("--session-id", next.sessionRef.id);
  execFile(reportBin, args, { timeout: REPORT_TIMEOUT_MS, windowsHide: true }, () => {
    sendInFlight = false;
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

function startHeartbeat(): void {
  if (heartbeat !== undefined) return;
  heartbeat = setInterval(() => {
    if (agentActive && blockedCount === 0) report("working");
  }, 300_000);
  heartbeat.unref?.();
}

function stopHeartbeat(): void {
  if (heartbeat !== undefined) clearInterval(heartbeat);
  heartbeat = undefined;
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
  pi.on("session_start", (_event, ctx) => {
    restoreBlockingUi?.();
    restoreBlockingUi = undefined;
    blockedCount = 0;
    currentSessionRef = sessionRef(ctx);
    agentActive = ctx.isIdle() === false;
    if (ctx.hasUI) restoreBlockingUi = observeBlockingUi(ctx.ui, updateBlocked);
    startHeartbeat();
    publish();
  });
  pi.on("agent_start", (_event, ctx) => {
    clearPendingSettlement();
    currentSessionRef = sessionRef(ctx);
    agentActive = true;
    lastRunAborted = false;
    publish();
  });
  pi.on("agent_end", (event, ctx) => {
    currentSessionRef = sessionRef(ctx);
    const finalAssistant = event.messages.slice().reverse().find((message) => message.role === "assistant");
    lastRunAborted = finalAssistant?.stopReason === "aborted";
  });
  pi.on("agent_settled", (_event, ctx) => {
    currentSessionRef = sessionRef(ctx);
    const settledSessionRef = currentSessionRef;
    const settledRunAborted = lastRunAborted;
    clearPendingSettlement();
    pendingSettlement = setTimeout(() => {
      pendingSettlement = undefined;
      if (ctx.isIdle() === false) {
        agentActive = true;
        publish();
        return;
      }
      agentActive = false;
      blockedCount = 0;
      report(settledRunAborted ? "idle" : "done", settledSessionRef);
    }, SETTLEMENT_DELAY_MS);
    pendingSettlement.unref?.();
  });
  pi.on("session_shutdown", () => {
    restoreBlockingUi?.();
    restoreBlockingUi = undefined;
    blockedCount = 0;
    clearPendingSettlement();
    stopHeartbeat();
  });
}
