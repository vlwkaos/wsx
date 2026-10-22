// Page shell: pairing gate, event stream, routes, and the two layouts.
// Phone: top-level screens share a bottom tab bar; a session owns the full height.
// Wide (900 px and more): a left rail with the home list (the inbox or the projects without
// task_view), the rest beside it.
import { api, openEvents, pageId } from "./api.js";
import { html, render, useEffect, useRef, useState } from "./lib.js";
import { can, events, noteBuild, parseRoute, shows, stackUnder, state, trackHistory, update, useRoute, useStore } from "./store.js";
import { Home } from "./home.js";
import { applyScheme, Notices, toast } from "./ui.js";
import { Pair } from "./pair.js";
import { Inbox } from "./inbox.js";
import { Project, Projects, ProjectSessions } from "./projects.js";
import { loadTasks, Manager, noteDecisions } from "./tasks.js";
import { Session } from "./session.js";
import { Files } from "./files.js";
import { Settings } from "./settings.js";

// ^ With task_view home lists the projects, one row each, and Sessions (the inbox, shells
// too) stays one tap away beside it, so work that no task owns is never hidden. A project's
// worktrees are in its chat's ⋯ menu (docs/web-client.md "Screens").
const tabs = () => can("task_view")
  ? [["home", "#/", "Home"], ["inbox", "#/sessions", "Sessions"], ["settings", "#/settings", "Settings"]]
  : [["home", "#/", "Inbox"], ["projects", "#/projects", "Projects"], ["settings", "#/settings", "Settings"]];

function useWide() {
  const query = matchMedia("(min-width: 900px)");
  const [wide, setWide] = useState(query.matches);
  useEffect(() => {
    const changed = () => setWide(query.matches);
    query.addEventListener("change", changed);
    return () => query.removeEventListener("change", changed);
  }, []);
  return wide;
}

// ^ iOS Safari and an installed PWA keep the layout viewport when the keyboard opens:
// only the visual viewport shrinks, and it can move down the page. While a field takes
// typing, the page is pinned to the visual viewport, so a composer or key row sits on the
// keyboard's top edge. Otherwise the page is the full screen (100dvh): iOS does not always
// tell the page that the keyboard closed (a route change, a return from the background, a
// page from the back-forward cache), and a pinned page then kept half the screen empty.
const SHORT_HEIGHT = 560;
const TYPING = "textarea, select, [contenteditable=''], [contenteditable=true], input:not([type]), "
  + "input[type=text], input[type=search], input[type=email], input[type=url], input[type=tel], input[type=password], input[type=number]";
function followViewport() {
  const viewport = window.visualViewport;
  if (!viewport) return;
  const changed = () => {
    const style = document.documentElement.style;
    const typing = !!document.activeElement?.matches(TYPING);
    if (typing) {
      style.setProperty("--viewport-height", `${viewport.height}px`);
      style.setProperty("--viewport-top", `${viewport.offsetTop}px`);
    } else {
      style.removeProperty("--viewport-height");
      style.removeProperty("--viewport-top");
    }
    // The keyboard covers the bottom safe area, so its inset must not stay as a gap.
    const keyboard = typing && viewport.scale <= 1.01 && document.documentElement.clientHeight - viewport.height > 120;
    document.body.toggleAttribute("data-keyboard", keyboard);
    // Little visible height, with the keyboard up or on a short screen: the `/` list keeps
    // to fewer rows so the conversation stays in view (docs/web-client.md "Commands").
    document.body.toggleAttribute("data-short", viewport.height < SHORT_HEIGHT);
  };
  // The focus has moved only after focusout, and the keyboard closes over about 300 ms.
  const settle = () => { setTimeout(changed, 0); setTimeout(changed, 400); };
  viewport.addEventListener("resize", changed);
  viewport.addEventListener("scroll", changed);
  window.addEventListener("resize", changed);
  window.addEventListener("pageshow", changed);
  document.addEventListener("visibilitychange", changed);
  document.addEventListener("focusin", changed);
  document.addEventListener("focusout", settle);
  changed();
}

const Tabs = ({ current, rail }) => html`
  <nav class=${rail ? "rail-tabs" : "tabbar"} aria-label="Screens">
    ${tabs().map(([name, href, label]) => html`
      <a href=${href} aria-current=${current === name ? "page" : null}>${label}</a>`)}
  </nav>`;

function Screen({ route }) {
  if (route.screen === "session") return html`<${Session} key=${route.id} id=${route.id} tab=${route.tab} />`;
  if (route.screen === "files") return html`<${Files} key=${route.id} project=${route.id} />`;
  // ^ The chat and the chat with a task's sheet are one Manager, so opening and closing the
  // sheet keeps the conversation, its scroll and the composer's draft.
  if (route.screen === "project" || route.screen === "task") {
    return can("task_view") ? html`<${Manager} key=${route.id} project=${route.id} task=${route.task ?? null} page=${Boolean(route.page)} checks=${Boolean(route.checks)} />` : html`<${Project} key=${route.id} id=${route.id} />`;
  }
  if (route.screen === "sessions") return html`<${ProjectSessions} key=${route.id} id=${route.id} />`;
  if (route.screen === "settings") return html`<${Settings} />`;
  if (route.screen === "projects") return html`<${Projects} />`;
  if (route.screen === "inbox" || !can("task_view")) return html`<${Inbox} />`;
  return html`<${Home} />`;
}

function App() {
  useStore();
  const route = useRoute();
  const wide = useWide();
  const lastList = useRef("home");
  const top = tabs().some(([name]) => name === route.screen) || (route.screen === "inbox" && !can("task_view"));
  useEffect(() => {
    const titles = { home: can("task_view") ? "Home" : "Inbox", inbox: can("task_view") ? "Sessions" : "Inbox",
      projects: "Projects", settings: "Settings", files: "Files" };
    const session = route.screen === "session" && state.sessions.find((candidate) => candidate.id === route.id);
    const project = ["project", "sessions", "task"].includes(route.screen) && state.projects.find((candidate) => candidate.id === route.id);
    document.title = `${session ? session.label : project ? project.name : titles[route.screen] ?? "Session"} - ${state.site.name}`;
  });
  if (!wide) {
    return html`
      <div class="app">
        <${Notices} />
        <main class="screen" id="main"><${Screen} route=${route} /></main>
        ${top && html`<${Tabs} current=${route.screen} />`}
      </div>`;
  }
  // The rail keeps the list the main area was reached from. With task_view a session or the
  // files keep the list they were opened from, and a project's chat has home.
  let list;
  if (!can("task_view")) list = ["projects", "project", "files", "sessions", "task"].includes(route.screen) ? "projects" : "inbox";
  else if (["home", "project", "task", "projects", "sessions"].includes(route.screen)) list = "home";
  else if (route.screen === "inbox") list = "inbox";
  else list = lastList.current;
  lastList.current = list;
  const railList = { home: html`<${Home} rail />`, projects: html`<${Projects} rail />`, inbox: html`<${Inbox} rail />` }[list];
  const listScreen = route.screen === "home" || route.screen === "projects" || route.screen === "inbox";
  const railTab = can("task_view") || list !== "inbox" ? list : "home";
  return html`
    <div class="app" data-wide>
      <aside class="rail">
        <div class="rail-head"><span class="rail-name">${state.site.name}</span><${Tabs} current=${top ? route.screen : railTab} rail /></div>
        <div class="rail-body">${railList}</div>
      </aside>
      <div class="stage">
        <${Notices} />
        <main class="screen" id="main">
          ${listScreen
            ? html`<p class="placeholder">${(!can("task_view") || list === "inbox") && (shows("chat") || shows("terminal")) ? "Choose a session." : "Choose a project."}</p>`
            : html`<${Screen} route=${route} />`}
        </main>
      </div>
    </div>`;
}

// The project this page shows while it is visible, so the server pushes no task
// notification for it to this device. Sent when it changes and when the stream reopens.
let shown;
function tellPresence(force) {
  if (!can("task_view") || !can("push")) return;
  const route = parseRoute(location.hash);
  const project = document.visibilityState === "visible" && ["project", "sessions", "task", "files"].includes(route.screen) ? route.id : null;
  if (project === shown && !force) return;
  shown = project;
  api("api/presence", { method: "POST", json: { page: pageId, project } }).catch(() => { shown = undefined; });
}

// A failed upgrade of the running wsxd: said once per reason, and it stays until dismissed.
// Sessions keep running on the old daemon (docs/web-client.md "Notices").
let upgradeSaid = null;
function noteUpgrade(reason) {
  if (!reason || reason === upgradeSaid) return;
  upgradeSaid = reason;
  toast(`wsxd upgrade failed: ${reason}; sessions keep running`, { failure: true });
}

function start(session) {
  update({ site: session.site, device: session.device, build: session.build });
  noteBuild(session.build?.id);
  noteUpgrade(session.upgrade_failed);
  openEvents({
    workspace: (data) => {
      update({ projects: data.projects, sessions: data.sessions, loaded: true, daemon: true });
      noteBuild(data.build);
    },
    history: (data) => events.dispatchEvent(new CustomEvent("history", { detail: data })),
    exchange: (data) => events.dispatchEvent(new CustomEvent("exchange", { detail: data })),
    daemon: (data) => {
      update(data.available ? { daemon: true } : { daemon: false, sessions: [], projects: [], loaded: true });
      noteUpgrade(data.upgrade_failed);
    },
    resync: () => {
      events.dispatchEvent(new CustomEvent("history", { detail: {} }));
      if (can("task_view")) loadTasks();
    },
    tasks: (data) => {
      noteDecisions(data);
      update({ tasks: { ...state.tasks, [data.project]: data } });
    },
    task: (data) => events.dispatchEvent(new CustomEvent("task", { detail: data })),
    manager: (data) => events.dispatchEvent(new CustomEvent("manager", { detail: data })),
    page: (data) => events.dispatchEvent(new CustomEvent("page", { detail: data })),
    revoked: () => location.reload(),
    down: () => update({ online: false }),
    // Tasks are read once the stream is open, so no change falls between the read and the
    // stream; a reconnected stream may have missed changes.
    up: () => {
      update({ online: true });
      if (can("task_view")) loadTasks();
      tellPresence(true);
    },
  });
  const opened = parseRoute(location.hash);
  if (opened.screen === "task" && can("task_view")) stackUnder(`#/p/${opened.id}`);
  addEventListener("hashchange", () => tellPresence(false));
  document.addEventListener("visibilitychange", () => tellPresence(false));
  // ^ index.html's "Loading…" placeholder is not Preact's: a first render adopts an
  // existing child only of the same element type, and the app's root is a div, so the
  // placeholder would stay beside the app and take half of the screen.
  const root = document.getElementById("app");
  root.replaceChildren();
  render(html`<${App} />`, root);
  if ("serviceWorker" in navigator) navigator.serviceWorker.register("sw.js", { scope: "./" }).catch(() => {});
}

async function boot() {
  const root = document.getElementById("app");
  // Only the session request decides between the app, pairing and the error page: a
  // fault inside the started app must not draw a second screen beside it.
  let session;
  try {
    session = await api("api/session");
  } catch (error) {
    if (error.status === 401) {
      render(html`<${Pair} onPaired=${() => { render(null, root); boot(); }} />`, root);
    } else {
      render(html`
        <main class="screen pair" id="main">
          <p class="error" role="alert">${error.message}</p>
          <button class="button" type="button" onClick=${() => location.reload()}>Try again</button>
        </main>`, root);
    }
    return;
  }
  start(session);
}

applyScheme();
followViewport();
trackHistory();
boot();
