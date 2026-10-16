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
  // The server's build when it differs from this page's: the page offers a reload.
  newBuild: null,
  // wsx version, commit, build id and server start, from api/session.
  build: null,
  // Each granted project's task overview by project id, with task_view (docs/web-client.md "Tasks").
  tasks: {},
};

// ^ This page's build: the path segment its modules were loaded from, assets/<build>/.
export const pageBuild = new URL(import.meta.url).pathname.split("/").at(-2);
export function noteBuild(build) {
  if (build && build !== pageBuild && build !== state.newBuild) update({ newBuild: build });
}
// The service worker fetches the new files first, so an offline reload still has them;
// then the page reloads. Drafts are kept on the device (chat.js), so nothing typed is lost.
export async function reloadForBuild() {
  try {
    const registration = await navigator.serviceWorker?.getRegistration();
    await Promise.race([registration?.update(), new Promise((done) => setTimeout(done, 3000))]);
  } catch { /* an update that fails still reloads from the network */ }
  location.reload();
}
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
// A session without an agent is a shell: no inbox group, no chat.
export const isAgent = (session) => Boolean(session.agent);
export function stateOf(session) {
  if (session.exited) return { key: "exited", word: "exited", group: session.agent ? "needs" : null, tone: session.agent ? "danger" : "muted" };
  if (!session.agent) return { key: "shell", word: "open", group: null, tone: "muted" };
  const key = session.agent.attached ? session.agent.state : "unknown";
  return { key, ...(STATES[key] ?? STATES.unknown) };
}

// The screen a session opens on: chat for an agent when the site has it, else the terminal.
export const sessionLink = (session, tab) =>
  `#/s/${session.id}/${tab ?? (isAgent(session) && shows("chat") ? "chat" : "terminal")}`;

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

// "Done, not seen" is per device: the time a landed task was last opened here, by `<project>/<task>`.
const TASK_SEEN_KEY = "wsx-task-seen";
function tasksSeen() {
  try { return JSON.parse(localStorage.getItem(TASK_SEEN_KEY)) ?? {}; } catch { return {}; }
}
export const taskSeen = (project, task, since) => (tasksSeen()[`${project}/${task}`] ?? 0) >= since;
export function markTaskSeen(project, task, since) {
  const known = tasksSeen();
  if ((known[`${project}/${task}`] ?? 0) >= since) return;
  known[`${project}/${task}`] = since;
  // The oldest go first, so the record stays small.
  const kept = Object.fromEntries(Object.entries(known).sort((left, right) => right[1] - left[1]).slice(0, 200));
  try { localStorage.setItem(TASK_SEEN_KEY, JSON.stringify(kept)); } catch { /* a private window shows it again */ }
}

// Routes: #/ (inbox), #/projects, #/settings, #/s/<id>/chat|terminal, #/p/<id> (the manager with
// task_view, else the sessions), #/p/<id>/sessions, #/p/<id>/files, #/p/<id>/t/<task>.
export function parseRoute(hash) {
  const [, first = "", second, third, fourth] = hash.split("/");
  if (first === "s" && Number(second)) return { screen: "session", id: Number(second), tab: third === "terminal" ? "terminal" : "chat" };
  if (first === "p" && Number(second) && third === "t" && Number(fourth)) return { screen: "task", id: Number(second), task: Number(fourth) };
  if (first === "p" && Number(second)) {
    return { screen: { files: "files", sessions: "sessions" }[third] ?? "project", id: Number(second) };
  }
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

// ^ Back returns to the screen the reader came from. Each history entry of this page
// records its depth and the hash before it; an entry opened from outside (a link or a
// notification) has depth 0, so Back goes to its fallback instead of leaving the page.
let depth = 0;
export function trackHistory() {
  const known = history.state?.wsx ? history.state : { wsx: true, depth: 0, from: null };
  depth = known.depth;
  history.replaceState(known, "");
  addEventListener("hashchange", (event) => {
    if (history.state?.wsx) {
      depth = history.state.depth;
      return;
    }
    depth += 1;
    history.replaceState({ wsx: true, depth, from: new URL(event.oldURL).hash || "#/" }, "");
  });
}
// The hash of this page's previous history entry, if there is one.
const previous = () => (depth > 0 && history.state?.from) || null;
// The screen Back leads to: the previous entry of this page, else `fallback`.
export const backTarget = (fallback) => previous() ?? fallback;
export function goBack(event) {
  if (previous()) {
    event.preventDefault();
    history.back();
  }
}
// Switch a view of the same screen (Chat or Terminal) without a new history entry,
// so Back still leads to where the screen was opened from.
export function replaceRoute(event, hash) {
  event.preventDefault();
  if (location.hash === hash) return;
  const oldURL = location.href;
  history.replaceState(history.state, "", hash);
  dispatchEvent(new HashChangeEvent("hashchange", { oldURL, newURL: location.href }));
}
