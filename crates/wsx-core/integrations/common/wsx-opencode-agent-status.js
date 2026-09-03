// managed by wsx
// WSX_INTEGRATION_VERSION=@VERSION@
import { execFile } from "node:child_process";

const provider = "@PROVIDER@";
const reportBin = process.env.WSX_AGENT_REPORT_BIN || "wsx";
let rootSession;
const childSessions = new Set();
const activeSessions = new Set();
let sendInFlight = false;
let pendingArgs;

function report(state, id) {
  if (id && ["working", "blocked"].includes(state)) activeSessions.add(id);
  if (id && ["idle", "done"].includes(state)) activeSessions.delete(id);
  const pane = process.env.WSX_PANE_ID;
  if (!pane) return;
  const args = [
    "agent", "report", pane, "--provider", provider, "--state", state, "--lifecycle",
  ];
  if (id) args.push("--session-id", id);
  pendingArgs = args;
  drain();
}

function drain() {
  if (sendInFlight || !pendingArgs) return;
  const args = pendingArgs;
  pendingArgs = undefined;
  sendInFlight = true;
  execFile(reportBin, args, { timeout: 1000, windowsHide: true }, () => {
    sendInFlight = false;
    drain();
  });
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

export const WsxAgentStatusPlugin = async () => {
  const heartbeat = setInterval(() => {
    if (rootSession && activeSessions.has(rootSession)) report("working", rootSession);
  }, 300_000);
  heartbeat.unref?.();
  return {
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
        if (["permission.asked", "question.asked"].includes(type)) {
          report("blocked", rootSession);
        }
        if (["permission.replied", "question.replied", "question.rejected"].includes(type)) {
          report("working", rootSession);
        }
        return;
      }
      // OpenCode server events are global. Only lifecycle events belonging to the
      // root selected by this pane's chat path may update pane state.
      if (!rootSession || id !== rootSession) return;
      if (type === "session.status") {
        const state = statusState(properties.status);
        if (state === "idle") report(activeSessions.has(id) ? "done" : "idle", id);
        else if (state) report(state, id);
      } else if (["permission.asked", "question.asked", "session.error"].includes(type)) {
        report("blocked", id);
      } else if (type === "session.idle") {
        report(activeSessions.has(id) ? "done" : "idle", id);
      } else if ([
        "tool.execute.before", "tool.execute.after", "permission.replied",
        "question.replied", "question.rejected", "session.compacted",
      ].includes(type)) {
        report("working", id);
      }
    },
  };
};
