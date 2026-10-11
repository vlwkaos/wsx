// Terminal panel: the pane's current screen as text, refreshed on events.
// It is a snapshot, not a live terminal emulator.
import { api } from "./api.js";
import { add, fill, h, sessionSelect, stateLabel } from "./dom.js";

const KEYS = [["enter", "Enter"], ["escape", "Esc"], ["tab", "Tab"], ["up", "Up"], ["down", "Down"], ["ctrl_c", "Ctrl+C"]];

export function terminalView(app, main, argument) {
  let current = Number(argument) || null;
  let closed = false;
  let generation = 0;
  const picker = h("div", { class: "row" });
  const screen = h("pre", { class: "screen", tabIndex: 0, "aria-label": "Terminal screen" });
  const error = h("p", { class: "error", role: "alert" });
  const text = h("input", {
    type: "text", class: "grow", "aria-label": "Text to type", autocapitalize: "off", autocomplete: "off", spellcheck: false,
    onkeydown: (event) => { if (event.key === "Enter") { event.preventDefault(); input({ text: text.value, key: "enter" }); } },
  });
  const controls = h(
    "div",
    {},
    h("div", { class: "row" }, text, h("button", { class: "primary", type: "button", onclick: () => input({ text: text.value, key: "enter" }) }, "Send line")),
    h("div", { class: "row", role: "group", "aria-label": "Keys" },
      KEYS.map(([key, label]) => h("button", { type: "button", onclick: () => input({ key }) }, label))),
  );
  add(main, picker, screen, error, app.can("terminal_input") ? controls : null);

  async function refresh() {
    if (!current) return;
    const mine = ++generation;
    try {
      const data = await api(`api/terminal/${current}`);
      if (closed || mine !== generation) return;
      screen.textContent = data.text || "(empty screen)";
      error.textContent = "";
    } catch (failure) {
      if (closed || mine !== generation) return;
      error.textContent = failure.message;
    }
  }

  async function input(body) {
    try {
      await api(`api/terminal/${current}/input`, { method: "POST", json: body });
      text.value = "";
      error.textContent = "";
      // The screen changes after the program reads the input; show the next frame once.
      setTimeout(refresh, 300);
    } catch (failure) {
      error.textContent = failure.message;
    }
  }

  function select() {
    if (!app.sessions.some((session) => session.id === current)) current = app.sessions[0]?.id ?? null;
    const session = app.sessions.find((candidate) => candidate.id === current);
    if (!session) {
      fill(picker, h("p", { class: "muted" }, "No session is open in this project."));
      screen.textContent = "";
      controls.hidden = true;
      return;
    }
    controls.hidden = false;
    fill(picker,
      h("div", { class: "grow" }, sessionSelect(app.sessions, current, (id) => app.navigate(`#/terminal/${id}`))),
      stateLabel(session),
      h("button", { type: "button", onclick: refresh }, "Refresh"),
    );
  }
  select();
  refresh();

  return {
    close: () => { closed = true; },
    onWorkspace: () => { select(); refresh(); },
    onHistory: (id) => { if (id === current) refresh(); },
    onExchange: (exchange) => { if (exchange.session_id === current) refresh(); },
  };
}
