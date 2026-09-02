// managed by wsx
// WSX_INTEGRATION_VERSION=10
import { execFile } from "node:child_process";
import path from "node:path";

const pane = process.env.WSX_PANE_ID;
let blocked = 0;
let active = false;

function report(state: "idle" | "working" | "blocked" | "done", ctx: any): void {
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
  if (sessionPath) args.push("--session-path", sessionPath);
  else if (sessionId) args.push("--session-id", sessionId);
  execFile(process.env.WSX_AGENT_REPORT_BIN || "wsx", args,
    { timeout: 1000, windowsHide: true }, () => {});
}

export default function wsxOmpAgentStatus(pi: any): void {
  const current = (ctx: any) => report(blocked > 0 ? "blocked" : active ? "working" : "idle", ctx);
  pi.on("session_start", (_event: any, ctx: any) => current(ctx));
  pi.on("session_switch", (_event: any, ctx: any) => {
    blocked = 0;
    active = false;
    current(ctx);
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
}
