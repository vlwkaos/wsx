// Chat panel. History is the provider's own saved conversation; the status line
// repeats the server's delivery evidence in its own words.
import { api, resolve } from "./api.js";
import { add, fill, h, sessionSelect, size, stateLabel } from "./dom.js";

// Unsent text, attachments and the message awaiting history, per session.
// Kept outside the view so a route change or a resync does not lose them.
const drafts = new Map();
const draftFor = (id) => {
  if (!drafts.has(id)) drafts.set(id, { text: "", attachments: [], pending: null });
  return drafts.get(id);
};

function attachmentList(attachments) {
  if (!attachments?.length) return null;
  return h(
    "ul",
    { class: "attachments" },
    attachments.map((file) =>
      h(
        "li",
        {},
        file.image
          ? h("a", { href: resolve(file.url) }, h("img", { src: resolve(file.url), alt: file.name, loading: "lazy" }))
          : h("a", { href: resolve(file.url), download: file.name }, `${file.name} (${size(file.size)})`),
      ),
    ),
  );
}

function messageNode(message, pending) {
  const who = message.role === "user" ? "You" : message.role === "assistant" ? "Agent" : message.role;
  return h(
    "article",
    { class: `bubble ${message.role === "user" ? "bubble-user" : "bubble-agent"}`, "data-role": message.role, "data-pending": pending ? "" : null },
    h("p", { class: "who" }, pending ? `${who} (not in the saved conversation yet)` : who),
    message.text ? h("p", { class: "text" }, message.text) : null,
    attachmentList(message.attachments),
  );
}

const HISTORY_WORDS = {
  no_agent: "No agent runs in this session.",
  identity_unavailable: "The agent has not reported its conversation yet.",
  unsupported_provider: "This agent type has no readable saved conversation.",
  history_not_found: "The saved conversation file was not found.",
  no_readable_messages: "The conversation is empty.",
};

export function chatView(app, main, argument) {
  let current = Number(argument) || null;
  let loaded = null;
  let closed = false;
  let generation = 0;

  const picker = h("div", { class: "row" });
  const log = h("div", { class: "log", role: "log", "aria-label": "Conversation", tabIndex: 0 });
  const status = h("p", { class: "small muted", role: "status" });
  const error = h("p", { class: "error", role: "alert" });
  const chips = h("ul", { class: "chips", "aria-label": "Files to send" });
  const text = h("textarea", {
    rows: 2, "aria-label": "Message", placeholder: "Write a message",
    oninput: () => { if (current) draftFor(current).text = text.value; },
    onkeydown: (event) => {
      // Enter stays a newline. Ctrl or Cmd with Enter sends.
      if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) { event.preventDefault(); send(); }
    },
  });
  const file = h("input", { type: "file", multiple: true, hidden: true, onchange: () => upload([...file.files]) });
  const attach = h("button", { type: "button", onclick: () => file.click() }, "Attach");
  const sendButton = h("button", { class: "button-primary", type: "button", onclick: () => send() }, "Send");
  const cancelButton = h("button", { type: "button", hidden: true, onclick: () => cancel() }, "Stop");
  const composer = h(
    "div",
    { class: "composer" },
    chips, text, error,
    h("div", { class: "row" }, app.can("media_upload") ? attach : null, file, h("span", { class: "grow" }), cancelButton, sendButton),
  );
  add(main, picker, log, status, app.can("chat_send") ? composer : null);

  function renderPicker() {
    const session = app.sessions.find((candidate) => candidate.id === current);
    const start = app.can("session_start")
      ? h("button", { type: "button", onclick: startSession }, "New session")
      : null;
    if (!app.sessions.length) {
      fill(picker,
        h("p", { class: "muted grow" }, app.daemon ? "No session is open in this project." : "wsx is not running on the host."),
        start,
      );
      return;
    }
    fill(picker,
      h("div", { class: "grow" }, sessionSelect(app.sessions, current, (id) => app.navigate(`#/chat/${id}`))),
      session ? stateLabel(session) : null,
      start,
    );
  }

  function renderChips() {
    const draft = draftFor(current);
    fill(chips,
      ...draft.attachments.map((item) =>
        h("li", {}, h("span", {}, `${item.name} (${size(item.size)})`),
          h("button", {
            type: "button", "aria-label": `Remove ${item.name}`,
            onclick: () => { draft.attachments = draft.attachments.filter((other) => other !== item); renderChips(); },
          }, "Remove")),
      ),
    );
  }

  function renderStatus() {
    const exchange = loaded?.exchange;
    const session = app.sessions.find((candidate) => candidate.id === current);
    const blocked = session?.agent?.state === "blocked";
    cancelButton.hidden = !exchange?.active;
    sendButton.disabled = !session?.agent?.prompt || blocked;
    const parts = [];
    if (blocked) parts.push(app.site.panels.includes("terminal") ? "The agent is waiting for an answer. Open Terminal to answer it." : "The agent is waiting for an answer on the host.");
    else if (session && !session.agent?.prompt) parts.push("This session cannot receive messages.");
    if (exchange) parts.push(`Last message: ${exchange.delivery} (${exchange.state.replaceAll("_", " ")}).`);
    status.textContent = parts.join(" ");
  }

  function renderLog() {
    const draft = draftFor(current);
    const last = loaded.messages.filter((message) => message.role === "user").at(-1);
    // The saved conversation now holds the sent message: stop showing it as pending.
    if (draft.pending && last?.text === draft.pending.text) draft.pending = null;
    const nearEnd = log.scrollHeight - log.scrollTop - log.clientHeight < 80;
    const nodes = loaded.messages.map((message) => messageNode(message, false));
    if (loaded.checkpoint) {
      nodes.unshift(h("details", {}, h("summary", { class: "muted small" }, "Earlier conversation (summary)"), h("p", { class: "text small" }, loaded.checkpoint)));
    } else if (loaded.truncated) {
      nodes.unshift(h("p", { class: "muted small" }, "Older messages are not shown."));
    }
    if (!loaded.messages.length) nodes.push(h("p", { class: "muted" }, HISTORY_WORDS[loaded.status] ?? "No messages yet."));
    if (draft.pending) nodes.push(messageNode({ role: "user", ...draft.pending }, true));
    fill(log, ...nodes);
    if (nearEnd) log.scrollTop = log.scrollHeight;
  }

  async function load() {
    if (!current) return;
    const mine = ++generation;
    try {
      const data = await api(`api/chat/${current}/history`);
      // A slower earlier response must not replace a newer one.
      if (closed || mine !== generation) return;
      loaded = data;
      renderLog();
      renderStatus();
    } catch (failure) {
      if (closed || mine !== generation) return;
      fill(log, h("p", { class: "error" }, failure.message));
    }
  }

  async function upload(files) {
    file.value = "";
    const draft = draftFor(current);
    error.textContent = "";
    for (const item of files) {
      if (item.size > app.site.max_file_bytes) {
        error.textContent = `${item.name} is larger than ${size(app.site.max_file_bytes)}.`;
        continue;
      }
      attach.disabled = true;
      try {
        draft.attachments.push(await api("api/media", { method: "PUT", query: { name: item.name }, body: item }));
      } catch (failure) {
        error.textContent = `${item.name}: ${failure.message}`;
      }
      attach.disabled = false;
      renderChips();
    }
  }

  async function send() {
    const draft = draftFor(current);
    const body = text.value.trim();
    if (!current || sendButton.disabled || (!body && !draft.attachments.length)) return;
    sendButton.disabled = true;
    error.textContent = "";
    try {
      const exchange = await api(`api/chat/${current}/send`, {
        method: "POST",
        json: { text: body, attachments: draft.attachments.map((item) => item.hash) },
      });
      draft.pending = { text: body, attachments: draft.attachments };
      draft.text = "";
      draft.attachments = [];
      text.value = "";
      renderChips();
      if (loaded) { loaded.exchange = exchange; renderLog(); log.scrollTop = log.scrollHeight; }
    } catch (failure) {
      // The draft stays in the box; nothing was sent.
      error.textContent = failure.message;
    }
    renderStatus();
  }

  async function cancel() {
    try {
      loaded.exchange = await api(`api/chat/${current}/cancel`, { method: "POST", json: {} });
    } catch (failure) {
      error.textContent = failure.message;
    }
    renderStatus();
  }

  async function startSession() {
    try {
      const created = await api("api/sessions", { method: "POST", json: { label: "chat" } });
      app.navigate(`#/chat/${created.session_id}`);
    } catch (failure) {
      status.textContent = failure.message;
    }
  }

  function select() {
    if (!app.sessions.some((session) => session.id === current)) {
      const next = app.sessions[0]?.id ?? null;
      if (next !== current) { current = next; loaded = null; log.replaceChildren(); }
    }
    renderPicker();
    composer.hidden = !current;
    if (current) {
      text.value = draftFor(current).text;
      renderChips();
      renderStatus();
      if (!loaded) load();
    }
  }
  select();

  return {
    close: () => { closed = true; },
    onWorkspace: () => select(),
    onHistory: (id) => { if (id === current) load(); },
    onExchange: (exchange) => {
      if (exchange.session_id !== current || !loaded) return;
      loaded.exchange = exchange;
      renderStatus();
    },
  };
}
