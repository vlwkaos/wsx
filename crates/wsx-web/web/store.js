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
  // The granted projects that declare a machine panel, with those panels (panels.js loadMachine).
  machine: [],
  // Where the header bar sits on this device: "top" or "bottom" (ui.js, settings.js).
  bar: "top",
  // The bottom-bar tabs this device hides, by name (ui.js; Settings is never in it).
  hiddenTabs: [],
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
const blockedCount = () => listedSessions(state.sessions).filter((session) => isAgent(session) && stateOf(session).key === "blocked").length;
const needsCount = () => Object.values(state.tasks).reduce((sum, overview) => sum + (overview?.needs?.length ?? 0), 0);
function showBadge() {
  if (!state.loaded || !("setAppBadge" in navigator)) return;
  const count = blockedCount() + needsCount();
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

// ^ The one list of the bottom bar's screens: name, link, label, icon and the count on its
// icon, the two counts the app badge sums. The bar, its hidden tabs and Settings > UI read it,
// so a tab added here shows in all three (docs/web-client.md "Screens").
// ^ Machine (docs/panels.md "Machine panels") is there only for a device that may read panels
// and a grant with a machine panel in it.
export const tabs = () => {
  const list = can("task_view")
    ? [["home", "#/", "Home", "home", needsCount()], ["inbox", "#/sessions", "Sessions", "terminal", blockedCount()]]
    : [["home", "#/", "Inbox", "inbox", blockedCount()], ["projects", "#/projects", "Projects", "projects", 0]];
  if (can("terminal_input") && shows("terminal") && state.machine.length) list.push(["machine", "#/machine", "Machine", "panel", 0]);
  return [...list, ["settings", "#/settings", "Settings", "settings", 0]];
};
// The tabs this device shows: Settings always, so the way back to the switches stays.
export const shownTabs = () => tabs().filter(([name]) => name === "settings" || !state.hiddenTabs.includes(name));

// Routes: #/ (home: the projects with task_view, else the inbox), #/sessions (the inbox),
// #/projects, #/machine, #/settings[/ui|usage], #/s/<id>/chat|terminal, #/p/<id> (the manager with task_view, else the
// sessions), #/p/<id>/sessions, #/p/<id>/files, #/p/<id>/t/<task> (the manager with the task's sheet),
// #/p/<id>/page (the manager with the project overview's sheet), #/p/<id>/checks (with the To check list),
// #/p/<id>/usage (with the project's token use and cost), #/p/<id>/routines (with its routines), #/p/<id>/panels (with its panels),
// #/p/<id>/r/<role> (a role's chat, docs/tasks.md "Roles").
export function parseRoute(hash) {
  const [, first = "", second, third, fourth] = hash.split("/");
  if (first === "s" && Number(second)) return { screen: "session", id: Number(second), tab: third === "terminal" ? "terminal" : "chat" };
  if (first === "p" && Number(second) && third === "t" && Number(fourth)) return { screen: "task", id: Number(second), task: Number(fourth) };
  if (first === "p" && Number(second) && third === "page") return { screen: "task", id: Number(second), task: null, page: true };
  if (first === "p" && Number(second) && third === "checks") return { screen: "task", id: Number(second), task: null, checks: true };
  if (first === "p" && Number(second) && third === "usage") return { screen: "task", id: Number(second), task: null, usage: true };
  if (first === "p" && Number(second) && third === "routines") return { screen: "task", id: Number(second), task: null, routines: true };
  if (first === "p" && Number(second) && third === "panels") return { screen: "task", id: Number(second), task: null, panels: true };
  if (first === "p" && Number(second) && third === "r" && fourth) return { screen: "role", id: Number(second), role: decodeURIComponent(fourth) };
  if (first === "p" && Number(second)) {
    // ^ `#/p/<project>/files/<path>` opens that file (a manager's message about a routine links it).
    if (third === "files" && fourth) {
      try {
        return { screen: "files", id: Number(second), file: hash.split("/").slice(4).map(decodeURIComponent).join("/") };
      } catch {
        return { screen: "files", id: Number(second) };
      }
    }
    return { screen: { files: "files", sessions: "sessions" }[third] ?? "project", id: Number(second) };
  }
  if (first === "settings") return { screen: first, sub: second };
  if (first === "projects" || first === "machine") return { screen: first };
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

// ^ Back returns to the screen the reader came from, as the system's swipe back does: both are
// history.back(). Each history entry of this page records its depth and the hash before it; an
// entry opened from outside (a link or a notification) has depth 0, and Back there replaces it
// with its fallback, so a swipe after it does not return (docs/web-client.md "Screens").
let depth = 0;
// ^ A list's scroll place, per entry (depth and hash): its screen remounts on a return, and the
// browser restores only the window's scroll. Held until a reload, one value per entry.
const places = new Map();
// ^ A hash change that a popstate started is a return; the viewer's popstate changes no hash.
let popped = null, returning = false;
const screenScroll = () => document.querySelector("#main > .page > .scroll");
export function trackHistory() {
  const known = history.state?.wsx ? history.state : { wsx: true, depth: 0, from: null };
  depth = known.depth;
  history.replaceState(known, "");
  addEventListener("popstate", () => { popped = location.href; });
  addEventListener("hashchange", (event) => {
    returning = popped === location.href;
    popped = null;
    const from = new URL(event.oldURL).hash || "#/";
    // ^ The old screen is still drawn: the route renders after this event.
    const scroll = screenScroll();
    if (scroll) places.set(`${depth}${from}`, scroll.scrollTop);
    if (history.state?.wsx) {
      depth = history.state.depth;
      return;
    }
    depth += 1;
    history.replaceState({ wsx: true, depth, from }, "");
  });
}
// After a route renders: a return through history puts the list where it was left.
export function restorePlace() {
  if (!returning) return;
  returning = false;
  const scroll = screenScroll();
  const place = places.get(`${depth}${location.hash || "#/"}`);
  if (scroll && place) scroll.scrollTop = place;
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
// ^ The one Back: the top row's Back, a sheet's close (its Back, Escape, a tap beside it, a swipe
// down) and the system's swipe back all take the same step.
export function goBack(fallback) {
  if (previous()) history.back();
  else replaceHash(fallback);
}
// Switch a section or a view of the same screen (a tab, a Settings section, Chat or Terminal)
// without a new history entry, so Back leaves the section instead of walking through the taps.
export function replaceRoute(event, hash) {
  event.preventDefault();
  leaveFor(hash);
}
// Go to `hash` without a new entry: Back when the previous entry is `hash`, else replace this
// entry, so the next Back does not return to the screen left.
export function leaveFor(hash) {
  if (previous() === hash) history.back();
  else replaceHash(hash);
}
function replaceHash(hash) {
  if (location.hash === hash) return;
  const oldURL = location.href;
  history.replaceState(history.state, "", hash);
  dispatchEvent(new HashChangeEvent("hashchange", { oldURL, newURL: location.href }));
}
