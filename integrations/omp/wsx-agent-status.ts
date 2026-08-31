// managed by wsx
// WSX_INTEGRATION_VERSION=8
import { execFile } from "node:child_process";

const pane = process.env.WSX_PANE_ID;
let blocked = 0;
let active = false;

function report(state: "idle" | "working" | "blocked" | "done", ctx: any): void {
  if (!pane) return;
  let id: string | undefined;
  try {
    id = ctx?.sessionManager?.getSessionId?.();
  } catch {
    id = undefined;
  }
  const args = [
    "agent", "report", pane, "--provider", "omp", "--state", state, "--lifecycle",
  ];
  if (id) args.push("--conversation-id", id);
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
    current(ctx);
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
  pi.on("session_shutdown", (_event: any, ctx: any) => report("done", ctx));
}
