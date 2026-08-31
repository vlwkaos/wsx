// managed by wsx
// WSX_INTEGRATION_VERSION=@VERSION@
import { execFile } from "node:child_process";

const provider = "@PROVIDER@";
let rootSession;
const childSessions = new Set();

function report(state, id) {
  const pane = process.env.WSX_PANE_ID;
  if (!pane) return;
  const args = [
    "agent", "report", pane, "--provider", provider, "--state", state, "--lifecycle",
  ];
  if (id) args.push("--conversation-id", id);
  execFile(process.env.WSX_AGENT_REPORT_BIN || "wsx", args,
    { timeout: 1000, windowsHide: true }, () => {});
}

function sessionID(properties) {
  return typeof properties?.sessionID === "string" ? properties.sessionID : undefined;
}

function statusState(status) {
  const kind = typeof status === "string" ? status : status?.type;
  if (typeof kind !== "string") return undefined;
  if (kind.toLowerCase() === "idle") return "idle";
  if (["active", "busy", "pending", "retry", "running", "streaming", "working"]
    .includes(kind.toLowerCase())) return "working";
  return undefined;
}

export const WsxAgentStatusPlugin = async () => ({
  "chat.message": async ({ sessionID: id }) => {
    if (!id || childSessions.has(id)) return;
    rootSession = id;
    report("working", id);
  },
  event: async ({ event }) => {
    const type = event?.type;
    const properties = event?.properties || {};
    const id = sessionID(properties);
    if (properties.info?.id && properties.info?.parentID) {
      childSessions.add(properties.info.id);
    }
    if (id && childSessions.has(id)) {
      if (["permission.asked", "question.asked"].includes(type)) report("blocked");
      if (["permission.replied", "question.replied", "question.rejected"].includes(type)) {
        report("working");
      }
      return;
    }
    // OpenCode server events are global. Only lifecycle events belonging to the
    // root selected by this pane's chat path may update pane state.
    if (!rootSession || id !== rootSession) return;
    if (type === "session.status") {
      const state = statusState(properties.status);
      if (state) report(state, id);
    } else if (["permission.asked", "question.asked", "session.error"].includes(type)) {
      report("blocked", id);
    } else if (type === "session.idle") {
      report("idle", id);
    } else if ([
      "tool.execute.before", "tool.execute.after", "permission.replied",
      "question.replied", "question.rejected", "session.compacted",
    ].includes(type)) {
      report("working", id);
    }
  },
});
