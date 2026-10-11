// Page shell: pairing gate, panel navigation, and event fan-out to the open view.
import { api, openEvents } from "./api.js";
import { fill, h } from "./dom.js";
import { pairView } from "./pair.js";
import { chatView } from "./chat.js";
import { filesView } from "./files.js";
import { terminalView } from "./terminal.js";
import { activityView } from "./activity.js";
import { settingsView } from "./settings.js";

const main = document.getElementById("main");
const notice = document.getElementById("notice");
const VIEWS = { chat: chatView, files: filesView, terminal: terminalView, activity: activityView, settings: settingsView };
const LABELS = { chat: "Chat", files: "Files", terminal: "Terminal", activity: "Activity", settings: "Settings" };

// One shared state object; views read it and never keep their own copy of it.
const app = {
  site: null,
  device: null,
  sessions: [],
  daemon: true,
  can: (capability) => app.site.capabilities.includes(capability),
  navigate: (hash) => {
    location.hash = hash;
  },
};
let view = null;

function setNotice(text) {
  notice.textContent = text ?? "";
  notice.hidden = !text;
}

function route() {
  const [, panel = "", argument] = location.hash.split("/");
  const panels = [...app.site.panels, "settings"];
  const name = panels.includes(panel) ? panel : panels[0];
  view?.close?.();
  for (const link of document.querySelectorAll("#nav a")) {
    if (link.dataset.panel === name) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  main.replaceChildren();
  view = VIEWS[name](app, main, argument);
  document.title = `${LABELS[name]} - ${app.site.name}`;
}

function start(session) {
  app.site = session.site;
  app.device = session.device;
  document.getElementById("bar").hidden = false;
  document.getElementById("nav").replaceChildren(
    ...[...app.site.panels, "settings"].map((panel) =>
      h("a", { href: `#/${panel}`, "data-panel": panel }, LABELS[panel]),
    ),
  );
  window.addEventListener("hashchange", route);
  openEvents({
    workspace: (data) => {
      app.sessions = data.sessions;
      app.daemon = true;
      setNotice(null);
      view?.onWorkspace?.();
    },
    history: (data) => view?.onHistory?.(data.session_id),
    exchange: (data) => view?.onExchange?.(data),
    daemon: (data) => {
      app.daemon = data.available;
      setNotice(data.available ? null : "wsx is not running on the host. Sessions will return when it starts.");
      if (!data.available) {
        app.sessions = [];
        view?.onWorkspace?.();
      }
    },
    resync: () => route(),
    revoked: () => location.reload(),
    down: () => setNotice("Connection lost. Reconnecting…"),
    up: () => setNotice(app.daemon ? null : notice.textContent),
  });
  route();
  if ("serviceWorker" in navigator) {
    navigator.serviceWorker.register("sw.js", { scope: "./" }).catch(() => {});
  }
}

async function boot() {
  try {
    start(await api("api/session"));
  } catch (error) {
    if (error.status === 401) {
      pairView(main, () => boot());
    } else {
      fill(main,
        h("p", { class: "error", role: "alert" }, error.message),
        h("button", { onclick: () => location.reload() }, "Try again"),
      );
    }
  }
}

boot();
