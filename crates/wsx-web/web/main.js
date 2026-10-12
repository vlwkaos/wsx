// Page shell: pairing gate, event stream, routes, and the two layouts.
// Phone: top-level screens share a bottom tab bar; a session owns the full height.
// Wide (900 px and more): a left rail with the inbox or the projects, the rest beside it.
import { api, openEvents } from "./api.js";
import { html, render, useEffect, useState } from "./lib.js";
import { events, shows, state, update, useRoute, useStore } from "./store.js";
import { applyScheme, Notices } from "./ui.js";
import { Pair } from "./pair.js";
import { Inbox } from "./inbox.js";
import { Projects } from "./projects.js";
import { Session } from "./session.js";
import { Files } from "./files.js";
import { Settings } from "./settings.js";

const TABS = [["inbox", "#/", "Inbox"], ["projects", "#/projects", "Projects"], ["settings", "#/settings", "Settings"]];

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
// only the visual viewport shrinks, and it can move down the page. The page is pinned
// to the visual viewport, so a composer or key row sits on the keyboard's top edge.
function followViewport() {
  const viewport = window.visualViewport;
  if (!viewport) return;
  const changed = () => {
    const style = document.documentElement.style;
    style.setProperty("--viewport-height", `${viewport.height}px`);
    style.setProperty("--viewport-top", `${viewport.offsetTop}px`);
    // The keyboard covers the bottom safe area, so its inset must not stay as a gap.
    const keyboard = viewport.scale <= 1.01 && document.documentElement.clientHeight - viewport.height > 120;
    document.body.toggleAttribute("data-keyboard", keyboard);
  };
  viewport.addEventListener("resize", changed);
  viewport.addEventListener("scroll", changed);
  changed();
}

const Tabs = ({ current, rail }) => html`
  <nav class=${rail ? "rail-tabs" : "tabbar"} aria-label="Screens">
    ${TABS.map(([name, href, label]) => html`
      <a href=${href} aria-current=${current === name ? "page" : null}>${label}</a>`)}
  </nav>`;

function Screen({ route }) {
  if (route.screen === "session") return html`<${Session} key=${route.id} id=${route.id} tab=${route.tab} />`;
  if (route.screen === "files") return html`<${Files} key=${route.id} project=${route.id} />`;
  if (route.screen === "settings") return html`<${Settings} />`;
  if (route.screen === "projects") return html`<${Projects} />`;
  return html`<${Inbox} />`;
}

function App() {
  useStore();
  const route = useRoute();
  const wide = useWide();
  const top = TABS.some(([name]) => name === route.screen);
  useEffect(() => {
    const titles = { inbox: "Inbox", projects: "Projects", settings: "Settings", files: "Files" };
    const session = route.screen === "session" && state.sessions.find((candidate) => candidate.id === route.id);
    document.title = `${session ? session.label : titles[route.screen] ?? "Session"} - ${state.site.name}`;
  });
  if (!wide) {
    return html`
      <div class="app">
        <${Notices} />
        <main class="screen" id="main"><${Screen} route=${route} /></main>
        ${top && html`<${Tabs} current=${route.screen} />`}
      </div>`;
  }
  // The rail keeps the list the main area was reached from.
  const list = route.screen === "projects" || route.screen === "files" ? "projects" : "inbox";
  return html`
    <div class="app" data-wide>
      <aside class="rail">
        <div class="rail-head"><span class="rail-name">${state.site.name}</span><${Tabs} current=${top ? route.screen : list} rail /></div>
        <div class="rail-body">${list === "projects" ? html`<${Projects} rail />` : html`<${Inbox} rail />`}</div>
      </aside>
      <div class="stage">
        <${Notices} />
        <main class="screen" id="main">
          ${route.screen === "inbox" || route.screen === "projects"
            ? html`<p class="placeholder">${shows("chat") || shows("terminal") ? "Choose a session." : "Choose a project."}</p>`
            : html`<${Screen} route=${route} />`}
        </main>
      </div>
    </div>`;
}

function start(session) {
  update({ site: session.site, device: session.device });
  openEvents({
    workspace: (data) => update({ projects: data.projects, sessions: data.sessions, loaded: true, daemon: true }),
    history: (data) => events.dispatchEvent(new CustomEvent("history", { detail: data })),
    exchange: (data) => events.dispatchEvent(new CustomEvent("exchange", { detail: data })),
    daemon: (data) => update(data.available ? { daemon: true } : { daemon: false, sessions: [], projects: [], loaded: true }),
    resync: () => events.dispatchEvent(new CustomEvent("history", { detail: {} })),
    revoked: () => location.reload(),
    down: () => update({ online: false }),
    up: () => update({ online: true }),
  });
  render(html`<${App} />`, document.getElementById("app"));
  if ("serviceWorker" in navigator) navigator.serviceWorker.register("sw.js", { scope: "./" }).catch(() => {});
}

async function boot() {
  const root = document.getElementById("app");
  try {
    start(await api("api/session"));
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
  }
}

applyScheme();
followViewport();
boot();
