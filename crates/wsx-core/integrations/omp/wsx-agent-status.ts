// managed by wsx
// WSX_INTEGRATION_VERSION=12
import { execFile } from "node:child_process";
import path from "node:path";

const pane = process.env.WSX_PANE_ID;
const reportBin = process.env.WSX_AGENT_REPORT_BIN || "wsx";
let blocked = 0;
let active = false;
let currentContext: any;
let sendInFlight = false;
let pendingArgs: string[] | undefined;
let drainWaiters: Array<() => void> = [];

function report(
  state: "idle" | "working" | "blocked" | "done",
  ctx: any,
  attached = true,
): void {
  if (!pane) return;
  let sessionPath: string | undefined;
  let sessionId: string | undefined;
  try {
    const value = ctx?.sessionManager?.getSessionFile?.();
    sessionPath = typeof value === "string" && path.isAbsolute(value) ? value : undefined;
  } catch {
    sessionPath = undefined;
  }
  try {
    const value = ctx?.sessionManager?.getSessionId?.();
    sessionId = typeof value === "string" && value ? value : undefined;
  } catch {
    sessionId = undefined;
  }
  const args = [
    "agent", "report", pane, "--provider", "omp", "--state", state, "--lifecycle",
  ];
  if (!attached) args.push("--detached");
  if (sessionPath) args.push("--session-path", sessionPath);
  else if (sessionId) args.push("--session-id", sessionId);
  pendingArgs = args;
  drain();
}

function settleDrainWaiters(): void {
  if (sendInFlight || pendingArgs) return;
  for (const resolve of drainWaiters.splice(0)) resolve();
}

function flushReports(): Promise<void> {
  if (!sendInFlight && !pendingArgs) return Promise.resolve();
  return new Promise((resolve) => drainWaiters.push(resolve));
}

function drain(): void {
  if (sendInFlight || !pendingArgs) {
    settleDrainWaiters();
    return;
  }
  const args = pendingArgs;
  pendingArgs = undefined;
  sendInFlight = true;
  execFile(reportBin, args, { timeout: 1000, windowsHide: true }, () => {
    sendInFlight = false;
    drain();
  });
}

export default function wsxOmpAgentStatus(pi: any): void {
  const current = (ctx: any) => {
    currentContext = ctx;
    report(blocked > 0 ? "blocked" : active ? "working" : "idle", ctx);
  };
  pi.on("session_start", (_event: any, ctx: any) => current(ctx));
  pi.on("session_switch", (_event: any, ctx: any) => {
    blocked = 0;
    active = false;
    current(ctx);
  });
  pi.on("session_shutdown", async (_event: any, ctx: any) => {
    blocked = 0;
    active = false;
    report("idle", ctx, false);
    await flushReports();
  });
  pi.on("agent_start", (_event: any, ctx: any) => {
    active = true;
    current(ctx);
  });
  pi.on("agent_end", (_event: any, ctx: any) => {
    active = false;
    blocked = 0;
    report("done", ctx);
  });
  pi.on("tool_approval_requested", (_event: any, ctx: any) => {
    blocked += 1;
    current(ctx);
  });
  pi.on("tool_approval_resolved", (_event: any, ctx: any) => {
    blocked = Math.max(0, blocked - 1);
    current(ctx);
  });
  pi.on("tool_execution_start", (event: any, ctx: any) => {
    if (event?.toolName === "ask") {
      blocked += 1;
      current(ctx);
    }
  });
  pi.on("tool_execution_end", (event: any, ctx: any) => {
    if (event?.toolName === "ask") {
      blocked = Math.max(0, blocked - 1);
      current(ctx);
    }
  });
  const heartbeat = setInterval(() => {
    if (active && blocked === 0) report("working", currentContext);
  }, 300_000);
  heartbeat.unref?.();
}
