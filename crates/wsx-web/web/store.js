// One shared state for the page. Views read it through `useStore` and never keep
// their own copy. The event stream is its only writer after the first load.
import { api } from "./api.js";
import { readMap, useEffect, useState, writeMap } from "./lib.js";

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
  // Each granted project's task overview by project id, with task_view (docs/web-client-tasks.md).
  tasks: {},
  // This device's notification switches and muted sessions (api/push), once read.
  push: null,
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
// ^ Read once when Settings or a session's menu first needs it; each change answers the new state.
let pushRead = null;
export function loadPush() {
  pushRead ??= api("api/push").then((data) => update({ push: data })).catch((failure) => { pushRead = null; throw failure; });
  return pushRead;
}
export async function setMuted(session, muted) {
  update({ push: await api("api/push/mute", { method: "POST", json: { session, muted } }) });
}
const listeners = new Set();
let version = 0;
export function update(patch) {
  Object.assign(state, patch);
  version += 1;
  showBadge();
  for (const listener of listeners) listener();
}
// ^ The app badge counts what needs the owner, as the server does for a push: blocked agent
// sessions of the lists and every project's task needs (docs/web-client.md "PWA and push").
// The count changes when the data loads and when an opened item is answered.
let badge = null;
function showBadge() {
  if (!state.loaded || !("setAppBadge" in navigator)) return;
  const blocked = listedSessions(state.sessions).filter((session) => isAgent(session) && stateOf(session).key === "blocked").length;
  const count = blocked + Object.values(state.tasks).reduce((sum, overview) => sum + (overview?.needs?.length ?? 0), 0);
  if (count === badge) return;
  badge = count;
  (count ? navigator.setAppBadge(count) : navigator.clearAppBadge()).catch(() => {});
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

// A screen that reads its data now and again when the stream says it changed: `load` runs on
// each event in `names` (one name or a list) whose detail `wanted` accepts. `deps` as for
// useEffect: the ids that `wanted` and `load` stand for.
export function useEventLoad(names, load, wanted, deps) {
  useEffect(() => {
    const list = [names].flat();
    const changed = (event) => { if (wanted(event.detail)) load(); };
    load();
    for (const name of list) events.addEventListener(name, changed);
    return () => { for (const name of list) events.removeEventListener(name, changed); };
  }, deps);
}

export const can = (capability) => state.site.capabilities.includes(capability);
export const shows = (panel) => state.site.panels.includes(panel);
export const touch = () => matchMedia("(pointer: coarse)").matches || innerWidth < 700;

// State in words. `group` places a session in the inbox; `tone` repeats the word in color.
const STATES = {
  blocked: { word: "needs input", group: "needs", tone: "attention" },
  error: { word: "error", group: "needs", tone: "danger" },
  working: { word: "working", group: "running", tone: "working" },
  done: { word: "done", group: "finished", tone: "muted" },
  idle: { word: "idle", group: "rest", tone: "muted" },
  unknown: { word: "not connected", group: "rest", tone: "muted" },
};
// A session without an agent is a shell: no inbox group, no chat.
export const isAgent = (session) => Boolean(session.agent);
// ^ A task worker (`task-<n>`) or the project manager (`task-manager`): its task or the
// project chat shows it, so the session lists leave it out (docs/web-client-tasks.md "Home").
// The hosts report as providers `task` and `manager`; the name covers a host not yet reporting.
const isTaskSession = (session) =>
  ["task", "manager"].includes(session.agent?.provider) || /^task-(\d+|manager)$/.test(session.label);
// The sessions a list shows.
export const listedSessions = (sessions) => sessions.filter((session) => !isTaskSession(session));
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
const seen = () => readMap(SEEN_KEY);
export const isNew = (session) => seen()[session.id] !== session.state_since_unix_ms;
export function markSeen(session) {
  const known = seen();
  if (known[session.id] === session.state_since_unix_ms) return;
  // Forget sessions that no longer exist, so the record stays small.
  const kept = Object.fromEntries(state.sessions.filter((other) => other.id in known).map((other) => [other.id, known[other.id]]));
  kept[session.id] = session.state_since_unix_ms;
  writeMap(SEEN_KEY, kept);
}

// Routes: #/ (home: the projects with task_view, else the inbox), #/sessions (the inbox),
// #/projects, #/settings, #/s/<id>/chat|terminal, #/p/<id> (the manager with task_view, else the
// sessions), #/p/<id>/sessions, #/p/<id>/files, #/p/<id>/t/<task> (the manager with the task's sheet),
// #/p/<id>/page (the manager with the project page's sheet), #/p/<id>/checks (with the To check list).
export function parseRoute(hash) {
  const [, first = "", second, third, fourth] = hash.split("/");
  if (first === "s" && Number(second)) return { screen: "session", id: Number(second), tab: third === "terminal" ? "terminal" : "chat" };
  if (first === "p" && Number(second) && third === "t" && Number(fourth)) return { screen: "task", id: Number(second), task: Number(fourth) };
  if (first === "p" && Number(second) && third === "page") return { screen: "task", id: Number(second), task: null, page: true };
  if (first === "p" && Number(second) && third === "checks") return { screen: "task", id: Number(second), task: null, checks: true };
  if (first === "p" && Number(second)) {
    return { screen: { files: "files", sessions: "sessions" }[third] ?? "project", id: Number(second) };
  }
  if (first === "projects" || first === "settings") return { screen: first };
  if (first === "sessions") return { screen: "inbox" };
  return { screen: "home" };
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
// Put `under` beneath the current entry when the page was opened on this one from outside:
// a task deep link (a notification) gets its chat beneath, so Back closes the sheet first.
export function stackUnder(under) {
  if (depth > 0) return;
  const hash = location.hash;
  history.replaceState({ wsx: true, depth: 0, from: null }, "", under);
  history.pushState({ wsx: true, depth: 1, from: under }, "", hash);
  depth = 1;
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
  replaceHash(hash);
}
function replaceHash(hash) {
  if (location.hash === hash) return;
  const oldURL = location.href;
  history.replaceState(history.state, "", hash);
  dispatchEvent(new HashChangeEvent("hashchange", { oldURL, newURL: location.href }));
}
// Leave a screen layer (a sheet) for `under`: Back when the previous entry is `under`,
// else replace this entry, so the next Back does not return to the layer.
export function leaveFor(under) {
  if (previous() === under) history.back();
  else replaceHash(under);
}
