// One shared state for the page. Views read it through `useStore` and never keep
// their own copy. The event stream is its only writer after the first load.
import { useEffect, useState } from "./lib.js";

export const state = {
  site: null,
  device: null,
  projects: [],
  sessions: [],
  // False until the first workspace event: the lists are not known yet.
  loaded: false,
  daemon: true,
  online: true,
};
const listeners = new Set();
let version = 0;
export function update(patch) {
  Object.assign(state, patch);
  version += 1;
  for (const listener of listeners) listener();
}
export function useStore() {
  const [, renew] = useState(0);
  const rendered = version;
  useEffect(() => {
    const listener = () => renew((count) => count + 1);
    listeners.add(listener);
    // An update between this render and this subscription would be missed.
    if (rendered !== version) listener();
    return () => listeners.delete(listener);
  }, []);
  return state;
}

// `history` and `exchange` events of the stream, for the open chat.
export const events = new EventTarget();

export const can = (capability) => state.site.capabilities.includes(capability);
export const shows = (panel) => state.site.panels.includes(panel);
export const touch = () => matchMedia("(pointer: coarse)").matches || innerWidth < 700;

// State in words. `group` places a session in the inbox; `tone` repeats the word in color.
const STATES = {
  blocked: { word: "needs input", group: "needs", tone: "attention" },
  error: { word: "error", group: "needs", tone: "danger" },
  working: { word: "working", group: "running", tone: "accent" },
  done: { word: "done", group: "finished", tone: "muted" },
  idle: { word: "idle", group: "rest", tone: "muted" },
  unknown: { word: "not connected", group: "rest", tone: "muted" },
};
export function stateOf(session) {
  if (session.exited) return { key: "exited", word: "exited", group: session.agent ? "needs" : "rest", tone: session.agent ? "danger" : "muted" };
  if (!session.agent) return { key: "shell", word: "shell", group: "rest", tone: "muted" };
  const key = session.agent.attached ? session.agent.state : "unknown";
  return { key, ...(STATES[key] ?? STATES.unknown) };
}

// The screen a session opens on: chat when the site has it, else the terminal.
export const sessionLink = (session, tab) =>
  `#/s/${session.id}/${tab ?? (shows("chat") ? "chat" : "terminal")}`;

// "Seen" is per device: the state a session had when its chat was last opened here.
const SEEN_KEY = "wsx-seen";
function seen() {
  try { return JSON.parse(localStorage.getItem(SEEN_KEY)) ?? {}; } catch { return {}; }
}
export const isNew = (session) => seen()[session.id] !== session.state_since_unix_ms;
export function markSeen(session) {
  const known = seen();
  if (known[session.id] === session.state_since_unix_ms) return;
  // Forget sessions that no longer exist, so the record stays small.
  const kept = Object.fromEntries(state.sessions.filter((other) => other.id in known).map((other) => [other.id, known[other.id]]));
  kept[session.id] = session.state_since_unix_ms;
  try { localStorage.setItem(SEEN_KEY, JSON.stringify(kept)); } catch { /* a private window shows "new" again */ }
}

// Routes: #/ (inbox), #/projects, #/settings, #/s/<id>/chat|terminal, #/p/<id>/files.
export function parseRoute(hash) {
  const [, first = "", second, third] = hash.split("/");
  if (first === "s" && Number(second)) return { screen: "session", id: Number(second), tab: third === "terminal" ? "terminal" : "chat" };
  if (first === "p" && Number(second)) return { screen: "files", id: Number(second) };
  if (first === "projects" || first === "settings") return { screen: first };
  return { screen: "inbox" };
}
export function useRoute() {
  const [route, setRoute] = useState(parseRoute(location.hash));
  const rendered = location.hash;
  useEffect(() => {
    const changed = () => setRoute(parseRoute(location.hash));
    addEventListener("hashchange", changed);
    // A hash change between this render and this subscription would be missed.
    if (location.hash !== rendered) changed();
    return () => removeEventListener("hashchange", changed);
  }, []);
  return route;
}
