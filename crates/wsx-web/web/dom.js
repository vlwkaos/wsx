// Element builder. Text is always set as text, never parsed as HTML.
export function h(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  // Theme hooks (docs/web-themes.md): every control carries its hook class.
  const hook = tag === "button" ? "button" : tag === "select" || tag === "textarea" || (tag === "input" && attributes.type === "text") ? "input" : null;
  if (hook) element.classList.add(hook);
  for (const [name, value] of Object.entries(attributes)) {
    if (value === false || value == null) continue;
    if (name.startsWith("on")) element.addEventListener(name.slice(2), value);
    else if (name === "class") element.classList.add(...value.split(" "));
    else if (name in element && name !== "list") element[name] = value;
    else element.setAttribute(name, value === true ? "" : value);
  }
  add(element, ...children);
  return element;
}

// Optional children are written as `condition ? node : null`; drop the gaps
// here so the DOM never renders the text "null".
const present = (children) => children.flat().filter((child) => child != null && child !== false);
export function add(parent, ...children) {
  parent.append(...present(children));
}
export function fill(parent, ...children) {
  parent.replaceChildren(...present(children));
}

const STATE_WORDS = {
  idle: "idle",
  working: "working",
  blocked: "needs input",
  done: "done",
  error: "error",
  unknown: "state unknown",
};

// State is text first; color only repeats it.
export function stateLabel(session) {
  if (session.exited) return h("span", { class: "badge", "data-state": "error" }, "exited");
  if (!session.agent) return h("span", { class: "badge muted" }, "no agent");
  const state = session.agent.attached ? session.agent.state : "unknown";
  return h("span", { class: "badge", "data-state": state }, `${session.agent.provider}: ${STATE_WORDS[state] ?? state}`);
}

export function sessionSelect(sessions, current, onchange) {
  return h(
    "select",
    { "aria-label": "Session", onchange: (event) => onchange(Number(event.target.value)) },
    sessions.map((session) =>
      h("option", { value: String(session.id), selected: session.id === current }, `${session.label} (${session.branch})`),
    ),
  );
}

export function size(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
