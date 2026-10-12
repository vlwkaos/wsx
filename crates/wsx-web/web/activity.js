// Activity panel: sessions of the project and the recent messages sent to them.
import { api } from "./api.js";
import { add, fill, h, stateLabel } from "./dom.js";

export function activityView(app, main) {
  let closed = false;
  const sessions = h("div", {});
  const exchanges = h("div", {});
  add(main, h("h2", {}, "Sessions"), sessions, h("h2", {}, "Recent messages"), exchanges);

  async function load() {
    try {
      const data = await api("api/activity");
      if (closed) return;
      const open = app.site.panels.includes("chat");
      fill(sessions,
        data.sessions.length
          ? h("ul", { class: "list" }, data.sessions.map((session) =>
              h("li", { class: "list-row line" },
                open ? h("a", { href: `#/chat/${session.id}` }, `${session.label} (${session.branch})`) : h("span", {}, `${session.label} (${session.branch})`),
                stateLabel(session))))
          : h("p", { class: "muted" }, "No session is open in this project."),
      );
      const label = new Map(data.sessions.map((session) => [session.id, session.label]));
      fill(exchanges,
        data.exchanges.length
          ? h("ul", { class: "list" }, data.exchanges.map((exchange) =>
              h("li", { class: "list-row line" },
                h("span", {}, `${label.get(exchange.session_id) ?? `session ${exchange.session_id}`}, message ${exchange.id} round ${exchange.round}`),
                h("span", { class: "small muted" }, `${exchange.state.replaceAll("_", " ")}: ${exchange.delivery}`))))
          : h("p", { class: "muted" }, "No message was sent yet."),
      );
    } catch (failure) {
      fill(sessions, h("p", { class: "error", role: "alert" }, failure.message));
    }
  }
  load();
  return { close: () => { closed = true; }, onWorkspace: load, onExchange: load };
}
