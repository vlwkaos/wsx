// Chat: the agent's own saved conversation as a typed timeline, what the agent does
// now, the messages that wait for it, and the composer (docs/web-client.md "Chat").
import { api, resolve } from "./api.js";
import { bytes, elapsed, html, Icon, useCallback, useEffect, useLayoutEffect, useNow, useRef, useState } from "./lib.js";
import { can, events, shows, stateOf, state, touch } from "./store.js";
import { Markdown } from "./markdown.js";

// Unsent text, attachments and the message that is not in the saved conversation
// yet, per session. Kept outside the view so a route change does not lose them.
const drafts = new Map();
const draftFor = (id) => {
  if (!drafts.has(id)) drafts.set(id, { text: "", attachments: [], pending: null });
  return drafts.get(id);
};

const EMPTY_WORDS = {
  no_agent: "No agent runs in this session.",
  identity_unavailable: "The agent has not reported its conversation yet.",
  unsupported_provider: "This agent type has no readable saved conversation.",
  history_not_found: "The saved conversation file was not found.",
  history_unreadable: "No messages yet.",
  no_readable_messages: "No messages yet.",
};
// Exchange states where the last sent message did not go the expected way.
const DELIVERY_WORDS = {
  delivery_failed: "Not delivered to the agent.",
  cancelled: "Stopped.",
  cancel_requested: "Stopping…",
  expired: "The agent did not finish in time.",
  interrupted: "The turn was interrupted.",
  target_exited: "The agent's program exited.",
  target_replaced: "The agent's program was replaced.",
};

const Attachments = ({ files }) => files?.length ? html`
  <ul class="attachments">
    ${files.map((file) => html`
      <li key=${file.hash}>
        ${file.image
          ? html`<a href=${resolve(file.url)}><img src=${resolve(file.url)} alt=${file.name} loading="lazy" /></a>`
          : html`<a href=${resolve(file.url)} download=${file.name}>${file.name} (${bytes(file.size)})</a>`}
      </li>`)}
  </ul>` : null;

const stepWord = (item) => item.kind === "thinking" ? "Thinking" : item.tool.name;
const Step = ({ item }) => html`
  <li class="step">
    <span class="step-title">${item.kind === "thinking" ? "Thinking" : item.tool.title}</span>
    ${item.tool?.status === "error" && html`<span class="step-state" data-tone="danger">failed</span>`}
  </li>`;

// Consecutive tool and thinking items between two messages are one line. The run that
// is still going shows its last three finished items: the step that runs now is in
// the working row and nowhere else.
function Steps({ items, live }) {
  if (live) {
    const finished = items.filter((item) => item.tool?.status !== "running");
    // With no tool running, a last thinking item is what the agent does now.
    items = finished.length === items.length && items.at(-1).kind === "thinking" ? items.slice(0, -1) : finished;
    if (!items.length) return null;
  }
  const names = [...new Set(items.map(stepWord))];
  const summary = `${items.length} ${items.length === 1 ? "step" : "steps"}: ${names.join(", ")}`;
  if (live) {
    const earlier = items.length - 3;
    return html`
      <div class="steps" data-live>
        ${earlier > 0 && html`<p class="steps-summary">${earlier} earlier ${earlier === 1 ? "step" : "steps"}</p>`}
        <ul class="step-list">${items.slice(-3).map((item) => html`<${Step} key=${item.id} item=${item} />`)}</ul>
      </div>`;
  }
  return html`
    <details class="steps">
      <summary class="steps-summary"><${Icon} name="chevron" />${summary}</summary>
      <ul class="step-list">${items.map((item) => html`<${Step} key=${item.id} item=${item} />`)}</ul>
    </details>`;
}

// Messages as they are, with each run of steps between them as one block.
function blocks(items) {
  const result = [];
  for (const item of items) {
    const last = result.at(-1);
    if (item.kind === "tool" || item.kind === "thinking") {
      if (last?.steps) last.steps.push(item);
      else result.push({ id: `steps-${item.id}`, steps: [item] });
    } else if (item.text || item.attachments?.length) {
      result.push(item);
    }
  }
  return result;
}

// The agent's commands for the `/` list, read once per session while the page is open.
const commandLists = new Map();
function loadCommands(id) {
  if (!commandLists.has(id)) {
    commandLists.set(id, api(`api/chat/${id}/commands`).catch((failure) => {
      commandLists.delete(id);
      throw failure;
    }));
  }
  return commandLists.get(id);
}

// While the field holds only `/` and a partial name: the commands that match it, names
// that start with it first. `null` when no list applies.
function matching(listing, text) {
  const typed = /^\/(\S*)$/.exec(text);
  if (!listing || !typed) return null;
  const query = typed[1].toLowerCase();
  const starts = listing.commands.filter((command) => command.name.toLowerCase().startsWith(query));
  const within = listing.commands.filter((command) => !starts.includes(command) && command.name.toLowerCase().includes(query));
  return [...starts, ...within];
}

const SOURCE_WORDS = { builtin: "built-in", project: "project", user: "yours", plugin: "plugin" };

// The `/` list above the composer row: a listbox that the field controls. A pointer
// press does not move focus, so the on-screen keyboard stays open.
const CommandList = ({ listing, matches, active, onPick, onHover }) => html`
  <div class="slash">
    <ul class="slash-list" id="slash-list" role="listbox" aria-label="Commands">
      ${matches.map((command, index) => html`
        <li key=${command.name} id=${`slash-${index}`} class="slash-option" role="option" aria-selected=${index === active}
          onPointerDown=${(event) => event.preventDefault()} onPointerEnter=${() => onHover(index)} onClick=${() => onPick(command)}>
          <span class="slash-name">/${command.name}</span>
          <span class="slash-description">${command.description}</span>
          <span class="slash-source">${SOURCE_WORDS[command.source]}</span>
        </li>`)}
    </ul>
    ${!matches.length && html`<p class="note slash-note">No command starts with that.</p>`}
    ${!listing.listed && html`<p class="note slash-note">Only the built-in commands of ${listing.provider || "this agent"} are listed.</p>`}
    ${!listing.runs && html`<p class="note slash-note">wsx on the host must be upgraded to run a command from here.</p>`}
  </div>`;

function Composer({ session, working, onSent }) {
  const draft = draftFor(session.id);
  const [, renew] = useState(0);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const field = useRef();
  const picker = useRef();
  const ready = Boolean(session.agent?.attached && session.agent.prompt) && !session.exited;
  const [listing, setListing] = useState(null);
  const [active, setActive] = useState(0);
  // Escape closes the list until the text changes.
  const [closed, setClosed] = useState(null);
  const matches = closed === draft.text ? null : matching(listing, draft.text);
  const listOpen = Boolean(matches) && ready;
  useEffect(() => {
    if (!ready || !can("chat_send") || !draft.text.startsWith("/") || listing) return undefined;
    let current = true;
    loadCommands(session.id).then((value) => { if (current) setListing(value); }, () => {});
    return () => { current = false; };
  }, [session.id, ready, draft.text.startsWith("/")]);
  useLayoutEffect(() => { if (listOpen) document.getElementById(`slash-${active}`)?.scrollIntoView({ block: "nearest" }); }, [active, listOpen]);
  const changed = (text) => {
    draft.text = text;
    setActive(0);
    renew((count) => count + 1);
  };
  const pick = (command) => {
    const text = `/${command.name} `;
    field.current.value = text;
    changed(text);
    field.current.focus();
    field.current.setSelectionRange(text.length, text.length);
  };

  // The field grows with its text, up to six lines.
  const grow = () => {
    const node = field.current;
    node.style.height = "auto";
    const line = parseFloat(getComputedStyle(node).lineHeight);
    node.style.height = `${Math.min(node.scrollHeight, line * 6 + (node.offsetHeight - node.clientHeight))}px`;
  };
  useLayoutEffect(() => { field.current.value = draft.text; grow(); }, [session.id]);

  const send = async () => {
    const body = field.current.value.trim();
    if (busy || !ready || (!body && !draft.attachments.length)) return;
    setBusy(true);
    setError("");
    try {
      const answer = await api(`api/chat/${session.id}/send`, {
        method: "POST", json: { text: body, attachments: draft.attachments.map((item) => item.hash) },
      });
      // A queued message shows in the queue; a delivered one waits for the saved conversation.
      if (!answer.queued) draft.pending = { text: body, attachments: draft.attachments };
      draft.text = "";
      draft.attachments = [];
      field.current.value = "";
      grow();
      onSent(answer);
    } catch (failure) {
      // The draft stays in the field; nothing was sent.
      setError(failure.message);
    }
    setBusy(false);
  };
  const upload = async (files) => {
    picker.current.value = "";
    setError("");
    for (const item of files) {
      if (item.size > state.site.max_file_bytes) {
        setError(`${item.name} is larger than ${bytes(state.site.max_file_bytes)}.`);
        continue;
      }
      try {
        draft.attachments.push(await api("api/media", { method: "PUT", query: { name: item.name }, body: item }));
      } catch (failure) {
        setError(`${item.name}: ${failure.message}`);
      }
      renew((count) => count + 1);
    }
  };
  const keydown = (event) => {
    if (event.isComposing || event.keyCode === 229) return;
    if (listOpen) {
      const move = { ArrowDown: 1, ArrowUp: -1 }[event.key];
      if (move && matches.length) {
        event.preventDefault();
        setActive((active + move + matches.length) % matches.length);
        return;
      }
      if ((event.key === "Tab" || event.key === "Enter") && !event.shiftKey && matches[active]) {
        event.preventDefault();
        pick(matches[active]);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setClosed(draft.text);
        return;
      }
    }
    // On a hardware keyboard Enter sends and Shift+Enter is a newline. On a touch
    // keyboard Enter is a newline. A key that belongs to an IME composition never sends.
    if (event.key !== "Enter" || event.shiftKey || touch()) return;
    event.preventDefault();
    send();
  };
  return html`
    <div class="composer">
      ${draft.attachments.length ? html`
        <ul class="chips" aria-label="Files to send">
          ${draft.attachments.map((item) => html`
            <li key=${item.hash}><span>${item.name} (${bytes(item.size)})</span>
              <button class="link-action" type="button" aria-label=${`Remove ${item.name}`}
                onClick=${() => { draft.attachments = draft.attachments.filter((other) => other !== item); renew((count) => count + 1); }}>Remove</button>
            </li>`)}
        </ul>` : null}
      ${error && html`<p class="error" role="alert">${error}</p>`}
      ${listOpen && html`<${CommandList} listing=${listing} matches=${matches} active=${active} onPick=${pick} onHover=${setActive} />`}
      <div class="composer-row">
        ${can("media_upload") && html`
          <button class="icon-button" type="button" aria-label="Attach files" onClick=${() => picker.current.click()}><${Icon} name="attach" /></button>
          <input type="file" multiple hidden ref=${picker} onChange=${(event) => upload([...event.target.files])} />`}
        <textarea class="composer-field" ref=${field} rows="1" aria-label="Message"
          placeholder=${ready ? "Message" : "Cannot receive messages"}
          disabled=${!ready} role="combobox" aria-autocomplete="list" aria-expanded=${listOpen}
          aria-controls=${listOpen ? "slash-list" : null} aria-activedescendant=${listOpen && matches.length ? `slash-${active}` : null}
          onInput=${() => { changed(field.current.value); grow(); }} onKeyDown=${keydown}></textarea>
        <button class="button button-primary" type="button" disabled=${busy || !ready} onClick=${send}>${working ? "Queue" : "Send"}</button>
      </div>
    </div>`;
}

const ANSWER_KEYS = [["enter", "Enter", "Enter"], ["escape", "Esc", "Escape"], ["up", "↑", "Up"], ["down", "↓", "Down"]];

// A blocked agent's own dialog, as it is on its screen, and the keys that answer it.
// ^ No choice is marked or filled: an approval must be a deliberate tap. An answer is
// bound to the screen it was read from; the server refuses it when the dialog changed.
function Question({ session, question, onAnswered }) {
  const [sent, setSent] = useState(null);
  const [busy, setBusy] = useState(false);
  const [typing, setTyping] = useState(false);
  const [error, setError] = useState("");
  const field = useRef();
  const waiting = busy || sent === question.stamp;
  const answer = async (key, text) => {
    if (waiting) return;
    setBusy(true);
    setError("");
    try {
      await api(`api/chat/${session.id}/answer`, { method: "POST", json: { stamp: question.stamp, key, ...(text === undefined ? {} : { text }) } });
      setSent(question.stamp);
      setTyping(false);
    } catch (failure) {
      setError(failure.code === "question_changed" ? "The question changed. Read it again." : failure.message);
    }
    setBusy(false);
    onAnswered();
  };
  const submit = (event) => {
    event.preventDefault();
    const text = field.current.value.trim();
    if (text) answer("text", text);
  };
  useEffect(() => { if (typing) field.current.focus(); }, [typing]);
  return html`
    <section class="question" aria-label="The agent asks">
      ${question.request && html`<h2 class="question-title">${question.request.title}</h2>`}
      <pre class="question-screen" tabindex="0" aria-label="The agent's screen">${question.screen.join("\n")}</pre>
      ${question.options.length ? html`
        <div class="question-options">
          ${question.options.map((option) => html`
            <button key=${option.key} class="button question-option" type="button" disabled=${waiting} onClick=${() => answer(option.key)}>
              <span class="question-key">${option.key}</span><span class="question-label">${option.label}</span>
            </button>`)}
        </div>` : null}
      ${typing && html`
        <form class="question-text" onSubmit=${submit}>
          <input class="input" ref=${field} type="text" maxlength="2000" aria-label="Answer" autocomplete="off" disabled=${waiting} />
          <button class="button" type="submit" disabled=${waiting}>Send</button>
        </form>`}
      ${error && html`<p class="error" role="alert">${error}</p>`}
      <div class="question-keys">
        ${sent === question.stamp ? html`<span class="note" role="status">Sent</span>` : null}
        ${ANSWER_KEYS.map(([key, label, name]) => html`
          <button key=${key} class="button question-keycap" type="button" aria-label=${name} disabled=${waiting} onClick=${() => answer(key)}>${label}</button>`)}
        <button class="link-action" type="button" aria-expanded=${typing} onClick=${() => setTyping(!typing)}>Type an answer</button>
        ${shows("terminal") && can("terminal_view") && html`<a class="link-action" href=${`#/s/${session.id}/terminal`}>Open terminal</a>`}
      </div>
    </section>`;
}

export function Chat({ session }) {
  const id = session.id;
  const [data, setData] = useState(null);
  const [failure, setFailure] = useState("");
  const [away, setAway] = useState(false);
  const log = useRef();
  const atEnd = useRef(true);
  const generation = useRef(0);
  const draft = draftFor(id);

  const load = useCallback(async () => {
    const mine = ++generation.current;
    try {
      const next = await api(`api/chat/${id}/timeline`);
      // A slower earlier answer must not replace a newer one.
      if (mine !== generation.current) return;
      setData(next);
      setFailure("");
    } catch (error) {
      if (mine === generation.current) setFailure(error.message);
    }
  }, [id]);
  useEffect(() => {
    load();
    const changed = (event) => { if (event.detail.session_id === undefined || event.detail.session_id === id) load(); };
    events.addEventListener("history", changed);
    events.addEventListener("exchange", changed);
    return () => {
      generation.current += 1;
      events.removeEventListener("history", changed);
      events.removeEventListener("exchange", changed);
    };
  }, [id]);
  // The session list says first that the agent started or stopped working.
  const status = stateOf(session);
  useEffect(() => { if (data) load(); }, [status.key]);

  // New items keep the reader at the end only when the reader is at the end.
  const scrolled = () => {
    const node = log.current;
    // A scroll event may still arrive after the view closed.
    if (!node) return;
    atEnd.current = node.scrollHeight - node.scrollTop - node.clientHeight < 80;
    setAway(!atEnd.current);
  };
  const toEnd = () => { log.current.scrollTop = log.current.scrollHeight; atEnd.current = true; setAway(false); };
  useLayoutEffect(() => { if (atEnd.current && log.current) log.current.scrollTop = log.current.scrollHeight; });

  const act = async (path, method = "POST") => {
    try {
      await api(`api/chat/${id}/${path}`, { method, json: {} });
      setFailure("");
    } catch (error) {
      setFailure(error.message);
    }
    load();
  };

  const items = data?.items ?? [];
  const lastUser = items.filter((item) => item.kind === "user").at(-1);
  // The saved conversation now holds the sent message: stop showing it as pending.
  if (draft.pending && lastUser?.text === draft.pending.text) draft.pending = null;
  const working = data?.working ?? null;
  const now = useNow(Boolean(working));
  const list = blocks(items);
  const delivery = data?.exchange && !working ? DELIVERY_WORDS[data.exchange.state]
    ?? (data.exchange.evidence === "intent_persisted" && data.exchange.active ? "Saved, not delivered to the agent yet." : null) : null;
  // A blocked agent shows its question where the composer was, or, on a site that may not
  // answer from the chat, a row that leads to the terminal. The draft stays for later.
  const blocked = status.key === "blocked";
  const lastUserBlock = list.findLast?.((block) => block.kind === "user");

  return html`
    <div class="chat">
      <div class="log-wrap">
      <div class="log" ref=${log} onScroll=${scrolled} role="log" aria-label="Conversation" tabindex="0">
        ${data?.checkpoint ? html`<details class="steps"><summary class="steps-summary"><${Icon} name="chevron" />Earlier conversation (summary)</summary><${Markdown} text=${data.checkpoint} /></details>`
          : data?.truncated ? html`<p class="note">Older messages are not shown.</p>` : null}
        ${data && !list.length && !draft.pending ? html`<p class="empty">${EMPTY_WORDS[data.status] ?? "No messages yet."}</p>` : null}
        ${list.map((block, index) => block.steps
          ? html`<${Steps} key=${block.id} items=${block.steps} live=${Boolean(working) && index === list.length - 1} />`
          : html`
            <article key=${block.id} class=${block.kind === "user" ? "message message-user" : "message"} data-kind=${block.kind}>
              ${block.text && (block.kind === "user" ? html`<p class="prose">${block.text}</p>`
                : html`<${Markdown} text=${block.text} files=${block.attachments} />`)}
              <${Attachments} files=${block.attachments} />
              ${block === lastUserBlock && delivery && !draft.pending ? html`<p class="note" role="status">${delivery}</p>` : null}
            </article>`)}
        ${draft.pending && html`
          <article class="message message-user" data-pending>
            ${draft.pending.text && html`<p class="prose">${draft.pending.text}</p>`}
            <${Attachments} files=${draft.pending.attachments} />
          </article>`}
        ${working && html`
          <div class="working" role="status">
            <span class="pulse" aria-hidden="true"></span>
            <span class="working-activity">${working.activity}</span>
            <span class="time">${elapsed(now - working.since_unix_ms)}</span>
            ${can("chat_send") && html`<button class="link-action" type="button" onClick=${() => act("cancel")}>Stop</button>`}
          </div>`}
      </div>
      ${away && html`<button class="jump" type="button" onClick=${toEnd}><${Icon} name="down" />Jump to latest</button>`}
      </div>
      ${failure && html`<p class="error chat-error" role="alert">${failure}</p>`}
      ${data?.queue?.length ? html`
        <ul class="queue" aria-label="Messages waiting for the agent">
          ${data.queue.map((queued) => html`
            <li key=${queued.id} class="queued">
              <span class="queued-text">${queued.text || `${queued.attachments.length} files`}</span>
              ${queued.failed && html`<span class="queued-reason" data-tone="danger">${queued.failed}</span>`}
              ${queued.failed && html`<button class="link-action" type="button" onClick=${() => act(`queue/${queued.id}/retry`)}>Retry</button>`}
              <button class="link-action" type="button" aria-label=${`Remove waiting message: ${queued.text.slice(0, 40)}`}
                onClick=${() => act(`queue/${queued.id}`, "DELETE")}>Remove</button>
            </li>`)}
        </ul>` : null}
      ${blocked && data?.question ? html`<${Question} session=${session} question=${data.question} onAnswered=${load} />`
      : blocked ? html`
        <div class="attention" role="status">
          <span>Waiting for your answer.</span>
          ${shows("terminal") && can("terminal_view") && html`<a class="link-action" href=${`#/s/${id}/terminal`}>Open terminal</a>`}
        </div>`
      : can("chat_send") && html`<${Composer} session=${session} working=${Boolean(working) || Boolean(data?.queue?.length)}
        onSent=${() => { atEnd.current = true; load(); }} />`}
    </div>`;
}
