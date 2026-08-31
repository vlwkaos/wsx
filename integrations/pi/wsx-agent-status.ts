// managed by wsx
// WSX_INTEGRATION_VERSION=8
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { execFile } from "node:child_process";

const REPORT_TIMEOUT_MS = 1_000;
const paneId = process.env.WSX_PANE_ID;
const reportBin = process.env.WSX_AGENT_REPORT_BIN || "wsx";
const enabled = typeof paneId === "string" && /^[1-9][0-9]*$/.test(paneId);

type ReportState = "idle" | "working" | "blocked" | "done";

let sendInFlight = false;
let pending: { state: ReportState; conversationId?: string } | undefined;
let agentActive = false;
let blockedCount = 0;
let currentConversationId: string | undefined;

function report(state: ReportState, conversationId = currentConversationId): void {
  if (!enabled) return;
  pending = { state, conversationId };
  drain();
}

function drain(): void {
  if (sendInFlight || !pending || !paneId) return;
  const next = pending;
  pending = undefined;
  sendInFlight = true;
  const args = ["agent", "report", paneId, "--provider", "pi", "--state", next.state, "--lifecycle"];
  if (next.conversationId) args.push("--conversation-id", next.conversationId);
  execFile(reportBin, args, { timeout: REPORT_TIMEOUT_MS, windowsHide: true }, () => {
    sendInFlight = false;
    drain();
  });
}

function conversationId(ctx: unknown): string | undefined {
  const value = (ctx as { sessionManager?: { getSessionId?: () => unknown } } | undefined)
    ?.sessionManager?.getSessionId?.();
  return typeof value === "string" && value ? value : undefined;
}

// ^ [[Session Model]] integrations/pi/wsx-agent-status.ts -> crates/wsx-core/src/runtime/domain.rs
// Pi owns lifecycle interpretation; wsx only accepts the normalized report.
export default function wsxAgentStatus(pi: ExtensionAPI): void {
  const publish = () => report(blockedCount > 0 ? "blocked" : agentActive ? "working" : "idle");

  pi.events.on("herdr:blocked", (data: unknown) => {
    const blocked = data as { active?: boolean } | undefined;
    blockedCount = blocked?.active ? blockedCount + 1 : Math.max(0, blockedCount - 1);
    publish();
  });
  pi.on("session_start", (_event, ctx) => {
    currentConversationId = conversationId(ctx);
    agentActive = ctx.isIdle() === false;
    publish();
  });
  pi.on("agent_start", (_event, ctx) => {
    currentConversationId = conversationId(ctx);
    agentActive = true;
    publish();
  });
  pi.on("agent_settled", (_event, ctx) => {
    currentConversationId = conversationId(ctx);
    agentActive = false;
    publish();
  });
  pi.on("session_shutdown", (event, ctx) => {
    if (event.reason === "quit") report("done", conversationId(ctx));
  });
}
