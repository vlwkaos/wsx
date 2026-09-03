// managed by wsx
// WSX_INTEGRATION_VERSION=11
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { execFile } from "node:child_process";
import path from "node:path";

const REPORT_TIMEOUT_MS = 1_000;
const paneId = process.env.WSX_PANE_ID;
const reportBin = process.env.WSX_AGENT_REPORT_BIN || "wsx";
const enabled = typeof paneId === "string" && /^[1-9][0-9]*$/.test(paneId);

type ReportState = "idle" | "working" | "blocked" | "done";
type SessionRef = { id?: string; path?: string };

let sendInFlight = false;
let pending: { state: ReportState; sessionRef?: SessionRef } | undefined;
let agentActive = false;
let blockedCount = 0;
let currentSessionRef: SessionRef | undefined;

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

// ^ [[Session Model]] crates/wsx-core/integrations/pi/wsx-agent-status.ts -> crates/wsx-core/src/runtime/domain.rs
// Pi owns lifecycle interpretation; wsx only accepts the normalized report.
export default function wsxAgentStatus(pi: ExtensionAPI): void {
  const publish = () => report(blockedCount > 0 ? "blocked" : agentActive ? "working" : "idle");

  pi.events.on("herdr:blocked", (data: unknown) => {
    const blocked = data as { active?: boolean } | undefined;
    blockedCount = blocked?.active ? blockedCount + 1 : Math.max(0, blockedCount - 1);
    publish();
  });
  pi.on("session_start", (_event, ctx) => {
    currentSessionRef = sessionRef(ctx);
    agentActive = ctx.isIdle() === false;
    publish();
  });
  pi.on("agent_start", (_event, ctx) => {
    currentSessionRef = sessionRef(ctx);
    agentActive = true;
    publish();
  });
  pi.on("agent_settled", (_event, ctx) => {
    currentSessionRef = sessionRef(ctx);
    agentActive = false;
    blockedCount = 0;
    report("done");
  });
  const heartbeat = setInterval(() => {
    if (agentActive && blockedCount === 0) report("working");
  }, 300_000);
  heartbeat.unref?.();
}
