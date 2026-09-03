// managed by wsx
// WSX_INTEGRATION_VERSION=13
import { execFile } from "node:child_process";

function report(sessionID) {
  const pane = process.env.WSX_PANE_ID;
  if (!pane || !sessionID) return Promise.resolve();
  const args = [
    "agent", "report", pane, "--provider", "opencode", "--state", "unknown",
    "--session-id", sessionID,
  ];
  return new Promise((resolve) => {
    execFile(process.env.WSX_AGENT_REPORT_BIN || "wsx", args,
      { timeout: 1000, windowsHide: true }, resolve);
  });
}

export default {
  id: "wsx.opencode.session-selection",
  tui: async (api) => {
    let selected;
    const timer = setInterval(() => {
      const route = api.route.current;
      const id = route?.name === "session" ? route.params?.sessionID : undefined;
      const session = typeof id === "string" ? api.state.session.get(id) : undefined;
      if (!session || session.parentID || id === selected) return;
      selected = id;
      void report(id);
    }, 100);
    timer.unref?.();
  },
};
